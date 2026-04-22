// A tiny drop-in for servoshell used only by servoperf's e2e test.
//
// It recognises just enough of the real CLI to satisfy runner.rs:
//   --headless   --exit   --tracing-filter FILTER   -o PATH
// plus ignores everything else (device-pixel-ratio, window-size, UA, URL).
// Mode is controlled by the `SERVOPERF_FAKE_MODE` env var:
//   - unset / "ok"   : write a canned minimal.pftrace to cwd, exit 0
//   - "crash_on_3"   : exit 11 on iteration index 3 (detected via cwd name
//                      containing `cwd_3`), otherwise ok
//
// The canned pftrace is built inline so this binary is standalone; it must
// contain at least a FirstContentfulPaint instant event for the parser to
// produce a metric.

use std::env;
use std::fs;

fn main() {
    // Parse `-o PATH` so we can write a zero-byte PNG.
    let mut args = env::args().skip(1);
    let mut out_png: Option<String> = None;
    while let Some(a) = args.next() {
        if a == "-o" || a == "--output" {
            out_png = args.next();
        }
    }
    if let Some(p) = out_png {
        let _ = fs::write(p, b"\x89PNG\r\n\x1a\n");
    }

    let cwd = env::current_dir().unwrap();
    let cwd_name = cwd.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if env::var("SERVOPERF_FAKE_MODE").as_deref() == Ok("crash_on_3")
        && cwd_name.contains("cwd_3")
    {
        std::process::exit(11);
    }

    let pftrace: &[u8] = FAKE_PFTRACE_BYTES;
    fs::write(cwd.join("servo.pftrace"), pftrace).unwrap();
}

// This is the identical minimal pftrace produced by servoperf's
// `gen_minimal_pftrace` helper (see src/bin/gen_minimal_pftrace.rs).
//
// Path is relative to this file: tests/fake_servoshell/src/main.rs
// → up to tests/fake_servoshell/ → up to tests/ → fixtures/minimal.pftrace
const FAKE_PFTRACE_BYTES: &[u8] = include_bytes!("../../fixtures/minimal.pftrace");
