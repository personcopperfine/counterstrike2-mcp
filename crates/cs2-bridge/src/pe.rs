//! The PE32+ facts [`crate::modules`] gates a module on, and SHA-256.
//!
//! Direct port of the C bridge's `PeInfo` / `pe_info` block (`bridge/mcp_bridge.c:1241-1302`)
//! and of `sha256_file` (`:1276-1298`): read the facts the Windows loader itself read
//! (`TimeDateStamp`, `SizeOfImage`, `CheckSum`, the CodeView RSDS GUID/age), never a field the
//! loader relocates. The constants are file-format facts from `winnt.h` - named after the field
//! they are the offset of, as the C does, because they are not game numbers and therefore do not
//! come from the pins.
//!
//! Two readers live here:
//!
//! * [`pe_info`] parses **mapped image** bytes (index = RVA), which is what `verify_one` sees in
//!   memory on Windows and what [`map_image`] builds from a file on the host.
//! * [`map_image`] turns a file's bytes into that same RVA-indexed layout the way the loader
//!   maps a section: the headers at RVA 0 and every section at its `VirtualAddress`. The checks
//!   are RVA-based, and `PointerToRawData` differs from `VirtualAddress` in every real DLL
//!   (FileAlignment), so the map is not optional for a file-backed image.
//!
//! SHA-256 is implemented here because no crate in `Cargo.lock` provides it (the brief allows a
//! dependency already in the lock, and `sha2` is not) and the workspace has no other SHA-256:
//! `crates/s2sdk/s2sdk/src/hash.rs` is MurmurHash2, not SHA-2.

/// `IMAGE_DOS_HEADER::e_lfanew` (`winnt.h`): offset of the PE header, from the image base
/// (`bridge/mcp_bridge.c:1248`).
pub const PE_E_LFANEW_OFF: usize = 0x3C;
/// `IMAGE_NT_SIGNATURE` (`winnt.h`): the PE header starts with `"PE\0\0"`
/// (`bridge/mcp_bridge.c:1249`).
pub const PE_SIG: u32 = 0x0000_4550;
/// `IMAGE_FILE_HEADER::TimeDateStamp`, from the PE header (`bridge/mcp_bridge.c:1250`):
/// `IMAGE_NT_SIGNATURE` (4) + `Machine` (2) + `NumberOfSections` (2).
pub const PE_TIME_DATE_STAMP_OFF: usize = 8;
/// The optional header starts after the signature (4) and `IMAGE_FILE_HEADER` (20)
/// (`bridge/mcp_bridge.c:1251`).
pub const PE_OPTIONAL_HDR_OFF: usize = 24;
/// `IMAGE_OPTIONAL_HEADER64::Magic` value for PE32+ (`winnt.h` `IMAGE_NT_OPTIONAL_HDR64_MAGIC`;
/// `bridge/mcp_bridge.c:1252`).
pub const PE32PLUS_MAGIC: u16 = 0x20B;
/// `IMAGE_OPTIONAL_HEADER64::SizeOfImage` (`bridge/mcp_bridge.c:1253`).
pub const PE32PLUS_SIZEOFIMAGE: usize = 56;
/// `IMAGE_OPTIONAL_HEADER64::CheckSum` (`bridge/mcp_bridge.c:1254`).
pub const PE32PLUS_CHECKSUM: usize = 64;
/// `IMAGE_OPTIONAL_HEADER64::NumberOfRvaAndSizes` (`bridge/mcp_bridge.c:1255`).
pub const PE32PLUS_NRVA_OFF: usize = 0x6C;
/// `IMAGE_OPTIONAL_HEADER64::DataDirectory` (`bridge/mcp_bridge.c:1256`).
pub const PE32PLUS_DATADIR_OFF: usize = 0x70;
/// `IMAGE_DIRECTORY_ENTRY_DEBUG` (`winnt.h`; `bridge/mcp_bridge.c:1257`).
pub const PE_DATADIR_DEBUG: usize = 6;
/// `sizeof(IMAGE_DATA_DIRECTORY)` (`bridge/mcp_bridge.c:1258`).
pub const PE_DATADIR_STRIDE: usize = 8;
/// `sizeof(IMAGE_DEBUG_DIRECTORY)` (`bridge/mcp_bridge.c:1259`).
pub const PE_DBGDIR_BYTES: usize = 28;
/// `IMAGE_DEBUG_DIRECTORY::Type` (`bridge/mcp_bridge.c:1260`).
pub const PE_DBGDIR_TYPE_OFF: usize = 12;
/// `IMAGE_DEBUG_TYPE_CODEVIEW` (`bridge/mcp_bridge.c:1261`).
pub const PE_DBGDIR_TYPE_CODEVIEW: u32 = 2;
/// `IMAGE_DEBUG_DIRECTORY::AddressOfRawData` - an RVA (`bridge/mcp_bridge.c:1262`).
pub const PE_DBGDIR_ADDRRAW_OFF: usize = 20;
/// The CodeView RSDS record: `"RSDS"` (4) + GUID (16) + Age (`bridge/mcp_bridge.c:1263`).
pub const CV_RSDS_AGE_OFF: usize = 20;

// The rest are only needed to build the RVA-indexed layout from a file; the C never maps a file
// (it verifies the loader's mapping) so `pe_info` above does not use them.

/// `IMAGE_OPTIONAL_HEADER64::ImageBase`: the address the file's absolute pointers are written
/// for, so a file image's base is the preferred base and a live image's base is the load base.
pub const PE32PLUS_IMAGEBASE: usize = 24;
/// `IMAGE_OPTIONAL_HEADER64::SizeOfHeaders`.
pub const PE32PLUS_SIZEOFHEADERS: usize = 60;
/// `IMAGE_FILE_HEADER::NumberOfSections`, from the PE header.
pub const PE_NUMBER_OF_SECTIONS_OFF: usize = 6;
/// `IMAGE_FILE_HEADER::SizeOfOptionalHeader`, from the PE header.
pub const PE_SIZE_OF_OPTIONAL_HEADER_OFF: usize = 20;
/// `sizeof(IMAGE_SECTION_HEADER)` (`winnt.h`).
pub const PE_SECTION_HEADER_BYTES: usize = 40;
/// `IMAGE_SECTION_HEADER::VirtualSize`.
pub const PE_SECTION_VIRTUAL_SIZE_OFF: usize = 8;
/// `IMAGE_SECTION_HEADER::VirtualAddress`.
pub const PE_SECTION_VIRTUAL_ADDRESS_OFF: usize = 12;
/// `IMAGE_SECTION_HEADER::SizeOfRawData`.
pub const PE_SECTION_RAW_SIZE_OFF: usize = 16;
/// `IMAGE_SECTION_HEADER::PointerToRawData`.
pub const PE_SECTION_RAW_PTR_OFF: usize = 20;

/// The mapped-image facts `verify_one` compares, mirroring the C's `PeInfo`
/// (`bridge/mcp_bridge.c:1244`). `rsds` is `None` when no CodeView RSDS record was found; the C
/// keeps that as `haveRsds` and fails the same check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeInfo {
    /// `IMAGE_FILE_HEADER::TimeDateStamp`.
    pub time_stamp: u32,
    /// `IMAGE_OPTIONAL_HEADER64::SizeOfImage`.
    pub size_of_image: u32,
    /// `IMAGE_OPTIONAL_HEADER64::CheckSum`.
    pub check_sum: u32,
    /// The RSDS GUID (`GUID` in the CodeView record) and age, or `None`.
    pub rsds: Option<([u8; 16], u32)>,
}

/// A bounds-checked little-endian `u16` read; `None` past the end.
fn u16_at(bytes: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(off..off + 2)?.try_into().ok()?))
}

/// A bounds-checked little-endian `u32` read; `None` past the end.
fn u32_at(bytes: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(off..off + 4)?.try_into().ok()?))
}

/// A bounds-checked little-endian `u64` read; `None` past the end.
fn u64_at(bytes: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(off..off + 8)?.try_into().ok()?))
}

/// `pe_info` (`bridge/mcp_bridge.c:1265-1301`): the mapped image's never-relocated PE facts.
///
/// `None` is the C's `return 0` - not an `MZ` image, not `"PE\0\0"`, or not PE32+ (the checks
/// the C's `pe_info` itself makes). A missing or malformed debug directory is **not** a parse
/// failure: the C leaves `haveRsds = 0` and `verify_one` then fails the RSDS check, and so does
/// this. Out-of-range reads that the C would fault on are treated the same way (no RSDS).
#[must_use]
pub fn pe_info(image: &[u8]) -> Option<PeInfo> {
    if image.get(..2) != Some(b"MZ") {
        return None;
    }
    let pe = usize::try_from(u32_at(image, PE_E_LFANEW_OFF)?).ok()?;
    if u32_at(image, pe)? != PE_SIG {
        return None;
    }
    let opt = pe.checked_add(PE_OPTIONAL_HDR_OFF)?;
    if u16_at(image, opt)? != PE32PLUS_MAGIC {
        return None;
    }
    let info = PeInfo {
        time_stamp: u32_at(image, pe.checked_add(PE_TIME_DATE_STAMP_OFF)?)?,
        size_of_image: u32_at(image, opt.checked_add(PE32PLUS_SIZEOFIMAGE)?)?,
        check_sum: u32_at(image, opt.checked_add(PE32PLUS_CHECKSUM)?)?,
        rsds: None,
    };

    let number_of_rva = u32_at(image, opt.checked_add(PE32PLUS_NRVA_OFF)?)?;
    if number_of_rva as usize <= PE_DATADIR_DEBUG {
        return Some(info);
    }
    let entry = opt
        .checked_add(PE32PLUS_DATADIR_OFF)?
        .checked_add(PE_DATADIR_DEBUG * PE_DATADIR_STRIDE)?;
    let (Some(debug_rva), Some(debug_size)) = (u32_at(image, entry), u32_at(image, entry + 4))
    else {
        return Some(info);
    };
    let debug_rva = usize::try_from(debug_rva).ok()?;
    let debug_size = usize::try_from(debug_size).ok()?;
    let mut off = 0usize;
    while off + PE_DBGDIR_BYTES <= debug_size {
        let dir = debug_rva.checked_add(off)?;
        off += PE_DBGDIR_BYTES;
        let Some(kind) = u32_at(image, dir + PE_DBGDIR_TYPE_OFF) else {
            return Some(info);
        };
        if kind != PE_DBGDIR_TYPE_CODEVIEW {
            continue;
        }
        let Some(raw_rva) = u32_at(image, dir + PE_DBGDIR_ADDRRAW_OFF) else {
            return Some(info);
        };
        let Ok(raw_rva) = usize::try_from(raw_rva) else {
            return Some(info);
        };
        if image.get(raw_rva..raw_rva + 4) != Some(b"RSDS") {
            continue;
        }
        let Some(guid) = image.get(raw_rva + 4..raw_rva + 20) else {
            return Some(info);
        };
        let Ok(guid) = <[u8; 16]>::try_from(guid) else {
            return Some(info);
        };
        let Some(age) = u32_at(image, raw_rva + CV_RSDS_AGE_OFF) else {
            return Some(info);
        };
        return Some(PeInfo {
            rsds: Some((guid, age)),
            ..info
        });
    }
    Some(info)
}

/// Why [`map_image`] refused a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// Not an `MZ`/`PE\0\0`/PE32+ file (the same three checks as [`pe_info`]).
    NotPe32Plus,
    /// A header field points outside the file or the image.
    OutOfRange,
}

/// A file mapped the way the Windows loader maps an image: headers at RVA 0, sections at their
/// `VirtualAddress`, gaps zero-filled. `bytes.len()` is `SizeOfImage`.
#[derive(Clone, Debug)]
pub struct MappedImage {
    /// The RVA-indexed image.
    pub bytes: Vec<u8>,
    /// `IMAGE_OPTIONAL_HEADER64::ImageBase`: the base the file's absolute pointers are written
    /// for (the loader relocates them if it maps the image elsewhere).
    pub image_base: u64,
}

/// Map `file` into an RVA-indexed image (`nt!MiCreateImageFileMap`'s layout in miniature).
///
/// The header block `[0, SizeOfHeaders)` is copied verbatim, then for every section
/// `min(VirtualSize, SizeOfRawData)` bytes (or `SizeOfRawData` when `VirtualSize` is 0) are
/// copied from `PointerToRawData` to `VirtualAddress`. Everything else stays zero, as the
/// loader leaves it.
pub fn map_image(file: &[u8]) -> Result<MappedImage, MapError> {
    let pe = usize::try_from(u32_at(file, PE_E_LFANEW_OFF).ok_or(MapError::NotPe32Plus)?)
        .map_err(|_| MapError::NotPe32Plus)?;
    if u32_at(file, pe) != Some(PE_SIG) {
        return Err(MapError::NotPe32Plus);
    }
    let opt = pe.checked_add(PE_OPTIONAL_HDR_OFF).ok_or(MapError::OutOfRange)?;
    if u16_at(file, opt) != Some(PE32PLUS_MAGIC) {
        return Err(MapError::NotPe32Plus);
    }
    let image_base = u64_at(file, opt.checked_add(PE32PLUS_IMAGEBASE).ok_or(MapError::OutOfRange)?)
        .ok_or(MapError::OutOfRange)?;
    let size_of_image =
        u32_at(file, opt.checked_add(PE32PLUS_SIZEOFIMAGE).ok_or(MapError::OutOfRange)?)
            .ok_or(MapError::OutOfRange)?;
    let size_of_headers =
        u32_at(file, opt.checked_add(PE32PLUS_SIZEOFHEADERS).ok_or(MapError::OutOfRange)?)
            .ok_or(MapError::OutOfRange)?;
    let number_of_sections =
        u16_at(file, pe.checked_add(PE_NUMBER_OF_SECTIONS_OFF).ok_or(MapError::OutOfRange)?)
            .ok_or(MapError::OutOfRange)?;
    let size_of_optional_header =
        u16_at(file, pe.checked_add(PE_SIZE_OF_OPTIONAL_HEADER_OFF).ok_or(MapError::OutOfRange)?)
            .ok_or(MapError::OutOfRange)?;

    let size_of_image = usize::try_from(size_of_image).map_err(|_| MapError::OutOfRange)?;
    let size_of_headers = usize::try_from(size_of_headers).map_err(|_| MapError::OutOfRange)?;
    if size_of_headers > size_of_image {
        return Err(MapError::OutOfRange);
    }
    let mut bytes = vec![0u8; size_of_image];
    let header_end = size_of_headers.min(file.len());
    bytes[..header_end].copy_from_slice(&file[..header_end]);

    let sections = opt
        .checked_add(usize::from(size_of_optional_header))
        .ok_or(MapError::OutOfRange)?;
    for index in 0..usize::from(number_of_sections) {
        let header = sections
            .checked_add(index.checked_mul(PE_SECTION_HEADER_BYTES).ok_or(MapError::OutOfRange)?)
            .ok_or(MapError::OutOfRange)?;
        let virtual_size =
            u32_at(file, header + PE_SECTION_VIRTUAL_SIZE_OFF).ok_or(MapError::OutOfRange)?;
        let virtual_address =
            u32_at(file, header + PE_SECTION_VIRTUAL_ADDRESS_OFF).ok_or(MapError::OutOfRange)?;
        let raw_size =
            u32_at(file, header + PE_SECTION_RAW_SIZE_OFF).ok_or(MapError::OutOfRange)?;
        let raw_pointer =
            u32_at(file, header + PE_SECTION_RAW_PTR_OFF).ok_or(MapError::OutOfRange)?;

        let virtual_size = usize::try_from(virtual_size).map_err(|_| MapError::OutOfRange)?;
        let virtual_address =
            usize::try_from(virtual_address).map_err(|_| MapError::OutOfRange)?;
        let raw_size = usize::try_from(raw_size).map_err(|_| MapError::OutOfRange)?;
        let raw_pointer = usize::try_from(raw_pointer).map_err(|_| MapError::OutOfRange)?;

        // The loader maps at most `max(VirtualSize, SizeOfRawData)`, but the bytes past
        // `VirtualSize` are not part of the image; a zero VirtualSize means "use raw size".
        let length = if virtual_size == 0 { raw_size } else { virtual_size.min(raw_size) };
        if length == 0 {
            continue;
        }
        let source = raw_pointer.checked_add(length).ok_or(MapError::OutOfRange)?;
        let destination = virtual_address.checked_add(length).ok_or(MapError::OutOfRange)?;
        if source > file.len() || destination > size_of_image {
            return Err(MapError::OutOfRange);
        }
        bytes[virtual_address..destination].copy_from_slice(&file[raw_pointer..source]);
    }
    Ok(MappedImage { bytes, image_base })
}

/// The SHA-256 round constants (`FIPS 180-4` 4.2.2).
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
    0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
    0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
    0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
    0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
    0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
    0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
    0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
    0xc67178f2,
];

/// The SHA-256 initial hash values (`FIPS 180-4` 5.3.3).
const SHA256_H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
    0x5be0cd19,
];

/// SHA-256 state, so the live adapter can hash a module file in `sha256_file`'s 256 KiB chunks
/// (`bridge/mcp_bridge.c:1294`) instead of reading a 100 MB DLL into memory.
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    block_len: usize,
    total_len: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    /// A fresh hash.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: SHA256_H0,
            block: [0; 64],
            block_len: 0,
            total_len: 0,
        }
    }

    /// Feed bytes (`BCryptHashData`'s per-chunk call).
    pub fn update(&mut self, mut bytes: &[u8]) {
        self.total_len = self.total_len.wrapping_add(bytes.len() as u64);
        if self.block_len != 0 {
            let take = (64 - self.block_len).min(bytes.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&bytes[..take]);
            self.block_len += take;
            bytes = &bytes[take..];
            if self.block_len < 64 {
                return;
            }
            let block = self.block;
            self.compress(&block);
            self.block_len = 0;
        }
        while bytes.len() >= 64 {
            let (block, rest) = bytes.split_at(64);
            self.compress(block.try_into().expect("64 bytes"));
            bytes = rest;
        }
        if !bytes.is_empty() {
            self.block[..bytes.len()].copy_from_slice(bytes);
            self.block_len = bytes.len();
        }
    }

    /// Padding and the final digest (`BCryptFinishHash`).
    #[must_use]
    pub fn finish(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.block[self.block_len] = 0x80;
        self.block_len += 1;
        if self.block_len > 56 {
            self.block[self.block_len..].fill(0);
            let block = self.block;
            self.compress(&block);
            self.block = [0; 64];
        } else {
            self.block[self.block_len..56].fill(0);
        }
        self.block[56..].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.block;
        self.compress(&block);

        let mut digest = [0u8; 32];
        for (chunk, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *chunk = word.to_be_bytes();
        }
        digest
    }

    /// One compression function call (`FIPS 180-4` 6.2.2).
    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (index, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[index] = u32::from_be_bytes(*word);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in self
            .state
            .iter_mut()
            .zip([a, b, c, d, e, f, g, h])
        {
            *slot = slot.wrapping_add(value);
        }
    }
}

/// SHA-256 of `bytes` (the whole-buffer form of [`Sha256`]).
#[must_use]
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(bytes);
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::{Sha256, map_image, pe_info, sha256};

    /// The `FIPS 180-4` / NIST CAVP vectors: empty, `"abc"`, the 56-byte two-block message and
    /// the million-`a` message (which exercises the streaming `update` path).
    #[test]
    fn sha256_matches_the_nist_vectors() {
        let vector = |bytes: &[u8]| {
            let digest = sha256(bytes);
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        assert_eq!(
            vector(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            vector(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            vector(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );

        let million = vec![b'a'; 1_000_000];
        let mut streaming = Sha256::new();
        for chunk in million.chunks(65_536) {
            streaming.update(chunk);
        }
        assert_eq!(
            streaming.finish(),
            [
                0xcd, 0xc7, 0x6e, 0x5c, 0x99, 0x14, 0xfb, 0x92, 0x81, 0xa1, 0xc7, 0xe2, 0x84,
                0xd7, 0x3e, 0x67, 0xf1, 0x80, 0x9a, 0x48, 0xa4, 0x97, 0x20, 0x0e, 0x04, 0x6d,
                0x39, 0xcc, 0xc7, 0x11, 0x2c, 0xd0,
            ]
        );
        // Chunked and one-shot agree for a length that ends exactly on a block boundary.
        let aligned = vec![0x5au8; 128];
        let mut chunked = Sha256::new();
        chunked.update(&aligned[..64]);
        chunked.update(&aligned[64..]);
        assert_eq!(chunked.finish(), sha256(&aligned));
    }

    /// A synthetic PE32+: a `PeInfo` read straight out of the mapped layout, as `verify_one`
    /// does it. The fields sit at the winnt.h offsets the constants name.
    #[test]
    fn pe_info_reads_the_mapped_header_and_rsds() {
        let mut image = vec![0u8; 0x2000];
        image[..2].copy_from_slice(b"MZ");
        image[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        image[0x80..0x84].copy_from_slice(&super::PE_SIG.to_le_bytes());
        let opt = 0x80 + super::PE_OPTIONAL_HDR_OFF;
        image[opt..opt + 2].copy_from_slice(&super::PE32PLUS_MAGIC.to_le_bytes());
        image[0x80 + super::PE_TIME_DATE_STAMP_OFF..0x80 + super::PE_TIME_DATE_STAMP_OFF + 4]
            .copy_from_slice(&0x6ac8001fu32.to_le_bytes());
        image[opt + super::PE32PLUS_SIZEOFIMAGE..opt + super::PE32PLUS_SIZEOFIMAGE + 4]
            .copy_from_slice(&0x2000u32.to_le_bytes());
        image[opt + super::PE32PLUS_CHECKSUM..opt + super::PE32PLUS_CHECKSUM + 4]
            .copy_from_slice(&0x1234u32.to_le_bytes());
        image[opt + super::PE32PLUS_NRVA_OFF..opt + super::PE32PLUS_NRVA_OFF + 4]
            .copy_from_slice(&16u32.to_le_bytes());
        let debug = opt + super::PE32PLUS_DATADIR_OFF + super::PE_DATADIR_DEBUG * 8;
        image[debug..debug + 4].copy_from_slice(&0x1000u32.to_le_bytes());
        image[debug + 4..debug + 8].copy_from_slice(&28u32.to_le_bytes());
        // IMAGE_DEBUG_DIRECTORY at RVA 0x1000: CodeView, data at 0x1100.
        let dir = 0x1000;
        image[dir + super::PE_DBGDIR_TYPE_OFF..dir + super::PE_DBGDIR_TYPE_OFF + 4]
            .copy_from_slice(&super::PE_DBGDIR_TYPE_CODEVIEW.to_le_bytes());
        image[dir + super::PE_DBGDIR_ADDRRAW_OFF..dir + super::PE_DBGDIR_ADDRRAW_OFF + 4]
            .copy_from_slice(&0x1100u32.to_le_bytes());
        image[0x1100..0x1104].copy_from_slice(b"RSDS");
        image[0x1104..0x1114].copy_from_slice(&[0xab; 16]);
        image[0x1114..0x1118].copy_from_slice(&7u32.to_le_bytes());

        let info = pe_info(&image).expect("the synthetic header parses");
        assert_eq!(info.time_stamp, 0x6ac8001f);
        assert_eq!(info.size_of_image, 0x2000);
        assert_eq!(info.check_sum, 0x1234);
        assert_eq!(info.rsds, Some(([0xab; 16], 7)));

        // No RSDS: pe_info still succeeds, with `rsds: None`.
        image[0x1100] = b'X';
        assert_eq!(pe_info(&image).expect("still PE32+").rsds, None);

        // A non-PE32+ magic is a parse failure, like the C's `return 0`.
        image[opt] = 0x0b;
        image[opt + 1] = 0x01;
        assert!(pe_info(&image).is_none());
    }

    /// `map_image` places a section at its `VirtualAddress`, not its file offset.
    #[test]
    fn map_image_places_sections_at_their_rvas() {
        let mut file = vec![0u8; 0x600];
        file[..2].copy_from_slice(b"MZ");
        file[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        file[0x40..0x44].copy_from_slice(&super::PE_SIG.to_le_bytes());
        file[0x46..0x48].copy_from_slice(&1u16.to_le_bytes()); // NumberOfSections
        file[0x54..0x56].copy_from_slice(&0xF0u16.to_le_bytes()); // SizeOfOptionalHeader
        let opt = 0x40 + super::PE_OPTIONAL_HDR_OFF;
        file[opt..opt + 2].copy_from_slice(&super::PE32PLUS_MAGIC.to_le_bytes());
        file[opt + super::PE32PLUS_IMAGEBASE..opt + super::PE32PLUS_IMAGEBASE + 8]
            .copy_from_slice(&0x1_8000_0000u64.to_le_bytes());
        file[opt + super::PE32PLUS_SIZEOFIMAGE..opt + super::PE32PLUS_SIZEOFIMAGE + 4]
            .copy_from_slice(&0x4000u32.to_le_bytes());
        file[opt + super::PE32PLUS_SIZEOFHEADERS..opt + super::PE32PLUS_SIZEOFHEADERS + 4]
            .copy_from_slice(&0x200u32.to_le_bytes());
        let section = opt + 0xF0;
        file[section + super::PE_SECTION_VIRTUAL_SIZE_OFF
            ..section + super::PE_SECTION_VIRTUAL_SIZE_OFF + 4]
            .copy_from_slice(&0x100u32.to_le_bytes());
        file[section + super::PE_SECTION_VIRTUAL_ADDRESS_OFF
            ..section + super::PE_SECTION_VIRTUAL_ADDRESS_OFF + 4]
            .copy_from_slice(&0x2000u32.to_le_bytes());
        file[section + super::PE_SECTION_RAW_SIZE_OFF..section + super::PE_SECTION_RAW_SIZE_OFF + 4]
            .copy_from_slice(&0x200u32.to_le_bytes());
        file[section + super::PE_SECTION_RAW_PTR_OFF..section + super::PE_SECTION_RAW_PTR_OFF + 4]
            .copy_from_slice(&0x400u32.to_le_bytes());
        file[0x400..0x600].fill(0x5a);

        let mapped = map_image(&file).expect("a well-formed image maps");
        assert_eq!(mapped.image_base, 0x1_8000_0000);
        assert_eq!(mapped.bytes.len(), 0x4000);
        assert_eq!(&mapped.bytes[0x2000..0x2100], &[0x5a; 0x100]);
        // The raw bytes at 0x400 are not at RVA 0x400 in the mapped image (headers live there).
        assert_eq!(mapped.bytes[0x400], 0);
    }
}
