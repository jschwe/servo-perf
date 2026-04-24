//! HTTPS proxy CONNECT shim for WPR.
//!
//! Why this exists:
//! WPR (Web Page Replay Go) is designed around Chrome's
//! `--host-resolver-rules` flag: Chrome resolves every hostname directly
//! to WPR's IP and port, so Chrome never emits a `CONNECT` through a
//! proxy — it just dials WPR as if it were the origin. WPR's source
//! contains zero occurrences of `CONNECT` or `MethodConnect`; it has no
//! CONNECT handler on any port.
//!
//! Non-Chrome clients (including Servo) don't have
//! `--host-resolver-rules`. The cleanest way to point them at WPR is
//! `https_proxy=http://127.0.0.1:<tunnel>`, which makes the client emit
//! `CONNECT origin:443 HTTP/1.1` toward the proxy. That's what this
//! shim does: accepts the CONNECT, replies `200 Connection Established`,
//! then copies bytes bidirectionally with a fixed WPR socket address,
//! ignoring whatever host:port the client asked for. The effect is that
//! the subsequent client TLS handshake (with SNI = origin) lands on WPR,
//! which mints a cert for that SNI and either serves from the archive
//! (replay) or forwards to the real origin and records (record).
//!
//! Because WPR is addressed only by socket, not by URL, it can run on
//! any unprivileged port — the client's URL stays `https://host/`
//! (implicit :443), so WPR's request reconstruction sees the original
//! port and forwards/replays correctly.
//!
//! Usage:  wpr_tunnel --listen 127.0.0.1:4480 --upstream 127.0.0.1:4443

use anyhow::{Context, Result};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct Args {
    listen: SocketAddr,
    upstream: SocketAddr,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut listen: Option<SocketAddr> = None;
    let mut upstream: Option<SocketAddr> = None;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--listen" => {
                let v = argv.get(i + 1).ok_or("missing value for --listen")?;
                listen = Some(v.parse().map_err(|e| format!("--listen {v}: {e}"))?);
                i += 2;
            }
            "--upstream" => {
                let v = argv.get(i + 1).ok_or("missing value for --upstream")?;
                upstream = Some(v.parse().map_err(|e| format!("--upstream {v}: {e}"))?);
                i += 2;
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Args {
        listen: listen.ok_or_else(|| "--listen required (e.g. 127.0.0.1:4480)".to_string())?,
        upstream: upstream
            .ok_or_else(|| "--upstream required (e.g. 127.0.0.1:4443)".to_string())?,
    })
}

async fn handle(mut client: TcpStream, upstream: SocketAddr) -> Result<()> {
    // Read request head up to CRLFCRLF.
    let mut head = Vec::with_capacity(1024);
    let mut tmp = [0u8; 2048];
    loop {
        let n = client.read(&mut tmp).await.context("reading request head")?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&tmp[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        anyhow::ensure!(head.len() < 65536, "request head exceeded 64 KiB");
    }
    let first_line = std::str::from_utf8(&head)
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or_default();
    if !first_line.starts_with("CONNECT ") {
        let _ = client.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
        return Ok(());
    }

    let mut up = match TcpStream::connect(upstream).await {
        Ok(s) => s,
        Err(e) => {
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
            return Err(e).context("dialing WPR upstream");
        }
    };
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .context("writing 200 response")?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut up).await;
    Ok(())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            eprintln!("usage: wpr_tunnel --listen <addr> --upstream <addr>");
            std::process::exit(2);
        }
    };
    let listener = TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    println!(
        "wpr_tunnel: CONNECT on {} → WPR at {}",
        args.listen, args.upstream
    );
    let upstream = args.upstream;
    loop {
        let (client, _) = listener.accept().await.context("accept")?;
        tokio::spawn(async move {
            let _ = handle(client, upstream).await;
        });
    }
}
