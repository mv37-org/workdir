//! The `Runtime` trait abstracts the actual microVM lifecycle so the control
//! plane is identical whether it drives real Firecracker microVMs on a
//! KVM-capable Hetzner node ([`firecracker::FirecrackerRuntime`]) or the
//! cross-platform development runtime ([`mock::MockRuntime`]).
//!
//! The boot path a `create` takes (hot pool / snapshot restore / cold boot) is
//! reported honestly so benchmarks never hide cold starts behind hot-pool
//! numbers (spec §3.5, §13.2, §21).

pub mod firecracker;
pub mod mock;
pub mod workspace;

use crate::knobs::Resources;
use crate::model::{BootPath, BrowserConfig, CodingAgentConfig, MountSpec};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};

/// Everything the runtime needs to boot one microVM.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VmSpec {
    pub sandbox_id: String,
    pub org_id: String,
    /// Curated image family key (`base`, `browser`, …) or `custom`.
    pub image_key: String,
    /// Full image reference (curated name or `custom/<org>/<name>:<version>`).
    pub image_ref: String,
    pub resources: Resources,
    pub env: BTreeMap<String, String>,
    /// Secret values injected late; kept separate from `env` so they are never
    /// persisted to the guest env file or captured in snapshots (review M3).
    pub secret_env: BTreeMap<String, String>,
    pub browser: Option<BrowserConfig>,
    /// Run dockerd inside the guest.
    pub docker: bool,
    /// Install a coding-agent CLI into the guest after boot (opt-in).
    pub coding_agent: Option<CodingAgentConfig>,
    /// Bucket mounts to set up after boot.
    pub mounts: Vec<MountSpec>,
    /// Persistent volumes to attach as block devices and mount in the guest.
    #[serde(default)]
    pub volumes: Vec<crate::model::VolumeAttach>,
    /// Inline files to write into the workspace before startup commands run.
    pub files: Vec<(String, Vec<u8>)>,
}

/// A pre-booted warm microVM waiting in a hot pool.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WarmVm {
    pub handle: String,
    pub image_key: String,
    pub resources: Resources,
}

/// A live microVM instance, with the timings the boot incurred.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VmInstance {
    pub handle: String,
    pub boot_path: BootPath,
    pub boot_ms: u64,
    pub image_cache_ms: u64,
    pub browser_ready_ms: u64,
    /// Time spent installing the opt-in coding agent (0 if not requested).
    pub agent_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExecRequest {
    pub cmd: String,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub background: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub dir: bool,
}

/// Network byte counters for a VM, used by abuse monitoring to flag miners /
/// scanners (high sustained egress).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct NetStats {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SnapshotArtifact {
    pub handle: String,
    pub storage_bytes: u64,
}

/// An interactive shell session (the `/pty` endpoint). The development runtime
/// backs this with a piped child shell; the Firecracker runtime backs it with
/// the in-guest agent over vsock. Not a true TTY in the dev runtime — documented
/// as such.
pub struct PtySession {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

#[async_trait]
pub trait Runtime: Send + Sync {
    fn kind(&self) -> &'static str;

    /// Whether this node can boot the given image right now (its rootfs is
    /// present). The warmer uses this to skip pools for images not yet built.
    fn image_available(&self, _image_key: &str) -> bool {
        true
    }

    /// Per-VM network byte counters (for abuse monitoring). None if unsupported.
    fn vm_net_stats(&self, _handle: &str) -> Option<NetStats> {
        None
    }

    /// Reclaim per-VM jail/chroot directories left behind by VMs that are no
    /// longer live (under the jailer, teardown can leak these). Returns how many
    /// directories were removed. Default: nothing to do.
    fn gc_stale_jails(&self) -> usize {
        0
    }

    /// Pre-boot a warm microVM for a hot pool. Returns its handle.
    async fn prewarm(&self, spec: &VmSpec) -> Result<WarmVm>;

    /// Allocate a microVM for a sandbox. If `warm` is provided, claim it (the
    /// `hot_pool` path); otherwise restore a snapshot if one exists, else cold
    /// boot. `snapshot_available` lets the runtime prefer restore over cold boot.
    async fn create(
        &self,
        spec: &VmSpec,
        warm: Option<WarmVm>,
        snapshot_available: bool,
    ) -> Result<VmInstance>;

    async fn exec(&self, handle: &str, req: &ExecRequest) -> Result<ExecResult>;

    async fn open_pty(&self, handle: &str) -> Result<PtySession>;

    async fn write_file(&self, handle: &str, path: &str, bytes: &[u8]) -> Result<()>;
    async fn read_file(&self, handle: &str, path: &str) -> Result<Vec<u8>>;
    async fn list_dir(&self, handle: &str, path: &str) -> Result<Vec<DirEntry>>;

    /// Make `port` reachable; returns the local upstream address the preview
    /// proxy should forward to.
    async fn expose_port(&self, handle: &str, port: u16) -> Result<SocketAddr>;

    /// Poll an in-sandbox HTTP URL until ready or timeout.
    async fn ready_check(&self, handle: &str, url: &str, timeout_seconds: u32) -> Result<()>;

    /// Stop the VM, releasing CPU/memory. Disk is preserved if `persist`.
    async fn pause(&self, handle: &str, persist: bool) -> Result<()>;

    /// Resume from a stopped disk/snapshot. Returns resume timing in ms.
    async fn resume(&self, handle: &str) -> Result<u64>;

    /// Park the VM in perpetual standby (roadmap Phase 1): snapshot it to disk,
    /// then kill the VM process so its guest RAM is returned to the host. The
    /// runtime retains just enough metadata (snapshot artifact, NIC) to bring it
    /// back with [`Runtime::restore`]. Returns the snapshot+evict time in ms.
    async fn standby(&self, handle: &str) -> Result<u64>;

    /// Resume a standby VM from its on-disk snapshot, re-establishing the guest's
    /// host networking (tap) that the eviction tore down. Returns the restore
    /// latency in ms — the number Phase 2 drives toward < 25ms.
    async fn restore(&self, handle: &str) -> Result<u64>;

    async fn snapshot(&self, handle: &str) -> Result<SnapshotArtifact>;

    /// Clone a running VM into an instant sibling (roadmap Phase 3): snapshot the
    /// parent's memory+disk and bring up a new, independent VM from that artifact
    /// under `child_spec` (its own id, disk copy, and host NIC). Returns the new
    /// VM with the [`BootPath::Fork`] label and its fork latency. "Nearly free
    /// once snapshots are solid" — fork latency should track resume latency.
    async fn fork(&self, parent_handle: &str, child_spec: &VmSpec) -> Result<VmInstance>;

    /// Tear down the VM and delete its ephemeral disk.
    async fn delete(&self, handle: &str) -> Result<()>;
}
