//! Integration tests for the fixture_server binary.
//! Spawns the binary as a subprocess and issues real TLS requests.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fixture_server"))
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn www_dir() -> PathBuf {
    fixtures_dir().join("www")
}

fn pick_free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

struct ServerHandle {
    child: Child,
    port: u16,
}
impl Drop for ServerHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ensure_cert() {
    let sh = fixtures_dir().join("gen_cert.sh");
    let status = Command::new("sh").arg(&sh).status().expect("run gen_cert.sh");
    assert!(status.success(), "gen_cert.sh failed");
}

fn spawn(mode: &str, doc_root: &Path) -> ServerHandle {
    ensure_cert();
    let port = pick_free_port();
    let child = Command::new(binary())
        .arg(format!("--mode={mode}"))
        .arg(port.to_string())
        .arg(doc_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fixture_server");
    let addr = format!("127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(&addr).is_ok() {
            return ServerHandle { child, port };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("fixture_server on {addr} never accepted a connection within 3 s");
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
}

#[tokio::test]
async fn http1_scaffolding_responds_200() {
    // Any valid static file returns 200; this proves the accept loop,
    // TLS handshake, and file-serving wire together.
    let srv = spawn("http1", &www_dir());
    let url = format!("https://127.0.0.1:{}/simple.html", srv.port);
    let resp = client().get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn http1_serves_simple_html() {
    let srv = spawn("http1", &www_dir());
    let url = format!("https://127.0.0.1:{}/simple.html", srv.port);
    let resp = client().get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-type").unwrap(), "text/html");
    let expected = std::fs::read(www_dir().join("simple.html")).unwrap();
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), expected.as_slice());
}

#[tokio::test]
async fn http1_serves_png_with_correct_content_type() {
    let srv = spawn("http1", &www_dir());
    let url = format!("https://127.0.0.1:{}/img1.png", srv.port);
    let resp = client().get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-type").unwrap(), "image/png");
}

#[tokio::test]
async fn http2_serves_simple_html_over_h2() {
    let srv = spawn("http2", &www_dir());
    let url = format!("https://127.0.0.1:{}/simple.html", srv.port);
    let resp = client().get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.version(), reqwest::Version::HTTP_2);
    assert_eq!(resp.headers().get("content-type").unwrap(), "text/html");
}
