use std::path::PathBuf;

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
    match parse_args(&argv[1..].to_vec()) {
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
