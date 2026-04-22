// tools/servoperf/src/cli.rs
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "servoperf", version, about = "Measure Servo startup performance")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run a workload once against a single binary.
    Bench(BenchArgs),
    /// Run a workload paired against two binaries, interleaved.
    Ab(AbArgs),
    /// Run a workload and compare against a saved baseline JSON.
    Regression(RegressionArgs),
}

#[derive(clap::Args, Clone)]
pub struct BenchArgs {
    /// Workload name (looked up in tools/servoperf/workloads/).
    pub workload: String,
    /// Prebuilt servoshell to measure.
    #[arg(long)]
    pub bin: PathBuf,
    /// Override the workload's default iteration count.
    #[arg(long)]
    pub iterations: Option<u32>,
    /// Output directory. Defaults to `out/<workload>-<UTC-timestamp>/`.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(clap::Args, Clone)]
pub struct AbArgs {
    pub workload: String,
    #[arg(long)]
    pub base_bin: PathBuf,
    #[arg(long)]
    pub patch_bin: PathBuf,
    #[arg(long)]
    pub iterations: Option<u32>,
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(clap::Args, Clone)]
pub struct RegressionArgs {
    pub workload: String,
    #[arg(long)]
    pub bin: PathBuf,
    /// Path to a previous `raw.json` to compare against.
    #[arg(long)]
    pub baseline: PathBuf,
    /// Regression threshold (percent). Exit 1 if FCP p50 worsens by more.
    #[arg(long, default_value_t = 5.0)]
    pub threshold: f64,
    #[arg(long)]
    pub iterations: Option<u32>,
    #[arg(long)]
    pub out: Option<PathBuf>,
}
