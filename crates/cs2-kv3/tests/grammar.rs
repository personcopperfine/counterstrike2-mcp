//! Grammar, one feature at a time, plus the writer forms that are not simple round trips.

use cs2_kv3::{
    parse, write, write_styled, write_value, Document, Header, Newline, Number, Object, ParseError,
    Style, Value,
};

const HDR: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} \
                   format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->";

/// A one-key document body, CRLF, Valve layout.
fn document(body_lines: &[&str]) -> String {
    let mut out = String::from(HDR);
    out.push_str("\r\n{\r\n");
    for line in body_lines {
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push('}');
    out
}

fn parsed_root(text: &str) -> Object {
    parse(text).expect("parses").root
}

/// An object from several `\t<entry>` lines, on the shared test header.
fn parse_kv3_text_root(entries: &[&str]) -> Object {
    parsed_root(&document(entries))
}

fn get<'a>(object: &'a Object, key: &str) -> &'a Value {
    object.get(key).unwrap_or_else(|| panic!("no key {key}"))
}

#[test]
fn header_fields_and_raw_line() {
    let header = Header::parse(HDR).expect("header");
    assert_eq!(header.raw, HDR);
    assert_eq!(header.encoding, "text");
    assert_eq!(header.format, "generic");
    assert_eq!(header.format_guid(), Some("7412167c-06e9-4698-aff2-e63eb59037e7"));

    // The header may carry a trailing CR (it usually comes from a CRLF file).
    let with_cr = Header::parse(&format!("{HDR}\r")).expect("header with CR");
    assert_eq!(with_cr.raw, HDR);

    assert!(Header::parse("{ not a header }").is_err());
    assert!(Header::parse("<!-- kv3 format:generic:version{x} -->").is_err());
    let binary = Header::parse(
        "<!-- kv3 encoding:binary:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} \
         format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->",
    );
    assert!(binary.unwrap_err().msg.contains("encoding"));
}

#[test]
fn with_format_rewrites_only_the_format_field() {
    let generic = Header::parse(HDR).expect("header");
    let modeldoc = generic.with_format("modeldoc36");
    assert_eq!(modeldoc.format, "modeldoc36");
    assert_eq!(modeldoc.encoding, "text");
    assert_eq!(modeldoc.format_guid(), Some("972dada4-b828-45a4-bb93-7795cf0585da"));
    assert!(modeldoc.raw.contains("encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}"));
    assert!(modeldoc.raw.starts_with("<!-- kv3 ") && modeldoc.raw.ends_with(" -->"));
    assert_eq!(modeldoc.with_format("modeldoc36"), modeldoc, "idempotent");
    Header::parse(&modeldoc.raw).expect("the rewritten header parses");

    // The editor's own format: the name changes, the modeldoc GUID stays (the real modeldoc41
    // GUID is not recorded in this repo).
    let editor = generic.with_format("modeldoc41");
    assert_eq!(editor.format, "modeldoc41");
    assert_eq!(editor.format_guid(), Some("972dada4-b828-45a4-bb93-7795cf0585da"));
    Header::parse(&editor.raw).expect("the rewritten header parses");
    assert_eq!(editor.with_format("modeldoc36").format_guid(), Some("972dada4-b828-45a4-bb93-7795cf0585da"));
}

#[test]
fn scalars() {
    let text = document(&[
        "\tnull_value = null",
        "\ttrue_value = true",
        "\tfalse_value = false",
        "\tint_value = -1337",
        "\tfloat_value = 64.0",
        "\tstring_value = \"hello world\"",
        "\tempty_string = \"\"",
    ]);
    let root = parsed_root(&text);
    assert_eq!(get(&root, "null_value"), &Value::Null);
    assert_eq!(get(&root, "true_value").as_bool(), Some(true));
    assert_eq!(get(&root, "false_value").as_bool(), Some(false));
    assert_eq!(get(&root, "int_value"), &Value::Number(Number::from_i64(-1337)));
    assert_eq!(get(&root, "float_value"), &Value::Number(Number::parse("64.0").unwrap()));
    assert_eq!(get(&root, "string_value").as_str(), Some("hello world"));
    assert_eq!(get(&root, "empty_string").as_str(), Some(""));
    assert_eq!(write(&parse(&text).unwrap()), text);
}

#[test]
fn number_lexemes_are_preserved() {
    let text = document(&[
        "\tzero = 0.0",
        "\tprecise = 74.56868",
        "\tnegative = -0.047012",
        "\texponent = 1e-05",
        "\tbig = 2E+3",
        "\tinteger = 1",
    ]);
    let root = parsed_root(&text);
    let lexeme = |key: &str| match get(&root, key) {
        Value::Number(n) => n.text.clone(),
        other => panic!("{key} is {other:?}"),
    };
    assert_eq!(lexeme("zero"), "0.0");
    assert_eq!(lexeme("precise"), "74.56868");
    assert_eq!(lexeme("negative"), "-0.047012");
    assert_eq!(lexeme("exponent"), "1e-05");
    assert_eq!(lexeme("big"), "2E+3");
    assert_eq!(lexeme("integer"), "1");
    assert_eq!(write(&parse(&text).unwrap()), text);
}

#[test]
fn number_accessors() {
    let int = Number::from_i64(7);
    assert_eq!(int.text, "7");
    assert_eq!(int.as_i64(), Some(7));
    assert_eq!(int.as_f64(), Some(7.0));
    assert!(!int.is_float());

    let float = Number::from_f64(1.0);
    assert_eq!(float.text, "1.0", "integral floats keep a `.0`");
    assert!(float.is_float());
    assert_eq!(float.as_i64(), None);
    assert_eq!(Number::from_f64(0.1).text, "0.1");

    let parsed = Number::parse("-0.047012").expect("valid");
    assert_eq!(parsed.as_f64(), Some(-0.047012));
    assert_eq!(parsed.text, "-0.047012");

    // The same refusals as 686975d's list, minus the three the reader must now accept
    // (`1.`, `.5`, the `nan` literal — upstream number forms), plus extra pins for lexemes
    // that fail under both the old and the new validator.
    for bad in ["", "-", "1abc", "1.2.3", "0x10", "1e", "1e+", "--1", "1-2", "1_0", "-nan", "-."] {
        assert!(Number::parse(bad).is_none(), "{bad} should not parse as a number");
    }
    // The upstream number forms (upstream `ParseValue`): a leading sign, a leading or trailing
    // decimal point, the `inf`/`nan` literals.
    for (text, value) in [
        (".5", 0.5),
        ("-.5", -0.5),
        ("5.", 5.0),
        ("+1.5", 1.5),
        ("+1337", 1337.0),
        ("1e5", 100_000.0),
        ("inf", f64::INFINITY),
        ("-inf", f64::NEG_INFINITY),
        ("nan", f64::NAN),
    ] {
        let parsed = Number::parse(text).unwrap_or_else(|| panic!("{text} should parse"));
        assert_eq!(parsed.text, text, "{text} keeps its lexeme");
        if text == "nan" {
            assert!(parsed.as_f64().is_some_and(|f| f.is_nan()));
        } else {
            assert_eq!(parsed.as_f64(), Some(value), "{text}");
        }
    }
    // A bare `+` is a number lexeme (`Number::parse` is the validator); the parser is what
    // normalises it to the value 0 upstream reads it as.
    assert_eq!(Number::parse("+").map(|n| n.text), Some("+".to_string()));
    // Not-a-number has no KV3 spelling; `from_f64` documents the 0.0 fallback (round 2 item 6:
    // the writer is Valve's pinned layout, so it never emits `inf`/`nan` from Rust values).
    assert_eq!(Number::from_f64(f64::NAN).text, "0.0");
    assert_eq!(Number::from_f64(f64::INFINITY).text, "0.0");
}

#[test]
fn flags_blobs_arrays_and_nesting() {
    let text = document(&[
        "\tresource = resource_name:\"particles/items3_fx/star_emblem.vpcf\"",
        "\tinline = [ 0.0, 0.0, 1.0 ]",
        "\tempty = [  ]",
        "\tpacked = ",
        "\t[",
        "\t\t1, 2, 3, 4,",
        "\t\t5,",
        "\t]",
        "\tflags = ",
        "\t[",
        "\t\tresource_name:\"a.vmat\",",
        "\t]",
        "\tnested = ",
        "\t{",
        "\t\t_class = \"Child\"",
        "\t\tchildren = [  ]",
        "\t}",
    ]);
    let root = parsed_root(&text);
    match get(&root, "resource") {
        Value::Flagged { flag, value } => {
            assert_eq!(flag, "resource_name");
            assert_eq!(value.as_str(), Some("particles/items3_fx/star_emblem.vpcf"));
        }
        other => panic!("flag: {other:?}"),
    }
    assert_eq!(get(&root, "inline").as_array().map(|a| a.len()), Some(3));
    assert_eq!(get(&root, "empty").as_array().map(|a| a.len()), Some(0));
    assert_eq!(
        get(&root, "packed").as_array().map(|a| a.iter().map(to_lexeme).collect::<Vec<_>>()),
        Some(
            ["1", "2", "3", "4", "5"].iter().map(|s| s.to_string()).collect::<Vec<_>>()
        )
    );
    assert_eq!(get(&root, "flags").as_array().map(|a| a.len()), Some(1));
    assert_eq!(
        get(&root, "nested").as_object().and_then(|o| o.get("_class")).and_then(Value::as_str),
        Some("Child")
    );
    assert_eq!(write(&parse(&text).unwrap()), text);

    let blob = document(&["\tdata = ", "#[", "41 42 43 44", "\t]"]);
    let root = parsed_root(&blob);
    assert_eq!(get(&root, "data"), &Value::Blob(vec![0x41, 0x42, 0x43, 0x44]));
    assert_eq!(write(&parse(&blob).unwrap()), blob);

    // A blob longer than one row, in Valve's layout (`#[` and the first row at column 0).
    let many: Vec<u8> = (0..=254u8).collect();
    let mut doc = Document { header: Header::parse(HDR).unwrap(), root: Object::new() };
    let mut thumb = Object::new();
    thumb.insert("bytes".to_string(), Value::Blob(many.clone()));
    doc.root.insert("data".to_string(), Value::Object(thumb));
    let written = write(&doc);
    assert!(written.contains("\tdata = \r\n\t{\r\n\t\tbytes = \r\n#[\r\n00 01 02"));
    assert!(written.contains("\r\n\t\t\t"));
    assert_eq!(
        parse(&written).unwrap().root.get("data").and_then(Value::as_object).and_then(|o| o.get("bytes")),
        Some(&Value::Blob(many))
    );
}

fn to_lexeme(value: &Value) -> String {
    match value {
        Value::Number(n) => n.text.clone(),
        other => panic!("expected a number, got {other:?}"),
    }
}

#[test]
fn array_forms_follow_valve() {
    let two = document(&["\tvector = [ 1.0, 2.0 ]"]);
    assert_eq!(write(&parse(&two).unwrap()), two);
    let three = document(&["\tangle = [ 0.0, 0.0, 0.0 ]"]);
    assert_eq!(write(&parse(&three).unwrap()), three);

    // One element is always a block, four numbers pack one row of four.
    let one = document(&["\tone = ", "\t[", "\t\t7,", "\t]"]);
    assert_eq!(write(&parse(&one).unwrap()), one);
    let four = document(&["\tquaternion = ", "\t[", "\t\t0.0, 0.0, 0.0, 1.0,", "\t]"]);
    assert_eq!(write(&parse(&four).unwrap()), four);
    let six = document(&["\tsix = ", "\t[", "\t\t1, 2, 3, 4,", "\t\t5, 6,", "\t]"]);
    assert_eq!(write(&parse(&six).unwrap()), six);

    // Strings and bools get one element per line, empty containers are inline.
    let strings = document(&["\tnames = ", "\t[", "\t\t\"a\",", "\t\t\"b\",", "\t]"]);
    assert_eq!(write(&parse(&strings).unwrap()), strings);
    let empty = document(&["\tnothing = [  ]", "\tno_object = {  }"]);
    assert_eq!(write(&parse(&empty).unwrap()), empty);

    // Objects as elements are blocks with a trailing comma, including the last one.
    let objects = document(&[
        "\tchildren = ",
        "\t[",
        "\t\t{",
        "\t\t\t_class = \"A\"",
        "\t\t},",
        "\t\t{",
        "\t\t\t_class = \"B\"",
        "\t\t},",
        "\t]",
    ]);
    assert_eq!(write(&parse(&objects).unwrap()), objects);
}

#[test]
fn object_api() {
    let mut object = Object::new();
    assert!(object.is_empty());
    object.insert("a".to_string(), Value::from(1i64));
    object.insert("b".to_string(), Value::from("two"));
    object.insert("c".to_string(), Value::from(3i64));
    assert_eq!(object.len(), 3);
    assert_eq!(object.keys().collect::<Vec<_>>(), vec!["a", "b", "c"]);

    // Replacing keeps the position; removing takes the value out.
    let old = object.insert("b".to_string(), Value::from("TWO"));
    assert_eq!(old, Some(Value::from("two")));
    assert_eq!(object.keys().collect::<Vec<_>>(), vec!["a", "b", "c"]);
    assert_eq!(object.get("b").and_then(Value::as_str), Some("TWO"));
    assert_eq!(object.remove("a"), Some(Value::from(1i64)));
    assert_eq!(object.keys().collect::<Vec<_>>(), vec!["b", "c"]);
    assert_eq!(object.remove("nope"), None);

    if let Some(slot) = object.get_mut("c") {
        *slot = Value::from(4i64);
    }
    assert_eq!(object.get("c"), Some(&Value::from(4i64)));
    assert_eq!(object.iter().map(|(k, _)| k).collect::<Vec<_>>(), vec!["b", "c"]);

    // Keys that are not bare identifiers are quoted on write; dotted keys are not.
    object.insert("dotted.key".to_string(), Value::from(1i64));
    object.insert("spaced key".to_string(), Value::from(1i64));
    object.insert("1lead".to_string(), Value::from(1i64));
    let doc = Document {
        header: Header::parse(HDR).unwrap(),
        root: object,
    };
    let written = write(&doc);
    assert!(written.contains("\tdotted.key = 1\r\n"));
    assert!(written.contains("\t\"spaced key\" = 1\r\n"));
    assert!(written.contains("\t\"1lead\" = 1\r\n"));
    assert!(doc.root.is_quoted("spaced key"));
    assert!(!doc.root.is_quoted("dotted.key"));
}

#[test]
fn value_accessors() {
    let text = document(&[
        "\tobject = ",
        "\t{",
        "\t\t_class = \"X\"",
        "\t}",
        "\tarray = [  ]",
        "\tstring = \"s\"",
        "\tbool = true",
        "\tnumber = 1",
    ]);
    let root = parsed_root(&text);
    assert!(get(&root, "object").as_object().is_some());
    assert!(get(&root, "array").as_array().is_some());
    assert_eq!(get(&root, "string").as_str(), Some("s"));
    assert_eq!(get(&root, "bool").as_bool(), Some(true));
    assert!(get(&root, "number").as_str().is_none());
    assert!(get(&root, "number").as_object().is_none());
    assert!(get(&root, "number").is_scalar());
    assert!(!get(&root, "array").is_scalar());

    let mut doc = parse(&text).unwrap();
    let array = doc.root.get_mut("array").unwrap();
    assert!(array.as_array_mut().map(|a| a.push(Value::from(1i64))).is_some());
    assert!(array.as_object_mut().is_none());
}

#[test]
fn comments_are_skipped_not_preserved() {
    // `parse_only_`: the file has comments, so it cannot round-trip byte for byte.
    let text = include_str!("../fixtures/parse_only_comments.kv3");
    let doc = parse(text).expect("parses");
    assert_eq!(
        doc.root.get("_class").and_then(Value::as_str),
        Some("Commented"),
        "a comment must not swallow the key after it"
    );
    assert_eq!(doc.root.get("value"), Some(&Value::from(1i64)));
    let root = doc.root.get("m_nested").and_then(Value::as_object).expect("m_nested");
    assert_eq!(root.get("inner"), Some(&Value::from(2i64)));
    let written = write(&doc);
    assert!(!written.contains("comment"));
    assert!(!written.contains("/*"));
    assert_eq!(write(&parse(&written).unwrap()), written);
}

#[test]
fn raw_multi_line_strings_are_read() {
    let text = include_str!("../fixtures/parse_only_raw_string.kv3");
    let doc = parse(text).expect("parses");
    assert_eq!(
        doc.root.get("m_text").and_then(Value::as_str),
        Some("first line\nsecond line"),
        "one leading and one trailing line ending are dropped"
    );
    assert_eq!(doc.root.get("m_after"), Some(&Value::from(1i64)));
    // Written back escaped, never as `"""`.
    let written = write(&doc);
    assert!(written.contains("m_text = \"first line\\nsecond line\""));
    assert_eq!(parse(&written).unwrap(), doc);
}

#[test]
fn loose_layout_parses() {
    let text = include_str!("../fixtures/parse_only_loose.kv3");
    let doc = parse(text).expect("parses");
    assert_eq!(doc.root.get("a"), Some(&Value::from(1i64)));
    assert_eq!(doc.root.get("f"), Some(&Value::Null));
    assert_eq!(
        doc.root.get("b").and_then(Value::as_array).map(|a| a.len()),
        Some(3),
        "commas between array elements are optional (686975d acceptance restored, round 2 item 2)"
    );
    let c = doc.root.get("c").and_then(Value::as_object).expect("c");
    assert_eq!(c.len(), 2, "commas between object entries are optional (round 2 item 1)");
    // The restored forms, spelled out so a future tightening does not sneak back in: element
    // commas, entry commas, trailing commas and even a leading comma all parse (686975d's
    // reader accepted all four; upstream only the trailing one).
    let root = parse_kv3_text_root(&[
        "\tone = [ 1 2 ]",
        "\ttwo = [ , 1, ]",
        "\tthree = { d = 2, e = 3, }",
        "\tfour = null,",
    ]);
    assert_eq!(root.get("one").unwrap().as_array().map(|a| a.len()), Some(2));
    assert_eq!(root.get("two").unwrap().as_array().map(|a| a.len()), Some(1));
    assert_eq!(root.get("three").unwrap().as_object().map(Object::len), Some(2));
    assert_eq!(root.get("four").unwrap(), &Value::Null);

    // LF input comes out canonical (Valve layout) and stays parseable.
    let written = write(&doc);
    assert!(written.ends_with('}'));
    assert!(!written.contains('\n') || written.contains("\r\n"));
    assert_eq!(parse(&written).unwrap(), doc);
    // `Style::LF` keeps LF and adds the final newline instead.
    let lf = write_styled(&doc, Style::LF);
    assert!(!lf.contains('\r') && lf.ends_with("}\n"));
    assert_eq!(
        write_styled(&parse(&lf).unwrap(), Style::LF),
        lf,
        "LF output is itself a fixed point"
    );

    let mut canonical = document(&[
        "\trootNode = ",
        "\t{",
        "\t\t_class = \"RootNode\"",
        "\t}",
    ]);
    canonical = canonical.replace("\r\n", "\n");
    let doc = parse(&canonical).expect("LF input parses");
    assert_eq!(
        write_styled(&doc, Style { newline: Newline::Lf, final_newline: false }),
        canonical
    );
}

#[test]
fn write_value_helper() {
    assert_eq!(write_value(&Value::Null, 0), "null");
    assert_eq!(write_value(&Value::from(1i64), 2), "\t\t1");
    assert_eq!(write_value(&Value::from("x"), 1), "\t\"x\"");
    let nested = parse(&document(&[
        "\to = ",
        "\t{",
        "\t\tk = [ 1.0, 2.0 ]",
        "\t}",
    ]))
    .unwrap();
    let object = nested.root.get("o").cloned().expect("o");
    assert_eq!(write_value(&object, 1), "\t{\n\t\tk = [ 1.0, 2.0 ]\n\t}");
}

#[test]
fn duplicate_keys_keep_the_first_position_and_the_last_value() {
    let text = document(&["\ta = 1", "\tb = 2", "\ta = 3"]);
    let root = parsed_root(&text);
    assert_eq!(root.len(), 2);
    assert_eq!(root.keys().collect::<Vec<_>>(), vec!["a", "b"]);
    assert_eq!(root.get("a"), Some(&Value::from(3i64)));
}

#[test]
fn empty_and_whitespace_only_documents_are_errors() {
    assert!(parse("").is_err());
    // With no body after it the file ends before the root object: refused by 686975d too.
    assert!(parse("<!-- kv3 encoding:text:version{x} format:generic:version{y} -->").is_err());
    // A non-GUID `version{x}` is accepted again (686975d behaviour, round 2 item 5); the
    // stricter upstream check stays available as `Header::validate_strict`.
    assert!(parse("<!-- kv3 encoding:text:version{x} format:generic:version{y} -->\r\n{ }").is_ok());
    assert!(ParseError { line: 1, col: 2, msg: "x".into() }.to_string().contains("line 1, column 2"));
}
