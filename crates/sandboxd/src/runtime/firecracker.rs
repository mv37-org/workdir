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
    DirEntry, ExecRequest, ExecResult, NetStats, PtySession, Runtime, SnapshotArtifact, VmInstance,
    VmSpec, WarmVm,
};
use crate::config::RuntimeConfig;
use crate::ids;
use crate::model::BootPath;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
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

#[derive(Serialize, Deserialize)]
struct VmRecord {
    /// Firecracker API control socket.
    api_sock: PathBuf,
    /// Host-side Unix socket that fronts the guest's vsock.
    vsock_uds: PathBuf,
    /// vsock uds path as Firecracker itself sees it (chroot-relative under the
    /// jailer); re-supplied to `/vsock` when restoring a standby snapshot.
    vsock_fc: String,
    /// Guest CID of the vsock device, re-supplied on restore.
    cid: u32,
    /// Jailer/Firecracker process id, for teardown. `None` once evicted to
    /// standby (the process is killed to free guest RAM).
    pid: Option<u32>,
    image_key: String,
    /// Host tap device for this VM's NIC, removed on teardown.
    tap: Option<String>,
    /// Tap allocation index; recreates the same MAC/IP on restore so the
    /// restored NIC matches the one captured in the snapshot.
    tap_idx: Option<u32>,
    /// Guest IP (on the bridge) the preview proxy dials for HTTP/VNC/CDP.
    guest_ip: Option<String>,
    /// Env applied to every exec: startup env + injected secrets. Kept in host
    /// memory and passed per-exec so secrets never persist to a guest env file
    /// or land in a snapshot (review M3).
    resident_env: std::collections::BTreeMap<String, String>,
    /// True while secret values are resident; snapshots are refused.
    has_secrets: bool,
    /// Parked in perpetual standby: snapshot on disk, RAM freed, tap torn down
    /// (roadmap Phase 1). `restore` brings it back.
    standby: bool,
    /// A base (Full) snapshot's mem.file already exists for this VM, so the next
    /// standby can take a fast Diff snapshot (only dirty pages) onto it.
    #[serde(default)]
    snapshotted: bool,
}

// --- sandbox networking (bridge + NAT) -------------------------------------
/// Host bridge sandbox taps attach to (set up by the installer / workdir-net).
const NET_BRIDGE: &str = "wdbr0";
/// Gateway = the bridge's host IP; guests route default through it.
const NET_GATEWAY: &str = "10.200.0.1";
const NET_DNS: &str = "1.1.1.1";

/// Guest IP for a tap index, from 10.200.0.0/16 (skipping .0 and the gateway .1).
fn guest_ip(index: u32) -> String {
    let n = index + 2;
    format!("10.200.{}.{}", (n >> 8) & 0xff, n & 0xff)
}

/// Deterministic locally-administered MAC for a guest.
fn guest_mac(index: u32) -> String {
    let n = index + 2;
    format!("06:00:0a:c8:{:02x}:{:02x}", (n >> 8) & 0xff, n & 0xff)
}

/// Run an `ip` command (needs CAP_NET_ADMIN on the daemon).
async fn run_ip(args: &[&str]) -> Result<()> {
    let status = tokio::process::Command::new("ip").args(args).status().await?;
    if !status.success() {
        bail!("ip {} failed", args.join(" "));
    }
    Ok(())
}

/// Create a host tap, attach it to the sandbox bridge as an isolated port (no
/// cross-tenant L2), and bring it up. Used on cold boot and recreated on restore
/// — the standby→resume path, where eviction tore the tap down and a resumed VM
/// would otherwise have no network (roadmap Phase 1).
async fn setup_tap(tap: &str) -> Result<()> {
    let _ = run_ip(&["link", "del", tap]).await; // clear any stale device
    run_ip(&["tuntap", "add", tap, "mode", "tap"]).await.context("create tap")?;
    run_ip(&["link", "set", tap, "master", NET_BRIDGE]).await.context("attach tap to bridge")?;
    run_ip(&["link", "set", tap, "type", "bridge_slave", "isolated", "on"]).await.context("isolate tap")?;
    run_ip(&["link", "set", tap, "up"]).await.context("bring tap up")?;
    Ok(())
}

/// Whether a host network device currently exists.
fn tap_exists(tap: &str) -> bool {
    std::path::Path::new(&format!("/sys/class/net/{tap}")).exists()
}

/// Pull a snapshot's memory file into the host page cache before a restore so
/// the guest's first accesses hit warm pages instead of stalling on disk reads
/// (roadmap Phase 2, lever #2 "keep the mem.file hot in page cache"). Best
/// effort: a failure here only costs latency, never correctness.
async fn prewarm_page_cache(path: &std::path::Path) {
    let path = path.to_path_buf();
    // Sequentially fault the file in on a blocking thread so the async runtime
    // is not stalled by the read. On Linux this populates the page cache that
    // the subsequent `File`/`Uffd` backend serves pages from.
    let _ = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open(&path) {
            let mut buf = vec![0u8; 1 << 20]; // 1 MiB
            while let Ok(n) = f.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        }
    })
    .await;
}

pub struct FirecrackerRuntime {
    firecracker_bin: String,
    jailer_bin: String,
    kernel_image: String,
    images_dir: PathBuf,
    workspaces: Workspaces,
    chroot_base: PathBuf,
    use_jailer: bool,
    jailer_uid_base: u32,
    /// Snapshot memory backend used on restore ("file" or "uffd"); Phase 2.
    restore_mem_backend: String,
    /// Warm the mem file into page cache before restore (Phase 2).
    prewarm_mem_cache: bool,
    /// Firecracker CPU template for snapshot portability (Phase 2, lever #4).
    cpu_template: String,
    /// Share one read-only base rootfs across VMs instead of a per-VM COW copy
    /// (Phase 3 density); the guest layers a tmpfs+overlayfs on top.
    shared_rootfs: bool,
    /// Backing images for persistent volumes (Phase 5); attached as extra drives.
    volumes_dir: PathBuf,
    /// Launch Firecracker with `--no-seccomp` (see config docs); needed for
    /// snapshot/create under the jailer on some kernels (firecracker#1088).
    no_seccomp: bool,
    next_cid: AtomicU32,
    next_tap: AtomicU32,
    /// Monotonic suffix for restore jail ids (the jailer refuses an existing
    /// chroot, so each restore gets a fresh one).
    next_restore: AtomicU32,
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
            use_jailer: cfg.use_jailer,
            jailer_uid_base: cfg.jailer_uid_base,
            restore_mem_backend: cfg.restore_mem_backend.clone(),
            prewarm_mem_cache: cfg.prewarm_mem_cache,
            cpu_template: cfg.cpu_template.clone(),
            shared_rootfs: cfg.shared_rootfs,
            volumes_dir: cfg.volumes_dir.clone(),
            no_seccomp: cfg.firecracker_no_seccomp,
            next_cid: AtomicU32::new(3), // CIDs 0-2 are reserved
            next_tap: AtomicU32::new(0),
            next_restore: AtomicU32::new(0),
            vms: Mutex::new(HashMap::new()),
        }
    }

    /// Whether THIS VM should share one read-only base + guest overlay (Phase 3
    /// density). Gated per image: only images whose `sandbox-init` can pivot into
    /// a tmpfs+overlayfs root qualify (base, browser). node-python/custom keep a
    /// per-VM writable COW copy, so enabling `shared_rootfs` globally never gives
    /// them a read-only root they can't write to.
    fn shared_for(&self, spec: &VmSpec) -> bool {
        self.shared_rootfs
            && crate::catalog::classify(&spec.image_key)
                .map(|c| c.supports_shared_rootfs())
                .unwrap_or(false)
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

    /// Connect to the Firecracker API socket, retrying to absorb the window
    /// between `bind()` (which creates the socket file) and `listen()` (which
    /// makes it accept), plus any startup latency. A bare connect right after
    /// the file appears can hit ECONNREFUSED.
    async fn fc_connect(sock: &PathBuf) -> Result<UnixStream> {
        let mut last: Option<std::io::Error> = None;
        for _ in 0..400 {
            match UnixStream::connect(sock).await {
                Ok(s) => return Ok(s),
                Err(e) => {
                    last = Some(e);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
        bail!("firecracker api socket {sock:?} never accepted a connection: {last:?}")
    }

    /// HTTP PUT/PATCH against the Firecracker API socket with the default read
    /// timeout. Firecracker answers most calls in well under a second.
    async fn fc_api(&self, sock: &PathBuf, method: &str, path: &str, body: &serde_json::Value) -> Result<()> {
        self.fc_api_to(sock, method, path, body, 10).await
    }

    /// Like [`fc_api`] but with an explicit read timeout. Snapshot create/load
    /// hold the connection open while Firecracker synchronously writes/maps the
    /// whole guest RAM (multiple GB), so they need a generous timeout — the
    /// previous fixed 5 s fired mid-snapshot, returning a false failure that then
    /// tore the VM down (the real cause of "snapshot under jailer fails").
    async fn fc_api_to(&self, sock: &PathBuf, method: &str, path: &str, body: &serde_json::Value, read_secs: u64) -> Result<()> {
        let mut stream = Self::fc_connect(sock).await?;
        let body_str = serde_json::to_string(body)?;
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body_str.len(),
            body_str
        );
        stream.write_all(req.as_bytes()).await?;
        stream.flush().await?;
        let mut resp = Vec::new();
        let mut buf = [0u8; 2048];
        loop {
            match tokio::time::timeout(Duration::from_secs(read_secs), stream.read(&mut buf)).await {
                Ok(Ok(0)) => break, // EOF
                Ok(Ok(n)) => {
                    resp.extend_from_slice(&buf[..n]);
                    // Return as soon as the full status line + headers are in: a
                    // 2xx needs nothing more. Waiting for the server to close the
                    // connection can add many seconds *after* a long snapshot
                    // (Firecracker delays the close) — that was the entire cause
                    // of the apparent multi-minute snapshot latency; the snapshot
                    // itself is ~20s. Errors carry a body, which Firecracker sends
                    // with the headers and then closes, so keep reading to capture
                    // the fault_message in that case.
                    if resp.windows(4).any(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&resp);
                        let line = head.lines().next().unwrap_or("");
                        if line.contains(" 200") || line.contains(" 201") || line.contains(" 204") {
                            return Ok(());
                        }
                        // non-2xx: fall through and keep reading the (small) body
                    }
                }
                Ok(Err(e)) => return Err(anyhow::Error::from(e).context("read firecracker api response")),
                Err(_) => break, // read timeout — proceed with what we have
            }
        }
        let text = String::from_utf8_lossy(&resp);
        let status = text.lines().next().unwrap_or("");
        // Include the response body (Firecracker's fault_message) after the
        // headers, so the error is actionable.
        let body = text.split("\r\n\r\n").nth(1).unwrap_or("").trim();
        bail!("firecracker api {method} {path} failed: {} {}", status.trim(), body);
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
        self.workspaces.create(&handle)?;
        let rootfs = self.rootfs_path(spec);

        // Per-VM tap on the host bridge, for NAT egress (common to both launch
        // modes). Snapshot restores reuse the snapshot's NIC, so tap only on a
        // fresh boot.
        let tap_idx = self.next_tap.fetch_add(1, Ordering::SeqCst);
        let tap = format!("wdtap{tap_idx}");
        let guest_ip = guest_ip(tap_idx);
        let guest_mac = guest_mac(tap_idx);
        if snapshot.is_none() {
            // Isolated bridge port: the guest can reach the gateway/uplink (NAT
            // egress) but NOT other sandboxes' taps — cross-tenant L2 isolation.
            setup_tap(&tap).await?;
        }

        // Launch Firecracker — directly, or wrapped by the jailer (chroot +
        // per-VM uid/gid + cgroups) when `use_jailer`. `*_fc` are the paths
        // Firecracker itself uses (chroot-relative under the jailer); `api_sock`
        // / `vsock_uds` are the host-side paths the daemon connects to.
        let (api_sock, vsock_uds, kernel_fc, rootfs_fc, vsock_fc, pid) = if self.use_jailer {
            // The jailer chroots to <base>/firecracker/<id>/root, drops to
            // uid/gid, and starts Firecracker there. Requires the daemon to run
            // as root. We stage the kernel + overlay into the chroot once it
            // exists, then reference them by chroot-relative path.
            let jail_id = handle.replace('_', "-");
            let uid = self.jailer_uid_base + tap_idx;
            let chroot_root = self.chroot_base.join("firecracker").join(&jail_id).join("root");
            let log = std::fs::File::create(jail.join("firecracker.log")).context("fc log")?;
            let log2 = log.try_clone().context("fc log clone")?;
            let child = tokio::process::Command::new(&self.jailer_bin)
                .args(["--id", &jail_id])
                .args(["--exec-file", &self.firecracker_bin])
                .args(["--uid", &uid.to_string(), "--gid", &uid.to_string()])
                .args(["--chroot-base-dir", self.chroot_base.to_str().unwrap()])
                .args(["--", "--api-sock", "api.sock"])
                .args(self.no_seccomp.then_some("--no-seccomp"))
                .stdout(log)
                .stderr(log2)
                .spawn()
                .context("spawn jailer (the daemon must run as root for the jailer)")?;
            let pid = child.id();
            for _ in 0..400 {
                if chroot_root.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let kdst = chroot_root.join("vmlinux");
            let rdst = chroot_root.join("rootfs.ext4");
            tokio::process::Command::new("cp").arg(&self.kernel_image).arg(&kdst).status().await
                .context("stage kernel into chroot")?;
            let owner = format!("{uid}:{uid}");
            let _ = tokio::process::Command::new("chown").arg(&owner).arg(&kdst).status().await;
            // Phase 3 density: with `shared_rootfs`, HARDLINK the read-only base
            // into the chroot — same inode → one copy in the host page cache
            // shared by every VM, instant, zero extra disk. No chown: that would
            // mutate the shared inode's owner; the base is world-readable so the
            // jailed uid still opens it read-only. The guest layers tmpfs+overlayfs
            // for writes (wd.overlay=tmpfs). Without sharing, give each VM its own
            // reflinked COW copy (chowned to the jailed uid) as before.
            if self.shared_for(spec) && std::fs::hard_link(&rootfs, &rdst).is_ok() {
                // shared inode staged (read-only, world-readable)
            } else {
                tokio::process::Command::new("cp").args(["--reflink=auto"]).arg(&rootfs).arg(&rdst).status().await
                    .context("stage rootfs into chroot")?;
                let _ = tokio::process::Command::new("chown").arg(&owner).arg(&rdst).status().await;
            }
            (
                chroot_root.join("api.sock"),
                chroot_root.join("vsock.sock"),
                "vmlinux".to_string(),
                "rootfs.ext4".to_string(),
                "vsock.sock".to_string(),
                pid,
            )
        } else {
            // Direct launch (default). The microVM is the isolation boundary.
            let api_sock = jail.join("api.sock");
            let vsock_uds = jail.join("vsock.sock");
            // Phase 3 density: with `shared_rootfs`, every VM mounts the SAME
            // read-only base rootfs (one copy in the host page cache, shared
            // across all sandboxes; DAX-mappable) and the guest layers a
            // tmpfs+overlayfs for writes — no per-VM rootfs copy at all.
            // Otherwise, give the VM its own reflinked COW overlay.
            let root_disk = if self.shared_for(spec) {
                rootfs.to_string_lossy().into_owned()
            } else {
                let overlay = jail.join("overlay.ext4");
                tokio::process::Command::new("cp").args(["--reflink=auto"]).arg(&rootfs).arg(&overlay).status().await
                    .context("create COW overlay (is the rootfs present?)")?;
                overlay.to_string_lossy().into_owned()
            };
            let _ = std::fs::remove_file(&api_sock);
            let log = std::fs::File::create(jail.join("firecracker.log")).context("fc log")?;
            let log2 = log.try_clone().context("fc log clone")?;
            let child = tokio::process::Command::new(&self.firecracker_bin)
                .args(["--api-sock", api_sock.to_str().unwrap()])
                .args(self.no_seccomp.then_some("--no-seccomp"))
                .stdout(log)
                .stderr(log2)
                .spawn()
                .context("spawn firecracker (requires /dev/kvm)")?;
            (
                api_sock,
                vsock_uds.clone(),
                self.kernel_image.clone(),
                root_disk,
                vsock_uds.to_string_lossy().into_owned(),
                child.id(),
            )
        };

        self.vms.lock().unwrap().insert(
            handle.clone(),
            VmRecord {
                api_sock: api_sock.clone(),
                vsock_uds: vsock_uds.clone(),
                vsock_fc: vsock_fc.clone(),
                cid,
                pid,
                image_key: spec.image_key.clone(),
                tap: if snapshot.is_none() { Some(tap.clone()) } else { None },
                tap_idx: if snapshot.is_none() { Some(tap_idx) } else { None },
                guest_ip: if snapshot.is_none() { Some(guest_ip.clone()) } else { None },
                resident_env: Default::default(),
                has_secrets: false,
                standby: false,
                snapshotted: false,
            },
        );
        self.persist_record(&handle);

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
            // uds_path is what Firecracker creates (chroot-relative under jailer).
            self.fc_api(&api_sock, "PUT", "/vsock", &json!({
                "guest_cid": cid,
                "uds_path": vsock_fc,
            })).await?;

            let boot_start = Instant::now();
            if let Some(snap) = snapshot {
                self.fc_api_to(&api_sock, "PUT", "/snapshot/load", &json!({
                    "snapshot_path": snap.join("snapshot.file").to_str().unwrap(),
                    "mem_backend": { "backend_path": snap.join("mem.file").to_str().unwrap(), "backend_type": "File" },
                    "enable_diff_snapshots": true,
                    "resume_vm": true,
                }), 300).await?;
            } else {
                let mut machine_cfg = json!({
                    "vcpu_count": vcpus,
                    "mem_size_mib": mem_mib,
                    "smt": false,
                    // Required for snapshots: without it, `snapshot/create` calls
                    // KVM_GET_DIRTY_LOG, which returns ENOENT (the memslots aren't
                    // dirty-logged) and Firecracker dies. Also enables diff
                    // snapshots (Phase 2). Must be set at boot — it can't be
                    // turned on for an already-running VM.
                    "track_dirty_pages": true,
                });
                // CPU template masks host CPUID to a portable baseline so a
                // snapshot restores across heterogeneous hosts (Phase 2 lever #4).
                if !self.cpu_template.is_empty() {
                    machine_cfg["cpu_template"] = json!(self.cpu_template);
                }
                self.fc_api(&api_sock, "PUT", "/machine-config", &machine_cfg).await?;
                // Network params are passed on the kernel cmdline; the guest init
                // configures eth0 from them (no in-guest DHCP needed). With a
                // shared read-only base, mount it `ro` and signal the guest init
                // to layer a tmpfs+overlayfs so writes land in RAM (Phase 3).
                let (root_mode, overlay_arg) = if self.shared_for(spec) {
                    ("ro", " wd.overlay=tmpfs")
                } else {
                    ("rw", "")
                };
                let boot_args = format!(
                    "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda {root_mode} \
                     wd.ip={guest_ip} wd.gw={NET_GATEWAY} wd.dns={NET_DNS}{overlay_arg} init=/sbin/sandbox-init"
                );
                self.fc_api(&api_sock, "PUT", "/boot-source", &json!({
                    "kernel_image_path": kernel_fc,
                    "boot_args": boot_args,
                })).await?;
                self.fc_api(&api_sock, "PUT", "/drives/rootfs", &json!({
                    "drive_id": "rootfs",
                    "path_on_host": rootfs_fc,
                    "is_root_device": true,
                    "is_read_only": self.shared_for(spec),
                })).await?;
                // Persistent volumes (Phase 5): stage each backing image into the
                // chroot (hardlink → writes hit the real file; chown to the jailed
                // uid) and attach it as an extra writable drive. The guest mounts
                // them by ext4 LABEL in `apply_features`. Volumes force cold boot,
                // so this fresh-boot path is the only place they're configured.
                for (i, va) in spec.volumes.iter().enumerate() {
                    let drive_id = format!("vol{i}");
                    let backing = self.volumes_dir.join(format!("{}.ext4", va.volume_id));
                    let path_on_host = if self.use_jailer {
                        let chroot_root = api_sock.parent().context("api_sock has no parent")?;
                        let dst = chroot_root.join(format!("{}.ext4", va.volume_id));
                        let _ = std::fs::remove_file(&dst);
                        std::fs::hard_link(&backing, &dst)
                            .with_context(|| format!("stage volume {} (allocated?)", va.volume_id))?;
                        let uid = self.jailer_uid_base + tap_idx;
                        let _ = tokio::process::Command::new("chown")
                            .arg(format!("{uid}:{uid}")).arg(&dst).status().await;
                        format!("{}.ext4", va.volume_id)
                    } else {
                        backing.to_string_lossy().into_owned()
                    };
                    self.fc_api(&api_sock, "PUT", &format!("/drives/{drive_id}"), &json!({
                        "drive_id": drive_id,
                        "path_on_host": path_on_host,
                        "is_root_device": false,
                        "is_read_only": false,
                    })).await?;
                }
                self.fc_api(&api_sock, "PUT", "/network-interfaces/eth0", &json!({
                    "iface_id": "eth0",
                    "host_dev_name": tap,
                    "guest_mac": guest_mac,
                })).await?;
                self.fc_api(&api_sock, "PUT", "/actions", &json!({
                    "action_type": "InstanceStart",
                })).await?;
            }

            // Wait for the guest agent. Env (and secrets) are NOT written into
            // the guest here; they are applied per-exec from the host record so
            // secrets never persist to a guest file (review M3, see
            // `apply_features`).
            let agent_ms = self.await_agent(&handle, Duration::from_secs(10)).await?;
            // boot_ms is the honest boot-to-ready time (config + VM boot +
            // agent up), not just the API-config latency.
            let boot_ms = boot_start.elapsed().as_millis() as u64;
            Ok((boot_ms, agent_ms))
        }
        .await;

        match booted {
            Ok((boot_ms, agent_ms)) => Ok((handle, boot_ms, agent_ms)),
            Err(e) => {
                // Surface Firecracker's own log so boot failures are diagnosable.
                let fc_log = std::fs::read_to_string(jail.join("firecracker.log")).unwrap_or_default();
                tracing::error!(
                    handle = %handle,
                    error = %e,
                    firecracker_log = %fc_log.lines().rev().take(8).collect::<Vec<_>>().join(" | "),
                    "microVM boot failed"
                );
                self.kill_and_reclaim(&handle, pid, &jail).await;
                Err(e)
            }
        }
    }

    /// Apply per-sandbox features to a booted (warm or cold) VM: resident env +
    /// secrets, inline ephemeral files, the opt-in coding agent, docker-in-docker,
    /// and bucket mounts. Returns the coding-agent install time (0 if not asked).
    async fn apply_features(&self, handle: &str, spec: &VmSpec) -> Result<u64> {
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

        // Coding agent (opt-in): install a lightweight agent CLI into the guest.
        // It is deliberately NOT baked into the rootfs, so we fetch it here only
        // when requested. Pre-staging the binary in a layered image is the
        // production speedup (see docs/FEATURES.md), but the honest default is to
        // install on demand and time it separately.
        let mut agent_ms = 0u64;
        if let Some(agent) = &spec.coding_agent {
            if agent.enabled {
                let t = Instant::now();
                let cmd = coding_agent_install_cmd(agent);
                let _ = self
                    .agent_call(handle, &json!({"op": "exec", "cmd": cmd, "background": false}))
                    .await;
                agent_ms = t.elapsed().as_millis() as u64;
            }
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

        // Persistent volumes: mount each attached block device by its ext4 LABEL
        // (assigned at volume-create), so the guest /dev/vdX ordering doesn't
        // matter. The fs already exists, so this is just mkdir + mount; data from
        // a prior attachment comes back intact.
        for va in &spec.volumes {
            let label = crate::ids::volume_label(&va.volume_id);
            let p = shell_quote(&va.mount_path);
            let _ = self
                .agent_call(handle, &json!({
                    "op": "exec",
                    "cmd": format!("mkdir -p {p} && mount LABEL={label} {p}"),
                    "background": false,
                }))
                .await;
        }
        Ok(agent_ms)
    }

    /// Kill a VM's jailer process and reclaim its jail dir + workspace + record.
    async fn kill_and_reclaim(&self, handle: &str, pid: Option<u32>, jail: &std::path::Path) {
        if let Some(pid) = pid {
            let _ = tokio::process::Command::new("kill").arg(pid.to_string()).status().await;
        }
        let tap = self.vms.lock().unwrap().remove(handle).and_then(|r| r.tap);
        if let Some(tap) = tap {
            let _ = run_ip(&["link", "del", &tap]).await;
        }
        let _ = std::fs::remove_dir_all(jail);
        if self.use_jailer {
            let _ = std::fs::remove_dir_all(self.jailer_chroot(handle));
        }
        self.workspaces.remove(handle).ok();
    }

    /// The jailer's chroot directory for a VM handle.
    fn jailer_chroot(&self, handle: &str) -> PathBuf {
        self.chroot_base.join("firecracker").join(handle.replace('_', "-"))
    }

    /// Per-VM jail directory (holds api.sock, the snapshot artifacts, and the
    /// persisted record).
    fn jail_dir(&self, handle: &str) -> PathBuf {
        self.chroot_base.join(handle)
    }

    /// Persist the in-memory record for `handle` to its jail dir, so a standby
    /// VM can be restored after a control-plane restart drops the in-memory map
    /// (roadmap Phase 1). Best effort.
    fn persist_record(&self, handle: &str) {
        let json = {
            let vms = self.vms.lock().unwrap();
            vms.get(handle).and_then(|r| serde_json::to_string(r).ok())
        };
        if let Some(json) = json {
            let _ = std::fs::write(self.jail_dir(handle).join("record.json"), json);
        }
    }

    /// Rehydrate `handle`'s record from disk if it is not resident (e.g. after a
    /// restart). No-op if already loaded or never persisted.
    fn ensure_record_loaded(&self, handle: &str) {
        {
            let vms = self.vms.lock().unwrap();
            if vms.contains_key(handle) {
                return;
            }
        }
        if let Ok(data) = std::fs::read_to_string(self.jail_dir(handle).join("record.json")) {
            if let Ok(rec) = serde_json::from_str::<VmRecord>(&data) {
                self.vms.lock().unwrap().insert(handle.to_string(), rec);
            }
        }
    }

    /// Pause a VM and snapshot (memory + device state) into `jail`, returning the
    /// (snapshot_file, mem_file) paths. Shared by `snapshot`, `standby`, `fork`.
    ///
    /// `diff` writes a Diff snapshot — only the pages dirtied since the load are
    /// written *onto the existing* mem.file (which holds the base), so an idle VM
    /// snapshots in ~1-2s instead of writing the full multi-GB image. The caller
    /// must guarantee a base mem.file already exists (a prior Full snapshot of
    /// the same VM, persisted across the restore via hardlinks).
    async fn write_snapshot(&self, sock: &PathBuf, jail: &std::path::Path, diff: bool) -> Result<(PathBuf, PathBuf)> {
        self.fc_api(sock, "PATCH", "/vm", &json!({"state": "Paused"})).await?;
        let snap_file = jail.join("snapshot.file");
        let mem_file = jail.join("mem.file");
        // Under the jailer, Firecracker is chrooted to `jail`, so it must be given
        // chroot-relative paths — it writes them inside the chroot it owns, which
        // lands them at `jail/<name>` on the host. A direct launch uses the
        // absolute host path. (Absolute paths handed to the chrooted process
        // resolve under the chroot and fail, which is the snapshot/create error
        // the jailer node hit.)
        let (snap_api, mem_api) = if self.use_jailer {
            ("snapshot.file".to_string(), "mem.file".to_string())
        } else {
            (snap_file.to_string_lossy().into_owned(), mem_file.to_string_lossy().into_owned())
        };
        self.fc_api_to(sock, "PUT", "/snapshot/create", &json!({
            "snapshot_type": if diff { "Diff" } else { "Full" },
            "snapshot_path": snap_api,
            "mem_file_path": mem_api,
        }), 300).await?;
        Ok((snap_file, mem_file))
    }
}

#[async_trait]
impl Runtime for FirecrackerRuntime {
    fn kind(&self) -> &'static str {
        "firecracker"
    }

    fn image_available(&self, image_key: &str) -> bool {
        // Curated images live at images_dir/<key>/rootfs.ext4.
        self.images_dir.join(image_key).join("rootfs.ext4").exists()
    }

    fn vm_net_stats(&self, handle: &str) -> Option<NetStats> {
        let tap = self.vms.lock().unwrap().get(handle).and_then(|r| r.tap.clone())?;
        // Host tap rx == guest egress, tx == guest ingress (mirrored from the VM).
        let stat = |f: &str| -> u64 {
            std::fs::read_to_string(format!("/sys/class/net/{tap}/statistics/{f}"))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0)
        };
        Some(NetStats { rx_bytes: stat("rx_bytes"), tx_bytes: stat("tx_bytes") })
    }

    fn gc_stale_jails(&self) -> usize {
        // Build the set of directories belonging to live VMs: the per-VM jail dir
        // (chroot_base/<handle>) and the active chroot (the firecracker/<id> dir
        // derived from the api sock — possibly a `-rN` restore chroot).
        let (live_jails, live_chroots): (HashSet<PathBuf>, HashSet<PathBuf>) = {
            let vms = self.vms.lock().unwrap();
            let mut jails = HashSet::new();
            let mut chroots = HashSet::new();
            for (handle, rec) in vms.iter() {
                jails.insert(self.chroot_base.join(handle));
                if let Some(c) = rec.api_sock.parent().and_then(|p| p.parent()) {
                    chroots.insert(c.to_path_buf());
                }
            }
            (jails, chroots)
        };
        // Only sweep dirs older than this, so a VM mid-boot (its dir created just
        // before its record is inserted) is never reclaimed.
        let stale = |p: &Path| -> bool {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .map(|d| d.as_secs() > 120)
                .unwrap_or(false)
        };
        let mut removed = 0;
        // Per-VM jail dirs directly under chroot_base (skip the `firecracker`
        // container dir and the snapshots cache).
        if let Ok(entries) = std::fs::read_dir(&self.chroot_base) {
            for e in entries.flatten() {
                let p = e.path();
                let name = e.file_name();
                if name == "firecracker" || name == "snapshots" {
                    continue;
                }
                if !live_jails.contains(&p) && stale(&p) && std::fs::remove_dir_all(&p).is_ok() {
                    removed += 1;
                }
            }
        }
        // Jailer chroots under chroot_base/firecracker/<id>.
        if let Ok(entries) = std::fs::read_dir(self.chroot_base.join("firecracker")) {
            for e in entries.flatten() {
                let p = e.path();
                if !live_chroots.contains(&p) && stale(&p) && std::fs::remove_dir_all(&p).is_ok() {
                    removed += 1;
                }
            }
        }
        removed
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

        // Apply secrets/env, inline files, the coding agent, docker, and mounts to
        // whichever VM we ended up with (warm or freshly booted).
        let coding_agent_ms = self.apply_features(&handle, spec).await?;

        let browser_ready_ms = if spec.browser.as_ref().map(|b| b.enabled).unwrap_or(false) {
            let start = Instant::now();
            let _ = self
                .agent_call(&handle, &json!({"op": "ready_http", "url": "http://127.0.0.1:9222/json/version", "timeout_seconds": 30}))
                .await;
            start.elapsed().as_millis() as u64
        } else {
            0
        };
        Ok(VmInstance { handle, boot_path, boot_ms, image_cache_ms: 0, browser_ready_ms, agent_ms: coding_agent_ms })
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

    async fn expose_port(&self, handle: &str, port: u16) -> Result<SocketAddr> {
        // Services run INSIDE the guest, on its bridge IP (10.200.0.x) — not on
        // the host loopback. The host reaches it directly over wdbr0. Return the
        // guest IP so the preview proxy dials the VM, not the host.
        let ip = self.vms.lock().unwrap().get(handle).and_then(|r| r.guest_ip.clone());
        match ip.and_then(|s| s.parse::<std::net::IpAddr>().ok()) {
            Some(ip) => Ok(SocketAddr::new(ip, port)),
            None => Ok(SocketAddr::from(([127, 0, 0, 1], port))), // snapshot/no-tap fallback
        }
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

    async fn standby(&self, handle: &str) -> Result<u64> {
        // Perpetual standby (roadmap Phase 1): snapshot the VM to disk, then kill
        // the Firecracker process so its guest RAM returns to the host, and tear
        // down the tap. The record is kept (snapshot artifacts + NIC identity) so
        // `restore` can bring the exact same VM back.
        let (sock, jail, pid, tap, has_secrets, snapshotted) = {
            let vms = self.vms.lock().unwrap();
            let v = vms.get(handle).ok_or_else(|| anyhow!("unknown vm {handle}"))?;
            (
                v.api_sock.clone(),
                v.api_sock.parent().unwrap().to_path_buf(),
                v.pid,
                v.tap.clone(),
                v.has_secrets,
                v.snapshotted,
            )
        };
        // Refuse to persist resident secrets into a snapshot (review M3).
        if has_secrets {
            bail!("cannot standby a sandbox with resident secrets; remove secrets first");
        }
        let start = Instant::now();
        // Diff snapshot once a base exists (fast — only dirty pages); Full the
        // first time. mem.file persists across the restore via hardlinks, so the
        // Diff updates the current memory image in place.
        self.write_snapshot(&sock, &jail, snapshotted).await?;
        // Free the guest RAM: SIGKILL Firecracker. The mem.file on disk now holds
        // the guest memory image.
        if let Some(pid) = pid {
            let _ = tokio::process::Command::new("kill").arg("-9").arg(pid.to_string()).status().await;
        }
        // Reclaim the tap too; `restore` recreates it (the bug the roadmap calls
        // out: a resumed VM must get its host networking back).
        if let Some(tap) = &tap {
            let _ = run_ip(&["link", "del", tap]).await;
        }
        if let Some(v) = self.vms.lock().unwrap().get_mut(handle) {
            v.pid = None;
            v.standby = true;
            v.snapshotted = true; // a base mem.file now exists → next standby is a Diff
        }
        // Persist the now-standby record so a restart can still restore it.
        self.persist_record(handle);
        Ok(start.elapsed().as_millis() as u64)
    }

    async fn restore(&self, handle: &str) -> Result<u64> {
        // Bring a standby VM back from its on-disk snapshot. Recreate the host
        // tap (the eviction tore it down), relaunch Firecracker, load the
        // snapshot, and wait for the guest agent. After a control-plane restart
        // the in-memory record is gone, so rehydrate it from disk first.
        self.ensure_record_loaded(handle);
        let (api_sock, jail, vsock_uds, tap, tap_idx) = {
            let vms = self.vms.lock().unwrap();
            let v = vms.get(handle).ok_or_else(|| anyhow!("unknown vm {handle} (no persisted record)"))?;
            (
                v.api_sock.clone(),
                v.api_sock.parent().unwrap().to_path_buf(),
                v.vsock_uds.clone(),
                v.tap.clone(),
                v.tap_idx,
            )
        };
        let start = Instant::now();

        // Recreate the tap (same name → same MAC/IP, so it matches the NIC saved
        // in the snapshot) if eviction removed it. THIS is the tap-recreation the
        // roadmap calls for; without it a resumed VM has no network.
        if let Some(tap) = &tap {
            if !tap_exists(tap) {
                setup_tap(tap).await.context("recreate tap on restore")?;
            }
        }

        // Jailer restore: relaunch under the jailer in a FRESH chroot (the jailer
        // refuses an existing chroot dir, so the original can't be reused). The
        // snapshot artifacts are HARDLINKED into the new chroot — same inode, no
        // multi-GB copy, and same owner uid as the new (dropped) Firecracker, so
        // it can read them. Paths are chroot-relative.
        if self.use_jailer {
            let n = self.next_restore.fetch_add(1, Ordering::SeqCst);
            let jail_id = format!("{}-r{}", handle.replace('_', "-"), n);
            let uid = self.jailer_uid_base + tap_idx.unwrap_or(0);
            let new_chroot = self.chroot_base.join("firecracker").join(&jail_id).join("root");
            let log = std::fs::File::create(jail.join("restore.log")).context("restore log")?;
            let log2 = log.try_clone()?;
            let pid = tokio::process::Command::new(&self.jailer_bin)
                .args(["--id", &jail_id])
                .args(["--exec-file", &self.firecracker_bin])
                .args(["--uid", &uid.to_string(), "--gid", &uid.to_string()])
                .args(["--chroot-base-dir", self.chroot_base.to_str().unwrap()])
                .args(["--", "--api-sock", "api.sock"])
                .args(self.no_seccomp.then_some("--no-seccomp"))
                .stdout(log).stderr(log2)
                .spawn().context("relaunch jailer for restore")?
                .id();
            for _ in 0..400 {
                if new_chroot.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // Stage the snapshot artifacts AND the VM's disk into the new chroot
            // (hardlink, instant — same inode, so writes still hit the one disk).
            // The snapshot's block device is backed by "rootfs.ext4"; without it
            // the load fails ("backing file ... No such file or directory").
            let stage = |name: &str| -> Result<()> {
                let dst = new_chroot.join(name);
                let _ = std::fs::remove_file(&dst);
                std::fs::hard_link(jail.join(name), &dst)
                    .or_else(|_| std::fs::copy(jail.join(name), &dst).map(|_| ()))
                    .with_context(|| format!("stage {name} into restore chroot"))
            };
            let new_mem = new_chroot.join("mem.file");
            stage("snapshot.file")?;
            stage("mem.file")?;
            stage("rootfs.ext4")?;
            let new_api = new_chroot.join("api.sock");
            for _ in 0..400 {
                if new_api.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // Warm the mem file into page cache in the BACKGROUND — the snapshot
            // loads with a `File` backend (mmap), so the guest demand-pages from
            // mem.file lazily; doing the prewarm synchronously here forced an
            // eager multi-GB read that dominated a cold restore (~1.3s → ~250ms
            // once moved off the critical path). The background warm just lowers
            // the latency of the guest's first faults.
            if self.prewarm_mem_cache {
                let m = new_mem.clone();
                tokio::spawn(async move { prewarm_page_cache(&m).await });
            }
            // Load FIRST: a snapshot restores all its devices (incl. the vsock,
            // recreated at its stored relative "vsock.sock" inside this chroot).
            // Configuring anything beforehand makes Firecracker reject the load
            // ("not allowed after configuring boot-specific resources").
            self.fc_api_to(&new_api, "PUT", "/snapshot/load", &json!({
                "snapshot_path": "snapshot.file",
                "mem_backend": { "backend_path": "mem.file", "backend_type": "File" },
                "enable_diff_snapshots": true,
                "resume_vm": true,
            }), 300).await?;
            if let Some(v) = self.vms.lock().unwrap().get_mut(handle) {
                v.api_sock = new_api;
                v.vsock_uds = new_chroot.join("vsock.sock");
                v.vsock_fc = "vsock.sock".to_string();
                v.pid = pid;
                v.standby = false;
                if let Some(idx) = tap_idx {
                    v.guest_ip = Some(guest_ip(idx));
                }
            }
            self.persist_record(handle);
            self.await_agent(handle, Duration::from_secs(15)).await?;
            return Ok(start.elapsed().as_millis() as u64);
        }

        // Direct (non-jailer) restore: relaunch an unchrooted Firecracker with
        // ABSOLUTE host paths so the api sock / vsock uds / snapshot / mem land
        // exactly where the host expects.
        let _ = std::fs::remove_file(&api_sock);
        let _ = std::fs::remove_file(&vsock_uds); // clear the evicted run's stale socket
        let log = std::fs::File::create(jail.join("restore.log")).context("restore log")?;
        let log2 = log.try_clone().context("restore log clone")?;
        let child = tokio::process::Command::new(&self.firecracker_bin)
            .args(["--api-sock", api_sock.to_str().unwrap()])
            .args(self.no_seccomp.then_some("--no-seccomp"))
            .stdout(log)
            .stderr(log2)
            .spawn()
            .context("relaunch firecracker for restore (requires /dev/kvm)")?;
        let pid = child.id();
        for _ in 0..400 {
            if api_sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Load FIRST (no device config beforehand): the snapshot restores the
        // vsock device at its stored absolute uds path, where the host connects.
        let snap_file = jail.join("snapshot.file");
        let mem_file = jail.join("mem.file");
        // Warm the mem file in the BACKGROUND (off the resume critical path); the
        // File backend demand-pages lazily, so an eager read here only slows the
        // restore.
        if self.prewarm_mem_cache {
            let m = mem_file.clone();
            tokio::spawn(async move { prewarm_page_cache(&m).await });
        }
        // Phase 2 lever #1: UFFD demand-paging returns the guest before its full
        // working set is resident. Selecting "uffd" requires a userfaultfd page
        // handler listening on the backend socket to serve faults from mem.file;
        // that handler is the remaining KVM-host increment. Until it lands we
        // fall back to the eager "File" backend (which `prewarm_page_cache` above
        // already makes warm) rather than configure a backend with no handler,
        // which would hang the restore.
        let backend_type = if self.restore_mem_backend.eq_ignore_ascii_case("uffd") {
            tracing::warn!("restore_mem_backend=uffd selected but the userfaultfd handler is not yet implemented; using File (page-cache prewarmed)");
            "File"
        } else {
            "File"
        };
        self.fc_api_to(&api_sock, "PUT", "/snapshot/load", &json!({
            "snapshot_path": snap_file.to_str().unwrap(),
            "mem_backend": { "backend_path": mem_file.to_str().unwrap(), "backend_type": backend_type },
            "enable_diff_snapshots": true,
            "resume_vm": true,
        }), 300).await?;

        // Update the record before the agent wait so a failure still leaves a
        // killable pid.
        if let Some(v) = self.vms.lock().unwrap().get_mut(handle) {
            v.pid = pid;
            v.standby = false;
            if let Some(idx) = tap_idx {
                v.guest_ip = Some(guest_ip(idx));
            }
        }
        self.persist_record(handle);
        // The guest agent should answer almost immediately on a warm restore.
        self.await_agent(handle, Duration::from_secs(10)).await?;
        Ok(start.elapsed().as_millis() as u64)
    }

    async fn snapshot(&self, handle: &str) -> Result<SnapshotArtifact> {
        let (sock, jail) = {
            let vms = self.vms.lock().unwrap();
            let v = vms.get(handle).ok_or_else(|| anyhow!("unknown vm {handle}"))?;
            (v.api_sock.clone(), v.api_sock.parent().unwrap().to_path_buf())
        };
        let (snap_file, mem_file) = self.write_snapshot(&sock, &jail, false).await?;
        let bytes = std::fs::metadata(&mem_file).map(|m| m.len()).unwrap_or(0)
            + std::fs::metadata(&snap_file).map(|m| m.len()).unwrap_or(0);
        Ok(SnapshotArtifact { handle: ids::snapshot_id(), storage_bytes: bytes })
    }

    async fn fork(&self, parent_handle: &str, child_spec: &VmSpec) -> Result<VmInstance> {
        // Clone a sibling from the parent's live snapshot (roadmap Phase 3). The
        // parent keeps running; the child gets its own disk copy, host tap, CID,
        // and (re-IP'd) guest networking.
        let (parent_sock, parent_jail, parent_image, has_secrets) = {
            let vms = self.vms.lock().unwrap();
            let v = vms.get(parent_handle).ok_or_else(|| anyhow!("unknown parent vm {parent_handle}"))?;
            (
                v.api_sock.clone(),
                v.api_sock.parent().unwrap().to_path_buf(),
                v.image_key.clone(),
                v.has_secrets,
            )
        };
        if has_secrets {
            bail!("cannot fork a sandbox with resident secrets; they would be copied into the child");
        }
        let start = Instant::now();

        // Snapshot the parent, then resume it so the fork is non-disruptive.
        let (parent_snap, parent_mem) = self.write_snapshot(&parent_sock, &parent_jail, false).await?;
        self.fc_api(&parent_sock, "PATCH", "/vm", &json!({"state": "Resumed"})).await?;

        // Stage the child VM (its own id, tap, CID, and a private disk copy).
        let child = format!("vm_{}", child_spec.sandbox_id);
        let child_jail = self.chroot_base.join(&child);
        std::fs::create_dir_all(&child_jail).context("create child jail")?;
        self.workspaces.create(&child)?;
        let tap_idx = self.next_tap.fetch_add(1, Ordering::SeqCst);
        let tap = format!("wdtap{tap_idx}");
        let child_ip = guest_ip(tap_idx);
        let cid = self.next_cid.fetch_add(1, Ordering::SeqCst);
        setup_tap(&tap).await.context("fork: child tap")?;

        // Bring up the child Firecracker (jailer-aware, mirroring restore) and
        // stage artifacts: snapshot + mem are COPIED (the child is independent),
        // the rootfs is reflink-copied so the child gets its OWN writable disk.
        let (child_sock, child_vsock, pid) = if self.use_jailer {
            let jail_id = child.replace('_', "-");
            let uid = self.jailer_uid_base + tap_idx;
            let new_chroot = self.chroot_base.join("firecracker").join(&jail_id).join("root");
            let log = std::fs::File::create(child_jail.join("firecracker.log")).context("child fc log")?;
            let log2 = log.try_clone()?;
            let pid = tokio::process::Command::new(&self.jailer_bin)
                .args(["--id", &jail_id])
                .args(["--exec-file", &self.firecracker_bin])
                .args(["--uid", &uid.to_string(), "--gid", &uid.to_string()])
                .args(["--chroot-base-dir", self.chroot_base.to_str().unwrap()])
                .args(["--", "--api-sock", "api.sock"])
                .args(self.no_seccomp.then_some("--no-seccomp"))
                .stdout(log).stderr(log2)
                .spawn().context("spawn child jailer")?
                .id();
            for _ in 0..400 {
                if new_chroot.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // mem.file and rootfs are the big artifacts — reflink-copy them so the
            // child gets an instant CoW private copy (it diverges independently)
            // instead of a multi-second full read+write of the 2 GB mem image.
            let _ = std::fs::copy(&parent_snap, new_chroot.join("snapshot.file"));
            let reflink = |from: PathBuf, to: PathBuf| async move {
                let _ = tokio::process::Command::new("cp").args(["--reflink=auto"]).arg(from).arg(to).status().await;
            };
            reflink(parent_mem.clone(), new_chroot.join("mem.file")).await;
            reflink(parent_jail.join("rootfs.ext4"), new_chroot.join("rootfs.ext4")).await;
            let owner = format!("{uid}:{uid}");
            let _ = tokio::process::Command::new("chown").arg("-R").arg(&owner).arg(&new_chroot).status().await;
            let new_api = new_chroot.join("api.sock");
            for _ in 0..400 {
                if new_api.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            (new_api, new_chroot.join("vsock.sock"), pid)
        } else {
            let _ = std::fs::copy(&parent_snap, child_jail.join("snapshot.file"));
            let _ = tokio::process::Command::new("cp").args(["--reflink=auto"])
                .arg(&parent_mem).arg(child_jail.join("mem.file")).status().await;
            if parent_jail.join("overlay.ext4").exists() {
                let _ = tokio::process::Command::new("cp").args(["--reflink=auto"])
                    .arg(parent_jail.join("overlay.ext4")).arg(child_jail.join("overlay.ext4")).status().await;
            }
            let child_sock = child_jail.join("api.sock");
            let _ = std::fs::remove_file(&child_sock);
            let log = std::fs::File::create(child_jail.join("firecracker.log")).context("child fc log")?;
            let log2 = log.try_clone()?;
            let pid = tokio::process::Command::new(&self.firecracker_bin)
                .args(["--api-sock", child_sock.to_str().unwrap()])
                .args(self.no_seccomp.then_some("--no-seccomp"))
                .stdout(log).stderr(log2)
                .spawn().context("spawn child firecracker")?
                .id();
            for _ in 0..400 {
                if child_sock.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            (child_sock, child_jail.join("vsock.sock"), pid)
        };

        // Warm the child's mem file in the background, then load *paused* — load
        // FIRST (the snapshot restores its own vsock); paths are chroot-relative
        // under the jailer, absolute for a direct launch.
        let (snap_api, mem_api) = if self.use_jailer {
            ("snapshot.file".to_string(), "mem.file".to_string())
        } else {
            (
                child_jail.join("snapshot.file").to_string_lossy().into_owned(),
                child_jail.join("mem.file").to_string_lossy().into_owned(),
            )
        };
        if self.prewarm_mem_cache {
            if let Some(m) = child_sock.parent().map(|p| p.join("mem.file")) {
                tokio::spawn(async move { prewarm_page_cache(&m).await });
            }
        }
        // The snapshot's NIC references the *parent's* tap, which the parent is
        // still holding (fork keeps the parent live) — so reopening it during
        // restore would hit EBUSY. `network_overrides` remaps eth0 to the child's
        // own tap at load time, so it opens a free device and resumes directly.
        self.fc_api_to(&child_sock, "PUT", "/snapshot/load", &json!({
            "snapshot_path": snap_api,
            "mem_backend": { "backend_path": mem_api, "backend_type": "File" },
            "enable_diff_snapshots": true,
            "network_overrides": [{ "iface_id": "eth0", "host_dev_name": tap.clone() }],
            "resume_vm": true,
        }), 300).await?;

        self.vms.lock().unwrap().insert(
            child.clone(),
            VmRecord {
                api_sock: child_sock,
                vsock_uds: child_vsock.clone(),
                vsock_fc: "vsock.sock".to_string(),
                cid,
                pid,
                image_key: parent_image,
                tap: Some(tap),
                tap_idx: Some(tap_idx),
                guest_ip: Some(child_ip.clone()),
                resident_env: Default::default(),
                has_secrets: false,
                standby: false,
                snapshotted: false,
            },
        );
        self.persist_record(&child);
        self.await_agent(&child, Duration::from_secs(10)).await?;
        // Re-IP the guest: the snapshot carried the parent's address, which would
        // collide on the bridge. Flushing eth0 also drops the connected /16 route
        // and with it the default route, so re-add `default via the gateway` or
        // the child loses egress/DNS. (MAC is inherited; the isolated tap keeps
        // that from mattering. A distinct MAC on fork is a follow-up.)
        let _ = self.agent_call(&child, &json!({
            "op": "exec",
            "cmd": format!(
                "ip addr flush dev eth0 2>/dev/null; ip addr add {child_ip}/16 dev eth0 2>/dev/null; \
                 ip link set eth0 up 2>/dev/null; \
                 ip route replace default via {NET_GATEWAY} dev eth0 2>/dev/null; true"
            ),
            "background": false,
        })).await;
        // Apply the child's own env/files/agent/mounts on top of the inherited state.
        let agent_ms = self.apply_features(&child, child_spec).await?;

        Ok(VmInstance {
            handle: child,
            boot_path: BootPath::Fork,
            boot_ms: start.elapsed().as_millis() as u64,
            image_cache_ms: 0,
            browser_ready_ms: 0,
            agent_ms,
        })
    }

    async fn delete(&self, handle: &str) -> Result<()> {
        let record = self.vms.lock().unwrap().remove(handle);
        // The active chroot may be a `-rN` restore chroot, not the original
        // handle-derived one; clean whatever the record's api sock points at
        // (chroot_base/firecracker/<id>) so restores don't leak chroots.
        let mut active_chroot: Option<PathBuf> = None;
        if let Some(r) = record {
            if let Some(pid) = r.pid {
                // SIGKILL Firecracker; it tears the VM down with it.
                let _ = tokio::process::Command::new("kill").arg("-9").arg(pid.to_string()).status().await;
            }
            if let Some(tap) = r.tap {
                let _ = run_ip(&["link", "del", &tap]).await;
            }
            // api_sock = <chroot_base>/firecracker/<id>/root/api.sock → remove the
            // <id> directory.
            active_chroot = r.api_sock.parent().and_then(|p| p.parent()).map(|p| p.to_path_buf());
            let _ = r.image_key; // (kept for future per-image teardown hooks)
        }
        self.workspaces.remove(handle).ok();
        let _ = std::fs::remove_dir_all(self.chroot_base.join(handle));
        if self.use_jailer {
            let _ = std::fs::remove_dir_all(self.jailer_chroot(handle));
            if let Some(c) = active_chroot {
                let _ = std::fs::remove_dir_all(c);
            }
        }
        Ok(())
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build the in-guest install command for the requested coding agent. Only
/// opencode is supported today (validated upstream in the service layer). The
/// installer drops the binary under `$HOME/.opencode/bin` (or `~/.local/bin`);
/// we symlink it onto the PATH so plain `opencode` works from every `exec`.
fn coding_agent_install_cmd(agent: &crate::model::CodingAgentConfig) -> String {
    let version_export = match &agent.version {
        Some(v) => format!("export VERSION={}; ", shell_quote(v)),
        None => String::new(),
    };
    format!(
        "{version_export}curl -fsSL https://opencode.ai/install | bash; \
         for d in \"$HOME/.opencode/bin\" \"$HOME/.local/bin\"; do \
           [ -x \"$d/opencode\" ] && ln -sf \"$d/opencode\" /usr/local/bin/opencode && break; \
         done"
    )
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
