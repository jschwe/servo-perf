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
    /// Run a workload paired against two binaries, sequentially: all base
    /// iterations first, then all patch iterations. In OHOS mode this lets
    /// both haps share a single bundle name (re-installing once between
    /// phases) at the cost of any drift in system state between phases.
    Ab(AbArgs),
    /// Run a workload and compare against a saved baseline JSON.
    Regression(RegressionArgs),
    /// Build an unstripped `libarkweb_engine.so` from a stripped on-device
    /// copy by re-attaching the `.symtab`/`.strtab` carried in the
    /// `.gnu_debugdata` MiniDebugInfo section, then push it to the device.
    /// Required once before `bench … --with-instructions` for ArkWeb-based
    /// targets (the on-device library has only `.dynsym`, and hiperf's
    /// `--symbol-dir` ignores `.gnu_debugdata`).
    #[command(name = "prepare-arkweb-symbols")]
    PrepareArkwebSymbols(PrepareArkwebSymbolsArgs),
    /// Dump every span above a duration threshold from a pftrace,
    /// grouped by thread. Useful when the registry-based critical-path
    /// report doesn't surface enough detail.
    Dump(DumpArgs),
}

/// Flags that select / configure a HarmonyOS device target reached over
/// `hdc`. When `--ohos` is set, `--bin` (or its A/B equivalents) becomes
/// optional: the .hap is expected to already be installed on the device,
/// or installed once before the run if `--bin` points at a `.hap` file.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct OhosArgs {
    /// Run against a HarmonyOS device via `hdc` instead of executing
    /// servoshell locally. Tracing uses `hitrace` (the
    /// `tracing-hitrace` backend) on the device — there is no `.pftrace`.
    #[arg(long)]
    pub ohos: bool,
    /// `hdc` server address (`host:port`). Forwarded as `-s <addr>`.
    /// Leave unset to use the local hdc server.
    /// See `~/.claude/skills/ohos-performance-testing/resources/remote-hdc.md`
    /// for the remote-server / SSH-tunnel setup.
    #[arg(long)]
    pub hdc_server: Option<String>,
    /// `hdc` binary to invoke (must match the device's hdc version).
    #[arg(long, default_value = "hdc")]
    pub hdc_bin: String,
    /// Bundle name to launch.
    #[arg(long, default_value = "org.servo.servo")]
    pub ohos_bundle: String,
    /// UIAbility name to launch.
    #[arg(long, default_value = "EntryAbility")]
    pub ohos_ability: String,
    /// Where on the device the captured hitrace text is written.
    #[arg(long, default_value = "/data/local/tmp/servoperf_hitrace.txt")]
    pub ohos_trace_path: String,
    /// Hitrace ring buffer in KiB.
    #[arg(long, default_value_t = 524_288)]
    pub ohos_trace_buffer_kib: u64,
    /// Comma-separated hitrace tag list (passed as positional args to
    /// `hitrace`). The default mirrors the servo CI bencher.
    #[arg(long, default_value = "app,graphic,ohos,freq,idle,memory")]
    pub ohos_trace_tags: String,
    /// Seconds to sleep after `aa start` before stopping the trace.
    /// Should comfortably exceed the workload's expected first-paint
    /// time. The CI bencher uses 10 s as its default.
    #[arg(long, default_value_t = 10)]
    pub ohos_capture_seconds: u64,
    /// `persist.hitrace.level.threshold` to set on the device for the
    /// duration of the run. Servo's `tracing-hitrace` layer maps each
    /// `tracing::Level` through to a hitrace level (`TRACE`/`DEBUG` →
    /// `Debug`, `INFO` → `Info`, ...) — and the OHOS hitrace daemon's
    /// default threshold is `Info`, which silently drops every
    /// TRACE-level Servo span (`Servo::new`, `script::init`,
    /// `Window::reflow`, `perform_updates`, `render`, every `handle_*`,
    /// ...). servoperf snapshots the current threshold, sets this
    /// value before the first iteration, and restores the original on
    /// drop. Pass an empty string to leave the threshold alone (e.g.
    /// when a higher-priority caller — like a parent CI job — has
    /// already configured it).
    #[arg(long, default_value = "Debug")]
    pub ohos_trace_level: String,
    /// Seconds to sleep after each `hdc install` so the device's SoC
    /// can cool from install-time CPU load (sustained `hdc install`
    /// of the ~90 MB servoshell hap pushes the SoC up by 10+ °C on the
    /// reference device) before the first iteration. The cooldown
    /// brackets are logged so it's visible whether the wait was
    /// sufficient. Set to 0 to skip.
    #[arg(long, default_value_t = 15)]
    pub ohos_post_install_cooldown_seconds: u64,
    /// Seconds to keep the warmup `aa start about:blank` alive after
    /// install (run between cooldown and the first measured iteration).
    /// First-launch-after-install on OHOS pays a one-time sandbox
    /// initialisation cost (inode/mmap warming, JIT cache priming);
    /// without this the first measured iteration is ~3× the steady-state
    /// FCP. Set to 0 to skip.
    #[arg(long, default_value_t = 5)]
    pub ohos_warmup_seconds: u64,
    /// Wall-clock window (seconds) for one WPR record pass on OHOS.
    /// Only consulted when a `wpr-replay` workload is running with
    /// `--ohos` and the .wprgo archive is missing — `servoperf` then
    /// drives a one-shot record pass through `aa start` against the
    /// live origin via `wpr_tunnel`. The window must comfortably
    /// exceed page load + lazy-load tail; 45 s captures
    /// `cdn-huaweimossel` cleanly including its image set (15 s
    /// missed several hundred MB worth of jpgs, producing an archive
    /// that replayed without pictures). For replay this flag is
    /// unused; that's bounded by `--ohos-capture-seconds`.
    #[arg(long, default_value_t = 45)]
    pub ohos_record_seconds: u64,
    /// Collect per-function inclusive instruction counts per iteration via
    /// `hiperf record -a -e hw-instructions`, running in parallel with the
    /// hitrace capture. The per-engine symbol list lives in
    /// `workloads/_instructions.toml`; engine selection is driven by
    /// `--ohos-bundle`. For ArkWeb-based bundles, run
    /// `servoperf prepare-arkweb-symbols …` once before benching so hiperf
    /// can resolve symbols in `libarkweb_engine.so`.
    #[arg(long)]
    pub with_instructions: bool,
    /// hiperf sampling period (events between samples) when
    /// `--with-instructions` is set. 100 000 retired instructions per
    /// sample matches what the manual investigation used and produces
    /// useful resolution without overwhelming the device.
    #[arg(long, default_value_t = 100_000)]
    pub instructions_period: u64,
}

#[derive(clap::Args, Clone)]
pub struct PrepareArkwebSymbolsArgs {
    /// Path to a stripped `libarkweb_engine.so` (the file that ships on
    /// device, e.g. pulled via
    /// `hdc shell cp /data/app/el1/bundle/public/com.huawei.hmos.arkwebcore/libs/arm64/libarkweb_engine.so /data/local/tmp/x && hdc file recv /data/local/tmp/x …`).
    /// Must contain a `.gnu_debugdata` section.
    #[arg(long)]
    pub input: PathBuf,
    /// Where to write the merged unstripped ELF on the host. Defaults to a
    /// sibling of the input named `libarkweb_engine.merged.so`.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Push the merged .so to the device under
    /// `/data/local/tmp/symbols/libarkweb_engine.so` so subsequent
    /// `bench --with-instructions` runs find it automatically. Set to
    /// false to write only the host file.
    #[arg(long, default_value_t = true)]
    pub push: bool,
    /// `hdc` binary used for the push step.
    #[arg(long, default_value = "hdc")]
    pub hdc_bin: String,
    /// `hdc -s <addr>` server forwarder (containerised agent setups). Leave
    /// unset to use the local daemon.
    #[arg(long)]
    pub hdc_server: Option<String>,
}

#[derive(clap::Args, Clone)]
pub struct BenchArgs {
    /// Workload name (looked up in tools/servoperf/workloads/).
    pub workload: String,
    /// Prebuilt servoshell to measure. In OHOS mode this is optional;
    /// when given it must point at a `.hap` and is installed once before
    /// the run.
    #[arg(long)]
    pub bin: Option<PathBuf>,
    /// Override the workload's default iteration count.
    #[arg(long)]
    pub iterations: Option<u32>,
    /// Output directory. Defaults to `out/<workload>-<UTC-timestamp>/`.
    #[arg(long)]
    pub out: Option<PathBuf>,
    #[command(flatten)]
    pub ohos: OhosArgs,
}

#[derive(clap::Args, Clone)]
pub struct AbArgs {
    pub workload: String,
    /// Base servoshell binary, or `.hap` in OHOS mode. In OHOS mode the
    /// hap is installed before the base phase begins — relying on a
    /// pre-installed bundle would risk measuring stale code.
    #[arg(long)]
    pub base_bin: PathBuf,
    /// Patch servoshell binary, or `.hap` in OHOS mode. In OHOS mode the
    /// hap is installed between the base and patch phases, overwriting
    /// the base install (both phases use `--ohos-bundle`).
    #[arg(long)]
    pub patch_bin: PathBuf,
    #[arg(long)]
    pub iterations: Option<u32>,
    #[arg(long)]
    pub out: Option<PathBuf>,
    #[command(flatten)]
    pub ohos: OhosArgs,
}

#[derive(clap::Args, Clone)]
pub struct DumpArgs {
    /// Path to a .pftrace file.
    pub pftrace: PathBuf,
    /// Minimum slice duration to print (ms).
    #[arg(long, default_value_t = 1.0)]
    pub min_dur_ms: f64,
    /// Only print slices starting before this timestamp (ms from t=0).
    /// Default 700 ms — covers up to FCP on mossel WPR replay.
    #[arg(long, default_value_t = 700.0)]
    pub until_ms: f64,
}

#[derive(clap::Args, Clone)]
pub struct RegressionArgs {
    pub workload: String,
    #[arg(long)]
    pub bin: Option<PathBuf>,
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
    #[command(flatten)]
    pub ohos: OhosArgs,
}
