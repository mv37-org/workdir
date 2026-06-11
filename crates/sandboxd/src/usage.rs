//! Billing primitives: orgs, API keys, and per-second usage intervals
//! (spec §22 "per-second running compute").

use crate::knobs::Resources;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrgStatus {
    Active,
    /// Flagged by abuse detection; still readable, placements throttled.
    Review,
    /// Kill switch engaged (spec §18): no new sandboxes, running ones stopped.
    Suspended,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Org {
    pub id: String,
    pub name: String,
    pub status: OrgStatus,
    /// Prepaid balance in USD (spec §22 "prepaid credits").
    pub prepaid_credits_usd: f64,
    /// Accumulated charges in USD, drawn down against prepaid credits.
    pub spent_usd: f64,
    /// Per-org default-equivalent unit quota (spec §22 "per-org resource quotas").
    /// Convention: a value of `<= 0` means unlimited.
    pub quota_units: f64,
    pub created_at: DateTime<Utc>,
}

impl Org {
    pub fn quota_unlimited(&self) -> bool {
        self.quota_units <= 0.0
    }

    /// Remaining prepaid balance in USD.
    pub fn balance_usd(&self) -> f64 {
        self.prepaid_credits_usd - self.spent_usd
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    /// SHA-256 hex of the presented key; the plaintext is shown once.
    pub key_hash: String,
    pub org_id: String,
    pub name: String,
    pub admin: bool,
    pub disabled: bool,
    pub created_at: DateTime<Utc>,
}

/// One metered running interval for a sandbox. Open intervals have `ended_at`
/// = None; closed ones contribute `seconds * resource_units * multiplier` to
/// delivered unit-seconds and to the org charge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageInterval {
    pub id: String,
    pub sandbox_id: String,
    pub org_id: String,
    pub resources: Resources,
    pub image_key: String,
    pub resource_units: f64,
    pub image_multiplier: f64,
    pub unit_price_usd_hr: f64,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl UsageInterval {
    /// Seconds elapsed, using `now` for still-open intervals.
    pub fn seconds(&self, now: DateTime<Utc>) -> f64 {
        let end = self.ended_at.unwrap_or(now);
        (end - self.started_at).num_milliseconds().max(0) as f64 / 1000.0
    }

    pub fn cost_usd(&self, now: DateTime<Utc>) -> f64 {
        let hours = self.seconds(now) / 3600.0;
        self.unit_price_usd_hr * self.resource_units * self.image_multiplier * hours
    }

    pub fn delivered_unit_seconds(&self, now: DateTime<Utc>) -> f64 {
        self.seconds(now) * self.resource_units * self.image_multiplier
    }
}
