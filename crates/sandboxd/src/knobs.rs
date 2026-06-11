//! Constrained resource knobs (spec §3.2).
//!
//! The API MUST reject unsupported arbitrary values such as 13 GB memory or
//! 250 GB disk. Constrained values are required for predictable packing,
//! pricing, and hot-pool management. This module is the single source of truth
//! for what is allowed; everything else asks here.

use serde::{Deserialize, Serialize};

/// Allowed shared-vCPU values.
pub const ALLOWED_CPU: [f64; 4] = [0.5, 1.0, 2.0, 4.0];
/// Allowed memory values in MB (1, 2, 4, 8, 16 GB).
pub const ALLOWED_MEMORY_MB: [u32; 5] = [1024, 2048, 4096, 8192, 16384];
/// Allowed writable disk values in GB.
pub const ALLOWED_DISK_GB: [u32; 4] = [8, 16, 32, 64];
/// Auto-stop idle window bounds, in seconds.
pub const AUTO_STOP_MIN_S: u32 = 30;
pub const AUTO_STOP_MAX_S: u32 = 3600;

/// The default cheap path (spec §3.3): 1 vCPU / 2 GB / 8 GB / base / 120 s.
pub const DEFAULT_CPU: f64 = 1.0;
pub const DEFAULT_MEMORY_MB: u32 = 2048;
pub const DEFAULT_DISK_GB: u32 = 8;
pub const DEFAULT_AUTO_STOP_S: u32 = 120;

/// A validated resource shape. Construct only via [`Resources::validate`] on
/// the request path; `Deserialize` exists only for rehydrating trusted records
/// from the store.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Resources {
    pub cpu: f64,
    pub memory_mb: u32,
    pub disk_gb: u32,
}

impl Default for Resources {
    fn default() -> Self {
        Resources { cpu: DEFAULT_CPU, memory_mb: DEFAULT_MEMORY_MB, disk_gb: DEFAULT_DISK_GB }
    }
}

/// As received on the wire. All fields optional so `create()` with no body
/// yields the default cheap path.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResourcesRequest {
    pub cpu: Option<f64>,
    pub memory_mb: Option<u32>,
    pub disk_gb: Option<u32>,
}

fn approx_eq(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

impl Resources {
    pub fn validate(req: &ResourcesRequest) -> Result<Resources, String> {
        let cpu = req.cpu.unwrap_or(DEFAULT_CPU);
        let memory_mb = req.memory_mb.unwrap_or(DEFAULT_MEMORY_MB);
        let disk_gb = req.disk_gb.unwrap_or(DEFAULT_DISK_GB);

        if !ALLOWED_CPU.iter().any(|&v| approx_eq(v, cpu)) {
            return Err(format!(
                "cpu={cpu} is not allowed; choose one of {:?} shared vCPU",
                ALLOWED_CPU
            ));
        }
        if !ALLOWED_MEMORY_MB.contains(&memory_mb) {
            return Err(format!(
                "memory_mb={memory_mb} is not allowed; choose one of {:?} MB (1/2/4/8/16 GB)",
                ALLOWED_MEMORY_MB
            ));
        }
        if !ALLOWED_DISK_GB.contains(&disk_gb) {
            return Err(format!(
                "disk_gb={disk_gb} is not allowed; choose one of {:?} GB",
                ALLOWED_DISK_GB
            ));
        }
        Ok(Resources { cpu, memory_mb, disk_gb })
    }

    pub fn memory_gb(&self) -> f64 {
        self.memory_mb as f64 / 1024.0
    }

    /// Human label used in API responses, e.g. "2 shared vCPU".
    pub fn cpu_label(&self) -> String {
        if approx_eq(self.cpu, 0.5) {
            "0.5 shared vCPU".to_string()
        } else {
            format!("{} shared vCPU", self.cpu as u32)
        }
    }
}

/// Validate the auto-stop idle window.
pub fn validate_auto_stop(seconds: Option<u32>) -> Result<u32, String> {
    let s = seconds.unwrap_or(DEFAULT_AUTO_STOP_S);
    if !(AUTO_STOP_MIN_S..=AUTO_STOP_MAX_S).contains(&s) {
        return Err(format!(
            "auto_stop_seconds={s} out of range; must be {AUTO_STOP_MIN_S}..={AUTO_STOP_MAX_S}"
        ));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_cheap_path() {
        let r = Resources::validate(&ResourcesRequest::default()).unwrap();
        assert_eq!(r, Resources::default());
        assert_eq!(r.cpu, 1.0);
        assert_eq!(r.memory_mb, 2048);
        assert_eq!(r.disk_gb, 8);
    }

    #[test]
    fn rejects_arbitrary_memory() {
        let req = ResourcesRequest { memory_mb: Some(13 * 1024), ..Default::default() };
        assert!(Resources::validate(&req).is_err());
    }

    #[test]
    fn rejects_arbitrary_disk() {
        let req = ResourcesRequest { disk_gb: Some(250), ..Default::default() };
        assert!(Resources::validate(&req).is_err());
    }

    #[test]
    fn accepts_half_cpu() {
        let req = ResourcesRequest { cpu: Some(0.5), ..Default::default() };
        assert!(Resources::validate(&req).is_ok());
    }

    #[test]
    fn auto_stop_bounds() {
        assert!(validate_auto_stop(Some(29)).is_err());
        assert!(validate_auto_stop(Some(30)).is_ok());
        assert!(validate_auto_stop(Some(3600)).is_ok());
        assert!(validate_auto_stop(Some(3601)).is_err());
        assert_eq!(validate_auto_stop(None).unwrap(), 120);
    }
}
