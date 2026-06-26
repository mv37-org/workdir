//! Domain model: sandboxes, nodes, custom images, boot paths, timings, and the
//! create-request / create-response wire shapes (spec §13, §14, §19).

use crate::knobs::{Resources, ResourcesRequest};
use crate::lifecycle::State;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::net::Ipv4Addr;

/// Where a sandbox came from. The response MUST report this honestly so that
/// best-case hot-pool numbers are never published unlabeled (spec §3.5, §21).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootPath {
    HotPool,
    SnapshotRestore,
    ColdBoot,
    /// Cloned from a sibling's snapshot artifact (roadmap Phase 3 fork). Reported
    /// separately so fork latency is never hidden behind cold-boot numbers.
    Fork,
}

impl BootPath {
    pub fn as_str(&self) -> &'static str {
        match self {
            BootPath::HotPool => "hot_pool",
            BootPath::SnapshotRestore => "snapshot_restore",
            BootPath::ColdBoot => "cold_boot",
            BootPath::Fork => "fork",
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
#[derive(PartialEq, Eq, Default)]
pub enum EgressMode {
    #[default]
    Default,
    Allowlist,
    Denylist,
    None,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkPolicy {
    #[serde(default)]
    pub egress: EgressMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<NetworkRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<NetworkRule>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkRuleKind {
    Cidr,
    Domain,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkProtocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NetworkRule {
    #[serde(rename = "type")]
    pub kind: NetworkRuleKind,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<NetworkProtocol>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,
}

impl<'de> Deserialize<'de> for NetworkRule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum WireRule {
            Shorthand(String),
            Object {
                #[serde(rename = "type")]
                kind: Option<NetworkRuleKind>,
                value: String,
                #[serde(default)]
                protocol: Option<NetworkProtocol>,
                #[serde(default)]
                ports: Vec<u16>,
            },
        }

        match WireRule::deserialize(deserializer)? {
            WireRule::Shorthand(value) => Ok(NetworkRule::from_value(value)),
            WireRule::Object {
                kind,
                value,
                protocol,
                ports,
            } => {
                let mut rule = NetworkRule::from_value(value);
                if let Some(kind) = kind {
                    rule.kind = kind;
                }
                rule.protocol = protocol;
                rule.ports = ports;
                Ok(rule)
            }
        }
    }
}

impl NetworkRule {
    fn from_value(value: String) -> NetworkRule {
        let trimmed = value.trim().to_ascii_lowercase();
        let kind = if parse_ipv4_cidr(&trimmed).is_some() {
            NetworkRuleKind::Cidr
        } else {
            NetworkRuleKind::Domain
        };
        NetworkRule {
            kind,
            value: trimmed,
            protocol: None,
            ports: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.value.trim().is_empty() {
            return Err("network rule value cannot be empty".into());
        }
        if self.ports.contains(&0) {
            return Err(format!("network rule '{}' has invalid port 0", self.value));
        }
        match self.kind {
            NetworkRuleKind::Cidr => {
                parse_ipv4_cidr(&self.value).ok_or_else(|| {
                    format!(
                        "network cidr rule '{}' must be an IPv4 address or CIDR",
                        self.value
                    )
                })?;
            }
            NetworkRuleKind::Domain => validate_domain_pattern(&self.value)?,
        }
        Ok(())
    }

    pub fn is_domain(&self) -> bool {
        matches!(self.kind, NetworkRuleKind::Domain)
    }
}

impl NetworkPolicy {
    pub fn validate(&self) -> Result<(), String> {
        match self.egress {
            EgressMode::Default => {
                if !self.allow.is_empty() || !self.deny.is_empty() {
                    return Err("network.egress=default cannot include allow or deny rules".into());
                }
            }
            EgressMode::Allowlist => {
                if self.allow.is_empty() {
                    return Err("network.egress=allowlist requires at least one allow rule".into());
                }
                if !self.deny.is_empty() {
                    return Err("network.egress=allowlist cannot include deny rules".into());
                }
            }
            EgressMode::Denylist => {
                if self.deny.is_empty() {
                    return Err("network.egress=denylist requires at least one deny rule".into());
                }
                if !self.allow.is_empty() {
                    return Err("network.egress=denylist cannot include allow rules".into());
                }
            }
            EgressMode::None => {
                if !self.allow.is_empty() || !self.deny.is_empty() {
                    return Err("network.egress=none cannot include allow or deny rules".into());
                }
            }
        }

        for rule in self.allow.iter().chain(self.deny.iter()) {
            rule.validate()?;
        }
        if matches!(self.egress, EgressMode::Allowlist) {
            for rule in &self.allow {
                if matches!(rule.kind, NetworkRuleKind::Cidr) && cidr_is_hard_denied(&rule.value) {
                    return Err(format!(
                        "network allow rule '{}' overlaps a hard-denied private or metadata range",
                        rule.value
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn uses_domain_rules(&self) -> bool {
        self.allow
            .iter()
            .chain(self.deny.iter())
            .any(NetworkRule::is_domain)
    }

    pub fn rules_for_mode(&self) -> &[NetworkRule] {
        match self.egress {
            EgressMode::Allowlist => &self.allow,
            EgressMode::Denylist => &self.deny,
            EgressMode::Default | EgressMode::None => &[],
        }
    }
}

pub fn domain_matches(pattern: &str, domain: &str) -> bool {
    let pattern = pattern.trim_end_matches('.').to_ascii_lowercase();
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    if let Some(suffix) = pattern.strip_prefix("*.") {
        if !domain.ends_with(&format!(".{suffix}")) {
            return false;
        }
        let prefix = &domain[..domain.len() - suffix.len() - 1];
        return !prefix.is_empty() && !prefix.contains('.');
    }
    pattern == domain
}

fn validate_domain_pattern(value: &str) -> Result<(), String> {
    if value.contains("://") || value.contains('/') || value.contains(':') {
        return Err(format!(
            "network domain rule '{value}' must be a hostname, not a URL"
        ));
    }
    if value == "*" || value.starts_with("*.") && value.matches('*').count() > 1 {
        return Err(format!(
            "network domain rule '{value}' has an unsafe wildcard"
        ));
    }
    if value.contains('*') && !value.starts_with("*.") {
        return Err(format!(
            "network domain rule '{value}' may only use a leading '*.' wildcard"
        ));
    }
    let host = value.strip_prefix("*.").unwrap_or(value);
    if host.len() > 253 || host.is_empty() || !host.contains('.') {
        return Err(format!(
            "network domain rule '{value}' is not a valid domain"
        ));
    }
    for label in host.split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return Err(format!(
                "network domain rule '{value}' has an invalid label"
            ));
        }
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if first == b'-' || last == b'-' {
            return Err(format!(
                "network domain rule '{value}' has an invalid label"
            ));
        }
        if !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        {
            return Err(format!(
                "network domain rule '{value}' has an invalid label"
            ));
        }
    }
    Ok(())
}

pub fn parse_ipv4_cidr(value: &str) -> Option<(Ipv4Addr, u8)> {
    let (ip, prefix) = match value.split_once('/') {
        Some((ip, prefix)) => (ip, prefix.parse::<u8>().ok()?),
        None => (value, 32),
    };
    if prefix > 32 {
        return None;
    }
    Some((ip.parse().ok()?, prefix))
}

fn cidr_is_hard_denied(value: &str) -> bool {
    let Some((ip, prefix)) = parse_ipv4_cidr(value) else {
        return false;
    };
    const HARD_DENY: &[(&str, u8)] = &[
        ("10.0.0.0", 8),
        ("172.16.0.0", 12),
        ("192.168.0.0", 16),
        ("127.0.0.0", 8),
        ("169.254.0.0", 16),
        ("100.100.100.200", 32),
    ];
    HARD_DENY.iter().any(|(net, hard_prefix)| {
        let hard_ip: Ipv4Addr = net.parse().expect("hard-coded IPv4 range");
        cidr_overlaps(ip, prefix, hard_ip, *hard_prefix)
    })
}

fn cidr_overlaps(a: Ipv4Addr, a_prefix: u8, b: Ipv4Addr, b_prefix: u8) -> bool {
    let prefix = a_prefix.min(b_prefix);
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    (u32::from(a) & mask) == (u32::from(b) & mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_rule_shorthand_infers_cidr_or_domain() {
        let cidr: NetworkRule = serde_json::from_str("\"93.184.216.34\"").unwrap();
        assert_eq!(cidr.kind, NetworkRuleKind::Cidr);
        assert_eq!(cidr.value, "93.184.216.34");

        let domain: NetworkRule = serde_json::from_str("\"API.OpenAI.com\"").unwrap();
        assert_eq!(domain.kind, NetworkRuleKind::Domain);
        assert_eq!(domain.value, "api.openai.com");
    }

    #[test]
    fn network_policy_rejects_invalid_mode_shapes() {
        let none_with_allow: NetworkPolicy = serde_json::from_value(serde_json::json!({
            "egress": "none",
            "allow": ["example.com"]
        }))
        .unwrap();
        assert!(none_with_allow.validate().is_err());

        let empty_allowlist: NetworkPolicy = serde_json::from_value(serde_json::json!({
            "egress": "allowlist"
        }))
        .unwrap();
        assert!(empty_allowlist.validate().is_err());
    }

    #[test]
    fn network_policy_rejects_urls_and_unsafe_wildcards() {
        for value in ["https://example.com", "example.com/path", "*", "api.*.com"] {
            let policy: NetworkPolicy = serde_json::from_value(serde_json::json!({
                "egress": "allowlist",
                "allow": [{ "type": "domain", "value": value }]
            }))
            .unwrap();
            assert!(policy.validate().is_err(), "{value} should be invalid");
        }
    }

    #[test]
    fn allowlist_rejects_private_or_metadata_cidrs() {
        for value in ["10.0.0.0/8", "192.168.1.1", "169.254.169.254"] {
            let policy: NetworkPolicy = serde_json::from_value(serde_json::json!({
                "egress": "allowlist",
                "allow": [{ "type": "cidr", "value": value }]
            }))
            .unwrap();
            assert!(policy.validate().is_err(), "{value} should be hard-denied");
        }
    }

    #[test]
    fn domain_wildcards_match_one_label_only() {
        assert!(domain_matches("*.example.com", "api.example.com"));
        assert!(!domain_matches("*.example.com", "deep.api.example.com"));
        assert!(!domain_matches("*.example.com", "example.com"));
        assert!(domain_matches("api.example.com", "api.example.com."));
    }
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
// Persistent volumes (Phase 5): org-scoped block storage that outlives any one
// sandbox. A volume is a backing ext4 image on the host; it can be attached to
// at most one running sandbox at a time and survives that sandbox's deletion.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    pub id: String,
    pub org_id: String,
    pub name: String,
    pub size_gb: u32,
    /// The sandbox this volume is currently attached to, if any (exclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_to: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `POST /v1/volumes` body.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateVolumeRequest {
    pub name: String,
    pub size_gb: u32,
}

/// One volume attachment requested at sandbox-create time.
#[derive(Debug, Clone, Deserialize)]
pub struct VolumeAttachRequest {
    pub volume_id: String,
    /// Absolute guest path to mount the volume at, e.g. "/mnt/data".
    pub mount_path: String,
}

/// A resolved volume attachment carried on the sandbox + runtime spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeAttach {
    pub volume_id: String,
    pub mount_path: String,
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
    /// Persistent volumes to attach (block storage surviving delete).
    #[serde(default)]
    pub volumes: Option<Vec<VolumeAttachRequest>>,
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
    /// Persistent volumes attached to the guest (survive this sandbox's delete).
    #[serde(default)]
    pub volumes: Vec<VolumeAttach>,
    /// Effective network policy enforced by the host runtime.
    #[serde(default)]
    pub network: NetworkPolicy,
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
