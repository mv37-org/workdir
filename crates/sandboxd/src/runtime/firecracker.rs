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
    /// Persistent volumes attached at boot (Phase 5). Restores must re-stage the
    /// backing files into the fresh chroot — the snapshot's drives reference them
    /// by chroot-relative name.
    #[serde(default)]
    volumes: Vec<VolumeStage>,
    /// Current balloon target in MiB (0 = deflated). Soft-standby bookkeeping.
    #[serde(default)]
    ballooned_mib: u32,
}

/// One attached persistent volume as the runtime staged it.
#[derive(Serialize, Deserialize, Clone)]
struct VolumeStage {
    /// Backing file name as the drive references it (chroot-relative under the
    /// jailer), e.g. `vol_ab12cd.ext4`.
    file: String,
    /// The real backing image under `volumes_dir`. Hardlinked into the chroot so
    /// guest writes land on the one persistent inode.
    host_path: PathBuf,
    /// Guest mount point.
    mount_path: String,
    /// ext4 label (set at mkfs) so the guest can mount by label, with the
    /// virtio device path as fallback.
    label: String,
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

/// Whether `pid` is a live firecracker/jailer process. Used when rehydrating a
/// persisted record after a daemon restart: a pid the kernel has recycled (or
/// from before a host reboot) must not be killed on the record's behalf. On
/// hosts without procfs this is always false, which only costs an orphan-kill.
fn pid_is_firecracker(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(comm) => matches!(comm.trim(), "firecracker" | "jailer"),
        Err(_) => false,
    }
}

/// Poll `cond` with exponential backoff (1 ms → 20 ms cap) until it holds or
/// `max_wait` elapses; returns whether it held. The fixed 10 ms sleeps this
/// replaces quantized every socket/chroot wait up to a full tick — tens of
/// milliseconds of pure waiting on the boot/restore/fork critical paths.
async fn poll_until(max_wait: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    let mut delay = Duration::from_millis(1);
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() >= max_wait {
            return false;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(20));
    }
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
    /// Persistent-volume backing images (Phase 5).
    volumes_dir: PathBuf,
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
    /// Attach a virtio-balloon to every VM (soft standby + guest mem stats).
    balloon: bool,
    /// Append `quiet loglevel=1` to the guest cmdline (skip serial boot logging).
    quiet_boot: bool,
    /// Launch Firecracker with `--no-seccomp` (see config docs); needed for
    /// snapshot/create under the jailer on some kernels (firecracker#1088).
    no_seccomp: bool,
    next_cid: AtomicU32,
    next_tap: AtomicU32,
    /// Monotonic suffix for restore jail ids (the jailer refuses an existing
    /// chroot, so each restore gets a fresh one).
    next_restore: AtomicU32,
    /// Serializes golden-snapshot production (one throwaway VM at a time).
    golden_lock: tokio::sync::Mutex<()>,
    /// Pre-spawned idle jailer+Firecracker processes (api.sock listening),
    /// claimed by restore/golden boots to skip the ~30 ms jailer relaunch.
    /// Empty unless `jailer_pool_size > 0`; refilled by `maintain`.
    jail_pool: Mutex<Vec<PooledJail>>,
    jailer_pool_size: u32,
    next_pool: AtomicU32,
    vms: Mutex<HashMap<String, VmRecord>>,
}

/// One pre-spawned jailer+Firecracker, configless until a snapshot load claims
/// it.
struct PooledJail {
    /// `pool-N`; its chroot lives at `chroot_base/firecracker/pool-N`.
    jail_id: String,
    chroot_root: PathBuf,
    uid: u32,
    pid: Option<u32>,
}

impl FirecrackerRuntime {
    pub fn new(cfg: &RuntimeConfig) -> FirecrackerRuntime {
        let chroot_base = cfg.workspace_dir.join("jail");
        // Rehydrate persisted per-VM records EAGERLY. Lazy rehydration (only on
        // the first restore) left two restart bugs:
        //  - `gc_stale_jails` judges liveness by the in-memory map, so ~5 min
        //    after a restart the sweeper deleted every parked standby VM's
        //    record.json and snapshot artifacts — "perpetual" standby quietly
        //    became "standby until the next deploy plus five minutes".
        //  - the tap/CID/restore counters restarted at zero, so fresh boots
        //    reused tap names and guest IPs still owned by parked VMs
        //    (`setup_tap`'s `link del` then yanks a restored VM's NIC), and
        //    restores reused `-rN` chroot names the jailer refuses.
        let (vms, next_tap, next_cid, next_restore) = Self::rehydrate(&chroot_base);
        FirecrackerRuntime {
            firecracker_bin: cfg.firecracker_bin.clone(),
            jailer_bin: cfg.jailer_bin.clone(),
            kernel_image: cfg.kernel_image.clone(),
            images_dir: cfg.images_dir.clone(),
            volumes_dir: cfg.volumes_dir.clone(),
            workspaces: Workspaces::new(cfg.workspace_dir.clone()),
            chroot_base,
            use_jailer: cfg.use_jailer,
            jailer_uid_base: cfg.jailer_uid_base,
            restore_mem_backend: cfg.restore_mem_backend.clone(),
            prewarm_mem_cache: cfg.prewarm_mem_cache,
            cpu_template: cfg.cpu_template.clone(),
            shared_rootfs: cfg.shared_rootfs,
            balloon: cfg.balloon,
            quiet_boot: cfg.quiet_guest_boot,
            no_seccomp: cfg.firecracker_no_seccomp,
            next_cid: AtomicU32::new(next_cid),
            next_tap: AtomicU32::new(next_tap),
            next_restore: AtomicU32::new(next_restore),
            golden_lock: tokio::sync::Mutex::new(()),
            jail_pool: Mutex::new(Vec::new()),
            jailer_pool_size: cfg.jailer_pool_size,
            next_pool: AtomicU32::new(0),
            vms: Mutex::new(vms),
        }
    }

    /// Load every persisted `record.json` under `chroot_base` and derive safe
    /// starting points for the tap/CID/restore counters (always above anything a
    /// surviving record or restore chroot still uses). Records of VMs that are
    /// neither parked in standby nor backed by a live process are tombstones —
    /// their record.json is removed so the jail GC can reclaim the dirs.
    fn rehydrate(chroot_base: &Path) -> (HashMap<String, VmRecord>, u32, u32, u32) {
        let mut vms = HashMap::new();
        let mut next_tap = 0u32;
        let mut next_cid = 3u32; // CIDs 0-2 are reserved
        if let Ok(entries) = std::fs::read_dir(chroot_base) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name == "firecracker" || name == "snapshots" || name == "jailer-pool" {
                    continue;
                }
                let record_path = e.path().join("record.json");
                let Ok(data) = std::fs::read_to_string(&record_path) else { continue };
                let Ok(mut rec) = serde_json::from_str::<VmRecord>(&data) else { continue };
                // A pid from a previous daemon run is only trustworthy while it
                // still names a live firecracker/jailer process; a recycled pid
                // must never be killed on the record's behalf.
                if let Some(pid) = rec.pid {
                    if !pid_is_firecracker(pid) {
                        rec.pid = None;
                    }
                }
                if !rec.standby && rec.pid.is_none() {
                    // Dead, unrestorable VM (it was running when the previous
                    // daemon died). Drop the tombstone; the GC reclaims the dirs.
                    let _ = std::fs::remove_file(&record_path);
                    continue;
                }
                if let Some(idx) = rec.tap_idx {
                    next_tap = next_tap.max(idx + 1);
                }
                next_cid = next_cid.max(rec.cid + 1);
                vms.insert(name, rec);
            }
        }
        // Restore chroots are "<jail-id>-rN"; the jailer refuses an existing
        // chroot dir, so resume the suffix above any survivor. Stale `pool-N`
        // chroots are DELETED instead: pooled processes die with the daemon's
        // cgroup, and a leftover chroot both makes the jailer refuse the reused
        // id and leaves a dead api.sock that fools an existence poll (observed
        // on the node as ConnectionRefused golden restores after a restart).
        let mut next_restore = 0u32;
        if let Ok(entries) = std::fs::read_dir(chroot_base.join("firecracker")) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("pool-") {
                    let _ = std::fs::remove_dir_all(e.path());
                    continue;
                }
                if let Some((_, n)) = name.rsplit_once("-r") {
                    if let Ok(n) = n.parse::<u32>() {
                        next_restore = next_restore.max(n + 1);
                    }
                }
            }
        }
        if !vms.is_empty() {
            tracing::info!(count = vms.len(), next_tap, "rehydrated persisted VM records");
        }
        (vms, next_tap, next_cid, next_restore)
    }

    /// Whether THIS VM should share one read-only base + guest overlay (Phase 3
    /// density). Gated per image: only images whose `sandbox-init` can pivot into
    /// a tmpfs+overlayfs root qualify (base, browser). node-python/custom keep a
    /// per-VM writable COW copy, so enabling `shared_rootfs` globally never gives
    /// them a read-only root they can't write to.
    fn shared_for(&self, spec: &VmSpec) -> bool {
        self.shared_image(&spec.image_key)
    }

    /// Same gate as [`Self::shared_for`] where only the image key is at hand
    /// (restore decides from the persisted record whether the staged rootfs is
    /// the shared base inode, which must never be chowned).
    fn shared_image(&self, image_key: &str) -> bool {
        self.shared_rootfs
            && crate::catalog::classify(image_key)
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
        let start = Instant::now();
        let mut delay = Duration::from_millis(1);
        let mut last: Option<std::io::Error> = None;
        while start.elapsed() < Duration::from_secs(4) {
            match UnixStream::connect(sock).await {
                Ok(s) => return Ok(s),
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_millis(20));
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

    /// GET against the Firecracker API socket, returning the parsed JSON body.
    /// Reads exactly Content-Length bytes — Firecracker can delay closing the
    /// connection (see `fc_api_to`), so reading to EOF would stall.
    async fn fc_api_get(&self, sock: &PathBuf, path: &str) -> Result<serde_json::Value> {
        let mut stream = Self::fc_connect(sock).await?;
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await?;
        stream.flush().await?;
        let mut resp = Vec::new();
        let mut buf = [0u8; 2048];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // Parse once the headers are in and the body is complete.
            if let Some(pos) = resp.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&resp[..pos]);
                let clen: usize = head
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                    .unwrap_or(0);
                let body_start = pos + 4;
                if resp.len() >= body_start + clen {
                    let status = head.lines().next().unwrap_or("");
                    if !status.contains(" 200") {
                        bail!("firecracker api GET {path} failed: {status}");
                    }
                    return Ok(serde_json::from_slice(&resp[body_start..body_start + clen])?);
                }
            }
            if Instant::now() >= deadline {
                bail!("firecracker api GET {path} timed out");
            }
            match tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await {
                Ok(Ok(0)) => bail!("firecracker api GET {path}: connection closed early"),
                Ok(Ok(n)) => resp.extend_from_slice(&buf[..n]),
                Ok(Err(e)) => return Err(anyhow::Error::from(e).context("read firecracker api response")),
                Err(_) => bail!("firecracker api GET {path} read timed out"),
            }
        }
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
        let mut delay = Duration::from_millis(2);
        loop {
            if self.agent_call(handle, &json!({"op": "ping"})).await.is_ok() {
                return Ok(start.elapsed().as_millis() as u64);
            }
            if start.elapsed() >= timeout {
                bail!("guest agent did not become ready within {timeout:?}");
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_millis(20));
        }
    }

    /// Boot a fresh microVM: jailer + firecracker, configure via API, start, and
    /// wait for the guest agent. Snapshot-based creates go through
    /// [`Self::boot_from_golden`], which handles the networking a restored
    /// sibling needs (fresh tap + `network_overrides` + guest re-IP) — the old
    /// `boot(snapshot)` form skipped tap setup entirely and left the VM with no
    /// usable network, which is why it was never wired up.
    async fn boot(&self, spec: &VmSpec) -> Result<(String, u64, u64)> {
        let handle = format!("vm_{}", spec.sandbox_id);
        let cid = self.next_cid.fetch_add(1, Ordering::SeqCst);
        let jail = self.chroot_base.join(&handle);
        std::fs::create_dir_all(&jail).context("create jail dir")?;
        self.workspaces.create(&handle)?;
        let rootfs = self.rootfs_path(spec);

        // Persistent volumes (Phase 5): resolve each attach to its backing image
        // up front so a missing volume fails before any resources are allocated.
        // Drives are configured pre-boot, so volumes only ride cold boots — the
        // service layer keeps volume sandboxes off the warm/snapshot paths.
        let vol_stages = spec
            .volumes
            .iter()
            .map(|v| {
                let file = format!("{}.ext4", v.volume_id);
                let host_path = self.volumes_dir.join(&file);
                if !host_path.exists() {
                    bail!("volume {} has no backing image at {host_path:?}", v.volume_id);
                }
                Ok(VolumeStage {
                    file,
                    host_path,
                    mount_path: v.mount_path.clone(),
                    label: ids::volume_label(&v.volume_id),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        // Per-VM tap on the host bridge, for NAT egress. Isolated bridge port:
        // the guest can reach the gateway/uplink (NAT egress) but NOT other
        // sandboxes' taps — cross-tenant L2 isolation.
        let tap_idx = self.next_tap.fetch_add(1, Ordering::SeqCst);
        let tap = format!("wdtap{tap_idx}");
        let guest_ip = guest_ip(tap_idx);
        let guest_mac = guest_mac(tap_idx);
        setup_tap(&tap).await?;

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
            poll_until(Duration::from_secs(4), || chroot_root.exists()).await;
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
            // Persistent volumes: HARDLINK the backing image into the chroot —
            // same inode, so guest writes persist under volumes_dir after this VM
            // is gone. A copy here would silently fork the data, so a failed link
            // (volumes_dir on a different filesystem) fails the boot instead.
            // chown is required (and safe — attachment is exclusive) so the
            // jailed uid can open it read-write.
            for v in &vol_stages {
                let dst = chroot_root.join(&v.file);
                let _ = std::fs::remove_file(&dst);
                std::fs::hard_link(&v.host_path, &dst).with_context(|| {
                    format!(
                        "hardlink volume {} into chroot (volumes_dir must share a filesystem with the jail)",
                        v.file
                    )
                })?;
                let _ = tokio::process::Command::new("chown").arg(&owner).arg(&dst).status().await;
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
                tap: Some(tap.clone()),
                tap_idx: Some(tap_idx),
                guest_ip: Some(guest_ip.clone()),
                resident_env: Default::default(),
                has_secrets: false,
                standby: false,
                snapshotted: false,
                volumes: vol_stages.clone(),
                ballooned_mib: 0,
            },
        );
        self.persist_record(&handle);

        // Everything after the jailer spawn is fallible (config errors, a 10 s
        // agent timeout). If any step fails we MUST kill the VM and reclaim its
        // RAM/jail dir, otherwise each failed boot leaks a live microVM
        // (review #10).
        let booted: Result<(u64, u64)> = async {
            // Give the API socket a moment to appear.
            poll_until(Duration::from_secs(4), || api_sock.exists()).await;

            let mem_mib = spec.resources.memory_mb;
            let vcpus = spec.resources.cpu.ceil().max(1.0) as u32;

            // Always wire the vsock device so the host can reach the guest agent.
            // uds_path is what Firecracker creates (chroot-relative under jailer).
            self.fc_api(&api_sock, "PUT", "/vsock", &json!({
                "guest_cid": cid,
                "uds_path": vsock_fc,
            })).await?;

            let boot_start = Instant::now();
            {
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
                // Balloon (config-gated): deflated at boot; the soft-standby
                // reaper inflates it on idle. deflate_on_oom lets the guest
                // reclaim pages under pressure rather than OOM-kill, and the 1s
                // stats interval feeds vm_metrics.
                if self.balloon {
                    self.fc_api(&api_sock, "PUT", "/balloon", &json!({
                        "amount_mib": 0,
                        "deflate_on_oom": true,
                        "stats_polling_interval_s": 1,
                    })).await?;
                }
                // Network params are passed on the kernel cmdline; the guest init
                // configures eth0 from them (no in-guest DHCP needed). With a
                // shared read-only base, mount it `ro` and signal the guest init
                // to layer a tmpfs+overlayfs so writes land in RAM (Phase 3).
                let (root_mode, overlay_arg) = if self.shared_for(spec) {
                    ("ro", " wd.overlay=tmpfs")
                } else {
                    ("rw", "")
                };
                // `quiet loglevel=1` skips per-line serial boot logging (a real
                // share of cold boot at virtualized-UART speed); the console
                // stays attached so panics still surface.
                let quiet = if self.quiet_boot { " quiet loglevel=1" } else { "" };
                let boot_args = format!(
                    "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda {root_mode}{quiet} \
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
                // Persistent volumes as extra virtio block drives. Order matters:
                // the rootfs is vda, so volume i appears at /dev/vd{b+i} — the
                // device-path fallback `apply_features` mounts by.
                for (i, v) in vol_stages.iter().enumerate() {
                    let path = if self.use_jailer {
                        v.file.clone()
                    } else {
                        v.host_path.to_string_lossy().into_owned()
                    };
                    self.fc_api(&api_sock, "PUT", &format!("/drives/vol{i}"), &json!({
                        "drive_id": format!("vol{i}"),
                        "path_on_host": path,
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

        // Mount persistent volumes first — inline files and startup commands may
        // target paths inside them. Prefer mount-by-label (stable across device
        // renumbering); fall back to the positional virtio path (/dev/vd{b+i},
        // rootfs is vda). A failed mount fails the boot: continuing silently
        // would let "persistent" writes land in the ephemeral root.
        for (i, v) in spec.volumes.iter().enumerate() {
            let label = ids::volume_label(&v.volume_id);
            let dev = format!("/dev/vd{}", (b'b' + i as u8) as char);
            let cmd = format!(
                "mkdir -p {mp} && (mount -L {label} {mp} 2>/dev/null || mount {dev} {mp})",
                mp = shell_quote(&v.mount_path),
            );
            let res = self
                .agent_call(handle, &json!({"op": "exec", "cmd": cmd, "background": false}))
                .await?;
            let exit = res.get("exit_code").and_then(|c| c.as_i64()).unwrap_or(-1);
            if exit != 0 {
                bail!(
                    "mount volume {} at {} failed (exit {exit}): {}",
                    v.volume_id,
                    v.mount_path,
                    res.get("stderr").and_then(|s| s.as_str()).unwrap_or("")
                );
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
            // The minimal Firecracker guest kernel ships without netfilter
            // (nf_tables), so dockerd's default bridge/NAT setup fails to
            // initialize and the daemon exits before opening its socket
            // (`iptables: Failed to initialize nft: Protocol not supported`,
            // observed on the node). `--iptables=false --bridge=none` skips the
            // network controller dockerd can't build here; the daemon comes up
            // in ~3s and `docker build` / `docker run` work (validated with
            // hello-world). Containers needing outbound networking require a
            // netfilter-capable guest kernel — a deliberate microVM trade-off.
            let _ = self
                .agent_call(handle, &json!({
                    "op": "exec",
                    "cmd": "nohup dockerd --host=unix:///var/run/docker.sock --iptables=false --bridge=none >/var/log/dockerd.log 2>&1 & \
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

    /// Spawn one pool entry: jailer + Firecracker with api.sock listening,
    /// configless until claimed. Pool uids live far above tap-derived uids so a
    /// pooled process never shares a uid with a tap-indexed VM.
    async fn spawn_pooled_jail(&self) -> Result<PooledJail> {
        let n = self.next_pool.fetch_add(1, Ordering::SeqCst);
        let jail_id = format!("pool-{n}");
        let uid = self.jailer_uid_base + 90_000 + (n % 9_000);
        let chroot_root = self.chroot_base.join("firecracker").join(&jail_id).join("root");
        let pool_dir = self.chroot_base.join("jailer-pool");
        std::fs::create_dir_all(&pool_dir).ok();
        let log = std::fs::File::create(pool_dir.join(format!("{jail_id}.log"))).context("pool log")?;
        let log2 = log.try_clone()?;
        let pid = tokio::process::Command::new(&self.jailer_bin)
            .args(["--id", &jail_id])
            .args(["--exec-file", &self.firecracker_bin])
            .args(["--uid", &uid.to_string(), "--gid", &uid.to_string()])
            .args(["--chroot-base-dir", self.chroot_base.to_str().unwrap()])
            .args(["--", "--api-sock", "api.sock"])
            .args(self.no_seccomp.then_some("--no-seccomp"))
            .stdout(log)
            .stderr(log2)
            .spawn()
            .context("spawn pooled jailer")?
            .id();
        let api = chroot_root.join("api.sock");
        poll_until(Duration::from_secs(4), || api.exists()).await;
        // Existence is not enough — a stale sock file from a dead process
        // passes that check; the pool must only hold processes that ACCEPT.
        if let Err(e) = Self::fc_connect(&api).await {
            if let Some(pid) = pid {
                let _ = tokio::process::Command::new("kill").arg("-9").arg(pid.to_string()).status().await;
            }
            let _ = std::fs::remove_dir_all(self.chroot_base.join("firecracker").join(&jail_id));
            return Err(e.context("pooled firecracker never accepted a connection"));
        }
        Ok(PooledJail { jail_id, chroot_root, uid, pid })
    }

    /// Claim a pre-spawned jailer, if any. The warmer's `maintain` refills.
    async fn claim_pooled_jail(&self) -> Option<PooledJail> {
        if !self.use_jailer || self.jailer_pool_size == 0 {
            return None;
        }
        self.jail_pool.lock().unwrap().pop()
    }

    /// Where the golden (per image+shape) snapshot artifacts live:
    /// `images_dir/snapshots/<image>/<memory_mb>/{snapshot.file, mem.file[, rootfs.ext4]}`.
    /// Keyed by memory size because a snapshot bakes in the guest RAM layout.
    fn golden_dir(&self, image_key: &str, resources: &crate::knobs::Resources) -> PathBuf {
        self.images_dir
            .join("snapshots")
            .join(image_key)
            .join(resources.memory_mb.to_string())
    }

    /// Produce the golden snapshot for `spec`'s image+shape: boot a throwaway
    /// VM, take a Full snapshot, publish the artifacts world-readable, tear the
    /// VM down. `snapshot.file` is published last, so a half-published dir is
    /// never considered available.
    async fn produce_golden_snapshot(&self, spec: &VmSpec) -> Result<()> {
        let dir = self.golden_dir(&spec.image_key, &spec.resources);
        let mut gspec = spec.clone();
        // The golden must capture only the image+shape: no tenant features.
        gspec.sandbox_id = format!("golden_{}", ids::sandbox_id());
        gspec.env.clear();
        gspec.secret_env.clear();
        gspec.volumes.clear();
        gspec.files.clear();
        gspec.mounts.clear();
        gspec.docker = false;
        gspec.coding_agent = None;
        let (handle, _boot_ms, _agent_ms) = self.boot(&gspec).await?;
        let publish: Result<()> = async {
            let (sock, jail) = {
                let vms = self.vms.lock().unwrap();
                let v = vms.get(&handle).ok_or_else(|| anyhow!("golden vm record missing"))?;
                (v.api_sock.clone(), v.api_sock.parent().unwrap().to_path_buf())
            };
            let (snap, mem) = self.write_snapshot(&sock, &jail, false).await?;
            std::fs::create_dir_all(&dir).context("create golden dir")?;
            // Shared-rootfs images boot read-only off the one base image (writes
            // live in the tmpfs overlay, captured in mem), so the base IS the
            // snapshot-time disk and restores hardlink it directly. Private-disk
            // images diverge during boot, so their disk must be published too.
            if !self.shared_for(spec) {
                publish_golden_file(&jail.join("rootfs.ext4"), &dir.join("rootfs.ext4")).await?;
            }
            publish_golden_file(&mem, &dir.join("mem.file")).await?;
            publish_golden_file(&snap, &dir.join("snapshot.file")).await?;
            Ok(())
        }
        .await;
        // The throwaway VM is torn down regardless of publish success.
        let _ = self.delete(&handle).await;
        publish
    }

    /// Bring up a NEW VM from the golden snapshot — the create path's
    /// `snapshot_restore`. Mirrors `fork`'s child launch: fresh jailer chroot,
    /// own tap/IP, `network_overrides` repointing eth0 at the new tap, then
    /// re-IP the guest. Jailer-only (see `golden_snapshot_available`).
    async fn boot_from_golden(&self, spec: &VmSpec) -> Result<(String, u64)> {
        let dir = self.golden_dir(&spec.image_key, &spec.resources);
        let handle = format!("vm_{}", spec.sandbox_id);
        let jail = self.chroot_base.join(&handle);
        std::fs::create_dir_all(&jail).context("create jail dir")?;
        self.workspaces.create(&handle)?;
        let start = Instant::now();

        let tap_idx = self.next_tap.fetch_add(1, Ordering::SeqCst);
        let tap = format!("wdtap{tap_idx}");
        let ip = guest_ip(tap_idx);
        let cid = self.next_cid.fetch_add(1, Ordering::SeqCst);
        setup_tap(&tap).await?;

        // A pooled jailer (api.sock already listening) makes the golden restore
        // pure staging + snapshot/load; otherwise spawn one for this VM.
        let (uid, chroot_root, pid) = match self.claim_pooled_jail().await {
            Some(p) => (p.uid, p.chroot_root, p.pid),
            None => {
                let jail_id = handle.replace('_', "-");
                let uid = self.jailer_uid_base + tap_idx;
                let chroot_root = self.chroot_base.join("firecracker").join(&jail_id).join("root");
                let log = std::fs::File::create(jail.join("firecracker.log")).context("fc log")?;
                let log2 = log.try_clone()?;
                let pid = tokio::process::Command::new(&self.jailer_bin)
                    .args(["--id", &jail_id])
                    .args(["--exec-file", &self.firecracker_bin])
                    .args(["--uid", &uid.to_string(), "--gid", &uid.to_string()])
                    .args(["--chroot-base-dir", self.chroot_base.to_str().unwrap()])
                    .args(["--", "--api-sock", "api.sock"])
                    .args(self.no_seccomp.then_some("--no-seccomp"))
                    .stdout(log)
                    .stderr(log2)
                    .spawn()
                    .context("spawn jailer for golden restore")?
                    .id();
                (uid, chroot_root, pid)
            }
        };

        // Record first so a failure below can reclaim the tap + chroot.
        self.vms.lock().unwrap().insert(
            handle.clone(),
            VmRecord {
                api_sock: chroot_root.join("api.sock"),
                vsock_uds: chroot_root.join("vsock.sock"),
                vsock_fc: "vsock.sock".into(),
                cid,
                pid,
                image_key: spec.image_key.clone(),
                tap: Some(tap.clone()),
                tap_idx: Some(tap_idx),
                guest_ip: Some(ip.clone()),
                resident_env: Default::default(),
                has_secrets: false,
                standby: false,
                // The staged golden artifacts are SHARED inodes; the first
                // standby must take a Full snapshot into this VM's own files
                // (write_snapshot truncates its target in place).
                snapshotted: false,
                volumes: Vec::new(),
                ballooned_mib: 0,
            },
        );
        self.persist_record(&handle);

        let booted: Result<u64> = async {
            poll_until(Duration::from_secs(4), || chroot_root.exists()).await;
            // Stage the golden artifacts under names the VM's own snapshots will
            // never write to ("golden-*"): hardlinks share the host page cache
            // across every sibling restored from this artifact, and must never
            // be truncated by a later standby.
            let stage = |name: &str, as_name: &str| -> Result<()> {
                let dst = chroot_root.join(as_name);
                let _ = std::fs::remove_file(&dst);
                std::fs::hard_link(dir.join(name), &dst)
                    .or_else(|_| std::fs::copy(dir.join(name), &dst).map(|_| ()))
                    .with_context(|| format!("stage golden {name}"))
            };
            stage("snapshot.file", "golden-snapshot.file")?;
            stage("mem.file", "golden-mem.file")?;
            // The snapshot's root drive reopens "rootfs.ext4" inside this chroot.
            if self.shared_for(spec) {
                // Shared images: hardlink the one read-only base (page cache is
                // shared; guest writes live in its tmpfs overlay).
                let dst = chroot_root.join("rootfs.ext4");
                let _ = std::fs::remove_file(&dst);
                std::fs::hard_link(self.rootfs_path(spec), &dst)
                    .context("hardlink shared base for golden restore")?;
            } else {
                // Private-disk images: an own (reflink) copy of the golden's
                // snapshot-time disk, so this VM diverges independently.
                let st = tokio::process::Command::new("cp")
                    .args(["--reflink=auto"])
                    .arg(dir.join("rootfs.ext4"))
                    .arg(chroot_root.join("rootfs.ext4"))
                    .status()
                    .await
                    .context("copy golden rootfs")?;
                if !st.success() {
                    bail!("copy golden rootfs failed");
                }
                let _ = tokio::process::Command::new("chown")
                    .arg(format!("{uid}:{uid}"))
                    .arg(chroot_root.join("rootfs.ext4"))
                    .status()
                    .await;
            }

            let api_sock = chroot_root.join("api.sock");
            poll_until(Duration::from_secs(4), || api_sock.exists()).await;
            if self.prewarm_mem_cache {
                let m = chroot_root.join("golden-mem.file");
                tokio::spawn(async move { prewarm_page_cache(&m).await });
            }
            // Load FIRST (the snapshot restores its own vsock at "vsock.sock");
            // network_overrides points eth0 at THIS VM's tap — the golden VM's
            // tap is long gone, and siblings each need their own device.
            self.fc_api_to(&api_sock, "PUT", "/snapshot/load", &json!({
                "snapshot_path": "golden-snapshot.file",
                "mem_backend": { "backend_path": "golden-mem.file", "backend_type": "File" },
                "enable_diff_snapshots": true,
                "network_overrides": [{ "iface_id": "eth0", "host_dev_name": tap.clone() }],
                "resume_vm": true,
            }), 300).await?;
            self.await_agent(&handle, Duration::from_secs(10)).await?;
            // The snapshot carries the golden VM's IP; re-IP and re-add the
            // default route (the flush drops it) so siblings don't collide.
            let _ = self.agent_call(&handle, &json!({
                "op": "exec",
                "cmd": format!(
                    "ip addr flush dev eth0 2>/dev/null; ip addr add {ip}/16 dev eth0 2>/dev/null; \
                     ip link set eth0 up 2>/dev/null; \
                     ip route replace default via {NET_GATEWAY} dev eth0 2>/dev/null; true"
                ),
                "background": false,
            })).await;
            Ok(start.elapsed().as_millis() as u64)
        }
        .await;

        match booted {
            Ok(ms) => Ok((handle, ms)),
            Err(e) => {
                let fc_log = std::fs::read_to_string(jail.join("firecracker.log")).unwrap_or_default();
                tracing::error!(
                    handle = %handle,
                    error = %e,
                    firecracker_log = %fc_log.lines().rev().take(8).collect::<Vec<_>>().join(" | "),
                    "golden restore failed"
                );
                self.kill_and_reclaim(&handle, pid, &jail).await;
                Err(e)
            }
        }
    }

    /// Reclaim per-VM jail/chroot directories not owned by a live VM and older
    /// than `min_age_secs` (so a VM mid-boot — its dir created just before its
    /// record is inserted — is never reclaimed). The age is a parameter so tests
    /// can exercise the liveness logic without faking mtimes. Liveness comes
    /// from the in-memory map, which [`Self::rehydrate`] populates at startup —
    /// that eager load is what keeps this sweeper away from parked standby VMs
    /// after a control-plane restart.
    fn gc_stale_jails_min_age(&self, min_age_secs: u64) -> usize {
        // Build the set of directories belonging to live VMs: the per-VM jail dir
        // (chroot_base/<handle>) and the active chroot (the firecracker/<id> dir
        // derived from the api sock — possibly a `-rN` restore chroot).
        let (live_jails, mut live_chroots): (HashSet<PathBuf>, HashSet<PathBuf>) = {
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
        // Idle pooled jailers are live too — sweeping a pool chroot would yank
        // the socket out from under a process waiting to be claimed.
        for pj in self.jail_pool.lock().unwrap().iter() {
            live_chroots.insert(self.chroot_base.join("firecracker").join(&pj.jail_id));
        }
        let stale = |p: &Path| -> bool {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .map(|d| d.as_secs() >= min_age_secs)
                .unwrap_or(false)
        };
        let mut removed = 0;
        // Per-VM jail dirs directly under chroot_base (skip the `firecracker`
        // container dir and the snapshots cache).
        if let Ok(entries) = std::fs::read_dir(&self.chroot_base) {
            for e in entries.flatten() {
                let p = e.path();
                let name = e.file_name();
                if name == "firecracker" || name == "snapshots" || name == "jailer-pool" {
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
        self.gc_stale_jails_min_age(120)
    }

    async fn maintain(&self) {
        // Keep the pre-spawned jailer pool at target (claimed entries are not
        // replaced inline — restores shouldn't pay the refill).
        if !self.use_jailer || self.jailer_pool_size == 0 {
            return;
        }
        loop {
            let len = self.jail_pool.lock().unwrap().len();
            if len >= self.jailer_pool_size as usize {
                break;
            }
            match self.spawn_pooled_jail().await {
                Ok(p) => self.jail_pool.lock().unwrap().push(p),
                Err(e) => {
                    tracing::warn!(error = %e, "jailer pool refill failed");
                    break;
                }
            }
        }
    }

    async fn balloon(&self, handle: &str, amount_mib: u32) -> Result<()> {
        if !self.balloon {
            bail!("balloon device disabled (runtime.balloon = false)");
        }
        let sock = {
            let vms = self.vms.lock().unwrap();
            let v = vms.get(handle).ok_or_else(|| anyhow!("unknown vm {handle}"))?;
            if v.standby {
                bail!("cannot balloon a standby vm");
            }
            v.api_sock.clone()
        };
        self.fc_api(&sock, "PATCH", "/balloon", &json!({ "amount_mib": amount_mib })).await?;
        if let Some(v) = self.vms.lock().unwrap().get_mut(handle) {
            v.ballooned_mib = amount_mib;
        }
        Ok(())
    }

    async fn vm_metrics(&self, handle: &str) -> Option<super::VmMetrics> {
        let (sock, pid, ballooned, standby) = {
            let vms = self.vms.lock().unwrap();
            let v = vms.get(handle)?;
            (v.api_sock.clone(), v.pid, v.ballooned_mib, v.standby)
        };
        // Host truth: the VM process's resident set (Linux procfs).
        let host_rss_bytes = pid.and_then(|pid| {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
            status.lines().find_map(|l| {
                l.strip_prefix("VmRSS:")
                    .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
                    .map(|kb| kb * 1024)
            })
        });
        let balloon_stats = if self.balloon && !standby {
            self.fc_api_get(&sock, "/balloon/statistics").await.ok()
        } else {
            None
        };
        Some(super::VmMetrics {
            host_rss_bytes,
            balloon_target_mib: (ballooned > 0).then_some(ballooned),
            balloon_stats,
            net: self.vm_net_stats(handle),
        })
    }

    fn golden_snapshot_available(&self, image_key: &str, resources: &crate::knobs::Resources) -> bool {
        // Jailer-only: a snapshot restores its vsock at the uds path captured at
        // snapshot time. Under the jailer that path is chroot-relative, so every
        // sibling gets its own socket in its own chroot; a direct launch bakes
        // in an absolute path that would collide across siblings.
        self.use_jailer && self.golden_dir(image_key, resources).join("snapshot.file").exists()
    }

    async fn ensure_golden_snapshot(&self, spec: &VmSpec) -> Result<bool> {
        if !self.use_jailer {
            return Ok(false);
        }
        let dir = self.golden_dir(&spec.image_key, &spec.resources);
        if dir.join("snapshot.file").exists() {
            return Ok(false);
        }
        // One golden production at a time (a throwaway VM boot + full-RAM
        // snapshot); re-check under the lock so racers don't double-produce.
        let _serialize = self.golden_lock.lock().await;
        if dir.join("snapshot.file").exists() {
            return Ok(false);
        }
        self.produce_golden_snapshot(spec).await?;
        Ok(true)
    }

    async fn prewarm(&self, spec: &VmSpec) -> Result<WarmVm> {
        // A warm VM is a fully booted, idle microVM kept ready; create()
        // attaches and applies features. Prefer restoring it from the golden
        // artifact: every sibling restored from the same mem.file shares its
        // clean guest pages through the host page cache (N warm VMs cost ~one
        // memory image plus dirty deltas — Phase 3 density), and it is ready in
        // restore time rather than cold-boot time.
        if self.golden_snapshot_available(&spec.image_key, &spec.resources) {
            let (handle, _ms) = self.boot_from_golden(spec).await?;
            return Ok(WarmVm { handle, image_key: spec.image_key.clone(), resources: spec.resources });
        }
        let (handle, _boot_ms, _agent_ms) = self.boot(spec).await?;
        Ok(WarmVm { handle, image_key: spec.image_key.clone(), resources: spec.resources })
    }

    async fn create(&self, spec: &VmSpec, warm: Option<WarmVm>, snapshot_available: bool) -> Result<VmInstance> {
        let (handle, boot_path, boot_ms) = if let Some(w) = warm {
            // Hot pool: the warm VM already booted. Attach and apply features.
            (w.handle, BootPath::HotPool, 0)
        } else if snapshot_available
            && spec.volumes.is_empty()
            && self.golden_snapshot_available(&spec.image_key, &spec.resources)
        {
            // Golden restore: a brand-new VM from the image+shape snapshot —
            // hundreds of ms instead of a ~1.4s cold boot. (Volume sandboxes
            // stay on the cold path: drives are configured pre-boot.)
            let (handle, boot_ms) = self.boot_from_golden(spec).await?;
            (handle, BootPath::SnapshotRestore, boot_ms)
        } else {
            let (handle, boot_ms, _agent_ms) = self.boot(spec).await?;
            (handle, BootPath::ColdBoot, boot_ms)
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

    async fn open_pty(&self, handle: &str) -> Result<PtySession> {
        // A REAL in-guest TTY over vsock: open a dedicated stream, ask the
        // agent for a pty (it openpty()s and spawns the shell on the slave),
        // then hand the raw stream to the websocket bridge. The guest agent is
        // one process per connection (socat fork), so dropping the streams
        // closes the connection, the agent exits, and the shell gets SIGHUP —
        // no host-side child to reap.
        let uds = {
            let vms = self.vms.lock().unwrap();
            vms.get(handle).map(|v| v.vsock_uds.clone()).ok_or_else(|| anyhow!("unknown vm {handle}"))?
        };
        let mut stream = UnixStream::connect(&uds)
            .await
            .with_context(|| format!("connect guest vsock uds {uds:?}"))?;
        stream.write_all(format!("CONNECT {GUEST_AGENT_VSOCK_PORT}\n").as_bytes()).await?;
        let mut ack = [0u8; 64];
        let n = stream.read(&mut ack).await?;
        let ack_str = String::from_utf8_lossy(&ack[..n]);
        if !ack_str.starts_with("OK") {
            bail!("vsock connect rejected: {ack_str}");
        }
        let req = json!({"op": "pty", "cols": 120, "rows": 32});
        stream.write_all(format!("{}\n", serde_json::to_string(&req)?).as_bytes()).await?;
        // One JSON response line; after it the connection IS the TTY stream.
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = stream.read(&mut byte).await?;
            if n == 0 || byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
        let resp: serde_json::Value = serde_json::from_slice(&line).context("parse pty response")?;
        if resp.get("status").and_then(|s| s.as_str()) == Some("error") {
            bail!(
                "guest agent error: {}",
                resp.get("message").and_then(|m| m.as_str()).unwrap_or("?")
            );
        }
        let (read_half, write_half) = stream.into_split();
        Ok(PtySession {
            input: Box::new(write_half),
            output: Box::new(read_half),
            stderr: None, // a real TTY merges stderr into the terminal stream
            child: None,
        })
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
        let (api_sock, jail, vsock_uds, tap, tap_idx, volumes, image_key) = {
            let vms = self.vms.lock().unwrap();
            let v = vms.get(handle).ok_or_else(|| anyhow!("unknown vm {handle} (no persisted record)"))?;
            (
                v.api_sock.clone(),
                v.api_sock.parent().unwrap().to_path_buf(),
                v.vsock_uds.clone(),
                v.tap.clone(),
                v.tap_idx,
                v.volumes.clone(),
                v.image_key.clone(),
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
            // A pre-spawned pooled jailer (api.sock already listening) skips the
            // ~30 ms relaunch — the measured floor of the resume path once
            // demand paging landed; fall back to spawning one on demand.
            let (uid, new_chroot, pid) = match self.claim_pooled_jail().await {
                Some(p) => (p.uid, p.chroot_root, p.pid),
                None => {
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
                    poll_until(Duration::from_secs(4), || new_chroot.exists()).await;
                    (uid, new_chroot, pid)
                }
            };
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
            // Persistent volumes: the snapshot's drives reference their backing
            // files by chroot-relative name; hardlink each from its real image
            // (same inode — writes keep landing on the persistent store).
            for v in &volumes {
                let dst = new_chroot.join(&v.file);
                let _ = std::fs::remove_file(&dst);
                std::fs::hard_link(&v.host_path, &dst)
                    .with_context(|| format!("re-stage volume {} into restore chroot", v.file))?;
            }
            // A pooled jailer runs as its own uid, which differs from the uid
            // that wrote these artifacts — chown the staged inodes so the
            // dropped Firecracker can read them (and write mem.file on the next
            // Diff snapshot). The shared read-only base keeps its ownership: a
            // chown there would mutate the one inode every sandbox maps.
            let owner = format!("{uid}:{uid}");
            for name in ["snapshot.file", "mem.file"] {
                let _ = tokio::process::Command::new("chown").arg(&owner).arg(new_chroot.join(name)).status().await;
            }
            if !self.shared_image(&image_key) {
                let _ = tokio::process::Command::new("chown").arg(&owner).arg(new_chroot.join("rootfs.ext4")).status().await;
            }
            for v in &volumes {
                let _ = tokio::process::Command::new("chown").arg(&owner).arg(new_chroot.join(&v.file)).status().await;
            }
            let new_api = new_chroot.join("api.sock");
            poll_until(Duration::from_secs(4), || new_api.exists()).await;
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
        poll_until(Duration::from_secs(4), || api_sock.exists()).await;

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
            poll_until(Duration::from_secs(4), || new_chroot.exists()).await;
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
            poll_until(Duration::from_secs(4), || new_api.exists()).await;
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
            poll_until(Duration::from_secs(4), || child_sock.exists()).await;
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
                // Fork children never inherit volumes (exclusive attach; the
                // service refuses to fork a volume-attached parent).
                volumes: Vec::new(),
                ballooned_mib: 0,
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

    async fn create_volume(&self, volume_id: &str, size_gb: u32) -> Result<()> {
        // A sparse, labelled ext4 image: disk is consumed as the guest writes,
        // and the label lets the guest mount it independent of /dev/vdX order.
        std::fs::create_dir_all(&self.volumes_dir).context("create volumes dir")?;
        let path = self.volumes_dir.join(format!("{volume_id}.ext4"));
        let f = std::fs::File::create(&path).context("create volume image")?;
        f.set_len(size_gb as u64 * 1024 * 1024 * 1024).context("size volume image")?;
        drop(f);
        let label = ids::volume_label(volume_id);
        let out = tokio::process::Command::new("mkfs.ext4")
            .args(["-F", "-q", "-L", &label])
            .arg(&path)
            .output()
            .await
            .context("run mkfs.ext4 (is e2fsprogs installed?)")?;
        if !out.status.success() {
            let _ = std::fs::remove_file(&path);
            bail!("mkfs.ext4 failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(())
    }

    async fn delete_volume(&self, volume_id: &str) -> Result<()> {
        let path = self.volumes_dir.join(format!("{volume_id}.ext4"));
        if path.exists() {
            std::fs::remove_file(&path).context("remove volume image")?;
        }
        Ok(())
    }
}

/// Publish one golden artifact: reflink-copy (instant on a reflink fs, real
/// I/O on ext4 — one-time per image+shape) and make it world-readable, since
/// restored VMs run as per-VM jailed uids and only ever read these.
async fn publish_golden_file(from: &Path, to: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(to);
    let st = tokio::process::Command::new("cp")
        .args(["--reflink=auto"])
        .arg(from)
        .arg(to)
        .status()
        .await
        .context("cp golden artifact")?;
    if !st.success() {
        bail!("publish golden artifact failed: {from:?} -> {to:?}");
    }
    let _ = std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o644));
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;

    fn cfg_at(dir: &Path) -> RuntimeConfig {
        let mut cfg = RuntimeConfig::default();
        cfg.workspace_dir = dir.to_path_buf();
        cfg.images_dir = dir.join("images");
        cfg.volumes_dir = dir.join("volumes");
        cfg
    }

    /// A record as `standby` persists it: process killed (pid None), snapshot
    /// artifacts in the (restore) chroot the api_sock points into.
    fn standby_record(chroot_base: &Path, jail_id: &str, tap_idx: u32, cid: u32) -> VmRecord {
        let chroot_root = chroot_base.join("firecracker").join(jail_id).join("root");
        VmRecord {
            api_sock: chroot_root.join("api.sock"),
            vsock_uds: chroot_root.join("vsock.sock"),
            vsock_fc: "vsock.sock".into(),
            cid,
            pid: None,
            image_key: "base".into(),
            tap: Some(format!("wdtap{tap_idx}")),
            tap_idx: Some(tap_idx),
            guest_ip: Some(guest_ip(tap_idx)),
            resident_env: Default::default(),
            has_secrets: false,
            standby: true,
            snapshotted: true,
            volumes: Vec::new(),
            ballooned_mib: 0,
        }
    }

    fn write_record(chroot_base: &Path, handle: &str, rec: &VmRecord) {
        let dir = chroot_base.join(handle);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("record.json"), serde_json::to_string(rec).unwrap()).unwrap();
    }

    #[test]
    fn restart_rehydrates_standby_records_and_counters() {
        let tmp = tempfile::tempdir().unwrap();
        let chroot_base = tmp.path().join("jail");
        // A parked standby VM from the previous daemon run: tap_idx 7, cid 11,
        // its artifacts in restore chroot "...-r2".
        let rec = standby_record(&chroot_base, "vm-sb-x-r2", 7, 11);
        std::fs::create_dir_all(chroot_base.join("firecracker").join("vm-sb-x-r2").join("root")).unwrap();
        write_record(&chroot_base, "vm_sb_x", &rec);

        let rt = FirecrackerRuntime::new(&cfg_at(tmp.path()));
        // The record is resident immediately (not lazily on first restore).
        assert!(rt.vms.lock().unwrap().contains_key("vm_sb_x"), "standby record must rehydrate at startup");
        // Counters resume ABOVE what survivors still hold: a fresh boot must not
        // reuse wdtap7/its guest IP, the CID, or restore chroot suffix -r2.
        assert_eq!(rt.next_tap.load(Ordering::SeqCst), 8, "next_tap must clear the survivor's tap_idx");
        assert!(rt.next_cid.load(Ordering::SeqCst) >= 12, "next_cid must clear the survivor's cid");
        assert_eq!(rt.next_restore.load(Ordering::SeqCst), 3, "next_restore must clear existing -rN chroots");
    }

    #[test]
    fn restart_gc_spares_standby_artifacts_and_sweeps_orphans() {
        let tmp = tempfile::tempdir().unwrap();
        let chroot_base = tmp.path().join("jail");
        let rec = standby_record(&chroot_base, "vm-sb-live-r0", 3, 5);
        std::fs::create_dir_all(chroot_base.join("firecracker").join("vm-sb-live-r0").join("root")).unwrap();
        write_record(&chroot_base, "vm_sb_live", &rec);
        // Orphans from dead VMs: a jail dir without a record and a chroot no
        // record points at.
        std::fs::create_dir_all(chroot_base.join("vm_sb_dead")).unwrap();
        std::fs::create_dir_all(chroot_base.join("firecracker").join("vm-sb-dead")).unwrap();

        let rt = FirecrackerRuntime::new(&cfg_at(tmp.path()));
        // min_age 0 = everything is old enough; only liveness should protect.
        let removed = rt.gc_stale_jails_min_age(0);
        assert_eq!(removed, 2, "exactly the two orphans are swept");
        assert!(
            chroot_base.join("vm_sb_live").join("record.json").exists(),
            "standby record must survive the sweep after a restart"
        );
        assert!(
            chroot_base.join("firecracker").join("vm-sb-live-r0").exists(),
            "standby snapshot chroot must survive the sweep after a restart"
        );
        assert!(!chroot_base.join("vm_sb_dead").exists());
        assert!(!chroot_base.join("firecracker").join("vm-sb-dead").exists());
    }

    #[test]
    fn rehydrate_drops_dead_running_records() {
        let tmp = tempfile::tempdir().unwrap();
        let chroot_base = tmp.path().join("jail");
        // A VM that was RUNNING when the previous daemon died: not standby, and
        // its pid no longer names a live firecracker process (recycled/rebooted).
        let mut rec = standby_record(&chroot_base, "vm-sb-gone-r0", 1, 4);
        rec.standby = false;
        rec.pid = Some(3_999_999); // far above any live pid on test hosts
        write_record(&chroot_base, "vm_sb_gone", &rec);

        let rt = FirecrackerRuntime::new(&cfg_at(tmp.path()));
        assert!(
            !rt.vms.lock().unwrap().contains_key("vm_sb_gone"),
            "an unrestorable record must not rehydrate"
        );
        assert!(
            !chroot_base.join("vm_sb_gone").join("record.json").exists(),
            "the tombstone record is removed so the GC can reclaim the dir"
        );
        // Its dir is now orphaned and sweepable.
        assert_eq!(rt.gc_stale_jails_min_age(0), 1);
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;
    use crate::config::RuntimeConfig;

    #[test]
    fn restart_removes_stale_pool_chroots() {
        // Pooled processes die with the daemon's cgroup; their chroots must not
        // survive into the next run (the jailer refuses an existing chroot, and
        // the leftover api.sock fools an existence poll — observed on the node).
        let tmp = tempfile::tempdir().unwrap();
        let chroot_base = tmp.path().join("jail");
        std::fs::create_dir_all(chroot_base.join("firecracker").join("pool-0").join("root")).unwrap();
        std::fs::create_dir_all(chroot_base.join("firecracker").join("pool-7").join("root")).unwrap();
        // A restore chroot must be left alone (and bump the counter).
        std::fs::create_dir_all(chroot_base.join("firecracker").join("vm-sbx-x-r1")).unwrap();

        let mut cfg = RuntimeConfig::default();
        cfg.workspace_dir = tmp.path().to_path_buf();
        let rt = FirecrackerRuntime::new(&cfg);

        assert!(!chroot_base.join("firecracker").join("pool-0").exists());
        assert!(!chroot_base.join("firecracker").join("pool-7").exists());
        assert!(chroot_base.join("firecracker").join("vm-sbx-x-r1").exists());
        assert_eq!(rt.next_restore.load(Ordering::SeqCst), 2);
    }
}
