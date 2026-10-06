fn main() {
    // The perfetto proto schema is only compiled when the `pftrace` feature is
    // enabled. Cargo compiles this build script with `--cfg feature="pftrace"`
    // for the active features, so gating here means a build without the feature
    // never touches `prost-build` and therefore never needs `protoc`.
    #[cfg(feature = "pftrace")]
    compile_perfetto_proto();
}

#[cfg(feature = "pftrace")]
fn compile_perfetto_proto() {
    use std::env;
    use std::path::PathBuf;

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed=proto/perfetto_trace.proto");
    if let Err(e) = prost_build::Config::new()
        .out_dir(&out_dir)
        .compile_protos(&["proto/perfetto_trace.proto"], &["proto/"])
    {
        // `protoc` is needed only by the default `pftrace` feature, and only
        // for desktop-target trace parsing — the OHOS/hitrace path never uses
        // it. Say so here, because prost-build's own message does not mention
        // that the feature is optional.
        panic!(
            "failed to compile proto/perfetto_trace.proto: {e}\n\
             \n\
             This step needs `protoc`, which is required only by the default \
             `pftrace` feature (Perfetto .pftrace parsing: the `dump` command \
             and desktop-target bench/ab). Measuring an OHOS device does not \
             use it.\n\
             \n\
             Either build without it:\n\
             \x20   cargo build --release --no-default-features\n\
             \n\
             or install protoc and re-run:\n\
             \x20   winget install Google.Protobuf     (Windows)\n\
             \x20   choco install protoc               (Windows, Chocolatey)\n\
             \x20   brew install protobuf              (macOS)\n\
             \x20   apt install protobuf-compiler      (Debian/Ubuntu)\n\
             \n\
             or point at an existing binary with the PROTOC environment \
             variable, e.g. PROTOC=C:\\tools\\protoc\\bin\\protoc.exe"
        );
    }
}
