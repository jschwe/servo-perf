use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
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

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    match parse_args(&argv[1..]) {
        Ok(_args) => {
            eprintln!("fixture_server: parse_args ok but serving not yet implemented");
            std::process::exit(1);
        }
        Err(msg) => {
            eprintln!("{msg}");
            eprintln!("usage: fixture_server --mode=http1|http2 <port> <doc_root>");
            std::process::exit(2);
        }
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

fn resolve_safe_path(doc_root: &Path, req_path: &str) -> Result<PathBuf, u16> {
    let relative = if req_path == "/" {
        "index.html"
    } else {
        req_path.strip_prefix('/').ok_or(400u16)?
    };
    let candidate = doc_root.join(relative);
    // If the target exists, canonicalise and verify it still lives under
    // doc_root — catches symlink escapes (Task 5). If it doesn't exist,
    // pass the candidate through; the file-read step will return 404.
    if let Ok(canon_target) = candidate.canonicalize() {
        let canon_root = doc_root.canonicalize().map_err(|_| 500u16)?;
        if !canon_target.starts_with(&canon_root) {
            return Err(400);
        }
        return Ok(canon_target);
    }
    Ok(candidate)
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
        // Safety check passes when the parent dir is under doc_root, even if
        // the file itself is missing; the file-read step will return 404.
        let tmp = tempdir().unwrap();
        let got = resolve_safe_path(tmp.path(), "/does-not-exist").unwrap();
        assert_eq!(got, tmp.path().join("does-not-exist"));
    }
}
