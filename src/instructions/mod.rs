//! Per-function instruction counting via `hiperf record -a -e hw-instructions`.
//!
//! Disabled by default; opt in with `--with-instructions`. When enabled, each
//! iteration runs `hiperf record` in parallel with the existing hitrace
//! capture (same time window, same device), then post-processes the perf.data
//! into per-function inclusive instruction counts using
//! [`crate::instructions::parse`].
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

mod parse;
mod perf_data;
pub mod symbols;

pub use parse::aggregate_inclusive;
pub use perf_data::aggregate_inclusive_from_perf_data;

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
}

impl InstructionsConfig {
    pub fn load(workloads_dir: &Path) -> Result<Self> {
        let path = workloads_dir.join("_instructions.toml");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading instructions config at {}", path.display()))?;
        let cfg: InstructionsConfig = toml::from_str(&text)
            .with_context(|| format!("parsing TOML at {}", path.display()))?;
        Ok(cfg)
    }

    /// Engine selection from a bundle name. Returns `None` when no engine
    /// claims the bundle — caller falls back to skipping instruction
    /// collection (with a one-line stderr warning) so an unknown bundle
    /// doesn't fail the bench.
    pub fn engine_for_bundle(&self, bundle: &str) -> Option<&EngineConfig> {
        self.engines.iter().find(|e| e.bundles.iter().any(|b| b == bundle))
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
