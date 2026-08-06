// tools/servoperf/src/cmd/bench.rs
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::SystemTime;

use crate::cli::{BenchArgs, OhosArgs};
use crate::fixtures::{self, FixtureHandle};
use crate::instructions::{self, EngineConfig, InstructionsConfig};
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

    // Resolve which engine the bench is exercising, so we know which list of
    // function-name substrings to aggregate inclusive instruction counts
    // for. Only consulted when `--with-instructions` is on (the target is
    // OHOS). Engines whose `symbol_file` is non-empty get the merged ELF
    // auto-pushed if a sibling file is present in `workloads/`.
    let engine: Option<EngineConfig> = if args.ohos.with_instructions {
        match &target {
            Target::Ohos(ohos) => {
                let cfg = InstructionsConfig::load(&workloads_dir)?;
                match cfg.engine_for_bundle(&ohos.bundle) {
                    Some(e) => {
                        push_engine_symbols(ohos, e, &workloads_dir)?;
                        Some(e.clone())
                    }
                    None => {
                        eprintln!(
                            "warning: --with-instructions: no engine matches bundle {:?} in \
                             _instructions.toml; per-function counts will be skipped",
                            ohos.bundle
                        );
                        None
                    }
                }
            }
            _ => None,
        }
    } else {
        None
    };

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
            (Some(_), Target::Local { .. }) => (
                Some(fixtures::spawn(
                    &workloads_dir,
                    &w,
                    driver.as_ref(),
                    &out_dir,
                )?),
                None,
            ),
            (None, _) => (None, None),
        };
    let proxy_uri = fx
        .as_ref()
        .and_then(|h| h.proxy_uri().map(|s| s.to_string()));

    let mut iterations = Vec::with_capacity(w.iterations as usize);
    let mut fcp_samples: Vec<f64> = Vec::new();
    let mut lcp_samples: Vec<f64> = Vec::new();
    let mut successful_wall: Vec<std::time::Duration> = Vec::new();
    // Per-iteration handles for the background instruction-count
    // aggregation. Each handle runs while the next iteration is
    // recording on the device, so wallclock for the whole bench is
    // bounded by max(recording_time, analyser_time) per iteration instead
    // of their sum. Joined in a single pass after the iteration loop.
    let mut instr_jobs: Vec<(
        u32,
        JoinHandle<Result<std::collections::HashMap<String, u64>>>,
    )> = Vec::new();
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
                if let Some(m) = cp
                    .milestones
                    .iter()
                    .find(|m| m.name == "LargestContentfulPaint")
                {
                    metrics.insert("LargestContentfulPaint".to_string(), m.ts_ms);
                    lcp_samples.push(m.ts_ms);
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
                if let (Some(b), Some(a)) = (art.thermal_before_milli_c, art.thermal_after_milli_c)
                {
                    metrics.insert("soc_thermal_milli_c.delta".to_string(), (a - b) as f64);
                }
                // Presented-frame and page-reported metrics for scenario
                // workloads; a no-op for page-load ones.
                crate::fps::merge_scenario_metrics(
                    &mut metrics,
                    &w,
                    &art.fps_dumps,
                    art.log.as_deref(),
                    art.thread_cpu
                        .as_ref()
                        .map(|(b, a)| (b.as_str(), a.as_str())),
                );
                // Per-function inclusive instruction counts run on a
                // background thread so the next iteration's recording
                // starts immediately. Results are joined at the end of
                // the iteration loop and merged into this iteration's
                // metrics map.
                if let (Some(engine), Some(perf_data)) = (engine.as_ref(), art.perf_data.clone()) {
                    let engine = engine.clone();
                    let workloads_dir_owned = workloads_dir.clone();
                    let handle = std::thread::spawn(move || {
                        instructions::aggregate_inclusive_from_perf_data(
                            &perf_data,
                            &engine,
                            &workloads_dir_owned,
                        )
                    });
                    instr_jobs.push((i, handle));
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
                    status: IterationStatus::Failed {
                        error: format!("{err:#}"),
                    },
                });
            }
        }
    }

    let ok = iterations
        .iter()
        .filter(|i| matches!(i.status, IterationStatus::Ok { .. }))
        .count();
    anyhow::ensure!(
        2 * ok >= iterations.len(),
        "more than 50% of iterations failed ({}/{}); aborting",
        iterations.len() - ok,
        iterations.len()
    );

    // Drain background instruction-count jobs and merge each result into
    // the corresponding iteration's metrics map. A job's failure is
    // logged but doesn't fail the run — the timing metrics already
    // succeeded, and the user can re-run with `--with-instructions` if
    // they need the counts.
    for (idx, handle) in instr_jobs {
        let result = match handle.join() {
            Ok(r) => r,
            Err(_) => {
                eprintln!("iter {idx}: instruction aggregator panicked");
                continue;
            }
        };
        let totals = match result {
            Ok(t) => t,
            Err(e) => {
                eprintln!("iter {idx}: instruction aggregator failed: {e:#}");
                continue;
            }
        };
        if let Some(it) = iterations.iter_mut().find(|it| it.index == idx) {
            if let IterationStatus::Ok { metrics, .. } = &mut it.status {
                for (func, events) in totals {
                    metrics.insert(format!("instructions.{func}"), events as f64);
                }
            }
        }
    }

    let mut summary: BTreeMap<String, stats::Summary> = BTreeMap::new();
    if let Some(s) = stats::summarise(&fcp_samples) {
        summary.insert("FirstContentfulPaint".to_string(), s);
    }
    if let Some(s) = stats::summarise(&lcp_samples) {
        summary.insert("LargestContentfulPaint".to_string(), s);
    }
    // One summary entry per configured instruction symbol, across every
    // iteration that resolved it.
    if let Some(engine) = engine.as_ref() {
        for func in &engine.functions {
            let key = format!("instructions.{func}");
            let samples: Vec<f64> = iterations
                .iter()
                .filter_map(|i| match &i.status {
                    IterationStatus::Ok { metrics, .. } => metrics.get(&key).copied(),
                    _ => None,
                })
                .collect();
            if let Some(s) = stats::summarise(&samples) {
                summary.insert(key, s);
            }
        }
    }

    let mut configs: BTreeMap<String, ConfigResults> = BTreeMap::new();
    configs.insert(
        "main".into(),
        ConfigResults {
            bin: target.bin_label(),
            iterations,
            summary,
        },
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

/// If the engine declares a `symbol_file`, look for the file under
/// `workloads/` and (if found) push it to the device's symbol-dir so
/// hiperf's per-iteration `report -s --symbol-dir …` resolves library
/// symbols. Missing file emits a hint but isn't fatal — the bench still
/// captures perf.data, and the user can re-run with the file present.
fn push_engine_symbols(
    target: &OhosTarget,
    engine: &EngineConfig,
    workloads_dir: &Path,
) -> Result<()> {
    if engine.symbol_file.is_empty() {
        return Ok(());
    }
    let host_path = workloads_dir.join(&engine.symbol_file);
    if !host_path.exists() {
        eprintln!(
            "warning: engine {:?} symbol_file {:?} not found — symbols won't resolve. \
             Run: servoperf prepare-arkweb-symbols --input <stripped-libarkweb_engine.so> \
             --output {}",
            engine.id,
            host_path,
            host_path.display(),
        );
        return Ok(());
    }
    eprintln!(
        "ohos: pushing engine symbols ({} → device)",
        host_path.display()
    );
    target.push_arkweb_symbols(&host_path)
}

/// Build the [`Target`] for this run. For local mode, validates the bin
/// exists. For OHOS mode, runs an `hdc list targets` smoke test and
/// installs the .hap once if `--bin` is given.
pub(crate) fn build_target(ohos: &OhosArgs, bin: Option<&Path>) -> Result<Target> {
    if !ohos.ohos {
        let bin = bin.ok_or_else(|| anyhow::anyhow!("--bin is required (path to servoshell)"))?;
        return Ok(Target::Local {
            bin: bin.to_path_buf(),
        });
    }
    let mut target = OhosTarget::from_args(ohos);
    // Hydrate engine-specific proxy-arg templates from the global
    // _instructions.toml. Done here (not in OhosTarget::from_args) so
    // workloads_dir is reachable. Bench callers don't pay for this unless
    // a workload supplies a proxy URI, but resolving once at startup
    // keeps the per-iteration aa_start path branchless.
    let workloads_dir = workloads_dir();
    if let Ok(cfg) = InstructionsConfig::load(&workloads_dir) {
        if let Some(engine) = cfg.engine_for_bundle(&target.bundle) {
            target.engine_proxy_args = engine.proxy_args.clone();
        }
    }
    target.preflight()?;
    if let Some(hap) = bin {
        eprintln!("ohos: installing {} on device", hap.display());
        target.install_hap(hap)?;
        target.cooldown_after_install();
        target.warmup_launch()?;
    }
    Ok(Target::Ohos(target))
}

/// Pick the right parser based on which target produced the trace file.
pub(crate) fn parse_trace(target: &Target, path: &Path) -> Result<Vec<trace::Slice>> {
    match target {
        #[cfg(feature = "pftrace")]
        Target::Local { .. } => trace::parse(path),
        #[cfg(not(feature = "pftrace"))]
        Target::Local { .. } => anyhow::bail!(
            "local-target benchmarking decodes `.pftrace` files, which requires the \
             `pftrace` feature (and `protoc` at build time). This binary was built with \
             `--no-default-features`; rebuild with the `pftrace` feature, or use `--ohos`."
        ),
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
        Target::Local { bin } => Box::new(fixtures::LocalServoshellDriver { bin: bin.clone() }),
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
