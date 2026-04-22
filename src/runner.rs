// tools/servoperf/src/runner.rs
//! Runs one servoshell iteration, captures its pftrace.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::workload::Workload;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("servoshell binary not found or not executable: {0}")]
    BinaryNotFound(PathBuf),
    #[error("servoshell exited with status {code} — stderr tail:\n{stderr}")]
    NonZeroExit { code: i32, stderr: String },
    #[error("servoshell produced no servo.pftrace in {dir}")]
    MissingTrace { dir: PathBuf },
}

/// Run `bin` against `workload` once. Returns the path to the copied
/// `iter_<iter>.pftrace` under `out_dir`.
pub fn run_once(bin: &Path, workload: &Workload, iter: u32, out_dir: &Path) -> Result<PathBuf> {
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

    let output = cmd.output().with_context(|| format!("spawning {}", bin.display()))?;
    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr)
            .lines()
            .rev()
            .take(15)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
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
    Ok(dest)
}
