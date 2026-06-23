//! Hot pool bookkeeping (spec §9.1 targets, §10.1 default pools, §15 scheduler
//! input). A hot pool holds pre-booted warm microVMs for a given image+shape so
//! a matching create can claim one instantly (the `hot_pool` boot path).
//!
//! This is a plain data structure; the local node owns it behind an async mutex
//! and a background warmer reconciles `ready` toward `target`.

use crate::knobs::Resources;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};

/// Identity of a warm pool: an image family at an exact resource shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ShapeKey {
    pub image_key: String,
    pub cpu_milli: u32,
    pub memory_mb: u32,
    pub disk_gb: u32,
}

impl ShapeKey {
    pub fn new(image_key: &str, r: &Resources) -> ShapeKey {
        ShapeKey {
            image_key: image_key.to_string(),
            cpu_milli: (r.cpu * 1000.0).round() as u32,
            memory_mb: r.memory_mb,
            disk_gb: r.disk_gb,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PoolStatus {
    pub image_key: String,
    pub cpu: f64,
    pub memory_mb: u32,
    pub disk_gb: u32,
    pub target: u32,
    pub ready: u32,
    pub deficit: u32,
}

#[derive(Default)]
pub struct HotPools {
    targets: HashMap<ShapeKey, u32>,
    ready: HashMap<ShapeKey, VecDeque<String>>,
}

impl HotPools {
    pub fn new() -> HotPools {
        HotPools::default()
    }

    pub fn set_target(&mut self, key: ShapeKey, target: u32) {
        self.targets.insert(key, target);
    }

    pub fn target(&self, key: &ShapeKey) -> u32 {
        self.targets.get(key).copied().unwrap_or(0)
    }

    pub fn available(&self, key: &ShapeKey) -> u32 {
        self.ready.get(key).map(|q| q.len() as u32).unwrap_or(0)
    }

    /// Claim a warm VM handle for an exact shape, if one is ready.
    pub fn claim(&mut self, key: &ShapeKey) -> Option<String> {
        self.ready.get_mut(key).and_then(|q| q.pop_front())
    }

    /// Add a freshly warmed VM handle to the pool.
    pub fn push(&mut self, key: ShapeKey, handle: String) {
        self.ready.entry(key).or_default().push_back(handle);
    }

    /// How many more warm VMs this pool needs to reach target.
    pub fn deficit(&self, key: &ShapeKey) -> u32 {
        self.target(key).saturating_sub(self.available(key))
    }

    /// All configured pools with their current deficit, for the warmer and the
    /// admin dashboard.
    pub fn status(&self) -> Vec<PoolStatus> {
        let mut keys: Vec<&ShapeKey> = self.targets.keys().collect();
        keys.sort_by(|a, b| {
            a.image_key
                .cmp(&b.image_key)
                .then(a.memory_mb.cmp(&b.memory_mb))
        });
        keys.into_iter()
            .map(|k| {
                let ready = self.available(k);
                let target = self.target(k);
                PoolStatus {
                    image_key: k.image_key.clone(),
                    cpu: k.cpu_milli as f64 / 1000.0,
                    memory_mb: k.memory_mb,
                    disk_gb: k.disk_gb,
                    target,
                    ready,
                    deficit: target.saturating_sub(ready),
                }
            })
            .collect()
    }

    /// Pools that still need warming, as (key, count) work items.
    pub fn pending_warm(&self) -> Vec<(ShapeKey, u32)> {
        self.targets
            .keys()
            .filter_map(|k| {
                let d = self.deficit(k);
                if d > 0 {
                    Some((k.clone(), d))
                } else {
                    None
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_and_refill() {
        let mut pools = HotPools::new();
        let key = ShapeKey::new("base", &Resources::default());
        pools.set_target(key.clone(), 2);
        assert_eq!(pools.deficit(&key), 2);
        pools.push(key.clone(), "vm1".into());
        pools.push(key.clone(), "vm2".into());
        assert_eq!(pools.available(&key), 2);
        assert_eq!(pools.deficit(&key), 0);
        assert_eq!(pools.claim(&key), Some("vm1".into()));
        assert_eq!(pools.deficit(&key), 1);
    }
}
