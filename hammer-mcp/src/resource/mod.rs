//! Source 2 **compiled resource** container + **binary KV3** + compiled-model facts (plan:
//! `target/WP.md` WP-M). This module is a *library*: `tools/model_info.rs` stays a placeholder in
//! this work package and a later one wires the `get_model_info` tool on top of
//! [`model::model_info_from_bytes`].
//!
//! # The container (verified against real files)
//!
//! A compiled resource (`*.vmdl_c`, `*.vmat_c`, …) starts with a fixed 16-byte header:
//!
//! ```text
//! u32 file_size        total file size (must equal the real length)
//! u16 header_version   12 for every current resource
//! u16 version          1 in every CS2 model; unknown to matter here
//! u32 block_offset     relative to the *field* (offset 8), so the block table is at 8 + value
//! u32 block_count
//! ```
//!
//! then a table of `block_count` 12-byte entries, one per block:
//!
//! ```text
//! [4]u8 type           4CC: DATA, MDAT, ASEQ, AGRP, ANIM, PHYS, CTRL, RERL, RED2, MBUF, MVTX, …
//! u32   offset         relative to the *field* that holds it (table entry start + 4)
//! u32   size
//! ```
//!
//! Every offset/size pair is range-checked against the file length at parse time, so a block read
//! never indexes out of range. Blocks may repeat: an agent model carries five `MDAT` blocks (one
//! render mesh each). Observed in 300 sampled `models/**/*.vmdl_c` of the installed
//! `game/csgo/pak01_dir.vpk`: `header_version` 12, container `version` 1, `block_offset` 8 in
//! all 300; every file's declared `file_size` matches its real length.
//!
//! # Blocks this library reads
//!
//! * `DATA` — binary KV3, the model's keyvalue tree (`m_name`, `m_modelInfo`, `m_materialGroups`,
//!   `m_meshGroups`, `m_modelSkeleton`, …);
//! * `MDAT` — one binary-KV3 `CRenderMesh` per model render mesh (union of `m_sceneObjects[].`
//!   `m_vMinBounds`/`m_vMaxBounds` = the model's render box; `m_attachments[]` = attachment points);
//! * `ASEQ` — binary KV3 sequence data (`m_localSequenceNameArray` = the sequence names);
//! * `PHYS` — binary KV3 physics (`m_parts[].m_rnShape`): presence means the model has collision.
//!
//! [`kv3bin`] decodes the KV3 block itself; [`model`] turns one file's blocks into [`ModelInfo`].
//!
//! Error discipline: a file that is not a resource, a truncated block table, a block that runs past
//! the file, a missing `DATA` block, an unsupported KV3 version or a corrupt KV3 payload are all
//! **errors** — never an empty success that looks like "this model has no facts".

pub mod kv3bin;
pub mod model;

pub use kv3bin::{decode as decode_kv3, Kv3Document, Kv3Error, Kv3Value};
pub use model::{model_info_from_bytes, Aabb, BodyGroup, ModelInfo};

/// `header_version` of every current compiled resource (ValveResourceFormat `Resource.cs`).
pub const HEADER_VERSION: u16 = 12;

/// One entry of a resource's block table. `offset`/`size` are already validated against the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRef {
    /// The block's 4CC type, e.g. `*b"DATA"`.
    pub kind: [u8; 4],
    /// Absolute byte offset in the file.
    pub offset: usize,
    /// Byte length (offset + size <= file length).
    pub size: usize,
}

impl BlockRef {
    /// The 4CC as text (`DATA`, `MDAT`, …); lossy for a non-ASCII type, which is only ever a
    /// corrupt file.
    pub fn type_name(&self) -> String {
        String::from_utf8_lossy(&self.kind).into_owned()
    }
}

/// A parsed resource container: the block table plus borrowed file bytes.
#[derive(Debug, Clone)]
pub struct Resource<'a> {
    bytes: &'a [u8],
    /// Header version (12 for every resource this library accepts).
    pub header_version: u16,
    /// Container version (1 in every CS2 model seen).
    pub version: u16,
    /// Block table in file order (a type may occur more than once).
    pub blocks: Vec<BlockRef>,
}

impl<'a> Resource<'a> {
    /// Parse the 16-byte header and the block table. Every block's `offset + size` must fit in the
    /// file; the declared `file_size` must equal the real length (a truncated or padded file is an
    /// error, not a smaller read).
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ResourceError> {
        if bytes.len() < 16 {
            return Err(ResourceError::HeaderTooSmall {
                need: 16,
                got: bytes.len(),
            });
        }
        let mut r = Reader::new(bytes, "resource header");
        let file_size = r.u32()?;
        let header_version = r.u16()?;
        let version = r.u16()?;
        let block_offset = r.u32()?;
        let block_count = r.u32()?;

        if header_version != HEADER_VERSION {
            return Err(ResourceError::UnsupportedHeaderVersion {
                found: header_version,
                expected: HEADER_VERSION,
            });
        }
        if file_size as usize != bytes.len() {
            return Err(ResourceError::FileSizeMismatch {
                declared: file_size,
                actual: bytes.len(),
            });
        }

        // `block_offset` is relative to its own field position (8), so the table starts at 8 + value.
        let table_start = 8u64 + u64::from(block_offset);
        let table_end = table_start + u64::from(block_count) * 12;
        if table_start < 16 || table_end > bytes.len() as u64 {
            return Err(ResourceError::BlockTableOutOfBounds {
                offset: table_start,
                count: block_count,
                file_size: bytes.len(),
            });
        }

        let mut blocks = Vec::with_capacity(block_count as usize);
        for index in 0..block_count as usize {
            let entry = table_start as usize + index * 12;
            let kind = [
                bytes[entry],
                bytes[entry + 1],
                bytes[entry + 2],
                bytes[entry + 3],
            ];
            let rel = u32::from_le_bytes([
                bytes[entry + 4],
                bytes[entry + 5],
                bytes[entry + 6],
                bytes[entry + 7],
            ]);
            let size = u32::from_le_bytes([
                bytes[entry + 8],
                bytes[entry + 9],
                bytes[entry + 10],
                bytes[entry + 11],
            ]);
            // Relative to the field that holds it, i.e. `entry + 4`.
            let offset = entry as u64 + 4 + u64::from(rel);
            if offset + u64::from(size) > bytes.len() as u64 {
                return Err(ResourceError::BlockOutOfBounds {
                    kind: String::from_utf8_lossy(&kind).into_owned(),
                    offset,
                    size: u64::from(size),
                    file_size: bytes.len(),
                });
            }
            blocks.push(BlockRef {
                kind,
                offset: offset as usize,
                size: size as usize,
            });
        }

        Ok(Resource {
            bytes,
            header_version,
            version,
            blocks,
        })
    }

    /// Every block of one type, in file order.
    pub fn blocks_of(&self, kind: &[u8; 4]) -> impl Iterator<Item = &BlockRef> + '_ {
        let kind = *kind;
        self.blocks.iter().filter(move |block| block.kind == kind)
    }

    /// The bytes of a block (its offset/size were validated by [`Resource::parse`]).
    pub fn block_bytes(&self, block: &BlockRef) -> &'a [u8] {
        &self.bytes[block.offset..block.offset + block.size]
    }

    /// The external resources this file references (`RERL`, the external-reference list), e.g. a
    /// `.vmat_c`'s `materials/x/y_color.vtex` (uncompiled spelling, no `_c`). A file with no `RERL`
    /// block references nothing (the compiler omits the block when the list is empty: 111 of the
    /// first 200 `materials/dev/` materials in the real csgo/core paks, all texture-less shaders
    /// such as `black_cheap`, `bloom`); `None` only for a malformed list. Layout (ValveResourceFormat `ResourceExtRefList`): `u32 offset` (relative to
    /// the block start), `u32 count`, then at `block + offset` `count` entries of `u64 id` and an
    /// `i64` offset to the NUL-terminated name, relative to that offset field itself.
    pub fn external_refs(&self) -> Option<Vec<String>> {
        self.external_refs_checked().ok()
    }

    /// [`Resource::external_refs`] that says why it has no answer: an empty list for a missing
    /// `RERL` block (nothing referenced), `Err` for a list that is truncated or malformed (a header shorter than 8 bytes, an
    /// entry or name running past the block, a name without its NUL). A partial list is never
    /// returned: a caller that treated it as "all the references" would pass a file whose last
    /// textures it never saw.
    pub fn external_refs_checked(&self) -> Result<Vec<String>, String> {
        let Some(block) = self.blocks_of(b"RERL").next() else {
            return Ok(Vec::new());
        };
        let data = self.block_bytes(block);
        let u32_at = |at: usize| data.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        let (Some(offset), Some(count)) = (u32_at(0), u32_at(4)) else {
            return Err(format!("the RERL block is {} bytes, shorter than its 8-byte header", data.len()));
        };
        let mut out = Vec::new();
        for i in 0..count as usize {
            let bad = |why: &str| format!("the RERL list is malformed at entry {i} of {count}: {why}");
            let field = offset as usize + i * 16 + 8;
            let Some(raw) = data.get(field..field + 8) else { return Err(bad("the entry table runs past the block")) };
            let rel = i64::from_le_bytes(raw.try_into().expect("8 bytes"));
            let Some(start) = (field as i64).checked_add(rel).filter(|s| *s >= 0) else {
                return Err(bad("a name offset is negative"));
            };
            let Some(tail) = data.get(start as usize..) else { return Err(bad("a name offset is past the block")) };
            let Some(end) = tail.iter().position(|b| *b == 0) else { return Err(bad("a name has no terminator")) };
            out.push(String::from_utf8_lossy(&tail[..end]).into_owned());
        }
        Ok(out)
    }

    /// The block types in file order (for reports/notes).
    pub fn block_types(&self) -> Vec<String> {
        self.blocks.iter().map(BlockRef::type_name).collect()
    }
}

/// Everything that can go wrong reading a compiled resource: the container, its binary-KV3 blocks,
/// or the facts a caller needed from them.
#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    #[error("resource header truncated: {got} bytes, need at least {need}")]
    HeaderTooSmall { need: usize, got: usize },
    #[error("not a compiled resource: header version {found}, expected {expected}")]
    UnsupportedHeaderVersion { found: u16, expected: u16 },
    #[error("resource file size field says {declared} bytes but the file is {actual} bytes")]
    FileSizeMismatch { declared: u32, actual: usize },
    #[error("block table at {offset} ({count} blocks) does not fit in the {file_size}-byte file")]
    BlockTableOutOfBounds {
        offset: u64,
        count: u32,
        file_size: usize,
    },
    #[error("block {kind} at {offset} + {size} bytes runs past the {file_size}-byte file")]
    BlockOutOfBounds {
        kind: String,
        offset: u64,
        size: u64,
        file_size: usize,
    },
    #[error("block table truncated at byte {at}: {reason}")]
    BlockTableMalformed { at: usize, reason: String },
    #[error("no {kind} block in the resource (blocks: {blocks})")]
    MissingBlock { kind: &'static str, blocks: String },
    #[error("{what} is not {expected}: it is {found}")]
    UnexpectedValue {
        what: &'static str,
        expected: &'static str,
        found: &'static str,
    },
    #[error(transparent)]
    Kv3(#[from] Kv3Error),
}

/// A bounded little-endian reader over borrowed bytes; every read is range-checked.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    what: &'static str,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8], what: &'static str) -> Self {
        Reader {
            bytes,
            pos: 0,
            what,
        }
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], ResourceError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| self.malformed("length overflow"))?;
        if end > self.bytes.len() {
            return Err(self.malformed(&format!(
                "need {n} bytes at {} but only {} remain",
                self.pos,
                self.bytes.len() - self.pos
            )));
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub(crate) fn u16(&mut self) -> Result<u16, ResourceError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, ResourceError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn malformed(&self, reason: &str) -> ResourceError {
        ResourceError::BlockTableMalformed {
            at: self.pos,
            reason: format!("{}: {reason}", self.what),
        }
    }
}
