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

## CPU frequency

On a big.LITTLE phone, CPU-milliseconds cannot tell a thread that does more
work from one that runs slower, and the difference is large: servo's canvas
thread has been measured at 0.52 GHz on a cluster that reaches 2.15 GHz, i.e.
24% of the clock available to it. A frame handed along a chain of threads is
exactly the workload a utilisation-driven governor mis-serves, because each
thread idles while the others work and no run queue ever looks busy.

OHOS runs report this automatically from the hitrace capture:

- `cluster_ghz.<cluster>`, `cluster_max_opp_pct.<cluster>` — per-cluster mean
  clock and time spent at the top operating point.
- `clock_headroom_pct` — how much clock the busiest thread (or, without
  `sched`, the busiest cluster) was not given.
- `thread_ghz.<thread>` — clock while that thread was on a CPU.
- `thread_pct_of_max_ghz.<thread>` — that clock as a share of the top
  frequency of the cores it ran on. **This is the number to read.**

The cluster rows and `clock_headroom_pct` need only `cpu_frequency`, which the
default tag list already captures, so every OHOS run gets them for free. Naming
the *thread* additionally needs `sched_switch`, which is high volume (~200 MB
for a 20 s capture, versus ~10 MB without) and so is opt-in:

```bash
servoperf bench <workload> --ohos --ohos-trace-tags=app,graphic,ohos,freq,idle,sched
```

Clusters are derived from the frequencies observed per CPU, so no core map is
hardcoded and the numbers are right on any device.

These land in `raw.json`; `report.md` stays a curated summary.

**Read `clock_headroom_pct` before trusting an A/B.** A run whose busiest
thread is near its ceiling is limited by the work it does, so an optimisation
moves the result. A run far below it is limited by the governor, and an A/B
there largely measures which side of the ramp each iteration landed on — a
null result from such a run has not tested the change. servoperf prints a
warning when this happens. The same device has been seen in both regimes on
consecutive days.

To inspect a capture directly:

```bash
./target/release/servoperf cpufreq out/<run>/iter_0.hitrace.txt
```

```text
thread                      n    cpu_s   eff_GHz  %of_max  clusters
Canvas                     49    21.69     0.522      24%  mid 100% big 0%
Script                      1    17.25     1.867      76%  big 89% mid 11%
org.servo.servo             9    11.46     0.754      46%  little 95% mid 4% big 1%

cluster                  cpus   mean_GHz    max_GHz    max_OPP_%
little                0,1,2,3      0.677      1.600         5.5%
mid               4,5,6,7,8,9      0.584      2.151         1.2%
big                     10,11      1.692      2.500         5.3%
```

## Workloads

TOML files in [`workloads/`](workloads/). Each names a URL and optional local fixture. To add one: copy an existing TOML, adjust, rerun.

### Scenario workloads

Most workloads are measured by a page-load milestone (FCP/LCP from the trace).
A workload that instead needs to be measured *while it renders* — an
animation, a game, anything with a steady-state frame rate — adds a
`[scenario]` block. The iteration lifecycle is unchanged; what changes is that
the device's presented-frame ring is sampled throughout the window and the app
log is captured at the end. See [`workloads/candle.toml`](workloads/candle.toml).

```toml
[scenario]
surface = "ServoDemoSurface"   # `hidumper -s RenderService -a "fps <name>"`
capture_seconds = 30           # overrides --ohos-capture-seconds
sample_interval_seconds = 3    # ring holds ~384 frames; samples must overlap
```

This always reports `presented.fps`, `presented.frames`, `presented.span_s`
and `frame_time_ms.p50/p95/p99` — raw numbers that assume nothing about which
frames "count".

**Post-processing is opt-in per workload**, because no single interpretation
suits every scenario:

```toml
[[scenario.post_processing]]
kind = "exclude-idle-gaps"
gap_ms = 80.0
```

`exclude-idle-gaps` drops inter-frame intervals of at least `gap_ms` from both
the frame count and the elapsed time, adding `presented.fps_active` next to
the unfiltered `presented.fps`. That is right for an animation that idles
between bursts, and **wrong** for a continuously-rendering workload such as a
WebGPU game, where a 100 ms frame is a dropped frame and excluding it would
flatter the result. Omit the block there and read `presented.fps` with the
frame-time percentiles.

Picking `gap_ms` is a judgement call, so the tool checks it: `frames_near_gap`
counts frames within a factor of two below the threshold, and the report warns
when they exceed 10% of the run — the sign that genuine slow frames are being
censored as idle.

Two further options answer "why is this workload not faster":

```toml
refresh_hz = 60      # bin frame intervals against the panel's refresh
thread_cpu = true    # sample /proc thread CPU across the window
```

`refresh_hz` adds `frame_time_ms.vsync{1,2,3,4plus}_pct`: how many refresh
intervals each frame occupied. A workload sitting at 99% "next vsync" is
*capped* by the display, not by its own work, and its frame time says nothing
about how much headroom it has — that only shows up by making the workload
heavier. The reported `frame_time_ms.p10/p25/p50/p75` exist for the same
reason: a page alternating 16.7/33.3 ms and one uniformly at 25 ms have similar
medians and need opposite fixes.

`thread_cpu` adds a CPU-milliseconds-per-presented-frame table per thread,
**with the thread count**, which is the part that makes it interpretable: 26 ms
across four canvas workers is 6.6 ms of wall clock and fits inside a 16.7 ms
frame, whereas 21 ms on a single thread cannot. Divide by `threads` to get the
floor that group puts under the frame time, and read `busy` (share of one core)
to tell a saturated thread from a blocked one.

Note it divides by *presented* frames while the CPU counters cover the whole
window, so on a workload that alternates bursts with idle — `candle` spends a
third of its window idle — the per-frame figures blend both phases. For
per-frame costs that mean what they say, use a workload that renders
continuously (`fullviewport`).

Pages that report their own numbers (via `console.log`, which reaches hilog on
OHOS) can export them as metrics:

```toml
[[scenario.log_metric]]
name = "chart_rebuild_ms"
pattern = 'init_to_finished_ms=([0-9.]+)'
aggregate = "median"   # or mean / min / max / first / last / count
```

The first capture group is parsed as a number, once per match, and reduced by
`aggregate` to the single value stored for that iteration. Work-normalized
metrics like this are worth having: they stay comparable when frame rate moves
for reasons unrelated to the change under test.

## Tests

```bash
cargo test              # unit + parser fixture tests
cargo test -- --ignored # + e2e smoke with fake servoshell
```
