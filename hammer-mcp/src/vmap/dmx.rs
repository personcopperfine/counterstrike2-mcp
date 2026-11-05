//! The `.vmap` view of a binary DMX (`encoding binary 9`) file: the tree the oracle
//! `bridge/vmapreader/vmap_dmx.py` (`DmxFile.parse`) builds, decoded by [`cs2_dmx`]. The
//! map-specific entity view built on top of the tree lives in [`super`]. Plan: entities-plan.md
//! §2 / §4.1 WP-R.
//!
//! [`cs2_dmx`] does the decoding; this module keeps the oracle's rules on top of it:
//!
//! * only `binary 9` is read ([`VmapError::UnsupportedEncoding`] /
//!   [`VmapError::UnsupportedBinaryVersion`] for the rest), although [`cs2_dmx`] reads more;
//! * the parse must consume the file exactly, otherwise [`VmapError::TrailingData`] ([`cs2_dmx`]
//!   ignores trailing bytes the way upstream does, so the count comes from
//!   [`cs2_dmx::codec::binary::decode_counted`]);
//! * GUID text is `uuid.UUID(bytes_le=...)`, which is [`cs2_dmx::Guid`]'s .NET "D" form;
//! * a `-2` stub reference to a GUID no element declares becomes an element appended after the
//!   declared ones (in first-reference order), with class and name `""` like the oracle's
//!   `_resolve_stubs` (`cs2_dmx` names it `DmeElement` / `Stub element`, like upstream).
//!
//! Differences from the oracle that come from [`cs2_dmx`] (none of them occurs in a file Hammer
//! writes): an attribute name repeated on one element keeps one entry, in its first position, with
//! the last value (upstream's `AttributeList`); a binary-array item of length `-1` is an error
//! rather than `null`; ids 17..=32 decode as their binary-v9 array kinds rather than failing; a
//! stub GUID must be valid GUID text; a duplicate element GUID is [`VmapError::Malformed`].
//!
//! Not ported (the entity view does not need it): the oracle's prefix-element detection
//! (`_find_prefix_element`), parent/owner links, the world/orphan walks and its substring
//! `find_entities`. The mesh view builds its own, narrower `children`-only parent map for the file
//! route's ancestor gate ([`super::mesh`] `children_parents`).
//!
//! Deviation: the oracle decodes strings as UTF-8 with `surrogateescape` (invalid bytes survive
//! as lone surrogates that only a byte-exact writer could round-trip); Rust strings are valid
//! UTF-8, so invalid bytes become U+FFFD here. Every map Hammer writes is valid UTF-8.

use cs2_dmx::Attribute;
use serde_json::{Map, Value as Json};
use std::collections::HashMap;

use super::VmapError;

/// The binary DMX version this view accepts (oracle `BINARY_VERSION`).
pub const BINARY_VERSION: i32 = 9;

/// One attribute value (or array item) of a DMX element.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `null` element reference (`-1`).
    Null,
    /// Element reference, by position in [`DmxDocument::elements`].
    Element(usize),
    Int(i32),
    Float(f32),
    Bool(bool),
    Str(String),
    Binary(Vec<u8>),
    /// `time`: int32 ticks of 1/10000 s.
    Time(i32),
    Color([u8; 4]),
    Vec2([f32; 2]),
    Vec3([f32; 3]),
    Vec4([f32; 4]),
    QAngle([f32; 3]),
    Quat([f32; 4]),
    Matrix([f32; 16]),
    U64(u64),
    Byte(u8),
    Array(Vec<Value>),
}

impl Value {
    /// The scalar as an `i64` when it is an integer-ish value (oracle `isinstance(v, int)`).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v as i64),
            Value::Time(v) => Some(*v as i64),
            Value::U64(v) => i64::try_from(*v).ok(),
            Value::Byte(v) => Some(*v as i64),
            _ => None,
        }
    }

    /// The scalar as a `u64` when it is a non-negative integer-ish value.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::U64(v) => Some(*v),
            _ => u64::try_from(self.as_i64()?).ok(),
        }
    }

    /// The scalar as a 3-vector (oracle `_as_vec3`: `(tuple, list) and len == 3`).
    pub fn as_vec3(&self) -> Option<[f32; 3]> {
        match self {
            Value::Vec3(v) => Some(*v),
            Value::QAngle(v) => Some(*v),
            _ => None,
        }
    }

    /// JSON view (oracle `_json_value`); element values are expanded up to `depth` 4.
    pub fn json_value(&self, elements: &[DmxElement], depth: usize) -> Json {
        match self {
            Value::Null => Json::Null,
            Value::Int(v) => Json::from(*v),
            Value::Time(v) => Json::from(*v),
            Value::Byte(v) => Json::from(*v),
            Value::U64(v) => Json::from(*v),
            Value::Float(v) => Json::from(*v as f64),
            Value::Bool(v) => Json::from(*v),
            Value::Str(v) => Json::from(v.as_str()),
            // oracle: {"__binary__": len(v)} — the bytes themselves never reach the MCP client
            Value::Binary(v) => {
                let mut map = Map::new();
                map.insert("__binary__".into(), Json::from(v.len()));
                Json::Object(map)
            }
            Value::Color(v) => Json::Array(v.iter().map(|b| Json::from(*b)).collect()),
            Value::Vec2(v) => Json::Array(v.iter().map(|f| Json::from(*f as f64)).collect()),
            Value::Vec3(v) => Json::Array(v.iter().map(|f| Json::from(*f as f64)).collect()),
            Value::Vec4(v) => Json::Array(v.iter().map(|f| Json::from(*f as f64)).collect()),
            Value::QAngle(v) => Json::Array(v.iter().map(|f| Json::from(*f as f64)).collect()),
            Value::Quat(v) => Json::Array(v.iter().map(|f| Json::from(*f as f64)).collect()),
            Value::Matrix(v) => Json::Array(v.iter().map(|f| Json::from(*f as f64)).collect()),
            Value::Array(items) => Json::Array(
                items
                    .iter()
                    .map(|item| item.json_value(elements, depth + 1))
                    .collect(),
            ),
            Value::Element(index) => elements
                .get(*index)
                .map(|element| element.json_value(elements, depth))
                .unwrap_or(Json::Null),
        }
    }

    /// Text view (oracle `_value_to_text`): the form entity keyvalues are compared in.
    pub fn to_text(&self, elements: &[DmxElement]) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bool(v) => if *v { "1" } else { "0" }.to_string(),
            Value::Str(v) => v.clone(),
            Value::Int(v) => v.to_string(),
            Value::Time(v) => v.to_string(),
            Value::Byte(v) => v.to_string(),
            Value::U64(v) => v.to_string(),
            Value::Float(v) => py_repr(*v as f64),
            Value::Vec2(v) => join_floats(v),
            Value::Vec3(v) => join_floats(v),
            Value::Vec4(v) => join_floats(v),
            Value::QAngle(v) => join_floats(v),
            Value::Quat(v) => join_floats(v),
            Value::Matrix(v) => join_floats(v),
            Value::Array(items) => items
                .iter()
                .map(|item| item.to_text(elements))
                .collect::<Vec<_>>()
                .join(" "),
            Value::Color(v) => v
                .iter()
                .map(|b| b.to_string())
                .collect::<Vec<_>>()
                .join(" "),
            Value::Binary(v) => v.iter().map(|b| format!("{b:02x}")).collect(),
            Value::Element(index) => match elements.get(*index) {
                Some(element) => format!("<element {} {}>", element.class_name, element.guid),
                None => String::new(),
            },
        }
    }
}

/// One DMX element: class name, element name, GUID and attributes in file order.
#[derive(Debug, Clone, PartialEq)]
pub struct DmxElement {
    /// Position in [`DmxDocument::elements`].
    pub index: usize,
    pub class_name: String,
    pub name: String,
    pub guid: String,
    /// Stands in for a stub reference no element declares (oracle class `""`).
    pub stub: bool,
    /// Attributes in file order (the oracle keeps its dict in insertion order).
    pub attributes: Vec<(String, Value)>,
}

impl DmxElement {
    /// Attribute by name (oracle `DmxElement.get`).
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    fn json_value(&self, elements: &[DmxElement], depth: usize) -> Json {
        let mut map = Map::new();
        map.insert("class".into(), Json::from(self.class_name.as_str()));
        map.insert("guid".into(), Json::from(self.guid.as_str()));
        if depth <= 4 {
            map.insert("name".into(), Json::from(self.name.as_str()));
            let mut attributes = Map::new();
            for (key, value) in &self.attributes {
                attributes.insert(key.clone(), value.json_value(elements, depth + 1));
            }
            map.insert("attributes".into(), Json::Object(attributes));
        }
        Json::Object(map)
    }
}

/// A parsed DMX file: header, v9 prefix attributes and the element index (bodies decoded).
#[derive(Debug, Clone)]
pub struct DmxDocument {
    pub header: String,
    pub encoding: String,
    pub encoding_version: i32,
    pub format: String,
    pub format_version: i32,
    /// Attributes of the first v9 prefix element (oracle `prefix_attributes`).
    pub prefix_attributes: Vec<(String, Value)>,
    /// Elements as written by the element index, then one element per stub GUID no element
    /// declares, in first-reference order (those have `stub = true`).
    pub elements: Vec<DmxElement>,
}

/// Parse a whole binary-v9 DMX file. The caller owns the "file changed while reading" check.
pub fn parse(data: &[u8]) -> Result<DmxDocument, VmapError> {
    let (header, header_len) =
        cs2_dmx::codec::parse_header(data).map_err(|error| malformed(0, error.to_string()))?;
    if header.encoding != "binary" {
        return Err(VmapError::UnsupportedEncoding {
            found: header.encoding,
        });
    }
    if header.encoding_version != BINARY_VERSION {
        return Err(VmapError::UnsupportedBinaryVersion {
            version: header.encoding_version,
        });
    }
    let rest = &data[header_len..];
    let (model, consumed) =
        cs2_dmx::codec::binary::decode_counted(&header, rest).map_err(|error| match error {
            // `at` is an offset into `rest`
            cs2_dmx::Error::Decode { at, message } => {
                let offset = at.parse::<u64>().map_or(0, |at| at + header_len as u64);
                malformed(offset, message)
            }
            other => malformed(0, other.to_string()),
        })?;
    if consumed != rest.len() {
        return Err(VmapError::TrailingData {
            count: (rest.len() - consumed) as u64,
        });
    }

    // The header line as the oracle prints it: everything up to the NUL (the decode found it).
    let header_text = match data.iter().position(|&b| b == 0) {
        Some(end) => String::from_utf8_lossy(&data[..end]).into_owned(),
        None => String::new(),
    };
    let elements = model
        .elements
        .iter()
        .enumerate()
        .map(|(index, element)| DmxElement {
            index,
            // the oracle's synthetic stub has no class and no name
            class_name: if element.stub {
                String::new()
            } else {
                element.class_name.clone()
            },
            name: if element.stub {
                String::new()
            } else {
                element.name.clone()
            },
            guid: element.id.to_string(),
            stub: element.stub,
            attributes: attributes(&element.attributes),
        })
        .collect();
    Ok(DmxDocument {
        header: header_text,
        encoding: model.encoding,
        encoding_version: model.encoding_version,
        format: model.format,
        format_version: model.format_version,
        prefix_attributes: attributes(&model.prefix_attributes),
        elements,
    })
}

impl DmxDocument {
    /// Element GUID -> index.
    pub fn by_guid(&self) -> HashMap<String, usize> {
        self.elements
            .iter()
            .map(|element| (element.guid.clone(), element.index))
            .collect()
    }
}

fn attributes(list: &[(String, Attribute)]) -> Vec<(String, Value)> {
    list.iter()
        .map(|(name, attribute)| (name.clone(), value(attribute)))
        .collect()
}

/// A [`cs2_dmx`] attribute in the oracle's shape: arrays are [`Value::Array`] of scalars, a time
/// is its int32 ticks, a matrix is its 16 floats in row order.
fn value(attribute: &Attribute) -> Value {
    fn array<T>(items: &[T], each: impl Fn(&T) -> Value) -> Value {
        Value::Array(items.iter().map(each).collect())
    }
    fn element(reference: &Option<cs2_dmx::ElementId>) -> Value {
        match reference {
            Some(id) => Value::Element(id.0),
            None => Value::Null,
        }
    }
    fn ticks(time: &cs2_dmx::Time) -> Value {
        // the decoder made the seconds from int32 ticks, so this is exact
        Value::Time((time.0 * 10_000.0).round() as i64 as i32)
    }
    fn matrix(rows: &[[f32; 4]; 4]) -> Value {
        let mut flat = [0f32; 16];
        for (slot, value) in flat.iter_mut().zip(rows.iter().flatten()) {
            *slot = *value;
        }
        Value::Matrix(flat)
    }
    match attribute {
        Attribute::Element(reference) => element(reference),
        Attribute::Int(v) => Value::Int(*v),
        Attribute::Float(v) => Value::Float(*v),
        Attribute::Bool(v) => Value::Bool(*v),
        Attribute::String(v) => Value::Str(v.clone()),
        Attribute::Binary(v) => Value::Binary(v.clone()),
        Attribute::Time(v) => ticks(v),
        Attribute::Color(v) => Value::Color(*v),
        Attribute::Vector2(v) => Value::Vec2(*v),
        Attribute::Vector3(v) => Value::Vec3(*v),
        Attribute::Vector4(v) => Value::Vec4(*v),
        Attribute::QAngle(v) => Value::QAngle(*v),
        Attribute::Quaternion(v) => Value::Quat(*v),
        Attribute::Matrix(v) => matrix(v),
        Attribute::UInt64(v) => Value::U64(*v),
        Attribute::Byte(v) => Value::Byte(*v),
        Attribute::ElementArray(items) => array(items, element),
        Attribute::IntArray(items) => array(items, |v| Value::Int(*v)),
        Attribute::FloatArray(items) => array(items, |v| Value::Float(*v)),
        Attribute::BoolArray(items) => array(items, |v| Value::Bool(*v)),
        Attribute::StringArray(items) => array(items, |v| Value::Str(v.clone())),
        Attribute::BinaryArray(items) => array(items, |v| Value::Binary(v.clone())),
        Attribute::TimeArray(items) => array(items, ticks),
        Attribute::ColorArray(items) => array(items, |v| Value::Color(*v)),
        Attribute::Vector2Array(items) => array(items, |v| Value::Vec2(*v)),
        Attribute::Vector3Array(items) => array(items, |v| Value::Vec3(*v)),
        Attribute::Vector4Array(items) => array(items, |v| Value::Vec4(*v)),
        Attribute::QAngleArray(items) => array(items, |v| Value::QAngle(*v)),
        Attribute::QuaternionArray(items) => array(items, |v| Value::Quat(*v)),
        Attribute::MatrixArray(items) => array(items, matrix),
        Attribute::UInt64Array(items) => array(items, |v| Value::U64(*v)),
        Attribute::ByteArray(items) => array(items, |v| Value::Byte(*v)),
    }
}

fn malformed(offset: u64, reason: impl Into<String>) -> VmapError {
    VmapError::Malformed {
        offset,
        reason: reason.into(),
    }
}

/// Text form of a float array (oracle joins tuples/lists with spaces).
fn join_floats(values: &[f32]) -> String {
    values
        .iter()
        .map(|value| py_repr(*value as f64))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Python `repr` of a float, for the text form of non-string values.
///
/// `Display` for `f64` prints an integral value without the `.0` Python adds (`1` vs `1.0`), so
/// that case is spelled out; the exponent spelling of very large/small magnitudes (`1e30` vs
/// `1e+30`) can still differ. Only non-string `classname`/`targetname` attributes go through it —
/// entity keyvalues use [`Value::json_value`], like the oracle's `to_dict`.
fn py_repr(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e16 {
        format!("{value:.1}")
    } else {
        value.to_string()
    }
}

/// Round a float32 like the oracle's `_round_f32` (`float("%.9g" % x)`, 9 significant digits).
///
/// `{:.8e}` is 9 significant digits and correctly rounded, which parses back to the same double
/// for every finite float32 (checked against `%.9g` over 20 000 bit patterns); `serde_json` and
/// `json.dumps` then both print the shortest round-tripping form of that double.
pub fn round_f32(value: f32) -> f64 {
    let wide = value as f64;
    format!("{wide:.8e}").parse::<f64>().unwrap_or(wide)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_text_is_little_endian_like_python_uuid() {
        // uuid.UUID('640359e1-31ba-4869-a25f-1ff83bf4de4a').bytes_le
        let bytes = [
            0xe1, 0x59, 0x03, 0x64, 0xba, 0x31, 0x69, 0x48, 0xa2, 0x5f, 0x1f, 0xf8, 0x3b, 0xf4,
            0xde, 0x4a,
        ];
        assert_eq!(
            cs2_dmx::Guid(bytes).to_string(),
            "640359e1-31ba-4869-a25f-1ff83bf4de4a"
        );
    }

    #[test]
    fn an_undeclared_stub_is_an_empty_element_after_the_declared_ones() {
        // string table ["x", "a"], one element (class "x", name "x"), one attribute `a` holding
        // a `-2` stub reference
        let mut data = b"<!-- dmx encoding binary 9 format vmap 40 -->\n\0".to_vec();
        data.extend_from_slice(&0i32.to_le_bytes()); // v9 prefix elements
        data.extend_from_slice(&2i32.to_le_bytes());
        data.extend_from_slice(b"x\0a\0");
        data.extend_from_slice(&1i32.to_le_bytes());
        data.extend_from_slice(&0i32.to_le_bytes());
        data.extend_from_slice(&0i32.to_le_bytes());
        data.extend_from_slice(&[7u8; 16]);
        data.extend_from_slice(&1i32.to_le_bytes()); // one attribute
        data.extend_from_slice(&1i32.to_le_bytes()); // name "a"
        data.push(1); // element
        data.extend_from_slice(&(-2i32).to_le_bytes());
        data.extend_from_slice(b"640359e1-31ba-4869-a25f-1ff83bf4de4a\0");
        let document = parse(&data).expect("parses");
        assert_eq!(document.elements.len(), 2);
        assert_eq!(document.elements[0].get("a"), Some(&Value::Element(1)));
        let stub = &document.elements[1];
        assert!(stub.stub);
        assert_eq!(
            (stub.index, stub.class_name.as_str(), stub.name.as_str()),
            (1, "", "")
        );
        assert_eq!(stub.guid, "640359e1-31ba-4869-a25f-1ff83bf4de4a");
    }

    #[test]
    fn round_f32_matches_python_nine_significant_digits() {
        assert_eq!(round_f32(0.0).to_bits(), 0.0f64.to_bits());
        assert_eq!(round_f32(1.5).to_bits(), 1.5f64.to_bits());
        // the f32 `probe.vmap` stores for an angle whose exact value is 89.99999237060547
        let angle = f32::from_bits(0x42b3_ffff);
        assert_eq!(angle as f64, 89.99999237060547f64);
        assert_eq!(round_f32(angle), 89.9999924f64);
        assert_eq!(round_f32(-320.0).to_bits(), (-320.0f64).to_bits());
    }

    #[test]
    fn rejects_non_dmx_and_keyvalues2() {
        assert!(matches!(
            parse(b"not a dmx file"),
            Err(VmapError::Malformed { .. })
        ));
        let header = b"<!-- dmx encoding keyvalues2 1 format vmap 40 -->\0";
        assert!(matches!(
            parse(header),
            Err(VmapError::UnsupportedEncoding { .. })
        ));
        let header = b"<!-- dmx encoding binary 8 format vmap 40 -->\0";
        assert!(matches!(
            parse(header),
            Err(VmapError::UnsupportedBinaryVersion { version: 8 })
        ));
    }
}
