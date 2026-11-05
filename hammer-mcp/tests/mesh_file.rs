//! `mesh_file` tool tests (workflow 03 mesh WP): `list_meshes` / `get_mesh` over the SAVED `.vmap`
//! with the committed fixtures and their `*.meshes.json` goldens. Same harness as
//! `tests/entities_file.rs` — the temp content tree, no bridge for the acceptance cases (every one
//! passes `path`), the `#[ignore]`d real-tree test copying a Valve template map at test time.
//!
//! ```sh
//! cargo test -p hammer-mcp --test mesh_file -- --ignored
//! ```
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::tools::{Budget, CallCtx, CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A temp project tree laid out like the real one (the `tests/entities_file.rs` harness).
struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "mesh-file-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("tmp tree");
        let app = root.join("workshop/run/app");
        Tree { root, app }
    }

    fn addon(&self) -> PathBuf {
        self.app.join("content/csgo_addons/mcp_test")
    }

    fn install(&self, fixture: &str, rel: &str) -> PathBuf {
        self.install_from(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/vmap")
                .join(fixture),
            rel,
        )
    }

    /// Copy an existing file (e.g. a real Valve map) into the addon under `rel`. The copy lives
    /// under `CARGO_TARGET_TMPDIR` (never /tmp, never the repo).
    fn install_from(&self, source: &Path, rel: &str) -> PathBuf {
        let target = self.addon().join(rel);
        std::fs::create_dir_all(target.parent().expect("rel has a parent")).expect("install dir");
        std::fs::copy(source, &target).expect("install file");
        target
    }

    /// Read a committed golden (`hammer-mcp/fixtures/vmap/<name>`).
    fn golden(&self, name: &str) -> Value {
        let text = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/vmap")
                .join(name),
        )
        .expect("golden file");
        serde_json::from_str(&text).expect("golden JSON")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn config(tree: &Tree) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let map = format!("z:\\app={}", tree.app.display());
    let given: Vec<&str> = vec![
        "--deadline-ms",
        "5000",
        "--tool-budget-ms",
        "45000",
        "--project-root",
        &root,
        "--path-map",
        &map,
    ];
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

/// [`config`] plus a mock bridge's discovery file (the `doc.active.dirty` / notes tests).
fn config_with_bridge(tree: &Tree, discovery: &Path) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let map = format!("z:\\app={}", tree.app.display());
    let discovery = discovery.display().to_string();
    let given: Vec<&str> = vec![
        "--deadline-ms",
        "5000",
        "--tool-budget-ms",
        "45000",
        "--discovery",
        &discovery,
        "--project-root",
        &root,
        "--path-map",
        &map,
    ];
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

async fn call(
    server: &McpServer<HammerState>,
    name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

fn ctx() -> CallCtx {
    CallCtx {
        budget: Budget::new(45_000, 5_000),
    }
}

/// The addon-relative path form the tools document (`maps/x.vmap`).
const PROBE_ARG: &str = "maps/probe.vmap";
const DEFUSE_ARG: &str = "maps/defuse_poc.vmap";

// -------------------------------------------------------------------------------------------
// list_meshes
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn list_reports_all_probe_meshes() {
    let tree = Tree::new("list");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    let out = call(&server, "list_meshes", json!({"path": PROBE_ARG}))
        .await
        .unwrap();
    assert_eq!(out["count"], json!(18), "probe.vmap has 18 CMapMeshes");
    assert_eq!(out["truncated"], json!(false));
    assert_eq!(out["source"]["fromDisk"], json!(true));
    assert_eq!(out["staleRead"], json!(false));
    assert!(out["source"]["mtime"].is_u64(), "mtime is unix secs");

    let meshes = out["meshes"].as_array().expect("meshes array");
    assert_eq!(meshes.len(), 18);
    let first = &meshes[0];
    assert_eq!(first["nodeID"], json!(3));
    assert_eq!(first["vertexCount"], json!(12));
    assert_eq!(first["faceCount"], json!(10));
    assert_eq!(
        first["materials"],
        json!(["materials/dev/reflectivity_30.vmat"])
    );
    assert_eq!(
        first["bounds"],
        json!({"min": [-256.0, -1024.0, 0.0], "max": [1280.0, 1024.0, 256.0]})
    );

    // limit truncates; the count stays the filter total.
    let out = call(&server, "list_meshes", json!({"path": PROBE_ARG, "limit": 5}))
        .await
        .unwrap();
    assert_eq!(out["count"], json!(18));
    assert_eq!(out["truncated"], json!(true));
    assert_eq!(out["meshes"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn list_within_overlaps_the_bounds_box() {
    let tree = Tree::new("within");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    // nodeID 32's own bounds: a tight box around one brush matches it (plus whichever meshes
    // overlap that space — every match must actually overlap the box)
    let out = call(
        &server,
        "list_meshes",
        json!({
            "path": PROBE_ARG,
            "within": {"min": [640.0, 512.0, 0.0], "max": [896.0, 768.0, 64.0]}
        }),
    )
    .await
    .unwrap();
    let meshes = out["meshes"].as_array().expect("meshes array");
    assert!(
        meshes.iter().any(|m| m["nodeID"] == json!(32)),
        "the box's own mesh matches: {out}"
    );
    for mesh in meshes {
        let (min, max) = (&mesh["bounds"]["min"], &mesh["bounds"]["max"]);
        let box_min = [640.0, 512.0, 0.0];
        let box_max = [896.0, 768.0, 64.0];
        assert!(
            (0..3).all(|axis| min[axis].as_f64().unwrap() <= box_max[axis]
                && max[axis].as_f64().unwrap() >= box_min[axis]),
            "every match overlaps the filter box: {mesh}"
        );
    }

    // a box far away from every mesh
    let out = call(
        &server,
        "list_meshes",
        json!({
            "path": PROBE_ARG,
            "within": {"min": [1e6, 1e6, 0.0], "max": [2e6, 2e6, 0.0]}
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(0));
    assert!(out["meshes"].as_array().unwrap().is_empty());

    // a huge box overlaps everything
    let out = call(
        &server,
        "list_meshes",
        json!({
            "path": PROBE_ARG,
            "within": {"min": [-1e9, -1e9, -1e9], "max": [1e9, 1e9, 1e9]}
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(18));
}

// -------------------------------------------------------------------------------------------
// get_mesh
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn get_probe_node_3_matches_the_golden() {
    let tree = Tree::new("get");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    let out = call(
        &server,
        "get_mesh",
        json!({"path": PROBE_ARG, "nodeID": 3}),
    )
    .await
    .unwrap();
    assert_eq!(out["nodeID"], json!(3));
    assert_eq!(out["source"], json!("file"), "the file route reads the disk");
    assert_eq!(out["staleRead"], json!(false));
    assert_eq!(out["unsavedBridgeEdits"], json!(0));

    // vertices/faces/faceMaterials equal the oracle record (numbers to 1e-9)
    let golden = tree.golden("probe.meshes.json");
    let golden = golden
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["nodeID"] == json!(3))
        .expect("golden record for nodeID 3");
    assert_eq!(out["faces"], golden["faces"]);
    assert_eq!(out["faceMaterials"], golden["faceMaterials"]);
    for (got, want) in out["vertices"]
        .as_array()
        .unwrap()
        .iter()
        .zip(golden["vertices"].as_array().unwrap())
    {
        assert_eq!(got.as_array().unwrap().len(), 3);
        for axis in 0..3 {
            let (a, b) = (
                got[axis].as_f64().unwrap(),
                want[axis].as_f64().unwrap(),
            );
            assert!((a - b).abs() <= 1e-9, "vertex {a} vs golden {b}");
        }
    }

    // faceInfo: per-face centre + normal, one entry per face (POC h_get_mesh)
    let face_info = out["faceInfo"].as_array().expect("faceInfo array");
    assert_eq!(face_info.len(), out["faces"].as_array().unwrap().len());
    for info in face_info {
        let normal = info["normal"].as_array().expect("normal array");
        let length = (normal[0].as_f64().unwrap().powi(2)
            + normal[1].as_f64().unwrap().powi(2)
            + normal[2].as_f64().unwrap().powi(2))
        .sqrt();
        assert!((length - 1.0).abs() < 1e-6, "unit normal, {info}");
        assert_eq!(info["centre"].as_array().unwrap().len(), 3);
    }
}

/// The faceInfo contract the editing tools rely on: on an axis-aligned box exactly one face has
/// its normal pointing straight up, and its material is the mesh's top material.
#[tokio::test]
async fn get_face_info_picks_the_top_face_of_a_box() {
    let tree = Tree::new("top-face");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    let out = call(
        &server,
        "get_mesh",
        json!({"path": PROBE_ARG, "nodeID": 32}),
    )
    .await
    .unwrap();
    let faces = out["faces"].as_array().expect("faces");
    let face_info = out["faceInfo"].as_array().expect("faceInfo");
    let materials = out["faceMaterials"].as_array().expect("faceMaterials");
    assert_eq!(faces.len(), 6, "a box");
    assert_eq!(face_info.len(), 6);
    let top: Vec<usize> = face_info
        .iter()
        .enumerate()
        .filter(|(_, info)| {
            let normal = info["normal"].as_array().expect("normal array");
            info["normal"][2].as_f64().unwrap_or(0.0) > 0.5
                && normal.len() == 3
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(top.len(), 1, "{out}");
    assert_eq!(
        materials[top[0]],
        json!("materials/tools/toolstrigger.vmat")
    );
}

/// A mesh with two materials keeps the per-face override (probe nodeID 106, a toolstrigger/...
/// wedge whose faces mix reflectivity_30 and toolsnodraw).
#[tokio::test]
async fn get_keeps_per_face_materials() {
    let tree = Tree::new("multi-mat");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    let out = call(
        &server,
        "get_mesh",
        json!({"path": PROBE_ARG, "nodeID": 106}),
    )
    .await
    .unwrap();
    assert_eq!(
        out["materials"],
        json!([
            "materials/dev/reflectivity_30.vmat",
            "materials/tools/toolsnodraw.vmat"
        ])
    );
    let overridden = out["faceMaterials"]
        .as_array()
        .expect("faceMaterials")
        .iter()
        .filter_map(Value::as_str)
        .filter(|m| *m == "materials/tools/toolsnodraw.vmat")
        .count();
    assert!(
        overridden > 0,
        "a face is overridden with the second material: {}",
        out["faceMaterials"]
    );
}

// -------------------------------------------------------------------------------------------
// staleRead
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn stale_read_follows_the_unsaved_counter_and_hammers_dirty_flag() {
    use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};

    let tree = Tree::new("stale");
    tree.install("probe.vmap", "maps/probe.vmap");

    // (a) the keep-it-simple rule: unsaved bridge edits > 0 => staleRead true (no bridge, handler
    // called directly — the server's state is not reachable)
    let state = HammerState::from_config(&config(&tree)).unwrap();
    let args = json!({"path": PROBE_ARG});
    let out =
        hammer_mcp::tools::mesh_file::list_meshes(state.clone(), ctx(), args.clone())
            .await
            .unwrap();
    assert_eq!(out.json["staleRead"], json!(false));
    {
        let mut edits = state.edits.lock().unwrap();
        edits.since_save += 2;
    }
    let out = hammer_mcp::tools::mesh_file::list_meshes(state.clone(), ctx(), args.clone())
        .await
        .unwrap();
    assert_eq!(out.json["staleRead"], json!(true));
    assert_eq!(out.json["unsavedBridgeEdits"], json!(2));
    let out = hammer_mcp::tools::mesh_file::get_mesh(
        state,
        ctx(),
        json!({"path": PROBE_ARG, "nodeID": 3}),
    )
    .await
    .unwrap();
    assert_eq!(out.json["staleRead"], json!(true));

    // (b) Hammer's own dirty flag (the user edited the document): the stale read + the POC note
    // on get_mesh's file-sourced geometry
    let mb = MockBridge::builder()
        .doc(Some(make_doc(
            "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap",
            Some(true),
        )))
        .hammer_nodes(vec![MockNode::world()])
        .hammer_mesh_get(false)
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config_with_bridge(&tree, &mb.discovery_path)).unwrap();

    let out = call(&server, "list_meshes", json!({}))
        .await
        .unwrap();
    assert_eq!(out["count"], json!(18), "the omitted-path form reads the active document");
    assert_eq!(out["staleRead"], json!(true));
    assert!(out["staleReason"].as_str().unwrap_or_default().contains("dirty"));
    assert_eq!(mb.method_calls("doc.active").len(), 1, "doc.active is the only bridge use");

    let out = call(&server, "get_mesh", json!({"nodeID": 3})).await.unwrap();
    assert_eq!(out["staleRead"], json!(true));
    assert!(out["staleReason"].as_str().unwrap_or_default().contains("dirty"));
    let notes = out["notes"].as_array().expect("dirty get_mesh carries notes");
    assert!(
        notes[0].as_str().unwrap_or_default().contains("SAVED"),
        "the POC's note names the SAVED-file source: {notes:?}"
    );
}

// -------------------------------------------------------------------------------------------
// Required negatives
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn unknown_node_id_is_mesh_not_found() {
    let tree = Tree::new("not-found");
    tree.install("defuse_poc.vmap", "maps/defuse_poc.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let err = call(
        &server,
        "get_mesh",
        json!({"path": DEFUSE_ARG, "nodeID": 70}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "MeshNotFound");
    assert_eq!(err.data["nodeID"], json!(70));
    assert_eq!(err.data["path"], json!(DEFUSE_ARG));
    assert!(
        err.message.contains("save_map"),
        "the hint names save_map first: {}",
        err.message
    );
    assert!(err.retry_safe);

    // an entity node id is not a mesh
    let err = call(&server, "get_mesh", json!({"path": DEFUSE_ARG, "nodeID": 1}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "MeshNotFound");
}

#[tokio::test]
async fn get_without_node_id_is_invalid_arguments() {
    let tree = Tree::new("get-args");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let err = call(&server, "get_mesh", json!({"path": PROBE_ARG}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert_eq!(err.name, "InvalidParams");
}

#[tokio::test]
async fn node_id_range_is_the_live_route_bound() {
    let tree = Tree::new("nodeid-range");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    for bad in [json!(-1), json!(2_147_483_648i64)] {
        let err = call(&server, "get_mesh", json!({"path": PROBE_ARG, "nodeID": bad}))
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
}

#[tokio::test]
async fn limit_and_within_argument_bounds() {
    let tree = Tree::new("args");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let with = |extra: Value| {
        let mut args = json!({"path": PROBE_ARG});
        let (base, extra) = (args.as_object_mut().unwrap(), extra.as_object().unwrap());
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
        args
    };
    for bad in [
        json!({"limit": 0}),
        json!({"limit": 5001}),
        json!({"limit": "500"}),
    ] {
        let err = call(&server, "list_meshes", with(bad.clone()))
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{err:?}");
    }
    for bad in [
        // truncated max, a wrong axis count, a non-numeric axis: all refused locally
        json!({"within": {"min": [0, 0, 0]}}),
        json!({"within": {"min": [0, 0, 0], "max": [1, 1]}}),
        json!({"within": {"min": [0, 0, "x"], "max": [1, 1, 1]}}),
    ] {
        let err = call(&server, "list_meshes", with(bad.clone()))
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
}

#[tokio::test]
async fn missing_file_is_file_not_found() {
    let tree = Tree::new("missing");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let err = call(&server, "list_meshes", json!({"path": "maps/nope.vmap"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "FileNotFound");
    assert_eq!(err.data["path"], json!("maps/nope.vmap"));
    assert!(err.retry_safe);
}

#[tokio::test]
async fn path_escaping_the_content_root_is_refused() {
    let tree = Tree::new("escape");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let escape = "maps/../../etc/passwd.vmap";
    let err = call(&server, "get_mesh", json!({"path": escape, "nodeID": 1}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidPath");
    assert!(err.message.contains(".."));
}

#[tokio::test]
async fn no_path_without_a_bridge_is_no_document() {
    let tree = Tree::new("no-doc");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    for (tool, args) in [
        ("list_meshes", json!({})),
        ("get_mesh", json!({"nodeID": 3})),
    ] {
        let err = call(&server, tool, args).await.unwrap_err();
        assert_eq!(err.name, "NoDocument", "{tool}");
        assert!(err.message.contains("path"), "{tool}");
        assert!(err.retry_safe);
    }
}

#[tokio::test]
async fn windows_path_form_resolves_through_the_path_map() {
    let tree = Tree::new("win-path");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let out = call(
        &server,
        "list_meshes",
        json!({"path": "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap"}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(18));
    assert_eq!(
        out["source"]["hostPath"],
        json!(tree.addon().join("maps/probe.vmap").display().to_string())
    );
}

// -------------------------------------------------------------------------------------------
// The real game tree (#[ignore])
// -------------------------------------------------------------------------------------------

/// `list_meshes` through the tool against a Valve template map: the oracle's counts and summaries.
#[tokio::test]
#[ignore]
async fn real_tree_template_wingman_meshes_through_list_meshes() {
    let source = real_tree_file(TEMPLATE_WINGMAN);
    let tree = Tree::new("real-meshes");
    tree.install_from(&source, "maps/template_wingman.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    let out = call(&server, "list_meshes", json!({"path": "maps/template_wingman.vmap"}))
        .await
        .unwrap();
    assert_eq!(out["count"], json!(18), "oracle: 18 usable CMapMeshes");
    assert_eq!(out["staleRead"], json!(false));
    for mesh in out["meshes"].as_array().expect("meshes") {
        assert!(mesh["nodeID"].is_number());
        assert!(
            mesh["faceCount"].as_u64().unwrap_or(0) >= 3,
            "every mesh has at least 3 faces: {mesh}"
        );
    }
}

// -------------------------------------------------------------------------------------------
// Review M2/L4: get_mesh and the editing tools agree on the face indices
// -------------------------------------------------------------------------------------------

/// The active document's own `.vmap` (the mesh_live mock's DOC path).
const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";
const NODRAW: &str = "materials/tools/toolsnodraw.vmat";
const WATER: &str = "materials/dev/water.vmat";

/// M2: for the ACTIVE document, an omitted `path` reports the geometry Hammer holds now (`mesh.get`,
/// `source:"live"`) — the geometry and face order `set_mesh_material {faces}` and the rebuild
/// operators index into — and only falls back to the SAVED file when the bridge has no `mesh.get`.
/// Reporting the file's own (Hammer-welded) loop order instead hands the caller indices that mean
/// a different face.
#[tokio::test]
async fn get_mesh_without_a_path_reads_the_live_geometry() {
    use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
    use std::collections::BTreeMap;

    let tree = Tree::new("get-cache");
    tree.install("probe.vmap", "maps/probe.vmap");
    // nodeID 3 exists as a mesh in the document AND in the file (12 vertices / 10 faces there)
    let mesh_node = MockNode {
        node_id: 3,
        class: "mesh".into(),
        classname: String::new(),
        targetname: None,
        origin: [0.0; 3],
        angles: [0.0; 3],
        keyvalues: BTreeMap::new(),
        locked: false,
        parent: None,
    };
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(vec![MockNode::world(), mesh_node])
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config_with_bridge(&tree, &mb.discovery_path)).unwrap();

    // this server writes nodeID 3's geometry: 4 vertices / 1 quad, nothing like the file's nodeID 3
    call(
        &server,
        "set_mesh_geometry",
        json!({"nodeID": 3,
               "vertices": [[0, 0, 0], [64, 0, 0], [64, 64, 0], [0, 64, 0]],
               "faces": [[0, 1, 2, 3]],
               "material": NODRAW}),
    )
    .await
    .unwrap();

    let cached = call(&server, "get_mesh", json!({"nodeID": 3}))
        .await
        .unwrap();
    assert_eq!(cached["source"], json!("live"), "{cached}");
    assert_eq!(cached["staleRead"], json!(false), "a live read is never stale: {cached}");
    assert_eq!(cached["vertexCount"], json!(4));
    assert_eq!(cached["faceCount"], json!(1));
    assert_eq!(cached["faces"], json!([[0, 1, 2, 3]]));
    assert_eq!(cached["faceMaterials"], json!([NODRAW]));
    assert_eq!(cached["materials"], json!([NODRAW]));
    assert_eq!(
        cached["bounds"],
        json!({"min": [0.0, 0.0, 0.0], "max": [64.0, 64.0, 0.0]})
    );
    let info = cached["faceInfo"].as_array().expect("faceInfo");
    assert_eq!(info.len(), 1);
    assert!(
        info[0]["normal"][2].as_f64().unwrap_or(0.0) > 0.5,
        "the quad's normal is +z: {cached}"
    );

    // the SAVED file's nodeID 3 is a different mesh (the golden's 12 vertices / 10 faces), which is
    // exactly the mismatch M2 is about: two views, one face index space (the live one is the editor's)
    let file = call(
        &server,
        "get_mesh",
        json!({"path": PROBE_ARG, "nodeID": 3}),
    )
    .await
    .unwrap();
    assert_eq!(file["source"], json!("file"));
    assert_eq!(file["vertexCount"], json!(12));
    assert_eq!(file["faceCount"], json!(10));

    // and the per-face route takes the indices the cached view reported, rebuilding from the same
    // numbers (read live again)
    let out = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": 3, "material": WATER, "faces": [0]}),
    )
    .await
    .unwrap();
    assert_eq!(out["geometrySource"], json!("live"), "{out}");
    let rebuild = mb.method_calls("mesh.setGeometry");
    assert_eq!(rebuild.len(), 2, "the write plus the per-face rebuild");
    assert_eq!(
        rebuild[1].params["vertices"], cached["vertices"],
        "the rebuild sends the geometry get_mesh reported, so the index means the same face"
    );
}

/// L4: the dirty-document note belongs to the *active* document's file. Reading another file with
/// `path` while the active document is dirty must not warn about that document's unsaved edits, and
/// the note says the edits are **not included** (`get_mesh` replaces nothing; the POC's "are
/// replaced" wording belongs to its editing paths).
#[tokio::test]
async fn the_dirty_note_is_only_for_the_active_document() {
    use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};

    let tree = Tree::new("dirty-note");
    tree.install("probe.vmap", "maps/probe.vmap");
    tree.install("defuse_poc.vmap", "maps/defuse_poc.vmap");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(vec![MockNode::world()])
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config_with_bridge(&tree, &mb.discovery_path)).unwrap();

    // another file: no note at all (the active document's unsaved edits are not this file's)
    let other = call(
        &server,
        "get_mesh",
        json!({"path": DEFUSE_ARG, "nodeID": 71}),
    )
    .await
    .unwrap();
    assert_eq!(other["source"], json!("file"));
    assert_eq!(other["nodeID"], json!(71));
    assert!(
        other["notes"].is_null(),
        "a file that is not the active document carries no dirty note: {other}"
    );

    // the active document's own file: the note, worded for a read
    let active = call(
        &server,
        "get_mesh",
        json!({"path": PROBE_ARG, "nodeID": 3}),
    )
    .await
    .unwrap();
    assert_eq!(active["staleRead"], json!(true));
    let notes = active["notes"].as_array().expect("the dirty note");
    assert!(
        notes[0].as_str().unwrap_or_default().contains("not included"),
        "the note says the edits are not included, not that they are replaced: {notes:?}"
    );
}

/// The project root (`tests/entities_file.rs` rule).
fn project_root() -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("CS2_MCP_PROJECT_ROOT") {
        candidates.push(PathBuf::from(root));
    }
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        candidates.push(dir.clone());
        if !dir.pop() {
            break;
        }
    }
    candidates
        .into_iter()
        .find(|dir| dir.join(CSGO_FGD).is_file())
        .unwrap_or_else(|| {
            panic!(
                "no project root with {CSGO_FGD} at $CS2_MCP_PROJECT_ROOT or above {}",
                env!("CARGO_MANIFEST_DIR")
            )
        })
}

fn real_tree_file(relative: &str) -> PathBuf {
    let path = project_root().join(relative);
    assert!(
        path.is_file(),
        "missing real map {}: the installed game tree moved?",
        path.display()
    );
    path
}

/// The tree marker (same as the other real-tree tests).
const CSGO_FGD: &str = "workshop/run/app/game/csgo/csgo.fgd";
/// A Valve template map with 18 usable CMapMeshes.
const TEMPLATE_WINGMAN: &str = "workshop/run/app/content/csgo/maps/templates/template_wingman.vmap";
