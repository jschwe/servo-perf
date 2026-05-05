// tools/servoperf/src/cmd/regression.rs
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

use crate::cli::RegressionArgs;
use crate::cmd::bench;

/// Just enough of `RunResults` to pick out per-config summaries.
#[derive(Debug, Deserialize)]
struct BaselineRunResults {
    workload: BaselineWorkload,
    configs: BTreeMap<String, BaselineConfig>,
}

#[derive(Debug, Deserialize)]
struct BaselineWorkload { name: String }

#[derive(Debug, Deserialize)]
struct BaselineConfig {
    summary: BTreeMap<String, BaselineSummary>,
}

#[derive(Debug, Deserialize)]
struct BaselineSummary { p50: f64 }

pub fn run(args: RegressionArgs) -> Result<()> {
    // Step 1: do a bench-style run first.
    let bench_args = crate::cli::BenchArgs {
        workload: args.workload.clone(),
        bin: args.bin.clone(),
        iterations: args.iterations,
        out: args.out.clone(),
        ohos: args.ohos.clone(),
    };
    bench::run(bench_args)?;

    // Step 2: load the baseline and compute FCP p50 delta.
    let baseline: BaselineRunResults = {
        let text = std::fs::read_to_string(&args.baseline)
            .with_context(|| format!("reading baseline {}", args.baseline.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("parsing baseline {}", args.baseline.display()))?
    };
    anyhow::ensure!(
        baseline.workload.name == args.workload,
        "baseline at {} is for workload '{}', but this run is '{}'",
        args.baseline.display(), baseline.workload.name, args.workload,
    );

    // Step 3: re-read the raw.json we just wrote to compute delta against the baseline.
    let out_dir = find_latest_out(&args.workload, args.out.as_deref());
    let raw = out_dir.join("raw.json");
    let current: BaselineRunResults = serde_json::from_str(
        &std::fs::read_to_string(&raw).with_context(|| format!("reading {}", raw.display()))?,
    )?;

    let base_fcp = baseline
        .configs.get("main")
        .and_then(|c| c.summary.get("FirstContentfulPaint"))
        .map(|s| s.p50);
    let new_fcp = current
        .configs.get("main")
        .and_then(|c| c.summary.get("FirstContentfulPaint"))
        .map(|s| s.p50);
    let (Some(base_fcp), Some(new_fcp)) = (base_fcp, new_fcp) else {
        anyhow::bail!("missing FirstContentfulPaint summary in baseline or current run");
    };

    let delta_pct = if base_fcp.abs() < f64::EPSILON { 0.0 } else { 100.0 * (new_fcp - base_fcp) / base_fcp };

    eprintln!(
        "FirstContentfulPaint p50: baseline={:.1} ms, current={:.1} ms, Δ={:+.2}% (threshold {:+.1}%)",
        base_fcp, new_fcp, delta_pct, args.threshold
    );

    if delta_pct > args.threshold {
        std::process::exit(1);
    }
    Ok(())
}

fn find_latest_out(workload: &str, explicit: Option<&Path>) -> std::path::PathBuf {
    if let Some(p) = explicit { return p.to_path_buf(); }
    // Find the most recently created `out/<workload>-*` directory.
    let entries = std::fs::read_dir("out").ok();
    entries
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with(&format!("{}-", workload)))
                .unwrap_or(false)
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
        .unwrap_or_else(|| std::path::PathBuf::from("out").join(format!("{}-latest", workload)))
}
