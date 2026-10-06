//! Presented-frame metrics for scenario workloads.
//!
//! A scenario workload renders for a fixed window instead of loading a page
//! once, so its headline number is how many frames actually reached the
//! display. The device reports that through
//! `hidumper -s RenderService -a "fps <surface>"`, which prints a ring of the
//! most recent presents as `<expected_ns>:<actual_ns>` lines. The ring holds
//! ~384 entries, so [`crate::ohos::OhosTarget`] samples it repeatedly during
//! the capture window and this module unions the samples into one timeline.
//!
//! Metrics come in two tiers. The *raw* tier ([`FrameTimeline::metrics`]) is
//! always reported and makes no judgement about what a frame means: frame
//! count, span, overall fps and frame-time percentiles. On top of that a
//! workload may declare [`PostProcessing`] steps that reinterpret the same
//! timeline — currently only [`PostProcessing::ExcludeIdleGaps`], which is
//! what an animation like a chart needs and what a continuously-rendering
//! workload (a WebGPU game, say) must *not* have applied, since there a long
//! frame is a dropped frame and censoring it would flatter the result.

use std::collections::BTreeMap;

use crate::workload::PostProcessing;

/// How many of the busiest thread groups get their own metric key. The tail of
/// a servo process is dozens of near-idle threads that would swamp a report.
const THREAD_CPU_ROWS: usize = 12;

/// Minimum on-CPU seconds before a thread's effective clock is reported.
/// Below this the mean is dominated by whichever operating point the thread
/// happened to catch.
const MIN_CLOCK_SAMPLE_S: f64 = 0.05;

/// Below this share of the available clock, a run is reported as
/// frequency-limited rather than work-limited. Measured runs cluster well away
/// from this line: a starved canvas frame sits near 25%, a saturated one above
/// 85%.
const STARVED_CLOCK_PCT: f64 = 70.0;

/// Presented-frame timestamps for one iteration, sorted and de-duplicated.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FrameTimeline {
    /// Present timestamps in nanoseconds, ascending.
    pub timestamps_ns: Vec<u64>,
    /// Number of gaps between consecutive ring dumps that did not overlap.
    /// Each one means frames were lost between samples, so the frame count
    /// and any span-based metric understate reality.
    pub dropped_sample_windows: usize,
}

/// Union the timestamps of several `hidumper` ring dumps of the same surface.
///
/// Only the first field of each `<expected>:<actual>` pair is used. The two
/// differ by well under a frame in practice and the expected-present time is
/// the one the ring is ordered by.
pub fn parse_ring_dumps<S: AsRef<str>>(dumps: &[S]) -> FrameTimeline {
    let mut all = std::collections::BTreeSet::new();
    let mut ranges: Vec<(u64, u64)> = Vec::new();

    for dump in dumps {
        let mut in_dump = std::collections::BTreeSet::new();
        for line in dump.as_ref().lines() {
            let line = line.trim();
            let Some((lhs, _)) = line.split_once(':') else {
                continue;
            };
            // Ring entries are bare nanosecond timestamps; every other line in
            // the dump (headers, the surface name) fails this parse.
            if lhs.len() >= 10 && lhs.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(ts) = lhs.parse::<u64>() {
                    in_dump.insert(ts);
                }
            }
        }
        if let (Some(&min), Some(&max)) = (in_dump.iter().next(), in_dump.iter().next_back()) {
            ranges.push((min, max));
        }
        all.extend(in_dump);
    }

    ranges.sort_unstable();
    let dropped_sample_windows = ranges.windows(2).filter(|w| w[1].0 > w[0].1).count();

    FrameTimeline {
        timestamps_ns: all.into_iter().collect(),
        dropped_sample_windows,
    }
}

impl FrameTimeline {
    /// Inter-frame intervals in milliseconds.
    pub fn frame_times_ms(&self) -> Vec<f64> {
        self.timestamps_ns
            .windows(2)
            .map(|w| (w[1] - w[0]) as f64 / 1e6)
            .collect()
    }

    /// Metrics that hold for any workload, with no assumption about which
    /// frames "count".
    pub fn metrics(&self) -> BTreeMap<String, f64> {
        let mut m = BTreeMap::new();
        let frame_times = self.frame_times_ms();
        if frame_times.is_empty() {
            return m;
        }
        let span_s =
            (self.timestamps_ns[self.timestamps_ns.len() - 1] - self.timestamps_ns[0]) as f64 / 1e9;
        m.insert(
            "presented.frames".to_string(),
            self.timestamps_ns.len() as f64,
        );
        m.insert("presented.span_s".to_string(), span_s);
        if span_s > 0.0 {
            m.insert(
                "presented.fps".to_string(),
                frame_times.len() as f64 / span_s,
            );
        }
        // The shape of the distribution, not just its middle: a workload that
        // alternates between hitting and missing vsync and one that is
        // uniformly a little slow have the same median and need opposite work.
        for (label, q) in [("p10", 0.10), ("p25", 0.25), ("p50", 0.50), ("p75", 0.75)] {
            m.insert(
                format!("frame_time_ms.{label}"),
                percentile(&frame_times, q),
            );
        }
        m.insert(
            "frame_time_ms.p95".to_string(),
            percentile(&frame_times, 0.95),
        );
        m.insert(
            "frame_time_ms.p99".to_string(),
            percentile(&frame_times, 0.99),
        );
        m.insert(
            "presented.lost_sample_windows".to_string(),
            self.dropped_sample_windows as f64,
        );
        m
    }

    /// How many display refresh intervals each presented frame occupied.
    ///
    /// A percentile alone cannot tell a workload that misses every other vsync
    /// (frames alternating 16.7/33.3 ms, median 16.7) from one that is
    /// uniformly a little slow (every frame 25 ms, median 25). Those need
    /// opposite fixes — the first is a cliff to be got under, the second is a
    /// budget to be shaved — so the distribution has to be reported, not just
    /// its middle.
    ///
    /// Intervals are binned by `round(dt / vsync)`, clamped at 1: bin 1 is a
    /// frame presented at the very next refresh, bin 2 one refresh late, and
    /// so on.
    pub fn vsync_histogram(&self, refresh_hz: f64, cutoff_ms: Option<f64>) -> Vec<(u32, usize)> {
        let vsync_ms = 1000.0 / refresh_hz;
        let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
        for dt in self.frame_times_ms() {
            if cutoff_ms.is_some_and(|c| dt >= c) {
                continue;
            }
            let bin = ((dt / vsync_ms).round() as i64).max(1) as u32;
            *counts.entry(bin).or_default() += 1;
        }
        counts.into_iter().collect()
    }

    /// Metrics produced by one optional post-processing step. Kept separate
    /// from [`Self::metrics`] so a report always carries the unfiltered
    /// numbers next to any reinterpretation of them.
    pub fn post_processed_metrics(&self, step: &PostProcessing) -> BTreeMap<String, f64> {
        let mut m = BTreeMap::new();
        let frame_times = self.frame_times_ms();
        if frame_times.is_empty() {
            return m;
        }
        match *step {
            PostProcessing::ExcludeIdleGaps { gap_ms } => {
                let (active, idle): (Vec<f64>, Vec<f64>) =
                    frame_times.iter().partition(|d| **d < gap_ms);
                let active_s: f64 = active.iter().sum::<f64>() / 1000.0;
                let idle_s: f64 = idle.iter().sum::<f64>() / 1000.0;
                if active_s > 0.0 {
                    m.insert(
                        "presented.fps_active".to_string(),
                        active.len() as f64 / active_s,
                    );
                }
                m.insert("presented.frames_active".to_string(), active.len() as f64);
                let total_s = active_s + idle_s;
                if total_s > 0.0 {
                    m.insert("presented.idle_pct".to_string(), 100.0 * idle_s / total_s);
                }
                // Frames just under the threshold are the ones a too-low
                // `gap_ms` would silently censor. Reporting the count makes
                // that visible instead of leaving it to trust.
                let near = frame_times
                    .iter()
                    .filter(|d| **d >= gap_ms / 2.0 && **d < gap_ms)
                    .count();
                m.insert("presented.frames_near_gap".to_string(), near as f64);
            }
        }
        m
    }
}

/// Nearest-rank percentile over an unsorted slice.
fn percentile(values: &[f64], p: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((p * sorted.len() as f64) as usize).min(sorted.len() - 1);
    sorted[idx]
}

/// Merge per-thread and per-cluster clock metrics read out of `trace`.
///
/// Split out so the file is only read when a capture might contain the events;
/// a trace without them costs one read and yields nothing.
fn merge_clock_metrics(metrics: &mut BTreeMap<String, f64>, trace: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(trace) else {
        return;
    };
    let parsed = crate::cpufreq::parse(&text);
    if parsed.is_empty() {
        return;
    }
    let report = crate::cpufreq::analyse(&parsed, crate::cpufreq::servo_pid(&parsed));
    for thread in report.threads.iter().take(THREAD_CPU_ROWS) {
        // A thread that barely ran has a frequency dominated by whichever OPP
        // it happened to catch; reporting it would invite reading noise as
        // signal.
        if thread.cpu_s < MIN_CLOCK_SAMPLE_S {
            continue;
        }
        metrics.insert(format!("thread_ghz.{}", thread.name), thread.effective_ghz);
        metrics.insert(
            format!("thread_pct_of_max_ghz.{}", thread.name),
            thread.pct_of_max,
        );
    }
    for cluster in &report.clusters {
        metrics.insert(format!("cluster_ghz.{}", cluster.name), cluster.mean_ghz);
        metrics.insert(
            format!("cluster_max_opp_pct.{}", cluster.name),
            cluster.max_opp_pct,
        );
    }

    // Whether the run had any clock headroom left decides what its numbers can
    // show. A busiest thread near its ceiling means the workload is limited by
    // the work it does, so an optimisation moves the result; one far below it
    // is limited by the governor, and an A/B there mostly measures which side
    // of the ramp each iteration landed on. Saying so is the difference between
    // a null result that rules a change out and one that never tested it.
    //
    // Prefer the busiest thread, which is the most direct statement. Without
    // the `sched` tag fall back to the busiest cluster, which needs only
    // `freq` and answers the same question about the run as a whole.
    let (what, ran_at, pct) = match report
        .threads
        .iter()
        .find(|t| t.cpu_s >= MIN_CLOCK_SAMPLE_S)
    {
        Some(busiest) => (
            busiest.name.clone(),
            busiest.effective_ghz,
            busiest.pct_of_max,
        ),
        None => {
            let Some(busiest) = report
                .clusters
                .iter()
                .filter(|c| c.max_ghz > 0.0)
                .max_by(|a, b| a.mean_ghz.total_cmp(&b.mean_ghz))
            else {
                return;
            };
            (
                format!("the {} cluster", busiest.name),
                busiest.mean_ghz,
                100.0 * busiest.mean_ghz / busiest.max_ghz,
            )
        }
    };
    metrics.insert("clock_headroom_pct".to_string(), (100.0 - pct).max(0.0));
    if pct < STARVED_CLOCK_PCT {
        eprintln!(
            "warning: {what} ran at {ran_at:.2} GHz, {pct:.0}% of what its cores offer — this \
             run was limited by cpu frequency, not by the work it did. Treat comparisons \
             against other runs with care."
        );
    }
}

/// Compute a scenario workload's metrics and merge them into `metrics`.
///
/// A no-op for workloads without a `[scenario]` block, so both `bench` and
/// `ab` can call it unconditionally. Bad patterns and empty timelines are
/// reported as warnings rather than failing the iteration: a scenario run
/// that produced a trace is still worth keeping.
pub fn merge_scenario_metrics(
    metrics: &mut BTreeMap<String, f64>,
    workload: &crate::workload::Workload,
    fps_dumps: &[String],
    log: Option<&str>,
    thread_cpu: Option<(&str, &str)>,
    trace: Option<&std::path::Path>,
) {
    let Some(scenario) = workload.scenario.as_ref() else {
        return;
    };

    let timeline = parse_ring_dumps(fps_dumps);
    if timeline.timestamps_ns.len() < 2 {
        eprintln!(
            "warning: scenario {:?} produced no presented frames for surface {:?}; \
             check the surface name with `hidumper -s RenderService -a fps`",
            workload.name, scenario.surface
        );
    } else {
        if timeline.dropped_sample_windows > 0 {
            eprintln!(
                "warning: {} gap(s) between frame-ring samples in {:?}: frames were lost, so \
                 span-based metrics understate the run. Lower `sample_interval_seconds`.",
                timeline.dropped_sample_windows, workload.name
            );
        }
        metrics.extend(timeline.metrics());

        // How much of the render window the timeline actually covers. A
        // single dump cannot be caught by the gap check above — there is no
        // second sample to compare against — but the ring still wraps, so a
        // too-long `sample_interval_seconds` silently measures only the tail
        // of the run. Coverage catches that case.
        if let (Some(window), Some(span)) = (
            scenario.capture_seconds,
            metrics.get("presented.span_s").copied(),
        ) {
            if window > 0 {
                let coverage = 100.0 * span / window as f64;
                metrics.insert("presented.window_coverage_pct".to_string(), coverage);
                if coverage < 90.0 {
                    eprintln!(
                        "warning: frame ring covers only {coverage:.0}% of the {window}s window \
                         in {:?} ({span:.1}s): the ring wrapped between samples and these \
                         metrics describe the tail of the run. Lower `sample_interval_seconds`.",
                        workload.name
                    );
                }
            }
        }
        for step in &scenario.post_processing {
            metrics.extend(timeline.post_processed_metrics(step));
        }

        // Bin against the refresh rate. Idle gaps are excluded when the
        // workload declares what counts as one, so the bins describe frames
        // the page was actually trying to produce.
        let cutoff = scenario.post_processing.iter().find_map(|s| match *s {
            PostProcessing::ExcludeIdleGaps { gap_ms } => Some(gap_ms),
        });
        let hist = timeline.vsync_histogram(scenario.refresh_hz, cutoff);
        let total: usize = hist.iter().map(|(_, n)| *n).sum();
        if total > 0 {
            for (bin, n) in &hist {
                let key = if *bin >= 4 {
                    "frame_time_ms.vsync4plus_pct".to_string()
                } else {
                    format!("frame_time_ms.vsync{bin}_pct")
                };
                *metrics.entry(key).or_insert(0.0) += 100.0 * *n as f64 / total as f64;
            }
        }
    }

    // Per-thread CPU cost, normalized by the frames those threads produced.
    // Uses the raw frame count rather than the active one: the CPU counters
    // span the whole window, idle time included.
    //
    // Skipped without a frame count: dividing by zero frames produced a full
    // table of `thread_cpu_ms_per_frame.* = 0.0`, which in a two-leg run sits
    // beside the other leg's real numbers and reads as a thread that costs
    // nothing rather than a surface name that was wrong.
    if let Some((before, after)) = thread_cpu {
        let frames = metrics.get("presented.frames").copied().unwrap_or(0.0);
        if frames <= 0.0 {
            eprintln!(
                "warning: no presented frames, so per-frame thread CPU is not reported for \
                 {:?} — the surface name is the usual cause",
                workload.name
            );
            return;
        }
        let before_sample = crate::threads::parse_sample(before);
        // Under `thread_cpu_from_start` the opening sample is a wall-clock
        // reading with no thread rows, so every lookup misses and the "delta"
        // is the whole absolute counter. Report that under names that say so
        // rather than under per-frame ones that would be a lie.
        let from_start = crate::threads::is_wall_clock_only(&before_sample);
        let costs =
            crate::threads::costs(&before_sample, &crate::threads::parse_sample(after), frames);
        let prefix = if from_start {
            "thread_cpu_ms_since_start_per_frame"
        } else {
            "thread_cpu_ms_per_frame"
        };
        for cost in costs.iter().take(THREAD_CPU_ROWS) {
            metrics.insert(format!("{prefix}.{}", cost.name), cost.cpu_ms_per_frame);
            // Without the thread count the per-frame figure is ambiguous:
            // 20 ms on one thread is a hard serial floor on frame time, while
            // 20 ms across four is 5 ms of wall clock.
            metrics.insert(format!("thread_count.{}", cost.name), cost.threads as f64);
            metrics.insert(format!("thread_core_pct.{}", cost.name), cost.core_pct);
        }
        let total: f64 = costs.iter().map(|c| c.cpu_ms_per_frame).sum();
        metrics.insert(format!("{prefix}.total"), total);

        // Same threads, counted from process start. Page-load workloads spend
        // most of their CPU before the opening sample can be taken, so the
        // window delta above misses it; the absolute counters do not, because
        // every iteration cold-starts the app.
        let absolute = crate::threads::costs_since_process_start(
            &crate::threads::parse_sample(before),
            &crate::threads::parse_sample(after),
            frames,
        );
        for cost in absolute.iter().take(THREAD_CPU_ROWS) {
            metrics.insert(format!("thread_cpu_ms.{}", cost.name), cost.cpu_ms);
        }
        let total: f64 = absolute.iter().map(|c| c.cpu_ms).sum();
        metrics.insert("thread_cpu_ms.total".to_string(), total);
    }

    // What clock those threads ran at. CPU-milliseconds alone cannot tell a
    // thread doing more work from one running slower, and on this hardware the
    // second is worth more than any optimisation measured so far. Needs the
    // `sched` and `freq` hitrace tags, so it is silently absent for local
    // (pftrace) targets and for captures taken without them.
    if let Some(path) = trace {
        merge_clock_metrics(metrics, path);
    }

    let Some(log) = log else { return };
    for lm in &scenario.log_metric {
        match regex::Regex::new(&lm.pattern) {
            Ok(re) => {
                if let Some(v) = lm.aggregate.apply_ordered(log_metric_value(log, &re)) {
                    metrics.insert(lm.name.clone(), v);
                }
            }
            Err(err) => eprintln!(
                "warning: log_metric {:?} has an invalid pattern: {err}",
                lm.name
            ),
        }
    }
}

/// Extract a named value from captured device log text.
///
/// The pattern's first capture group is parsed as an `f64`; every match in the
/// log contributes one sample and [`crate::workload::LogAggregate`] reduces
/// them to the single number that lands in the metrics map.
pub fn log_metric_value(log: &str, pattern: &regex::Regex) -> Vec<f64> {
    pattern
        .captures_iter(log)
        .filter_map(|c| c.get(1))
        .filter_map(|m| m.as_str().parse::<f64>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two overlapping dumps of a 60 Hz surface, in the exact shape hidumper
    /// prints: a header block, a surface line, then `expected:actual` pairs
    /// in descending order.
    fn dump(start_ms: u64, count: u64, step_ms: u64) -> String {
        let mut s = String::from(
            "\n---[ability]---\n\n----RenderService----\n\
             -- The recently fps records info of screens:\n surface [ServoDemoSurface]:\n",
        );
        for i in (0..count).rev() {
            let ts = (start_ms + i * step_ms) * 1_000_000;
            s.push_str(&format!("{}:{}\n", ts, ts + 1_000_000));
        }
        s
    }

    #[test]
    fn parses_and_unions_overlapping_dumps() {
        let a = dump(1_000, 10, 16);
        let b = dump(1_080, 10, 16);
        let t = parse_ring_dumps(&[a, b]);
        // 10 frames each, overlapping by 5 (1080..1144 appears in both).
        assert_eq!(t.timestamps_ns.len(), 15);
        assert_eq!(t.dropped_sample_windows, 0);
        assert!(t.timestamps_ns.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn flags_non_overlapping_dumps() {
        let a = dump(1_000, 5, 16);
        let b = dump(9_000, 5, 16);
        assert_eq!(parse_ring_dumps(&[a, b]).dropped_sample_windows, 1);
    }

    #[test]
    fn ignores_non_ring_lines() {
        let t = parse_ring_dumps(&["surface [ServoDemoSurface]:\nnot:a:timestamp\n123:456\n"]);
        assert!(t.timestamps_ns.is_empty(), "short lhs is not a timestamp");
    }

    #[test]
    fn raw_metrics_make_no_idle_judgement() {
        // 16 ms frames with one 500 ms stall in the middle.
        let mut ts: Vec<u64> = (0..10).map(|i| i * 16_000_000).collect();
        ts.push(ts.last().unwrap() + 500_000_000);
        ts.push(ts.last().unwrap() + 16_000_000);
        let t = FrameTimeline {
            timestamps_ns: ts,
            dropped_sample_windows: 0,
        };
        let m = t.metrics();
        assert_eq!(m["presented.frames"], 12.0);
        // 11 intervals over 0.644 s of span: the stall drags fps down, which
        // is exactly what a game workload wants to see.
        assert!((m["presented.fps"] - 17.0).abs() < 0.5, "{m:?}");
        assert!((m["frame_time_ms.p50"] - 16.0).abs() < 0.01);
        assert!(m["frame_time_ms.p99"] > 400.0);
    }

    #[test]
    fn idle_gap_filter_excludes_the_stall() {
        let mut ts: Vec<u64> = (0..10).map(|i| i * 16_000_000).collect();
        ts.push(ts.last().unwrap() + 500_000_000);
        ts.push(ts.last().unwrap() + 16_000_000);
        let t = FrameTimeline {
            timestamps_ns: ts,
            dropped_sample_windows: 0,
        };
        let m = t.post_processed_metrics(&PostProcessing::ExcludeIdleGaps { gap_ms: 80.0 });
        // 10 of the 11 intervals are 16 ms; the 500 ms one is dropped.
        assert!((m["presented.fps_active"] - 62.5).abs() < 0.1, "{m:?}");
        assert_eq!(m["presented.frames_active"], 10.0);
        assert!(m["presented.idle_pct"] > 75.0);
        assert_eq!(m["presented.frames_near_gap"], 0.0);
    }

    #[test]
    fn near_gap_frames_are_counted() {
        // 50 ms frames under an 80 ms threshold: kept, but flagged.
        let ts: Vec<u64> = (0..5).map(|i| i * 50_000_000).collect();
        let t = FrameTimeline {
            timestamps_ns: ts,
            dropped_sample_windows: 0,
        };
        let m = t.post_processed_metrics(&PostProcessing::ExcludeIdleGaps { gap_ms: 80.0 });
        assert_eq!(m["presented.frames_near_gap"], 4.0);
    }

    #[test]
    fn coverage_flags_a_wrapped_ring() {
        // 384 frames at 48 ms is 18.4 s of a 30 s window: what a single
        // end-of-window sample sees once the ring has wrapped.
        let ts: Vec<u64> = (0..384).map(|i| i * 48_000_000).collect();
        let t = FrameTimeline {
            timestamps_ns: ts,
            dropped_sample_windows: 0,
        };
        // The gap check cannot see this: there is only one dump.
        assert_eq!(t.dropped_sample_windows, 0);
        let span = t.metrics()["presented.span_s"];
        assert!(
            (100.0 * span / 30.0) < 90.0,
            "span {span} should undercover"
        );
    }

    #[test]
    fn log_metric_extracts_every_match() {
        let re = regex::Regex::new(r"init_to_finished_ms=([0-9.]+)").unwrap();
        let log = "COMPLETE: cycle=1 init_to_finished_ms=1330.8 x\n\
                   COMPLETE: cycle=2 init_to_finished_ms=1345.1 x\n";
        assert_eq!(log_metric_value(log, &re), vec![1330.8, 1345.1]);
    }
}
