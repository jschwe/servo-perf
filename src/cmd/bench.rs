// tools/servoperf/src/cmd/bench.rs
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::cli::BenchArgs;
use crate::fixtures::{self, FixtureHandle};
use crate::report::{self, ConfigResults, Iteration, IterationStatus, RunResults};
use crate::runner;
use crate::stats;
use crate::trace;
use crate::workload;

pub fn run(args: BenchArgs) -> Result<()> {
    let workloads_dir = workloads_dir();
    let mut w = workload::load(&workloads_dir, &args.workload)?;
    if let Some(n) = args.iterations {
        w.iterations = n;
    }
    let registry = trace::load_registry(&workloads_dir)?;
    let out_dir = resolve_out(args.out.as_deref(), &w.name);
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    let fx: Option<FixtureHandle> = match w.fixture.as_ref() {
        Some(_) => Some(fixtures::spawn(&workloads_dir, &w, &args.bin, &out_dir)?),
        None => None,
    };
    let proxy_uri = fx.as_ref().and_then(|h| h.proxy_uri().map(|s| s.to_string()));

    let mut iterations = Vec::with_capacity(w.iterations as usize);
    let mut fcp_samples: Vec<f64> = Vec::new();
    let mut successful_wall: Vec<std::time::Duration> = Vec::new();
    for i in 0..w.iterations {
        let timeout = runner::pick_timeout(&successful_wall);
        match runner::run_once(&args.bin, &w, i, &out_dir, proxy_uri.as_deref(), timeout) {
            Ok(art) => {
                let wall = std::time::Duration::from_nanos(
                    art.exit_wall_ns.saturating_sub(art.spawn_wall_ns),
                );
                successful_wall.push(wall);
                let pftrace = art.pftrace;
                let slices = trace::parse(&pftrace)?;
                let cp = trace::analyse(&slices, &registry, art.spawn_wall_ns);
                let mut metrics = BTreeMap::new();
                if let Some(m) = cp.milestones.iter().find(|m| m.name == "FirstContentfulPaint") {
                    metrics.insert("FirstContentfulPaint".to_string(), m.ts_ms);
                    fcp_samples.push(m.ts_ms);
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

    let mut configs: BTreeMap<String, ConfigResults> = BTreeMap::new();
    configs.insert(
        "main".into(),
        ConfigResults { bin: args.bin.clone(), iterations, summary },
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
