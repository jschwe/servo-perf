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

/// Return the iteration whose FCP is closest to the median FCP for a config,
/// or `None` if no iterations succeeded.
fn repr_iteration(cfg: &ConfigResults) -> Option<&Iteration> {
    let mut fcp_values: Vec<f64> = cfg
        .iterations
        .iter()
        .filter_map(|i| {
            if let IterationStatus::Ok { ref metrics, .. } = i.status {
                metrics.get("FirstContentfulPaint").copied()
            } else {
                None
            }
        })
        .collect();
    if fcp_values.is_empty() {
        return None;
    }
    fcp_values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = fcp_values[fcp_values.len() / 2];
    cfg.iterations.iter().min_by(|a, b| {
        let fcp_a = if let IterationStatus::Ok { ref metrics, .. } = a.status {
            metrics.get("FirstContentfulPaint").copied().unwrap_or(f64::MAX)
        } else {
            f64::MAX
        };
        let fcp_b = if let IterationStatus::Ok { ref metrics, .. } = b.status {
            metrics.get("FirstContentfulPaint").copied().unwrap_or(f64::MAX)
        } else {
            f64::MAX
        };
        let da = (fcp_a - median).abs();
        let db = (fcp_b - median).abs();
        da.partial_cmp(&db).unwrap()
    })
}

/// Render an ASCII bar proportional to `ms` out of `max`, `width` chars wide.
fn fcp_bar(ms: f64, max: f64, width: usize) -> String {
    let fill = if max > 0.0 {
        ((ms / max) * width as f64).round() as usize
    } else {
        0
    }
    .min(width);
    let blank = width - fill;
    format!("[{}{}]", "█".repeat(fill), " ".repeat(blank))
}

/// Append a "Per-iteration <SHORT>" bar-chart section for a milestone
/// metric. Iterations that didn't produce the metric (e.g. LCP on a
/// page without a large enough fragment) print as "—" so the chart
/// stays aligned with the iteration list.
fn render_per_iter_chart(
    s: &mut String,
    short: &str,
    metric: &str,
    cfg: &ConfigResults,
) {
    writeln!(s, "### Per-iteration {}\n", short).unwrap();
    let bar_width = 30usize;
    let max_v = cfg
        .iterations
        .iter()
        .filter_map(|i| {
            if let IterationStatus::Ok { ref metrics, .. } = i.status {
                metrics.get(metric).copied()
            } else {
                None
            }
        })
        .fold(0.0_f64, f64::max);
    for iter in &cfg.iterations {
        match &iter.status {
            IterationStatus::Ok { ref metrics, .. } => match metrics.get(metric) {
                Some(&v) => {
                    let bar = fcp_bar(v, max_v, bar_width);
                    writeln!(s, "iter {:>2}  {} {:.0} ms", iter.index, bar, v).unwrap();
                }
                None => {
                    writeln!(
                        s,
                        "iter {:>2}  [{}] —",
                        iter.index,
                        " ".repeat(bar_width)
                    )
                    .unwrap();
                }
            },
            IterationStatus::Failed { .. } => {
                writeln!(s, "iter {:>2}  FAILED", iter.index).unwrap();
            }
        }
    }
    writeln!(s).unwrap();
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

    // ## Reproduction
    // Infer subcommand from config keys.
    let subcommand = if data.configs.contains_key("base") && data.configs.contains_key("patch") {
        "ab"
    } else {
        "bench"
    };
    writeln!(s, "## Reproduction\n").unwrap();
    if subcommand == "ab" {
        let base_bin = data.configs.get("base").map(|c| c.bin.display().to_string()).unwrap_or_default();
        let patch_bin = data.configs.get("patch").map(|c| c.bin.display().to_string()).unwrap_or_default();
        writeln!(
            s,
            "```\nservoperf ab {} --base-bin={} --patch-bin={}\n```\n",
            data.workload.name, base_bin, patch_bin
        )
        .unwrap();
    } else {
        let bin = data.configs.values().next().map(|c| c.bin.display().to_string()).unwrap_or_default();
        writeln!(
            s,
            "```\nservoperf bench {} --bin={}\n```\n",
            data.workload.name, bin
        )
        .unwrap();
    }

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

        // Critical-path phase table (representative iteration).
        writeln!(s, "### Critical path\n").unwrap();
        if let Some(rep) = repr_iteration(cfg) {
            if let IterationStatus::Ok { ref critical_path, .. } = rep.status {
                writeln!(s, "| phase | thread | ts (ms) | dur (ms) | flag |").unwrap();
                writeln!(s, "|---|---|---:|---:|---|").unwrap();
                // Collect all rows (named spans + milestones) sorted by ts_ms.
                // Each row carries (name, thread, ts_ms, dur_ms, count, is_milestone).
                let mut rows: Vec<(String, String, f64, Option<f64>, Option<u32>, bool)> =
                    Vec::new();
                for span in &critical_path.named_spans {
                    rows.push((
                        span.name.clone(),
                        span.thread.clone(),
                        span.ts_ms,
                        Some(span.dur_ms),
                        span.count,
                        false,
                    ));
                }
                for ms in &critical_path.milestones {
                    rows.push((
                        ms.name.clone(),
                        "main".to_string(),
                        ms.ts_ms,
                        None,
                        None,
                        true,
                    ));
                }
                rows.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap());

                // Interleave `_gap_` rows between consecutive phase rows when
                // there is meaningful unaccounted time (≥ 1 ms) between the
                // end of one phase and the start of the next. This gives a
                // continuous chronological view from 0 to FCP instead of
                // leaving the gaps implicit.
                const GAP_THRESHOLD_MS: f64 = 1.0;
                let mut prev_end: Option<f64> = None;
                for (phase, thread, ts, dur, count, is_ms) in rows {
                    if let Some(end) = prev_end {
                        let gap = ts - end;
                        if gap >= GAP_THRESHOLD_MS {
                            writeln!(
                                s,
                                "| _gap_ |  | {:.1} | {:.1} |  |",
                                end, gap
                            ).unwrap();
                        }
                    }
                    let dur_str = match dur {
                        Some(d) => match count {
                            Some(n) => format!("{:.1} (×{})", d, n),
                            None => format!("{:.1}", d),
                        },
                        None => String::new(),
                    };
                    let flag = if is_ms { "milestone" } else { "" };
                    writeln!(
                        s,
                        "| {} | {} | {:.1} | {} | {} |",
                        phase, thread, ts, dur_str, flag
                    ).unwrap();
                    // An aggregated row's `ts + dur` is meaningless as a
                    // chronology bound (the N occurrences are scattered, not
                    // contiguous). Only advance `prev_end` for single-span
                    // rows; otherwise just carry the prior boundary forward.
                    if let Some(d) = dur {
                        if count.is_none() {
                            prev_end = Some(ts + d);
                        }
                    } else {
                        // Milestones have no duration; treat as points.
                        prev_end = Some(ts);
                    }
                }
            }
        } else {
            writeln!(s, "_No successful iterations — phase table unavailable._").unwrap();
        }
        writeln!(s).unwrap();

        // Flagged gaps (representative iteration).
        writeln!(s, "### Flagged gaps\n").unwrap();
        let gaps_present = repr_iteration(cfg).and_then(|rep| {
            if let IterationStatus::Ok { ref critical_path, .. } = rep.status {
                if !critical_path.gaps.is_empty() {
                    Some(critical_path.gaps.clone())
                } else {
                    None
                }
            } else {
                None
            }
        });
        if let Some(gaps) = gaps_present {
            writeln!(s, "| from → to | actual gap (ms) | threshold (ms) |").unwrap();
            writeln!(s, "|---|---:|---:|").unwrap();
            for g in &gaps {
                writeln!(s, "| {} → {} | {:.1} | {:.1} |", g.from, g.to, g.actual_gap_ms, g.threshold_ms).unwrap();
            }
        } else {
            writeln!(s, "None flagged.").unwrap();
        }
        writeln!(s).unwrap();

        // Per-iteration milestone bar charts. FCP always; LCP only
        // when at least one iteration produced one (pages without a
        // large enough text/image fragment never fire LCP).
        render_per_iter_chart(&mut s, "FCP", "FirstContentfulPaint", cfg);
        if cfg.iterations.iter().any(|i| {
            matches!(&i.status, IterationStatus::Ok { metrics, .. }
                if metrics.contains_key("LargestContentfulPaint"))
        }) {
            render_per_iter_chart(&mut s, "LCP", "LargestContentfulPaint", cfg);
        }
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
        // New §8.2 sections.
        assert!(md.contains("## Reproduction"), "missing Reproduction section");
        assert!(md.contains("### Critical path"), "missing Critical path section");
        assert!(md.contains("### Flagged gaps"), "missing Flagged gaps section");
        assert!(md.contains("### Per-iteration FCP"), "missing Per-iteration FCP section");
        assert!(md.contains("iter  0"), "missing iter 0 bar line");
    }
}
