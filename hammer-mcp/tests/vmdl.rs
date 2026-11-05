//! The `hammer_mcp::vmdl` model layer: tree walk, node paths, edit ops, validation, templates,
//! content paths and the atomic writer.
//!
//! The fixtures under `fixtures/vmdl/` are synthetic files in Valve's own layout (CRLF, tabs, no
//! final newline, `key = ` before a block), written for these tests — the only real `.vmdl` files
//! in the repo are the ones under `cs2-kv3` and the game install, which never get copied here.

use hammer_mcp::vmdl::fsutil;
use hammer_mcp::vmdl::{load_snapshot, template, ContentPaths, Model, NodePath, Op, VmdlError, TEMPLATE_CLASSES};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- fixtures

const STATIC_PROP: &str = include_str!("../fixtures/vmdl/static_prop.vmdl");
const SKELETAL: &str = include_str!("../fixtures/vmdl/skeletal.vmdl");
const BAD_TREE: &str = include_str!("../fixtures/vmdl/bad_tree.vmdl");
const BAD_ROOT: &str = include_str!("../fixtures/vmdl/bad_root.vmdl");
const MISSING_ROOT: &str = include_str!("../fixtures/vmdl/missing_root.vmdl");

fn fixture_texts() -> Vec<(&'static str, &'static str)> {
    vec![
        ("static_prop.vmdl", STATIC_PROP),
        ("skeletal.vmdl", SKELETAL),
        ("bad_tree.vmdl", BAD_TREE),
        ("bad_root.vmdl", BAD_ROOT),
        ("missing_root.vmdl", MISSING_ROOT),
    ]
}

/// A fresh scratch directory under `target/tmp` (never `/tmp`), unique per test.
fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("vmdl-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::canonicalize(&dir).expect("scratch dir canonical")
}

fn parse(text: &str) -> Model {
    Model::parse(text).unwrap_or_else(|error| panic!("{error}"))
}

fn path(text: &str) -> NodePath {
    text.parse().unwrap_or_else(|error| panic!("{text}: {error}"))
}

fn op(json: Value) -> Op {
    serde_json::from_value(json).unwrap_or_else(|error| panic!("{error}"))
}

fn ops_value(json: Value) -> Vec<Op> {
    serde_json::from_value(json).unwrap_or_else(|error| panic!("{error}"))
}

/// The classes of every node, in pre-order.
fn classes(model: &Model) -> Vec<String> {
    model
        .list(None, None, None)
        .into_iter()
        .map(|node| node.class)
        .collect()
}

/// One attribute as JSON (numbers, flags and blobs included).
fn attribute(node: &cs2_kv3::Object, key: &str) -> Value {
    let value = node.get(key).unwrap_or_else(|| panic!("no attribute {key}"));
    cs2_kv3::to_json(value)
}

/// One numeric attribute as JSON.
fn number_of(node: &cs2_kv3::Object, key: &str) -> Value {
    let value = attribute(node, key);
    assert!(value.is_number(), "{key} is not a number: {value}");
    value
}

// ---------------------------------------------------------------- parse / write

#[test]
fn every_fixture_round_trips_byte_for_byte() {
    for (name, text) in fixture_texts() {
        let parsed = Model::parse(text);
        let valid = matches!(name, "static_prop.vmdl" | "skeletal.vmdl");
        let doc = cs2_kv3::parse(text).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(cs2_kv3::write(&doc), text, "{name} does not round-trip byte for byte");
        match (valid, parsed) {
            (true, Err(error)) => panic!("{name} should validate, got {error}"),
            (false, Ok(_)) => panic!("{name} should not validate"),
            _ => {}
        }
    }
}

#[test]
fn model_write_is_the_fixture_bytes() {
    assert_eq!(parse(STATIC_PROP).write(), STATIC_PROP);
    assert_eq!(parse(SKELETAL).write(), SKELETAL);
}

// ---------------------------------------------------------------- node paths

#[test]
fn node_paths_parse_and_display() {
    assert_eq!(path("/").0, Vec::<usize>::new());
    assert_eq!(path("").0, Vec::<usize>::new());
    assert_eq!(path("0/2").0, vec![0, 2]);
    assert_eq!(path("/0/1/2").0, vec![0, 1, 2]);
    assert_eq!(path("/0/1/2").to_string(), "/0/1/2");
    assert_eq!(path("/").to_string(), "/");
    assert_eq!(path("/3/0").last(), Some(0));
    assert_eq!(path("/").last(), None);
    assert_eq!(path("/3/0").parent().map(|p| p.to_string()), Some("/3".to_string()));
    assert_eq!(path("/").parent(), None);
    assert!(path("/3/0").starts_with(&path("/3")));
    assert!(!path("/3/0").starts_with(&path("/0")));

    for bad in ["/0/x", "/0/", "a/b", "-1"] {
        let error = bad.parse::<NodePath>().unwrap_err();
        assert!(matches!(error, VmdlError::Path(_)), "{bad}: {error:?}");
    }
}

#[test]
fn node_lookup_and_errors() {
    let model = parse(STATIC_PROP);
    assert_eq!(model.node(&path("/")).unwrap().get("_class").and_then(cs2_kv3::Value::as_str), Some("RootNode"));
    assert_eq!(
        model.node(&path("/0/0")).unwrap().get("name").and_then(cs2_kv3::Value::as_str),
        Some("body")
    );
    // The path of the deepest node that does exist is reported.
    let error = model.node(&path("/0/0/7/3")).unwrap_err();
    assert_eq!(error.to_string(), "no node at /0/0/7");
    assert!(matches!(error, VmdlError::NodeNotFound(path) if path == "/0/0/7"));
    // A model with no rootNode (only reachable by hand: `parse` rejects it).
    let doc = cs2_kv3::parse(MISSING_ROOT).unwrap();
    let model = Model { doc };
    assert!(matches!(model.node(&path("/")), Err(VmdlError::NodeNotFound(_))));
    assert!(model.root().is_empty());
}

// ---------------------------------------------------------------- list

#[test]
fn list_walks_pre_order_with_paths_and_attrs() {
    let model = parse(STATIC_PROP);
    let nodes = model.list(None, None, None);
    let paths: Vec<&str> = nodes.iter().map(|node| node.path.as_str()).collect();
    assert_eq!(
        paths,
        ["/", "/0", "/0/0", "/0/0/0", "/1", "/1/0", "/2", "/2/0"]
    );
    assert_eq!(
        classes(&model),
        [
            "RootNode",
            "RenderMeshList",
            "RenderMeshFile",
            "Mesh Operation Stack",
            "MaterialGroupList",
            "DefaultMaterialGroup",
            "GameDataList",
            "GenericGameData",
        ]
    );
    assert_eq!(nodes[0].name, None);
    assert_eq!(nodes[2].name.as_deref(), Some("body"));
    assert_eq!(nodes[2].child_count, 1);
    assert_eq!(nodes[2].attr_keys, ["filename", "import_scale", "import_filter"]);
    assert_eq!(nodes[3].child_count, 0);
    assert_eq!(nodes[7].attr_keys, ["game_class", "game_keys"]);
    assert_eq!(nodes[0].child_count, 3);
}

#[test]
fn list_filters_and_depth() {
    let model = parse(SKELETAL);
    let anim_files = model.list(None, Some("AnimFile"), None);
    assert_eq!(anim_files.len(), 1);
    assert_eq!(anim_files[0].path, "/1/0");
    assert_eq!(anim_files[0].name.as_deref(), Some("idle"));

    let named = model.list(None, None, Some("default"));
    assert_eq!(named.len(), 1);
    assert_eq!(named[0].class, "HitboxSet");

    assert!(model.list(None, Some("NoSuchClass"), None).is_empty());

    let shallow: Vec<String> = model
        .list(Some(1), None, None)
        .into_iter()
        .map(|node| node.path)
        .collect();
    assert_eq!(shallow, ["/", "/0", "/1", "/2", "/3", "/4", "/5"]);
    let root_only = model.list(Some(0), None, None);
    assert_eq!(root_only.len(), 1);
    assert_eq!(root_only[0].path, "/");
    assert_eq!(root_only[0].child_count, 6);
}

// ---------------------------------------------------------------- node_json

#[test]
fn node_json_has_the_documented_shape() {
    let model = parse(STATIC_PROP);
    let root = model.node_json(&path("/"), None).unwrap();
    assert_eq!(root["path"], json!("/"));
    assert_eq!(root["_class"], json!("RootNode"));
    assert_eq!(root["name"], Value::Null);
    assert_eq!(root["attrs"], json!({"model_archetype": "", "document_sub_type": "ModelDocSubType_None"}));
    assert_eq!(root["children"].as_array().unwrap().len(), 3);
    assert_eq!(root["children"][0]["_class"], json!("RenderMeshList"));

    let mesh = model.node_json(&path("/0/0"), None).unwrap();
    assert_eq!(mesh["name"], json!("body"));
    assert_eq!(mesh["attrs"]["filename"], json!("models/mcp_probe/body.fbx"));
    assert_eq!(mesh["attrs"]["import_scale"], json!(1.0));
    assert_eq!(
        mesh["attrs"]["import_filter"],
        json!({"exclude_by_default": false, "exception_list": []})
    );
    assert_eq!(mesh["children"][0]["_class"], json!("Mesh Operation Stack"));

    // A flagged/blob-free tree keeps scalars as JSON, including a dotted key.
    let game_data = model.node_json(&path("/2/0"), None).unwrap();
    assert_eq!(game_data["attrs"]["game_keys"]["dmg.bullets"], json!(1.0));
    assert_eq!(game_data["attrs"]["game_keys"]["health"], json!(100));
    assert_eq!(game_data["attrs"]["game_keys"]["base"], json!("Metal.Small"));
}

#[test]
fn node_json_depth_truncates() {
    let model = parse(STATIC_PROP);
    let shallow = model.node_json(&path("/"), Some(1)).unwrap();
    let children = shallow["children"].as_array().unwrap();
    assert_eq!(children.len(), 3);
    for child in children {
        assert_eq!(child["children"], json!([]));
        assert!(child["_class"].is_string());
    }
    let only_root = model.node_json(&path("/"), Some(0)).unwrap();
    assert_eq!(only_root["children"], json!([]));
    assert_eq!(only_root["_class"], json!("RootNode"));

    let error = model.node_json(&path("/9"), None).unwrap_err();
    assert!(matches!(error, VmdlError::NodeNotFound(path) if path == "/9"));
}

// ---------------------------------------------------------------- validate

#[test]
fn validate_accepts_the_good_fixtures() {
    for (name, text) in [("static_prop.vmdl", STATIC_PROP), ("skeletal.vmdl", SKELETAL)] {
        let model = parse(text);
        assert!(model.validate(None).is_empty(), "{name}: {:?}", model.validate(None));
    }
}

#[test]
fn validate_reports_every_structural_rule() {
    let doc = cs2_kv3::parse(BAD_TREE).unwrap();
    let issues = Model { doc }.validate(None);
    let messages: Vec<String> = issues
        .iter()
        .map(|issue| format!("{}: {}", issue.path, issue.msg))
        .collect();
    let has = |needle: &str| messages.iter().any(|message| message.contains(needle));
    assert!(has("/0: _class is missing"), "{messages:?}");
    assert!(has("/1: name is not a string"), "{messages:?}");
    assert!(has("/1: children is not an array"), "{messages:?}");
    assert!(has("/2/0: child is not an object"), "{messages:?}");
    assert!(has("/2/1: _class is not a string"), "{messages:?}");

    // The generic header format and a wrong root class are model-level issues at "/".
    let doc = cs2_kv3::parse(BAD_ROOT).unwrap();
    let issues = Model { doc }.validate(None);
    let messages: Vec<String> = issues.iter().map(|issue| issue.msg.clone()).collect();
    assert!(messages.iter().any(|m| m.contains("is not a modeldoc format")), "{messages:?}");
    assert!(messages.iter().any(|m| m.contains("rootNode._class")), "{messages:?}");

    let doc = cs2_kv3::parse(MISSING_ROOT).unwrap();
    let issues = Model { doc }.validate(None);
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].path, "/");
    assert!(issues[0].msg.contains("rootNode is missing"));

    let doc = cs2_kv3::parse("<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:modeldoc36:version{972dada4-b828-45a4-bb93-7795cf0585da} -->\r\n{\r\n\trootNode = 1\r\n}").unwrap();
    let issues = Model { doc }.validate(None);
    assert_eq!(issues.len(), 1);
    assert!(issues[0].msg.contains("rootNode is not an object"));
}

#[test]
fn model_parse_and_validate_reject_a_broken_file() {
    assert!(matches!(Model::parse(BAD_TREE), Err(VmdlError::Invalid(_))));
    assert!(matches!(Model::parse("not kv3"), Err(VmdlError::Parse(_))));
}

#[test]
fn validate_with_a_class_list_and_validate_classes() {
    let model = parse(STATIC_PROP);
    let known = vec!["RootNode".to_string(), "RenderMeshList".to_string()];
    let issues = model.validate(Some(&known));
    assert_eq!(issues.len(), 6, "{issues:?}");
    assert!(issues[0].msg.contains("unknown _class \"RenderMeshFile\""), "{issues:?}");

    let error = model.validate_classes(&known).unwrap_err();
    assert!(
        matches!(error, VmdlError::UnknownClass { ref path, ref class } if path == "/0/0" && class == "RenderMeshFile"),
        "{error:?}"
    );
    let all: Vec<String> = classes(&model);
    assert!(model.validate_classes(&all).is_ok());
}

// ---------------------------------------------------------------- ops

#[test]
fn op_json_shapes_deserialize() {
    assert_eq!(
        op(json!({"op": "add_node", "parent": "/0", "node": {"_class": "X"}})),
        Op::AddNode {
            parent: "/0".to_string(),
            index: None,
            node: json!({"_class": "X"})
        }
    );
    assert_eq!(
        op(json!({"op": "remove_node", "path": "/0/1"})),
        Op::RemoveNode { path: "/0/1".to_string() }
    );
    assert_eq!(
        op(json!({"op": "move_node", "path": "/0", "parent": "/1", "index": 2})),
        Op::MoveNode {
            path: "/0".to_string(),
            parent: "/1".to_string(),
            index: Some(2)
        }
    );
    assert_eq!(
        op(json!({"op": "set_attr", "path": "/", "key": "k", "value": [1, 2]})),
        Op::SetAttr {
            path: "/".to_string(),
            key: "k".to_string(),
            value: json!([1, 2])
        }
    );
    assert_eq!(
        op(json!({"op": "remove_attr", "path": "/", "key": "k"})),
        Op::RemoveAttr {
            path: "/".to_string(),
            key: "k".to_string()
        }
    );
    assert_eq!(op(json!({"op": "remove_node", "path": "/"})).name(), "remove_node");
    assert!(serde_json::from_value::<Op>(json!({"op": "no_such_op", "path": "/"})).is_err());
}

#[test]
fn add_node_uses_valves_key_order() {
    let mut model = parse(STATIC_PROP);
    let results = model
        .apply(&[op(json!({
            "op": "add_node",
            "parent": "/0",
            "node": {
                "_class": "RenderMeshFile",
                "name": "glass",
                "filename": "models/mcp_probe/glass.fbx",
                "import_scale": 1.0,
                "children": [{"_class": "Mesh Operation Stack", "position": [0.0, 0.0, 0.0]}]
            }
        }))])
        .unwrap();
    assert_eq!(results[0].op, "add_node");
    assert_eq!(results[0].path, "/0/1");

    let added = model.node(&path("/0/1")).unwrap();
    let keys: Vec<&str> = added.keys().collect();
    assert_eq!(
        keys,
        ["_class", "name", "filename", "import_scale", "children"],
        "serde_json order is alphabetical: _class first, name, attrs, children last"
    );
    let meshes = model.list(None, Some("RenderMeshFile"), None);
    assert_eq!(
        meshes.iter().map(|node| node.path.as_str()).collect::<Vec<_>>(),
        ["/0/0", "/0/1"]
    );
    assert_eq!(meshes[1].name.as_deref(), Some("glass"));
    assert_eq!(model.node(&path("/0/1/0")).unwrap().get("_class").and_then(cs2_kv3::Value::as_str), Some("Mesh Operation Stack"));
    assert_eq!(model.validate(None), Vec::new());
}

#[test]
fn add_node_at_index_appends_by_default_and_refuses_bad_nodes() {
    let mut model = parse(STATIC_PROP);
    model
        .apply(&[op(json!({
            "op": "add_node",
            "parent": "/",
            "index": 0,
            "node": {"_class": "BodyGroupList"}
        }))])
        .unwrap();
    assert_eq!(classes(&model)[1], "BodyGroupList");

    let mut model = parse(STATIC_PROP);
    let results = model
        .apply(&[op(json!({"op": "add_node", "parent": "/", "node": {"_class": "LODGroupList"}}))])
        .unwrap();
    assert_eq!(results[0].path, "/3");
    assert_eq!(model.list(None, None, None).last().unwrap().path, "/3");

    // A node that is not an object with `_class`, and an index past the end.
    for (ops, expected) in [
        (json!([{"op": "add_node", "parent": "/", "node": 42}]), "node must be an object"),
        (json!([{"op": "add_node", "parent": "/", "node": {"name": "x"}}]), "needs a string _class"),
        (
            json!([{"op": "add_node", "parent": "/", "index": 9, "node": {"_class": "X"}}]),
            "past the end of /",
        ),
        (
            json!([{"op": "add_node", "parent": "/0/0", "node": {"_class": "X", "children": [1]}}]),
            "children[0]: node must be an object",
        ),
        (
            json!([{"op": "add_node", "parent": "/7", "node": {"_class": "X"}}]),
            "no node at /7",
        ),
    ] {
        let mut model = parse(STATIC_PROP);
        let before = model.write();
        let error = model.apply(&ops_value(ops)).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(matches!(error, VmdlError::BadOp { index: 0, .. }), "{error:?}");
        assert_eq!(model.write(), before, "a refused add_node changed the model");
    }
}

#[test]
fn set_attr_keeps_position_and_goes_before_children() {
    let mut model = parse(STATIC_PROP);
    model
        .apply(&[
            // An existing key stays where it is.
            op(json!({"op": "set_attr", "path": "/0/0", "key": "filename", "value": "models/mcp_probe/other.fbx"})),
            // A new key goes before `children`.
            op(json!({"op": "set_attr", "path": "/0/0", "key": "lod_bias", "value": 0.5})),
        ])
        .unwrap();
    let node = model.node(&path("/0/0")).unwrap();
    let keys: Vec<&str> = node.keys().collect();
    assert_eq!(
        keys,
        ["_class", "name", "filename", "import_scale", "import_filter", "lod_bias", "children"],
        "`children` is moved to the end so the new key sits before it; the old attributes keep their order"
    );
    assert_eq!(node.get("filename").and_then(cs2_kv3::Value::as_str), Some("models/mcp_probe/other.fbx"));

    // A node without `children` gets the key appended.
    model
        .apply(&[op(json!({"op": "set_attr", "path": "/1/0", "key": "extra", "value": null}))])
        .unwrap();
    let keys: Vec<&str> = model.node(&path("/1/0")).unwrap().keys().collect();
    assert_eq!(keys, ["_class", "name", "remaps", "extra"]);
}

#[test]
fn set_attr_converts_json_including_flags_and_blobs() {
    let mut model = parse(STATIC_PROP);
    model
        .apply(&[
            op(json!({"op": "set_attr", "path": "/0/0", "key": "physics", "value": {"$flag": "resource_name", "value": "models/x.vmdl"}})),
            op(json!({"op": "set_attr", "path": "/0/0", "key": "blob", "value": {"$blob": "AB CD"}})),
            op(json!({"op": "set_attr", "path": "/0/0", "key": "scale_vector", "value": [0.0, 0.0, 0.0]})),
        ])
        .unwrap();
    let written = model.write();
    assert!(written.contains("physics = resource_name:\"models/x.vmdl\"\r\n"), "{written}");
    assert!(written.contains("blob = \r\n#[\r\nAB CD\r\n"), "{written}");
    assert!(written.contains("scale_vector = [ 0.0, 0.0, 0.0 ]\r\n"), "{written}");
    // The JSON tagged forms survive the KV3 write as a flag and a blob.
    let node = model.node(&path("/0/0")).unwrap();
    assert_eq!(
        attribute(node, "physics"),
        json!({"$flag": "resource_name", "value": "models/x.vmdl"})
    );
    assert_eq!(attribute(node, "blob"), json!({"$blob": "ABCD"}));
    // And the file still parses back to the same tree.
    assert_eq!(parse(&written).write(), written);
}

#[test]
fn set_and_remove_attr_refuse_structural_keys_and_missing_attributes() {
    for (ops, expected) in [
        (
            json!([{"op": "set_attr", "path": "/", "key": "_class", "value": "X"}]),
            "structural",
        ),
        (
            json!([{"op": "set_attr", "path": "/", "key": "children", "value": []}]),
            "structural",
        ),
        (
            json!([{"op": "remove_attr", "path": "/", "key": "name"}]),
            "structural",
        ),
        (
            json!([{"op": "remove_attr", "path": "/", "key": "nope"}]),
            "has no attribute",
        ),
        (
            json!([{"op": "set_attr", "path": "/3", "key": "k", "value": 1}]),
            "no node at /3",
        ),
    ] {
        let mut model = parse(STATIC_PROP);
        let before = model.write();
        let error = model.apply(&ops_value(ops)).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(model.write(), before);
    }

    let mut model = parse(STATIC_PROP);
    let results = model
        .apply(&[op(json!({"op": "remove_attr", "path": "/0/0", "key": "import_filter"}))])
        .unwrap();
    assert_eq!(results[0].path, "/0/0");
    assert!(model.node(&path("/0/0")).unwrap().get("import_filter").is_none());
}

#[test]
fn remove_node_refuses_the_root_and_reports_the_removed_path() {
    let mut model = parse(STATIC_PROP);
    let error = model.apply(&[op(json!({"op": "remove_node", "path": "/"}))]).unwrap_err();
    assert!(matches!(error, VmdlError::BadOp { index: 0, .. }), "{error:?}");
    assert!(error.to_string().contains("root node"), "{error}");

    let results = model
        .apply(&[op(json!({"op": "remove_node", "path": "/1"}))])
        .unwrap();
    assert_eq!(results[0].path, "/1");
    assert_eq!(classes(&model), ["RootNode", "RenderMeshList", "RenderMeshFile", "Mesh Operation Stack", "GameDataList", "GenericGameData"]);

    let error = model.apply(&[op(json!({"op": "remove_node", "path": "/9"}))]).unwrap_err();
    assert!(error.to_string().contains("no node at /9"), "{error}");
}

#[test]
fn move_node_moves_within_and_across_lists() {
    // Within one list: /0/0 -> index 1 of "/0" (after the node was taken out).
    let mut model = parse(STATIC_PROP);
    let results = model
        .apply(&[op(json!({"op": "move_node", "path": "/0/0", "parent": "/0", "index": 0}))])
        .unwrap();
    assert_eq!(results[0].path, "/0/0");

    model
        .apply(&[op(json!({
            "op": "add_node",
            "parent": "/0",
            "node": {"_class": "RenderMeshFile", "name": "second", "filename": "b.fbx"}
        }))])
        .unwrap();
    assert_eq!(model.list(None, Some("RenderMeshFile"), None).iter().map(|n| n.path.clone()).collect::<Vec<_>>(), ["/0/0", "/0/1"]);
    let results = model
        .apply(&[op(json!({"op": "move_node", "path": "/0/0", "parent": "/0", "index": 1}))])
        .unwrap();
    assert_eq!(results[0].path, "/0/1");
    let names: Vec<Option<String>> = model
        .list(None, Some("RenderMeshFile"), None)
        .into_iter()
        .map(|node| node.name)
        .collect();
    assert_eq!(names, [Some("second".to_string()), Some("body".to_string())]);

    // Across lists, appended by default.
    let mut model = parse(STATIC_PROP);
    let results = model
        .apply(&[op(json!({"op": "move_node", "path": "/0/0/0", "parent": "/1"}))])
        .unwrap();
    assert_eq!(results[0].path, "/1/1");
    assert_eq!(model.node(&path("/1/1")).unwrap().get("_class").and_then(cs2_kv3::Value::as_str), Some("Mesh Operation Stack"));
    assert_eq!(model.list(None, Some("RenderMeshFile"), None)[0].child_count, 0);

    // Into itself, the root, a missing source/destination and a stale index.
    for (ops, expected) in [
        (json!([{"op": "move_node", "path": "/0", "parent": "/0/0"}]),
         "cannot move /0 into itself (/0/0)"),
        (json!([{"op": "move_node", "path": "/", "parent": "/0"}]),
         "root node (/) cannot be moved"),
        (json!([{"op": "move_node", "path": "/4", "parent": "/0"}]),
         "no node at /4"),
        (json!([{"op": "move_node", "path": "/0/0", "parent": "/9"}]),
         "no node at /9"),
        (json!([{"op": "move_node", "path": "/0/0/0", "parent": "/1", "index": 4}]),
         "index 4 is past the end"),
    ] {
        let mut model = parse(STATIC_PROP);
        let before = model.write();
        let error = model.apply(&ops_value(ops)).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(model.write(), before, "a refused move changed the model");
    }
}

#[test]
fn apply_is_atomic() {
    let mut model = parse(STATIC_PROP);
    let before = model.write();
    let error = model
        .apply(&[
            op(json!({"op": "set_attr", "path": "/0/0", "key": "filename", "value": "changed.fbx"})),
            op(json!({"op": "add_node", "parent": "/", "node": {"_class": "AttachmentList"}})),
            op(json!({"op": "remove_node", "path": "/1"})),
            op(json!({"op": "remove_node", "path": "/3"})),
        ])
        .unwrap_err();
    assert!(matches!(error, VmdlError::BadOp { index: 3, .. }), "{error:?}");
    assert_eq!(model.write(), before, "the 3rd op failed but the model changed");
    assert_eq!(classes(&model).len(), 8);

    // The same ops minus the failing one do land.
    model
        .apply(&[
            op(json!({"op": "set_attr", "path": "/0/0", "key": "filename", "value": "changed.fbx"})),
            op(json!({"op": "remove_node", "path": "/1"})),
        ])
        .unwrap();
    assert_eq!(model.node(&path("/0/0")).unwrap().get("filename").and_then(cs2_kv3::Value::as_str), Some("changed.fbx"));
    assert!(!classes(&model).contains(&"MaterialGroupList".to_string()));
}

// ---------------------------------------------------------------- templates

#[test]
fn static_prop_template_parses_validates_and_round_trips() {
    let model = template(
        "static_prop",
        &json!({
            "meshFile": "models/mcp_probe/body.fbx",
            "physicsFile": "models\\mcp_probe\\body_phys.dmx",
            "material": "materials/mcp_probe/body.vmat",
            "lodGroup": {"switch_threshold": 2.5, "meshes": ["body", "body_lod1"]}
        }),
    )
    .unwrap();
    assert_eq!(model.doc.header.format, "modeldoc36");
    assert_eq!(model.doc.header.raw, STATIC_PROP.lines().next().unwrap());
    assert!(model.validate(None).is_empty());

    assert_eq!(
        classes(&model),
        [
            "RootNode",
            "RenderMeshList",
            "RenderMeshFile",
            "PhysicsShapeList",
            "PhysicsHullFile",
            "MaterialGroupList",
            "DefaultMaterialGroup",
            "LODGroupList",
            "LODGroup",
        ]
    );
    let mesh = model.node(&path("/0/0")).unwrap();
    assert_eq!(mesh.get("name").and_then(cs2_kv3::Value::as_str), Some("body"));
    assert_eq!(mesh.get("filename").and_then(cs2_kv3::Value::as_str), Some("models/mcp_probe/body.fbx"));
    assert_eq!(number_of(mesh, "import_scale"), json!(1.0));
    let hull = model.node(&path("/1/0")).unwrap();
    assert_eq!(hull.get("filename").and_then(cs2_kv3::Value::as_str), Some("models/mcp_probe/body_phys.dmx"));
    let group = model.node(&path("/2/0")).unwrap();
    assert_eq!(group.get("use_global_default").and_then(cs2_kv3::Value::as_bool), Some(true));
    assert_eq!(
        group.get("global_default_material").and_then(cs2_kv3::Value::as_str),
        Some("materials/mcp_probe/body.vmat")
    );
    let group = model.node(&path("/3/0")).unwrap();
    assert_eq!(number_of(group, "switch_threshold"), json!(2.5));
    let references = group.get("mesh_references").and_then(cs2_kv3::Value::as_array).unwrap();
    assert_eq!(references.len(), 2);

    // The brief's check: a template's bytes survive parse + write unchanged.
    let written = model.write();
    let again = cs2_kv3::write(&cs2_kv3::parse(&written).unwrap());
    assert_eq!(again, written);
    assert_eq!(parse(&written).write(), written);
}

#[test]
fn static_prop_template_minimal_and_lod_off() {
    let model = template("static_prop", &json!({"meshFile": "models/x/prop.fbx"})).unwrap();
    assert_eq!(
        classes(&model),
        ["RootNode", "RenderMeshList", "RenderMeshFile", "MaterialGroupList", "DefaultMaterialGroup"]
    );
    assert!(model.validate(None).is_empty());
    let written = model.write();
    assert!(written.contains("\t\t\t\t\t\tname = \"prop\"\r\n"), "{written}");
    assert!(!written.contains("LODGroup"), "{written}");

    let on = template("static_prop", &json!({"meshFile": "models/x/prop.fbx", "lodGroup": true})).unwrap();
    assert!(classes(&on).contains(&"LODGroup".to_string()));
    let off = template("static_prop", &json!({"meshFile": "models/x/prop.fbx", "lodGroup": false})).unwrap();
    assert!(!classes(&off).contains(&"LODGroup".to_string()));
}

#[test]
fn skeletal_template_adds_the_skeletal_lists() {
    let model = template(
        "skeletal",
        &json!({"meshFile": "models/x/hero.fbx", "animFiles": ["anims/idle.dmx", "anims\\run.dmx"]}),
    )
    .unwrap();
    assert!(model.validate(None).is_empty());
    let classes = classes(&model);
    for expected in [
        "RootNode",
        "RenderMeshList",
        "RenderMeshFile",
        "MaterialGroupList",
        "DefaultMaterialGroup",
        "AnimationList",
        "AnimFile",
        "BoneMarkupList",
        "HitboxSetList",
        "HitboxSet",
        "AttachmentList",
    ] {
        assert!(classes.contains(&expected.to_string()), "{expected} missing from {classes:?}");
    }
    let anim_files = model.list(None, Some("AnimFile"), None);
    assert_eq!(anim_files.len(), 2);
    assert_eq!(anim_files[1].name.as_deref(), Some("run"));
    assert_eq!(
        model.node(&path("/2/1")).unwrap().get("source_filename").and_then(cs2_kv3::Value::as_str),
        Some("anims/run.dmx")
    );
    assert_eq!(
        model.node(&path("/3")).unwrap().get("bone_cull_type").and_then(cs2_kv3::Value::as_str),
        Some("None")
    );
    assert_eq!(model.node(&path("/4/0")).unwrap().get("name").and_then(cs2_kv3::Value::as_str), Some("default"));
    assert_eq!(model.list(None, Some("AttachmentList"), None)[0].child_count, 0);

    let written = model.write();
    assert_eq!(cs2_kv3::write(&cs2_kv3::parse(&written).unwrap()), written);
}

#[test]
fn template_refuses_bad_arguments() {
    for (kind, params, expected) in [
        ("prop", json!({"meshFile": "models/x/y.fbx"}), "unknown template \"prop\""),
        ("static_prop", json!({}), "params.meshFile is required"),
        ("static_prop", json!({"meshFile": ""}), "params.meshFile must be a non-empty string"),
        ("static_prop", json!({"meshFile": 7}), "params.meshFile must be a non-empty string"),
        ("static_prop", json!({"meshFile": "x.fbx", "physicsFile": 3}), "params.physicsFile"),
        ("static_prop", json!({"meshFile": "x.fbx", "lodGroup": "yes"}), "params.lodGroup must be"),
        ("static_prop", json!({"meshFile": "x.fbx", "lodGroup": {"switch_threshold": "high"}}), "must be a number"),
        ("skeletal", json!({"meshFile": "x.fbx", "animFiles": "idle.dmx"}), "params.animFiles must be an array"),
        ("skeletal", json!({"meshFile": "x.fbx", "animFiles": [7]}), "entries must be non-empty strings"),
    ] {
        let error = template(kind, &params).unwrap_err();
        assert!(matches!(error, VmdlError::Invalid(_)), "{error:?}");
        assert!(error.to_string().contains(expected), "{error}");
    }
    assert!(matches!(template("static_prop", &json!(null)), Err(VmdlError::Invalid(_))));
}

#[test]
fn template_classes_match_the_snapshot_when_there_is_one() {
    let snapshot = Path::new(env!("CARGO_MANIFEST_DIR")).join("research/modeldoc-nodetypes.json");
    let Some(known) = load_snapshot(&snapshot) else {
        println!("skipped: snapshot missing ({})", snapshot.display());
        return;
    };
    assert!(!known.is_empty(), "the snapshot has no classes");
    for class in TEMPLATE_CLASSES {
        assert!(known.iter().any(|candidate| candidate == class), "{class} is not in the node-type snapshot");
    }
}

#[test]
fn template_physics_kind_remaps_and_import_scale() {
    let model = template(
        "static_prop",
        &json!({
            "meshFile": "models/x/x.smd",
            "physicsFile": "models/x/x.smd",
            "physicsKind": "mesh",
            "importScale": 2.5,
            "materialRemaps": [{"from": "wall.vmat", "to": "materials/models/x/wall.vmat"}],
        }),
    )
    .unwrap();
    let text = model.write();
    assert!(text.contains("_class = \"PhysicsMeshFile\""), "{text}");
    assert!(text.contains("import_scale = 2.5"), "{text}");
    assert!(text.contains("from = \"wall.vmat\""), "{text}");
    assert!(text.contains("to = \"materials/models/x/wall.vmat\""), "{text}");
    for bad in [
        json!({"meshFile": "a.smd", "physicsFile": "a.smd", "physicsKind": "sphere"}),
        json!({"meshFile": "a.smd", "materialRemaps": [{"from": "a"}]}),
    ] {
        assert!(template("static_prop", &bad).is_err(), "{bad}");
    }
}

// ---------------------------------------------------------------- content paths

fn content_tree(tag: &str) -> PathBuf {
    let root = scratch(tag);
    for relative in [
        "content/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl",
        "content/csgo/workshop/content_examples/breakable_glass/breakable_glass_01.vmdl",
    ] {
        let file = root.join(relative);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, STATIC_PROP).unwrap();
    }
    root
}

fn content_paths(root: &Path) -> (ContentPaths, cs2_mcp_common::pathmap::PathMap) {
    let map = cs2_mcp_common::pathmap::PathMap::parse(&format!("z:\\app={}", root.display())).unwrap();
    (ContentPaths::new(root, "mcp_test"), map)
}

#[test]
fn resolve_read_accepts_all_three_forms() {
    let root = content_tree("paths-read");
    let (paths, map) = content_paths(&root);
    let expected = root.join("content/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl");
    let forms = [
        expected.display().to_string(),
        "Z:\\app\\content\\csgo_addons\\mcp_test\\models\\mcp_probe\\glass_probe.vmdl".to_string(),
        "z:/app/content/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl".to_string(),
        "csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl".to_string(),
    ];
    for form in forms {
        assert_eq!(paths.resolve_read(&form, &map).unwrap(), expected, "{form}");
    }
    // A core content file is readable.
    assert!(paths
        .resolve_read("csgo/workshop/content_examples/breakable_glass/breakable_glass_01.vmdl", &map)
        .is_ok());
}

#[test]
fn resolve_refuses_dot_dot_extensions_and_unmapped_windows_paths() {
    let root = content_tree("paths-refuse");
    let (paths, map) = content_paths(&root);
    for (input, expected) in [
        ("csgo_addons/../../etc/passwd.vmdl", "contains `..`"),
        ("..\\..\\secrets.vmdl", "contains `..`"),
        ("csgo_addons/mcp_test/models/mcp_probe/glass_probe.txt", "is not a .vmdl file"),
        ("csgo_addons/mcp_test/models/mcp_probe/glass_probe", "is not a .vmdl file"),
        ("Q:\\app\\content\\x.vmdl", "has no host mapping"),
        ("", "path is empty"),
        ("csgo_addons/mcp_test/models/mcp_probe/missing.vmdl", "cannot be read"),
        ("csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl.bak-1", "is not a .vmdl file"),
    ] {
        let error = paths.resolve_read(input, &map).unwrap_err();
        assert!(matches!(error, VmdlError::Path(_)), "{input}: {error:?}");
        assert!(error.to_string().contains(expected), "{input}: {error}");
    }
    // A host path outside the content root cannot be read either.
    let outside = root.join("game/csgo_addons/mcp_test/models/x.vmdl");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::write(&outside, STATIC_PROP).unwrap();
    assert!(paths.resolve_read(&outside.display().to_string(), &map).is_err());
}

#[test]
fn resolve_write_stays_inside_the_addon() {
    let root = content_tree("paths-write");
    let (paths, map) = content_paths(&root);
    let expected = root.join("content/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl");
    assert_eq!(
        paths.resolve_write("csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl", &map).unwrap(),
        expected
    );
    assert_eq!(
        paths
            .resolve_write("Z:\\app\\content\\csgo_addons\\mcp_test\\models\\mcp_probe\\glass_probe.vmdl", &map)
            .unwrap(),
        expected
    );
    // A directory that does not exist yet is fine (model_create makes it).
    assert_eq!(
        paths.resolve_write("csgo_addons/mcp_test/models/mcp_live/run_1.vmdl", &map).unwrap(),
        root.join("content/csgo_addons/mcp_test/models/mcp_live/run_1.vmdl")
    );
    // Core content, another addon, the game tree and an outside host path are refused.
    for input in [
        "csgo/workshop/content_examples/breakable_glass/breakable_glass_01.vmdl".to_string(),
        "csgo_addons/other_addon/models/x.vmdl".to_string(),
        root.join("game/csgo_addons/mcp_test/models/x.vmdl").display().to_string(),
        root.join("content/csgo_addons/mcp_test/../../../../tmp/x.vmdl").display().to_string(),
        root.join("elsewhere/x.vmdl").display().to_string(),
    ] {
        let error = paths.resolve_write(&input, &map).unwrap_err();
        assert!(matches!(error, VmdlError::Path(_)), "{input}: {error:?}");
    }
    let error = paths
        .resolve_write("csgo/workshop/content_examples/breakable_glass/breakable_glass_01.vmdl", &map)
        .unwrap_err();
    assert!(error.to_string().contains("only models of the active addon"), "{error}");
}

#[test]
fn derived_paths() {
    let root = content_tree("paths-derived");
    let (paths, _map) = content_paths(&root);
    let host = root.join("content/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl");
    assert_eq!(
        paths.content_rel(&host).as_deref(),
        Some("csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl")
    );
    assert_eq!(
        paths.container_path(&host).as_deref(),
        Some("/app/content/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl")
    );
    assert_eq!(
        paths.compiled_output(&host),
        Some(root.join("game/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl_c"))
    );
    assert!(paths.in_addon(&host));
    assert_eq!(
        paths.addon_rel(&host).as_deref(),
        Some("models/mcp_probe/glass_probe.vmdl")
    );
    let core = root.join("content/csgo/workshop/content_examples/breakable_glass/breakable_glass_01.vmdl");
    assert!(!paths.in_addon(&core));
    assert_eq!(paths.addon_rel(&core), None);
    assert_eq!(
        paths.content_rel(&core).as_deref(),
        Some("csgo/workshop/content_examples/breakable_glass/breakable_glass_01.vmdl")
    );
    let elsewhere = root.join("game/csgo_addons/mcp_test/models/x.vmdl");
    assert_eq!(paths.content_rel(&elsewhere), None);
    assert_eq!(paths.container_path(&elsewhere), None);
    assert_eq!(paths.compiled_output(&elsewhere), None);
    assert!(!paths.in_addon(&elsewhere));

    // `<app_root>/content` itself is not `None`, it is the empty suffix.
    assert_eq!(paths.content_rel(&root.join("content")), Some(String::new()));
    let other = ContentPaths::new(&root, "addon_template");
    assert!(!other.in_addon(&host));
    assert_eq!(other.content_rel(&host).as_deref(), paths.content_rel(&host).as_deref());
}

// ---------------------------------------------------------------- fsutil

#[test]
fn write_atomic_with_backup_keeps_the_previous_text() {
    let dir = scratch("fsutil-existing");
    let file = dir.join("model.vmdl");
    std::fs::write(&file, STATIC_PROP).unwrap();

    let backup = fsutil::write_atomic_with_backup(&file, "new text").unwrap().expect("backup");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "new text");
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), STATIC_PROP);
    let name = backup.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("model.vmdl.bak-"), "{name}");
    assert_eq!(backup.parent().unwrap(), file.parent().unwrap());

    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // Every write keeps its own backup: a second write (usually in the same second) must not
    // overwrite the backup of the original.
    let again = fsutil::write_atomic_with_backup(&file, "third").unwrap().expect("backup");
    let name = again.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("model.vmdl.bak-"), "{name}");
    assert_ne!(again, backup);
    assert_eq!(std::fs::read_to_string(&again).unwrap(), "new text");
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), STATIC_PROP, "original backup kept");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "third");
}

#[test]
fn write_atomic_with_backup_creates_a_new_file_without_a_backup() {
    let dir = scratch("fsutil-new");
    let file = dir.join("fresh.vmdl");
    assert!(fsutil::write_atomic_with_backup(&file, STATIC_PROP).unwrap().is_none());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), STATIC_PROP);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
}

#[test]
fn write_atomic_with_backup_reports_a_missing_parent_without_a_leftover() {
    let dir = scratch("fsutil-fail");
    let file = dir.join("no-such-dir/model.vmdl");
    let error = fsutil::write_atomic_with_backup(&file, "text").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

// ---------------------------------------------------------------- snapshot

#[test]
fn load_snapshot_reads_the_classes() {
    let dir = scratch("snapshot");
    let snapshot = dir.join("modeldoc-nodetypes.json");
    std::fs::write(&snapshot, r#"{"build":"2000930","classes":["RootNode","RenderMeshFile"]}"#).unwrap();
    assert_eq!(
        load_snapshot(&snapshot),
        Some(vec!["RootNode".to_string(), "RenderMeshFile".to_string()])
    );

    std::fs::write(&snapshot, r#"{"classes":["RootNode",7]}"#).unwrap();
    assert_eq!(load_snapshot(&snapshot), None);
    std::fs::write(&snapshot, "not json").unwrap();
    assert_eq!(load_snapshot(&snapshot), None);
    std::fs::write(&snapshot, r#"{"build":"2000930"}"#).unwrap();
    assert_eq!(load_snapshot(&snapshot), None);
    assert_eq!(load_snapshot(&dir.join("missing.json")), None);
}

// ---------------------------------------------------------------- shipped samples

/// The **real** shipped `.vmdl`s, when a game install is reachable (read-only; nothing is copied
/// into the repo). They are the check that the structural rules are Valve's and not this module's:
/// both files must parse, validate and round-trip, and the edit the live acceptance run performs on
/// the glass probe (`set_attr` on the Voronoi node's `rnd_seed`) must keep the others' quoted
/// keys intact.
#[test]
fn shipped_samples_parse_validate_and_survive_an_edit() {
    let project = std::env::var("CS2_MCP_PROJECT_ROOT")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/..").to_string());
    let content = Path::new(&project).join("workshop/run/app/content");
    if !content.is_dir() {
        println!("skipped: sample missing ({})", content.display());
        return;
    }
    let mut checked = 0;
    for relative in [
        "csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl",
        "csgo/workshop/content_examples/breakable_glass/breakable_glass_01.vmdl",
    ] {
        let sample = content.join(relative);
        if !sample.is_file() {
            println!("skipped: sample missing ({})", sample.display());
            continue;
        }
        let text = std::fs::read_to_string(&sample).expect("utf-8");
        let model = Model::parse(&text).unwrap_or_else(|error| panic!("{relative}: {error}"));
        assert!(model.validate(None).is_empty(), "{relative}: {:?}", model.validate(None));
        assert_eq!(model.write(), text, "{relative} did not round-trip");
        let nodes = model.list(None, None, None);
        assert_eq!(nodes[0].path, "/");
        assert!(nodes[0].child_count > 0, "{relative} has no root children");
        let with_children = nodes.iter().filter(|node| node.child_count > 0).count();
        assert!(with_children > 1, "{relative}: only {with_children} node(s) with children");
        let root = model.node_json(&path("/"), None).unwrap();
        assert_eq!(root["_class"], json!("RootNode"));
        assert!(root["children"][0]["_class"].is_string());

        if relative.ends_with("glass_probe.vmdl") {
            let voronoi = model.list(None, Some("Voronoi Plane Primitive"), None);
            assert_eq!(voronoi.len(), 1, "expected one Voronoi node");
            let target = voronoi[0].path.parse::<NodePath>().unwrap();
            let mut edited = Model::parse(&text).unwrap();
            edited
                .apply(&[op(json!({"op": "set_attr", "path": target.to_string(), "key": "rnd_seed", "value": 7}))])
                .unwrap();
            let written = edited.write();
            assert!(written.contains("\"dmg.bullets\" = -1.0"), "a quoted key lost its quotes");
            assert!(written.contains("\t\t\t\t\t\t\t\trnd_seed = 7\r\n"), "{written}");
            assert_eq!(Model::parse(&written).unwrap().write(), written);
        }
        checked += 1;
    }
    assert!(checked > 0, "no shipped sample found; nothing was verified");
    println!("model layer: checked {checked} shipped sample(s)");
}
