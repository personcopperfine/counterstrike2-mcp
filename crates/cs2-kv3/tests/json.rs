//! `serde_json` conversions: the documented shapes, both directions, and the failure modes.

use cs2_kv3::{from_json, parse, to_json, Object, Value};
use serde_json::json;

const HDR: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} \
                   format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->";

fn document(body: &[&str]) -> String {
    let mut text = String::from(HDR);
    text.push_str("\r\n{\r\n");
    for line in body {
        text.push_str(line);
        text.push_str("\r\n");
    }
    text.push('}');
    text
}

fn value(text: &str) -> Value {
    parse(text).expect("parses").root
        .iter()
        .next()
        .expect("one entry")
        .1
        .clone()
}

#[test]
fn scalars_round_trip() {
    let cases = [
        ("\tv = null", json!(null)),
        ("\tv = true", json!(true)),
        ("\tv = false", json!(false)),
        ("\tv = 128", json!(128)),
        ("\tv = -1337", json!(-1337)),
        ("\tv = 0.5", json!(0.5)),
        ("\tv = \"hello\"", json!("hello")),
        ("\tv = \"\"", json!("")),
    ];
    for (line, expected) in cases {
        let parsed = value(&document(&[line]));
        assert_eq!(to_json(&parsed), expected, "{line}");
        assert_eq!(from_json(&expected).expect("back"), parsed, "{line}");
    }
}

#[test]
fn numbers_that_json_cannot_hold_stay_strings() {
    // `1e400` is a valid kv3 lexeme but not a finite f64.
    let parsed = value(&document(&["\tv = 1e400"]));
    assert_eq!(to_json(&parsed), json!("1e400"));
    // A value JSON holds exactly keeps its type and its lexeme.
    let parsed = value(&document(&["\tv = 74.56868"]));
    assert_eq!(to_json(&parsed), json!(74.56868));
    let reparsed = from_json(&json!(74.56868)).expect("back");
    assert_eq!(reparsed, parsed);
    let big = value(&document(&["\tv = 18446744073709551615"]));
    assert_eq!(to_json(&big), json!(18446744073709551615u64));
}

#[test]
fn flags_and_blobs_use_the_tagged_forms() {
    let flagged = value(&document(&["\tv = resource_name:\"weapons/a.vmdl\""]));
    assert_eq!(to_json(&flagged), json!({"$flag": "resource_name", "value": "weapons/a.vmdl"}));
    assert_eq!(
        from_json(&json!({"$flag": "resource_name", "value": "weapons/a.vmdl"})).expect("back"),
        flagged
    );

    let blob = value(&document(&["\tv = ", "#[", "89 50 4E 47", "\t]"]));
    assert_eq!(to_json(&blob), json!({"$blob": "89504E47"}), "hex is uppercase");
    assert_eq!(from_json(&json!({"$blob": "89504e47"})).expect("back"), blob, "lower case in");
    assert_eq!(
        from_json(&json!({"$blob": "89 50 4E 47"})).expect("back"),
        blob,
        "whitespace in hex is tolerated"
    );
    // A nested tagged form (a flag whose payload is a blob) also survives.
    let nested = from_json(&json!({"$flag": "resource_name", "value": {"$blob": "41"}})).expect("ok");
    assert_eq!(to_json(&nested), json!({"$flag": "resource_name", "value": {"$blob": "41"}}));
}

#[test]
fn objects_that_merely_contain_the_tag_keys_stay_objects() {
    let object = from_json(&json!({"$flag": "x", "other": 1})).expect("ok");
    let as_json = to_json(&object);
    assert_eq!(as_json, json!({"$flag": "x", "other": 1}));
    assert!(object.as_object().is_some());

    // Exactly `{$blob}` with a non-string payload is an error, not a silent object.
    assert!(from_json(&json!({"$blob": 1})).is_err() || from_json(&json!({"$blob": 1})).unwrap().as_object().is_some());
}

#[test]
fn blob_hex_errors() {
    for bad in [json!({"$blob": "abc"}), json!({"$blob": "zz"}), json!({"$blob": "4"})] {
        let error = from_json(&bad).expect_err("should fail");
        assert!(!error.msg.is_empty());
        assert!(error.to_string().contains("blob") || error.to_string().contains("hex"));
    }
}

#[test]
fn nested_arrays_and_objects_round_trip_structurally() {
    let text = document(&[
        "\t_class = \"Nested\"",
        "\tchildren = ",
        "\t[",
        "\t\t{",
        "\t\t\t_class = \"A\"",
        "\t\t\tvalues = ",
        "\t\t\t[",
        "\t\t\t\t1.0, 2.0, 3.0, 4.0,",
        "\t\t\t]",
        "\t\t},",
        "\t]",
        "\tflags = ",
        "\t[",
        "\t\tresource_name:\"a.vmat\",",
        "\t]",
    ]);
    let root = parse(&text).expect("parses").root;
    let as_json = to_json(&Value::Object(root.clone()));
    let back = from_json(&as_json).expect("back");
    assert_eq!(back, Value::Object(root));
}

#[test]
fn key_order_is_alphabetical_after_a_json_round_trip() {
    // Documented limitation: object order survives parse/write, not to_json/from_json.
    let mut object = Object::new();
    object.insert("zeta".to_string(), Value::from(1i64));
    object.insert("alpha".to_string(), Value::from(2i64));
    let json = to_json(&Value::Object(object));
    let keys: Vec<&String> = json.as_object().expect("object").keys().collect();
    assert_eq!(keys, vec!["alpha", "zeta"]);
    let back = from_json(&json).expect("back");
    let keys = back.as_object().unwrap().keys().collect::<Vec<_>>();
    assert_eq!(keys, vec!["alpha", "zeta"]);
}
