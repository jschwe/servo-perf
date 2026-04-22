use std::env;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed=proto/perfetto_trace.proto");
    prost_build::Config::new()
        .out_dir(&out_dir)
        .compile_protos(&["proto/perfetto_trace.proto"], &["proto/"])
        .expect("compile perfetto_trace.proto");
}
