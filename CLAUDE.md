# servoperf

`servoperf` is a standalone Cargo crate (it is **not** part of the main servo
cargo workspace) that measures Servo startup performance. `cd` into
`tools/servoperf` before running any command here.

## Formatting

ALWAYS run `cargo fmt` before committing.

## Building / `protoc`

Perfetto `.pftrace` parsing lives behind the default-on `pftrace` feature,
which requires `protoc` at build time (it compiles `proto/perfetto_trace.proto`
through `prost-build`). To build on a host without `protoc`:

    cargo build --no-default-features

That drops the `dump` subcommand, local-target `bench`/`ab` pftrace parsing,
and the `gen_minimal_pftrace` / `pftrace_stats` helper binaries. The
OHOS/hitrace path, the fixture servers, and critical-path analysis still build
and run. When changing the `pftrace` gating, verify both configurations:

    cargo build
    cargo build --no-default-features

## Testing

Run tests with `cargo test`, or `cargo nextest run` if you need more control
over the runner. When touching the `pftrace` gating, test both feature
configurations:

    cargo test
    cargo test --no-default-features
