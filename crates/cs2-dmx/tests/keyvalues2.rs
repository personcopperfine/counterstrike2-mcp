//! The keyvalues2 port's behavioral suite: the exact writer layout, round trips of models
//! carrying every attribute and array type across the encoding versions, the float tokens,
//! the tokenizer quirks upstream's codec keeps, and every malformed shape upstream throws on.
//!
//! Ported from the keyvalues2-relevant parts of upstream `Tests/RoundTripTests.cs` (the
//! MatchesReferenceLayout and FloatFormat tests) and `Tests/Tests.cs` (the Create descendants
//! and ElementHashCollisionsStayDistinct). The reflection, import, typed-element and
//! performance tests have no Rust counterpart here and are not ports — see the report.

use std::collections::HashMap;

use cs2_dmx::codec::{keyvalues2, parse_header};
use cs2_dmx::Element;
use cs2_dmx::{Attribute, Datamodel, ElementId, Guid, Time};

/// Text GUID `00000000-0000-0000-0000-00000000000N`: byte 0 stays zero (it is .NET's LE
/// first text group) and the last raw byte carries the id, matching the reference ids.
fn guid(id: u8) -> Guid {
    let mut bytes = [0u8; 16];
    bytes[15] = id;
    Guid(bytes)
}

fn push_element(dm: &mut Datamodel, element: Element) -> ElementId {
    let id = ElementId(dm.elements.len());
    dm.elements.push(element);
    id
}

/// A named attribute slot on an already-placed element.
fn set_attrs(dm: &mut Datamodel, id: ElementId, attributes: impl IntoIterator<Item = (impl Into<String>, Attribute)>) {
    dm.elements[id.0].attributes = attributes
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect();
}

// ---------------------------------------------------------------------------
// exact layout (upstream RoundTripTests.KeyValues2_MatchesReferenceLayout)
// ---------------------------------------------------------------------------

/// The upstream test's model: a prefix with a string array, a root whose child is inline,
/// whose `shared` is referenced twice (a block after the root), whose element array holds
/// that shared id plus an inline item, plus an empty int array and a null element. The prefix
/// element is a real element at its file position *and* fills `prefix_attributes`, which is
/// what a decode produces (see the module docs of the codec).
fn reference_model() -> Datamodel {
    let mut dm = Datamodel {
        encoding: "keyvalues2".to_string(),
        encoding_version: 4,
        format: "test".to_string(),
        format_version: 1,
        ..Datamodel::default()
    };
    push_element(
        &mut dm,
        element(
            "$prefix_element$",
            "",
            guid(1),
            vec![(
                "refs",
                Attribute::StringArray(vec!["a".to_string(), "b".to_string()]),
            )],
        ),
    );
    let child_id = push_element(
        &mut dm,
        element("DmeChild", "", guid(3), vec![("value", Attribute::Int(1))]),
    );
    let shared_id = push_element(
        &mut dm,
        element("DmeShared", "", guid(4), vec![("flag", Attribute::Bool(true))]),
    );
    let item_id = push_element(&mut dm, element("DmeItem", "", guid(5), Vec::new()));
    let root_id = push_element(&mut dm, element("DmeRoot", "root", guid(2), Vec::new()));
    set_attrs(
        &mut dm,
        root_id,
        vec![
            ("child".to_string(), Attribute::Element(Some(child_id))),
            ("shared".to_string(), Attribute::Element(Some(shared_id))),
            (
                "list".to_string(),
                Attribute::ElementArray(vec![Some(shared_id), Some(item_id)]),
            ),
            ("empty".to_string(), Attribute::IntArray(Vec::new())),
            ("nothing".to_string(), Attribute::Element(None)),
        ],
    );
    dm.root = Some(root_id);
    dm.prefix_attributes = vec![(
        "refs".to_string(),
        Attribute::StringArray(vec!["a".to_string(), "b".to_string()]),
    )];
    dm
}

fn element(class_name: &str, name: &str, id: Guid, attributes: Vec<(&str, Attribute)>) -> Element {
    Element {
        class_name: class_name.to_string(),
        name: name.to_string(),
        id,
        stub: false,
        attributes: attributes
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    }
}

/// The layout, byte for byte, with the pieces annotated the way upstream derives them from
/// `WriteElementBody`, `WriteArrayAttribute` and the blank-line rules around inline elements.
/// This is upstream's expectation, so it is [`keyvalues2::Layout::Upstream`] that writes it.
const REFERENCE_LAYOUT: &str = concat!(
    "<!-- dmx encoding keyvalues2 4 format test 1 -->\n",
    // the prefix block, with the prefix element's own GUID
    "\"$prefix_element$\"\n",
    "{\n",
        "\t\"id\" \"elementid\" \"00000000-0000-0000-0000-000000000001\"\n",
        "\t\"refs\" \"string_array\" \n",
        "\t[\n",
            "\t\t\"a\",\n",
            "\t\t\"b\"\n",
        "\t]\n",
    "}\n",
    "\"DmeRoot\"\n",
    "{\n",
        "\t\"id\" \"elementid\" \"00000000-0000-0000-0000-000000000002\"\n",
        "\t\"name\" \"string\" \"root\"\n",
        "\t\"child\" \"DmeChild\"\n", // inline: referenced once
        "\t{\n",
            // quoting the nested indentation as concat lines keeps the tabs literal
        "\t\t\"id\" \"elementid\" \"00000000-0000-0000-0000-000000000003\"\n",
        "\t\t\"value\" \"int\" \"1\"\n",
        "\t}\n",
        "\n", // blank line after an inline element attribute
        "\t\"shared\" \"element\" \"00000000-0000-0000-0000-000000000004\"\n", // twice → id ref
        "\t\"list\" \"element_array\" \n", // trailing space after the type
        "\t[\n",
            "\t\t\"element\" \"00000000-0000-0000-0000-000000000004\",\n",
            "\t\t\"DmeItem\"\n", // inline; array items take no blank line after them
            "\t\t{\n",
                "\t\t\t\"id\" \"elementid\" \"00000000-0000-0000-0000-000000000005\"\n",
            "\t\t}\n",
        "\t]\n",
        "\t\"empty\" \"int_array\" \n",
        "\t[\n",
        "\t]\n",
        "\t\"nothing\" \"element\" \"\"\n",
    "}\n",
    "\n", // blank line after the root
    "\"DmeShared\"\n", // the twice-referenced block, first-seen order after the root
    "{\n",
        "\t\"id\" \"elementid\" \"00000000-0000-0000-0000-000000000004\"\n",
        "\t\"flag\" \"bool\" \"1\"\n",
    "}\n",
    "\n",
);

#[test]
fn reference_layout() {
    let bytes = keyvalues2::encode_with(&reference_model(), keyvalues2::Layout::Upstream).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert_eq!(text, REFERENCE_LAYOUT, "the writer must lay this out like upstream, byte for byte");
}

/// The same model through the default (Valve) layout: with one multiply-referenced element, the
/// two layouts agree, so this pins that they do not drift apart. `crate::save` goes through the
/// same call.
#[test]
fn reference_layout_valve() {
    let bytes = keyvalues2::encode(&reference_model()).unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert_eq!(text, REFERENCE_LAYOUT);
    assert_eq!(cs2_dmx::save(&reference_model()).unwrap(), bytes);
}

/// Re-decoding must give back the same elements, names, attributes and prefix, in the walk
/// order, and re-encoding must reproduce the bytes.
#[test]
fn reference_layout_round_trip() {
    for layout in [keyvalues2::Layout::Valve, keyvalues2::Layout::Upstream] {
        let original = reference_model();
        let bytes = keyvalues2::encode_with(&original, layout).unwrap();
        let (header, rest_offset) = parse_header(&bytes).unwrap();
        let decoded = keyvalues2::decode(&header, &bytes[rest_offset..]).unwrap();
        assert_same(&original, &decoded);
        assert_eq!(keyvalues2::encode_with(&decoded, layout).unwrap(), bytes, "{layout:?}");
    }
}

/// Two datamodels are equivalent when they carry the same root (by GUID), the same prefix
/// attributes and the same set of elements by GUID — each with the same class, name and
/// attributes in order; floats compare with upstream's `1e-9` round-trip tolerance.
fn assert_same(expected: &Datamodel, actual: &Datamodel) {
    let expected_map: HashMap<Guid, &Element> =
        expected.elements.iter().map(|e| (e.id, e)).collect();
    let actual_map: HashMap<Guid, &Element> = actual.elements.iter().map(|e| (e.id, e)).collect();
    assert_eq!(expected_map.len(), actual_map.len(), "element counts differ");
    let expected_root = expected.root.map(|id| expected.elements[id.0].id);
    let actual_root = actual.root.map(|id| actual.elements[id.0].id);
    assert_eq!(expected_root, actual_root, "roots differ");
    assert_eq!(expected.prefix_attributes, actual.prefix_attributes, "prefix attrs differ");
    let expected_ids: HashMap<ElementId, Guid> =
        expected.elements.iter().enumerate().map(|(i, e)| (ElementId(i), e.id)).collect();
    let actual_ids: HashMap<ElementId, Guid> =
        actual.elements.iter().enumerate().map(|(i, e)| (ElementId(i), e.id)).collect();
    for (id, expected_element) in expected_map {
        let actual_element = actual_map
            .get(&id)
            .expect("actual must hold every element of expected");
        assert_eq!(expected_element.class_name, actual_element.class_name, "class of {id:?}");
        assert_eq!(expected_element.name, actual_element.name, "name of {id:?}");
        assert_eq!(expected_element.attributes.len(), actual_element.attributes.len(), "attr count of {id:?}");
        for (index, (name, value)) in expected_element.attributes.iter().enumerate() {
            let (actual_name, actual_value) = &actual_element.attributes[index];
            assert_eq!(name, actual_name, "attr name of {id:?} #{index}");
            assert_value_equals(value, actual_value, &format!("{id:?}.{name}"), &expected_ids, &actual_ids);
        }
    }
}

/// Attribute equality with the float tolerance upstream's round trip needs (text keeps ten
/// decimals, so sub-1e-9 float drift is fine); element references compare through the
/// side's own id→GUID map, because the walk order of a decoded arena is not the build order.
fn assert_value_equals(
    expected: &Attribute,
    actual: &Attribute,
    context: &str,
    expected_ids: &HashMap<ElementId, Guid>,
    actual_ids: &HashMap<ElementId, Guid>,
) {
    let element_ref = |id: &Option<ElementId>, map: &HashMap<ElementId, Guid>| -> Option<Guid> {
        id.map(|id| map[&id])
    };
    let elem_refs = |items: &Vec<Option<ElementId>>, map: &HashMap<ElementId, Guid>| -> Vec<Option<Guid>> {
        items.iter().map(|item| element_ref(item, map)).collect()
    };
    match (expected, actual) {
        (Attribute::Element(a), Attribute::Element(b)) => {
            assert_eq!(element_ref(a, expected_ids), element_ref(b, actual_ids), "{context}");
        }
        (Attribute::ElementArray(a), Attribute::ElementArray(b)) => {
            assert_eq!(elem_refs(a, expected_ids), elem_refs(b, actual_ids), "{context}");
        }
        (Attribute::Float(a), Attribute::Float(b)) => {
            assert!((a - b).abs() <= 1e-9, "{context}: {a} vs {b}");
        }
        (Attribute::Time(a), Attribute::Time(b)) => {
            assert!((a.0 - b.0).abs() <= 1e-7, "{context}: {:?} vs {:?}", a, b);
        }
        (Attribute::Vector2(a), Attribute::Vector2(b)) => {
            assert!(components_close(a, b), "{context}: {a:?} vs {b:?}");
        }
        (Attribute::Vector3(a), Attribute::Vector3(b)) => {
            assert!(components_close(a, b), "{context}: {a:?} vs {b:?}");
        }
        (Attribute::Vector4(a), Attribute::Vector4(b)) => {
            assert!(components_close(a, b), "{context}: {a:?} vs {b:?}");
        }
        (Attribute::QAngle(a), Attribute::QAngle(b)) => {
            assert!(components_close(a, b), "{context}: {a:?} vs {b:?}");
        }
        (Attribute::Quaternion(a), Attribute::Quaternion(b)) => {
            assert!(components_close(a, b), "{context}: {a:?} vs {b:?}");
        }
        (Attribute::Matrix(a), Attribute::Matrix(b)) => {
            for (row_a, row_b) in a.iter().zip(b.iter()) {
                assert!(components_close(row_a, row_b), "{context}: {a:?} vs {b:?}");
            }
        }
        (Attribute::FloatArray(a), Attribute::FloatArray(b)) => {
            assert_eq!(a.len(), b.len(), "{context} count");
            for (x, y) in a.iter().zip(b.iter()) {
                assert!((x - y).abs() <= 1e-9, "{context}: {x} vs {y}");
            }
        }
        (a, b) => {
            assert_eq!(a, b, "{context}");
        }
    }
}

fn components_close<const N: usize>(a: &[f32; N], b: &[f32; N]) -> bool {
    a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() <= 1e-9)
}

// ---------------------------------------------------------------------------
// full models through v1 / v4 / noids (upstream Tests.cs `Create` descendants)
// ---------------------------------------------------------------------------

/// The synthetic model upstream builds: one of every scalar (twice each: scalar + an array
/// of two), a recursion, an unnamed child, an element array (a stub, an unnamed element, the
/// root itself, a named element), a stub-reference, and a null element. The extras (like the
/// version-gated uint8/uint64/qangle) are the caller's.
fn full_model(encoding: &str, version: i32, extras: Vec<(&str, Attribute)>) -> Datamodel {
    let mut dm = Datamodel {
        encoding: encoding.to_string(),
        encoding_version: version,
        format: "model".to_string(),
        format_version: 1,
        ..Datamodel::default()
    };
    let blob = vec![18u8, 255, 3, 140, 200, 1, 0, 77];
    let blob2 = vec![9u8, 12, 200, 7, 3, 1, 0, 44];
    // .NET Quaternion.Normalize(new Quaternion(1, 2, 3, 4)), evaluated with the same IEEE ops
    let quat = {
        let v = [1.0f32, 2.0, 3.0, 4.0];
        let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2] + v[3] * v[3]).sqrt();
        let norm = 1.0 / length;
        [v[0] * norm, v[1] * norm, v[2] * norm, v[3] * norm]
    };
    let quat2 = [0.0f32, -1.0, 0.5, 0.5];

    // push children before the root so refs can point at them by arena id
    let unnamed_id = push_element(&mut dm, element("DmeElement", "", guid(3), Vec::new()));
    let stub_id = push_element(&mut dm, Element {
        class_name: "DmeElement".to_string(),
        name: "Stub element".to_string(),
        id: guid(4),
        stub: true,
        attributes: Vec::new(),
    });
    // upstream Populate also stubs an element outright (ElementStub = new Element(dm, Guid)),
    // which the writer inlines because nothing references it a second time
    let stub_extra_id = push_element(&mut dm, Element {
        class_name: "DmeElement".to_string(),
        name: "Stub element".to_string(),
        id: guid(6),
        stub: true,
        attributes: Vec::new(),
    });
    let named_id = push_element(&mut dm, element("DmeElement", "TestElement", guid(5), Vec::new()));
    let root_id = push_element(&mut dm, element("DmeElement", "root", guid(2), Vec::new()));

    set_attrs(&mut dm, root_id, {
        let mut attrs: Vec<(&str, Attribute)> = vec![
            ("string_attr", Attribute::String("hello_world".to_string())),
            (
                "string_attr_array",
                Attribute::StringArray(vec!["hello_world".to_string(), "hello_world".to_string()]),
            ),
            ("int_attr", Attribute::Int(1)),
            ("int_attr_array", Attribute::IntArray(vec![1, 1])),
            ("float_attr", Attribute::Float(1.5)),
            ("float_attr_array", Attribute::FloatArray(vec![1.5, 1.5])),
            ("bool_attr", Attribute::Bool(true)),
            ("bool_attr_array", Attribute::BoolArray(vec![true, false])),
            ("binary_attr", Attribute::Binary(blob.clone())),
            ("binary_attr_array", Attribute::BinaryArray(vec![blob.clone(), blob2.clone()])),
            ("color_attr", Attribute::Color([1, 255, 2, 244])),
            ("color_attr_array", Attribute::ColorArray(vec![[1, 255, 2, 244], [0, 9, 18, 27]])),
            ("vector2_attr", Attribute::Vector2([1.0, 2.0])),
            ("vector2_attr_array", Attribute::Vector2Array(vec![[1.0, 2.0], [3.5, 4.25]])),
            ("vector3_attr", Attribute::Vector3([1.0, 2.0, 3.0])),
            ("vector3_attr_array", Attribute::Vector3Array(vec![[1.0, 2.0, 3.0], [-1.0, 0.5, 0.25]])),
            ("vector4_attr", Attribute::Vector4([1.0, 2.0, 3.0, 4.0])),
            (
                "vector4_attr_array",
                Attribute::Vector4Array(vec![[1.0, 2.0, 3.0, 4.0], [-4.0, -3.0, -2.0, -1.0]]),
            ),
            ("quaternion_attr", Attribute::Quaternion(quat)),
            (
                "quaternion_attr_array",
                Attribute::QuaternionArray(vec![quat, quat2]),
            ),
            (
                "matrix_attr",
                Attribute::Matrix([
                    [1.0, 2.0, 3.0, 4.0],
                    [5.0, 6.0, 7.0, 8.0],
                    [9.0, 10.0, 11.0, 12.0],
                    [13.0, 14.0, 15.0, 16.0],
                ]),
            ),
            (
                "matrix_attr_array",
                Attribute::MatrixArray(vec![
                    [
                        [1.0, 2.0, 3.0, 4.0],
                        [5.0, 6.0, 7.0, 8.0],
                        [9.0, 10.0, 11.0, 12.0],
                        [13.0, 14.0, 15.0, 16.0],
                    ],
                    [
                        [-1.0, -2.0, -3.0, -4.0],
                        [-5.0, -6.0, -7.0, -8.0],
                        [-9.0, -10.0, -11.0, -12.0],
                        [-13.0, -14.0, -15.0, -16.0],
                    ],
                ]),
            ),
            ("time_attr", Attribute::Time(Time(0.25))),
            ("time_attr_array", Attribute::TimeArray(vec![Time(0.25), Time(-0.5)])),
        ];
        attrs.push(("no_name_properly", Attribute::Element(Some(unnamed_id))));
        attrs.push((
            "elem_array",
            Attribute::ElementArray(vec![
                Some(stub_id),
                Some(unnamed_id),
                Some(root_id),
                Some(named_id),
            ]),
        ));
        attrs.push(("element_stub", Attribute::Element(Some(stub_extra_id))));
        attrs.push(("recursive", Attribute::Element(Some(root_id))));
        attrs.push(("nothing", Attribute::Element(None)));
        for (name, value) in extras {
            attrs.push((name, value));
        }
        attrs
    });
    dm.root = Some(root_id);
    dm
}

/// The upstream-permitted scalars and arrays of keyvalues2 1 (not reminding the port's own
/// extension types — uint8/uint64/qangle are version 4), through a full encode/decode/encode.
#[test]
fn create_keyvalues2_1() {
    let dm = full_model("keyvalues2", 1, Vec::new());
    round_trip_entirely(&dm);
}

/// The same through keyvalues2 4, plus the types that only exist there.
#[test]
fn create_keyvalues2_4() {
    let dm = full_model(
        "keyvalues2",
        4,
        vec![
            ("uint8_attr", Attribute::Byte(255)),
            ("uint64_attr", Attribute::UInt64(0xFFFF_FFFF)),
            ("qangle_attr", Attribute::QAngle([0.0, 90.0, 180.0])),
        ],
    );
    round_trip_entirely(&dm);
}

/// decode/encode/decode/encode with the byte-identity of the second write, and the elements'
/// equivalence, walking the model the upstream round trip walks.
fn round_trip_entirely(dm: &Datamodel) {
    let bytes = keyvalues2::encode(dm).unwrap();
    let (header, offset) = parse_header(&bytes).unwrap();
    let decoded = keyvalues2::decode(&header, &bytes[offset..]).unwrap();
    let bytes2 = keyvalues2::encode(&decoded).unwrap();
    assert_eq!(bytes2, bytes, "re-encoding the decoded model must reproduce the bytes");
    assert_same(dm, &decoded);
}

// ---------------------------------------------------------------------------
// float tokens (upstream RoundTripTests.KeyValues2_FloatFormat, plus a token table)
// ---------------------------------------------------------------------------

/// A root element whose attributes are the given (name, float) pairs; the encoded text lines
/// are what gets asserted.
fn float_tokens_model(pairs: Vec<(&str, f32)>) -> Datamodel {
    let mut dm = Datamodel {
        encoding: "keyvalues2".to_string(),
        encoding_version: 4,
        format: "test".to_string(),
        format_version: 1,
        ..Datamodel::default()
    };
    let root_id = push_element(&mut dm, element("DmeElement", "root", Guid::default(), Vec::new()));
    set_attrs(&mut dm, root_id, {
        pairs.into_iter().map(|(name, value)| {
            (name, Attribute::Float(value))
        }).collect::<Vec<(&str, Attribute)>>()
    });
    dm.root = Some(root_id);
    dm
}

/// Upstream's `FormatFloat` is `((double)value).ToString("0.##########", InvariantCulture)`:
/// 15 significant digits, then ten decimals at most, trailing zeros dropped. These are the
/// numbers upstream's own test asserts; `float_text_matches_the_dotnet_table` checks the rule
/// against .NET 10 for thousands of values.
#[test]
fn float_format_matches_upstream() {
    let dm = float_tokens_model(vec![
        ("-270.11304", -270.11304),
        ("-233.07538", -233.07538),
        ("562.09106", 562.09106),
        ("whole", 40.0),
        ("negative", -1.0),
    ]);
    let text = String::from_utf8(keyvalues2::encode(&dm).unwrap()).unwrap();
    // the exact lines upstream's test asserts (its Vector3 becomes three float lines here)
    assert!(text.contains("\"-270.1130371094\""), "missing -270.1130371094 in:\n{text}");
    assert!(text.contains("\"-233.075378418\""), "{text}");
    assert!(text.contains("\"562.0910644531\""), "{text}");
    assert!(text.contains("\"40\""), "{text}");
    assert!(text.contains("\"-1\""), "{text}");
}

/// The tricky-value token table, values verified against .NET 10: tiny values that round to
/// zero, the tie at the tenth decimal (.NET rounds it up), a huge value that keeps only 15
/// significant digits, and the non-finite spellings.
#[test]
fn float_token_table() {
    let cases: Vec<(&str, f32, &str)> = vec![
        ("int_f", 40.0, "40"),
        ("neg_int_f", -1.0, "-1"),
        ("one_sixteenth", 0.0625, "0.0625"),
        ("tenth", 0.1, "0.1000000015"),
        ("micro", 1e-7, "0.0000001"),
        ("tiny", 1e-30, "0"),
        ("subnormal_min", 1e-45, "0"),
        ("negative_zero", -0.0, "-0"),
        ("root_of_half", 0.5, "0.5"),
        ("thousand", 123456789.0, "123456792"),
        ("half_max", 65504.0, "65504"),
        ("near_one", 0.9999999999, "1"),
        ("two_and_half", 2.5, "2.5"),
        ("c30", 0.3, "0.3000000119"),
        ("c15", 0.015, "0.0149999997"),
        ("c4", 0.0001, "0.0001"),
        ("hundren_pico", 1e-10, "0.0000000001"),
        ("neg_hundren_pico", -1e-10, "-0.0000000001"),
        ("c05", 1.05, "1.0499999523"),
        ("c596", 5.96e-8, "0.0000000596"),
        ("int_float_denom_tie", 0.00048828125, "0.0004882813"), // 2^-11: a tie at the tenth decimal, .NET rounds it up
        ("huge_coord", 3.4e38, "339999995214436000000000000000000000000"), // 15 significant digits, zero padded
        ("float_nan", f32::NAN, "NaN"),
        ("float_pos_inf", f32::INFINITY, "Infinity"),
        ("float_neg_inf", -f32::INFINITY, "-Infinity"),
    ];
    let model = float_tokens_model(
        cases.iter().map(|(name, value, _)| (*name, *value)).collect()
    );
    let text = String::from_utf8(keyvalues2::encode(&model).unwrap()).unwrap();
    for (name, _, token) in &cases {
        let line = format!("\"{name}\" \"float\" \"{token}\"");
        assert!(text.contains(&line), "expected the line {line:?} in:\n{text}");
    }
}

// ---------------------------------------------------------------------------
// tokenizer keeps upstream's hand rules (`Decode_NextToken`); round trips of escapes
// ---------------------------------------------------------------------------

fn encode_string_value(value: &str) -> String {
    let mut dm = Datamodel {
        encoding: "keyvalues2".to_string(),
        encoding_version: 1,
        format: "test".to_string(),
        format_version: 1,
        ..Datamodel::default()
    };
    let root_id = push_element(&mut dm, element("E", "", Guid::default(), Vec::new()));
    set_attrs(&mut dm, root_id, vec![("text", Attribute::String(value.to_string()))]);
    dm.root = Some(root_id);
    String::from_utf8(keyvalues2::encode(&dm).unwrap()).unwrap()
}

fn lone_root_datamodel() -> Datamodel {
    let mut dm = Datamodel {
        encoding: "keyvalues2".to_string(),
        encoding_version: 1,
        format: "test".to_string(),
        format_version: 1,
        ..Datamodel::default()
    };
    let root_id = push_element(&mut dm, element("E", "", Guid::default(), Vec::new()));
    dm.root = Some(root_id);
    dm
}

fn decode_bytes(bytes: &[u8]) -> Result<Datamodel, cs2_dmx::Error> {
    let (header, offset) = parse_header(bytes).unwrap();
    keyvalues2::decode(&header, &bytes[offset..])
}

/// Every sanitise turn-around: backslash, quotes, newlines, tabs, real tab characters and a
/// non-ASCII round trip; decode then gives back exactly the string.
#[test]
fn string_escapes_round_trip() {
    let tricky = String::from("back\\slash \"quote\" newline\n carriage\r\tliteral anda émoji ↔");
    let text = encode_string_value(&tricky);
    let (header, offset) = parse_header(text.as_bytes()).unwrap();
    let decoded = keyvalues2::decode(&header, &text.as_bytes()[offset..]).unwrap();
    let Attribute::String(got) = &decoded.elements[0].attributes[0].1 else { panic!() };
    assert_eq!(got, &tricky);
}

/// A blank body decodes to a model with no elements; the upstream loop simply finds no
/// tokens at its EOF. A body that shares the header's line is on the line upstream's
/// `ReadLine` skips, so it is lost the same way.
#[test]
fn empty_body_decodes_to_no_elements() {
    let mut bytes = keyvalues2::encode(&lone_root_datamodel()).unwrap();
    // drop everything past the trailing \n\u{...} — keep the header and the newline
    while bytes.last() == Some(&b'\n') { bytes.pop(); }
    bytes.truncate(bytes.iter().position(|&b| b == b'\n').unwrap() + 1);
    let decoded = decode_bytes(&bytes).unwrap();
    assert!(decoded.elements.is_empty());
    assert!(decoded.root.is_none());

    // a body that shares the header's line is on the line upstream's `ReadLine` skips, so the
    // class token is gone and the stray `{` is refused, exactly as in C#
    let same_line = "<!-- dmx encoding keyvalues2 1 format test 1 -->\"E\"\n{\n}\n\n";
    let error = decode_bytes(same_line.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("Expected Element opener, got '}'"), "{error}");
}

/// Upstream skips every character outside quotes except `{ } [ ]`: bare words injected
/// anywhere do not change what the codec reads (whose flow reads only quoted tokens).
#[test]
fn bare_words_outside_quotes_are_skipped() {
    let clean = encode_string_value("hello");
    let (_, offset) = parse_header(clean.as_bytes()).unwrap();
    let mut junky = clean.as_bytes()[..offset].to_vec();
    junky.extend_from_slice(b"junk stuff < > & , ;\njunk\n");
    for byte in &clean.as_bytes()[offset..] {
        junky.push(*byte);
    }
    let decoded = decode_bytes(&junky).unwrap();
    let Attribute::String(got) = &decoded.elements[0].attributes[0].1 else { panic!() };
    assert_eq!(got, "hello");
}

// ---------------------------------------------------------------------------
// version and type-table rules
// ---------------------------------------------------------------------------

/// The types upper-listed in upstream `ValidAttributes` cannot arrive before version 4; the
/// writer says so explicitly. keyvalues2 1's one line then vanishes like upstream's — the
/// three extension types print an error instead.
#[test]
fn version_gated_types_reject_on_v1() {
    for (name, attr) in [
        ("uint8", Attribute::Byte(255)),
        ("uint64", Attribute::UInt64(1)),
        ("qangle", Attribute::QAngle([90.0, 45.0, 0.0])),
        ("uint8_arr", Attribute::ByteArray(vec![1, 2])),
        ("uint64_arr", Attribute::UInt64Array(vec![1, 2])),
        ("qangle_arr", Attribute::QAngleArray(vec![[0.0, 90.0, 180.0]])),
    ] {
        let mut dm = lone_root_datamodel();
        let root = dm.root.unwrap();
        set_attrs(&mut dm, root, vec![(name, attr)]);
        let error = keyvalues2::encode(&dm).unwrap_err();
        let error_message = error.to_string();
        assert!(
            error_message.contains("not valid in KeyValues2"),
            "{name}: {error_message}"
        );
    }
    // in version 4 they encode (create_keyvalues2_4 carries them)
}

/// Class names that collide with type names are refused, mirroring
/// `WriteElementBody`'s reserved-type check.
#[test]
fn reserved_class_names_refuse() {
    for class_name in ["int", "element", "qangle", "uint64"] {
        let mut dm = lone_root_datamodel();
        dm.elements[dm.root.unwrap().0].class_name = class_name.to_string();
        let error = keyvalues2::encode(&dm).unwrap_err();
        assert!(error.to_string().contains("reserved type name"), "{class_name}: {error}");
    }
}


// ---------------------------------------------------------------------------
// the two layouts (`Layout::Valve` is the default, `Layout::Upstream` is the C# writer)
// ---------------------------------------------------------------------------

/// A keyvalues2 4 file with the given body text after the header line.
fn file(body: &str) -> String {
    format!("<!-- dmx encoding keyvalues2 4 format test 1 -->\n{body}")
}

/// A one-element model with the encoding version and class a test wants.
fn single_element_model(version: i32, class_name: &str) -> Datamodel {
    let mut dm = Datamodel {
        encoding: "keyvalues2".to_string(),
        encoding_version: version,
        format: "test".to_string(),
        format_version: 1,
        ..Datamodel::default()
    };
    let root = push_element(&mut dm, element(class_name, "", Guid::default(), Vec::new()));
    dm.root = Some(root);
    dm
}

/// The `D` text form of a GUID, the way the writer prints ids.
fn guid_text(guid: &Guid) -> String {
    let bytes = guid.0;
    format!(
        "{:08x}-{:04x}-{:04x}-{}-{}",
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_le_bytes([bytes[4], bytes[5]]),
        u16::from_le_bytes([bytes[6], bytes[7]]),
        bytes[8..10].iter().map(|b| format!("{b:02x}")).collect::<String>(),
        bytes[10..16].iter().map(|b| format!("{b:02x}")).collect::<String>(),
    )
}

/// A root whose two children are each referenced twice: the walk from the root meets `B` first
/// (its attribute comes first), the arena holds `A` first (it was built first). Valve's writer
/// follows the arena, upstream's the walk, so this is the model that tells the two apart.
fn order_probe_model() -> Datamodel {
    let mut dm = lone_root_datamodel();
    let a = push_element(&mut dm, element("DmeA", "", guid(10), Vec::new()));
    let b = push_element(&mut dm, element("DmeB", "", guid(11), Vec::new()));
    let root = dm.root.unwrap();
    set_attrs(
        &mut dm,
        root,
        vec![
            ("b", Attribute::Element(Some(b))),
            ("a", Attribute::Element(Some(a))),
            ("b_again", Attribute::Element(Some(b))),
            ("a_again", Attribute::Element(Some(a))),
        ],
    );
    dm
}

/// The class-name lines of the blocks at column 0, in the order they are written.
fn top_level_blocks(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.starts_with('"'))
        .map(|line| line.to_string())
        .collect()
}

#[test]
fn layouts_write_after_root_blocks_in_their_own_order() {
    let dm = order_probe_model();
    let valve = String::from_utf8(keyvalues2::encode_with(&dm, keyvalues2::Layout::Valve).unwrap())
        .unwrap();
    assert_eq!(
        top_level_blocks(&valve),
        ["\"E\"", "\"DmeA\"", "\"DmeB\""],
        "Valve's layout follows the element list:\n{valve}"
    );
    let upstream =
        String::from_utf8(keyvalues2::encode_with(&dm, keyvalues2::Layout::Upstream).unwrap())
            .unwrap();
    assert_eq!(
        top_level_blocks(&upstream),
        ["\"E\"", "\"DmeB\"", "\"DmeA\""],
        "upstream's layout follows its reference walk:\n{upstream}"
    );
    // `encode` is the default, and that is the Valve layout
    assert_eq!(String::from_utf8(keyvalues2::encode(&dm).unwrap()).unwrap(), valve);
}

/// A `binary` value longer than one line of hex: exact text in both layouts.
#[test]
fn binary_wrapping_is_the_valve_layout() {
    let long: Vec<u8> = (0u8..41).collect();
    let short: Vec<u8> = (0u8..40).collect();
    let long_hex: String = long.iter().map(|byte| format!("{byte:02X}")).collect();
    let short_hex: String = short.iter().map(|byte| format!("{byte:02X}")).collect();
    let mut dm = single_element_model(4, "E");
    let root = dm.root.unwrap();
    set_attrs(
        &mut dm,
        root,
        vec![
            ("long", Attribute::Binary(long.clone())),
            ("short", Attribute::Binary(short.clone())),
        ],
    );

    let expected_valve = file(&format!(
        "\"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\
         \t\"long\" \"binary\" \n\t\"\n\t\t{}\n\t\t{}\n\t\"\n\
         \t\"short\" \"binary\" \"{short_hex}\"\n}}\n\n",
        guid_text(&Guid::default()),
        &long_hex[..80],
        &long_hex[80..],
    ));
    assert_eq!(String::from_utf8(keyvalues2::encode(&dm).unwrap()).unwrap(), expected_valve);
    // the wrapped token still decodes to the same bytes: the line breaks and tabs are inside
    // the quotes, and the reference reader strips them off a binary value
    let decoded = decode_bytes(expected_valve.as_bytes()).unwrap();
    assert_eq!(decoded.elements[0].get("long"), Some(&Attribute::Binary(long)));
    assert_eq!(decoded.elements[0].get("short"), Some(&Attribute::Binary(short)));

    let upstream =
        String::from_utf8(keyvalues2::encode_with(&dm, keyvalues2::Layout::Upstream).unwrap())
            .unwrap();
    assert!(upstream.contains(&format!("\"long\" \"binary\" \"{long_hex}\"")), "{upstream}");
}

// ---------------------------------------------------------------------------
// the `$prefix_element$` block
// ---------------------------------------------------------------------------

/// A decoded prefix block is a real element in `elements` at its file position (first here),
/// carrying the block's GUID, and its attributes mirror `prefix_attributes`; the root is the
/// element after it, and re-encoding writes the block exactly once.
#[test]
fn prefix_element_is_decoded_beside_its_attributes() {
    let text = String::from_utf8(keyvalues2::encode(&reference_model()).unwrap()).unwrap();
    let decoded = decode_bytes(text.as_bytes()).unwrap();

    let prefix = &decoded.elements[0];
    assert_eq!(prefix.class_name, keyvalues2::PREFIX_ELEMENT_CLASS);
    assert_eq!(prefix.id, guid(1));
    assert_eq!(prefix.name, "");
    assert_eq!(prefix.attributes, decoded.prefix_attributes);
    assert_eq!(
        decoded.root.map(|id| decoded.elements[id.0].class_name.clone()),
        Some("DmeRoot".to_string()),
        "the prefix element never becomes the root"
    );
    assert_eq!(text.matches("\"$prefix_element$\"").count(), 1, "written once, not twice");
    assert_eq!(keyvalues2::encode(&decoded).unwrap(), text.as_bytes());
}

/// A prefix block with nothing but its id line: upstream's writer drops it (no attributes to
/// write), the port keeps the element and writes the block back.
#[test]
fn prefix_block_without_attributes_survives_the_round_trip() {
    let text = file(&format!(
        "\"$prefix_element$\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n}}\n\
         \"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n}}\n\n",
        guid_text(&guid(9)),
        guid_text(&guid(10)),
    ));
    let decoded = decode_bytes(text.as_bytes()).unwrap();
    assert_eq!(decoded.elements.len(), 2);
    assert_eq!(decoded.elements[0].class_name, keyvalues2::PREFIX_ELEMENT_CLASS);
    assert_eq!(decoded.elements[0].id, guid(9));
    assert!(decoded.prefix_attributes.is_empty());
    assert_eq!(decoded.root, Some(ElementId(1)));
    assert_eq!(String::from_utf8(keyvalues2::encode(&decoded).unwrap()).unwrap(), text);
}

/// A model built in code with prefix attributes but no prefix element still writes the block;
/// the id line is all zeroes because the frozen `Datamodel` has no `PrefixElementId` field
/// (upstream writes a fresh random GUID there). Decoding that output gives the port's shape
/// back: an element with that zero GUID and the attributes mirrored.
#[test]
fn prefix_attributes_alone_write_the_block_under_the_zero_guid() {
    let mut dm = single_element_model(4, "E");
    dm.prefix_attributes =
        vec![("thumbnail_format".to_string(), Attribute::String("jpg".to_string()))];
    let text = String::from_utf8(keyvalues2::encode(&dm).unwrap()).unwrap();
    assert!(text.contains(&format!("\"id\" \"elementid\" \"{}\"", guid_text(&Guid::default()))));

    let decoded = decode_bytes(text.as_bytes()).unwrap();
    let prefix = &decoded.elements[0];
    assert_eq!(prefix.class_name, keyvalues2::PREFIX_ELEMENT_CLASS);
    assert_eq!(prefix.id, Guid::default());
    assert_eq!(
        decoded.prefix_attributes,
        vec![("thumbnail_format".to_string(), Attribute::String("jpg".to_string()))]
    );
    assert_eq!(decoded.elements.len(), 2);
}

/// An element reference inside a prefix block: upstream's fold has no element to attach one to,
/// so a non-empty GUID is an error (its `HandleElementProp` over a null element) and an empty
/// one is dropped without a trace.
#[test]
fn element_references_in_a_prefix_block_are_refused() {
    let text = file(&format!(
        "\"$prefix_element$\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\t\"ref\" \"element\" \"{}\"\n}}\n",
        guid_text(&guid(9)),
        guid_text(&guid(10)),
    ));
    let error = decode_bytes(text.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("no element to attach to"), "{error}");

    let empty = file(&format!(
        "\"$prefix_element$\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\t\"ref\" \"element\" \"\"\n}}\n",
        guid_text(&guid(9)),
    ));
    let decoded = decode_bytes(empty.as_bytes()).unwrap();
    assert!(decoded.prefix_attributes.is_empty(), "an empty reference is dropped");
    assert!(decoded.elements[0].attributes.is_empty());
}

/// A model built in code that puts a *defined* element into `prefix_attributes` (upstream's
/// `AttributeList` refuses that outright) is an `Err` rather than a reference the prefix block
/// cannot hold. A null one is as fine as it is upstream, where the setter only checks for an
/// `Element` instance.
#[test]
fn a_defined_element_as_a_prefix_attribute_is_refused_on_encode() {
    let mut dm = single_element_model(4, "E");
    let root = dm.root.unwrap();
    dm.prefix_attributes = vec![("ref".to_string(), Attribute::Element(Some(root)))];
    let error = keyvalues2::encode(&dm).unwrap_err();
    assert!(error.to_string().contains("not supported as prefix attributes"), "{error}");

    dm.prefix_attributes = vec![("ref".to_string(), Attribute::Element(None))];
    let text = String::from_utf8(keyvalues2::encode(&dm).unwrap()).unwrap();
    assert!(text.contains("\t\"ref\" \"element\" \"\"\n"), "{text}");
}

// ---------------------------------------------------------------------------
// noids
// ---------------------------------------------------------------------------

/// A tree for the noids tests: no element is referenced twice and nothing refers back up, so
/// every reference is inlined exactly once (a cycle — which `full_model` has — cannot be
/// written without ids at all, and is an `Err`, see the module docs).
fn noids_model() -> Datamodel {
    let mut dm = Datamodel {
        encoding: "keyvalues2_noids".to_string(),
        encoding_version: 4,
        format: "test".to_string(),
        format_version: 1,
        ..Datamodel::default()
    };
    let scalar = push_element(&mut dm, element("DmeScalar", "", Guid::default(), Vec::new()));
    set_attrs(
        &mut dm,
        scalar,
        vec![
            ("int_attr", Attribute::Int(1)),
            ("float_attr", Attribute::Float(1.5)),
            ("bool_attr", Attribute::Bool(true)),
            ("string_attr", Attribute::String("hello".to_string())),
            ("binary_attr", Attribute::Binary(vec![1, 2, 3])),
            ("time_attr", Attribute::Time(Time(0.25))),
            ("color_attr", Attribute::Color([1, 255, 2, 244])),
            ("vector2_attr", Attribute::Vector2([1.0, 2.0])),
            ("vector3_attr", Attribute::Vector3([1.0, 2.0, 3.0])),
            ("vector4_attr", Attribute::Vector4([1.0, 2.0, 3.0, 4.0])),
            ("quaternion_attr", Attribute::Quaternion([1.0, 2.0, 3.0, 4.0])),
            ("qangle_attr", Attribute::QAngle([0.0, 90.0, 180.0])),
            ("matrix_attr", Attribute::Matrix([[1.0; 4]; 4])),
            ("uint64_attr", Attribute::UInt64(7)),
            ("uint8_attr", Attribute::Byte(255)),
            ("int_array", Attribute::IntArray(vec![1, 2])),
            ("float_array", Attribute::FloatArray(vec![1.5, 2.5])),
            ("string_array", Attribute::StringArray(vec!["a".to_string()])),
            ("binary_array", Attribute::BinaryArray(vec![vec![1, 2]])),
            ("element_array", Attribute::ElementArray(vec![None])),
            ("nothing", Attribute::Element(None)),
        ],
    );
    let unnamed = push_element(&mut dm, element("DmeUnnamed", "", Guid::default(), Vec::new()));
    let root = push_element(&mut dm, element("DmeRoot", "root", Guid::default(), Vec::new()));
    set_attrs(
        &mut dm,
        root,
        vec![
            ("scalar", Attribute::Element(Some(scalar))),
            ("unnamed", Attribute::Element(Some(unnamed))),
        ],
    );
    dm.root = Some(root);
    // a noids file can carry the version-4 prefix block too; upstream writes its id line even
    // when the encoding has no ids, and the decoder skips that line (see the module docs)
    dm.prefix_attributes = vec![(
        "map_asset_references".to_string(),
        Attribute::StringArray(vec!["a.vmdl".to_string()]),
    )];
    dm
}

/// keyvalues2_noids (which upstream refuses to decode) encodes without ids and without GUID
/// references, decodes back into the same elements — every GUID is the zero GUID, so the arena
/// is the only order there is — and writes the very same text again.
#[test]
fn noids_round_trip_is_stable() {
    let dm = noids_model();
    let bytes = keyvalues2::encode(&dm).unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    // the one id line a noids file may hold is the prefix block's own, which upstream writes
    // there too (its `Encode` does not check `SupportsReferenceIds` for that line)
    assert!(text.contains("\"$prefix_element$\""), "the prefix block is written:\n{text}");
    assert_eq!(text.matches("\"elementid\"").count(), 1, "only the prefix id line:\n{text}");
    assert!(text.contains(&format!("\"id\" \"elementid\" \"{}\"", guid_text(&Guid::default()))));
    assert_eq!(
        text.matches(&guid_text(&Guid::default())).count(),
        1,
        "a noids file writes no GUID references besides the prefix id line:\n{text}"
    );
    // both children are inlined where the root refers to them
    assert!(text.contains("\"scalar\" \"DmeScalar\""), "{text}");
    assert!(text.contains("\"unnamed\" \"DmeUnnamed\""), "{text}");

    let decoded = decode_bytes(&bytes).unwrap();
    // the decoder keeps the prefix block as an element of its own (see the module docs)
    assert_eq!(decoded.elements.len(), dm.elements.len() + 1);
    assert!(decoded.elements.iter().all(|element| element.id == Guid::default()));
    assert_eq!(decoded.prefix_attributes, dm.prefix_attributes, "the prefix block survives");
    assert_eq!(preorder_classes(&decoded), preorder_classes(&dm), "the tree must survive");
    assert_eq!(keyvalues2::encode(&decoded).unwrap(), bytes, "a noids write is stable");
}

/// The class names of the model in the order a walk from the root meets them; the noids writer
/// inlines every element at its reference, so this is the whole shape of the file.
fn preorder_classes(dm: &Datamodel) -> Vec<String> {
    let mut classes = Vec::new();
    let mut stack: Vec<ElementId> = dm.root.into_iter().collect();
    stack.reverse();
    while let Some(id) = stack.pop() {
        let element = &dm.elements[id.0];
        classes.push(element.class_name.clone());
        let mut children: Vec<ElementId> = Vec::new();
        for (_, attr) in &element.attributes {
            match attr {
                Attribute::Element(Some(child)) => children.push(*child),
                Attribute::ElementArray(items) => children.extend(items.iter().flatten().copied()),
                _ => {}
            }
        }
        children.reverse();
        stack.extend(children);
    }
    classes
}

/// A noids file that does carry an id line (nothing writes one, but the format tolerates it)
/// has it skipped, exactly as the ids would be ignored.
#[test]
fn noids_skips_an_id_line() {
    let text = file("\"E\"\n{\n\t\"id\" \"elementid\" \"abcdefab-1234-5678-9abc-def012345678\"\n}\n\n");
    let header = parse_header(text.as_bytes()).unwrap().0;
    let mut header = header;
    header.encoding = "keyvalues2_noids".to_string();
    // `rest` is everything after the header's `>`, as the codec contract says
    let after_header = text.find('>').expect("the header has a `>`") + 1;
    let decoded = keyvalues2::decode(&header, &text.as_bytes()[after_header..]).unwrap();
    assert_eq!(decoded.elements.len(), 1);
    assert_eq!(decoded.elements[0].id, Guid::default());
}

// ---------------------------------------------------------------------------
// malformed input (`Decode_NextToken` / `Decode_ParseElement` / `Decode_ParseValue` cases)
// ---------------------------------------------------------------------------

/// A type token that is no type name is an element class name to this reader (upstream resolves
/// it the same way), so the value it was supposed to have is read as a block opener and the
/// decoder fails there — and it fails the same way for an `_array` token that names no type.
#[test]
fn unknown_type_tokens_are_read_as_element_classes() {
    let value = file("\"E\"\n{\n\t\"a\" \"frobnicate\" \"1\"\n}\n");
    let error = decode_bytes(value.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("Expected Element opener, got '1'"), "{error}");

    let array = file("\"E\"\n{\n\t\"a\" \"frobnicate_array\" \n\t[\n\t]\n}\n");
    let error = decode_bytes(array.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("Expected Element opener, got '['"), "{error}");
}

#[test]
fn unterminated_string_errors() {
    let text = file("\"E\"\n{\n\t\"a\" \"int\" \"1");
    let error = decode_bytes(text.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("unexpected end of input"), "{error}");
}

#[test]
fn missing_braces_error() {
    let no_brace = file("\"E\"\n\"int\" \"1\"\n");
    let error = decode_bytes(no_brace.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("Expected Element opener, got 'int'"), "{error}");

    let no_array = file("\"E\"\n{\n\t\"a\" \"int_array\" \n\t\"1\"\n}\n");
    let error = decode_bytes(no_array.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("Expected array opener, got '1'"), "{error}");
}

#[test]
fn unparsable_element_reference_errors() {
    for reference in ["not-a-guid", "0000", "00000000-0000-0000-0000-00000000000"] {
        let text = file(&format!(
            "\"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\t\"a\" \"element\" \"{reference}\"\n}}\n",
            guid_text(&guid(1)),
        ));
        let error = decode_bytes(text.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("could not parse GUID text"), "{reference}: {error}");
    }
}

/// A well-formed GUID no element in the file defines is a *null* reference, as upstream's
/// `AllElements[guid]` lookup gives: accepted, and written back as an empty id.
#[test]
fn unresolvable_reference_becomes_null() {
    let text = file(&format!(
        "\"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\t\"a\" \"element\" \"{}\"\n}}\n\n",
        guid_text(&guid(1)),
        guid_text(&guid(99))
    ));
    let decoded = decode_bytes(text.as_bytes()).unwrap();
    assert_eq!(decoded.elements[0].get("a"), Some(&Attribute::Element(None)));
    let again = String::from_utf8(keyvalues2::encode(&decoded).unwrap()).unwrap();
    assert!(again.contains("\"a\" \"element\" \"\""), "{again}");
}

#[test]
fn duplicate_element_id_errors() {
    let text = file(&format!(
        "\"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n}}\n\"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n}}\n",
        guid_text(&guid(7)),
        guid_text(&guid(7)),
    ));
    let error = decode_bytes(text.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("already in use"), "{error}");
}

/// An array item of the wrong kind for its declared type: a scalar that does not parse, a
/// vector with too few numbers, and a `bool` that is not a byte.
#[test]
fn wrong_array_item_types_error() {
    let id = format!("\t\"id\" \"elementid\" \"{}\"\n", guid_text(&guid(1)));
    let ints = file(&format!(
        "\"E\"\n{{\n{id}\t\"a\" \"int_array\" \n\t[\n\t\t\"1\",\n\t\t\"x\"\n\t]\n}}\n"
    ));
    let error = decode_bytes(ints.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("invalid int"), "{error}");

    let vectors = file(&format!(
        "\"E\"\n{{\n{id}\t\"a\" \"vector3_array\" \n\t[\n\t\t\"1 2\"\n\t]\n}}\n"
    ));
    let error = decode_bytes(vectors.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("expected 3 floats, found 2"), "{error}");

    let bools = file(&format!(
        "\"E\"\n{{\n{id}\t\"a\" \"bool_array\" \n\t[\n\t\t\"x\"\n\t]\n}}\n"
    ));
    let error = decode_bytes(bools.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("invalid bool"), "{error}");
}

/// A type token that starts with a type name but is no type (upstream splits at the first `_`
/// and looks the head up), e.g. `element_something`: upstream routes it through
/// `Decode_ParseValue` with the `element` type, so the value token names the class of the
/// element that follows. Its class name is trimmed, as every non-string value token is.
#[test]
fn element_prefixed_type_tokens_read_an_inline_element() {
    let text = file(&format!(
        "\"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\t\"a\" \"element_something\" \"  DmeChild  \"\n\
         \t{{\n\t\t\"id\" \"elementid\" \"{}\"\n\t}}\n}}\n",
        guid_text(&guid(1)),
        guid_text(&guid(2)),
    ));
    let decoded = decode_bytes(text.as_bytes()).unwrap();
    let child = match decoded.elements[0].get("a") {
        Some(Attribute::Element(Some(child))) => child,
        other => panic!("expected an inline element, got {other:?}"),
    };
    assert_eq!(decoded.elements[child.0].class_name, "DmeChild");
    assert_eq!(decoded.elements[child.0].id, guid(2));
}

/// `uint64` follows upstream's `value.Remove(0, 2)` + `NumberStyles.HexNumber`: the first two
/// characters are dropped whatever they are, and a hex-style `0x` prefix after that is still
/// accepted by the parser.
#[test]
fn uint64_values_follow_dotnet_hex_parsing() {
    let cases = [
        ("0x7", Ok(7u64)),
        ("0X7", Ok(7)),
        ("zz7", Ok(7)),
        ("0x0x7", Err("invalid uint64")), // Remove(0, 2) leaves "0x7", which HexNumber refuses
        ("0x 7", Ok(7)),                  // white space around the digits is allowed
        ("0x7 ", Ok(7)),
        ("0x+7", Err("invalid uint64")),
        ("0x-7", Err("invalid uint64")),
        ("0x1ffffffffffffffff", Err("invalid uint64")), // overflow
        ("€1", Err("invalid uint64")), // two UTF-16 units: nothing is left to parse (was a panic)
        ("é1", Err("invalid uint64")),
        ("😀1", Ok(1)), // one astral char is the two units that go; "1" is left
        ("0xffffffffffffffff", Ok(u64::MAX)),
        ("0x", Err("invalid uint64")),
        ("77", Err("invalid uint64")),
        ("7", Err("must carry a 0x prefix")),
    ];
    for (text, expected) in cases {
        let file_text = file(&format!(
            "\"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\t\"a\" \"uint64\" \"{text}\"\n}}\n",
            guid_text(&guid(1)),
        ));
        match (decode_bytes(file_text.as_bytes()), expected) {
            (Ok(dm), Ok(value)) => {
                assert_eq!(dm.elements[0].get("a"), Some(&Attribute::UInt64(value)), "{text}");
            }
            (Err(error), Err(needle)) => {
                assert!(error.to_string().contains(needle), "{text}: {error}");
            }
            (result, expected) => panic!("{text}: expected {expected:?}, got {result:?}"),
        }
    }
}

/// Upstream's `bool` is `byte.Parse(value) == 1`: any byte that is not 1 is false (2, 255), and
/// a value that is no byte at all is the error.
#[test]
fn bool_values_are_byte_equals_one() {
    for (text, expected) in [("1", true), ("0", false), ("2", false), ("255", false)] {
        let decoded = decode_bytes(
            file(&format!(
                "\"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\t\"a\" \"bool\" \"{text}\"\n}}\n",
                guid_text(&guid(1)),
            ))
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(decoded.elements[0].get("a"), Some(&Attribute::Bool(expected)), "{text}");
    }
    let error = decode_bytes(
        file(&format!(
            "\"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\t\"a\" \"bool\" \"true\"\n}}\n",
            guid_text(&guid(1)),
        ))
        .as_bytes(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("invalid bool"), "{error}");
}

/// An inline element with an id of its own, met before its parent has an id: upstream's fold
/// has no element to attach it to, so it is an error (the prefix-attribute setter refuses
/// Element values). Without an id the nested element is a null, which upstream stores happily.
#[test]
fn inline_element_before_any_id_is_refused() {
    let text = file(&format!(
        "\"E\"\n{{\n\t\"child\" \"SomeClass\"\n\t{{\n\t\t\"id\" \"elementid\" \"{}\"\n\t}}\n}}\n",
        guid_text(&guid(3)),
    ));
    let error = decode_bytes(text.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("cannot become a prefix attribute"), "{error}");

    let nulls = file("\"E\"\n{\n\t\"child\" \"SomeClass\"\n\t{\n\t}\n}\n");
    let decoded = decode_bytes(nulls.as_bytes()).unwrap();
    assert!(decoded.elements.is_empty(), "an element with no id never exists");
    assert_eq!(
        decoded.prefix_attributes,
        vec![("child".to_string(), Attribute::Element(None))]
    );
}

/// Every prefix of a valid file, and of a file cut inside its binary value, must decode to
/// `Ok` or `Err` — never a panic, an overflow or a huge allocation.
#[test]
fn truncating_a_file_at_any_offset_never_panics() {
    let mut dm = lone_root_datamodel();
    let root = dm.root.unwrap();
    set_attrs(
        &mut dm,
        root,
        vec![
            ("blob", Attribute::Binary((0u8..=255).collect())),
            ("refs", Attribute::ElementArray(vec![Some(root), None])),
            ("text", Attribute::String("hello \"quoted\" \\ spaced\t".to_string())),
        ],
    );
    let text = String::from_utf8(keyvalues2::encode_with(&dm, keyvalues2::Layout::Upstream).unwrap())
        .unwrap();
    let bytes = text.as_bytes();
    for length in 0..bytes.len() {
        // the crate-level entry point dispatches on what header the prefix holds, so a cut in
        // the middle of the header, the marker or the body is all covered
        let _ = cs2_dmx::codec::load(&bytes[..length]);
    }
    // and the whole thing still decodes
    assert!(decode_bytes(bytes).is_ok());
}

// ---------------------------------------------------------------------------
// nesting depth
// ---------------------------------------------------------------------------

/// A keyvalues2 file that nests `depth` inline elements, one per level, each with its own id
/// (ids are what make an element exist at all in a keyvalues2 file).
fn nested_file(depth: usize) -> String {
    let mut body = String::from("{\n");
    for level in 0..depth {
        if level > 0 {
            body.push_str("\t\"child\" \"DmeRoot\"\n\t{\n");
        }
        body.push_str(&format!("\t\t\"id\" \"elementid\" \"{}\"\n", guid_text(&guid_index(level))));
    }
    // close every block, innermost first
    for _ in 0..depth {
        body.push_str("\t}\n");
    }
    file(&format!("\"DmeRoot\"\n{body}"))
}

/// A GUID whose two low bytes carry `index`, so a deep file has one id per element.
fn guid_index(index: usize) -> Guid {
    let mut bytes = [0u8; 16];
    bytes[14] = (index >> 8) as u8;
    bytes[15] = index as u8;
    Guid(bytes)
}

/// A model that nests `depth` elements, each referenced once (so each is written inline).
fn nested_model(depth: usize) -> Datamodel {
    let mut dm = lone_root_datamodel();
    let root = dm.root.unwrap();
    dm.elements[root.0].id = guid_index(0);
    let mut previous = root;
    for level in 1..depth {
        let child = push_element(&mut dm, element("DmeRoot", "", guid_index(level), Vec::new()));
        set_attrs(&mut dm, previous, vec![("child", Attribute::Element(Some(child)))]);
        previous = child;
    }
    dm
}

/// The cap is exactly [`keyvalues2::MAX_KV2_DEPTH`] on both sides: a file (or a model) that
/// nests that many elements works, one more is an `Err`, and a hostile 10 000 is an `Err` too —
/// never a stack overflow.
#[test]
fn nesting_is_capped_not_overflowed() {
    let deepest = keyvalues2::MAX_KV2_DEPTH;
    assert_eq!(deepest, 256);
    let decode_error = |depth: usize| -> String {
        match decode_bytes(nested_file(depth).as_bytes()) {
            Ok(_) => panic!("{depth} nested elements must not decode"),
            Err(error) => error.to_string(),
        }
    };
    assert!(
        decode_bytes(nested_file(deepest).as_bytes()).is_ok(),
        "{deepest} nested elements must decode: {:?}",
        decode_bytes(nested_file(deepest).as_bytes()).err()
    );
    let error = decode_error(deepest + 1);
    assert!(error.contains("nesting exceeds 256 levels"), "{error}");
    let error = decode_error(10_000);
    assert!(error.contains("nesting exceeds 256 levels"), "{error}");

    // the writer's cap sits on the same level: the deepest model encodes, and decodes again
    let deepest_text = keyvalues2::encode(&nested_model(deepest)).unwrap();
    assert!(decode_bytes(&deepest_text).is_ok());
    let error = keyvalues2::encode(&nested_model(deepest + 1)).unwrap_err();
    assert!(error.to_string().contains("nesting exceeds 256 levels"), "{error}");
    let error = keyvalues2::encode(&nested_model(10_000)).unwrap_err();
    assert!(error.to_string().contains("nesting exceeds 256 levels"), "{error}");
}

// ---------------------------------------------------------------------------
// float text against .NET 10 itself
// ---------------------------------------------------------------------------

/// Every row of `tests/data/float_format_table.txt` — what .NET 10 prints for
/// `((double)f).ToString("0.##########", InvariantCulture)` (generated by
/// `tests/dotnet/FloatTable`, which says how to regenerate it) — comes out of the encoder
/// unchanged: ties at the tenth decimal, the 1.8e9..2^53 range, everything up to `f32::MAX`
/// (15 significant digits, zero padded), subnormals, random bit patterns.
#[test]
fn float_text_matches_the_dotnet_table() {
    let table = include_str!("data/float_format_table.txt");
    let rows: Vec<(&str, &str)> = table.lines().map(|line| line.split_once(' ').expect("bits/text row")).collect();
    assert!(rows.len() >= 2000, "the table holds {} rows", rows.len());
    // one root per chunk keeps every attribute name unique
    for chunk in rows.chunks(500) {
        let pairs: Vec<(String, f32)> = chunk
            .iter()
            .map(|(bits, _)| (format!("f{bits}"), f32::from_bits(u32::from_str_radix(bits, 16).unwrap())))
            .collect();
        let dm = float_tokens_model(pairs.iter().map(|(name, value)| (name.as_str(), *value)).collect());
        let text = String::from_utf8(keyvalues2::encode(&dm).unwrap()).unwrap();
        let mut lines = text.lines().filter(|line| line.contains("\"float\""));
        for (bits, expected) in chunk {
            let line = lines.next().expect("one line per float");
            assert_eq!(line.trim(), format!("\"f{bits}\" \"float\" \"{expected}\""), "bits {bits}");
        }
    }
}

/// The rows in the 1.845e9..2^53 range specifically (blocker 1: `2000000000` printed as
/// `155325592.6290448384`).
#[test]
fn floats_above_the_u64_overflow_point_keep_their_digits() {
    let dm = float_tokens_model(vec![("two_billion", 2_000_000_000.0), ("big", 9_007_199_254_740_992.0), ("mid", 4_294_967_296.0)]);
    let text = String::from_utf8(keyvalues2::encode(&dm).unwrap()).unwrap();
    assert!(text.contains("\"two_billion\" \"float\" \"2000000000\""), "{text}");
    assert!(text.contains("\"big\" \"float\" \"9007199254740990\""), "15 digits, zero padded: {text}");
    assert!(text.contains("\"mid\" \"float\" \"4294967296\""), "{text}");
}

/// A vector line joins the formatted components with single spaces.
#[test]
fn vector3_line_is_space_joined_dotnet_text() {
    let mut dm = lone_root_datamodel();
    dm.encoding_version = 4;
    set_attrs(
        &mut dm,
        ElementId(0),
        vec![("origin", Attribute::Vector3([-270.11304, 0.1, 2_000_000_000.0]))],
    );
    let text = String::from_utf8(keyvalues2::encode(&dm).unwrap()).unwrap();
    assert!(text.contains("\"origin\" \"vector3\" \"-270.1130371094 0.1000000015 2000000000\""), "{text}");
}

// ---------------------------------------------------------------------------
// number grammar corners (ground truth: .NET 10, see tests/dotnet and the review notes)
// ---------------------------------------------------------------------------

/// Decode one attribute of the given type from `text`.
fn decode_attr(type_name: &str, text: &str) -> Result<Attribute, cs2_dmx::Error> {
    let dm = decode_bytes(
        file(&format!(
            "\"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\t\"a\" \"{type_name}\" \"{text}\"\n}}\n",
            guid_text(&guid(1)),
        ))
        .as_bytes(),
    )?;
    Ok(dm.elements[0].get("a").expect("attribute a").clone())
}

#[test]
fn float_grammar_follows_dotnet() {
    for text in ["inf", "-inf", "+INF", "∞", "0x10", "1_0", "1d", "1.5e", "--1"] {
        assert!(decode_attr("float", text).is_err(), "{text:?} is no float to .NET");
    }
    assert_eq!(decode_attr("float", "Infinity").unwrap(), Attribute::Float(f32::INFINITY));
    assert_eq!(decode_attr("float", "-infinity").unwrap(), Attribute::Float(f32::NEG_INFINITY));
    assert!(matches!(decode_attr("float", "nan").unwrap(), Attribute::Float(v) if v.is_nan()));
    assert_eq!(decode_attr("float", "+1").unwrap(), Attribute::Float(1.0));
    assert_eq!(decode_attr("float", ".5").unwrap(), Attribute::Float(0.5));
    assert_eq!(decode_attr("float", "5.").unwrap(), Attribute::Float(5.0));
    assert_eq!(decode_attr("float", "1E+2").unwrap(), Attribute::Float(100.0));
    // .NET also accepts digit group separators ("1,000" is 1000); this port does not: no
    // writer produces them, so they are refused instead of guessed at
    assert!(decode_attr("float", "1,000").is_err());
    assert!(decode_attr("time", "inf").is_err());
}

#[test]
fn byte_grammar_follows_dotnet() {
    assert_eq!(decode_attr("uint8", "-0").unwrap(), Attribute::Byte(0));
    assert_eq!(decode_attr("uint8", "+1").unwrap(), Attribute::Byte(1));
    assert_eq!(decode_attr("uint8", "01").unwrap(), Attribute::Byte(1));
    assert_eq!(decode_attr("bool", "-0").unwrap(), Attribute::Bool(false));
    assert!(decode_attr("uint8", "-1").is_err());
    assert!(decode_attr("uint8", "256").is_err());
    assert!(decode_attr("uint8", "1,0").is_err());
    assert_eq!(decode_attr("color", "1 2 -0 4").unwrap(), Attribute::Color([1, 2, 0, 4]));
}

#[test]
fn guid_text_forms_dotnet_accepts_decode() {
    let want = "c73e29a3-b55d-40db-afdc-61203fabce54".parse::<Guid>().unwrap();
    for text in [
        "C73E29A3-B55D-40DB-AFDC-61203FABCE54",
        "c73e29a3b55d40dbafdc61203fabce54",
        "(c73e29a3-b55d-40db-afdc-61203fabce54)",
        " c73e29a3-b55d-40db-afdc-61203fabce54 ",
        "{c73e29a3-b55d-40db-afdc-61203fabce54}",
    ] {
        let dm = decode_bytes(file(&format!("\"E\"\n{{\n\t\"id\" \"elementid\" \"{text}\"\n}}\n")).as_bytes()).unwrap();
        assert_eq!(dm.elements[0].id, want, "{text:?}");
    }
    let bad = decode_bytes(file("\"E\"\n{\n\t\"id\" \"elementid\" \"c73e29a3-b55d-40db-afdc-61203fabce5\"\n}\n").as_bytes());
    assert!(bad.is_err(), "a short GUID is a FormatException");
}

/// Upstream reads an `element` item in a value array as a reference and casts the array to
/// `ElementArray`, which throws for any other array type.
#[test]
fn element_item_in_a_value_array_is_an_error() {
    for type_name in ["string_array", "int_array", "float_array"] {
        let text = file(&format!(
            "\"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\t\"a\" \"{type_name}\" [\n\t\t\"element\"\n\t]\n}}\n",
            guid_text(&guid(1)),
        ));
        assert!(decode_bytes(text.as_bytes()).is_err(), "{type_name}");
    }
}

/// Blocker 2: `value[2..]` split a multi-byte character and panicked.
#[test]
fn non_ascii_uint64_is_an_error_not_a_panic() {
    for text in ["€1", "é1", "ée", "0é"] {
        assert!(decode_attr("uint64", text).is_err(), "{text:?}");
    }
}

// ---------------------------------------------------------------------------
// an element array redefined under the same name (blocker 3)
// ---------------------------------------------------------------------------

fn two_arrays(first: &[&str], second: &[&str]) -> String {
    let items = |refs: &[&str]| {
        refs.iter().map(|id| format!("\t\t\"element\" \"{id}\"")).collect::<Vec<_>>().join(",\n")
    };
    file(&format!(
        "\"R\"\n{{\n\t\"id\" \"elementid\" \"{r}\"\n\t\"a\" \"element_array\"\n\t[\n{}\n\t]\n\t\"a\" \"element_array\"\n\t[\n{}\n\t]\n}}\n",
        items(first),
        items(second),
        r = guid_text(&guid(1)),
    ))
}

/// The second `a` is shorter than the first: the first array's references used to index past
/// the end of the second and panic.
#[test]
fn shorter_replacement_array_drops_the_orphans_references() {
    let r = guid_text(&guid(1));
    let dm = decode_bytes(two_arrays(&[&r, &r, &r], &[]).as_bytes()).unwrap();
    assert_eq!(dm.elements[0].get("a"), Some(&Attribute::ElementArray(vec![])));
    assert_eq!(dm.elements[0].attributes.len(), 1, "the name keeps one slot");
}

/// The second `a` is longer: the first array's references must not write into it.
#[test]
fn longer_replacement_array_keeps_its_own_references() {
    let r = guid_text(&guid(1));
    let dm = decode_bytes(two_arrays(&[&r], &["", "", ""]).as_bytes()).unwrap();
    assert_eq!(dm.elements[0].get("a"), Some(&Attribute::ElementArray(vec![None, None, None])));
    let dm = decode_bytes(two_arrays(&[&r, &r], &["", &r, ""]).as_bytes()).unwrap();
    assert_eq!(dm.elements[0].get("a"), Some(&Attribute::ElementArray(vec![None, Some(ElementId(0)), None])));
}

/// A reference attribute that replaces an element array of the same name wins, as before.
#[test]
fn scalar_reference_replacing_an_array_drops_the_array_references() {
    let r = guid_text(&guid(1));
    let text = file(&format!(
        "\"R\"\n{{\n\t\"id\" \"elementid\" \"{r}\"\n\t\"a\" \"element_array\"\n\t[\n\t\t\"element\" \"{r}\"\n\t]\n\t\"a\" \"int\" \"4\"\n}}\n"
    ));
    let dm = decode_bytes(text.as_bytes()).unwrap();
    assert_eq!(dm.elements[0].get("a"), Some(&Attribute::Int(4)));
}

// ---------------------------------------------------------------------------
// round 3: id lines, prefix blocks, noids prefix id, duplicate ids
// ---------------------------------------------------------------------------

fn as_noids(text: &str) -> (cs2_dmx::codec::Header, Vec<u8>) {
    let mut header = parse_header(text.as_bytes()).unwrap().0;
    header.encoding = "keyvalues2_noids".to_string();
    let after_header = text.find('>').expect("the header has a `>`") + 1;
    (header, text.as_bytes()[after_header..].to_vec())
}

/// Upstream looks for the id line only while the element does not exist yet; a second one is an
/// attribute whose type `elementid` is taken for a class name, which then lacks its opener.
#[test]
fn a_second_id_line_on_an_element_is_an_error() {
    let text = file(&format!(
        "\"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\t\"id\" \"elementid\" \"{}\"\n}}\n",
        guid_text(&guid(1)),
        guid_text(&guid(2)),
    ));
    let error = decode_bytes(text.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("Expected Element opener"), "{error}");

    // noids tolerates one id line (it is skipped), not two
    let (header, body) = as_noids(&text);
    let error = keyvalues2::decode(&header, &body).unwrap_err();
    assert!(error.to_string().contains("Expected Element opener"), "{error}");
}

/// A prefix block never has an element, so it takes every id line; the last one wins.
#[test]
fn a_prefix_block_takes_repeated_id_lines() {
    let text = file(&format!(
        "\"$prefix_element$\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\t\"id\" \"elementid\" \"{}\"\n}}\n",
        guid_text(&guid(1)),
        guid_text(&guid(2)),
    ));
    let dm = decode_bytes(text.as_bytes()).unwrap();
    assert_eq!(dm.elements[0].id, guid(2));
}

/// Blocker 6: a second `$prefix_element$` block only sets the GUID (upstream `PrefixElementId`)
/// and merges its attributes into the prefix attributes. It used to create and register an
/// element, which then stole the root from the real first element.
#[test]
fn a_second_prefix_block_never_makes_an_element() {
    let text = file(&format!(
        "\"$prefix_element$\"\n{{\n\t\"id\" \"elementid\" \"{p1}\"\n\t\"a\" \"int\" \"1\"\n}}\n\
         \"$prefix_element$\"\n{{\n\t\"id\" \"elementid\" \"{p2}\"\n\t\"a\" \"int\" \"2\"\n\t\"b\" \"int\" \"3\"\n}}\n\
         \"R\"\n{{\n\t\"id\" \"elementid\" \"{r}\"\n}}\n\n",
        p1 = guid_text(&guid(1)),
        p2 = guid_text(&guid(2)),
        r = guid_text(&guid(3)),
    ));
    let dm = decode_bytes(text.as_bytes()).unwrap();
    assert_eq!(dm.elements.len(), 2, "the prefix element and R");
    assert_eq!(dm.root.map(|id| dm.elements[id.0].class_name.as_str()), Some("R"));
    assert_eq!(dm.elements[0].class_name, "$prefix_element$");
    assert_eq!(dm.elements[0].id, guid(2), "the last block's GUID");
    assert_eq!(
        dm.prefix_attributes,
        vec![("a".to_string(), Attribute::Int(2)), ("b".to_string(), Attribute::Int(3))]
    );
    // and in noids the second block does not become an element either
    let (header, body) = as_noids(&text);
    let noids = keyvalues2::decode(&header, &body).unwrap();
    assert_eq!(noids.elements.len(), 2);
    assert_eq!(noids.root.map(|id| noids.elements[id.0].class_name.as_str()), Some("R"));
    assert_eq!(noids.elements[0].id, guid(2));
}

/// Blocker 7: the block is written from `prefix_attributes`, so editing them after a decode
/// shows in the file; the GUID still comes from the prefix element.
#[test]
fn an_edit_to_the_prefix_attributes_reaches_the_encoded_block() {
    let bytes = keyvalues2::encode(&reference_model()).unwrap();
    let mut dm = decode_bytes(&bytes).unwrap();
    assert!(!dm.prefix_attributes.is_empty());
    dm.prefix_attributes = vec![("edited".to_string(), Attribute::String("yes".to_string()))];
    let text = String::from_utf8(keyvalues2::encode(&dm).unwrap()).unwrap();
    assert!(text.contains("\t\"edited\" \"string\" \"yes\"\n"), "{text}");
    assert!(text.contains(&format!("\"id\" \"elementid\" \"{}\"", guid_text(&guid(1)))), "{text}");
    let again = decode_bytes(text.as_bytes()).unwrap();
    assert_eq!(again.prefix_attributes, dm.prefix_attributes);
    assert_eq!(text.matches("\"$prefix_element$\"").count(), 1);
}

/// Blocker 8: a model that came from binary 9 holds the prefix as a `DmElement` right after the
/// root. Encoded as keyvalues2 it gets the block with *that element's* GUID, and the element is
/// not written a second time as an ordinary block.
#[test]
fn a_binary_shaped_prefix_element_is_the_block_not_a_second_element() {
    let mut dm = Datamodel {
        encoding: "keyvalues2".to_string(),
        encoding_version: 4,
        format: "vmap".to_string(),
        format_version: 35,
        prefix_attributes: vec![("map_asset_references".to_string(), Attribute::StringArray(vec!["a.vmap".to_string()]))],
        ..Datamodel::default()
    };
    let root = push_element(&mut dm, element("CMapRootElement", "", guid(1), vec![]));
    let prefix = push_element(
        &mut dm,
        element(
            "DmElement",
            "",
            guid(2),
            vec![("map_asset_references", Attribute::StringArray(vec!["a.vmap".to_string()]))],
        ),
    );
    dm.root = Some(root);
    assert_eq!(dm.prefix_element(), Some(prefix));
    // the root refers to the prefix element twice, which would write it as a block of its own
    set_attrs(
        &mut dm,
        root,
        vec![("one", Attribute::Element(Some(prefix))), ("two", Attribute::Element(Some(prefix)))],
    );
    let text = String::from_utf8(keyvalues2::encode(&dm).unwrap()).unwrap();
    let prefix_block = text.split("\"CMapRootElement\"").next().unwrap();
    assert!(prefix_block.contains(&format!("\"id\" \"elementid\" \"{}\"", guid_text(&guid(2)))), "{text}");
    assert_eq!(text.matches("\"DmElement\"").count(), 0, "the prefix element is not an ordinary block:\n{text}");
    assert_eq!(text.matches(&guid_text(&guid(2))).count(), 3, "the prefix id and the two references:\n{text}");
}

/// The upstream-shaped binary fold needs the same keys, the class and an empty name; anything
/// else is an ordinary element.
#[test]
fn prefix_element_shape_is_strict() {
    let mut dm = Datamodel {
        prefix_attributes: vec![("k".to_string(), Attribute::Int(1))],
        ..Datamodel::default()
    };
    push_element(&mut dm, element("Root", "", guid(1), vec![]));
    let candidate = push_element(&mut dm, element("DmElement", "", guid(2), vec![("k", Attribute::Int(1))]));
    assert_eq!(dm.prefix_element(), Some(candidate));
    dm.elements[1].name = "named".to_string();
    assert_eq!(dm.prefix_element(), None, "a name");
    dm.elements[1].name.clear();
    dm.elements[1].class_name = "DmeOther".to_string();
    assert_eq!(dm.prefix_element(), None, "the class");
    dm.elements[1].class_name = "DmElement".to_string();
    dm.elements[1].attributes[0].0 = "other".to_string();
    assert_eq!(dm.prefix_element(), None, "the keys");
    dm.elements[1].attributes[0].0 = "k".to_string();
    dm.prefix_attributes.clear();
    assert_eq!(dm.prefix_element(), None, "no prefix attributes");
    dm.elements[0].class_name = "$prefix_element$".to_string();
    assert_eq!(dm.prefix_element(), Some(ElementId(0)), "the keyvalues2 class wins");
}

/// Blocker 9: a noids file's prefix block keeps its GUID, so the file reads back byte for byte.
#[test]
fn noids_prefix_guid_round_trips() {
    let text = format!(
        "<!-- dmx encoding keyvalues2_noids 4 format test 1 -->\n\"$prefix_element$\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\t\"k\" \"int\" \"1\"\n}}\n\"E\"\n{{\n}}\n\n",
        guid_text(&guid(9)),
    );
    let dm = cs2_dmx::load(text.as_bytes()).unwrap();
    assert_eq!(dm.elements[0].id, guid(9));
    assert_eq!(String::from_utf8(keyvalues2::encode(&dm).unwrap()).unwrap(), text);
}

/// Blocker 9: two reachable elements with one id cannot be written as a file that reads back.
#[test]
fn duplicate_element_ids_refuse_to_encode() {
    let mut dm = single_element_model(4, "E");
    let root = dm.root.unwrap();
    let shared = dm.elements[root.0].id;
    let child = push_element(&mut dm, element("C", "", shared, vec![]));
    set_attrs(&mut dm, root, vec![("c", Attribute::Element(Some(child)))]);
    let error = keyvalues2::encode(&dm).unwrap_err();
    assert!(matches!(error, cs2_dmx::Error::Encode(_)), "{error:?}");
    assert!(error.to_string().contains("share the id"), "{error}");
    // the same model without ids is fine, ids are never written there
    dm.encoding = "keyvalues2_noids".to_string();
    keyvalues2::encode(&dm).unwrap();
}

/// An unreachable duplicate is nobody's problem: it is not written.
#[test]
fn an_unreachable_duplicate_id_is_not_written() {
    let mut dm = single_element_model(4, "E");
    let root = dm.root.unwrap();
    let shared = dm.elements[root.0].id;
    push_element(&mut dm, element("Orphan", "", shared, vec![]));
    keyvalues2::encode(&dm).unwrap();
}

// ---------------------------------------------------------------------------
// round 3 nits
// ---------------------------------------------------------------------------

/// Nit 12: upstream writes no prefix block when there are no prefix attributes; the Valve layout
/// keeps a block that only holds its id line (a decoded file can have one).
#[test]
fn upstream_layout_writes_no_empty_prefix_block() {
    let text = file(&format!(
        "\"$prefix_element$\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n}}\n\"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n}}\n\n",
        guid_text(&guid(9)),
        guid_text(&guid(10)),
    ));
    let dm = decode_bytes(text.as_bytes()).unwrap();
    let upstream = keyvalues2::encode_with(&dm, keyvalues2::Layout::Upstream).unwrap();
    assert!(!String::from_utf8(upstream).unwrap().contains("$prefix_element$"));
    let valve = keyvalues2::encode_with(&dm, keyvalues2::Layout::Valve).unwrap();
    assert_eq!(String::from_utf8(valve).unwrap(), text);
}

/// Nit 13: an inline element of class `$prefix_element$` is a null to its parent (upstream
/// returns null for that block), and its attributes land in the prefix attributes.
#[test]
fn an_inline_prefix_class_element_is_a_null_attribute() {
    let text = file(&format!(
        "\"E\"\n{{\n\t\"id\" \"elementid\" \"{}\"\n\t\"x\" \"$prefix_element$\"\n\t{{\n\t\t\"id\" \"elementid\" \"{}\"\n\t\t\"k\" \"int\" \"5\"\n\t}}\n}}\n\n",
        guid_text(&guid(1)),
        guid_text(&guid(2)),
    ));
    let dm = decode_bytes(text.as_bytes()).unwrap();
    assert_eq!(dm.elements.len(), 2);
    assert_eq!(dm.elements[1].class_name, "$prefix_element$", "built when the inline block opens");
    assert_eq!(dm.elements[1].id, guid(2));
    let root = dm.root.unwrap();
    assert_eq!(dm.elements[root.0].get("x"), Some(&Attribute::Element(None)));
    assert_eq!(dm.prefix_attributes, vec![("k".to_string(), Attribute::Int(5))]);
}

/// Nit 13: the codec only writes keyvalues2 1-4.
#[test]
fn encode_refuses_versions_outside_the_supported_range() {
    for version in [0, 5, 9] {
        let dm = single_element_model(version, "E");
        assert!(keyvalues2::encode(&dm).is_err(), "version {version}");
    }
    let mut dm = single_element_model(1, "E");
    dm.encoding = "binary".to_string();
    assert!(keyvalues2::encode(&dm).is_err(), "not a keyvalues2 encoding");
}

/// A reference met twice is not a noids problem: with no ids it is written inline both times.
#[test]
fn noids_writes_a_shared_reference_inline_each_time() {
    let mut dm = single_element_model(4, "Root");
    dm.encoding = "keyvalues2_noids".to_string();
    let root = dm.root.unwrap();
    let shared = push_element(&mut dm, element("Shared", "", Guid::default(), vec![]));
    set_attrs(&mut dm, root, vec![("a", Attribute::Element(Some(shared))), ("b", Attribute::Element(Some(shared)))]);
    let text = String::from_utf8(keyvalues2::encode(&dm).unwrap()).unwrap();
    assert_eq!(text.matches("\"Shared\"").count(), 2, "{text}");
}

// ---------------------------------------------------------------------------
// tests that pin lines mutation testing found unguarded
// ---------------------------------------------------------------------------

/// `TimeSpan.FromTicks((long)(seconds * 10^7))`: the cast truncates toward zero, so the
/// 1.9 ticks of `0.00000019` are 1 and the -1.9 of its negation are -1 (rounding would give
/// 2 and -2); the ticks are the only thing the value keeps.
#[test]
fn time_seconds_truncate_to_whole_ticks() {
    let ticks = |text: &str| match decode_attr("time", text).unwrap() {
        Attribute::Time(time) => (time.0 * 10_000_000.0).round() as i64,
        other => panic!("{other:?}"),
    };
    assert_eq!(ticks("0.00000019"), 1);
    assert_eq!(ticks("-0.00000019"), -1);
    assert_eq!(ticks("1.23456789012"), 12_345_678);
    assert_eq!(ticks("0.00000001"), 0);
    assert_eq!(ticks("NaN"), 0, "a NaN is no ticks (.NET: 0)");
    assert_eq!(ticks(" 2 "), 20_000_000, "the padding is trimmed");
    // out-of-range seconds saturate at the tick limits like .NET 10
    assert!(matches!(decode_attr("time", "1e300").unwrap(), Attribute::Time(time) if time.0 > 9.0e11));
    assert!(matches!(decode_attr("time", "-1e300").unwrap(), Attribute::Time(time) if time.0 < -9.0e11));
}

/// A color row has to be bytes all the way: the first four are the color, the rest are still
/// parsed (`byte.Parse` over every part, upstream) and refuse junk or out-of-range values.
#[test]
fn color_rows_must_be_bytes_throughout() {
    assert_eq!(decode_attr("color", "1 2 3 4").unwrap(), Attribute::Color([1, 2, 3, 4]));
    assert_eq!(decode_attr("color", "1 2 3 4 5 6").unwrap(), Attribute::Color([1, 2, 3, 4]));
    assert!(decode_attr("color", "1 2 3 4 junk").is_err());
    assert!(decode_attr("color", "1 2 3 4 256").is_err());
    assert!(decode_attr("color", "1 2 3 4 -1").is_err());
    assert!(decode_attr("color", "1 2 3").is_err());
    assert!(decode_attr("color", "1 2 3 256").is_err());
}

/// Every type but `string` is trimmed before it is parsed; a string keeps its padding.
#[test]
fn values_are_trimmed_but_strings_are_not() {
    assert_eq!(decode_attr("int", "  7\t").unwrap(), Attribute::Int(7));
    assert_eq!(decode_attr("float", " 1.5 ").unwrap(), Attribute::Float(1.5));
    assert_eq!(decode_attr("bool", " 1 ").unwrap(), Attribute::Bool(true));
    assert_eq!(decode_attr("uint8", " 9 ").unwrap(), Attribute::Byte(9));
    assert_eq!(decode_attr("vector2", "  1 2  ").unwrap(), Attribute::Vector2([1.0, 2.0]));
    assert_eq!(decode_attr("binary", "  0aFF ").unwrap(), Attribute::Binary(vec![0x0a, 0xff]));
    assert_eq!(decode_attr("string", "  padded  ").unwrap(), Attribute::String("  padded  ".to_string()));
    // a binary value tolerates Hammer's wrapping (tabs, line breaks, spaces, backslashes inside)
    assert_eq!(decode_attr("binary", "0a\\n\\t\\t FF").unwrap(), Attribute::Binary(vec![0x0a, 0xff]));
    assert!(decode_attr("binary", "0aF").is_err(), "an odd digit count");
    assert!(decode_attr("binary", "0g").is_err(), "a non-hex digit");
    assert!(decode_attr("binary", "+1").is_err(), "HexNumber takes no sign");
}

/// Every prefix of models the size of the real sample files, in both layouts and both
/// encodings, decodes to `Ok` or `Err` and never panics (a cut can land inside a wrapped
/// binary value, an escape, a GUID, a header, an array).
#[test]
fn truncating_big_models_at_any_offset_never_panics() {
    let mut dm = full_model(
        "keyvalues2",
        4,
        vec![
            ("uint8_attr", Attribute::Byte(255)),
            ("uint64_attr", Attribute::UInt64(0xFFFF_FFFF)),
            ("qangle_attr", Attribute::QAngle([0.0, 90.0, 180.0])),
            ("blob_big", Attribute::Binary((0..1200u32).map(|i| (i * 7 % 256) as u8).collect())),
            ("blobs", Attribute::BinaryArray(vec![vec![1, 2, 3], vec![4; 100]])),
        ],
    );
    dm.prefix_attributes = vec![
        ("thumbnail".to_string(), Attribute::Binary((0..500u32).map(|i| (i % 251) as u8).collect())),
        ("refs".to_string(), Attribute::StringArray(vec!["a.vmap".to_string(), "b \"c\"".to_string()])),
    ];
    // forty children, each referenced from an array, a few twice (blocks after the root)
    let root = dm.root.unwrap();
    let mut children = Vec::new();
    for index in 0..40u8 {
        let mut id = guid(0);
        id.0[0] = index + 1;
        let child = push_element(
            &mut dm,
            element("DmeChild", &format!("child {index}"), id, vec![("order", Attribute::Int(i32::from(index)))]),
        );
        children.push(Some(child));
    }
    let shared: Vec<Option<ElementId>> = children.iter().take(5).copied().collect();
    dm.elements[root.0].attributes.push(("children".to_string(), Attribute::ElementArray(children)));
    dm.elements[root.0].attributes.push(("again".to_string(), Attribute::ElementArray(shared)));

    // the same children and prefix on a tree the noids writer can inline (full_model has a cycle)
    let mut tree = noids_model();
    tree.prefix_attributes = dm.prefix_attributes.clone();
    let tree_root = tree.root.unwrap();
    let mut tree_children = Vec::new();
    for index in 0..40i32 {
        let child = push_element(
            &mut tree,
            element("DmeChild", &format!("child {index}"), Guid::default(), vec![("order", Attribute::Int(index))]),
        );
        tree_children.push(Some(child));
    }
    tree.elements[tree_root.0].attributes.push(("children".to_string(), Attribute::ElementArray(tree_children)));
    tree.elements[tree_root.0].attributes.push(("blob".to_string(), Attribute::Binary(vec![0xab; 1200])));

    // and one the size of the 22 KB sample (too slow to cut at every byte in a debug build, so
    // the cuts are every 13th byte plus every byte near both ends, covering each token shape)
    let mut huge = dm.clone();
    let huge_root = huge.root.unwrap();
    let mut more = Vec::new();
    for index in 0..250u32 {
        let mut id = guid(0);
        id.0[0] = 200;
        id.0[1] = (index % 250) as u8;
        id.0[2] = (index / 250) as u8 + 1;
        let child = push_element(
            &mut huge,
            element("DmeChild", &format!("more {index}"), id, vec![("pos", Attribute::Vector3([index as f32, 0.5, -1.0]))]),
        );
        more.push(Some(child));
    }
    huge.elements[huge_root.0].attributes.push(("more".to_string(), Attribute::ElementArray(more)));

    let mut checked = 0usize;
    for (model, layout, sparse) in [
        (&dm, keyvalues2::Layout::Valve, false),
        (&dm, keyvalues2::Layout::Upstream, false),
        (&tree, keyvalues2::Layout::Valve, false),
        (&huge, keyvalues2::Layout::Valve, true),
    ] {
        let bytes = keyvalues2::encode_with(model, layout).unwrap();
        assert!(bytes.len() > 5_000, "{} bytes is too small to matter", bytes.len());
        if sparse {
            assert!(bytes.len() > 20_000, "{} bytes", bytes.len());
        }
        for length in 0..bytes.len() {
            if sparse && length > 600 && length + 600 < bytes.len() && length % 13 != 0 {
                continue;
            }
            let _ = cs2_dmx::codec::load(&bytes[..length]);
            checked += 1;
        }
        assert!(cs2_dmx::codec::load(&bytes).is_ok(), "{} {layout:?} decodes whole", model.encoding);
    }
    assert!(checked > 15_000, "{checked} prefixes");
}
