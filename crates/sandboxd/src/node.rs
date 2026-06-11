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
    DirEntry, ExecRequest, ExecResult, PtySession, Runtime, SnapshotArtifact, VmInstance, VmSpec,
    WarmVm,
};
use anyhow::Result;
use async_trait::async_trait;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

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
    async fn snapshot(&self, handle: &str) -> Result<SnapshotArtifact>;
    async fn delete(&self, handle: &str) -> Result<()>;

    /// Ready warm VMs matching an exact image+shape, for the scheduler input.
    async fn hot_pool_available(&self, image_key: &str, resources: &Resources) -> u32;
}

pub struct LocalNode {
    node_id: String,
    runtime: Arc<dyn Runtime>,
    hotpools: Mutex<HotPools>,
}

impl LocalNode {
    pub fn new(node_id: impl Into<String>, runtime: Arc<dyn Runtime>) -> LocalNode {
        LocalNode { node_id: node_id.into(), runtime, hotpools: Mutex::new(HotPools::new()) }
    }

    pub fn runtime(&self) -> Arc<dyn Runtime> {
        self.runtime.clone()
    }

    /// Configure default hot-pool targets (spec §10.1) plus an override for the
    /// base pool target.
    pub async fn configure_default_pools(&self, base_target: u32) {
        let mut pools = self.hotpools.lock().await;
        for (image_key, shape, target) in catalog::default_hot_pools() {
            let t = if image_key == "base" { base_target } else { target };
            pools.set_target(ShapeKey::new(image_key, &shape), t);
        }
    }

    pub async fn pool_status(&self) -> Vec<PoolStatus> {
        self.hotpools.lock().await.status()
    }

    /// Warm pools toward their targets, one VM per pending shape per call.
    /// Returns how many VMs were warmed.
    pub async fn warm_once(&self) -> usize {
        let pending = { self.hotpools.lock().await.pending_warm() };
        let mut warmed = 0;
        for (key, _deficit) in pending {
            // Skip pools whose image isn't built on this node (no log spam).
            if !self.runtime.image_available(&key.image_key) {
                continue;
            }
            let spec = VmSpec {
                sandbox_id: crate::ids::sandbox_id(),
                org_id: "pool".to_string(),
                image_key: key.image_key.clone(),
                image_ref: key.image_key.clone(),
                resources: Resources {
                    cpu: key.cpu_milli as f64 / 1000.0,
                    memory_mb: key.memory_mb,
                    disk_gb: key.disk_gb,
                },
                env: Default::default(),
                secret_env: Default::default(),
                browser: None,
                docker: false,
                coding_agent: None,
                mounts: Vec::new(),
                files: Vec::new(),
            };
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
        let warm = {
            let mut pools = self.hotpools.lock().await;
            pools.claim(&key).map(|handle| WarmVm {
                handle,
                image_key: spec.image_key.clone(),
                resources: spec.resources,
            })
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
    async fn snapshot(&self, handle: &str) -> Result<SnapshotArtifact> {
        self.runtime.snapshot(handle).await
    }
    async fn delete(&self, handle: &str) -> Result<()> {
        self.runtime.delete(handle).await
    }

    async fn hot_pool_available(&self, image_key: &str, resources: &Resources) -> u32 {
        let key = ShapeKey::new(image_key, resources);
        self.hotpools.lock().await.available(&key)
    }
}
