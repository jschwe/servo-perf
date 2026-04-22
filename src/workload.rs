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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Fixture {
    pub kind: FixtureKind,
    pub port: u16,
    pub doc_root: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FixtureKind {
    Http1,
    Http2,
}

fn default_tracing_filter() -> String { "info".to_string() }
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
        let fx = w.fixture.expect("fixture present");
        assert_eq!(fx.kind, FixtureKind::Http2);
        assert_eq!(fx.port, 4444);
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
        assert_eq!(w.tracing_filter, "info");
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
