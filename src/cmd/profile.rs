//! `servoperf profile`: rank where an engine spends its instructions.

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::cli::ProfileArgs;
use crate::instructions::{self, InstructionsConfig};

pub fn run(args: ProfileArgs) -> Result<()> {
    let workloads_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("workloads");
    let cfg = InstructionsConfig::load(&workloads_dir)?;
    let engine = cfg.engine_by_id(&args.engine).ok_or_else(|| {
        anyhow::anyhow!("--engine {:?} not found in _instructions.toml", args.engine)
    })?;

    let mut files: Vec<PathBuf> = Vec::new();
    for input in &args.inputs {
        if input.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(input)
                .with_context(|| format!("reading {}", input.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("iter_") && n.ends_with(".perf.data"))
                })
                .collect();
            found.sort();
            files.extend(found);
        } else {
            files.push(input.clone());
        }
    }
    anyhow::ensure!(!files.is_empty(), "no perf.data files found in the inputs");

    let profile = instructions::profile_from_perf_data(
        &files,
        engine,
        &workloads_dir,
        args.under.as_deref(),
        args.callers_of.as_deref(),
    )?;
    anyhow::ensure!(
        profile.total > 0,
        "no in-library samples{} across {} capture(s)",
        args.under
            .as_deref()
            .map(|u| format!(" under {u:?}"))
            .unwrap_or_default(),
        files.len()
    );

    println!(
        "{} capture(s), {} samples, {:.1} M instructions{}",
        files.len(),
        profile.samples,
        profile.total as f64 / 1e6,
        args.under
            .as_deref()
            .map(|u| format!(" under `{u}`"))
            .unwrap_or_default()
    );
    print_table("self", &profile.self_by_function, profile.total, args.top);
    print_table(
        "inclusive",
        &profile.inclusive_by_function,
        profile.total,
        args.top,
    );
    if let Some(c) = args.callers_of.as_deref() {
        let sub: u64 = profile.callers.values().sum();
        println!(
            "\n{:.1} M instructions ({:.1}% of the total) are self time in `{c}`; \
             nearest engine frame above them:",
            sub as f64 / 1e6,
            100.0 * sub as f64 / profile.total as f64
        );
        print_table("callers", &profile.callers, sub.max(1), args.top);
    }
    Ok(())
}

fn print_table(label: &str, by: &std::collections::HashMap<String, u64>, total: u64, top: usize) {
    let mut rows: Vec<(&String, &u64)> = by.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1));
    println!("\n## {label}\n");
    println!("| % | M instr | function |");
    println!("|---:|---:|---|");
    for (name, v) in rows.into_iter().take(top) {
        println!(
            "| {:.1} | {:.1} | `{}` |",
            100.0 * *v as f64 / total as f64,
            *v as f64 / 1e6,
            shorten(name)
        );
    }
}

/// Rust symbols run to hundreds of characters of generic parameters; the
/// path and method are what identify a hotspot.
fn shorten(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut depth = 0usize;
    for c in name.chars() {
        match c {
            '<' => {
                if depth == 0 {
                    out.push('<');
                }
                depth += 1;
            }
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    out.push('>');
                }
            }
            _ if depth <= 1 => out.push(c),
            _ => {}
        }
    }
    if let Some((cut, _)) = out.char_indices().nth(160) {
        out.truncate(cut);
        out.push('…');
    }
    out
}
