//! The defined dynamic symbols of an ELF64 little-endian shared object (x86_64, the only Linux
//! target the bridge and the loader are built for): what `llvm-nm -D --defined-only` lists.
//!
//! The symbols come from the `SHT_DYNSYM` section and the string table its `sh_link` names. A
//! symbol is *defined* when its `st_shndx` is not `SHN_UNDEF`; `SHN_ABS` symbols (a linker's
//! version-node symbols, for one) count as defined, the way `nm` lists them. Symbol versions are
//! not appended: the gate compares names, and neither our cdylibs (rustc's anonymous version
//! script) nor the gate's expected sets carry a version. A file without a `.dynsym` is refused,
//! not reported empty: a shared object always has one, so its absence means the file is not
//! what the gate thinks it is.

use anyhow::{ensure, Context, Result};

/// `ELFCLASS64`, `ELFDATA2LSB`.
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
/// `Elf64_Ehdr`: `e_shoff`, `e_shentsize`, `e_shnum`, `e_shstrndx`.
const E_SHOFF: usize = 0x28;
const E_SHENTSIZE: usize = 0x3A;
const E_SHNUM: usize = 0x3C;
/// `sizeof(Elf64_Shdr)` and its `sh_type`, `sh_offset`, `sh_size`, `sh_link`, `sh_entsize`.
const SHDR: usize = 64;
const SH_TYPE: usize = 4;
const SH_OFFSET: usize = 24;
const SH_SIZE: usize = 32;
const SH_LINK: usize = 40;
const SH_ENTSIZE: usize = 56;
/// `SHT_DYNSYM`.
const SHT_DYNSYM: u32 = 11;
/// `sizeof(Elf64_Sym)` and its `st_name`, `st_shndx`.
const SYM: usize = 24;
const ST_NAME: usize = 0;
const ST_SHNDX: usize = 6;
/// `SHN_UNDEF`.
const SHN_UNDEF: u16 = 0;

/// The names of the defined dynamic symbols in `bytes`, in symbol-table order (the null symbol
/// and empty names skipped).
pub fn defined_dynamic_symbols(bytes: &[u8]) -> Result<Vec<String>> {
    ensure!(bytes.get(..4) == Some(b"\x7fELF"), "not an ELF image (no ELF magic)");
    ensure!(
        bytes.get(4) == Some(&ELFCLASS64) && bytes.get(5) == Some(&ELFDATA2LSB),
        "not an ELF64 little-endian image"
    );
    let shoff = usize::try_from(u64_at(bytes, E_SHOFF)?).context("e_shoff")?;
    ensure!(shoff != 0, "no section header table");
    ensure!(usize::from(u16_at(bytes, E_SHENTSIZE)?) == SHDR, "e_shentsize is not sizeof(Elf64_Shdr)");
    let mut shnum = usize::from(u16_at(bytes, E_SHNUM)?);
    if shnum == 0 {
        // Extended numbering: the real count is section 0's sh_size.
        shnum = usize::try_from(u64_at(bytes, shoff + SH_SIZE)?).context("extended e_shnum")?;
    }
    let section = |i: usize| -> Result<usize> {
        let off = i
            .checked_mul(SHDR)
            .and_then(|o| o.checked_add(shoff))
            .filter(|o| o.checked_add(SHDR).is_some_and(|end| end <= bytes.len()))
            .with_context(|| format!("section header {i} lies outside the file"))?;
        Ok(off)
    };
    let dynsym = (0..shnum)
        .map(section)
        .find(|s| s.as_ref().map_or(true, |s| u32_at(bytes, s + SH_TYPE).ok() == Some(SHT_DYNSYM)))
        .context("no .dynsym section (SHT_DYNSYM)")??;
    let syms = slice(bytes, dynsym).context(".dynsym")?;
    let entsize = usize::try_from(u64_at(bytes, dynsym + SH_ENTSIZE)?).unwrap_or(0);
    ensure!(entsize == SYM, ".dynsym sh_entsize {entsize} is not sizeof(Elf64_Sym)");
    let strtab = section(u32_at(bytes, dynsym + SH_LINK)? as usize)?;
    let strings = slice(bytes, strtab).context(".dynstr")?;

    let mut names = Vec::new();
    for sym in syms.as_chunks::<SYM>().0.iter().skip(1) {
        if u16_at(sym, ST_SHNDX)? == SHN_UNDEF {
            continue;
        }
        let name = u32_at(sym, ST_NAME)? as usize;
        let rest = strings.get(name..).with_context(|| format!("symbol name offset {name} past .dynstr"))?;
        let len = rest.iter().position(|b| *b == 0).context("symbol name has no NUL")?;
        if len > 0 {
            names.push(String::from_utf8_lossy(&rest[..len]).into_owned());
        }
    }
    Ok(names)
}

/// The file bytes of the section whose header is at `shdr`.
fn slice(bytes: &[u8], shdr: usize) -> Result<&[u8]> {
    let off = usize::try_from(u64_at(bytes, shdr + SH_OFFSET)?)?;
    let size = usize::try_from(u64_at(bytes, shdr + SH_SIZE)?)?;
    off.checked_add(size)
        .and_then(|end| bytes.get(off..end))
        .with_context(|| format!("section data {off:#x}+{size:#x} lies outside the file"))
}

fn u16_at(bytes: &[u8], off: usize) -> Result<u16> {
    let b = bytes.get(off..off.saturating_add(2)).with_context(|| format!("truncated at {off:#x}"))?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(bytes: &[u8], off: usize) -> Result<u32> {
    let b = bytes.get(off..off.saturating_add(4)).with_context(|| format!("truncated at {off:#x}"))?;
    Ok(u32::from_le_bytes(b.try_into().expect("four bytes")))
}

fn u64_at(bytes: &[u8], off: usize) -> Result<u64> {
    let b = bytes.get(off..off.saturating_add(8)).with_context(|| format!("truncated at {off:#x}"))?;
    Ok(u64::from_le_bytes(b.try_into().expect("eight bytes")))
}
