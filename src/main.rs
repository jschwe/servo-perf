// tools/servoperf/src/main.rs
//! servoperf — Servo startup-performance workflow.

mod cli;
mod cmd;
mod fixtures;
mod ohos;
mod proto;
mod report;
mod runner;
mod stats;
mod trace;
mod workload;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let args = cli::Cli::parse();
    match args.command {
        cli::Command::Bench(a) => cmd::bench::run(a),
        cli::Command::Ab(a) => cmd::ab::run(a),
        cli::Command::Regression(a) => cmd::regression::run(a),
    }
}
