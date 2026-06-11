//! Custom image registry types (spec §11). Custom images are built/imported
//! asynchronously and never built or pulled synchronously on the create path.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageStatus {
    /// Build/import in progress.
    Building,
    /// Published immutable version, usable for sandbox create.
    Ready,
    /// Build failed; previous versions remain usable.
    Failed,
    /// Soft-deleted: prevents new creates, does not kill running sandboxes.
    Deleted,
}

impl ImageStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ImageStatus::Building => "building",
            ImageStatus::Ready => "ready",
            ImageStatus::Failed => "failed",
            ImageStatus::Deleted => "deleted",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ImageSource {
    /// Build from a Dockerfile + build context tarball URL.
    Dockerfile { context_url: String, #[serde(default = "default_dockerfile")] dockerfile: String },
    /// Import a pre-built OCI image reference.
    Oci { image_ref: String },
}

fn default_dockerfile() -> String {
    "Dockerfile".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourcesHint {
    #[serde(default)]
    pub cpu: Option<f64>,
    #[serde(default)]
    pub memory_mb: Option<u32>,
    #[serde(default)]
    pub disk_gb: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomImage {
    pub id: String,
    pub org_id: String,
    /// Canonical name `custom/<org>/<name>`.
    pub name: String,
    /// Immutable version string, e.g. a date or content hash.
    pub version: String,
    pub source: ImageSource,
    pub status: ImageStatus,
    pub resources_hint: ResourcesHint,
    /// Appended build/import log lines (spec §11.3 "expose image build logs").
    #[serde(default)]
    pub build_log: String,
    /// First-node image cache miss time once measured (spec §11.3).
    #[serde(default)]
    pub first_node_cache_miss_ms: Option<u64>,
    /// Approx stored artifact size, billed separately (spec §11.3, §17).
    #[serde(default)]
    pub storage_bytes: u64,
    /// Ephemeral images are garbage-collected after `expires_at` once no active
    /// sandbox references them (feature).
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl CustomImage {
    /// Full reference used in sandbox create: `custom/<org>/<name>:<version>`.
    pub fn reference(&self) -> String {
        format!("{}:{}", self.name, self.version)
    }
}

/// Wire shape for `POST /v1/images` (spec §11.2).
#[derive(Debug, Clone, Deserialize)]
pub struct CreateImageRequest {
    pub source: CreateImageSource,
    pub name: String,
    #[serde(default)]
    pub resources_hint: ResourcesHint,
    /// Build an ephemeral image that is auto-GC'd after `ttl_seconds` once no
    /// active sandbox uses it (feature).
    #[serde(default)]
    pub ephemeral: bool,
    /// TTL for ephemeral images (default 1 hour).
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(default)]
    pub context_url: Option<String>,
    #[serde(default)]
    pub dockerfile: Option<String>,
    #[serde(default)]
    pub image_ref: Option<String>,
}
