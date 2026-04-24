// tools/servoperf/src/cmd/ab.rs
use anyhow::{Context, Result};
use rand::seq::SliceRandom;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::cli::AbArgs;
use crate::fixtures::{self, FixtureHandle};
use crate::report::{self, ConfigResults, Iteration, IterationStatus, RunResults};
use crate::runner;
use crate::stats;
use crate::trace;
use crate::workload::{self, Workload};

pub fn run(args: AbArgs) -> Result<()> {
    let workloads_dir = workloads_dir();
    let mut w = workload::load(&workloads_dir, &args.workload)?;
    if let Some(n) = args.iterations {
        w.iterations = n;
    }
    let registry = trace::load_registry(&workloads_dir)?;
    let out_dir = resolve_out(args.out.as_deref(), &w.name);
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    // For the (one-off) wpr-replay record pass we need a servoshell bin;
    // use `base` since the record is discarded (we only care about the
    // archive it produces).
    let fx: Option<FixtureHandle> = match w.fixture.as_ref() {
        Some(_) => Some(fixtures::spawn(&workloads_dir, &w, &args.base_bin, &out_dir)?),
        None => None,
    };
    let proxy_uri = fx.as_ref().and_then(|h| h.proxy_uri().map(|s| s.to_string()));

    // For each iteration, shuffle the order of (base, patch) so the pair
    // is symmetric against short-term system noise.
    let mut rng = rand::thread_rng();
    let mut base_iters: Vec<Iteration> = Vec::new();
    let mut patch_iters: Vec<Iteration> = Vec::new();
    let mut base_fcp: Vec<f64> = Vec::new();
    let mut patch_fcp: Vec<f64> = Vec::new();

    // Track successful iteration wallclock durations for adaptive timeout.
    // Shared across base + patch — both are running the same workload.
    let mut successful_wall: Vec<std::time::Duration> = Vec::new();
    for i in 0..w.iterations {
        let mut order = [("base", &args.base_bin), ("patch", &args.patch_bin)];
        order.shuffle(&mut rng);
        for (label, bin) in order {
            let iter_out = out_dir.join(format!("{label}_{i}_cwd"));
            std::fs::create_dir_all(&iter_out)?;
            let timeout = runner::pick_timeout(&successful_wall);
            let outcome =
                run_and_record(bin, &w, i, &iter_out, &registry, proxy_uri.as_deref(), timeout);
            if let Some(wall) = outcome.wall_duration {
                successful_wall.push(wall);
            }
            if let IterationStatus::Ok { ref metrics, .. } = outcome.iteration.status {
                if let Some(&v) = metrics.get("FirstContentfulPaint") {
                    match label {
                        "base" => base_fcp.push(v),
                        "patch" => patch_fcp.push(v),
                        _ => {}
                    }
                }
            }
            match label {
                "base" => base_iters.push(outcome.iteration),
                "patch" => patch_iters.push(outcome.iteration),
                _ => {}
            }
        }
    }

    // Abort if either config had too many failed iterations — the report
    // would otherwise be a misleading near-empty summary.
    for (label, iters) in [("base", &base_iters), ("patch", &patch_iters)] {
        let ok = iters.iter().filter(|i| matches!(i.status, IterationStatus::Ok { .. })).count();
        anyhow::ensure!(
            2 * ok >= iters.len(),
            "{label}: more than 50% of iterations failed ({}/{}); aborting",
            iters.len() - ok, iters.len()
        );
    }

    // Summaries + deltas.
    let base_summary = single_summary("FirstContentfulPaint", &base_fcp);
    let patch_summary = single_summary("FirstContentfulPaint", &patch_fcp);
    let mut deltas = BTreeMap::new();
    if let (Some(bs), Some(ps)) = (base_summary.get("FirstContentfulPaint"), patch_summary.get("FirstContentfulPaint")) {
        deltas.insert("FirstContentfulPaint".to_string(), stats::delta(bs, ps));
    }

    let mut configs: BTreeMap<String, ConfigResults> = BTreeMap::new();
    configs.insert(
        "base".into(),
        ConfigResults { bin: args.base_bin.clone(), iterations: base_iters, summary: base_summary },
    );
    configs.insert(
        "patch".into(),
        ConfigResults { bin: args.patch_bin.clone(), iterations: patch_iters, summary: patch_summary },
    );

    let data = RunResults {
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        timestamp_utc: format!("@{}s",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)),
        workload: w,
        configs,
        deltas,
    };
    report::write_json(&out_dir, &data)?;
    report::write_markdown(&out_dir, &data)?;
    println!("{}", out_dir.display());
    Ok(())
}

/// Iteration result plus its wallclock duration (if the run exited
/// cleanly). Duration is recorded even when parsing the pftrace fails,
/// since the goal is to keep the adaptive timeout grounded in real
/// servoshell-run times.
struct IterationOutcome {
    iteration: Iteration,
    wall_duration: Option<std::time::Duration>,
}

fn run_and_record(
    bin: &Path,
    w: &Workload,
    iter: u32,
    out_dir: &Path,
    registry: &trace::SpanRegistry,
    proxy_uri: Option<&str>,
    timeout: std::time::Duration,
) -> IterationOutcome {
    match runner::run_once(bin, w, iter, out_dir, proxy_uri, timeout) {
        Ok(art) => {
            let wall = std::time::Duration::from_nanos(
                art.exit_wall_ns.saturating_sub(art.spawn_wall_ns),
            );
            let iteration = match trace::parse(&art.pftrace) {
                Ok(slices) => {
                    let cp = trace::analyse(&slices, registry, art.spawn_wall_ns);
                    let pftrace = art.pftrace;
                    let mut metrics = BTreeMap::new();
                    if let Some(m) =
                        cp.milestones.iter().find(|m| m.name == "FirstContentfulPaint")
                    {
                        metrics.insert("FirstContentfulPaint".to_string(), m.ts_ms);
                    }
                    for row in &cp.named_spans {
                        metrics.insert(format!("{}.dur_ms", row.name), row.dur_ms);
                    }
                    Iteration {
                        index: iter,
                        status: IterationStatus::Ok { pftrace, metrics, critical_path: cp },
                    }
                }
                Err(err) => Iteration {
                    index: iter,
                    status: IterationStatus::Failed { error: format!("parse: {err:#}") },
                },
            };
            IterationOutcome { iteration, wall_duration: Some(wall) }
        }
        Err(err) => IterationOutcome {
            iteration: Iteration {
                index: iter,
                status: IterationStatus::Failed { error: format!("run: {err:#}") },
            },
            wall_duration: None,
        },
    }
}

fn single_summary(metric: &str, samples: &[f64]) -> BTreeMap<String, stats::Summary> {
    let mut m = BTreeMap::new();
    if let Some(s) = stats::summarise(samples) {
        m.insert(metric.to_string(), s);
    }
    m
}

fn workloads_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("workloads")
}

fn resolve_out(explicit: Option<&Path>, workload_name: &str) -> PathBuf {
    if let Some(p) = explicit { return p.to_path_buf(); }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    PathBuf::from("out").join(format!("{}-{}", workload_name, ts))
}
