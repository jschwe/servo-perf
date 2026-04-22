//! Trace parsing, critical-path extraction.
//!
//! Parsing (a `.pftrace` → `Vec<Slice>`) is added in Task 5 once the
//! generated protobuf types are wired up. For now this file only
//! contains the span registry: the list of phase names we measure on
//! the critical path, plus per-edge gap thresholds for the unexplained-
//! gap flagger.

use anyhow::{Context, Result};
#[allow(unused_imports)]
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
}
