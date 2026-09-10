// tools/servoperf/src/main.rs
//! servoperf — Servo startup-performance workflow.

mod cancel;
mod cli;
mod cmd;
mod cpufreq;
mod fixtures;
mod fps;
mod instructions;
mod log_once;
mod ohos;
#[cfg(feature = "pftrace")]
mod proto;
mod report;
mod runner;
mod stats;
mod suite;
mod threads;
mod trace;
mod workload;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    cancel::install();
    let args = cli::Cli::parse();
    match args.command {
        cli::Command::Bench(a) => cmd::bench::run(a),
        cli::Command::Ab(a) => cmd::ab::run(a),
        #[cfg(feature = "pftrace")]
        cli::Command::Dump(a) => cmd::dump::run(a),
        cli::Command::CpuFreq(a) => cpufreq::run(&a),
        cli::Command::Regression(a) => cmd::regression::run(a),
        cli::Command::Suite(a) => cmd::suite::run(a),
        cli::Command::PrepareArkwebSymbols(a) => cmd::prepare_symbols::run(a),
    }
}
