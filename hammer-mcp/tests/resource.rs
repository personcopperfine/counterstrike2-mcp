//! Tests for the compiled-resource library ([`hammer_mcp::resource`]): synthetic resource
//! containers and synthetic binary-KV3 blocks built here in the test, model facts extracted from a
//! synthetic model, the negative cases, and the `#[ignore]`d VPK corpus test.
//!
//! ```text
//! cargo test -p hammer-mcp --test resource
//! cargo test -p hammer-mcp --test resource -- --ignored     # needs the installed game tree
//! ```
//!
//! The encoder below doubles as a specification of the serialisation: every positive case is a round
//! trip `decode(encode(value)) == value`, for **both** KV3 versions (4 and 5) and **all three**
//! compressions (none, LZ4, ZSTD). ZSTD has no encoder in `ruzstd`, so the tests build a *raw-block*
//! frame by hand ([`zstd_raw`]); LZ4 uses `lz4_flex`'s block compressor.

use hammer_mcp::resource::kv3bin::{Compression, Kv3Error, Kv3Value, MAX_DEPTH};
use hammer_mcp::resource::{model_info_from_bytes, Resource, ResourceError};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// =============================================================================================
// A binary-KV3 encoder (test-only)
// =============================================================================================

/// The "generic" KV3 format GUID as the file stores it.
const GENERIC_FORMAT: [u8; 16] = [
    0x7C, 0x16, 0x12, 0x74, 0xE9, 0x06, 0x98, 0x46, 0xAF, 0xF2, 0xE6, 0x3E, 0xB5, 0x90, 0x37, 0xE7,
];
const TRAILER: u32 = 0xFFEE_DD00;
/// The writer's LZ4 frame size.
const FRAME_SIZE: usize = 16_384;
/// A ZSTD frame's raw blocks may not exceed 128 KiB (the spec's block maximum).
const ZSTD_BLOCK: usize = 128 * 1024;

/// The encoder's state: value lanes, type stream, string table, blobs — filled in decode order.
#[derive(Default)]
struct Encoder {
    version: u8,
    b1: Vec<u8>,
    b4: Vec<u8>,
    b8: Vec<u8>,
    /// Version 5 auxiliary lanes (they live in buffer 1).
    aux1: Vec<u8>,
    aux4: Vec<u8>,
    aux8: Vec<u8>,
    types: Vec<u8>,
    object_lengths: Vec<u8>,
    strings: Vec<String>,
    blob_lengths: Vec<usize>,
    blob_data: Vec<u8>,
    /// The LZ4 blob frames as the test hands them in. `lz4_flex` compresses a block without a
    /// dictionary, so a frame that matches into *earlier* blob bytes (what real `PHYS`/`ANIM`
    /// frames do) has to be written by hand; this hook carries it.
    blob_frames: Option<(Vec<u8>, Vec<u16>)>,
}

impl Encoder {
    fn new(version: u8) -> Self {
        // Before version 5 the 4-byte lane starts with the string count (patched in
        // `encode_encoder` once the table is known).
        let b4 = if version >= 5 {
            Vec::new()
        } else {
            vec![0, 0, 0, 0]
        };
        Encoder {
            version,
            b4,
            ..Encoder::default()
        }
    }

    fn type_byte(&mut self, type_id: u8) {
        self.types.push(type_id);
    }

    fn lane32(&mut self, aux: bool, value: i32) {
        let bytes = value.to_le_bytes();
        if aux && self.version >= 5 {
            self.aux4.extend_from_slice(&bytes);
        } else {
            self.b4.extend_from_slice(&bytes);
        }
    }

    fn lane64(&mut self, aux: bool, value: u64) {
        let bytes = value.to_le_bytes();
        if aux && self.version >= 5 {
            self.aux8.extend_from_slice(&bytes);
        } else {
            self.b8.extend_from_slice(&bytes);
        }
    }

    fn string_id(&mut self, text: &str) -> i32 {
        if text.is_empty() {
            return -1; // the writer's id for the empty string
        }
        match self.strings.iter().position(|existing| existing == text) {
            Some(index) => index as i32,
            None => {
                self.strings.push(text.to_owned());
                (self.strings.len() - 1) as i32
            }
        }
    }

    /// A value with its leading type byte.
    fn value(&mut self, value: &Kv3Value) {
        self.type_byte(self.type_of(value));
        self.body(value, false);
    }

    /// Everything a value carries besides its leading type byte (which the caller wrote).
    fn body(&mut self, value: &Kv3Value, aux: bool) {
        match value {
            // `null`, booleans and 0/1 have hardcoded type bytes: no lane bytes follow them.
            Kv3Value::Null
            | Kv3Value::Bool(_)
            | Kv3Value::Int(0)
            | Kv3Value::Int(1)
            | Kv3Value::UInt(0)
            | Kv3Value::UInt(1) => {}
            Kv3Value::Int(number) => match i32::try_from(*number) {
                Ok(number) => self.lane32(aux, number),
                Err(_) => self.lane64(aux, *number as u64),
            },
            Kv3Value::UInt(number) => match u32::try_from(*number) {
                Ok(number) => self.lane32(aux, number as i32),
                Err(_) => self.lane64(aux, *number),
            },
            Kv3Value::Double(number) => self.lane64(aux, number.to_bits()),
            Kv3Value::String(text) => {
                let id = self.string_id(text);
                self.lane32(false, id);
            }
            Kv3Value::Blob(bytes) => {
                self.blob_lengths.push(bytes.len());
                self.blob_data.extend_from_slice(bytes);
            }
            Kv3Value::Array(elements) => {
                let len = elements.len() as i32;
                self.lane32(false, len);
                for element in elements {
                    self.value(element);
                }
            }
            Kv3Value::TypedArray(elements) => {
                let len = elements.len() as i32;
                self.lane32(false, len);
                // One subtype byte covers every element; the elements carry no type byte. A
                // mixed uint array cannot use the 0/1 constant subtypes: it goes to the lane.
                let subtype = self.type_of(&elements[0]);
                if matches!(subtype, 26 | 27) && elements.iter().any(|e| self.type_of(e) != subtype) {
                    self.type_byte(12);
                    for element in elements {
                        let Kv3Value::UInt(number) = element else { panic!("mixed typed array") };
                        self.lane32(aux, *number as i32);
                    }
                } else {
                    self.type_byte(subtype);
                    for element in elements {
                        self.body(element, aux);
                    }
                }
            }
            Kv3Value::Object(members) => {
                let len = members.len() as i32;
                if self.version >= 5 {
                    self.object_lengths.extend_from_slice(&len.to_le_bytes());
                } else {
                    self.b4.extend_from_slice(&len.to_le_bytes());
                }
                for (name, member) in members {
                    let id = self.string_id(name);
                    self.type_byte(self.type_of(member));
                    self.lane32(false, id);
                    self.body(member, false);
                }
            }
            Kv3Value::Flagged { .. } => panic!("the tests never encode flagged values"),
        }
    }

    /// The type byte a value uses when it stands on its own.
    fn type_of(&self, value: &Kv3Value) -> u8 {
        match value {
            Kv3Value::Null => 1,
            Kv3Value::Bool(true) => 13,
            Kv3Value::Bool(false) => 14,
            Kv3Value::Int(0) => 15,
            Kv3Value::Int(1) => 16,
            Kv3Value::Int(number) if i32::try_from(*number).is_ok() => 11,
            Kv3Value::Int(_) => 3,
            // UINT32_ZERO / UINT32_ONE (current resourcecompiler output)
            Kv3Value::UInt(0) => 26,
            Kv3Value::UInt(1) => 27,
            Kv3Value::UInt(number) if u32::try_from(*number).is_ok() => 12,
            Kv3Value::UInt(_) => 4,
            Kv3Value::Double(_) => 5,
            Kv3Value::String(_) => 6,
            Kv3Value::Blob(_) => 7,
            Kv3Value::Array(_) => 8,
            Kv3Value::TypedArray(_) => 10,
            Kv3Value::Object(_) => 9,
            Kv3Value::Flagged { .. } => panic!("the tests never encode flagged values"),
        }
    }

    /// The string table as bytes (null-terminated UTF-8).
    fn string_blob(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for text in &self.strings {
            out.extend_from_slice(text.as_bytes());
            out.push(0);
        }
        out
    }

    /// The value buffer's lanes (`[b1][align 2][b4][align 8][b8]`); the 2-byte lane has no test user.
    /// Before version 5 the 8-byte alignment is applied even when the lane is absent. `prefix` is
    /// what precedes the lanes in the buffer (version 5: the object-length lane), because the writer
    /// aligns against the buffer, not against the lanes.
    fn lanes(&self, prefix: usize) -> Vec<u8> {
        let mut out = self.b1.clone();
        if !self.b4.is_empty() {
            out.resize(align(out.len(), 4), 0);
        }
        out.extend_from_slice(&self.b4);
        if !self.b8.is_empty() || self.version < 5 {
            let aligned = align(prefix + out.len(), 8) - prefix;
            out.resize(aligned, 0);
        }
        out.extend_from_slice(&self.b8);
        out
    }

    /// The blob tail: `[lengths][trailer][frame sizes]` (no blobs: just the trailer).
    fn tail(&self, frame_sizes: &[u16]) -> Vec<u8> {
        let mut out = Vec::new();
        for length in &self.blob_lengths {
            out.extend_from_slice(&(*length as i32).to_le_bytes());
        }
        out.extend_from_slice(&TRAILER.to_le_bytes());
        for size in frame_sizes {
            out.extend_from_slice(&size.to_le_bytes());
        }
        out
    }

    /// The blob payload as the file carries it, plus the `u16` LZ4 frame lengths that go into the
    /// buffer tail: LZ4 splits every blob into frames of at most [`FRAME_SIZE`] *decoded* bytes (the
    /// frames of one blob finish at the blob's end), ZSTD puts the whole blob area in one frame
    /// group, uncompressed stores the bytes as they are.
    fn blob_payload(&self, compression: Compression) -> (Vec<u8>, Vec<u16>) {
        if self.blob_lengths.is_empty() {
            return (Vec::new(), Vec::new());
        }
        if let Some((payload, frame_sizes)) = &self.blob_frames {
            return (payload.clone(), frame_sizes.clone());
        }
        match compression {
            Compression::Lz4 => {
                let mut payload = Vec::new();
                let mut frame_sizes = Vec::new();
                let mut start = 0usize;
                for length in &self.blob_lengths {
                    let blob = self.blob_data.get(start..start + *length).unwrap_or(&[]);
                    start += *length;
                    for chunk in blob.chunks(FRAME_SIZE) {
                        let frame = compress(chunk, compression);
                        frame_sizes.push(frame.len() as u16);
                        payload.extend_from_slice(&frame);
                    }
                }
                (payload, frame_sizes)
            }
            Compression::Zstd => (zstd_raw(&self.blob_data), Vec::new()),
            Compression::None => (self.blob_data.clone(), Vec::new()),
        }
    }
}

fn align(offset: usize, alignment: usize) -> usize {
    (offset + alignment - 1) & !(alignment - 1)
}

/// A raw-block ZSTD frame: magic, single-segment header with the content size, raw blocks of at
/// most [`ZSTD_BLOCK`] bytes (the last one flagged), no checksum. `ruzstd` decodes it; `ruzstd` has
/// no encoder, which is why the test builds this.
fn zstd_raw(content: &[u8]) -> Vec<u8> {
    let size = content.len();
    let (fcs_flag, fcs): (u8, Vec<u8>) = if size < 256 {
        (0, vec![size as u8])
    } else if size < 65_536 + 256 {
        (1, ((size - 256) as u16).to_le_bytes().to_vec())
    } else {
        (2, (size as u32).to_le_bytes().to_vec())
    };
    let mut out = Vec::new();
    out.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
    out.extend_from_slice(&[0b0010_0000 | (fcs_flag << 6)]); // single segment, no checksum
    out.extend_from_slice(&fcs);
    let mut blocks = content.chunks(ZSTD_BLOCK).peekable();
    while let Some(block) = blocks.next() {
        let last = u32::from(blocks.peek().is_none());
        let header = ((block.len() as u32) << 3) | last; // type 0 = raw, 24-bit block size
        out.extend_from_slice(&header.to_le_bytes()[..3]);
        out.extend_from_slice(block);
    }
    out
}

fn compress(input: &[u8], compression: Compression) -> Vec<u8> {
    match compression {
        Compression::None => input.to_vec(),
        Compression::Lz4 => lz4_flex::block::compress(input),
        Compression::Zstd => zstd_raw(input),
    }
}

/// The 72-byte KV3 header (plus the 12 version-5 fields) of a synthetic block.
#[derive(Default, Clone, Copy)]
struct Header {
    count_bytes1: usize,
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
    b2_count_bytes4: usize,
    b2_count_bytes8: usize,
    b2_count_objects: usize,
}

impl Header {
    fn write(&self, version: u8, compression: Compression, out: &mut Vec<u8>) {
        out.extend_from_slice(&(0x4B56_3300u32 | u32::from(version)).to_le_bytes());
        out.extend_from_slice(&GENERIC_FORMAT);
        out.extend_from_slice(&(compression as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // dictionary id
        let frame_size: u16 = if compression == Compression::Lz4 {
            FRAME_SIZE as u16
        } else {
            0
        };
        out.extend_from_slice(&frame_size.to_le_bytes());
        out.extend_from_slice(&(self.count_bytes1 as i32).to_le_bytes());
        out.extend_from_slice(&(self.count_bytes4 as i32).to_le_bytes());
        out.extend_from_slice(&(self.count_bytes8 as i32).to_le_bytes());
        out.extend_from_slice(&(self.count_types as i32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // object hint
        out.extend_from_slice(&0u16.to_le_bytes()); // array hint
        out.extend_from_slice(&(self.size_uncompressed as i32).to_le_bytes());
        out.extend_from_slice(&(self.size_compressed as i32).to_le_bytes());
        out.extend_from_slice(&(self.count_blobs as i32).to_le_bytes());
        out.extend_from_slice(&(self.size_blobs as i32).to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes()); // 2-byte lane (buffer 1; no test user)
        out.extend_from_slice(&(self.size_frame_sizes as i32).to_le_bytes());
        if version >= 5 {
            out.extend_from_slice(&(self.unc_buffer1 as i32).to_le_bytes());
            out.extend_from_slice(&(self.comp_buffer1 as i32).to_le_bytes());
            out.extend_from_slice(&(self.unc_buffer2 as i32).to_le_bytes());
            out.extend_from_slice(&(self.comp_buffer2 as i32).to_le_bytes());
            out.extend_from_slice(&(self.b2_count_bytes1 as i32).to_le_bytes());
            out.extend_from_slice(&0i32.to_le_bytes()); // 2-byte lane (buffer 2)
            out.extend_from_slice(&(self.b2_count_bytes4 as i32).to_le_bytes());
            out.extend_from_slice(&(self.b2_count_bytes8 as i32).to_le_bytes());
            out.extend_from_slice(&0i32.to_le_bytes()); // node hint
            out.extend_from_slice(&(self.b2_count_objects as i32).to_le_bytes());
            out.extend_from_slice(&0i32.to_le_bytes()); // array hint
            out.extend_from_slice(&0i32.to_le_bytes()); // element hint
        }
    }
}

/// Build a complete block: header + buffer(s) + blob payload.
fn encode_kv3(value: &Kv3Value, version: u8, compression: Compression) -> Vec<u8> {
    let mut encoder = Encoder::new(version);
    encoder.value(value);
    encode_encoder(encoder, version, compression)
}

fn encode_encoder(encoder: Encoder, version: u8, compression: Compression) -> Vec<u8> {
    let string_blob = encoder.string_blob();
    let (blob_payload, frame_sizes) = encoder.blob_payload(compression);
    let has_blobs = !encoder.blob_lengths.is_empty();
    let header;
    let mut out = Vec::new();

    if version >= 5 {
        // Buffer 1: the strings (its 1-byte lane), then the auxiliary lanes.
        let mut buffer1 = string_blob.clone();
        buffer1.extend_from_slice(&encoder.aux1);
        buffer1.resize(align(buffer1.len(), 4), 0);
        buffer1.extend_from_slice(&(encoder.strings.len() as i32).to_le_bytes());
        buffer1.extend_from_slice(&encoder.aux4);
        if !encoder.aux8.is_empty() {
            buffer1.resize(align(buffer1.len(), 8), 0);
        }
        buffer1.extend_from_slice(&encoder.aux8);

        // Buffer 2: object lengths, value lanes, types, tail.
        let mut buffer2 = encoder.object_lengths.clone();
        let lanes = encoder.lanes(encoder.object_lengths.len());
        buffer2.extend_from_slice(&lanes);
        buffer2.extend_from_slice(&encoder.types);
        let tail = encoder.tail(&frame_sizes);
        buffer2.extend_from_slice(&tail);

        let compressed1 = compress(&buffer1, compression);
        let compressed2 = compress(&buffer2, compression);
        let compressed = compression != Compression::None;
        header = Header {
            count_bytes1: string_blob.len() + encoder.aux1.len(),
            count_bytes4: 1 + encoder.aux4.len() / 4,
            count_bytes8: encoder.aux8.len() / 8,
            count_types: encoder.types.len(),
            size_uncompressed: buffer1.len() + buffer2.len(),
            size_compressed: compressed1.len() + compressed2.len(),
            count_blobs: encoder.blob_lengths.len(),
            size_blobs: encoder.blob_data.len(),
            size_frame_sizes: frame_sizes.len() * 2,
            unc_buffer1: buffer1.len(),
            comp_buffer1: if compressed { compressed1.len() } else { 0 },
            unc_buffer2: buffer2.len(),
            comp_buffer2: if compressed { compressed2.len() } else { 0 },
            b2_count_bytes1: encoder.b1.len(),
            b2_count_bytes4: encoder.b4.len() / 4,
            b2_count_bytes8: encoder.b8.len() / 8,
            b2_count_objects: encoder.object_lengths.len() / 4,
        };
        header.write(version, compression, &mut out);
        out.extend_from_slice(&compressed1);
        out.extend_from_slice(&compressed2);
    } else {
        let lanes = encoder.lanes(0);
        let mut buffer = lanes;
        // Version 4: the first int of the 4-byte lane is the string count.
        let b4_start = align(encoder.b1.len(), 4);
        buffer[b4_start..b4_start + 4]
            .copy_from_slice(&(encoder.strings.len() as i32).to_le_bytes());
        let strings_start = buffer.len();
        buffer.extend_from_slice(&string_blob);
        buffer.extend_from_slice(&encoder.types);
        let tail = encoder.tail(&frame_sizes);
        buffer.extend_from_slice(&tail);

        // Before version 5 a ZSTD frame group holds the buffer *and* the blobs (real block:
        // a `PHYS` v4 ZSTD block); for LZ4 the blobs follow the buffer separately.
        let zstd_with_blobs = compression == Compression::Zstd && has_blobs;
        let payload = if zstd_with_blobs {
            let mut framed = buffer.clone();
            framed.extend_from_slice(&encoder.blob_data);
            zstd_raw(&framed)
        } else {
            compress(&buffer, compression)
        };
        header = Header {
            count_bytes1: encoder.b1.len(),
            count_bytes4: encoder.b4.len() / 4,
            count_bytes8: encoder.b8.len() / 8,
            // The type stream ends `count_types` bytes after the string table starts.
            count_types: buffer.len() - tail.len() - strings_start,
            size_uncompressed: buffer.len(),
            size_compressed: payload.len(),
            count_blobs: encoder.blob_lengths.len(),
            size_blobs: encoder.blob_data.len(),
            size_frame_sizes: frame_sizes.len() * 2,
            ..Header::default()
        };
        header.write(version, compression, &mut out);
        out.extend_from_slice(&payload);
        if zstd_with_blobs {
            return out; // the blobs rode inside that frame
        }
    }
    out.extend_from_slice(&blob_payload);
    out
}

/// Build a resource container holding `(type, body)` blocks; bodies are 4-byte aligned.
fn container(blocks: &[([u8; 4], &[u8])]) -> Vec<u8> {
    let table_len = blocks.len() * 12;
    let mut entries = Vec::new();
    let mut bodies: Vec<u8> = Vec::new();
    for (index, (kind, body)) in blocks.iter().enumerate() {
        let field_pos = 20 + index * 12; // the offset field of table entry `index`
        let body_abs = 16 + table_len + bodies.len();
        entries.push((*kind, (body_abs - field_pos) as u32, body.len() as u32));
        bodies.extend_from_slice(body);
        bodies.resize(align(bodies.len(), 4), 0);
    }
    let size = 16 + table_len + bodies.len();
    let mut out = Vec::new();
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&12u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
    for (kind, rel, size) in entries {
        out.extend_from_slice(&kind);
        out.extend_from_slice(&rel.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
    }
    out.extend_from_slice(&bodies);
    out
}

fn decode(bytes: &[u8]) -> Result<Kv3Value, Kv3Error> {
    hammer_mcp::resource::decode_kv3(bytes).map(|document| document.root)
}

// ---------------------------------------------------------------------------------------------
// Value-tree helpers
// ---------------------------------------------------------------------------------------------

fn object(members: Vec<(&str, Kv3Value)>) -> Kv3Value {
    Kv3Value::Object(
        members
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
    )
}

fn text(value: &str) -> Kv3Value {
    Kv3Value::String(value.to_owned())
}

fn vec3(x: f32, y: f32, z: f32) -> Kv3Value {
    Kv3Value::TypedArray(vec![
        Kv3Value::Double(f64::from(x)),
        Kv3Value::Double(f64::from(y)),
        Kv3Value::Double(f64::from(z)),
    ])
}

fn strings(values: &[&str]) -> Kv3Value {
    Kv3Value::TypedArray(values.iter().map(|v| text(v)).collect())
}

/// A tree that uses every value kind the decoder supports, including a blob larger than one LZ4
/// frame (so the blob path exercises the frame chain).
fn kitchen_sink() -> Kv3Value {
    let blob: Vec<u8> = (0..40_000u32).map(|index| (index % 251) as u8).collect();
    object(vec![
        ("m_name", text("models/test/kitchen_sink.vmdl")),
        ("m_null", Kv3Value::Null),
        ("m_true", Kv3Value::Bool(true)),
        ("m_false", Kv3Value::Bool(false)),
        ("m_zero", Kv3Value::Int(0)),
        ("m_one", Kv3Value::Int(1)),
        ("m_int", Kv3Value::Int(-123_456)),
        ("m_big", Kv3Value::Int(-9_000_000_000)),
        ("m_uzero", Kv3Value::UInt(0)),
        ("m_uone", Kv3Value::UInt(1)),
        ("m_uint", Kv3Value::UInt(7)),
        ("m_biguint", Kv3Value::UInt(u64::MAX)),
        ("m_double", Kv3Value::Double(0.5)),
        ("m_empty_string", text("")),
        ("m_blob", Kv3Value::Blob(blob)),
        (
            "m_array",
            Kv3Value::Array(vec![Kv3Value::Int(1), text("mixed"), Kv3Value::Bool(false)]),
        ),
        ("m_vector", vec3(1.0, -2.5, 0.25)),
        ("m_empty_array", Kv3Value::Array(Vec::new())),
        (
            "m_nested",
            object(vec![
                ("m_hull_min", vec3(-8.0, -8.0, 0.0)),
                ("m_hull_max", vec3(8.0, 8.0, 72.0)),
                ("m_skins", strings(&["default", "tint"])),
            ]),
        ),
    ])
}

// =============================================================================================
// Positive cases: round trips
// =============================================================================================

#[test]
fn round_trip_v4_none() {
    let value = kitchen_sink();
    let encoded = encode_kv3(&value, 4, Compression::None);
    assert_eq!(decode(&encoded).expect("decode"), value);
}

#[test]
fn round_trip_v4_lz4() {
    let value = kitchen_sink();
    let encoded = encode_kv3(&value, 4, Compression::Lz4);
    assert_eq!(decode(&encoded).expect("decode"), value);
}

#[test]
fn round_trip_v4_zstd() {
    let value = kitchen_sink();
    let encoded = encode_kv3(&value, 4, Compression::Zstd);
    assert_eq!(decode(&encoded).expect("decode"), value);
}

#[test]
fn round_trip_v5_none() {
    let value = kitchen_sink();
    let encoded = encode_kv3(&value, 5, Compression::None);
    assert_eq!(decode(&encoded).expect("decode"), value);
}

#[test]
fn round_trip_v5_lz4() {
    let value = kitchen_sink();
    let encoded = encode_kv3(&value, 5, Compression::Lz4);
    assert_eq!(decode(&encoded).expect("decode"), value);
}

#[test]
fn round_trip_v5_zstd() {
    let value = kitchen_sink();
    let encoded = encode_kv3(&value, 5, Compression::Zstd);
    assert_eq!(decode(&encoded).expect("decode"), value);
}

/// A blob that spans several frames (40 000 bytes = 3 frames of 16384) must come back byte-exact.
#[test]
fn multi_frame_blob_survives() {
    let blob: Vec<u8> = (0..40_000u32).map(|index| (index % 97) as u8).collect();
    let value = object(vec![("m_blob", Kv3Value::Blob(blob.clone()))]);
    for version in [4u8, 5] {
        let encoded = encode_kv3(&value, version, Compression::Lz4);
        let decoded = decode(&encoded).expect("decode");
        assert_eq!(
            decoded.get("m_blob").and_then(Kv3Value::as_blob),
            Some(blob.as_slice()),
            "version {version}"
        );
    }
}

/// `ARRAY_TYPE_AUXILIARY_BUFFER` (25) reads its elements' primitives from the *auxiliary* buffer:
/// buffer 1 in version 5, the buffer itself before that. Both layouts are hand-built here.
#[test]
fn auxiliary_buffer_array_reads_its_own_lane() {
    let expected = object(vec![(
        "m_aux",
        Kv3Value::TypedArray(vec![
            Kv3Value::Double(1.5),
            Kv3Value::Double(2.5),
            Kv3Value::Double(3.5),
        ]),
    )]);
    for version in [4u8, 5] {
        let mut encoder = Encoder::new(version);
        encoder.string_id("m_aux");
        encoder.type_byte(9); // root object
        if version >= 5 {
            encoder
                .object_lengths
                .extend_from_slice(&1i32.to_le_bytes());
        } else {
            encoder.b4.extend_from_slice(&1i32.to_le_bytes());
        }
        encoder.type_byte(25); // auxiliary-buffer array
        encoder.lane32(false, 0); // the member's name id
        encoder.b1.push(3); // the array length (main 1-byte lane)
        encoder.type_byte(5); // subtype: double
        let mut aux = Vec::new();
        for number in [1.5f64, 2.5, 3.5] {
            aux.extend_from_slice(&number.to_le_bytes());
        }
        if version >= 5 {
            encoder.aux8 = aux;
        } else {
            encoder.b8 = aux;
        }
        let encoded = encode_encoder(encoder, version, Compression::None);
        assert_eq!(
            decode(&encoded).expect("decode"),
            expected,
            "version {version}"
        );
    }
}

/// The blob frames chain: a frame may match into the bytes decoded *before* it, **across** blob
/// boundaries (real `PHYS`/`ANIM` frames do). `lz4_flex` cannot compress against a dictionary, so
/// the two frames here are hand-written:
///
/// * blob 1: token `0x80` = 8 literals, no match — `ABCDEFGH`;
/// * blob 2: token `0x04` = 0 literals, match length 4 + 4, offset 8 → copies `ABCDEFGH` again,
///   then the empty final sequence (`0x00`) every LZ4 block ends with.
#[test]
fn blob_frames_chain_across_blob_boundaries() {
    let decoded = b"ABCDEFGH";
    let mut frames = vec![0x80];
    frames.extend_from_slice(decoded);
    frames.extend_from_slice(&[0x04, 0x08, 0x00, 0x00]); // match length 8 at offset 8, final token
    for version in [4u8, 5] {
        let mut encoder = Encoder::new(version);
        let value = object(vec![
            ("m_first", Kv3Value::Blob(decoded.to_vec())),
            ("m_second", Kv3Value::Blob(decoded.to_vec())),
        ]);
        encoder.value(&value);
        encoder.blob_lengths = vec![8, 8];
        encoder.blob_data = decoded.repeat(2);
        encoder.blob_frames = Some((frames.clone(), vec![9, 4]));
        let encoded = encode_encoder(encoder, version, Compression::Lz4);
        let root = decode(&encoded).expect("decode");
        assert_eq!(root, value, "version {version}");
        assert_eq!(
            root.get("m_second").and_then(Kv3Value::as_blob),
            Some(decoded.as_slice()),
            "the second frame matched into the first blob"
        );
    }
}

/// The blob tail's byte order, which real `PHYS`/`ANIM` blocks pin down: the end-of-buffer trailer
/// sits **before** the LZ4 frame sizes, not at the end of the buffer.
#[test]
fn blob_tail_is_lengths_then_trailer_then_frame_sizes() {
    let blob = vec![3u8; FRAME_SIZE + 8]; // two frames
    let value = object(vec![("m_blob", Kv3Value::Blob(blob.clone()))]);
    let encoded = encode_kv3(&value, 5, Compression::Lz4);
    let i32_at = |at: usize| i32::from_le_bytes(encoded[at..at + 4].try_into().unwrap());
    let (comp1, unc2, comp2) = (
        i32_at(76) as usize,
        i32_at(80) as usize,
        i32_at(84) as usize,
    );
    let buffer2 = &encoded[120 + comp1..120 + comp1 + comp2];
    let plain = lz4_flex::block::decompress(buffer2, unc2).expect("buffer 2");
    let tail = &plain[plain.len() - 12..];
    assert_eq!(tail[0..4], (blob.len() as i32).to_le_bytes(), "blob length");
    assert_eq!(tail[4..8], 0xFFEE_DD00u32.to_le_bytes(), "trailer");
    let frame_sizes = [
        u16::from_le_bytes([tail[8], tail[9]]),
        u16::from_le_bytes([tail[10], tail[11]]),
    ];
    assert!(frame_sizes.iter().all(|size| *size != 0), "{frame_sizes:?}");
    // The frames are the whole rest of the block.
    let payload = encoded.len() - (120 + comp1 + comp2);
    assert_eq!(
        usize::from(frame_sizes[0]) + usize::from(frame_sizes[1]),
        payload
    );
}

/// Before version 5 a ZSTD frame group holds the buffer *and* the blobs (real `PHYS` v4 ZSTD block),
/// so no separate blob payload follows the frame in the file.
#[test]
fn v4_zstd_blobs_ride_in_the_buffer_frame() {
    let blob = vec![5u8; 4_000];
    let value = object(vec![("m_blob", Kv3Value::Blob(blob))]);
    let encoded = encode_kv3(&value, 4, Compression::Zstd);
    let size_compressed = i32::from_le_bytes(encoded[52..56].try_into().unwrap()) as usize;
    assert_eq!(
        encoded.len(),
        72 + size_compressed,
        "the file is the header plus one frame group"
    );
    assert_eq!(decode(&encoded).expect("decode"), value);
}

// =============================================================================================
// The container
// =============================================================================================

#[test]
fn container_lists_blocks_and_slices_them() {
    let data = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), 4, Compression::Lz4);
    let bytes = container(&[(*b"DATA", data.as_slice()), (*b"MDAT", &[1, 2, 3, 4, 5])]);
    let resource = Resource::parse(&bytes).expect("parse");
    assert_eq!(resource.header_version, 12);
    assert_eq!(resource.version, 1);
    assert_eq!(resource.block_types(), vec!["DATA", "MDAT"]);
    let data_block = resource.blocks_of(b"DATA").next().expect("DATA");
    assert_eq!(resource.block_bytes(data_block).len(), data.len());
    let mdat = resource.blocks_of(b"MDAT").next().expect("MDAT");
    assert_eq!(resource.block_bytes(mdat), &[1, 2, 3, 4, 5]);
    assert!(resource.blocks_of(b"PHYS").next().is_none());
}

/// A real agent model has five MDAT blocks: the container must keep repeats in file order.
#[test]
fn repeated_blocks_are_kept_in_order() {
    let bytes = container(&[
        (*b"MDAT", &[1][..]),
        (*b"DATA", &[9][..]),
        (*b"MDAT", &[2][..]),
    ]);
    let resource = Resource::parse(&bytes).expect("parse");
    let mdat: Vec<usize> = resource
        .blocks_of(b"MDAT")
        .map(|block| resource.block_bytes(block)[0] as usize)
        .collect();
    assert_eq!(mdat, vec![1, 2]);
}

// =============================================================================================
// Model facts (synthetic)
// =============================================================================================

/// A synthetic model: DATA (hull zero as real models write it, skins, bodygroups, keyvalue text),
/// MDAT (render bounds + attachments), ASEQ (sequences), PHYS (hull bounds).
fn model_blocks(with_aseq: bool, with_phys: bool) -> Vec<(Vec<u8>, [u8; 4])> {
    let masks = Kv3Value::TypedArray(vec![
        Kv3Value::UInt(0b001),
        Kv3Value::UInt(0b010),
        Kv3Value::UInt(0b011),
    ]);
    let data = object(vec![
        ("m_name", text("models/test/prop.vmdl")),
        (
            "m_modelInfo",
            object(vec![
                ("m_vHullMin", vec3(0.0, 0.0, 0.0)),
                ("m_vHullMax", vec3(0.0, 0.0, 0.0)),
                ("m_vViewMin", vec3(0.0, 0.0, 0.0)),
                ("m_vViewMax", vec3(0.0, 0.0, 0.0)),
                ("m_flMass", Kv3Value::Double(0.0)),
                ("m_sSurfaceProperty", text("metal")),
                (
                    "m_keyValueText",
                    text(
                        "<!-- kv3 -->\n{\n\tprop_data = \n\t{\n\t\tbase = \"Metal.Small\"\n\t}\n}",
                    ),
                ),
            ]),
        ),
        (
            "m_materialGroups",
            Kv3Value::TypedArray(vec![
                object(vec![
                    ("m_name", text("default")),
                    ("m_materials", strings(&["a.vmat"])),
                ]),
                object(vec![
                    ("m_name", text("tint_clean")),
                    ("m_materials", strings(&["b.vmat"])),
                ]),
            ]),
        ),
        ("m_meshGroups", strings(&["bg_@0", "bg_@1"])),
        ("m_refMeshGroupMasks", masks),
        ("m_refPhysicsData", Kv3Value::Array(Vec::new())),
    ]);
    let data = encode_kv3(&data, 4, Compression::Lz4);

    let mdat = object(vec![
        (
            "m_sceneObjects",
            Kv3Value::TypedArray(vec![
                object(vec![
                    ("m_vMinBounds", vec3(-10.0, -20.0, 0.0)),
                    ("m_vMaxBounds", vec3(10.0, 20.0, 40.0)),
                ]),
                object(vec![
                    ("m_vMinBounds", vec3(-5.0, -30.0, -2.0)),
                    ("m_vMaxBounds", vec3(5.0, 0.0, 41.0)),
                ]),
            ]),
        ),
        (
            "m_attachments",
            Kv3Value::Array(vec![
                object(vec![
                    ("key", text("muzzle")),
                    ("value", object(vec![("m_name", text("muzzle"))])),
                ]),
                object(vec![("key", text("laser"))]),
            ]),
        ),
    ]);
    let mdat = encode_kv3(&mdat, 5, Compression::Lz4);

    let mut blocks = vec![(data, *b"DATA"), (mdat, *b"MDAT")];
    if with_aseq {
        let aseq = object(vec![(
            "m_localSequenceNameArray",
            strings(&["idle", "open", "idle"]),
        )]);
        blocks.push((encode_kv3(&aseq, 4, Compression::None), *b"ASEQ"));
    }
    if with_phys {
        let phys = object(vec![(
            "m_parts",
            Kv3Value::TypedArray(vec![object(vec![(
                "m_rnShape",
                object(vec![(
                    "m_hulls",
                    Kv3Value::TypedArray(vec![object(vec![(
                        "m_Hull",
                        object(vec![(
                            "m_Bounds",
                            object(vec![
                                ("m_vMinBounds", vec3(-11.0, -21.0, -1.0)),
                                ("m_vMaxBounds", vec3(11.0, 21.0, 41.0)),
                            ]),
                        )]),
                    )])]),
                )]),
            )])]),
        )]);
        blocks.push((encode_kv3(&phys, 5, Compression::Lz4), *b"PHYS"));
    }
    blocks
}

fn model_bytes(blocks: &[(Vec<u8>, [u8; 4])]) -> Vec<u8> {
    let refs: Vec<([u8; 4], &[u8])> = blocks
        .iter()
        .map(|(body, kind)| (*kind, body.as_slice()))
        .collect();
    container(&refs)
}

#[test]
fn model_info_reads_every_fact() {
    let bytes = model_bytes(&model_blocks(true, true));
    let info = model_info_from_bytes(&bytes).expect("model info");
    assert_eq!(info.name.as_deref(), Some("models/test/prop.vmdl"));
    assert_eq!(info.hull, None, "an all-zero m_modelInfo hull is not a box");
    assert_eq!(info.view, None);
    assert_eq!(info.mass, None);
    assert_eq!(info.surface_property.as_deref(), Some("metal"));
    assert!(info
        .keyvalues_text
        .as_deref()
        .unwrap()
        .contains("prop_data"));
    assert_eq!(info.skins, vec!["default", "tint_clean"]);
    assert_eq!(
        info.sequences,
        vec!["idle", "open"],
        "deduplicated, file order"
    );
    assert_eq!(info.attachments, vec!["muzzle", "laser"]);
    assert_eq!(info.has_physics, Some(true));
    // The MDAT boxes are unioned.
    assert_eq!(
        info.render_bounds,
        Some(([-10.0, -30.0, -2.0], [10.0, 20.0, 41.0]))
    );
    // The PHYS hull bound is the physics box.
    assert_eq!(
        info.physics_bounds,
        Some(([-11.0, -21.0, -1.0], [11.0, 21.0, 41.0]))
    );
    assert_eq!(info.blocks, vec!["DATA", "MDAT", "ASEQ", "PHYS"]);
    assert_eq!(info.bodygroups.len(), 2);
    let first = &info.bodygroups[0];
    assert_eq!(first.index, 0);
    assert_eq!(first.name, "bg_@0");
    assert_eq!(first.value, 1);
    assert_eq!(first.mesh_count, 2, "masks 001 and 011 have bit 0");
    assert_eq!(first.group.as_deref(), Some("bg"));
    assert_eq!(first.choice, None);
    let second = &info.bodygroups[1];
    assert_eq!(second.value, 2);
    assert_eq!(second.mesh_count, 2, "masks 010 and 011 have bit 1");
    // Honest notes for the zeroed boxes, the missing mass and the empty physics data list.
    assert!(
        info.notes
            .iter()
            .any(|note| note.contains("hull and view boxes are all zero")),
        "{:?}",
        info.notes
    );
    assert!(
        info.notes.iter().any(|note| note.contains("m_flMass")),
        "{:?}",
        info.notes
    );
}

#[test]
fn model_info_notes_missing_blocks() {
    let bytes = model_bytes(&model_blocks(false, false));
    let info = model_info_from_bytes(&bytes).expect("model info");
    assert_eq!(info.sequences, Vec::<String>::new());
    assert_eq!(info.physics_bounds, None);
    assert_eq!(info.has_physics, Some(false));
    assert!(
        info.notes.iter().any(|note| note.contains("no ASEQ block")),
        "{:?}",
        info.notes
    );
    assert!(
        info.notes.iter().any(|note| note.contains("no PHYS block")),
        "{:?}",
        info.notes
    );
}

/// A model whose `m_modelInfo` really carries a hull (the player models of the market build do).
#[test]
fn model_info_keeps_a_real_hull() {
    let data = object(vec![
        ("m_name", text("models/test/player.vmdl")),
        (
            "m_modelInfo",
            object(vec![
                ("m_vHullMin", vec3(-16.0, -16.0, 0.0)),
                ("m_vHullMax", vec3(16.0, 16.0, 72.0)),
                ("m_vViewMin", vec3(-16.0, -16.0, 0.0)),
                ("m_vViewMax", vec3(16.0, 16.0, 72.0)),
                ("m_flMass", Kv3Value::Double(85.0)),
            ]),
        ),
    ]);
    let bytes = model_bytes(&[(encode_kv3(&data, 5, Compression::Lz4), *b"DATA")]);
    let info = model_info_from_bytes(&bytes).expect("model info");
    assert_eq!(info.hull, Some(([-16.0, -16.0, 0.0], [16.0, 16.0, 72.0])));
    assert_eq!(info.view, Some(([-16.0, -16.0, 0.0], [16.0, 16.0, 72.0])));
    assert_eq!(info.mass, Some(85.0));
    assert!(
        !info.notes.iter().any(|note| note.contains("all zero")),
        "{:?}",
        info.notes
    );
}

/// Bodygroup names in the authored form split into group and choice; the mask bit is the group's
/// position in `m_meshGroups` (the agent models carry six such groups, masks `[3,1,4,12,32]`).
#[test]
fn bodygroup_names_split() {
    let data = object(vec![
        ("m_name", text("models/test/agent.vmdl")),
        (
            "m_meshGroups",
            strings(&[
                "first_or_third_person_@0_#&thirdperson_default",
                "defusekit_@1_#&defusekit_visible",
                "unlabelled",
            ]),
        ),
        (
            "m_refMeshGroupMasks",
            Kv3Value::TypedArray(vec![Kv3Value::UInt(0b101), Kv3Value::UInt(0b100)]),
        ),
    ]);
    let bytes = model_bytes(&[(encode_kv3(&data, 5, Compression::Lz4), *b"DATA")]);
    let info = model_info_from_bytes(&bytes).expect("model info");
    assert_eq!(info.bodygroups.len(), 3);
    let group = &info.bodygroups[0];
    assert_eq!(group.group.as_deref(), Some("first_or_third_person"));
    assert_eq!(group.choice.as_deref(), Some("thirdperson_default"));
    assert_eq!(group.value, 1);
    assert_eq!(group.mesh_count, 1);
    assert_eq!(info.bodygroups[1].value, 2);
    assert_eq!(info.bodygroups[1].mesh_count, 0, "no mask has bit 1");
    assert_eq!(
        info.bodygroups[2].group, None,
        "an unlabelled name does not split"
    );
    assert_eq!(info.bodygroups[2].value, 4);
    assert_eq!(info.bodygroups[2].mesh_count, 2);
}

// =============================================================================================
// Negative cases
// =============================================================================================

fn kv3_error(encoded: &[u8]) -> Kv3Error {
    decode(encoded).expect_err("expected a decode error")
}

fn resource_error(bytes: &[u8]) -> ResourceError {
    model_info_from_bytes(bytes).expect_err("expected an error")
}

#[test]
fn container_empty_is_an_error() {
    assert!(matches!(
        Resource::parse(&[]).expect_err("empty"),
        ResourceError::HeaderTooSmall { .. }
    ));
}

#[test]
fn container_truncated_header_is_an_error() {
    let bytes = [0u8; 12];
    assert!(matches!(
        Resource::parse(&bytes).expect_err("short"),
        ResourceError::HeaderTooSmall { .. }
    ));
}

#[test]
fn container_wrong_header_version_is_an_error() {
    let good = container(&[(*b"DATA", &[1, 2, 3, 4][..])]);
    let mut bad = good.clone();
    bad[4..6].copy_from_slice(&11u16.to_le_bytes());
    assert!(matches!(
        Resource::parse(&bad).expect_err("version"),
        ResourceError::UnsupportedHeaderVersion { found: 11, .. }
    ));
}

#[test]
fn container_file_size_mismatch_is_an_error() {
    let good = container(&[(*b"DATA", &[1, 2, 3, 4][..])]);
    let mut bad = good.clone();
    bad[0..4].copy_from_slice(&999u32.to_le_bytes());
    assert!(matches!(
        Resource::parse(&bad).expect_err("size"),
        ResourceError::FileSizeMismatch { declared: 999, .. }
    ));
}

#[test]
fn container_block_table_past_eof_is_an_error() {
    let good = container(&[(*b"DATA", &[1, 2, 3, 4][..])]);
    let mut bad = good.clone();
    bad[12..16].copy_from_slice(&100u32.to_le_bytes()); // block count
    assert!(matches!(
        Resource::parse(&bad).expect_err("table"),
        ResourceError::BlockTableOutOfBounds { .. }
    ));
}

#[test]
fn container_block_past_eof_is_an_error() {
    let good = container(&[(*b"DATA", &[1, 2, 3, 4][..])]);
    let mut bad = good.clone();
    let size_field = 16 + 8; // table entry: type (4), offset field (4), size field (4)
    bad[size_field..size_field + 4].copy_from_slice(&4096u32.to_le_bytes());
    assert!(matches!(
        Resource::parse(&bad).expect_err("block"),
        ResourceError::BlockOutOfBounds { .. }
    ));
}

#[test]
fn model_without_data_is_an_error() {
    let bytes = model_bytes(&[(
        encode_kv3(&object(vec![("k", Kv3Value::Int(2))]), 4, Compression::Lz4),
        *b"MDAT",
    )]);
    match resource_error(&bytes) {
        ResourceError::MissingBlock { kind, blocks } => {
            assert_eq!(kind, "DATA");
            assert_eq!(blocks, "MDAT");
        }
        other => panic!("expected MissingBlock, got {other:?}"),
    }
}

#[test]
fn model_with_a_non_object_data_root_is_an_error() {
    let bytes = model_bytes(&[(
        encode_kv3(&Kv3Value::Int(7), 4, Compression::None),
        *b"DATA",
    )]);
    assert!(matches!(
        resource_error(&bytes),
        ResourceError::UnexpectedValue { found: "int", .. }
    ));
}

#[test]
fn kv3_unknown_magic_is_an_error() {
    let mut encoded = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), 4, Compression::None);
    encoded[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
    assert!(matches!(kv3_error(&encoded), Kv3Error::NotKv3 { .. }));
}

#[test]
fn kv3_legacy_vkv3_is_an_error() {
    let mut encoded = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), 4, Compression::None);
    encoded[0..4].copy_from_slice(&0x0356_4B56u32.to_le_bytes());
    assert!(matches!(kv3_error(&encoded), Kv3Error::LegacyVkv3 { .. }));
}

#[test]
fn kv3_unsupported_version_is_an_error() {
    let mut encoded = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), 4, Compression::None);
    encoded[0..4].copy_from_slice(&0x4B56_3303u32.to_le_bytes()); // version 3
    match kv3_error(&encoded) {
        Kv3Error::UnsupportedVersion { version, .. } => assert_eq!(version, 3),
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}

#[test]
fn kv3_unsupported_compression_is_an_error() {
    let mut encoded = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), 4, Compression::None);
    encoded[20..24].copy_from_slice(&7u32.to_le_bytes());
    assert!(matches!(
        kv3_error(&encoded),
        Kv3Error::UnsupportedCompression { method: 7 }
    ));
}

#[test]
fn kv3_wrong_lz4_frame_size_is_an_error() {
    let mut encoded = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), 4, Compression::Lz4);
    encoded[26..28].copy_from_slice(&4096u16.to_le_bytes());
    assert!(matches!(
        kv3_error(&encoded),
        Kv3Error::CompressionFrameSize { found: 4096, .. }
    ));
}

/// The declared uncompressed size is a lie: it must be caught, not silently believed.
#[test]
fn kv3_declared_size_mismatch_is_an_error() {
    // `size_uncompressed` sits at offset 48; claim more than the buffer holds.
    let compressed = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), 4, Compression::Lz4);
    let mut encoded = compressed.clone();
    encoded[48..52].copy_from_slice(&(compressed.len() as i32 + 100).to_le_bytes());
    match kv3_error(&encoded) {
        Kv3Error::DecompressedSizeMismatch { .. } => {}
        other => panic!("expected DecompressedSizeMismatch, got {other:?}"),
    }
    // An uncompressed block compares the two sizes directly.
    let mut encoded = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), 4, Compression::None);
    let declared = encoded.len() as i32 + 100;
    encoded[48..52].copy_from_slice(&declared.to_le_bytes());
    match kv3_error(&encoded) {
        Kv3Error::DecompressedSizeMismatch { .. } | Kv3Error::Malformed { .. } => {}
        other => panic!("expected a size error, got {other:?}"),
    }
}

/// A declaration above the 64 MiB cap is refused before anything is allocated.
#[test]
fn kv3_oversized_declaration_is_an_error() {
    let mut encoded = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), 4, Compression::Lz4);
    encoded[48..52].copy_from_slice(&(200 * 1024 * 1024i32).to_le_bytes());
    match kv3_error(&encoded) {
        Kv3Error::DeclaredTooLarge { declared, cap, .. } => {
            assert_eq!(declared, 200 * 1024 * 1024);
            assert_eq!(cap as usize, hammer_mcp::resource::kv3bin::MAX_DECOMPRESSED);
        }
        other => panic!("expected DeclaredTooLarge, got {other:?}"),
    }
}

#[test]
fn kv3_negative_count_is_an_error() {
    let mut encoded = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), 4, Compression::None);
    encoded[28..32].copy_from_slice(&(-4i32).to_le_bytes()); // count_bytes1
    assert!(matches!(kv3_error(&encoded), Kv3Error::Malformed { .. }));
}

#[test]
fn kv3_wrong_trailer_is_an_error() {
    let mut encoded = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), 4, Compression::None);
    let position = encoded.len() - 4;
    encoded[position..].copy_from_slice(&0u32.to_le_bytes());
    assert!(matches!(
        kv3_error(&encoded),
        Kv3Error::TrailerMismatch { found: 0, .. }
    ));
}

/// A deep nesting bomb stops at the depth cap instead of blowing the stack.
#[test]
fn kv3_depth_bomb_is_an_error() {
    let mut value = object(vec![("k", Kv3Value::Int(3))]);
    for _ in 0..(MAX_DEPTH + 8) {
        value = object(vec![("m_nested", value)]);
    }
    let encoded = encode_kv3(&value, 4, Compression::None);
    assert!(matches!(kv3_error(&encoded), Kv3Error::DepthLimit { limit } if limit == MAX_DEPTH));
}

#[test]
fn kv3_empty_typed_array_is_an_error() {
    // Hand-built: a typed array with length 0 (the writer never emits one).
    let mut encoder = Encoder::new(4);
    encoder.type_byte(10);
    encoder.lane32(false, 0);
    encoder.type_byte(5);
    let encoded = encode_encoder(encoder, 4, Compression::None);
    assert!(matches!(kv3_error(&encoded), Kv3Error::EmptyTypedArray));
}

#[test]
fn kv3_string_id_out_of_range_is_an_error() {
    let mut encoder = Encoder::new(4);
    encoder.type_byte(6);
    encoder.lane32(false, 99); // no such string
    let encoded = encode_encoder(encoder, 4, Compression::None);
    assert!(matches!(kv3_error(&encoded), Kv3Error::Malformed { .. }));
}

#[test]
fn kv3_array_longer_than_the_type_stream_is_an_error() {
    let mut encoder = Encoder::new(4);
    encoder.type_byte(8); // array
    encoder.lane32(false, 1000); // 1000 elements, but no type bytes follow
    let encoded = encode_encoder(encoder, 4, Compression::None);
    assert!(matches!(kv3_error(&encoded), Kv3Error::Malformed { .. }));
}

#[test]
fn kv3_unknown_value_type_is_an_error() {
    let mut encoder = Encoder::new(4);
    encoder.type_byte(40);
    let encoded = encode_encoder(encoder, 4, Compression::None);
    assert!(matches!(
        kv3_error(&encoded),
        Kv3Error::UnknownValueType { type_id: 40 }
    ));
}

#[test]
fn kv3_blob_length_mismatch_is_an_error() {
    let mut encoder = Encoder::new(4);
    encoder.type_byte(7);
    encoder.blob_lengths.push(8);
    encoder.blob_data.extend_from_slice(&[0u8; 4]); // the payload is shorter than declared
    let encoded = encode_encoder(encoder, 4, Compression::None);
    assert!(matches!(kv3_error(&encoded), Kv3Error::Malformed { .. }));
}

#[test]
fn kv3_unused_blob_is_an_error() {
    // The header declares a blob that no value consumes: a decode must not report success.
    let mut encoder = Encoder::new(4);
    encoder.value(&object(vec![("k", Kv3Value::Int(3))]));
    encoder.blob_lengths.push(2);
    encoder.blob_data.extend_from_slice(&[1, 2]);
    let encoded = encode_encoder(encoder, 4, Compression::None);
    assert!(matches!(kv3_error(&encoded), Kv3Error::Malformed { .. }));
}

/// The frame-size count in the header is a claim about the tail: when it disagrees with the bytes
/// that really follow the trailer, the block is refused (a wrong tail would misplace every frame).
#[test]
fn kv3_frame_size_count_mismatch_is_an_error() {
    let value = object(vec![("m_blob", Kv3Value::Blob(vec![7u8; 40_000]))]); // 3 frames = 6 bytes
    let mut encoded = encode_kv3(&value, 5, Compression::Lz4);
    encoded[68..72].copy_from_slice(&8i32.to_le_bytes()); // size_frame_sizes
    assert!(matches!(kv3_error(&encoded), Kv3Error::Malformed { .. }));
}

/// LZ4 frame sizes belong to LZ4 only: an uncompressed or ZSTD block that carries them is refused
/// instead of having them silently ignored.
#[test]
fn kv3_frame_sizes_on_a_non_lz4_block_are_an_error() {
    for compression in [Compression::None, Compression::Zstd] {
        for version in [4u8, 5] {
            let mut encoder = Encoder::new(version);
            encoder.value(&object(vec![("m_blob", Kv3Value::Blob(vec![1, 2, 3, 4]))]));
            encoder.blob_lengths = vec![4];
            encoder.blob_data = vec![1, 2, 3, 4];
            encoder.blob_frames = Some((vec![1, 2, 3, 4], vec![4]));
            let encoded = encode_encoder(encoder, version, compression);
            assert!(
                matches!(kv3_error(&encoded), Kv3Error::Malformed { .. }),
                "{compression:?} version {version} accepted LZ4 frame sizes"
            );
        }
    }
}

#[test]
fn kv3_truncated_block_is_an_error() {
    let encoded = encode_kv3(&kitchen_sink(), 5, Compression::Lz4);
    let cut = &encoded[..encoded.len() - 8];
    assert!(
        matches!(
            kv3_error(cut),
            Kv3Error::DecompressFailed { .. } | Kv3Error::Malformed { .. }
        ),
        "truncating a compressed block must be an error, got {:?}",
        kv3_error(cut)
    );
}

#[test]
fn kv3_header_only_block_is_an_error() {
    let encoded = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), 5, Compression::Zstd);
    let cut = &encoded[..100];
    assert!(matches!(
        kv3_error(cut),
        Kv3Error::Malformed { .. } | Kv3Error::HeaderTooSmall { .. }
    ));
}

// =============================================================================================
// Review-1 fixes (tool: each test names its finding; each regresses on the old code)
// =============================================================================================

/// Finding 6 (taxonomy): the header length check runs before any lane read, so a block shorter
/// than its version's header is `HeaderTooSmall` — not whichever lane read happened to fail
/// first (`Malformed` inside the header).
#[test]
fn an_undersized_header_is_header_too_small() {
    let encoded = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), 4, Compression::None);
    let cut = &encoded[..50];
    match kv3_error(cut) {
        Kv3Error::HeaderTooSmall { need, got } => {
            assert_eq!(need, 72);
            assert_eq!(got, 50);
        }
        other => panic!("expected HeaderTooSmall, got {other:?}"),
    }
    let encoded = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), 5, Compression::None);
    let cut = &encoded[..100];
    match kv3_error(cut) {
        Kv3Error::HeaderTooSmall { need, got } => {
            assert_eq!(need, 120);
            assert_eq!(got, 100);
        }
        other => panic!("expected HeaderTooSmall, got {other:?}"),
    }
}

/// Finding 7: bytes past the accounted payload are refused (the decoder "refuses anything the
/// frames do not account for") — before the fix they were silently ignored in the v4 (and
/// v5-without-blobs) branches.
#[test]
fn bytes_past_the_accounted_payload_are_refused() {
    for (version, compression) in [
        (4u8, Compression::None),
        (4, Compression::Lz4),
        (4, Compression::Zstd),
        (5, Compression::None),
        (5, Compression::Lz4),
    ] {
        let mut encoded = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), version, compression);
        assert!(
            decode(&encoded).is_ok(),
            "the block itself decodes ({compression:?} v{version})"
        );
        encoded.extend_from_slice(&[1, 2, 3, 4, 5]); // junk past the payload
        assert!(
            matches!(kv3_error(&encoded), Kv3Error::TrailingBytes { .. }),
            "{compression:?} v{version} accepted junk past the payload"
        );
        // The end-of-buffer trailer is the only byte sequence allowed after the payload.
        let mut encoded = encode_kv3(&object(vec![("k", Kv3Value::Int(3))]), version, compression);
        encoded.extend_from_slice(&TRAILER.to_le_bytes());
        assert!(
            decode(&encoded).is_ok(),
            "{compression:?} v{version} with the file trailer still decodes"
        );
    }
    // A blob block refuses junk after the file-side blob payload too.
    let mut encoder = Encoder::new(4);
    encoder.value(&Kv3Value::Blob(vec![1, 2, 3, 4]));
    let mut encoded = encode_encoder(encoder, 4, Compression::None);
    encoded.extend_from_slice(&[9, 9]);
    assert!(matches!(
        kv3_error(&encoded),
        Kv3Error::TrailingBytes { .. }
    ));
}

/// Finding 5: the object-member dedup must not be quadratic. Every cap allows an object with
/// a million distinct members (1 type byte + 4 lane bytes + a short string each — a few MiB),
/// and the old linear `find` per member pins the blocking decode thread for tens of minutes on
/// one. The time bound is generous (the fixed code builds the document in about a second) but
/// far below the quadratic scan's ~5e11 string compares.
#[test]
fn a_million_member_object_decodes_in_bounded_time() {
    let n = 999_983usize; // root object + members stay under MAX_NODES and MAX_STRINGS
    let mut encoder = Encoder::new(4);
    // The names straight into the table (the shared `string_id` dedupes linearly — that would
    // make the *test* quadratic).
    encoder.strings = (0..n).map(|i| format!("m{i}")).collect();
    encoder.types.push(9); // T_OBJECT (the root)
                           // v4: the object's member count rides the 4-byte lane right after the string count.
    encoder.b4.extend_from_slice(&(n as i32).to_le_bytes());
    for id in 0..n {
        encoder.types.push(16); // T_INT64_ONE: no lane bytes
        encoder.b4.extend_from_slice(&(id as i32).to_le_bytes());
    }
    let encoded = encode_encoder(encoder, 4, Compression::None);

    let started = std::time::Instant::now();
    let document =
        hammer_mcp::resource::kv3bin::decode(&encoded).expect("the hostile object still decodes");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "dedup is not linear in the member count: {:?}",
        started.elapsed()
    );
    let root = document.root.as_object().expect("object root");
    assert_eq!(root.len(), n, "distinct members all survive");
    assert_eq!(
        root[499_999].0.as_str(),
        "m499999",
        "member order and spelling preserved"
    );
    assert_eq!(root[499_999].1, Kv3Value::Int(1));
}

/// Finding 8 pinned the other way: an auto-generated bodygroup name (`bg_@<index>`) still
/// splits to its group; only the choice is absent.
#[test]
fn bodygroup_auto_generated_names_split_to_their_group() {
    let data = object(vec![
        ("m_name", text("models/test/agent.vmdl")),
        ("m_meshGroups", strings(&["bg_@0"])),
        (
            "m_refMeshGroupMasks",
            Kv3Value::TypedArray(vec![Kv3Value::UInt(1)]),
        ),
    ]);
    let bytes = model_bytes(&[(encode_kv3(&data, 5, Compression::Lz4), *b"DATA")]);
    let info = model_info_from_bytes(&bytes).expect("model info");
    assert_eq!(info.bodygroups.len(), 1);
    assert_eq!(info.bodygroups[0].group.as_deref(), Some("bg"));
    assert_eq!(info.bodygroups[0].choice, None);
}

// =============================================================================================
// The real VPK corpus (`#[ignore]`: needs the installed game tree)
// =============================================================================================

/// The project root (the rule the other real-tree tests use).
fn project_root() -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("CS2_MCP_PROJECT_ROOT") {
        candidates.push(PathBuf::from(root));
    }
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        candidates.push(dir.clone());
        if !dir.pop() {
            break;
        }
    }
    candidates
        .into_iter()
        .find(|dir| dir.join(CSGO_FGD).is_file())
        .unwrap_or_else(|| {
            panic!(
                "no project root with {CSGO_FGD} at $CS2_MCP_PROJECT_ROOT or above {}",
                env!("CARGO_MANIFEST_DIR")
            )
        })
}

const CSGO_FGD: &str = "workshop/run/app/game/csgo/csgo.fgd";

/// One VPK directory entry (the fields this test needs).
struct VpkEntry {
    path: String,
    archive: u16,
    offset: u32,
    length: u32,
}

/// A minimal VPK v2 reader: header, then the nested `extension > directory > file` tree, each level
/// terminated by an empty string, then per file
/// `u32 crc, u16 preload, u16 archive, u32 offset, u32 length, u16 0xFFFF` (and the preload bytes).
fn read_vpk_tree(path: &Path) -> (Vec<VpkEntry>, usize) {
    let bytes = std::fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    assert_eq!(
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        0x55AA_1234,
        "{}: VPK magic",
        path.display()
    );
    let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    assert_eq!(version, 2, "{}: VPK version", path.display());
    let tree_size = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let tree = &bytes[28..28 + tree_size];
    let mut entries = Vec::new();
    let mut pos = 0usize;
    let cstring = |pos: &mut usize| -> String {
        let end = tree[*pos..]
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or_else(|| panic!("{}: unterminated VPK string", path.display()));
        let text = String::from_utf8_lossy(&tree[*pos..*pos + end]).into_owned();
        *pos += end + 1;
        text
    };
    loop {
        let extension = cstring(&mut pos);
        if extension.is_empty() {
            break;
        }
        loop {
            let directory = cstring(&mut pos);
            if directory.is_empty() {
                break;
            }
            loop {
                let name = cstring(&mut pos);
                if name.is_empty() {
                    break;
                }
                let field = |offset: usize| {
                    u32::from_le_bytes([
                        tree[offset],
                        tree[offset + 1],
                        tree[offset + 2],
                        tree[offset + 3],
                    ])
                };
                let preload = u16::from_le_bytes([tree[pos + 4], tree[pos + 5]]) as usize;
                let archive = u16::from_le_bytes([tree[pos + 6], tree[pos + 7]]);
                let offset = field(pos + 8);
                let length = field(pos + 12);
                let terminator = u16::from_le_bytes([tree[pos + 16], tree[pos + 17]]);
                assert_eq!(
                    terminator,
                    0xFFFF,
                    "{}: VPK entry terminator",
                    path.display()
                );
                pos += 18 + preload;
                entries.push(VpkEntry {
                    path: format!("{directory}/{name}.{extension}"),
                    archive,
                    offset,
                    length,
                });
            }
        }
    }
    assert_eq!(pos, tree_size, "{}: tree walk", path.display());
    (entries, 28 + tree_size)
}

/// Read one entry's bytes (from the `_dir.vpk` itself or its `pak01_NNN.vpk` archive).
fn read_vpk_entry(dir_vpk: &Path, data_start: usize, entry: &VpkEntry) -> Vec<u8> {
    let (path, offset) = if entry.archive == 0x7FFF {
        (dir_vpk.to_path_buf(), data_start + entry.offset as usize)
    } else {
        let stem = dir_vpk
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.split("_dir.vpk").next())
            .expect("a *_dir.vpk name");
        (
            dir_vpk.with_file_name(format!("{stem}_{:03}.vpk", entry.archive)),
            entry.offset as usize,
        )
    };
    let mut file =
        std::fs::File::open(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(offset as u64)).expect("seek");
    let mut body = vec![0u8; entry.length as usize];
    file.read_exact(&mut body).expect("read");
    body
}

/// Decode 200 sampled real models out of the installed `game/csgo/pak01_dir.vpk`: every one must
/// parse, every KV3 block of every sampled model must decode (which exercises the LZ4 blob frames
/// and the ZSTD frame groups of real `PHYS`/`ANIM`/`DATA` blocks), and the run prints what the
/// facts look like across the corpus.
///
/// `CS2_MCP_CORPUS_MODELS=N` widens the sample (the whole install is ~13 100 models; the default is
/// the 200 the work package asked for).
#[test]
#[ignore = "needs the installed CS2 game tree"]
fn corpus_models_decode() {
    let dir_vpk = project_root().join("workshop/run/app/game/csgo/pak01_dir.vpk");
    let (entries, data_start) = read_vpk_tree(&dir_vpk);
    let models: Vec<&VpkEntry> = entries
        .iter()
        .filter(|entry| entry.path.ends_with(".vmdl_c"))
        .collect();
    assert!(
        models.len() > 2000,
        "expected a full model tree, found {} .vmdl_c entries",
        models.len()
    );
    let wanted = std::env::var("CS2_MCP_CORPUS_MODELS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(200, models.len());

    let step = (models.len() / wanted).max(1);
    let mut checked = 0usize;
    let mut failed = Vec::new();
    let mut block_failed = Vec::new();
    let mut skins = Vec::new();
    let mut bodygroups = Vec::new();
    let mut sequences = Vec::new();
    let mut attachments = Vec::new();
    let mut versions: BTreeMap<u32, usize> = BTreeMap::new();
    let mut compressions: BTreeMap<u32, usize> = BTreeMap::new();
    let mut block_types: BTreeMap<String, usize> = BTreeMap::new();
    let mut with_physics = 0usize;
    let mut with_keyvalues = 0usize;
    let mut with_surface = 0usize;
    let mut with_hull = 0usize;
    let mut with_view = 0usize;
    let mut with_mass = 0usize;
    let mut with_render_bounds = 0usize;
    let mut with_physics_bounds = 0usize;
    let mut with_notes = 0usize;
    let mut with_skins = 0usize;
    let mut with_bodygroups = 0usize;
    let mut with_sequences = 0usize;
    let mut with_attachments = 0usize;
    let mut blob_blocks = 0usize;
    let mut v4_blob_frame_size_field_zero = 0usize;
    let mut kv3_blocks = 0usize;
    for entry in models.iter().step_by(step) {
        let bytes = read_vpk_entry(&dir_vpk, data_start, entry);
        let resource = match Resource::parse(&bytes) {
            Ok(resource) => resource,
            Err(error) => {
                failed.push(format!("{}: {error}", entry.path));
                continue;
            }
        };
        // Every KV3 block of the file (not only the ones ModelInfo reads) must decode: the corpus
        // carries the real blob frames and frame groups this way.
        for block in &resource.blocks {
            let name = block.type_name();
            *block_types.entry(name).or_default() += 1;
            let body = resource.block_bytes(block);
            if body.len() < 72 {
                continue; // too small to be a KV3 block
            }
            let magic = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            if magic & 0xFFFF_FF00 != 0x4B56_3300 {
                continue; // MBUF, MVTX, RERL, … are not KV3
            }
            kv3_blocks += 1;
            let compression = u32::from_le_bytes([body[20], body[21], body[22], body[23]]);
            let blob_count = u32::from_le_bytes([body[56], body[57], body[58], body[59]]);
            let frame_size_field = u32::from_le_bytes([body[68], body[69], body[70], body[71]]);
            match hammer_mcp::resource::decode_kv3(body) {
                Ok(document) => {
                    *versions.entry(u32::from(document.version)).or_default() += 1;
                    *compressions.entry(compression).or_default() += 1;
                    blob_blocks += usize::from(blob_count > 0);
                    v4_blob_frame_size_field_zero += usize::from(
                        document.version == 4 && blob_count > 0 && frame_size_field == 0,
                    );
                }
                Err(error) => {
                    block_failed.push(format!("{} {} v?: {error}", entry.path, block.type_name()))
                }
            }
        }
        match model_info_from_bytes(&bytes) {
            Ok(info) => {
                checked += 1;
                skins.push(info.skins.len());
                bodygroups.push(info.bodygroups.len());
                sequences.push(info.sequences.len());
                attachments.push(info.attachments.len());
                with_physics += usize::from(info.has_physics == Some(true));
                with_keyvalues += usize::from(info.keyvalues_text.is_some());
                with_surface += usize::from(info.surface_property.is_some());
                with_hull += usize::from(info.hull.is_some());
                with_view += usize::from(info.view.is_some());
                with_mass += usize::from(info.mass.is_some());
                with_render_bounds += usize::from(info.render_bounds.is_some());
                with_physics_bounds += usize::from(info.physics_bounds.is_some());
                with_notes += usize::from(!info.notes.is_empty());
                with_skins += usize::from(!info.skins.is_empty());
                with_bodygroups += usize::from(!info.bodygroups.is_empty());
                with_sequences += usize::from(!info.sequences.is_empty());
                with_attachments += usize::from(!info.attachments.is_empty());
                assert!(info.name.is_some(), "{}: no m_name", entry.path);
                assert!(
                    info.blocks.contains(&"DATA".to_owned()),
                    "{}: no DATA block",
                    entry.path
                );
            }
            Err(error) => failed.push(format!("{}: {error}", entry.path)),
        }
    }
    assert!(
        failed.is_empty(),
        "{} models failed:\n{}",
        failed.len(),
        failed.join("\n")
    );
    assert!(
        block_failed.is_empty(),
        "{} KV3 blocks failed:\n{}",
        block_failed.len(),
        block_failed.join("\n")
    );
    assert!(checked >= 200, "only {checked} models decoded");
    println!(
        "{checked} models, {kv3_blocks} KV3 blocks ({blob_blocks} with binary blobs, \
         {v4_blob_frame_size_field_zero} of them version-4 blocks whose frame-size field is 0), \
         versions {versions:?}, compressions {compressions:?}:\n  \
         skins {:?}/model ({with_skins} models), bodygroups {:?} ({with_bodygroups}), \
         sequences {:?} ({with_sequences}), attachments {:?} ({with_attachments})\n  \
         hull {with_hull}, view {with_view}, mass {with_mass}, surface property {with_surface}, \
         keyvalue text {with_keyvalues}, physics {with_physics}, render bounds {with_render_bounds}, \
         physics bounds {with_physics_bounds}, notes {with_notes}\n  blocks {block_types:?}",
        skins.iter().max(),
        bodygroups.iter().max(),
        sequences.iter().max(),
        attachments.iter().max(),
    );
    assert!(
        skins.iter().any(|count| *count > 0),
        "no model in the sample had skins"
    );
    assert!(
        versions.contains_key(&4) && versions.contains_key(&5),
        "expected both KV3 versions in the sample, saw {versions:?}"
    );
    assert!(blob_blocks > 0, "no sampled block carried binary blobs");
}

/// The install's rare ZSTD blocks (42 of the ~13 000 models' blocks carry one): a version-4 block
/// packs its value buffer **and** its binary blobs into one frame group (`size_uncompressed +
/// size_blobs` bytes over concatenated frames), a version-5 block keeps its blobs in one frame
/// followed by the end-of-buffer trailer. The sampled corpus above does not reach either.
#[test]
#[ignore = "needs the installed CS2 game tree"]
fn corpus_zstd_blocks_decode() {
    // (model, block type, KV3 version) — measured over the installed `pak01_dir.vpk`.
    const CASES: [(&str, &str, u8); 3] = [
        (
            "models/props/de_dust/hr_dust/dust_kasbah/dust_kasbah_mudbrick_gap_01.vmdl_c",
            "PHYS",
            4,
        ),
        ("weapons/models/ak47/weapon_rif_ak47.vmdl_c", "PHYS", 5),
        ("models/chicken/chicknegg.vmdl_c", "ANIM", 5),
    ];
    let dir_vpk = project_root().join("workshop/run/app/game/csgo/pak01_dir.vpk");
    let (entries, data_start) = read_vpk_tree(&dir_vpk);
    for (path, kind, version) in CASES {
        let entry = entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("{path} is not in {}", dir_vpk.display()));
        let bytes = read_vpk_entry(&dir_vpk, data_start, entry);
        let resource = Resource::parse(&bytes).unwrap_or_else(|error| panic!("{path}: {error}"));
        let mut seen = 0usize;
        for block in resource.blocks.iter().filter(|b| b.type_name() == kind) {
            let body = resource.block_bytes(block);
            assert!(
                body.len() > 72,
                "{path}: a {kind} block shorter than a KV3 header"
            );
            assert_eq!(body[0], version, "{path}: {kind} KV3 version");
            assert_eq!(body[20], 2, "{path}: {kind} compression (2 = ZSTD)");
            let blobs = u32::from_le_bytes([body[56], body[57], body[58], body[59]]);
            assert!(blobs > 0, "{path}: expected a blob-carrying {kind} block");
            let document = hammer_mcp::resource::decode_kv3(body)
                .unwrap_or_else(|error| panic!("{path}: {kind} must decode: {error}"));
            assert!(document.root.as_object().is_some(), "{path}: {kind} root");
            seen += 1;
        }
        assert!(seen > 0, "{path}: no {kind} block");
        // The model still yields its facts, ZSTD block included.
        let info = model_info_from_bytes(&bytes).unwrap_or_else(|error| panic!("{path}: {error}"));
        assert!(info.name.is_some(), "{path}: no m_name");
        assert!(info.render_bounds.is_some(), "{path}: no render bounds");
    }
}
