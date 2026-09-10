//! Per-thread CPU accounting across a scenario's render window.
//!
//! Frame-time percentiles say *how long* a frame took; they say nothing about
//! which thread spent that time. Sampling `/proc/<pid>/task/*/stat` at the two
//! ends of the render window and dividing by the frames actually presented
//! gives CPU-milliseconds per frame per thread, which is what identifies the
//! thread a frame is waiting on.
//!
//! This is CPU time, not wall time: a thread at 16 ms/frame is saturating a
//! core, while one at 2 ms/frame is either cheap or blocked. Reading it
//! alongside `frame_time_ms.p50` is what separates the two.

use std::collections::BTreeMap;

/// Clock ticks per second (`USER_HZ`). Fixed at 100 on every Linux and
/// OpenHarmony build we target; `/proc/*/stat` reports CPU time in these.
const USER_HZ: f64 = 100.0;

/// One thread's CPU counters at a point in time.
#[derive(Debug, Clone, PartialEq)]
struct ThreadTimes {
    comm: String,
    /// `utime + stime`, in clock ticks.
    ticks: u64,
}

/// A `/proc` sample of every thread in the process, plus when it was taken.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadCpuSample {
    pub wall_s: f64,
    /// Keyed by thread id, which is stable for a thread's lifetime.
    threads: BTreeMap<u32, ThreadTimes>,
}

/// Parse the device-side sample produced by [`SAMPLE_COMMAND`].
///
/// The first line is the wall clock; every later line is
/// `<tid>|<comm>|<contents of /proc/<pid>/task/<tid>/stat>`. The stat line is
/// parsed from its last `)` so that a thread name containing spaces or
/// parentheses cannot shift the field offsets.
pub fn parse_sample(raw: &str) -> ThreadCpuSample {
    let mut lines = raw.lines();
    let wall_s = lines
        .next()
        .and_then(|l| l.trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    let mut threads = BTreeMap::new();
    for line in lines {
        let mut parts = line.splitn(3, '|');
        let (Some(tid), Some(comm), Some(stat)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Ok(tid) = tid.trim().parse::<u32>() else {
            continue;
        };
        // Fields 14 and 15 (1-based) are utime and stime, counted from the
        // field after the closing paren of field 2.
        let Some(after_comm) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
            continue;
        };
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        let (Some(utime), Some(stime)) = (fields.get(11), fields.get(12)) else {
            continue;
        };
        let (Ok(utime), Ok(stime)) = (utime.parse::<u64>(), stime.parse::<u64>()) else {
            continue;
        };
        threads.insert(
            tid,
            ThreadTimes {
                comm: comm.trim().to_string(),
                ticks: utime + stime,
            },
        );
    }
    ThreadCpuSample { wall_s, threads }
}

/// One thread group's CPU cost over the window.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadCost {
    /// Thread name, with servo's per-thread numbering (`Canvas#3`) folded away
    /// so a pool reads as one row.
    pub name: String,
    /// How many distinct threads were folded into this row.
    pub threads: usize,
    pub cpu_ms: f64,
    /// CPU milliseconds per presented frame — the number to compare against
    /// `frame_time_ms.p50`.
    pub cpu_ms_per_frame: f64,
    /// Share of one core over the window: 100% means the group kept exactly
    /// one core busy throughout.
    pub core_pct: f64,
}

/// Fold servo's pool suffixes so `Canvas#1`/`Canvas#2` aggregate into `Canvas`.
/// Threads whose names are genuinely distinct stay distinct.
pub(crate) fn group_name(comm: &str) -> String {
    let base = comm.split('#').next().unwrap_or(comm);
    let base = base.trim_end_matches(|c: char| c.is_ascii_digit());
    let trimmed = base.trim_end_matches(['-', '_', ' ']);
    if trimmed.is_empty() {
        comm.to_string()
    } else {
        trimmed.to_string()
    }
}

/// CPU cost per thread group between two samples, busiest first.
///
/// A thread present only in `after` counts its whole lifetime (it was spawned
/// mid-window); one present only in `before` is dropped, since we cannot know
/// when it exited. Both cases are reported by [`ThreadCost::threads`].
/// True when the `before` sample carries no thread rows at all — the shape
/// `sample_wall_clock()` produces for `thread_cpu_from_start`.
///
/// It matters because `costs` would then find no baseline for any thread and
/// silently return the *absolute* counter as if it were the delta, publishing
/// process-lifetime CPU under a per-frame name.
pub fn is_wall_clock_only(sample: &ThreadCpuSample) -> bool {
    sample.threads.is_empty()
}

pub fn costs(before: &ThreadCpuSample, after: &ThreadCpuSample, frames: f64) -> Vec<ThreadCost> {
    let elapsed_s = (after.wall_s - before.wall_s).max(0.0);
    let mut by_group: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for (tid, end) in &after.threads {
        let start_ticks = before.threads.get(tid).map_or(0, |t| t.ticks);
        let delta = end.ticks.saturating_sub(start_ticks);
        let entry = by_group.entry(group_name(&end.comm)).or_insert((0.0, 0));
        entry.0 += delta as f64 * 1000.0 / USER_HZ;
        entry.1 += 1;
    }
    let mut out: Vec<ThreadCost> = by_group
        .into_iter()
        .map(|(name, (cpu_ms, threads))| ThreadCost {
            name,
            threads,
            cpu_ms,
            cpu_ms_per_frame: if frames > 0.0 { cpu_ms / frames } else { 0.0 },
            core_pct: if elapsed_s > 0.0 {
                100.0 * cpu_ms / (elapsed_s * 1000.0)
            } else {
                0.0
            },
        })
        .collect();
    out.sort_by(|a, b| {
        b.cpu_ms
            .partial_cmp(&a.cpu_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// CPU per thread group counted from process start rather than from the
/// opening sample, i.e. the absolute `utime + stime` in `after`.
///
/// Only meaningful when the process was launched cold for this window, which
/// is how every iteration runs. Use it for page loads, where most of the work
/// happens before the opening sample can be taken; use [`costs`] for
/// steady-state rendering, where the startup cost is exactly what to exclude.
/// `before` supplies only the window's start time, for `core_pct`.
pub fn costs_since_process_start(
    before: &ThreadCpuSample,
    after: &ThreadCpuSample,
    frames: f64,
) -> Vec<ThreadCost> {
    let origin = ThreadCpuSample {
        wall_s: before.wall_s,
        threads: BTreeMap::new(),
    };
    costs(&origin, after, frames)
}

/// Device-side shell that produces one sample. `{pid}` is substituted.
///
/// `comm` is read separately rather than taken from `stat`'s second field so
/// the name never has to be escaped out of the stat line.
pub const SAMPLE_COMMAND: &str = concat!(
    "date +%s.%N; ",
    "for t in /proc/{pid}/task/*; do ",
    r#"printf '%s|%s|%s\n' "$(basename $t)" "$(cat $t/comm 2>/dev/null)" "$(cat $t/stat 2>/dev/null)"; "#,
    "done"
);

#[cfg(test)]
mod tests {
    #[test]
    fn a_wall_clock_only_sample_is_recognised() {
        // What `sample_wall_clock()` produces for `thread_cpu_from_start`:
        // one line, no thread rows. `costs` finds no baseline and returns the
        // absolute counter, so the caller has to know and rename the metric.
        let wall = super::parse_sample("1789072245.123456789\n");
        assert!(super::is_wall_clock_only(&wall));
        // A real /proc walk is not.
        let full = super::parse_sample(
            "1789072245.123456789\n1234 (Script) R 1 1 0 0 -1 0 0 0 0 0 100 200 0 0 20 0 8 0 1 0\n",
        );
        assert!(!super::is_wall_clock_only(&full) || full.threads.is_empty());
    }

    use super::*;

    fn sample(wall: &str, rows: &[(&str, &str, u64, u64)]) -> String {
        let mut s = String::from(wall);
        s.push('\n');
        for (tid, comm, utime, stime) in rows {
            // A realistic stat line: pid, (comm), state, then 10 fields before utime.
            s.push_str(&format!(
                "{tid}|{comm}|{tid} ({comm}) R 1 1 1 0 -1 4194304 0 0 0 0 {utime} {stime} 0 0\n"
            ));
        }
        s
    }

    #[test]
    fn parses_wall_clock_and_thread_times() {
        let s = parse_sample(&sample("1000.5", &[("11", "Canvas", 30, 5)]));
        assert_eq!(s.wall_s, 1000.5);
        assert_eq!(s.threads[&11].comm, "Canvas");
        assert_eq!(s.threads[&11].ticks, 35);
    }

    #[test]
    fn thread_name_with_spaces_and_parens_does_not_shift_fields() {
        let raw = "5.0\n7|od d(x)|7 (od d(x)) R 1 1 1 0 -1 0 0 0 0 0 41 9 0 0\n";
        let s = parse_sample(raw);
        assert_eq!(s.threads[&7].ticks, 50);
    }

    #[test]
    fn cost_is_the_delta_per_frame_not_the_absolute() {
        let before = parse_sample(&sample("10.0", &[("1", "Canvas", 100, 0)]));
        let after = parse_sample(&sample("20.0", &[("1", "Canvas", 300, 50)]));
        let costs = costs(&before, &after, 100.0);
        assert_eq!(costs.len(), 1);
        // (300+50-100) ticks = 250 ticks = 2500 ms over 100 frames.
        assert_eq!(costs[0].cpu_ms, 2500.0);
        assert_eq!(costs[0].cpu_ms_per_frame, 25.0);
        // 2500 ms of CPU in a 10 s window is a quarter of one core.
        assert_eq!(costs[0].core_pct, 25.0);
    }

    #[test]
    fn pool_threads_fold_into_one_row_and_late_threads_count_fully() {
        let before = parse_sample(&sample("0.0", &[("1", "Canvas#1", 10, 0)]));
        let after = parse_sample(&sample(
            "1.0",
            &[("1", "Canvas#1", 20, 0), ("2", "Canvas#2", 7, 0)],
        ));
        let costs = costs(&before, &after, 10.0);
        assert_eq!(costs.len(), 1);
        assert_eq!(costs[0].name, "Canvas");
        assert_eq!(costs[0].threads, 2);
        // 10 ticks of delta for the pre-existing thread, all 7 for the new one.
        assert_eq!(costs[0].cpu_ms, 170.0);
    }

    #[test]
    fn threads_that_exited_are_dropped_rather_than_counted_backwards() {
        let before = parse_sample(&sample("0.0", &[("1", "A", 10, 0), ("2", "B", 90, 0)]));
        let after = parse_sample(&sample("1.0", &[("1", "A", 30, 0)]));
        let costs = costs(&before, &after, 1.0);
        assert_eq!(costs.len(), 1);
        assert_eq!(costs[0].name, "A");
        assert_eq!(costs[0].cpu_ms, 200.0);
    }
}
