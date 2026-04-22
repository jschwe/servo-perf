// tools/servoperf/src/main.rs
//! servoperf — Servo startup-performance workflow.
//!
//! See docs/superpowers/specs/2026-04-22-startup-perf-workflow-design.md.

mod workload;
mod trace;
mod proto;
mod fixtures;
mod runner;
mod stats;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "servoperf", version, about = "Measure Servo startup performance")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a workload once against a single binary.
    Bench,
    /// Run a workload paired against two binaries, interleaved.
    Ab,
    /// Run a workload and compare against a saved baseline JSON.
    Regression,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Bench => anyhow::bail!("bench: not implemented yet"),
        Command::Ab => anyhow::bail!("ab: not implemented yet"),
        Command::Regression => anyhow::bail!("regression: not implemented yet"),
    }
}
