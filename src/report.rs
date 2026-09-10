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
    /// The command line this run was invoked with, so the report reproduces
    /// what actually ran rather than a guess reassembled from the results.
    #[serde(default)]
    pub command: String,
    pub tool_version: String,
    pub timestamp_utc: String,
    pub workload: Workload,
    pub configs: BTreeMap<String, ConfigResults>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub deltas: BTreeMap<String, SummaryDelta>,
}

pub fn write_json(out_dir: &Path, data: &RunResults) -> Result<()> {
    let path = out_dir.join("raw.json");
    let file =
        std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    serde_json::to_writer_pretty(file, data)
        .with_context(|| format!("writing JSON to {}", path.display()))?;
    Ok(())
}

/// Render a Unix timestamp as `YYYY-MM-DD HH:MM:SS UTC`.
///
/// Civil-from-days per Howard Hinnant's algorithm, so the report carries a
/// date a reader can compare against a lab notebook without pulling in a
/// date-time crate for one line of output.
pub fn format_utc(secs: i64) -> String {
    let (y, m, d, h, mi, sec) = civil_from_unix(secs);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{sec:02} UTC")
}

/// The same instant as a directory-name component: `20260910-163045`.
///
/// No colons or spaces — a path with either is awkward on Windows, and these
/// names end up in shell commands and report links.
pub fn format_utc_compact(secs: i64) -> String {
    let (y, m, d, h, mi, sec) = civil_from_unix(secs);
    format!("{y:04}{m:02}{d:02}-{h:02}{mi:02}{sec:02}")
}

fn civil_from_unix(secs: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Shift the epoch to 0000-03-01 so leap days land at the end of the cycle.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, mi, sec)
}

/// The current process's command line, shell-quoted well enough to paste back.
pub fn invocation() -> String {
    std::env::args()
        .map(|a| {
            if a.is_empty() || a.contains([' ', '"', '\'', '$', '`']) {
                format!("'{}'", a.replace('\'', "'\\''"))
            } else {
                a
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The current time, formatted for a report header.
pub fn now_utc() -> String {
    format_utc(unix_now())
}

/// Default output directory for a run: `out/<name>-<YYYYMMDD-HHMMSS>`.
///
/// Shared by every command. Three copies of this is how `suite` kept writing
/// epoch seconds after the other two were fixed.
pub fn default_out_dir(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from("out").join(format!("{}-{}", name, format_utc_compact(unix_now())))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64
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
            metrics
                .get("FirstContentfulPaint")
                .copied()
                .unwrap_or(f64::MAX)
        } else {
            f64::MAX
        };
        let fcp_b = if let IterationStatus::Ok { ref metrics, .. } = b.status {
            metrics
                .get("FirstContentfulPaint")
                .copied()
                .unwrap_or(f64::MAX)
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
fn render_per_iter_chart(s: &mut String, short: &str, metric: &str, cfg: &ConfigResults) {
    writeln!(s, "### Per-iteration {}\n", short).unwrap();
    // Fenced: without it Markdown reflows the rows into one paragraph and the
    // bars stop lining up.
    writeln!(s, "```").unwrap();
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
                    writeln!(s, "iter {:>2}  [{}] —", iter.index, " ".repeat(bar_width)).unwrap();
                }
            },
            IterationStatus::Failed { .. } => {
                writeln!(s, "iter {:>2}  FAILED", iter.index).unwrap();
            }
        }
    }
    writeln!(s, "```\n").unwrap();
}

/// Presented-frame metrics for scenario workloads. Rendered only when the
/// run produced them, so page-load reports are unchanged.
///
/// The unfiltered `presented.fps` is listed first and any post-processed
/// interpretation after it, so a reader always sees what the device actually
/// presented before seeing a number that reinterprets it.
/// Per-thread CPU cost, when the scenario asked for it.
///
/// The row to read against `frame_time_ms.p50`: a thread whose CPU per frame
/// approaches the frame time is the one the frame is waiting on, and the sum
/// over all threads against the frame time says how much parallelism is
/// actually being had.
fn render_thread_cpu_section(s: &mut String, cfg: &ConfigResults) {
    const PREFIX: &str = "thread_cpu_ms_per_frame.";
    let mut keys: Vec<&String> = cfg
        .iterations
        .iter()
        .filter_map(|i| match &i.status {
            IterationStatus::Ok { metrics, .. } => Some(metrics),
            _ => None,
        })
        .flat_map(|m| m.keys())
        .filter(|k| k.starts_with(PREFIX))
        .collect();
    keys.sort();
    keys.dedup();
    if keys.is_empty() {
        return;
    }

    let median = |key: &str| -> Option<f64> {
        let v: Vec<f64> = cfg
            .iterations
            .iter()
            .filter_map(|i| match &i.status {
                IterationStatus::Ok { metrics, .. } => metrics.get(key).copied(),
                _ => None,
            })
            .collect();
        crate::stats::summarise(&v).map(|s| s.p50)
    };

    let mut rows: Vec<(String, f64)> = keys
        .iter()
        .filter(|k| k.as_str() != "thread_cpu_ms_per_frame.total")
        .filter_map(|k| median(k).map(|v| (k[PREFIX.len()..].to_string(), v)))
        .collect();
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    writeln!(s, "### CPU per presented frame, by thread\n").unwrap();
    writeln!(
        s,
        "`CPU ms/frame` divided by `threads` is the floor this group puts under the frame time; \
         `busy` is how much of one core it kept.\n"
    )
    .unwrap();
    writeln!(s, "| thread | CPU ms/frame | threads | per thread | busy |").unwrap();
    writeln!(s, "|---|---:|---:|---:|---:|").unwrap();
    for (name, v) in &rows {
        let n = median(&format!("thread_count.{name}"))
            .unwrap_or(1.0)
            .max(1.0);
        let busy = median(&format!("thread_core_pct.{name}")).unwrap_or(0.0);
        writeln!(
            s,
            "| {name} | {v:.2} | {n:.0} | {:.2} | {busy:.0}% |",
            v / n
        )
        .unwrap();
    }
    if let Some(total) = median("thread_cpu_ms_per_frame.total") {
        writeln!(s, "| **all threads** | **{total:.2}** | | | |").unwrap();
    }
    writeln!(s).unwrap();
}

fn render_scenario_section(s: &mut String, cfg: &ConfigResults) {
    const ROWS: &[(&str, &str, &str)] = &[
        ("presented.fps", "Presented fps", ""),
        ("presented.fps_active", "…excluding idle gaps", ""),
        ("frame_time_ms.p10", "Frame time p10", " ms"),
        ("frame_time_ms.p25", "Frame time p25", " ms"),
        ("frame_time_ms.p50", "Frame time p50", " ms"),
        ("frame_time_ms.p75", "Frame time p75", " ms"),
        ("frame_time_ms.p95", "Frame time p95", " ms"),
        ("frame_time_ms.p99", "Frame time p99", " ms"),
        ("presented.idle_pct", "Idle share", " %"),
        ("presented.frames", "Frames presented", ""),
        ("presented.frames_near_gap", "Frames near the idle gap", ""),
        ("presented.window_coverage_pct", "Window coverage", " %"),
        ("presented.lost_sample_windows", "Lost sample windows", ""),
        (
            "frame_time_ms.vsync1_pct",
            "…presented at the next vsync",
            " %",
        ),
        ("frame_time_ms.vsync2_pct", "…one vsync late", " %"),
        ("frame_time_ms.vsync3_pct", "…two vsyncs late", " %"),
        (
            "frame_time_ms.vsync4plus_pct",
            "…three or more vsyncs late",
            " %",
        ),
    ];

    let samples = |key: &str| -> Vec<f64> {
        cfg.iterations
            .iter()
            .filter_map(|i| match &i.status {
                IterationStatus::Ok { metrics, .. } => metrics.get(key).copied(),
                _ => None,
            })
            .collect()
    };

    writeln!(s, "### Presented frames\n").unwrap();
    writeln!(s, "| metric | p50 | min | max | n |").unwrap();
    writeln!(s, "|---|---|---|---|---|").unwrap();
    for (key, label, unit) in ROWS {
        let v = samples(key);
        if v.is_empty() {
            continue;
        }
        let Some(sum) = crate::stats::summarise(&v) else {
            continue;
        };
        writeln!(
            s,
            "| {label} | {:.2}{unit} | {:.2} | {:.2} | {} |",
            sum.p50,
            v.iter().cloned().fold(f64::INFINITY, f64::min),
            v.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            v.len()
        )
        .unwrap();
    }
    writeln!(s).unwrap();

    render_thread_cpu_section(s, cfg);

    // Frames sitting just under the idle threshold are the ones the filter
    // would censor next; a large share means the threshold is too low for
    // this workload and `presented.fps_active` is flattering it.
    let near: f64 = samples("presented.frames_near_gap").iter().sum();
    let total: f64 = samples("presented.frames").iter().sum();
    if total > 0.0 && near / total > 0.1 {
        writeln!(
            s,
            "> {:.0}% of frames fall within a factor of two below the idle-gap \
             threshold. Raise `gap_ms` for this workload, or read \
             `presented.fps` rather than `presented.fps_active`.\n",
            100.0 * near / total
        )
        .unwrap();
    }
}

/// Append a "Thermal" section with a min/max/peak-delta summary and a
/// per-iteration before/after/Δ table. Values are stored as milli-Celsius
/// in the metrics map; the section converts to °C with one decimal for
/// readability. Iterations missing the metric (read failures, local
/// target) render as `—`.
fn render_thermal_section(s: &mut String, cfg: &ConfigResults) {
    fn mc_to_c(mc: f64) -> f64 {
        mc / 1000.0
    }
    fn all_equal(v: &[f64]) -> bool {
        v.len() > 1 && v.windows(2).all(|w| w[0] == w[1])
    }
    let befores: Vec<f64> = cfg
        .iterations
        .iter()
        .filter_map(|i| match &i.status {
            IterationStatus::Ok { metrics, .. } => {
                metrics.get("soc_thermal_milli_c.before").copied()
            }
            _ => None,
        })
        .collect();
    let afters: Vec<f64> = cfg
        .iterations
        .iter()
        .filter_map(|i| match &i.status {
            IterationStatus::Ok { metrics, .. } => {
                metrics.get("soc_thermal_milli_c.after").copied()
            }
            _ => None,
        })
        .collect();
    let deltas: Vec<f64> = cfg
        .iterations
        .iter()
        .filter_map(|i| match &i.status {
            IterationStatus::Ok { metrics, .. } => {
                metrics.get("soc_thermal_milli_c.delta").copied()
            }
            _ => None,
        })
        .collect();

    writeln!(s, "### SoC thermal\n").unwrap();
    if all_equal(&befores) && all_equal(&afters) {
        writeln!(
            s,
            "> Every iteration reported the same before and after value. That is a static \
             zone, not a thermally flat run — some devices pin a placeholder here (PLR-AL00 \
             reports a flat 30000 m°C on `soc_thermal` under any load). Re-run with \
             `--ohos-thermal-zone board_thermal`, or read these two columns as unavailable.\n"
        )
        .unwrap();
    }
    let min_temp = befores
        .iter()
        .chain(afters.iter())
        .cloned()
        .fold(f64::INFINITY, f64::min);
    let max_temp = befores
        .iter()
        .chain(afters.iter())
        .cloned()
        .fold(f64::NEG_INFINITY, f64::max);
    let max_delta = deltas.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    if min_temp.is_finite() && max_temp.is_finite() {
        let headroom_to_trip = 70.0 - mc_to_c(max_temp);
        let max_delta_str = if max_delta.is_finite() {
            format!("{:+.1} °C", mc_to_c(max_delta))
        } else {
            "—".to_string()
        };
        writeln!(
            s,
            "min {:.1} °C, max {:.1} °C (headroom to trip: {:.1} °C), peak Δ per iter: {}.\n",
            mc_to_c(min_temp),
            mc_to_c(max_temp),
            headroom_to_trip,
            max_delta_str,
        )
        .unwrap();
    }
    writeln!(s, "| iter | before (°C) | after (°C) | Δ (°C) |").unwrap();
    writeln!(s, "|---:|---:|---:|---:|").unwrap();
    for iter in &cfg.iterations {
        let (b, a, d) = match &iter.status {
            IterationStatus::Ok { metrics, .. } => (
                metrics
                    .get("soc_thermal_milli_c.before")
                    .copied()
                    .map(mc_to_c),
                metrics
                    .get("soc_thermal_milli_c.after")
                    .copied()
                    .map(mc_to_c),
                metrics
                    .get("soc_thermal_milli_c.delta")
                    .copied()
                    .map(mc_to_c),
            ),
            _ => (None, None, None),
        };
        let fmt = |v: Option<f64>| match v {
            Some(x) => format!("{:.1}", x),
            None => "—".to_string(),
        };
        let dfmt = match d {
            Some(x) => format!("{:+.1}", x),
            None => "—".to_string(),
        };
        writeln!(s, "| {} | {} | {} | {} |", iter.index, fmt(b), fmt(a), dfmt).unwrap();
    }
    writeln!(s).unwrap();
}

/// Render per-function inclusive instruction summaries when the run was
/// invoked with `--with-instructions`. Quietly skipped otherwise.
///
/// Counts come from the iteration's perf.data, aggregated by
/// [`crate::instructions::aggregate_inclusive_from_perf_data`] and stored
/// on each iteration under the key `instructions.<func>`. The summary row is
/// computed in `cmd::bench` and lives at the same key in `cfg.summary`.
fn render_instructions_section(s: &mut String, cfg: &ConfigResults) {
    let entries: Vec<(&String, &Summary)> = cfg
        .summary
        .iter()
        .filter(|(k, _)| k.starts_with("instructions."))
        .collect();
    if entries.is_empty() {
        return;
    }
    writeln!(s, "### Instructions (inclusive, hw-instructions)\n").unwrap();
    writeln!(
        s,
        "| function | n | p50 (M) | mean (M) | p90 (M) | max (M) |"
    )
    .unwrap();
    writeln!(s, "|---|---:|---:|---:|---:|---:|").unwrap();
    for (key, sum) in entries {
        let func = key.trim_start_matches("instructions.");
        let m = |v: f64| v / 1_000_000.0;
        writeln!(
            s,
            "| `{}` | {} | {:.1} | {:.1} | {:.1} | {:.1} |",
            func,
            sum.n,
            m(sum.p50),
            m(sum.mean),
            m(sum.p90),
            m(sum.max),
        )
        .unwrap();
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
    if !data.command.is_empty() {
        writeln!(s, "```\n{}\n```\n", data.command).unwrap();
    } else if subcommand == "ab" {
        let base_bin = data
            .configs
            .get("base")
            .map(|c| c.bin.display().to_string())
            .unwrap_or_default();
        let patch_bin = data
            .configs
            .get("patch")
            .map(|c| c.bin.display().to_string())
            .unwrap_or_default();
        writeln!(
            s,
            "```\nservoperf ab {} --base-bin={} --patch-bin={}\n```\n",
            data.workload.name, base_bin, patch_bin
        )
        .unwrap();
    } else {
        let bin = data
            .configs
            .values()
            .next()
            .map(|c| c.bin.display().to_string())
            .unwrap_or_default();
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
        writeln!(
            s,
            "| metric | n | min | p25 | p50 | mean | p75 | p90 | max |"
        )
        .unwrap();
        writeln!(s, "|---|---:|---:|---:|---:|---:|---:|---:|---:|").unwrap();
        for (metric, sum) in &cfg.summary {
            // Instruction summaries are rendered in their own section with
            // millions-formatting — skip them here to keep this table about
            // timing metrics.
            if metric.starts_with("instructions.") {
                continue;
            }
            writeln!(
                s,
                "| {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
                metric, sum.n, sum.min, sum.p25, sum.p50, sum.mean, sum.p75, sum.p90, sum.max
            )
            .unwrap();
        }
        writeln!(s).unwrap();

        // Per-function inclusive hw-instructions, populated only when
        // `bench --with-instructions` ran. Numbers shown as millions; the
        // raw integer counts are available in `raw.json`.
        render_instructions_section(&mut s, cfg);

        // Critical-path phase table (representative iteration).
        writeln!(s, "### Critical path\n").unwrap();
        if let Some(rep) = repr_iteration(cfg) {
            if let IterationStatus::Ok {
                ref critical_path, ..
            } = rep.status
            {
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
                            writeln!(s, "| _gap_ |  | {:.1} | {:.1} |  |", end, gap).unwrap();
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
                    )
                    .unwrap();
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
            if let IterationStatus::Ok {
                ref critical_path, ..
            } = rep.status
            {
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
                writeln!(
                    s,
                    "| {} → {} | {:.1} | {:.1} |",
                    g.from, g.to, g.actual_gap_ms, g.threshold_ms
                )
                .unwrap();
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

        // SoC thermal trace, OHOS only — gated on at least one
        // iteration having captured a `before` sample. The SoC trip
        // point is 70 °C on this hardware (`thermal_zone0` passive
        // trip); a max anywhere near that means the run was thermally
        // bounded and FCP/LCP numbers should be treated with caution.
        if cfg.iterations.iter().any(|i| {
            matches!(&i.status, IterationStatus::Ok { metrics, .. }
                if metrics.contains_key("soc_thermal_milli_c.before"))
        }) {
            render_thermal_section(&mut s, cfg);
        }

        // Scenario workloads only: what the display actually showed.
        if cfg.iterations.iter().any(|i| {
            matches!(&i.status, IterationStatus::Ok { metrics, .. }
                if metrics.contains_key("presented.fps"))
        }) {
            render_scenario_section(&mut s, cfg);
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
    /// Guards the shape of what every command derives its output directory
    /// and report header from. The previous bug was not a broken formatter but
    /// a call site that kept its own copy of the epoch-seconds version.
    #[test]
    fn shared_helpers_produce_dates_not_epochs() {
        let dir = super::default_out_dir("mossel");
        let name = dir.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("mossel-"), "{name}");
        let stamp = name.trim_start_matches("mossel-");
        assert_eq!(stamp.len(), 15, "expected YYYYMMDD-HHMMSS, got {stamp}");
        assert_eq!(&stamp[8..9], "-");
        assert!(stamp.chars().filter(|c| c.is_ascii_digit()).count() == 14);
        let now = super::now_utc();
        assert!(now.ends_with(" UTC") && now.starts_with("20"), "{now}");
    }

    #[test]
    fn utc_formatting_matches_a_known_instant() {
        // 2026-09-10T20:30:45Z
        let t = 1_789_072_245;
        assert_eq!(super::format_utc(t), "2026-09-10 20:30:45 UTC");
        assert_eq!(super::format_utc_compact(t), "20260910-203045");
        // Epoch, and a leap day, to exercise the civil-date arithmetic.
        assert_eq!(super::format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(super::format_utc_compact(1_709_164_800), "20240229-000000");
    }

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
            scenario: None,
            steps: vec![],
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
            command: "servoperf bench demo --ohos".to_string(),
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
        assert!(
            md.contains("## Reproduction"),
            "missing Reproduction section"
        );
        assert!(
            md.contains("### Critical path"),
            "missing Critical path section"
        );
        assert!(
            md.contains("### Flagged gaps"),
            "missing Flagged gaps section"
        );
        assert!(
            md.contains("### Per-iteration FCP"),
            "missing Per-iteration FCP section"
        );
        assert!(md.contains("iter  0"), "missing iter 0 bar line");
    }
}
