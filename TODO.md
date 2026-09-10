# servoperf TODO

Open work, roughly in priority order. Measured facts that motivate an item are
dated so a later reader can tell what still needs re-checking.

## 1. Per-reflow instruction distribution (time-slicing)

`instructions.per_reflow` is currently a mean: reflow instructions ÷ reflow
count. That hides the shape of the data, and the shape is what matters — a page
load runs one enormous initial layout followed by a few hundred small
incremental ones, so the mean moves whenever the composition of that tail moves,
even if no individual layout got cheaper.

The pieces to fix this already exist:

- `hiperf` samples carry timestamps in the device's monotonic clock (noted in
  the `perf_data.rs` header: "per-sample timestamps are in the same monotonic
  clock as the trace").
- `parse_hitrace_text` gives every `LocalFrameView::performLayout` /
  `handle_reflow` span an exact `[ts_ns, ts_ns + dur_ns]` interval.

So: bucket each sample into the layout span whose interval contains its
timestamp, and sum periods per span. That yields one instruction count per
layout invocation, i.e. a distribution instead of a mean — enough to separate
"the first layout got 15% cheaper" from "there were fewer trivial relayouts".

Open questions before implementing:

- Clock alignment. Both are monotonic, but confirm they share an epoch rather
  than assuming it (compare a known marker against sample timestamps in the
  same capture). If they don't, anchor on a span that also appears in the perf
  data.
- Nested/re-entrant layout spans: attribute to the innermost containing span,
  matching the existing "deepest occurrence wins" rule in
  `aggregate_inclusive_from_perf_data`.
- Samples outside any layout span are the non-reflow remainder; keep them as a
  bucket rather than dropping them, so the buckets sum to the capture total.
- Report shape: per-iteration percentiles (p50/p90/max) plus the count, rather
  than dumping every span into `raw.json`.

## 2. `[[steps]]` — inject device-side actions inside the capture window

Today the only "app work" a workload can express is: launch with a URL,
optionally through a fixture, and hold a render window open. There is no way to
drive the app, which blocks the scroll half of the mossel plan below.

Proposed schema:

```toml
[[steps]]
after_ms = 2000
run = "uitest uiInput swipe 540 2200 540 600 600"
```

Executed as `hdc shell <run>` on a timer thread started at `aa start`, inside
the same window as the hitrace/hiperf capture. Notes:

- `uitest uiInput swipe` is the OHOS-native injector and needs no app support.
- Keep the steps thread separate from the frame-ring sampler thread; both
  already run alongside the blocking `hiperf record` inside the `thread::scope`
  in `run_iteration`.
- Fail loudly if a step's `after_ms` exceeds the capture window.

## 3. Screen wakelock for the duration of a run

Nothing in the crate takes one. OHOS backgrounds and then *freezes* an app when
the screen sleeps, so a capture can silently cover a partly-suspended process:
measured 2026-07 on SGT-AL50, 2341 samples frozen vs 6227 awake, and reflow
7.6% vs 11.3% — a ~50% error with no warning in the output.

Take `hidumper -s PowerManagerService -a "-t"` before the first iteration,
release with `-f` on Drop, mirroring `guard_trace_level`. Also worth asserting
`screenLocked=false` via `hidumper -s ScreenlockService -a "-all"` at preflight,
since the wakelock prevents future screen-offs but does not dismiss an existing
lock screen.

## 4. Generalise `prepare-arkweb-symbols`

The device-side push target is hardcoded to
`/data/local/tmp/symbols/libarkweb_engine.so` (`DevicePaths::SYMBOL_FILE`), so a
third-party engine whose symbols live in `.gnu_debugdata` cannot use it.

*Partly resolved 2026-09-10:* `bench` no longer pushes anything — symbol
resolution is host-side, and the 290 MB push cost more than the capture. The
`prepare-arkweb-symbols --push` path still hardcodes the name; that flag is
also declared `default_value_t = true` on a `bool`, which clap renders as a
set-true flag with no way to pass `--push=false`.

## 5. Servo `reflow.count` requires a tracing-enabled hap

`handle_reflow` carries `#[servo_tracing::instrument]`, but the macro expands
behind `#[cfg_attr(feature = "tracing", …)]`. A hap built without that feature
emits no Servo spans at all and `reflow.count` silently stays 0 — verified
2026-09-09 on PLR-AL00 against the installed `org.servo.servo`, which produced
zero Servo markers even with `persist.hitrace.level.threshold=Debug` and the
`--tracing-filter` arg set.

Emit a warning when an engine declares `reflow_spans` and the iteration's trace
contains none of them, so this is diagnosed rather than silently reported as 0.

## 6. The `ports/arkweb` shim in this workspace emits no trace spans

Build-configuration dependent, not a property of every Servo-backed image: the
`ports/arkweb` crate on branch `arkweb` has no `tracing` dependency and never
installs a subscriber — verified 2026-09-09 by grep and on DAYU200, where a
12 s capture of the shim rendering m.huaweimossel.com contained zero
`handle_reflow` markers. With that build, `reflow.count` and therefore
`instructions.per_reflow` are unavailable for the Servo leg while the ArkWeb
side has them (Blink emits its markers unconditionally under `nweb`). A build
that does enable tracing needs none of the work below.

The tracepoint itself is upstream — `#[servo_tracing::instrument(skip_all)]` on
`LayoutThread::handle_reflow` is on servo's `origin/main` — but it expands to
nothing unless the `tracing` feature is on, and nothing installs a subscriber.
`ports/arkweb/Cargo.toml` declares `servo = { features = ["js_jit",
"clipboard"] }`, so both halves are missing.

Fix, all inside `ports/arkweb` (branch `arkweb`, not upstream):

1. add `"tracing"` to the servo features, plus the `tracing`,
   `tracing-subscriber` and `hitrace` deps;
2. copy servoshell's `HitraceLayer` (`ports/servoshell/lib.rs`, ~30 lines
   behind `tracing-hitrace`) and install it from `LibraryLoaded` with the
   filter servoshell uses, `info,[{servo_profiling=true}]=trace`.

Then rebuild and reinstall `libservo_arkweb.so` (~153 MB, into `/system/lib64`,
so the device needs a writable system image). Until that lands, compare the
engines on absolute reflow instructions per load — `instructions.*` needs none
of this, only the PMU and a symbol file.

## 7. servoperf ergonomics found while running the mossel suite

*Resolved 2026-09-10: `--hdc-target`, `--trace_begin` errors surfaced, symbol push dropped, zero-`reflow.count`
warning, Windows `.exe` / `USERPROFILE` handling.* Remaining below.
- **`--instructions-period 100000` is too dense for this board.** It dropped
  4528 samples per iteration on DAYU200 even with `--delay-unwind`; 250000 is
  the working value there. The PLR is fine at 100000.
- **`--overwrite` + a long window silently discards the load burst.** hitrace
  keeps the *newest* data, so a 40-75 s capture of a page load on DAYU200 threw
  away the first seconds and reported 0-6 `performLayout` calls where a 15-20 s
  capture of the same page reported 118-170. The engine was fine — a screenshot
  at 45 s shows the page fully rendered. A zero `reflow.count` now warns and
  names this cause, but that only catches the total wipe-out; a *partial* one
  still passes silently. Real fix: compare the trace's first timestamp against
  `aa start` and warn when the capture does not reach back to launch.
- **Default trace tags are too heavy for long windows.** `app,graphic,ohos,…`
  produced 163 MB of trace text for a 12 s capture; at 75 s that overflows any
  allowed buffer. `--ohos-trace-tags app,nweb` keeps the Blink layout markers
  and drops the RenderService flood.

## 8. Dependency audit (2026-09-10)

OSV over the 247 locked packages reports four advisories, none of which
warrants acting before a measurement campaign:

| advisory | crate | applies here? |
| --- | --- | --- |
| RUSTSEC-2026-0190 | anyhow 1.0.102 | **No.** Unsoundness in `Error::downcast_mut()`; nothing in `src/` downcasts. Fixed in 1.0.103. |
| RUSTSEC-2026-0258 | h2 0.4.13 | **Barely.** Unbounded empty DATA frames (DoS) in the h2 the fixture server speaks — a localhost server the device reaches over `hdc rport`. Fixed in 0.4.16. |
| RUSTSEC-2026-0185 | quinn-proto 0.11.14 | **No.** `cargo tree -i` finds no path; it is in the lockfile but never compiled. |
| RUSTSEC-2025-0134 | rustls-pemfile 2.2.0 | Unmaintained, no fix published. rustls 0.23 can parse PEM itself; migrating is optional cleanup. |

A plain `cargo update` (semver-compatible, ~145 crates) clears the first three.
Safe, but run it **between** campaigns, never inside one — the lockfile is part
of the instrument.

Major bumps deliberately deferred: `object` 0.36→0.40, `addr2line` 0.24→0.27,
`gimli` 0.31→0.34, `linux-perf-data` 0.11→0.13. These *are* the symbolization
path, they are version-coupled (addr2line pins gimli and object), and changing
them can change the numbers. `addr2line` was added at 0.24 rather than 0.27 for
exactly that coupling. Do them as one change, after a review deadline, and
verify by re-aggregating a stored `perf.data` against known per-symbol totals —
that check caught nothing when the symbolizer was rewritten, which is the
standard to hold a version bump to.

Cosmetic and not worth churn: `thiserror` 1→2, `toml` 0.8→1, `rand` 0.8→0.10,
`prost` 0.13→0.14, `cpp_demangle` 0.4→0.5.

## 9. Plan: strengthen the mossel scenario

Goal: replace the single-URL `cdn-huaweimossel` workload with a suite covering
all five site pages, each measured twice — once as a page load, once with
scrolling injected — reporting `instructions.per_reflow` for both engines.

**Phase 0 — inputs (done 2026-09-09).** The site is a uni-app SPA in hash mode
(`router:{mode:"hash",base:"/"}` in `/assets/index-*.js`), so its five tabBar
pages are:

| # | URL | title | logged-out content | scroll value |
| --- | --- | --- | --- | --- |
| 1 | `https://m.huaweimossel.com/#/pages/index/index` | 首页 | banners + promo modal | good |
| 2 | `https://m.huaweimossel.com/#/pages/sort/sort` | 分类 | sidebar + product list | excellent |
| 3 | `https://m.huaweimossel.com/#/pages/topic/articles` | 种草 | masonry image feed | excellent (heaviest) |
| 4 | `https://m.huaweimossel.com/#/pages/tabbarcart` | 购物车 | **empty** | poor |
| 5 | `https://m.huaweimossel.com/#/pages/user` | 我的 | login card + grid | moderate |

All five verified rendering on PLR-AL00 in `com.huawei.hmos.browser`.

The cart page is empty when logged out — it lays out almost nothing, so it
contributes a near-zero reflow bucket. Either keep it deliberately as a
low-layout control, or substitute a content page; `pages/product/ranking` and
`pages/search/search` both exist and load without a query parameter.

Environment hygiene seen while verifying, all of which perturb a capture:

- The browser raises an "Allow notifications?" permission dialog on this origin
  on every cold start. Deny it once per profile before benching, and confirm it
  is gone — it overlays the page and changes what gets laid out. (The ArkWeb
  wrapper hap under test most likely never raises it; this is a stock-browser
  problem.)
- A device-level "Use USB to…" dialog can be up from the hdc session.
- The home page shows its own promo modal (关闭 / 去看看) over the content.

**Phase 1 — load workloads (written 2026-09-09).** `mossel-index`,
`mossel-sort`, `mossel-articles`, `mossel-cart`, `mossel-user`. They point at
the **live** site: no `[fixture]` block yet, so run-to-run network variance is
in the numbers.

*Still to do:* record a WPR archive per page (`--ohos-record-seconds 45`; 15 s
was too short for the image set) and add the `[fixture]` block. Hash-routed
navigation means WPR keys all five on the same pre-`#` URL — verify one archive
actually replays a second route before recording five, and if it does not, fall
back to one shared archive recorded by visiting all five routes in a single
record pass.

**Phase 2 — scroll workloads (written 2026-09-09).**
`mossel-<page>-scroll.toml` for each of the five: a `[scenario]` block on
surface `RosenWeb` — the Web component's content surface, verified present
under both the stock ArkWeb engine and the Servo backend on DAYU200 — with
`capture_seconds = 20`, plus a `[[steps]]` swipe every 1.5 s from 6 s in
(~9 scrolls per iteration).

**Phase 3 — reporting.** Run each workload against both engines
(`--ohos-bundle org.servo.servo` / the ArkWeb wrapper bundle) with
`--with-instructions`. Compare by `instructions.per_reflow` and by
`instructions.<reflow fn>` per iteration; treat the reflow *share* of the
capture as secondary, since the denominator is the noisy part.

**Phase 4 — validation.** Before trusting any comparison:

- Confirm the reflow markers are present in each iteration's trace. One capture
  in six produced a 34 MB trace with zero Blink markers (cold start raced the
  window), 2026-09-09.
- Confirm `Sample lost: 0` (now surfaced as a warning).
- Confirm which engine is actually rendering — the PLR served a Servo-derived
  `libhtweb_core.so` in July 2026 and genuine Chromium a week later.
- Read CPU-frequency headroom (`servoperf cpufreq`) before comparing legs, and
  alternate leg order.

### Measured baseline (2026-09-09, DAYU200, `mossel-index`, live site)

5 iterations per leg, 20 s window, `--instructions-period 250000`,
`--ohos-trace-tags app,nweb`, same wrapper app, engine switched with the
device's engine-selection system parameter:

| metric | median | CV |
| --- | --- | --- |
| ArkWeb `Document::UpdateStyleAndLayout(` | 242.3 M instr | 36.7% |
| ArkWeb `reflow.count` | 328 | 25.8% |
| **ArkWeb `instructions.per_reflow`** | **0.78 M instr** | **18.0%** |
| Servo `<LayoutThread as Layout>::reflow` | 84.8 M instr | 32.3% |
| Servo `reflow.count` | — (shim has no tracing, item 6) | — |

Two things worth keeping:

- **`per_reflow` is tighter than either of its inputs** (18.0% vs 36.7% and
  25.8%). Numerator and denominator co-vary — a run that lays out more times
  also spends proportionally more instructions — so the ratio cancels most of
  the load-to-load variance. That is the argument for reporting it rather than
  the totals.
- **The engine totals are not comparable as fidelity-equal work.** Servo's
  84.8 M against ArkWeb's 242.3 M is ~2.9x lower, but a screenshot of the same
  URL shows ArkWeb rendering the page fully (banners, product rails, promo
  modal) while Servo renders the tab bar over a flat grey body. Servo is doing
  less layout, not necessarily cheaper layout. Re-measure once the Servo
  backend renders this page fully, or compare on a page both engines render
  identically.

### DAYU200 is a poor host for this workload

The mossel home page on the rk3568 board reached FCP at 16 s and 29 s in two
iterations of the same run, and a 40 s capture window recorded only 1-6
`performLayout` calls (against 250-270 in a 12 s window on the PLR). Either
give the board a much longer window, use one of the lighter routes, or run the
suite on a phone. The Servo backend's numbers were stable there
(`Layout>::reflow` 110.0 / 112.5 / 113.75 M instructions, ±1.7%) because it
lays out much less of the page — see the fidelity caveat below.

### Expected precision

Measured 2026-09-09, five cold loads of `m.huaweimossel.com` in the HarmonyOS
browser, 12 s hitrace windows:

| denominator | run-to-run CV |
| --- | --- |
| `LocalFrameView::performLayout` | 2.9% (251–270) |
| `UpdateLayoutTree` | 1.8% (231–243) |
| `Blink.ForcedStyleAndLayout.UpdateTime` | 21% (6 892–10 869) |

`performLayout` is the configured denominator: it counts layouts that actually
ran. The forced-style-and-layout count is 26× larger and unstable because most
of those calls find style already clean and do no layout — it measures how often
JS asked, not what layout cost.

With the numerator at ~±3% (541.6 M vs 512.4 M across the two 2026-07 fp runs)
and ~1% sampling error at `--period 100000`, `instructions.per_reflow` should
land within ±4–6% run-to-run, and considerably tighter on a median of 15
iterations.
