//! KV1 text writing: upstream's expected byte streams, the round-trip properties, and the
//! documented spelling of the binary-only value variants.
//!
//! Upstream test cases ported: `Text/SerializationTestCase.cs` (with
//! `Test Data/Text/serialization_expected.vdf`), `Text/ObjectSerializationTestCase.cs`,
//! `Text/ObjectSerializationTypesTestCase.cs` (`serialization_types_expected.vdf` and the
//! inline `CharAndDecimalSerializeAsScalars` expectation), `Text/BooleanTestCase.cs`'
//! serialization half (as integers — this value model has no boolean),
//! `RoundTripTestCase.cs` (the KV1 text parts) and `Text/Escaped*TestCase.cs` for the
//! escape-on-write side.

use cs2_kv1::text::{self, WriteOptions};
use cs2_kv1::{KeyValue, Object, Value};

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/text").join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("cannot read fixture {}: {err}", path.display()))
}

/// What upstream's `TestDataHelper.ReadTextResource` hands a test: no BOM (a `StreamReader`
/// eats it) and exactly one trailing newline.
fn expected(name: &str) -> String {
    let mut text = fixture(name);
    if let Some(stripped) = text.strip_prefix('\u{feff}') {
        text = stripped.to_string();
    }
    if text.ends_with('\n') {
        text.pop();
    }
    text.push('\n');
    text
}

/// Every fixture name in `fixtures/text`, sorted.
fn fixture_names() -> Vec<String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/text");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("fixtures/text")
        .map(|entry| entry.expect("dir entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".vdf"))
        .collect();
    names.sort();
    names
}

fn pair(key: &str, value: &str) -> KeyValue {
    object_pair(key, Value::String(value.to_string()))
}

fn object_pair(key: &str, value: Value) -> KeyValue {
    KeyValue { key: key.to_string(), value, condition: None }
}

fn object(entries: Vec<KeyValue>) -> Value {
    Value::Object(Object { entries })
}

// ---------------------------------------------------------------------------------------
// SerializationTestCase / ObjectSerializationTestCase / ObjectSerializationTypesTestCase
// ---------------------------------------------------------------------------------------

#[test]
fn creates_text_document() {
    // upstream SerializationTestCase.CreatesTextDocument
    let item0 = object(vec![
        pair("description", "Dota 2 is a complex game where you get sworn at\nin Russian all the time."),
        pair("Developer", "Valve Software"),
        pair("Name", "Dota 2"),
    ]);
    let item1 = object(vec![
        pair("description", "Known as \"America's #1 war-themed hat simulator\", this game lets you wear stupid items while killing people."),
        pair("Developer", "Valve Software"),
        pair("Name", "Team Fortress 2"),
    ]);

    let doc = KeyValue {
        key: "test data".to_string(),
        value: object(vec![object_pair("0", item0), object_pair("1", item1)]),
        condition: None,
    };

    assert_eq!(text::write(&doc), expected("serialization_expected.vdf"));
}

#[test]
fn creates_text_document_from_object_shaped_data() {
    // upstream ObjectSerializationTestCase.CreatesTextDocument
    let item0 = object(vec![
        pair("Name", "Dota 2"),
        pair("Developer", "Valve Software"),
        pair("description", "Dota 2 is a complex game where you get sworn at\nin Russian all the time."),
    ]);
    let item1 = object(vec![
        pair("Name", "Team Fortress 2"),
        pair("Developer", "Valve Software"),
        pair("description", "Known as \"America's #1 war-themed hat simulator\", this game lets you wear stupid items while killing people."),
    ]);

    let doc = KeyValue {
        key: "test data".to_string(),
        value: object(vec![object_pair("0", item0), object_pair("1", item1)]),
        condition: None,
    };

    assert_eq!(text::write(&doc), expected("object_serialization_expected.vdf"));
}

#[test]
fn creates_text_document_with_scalar_types() {
    // upstream ObjectSerializationTypesTestCase.CreatesTextDocument: the C# test serializes an
    // object with int/long/ulong/float/enum properties; here the same values are set directly.
    let scalars = object(vec![
        pair("VString", "Test String"),
        object_pair("VInt", Value::Int32(0x1020_3040)),
        object_pair("VLong", Value::Int64(0x0102_0304_0506_0708)),
        object_pair("VULong", Value::UInt64(0x8877_6655_4433_2211)),
        object_pair("VFloat", Value::Float32(1234.5678)),
        object_pair("VEnum", Value::Int32(1337)),
        object_pair("VFlags", Value::Int32(10)),
        object_pair("VByteEnum", Value::Int32(255)),
        object_pair("VShortEnum", Value::Int32(-1)),
        object_pair("VLongEnum", Value::Int64(0x0102_0304_0506_0708)),
        object_pair("VULongEnum", Value::UInt64(0x8877_6655_4433_2211)),
    ]);

    let doc = KeyValue {
        key: "test data".to_string(),
        value: object(vec![object_pair("0", scalars)]),
        condition: None,
    };

    assert_eq!(text::write(&doc), expected("serialization_types_expected.vdf"));
}

#[test]
fn char_and_decimal_serialize_as_scalars() {
    // upstream ObjectSerializationTypesTestCase.CharAndDecimalSerializeAsScalars: a char is a
    // one-character string and a decimal a float for this model.
    let doc = KeyValue {
        key: "test data".to_string(),
        value: object(vec![
            pair("VChar", "q"),
            object_pair("VDecimal", Value::Float32(-0.5)),
        ]),
        condition: None,
    };

    assert_eq!(text::write(&doc), "\"test data\"\n{\n\t\"VChar\"\t\"q\"\n\t\"VDecimal\"\t\"-0.5\"\n}\n");
}

#[test]
fn booleans_are_written_as_one_and_zero() {
    // upstream BooleanTestCase.DynamicSerialization, where the values are `KVValueType.Boolean`.
    // KV1 text has no boolean and this value model has no boolean variant either, so the same
    // bytes come out of the integers Valve itself uses for them.
    let doc = KeyValue {
        key: "object".to_string(),
        value: object(vec![
            object_pair("test1_false", Value::Int32(0)),
            object_pair("test2_true", Value::Int32(1)),
        ]),
        condition: None,
    };

    assert_eq!(text::write(&doc), expected("boolean_serialization.vdf"));
}

// ---------------------------------------------------------------------------------------
// The writer's rules
// ---------------------------------------------------------------------------------------

#[test]
fn an_empty_object_is_written_as_an_empty_block() {
    let doc = KeyValue { key: "root".to_string(), value: object(vec![]), condition: None };
    assert_eq!(text::write(&doc), "\"root\"\n{\n}\n");
}

#[test]
fn a_root_that_is_a_single_pair_writes_that_pair() {
    let doc = pair("a", "1");
    assert_eq!(text::write(&doc), "\"a\"\t\"1\"\n");

    // And that output parses back to the same document.
    assert_eq!(text::parse(&text::write(&doc)).unwrap(), doc);
}

#[test]
fn quotes_are_always_escaped_and_backslashes_only_with_escapes_on() {
    let doc = KeyValue {
        key: "root".to_string(),
        value: object(vec![pair("quoted", "a\"b"), pair("slashed", r"c\d"), pair("both", "e\"f\\g")]),
        condition: None,
    };

    assert_eq!(
        text::write(&doc),
        "\"root\"\n{\n\t\"quoted\"\t\"a\\\"b\"\n\t\"slashed\"\t\"c\\d\"\n\t\"both\"\t\"e\\\"f\\g\"\n}\n"
    );

    let escaped = text::write_with(&doc, &WriteOptions { escapes: true });
    assert_eq!(
        escaped,
        "\"root\"\n{\n\t\"quoted\"\t\"a\\\"b\"\n\t\"slashed\"\t\"c\\\\d\"\n\t\"both\"\t\"e\\\"f\\\\g\"\n}\n"
    );

    // With escapes on the output reads back as the document that was written.
    let options = text::ReadOptions { escapes: true, ..Default::default() };
    assert_eq!(text::parse_with(&escaped, &options).unwrap(), doc);
}

#[test]
fn real_newlines_in_a_value_are_written_as_they_are() {
    let doc = KeyValue {
        key: "root".to_string(),
        value: object(vec![pair("multi", "one\ntwo")]),
        condition: None,
    };
    assert_eq!(text::write(&doc), "\"root\"\n{\n\t\"multi\"\t\"one\ntwo\"\n}\n");
    assert_eq!(text::parse(&text::write(&doc)).unwrap(), doc);
}

#[test]
fn binary_only_values_are_stringified() {
    // The documented spellings (see the writer's `# Differences from upstream`): integers in
    // decimal, a float in Rust's shortest round-trip form, a colour as the decimal of its
    // little-endian i32 (what upstream's reader turns a type 6 into), a blob as upstream's
    // uppercase hex, a wide string as its text.
    let doc = KeyValue {
        key: "root".to_string(),
        value: object(vec![
            object_pair("i32", Value::Int32(-5)),
            object_pair("ptr", Value::Pointer(7)),
            object_pair("i64", Value::Int64(-9_223_372_036_854_775_808)),
            object_pair("u64", Value::UInt64(18_446_744_073_709_551_615)),
            object_pair("f32", Value::Float32(1234.5678)),
            object_pair("f32small", Value::Float32(-0.5)),
            object_pair("wide", Value::WideString("herzlich".to_string())),
            object_pair("color", Value::Color([0xab, 0xcd, 0xef, 0x00])),
            object_pair("blob", Value::Binary(vec![0xab, 0xcd, 0xef])),
            object_pair("empty blob", Value::Binary(vec![])),
        ]),
        condition: None,
    };

    assert_eq!(
        text::write(&doc),
        "\"root\"\n{\n\
         \t\"i32\"\t\"-5\"\n\
         \t\"ptr\"\t\"7\"\n\
         \t\"i64\"\t\"-9223372036854775808\"\n\
         \t\"u64\"\t\"18446744073709551615\"\n\
         \t\"f32\"\t\"1234.5677\"\n\
         \t\"f32small\"\t\"-0.5\"\n\
         \t\"wide\"\t\"herzlich\"\n\
         \t\"color\"\t\"15715755\"\n\
         \t\"blob\"\t\"AB CD EF\"\n\
         \t\"empty blob\"\t\"\"\n\
         }\n"
    );
}

#[test]
fn value_tuples_serialize_as_item_pairs() {
    // upstream ValueTupleTextTestCase.SerializeValueTuple2Elements / 10Elements (the
    // deserialization halves are object mapping). The C# copier names the fields `Item1…Item7`
    // and nests the rest in a `Rest` object; the bytes are the fixtures' — note that
    // `valuetuple2.vdf` has no trailing newline, so it goes through `expected`.
    let two = KeyValue {
        key: "root".to_string(),
        value: object(vec![pair("Item1", "hello"), pair("Item2", "world")]),
        condition: None,
    };
    assert_eq!(text::write(&two), expected("valuetuple2.vdf"));

    let ten = KeyValue {
        key: "root".to_string(),
        value: object(vec![
            pair("Item1", "1"),
            pair("Item2", "2"),
            pair("Item3", "3"),
            pair("Item4", "4"),
            pair("Item5", "5"),
            pair("Item6", "6"),
            pair("Item7", "7"),
            object_pair(
                "Rest",
                object(vec![pair("Item1", "8"), pair("Item2", "9"), pair("Item3", "10")]),
            ),
        ]),
        condition: None,
    };
    assert_eq!(text::write(&ten), expected("valuetuple10.vdf"));
}

#[test]
fn scalars_format_without_a_culture() {
    // upstream CultureInvariantFormattingTestCase, numerically: the same values and the same
    // expected strings, from the four C# types this model has (short/ushort/uint come out of
    // `Int32`/`UInt64` here). Upstream needs the culture test because .NET formatting is
    // locale-dependent; Rust's `Display` is not, so repeating it under a different locale is
    // structurally impossible rather than untested.
    let cases = [
        (Value::Int32(-12345), "-12345"),
        (Value::Int32(54321), "54321"),
        (Value::Int32(-1234567), "-1234567"),
        // upstream's `uint` case; this model has no `UInt32`, and the text is the point.
        (Value::UInt64(4_000_000_000), "4000000000"),
        (Value::Int64(-9876543210), "-9876543210"),
        (Value::UInt64(18_000_000_000_000_000_000), "18000000000000000000"),
        (Value::Pointer(-1234), "-1234"),
        (Value::Float32(0.8), "0.8"),
        (Value::Float32(-2.75), "-2.75"),
    ];

    for (value, expected_text) in cases {
        let doc = KeyValue { key: "root".to_string(), value: object(vec![object_pair("v", value)]), condition: None };
        assert_eq!(
            text::write(&doc),
            format!("\"root\"\n{{\n\t\"v\"\t\"{expected_text}\"\n}}\n")
        );
    }
}

#[test]
fn conditions_are_written_back() {
    let doc = KeyValue {
        key: "test case".to_string(),
        value: object(vec![
            KeyValue { key: "scalar".to_string(), value: Value::String("v".to_string()), condition: Some("$WIN32".to_string()) },
            KeyValue { key: "block".to_string(), value: object(vec![]), condition: Some("!$X360 && $PC".to_string()) },
        ]),
        condition: Some("$WIN32".to_string()),
    };

    // After the key for a block (Valve's own layout), after the value for a scalar pair.
    assert_eq!(
        text::write(&doc),
        "\"test case\" [$WIN32]\n{\n\t\"scalar\"\t\"v\" [$WIN32]\n\t\"block\" [!$X360 && $PC]\n\t{\n\t}\n}\n"
    );

    // And that is exactly the document that reads back.
    assert_eq!(text::parse(&text::write(&doc)).unwrap(), doc);
}

#[test]
fn a_root_scalar_pairs_condition_sits_between_the_key_and_the_value() {
    // The document's own pair is the one place where a condition after the value would not read
    // back: it would sit at document level behind the root pair, which the reader rejects.
    let doc = KeyValue {
        key: "a".to_string(),
        value: Value::String("1".to_string()),
        condition: Some("$X".to_string()),
    };

    assert_eq!(text::write(&doc), "\"a\" [$X]\t\"1\"\n");
    assert_eq!(text::parse(&text::write(&doc)).unwrap(), doc);
}

#[test]
fn a_written_condition_always_reads_back() {
    // Conditions in every position the reader accepts, root pair included: whatever the writer
    // does with them, the result has to parse to the same document.
    for input in [
        "\"a\" \"1\"",
        "\"a\" [$X] \"1\"",
        "\"root\" { \"a\" \"1\" [$X] }",
        "\"root\" [$X] { \"a\" \"1\" [$Y] }",
        "\"root\" { \"a\" [$X] { \"b\" \"1\" [$Y] } }",
        "\"root\" { \"a\" { \"b\" \"1\" } [$X] }",
    ] {
        let doc = text::parse(input).unwrap_or_else(|err| panic!("{input:?}: {err:?}"));
        let written = text::write(&doc);
        assert_eq!(
            text::parse(&written).unwrap_or_else(|err| panic!("{input:?} -> {written:?}: {err:?}")),
            doc,
            "{input:?} -> {written:?}"
        );
    }
}

#[test]
fn the_writer_never_emits_a_bom() {
    let input = fixture("object_person.vdf");
    assert!(input.starts_with('\u{feff}'));

    let output = text::write(&text::parse(&input).unwrap());
    assert!(!output.starts_with('\u{feff}'));
    assert_eq!(output, "\"object\"\n{\n\t\"FirstName\"\t\"Bob\"\n\t\"LastName\"\t\"Builder\"\n\t\"CanFixIt\"\t\"1\"\n}\n");
}

#[test]
fn comments_do_not_survive_a_round_trip() {
    let input = fixture("comment_singleline.vdf");
    let output = text::write(&text::parse(&input).unwrap());

    assert_ne!(output, input);
    assert!(!output.contains("Traditional dummy names"), "{output}");
    assert_eq!(output, "\"test case\"\n{\n\t\"foo\"\t\"bar\"\n}\n");
}

// ---------------------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------------------

/// The one fixture that already has the shape the writer produces: no BOM, exactly one tab
/// between a key and its value, every key and value quoted, LF line endings and a final
/// newline. Upstream's *other* fixtures are hand-written (a BOM, alignment tabs, unquoted
/// tokens) and are not the writer's output — see `the_fixtures_are_hand_written_not_writer_output`.
const WRITER_SHAPED: &[&str] = &["non_circular_list.vdf"];

#[test]
fn only_the_writer_shaped_fixture_comes_back_byte_for_byte() {
    for name in WRITER_SHAPED {
        let input = fixture(name);
        let doc = text::parse(&input).unwrap_or_else(|err| panic!("{name}: {err:?}"));
        assert_eq!(text::write(&doc), input, "{name} should round-trip byte for byte");
    }

    // Nothing else may: the comparison has to stay byte-exact (a BOM or a second tab must not
    // creep in), which is what `cargo test` on a `--nocapture` run showed for every fixture.
    for name in fixture_names() {
        if WRITER_SHAPED.contains(&name.as_str()) {
            continue;
        }
        let input = fixture(&name);
        let Ok(doc) = text::parse(&input) else {
            continue;
        };
        assert_ne!(
            text::write(&doc),
            input,
            "{name} is byte-identical to the writer's output and belongs in WRITER_SHAPED"
        );
    }
}

#[test]
fn the_fixtures_are_hand_written_not_writer_output() {
    // The upstream test data is written for the reader: a BOM, alignment tabs and unquoted
    // tokens are all normalised away by `write`, so only a semantic round trip holds for them.
    assert!(fixture("conditional.vdf").starts_with('\u{feff}'));
    assert!(fixture("conditional.vdf").contains("\"operating system\"\t\t\"windows 32-bit\""));
    assert!(fixture("boolean.vdf").contains("\"test1_false\"   \"0\""));
    assert!(fixture("conditional_discard.vdf").contains("\tOne\n"));

    // The writer's own output is pinned by the four upstream `*_expected.vdf` files
    // (`creates_text_document`, `creates_text_document_from_object_shaped_data`,
    // `creates_text_document_with_scalar_types`, `booleans_are_written_as_one_and_zero`).
    let doc = text::parse(&fixture("conditional.vdf")).unwrap();
    let written = text::write(&doc);
    assert!(!written.starts_with('\u{feff}'));
    assert!(!written.contains("\"\t\t\""), "{written}");
    assert!(!written.contains("\tOne\n"), "{written}");
}

/// The upstream fixtures `ReadOptions::default()` rejects: `InvalidSyntaxTestCase`'s set, the
/// conditional that may not sit before a key, the file whose stray `\"` needs escape
/// sequences, the `{CONDITION}` template, and the six inclusion fixtures (which need a loader).
/// Everything else parses and has to survive a write/parse round trip.
const REJECTED_BY_DEFAULT: &[&str] = &[
    "conditional_before_key.vdf",
    "empty.vdf",
    "escaped_ending_quote.vdf",
    "invalid_conditional.vdf",
    "invalid_zerobracerepeated.vdf",
    "kv_base_nested_once.vdf",
    "kv_include_nested_once.vdf",
    "kv_with_base.vdf",
    "kv_with_base_nesting.vdf",
    "kv_with_include.vdf",
    "kv_with_include_nesting.vdf",
    "multiple_root_objects.vdf",
    "nameonly.vdf",
    "partial_noclose.vdf",
    "partial_nodata.vdf",
    "partial_novalue.vdf",
    "partial_opening_key.vdf",
    "partial_opening_value.vdf",
    "partial_partialkey.vdf",
    "partial_partialvalue.vdf",
    "partialname.vdf",
    "quoteonly.vdf",
];

#[test]
fn every_fixture_that_parses_round_trips_semantically() {
    let names = fixture_names();
    let mut parsed = 0;
    let mut rejected = Vec::new();

    for name in &names {
        let input = fixture(name);
        let doc = match text::parse(&input) {
            Ok(doc) => doc,
            Err(_) => {
                rejected.push(name.clone());
                continue;
            }
        };
        parsed += 1;

        let written = text::write(&doc);
        let again = text::parse(&written).unwrap_or_else(|err| panic!("{name}: re-parse failed: {err:?}"));
        assert_eq!(again, doc, "{name} does not round-trip");
    }

    // Exactly the documented rejections, so a new failure cannot go unnoticed and a fixture that
    // starts parsing again does not slip through either.
    assert_eq!(rejected, REJECTED_BY_DEFAULT);
    assert_eq!(parsed + rejected.len(), names.len());
    assert_eq!(parsed, 51);
}

#[test]
fn a_written_document_with_escapes_round_trips_through_escaped_reading() {
    // A value with a backslash only survives a byte round trip with escapes on both sides.
    let doc = KeyValue {
        key: "root".to_string(),
        value: object(vec![pair("path", r"C:\games\cs2"), pair("quote", "a \"quoted\" word")]),
        condition: None,
    };

    let written = text::write_with(&doc, &WriteOptions { escapes: true });
    let options = text::ReadOptions { escapes: true, ..Default::default() };
    assert_eq!(text::parse_with(&written, &options).unwrap(), doc);

    // With escapes off the backslash survives (it is literal on both sides) but the quote does
    // not: `\"` writes a `\` plus a quote, which reads back as an unterminated string.
    let written = text::write(&doc);
    assert!(written.contains("\\\""), "{written}");
}

#[test]
fn deep_nesting_round_trips_through_the_writer() {
    let mut current = object(vec![pair("leaf", "found")]);
    for level in 0..64 {
        current = object(vec![object_pair(&format!("level{level}"), current)]);
    }
    let doc = KeyValue { key: "root".to_string(), value: current, condition: None };

    let written = text::write(&doc);
    assert!(written.contains("\t\t\t\t\"leaf\"\t\"found\""));
    assert_eq!(text::parse(&written).unwrap(), doc);
}
