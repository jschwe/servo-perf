# servoperf

Servo startup-performance measurement tool. See
[`docs/superpowers/specs/2026-04-22-startup-perf-workflow-design.md`](../../docs/superpowers/specs/2026-04-22-startup-perf-workflow-design.md).

## Build

```bash
cd tools/servoperf
cargo build --release
```

## Prerequisites

- `openssl` (TLS cert gen, used by the built-in fixture server).
- A prebuilt `servoshell` with the `tracing-perfetto` feature enabled. **Build from servo's upstream `main`** — patched feature worktrees will bias the results.

## Examples

```bash
# Build servo/main once
( cd /path/to/servo && ./mach build --profile=profiling --features tracing-perfetto )
SERVO=/path/to/servo/target/profiling/servoshell

# One-shot bench
./target/release/servoperf bench h2-multi --bin=$SERVO

# Paired A/B
./target/release/servoperf ab h2-multi \
  --base-bin=/path/to/servo-main/servoshell \
  --patch-bin=/path/to/servo-patched/servoshell

# Regression check
./target/release/servoperf regression h2-multi \
  --bin=$SERVO \
  --baseline=baselines/h2-multi.json \
  --threshold=5
# Exits 1 iff FCP p50 is >5% slower than the baseline.
```

## Outputs

`out/<workload>-<timestamp>/`:
- `raw.json` — machine-readable per-iter data + summaries.
- `report.md` — human-readable quantile tables + critical path.
- `iter_N.pftrace` — raw traces (drag into <https://ui.perfetto.dev>).

## Workloads

TOML files in [`workloads/`](workloads/). Each names a URL and optional local fixture. To add one: copy an existing TOML, adjust, rerun.

## Tests

```bash
cargo test              # unit + parser fixture tests
cargo test -- --ignored # + e2e smoke with fake servoshell
```
