//! Ports of `ValveKeyValue.Test/Binary/*.cs` (the wire-format ones) plus the hostile-input,
//! depth and round-trip tests this port needs.
//!
//! Ported: `SimpleBinaryTestCase`, `BinaryObjectDeserializationTestCase`,
//! `BinaryObjectSerializationTestCase` (both cases; the typed one's bytes verbatim),
//! `BinaryObjectConsecutiveDeserializationTestCase`,
//! `BinaryObjectConsecutiveSerializationTestCase`, `BinaryWithMarkerTestCase`,
//! `StringTableTestCase`, `StringTableFromScratchTestCase`, `BinaryBlobTestCase`,
//! `WideStringTestCase`, `TruncatedDataTestCase`, `OutOfRangeNodeTypeTestCase`.
//!
//! Not ported, and why:
//!
//! * `BinaryObjectSerializationTypedTestCase` — its exact byte array is
//!   [`binary_serialization_typed_object`], and the `double` widening that upstream only
//!   round-trips is [`binary_serialization_widened_value_types`]. The rest of the case maps a C#
//!   class onto a document (`ObjectCopier`), which has no equivalent here.
//! * The typed halves of `BinaryObjectDeserializationTestCase` (`Deserialize<TestObject>`) and of
//!   `BinaryObjectConsecutiveDeserializationTestCase`: reflection over a C# object.
//! * `KVObject`-specific helpers (`AsBlob`, `Convert.ChangeType`, `KVObject.Null()`, `ListCollection`,
//!   `KVValueType.Boolean`/`Int16`/`UInt16`/`UInt32`/`FloatingPoint64`) that have no variant in the
//!   frozen data model; every one of them is noted where it would have appeared.

use cs2_kv1::binary::{self, ReadOptions, StringTable, WriteOptions, MAX_BINARY_DEPTH};
use cs2_kv1::{Document, Error, KeyValue, Object, Value};

/// `SimpleBinaryTestCase`.
const SIMPLE: &[u8] = &[
    0x00, // object: TestObject
    0x54, 0x65, 0x73, 0x74, 0x4F, 0x62, 0x6A, 0x65, 0x63, 0x74, 0x00,
    0x01, // string: key = value
    0x6B, 0x65, 0x79, 0x00, 0x76, 0x61, 0x6C, 0x75, 0x65, 0x00,
    0x02, // int32: int = 0x01020304
    0x69, 0x6E, 0x74, 0x00, 0x04, 0x03, 0x02, 0x01,
    0x03, // float32: flt = 1234.5678f
    0x66, 0x6C, 0x74, 0x00, 0x2B, 0x52, 0x9A, 0x44,
    0x04, // pointer: col = 0x10203040
    0x63, 0x6F, 0x6C, 0x00, 0x40, 0x30, 0x20, 0x10,
    0x06, // color: ptr = 0x11223344
    0x70, 0x74, 0x72, 0x00, 0x44, 0x33, 0x22, 0x11,
    0x07, // uint64: lng = 0x1122334455667788
    0x6C, 0x6E, 0x67, 0x00, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11,
    0x0A, // int64: i64 = 0x0102030405060708
    0x69, 0x36, 0x34, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01,
    0x08, // end object
    0x08, // end document
];

/// `BinaryObjectDeserializationTestCase` — note that it differs from [`SERIALIZED`]: the integer
/// is `0x01020304`, it carries a colour, and its pointer is `0x11223344`.
const DESERIALIZE: &[u8] = &[
    0x00, // object: TestObject
    0x54, 0x65, 0x73, 0x74, 0x4F, 0x62, 0x6A, 0x65, 0x63, 0x74, 0x00,
    0x01, // string: key = value
    0x6B, 0x65, 0x79, 0x00, 0x76, 0x61, 0x6C, 0x75, 0x65, 0x00,
    0x01, // string: key_utf8 = 邪恶的战
    0x6B, 0x65, 0x79, 0x5F, 0x75, 0x74, 0x66, 0x38, 0x00, 0xE9, 0x82, 0xAA, 0xE6, 0x81, 0xB6,
    0xE7, 0x9A, 0x84, 0xE6, 0x88, 0x98, 0x00,
    0x02, // int32: int = 0x01020304
    0x69, 0x6E, 0x74, 0x00, 0x04, 0x03, 0x02, 0x01,
    0x03, // float32: flt = 1234.5678f
    0x66, 0x6C, 0x74, 0x00, 0x2B, 0x52, 0x9A, 0x44,
    0x04, // pointer: ptr = 0x11223344
    0x70, 0x74, 0x72, 0x00, 0x44, 0x33, 0x22, 0x11,
    0x06, // color: col = 0x10203040
    0x63, 0x6F, 0x6C, 0x00, 0x40, 0x30, 0x20, 0x10,
    0x07, // uint64: lng = 0x1122334455667788
    0x6C, 0x6E, 0x67, 0x00, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11,
    0x0A, // int64: i64 = 0x0102030405060708
    0x69, 0x36, 0x34, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01,
    0x08, // end object
    0x08, // end document
];

/// `BinaryObjectSerializationTestCase` (the missing colour and the different integers are
/// upstream's own difference between the two cases).
const SERIALIZED: &[u8] = &[
    0x00, // object: TestObject
    0x54, 0x65, 0x73, 0x74, 0x4F, 0x62, 0x6A, 0x65, 0x63, 0x74, 0x00,
    0x01, // string: key = value
    0x6B, 0x65, 0x79, 0x00, 0x76, 0x61, 0x6C, 0x75, 0x65, 0x00,
    0x01, // string: key_utf8 = 邪恶的战
    0x6B, 0x65, 0x79, 0x5F, 0x75, 0x74, 0x66, 0x38, 0x00, 0xE9, 0x82, 0xAA, 0xE6, 0x81, 0xB6,
    0xE7, 0x9A, 0x84, 0xE6, 0x88, 0x98, 0x00,
    0x02, // int32: int = 0x10203040
    0x69, 0x6E, 0x74, 0x00, 0x40, 0x30, 0x20, 0x10,
    0x03, // float32: flt = 1234.5678f
    0x66, 0x6C, 0x74, 0x00, 0x2B, 0x52, 0x9A, 0x44,
    0x04, // pointer: ptr = 0x12345678
    0x70, 0x74, 0x72, 0x00, 0x78, 0x56, 0x34, 0x12,
    0x07, // uint64: lng = 0x8877665544332211
    0x6C, 0x6E, 0x67, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
    0x0A, // int64: i64 = 0x0102030405060708
    0x69, 0x36, 0x34, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01,
    0x08, // end object
    0x08, // end document
];

/// `BinaryObjectConsecutiveDeserializationTestCase` / `...SerializationTestCase`.
const CONSECUTIVE: &[u8] = &[
    0x00, // object: FirstObject
    0x46, 0x69, 0x72, 0x73, 0x74, 0x4F, 0x62, 0x6A, 0x65, 0x63, 0x74, 0x00,
    0x01, // string: firstkey = firstvalue
    0x66, 0x69, 0x72, 0x73, 0x74, 0x6B, 0x65, 0x79, 0x00, 0x66, 0x69, 0x72, 0x73, 0x74, 0x76,
    0x61, 0x6C, 0x75, 0x65, 0x00,
    0x08, // end object
    0x08, // end document
    0x00, // object: SecondObject
    0x53, 0x65, 0x63, 0x6F, 0x6E, 0x64, 0x4F, 0x62, 0x6A, 0x65, 0x63, 0x74, 0x00,
    0x01, // string: secondkey = secondvalue
    0x73, 0x65, 0x63, 0x6F, 0x6E, 0x64, 0x6B, 0x65, 0x79, 0x00, 0x73, 0x65, 0x63, 0x6F, 0x6E,
    0x64, 0x76, 0x61, 0x6C, 0x75, 0x65, 0x00,
    0x08, // end object
    0x08, // end document
];

/// `BinaryWithMarkerTestCase.WithMagicHeader` — plus two trailing bytes that are not read.
const WITH_MARKER: &[u8] = &[
    0x56, 0x42, 0x4B, 0x56, // VBKV
    0x00, 0x00, 0x00, 0x00, // CRC32, not verified
    0x00, // object: TestObject
    0x54, 0x65, 0x73, 0x74, 0x4F, 0x62, 0x6A, 0x65, 0x63, 0x74, 0x00,
    0x01, // string: key = value
    0x6B, 0x65, 0x79, 0x00, 0x76, 0x61, 0x6C, 0x75, 0x65, 0x00,
    0x02, // int32: int = 0x01020304
    0x69, 0x6E, 0x74, 0x00, 0x04, 0x03, 0x02, 0x01,
    0x03, // float32: flt = 1234.5678f
    0x66, 0x6C, 0x74, 0x00, 0x2B, 0x52, 0x9A, 0x44,
    0x05, // wide string: wstr = "hi", int16 length without terminator
    0x77, 0x73, 0x74, 0x72, 0x00, 0x02, 0x00, 0x68, 0x00, 0x69, 0x00,
    0x06, // color: col = 0x10203040
    0x63, 0x6F, 0x6C, 0x00, 0x40, 0x30, 0x20, 0x10,
    0x07, // uint64: lng = 0x1122334455667788
    0x6C, 0x6E, 0x67, 0x00, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11,
    0x00, // object: empty
    0x65, 0x6D, 0x70, 0x74, 0x79, 0x00,
    0x0B, // end object: empty
    0x0B, // end object: TestObject
    0x0B, // end document
    0xFF, 0xFF, // trailing data, not read
];

/// `BinaryWithMarkerTestCase.WithoutMagicHeader` — `AlternateEnd` throughout.
const ALTERNATE_END: &[u8] = &[
    0x00, // object: TestObject
    0x54, 0x65, 0x73, 0x74, 0x4F, 0x62, 0x6A, 0x65, 0x63, 0x74, 0x00,
    0x01, // string: key = value
    0x6B, 0x65, 0x79, 0x00, 0x76, 0x61, 0x6C, 0x75, 0x65, 0x00,
    0x00, // object: empty
    0x65, 0x6D, 0x70, 0x74, 0x79, 0x00,
    0x0B, // end object: empty
    0x0B, // end object: TestObject
    0x0B, // end document
];

/// `StringTableTestCase` — keys are indices into `TestStringTable`.
const STRING_TABLE: &[u8] = &[
    0x00, // object: TestObject
    0x05, 0x00, 0x00, 0x00, // stringTable[5] = "TestObject"
    0x01, // string: key = value
    0x03, 0x00, 0x00, 0x00, // stringTable[3] = "key"
    0x76, 0x61, 0x6C, 0x75, 0x65, 0x00,
    0x02, // int32: int = 0x01020304
    0x02, 0x00, 0x00, 0x00, // stringTable[2] = "int"
    0x04, 0x03, 0x02, 0x01,
    0x03, // float32: flt = 1234.5678f
    0x00, 0x00, 0x00, 0x00, // stringTable[0] = "flt"
    0x2B, 0x52, 0x9A, 0x44,
    0x07, // uint64: lng = 0x1122334455667788
    0x04, 0x00, 0x00, 0x00, // stringTable[4] = "lng"
    0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11,
    0x0A, // int64: i64 = 0x0102030405060708
    0x01, 0x00, 0x00, 0x00, // stringTable[1] = "i64"
    0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01,
    0x08, // end object
    0x08, // end document
];

/// `StringTableTestCase.TestStringTable`.
const TEST_STRING_TABLE: [&str; 6] = ["flt", "i64", "int", "key", "lng", "TestObject"];

/// `BinaryBlobTestCase.DeserializesBlob`.
const BLOB: &[u8] = &[
    0x00, // object: TestObject
    0x54, 0x65, 0x73, 0x74, 0x4F, 0x62, 0x6A, 0x65, 0x63, 0x74, 0x00,
    0x09, // binary blob: blob = 00 01 FF, uint32 length
    0x62, 0x6C, 0x6F, 0x62, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01, 0xFF,
    0x09, // binary blob: empty
    0x65, 0x6D, 0x70, 0x74, 0x79, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x08, // end object
    0x08, // end document
];

/// A `key value` pair with no condition (conditions are a text-only feature).
fn kv(key: &str, value: Value) -> KeyValue {
    KeyValue { key: key.to_string(), value, condition: None }
}

/// A document whose root is an object.
fn document(root: &str, entries: Vec<KeyValue>) -> Document {
    kv(root, Value::Object(Object { entries }))
}

/// [`binary::write`] for an ordinary document. The call is fallible (a blob over `u32::MAX` bytes
/// or a wide string over `i32::MAX` UTF-16 units does not fit the wire format), so the helper
/// keeps the tests that cannot hit those shorter.
fn write(document: &Document) -> Vec<u8> {
    binary::write(document).expect("an ordinary document fits the wire format")
}

/// The object of a root pair, for assertions.
fn root_object(document: &Document) -> &Object {
    document.value.as_object().expect("the root is an object")
}

/// The value of a key, for assertions.
fn value<'a>(document: &'a Document, key: &str) -> &'a Value {
    root_object(document).get(key).unwrap_or_else(|| panic!("no child {key:?}"))
}

/// Byte equality with a readable failure message.
fn assert_bytes(actual: &[u8], expected: &[u8], what: &str) {
    assert!(actual == expected, "{what}\n got: {actual:02X?}\nwant: {expected:02X?}");
}

#[test]
fn simple_binary_document() {
    let document = binary::parse(SIMPLE).expect("SimpleBinaryTestCase data");
    assert_eq!(document.key, "TestObject");
    assert_eq!(root_object(&document).entries.len(), 7);
    assert_eq!(value(&document, "key"), &Value::String("value".to_string()));
    assert_eq!(value(&document, "int"), &Value::Int32(0x01020304));
    assert_eq!(value(&document, "flt"), &Value::Float32(1234.5678f32));
    // Upstream's data carries a *pointer* (4) under "col" and a *colour* (6) under "ptr"; its
    // comments are swapped. A pointer arrives as an i32 ...
    assert_eq!(value(&document, "col"), &Value::Pointer(0x10203040));
    // ... and a colour keeps its four bytes in stream order, so the little-endian reading of them
    // is the integer upstream asserts (`0x11223344`).
    let color = match value(&document, "ptr") {
        Value::Color(bytes) => *bytes,
        other => panic!("expected a colour, got {other:?}"),
    };
    assert_eq!(color, [0x44, 0x33, 0x22, 0x11]);
    assert_eq!(i32::from_le_bytes(color), 0x11223344);
    assert_eq!(value(&document, "lng"), &Value::UInt64(0x1122334455667788));
    assert_eq!(value(&document, "i64"), &Value::Int64(0x0102030405060708));
}

#[test]
fn binary_object_deserialization() {
    let document = binary::parse(DESERIALIZE).expect("deserialization test data");
    assert_eq!(document.key, "TestObject");
    assert_eq!(value(&document, "key"), &Value::String("value".to_string()));
    assert_eq!(value(&document, "key_utf8"), &Value::String("邪恶的战".to_string()));
    assert_eq!(value(&document, "int"), &Value::Int32(0x01020304));
    assert_eq!(value(&document, "flt"), &Value::Float32(1234.5678f32));
    assert_eq!(value(&document, "ptr"), &Value::Pointer(0x11223344));
    assert_eq!(value(&document, "col"), &Value::Color([0x40, 0x30, 0x20, 0x10]));
    assert_eq!(value(&document, "lng"), &Value::UInt64(0x1122334455667788));
    assert_eq!(value(&document, "i64"), &Value::Int64(0x0102030405060708));
}

#[test]
fn binary_object_serialization() {
    let document = document(
        "TestObject",
        vec![
            kv("key", Value::String("value".to_string())),
            kv("key_utf8", Value::String("邪恶的战".to_string())),
            kv("int", Value::Int32(0x10203040)),
            kv("flt", Value::Float32(1234.5678f32)),
            kv("ptr", Value::Pointer(0x12345678)),
            kv("lng", Value::UInt64(0x8877665544332211)),
            kv("i64", Value::Int64(0x0102030405060708)),
        ],
    );

    assert_bytes(&write(&document), SERIALIZED, "BinaryObjectSerializationTestCase bytes");

    // ... and the written bytes read back as the same document.
    let reparsed = binary::parse(&write(&document)).expect("our own output");
    assert_eq!(reparsed, document);
}

/// `BinaryObjectSerializationTypedTestCase` — the upstream byte array verbatim. It is also the
/// case that pins how C# widens `short` to `Int32` and `ushort`/`uint` to `UInt64` before writing
/// (tags 0x02 and 0x07 at the end).
const TYPED_SERIALIZED: &[u8] = &[
    0x00, // object: TestObject
    0x54, 0x65, 0x73, 0x74, 0x4F, 0x62, 0x6A, 0x65, 0x63, 0x74, 0x00,
    0x01, // string: key = value
    0x6B, 0x65, 0x79, 0x00,
    0x76, 0x61, 0x6C, 0x75, 0x65, 0x00,
    0x01, // string: key_utf8 = 邪恶的战
    0x6B, 0x65, 0x79, 0x5F, 0x75, 0x74, 0x66, 0x38, 0x00,
    0xE9, 0x82, 0xAA, 0xE6, 0x81, 0xB6, 0xE7, 0x9A, 0x84, 0xE6, 0x88, 0x98, 0x00,
    0x02, // int32: int = 0x10203040
    0x69, 0x6E, 0x74, 0x00,
    0x40, 0x30, 0x20, 0x10,
    0x03, // float32: flt = 1234.5678f
    0x66, 0x6C, 0x74, 0x00,
    0x2B, 0x52, 0x9A, 0x44,
    0x04, // pointer: ptr = 0x12345678
    0x70, 0x74, 0x72, 0x00,
    0x78, 0x56, 0x34, 0x12,
    0x07, // uint64: lng = 0x8877665544332211
    0x6C, 0x6E, 0x67, 0x00,
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
    0x0A, // int64: i64 = 0x0102030405060708
    0x69, 0x36, 0x34, 0x00,
    0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01,
    0x02, // int32: i16 = short.MaxValue
    0x69, 0x31, 0x36, 0x00,
    0xFF, 0x7F, 0x00, 0x00,
    0x07, // uint64: u16 = ushort.MaxValue
    0x75, 0x31, 0x36, 0x00,
    0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x07, // uint64: u32 = uint.MaxValue
    0x75, 0x33, 0x32, 0x00,
    0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00,
    0x08, // end object
    0x08, // end document
];

#[test]
fn binary_serialization_typed_object() {
    // `BinaryObjectSerializationTypedTestCase.SerializesToBinaryStructure`: the literal upstream
    // byte array, widened value types included. `bool`, `null` and a `Blob` have no equivalent in
    // the frozen model; a blob would be type 9 here rather than the hex string upstream writes.
    let document = document(
        "TestObject",
        vec![
            kv("key", Value::String("value".to_string())),
            kv("key_utf8", Value::String("邪恶的战".to_string())),
            kv("int", Value::Int32(0x10203040)),
            kv("flt", Value::Float32(1234.5678f32)),
            kv("ptr", Value::Pointer(0x12345678)),
            kv("lng", Value::UInt64(0x8877665544332211)),
            kv("i64", Value::Int64(0x0102030405060708)),
            // C# widens `short` (Int16) to Int32 and `ushort`/`uint` to UInt64.
            kv("i16", Value::Int32(i16::MAX as i32)),
            kv("u16", Value::UInt64(u16::MAX as u64)),
            kv("u32", Value::UInt64(u32::MAX as u64)),
        ],
    );
    let written = write(&document);
    assert_bytes(&written, TYPED_SERIALIZED, "BinaryObjectSerializationTypedTestCase bytes");
    assert_eq!(binary::parse(&written).expect("our own output"), document);
}

#[test]
fn binary_serialization_widened_value_types() {
    // `NewValueTypesAreWidenedInBinarySerialization` also widens `double` to a Float32, which has
    // no byte array upstream; 0.1f32 is 0x3DCCCCCD, so the payload below is literal IEEE-754.
    let document = document("Test", vec![kv("f64", Value::Float32(0.1f64 as f32))]);
    assert_bytes(
        &write(&document),
        &[
            0x00, b'T', b'e', b's', b't', 0x00, // object: Test
            0x03, b'f', b'6', b'4', 0x00, // float32: f64
            0xCD, 0xCC, 0xCC, 0x3D, // 0.1f
            0x08, 0x08, // end object, end document
        ],
        "a double is written as a float32",
    );
}

#[test]
fn consecutive_documents_deserialize_one_at_a_time() {
    // Upstream asserts the stream position after each of its two `Deserialize` calls: 36 and 75.
    let (first, used) = binary::parse_prefix(CONSECUTIVE).expect("first document");
    assert_eq!(used, 36);
    assert_eq!(first.key, "FirstObject");
    assert_eq!(value(&first, "firstkey"), &Value::String("firstvalue".to_string()));

    let (second, used) = binary::parse_prefix(&CONSECUTIVE[used..]).expect("second document");
    assert_eq!(used, 39);
    assert_eq!(second.key, "SecondObject");
    assert_eq!(value(&second, "secondkey"), &Value::String("secondvalue".to_string()));

    assert_eq!(binary::parse_all(CONSECUTIVE).expect("both documents"), vec![first, second]);
}

#[test]
fn consecutive_documents_serialize_back_to_back() {
    let first = document("FirstObject", vec![kv("firstkey", Value::String("firstvalue".into()))]);
    let second = document("SecondObject", vec![kv("secondkey", Value::String("secondvalue".into()))]);

    let mut written = write(&first);
    written.extend_from_slice(&write(&second));
    assert_bytes(&written, CONSECUTIVE, "consecutive serialization");
}

#[test]
fn document_with_magic_header() {
    let (document, used) = binary::parse_prefix(WITH_MARKER).expect("data with the VBKV marker");
    // The two trailing bytes are not read, exactly as upstream's test asserts.
    assert_eq!(used, WITH_MARKER.len() - 2);
    assert_eq!(document.key, "TestObject");
    assert_eq!(root_object(&document).entries.len(), 7);
    assert_eq!(value(&document, "key"), &Value::String("value".to_string()));
    assert_eq!(value(&document, "int"), &Value::Int32(0x01020304));
    assert_eq!(value(&document, "flt"), &Value::Float32(1234.5678f32));
    // "hi" as Source 2 writes it: an int16 character count and no terminator.
    assert_eq!(value(&document, "wstr"), &Value::WideString("hi".to_string()));
    assert_eq!(value(&document, "col"), &Value::Color([0x40, 0x30, 0x20, 0x10]));
    assert_eq!(value(&document, "lng"), &Value::UInt64(0x1122334455667788));
    assert!(matches!(value(&document, "empty"), Value::Object(object) if object.entries.is_empty()));
}

/// A two-document stream whose *first* document carries the `VBKV` marker and whose second one
/// does not: the marker must be re-detected per document (see
/// `magic_header_is_detected_per_document`). The second document's wide string is 65538 `h`
/// characters with Steam's int32 count 65539, which includes the terminator.
fn magic_header_then_plain() -> Vec<u8> {
    let mut data: Vec<u8> = Vec::new();
    data.extend_from_slice(b"VBKV");
    data.extend_from_slice(&[0, 0, 0, 0]); // CRC32, not verified
    data.extend_from_slice(&[0x00]); // object: one
    data.extend_from_slice(b"one\0");
    data.extend_from_slice(&[0x08, 0x08]); // end object, end document

    data.extend_from_slice(&[0x00]); // object: two
    data.extend_from_slice(b"two\0");
    data.extend_from_slice(&[0x05]); // wide string: w
    data.extend_from_slice(b"w\0");
    data.extend_from_slice(&65539i32.to_le_bytes());
    for unit in "h".repeat(65538).encode_utf16() {
        data.extend_from_slice(&unit.to_le_bytes());
    }
    data.extend_from_slice(&0u16.to_le_bytes()); // the terminator unit the count includes
    data.extend_from_slice(&[0x08, 0x08]); // end object, end document
    data
}

#[test]
fn magic_header_is_detected_per_document() {
    // Upstream constructs a fresh reader for every `Deserialize` call, so `DetectMagicHeader` runs
    // per document: only a document that itself starts with `VBKV` reads a wide string the Source 2
    // way. A document after a marker-carrying one must use Steam's int32 count.
    let data = magic_header_then_plain();
    let wide = "h".repeat(65538);
    // Read with the first document's int16 convention the second document's count would be 3 and
    // the reader would run into the character bytes — "unhandled binary node type 0x68", which is
    // exactly what the reader did before it re-detected per document.

    let documents = binary::parse_all(&data).expect("two consecutive documents");
    assert_eq!(documents.len(), 2);
    assert_eq!(documents[0].key, "one");
    assert_eq!(documents[1].key, "two");
    assert_eq!(value(&documents[1], "w"), &Value::WideString(wide));

    // The mirror image: a marker only in the second document makes only it Source 2 shaped.
    let mut data: Vec<u8> = Vec::new();
    data.extend_from_slice(&[0x00]);
    data.extend_from_slice(b"one\0");
    data.extend_from_slice(&[0x08, 0x08]);
    data.extend_from_slice(b"VBKV");
    data.extend_from_slice(&[0, 0, 0, 0]);
    data.extend_from_slice(&[0x00]); // object: two
    data.extend_from_slice(b"two\0");
    data.extend_from_slice(&[0x05]); // wide string: w, int16 count without a terminator
    data.extend_from_slice(b"w\0");
    data.extend_from_slice(&2i16.to_le_bytes());
    data.extend_from_slice(&[0x68, 0x00, 0x69, 0x00]);
    data.extend_from_slice(&[0x08, 0x08]);

    let documents = binary::parse_all(&data).expect("a marker in the second document");
    assert_eq!(documents.len(), 2);
    assert_eq!(value(&documents[1], "w"), &Value::WideString("hi".to_string()));
}

#[test]
fn document_with_alternate_end_markers() {
    let (document, used) = binary::parse_prefix(ALTERNATE_END).expect("AlternateEnd markers");
    assert_eq!(used, ALTERNATE_END.len());
    assert_eq!(document.key, "TestObject");
    assert_eq!(value(&document, "key"), &Value::String("value".to_string()));
    assert!(matches!(value(&document, "empty"), Value::Object(object) if object.entries.is_empty()));
}

#[test]
fn string_table_deserialization_and_symmetric_serialization() {
    let table = StringTable::read_only(TEST_STRING_TABLE);
    let options = ReadOptions::with_table(&table);

    let document = binary::parse_with(STRING_TABLE, &options).expect("string table data");
    assert_eq!(document.key, "TestObject");
    assert_eq!(root_object(&document).entries.len(), 5);
    assert_eq!(value(&document, "key"), &Value::String("value".to_string()));
    assert_eq!(value(&document, "int"), &Value::Int32(0x01020304));
    assert_eq!(value(&document, "flt"), &Value::Float32(1234.5678f32));
    assert_eq!(value(&document, "lng"), &Value::UInt64(0x1122334455667788));
    assert_eq!(value(&document, "i64"), &Value::Int64(0x0102030405060708));

    // `SymmetricStringTableSerialization`: the same (read-only) table writes the same bytes.
    let mut table = StringTable::read_only(TEST_STRING_TABLE);
    let written = binary::write_with(&document, &mut WriteOptions::with_table(&mut table))
        .expect("every key is in the table");
    assert_bytes(&written, STRING_TABLE, "symmetric string table serialization");
}

#[test]
fn string_table_is_filled_while_serializing() {
    // `StringTableFromScratchTestCase`: an empty table ends up holding the keys in write order.
    let document = document(
        "root",
        vec![
            kv("key", Value::String("value".to_string())),
            kv("child", Value::Object(Object { entries: vec![kv("key", Value::Int32(123))] })),
        ],
    );
    let mut table = StringTable::new();
    let written = binary::write_with(&document, &mut WriteOptions::with_table(&mut table))
        .expect("a writable table accepts new keys");

    assert_eq!(table.to_vec(), vec!["root", "key", "child"]);
    assert_eq!(binary::parse_with(&written, &ReadOptions::with_table(&table)).expect("round trip"), document);
}

#[test]
fn string_table_duplicates_follow_upstream_index_rules() {
    // Upstream's `StringTable(IList<string>)` assigns `reverse[value] = i`, so a repeated key
    // resolves to its *last* index (`get_or_add` of the list constructor would hand that one out).
    let mut table = StringTable::from_values(["a", "b", "a"]);
    assert_eq!(table.to_vec(), vec!["a", "b", "a"], "the list keeps the duplicate");
    assert_eq!(table.index_of("a"), Some(2), "the constructor is last-index-wins");
    assert_eq!(table.get(2), Some("a"));
    assert_eq!(table.get_or_add("a"), Some(2), "an existing key is not appended again");

    // `Add` is first-wins instead (`reverse.TryAdd`): appending a duplicate must not move the
    // index the key resolves to, and the duplicate still lands in the list.
    assert_eq!(table.add("a"), Some(3), "the duplicate is appended");
    assert_eq!(table.index_of("a"), Some(2), "Add is first-index-wins");
    assert_eq!(table.to_vec(), vec!["a", "b", "a", "a"]);
    assert_eq!(table.get(3), Some("a"));

    // A fresh key resolves to the index it was added at, and `get_or_add` appends a missing one.
    assert_eq!(table.add("c"), Some(4));
    assert_eq!(table.index_of("c"), Some(4));
    assert_eq!(table.get_or_add("c"), Some(4), "an existing key");
    assert_eq!(table.get_or_add("d"), Some(5), "a missing key is appended");
    assert_eq!(table.to_vec(), vec!["a", "b", "a", "a", "c", "d"]);

    // A read-only table built from a duplicate list resolves keys the same way.
    let mut table = StringTable::read_only(["x", "y", "x"]);
    assert_eq!(table.index_of("x"), Some(2));
    assert_eq!(table.get_or_add("x"), Some(2));
}

#[test]
fn read_only_string_table_rejects_a_new_key() {
    // Upstream: `StringTable.Add` throws `InvalidOperationException` on a read-only table.
    let document = document("TestObject", vec![kv("unknown", Value::Int32(1))]);
    let mut table = StringTable::read_only(TEST_STRING_TABLE);
    let error = binary::write_with(&document, &mut WriteOptions::with_table(&mut table))
        .expect_err("the read-only table does not have \"unknown\"");
    match error {
        Error::Binary { offset, message } => {
            // The root's tag and its string-table index are written before the child's tag.
            assert_eq!(offset, 1 + 4 + 1, "the error offset is the length of the output so far");
            assert!(message.contains("read-only string table"), "{message}");
            assert!(message.contains("unknown"), "{message}");
        }
        other => panic!("expected Error::Binary, got {other:?}"),
    }
    assert_eq!(table.len(), TEST_STRING_TABLE.len(), "the table did not grow");
}

#[test]
fn string_table_index_out_of_range_is_an_error() {
    let table = StringTable::read_only(TEST_STRING_TABLE);
    // The root key index (5) is fine; 7 is not in the table and not a node type either, so swap
    // the root key index for one past the end of the table.
    let mut data = STRING_TABLE.to_vec();
    data[1..5].copy_from_slice(&7i32.to_le_bytes());
    let error = binary::parse_with(&data, &ReadOptions::with_table(&table)).expect_err("index 7");
    match error {
        Error::Binary { offset, message } => {
            assert_eq!(offset, 1, "the offset of the index");
            assert!(message.contains("out of range"), "{message}");
        }
        other => panic!("expected Error::Binary, got {other:?}"),
    }

    // A negative index is rejected as well.
    data[1..5].copy_from_slice(&(-1i32).to_le_bytes());
    assert!(binary::parse_with(&data, &ReadOptions::with_table(&table)).is_err());
}

#[test]
fn binary_blob_deserializes() {
    let document = binary::parse(BLOB).expect("blob test data");
    assert_eq!(value(&document, "blob"), &Value::Binary(vec![0x00, 0x01, 0xFF]));
    assert_eq!(value(&document, "empty"), &Value::Binary(Vec::new()));
}

#[test]
fn truncated_blob_is_an_error() {
    // `BinaryBlobTestCase.TruncatedBlobThrows`: a length of 0x10 with only two bytes left.
    let mut data = vec![0x00];
    data.extend_from_slice(b"TestObject\0");
    data.extend_from_slice(&[0x09]);
    data.extend_from_slice(b"blob\0");
    let length_offset = data.len();
    data.extend_from_slice(&0x10u32.to_le_bytes());
    data.extend_from_slice(&[0x00, 0x01]);
    match binary::parse(&data) {
        Err(Error::Binary { offset, message }) => {
            assert_eq!(offset, length_offset, "the offset of the blob length");
            assert!(message.contains("does not fit"), "{message}");
        }
        other => panic!("expected Error::Binary, got {other:?}"),
    }
}

#[test]
fn wide_strings_deserialize() {
    // `WideStringTestCase.DeserializesSteamWideString`: int32 counts that include the terminator.
    let mut data = vec![0x00];
    data.extend_from_slice(b"TestObject\0");
    data.extend_from_slice(&[0x05]);
    data.extend_from_slice(b"key\0");
    data.extend_from_slice(&6i32.to_le_bytes());
    for unit in "value\0".encode_utf16() {
        data.extend_from_slice(&unit.to_le_bytes());
    }
    data.extend_from_slice(&[0x05]);
    data.extend_from_slice(b"empty\0");
    data.extend_from_slice(&1i32.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&[0x08, 0x08]);

    let document = binary::parse(&data).expect("Steam wide strings");
    assert_eq!(value(&document, "key"), &Value::WideString("value".to_string()));
    assert_eq!(value(&document, "empty"), &Value::WideString(String::new()));
}

#[test]
fn negative_wide_string_length_is_an_error() {
    // `WideStringTestCase.NegativeLengthThrows`: `0xFFFFFFFF` as an int32.
    let mut data = vec![0x00];
    data.extend_from_slice(b"TestObject\0");
    data.extend_from_slice(&[0x05]);
    data.extend_from_slice(b"key\0");
    data.extend_from_slice(&(-1i32).to_le_bytes());
    match binary::parse(&data) {
        Err(Error::Binary { message, .. }) => {
            assert!(message.contains("invalid wide string length -1"), "{message}");
        }
        other => panic!("expected Error::Binary, got {other:?}"),
    }

    // With the magic header the count is an int16, so `0xFFFF` is the negative case.
    let mut data = vec![0x56, 0x42, 0x4B, 0x56, 0x00, 0x00, 0x00, 0x00, 0x00];
    data.extend_from_slice(b"TestObject\0");
    data.extend_from_slice(&[0x05]);
    data.extend_from_slice(b"key\0");
    data.extend_from_slice(&(-1i16).to_le_bytes());
    assert!(binary::parse(&data).is_err(), "a negative int16 wide string length");
}

#[test]
fn truncated_data_is_an_error() {
    // `TruncatedDataTestCase`, all six cases: upstream expects a KeyValueException wrapping an
    // EndOfStreamException in every one of them.
    let cases: [(&str, &[u8]); 6] = [
        ("empty input", &[]),
        ("object tag only", &[0x00]),
        ("object name without a terminator", &[0x00, b'T', b'e', b's', b't', b'O', b'b', b'j', b'e', b'c', b't']),
        (
            "object with no entries and no end marker",
            &[0x00, 0x54, 0x65, 0x73, 0x74, 0x4F, 0x62, 0x6A, 0x65, 0x63, 0x74, 0x00],
        ),
        ("value name without a terminator", &[0x00, 0x54, 0x65, 0x73, 0x74, 0x4F, 0x62, 0x6A, 0x65, 0x63, 0x74, 0x00, 0x02, 0x6B, 0x65, 0x79]),
        ("value without its payload", &[0x00, 0x54, 0x65, 0x73, 0x74, 0x4F, 0x62, 0x6A, 0x65, 0x63, 0x74, 0x00, 0x02, 0x6B, 0x65, 0x79, 0x00, 0x01]),
    ];
    for (name, data) in cases {
        match binary::parse(data) {
            Err(Error::Binary { .. }) => {}
            other => panic!("{name}: expected Error::Binary, got {other:?}"),
        }
    }
}

#[test]
fn out_of_range_node_type_is_an_error() {
    // `OutOfRangeNodeTypeTestCase`: tag 0xA0 inside the root object.
    let mut data = vec![0x00];
    data.extend_from_slice(b"TestObject\0");
    let tag_offset = data.len();
    data.extend_from_slice(&[0xA0]);
    data.extend_from_slice(b"key\0");
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&[0x08, 0x08]);
    match binary::parse(&data) {
        Err(Error::Binary { offset, message }) => {
            // The tag is rejected before the key is read, so the offset is the tag itself
            // (upstream consumes the key first; see the module docs).
            assert_eq!(offset, tag_offset);
            assert!(message.contains("unhandled binary node type 0xA0"), "{message}");
        }
        other => panic!("expected Error::Binary, got {other:?}"),
    }
}

#[test]
fn every_prefix_of_a_valid_file_is_handled() {
    // Truncating a valid document anywhere must be an error, never a panic. `first_document` is
    // the length of the first document in the file: every shorter prefix is truncated.
    let cases: [(&str, &[u8], usize); 7] = [
        ("simple", SIMPLE, SIMPLE.len()),
        ("deserialize", DESERIALIZE, DESERIALIZE.len()),
        ("serialized", SERIALIZED, SERIALIZED.len()),
        ("marker", WITH_MARKER, WITH_MARKER.len() - 2),
        ("alternate end", ALTERNATE_END, ALTERNATE_END.len()),
        ("blob", BLOB, BLOB.len()),
        ("consecutive", CONSECUTIVE, 36),
    ];
    for (name, bytes, first_document) in cases {
        assert!(binary::parse(bytes).is_ok(), "{name} is a valid file");
        assert!(binary::parse(&bytes[..first_document]).is_ok(), "{name}: its first document");
        for length in 0..bytes.len() {
            let _ = binary::parse(&bytes[..length]);
        }
        for length in 0..first_document {
            assert!(binary::parse(&bytes[..length]).is_err(), "{name}: a {length}-byte prefix");
        }
    }

    // The string-table file needs its table; every prefix of it must be handled as well.
    let table = StringTable::read_only(TEST_STRING_TABLE);
    let options = ReadOptions::with_table(&table);
    for length in 0..STRING_TABLE.len() {
        let _ = binary::parse_with(&STRING_TABLE[..length], &options);
    }

    // Byte soup: every prefix of every byte value and of a pseudo-random block.
    let mut soup: Vec<u8> = (0u8..=255).collect();
    let mut state = 0x1234_5678u32;
    for _ in 0..512 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        soup.push((state >> 16) as u8);
    }
    for length in 0..soup.len() {
        let _ = binary::parse(&soup[..length]);
        let _ = binary::parse_with(&soup[..length], &options);
        let _ = binary::parse_all(&soup[..length]);
    }

    // The two-document stream with the per-document marker is valid as well: every prefix of it,
    // the 65539-unit wide string included, has to be handled without a panic.
    let two_documents = magic_header_then_plain();
    assert!(binary::parse_all(&two_documents).is_ok(), "the stream itself is valid");
    for length in 0..two_documents.len() {
        let _ = binary::parse_all(&two_documents[..length]);
    }
}

#[test]
fn hostile_lengths_are_rejected_without_allocating() {
    // A blob length is checked against the remaining input before anything is reserved.
    for length in [0xFFFF_FFFFu32, 0x7FFF_FFFF, 0x1000_0000, 0x20] {
        let mut data = vec![0x00];
        data.extend_from_slice(b"TestObject\0");
        data.extend_from_slice(&[0x09]);
        data.extend_from_slice(b"blob\0");
        data.extend_from_slice(&length.to_le_bytes());
        data.extend_from_slice(&[0x00, 0x01, 0x02]);
        match binary::parse(&data) {
            Err(Error::Binary { message, .. }) => assert!(message.contains("does not fit"), "{message}"),
            other => panic!("blob length {length:#X}: expected Error::Binary, got {other:?}"),
        }
    }

    // A wide string length is capped the same way, and a negative one is rejected outright.
    for length in [0x7FFF_FFFFi32, 0x0010_0000, 0x3] {
        let mut data = vec![0x00];
        data.extend_from_slice(b"TestObject\0");
        data.extend_from_slice(&[0x05]);
        data.extend_from_slice(b"wide\0");
        data.extend_from_slice(&length.to_le_bytes());
        data.extend_from_slice(&[0x68, 0x00]);
        match binary::parse(&data) {
            Err(Error::Binary { message, .. }) => {
                assert!(message.contains("unexpected end of input"), "{message}");
            }
            other => panic!("wide string length {length:#X}: expected Error::Binary, got {other:?}"),
        }
    }
}

#[test]
fn nesting_is_capped_at_max_binary_depth() {
    // A root object holding `depth` nested objects, each one inside the previous one.
    fn nested(depth: usize) -> Vec<u8> {
        let mut data = vec![0x00];
        data.extend_from_slice(b"root\0");
        for _ in 0..depth {
            data.extend_from_slice(&[0x00]);
            data.extend_from_slice(b"child\0");
        }
        // One marker per object (`depth` of them plus the root) and one for the document.
        for _ in 0..=depth + 1 {
            data.extend_from_slice(&[0x08]);
        }
        data
    }

    // The root object counts as one level, so `MAX_BINARY_DEPTH - 1` objects may sit inside it.
    let deepest = nested(MAX_BINARY_DEPTH - 1);
    let document = binary::parse(&deepest).expect("at the cap");
    assert_bytes(&write(&document), &deepest, "the deepest accepted document round trips");

    let too_deep = nested(MAX_BINARY_DEPTH);
    match binary::parse(&too_deep) {
        Err(Error::Binary { message, .. }) => {
            assert!(message.contains("nested deeper than"), "{message}");
        }
        other => panic!("expected Error::Binary, got {other:?}"),
    }
}

#[test]
fn wide_strings_and_colors_and_blobs_round_trip() {
    // A wide string is written the way a document without the magic header is read back: an int32
    // character count that includes the terminator, then UTF-16LE.
    let wide = document("t", vec![kv("w", Value::WideString("hi".to_string()))]);
    assert_bytes(
        &write(&wide),
        &[
            0x00, b't', 0x00, // object: t
            0x05, b'w', 0x00, // wide string: w
            0x03, 0x00, 0x00, 0x00, // 3 UTF-16 units, terminator included
            0x68, 0x00, 0x69, 0x00, 0x00, 0x00, // "hi\0"
            0x08, 0x08, // end object, end document
        ],
        "wide string layout",
    );

    let document = document(
        "type5",
        vec![
            kv("wide", Value::WideString("hi".to_string())),
            kv("empty_wide", Value::WideString(String::new())),
            kv("unicode", Value::WideString("邪恶的战".to_string())),
            kv("color", Value::Color([0x01, 0x02, 0x03, 0x04])),
            kv("blob", Value::Binary(vec![0x00, 0x01, 0xFF])),
        ],
    );
    let written = write(&document);
    // A colour is written as its four bytes in stream order, a blob as an int32 length + bytes.
    assert!(
        written.windows(4).any(|window| window == [0x01, 0x02, 0x03, 0x04]),
        "the colour bytes are in the output in stream order"
    );
    assert_eq!(binary::parse(&written).expect("round trip"), document);
}

#[test]
fn trailing_bytes_are_ignored_and_a_root_is_required() {
    // Upstream stops reading at the document's end marker.
    let mut with_trailing = SIMPLE.to_vec();
    with_trailing.extend_from_slice(&[0xFF, 0xFF, 0x00, 0x00]);
    assert_eq!(binary::parse(&with_trailing).expect("trailing bytes"), binary::parse(SIMPLE).unwrap());

    // A document that is nothing but an end marker has no root object (upstream fails in its
    // object builder for it).
    assert!(binary::parse(&[0x08]).is_err());
    assert!(binary::parse(&[0x0B]).is_err());
    // The byte that closes a document has to be a marker: another root object where the marker
    // belongs is an error (upstream: "Found data after the root object").
    let mut second_root = SIMPLE[..SIMPLE.len() - 1].to_vec();
    second_root.push(0x00);
    match binary::parse(&second_root) {
        Err(Error::Binary { message, .. }) => {
            assert!(message.contains("found data after the root object"), "{message}");
        }
        other => panic!("expected Error::Binary, got {other:?}"),
    }
    // ... and the same for a root that is not an object.
    let mut scalar_root = vec![0x01, b'k', 0x00, b'v', 0x00];
    scalar_root.extend_from_slice(&[0x01, b'k', 0x00, b'v', 0x00]);
    assert!(binary::parse(&scalar_root).is_err());
    // An empty input holds no document at all, but no consecutive documents either.
    assert!(binary::parse(&[]).is_err());
    assert_eq!(binary::parse_all(&[]).expect("no documents"), Vec::<Document>::new());
    // A truncated second document is an error.
    let mut truncated = CONSECUTIVE.to_vec();
    truncated.truncate(CONSECUTIVE.len() - 3);
    assert!(binary::parse_all(&truncated).is_err());
}

#[test]
fn a_non_object_root_round_trips() {
    // Upstream's writer emits no document marker for a primitive root (and its reader then fails
    // on the missing marker); this port writes the marker (see the module docs).
    let document = kv("key", Value::String("value".to_string()));
    let written = write(&document);
    assert_bytes(&written, &[0x01, 0x6B, 0x65, 0x79, 0x00, 0x76, 0x61, 0x6C, 0x75, 0x65, 0x00, 0x08], "primitive root");
    assert_eq!(binary::parse(&written).expect("round trip"), document);
}

#[test]
fn duplicate_keys_keep_their_order() {
    let document = document(
        "root",
        vec![
            kv("path", Value::String("a".to_string())),
            kv("path", Value::String("b".to_string())),
            kv("path", Value::String("a".to_string())),
        ],
    );
    let reparsed = binary::parse(&write(&document)).expect("round trip");
    assert_eq!(reparsed, document);
    assert_eq!(root_object(&reparsed).entries.len(), 3);
    assert_eq!(value(&reparsed, "path"), &Value::String("a".to_string()));
    let all: Vec<&Value> = root_object(&reparsed).get_all("path").collect();
    assert_eq!(all, vec![
        &Value::String("a".to_string()),
        &Value::String("b".to_string()),
        &Value::String("a".to_string()),
    ]);
}

