//! What clock the critical threads actually ran at.
//!
//! [`crate::threads`] answers *which* thread spent the CPU time; it cannot say
//! whether that time was expensive because the thread did a lot of work or
//! because it ran slowly. On a big.LITTLE phone those are very different
//! problems with different fixes, and the difference is large: servo's canvas
//! path has been measured running at a quarter of the available clock, which
//! costs more than any code change so far has recovered.
//!
//! A frame handed along a chain of threads is exactly the workload a
//! utilisation-driven governor mis-serves. Each thread is idle while the others
//! work, so no single run queue looks busy and the clocks stay low even though
//! the frame is late. Nothing in a wall-clock or CPU-time metric distinguishes
//! that from "the code is slow", which is why an optimisation can look like a
//! win or a loss purely from where the governor happened to sit.
//!
//! The inputs are already in every OHOS capture: the `freq` tag emits
//! `cpu_frequency`, and `sched` emits `sched_switch`. Intersecting them gives,
//! per thread, the frequency in force over each interval it was on a CPU.

use std::collections::{BTreeMap, HashMap};

/// A `cpu_frequency` event: from `ts_ns` on, `cpu` runs at `khz`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FreqChange {
    ts_ns: u64,
    cpu: u32,
    khz: u64,
}

/// A `sched_switch`: on `cpu` at `ts_ns`, `prev` stopped running and `next` started.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Switch {
    ts_ns: u64,
    cpu: u32,
    prev: u64,
    next: u64,
}

/// The scheduling and frequency events of one capture.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FreqTrace {
    freqs: Vec<FreqChange>,
    switches: Vec<Switch>,
    /// tid → thread name, from whichever events mentioned it.
    comms: HashMap<u64, String>,
    /// tid → pid, for restricting the report to servo's own threads.
    tgids: HashMap<u64, u64>,
}

impl FreqTrace {
    /// Nothing to report at all: without `cpu_frequency` events there is no
    /// clock to attribute.
    pub fn is_empty(&self) -> bool {
        self.freqs.is_empty()
    }

    /// Whether per-thread attribution is possible. Cluster clocks need only the
    /// `freq` tag, which every capture already has; naming the *thread* that
    /// ran slowly additionally needs `sched`, which is high volume and so is
    /// opt-in. A capture without it still answers "did this run have clock
    /// headroom", which is the question that decides whether an A/B means
    /// anything.
    pub fn has_threads(&self) -> bool {
        !self.switches.is_empty()
    }
}

/// One thread group's clock, over the intervals it was actually on a CPU.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadClock {
    /// Thread name, with servo's pool numbering folded away, matching
    /// [`crate::threads::ThreadCost::name`] so the two tables line up.
    pub name: String,
    pub threads: usize,
    /// Seconds spent on a CPU. Small values make the frequency unreliable.
    pub cpu_s: f64,
    /// Time-weighted mean clock while running.
    pub effective_ghz: f64,
    /// That clock as a share of the top frequency offered by the cores it ran
    /// on. This, not the absolute GHz, is the number that says "this thread is
    /// being starved": 100% means the governor gave it everything it had.
    pub pct_of_max: f64,
    /// Share of its running time per cluster, largest first.
    pub cluster_pct: Vec<(String, f64)>,
}

/// One CPU cluster's clock over the whole capture, whoever was running.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterClock {
    pub name: String,
    pub cpus: Vec<u32>,
    pub mean_ghz: f64,
    pub max_ghz: f64,
    /// Share of the window spent at the top frequency. A cluster that is busy
    /// but rarely at its top OPP is one the governor is not ramping.
    pub max_opp_pct: f64,
}

/// Parse `sched_switch` and `cpu_frequency` out of a hitrace text capture.
///
/// Unrelated lines are skipped, so this is safe to run over any capture; a
/// trace taken without the `sched` or `freq` tags simply yields
/// [`FreqTrace::is_empty`].
pub fn parse(text: &str) -> FreqTrace {
    let mut trace = FreqTrace::default();
    for line in text.lines() {
        if let Some(idx) = line.find(": cpu_frequency: ") {
            let Some(head) = header(&line[..idx]) else {
                continue;
            };
            let rest = &line[idx + ": cpu_frequency: ".len()..];
            let (Some(khz), Some(cpu)) = (field(rest, "state="), field(rest, "cpu_id=")) else {
                continue;
            };
            trace.freqs.push(FreqChange {
                ts_ns: head.ts_ns,
                cpu: cpu as u32,
                khz,
            });
        } else if let Some(idx) = line.find(": sched_switch: ") {
            let Some(head) = header(&line[..idx]) else {
                continue;
            };
            let rest = &line[idx + ": sched_switch: ".len()..];
            let (Some(prev), Some(next)) = (field(rest, "prev_pid="), field(rest, "next_pid="))
            else {
                continue;
            };
            if let Some(comm) = between(rest, "prev_comm=", " prev_pid=") {
                trace.comms.insert(prev, comm.to_string());
            }
            if let Some(comm) = between(rest, "next_comm=", " next_pid=") {
                trace.comms.insert(next, comm.to_string());
            }
            trace.switches.push(Switch {
                ts_ns: head.ts_ns,
                cpu: head.cpu,
                prev,
                next,
            });
        } else {
            continue;
        }
        // The ftrace header names the emitting thread and its process, which
        // is the only place a tid → pid mapping appears in a text capture.
        if let Some(head) = header_of_event_line(line) {
            if let (Some(tid), Some(pid)) = (head.tid, head.pid) {
                trace.tgids.insert(tid, pid);
            }
        }
    }
    trace.freqs.sort_by_key(|f| f.ts_ns);
    trace.switches.sort_by_key(|s| s.ts_ns);
    trace
}

struct Header {
    cpu: u32,
    ts_ns: u64,
    tid: Option<u64>,
    pid: Option<u64>,
}

/// Parse the ftrace line prefix, which precedes every event:
/// `<comm>-<tid>  ( <pid>) [cpu] flags <ts_s>.<ts_us>`.
fn header(head: &str) -> Option<Header> {
    let head = head.trim_end();
    let ts_ns = parse_ts_to_ns(head.split_whitespace().last()?)?;
    let open = head.find('[')?;
    let close = head[open..].find(']')? + open;
    let cpu = head[open + 1..close].trim().parse::<u32>().ok()?;
    let first = head.split_whitespace().next()?;
    let tid = first
        .rfind('-')
        .and_then(|i| first[i + 1..].parse::<u64>().ok());
    let pid = between(head, "(", ")").and_then(|s| s.trim().parse::<u64>().ok());
    Some(Header {
        cpu,
        ts_ns,
        tid,
        pid,
    })
}

fn header_of_event_line(line: &str) -> Option<Header> {
    let idx = line
        .find(": cpu_frequency: ")
        .or_else(|| line.find(": sched_switch: "))?;
    header(&line[..idx])
}

/// Read `<key><integer>` out of an ftrace event's field list.
fn field(rest: &str, key: &str) -> Option<u64> {
    let at = rest.find(key)? + key.len();
    let end = rest[at..]
        .find(|c: char| !c.is_ascii_digit())
        .map_or(rest.len(), |i| at + i);
    rest[at..end].parse().ok()
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let at = s.find(open)? + open.len();
    let end = s[at..].find(close)? + at;
    Some(&s[at..end])
}

fn parse_ts_to_ns(s: &str) -> Option<u64> {
    let (sec, sub) = s.split_once('.')?;
    let sec: u64 = sec.parse().ok()?;
    let mut ns = sub.to_string();
    if ns.len() < 9 {
        ns.push_str(&"0".repeat(9 - ns.len()));
    } else {
        ns.truncate(9);
    }
    Some(
        sec.saturating_mul(1_000_000_000)
            .saturating_add(ns.parse::<u64>().ok()?),
    )
}

/// A per-CPU frequency timeline that can be integrated or sliced over an interval.
struct ClockIndex {
    /// cpu → ascending (ts_ns, khz).
    per_cpu: HashMap<u32, Vec<(u64, u64)>>,
}

impl ClockIndex {
    fn new(freqs: &[FreqChange]) -> Self {
        let mut per_cpu: HashMap<u32, Vec<(u64, u64)>> = HashMap::new();
        for f in freqs {
            per_cpu.entry(f.cpu).or_default().push((f.ts_ns, f.khz));
        }
        for v in per_cpu.values_mut() {
            v.sort_unstable();
        }
        ClockIndex { per_cpu }
    }

    /// Frequency-time integral over `[from, to)` on `cpu`, as (nanoseconds
    /// covered, kHz·ns). Time before the first known frequency is not counted,
    /// since guessing it would bias the mean.
    fn integrate(&self, cpu: u32, from: u64, to: u64) -> (u64, u128) {
        let Some(tl) = self.per_cpu.get(&cpu) else {
            return (0, 0);
        };
        if to <= from {
            return (0, 0);
        }
        // Index of the last change at or before `from`.
        let start = match tl.binary_search_by_key(&from, |&(ts, _)| ts) {
            Ok(i) => i,
            Err(0) => {
                // Interval starts before any known frequency; skip that part.
                return self.integrate_from_first(tl, from, to);
            }
            Err(i) => i - 1,
        };
        let mut covered = 0u64;
        let mut cycles = 0u128;
        let mut at = from;
        for i in start..tl.len() {
            let khz = tl[i].1;
            let next = tl.get(i + 1).map_or(to, |&(ts, _)| ts.min(to));
            if next <= at {
                continue;
            }
            let dt = next - at;
            covered += dt;
            cycles += dt as u128 * khz as u128;
            at = next;
            if at >= to {
                break;
            }
        }
        if at < to {
            let khz = tl[tl.len() - 1].1;
            let dt = to - at;
            covered += dt;
            cycles += dt as u128 * khz as u128;
        }
        (covered, cycles)
    }

    fn integrate_from_first(&self, tl: &[(u64, u64)], from: u64, to: u64) -> (u64, u128) {
        let first = tl[0].0;
        if first >= to {
            return (0, 0);
        }
        let _ = from;
        let mut covered = 0u64;
        let mut cycles = 0u128;
        let mut at = first;
        for i in 0..tl.len() {
            let khz = tl[i].1;
            let next = tl.get(i + 1).map_or(to, |&(ts, _)| ts.min(to));
            if next <= at {
                continue;
            }
            let dt = next - at;
            covered += dt;
            cycles += dt as u128 * khz as u128;
            at = next;
            if at >= to {
                break;
            }
        }
        (covered, cycles)
    }

    /// Split `[from, to)` on `cpu` into runs of constant frequency.
    fn segments(&self, cpu: u32, from: u64, to: u64) -> Vec<(u64, u64, u64)> {
        let Some(tl) = self.per_cpu.get(&cpu) else {
            return vec![];
        };
        let mut out = Vec::new();
        let mut at = from;
        while at < to {
            // The change in force at `at` is the last one at or before it.
            let i = match tl.binary_search_by_key(&at, |&(ts, _)| ts) {
                Ok(i) => i,
                Err(0) => {
                    // Before any known frequency; skip to the first one.
                    at = tl[0].0;
                    continue;
                }
                Err(i) => i - 1,
            };
            let next = tl.get(i + 1).map_or(to, |&(ts, _)| ts.min(to));
            if next <= at {
                break;
            }
            out.push((at, next, tl[i].1));
            at = next;
        }
        out
    }

    /// Top frequency ever seen on `cpu`, i.e. its cluster's maximum OPP.
    fn max_khz(&self, cpu: u32) -> u64 {
        self.per_cpu
            .get(&cpu)
            .map(|v| v.iter().map(|&(_, k)| k).max().unwrap_or(0))
            .unwrap_or(0)
    }
}

/// Group CPUs into clusters by the set of frequencies they were observed at.
///
/// Deriving this from the trace rather than from a hardcoded core map keeps it
/// correct on any device: cores that share a clock domain share an OPP table.
fn clusters(timeline: &ClockIndex) -> Vec<(String, Vec<u32>)> {
    let mut by_opps: BTreeMap<Vec<u64>, Vec<u32>> = BTreeMap::new();
    for (&cpu, changes) in &timeline.per_cpu {
        let mut opps: Vec<u64> = changes.iter().map(|&(_, k)| k).collect();
        opps.sort_unstable();
        opps.dedup();
        by_opps.entry(opps).or_default().push(cpu);
    }
    let mut groups: Vec<(u64, Vec<u32>)> = by_opps
        .into_iter()
        .map(|(opps, mut cpus)| {
            cpus.sort_unstable();
            (opps.last().copied().unwrap_or(0), cpus)
        })
        .collect();
    // Ascending by top OPP, so the weakest cluster is first.
    groups.sort_by_key(|(max, _)| *max);
    let names: &[&str] = match groups.len() {
        1 => &["cpu"],
        2 => &["little", "big"],
        3 => &["little", "mid", "big"],
        _ => &[],
    };
    groups
        .into_iter()
        .enumerate()
        .map(|(i, (_, cpus))| {
            let name = names
                .get(i)
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("cluster{i}"));
            (name, cpus)
        })
        .collect()
}

/// The process id servo's threads belong to, or `None` if the capture has no
/// thread named after the servo binary.
///
/// A system-wide capture contains every process on the device, so the thread
/// table has to be narrowed to ours. The main thread carries the process name,
/// which is the only handle a text capture gives us.
pub fn servo_pid(trace: &FreqTrace) -> Option<u64> {
    const NAMES: [&str; 2] = ["org.servo", "servoshell"];
    let mut votes: HashMap<u64, usize> = HashMap::new();
    for (tid, comm) in &trace.comms {
        if NAMES.iter().any(|n| comm.starts_with(n)) {
            if let Some(pid) = trace.tgids.get(tid) {
                *votes.entry(*pid).or_insert(0) += 1;
            }
        }
    }
    votes
        .into_iter()
        .max_by_key(|&(_, n)| n)
        .map(|(pid, _)| pid)
}

/// Per-thread and per-cluster clocks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClockReport {
    pub threads: Vec<ThreadClock>,
    pub clusters: Vec<ClusterClock>,
}

/// Attribute every on-CPU interval in the trace to a thread, at the frequency
/// in force over that interval.
///
/// `pid` restricts the thread table to one process; pass `None` to include
/// every thread in the capture. The cluster table is always system-wide, since
/// the clock is a property of the machine, not of the process asking.
pub fn analyse(trace: &FreqTrace, pid: Option<u64>) -> ClockReport {
    if trace.is_empty() {
        return ClockReport::default();
    }
    let timeline = ClockIndex::new(&trace.freqs);
    let clusters = clusters(&timeline);
    let cluster_of: HashMap<u32, String> = clusters
        .iter()
        .flat_map(|(name, cpus)| cpus.iter().map(move |&c| (c, name.clone())))
        .collect();

    // Per tid: running ns, kHz·ns, per-cluster ns, and the top OPP it could
    // have had (weighted by where it ran).
    struct Acc {
        ns: u64,
        cycles: u128,
        max_cycles: u128,
        per_cluster: BTreeMap<String, u64>,
    }
    let mut acc: HashMap<u64, Acc> = HashMap::new();
    // cpu → (tid, since_ns) for whoever is currently on it.
    let mut running: HashMap<u32, (u64, u64)> = HashMap::new();

    for sw in &trace.switches {
        if let Some(&(tid, since)) = running.get(&sw.cpu) {
            // The thread leaving the CPU must be the one we recorded, or we
            // missed an event and the interval is not attributable.
            if tid != 0 && tid == sw.prev {
                let keep = pid.is_none_or(|p| trace.tgids.get(&tid) == Some(&p));
                if keep {
                    let (covered, cycles) = timeline.integrate(sw.cpu, since, sw.ts_ns);
                    if covered > 0 {
                        let e = acc.entry(tid).or_insert_with(|| Acc {
                            ns: 0,
                            cycles: 0,
                            max_cycles: 0,
                            per_cluster: BTreeMap::new(),
                        });
                        e.ns += covered;
                        e.cycles += cycles;
                        e.max_cycles += covered as u128 * timeline.max_khz(sw.cpu) as u128;
                        if let Some(cluster) = cluster_of.get(&sw.cpu) {
                            *e.per_cluster.entry(cluster.clone()).or_insert(0) += covered;
                        }
                    }
                }
            }
        }
        running.insert(sw.cpu, (sw.next, sw.ts_ns));
    }

    // Fold pool threads together the same way the CPU-time table does.
    let mut by_group: BTreeMap<String, (u64, u128, u128, BTreeMap<String, u64>, usize)> =
        BTreeMap::new();
    for (tid, a) in &acc {
        let comm = trace.comms.get(tid).cloned().unwrap_or_default();
        let name = crate::threads::group_name(&comm);
        let e = by_group
            .entry(name)
            .or_insert((0, 0, 0, BTreeMap::new(), 0));
        e.0 += a.ns;
        e.1 += a.cycles;
        e.2 += a.max_cycles;
        for (cluster, ns) in &a.per_cluster {
            *e.3.entry(cluster.clone()).or_insert(0) += ns;
        }
        e.4 += 1;
    }

    let mut threads: Vec<ThreadClock> = by_group
        .into_iter()
        .map(|(name, (ns, cycles, max_cycles, per_cluster, count))| {
            let mut cluster_pct: Vec<(String, f64)> = per_cluster
                .into_iter()
                .map(|(c, v)| (c, 100.0 * v as f64 / ns as f64))
                .collect();
            cluster_pct.sort_by(|a, b| b.1.total_cmp(&a.1));
            ThreadClock {
                name,
                threads: count,
                cpu_s: ns as f64 / 1e9,
                effective_ghz: cycles as f64 / ns as f64 / 1e6,
                pct_of_max: if max_cycles > 0 {
                    100.0 * cycles as f64 / max_cycles as f64
                } else {
                    0.0
                },
                cluster_pct,
            }
        })
        .collect();
    threads.sort_by(|a, b| b.cpu_s.total_cmp(&a.cpu_s));

    // Cluster residency over the window the trace actually covers. Prefer the
    // scheduling events, which bracket the busy part; fall back to the
    // frequency events when the capture had no `sched` tag.
    let (from, to) = match (trace.switches.first(), trace.switches.last()) {
        (Some(first), Some(last)) => (first.ts_ns, last.ts_ns),
        _ => (
            trace.freqs.first().map(|f| f.ts_ns).unwrap_or(0),
            trace.freqs.last().map(|f| f.ts_ns).unwrap_or(0),
        ),
    };
    let cluster_rows = clusters
        .iter()
        .map(|(name, cpus)| {
            let mut ns = 0u64;
            let mut cycles = 0u128;
            let mut at_max = 0u64;
            let max_khz = cpus.iter().map(|&c| timeline.max_khz(c)).max().unwrap_or(0);
            for &cpu in cpus {
                let (covered, c) = timeline.integrate(cpu, from, to);
                ns += covered;
                cycles += c;
                at_max += time_at(&timeline, cpu, from, to, max_khz);
            }
            ClusterClock {
                name: name.clone(),
                cpus: cpus.clone(),
                mean_ghz: if ns > 0 {
                    cycles as f64 / ns as f64 / 1e6
                } else {
                    0.0
                },
                max_ghz: max_khz as f64 / 1e6,
                max_opp_pct: if ns > 0 {
                    100.0 * at_max as f64 / ns as f64
                } else {
                    0.0
                },
            }
        })
        .collect();

    ClockReport {
        threads,
        clusters: cluster_rows,
    }
}

/// Nanoseconds `cpu` spent at exactly `khz` within `[from, to)`.
fn time_at(timeline: &ClockIndex, cpu: u32, from: u64, to: u64, khz: u64) -> u64 {
    let Some(tl) = timeline.per_cpu.get(&cpu) else {
        return 0;
    };
    let mut total = 0u64;
    for (i, &(ts, k)) in tl.iter().enumerate() {
        if k != khz {
            continue;
        }
        let start = ts.max(from);
        let end = tl.get(i + 1).map_or(to, |&(n, _)| n.min(to));
        if end > start {
            total += end - start;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[cpu]` and the `cpu_id=` field can disagree; the event's own field is
    /// the one that says which CPU changed frequency.
    const TRACE: &str = "\
# tracer: nop
          <idle>-0     (-------) [000] d..2 100.000000: cpu_frequency: state=500000 cpu_id=0
          <idle>-0     (-------) [004] d..2 100.000000: cpu_frequency: state=2000000 cpu_id=4
          <idle>-0     (-------) [000] d..3 100.000000: sched_switch: prev_comm=swapper/0 prev_pid=0 prev_prio=120 prev_state=R ==> next_comm=Canvas next_pid=11 next_prio=120
          Canvas-11    (   10) [000] d..3 100.100000: sched_switch: prev_comm=Canvas prev_pid=11 prev_prio=120 prev_state=S ==> next_comm=swapper/0 next_pid=0 next_prio=120
          <idle>-0     (-------) [004] d..3 100.100000: sched_switch: prev_comm=swapper/0 prev_pid=0 prev_prio=120 prev_state=R ==> next_comm=Script#1 next_pid=12 next_prio=120
        Script#1-12    (   10) [004] d..3 100.200000: sched_switch: prev_comm=Script#1 prev_pid=12 prev_prio=120 prev_state=S ==> next_comm=swapper/0 next_pid=0 next_prio=120
";

    #[test]
    fn parses_both_event_kinds() {
        let t = parse(TRACE);
        assert_eq!(t.freqs.len(), 2);
        assert_eq!(t.switches.len(), 4);
        assert_eq!(t.comms[&11], "Canvas");
        assert_eq!(t.tgids[&11], 10);
    }

    #[test]
    fn attributes_each_thread_the_clock_of_the_cpu_it_ran_on() {
        let r = analyse(&parse(TRACE), None);
        let canvas = r.threads.iter().find(|t| t.name == "Canvas").unwrap();
        let script = r.threads.iter().find(|t| t.name == "Script").unwrap();
        assert!((canvas.cpu_s - 0.1).abs() < 1e-9);
        assert!((canvas.effective_ghz - 0.5).abs() < 1e-9);
        assert!((script.effective_ghz - 2.0).abs() < 1e-9);
        // Each ran at the only frequency its cluster was ever seen at.
        assert!((canvas.pct_of_max - 100.0).abs() < 1e-6);
    }

    #[test]
    fn a_thread_below_its_cluster_ceiling_reports_the_shortfall() {
        let trace = format!(
            "{TRACE}          <idle>-0     (-------) [000] d..2 100.300000: cpu_frequency: \
             state=2000000 cpu_id=0\n"
        );
        let r = analyse(&parse(&trace), None);
        let canvas = r.threads.iter().find(|t| t.name == "Canvas").unwrap();
        // Ran the whole time at 500 MHz on a cluster that reaches 2 GHz.
        assert!((canvas.pct_of_max - 25.0).abs() < 1e-6);
    }

    #[test]
    fn clusters_are_derived_from_the_observed_frequency_sets() {
        let r = analyse(&parse(TRACE), None);
        let names: Vec<&str> = r.clusters.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["little", "big"]);
        assert_eq!(r.clusters[0].cpus, vec![0]);
        assert_eq!(r.clusters[1].cpus, vec![4]);
        assert!((r.clusters[1].max_ghz - 2.0).abs() < 1e-9);
    }

    #[test]
    fn frequency_changes_inside_an_interval_are_integrated_not_snapshotted() {
        // One thread runs 100ms, with the clock doubling halfway through.
        let trace = "\
          <idle>-0     (-------) [000] d..2 10.000000: cpu_frequency: state=1000000 cpu_id=0
          <idle>-0     (-------) [000] d..3 10.000000: sched_switch: prev_comm=swapper/0 prev_pid=0 prev_prio=120 prev_state=R ==> next_comm=Canvas next_pid=11 next_prio=120
          <idle>-0     (-------) [000] d..2 10.050000: cpu_frequency: state=3000000 cpu_id=0
          Canvas-11    (   10) [000] d..3 10.100000: sched_switch: prev_comm=Canvas prev_pid=11 prev_prio=120 prev_state=S ==> next_comm=swapper/0 next_pid=0 next_prio=120
";
        let r = analyse(&parse(trace), None);
        let canvas = r.threads.iter().find(|t| t.name == "Canvas").unwrap();
        assert!((canvas.effective_ghz - 2.0).abs() < 1e-6);
    }

    #[test]
    fn a_pid_filter_keeps_only_that_process() {
        let t = parse(TRACE);
        assert_eq!(analyse(&t, Some(10)).threads.len(), 2);
        assert!(analyse(&t, Some(999)).threads.is_empty());
    }

    /// The `sched` tag is high volume and opt-in, so the common case is a
    /// capture with frequencies but no scheduling events. That must still
    /// answer "did this run have clock headroom".
    #[test]
    fn a_timeline_splits_a_slice_at_each_frequency_change() {
        let trace = "\
          <idle>-0     (-------) [000] d..2 10.000000: cpu_frequency: state=1000000 cpu_id=0
          <idle>-0     (-------) [000] d..3 10.000000: sched_switch: prev_comm=swapper/0 prev_pid=0 prev_prio=120 prev_state=R ==> next_comm=Canvas next_pid=11 next_prio=120
          <idle>-0     (-------) [000] d..2 10.050000: cpu_frequency: state=3000000 cpu_id=0
          Canvas-11    (   10) [000] d..3 10.100000: sched_switch: prev_comm=Canvas prev_pid=11 prev_prio=120 prev_state=S ==> next_comm=swapper/0 next_pid=0 next_prio=120
";
        let tl = timeline(&parse(trace), None, &["Canvas"], 100);
        assert_eq!(tl.threads.len(), 1);
        // One 100 ms run, cut in two by the clock doubling halfway.
        let slices = &tl.threads[0].slices;
        assert_eq!(slices.len(), 2);
        assert_eq!((slices[0].khz, slices[1].khz), (1_000_000, 3_000_000));
        assert_eq!(slices[0].d, 50_000_000);
        assert_eq!(slices[0].cluster, "cpu");
    }

    #[test]
    fn a_capture_without_the_sched_tag_still_reports_clusters() {
        let no_sched: String = TRACE
            .lines()
            .filter(|l| !l.contains("sched_switch"))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed = parse(&no_sched);
        assert!(!parsed.is_empty());
        assert!(!parsed.has_threads());
        let r = analyse(&parsed, None);
        assert!(r.threads.is_empty());
        assert_eq!(r.clusters.len(), 2);
        assert!((r.clusters[1].max_ghz - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_capture_without_the_freq_tag_yields_nothing_rather_than_zeroes() {
        let no_freq: String = TRACE
            .lines()
            .filter(|l| !l.contains("cpu_frequency"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse(&no_freq).is_empty());
        assert_eq!(analyse(&parse(&no_freq), None), ClockReport::default());
    }
}

/// Ad-hoc report for one capture, used by `servoperf cpufreq`.
pub fn run(args: &crate::cli::CpuFreqArgs) -> anyhow::Result<()> {
    print_report(&args.trace)?;
    let Some(out) = args.timeline.as_ref() else {
        return Ok(());
    };
    let text = std::fs::read_to_string(&args.trace)?;
    let parsed = parse(&text);
    let want: Vec<&str> = args
        .timeline_threads
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let tl = timeline(&parsed, servo_pid(&parsed), &want, args.timeline_ms);
    std::fs::write(out, serde_json::to_string(&tl)?)?;
    println!(
        "\nwrote {} ({} threads, {} ms window)",
        out.display(),
        tl.threads.len(),
        args.timeline_ms
    );
    Ok(())
}

fn print_report(path: &std::path::Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let parsed = parse(&text);
    if parsed.is_empty() {
        println!(
            "(no sched_switch/cpu_frequency events: capture needs the `sched` and `freq` tags)"
        );
        return Ok(());
    }
    let pid = servo_pid(&parsed);
    let report = analyse(&parsed, pid);
    println!("servo pid: {pid:?}");
    println!(
        "\n{:<22} {:>6} {:>8} {:>9} {:>8}  {}",
        "thread", "n", "cpu_s", "eff_GHz", "%of_max", "clusters"
    );
    for t in report.threads.iter().take(15) {
        let clusters = t
            .cluster_pct
            .iter()
            .map(|(c, p)| format!("{c} {p:.0}%"))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "{:<22} {:>6} {:>8.2} {:>9.3} {:>7.0}%  {}",
            t.name, t.threads, t.cpu_s, t.effective_ghz, t.pct_of_max, clusters
        );
    }
    println!(
        "\n{:<10} {:>18} {:>10} {:>10} {:>12}",
        "cluster", "cpus", "mean_GHz", "max_GHz", "max_OPP_%"
    );
    for c in &report.clusters {
        let cpus = c
            .cpus
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{:<10} {:>18} {:>10.3} {:>10.3} {:>11.1}%",
            c.name, cpus, c.mean_ghz, c.max_ghz, c.max_opp_pct
        );
    }
    Ok(())
}

/// One interval a thread spent on a CPU, at a known clock.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TimelineSlice {
    /// Nanoseconds from the start of the window.
    pub t: u64,
    pub d: u64,
    pub cpu: u32,
    pub khz: u64,
    pub cluster: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TimelineThread {
    pub name: String,
    pub tid: u64,
    pub slices: Vec<TimelineSlice>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TimelineCluster {
    pub name: String,
    pub cpus: Vec<u32>,
    pub max_khz: u64,
    /// Step points: from `t`, the cluster's CPUs were seen at `khz`.
    pub freq: Vec<(u64, u64)>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Timeline {
    pub span_ns: u64,
    pub threads: Vec<TimelineThread>,
    pub clusters: Vec<TimelineCluster>,
}

/// Extract a scheduling + frequency timeline for the named threads.
///
/// Shows the two things a frame-time number hides: which threads were able to
/// run at the same time, and what clock each was given while it ran. `window_ms`
/// is taken from the busiest part of the capture, so the picture is of the
/// steady state rather than of start-up.
pub fn timeline(trace: &FreqTrace, pid: Option<u64>, want: &[&str], window_ms: u64) -> Timeline {
    let timeline = ClockIndex::new(&trace.freqs);
    let clusters = clusters(&timeline);
    let cluster_of: HashMap<u32, String> = clusters
        .iter()
        .flat_map(|(name, cpus)| cpus.iter().map(move |&c| (c, name.clone())))
        .collect();

    let keep = |tid: u64| -> bool {
        if pid.is_some_and(|p| trace.tgids.get(&tid) != Some(&p)) {
            return false;
        }
        let comm = trace.comms.get(&tid).map(String::as_str).unwrap_or("");
        want.is_empty() || want.iter().any(|w| comm.starts_with(w))
    };

    // Every on-CPU interval of the threads of interest, in trace order.
    let mut all: Vec<(u64, u64, u64, u32)> = Vec::new(); // (tid, start, end, cpu)
    let mut running: HashMap<u32, (u64, u64)> = HashMap::new();
    for sw in &trace.switches {
        if let Some(&(tid, since)) = running.get(&sw.cpu) {
            if tid != 0 && tid == sw.prev && sw.ts_ns > since && keep(tid) {
                all.push((tid, since, sw.ts_ns, sw.cpu));
            }
        }
        running.insert(sw.cpu, (sw.next, sw.ts_ns));
    }
    if all.is_empty() {
        return Timeline {
            span_ns: 0,
            threads: vec![],
            clusters: vec![],
        };
    }

    // Choose the window with the most on-CPU time, so the picture is of the
    // steady state and not of an idle stretch.
    let window_ns = window_ms * 1_000_000;
    let (first, last) = (all[0].1, all[all.len() - 1].2);
    let mut best = (first, 0u64);
    let mut start = first;
    while start + window_ns <= last.max(first + window_ns) {
        let busy: u64 = all
            .iter()
            .filter(|(_, s, e, _)| *e > start && *s < start + window_ns)
            .map(|(_, s, e, _)| e.min(&(start + window_ns)) - s.max(&start))
            .sum();
        if busy > best.1 {
            best = (start, busy);
        }
        start += window_ns / 4;
    }
    let (from, to) = (best.0, best.0 + window_ns);

    let mut by_tid: BTreeMap<u64, Vec<TimelineSlice>> = BTreeMap::new();
    for (tid, s, e, cpu) in &all {
        let (s, e) = ((*s).max(from), (*e).min(to));
        if e <= s {
            continue;
        }
        // Split at each frequency change so a bar never spans two clocks.
        for (seg_start, seg_end, khz) in timeline.segments(*cpu, s, e) {
            by_tid.entry(*tid).or_default().push(TimelineSlice {
                t: seg_start - from,
                d: seg_end - seg_start,
                cpu: *cpu,
                khz,
                cluster: cluster_of.get(cpu).cloned().unwrap_or_default(),
            });
        }
    }

    let mut threads: Vec<TimelineThread> = by_tid
        .into_iter()
        .map(|(tid, slices)| TimelineThread {
            name: trace
                .comms
                .get(&tid)
                .cloned()
                .unwrap_or_else(|| format!("tid:{tid}")),
            tid,
            slices,
        })
        .collect();
    threads.sort_by_key(|t| std::cmp::Reverse(t.slices.iter().map(|s| s.d).sum::<u64>()));

    let cluster_rows = clusters
        .iter()
        .map(|(name, cpus)| {
            let max_khz = cpus.iter().map(|&c| timeline.max_khz(c)).max().unwrap_or(0);
            // One representative CPU's steps is enough: a cluster shares a clock.
            let mut freq: Vec<(u64, u64)> = Vec::new();
            if let Some(&cpu) = cpus.first() {
                for (s, _e, khz) in timeline.segments(cpu, from, to) {
                    freq.push((s - from, khz));
                }
            }
            TimelineCluster {
                name: name.clone(),
                cpus: cpus.clone(),
                max_khz,
                freq,
            }
        })
        .collect();

    Timeline {
        span_ns: window_ns,
        threads,
        clusters: cluster_rows,
    }
}
