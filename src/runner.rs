// tools/servoperf/src/runner.rs
//! Runs one servoshell iteration, captures its pftrace.

use anyhow::{Context, Result};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::workload::Workload;

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

/// Run `bin` against `workload` once. Returns a [`RunArtifact`] containing
/// the path to the copied `iter_<iter>.pftrace` and wallclock brackets.
///
/// `proxy_uri`, when `Some`, is passed to servoshell via `https_proxy` /
/// `http_proxy` env vars so its fetches route through e.g. the
/// `wpr-replay` tunnel.
///
/// `timeout` bounds how long we wait for servoshell to exit. A hung
/// servoshell (e.g. a page script retrying a 404 forever) is SIGKILL'd
/// and the iteration is reported as a [`RunError::Timeout`]. The caller
/// picks the bound dynamically from the median of prior successful
/// iterations (see `bench.rs` / `ab.rs`).
pub fn run_once(
    bin: &Path,
    workload: &Workload,
    iter: u32,
    out_dir: &Path,
    proxy_uri: Option<&str>,
    timeout: Duration,
) -> Result<RunArtifact> {
    if !bin.is_file() {
        anyhow::bail!(RunError::BinaryNotFound(bin.into()));
    }
    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    // Each iteration runs in a unique cwd so servo.pftrace lands in isolation.
    let iter_cwd = out_dir.join(format!("cwd_{iter}"));
    fs::create_dir_all(&iter_cwd).with_context(|| format!("creating {}", iter_cwd.display()))?;

    let mut cmd = Command::new(bin);
    cmd.current_dir(&iter_cwd);
    cmd.arg("--headless").arg("--exit");
    cmd.arg("--tracing-filter").arg(&workload.tracing_filter);
    cmd.arg("-o").arg(iter_cwd.join("out.png"));
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
