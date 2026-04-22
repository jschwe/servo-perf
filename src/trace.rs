//! Trace parsing and critical-path extraction.
//!
//! Public API:
//! - [`parse`] — decodes a `.pftrace` file into a `Vec<Slice>`.
//! - [`load_registry`] — loads the shared `_critical_path.toml`.
//! - [`analyse`] — critical-path attribution.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Phase {
    pub name: String,
    pub owner_thread: String,
    #[serde(default)]
    pub is_milestone: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub expected_gap_ms: f64,
    pub flag_threshold_ms: f64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SpanRegistry {
    #[serde(default, rename = "phase")]
    pub phases: Vec<Phase>,
    #[serde(default, rename = "edge")]
    pub edges: Vec<Edge>,
}

/// Load the shared critical-path registry from
/// `<workloads_dir>/_critical_path.toml`.
pub fn load_registry(workloads_dir: &Path) -> Result<SpanRegistry> {
    let path = workloads_dir.join("_critical_path.toml");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading critical-path registry at {}", path.display()))?;
    let r: SpanRegistry = toml::from_str(&text)
        .with_context(|| format!("parsing critical-path registry at {}", path.display()))?;
    // Cross-check: each edge endpoint must be a declared phase.
    for e in &r.edges {
        if !r.phases.iter().any(|p| p.name == e.from) {
            anyhow::bail!("edge `from` refers to undeclared phase: {}", e.from);
        }
        if !r.phases.iter().any(|p| p.name == e.to) {
            anyhow::bail!("edge `to` refers to undeclared phase: {}", e.to);
        }
    }
    Ok(r)
}

// --- Trace parsing ------------------------------------------------------

use crate::proto::{
    InternedData, TrackEvent,
    perfetto_protos::track_event::NameField,
    perfetto_protos::{trace_packet, Trace},
};
use prost::Message;
use std::collections::HashMap;

/// One completed span from a pftrace, materialised.
#[derive(Debug, Clone, PartialEq)]
pub struct Slice {
    pub name: String,
    pub thread: String,
    pub ts_ns: u64,
    pub dur_ns: u64,
}

/// Parse a `.pftrace` file into a `Vec<Slice>`.
///
/// Handles Perfetto's track-event protocol: BEGIN/END pairs keyed by
/// (track_uuid, name) on the same track, with interned string names
/// resolved via the interning tables.
pub fn parse(path: &Path) -> Result<Vec<Slice>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading pftrace at {}", path.display()))?;

    let trace = Trace::decode(bytes.as_slice())
        .with_context(|| format!("decoding Trace from {}", path.display()))?;

    // Resolve interned strings per trusted_packet_sequence_id.
    let mut interned_names: HashMap<(u32, u64), String> = HashMap::new();
    // Track thread name by (pid, tid).
    let mut thread_names: HashMap<(i32, i32), String> = HashMap::new();
    // Map track_uuid → (pid, tid).
    let mut track_to_thread: HashMap<u64, (i32, i32)> = HashMap::new();
    // Open begin events keyed by (track_uuid, name_resolved).
    let mut open: HashMap<(u64, String), u64> = HashMap::new();

    let mut slices = Vec::new();
    for pkt in trace.packet.iter() {
        let seq = match pkt.optional_trusted_packet_sequence_id.as_ref() {
            Some(trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(id)) => {
                *id
            }
            None => 0,
        };

        // Update interning table if present.
        if let Some(id) = pkt.interned_data.as_ref() {
            update_interned(seq, id, &mut interned_names);
        }

        // Track / thread descriptors (TrackDescriptor lives in the data oneof).
        if let Some(trace_packet::Data::TrackDescriptor(td)) = pkt.data.as_ref() {
            if let Some(thread) = td.thread.as_ref() {
                let pid = thread.pid.unwrap_or(0);
                let tid = thread.tid.unwrap_or(0);
                if let Some(name) = thread.thread_name.as_ref() {
                    thread_names.insert((pid, tid), name.clone());
                }
                track_to_thread.insert(td.uuid.unwrap_or(0), (pid, tid));
            }
        }

        // Track event.
        if let Some(trace_packet::Data::TrackEvent(te)) = pkt.data.as_ref() {
            let ts = pkt.timestamp.unwrap_or(0);
            let name = resolve_name(seq, te, &interned_names);
            let track_uuid = te.track_uuid.unwrap_or(0);

            use crate::proto::perfetto_protos::track_event::Type as TE;
            let event_type = te.r#type.unwrap_or(TE::Unspecified as i32);

            if event_type == TE::SliceBegin as i32 {
                open.insert((track_uuid, name), ts);
            } else if event_type == TE::SliceEnd as i32 {
                if let Some(begin_ts) = open.remove(&(track_uuid, name.clone())) {
                    let thread = track_to_thread
                        .get(&track_uuid)
                        .and_then(|pt| thread_names.get(pt))
                        .cloned()
                        .unwrap_or_else(|| "?".to_string());
                    slices.push(Slice {
                        name,
                        thread,
                        ts_ns: begin_ts,
                        dur_ns: ts.saturating_sub(begin_ts),
                    });
                }
            } else if event_type == TE::Instant as i32 {
                let thread = track_to_thread
                    .get(&track_uuid)
                    .and_then(|pt| thread_names.get(pt))
                    .cloned()
                    .unwrap_or_else(|| "?".to_string());
                slices.push(Slice { name, thread, ts_ns: ts, dur_ns: 0 });
            }
        }
    }

    // Normalise: sort by ts so downstream code can walk in order.
    slices.sort_by_key(|s| s.ts_ns);
    Ok(slices)
}

fn update_interned(seq: u32, id: &InternedData, out: &mut HashMap<(u32, u64), String>) {
    for e in id.event_names.iter() {
        if let Some(name) = e.name.as_ref() {
            out.insert((seq, e.iid.unwrap_or(0)), name.clone());
        }
    }
}

fn resolve_name(seq: u32, te: &TrackEvent, interned: &HashMap<(u32, u64), String>) -> String {
    match te.name_field.as_ref() {
        Some(NameField::Name(n)) => n.clone(),
        Some(NameField::NameIid(iid)) => interned
            .get(&(seq, *iid))
            .cloned()
            .unwrap_or_else(|| format!("<name_iid={}>", iid)),
        None => "<anon>".to_string(),
    }
}

// --- Critical path analysis --------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NamedSpanRow {
    pub name: String,
    pub ts_ms: f64,
    pub dur_ms: f64,
    pub thread: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Milestone {
    pub name: String,
    pub ts_ms: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FlaggedGap {
    pub from: String,
    pub to: String,
    pub actual_gap_ms: f64,
    pub threshold_ms: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct CriticalPathReport {
    pub named_spans: Vec<NamedSpanRow>,
    pub milestones: Vec<Milestone>,
    pub gaps: Vec<FlaggedGap>,
}

/// Produce a critical-path report for the given slices against the registry.
///
/// * Named spans: for each phase in the registry, the *first* slice with that
///   name (by ts) is used. Later duplicates are ignored — startup phases
///   occur at most once.
/// * Milestones: phases flagged `is_milestone = true` become standalone
///   entries (their duration is ignored; only the start time matters).
/// * Gaps: for each registry edge `from → to`, compute `to.ts_ns -
///   (from.ts_ns + from.dur_ns)` and flag if above `flag_threshold_ms`.
///
/// All `ts_ms` values in the returned report are **trace-relative** — the
/// smallest `ts_ns` across all input slices is treated as time zero. Raw
/// perfetto timestamps are a platform clock (usually `CLOCK_BOOTTIME` /
/// `CLOCK_MONOTONIC`) whose absolute value is meaningless outside the run.
pub fn analyse(slices: &[Slice], registry: &SpanRegistry) -> CriticalPathReport {
    // Pick the slice with the smallest ts_ns per name. This makes `analyse`
    // correct regardless of the caller's input order — callers aren't
    // required to sort before calling us.
    let first_by_name: HashMap<&str, &Slice> =
        slices.iter().fold(HashMap::new(), |mut acc, s| {
            acc.entry(s.name.as_str())
                .and_modify(|existing: &mut &Slice| {
                    if s.ts_ns < existing.ts_ns {
                        *existing = s;
                    }
                })
                .or_insert(s);
            acc
        });

    // Rebase everything to the trace's earliest timestamp so ts_ms is the
    // duration-from-trace-start, not an absolute wallclock value.
    let t0_ns: u64 = slices.iter().map(|s| s.ts_ns).min().unwrap_or(0);
    let rel_ms = |ns: u64| (ns.saturating_sub(t0_ns)) as f64 / 1_000_000.0;

    let mut report = CriticalPathReport::default();

    for phase in &registry.phases {
        if let Some(s) = first_by_name.get(phase.name.as_str()) {
            let ts_ms = rel_ms(s.ts_ns);
            let dur_ms = s.dur_ns as f64 / 1_000_000.0;
            if phase.is_milestone {
                report.milestones.push(Milestone { name: phase.name.clone(), ts_ms });
            } else {
                report.named_spans.push(NamedSpanRow {
                    name: phase.name.clone(),
                    ts_ms,
                    dur_ms,
                    thread: s.thread.clone(),
                });
            }
        }
    }

    for edge in &registry.edges {
        let from = first_by_name.get(edge.from.as_str());
        let to = first_by_name.get(edge.to.as_str());
        if let (Some(f), Some(t)) = (from, to) {
            let gap_ns = t.ts_ns.saturating_sub(f.ts_ns + f.dur_ns);
            let gap_ms = gap_ns as f64 / 1_000_000.0;
            if gap_ms > edge.flag_threshold_ms {
                report.gaps.push(FlaggedGap {
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                    actual_gap_ms: gap_ms,
                    threshold_ms: edge.flag_threshold_ms,
                });
            }
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_checked_in_registry() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("workloads");
        let r = load_registry(&dir).expect("load registry");
        assert!(!r.phases.is_empty());
        assert!(r.phases.iter().any(|p| p.name == "FirstContentfulPaint"));
        assert!(r.phases.iter().any(|p| p.name == "ScriptThread::new"));
        for e in &r.edges {
            assert!(e.flag_threshold_ms >= e.expected_gap_ms);
        }
    }

    #[test]
    fn parse_minimal_pftrace_fixture() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("minimal.pftrace");
        let slices = super::parse(&path).expect("parse minimal");
        assert_eq!(slices.len(), 2, "expected 1 span + 1 instant, got {slices:?}");
        assert_eq!(slices[0].name, "A");
        assert_eq!(slices[0].thread, "main");
        assert_eq!(slices[0].ts_ns, 100);
        assert_eq!(slices[0].dur_ns, 100);
        assert_eq!(slices[1].name, "FirstContentfulPaint");
        assert_eq!(slices[1].dur_ns, 0);
    }

    #[test]
    fn rejects_edge_referring_to_unknown_phase() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("_critical_path.toml"),
            r#"
[[phase]]
name = "A"
owner_thread = "main"

[[edge]]
from = "A"
to = "B"
expected_gap_ms = 1
flag_threshold_ms = 10
"#,
        )
        .unwrap();
        let err = load_registry(dir.path()).unwrap_err();
        assert!(err.to_string().contains("undeclared phase"));
    }

    fn slice(name: &str, ts_ms: f64, dur_ms: f64) -> Slice {
        Slice {
            name: name.into(),
            thread: "main".into(),
            ts_ns: (ts_ms * 1_000_000.0) as u64,
            dur_ns: (dur_ms * 1_000_000.0) as u64,
        }
    }

    fn simple_registry() -> SpanRegistry {
        SpanRegistry {
            phases: vec![
                Phase { name: "A".into(), owner_thread: "main".into(), is_milestone: false },
                Phase { name: "B".into(), owner_thread: "main".into(), is_milestone: false },
                Phase { name: "FirstContentfulPaint".into(), owner_thread: "main".into(), is_milestone: true },
            ],
            edges: vec![
                Edge { from: "A".into(), to: "B".into(), expected_gap_ms: 1.0, flag_threshold_ms: 10.0 },
            ],
        }
    }

    #[test]
    fn analyse_produces_named_spans_and_milestones() {
        let slices = vec![
            slice("A", 5.0, 2.0),
            slice("B", 10.0, 4.0),
            slice("FirstContentfulPaint", 20.0, 0.0),
        ];
        let r = super::analyse(&slices, &simple_registry());
        assert_eq!(r.named_spans.len(), 2);
        assert_eq!(r.named_spans[0].name, "A");
        assert_eq!(r.named_spans[0].dur_ms, 2.0);
        // ts_ms is trace-relative: t0 = 5 ms (earliest slice), so A = 0 ms.
        assert_eq!(r.named_spans[0].ts_ms, 0.0);
        assert_eq!(r.named_spans[1].ts_ms, 5.0);
        assert_eq!(r.milestones.len(), 1);
        assert_eq!(r.milestones[0].name, "FirstContentfulPaint");
        // t0 = 5 ms → FCP at 20 ms in the source → 15 ms relative.
        assert_eq!(r.milestones[0].ts_ms, 15.0);
    }

    #[test]
    fn analyse_flags_gap_over_threshold() {
        // A ends at 7 ms, B starts at 20 ms → gap = 13 ms, threshold = 10 ms.
        let slices = vec![slice("A", 5.0, 2.0), slice("B", 20.0, 1.0)];
        let r = super::analyse(&slices, &simple_registry());
        assert_eq!(r.gaps.len(), 1);
        assert_eq!(r.gaps[0].from, "A");
        assert_eq!(r.gaps[0].to, "B");
        assert!((r.gaps[0].actual_gap_ms - 13.0).abs() < 0.01);
    }

    #[test]
    fn analyse_stays_silent_on_within_threshold_gap() {
        // A ends at 7 ms, B starts at 15 ms → gap = 8 ms < 10 ms.
        let slices = vec![slice("A", 5.0, 2.0), slice("B", 15.0, 1.0)];
        let r = super::analyse(&slices, &simple_registry());
        assert!(r.gaps.is_empty());
    }

    #[test]
    fn analyse_skips_missing_phases() {
        let slices = vec![slice("A", 5.0, 2.0)]; // no B, no FCP
        let r = super::analyse(&slices, &simple_registry());
        assert_eq!(r.named_spans.len(), 1);
        assert!(r.milestones.is_empty());
        assert!(r.gaps.is_empty());
    }

    #[test]
    fn analyse_picks_smallest_ts_when_duplicates_exist() {
        // Two slices named "A" — the later-in-vec one has earlier ts_ns.
        // The analyser should pick the earliest by ts, not the first in
        // iteration order.
        let slices = vec![
            slice("A", 10.0, 2.0),
            slice("A",  3.0, 1.0),
        ];
        let r = super::analyse(&slices, &simple_registry());
        assert_eq!(r.named_spans.len(), 1);
        // t0 = 3 ms (the earliest slice); the picked "A" is at 3 ms → relative 0 ms.
        assert_eq!(r.named_spans[0].ts_ms, 0.0, "analyse should pick the earliest occurrence (rebased to t0)");
        assert_eq!(r.named_spans[0].dur_ms, 1.0);
    }
}
