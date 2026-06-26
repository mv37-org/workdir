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
use crate::model::{BootPath, BrowserConfig, CodingAgentConfig, MountSpec, NetworkPolicy};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Child;

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
    /// Host-enforced network policy for the sandbox lifetime.
    #[serde(default)]
    pub network: NetworkPolicy,
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

/// Per-VM working-set metrics: what a sandbox actually costs the node right
/// now, vs the shape it reserves. Feeds `GET /v1/sandboxes/:id/metrics` and
/// gives measured-memory overcommit its ground truth.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct VmMetrics {
    /// Host-resident bytes of the VM process (VmRSS): the real footprint.
    pub host_rss_bytes: Option<u64>,
    /// Current balloon target (MiB reclaimed from the guest), if ballooned.
    pub balloon_target_mib: Option<u32>,
    /// Latest guest balloon statistics (free/available memory), verbatim from
    /// the device. None when the balloon is absent or stats are unavailable.
    pub balloon_stats: Option<serde_json::Value>,
    pub net: Option<NetStats>,
}

/// An interactive shell session (the `/pty` endpoint), as a pair of byte
/// streams. The Firecracker runtime bridges a REAL in-guest TTY over vsock
/// (stderr is merged into the terminal stream, as a TTY does); the development
/// runtime backs it with a piped child shell, whose stderr arrives separately
/// and whose process must be reaped when the session ends.
pub struct PtySession {
    /// Client keystrokes flow here.
    pub input: Box<dyn AsyncWrite + Send + Unpin>,
    /// Terminal output (the TTY stream; piped stdout in the dev runtime).
    pub output: Box<dyn AsyncRead + Send + Unpin>,
    /// Piped stderr (dev runtime only; a real TTY merges it into `output`).
    pub stderr: Option<Box<dyn AsyncRead + Send + Unpin>>,
    /// Child to kill/reap on close (dev runtime only; the vsock session ends
    /// when the streams drop and the per-connection agent exits).
    pub child: Option<Child>,
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

    /// Periodic runtime maintenance, driven by the background warmer's tick
    /// (e.g. keeping the pre-spawned jailer pool full). Default: nothing.
    async fn maintain(&self) {}

    /// Whether a restorable "golden" image snapshot exists for this image+shape
    /// on this node. When true, an empty-pool create takes the
    /// `snapshot_restore` path (~hundreds of ms) instead of a cold boot (~1.4s),
    /// and warm VMs restored from the same artifact share its mem image's host
    /// page cache (clean guest pages are stored once per image+shape).
    fn golden_snapshot_available(&self, _image_key: &str, _resources: &Resources) -> bool {
        false
    }

    /// Produce the golden snapshot for an image+shape if it is missing: boot a
    /// throwaway VM, snapshot it, publish the artifacts, tear the VM down.
    /// Idempotent; returns true only when a new artifact was produced. Driven by
    /// the hot-pool warmer (it already knows which image+shapes matter).
    async fn ensure_golden_snapshot(&self, _spec: &VmSpec) -> Result<bool> {
        Ok(false)
    }

    /// Set the guest balloon target: `amount_mib` MiB reclaimed from the guest
    /// back to the host (0 deflates fully). The soft-standby tier between
    /// "running" and snapshot eviction — zero resume latency, smaller RSS.
    /// Requires the balloon device (`runtime.balloon`) configured at boot.
    async fn balloon(&self, _handle: &str, _amount_mib: u32) -> Result<()> {
        anyhow::bail!("balloon device not supported by this runtime")
    }

    /// Per-VM working-set metrics. None for an unknown handle.
    async fn vm_metrics(&self, _handle: &str) -> Option<VmMetrics> {
        None
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

    /// Allocate the backing store for a new persistent volume (Phase 5). The
    /// Firecracker runtime formats a labelled ext4 image the guest mounts as a
    /// block device; the dev runtime creates a plain host directory.
    async fn create_volume(&self, volume_id: &str, size_gb: u32) -> Result<()>;

    /// Remove a volume's backing store. Only called once the volume is detached.
    async fn delete_volume(&self, volume_id: &str) -> Result<()>;
}
