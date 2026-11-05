//! The binary codec against models built in code: every attribute type of every version, the
//! exact bytes of small files, and the ways hostile input must fail.
//!
//! Modelled on the binary parts of upstream `Tests/Tests.cs` (`Create_Binary_1` … `Create_Binary_9`,
//! `DmxSaveAndLoad*`) and `Tests/RoundTripTests.cs` (`Binary_PrefixAttributes`), which build a
//! datamodel in code and check that a save and a load return the same values. The upstream tests
//! that need a typed element layer, `dmxconvert.exe` or the reflection factory have no Rust
//! equivalent and are not ported.

use std::collections::BTreeMap;

use cs2_dmx::codec::binary::Layout;
use cs2_dmx::codec::{self, binary, Header};
use cs2_dmx::{Attribute, Datamodel, Element, ElementId, Error, Guid, Time};

// --------------------------------------------------------------------------- helpers

/// A recognizable GUID: `nn` in every byte, prefixed by a counter.
fn guid(n: u8) -> Guid {
    let mut bytes = [n; 16];
    bytes[0] = n;
    bytes[15] = 0xaa;
    Guid(bytes)
}

fn push_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_i16(out: &mut Vec<u8>, value: i16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn raw(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}

/// `<!-- dmx encoding … -->` plus the newline and NUL the header line ends with.
fn file_head(encoding: &str, version: i32, format: &str, format_version: i32) -> Vec<u8> {
    let mut out = format!("<!-- dmx encoding {encoding} {version} format {format} {format_version} -->\n").into_bytes();
    out.push(0);
    out
}

/// The header and the version 9 prefix element count of an empty file.
fn v9_head() -> Vec<u8> {
    let mut out = file_head("binary", 9, "test", 1);
    push_i32(&mut out, 0); // no prefix elements
    out
}

fn element(class_name: &str, name: &str, id: Guid) -> Element {
    Element { class_name: class_name.to_string(), name: name.to_string(), id, stub: false, attributes: Vec::new() }
}

fn model(version: i32) -> Datamodel {
    Datamodel {
        encoding: "binary".into(),
        encoding_version: version,
        format: "test".into(),
        format_version: 1,
        prefix_attributes: Vec::new(),
        binary_string_table: Vec::new(),
        elements: Vec::new(),
        root: None,
    }
}

/// An attribute with every element reference replaced by the referenced element's GUID, so two
/// models compare equal whatever order their element lists are in (a decode appends stubs last).
fn canonical(attribute: &Attribute, dm: &Datamodel) -> Attribute {
    let name = |id: &ElementId| format!("element:{}", dm.elements[id.0].id);
    match attribute {
        Attribute::Element(Some(id)) => Attribute::String(name(id)),
        Attribute::ElementArray(items) => {
            Attribute::StringArray(items.iter().map(|item| item.as_ref().map(name).unwrap_or_else(|| "null".into())).collect())
        }
        other => other.clone(),
    }
}

type ElementForm = (String, String, bool, Vec<(String, Attribute)>);

/// A datamodel as a GUID-keyed map, with element references reduced to GUIDs.
fn view(dm: &Datamodel) -> (Option<Guid>, BTreeMap<Guid, ElementForm>) {
    let elements = dm
        .elements
        .iter()
        .map(|element| {
            let attributes = element.attributes.iter().map(|(name, value)| (name.clone(), canonical(value, dm))).collect();
            (element.id, (element.class_name.clone(), element.name.clone(), element.stub, attributes))
        })
        .collect();
    (dm.root.map(|root| dm.elements[root.0].id), elements)
}

fn assert_same_model(left: &Datamodel, right: &Datamodel) {
    assert_eq!(left.encoding, right.encoding);
    assert_eq!(left.encoding_version, right.encoding_version);
    assert_eq!(left.format, right.format);
    assert_eq!(left.format_version, right.format_version);
    assert_eq!(left.prefix_attributes, right.prefix_attributes, "prefix attributes");
    assert_eq!(view(left), view(right), "elements");
}

/// A save and a load return the same model, and saving it again returns the same bytes.
/// [`codec::save`] is [`Layout::Valve`].
fn round_trip(dm: &Datamodel) -> Datamodel {
    let bytes = codec::save(dm).expect("save");
    let back = codec::load(&bytes).expect("load");

    assert_eq!(codec::save(&back).expect("save again"), bytes, "the encoder must be stable on its own output");
    back
}

/// The same, through the upstream-shaped layout, which writes only what the root reaches.
fn round_trip_upstream(dm: &Datamodel) -> Datamodel {
    let bytes = binary::encode_with(dm, Layout::Upstream).expect("save");
    let back = codec::load(&bytes).expect("load");

    assert_eq!(
        binary::encode_with(&back, Layout::Upstream).expect("save again"),
        bytes,
        "the encoder must be stable on its own output"
    );
    back
}

/// Upstream `Tests.cs` `TestValues_V1/V2/V3`: one value of every type the version supports.
fn scalar_values(version: i32) -> Vec<(&'static str, Attribute)> {
    let mut values: Vec<(&'static str, Attribute)> = vec![
        ("String", Attribute::String("hello_world".into())),
        ("Int", Attribute::Int(-1)),
        ("Float", Attribute::Float(1.5)),
        ("Bool", Attribute::Bool(true)),
        ("Binary", Attribute::Binary(vec![1, 2, 3, 4])),
        ("Color", Attribute::Color([1, 255, 2, 244])),
        ("Vector2", Attribute::Vector2([1.0, 2.0])),
        ("Vector3", Attribute::Vector3([1.0, 2.0, 3.0])),
        ("Vector4", Attribute::Vector4([1.0, 2.0, 3.0, 4.0])),
        ("Quaternion", Attribute::Quaternion([0.5, 0.5, 0.5, 0.5])),
        (
            "Matrix",
            Attribute::Matrix([[1.0, 2.0, 3.0, 4.0], [5.0, 6.0, 7.0, 8.0], [9.0, 10.0, 11.0, 12.0], [13.0, 14.0, 15.0, 16.0]]),
        ),
    ];

    if version >= 3 {
        values.push(("Time", Attribute::Time(Time(300.0001))));
    }
    if version >= 9 {
        values.push(("Byte", Attribute::Byte(0xff)));
        values.push(("UInt64", Attribute::UInt64(0xffff_ffff)));
        values.push(("QAngle", Attribute::QAngle([0.0, 90.0, 180.0])));
        values.push(("ByteArray", Attribute::ByteArray(vec![9, 8, 7])));
    }
    values
}

/// The same value as an array of two items (upstream's "… array" attributes).
fn array_of(value: &Attribute) -> Attribute {
    match value {
        Attribute::Element(value) => Attribute::ElementArray(vec![*value, None]),
        Attribute::Int(value) => Attribute::IntArray(vec![*value, 2]),
        Attribute::Float(value) => Attribute::FloatArray(vec![*value, -0.5]),
        Attribute::Bool(value) => Attribute::BoolArray(vec![*value, false]),
        Attribute::String(value) => Attribute::StringArray(vec![value.clone(), String::new()]),
        Attribute::Binary(value) => Attribute::BinaryArray(vec![value.clone(), Vec::new()]),
        Attribute::Time(value) => Attribute::TimeArray(vec![*value, Time(-0.5)]),
        Attribute::Color(value) => Attribute::ColorArray(vec![*value, [0, 0, 0, 255]]),
        Attribute::Vector2(value) => Attribute::Vector2Array(vec![*value, [0.0, 0.0]]),
        Attribute::Vector3(value) => Attribute::Vector3Array(vec![*value, [0.0, 0.0, 0.0]]),
        Attribute::Vector4(value) => Attribute::Vector4Array(vec![*value, [0.0, 0.0, 0.0, 0.0]]),
        Attribute::QAngle(value) => Attribute::QAngleArray(vec![*value, [1.0, 2.0, 3.0]]),
        Attribute::Quaternion(value) => Attribute::QuaternionArray(vec![*value, [0.0, 0.0, 0.0, 1.0]]),
        Attribute::Matrix(value) => Attribute::MatrixArray(vec![*value, [[0.0; 4]; 4]]),
        Attribute::UInt64(value) => Attribute::UInt64Array(vec![*value, 0]),
        Attribute::Byte(value) => Attribute::ByteArray(vec![*value, 0]),
        // upstream's "array of a byte[]" is a `List<byte[]>`, a binary array of one blob and an
        // empty one
        Attribute::ByteArray(value) => Attribute::BinaryArray(vec![value.clone(), Vec::new()]),
        other => panic!("no array of {other:?}"),
    }
}

/// Upstream `DatamodelTests.Populate` for `binary`: one element with an attribute of every type
/// the version supports and an array of each, a self-reference, a referenced child, an element
/// array and a stub reference.
fn populated(version: i32) -> Datamodel {
    let root = ElementId(0);
    let child = ElementId(1);
    let named = ElementId(2);
    let stub = ElementId(3);

    let mut dm = model(version);
    dm.root = Some(root);
    dm.elements.push(element("DmeElement", "root", guid(1)));
    dm.elements.push(element("DmeElement", "", guid(2)));
    dm.elements.push(element("DmeElement", "TestElement", guid(3)));
    dm.elements.push(Element {
        class_name: "DmeElement".into(),
        name: "Stub element".into(),
        id: guid(4),
        stub: true,
        attributes: Vec::new(),
    });

    let mut attributes: Vec<(String, Attribute)> = Vec::new();
    for (name, value) in scalar_values(version) {
        attributes.push((name.to_string(), value.clone()));
        attributes.push((format!("{name} array"), array_of(&value)));
    }
    attributes.push(("Recursive".into(), Attribute::Element(Some(root))));
    attributes.push(("NoName".into(), Attribute::Element(Some(child))));
    attributes.push(("NullElement".into(), Attribute::Element(None)));
    attributes.push(("ElemArray".into(), Attribute::ElementArray(vec![Some(stub), Some(root), Some(named), None])));
    attributes.push(("ElementStub".into(), Attribute::Element(Some(stub))));
    dm.elements[root.0].attributes = attributes;

    dm
}

// --------------------------------------------------------------------------- round trips

/// Upstream `Create_Binary_1` … `Create_Binary_9`: a model with every value the version supports
/// comes back identical, for every version.
#[test]
fn every_version_round_trips_every_value_it_supports() {
    for version in [1, 2, 3, 4, 5, 9] {
        let dm = populated(version);
        let back = round_trip(&dm);
        assert_same_model(&dm, &back);
        assert_eq!(back.elements.len(), 4, "v{version}: root, child, named and the stub");
        assert_eq!(back.root, Some(ElementId(0)), "v{version}");
    }
}

/// The values a version cannot store are refused with [`Error::Encode`], naming the type, rather
/// than written under another type's id (upstream `CodecException`).
#[test]
fn version_limited_values_are_refused() {
    let cases: Vec<(&'static str, i32, Attribute)> = vec![
        ("Time", 2, Attribute::Time(Time(1.0))),
        ("Time", 1, Attribute::Time(Time(1.0))),
        ("Time", 2, Attribute::TimeArray(vec![Time(1.0)])),
        ("Byte", 5, Attribute::Byte(1)),
        ("Byte", 1, Attribute::Byte(1)),
        ("UInt64", 5, Attribute::UInt64(1)),
        ("UInt64", 2, Attribute::UInt64(1)),
        ("UInt64", 5, Attribute::UInt64Array(vec![1])),
        ("QAngle", 5, Attribute::QAngle([0.0, 1.0, 2.0])),
        ("QAngle", 1, Attribute::QAngle([0.0, 1.0, 2.0])),
        ("QAngle", 5, Attribute::QAngleArray(vec![[0.0, 1.0, 2.0]])),
    ];

    for (type_name, version, attribute) in cases {
        let mut dm = populated(version);
        dm.elements[0].attributes.push(("extra".into(), attribute));
        match codec::save(&dm) {
            Err(Error::Encode(message)) => assert!(
                message.contains(type_name) && message.contains(&format!("binary {version}")),
                "v{version}: {message:?} must name {type_name}"
            ),
            other => panic!("v{version} {type_name}: expected an encode error, got {other:?}"),
        }
    }
}

/// A `byte[]` in a version without `Byte` is a `binary` blob ("these are the same thing!"), so it
/// comes back as [`Attribute::Binary`]; version 9 keeps the two apart.
#[test]
fn byte_array_before_version_9_is_a_binary_blob() {
    for version in [1, 2, 5] {
        let mut dm = model(version);
        dm.root = Some(ElementId(0));
        dm.elements.push(element("DmeElement", "root", guid(1)));
        dm.elements[0].attributes.push(("bytes".into(), Attribute::ByteArray(vec![7, 8, 9])));

        let back = round_trip(&dm);
        assert_eq!(back.elements[0].attributes, vec![("bytes".to_string(), Attribute::Binary(vec![7, 8, 9]))], "v{version}");

        // An array of `binary` keeps its own id there, so it stays an array.
        dm.elements[0].attributes = vec![("blobs".into(), Attribute::BinaryArray(vec![vec![1], vec![2, 3]]))];
        assert_same_model(&dm, &round_trip(&dm));
    }

    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeElement", "root", guid(1)));
    dm.elements[0].attributes.push(("bytes".into(), Attribute::ByteArray(vec![7, 8, 9])));
    assert_same_model(&dm, &round_trip(&dm));
}

/// An angle in a version before 9 is the id of `Vector3`, so it decodes as one and is written
/// again as one: those two slots share a kind and upstream's encoder always picks the first.
#[test]
fn angle_id_before_version_9_decodes_as_vector3() {
    let mut bytes = file_head("binary", 5, "test", 1);
    push_i32(&mut bytes, 3); // the string table: "root", "DmeElement", "angle"
    raw(&mut bytes, "root");
    raw(&mut bytes, "DmeElement");
    raw(&mut bytes, "angle");
    push_i32(&mut bytes, 1); // one element
    push_i32(&mut bytes, 1); // class index 1 ("DmeElement")
    push_i32(&mut bytes, 0); // name index 0 ("root")
    bytes.extend_from_slice(&guid(1).0);
    push_i32(&mut bytes, 1); // one attribute
    push_i32(&mut bytes, 2); // "angle"
    bytes.push(12); // the angle id of versions 1-8
    for value in [1.0f32, 2.0, 3.0] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    let dm = codec::load(&bytes).expect("load");
    assert_eq!(dm.elements[0].attributes, vec![("angle".to_string(), Attribute::Vector3([1.0, 2.0, 3.0]))]);

    let again = codec::save(&dm).expect("save");
    assert_eq!(again[again.len() - 13], 10, "written again as Vector3's id");
}

/// The version 9 prefix attributes survive a round trip, in the block and, for a model that came
/// out of a file, in the element that carries them (upstream `Binary_PrefixAttributes`).
#[test]
fn prefix_attributes_round_trip() {
    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeElement", "root", guid(1)));
    dm.elements[0].attributes.push(("hello".into(), Attribute::String("world".into())));
    dm.prefix_attributes = vec![
        ("map_asset_references".into(), Attribute::StringArray(vec!["a.vmdl".into(), "b.vmat".into()])),
        ("thumbnail_format".into(), Attribute::String("jpg".into())),
        ("thumbnail".into(), Attribute::Binary(vec![1, 2, 3])),
    ];

    let bytes = codec::save(&dm).expect("save");
    let back = codec::load(&bytes).expect("load");

    assert_eq!(back.prefix_attributes, dm.prefix_attributes, "prefix attributes");
    assert_eq!(back.elements[0].get("hello"), Some(&Attribute::String("world".into())));
    assert_eq!(codec::save(&back).expect("save again"), bytes);
    // The prefix block writes its names and string values in place, before the table exists.
    assert!(bytes.windows(4).any(|window| window == b"jpg\0"), "the block's string value");
    assert_eq!(back.elements.len(), 1, "a model built in code has no prefix element to write");

    // The same attributes in an element right after the root, which is how a file stores them:
    // the element is written as it stands, and a load keeps it where the file put it.
    dm.elements.push(element("DmElement", "", guid(2)));
    dm.elements[1].attributes = dm.prefix_attributes.clone();
    let bytes = codec::save(&dm).expect("save");
    let back = codec::load(&bytes).expect("load");
    assert_eq!(back.elements.len(), 2);
    assert_eq!(back.elements[1].id, guid(2));
    assert_eq!(back.prefix_attributes, dm.prefix_attributes);
    assert_eq!(codec::save(&back).expect("save again"), bytes, "the element is written back in place");
}

/// [`Layout::Upstream`] re-emits the element upstream folds away only for the class upstream
/// folds (`DmElement`); Hammer writes `$prefix_element$`, whose element it does not write at all,
/// because nothing refers to it and the walk never reaches it.
#[test]
fn upstream_layout_only_folds_the_class_it_re_emits() {
    // An element right after the root that looks like the prefix element but has another class.
    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeElement", "root", guid(1)));
    dm.elements.push(element("$prefix_element$", "", guid(2)));
    dm.elements[1].attributes.push(("thumbnail".into(), Attribute::Binary(vec![1, 2, 3])));
    dm.prefix_attributes = vec![("thumbnail".into(), Attribute::Binary(vec![1, 2, 3]))];

    let back = round_trip_upstream(&dm);
    // The walk never reaches the `$prefix_element$` element, so its id is gone; the prefix
    // element in the file is the one upstream re-emits for the block, under the all-zero GUID
    // because a model built in code has no folded element whose id it could keep.
    assert_eq!(back.prefix_attributes, dm.prefix_attributes);
    assert_eq!(back.elements.len(), 2, "the root and the re-emitted prefix element");
    assert_eq!(back.elements[1].class_name, "DmElement");
    assert_eq!(back.elements[1].id, Guid::default());
    assert!(!back.elements.iter().any(|element| element.id == guid(2)), "the `$prefix_element$` element is gone");

    // The class upstream folds, with the keys the block has: the element is written, just under
    // the id it already had rather than a fresh one.
    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeElement", "root", guid(1)));
    dm.elements.push(element("DmElement", "", guid(2)));
    dm.elements[1].attributes.push(("thumbnail".into(), Attribute::Binary(vec![1, 2, 3])));
    dm.prefix_attributes = vec![("thumbnail".into(), Attribute::Binary(vec![1, 2, 3]))];

    let bytes = binary::encode_with(&dm, Layout::Upstream).expect("save");
    let back = codec::load(&bytes).expect("load");
    assert_eq!(back.elements.len(), 2, "the root and the prefix element");
    assert_eq!(back.elements[1].id, guid(2), "the re-emitted element keeps the id it had");
    assert!(!bytes.windows(16).any(|window| window == Guid::default().0), "no invented id");
}

/// The file whose second element holds a key the block does not: the keys differ, so it stays an
/// element of the datamodel.
#[test]
fn prefix_element_fold_requires_upstreams_keys() {
    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeElement", "root", guid(1)));
    dm.prefix_attributes = vec![("one".into(), Attribute::Int(1))];

    let mut bytes = file_head("binary", 9, "test", 1);
    push_i32(&mut bytes, 1); // one prefix element
    push_i32(&mut bytes, 1); // ... with one attribute, "one"
    raw(&mut bytes, "one");
    bytes.push(2); // Int
    push_i32(&mut bytes, 1);
    push_i32(&mut bytes, 5); // five strings: the body writes names as indices
    raw(&mut bytes, "DmeElement"); // 0
    raw(&mut bytes, ""); // 1
    raw(&mut bytes, "root"); // 2
    raw(&mut bytes, "one"); // 3
    raw(&mut bytes, "two"); // 4
    push_i32(&mut bytes, 2); // two elements
    push_i32(&mut bytes, 2); // root: class "root"
    push_i32(&mut bytes, 1); // ... name ""
    bytes.extend_from_slice(&guid(1).0);
    push_i32(&mut bytes, 0); // the second: class "DmeElement"
    push_i32(&mut bytes, 1); // ... name ""
    bytes.extend_from_slice(&guid(2).0);
    push_i32(&mut bytes, 0); // the root has no attributes
    push_i32(&mut bytes, 2); // the second element has two, "one" and "two"
    push_i32(&mut bytes, 3);
    bytes.push(2);
    push_i32(&mut bytes, 1);
    push_i32(&mut bytes, 4);
    bytes.push(2);
    push_i32(&mut bytes, 2);

    let back = codec::load(&bytes).expect("load");
    assert_eq!(back.prefix_attributes.len(), 1, "the block's attributes");
    assert_eq!(back.elements.len(), 2, "the keys differ, so the element is not the prefix element");
    // This file's table is not a Valve table (it has no `name` entry), so it does not come back
    // byte for byte: what has to hold is that a save and a load stay in step from here on.
    let again = codec::save(&back).expect("save");
    assert_same_model(&back, &codec::load(&again).expect("load"));
}

/// A datamodel without a root writes its prefix attributes but no element, and reads back the
/// same way.
#[test]
fn a_datamodel_without_a_root_writes_no_elements() {
    let mut dm = model(9);
    dm.prefix_attributes = vec![("thumbnail_format".into(), Attribute::String("jpg".into()))];

    let bytes = codec::save(&dm).expect("save");
    let back = codec::load(&bytes).expect("load");
    assert!(back.elements.is_empty());
    assert_eq!(back.root, None);
    assert_eq!(back.prefix_attributes, dm.prefix_attributes);
    assert_eq!(back.encoding_version, 9);

    // A stub root writes no elements either, as upstream's `root != null && !root.Stub` guard does.
    let mut dm = model(2);
    dm.elements.push(Element { class_name: "DmeElement".into(), name: "Stub element".into(), id: guid(1), stub: true, attributes: Vec::new() });
    dm.root = Some(ElementId(0));
    let back = codec::load(&codec::save(&dm).expect("save")).expect("load");
    assert!(back.elements.is_empty() && back.root.is_none());
}

/// [`Layout::Upstream`] writes only the elements a reference reaches, in the order they are first
/// reached (upstream's depth-first walk in attribute order); an unreachable element is dropped.
/// [`Layout::Valve`] writes every element, which is what a Valve file holds.
#[test]
fn valve_writes_every_element_and_upstream_only_the_reached_ones() {
    // elements[0] is the root, elements[1] is only reached from elements[2].
    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeRoot", "root", guid(1)));
    dm.elements.push(element("DmeLeaf", "leaf", guid(2)));
    dm.elements.push(element("DmeMiddle", "middle", guid(3)));
    dm.elements.push(element("DmeOrphan", "orphan", guid(4)));
    dm.elements[0].attributes.push(("middle".into(), Attribute::Element(Some(ElementId(2)))));
    dm.elements[2].attributes.push(("leaf".into(), Attribute::Element(Some(ElementId(1)))));
    dm.elements[2].attributes.push(("self".into(), Attribute::Element(Some(ElementId(2)))));
    dm.elements[2].attributes.push(("back".into(), Attribute::Element(Some(ElementId(0)))));

    let back = round_trip_upstream(&dm);
    let written: Vec<Guid> = back.elements.iter().map(|element| element.id).collect();
    assert_eq!(written, vec![guid(1), guid(3), guid(2)], "root, then the elements in first-reach order");
    assert!(!written.contains(&guid(4)), "the orphan is not written");
    assert_eq!(back.elements[1].get("self"), Some(&Attribute::Element(Some(ElementId(1)))), "self-reference");
    assert_eq!(back.elements[1].get("back"), Some(&Attribute::Element(Some(ElementId(0)))), "a reference back to the root");

    // Valve's layout keeps the arena, so the orphan is written and comes back in its own place.
    let back = round_trip(&dm);
    let written: Vec<Guid> = back.elements.iter().map(|element| element.id).collect();
    assert_eq!(written, vec![guid(1), guid(2), guid(3), guid(4)], "every element, in arena order");
    assert_eq!(back.elements[0].get("middle"), Some(&Attribute::Element(Some(ElementId(2)))), "the index did not move");
}

/// A `-2` reference is written as GUID text and decodes into an element marked as a stub with
/// `Element`'s defaults, shared between every reference to the same GUID.
#[test]
fn stubs_are_guid_text_and_share_one_element() {
    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeElement", "root", guid(1)));
    dm.elements.push(Element { class_name: "DmeElement".into(), name: "Stub element".into(), id: guid(9), stub: true, attributes: Vec::new() });
    dm.elements[0].attributes.push(("one".into(), Attribute::Element(Some(ElementId(1)))));
    dm.elements[0].attributes.push(("two".into(), Attribute::Element(Some(ElementId(1)))));
    dm.elements[0].attributes.push(("many".into(), Attribute::ElementArray(vec![Some(ElementId(1)), None])));

    let bytes = codec::save(&dm).expect("save");
    let text = format!("{}\0", guid(9));
    assert!(bytes.windows(text.len()).any(|window| window == text.as_bytes()), "the stub's GUID text");

    let back = codec::load(&bytes).expect("load");
    assert_eq!(back.elements.len(), 2, "the stub becomes one element, however often it is named");
    let stub = &back.elements[1];
    assert!(stub.stub);
    assert_eq!((stub.class_name.as_str(), stub.name.as_str(), stub.id), ("DmeElement", "Stub element", guid(9)));
    assert_eq!(back.elements[0].get("one"), Some(&Attribute::Element(Some(ElementId(1)))));
    assert_eq!(back.elements[0].get("many"), Some(&Attribute::ElementArray(vec![Some(ElementId(1)), None])));
    assert_eq!(codec::save(&back).expect("save again"), bytes);
}

/// The prefix element keeps its place in the arena: the file's element index is arena order, and
/// a load that dropped the element would move every element behind it. Its references stay valid.
#[test]
fn a_load_keeps_the_prefix_element_in_its_place() {
    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeElement", "root", guid(1)));
    dm.elements.push(element("DmElement", "", guid(2)));
    dm.elements.push(element("DmeChild", "child", guid(3)));
    dm.elements.push(element("DmeGrandchild", "grandchild", guid(4)));
    dm.elements[1].attributes.push(("thumbnail_format".into(), Attribute::String("jpg".into())));
    dm.elements[0].attributes.push(("child".into(), Attribute::Element(Some(ElementId(2)))));
    dm.elements[2].attributes.push(("grandchild".into(), Attribute::Element(Some(ElementId(3)))));
    dm.elements[0].attributes.push(("many".into(), Attribute::ElementArray(vec![Some(ElementId(2)), Some(ElementId(3))])));
    dm.prefix_attributes = vec![("thumbnail_format".into(), Attribute::String("jpg".into()))];

    let back = round_trip(&dm);

    assert_eq!(back.elements.len(), 4, "nothing is folded away");
    assert_eq!(back.elements[1].id, guid(2), "the prefix element is where the file has it");
    assert_eq!(back.elements[1].class_name, "DmElement");
    assert_eq!(back.elements[2].id, guid(3), "the child did not move");
    assert_eq!(back.elements[0].get("child"), Some(&Attribute::Element(Some(ElementId(2)))), "reference unchanged");
    assert_eq!(back.elements[2].get("grandchild"), Some(&Attribute::Element(Some(ElementId(3)))));
    assert_eq!(
        back.elements[0].get("many"),
        Some(&Attribute::ElementArray(vec![Some(ElementId(2)), Some(ElementId(3))])),
        "array references unchanged"
    );
    assert_eq!(back.prefix_attributes, dm.prefix_attributes);
}

/// A prefix element something refers to: upstream sets `PrefixElementId` in its fold branch
/// whether or not anything refers to the element (`RemoveUnreferenced` does not look at
/// references), so the re-emitted prefix element carries the element's real id. The reference then
/// resolves to that prefix element, because the id may only be written once. Valve's layout has no
/// fold at all and writes the element once, where it stands.
#[test]
fn a_referred_to_prefix_element_is_kept() {
    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeElement", "root", guid(1)));
    dm.elements.push(element("DmElement", "", guid(2)));
    dm.elements[1].attributes.push(("thumbnail_format".into(), Attribute::String("jpg".into())));
    dm.elements[0].attributes.push(("prefix".into(), Attribute::Element(Some(ElementId(1)))));
    dm.prefix_attributes = vec![("thumbnail_format".into(), Attribute::String("jpg".into()))];

    let back = round_trip_upstream(&dm);
    assert_eq!(back.elements.len(), 2, "the root and the prefix element, which the reference resolves to");
    assert_eq!(back.elements[1].id, guid(2), "the prefix element carries the folded element's real id");
    assert_eq!(back.elements[0].get("prefix"), Some(&Attribute::Element(Some(ElementId(1)))));

    let back = round_trip(&dm);
    assert_eq!(back.elements.len(), 2, "nothing is folded and nothing is invented");
    assert_eq!(back.elements[1].id, guid(2));
    assert_eq!(back.elements[0].get("prefix"), Some(&Attribute::Element(Some(ElementId(1)))));
    assert_eq!(back.prefix_attributes, dm.prefix_attributes);
}

// --------------------------------------------------------------------------- exact bytes

/// A tiny version 9 file whose bytes are derived by hand from upstream's encoder, one comment per
/// field: the header, the prefix block, the string table, the element index and the bodies.
/// [`Layout::Upstream`] is the layout upstream's own encoder writes.
#[test]
fn exact_bytes_binary_9() {
    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeRoot", "root", Guid([0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f])));
    dm.elements.push(element("DmeChild", "", Guid([0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f])));
    dm.elements[0].attributes.push(("value".into(), Attribute::Int(7)));
    dm.elements[0].attributes.push(("child".into(), Attribute::Element(Some(ElementId(1)))));
    dm.elements[1].attributes.push(("flag".into(), Attribute::Bool(true)));

    let mut expected: Vec<u8> = Vec::new();
    expected.extend_from_slice(b"<!-- dmx encoding binary 9 format test 1 -->"); // header line
    expected.push(b'\n');
    expected.push(0); // the header's NUL, which the codec skips to
    push_i32(&mut expected, 0); // prefix block: no prefix elements
    push_i32(&mut expected, 7); // string table: 7 strings (an int32 in version 9)
    raw(&mut expected, "root"); // 0 - the root's name, gathered first
    raw(&mut expected, "DmeRoot"); // 1 - its class name
    raw(&mut expected, "value"); // 2 - the first attribute's name
    raw(&mut expected, "child"); // 3 - the second attribute's name
    raw(&mut expected, ""); // 4 - the child's empty name, gathered when the reference reaches it
    raw(&mut expected, "DmeChild"); // 5 - the child's class name
    raw(&mut expected, "flag"); // 6 - the child's attribute name
    push_i32(&mut expected, 2); // two elements in the index
    push_i32(&mut expected, 1); // root: class index 1 ("DmeRoot")
    push_i32(&mut expected, 0); // ... name index 0 ("root", an index from version 4 on)
    expected.extend_from_slice(&[0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f]);
    push_i32(&mut expected, 5); // child: class index 5 ("DmeChild")
    push_i32(&mut expected, 4); // ... name index 4 ("")
    expected.extend_from_slice(&[0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f]);
    push_i32(&mut expected, 2); // root body: two attributes
    push_i32(&mut expected, 2); // ... "value" (index 2)
    expected.push(2); // ... type id 2 = Int
    push_i32(&mut expected, 7);
    push_i32(&mut expected, 3); // ... "child" (index 3)
    expected.push(1); // ... type id 1 = Element
    push_i32(&mut expected, 1); // ... the child's position in the index
    push_i32(&mut expected, 1); // child body: one attribute
    push_i32(&mut expected, 6); // ... "flag" (index 6)
    expected.push(4); // ... type id 4 = Bool
    expected.push(1); // ... true

    assert_eq!(binary::encode_with(&dm, Layout::Upstream).expect("save"), expected);
    let back = codec::load(&expected).expect("load");
    assert_same_model(&dm, &back);
}

/// A tiny version 2 file: the table length is a `short` but its indices are `short`s too, element
/// names are in place, and so are string values.
#[test]
fn exact_bytes_binary_2() {
    let mut dm = model(2);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeRoot", "root", Guid([0x10; 16])));
    dm.elements.push(element("DmeChild", "", Guid([0x20; 16])));
    dm.elements[0].attributes.push(("value".into(), Attribute::Int(7)));
    dm.elements[0].attributes.push(("text".into(), Attribute::String("hi".into())));
    dm.elements[0].attributes.push(("child".into(), Attribute::Element(Some(ElementId(1)))));
    dm.elements[0].attributes.push(("bin".into(), Attribute::Binary(vec![1, 2, 3])));
    dm.elements[1].attributes.push(("flag".into(), Attribute::Bool(true)));

    let mut expected: Vec<u8> = Vec::new();
    expected.extend_from_slice(b"<!-- dmx encoding binary 2 format test 1 -->");
    expected.push(b'\n');
    expected.push(0);
    push_i16(&mut expected, 10); // string table: 10 strings, as a short before version 4
    raw(&mut expected, "root");
    raw(&mut expected, "DmeRoot");
    raw(&mut expected, "value");
    raw(&mut expected, "text");
    raw(&mut expected, "hi"); // a string value still has a table entry, though it is written in place
    raw(&mut expected, "child");
    raw(&mut expected, ""); // the child, reached from "child"
    raw(&mut expected, "DmeChild");
    raw(&mut expected, "flag");
    raw(&mut expected, "bin"); // the root's remaining attributes, after the child's whole subtree
    push_i32(&mut expected, 2); // two elements
    push_i16(&mut expected, 1); // root: class index 1 ("DmeRoot")
    raw(&mut expected, "root"); // ... element names are in place before version 4
    expected.extend_from_slice(&[0x10; 16]);
    push_i16(&mut expected, 7); // child: class index 7 ("DmeChild")
    raw(&mut expected, ""); // ... its empty name, in place
    expected.extend_from_slice(&[0x20; 16]);
    push_i32(&mut expected, 4); // root body: four attributes
    push_i16(&mut expected, 2); // ... "value"
    expected.push(2); // Int
    push_i32(&mut expected, 7);
    push_i16(&mut expected, 3); // ... "text"
    expected.push(5); // String
    raw(&mut expected, "hi"); // ... in place before version 4
    push_i16(&mut expected, 5); // ... "child"
    expected.push(1); // Element
    push_i32(&mut expected, 1); // the child's position
    push_i16(&mut expected, 9); // ... "bin"
    expected.push(6); // Binary
    push_i32(&mut expected, 3); // three bytes
    expected.extend_from_slice(&[1, 2, 3]);
    push_i32(&mut expected, 1); // child body: one attribute
    push_i16(&mut expected, 8); // ... "flag"
    expected.push(4); // Bool
    expected.push(1);

    assert_eq!(binary::encode_with(&dm, Layout::Upstream).expect("save"), expected);
    let back = codec::load(&expected).expect("load");
    assert_same_model(&dm, &back);
}

/// Version 1 has no string table: every string, attribute names included, is written in place.
#[test]
fn exact_bytes_binary_1() {
    let mut dm = model(1);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeRoot", "root", Guid([0x30; 16])));
    dm.elements[0].attributes.push(("value".into(), Attribute::Int(7)));
    dm.elements[0].attributes.push(("text".into(), Attribute::String("hi".into())));

    let mut expected: Vec<u8> = Vec::new();
    expected.extend_from_slice(b"<!-- dmx encoding binary 1 format test 1 -->");
    expected.push(b'\n');
    expected.push(0);
    push_i32(&mut expected, 1); // one element (there is no string table to write at all)
    raw(&mut expected, "DmeRoot"); // class name, in place
    raw(&mut expected, "root"); // element name, in place
    expected.extend_from_slice(&[0x30; 16]);
    push_i32(&mut expected, 2); // two attributes
    raw(&mut expected, "value"); // attribute name, in place
    expected.push(2); // Int
    push_i32(&mut expected, 7);
    raw(&mut expected, "text");
    expected.push(5); // String
    raw(&mut expected, "hi");

    assert_eq!(binary::encode_with(&dm, Layout::Upstream).expect("save"), expected);
    assert_same_model(&dm, &codec::load(&expected).expect("load"));
}

/// Version 4 writes the table length as an `int` but its indices as `short`s; version 5 writes
/// both as `int`s. Those two widths are what a wrong port gets silently wrong.
#[test]
fn exact_bytes_binary_4_and_5() {
    let build = |version: i32| {
        let mut dm = model(version);
        dm.root = Some(ElementId(0));
        dm.elements.push(element("DmeRoot", "root", Guid([0x40; 16])));
        dm.elements[0].attributes.push(("text".into(), Attribute::String("hi".into())));
        dm
    };

    let mut four: Vec<u8> = file_head("binary", 4, "test", 1);
    push_i32(&mut four, 4); // four strings: the table length is an int32 from version 4
    raw(&mut four, "root");
    raw(&mut four, "DmeRoot");
    raw(&mut four, "text");
    raw(&mut four, "hi"); // a string value still has a table entry
    push_i32(&mut four, 1);
    push_i16(&mut four, 1); // the class index is still a short in version 4
    push_i16(&mut four, 0); // the element name is an index from version 4 on
    four.extend_from_slice(&[0x40; 16]);
    push_i32(&mut four, 1);
    push_i16(&mut four, 2); // the attribute name
    four.push(5); // String
    push_i16(&mut four, 3); // ... written as an index from version 4 on
    assert_eq!(binary::encode_with(&build(4), Layout::Upstream).expect("save"), four);

    let mut five: Vec<u8> = file_head("binary", 5, "test", 1);
    push_i32(&mut five, 4);
    raw(&mut five, "root");
    raw(&mut five, "DmeRoot");
    raw(&mut five, "text");
    raw(&mut five, "hi");
    push_i32(&mut five, 1);
    push_i32(&mut five, 1); // indices are int32s from version 5
    push_i32(&mut five, 0);
    five.extend_from_slice(&[0x40; 16]);
    push_i32(&mut five, 1);
    push_i32(&mut five, 2);
    five.push(5);
    push_i32(&mut five, 3);
    assert_eq!(binary::encode_with(&build(5), Layout::Upstream).expect("save"), five);
}

/// The prefix block writes its strings in place (the table is not there yet) and the prefix
/// element body writes the same names as table indices (upstream's layout, which re-emits an
/// element for attributes a model built in code has no element for).
#[test]
fn exact_bytes_binary_9_prefix_block_and_element() {
    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeRoot", "root", Guid([0x50; 16])));
    dm.prefix_attributes = vec![("fmt".into(), Attribute::String("jpg".into()))];

    let mut expected: Vec<u8> = Vec::new();
    expected.extend_from_slice(b"<!-- dmx encoding binary 9 format test 1 -->");
    expected.push(b'\n');
    expected.push(0);
    push_i32(&mut expected, 1); // one prefix element
    push_i32(&mut expected, 1); // ... with one attribute
    raw(&mut expected, "fmt"); // its name, in place
    expected.push(5); // String
    raw(&mut expected, "jpg"); // ... in place: the table does not exist yet
    push_i32(&mut expected, 6); // the table: root, DmeRoot, "", DmElement, fmt, jpg
    raw(&mut expected, "root");
    raw(&mut expected, "DmeRoot");
    raw(&mut expected, ""); // the prefix element's empty name
    raw(&mut expected, "DmElement"); // its class
    raw(&mut expected, "fmt"); // the prefix attribute's name, added with its value
    raw(&mut expected, "jpg"); // ... and the value, which the prefix element body writes as an index
    push_i32(&mut expected, 2); // two bodies: the root and the prefix element
    push_i32(&mut expected, 1); // root: class index 1
    push_i32(&mut expected, 0); // ... name index 0
    expected.extend_from_slice(&[0x50; 16]);
    push_i32(&mut expected, 3); // the prefix element: class index 3 ("DmElement")
    push_i32(&mut expected, 2); // ... name index 2 ("")
    expected.extend_from_slice(&Guid::default().0); // the all-zero GUID the frozen model has no field for
    push_i32(&mut expected, 0); // the root has no attributes
    push_i32(&mut expected, 1); // the prefix element body has one attribute
    push_i32(&mut expected, 4); // ... "fmt", the same name as a table index here
    expected.push(5); // String
    push_i32(&mut expected, 5); // ... the value "jpg" as a table index too

    assert_eq!(binary::encode_with(&dm, Layout::Upstream).expect("save"), expected);
}

// --------------------------------------------------------------------------- the Valve layout

/// A tiny version 9 file in Valve's layout, whose bytes are derived by hand from the encoder's
/// rule (the module docs): the header, the prefix block, the table — per element its class, its
/// name, its attributes in *reverse* attribute order and then `name` — the element index and the
/// bodies, which still write the attributes in their own order.
#[test]
fn exact_bytes_binary_9_valve() {
    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeRoot", "root", Guid([0x10; 16])));
    dm.elements.push(element("DmeChild", "", Guid([0x20; 16])));
    dm.elements[0].attributes.push(("value".into(), Attribute::Int(7)));
    dm.elements[0].attributes.push(("child".into(), Attribute::Element(Some(ElementId(1)))));
    dm.elements[1].attributes.push(("flag".into(), Attribute::Bool(true)));

    let mut expected: Vec<u8> = Vec::new();
    expected.extend_from_slice(b"<!-- dmx encoding binary 9 format test 1 -->");
    expected.push(b'\n');
    expected.push(0); // the header's NUL
    push_i32(&mut expected, 0); // prefix block: no prefix elements
    push_i32(&mut expected, 8); // string table: 8 strings
    raw(&mut expected, "DmeRoot"); // 0 - the root's class, which Valve interns before the name
    raw(&mut expected, "root"); // 1 - the root's name
    raw(&mut expected, "child"); // 2 - the root's attributes, last one first
    raw(&mut expected, "value"); // 3
    raw(&mut expected, "name"); // 4 - the root's implicit name attribute, created before them all
    raw(&mut expected, "DmeChild"); // 5 - the child, in the arena right after the root
    raw(&mut expected, ""); // 6 - its empty name
    raw(&mut expected, "flag"); // 7 - its attribute
    push_i32(&mut expected, 2); // two elements in the index
    push_i32(&mut expected, 0); // root: class index 0 ("DmeRoot")
    push_i32(&mut expected, 1); // ... name index 1 ("root")
    expected.extend_from_slice(&[0x10; 16]);
    push_i32(&mut expected, 5); // child: class index 5 ("DmeChild")
    push_i32(&mut expected, 6); // ... name index 6 ("")
    expected.extend_from_slice(&[0x20; 16]);
    push_i32(&mut expected, 2); // root body: two attributes, in the model's order
    push_i32(&mut expected, 3); // ... "value" (index 3)
    expected.push(2); // ... Int
    push_i32(&mut expected, 7);
    push_i32(&mut expected, 2); // ... "child" (index 2)
    expected.push(1); // ... Element
    push_i32(&mut expected, 1); // ... the child's position in the index
    push_i32(&mut expected, 1); // child body: one attribute
    push_i32(&mut expected, 7); // ... "flag" (index 7)
    expected.push(4); // ... Bool
    expected.push(1);

    assert_eq!(codec::save(&dm).expect("save"), expected);
    let back = codec::load(&expected).expect("load");
    assert_same_model(&dm, &back);
    assert_eq!(back.binary_string_table, vec!["DmeRoot", "root", "child", "value", "name", "DmeChild", "", "flag"]);
}

/// The two entries Valve's writer interns without a body ever naming them:
/// `$prefix_attribute$`, right after the root's class and name, and the element's implicit `name`
/// attribute after its attributes. The prefix block's own strings are written in place and never
/// interned, so a model whose elements do not carry them has none.
#[test]
fn valve_layout_rebuilds_the_pseudo_attributes() {
    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("CMapRootElement", "", Guid([0x30; 16])));
    dm.elements[0].attributes.push(("first".into(), Attribute::Int(1)));
    dm.prefix_attributes = vec![("fmt".into(), Attribute::String("jpg".into()))];

    let mut expected: Vec<u8> = Vec::new();
    expected.extend_from_slice(b"<!-- dmx encoding binary 9 format test 1 -->");
    expected.push(b'\n');
    expected.push(0);
    push_i32(&mut expected, 1); // one prefix element
    push_i32(&mut expected, 1); // ... with one attribute, written in place
    raw(&mut expected, "fmt");
    expected.push(5); // String
    raw(&mut expected, "jpg");
    push_i32(&mut expected, 5); // the table: class, name, $prefix_attribute$, first, name
    raw(&mut expected, "CMapRootElement"); // 0
    raw(&mut expected, ""); // 1 - the root's name
    raw(&mut expected, "$prefix_attribute$"); // 2 - the pseudo-attribute the block belongs to
    raw(&mut expected, "first"); // 3 - the root's attribute (there is only one to reverse)
    raw(&mut expected, "name"); // 4 - the root's implicit name attribute
    push_i32(&mut expected, 1); // one element (the model has no prefix element of its own)
    push_i32(&mut expected, 0);
    push_i32(&mut expected, 1);
    expected.extend_from_slice(&[0x30; 16]);
    push_i32(&mut expected, 1); // one attribute
    push_i32(&mut expected, 3); // "first"
    expected.push(2); // Int
    push_i32(&mut expected, 1);

    assert_eq!(codec::save(&dm).expect("save"), expected);
}

/// A file whose table holds a string no attribute uses any more (Valve's tables are append-only
/// history) and whose bodies name entries in it: the table comes back as it was, in its own order,
/// and nothing is added, so the file is reproduced byte for byte.
#[test]
fn valve_layout_keeps_the_table_history_a_file_carried() {
    let mut bytes = file_head("binary", 9, "test", 1);
    push_i32(&mut bytes, 0); // no prefix elements
    push_i32(&mut bytes, 5);
    raw(&mut bytes, "DmeRoot"); // 0
    raw(&mut bytes, "root"); // 1
    raw(&mut bytes, "gone"); // 2 - an entry no attribute names any more
    raw(&mut bytes, "value"); // 3
    raw(&mut bytes, "name"); // 4
    push_i32(&mut bytes, 1);
    push_i32(&mut bytes, 0);
    push_i32(&mut bytes, 1);
    bytes.extend_from_slice(&guid(1).0);
    push_i32(&mut bytes, 1);
    push_i32(&mut bytes, 3); // "value"
    bytes.push(2); // Int
    push_i32(&mut bytes, 7);

    let dm = codec::load(&bytes).expect("load");
    assert_eq!(dm.binary_string_table, vec!["DmeRoot", "root", "gone", "value", "name"]);
    assert_eq!(codec::save(&dm).expect("save"), bytes);
}

/// A model whose table does not hold a string it needs has it appended, after every string the
/// table already had, so the entries the file's bodies name keep their index.
#[test]
fn valve_layout_appends_a_missing_string() {
    let new_value = "a longer new value";
    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeRoot", "root", guid(1)));
    dm.elements[0].attributes.push(("text".into(), Attribute::String("old".into())));
    dm.binary_string_table = vec!["DmeRoot".into(), "root".into(), "text".into(), "old".into()];
    let before = codec::save(&dm).expect("save");

    // The same model with the value changed: the old string stays in the table and the new one is
    // appended, so the table and the index keep every entry they had in the same place.
    dm.elements[0].attributes[0] = ("text".into(), Attribute::String(new_value.into()));
    let after = codec::save(&dm).expect("save the edit");

    let count_at = before.iter().position(|byte| *byte == 0).expect("the header's NUL") + 1 + 4;
    let old_table_end = count_at + 4 + dm.binary_string_table.iter().map(|value| value.len() + 1).sum::<usize>();
    assert_eq!(
        after.len(),
        before.len() + new_value.len() + 1,
        "the appended string and its terminator"
    );
    assert_eq!(after[..count_at], before[..count_at], "the header and the prefix block are untouched");
    assert_eq!(
        &after[count_at + 4..old_table_end],
        &before[count_at + 4..old_table_end],
        "the table's entries keep their places"
    );
    assert_eq!(&after[old_table_end..old_table_end + new_value.len() + 1], [new_value.as_bytes(), b"\0"].concat(), "appended");
    assert_eq!(i32::from_le_bytes(after[count_at..count_at + 4].try_into().unwrap()), 5);
    // The rest of the file is the original, except the changed value's index: 3 was the old
    // value's place in the table, 4 is the appended string's.
    let body_at = old_table_end + new_value.len() + 1;
    let moved: Vec<usize> = after[body_at..]
        .iter()
        .zip(&before[old_table_end..])
        .enumerate()
        .filter(|(_, (left, right))| left != right)
        .map(|(offset, _)| offset)
        .collect();
    assert!(!moved.is_empty(), "the changed value has to differ");
    let window_at = moved[0];
    assert!(moved.iter().all(|offset| *offset < window_at + 4), "one int32 moved: {moved:?}");
    assert_eq!(i32::from_le_bytes(after[body_at + window_at..body_at + window_at + 4].try_into().unwrap()), 4);
    assert_eq!(i32::from_le_bytes(before[old_table_end + window_at..old_table_end + window_at + 4].try_into().unwrap()), 3);

    let back = codec::load(&after).expect("load");
    assert_eq!(back.binary_string_table, vec!["DmeRoot", "root", "text", "old", new_value]);
    assert_eq!(back.elements[0].get("text"), Some(&Attribute::String(new_value.into())));
}

// --------------------------------------------------------------------------- hostile input

/// Every truncation of a valid file fails cleanly and nothing panics.
#[test]
fn truncation_at_every_length_is_an_error_and_never_panics() {
    for version in [1, 2, 3, 4, 5, 9] {
        let mut dm = populated(version);
        dm.prefix_attributes = if version >= 9 { vec![("fmt".into(), Attribute::String("jpg".into()))] } else { Vec::new() };
        let bytes = codec::save(&dm).expect("save");

        for length in 0..bytes.len() {
            // The decoder consumes every byte the encoder wrote, so a prefix is always short of
            // something the file promised.
            assert!(codec::load(&bytes[..length]).is_err(), "v{version}: {length} of {} bytes", bytes.len());
        }
        assert!(codec::load(&bytes).is_ok(), "v{version}");
    }

    // Anything at all, without panicking.
    for bytes in [vec![], vec![0], vec![0xff; 64], b"<!-- dmx encoding binary 9 format test 1 -->".to_vec()] {
        let _ = codec::load(&bytes);
    }
}

/// Counts that cannot be honoured are refused before they size anything: a file small enough to
/// hold a handful of items must not ask for a multi-gigabyte table.
#[test]
fn hostile_counts_are_refused() {
    const HUGE: i32 = 0x7fff_ffff;

    // The string table.
    let mut bytes = v9_head();
    push_i32(&mut bytes, HUGE);
    assert!(codec::load(&bytes).is_err(), "string table count");

    // The element count.
    let mut bytes = v9_head();
    push_i32(&mut bytes, 0); // empty string table
    push_i32(&mut bytes, HUGE);
    assert!(codec::load(&bytes).is_err(), "element count");

    // The number of prefix elements, and the attributes of one.
    let mut bytes = file_head("binary", 9, "test", 1);
    push_i32(&mut bytes, HUGE);
    assert!(codec::load(&bytes).is_err(), "prefix element count");

    let mut bytes = file_head("binary", 9, "test", 1);
    push_i32(&mut bytes, 1); // one prefix element
    push_i32(&mut bytes, HUGE);
    assert!(codec::load(&bytes).is_err(), "prefix attribute count");

    // One element, one attribute: the body's attribute count, an array's length, and a binary's
    // length.
    let one_element = |attribute: &[u8]| {
        let mut bytes = v9_head();
        push_i32(&mut bytes, 2); // two strings
        raw(&mut bytes, "root");
        raw(&mut bytes, "DmeElement");
        push_i32(&mut bytes, 1); // one element
        push_i32(&mut bytes, 1); // class "DmeElement"
        push_i32(&mut bytes, 0); // name "root"
        bytes.extend_from_slice(&guid(1).0);
        bytes.extend_from_slice(attribute);
        bytes
    };
    let mut attribute = Vec::new();
    push_i32(&mut attribute, HUGE); // the attribute count itself
    assert!(codec::load(&one_element(&attribute)).is_err(), "attribute count");

    let mut attribute = Vec::new();
    push_i32(&mut attribute, 1); // one attribute
    push_i32(&mut attribute, 0); // "root"
    attribute.push(34); // int array
    push_i32(&mut attribute, HUGE);
    assert!(codec::load(&one_element(&attribute)).is_err(), "array count");

    let mut attribute = Vec::new();
    push_i32(&mut attribute, 1);
    push_i32(&mut attribute, 0);
    attribute.push(6); // binary
    push_i32(&mut attribute, HUGE);
    assert!(codec::load(&one_element(&attribute)).is_err(), "binary length");

    let mut attribute = Vec::new();
    push_i32(&mut attribute, 1);
    push_i32(&mut attribute, 0);
    attribute.push(34);
    push_i32(&mut attribute, 1024); // an array whose items cannot all be there
    assert!(codec::load(&one_element(&attribute)).is_err(), "array longer than the input");
}

/// A negative count fails wherever upstream's own allocator would throw: the string table, the
/// element count, the prefix block, an element's attribute count, an array's count and a `binary`
/// length.
#[test]
fn negative_counts_are_errors() {
    let one_element = |attribute: &[u8]| {
        let mut bytes = v9_head();
        push_i32(&mut bytes, 2);
        raw(&mut bytes, "root");
        raw(&mut bytes, "DmeElement");
        push_i32(&mut bytes, 1);
        push_i32(&mut bytes, 1);
        push_i32(&mut bytes, 0);
        bytes.extend_from_slice(&guid(1).0);
        bytes.extend_from_slice(attribute);
        bytes
    };

    let mut attribute = Vec::new();
    push_i32(&mut attribute, -5); // a negative attribute count
    assert!(codec::load(&one_element(&attribute)).is_err(), "a negative attribute count");

    let mut attribute = Vec::new();
    push_i32(&mut attribute, 1);
    push_i32(&mut attribute, 0);
    attribute.push(34); // int array
    push_i32(&mut attribute, -3);
    assert!(codec::load(&one_element(&attribute)).is_err(), "a negative array count");

    let mut bytes = v9_head();
    push_i32(&mut bytes, -1);
    assert!(codec::load(&bytes).is_err(), "a negative string table count");

    let mut bytes = v9_head();
    push_i32(&mut bytes, 0);
    push_i32(&mut bytes, -1);
    assert!(codec::load(&bytes).is_err(), "a negative element count");

    let mut bytes = file_head("binary", 9, "test", 1);
    push_i32(&mut bytes, -1);
    assert!(codec::load(&bytes).is_err(), "a negative prefix element count");

    let mut attribute = Vec::new();
    push_i32(&mut attribute, 1);
    push_i32(&mut attribute, 0);
    attribute.push(6); // binary
    push_i32(&mut attribute, -1);
    assert!(codec::load(&one_element(&attribute)).is_err(), "a negative binary length");
}

/// Out-of-range references, unknown ids and bad GUID text are errors, not panics.
#[test]
fn bad_references_and_ids_are_errors() {
    let one_element = |attribute: &[u8]| {
        let mut bytes = v9_head();
        push_i32(&mut bytes, 2);
        raw(&mut bytes, "root");
        raw(&mut bytes, "DmeElement");
        push_i32(&mut bytes, 1);
        push_i32(&mut bytes, 1);
        push_i32(&mut bytes, 0);
        bytes.extend_from_slice(&guid(1).0);
        bytes.extend_from_slice(attribute);
        bytes
    };
    let attribute = |id: u8, payload: &[u8]| {
        let mut attribute = Vec::new();
        push_i32(&mut attribute, 1);
        push_i32(&mut attribute, 0);
        attribute.push(id);
        attribute.extend_from_slice(payload);
        attribute
    };

    // A string index past the table.
    let mut payload = Vec::new();
    push_i32(&mut payload, 9);
    assert!(codec::load(&one_element(&attribute(5, &payload))).is_err(), "string index");

    // A negative string index.
    let payload = (-1i32).to_le_bytes();
    assert!(codec::load(&one_element(&attribute(5, &payload))).is_err(), "negative string index");

    // An element index past the index.
    let payload = 7i32.to_le_bytes();
    assert!(codec::load(&one_element(&attribute(1, &payload))).is_err(), "element index");

    // A negative element index that is not -1 or -2.
    let payload = (-3i32).to_le_bytes();
    assert!(codec::load(&one_element(&attribute(1, &payload))).is_err(), "negative element index");

    // Stub GUID text that is not a GUID.
    let mut payload = Vec::new();
    push_i32(&mut payload, -2);
    raw(&mut payload, "not a guid");
    assert!(codec::load(&one_element(&attribute(1, &payload))).is_err(), "stub GUID text");

    // Unknown type ids: the reserved `ObjectID` slot of version 9's neighbours and a value past
    // every id.
    for id in [0u8, 17, 49, 200, 255] {
        assert!(codec::load(&one_element(&attribute(id, &[]))).is_err(), "type id {id}");
    }

    // The same for a version that has no array ids at 17: those are arrays in version 9 only.
    let mut bytes = file_head("binary", 5, "test", 1);
    push_i32(&mut bytes, 0); // no strings
    push_i32(&mut bytes, 0); // no elements
    assert!(codec::load(&bytes).is_ok(), "an empty version 5 file");
    let mut bytes = file_head("binary", 5, "test", 1);
    push_i32(&mut bytes, 0);
    push_i32(&mut bytes, 0);
    bytes.push(17); // trailing data the decoder does not even look at
    assert!(codec::load(&bytes).is_ok(), "trailing bytes are tolerated, as upstream does");
}

/// A GUID the index already holds is a duplicate (upstream's `ElementIdException`).
#[test]
fn duplicate_element_guids_are_errors() {
    let mut bytes = v9_head();
    push_i32(&mut bytes, 2);
    raw(&mut bytes, "root");
    raw(&mut bytes, "DmeElement");
    push_i32(&mut bytes, 2); // two elements
    for _ in 0..2 {
        push_i32(&mut bytes, 1);
        push_i32(&mut bytes, 0);
        bytes.extend_from_slice(&guid(1).0);
    }
    push_i32(&mut bytes, 0); // the first body
    push_i32(&mut bytes, 0); // the second body

    match codec::load(&bytes) {
        Err(Error::Decode { message, .. }) => assert!(message.contains("already in use"), "{message}"),
        other => panic!("expected a duplicate-GUID error, got {other:?}"),
    }
}

/// An attribute name that repeats on one element replaces the first value in place, the way
/// upstream's `AttributeList` indexer does.
#[test]
fn a_repeated_attribute_name_replaces_the_first_value() {
    let mut bytes = v9_head();
    push_i32(&mut bytes, 4);
    raw(&mut bytes, "root");
    raw(&mut bytes, "DmeElement");
    raw(&mut bytes, "value");
    raw(&mut bytes, "other");
    push_i32(&mut bytes, 1);
    push_i32(&mut bytes, 1);
    push_i32(&mut bytes, 0);
    bytes.extend_from_slice(&guid(1).0);
    push_i32(&mut bytes, 3); // three attributes, two of them named "value"
    push_i32(&mut bytes, 2); // "value"
    bytes.push(2); // Int
    push_i32(&mut bytes, 1);
    push_i32(&mut bytes, 3); // "other"
    bytes.push(4); // Bool
    bytes.push(1);
    push_i32(&mut bytes, 2); // "value" again
    bytes.push(2); // Int
    push_i32(&mut bytes, 9);

    let dm = codec::load(&bytes).expect("load");
    assert_eq!(
        dm.elements[0].attributes,
        vec![
            ("value".to_string(), Attribute::Int(9)),
            ("other".to_string(), Attribute::Bool(true)),
        ],
        "the first slot keeps its position and takes the last value"
    );
}

/// Trailing bytes are ignored, which is what upstream's decoder does (it never checks that the
/// stream ended).
#[test]
fn trailing_data_is_ignored_like_upstream() {
    let mut dm = populated(9);
    dm.prefix_attributes = vec![("thumbnail".into(), Attribute::Binary(vec![1, 2, 3]))];
    let bytes = codec::save(&dm).expect("save");

    let mut trailing = bytes.clone();
    trailing.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    let back = codec::load(&trailing).expect("load");
    assert_same_model(&codec::load(&bytes).expect("load"), &back);
    assert_eq!(codec::save(&back).expect("save"), bytes, "the trailing bytes are not written again");
}

/// A version 9 prefix attribute that is an element is refused by both directions; upstream's
/// `PrefixAttributes` cannot hold one either.
#[test]
fn element_prefix_attributes_are_refused() {
    let mut dm = model(9);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeElement", "root", guid(1)));
    dm.prefix_attributes = vec![("element".into(), Attribute::Element(Some(ElementId(0))))];

    match codec::save(&dm) {
        Err(Error::Encode(message)) => assert!(message.contains("prefix attributes"), "{message}"),
        other => panic!("expected an encode error, got {other:?}"),
    }

    // A null element is not a value upstream refuses: it is skipped in the block and written into
    // the prefix element body, so the two disagree and the element cannot be recognised as the
    // prefix element when the file is read again.
    dm.prefix_attributes = vec![("nil".into(), Attribute::Element(None))];
    let bytes = codec::save(&dm).expect("save");
    let back = codec::load(&bytes).expect("load");
    assert_eq!(back.prefix_attributes, Vec::new(), "the block skips the null attribute");
    assert_eq!(back.elements.len(), 1, "a model built in code has no prefix element to disagree with");
    assert!(back.elements[0].get("nil").is_none(), "the root has no such attribute");
    assert_eq!(back.binary_string_table, vec!["DmeElement".to_string(), "root".to_string(), "name".to_string()]);

    // Reading one fails the way storing one does upstream. The element index has not been read
    // yet while the prefix block is, so the only element a prefix attribute can name is a stub.
    let mut bytes = file_head("binary", 9, "test", 1);
    push_i32(&mut bytes, 1);
    push_i32(&mut bytes, 1);
    raw(&mut bytes, "element");
    bytes.push(1); // Element
    push_i32(&mut bytes, -2); // a stub reference
    raw(&mut bytes, &guid(1).to_string());
    match codec::load(&bytes) {
        Err(Error::Decode { message, .. }) => assert!(message.contains("prefix attributes"), "{message}"),
        other => panic!("expected a decode error, got {other:?}"),
    }
}

/// A datamodel format the header grammar cannot hold is refused instead of written.
#[test]
fn blank_or_spaced_formats_are_refused() {
    let mut dm = model(9);
    dm.elements.push(element("DmeElement", "root", guid(1)));
    dm.root = Some(ElementId(0));

    for format in ["", "  ", "my map"] {
        dm.format = format.to_string();
        match codec::save(&dm) {
            Err(Error::Encode(_)) => {}
            other => panic!("format {format:?} must be refused, got {other:?}"),
        }
    }
}

/// An element id the datamodel does not hold is an error rather than a panic.
#[test]
fn out_of_range_element_ids_are_errors() {
    let mut dm = model(2);
    dm.root = Some(ElementId(0));
    dm.elements.push(element("DmeElement", "root", guid(1)));
    dm.elements[0].attributes.push(("bad".into(), Attribute::Element(Some(ElementId(9)))));
    assert!(matches!(codec::save(&dm), Err(Error::Encode(_))));
    assert!(matches!(binary::encode_with(&dm, Layout::Upstream), Err(Error::Encode(_))));

    dm.elements[0].attributes = vec![];
    dm.root = Some(ElementId(9));
    assert!(matches!(codec::save(&dm), Err(Error::Encode(_))));
    assert!(matches!(binary::encode_with(&dm, Layout::Upstream), Err(Error::Encode(_))));
}

/// The codec's public entry points refuse a version the encoding has no table for.
#[test]
fn unsupported_versions_are_refused() {
    let header = Header { encoding: "binary".into(), encoding_version: 7, format: "test".into(), format_version: 1 };
    assert!(matches!(binary::decode(&header, &[0]), Err(Error::Unsupported { version: 7, .. })));

    let mut dm = model(7);
    dm.elements.push(element("DmeElement", "root", guid(1)));
    dm.root = Some(ElementId(0));
    assert!(matches!(binary::encode(&dm), Err(Error::Unsupported { version: 7, .. })));

    // The header-driven entry point refuses it before the codec sees it.
    assert!(matches!(codec::load(b"<!-- dmx encoding binary 7 format test 1 -->\n\0"), Err(Error::Unsupported { .. })));
}

/// Strings decode as UTF-8 with the invalid bytes replaced, and a missing terminator is an error.
#[test]
fn strings_are_utf8_and_unterminated_ones_fail() {
    let mut bytes = v9_head();
    push_i32(&mut bytes, 2);
    bytes.extend_from_slice(&[0xff, 0xfe, 0x41, 0]); // invalid UTF-8, then 'A'
    raw(&mut bytes, "root");
    push_i32(&mut bytes, 1);
    push_i32(&mut bytes, 0); // the class name is the invalid string
    push_i32(&mut bytes, 1); // ... and the name the second entry
    bytes.extend_from_slice(&guid(1).0);
    push_i32(&mut bytes, 0); // no attributes
    let dm = codec::load(&bytes).expect("load");
    assert_eq!(dm.elements[0].class_name, "\u{fffd}\u{fffd}A");
    assert_eq!(dm.elements[0].name, "root");

    // A string that never ends.
    let mut bytes = v9_head();
    push_i32(&mut bytes, 1);
    bytes.extend_from_slice(b"never terminated");
    assert!(codec::load(&bytes).is_err());
}

/// An empty datamodel (no root, no attributes) writes a file with no elements.
#[test]
fn an_empty_datamodel_round_trips() {
    for version in [1, 2, 5, 9] {
        let dm = model(version);
        let bytes = codec::save(&dm).expect("save");
        let back = codec::load(&bytes).expect("load");
        assert!(back.elements.is_empty() && back.root.is_none());
        assert_eq!(back.encoding_version, version);
        assert_eq!(back.format, "test");
        assert_eq!(back.format_version, 1);
    }
}

// --------------------------------------------------------------------------- review fixes

/// A version 2 file declaring one element, whose body is `a` and `b`; `a` and `b` are table entries
/// 0 and 1 and the body's two attributes are written by `body`.
fn one_element_v2(body: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut bytes = file_head("binary", 2, "test", 1);
    push_i16(&mut bytes, 2); // two table strings
    raw(&mut bytes, "a");
    raw(&mut bytes, "b");
    push_i32(&mut bytes, 1); // one declared element
    push_i16(&mut bytes, 0); // class "a"
    raw(&mut bytes, ""); // name, in place before version 4
    bytes.extend_from_slice(&guid(1).0);
    body(&mut bytes);
    bytes
}

/// An element index bounds against the declared element count, not against the arena, which grows
/// with every `-2` stub already read: after a stub, index 1 would otherwise name the stub.
#[test]
fn an_element_index_is_bounded_by_the_declared_count_not_the_stubs() {
    let stub = guid(5).to_string();
    let element_ref = |index: i32| {
        one_element_v2(|bytes| {
            push_i32(bytes, 2); // two attributes
            push_i16(bytes, 0);
            bytes.push(1); // Element
            push_i32(bytes, -2);
            raw(bytes, &stub);
            push_i16(bytes, 1);
            bytes.push(1);
            push_i32(bytes, index);
        })
    };

    let loaded = codec::load(&element_ref(0)).expect("index 0 is the declared element");
    assert_eq!(loaded.elements[0].get("b"), Some(&Attribute::Element(Some(ElementId(0)))));
    assert!(matches!(codec::load(&element_ref(1)), Err(Error::Decode { .. })), "index 1 would be the stub");
}

/// A name the prefix block repeats replaces the value in place, like upstream's `AttributeList`
/// indexer (and like the element body path).
#[test]
fn duplicate_prefix_attribute_names_replace_in_place() {
    let mut bytes = file_head("binary", 9, "test", 1);
    push_i32(&mut bytes, 1); // one prefix element
    push_i32(&mut bytes, 3); // three attributes
    for (name, value) in [("x", 1), ("y", 5), ("x", 2)] {
        raw(&mut bytes, name);
        bytes.push(2); // Int
        push_i32(&mut bytes, value);
    }
    push_i32(&mut bytes, 0); // empty table
    push_i32(&mut bytes, 0); // no elements

    let loaded = codec::load(&bytes).expect("load");
    assert_eq!(
        loaded.prefix_attributes,
        vec![("x".to_string(), Attribute::Int(2)), ("y".to_string(), Attribute::Int(5))]
    );
}

/// Upstream encodes a chain of references without a stack of its own; a long one must not
/// overflow the 2 MB stack of a default thread here.
#[test]
fn the_upstream_layout_walks_a_long_chain_without_recursion() {
    const LENGTH: usize = 60_000;
    let handle = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let mut dm = model(9);
            dm.root = Some(ElementId(0));
            for n in 0..LENGTH {
                let mut id = [0u8; 16];
                id[..8].copy_from_slice(&(n as u64 + 1).to_le_bytes());
                let mut link = element("DmeLink", "link", Guid(id));
                if n + 1 < LENGTH {
                    link.attributes.push(("next".into(), Attribute::Element(Some(ElementId(n + 1)))));
                }
                dm.elements.push(link);
            }
            let bytes = binary::encode_with(&dm, Layout::Upstream).expect("encode");
            let back = codec::load(&bytes).expect("load");
            assert_eq!(back.elements.len(), LENGTH);
            assert_eq!(back.elements[LENGTH - 1].id, dm.elements[LENGTH - 1].id);
        })
        .expect("spawn");
    handle.join().expect("the chain must encode without a stack overflow or a panic");
}

/// The `$prefix_attribute$` entry belongs to the root element, wherever the arena holds it, and a
/// stub in front of it does not move it.
#[test]
fn the_pseudo_prefix_attribute_is_interned_for_the_root() {
    let mut dm = model(9);
    let mut stub = element("DmeElement", "Stub element", guid(9));
    stub.stub = true;
    dm.elements.push(stub);
    dm.elements.push(element("DmeChild", "child", guid(2)));
    dm.elements.push(element("DmeRoot", "root", guid(1)));
    dm.root = Some(ElementId(2));
    dm.prefix_attributes = vec![("fmt".into(), Attribute::Int(1))];

    let bytes = codec::save(&dm).expect("save");
    let back = codec::load(&bytes).expect("load");
    let table = &back.binary_string_table;
    let at = |value: &str| table.iter().position(|entry| entry == value).unwrap_or_else(|| panic!("{value} missing from {table:?}"));
    assert_eq!(table.iter().filter(|entry| *entry == "$prefix_attribute$").count(), 1);
    assert_eq!(at("$prefix_attribute$"), at("root") + 1, "right after the root's class and name: {table:?}");
}
