// tools/servoperf/tests/e2e.rs
//! Opt-in end-to-end smoke test.
//!
//! Run with:  cargo test -- --ignored

use std::process::Command;

#[test]
#[ignore]
fn bench_localhost_simple_with_fake_servoshell() {
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    // Build the fake servoshell so its binary is on disk.
    // Unset CARGO_TARGET_DIR so the binary lands in the crate's own
    // target/ dir (not the workspace-level override), making the path
    // predictable for the assert below.
    let fake_crate_dir = crate_dir.join("tests").join("fake_servoshell");
    let status = Command::new("cargo")
        .current_dir(&fake_crate_dir)
        .arg("build").arg("--release")
        .env_remove("CARGO_TARGET_DIR")
        .status().expect("cargo build fake_servoshell");
    assert!(status.success());

    let fake = fake_crate_dir.join("target/release/fake_servoshell");
    assert!(fake.is_file(), "fake binary missing at {}", fake.display());

    let tmp = tempfile::tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_servoperf"))
        .arg("bench").arg("localhost-simple")
        .arg("--bin").arg(&fake)
        .arg("--iterations").arg("3")
        .arg("--out").arg(tmp.path())
        .current_dir(crate_dir)
        .status()
        .expect("run servoperf bench");
    assert!(status.success(), "servoperf bench exited non-zero");

    let raw_json = tmp.path().join("raw.json");
    let report_md = tmp.path().join("report.md");
    assert!(raw_json.is_file());
    assert!(report_md.is_file());

    let raw: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(&raw_json).unwrap()).unwrap();
    let iters = raw["configs"]["main"]["iterations"].as_array().unwrap();
    assert_eq!(iters.len(), 3);
    for i in iters {
        // IterationStatus is serde-flattened: successful iterations have an
        // "ok" object key, failed ones have "failed".
        assert!(i["ok"].is_object(), "unexpected iteration failure: {i}");
    }

    let md = std::fs::read_to_string(&report_md).unwrap();
    assert!(md.contains("servoperf — `localhost-simple`"));
    assert!(md.contains("FirstContentfulPaint"));
}

#[test]
#[ignore]
fn bench_tolerates_per_iteration_crash() {
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fake_crate_dir = crate_dir.join("tests").join("fake_servoshell");
    let status = Command::new("cargo")
        .current_dir(&fake_crate_dir)
        .arg("build").arg("--release")
        .env_remove("CARGO_TARGET_DIR")
        .status().unwrap();
    assert!(status.success());

    let fake = fake_crate_dir.join("target/release/fake_servoshell");
    let tmp = tempfile::tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_servoperf"))
        .env("SERVOPERF_FAKE_MODE", "crash_on_3")
        .arg("bench").arg("localhost-simple")
        .arg("--bin").arg(&fake)
        .arg("--iterations").arg("5")
        .arg("--out").arg(tmp.path())
        .current_dir(crate_dir)
        .status().unwrap();
    assert!(status.success(), "run should succeed with a single crashed iteration");

    let raw: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(tmp.path().join("raw.json")).unwrap()).unwrap();
    let iters = raw["configs"]["main"]["iterations"].as_array().unwrap();
    // IterationStatus is serde-flattened: failed iterations have a "failed"
    // object key.
    let failed = iters.iter().filter(|i| i["failed"].is_object()).count();
    assert_eq!(failed, 1, "exactly one iteration should be marked failed");
}
