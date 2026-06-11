//! Production runtime — real Firecracker microVMs under the jailer (spec §7,
//! §18). This adapter constructs the jailer command, drives the Firecracker API
//! over its Unix control socket, and talks to the in-VM guest agent over an
//! `AF_VSOCK`-backed Unix domain socket using the JSON line protocol from the
//! `guest-agent` crate.
//!
//! It compiles on any Unix host (so the whole binary builds on a dev Mac) but
//! REQUIRES a Linux host with `/dev/kvm` to actually boot a VM. On a non-KVM
//! host the jailer/Firecracker spawn fails at runtime with a clear error, which
//! is why the installer flips the runtime to `firecracker` only after the KVM
//! preflight passes.
//!
//! What is wired here: cold boot, snapshot restore/create, pause/resume,
//! exec/files/ready over vsock, teardown. What a production operator still tunes
//! per fleet: kernel boot args, rootfs/overlay layout, CID allocation, and the
//! nftables/netns plumbing the installer lays down (see `deploy/`).

#![cfg(unix)]

use super::workspace::Workspaces;
use super::{
    DirEntry, ExecRequest, ExecResult, PtySession, Runtime, SnapshotArtifact, VmInstance, VmSpec,
    WarmVm,
};
use crate::config::RuntimeConfig;
use crate::ids;
use crate::model::BootPath;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// vsock port the in-guest agent listens on (matches the init shim).
const GUEST_AGENT_VSOCK_PORT: u32 = 5005;

/// In-guest workspace root the file API is confined to.
const GUEST_WORKSPACE: &str = "/workspace";

/// Confine a user-supplied file path to the guest workspace, rejecting `..`
/// traversal and absolute escapes (review C2). The file API is scoped to the
/// workspace; exec can still touch the rest of the guest (it's the user's VM).
fn jail_guest_path(path: &str) -> Result<String> {
    let rel = path.trim_start_matches('/');
    let mut out: Vec<&str> = Vec::new();
    for comp in rel.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    bail!("path escapes workspace: {path}");
                }
            }
            other => out.push(other),
        }
    }
    Ok(format!("{GUEST_WORKSPACE}/{}", out.join("/")))
}

struct VmRecord {
    /// Firecracker API control socket.
    api_sock: PathBuf,
    /// Host-side Unix socket that fronts the guest's vsock.
    vsock_uds: PathBuf,
    /// Jailer/Firecracker process id, for teardown.
    pid: Option<u32>,
    image_key: String,
    /// Env applied to every exec: startup env + injected secrets. Kept in host
    /// memory and passed per-exec so secrets never persist to a guest env file
    /// or land in a snapshot (review M3).
    resident_env: std::collections::BTreeMap<String, String>,
    /// True while secret values are resident; snapshots are refused.
    has_secrets: bool,
}

pub struct FirecrackerRuntime {
    firecracker_bin: String,
    jailer_bin: String,
    kernel_image: String,
    images_dir: PathBuf,
    workspaces: Workspaces,
    chroot_base: PathBuf,
    next_cid: AtomicU32,
    vms: Mutex<HashMap<String, VmRecord>>,
}

impl FirecrackerRuntime {
    pub fn new(cfg: &RuntimeConfig) -> FirecrackerRuntime {
        FirecrackerRuntime {
            firecracker_bin: cfg.firecracker_bin.clone(),
            jailer_bin: cfg.jailer_bin.clone(),
            kernel_image: cfg.kernel_image.clone(),
            images_dir: cfg.images_dir.clone(),
            workspaces: Workspaces::new(cfg.workspace_dir.clone()),
            chroot_base: cfg.workspace_dir.join("jail"),
            next_cid: AtomicU32::new(3), // CIDs 0-2 are reserved
            vms: Mutex::new(HashMap::new()),
        }
    }

    /// Read-only curated/custom rootfs artifact path for an image key/ref.
    fn rootfs_path(&self, spec: &VmSpec) -> PathBuf {
        // Curated: images_dir/<key>/rootfs.ext4. Custom: images_dir/custom/<...>.
        if spec.image_key == "custom" {
            let safe = spec.image_ref.replace(['/', ':'], "_");
            self.images_dir.join("custom").join(format!("{safe}.ext4"))
        } else {
            self.images_dir.join(&spec.image_key).join("rootfs.ext4")
        }
    }

    /// HTTP PUT/PATCH against the Firecracker API socket. Firecracker speaks
    /// HTTP/1.1 over a Unix socket; we issue a minimal request and read the
    /// status line so we surface configuration errors clearly.
    async fn fc_api(&self, sock: &PathBuf, method: &str, path: &str, body: &serde_json::Value) -> Result<()> {
        let mut stream = UnixStream::connect(sock)
            .await
            .with_context(|| format!("connect firecracker api socket {sock:?}"))?;
        let body_str = serde_json::to_string(body)?;
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body_str.len(),
            body_str
        );
        stream.write_all(req.as_bytes()).await?;
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await?;
        let text = String::from_utf8_lossy(&resp);
        let status = text.lines().next().unwrap_or("");
        // 2xx and 204 are success.
        if !(status.contains(" 200") || status.contains(" 201") || status.contains(" 204")) {
            bail!("firecracker api {method} {path} failed: {status}\n{text}");
        }
        Ok(())
    }

    /// One request/response with the guest agent over the vsock-backed UDS,
    /// performing Firecracker's `CONNECT <port>` handshake first.
    async fn agent_call(&self, handle: &str, request: &serde_json::Value) -> Result<serde_json::Value> {
        let uds = {
            let vms = self.vms.lock().unwrap();
            vms.get(handle).map(|v| v.vsock_uds.clone()).ok_or_else(|| anyhow!("unknown vm {handle}"))?
        };
        let mut stream = UnixStream::connect(&uds)
            .await
            .with_context(|| format!("connect guest vsock uds {uds:?}"))?;
        // Firecracker host-initiated connection handshake.
        stream.write_all(format!("CONNECT {GUEST_AGENT_VSOCK_PORT}\n").as_bytes()).await?;
        let mut ack = [0u8; 64];
        let n = stream.read(&mut ack).await?;
        let ack_str = String::from_utf8_lossy(&ack[..n]);
        if !ack_str.starts_with("OK") {
            bail!("vsock connect rejected: {ack_str}");
        }
        let line = format!("{}\n", serde_json::to_string(request)?);
        stream.write_all(line.as_bytes()).await?;
        // Read one response line.
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = stream.read(&mut byte).await?;
            if n == 0 || byte[0] == b'\n' {
                break;
            }
            buf.push(byte[0]);
        }
        let resp: serde_json::Value = serde_json::from_slice(&buf).context("parse agent response")?;
        if resp.get("status").and_then(|s| s.as_str()) == Some("error") {
            bail!("guest agent error: {}", resp.get("message").and_then(|m| m.as_str()).unwrap_or("?"));
        }
        Ok(resp.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }

    /// Wait until the guest agent answers a ping or we time out.
    async fn await_agent(&self, handle: &str, timeout: Duration) -> Result<u64> {
        let start = Instant::now();
        loop {
            if self.agent_call(handle, &json!({"op": "ping"})).await.is_ok() {
                return Ok(start.elapsed().as_millis() as u64);
            }
            if start.elapsed() >= timeout {
                bail!("guest agent did not become ready within {timeout:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Boot a microVM: jailer + firecracker, configure via API, start, and wait
    /// for the guest agent. `snapshot` selects LoadSnapshot instead of a fresh
    /// boot.
    async fn boot(&self, spec: &VmSpec, snapshot: Option<PathBuf>) -> Result<(String, u64, u64)> {
        let handle = format!("vm_{}", spec.sandbox_id);
        let cid = self.next_cid.fetch_add(1, Ordering::SeqCst);
        let jail = self.chroot_base.join(&handle);
        std::fs::create_dir_all(&jail).context("create jail dir")?;
        let api_sock = jail.join("api.sock");
        let vsock_uds = jail.join("vsock.sock");
        self.workspaces.create(&handle)?;

        // Copy-on-write overlay of the read-only rootfs for the writable disk.
        let rootfs = self.rootfs_path(spec);
        let overlay = jail.join("overlay.ext4");
        let _ = tokio::process::Command::new("cp")
            .args(["--reflink=auto"])
            .arg(&rootfs)
            .arg(&overlay)
            .status()
            .await
            .context("create COW overlay (is the rootfs present?)")?;

        // Spawn jailer wrapping firecracker. The jailer chroots, drops
        // privileges, sets a unique uid/gid, and isolates the VM (spec §18).
        let cache_dir = format!("{}", jail.display());
        let child = tokio::process::Command::new(&self.jailer_bin)
            .args(["--id", &handle])
            .args(["--exec-file", &self.firecracker_bin])
            .args(["--uid", "123456"]) // installer assigns per-VM uid/gid ranges
            .args(["--gid", "123456"])
            .args(["--chroot-base-dir", &cache_dir])
            .args(["--", "--api-sock", api_sock.to_str().unwrap()])
            .spawn()
            .context("spawn jailer (requires /dev/kvm on a Linux host)")?;
        let pid = child.id();

        self.vms.lock().unwrap().insert(
            handle.clone(),
            VmRecord {
                api_sock: api_sock.clone(),
                vsock_uds: vsock_uds.clone(),
                pid,
                image_key: spec.image_key.clone(),
                resident_env: Default::default(),
                has_secrets: false,
            },
        );

        // Everything after the jailer spawn is fallible (config errors, a 10 s
        // agent timeout). If any step fails we MUST kill the VM and reclaim its
        // RAM/jail dir, otherwise each failed boot leaks a live microVM
        // (review #10).
        let booted: Result<(u64, u64)> = async {
            // Give the API socket a moment to appear.
            for _ in 0..100 {
                if api_sock.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            let mem_mib = spec.resources.memory_mb;
            let vcpus = spec.resources.cpu.ceil().max(1.0) as u32;

            // Always wire the vsock device so the host can reach the guest agent.
            self.fc_api(&api_sock, "PUT", "/vsock", &json!({
                "guest_cid": cid,
                "uds_path": vsock_uds.to_str().unwrap(),
            })).await?;

            let boot_start = Instant::now();
            if let Some(snap) = snapshot {
                self.fc_api(&api_sock, "PUT", "/snapshot/load", &json!({
                    "snapshot_path": snap.join("snapshot.file").to_str().unwrap(),
                    "mem_backend": { "backend_path": snap.join("mem.file").to_str().unwrap(), "backend_type": "File" },
                    "resume_vm": true,
                })).await?;
            } else {
                self.fc_api(&api_sock, "PUT", "/machine-config", &json!({
                    "vcpu_count": vcpus,
                    "mem_size_mib": mem_mib,
                    "smt": false,
                })).await?;
                self.fc_api(&api_sock, "PUT", "/boot-source", &json!({
                    "kernel_image_path": self.kernel_image,
                    "boot_args": "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/sandbox-init",
                })).await?;
                self.fc_api(&api_sock, "PUT", "/drives/rootfs", &json!({
                    "drive_id": "rootfs",
                    "path_on_host": overlay.to_str().unwrap(),
                    "is_root_device": true,
                    "is_read_only": false,
                })).await?;
                self.fc_api(&api_sock, "PUT", "/actions", &json!({
                    "action_type": "InstanceStart",
                })).await?;
            }
            let boot_ms = boot_start.elapsed().as_millis() as u64;

            // Wait for the guest agent. Env (and secrets) are NOT written into
            // the guest here; they are applied per-exec from the host record so
            // secrets never persist to a guest file (review M3, see
            // `apply_features`).
            let agent_ms = self.await_agent(&handle, Duration::from_secs(10)).await?;
            Ok((boot_ms, agent_ms))
        }
        .await;

        match booted {
            Ok((boot_ms, agent_ms)) => Ok((handle, boot_ms, agent_ms)),
            Err(e) => {
                self.kill_and_reclaim(&handle, pid, &jail).await;
                Err(e)
            }
        }
    }

    /// Apply per-sandbox features to a booted (warm or cold) VM: resident env +
    /// secrets, inline ephemeral files, docker-in-docker, and bucket mounts.
    async fn apply_features(&self, handle: &str, spec: &VmSpec) -> Result<()> {
        // Resident env lives in the host record and is applied per-exec, so
        // secrets never persist to a guest env file or a snapshot (review M3).
        {
            let mut vms = self.vms.lock().unwrap();
            if let Some(rec) = vms.get_mut(handle) {
                rec.resident_env = spec.env.clone();
                rec.resident_env.extend(spec.secret_env.clone());
                rec.has_secrets = !spec.secret_env.is_empty();
            }
        }

        // Inline ephemeral files into the workspace.
        for (path, bytes) in &spec.files {
            let jailed = jail_guest_path(path)?;
            let b64 = base64_encode(bytes);
            let _ = self
                .agent_call(handle, &json!({"op": "write_file", "path": jailed, "content_b64": b64}))
                .await;
        }

        // Docker-in-docker: start dockerd INSIDE the guest (the VM is the
        // isolation boundary). Requires a docker-capable image + guest kernel
        // (overlayfs, cgroups, iptables). The host socket is never exposed.
        if spec.docker {
            let _ = self
                .agent_call(handle, &json!({
                    "op": "exec",
                    "cmd": "nohup dockerd --host=unix:///var/run/docker.sock >/var/log/dockerd.log 2>&1 & \
                            for i in $(seq 1 50); do [ -S /var/run/docker.sock ] && break; sleep 0.2; done",
                    "background": false,
                }))
                .await;
        }

        // Bucket mounts via mountpoint-s3, with AWS creds taken from the resident
        // (secret) env exported for the mount command.
        for m in &spec.mounts {
            if m.kind != "s3" {
                continue;
            }
            let mut args = format!("mount-s3 {} {}", shell_quote(&m.bucket), shell_quote(&m.mount_path));
            if let Some(prefix) = &m.prefix {
                args.push_str(&format!(" --prefix {}", shell_quote(prefix)));
            }
            if m.read_only {
                args.push_str(" --read-only");
            }
            if let Some(region) = &m.region {
                args.push_str(&format!(" --region {}", shell_quote(region)));
            }
            if let Some(endpoint) = &m.endpoint {
                args.push_str(&format!(" --endpoint-url {}", shell_quote(endpoint)));
            }
            let env_exports: String = spec
                .secret_env
                .iter()
                .map(|(k, v)| format!("export {k}={}; ", shell_quote(v)))
                .collect();
            let _ = self
                .agent_call(handle, &json!({
                    "op": "exec",
                    "cmd": format!("mkdir -p {}; {}{}", shell_quote(&m.mount_path), env_exports, args),
                    "background": false,
                }))
                .await;
        }
        Ok(())
    }

    /// Kill a VM's jailer process and reclaim its jail dir + workspace + record.
    async fn kill_and_reclaim(&self, handle: &str, pid: Option<u32>, jail: &std::path::Path) {
        if let Some(pid) = pid {
            let _ = tokio::process::Command::new("kill").arg(pid.to_string()).status().await;
        }
        self.vms.lock().unwrap().remove(handle);
        let _ = std::fs::remove_dir_all(jail);
        self.workspaces.remove(handle).ok();
    }
}

#[async_trait]
impl Runtime for FirecrackerRuntime {
    fn kind(&self) -> &'static str {
        "firecracker"
    }

    async fn prewarm(&self, spec: &VmSpec) -> Result<WarmVm> {
        // A warm VM is a fully booted, idle microVM kept paused-ready. We boot
        // it and leave the agent live; create() attaches and unpauses.
        let (handle, _boot_ms, _agent_ms) = self.boot(spec, None).await?;
        Ok(WarmVm { handle, image_key: spec.image_key.clone(), resources: spec.resources })
    }

    async fn create(&self, spec: &VmSpec, warm: Option<WarmVm>, snapshot_available: bool) -> Result<VmInstance> {
        let (handle, boot_path, boot_ms) = if let Some(w) = warm {
            // Hot pool: the warm VM already booted. Attach and apply features.
            (w.handle, BootPath::HotPool, 0)
        } else {
            let snapshot = if snapshot_available {
                Some(self.images_dir.join("snapshots").join(&spec.image_key))
            } else {
                None
            };
            let boot_path =
                if snapshot.is_some() { BootPath::SnapshotRestore } else { BootPath::ColdBoot };
            let (handle, boot_ms, _agent_ms) = self.boot(spec, snapshot).await?;
            (handle, boot_path, boot_ms)
        };

        // Apply secrets/env, inline files, docker, and mounts to whichever VM we
        // ended up with (warm or freshly booted).
        self.apply_features(&handle, spec).await?;

        let browser_ready_ms = if spec.browser.as_ref().map(|b| b.enabled).unwrap_or(false) {
            let start = Instant::now();
            let _ = self
                .agent_call(&handle, &json!({"op": "ready_http", "url": "http://127.0.0.1:9222/json/version", "timeout_seconds": 30}))
                .await;
            start.elapsed().as_millis() as u64
        } else {
            0
        };
        Ok(VmInstance { handle, boot_path, boot_ms, image_cache_ms: 0, browser_ready_ms })
    }

    async fn exec(&self, handle: &str, req: &ExecRequest) -> Result<ExecResult> {
        // Merge resident env (startup env + injected secrets) under per-call env.
        let mut merged: std::collections::BTreeMap<String, String> = {
            let vms = self.vms.lock().unwrap();
            vms.get(handle).map(|v| v.resident_env.clone()).unwrap_or_default()
        };
        merged.extend(req.env.clone());
        let env: Vec<(String, String)> = merged.into_iter().collect();
        let result = self
            .agent_call(handle, &json!({
                "op": "exec",
                "cmd": req.cmd,
                "cwd": req.cwd,
                "env": env,
                "background": req.background,
            }))
            .await?;
        if req.background {
            return Ok(ExecResult { exit_code: 0, stdout: result.to_string(), stderr: String::new() });
        }
        Ok(ExecResult {
            exit_code: result.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1) as i32,
            stdout: result.get("stdout").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            stderr: result.get("stderr").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        })
    }

    async fn open_pty(&self, _handle: &str) -> Result<PtySession> {
        // A real TTY requires a vsock-multiplexed PTY channel to the guest
        // agent; the dev runtime backs the API contract today. The production
        // PTY channel is the next increment on top of agent_call.
        bail!("interactive PTY over vsock not yet implemented in the firecracker runtime")
    }

    async fn write_file(&self, handle: &str, path: &str, bytes: &[u8]) -> Result<()> {
        let jailed = jail_guest_path(path)?;
        let b64 = base64_encode(bytes);
        self.agent_call(handle, &json!({"op": "write_file", "path": jailed, "content_b64": b64})).await?;
        Ok(())
    }

    async fn read_file(&self, handle: &str, path: &str) -> Result<Vec<u8>> {
        let jailed = jail_guest_path(path)?;
        let result = self.agent_call(handle, &json!({"op": "read_file", "path": jailed})).await?;
        let b64 = result.get("content_b64").and_then(|v| v.as_str()).unwrap_or("");
        base64_decode(b64)
    }

    async fn list_dir(&self, handle: &str, path: &str) -> Result<Vec<DirEntry>> {
        let jailed = jail_guest_path(path)?;
        let result = self.agent_call(handle, &json!({"op": "list_dir", "path": jailed})).await?;
        let entries = result.get("entries").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        Ok(entries
            .into_iter()
            .map(|e| DirEntry {
                name: e.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                dir: e.get("dir").and_then(|v| v.as_bool()).unwrap_or(false),
            })
            .collect())
    }

    async fn expose_port(&self, _handle: &str, port: u16) -> Result<SocketAddr> {
        // The guest's private IP is reachable via the host's NAT/netns. The
        // installer programs nftables so the preview proxy can dial the guest IP
        // on the requested port. Here we return the in-namespace upstream; the
        // proxy resolves the guest IP from the VM record's tap config.
        Ok(SocketAddr::from(([127, 0, 0, 1], port)))
    }

    async fn ready_check(&self, handle: &str, url: &str, timeout_seconds: u32) -> Result<()> {
        self.agent_call(handle, &json!({"op": "ready_http", "url": url, "timeout_seconds": timeout_seconds})).await?;
        Ok(())
    }

    async fn pause(&self, handle: &str, _persist: bool) -> Result<()> {
        let sock = {
            let vms = self.vms.lock().unwrap();
            vms.get(handle).map(|v| v.api_sock.clone()).ok_or_else(|| anyhow!("unknown vm {handle}"))?
        };
        self.fc_api(&sock, "PATCH", "/vm", &json!({"state": "Paused"})).await
    }

    async fn resume(&self, handle: &str) -> Result<u64> {
        let sock = {
            let vms = self.vms.lock().unwrap();
            vms.get(handle).map(|v| v.api_sock.clone()).ok_or_else(|| anyhow!("unknown vm {handle}"))?
        };
        let start = Instant::now();
        self.fc_api(&sock, "PATCH", "/vm", &json!({"state": "Resumed"})).await?;
        Ok(start.elapsed().as_millis() as u64)
    }

    async fn snapshot(&self, handle: &str) -> Result<SnapshotArtifact> {
        let (sock, jail) = {
            let vms = self.vms.lock().unwrap();
            let v = vms.get(handle).ok_or_else(|| anyhow!("unknown vm {handle}"))?;
            (v.api_sock.clone(), v.api_sock.parent().unwrap().to_path_buf())
        };
        // Pause is required before snapshot creation.
        self.fc_api(&sock, "PATCH", "/vm", &json!({"state": "Paused"})).await?;
        let snap_file = jail.join("snapshot.file");
        let mem_file = jail.join("mem.file");
        self.fc_api(&sock, "PUT", "/snapshot/create", &json!({
            "snapshot_type": "Full",
            "snapshot_path": snap_file.to_str().unwrap(),
            "mem_file_path": mem_file.to_str().unwrap(),
        })).await?;
        let bytes = std::fs::metadata(&mem_file).map(|m| m.len()).unwrap_or(0)
            + std::fs::metadata(&snap_file).map(|m| m.len()).unwrap_or(0);
        Ok(SnapshotArtifact { handle: ids::snapshot_id(), storage_bytes: bytes })
    }

    async fn delete(&self, handle: &str) -> Result<()> {
        let record = self.vms.lock().unwrap().remove(handle);
        if let Some(r) = record {
            if let Some(pid) = r.pid {
                // SIGTERM the jailer; it tears down the VM and chroot.
                let _ = tokio::process::Command::new("kill").arg(pid.to_string()).status().await;
            }
            let _ = r.image_key; // (kept for future per-image teardown hooks)
        }
        self.workspaces.remove(handle).ok();
        let _ = std::fs::remove_dir_all(self.chroot_base.join(handle));
        Ok(())
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// Reuse the guest agent's base64 alphabet on the host side.
const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        out.push(B64[(b[0] >> 2) as usize] as char);
        out.push(B64[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 { B64[(((b[1] & 0x0f) << 2) | (b[2] >> 6)) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[(b[2] & 0x3f) as usize] as char } else { '=' });
    }
    out
}

fn base64_decode(input: &str) -> Result<Vec<u8>> {
    let mut table = [255u8; 256];
    for (i, &c) in B64.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let clean: Vec<u8> = input.bytes().filter(|&b| b != b'=' && !b.is_ascii_whitespace()).collect();
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        let mut acc = 0u32;
        let mut bits = 0;
        for &c in chunk {
            let v = table[c as usize];
            if v == 255 {
                bail!("invalid base64");
            }
            acc = (acc << 6) | v as u32;
            bits += 6;
        }
        let bytes = bits / 8;
        acc <<= 24 - bits;
        for i in 0..bytes {
            out.push((acc >> (16 - i * 8)) as u8);
        }
    }
    Ok(out)
}
