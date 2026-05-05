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
    Http1 {
        port: u16,
        doc_root: PathBuf,
    },
    /// Same as `Http1` but negotiates HTTP/2 via ALPN.
    Http2 {
        port: u16,
        doc_root: PathBuf,
    },
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
fn default_iterations() -> u32 { 20 }

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
            path.display(), w.name, name
        );
    }
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let f = Fixture::Http1 { port: 4443, doc_root: "www".into() };
        assert_eq!(f.ports_to_forward(), vec![4443]);
        let f = Fixture::Http2 { port: 4444, doc_root: "www".into() };
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
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
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
