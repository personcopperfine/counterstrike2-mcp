//! KV1 binary reader and writer. Port of upstream `KV1BinaryReader`,
//! `KV1BinarySerializer`, `KeyValues1/KV1BinaryNodeType.cs` and `StringTable.cs`.
//!
//! The format is a tree of one-byte tags, each followed by a key and, for everything but an
//! object, the payload. Keys are NUL-terminated UTF-8, or a little-endian `int32` index into a
//! caller-supplied [`StringTable`] (Steam's `appinfo.vdf` v41 does this).
//!
//! | tag | upstream `KV1BinaryNodeType` | key | payload | [`Value`] |
//! |-----|------------------------------|-----|---------|-----------|
//! | 0   | `ChildObject`                | yes | nested entries, closed by an end marker | [`Value::Object`] |
//! | 1   | `String`                     | yes | NUL-terminated UTF-8 | [`Value::String`] |
//! | 2   | `Int32`                      | yes | `int32` | [`Value::Int32`] |
//! | 3   | `Float32`                    | yes | IEEE-754 binary32 | [`Value::Float32`] |
//! | 4   | `Pointer`                    | yes | `int32` (Source 1 size; see below) | [`Value::Pointer`] |
//! | 5   | `WideString`                 | yes | UTF-16LE, length-prefixed (see below) | [`Value::WideString`] |
//! | 6   | `Color`                      | yes | four bytes, in stream order | [`Value::Color`] |
//! | 7   | `UInt64`                     | yes | `uint64` | [`Value::UInt64`] |
//! | 8   | `End`                        | no  | closes the innermost object / the document | |
//! | 9   | `BinaryBlob`                 | yes | `uint32` length, then the bytes | [`Value::Binary`] |
//! | 10  | `Int64`                      | yes | `int64` | [`Value::Int64`] |
//! | 11  | `AlternateEnd`               | no  | same as `End` | |
//!
//! An object is closed by `End` or `AlternateEnd` — Steam writes the first, Source 2 the second,
//! and each rejects the other's marker as a node type, so both are always accepted. A whole
//! document is `<root pair>` followed by one more end marker ([`parse`] reads the marker and
//! ignores anything after it, like upstream).
//!
//! Three input shapes are recognised on top of that:
//!
//! * **String table.** With [`ReadOptions::string_table`] / [`WriteOptions::string_table`] every
//!   key is a little-endian `int32` index into that table instead of a NUL-terminated string.
//!   Values are unaffected: a `String` payload is always inline UTF-8.
//! * **Magic header.** A document that starts with `VBKV` + a 4-byte CRC32 that is skipped,
//!   never verified (upstream `DetectMagicHeader`; the marker is only looked for when at least
//!   eight bytes are present). Its presence changes one thing: a wide string then uses Source 2's
//!   `int16` character count instead of Steam's `int32` count that includes the terminator. The
//!   marker is looked for at the start of *every* document, the way upstream builds a fresh reader
//!   for each `Deserialize` call, so a marker in the first document does not change how the next
//!   one is read.
//! * **Consecutive documents.** A stream may hold several documents back to back, which upstream
//!   reads by calling `Deserialize` repeatedly on the same stream; [`parse_all`] does that in one
//!   call, and [`parse_prefix`] parses just the first one and reports its length.
//!
//! Errors are [`Error::Binary`] with the byte offset of the offending item: the start of a read
//! that ran past the end of the input, the tag byte of an unknown node type, or the index of a
//! string-table entry that does not exist.
//!
//! # Differences from upstream
//!
//! * **Errors instead of exceptions.** Everything upstream throws reaches the caller as
//!   [`Error::Binary`] with a byte offset into the input: the start of a read that ran past the end
//!   of the slice (`EndOfStreamException`), the tag byte of an unknown node type
//!   (`ArgumentOutOfRangeException`), a string-table index that is negative or past the end of the
//!   table (the indexer's `ArgumentOutOfRangeException`), a key a read-only [`StringTable`] does
//!   not have (`InvalidOperationException`). Messages and offsets are this port's own; upstream
//!   wraps most of it in a `KeyValueException` carrying the stream position. Reading also takes a
//!   byte slice rather than requiring a seekable `Stream`.
//! * **Unknown tag.** An unknown tag is rejected at the tag itself, before the key is read;
//!   upstream reads the key first and only throws in the `default:` arm of `ReadValue`, so its
//!   position is past that key. `out_of_range_node_type_is_an_error` asserts the tag's offset.
//! * **No root object.** A document whose first byte is an end marker is an error here (at that
//!   marker). Upstream's `ReadRootObject` returns without reading a value and the failure comes
//!   later from the object builder — `KVObjectBuilder.GetObject` throws "Builder is not in a fully
//!   completed state (stack count is 0)".
//! * **New entry points.** [`parse_prefix`] / [`parse_prefix_with`] (document length) and
//!   [`parse_all`] / [`parse_all_with`] (consecutive documents) have no upstream equivalent; its
//!   own consecutive-document test calls `Deserialize` twice on one stream. [`parse_all`] on an
//!   empty input returns an empty `Vec`, where a single `Deserialize` on an empty stream throws
//!   `EndOfStreamException`. A trailing document that is truncated is an error, not an empty last
//!   document.
//! * **[`MAX_BINARY_DEPTH`] caps nesting**, the root object included. Upstream has no cap and
//!   recurses once per nesting level, so a hostile file can overflow its stack; this reader is
//!   iterative and rejects depth > cap so the returned tree (whose `Drop` is recursive) stays safe
//!   to hold. The writer is iterative too and has no cap, so a deeper document can be written but
//!   not read back.
//! * **Hostile lengths are rejected before anything is allocated.** A type-9 length is checked
//!   against the remaining input, and a type-5 one is turned into a byte count with a checked
//!   multiply and then capped the same way. Upstream allocates `new byte[length]` (type 9) or
//!   `new byte[length * 2]` (type 5) first, so a hostile length is an out-of-memory throw — or, for
//!   a wide string longer than `int32::MAX / 2`, an overflowing multiplication — rather than a
//!   parse error.
//! * **[`Value::Color`] and [`Value::Pointer`] keep their wire tag.** Upstream's `KVObject` turns a
//!   type 6 into an `Int32` and a type 4 into a 64-bit `KVValueType.Pointer`, losing the tag: after
//!   a read, a colour cannot be told apart from an `Int32`. Here a colour is four bytes in stream
//!   order and a pointer is an `i32`, and both survive a read/write round trip byte for byte.
//! * **The writer emits type 5 and type 9.** Upstream's serializer has no value type for either: it
//!   writes [`Value::Binary`] as a *hex string* (`"AB CD"`, type 1) and can never write a wide
//!   string at all, so neither value would survive a round trip through it. A wide string is
//!   written the way a document without the magic header is read back (an `int32` count including
//!   the terminator), so it always comes back as written; an interior NUL still truncates it, the
//!   way it truncates an upstream read.
//! * **A document whose root is not an object gets an end marker** from [`write`]. Upstream writes
//!   no marker for that case (its own reader then fails on the missing marker).
//! * **[`write`] and [`write_with`] return a `Result`.** Without a string table only a length that
//!   does not fit the wire format can fail (a blob longer than `u32::MAX` bytes, a wide string
//!   longer than `i32::MAX` UTF-16 units); with one, a read-only [`StringTable`] can reject a key.
//!   Upstream's serializer cannot fail for those lengths — it cannot write a blob as bytes at all —
//!   and throws `InvalidOperationException` for the read-only table.
//! * **`StringTable` returns `Option` where upstream throws**: [`StringTable::get`] and
//!   [`StringTable::index_of`] for a missing entry, [`StringTable::add`] and
//!   [`StringTable::get_or_add`] for a read-only table. [`StringTable::len`], `iter` and `to_vec`
//!   (`ToArray`) are additions; upstream's `Count` and indexer are not exposed.
//! * Upstream's `StringTable(IList<string>)` is read-only when it is handed an array (arrays report
//!   `IsReadOnly`); here that is the explicit [`StringTable::read_only`] constructor and
//!   [`StringTable::from_values`] is always writable. Both build the index the way upstream does:
//!   the constructor is last-index-wins, `Add` is first-wins.

mod reader;
mod writer;

use std::collections::HashMap;

use crate::{Document, Error};

/// Maximum number of objects that may nest, the root object included.
///
/// A deeper document is rejected with [`Error::Binary`] instead of being built, because the
/// returned [`crate::Object`] tree is dropped recursively (upstream has no such cap).
pub const MAX_BINARY_DEPTH: usize = 256;

/// `VBKV`, the little-endian magic a document may start with (upstream `BinaryMagicHeader`).
pub(crate) const MAGIC_HEADER: u32 = 0x564B_4256;

/// The one-byte tag in front of every entry, upstream `KV1BinaryNodeType`.
///
/// Not public: the tag a value was read from is already carried by the [`Value`] variant, and no
/// caller needs to name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeType {
    ChildObject,
    String,
    Int32,
    Float32,
    Pointer,
    WideString,
    Color,
    UInt64,
    End,
    BinaryBlob,
    Int64,
    AlternateEnd,
}

impl NodeType {
    /// The tag byte, or `None` when it is not a node type. Upstream casts the byte to its
    /// `KV1BinaryNodeType` enum, so an unknown tag survives that step and is only rejected in the
    /// `default:` arm of `ReadValue` (after the key has been read).
    pub(crate) fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0 => NodeType::ChildObject,
            1 => NodeType::String,
            2 => NodeType::Int32,
            3 => NodeType::Float32,
            4 => NodeType::Pointer,
            5 => NodeType::WideString,
            6 => NodeType::Color,
            7 => NodeType::UInt64,
            8 => NodeType::End,
            9 => NodeType::BinaryBlob,
            10 => NodeType::Int64,
            11 => NodeType::AlternateEnd,
            _ => return None,
        })
    }

    /// The tag byte.
    pub(crate) fn byte(self) -> u8 {
        match self {
            NodeType::ChildObject => 0,
            NodeType::String => 1,
            NodeType::Int32 => 2,
            NodeType::Float32 => 3,
            NodeType::Pointer => 4,
            NodeType::WideString => 5,
            NodeType::Color => 6,
            NodeType::UInt64 => 7,
            NodeType::End => 8,
            NodeType::BinaryBlob => 9,
            NodeType::Int64 => 10,
            NodeType::AlternateEnd => 11,
        }
    }

    /// True for the two markers that close an object (upstream `IsEndMarker`).
    pub(crate) fn is_end(self) -> bool {
        matches!(self, NodeType::End | NodeType::AlternateEnd)
    }
}

/// Keys of a binary document, in the order they were added.
///
/// In string-table mode a document stores `int32` indices instead of key strings, which is how
/// Steam's `appinfo.vdf` v41 (and anything else with a shared key pool) is written. Reading looks
/// indices up with [`StringTable::get`]; writing resolves keys with
/// [`StringTable::get_or_add`], which appends missing ones, so the table a document was written
/// with is the table a reader needs to read it back.
#[derive(Debug, Clone, Default)]
pub struct StringTable {
    entries: Vec<String>,
    /// `entry -> first index`, so lookups are O(1) like upstream's reverse dictionary.
    reverse: HashMap<String, usize>,
    writable: bool,
}

impl StringTable {
    /// An empty, writable table (upstream `new StringTable()`).
    pub fn new() -> Self {
        Self { entries: Vec::new(), reverse: HashMap::new(), writable: true }
    }

    /// An empty, writable table with room for `capacity` keys.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            reverse: HashMap::with_capacity(capacity),
            writable: true,
        }
    }

    /// A writable table holding `values`.
    ///
    /// A key that appears more than once keeps the *last* index it has in `values`, the way
    /// upstream's constructor assigns `reverse[value] = i`.
    ///
    /// Note that upstream's `StringTable(IList<string>)` is read-only when it is given an array
    /// (`IList.IsReadOnly`); use [`StringTable::read_only`] for that behaviour.
    pub fn from_values<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut table = Self::new();
        for (index, value) in values.into_iter().enumerate() {
            let value = value.into();
            table.reverse.insert(value.clone(), index);
            table.entries.push(value);
        }
        table
    }

    /// A table holding `values` that refuses to grow, like upstream's `StringTable(IList<string>)`
    /// when the list is read-only: [`StringTable::get_or_add`] then returns `None` for a key it
    /// does not have instead of appending it.
    pub fn read_only<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut table = Self::from_values(values);
        table.writable = false;
        table
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the table has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// True when [`StringTable::get_or_add`] may append missing keys.
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// The key at `index`, or `None` when it is out of range.
    pub fn get(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(String::as_str)
    }

    /// The index of `value`, or `None` when the table does not have it. A key may repeat in the
    /// table: [`StringTable::from_values`] resolves it to the last index it has there (upstream's
    /// list constructor), [`StringTable::add`] to the first one it was added at (upstream's
    /// `TryAdd`).
    pub fn index_of(&self, value: &str) -> Option<usize> {
        self.reverse.get(value).copied()
    }

    /// Appends `value` and returns its index, or `None` when the table is read-only (upstream's
    /// `Add` throws `InvalidOperationException` then). An existing `value` is appended again: the
    /// table may hold duplicates, like upstream's list-backed one, and its index is not moved
    /// (upstream's `TryAdd`).
    pub fn add(&mut self, value: impl Into<String>) -> Option<usize> {
        if !self.writable {
            return None;
        }
        let value = value.into();
        let index = self.entries.len();
        self.reverse.entry(value.clone()).or_insert(index);
        self.entries.push(value);
        Some(index)
    }

    /// The index of `value`, appending it when missing; `None` when the table is read-only and
    /// does not have it (upstream's `GetOrAdd`).
    pub fn get_or_add(&mut self, value: &str) -> Option<usize> {
        match self.index_of(value) {
            Some(index) => Some(index),
            None => self.add(value),
        }
    }

    /// A copy of the keys in index order (upstream `ToArray`).
    pub fn to_vec(&self) -> Vec<String> {
        self.entries.clone()
    }

    /// The keys in index order.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(String::as_str)
    }
}

/// Where [`parse_with`] and [`parse_all_with`] get their keys from.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadOptions<'a> {
    /// When set, a key is an `int32` index into this table instead of a NUL-terminated string.
    pub string_table: Option<&'a StringTable>,
}

impl<'a> ReadOptions<'a> {
    /// Options that read keys as indices into `table`.
    pub fn with_table(table: &'a StringTable) -> Self {
        Self { string_table: Some(table) }
    }
}

/// Where [`write_with`] gets its keys from.
#[derive(Debug, Default)]
pub struct WriteOptions<'a> {
    /// When set, a key is written as an `int32` index into this table, which grows as needed
    /// (unless it is a [`StringTable::read_only`] one without the key, which is an error).
    pub string_table: Option<&'a mut StringTable>,
}

impl<'a> WriteOptions<'a> {
    /// Options that write keys as indices into `table`.
    pub fn with_table(table: &'a mut StringTable) -> Self {
        Self { string_table: Some(table) }
    }
}

/// Parses the first document of `bytes` with no string table.
///
/// Anything after the document is ignored, like upstream (which never reads past the document's
/// end marker). Use [`parse_prefix`] to learn where the document ended.
pub fn parse(bytes: &[u8]) -> Result<Document, Error> {
    parse_with(bytes, &ReadOptions::default())
}

/// Parses the first document of `bytes`, resolving keys through `opts`.
pub fn parse_with(bytes: &[u8], opts: &ReadOptions<'_>) -> Result<Document, Error> {
    parse_prefix_with(bytes, opts).map(|(document, _)| document)
}

/// Parses the first document of `bytes` and reports how many bytes it used.
///
/// The count covers the document's own end marker — the byte position upstream's tests assert on
/// after a `Deserialize` call — and is what a container format such as Steam's `packageinfo.vdf`,
/// which stores documents without a length, has to walk with.
pub fn parse_prefix(bytes: &[u8]) -> Result<(Document, usize), Error> {
    parse_prefix_with(bytes, &ReadOptions::default())
}

/// [`parse_prefix`] with options.
pub fn parse_prefix_with(bytes: &[u8], opts: &ReadOptions<'_>) -> Result<(Document, usize), Error> {
    let mut reader = reader::Reader::new(bytes, opts.string_table);
    let document = reader.read_document()?;
    Ok((document, reader.position()))
}

/// Parses every consecutive document in `bytes` with no string table.
///
/// Upstream has no such call: its `BinaryObjectConsecutiveDeserializationTestCase` deserializes
/// twice from one stream instead. An empty input holds no documents and returns an empty vector;
/// a truncated trailing document is an error.
pub fn parse_all(bytes: &[u8]) -> Result<Vec<Document>, Error> {
    parse_all_with(bytes, &ReadOptions::default())
}

/// [`parse_all`] with options. The magic header is looked for at the start of each document, like
/// upstream's fresh reader per `Deserialize` call.
pub fn parse_all_with(bytes: &[u8], opts: &ReadOptions<'_>) -> Result<Vec<Document>, Error> {
    let mut reader = reader::Reader::new(bytes, opts.string_table);
    let mut documents = Vec::new();
    while !reader.at_end() {
        documents.push(reader.read_document()?);
    }
    Ok(documents)
}

/// Serializes `doc` with inline keys.
///
/// Fallible even with no string table: a [`Value::Binary`] longer than `u32::MAX` bytes or a
/// [`Value::WideString`] longer than `i32::MAX` UTF-16 units does not fit the wire format.
pub fn write(doc: &Document) -> Result<Vec<u8>, Error> {
    // Without a string table every key is written inline, so no key can be rejected.
    write_with(doc, &mut WriteOptions::default())
}

/// Serializes `doc`, resolving keys through `opts`.
///
/// Fails when a length does not fit the wire format (`u32` blob length, `i32` wide-string count)
/// or when `opts` holds a [`StringTable::read_only`] table that is missing one of the document's
/// keys; the error offset is the length of the output written so far.
pub fn write_with(doc: &Document, opts: &mut WriteOptions<'_>) -> Result<Vec<u8>, Error> {
    let mut writer = writer::Writer::new(opts.string_table.as_deref_mut());
    writer.write_document(doc)?;
    Ok(writer.finish())
}
