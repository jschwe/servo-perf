// tools/servoperf/src/report.rs
//! JSON + Markdown writers for run results.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::stats::{Summary, SummaryDelta};
use crate::trace::CriticalPathReport;
use crate::workload::Workload;

#[derive(Debug, Serialize)]
pub enum IterationStatus {
    #[serde(rename = "ok")]
    Ok {
        pftrace: PathBuf,
        metrics: BTreeMap<String, f64>,
        critical_path: CriticalPathReport,
    },
    #[serde(rename = "failed")]
    Failed { error: String },
}

#[derive(Debug, Serialize)]
pub struct Iteration {
    pub index: u32,
    #[serde(flatten)]
    pub status: IterationStatus,
}

#[derive(Debug, Serialize)]
pub struct ConfigResults {
    pub bin: PathBuf,
    pub iterations: Vec<Iteration>,
    /// Metric name → summary. Computed from `iterations` filtered to status = ok.
    pub summary: BTreeMap<String, Summary>,
}

#[derive(Debug, Serialize)]
pub struct RunResults {
    pub tool_version: String,
    pub timestamp_utc: String,
    pub workload: Workload,
    pub configs: BTreeMap<String, ConfigResults>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub deltas: BTreeMap<String, SummaryDelta>,
}

pub fn write_json(out_dir: &Path, data: &RunResults) -> Result<()> {
    let path = out_dir.join("raw.json");
    let file = std::fs::File::create(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    serde_json::to_writer_pretty(file, data)
        .with_context(|| format!("writing JSON to {}", path.display()))?;
    Ok(())
}

pub fn write_markdown(out_dir: &Path, data: &RunResults) -> Result<()> {
    let path = out_dir.join("report.md");
    let md = render_markdown(data);
    std::fs::write(&path, md).with_context(|| format!("writing Markdown to {}", path.display()))?;
    Ok(())
}

fn render_markdown(data: &RunResults) -> String {
    let mut s = String::new();
    writeln!(
        s,
        "# servoperf — `{}`\n\nRun at {}, tool v{}. URL: `{}`. Iterations requested: {}.\n",
        data.workload.name,
        data.timestamp_utc,
        data.tool_version,
        data.workload.url,
        data.workload.iterations,
    )
    .unwrap();

    for (name, cfg) in &data.configs {
        writeln!(s, "## Config `{}`  (`{}`)\n", name, cfg.bin.display()).unwrap();
        let ok = cfg
            .iterations
            .iter()
            .filter(|i| matches!(i.status, IterationStatus::Ok { .. }))
            .count();
        let failed = cfg.iterations.len() - ok;
        writeln!(s, "Iterations: {} ok, {} failed.\n", ok, failed).unwrap();
        writeln!(s, "| metric | n | min | p25 | p50 | mean | p75 | p90 | max |").unwrap();
        writeln!(s, "|---|---:|---:|---:|---:|---:|---:|---:|---:|").unwrap();
        for (metric, sum) in &cfg.summary {
            writeln!(
                s,
                "| {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
                metric, sum.n, sum.min, sum.p25, sum.p50, sum.mean, sum.p75, sum.p90, sum.max
            )
            .unwrap();
        }
        writeln!(s).unwrap();
    }

    if !data.deltas.is_empty() {
        writeln!(s, "## Deltas (patch vs base, p50)\n").unwrap();
        writeln!(s, "| metric | Δ abs (ms) | Δ % |").unwrap();
        writeln!(s, "|---|---:|---:|").unwrap();
        for (m, d) in &data.deltas {
            writeln!(s, "| {} | {:+.1} | {:+.1}% |", m, d.abs_ms, d.pct).unwrap();
        }
        writeln!(s).unwrap();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::summarise;
    use crate::trace::CriticalPathReport;
    use crate::workload::Workload;

    fn dummy_workload() -> Workload {
        Workload {
            name: "test".into(),
            url: "https://x.test/".into(),
            tracing_filter: "info".into(),
            iterations: 3,
            user_agent: None,
            viewport: None,
            device_pixel_ratio: None,
            servoshell_args: vec![],
            fixture: None,
        }
    }

    #[test]
    fn markdown_has_expected_headings() {
        let mut iters = vec![];
        for (i, fcp) in [210.0, 230.0, 250.0].into_iter().enumerate() {
            let mut metrics = BTreeMap::new();
            metrics.insert("FirstContentfulPaint".to_string(), fcp);
            iters.push(Iteration {
                index: i as u32,
                status: IterationStatus::Ok {
                    pftrace: PathBuf::from(format!("iter_{i}.pftrace")),
                    metrics,
                    critical_path: CriticalPathReport::default(),
                },
            });
        }
        let mut summary = BTreeMap::new();
        summary.insert(
            "FirstContentfulPaint".into(),
            summarise(&[210.0, 230.0, 250.0]).unwrap(),
        );

        let mut configs = BTreeMap::new();
        configs.insert(
            "main".into(),
            ConfigResults {
                bin: PathBuf::from("/tmp/bin"),
                iterations: iters,
                summary,
            },
        );

        let data = RunResults {
            tool_version: "0.1.0".into(),
            timestamp_utc: "2026-04-22T12:00:00Z".into(),
            workload: dummy_workload(),
            configs,
            deltas: BTreeMap::new(),
        };
        let md = super::render_markdown(&data);
        assert!(md.contains("# servoperf — `test`"));
        assert!(md.contains("## Config `main`"));
        assert!(md.contains("FirstContentfulPaint"));
        assert!(md.contains("| 3 |"));
    }
}
