//! Resource units and the hosted at-cost price model (spec §9.2, §9.3, §22).
//!
//! Memory is the primary constraint; we still charge for larger CPU and disk
//! choices. The default base shape MUST remain visibly cheaper than everything
//! else, which falls out of `resource_units == 1.0` for the base shape.

use crate::catalog::ImageClass;
use crate::knobs::Resources;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Pricing configuration. Defaults reproduce the spec's worked example for a
/// ~EUR 44/month, 64 GB Hetzner node.
#[derive(Debug, Clone, Deserialize)]
pub struct PricingConfig {
    /// USD price per resource-unit-hour at the default base shape.
    pub default_unit_price_usd_hr: f64,
    /// Per-image multipliers, keyed by image class key (see [`ImageClass::key`]).
    pub image_multipliers: HashMap<String, f64>,
    /// Monthly cost of one data-plane node, used for at-cost reconciliation.
    pub monthly_node_cost_usd: f64,
    /// Default-equivalent practical units a 64 GB node delivers (spec §9.1).
    pub node_practical_units: u32,
}

impl Default for PricingConfig {
    fn default() -> Self {
        // raw_default_unit_hour = (44/730)/20 ≈ EUR 0.0030/hr. We publish a
        // USD target near the low end of the spec's 0.008-0.012/hr band for the
        // base shape, leaving headroom for platform overhead and abuse reserve.
        let mut image_multipliers = HashMap::new();
        for c in [
            ImageClass::Base,
            ImageClass::NodePython,
            ImageClass::Browser,
            ImageClass::HeavyBuild,
            ImageClass::Custom(String::new()),
        ] {
            image_multipliers.insert(c.key().to_string(), c.default_multiplier());
        }
        PricingConfig {
            default_unit_price_usd_hr: 0.009,
            image_multipliers,
            monthly_node_cost_usd: 48.0, // ~EUR 44 + tax/overhead, in USD
            node_practical_units: 20,
        }
    }
}

impl PricingConfig {
    pub fn multiplier_for(&self, class: &ImageClass) -> f64 {
        self.image_multipliers
            .get(class.key())
            .copied()
            .unwrap_or_else(|| class.default_multiplier())
    }
}

/// resource_units = max(memory_gb/2, cpu/1, disk_gb/8 * 0.25)  (spec §9.2)
///
/// This makes memory the primary constraint while still charging for larger
/// CPU and disk choices. The base shape (1 vCPU / 2 GB / 8 GB) yields exactly
/// 1.0 units.
pub fn resource_units(r: &Resources) -> f64 {
    let by_mem = r.memory_gb() / 2.0;
    let by_cpu = r.cpu / 1.0;
    let by_disk = (r.disk_gb as f64 / 8.0) * 0.25;
    by_mem.max(by_cpu).max(by_disk)
}

/// Hosted price per hour for a given shape+image (spec §9.2, §22).
/// sandbox_price = unit_price * resource_units * image_multiplier
pub fn sandbox_price_usd_hr(cfg: &PricingConfig, r: &Resources, class: &ImageClass) -> f64 {
    cfg.default_unit_price_usd_hr * resource_units(r) * cfg.multiplier_for(class)
}

/// A priced quote attached to create/get responses.
#[derive(Debug, Clone, Serialize)]
pub struct PriceQuote {
    pub resource_units: f64,
    pub image_multiplier: f64,
    pub unit_price_usd_hr: f64,
    pub price_usd_hr: f64,
    pub price_usd_second: f64,
}

pub fn quote(cfg: &PricingConfig, r: &Resources, class: &ImageClass) -> PriceQuote {
    let units = resource_units(r);
    let mult = cfg.multiplier_for(class);
    let hr = cfg.default_unit_price_usd_hr * units * mult;
    PriceQuote {
        resource_units: round4(units),
        image_multiplier: mult,
        unit_price_usd_hr: cfg.default_unit_price_usd_hr,
        price_usd_hr: round6(hr),
        price_usd_second: round6(hr / 3600.0),
    }
}

/// Cost charged for a metered running interval.
pub fn interval_cost_usd(cfg: &PricingConfig, r: &Resources, class: &ImageClass, seconds: f64) -> f64 {
    sandbox_price_usd_hr(cfg, r, class) * (seconds / 3600.0)
}

/// Reconciled at-cost unit price for the public dashboard (spec §22):
/// unit_price = (host_pool_cost + platform_overhead) / delivered_units
pub fn reconciled_unit_price_usd_hr(
    host_pool_cost_month: f64,
    platform_overhead_month: f64,
    delivered_unit_hours_month: f64,
) -> f64 {
    if delivered_unit_hours_month <= 0.0 {
        return 0.0;
    }
    (host_pool_cost_month + platform_overhead_month) / delivered_unit_hours_month
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}
fn round6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_shape_is_one_unit() {
        let base = Resources::default();
        assert!((resource_units(&base) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn memory_is_primary_constraint() {
        // 1 vCPU / 16 GB / 8 GB -> memory dominates at 8 units.
        let r = Resources { cpu: 1.0, memory_mb: 16384, disk_gb: 8 };
        assert!((resource_units(&r) - 8.0).abs() < 1e-9);
    }

    #[test]
    fn base_is_cheapest() {
        let cfg = PricingConfig::default();
        let base = sandbox_price_usd_hr(&cfg, &Resources::default(), &ImageClass::Base);
        let browser = sandbox_price_usd_hr(
            &cfg,
            &Resources { cpu: 2.0, memory_mb: 4096, disk_gb: 16 },
            &ImageClass::Browser,
        );
        assert!(base < browser, "base {base} should be cheaper than browser {browser}");
    }

    #[test]
    fn base_price_under_target_ceiling() {
        // Exit criterion (spec §26 Phase 5): default base price < USD 0.015/hr.
        let cfg = PricingConfig::default();
        let base = sandbox_price_usd_hr(&cfg, &Resources::default(), &ImageClass::Base);
        assert!(base < 0.015, "base price {base} must stay under 0.015/hr");
    }
}
