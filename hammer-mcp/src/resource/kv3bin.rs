//! Binary KV3 — the serialisation of compiled Source 2 keyvalues (`DATA`, `MDAT`, `ASEQ`, `PHYS`,
//! `CTRL`, … blocks of a [`crate::resource`] container).
//!
//! Reimplemented from the format (the layout was read off ValveResourceFormat's
//! `Resource/ResourceTypes/BinaryKV3.cs`, MIT; no code copied). Everything below is verified against
//! real files: **versions 4 and 5** and **compressions 0 (none), 1 (LZ4), 2 (ZSTD)** all occur in the
//! installed `game/csgo/pak01_dir.vpk` (a header census over the 66 334 KV3 blocks of its 13 105
//! `*.vmdl_c` finds 53 236 version-4 and 13 098 version-5 blocks; 1 892 uncompressed, 64 400 LZ4,
//! 42 ZSTD — every ZSTD block carries binary blobs, as do 10 147 LZ4 blocks). Versions 1–3 and the
//! legacy `VKV\x03` container are refused with a clear error — the CS2 model compiler does not emit
//! them.
//!
//! # Layout (version 4)
//!
//! ```text
//! u32 magic             0x4B563304 ("4VK" + version byte)
//! [16]u8 format         KV3 format GUID ("generic" = 7c161274e9069846aff2e63eb59037e7 in models)
//! u32 compression       0 none, 1 LZ4 block, 2 ZSTD frame(s)
//! u16 dict_id           must be 0
//! u16 frame_size        16384 for LZ4, otherwise 0
//! i32 count_bytes1      values in the main buffer's 1-byte lane
//! i32 count_bytes4      ditto, 4-byte lane (its first i32 is the string count)
//! i32 count_bytes8      ditto, 8-byte lane
//! i32 count_types       offset of the type stream's end from the string table's start
//! u16 count_objects     allocation hint
//! u16 count_arrays      allocation hint
//! i32 size_uncompressed
//! i32 size_compressed
//! i32 count_blobs       number of binary blobs
//! i32 size_blobs        total blob bytes
//! i32 count_bytes2      2-byte lane (version >= 4)
//! i32 size_frame_sizes  bytes of LZ4 blob frame lengths (version >= 4; **0** in every real
//!                       version-4 block that carries frames anyway)
//! ```
//!
//! The main buffer follows, then it is decompressed (one LZ4 block, a ZSTD frame group, or raw).
//! Its layout is `[byte lane][align 2][2-byte lane][align 4][4-byte lane][align 8][8-byte lane]
//! [align 8]`, then the null-terminated UTF-8 string table, then the type stream up to
//! `string_start + count_types`, then either the `0xFFEEDD00` trailer (no blobs) or
//! `[blob lengths][trailer][blob frame sizes]`. A lane's alignment applies only when the lane exists.
//!
//! A **ZSTD frame group** holds the buffer and its binary blobs as *concatenated* frames: a real
//! version-4 `PHYS` block is a frame of the buffer (2 MiB window, its content size is
//! `size_uncompressed`) followed by a frame of the blobs, so `size_uncompressed + size_blobs` bytes
//! come out of the group in total.
//!
//! # Binary blobs
//!
//! A blob value consumes the next entry of the blob-length table, and the blob payload follows the
//! buffer in the file (`[LZ4 frames][trailer]`, or one ZSTD frame, or the raw bytes). LZ4 stores
//! every blob as frames of at most 16 384 *decoded* bytes: the frame lengths are the `u16`s that
//! follow the `[blob lengths][trailer]` part of the buffer, and a frame may match up to 64 KiB back
//! into **everything decoded before it**, including the previous blobs (the frames chain into one
//! output stream). A `0xFFEEDD00` trailer follows the last frame in the file — the decoder consumes
//! the frames by their lengths and accepts the trailer as the only byte left after them (anything
//! else after an accounted payload is refused: `TrailingBytes`).
//!
//! # Layout (version 5)
//!
//! The same 72-byte header plus 12 `i32`s: `size_uncompressed/second buffer`,
//! `size_compressed_buffer1/2`, `count_bytes1/2/4/8_buffer2`, `count_nodes`,
//! `count_objects_buffer2`, `count_arrays_buffer2`, `count_array_elements`. Two buffers:
//!
//! * **buffer 1** — the string table: its 1-byte lane *is* the null-terminated string block and the
//!   4-byte lane starts (aligned to 4) with the string count. The remaining lanes 2/4/8 are the
//!   *auxiliary* value lanes read by `ARRAY_TYPE_AUXILIARY_BUFFER` elements.
//! * **buffer 2** — the values: `[object lengths][1-byte][align 2][2-byte][align 4][4-byte]
//!   [align 8][8-byte]`, then the type stream, then the trailer / blob tail. Object member counts
//!   come from the object-lengths lane instead of the 4-byte lane. The alignment is measured against
//!   the **start of buffer 2**, i.e. the object-length lane counts (a real `DATA` block with an odd
//!   object count keeps its 8-byte lane 4 bytes later than lane-relative alignment would put it —
//!   two of the 202 sampled models' `DATA` blocks are exactly that).
//!
//! The blobs of a version-5 block are their own frame group after buffer 2 (one ZSTD frame, or LZ4
//! frames, followed by the trailer; `size_frame_sizes` correctly reports `2 × frames` there).
//!
//! # Bounds
//!
//! Every count read from the file is checked against the bytes that remain (a lane read can never
//! index out of range), every declared size is checked against its buffer, decompressed output is
//! capped at [`MAX_DECOMPRESSED`], nesting at [`MAX_DEPTH`], values at [`MAX_NODES`], the string
//! table at [`MAX_STRINGS`] and a ZSTD window at [`ZSTD_WINDOW_CAP`]. `Lane` returns errors, never
//! panics; memory is allocated only after the length behind it was validated.

/// Decompressed KV3 buffer cap (per buffer). Real blocks decompress to at most a few hundred KiB.
pub const MAX_DECOMPRESSED: usize = 64 * 1024 * 1024;
/// Nesting cap. The deepest real `.vmdl_c` block of the sample nests 9 levels.
pub const MAX_DEPTH: usize = 64;
/// Total values (objects, arrays, scalars) per block. Real blocks hold at most 3322.
pub const MAX_NODES: usize = 1_000_000;
/// String-table entries per block. Real blocks hold at most a few thousand.
pub const MAX_STRINGS: usize = 1_000_000;

/// `0x4B563300`: the three `"VK"`/`'3'` bytes plus the version byte.
const MAGIC_PREFIX: u32 = 0x4B56_3300;
/// The legacy container magic, `VKV\x03` little-endian.
const MAGIC_LEGACY: u32 = 0x0356_4B56;
/// The end-of-buffer marker.
const TRAILER: u32 = 0xFFEE_DD00;
/// KV3's LZ4 frame size.
const LZ4_FRAME_SIZE: u16 = 16_384;
/// The LZ4 block format's match window: the longest distance a frame can reach back.
const LZ4_WINDOW: usize = 65_536;
/// Longest LZ4 frame chain a blob may use (a safety bound on the frame count).
const MAX_BLOB_FRAMES: usize = 1_000_000;
/// Cap on a ZSTD frame's window (the writer's level-3 frames declare 2 MiB).
const ZSTD_WINDOW_CAP: u64 = 64 * 1024 * 1024;

const T_NULL: u8 = 1;
const T_BOOL: u8 = 2;
const T_INT64: u8 = 3;
const T_UINT64: u8 = 4;
const T_DOUBLE: u8 = 5;
const T_STRING: u8 = 6;
const T_BLOB: u8 = 7;
const T_ARRAY: u8 = 8;
const T_OBJECT: u8 = 9;
const T_ARRAY_TYPED: u8 = 10;
const T_INT32: u8 = 11;
const T_UINT32: u8 = 12;
const T_BOOL_TRUE: u8 = 13;
const T_BOOL_FALSE: u8 = 14;
const T_INT64_ZERO: u8 = 15;
const T_INT64_ONE: u8 = 16;
const T_DOUBLE_ZERO: u8 = 17;
const T_DOUBLE_ONE: u8 = 18;
const T_FLOAT: u8 = 19;
const T_INT16: u8 = 20;
const T_UINT16: u8 = 21;
const T_INT8: u8 = 22;
const T_UINT8: u8 = 23;
const T_ARRAY_BYTE_LENGTH: u8 = 24;
const T_ARRAY_AUX_BUFFER: u8 = 25;
/// Constant `uint32` 0 / 1 without a lane read: the current resourcecompiler (build 2000933) writes
/// them in freshly compiled `MDAT`/`PHYS`/`ASEQ`/`AGRP`/`CTRL`/`RED2` blocks (shipped VPK models
/// predate them). Same numbers as ValveResourceFormat's `UINT32_ZERO`/`UINT32_ONE`.
const T_UINT32_ZERO: u8 = 26;
const T_UINT32_ONE: u8 = 27;

/// One KV3 value. Objects keep the file's member order; a repeated member name replaces the earlier
/// value in place.
#[derive(Debug, Clone, PartialEq)]
pub enum Kv3Value {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Double(f64),
    String(String),
    Blob(Vec<u8>),
    /// An untyped array: every element carries its own type byte.
    Array(Vec<Kv3Value>),
    /// A typed array: one type byte covers all elements (`ARRAY_TYPED`, `ARRAY_TYPE_BYTE_LENGTH`,
    /// `ARRAY_TYPE_AUXILIARY_BUFFER`).
    TypedArray(Vec<Kv3Value>),
    /// An object: members in file order.
    Object(Vec<(String, Kv3Value)>),
    /// A flagged value. The flag byte is kept verbatim; its mapping to KV3 flag *names*
    /// (`resource_name:` and friends) is not verified here, so callers get the number.
    Flagged {
        flag: u8,
        value: Box<Kv3Value>,
    },
}

impl Kv3Value {
    /// The value without its flag, if any.
    pub fn resolve(&self) -> &Kv3Value {
        match self {
            Kv3Value::Flagged { value, .. } => value.resolve(),
            other => other,
        }
    }

    /// The flag byte of this value, when it carries one.
    pub fn flag(&self) -> Option<u8> {
        match self {
            Kv3Value::Flagged { flag, .. } => Some(*flag),
            _ => None,
        }
    }

    /// The object's members (flags are looked through).
    pub fn as_object(&self) -> Option<&[(String, Kv3Value)]> {
        match self.resolve() {
            Kv3Value::Object(members) => Some(members),
            _ => None,
        }
    }

    /// Array elements (`Array` or `TypedArray`).
    pub fn as_array(&self) -> Option<&[Kv3Value]> {
        match self.resolve() {
            Kv3Value::Array(elements) | Kv3Value::TypedArray(elements) => Some(elements),
            _ => None,
        }
    }

    /// A string value. `None` when the value is not a string (an empty string is `Some("")`).
    pub fn as_str(&self) -> Option<&str> {
        match self.resolve() {
            Kv3Value::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self.resolve() {
            Kv3Value::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self.resolve() {
            Kv3Value::Int(value) => Some(*value),
            Kv3Value::UInt(value) => i64::try_from(*value).ok(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self.resolve() {
            Kv3Value::UInt(value) => Some(*value),
            Kv3Value::Int(value) => u64::try_from(*value).ok(),
            _ => None,
        }
    }

    /// Any numeric value as `f64` (int, uint or double).
    pub fn as_f64(&self) -> Option<f64> {
        match self.resolve() {
            Kv3Value::Double(value) => Some(*value),
            Kv3Value::Int(value) => Some(*value as f64),
            Kv3Value::UInt(value) => Some(*value as f64),
            _ => None,
        }
    }

    /// The value as `f32` (compiled `float` values arrive as doubles).
    pub fn as_f32(&self) -> Option<f32> {
        self.as_f64().map(|value| value as f32)
    }

    /// A three-component vector (KV3 stores vectors as arrays of three numbers).
    pub fn as_vec3(&self) -> Option<[f32; 3]> {
        let elements = self.as_array()?;
        if elements.len() != 3 {
            return None;
        }
        let mut out = [0.0f32; 3];
        for (slot, element) in out.iter_mut().zip(elements) {
            *slot = element.as_f32()?;
        }
        Some(out)
    }

    /// One object member by name.
    pub fn get(&self, key: &str) -> Option<&Kv3Value> {
        self.as_object()?
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    /// A member's string value.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    /// The blob bytes, when this value is a blob.
    pub fn as_blob(&self) -> Option<&[u8]> {
        match self.resolve() {
            Kv3Value::Blob(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// The value's kind as a word (`object`, `array`, `string`, …) — for error messages and reports.
    pub fn kind_name(&self) -> &'static str {
        match self.resolve() {
            Kv3Value::Null => "null",
            Kv3Value::Bool(_) => "bool",
            Kv3Value::Int(_) => "int",
            Kv3Value::UInt(_) => "uint",
            Kv3Value::Double(_) => "double",
            Kv3Value::String(_) => "string",
            Kv3Value::Blob(_) => "blob",
            Kv3Value::Array(_) => "array",
            Kv3Value::TypedArray(_) => "typed array",
            Kv3Value::Object(_) => "object",
            // `resolve` never returns a flagged value.
            Kv3Value::Flagged { .. } => "flagged",
        }
    }
}

/// A decoded KV3 block.
#[derive(Debug, Clone, PartialEq)]
pub struct Kv3Document {
    /// 4 or 5.
    pub version: u8,
    /// The format GUID in the file's byte order.
    pub format: [u8; 16],
    /// 0 none, 1 LZ4, 2 ZSTD.
    pub compression: u32,
    /// The root value (an object in every block of a compiled model).
    pub root: Kv3Value,
}

/// Compression methods (`KV3BinaryCompressionMethod`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    #[default]
    None,
    Lz4,
    Zstd,
}

impl Compression {
    fn from_u32(value: u32) -> Result<Self, Kv3Error> {
        match value {
            0 => Ok(Compression::None),
            1 => Ok(Compression::Lz4),
            2 => Ok(Compression::Zstd),
            other => Err(Kv3Error::UnsupportedCompression { method: other }),
        }
    }
}

/// Everything that can go wrong decoding a binary-KV3 block. `at` in [`Kv3Error::Malformed`] is an
/// offset inside the region the message names (a lane, the block, the type stream).
#[derive(Debug, thiserror::Error)]
pub enum Kv3Error {
    #[error("KV3 header truncated: {got} bytes, need at least {need}")]
    HeaderTooSmall { need: usize, got: usize },
    #[error("not a binary KV3 block: magic {magic:#010x}")]
    NotKv3 { magic: u32 },
    #[error("unsupported binary KV3 version {version} (magic {magic:#010x}); this reader handles 4 and 5")]
    UnsupportedVersion { magic: u32, version: u8 },
    #[error(
        "legacy VKV3 binary KV3 (magic {magic:#010x}) is not produced by the CS2 model compiler"
    )]
    LegacyVkv3 { magic: u32 },
    #[error("unsupported KV3 compression method {method} (0 none, 1 LZ4, 2 ZSTD)")]
    UnsupportedCompression { method: u32 },
    #[error("KV3 compression dictionary {id} is not supported")]
    CompressionDictionary { id: u16 },
    #[error("LZ4 frame size {found} is not {expected}")]
    CompressionFrameSize { found: u16, expected: u16 },
    #[error("malformed KV3 at {at}: {reason}")]
    Malformed { at: u64, reason: String },
    #[error("{what} is {declared} bytes, above the {cap}-byte cap")]
    DeclaredTooLarge {
        what: &'static str,
        declared: u64,
        cap: u64,
    },
    #[error("{method} decompression failed: {reason}")]
    DecompressFailed {
        method: &'static str,
        reason: String,
    },
    #[error("{method} decompressed to {actual} bytes, expected {expected}")]
    DecompressedSizeMismatch {
        method: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("KV3 end-of-buffer trailer is {found:#010x}, expected {expected:#010x}")]
    TrailerMismatch { found: u32, expected: u32 },
    #[error("{left} unconsumed bytes at the end of the KV3 buffer")]
    TrailingBytes { left: usize },
    #[error("KV3 nesting deeper than {limit}")]
    DepthLimit { limit: usize },
    #[error("more than {limit} KV3 values in one block")]
    NodeLimit { limit: usize },
    #[error("unknown KV3 value type {type_id}")]
    UnknownValueType { type_id: u8 },
    #[error("typed KV3 array with 0 elements (the writer never emits one)")]
    EmptyTypedArray,
}

/// Decode one binary-KV3 block (the block's whole body, e.g. the `DATA` block).
pub fn decode(data: &[u8]) -> Result<Kv3Document, Kv3Error> {
    let mut reader = Lane::new(data, "kv3 header");
    let magic = reader.u32()?;
    if magic == MAGIC_LEGACY {
        return Err(Kv3Error::LegacyVkv3 { magic });
    }
    if magic & 0xFFFF_FF00 != MAGIC_PREFIX {
        return Err(Kv3Error::NotKv3 { magic });
    }
    let version = (magic & 0xFF) as u8;
    if !(4..=5).contains(&version) {
        return Err(Kv3Error::UnsupportedVersion { magic, version });
    }
    // One length check before every lane read: a block shorter than its version's header is
    // reported as a truncated header, not as whichever lane read ran out first.
    let header_len = if version >= 5 { 120 } else { 72 };
    if data.len() < header_len {
        return Err(Kv3Error::HeaderTooSmall {
            need: header_len,
            got: data.len(),
        });
    }

    let mut format = [0u8; 16];
    format.copy_from_slice(reader.take(16)?);
    let compression = Compression::from_u32(reader.u32()?)?;
    let dict_id = reader.u16()?;
    let frame_size = reader.u16()?;
    if dict_id != 0 {
        return Err(Kv3Error::CompressionDictionary { id: dict_id });
    }
    if compression == Compression::Lz4 && frame_size != LZ4_FRAME_SIZE {
        return Err(Kv3Error::CompressionFrameSize {
            found: frame_size,
            expected: LZ4_FRAME_SIZE,
        });
    }

    let mut header = Header {
        compression,
        ..Header::default()
    };
    header.count_bytes1 = count(reader.i32()?, "1-byte lane")?;
    header.count_bytes4 = count(reader.i32()?, "4-byte lane")?;
    header.count_bytes8 = count(reader.i32()?, "8-byte lane")?;
    header.count_types = byte_len(reader.i32()?, "type stream")?;
    reader.u16()?; // object count (allocation hint, unused)
    reader.u16()?; // array count (allocation hint, unused)
    header.size_uncompressed = byte_len(reader.i32()?, "uncompressed buffer")?;
    header.size_compressed = count(reader.i32()?, "compressed buffer")?;
    header.count_blobs = count(reader.i32()?, "binary blob count")?;
    header.size_blobs = byte_len(reader.i32()?, "binary blobs")?;
    header.count_bytes2 = count(reader.i32()?, "2-byte lane")?;
    header.size_frame_sizes = count(reader.i32()?, "blob frame sizes")?;
    if version >= 5 {
        header.unc_buffer1 = byte_len(reader.i32()?, "uncompressed buffer 1")?;
        header.comp_buffer1 = count(reader.i32()?, "compressed buffer 1")?;
        header.unc_buffer2 = byte_len(reader.i32()?, "uncompressed buffer 2")?;
        header.comp_buffer2 = count(reader.i32()?, "compressed buffer 2")?;
        header.b2_count_bytes1 = count(reader.i32()?, "buffer 2 1-byte lane")?;
        header.b2_count_bytes2 = count(reader.i32()?, "buffer 2 2-byte lane")?;
        header.b2_count_bytes4 = count(reader.i32()?, "buffer 2 4-byte lane")?;
        header.b2_count_bytes8 = count(reader.i32()?, "buffer 2 8-byte lane")?;
        count(reader.i32()?, "node count (hint)")?; // allocation hint, unused
        header.b2_count_objects = count(reader.i32()?, "buffer 2 object count")?;
        count(reader.i32()?, "buffer 2 array count (hint)")?; // allocation hint, unused
        count(reader.i32()?, "array element count (hint)")?; // allocation hint, unused
        if header.unc_buffer1.checked_add(header.unc_buffer2) != Some(header.size_uncompressed) {
            return Err(malformed(
                72,
                &format!(
                    "buffer sizes {} + {} do not add up to {}",
                    header.unc_buffer1, header.unc_buffer2, header.size_uncompressed
                ),
            ));
        }
    }
    if header.size_uncompressed > MAX_DECOMPRESSED {
        return Err(Kv3Error::DeclaredTooLarge {
            what: "uncompressed KV3 buffer",
            declared: header.size_uncompressed as u64,
            cap: MAX_DECOMPRESSED as u64,
        });
    }
    if header.size_blobs > MAX_DECOMPRESSED {
        return Err(Kv3Error::DeclaredTooLarge {
            what: "binary blobs",
            declared: header.size_blobs as u64,
            cap: MAX_DECOMPRESSED as u64,
        });
    }

    let header_len = if version >= 5 { 120 } else { 72 };
    let payload = &data[header_len..]; // the length check at the top guarantees this
    let mut consumed = 0usize;

    // Decompress the buffer(s); `rest` is what follows them inside the block. A `rest` the
    // decoder does not otherwise use is the end of the block: the end-of-buffer trailer is all
    // it may carry (`rest_is_accounted`) — bytes past the accounted payload are never silently
    // dropped.
    let (buffer1, buffer2, rest, v4_blobs) = if version >= 5 {
        let (buffer1, rest) = take_payload(
            payload,
            header.comp_buffer1,
            header.unc_buffer1,
            compression,
            &mut consumed,
        )?;
        let (buffer2, rest) = take_payload(
            rest,
            header.comp_buffer2,
            header.unc_buffer2,
            compression,
            &mut consumed,
        )?;
        if header.count_blobs == 0 {
            rest_is_accounted(rest)?;
        }
        (buffer1, buffer2, rest, None)
    } else {
        // Before version 5 a ZSTD frame group holds the buffer *and* the blobs.
        let sized_with_blobs = compression == Compression::Zstd && header.count_blobs > 0;
        let uncompressed = if sized_with_blobs {
            header
                .size_uncompressed
                .checked_add(header.size_blobs)
                .ok_or_else(|| malformed(48, "uncompressed size overflow"))?
        } else {
            header.size_uncompressed
        };
        let (buffer, rest) = take_payload(
            payload,
            header.size_compressed,
            uncompressed,
            compression,
            &mut consumed,
        )?;
        if sized_with_blobs || header.count_blobs == 0 {
            // The frame group (or plain buffer) accounted for everything it holds.
            rest_is_accounted(rest)?;
        }
        let (main, blobs) = if sized_with_blobs {
            let (main, blobs) = buffer.split_at(header.size_uncompressed);
            (main.to_vec(), Some(blobs.to_vec()))
        } else {
            (buffer, None)
        };
        // For an uncompressed/LZ4 v4 block with blobs, `rest` IS the file-side blob payload;
        // its full consumption is checked where it is read (`split_v4` -> `read_blob_tail`).
        (main, Vec::new(), rest, blobs)
    };

    let mut parser = Parser {
        version,
        header: &header,
        types: &[],
        types_pos: 0,
        strings: Vec::new(),
        main: Buffers::default(),
        aux: Buffers::default(),
        object_lengths: Lane::empty(),
        blobs: BlobTable::default(),
        nodes: 0,
    };

    if version >= 5 {
        parser.split_v5(&buffer1, &buffer2, rest)?;
    } else {
        let blob_source = v4_blobs.as_deref().unwrap_or(rest);
        parser.split_v4(&buffer1, blob_source)?;
    }

    let root = parser.read_root()?;
    Ok(Kv3Document {
        version,
        format,
        compression: compression as u32,
        root,
    })
}

/// The KV3 header fields this reader uses (`count_*` hints are validated and dropped).
#[derive(Debug, Default, Clone, Copy)]
struct Header {
    compression: Compression,
    count_bytes1: usize,
    count_bytes2: usize,
    count_bytes4: usize,
    count_bytes8: usize,
    count_types: usize,
    size_uncompressed: usize,
    size_compressed: usize,
    count_blobs: usize,
    size_blobs: usize,
    size_frame_sizes: usize,
    unc_buffer1: usize,
    comp_buffer1: usize,
    unc_buffer2: usize,
    comp_buffer2: usize,
    b2_count_bytes1: usize,
    b2_count_bytes2: usize,
    b2_count_bytes4: usize,
    b2_count_bytes8: usize,
    b2_count_objects: usize,
}

/// A count that may not be negative.
fn count(value: i32, what: &'static str) -> Result<usize, Kv3Error> {
    usize::try_from(value).map_err(|_| malformed(0, &format!("negative {what} ({value})")))
}

/// A byte count that is also capped by [`MAX_DECOMPRESSED`].
fn byte_len(value: i32, what: &'static str) -> Result<usize, Kv3Error> {
    let size = count(value, what)?;
    if size > MAX_DECOMPRESSED {
        return Err(Kv3Error::DeclaredTooLarge {
            what,
            declared: size as u64,
            cap: MAX_DECOMPRESSED as u64,
        });
    }
    Ok(size)
}

fn malformed(at: u64, reason: &str) -> Kv3Error {
    Kv3Error::Malformed {
        at,
        reason: reason.to_string(),
    }
}

/// Bytes that remain where the decoder's frames/sizes should have accounted for everything:
/// the file's end-of-buffer trailer is all that may be there ([`TRAILER`], little-endian
/// `00 DD EE FF`; the writer puts it after a frame group or blob payload), anything else is a
/// `TrailingBytes` error — never bytes silently dropped.
fn rest_is_accounted(rest: &[u8]) -> Result<(), Kv3Error> {
    match rest {
        [] | [0x00, 0xDD, 0xEE, 0xFF] => Ok(()),
        rest => Err(Kv3Error::TrailingBytes { left: rest.len() }),
    }
}

/// Consume one compressed (or raw) payload: the decompressed bytes plus the bytes that follow it.
fn take_payload<'a>(
    payload: &'a [u8],
    compressed: usize,
    uncompressed: usize,
    compression: Compression,
    consumed: &mut usize,
) -> Result<(Vec<u8>, &'a [u8]), Kv3Error> {
    let taken = if compression == Compression::None {
        uncompressed
    } else {
        compressed
    };
    let input = payload.get(..taken).ok_or_else(|| {
        malformed(
            *consumed as u64,
            &format!(
                "payload of {taken} bytes does not fit the block ({} left)",
                payload.len()
            ),
        )
    })?;
    let output = inflate(input, uncompressed, compression)?;
    *consumed += taken;
    Ok((output, &payload[taken..]))
}

/// Decompress one whole buffer: a single LZ4 block, a ZSTD frame group, or raw bytes.
fn inflate(input: &[u8], size: usize, compression: Compression) -> Result<Vec<u8>, Kv3Error> {
    if size > MAX_DECOMPRESSED {
        return Err(Kv3Error::DeclaredTooLarge {
            what: "decompressed block",
            declared: size as u64,
            cap: MAX_DECOMPRESSED as u64,
        });
    }
    match compression {
        Compression::None => {
            if input.len() != size {
                return Err(Kv3Error::DecompressedSizeMismatch {
                    method: "uncompressed",
                    expected: size,
                    actual: input.len(),
                });
            }
            Ok(input.to_vec())
        }
        Compression::Lz4 => {
            let mut output = vec![0u8; size];
            let written =
                lz4_flex::block::decompress_into(input, &mut output).map_err(|error| {
                    Kv3Error::DecompressFailed {
                        method: "LZ4",
                        reason: error.to_string(),
                    }
                })?;
            if written != size {
                return Err(Kv3Error::DecompressedSizeMismatch {
                    method: "LZ4",
                    expected: size,
                    actual: written,
                });
            }
            Ok(output)
        }
        Compression::Zstd => zstd_frames(input, size),
    }
}

/// Decompress a ZSTD **frame group**: the writer stores the value buffer and (before version 5) its
/// binary blobs as *concatenated* frames in one region — a real `PHYS` v4 ZSTD block is a frame of
/// the buffer followed by a frame of the blobs, and the blobs of a real v5 block are one frame
/// followed by the end-of-buffer trailer. Reads exactly `size` bytes, block by block so no frame can
/// grow the buffer beyond one block, and refuses anything the frames do not account for.
fn zstd_frames(input: &[u8], size: usize) -> Result<Vec<u8>, Kv3Error> {
    use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};

    let failed = |reason: String| Kv3Error::DecompressFailed {
        method: "ZSTD",
        reason,
    };
    let mut source = input;
    let mut decoder = FrameDecoder::new();
    // The KV3 writer picks the window by compression *level*, not by the frame's content (a real
    // 17 KiB buffer frame declares a 2 MiB window), so the bound is a constant rather than `size`.
    // It keeps an untrusted frame from making the decoder allocate what it likes.
    decoder.set_max_window_size(ZSTD_WINDOW_CAP);
    let mut output = Vec::with_capacity(size);
    while output.len() < size {
        if source.is_empty() {
            break; // no further frame in the group
        }
        decoder
            .reset(&mut source)
            .map_err(|error| failed(error.to_string()))?;
        while !decoder.is_finished() {
            decoder
                .decode_blocks(&mut source, BlockDecodingStrategy::UptoBlocks(1))
                .map_err(|error| failed(error.to_string()))?;
            if let Some(chunk) = decoder.collect() {
                output.extend_from_slice(&chunk);
            }
            if output.len() > size {
                return Err(Kv3Error::DecompressedSizeMismatch {
                    method: "ZSTD",
                    expected: size,
                    actual: output.len(),
                });
            }
        }
    }
    if output.len() != size {
        return Err(Kv3Error::DecompressedSizeMismatch {
            method: "ZSTD",
            expected: size,
            actual: output.len(),
        });
    }
    // The writer's end-of-buffer trailer may follow the frames (it does after the blob frame of a
    // real v5 ZSTD block); nothing else may.
    let rest = &input[input.len() - source.len()..];
    let rest = match rest {
        [0x00, 0xDD, 0xEE, 0xFF, tail @ ..] => tail,
        rest => rest,
    };
    if !rest.is_empty() {
        return Err(Kv3Error::TrailingBytes { left: rest.len() });
    }
    Ok(output)
}

/// A cursor over one value lane. Every read is bounds-checked.
#[derive(Debug, Default, Clone, Copy)]
struct Lane<'a> {
    bytes: &'a [u8],
    pos: usize,
    what: &'static str,
}

impl<'a> Lane<'a> {
    fn new(bytes: &'a [u8], what: &'static str) -> Self {
        Lane {
            bytes,
            pos: 0,
            what,
        }
    }

    fn empty() -> Self {
        Lane::new(&[], "unused")
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Kv3Error> {
        let end = self.pos.checked_add(n).ok_or_else(|| self.overrun(n))?;
        if end > self.bytes.len() {
            return Err(self.overrun(n));
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, Kv3Error> {
        Ok(self.take(1)?[0])
    }

    fn i8(&mut self) -> Result<i8, Kv3Error> {
        Ok(self.u8()? as i8)
    }

    fn u16(&mut self) -> Result<u16, Kv3Error> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn i16(&mut self) -> Result<i16, Kv3Error> {
        Ok(self.u16()? as i16)
    }

    fn u32(&mut self) -> Result<u32, Kv3Error> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i32(&mut self) -> Result<i32, Kv3Error> {
        Ok(self.u32()? as i32)
    }

    fn f32(&mut self) -> Result<f32, Kv3Error> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn u64(&mut self) -> Result<u64, Kv3Error> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn i64(&mut self) -> Result<i64, Kv3Error> {
        Ok(self.u64()? as i64)
    }

    fn f64(&mut self) -> Result<f64, Kv3Error> {
        Ok(f64::from_bits(self.u64()?))
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn overrun(&self, n: usize) -> Kv3Error {
        malformed(
            self.pos as u64,
            &format!(
                "{} lane: need {n} bytes, {} left",
                self.what,
                self.remaining()
            ),
        )
    }
}

/// The four value lanes of one buffer.
#[derive(Debug, Default, Clone, Copy)]
struct Buffers<'a> {
    /// 1-byte values.
    b1: Lane<'a>,
    /// 2-byte values.
    b2: Lane<'a>,
    /// 4-byte values (also string ids, array lengths and — before version 5 — object lengths).
    b4: Lane<'a>,
    /// 8-byte values.
    b8: Lane<'a>,
}

/// Take one lane out of a buffer, aligning the offset only when the lane is present (exactly what
/// the writer does and what the reference reader expects).
fn lane_at<'a>(
    raw: &'a [u8],
    offset: &mut usize,
    len: usize,
    align_to: usize,
    unconditional_align: bool,
    what: &'static str,
) -> Result<Lane<'a>, Kv3Error> {
    if len > 0 || unconditional_align {
        *offset = align(*offset, align_to);
    }
    let end = offset
        .checked_add(len)
        .ok_or_else(|| malformed(*offset as u64, &format!("{what} lane length overflow")))?;
    let bytes = raw
        .get(*offset..end)
        .ok_or_else(|| malformed(*offset as u64, &format!("{what} lane past the buffer")))?;
    *offset = end;
    Ok(Lane::new(bytes, what))
}

impl<'a> Buffers<'a> {
    /// Split `raw` into the four lanes, starting the lane layout at `start` (version 5 measures it
    /// from the beginning of buffer 2, i.e. *past* the object-length lane: the writer aligns the
    /// 8-byte lane against the buffer, not against the lanes). Returns the lanes and the offset
    /// after them.
    fn split(
        raw: &'a [u8],
        start: usize,
        b1: usize,
        b2: usize,
        b4: usize,
        b8: usize,
        unconditional_b8_align: bool,
    ) -> Result<(Self, usize), Kv3Error> {
        let mut offset = start;
        let lane1 = lane_at(raw, &mut offset, b1, 1, false, "1-byte")?;
        let lane2 = lane_at(raw, &mut offset, b2 * 2, 2, false, "2-byte")?;
        let lane4 = lane_at(raw, &mut offset, b4 * 4, 4, false, "4-byte")?;
        let lane8 = lane_at(
            raw,
            &mut offset,
            b8 * 8,
            8,
            unconditional_b8_align,
            "8-byte",
        )?;
        Ok((
            Buffers {
                b1: lane1,
                b2: lane2,
                b4: lane4,
                b8: lane8,
            },
            offset,
        ))
    }
}

fn align(offset: usize, alignment: usize) -> usize {
    (offset + alignment - 1) & !(alignment - 1)
}

/// The binary blobs of a block, handed out in the order the values ask for them.
#[derive(Debug, Default)]
struct BlobTable {
    lengths: Vec<usize>,
    /// Next length slot (a blob value takes them in order).
    next: usize,
    data: Vec<u8>,
    /// Byte cursor into `data`.
    pos: usize,
}

impl BlobTable {
    fn next_length(&mut self) -> Result<usize, Kv3Error> {
        let length = self
            .lengths
            .get(self.next)
            .copied()
            .ok_or_else(|| malformed(0, "more blob values than the header declares"))?;
        self.next += 1;
        Ok(length)
    }

    fn take(&mut self, length: usize) -> Result<Vec<u8>, Kv3Error> {
        let end = self
            .pos
            .checked_add(length)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| malformed(self.pos as u64, "blob value past the blob payload"))?;
        let bytes = self.data[self.pos..end].to_vec();
        self.pos = end;
        Ok(bytes)
    }

    /// Every declared blob must have been consumed by a value.
    fn finish(&self) -> Result<(), Kv3Error> {
        if self.next != self.lengths.len() || self.pos != self.data.len() {
            return Err(malformed(
                self.pos as u64,
                &format!(
                    "{} of {} declared blobs used, {} of {} blob bytes read",
                    self.next,
                    self.lengths.len(),
                    self.pos,
                    self.data.len()
                ),
            ));
        }
        Ok(())
    }
}

/// The KV3 value decoder: lanes, string table, blobs and the type stream.
#[derive(Debug)]
struct Parser<'a> {
    version: u8,
    header: &'a Header,
    types: &'a [u8],
    types_pos: usize,
    strings: Vec<String>,
    /// Values of the main buffer.
    main: Buffers<'a>,
    /// Values of the auxiliary buffer (version 5: buffer 1's lanes; before: the main buffer).
    aux: Buffers<'a>,
    object_lengths: Lane<'a>,
    blobs: BlobTable,
    nodes: usize,
}

impl<'a> Parser<'a> {
    /// Version 4: one buffer plus the blob payload that follows it in the block.
    fn split_v4(&mut self, raw: &'a [u8], blob_source: &'a [u8]) -> Result<(), Kv3Error> {
        let header = *self.header;
        let (buffers, mut offset) = Buffers::split(
            raw,
            0,
            header.count_bytes1,
            header.count_bytes2,
            header.count_bytes4,
            header.count_bytes8,
            true,
        )?;
        self.main = buffers;
        self.aux = buffers;

        // The string table: the first integer of the 4-byte lane, then the bytes after the lanes.
        let count = self.main.b4.i32()?;
        let count = self.string_count(count)?;
        let strings_start = offset;
        for _ in 0..count {
            let rest = raw
                .get(offset..)
                .ok_or_else(|| malformed(offset as u64, "string table past the buffer"))?;
            let end = rest
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| malformed(offset as u64, "unterminated string in the table"))?;
            self.strings
                .push(String::from_utf8_lossy(&rest[..end]).into_owned());
            offset += end + 1;
        }

        // The type stream ends at `strings_start + count_types`.
        let types_end = strings_start
            .checked_add(header.count_types)
            .ok_or_else(|| malformed(offset as u64, "type stream length overflow"))?;
        self.types = raw
            .get(offset..types_end)
            .ok_or_else(|| malformed(offset as u64, "type stream past the buffer"))?;
        let tail = raw
            .get(types_end..)
            .ok_or_else(|| malformed(types_end as u64, "blob tail past the buffer"))?;
        if header.count_blobs == 0 {
            expect_trailer(tail, "v4")?;
        } else {
            self.read_blob_tail(tail, blob_source)?;
        }
        Ok(())
    }

    /// Version 5: buffer 1 holds the strings (and the auxiliary lanes), buffer 2 the values.
    fn split_v5(
        &mut self,
        buffer1: &'a [u8],
        buffer2: &'a [u8],
        blob_source: &'a [u8],
    ) -> Result<(), Kv3Error> {
        let header = *self.header;

        // --- buffer 1: the string table is the 1-byte lane; lanes 2/4/8 are the auxiliary lanes.
        let blob = buffer1
            .get(..header.count_bytes1)
            .ok_or_else(|| malformed(0, "v5 string block past buffer 1"))?;
        let count_pos = align(header.count_bytes1, 4);
        let count_bytes = buffer1
            .get(count_pos..count_pos + 4)
            .ok_or_else(|| malformed(count_pos as u64, "v5 string count past buffer 1"))?;
        let count = self.string_count(i32::from_le_bytes([
            count_bytes[0],
            count_bytes[1],
            count_bytes[2],
            count_bytes[3],
        ]))?;
        let mut offset = 0usize;
        for _ in 0..count {
            let rest = blob
                .get(offset..)
                .ok_or_else(|| malformed(offset as u64, "v5 string table past buffer 1"))?;
            let end = rest
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| malformed(offset as u64, "v5 unterminated string"))?;
            self.strings
                .push(String::from_utf8_lossy(&rest[..end]).into_owned());
            offset += end + 1;
        }
        if header.count_bytes4 == 0 {
            return Err(malformed(
                count_pos as u64,
                "v5 buffer 1 has no string count lane",
            ));
        }
        let lane4_end = count_pos
            .checked_add(header.count_bytes4 * 4)
            .ok_or_else(|| malformed(count_pos as u64, "v5 buffer 1 4-byte lane overflow"))?;
        let aux4 = buffer1.get(count_pos + 4..lane4_end).ok_or_else(|| {
            malformed(count_pos as u64, "v5 buffer 1 4-byte lane past the buffer")
        })?;
        // A lane that is absent occupies no bytes at all: it is not range-checked, exactly like the
        // writer that only emits it when it has values.
        let lane2 = if header.count_bytes2 > 0 {
            let start = align(header.count_bytes1, 2);
            buffer1
                .get(start..start + header.count_bytes2 * 2)
                .ok_or_else(|| malformed(start as u64, "v5 buffer 1 2-byte lane past the buffer"))?
        } else {
            &[][..]
        };
        let lane8 = if header.count_bytes8 > 0 {
            let start = align(lane4_end, 8);
            buffer1
                .get(start..start + header.count_bytes8 * 8)
                .ok_or_else(|| malformed(start as u64, "v5 buffer 1 8-byte lane past the buffer"))?
        } else {
            &[][..]
        };
        self.aux = Buffers {
            b1: Lane::new(&blob[offset..], "aux 1-byte"),
            b2: Lane::new(lane2, "aux 2-byte"),
            b4: Lane::new(aux4, "aux 4-byte"),
            b8: Lane::new(lane8, "aux 8-byte"),
        };

        // --- buffer 2: object lengths, value lanes, type stream, then the trailer / blob tail.
        let object_bytes = header
            .b2_count_objects
            .checked_mul(4)
            .ok_or_else(|| malformed(0, "v5 object-length lane overflow"))?;
        let objects = buffer2
            .get(..object_bytes)
            .ok_or_else(|| malformed(0, "v5 object-length lane past buffer 2"))?;
        self.object_lengths = Lane::new(objects, "object lengths");
        let (buffers, offset) = Buffers::split(
            buffer2,
            object_bytes,
            header.b2_count_bytes1,
            header.b2_count_bytes2,
            header.b2_count_bytes4,
            header.b2_count_bytes8,
            false,
        )?;
        self.main = buffers;
        let types_end = offset
            .checked_add(header.count_types)
            .ok_or_else(|| malformed(0, "v5 type stream overflow"))?;
        self.types = buffer2
            .get(offset..types_end)
            .ok_or_else(|| malformed(offset as u64, "v5 type stream past buffer 2"))?;
        let tail = buffer2
            .get(types_end..)
            .ok_or_else(|| malformed(types_end as u64, "v5 tail past buffer 2"))?;
        if header.count_blobs == 0 {
            expect_trailer(tail, "v5")?;
        } else {
            self.read_blob_tail(tail, blob_source)?;
        }
        Ok(())
    }

    /// A string count, bounded by the cap.
    fn string_count(&self, declared: i32) -> Result<usize, Kv3Error> {
        let count = count(declared, "string count")?;
        if count > MAX_STRINGS {
            return Err(Kv3Error::DeclaredTooLarge {
                what: "string table",
                declared: count as u64,
                cap: MAX_STRINGS as u64,
            });
        }
        Ok(count)
    }

    /// The tail after the type stream when the block carries binary blobs:
    /// `[blob lengths][trailer][LZ4 frame sizes]`, with the blob payload in `blob_source`.
    fn read_blob_tail(&mut self, tail: &'a [u8], blob_source: &'a [u8]) -> Result<(), Kv3Error> {
        let header = *self.header;
        if header.count_blobs > MAX_BLOB_FRAMES {
            return Err(Kv3Error::DeclaredTooLarge {
                what: "binary blob count",
                declared: header.count_blobs as u64,
                cap: MAX_BLOB_FRAMES as u64,
            });
        }
        let lengths_end = header
            .count_blobs
            .checked_mul(4)
            .ok_or_else(|| malformed(0, "blob length table overflow"))?;
        let lengths = tail
            .get(..lengths_end)
            .ok_or_else(|| malformed(0, "blob length table past the buffer"))?;
        let mut sum = 0usize;
        let mut blob_lengths = Vec::with_capacity(header.count_blobs.min(1024));
        for (index, chunk) in lengths.as_chunks::<4>().0.iter().enumerate() {
            let length = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            if length < 0 {
                return Err(malformed(index as u64 * 4, "negative blob length"));
            }
            let length = length as usize;
            sum = sum
                .checked_add(length)
                .ok_or_else(|| malformed(0, "blob length sum overflow"))?;
            blob_lengths.push(length);
        }
        if sum != header.size_blobs {
            return Err(malformed(
                lengths_end as u64,
                &format!(
                    "blob lengths add up to {sum}, header says {}",
                    header.size_blobs
                ),
            ));
        }
        let after_lengths = &tail[lengths_end..];
        // The writer puts the end-of-buffer trailer *before* the LZ4 frame sizes, so the trailer is
        // not the last thing in the buffer when the blobs are LZ4 frames. (Real `PHYS`/`ANIM`
        // blocks carry exactly this: `[blob lengths][trailer][u16 frame sizes]`.)
        let trailer = after_lengths
            .get(..4)
            .ok_or_else(|| malformed(lengths_end as u64, "blob tail trailer missing"))?;
        let found = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
        if found != TRAILER {
            return Err(Kv3Error::TrailerMismatch {
                found,
                expected: TRAILER,
            });
        }
        let frame_sizes = &after_lengths[4..];
        // The count is a hint: it is 0 in some real version-4 blocks that do carry frame sizes
        // (the frame walk below consumes them either way).
        if header.size_frame_sizes != 0 && header.size_frame_sizes != frame_sizes.len() {
            return Err(malformed(
                (lengths_end + 4) as u64,
                &format!(
                    "{} bytes of LZ4 frame sizes, header says {}",
                    frame_sizes.len(),
                    header.size_frame_sizes
                ),
            ));
        }

        // A ZSTD frame group before version 5 was already decompressed with the buffer; its blob
        // bytes are the tail of what `blob_source` points at.
        let already_inflated = self.version < 5
            && header.compression == Compression::Zstd
            && blob_source.len() == header.size_blobs;
        let data = match header.compression {
            Compression::None => {
                if !frame_sizes.is_empty() {
                    return Err(malformed(0, "uncompressed blobs carry frame sizes"));
                }
                let taken = blob_source
                    .get(..header.size_blobs)
                    .ok_or_else(|| malformed(0, "blob payload past the block"))?;
                let leftover = &blob_source[header.size_blobs.min(blob_source.len())..];
                rest_is_accounted(leftover)?;
                taken.to_vec()
            }
            Compression::Lz4 => {
                inflate_blob_frames(blob_source, frame_sizes, &blob_lengths, header.size_blobs)?
            }
            Compression::Zstd if already_inflated => {
                if !frame_sizes.is_empty() {
                    return Err(malformed(0, "ZSTD blobs carry LZ4 frame sizes"));
                }
                blob_source.to_vec()
            }
            Compression::Zstd => {
                if !frame_sizes.is_empty() {
                    return Err(malformed(0, "ZSTD blobs carry LZ4 frame sizes"));
                }
                inflate(blob_source, header.size_blobs, Compression::Zstd)?
            }
        };
        self.blobs.lengths = blob_lengths;
        self.blobs.data = data;
        Ok(())
    }

    // ---------------------------------------------------------------- value decoding

    fn read_root(&mut self) -> Result<Kv3Value, Kv3Error> {
        let (type_id, flag) = self.read_type()?;
        let value = self.read_value(type_id, false, 0)?;
        if self.types_pos != self.types.len() {
            return Err(Kv3Error::TrailingBytes {
                left: self.types.len() - self.types_pos,
            });
        }
        self.blobs.finish()?;
        Ok(wrap_flag(flag, value))
    }

    fn read_type(&mut self) -> Result<(u8, Option<u8>), Kv3Error> {
        let byte = self.next_type_byte()?;
        let mut flag = None;
        if byte & 0x80 != 0 {
            flag = Some(self.next_type_byte()?);
        }
        if byte & 0x40 != 0 {
            // No known writer sets this bit; every reader skips the byte that follows it.
            self.next_type_byte()?;
        }
        Ok((byte & 0x3F, flag))
    }

    fn next_type_byte(&mut self) -> Result<u8, Kv3Error> {
        let byte = *self
            .types
            .get(self.types_pos)
            .ok_or_else(|| malformed(self.types_pos as u64, "type stream exhausted"))?;
        self.types_pos += 1;
        Ok(byte)
    }

    /// The lanes of the value buffer (`aux` selects the auxiliary buffer).
    fn lanes(&mut self, aux: bool) -> &mut Buffers<'a> {
        if aux {
            &mut self.aux
        } else {
            &mut self.main
        }
    }

    fn bump_nodes(&mut self) -> Result<(), Kv3Error> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(Kv3Error::NodeLimit { limit: MAX_NODES });
        }
        Ok(())
    }

    fn string_of(&self, id: i32) -> Result<String, Kv3Error> {
        if id == -1 {
            // The writer's id for the empty string.
            return Ok(String::new());
        }
        let index = usize::try_from(id).map_err(|_| Kv3Error::Malformed {
            at: id as u64,
            reason: format!("negative string id {id} (only -1, the empty string, is legal)"),
        })?;
        self.strings
            .get(index)
            .cloned()
            .ok_or_else(|| Kv3Error::Malformed {
                at: index as u64,
                reason: format!(
                    "string id {index} is out of range ({} strings)",
                    self.strings.len()
                ),
            })
    }

    fn read_value(&mut self, type_id: u8, aux: bool, depth: usize) -> Result<Kv3Value, Kv3Error> {
        if depth > MAX_DEPTH {
            return Err(Kv3Error::DepthLimit { limit: MAX_DEPTH });
        }
        self.bump_nodes()?;
        let value = match type_id {
            T_NULL => Kv3Value::Null,
            T_BOOL => Kv3Value::Bool(self.lanes(aux).b1.u8()? != 0),
            T_INT64 => Kv3Value::Int(self.lanes(aux).b8.i64()?),
            T_UINT64 => Kv3Value::UInt(self.lanes(aux).b8.u64()?),
            T_DOUBLE => Kv3Value::Double(self.lanes(aux).b8.f64()?),
            T_STRING => {
                let id = self.main.b4.i32()?;
                Kv3Value::String(self.string_of(id)?)
            }
            T_BLOB => {
                let length = self.blobs.next_length()?;
                Kv3Value::Blob(self.blobs.take(length)?)
            }
            T_ARRAY => {
                // An untyped array: every element carries its own type byte.
                let count = self.array_count()?;
                self.check_array_len(count)?;
                Kv3Value::Array(self.read_elements(count, false, depth)?)
            }
            T_ARRAY_TYPED => {
                let count = Self::typed_array_count(self.main.b4.i32()?)?;
                if count == 0 {
                    return Err(Kv3Error::EmptyTypedArray);
                }
                Kv3Value::TypedArray(self.read_typed_elements(count, false, depth)?)
            }
            T_ARRAY_BYTE_LENGTH => {
                let count = self.main.b1.u8()? as usize;
                Kv3Value::TypedArray(self.read_typed_elements(count, false, depth)?)
            }
            T_ARRAY_AUX_BUFFER => {
                let count = self.main.b1.u8()? as usize;
                Kv3Value::TypedArray(self.read_typed_elements(count, true, depth)?)
            }
            T_OBJECT => {
                let count = self.object_count()?;
                Kv3Value::Object(self.read_members(count, depth)?)
            }
            T_INT32 => Kv3Value::Int(self.lanes(aux).b4.i32()? as i64),
            T_UINT32 => Kv3Value::UInt(self.lanes(aux).b4.u32()? as u64),
            T_BOOL_TRUE => Kv3Value::Bool(true),
            T_BOOL_FALSE => Kv3Value::Bool(false),
            T_INT64_ZERO => Kv3Value::Int(0),
            T_INT64_ONE => Kv3Value::Int(1),
            T_DOUBLE_ZERO => Kv3Value::Double(0.0),
            T_DOUBLE_ONE => Kv3Value::Double(1.0),
            T_UINT32_ZERO => Kv3Value::UInt(0),
            T_UINT32_ONE => Kv3Value::UInt(1),
            T_FLOAT => Kv3Value::Double(self.lanes(aux).b4.f32()? as f64),
            T_INT16 => Kv3Value::Int(self.lanes(aux).b2.i16()? as i64),
            T_UINT16 => Kv3Value::UInt(self.lanes(aux).b2.u16()? as u64),
            T_INT8 => Kv3Value::Int(self.lanes(aux).b1.i8()? as i64),
            T_UINT8 => Kv3Value::UInt(self.lanes(aux).b1.u8()? as u64),
            other => return Err(Kv3Error::UnknownValueType { type_id: other }),
        };
        Ok(value)
    }

    fn read_elements(
        &mut self,
        count: usize,
        aux: bool,
        depth: usize,
    ) -> Result<Vec<Kv3Value>, Kv3Error> {
        self.check_array_len(count)?;
        let mut elements = Vec::with_capacity(allocation_hint(count));
        for _ in 0..count {
            let (type_id, flag) = self.read_type()?;
            let value = self.read_value(type_id, aux, depth + 1)?;
            elements.push(wrap_flag(flag, value));
        }
        Ok(elements)
    }

    /// A typed array's elements: one subtype byte covers all of them (`ARRAY_TYPED`,
    /// `ARRAY_TYPE_BYTE_LENGTH`, `ARRAY_TYPE_AUXILIARY_BUFFER`).
    fn read_typed_elements(
        &mut self,
        count: usize,
        aux: bool,
        depth: usize,
    ) -> Result<Vec<Kv3Value>, Kv3Error> {
        let (type_id, flag) = self.read_type()?;
        let mut elements = Vec::with_capacity(allocation_hint(count));
        for _ in 0..count {
            let value = self.read_value(type_id, aux, depth + 1)?;
            elements.push(wrap_flag(flag, value));
        }
        Ok(elements)
    }

    fn read_members(
        &mut self,
        count: usize,
        depth: usize,
    ) -> Result<Vec<(String, Kv3Value)>, Kv3Error> {
        // Every member costs a type byte and an i32 name id.
        let types_left = self.types.len() - self.types_pos;
        let names_left = self.main.b4.remaining() / 4;
        if count > types_left || count > names_left {
            return Err(malformed(
                self.types_pos as u64,
                &format!(
                    "object of {count} members but only {types_left} type bytes and {names_left} name ids left"
                ),
            ));
        }
        let mut members: Vec<(String, Kv3Value)> = Vec::with_capacity(allocation_hint(count));
        // Dedup through a name -> slot map: a linear `find` per member is quadratic, and every
        // size cap allows an object with a million distinct names (a hostile block must not be
        // able to pin the decoding thread for hours).
        let mut slots: std::collections::HashMap<String, usize> =
            std::collections::HashMap::with_capacity(allocation_hint(count));
        for _ in 0..count {
            let (type_id, flag) = self.read_type()?;
            let name_id = self.main.b4.i32()?;
            let name = self.string_of(name_id)?;
            let value = self.read_value(type_id, false, depth + 1)?;
            let value = wrap_flag(flag, value);
            match slots.get(&name) {
                // A repeated member name replaces the earlier value and keeps its position.
                Some(&index) => members[index].1 = value,
                None => {
                    slots.insert(name.clone(), members.len());
                    members.push((name, value));
                }
            }
        }
        Ok(members)
    }

    fn check_array_len(&self, count: usize) -> Result<(), Kv3Error> {
        // Every element of an *untyped* array costs one type byte; that bounds the allocation.
        let types_left = self.types.len() - self.types_pos;
        if count > types_left {
            return Err(malformed(
                self.types_pos as u64,
                &format!("array of {count} elements but only {types_left} type bytes left"),
            ));
        }
        Ok(())
    }

    /// A typed array's element count: no per-element type bytes, so it is bounded by [`MAX_NODES`]
    /// only (the lane reads below fail on their own if the file is short).
    fn typed_array_count(count: i32) -> Result<usize, Kv3Error> {
        let count = usize::try_from(count)
            .map_err(|_| malformed(0, &format!("negative array length {count}")))?;
        if count > MAX_NODES {
            return Err(Kv3Error::NodeLimit { limit: MAX_NODES });
        }
        Ok(count)
    }

    fn array_count(&mut self) -> Result<usize, Kv3Error> {
        Self::typed_array_count(self.main.b4.i32()?)
    }

    fn object_count(&mut self) -> Result<usize, Kv3Error> {
        let count = if self.version >= 5 {
            self.object_lengths.i32()?
        } else {
            self.main.b4.i32()?
        };
        usize::try_from(count).map_err(|_| malformed(0, &format!("negative object length {count}")))
    }
}

/// A `Vec` capacity that never asks for more than a page-ish of pointers: a count read from a file
/// only ever sizes the allocation up to what a plausible block holds, and the real growth is driven
/// by the values that follow (bounded by [`MAX_NODES`]).
fn allocation_hint(count: usize) -> usize {
    count.min(4096)
}

fn wrap_flag(flag: Option<u8>, value: Kv3Value) -> Kv3Value {
    match flag {
        Some(flag) => Kv3Value::Flagged {
            flag,
            value: Box::new(value),
        },
        None => value,
    }
}

/// The end-of-buffer marker, which must be the last thing in the buffer.
fn expect_trailer(tail: &[u8], what: &str) -> Result<(), Kv3Error> {
    let bytes = tail
        .get(..4)
        .ok_or_else(|| malformed(0, &format!("{what} end-of-buffer trailer missing")))?;
    let found = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if found != TRAILER {
        return Err(Kv3Error::TrailerMismatch {
            found,
            expected: TRAILER,
        });
    }
    if tail.len() > 4 {
        return Err(Kv3Error::TrailingBytes {
            left: tail.len() - 4,
        });
    }
    Ok(())
}

/// LZ4 binary blobs: every blob is stored as frames of at most the LZ4 frame size, and the frames are
/// chained (a frame may match into the bytes decoded before it).
fn inflate_blob_frames(
    payload: &[u8],
    frame_sizes: &[u8],
    blob_lengths: &[usize],
    total: usize,
) -> Result<Vec<u8>, Kv3Error> {
    let mut out = vec![0u8; total];
    let mut sizes = frame_sizes;
    let mut payload_pos = 0usize;
    let mut written = 0usize;
    for length in blob_lengths {
        let blob_end = written + length;
        while written < blob_end {
            let header = sizes
                .get(..2)
                .ok_or_else(|| malformed(0, "LZ4 blob frame sizes exhausted"))?;
            let compressed = u16::from_le_bytes([header[0], header[1]]) as usize;
            sizes = &sizes[2..];
            if compressed == 0 {
                return Err(malformed(payload_pos as u64, "empty LZ4 blob frame"));
            }
            let frame = payload
                .get(payload_pos..payload_pos + compressed)
                .ok_or_else(|| malformed(payload_pos as u64, "LZ4 blob frame past the block"))?;
            payload_pos += compressed;
            let history_start = written.saturating_sub(LZ4_WINDOW);
            let (history, target) = out.split_at_mut(written);
            let decoded = lz4_flex::block::decompress_into_with_dict(
                frame,
                &mut target[..blob_end - written],
                &history[history_start..],
            )
            .map_err(|error| Kv3Error::DecompressFailed {
                method: "LZ4 (blob frame)",
                reason: error.to_string(),
            })?;
            if decoded == 0 {
                return Err(malformed(
                    written as u64,
                    "LZ4 blob frame decoded to nothing (no progress)",
                ));
            }
            written += decoded;
        }
        if written != blob_end {
            return Err(malformed(
                written as u64,
                &format!("blob decoded to {written} bytes, the header declares {length}"),
            ));
        }
    }
    if !sizes.is_empty() {
        return Err(Kv3Error::TrailingBytes { left: sizes.len() });
    }
    // The frames accounted for every payload byte but the file's end-of-buffer trailer.
    rest_is_accounted(&payload[payload_pos..])?;
    Ok(out)
}
