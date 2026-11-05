//! `cs2-dmx` — Valve Datamodel (DMX): `.vmap`, `.dmx` meshes/animations, `.pcf`.
//!
//! A Rust port of [Datamodel.NET](https://github.com/ValveResourceFormat/Datamodel.NET)
//! (MIT, © 2013 Tom Edwards). Codecs: `binary` 1, 2, 3, 4, 5, 9 and `keyvalues2` /
//! `keyvalues2_noids` 1–4, read **and** write. Deliberately not ported: the deferred-load mode,
//! the reflection/ElementFactory typed-element layer, the `ElementSchema` validator.
//!
//! [`save`] writes the binary layout Valve's own `CDmSerializerBinary` writes, so a file read from
//! a game or from Hammer comes back byte for byte; [`codec::binary::encode_with`] with
//! [`codec::binary::Layout::Upstream`] writes the layout upstream's `Encoder` writes instead, which
//! is what the upstream test vectors pin.
//!
//! The model is an arena: [`Datamodel::elements`] owns every [`Element`], and an element
//! reference is an [`ElementId`] (index into that `Vec`). Element GUIDs live on the element.
//! A stub (an element referenced by GUID but defined in another file) is an `Element` with
//! `stub == true` and no attributes, exactly like upstream `Element.Stub`.

#![forbid(unsafe_code)]

pub mod codec;

pub use codec::{load, save, Header};

/// Index into [`Datamodel::elements`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ElementId(pub usize);

/// 16 GUID bytes in **stream order** (binary DMX writes .NET `Guid.ToByteArray()`: the first
/// three fields little-endian). [`Guid::to_string`] gives the .NET text form
/// (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, the way keyvalues2 and Hammer print it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Guid(pub [u8; 16]);

/// The text a [`Guid`] could not be parsed from.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("bad GUID text {0:?}")]
pub struct GuidParseError(pub String);

impl std::fmt::Display for Guid {
    /// .NET's "D" format: the first field (4 bytes), then 2 + 2 + 2 fields, all little-endian,
    /// then the last eight bytes raw — the form `Guid.ToString()` prints and DMX stores in text.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
        )
    }
}

impl std::str::FromStr for Guid {
    type Err = GuidParseError;

    /// Parses the text forms .NET's `Guid(string)` accepts that a DMX file can hold: "D"
    /// (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, what [`Guid`]'s `Display` writes), "N" (32 hex
    /// digits), "B" (`{…}`) and "P" (`(…)`), with surrounding whitespace. .NET's "X" form
    /// (`{0x…}`) is not accepted; no DMX writer emits it.
    fn from_str(text: &str) -> Result<Guid, GuidParseError> {
        let trimmed = text.trim();
        let inner = trimmed
            .strip_prefix('{')
            .and_then(|rest| rest.strip_suffix('}'))
            .or_else(|| trimmed.strip_prefix('(').and_then(|rest| rest.strip_suffix(')')))
            .unwrap_or(trimmed);
        let digits: Vec<u8> = inner
            .bytes()
            .filter(|b| *b != b'-')
            .map(|b| match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                b'A'..=b'F' => Ok(b - b'A' + 10),
                _ => Err(GuidParseError(text.to_string())),
            })
            .collect::<Result<_, _>>()?;
        if digits.len() != 32 {
            return Err(GuidParseError(text.to_string()));
        }
        let hex = |range: std::ops::Range<usize>| -> u64 {
            digits[range].iter().fold(0u64, |acc, d| (acc << 4) | u64::from(*d))
        };
        let mut bytes = [0u8; 16];
        // The text's first three fields are little-endian in the stream, the rest is raw.
        bytes[0..4].copy_from_slice(&(hex(0..8) as u32).to_le_bytes());
        bytes[4..6].copy_from_slice(&(hex(8..12) as u16).to_le_bytes());
        bytes[6..8].copy_from_slice(&(hex(12..16) as u16).to_le_bytes());
        for (i, pair) in digits[16..32].chunks(2).enumerate() {
            bytes[8 + i] = (pair[0] << 4) | pair[1];
        }
        Ok(Guid(bytes))
    }
}

/// The whole file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Datamodel {
    /// `binary`, `keyvalues2`, `keyvalues2_noids`.
    pub encoding: String,
    pub encoding_version: i32,
    /// `vmap`, `model`, `pcf`, `dmx`, …
    pub format: String,
    pub format_version: i32,
    /// Binary 9 "prefix attributes" (upstream `Datamodel.PrefixAttributes`), in stream order.
    pub prefix_attributes: Vec<(String, Attribute)>,
    /// The string table a binary file carried, in file order, exactly as read (a table is
    /// append-only history: it can hold strings no attribute uses any more, and the entry a body
    /// names is the one the file names). Empty for a new model and for keyvalues2, which has no
    /// table.
    ///
    /// [`codec::save`] writes this table first and appends the strings the model needs but the
    /// table does not hold, which is what makes a load and a save reproduce a Valve file byte for
    /// byte. The upstream-shaped [`codec::binary::Layout::Upstream`] ignores it.
    pub binary_string_table: Vec<String>,
    pub elements: Vec<Element>,
    pub root: Option<ElementId>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Element {
    pub class_name: String,
    pub name: String,
    pub id: Guid,
    pub stub: bool,
    /// Attributes in stream order (DMX attribute names are unique per element).
    pub attributes: Vec<(String, Attribute)>,
}

/// Seconds, stored by binary DMX as `int32` ticks of 1/10000 s (upstream `TimeSpan`).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct Time(pub f64);

/// Upstream `AttributeType`: the 16 scalar types, and an array of each.
#[derive(Debug, Clone, PartialEq)]
pub enum Attribute {
    Element(Option<ElementId>),
    Int(i32),
    Float(f32),
    Bool(bool),
    String(String),
    Binary(Vec<u8>),
    Time(Time),
    /// RGBA bytes.
    Color([u8; 4]),
    Vector2([f32; 2]),
    Vector3([f32; 3]),
    Vector4([f32; 4]),
    /// Pitch, yaw, roll. Binary < 9 has no QAngle id (angles are `Vector3` there).
    QAngle([f32; 3]),
    /// x, y, z, w.
    Quaternion([f32; 4]),
    /// Row-major 4×4.
    Matrix([[f32; 4]; 4]),
    UInt64(u64),
    Byte(u8),
    ElementArray(Vec<Option<ElementId>>),
    IntArray(Vec<i32>),
    FloatArray(Vec<f32>),
    BoolArray(Vec<bool>),
    StringArray(Vec<String>),
    BinaryArray(Vec<Vec<u8>>),
    TimeArray(Vec<Time>),
    ColorArray(Vec<[u8; 4]>),
    Vector2Array(Vec<[f32; 2]>),
    Vector3Array(Vec<[f32; 3]>),
    Vector4Array(Vec<[f32; 4]>),
    QAngleArray(Vec<[f32; 3]>),
    QuaternionArray(Vec<[f32; 4]>),
    MatrixArray(Vec<[[f32; 4]; 4]>),
    UInt64Array(Vec<u64>),
    ByteArray(Vec<u8>),
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Error {
    /// Not `<!-- dmx encoding X N format Y M -->`.
    #[error("bad DMX header: {0}")]
    Header(String),
    #[error("unsupported encoding {encoding} {version}")]
    Unsupported { encoding: String, version: i32 },
    /// Malformed body. `at` = byte offset (binary) or `line:column` (keyvalues2).
    #[error("{at}: {message}")]
    Decode { at: String, message: String },
    /// A value the chosen encoding/version cannot represent (upstream `CodecException`).
    #[error("cannot encode: {0}")]
    Encode(String),
}

impl Element {
    pub fn get(&self, name: &str) -> Option<&Attribute> {
        self.attributes.iter().find(|(n, _)| n == name).map(|(_, a)| a)
    }
}

impl Datamodel {
    pub fn element(&self, id: ElementId) -> &Element {
        &self.elements[id.0]
    }

    /// The element that stands for the file's prefix block, if the model has one: the first
    /// element of class `$prefix_element$` (what a keyvalues2 4 file and Hammer's binary 9 write),
    /// else the shape upstream's binary reader folds into the prefix attributes — the element
    /// right after the root, of class `DmElement`, unnamed, not a stub, holding the same keys as
    /// [`Datamodel::prefix_attributes`] (and there are some). It carries the GUID of the prefix
    /// block; a codec that writes the block itself must not write this element a second time.
    pub fn prefix_element(&self) -> Option<ElementId> {
        if let Some(position) = self.elements.iter().position(|element| element.class_name == PREFIX_ELEMENT_CLASS) {
            return Some(ElementId(position));
        }
        if self.prefix_attributes.is_empty() {
            return None;
        }
        let element = self.elements.get(1)?;
        let same_keys = element.attributes.len() == self.prefix_attributes.len()
            && element.attributes.iter().zip(self.prefix_attributes.iter()).all(|((left, _), (right, _))| left == right);
        (!element.stub && element.class_name == "DmElement" && element.name.is_empty() && same_keys)
            .then_some(ElementId(1))
    }
}

/// The class of the element that carries a file's prefix attributes (see
/// [`Datamodel::prefix_element`]).
const PREFIX_ELEMENT_CLASS: &str = "$prefix_element$";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_text_is_dotnet_d_format() {
        // uuid.UUID('640359e1-31ba-4869-a25f-1ff83bf4de4a').bytes_le is the stream order.
        let guid: Guid = "640359e1-31ba-4869-a25f-1ff83bf4de4a".parse().unwrap();
        assert_eq!(
            guid.0,
            [0xe1, 0x59, 0x03, 0x64, 0xba, 0x31, 0x69, 0x48, 0xa2, 0x5f, 0x1f, 0xf8, 0x3b, 0xf4, 0xde, 0x4a]
        );
        assert_eq!(guid.to_string(), "640359e1-31ba-4869-a25f-1ff83bf4de4a");
        assert_eq!(Guid::default().to_string(), "00000000-0000-0000-0000-000000000000");
        assert_eq!("640359E131BA4869A25F1FF83BF4DE4A".parse::<Guid>().unwrap(), guid, "N form");
        assert_eq!("{640359e1-31ba-4869-a25f-1ff83bf4de4a}".parse::<Guid>().unwrap(), guid, "B form");
        assert_eq!("(640359e1-31ba-4869-a25f-1ff83bf4de4a)".parse::<Guid>().unwrap(), guid, "P form");
        assert_eq!(" 640359e1-31ba-4869-a25f-1ff83bf4de4a ".parse::<Guid>().unwrap(), guid, "padded");
    }

    #[test]
    fn guid_text_rejects_junk() {
        for text in ["", "not a guid", "640359e1-31ba-4869-a25f-1ff83bf4de4", "640359e1-31ba-4869-a25f-1ff83bf4de4g"] {
            assert!(text.parse::<Guid>().is_err(), "{text:?} must not parse");
        }
        assert_eq!(Guid::default().to_string().len(), 36);
    }
}
