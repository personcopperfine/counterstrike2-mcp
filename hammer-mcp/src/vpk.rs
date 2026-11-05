//! Valve VPK directory reader (pure Rust, std only): opens a `_dir.vpk`, parses its directory
//! tree and reads file bytes (preload + archive data) for single entries. This is the reader the
//! model catalog's VPK sources run on (`tools/model_catalog.rs`).
//!
//! Format (validated against the real install 2026-10-15):
//!
//! * header: u32 magic `0x55AA1234`, u32 version (1 or 2); v1 keeps u32 `tree_size` at offset 8
//!   (12-byte header), v2 adds file_data_section_size, archive_md5_section_size,
//!   other_md5_section_size, signature_section_size (28-byte header).
//! * tree (header + tree_size bytes): repeated `{ ext\0 { dir\0 { name\0 entry }* \0 }* \0 }* \0`;
//!   a dir of `" "` means the root directory, an ext of `" "` means no extension.
//! * entry: u32 crc, u16 preload_bytes, u16 archive_index, u32 entry_offset, u32 entry_length,
//!   u16 terminator `0xFFFF`, then preload_bytes bytes.
//! * `archive_index == 0x7FFF`: the data lives in the `_dir.vpk` itself at
//!   `header_size + tree_size + entry_offset`; otherwise in the sibling `pak01_{index:03}.vpk`
//!   at `entry_offset` (the reader seeks + reads, never a whole archive into memory).
//!   File bytes = preload ++ archive data.
//! * preload-only entries (the WP: "preload-only file") also read from the tree — the real
//!   core pak carries them as `archive_index == 0x7FFF` with the **sentinel** offset
//!   `0xFFFFFFFF` and `entry_length == 0` (a real-install detail the corpus test enforced;
//!   `read` answers the preload bytes and touches no archive for a zero length).
//!
//! Every read is bounds-checked and reports a [`VpkError`] — parsing never panics and never
//! allocates a length taken from the file without checking it against the remaining bytes.
//! Reads are capped at [`MAX_READ_BYTES`] (256 MiB, [`VpkError::TooLarge`]).
//!
//! Real-install facts this was checked against (both `game/csgo/pak01_dir.vpk` and
//! `game/core/pak01_dir.vpk` are v2; the csgo tree is 6277806 bytes over 137693 entries, 12715
//! of them `models/*.vmdl_c`): for a read model the u32 file-size field equals the read length
//! and the u16 header version is 12; the tree ends exactly at header + tree_size. The
//! `#[ignore]`d corpus tests of `tests/vpk.rs` re-check all of that against the live install.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use error::VpkError;

/// Magic of every VPK (written little-endian: the bytes `34 12 AA 55`).
const MAGIC: u32 = 0x55AA_1234;
/// Version 1 header (magic, version, tree_size).
const HEADER_V1: usize = 12;
/// Version 2 header: the v1 fields plus the four v2 section sizes.
const HEADER_V2: usize = 28;
/// `tree_size` lives at this offset in both versions.
const TREE_SIZE_OFFSET: usize = 8;
/// An entry whose archive index is `0x7FFF` keeps its data in the `_dir.vpk` itself.
const ARCHIVE_IN_DIR: u16 = 0x7FFF;
/// The u16 that must close every entry.
const ENTRY_TERMINATOR: u16 = 0xFFFF;
/// Entry fixed length: crc u32 + preload u16 + archive_index u16 + entry_offset u32 +
/// entry_length u32 + terminator u16.
const ENTRY_FIXED: usize = 18;
/// The size cap of one [`VpkDir::read`]: a model file sits far below it, a corrupted
/// `entry_length` would ask for the whole archive.
pub const MAX_READ_BYTES: u64 = 256 * 1024 * 1024;

/// One directory-tree entry (one file inside the VPK).
#[derive(Debug, Clone, Copy)]
struct Entry {
    /// The entry's stored crc (parsed and kept; not re-checked on read).
    #[allow(dead_code)]
    crc: u32,
    /// Absolute offset of the preload bytes inside `raw` (`preload_len` bytes there).
    preload_at: usize,
    preload_len: u16,
    archive_index: u16,
    entry_offset: u32,
    entry_length: u32,
}

/// A parsed VPK directory (`pak01_dir.vpk`): the tree, its lowered paths and one `read()` per
/// entry. Sibling archives are opened per read (seek + bounded read).
#[derive(Debug, Clone)]
pub struct VpkDir {
    /// The `_dir.vpk` this was opened from (sibling archives resolve next to it).
    dir_vpk: PathBuf,
    /// The directory file's bytes: the tree occupies `..tree_end` (its start is the header
    /// length) and the in-dir data (`0x7FFF` entries) starts right at `tree_end`.
    raw: Arc<Vec<u8>>,
    /// The tree's end inside `raw` (== header length + tree_size, checked at open).
    tree_end: usize,
    /// Lowercase full path (`"models/foo/bar.vmdl_c"`, `/` separated) -> entry.
    map: BTreeMap<String, Entry>,
}

impl VpkDir {
    /// Open and parse a `_dir.vpk`. Errors: [`VpkError::Io`] (cannot read the file) and
    /// [`VpkError::Format`] (bad magic/version, truncated tree, unterminated string, a tree that
    /// does not end where `tree_size` says, a bad `0xFFFF` terminator, an entry whose preload
    /// or in-dir data points outside the file).
    pub fn open(dir_vpk: &Path) -> Result<VpkDir, VpkError> {
        let label = dir_vpk.display().to_string();
        let raw = std::fs::read(dir_vpk).map_err(|source| VpkError::Io {
            src: label.clone(),
            source,
        })?;
        parse(label, raw.into())
    }

    /// The number of files in the tree.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// `true` when the tree holds no files.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Every full path (lowercase, `/` separated, e.g. `"models/foo/bar.vmdl_c"`), sorted.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.map.keys().map(String::as_str)
    }

    /// Whether the tree holds this path. Case-insensitive; `\` accepted for `/`.
    pub fn contains(&self, path: &str) -> bool {
        self.map.contains_key(&normalize_key(path))
    }

    /// The bytes of one entry: preload data ++ the owning archive's data. `archive_index ==
    /// 0x7FFF` reads out of the directory file itself, anything else out of the sibling
    /// `<stem>_{index:03}.vpk`. Every failure names the requested path as it was spelled.
    ///
    /// The 256 MiB cap applies to the whole read; a bigger entry is [`VpkError::TooLarge`]
    /// before any allocation.
    pub fn read(&self, path: &str) -> Result<Vec<u8>, VpkError> {
        let entry = match self.map.get(&normalize_key(path)) {
            Some(entry) => entry,
            None => return Err(VpkError::NotFound(path.to_string())),
        };
        let total = u64::from(entry.preload_len) + u64::from(entry.entry_length);
        if total > MAX_READ_BYTES {
            return Err(VpkError::TooLarge {
                path: path.to_string(),
                size: total,
                max: MAX_READ_BYTES,
            });
        }
        let mut out = Vec::with_capacity(total as usize);
        out.extend_from_slice(
            &self.raw[entry.preload_at..entry.preload_at + usize::from(entry.preload_len)],
        );
        if entry.entry_length > 0 {
            match entry.archive_index {
                ARCHIVE_IN_DIR => out.extend_from_slice(in_dir_data(self, entry, path)?),
                archive => out.extend(sibling_data(&self.dir_vpk, entry, archive, path)?),
            }
        }
        Ok(out)
    }
}

// -------------------------------------------------------------------------------------------
// errors
// -------------------------------------------------------------------------------------------

mod error {
    use std::io;

    /// `VpkError::Io` with its file spelled (the `src` field is the message's prefix).
    /// `Format` names the file and the byte position; `NotFound` the asked path; `TooLarge`
    /// the path, its size and the cap.
    #[derive(Debug, thiserror::Error)]
    pub enum VpkError {
        #[error("io error on {src}: {source}")]
        Io {
            src: String,
            #[source]
            source: io::Error,
        },
        #[error("{0}")]
        Format(String),
        #[error("not found in the VPK tree: {0}")]
        NotFound(String),
        #[error("{path} is {size} bytes, the read cap is {max}")]
        TooLarge { path: String, size: u64, max: u64 },
    }
}

/// u32 -> usize of a parsed field (the 64-bit host can hold any u32 offset).
fn to_usize(v: u32) -> usize {
    v as usize
}

// -------------------------------------------------------------------------------------------
// helpers
// -------------------------------------------------------------------------------------------

/// A path's tree-key spelling: lowercase, `/` separated.
fn normalize_key(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

/// `<stem>_{archive:03}.vpk` next to the `_dir.vpk` (`pak01_dir.vpk` -> `pak01_004.vpk`). `None`
/// for the in-dir marker (`read` routes that into the directory file itself) and for a
/// directory file whose name does not end in `_dir.vpk`.
fn sibling_path(dir_vpk: &Path, archive: u16) -> Option<PathBuf> {
    if archive == ARCHIVE_IN_DIR {
        return None;
    }
    let name = dir_vpk.file_name()?.to_str()?;
    let stem = name.strip_suffix("_dir.vpk")?;
    let mut out = dir_vpk.to_path_buf();
    out.set_file_name(format!("{stem}_{archive:03}.vpk"));
    Some(out)
}

/// One entry's data from the directory file itself (`archive_index == 0x7FFF`). The range is
/// validated against the file at open, so this is a plain slice with an error as redundancy.
fn in_dir_data<'a>(vpk: &'a VpkDir, entry: &Entry, path: &str) -> Result<&'a [u8], VpkError> {
    let start = vpk.tree_end + to_usize(entry.entry_offset);
    vpk.raw
        .get(start..start + to_usize(entry.entry_length))
        .ok_or_else(|| {
            VpkError::Format(format!(
                "{path}: in-dir data from {start} runs past the directory file ({} bytes)",
                vpk.raw.len()
            ))
        })
}

/// One entry's data from the owning sibling archive: metadata first (the bounds check), then
/// seek + read_exact — never a whole archive into memory.
fn sibling_data(
    dir_vpk: &Path,
    entry: &Entry,
    archive: u16,
    path: &str,
) -> Result<Vec<u8>, VpkError> {
    let sibling = sibling_path(dir_vpk, archive).ok_or_else(|| {
        VpkError::Format(format!(
            "{path}: cannot name the sibling archive for index {archive} (the directory file \
             must end in _dir)"
        ))
    })?;
    let src = sibling.display().to_string();
    let len = std::fs::metadata(&sibling).map_err(io_err(&src))?.len();
    let start = u64::from(entry.entry_offset);
    let end = start + u64::from(entry.entry_length);
    if end > len {
        return Err(VpkError::Format(format!(
            "{path}: archive index {archive} data at {start}..{end} runs past the sibling \
             {src} ({len} bytes)"
        )));
    }
    let mut file = std::fs::File::open(&sibling).map_err(io_err(&src))?;
    file.seek(SeekFrom::Start(start)).map_err(io_err(&src))?;
    let mut buf = vec![0u8; to_usize(entry.entry_length)];
    file.read_exact(&mut buf).map_err(io_err(&src))?;
    Ok(buf)
}

/// `VpkError::Io` with `src` spelled (`File::open`/`metadata`/`seek`/`read_exact` sites).
fn io_err(src: &str) -> impl Fn(std::io::Error) -> VpkError + '_ {
    |source| VpkError::Io {
        src: src.to_string(),
        source,
    }
}

/// Checked cursor primitives: each answers `None` on any out-of-bounds, never a panic.
mod checked {
    pub fn u32(buf: &[u8], pos: usize) -> Option<u32> {
        let four: [u8; 4] = buf.get(pos..pos.checked_add(4)?)?.try_into().ok()?;
        Some(u32::from_le_bytes(four))
    }

    /// The `\0`-terminated string at `pos` (its length is found by scan, so the allocation is
    /// bounded by the input); non-UTF-8 bytes are made readable lossily, `None` is unterminated.
    pub fn cstr(buf: &[u8], pos: usize) -> Option<String> {
        if pos > buf.len() {
            return None;
        }
        let end = buf[pos..].iter().position(|byte| *byte == 0u8)? + pos;
        Some(String::from_utf8_lossy(&buf[pos..end]).into_owned())
    }
}

/// Normalize one tree path: a dir of `" "` is the root, an ext of `" "` means no extension.
fn full_path(dir: &str, name: &str, ext: &str) -> String {
    let mut out = String::new();
    if dir != " " {
        out.push_str(dir);
        out.push('/');
    }
    out.push_str(name);
    if ext != " " {
        out.push('.');
        out.push_str(ext);
    }
    out.to_lowercase()
}

// -------------------------------------------------------------------------------------------
// header + tree parsing
// -------------------------------------------------------------------------------------------

/// Parse the header and tree into a [`VpkDir`]; `label` names the file inside every error.
fn parse(label: String, raw: Arc<Vec<u8>>) -> Result<VpkDir, VpkError> {
    if raw.len() < HEADER_V1 {
        return Err(VpkError::Format(format!(
            "{label}: too small ({}/{HEADER_V1} bytes)",
            raw.len()
        )));
    }
    let magic = u32_at(&raw, 0, "magic", &label)?;
    if magic != MAGIC {
        return Err(VpkError::Format(format!(
            "{label}: bad magic {magic:#010x} (expected {MAGIC:#010x})"
        )));
    }
    let version = u32_at(&raw, 4, "version", &label)?;
    let header = match version {
        1 => HEADER_V1,
        2 => HEADER_V2,
        other => {
            return Err(VpkError::Format(format!(
                "{label}: unsupported VPK version {other} (expected 1 or 2)"
            )))
        }
    };
    if raw.len() < header {
        return Err(VpkError::Format(format!(
            "{label}: too small for a v{version} header ({} bytes)",
            raw.len()
        )));
    }
    let tree_size = u32_at(&raw, TREE_SIZE_OFFSET, "tree_size", &label)? as usize;
    let tree_start = header;
    let tree_end = tree_start
        .checked_add(tree_size)
        .filter(|check| *check <= raw.len())
        .ok_or_else(|| {
            VpkError::Format(format!(
                "{label}: tree_size {tree_size} runs past the file ({} bytes)",
                raw.len()
            ))
        })?;
    let tree = &raw[tree_start..tree_end];

    let mut map: BTreeMap<String, Entry> = BTreeMap::new();
    let mut pos = 0usize;
    loop {
        let ext = cstr(tree, &mut pos, &label)?;
        if ext.is_empty() {
            break;
        }
        loop {
            let dir = cstr(tree, &mut pos, &label)?;
            if dir.is_empty() {
                break;
            }
            loop {
                let name = cstr(tree, &mut pos, &label)?;
                if name.is_empty() {
                    break;
                }
                let mut entry = parse_entry(tree, &mut pos, &label)?;
                // The preload bytes must exist inside the tree; the caller consumed `pos` up to
                // them, so skipping is checked here before anything reads them.
                let preload_end = pos + usize::from(entry.preload_len);
                if tree.get(pos..preload_end).is_none() {
                    return Err(VpkError::Format(format!(
                        "{label}: entry preload runs to {preload_end}, the tree ends at {}",
                        tree.len()
                    )));
                }
                entry.preload_at = tree_start + pos;
                pos = preload_end;
                let path = full_path(&dir, &name, &ext);
                if map.insert(path.clone(), entry).is_some() {
                    return Err(VpkError::Format(format!(
                        "{label}: duplicate tree path {path:?}"
                    )));
                }
            }
        }
    }
    if pos != tree.len() {
        return Err(VpkError::Format(format!(
            "{label}: the tree has {} unexpected trailing byte(s) at offset {}",
            tree.len() - pos,
            tree_start + pos
        )));
    }

    // Every in-dir entry that claims data is checked against the file at open, so `read`
    // stays a slice. Preload-only entries also carry index `0x7FFF` (with the sentinel offset
    // `0xFFFFFFFF`, length 0 — seen in the real core pak) and skip this entirely.
    let base = tree_end;
    for (path, entry) in &map {
        if entry.archive_index == ARCHIVE_IN_DIR && entry.entry_length > 0 {
            let start = base + to_usize(entry.entry_offset);
            if raw.len() < start.saturating_add(to_usize(entry.entry_length)) {
                return Err(VpkError::Format(format!(
                    "{label}: in-dir data of {path} at {start}..{} runs past the file ({} bytes)",
                    start + to_usize(entry.entry_length),
                    raw.len()
                )));
            }
        }
    }

    Ok(VpkDir {
        dir_vpk: PathBuf::from(&label),
        raw,
        tree_end,
        map,
    })
}

/// u32 at `pos` (the buffer is already known to cover the header, so this is a named refusal).
fn u32_at(buf: &[u8], pos: usize, field: &str, label: &str) -> Result<u32, VpkError> {
    checked::u32(buf, pos)
        .ok_or_else(|| VpkError::Format(format!("{label}: corrupted header (cannot read {field})")))
}

/// One entry's fixed fields; `pos` stands on the crc and is left on the preload bytes (the call
/// site bounds-checks and skips them).
fn parse_entry(tree: &[u8], pos: &mut usize, label: &str) -> Result<Entry, VpkError> {
    let at = *pos;
    let Some(fixed) = tree.get(at..at + ENTRY_FIXED) else {
        return Err(VpkError::Format(format!(
            "{label}: entry at {at} needs {ENTRY_FIXED} bytes, the tree ends at {}",
            tree.len()
        )));
    };
    let four = |range: std::ops::Range<usize>| -> u32 {
        u32::from_le_bytes(fixed[range].try_into().expect("fixed fields are 4 bytes"))
    };
    let entry = Entry {
        crc: four(0..4),
        preload_at: 0,
        preload_len: u16::from_le_bytes(fixed[4..6].try_into().expect("fixed fields are 2 bytes")),
        archive_index: u16::from_le_bytes(
            fixed[6..8].try_into().expect("fixed fields are 2 bytes"),
        ),
        entry_offset: four(8..12),
        entry_length: four(12..16),
    };
    let terminator =
        u16::from_le_bytes(fixed[16..18].try_into().expect("fixed fields are 2 bytes"));
    if terminator != ENTRY_TERMINATOR {
        return Err(VpkError::Format(format!(
            "{label}: entry at {at} has terminator {terminator:#06x} (expected \
             {ENTRY_TERMINATOR:#06x})"
        )));
    }
    *pos = at + ENTRY_FIXED;
    Ok(entry)
}

/// `\0`-terminated tree string with the terminator consumed; unterminated -> `Format`.
fn cstr(tree: &[u8], pos: &mut usize, label: &str) -> Result<String, VpkError> {
    let text = checked::cstr(tree, *pos).ok_or_else(|| {
        VpkError::Format(format!(
            "{label}: unterminated string at {} (truncated tree)",
            tree.len()
        ))
    })?;
    *pos += text.len() + 1;
    Ok(text)
}

// -------------------------------------------------------------------------------------------
// unit tests: the small internal invariants. The opened-file/acceptance coverage, including the
// `#[ignore]`d real-install corpus tests, is `tests/vpk.rs`.
// -------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Root dir (`" "` `readme` `txt`), a normal model path, an extension-less file, case.
    #[test]
    fn full_path_roots_and_extensions() {
        assert_eq!(
            full_path("models/props", "box", "vmdl_c"),
            "models/props/box.vmdl_c"
        );
        assert_eq!(full_path(" ", "readme", "txt"), "readme.txt");
        assert_eq!(full_path("dir", "LICENSE", " "), "dir/license");
        assert_eq!(
            full_path("models/A/B", "MixEd", "vmdl_C"),
            "models/a/b/mixed.vmdl_c"
        );
    }

    /// The lookup spelling: backslash in, `/` out, lowercase.
    #[test]
    fn normalize_key_swaps_separators() {
        assert_eq!(
            normalize_key("models\\PROPS\\Box.vmdl_c"),
            "models/props/box.vmdl_c"
        );
        assert_eq!(
            normalize_key("models/props/Box.vmdl_c"),
            "models/props/box.vmdl_c"
        );
    }

    /// `0x7FFF` belongs to the directory file itself and names no sibling; other indexes map to
    /// `pak01_<NNN>.vpk` next to the `_dir.vpk`; a non-`_dir` tree has no resolvable sibling.
    #[test]
    fn sibling_paths() {
        let dir = Path::new("/x/game/csgo/pak01_dir.vpk");
        assert_eq!(
            sibling_path(dir, 0),
            Some(PathBuf::from("/x/game/csgo/pak01_000.vpk"))
        );
        assert_eq!(
            sibling_path(dir, 7),
            Some(PathBuf::from("/x/game/csgo/pak01_007.vpk"))
        );
        let bogus = Path::new("/x/game/csgo/other.vpk");
        assert_eq!(sibling_path(bogus, 0), None);
        assert_eq!(sibling_path(dir, ARCHIVE_IN_DIR), None);
    }
}
