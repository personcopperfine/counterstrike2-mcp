//! Binary codec, versions 1-5 and 9. Port of upstream `Codecs/Binary.cs`, plus Valve's own
//! `CDmSerializerBinary` layout.
//!
//! The codec reads `rest` (everything after the header's `>`) the way upstream reads the file
//! after rewinding: it skips to the NUL that ends the header line (`…-->\n\0`), then reads the
//! version 9 prefix-attribute block, the string table, the element index and the element bodies.
//!
//! Version notes, all taken from upstream's `SupportedAttributes` and `StringDictionary`:
//!
//! * **1** has no string table at all (upstream `Dummy`): every string is written in place,
//!   attribute names included;
//! * **2-3** have a table whose length and indices are `short`s, but element names and string
//!   *values* are still written in place;
//! * **4** writes the table length as an `int` and its indices as `short`s ("binary 4 uses int for
//!   dictionary length, but short for dictionary indices. Whoops!"), 5 and 9 write both as `int`s;
//! * **3-8** add `Time` (int32 ticks of 1/10000 s); **9** adds `QAngle`, `UInt64` and `Byte`, moves
//!   the array ids from `scalar + 14` to `scalar + 32`, and splits `binary` (a blob) from
//!   `uint8_array` (an array of bytes);
//! * in 1-8 an *angle* has the id of `Vector3` (the lists name that kind twice, at ids 10 and 12),
//!   so an angle in those versions decodes as [`Attribute::Vector3`];
//! * an array of bytes in a version without `Byte` is written as a `binary` blob and therefore
//!   decodes as [`Attribute::Binary`] ("these are the same thing!"), so that pair is not
//!   shape-preserving.
//!
//! Version 9 stores the prefix attributes both before the string table and as an unreferenced
//! element right after the root — Hammer writes that element with the class `$prefix_element$` for
//! the maps it does not own outright, and most files use `DmElement`. This port keeps the element
//! in [`Datamodel::elements`] at its file position, whatever its class, and fills
//! [`Datamodel::prefix_attributes`] from the block in front of the table.
//!
//! # The two layouts
//!
//! [`Layout::Valve`] is the default ([`encode`], and so [`crate::save`]): it reproduces what
//! Valve's `CDmSerializerBinary` writes, which is the layout of every `.vmap` and `.vmdl` in a
//! game install.
//!
//! * Every element that is not a stub is written, in [`Datamodel::elements`] order, reachable from
//!   the root or not; its index is its position in that order, and the stub *is* skipped (`-2` and
//!   GUID text wherever something refers to it).
//! * The string table starts with [`Datamodel::binary_string_table`], the table the file carried —
//!   a table is append-only history, so it holds strings no attribute uses any more — and then
//!   appends the strings the model needs that it does not hold.
//! * Bodies write an element's attributes in [`Element::attributes`] order, which is the order the
//!   file had.
//!
//! [`Layout::Upstream`] writes what upstream's `Encoder` writes: only the elements a reference
//! reaches from the root, in the order it first reaches them, with an empty table (upstream's
//! `Encoder.Gather` interns `name` before `class name`, walks attribute names in attribute order,
//! and never looks at a table a file had).
//!
//! # Differences from upstream
//!
//! * Malformed input returns [`Error`] instead of throwing: out-of-range string and element
//!   indices, bad GUID text, truncated reads and unsupported type ids all become
//!   [`Error::Decode`] or [`Error::Unsupported`].
//! * Every count read from the file is bounded by the bytes that are left before it can size an
//!   allocation (upstream sizes tables straight from the count, so an `int` of `0x7fffffff` asks
//!   for gigabytes before the first read fails). A negative count fails in every place upstream
//!   fails: the string table, element, prefix element and prefix attribute lists throw in
//!   `List.Capacity`, an element's attribute count throws in `Array.Resize`, an array count throws
//!   in the array's constructor, and a `binary` length throws in `ReadBytes`.
//! * A negative attribute count always fails here. Upstream's `AttributeList.EnsureCapacity` only
//!   calls `Array.Resize` (which throws for a negative size) when the element's slot array is
//!   still `null` or shorter than the count, so a negative count throws for an element that has no
//!   slots yet and is silently ignored (the loop runs zero times) for one that already has some.
//!   Erroring everywhere is deliberate: a count below zero is corrupt whichever element it is on.
//! * Encoding refuses a format name containing *any* whitespace character; upstream's `Format`
//!   setter only refuses a space. The header reader splits on whitespace, so a tab or newline in
//!   the name would write a file this crate cannot read back.
//! * **Trailing bytes are ignored**, exactly like upstream's decoder, which stops after the last
//!   body instead of checking that the stream ended; [`decode_counted`] reports where the stream
//!   ended for a caller that refuses them.
//! * A version 9 prefix attribute whose value is an element is refused by both directions:
//!   upstream refuses to store one while decoding (`AttributeTypeException`, "Elements are not
//!   supported as prefix attributes") and its `PrefixAttributes` cannot hold one to encode. The
//!   element index is read after the prefix block, so the only element a prefix attribute can
//!   name is a stub (`-2`), which is exactly the case that fails.
//! * A version 9 prefix element named by a reference is kept in the datamodel instead of dropped
//!   the way upstream drops it: upstream's fold removes it from `AllElements` and leaves the
//!   reference pointing at nothing, which an index-based arena cannot express. Upstream's own
//!   `PrefixElementId` field is not ported (it is only used to generate the id of the element it
//!   re-emits); [`Layout::Upstream`] re-emits a folded element under the id it had in the file
//!   when the arena holds one (whether or not a reference names it), and under the all-zero GUID
//!   for a model built in code. When a reference names the folded element, [`Layout::Upstream`]
//!   points it at the re-emitted prefix element, which has the same id and keys, rather than
//!   writing the element a second time under an id the file already uses.
//! * [`Attribute::Element`] values in `prefix_attributes` are refused with [`Error::Encode`]
//!   instead of panicking on an index that was never written.
//! * [`Attribute::Time`] is `f64` seconds here and an upstream `TimeSpan` there. Ticks are rounded
//!   to the nearest tick instead of dropping the remainder below one (upstream's
//!   `(int)(ticks / 1000)`), because a value read from a file must be written back unchanged and
//!   `f64` cannot hold every tick exactly; the int32 conversion wraps like upstream's cast.
//! * Strings decode as UTF-8 with invalid sequences replaced by U+FFFD, which is what .NET's
//!   `UTF8Encoding(false).GetString` does with them too.
//!
//! ## Valve's string table order
//!
//! [`Layout::Valve`] appends a missing string in the order Valve's writer interns it, which was
//! derived by replaying the tables of the 39 binary files of the game and test installs (the two
//! keyvalues2 fixtures among the upstream test resources have no table). The replay clears each
//! file's [`Datamodel::binary_string_table`], saves, and compares the table that comes back with
//! the file's own: **33 of the 39 match entry for entry**, `taunt05_b5.dmx` (version 5) among them.
//! The six that do not all hold strings no body names, which is what an append-only history looks
//! like: `roundtrip_test.vmap`, `ancient_zoo.vmap`, `cache_zoo.vmap` and `train_zoo.vmap` each
//! hold one extra `m_nFieldDataStates` (an attribute CS2's engine creates and only sometimes
//! sets); `binary4.dmx` (version 4) holds `__bind` and `__RootNode` extra; `overboss_run.dmx` holds
//! `disableOverride`, `__RootNode` and `__bind` extra and also has `$prefix_attribute$` after the
//! root's first attribute instead of right after its name. For all but the last the rebuilt table
//! is the file's table with the extras left out; the extras are the table's history, which
//! [`Datamodel::binary_string_table`] carries, so the rule only has to be exact for a model built
//! in code. For each element, in [`Datamodel::elements`] order:
//!
//! 1. the class name, then the element name;
//! 2. for a version 9 model with prefix attributes, `$prefix_attribute$` — the root element's
//!    pseudo-attribute the prefix block belongs to;
//! 3. every attribute in **reverse** [`Element::attributes`] order, the attribute's name and, for
//!    a [`Attribute::String`], its value right after it. Valve walks its attribute list the
//!    opposite way it writes the body, which is why the table is the body order reversed. Array
//!    items are written in place and are never interned;
//! 4. `name`, the element's own implicit attribute, which Valve creates before the others.
//!
//! Two of those entries are normally in a file's table without any body naming them:
//! `$prefix_attribute$` and the first element's `name`. Replaying a table can tell where Valve put
//! them (`$prefix_attribute$` sits right after the root's class and name in 36 of the 37 version 9
//! files of the installs the encoder was checked against, and after the root's first attribute in
//! the last one; `name` after the root's attributes in all 39), but which point of the element's
//! construction interned them is not in the file. A model that carries a table keeps them in
//! position through [`Datamodel::binary_string_table`], so a load and a save change nothing; the
//! two entries are only rebuilt for a model with no table of its own (a model built in code), so
//! that what it writes looks like what Valve writes.

use std::collections::HashMap;

use super::{format_header, Header};
use crate::{Attribute, Datamodel, Element, ElementId, Error, Guid, Time};

/// The class name upstream writes for the prefix element it re-emits, and the one it folds back.
const PREFIX_ELEMENT_CLASS: &str = "DmElement";

/// The root element's pseudo-attribute the version 9 prefix block belongs to. Valve's writer puts
/// the name in the string table; nothing in a file's bodies names it (see the module docs).
const PREFIX_ATTRIBUTE: &str = "$prefix_attribute$";

/// An element's implicit name attribute. Valve creates it before an element's other attributes, so
/// the table walk (which is the reverse) interns it after them.
const NAME_ATTRIBUTE: &str = "name";

/// Datamodel binary ticks in one second (upstream `DatamodelTicksPerSecond`).
const TICKS_PER_SECOND: f64 = 10_000.0;

/// How [`encode_with`] lays a binary file out.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Layout {
    /// What Valve's `CDmSerializerBinary` writes: every element in [`Datamodel::elements`] order,
    /// the table [`Datamodel::binary_string_table`] carried first, and the strings the model adds
    /// appended in Valve's order. This is [`encode`] and so [`crate::save`], because it is what
    /// makes Hammer's own `.vmap` files come back byte for byte.
    #[default]
    Valve,
    /// What upstream's `Encoder` writes: only the elements a reference reaches from the root, in
    /// the order it first reaches them, with a table gathered from those elements alone, and the
    /// version 9 prefix element re-emitted right after the root. The upstream C# tests pin this
    /// layout, and it is the one that does not reproduce a Valve file; see the module docs.
    Upstream,
}

/// The value kinds a binary version can have an id for. Mirrors upstream's `AttributeType` for
/// every id a supported version uses; the model side is [`Attribute`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Element,
    Int,
    Float,
    Bool,
    String,
    Binary,
    Time,
    Color,
    Vector2,
    Vector3,
    Vector4,
    QAngle,
    Quaternion,
    Matrix,
    UInt64,
    Byte,
}

impl Kind {
    /// Upstream's `AttributeType` name, for the error messages.
    fn name(self) -> &'static str {
        match self {
            Kind::Element => "Element",
            Kind::Int => "Int",
            Kind::Float => "Float",
            Kind::Bool => "Bool",
            Kind::String => "String",
            Kind::Binary => "Binary",
            Kind::Time => "Time",
            Kind::Color => "Color",
            Kind::Vector2 => "Vector2",
            Kind::Vector3 => "Vector3",
            Kind::Vector4 => "Vector4",
            Kind::QAngle => "QAngle",
            Kind::Quaternion => "Quaternion",
            Kind::Matrix => "Matrix",
            Kind::UInt64 => "UInt64",
            Kind::Byte => "Byte",
        }
    }

    /// The id a version writes for a value of this kind (upstream `TypeToId(AttributeType, int)`),
    /// or [`Error::Encode`] when the version has none (upstream `CodecException`).
    fn id(self, version: i32) -> Result<u8, Error> {
        supported_attributes(version)
            .iter()
            .position(|entry| *entry == Some(self))
            .map(|index| index as u8 + 1)
            .ok_or_else(|| Error::Encode(format!("\"{}\" is not supported in encoding binary {version}", self.name())))
    }
}

/// The kinds each encoding version supports, in the order of their ids. `None` marks an id the
/// version reserves for something this library does not read (`ObjectID` in versions 1 and 2).
fn supported_attributes(version: i32) -> &'static [Option<Kind>] {
    use Kind::*;

    const VERSIONS_1_2: [Option<Kind>; 14] = [
        Some(Element),
        Some(Int),
        Some(Float),
        Some(Bool),
        Some(String),
        Some(Binary),
        None,
        Some(Color),
        Some(Vector2),
        Some(Vector3),
        Some(Vector4),
        Some(Vector3),
        Some(Quaternion),
        Some(Matrix),
    ];
    const VERSIONS_3_5: [Option<Kind>; 14] = [
        Some(Element),
        Some(Int),
        Some(Float),
        Some(Bool),
        Some(String),
        Some(Binary),
        Some(Time),
        Some(Color),
        Some(Vector2),
        Some(Vector3),
        Some(Vector4),
        Some(Vector3),
        Some(Quaternion),
        Some(Matrix),
    ];
    const VERSION_9: [Option<Kind>; 16] = [
        Some(Element),
        Some(Int),
        Some(Float),
        Some(Bool),
        Some(String),
        Some(Binary),
        Some(Time),
        Some(Color),
        Some(Vector2),
        Some(Vector3),
        Some(Vector4),
        Some(QAngle),
        Some(Quaternion),
        Some(Matrix),
        Some(UInt64),
        Some(Byte),
    ];

    match version {
        1 | 2 => &VERSIONS_1_2,
        3..=5 => &VERSIONS_3_5,
        9 => &VERSION_9,
        _ => &[],
    }
}

/// The kind of an array's items (upstream `Datamodel.GetArrayInnerType` of the array's CLR type),
/// or `None` when the attribute is not an array.
fn array_kind(attribute: &Attribute) -> Option<Kind> {
    Some(match attribute {
        Attribute::ElementArray(_) => Kind::Element,
        Attribute::IntArray(_) => Kind::Int,
        Attribute::FloatArray(_) => Kind::Float,
        Attribute::BoolArray(_) => Kind::Bool,
        Attribute::StringArray(_) => Kind::String,
        Attribute::BinaryArray(_) => Kind::Binary,
        Attribute::TimeArray(_) => Kind::Time,
        Attribute::ColorArray(_) => Kind::Color,
        Attribute::Vector2Array(_) => Kind::Vector2,
        Attribute::Vector3Array(_) => Kind::Vector3,
        Attribute::Vector4Array(_) => Kind::Vector4,
        Attribute::QAngleArray(_) => Kind::QAngle,
        Attribute::QuaternionArray(_) => Kind::Quaternion,
        Attribute::MatrixArray(_) => Kind::Matrix,
        Attribute::UInt64Array(_) => Kind::UInt64,
        Attribute::ByteArray(_) => Kind::Byte,
        _ => return None,
    })
}

/// The id an array attribute is written with (upstream `TypeToId(Type, int)`): the id of its item
/// kind plus the version's array offset, except that an array of bytes in a version without `Byte`
/// is a `binary` blob, whose id has no offset.
fn array_id(attribute: &Attribute, version: i32) -> Result<u8, Error> {
    let mut kind = array_kind(attribute).ok_or_else(|| Error::Encode("not an array attribute".into()))?;
    let list = supported_attributes(version);
    let mut is_array = true;

    if kind == Kind::Byte && !list.contains(&Some(Kind::Byte)) {
        kind = Kind::Binary;
        is_array = false;
    }

    // Upstream finds the id by scanning for the first entry whose CLR type matches the item type;
    // the duplicate `Vector3` entry of versions 1-5 is never reached that way.
    let scalar = kind
        .id(version)
        .map_err(|_| Error::Encode(format!("\"{}\" array is not supported in encoding binary {version}", kind.name())))?;

    if is_array {
        Ok(scalar + (list.len() * if version >= 9 { 2 } else { 1 }) as u8)
    } else {
        Ok(scalar)
    }
}

/// The kind an id denotes and whether it denotes an array of it (upstream `IdToType`), or the
/// message of an [`Error::Decode`] for an id the version has no kind for. Ids 17-32 decode as
/// arrays in version 9 as well, because upstream's `else` branch still subtracts the list length;
/// encode only ever writes 33-48 there.
fn id_to_kind(id: u8, version: i32) -> Result<(Kind, bool), String> {
    let list = supported_attributes(version);
    // upstream decrements a byte, so id 0 becomes 255 and fails every lookup below
    let mut index = i32::from(id) - 1;
    let mut is_array = false;

    if version >= 9 && index >= (list.len() * 2) as i32 {
        is_array = true;
        index -= (list.len() * 2) as i32;
    } else if index >= list.len() as i32 {
        index -= list.len() as i32;
        is_array = true;
    }

    match list.get(index as usize).copied().flatten() {
        Some(kind) => Ok((kind, is_array)),
        None => Err(format!("unrecognised attribute type id {id}")),
    }
}

// --------------------------------------------------------------------------- reading

/// A bounds-checked cursor over the codec's input.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }

    /// The bytes left in the stream.
    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// A malformed-input error pointing at the current offset.
    fn error<T>(&self, message: impl Into<String>) -> Result<T, Error> {
        self.error_at(self.pos, message)
    }

    /// A malformed-input error pointing at `at`.
    fn error_at<T>(&self, at: usize, message: impl Into<String>) -> Result<T, Error> {
        Err(Error::Decode { at: at.to_string(), message: message.into() })
    }

    /// `n` bytes, or an error.
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.remaining() < n {
            return self.error(format!("truncated: want {n} byte(s), {} left", self.remaining()));
        }
        let bytes = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(bytes)
    }

    /// Fails unless `n` more bytes are available, without consuming them: used to refuse a count
    /// before it sizes an allocation.
    fn need(&self, n: usize) -> Result<(), Error> {
        if self.remaining() < n {
            return self.error(format!("truncated: want {n} byte(s), {} left", self.remaining()));
        }
        Ok(())
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn i16(&mut self) -> Result<i16, Error> {
        let bytes = self.take(2)?;
        Ok(i16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn i32(&mut self) -> Result<i32, Error> {
        let bytes = self.take(4)?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes(bytes.try_into().expect("take(8) hands out eight bytes")))
    }

    fn f32(&mut self) -> Result<f32, Error> {
        let bytes = self.take(4)?;
        Ok(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// `N` consecutive `f32`s.
    fn f32s<const N: usize>(&mut self) -> Result<[f32; N], Error> {
        let bytes = self.take(N * 4)?;
        let mut values = [0f32; N];
        for (index, value) in values.iter_mut().enumerate() {
            let slice = &bytes[index * 4..index * 4 + 4];
            *value = f32::from_le_bytes(slice.try_into().expect("four bytes per float"));
        }
        Ok(values)
    }

    /// Sixteen consecutive `f32`s, as the row-major 4x4 matrix upstream writes (`M11 … M44`).
    fn matrix(&mut self) -> Result<[[f32; 4]; 4], Error> {
        let bytes = self.take(64)?;
        let mut matrix = [[0f32; 4]; 4];
        for (index, row) in matrix.iter_mut().enumerate() {
            for (column, value) in row.iter_mut().enumerate() {
                let offset = (index * 4 + column) * 4;
                *value = f32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes per float"));
            }
        }
        Ok(matrix)
    }

    fn guid(&mut self) -> Result<Guid, Error> {
        let bytes = self.take(16)?;
        Ok(Guid(bytes.try_into().expect("take(16) hands out sixteen bytes")))
    }

    /// `length` bytes, refusing a negative length the way upstream's `ReadBytes(-1)` does.
    fn bytes(&mut self, length: i32) -> Result<&'a [u8], Error> {
        if length < 0 {
            return self.error(format!("negative binary length {length}"));
        }
        self.take(length as usize)
    }

    /// A NUL-terminated string (upstream `ReadString_Raw`), with invalid UTF-8 replaced.
    fn raw_string(&mut self) -> Result<String, Error> {
        match self.data[self.pos..].iter().position(|byte| *byte == 0) {
            Some(length) => {
                let text = String::from_utf8_lossy(&self.data[self.pos..self.pos + length]).into_owned();
                self.pos += length + 1;
                Ok(text)
            }
            None => self.error("unterminated string"),
        }
    }

    /// Skips to and consumes the header line's NUL. Upstream rewinds the stream and reads bytes
    /// until it meets a zero, which is that same NUL.
    fn skip_to_nul(&mut self) -> Result<(), Error> {
        match self.data.iter().position(|byte| *byte == 0) {
            Some(index) => {
                self.pos = index + 1;
                Ok(())
            }
            None => self.error_at(0, "no NUL after the DMX header"),
        }
    }

    /// A count read off the file, refused when it is negative or when the bytes that are left could
    /// not possibly hold that many items (every item needs at least `min_bytes`). Upstream sizes
    /// its tables from these counts, so a corrupt one allocates before it fails.
    fn checked_count(&self, count: i32, min_bytes: usize, what: &str) -> Result<usize, Error> {
        if count < 0 {
            return self.error(format!("negative {what} {count}"));
        }
        let maximum = self.remaining() / min_bytes.max(1);
        if count as usize > maximum {
            return self.error(format!("implausible {what} {count} ({} byte(s) left)", self.remaining()));
        }
        Ok(count as usize)
    }
}

/// The string table (upstream `StringDictionary`).
struct StringDict {
    version: i32,
    /// Insertion order, which is the order the table is written in. Empty for version 1.
    strings: Vec<String>,
    /// Value to index; `None` for version 1, which writes every string in place.
    indices: Option<HashMap<String, usize>>,
}

impl StringDict {
    fn new(version: i32) -> Self {
        StringDict {
            version,
            strings: Vec::new(),
            indices: if version == 1 { None } else { Some(HashMap::new()) },
        }
    }

    /// Whether every string is written in place (upstream `Dummy`, version 1 only).
    fn is_dummy(&self) -> bool {
        self.indices.is_none()
    }

    /// The width of the table's own length: `short` before version 4, `int` from 4.
    fn length_size(&self) -> usize {
        if self.version < 4 {
            2
        } else {
            4
        }
    }

    /// The width of a table index: `short` before version 5, `int` from 5.
    fn indice_size(&self) -> usize {
        if self.version < 5 {
            2
        } else {
            4
        }
    }

    /// Reads the table a version wrote: its length (a `short` before version 4), then every string
    /// (upstream `StringDictionary(int encoding_version, BinaryReader reader)`). Version 1 has no
    /// table and nothing is read for it.
    fn read(version: i32, reader: &mut Reader<'_>) -> Result<Self, Error> {
        let mut dict = StringDict::new(version);
        if dict.is_dummy() {
            return Ok(dict);
        }

        let count = if dict.length_size() == 2 { i32::from(reader.i16()?) } else { reader.i32()? };
        // Every string occupies at least its terminator, so a count larger than the bytes left
        // cannot be honoured; upstream sizes its list from the count before reading any of them.
        let count = reader.checked_count(count, 1, "string table count")?;
        dict.strings.reserve(count.min(4096));
        for index in 0..count {
            // A file may repeat a string; upstream's list keeps both entries and an index finds
            // the one it names, so the table is a list here too and only `add_string` dedups.
            let value = reader.raw_string()?;
            dict.indices.as_mut().expect("a non-dummy table has an index").entry(value.clone()).or_insert(index);
            dict.strings.push(value);
        }
        Ok(dict)
    }

    /// The table a file carried, ready to be appended to: an index is the position of the string's
    /// **first** entry, which is the one a body of that file names, and later entries stay in the
    /// table (append-only history). The table is the same size as the file's, so no string can be
    /// written as the -1 [`StringDict::write_string`] falls back to.
    fn seeded(version: i32, table: &[String]) -> Self {
        let mut dict = StringDict::new(version);
        if let Some(indices) = dict.indices.as_mut() {
            for (position, value) in table.iter().enumerate() {
                indices.entry(value.clone()).or_insert(position);
            }
        }
        dict.strings = table.to_vec();
        dict
    }

    /// Adds a string unless it is already there. Nothing is added for a version that writes every
    /// string in place. Upstream keeps a second table for the names it has met, which resolves to
    /// the same index because both are keyed by the string's value.
    fn add_string(&mut self, value: &str) {
        let Some(indices) = self.indices.as_mut() else {
            return;
        };
        if indices.contains_key(value) {
            return;
        }
        indices.insert(value.to_string(), self.strings.len());
        self.strings.push(value.to_string());
    }

    /// Reads a string the table holds the index of (upstream `StringDictionary.ReadString`).
    fn read_string(&self, reader: &mut Reader<'_>) -> Result<String, Error> {
        if self.is_dummy() {
            return reader.raw_string();
        }
        let index = if self.indice_size() == 2 { i32::from(reader.i16()?) } else { reader.i32()? };
        match usize::try_from(index).ok().and_then(|index| self.strings.get(index)) {
            Some(value) => Ok(value.clone()),
            None => reader.error(format!("string index {index} is out of range (table has {})", self.strings.len())),
        }
    }

    /// Writes a string as a table index, or in place for version 1. A string the table never met is
    /// written as index -1, which is what upstream's `GetIndex` returns for it.
    fn write_string(&self, value: &str, out: &mut Vec<u8>) {
        if self.is_dummy() {
            write_raw_string(out, value);
            return;
        }
        let index = self.indices.as_ref().and_then(|indices| indices.get(value)).map(|index| *index as i64).unwrap_or(-1);
        self.write_index(index, out);
    }

    /// Writes the table: its length, then every string in insertion order.
    fn write_self(&self, out: &mut Vec<u8>) {
        if self.is_dummy() {
            return;
        }
        if self.length_size() == 2 {
            push_i16(out, self.strings.len() as i16);
        } else {
            push_i32(out, self.strings.len() as i32);
        }
        for value in &self.strings {
            write_raw_string(out, value);
        }
    }

    fn write_index(&self, index: i64, out: &mut Vec<u8>) {
        if self.indice_size() == 2 {
            push_i16(out, index as i16);
        } else {
            push_i32(out, index as i32);
        }
    }
}

/// The decode half: the stream state, the string table and the elements found so far.
struct Decoder<'a> {
    version: i32,
    reader: Reader<'a>,
    dict: StringDict,
    /// Element GUID to position in `elements`, including the stubs `-2` creates. This is upstream's
    /// `Datamodel.AllElements`, which is also what resolves a stub to a declared element.
    by_guid: HashMap<Guid, ElementId>,
    elements: Vec<Element>,
    /// The number of elements the index declares (upstream `ElementIndex.Count`). `elements` grows
    /// past it with the stubs `-2` creates, but an index reference may only name a declared one.
    declared: usize,
    prefix_attributes: Vec<(String, Attribute)>,
}

impl<'r> Decoder<'r> {
    /// The attribute an id and its payload denote. `raw_string` is upstream's
    /// `EncodingVersion < 4`: versions 1-3 write string values in place.
    fn read_value(&mut self, kind: Kind, raw_string: bool) -> Result<Attribute, Error> {
        Ok(match kind {
            Kind::Element => Attribute::Element(self.read_element_ref()?),
            Kind::Int => Attribute::Int(self.reader.i32()?),
            Kind::Float => Attribute::Float(self.reader.f32()?),
            Kind::Bool => Attribute::Bool(self.reader.u8()? != 0),
            Kind::String => {
                let value = if raw_string { self.reader.raw_string()? } else { self.dict.read_string(&mut self.reader)? };
                Attribute::String(value)
            }
            Kind::Binary => {
                let length = self.reader.i32()?;
                Attribute::Binary(self.reader.bytes(length)?.to_vec())
            }
            Kind::Time => Attribute::Time(Time(f64::from(self.reader.i32()?) / TICKS_PER_SECOND)),
            Kind::Color => {
                let bytes = self.reader.take(4)?;
                Attribute::Color([bytes[0], bytes[1], bytes[2], bytes[3]])
            }
            Kind::Vector2 => Attribute::Vector2(self.reader.f32s()?),
            Kind::Vector3 => Attribute::Vector3(self.reader.f32s()?),
            Kind::Vector4 => Attribute::Vector4(self.reader.f32s()?),
            Kind::QAngle => Attribute::QAngle(self.reader.f32s()?),
            Kind::Quaternion => Attribute::Quaternion(self.reader.f32s()?),
            Kind::Matrix => Attribute::Matrix(self.reader.matrix()?),
            Kind::UInt64 => Attribute::UInt64(self.reader.u64()?),
            Kind::Byte => Attribute::Byte(self.reader.u8()?),
        })
    }

    /// An id and the value that follows it, array or scalar (upstream `DecodeAttribute`).
    /// `raw_string` is set for the prefix attributes, which upstream always reads in place.
    fn read_attribute(&mut self, raw_string: bool) -> Result<Attribute, Error> {
        let at = self.reader.pos;
        let version = self.version;
        let (kind, is_array) = id_to_kind(self.reader.u8()?, version).map_err(|message| Error::Decode { at: at.to_string(), message })?;

        if !is_array {
            return self.read_value(kind, raw_string || version < 4);
        }
        let count = self.reader.i32()?;
        // Upstream hands the count to an array allocator, which throws for a negative capacity.
        let count = self.reader.checked_count(count, 1, "array count")?;
        self.read_array(kind, count)
    }

    /// `count` items of an array attribute (upstream `ReadArray`). Item strings are always in
    /// place, whatever the version.
    fn read_array(&mut self, kind: Kind, count: usize) -> Result<Attribute, Error> {
        // The least an item of this kind can occupy; a valid stream must hold that many bytes, and
        // checking first keeps a corrupt count from sizing a vector.
        let item_min_bytes = match kind {
            Kind::Element | Kind::Int | Kind::Float | Kind::Time | Kind::Color => 4,
            Kind::Vector2 => 8,
            Kind::Vector3 | Kind::QAngle => 12,
            Kind::Vector4 | Kind::Quaternion => 16,
            Kind::UInt64 => 8,
            Kind::Matrix => 64,
            Kind::Bool | Kind::Byte | Kind::String => 1,
            Kind::Binary => 4,
        };
        let needed = count.checked_mul(item_min_bytes).ok_or_else(|| Error::Decode {
            at: self.reader.pos.to_string(),
            message: format!("array of {count} items overflows"),
        })?;
        self.reader.need(needed)?;

        Ok(match kind {
            Kind::Element => {
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(self.read_element_ref()?);
                }
                Attribute::ElementArray(items)
            }
            Kind::Int => Attribute::IntArray(self.read_items(count, Reader::i32)?),
            Kind::Float => Attribute::FloatArray(self.read_items(count, Reader::f32)?),
            Kind::Bool => Attribute::BoolArray(self.read_items(count, Reader::u8)?.into_iter().map(|byte| byte != 0).collect()),
            Kind::String => {
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(self.reader.raw_string()?);
                }
                Attribute::StringArray(items)
            }
            Kind::Binary => {
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    let length = self.reader.i32()?;
                    items.push(self.reader.bytes(length)?.to_vec());
                }
                Attribute::BinaryArray(items)
            }
            Kind::Time => {
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(Time(f64::from(self.reader.i32()?) / TICKS_PER_SECOND));
                }
                Attribute::TimeArray(items)
            }
            Kind::Color => {
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    let bytes = self.reader.take(4)?;
                    items.push([bytes[0], bytes[1], bytes[2], bytes[3]]);
                }
                Attribute::ColorArray(items)
            }
            Kind::Vector2 => Attribute::Vector2Array(self.read_items(count, Reader::f32s::<2>)?),
            Kind::Vector3 => Attribute::Vector3Array(self.read_items(count, Reader::f32s::<3>)?),
            Kind::Vector4 => Attribute::Vector4Array(self.read_items(count, Reader::f32s::<4>)?),
            Kind::QAngle => Attribute::QAngleArray(self.read_items(count, Reader::f32s::<3>)?),
            Kind::Quaternion => Attribute::QuaternionArray(self.read_items(count, Reader::f32s::<4>)?),
            Kind::Matrix => Attribute::MatrixArray(self.read_items(count, Reader::matrix)?),
            Kind::UInt64 => Attribute::UInt64Array(self.read_items(count, Reader::u64)?),
            Kind::Byte => Attribute::ByteArray(self.read_items(count, Reader::u8)?),
        })
    }

    /// Calls `read` `count` times, into a vector that is sized only once the bytes are known to be
    /// there.
    fn read_items<T>(&mut self, count: usize, mut read: impl FnMut(&mut Reader<'r>) -> Result<T, Error>) -> Result<Vec<T>, Error> {
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(read(&mut self.reader)?);
        }
        Ok(items)
    }

    /// An element reference: -1 is null, -2 is a stub named by GUID text, anything else is a
    /// position in the element index (upstream `ReadElement`).
    fn read_element_ref(&mut self) -> Result<Option<ElementId>, Error> {
        let index = self.reader.i32()?;

        if index == -1 {
            return Ok(None);
        }
        if index == -2 {
            let at = self.reader.pos;
            let text = self.reader.raw_string()?;
            let guid = text
                .parse::<Guid>()
                .map_err(|_| Error::Decode { at: at.to_string(), message: format!("bad stub GUID text {text:?}") })?;
            return Ok(Some(self.stub_for(guid)));
        }
        match usize::try_from(index).ok().filter(|position| *position < self.declared) {
            Some(position) => Ok(Some(ElementId(position))),
            None => self.reader.error(format!("element index {index} is out of range ({} declared elements)", self.declared)),
        }
    }

    /// The element a GUID names: the declared one, or a new stub (upstream
    /// `dm.AllElements[id] ?? new Element(dm, id)`, whose stub keeps `Element`'s defaults).
    fn stub_for(&mut self, guid: Guid) -> ElementId {
        if let Some(known) = self.by_guid.get(&guid) {
            return *known;
        }
        let id = ElementId(self.elements.len());
        self.by_guid.insert(guid, id);
        self.elements.push(Element {
            class_name: "DmeElement".to_string(),
            name: "Stub element".to_string(),
            id: guid,
            stub: true,
            attributes: Vec::new(),
        });
        id
    }

    /// The whole body, mirroring upstream `Binary.Decode`: the model, and the bytes of `rest` the
    /// stream used (everything up to the end of the last body).
    fn run(mut self, header: &Header) -> Result<(Datamodel, usize), Error> {
        let version = self.version;

        if version >= 9 {
            // Prefix elements: only the first one's attributes are kept, the later ones ("are they
            // considered 'old versions'?") are consumed all the same.
            let prefix_elements = self.reader.i32()?;
            let prefix_elements = self.reader.checked_count(prefix_elements, 4, "prefix element count")?;
            for prefix_element in 0..prefix_elements {
                let count = self.reader.i32()?;
                let count = self.reader.checked_count(count, 3, "prefix attribute count")?;
                for _ in 0..count {
                    let name = self.reader.raw_string()?;
                    let value = self.read_attribute(true)?;
                    if prefix_element == 0 {
                        if matches!(value, Attribute::Element(Some(_))) {
                            return self.reader.error("elements are not supported as prefix attributes");
                        }
                        // Upstream's AttributeList indexer replaces a name it already holds.
                        match self.prefix_attributes.iter_mut().find(|(existing, _)| *existing == name) {
                            Some(slot) => slot.1 = value,
                            None => self.prefix_attributes.push((name, value)),
                        }
                    }
                }
            }
        }

        // Every index row holds a class name and a name reference plus 16 GUID bytes; before
        // version 4 the name is in place, so a row is three bytes shorter.
        self.dict = StringDict::read(version, &mut self.reader)?;

        let element_min_bytes = if version >= 4 { 24 } else { 21 };
        let element_count = self.reader.i32()?;
        let element_count = self.reader.checked_count(element_count, element_min_bytes, "element count")?;
        self.declared = element_count;
        for _ in 0..element_count {
            let class_name = self.dict.read_string(&mut self.reader)?;
            let name = if version >= 4 { self.dict.read_string(&mut self.reader)? } else { self.reader.raw_string()? };
            let guid = self.reader.guid()?;

            // Stubs only appear while reading bodies, so a GUID the index already declared is a
            // duplicate here (upstream's `ElementList.Add` throws for a non-stub one).
            if self.by_guid.contains_key(&guid) {
                return self.reader.error(format!("element ID {guid} already in use in this datamodel"));
            }

            let id = ElementId(self.elements.len());
            self.by_guid.insert(guid, id);
            self.elements.push(Element { class_name, name, id: guid, stub: false, attributes: Vec::new() });
        }

        for position in 0..self.elements.len() {
            let count = self.reader.i32()?;
            // Upstream resizes its attribute slots to the count, which throws for a negative one.
            let count = self.reader.checked_count(count, 3, "attribute count")?;
            let mut attributes: Vec<(String, Attribute)> = Vec::with_capacity(count.min(4096));

            for _ in 0..count {
                let name = self.dict.read_string(&mut self.reader)?;
                let value = self.read_attribute(false)?;

                // Upstream's AttributeList indexer replaces the value of a name it already holds,
                // in the position that name has.
                match attributes.iter_mut().find(|(existing, _)| *existing == name) {
                    Some(slot) => slot.1 = value,
                    None => attributes.push((name, value)),
                }
            }

            self.elements[position].attributes = attributes;
        }

        // Version 9 also stores the prefix attributes as an unreferenced element right after the
        // root. Hammer writes it with the class `$prefix_element$` for the maps it does not own
        // outright; either way it stays in `elements` at its file position, because that is where
        // the file's element index has it and Valve's own encoder writes it back from there.
        let root = if self.elements.is_empty() { None } else { Some(ElementId(0)) };
        let consumed = self.reader.pos;
        let dm = Datamodel {
            encoding: header.encoding.clone(),
            encoding_version: version,
            format: header.format.clone(),
            format_version: header.format_version,
            prefix_attributes: self.prefix_attributes,
            binary_string_table: self.dict.strings,
            elements: self.elements,
            root,
        };
        Ok((dm, consumed))
    }
}

/// Decode a binary DMX body: `rest` is every byte after the header's `>`.
pub fn decode(header: &Header, rest: &[u8]) -> Result<Datamodel, Error> {
    decode_counted(header, rest).map(|(dm, _)| dm)
}

/// [`decode`], plus how many bytes of `rest` the stream used. Bytes after the last body are
/// ignored by [`decode`] (like upstream); a caller that treats them as corruption compares this
/// count with `rest.len()`.
pub fn decode_counted(header: &Header, rest: &[u8]) -> Result<(Datamodel, usize), Error> {
    let version = header.encoding_version;
    if supported_attributes(version).is_empty() {
        return Err(Error::Unsupported { encoding: header.encoding.clone(), version });
    }

    let mut reader = Reader::new(rest);
    reader.skip_to_nul()?;

    Decoder {
        version,
        reader,
        dict: StringDict::new(version),
        by_guid: HashMap::new(),
        elements: Vec::new(),
        declared: 0,
        prefix_attributes: Vec::new(),
    }
    .run(header)
}

// --------------------------------------------------------------------------- writing

/// One body the encoder writes, in the order its index entry is written (upstream's
/// `Encoder.Order`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Body {
    /// An element of the datamodel.
    Element(ElementId),
    /// The prefix attributes, which version 9 also writes as an element of class `DmElement`.
    Prefix,
}

fn push_i16(out: &mut Vec<u8>, value: i16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// A string in the stream's text encoding, NUL-terminated (upstream `OutputBuffer.Write(string)`).
fn write_raw_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}

/// The 1/10000 s ticks a time is written as (upstream `Encoder.ToTicks`).
///
/// Upstream divides a `TimeSpan`'s 100 ns ticks by 1000, dropping any remainder smaller than one
/// tick. This port rounds the seconds value to the nearest tick instead, because a value that came
/// out of a file has to be written back unchanged: 3000001 ticks is 300.0001 s, and `f64` keeps
/// that value a hair below the exact one, so dividing there would lose the tick.
fn time_to_ticks(time: Time) -> i32 {
    let ticks = (time.0 * TICKS_PER_SECOND).round() as i64;
    ticks as i32
}

/// The element at `id`, refusing an id the datamodel does not hold instead of panicking the way
/// upstream's list indexer does.
fn element_at(dm: &Datamodel, id: ElementId) -> Result<&Element, Error> {
    dm.elements
        .get(id.0)
        .ok_or_else(|| Error::Encode(format!("element index {} is out of range ({} elements)", id.0, dm.elements.len())))
}

/// The write half: the layout, the body order, the string table and the output.
struct Encoder<'a> {
    dm: &'a Datamodel,
    layout: Layout,
    version: i32,
    dict: StringDict,
    /// The bodies in the order their index entries are written (upstream's `Encoder.Order`).
    order: Vec<Body>,
    /// Element to position in the body order, which is the index the file writes (upstream's
    /// `Encoder.Indices`).
    indices: HashMap<ElementId, usize>,
    /// The prefix element [`Layout::Upstream`] folds away, so that its own body is not written a
    /// second time and the re-emitted prefix element keeps its id.
    folded: Option<ElementId>,
    out: Vec<u8>,
}

impl<'a> Encoder<'a> {
    /// The element at `id`, borrowed from the datamodel rather than from `self`, so a visitor can
    /// keep it while the encoder writes.
    fn element(&self, id: ElementId) -> Result<&'a Element, Error> {
        let dm: &'a Datamodel = self.dm;
        element_at(dm, id)
    }

    /// Writes an element reference: -2 and the GUID text for a stub, the element's index
    /// otherwise (upstream `Encoder.WriteElement`).
    fn write_element_ref(&mut self, id: ElementId) -> Result<(), Error> {
        let element = self.element(id)?;
        if element.stub {
            push_i32(&mut self.out, -2);
            write_raw_string(&mut self.out, &element.id.to_string());
            return Ok(());
        }
        match self.indices.get(&id) {
            Some(index) => {
                push_i32(&mut self.out, *index as i32);
                Ok(())
            }
            None => Err(Error::Encode(format!("element {} is not reachable from the root, so it has no index", element.id))),
        }
    }

    /// Writes a string value: in place before version 4 and for the prefix attribute block and
    /// array items, as a table index otherwise (upstream `Encoder.WriteString`).
    fn write_value_string(&self, value: &str, raw: bool, out: &mut Vec<u8>) {
        if self.version < 4 || raw {
            write_raw_string(out, value);
        } else {
            self.dict.write_string(value, out);
        }
    }

    /// Writes an array attribute with its type id and count. Items are written little-endian,
    /// which is their layout in the stream (upstream `Encoder.WriteArray` / `WriteItems`).
    fn write_array(&mut self, attribute: &Attribute) -> Result<(), Error> {
        self.out.push(array_id(attribute, self.version)?);

        match attribute {
            Attribute::ElementArray(items) => {
                push_i32(&mut self.out, items.len() as i32);
                for item in items {
                    match item {
                        Some(id) => self.write_element_ref(*id)?,
                        None => push_i32(&mut self.out, -1),
                    }
                }
            }
            Attribute::IntArray(items) => {
                push_i32(&mut self.out, items.len() as i32);
                for item in items {
                    push_i32(&mut self.out, *item);
                }
            }
            Attribute::FloatArray(items) => {
                push_i32(&mut self.out, items.len() as i32);
                for item in items {
                    self.out.extend_from_slice(&item.to_le_bytes());
                }
            }
            Attribute::BoolArray(items) => {
                push_i32(&mut self.out, items.len() as i32);
                for item in items {
                    self.out.push(u8::from(*item));
                }
            }
            Attribute::StringArray(items) => {
                push_i32(&mut self.out, items.len() as i32);
                for item in items {
                    write_raw_string(&mut self.out, item);
                }
            }
            Attribute::BinaryArray(items) => {
                push_i32(&mut self.out, items.len() as i32);
                for item in items {
                    push_i32(&mut self.out, item.len() as i32);
                    self.out.extend_from_slice(item);
                }
            }
            Attribute::TimeArray(items) => {
                push_i32(&mut self.out, items.len() as i32);
                for item in items {
                    push_i32(&mut self.out, time_to_ticks(*item));
                }
            }
            Attribute::ColorArray(items) => {
                push_i32(&mut self.out, items.len() as i32);
                for item in items {
                    self.out.extend_from_slice(item);
                }
            }
            Attribute::Vector2Array(items) => {
                push_i32(&mut self.out, items.len() as i32);
                for item in items {
                    for component in item {
                        self.out.extend_from_slice(&component.to_le_bytes());
                    }
                }
            }
            Attribute::Vector3Array(items) | Attribute::QAngleArray(items) => {
                push_i32(&mut self.out, items.len() as i32);
                for item in items {
                    for component in item {
                        self.out.extend_from_slice(&component.to_le_bytes());
                    }
                }
            }
            Attribute::Vector4Array(items) | Attribute::QuaternionArray(items) => {
                push_i32(&mut self.out, items.len() as i32);
                for item in items {
                    for component in item {
                        self.out.extend_from_slice(&component.to_le_bytes());
                    }
                }
            }
            Attribute::MatrixArray(items) => {
                push_i32(&mut self.out, items.len() as i32);
                for item in items {
                    for row in item {
                        for component in row {
                            self.out.extend_from_slice(&component.to_le_bytes());
                        }
                    }
                }
            }
            Attribute::UInt64Array(items) => {
                push_i32(&mut self.out, items.len() as i32);
                for item in items {
                    self.out.extend_from_slice(&item.to_le_bytes());
                }
            }
            Attribute::ByteArray(items) => {
                // Version 9 knows `uint8_array`; before it the same bytes are a `binary` blob,
                // whose length is this count - `array_id` picks the id for that.
                push_i32(&mut self.out, items.len() as i32);
                self.out.extend_from_slice(items);
            }
            _ => return Err(Error::Encode("not an array attribute".into())),
        }

        Ok(())
    }

    /// Writes an attribute: its type id, then the value (upstream `Encoder.WriteValue`).
    /// `raw_string` is set for the prefix attribute block, whose strings are written in place.
    fn write_attribute(&mut self, attribute: &Attribute, raw_string: bool) -> Result<(), Error> {
        match attribute {
            Attribute::Element(value) => {
                self.out.push(Kind::Element.id(self.version)?);
                match value {
                    Some(id) => self.write_element_ref(*id)?,
                    None => push_i32(&mut self.out, -1),
                }
            }
            Attribute::Int(value) => {
                self.out.push(Kind::Int.id(self.version)?);
                push_i32(&mut self.out, *value);
            }
            Attribute::Float(value) => {
                self.out.push(Kind::Float.id(self.version)?);
                self.out.extend_from_slice(&value.to_le_bytes());
            }
            Attribute::Bool(value) => {
                self.out.push(Kind::Bool.id(self.version)?);
                self.out.push(u8::from(*value));
            }
            Attribute::String(value) => {
                self.out.push(Kind::String.id(self.version)?);
                let raw = raw_string;
                let mut out = std::mem::take(&mut self.out);
                self.write_value_string(value, raw, &mut out);
                self.out = out;
            }
            Attribute::Binary(value) => {
                self.out.push(Kind::Binary.id(self.version)?);
                push_i32(&mut self.out, value.len() as i32);
                self.out.extend_from_slice(value);
            }
            Attribute::Time(value) => {
                self.out.push(Kind::Time.id(self.version)?);
                push_i32(&mut self.out, time_to_ticks(*value));
            }
            Attribute::Color(value) => {
                self.out.push(Kind::Color.id(self.version)?);
                self.out.extend_from_slice(value);
            }
            Attribute::Vector2(value) => {
                self.out.push(Kind::Vector2.id(self.version)?);
                for component in value {
                    self.out.extend_from_slice(&component.to_le_bytes());
                }
            }
            Attribute::Vector3(value) => {
                self.out.push(Kind::Vector3.id(self.version)?);
                for component in value {
                    self.out.extend_from_slice(&component.to_le_bytes());
                }
            }
            Attribute::Vector4(value) => {
                self.out.push(Kind::Vector4.id(self.version)?);
                for component in value {
                    self.out.extend_from_slice(&component.to_le_bytes());
                }
            }
            Attribute::QAngle(value) => {
                self.out.push(Kind::QAngle.id(self.version)?);
                for component in value {
                    self.out.extend_from_slice(&component.to_le_bytes());
                }
            }
            Attribute::Quaternion(value) => {
                self.out.push(Kind::Quaternion.id(self.version)?);
                for component in value {
                    self.out.extend_from_slice(&component.to_le_bytes());
                }
            }
            Attribute::Matrix(value) => {
                self.out.push(Kind::Matrix.id(self.version)?);
                for row in value {
                    for component in row {
                        self.out.extend_from_slice(&component.to_le_bytes());
                    }
                }
            }
            Attribute::UInt64(value) => {
                self.out.push(Kind::UInt64.id(self.version)?);
                self.out.extend_from_slice(&value.to_le_bytes());
            }
            Attribute::Byte(value) => {
                self.out.push(Kind::Byte.id(self.version)?);
                self.out.push(*value);
            }
            array => self.write_array(array)?,
        }
        Ok(())
    }

    /// Adds an element's strings to the table and reaches the elements it refers to, depth first in
    /// attribute order (upstream `Encoder.Gather`). Upstream recurses; this keeps its own stack of
    /// frames so that a long chain of elements cannot overflow the thread's stack, and interns and
    /// numbers in exactly the order the recursion would.
    fn gather(&mut self, id: ElementId) -> Result<(), Error> {
        /// Where a gathered element's walk stands: the next attribute, and the next item of an
        /// element array attribute it is in the middle of.
        struct Frame<'e> {
            element: &'e Element,
            attribute: usize,
            item: usize,
        }

        let mut stack: Vec<Frame<'a>> = Vec::new();
        let element = self.element(id)?;
        self.dict.add_string(&element.name);
        self.dict.add_string(&element.class_name);
        stack.push(Frame { element, attribute: 0, item: 0 });

        while let Some(frame) = stack.last_mut() {
            let Some((name, attribute)) = frame.element.attributes.get(frame.attribute) else {
                stack.pop();
                continue;
            };
            // The child this step reaches, if any; the frame has moved on before the child is
            // pushed, so the walk resumes after it.
            let mut child = None;
            match attribute {
                Attribute::Element(Some(target)) => {
                    self.dict.add_string(name);
                    frame.attribute += 1;
                    child = Some(*target);
                }
                Attribute::ElementArray(items) => {
                    if frame.item == 0 {
                        self.dict.add_string(name);
                    }
                    match items.iter().skip(frame.item).enumerate().find_map(|(skipped, item)| item.map(|target| (skipped, target))) {
                        Some((skipped, target)) => {
                            frame.item += skipped + 1;
                            child = Some(target);
                        }
                        None => {
                            frame.attribute += 1;
                            frame.item = 0;
                        }
                    }
                }
                other => {
                    self.dict.add_string(name);
                    if let Attribute::String(value) = other {
                        self.dict.add_string(value);
                    }
                    frame.attribute += 1;
                }
            }

            if let Some(target) = child {
                let reached = self.element(target)?;
                if reached.stub || self.indices.contains_key(&target) || self.folded == Some(target) {
                    continue;
                }
                self.indices.insert(target, self.order.len());
                self.order.push(Body::Element(target));
                self.dict.add_string(&reached.name);
                self.dict.add_string(&reached.class_name);
                stack.push(Frame { element: reached, attribute: 0, item: 0 });
            }
        }
        Ok(())
    }

    /// The element upstream folds into the prefix attributes: the one right after the root, when
    /// it is the class upstream re-emits, has no name and names the same keys (upstream's
    /// `Binary.Decode` fold, which upstream does at load time and which this port does when it
    /// writes that layout). Upstream sets `PrefixElementId` in that branch whether or not anything
    /// refers to the element (`RemoveUnreferenced` does not look), so a reference does not stop
    /// the fold here either: the re-emitted prefix element takes the element's id, and a
    /// reference to it resolves to that prefix element, which holds the same keys.
    fn folded_prefix_element(&self) -> Option<ElementId> {
        if self.version < 9 || self.dm.prefix_attributes.is_empty() {
            return None;
        }
        let element = self.dm.elements.get(1)?;
        let same_keys = element.attributes.len() == self.dm.prefix_attributes.len()
            && element
                .attributes
                .iter()
                .zip(self.dm.prefix_attributes.iter())
                .all(|((left, _), (right, _))| left == right);
        let foldable = !element.stub && element.class_name == PREFIX_ELEMENT_CLASS && element.name.is_empty() && same_keys;
        foldable.then_some(ElementId(1))
    }

    /// Valve's write plan: every element that is not a stub, in arena order, whether the root
    /// reaches it or not — that is the order a Valve file's element index is in, so it is the
    /// order that reproduces one.
    fn plan_valve(&mut self) {
        for (position, element) in self.dm.elements.iter().enumerate() {
            if element.stub {
                continue;
            }
            self.indices.insert(ElementId(position), self.order.len());
            self.order.push(Body::Element(ElementId(position)));
        }
    }

    /// The attributes the version 9 prefix block writes: every prefix attribute except a null
    /// element, which has no value to write (`write_prefix_block` skips the same ones).
    fn prefix_block_attributes(&self) -> impl Iterator<Item = &(String, Attribute)> {
        self.dm.prefix_attributes.iter().filter(|(_, attribute)| !matches!(attribute, Attribute::Element(None)))
    }

    /// Appends the strings Valve's writer would intern, in the order it interns them (see the
    /// module docs), for the strings the table does not hold yet. A stub's class and name are
    /// never written — its references carry GUID text instead — so they are not interned either.
    ///
    /// The two pseudo-attribute names are only rebuilt when the model carries no table of its own:
    /// they are the table's history, and a table that came out of a file already says whether the
    /// writer that wrote it interned them (Valve's writer always does; a file from another writer
    /// need not, and then its bytes stay as they are).
    fn intern_valve_strings(&mut self) {
        let dm = self.dm;
        let has_prefix_block = self.version >= 9 && self.prefix_block_attributes().next().is_some();
        let rebuild_history = dm.binary_string_table.is_empty();
        for (position, element) in dm.elements.iter().enumerate() {
            if element.stub {
                continue;
            }
            self.dict.add_string(&element.class_name);
            self.dict.add_string(&element.name);
            if rebuild_history && has_prefix_block && dm.root == Some(ElementId(position)) {
                self.dict.add_string(PREFIX_ATTRIBUTE);
            }
            for (name, attribute) in element.attributes.iter().rev() {
                self.dict.add_string(name);
                if let Attribute::String(value) = attribute {
                    self.dict.add_string(value);
                }
            }
            if rebuild_history {
                self.dict.add_string(NAME_ATTRIBUTE);
            }
        }
    }

    /// Upstream's write plan: the root, the prefix element it re-emits, then every element a
    /// reference first reaches (upstream `Encoder.Encode`). The table starts empty, whatever the
    /// model carries, because upstream's `Encoder.Gather` never reads one.
    fn plan_upstream(&mut self) -> Result<(), Error> {
        let dm = self.dm;
        let Some(root) = dm.root else { return Ok(()) };
        if self.element(root)?.stub {
            return Ok(());
        }
        self.folded = self.folded_prefix_element();
        let has_prefix_element = self.version >= 9 && !dm.prefix_attributes.is_empty();

        self.indices.insert(root, 0);
        self.order.push(Body::Element(root));
        if self.folded == Some(root) {
            self.folded = None;
        }
        if has_prefix_element {
            self.order.push(Body::Prefix);
            if let Some(folded) = self.folded {
                self.indices.insert(folded, 1);
            }
        }

        self.gather(root)?;

        if has_prefix_element {
            self.dict.add_string("");
            self.dict.add_string(PREFIX_ELEMENT_CLASS);
            for (name, attribute) in &dm.prefix_attributes {
                self.dict.add_string(name);
                if let Attribute::String(value) = attribute {
                    self.dict.add_string(value);
                }
            }
        }
        Ok(())
    }

    /// Writes one body: its attribute count, then every name and value (upstream
    /// `Encoder.WriteBody`).
    fn write_body(&mut self, body: Body) -> Result<(), Error> {
        let dm = self.dm;
        let attributes = match body {
            Body::Element(id) => &element_at(dm, id)?.attributes,
            Body::Prefix => &dm.prefix_attributes,
        };

        push_i32(&mut self.out, attributes.len() as i32);
        for (name, attribute) in attributes {
            self.dict.write_string(name, &mut self.out);
            self.write_attribute(attribute, false)?;
        }
        Ok(())
    }

    /// Writes the version 9 prefix attribute block: the number of prefix elements (1, or 0 when
    /// there is nothing to write), then the attributes, with in-place strings (upstream
    /// `Encoder.WritePrefixAttributes`). Null element values are skipped here but kept in the
    /// prefix element body below, as upstream does.
    fn write_prefix_block(&mut self) -> Result<(), Error> {
        let count = self.prefix_block_attributes().count();

        if count == 0 {
            push_i32(&mut self.out, 0);
            return Ok(());
        }

        push_i32(&mut self.out, 1);
        push_i32(&mut self.out, count as i32);

        let dm = self.dm;
        for (name, attribute) in &dm.prefix_attributes {
            if matches!(attribute, Attribute::Element(None)) {
                continue;
            }
            write_raw_string(&mut self.out, name);
            self.write_attribute(attribute, true)?;
        }
        Ok(())
    }

    /// The whole file: the header, the prefix block, the layout's plan, then the table, the index
    /// and the bodies (upstream `Encoder.Encode` for [`Layout::Upstream`]).
    fn run(&mut self) -> Result<(), Error> {
        self.out.extend_from_slice(format_header("binary", self.version, &self.dm.format, self.dm.format_version).as_bytes());
        self.out.push(b'\n');
        self.out.push(0);

        if self.version >= 9 {
            // The block is at the same place in both layouts and writes its strings in place: it
            // comes before the table, so no index exists to write them as.
            self.write_prefix_block()?;
        }

        match self.layout {
            Layout::Valve => {
                self.plan_valve();
                self.intern_valve_strings();
            }
            Layout::Upstream => self.plan_upstream()?,
        }

        self.dict.write_self(&mut self.out);
        push_i32(&mut self.out, self.order.len() as i32);

        let order = std::mem::take(&mut self.order);

        for body in &order {
            match body {
                Body::Element(id) => {
                    let element = self.element(*id)?;
                    self.dict.write_string(&element.class_name, &mut self.out);
                    if self.version >= 4 {
                        self.dict.write_string(&element.name, &mut self.out);
                    } else {
                        write_raw_string(&mut self.out, &element.name);
                    }
                    self.out.extend_from_slice(&element.id.0);
                }
                Body::Prefix => {
                    self.dict.write_string(PREFIX_ELEMENT_CLASS, &mut self.out);
                    if self.version >= 4 {
                        self.dict.write_string("", &mut self.out);
                    } else {
                        write_raw_string(&mut self.out, "");
                    }
                    // The element upstream folded away keeps its id; a model built in code has no
                    // such element, so it gets the all-zero GUID.
                    let id = match self.folded {
                        Some(folded) => element_at(self.dm, folded)?.id,
                        None => Guid::default(),
                    };
                    self.out.extend_from_slice(&id.0);
                }
            }
        }

        for body in &order {
            self.write_body(*body)?;
        }

        self.order = order;
        Ok(())
    }
}

/// Encode a whole binary DMX file in [`Layout::Valve`], header line included: what [`crate::save`]
/// writes, and what reproduces a file Valve's own tools wrote.
pub fn encode(dm: &Datamodel) -> Result<Vec<u8>, Error> {
    encode_with(dm, Layout::Valve)
}

/// Encode a whole binary DMX file in the layout asked for, header line included.
pub fn encode_with(dm: &Datamodel, layout: Layout) -> Result<Vec<u8>, Error> {
    let version = dm.encoding_version;
    if supported_attributes(version).is_empty() {
        return Err(Error::Unsupported { encoding: dm.encoding.clone(), version });
    }
    // Upstream's `Datamodel.Save` refuses a blank format and its `Format` setter a name with a
    // space. This rejects any whitespace character (tab, newline, ...), a deliberate tightening:
    // the header reader splits on whitespace, so such a name would write a header this crate's
    // own reader rejects.
    if dm.format.trim().is_empty() {
        return Err(Error::Encode("cannot save while Datamodel.Format is blank".into()));
    }
    if dm.format.chars().any(char::is_whitespace) {
        return Err(Error::Encode(format!("format name {:?} contains whitespace", dm.format)));
    }
    // Upstream's Datamodel refuses to store an element as a prefix attribute, so its encoder never
    // meets one; this refuses the model instead of writing a reference that has no index.
    if dm.prefix_attributes.iter().any(|(_, attribute)| matches!(attribute, Attribute::Element(Some(_)))) {
        return Err(Error::Encode("elements are not supported as prefix attributes".into()));
    }
    // A root the arena does not hold is a broken model, whatever the layout does with it: Valve's
    // walk never looks at it, upstream's starts from it.
    if let Some(root) = dm.root {
        element_at(dm, root)?;
    }

    let dict = match layout {
        Layout::Valve => StringDict::seeded(version, &dm.binary_string_table),
        Layout::Upstream => StringDict::new(version),
    };
    let mut encoder = Encoder {
        dm,
        layout,
        version,
        dict,
        order: Vec::new(),
        indices: HashMap::new(),
        folded: None,
        out: Vec::new(),
    };
    encoder.run()?;
    Ok(encoder.out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every scalar id of every version maps back to its own kind, its array id is the scalar id
    /// plus the offset that version writes with, and a kind the list names twice (`Vector3`, which
    /// is also the angle slot) encodes with its first id.
    #[test]
    fn ids_round_trip() {
        for version in [1, 2, 3, 4, 5, 9] {
            let list = supported_attributes(version);
            let offset = (list.len() * if version >= 9 { 2 } else { 1 }) as u8;
            for (index, entry) in list.iter().enumerate() {
                let Some(kind) = entry else { continue };
                let slot = index as u8 + 1;

                assert_eq!(id_to_kind(slot, version).unwrap(), (*kind, false), "v{version} id {slot}");
                let first = list.iter().position(|entry| entry == &Some(*kind)).expect("the slot itself") as u8 + 1;
                assert_eq!(kind.id(version).unwrap(), first, "v{version} {kind:?} writes its first slot");

                if *kind == Kind::Byte && version < 9 {
                    continue; // there is no `uint8_array` id in these versions
                }
                assert_eq!(id_to_kind(first + offset, version).unwrap(), (*kind, true), "v{version} {kind:?}");
                assert_eq!(id_to_kind(slot + offset, version).unwrap(), (*kind, true), "v{version} id {}", slot + offset);
            }
        }
    }

    /// The ids are the ones upstream's tables give, including the reserved slot of versions 1-2.
    #[test]
    fn ids_match_upstream_tables() {
        assert_eq!(Kind::Element.id(9).unwrap(), 1);
        assert_eq!(Kind::Binary.id(9).unwrap(), 6);
        assert_eq!(Kind::QAngle.id(9).unwrap(), 12, "an angle has its own id in version 9");
        assert_eq!(Kind::Color.id(2).unwrap(), 8, "ObjectID keeps id 7 reserved");
        assert_eq!(Kind::Vector3.id(2).unwrap(), 10, "Vector3's first entry wins over the angle one");
        assert_eq!(
            Kind::Time.id(2),
            Err(Error::Encode("\"Time\" is not supported in encoding binary 2".into()))
        );
        assert_eq!(
            Kind::QAngle.id(5),
            Err(Error::Encode("\"QAngle\" is not supported in encoding binary 5".into()))
        );
        assert_eq!(
            Kind::UInt64.id(1),
            Err(Error::Encode("\"UInt64\" is not supported in encoding binary 1".into()))
        );
        assert_eq!(Kind::Byte.id(5), Err(Error::Encode("\"Byte\" is not supported in encoding binary 5".into())));

        let byte_array = Attribute::ByteArray(vec![1, 2, 3]);
        assert_eq!(array_id(&byte_array, 9).unwrap(), 48, "uint8_array");
        assert_eq!(array_id(&byte_array, 5).unwrap(), 6, "binary before version 9");
        assert_eq!(array_id(&Attribute::BinaryArray(vec![]), 9).unwrap(), 38);
        assert_eq!(array_id(&Attribute::BinaryArray(vec![]), 5).unwrap(), 20);
        assert_eq!(array_id(&Attribute::ElementArray(vec![]), 9).unwrap(), 33);
        assert_eq!(array_id(&Attribute::ElementArray(vec![]), 2).unwrap(), 15);
        assert_eq!(array_id(&Attribute::QAngleArray(vec![]), 9).unwrap(), 44);
        assert!(array_id(&Attribute::QAngleArray(vec![]), 5).is_err());
    }

    /// Version 9 accepts the pre-9 array ids as arrays too, which is upstream's fall-through.
    #[test]
    fn version_9_accepts_both_array_ranges() {
        assert_eq!(id_to_kind(33, 9).unwrap(), (Kind::Element, true));
        assert_eq!(id_to_kind(17, 9).unwrap(), (Kind::Element, true));
        assert_eq!(id_to_kind(48, 9).unwrap(), (Kind::Byte, true));
        assert_eq!(id_to_kind(32, 9).unwrap(), (Kind::Byte, true));
        assert!(id_to_kind(49, 9).is_err());
        assert!(id_to_kind(0, 9).is_err());
        assert!(id_to_kind(7, 2).is_err(), "the reserved ObjectID id");
        assert!(id_to_kind(29, 2).is_err());
    }

    /// A tick count read from a file is written back unchanged, and the int32 conversion wraps the
    /// way upstream's unchecked cast does.
    #[test]
    fn time_ticks_round_trip() {
        assert_eq!(time_to_ticks(Time(0.0)), 0);
        assert_eq!(time_to_ticks(Time(300.0)), 3_000_000);
        assert_eq!(time_to_ticks(Time(214_748.365)), -2_147_483_646, "wraps like the unchecked C# cast");

        for ticks in [0, 1, -1, 7, 10_000, 3_000_001, -3_000_001, i32::MAX, i32::MIN] {
            let seconds = f64::from(ticks) / TICKS_PER_SECOND;
            assert_eq!(time_to_ticks(Time(seconds)), ticks, "{ticks} ticks");
        }
    }
}
