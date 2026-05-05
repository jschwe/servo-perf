// tools/servoperf/src/runner.rs
//! Runs one servoshell iteration and produces a parseable trace file.
//!
//! Two targets are supported:
//!   * **Local** — spawn `servoshell` as a subprocess on the host, copy
//!     out `servo.pftrace` (perfetto binary).
//!   * **OHOS** — drive a HarmonyOS device via `hdc`, capture `hitrace`
//!     text, pull it via `hdc file recv`. See [`crate::ohos`].

use anyhow::{Context, Result};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::ohos::OhosTarget;
use crate::workload::Workload;

/// Where this iteration is going to execute. Built once per servoperf
/// invocation from CLI args, then passed by reference into [`run_once`].
#[derive(Debug, Clone)]
pub enum Target {
    /// Spawn a local servoshell binary (the original behaviour).
    Local { bin: PathBuf },
    /// Drive a HarmonyOS device. The .hap is expected to already be
    /// installed; install once via `OhosTarget::install_hap` before the
    /// loop if you have a hap path.
    Ohos(OhosTarget),
}

impl Target {
    /// Path-shaped identifier for `report.rs` ("which binary did we
    /// measure?"). For OHOS this is the device's bundle name dressed up
    /// as a fake path so existing JSON consumers keep working.
    pub fn bin_label(&self) -> PathBuf {
        match self {
            Target::Local { bin } => bin.clone(),
            Target::Ohos(t) => PathBuf::from(format!("ohos://{}", t.bundle)),
        }
    }

    /// Filename stem (under `workloads/`) of the critical-path registry
    /// to use for this target. OHOS lacks several spans desktop has
    /// (FCP / FirstPaint), and labels its `Script` thread differently,
    /// so it gets its own registry.
    pub fn registry_stem(&self) -> &'static str {
        match self {
            Target::Local { .. } => "_critical_path",
            Target::Ohos(_) => "_critical_path_ohos",
        }
    }

    /// Span name used as the headline "first-paint-like" milestone for
    /// this target. Both desktop and OHOS now report
    /// `FirstContentfulPaint` — servo's metrics setters emit a tracing
    /// span tagged `servo_profiling = true` (see
    /// `components/metrics/lib.rs`), which both the PerfettoLayer
    /// (desktop) and HitraceLayer (OHOS) pick up. The OHOS-only
    /// `PageLoadEndedPrompt` fallback stays in the registry as a
    /// further-down phase so it still shows in the critical-path
    /// table for diagnostic purposes.
    pub fn primary_milestone(&self) -> &'static str {
        match self {
            Target::Local { .. } => "FirstContentfulPaint",
            Target::Ohos(_) => "FirstContentfulPaint",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("servoshell binary not found or not executable: {0}")]
    BinaryNotFound(PathBuf),
    #[error("servoshell exited with status {code} — stderr tail:\n{stderr}")]
    NonZeroExit { code: i32, stderr: String },
    #[error("servoshell produced no servo.pftrace in {dir}")]
    MissingTrace { dir: PathBuf },
    #[error(
        "servoshell iteration hung past {timeout_s}s — killed. stderr tail:\n{stderr}"
    )]
    Timeout { timeout_s: u64, stderr: String },
}

/// Artifacts from a single servoshell run.
pub struct RunArtifact {
    pub pftrace: PathBuf,
    pub spawn_wall_ns: u64,
    pub exit_wall_ns: u64,
}

/// Run a single iteration against `target`. Returns a [`RunArtifact`]
/// containing the path to the produced trace file and wallclock
/// brackets.
///
/// `proxy_uri`, when `Some`, is passed to servoshell via `https_proxy` /
/// `http_proxy` env vars (Local) or as `network_*_proxy_uri` prefs
/// (OHOS) so its fetches route through e.g. the `wpr-replay` tunnel.
///
/// `timeout` bounds the local case only — OHOS uses
/// `OhosTarget::capture_seconds` instead, which the device-side hitrace
/// capture window already enforces.
pub fn run_once(
    target: &Target,
    workload: &Workload,
    iter: u32,
    out_dir: &Path,
    proxy_uri: Option<&str>,
    timeout: Duration,
) -> Result<RunArtifact> {
    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let bin = match target {
        Target::Local { bin } => bin.as_path(),
        Target::Ohos(ohos) => {
            let art = ohos.run_iteration(workload, iter, out_dir, proxy_uri)?;
            return Ok(RunArtifact {
                pftrace: art.trace,
                spawn_wall_ns: art.spawn_wall_ns,
                exit_wall_ns: art.exit_wall_ns,
            });
        }
    };
    if !bin.is_file() {
        anyhow::bail!(RunError::BinaryNotFound(bin.into()));
    }
    // Each iteration runs in a unique cwd so servo.pftrace lands in isolation.
    let iter_cwd = out_dir.join(format!("cwd_{iter}"));
    fs::create_dir_all(&iter_cwd).with_context(|| format!("creating {}", iter_cwd.display()))?;

    let mut cmd = Command::new(bin);
    cmd.current_dir(&iter_cwd);
    cmd.arg("--headless").arg("--exit");
    cmd.arg("--tracing-filter").arg(&workload.tracing_filter);
    cmd.arg("-o").arg(iter_cwd.join("out.png"));
    // LCP fragment-area accounting is gated by an off-by-default pref;
    // servoperf cares about LCP on every workload, so enable it
    // unconditionally here. Mirrored in
    // `crate::ohos::workload_args_to_aa_params` for the OHOS path.
    cmd.arg("--pref").arg("largest_contentful_paint_enabled=true");
    if let Some((w, h)) = workload.viewport {
        cmd.arg("--window-size").arg(format!("{}x{}", w, h));
    }
    if let Some(ratio) = workload.device_pixel_ratio {
        cmd.arg("--device-pixel-ratio").arg(ratio.to_string());
    }
    if let Some(ua) = workload.user_agent.as_deref() {
        cmd.arg("-u").arg(ua);
    }
    for extra in &workload.servoshell_args {
        cmd.arg(extra);
    }
    cmd.arg(&workload.url);
    if let Some(proxy) = proxy_uri {
        cmd.env("https_proxy", proxy);
        cmd.env("http_proxy", proxy);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let spawn_wall_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", bin.display()))?;

    // Wait for exit or timeout. Poll every 50 ms — cheap relative to
    // the typical 500 ms – 5 s iteration, and bounds kill latency.
    let deadline = Instant::now() + timeout;
    let exit_status = loop {
        match child.try_wait().context("polling servoshell child")? {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let stderr = collect_stderr_tail(&mut child);
                    anyhow::bail!(RunError::Timeout {
                        timeout_s: timeout.as_secs(),
                        stderr,
                    });
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    let exit_wall_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    if !exit_status.success() {
        let code = exit_status.code().unwrap_or(-1);
        let stderr = collect_stderr_tail(&mut child);
        anyhow::bail!(RunError::NonZeroExit { code, stderr });
    }

    // Copy servo.pftrace out of the per-iteration cwd.
    let source = iter_cwd.join("servo.pftrace");
    if !source.is_file() {
        anyhow::bail!(RunError::MissingTrace { dir: iter_cwd.clone() });
    }
    let dest = out_dir.join(format!("iter_{iter}.pftrace"));
    fs::copy(&source, &dest).with_context(|| {
        format!("copying {} → {}", source.display(), dest.display())
    })?;
    // Clean up the iteration cwd (but keep the pftrace outside it).
    let _ = fs::remove_dir_all(&iter_cwd);
    Ok(RunArtifact { pftrace: dest, spawn_wall_ns, exit_wall_ns })
}

/// Drain whatever is pending on the child's stderr pipe and return the
/// last ~15 lines. Best-effort — returns empty string on any read error.
fn collect_stderr_tail(child: &mut std::process::Child) -> String {
    let Some(mut stderr) = child.stderr.take() else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = stderr.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(15);
    lines[start..].join("\n")
}

/// Pick a timeout for the next iteration based on how long the
/// successfully-completed ones took. Caller accumulates the wall-clock
/// times of successful iterations and passes them here each time.
///
/// Policy (per user agreement): `max(20s, 10 × median_successful)`. The
/// 20 s floor keeps the first few iterations (no history) reasonable;
/// the 10× factor tolerates transient slowness without letting a truly
/// hung run stall forever.
pub fn pick_timeout(successful_durations: &[Duration]) -> Duration {
    const MIN: Duration = Duration::from_secs(20);
    if successful_durations.is_empty() {
        return MIN;
    }
    let mut sorted: Vec<Duration> = successful_durations.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    let ten_x = median.saturating_mul(10);
    if ten_x > MIN { ten_x } else { MIN }
}
