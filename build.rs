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
    prost_build::Config::new()
        .out_dir(&out_dir)
        .compile_protos(&["proto/perfetto_trace.proto"], &["proto/"])
        .expect("compile perfetto_trace.proto");
}
