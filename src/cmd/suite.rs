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
