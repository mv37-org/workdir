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
use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use tokio::process::Command;

#[derive(Default)]
struct VmState {
    paused: bool,
    /// Env that persists across exec calls: startup env + injected secrets.
    resident_env: std::collections::BTreeMap<String, String>,
}

pub struct MockRuntime {
    workspaces: Workspaces,
    state: Mutex<HashMap<String, VmState>>,
}

impl MockRuntime {
    pub fn new(workspace_root: impl Into<std::path::PathBuf>) -> MockRuntime {
        MockRuntime { workspaces: Workspaces::new(workspace_root), state: Mutex::new(HashMap::new()) }
    }

    fn dir(&self, handle: &str) -> std::path::PathBuf {
        self.workspaces.dir_for(handle).join("workspace")
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
            .insert(handle.clone(), VmState { paused: false, resident_env });

        Ok(VmInstance { handle, boot_path, boot_ms, image_cache_ms, browser_ready_ms, agent_ms: 0 })
    }

    async fn exec(&self, handle: &str, req: &ExecRequest) -> Result<ExecResult> {
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
        if let Some(s) = self.state.lock().unwrap().get_mut(handle) {
            s.paused = true;
        }
        Ok(())
    }

    async fn resume(&self, handle: &str) -> Result<u64> {
        if let Some(s) = self.state.lock().unwrap().get_mut(handle) {
            s.paused = false;
        }
        let ms = 180 + Self::jitter(handle, 60);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(ms)
    }

    async fn snapshot(&self, handle: &str) -> Result<SnapshotArtifact> {
        // Approximate stored size from the workspace footprint.
        let dir = self.workspaces.dir_for(handle);
        let bytes = dir_size(&dir);
        Ok(SnapshotArtifact { handle: ids::snapshot_id(), storage_bytes: bytes })
    }

    async fn delete(&self, handle: &str) -> Result<()> {
        self.state.lock().unwrap().remove(handle);
        self.workspaces.remove(handle)?;
        Ok(())
    }
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
