//! Host-side aggregation of inclusive instruction counts from `perf.data`.
//!
//! Replaces the previous on-device `hiperf report -s` + text-parse round-trip
//! with a direct Rust parser. Two wins:
//!
//! - **Throughput.** A capture's `perf.data` can be pulled to the host as
//!   soon as `hiperf record` finishes, so the analyser can run while the
//!   next iteration is recording (see the background-thread plumbing in
//!   [`crate::cmd::bench`]).
//! - **Flexibility.** Per-sample timestamps land in [`crate::trace`]'s
//!   monotonic clock, so a future patch can slice the same data by hitrace
//!   milestone windows (e.g. instructions between PageBegin and FCP) without
//!   reshaping the inputs.
//!
//! Method:
//!
//! 1. Read the unstripped engine library (built once by
//!    `servoperf prepare-arkweb-symbols` for ArkWeb, or staged from a Servo
//!    `target/.../release/libservoshell.so` whose BuildID matches the
//!    installed bundle). Iterate `.symtab`, keep `STT_FUNC` symbols, store
//!    each as a `(virt_start, virt_end, demangled_name)` triple sorted by
//!    `virt_start` for binary search. When the library also carries
//!    `.debug_info`, an addr2line context is built alongside it so an address
//!    resolves to the whole inline chain rather than to whichever function
//!    the compiler folded the code into — without it an inlined boundary
//!    reports **0**, which reads as "this phase is free" rather than "this
//!    measurement is blind". Build the library with `debug = 1`: that is the
//!    cheapest level carrying `DW_AT_linkage_name`, so inline frames demangle
//!    to the same `<Type>::method` form the symbol-table path produces and
//!    existing patterns keep matching.
//! 2. Stream the perf.data once, accumulating two pieces of state:
//!    - per-PID interval list of `MMAP2` mappings (address range +
//!      page-offset + path basename) so an IP can be resolved to a
//!      `(dso, file_offset)` pair;
//!    - per-sample aggregation: walk the callchain, look each frame up in
//!      the matching DSO's symbol index, and credit the sample's
//!      `period` to every target substring matched. A target may match
//!      multiple frames in one callchain — same semantics as the legacy
//!      tree-walk parser, so existing baselines stay comparable.
//!
//! The matching semantics deliberately mirror
//! [`crate::instructions::parse::aggregate_inclusive`]: case-sensitive
//! substring, first-match-wins on a per-frame basis. Numbers should agree
//! within hiperf's sample quantisation.

use anyhow::{Context, Result};
use linux_perf_data::{
    linux_perf_event_reader::{CpuMode, EventRecord},
    PerfFileReader, PerfFileRecord,
};
use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use crate::instructions::EngineConfig;

/// Aggregate per-target inclusive hw-instruction counts from one
/// `perf.data` produced by `hiperf record -a -e hw-instructions`.
///
/// `engine` selects which library to symbolicate; only frames inside that
/// library are matched against the engine's target substrings — frames in
/// libc, the kernel, etc. are ignored. `workloads_dir` is where the
/// unstripped `.so` named by `engine.symbol_file` lives.
///
/// Returns a map covering every engine target (with `0` for unmatched
/// targets) so downstream serialisation has a stable shape.
pub fn aggregate_inclusive_from_perf_data(
    perf_data_path: &Path,
    engine: &EngineConfig,
    workloads_dir: &Path,
) -> Result<HashMap<String, u64>> {
    let mut totals: HashMap<String, u64> = engine
        .functions
        .iter()
        .map(|t| (t.clone(), 0u64))
        .chain(engine.groups.iter().map(|g| (g.name.clone(), 0u64)))
        .collect();

    if engine.symbol_file.is_empty() {
        // Caller's already warned about this; produce empty results so the
        // bench keeps running.
        return Ok(totals);
    }

    let sym_path = workloads_dir.join(&engine.symbol_file);
    let symbolizer = Symbolizer::load(&sym_path)
        .with_context(|| format!("loading symbols from {}", sym_path.display()))?;
    // The DSO basename hiperf records under MMAP2's path field (e.g.
    // "libarkweb_engine.so" or "libservoshell.so"). Pre-compute the symbol
    // file's basename for the same comparison.
    let symbol_basename = sym_basename_from_path(&engine.symbol_file);
    if symbolizer.has_inline_info() {
        eprintln!(
            "instructions: {} carries DWARF; inlined functions are attributed",
            sym_path.display()
        );
    }
    // Reused across samples so the hot loop allocates nothing.
    let mut cache = MatchCache::new();
    let mut scratch_names: Vec<String> = Vec::new();
    let mut scratch_match = AddressMatch::default();
    let mut frame_ips: Vec<u64> = Vec::with_capacity(64);

    let file = File::open(perf_data_path)
        .with_context(|| format!("opening {}", perf_data_path.display()))?;
    let reader = BufReader::new(file);
    let PerfFileReader {
        mut perf_file,
        mut record_iter,
    } = PerfFileReader::parse_file(reader)
        .with_context(|| format!("parsing perf.data at {}", perf_data_path.display()))?;

    // Per-pid mmap intervals. We don't bother with a tree — a small Vec
    // per pid is fine: perf records hundreds of mmaps for a typical
    // capture, and binary search on the Vec is ~O(log n) per lookup.
    let mut mmaps_by_pid: HashMap<i32, Vec<Mapping>> = HashMap::new();

    while let Some(record) = record_iter
        .next_record(&mut perf_file)
        .context("reading next perf record")?
    {
        match record {
            PerfFileRecord::EventRecord { record, .. } => {
                let parsed = record.parse().context("parsing event record")?;
                match parsed {
                    EventRecord::Mmap2(m) => {
                        // Only user-space text mappings are useful for IP→function
                        // resolution. The protection bits are at PROT_* offsets,
                        // bit 0x4 = PROT_EXEC.
                        if matches!(m.cpu_mode, CpuMode::User) && (m.protection & 0x4) != 0 {
                            let path = m.path.as_slice();
                            let basename = basename_of(&path);
                            mmaps_by_pid.entry(m.pid).or_default().push(Mapping {
                                start: m.address,
                                end: m.address.saturating_add(m.length),
                                page_offset: m.page_offset,
                                basename: basename.into_owned(),
                            });
                        }
                    }
                    EventRecord::Mmap(m) => {
                        if matches!(m.cpu_mode, CpuMode::User) && m.is_executable {
                            let path = m.path.as_slice();
                            let basename = basename_of(&path);
                            mmaps_by_pid.entry(m.pid).or_default().push(Mapping {
                                start: m.address,
                                end: m.address.saturating_add(m.length),
                                page_offset: m.page_offset,
                                basename: basename.into_owned(),
                            });
                        }
                    }
                    EventRecord::Sample(s) => {
                        let Some(pid) = s.pid else { continue };
                        let Some(period) = s.period else { continue };
                        let Some(mappings) = mmaps_by_pid.get(&pid) else {
                            continue;
                        };

                        // The sample's IP is the leaf frame; the callchain
                        // is a list of caller IPs above it. Every frame — and,
                        // where DWARF is available, every function inlined
                        // into it — is matched against the target list, each
                        // target credited at most once per sample so recursion
                        // cannot run the count away.
                        let mut matched_for_this_sample: Vec<bool> =
                            vec![false; engine.functions.len()];
                        // Groups are credited independently of the per-symbol
                        // targets, and once per sample: a chain containing two
                        // members of the same group counts once, so nesting
                        // does not double-count.
                        let mut matched_groups: Vec<bool> = vec![false; engine.groups.len()];

                        frame_ips.clear();
                        if let Some(ip) = s.ip {
                            frame_ips.push(ip);
                        }
                        if let Some(chain) = s.callchain {
                            for idx in 0..chain.len() {
                                let Some(ip) = chain.get(idx) else { break };
                                // Skip the synthetic context markers
                                // (PERF_CONTEXT_USER, PERF_CONTEXT_KERNEL,
                                // etc — defined as ~0u8 .. ~31u8 in the
                                // kernel headers).
                                if ip >= 0xffff_ffff_ffff_ff00 {
                                    continue;
                                }
                                frame_ips.push(ip);
                            }
                        }

                        for &ip in frame_ips.iter() {
                            let Some(mapping) = find_mapping(mappings, ip) else {
                                continue;
                            };
                            if mapping.basename != symbol_basename {
                                continue;
                            }
                            let file_offset = ip - mapping.start + mapping.page_offset;
                            let hit = cache.get(
                                file_offset,
                                &symbolizer,
                                engine,
                                &mut scratch_names,
                                &mut scratch_match,
                            );
                            for &i in &hit.funcs {
                                let i = i as usize;
                                if matched_for_this_sample[i] {
                                    continue;
                                }
                                if let Some(slot) = totals.get_mut(&engine.functions[i]) {
                                    *slot += period;
                                }
                                matched_for_this_sample[i] = true;
                            }
                            for &g in &hit.groups {
                                let g = g as usize;
                                if matched_groups[g] {
                                    continue;
                                }
                                if let Some(slot) = totals.get_mut(&engine.groups[g].name) {
                                    *slot += period;
                                }
                                matched_groups[g] = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            PerfFileRecord::UserRecord(_) => {}
        }
    }

    explain_zeros(&totals, &symbolizer, engine, &sym_path);
    Ok(totals)
}

/// Say why a configured target came back at zero.
///
/// A zero reads as "this phase is free" rather than "this measurement did not
/// find anything", and the two are indistinguishable in the output. The
/// symbol table can tell them apart.
fn explain_zeros(
    totals: &HashMap<String, u64>,
    symbolizer: &Symbolizer,
    engine: &EngineConfig,
    sym_path: &Path,
) {
    let mut unmatched: Vec<String> = Vec::new();
    let mut cold: Vec<String> = Vec::new();

    let mut classify = |label: &str, patterns: &[String]| {
        if totals.get(label).copied().unwrap_or(0) != 0 {
            return;
        }
        let missing: Vec<&str> = patterns
            .iter()
            .filter(|p| !symbolizer.symtab.matches_any(p))
            .map(|p| p.as_str())
            .collect();
        if missing.len() == patterns.len() {
            unmatched.push(format!("`{label}` ({})", missing.join("`, `")));
        } else {
            cold.push(label.to_string());
        }
    };

    for f in &engine.functions {
        classify(f, std::slice::from_ref(f));
    }
    for g in &engine.groups {
        classify(&g.name, &g.functions);
    }

    if !unmatched.is_empty() {
        eprintln!(
            "warning: these targets matched no symbol in {}, so their zero means \
             'not found', not 'not run': {}. rustc renders an inherent method as \
             `<Type>::method`, so the `>` is part of the name \
             (`LayoutThread>::handle_reflow`, not `LayoutThread::handle_reflow`). Check one \
             with `nm -C {} | grep <pattern>`.{}",
            sym_path.display(),
            unmatched.join(", "),
            sym_path.display(),
            if symbolizer.has_inline_info() {
                ""
            } else {
                " This library carries no DWARF, so an inlined function has no symbol \
                 of its own and can only report zero; build it with `debug = 1`."
            }
        );
    }
    if !cold.is_empty() {
        eprintln!(
            "note: {} resolved to a symbol but no sample landed in it — either the phase \
             did not run, or the capture missed it.",
            cold.join(", ")
        );
    }
}

/// Collapse `>::` to `::` so a pattern matches under either name-mangling
/// scheme.
///
/// rustc demangles an *inherent* method as `Type::method` under the legacy
/// scheme and `<Type>::method` under v0, and the two forms are mutually
/// exclusive as substrings — a pattern written against one silently reports
/// zero against the other. Trait-impl methods (`<A as B>::m`) render the same
/// either way and keep matching, since the raw form is tried too.
fn normalize_symbol(name: &str) -> String {
    name.replace(">::", "::")
}

/// Does `name` contain `pattern`, under either mangling?
fn name_matches(name: &str, pattern: &str, normalized_pattern: &str) -> bool {
    name.contains(pattern) || normalize_symbol(name).contains(normalized_pattern)
}

/// Pull the basename out of an mmap path. The path field is null-terminated
/// in the wire format but we work with the raw byte slice — easier than
/// hauling around a string.
fn basename_of(path: &[u8]) -> std::borrow::Cow<'_, str> {
    let s = std::str::from_utf8(path).unwrap_or("");
    let trimmed = s.trim_end_matches('\0');
    let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
    std::borrow::Cow::Owned(base.to_string())
}

fn sym_basename_from_path(symbol_file: &str) -> String {
    // Strip the optional `.merged.so` / `.symbols.so` suffix added by the
    // prepare-symbols subcommands so we compare against the actual on-device
    // basename (libarkweb_engine.so / libservoshell.so).
    let base = Path::new(symbol_file)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(symbol_file)
        .to_string();
    base.replace(".merged.so", ".so")
        .replace(".symbols.so", ".so")
}

#[derive(Debug, Clone)]
struct Mapping {
    start: u64,
    end: u64,
    page_offset: u64,
    basename: String,
}

/// Locate the executable mapping containing `ip`. `mappings` isn't kept
/// sorted (perf can emit overlapping ranges for the same file); linear scan
/// keeps the code simple and is fine at the scale of mappings per process
/// we see in practice (~200).
fn find_mapping(mappings: &[Mapping], ip: u64) -> Option<&Mapping> {
    // Walk in reverse so the most-recent mapping for an overlapping range
    // wins, matching how the kernel resolves addresses.
    mappings.iter().rev().find(|m| ip >= m.start && ip < m.end)
}

/// In-memory symbol table for one shared library, built from `.symtab`.
///
/// We keep only `STT_FUNC` entries with non-zero size. The lookup key is the
/// **file offset** (which, for the position-independent shared libraries we
/// profile, equals the `.symtab` `st_value` of the symbol). The stored name
/// is best-effort demangled — Rust v0/legacy first, then C++ Itanium —
/// falling back to the raw mangled string when demangling fails.
struct SymbolIndex {
    /// Sorted by `start`; lookups are binary search.
    entries: Vec<SymbolEntry>,
}

struct SymbolEntry {
    start: u64,
    end: u64,
    name: String,
}

impl SymbolIndex {
    /// Does any symbol's demangled name contain `pattern`?
    ///
    /// Separates the two reasons a target reports zero: a pattern that matches
    /// no symbol at all (usually a typo, or the `<Type>::method` form rustc
    /// renders inherent methods in), and a symbol that exists but never had a
    /// sample land in it.
    fn matches_any(&self, pattern: &str) -> bool {
        let normalized = normalize_symbol(pattern);
        self.entries
            .iter()
            .any(|e| name_matches(&e.name, pattern, &normalized))
    }

    /// Build a file-offset → demangled-name lookup table.
    ///
    /// On most shared libraries the executable section's `vaddr` differs
    /// from its `file_offset` by a small bias (e.g. 0x1000 on the ArkWeb
    /// `libarkweb_engine.so`). The `.symtab` reports symbols by `st_value`
    /// (vaddr), but per-sample IP→`file_offset` translation can only
    /// produce a file offset. We resolve the mismatch by walking the
    /// section table once and rewriting every symbol's bounds in
    /// file-offset space, so lookups at runtime are a single binary
    /// search.
    fn load(elf_path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(elf_path).with_context(|| format!("reading {}", elf_path.display()))?;
        let elf = object::File::parse(bytes.as_slice())
            .with_context(|| format!("parsing ELF {}", elf_path.display()))?;

        // Build a vaddr-range → file-offset-delta map, one entry per
        // allocated section that backs file content. NOBITS sections
        // (.bss etc) have no file backing so we skip them.
        let mut section_map: Vec<(u64, u64, i64)> = Vec::new(); // (vaddr_start, vaddr_end, file_off - vaddr)
        for section in elf.sections() {
            let Some((file_off, file_size)) = section.file_range() else {
                continue;
            };
            if file_size == 0 {
                continue;
            }
            let vaddr = section.address();
            section_map.push((vaddr, vaddr + file_size, file_off as i64 - vaddr as i64));
        }
        section_map.sort_by_key(|s| s.0);

        let mut entries: Vec<SymbolEntry> = Vec::new();
        for sym in elf.symbols() {
            if sym.kind() != SymbolKind::Text {
                continue;
            }
            let vaddr = sym.address();
            let size = sym.size();
            if size == 0 {
                continue;
            }
            let raw = match sym.name() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if raw.is_empty() {
                continue;
            }
            // Find the section containing this symbol's vaddr; skip
            // symbols outside any section (synthetic markers etc.).
            let idx = section_map.partition_point(|s| s.0 <= vaddr);
            if idx == 0 {
                continue;
            }
            let (sec_vstart, sec_vend, delta) = section_map[idx - 1];
            if vaddr + size > sec_vend {
                // Symbol crosses a section boundary — too unusual to
                // attempt translation; drop it.
                continue;
            }
            let _ = sec_vstart;
            let file_start = (vaddr as i64 + delta) as u64;
            entries.push(SymbolEntry {
                start: file_start,
                end: file_start + size,
                name: demangle_name(raw),
            });
        }
        entries.sort_by_key(|e| e.start);
        Ok(SymbolIndex { entries })
    }

    /// File-offset → vaddr map, the inverse of the rewrite [`Self::load`]
    /// performs. DWARF is addressed in vaddr space, so an inline lookup has
    /// to undo the translation the symtab path bakes in.
    fn offset_to_vaddr_map(elf: &object::File) -> Vec<(u64, u64, i64)> {
        let mut map: Vec<(u64, u64, i64)> = Vec::new();
        for section in elf.sections() {
            let Some((file_off, file_size)) = section.file_range() else {
                continue;
            };
            if file_size == 0 {
                continue;
            }
            let vaddr = section.address();
            if vaddr == 0 {
                // Not mapped at runtime (debug sections); no IP lands here.
                continue;
            }
            map.push((
                file_off,
                file_off + file_size,
                vaddr as i64 - file_off as i64,
            ));
        }
        map.sort_by_key(|s| s.0);
        map
    }

    /// Return the demangled name covering `offset`, or `None` when no
    /// symbol covers it (function body in a section with no symbol, or
    /// an address outside the library's text).
    fn lookup(&self, offset: u64) -> Option<&str> {
        // Largest start ≤ offset. partition_point returns insertion index
        // for a value strictly greater; subtract 1 to get the candidate.
        let idx = self.entries.partition_point(|e| e.start <= offset);
        if idx == 0 {
            return None;
        }
        let entry = &self.entries[idx - 1];
        if offset < entry.end {
            Some(entry.name.as_str())
        } else {
            None
        }
    }
}

/// Best-effort demangle: try Rust v0/legacy first (cheap; idempotent on
/// non-Rust names), then C++ Itanium, finally pass the raw name through.
/// Result is always owned so the caller doesn't have to track lifetimes
/// against the raw symbol slice.
fn demangle_name(raw: &str) -> String {
    // rustc_demangle::try_demangle is fallible — if the name isn't a Rust
    // mangled symbol, fall through.
    if let Ok(demangled) = rustc_demangle::try_demangle(raw) {
        return format!("{:#}", demangled);
    }
    if let Ok(sym) = cpp_demangle::Symbol::new(raw) {
        let options = cpp_demangle::DemangleOptions::default().no_return_type();
        if let Ok(s) = sym.demangle(&options) {
            return s;
        }
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    /// The same pattern has to work whichever mangling the build used: rustc
    /// renders an inherent method as `Type::method` (legacy) or
    /// `<Type>::method` (v0), and the two are mutually exclusive as
    /// substrings.
    #[test]
    fn patterns_match_under_either_mangling() {
        let v0 = "<script::dom::window::window::Window>::reflow";
        let legacy = "script::dom::window::window::Window::reflow";
        for pattern in ["Window>::reflow", "Window::reflow"] {
            let n = super::normalize_symbol(pattern);
            assert!(super::name_matches(v0, pattern, &n), "v0 vs {pattern}");
            assert!(
                super::name_matches(legacy, pattern, &n),
                "legacy vs {pattern}"
            );
        }
        // A trait impl renders the same either way and must still match.
        let trait_impl = "<layout::layout_impl::LayoutThread as layout_api::Layout>::reflow";
        let n = super::normalize_symbol("Layout>::reflow");
        assert!(super::name_matches(trait_impl, "Layout>::reflow", &n));
        // And an unrelated symbol still must not.
        let n = super::normalize_symbol("Window::reflow");
        assert!(!super::name_matches(
            "<script::dom::window::window::Window>::handle_pending_images",
            "Window::reflow",
            &n
        ));
    }

    #[test]
    fn symbol_presence_separates_a_typo_from_a_cold_symbol() {
        let idx = super::SymbolIndex {
            entries: vec![super::SymbolEntry {
                start: 0,
                end: 16,
                name: "<layout::layout_impl::LayoutThread>::handle_reflow".into(),
            }],
        };
        // Either mangling of the same method resolves.
        assert!(idx.matches_any("LayoutThread>::handle_reflow"));
        assert!(idx.matches_any("LayoutThread::handle_reflow"));
        // A pattern for something that is not there does not, which is what
        // lets a zero be reported as "not found" rather than "not run".
        assert!(!idx.matches_any("LayoutThread>::build_display_list"));
    }

    /// The test binary itself is built with debug info, so it doubles as a
    /// DWARF fixture: some address in it must expand to more than one frame,
    /// or inline attribution is silently not working.
    #[test]
    fn dwarf_symbolizer_expands_inline_frames() {
        let exe = std::env::current_exe().unwrap();
        let sym = match super::Symbolizer::load(&exe) {
            Ok(s) => s,
            Err(_) => return, // stripped build; nothing to assert
        };
        if !sym.has_inline_info() {
            return;
        }
        let len = std::fs::metadata(&exe).unwrap().len();
        let mut names = Vec::new();
        let mut deepest = 0usize;
        let mut single = 0usize;
        let step = (len / 20_000).max(4);
        for off in (0..len).step_by(step as usize) {
            sym.names_at(off, &mut names);
            deepest = deepest.max(names.len());
            if names.len() == 1 {
                single += 1;
            }
            if deepest >= 2 && single > 0 {
                break;
            }
        }
        assert!(
            deepest >= 2,
            "no address expanded to an inline chain — inline attribution is not working"
        );
        assert!(
            single > 0,
            "expected some addresses to resolve to one frame"
        );
    }

    #[test]
    fn file_offsets_map_back_to_vaddrs() {
        // .text at file 0x1000 mapped to vaddr 0x2000, plus a section mapped 1:1.
        let map = [(0x1000u64, 0x2000u64, 0x1000i64), (0x9000, 0xa000, 0)];
        assert_eq!(super::vaddr_for(&map, 0x1000), Some(0x2000));
        assert_eq!(super::vaddr_for(&map, 0x1fff), Some(0x2fff));
        assert_eq!(super::vaddr_for(&map, 0x9500), Some(0x9500));
        // Before the first section, between two, and past the end: unmapped.
        assert_eq!(super::vaddr_for(&map, 0x0100), None);
        assert_eq!(super::vaddr_for(&map, 0x5000), None);
        assert_eq!(super::vaddr_for(&map, 0xffff), None);
    }

    use super::*;

    #[test]
    fn basename_strips_path_and_nul() {
        let got = basename_of(b"/data/storage/el1/bundle/libs/arm64/libfoo.so\0");
        assert_eq!(&*got, "libfoo.so");
    }

    #[test]
    fn sym_basename_strips_merged_suffix() {
        assert_eq!(
            sym_basename_from_path("libarkweb_engine.merged.so"),
            "libarkweb_engine.so"
        );
        assert_eq!(
            sym_basename_from_path("libservoshell.symbols.so"),
            "libservoshell.so"
        );
        assert_eq!(
            sym_basename_from_path("libcompletely.unique.so"),
            "libcompletely.unique.so"
        );
    }

    #[test]
    fn demangle_handles_rust_v0_and_cpp_itanium() {
        // Rust v0 mangling for `<core::ptr::Unique<T> as core::fmt::Debug>::fmt`.
        // rustc_demangle should give us a readable form.
        let demangled = demangle_name("_ZN4core3fmt9Formatter9write_str17h0123456789abcdefE");
        assert!(
            demangled.contains("core::fmt::Formatter::write_str"),
            "rustc legacy demangle failed: {demangled}"
        );

        // C++ Itanium: ::std::vector<int>::push_back(int&&).
        let demangled = demangle_name("_ZNSt6vectorIiSaIiEE9push_backEOi");
        assert!(
            demangled.contains("std::vector") && demangled.contains("push_back"),
            "cpp itanium demangle failed: {demangled}"
        );

        // Unrecognised → pass through.
        assert_eq!(demangle_name("plain_c_func"), "plain_c_func");
    }

    #[test]
    fn symbol_index_binary_search_in_range() {
        let idx = SymbolIndex {
            entries: vec![
                SymbolEntry {
                    start: 0x1000,
                    end: 0x1100,
                    name: "low".into(),
                },
                SymbolEntry {
                    start: 0x1200,
                    end: 0x1300,
                    name: "mid".into(),
                },
                SymbolEntry {
                    start: 0x1400,
                    end: 0x1500,
                    name: "high".into(),
                },
            ],
        };
        // In range.
        assert_eq!(idx.lookup(0x1050), Some("low"));
        assert_eq!(idx.lookup(0x12ff), Some("mid"));
        // At exact start.
        assert_eq!(idx.lookup(0x1400), Some("high"));
        // In gap.
        assert_eq!(idx.lookup(0x1150), None);
        // Below first.
        assert_eq!(idx.lookup(0x0fff), None);
        // Above last.
        assert_eq!(idx.lookup(0x1500), None);
    }
}

/// Resolves a file offset in the engine library to the names of every
/// function active at that address — the containing function plus, when the
/// library carries DWARF, the chain of functions inlined into it.
///
/// Without inline information an inlined boundary resolves to whichever
/// function it was folded into, so its own metric reports **0**. A zero reads
/// as "this phase is free" rather than "this measurement is blind", which is
/// the failure this type exists to remove.
struct Symbolizer {
    symtab: SymbolIndex,
    dwarf: Option<DwarfIndex>,
}

struct DwarfIndex {
    ctx: addr2line::Context<gimli::EndianArcSlice<gimli::RunTimeEndian>>,
    /// `(file_off_start, file_off_end, vaddr - file_off)`, sorted by start.
    offset_to_vaddr: Vec<(u64, u64, i64)>,
}

impl Symbolizer {
    fn load(elf_path: &Path) -> Result<Self> {
        let symtab = SymbolIndex::load(elf_path)?;
        let bytes =
            std::fs::read(elf_path).with_context(|| format!("reading {}", elf_path.display()))?;
        let elf = object::File::parse(bytes.as_slice())
            .with_context(|| format!("parsing ELF {}", elf_path.display()))?;
        let has_debug_info = elf.section_by_name(".debug_info").is_some();
        let dwarf = if has_debug_info {
            match load_dwarf_context(&elf) {
                Ok(ctx) => Some(DwarfIndex {
                    ctx,
                    offset_to_vaddr: SymbolIndex::offset_to_vaddr_map(&elf),
                }),
                Err(e) => {
                    eprintln!(
                        "warning: {} has .debug_info but it could not be read ({e}); \
                         falling back to symbol-table lookups, so inlined functions \
                         will report 0",
                        elf_path.display()
                    );
                    None
                }
            }
        } else {
            None
        };
        Ok(Symbolizer { symtab, dwarf })
    }

    /// True when inline expansion is available.
    fn has_inline_info(&self) -> bool {
        self.dwarf.is_some()
    }

    /// Append every function name active at `file_offset` to `out`,
    /// innermost inlined frame first. Falls back to the symbol table when
    /// DWARF is absent or covers no frame at this address (the two disagree
    /// at, for instance, PLT stubs).
    fn names_at(&self, file_offset: u64, out: &mut Vec<String>) {
        out.clear();
        if let Some(d) = &self.dwarf {
            if let Some(vaddr) = d.vaddr_for(file_offset) {
                if let Ok(mut frames) = d.ctx.find_frames(vaddr).skip_all_loads() {
                    while let Ok(Some(frame)) = frames.next() {
                        if let Some(f) = frame.function {
                            if let Ok(name) = f.demangle() {
                                out.push(name.into_owned());
                            }
                        }
                    }
                }
            }
        }
        if out.is_empty() {
            if let Some(name) = self.symtab.lookup(file_offset) {
                out.push(name.to_string());
            }
        }
    }
}

/// Build an addr2line context over owning readers, so it outlives the parsed
/// `object::File` the section bytes came from.
fn load_dwarf_context(
    elf: &object::File,
) -> Result<addr2line::Context<gimli::EndianArcSlice<gimli::RunTimeEndian>>> {
    let endian = if elf.is_little_endian() {
        gimli::RunTimeEndian::Little
    } else {
        gimli::RunTimeEndian::Big
    };
    let load = |id: gimli::SectionId| -> Result<gimli::EndianArcSlice<gimli::RunTimeEndian>> {
        let data = match elf.section_by_name(id.name()) {
            Some(section) => section.uncompressed_data()?.into_owned(),
            None => Vec::new(),
        };
        Ok(gimli::EndianArcSlice::new(data.into(), endian))
    };
    let dwarf = gimli::Dwarf::load(load)?;
    Ok(addr2line::Context::from_dwarf(dwarf)?)
}

impl DwarfIndex {
    fn vaddr_for(&self, file_offset: u64) -> Option<u64> {
        vaddr_for(&self.offset_to_vaddr, file_offset)
    }
}

/// Translate a file offset into the vaddr DWARF is addressed by, using the
/// `(start, end, vaddr - offset)` map built from the section headers.
/// `None` for an offset in no mapped section — a debug section, or padding.
fn vaddr_for(map: &[(u64, u64, i64)], file_offset: u64) -> Option<u64> {
    let idx = map.partition_point(|s| s.0 <= file_offset);
    if idx == 0 {
        return None;
    }
    let (_, end, delta) = map[idx - 1];
    if file_offset >= end {
        return None;
    }
    Some((file_offset as i64 + delta) as u64)
}

/// Which configured patterns an address matches, cached per address.
///
/// Resolving a DWARF inline chain is orders of magnitude slower than a binary
/// search over a symbol table, and a capture repeats the same hot addresses
/// across hundreds of thousands of samples — so the cache is what makes the
/// inline path affordable rather than an optimisation.
#[derive(Default, Clone)]
struct AddressMatch {
    /// Indices into `engine.functions`, deduplicated, in frame order.
    funcs: Vec<u16>,
    /// Indices into `engine.groups`, deduplicated.
    groups: Vec<u16>,
}

/// Bytes an [`AddressMatch`] entry costs in the map, near enough for
/// budgeting: 8 for the key, two `Vec` headers, and slot overhead. Entries
/// that actually match allocate on top, but those are a small minority.
const CACHE_ENTRY_BYTES: usize = 96;

/// Cap on the address cache. Generous, because overshooting costs the host
/// RSS while a too-small cache costs analysis time on every iteration.
const MAX_CACHE_BYTES: usize = 1 << 30;

struct MatchCache {
    map: rustc_hash::FxHashMap<u64, AddressMatch>,
    max_entries: usize,
    /// Set once the cap is hit, so the warning is printed at most once.
    warned: bool,
}

impl MatchCache {
    fn new() -> Self {
        MatchCache {
            map: rustc_hash::FxHashMap::default(),
            max_entries: MAX_CACHE_BYTES / CACHE_ENTRY_BYTES,
            warned: false,
        }
    }

    /// Resolve `file_offset` against the engine's patterns, memoised.
    ///
    /// At the cap the map is emptied and refilled rather than evicted
    /// entry-by-entry: an LRU would cost more per lookup than it saves, and
    /// the working set of hot addresses re-warms in a few thousand samples.
    /// Results are unaffected either way — only speed is.
    fn get<'a>(
        &'a mut self,
        file_offset: u64,
        symbolizer: &Symbolizer,
        engine: &EngineConfig,
        scratch_names: &mut Vec<String>,
        scratch_match: &mut AddressMatch,
    ) -> &'a AddressMatch {
        if !self.map.contains_key(&file_offset) {
            symbolizer.names_at(file_offset, scratch_names);
            scratch_match.funcs.clear();
            scratch_match.groups.clear();
            for name in scratch_names.iter() {
                let normalized_name = normalize_symbol(name);
                // One frame credits at most one function pattern — the first
                // that matches — mirroring the symbol-table path.
                for (i, t) in engine.functions.iter().enumerate() {
                    if name.contains(t.as_str()) || normalized_name.contains(&normalize_symbol(t)) {
                        let i = i as u16;
                        if !scratch_match.funcs.contains(&i) {
                            scratch_match.funcs.push(i);
                        }
                        break;
                    }
                }
                for (g, group) in engine.groups.iter().enumerate() {
                    let g = g as u16;
                    if scratch_match.groups.contains(&g) {
                        continue;
                    }
                    if group.functions.iter().any(|f| {
                        name.contains(f.as_str()) || normalized_name.contains(&normalize_symbol(f))
                    }) {
                        scratch_match.groups.push(g);
                    }
                }
            }
            if self.map.len() < self.max_entries {
                self.map.insert(file_offset, scratch_match.clone());
            } else {
                if !self.warned {
                    eprintln!(
                        "warning: symbolization cache hit its {} MiB cap and was reset; \
                         analysis will be slower",
                        MAX_CACHE_BYTES / (1 << 20)
                    );
                    self.warned = true;
                }
                self.map.clear();
                self.map.insert(file_offset, scratch_match.clone());
            }
        }
        &self.map[&file_offset]
    }
}
