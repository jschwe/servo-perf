//! `servoperf suite <name>` — run a whole measurement matrix.
//!
//! Every (leg × workload) cell is an ordinary `bench` run; this command only
//! sequences them, applies the suite's settings uniformly, and makes the
//! engine switch between legs an explicit step rather than something a human
//! remembers to do. A cell that fails is reported and the run continues, so
//! one bad workload does not cost the campaign.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::{BenchArgs, SuiteArgs};
use crate::suite::{Leg, Suite};

pub fn run(args: SuiteArgs) -> Result<()> {
    let suites_dir = suites_dir();
    let suite = Suite::load(&suites_dir, &args.suite)?;

    let legs: Vec<&Leg> = match &args.legs {
        Some(only) => {
            let wanted: Vec<&str> = only.split(',').map(str::trim).collect();
            for w in &wanted {
                anyhow::ensure!(
                    suite.legs.iter().any(|l| l.id == *w),
                    "--legs {w:?} is not a leg of suite {:?}",
                    suite.name
                );
            }
            suite
                .legs
                .iter()
                .filter(|l| wanted.contains(&l.id.as_str()))
                .collect()
        }
        None => suite.legs.iter().collect(),
    };
    let workloads: Vec<&crate::suite::SuiteWorkload> = match &args.only {
        Some(only) => {
            let wanted: Vec<&str> = only.split(',').map(str::trim).collect();
            for w in &wanted {
                anyhow::ensure!(
                    suite.workloads.iter().any(|x| x.name == *w),
                    "--only {w:?} is not a workload of suite {:?}",
                    suite.name
                );
            }
            suite
                .workloads
                .iter()
                .filter(|x| wanted.contains(&x.name.as_str()))
                .collect()
        }
        None => suite.workloads.iter().collect(),
    };

    let root = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from("out").join(format!("{}-{}", suite.name, now_stamp())));
    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;

    eprintln!(
        "suite {}: {} leg(s) x {} workload(s) -> {}",
        suite.name,
        legs.len(),
        workloads.len(),
        root.display()
    );

    let mut failures: Vec<String> = Vec::new();
    for leg in &legs {
        prepare_leg(leg, &args)?;
        for w in &workloads {
            let out = root.join(format!("{}-{}", leg.id, w.name));
            eprintln!("\n=== {} / {} ===", leg.id, w.name);
            let bench = BenchArgs {
                workload: w.name.clone(),
                bin: args.bin.clone(),
                iterations: args.iterations.or_else(|| suite.iterations_for(w)),
                out: Some(out),
                ohos: leg_ohos_args(&args, &suite, leg, w),
            };
            if let Err(e) = crate::cmd::bench::run(bench) {
                eprintln!("suite: {} / {} FAILED: {e:#}", leg.id, w.name);
                failures.push(format!("{}/{}", leg.id, w.name));
            }
        }
    }

    if let Err(e) = write_comparison(&root, &suite, &legs, &workloads) {
        eprintln!("suite: could not write the comparison: {e:#}");
    }
    eprintln!("\nsuite {} complete: {}", suite.name, root.display());
    if !failures.is_empty() {
        eprintln!("failed cells: {}", failures.join(", "));
    }
    println!("{}", root.display());
    Ok(())
}

/// Select the engine for a leg: run its `setup` command, or ask for the switch
/// to be made by hand when the suite does not say how.
fn prepare_leg(leg: &Leg, args: &SuiteArgs) -> Result<()> {
    match &leg.setup {
        Some(cmd) => {
            eprintln!("suite: leg {} setup: hdc shell {cmd}", leg.id);
            let target = crate::ohos::OhosTarget::from_args(&args.ohos);
            target
                .shell(cmd)
                .with_context(|| format!("leg {} setup command failed", leg.id))?;
        }
        None if args.assume_yes => {
            eprintln!(
                "suite: leg {} has no `setup`; assuming the device is already on engine {:?}",
                leg.id, leg.engine
            );
        }
        None => {
            eprint!(
                "suite: switch the device to the {} engine ({}), then press Enter: ",
                leg.id, leg.engine
            );
            std::io::stderr().flush().ok();
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .context("reading confirmation from stdin")?;
        }
    }
    Ok(())
}

/// Layer the suite's settings over the command line's device flags. Anything
/// the suite does not set is left as the CLI default, so a one-off override
/// still works.
fn leg_ohos_args(
    args: &SuiteArgs,
    suite: &Suite,
    leg: &Leg,
    w: &crate::suite::SuiteWorkload,
) -> crate::cli::OhosArgs {
    let mut ohos = args.ohos.clone();
    ohos.ohos = true;
    ohos.engine = Some(leg.engine.clone());
    if let Some(v) = suite.defaults.with_instructions {
        ohos.with_instructions = v;
    }
    if let Some(v) = suite.defaults.instructions_period {
        ohos.instructions_period = v;
    }
    if let Some(v) = suite.capture_seconds_for(w) {
        ohos.ohos_capture_seconds = v;
    }
    if let Some(v) = leg.trace_tags.clone().or(suite.defaults.trace_tags.clone()) {
        ohos.ohos_trace_tags = v;
    }
    if let Some(v) = leg.trace_level.clone() {
        ohos.ohos_trace_level = v;
    }
    ohos
}

fn suites_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("suites")
}

fn now_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn metrics_render_in_their_own_units() {
        assert_eq!(
            super::format_metric("instructions.layout_proper", 2.16e8),
            "216.0 M"
        );
        assert_eq!(super::format_metric("reflow.count", 24.0), "24");
        assert_eq!(
            super::format_metric("FirstContentfulPaint", 49065.0),
            "49065 ms"
        );
    }

    #[test]
    fn headline_metrics_cover_the_groups_and_their_ratios() {
        for m in [
            "instructions.layout_proper",
            "instructions.paint_prep",
            "instructions.colleagues_reflow",
            "instructions.per_reflow",
            "instructions.per_reflow.layout_proper",
            "reflow.count",
            "FirstContentfulPaint",
        ] {
            assert!(
                super::is_headline_metric(m),
                "{m} should be in the comparison"
            );
        }
        assert!(!super::is_headline_metric("soc_thermal_milli_c.before"));
    }
}

/// Compare the legs of a finished suite run.
///
/// Reads each cell's `raw.json` back and tabulates the metrics that answer
/// the question the suite exists to ask: total reflow instructions, and
/// instructions per reflow, per workload, per leg. Written as
/// `<out>/comparison.md`; the per-cell reports keep the detail.
fn write_comparison(
    root: &Path,
    suite: &Suite,
    legs: &[&Leg],
    workloads: &[&crate::suite::SuiteWorkload],
) -> Result<()> {
    use std::fmt::Write as _;

    // Metric -> leg id -> workload -> median across iterations.
    let mut cells: std::collections::BTreeMap<String, Vec<(String, String, f64)>> =
        Default::default();
    for leg in legs {
        for w in workloads {
            let raw = root.join(format!("{}-{}", leg.id, w.name)).join("raw.json");
            let Ok(text) = std::fs::read_to_string(&raw) else {
                continue;
            };
            // Read generically: `RunResults` is serialize-only, and a
            // comparison that keeps working across schema tweaks is worth more
            // here than static typing.
            let Ok(data): Result<serde_json::Value, _> = serde_json::from_str(&text) else {
                continue;
            };
            let Some(configs) = data.get("configs").and_then(|c| c.as_object()) else {
                continue;
            };
            for cfg in configs.values() {
                let Some(summary) = cfg.get("summary").and_then(|s| s.as_object()) else {
                    continue;
                };
                for (metric, stats) in summary {
                    if !is_headline_metric(metric) {
                        continue;
                    }
                    let Some(p50) = stats.get("p50").and_then(|v| v.as_f64()) else {
                        continue;
                    };
                    cells.entry(metric.clone()).or_default().push((
                        leg.id.clone(),
                        w.name.clone(),
                        p50,
                    ));
                }
            }
        }
    }

    let mut s = String::new();
    writeln!(s, "# servoperf suite `{}` — comparison\n", suite.name).unwrap();
    writeln!(
        s,
        "Median across iterations. Instruction counts unless the metric says \
         otherwise; milestones are milliseconds and `reflow.count` is a count.\n"
    )
    .unwrap();
    if cells.is_empty() {
        writeln!(s, "No comparable metrics were produced.\n").unwrap();
    }

    for (metric, rows) in &cells {
        writeln!(s, "## `{metric}`\n").unwrap();
        write!(s, "| workload |").unwrap();
        for leg in legs {
            write!(s, " {} |", leg.id).unwrap();
        }
        if legs.len() == 2 {
            write!(s, " {} vs {} |", legs[1].id, legs[0].id).unwrap();
        }
        writeln!(s).unwrap();
        writeln!(
            s,
            "|---{}|",
            "|---:".repeat(legs.len() + usize::from(legs.len() == 2))
        )
        .unwrap();
        for w in workloads {
            write!(s, "| {} |", w.name).unwrap();
            let mut values: Vec<Option<f64>> = Vec::new();
            for leg in legs {
                let v = rows
                    .iter()
                    .find(|(l, wn, _)| l == &leg.id && wn == &w.name)
                    .map(|(_, _, v)| *v);
                values.push(v);
                match v {
                    Some(v) => write!(s, " {} |", format_metric(metric, v)).unwrap(),
                    None => write!(s, " — |").unwrap(),
                }
            }
            if legs.len() == 2 {
                match (values[0], values[1]) {
                    (Some(a), Some(b)) if a > 0.0 => {
                        write!(s, " {:+.1}% |", 100.0 * (b - a) / a).unwrap()
                    }
                    _ => write!(s, " — |").unwrap(),
                }
            }
            writeln!(s).unwrap();
        }
        writeln!(s).unwrap();
    }

    writeln!(
        s,
        "Read `instructions.per_reflow.*` in preference to the totals: numerator \
         and denominator co-vary, so the ratio cancels most of the load-to-load \
         variance that makes the totals swing. Compare `layout_proper` against \
         `layout_proper` — the groupings are defined per engine in \
         `workloads/_instructions.toml` so that they mean the same phase on both \
         sides."
    )
    .unwrap();

    let path = root.join("comparison.md");
    std::fs::write(&path, s).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("suite: comparison written to {}", path.display());
    Ok(())
}

/// The metrics worth putting side by side; the per-cell report keeps the rest.
fn is_headline_metric(metric: &str) -> bool {
    metric.starts_with("instructions.per_reflow")
        || metric == "reflow.count"
        || metric.starts_with("instructions.layout_proper")
        || metric.starts_with("instructions.paint_prep")
        || metric.starts_with("instructions.colleagues_reflow")
        || metric == "FirstContentfulPaint"
}

/// Not every headline metric is an instruction count: milestones are
/// milliseconds and `reflow.count` is a plain count, and rendering either
/// through the instruction formatter reads as a wrong unit.
fn format_metric(metric: &str, v: f64) -> String {
    if metric.starts_with("instructions.") {
        human(v)
    } else if metric == "reflow.count" {
        format!("{v:.0}")
    } else {
        format!("{v:.0} ms")
    }
}

/// Instruction counts run to nine digits; a table of them is unreadable.
fn human(v: f64) -> String {
    if v >= 1e9 {
        format!("{:.2} G", v / 1e9)
    } else if v >= 1e6 {
        format!("{:.1} M", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.1} k", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}
