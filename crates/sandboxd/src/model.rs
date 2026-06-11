//! Domain model: sandboxes, nodes, custom images, boot paths, timings, and the
//! create-request / create-response wire shapes (spec §13, §14, §19).

use crate::knobs::{Resources, ResourcesRequest};
use crate::lifecycle::State;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a sandbox came from. The response MUST report this honestly so that
/// best-case hot-pool numbers are never published unlabeled (spec §3.5, §21).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootPath {
    HotPool,
    SnapshotRestore,
    ColdBoot,
}

impl BootPath {
    pub fn as_str(&self) -> &'static str {
        match self {
            BootPath::HotPool => "hot_pool",
            BootPath::SnapshotRestore => "snapshot_restore",
            BootPath::ColdBoot => "cold_boot",
        }
    }
}

/// Timing breakdown returned on every create (spec §14). Optional-feature time
/// is reported separately from boot time so benchmarks stay honest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Timings {
    #[serde(default)]
    pub boot_ms: u64,
    #[serde(default)]
    pub image_cache_ms: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub browser_ready_ms: u64,
    /// Time spent installing the opt-in coding agent (0 unless requested).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub agent_ms: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub git_ms: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub install_ms: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ready_ms: u64,
    #[serde(default)]
    pub total_ms: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

// ---------------------------------------------------------------------------
// Startup recipe (spec §14)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StartupRecipe {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitSpec>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// Secret *names* only. Values are injected late from the org secret store
    /// and never included in snapshots (spec §14 feature table).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CommandSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<ReadyCheck>,
    #[serde(default)]
    pub network: NetworkPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSpec {
    pub url: String,
    #[serde(default = "default_ref")]
    pub r#ref: String,
    #[serde(default = "default_depth")]
    pub depth: u32,
}

fn default_ref() -> String {
    "main".to_string()
}
fn default_depth() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheSpec {
    #[serde(default)]
    pub package_managers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSpec {
    pub name: String,
    pub run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
    #[serde(default)]
    pub background: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyCheck {
    /// HTTP URL polled until 2xx, e.g. "http://127.0.0.1:3000".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
    #[serde(default = "default_ready_timeout")]
    pub timeout_seconds: u32,
}

fn default_ready_timeout() -> u32 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum EgressMode {
    #[default]
    Default,
    Allowlist,
    Denylist,
}


#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkPolicy {
    #[serde(default)]
    pub egress: EgressMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

// ---------------------------------------------------------------------------
// Browser config (spec §12)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrowserConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub vnc: bool,
    #[serde(default)]
    pub cdp: bool,
}

// ---------------------------------------------------------------------------
// Coding agent (feature): a lightweight in-sandbox coding-agent CLI (opencode by
// default). Opt-in — it is NOT baked into the base rootfs, so most sandboxes
// stay minimal; when requested it is installed into the guest at provision time.
// Provide a provider API key via `startup.secrets` (e.g. ANTHROPIC_API_KEY) to
// make it usable.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodingAgentConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Which agent CLI to install. Currently only "opencode" (the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Optional version to pin; defaults to the installer's latest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl CodingAgentConfig {
    /// The agent CLI to install, defaulting to opencode.
    pub fn kind(&self) -> &str {
        self.kind.as_deref().unwrap_or("opencode")
    }
}

// ---------------------------------------------------------------------------
// Docker-in-Docker (feature): dockerd runs INSIDE the guest microVM. The VM is
// the isolation boundary; the host Docker socket is never exposed (spec §18).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DockerConfig {
    #[serde(default)]
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// Bucket mounts (feature): mount an S3 bucket into the guest via mountpoint-s3.
// Credentials come from the sandbox's injected secret env (AWS_ACCESS_KEY_ID /
// AWS_SECRET_ACCESS_KEY), never inline here.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSpec {
    /// Currently only "s3".
    #[serde(rename = "type")]
    pub kind: String,
    pub bucket: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Absolute guest path, e.g. "/mnt/data".
    pub mount_path: String,
    #[serde(default = "default_true")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Custom S3-compatible endpoint (MinIO, R2, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Ephemeral files (feature): inline files written into the workspace at boot,
// living only for the session (wiped on delete, never snapshotted).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct EphemeralFile {
    pub path: String,
    pub content: String,
    /// "utf8" (default) or "base64".
    #[serde(default)]
    pub encoding: Option<String>,
}

// ---------------------------------------------------------------------------
// Create request (spec §3.3, §3.4, §19)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateSandboxRequest {
    #[serde(default)]
    pub resources: Option<ResourcesRequest>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub browser: Option<BrowserConfig>,
    #[serde(default)]
    pub startup: Option<serde_json::Value>,
    #[serde(default)]
    pub auto_stop_seconds: Option<u32>,
    /// Enable persistent writable disk / snapshot semantics for stop/resume.
    #[serde(default)]
    pub snapshot: Option<bool>,
    /// Pin a specific published custom image version.
    #[serde(default)]
    pub image_version: Option<String>,
    /// Run dockerd inside the guest microVM (docker-in-docker).
    #[serde(default)]
    pub docker: Option<DockerConfig>,
    /// Install a lightweight coding-agent CLI (opencode) into the guest. Opt-in;
    /// not present unless requested.
    #[serde(default)]
    pub coding_agent: Option<CodingAgentConfig>,
    /// Bucket mounts to attach inside the guest.
    #[serde(default)]
    pub mounts: Option<Vec<MountSpec>>,
    /// Inline ephemeral files written into the workspace at boot.
    #[serde(default)]
    pub files: Option<Vec<EphemeralFile>>,
}

// ---------------------------------------------------------------------------
// Persisted sandbox record
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sandbox {
    pub id: String,
    pub org_id: String,
    pub state: State,
    pub node_id: Option<String>,
    pub image: String,
    pub resources: Resources,
    pub auto_stop_seconds: u32,
    pub snapshot_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<BrowserConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup: Option<StartupRecipe>,
    pub boot_path: BootPath,
    pub timings: Timings,
    /// Names (not values) of secrets injected into this sandbox. Used for the
    /// view and to refuse snapshots while secrets are resident (review M3).
    #[serde(default)]
    pub secret_names: Vec<String>,
    /// Whether dockerd is running inside the guest.
    #[serde(default)]
    pub docker: bool,
    /// The coding-agent CLI installed in the guest (e.g. "opencode"), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coding_agent: Option<String>,
    /// Bucket mounts attached to the guest (no credentials).
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    /// Preview ports exposed via the wildcard proxy.
    #[serde(default)]
    pub ports: Vec<u16>,
    /// Opaque handle the runtime uses to find this VM (workspace dir / vm id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Last time we observed activity; drives the idle auto-stop detector.
    pub last_active_at: DateTime<Utc>,
}

impl Sandbox {
    /// Is the VNC/CDP preview applicable?
    pub fn browser_enabled(&self) -> bool {
        self.browser.as_ref().map(|b| b.enabled).unwrap_or(false)
    }
}

// Re-exported via crate::knobs but referenced here for convenience.
pub use crate::knobs::ResourcesRequest as WireResources;
