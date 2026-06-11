//! Placement scheduler (spec §15). Places configurable Firecracker sandboxes
//! onto homogeneous Hetzner nodes. Pure scoring over node snapshots so it is
//! deterministic and unit-testable; the caller gathers snapshots and applies
//! the chosen placement.

use crate::capacity::UNIT_MEMORY_GB;
use crate::knobs::Resources;
use crate::model::BootPath;
use crate::nodes::Node;
use serde::Serialize;

/// What we are trying to place.
#[derive(Debug, Clone)]
pub struct PlacementRequest {
    pub org_id: String,
    pub image_key: String,
    pub resources: Resources,
    pub browser_required: bool,
    pub is_custom_image: bool,
}

/// A node's current state as seen by the scheduler.
#[derive(Debug, Clone)]
pub struct NodeSnapshot {
    pub node: Node,
    /// Memory (GB) currently committed to active sandboxes.
    pub used_memory_gb: f64,
    pub active_count: u32,
    /// Active sandboxes already belonging to the requesting org on this node.
    pub org_active_count: u32,
    /// Ready warm VMs that match the requested image+shape exactly.
    pub hot_pool_available: u32,
    /// Whether the image rootfs is cached on this node (curated always true).
    pub image_cached: bool,
    /// Whether a restorable snapshot for this shape exists on this node.
    pub snapshot_available: bool,
    /// 0.0 = idle, 1.0 = saturated.
    pub cpu_pressure: f64,
    pub io_pressure: f64,
    pub custom_image_cache_pressure: f64,
}

impl NodeSnapshot {
    /// Memory admission ceiling in GB: practical default-equivalent units times
    /// the base unit's footprint. Intentionally below usable memory (spec §9.1).
    fn admission_ceiling_gb(&self) -> f64 {
        self.node.capacity().practical_units as f64 * UNIT_MEMORY_GB
    }

    fn fits(&self, req: &Resources) -> bool {
        self.used_memory_gb + req.memory_gb() <= self.admission_ceiling_gb() + 1e-9
    }

    fn free_ratio_after(&self, req: &Resources) -> f64 {
        let ceiling = self.admission_ceiling_gb();
        if ceiling <= 0.0 {
            return 0.0;
        }
        ((ceiling - self.used_memory_gb - req.memory_gb()) / ceiling).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Placement {
    pub node_id: String,
    pub score: f64,
    /// The boot path the scheduler expects; the runtime confirms the actual one.
    pub boot_path_hint: BootPath,
}

#[derive(Debug, Clone, Serialize)]
pub struct Rejection {
    /// Stable machine code (spec §15 "expose why a request was rejected").
    pub reason: String,
    pub detail: String,
}

/// Score one node for one request. Returns None if the node cannot admit.
pub fn score_node(req: &PlacementRequest, snap: &NodeSnapshot) -> Option<f64> {
    if !snap.node.is_available() {
        return None;
    }
    if req.browser_required && snap.node.total_memory_gb < 16.0 {
        // Browser shapes need real RAM; tiny nodes are not eligible.
        return None;
    }
    if !snap.fits(&req.resources) {
        return None;
    }

    let matching_hot_pool_available = if snap.hot_pool_available > 0 { 1.0 } else { 0.0 };
    let image_cached = if snap.image_cached { 1.0 } else { 0.0 };
    let memory_fit_score = snap.free_ratio_after(&req.resources);
    let low_cpu_pressure = (1.0 - snap.cpu_pressure).clamp(0.0, 1.0);
    let low_io_pressure = (1.0 - snap.io_pressure).clamp(0.0, 1.0);

    let custom_image_cache_miss_penalty = if req.is_custom_image && !snap.image_cached {
        80.0 + 40.0 * snap.custom_image_cache_pressure
    } else {
        0.0
    };
    // Customer anti-affinity: discourage piling one org's sandboxes on one node.
    let noisy_customer_penalty = snap.org_active_count as f64 * 15.0;

    let score = matching_hot_pool_available * 100.0
        + image_cached * 60.0
        + memory_fit_score * 50.0
        + low_cpu_pressure * 20.0
        + low_io_pressure * 10.0
        - custom_image_cache_miss_penalty
        - noisy_customer_penalty;

    Some(score)
}

/// Choose the best node, or explain why none was chosen.
pub fn select(req: &PlacementRequest, snapshots: &[NodeSnapshot]) -> Result<Placement, Rejection> {
    if snapshots.is_empty() {
        return Err(Rejection {
            reason: "no_nodes".to_string(),
            detail: "no nodes are registered".to_string(),
        });
    }
    let schedulable: Vec<&NodeSnapshot> = snapshots.iter().filter(|s| s.node.is_available()).collect();
    if schedulable.is_empty() {
        return Err(Rejection {
            reason: "no_schedulable_nodes".to_string(),
            detail: "all nodes are draining or unschedulable".to_string(),
        });
    }
    // If browser is required but no node is large enough.
    if req.browser_required && !schedulable.iter().any(|s| s.node.total_memory_gb >= 16.0) {
        return Err(Rejection {
            reason: "no_browser_capable_node".to_string(),
            detail: "no node has enough memory for a browser sandbox".to_string(),
        });
    }
    let any_fit = schedulable.iter().any(|s| s.fits(&req.resources));
    if !any_fit {
        return Err(Rejection {
            reason: "memory_admission".to_string(),
            detail: format!(
                "no node can admit {} GB without exceeding its practical capacity",
                req.resources.memory_gb()
            ),
        });
    }

    let mut best: Option<(f64, &NodeSnapshot)> = None;
    for snap in &schedulable {
        if let Some(score) = score_node(req, snap) {
            if best.as_ref().map(|(b, _)| score > *b).unwrap_or(true) {
                best = Some((score, snap));
            }
        }
    }
    let (score, snap) = best.ok_or(Rejection {
        reason: "no_fit".to_string(),
        detail: "no node satisfied the request".to_string(),
    })?;

    let boot_path_hint = if snap.hot_pool_available > 0 {
        BootPath::HotPool
    } else if snap.snapshot_available {
        BootPath::SnapshotRestore
    } else {
        BootPath::ColdBoot
    };

    Ok(Placement { node_id: snap.node.id.clone(), score, boot_path_hint })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn node(id: &str, mem: f64) -> Node {
        Node {
            id: id.to_string(),
            hostname: id.to_string(),
            role: "all-in-one".to_string(),
            total_memory_gb: mem,
            advertise_addr: "127.0.0.1:9000".to_string(),
            schedulable: true,
            draining: false,
            kvm_ok: true,
            registered_at: Utc::now(),
            last_heartbeat_at: Utc::now(),
        }
    }

    fn snap(node: Node) -> NodeSnapshot {
        NodeSnapshot {
            node,
            used_memory_gb: 0.0,
            active_count: 0,
            org_active_count: 0,
            hot_pool_available: 0,
            image_cached: true,
            snapshot_available: false,
            cpu_pressure: 0.0,
            io_pressure: 0.0,
            custom_image_cache_pressure: 0.0,
        }
    }

    fn base_req() -> PlacementRequest {
        PlacementRequest {
            org_id: "o1".to_string(),
            image_key: "base".to_string(),
            resources: Resources::default(),
            browser_required: false,
            is_custom_image: false,
        }
    }

    #[test]
    fn prefers_hot_pool_node() {
        let mut a = snap(node("a", 64.0));
        let mut b = snap(node("b", 64.0));
        a.hot_pool_available = 0;
        b.hot_pool_available = 1;
        let p = select(&base_req(), &[a, b]).unwrap();
        assert_eq!(p.node_id, "b");
        assert_eq!(p.boot_path_hint, BootPath::HotPool);
    }

    #[test]
    fn refuses_when_no_memory() {
        let mut a = snap(node("a", 64.0));
        a.used_memory_gb = 40.0; // at the 20-unit ceiling
        let err = select(&base_req(), &[a]).unwrap_err();
        assert_eq!(err.reason, "memory_admission");
    }

    #[test]
    fn skips_draining_nodes() {
        let mut a = snap(node("a", 64.0));
        a.node.draining = true;
        let err = select(&base_req(), &[a]).unwrap_err();
        assert_eq!(err.reason, "no_schedulable_nodes");
    }

    #[test]
    fn anti_affinity_spreads_orgs() {
        let mut a = snap(node("a", 64.0));
        let b = snap(node("b", 64.0));
        a.org_active_count = 5; // org already crowded on a
        let p = select(&base_req(), &[a, b]).unwrap();
        assert_eq!(p.node_id, "b");
    }
}
