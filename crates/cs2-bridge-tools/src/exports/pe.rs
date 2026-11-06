//! The two PE directories the export gate reads, straight from a file: the export directory
//! (every export address table entry, and the name each one has) and the DLL names of the
//! import and delay-import directories.
//!
//! This is a file reader, not a loader: an RVA is turned into a file offset through the section
//! table (`PointerToRawData` differs from `VirtualAddress` in every real DLL, FileAlignment), and
//! anything that points outside the file is a malformed image, refused rather than skipped - a
//! gate that cannot read a directory must not report it empty. The offsets are file-format facts
//! from `winnt.h`, named after the field they are the offset of; they are not game numbers.

use anyhow::{bail, ensure, Context, Result};

/// `IMAGE_DOS_HEADER::e_lfanew`: the file offset of the PE header.
const E_LFANEW: usize = 0x3C;
/// `IMAGE_NT_SIGNATURE`, `"PE\0\0"`.
const PE_SIGNATURE: &[u8; 4] = b"PE\0\0";
/// `IMAGE_FILE_HEADER::NumberOfSections`, from the PE signature.
const NUMBER_OF_SECTIONS: usize = 6;
/// `IMAGE_FILE_HEADER::SizeOfOptionalHeader`, from the PE signature.
const SIZE_OF_OPTIONAL_HEADER: usize = 20;
/// The optional header follows the signature (4) and `IMAGE_FILE_HEADER` (20).
const OPTIONAL_HEADER: usize = 24;
/// `IMAGE_NT_OPTIONAL_HDR32_MAGIC` / `IMAGE_NT_OPTIONAL_HDR64_MAGIC`.
const PE32_MAGIC: u16 = 0x10B;
const PE32PLUS_MAGIC: u16 = 0x20B;
/// `ImageBase` (a u32 in PE32 at 28, a u64 in PE32+ at 24): only an old VA-based delay-import
/// descriptor needs it.
const PE32_IMAGE_BASE: usize = 28;
const PE32PLUS_IMAGE_BASE: usize = 24;
/// `NumberOfRvaAndSizes` and the `DataDirectory` array, PE32 / PE32+.
const PE32_NUMBER_OF_RVA: usize = 92;
const PE32_DATA_DIRECTORY: usize = 96;
const PE32PLUS_NUMBER_OF_RVA: usize = 108;
const PE32PLUS_DATA_DIRECTORY: usize = 112;
/// `IMAGE_DIRECTORY_ENTRY_EXPORT` / `_IMPORT` / `_DELAY_IMPORT`.
const DIR_EXPORT: usize = 0;
const DIR_IMPORT: usize = 1;
const DIR_DELAY_IMPORT: usize = 13;
/// `sizeof(IMAGE_SECTION_HEADER)` and its `VirtualSize`, `VirtualAddress`, `SizeOfRawData`,
/// `PointerToRawData`.
const SECTION_HEADER: usize = 40;
const SECTION_VIRTUAL_SIZE: usize = 8;
const SECTION_VIRTUAL_ADDRESS: usize = 12;
const SECTION_RAW_SIZE: usize = 16;
const SECTION_RAW_POINTER: usize = 20;
/// `IMAGE_EXPORT_DIRECTORY`: `NumberOfFunctions`, `NumberOfNames`, `AddressOfNames`,
/// `AddressOfNameOrdinals` (the export address table itself is not needed: an entry is an entry
/// whatever it points at).
const EXPORT_NUMBER_OF_FUNCTIONS: usize = 20;
const EXPORT_NUMBER_OF_NAMES: usize = 24;
const EXPORT_ADDRESS_OF_NAMES: usize = 32;
const EXPORT_ADDRESS_OF_NAME_ORDINALS: usize = 36;
/// `sizeof(IMAGE_IMPORT_DESCRIPTOR)` and its `Name` RVA.
const IMPORT_DESCRIPTOR: usize = 20;
const IMPORT_NAME: usize = 12;
/// `sizeof(IMAGE_DELAYLOAD_DESCRIPTOR)`, its `Attributes` (bit 0: `RvaBased`) and `DllNameRVA`.
const DELAY_DESCRIPTOR: usize = 32;
const DELAY_ATTRIBUTES: usize = 0;
const DELAY_DLL_NAME: usize = 4;
/// The most entries any one table is walked for. An export or import table of our artefacts has
/// a handful; a count past this is a corrupt header, refused before it turns into a long loop.
const MAX_ENTRIES: usize = 1 << 16;

/// What the export gate needs from a PE file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeFacts {
    /// `NumberOfFunctions`: every export address table entry, named or not (`0` with no export
    /// directory).
    pub export_entries: usize,
    /// The name of each entry that has one, in name-table order (an entry with two names
    /// appears twice).
    pub export_names: Vec<String>,
    /// How many entries have at least one name; `export_entries - named_entries` are
    /// ordinal-only.
    pub named_entries: usize,
    /// The DLL names of the import directory, then of the delay-import directory.
    pub imports: Vec<String>,
}

/// Reads the export and import facts of the PE image in `bytes`.
pub fn read(bytes: &[u8]) -> Result<PeFacts> {
    ensure!(bytes.get(..2) == Some(b"MZ"), "not a PE image (no MZ header)");
    let pe = u32_at(bytes, E_LFANEW)? as usize;
    ensure!(
        bytes.get(pe..pe.saturating_add(4)) == Some(PE_SIGNATURE),
        "not a PE image (no PE signature at e_lfanew {pe:#x})"
    );
    let image = Image::new(bytes, pe)?;

    let mut facts = PeFacts::default();
    if let Some((rva, _)) = image.directory(DIR_EXPORT)? {
        let dir = image.offset(rva, 40).context("export directory")?;
        let functions = image.count(dir + EXPORT_NUMBER_OF_FUNCTIONS, "NumberOfFunctions")?;
        let names = image.count(dir + EXPORT_NUMBER_OF_NAMES, "NumberOfNames")?;
        let name_table = u32_at(bytes, dir + EXPORT_ADDRESS_OF_NAMES)?;
        let ordinal_table = u32_at(bytes, dir + EXPORT_ADDRESS_OF_NAME_ORDINALS)?;
        let mut named = vec![false; functions];
        for i in 0..names {
            let name_rva = u32_at(bytes, image.offset(rva_add(name_table, 4 * i)?, 4).context("export name table")?)?;
            let index = u16_at(bytes, image.offset(rva_add(ordinal_table, 2 * i)?, 2).context("export ordinal table")?)?;
            let slot = named
                .get_mut(usize::from(index))
                .with_context(|| format!("export name {i} points at entry {index}, past NumberOfFunctions {functions}"))?;
            *slot = true;
            facts.export_names.push(image.c_string(name_rva).context("export name")?);
        }
        facts.export_entries = functions;
        facts.named_entries = named.iter().filter(|n| **n).count();
    }

    if let Some((rva, _)) = image.directory(DIR_IMPORT)? {
        for i in 0..MAX_ENTRIES {
            let desc = image.offset(rva_add(rva, i * IMPORT_DESCRIPTOR)?, IMPORT_DESCRIPTOR).context("import directory")?;
            if bytes[desc..desc + IMPORT_DESCRIPTOR].iter().all(|b| *b == 0) {
                break;
            }
            let name = u32_at(bytes, desc + IMPORT_NAME)?;
            facts.imports.push(image.c_string(name).context("import DLL name")?);
        }
    }

    if let Some((rva, _)) = image.directory(DIR_DELAY_IMPORT)? {
        for i in 0..MAX_ENTRIES {
            let desc = image.offset(rva_add(rva, i * DELAY_DESCRIPTOR)?, DELAY_DESCRIPTOR).context("delay-import directory")?;
            if bytes[desc..desc + DELAY_DESCRIPTOR].iter().all(|b| *b == 0) {
                break;
            }
            let attributes = u32_at(bytes, desc + DELAY_ATTRIBUTES)?;
            let mut name = u64::from(u32_at(bytes, desc + DELAY_DLL_NAME)?);
            // Version 1 descriptors (attribute bit 0 clear, pre-VC7 linkers) hold VAs.
            if attributes & 1 == 0 {
                name = name.checked_sub(image.image_base).context("delay-import DLL name VA below ImageBase")?;
            }
            let name = u32::try_from(name).context("delay-import DLL name past 4 GiB")?;
            facts.imports.push(image.c_string(name).context("delay-import DLL name")?);
        }
    }
    Ok(facts)
}

/// The parsed headers: where the data directories and the section table are.
struct Image<'a> {
    bytes: &'a [u8],
    image_base: u64,
    size_of_headers: usize,
    number_of_rva: usize,
    data_directory: usize,
    sections: Vec<Section>,
}

/// One `IMAGE_SECTION_HEADER`, the fields an RVA-to-offset lookup uses.
struct Section {
    virtual_address: u32,
    virtual_size: u32,
    raw_size: u32,
    raw_pointer: u32,
}

impl<'a> Image<'a> {
    fn new(bytes: &'a [u8], pe: usize) -> Result<Self> {
        let opt = pe + OPTIONAL_HEADER;
        let (image_base, number_of_rva, data_directory) = match u16_at(bytes, opt)? {
            PE32PLUS_MAGIC => (
                u64_at(bytes, opt + PE32PLUS_IMAGE_BASE)?,
                u32_at(bytes, opt + PE32PLUS_NUMBER_OF_RVA)? as usize,
                opt + PE32PLUS_DATA_DIRECTORY,
            ),
            PE32_MAGIC => (
                u64::from(u32_at(bytes, opt + PE32_IMAGE_BASE)?),
                u32_at(bytes, opt + PE32_NUMBER_OF_RVA)? as usize,
                opt + PE32_DATA_DIRECTORY,
            ),
            magic => bail!("unknown optional header magic {magic:#x}"),
        };
        let section_table = opt + usize::from(u16_at(bytes, pe + SIZE_OF_OPTIONAL_HEADER)?);
        let count = usize::from(u16_at(bytes, pe + NUMBER_OF_SECTIONS)?);
        let sections = (0..count)
            .map(|i| {
                let s = section_table + i * SECTION_HEADER;
                Ok(Section {
                    virtual_size: u32_at(bytes, s + SECTION_VIRTUAL_SIZE)?,
                    virtual_address: u32_at(bytes, s + SECTION_VIRTUAL_ADDRESS)?,
                    raw_size: u32_at(bytes, s + SECTION_RAW_SIZE)?,
                    raw_pointer: u32_at(bytes, s + SECTION_RAW_POINTER)?,
                })
            })
            .collect::<Result<_>>()
            .context("section table")?;
        Ok(Image {
            bytes,
            image_base,
            size_of_headers: section_table + count * SECTION_HEADER,
            number_of_rva,
            data_directory,
            sections,
        })
    }

    /// The `(rva, size)` of data directory `index`, `None` when absent or empty.
    fn directory(&self, index: usize) -> Result<Option<(u32, u32)>> {
        if index >= self.number_of_rva {
            return Ok(None);
        }
        let entry = self.data_directory + index * 8;
        let rva = u32_at(self.bytes, entry)?;
        let size = u32_at(self.bytes, entry + 4)?;
        Ok((rva != 0 && size != 0).then_some((rva, size)))
    }

    /// The file offset of `len` bytes at `rva`, which must lie in one section's file data (or
    /// in the headers).
    fn offset(&self, rva: u32, len: usize) -> Result<usize> {
        let found = if (rva as usize) < self.size_of_headers {
            Some(rva as usize)
        } else {
            self.sections.iter().find_map(|s| {
                let delta = rva.checked_sub(s.virtual_address)?;
                // Bytes past VirtualSize are not mapped, bytes past SizeOfRawData are not in the
                // file (the loader zero-fills them): neither holds a name the gate could read.
                let mapped = if s.virtual_size == 0 { s.raw_size } else { s.virtual_size };
                (delta < mapped && delta < s.raw_size).then(|| s.raw_pointer as usize + delta as usize)
            })
        };
        let off = found.with_context(|| format!("RVA {rva:#x} is in no section's file data"))?;
        ensure!(
            off.checked_add(len).is_some_and(|end| end <= self.bytes.len()),
            "RVA {rva:#x} (+{len}) runs past the end of the file"
        );
        Ok(off)
    }

    /// A count field, refused past [`MAX_ENTRIES`].
    fn count(&self, off: usize, what: &str) -> Result<usize> {
        let n = u32_at(self.bytes, off)? as usize;
        ensure!(n <= MAX_ENTRIES, "{what} {n} is not plausible (corrupt export directory?)");
        Ok(n)
    }

    /// The NUL-terminated string at `rva`.
    fn c_string(&self, rva: u32) -> Result<String> {
        let start = self.offset(rva, 1)?;
        let rest = &self.bytes[start..];
        let len = rest.iter().position(|b| *b == 0).with_context(|| format!("string at RVA {rva:#x} has no NUL"))?;
        Ok(String::from_utf8_lossy(&rest[..len]).into_owned())
    }
}

/// `rva + delta`, refused when it leaves the 32-bit RVA space (a corrupt table pointer).
fn rva_add(rva: u32, delta: usize) -> Result<u32> {
    u32::try_from(delta)
        .ok()
        .and_then(|d| rva.checked_add(d))
        .with_context(|| format!("RVA {rva:#x} + {delta:#x} overflows"))
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
