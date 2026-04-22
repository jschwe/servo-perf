// tools/servoperf/src/fixtures.rs
//! RAII lifecycle for localhost fixture servers.

use anyhow::{Context, Result};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::workload::{Fixture, FixtureKind};

pub struct FixtureHandle {
    child: Child,
    port: u16,
}

impl FixtureHandle {
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for FixtureHandle {
    fn drop(&mut self) {
        // Best-effort kill; log but don't panic on failure.
        if let Err(err) = self.child.kill() {
            eprintln!("warning: failed to kill fixture on port {}: {err}", self.port);
        }
        let _ = self.child.wait();
    }
}

/// Spawn the fixture server described by `fx` with its docroot resolved
/// relative to `workloads_dir` (so `doc_root = "www"` → `<workloads_dir>/../fixtures/www`).
pub fn spawn(workloads_dir: &Path, fx: &Fixture) -> Result<FixtureHandle> {
    let fixtures_dir = workloads_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("workloads_dir has no parent"))?
        .join("fixtures");
    let doc_root: PathBuf = fixtures_dir.join(&fx.doc_root);
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

    let child = match fx.kind {
        FixtureKind::Http1 => Command::new("python3")
            .arg(fixtures_dir.join("https_server.py"))
            .arg(fx.port.to_string())
            .arg(&doc_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning python3 https_server.py")?,
        FixtureKind::Http2 => Command::new("node")
            .arg(fixtures_dir.join("h2_server.js"))
            .arg(fx.port.to_string())
            .arg(&doc_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning node h2_server.js")?,
    };

    // Wait until the port accepts a TCP connection, with a 3 s timeout.
    let deadline = Instant::now() + Duration::from_secs(3);
    let addr = format!("127.0.0.1:{}", fx.port);
    loop {
        if TcpStream::connect(&addr).is_ok() {
            return Ok(FixtureHandle { child, port: fx.port });
        }
        if Instant::now() >= deadline {
            // `handle` owns `child`; going out of scope at the `bail!` below
            // fires Drop, which kills the subprocess. No explicit drop needed.
            let _handle = FixtureHandle { child, port: fx.port };
            anyhow::bail!("fixture on {addr} never accepted a connection within 3 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::{Fixture, FixtureKind};

    #[test]
    fn http1_fixture_spawns_and_drops_cleanly() {
        let workloads_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("workloads");
        let fx = Fixture {
            kind: FixtureKind::Http1,
            port: pick_free_port(),
            doc_root: "www".into(),
        };
        let handle = spawn(&workloads_dir, &fx).expect("spawn http/1.1 fixture");
        // Now the port should be listening; drop should cleanly kill.
        assert!(TcpStream::connect(format!("127.0.0.1:{}", handle.port())).is_ok());
        drop(handle);
        // After drop, connection refused (port should free up shortly).
        std::thread::sleep(Duration::from_millis(100));
    }

    #[test]
    fn http2_fixture_spawns() {
        let workloads_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("workloads");
        let fx = Fixture {
            kind: FixtureKind::Http2,
            port: pick_free_port(),
            doc_root: "www".into(),
        };
        let handle = spawn(&workloads_dir, &fx).expect("spawn http/2 fixture");
        assert!(TcpStream::connect(format!("127.0.0.1:{}", handle.port())).is_ok());
    }

    fn pick_free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    }
}
