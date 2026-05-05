// tools/servoperf/src/cmd/ab.rs
use anyhow::{Context, Result};
use rand::seq::SliceRandom;
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
    // Build the two configs (base/patch) into Target values up front so
    // any preflight / install errors fail before iterations run.
    let (base_target, patch_target) = build_ab_targets(&args)?;
    // Both sides of an A/B share the same target kind, so the registry
    // is determined by `base_target` alone.
    let registry = trace::load_registry_named(&workloads_dir, base_target.registry_stem())?;
    let primary_milestone = base_target.primary_milestone();
    let out_dir = resolve_out(args.out.as_deref(), &w.name);
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    // Local fixtures (http1/h2 servers, wpr) only make sense for local
    // targets unless we set up `hdc rport` to bridge them. See bench.rs
    // for the matching logic.
    //
    // For A/B both targets share the same target *kind* (Local-vs-OHOS)
    // and the same workload, so it's fine to build the record driver
    // off `base_target` and reuse it for any auto-record pass.
    // Bump the device's hitrace threshold for the duration of the
    // pair-run so TRACE-level Servo spans land in the trace. Same
    // semantics as in `cmd/bench.rs`; only one guard is needed since
    // the two OHOS bundles share a device. See `OhosArgs::ohos_trace_level`.
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

    // For each iteration, shuffle the order of (base, patch) so the pair
    // is symmetric against short-term system noise.
    let mut rng = rand::thread_rng();
    let mut base_iters: Vec<Iteration> = Vec::new();
    let mut patch_iters: Vec<Iteration> = Vec::new();
    let mut base_fcp: Vec<f64> = Vec::new();
    let mut patch_fcp: Vec<f64> = Vec::new();
    let mut base_lcp: Vec<f64> = Vec::new();
    let mut patch_lcp: Vec<f64> = Vec::new();

    // Track successful iteration wallclock durations for adaptive timeout.
    // Shared across base + patch — both are running the same workload.
    let mut successful_wall: Vec<std::time::Duration> = Vec::new();
    for i in 0..w.iterations {
        let mut order = [("base", &base_target), ("patch", &patch_target)];
        order.shuffle(&mut rng);
        for (label, target) in order {
            let iter_out = out_dir.join(format!("{label}_{i}_cwd"));
            std::fs::create_dir_all(&iter_out)?;
            let timeout = runner::pick_timeout(&successful_wall);
            let outcome = run_and_record(
                target,
                &w,
                i,
                &iter_out,
                &registry,
                primary_milestone,
                proxy_uri.as_deref(),
                timeout,
            );
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
                // LCP is optional: pages without a large enough text /
                // image fragment never fire it. The summary's `n`
                // therefore can drop below the iteration count, which
                // is the right shape — it makes "LCP missing on N
                // iterations" visible in the report.
                if let Some(&v) = metrics.get("LargestContentfulPaint") {
                    match label {
                        "base" => base_lcp.push(v),
                        "patch" => patch_lcp.push(v),
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

    // Summaries + deltas. Each metric tracked separately so that
    // pages without LCP only get an FCP delta (rather than the entire
    // run failing for missing LCP).
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
        ConfigResults { bin: base_target.bin_label(), iterations: base_iters, summary: base_summary },
    );
    configs.insert(
        "patch".into(),
        ConfigResults { bin: patch_target.bin_label(), iterations: patch_iters, summary: patch_summary },
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

/// Construct the (base, patch) targets. In OHOS mode the two configs
/// can either:
///   * share one bundle name and re-install before each iteration —
///     impractical for AB since `hdc install` takes seconds to minutes;
///   * use distinct bundle names (one per .hap), via the
///     `--base-ohos-bundle` / `--patch-ohos-bundle` overrides. The user
///     installs both haps once up front.
///
/// We default to the second model and *refuse* OHOS AB without distinct
/// bundles to make the failure mode loud, since silently re-installing
/// every iteration would tank throughput and noise out the comparison.
fn build_ab_targets(args: &AbArgs) -> Result<(Target, Target)> {
    if !args.ohos.ohos {
        let base_bin = args
            .base_bin
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("--base-bin is required (path to servoshell)"))?;
        let patch_bin = args
            .patch_bin
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("--patch-bin is required (path to servoshell)"))?;
        return Ok((
            Target::Local { bin: base_bin.clone() },
            Target::Local { bin: patch_bin.clone() },
        ));
    }
    let base_bundle = args
        .base_ohos_bundle
        .clone()
        .unwrap_or_else(|| args.ohos.ohos_bundle.clone());
    let patch_bundle = args.patch_ohos_bundle.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "OHOS AB needs `--patch-ohos-bundle` (and optionally \
             `--base-ohos-bundle`) so each .hap can be installed under a \
             distinct bundle name. Re-installing on every iteration would \
             dominate the measurement, so it's not done implicitly."
        )
    })?;
    anyhow::ensure!(
        base_bundle != patch_bundle,
        "OHOS AB: base and patch bundles must differ (got '{base_bundle}' for both)"
    );

    let base = OhosTarget::from_args(&args.ohos).with_bundle(base_bundle);
    let patch = OhosTarget::from_args(&args.ohos).with_bundle(patch_bundle);
    base.preflight()?;

    if let Some(hap) = args.base_bin.as_deref() {
        eprintln!("ohos: installing {} as bundle {}", hap.display(), base.bundle);
        base.install_hap(hap)?;
    }
    if let Some(hap) = args.patch_bin.as_deref() {
        eprintln!("ohos: installing {} as bundle {}", hap.display(), patch.bundle);
        patch.install_hap(hap)?;
    }

    Ok((Target::Ohos(base), Target::Ohos(patch)))
}

fn resolve_out(explicit: Option<&Path>, workload_name: &str) -> PathBuf {
    if let Some(p) = explicit { return p.to_path_buf(); }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    PathBuf::from("out").join(format!("{}-{}", workload_name, ts))
}
