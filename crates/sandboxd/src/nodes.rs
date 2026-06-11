//! Node registry types (spec §6.3, §7, §8). Nodes are homogeneous Hetzner
//! dedicated servers that join one control plane.

use crate::capacity::{node_capacity, NodeCapacity};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub hostname: String,
    /// "all-in-one" or "worker".
    pub role: String,
    pub total_memory_gb: f64,
    /// Internal address for cross-node placement RPC (host:port).
    pub advertise_addr: String,
    /// Marked false while draining; the scheduler skips unschedulable nodes.
    pub schedulable: bool,
    pub draining: bool,
    /// Preflight result; a node with kvm_ok=false can register but not host VMs.
    pub kvm_ok: bool,
    pub registered_at: DateTime<Utc>,
    pub last_heartbeat_at: DateTime<Utc>,
}

impl Node {
    pub fn capacity(&self) -> NodeCapacity {
        node_capacity(self.total_memory_gb)
    }

    /// True if the node should receive new placements right now.
    pub fn is_available(&self) -> bool {
        self.schedulable && !self.draining && self.kvm_ok
    }
}
