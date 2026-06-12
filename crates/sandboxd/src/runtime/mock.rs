//! Development runtime — runs anywhere, no KVM required.
//!
//! It simulates the boot paths and their timings honestly (hot pool is fast,
//! snapshot restore is medium, cold boot is slow) so the benchmark harness and
//! API behave like production, while backing exec/files with a real per-sandbox
//! host workspace so the acceptance flows (exec echo, file read/write, preview
//! port) genuinely work.
//!
//! SECURITY: this runtime executes commands on the host shell inside a jailed
//! workspace. It provides NO isolation and MUST NOT be used to run untrusted
//! code. Production uses [`super::firecracker::FirecrackerRuntime`].

use super::workspace::Workspaces;
use super::{
    DirEntry, ExecRequest, ExecResult, PtySession, Runtime, SnapshotArtifact, VmInstance, VmSpec,
    WarmVm,
};
use crate::ids;
use crate::model::BootPath;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use tokio::process::Command;

#[derive(Default, Serialize, Deserialize)]
struct VmState {
    paused: bool,
    /// Parked in perpetual standby: snapshot taken, "RAM" freed (Phase 1).
    standby: bool,
    /// Env that persists across exec calls: startup env + injected secrets.
    resident_env: std::collections::BTreeMap<String, String>,
}

pub struct MockRuntime {
    workspaces: Workspaces,
    /// Persistent-volume backing stores (Phase 5). Each volume is a plain host
    /// directory here; production backs it with a labelled ext4 image.
    volumes_dir: std::path::PathBuf,
    state: Mutex<HashMap<String, VmState>>,
}

impl MockRuntime {
    pub fn new(
        workspace_root: impl Into<std::path::PathBuf>,
        volumes_dir: impl Into<std::path::PathBuf>,
    ) -> MockRuntime {
        MockRuntime {
            workspaces: Workspaces::new(workspace_root),
            volumes_dir: volumes_dir.into(),
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Simulate a block-volume attach: the volume's data lives in a host dir
    /// (`<volumes_dir>/<id>.data`) and the "mount" is a symlink at the guest
    /// mount path. Data genuinely persists across sandboxes, so the acceptance
    /// flow (write in sandbox A, read in sandbox B) works like production.
    /// Exclusive attachment is enforced upstream by the service layer.
    fn attach_volume(&self, handle: &str, v: &crate::model::VolumeAttach) -> Result<()> {
        let data = self.volumes_dir.join(format!("{}.data", v.volume_id));
        if !data.exists() {
            anyhow::bail!("volume {} has no backing store", v.volume_id);
        }
        let link = self.workspaces.resolve(handle, &v.mount_path)?;
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&data, &link).context("simulate volume mount (symlink)")?;
        Ok(())
    }

    fn dir(&self, handle: &str) -> std::path::PathBuf {
        self.workspaces.dir_for(handle).join("workspace")
    }

    /// Where this VM's runtime state is persisted (a sibling of the workspace,
    /// so it is outside the user-visible file API). Persisting it lets a standby
    /// sandbox survive a control-plane restart: a fresh runtime rehydrates the
    /// record from disk and can still `restore` it (roadmap Phase 1).
    fn state_path(&self, handle: &str) -> std::path::PathBuf {
        self.workspaces.dir_for(handle).join("vm.json")
    }

    /// Write the in-memory state for `handle` to disk.
    fn persist(&self, handle: &str) {
        let json = {
            let state = self.state.lock().unwrap();
            state.get(handle).and_then(|s| serde_json::to_string(s).ok())
        };
        if let Some(json) = json {
            let _ = std::fs::write(self.state_path(handle), json);
        }
    }

    /// If `handle` is not resident in memory (e.g. after a restart), rehydrate it
    /// from its on-disk `vm.json`. No-op if already loaded or never persisted.
    fn ensure_loaded(&self, handle: &str) {
        {
            let state = self.state.lock().unwrap();
            if state.contains_key(handle) {
                return;
            }
        }
        if let Ok(data) = std::fs::read_to_string(self.state_path(handle)) {
            if let Ok(st) = serde_json::from_str::<VmState>(&data) {
                self.state.lock().unwrap().insert(handle.to_string(), st);
            }
        }
    }

    /// Deterministic small jitter (0..span ms) derived from the handle so the
    /// numbers look realistic without using a clock-based RNG.
    fn jitter(handle: &str, span: u64) -> u64 {
        let sum: u64 = handle.bytes().map(|b| b as u64).sum();
        sum % (span + 1)
    }
}

#[async_trait]
impl Runtime for MockRuntime {
    fn kind(&self) -> &'static str {
        "mock"
    }

    async fn prewarm(&self, spec: &VmSpec) -> Result<WarmVm> {
        let handle = format!("warm_{}_{}", spec.image_key, &ids::sandbox_id()[4..]);
        self.workspaces.create(&handle)?;
        // Warming a VM takes real time even in the pool; keep it modest.
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.state
            .lock()
            .unwrap()
            .insert(handle.clone(), VmState::default());
        Ok(WarmVm { handle, image_key: spec.image_key.clone(), resources: spec.resources })
    }

    async fn create(
        &self,
        spec: &VmSpec,
        warm: Option<WarmVm>,
        snapshot_available: bool,
    ) -> Result<VmInstance> {
        let (handle, boot_path, boot_ms, image_cache_ms) = match warm {
            Some(w) => {
                // Hot pool: the warm VM becomes the sandbox. Just attach.
                (w.handle, BootPath::HotPool, 35 + Self::jitter(&spec.sandbox_id, 15), 0)
            }
            None if snapshot_available => {
                let h = format!("vm_{}", spec.sandbox_id);
                self.workspaces.create(&h)?;
                (h, BootPath::SnapshotRestore, 200 + Self::jitter(&spec.sandbox_id, 60), 0)
            }
            None => {
                let h = format!("vm_{}", spec.sandbox_id);
                self.workspaces.create(&h)?;
                // Cold boot also pays an image cache cost for custom images.
                let cache = if spec.image_key == "custom" { 400 } else { 60 };
                (h, BootPath::ColdBoot, 1500 + Self::jitter(&spec.sandbox_id, 400), cache)
            }
        };

        // Simulate the boot wall time so benchmarks measure something real.
        tokio::time::sleep(Duration::from_millis(boot_ms + image_cache_ms)).await;

        let browser_ready_ms = if spec.browser.as_ref().map(|b| b.enabled).unwrap_or(false) {
            let ms = 1200 + Self::jitter(&spec.sandbox_id, 200);
            tokio::time::sleep(Duration::from_millis(ms / 8)).await; // keep tests snappy
            ms
        } else {
            0
        };

        // Apply features: resident env (incl. secrets), inline files, simulated
        // mounts. Docker-in-docker and the opt-in coding agent are no-ops in the
        // dev runtime (no real guest / no network install); intent is recorded on
        // the sandbox record so the API still reflects it.
        let mut resident_env = spec.env.clone();
        resident_env.extend(spec.secret_env.clone());
        for (path, bytes) in &spec.files {
            let _ = self.write_file(&handle, path, bytes).await;
        }
        for m in &spec.mounts {
            // Simulate the mount by creating its directory under the workspace.
            let _ = self.write_file(&handle, &format!("{}/.s3-mount-{}", m.mount_path, m.bucket), b"mounted (dev simulation)").await;
        }
        self.state
            .lock()
            .unwrap()
            .insert(handle.clone(), VmState { paused: false, standby: false, resident_env });
        self.persist(&handle);

        Ok(VmInstance { handle, boot_path, boot_ms, image_cache_ms, browser_ready_ms, agent_ms: 0 })
    }

    async fn exec(&self, handle: &str, req: &ExecRequest) -> Result<ExecResult> {
        self.ensure_loaded(handle); // serve even after a restart rehydrated nothing yet
        let cwd = match &req.cwd {
            Some(c) => self.workspaces.resolve(handle, c)?,
            None => self.dir(handle),
        };
        std::fs::create_dir_all(&cwd).ok();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-lc").arg(&req.cmd).current_dir(&cwd);
        // Resident env (startup env + injected secrets) first, then per-call env.
        if let Some(state) = self.state.lock().unwrap().get(handle) {
            for (k, v) in &state.resident_env {
                cmd.env(k, v);
            }
        }
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        if req.background {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            let child = cmd.spawn().context("spawn background command")?;
            return Ok(ExecResult {
                exit_code: 0,
                stdout: format!("started background pid {}", child.id().unwrap_or(0)),
                stderr: String::new(),
            });
        }
        let out = cmd.output().await.context("run command")?;
        Ok(ExecResult {
            exit_code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
    }

    async fn open_pty(&self, handle: &str) -> Result<PtySession> {
        let cwd = self.dir(handle);
        std::fs::create_dir_all(&cwd).ok();
        let mut child = Command::new("/bin/sh")
            .arg("-i")
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn interactive shell")?;
        let stdin = child.stdin.take().context("pty stdin")?;
        let stdout = child.stdout.take().context("pty stdout")?;
        let stderr = child.stderr.take().context("pty stderr")?;
        Ok(PtySession { child, stdin, stdout, stderr })
    }

    async fn write_file(&self, handle: &str, path: &str, bytes: &[u8]) -> Result<()> {
        let p = self.workspaces.resolve(handle, path)?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, bytes)?;
        Ok(())
    }

    async fn read_file(&self, handle: &str, path: &str) -> Result<Vec<u8>> {
        let p = self.workspaces.resolve(handle, path)?;
        Ok(std::fs::read(&p)?)
    }

    async fn list_dir(&self, handle: &str, path: &str) -> Result<Vec<DirEntry>> {
        let p = self.workspaces.resolve(handle, path)?;
        let mut out = vec![];
        for e in std::fs::read_dir(&p)? {
            let e = e?;
            out.push(DirEntry {
                name: e.file_name().to_string_lossy().to_string(),
                dir: e.file_type().map(|t| t.is_dir()).unwrap_or(false),
            });
        }
        Ok(out)
    }

    async fn expose_port(&self, _handle: &str, port: u16) -> Result<SocketAddr> {
        // In the dev runtime the sandbox process binds the host loopback, so the
        // preview proxy forwards straight there. Production gives each VM its own
        // netns, so ports never collide across sandboxes.
        Ok(SocketAddr::from(([127, 0, 0, 1], port)))
    }

    async fn ready_check(&self, _handle: &str, url: &str, timeout_seconds: u32) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds as u64);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_default();
        loop {
            if let Ok(resp) = client.get(url).send().await {
                if resp.status().is_success() {
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("ready check timed out after {timeout_seconds}s for {url}");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn pause(&self, handle: &str, _persist: bool) -> Result<()> {
        self.ensure_loaded(handle);
        if let Some(s) = self.state.lock().unwrap().get_mut(handle) {
            s.paused = true;
        }
        self.persist(handle);
        Ok(())
    }

    async fn resume(&self, handle: &str) -> Result<u64> {
        self.ensure_loaded(handle);
        if let Some(s) = self.state.lock().unwrap().get_mut(handle) {
            s.paused = false;
        }
        self.persist(handle);
        let ms = 180 + Self::jitter(handle, 60);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(ms)
    }

    async fn standby(&self, handle: &str) -> Result<u64> {
        // Snapshot-to-disk + free RAM. The dev runtime keeps the workspace on
        // the host (its "disk"), so state survives; we flip the flag and persist
        // the record so the sandbox can be restored even after a control-plane
        // restart drops the in-memory map (Phase 1).
        self.ensure_loaded(handle);
        {
            let mut state = self.state.lock().unwrap();
            let s = state.get_mut(handle).context("standby: unknown vm")?;
            s.standby = true;
            s.paused = true;
        }
        self.persist(handle);
        let ms = 90 + Self::jitter(handle, 40);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(ms)
    }

    async fn restore(&self, handle: &str) -> Result<u64> {
        // In-place restore from the on-disk snapshot. This is the perpetual-
        // standby resume path; the dev runtime simulates the *optimized* target
        // (page-cache-hot mem.file / UFFD demand paging — Phase 2), so the
        // number models a < 25ms resume rather than a cold 180ms one. After a
        // restart the in-memory record is gone, so rehydrate it from disk first.
        self.ensure_loaded(handle);
        {
            let mut state = self.state.lock().unwrap();
            let s = state.get_mut(handle).context("restore: unknown vm (no persisted record)")?;
            s.standby = false;
            s.paused = false;
        }
        self.persist(handle);
        let ms = 12 + Self::jitter(handle, 8);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(ms)
    }

    async fn snapshot(&self, handle: &str) -> Result<SnapshotArtifact> {
        // Approximate stored size from the workspace footprint.
        let dir = self.workspaces.dir_for(handle);
        let bytes = dir_size(&dir);
        Ok(SnapshotArtifact { handle: ids::snapshot_id(), storage_bytes: bytes })
    }

    async fn fork(&self, parent_handle: &str, child_spec: &VmSpec) -> Result<VmInstance> {
        // Instant sibling: copy the parent's workspace (its "disk") into a fresh
        // VM so the child starts from the parent's exact state, then apply the
        // child's own env and inline files on top.
        let child = format!("vm_{}", child_spec.sandbox_id);
        self.workspaces.create(&child)?;
        let parent_ws = self.dir(parent_handle);
        let child_ws = self.dir(&child);
        copy_dir(&parent_ws, &child_ws).context("fork: copy parent workspace")?;

        let mut resident_env = child_spec.env.clone();
        resident_env.extend(child_spec.secret_env.clone());
        self.state
            .lock()
            .unwrap()
            .insert(child.clone(), VmState { paused: false, standby: false, resident_env });
        self.persist(&child);
        for (path, bytes) in &child_spec.files {
            let _ = self.write_file(&child, path, bytes).await;
        }

        // Fork tracks resume latency (cloning the artifact, not booting).
        let ms = 15 + Self::jitter(&child_spec.sandbox_id, 10);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(VmInstance {
            handle: child,
            boot_path: BootPath::Fork,
            boot_ms: ms,
            image_cache_ms: 0,
            browser_ready_ms: 0,
            agent_ms: 0,
        })
    }

    async fn delete(&self, handle: &str) -> Result<()> {
        self.state.lock().unwrap().remove(handle);
        self.workspaces.remove(handle)?;
        Ok(())
    }
}

/// Recursively copy `src` into `dst` (used by `fork` to clone the parent's
/// workspace). Missing `src` is treated as an empty tree.
fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    if !src.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn dir_size(path: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for e in entries.flatten() {
            let meta = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                total += dir_size(&e.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}
