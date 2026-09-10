//! Workload definitions loaded from on-disk TOML files.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Workload {
    pub name: String,
    pub url: String,
    #[serde(default = "default_tracing_filter")]
    pub tracing_filter: String,
    #[serde(default = "default_iterations")]
    pub iterations: u32,
    pub user_agent: Option<String>,
    pub viewport: Option<(u32, u32)>,
    pub device_pixel_ratio: Option<f32>,
    #[serde(default)]
    pub servoshell_args: Vec<String>,
    pub fixture: Option<Fixture>,
    /// Present for workloads measured over a render window rather than by a
    /// page-load milestone. See [`Scenario`].
    pub scenario: Option<Scenario>,
    /// Device-side actions injected into the capture window. See [`Step`].
    #[serde(default)]
    pub steps: Vec<Step>,
}

/// One device-side action executed inside the capture window, so a workload
/// can measure the app *doing* something rather than only loading.
///
/// The command runs as `hdc shell <run>`; `uitest uiInput` is the OHOS-native
/// event injector and needs no cooperation from the app under test.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Step {
    /// Milliseconds after `aa start` at which to run the command. Steps
    /// scheduled past the end of the capture window are skipped with a
    /// warning rather than silently dropped.
    pub after_ms: u64,
    /// Repeat every `every_ms` until the window ends. Omit to run once.
    #[serde(default)]
    pub every_ms: Option<u64>,
    /// Shell command, e.g. `uitest uiInput swipe 540 2200 540 600 600`.
    pub run: String,
}

/// A workload that is measured while it renders, not while it loads.
///
/// The iteration lifecycle is unchanged — launch, hold the window open, stop —
/// but the device's presented-frame ring is sampled throughout and the app's
/// log is captured at the end, so the metrics describe steady-state rendering
/// instead of startup milestones.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Scenario {
    /// RenderService surface to sample. servoshell's is `ServoDemoSurface`;
    /// `hidumper -s RenderService -a "fps <name>"` lists what a device has.
    pub surface: String,
    /// Length of the render window. Overrides `--ohos-capture-seconds`,
    /// which is sized for page loads and is usually far too short here.
    pub capture_seconds: Option<u64>,
    /// How often to sample the frame ring. The ring holds ~384 presents, so
    /// this must be short enough that consecutive samples overlap: at 60 fps
    /// that is under 6 s. `presented.lost_sample_windows` reports when it
    /// wasn't.
    #[serde(default = "default_sample_interval_seconds")]
    pub sample_interval_seconds: u64,
    /// Optional reinterpretations of the frame timeline, applied on top of
    /// the always-reported raw metrics. Empty for workloads where every
    /// frame interval is meaningful.
    #[serde(default)]
    pub post_processing: Vec<PostProcessing>,
    /// Values scraped out of the device log, for pages that report their own
    /// numbers (`console.log` reaches hilog on OHOS).
    #[serde(default)]
    pub log_metric: Vec<LogMetric>,
    /// Display refresh rate the frame timeline is binned against, to report how
    /// many refresh intervals each frame occupied. Set this to what the panel
    /// is actually running at, which on an LTPO display is not always its
    /// maximum.
    #[serde(default = "default_refresh_hz")]
    pub refresh_hz: f64,
    /// Sample per-thread CPU time across the render window and report
    /// CPU-milliseconds per presented frame. Off by default: it costs two
    /// `/proc` walks per iteration and only matters when the question is which
    /// thread a frame is waiting on.
    #[serde(default)]
    pub thread_cpu: bool,
    /// Count CPU from process start instead of across the window, and skip the
    /// opening `/proc` walk. Set this for cold-start page loads: the work being
    /// measured happens before an opening sample could be taken anyway, and
    /// walking ~100 `/proc/<pid>/task/*` entries while the page is loading
    /// costs enough device CPU to more than double the measured first paint.
    #[serde(default)]
    pub thread_cpu_from_start: bool,
}

fn default_refresh_hz() -> f64 {
    60.0
}

/// An optional post-processing step over the presented-frame timeline.
///
/// Each step is opt-in per workload because there is no interpretation that
/// suits every scenario: an animation that idles between bursts needs its
/// idle time excluded to show animation cadence, whereas a continuously
/// rendering workload (a WebGPU game, say) must keep long frames in the
/// average — there a 100 ms frame is the very thing being measured.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PostProcessing {
    /// Treat inter-frame intervals of at least `gap_ms` as idle and drop them
    /// from both the frame count and the elapsed time, reporting
    /// `presented.fps_active` alongside the unfiltered `presented.fps`.
    ///
    /// Pick `gap_ms` above the workload's slowest genuine frame:
    /// `presented.frames_near_gap` counts frames within a factor of two below
    /// the threshold, which is the warning sign that it is set too low and
    /// slow frames are being censored as idle.
    ExcludeIdleGaps { gap_ms: f64 },
}

/// One value extracted from the device log per iteration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogMetric {
    /// Key this lands under in the iteration's metrics map.
    pub name: String,
    /// Regular expression whose first capture group parses as a number.
    pub pattern: String,
    /// How to reduce multiple matches within one iteration to one value.
    #[serde(default)]
    pub aggregate: LogAggregate,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LogAggregate {
    #[default]
    Median,
    Mean,
    Min,
    Max,
    First,
    Last,
    Count,
}

impl LogAggregate {
    /// Reduce one iteration's matches. `None` when there were none, which
    /// leaves the metric absent for that iteration rather than reporting a
    /// zero that would drag the summary down.
    pub fn apply(&self, mut values: Vec<f64>) -> Option<f64> {
        if values.is_empty() {
            return None;
        }
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(match self {
            Self::Median => values[values.len() / 2],
            Self::Mean => values.iter().sum::<f64>() / values.len() as f64,
            Self::Min => values[0],
            Self::Max => values[values.len() - 1],
            // `First`/`Last` are in emission order, which sorting destroyed;
            // both are recovered from the extremes only when the caller wants
            // them, so keep the unsorted semantics explicit here.
            Self::First | Self::Last => unreachable!("handled by apply_ordered"),
            Self::Count => values.len() as f64,
        })
    }

    /// Reduce matches keeping emission order, which `First`/`Last` need.
    pub fn apply_ordered(&self, values: Vec<f64>) -> Option<f64> {
        match self {
            Self::First => values.first().copied(),
            Self::Last => values.last().copied(),
            _ => self.apply(values),
        }
    }
}

fn default_sample_interval_seconds() -> u64 {
    3
}

/// A background server that needs to be running while a workload's
/// iterations execute. Each variant's fields live inline so TOML reads as
/// `kind = "http1"` + sibling fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Fixture {
    /// Local static-file server over HTTP/1.1 + TLS. Used by `h1-multi`,
    /// `simple`, etc. Doc root is resolved under
    /// `<workloads_dir>/../fixtures/<doc_root>`.
    Http1 { port: u16, doc_root: PathBuf },
    /// Same as `Http1` but negotiates HTTP/2 via ALPN.
    Http2 { port: u16, doc_root: PathBuf },
    /// Replay a Web Page Replay archive through a local CONNECT shim so
    /// servoshell talks to a deterministic on-disk recording instead of
    /// the live origin. On first use (archive missing), a single
    /// recording pass is made against the live origin automatically.
    WprReplay {
        /// Path to the `.wprgo` archive, resolved relative to
        /// `<workloads_dir>/../wpr-archives/` if not absolute.
        archive: PathBuf,
        /// Port WPR's HTTPS server listens on.
        #[serde(default = "default_wpr_port")]
        wpr_port: u16,
        /// Port the CONNECT-tunnel shim (`wpr_tunnel` binary) listens on.
        /// servoshell is invoked with
        /// `https_proxy=http://127.0.0.1:<tunnel_port>`.
        #[serde(default = "default_tunnel_port")]
        tunnel_port: u16,
    },
}

impl Fixture {
    /// Ports on `127.0.0.1` (host) that need to be reachable from the
    /// servoshell process. Used by the OHOS target to set up
    /// `hdc rport` forwards so the device can talk to host-side
    /// fixtures.
    ///
    /// For HTTP/1.1 + HTTP/2: the fixture's listening port (the URL
    /// targets it directly).
    ///
    /// For WPR replay: only the tunnel port — servoshell is configured
    /// with `https_proxy=http://127.0.0.1:<tunnel_port>` and never
    /// connects to the WPR server itself.
    pub fn ports_to_forward(&self) -> Vec<u16> {
        match self {
            Fixture::Http1 { port, .. } | Fixture::Http2 { port, .. } => vec![*port],
            Fixture::WprReplay { tunnel_port, .. } => vec![*tunnel_port],
        }
    }
}

fn default_wpr_port() -> u16 {
    4443
}
fn default_tunnel_port() -> u16 {
    4480
}

fn default_tracing_filter() -> String {
    // info globally, plus trace-level for any span/event tagged
    // `servo_profiling = true`. The `servo_tracing::instrument` macro
    // injects that field automatically (and defaults to TRACE level),
    // so this upgrade is what makes upstream startup spans like
    // Servo::new, script::init, ScripThread::new, pre_page_load
    // visible without flooding the trace with every per-frame TRACE span.
    "info,[{servo_profiling=true}]=trace".to_string()
}
fn default_iterations() -> u32 {
    20
}

/// Load a workload from `<workloads_dir>/<name>.toml`.
pub fn load(workloads_dir: &Path, name: &str) -> Result<Workload> {
    let path = workloads_dir.join(format!("{name}.toml"));
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading workload file at {}", path.display()))?;
    let w: Workload = toml::from_str(&text)
        .with_context(|| format!("parsing workload TOML at {}", path.display()))?;
    if w.name != name {
        anyhow::bail!(
            "workload file {}: `name` field ({:?}) does not match filename stem ({:?})",
            path.display(),
            w.name,
            name
        );
    }
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_candle_scenario_workload() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("workloads");
        let w = load(&dir, "candle").expect("load candle");
        let s = w.scenario.expect("candle declares a scenario");
        assert_eq!(s.surface, "ServoDemoSurface");
        assert_eq!(s.capture_seconds, Some(30));
        assert_eq!(
            s.post_processing,
            vec![PostProcessing::ExcludeIdleGaps { gap_ms: 80.0 }]
        );
        let names: Vec<&str> = s.log_metric.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["chart_rebuild_ms", "raf_fps"]);
        for m in &s.log_metric {
            regex::Regex::new(&m.pattern).expect("log_metric pattern compiles");
        }
    }

    #[test]
    fn page_load_workloads_declare_no_scenario() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("workloads");
        assert!(load(&dir, "wikipedia")
            .expect("load wikipedia")
            .scenario
            .is_none());
    }

    #[test]
    fn loads_h2_multi_workload() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("workloads");
        let w = load(&dir, "h2-multi").expect("load h2-multi");
        assert_eq!(w.name, "h2-multi");
        assert_eq!(w.iterations, 20);
        assert!(w.url.starts_with("https://127.0.0.1:4444/"));
        match w.fixture.expect("fixture present") {
            Fixture::Http2 { port, .. } => assert_eq!(port, 4444),
            other => panic!("expected Http2 fixture, got {other:?}"),
        }
    }

    #[test]
    fn defaults_are_applied_for_minimal_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("min.toml"),
            r#"name = "min"
url = "https://example.test/"
"#,
        )
        .unwrap();
        let w = load(dir.path(), "min").unwrap();
        assert_eq!(w.tracing_filter, "info,[{servo_profiling=true}]=trace");
        assert_eq!(w.iterations, 20);
        assert!(w.fixture.is_none());
    }

    #[test]
    fn mismatched_name_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("x.toml"),
            r#"name = "y"
url = "https://example.test/"
"#,
        )
        .unwrap();
        let err = load(dir.path(), "x").unwrap_err();
        assert!(err.to_string().contains("does not match filename"));
    }

    #[test]
    fn ports_to_forward_lists_relevant_host_ports() {
        // http1/http2 → fixture port (the URL hits it directly).
        let f = Fixture::Http1 {
            port: 4443,
            doc_root: "www".into(),
        };
        assert_eq!(f.ports_to_forward(), vec![4443]);
        let f = Fixture::Http2 {
            port: 4444,
            doc_root: "www".into(),
        };
        assert_eq!(f.ports_to_forward(), vec![4444]);
        // wpr-replay → tunnel only; servoshell never connects to wpr_port directly.
        let f = Fixture::WprReplay {
            archive: "x.wprgo".into(),
            wpr_port: 4443,
            tunnel_port: 4480,
        };
        assert_eq!(f.ports_to_forward(), vec![4480]);
    }

    #[test]
    fn all_checked_in_workloads_load() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("workloads");
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem.starts_with('_') {
                continue; // registry file, not a workload
            }
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let w = super::load(&dir, stem).unwrap_or_else(|e| panic!("loading {stem}: {e:#}"));
            assert_eq!(w.name, stem);
        }
    }
}
