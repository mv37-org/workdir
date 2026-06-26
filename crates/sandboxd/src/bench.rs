//! Phase 0 — the benchmark harness behind `GET /v1/benchmarks` (spec §21.3).
//!
//! Every later phase is judged against a baseline, so the baseline has to be
//! honest: the three boot paths are measured *separately* and never merged. The
//! harness drives the runtime directly with throwaway VMs — it does not create
//! billable sandbox records or touch the scheduler — so a sweep measures the
//! boot machinery, not the billing/quota path around it.
//!
//! Measured paths:
//! - `cold_boot` — fresh rootfs, no warm VM, no snapshot.
//! - `hot_pool` — claim a pre-booted warm VM (prewarm time excluded; it happens
//!   ahead of the request).
//! - `snapshot_restore` — boot, evict to standby (snapshot + free RAM), then
//!   `restore`. This is the perpetual-standby resume path, i.e. the exact number
//!   Phase 2 drives toward < 25ms.

use crate::catalog::classify;
use crate::ids;
use crate::knobs::Resources;
use crate::model::BootPath;
use crate::runtime::{ExecRequest, Runtime, VmSpec, WarmVm};
use crate::state::AppState;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;

/// One measured allocation along one boot path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkSample {
    pub id: String,
    pub image: String,
    pub boot_path: BootPath,
    /// Allocation request → guest agent answering (boot/restore ready).
    pub ready_ms: u64,
    /// Allocation request → first `echo` round-trip completes.
    pub create_to_echo_ms: u64,
    pub recorded_at: DateTime<Utc>,
}

fn bench_spec(image_key: &str, resources: Resources) -> VmSpec {
    VmSpec {
        sandbox_id: ids::sandbox_id(),
        org_id: "bench".to_string(),
        image_key: image_key.to_string(),
        image_ref: image_key.to_string(),
        resources,
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

/// The curated images a full sweep covers, each at its minimum shape (spec
/// §10.1). `base` first so the headline number is the cheapest path.
fn sweep_targets(image: &str) -> Vec<(String, Resources)> {
    let curated = ["base", "node-python", "browser", "heavy-build"];
    let names: Vec<&str> = if image == "all" {
        curated.to_vec()
    } else {
        vec![image]
    };
    names
        .into_iter()
        .filter_map(|name| {
            classify(name)
                .ok()
                .map(|c| (name.to_string(), c.minimum_resources()))
        })
        .collect()
}

async fn echo(rt: &Arc<dyn Runtime>, handle: &str) {
    let _ = rt
        .exec(
            handle,
            &ExecRequest {
                cmd: "echo ok".into(),
                cwd: None,
                env: Default::default(),
                background: false,
            },
        )
        .await;
}

fn sample(
    image: &str,
    boot_path: BootPath,
    ready: Instant,
    ready_ms: u64,
    echo_done: Instant,
    start: Instant,
) -> BenchmarkSample {
    let _ = ready;
    BenchmarkSample {
        id: ids::build_id(),
        image: image.to_string(),
        boot_path,
        ready_ms,
        create_to_echo_ms: echo_done.duration_since(start).as_millis() as u64,
        recorded_at: Utc::now(),
    }
}

/// Cold boot: pay the full boot (and, for custom images, the image-cache cost).
async fn measure_cold(
    rt: &Arc<dyn Runtime>,
    image: &str,
    res: Resources,
) -> Result<BenchmarkSample> {
    let spec = bench_spec(image, res);
    let start = Instant::now();
    let inst = rt.create(&spec, None, false).await?;
    // Always tear the throwaway VM down, even if a later step errors.
    let out = async {
        let ready = Instant::now();
        let ready_ms = (inst.boot_ms + inst.image_cache_ms)
            .max(ready.duration_since(start).as_millis() as u64);
        echo(rt, &inst.handle).await;
        let echo_done = Instant::now();
        Ok(sample(
            image,
            BootPath::ColdBoot,
            ready,
            ready_ms,
            echo_done,
            start,
        ))
    }
    .await;
    let _ = rt.delete(&inst.handle).await;
    out
}

/// Hot pool: claim a pre-booted warm VM. Prewarm happens ahead of the request,
/// so only the claim is on the measured path (the honest hot-pool number).
async fn measure_hot(
    rt: &Arc<dyn Runtime>,
    image: &str,
    res: Resources,
) -> Result<BenchmarkSample> {
    let spec = bench_spec(image, res);
    let warm: WarmVm = rt.prewarm(&spec).await?;
    let start = Instant::now();
    let inst = rt.create(&spec, Some(warm), false).await?;
    let out = async {
        let ready = Instant::now();
        let ready_ms = ready.duration_since(start).as_millis() as u64;
        echo(rt, &inst.handle).await;
        let echo_done = Instant::now();
        Ok(sample(
            image,
            BootPath::HotPool,
            ready,
            ready_ms,
            echo_done,
            start,
        ))
    }
    .await;
    let _ = rt.delete(&inst.handle).await;
    out
}

/// Snapshot restore: the perpetual-standby resume path. Boot a victim, evict it
/// to standby (snapshot + free RAM), then time `restore`. The throwaway VM is
/// always deleted, even if standby/restore errors (important when this runs as a
/// canary against a live node).
async fn measure_restore(
    rt: &Arc<dyn Runtime>,
    image: &str,
    res: Resources,
) -> Result<BenchmarkSample> {
    let spec = bench_spec(image, res);
    let inst = rt.create(&spec, None, false).await?;
    let out = async {
        rt.standby(&inst.handle).await?;
        let start = Instant::now();
        let restore_ms = rt.restore(&inst.handle).await?;
        let ready = Instant::now();
        let ready_ms = restore_ms.max(ready.duration_since(start).as_millis() as u64);
        echo(rt, &inst.handle).await;
        let echo_done = Instant::now();
        Ok(sample(
            image,
            BootPath::SnapshotRestore,
            ready,
            ready_ms,
            echo_done,
            start,
        ))
    }
    .await;
    let _ = rt.delete(&inst.handle).await;
    out
}

/// Run `iterations` measurements of each boot path against the local runtime,
/// persisting every sample. `image` is a curated name or `"all"` (every curated
/// image at its minimum shape). A target whose rootfs is not built on this node
/// is skipped and logged, never faked.
pub async fn run_sweep(state: &AppState, image: &str, iterations: u32) -> Vec<BenchmarkSample> {
    let rt = state.local.runtime();
    let mut samples = vec![];
    for (img, res) in sweep_targets(image) {
        // Skip images this node can't boot (e.g. rootfs not built yet) rather
        // than log a failure for every path/iteration.
        if !rt.image_available(&img) {
            tracing::info!(image = %img, "benchmark target skipped (image not built on this node)");
            continue;
        }
        for i in 0..iterations.max(1) {
            for (label, result) in [
                ("cold_boot", measure_cold(&rt, &img, res).await),
                ("hot_pool", measure_hot(&rt, &img, res).await),
                ("snapshot_restore", measure_restore(&rt, &img, res).await),
            ] {
                match result {
                    Ok(s) => {
                        if let Err(e) = state.store.put_benchmark_sample(&s) {
                            tracing::warn!(error = %e, "persist benchmark sample failed");
                        }
                        samples.push(s);
                    }
                    Err(e) => {
                        tracing::warn!(image = %img, path = label, iter = i, error = %e, "benchmark path skipped");
                    }
                }
            }
        }
    }
    samples
}

/// p-th percentile (nearest-rank) of a pre-sorted slice.
pub fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p / 100.0 * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_pick_expected_ranks() {
        // Nearest-rank: rank = round(p/100 * (n-1)). For 1..=100 that lands on
        // index 50/89/94 -> values 51/90/95.
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50.0), 51);
        assert_eq!(percentile(&v, 90.0), 90);
        assert_eq!(percentile(&v, 95.0), 95);
        assert_eq!(percentile(&v, 100.0), 100);
        assert_eq!(percentile(&v, 0.0), 1);
        assert_eq!(percentile(&[], 50.0), 0);
        assert_eq!(percentile(&[7], 99.0), 7);
    }
}
