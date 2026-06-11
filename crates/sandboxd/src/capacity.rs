//! Capacity math and default-equivalent units (spec §9.1, §8).
//!
//! Capacity is displayed in default-equivalent units where
//! `1 unit = 1 vCPU / 2 GB / 8 GB base sandbox`. Memory is the admission
//! constraint; the practical default is intentionally below the theoretical
//! memory limit to protect against host pressure, page cache, Firecracker
//! overhead, and browser workloads.

use serde::Serialize;

/// Memory reserved for the host (kernel, page cache, Firecracker/jailer
/// overhead, host agent) and never sold to sandboxes.
pub const DEFAULT_HOST_RESERVE_GB: f64 = 12.0;
/// One default-equivalent unit's memory footprint (the base shape).
pub const UNIT_MEMORY_GB: f64 = 2.0;
/// Fraction of theoretical slots we actually admit, to absorb spikes.
/// 26 theoretical -> 20 practical on a 64 GB node ≈ 0.77.
pub const PRACTICAL_DERATE: f64 = 20.0 / 26.0;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct NodeCapacity {
    pub total_memory_gb: f64,
    pub host_reserve_gb: f64,
    pub usable_for_sandboxes_gb: f64,
    /// memory-only theoretical count of default-shape sandboxes
    pub theoretical_units: u32,
    /// derated admission ceiling in default-equivalent units
    pub practical_units: u32,
}

/// Compute node capacity from total RAM. For a 64 GB node this yields
/// usable=52, theoretical=26, practical=20 (spec §9.1).
pub fn node_capacity(total_memory_gb: f64) -> NodeCapacity {
    let host_reserve_gb = DEFAULT_HOST_RESERVE_GB;
    let usable = (total_memory_gb - host_reserve_gb).max(0.0);
    let theoretical = (usable / UNIT_MEMORY_GB).floor() as u32;
    let practical = (theoretical as f64 * PRACTICAL_DERATE).floor() as u32;
    NodeCapacity {
        total_memory_gb,
        host_reserve_gb,
        usable_for_sandboxes_gb: usable,
        theoretical_units: theoretical,
        practical_units: practical,
    }
}

/// How many default-equivalent units a memory footprint consumes. Used both for
/// admission and for the dashboard's "default-equivalent capacity" display.
pub fn units_for_memory_gb(memory_gb: f64) -> f64 {
    memory_gb / UNIT_MEMORY_GB
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixty_four_gb_node_matches_spec() {
        let c = node_capacity(64.0);
        assert_eq!(c.usable_for_sandboxes_gb, 52.0);
        assert_eq!(c.theoretical_units, 26);
        assert_eq!(c.practical_units, 20);
    }
}
