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
        let default_kind = if cfg!(target_os = "linux") { "firecracker" } else { "mock" };
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
        }
    }
}

fn default_jailer_uid() -> u32 {
    100_000
}

impl Default for HotPoolConfig {
    fn default() -> Self {
        HotPoolConfig { enabled: true, warm_interval_seconds: 5, base_target: 2 }
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig { bootstrap_admin_key: String::new(), bootstrap_org: "org_admin".to_string() }
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
        if let Ok(v) = std::env::var("WORKDIR_RUNTIME") {
            cfg.runtime.kind = v;
        }
        if let Ok(v) = std::env::var("WORKDIR_ADMIN_KEY") {
            cfg.auth.bootstrap_admin_key = v;
        }
        if let Ok(v) = std::env::var("WORKDIR_RPC_TOKEN") {
            cfg.node.rpc_token = v;
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
        if cfg.runtime.kernel_image.is_empty() {
            cfg.runtime.kernel_image = data.join("kernel/vmlinux").to_string_lossy().to_string();
        }
        Ok(cfg)
    }

    pub fn db_path(&self) -> PathBuf {
        self.server.data_dir.join("workdir.db")
    }
}
