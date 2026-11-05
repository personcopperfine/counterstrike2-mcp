//! `prefab {create, instance, list, read}` (I4) on the mock bridge, plus the `CMapPrefab` node in the
//! `src/vmap` reader and the node listings. The prefab `.vmap` fixture is built with `cs2_dmx`
//! (binary 9); its `CMapPrefab` layout is INFERRED (no Hammer-saved one exists yet, see
//! workflows/import/FIXME.md `## I4`). The temp tree lives under `CARGO_TARGET_TMPDIR`.
#![allow(clippy::result_large_err)]

use cs2_dmx::{codec::binary, Attribute, Datamodel, Element, ElementId, Guid};
use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockBuilder};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);
const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "prefab-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let app = root.join("workshop/run/app");
        std::fs::create_dir_all(app.join("content/csgo_addons/mcp_test/maps")).unwrap();
        Tree { root, app }
    }
    fn addon(&self) -> PathBuf {
        self.app.join("content/csgo_addons/mcp_test")
    }
    fn server(&self, discovery: &std::path::Path) -> McpServer<HammerState> {
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: CommonConfig,
        }
        let root = self.root.display().to_string();
        let map = format!("z:\\app={}", self.app.display());
        let discovery = discovery.display().to_string();
        let given = ["--deadline-ms", "5000", "--project-root", &root, "--path-map", &map, "--discovery", &discovery];
        let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
        hammer_mcp::build_server(&W::parse_from(argv).c).unwrap()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn call(server: &McpServer<HammerState>, name: &str, args: Value) -> Result<(Value, Vec<String>), ToolError> {
    match server.call(name, args).await {
        Ok(out) => {
            let next = out.next.iter().flatten().map(|s| s.tool.clone()).collect();
            Ok((out.json, next))
        }
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

// -------------------------------------------------------------------------------------------
// The fixture: a map holding one entity and one CMapPrefab node
// -------------------------------------------------------------------------------------------

fn guid(n: u8) -> Guid {
    let mut bytes = [0u8; 16];
    bytes[0] = n;
    bytes[15] = 0xA0;
    Guid(bytes)
}

fn element(n: u8, class: &str, attributes: Vec<(&str, Attribute)>) -> Element {
    Element {
        class_name: class.into(),
        name: String::new(),
        id: guid(n),
        stub: false,
        attributes: attributes.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
    }
}

/// root(0) -> world(1) -> [prefab(2), entity(3)]; entity_properties(4).
fn map_with_a_prefab() -> Vec<u8> {
    let elements = vec![
        element(1, "CMapRootElement", vec![("world", Attribute::Element(Some(ElementId(1))))]),
        element(
            2,
            "CMapWorld",
            vec![
                ("nodeID", Attribute::Int(1)),
                ("children", Attribute::ElementArray(vec![Some(ElementId(2)), Some(ElementId(3))])),
            ],
        ),
        element(
            3,
            "CMapPrefab",
            vec![
                ("nodeID", Attribute::Int(7)),
                ("origin", Attribute::Vector3([128.0, 64.0, 0.0])),
                ("angles", Attribute::QAngle([0.0, 45.0, 0.0])),
                ("scales", Attribute::Vector3([1.0, 1.0, 1.0])),
                ("targetMapPath", Attribute::String("maps/prefabs/crate.vmap".into())),
                ("fixupEntityNames", Attribute::Bool(true)),
                ("loadAtRuntime", Attribute::Bool(false)),
            ],
        ),
        element(
            4,
            "CMapEntity",
            vec![
                ("nodeID", Attribute::Int(9)),
                ("origin", Attribute::Vector3([1.0, 2.0, 3.0])),
                ("entity_properties", Attribute::Element(Some(ElementId(4)))),
            ],
        ),
        element(
            5,
            "EditGameClassProps",
            vec![
                ("classname", Attribute::String("info_target".into())),
                ("targetname", Attribute::String("marker".into())),
            ],
        ),
    ];
    let dm = Datamodel {
        encoding: "binary".into(),
        encoding_version: 9,
        format: "vmap".into(),
        format_version: 29,
        elements,
        root: Some(ElementId(0)),
        ..Default::default()
    };
    binary::encode(&dm).expect("encode the fixture")
}

fn write_vmap(tree: &Tree, rel: &str, bytes: &[u8]) {
    let path = tree.addon().join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn the_reader_lists_a_prefab_node_and_keeps_it_out_of_the_entities() {
    let tree = Tree::new("reader");
    write_vmap(&tree, "maps/prefabs/fx.vmap", &map_with_a_prefab());
    let doc = hammer_mcp::vmap::load(&tree.addon().join("maps/prefabs/fx.vmap")).expect("load");
    assert_eq!(doc.prefabs().len(), 1);
    let prefab = &doc.prefabs()[0];
    assert_eq!(prefab.node_id, Some(7));
    assert_eq!(prefab.target_map_path, "maps/prefabs/crate.vmap");
    assert_eq!(prefab.origin, Some([128.0, 64.0, 0.0]));
    assert_eq!(prefab.angles, Some([0.0, 45.0, 0.0]));
    assert_eq!(prefab.scales, Some([1.0, 1.0, 1.0]));
    let json = prefab.to_json();
    assert_eq!(json["class"], "prefab");
    assert_eq!(json["fixupEntityNames"], true);
    assert_eq!(json["loadAtRuntime"], false);
    assert!(json.get("loadIfNested").is_none(), "only the attributes the file carries");
    // an instance is not an entity, and not a mesh parent
    let classes: Vec<&str> = doc.entities().iter().map(|e| e.classname.as_str()).collect();
    assert_eq!(classes, ["info_target"]);
    assert!(doc.meshes().is_empty());
}

// -------------------------------------------------------------------------------------------
// The tool on the mock bridge
// -------------------------------------------------------------------------------------------

fn base(tree: &Tree) -> MockBuilder {
    let _ = tree;
    MockBridge::builder().doc(Some(make_doc(DOC_WIN, Some(false))))
}

/// A bridge with the three prefab methods answering like the live m3.18 (`selection.set` applies
/// everything it is given, unless `filter` drops ids).
fn full(tree: &Tree, loaded: bool) -> MockBuilder {
    base(tree)
        .responder("selection.get", |_| json!({"mode": "objects", "nodeIDs": [4], "count": 1}))
        .responder("selection.set", |p| {
            json!({"requested": {"nodeIDs": 2}, "applied": {"mode": "objects", "nodeIDs": p["nodeIDs"]}})
        })
        .responder("node.prefabFromSelection", move |p| {
            json!({
                "nodeID": 77, "class": "prefab", "targetMapPath": p["path"], "written": true, "loaded": loaded,
                "selectedCount": 2, "centered": p["centered"], "fileStaysOnUndo": true,
                "undoLabel": "MCP: create prefab from selection", "doc": {"id": "d1", "epoch": 1}
            })
        })
        .responder("node.createPrefab", move |p| {
            json!({
                "nodeID": 78, "class": "prefab", "targetMapPath": p["path"], "loaded": loaded,
                "origin": p["origin"], "angles": p.get("angles").cloned().unwrap_or(json!([0, 0, 0])),
                "undoLabel": "MCP: create prefab"
            })
        })
}

#[tokio::test]
async fn create_makes_the_directory_selects_then_writes_the_prefab() {
    let tree = Tree::new("create");
    let mb = full(&tree, true).start().await.unwrap();
    let server = tree.server(&mb.discovery_path);
    assert!(!tree.addon().join("maps/prefabs").exists());
    let (out, next) = call(&server, "prefab", json!({"action": "create", "name": "crate", "nodeIDs": [4, 5, 4], "centered": true}))
        .await
        .unwrap();
    assert!(tree.addon().join("maps/prefabs").is_dir(), "the bridge needs the directory");
    let set = mb.method_calls("selection.set");
    assert_eq!(set.len(), 1);
    assert_eq!(set[0].params["op"], "replace");
    assert_eq!(set[0].params["nodeIDs"], json!([4, 5]), "deduplicated, order kept");
    assert!(set[0].params["expectDoc"].is_object());
    let made = mb.method_calls("node.prefabFromSelection");
    assert_eq!(made.len(), 1);
    assert_eq!(made[0].params["path"], "maps/prefabs/crate.vmap");
    assert_eq!(made[0].params["centered"], true);
    assert!(made[0].params["expectDoc"].is_object());
    // select came first
    let order: Vec<String> = mb.with_state(|s| s.requests.iter().map(|r| r.method.clone()).collect());
    let at = |m: &str| order.iter().position(|x| x == m).unwrap();
    assert!(at("selection.set") < at("node.prefabFromSelection"));

    assert_eq!(out["nodeID"], 77);
    assert_eq!(out["path"], "maps/prefabs/crate.vmap");
    assert_eq!(out["name"], "crate");
    assert_eq!(out["replacedNodeIDs"], json!([4, 5]));
    assert_eq!(out["previousSelection"], json!({"mode": "objects", "nodeIDs": [4]}));
    assert_eq!(out["saved"], false);
    assert!(out.get("doc").is_none());
    assert!(out["note"].as_str().unwrap().contains("REPLACED"), "{out}");
    assert_eq!(next, ["prefab", "save_map"]);
}

#[tokio::test]
async fn create_refuses_an_existing_name_before_any_bridge_call() {
    let tree = Tree::new("exists");
    write_vmap(&tree, "maps/prefabs/crate.vmap", &map_with_a_prefab());
    let mb = full(&tree, true).start().await.unwrap();
    let server = tree.server(&mb.discovery_path);
    let err = call(&server, "prefab", json!({"action": "create", "name": "crate", "nodeIDs": [4]})).await.unwrap_err();
    assert_eq!(err.name, "PrefabExists");
    assert!(err.retry_safe);
    for method in ["selection.set", "selection.get", "node.prefabFromSelection"] {
        assert!(mb.method_calls(method).is_empty(), "{method} must not be called");
    }
}

#[tokio::test]
async fn create_refuses_an_existing_name_in_another_case() {
    let tree = Tree::new("exists-case");
    write_vmap(&tree, "maps/prefabs/Crate.vmap", &map_with_a_prefab());
    let mb = full(&tree, true).start().await.unwrap();
    let server = tree.server(&mb.discovery_path);
    let err = call(&server, "prefab", json!({"action": "create", "name": "crate", "nodeIDs": [4]})).await.unwrap_err();
    assert_eq!(err.name, "PrefabExists");
    assert!(mb.method_calls("selection.set").is_empty());
}

#[tokio::test]
async fn list_honours_limit() {
    let tree = Tree::new("list-limit");
    for n in ["a", "b", "c"] {
        write_vmap(&tree, &format!("maps/prefabs/{n}.vmap"), &map_with_a_prefab());
    }
    let mb = full(&tree, true).start().await.unwrap();
    let server = tree.server(&mb.discovery_path);
    let (out, _) = call(&server, "prefab", json!({"action": "list", "limit": 2})).await.unwrap();
    assert_eq!(out["count"], 3);
    assert_eq!(out["truncated"], true);
    assert_eq!(out["prefabs"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn names_and_arguments_are_validated_locally() {
    let tree = Tree::new("names");
    let mb = full(&tree, true).start().await.unwrap();
    let server = tree.server(&mb.discovery_path);
    for bad in [
        json!({"action": "create", "name": "../evil", "nodeIDs": [1]}),
        json!({"action": "create", "name": "a/b", "nodeIDs": [1]}),
        json!({"action": "create", "name": "Crate", "nodeIDs": [1]}),
        json!({"action": "create", "name": "a.b", "nodeIDs": [1]}),
        json!({"action": "create", "name": "con", "nodeIDs": [1]}),
        json!({"action": "create", "name": "lpt1", "nodeIDs": [1]}),
        json!({"action": "list", "limit": 0}),
        json!({"action": "create", "nodeIDs": [1]}),
        json!({"action": "create", "name": "ok"}),
        json!({"action": "create", "name": "ok", "nodeIDs": []}),
        json!({"action": "create", "name": "ok", "nodeIDs": [-1]}),
        json!({"action": "create", "name": "ok", "nodeIDs": [1], "origin": [0, 0, 0]}),
        json!({"action": "instance", "name": "ok"}),
        json!({"action": "instance", "name": "ok", "path": "maps/prefabs/ok.vmap", "origin": [0, 0, 0]}),
        json!({"action": "instance", "path": "maps/../x.vmap", "origin": [0, 0, 0]}),
        json!({"action": "instance", "path": "x.vmap", "origin": [0, 0, 0]}),
        json!({"action": "instance", "name": "ok", "origin": [0, 0]}),
        json!({"action": "instance", "name": "ok", "origin": [9999999, 0, 0]}),
        json!({"action": "list", "name": "x"}),
        json!({"action": "read"}),
        json!({"action": "dance"}),
        json!({}),
    ] {
        let err = call(&server, "prefab", bad.clone()).await.unwrap_err();
        assert!(
            err.name == "InvalidParams" || err.type_ == "InvalidArguments",
            "{bad}: {}/{}",
            err.type_,
            err.name
        );
    }
    for method in ["selection.set", "node.prefabFromSelection", "node.createPrefab"] {
        assert!(mb.method_calls(method).is_empty(), "{method}");
    }
    assert!(!tree.addon().join("maps/prefabs").exists(), "no directory made by a refused call");
}

#[tokio::test]
async fn a_selection_hammer_filtered_makes_no_prefab() {
    let tree = Tree::new("filtered");
    let mb = full(&tree, true)
        .responder("selection.set", |_| json!({"applied": {"mode": "objects", "nodeIDs": [4]}}))
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb.discovery_path);
    let err = call(&server, "prefab", json!({"action": "create", "name": "crate", "nodeIDs": [4, 5]})).await.unwrap_err();
    assert_eq!(err.name, "SelectionNotApplied");
    assert_eq!(err.data["notSelected"], json!([5]));
    assert!(mb.method_calls("node.prefabFromSelection").is_empty());
}

#[tokio::test]
async fn instance_sends_the_path_origin_and_angles_and_chains_to_look_and_compile() {
    let tree = Tree::new("instance");
    write_vmap(&tree, "maps/prefabs/crate.vmap", &map_with_a_prefab());
    let mb = full(&tree, true).start().await.unwrap();
    let server = tree.server(&mb.discovery_path);
    let (out, next) = call(
        &server,
        "prefab",
        json!({"action": "instance", "name": "crate", "origin": [10, 20, 30], "angles": [0, 90, 0]}),
    )
    .await
    .unwrap();
    let sent = mb.method_calls("node.createPrefab");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].params["path"], "maps/prefabs/crate.vmap");
    assert_eq!(sent[0].params["origin"], json!([10.0, 20.0, 30.0]));
    assert_eq!(sent[0].params["angles"], json!([0.0, 90.0, 0.0]));
    assert!(sent[0].params["expectDoc"].is_object());
    assert_eq!(out["nodeID"], 78);
    assert_eq!(out["saved"], false);
    assert_eq!(next, ["look", "save_map", "compile_map", "run_map"]);
    assert!(!out["note"].as_str().unwrap().contains("WARNING"));

    // by path, no angles; a second instance of the same file is a second node
    let (_, _) = call(&server, "prefab", json!({"action": "instance", "path": "maps/prefabs/crate.vmap", "origin": [0, 0, 0]}))
        .await
        .unwrap();
    let sent = mb.method_calls("node.createPrefab");
    assert_eq!(sent.len(), 2);
    assert!(sent[1].params.get("angles").is_none());
}

#[tokio::test]
async fn instance_warns_when_hammer_did_not_load_the_target() {
    let tree = Tree::new("unloaded");
    write_vmap(&tree, "maps/prefabs/crate.vmap", &map_with_a_prefab());
    let mb = full(&tree, false).start().await.unwrap();
    let server = tree.server(&mb.discovery_path);
    let (out, _) = call(&server, "prefab", json!({"action": "instance", "name": "crate", "origin": [0, 0, 0]})).await.unwrap();
    assert!(out["note"].as_str().unwrap().contains("WARNING loaded:false"), "{out}");
}

#[tokio::test]
async fn instance_of_a_missing_file_names_what_is_available() {
    let tree = Tree::new("missing");
    write_vmap(&tree, "maps/prefabs/crate.vmap", &map_with_a_prefab());
    write_vmap(&tree, "maps/prefabs/sub/lamp.vmap", &map_with_a_prefab());
    let mb = full(&tree, true).start().await.unwrap();
    let server = tree.server(&mb.discovery_path);
    let err = call(&server, "prefab", json!({"action": "instance", "name": "nope", "origin": [0, 0, 0]})).await.unwrap_err();
    assert_eq!(err.name, "PrefabNotFound");
    assert_eq!(err.data["available"], json!(["maps/prefabs/crate.vmap", "maps/prefabs/sub/lamp.vmap"]));
    assert!(mb.method_calls("node.createPrefab").is_empty());
}

#[tokio::test]
async fn list_and_read_go_through_the_vmap_reader() {
    let tree = Tree::new("listread");
    write_vmap(&tree, "maps/prefabs/fx.vmap", &map_with_a_prefab());
    let mb = base(&tree).start().await.unwrap();
    let server = tree.server(&mb.discovery_path);
    // no bridge method is needed for the file routes
    let (list, _) = call(&server, "prefab", json!({"action": "list"})).await.unwrap();
    assert_eq!(list["count"], 1);
    assert_eq!(list["prefabs"][0]["path"], "maps/prefabs/fx.vmap");
    assert_eq!(list["prefabs"][0]["name"], "fx");
    assert_eq!(list["prefabs"][0]["counts"], json!({"entities": 1, "meshes": 0, "prefabs": 1}));

    let (read, next) = call(&server, "prefab", json!({"action": "read", "name": "fx"})).await.unwrap();
    assert_eq!(read["counts"], json!({"entities": 1, "meshes": 0, "prefabs": 1}));
    assert_eq!(read["entities"][0]["classname"], "info_target");
    assert_eq!(read["entities"][0]["nodeID"], 9);
    assert_eq!(read["prefabs"][0]["targetMapPath"], "maps/prefabs/crate.vmap");
    assert_eq!(next, ["prefab"]);
    let err = call(&server, "prefab", json!({"action": "read", "name": "nope"})).await.unwrap_err();
    assert_eq!(err.name, "PrefabNotFound");

    // the node listings show the prefab beside the entities
    let (ents, _) = call(&server, "list_entities", json!({"path": "maps/prefabs/fx.vmap"})).await.unwrap();
    assert_eq!(ents["total"], 1);
    assert_eq!(ents["prefabCount"], 1);
    assert_eq!(ents["prefabs"][0]["targetMapPath"], "maps/prefabs/crate.vmap");
    let (scene, _) = call(&server, "get_scene_info", json!({"source": "saved", "path": "maps/prefabs/fx.vmap"})).await.unwrap();
    assert_eq!(scene["counts"]["prefabs"], 1);
    assert_eq!(scene["prefabs"][0]["nodeID"], 7);
}

#[tokio::test]
async fn a_bridge_without_the_methods_is_not_implemented() {
    let tree = Tree::new("gating");
    write_vmap(&tree, "maps/prefabs/crate.vmap", &map_with_a_prefab());
    let mb = base(&tree).start().await.unwrap();
    let server = tree.server(&mb.discovery_path);
    let err = call(&server, "prefab", json!({"action": "instance", "name": "crate", "origin": [0, 0, 0]})).await.unwrap_err();
    assert_eq!(err.name, "NotImplemented");
    assert_eq!(err.data["missing"], json!(["node.createPrefab"]));
    let err = call(&server, "prefab", json!({"action": "create", "name": "new", "nodeIDs": [1]})).await.unwrap_err();
    assert_eq!(err.name, "NotImplemented");
    let missing = err.data["missing"].as_array().unwrap();
    assert!(missing.contains(&json!("node.prefabFromSelection")), "{missing:?}");
    assert!(!tree.addon().join("maps/prefabs/new.vmap").exists());
}
