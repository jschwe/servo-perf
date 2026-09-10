//! Produce an unstripped ELF that `hiperf --symbol-dir` can use for ArkWeb.
//!
//! The on-device `libarkweb_engine.so` keeps its full symbol table in a
//! `.gnu_debugdata` section: an XZ-compressed stripped-down ELF whose only
//! useful contents are its `.symtab`/`.strtab`. Hiperf does **not** unpack
//! that section automatically, and `--json` output ignores `--symbol-dir`
//! entirely — both confirmed during the manual investigation.
//!
//! The recipe ported here:
//!   1. Read the on-device `.so`. Locate the `.gnu_debugdata` section.
//!   2. XZ-decompress it (pure Rust via `lzma-rs`) → minidebug ELF bytes.
//!   3. From the minidebug ELF, dump the `.symtab` and `.strtab` section
//!      data verbatim.
//!   4. Build an output ELF: original file bytes verbatim, followed by
//!      newly-appended `.symtab` / `.strtab` / `.shstrtab` data, followed
//!      by a freshly-rewritten section header table. The ELF header is
//!      patched to point at the new SHT.
//!   5. The new SHT links `.symtab` → `.strtab` (`sh_link`), sets
//!      `sh_entsize = 24` (ELF64 Sym size), and sets `sh_info` to the
//!      symbol count (every symbol in the minidebug-derived table has
//!      `STB_LOCAL` binding, so this is also the local-count, satisfying
//!      hiperf's "one past last local" expectation).
//!
//! This matches the byte-for-byte output that the previous shell recipe
//! (`llvm-objcopy --add-section … && /tmp/fix_symtab.py`) produced, modulo
//! that we don't need to set `--set-section-type` because we write the
//! section headers ourselves.

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::io::Cursor;
use std::path::Path;

/// ELF section type constants we care about.
const SHT_PROGBITS: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;

/// ELF64 Sym entry size — sh_entsize for a SYMTAB section header.
const ELF64_SYM_SIZE: u64 = 24;

/// Read an entire ELF file, locate the named section, return its raw bytes.
///
/// Errors out if the file isn't a 64-bit little-endian ELF, or the section
/// doesn't exist. Section name comparison is exact (`==`).
pub fn read_section_bytes(elf_path: &Path, name: &str) -> Result<Vec<u8>> {
    let bytes =
        fs::read(elf_path).with_context(|| format!("reading ELF {}", elf_path.display()))?;
    let (_hdr, sections, shstrtab) = parse_section_table(&bytes)
        .with_context(|| format!("parsing ELF section table from {}", elf_path.display()))?;
    for s in &sections {
        if section_name(shstrtab, s.sh_name) == name {
            return Ok(bytes[s.sh_offset as usize..(s.sh_offset + s.sh_size) as usize].to_vec());
        }
    }
    bail!("section {name} not found in {}", elf_path.display())
}

/// Decompress XZ-formatted bytes — `.gnu_debugdata` content uses XZ.
pub fn xz_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 8);
    lzma_rs::xz_decompress(&mut Cursor::new(data), &mut out)
        .context("xz decompression of .gnu_debugdata")?;
    Ok(out)
}

/// Build an unstripped ELF at `out_path` by merging the on-device
/// stripped `.so` with the `.symtab`/`.strtab` recovered from its
/// `.gnu_debugdata` minidebuginfo.
///
/// On success, the output file:
///   - has the same Build ID as the input (section data isn't rewritten);
///   - is usable by `hiperf report --symbol-dir <dir-containing-it>` to
///     resolve symbol names in stack-mode and flat reports.
pub fn merge_symbols(input: &Path, out_path: &Path) -> Result<()> {
    let orig = fs::read(input).with_context(|| format!("reading {}", input.display()))?;
    let (orig_hdr, orig_sections, orig_shstrtab) = parse_section_table(&orig)
        .with_context(|| format!("parsing section table from {}", input.display()))?;

    // 1. Locate `.gnu_debugdata`.
    let dbg_section = orig_sections
        .iter()
        .find(|s| section_name(orig_shstrtab, s.sh_name) == ".gnu_debugdata")
        .ok_or_else(|| {
            anyhow!(
                ".gnu_debugdata section not found in {} — either the file \
                     wasn't stripped or it's already unstripped (no merge needed)",
                input.display()
            )
        })?;
    let dbg_bytes = &orig
        [dbg_section.sh_offset as usize..(dbg_section.sh_offset + dbg_section.sh_size) as usize];

    // 2. XZ → minidebug ELF.
    let mini = xz_decompress(dbg_bytes).context("decompressing .gnu_debugdata")?;

    // 3. Extract .symtab / .strtab raw bytes from the minidebug ELF.
    let (_mini_hdr, mini_sections, mini_shstrtab) =
        parse_section_table(&mini).context("parsing minidebug ELF section table")?;
    let mini_symtab = mini_sections
        .iter()
        .find(|s| section_name(mini_shstrtab, s.sh_name) == ".symtab")
        .ok_or_else(|| anyhow!(".symtab missing inside minidebug ELF"))?;
    let mini_strtab = mini_sections
        .iter()
        .find(|s| section_name(mini_shstrtab, s.sh_name) == ".strtab")
        .ok_or_else(|| anyhow!(".strtab missing inside minidebug ELF"))?;
    let symtab_bytes = &mini
        [mini_symtab.sh_offset as usize..(mini_symtab.sh_offset + mini_symtab.sh_size) as usize];
    let strtab_bytes = &mini
        [mini_strtab.sh_offset as usize..(mini_strtab.sh_offset + mini_strtab.sh_size) as usize];

    // Heuristic for sh_info: number of leading STB_LOCAL symbols. The
    // minidebug-derived table has every symbol marked LOCAL (binding == 0),
    // so this equals total symbol count. Matches what the original
    // fix_symtab.py recipe produced.
    let n_syms = symtab_bytes.len() as u64 / ELF64_SYM_SIZE;
    let mut sh_info = 0u64;
    for i in 0..n_syms as usize {
        // st_info is at offset 4 of each 24-byte Sym entry. Upper nibble = binding.
        let st_info = symtab_bytes[i * ELF64_SYM_SIZE as usize + 4];
        if (st_info >> 4) == 0 {
            sh_info = (i + 1) as u64;
        } else {
            break;
        }
    }

    // 4. Build the new file.
    //
    // Layout we emit, in order:
    //   [orig_bytes verbatim]
    //   [pad to 8-byte alignment]
    //   [new .symtab data]
    //   [new .strtab data]
    //   [new .shstrtab data]   (orig names + ".symtab\0" + ".strtab\0")
    //   [new section header table]   (N_orig + 2 entries; the orig
    //                                 .shstrtab section's offset is
    //                                 rewritten to point at the new
    //                                 shstrtab location)
    //
    // The original section header table inside orig_bytes is left in place
    // but unreferenced — the patched ELF header's e_shoff points at the
    // newly-appended SHT.
    let mut out = orig.clone();
    pad_to_8(&mut out);

    let symtab_off = out.len() as u64;
    out.extend_from_slice(symtab_bytes);
    pad_to_8(&mut out);

    let strtab_off = out.len() as u64;
    out.extend_from_slice(strtab_bytes);
    pad_to_8(&mut out);

    // Build new shstrtab. Orig shstrtab bytes + ".symtab\0" + ".strtab\0".
    let new_shstr_off = out.len() as u64;
    let mut new_shstrtab = orig_shstrtab.to_vec();
    let symtab_name_off = new_shstrtab.len() as u32;
    new_shstrtab.extend_from_slice(b".symtab\0");
    let strtab_name_off = new_shstrtab.len() as u32;
    new_shstrtab.extend_from_slice(b".strtab\0");
    let new_shstrtab_size = new_shstrtab.len() as u64;
    out.extend_from_slice(&new_shstrtab);
    pad_to_8(&mut out);

    // Build new section header table. Use orig headers, then append two new
    // headers for .symtab/.strtab. We also REPLACE the old .shstrtab header
    // — keep it at its original index (e_shstrndx unchanged) but rewrite
    // its sh_offset/sh_size to point at the new shstrtab.
    let new_sht_off = out.len() as u64;
    let n_orig = orig_sections.len() as u32;
    let new_strtab_section_index = n_orig + 1; // .symtab first, .strtab second
    let mut new_sht = Vec::with_capacity(orig_sections.len() * 64 + 128);
    for (i, s) in orig_sections.iter().enumerate() {
        if i == orig_hdr.e_shstrndx as usize {
            let mut updated = s.clone();
            updated.sh_offset = new_shstr_off;
            updated.sh_size = new_shstrtab_size;
            write_shdr(&mut new_sht, &updated);
        } else {
            write_shdr(&mut new_sht, s);
        }
    }
    let symtab_hdr = SectionHeader {
        sh_name: symtab_name_off,
        sh_type: SHT_SYMTAB,
        sh_flags: 0,
        sh_addr: 0,
        sh_offset: symtab_off,
        sh_size: symtab_bytes.len() as u64,
        sh_link: new_strtab_section_index, // .strtab is the next new entry
        sh_info: sh_info as u32,
        sh_addralign: 8,
        sh_entsize: ELF64_SYM_SIZE,
    };
    write_shdr(&mut new_sht, &symtab_hdr);
    let strtab_hdr = SectionHeader {
        sh_name: strtab_name_off,
        sh_type: SHT_STRTAB,
        sh_flags: 0,
        sh_addr: 0,
        sh_offset: strtab_off,
        sh_size: strtab_bytes.len() as u64,
        sh_link: 0,
        sh_info: 0,
        sh_addralign: 1,
        sh_entsize: 0,
    };
    write_shdr(&mut new_sht, &strtab_hdr);
    out.extend_from_slice(&new_sht);

    // Patch ELF header: e_shoff, e_shnum. e_shstrndx stays the same (we
    // kept the original .shstrtab at its existing index, just retargeted
    // its sh_offset/sh_size).
    write_u64_le(&mut out, 40, new_sht_off); // e_shoff
    write_u16_le(&mut out, 60, (orig_sections.len() + 2) as u16); // e_shnum

    fs::write(out_path, &out)
        .with_context(|| format!("writing merged ELF to {}", out_path.display()))?;
    let _ = (SHT_PROGBITS,); // silence unused warning in non-test builds
    Ok(())
}

// ---------- ELF parsing helpers ----------

#[derive(Clone, Copy, Debug)]
struct ElfHeader {
    e_shoff: u64,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

#[derive(Clone, Debug)]
struct SectionHeader {
    sh_name: u32,
    sh_type: u32,
    sh_flags: u64,
    sh_addr: u64,
    sh_offset: u64,
    sh_size: u64,
    sh_link: u32,
    sh_info: u32,
    sh_addralign: u64,
    sh_entsize: u64,
}

fn parse_section_table(bytes: &[u8]) -> Result<(ElfHeader, Vec<SectionHeader>, &[u8])> {
    if bytes.len() < 64 || &bytes[0..4] != b"\x7fELF" {
        bail!("not an ELF file");
    }
    if bytes[4] != 2 {
        bail!("only ELF64 is supported (EI_CLASS != ELFCLASS64)");
    }
    if bytes[5] != 1 {
        bail!("only little-endian ELF is supported");
    }
    let e_shoff = read_u64_le(bytes, 40);
    let e_shentsize = read_u16_le(bytes, 58);
    let e_shnum = read_u16_le(bytes, 60);
    let e_shstrndx = read_u16_le(bytes, 62);
    if e_shentsize != 64 {
        bail!("unexpected e_shentsize {e_shentsize} (expected 64 for ELF64)");
    }
    let hdr = ElfHeader {
        e_shoff,
        e_shentsize,
        e_shnum,
        e_shstrndx,
    };

    let mut sections = Vec::with_capacity(e_shnum as usize);
    for i in 0..e_shnum as u64 {
        let off = (e_shoff + i * 64) as usize;
        if off + 64 > bytes.len() {
            bail!("section header {i} extends past end of file");
        }
        sections.push(read_shdr(&bytes[off..off + 64]));
    }

    // shstrtab is the bytes pointed at by sections[e_shstrndx].
    let shstr_section = sections
        .get(e_shstrndx as usize)
        .ok_or_else(|| anyhow!("e_shstrndx {e_shstrndx} out of range"))?;
    let shstr_off = shstr_section.sh_offset as usize;
    let shstr_end = shstr_off + shstr_section.sh_size as usize;
    if shstr_end > bytes.len() {
        bail!("shstrtab extends past end of file");
    }
    let shstrtab: &[u8] = &bytes[shstr_off..shstr_end];
    Ok((hdr, sections, shstrtab))
}

fn read_shdr(buf: &[u8]) -> SectionHeader {
    SectionHeader {
        sh_name: read_u32_le(buf, 0),
        sh_type: read_u32_le(buf, 4),
        sh_flags: read_u64_le(buf, 8),
        sh_addr: read_u64_le(buf, 16),
        sh_offset: read_u64_le(buf, 24),
        sh_size: read_u64_le(buf, 32),
        sh_link: read_u32_le(buf, 40),
        sh_info: read_u32_le(buf, 44),
        sh_addralign: read_u64_le(buf, 48),
        sh_entsize: read_u64_le(buf, 56),
    }
}

fn write_shdr(out: &mut Vec<u8>, s: &SectionHeader) {
    out.extend_from_slice(&s.sh_name.to_le_bytes());
    out.extend_from_slice(&s.sh_type.to_le_bytes());
    out.extend_from_slice(&s.sh_flags.to_le_bytes());
    out.extend_from_slice(&s.sh_addr.to_le_bytes());
    out.extend_from_slice(&s.sh_offset.to_le_bytes());
    out.extend_from_slice(&s.sh_size.to_le_bytes());
    out.extend_from_slice(&s.sh_link.to_le_bytes());
    out.extend_from_slice(&s.sh_info.to_le_bytes());
    out.extend_from_slice(&s.sh_addralign.to_le_bytes());
    out.extend_from_slice(&s.sh_entsize.to_le_bytes());
}

fn section_name(shstrtab: &[u8], name_off: u32) -> &str {
    let start = name_off as usize;
    if start >= shstrtab.len() {
        return "";
    }
    let end = shstrtab[start..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| start + p)
        .unwrap_or(shstrtab.len());
    std::str::from_utf8(&shstrtab[start..end]).unwrap_or("")
}

fn read_u16_le(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}
fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}
fn read_u64_le(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn write_u16_le(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn write_u64_le(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn pad_to_8(out: &mut Vec<u8>) {
    while out.len() % 8 != 0 {
        out.push(0);
    }
}

/// Read the GNU build id out of an ELF prefix, via the program headers.
///
/// Deliberately not via the section table: a stripped device library can drop
/// its section headers while always keeping the loadable `PT_NOTE`, and the
/// note sits in the first page of every library we stage (offset 0x2a8 and
/// 0x2c4 in the two real ones), so a 16 KB prefix is enough. That is what
/// makes it affordable to check the *device's* copy before a run rather than
/// pulling 150-300 MB.
///
/// Length varies by linker setting — Servo's OHOS build emits 8 bytes, the
/// ArkWeb engine and the servo-arkweb shim 20 — so callers must compare, not
/// assume a width.
pub fn build_id_from_elf_prefix(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" || bytes[4] != 2 {
        return None; // not a 64-bit ELF
    }
    let little = bytes[5] == 1;
    let u16_at = |o: usize| -> u16 {
        let b = [bytes[o], bytes[o + 1]];
        if little {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        }
    };
    let u32_at = |o: usize| -> u32 {
        let b = [bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]];
        if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }
    };
    let u64_at = |o: usize| -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&bytes[o..o + 8]);
        if little {
            u64::from_le_bytes(b)
        } else {
            u64::from_be_bytes(b)
        }
    };

    let phoff = u64_at(0x20) as usize;
    let phentsize = u16_at(0x36) as usize;
    let phnum = u16_at(0x38) as usize;
    for i in 0..phnum {
        let ph = phoff.checked_add(i.checked_mul(phentsize)?)?;
        if ph + 56 > bytes.len() {
            break;
        }
        if u32_at(ph) != 4 {
            continue; // PT_NOTE
        }
        let off = u64_at(ph + 0x08) as usize;
        let size = u64_at(ph + 0x20) as usize;
        let end = off.checked_add(size)?;
        if end > bytes.len() {
            continue; // note beyond the prefix we were given
        }
        let notes = &bytes[off..end];
        let mut pos = 0usize;
        while pos + 12 <= notes.len() {
            let namesz = u32_at(off + pos) as usize;
            let descsz = u32_at(off + pos + 4) as usize;
            let ntype = u32_at(off + pos + 8);
            pos += 12;
            let name_end = pos.checked_add(namesz)?;
            if name_end > notes.len() {
                break;
            }
            let name = &notes[pos..name_end];
            pos = name_end.checked_add(3)? & !3;
            let desc_end = pos.checked_add(descsz)?;
            if desc_end > notes.len() {
                break;
            }
            // NT_GNU_BUILD_ID, owner "GNU\0".
            if ntype == 3 && name.starts_with(b"GNU") {
                return Some(
                    notes[pos..desc_end]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect(),
                );
            }
            pos = desc_end.checked_add(3)? & !3;
        }
    }
    None
}

#[cfg(test)]
mod build_id_tests {
    #[test]
    fn rejects_things_that_are_not_a_64_bit_elf() {
        assert_eq!(super::build_id_from_elf_prefix(b"not an elf"), None);
        let mut fake = vec![0u8; 128];
        fake[..4].copy_from_slice(b"\x7fELF");
        fake[4] = 1; // 32-bit
        assert_eq!(super::build_id_from_elf_prefix(&fake), None);
    }

    #[test]
    fn a_64_bit_elf_without_a_note_has_no_build_id() {
        let mut fake = vec![0u8; 128];
        fake[..4].copy_from_slice(b"\x7fELF");
        fake[4] = 2; // 64-bit
        fake[5] = 1; // little endian
        assert_eq!(super::build_id_from_elf_prefix(&fake), None);
    }
}
