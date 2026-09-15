//! `servoperf page <path>` — measure a static page of your own.
//!
//! A page suite (`suites/page-plr.toml`) names its legs and device settings
//! like any suite, but its workloads are templates (`workloads/_page-*.toml`)
//! that hold only what one measurement adds — a scroll gesture, a window
//! length. This command fills each template in with the page, serves the page
//! from the host, and runs the matrix as `suite` would.

use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::cli::{PageArgs, SuiteArgs};
use crate::suite::{Suite, SuiteWorkload};
use crate::workload::{self, Fixture};

pub fn run(args: PageArgs) -> Result<()> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (dir, entry) = resolve_page(&args.path, args.entry.as_deref())?;
    let name = sanitize(&match &args.name {
        Some(n) => n.clone(),
        None => default_name(&args.path, &dir),
    });

    let templates = Suite::load(&manifest.join("suites"), &args.suite)?;
    let wanted: Option<Vec<&str>> = args
        .only
        .as_deref()
        .map(|o| o.split(',').map(str::trim).collect());
    let mut suite = templates.clone();
    suite.workloads.clear();
    let mut defs = HashMap::new();
    for t in &templates.workloads {
        let kind = template_kind(&t.name).ok_or_else(|| {
            anyhow::anyhow!(
                "suite {:?} is not a page suite: its workload {:?} is not a `_`-prefixed \
                 template. Use one like page-plr.",
                templates.name,
                t.name
            )
        })?;
        if wanted.as_ref().is_some_and(|w| !w.contains(&kind)) {
            continue;
        }
        let mut w = workload::load(&manifest.join("workloads"), &t.name)?;
        let cell = format!("{name}-{kind}");
        w.name = cell.clone();
        w.url = format!("http://127.0.0.1:{}/{entry}", args.port);
        w.fixture = Some(Fixture::Static {
            dir: dir.clone(),
            port: args.port,
        });
        defs.insert(cell.clone(), w);
        suite.workloads.push(SuiteWorkload {
            name: cell,
            ..t.clone()
        });
    }
    if let Some(w) = &wanted {
        let kinds: Vec<&str> = templates
            .workloads
            .iter()
            .filter_map(|t| template_kind(&t.name))
            .collect();
        for k in w {
            anyhow::ensure!(
                kinds.contains(k),
                "--only {k:?} is not a measurement of suite {:?} (it has: {})",
                templates.name,
                kinds.join(", ")
            );
        }
    }

    eprintln!(
        "page: serving {} as http://127.0.0.1:{}/{entry}",
        dir.display(),
        args.port
    );
    let root = args
        .out
        .clone()
        .unwrap_or_else(|| crate::report::default_out_dir(&format!("page-{name}")));
    let suite_args = SuiteArgs {
        suite: suite.name.clone(),
        legs: args.legs,
        only: None,
        iterations: args.iterations,
        out: Some(root.clone()),
        bin: args.bin,
        assume_yes: args.assume_yes,
        report_only: false,
        live: false,
        no_instructions: args.no_instructions,
        ohos: args.ohos,
    };
    super::suite::run_matrix(&suite_args, &suite, &defs)?;

    // Repeated here because each cell's warning has long scrolled away.
    let mut missing: Vec<String> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .flat_map(|e| crate::fixtures::missing_resources(&e.path()))
        .collect();
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        eprintln!(
            "page: warning: the page asked for files {} does not have, and was measured \
             without them: {}",
            dir.display(),
            missing.join(", ")
        );
    }
    Ok(())
}

/// The directory to serve and the page to open in it, as a URL path.
fn resolve_page(path: &Path, entry: Option<&str>) -> Result<(PathBuf, String)> {
    anyhow::ensure!(path.exists(), "{} does not exist", path.display());
    // `absolute`, not `canonicalize`: on Windows the latter yields a `\\?\`
    // path, in which `/` is not a separator, and every resource in a
    // subdirectory would then 404.
    let path = std::path::absolute(path)?;
    let (dir, default_entry) = if path.is_dir() {
        (path, "index.html".to_string())
    } else {
        let file = path
            .file_name()
            .and_then(|f| f.to_str())
            .ok_or_else(|| anyhow::anyhow!("{} has no usable file name", path.display()))?
            .to_string();
        let dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?
            .to_path_buf();
        (dir, file)
    };
    let entry = entry
        .map(|e| e.replace('\\', "/").trim_start_matches('/').to_string())
        .unwrap_or(default_entry);
    let file_part = entry.split(['?', '#']).next().unwrap_or_default();
    let on_disk = if file_part.is_empty() || file_part.ends_with('/') {
        dir.join(file_part).join("index.html")
    } else {
        dir.join(file_part)
    };
    anyhow::ensure!(
        on_disk.is_file(),
        "no page to open: {} does not exist (pass --entry to pick another file)",
        on_disk.display()
    );
    Ok((dir, entry.replace(' ', "%20")))
}

/// What a template measures, from its name: `_page-scroll` -> `scroll`.
fn template_kind(template: &str) -> Option<&str> {
    let rest = template.strip_prefix('_')?;
    Some(rest.strip_prefix("page-").unwrap_or(rest))
}

fn default_name(given: &Path, dir: &Path) -> String {
    let stem = |p: &Path| p.file_stem().and_then(|s| s.to_str()).map(str::to_string);
    let dir_name = stem(dir).unwrap_or_else(|| "page".to_string());
    if given.is_dir() {
        return dir_name;
    }
    match stem(given) {
        Some(s) if s != "index" => s,
        _ => dir_name,
    }
}

/// Cell names become directory names; keep them portable.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_opens_its_index_and_a_file_opens_itself() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("index.html"), b"").unwrap();
        std::fs::write(root.path().join("my page.html"), b"").unwrap();

        let (dir, entry) = resolve_page(root.path(), None).unwrap();
        assert_eq!(dir, std::path::absolute(root.path()).unwrap());
        assert_eq!(entry, "index.html");

        let (dir, entry) = resolve_page(&root.path().join("my page.html"), None).unwrap();
        assert_eq!(dir, std::path::absolute(root.path()).unwrap());
        assert_eq!(entry, "my%20page.html");
        assert_eq!(
            default_name(&root.path().join("my page.html"), &dir),
            "my page"
        );
        assert_eq!(sanitize("my page"), "my-page");

        let (_, entry) = resolve_page(root.path(), Some("index.html?n=3#x")).unwrap();
        assert_eq!(entry, "index.html?n=3#x");
        assert!(resolve_page(root.path(), Some("nope.html")).is_err());
    }

    #[test]
    fn template_names_give_the_measurement() {
        assert_eq!(template_kind("_page-scroll"), Some("scroll"));
        assert_eq!(template_kind("_warm"), Some("warm"));
        assert_eq!(template_kind("mossel-index"), None);
    }

    #[test]
    fn the_page_suite_and_its_templates_load() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let suite = Suite::load(&manifest.join("suites"), "page-plr").unwrap();
        for t in &suite.workloads {
            assert!(template_kind(&t.name).is_some(), "{}", t.name);
            workload::load(&manifest.join("workloads"), &t.name).unwrap();
        }
        let scroll = workload::load(&manifest.join("workloads"), "_page-scroll").unwrap();
        let last = scroll
            .steps
            .iter()
            .map(|s| s.after_ms + s.every_ms.unwrap_or(0) * (s.times.unwrap_or(1) as u64 - 1))
            .max()
            .unwrap();
        let window = suite
            .workloads
            .iter()
            .find(|w| w.name == "_page-scroll")
            .and_then(|w| w.capture_seconds)
            .unwrap();
        assert!(
            last + 1000 <= window * 1000,
            "the last swipe ({last} ms) must finish inside the {window} s window"
        );
    }
}
