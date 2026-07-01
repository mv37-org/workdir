//! Data-plane node abstraction.
//!
//! The control plane talks to a node through [`NodeClient`]. In the all-in-one
//! deployment (spec §6.2) the only node is [`LocalNode`], which drives the
//! in-process [`Runtime`] and owns this node's hot pools. The trait is the seam
//! where a `RemoteNodeClient` (control plane → worker host agent over the
//! internal API) plugs in for the multi-node data path described in
//! ARCHITECTURE.md; registry/join/drain/scheduling already span all nodes.

use crate::catalog;
use crate::hotpool::{HotPools, PoolStatus, ShapeKey};
use crate::knobs::Resources;
use crate::runtime::{
    DirEntry, ExecRequest, ExecResult, PtySession, Runtime, SnapshotArtifact, VmInstance,
    VmMetrics, VmSpec, WarmVm,
};
use anyhow::Result;
use async_trait::async_trait;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("file not found: {0}")]
    FileNotFound(String),
}

pub fn file_not_found(path: impl Into<String>) -> anyhow::Error {
    NodeError::FileNotFound(path.into()).into()
}

pub fn is_file_not_found(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<NodeError>(),
        Some(NodeError::FileNotFound(_))
    )
}

#[async_trait]
pub trait NodeClient: Send + Sync {
    fn node_id(&self) -> &str;

    /// Allocate a microVM for `spec`, choosing the boot path locally: claim a
    /// matching warm VM (hot pool), else restore a snapshot, else cold boot.
    async fn place(&self, spec: &VmSpec, snapshot_available: bool) -> Result<VmInstance>;

    async fn exec(&self, handle: &str, req: &ExecRequest) -> Result<ExecResult>;
    async fn write_file(&self, handle: &str, path: &str, bytes: &[u8]) -> Result<()>;
    async fn read_file(&self, handle: &str, path: &str) -> Result<Vec<u8>>;
    async fn list_dir(&self, handle: &str, path: &str) -> Result<Vec<DirEntry>>;
    async fn expose_port(&self, handle: &str, port: u16) -> Result<SocketAddr>;
    async fn ready_check(&self, handle: &str, url: &str, timeout_seconds: u32) -> Result<()>;
    async fn pause(&self, handle: &str, persist: bool) -> Result<()>;
    async fn resume(&self, handle: &str) -> Result<u64>;
    /// Snapshot + free RAM (perpetual standby, Phase 1). Returns evict time (ms).
    async fn standby(&self, handle: &str) -> Result<u64>;
    /// Restore a standby VM from its snapshot. Returns restore latency (ms).
    async fn restore(&self, handle: &str) -> Result<u64>;
    async fn snapshot(&self, handle: &str) -> Result<SnapshotArtifact>;
    /// Clone a parent VM into an instant sibling (Phase 3 fork).
    async fn fork(&self, parent_handle: &str, child_spec: &VmSpec) -> Result<VmInstance>;
    async fn delete(&self, handle: &str) -> Result<()>;

    /// Ready warm VMs matching an exact image+shape, for the scheduler input.
    async fn hot_pool_available(&self, image_key: &str, resources: &Resources) -> u32;

    /// Set the guest balloon target (soft standby). Default: unsupported —
    /// worker RPC for this lands with Phase 4.
    async fn balloon(&self, _handle: &str, _amount_mib: u32) -> Result<()> {
        anyhow::bail!("balloon not supported on this node client")
    }

    /// Per-VM working-set metrics. Default: unknown (remote nodes report theirs
    /// once worker RPC lands, Phase 4).
    async fn vm_metrics(&self, _handle: &str) -> Option<VmMetrics> {
        None
    }
}

pub struct LocalNode {
    node_id: String,
    runtime: Arc<dyn Runtime>,
    hotpools: Mutex<HotPools>,
}

impl LocalNode {
    pub fn new(node_id: impl Into<String>, runtime: Arc<dyn Runtime>) -> LocalNode {
        LocalNode {
            node_id: node_id.into(),
            runtime,
            hotpools: Mutex::new(HotPools::new()),
        }
    }

    pub fn runtime(&self) -> Arc<dyn Runtime> {
        self.runtime.clone()
    }

    /// Configure default hot-pool targets (spec §10.1) plus an override for the
    /// base pool target.
    pub async fn configure_default_pools(&self, base_target: u32) {
        let mut pools = self.hotpools.lock().await;
        for (image_key, shape, target) in catalog::default_hot_pools() {
            let t = if image_key == "base" {
                base_target
            } else {
                target
            };
            pools.set_target(ShapeKey::new(image_key, &shape), t);
        }
    }

    pub async fn pool_status(&self) -> Vec<PoolStatus> {
        self.hotpools.lock().await.status()
    }

    /// Warm pools toward their targets, one VM per pending shape per call.
    /// Returns how many VMs were warmed.
    pub async fn warm_once(&self) -> usize {
        // Runtime maintenance rides the warmer tick (e.g. jailer-pool refill).
        self.runtime.maintain().await;

        // Golden snapshots first: once one exists for an image+shape, the warm
        // VMs below restore from it (sharing its mem image's host page cache)
        // and an empty-pool create takes `snapshot_restore` instead of a cold
        // boot. ensure_golden_snapshot is a cheap existence check after the
        // first production.
        let status = { self.hotpools.lock().await.status() };
        for st in &status {
            if !self.runtime.image_available(&st.image_key) {
                continue;
            }
            let spec = Self::pool_spec(&st.image_key, st.cpu, st.memory_mb, st.disk_gb);
            match self.runtime.ensure_golden_snapshot(&spec).await {
                Ok(true) => {
                    tracing::info!(image = %st.image_key, memory_mb = st.memory_mb, "produced golden image snapshot")
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, image = %st.image_key, "golden snapshot production failed")
                }
            }
        }

        let pending = { self.hotpools.lock().await.pending_warm() };
        let mut warmed = 0;
        for (key, _deficit) in pending {
            // Skip pools whose image isn't built on this node (no log spam).
            if !self.runtime.image_available(&key.image_key) {
                continue;
            }
            let spec = Self::pool_spec(
                &key.image_key,
                key.cpu_milli as f64 / 1000.0,
                key.memory_mb,
                key.disk_gb,
            );
            match self.runtime.prewarm(&spec).await {
                Ok(warm) => {
                    self.hotpools.lock().await.push(key, warm.handle);
                    warmed += 1;
                }
                Err(e) => {
                    tracing::warn!(error = %e, image = %key.image_key, "prewarm failed");
                }
            }
        }
        warmed
    }

    /// A bare pool/golden spec: just the image at a shape, no tenant features.
    fn pool_spec(image_key: &str, cpu: f64, memory_mb: u32, disk_gb: u32) -> VmSpec {
        VmSpec {
            sandbox_id: crate::ids::sandbox_id(),
            org_id: "pool".to_string(),
            image_key: image_key.to_string(),
            image_ref: image_key.to_string(),
            resources: Resources {
                cpu,
                memory_mb,
                disk_gb,
            },
            env: Default::default(),
            secret_env: Default::default(),
            browser: None,
            docker: false,
            coding_agent: None,
            mounts: Vec::new(),
            volumes: Vec::new(),
            files: Vec::new(),
            network: Default::default(),
        }
    }

    /// Whether a golden image snapshot exists locally for this image+shape
    /// (scheduler input + the create path's snapshot_restore gate).
    pub fn golden_snapshot_available(&self, image_key: &str, resources: &Resources) -> bool {
        self.runtime.golden_snapshot_available(image_key, resources)
    }

    pub async fn open_pty(&self, handle: &str) -> Result<PtySession> {
        self.runtime.open_pty(handle).await
    }

    pub fn runtime_kind(&self) -> &'static str {
        self.runtime.kind()
    }
}

#[async_trait]
impl NodeClient for LocalNode {
    fn node_id(&self) -> &str {
        &self.node_id
    }

    async fn place(&self, spec: &VmSpec, snapshot_available: bool) -> Result<VmInstance> {
        let key = ShapeKey::new(&spec.image_key, &spec.resources);
        // A hot-pool VM booted without this sandbox's volume drives, so a sandbox
        // that attaches volumes must cold-boot (drives are configured pre-boot).
        let warm = if spec.volumes.is_empty() {
            let mut pools = self.hotpools.lock().await;
            pools.claim(&key).map(|handle| WarmVm {
                handle,
                image_key: spec.image_key.clone(),
                resources: spec.resources,
            })
        } else {
            None
        };
        self.runtime.create(spec, warm, snapshot_available).await
    }

    async fn exec(&self, handle: &str, req: &ExecRequest) -> Result<ExecResult> {
        self.runtime.exec(handle, req).await
    }
    async fn write_file(&self, handle: &str, path: &str, bytes: &[u8]) -> Result<()> {
        self.runtime.write_file(handle, path, bytes).await
    }
    async fn read_file(&self, handle: &str, path: &str) -> Result<Vec<u8>> {
        self.runtime.read_file(handle, path).await
    }
    async fn list_dir(&self, handle: &str, path: &str) -> Result<Vec<DirEntry>> {
        self.runtime.list_dir(handle, path).await
    }
    async fn expose_port(&self, handle: &str, port: u16) -> Result<SocketAddr> {
        self.runtime.expose_port(handle, port).await
    }
    async fn ready_check(&self, handle: &str, url: &str, timeout_seconds: u32) -> Result<()> {
        self.runtime.ready_check(handle, url, timeout_seconds).await
    }
    async fn pause(&self, handle: &str, persist: bool) -> Result<()> {
        self.runtime.pause(handle, persist).await
    }
    async fn resume(&self, handle: &str) -> Result<u64> {
        self.runtime.resume(handle).await
    }
    async fn standby(&self, handle: &str) -> Result<u64> {
        self.runtime.standby(handle).await
    }
    async fn restore(&self, handle: &str) -> Result<u64> {
        self.runtime.restore(handle).await
    }
    async fn snapshot(&self, handle: &str) -> Result<SnapshotArtifact> {
        self.runtime.snapshot(handle).await
    }
    async fn fork(&self, parent_handle: &str, child_spec: &VmSpec) -> Result<VmInstance> {
        self.runtime.fork(parent_handle, child_spec).await
    }
    async fn delete(&self, handle: &str) -> Result<()> {
        self.runtime.delete(handle).await
    }

    async fn hot_pool_available(&self, image_key: &str, resources: &Resources) -> u32 {
        let key = ShapeKey::new(image_key, resources);
        self.hotpools.lock().await.available(&key)
    }

    async fn balloon(&self, handle: &str, amount_mib: u32) -> Result<()> {
        self.runtime.balloon(handle, amount_mib).await
    }

    async fn vm_metrics(&self, handle: &str) -> Option<VmMetrics> {
        self.runtime.vm_metrics(handle).await
    }
}
