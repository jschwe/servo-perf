use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

#[derive(Debug, PartialEq, Clone, Copy)]
enum Mode {
    Http1,
    Http2,
}

#[derive(Debug, PartialEq)]
struct Args {
    mode: Mode,
    port: u16,
    doc_root: PathBuf,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    if argv.len() != 3 {
        return Err(format!(
            "expected 3 arguments, got {} (usage: --mode=http1|http2 <port> <doc_root>)",
            argv.len()
        ));
    }
    let mode_arg = &argv[0];
    let mode_str = mode_arg
        .strip_prefix("--mode=")
        .ok_or_else(|| format!("first argument must be --mode=http1 or --mode=http2, got {mode_arg:?}"))?;
    let mode = match mode_str {
        "http1" => Mode::Http1,
        "http2" => Mode::Http2,
        other => return Err(format!("unknown --mode value {other:?} (expected http1 or http2)")),
    };
    let port: u16 = argv[1]
        .parse()
        .map_err(|e| format!("invalid port {:?}: {e}", argv[1]))?;
    let doc_root = PathBuf::from(&argv[2]);
    Ok(Args { mode, port, doc_root })
}

fn resolve_safe_path(doc_root: &Path, req_path: &str) -> Result<PathBuf, u16> {
    let relative = if req_path == "/" {
        "index.html"
    } else {
        req_path.strip_prefix('/').ok_or(400u16)?
    };
    if relative.is_empty() || relative.starts_with('/') {
        return Err(400);
    }
    for seg in relative.split('/') {
        if seg.is_empty() || seg == ".." || seg == "." {
            return Err(400);
        }
    }
    let candidate = doc_root.join(relative);
    if let Ok(canon_target) = candidate.canonicalize() {
        let canon_root = doc_root.canonicalize().map_err(|_| 500u16)?;
        if !canon_target.starts_with(&canon_root) {
            return Err(400);
        }
        return Ok(canon_target);
    }
    Ok(candidate)
}

fn content_type_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn build_tls_config(cert_path: &Path, key_path: &Path, mode: Mode) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::fs::File;
    use std::io::BufReader;

    let cert_file = File::open(cert_path)
        .with_context(|| format!("opening cert file {}", cert_path.display()))?;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut BufReader::new(cert_file))
        .collect::<Result<_, _>>()
        .context("parsing cert PEM")?;
    anyhow::ensure!(!certs.is_empty(), "no certificates found in {}", cert_path.display());

    let key_file = File::open(key_path)
        .with_context(|| format!("opening key file {}", key_path.display()))?;
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut BufReader::new(key_file))
        .context("parsing key PEM")?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", key_path.display()))?;

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building rustls ServerConfig")?;
    config.alpn_protocols = match mode {
        Mode::Http1 => vec![b"http/1.1".to_vec()],
        Mode::Http2 => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
    };
    Ok(Arc::new(config))
}

async fn serve(
    req: Request<Incoming>,
    doc_root: Arc<PathBuf>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let method = req.method().clone();
    let uri_path = req.uri().path().to_string();

    let is_head = method == hyper::Method::HEAD;
    if method != hyper::Method::GET && !is_head {
        log_request(method.as_str(), &uri_path, 0);
        return Ok(status_response(405, Bytes::new()));
    }

    let resolved = match resolve_safe_path(&doc_root, &uri_path) {
        Ok(p) => p,
        Err(code) => {
            log_request(method.as_str(), &uri_path, 0);
            return Ok(status_response(code, Bytes::new()));
        }
    };

    match tokio::fs::read(&resolved).await {
        Ok(bytes) => {
            let ct = content_type_for(&resolved);
            let body_len = bytes.len();
            log_request(method.as_str(), &uri_path, body_len);
            let body = if is_head { Bytes::new() } else { Bytes::from(bytes) };
            let resp = Response::builder()
                .status(200)
                .header("content-type", ct)
                .header("content-length", body_len.to_string())
                .body(Full::new(body))
                .unwrap();
            Ok(resp)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log_request(method.as_str(), &uri_path, 0);
            Ok(status_response(404, Bytes::from_static(b"nf")))
        }
        Err(e) => {
            eprintln!("serve: IO error reading {}: {e}", resolved.display());
            Ok(status_response(500, Bytes::new()))
        }
    }
}

fn log_request(method: &str, path: &str, body_len: usize) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    println!("[srv {ts:.3}] {method} {path} ({body_len}B)");
}

fn status_response(code: u16, body: Bytes) -> Response<Full<Bytes>> {
    let body_len = body.len();
    Response::builder()
        .status(code)
        .header("content-length", body_len.to_string())
        .body(Full::new(body))
        .unwrap()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install default crypto provider"))?;

    let argv: Vec<String> = std::env::args().collect();
    let args = match parse_args(&argv[1..]) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            eprintln!("usage: fixture_server --mode=http1|http2 <port> <doc_root>");
            std::process::exit(2);
        }
    };

    let cert_path = args.doc_root.join("..").join("cert.pem");
    let key_path = args.doc_root.join("..").join("key.pem");
    let tls_config = build_tls_config(&cert_path, &key_path, args.mode)?;
    let acceptor = TlsAcceptor::from(tls_config);

    let listener = TcpListener::bind(("127.0.0.1", args.port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{}", args.port))?;
    let doc_root = Arc::new(args.doc_root);
    let mode_str = if args.mode == Mode::Http1 { "http1" } else { "http2" };
    println!(
        "listening on https://127.0.0.1:{}/ doc_root={} mode={mode_str}",
        args.port,
        doc_root.display()
    );

    loop {
        let (tcp, _peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let doc_root = Arc::clone(&doc_root);
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(tcp).await else {
                return;
            };
            let io = TokioIo::new(tls);
            let _ = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, service_fn(move |req| {
                    let doc_root = Arc::clone(&doc_root);
                    async move { serve(req, doc_root).await }
                }))
                .await;
        });
    }
}

#[cfg(test)]
mod arg_tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_http1_mode() {
        let got = parse_args(&argv(&["--mode=http1", "4443", "/tmp/root"])).unwrap();
        assert_eq!(got, Args {
            mode: Mode::Http1,
            port: 4443,
            doc_root: PathBuf::from("/tmp/root"),
        });
    }

    #[test]
    fn parses_http2_mode() {
        let got = parse_args(&argv(&["--mode=http2", "4444", "/tmp/root"])).unwrap();
        assert_eq!(got.mode, Mode::Http2);
        assert_eq!(got.port, 4444);
    }

    #[test]
    fn rejects_missing_mode() {
        let err = parse_args(&argv(&["4443", "/tmp/root"])).unwrap_err();
        assert!(err.contains("--mode"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_mode() {
        let err = parse_args(&argv(&["--mode=http3", "4443", "/tmp/root"])).unwrap_err();
        assert!(err.contains("http3"), "got: {err}");
    }

    #[test]
    fn rejects_bad_port() {
        let err = parse_args(&argv(&["--mode=http1", "not-a-port", "/tmp/root"])).unwrap_err();
        assert!(err.contains("port"), "got: {err}");
    }

    #[test]
    fn rejects_wrong_argc() {
        let err = parse_args(&argv(&["--mode=http1", "4443"])).unwrap_err();
        assert!(err.to_lowercase().contains("usage") || err.to_lowercase().contains("expected"),
                "got: {err}");
    }
}

#[cfg(test)]
mod path_happy_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn root_resolves_to_index_html() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("index.html"), b"<html></html>").unwrap();
        let got = resolve_safe_path(tmp.path(), "/").unwrap();
        assert_eq!(got, tmp.path().canonicalize().unwrap().join("index.html"));
    }

    #[test]
    fn simple_file_resolves() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("simple.html"), b"hi").unwrap();
        let got = resolve_safe_path(tmp.path(), "/simple.html").unwrap();
        assert_eq!(got, tmp.path().canonicalize().unwrap().join("simple.html"));
    }

    #[test]
    fn subdir_file_resolves() {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/a.css"), b"body{}").unwrap();
        let got = resolve_safe_path(tmp.path(), "/sub/a.css").unwrap();
        assert_eq!(
            got,
            tmp.path().canonicalize().unwrap().join("sub").join("a.css"),
        );
    }

    #[test]
    fn nonexistent_file_still_returns_ok_path() {
        let tmp = tempdir().unwrap();
        let got = resolve_safe_path(tmp.path(), "/does-not-exist").unwrap();
        assert_eq!(got, tmp.path().join("does-not-exist"));
    }
}

#[cfg(test)]
mod path_rejection_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn rejects_dot_dot_segment() {
        let tmp = tempdir().unwrap();
        let err = resolve_safe_path(tmp.path(), "/../Cargo.toml").unwrap_err();
        assert_eq!(err, 400);
    }

    #[test]
    fn rejects_dot_dot_nested() {
        let tmp = tempdir().unwrap();
        let err = resolve_safe_path(tmp.path(), "/sub/../../escape").unwrap_err();
        assert_eq!(err, 400);
    }

    #[test]
    fn rejects_double_slash_absolute_hijack() {
        let tmp = tempdir().unwrap();
        let err = resolve_safe_path(tmp.path(), "//etc/passwd").unwrap_err();
        assert_eq!(err, 400);
    }

    #[test]
    fn rejects_single_dot_segment() {
        let tmp = tempdir().unwrap();
        let err = resolve_safe_path(tmp.path(), "/./simple.html").unwrap_err();
        assert_eq!(err, 400);
    }

    #[test]
    fn rejects_missing_leading_slash() {
        let tmp = tempdir().unwrap();
        let err = resolve_safe_path(tmp.path(), "no-slash").unwrap_err();
        assert_eq!(err, 400);
    }

    #[test]
    #[cfg(unix)]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let tmp = tempdir().unwrap();
        symlink("/etc", tmp.path().join("escape")).unwrap();
        // If /etc/hostname doesn't exist on this host, substitute any readable file under /etc.
        let err = resolve_safe_path(tmp.path(), "/escape/hostname").unwrap_err();
        assert_eq!(err, 400);
    }
}

#[cfg(test)]
mod mime_tests {
    use super::*;

    #[test]
    fn html() { assert_eq!(content_type_for(Path::new("x.html")), "text/html"); }
    #[test]
    fn css()  { assert_eq!(content_type_for(Path::new("x.css")),  "text/css"); }
    #[test]
    fn js()   { assert_eq!(content_type_for(Path::new("x.js")),   "application/javascript"); }
    #[test]
    fn png()  { assert_eq!(content_type_for(Path::new("img.png")),"image/png"); }
    #[test]
    fn jpg_and_jpeg() {
        assert_eq!(content_type_for(Path::new("a.jpg")),  "image/jpeg");
        assert_eq!(content_type_for(Path::new("a.jpeg")), "image/jpeg");
    }
    #[test]
    fn ico()  { assert_eq!(content_type_for(Path::new("a.ico")),  "image/x-icon"); }
    #[test]
    fn case_insensitive() {
        assert_eq!(content_type_for(Path::new("a.HTML")), "text/html");
        assert_eq!(content_type_for(Path::new("a.PNG")),  "image/png");
    }
    #[test]
    fn unknown_falls_back_to_octet_stream() {
        assert_eq!(content_type_for(Path::new("a.bin")), "application/octet-stream");
        assert_eq!(content_type_for(Path::new("noext")), "application/octet-stream");
    }
}

#[cfg(test)]
mod tls_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Once;

    static INSTALL: Once = Once::new();
    fn install_provider() {
        INSTALL.call_once(|| {
            rustls::crypto::ring::default_provider()
                .install_default()
                .expect("install default crypto provider");
        });
    }

    fn fixture_paths() -> (PathBuf, PathBuf) {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        (base.join("cert.pem"), base.join("key.pem"))
    }

    #[test]
    fn http1_mode_advertises_only_http1_1() {
        install_provider();
        let (cert, key) = fixture_paths();
        let cfg = build_tls_config(&cert, &key, Mode::Http1).unwrap();
        assert_eq!(cfg.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn http2_mode_advertises_h2_then_http1_1() {
        install_provider();
        let (cert, key) = fixture_paths();
        let cfg = build_tls_config(&cert, &key, Mode::Http2).unwrap();
        assert_eq!(cfg.alpn_protocols, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
    }

    #[test]
    fn missing_cert_errors_cleanly() {
        install_provider();
        let err = build_tls_config(
            Path::new("/does/not/exist.pem"),
            Path::new("/does/not/exist.pem"),
            Mode::Http1,
        ).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.to_lowercase().contains("no such file") || msg.to_lowercase().contains("cert"),
                "unexpected error message: {msg}");
    }
}
