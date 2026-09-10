# Measuring reflow instructions on an OHOS device

End-to-end process for comparing how many CPU instructions a web engine spends
in reflow, using the arkweb wrapper app to run the same page on either the
system ArkWeb (Chromium) engine or Servo.

Host setup is separate: [windows-setup.md](windows-setup.md) for a Windows
host; on Linux the only difference is path syntax.

The two numbers this produces:

- **`reflow.count`** — completed layouts, counted from the engine's own trace
  spans in the same capture the instruction counts come from.
- **`instructions.per_reflow`** — reflow instructions ÷ that count. This is the
  number to compare between engines: measured 18% run-to-run variation against
  36.7% for the raw instruction total and 25.8% for the count, because
  numerator and denominator co-vary.

## 1. Device prechecks

Three commands, each of which catches a failure that otherwise shows up as a
silent zero.

```sh
hdc shell id                                    # root is easiest; see below
hdc shell "hiperf record -a -d 3 -e hw-instructions -o /data/local/tmp/t.data"
hdc shell bm dump -a | grep arkwebtest          # the wrapper app is installed
```

servoperf records **system-wide** (`hiperf record -a`), which is what lets it
capture Chromium's out-of-process `:render` without chasing its pid. If `-a` is
refused on a non-root shell, that half of the measurement will not work.

Then start the app on the engine you want and check which library actually
loaded. The engine-selection parameter takes effect on the next **cold start**,
so a still-running process silently keeps the old engine — this is the single
easiest way to mis-attribute a whole run:

```sh
hdc shell "aa force-stop org.openharmonyrs.arkwebtest"
hdc shell "aa start -b org.openharmonyrs.arkwebtest -a EntryAbility \
    -U https://m.huaweimossel.com/#/pages/index/index"
hdc shell "grep -oE '/[^ ]*(arkweb_engine|servo)[^ ]*\.so' \
    /proc/$(hdc shell pidof org.openharmonyrs.arkwebtest)/maps | sort -u"
```

Keep the path it prints — it names the library to pull in step 2.

## 2. Stage symbol files (once per engine)

Instruction counts are resolved on the host against an unstripped ELF under
`workloads/`. Without it every `instructions.*` is 0 (with a warning).

```sh
# Servo — ships with a full .symtab, copy it straight off the device
hdc file recv /system/lib64/libservo_arkweb.so workloads/libservo_arkweb.symbols.so

# ArkWeb — stripped to .dynsym + compressed .gnu_debugdata; re-attach symbols
hdc shell "cp /data/storage/el1/bundle/arkwebcore/libs/arm64/libarkweb_engine.so /data/local/tmp/ae.so"
hdc file recv /data/local/tmp/ae.so ae.so
servoperf prepare-arkweb-symbols --input ae.so --output workloads/libarkweb_engine.merged.so
```

The staged filename must match the device library's basename: the parser strips
a `.symbols.so` / `.merged.so` suffix and matches what the perf.data mmap
records name. If step 1 printed a differently-named library, rename the file
and update `symbol_file` in that engine's entry in
[`workloads/_instructions.toml`](../workloads/_instructions.toml).

`workloads/*.so` is gitignored.

## 3. Smoke run — one iteration per engine

Do this before committing an hour to the full matrix.

```sh
COMMON="--ohos --ohos-bundle org.openharmonyrs.arkwebtest --with-instructions \
        --ohos-trace-tags app,nweb --ohos-capture-seconds 20 \
        --instructions-period 1000000"

# device switched to ArkWeb:
servoperf bench mossel-index $COMMON --engine arkweb --ohos-trace-level "" \
    --iterations 1 --out out/smoke-arkweb

# device switched to Servo:
servoperf bench mossel-index $COMMON --engine servo-arkweb \
    --iterations 1 --out out/smoke-servo
```

`--engine` is required: both legs launch the same bundle, so the engine cannot
be inferred from the bundle name.

Check three things in `out/smoke-*/raw.json` before going further:

1. `reflow.count` is in the low hundreds — 190-346 for this page on a phone.
   Zero prints a warning naming the causes.
2. The reflow `instructions.*` entry is non-zero.
3. No sample-loss warning, or under ~1%.

## 4. The full matrix

One command runs every workload against every engine:

```sh
servoperf suite mossel
```

No device flags: the suite's `[device]` section carries the bundle, and
optionally the serial, thermal zone and trace-buffer size, so a campaign is
reproduced by the file rather than by remembering a flag list. A flag actually
passed still wins — these fill in only where the command line is on its
default.

The matrix and its settings live in [`suites/mossel.toml`](../suites/mossel.toml)
— repetitions, capture window, sampling period, trace tags, which workloads are
in, and one `[[leg]]` per engine. Edit it rather than assembling flags: a
campaign where one leg quietly ran with a different window than the other is
not a comparison, and the file makes that visible in review.

Each cell lands in `<out>/<leg>-<workload>/`, a normal `bench` output
directory, and the run writes `<out>/comparison.md` tabulating the legs side by
side per workload — totals, `reflow.count`, and a `per_reflow` for each
grouping, with a percentage column when there are exactly two legs. A failing
cell is reported and the run continues.

Selecting the engine is the one thing servoperf will not guess. Give a leg a
`setup` command — `setup = "param set <engine-param> <value>"`, run as
`hdc shell` before the leg — or leave it out and the run pauses and asks you to
switch by hand. `--assume-yes` skips the prompt when the device is already set.

Useful for a first pass:

```sh
servoperf suite mossel --only mossel-index --legs arkweb --iterations 1
```

Budget ~7 minutes per workload per leg. The `-scroll` variants need `uitest` on
the device.

All ten workloads pass `--chrome=none`, which hides the wrapper app's toolbar
so the Web component fills the window. That is not cosmetic: the toolbar costs
ArkUI layout every frame and shrinks the viewport, so it changes how much of
the page has to be laid out — it lands in the number being measured. Numbers
taken with and without it are not comparable; re-baseline if you switch.

Two things about the workloads:

- **`mossel-cart` renders almost nothing** when logged out, so it was meant as
  a low-layout control — but a first run on DAYU200 recorded 632 layouts
  against 19 for `mossel-user`, i.e. the opposite of the intent. Something on
  that page relayouts repeatedly. Check what it is before reading anything into
  its numbers.
- **Launch the app once by hand after installing it.** Without `--bin`,
  servoperf skips its install cooldown and warmup, and the first cold launch
  after an install runs ~3x slow, skewing iteration 0.

## 5. Reading the results

`out/<dir>/report.md` for the summary, `raw.json` for per-iteration values.

Compare on `instructions.per_reflow`. Treat reflow's *share* of the capture as
secondary — the capture total is the noisy part, moving with V8, network and
GC while the reflow footprint itself stays stable.

**Compare like for like.** Servo's `handle_reflow` covers style recalc, box and
fragment trees, **plus** the stacking-context tree and display-list building.
Blink's `LocalFrameView::UpdateStyleAndLayout` covers only the first group —
SCT and display lists live in Blink's paint lifecycle. Comparing
`Layout>::reflow` against `UpdateStyleAndLayout` therefore charges Servo for
paint prep that the Chromium number excludes.

Three groupings are reported per engine so the boundary is explicit:

| metric | Servo | Chromium |
| --- | --- | --- |
| `instructions.layout_proper` | `restyle_and_build_trees` | `LocalFrameView::UpdateStyleAndLayout(` |
| `instructions.paint_prep` | `build_stacking_context_tree` + `build_display_list` | `RunPaintLifecyclePhase` + `PrePaintTreeWalk` |
| `instructions.colleagues_reflow` | `Window>::reflow` | `Document::UpdateStyleAndLayout(` + `WebFrameWidgetImpl::UpdateLifecycle` |

Each grouping also gets its own ratio — `instructions.per_reflow.layout_proper`
and so on — alongside the bare `instructions.per_reflow`, which uses the
engine's configured `reflow_instructions` symbol and is also published under
that symbol's own name so it is never ambiguous which numerator produced it.

A group credits a sample **once** if the chain contains any member, so nested
members are not double-counted. That matters for `colleagues_reflow` on the
Chromium side: those two symbols overlap (lifecycle style+layout runs under
`UpdateLifecycle`), so the group comes out *below* the two per-symbol metrics
added by hand, and the gap is exactly the double-count. `UpdateLifecycle` is
also mostly not layout — measured 2026-07, only 18% of its inclusive
instructions contained a layout frame, the rest paint and compositing.

### Inlined boundaries

`restyle_and_build_trees` and `build_display_list` are inlined into
`handle_reflow` at `codegen-units=1`. A symbol table has no record of an
inlined function, so such a boundary reports **0** — which reads as "this phase
is free" rather than "this measurement is blind".

servoperf resolves this when the staged library carries DWARF: it builds an
addr2line context and attributes every function in an address's inline chain,
not just the one the compiler folded the code into. The run prints
`instructions: … carries DWARF; inlined functions are attributed` when this is
active.

Build the library with **`debug = 1`** — the cheapest level that still carries
`DW_AT_linkage_name`, so inline frames demangle to the same `<Type>::method`
form as symbol-table entries and every existing pattern keeps matching.
(`line-tables-only` does carry the inline records but drops the linkage name,
leaving bare identifiers; `2` only adds type information you do not need.) Keep
the unstripped copy on the host as the symbol file and push a stripped one to
the device.

Without DWARF the fallback is `#[inline(never)]` on those two functions, or
bracketing the value as `Layout>::reflow` minus `paint_prep`. ArkWeb has no
option here — its engine ships `.gnu_debugdata` and never DWARF — but Blink
keeps the functions of interest as real symbols, so it does not need one.

**Check fidelity before believing a gap.** Screenshot both legs:

```sh
hdc shell uitest screenCap -p /data/local/tmp/s.png && hdc file recv /data/local/tmp/s.png .
```

On DAYU200 Servo rendered the tab bar over a flat grey body while ArkWeb
rendered the page fully — its ~3x lower instruction count was partly less work,
not cheaper work.

## 6. Tuning

| knob | what it is for |
| --- | --- |
| `--instructions-period` | How many retired instructions pass between samples. hiperf programs the PMU counter to fire every N instructions, and each sample carries N as its weight, credited to every frame in its callchain — so the metric is a sum of periods, not a count of samples, and lowering N buys resolution rather than accuracy. It costs interrupt overhead and, past a point, dropped samples. Measured on PLR-AL00 over 20 s: 100000 → 808k samples, 6.8% lost; 500000 → 295k, 0.43%; **1000000 → 165k, 0%**. Use 1000000 on a phone, 250000 on a slower board. 165k samples still puts ~16k in the reflow bucket, far past the ~1000 needed for ±1%. |
| `--ohos-trace-tags` | Must include `nweb` for ArkWeb — Blink's layout markers are emitted under that tag and there are none without it. The default set also produces ~160 MB of trace text per 12 s, hence `app,nweb`. |
| `--ohos-capture-seconds` | Keep it near the page's load time. hitrace's `--overwrite` keeps the *newest* records, so a long window discards the load burst: on DAYU200 a 75 s window reported 1 layout where 20 s reported 330. |
| `--ohos-trace-buffer-kib` | Ring buffer, default 512 MB. hitrace accepts 256 KiB - 300 MB and some devices enforce the upper end (DAYU200 rejects the default; `--trace_begin` then fails with hitrace's own message). |
| `--ohos-trace-level` | Only the Servo leg needs `Debug`. Blink emits its `nweb` markers regardless of the threshold; Servo's spans are TRACE-level and the daemon's default drops them. Pass `""` on the ArkWeb leg to leave the device untouched. |
| `--hdc-target` | Device serial, when more than one is attached. |

Two levers that do **not** help sample loss: hiperf's `--cpu-limit` (at 100 it
collected twice as many samples and lost 17.9% instead of 6.8%) and `-m`
(already at its 1024-page maximum).

## 7. Stopping a run

Ctrl-C once: the run stops at the next iteration boundary, writes the results
collected so far, and restores the device state it changed (hitrace level,
screen wakelock). It waits for the current capture because abandoning one
mid-`hiperf record` leaves the device holding both.

Ctrl-C twice: stops immediately, still running those restores first.

Before this existed, a Ctrl-C that killed the child `hdc` process was seen by
servoperf as a failed *iteration* — logged, and the loop carried on to the
next — so a long run appeared to ignore it.

## 8. When a number looks wrong

An iteration whose app crashed is reported as **failed**, not as a success
with missing metrics, and the device's own crash report is pulled next to the
other artefacts as `iter_<n>.faultlog.txt`. The run continues; the report's
`Iterations: N ok, M failed` line is where to look, and a run that is more than
half failures aborts.

| symptom | cause |
| --- | --- |
| every `instructions.*` is 0 | symbol file missing or misnamed (step 2) |
| `reflow.count` is 0 | engine build emits no spans; `nweb` missing from the tags; or the ring buffer wrapped past the load |
| `reflow.count` is 1-6 on a real page | capture window too long for the buffer — shorten it |
| counts look plausible but the engine is wrong | a stale process kept the previous engine; force-stop before switching |
| sample-loss warning | raise `--instructions-period` |
| `the app exited during the capture` | it crashed; read `iter_<n>.faultlog.txt` |
| `the app restarted during the capture` | it crashed and was respawned — the same thing, just harder to see |
| `OpenRecording failed, errorCode(1103)` | a previous run left a hitrace recording open. Should no longer happen; clear it with `hdc shell hitrace --trace_finish -o /dev/null` |
| `reflow.count` 0 on Servo only | that build lacks the `tracing` feature or installs no subscriber. `handle_reflow` is instrumented upstream, but the attribute expands only when both are present. `instructions.*` works either way — it comes from the PMU, not from tracing. |
