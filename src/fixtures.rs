// tools/servoperf/src/fixtures.rs
//! RAII lifecycle for localhost fixture servers.
//!
//! Three kinds of fixtures are supported:
//!   * `Fixture::Http1` / `Fixture::Http2` — a Rust static-file server
//!     spawned from the sibling `fixture_server` binary.
//!   * `Fixture::WprReplay` — a Web Page Replay archive served through
//!     the patched `wpr` binary plus a small CONNECT-tunneling shim
//!     (the sibling `wpr_tunnel` binary). If the archive file doesn't
//!     exist, one record pass is made against the live origin before
//!     switching to replay mode.

use anyhow::{Context, Result};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::workload::{Fixture, Workload};

pub struct FixtureHandle {
    /// Subprocesses this fixture owns. All are SIGKILL'd on Drop.
    children: Vec<Child>,
    /// Primary TCP port (for diagnostics). Zero means "not applicable".
    port: u16,
    /// When `Some(uri)`, the runner must launch servoshell with
    /// `https_proxy` / `http_proxy` set to this URI so its fetches route
    /// through the fixture (currently only set by `wpr-replay`).
    proxy_uri: Option<String>,
}

impl FixtureHandle {
    #[cfg(test)]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Proxy URI that servoshell should be started with, if any.
    pub fn proxy_uri(&self) -> Option<&str> {
        self.proxy_uri.as_deref()
    }
}

impl Drop for FixtureHandle {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Spawn the fixture described by `workload.fixture` (must be `Some`).
/// `servoshell_bin` is only consulted when a `wpr-replay` fixture needs
/// a one-time recording pass; it is otherwise unused.
pub fn spawn(
    workloads_dir: &Path,
    workload: &Workload,
    servoshell_bin: &Path,
    out_dir: &Path,
) -> Result<FixtureHandle> {
    let fixture = workload
        .fixture
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("workload has no fixture to spawn"))?;
    match fixture {
        Fixture::Http1 { port, doc_root } => {
            spawn_local_server(workloads_dir, "http1", *port, doc_root, out_dir)
        }
        Fixture::Http2 { port, doc_root } => {
            spawn_local_server(workloads_dir, "http2", *port, doc_root, out_dir)
        }
        Fixture::WprReplay {
            archive,
            wpr_port,
            tunnel_port,
        } => spawn_wpr_replay(
            workloads_dir,
            workload,
            servoshell_bin,
            archive,
            *wpr_port,
            *tunnel_port,
            out_dir,
        ),
    }
}

// -- Http1 / Http2 static-file fixtures ---------------------------------

fn spawn_local_server(
    workloads_dir: &Path,
    mode: &str,
    port: u16,
    doc_root_rel: &Path,
    out_dir: &Path,
) -> Result<FixtureHandle> {
    let fixtures_dir = workloads_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("workloads_dir has no parent"))?
        .join("fixtures");
    let doc_root: PathBuf = fixtures_dir.join(doc_root_rel);
    anyhow::ensure!(
        doc_root.is_dir(),
        "fixture doc_root not a directory: {}",
        doc_root.display()
    );

    // Ensure TLS cert exists.
    let _ = Command::new("sh")
        .arg(fixtures_dir.join("gen_cert.sh"))
        .status()
        .context("running gen_cert.sh")?;

    preflight_port(port)?;

    let fixture_server_bin = sibling_binary("fixture_server")?;

    let child = Command::new(&fixture_server_bin)
        .arg(format!("--mode={mode}"))
        .arg(port.to_string())
        .arg(&doc_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(open_log(out_dir, "fixture_server.stderr")?)
        .spawn()
        .context("spawning fixture_server")?;

    let mut handle = FixtureHandle {
        children: vec![child],
        port,
        proxy_uri: None,
    };
    wait_for_accept(&mut handle, port, "fixture_server")?;
    Ok(handle)
}

// -- WPR replay fixture -------------------------------------------------

fn spawn_wpr_replay(
    workloads_dir: &Path,
    workload: &Workload,
    servoshell_bin: &Path,
    archive_rel: &Path,
    wpr_port: u16,
    tunnel_port: u16,
    out_dir: &Path,
) -> Result<FixtureHandle> {
    let archives_dir = workloads_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("workloads_dir has no parent"))?
        .join("wpr-archives");
    std::fs::create_dir_all(&archives_dir)
        .with_context(|| format!("creating {}", archives_dir.display()))?;
    let archive = if archive_rel.is_absolute() {
        archive_rel.to_path_buf()
    } else {
        archives_dir.join(archive_rel)
    };

    preflight_port(wpr_port)?;
    preflight_port(tunnel_port)?;

    let wpr_bin = std::env::var_os("SERVOPERF_WPR_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs_home().map(|h| h.join("bin").join("wpr")).unwrap_or_else(
                || PathBuf::from("wpr"),
            )
        });
    anyhow::ensure!(
        wpr_bin.is_file() || which_on_path(&wpr_bin).is_some(),
        "wpr binary not found at {} (set SERVOPERF_WPR_BIN to override). \
         See tools/servoperf/docs/wpr-setup.md.",
        wpr_bin.display()
    );

    let cert_path = std::env::var_os("SERVOPERF_WPR_CERT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs_home()
                .map(|h| h.join("wpr").join("wpr_cert.pem"))
                .unwrap_or_default()
        });
    let key_path = std::env::var_os("SERVOPERF_WPR_KEY")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs_home()
                .map(|h| h.join("wpr").join("wpr_key.pem"))
                .unwrap_or_default()
        });
    anyhow::ensure!(
        cert_path.is_file() && key_path.is_file(),
        "WPR cert/key not found at {} / {} (set SERVOPERF_WPR_CERT / \
         SERVOPERF_WPR_KEY to override). See tools/servoperf/docs/wpr-setup.md.",
        cert_path.display(),
        key_path.display()
    );

    // If the archive doesn't exist yet, make one recording pass against
    // the live origin. This runs WPR in record mode, starts the tunnel,
    // runs servoshell once at the workload URL, then tears WPR down with
    // SIGINT so it flushes the archive to disk before we start replay.
    if !archive.exists() {
        eprintln!(
            "wpr-replay: archive {} missing — recording one pass from live origin",
            archive.display()
        );
        record_one_pass(
            &wpr_bin,
            &cert_path,
            &key_path,
            &archive,
            wpr_port,
            tunnel_port,
            workload,
            servoshell_bin,
            out_dir,
        )
        .with_context(|| "wpr-replay record pass")?;
        anyhow::ensure!(
            archive.is_file() && std::fs::metadata(&archive).map(|m| m.len() > 0).unwrap_or(false),
            "record pass did not produce a non-empty archive at {}",
            archive.display()
        );
        eprintln!(
            "wpr-replay: recorded {} bytes to {}",
            std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0),
            archive.display()
        );
    }

    // Replay phase: WPR + tunnel stay up for the duration of the bench.
    let wpr_child = spawn_wpr(
        &wpr_bin,
        "replay",
        &cert_path,
        &key_path,
        &archive,
        wpr_port,
        open_log(out_dir, "wpr.stderr")?,
    )?;
    let tunnel_child = spawn_tunnel(tunnel_port, wpr_port, open_log(out_dir, "tunnel.stderr")?)?;
    let mut handle = FixtureHandle {
        children: vec![wpr_child, tunnel_child],
        port: tunnel_port,
        proxy_uri: Some(format!("http://127.0.0.1:{tunnel_port}")),
    };
    wait_for_accept(&mut handle, wpr_port, "wpr")?;
    wait_for_accept(&mut handle, tunnel_port, "wpr_tunnel")?;
    Ok(handle)
}

fn record_one_pass(
    wpr_bin: &Path,
    cert_path: &Path,
    key_path: &Path,
    archive: &Path,
    wpr_port: u16,
    tunnel_port: u16,
    workload: &Workload,
    servoshell_bin: &Path,
    out_dir: &Path,
) -> Result<()> {
    let wpr_child = spawn_wpr(
        wpr_bin,
        "record",
        cert_path,
        key_path,
        archive,
        wpr_port,
        open_log(out_dir, "wpr-record.stderr")?,
    )?;
    let tunnel_child = spawn_tunnel(
        tunnel_port,
        wpr_port,
        open_log(out_dir, "tunnel-record.stderr")?,
    )?;
    let mut handle = FixtureHandle {
        children: vec![wpr_child, tunnel_child],
        port: tunnel_port,
        proxy_uri: Some(format!("http://127.0.0.1:{tunnel_port}")),
    };
    wait_for_accept(&mut handle, wpr_port, "wpr")?;
    wait_for_accept(&mut handle, tunnel_port, "wpr_tunnel")?;

    // One servoshell run; we don't care about the trace output.
    run_servoshell_once(servoshell_bin, workload, &handle)
        .context("servoshell record pass")?;

    // SIGINT WPR so it flushes the archive. The child is the second-to-
    // last entry in handle.children (wpr was pushed first); but for
    // robustness, send SIGINT to whichever one is the wpr binary.
    flush_wpr(&mut handle);
    // FixtureHandle::drop still runs on function return: it SIGKILLs
    // anything that didn't already exit, and waits.
    drop(handle);
    // Give WPR a moment to close the archive cleanly after SIGINT.
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}

fn spawn_wpr(
    wpr_bin: &Path,
    mode: &str,
    cert_path: &Path,
    key_path: &Path,
    archive: &Path,
    wpr_port: u16,
    stderr: Stdio,
) -> Result<Child> {
    // WPR needs CWD-relative access to its deterministic.js — rely on
    // --https-cert-file/--https-key-file being absolute, and run WPR
    // from the cert directory so deterministic.js resolves.
    let cwd = cert_path.parent().unwrap_or(Path::new("."));
    Command::new(wpr_bin)
        .arg(mode)
        .arg("--https-port")
        .arg(wpr_port.to_string())
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--https-cert-file")
        .arg(cert_path)
        .arg("--https-key-file")
        .arg(key_path)
        .arg(archive)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .with_context(|| format!("spawning wpr ({mode})"))
}

fn spawn_tunnel(tunnel_port: u16, wpr_port: u16, stderr: Stdio) -> Result<Child> {
    let tunnel_bin = sibling_binary("wpr_tunnel")?;
    Command::new(&tunnel_bin)
        .arg("--listen")
        .arg(format!("127.0.0.1:{tunnel_port}"))
        .arg("--upstream")
        .arg(format!("127.0.0.1:{wpr_port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .context("spawning wpr_tunnel")
}

/// Open (create/truncate) a log file under `out_dir` for a fixture child's
/// stderr. Returns a `Stdio` ready to hand to `Command::stderr`.
///
/// Why this exists: WPR logs ~150 bytes per replayed request. With
/// `Stdio::piped()` and no reader, the ~64 KiB default pipe buffer fills
/// after a handful of iterations and WPR's next log write blocks inside
/// its request-serving goroutine — freezing the HTTPS server for the
/// rest of the bench. Writing to a regular file on disk is never
/// flow-controlled the same way, so this deadlock can't recur.
fn open_log(out_dir: &Path, name: &str) -> Result<Stdio> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating {}", out_dir.display()))?;
    let path = out_dir.join(name);
    let file = std::fs::File::create(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    Ok(Stdio::from(file))
}

fn run_servoshell_once(
    servoshell_bin: &Path,
    workload: &Workload,
    handle: &FixtureHandle,
) -> Result<()> {
    // Mimic runner::run_once's argv, but without writing pftrace/png to
    // any particular spot — the record pass discards output. Use a
    // std::env::temp_dir() subdir (no cleanup needed; record is one-off).
    let tmp = std::env::temp_dir().join(format!(
        "servoperf-wpr-record-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&tmp).with_context(|| format!("mkdir {}", tmp.display()))?;
    let mut cmd = Command::new(servoshell_bin);
    cmd.current_dir(&tmp);
    cmd.arg("--headless").arg("--exit");
    cmd.arg("--tracing-filter").arg(&workload.tracing_filter);
    cmd.arg("-o").arg(tmp.join("out.png"));
    if let Some((w, h)) = workload.viewport {
        cmd.arg("--window-size").arg(format!("{w}x{h}"));
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
    if let Some(proxy) = handle.proxy_uri() {
        cmd.env("https_proxy", proxy);
        cmd.env("http_proxy", proxy);
    }
    let status = cmd.status().context("servoshell record pass")?;
    anyhow::ensure!(
        status.success(),
        "servoshell exited non-zero during record pass (status={status})"
    );
    Ok(())
}

fn flush_wpr(handle: &mut FixtureHandle) {
    // SIGINT the wpr child so it flushes the archive file. Our children
    // are [wpr, tunnel] in spawn order; the first is wpr.
    if let Some(wpr) = handle.children.first_mut() {
        #[cfg(unix)]
        unsafe {
            libc_kill(wpr.id() as i32, SIGINT);
        }
        #[cfg(not(unix))]
        {
            let _ = wpr.kill();
        }
        let _ = wpr.wait();
    }
}

#[cfg(unix)]
const SIGINT: i32 = 2;

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

// -- Shared helpers -----------------------------------------------------

fn preflight_port(port: u16) -> Result<()> {
    let addr = format!("127.0.0.1:{port}");
    if TcpStream::connect(&addr).is_ok() {
        anyhow::bail!(
            "port {port} is already in use by another process; stop it before running servoperf"
        );
    }
    Ok(())
}

fn wait_for_accept(
    handle: &mut FixtureHandle,
    port: u16,
    what: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let addr = format!("127.0.0.1:{port}");
    loop {
        if TcpStream::connect(&addr).is_ok() {
            return Ok(());
        }
        for child in &mut handle.children {
            if let Some(status) = child.try_wait().context("polling fixture child")? {
                anyhow::bail!("{what} exited before accepting connections (status={status})");
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("{what} on {addr} never accepted a connection within 5 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sibling_binary(name: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("resolving current exe")?;
    let exe_dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("servoperf binary has no parent directory"))?;
    let candidates = [
        exe_dir.join(name),
        exe_dir.join("..").join(name),
    ];
    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{name} binary not found; searched {:?}. \
                 Build with `cargo build -p servoperf --bins`.",
                candidates,
            )
        })
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn which_on_path(name: &Path) -> Option<PathBuf> {
    if name.components().count() > 1 {
        return None; // not a bare name
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::Fixture;

    fn workload_with(fx: Fixture) -> Workload {
        Workload {
            name: "test".into(),
            url: "https://127.0.0.1/".into(),
            tracing_filter: "info".into(),
            iterations: 1,
            user_agent: None,
            viewport: None,
            device_pixel_ratio: None,
            servoshell_args: vec![],
            fixture: Some(fx),
        }
    }

    #[test]
    fn http1_fixture_spawns_and_drops_cleanly() {
        let workloads_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("workloads");
        let out_dir = tempfile::tempdir().unwrap();
        let fx = Fixture::Http1 {
            port: pick_free_port(),
            doc_root: "www".into(),
        };
        let w = workload_with(fx);
        // servoshell_bin is only used by WprReplay; a non-existent path is OK here.
        let handle = spawn(&workloads_dir, &w, Path::new("/does/not/exist"), out_dir.path())
            .expect("spawn http/1.1 fixture");
        assert!(TcpStream::connect(format!("127.0.0.1:{}", handle.port())).is_ok());
        drop(handle);
        std::thread::sleep(Duration::from_millis(100));
    }

    #[test]
    fn refuses_port_already_in_use() {
        let workloads_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("workloads");
        let out_dir = tempfile::tempdir().unwrap();
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = squatter.local_addr().unwrap().port();
        let w = workload_with(Fixture::Http1 {
            port,
            doc_root: "www".into(),
        });
        let err = match spawn(&workloads_dir, &w, Path::new("/does/not/exist"), out_dir.path()) {
            Ok(_) => panic!("spawn should have refused an occupied port"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("already in use"),
            "expected 'already in use' error, got: {err}"
        );
        drop(squatter);
    }

    #[test]
    fn http2_fixture_spawns() {
        let workloads_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("workloads");
        let out_dir = tempfile::tempdir().unwrap();
        let fx = Fixture::Http2 {
            port: pick_free_port(),
            doc_root: "www".into(),
        };
        let w = workload_with(fx);
        let handle = spawn(&workloads_dir, &w, Path::new("/does/not/exist"), out_dir.path())
            .expect("spawn http/2 fixture");
        assert!(TcpStream::connect(format!("127.0.0.1:{}", handle.port())).is_ok());
    }

    fn pick_free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    }
}
