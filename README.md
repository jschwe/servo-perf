# servoperf

Servo startup-performance workflow. See
`docs/superpowers/specs/2026-04-22-startup-perf-workflow-design.md` for design.

## Quick start

```bash
cd tools/servoperf
cargo build --release
./target/release/servoperf --help
```

## Commands

- `servoperf bench <workload> --bin=<path>` — run a single binary N times, produce report.
- `servoperf ab <workload> --base-bin=<p1> --patch-bin=<p2>` — paired A/B with interleaved iterations.
- `servoperf regression <workload> --bin=<path> --baseline=<raw.json>` — exit code 1 if regressed.

When building servoshell binaries to measure, use servo's upstream `main` branch.
