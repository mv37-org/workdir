//! Runtime configuration, loaded from a TOML file with environment overrides.

use crate::pricing::PricingConfig;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub server: ServerConfig,
    pub node: NodeConfig,
    pub runtime: RuntimeConfig,
    pub pricing: PricingConfig,
    pub hotpool: HotPoolConfig,
    pub auth: AuthConfig,
    pub standby: StandbyConfig,
    pub capacity: CapacityConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Address the control-plane API binds to.
    pub bind: String,
    /// Public wildcard domain for preview/VNC URLs, e.g. "sandboxes.example.com".
    pub public_domain: String,
    /// Whether preview URLs use https in their public form.
    pub public_https: bool,
    /// Public port to include in preview URLs when not the scheme default
    /// (443 for https, 80 for http). Set this when the service is reached on a
    /// non-standard port, e.g. on a LAN at :8080.
    pub public_port: Option<u16>,
    /// Extra text appended to `no_capacity` API errors. Hosted demos can use
    /// this to explain that callers should self-host for production; self-hosted
    /// operators can leave it empty.
    pub capacity_exhausted_message: String,
    /// Directory for the SQLite database and runtime state.
    pub data_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    /// "all-in-one" runs control plane + data plane; "worker" runs data plane.
    pub role: String,
    /// Stable node id; generated and persisted if empty.
    pub node_id: String,
    /// Address other nodes / the control plane use to reach this node's agent.
    pub advertise_addr: String,
    /// Total RAM in GB. Auto-detected at boot when 0.
    pub total_memory_gb: f64,
    /// For workers: control-plane base URL to join.
    pub control_plane_url: String,
    /// Join token presented to the control plane.
    pub join_token: String,
    /// Shared cluster secret authenticating control-plane↔worker RPC (the
    /// `/internal` node API). Empty disables the internal API (single-node).
    /// Must be identical on the control plane and every worker.
    pub rpc_token: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
    /// "mock" (dev, any OS) or "firecracker" (production, Linux + /dev/kvm).
    pub kind: String,
    pub firecracker_bin: String,
    pub jailer_bin: String,
    /// Run Firecracker under the jailer (chroot + per-VM uid/gid + cgroups) for
    /// defense-in-depth. Requires the daemon to run as root (the jailer sets up
    /// the chroot and drops privileges). Default false = launch Firecracker
    /// directly; the microVM is still the isolation boundary.
    #[serde(default)]
    pub use_jailer: bool,
    /// Base uid/gid for per-VM jailer isolation; each VM gets base+index.
    #[serde(default = "default_jailer_uid")]
    pub jailer_uid_base: u32,
    pub kernel_image: String,
    /// Directory holding curated/custom rootfs artifacts and snapshots.
    pub images_dir: PathBuf,
    /// Per-sandbox writable workspace / COW disk root.
    pub workspace_dir: PathBuf,
    /// Persistent-volume backing images (Phase 5); survive sandbox deletion.
    #[serde(default)]
    pub volumes_dir: PathBuf,
    /// Snapshot memory backend used on restore (roadmap Phase 2).
    /// `"file"` (default) maps the mem file directly; `"uffd"` serves pages
    /// lazily over a userfaultfd handler so resume returns before the working
    /// set is paged in. `uffd` requires a Linux host with userfaultfd enabled.
    #[serde(default = "default_restore_mem_backend")]
    pub restore_mem_backend: String,
    /// Pull the snapshot's mem file into the page cache just before a restore so
    /// the guest faults against warm pages (Phase 2, lever #2). Cheap; on by
    /// default.
    #[serde(default = "default_true_bool")]
    pub prewarm_mem_cache: bool,
    /// Share one read-only base rootfs across sandboxes (EROFS + tmpfs +
    /// overlayfs) instead of giving each VM a full private COW copy (roadmap
    /// Phase 3 density). Requires base images built as EROFS; see deploy/images.
    #[serde(default)]
    pub shared_rootfs: bool,
    /// Firecracker CPU template (e.g. "T2", "C3", "T2CL") that masks host CPUID
    /// to a portable baseline so a snapshot taken on one host class restores on
    /// another (roadmap Phase 2, lever #4). Empty = no template (host CPUID
    /// passthrough; snapshots are then only portable within identical hardware).
    #[serde(default)]
    pub cpu_template: String,
    /// Attach a virtio-balloon device to every VM (with deflate-on-OOM and 1s
    /// stats polling). Enables the soft-standby tier (`standby.
    /// balloon_idle_seconds`) and per-VM guest memory stats; snapshots carry
    /// the device, so flipping this only affects newly booted VMs.
    #[serde(default)]
    pub balloon: bool,
    /// Keep this many idle jailer+Firecracker processes pre-spawned, each with
    /// its api.sock already listening, so a standby resume / golden restore
    /// skips the jailer relaunch (~30 ms — the measured floor of the resume
    /// path once demand paging landed). 0 disables the pool. Jailer-only.
    #[serde(default)]
    pub jailer_pool_size: u32,
    /// Append `quiet loglevel=1` to the guest kernel cmdline. Serial-console
    /// boot logging runs at virtualized-UART speed and is a measurable share of
    /// cold-boot latency; the console stays attached, so panics still surface.
    /// Disable when debugging guest boot problems.
    #[serde(default = "default_true_bool")]
    pub quiet_guest_boot: bool,
    /// Run Firecracker with its built-in seccomp filter disabled (`--no-seccomp`).
    /// Firecracker's default per-thread filter can SIGSYS-kill the process during
    /// `snapshot/create` (a blocked syscall on the vmm thread; firecracker#1088),
    /// which breaks perpetual standby. Under the jailer (chroot + uid drop + the
    /// KVM boundary) this is a defensible defense-in-depth reduction; the proper
    /// alternative is a custom seccompiler filter. Default false.
    #[serde(default)]
    pub firecracker_no_seccomp: bool,
    /// Require the data filesystem to support reflinks (FICLONE / `cp
    /// --reflink`) before any fork or private-disk staging emits a copy. When
    /// true, the runtime probes the data FS once at startup and FAILS CLOSED —
    /// fork / private-disk staging bail rather than silently emit a multi-GB
    /// full copy on a non-reflink FS (ext4 without reflink, tmpfs, etc.). When
    /// false (default) the staging path falls back to a real copy, preserving
    /// today's behavior on dev / ext4 boxes. This is deliberately softer than
    /// forcing `cp --reflink=always` everywhere, which would also hard-fail
    /// restore's legitimate cross-FS copies — only the multi-GB hot-path copies
    /// are gated.
    #[serde(default)]
    pub require_reflink: bool,
}

fn default_restore_mem_backend() -> String {
    "file".to_string()
}

fn default_true_bool() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HotPoolConfig {
    pub enabled: bool,
    /// How often the warmer reconciles pools toward targets.
    pub warm_interval_seconds: u64,
    /// Override base hot-pool target (spec default 2).
    pub base_target: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StandbyConfig {
    /// When true, the idle reaper parks idle sandboxes in perpetual standby
    /// (snapshot → free RAM → $0 → auto-resume on next request; roadmap Phase 1).
    /// When false (default), it stops them — the pre-Phase-1 behavior. Off by
    /// default so the snapshot/restore path can be validated on a given node
    /// (e.g. via `POST /v1/benchmarks/run`) before real sandboxes depend on it.
    pub enabled: bool,
    /// Soft standby: after this many idle seconds (but before the snapshot
    /// eviction window), inflate the guest balloon to hand free guest memory
    /// back to the host — zero resume latency, smaller RSS. Deflated again when
    /// activity returns. 0 disables. Requires `runtime.balloon`.
    pub balloon_idle_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CapacityConfig {
    /// Memory reserved for the host and never sold to sandboxes (GB). The spec
    /// default (12) predates the shared rootfs (one base image in page cache
    /// instead of N copies); measure before trimming.
    pub host_reserve_gb: f64,
    /// Fraction of theoretical memory slots admitted statically (spec §9.1's
    /// 20/26 on a 64 GB node).
    pub practical_derate: f64,
    /// Admit on measured host memory too: when the static shape-sum ceiling is
    /// full but `MemAvailable` still clears `overcommit_headroom_gb` beyond the
    /// request, admit anyway. Guests fault their pages lazily and idle
    /// sandboxes are parked at $0, so shape sums badly overstate real usage.
    /// Enable together with `psi_standby_threshold` — the pressure reaper is
    /// the backpressure that makes overcommit safe.
    pub overcommit: bool,
    /// Minimum measured headroom (GB) kept free when overcommitting.
    pub overcommit_headroom_gb: f64,
    /// When memory PSI (`some avg10` in /proc/pressure/memory) exceeds this
    /// percentage, park the least-recently-active running sandbox in standby
    /// ahead of its idle window. 0 disables. Requires `[standby] enabled`.
    pub psi_standby_threshold: f64,
}

impl Default for CapacityConfig {
    fn default() -> Self {
        CapacityConfig {
            host_reserve_gb: crate::capacity::DEFAULT_HOST_RESERVE_GB,
            practical_derate: crate::capacity::PRACTICAL_DERATE,
            overcommit: false,
            overcommit_headroom_gb: 8.0,
            psi_standby_threshold: 0.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// If set, this key is seeded as an admin key on first boot (else generated
    /// and printed once). Useful for reproducible installs.
    pub bootstrap_admin_key: String,
    /// Org id for the bootstrap admin key.
    pub bootstrap_org: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind: "0.0.0.0:8080".to_string(),
            public_domain: "sandboxes.local".to_string(),
            public_https: true,
            public_port: None,
            capacity_exhausted_message: String::new(),
            data_dir: PathBuf::from("/var/lib/workdir"),
        }
    }
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            role: "all-in-one".to_string(),
            node_id: String::new(),
            advertise_addr: String::new(),
            total_memory_gb: 0.0,
            control_plane_url: String::new(),
            join_token: String::new(),
            rpc_token: String::new(),
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        // Default to mock so a fresh checkout runs anywhere; the installer flips
        // this to "firecracker" on a real KVM-capable Hetzner node.
        let default_kind = if cfg!(target_os = "linux") {
            "firecracker"
        } else {
            "mock"
        };
        RuntimeConfig {
            kind: default_kind.to_string(),
            firecracker_bin: "/usr/local/bin/firecracker".to_string(),
            jailer_bin: "/usr/local/bin/jailer".to_string(),
            use_jailer: false,
            jailer_uid_base: default_jailer_uid(),
            // Empty paths are filled relative to `data_dir` in `Config::load`.
            kernel_image: String::new(),
            images_dir: PathBuf::new(),
            workspace_dir: PathBuf::new(),
            volumes_dir: PathBuf::new(),
            restore_mem_backend: default_restore_mem_backend(),
            prewarm_mem_cache: true,
            shared_rootfs: false,
            cpu_template: String::new(),
            balloon: false,
            jailer_pool_size: 0,
            quiet_guest_boot: true,
            firecracker_no_seccomp: false,
            require_reflink: false,
        }
    }
}

fn default_jailer_uid() -> u32 {
    100_000
}

impl Default for HotPoolConfig {
    fn default() -> Self {
        HotPoolConfig {
            enabled: true,
            warm_interval_seconds: 5,
            base_target: 2,
        }
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            bootstrap_admin_key: String::new(),
            bootstrap_org: "org_admin".to_string(),
        }
    }
}

impl Config {
    /// Load from a TOML path if it exists, else defaults. Then apply a few
    /// environment overrides handy for containers/tests.
    pub fn load(path: Option<&std::path::Path>) -> anyhow::Result<Config> {
        let mut cfg = match path {
            Some(p) if p.exists() => {
                let text = std::fs::read_to_string(p)?;
                toml::from_str(&text)?
            }
            _ => Config::default(),
        };
        if let Ok(v) = std::env::var("WORKDIR_BIND") {
            cfg.server.bind = v;
        }
        if let Ok(v) = std::env::var("WORKDIR_DATA_DIR") {
            cfg.server.data_dir = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("WORKDIR_PUBLIC_DOMAIN") {
            cfg.server.public_domain = v;
        }
        if let Ok(v) = std::env::var("WORKDIR_PUBLIC_HTTPS") {
            cfg.server.public_https = matches!(v.as_str(), "1" | "true" | "yes");
        }
        if let Ok(v) = std::env::var("WORKDIR_PUBLIC_PORT") {
            cfg.server.public_port = v.parse().ok();
        }
        if let Ok(v) = std::env::var("WORKDIR_CAPACITY_EXHAUSTED_MESSAGE") {
            cfg.server.capacity_exhausted_message = v;
        }
        if let Ok(v) = std::env::var("WORKDIR_RUNTIME") {
            cfg.runtime.kind = v;
        }
        if let Ok(v) = std::env::var("WORKDIR_ADMIN_KEY") {
            cfg.auth.bootstrap_admin_key = v;
        }
        if let Ok(v) = std::env::var("WORKDIR_RPC_TOKEN") {
            cfg.node.rpc_token = v;
        }
        if let Ok(v) = std::env::var("WORKDIR_STANDBY") {
            cfg.standby.enabled = matches!(v.as_str(), "1" | "true" | "yes");
        }
        if let Ok(v) = std::env::var("WORKDIR_FC_NO_SECCOMP") {
            cfg.runtime.firecracker_no_seccomp = matches!(v.as_str(), "1" | "true" | "yes");
        }
        // Derive runtime storage paths from data_dir when not explicitly set, so
        // a single `data_dir` is enough to run anywhere (dev or production).
        let data = cfg.server.data_dir.clone();
        if cfg.runtime.workspace_dir.as_os_str().is_empty() {
            cfg.runtime.workspace_dir = data.join("workspaces");
        }
        if cfg.runtime.images_dir.as_os_str().is_empty() {
            cfg.runtime.images_dir = data.join("images");
        }
        if cfg.runtime.volumes_dir.as_os_str().is_empty() {
            cfg.runtime.volumes_dir = data.join("volumes");
        }
        if cfg.runtime.kernel_image.is_empty() {
            cfg.runtime.kernel_image = data.join("kernel/vmlinux").to_string_lossy().to_string();
        }
        Ok(cfg)
    }

    pub fn db_path(&self) -> PathBuf {
        self.server.data_dir.join("workdir.db")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_deserializes() {
        // The config `gen-config` prints must always parse back through the real
        // loader, so a new field can't drift the two out of sync.
        let cfg: Config = toml::from_str(crate::config_example::EXAMPLE_CONFIG)
            .expect("example config must deserialize");
        assert_eq!(cfg.runtime.restore_mem_backend, "file");
        assert!(cfg.runtime.prewarm_mem_cache);
        assert!(!cfg.runtime.shared_rootfs);
        assert!(!cfg.runtime.require_reflink);
    }

    #[test]
    fn runtime_defaults_are_safe() {
        let r = RuntimeConfig::default();
        assert_eq!(r.restore_mem_backend, "file");
        assert!(r.prewarm_mem_cache);
        assert!(!r.shared_rootfs);
        assert!(r.cpu_template.is_empty());
        assert!(!r.require_reflink);
    }
}
