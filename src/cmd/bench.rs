// tools/servoperf/src/cmd/bench.rs
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::cli::{BenchArgs, OhosArgs};
use crate::fixtures::{self, FixtureHandle};
use crate::ohos::{self, OhosTarget};
use crate::report::{self, ConfigResults, Iteration, IterationStatus, RunResults};
use crate::runner::{self, Target};
use crate::stats;
use crate::trace;
use crate::workload;

pub fn run(args: BenchArgs) -> Result<()> {
    let workloads_dir = workloads_dir();
    let mut w = workload::load(&workloads_dir, &args.workload)?;
    if let Some(n) = args.iterations {
        w.iterations = n;
    }
    let target = build_target(&args.ohos, args.bin.as_deref())?;
    let registry = trace::load_registry_named(&workloads_dir, target.registry_stem())?;
    let primary_milestone = target.primary_milestone();
    let out_dir = resolve_out(args.out.as_deref(), &w.name);
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    // Local fixtures (http1/h2 servers, wpr) live on the host's
    // 127.0.0.1; on OHOS we need `hdc rport` so the device can reach
    // them. The reverse-port guard is held in scope alongside `fx` and
    // tears down the forwards on Drop.
    // On OHOS, the system-wide `persist.hitrace.level.threshold`
    // gates which trace-level events reach the dump. Snapshot and
    // bump it for the duration of the run; the guard restores on
    // Drop. Held alongside `fx` / `_rport` so scope-exit ordering
    // is: tear down rport → stop fixture → restore trace level.
    let _trace_level_guard = match &target {
        Target::Ohos(ohos) => Some(ohos.guard_trace_level(&ohos.trace_level.clone())?),
        Target::Local { .. } => None,
    };
    let driver = build_record_driver(&target, args.ohos.ohos_record_seconds);
    let (fx, _rport): (Option<FixtureHandle>, Option<crate::ohos::RPortGuard>) =
        match (w.fixture.as_ref(), &target) {
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

    let mut iterations = Vec::with_capacity(w.iterations as usize);
    let mut fcp_samples: Vec<f64> = Vec::new();
    let mut lcp_samples: Vec<f64> = Vec::new();
    let mut successful_wall: Vec<std::time::Duration> = Vec::new();
    for i in 0..w.iterations {
        let timeout = runner::pick_timeout(&successful_wall);
        match runner::run_once(&target, &w, i, &out_dir, proxy_uri.as_deref(), timeout) {
            Ok(art) => {
                let wall = std::time::Duration::from_nanos(
                    art.exit_wall_ns.saturating_sub(art.spawn_wall_ns),
                );
                successful_wall.push(wall);
                let pftrace = art.pftrace;
                let slices = parse_trace(&target, &pftrace)?;
                let cp = trace::analyse(&slices, &registry, art.spawn_wall_ns);
                let mut metrics = BTreeMap::new();
                // The "FirstContentfulPaint" key in the metrics map is
                // a *role*, not a literal span name: on desktop it's
                // sourced from the FCP instant; on OHOS it's sourced
                // from the closest available proxy milestone (see
                // `Target::primary_milestone`). Storing under one key
                // keeps regression baselines and report schemas
                // consistent across targets.
                if let Some(m) = cp.milestones.iter().find(|m| m.name == primary_milestone) {
                    metrics.insert("FirstContentfulPaint".to_string(), m.ts_ms);
                    fcp_samples.push(m.ts_ms);
                }
                // LCP is independent of the per-target "primary
                // milestone" alias: it always reports the time of the
                // last largest-contentful-paint instant, which servo
                // emits via tracing only when the
                // `largest_contentful_paint_enabled` pref is on
                // (servoperf forces it on — see `runner::run_once`
                // and `ohos::workload_args_to_aa_params`). When the
                // page never triggers an LCP (no large enough text
                // or image fragment), the metric is absent for that
                // iteration; that's recorded as a missing sample so
                // the summary's `n` reflects reality.
                if let Some(m) = cp.milestones.iter().find(|m| m.name == "LargestContentfulPaint") {
                    metrics.insert("LargestContentfulPaint".to_string(), m.ts_ms);
                    lcp_samples.push(m.ts_ms);
                }
                for row in &cp.named_spans {
                    metrics.insert(format!("{}.dur_ms", row.name), row.dur_ms);
                }
                iterations.push(Iteration {
                    index: i,
                    status: IterationStatus::Ok {
                        pftrace,
                        metrics,
                        critical_path: cp,
                    },
                });
            }
            Err(err) => {
                eprintln!("iter {i} failed: {err:#}");
                iterations.push(Iteration {
                    index: i,
                    status: IterationStatus::Failed { error: format!("{err:#}") },
                });
            }
        }
    }

    let ok = iterations.iter().filter(|i| matches!(i.status, IterationStatus::Ok { .. })).count();
    anyhow::ensure!(
        2 * ok >= iterations.len(),
        "more than 50% of iterations failed ({}/{}); aborting",
        iterations.len() - ok, iterations.len()
    );

    let mut summary: BTreeMap<String, stats::Summary> = BTreeMap::new();
    if let Some(s) = stats::summarise(&fcp_samples) {
        summary.insert("FirstContentfulPaint".to_string(), s);
    }
    if let Some(s) = stats::summarise(&lcp_samples) {
        summary.insert("LargestContentfulPaint".to_string(), s);
    }

    let mut configs: BTreeMap<String, ConfigResults> = BTreeMap::new();
    configs.insert(
        "main".into(),
        ConfigResults { bin: target.bin_label(), iterations, summary },
    );

    let data = RunResults {
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        timestamp_utc: now_rfc3339(),
        workload: w,
        configs,
        deltas: BTreeMap::new(),
    };

    report::write_json(&out_dir, &data)?;
    report::write_markdown(&out_dir, &data)?;
    println!("{}", out_dir.display());
    Ok(())
}

fn workloads_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("workloads")
}

/// Build the [`Target`] for this run. For local mode, validates the bin
/// exists. For OHOS mode, runs an `hdc list targets` smoke test and
/// installs the .hap once if `--bin` is given.
pub(crate) fn build_target(ohos: &OhosArgs, bin: Option<&Path>) -> Result<Target> {
    if !ohos.ohos {
        let bin = bin
            .ok_or_else(|| anyhow::anyhow!("--bin is required (path to servoshell)"))?;
        return Ok(Target::Local { bin: bin.to_path_buf() });
    }
    let target = OhosTarget::from_args(ohos);
    target.preflight()?;
    if let Some(hap) = bin {
        eprintln!("ohos: installing {} on device", hap.display());
        target.install_hap(hap)?;
    }
    Ok(Target::Ohos(target))
}

/// Pick the right parser based on which target produced the trace file.
pub(crate) fn parse_trace(target: &Target, path: &Path) -> Result<Vec<trace::Slice>> {
    match target {
        Target::Local { .. } => trace::parse(path),
        Target::Ohos(_) => ohos::parse_hitrace_file(path),
    }
}

/// Build the [`fixtures::RecordDriver`] matching the target.
///
/// Only consulted when a workload uses `wpr-replay` *and* the archive
/// is missing — `fixtures::spawn` then drives a one-shot record pass
/// through the chosen driver before flipping into replay mode for the
/// iteration loop.
pub(crate) fn build_record_driver(
    target: &Target,
    ohos_record_seconds: u64,
) -> Box<dyn fixtures::RecordDriver> {
    match target {
        Target::Local { bin } => {
            Box::new(fixtures::LocalServoshellDriver { bin: bin.clone() })
        }
        Target::Ohos(ohos) => Box::new(crate::ohos::OhosRecordDriver {
            target: ohos.clone(),
            record_seconds: ohos_record_seconds,
        }),
    }
}

fn resolve_out(explicit: Option<&Path>, workload_name: &str) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    PathBuf::from("out").join(format!("{}-{}", workload_name, ts))
}

fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    format!("@{}s", secs)
}
