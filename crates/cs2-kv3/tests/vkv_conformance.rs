//! `cs2-kv3` conformance against ValveKeyValue's KV3 text test suite.
//!
//! Source: ValveKeyValue (MIT) **commit 28ca0e5**, `ValveKeyValue/ValveKeyValue.Test/TextKV3/*.cs`
//! and its `Test Data/TextKV3/*` fixtures (copied to `fixtures/vkv/`, see that README). Each test
//! carries the upstream name in snake_case; `ParseKV3Text`-style inputs use upstream's own
//! generic header via [`parse_kv3_text`], and the header tests assert through
//! [`parse_strict`] ([`Header::validate_strict`]) because plain `parse` is the documented
//! 686975d-∪-upstream union. Where the two platforms decode a value differently — the string
//! escape table, and a `Number::from_f64` of a non-finite — cs2-kv3 keeps the pre-conformance
//! behaviour (commit 686975d) per WP5 round 2, and the test comments say so.
//!
//! # Not ported, and why
//!
//! | upstream test(s) | why |
//! |---|---|
//! | `RootTypesTestCase` (11 root-value tests, `DeserializesRootNull/String/MultilineString/FlaggedString/FlaggedObject/BinaryBlob/Number/NumberNegative/Float/Array/RootUppercaseLiteral/RootArrayOfMixedValues`), `SerializesRootValues`, `SerializesFlaggedObject`, `SerializesFloatInfAndNanFromFloat` | the root value may be any value upstream; `Document.root` is an `Object` (frozen model — hammer-mcp depends on it). Unlocking needs `root: Value`; the change request is recorded in the crate docs. `DeserializesRootWithSurroundingComments` keeps its object root and IS ported below. |
//! | `SkipHeaderTestCase` (7 tests) | the `KVSerializerOptions.SkipHeader` API has no Rust equivalent (`parse` always reads the header). |
//! | `DeserializesBinaryBlobToTypedByteArray`, `DeserializesArrayToTypedIntList`, `DeserializesArrayToTypedStringArray`, `DeserializesArrayToTypedIntArray` | object mapping onto C# types. |
//! | `SerializesPointerType`, `SerializesInt16AndUInt16` | C# `IntPtr`/`short`/`ushort` value types (cs2-kv3 holds i64/f64 only). |
//! | `Kv1ToKv3TestCase`, `Kv3ToKv1TestCase` (4 tests) | KV1<=>KV3 conversion (cs2-kv1's field, not this crate). |
//!
//! # Writer differences (upstream `KV3TextSerializer` vs `cs2_kv3::write`)
//!
//! `cs2_kv3::write` is pinned byte-for-byte to Valve's own tools (93/95 shipped files), which is
//! NOT the layout of upstream's serializer. Where upstream's suite asserts its writer's bytes,
//! this file asserts the semantic round trip (`parse → write → parse` gives the same tree), and
//! every such case is listed here:
//!
//! | case | upstream writes | cs2-kv3 writes | because Valve's tools write |
//! |---|---|---|---|
//! | floats | re-formatted to 6 decimals with trailing zeros trimmed (`1.500000`→`1.5`, `1e20`→`100000000000000000000.0`, `0.0000001`→`0.0` — precision lost) | the source lexeme (`74.56868`, `0.0000001`, `1e20` stay) | the original spelling — byte-identical on the shipped files |
//! | `+inf` / `NaN` | `inf` / `nan` (canonical) | the source spelling (`+inf` stays `+inf`) | (same shipping evidence) |
//! | a string containing `\n` | `"""…"""` | `"…"` with `\n` escapes | 28 shipped files embed `\n` escapes; Valve never emits `""";` upstream does (`multiline_serialized.kv3`) |
//! | flags | the canonical lowercase name (`soundEvent:`→`soundevent:`) | the source spelling (`RESOURCE:` stays) | the shipped files spell them canonically |
//! | chained flags (`a:b\|c:`) | only the last flag, canonical | every flag, nested, in its source spelling (686975d) | (same) |
//! | arrays of ≤4 "simple" elements | inline `[ 42 ]`, `[ "a", "b" ]` | only 2–3 numbers inline; 4+ numbers packed 4-per-line; other arrays one element per line | `[ 0.0, 0.0, 0.0 ]`, the 4-number packing and one-per-line strings of the shipped files |
//! | an empty object | a multi-line `{\n}` block | `{  }` (two spaces, symmetric with `[  ]`) | `{  }` in the shipped files (`empty_object_serialized.kv3`) |
//! | a key with a leading `.` (`.y`) | bare `.y` | `".y"` (the reader accepts it bare) | 686975d's key predicate `[A-Za-z_][A-Za-z0-9_.]*`; its reader cannot read `.y` back |
//! | a CR in a string | a raw CR (its `\r` *escape* decodes to the letter `r`); upstream's writer emits the CR raw | `\r`, which the reader decodes to the carriage return — a write→parse round trip is exact (round 2 item 3) | unverified either way; no shipped file contains a CR value |
//! | `inf` / `-inf` / `nan` built from Rust `f64` | `inf` / `-inf` / `nan` | `0.0` (`Number::from_f64`'s fallback, restored by round 2 item 6; the reader still accepts the literals) | unverified; Valve's tools write what they lexed |
//! | the header | re-written as generic `<!-- kv3 encoding:text:version{…} format:generic:version{…} -->` | the header span as read (`Header::raw` keeps its format id and GUID) | Valve's headers keep theirs |

use std::fs;
use std::path::Path;

use cs2_kv3::{parse, write, Document, Header, Number, Object, ParseError, Value};

/// Upstream's header acceptance: a successful header scan plus the strict GUID checks. The
/// plain [`parse`] is the 686975d-∪-upstream union and accepts some inputs upstream refuses
/// (`version{x}`, wrong GUIDs, swapped field order — see `compat_686975d.rs`), so the header
/// conformance tests assert through `Header::validate_strict` (WP5 round 2 item 5).
fn parse_strict(text: &str) -> Result<Document, ParseError> {
    let doc = parse(text)?;
    doc.header.validate_strict()?;
    Ok(doc)
}

/// Upstream's `TestDataHelper.KV3Header`, verbatim.
const UPSTREAM_HEADER: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} \
         format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n";

/// Upstream's `TestDataHelper.ParseKV3Text(body)`.
fn parse_kv3_text(body: &str) -> Object {
    parse(&format!("{UPSTREAM_HEADER}{body}"))
        .unwrap_or_else(|error| panic!("`{body}` should parse: {error}"))
        .root
}

/// Upstream's must-throw cases: the text parses with upstream's header around it.
fn assert_kv3_text_rejected(body: &str, why: &str) {
    assert!(
        parse(&format!("{UPSTREAM_HEADER}{body}")).is_err(),
        "{why}: `{body}` must not parse",
    );
}

fn fixture(name: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vkv").join(name))
        .unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn value<'a>(root: &'a Object, key: &str) -> &'a Value {
    root.get(key).unwrap_or_else(|| panic!("no key {key}"))
}

/// Upstream compares C# values: numbers by value (±inf equal, NaN equal to itself), flags by
/// their canonical enum (hence case-insensitive here — cs2-kv3 keeps the source spelling), keys
/// and strings exactly. This is the comparator for the serialized-fixture comparisons.
fn semantic_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(fx), Some(fy)) => fx == fy || (fx.is_nan() && fy.is_nan()),
            // A lexeme neither accessor can read is equal to itself only (a bare `+`-value).
            (None, None) => x.text == y.text,
            _ => false,
        },
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Flagged { flag: x, value: vx }, Value::Flagged { flag: y, value: vy }) => {
            x.eq_ignore_ascii_case(y) && semantic_eq(vx, vy)
        }
        (Value::Blob(x), Value::Blob(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| semantic_eq(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x
                    .iter()
                    .zip(y.iter())
                    .all(|((kx, vx), (ky, vy))| kx == ky && semantic_eq(vx, vy))
        }
        _ => false,
    }
}

fn roots_equal(input: &Object, expected: &Object, what: &str) {
    assert!(
        semantic_eq(&Value::Object(input.clone()), &Value::Object(expected.clone())),
        "{what}: {input:?} != {expected:?}",
    );
}

fn object_of(text: &str, what: &str) -> Object {
    parse(text).unwrap_or_else(|error| panic!("{what}: {error}")).root
}

/// parse → write → parse gives back the same tree, and re-writing is stable.
fn assert_round_trip_semantically(text: &str, what: &str) {
    let first = parse(text).unwrap_or_else(|error| panic!("{what}: {error}"));
    let written = write(&first);
    let second = parse(&written).unwrap_or_else(|error| panic!("{what}: rewritten: {error}"));
    roots_equal(&first.root, &second.root, what);
    assert_eq!(second.header, first.header, "{what}: the rewrite changed the header");
    assert_eq!(write(&second), written, "{what}: write is not stable");
}

/// The lexeme of a number value, for the spelling-preservation assertions.
fn lexeme(value: &Value) -> String {
    match value {
        Value::Number(number) => number.text.clone(),
        other => panic!("expected a number, got {other:?}"),
    }
}

/// A document with `pairs` (`(key, value)`), in cs2-kv3's canonical layout, for the writer tests.
fn document_with(pairs: Vec<(&str, Value)>) -> Document {
    let mut doc =
        Document { header: Header::parse(UPSTREAM_HEADER.trim_end()).expect("header"), root: Object::new() };
    for (key, value) in pairs {
        doc.root.insert(key.to_string(), value);
    }
    doc
}

// ============================================================================================
// BasicKV3TestCases.cs
// ============================================================================================

#[test]
fn deserializes_header_and_value() {
    let text = include_str!("../fixtures/vkv/basic.kv3");
    let root = parse(text).expect("parses").root;
    assert_eq!(value(&root, "foo").as_str(), Some("bar"));
}

#[test]
fn quoted_flag_prefix_throws() {
    // A quoted text before `:` is a string value, not a flag — the `:` is left over.
    assert_kv3_text_rejected("{\n\tfoo = \"resource\":\"path/to/file.vmdl\"\n}\n", "quoted flag prefix");
}

#[test]
fn deserializes_flagged_values() {
    let text = include_str!("../fixtures/vkv/flagged_value.kv3");
    let root = parse(text).expect("parses").root;
    // `(the flag name with its source spelling, the flagged value)` — coerced out of the Box so
    // the accessors below read like the rest.
    fn as_flagged(v: &Value) -> (&String, &Value) {
        match v {
            Value::Flagged { flag, value } => (flag, value),
            other => panic!("expected a flagged value, got {other:?}"),
        }
    }
    let (flag, inner) = as_flagged(value(&root, "foo"));
    assert_eq!(flag, "resource");
    assert_eq!(inner.as_str(), Some("bar"));
    let (flag, inner) = as_flagged(value(&root, "bar"));
    assert_eq!(flag, "resource");
    assert_eq!(inner.as_str(), Some("foo"));
    // Chained flags (`resource:resource_name|subclass:`): upstream keeps only the last one;
    // cs2-kv3 nests the chain like 686975d did, so a re-write keeps every flag (the innermost
    // flag is upstream's answer).
    let (flag, inner) = as_flagged(value(&root, "multipleFlags"));
    assert_eq!(flag, "resource", "a chain nests, outermost first");
    let (flag, inner) = as_flagged(inner);
    assert_eq!(flag, "resource_name");
    let (flag, inner) = as_flagged(inner);
    assert_eq!(flag, "subclass", "upstream's last-flag-wins value is the innermost flag");
    assert_eq!(inner.as_str(), Some("cool value"));
    let (flag, inner) = as_flagged(value(&root, "flaggedNumber"));
    assert_eq!(flag, "panorama");
    assert_eq!(inner.as_i64(), Some(-1234));
    let (flag, inner) = as_flagged(value(&root, "soundEvent"));
    assert_eq!(flag, "soundEvent");
    assert_eq!(inner.as_str(), Some("event sound"));
    assert_eq!(value(&root, "noFlags"), &Value::Number(Number::parse("5").unwrap()));
    let (flag, inner) = as_flagged(value(&root, "flaggedObject"));
    assert_eq!(flag, "panorama");
    let object = inner.as_object().expect("flaggedObject is an object");
    let (flag, inner) = as_flagged(value(object, "1"));
    assert_eq!(flag, "soundEvent");
    assert_eq!(inner.as_str(), Some("test1"));
    // A quoted member without a flag is a plain string (no flag, no wrapper).
    assert_eq!(value(object, "2"), &Value::String("test2".into()));
    let (flag, inner) = as_flagged(value(object, "3"));
    assert_eq!(flag, "subclass");
    assert_eq!(inner.as_array().map(|a| a.to_vec()), Some(vec![Value::String("test3".into())]));
    let (flag, inner) = as_flagged(value(object, "4"));
    assert_eq!(flag, "resource_name");
    assert_eq!(inner.as_str(), Some("test4"));
}

#[test]
fn deserializes_multiline_strings() {
    let text = include_str!("../fixtures/vkv/multiline.kv3");
    let root = parse(text).expect("parses").root;
    assert_eq!(
        value(&root, "multiLineStringValue").as_str(),
        Some("First line of a multi-line string literal.\nSecond line of a multi-line string literal."),
    );
    assert_eq!(
        value(&root, "multiLineWithQuotesInside").as_str(),
        Some("hmm this \"\"\"is awkward\n\"\"\" yes"),
        "a mid-line `\"\"\"` does not close the string; `\"\"\"` is un-escaped by a preceding backslash",
    );
    // A real newline inside a single `"…"` string is allowed and kept.
    assert_eq!(value(&root, "singleQuotesButWithNewLineAnyway").as_str(), Some("hello\nvalve"));
}

#[test]
fn deserializes_multiline_strings_crlf() {
    let text = include_str!("../fixtures/vkv/multiline_crlf.kv3");
    let root = parse(text).expect("parses").root;
    assert_eq!(
        value(&root, "multiLineStringValue").as_str(),
        Some("First line of a multi-line string literal.\nSecond line of a multi-line string literal."),
    );
}

#[test]
fn deserializes_comments() {
    let text = include_str!("../fixtures/vkv/comments.kv3");
    let root = parse(text).expect("parses").root;
    assert_eq!(value(&root, "foo").as_str(), Some("bar"));
    assert_eq!(value(&root, "one").as_str(), Some("1"));
    assert_eq!(value(&root, "two").as_str(), Some("2"));
}

#[test]
fn deserializes_array() {
    let text = include_str!("../fixtures/vkv/array.kv3");
    let root = parse(text).expect("parses").root;
    assert_eq!(
        value(&root, "arrayValue").as_array(),
        Some([Value::String("a".into()), Value::String("b".into())].as_slice()),
    );
    // No space around `=`, single line, many floats.
    let single = value(&root, "arrayOnSingleLine").as_array().expect("array");
    assert_eq!(single.len(), 3);
    assert_eq!(lexeme(&single[0]), "16.7551");
    let no_space = value(&root, "arrayNoSpace").as_array().expect("array");
    assert_eq!(lexeme(&no_space[0]), "1.3763");
    let mixed = value(&root, "arrayMixedTypes").as_array().expect("array");
    assert_eq!(mixed.len(), 11);
    assert!(mixed[5].as_object().is_some(), "an object element");
    assert_eq!(mixed[6].as_array().map(|a| a.len()), Some(4), "a packed number array");
    assert_eq!(mixed[7], Value::Blob(vec![0x11, 0xFF]), "a blob element");
    assert!(matches!(
        &mixed[8],
        Value::Flagged { flag, value } if flag == "resource" && value.as_str() == Some("hello.world"),
    ));
    assert_eq!(mixed[9].as_str(), Some("multiline\nstring"), "a `\"\"\"` element");
    assert_eq!(mixed[10].as_f64(), Some(-69.420));
}

#[test]
fn deserializes_binary_blob() {
    let text = include_str!("../fixtures/vkv/binary_blob.kv3");
    let root = parse(text).expect("parses").root;
    let expected: Vec<u8> = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xFF].into();
    assert_eq!(value(&root, "array"), &Value::Blob(expected));
}

#[test]
fn deserializes_nested_object() {
    let text = include_str!("../fixtures/vkv/object.kv3");
    let doc = parse(text).expect("parses");
    let a = doc.root.get("a").and_then(Value::as_object).expect("a");
    let b = a.get("b").and_then(Value::as_object).expect("b");
    assert_eq!(b.get("c").and_then(Value::as_str), Some("d"));
}

#[test]
fn deserializes_entity_name_flag() {
    let text = include_str!("../fixtures/vkv/entity_name.kv3");
    let root = parse(text).expect("parses").root;
    assert!(matches!(
        value(&root, "name"),
        Value::Flagged { flag, value } if flag == "entity_name" && value.as_str() == Some("some_entity"),
    ));
}

#[test]
fn deserializes_localize_flag() {
    let text = include_str!("../fixtures/vkv/localize.kv3");
    let root = parse(text).expect("parses").root;
    assert!(matches!(
        value(&root, "title"),
        Value::Flagged { flag, value } if flag == "localize" && value.as_str() == Some("#SFUI_Title"),
    ));
}

#[test]
fn deserializes_escape_sequences() {
    let text = include_str!("../fixtures/vkv/escape_sequences.kv3");
    let root = parse(text).expect("parses").root;
    assert_eq!(value(&root, "newline").as_str(), Some("hello\nworld"));
    assert_eq!(value(&root, "tab").as_str(), Some("hello\tworld"));
    assert_eq!(value(&root, "backslash").as_str(), Some("hello\\world"));
    assert_eq!(value(&root, "quote").as_str(), Some("hello\"world"));
    assert_eq!(value(&root, "combined").as_str(), Some("line1\nline2\ttab\\slash\"quote"));
}

#[test]
fn deserializes_basic_types() {
    let text = include_str!("../fixtures/vkv/types.kv3");
    let root = parse(text).expect("parses").root;
    assert_eq!(value(&root, "boolFalseValue").as_bool(), Some(false));
    assert_eq!(value(&root, "boolTrueValue").as_bool(), Some(true));
    assert_eq!(value(&root, "nullValue"), &Value::Null);
    assert_eq!(value(&root, "intValue").as_i64(), Some(128));
    assert_eq!(lexeme(value(&root, "intValue")), "128");
    assert_eq!(value(&root, "doubleValue").as_f64(), Some(64.123));
    assert_eq!(value(&root, "negativeIntValue").as_i64(), Some(-1337));
    assert_eq!(value(&root, "negativeDoubleValue").as_f64(), Some(-0.1337));
    assert_eq!(value(&root, "plusIntValue").as_i64(), Some(1337));
    assert_eq!(lexeme(value(&root, "plusIntValue")), "+1337", "the `+` spelling is kept");
    assert_eq!(value(&root, "plusDoubleValue").as_f64(), Some(0.1337));
    assert_eq!(value(&root, "stringValue").as_str(), Some("hello world"));
    assert_eq!(value(&root, "negativeMaxInt").as_i64(), Some(-9223372036854775807));
    // Upstream reads `positiveMaxInt` as the UInt64 maximum; this model has no UInt64 variant,
    // so the lexeme is kept and only the f64 accessor can read it.
    assert_eq!(lexeme(value(&root, "positiveMaxInt")), "18446744073709551615");
    assert_eq!(value(&root, "positiveMaxInt").as_i64(), None);
    assert_eq!(value(&root, "positiveMaxInt").as_f64(), Some(18446744073709551615.0));
    assert_eq!(value(&root, "doubleMaxValue").as_f64(), Some(62147483647.1337));
    assert_eq!(value(&root, "doubleNegativeMaxValue").as_f64(), Some(-62147483647.1337));
    assert_eq!(value(&root, "doubleExponent").as_f64(), Some(123.456));
    assert_eq!(lexeme(value(&root, "doubleExponent")), "1.23456E+2");
    assert_eq!(value(&root, "intWithStringSuffix").as_str(), Some("123foobar"));
    assert_eq!(value(&root, "singleQuotes").as_str(), Some("string"));
    assert_eq!(value(&root, "singleQuotesWithQuotesInside").as_str(), Some("string is \"pretty\" cool"));
    assert_eq!(value(&root, "key_with._various.separators").as_str(), Some("test"));
    assert_eq!(value(&root, "quoted key with : {} terminators").as_str(), Some("test quoted key"));
    // A `"""` key is a key like any other.
    assert_eq!(value(&root, "this is a multi\nline\nkey").as_str(), Some("multi line key parsed"));
    assert_eq!(value(&root, "empty.string").as_str(), Some(""));
}

#[test]
fn odd_length_hex_blob_throws() {
    // `#[ AB C ]` — a one-digit byte.
    assert_kv3_text_rejected("#[ AB C ]", "odd-length blob");
}

#[test]
fn invalid_hex_blob_throws() {
    // `#[ AB ZZ ]` — not a hex digit.
    assert_kv3_text_rejected("#[ AB ZZ ]", "non-hex blob");
}

#[test]
fn unterminated_block_comment_throws() {
    assert_kv3_text_rejected("{ /* unterminated comment\n}", "unterminated comment");
}

#[test]
fn quoted_literals_are_strings() {
    let root = parse_kv3_text("{ a = \"42\" b = \"true\" c = \"null\" d = '1.5' e = \"nan\" }");
    for (key, expected) in [("a", "42"), ("b", "true"), ("c", "null"), ("d", "1.5"), ("e", "nan")] {
        assert_eq!(value(&root, key).as_str(), Some(expected), "quoted `{expected}` stays a string");
    }
}

#[test]
fn literals_are_case_insensitive() {
    let root = parse_kv3_text("{ a = True b = FALSE c = Null d = NaN e = INF f = +inf g = -INF }");
    assert_eq!(value(&root, "a").as_bool(), Some(true));
    assert_eq!(value(&root, "b").as_bool(), Some(false));
    assert_eq!(value(&root, "c"), &Value::Null);
    assert!(value(&root, "d").as_f64().is_some_and(|f| f.is_nan()));
    assert_eq!(value(&root, "e").as_f64(), Some(f64::INFINITY));
    assert_eq!(value(&root, "f").as_f64(), Some(f64::INFINITY));
    assert_eq!(value(&root, "g").as_f64(), Some(f64::NEG_INFINITY));
    // The spellings survive for a byte-faithful write (upstream canonicalises them).
    assert_eq!(lexeme(value(&root, "d")), "NaN");
    assert_eq!(lexeme(value(&root, "f")), "+inf");
}

#[test]
fn deserializes_integer_forms() {
    let cases = [
        ("007", 7),
        ("-0", 0),
        ("+5", 5),
        // A lone plus is the integer zero (upstream `ParseValue`).
        ("+", 0),
        ("-9223372036854775808", i64::MIN),
    ];
    for (text, expected) in cases {
        let root = parse_kv3_text(&format!("{{ a = {text} }}"));
        assert_eq!(value(&root, "a").as_i64(), Some(expected), "`{text}`");
        assert!(!matches!(value(&root, "a"), Value::String(_)), "{text} is a number, not a string");
    }
    // The bare `+` value is written as the 0 it means (upstream's value, normalised here).
    let doc = Document {
        header: Header::parse(UPSTREAM_HEADER.trim_end()).expect("header"),
        root: parse_kv3_text("{ a = + }"),
    };
    assert!(write(&doc).contains("\ta = 0\r\n"), "{}", write(&doc));
}

#[test]
fn deserializes_float_forms() {
    let cases = [
        (".5", 0.5),
        ("-.5", -0.5),
        ("5.", 5.0),
        ("+1.5", 1.5),
        ("1e5", 100_000.0),
        ("1E5", 100_000.0),
        ("1e+5", 100_000.0),
        ("-1.5e-5", -0.000_015),
        ("1E-2", 0.01),
        // Overflow reads as infinity and underflow as zero, like upstream's `double.TryParse`.
        ("1.5e400", f64::INFINITY),
        ("1e-400", 0.0),
        ("-0.0", -0.0),
    ];
    for (text, expected) in cases {
        let root = parse_kv3_text(&format!("{{ a = {text} }}"));
        let parsed = value(&root, "a");
        assert_eq!(parsed.as_f64(), Some(expected), "`{text}`");
        assert_eq!(lexeme(parsed), text, "{text} keeps its lexeme");
        // -0.0 stays negative-zero, like upstream's `double.IsNegative` check.
        assert_eq!(
            parsed.as_f64().unwrap().is_sign_negative(),
            expected.is_sign_negative(),
            "{text}",
        );
    }
}

#[test]
fn deserializes_flag_syntax() {
    let cases = [
        "resource:\"x\"",
        "resource|\"x\"",
        "resource : \"x\"",
        "resource\t|\t\"x\"",
        "resource\n|\n\"x\"",
        "resource:\n\"x\"",
        "resource: /* c */ \"x\"",
        "RESOURCE:\"x\"",
        "Resource | \"x\"",
        "resource:\"\"\"\nx\n\"\"\"",
    ];
    for text in cases {
        let root = parse_kv3_text(&format!("{{ a = {text} }}"));
        assert!(
            matches!(
                value(&root, "a"),
                Value::Flagged { flag, value } if flag.eq_ignore_ascii_case("resource") && value.as_str() == Some("x"),
            ),
            "`{text}`",
        );
    }
}

#[test]
fn a_quoted_flag_prefix_is_refused() {
    assert_kv3_text_rejected("{ a = \"resource\":\"x\" }", "quoted flag prefix");
}

#[test]
fn deserializes_nested_flags() {
    let root = parse_kv3_text(
        "{ a = subclass: { b = [ panorama : \"p\", entity_name:\"e\", \"plain\" ] } \
         c = [ subclass:{ d = 2 }, { e = 3 } ] }",
    );
    let (a_flag, a_value) = match value(&root, "a") {
        Value::Flagged { flag, value } => (flag.as_str(), value),
        other => panic!("a is {other:?}"),
    };
    assert_eq!(a_flag, "subclass");
    let b = a_value.as_object().and_then(|o| o.get("b")).and_then(Value::as_array).expect("array b");
    assert!(matches!(&b[0], Value::Flagged { flag, value } if flag == "panorama" && value.as_str() == Some("p")));
    assert!(matches!(&b[1], Value::Flagged { flag, value } if flag == "entity_name" && value.as_str() == Some("e")));
    assert!(!matches!(&b[2], Value::Flagged { .. }), "a plain element carries no flag");
    let c = value(&root, "c").as_array().expect("array c");
    assert!(matches!(&c[0], Value::Flagged { flag, .. } if flag == "subclass"));
    assert!(!matches!(&c[1], Value::Flagged { .. }));
}

#[test]
fn deserializes_blob_syntax() {
    let cases: [(&str, &[u8]); 8] = [
        ("#[ 01 02 ]", &[1, 2]),
        ("# [ 01 02 ]", &[1, 2]),
        ("#[ 01 /* x */ 02 ]", &[1, 2]),
        ("# /* c */ [ 01 // x\n 02 /* y */ 03 ]", &[1, 2, 3]),
        ("#[ aB Cd ef ]", &[0xAB, 0xCD, 0xEF]),
        ("#[]", &[]),
        ("#[ ]", &[]),
        ("#[\n]", &[]),
    ];
    for (text, expected) in cases {
        let root = parse_kv3_text(&format!("{{ a = {text} }}"));
        assert_eq!(value(&root, "a"), &Value::Blob(expected.to_vec()), "`{text}`");
    }
}

/// Documented difference: upstream's `SkipComment` continues a `//` comment onto the next line
/// after a trailing `\`; cs2-kv3 (686975d) ends it at the newline, because Windows paths end
/// comments with a backslash (`// see C:\models\`) and must not swallow the next entry.
#[test]
fn line_comment_does_not_continue_after_backslash() {
    for newline in ["\n", "\r\n"] {
        let body = format!(
            "{{{n}  // comment \\{n}  b = 2{n}  c = 3{n}}}",
            n = newline,
        );
        let root = parse_kv3_text(&body);
        assert_eq!(value(&root, "b").as_i64(), Some(2), "{body:?}: the comment ended at the newline");
        assert_eq!(value(&root, "c").as_i64(), Some(3), "{body:?}");
    }
}

#[test]
fn deserializes_comments_between_tokens() {
    let root = parse_kv3_text(
        "{ /*a*/ a /*b*/ = /*c*/ 1 /*d*/ b = [ /*e*/ 1 /*f*/ , /*g*/ 2 /*h*/ ] /*i*/ c // x\n = 3 }",
    );
    assert_eq!(value(&root, "a").as_i64(), Some(1));
    assert_eq!(
        value(&root, "b").as_array().map(|a| a.iter().filter_map(|v| v.as_i64()).collect::<Vec<_>>()),
        Some(vec![1, 2]),
    );
    assert_eq!(value(&root, "c").as_i64(), Some(3));
}

/// Upstream `DeserializesMultilineStringSyntax`: `(source, expected)` with C# literals expanded
/// (`\"` is a quote, `\\n` a backslash-n kept verbatim, `\\\"` a backslash-quote pair).
#[test]
fn deserializes_multiline_string_syntax() {
    let cases = [
        // The closer must be on a line of its own, so `"""` mid-line keeps reading:
        ("\"\"\"\nabc\"\"\" b = 1\n\"\"\"", "abc\"\"\" b = 1"),
        // A backslash before the newline defers the close past a line-start `"""`.
        ("\"\"\"\nline\\\n\"\"\"\nstill\n\"\"\"", "line\\\n\"\"\"\nstill"),
        ("\"\"\"\n\\\"\"\"x\n\"\"\"", "\"\"\"x"),
        ("\"\"\"\nx \\\"\"\" y\n\"\"\"", "x \"\"\" y"),
        ("\"\"\"\r\nline1\r\nline2\r\n\"\"\"", "line1\nline2"),
        ("\"\"\"\nx\r\ny\r\n\"\"\"", "x\ny"),
        ("\"\"\"\n\"\"\"", ""),
        ("\"\"\"\r\n\"\"\"", ""),
        ("\"\"\"\n\n\"\"\"", ""),
        ("\"\"\"\n\n\nx\n\n\"\"\"", "\n\nx\n"),
        ("\"\"\"\n\tline\n\t\"\"\"\n\tb = 1\n\"\"\"", "\tline\n\t\"\"\"\n\tb = 1"),
        ("\"\"\"\n\"\"\n\"x\"\n\"\"\"", "\"\"\n\"x\""),
        // Escapes are NOT processed in raw strings.
        ("\"\"\"\nx\\ny\\t\n\"\"\"", "x\\ny\\t"),
    ];
    for (text, expected) in cases {
        let body = format!("{{\n\ta = {text}\n}}");
        let root = parse_kv3_text(&body);
        assert_eq!(value(&root, "a").as_str(), Some(expected), "`{text}`");
    }
}

#[test]
fn deserializes_multiline_strings_in_array_and_key() {
    let root = parse_kv3_text("{\n\ta = [ \"\"\"\nx\n\"\"\", \"\"\"\ny\n\"\"\"]\n\"\"\"\nkey\nname\n\"\"\" = 1\n}");
    let array = value(&root, "a").as_array().expect("array");
    assert_eq!(array[0].as_str(), Some("x"));
    assert_eq!(array[1].as_str(), Some("y"));
    assert_eq!(value(&root, "key\nname").as_i64(), Some(1));
}

#[test]
fn deserializes_unusual_keys() {
    let root = parse_kv3_text(
        "{ true = 1 null = 2 false = 3 a.b = 4 .y = 5 _x = 6 x_ = 7 \"\" = 8 \"1abc\" = 9 \"a:b\" = 10 }",
    );
    // Object key order is preserved.
    assert_eq!(
        root.keys().collect::<Vec<_>>(),
        vec!["true", "null", "false", "a.b", ".y", "_x", "x_", "", "1abc", "a:b"],
        "unusual keys in source order",
    );
    // The bare `.y` key reads fine (a leading dot IS a valid identifier upstream) but is written
    // quoted: the writer's predicate is 686975d's `[A-Za-z_][A-Za-z0-9_.]*`.
    let doc = Document {
        header: Header::parse(UPSTREAM_HEADER.trim_end()).expect("header"),
        root,
    };
    let written = write(&doc);
    for (line, why) in [
        ("\n\ta.b = 4\r\n", "a dotted identifier stays bare"),
        ("\n\t\".y\" = 5\r\n", "a leading dot is quoted on write"),
        ("\n\t_x = 6\r\n", "a leading underscore stays bare"),
        ("\n\t\"1abc\" = 9\r\n", "a digit-leading key is quoted"),
        ("\n\t\"a:b\" = 10\r\n", "a key with `:` is quoted"),
    ] {
        assert!(written.contains(line), "{why}: {line:?} not in\n{written}");
    }
}

#[test]
fn deserializes_vertical_tab_and_form_feed_as_whitespace() {
    let root = parse_kv3_text("{\u{b}a = 1\u{c}}");
    assert_eq!(value(&root, "a").as_i64(), Some(1));
}

#[test]
fn deserializes_containers_without_whitespace() {
    let root = parse_kv3_text(
        "{a=1 b=[1,2,] c={d=\"x\"} e=[[]] f={g={}} h=[{}] i=[1,\"a\",[2],{j=3},#[01],null,true,1.5,-2]}",
    );
    assert_eq!(value(&root, "a").as_i64(), Some(1));
    assert_eq!(value(&root, "b").as_array().map(|a| a.len()), Some(2));
    let c = value(&root, "c").as_object().expect("c");
    assert_eq!(c.get("d").and_then(Value::as_str), Some("x"));
    let e = value(&root, "e").as_array().expect("e");
    assert_eq!(e[0].as_array().map(|a| a.len()), Some(0));
    let f = value(&root, "f").as_object().expect("f");
    assert_eq!(f.get("g").and_then(Value::as_object).map(Object::len), Some(0));
    let h = value(&root, "h").as_array().expect("h");
    assert!(h[0].as_object().is_some());
    let i = value(&root, "i").as_array().expect("i");
    assert_eq!(i.len(), 9);
    assert_eq!(i[0].as_i64(), Some(1));
    assert_eq!(i[1].as_str(), Some("a"));
    assert_eq!(i[2].as_array().map(|a| a.len()), Some(1));
    assert_eq!(i[3].as_object().and_then(|o| o.get("j")).and_then(Value::as_i64), Some(3));
    assert_eq!(i[4], Value::Blob(vec![1]));
    assert_eq!(i[5], Value::Null);
    assert_eq!(i[6].as_bool(), Some(true));
    assert_eq!(i[7].as_f64(), Some(1.5));
    assert_eq!(i[8].as_i64(), Some(-2));
}

#[test]
fn deserializes_deeply_nested_arrays() {
    let deep = format!("{}1{}", "[".repeat(50), "]".repeat(50));
    let root = parse_kv3_text(&format!("{{ a = {deep} }}"));
    let mut current = value(&root, "a");
    for _ in 0..50 {
        current = current.as_array().expect("nested array").first().expect("element");
    }
    assert_eq!(current.as_i64(), Some(1));
}

// ============================================================================================
// EscapeSequenceTestCase.cs
// ============================================================================================

/// Upstream's `UnescapeString` maps only `\n` and `\t` and drops the backslash for every other
/// escape (`\r` -> `r`, `\v` -> `v`, …). WP5 round 2 item 3 restores the pre-conformance
/// behaviour where the two platforms agree to disagree, so cs2-kv3 keeps those escapes: `\r` is
/// a carriage return (the writer emits it; a write->parse round trip must be exact — see the
/// `escapes.kv3` fixture round trip) and any other `\X` keeps BOTH characters. Values below are
/// the restored ones, inputs are upstream's fixture.
#[test]
fn rare_escape_sequences() {
    let text = include_str!("../fixtures/vkv/rare_escape_sequences.kv3");
    let root = parse(text).expect("parses").root;
    for (key, expected) in [
        ("carriage_return", "hello\rworld"),
        ("vertical_tab", "hello\\vworld"),
        ("backspace", "hello\\bworld"),
        ("form_feed", "hello\\fworld"),
        ("alert", "hello\\aworld"),
        ("question_mark", "hello\\?world"),
        ("single_quote", "hello\\'world"),
    ] {
        assert_eq!(value(&root, key).as_str(), Some(expected), "`{key}`");
    }
}

#[test]
fn chained_backslash_patterns() {
    let text = include_str!("../fixtures/vkv/chained_backslash_patterns.kv3");
    let root = parse(text).expect("parses").root;
    // The escape toggle counts backslashes in pairs: `\"` closes nothing.
    assert_eq!(value(&root, "backslash_quote").as_str(), Some("\\\""));
    assert_eq!(value(&root, "four_backslashes").as_str(), Some("\\\\\\\\"));
    assert_eq!(value(&root, "alternating").as_str(), Some("\\\"\\\""));
    assert_eq!(value(&root, "complex_path").as_str(), Some("C:\\Program Files\\\"Game\"\\data\\"));
    // Upstream drops the backslash of an unknown escape (`x`, `hexllo`); the union keeps it,
    // so `\x` stays the two characters backslash and `x` (round 2 item 3).
    assert_eq!(value(&root, "unknown_escape").as_str(), Some("\\x"), "`\\x` stays two characters");
    assert_eq!(value(&root, "unknown_escape_mid").as_str(), Some("he\\xllo"));
}

#[test]
fn escaped_quote_at_start() {
    let text = include_str!("../fixtures/vkv/escaped_quote_at_start.kv3");
    let root = parse(text).expect("parses").root;
    assert_eq!(value(&root, "escaped_quote").as_str(), Some("\""));
    assert_eq!(value(&root, "escaped_quote_then_text").as_str(), Some("\"hello"));
    assert_eq!(value(&root, "text_then_escaped_quote").as_str(), Some("hello\""));
}

#[test]
fn string_value_escapes() {
    // Upstream's decoded values sit in the right column; ours differ where the escape tables
    // disagree (round 2 item 3): unknown `\X` keeps its backslash and `\r` is a carriage
    // return, so upstream's `xr0u0041a` is `\x` + CR + `\0` + `\u0041` + `\a` here.
    let cases = [
        ("\"\\x\\r\\0\\u0041\\a\"", "\\x\r\\0\\u0041\\a"),
        ("\"line1\nline2\"", "line1\nline2"),
        // `\<LF>`: upstream drops the backslash; the union keeps the pair.
        ("\"line1\\\nline2\"", "line1\\\nline2"),
        ("'it\\'s \"q\"'", "it\\'s \"q\""),
        ("'a\\tb'", "a\tb"),
        ("\"\"", ""),
        ("''", ""),
        ("\"#[01] //x /*y*/ {}[]=,:|;\"", "#[01] //x /*y*/ {}[]=,:|;"),
    ];
    for (text, expected) in cases {
        let body = format!("{{ a = {text} }}");
        let root = parse_kv3_text(&body);
        assert_eq!(value(&root, "a").as_str(), Some(expected), "`{text}`");
    }
}

#[test]
fn quoted_key_escapes() {
    let root = parse_kv3_text(
        "{ \"a\\tb\" = 1 \"c\\nd\" = 2 \"e\\\\f\" = 3 \"g\\\"h\" = 4 'i' = 5 \"\\u00e9\" = 6 }",
    );
    // Upstream's last key is `u00e9` (the backslash of `\u00e9` is dropped); the union keeps
    // it, so the key keeps its backslash (round 2 item 3).
    assert_eq!(
        root.keys().collect::<Vec<_>>(),
        vec!["a\tb", "c\nd", "e\\f", "g\"h", "i", "\\u00e9"],
    );
}

// ============================================================================================
// HeadersTestCase.cs
// ============================================================================================

#[test]
fn invalid_headers_throw() {
    let version = "encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}";
    let generic = "format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7}";
    let headers = [
        "<!--".to_string(),
        "<!-- -->".to_string(),
        "<!-- kv3 -->".to_string(),
        "<!-- kv3 encoding:text:".to_string(),
        format!("<!-- kv3 {generic} {version} -->\n{{}}"),
        format!("<!-- kv4 {version} {generic} -->\n{{}}"),
        format!("<!-- kv3 encoding~text:version{{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}} {generic} -->\n{{}}"),
        format!("<!-- kv3 encoding:text~version{{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}} {generic} -->\n{{}}"),
        format!("<!-- kv3 encoding:text:version~e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}} {generic} -->\n{{}}"),
        format!("<!-- kv3 {version} format~generic:version{{7412167c-06e9-4698-aff2-e63eb59037e7}} -->\n{{}}"),
        format!("<!-- kv3 {version} format:generic~version{{7412167c-06e9-4698-aff2-e63eb59037e7}} -->\n{{}}"),
        format!("<!-- kv3 {version} format:generic:version~7412167c-06e9-4698-aff2-e63eb59037e7}} -->\n{{}}"),
        format!("<!-- kv3 {version} {generic} extra-data -->\n{{}}"),
        format!("<!-- kv3 {version} {generic} ->-\n{{}}"),
        // No space after `<!--` (or after `kv3`): one over-long token, not the keyword.
        format!("<!--kv3 {version} {generic} -->\n{{}}"),
        format!("<!-- kv3{version} {generic} -->\n{{}}"),
        format!("<!-- kv3 encoding:binary:version{{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}} {generic} -->\n{{}}"),
        format!("<!-- kv3 encoding:text2:version{{00000000-8a33-41c5-9977-a76d3a32aa0d}} {generic} -->\n{{}}"),
        format!("<!-- kv3 encoding:\"text\":version{{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}} {generic} -->\n{{}}"),
        format!("<!-- kv3 {version} format:'generic':version{{7412167c-06e9-4698-aff2-e63eb59037e7}} -->\n{{}}"),
        format!("<!-- kv3 {version} format:1generic:version{{7412167c-06e9-4698-aff2-e63eb59037e7}} -->\n{{}}"),
        format!("< !-- kv3 {version} {generic} -->\n{{}}"),
        // `version` without its `{`.
        format!("<!-- kv3 encoding:text:version e21c7f3c-8a33-41c5-9977-a76d3a32aa0d {generic} -->\n{{}}"),
        format!("<!-- kv3 encoding:text:version{{{{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}}}} {generic} -->\n{{}}"),
        format!("<!-- kv3 {version} {generic} - ->\n{{}}"),
    ];
    for header in headers {
        let error = parse_strict(&header).map(|doc| (doc.header.encoding, header.clone()));
        assert!(error.clone().is_err(), "must refuse: {header:?}");
        assert!(
            error.unwrap_err().msg.contains("kv3 header"),
            "the message should say the header failed: {header:?}",
        );
    }
}

#[test]
fn incorrect_encoding_text_guid_throws() {
    // The body is attached so the only reason to refuse is the GUID (the upstream input has no
    // body either, but then the root check would fire first and make the test vacuous).
    assert!(parse_strict(
        "<!-- kv3 encoding:text:version{e21c7f3c-8a33-4111-9977-a76d3a32aa0d} \
         format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{}",
    )
    .is_err(), "a wrong encoding GUID is refused");
}

#[test]
fn incorrect_format_generic_guid_throws() {
    // (Plain `parse` keeps it — 686975d never checked a GUID — so this is the strict reader.)
    assert!(parse_strict(
        "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} \
         format:generic:version{7412167c-06e9-1337-aff2-e63eb59037e7} -->\n{}",
    )
    .is_err(), "a wrong generic-format GUID is refused");
}

#[test]
fn invalid_guid_forms_throw() {
    let headers = [
        "<!-- kv3 encoding:text:version{abc} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->",
        "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{abc} -->",
        "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d~ format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->",
        "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7~ -->}",
    ];
    for header in headers {
        // Through the strict reader: plain `parse` takes the 686975d union, whose line reader
        // also accepted some of these; with a missing body the object check refuses them
        // anyway, so this pins the header itself (a present body would reach the fallback).
        assert!(parse_strict(header).is_err(), "must refuse: {header:?}");
    }
}

#[test]
fn valid_headers_are_parsed() {
    let encoding = "encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}";
    let headers = [
        "<!-- KV3 ENCODING:TEXT:VERSION{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} FORMAT:GENERIC:VERSION{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{}".to_string(),
        "<!--    kv3     encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}      format:GENERIC:version{7412167c-06e9-4698-aff2-e63eb59037e7}     -->".to_string() + "\n{}",
        "<!--\tkv3\tencoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}\tformat:GENERIC:version{7412167c-06e9-4698-aff2-e63eb59037e7}\t-->\n{}".to_string(),
        "<!--\nkv3\n".to_string() + encoding + "\nformat:GENERIC:version{7412167c-06e9-4698-aff2-e63eb59037e7}\n-->\n{}",
        // Whitespace inside and around the `:`s and the braces.
        "<!-- kv3 encoding : text : version { e21c7f3c-8a33-41c5-9977-a76d3a32aa0d } format : generic : version { 7412167c-06e9-4698-aff2-e63eb59037e7 } -->\n{}".to_string(),
        // One token per line.
        "<!--\nkv3\nencoding\n:\ntext\n:\nversion\n{\ne21c7f3c-8a33-41c5-9977-a76d3a32aa0d\n}\nformat:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7}\n-->\n{}".to_string(),
        "<!-- kv3 /* c */ encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} // c\nformat:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{}".to_string(),
        "<!-- kv3 encoding:text:version{E21C7F3C-8A33-41C5-9977-A76D3A32AA0D} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7}-->{}".to_string(),
        "  \n\t<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{}".to_string(),
        "// c\n<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{}".to_string(),
        "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\r\n{}".to_string(),
    ];
    for header in headers {
        // The union accepts them and so does upstream: assert both.
        let doc = parse_strict(&header).unwrap_or_else(|error| panic!("{header:?}: {error}"));
        assert_eq!(doc.header.encoding.to_ascii_lowercase(), "text", "{header:?}");
        assert!(doc.root.is_empty(), "the body of {header:?} parses");
    }
}

/// Round-trip stability of a multi-line header: `Header::raw` keeps the consumed span verbatim.
#[test]
fn multiline_header_keeps_its_raw_form() {
    let text = "<!--\nkv3\nencoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}\nformat:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7}\n-->\n{}";
    let doc = parse(text).expect("parses");
    assert!(doc.header.raw.contains("<!--\nkv3\n"), "{:?}", doc.header.raw);
    assert_eq!(parse(&write(&doc)).expect("re-parses").header, doc.header);
}

// ============================================================================================
// RootTypesTestCase.cs — only the parts the frozen `Document` model can express
// ============================================================================================

#[test]
fn root_with_surrounding_comments() {
    // The root object survives comments before, inside and after it (upstream's
    // `DeserializesRootWithSurroundingComments`).
    for body in ["// hi\n/* x */\n{ a = 1 }", "{ a = 1 }\n// end", "{ a = 1 } // end"] {
        let root = parse_kv3_text(body);
        assert_eq!(value(&root, "a").as_i64(), Some(1), "{body:?}");
    }
}

// ============================================================================================
// SerializationTestCase.cs + ValveFormatTestCase.cs — semantic round trips (see the table)
// ============================================================================================

#[test]
fn creates_text_document() {
    // upstream adds a key and compares bytes with types_serialized.kv3; here the comparison is
    // between trees (see the writer table).
    let text = include_str!("../fixtures/vkv/types.kv3");
    let mut doc = parse(text).expect("parses");
    doc.root.insert("multiLineString".to_string(), Value::String("hello\nworld".into()));
    let written = write(&doc);
    let expected = object_of(include_str!("../fixtures/vkv/types_serialized.kv3"), "types_serialized");
    roots_equal(&doc.root, &expected, "types tree with the added key");
    assert_round_trip_semantically(&written, "types with the added key");
    // The added multi-line value is where the writers differ:
    assert!(
        written.contains("\tmultiLineString = \"hello\\nworld\"\r\n"),
        "cs2-kv3 escapes the newline (Valve's layout), upstream writes \"\"\":\n{written}",
    );
}

#[test]
fn serializes_array_with_added_key() {
    let text = include_str!("../fixtures/vkv/array.kv3");
    let mut doc = parse(text).expect("parses");
    doc.root.insert("test".to_string(), Value::String("success".into()));
    let written = write(&doc);
    let expected = object_of(include_str!("../fixtures/vkv/array_serialized.kv3"), "array_serialized");
    roots_equal(&doc.root, &expected, "array tree with the added key");
    assert_round_trip_semantically(&written, "array with the added key");
}

#[test]
fn serializes_nested_array_stably() {
    assert_round_trip_semantically(&fixture("array_nested.kv3"), "array_nested");
}

#[test]
fn serializes_escape_sequences_round_trip() {
    let text = include_str!("../fixtures/vkv/escape_sequences.kv3");
    assert_round_trip_semantically(text, "escape_sequences");
    let second = object_of(&write(&parse(text).expect("parses")), "escape_sequences rewritten");
    assert_eq!(value(&second, "combined").as_str(), Some("line1\nline2\ttab\\slash\"quote"));
}

#[test]
fn serializes_entity_name_and_localize_flags() {
    for (name, key, flag_name, expected) in [
        ("entity_name.kv3", "name", "entity_name", "some_entity"),
        ("localize.kv3", "title", "localize", "#SFUI_Title"),
    ] {
        let text = fixture(name);
        let doc = parse(&text).expect("parses");
        let again = parse(&write(&doc)).expect("re-parses");
        assert!(
            matches!(
                value(&again.root, key),
                Value::Flagged { flag, value } if flag == flag_name && value.as_str() == Some(expected),
            ),
            "{name}: the flag survives the write",
        );
    }
}

/// Upstream's view of a tree: a flag chain (`a:b:v`, nested here) is only its last flag (`b:v`).
fn collapse_flag_chains(object: &Object) -> Object {
    fn collapse(value: &Value) -> Value {
        match value {
            Value::Flagged { flag, value } => match value.as_ref() {
                Value::Flagged { .. } => collapse(value),
                inner => Value::Flagged { flag: flag.clone(), value: Box::new(collapse(inner)) },
            },
            Value::Array(items) => Value::Array(items.iter().map(collapse).collect()),
            Value::Object(inner) => Value::Object(collapse_flag_chains(inner)),
            other => other.clone(),
        }
    }
    let mut out = object.clone();
    let keys: Vec<String> = object.keys().map(str::to_string).collect();
    for key in keys {
        if let Some(value) = out.get_mut(&key) {
            *value = collapse(value);
        }
    }
    out
}

#[test]
fn serializes_flags_stably() {
    let text = include_str!("../fixtures/vkv/flagged_value.kv3");
    let mut doc = parse(text).expect("parses");
    doc.root.insert("test".to_string(), Value::String("success".into()));
    let expected = object_of(include_str!("../fixtures/vkv/flagged_value_serialized.kv3"), "flagged_value_serialized");
    // Upstream's expected file holds the chain collapsed to its last flag; cs2-kv3 keeps the
    // nested chain, so compare after applying upstream's collapse.
    roots_equal(&collapse_flag_chains(&doc.root), &expected, "flagged tree with the added key");
    assert_round_trip_semantically(&write(&doc), "flagged values");
}

#[test]
fn serializes_multiline_string_with_backslashes() {
    let doc = document_with(vec![("path", Value::String("C:\\Program Files\\hello\nworld".into()))]);
    let second = parse(&write(&doc)).expect("re-parses");
    assert_eq!(second.root.get("path").and_then(Value::as_str), Some("C:\\Program Files\\hello\nworld"));
}

/// Upstream `StringRoundTrips`: awkward strings written and re-read. (cs2-kv3 escapes `\n`
/// where upstream may write `"""` — see the table.)
#[test]
fn string_round_trips() {
    for text in [
        "a\n\"\"\"b\n\"\"\"",
        "a\nb\\",
        "\\\"\"\"\n",
        "\n",
        "42",
        "true",
        "null",
        "-1.5",
    ] {
        let doc = document_with(vec![("a", Value::String(text.to_string()))]);
        let second = parse(&write(&doc)).unwrap_or_else(|error| panic!("{text:?}: rewritten: {error}"));
        assert_eq!(second.root.get("a"), Some(&Value::String(text.to_string())), "{text:?} round trips");
    }
    // Upstream's writer emits a raw CR and its reader keeps it (its `\r` escape would decode
    // to a plain `r`); cs2-kv3 writes the `\r` escape, which the restored escape table decodes
    // to the exact carriage return again: no asymmetry (round 2 item 3).
    assert_eq!(
        parse(&write(&document_with(vec![("a", Value::String("a\nb\r".into()))])))
            .expect("re-parses")
            .root
            .get("a")
            .and_then(Value::as_str),
        Some("a\nb\r"),
        "the writer's `\\r` and the reader's CR are one thing again",
    );
}

/// Truncated or garbage input never panics: every byte prefix (respecting UTF-8 boundaries) of
/// one valid fixture either parses or errs (CM rule 8).
#[test]
fn every_prefix_of_a_file_parses_without_panicking() {
    let text = include_str!("../fixtures/vkv/array.kv3");
    for n in 0..=text.len() {
        if !text.is_char_boundary(n) {
            continue;
        }
        let Some(prefix) = text.get(..n) else { continue };
        let _ = parse(prefix);
        // Any `Err` is fine; a panic or UB is what this test is about.
    }
}

#[test]
fn serializes_empty_array() {
    let text = fixture("empty_array.kv3");
    let expected = fixture("empty_array_serialized.kv3");
    roots_equal(&object_of(&text, "empty_array"), &object_of(&expected, "empty_array_serialized"), "empty arrays");
    assert_round_trip_semantically(&text, "empty arrays");
}

#[test]
fn serializes_short_array() {
    let text = fixture("short_array.kv3");
    let expected = fixture("short_array_serialized.kv3");
    roots_equal(&object_of(&text, "short_array"), &object_of(&expected, "short_array_serialized"), "short arrays");
    assert_round_trip_semantically(&text, "short arrays");
    // Layout difference, spelled out: upstream writes `[ 42 ]` for a one-element number array
    // and `[ "a", "b" ]` inline, cs2-kv3 keeps one-element arrays as blocks and packs 4 numbers
    // per line (Valve's real output).
    let written = write(&parse(&text).expect("parses"));
    assert!(written.contains("\tfive_not_short = \r\n\t["), "{written}");
    assert!(written.contains("\t\t1, 2, 3, 4,"), "{written}");
    assert!(written.contains("\t\t\"a\","), "{written}");
}

#[test]
fn serializes_array_formatting() {
    let text = fixture("array_formatting.kv3");
    let expected = fixture("array_formatting_serialized.kv3");
    roots_equal(&object_of(&text, "array_formatting"), &object_of(&expected, "array_formatting_serialized"), "array formatting");
    assert_round_trip_semantically(&text, "array formatting");

    let doc = parse(&text).expect("parses");
    let root = &doc.root;
    assert_eq!(value(root, "four_ints").as_array().map(|a| a.len()), Some(4));
    assert_eq!(value(root, "nine_ints").as_array().map(|a| a.len()), Some(9));
    assert_eq!(value(root, "matrix4x4").as_array().map(|a| a.len()), Some(16));
    let vectors = value(root, "matrix_as_vectors").as_array().expect("matrix as vectors");
    assert_eq!(vectors.len(), 4);
    assert_eq!(vectors[0].as_array().map(|a| a.len()), Some(4));
    assert_eq!(
        value(root, "empty_arrays").as_array().map(|a| a.iter().filter(|v| v.as_array().is_some_and(|a| a.is_empty())).count()),
        Some(3),
    );
    let flagged = value(root, "flagged_array_element").as_array().expect("flagged array elements");
    assert!(matches!(&flagged[0], Value::Flagged { flag, .. } if flag == "resource"));
    assert!(matches!(&flagged[1], Value::Flagged { flag, .. } if flag == "panorama"));
}

#[test]
fn serializes_special_floats_in_arrays() {
    // Built the way upstream builds them: from Rust f64 values, not from source lexemes.
    // Value difference (WP5 round 2 item 6, restore): upstream's writer emits `inf`, `-inf`,
    // `nan` for non-finite doubles, `Number::from_f64` falls back to `0.0` (the writer is
    // Valve's pinned layout and out of scope, and a number's identity is its lexeme; the
    // *reader* accepts the inf/nan literals — see the `float_inf_nan.kv3` test below).
    let doc = document_with(vec![(
        "special",
        Value::Array(vec![
            Value::Number(Number::from_f64(1.0)),
            Value::Number(Number::from_f64(f64::INFINITY)),
            Value::Number(Number::from_f64(f64::NEG_INFINITY)),
            Value::Number(Number::from_f64(f64::NAN)),
        ]),
    )]);
    let written = write(&doc);
    // Layout difference (see the table): four numbers are not inline for cs2-kv3.
    assert!(written.contains("\tspecial = \r\n\t[\r\n\t\t1.0, 0.0, 0.0, 0.0,"), "{written}");
    let second = parse(&written).expect("re-parses").root;
    let array = value(&second, "special").as_array().expect("array");
    assert_eq!(array[0].as_f64(), Some(1.0), "the finite element survives");
    assert_eq!(array[1].as_f64(), Some(0.0), "the inf lexeme became 0.0");
    assert_eq!(array[2].as_f64(), Some(0.0), "the -inf lexeme became 0.0");
    assert_eq!(array[3].as_f64(), Some(0.0), "the nan lexeme became 0.0");
}

#[test]
fn serializes_float_inf_and_nan_input() {
    // The input side: `positiveInf = inf`, `positiveInf2 = +inf`, `negativeInf = -inf`, `nan = nan`
    // read as f64 specials, compared against the serialized fixture, and lexeme-faithful.
    let text = include_str!("../fixtures/vkv/float_inf_nan.kv3");
    let doc = parse(text).expect("parses");
    let expected = object_of(include_str!("../fixtures/vkv/float_inf_nan_serialized.kv3"), "inf_nan serialized");
    roots_equal(&doc.root, &expected, "inf/nan tree");
    assert_eq!(lexeme(value(&doc.root, "positiveInf2")), "+inf");
    assert_eq!(lexeme(value(&doc.root, "negativeInf")), "-inf");
    assert_round_trip_semantically(text, "float inf/nan");
}

#[test]
fn serializes_float_inf_and_nan_from_f64_falls_back_to_zero() {
    // Upstream `SerializesFloatInfAndNanFromDouble` writes `inf` / `-inf` / `nan`; WP5 round 2
    // item 6 restores the `0.0` fallback in `Number::from_f64` instead (the writer is out of
    // scope and pinned to Valve's layout). Round trips: `0.0` is what re-parses, and the
    // reader-side half of upstream's test — reading the inf/nan literals back as specials — is
    // the `float_inf_nan.kv3` test above; the fallback does not affect it.
    let doc = document_with(vec![
        ("positiveInf", Value::Number(Number::from_f64(f64::INFINITY))),
        ("negativeInf", Value::Number(Number::from_f64(f64::NEG_INFINITY))),
        ("nan", Value::Number(Number::from_f64(f64::NAN))),
    ]);
    let written = write(&doc);
    assert!(written.contains("\tpositiveInf = 0.0\r\n"), "{written}");
    assert!(written.contains("\tnegativeInf = 0.0\r\n"), "{written}");
    assert!(written.contains("\tnan = 0.0\r\n"), "{written}");
    let second = object_of(&written, "inf/nan rewritten");
    assert_eq!(value(&second, "positiveInf").as_f64(), Some(0.0));
    assert_eq!(value(&second, "negativeInf").as_f64(), Some(0.0));
    assert_eq!(value(&second, "nan").as_f64(), Some(0.0));
}

#[test]
fn serializes_binary_blobs() {
    let text = fixture("binary_blobs.kv3");
    let expected = fixture("binary_blobs_serialized.kv3");
    roots_equal(&object_of(&text, "binary_blobs"), &object_of(&expected, "binary_blobs_serialized"), "binary blobs");
    assert_round_trip_semantically(&text, "binary blobs");
}

#[test]
fn serializes_blob_in_array_is_not_short() {
    let doc = document_with(vec![(
        "blobs",
        Value::Array(vec![
            Value::Blob(vec![0xAA]),
            Value::Blob(vec![0xBB]),
            Value::Blob(vec![0xCC]),
        ]),
    )]);
    // Blobs are not written into an inline array (upstream `SerializesBlobInArrayIsNotShort`).
    let written = write(&doc);
    assert!(!written.contains("[ #["), "{written}");
}

#[test]
fn serializes_empty_object() {
    let text = fixture("empty_object.kv3");
    let expected = fixture("empty_object_serialized.kv3");
    roots_equal(&object_of(&text, "empty_object"), &object_of(&expected, "empty_object_serialized"), "empty objects");
    assert_round_trip_semantically(&text, "empty objects");
    // Layout difference (see the table): cs2-kv3 writes `{  }` inline, upstream a block.
    assert!(
        write(&parse(&text).expect("parses")).contains("\temptyObj = {  }"),
        "{}",
        write(&parse(&text).expect("parses")),
    );
}

#[test]
fn serializes_nested_objects_stably() {
    assert_round_trip_semantically(&fixture("nested_objects.kv3"), "nested objects");
}

#[test]
fn serializes_multiline_strings_stably() {
    let text = fixture("multiline_serialized.kv3");
    assert_round_trip_semantically(&text, "multiline strings");
    // The escaped triple quotes survive a rewrite byte for byte (the writer's escape table can
    // produce exactly the input line).
    let doc = parse(&text).expect("parses");
    let written = write(&doc);
    let needle = text.lines().find(|line| line.contains("withQuotes")).expect("withQuotes line");
    assert!(written.contains(needle), "the rewritten line differs:\n{written}\n(fixture: {needle})");
}

#[test]
fn serializes_string_escaping() {
    // The same escaping as upstream's writer: `\n`, `\t`, `\\`, `\"` (upstream's comment notes
    // `hello\nworld` would go to `"""`; cs2-kv3 always escapes — see the writer table).
    let doc = document_with(vec![
        ("newline", Value::String("hello\nworld".into())),
        ("tab", Value::String("a\tb".into())),
        ("backslash", Value::String("a\\b".into())),
        ("quote", Value::String("a\"b".into())),
        ("all", Value::String("\n\t\\\"".into())),
    ]);
    let written = write(&doc);
    for line in [
        "\tnewline = \"hello\\nworld\"",
        "\ttab = \"a\\tb\"",
        "\tbackslash = \"a\\\\b\"",
        "\tquote = \"a\\\"b\"",
        // For `all` compare against the writer's own rendering of the value, so no
        // hand-escaped needle is involved:
    ] {
        assert!(written.contains(&format!("{line}\r\n")), "missing `{line}`:\n{written}");
    }
    let all_rendering = cs2_kv3::write_value(&Value::String("\n\t\\\"".into()), 0);
    assert!(
        written.contains(&format!("\tall = {all_rendering}\r\n")),
        "`all` should be written as {all_rendering:?}:\n{written}",
    );
    let second = parse(&written).expect("re-parses");
    assert_eq!(second.root.get("all").and_then(Value::as_str), Some("\n\t\\\""));
}

#[test]
fn serializes_key_quoting() {
    // The upstream writer rules match cs2-kv3's exactly (bare identifier or quoted), so this
    // asserts the same written lines upstream's test does.
    let doc = document_with(vec![
        ("simple", Value::String("a".into())),
        ("with.dot", Value::String("b".into())),
        ("under_score", Value::String("c".into())),
        ("has space", Value::String("d".into())),
        ("has-dash", Value::String("e".into())),
        ("0startsWithDigit", Value::String("f".into())),
        ("", Value::String("g".into())),
        ("has\"quote", Value::String("h".into())),
        ("has\\backslash", Value::String("i".into())),
        ("has\nnewline", Value::String("j".into())),
        ("has\ttab", Value::String("k".into())),
        ("has'apostrophe", Value::String("l".into())),
        // A leading dot reads bare but is written quoted (686975d's predicate).
        (".dotFirst", Value::String("m".into())),
    ]);
    let written = write(&doc);
    for line in [
        "\tsimple = \"a\"",
        "\twith.dot = \"b\"",
        "\tunder_score = \"c\"",
        "\t\"has space\" = \"d\"",
        "\t\"has-dash\" = \"e\"",
        "\t\"0startsWithDigit\" = \"f\"",
        "\t\"\" = \"g\"",
        "\t\"has\\\"quote\" = \"h\"",
        "\t\"has\\\\backslash\" = \"i\"",
        "\t\"has\\nnewline\" = \"j\"",
        "\t\"has\\ttab\" = \"k\"",
        "\t\"has'apostrophe\" = \"l\"",
        "\t\".dotFirst\" = \"m\"",
    ] {
        assert!(written.contains(&format!("{line}\r\n")), "missing `{line}`:\n{written}");
    }
    let again = parse(&written).expect("re-parses");
    assert_eq!(
        again.root.keys().collect::<Vec<_>>(),
        vec![
            "simple", "with.dot", "under_score", "has space", "has-dash", "0startsWithDigit", "",
            "has\"quote", "has\\backslash", "has\nnewline", "has\ttab", "has'apostrophe", ".dotFirst",
        ],
    );
}

#[test]
fn binary_blob_inline_round_trip() {
    // Small blobs the writer emits inline re-read the same way (upstream's inline blob format).
    let doc = document_with(vec![
        ("empty", Value::Blob(vec![])),
        ("small", Value::Blob(vec![0x11, 0xFF])),
        ("exact32", Value::Blob((0..32u8).collect())),
    ]);
    let written = write(&doc);
    let second = object_of(&written, "blobs rewritten");
    assert_eq!(value(&second, "empty"), &Value::Blob(vec![]));
    assert_eq!(value(&second, "small"), &Value::Blob(vec![0x11, 0xFF]));
    assert_eq!(value(&second, "exact32"), &Value::Blob((0..32u8).collect()));
    // A blob over 32 bytes goes to Valve's column-0 layout (upstream indents differently).
    let with_large = document_with(vec![("large", Value::Blob((0..33u8).collect()))]);
    let written = write(&with_large);
    assert!(written.contains("large = \r\n#["), "{written}");
}

#[test]
fn comprehensive_round_trip() {
    let text = include_str!("../fixtures/vkv/types.kv3");
    let doc = parse(text).expect("parses");
    let second = object_of(&write(&doc), "types rewritten");
    assert_eq!(value(&second, "boolFalseValue").as_bool(), Some(false));
    assert_eq!(value(&second, "boolTrueValue").as_bool(), Some(true));
    assert_eq!(value(&second, "nullValue"), &Value::Null);
    assert_eq!(value(&second, "intValue").as_i64(), Some(128));
    assert_eq!(value(&second, "doubleValue").as_f64(), Some(64.123));
    assert_eq!(value(&second, "negativeIntValue").as_i64(), Some(-1337));
    assert_eq!(value(&second, "negativeDoubleValue").as_f64(), Some(-0.1337));
    assert_eq!(value(&second, "stringValue").as_str(), Some("hello world"));
    assert_eq!(value(&second, "empty.string").as_str(), Some(""));
    assert_eq!(value(&second, "singleQuotes").as_str(), Some("string"));
    assert_eq!(value(&second, "singleQuotesWithQuotesInside").as_str(), Some("string is \"pretty\" cool"));
}

/// Upstream canonicalising floats is the one writer difference that LOSES information
/// (`0.0000001` → `0.0`); the two trees are therefore intentionally NOT equal.
#[test]
fn serializes_floats_keep_their_lexemes() {
    let text = include_str!("../fixtures/vkv/floats.kv3");
    let doc = parse(text).expect("parses");
    let written = write(&doc);
    for (key, spelling) in [
        ("trailingZeros", "1.500000"),
        ("belowPrecision", "0.0000001"),
        ("huge", "1e20"),
        ("tiny", "1e-20"),
        ("mixedTrailing", "1.230000"),
    ] {
        assert!(written.contains(&format!("\t{key} = {spelling}\r\n")), "{key}:\n{written}");
    }
    assert!(write(&parse(&written).expect("re-parses")) == written, "floats rewrite is stable");
    let expected = object_of(include_str!("../fixtures/vkv/floats_serialized.kv3"), "floats_serialized");
    assert!(
        !semantic_eq(&Value::Object(doc.root.clone()), &Value::Object(expected.clone())),
        "upstream's writer destroyed 0.0000001 → 0.0; the trees may not be semantically equal",
    );
}
