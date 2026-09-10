//! Suite definitions: a checked-in description of a measurement campaign.
//!
//! A campaign is a matrix — every workload run against every engine leg — and
//! doing that by hand is where the mistakes live: a leg run with the previous
//! engine still selected, one workload given a different capture window than
//! the rest, a repetition count that drifted between legs. The suite file
//! makes the matrix and its settings a reviewable artefact instead of shell
//! history.
//!
//! Loaded from `suites/<name>.toml`. See `suites/mossel.toml`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Suite {
    /// Must match the file stem, so a report cannot claim a name the file
    /// does not have.
    pub name: String,
    /// Applied to every leg/workload pair; individual entries override.
    #[serde(default)]
    pub defaults: SuiteDefaults,
    /// Device settings for the campaign, so they do not have to be retyped on
    /// every invocation. A command-line flag that was actually given still
    /// wins; these fill in only where the CLI is still on its default.
    #[serde(default)]
    pub device: DeviceSettings,
    /// One entry per engine under test, run in file order.
    #[serde(rename = "leg")]
    pub legs: Vec<Leg>,
    /// The workloads in the matrix, run in file order within each leg.
    #[serde(rename = "workload")]
    pub workloads: Vec<SuiteWorkload>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SuiteDefaults {
    /// Repetitions per workload. Unset keeps each workload's own count.
    #[serde(default)]
    pub iterations: Option<u32>,
    /// Capture window per iteration, seconds. Keep it near the page's load
    /// time: hitrace's ring buffer discards the *oldest* records, so a long
    /// window throws away the page load it was meant to capture.
    #[serde(default)]
    pub capture_seconds: Option<u64>,
    /// hiperf sampling period, in retired instructions.
    #[serde(default)]
    pub instructions_period: Option<u64>,
    /// Comma-separated hitrace tags. `nweb` is required for ArkWeb's layout
    /// markers.
    #[serde(default)]
    pub trace_tags: Option<String>,
    /// Whether to collect per-function instruction counts.
    #[serde(default)]
    pub with_instructions: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DeviceSettings {
    /// Bundle to launch — the CLI default is Servo's own, not the wrapper's.
    #[serde(default)]
    pub bundle: Option<String>,
    /// UIAbility name, when it is not `EntryAbility`.
    #[serde(default)]
    pub ability: Option<String>,
    /// Device serial, for a host with several attached.
    #[serde(default)]
    pub hdc_target: Option<String>,
    /// Thermal zone `type` to sample. Worth setting per campaign: the CLI
    /// default (`soc_thermal`) is a flat placeholder on some devices.
    #[serde(default)]
    pub thermal_zone: Option<String>,
    /// hitrace ring buffer, KiB. Some devices cap below the CLI default.
    #[serde(default)]
    pub trace_buffer_kib: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Leg {
    /// Short label; names the per-leg output directory.
    pub id: String,
    /// `_instructions.toml` engine entry to attribute against.
    pub engine: String,
    /// Optional `hdc shell` command run once before the leg — typically the
    /// device's engine-selection parameter. Left unset, the run pauses and
    /// asks for the switch to be made by hand, because servoperf has no
    /// business guessing how a given image selects its web engine.
    #[serde(default)]
    pub setup: Option<String>,
    /// Overrides `defaults.trace_tags` for this leg.
    #[serde(default)]
    pub trace_tags: Option<String>,
    /// Overrides the trace-level threshold for this leg. `""` leaves the
    /// device untouched, which is right for engines whose markers are not
    /// level-gated.
    #[serde(default)]
    pub trace_level: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuiteWorkload {
    /// Workload file stem under `workloads/`.
    pub name: String,
    /// Overrides `defaults.iterations`.
    #[serde(default)]
    pub iterations: Option<u32>,
    /// Overrides `defaults.capture_seconds`.
    #[serde(default)]
    pub capture_seconds: Option<u64>,
}

impl Suite {
    pub fn load(suites_dir: &Path, name: &str) -> Result<Self> {
        let path = suites_dir.join(format!("{name}.toml"));
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading suite at {}", path.display()))?;
        let suite: Suite = toml::from_str(&text)
            .with_context(|| format!("parsing suite at {}", path.display()))?;
        anyhow::ensure!(
            suite.name == name,
            "suite {:?} declares name {:?}",
            name,
            suite.name
        );
        anyhow::ensure!(!suite.legs.is_empty(), "suite {name:?} has no [[leg]]");
        anyhow::ensure!(
            !suite.workloads.is_empty(),
            "suite {name:?} has no [[workload]]"
        );
        let mut ids: Vec<&str> = suite.legs.iter().map(|l| l.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        anyhow::ensure!(ids.len() == before, "suite {name:?} has duplicate leg ids");
        Ok(suite)
    }

    /// Effective iteration count for one cell of the matrix.
    pub fn iterations_for(&self, w: &SuiteWorkload) -> Option<u32> {
        w.iterations.or(self.defaults.iterations)
    }

    /// Effective capture window for one cell of the matrix.
    pub fn capture_seconds_for(&self, w: &SuiteWorkload) -> Option<u64> {
        w.capture_seconds.or(self.defaults.capture_seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suites_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("suites")
    }

    #[test]
    fn checked_in_suites_load() {
        for entry in std::fs::read_dir(suites_dir()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let stem = path.file_stem().unwrap().to_str().unwrap();
            Suite::load(&suites_dir(), stem)
                .unwrap_or_else(|e| panic!("loading suite {stem}: {e:#}"));
        }
    }

    #[test]
    fn the_mossel_suite_names_the_wrapper_bundle() {
        // The CLI default bundle is Servo's own; a suite that forgets to say
        // otherwise silently measures the wrong app.
        let suite = Suite::load(&suites_dir(), "mossel").unwrap();
        assert_eq!(
            suite.device.bundle.as_deref(),
            Some("org.openharmonyrs.arkwebtest")
        );
    }

    #[test]
    fn workload_overrides_win_over_defaults() {
        let suite = Suite {
            name: "s".into(),
            device: DeviceSettings::default(),
            defaults: SuiteDefaults {
                iterations: Some(15),
                capture_seconds: Some(20),
                ..Default::default()
            },
            legs: vec![],
            workloads: vec![],
        };
        let plain = SuiteWorkload {
            name: "w".into(),
            iterations: None,
            capture_seconds: None,
        };
        let overridden = SuiteWorkload {
            name: "w".into(),
            iterations: Some(3),
            capture_seconds: Some(60),
        };
        assert_eq!(suite.iterations_for(&plain), Some(15));
        assert_eq!(suite.iterations_for(&overridden), Some(3));
        assert_eq!(suite.capture_seconds_for(&overridden), Some(60));
    }
}
