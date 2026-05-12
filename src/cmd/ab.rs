// tools/servoperf/src/cmd/ab.rs
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::cli::AbArgs;
use crate::cmd::bench::parse_trace;
use crate::fixtures::{self, FixtureHandle};
use crate::ohos::OhosTarget;
use crate::report::{self, ConfigResults, Iteration, IterationStatus, RunResults};
use crate::runner::{self, Target};
use crate::stats;
use crate::trace;
use crate::workload::{self, Workload};

pub fn run(args: AbArgs) -> Result<()> {
    let workloads_dir = workloads_dir();
    let mut w = workload::load(&workloads_dir, &args.workload)?;
    if let Some(n) = args.iterations {
        w.iterations = n;
    }
    let (base_target, patch_target) = build_ab_targets(&args)?;
    // Both sides share the same target kind so the registry is determined
    // by `base_target` alone.
    let registry = trace::load_registry_named(&workloads_dir, base_target.registry_stem())?;
    let primary_milestone = base_target.primary_milestone();
    let out_dir = resolve_out(args.out.as_deref(), &w.name);
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    // Bump the device's hitrace threshold for the duration of the run so
    // TRACE-level Servo spans land in the trace. Same semantics as in
    // `cmd/bench.rs`; one guard covers both phases since they share the
    // device.
    let _trace_level_guard = match &base_target {
        Target::Ohos(ohos) => Some(ohos.guard_trace_level(&ohos.trace_level.clone())?),
        Target::Local { .. } => None,
    };
    let driver = crate::cmd::bench::build_record_driver(&base_target, args.ohos.ohos_record_seconds);
    let (fx, _rport): (Option<FixtureHandle>, Option<crate::ohos::RPortGuard>) =
        match (w.fixture.as_ref(), &base_target) {
            (Some(fx_def), Target::Ohos(ohos)) => {
                let handle = fixtures::spawn(&workloads_dir, &w, driver.as_ref(), &out_dir)?;
                let guard = ohos.setup_rport(&fx_def.ports_to_forward())?;
                (Some(handle), Some(guard))
            }
            (Some(_), Target::Local { .. }) => {
                (Some(fixtures::spawn(&workloads_dir, &w, driver.as_ref(), &out_dir)?), None)
            }
            (None, _) => (None, None),
        };
    let proxy_uri = fx.as_ref().and_then(|h| h.proxy_uri().map(|s| s.to_string()));

    // Track successful iteration wallclock durations for adaptive timeout,
    // shared across both phases — both run the same workload.
    let mut successful_wall: Vec<std::time::Duration> = Vec::new();

    // Phase 1: install base hap (OHOS only — local has nothing to install)
    // and run all base iterations.
    if let Target::Ohos(ohos) = &base_target {
        eprintln!("ohos: installing {} on device (base phase)", args.base_bin.display());
        ohos.install_hap(&args.base_bin)?;
        ohos.cooldown_after_install();
        ohos.warmup_launch()?;
    }
    let (base_iters, base_fcp, base_lcp) = run_phase(
        "base",
        &base_target,
        &w,
        &out_dir,
        &registry,
        primary_milestone,
        proxy_uri.as_deref(),
        &mut successful_wall,
    );

    // Phase 2: re-install with the patch hap (overwriting base), then run
    // all patch iterations.
    if let Target::Ohos(ohos) = &patch_target {
        eprintln!("ohos: installing {} on device (patch phase)", args.patch_bin.display());
        ohos.install_hap(&args.patch_bin)?;
        ohos.cooldown_after_install();
        ohos.warmup_launch()?;
    }
    let (patch_iters, patch_fcp, patch_lcp) = run_phase(
        "patch",
        &patch_target,
        &w,
        &out_dir,
        &registry,
        primary_milestone,
        proxy_uri.as_deref(),
        &mut successful_wall,
    );

    // Abort if either phase had too many failed iterations — the report
    // would otherwise be a misleading near-empty summary.
    for (label, iters) in [("base", &base_iters), ("patch", &patch_iters)] {
        let ok = iters.iter().filter(|i| matches!(i.status, IterationStatus::Ok { .. })).count();
        anyhow::ensure!(
            2 * ok >= iters.len(),
            "{label}: more than 50% of iterations failed ({}/{}); aborting",
            iters.len() - ok, iters.len()
        );
    }

    // Summaries + deltas. Each metric tracked separately so that pages
    // without LCP only get an FCP delta (rather than the entire run
    // failing for missing LCP).
    let mut base_summary = single_summary("FirstContentfulPaint", &base_fcp);
    let mut patch_summary = single_summary("FirstContentfulPaint", &patch_fcp);
    if let Some(s) = stats::summarise(&base_lcp) {
        base_summary.insert("LargestContentfulPaint".to_string(), s);
    }
    if let Some(s) = stats::summarise(&patch_lcp) {
        patch_summary.insert("LargestContentfulPaint".to_string(), s);
    }
    let mut deltas = BTreeMap::new();
    for metric in ["FirstContentfulPaint", "LargestContentfulPaint"] {
        if let (Some(bs), Some(ps)) = (base_summary.get(metric), patch_summary.get(metric)) {
            deltas.insert(metric.to_string(), stats::delta(bs, ps));
        }
    }

    let mut configs: BTreeMap<String, ConfigResults> = BTreeMap::new();
    configs.insert(
        "base".into(),
        ConfigResults {
            bin: bin_label(&base_target, &args.base_bin),
            iterations: base_iters,
            summary: base_summary,
        },
    );
    configs.insert(
        "patch".into(),
        ConfigResults {
            bin: bin_label(&patch_target, &args.patch_bin),
            iterations: patch_iters,
            summary: patch_summary,
        },
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

/// Drive `w.iterations` iterations of `target`, collecting `Iteration`
/// records plus FCP/LCP samples for the summary. `successful_wall` is
/// extended in place so the adaptive timeout stays grounded as the run
/// progresses across both phases.
fn run_phase(
    label: &str,
    target: &Target,
    w: &Workload,
    out_dir: &Path,
    registry: &trace::SpanRegistry,
    primary_milestone: &str,
    proxy_uri: Option<&str>,
    successful_wall: &mut Vec<std::time::Duration>,
) -> (Vec<Iteration>, Vec<f64>, Vec<f64>) {
    let mut iters = Vec::with_capacity(w.iterations as usize);
    let mut fcp = Vec::new();
    let mut lcp = Vec::new();
    for i in 0..w.iterations {
        let iter_out = out_dir.join(format!("{label}_{i}_cwd"));
        let _ = std::fs::create_dir_all(&iter_out);
        let timeout = runner::pick_timeout(successful_wall);
        let outcome = run_and_record(
            target, w, i, &iter_out, registry, primary_milestone, proxy_uri, timeout,
        );
        if let Some(wall) = outcome.wall_duration {
            successful_wall.push(wall);
        }
        if let IterationStatus::Ok { ref metrics, .. } = outcome.iteration.status {
            if let Some(&v) = metrics.get("FirstContentfulPaint") {
                fcp.push(v);
            }
            // LCP is optional: pages without a large enough text / image
            // fragment never fire it. The summary's `n` therefore can
            // drop below the iteration count, which is the right shape —
            // it makes "LCP missing on N iterations" visible in the report.
            if let Some(&v) = metrics.get("LargestContentfulPaint") {
                lcp.push(v);
            }
        }
        iters.push(outcome.iteration);
    }
    (iters, fcp, lcp)
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
    target: &Target,
    w: &Workload,
    iter: u32,
    out_dir: &Path,
    registry: &trace::SpanRegistry,
    primary_milestone: &str,
    proxy_uri: Option<&str>,
    timeout: std::time::Duration,
) -> IterationOutcome {
    match runner::run_once(target, w, iter, out_dir, proxy_uri, timeout) {
        Ok(art) => {
            let wall = std::time::Duration::from_nanos(
                art.exit_wall_ns.saturating_sub(art.spawn_wall_ns),
            );
            let iteration = match parse_trace(target, &art.pftrace) {
                Ok(slices) => {
                    let cp = trace::analyse(&slices, registry, art.spawn_wall_ns);
                    let pftrace = art.pftrace;
                    let mut metrics = BTreeMap::new();
                    // See bench.rs: the metric is keyed
                    // `FirstContentfulPaint` regardless of which span
                    // sourced it (proxy on OHOS).
                    if let Some(m) = cp.milestones.iter().find(|m| m.name == primary_milestone) {
                        metrics.insert("FirstContentfulPaint".to_string(), m.ts_ms);
                    }
                    if let Some(m) = cp.milestones.iter().find(|m| m.name == "LargestContentfulPaint") {
                        metrics.insert("LargestContentfulPaint".to_string(), m.ts_ms);
                    }
                    for row in &cp.named_spans {
                        metrics.insert(format!("{}.dur_ms", row.name), row.dur_ms);
                    }
                    // Thermal snapshots (OHOS only). Absent on local targets.
                    if let Some(v) = art.thermal_before_milli_c {
                        metrics.insert("soc_thermal_milli_c.before".to_string(), v as f64);
                    }
                    if let Some(v) = art.thermal_after_milli_c {
                        metrics.insert("soc_thermal_milli_c.after".to_string(), v as f64);
                    }
                    if let (Some(b), Some(a)) =
                        (art.thermal_before_milli_c, art.thermal_after_milli_c)
                    {
                        metrics.insert(
                            "soc_thermal_milli_c.delta".to_string(),
                            (a - b) as f64,
                        );
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

/// Construct the (base, patch) targets. Both sides of the AB share the
/// same `--ohos-bundle` on OHOS; the haps are installed phase-by-phase
/// (see `run`) rather than side-by-side. On local targets the two
/// binaries are independent paths on disk.
fn build_ab_targets(args: &AbArgs) -> Result<(Target, Target)> {
    if !args.ohos.ohos {
        return Ok((
            Target::Local { bin: args.base_bin.clone() },
            Target::Local { bin: args.patch_bin.clone() },
        ));
    }
    let base = OhosTarget::from_args(&args.ohos);
    let patch = OhosTarget::from_args(&args.ohos);
    base.preflight()?;
    Ok((Target::Ohos(base), Target::Ohos(patch)))
}

/// Surface the .hap path (OHOS) or binary path (local) as the bin label
/// in the report. For OHOS both phases share a bundle, so the bundle URL
/// alone wouldn't distinguish them.
fn bin_label(target: &Target, hap_or_bin: &Path) -> PathBuf {
    match target {
        Target::Local { .. } => target.bin_label(),
        Target::Ohos(_) => hap_or_bin.to_path_buf(),
    }
}

fn resolve_out(explicit: Option<&Path>, workload_name: &str) -> PathBuf {
    if let Some(p) = explicit { return p.to_path_buf(); }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    PathBuf::from("out").join(format!("{}-{}", workload_name, ts))
}
