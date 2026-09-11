//! `servoperf suite <name>` — run a whole measurement matrix.
//!
//! Every (leg × workload) cell is an ordinary `bench` run; this command only
//! sequences them, applies the suite's settings uniformly, and makes the
//! engine switch between legs an explicit step rather than something a human
//! remembers to do. A cell that fails is reported and the run continues, so
//! one bad workload does not cost the campaign.

use anyhow::{Context, Result};
use std::collections::HashMap;
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
        .unwrap_or_else(|| crate::report::default_out_dir(&suite.name));
    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;

    // Held across the whole matrix, not per cell: the gaps between cells — and
    // especially the wait at an engine-switch prompt — are exactly when the
    // screen would sleep, and a screen-off freezes the app mid-campaign.
    let _screen_guard =
        crate::ohos::OhosTarget::from_args(&leg_ohos_args(&args, &suite, legs[0], None))
            .guard_screen_awake();

    eprintln!(
        "suite {}: {} leg(s) x {} workload(s) -> {}",
        suite.name,
        legs.len(),
        workloads.len(),
        root.display()
    );

    let mut failures: Vec<String> = Vec::new();
    'legs: for leg in &legs {
        if crate::cancel::requested() {
            break;
        }
        prepare_leg(leg, &args, &suite)?;
        for w in &workloads {
            if crate::cancel::requested() {
                eprintln!("suite: cancelled; skipping the rest of the matrix");
                break 'legs;
            }
            let out = root.join(format!("{}-{}", leg.id, w.name));
            eprintln!("\n=== {} / {} ===", leg.id, w.name);
            let bench = BenchArgs {
                workload: w.name.clone(),
                bin: args.bin.clone(),
                iterations: args.iterations.or_else(|| suite.iterations_for(w)),
                out: Some(out),
                ohos: leg_ohos_args(&args, &suite, leg, Some(w)),
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
fn prepare_leg(leg: &Leg, args: &SuiteArgs, suite: &Suite) -> Result<()> {
    // A leg that names its own bundle *is* the engine selection: the two legs
    // are different apps, so there is nothing to switch and nothing to ask.
    if leg.bundle.is_some() && leg.setup.is_none() {
        eprintln!(
            "suite: leg {} runs its own bundle ({}); no engine switch needed",
            leg.id,
            leg.bundle.as_deref().unwrap_or_default()
        );
        return Ok(());
    }
    match &leg.setup {
        Some(cmd) => {
            eprintln!("suite: leg {} setup: hdc shell {cmd}", leg.id);
            let target = crate::ohos::OhosTarget::from_args(&leg_ohos_args(args, suite, leg, None));
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
                "suite: switch the device to the {} engine ({}), then press Enter \
                 (Ctrl-C to stop): ",
                leg.id, leg.engine
            );
            std::io::stderr().flush().ok();
            wait_for_enter_or_cancel();
        }
    }
    Ok(())
}

/// Block until the operator presses Enter, or until a stop is asked for.
///
/// A plain `read_line` would swallow the interrupt: the signal handler sets the
/// flag but nothing reads it until Enter arrives, so Ctrl-C at the prompt looks
/// like a hang. Reading on a side thread lets the wait notice the flag instead —
/// and there is nothing to wind down at this point, the leg has not started.
fn wait_for_enter_or_cancel() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        // The receiver is gone if we were cancelled; that is not an error.
        let _ = tx.send(());
    });
    loop {
        if crate::cancel::requested() {
            eprintln!();
            return;
        }
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Layer the suite's settings over the command line's device flags. Anything
/// the suite does not set is left as the CLI default, so a one-off override
/// still works.
fn leg_ohos_args(
    args: &SuiteArgs,
    suite: &Suite,
    leg: &Leg,
    w: Option<&crate::suite::SuiteWorkload>,
) -> crate::cli::OhosArgs {
    let mut ohos = args.ohos.clone();
    // A suite is always a device campaign; `--ohos` would be noise.
    ohos.ohos = true;
    ohos.engine = Some(leg.engine.clone());
    // Device settings from the file, applied only where the command line is
    // still on its default — an explicitly passed flag keeps winning.
    let d = &suite.device;
    // The leg wins over the campaign default: a leg bundle is the whole point
    // of a two-app comparison, so it must not be filled in only when the CLI
    // is still on its default.
    if let Some(v) = leg.bundle.clone() {
        ohos.ohos_bundle = v;
    } else if let Some(v) = d.bundle.clone() {
        if ohos.ohos_bundle == crate::cli::defaults::BUNDLE {
            ohos.ohos_bundle = v;
        }
    }
    if let Some(v) = leg.ability.clone() {
        ohos.ohos_ability = v;
    } else if let Some(v) = d.ability.clone() {
        if ohos.ohos_ability == crate::cli::defaults::ABILITY {
            ohos.ohos_ability = v;
        }
    }
    if let Some(v) = d.hdc_target.clone() {
        if ohos.hdc_target.is_none() {
            ohos.hdc_target = Some(v);
        }
    }
    if let Some(v) = d.thermal_zone.clone() {
        if ohos.ohos_thermal_zone == crate::cli::defaults::THERMAL_ZONE {
            ohos.ohos_thermal_zone = v;
        }
    }
    if let Some(v) = d.trace_buffer_kib {
        if ohos.ohos_trace_buffer_kib == crate::cli::defaults::TRACE_BUFFER_KIB {
            ohos.ohos_trace_buffer_kib = v;
        }
    }
    if let Some(v) = suite.defaults.with_instructions {
        ohos.with_instructions = v;
    }
    if let Some(v) = suite.defaults.instructions_period {
        ohos.instructions_period = v;
    }
    if let Some(v) = w.and_then(|w| suite.capture_seconds_for(w)) {
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

#[cfg(test)]
mod tests {
    /// The prompt must notice a cancellation without an Enter to unblock it,
    /// or Ctrl-C at the engine switch looks like a hang.
    #[test]
    fn the_engine_prompt_returns_when_cancelled() {
        crate::cancel::request();
        let t = std::time::Instant::now();
        super::wait_for_enter_or_cancel();
        assert!(t.elapsed() < std::time::Duration::from_secs(2));
        crate::cancel::clear_for_test();
    }

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

    // metric -> (leg, workload) -> the iteration values behind it.
    let mut cells: std::collections::BTreeMap<String, HashMap<(String, String), Vec<f64>>> =
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
                let Some(iters) = cfg.get("iterations").and_then(|i| i.as_array()) else {
                    continue;
                };
                for it in iters {
                    let Some(metrics) = it
                        .get("ok")
                        .and_then(|o| o.get("metrics"))
                        .and_then(|m| m.as_object())
                    else {
                        continue;
                    };
                    for (metric, value) in metrics {
                        if !is_headline_metric(metric) {
                            continue;
                        }
                        let Some(v) = value.as_f64() else { continue };
                        cells
                            .entry(metric.clone())
                            .or_default()
                            .entry((leg.id.clone(), w.name.clone()))
                            .or_default()
                            .push(v);
                    }
                }
            }
        }
    }

    let mut s = String::new();
    let mut notes: Vec<String> = Vec::new();
    writeln!(s, "# servoperf suite `{}` — comparison\n", suite.name).unwrap();
    writeln!(
        s,
        "Each cell is `median ±cv (n)` across iterations — the spread is there \
         because a median alone cannot say whether a difference between legs is \
         a result. Instruction counts unless the metric says otherwise; \
         milestones are milliseconds and `reflow.count` is a count.\n"
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
            let mut high_spread: Vec<(String, String, crate::stats::Spread)> = Vec::new();
            let mut stats: Vec<Option<crate::stats::Spread>> = Vec::new();
            for leg in legs {
                let samples = rows.get(&(leg.id.clone(), w.name.clone()));
                let sp = samples.and_then(|v| crate::stats::spread(v));
                stats.push(sp);
                match (&sp, samples) {
                    // One iteration: the value is real, the uncertainty is
                    // unknown. Say so rather than implying either.
                    (None, Some(values)) if values.len() == 1 => {
                        write!(s, " {} (n=1) |", format_metric(metric, values[0])).unwrap();
                        notes.push(format!(
                            "`{metric}` on {}/{}: one iteration, so nothing can be said about \
                             its spread and no difference involving it is resolvable.",
                            leg.id, w.name
                        ));
                    }
                    (Some(sp), Some(values)) => {
                        let flag = if sp.cv > 0.20 { " ⚠" } else { "" };
                        write!(
                            s,
                            " {} ±{:.0}%{} (n={}) |",
                            format_metric(metric, sp.median),
                            100.0 * sp.cv,
                            flag,
                            sp.n
                        )
                        .unwrap();
                        if sp.cv > 0.20 {
                            // Noted below, where the other leg is known: the
                            // floor for a comparison carries both legs' error.
                            high_spread.push((leg.id.clone(), w.name.clone(), *sp));
                        }
                        for i in crate::stats::outliers(values) {
                            notes.push(format!(
                                "`{}` on {}/{}: iteration {} is {:.1}x the median — check its \
                                 thermals, whether the page came from cache, and the run's \
                                 failed-iteration count.",
                                metric,
                                leg.id,
                                w.name,
                                i,
                                if sp.median != 0.0 {
                                    values[i] / sp.median
                                } else {
                                    0.0
                                }
                            ));
                        }
                    }
                    _ => write!(s, " — |").unwrap(),
                }
            }
            // One floor for the whole document: quoting a single leg's own
            // understated it by about sqrt(2), enough to bold a delta in the
            // table and call it unresolvable in the note beneath.
            let pairwise = match (
                stats.first().copied().flatten(),
                stats.get(1).copied().flatten(),
            ) {
                (Some(a), Some(b)) => Some(100.0 * crate::stats::resolvable_between(&a, &b)),
                _ => None,
            };
            for (leg_id, wl, sp) in high_spread.drain(..) {
                match pairwise {
                    Some(floor) => notes.push(format!(
                        "`{metric}` on {leg_id}/{wl}: spread is {:.0}% at n={}, so a difference \
                         below ~{floor:.0}% cannot be told from noise — raise `iterations` if \
                         the effect you are chasing is smaller than that.",
                        100.0 * sp.cv,
                        sp.n
                    )),
                    None => notes.push(format!(
                        "`{metric}` on {leg_id}/{wl}: spread is {:.0}% at n={}.",
                        100.0 * sp.cv,
                        sp.n
                    )),
                }
            }
            if legs.len() == 2 {
                match (stats[0], stats[1]) {
                    (Some(a), Some(b)) if a.mean != 0.0 => {
                        let delta = 100.0 * (b.mean - a.mean) / a.mean;
                        let noise = pairwise.unwrap_or(0.0);
                        if delta.abs() < noise {
                            write!(s, " {delta:+.1}% (within noise, ±{noise:.1}%) |").unwrap();
                        } else {
                            write!(s, " **{delta:+.1}%** |").unwrap();
                        }
                    }
                    _ => write!(s, " — |").unwrap(),
                }
            }
            writeln!(s).unwrap();
        }
        writeln!(s).unwrap();
    }

    if crate::cancel::requested() {
        notes.push(
            "the campaign was interrupted, so some cells hold fewer iterations than the \
             suite asked for — check each cell's n before comparing them"
                .to_string(),
        );
    }
    if !notes.is_empty() {
        writeln!(s, "## Notes\n").unwrap();
        notes.sort();
        notes.dedup();
        for n in &notes {
            writeln!(s, "- {n}").unwrap();
        }
        writeln!(s).unwrap();
    }

    writeln!(
        s,
        "A delta in **bold** is larger than the combined standard error of the two \
         legs; one marked *within noise* is not, and repeating the campaign will \
         move it. Read `instructions.per_reflow.*` in preference to the totals: \
         numerator and denominator co-vary, so the ratio cancels most of the \
         load-to-load variance that makes the totals swing. Compare \
         `layout_proper` against `layout_proper` — the groupings are defined per \
         engine in `workloads/_instructions.toml` so that they mean the same \
         phase on both sides."
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
