# servoperf

Servo startup-performance measurement tool. See
[`docs/superpowers/specs/2026-04-22-startup-perf-workflow-design.md`](../../docs/superpowers/specs/2026-04-22-startup-perf-workflow-design.md).

Two targets are supported: a local `servoshell` binary (perfetto traces),
and a HarmonyOS / OpenHarmony device reached over `hdc` (hitrace text
captures). Both flow through the same critical-path analyser.

## Build

```bash
cd tools/servoperf
cargo build --release
```

## Prerequisites

- `openssl` (TLS cert gen, used by the built-in fixture server).
- A prebuilt `servoshell` with the `tracing-perfetto` feature enabled
  for local runs, **or** a signed `.hap` built with `tracing-hitrace`
  for OHOS runs. **Build from servo's upstream `main`** — patched
  feature worktrees will bias the results.
- For OHOS: `hdc` on `PATH`, a connected device (`hdc list targets`
  non-empty).

## Examples — local

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

## Examples — HarmonyOS / OpenHarmony

```bash
# Source the workspace .envrc from the workspace root so
# SERVO_OHOS_SIGNING_CONFIG resolves correctly. The build is auto-signed
# during hvigor's SignHap step.
source .envrc

cd servo
# tracing-hitrace is NOT in the default OHOS feature set — pass it
# explicitly. Without it servo's spans never reach hitrace.
./mach build --ohos --flavor=harmonyos --profile=release \
             --features tracing,tracing-hitrace
./mach install --ohos --flavor=harmonyos --profile=release   # match the build's profile
cd ..

# Bench against the installed bundle.
./tools/servoperf/target/release/servoperf bench cdn-huaweimossel-live --ohos --iterations=5

# A/B with two haps installed under distinct bundles.
servoperf ab cdn-huaweimossel-live --ohos \
  --base-bin=base.signed.hap   --base-ohos-bundle=org.servo.servo.base \
  --patch-bin=patch.signed.hap --patch-ohos-bundle=org.servo.servo.patch

# Regression check on-device.
servoperf regression cdn-huaweimossel-live --ohos \
  --baseline=baselines/cdn-huaweimossel-live.ohos.json --threshold=5
```

The OHOS path uses a separate critical-path registry
([`workloads/_critical_path_ohos.toml`](workloads/_critical_path_ohos.toml)):
servo on OHOS doesn't emit `FirstPaint`/`FirstContentfulPaint` through
hitrace (the time profiler bypasses it), so we substitute
`PageLoadEndedPrompt` (servoshell's `LoadStatus::Complete` callback) as
the primary milestone. Reports still publish the metric under the key
`FirstContentfulPaint` so regression baselines and report schemas stay
consistent across targets — read it as "first-paint-like proxy."

OHOS-specific options (full list: `servoperf bench --help`):

| Flag | Default | Purpose |
| --- | --- | --- |
| `--ohos` | — | Switches to the device path. |
| `--hdc-server <host:port>` | (local) | `-s <addr>` for `hdc` (containerized agent → remote server). |
| `--ohos-bundle <name>` | `org.servo.servo` | Bundle to launch. |
| `--ohos-ability <name>` | `EntryAbility` | UIAbility name. |
| `--ohos-trace-path <path>` | `/data/local/tmp/servoperf_hitrace.txt` | On-device trace destination. |
| `--ohos-trace-tags <csv>` | `app,graphic,ohos,freq,idle,memory` | hitrace tags. |
| `--ohos-trace-buffer-kib <n>` | 524288 | hitrace ring buffer (KiB). |
| `--ohos-capture-seconds <n>` | 10 | Seconds between `aa start` and `--trace_finish`. |

Localhost fixtures (`Http1`, `Http2`, `WprReplay`) are bridged to the
device with `hdc rport tcp:<port> tcp:<port>` automatically: the host
spawns the fixture, servoperf opens a reverse forward so the device's
`127.0.0.1:<port>` connects through to the host, and the forward is
removed on exit. Self-signed certs are accepted via the workload's
existing `--ignore-certificate-errors` arg (translated to
`--psn=--ignore-certificate-errors` on aa start).

WPR record passes are *not* supported on OHOS — they need a local
`servoshell`. Run the workload locally once first to populate the
archive, then re-run with `--ohos`.

## Outputs

`out/<workload>-<timestamp>/`:
- `raw.json` — machine-readable per-iter data + summaries.
- `report.md` — human-readable quantile tables + critical path.
- Per-iteration trace:
  - Local: `iter_N.pftrace` — drag into <https://ui.perfetto.dev>.
  - OHOS: `iter_N.hitrace.txt` — ftrace-style text. Open in
    [SmartPerf Host](https://gitcode.com/openharmony/developtools_smartperf_host/releases)
    for a swimlane view, or `grep` for specific span names.

## Workloads

TOML files in [`workloads/`](workloads/). Each names a URL and optional local fixture. To add one: copy an existing TOML, adjust, rerun.

## Tests

```bash
cargo test              # unit + parser fixture tests
cargo test -- --ignored # + e2e smoke with fake servoshell
```
