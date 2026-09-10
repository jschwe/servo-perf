//! Per-function instruction counting via `hiperf record -a -e hw-instructions`.
//!
//! Disabled by default; opt in with `--with-instructions`. When enabled, each
//! iteration runs `hiperf record` alongside the hitrace capture, then
//! post-processes the perf.data into per-function inclusive instruction counts
//! using
//! [`crate::instructions::perf_data`].
//!
//! Engine selection (which set of function-name substrings to aggregate) is
//! driven by `workloads/_instructions.toml`. The bundle name configured via
//! `--ohos-bundle` picks the engine.
//!
//! Symbol resolution for stripped libraries (e.g. ArkWeb's libarkweb_engine.so)
//! is handled by the [`symbols`] submodule — it parses the `.gnu_debugdata`
//! MiniDebugInfo section, extracts `.symtab`/`.strtab`, and produces a merged
//! ELF suitable for `hiperf report --symbol-dir`. Run once via
//! `servoperf prepare-arkweb-symbols` before benching.

mod perf_data;
pub mod symbols;

pub use perf_data::{aggregate_inclusive_from_perf_data, Aggregation};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Top-level shape of `workloads/_instructions.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionsConfig {
    #[serde(rename = "engines")]
    pub engines: Vec<EngineConfig>,
}

/// One engine entry. `bundles` is an exact-match list against
/// `--ohos-bundle`. `functions` are case-sensitive substring matches against
/// resolved symbol names in hiperf's text report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    pub id: String,
    pub bundles: Vec<String>,
    /// Host-side filename of the unstripped library that hiperf needs in
    /// `--symbol-dir`. Empty when no merge is needed. Relative paths are
    /// resolved under the output directory at prep time; the device-side
    /// push location is fixed.
    #[serde(default)]
    pub symbol_file: String,
    pub functions: Vec<String>,
    /// Engine-specific launch flags emitted when the workload's fixture
    /// requires the app to use a proxy (e.g. `wpr-replay`). `$PROXY` is
    /// replaced with the proxy URI at launch time. Each entry is a
    /// single flag (no whitespace splitting); bare `--key=value` strings
    /// are translated to `--psn=--key=value` automatically by
    /// `workload_args_to_aa_params`.
    #[serde(default)]
    pub proxy_args: Vec<String>,
    /// Trace-span name substrings that each denote one completed layout
    /// run. Counted from the iteration's hitrace capture to give the
    /// denominator of `instructions.per_reflow`. Case-sensitive substring
    /// match, so it works against both Servo's bare span names and the
    /// `H:`-prefixed markers OHOS system emitters use.
    #[serde(default)]
    pub reflow_spans: Vec<String>,
    /// Which entry of `functions` — or which `group` name — is the reflow
    /// numerator. When set (and `reflow_spans` produced a non-zero count) the
    /// bench also reports `instructions.per_reflow`.
    #[serde(default)]
    pub reflow_instructions: Option<String>,
    /// Named sums over several `functions` patterns, for reporting a phase
    /// that no single symbol covers — Servo's paint prep is a stacking-context
    /// tree plus a display list, for instance.
    ///
    /// A group is **not** the sum of its members' individual metrics: a sample
    /// is credited to the group once if its callchain contains *any* member,
    /// so nested members are not double-counted. Where a group sits below the
    /// arithmetic sum of its parts, the difference is exactly that overlap.
    #[serde(default, rename = "group")]
    pub groups: Vec<MetricGroup>,
}

/// One named sum over `functions` patterns. Reported as
/// `instructions.<name>` alongside the per-symbol counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricGroup {
    pub name: String,
    pub functions: Vec<String>,
}

impl InstructionsConfig {
    pub fn load(workloads_dir: &Path) -> Result<Self> {
        let path = workloads_dir.join("_instructions.toml");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading instructions config at {}", path.display()))?;
        let cfg: InstructionsConfig =
            toml::from_str(&text).with_context(|| format!("parsing TOML at {}", path.display()))?;
        Ok(cfg)
    }

    /// Engine selection from a bundle name. Returns `None` when no engine
    /// claims the bundle — caller falls back to skipping instruction
    /// collection (with a one-line stderr warning) so an unknown bundle
    /// doesn't fail the bench.
    /// Engine selection by explicit `id` (`--engine`). Takes precedence over
    /// bundle matching, for bundles whose engine is switchable at runtime.
    pub fn engine_by_id(&self, id: &str) -> Option<&EngineConfig> {
        self.engines.iter().find(|e| e.id == id)
    }

    pub fn engine_for_bundle(&self, bundle: &str) -> Option<&EngineConfig> {
        self.engines
            .iter()
            .find(|e| e.bundles.iter().any(|b| b == bundle))
    }
}

/// Where on the device hiperf reads/writes per-iteration artefacts. These
/// live under `/data/local/tmp/` rather than the app sandbox so they survive
/// `aa force-stop` and `hiperf record -a` (running as the shell user) can
/// write to them.
pub struct DevicePaths;

impl DevicePaths {
    pub const PERF_DATA: &str = "/data/local/tmp/servoperf_iter.data";
    pub const PERF_REPORT_TXT: &str = "/data/local/tmp/servoperf_iter.stack.txt";
    pub const SYMBOL_DIR: &str = "/data/local/tmp/symbols";
    pub const SYMBOL_FILE: &str = "/data/local/tmp/symbols/libarkweb_engine.so";
}

/// Count completed reflows in one iteration's trace.
///
/// Returns `reflow.count` (the total across every configured pattern)
/// plus one `reflow.count.<pattern>` per pattern, so a mis-specified
/// pattern is visible rather than silently folded into the total.
///
/// Counting *spans* rather than begin-markers means a layout that starts
/// inside the capture window but ends after it is not counted; that
/// undercounts by at most one per run.
pub fn reflow_span_starts(
    slices: &[crate::trace::Slice],
    engine: &EngineConfig,
) -> std::collections::BTreeMap<String, Vec<u64>> {
    let mut out = std::collections::BTreeMap::new();
    for pattern in &engine.reflow_spans {
        let starts: Vec<u64> = slices
            .iter()
            .filter(|s| s.name.contains(pattern))
            .map(|s| s.ts_ns)
            .collect();
        out.insert(pattern.clone(), starts);
    }
    out
}

/// Count the reflows inside `window`, the interval the instruction counts
/// actually came from.
///
/// Without this the two halves of `instructions.per_reflow` describe different
/// intervals: hitrace is running before `aa start` and keeps recording while
/// `--trace_finish` flushes, so the denominator spans measurably more than the
/// PMU window (measured on one 20 s capture: a 49.5 s trace against a 20.0 s
/// sample window). Both clocks are the device's monotonic one, so the
/// timestamps are directly comparable.
///
/// `window` is `None` when instruction counting is off, or when the capture
/// produced no timestamped sample; the whole trace is then counted, which is
/// the right answer for a `reflow.count` that is not a denominator.
pub fn count_reflow_spans_in(
    starts: &std::collections::BTreeMap<String, Vec<u64>>,
    window: Option<(u64, u64)>,
) -> std::collections::BTreeMap<String, f64> {
    let mut out = std::collections::BTreeMap::new();
    if starts.is_empty() {
        return out;
    }
    let mut total = 0u64;
    for (pattern, ts) in starts {
        let n = ts
            .iter()
            .filter(|t| match window {
                Some((lo, hi)) => **t >= lo && **t <= hi,
                None => true,
            })
            .count() as u64;
        total += n;
        out.insert(format!("reflow.count.{pattern}"), n as f64);
    }
    out.insert("reflow.count".to_string(), total as f64);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::Slice;

    fn slice_at(name: &str, ts_ns: u64) -> Slice {
        Slice {
            ts_ns,
            ..slice(name)
        }
    }

    fn slice(name: &str) -> Slice {
        Slice {
            name: name.to_string(),
            thread: "t".into(),
            ts_ns: 0,
            dur_ns: 0,
            debug_annotations: vec![],
        }
    }

    #[test]
    fn groups_are_seeded_and_named_alongside_functions() {
        let cfg = InstructionsConfig::load(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/workloads"
        )))
        .expect("workloads/_instructions.toml parses");
        for engine in &cfg.engines {
            let names: Vec<&str> = engine.groups.iter().map(|g| g.name.as_str()).collect();
            assert!(
                names.contains(&"layout_proper")
                    && names.contains(&"paint_prep")
                    && names.contains(&"colleagues_reflow"),
                "engine {:?} is missing a comparison group: {names:?}",
                engine.id
            );
            for g in &engine.groups {
                assert!(!g.functions.is_empty(), "group {:?} is empty", g.name);
                // A group name that collides with a symbol pattern would have
                // the two fight over the same metric key.
                assert!(
                    !engine.functions.contains(&g.name),
                    "group {:?} collides with a functions entry",
                    g.name
                );
            }
        }
    }

    #[test]
    fn counts_only_the_spans_inside_the_sampled_window() {
        let engine = EngineConfig {
            id: "arkweb".into(),
            bundles: vec![],
            symbol_file: String::new(),
            functions: vec![],
            proxy_args: vec![],
            reflow_spans: vec!["performLayout".into()],
            reflow_instructions: None,
            groups: vec![],
        };
        let mut slices = vec![
            slice_at("H:LocalFrameView::performLayout", 50), // before the window
            slice_at("H:LocalFrameView::performLayout", 150),
            slice_at("H:LocalFrameView::performLayout", 250),
            slice_at("H:LocalFrameView::performLayout", 950), // after the window
        ];
        slices.push(slice_at("H:Something::else", 150));
        let starts = reflow_span_starts(&slices, &engine);

        // Narrowed to the interval the instruction samples came from.
        let m = count_reflow_spans_in(&starts, Some((100, 300)));
        assert_eq!(m["reflow.count"], 2.0);
        // Without a window — no instruction counting — the whole trace counts.
        let m = count_reflow_spans_in(&starts, None);
        assert_eq!(m["reflow.count"], 4.0);
    }

    #[test]
    fn counts_reflow_spans_by_substring() {
        let engine = EngineConfig {
            id: "arkweb".into(),
            bundles: vec![],
            symbol_file: String::new(),
            functions: vec![],
            proxy_args: vec![],
            reflow_spans: vec!["LocalFrameView::performLayout".into()],
            reflow_instructions: None,
            groups: vec![],
        };
        let slices = vec![
            slice("H:LocalFrameView::performLayout"),
            slice("H:LocalFrameView::performLayout"),
            slice("H:UpdateLayoutTree"),
        ];
        let m = count_reflow_spans_in(&reflow_span_starts(&slices, &engine), None);
        assert_eq!(m["reflow.count"], 2.0);
        assert_eq!(m["reflow.count.LocalFrameView::performLayout"], 2.0);
    }

    #[test]
    fn no_patterns_yields_no_metrics() {
        let engine = EngineConfig {
            id: "x".into(),
            bundles: vec![],
            symbol_file: String::new(),
            functions: vec![],
            proxy_args: vec![],
            reflow_spans: vec![],
            reflow_instructions: None,
            groups: vec![],
        };
        assert!(
            count_reflow_spans_in(&reflow_span_starts(&[slice("a")], &engine), None).is_empty()
        );
    }
}
