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
//!    `virt_start` for binary search.
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
    let mut totals: HashMap<String, u64> =
        engine.functions.iter().map(|t| (t.clone(), 0u64)).collect();

    if engine.symbol_file.is_empty() {
        // Caller's already warned about this; produce empty results so the
        // bench keeps running.
        return Ok(totals);
    }

    let sym_path = workloads_dir.join(&engine.symbol_file);
    let symbols = SymbolIndex::load(&sym_path)
        .with_context(|| format!("loading symbols from {}", sym_path.display()))?;
    // The DSO basename hiperf records under MMAP2's path field (e.g.
    // "libarkweb_engine.so" or "libservoshell.so"). Pre-compute the symbol
    // file's basename for the same comparison.
    let symbol_basename = sym_basename_from_path(&engine.symbol_file);

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

                        // The sample's IP is the leaf frame; the callchain is
                        // a list of caller IPs above it. We process the IP +
                        // every callchain entry as a single set of frames.
                        // Match each frame against the target list once.
                        let mut matched_for_this_sample: Vec<bool> =
                            vec![false; engine.functions.len()];

                        let mut process_ip =
                            |ip: u64,
                             symbols: &SymbolIndex,
                             mappings: &[Mapping],
                             matched: &mut [bool]| {
                                let Some(mapping) = find_mapping(mappings, ip) else {
                                    return;
                                };
                                if mapping.basename != symbol_basename {
                                    return;
                                }
                                let file_offset = ip - mapping.start + mapping.page_offset;
                                let Some(name) = symbols.lookup(file_offset) else {
                                    return;
                                };
                                for (i, t) in engine.functions.iter().enumerate() {
                                    if matched[i] {
                                        // Already credited this sample for this target —
                                        // matches the dedup semantics noted in module
                                        // docs (avoid runaway counts on recursion).
                                        continue;
                                    }
                                    if name.contains(t.as_str()) {
                                        if let Some(slot) = totals.get_mut(t) {
                                            *slot += period;
                                        }
                                        matched[i] = true;
                                        break;
                                    }
                                }
                            };

                        if let Some(ip) = s.ip {
                            process_ip(ip, &symbols, mappings, &mut matched_for_this_sample);
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
                                process_ip(ip, &symbols, mappings, &mut matched_for_this_sample);
                            }
                        }
                    }
                    _ => {}
                }
            }
            PerfFileRecord::UserRecord(_) => {}
        }
    }

    Ok(totals)
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
