# Running servoperf from a Windows host

servoperf never runs code on the host under test — it drives the device over
`hdc` and parses what comes back. So a Windows host needs the Rust toolchain,
`hdc.exe`, and the two symbol files. Nothing else for a first run.

## 1. Host prerequisites

| what | why | notes |
| --- | --- | --- |
| Rust (MSVC toolchain) | building servoperf | `rustup default stable-x86_64-pc-windows-msvc` |
| `hdc.exe` | every device interaction | ships in the OpenHarmony command-line tools, under `sdk/default/openharmony/toolchains/`. Put that directory on `PATH`, or pass `--hdc-bin C:\path\to\hdc.exe`. It must match the device's hdc version. |
| `protoc` | **not needed** | only the default `pftrace` feature wants it (Perfetto trace parsing for the `dump` command and desktop-target bench/ab). Measuring a device never uses it, so build with `--no-default-features`. If you want it anyway: `winget install Google.Protobuf`, or set `PROTOC` to an existing `protoc.exe`. |

servoperf is its own repository — it appears in the servo workspace as the
submodule `tools/servoperf`, but on Windows just clone it directly:

```powershell
git clone https://github.com/jschwe/servo-perf.git
cd servo-perf
git switch reflow-count
cargo build --release --no-default-features
```

`--no-default-features` is not optional unless you have `protoc` installed: the
default feature set compiles a Perfetto proto schema at build time, and without
`protoc` the build fails in `build.rs` before compiling any Rust. Nothing on
the device-measurement path needs it.

Everything below assumes `.\target\release\servoperf.exe` and a working
directory of the repo root.

**Use the MSVC toolchain.** `x86_64-pc-windows-gnu` should work — every
dependency supports it — but `rustls`'s `ring` backend wants a mingw-w64
toolchain there and is the one thing likely to fight you. servoperf links
against no Servo or mozjs artifacts, so there is nothing to gain from the GNU
ABI here.

**You do not need Go, WPR, or Python for a first run.** Go is only required
later, to build the `wpr` replay proxy — see §6.

## 2. Device prerequisites

- A device that can select the webview backend, switched with whatever system
  parameter your image uses:

  ```powershell
  hdc shell param set <engine-param> <value>
  ```

  servoperf does not care which parameter it is or when you set it — it never
  touches the engine selection. Set it before the leg and tell servoperf which
  engine to expect with `--engine`.

  The parameter alone is not proof that the engine changed — it takes effect
  on the next cold start, so a stale process keeps the old one. Step 1 of
  [reflow-benchmark.md](reflow-benchmark.md) has the check.

- The wrapper app installed (`org.openharmonyrs.arkwebtest`). The same app
  serves both engines — that is the point of it. Check first:

  ```powershell
  hdc shell bm dump -a | findstr arkwebtest
  ```

  Two sources, and they are not equivalent:

  - **Canonical:** vendored in the servo tree at `ports/arkweb/test-app`, on
    the `arkweb` branch, kept in step with the shim and driven by the pytest
    suite in `ports/arkweb/tools/`. Build, sign and install with
    `uv run ports/arkweb/tools/build_test_app.py` (add `--no-install` to only
    build). That needs node, hvigor and a *writable* OHOS SDK, so it is far
    easier on the Linux box than on Windows — build the `.hap` there and
    `hdc install` it from Windows.
  - **Standalone:** `https://github.com/jschwe/arkweb-test.git`, the original
    browser-wrapper repo the vendored copy came from. Its `vendored-sync`
    branch carries the app code from the vendored copy; `main` is older. Fine
    if you only need the app, but it can drift from the shim.

  Signing: generate your own material in DevEco Studio for the device and
  point `build-profile.json5`'s `signingConfigs` at it. (`build_test_app.py`
  defaults to the public OpenHarmony debug certs vendored at
  `test-app/signing`, which a board accepts but a HarmonyOS phone does not.)
- `hiperf` on the device (it is part of the OHOS image).
- If more than one device is attached, pass `--hdc-target <serial>`
  (`hdc list targets` prints serials).

## 3. Next: run the benchmark

Symbol staging, the smoke run, the full matrix, how to read the output and what
every knob is for live in **[reflow-benchmark.md](reflow-benchmark.md)** — that
file is host-agnostic; only the path syntax differs here.

For PowerShell, build a splattable argument string once:

```powershell
$c = "--ohos --ohos-bundle org.openharmonyrs.arkwebtest --with-instructions " +
     "--ohos-trace-tags app,nweb --ohos-capture-seconds 20 --instructions-period 1000000"
.\target\release\servoperf.exe bench mossel-index $c.Split(' ') --engine arkweb --out out\arkweb
```

## 4. Later: WPR replay

The `mossel-*` workloads currently hit the live site, so network variance is in
the numbers. Adding a `[fixture]` block pins them to a recorded archive, which
needs the `wpr` binary on the host — see [wpr-setup.md](wpr-setup.md). That
does require a Go toolchain (>= 1.23, plus the documented cert-minting patch for
Go 1.25). A Windows `wpr.exe` can also be cross-compiled from a Linux host with
`GOOS=windows GOARCH=amd64 go build -o wpr.exe ./src/wpr.go`, which avoids
installing Go on the Windows machine. Point servoperf at it with
`SERVOPERF_WPR_BIN`.
