//! `servoperf prepare-arkweb-symbols` subcommand.
//!
//! Builds an unstripped `libarkweb_engine.so` (suitable for hiperf's
//! `--symbol-dir`) from the stripped on-device copy, by re-attaching the
//! `.symtab`/`.strtab` recovered from the `.gnu_debugdata` MiniDebugInfo
//! section. See [`crate::instructions::symbols`] for the merge logic and
//! the rationale for each step.
//!
//! The default flow writes the merged file next to the input and pushes
//! it to `/data/local/tmp/symbols/libarkweb_engine.so` on the device so
//! the next `bench --with-instructions` run finds it automatically.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::cli::PrepareArkwebSymbolsArgs;
use crate::instructions::{symbols, DevicePaths};

pub fn run(args: PrepareArkwebSymbolsArgs) -> Result<()> {
    let input = args.input.clone();
    anyhow::ensure!(
        input.is_file(),
        "input ELF not found at {} — pull the on-device \
         libarkweb_engine.so first (see docs)",
        input.display()
    );
    let output = args.output.clone().unwrap_or_else(|| {
        // Default sibling: <input-dir>/libarkweb_engine.merged.so
        let parent = input.parent().unwrap_or_else(|| std::path::Path::new("."));
        parent.join("libarkweb_engine.merged.so")
    });

    eprintln!(
        "prepare-arkweb-symbols: {} → {}",
        input.display(),
        output.display()
    );
    symbols::merge_symbols(&input, &output)
        .with_context(|| format!("merging symbols into {}", output.display()))?;
    eprintln!(
        "prepare-arkweb-symbols: wrote {} ({} bytes)",
        output.display(),
        std::fs::metadata(&output)?.len(),
    );

    if args.push {
        push_to_device(&output, args.hdc_bin.as_str(), args.hdc_server.as_deref())?;
        eprintln!(
            "prepare-arkweb-symbols: pushed to {}",
            DevicePaths::SYMBOL_FILE
        );
    } else {
        eprintln!(
            "prepare-arkweb-symbols: skipped device push (--push=false). \
             Push manually with: hdc file send {} {}",
            output.display(),
            DevicePaths::SYMBOL_FILE,
        );
    }

    Ok(())
}

fn push_to_device(
    host_path: &std::path::Path,
    hdc_bin: &str,
    hdc_server: Option<&str>,
) -> Result<()> {
    fn run_hdc(hdc_bin: &str, hdc_server: Option<&str>, args: &[&str]) -> Result<()> {
        let mut cmd = Command::new(hdc_bin);
        if let Some(s) = hdc_server {
            cmd.args(["-s", s]);
        }
        cmd.args(args);
        cmd.stdin(Stdio::null());
        let out = cmd
            .output()
            .with_context(|| format!("running {hdc_bin} {args:?}"))?;
        anyhow::ensure!(
            out.status.success(),
            "hdc {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr),
        );
        Ok(())
    }
    run_hdc(
        hdc_bin,
        hdc_server,
        &["shell", "mkdir", "-p", DevicePaths::SYMBOL_DIR],
    )?;
    let host_str = host_path.to_string_lossy().to_string();
    run_hdc(
        hdc_bin,
        hdc_server,
        &["file", "send", &host_str, DevicePaths::SYMBOL_FILE],
    )?;
    Ok(())
}

/// Convenience: where to write the merged .so when the caller hasn't
/// explicitly asked. Lives next to the input.
#[allow(dead_code)]
fn default_output(input: &std::path::Path) -> PathBuf {
    let parent = input.parent().unwrap_or_else(|| std::path::Path::new("."));
    parent.join("libarkweb_engine.merged.so")
}
