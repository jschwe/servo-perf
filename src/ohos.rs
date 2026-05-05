// tools/servoperf/src/ohos.rs
//! HarmonyOS / OpenHarmony device target via `hdc`.
//!
//! Replaces the local `Command::new(servoshell_bin)` flow with:
//!   1. `hdc shell aa force-stop <bundle>` — drop any prior instance.
//!   2. `hdc shell hitrace -b <buf> <tags> --trace_begin` — start ring-buffer.
//!   3. `hdc shell aa start -a <ability> -b <bundle> -U <url> --ps=...` — launch.
//!   4. Sleep `capture_seconds` on the host while the device renders.
//!   5. `hdc shell hitrace -b <buf> --trace_finish -o <on-device-path>` —
//!      flush captured events to a text file.
//!   6. `hdc file recv` to pull the text trace to the host.
//!
//! The captured text uses ftrace-style `tracing_mark_write: B|tid|name` /
//! `E|tid|` markers. Servo's `tracing-hitrace` layer emits these for every
//! instrumented span, so the same critical-path registry that drives the
//! perfetto path can be reused — we only need to translate hitrace text
//! into [`crate::trace::Slice`] values.
//!
//! See servo's CI bencher for the canonical sequence:
//! <https://github.com/openharmony-rs/hitrace-bench>.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::cli::OhosArgs;
use crate::trace::Slice;
use crate::workload::Workload;

/// One device-target invocation context. Constructed once per servoperf
/// run and shared across iterations — it carries no per-iteration state.
#[derive(Debug, Clone)]
pub struct OhosTarget {
    pub hdc_bin: String,
    pub hdc_server: Option<String>,
    pub bundle: String,
    pub ability: String,
    pub trace_path_on_device: String,
    pub trace_buffer_kib: u64,
    pub trace_tags: Vec<String>,
    pub capture_seconds: u64,
    pub trace_level: String,
}

impl OhosTarget {
    pub fn from_args(a: &OhosArgs) -> Self {
        Self {
            hdc_bin: a.hdc_bin.clone(),
            hdc_server: a.hdc_server.clone(),
            bundle: a.ohos_bundle.clone(),
            ability: a.ohos_ability.clone(),
            trace_path_on_device: a.ohos_trace_path.clone(),
            trace_buffer_kib: a.ohos_trace_buffer_kib,
            trace_tags: a
                .ohos_trace_tags
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            capture_seconds: a.ohos_capture_seconds,
            trace_level: a.ohos_trace_level.clone(),
        }
    }

    /// Bundle override (used by `ab` to launch base vs patch hap when
    /// they're installed under distinct bundle names).
    pub fn with_bundle(mut self, bundle: String) -> Self {
        self.bundle = bundle;
        self
    }

    /// Run `hdc <args>`, threading `-s <server>` if configured. Returns
    /// the captured stdout on success (stderr is forwarded for surfacing
    /// in failure output).
    fn hdc(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut cmd = Command::new(&self.hdc_bin);
        if let Some(s) = &self.hdc_server {
            cmd.args(["-s", s]);
        }
        cmd.args(args);
        let out = cmd
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {} {:?}", self.hdc_bin, args))?;
        if !out.status.success() {
            anyhow::bail!(
                "hdc {:?} failed (status={}): {}",
                args,
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(out)
    }

    /// Smoke-test the connection: a device must be listed, and `hdc
    /// shell` must work. Called once before iteration starts.
    pub fn preflight(&self) -> Result<()> {
        let targets = self.hdc(&["list", "targets"])?;
        let listing = String::from_utf8_lossy(&targets.stdout);
        anyhow::ensure!(
            !listing.trim().is_empty() && !listing.contains("[Empty]"),
            "no hdc target visible (output: {:?}) — is the device connected and \
             the hdc server running on the device-attached host?",
            listing.trim()
        );
        // `shell echo` confirms the daemon is actually responsive.
        self.hdc(&["shell", "echo", "servoperf-ok"])
            .context("device unreachable via `hdc shell`")?;
        Ok(())
    }

    /// Push a `.hap` file to the device, replacing any prior install. The
    /// hap must already be signed for the device's profile.
    pub fn install_hap(&self, hap: &Path) -> Result<()> {
        anyhow::ensure!(
            hap.is_file(),
            ".hap not found at {} — pass --bin pointing to the signed .hap, \
             or pre-install and omit --bin",
            hap.display()
        );
        // Best-effort uninstall first; hdc isn't reliable about exit codes.
        let _ = self.hdc(&["uninstall", &self.bundle]);
        let hap_str = hap.to_string_lossy().to_string();
        self.hdc(&["install", "-r", &hap_str])
            .with_context(|| format!("installing {}", hap.display()))?;
        Ok(())
    }

    /// Stop any running instance of the bundle. Tolerates "not running".
    pub fn force_stop(&self) {
        let _ = self.hdc(&["shell", "aa", "force-stop", &self.bundle]);
    }

    /// Read the current `persist.hitrace.level.threshold` via
    /// `hitrace --get_level`. The setting is system-wide (not per-app)
    /// and gates *every* call to `OH_HiTrace_StartTraceEx` — including
    /// the `Debug`-mapped TRACE-level spans that Servo emits from
    /// `profile_traits::trace_span!`. Returns the canonical name
    /// (`Debug` / `Info` / `Critical` / `Commercial`) parsed out of
    /// `hitrace`'s line `the current trace level threshold is X`.
    pub fn get_trace_level(&self) -> Result<String> {
        let out = self.hdc(&["shell", "hitrace", "--get_level"])
            .context("hitrace --get_level")?;
        let s = String::from_utf8_lossy(&out.stdout);
        // Tolerate single quotes / trailing whitespace / log prefix —
        // hitrace prefixes lines with a wallclock timestamp, so we
        // scan rather than match the whole output.
        let needle = "trace level threshold is";
        for line in s.lines() {
            if let Some(idx) = line.find(needle) {
                let tail = line[idx + needle.len()..].trim().trim_matches('\'');
                if !tail.is_empty() {
                    return Ok(tail.to_string());
                }
            }
        }
        anyhow::bail!("could not parse hitrace --get_level output: {s:?}")
    }

    /// Set the `persist.hitrace.level.threshold` system parameter via
    /// `hitrace --trace_level <level>`. Valid `level` values per the
    /// hitrace CLI: `D|Debug|I|Info|C|Critical|M|Commercial`.
    pub fn set_trace_level(&self, level: &str) -> Result<()> {
        self.hdc(&["shell", "hitrace", "--trace_level", level])
            .with_context(|| format!("hitrace --trace_level {level}"))?;
        Ok(())
    }

    /// Snapshot the current trace-level threshold and set it to
    /// `desired` for the lifetime of the returned guard. The guard
    /// restores the previous threshold on drop. If `desired` is empty
    /// or already matches the current threshold, returns a no-op
    /// guard (no shell calls on drop). On any failure during readout
    /// we surface the error rather than silently running at the
    /// wrong level — the whole point of this guard is to make the
    /// trace contents predictable.
    pub fn guard_trace_level(&self, desired: &str) -> Result<TraceLevelGuard> {
        if desired.is_empty() {
            return Ok(TraceLevelGuard { target: None, previous: String::new() });
        }
        let previous = self.get_trace_level()?;
        if previous.eq_ignore_ascii_case(desired) {
            return Ok(TraceLevelGuard { target: None, previous });
        }
        self.set_trace_level(desired)?;
        eprintln!(
            "ohos: hitrace level {} → {} (will restore on exit)",
            previous, desired
        );
        Ok(TraceLevelGuard { target: Some(self.clone()), previous })
    }

    /// `hdc shell aa start` with the workload's args translated into
    /// `aa start`'s `--ps=` / `--psn=` parameter encoding. Shared
    /// between [`Self::run_iteration`] (replay/measure) and
    /// [`OhosRecordDriver::drive`] (record).
    fn aa_start(&self, workload: &Workload, proxy_uri: Option<&str>) -> Result<()> {
        let aa_params = workload_args_to_aa_params(workload, proxy_uri);
        let mut start_args: Vec<String> = vec![
            "shell".into(),
            "aa".into(),
            "start".into(),
            "-a".into(),
            self.ability.clone(),
            "-b".into(),
            self.bundle.clone(),
            "-U".into(),
            workload.url.clone(),
        ];
        start_args.extend(aa_params);
        let start_args_ref: Vec<&str> = start_args.iter().map(String::as_str).collect();
        self.hdc(&start_args_ref).map(|_| ())
    }

    /// Set up `hdc rport` for each port so the device can reach the
    /// host's `127.0.0.1:<port>` listener. Returns an [`RPortGuard`]
    /// that tears the forwards down on Drop — the device retains rport
    /// state across app launches, so leaving stale forwards behind would
    /// shadow whatever the user does next.
    ///
    /// Each port is reset before the new mapping is installed so an
    /// orphaned forward from a previous run doesn't make this call
    /// silently no-op.
    pub fn setup_rport(&self, ports: &[u16]) -> Result<RPortGuard> {
        let mut installed: Vec<u16> = Vec::new();
        for &port in ports {
            let spec = format!("tcp:{port}");
            // Best-effort cleanup of any prior forward. hdc unifies
            // fport/rport removal under `fport rm`, with the *local*
            // (host) port first — same argument order as in the setup
            // call below.
            let _ = self.hdc(&["fport", "rm", &spec, &spec]);
            self.hdc(&["rport", &spec, &spec])
                .with_context(|| format!("hdc rport tcp:{port} tcp:{port}"))?;
            installed.push(port);
        }
        Ok(RPortGuard { hdc_bin: self.hdc_bin.clone(), hdc_server: self.hdc_server.clone(), ports: installed })
    }

    /// Run one iteration on the device:
    ///   * stop the app, clear any old trace
    ///   * `hitrace --trace_begin`
    ///   * `aa start … -U <url> [--ps=…]`
    ///   * sleep `capture_seconds`
    ///   * `hitrace --trace_finish -o <on-device-path>`
    ///   * `hdc file recv` → `out_dir/iter_<iter>.hitrace.txt`
    pub fn run_iteration(
        &self,
        workload: &Workload,
        iter: u32,
        out_dir: &Path,
        proxy_uri: Option<&str>,
    ) -> Result<RunArtifact> {
        // Pre-iteration housekeeping.
        self.force_stop();
        let _ = self.hdc(&["shell", "rm", "-f", &self.trace_path_on_device]);

        // Begin trace capture. The buffer arg matches hitrace-bench's CI
        // default — large enough for ~10 s of full-tag capture without
        // overflow on the test devices we have access to.
        let buffer = self.trace_buffer_kib.to_string();
        let mut begin_args: Vec<&str> =
            vec!["shell", "hitrace", "-b", &buffer];
        for tag in &self.trace_tags {
            begin_args.push(tag);
        }
        begin_args.push("--trace_begin");
        self.hdc(&begin_args).context("hitrace --trace_begin")?;

        let spawn_wall_ns = wall_now_ns();
        self.aa_start(workload, proxy_uri)
            .with_context(|| format!("aa start failed for {}", self.bundle))?;

        // Wait for the app to render. The capture window is fixed: too
        // short and we miss FCP, too long and we waste seconds per iter.
        std::thread::sleep(Duration::from_secs(self.capture_seconds));

        // Stop the trace, flushing the buffer to a file on the device.
        let stop_args: Vec<&str> = vec![
            "shell",
            "hitrace",
            "-b",
            &buffer,
            "--trace_finish",
            "-o",
            &self.trace_path_on_device,
        ];
        self.hdc(&stop_args).context("hitrace --trace_finish")?;
        let exit_wall_ns = wall_now_ns();

        // Pull the trace text back to the host.
        let dest = out_dir.join(format!("iter_{iter}.hitrace.txt"));
        let dest_str = dest.to_string_lossy().to_string();
        self.hdc(&["file", "recv", &self.trace_path_on_device, &dest_str])
            .with_context(|| format!("recv {} → {}", self.trace_path_on_device, dest.display()))?;

        // Stop the app so the next iteration starts cold.
        self.force_stop();

        Ok(RunArtifact { trace: dest, spawn_wall_ns, exit_wall_ns })
    }
}

/// What the OHOS path produces for one iteration. Mirrors
/// `runner::RunArtifact` but the trace file is hitrace text, not pftrace.
pub struct RunArtifact {
    pub trace: PathBuf,
    pub spawn_wall_ns: u64,
    pub exit_wall_ns: u64,
}

/// RAII handle that drops `hdc rport` forwards on scope exit. Holds its
/// own copy of the hdc invocation parameters so the parent
/// [`OhosTarget`] doesn't need to outlive it.
pub struct RPortGuard {
    hdc_bin: String,
    hdc_server: Option<String>,
    ports: Vec<u16>,
}

impl Drop for RPortGuard {
    fn drop(&mut self) {
        for port in &self.ports {
            let spec = format!("tcp:{port}");
            let mut cmd = Command::new(&self.hdc_bin);
            if let Some(s) = &self.hdc_server {
                cmd.args(["-s", s]);
            }
            // `hdc fport rm` works for both fport and rport mappings
            // (the daemon distinguishes them internally). `hdc rport rm`
            // is rejected as "Incorrect forward command".
            cmd.args(["fport", "rm", &spec, &spec]);
            // Best-effort: if the device is gone, there's nothing to
            // clean up and we don't want Drop to panic.
            let _ = cmd.stdin(Stdio::null()).output();
        }
    }
}

/// RAII handle that restores the previous `persist.hitrace.level.threshold`
/// on drop. `target` is `None` when no change was made (empty desired or
/// already-matching threshold) — drop is then a no-op. Errors during
/// restore are reported on stderr but never panic.
pub struct TraceLevelGuard {
    target: Option<OhosTarget>,
    previous: String,
}

impl Drop for TraceLevelGuard {
    fn drop(&mut self) {
        if let Some(t) = &self.target {
            if let Err(e) = t.set_trace_level(&self.previous) {
                eprintln!(
                    "warning: failed to restore hitrace level to {}: {e:#}",
                    self.previous
                );
            }
        }
    }
}

fn wall_now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Translate the desktop-style `Workload.servoshell_args` (plus the
/// universal options like `--window-size`, `--user-agent`, the proxy
/// env-vars, etc.) into the `aa start` `--ps=` / `--psn=` form expected
/// by servoshell's OHOS EntryAbility.
///
/// Rules:
///   * `--key=value`               → `--ps=--key`, `value`         (two argv tokens; aa start consumes them as one key+value)
///   * `--key value` (next item)   → `--ps=--key`, `value`
///   * `--key`        (boolean)    → `--psn=--key`
///   * already-OHOS-encoded args (`--ps=…`, `--psn=…`) pass through.
///   * `--headless` / `--exit` / `-o foo.png` are dropped — they don't
///     apply to the OHOS UI ability.
///   * the proxy URI (when set) is converted to two preferences so it
///     reaches servo without env-var inheritance, which doesn't survive
///     the `aa start` boundary.
fn workload_args_to_aa_params(workload: &Workload, proxy_uri: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // First, collect args from the workload definition. We synthesize
    // the same options the local runner injects (window size, dpr, UA)
    // into OHOS form so the same TOML drives both targets.
    let mut synthetic: Vec<String> = Vec::new();
    if let Some((w, h)) = workload.viewport {
        synthetic.push(format!("--window-size={}x{}", w, h));
    }
    if let Some(ratio) = workload.device_pixel_ratio {
        synthetic.push(format!("--device-pixel-ratio={}", ratio));
    }
    if let Some(ua) = workload.user_agent.as_deref() {
        synthetic.push(format!("--user-agent={}", ua));
    }
    synthetic.push(format!("--tracing-filter={}", workload.tracing_filter));
    // LCP fragment-area accounting is gated by an off-by-default pref
    // (`largest_contentful_paint_enabled`). servoperf wants LCP on
    // every bench, so enable it unconditionally. Mirrored in
    // `crate::runner::run_once` for the local path.
    synthetic.push("--pref=largest_contentful_paint_enabled=true".to_string());

    let mut iter = workload
        .servoshell_args
        .iter()
        .chain(synthetic.iter())
        .cloned()
        .peekable();
    while let Some(arg) = iter.next() {
        // Pass-through for already-OHOS-encoded args. We accept both
        // `--ps=--foo bar` (where bar is a separate workload entry) and
        // `--ps=--foo=bar`. aa start splits on whitespace.
        if arg.starts_with("--ps=") || arg.starts_with("--psn=") {
            out.push(arg);
            continue;
        }
        // Drop desktop-only flags that have no OHOS analogue.
        if matches!(arg.as_str(), "--headless" | "--exit") {
            continue;
        }
        if arg == "-o" {
            // skip `-o` and its filename argument
            let _ = iter.next();
            continue;
        }
        if arg == "-u" {
            if let Some(ua) = iter.next() {
                out.push("--ps=--user-agent".to_string());
                out.push(ua);
            }
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--") {
            if let Some(eq) = rest.find('=') {
                let key = &rest[..eq];
                let value = &rest[eq + 1..];
                out.push(format!("--ps=--{}", key));
                out.push(value.to_string());
            } else if iter
                .peek()
                .map(|n| !n.starts_with('-'))
                .unwrap_or(false)
            {
                let value = iter.next().unwrap();
                out.push(format!("--ps=--{}", rest));
                out.push(value);
            } else {
                out.push(format!("--psn=--{}", rest));
            }
            continue;
        }
        // Anything else (e.g. a stray positional) — pass through; aa
        // start will likely reject it but at least the failure points at
        // the right argument.
        out.push(arg);
    }

    // Inject the proxy as preferences. Servo on OHOS reads
    // `network_https_proxy_uri` / `network_http_proxy_uri` prefs.
    if let Some(uri) = proxy_uri {
        out.push(format!(
            "--psn=--pref=network_https_proxy_uri={}",
            uri
        ));
        out.push(format!(
            "--psn=--pref=network_http_proxy_uri={}",
            uri
        ));
        out.push("--psn=--ignore-certificate-errors".to_string());
    }

    out
}

// --- Hitrace text parsing ----------------------------------------------

/// Parse a hitrace text file into the same `Slice` shape used for
/// pftrace-derived data, so [`crate::trace::analyse`] works unchanged.
///
/// Format (from `hitrace --trace_finish -o`, ftrace-style):
///
/// ```text
///   org.servo.servo-44962 ( 44682) [010] .... 17864.716645: tracing_mark_write: B|44682|Servo::new
///   org.servo.servo-44962 ( 44682) [010] .... 17864.717100: tracing_mark_write: E|44682|
///   org.servo.servo-44962 ( 44682) [010] .... 17864.720000: tracing_mark_write: I|44682|FirstContentfulPaint
/// ```
///
/// We pair B/E events on `(tid, name)` (tid is the only reliable
/// per-emitter key — names can repeat). Async S/F pairs are also handled.
/// Instant `I|` markers become zero-duration slices.
///
/// Timestamps appear as `<seconds>.<microseconds>` from the device's
/// monotonic clock; we convert to nanoseconds.
pub fn parse_hitrace_file(path: &Path) -> Result<Vec<Slice>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading hitrace text at {}", path.display()))?;
    Ok(parse_hitrace_text(&text))
}

pub fn parse_hitrace_text(text: &str) -> Vec<Slice> {
    let mut slices: Vec<Slice> = Vec::new();
    // Open begin events keyed by (tid, name) → ts_ns.
    use std::collections::HashMap;
    let mut open_sync: HashMap<(u64, String), u64> = HashMap::new();
    let mut open_async: HashMap<(u64, String), u64> = HashMap::new();
    // Cache thread name per tid; ftrace prints it before each event so
    // the last value wins in the (rare) case it changes.
    let mut tid_names: HashMap<u64, String> = HashMap::new();

    for line in text.lines() {
        let Some(ev) = parse_hitrace_line(line) else {
            continue;
        };
        // Remember the comm string so we can populate Slice.thread.
        if !ev.comm.is_empty() {
            tid_names.insert(ev.tid, ev.comm.clone());
        }
        let thread_name = tid_names
            .get(&ev.tid)
            .cloned()
            .unwrap_or_else(|| format!("tid:{}", ev.tid));
        match ev.marker {
            Marker::BeginSync => {
                open_sync.insert((ev.tid, ev.payload.clone()), ev.ts_ns);
            }
            Marker::EndSync => {
                // EndSync sometimes carries its begin's name and
                // sometimes is empty. Try the named match first; on
                // miss, close the most-recent open sync on this tid
                // (LIFO — span depth on one thread is shallow in
                // practice, so iterating the HashMap is cheap).
                let key_named = (ev.tid, ev.payload.clone());
                let (matched_name, matched_ts) =
                    if !ev.payload.is_empty() && open_sync.contains_key(&key_named) {
                        let ts = open_sync.remove(&key_named).unwrap();
                        (ev.payload.clone(), ts)
                    } else {
                        let key_to_close = open_sync
                            .iter()
                            .filter(|((tid, _), _)| *tid == ev.tid)
                            .max_by_key(|(_, ts)| *ts)
                            .map(|(k, _)| k.clone());
                        match key_to_close {
                            Some(k) => {
                                let ts = open_sync.remove(&k).unwrap();
                                (k.1, ts)
                            }
                            None => continue,
                        }
                    };
                slices.push(Slice {
                    name: matched_name,
                    thread: thread_name,
                    ts_ns: matched_ts,
                    dur_ns: ev.ts_ns.saturating_sub(matched_ts),
                    debug_annotations: vec![],
                });
            }
            Marker::BeginAsync => {
                open_async.insert((ev.tid, ev.payload.clone()), ev.ts_ns);
            }
            Marker::EndAsync => {
                let key = (ev.tid, ev.payload.clone());
                if let Some(ts) = open_async.remove(&key) {
                    slices.push(Slice {
                        name: ev.payload,
                        thread: thread_name,
                        ts_ns: ts,
                        dur_ns: ev.ts_ns.saturating_sub(ts),
                        debug_annotations: vec![],
                    });
                }
            }
            Marker::Instant => {
                slices.push(Slice {
                    name: ev.payload,
                    thread: thread_name,
                    ts_ns: ev.ts_ns,
                    dur_ns: 0,
                    debug_annotations: vec![],
                });
            }
            Marker::Counter => {
                // Counters aren't part of the critical-path model;
                // ignore. (hitrace-bench surfaces them as point
                // filters; servoperf doesn't yet.)
            }
        }
    }

    // Always sort by timestamp so analyse() can walk in order.
    slices.sort_by_key(|s| s.ts_ns);
    slices
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    BeginSync,
    EndSync,
    BeginAsync,
    EndAsync,
    Instant,
    Counter,
}

#[derive(Debug)]
struct ParsedLine {
    comm: String,
    tid: u64,
    ts_ns: u64,
    marker: Marker,
    payload: String,
}

/// Parse a single hitrace text line. Returns `None` for non-event lines
/// (banner, blank lines, anything not matching the ftrace tracing-mark-write
/// shape).
///
/// The format is ftrace-derived; representative lines look like:
///
/// ```text
///   <comm>-<tid>  ( <pid> ) [cpu] flags ts_s.ts_us: tracing_mark_write: <type>|<tid>|<payload>
/// ```
///
/// Some emitters omit the parenthesized pid (it then becomes `(-------)`),
/// which is why we don't anchor on it. We only care about `comm`, `tid`,
/// the timestamp, and the trailing `<type>|<tid_in_payload>|<name>`.
fn parse_hitrace_line(line: &str) -> Option<ParsedLine> {
    // Look for the marker substring; cheap pre-filter that lets us bail
    // on banner / blank lines without regex. `mark_idx` points at the
    // colon that separates the timestamp from "tracing_mark_write".
    let mark_idx = line.find(": tracing_mark_write: ")?;
    let head = line[..mark_idx].trim_end();
    let payload_field = &line[mark_idx + ": tracing_mark_write: ".len()..];

    // `head` looks like "<comm>-<tid>  ( <pid> ) [cpu] flags <ts_s>.<ts_us>".
    // The timestamp is the last whitespace-separated token; the first
    // is "<comm>-<tid>".
    let first_chunk = head.split_whitespace().next()?;
    // Last whitespace-separated token is the timestamp.
    let ts_token = head.split_whitespace().last()?;
    let ts_ns = parse_ts_to_ns(ts_token)?;

    let (comm, tid) = match first_chunk.rfind('-') {
        Some(idx) => {
            let comm = first_chunk[..idx].to_string();
            let tid = first_chunk[idx + 1..].parse::<u64>().ok()?;
            (comm, tid)
        }
        None => return None,
    };

    // Payload form: `<MARK>|<tid>|<rest>`. On OHOS `<rest>` may be:
    //   * `H:<name>|<lvl><id>`  for begin/instant markers (e.g.
    //     `H:Servo::new|M62`),
    //   * `<lvl><id>` alone for sync ends (e.g. `M62`, `I30`),
    //   * a custom string for counters (`C|...|N`).
    // We strip both the leading `H:` and any trailing `|<token>` so the
    // critical-path registry's exact-match names line up; level/id
    // metadata isn't part of servoperf's model.
    let mut parts = payload_field.splitn(3, '|');
    let kind = parts.next()?.trim();
    let _payload_tid = parts.next()?; // sometimes empty / sometimes pid
    let raw_rest = parts.next().unwrap_or("");
    let trimmed_rest = raw_rest.split('|').next().unwrap_or("");
    let name_or_level = trimmed_rest
        .strip_prefix("H:")
        .unwrap_or(trimmed_rest)
        .to_string();
    let marker = match kind {
        "B" => Marker::BeginSync,
        "E" => Marker::EndSync,
        "S" => Marker::BeginAsync,
        "F" => Marker::EndAsync,
        "I" => Marker::Instant,
        "C" => Marker::Counter,
        _ => return None,
    };
    // For sync-end markers, the residual payload is the level+id token
    // (`M62`, `I30`, …), never the begin's name. Force it empty so the
    // parser falls back to LIFO close-on-tid, which is the only correct
    // semantic for OHOS-style end markers.
    let payload = if matches!(marker, Marker::EndSync) {
        String::new()
    } else {
        name_or_level
    };
    Some(ParsedLine { comm, tid, ts_ns, marker, payload })
}

/// Convert a `<seconds>.<sub>` timestamp string to nanoseconds.
/// Hitrace's text emitter prints microseconds (6 digits); we accept any
/// decimal width and zero-pad to nanoseconds.
fn parse_ts_to_ns(s: &str) -> Option<u64> {
    let (sec, sub) = s.split_once('.')?;
    let sec: u64 = sec.parse().ok()?;
    // Right-pad/truncate the fractional part to 9 digits (nanoseconds).
    let mut ns_str = sub.to_string();
    if ns_str.len() < 9 {
        ns_str.push_str(&"0".repeat(9 - ns_str.len()));
    } else {
        ns_str.truncate(9);
    }
    let ns: u64 = ns_str.parse().ok()?;
    Some(sec.saturating_mul(1_000_000_000).saturating_add(ns))
}

/// Pick a sensible default capture window for the next iteration based
/// on prior successes. Mirrors `runner::pick_timeout`'s policy
/// (`max(20s, 10×median)`) but capped on the upper end to keep total
/// wall time bounded — a 5-minute capture isn't useful for FCP and burns
/// the device's hitrace ring buffer.
/// Pick a sensible default capture window for the next iteration based
/// on prior successes. Mirrors `runner::pick_timeout`'s policy
/// (`max(floor, 2×median)`) but capped on the upper end to keep total
/// wall time bounded — a 5-minute capture isn't useful for FCP and burns
/// the device's hitrace ring buffer.
#[allow(dead_code)]
pub fn pick_capture_seconds(successful_durations: &[Duration], floor_s: u64, ceil_s: u64) -> u64 {
    if successful_durations.is_empty() {
        return floor_s;
    }
    let mut sorted: Vec<Duration> = successful_durations.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    let bumped = median.saturating_mul(2).as_secs();
    bumped.clamp(floor_s, ceil_s)
}

// --- OHOS record driver -----------------------------------------------

/// Drives one WPR record pass against a HarmonyOS device. Implements
/// [`crate::fixtures::RecordDriver`] so `fixtures::spawn` can use it
/// in place of the desktop `LocalServoshellDriver`.
///
/// The drive() method:
///   1. Sets up `hdc rport tcp:<proxy_port> tcp:<proxy_port>` so the
///      device can reach the host's `wpr_tunnel`.
///   2. `aa force-stop` the bundle (cold start).
///   3. `aa start` the EntryAbility with `network_*_proxy_uri` prefs +
///      `--ignore-certificate-errors` (the args translator injects
///      these whenever `proxy_uri` is set).
///   4. Sleeps `record_seconds` so lazy-loaded resources (images,
///      async fetches) are captured. The default of 45 s is what we
///      empirically validated against `cdn-huaweimossel`; 15 s missed
///      the image tail and produced a 4.5 MB archive that replayed
///      without pictures, while 45 s produced 38 MB and replayed
///      cleanly.
///   5. `aa force-stop` so the device is in a known state for the
///      replay phase that follows.
///   6. RPortGuard drops → `hdc fport rm` cleans the forward.
///
/// The caller (`record_one_pass` in fixtures.rs) is responsible for
/// SIGINTing WPR after this returns, which flushes the archive.
pub struct OhosRecordDriver {
    pub target: OhosTarget,
    pub record_seconds: u64,
}

impl crate::fixtures::RecordDriver for OhosRecordDriver {
    fn drive(
        &self,
        workload: &Workload,
        handle: &crate::fixtures::FixtureHandle,
        _out_dir: &Path,
    ) -> Result<()> {
        let proxy_uri = handle.proxy_uri().ok_or_else(|| {
            anyhow::anyhow!(
                "OhosRecordDriver requires a fixture with a proxy URI \
                 (only `wpr-replay` provides one)"
            )
        })?;
        let port = parse_proxy_port(proxy_uri)?;

        let _rport = self
            .target
            .setup_rport(&[port])
            .with_context(|| format!("setting up hdc rport for record on tcp:{port}"))?;

        self.target.force_stop();
        self.target
            .aa_start(workload, Some(proxy_uri))
            .with_context(|| format!("aa start failed for {} during record", self.target.bundle))?;

        eprintln!(
            "ohos: recording WPR archive for {} s (override with --ohos-record-seconds)",
            self.record_seconds
        );
        std::thread::sleep(Duration::from_secs(self.record_seconds));

        self.target.force_stop();
        // _rport drops here, removing the forward.
        Ok(())
    }
}

/// Parse a `host:port`-shaped TCP port out of a proxy URI like
/// `http://127.0.0.1:4480`. Errors if the URI doesn't have an explicit
/// numeric port — without it we can't set up the right rport.
fn parse_proxy_port(uri: &str) -> Result<u16> {
    let after_scheme = uri.split_once("://").map(|(_, rest)| rest).unwrap_or(uri);
    let host_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    let port_str = host_port.rsplit(':').next().ok_or_else(|| {
        anyhow::anyhow!("proxy URI {uri:?} has no port — can't set up hdc rport")
    })?;
    port_str.parse::<u16>().with_context(|| {
        format!("proxy URI {uri:?} has a non-numeric port {port_str:?}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_begin_end_pair() {
        let txt = "\
   org.servo.servo-44962  ( 44682) [010] .... 17864.716645: tracing_mark_write: B|44682|Servo::new\n\
   org.servo.servo-44962  ( 44682) [010] .... 17864.716745: tracing_mark_write: E|44682|Servo::new\n\
";
        let slices = parse_hitrace_text(txt);
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].name, "Servo::new");
        assert_eq!(slices[0].thread, "org.servo.servo");
        assert_eq!(slices[0].ts_ns, 17_864_716_645_000);
        assert_eq!(slices[0].dur_ns, 100_000); // 100 µs
    }

    #[test]
    fn parses_end_with_empty_payload_pairs_with_topmost() {
        let txt = "\
   servoshell-100  ( 100) [000] .... 1.000000: tracing_mark_write: B|100|outer\n\
   servoshell-100  ( 100) [000] .... 1.000100: tracing_mark_write: B|100|inner\n\
   servoshell-100  ( 100) [000] .... 1.000200: tracing_mark_write: E|100|\n\
   servoshell-100  ( 100) [000] .... 1.000300: tracing_mark_write: E|100|\n\
";
        let mut slices = parse_hitrace_text(txt);
        slices.sort_by_key(|s| s.ts_ns);
        assert_eq!(slices.len(), 2);
        // After sort by start ts, outer is first (earlier begin).
        assert_eq!(slices[0].name, "outer");
        assert_eq!(slices[0].dur_ns, 300_000);
        assert_eq!(slices[1].name, "inner");
        assert_eq!(slices[1].dur_ns, 100_000);
    }

    #[test]
    fn parses_instant_event() {
        let txt = "   servo-1  ( 1) [000] .... 5.000000: tracing_mark_write: I|1|FirstContentfulPaint\n";
        let slices = parse_hitrace_text(txt);
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].name, "FirstContentfulPaint");
        assert_eq!(slices[0].dur_ns, 0);
    }

    #[test]
    fn skips_non_event_lines() {
        let txt = "# tracer: nop\n\
                   #\n\
                   bogus line without anything\n\
                   servo-1  ( 1) [0] .... 1.0: tracing_mark_write: B|1|x\n\
                   servo-1  ( 1) [0] .... 2.0: tracing_mark_write: E|1|x\n";
        let slices = parse_hitrace_text(txt);
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].name, "x");
    }

    #[test]
    fn parse_proxy_port_accepts_typical_urls() {
        assert_eq!(parse_proxy_port("http://127.0.0.1:4480").unwrap(), 4480);
        assert_eq!(parse_proxy_port("https://localhost:8443").unwrap(), 8443);
        // No scheme — still works (we only need the trailing :port).
        assert_eq!(parse_proxy_port("127.0.0.1:7000").unwrap(), 7000);
    }

    #[test]
    fn parse_proxy_port_rejects_missing_port() {
        // No ":port" → IPv4 string falls through to a non-numeric parse.
        assert!(parse_proxy_port("http://example.com/").is_err());
    }

    #[test]
    fn ts_parser_handles_microseconds_and_nanoseconds() {
        assert_eq!(parse_ts_to_ns("1.000000"), Some(1_000_000_000));
        assert_eq!(parse_ts_to_ns("1.000001"), Some(1_000_001_000));
        assert_eq!(parse_ts_to_ns("0.123456789"), Some(123_456_789));
        assert!(parse_ts_to_ns("not-a-time").is_none());
    }

    #[test]
    fn workload_args_translation() {
        use crate::workload::Workload;
        let w = Workload {
            name: "t".into(),
            url: "https://x/".into(),
            tracing_filter: "info".into(),
            iterations: 1,
            user_agent: Some("UA".into()),
            viewport: Some((800, 600)),
            device_pixel_ratio: Some(2.0),
            servoshell_args: vec![
                "--headless".into(), // dropped
                "--exit".into(),     // dropped
                "--pref=foo=bar".into(),
                "--ps=--passthrough".into(),
                "value-stays-with-it".into(),
                "--psn=--flag".into(),
            ],
            fixture: None,
        };
        let aa = workload_args_to_aa_params(&w, Some("http://127.0.0.1:9999"));
        // headless / exit dropped
        assert!(!aa.iter().any(|a| a == "--headless" || a == "--exit"));
        // viewport synthesized
        assert!(aa.iter().zip(aa.iter().skip(1)).any(|(k, v)| k == "--ps=--window-size" && v == "800x600"));
        // dpr synthesized
        assert!(aa.iter().zip(aa.iter().skip(1)).any(|(k, v)| k == "--ps=--device-pixel-ratio" && v == "2"));
        // user agent synthesized
        assert!(aa.iter().zip(aa.iter().skip(1)).any(|(k, v)| k == "--ps=--user-agent" && v == "UA"));
        // tracing_filter synthesized
        assert!(aa.iter().any(|a| a == "--ps=--tracing-filter"));
        // pref translated to --ps=--pref / value
        assert!(aa.iter().zip(aa.iter().skip(1)).any(|(k, v)| k == "--ps=--pref" && v == "foo=bar"));
        // already-OHOS-encoded passthrough
        assert!(aa.iter().any(|a| a == "--ps=--passthrough"));
        assert!(aa.iter().any(|a| a == "--psn=--flag"));
        // proxy injected
        assert!(aa.iter().any(|a| a.starts_with("--psn=--pref=network_https_proxy_uri=")));
        assert!(aa.iter().any(|a| a == "--psn=--ignore-certificate-errors"));
    }
}
