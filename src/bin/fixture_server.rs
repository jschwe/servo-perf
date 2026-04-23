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
        // After strip_prefix('/'), this starts with "/etc/passwd", which is absolute.
        let tmp = tempdir().unwrap();
        let err = resolve_safe_path(tmp.path(), "//etc/passwd").unwrap_err();
        assert_eq!(err, 400);
    }

    #[test]
    fn rejects_single_dot_segment() {
        // Disallow "./x" as a belt-and-braces check — no workload needs it.
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
        // doc_root/escape -> /etc  (target must exist for canonicalize to succeed)
        symlink("/etc", tmp.path().join("escape")).unwrap();
        let err = resolve_safe_path(tmp.path(), "/escape/hostname").unwrap_err();
        assert_eq!(err, 400);
    }
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

#[cfg(test)]
mod mime_tests {
    use super::*;

    #[test]
    fn html() {
        assert_eq!(content_type_for(Path::new("x.html")), "text/html");
    }

    #[test]
    fn css() {
        assert_eq!(content_type_for(Path::new("x.css")), "text/css");
    }

    #[test]
    fn js() {
        assert_eq!(content_type_for(Path::new("x.js")), "application/javascript");
    }

    #[test]
    fn png() {
        assert_eq!(content_type_for(Path::new("img.png")), "image/png");
    }

    #[test]
    fn jpg_and_jpeg() {
        assert_eq!(content_type_for(Path::new("a.jpg")), "image/jpeg");
        assert_eq!(content_type_for(Path::new("a.jpeg")), "image/jpeg");
    }

    #[test]
    fn ico() {
        assert_eq!(content_type_for(Path::new("a.ico")), "image/x-icon");
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(content_type_for(Path::new("a.HTML")), "text/html");
        assert_eq!(content_type_for(Path::new("a.PNG")), "image/png");
    }

    #[test]
    fn unknown_falls_back_to_octet_stream() {
        assert_eq!(content_type_for(Path::new("a.bin")), "application/octet-stream");
        assert_eq!(content_type_for(Path::new("noext")), "application/octet-stream");
    }
}
