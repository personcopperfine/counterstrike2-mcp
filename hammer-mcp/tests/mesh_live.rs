//! `mesh_live` tool tests (WP-B): `create_mesh`, `create_block`, `set_mesh_geometry` and
//! `set_mesh_material` against the mock bridge's Hammer world (`mesh.create` / `mesh.setGeometry` /
//! `mesh.setMaterial`, WP-B's arms), with the geometry itself from `hammer_mcp::geometry` (the port
//! of the POC's `geometry.py`, unit-tested in `src/geometry.rs`).
//!
//! What these tests pin, beyond the happy path:
//!
//! * the **request** the bridge gets: rounded vertices, index loops, the POC's material split
//!   (`material` + `faceMaterials` with `null` = the base), `expectDoc` and a positive `deadlineMs`;
//! * the **result** the POC's `_send_geometry` builds (bounds of the sent vertices, the bridge's own
//!   `undoLabel`, the `undoNote`, the unsaved counter) plus the bridge's `vertexSlots`/`faceSlots`;
//! * local validation sends **nothing** (a bad mesh, a zero-area face, an out-of-range coordinate,
//!   a material the bridge's buffer cannot hold, an oversized request);
//! * error mapping: the bridge's target conditions (`MeshNotFound`, `NotAMesh`) are retry-safe and
//!   do not touch the counter, while a protocol/internal error after dispatch is `outcomeUnknown`
//!   and bumps it;
//! * the per-face `set_mesh_material` route rebuilds through `mesh.setGeometry` from the mesh's
//!   LIVE geometry (`mesh.get`, `geometrySource:"live"`), so an undo, a failed write or another
//!   editor between two calls is simply seen by the next one; a bridge without `mesh.get` falls
//!   back to the saved file under the rebuild operators' rules.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::geometry;
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The document path the mock serves (Windows form; `--path-map` maps `z:\app` at the tree).
const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";
/// The POC's default material.
const DEFAULT_MATERIAL: &str = "materials/dev/reflectivity_30.vmat";
/// `materials/tools/toolsnodraw.vmat` as the POC's own tests use it.
const NODRAW: &str = "materials/tools/toolsnodraw.vmat";
/// The bridge's labels for the three geometry groups (`mcp_bridge.c`).
const CREATE_LABEL: &str = "MCP: create mesh";
const GEOMETRY_LABEL: &str = "MCP: set mesh geometry";
const MATERIAL_LABEL: &str = "MCP: set mesh material";

struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "mesh-live-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("workshop/run/app/content/csgo_addons/mcp_test/maps"))
            .expect("tmp tree");
        Tree { root }
    }

    /// Copy a committed fixture (`hammer-mcp/fixtures/vmap/<fixture>`) into the addon under `rel`.
    fn install(&self, fixture: &str, rel: &str) -> PathBuf {
        let target = self
            .root
            .join("workshop/run/app/content/csgo_addons/mcp_test")
            .join(rel);
        std::fs::create_dir_all(target.parent().expect("rel has a parent")).expect("install dir");
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/vmap")
                .join(fixture),
            &target,
        )
        .expect("install fixture");
        target
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn config(tree: &Tree, discovery: &Path) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let app = tree.root.join("workshop/run/app").display().to_string();
    let map = format!("z:\\app={app}");
    let discovery = discovery.display().to_string();
    let given: Vec<&str> = vec![
        "--deadline-ms",
        "5000",
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

/// A `class:"mesh"` node with no geometry in the world (what a saved map's mesh looks like to the
/// mock; only `create_*` writes geometry).
fn mesh_node(id: i64) -> MockNode {
    MockNode {
        node_id: id,
        class: "mesh".into(),
        classname: String::new(),
        targetname: None,
        origin: [0.0; 3],
        angles: [0.0; 3],
        keyvalues: BTreeMap::new(),
        locked: false,
        parent: None,
    }
}

/// A Hammer world (WP-M/WP-B) with the built-in `doc.active` and a dirty, editable document.
async fn hammer_mock(world_nodes: Vec<MockNode>) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(world_nodes)
        .start()
        .await
        .unwrap()
}

fn server(tree: &Tree, mb: &MockBridge) -> McpServer<HammerState> {
    hammer_mcp::build_server(&config(tree, &mb.discovery_path)).unwrap()
}

/// The geometry the mock world holds for `node_id`.
fn mock_mesh(mb: &MockBridge, node_id: i64) -> Option<cs2_mcp_common::mock::MockMesh> {
    mb.with_state(|state| {
        state
            .hammer
            .as_ref()
            .and_then(|world| world.meshes.get(&node_id).cloned())
    })
}

/// The node origin the mock world holds for `node_id`.
fn mock_origin(mb: &MockBridge, node_id: i64) -> Option<[f64; 3]> {
    mb.with_state(|state| {
        state
            .hammer
            .as_ref()
            .and_then(|world| world.nodes.iter().find(|n| n.node_id == node_id))
            .map(|node| node.origin)
    })
}

/// The `faceMaterials` override at `index` of a sent request (`null` = the base material).
fn override_at(params: &Value, index: usize) -> Value {
    params
        .get("faceMaterials")
        .and_then(|materials| materials.get(index))
        .cloned()
        .unwrap_or(Value::Null)
}

fn notes(out: &Value) -> Vec<String> {
    out["notes"]
        .as_array()
        .map(|notes| {
            notes
                .iter()
                .filter_map(|note| note.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// -------------------------------------------------------------------------------------------
// create_block
// -------------------------------------------------------------------------------------------

/// The POC's `test_create_block_box_sends_mesh_create`: one `mesh.create`, the top face
/// carries `topMaterial`, the reply names the new nodeID and the bridge's own undo group.
#[tokio::test]
async fn create_block_box_sends_one_mesh_create_with_the_per_face_material() {
    let tree = Tree::new("block-box");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [128, 128, 64], "topMaterial": NODRAW}),
    )
    .await
    .unwrap();

    let sent = mb.method_calls("mesh.create");
    assert_eq!(sent.len(), 1, "one create per call");
    let params = &sent[0].params;
    assert_eq!(params["vertices"].as_array().map(Vec::len), Some(8));
    assert_eq!(params["faces"].as_array().map(Vec::len), Some(6));
    assert_eq!(params["material"], json!(DEFAULT_MATERIAL));
    assert_eq!(
        params["faceMaterials"]
            .as_array()
            .map(|materials| materials.iter().filter(|m| !m.is_null()).count()),
        Some(1),
        "exactly the top face overrides the base material: {params}"
    );
    // face order is bottom, top, -y, +y, -x, +x (geometry.py): index 1 is the top
    assert_eq!(override_at(params, 1), json!(NODRAW));
    assert_eq!(override_at(params, 0), Value::Null);
    assert!(
        params.get("nodeID").is_none(),
        "a create has no nodeID: {params}"
    );
    assert!(params["expectDoc"].is_object(), "{params}");
    assert!(params["deadlineMs"].as_u64().unwrap_or(0) > 0, "{params}");

    // the result is the POC's `_send_geometry` shape plus the bridge's slot counts
    let node_id = out["nodeID"].as_i64().expect("new nodeID");
    assert_eq!(out["vertexCount"], json!(8));
    assert_eq!(out["faceCount"], json!(6));
    assert_eq!(out["vertexSlots"], json!(8), "the bridge's live slots: {out}");
    assert_eq!(out["faceSlots"], json!(6));
    assert_eq!(out["undoLabel"], json!(CREATE_LABEL));
    assert_eq!(out["undoNote"], json!("undo_last removes the mesh"));
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
    assert_eq!(out["saved"], json!(false));
    assert_eq!(out["shape"], json!("box"));
    assert_eq!(
        out["faceNames"],
        json!(["bottom", "top", "-y", "+y", "-x", "+x"])
    );
    assert_eq!(out["bounds"], json!({"min": [0.0, 0.0, 0.0], "max": [128.0, 128.0, 64.0]}));
    assert!(
        notes(&out).iter().any(|note| note.contains("no auto-save")),
        "{out}"
    );
    assert!(
        notes(&out).iter().any(|note| note.contains("pivot re-centred")),
        "{out}"
    );

    // the world really holds the mesh: the pivot is the bounds centre and the top face is nodraw
    assert_eq!(node_id, 2, "the world owns nodeID 1");
    assert_eq!(mock_origin(&mb, node_id), Some([64.0, 64.0, 32.0]));
    let stored = mock_mesh(&mb, node_id).expect("geometry stored");
    assert_eq!((stored.vertices.len(), stored.faces.len()), (8, 6));
    assert_eq!(stored.materials[1], NODRAW);
    assert_eq!(stored.materials[0], DEFAULT_MATERIAL);
}

/// The primitive shapes the POC offers, each with its own geometry and per-side material.
#[tokio::test]
async fn create_block_wedge_and_cylinder_build_their_own_geometry() {
    let tree = Tree::new("block-shapes");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);

    let wedge = call(
        &server,
        "create_block",
        json!({"shape": "wedge", "min": [64, -1100, 0], "max": [320, -972, 96],
               "riseToward": "+x"}),
    )
    .await
    .unwrap();
    assert_eq!(wedge["shape"], json!("wedge"));
    assert_eq!(wedge["faceNames"], json!(["bottom", "side_a", "side_b", "high", "slope"]));
    let params = &mb.method_calls("mesh.create")[0].params;
    assert_eq!(params["vertices"].as_array().map(Vec::len), Some(6));
    assert_eq!(params["faces"].as_array().map(Vec::len), Some(5));

    let cylinder = call(
        &server,
        "create_block",
        json!({"shape": "cylinder", "center": [448, -1036, 0], "radius": 48, "height": 160,
               "sides": 12, "sideMaterial": NODRAW}),
    )
    .await
    .unwrap();
    assert_eq!(cylinder["shape"], json!("cylinder"));
    assert_eq!(cylinder["vertexCount"], json!(24));
    assert_eq!(cylinder["faceCount"], json!(14));
    let params = &mb.method_calls("mesh.create")[1].params;
    assert_eq!(params["vertices"].as_array().map(Vec::len), Some(24));
    // the 12 sides are the majority, so THEY become the base material and the two caps carry the
    // default explicitly — the POC's `material_params` picks the most common material as the base
    assert_eq!(params["material"], json!(NODRAW));
    assert_eq!(override_at(params, 0), json!(DEFAULT_MATERIAL), "the bottom cap");
    assert_eq!(override_at(params, 1), json!(DEFAULT_MATERIAL), "the top cap");
    assert_eq!(override_at(params, 2), Value::Null, "the sides take the base");
    assert_eq!(
        params["faceMaterials"]
            .as_array()
            .map(|materials| materials.iter().filter(|m| !m.is_null()).count()),
        Some(2),
        "{params}"
    );

    // the default `sides` is 16 and the cylinder stands on `center`
    let default_sides = call(
        &server,
        "create_block",
        json!({"shape": "cylinder", "center": [0, 0, 0], "radius": 16, "height": 32}),
    )
    .await
    .unwrap();
    assert_eq!(default_sides["vertexCount"], json!(32));
    assert_eq!(default_sides["faceCount"], json!(18), "2 caps + 16 sides");
}

// -------------------------------------------------------------------------------------------
// create_mesh
// -------------------------------------------------------------------------------------------

/// `orientOutward` flips the winding of a closed solid and says how many faces it touched.
#[tokio::test]
async fn create_mesh_orient_outward_fixes_the_winding() {
    let tree = Tree::new("create-oriented");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);

    // the POC's box, with every loop reversed (inside-out)
    let canonical = geometry::r#box([256.0, -512.0, 0.0], [384.0, -384.0, 128.0]).unwrap();
    let reversed: Vec<Vec<usize>> = canonical
        .mesh
        .faces
        .iter()
        .map(|face| face.iter().copied().rev().collect())
        .collect();
    let out = call(
        &server,
        "create_mesh",
        json!({
            "vertices": canonical.mesh.vertices,
            "faces": reversed,
            "orientOutward": true,
            "material": NODRAW,
        }),
    )
    .await
    .unwrap();

    let params = &mb.method_calls("mesh.create")[0].params;
    assert_eq!(
        params["faces"],
        json!(canonical.mesh.faces),
        "the bridge gets the outward winding: {params}"
    );
    assert_eq!(params["material"], json!(NODRAW));
    assert_eq!(out["faceCount"], json!(6));
    assert_eq!(out["undoLabel"], json!(CREATE_LABEL));
    assert!(
        notes(&out).iter().any(|note| note == "orientOutward flipped 6 face(s)"),
        "{out}"
    );

    // without the flag the loops go out exactly as they came in (Hammer decides what that means)
    let plain = call(
        &server,
        "create_mesh",
        json!({"vertices": canonical.mesh.vertices, "faces": reversed}),
    )
    .await
    .unwrap();
    assert_eq!(mb.method_calls("mesh.create")[1].params["faces"], json!(reversed));
    assert_eq!(plain["unsavedBridgeEdits"], json!(2));
    assert!(
        !notes(&plain).iter().any(|note| note.contains("orientOutward")),
        "{plain}"
    );
}

/// Coordinates go out rounded to 4 decimals (the POC's `round(c, 4)`), the same numbers the fresh
/// node's bounds report.
#[tokio::test]
async fn create_mesh_rounds_the_vertices_it_sends() {
    let tree = Tree::new("create-round");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_mesh",
        json!({
            "vertices": [[0.123456789, 0.0, 0.0], [64.0, 0.0, 0.0], [0.0, 64.0, 0.0]],
            "faces": [[0, 1, 2]],
        }),
    )
    .await
    .unwrap();
    let sent = &mb.method_calls("mesh.create")[0].params["vertices"];
    assert_eq!(sent[0][0], json!(0.1235));
    assert_eq!(out["bounds"]["max"][0], json!(64.0));
    assert_eq!(out["vertexCount"], json!(3));
}

// -------------------------------------------------------------------------------------------
// Local refusals: nothing is dispatched
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn bad_geometry_is_refused_locally_and_sends_nothing() {
    let tree = Tree::new("geometry-bad");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);

    // [[0,0,0],[1,0,0]] cannot carry a face, [5] is out of range, three collinear points are a
    // zero-area face, and a coordinate at the bridge's limit is refused by geom_parse too
    let cases = [
        json!({"vertices": [[0, 0, 0], [1, 0, 0]], "faces": [[0, 1, 0]]}),
        json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 5]]}),
        json!({"vertices": [[0, 0, 0], [1, 0, 0], [2, 0, 0]], "faces": [[0, 1, 2]]}),
        json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 1]]}),
        json!({"vertices": [[1e6, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]]}),
        json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": []}),
        json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]]}),
        json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]],
               "material": "  "}),
        // the bridge's material buffer is 260 bytes
        json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]],
               "material": "m".repeat(300)}),
    ];
    for case in &cases {
        let err = call(&server, "create_mesh", case.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{case}: {err:?}");
        assert!(err.retry_safe, "{case}: nothing was dispatched");
    }
    // faceMaterials must line up with faces
    let err = call(
        &server,
        "create_mesh",
        json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]],
               "faceMaterials": ["a", "b"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams");

    // 65 distinct face materials: the bridge's GEO_MAX_MATS is 64
    let faces: Vec<Vec<usize>> = vec![vec![0, 1, 2]; 65];
    let many: Vec<String> = (0..65).map(|i| format!("materials/m{i}.vmat")).collect();
    let err = call(
        &server,
        "create_mesh",
        json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": faces,
               "faceMaterials": many}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(err.message.contains("64 distinct materials"), "{}", err.message);

    for case in [
        json!({}),
        json!({"min": [0, 0, 0], "max": [0, 10, 10]}),
        json!({"shape": "cylinder", "center": [0, 0, 0], "radius": 0, "height": 1}),
        json!({"shape": "cylinder", "center": [0, 0, 0], "radius": 1, "height": 1, "sides": 2}),
        json!({"shape": "sphere", "min": [0, 0, 0], "max": [1, 1, 1]}),
        json!({"min": [0, 0], "max": [1, 1, 1]}),
    ] {
        let err = call(&server, "create_block", case.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{case}: {err:?}");
    }
    // set_mesh_geometry validates the same way (and its nodeID)
    for case in [
        json!({"nodeID": -1, "vertices": [], "faces": []}),
        json!({"nodeID": 2_147_483_648i64, "vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]],
               "faces": [[0, 1, 2]]}),
        json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]]}),
    ] {
        let err = call(&server, "set_mesh_geometry", case.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{case}: {err:?}");
    }

    // every one of them was decided locally: the bridge never saw a geometry request, and the
    // document still has no mesh of its own
    assert!(
        mb.method_calls("mesh.create").is_empty(),
        "geometry validation refuses before dispatch"
    );
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
    assert!(
        mb.with_state(|state| state
            .hammer
            .as_ref()
            .map(|world| world.meshes.is_empty())
            .unwrap_or(false)),
        "nothing was created"
    );
}

/// A request the bridge's 1 MiB line buffer cannot take is refused here: an oversized line answers
/// `-32600 "line too long"` and closes the connection, taking every later call with it.
#[tokio::test]
async fn an_oversized_mesh_is_refused_before_dispatch() {
    let tree = Tree::new("geometry-big");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);

    // vertices that are cheap in bytes but not collinear, so the face is real and the size check is
    // what refuses the request
    let vertices: Vec<[f64; 3]> = (0..60_000)
        .map(|i| {
            let i = i as i64;
            [
                i as f64 * 0.5 + 0.1234,
                (i * i % 977) as f64 + 0.4321,
                (i * 7919 % 997) as f64 + 0.1111,
            ]
        })
        .collect();
    let err = call(
        &server,
        "create_mesh",
        json!({"vertices": vertices, "faces": [[0, 1, 2]]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(
        err.message.contains("line too long"),
        "the refusal names the bridge limit it protects: {}",
        err.message
    );
    assert!(err.retry_safe);
    assert!(mb.method_calls("mesh.create").is_empty(), "nothing dispatched");
}

// -------------------------------------------------------------------------------------------
// set_mesh_geometry
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn set_mesh_geometry_rebuilds_a_live_mesh() {
    let tree = Tree::new("set-geometry");
    let mb = hammer_mock(vec![mesh_node(9)]).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_mesh_geometry",
        json!({
            "nodeID": 9,
            "vertices": [[0, 0, 0], [64, 0, 0], [64, 64, 0], [0, 64, 0]],
            "faces": [[0, 1, 2, 3]],
            "material": NODRAW,
        }),
    )
    .await
    .unwrap();

    let sent = mb.method_calls("mesh.setGeometry");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].params["nodeID"], json!(9));
    assert_eq!(sent[0].params["material"], json!(NODRAW));
    assert!(sent[0].params["expectDoc"].is_object());
    assert!(sent[0].params["deadlineMs"].as_u64().unwrap_or(0) > 0);

    assert_eq!(out["nodeID"], json!(9));
    assert_eq!(out["faceCount"], json!(1));
    assert_eq!(out["undoLabel"], json!(GEOMETRY_LABEL));
    assert_eq!(
        out["undoNote"],
        json!("undo_last restores the previous geometry")
    );
    assert_eq!(out["bounds"], json!({"min": [0.0, 0.0, 0.0], "max": [64.0, 64.0, 0.0]}));
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
    let stored = mock_mesh(&mb, 9).expect("geometry replaced");
    assert_eq!(stored.faces, vec![vec![0, 1, 2, 3]]);
    assert_eq!(stored.materials, vec![NODRAW.to_string()]);
}

#[tokio::test]
async fn set_mesh_geometry_maps_the_bridge_target_errors_and_keeps_the_counter() {
    let tree = Tree::new("set-geometry-errors");
    let mb = hammer_mock(vec![MockNode::world(), mesh_node(9)]).await;
    let server = server(&tree, &mb);

    // a node that is not in the document (the bridge's -32022, before the undo group opens)
    let err = call(
        &server,
        "set_mesh_geometry",
        json!({"nodeID": 999, "vertices": [[0, 0, 0], [64, 0, 0], [0, 64, 0]],
               "faces": [[0, 1, 2]]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "MeshNotFound", "{err:?}");
    assert_eq!(err.type_, "MeshNotFound");
    assert_eq!(err.code, Some(-32022));
    assert!(err.retry_safe, "nothing was written: {err:?}");
    assert_eq!(err.data["nodeID"], json!(999));
    assert!(
        err.data.get("unsavedBridgeEdits").is_none(),
        "a refused target does not touch the counter: {err:?}"
    );

    // a node that exists but is not a mesh (the world itself: -32602 "node is not a mesh")
    let err = call(
        &server,
        "set_mesh_geometry",
        json!({"nodeID": 1, "vertices": [[0, 0, 0], [64, 0, 0], [0, 64, 0]],
               "faces": [[0, 1, 2]]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NotAMesh", "{err:?}");
    assert_eq!(err.code, Some(-32602));
    assert!(err.retry_safe);
    assert_eq!(err.data["reason"], json!("node is not a mesh"));

    // setMaterial maps the same two conditions
    let err = call(&server, "set_mesh_material", json!({"nodeID": 1, "material": NODRAW}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "NotAMesh");
    assert_eq!(mb.method_calls("mesh.setMaterial").len(), 1, "the request ran the gate");
    assert_eq!(mock_mesh(&mb, 1), None, "the world holds no geometry for it");

    // ... and nothing bumped the counter: the next real edit reports 1
    let out = call(
        &server,
        "set_mesh_geometry",
        json!({"nodeID": 9, "vertices": [[0, 0, 0], [64, 0, 0], [0, 64, 0]],
               "faces": [[0, 1, 2]]}),
    )
    .await
    .unwrap();
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
}

/// A document switch between `doc.active` and the write is the bridge's `-32004`: nothing ran, so
/// the counter stays where it was.
#[tokio::test]
async fn a_document_changed_refusal_is_retry_safe_and_keeps_the_counter() {
    let tree = Tree::new("geometry-changed");
    let mb = hammer_mock(vec![MockNode::world(), mesh_node(9)]).await;
    let server = server(&tree, &mb);
    mb.add_error(
        "mesh.setGeometry",
        -32004,
        "DocumentChanged",
        Some(json!({"reason": "doc_id"})),
    );

    let err = call(
        &server,
        "set_mesh_geometry",
        json!({"nodeID": 9, "vertices": [[0, 0, 0], [64, 0, 0], [0, 64, 0]],
               "faces": [[0, 1, 2]]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "DocumentChanged");
    assert_eq!(err.code, Some(-32004));
    assert!(err.retry_safe);
    assert!(!err.outcome_unknown);
    assert!(err.data.get("unsavedBridgeEdits").is_none());

    // the counter is still 0: the next edit reports 1 (negative evidence for a wrong bump)
    let out = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [8, 8, 8]}),
    )
    .await
    .unwrap();
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
}

/// A `-32603` after dispatch may have left the document half-written (a rebuild clears the mesh
/// first): `outcomeUnknown` and the counter move.
#[tokio::test]
async fn an_internal_error_on_a_mesh_write_is_maybe_executed() {
    let tree = Tree::new("geometry-internal");
    let mb = hammer_mock(vec![MockNode::world(), mesh_node(9)]).await;
    let server = server(&tree, &mb);
    mb.add_error(
        "mesh.setGeometry",
        -32603,
        "InternalError",
        Some(json!({"reason": "could not clear the mesh"})),
    );

    let err = call(
        &server,
        "set_mesh_geometry",
        json!({"nodeID": 9, "vertices": [[0, 0, 0], [64, 0, 0], [0, 64, 0]],
               "faces": [[0, 1, 2]]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "ProtocolError");
    assert!(err.outcome_unknown, "{err:?}");
    assert!(!err.retry_safe);
    assert_eq!(err.data["unsavedBridgeEdits"], json!(1));
}

// -------------------------------------------------------------------------------------------
// set_mesh_material
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn set_mesh_material_without_faces_uses_mesh_set_material() {
    let tree = Tree::new("material-all");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);
    let created = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [64, 64, 64]}),
    )
    .await
    .unwrap();
    let node_id = created["nodeID"].as_i64().expect("nodeID");

    let out = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": node_id, "material": NODRAW}),
    )
    .await
    .unwrap();

    let sent = mb.method_calls("mesh.setMaterial");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].params["nodeID"], json!(node_id));
    assert_eq!(sent[0].params["material"], json!(NODRAW));
    assert!(sent[0].params["expectDoc"].is_object());
    assert!(
        sent[0].params.get("vertices").is_none(),
        "the all-faces route does not rebuild: {sent:?}"
    );

    assert_eq!(out["nodeID"], json!(node_id));
    assert_eq!(out["material"], json!(NODRAW));
    assert_eq!(out["faceCount"], json!(6), "the bridge reports the faces it assigned");
    assert_eq!(out["undoLabel"], json!(MATERIAL_LABEL));
    assert_eq!(out["undoNote"], json!("undo_last restores the previous materials"));
    assert_eq!(out["unsavedBridgeEdits"], json!(2), "the create plus this");
    assert!(
        notes(&out).iter().any(|note| note == "texture alignment is kept (material only)"),
        "{out}"
    );
    let stored = mock_mesh(&mb, node_id).expect("geometry stored");
    assert!(
        stored.materials.iter().all(|material| material == NODRAW),
        "{stored:?}"
    );
    assert!(
        mb.method_calls("mesh.setGeometry").is_empty(),
        "the all-faces route never rebuilds"
    );
}

/// The per-face route (the POC's `h_set_mesh_material` with `faces`) rebuilds through
/// `mesh.setGeometry`, keeping every other face's material.
#[tokio::test]
async fn set_mesh_material_for_faces_rebuilds_from_the_live_geometry() {
    let tree = Tree::new("material-faces");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);
    let created = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [64, 64, 64]}),
    )
    .await
    .unwrap();
    let node_id = created["nodeID"].as_i64().expect("nodeID");

    let out = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": node_id, "material": NODRAW, "faces": [1]}),
    )
    .await
    .unwrap();

    assert!(mb.method_calls("mesh.setMaterial").is_empty(), "the rebuild route");
    let sent = mb.method_calls("mesh.setGeometry");
    assert_eq!(sent.len(), 1);
    let params = &sent[0].params;
    assert_eq!(params["nodeID"], json!(node_id));
    assert_eq!(params["material"], json!(DEFAULT_MATERIAL), "the remaining faces' material");
    assert_eq!(override_at(params, 1), json!(NODRAW));
    for index in [0, 2, 3, 4, 5] {
        assert_eq!(override_at(params, index), Value::Null, "face {index} keeps the base");
    }
    assert_eq!(
        params["vertices"],
        mb.method_calls("mesh.create")[0].params["vertices"],
        "the rebuild sends the geometry the live read returned"
    );
    assert_eq!(mb.method_calls("mesh.get").len(), 1, "one live read");

    assert_eq!(out["geometrySource"], json!("live"));
    assert_eq!(out["undoLabel"], json!(GEOMETRY_LABEL), "the bridge opens the rebuild group");
    assert_eq!(out["undoNote"], json!("undo_last restores the previous materials"));
    let stored = mock_mesh(&mb, node_id).expect("geometry");
    assert_eq!(stored.materials[1], NODRAW);
    assert_eq!(stored.materials[0], DEFAULT_MATERIAL);
}

/// A mesh node with no faces (the mock's model of a mesh Hammer holds empty) has no face to
/// re-material: a plain argument error, nothing dispatched. A bridge without `mesh.get` falls back
/// to the saved file, whose gate refuses the dirty document (`UnsavedChanges`) before any read.
#[tokio::test]
async fn set_mesh_material_for_faces_of_a_mesh_without_faces_is_an_argument_error() {
    let tree = Tree::new("material-empty");
    let mb = hammer_mock(vec![MockNode::world(), mesh_node(9)]).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": 9, "material": NODRAW, "faces": [0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
    assert!(mb.method_calls("mesh.setMaterial").is_empty());

    // an out-of-range face index on a mesh with geometry is the same plain argument error
    let created = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [8, 8, 8]}),
    )
    .await
    .unwrap();
    let node_id = created["nodeID"].as_i64().expect("nodeID");
    let err = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": node_id, "material": NODRAW, "faces": [6]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    assert!(err.message.contains("0..5"), "{}", err.message);
}

/// A bridge older than m3.12 has no `mesh.get`: the per-face route falls back to the saved file,
/// and the document here is dirty, so it is refused before anything is read or dispatched.
#[tokio::test]
async fn without_mesh_get_the_per_face_route_falls_back_to_the_file_gate() {
    let tree = Tree::new("material-no-mesh-get");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(vec![MockNode::world(), mesh_node(9)])
        .hammer_mesh_get(false)
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": 9, "material": NODRAW, "faces": [0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnsavedChanges", "{err:?}");
    assert!(mb.method_calls("mesh.get").is_empty(), "the method is not advertised");
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
}

// -------------------------------------------------------------------------------------------
// Capability gating and the document
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn mesh_tools_are_gated_on_the_advertised_methods() {
    let tree = Tree::new("gating");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .methods(["control.ping", "doc.active"])
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    for (tool, args, missing) in [
        (
            "create_mesh",
            json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]]}),
            vec!["mesh.create"],
        ),
        ("create_block", json!({"min": [0, 0, 0], "max": [1, 1, 1]}), vec!["mesh.create"]),
        (
            "set_mesh_geometry",
            json!({"nodeID": 9, "vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]],
                   "faces": [[0, 1, 2]]}),
            vec!["mesh.setGeometry"],
        ),
        (
            "set_mesh_material",
            json!({"nodeID": 9, "material": NODRAW}),
            vec!["mesh.setMaterial", "mesh.setGeometry"],
        ),
    ] {
        let err = call(&server, tool, args).await.unwrap_err();
        assert_eq!(err.name, "NotImplemented", "{tool}: {err:?}");
        assert_eq!(err.code, Some(-32011), "{tool}");
        assert_eq!(err.data["missing"], json!(missing), "{tool}");
        assert!(err.retry_safe, "{tool}");
    }
    for method in ["mesh.create", "mesh.setGeometry", "mesh.setMaterial"] {
        assert!(mb.method_calls(method).is_empty(), "{method}");
    }
}

#[tokio::test]
async fn mesh_tools_without_an_active_document_are_no_document() {
    let tree = Tree::new("no-document");
    let mb = MockBridge::builder()
        .hammer_nodes(vec![MockNode::world()])
        .responder_value("doc.active", json!({"doc": null, "reason": "no_document"}))
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    for (tool, args) in [
        (
            "create_mesh",
            json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]]}),
        ),
        ("create_block", json!({"min": [0, 0, 0], "max": [1, 1, 1]})),
        (
            "set_mesh_geometry",
            json!({"nodeID": 9, "vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]],
                   "faces": [[0, 1, 2]]}),
        ),
        ("set_mesh_material", json!({"nodeID": 9, "material": NODRAW})),
    ] {
        let err = call(&server, tool, args).await.unwrap_err();
        assert_eq!(err.name, "NoDocument", "{tool}: {err:?}");
        assert_eq!(err.code, Some(-32003), "{tool}");
        assert!(err.retry_safe, "{tool}");
    }
    for method in ["mesh.create", "mesh.setGeometry", "mesh.setMaterial"] {
        assert!(mb.method_calls(method).is_empty(), "{method}");
    }
}

/// A read-only document is the bridge's `-32018` (`geo_dispatch` checks editable before expectDoc);
/// the tool surfaces it unchanged and sends one request that writes nothing.
#[tokio::test]
async fn mesh_writes_on_a_read_only_document_surface_document_not_editable() {
    let tree = Tree::new("read-only");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .hammer_editable(false)
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [1, 1, 1]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "DocumentNotEditable");
    assert_eq!(err.code, Some(-32018));
    assert!(err.retry_safe, "the gate refuses before the op: {err:?}");
    assert_eq!(mb.method_calls("mesh.create").len(), 1, "the bridge decides this one");
    assert_eq!(mock_origin(&mb, 2), None, "no mesh was created");
}

/// What the tools send is what the bridge stores, and `undo_last` pops the group the bridge
/// labelled (with its geometry restored from the record).
#[tokio::test]
async fn the_tool_reports_the_bridges_own_undo_label_and_undo_restores_the_mesh() {
    let tree = Tree::new("undo-label");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_mesh",
        json!({
            "vertices": [[0, 0, 0], [64, 0, 0], [64, 64, 0], [0, 64, 0]],
            "faces": [[0, 1, 2, 3], [3, 2, 1, 0]],
            "faceMaterials": [NODRAW, None::<&str>],
        }),
    )
    .await
    .unwrap();
    let params = &mb.method_calls("mesh.create")[0].params;
    assert_eq!(params["material"], json!(NODRAW), "the most common face material");
    assert_eq!(override_at(params, 1), json!(DEFAULT_MATERIAL));
    assert_eq!(out["undoLabel"], json!(CREATE_LABEL));
    assert_eq!(out["undoNote"], json!("undo_last removes the mesh"));

    // the group is the top one and carries the bridge's label
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["document"]["undoName"], json!(CREATE_LABEL), "{status}");
    let undone = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(undone["undoName"], json!(CREATE_LABEL));
    assert_eq!(undone["undone"], json!(true));
    assert!(
        mb.with_state(|state| state
            .hammer
            .as_ref()
            .map(|world| world.meshes.is_empty())
            .unwrap_or(false)),
        "the group's snapshot removed the mesh again"
    );
}

// -------------------------------------------------------------------------------------------
// Live reads: nothing is remembered between calls (m3.12)
// -------------------------------------------------------------------------------------------
//
// The per-face `set_mesh_material` route and the rebuild operators read the mesh's geometry from
// Hammer (`mesh.get`) on every call. Until m3.12 they worked on a server-side copy of what this
// server had last written and had to drop it whenever the document might have moved on (undo, a
// map switch, a reload, a failed write); with a read per call each of those is just what the next
// read returns.

/// Write, undo, rebuild: the per-face route after an `undo_last` rebuilds the geometry Hammer holds
/// NOW (the box the undo restored), not the plane the undone write sent.
#[tokio::test]
async fn a_write_then_an_undo_then_a_rebuild_sees_the_restored_geometry() {
    let tree = Tree::new("live-undo");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);

    let created = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [64, 64, 64]}),
    )
    .await
    .unwrap();
    let node_id = created["nodeID"].as_i64().expect("nodeID");
    let box_vertices = mb.method_calls("mesh.create")[0].params["vertices"].clone();
    // replace the geometry (a plane, so the shape is clearly not the box's)
    call(
        &server,
        "set_mesh_geometry",
        json!({"nodeID": node_id, "vertices": [[0, 0, 0], [64, 0, 0], [64, 64, 0], [0, 64, 0]],
               "faces": [[0, 1, 2, 3]]}),
    )
    .await
    .unwrap();
    let undone = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(undone["undone"], json!(true), "{undone}");

    let out = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": node_id, "material": NODRAW, "faces": [5]}),
    )
    .await
    .unwrap();
    assert_eq!(out["geometrySource"], json!("live"), "{out}");
    assert_eq!(out["faceCount"], json!(6), "the box came back: {out}");
    let rebuild = mb.method_calls("mesh.setGeometry").pop().expect("the rebuild");
    assert_eq!(rebuild.params["vertices"], box_vertices, "the undone plane was not rebuilt");
    assert_eq!(override_at(&rebuild.params, 5), json!(NODRAW));
}

/// Opening another map: the new document's mesh ids collide with the old ones (Hammer starts at 1),
/// and the read is of whatever the bridge answers for the id now — one `mesh.get` per call, with
/// the new document's fingerprint, never a remembered shape.
#[tokio::test]
async fn opening_another_map_leaves_nothing_to_serve_stale() {
    let tree = Tree::new("live-open");
    let other = tree.install("defuse_poc.vmap", "maps/other.vmap");
    assert!(other.is_file(), "{}", other.display());
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(vec![MockNode::world()])
        .hammer_map_files(["maps/other.vmap"])
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let created = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [64, 64, 64]}),
    )
    .await
    .unwrap();
    let node_id = created["nodeID"].as_i64().expect("nodeID");
    let opened = call(&server, "open_map", json!({"path": "maps/other.vmap"}))
        .await
        .unwrap();
    assert_eq!(opened["opened"], json!(true), "{opened}");

    let reads = mb.method_calls("mesh.get").len();
    let _ = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": node_id, "material": NODRAW, "faces": [0]}),
    )
    .await;
    assert_eq!(
        mb.method_calls("mesh.get").len(),
        reads + 1,
        "the per-face route asks the bridge again instead of serving map A's copy"
    );
}

/// A write that failed with an outcome-unknown class (here the `-32603` a `mesh.setGeometry` can
/// only answer after `geo_reset_mesh` cleared the mesh) leaves nothing stale behind: the next
/// per-face call reads whatever Hammer holds.
#[tokio::test]
async fn an_outcome_unknown_write_is_followed_by_a_fresh_live_read() {
    let tree = Tree::new("live-unknown");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);

    let created = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [64, 64, 64]}),
    )
    .await
    .unwrap();
    let node_id = created["nodeID"].as_i64().expect("nodeID");

    mb.add_error(
        "mesh.setGeometry",
        -32603,
        "InternalError",
        Some(json!({"reason": "could not clear the mesh"})),
    );
    let err = call(
        &server,
        "set_mesh_geometry",
        json!({"nodeID": node_id, "vertices": [[0, 0, 0], [64, 0, 0], [64, 64, 0], [0, 64, 0]],
               "faces": [[0, 1, 2, 3]]}),
    )
    .await
    .unwrap_err();
    assert!(err.outcome_unknown, "the write may have landed: {err:?}");
    mb.with_state(|state| state.errors.clear());

    let reads = mb.method_calls("mesh.get").len();
    let out = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": node_id, "material": NODRAW, "faces": [0]}),
    )
    .await
    .unwrap();
    assert_eq!(out["geometrySource"], json!("live"), "{out}");
    assert_eq!(mb.method_calls("mesh.get").len(), reads + 1);
}

/// A per-face edit works right after a successful write: the live read returns the geometry the
/// write just left.
#[tokio::test]
async fn a_per_face_edit_works_after_a_write() {
    let tree = Tree::new("h1-success");
    let mb = hammer_mock(vec![mesh_node(9)]).await;
    let server = server(&tree, &mb);

    call(
        &server,
        "set_mesh_geometry",
        json!({"nodeID": 9, "vertices": [[0, 0, 0], [64, 0, 0], [64, 64, 0], [0, 64, 0]],
               "faces": [[0, 1, 2, 3]], "material": NODRAW}),
    )
    .await
    .unwrap();
    let out = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": 9, "material": "materials/dev/water.vmat", "faces": [0]}),
    )
    .await
    .unwrap();
    assert_eq!(out["geometrySource"], json!("live"), "{out}");
    assert_eq!(out["faceCount"], json!(1));
}

// -------------------------------------------------------------------------------------------
// Review-5 L1: concurrent tool calls of one server and the write gate
// -------------------------------------------------------------------------------------------

/// The prepared state of node 9: the geometry the setup write put into the document.
fn ground_geometry() -> Value {
    json!({
        "nodeID": 9,
        "vertices": [[0, 0, 0], [64, 0, 0], [64, 64, 0], [0, 64, 0]],
        "faces": [[0, 1, 2, 3]],
    })
}

/// What a storm round's rewrite names for node 9 — clearly different vertices from
/// [`ground_geometry`], so an answer that no longer matches the live document cannot hide
/// in rounding.
fn storm_geometry() -> Value {
    json!({
        "nodeID": 9,
        "vertices": [[100, 100, 100], [164, 100, 100], [164, 164, 100], [100, 164, 100]],
        "faces": [[0, 1, 2, 3]],
    })
}

/// The invariant the gate protects: the live read of the mesh (`get_mesh` without `path`) is the
/// geometry the document really holds (the mock world's own view of the node). `get_mesh` errors
/// (the mesh was undone away) are skipped; a non-live answer is a failure.
async fn assert_read_matches_live(
    server: &McpServer<HammerState>,
    mb: &MockBridge,
    node_id: i64,
    when: &str,
) {
    if let Ok(out) = call(server, "get_mesh", json!({"nodeID": node_id})).await {
        assert_eq!(out["source"], json!("live"), "{when}");
        {
            let live = mb.with_state(|state| {
                state
                    .hammer
                    .as_ref()
                    .and_then(|world| world.meshes.get(&node_id))
                    .cloned()
            });
            let live = live
                .unwrap_or_else(|| panic!("a served mesh exists in the live document ({when})"));
            let served: Vec<Value> = out["vertices"]
                .as_array()
                .expect("vertices")
                .to_vec();
            let stored: Vec<Value> = live.vertices.iter().map(|point| json!(point)).collect();
            assert_eq!(
                served, stored,
                "stale read of node {node_id} ({when}): get_mesh serves {served:?}, \
                 the document holds {stored:?}"
            );
            assert_eq!(
                out["faces"], json!(live.faces),
                "stale face list for node {node_id} ({when})"
            );
        }
    }
}

/// Review-5 L1: three concurrent calls of one server (a `set_mesh_geometry` of node 9, a
/// `create_mesh`, an `undo_last`) each run pre-read / write / post-read as separate bridge
/// requests. Every call holds [`HammerState::write_gate`] for its whole sequence, so whatever the
/// executor does with the three futures a reader observes the state before or after a complete
/// verified write. Each round asserts that the live read of node 9 is the geometry the document
/// really holds; the join order is rotated and the per-method reply delays widen the windows.
/// (A deterministic negative is not possible from the outside; the invariant holds under any
/// schedule and the argument is in `state.rs`.)
#[tokio::test]
async fn concurrent_mesh_writes_and_an_undo_keep_the_live_read_honest() {
    let tree = Tree::new("l1-write-gate");
    let mb = hammer_mock(vec![mesh_node(9)]).await;
    // widen the interleaving windows: every doc.active and geometry reply is held at the mock,
    // so the pre-read/write/post-read reads of the three concurrent flows overlap
    mb.add_delay("doc.active", 2);
    mb.add_delay("mesh.setGeometry", 4);
    mb.add_delay("undo.undo", 6);
    let server = server(&tree, &mb);

    // ground state: one written mesh (the server's own one undo group)
    call(&server, "set_mesh_geometry", ground_geometry())
        .await
        .unwrap();
    assert_read_matches_live(&server, &mb, 9, "after the setup write").await;

    // 0 = the node-9 rewrite (A), 1 = the create (B), 2 = the undo_last (D); all six join
    // orders, so the review's A/D/B ordering is among the schedules a regression would face
    const ORDERS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    async fn storm_call(server: &McpServer<HammerState>, which: usize) {
        let (name, args) = match which {
            0 => ("set_mesh_geometry", storm_geometry()),
            1 => (
                "create_mesh",
                json!({"vertices": [[0, 0, 0], [32, 0, 0], [0, 32, 0]], "faces": [[0, 1, 2]]}),
            ),
            _ => ("undo_last", json!({})),
        };
        let _ = call(server, name, args).await;
    }
    for (round, order) in ORDERS.iter().enumerate().cycle().take(4 * ORDERS.len()) {
        tokio::join!(
            storm_call(&server, order[0]),
            storm_call(&server, order[1]),
            storm_call(&server, order[2]),
        );
        assert_read_matches_live(
            &server,
            &mb,
            9,
            &format!("after storm round {round}"),
        )
        .await;
    }
}

// -------------------------------------------------------------------------------------------
// Review H2 / M1 / L1 / L2 / L3 / L5 / N2
// -------------------------------------------------------------------------------------------

/// H2: `create_block {material: A, sideMaterial: S}` sends the POC's wire split (base = the most
/// common material, S; overrides `[A, A, null…]`), and the later per-face edit (a live read) sees
/// **A** on the caps — the caller's `material` — because that is what the document holds.
#[tokio::test]
async fn create_block_keeps_the_callers_material_for_the_faces_that_use_it() {
    const WATER: &str = "materials/dev/water.vmat";
    const CLIP: &str = "materials/tools/toolsclip.vmat";
    let tree = Tree::new("h2-block");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);

    let created = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [64, 64, 64], "material": NODRAW, "sideMaterial": WATER}),
    )
    .await
    .unwrap();
    let node_id = created["nodeID"].as_i64().expect("nodeID");

    // the wire is the POC's: the 4 sides are the majority, so THEY are the base and the two caps
    // carry `material` explicitly
    let sent = &mb.method_calls("mesh.create")[0].params;
    assert_eq!(sent["material"], json!(WATER));
    assert_eq!(override_at(sent, 0), json!(NODRAW), "the bottom cap");
    assert_eq!(override_at(sent, 1), json!(NODRAW), "the top cap");

    // one face's material changes: the rebuild must keep NODRAW on the caps (the faces that use
    // `material`), not silently repaint them with the sides' WATER
    let out = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": node_id, "material": CLIP, "faces": [2]}),
    )
    .await
    .unwrap();
    assert_eq!(out["geometrySource"], json!("live"), "{out}");
    let rebuild = &mb.method_calls("mesh.setGeometry")[0].params;
    assert_eq!(override_at(rebuild, 0), json!(NODRAW), "{rebuild}");
    assert_eq!(override_at(rebuild, 1), json!(NODRAW), "{rebuild}");
    assert_eq!(override_at(rebuild, 2), json!(CLIP), "{rebuild}");
    assert_eq!(rebuild["material"], json!(WATER), "{rebuild}");
    let stored = mock_mesh(&mb, node_id).expect("geometry");
    assert_eq!(stored.materials[0], NODRAW, "the document keeps A on the caps");
    assert_eq!(stored.materials[2], CLIP);
    assert_eq!(stored.materials[3], WATER);
}

/// M1: the bridge applies `faceMaterials` only when Hammer's face count equals the one sent. When
/// Hammer welds a polygon away, the tool says so (face by face), sets `faceMaterialsApplied:false`
/// — and a later per-face edit rebuilds from the geometry Hammer really holds (the live read).
#[tokio::test]
async fn a_face_count_mismatch_is_reported_and_the_rebuild_reads_the_live_faces() {
    let tree = Tree::new("m1-mismatch");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    // Hammer "welds away" one of the box's six faces
    mb.set_hammer_drop_faces(1);
    let server = server(&tree, &mb);

    let created = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [64, 64, 64], "topMaterial": NODRAW}),
    )
    .await
    .unwrap();
    let node_id = created["nodeID"].as_i64().expect("nodeID");
    assert_eq!(created["faceCount"], json!(5), "the bridge's live count: {created}");
    assert_eq!(
        created["faceMaterialsApplied"],
        json!(false),
        "{created}"
    );
    let note = notes(&created)
        .into_iter()
        .find(|note| note.contains("Hammer created"))
        .unwrap_or_else(|| panic!("a mismatch note: {created}"));
    assert!(note.contains("5 of 6"), "{note}");
    assert!(
        note.contains("NOT applied"),
        "the per-face materials were skipped: {note}"
    );

    // the per-face route rebuilds from what Hammer holds (5 faces), not from what was sent (6)
    let out = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": node_id, "material": NODRAW, "faces": [0]}),
    )
    .await
    .unwrap();
    assert_eq!(out["geometrySource"], json!("live"), "{out}");
    let rebuild = mb.method_calls("mesh.setGeometry").pop().expect("the rebuild");
    assert_eq!(rebuild.params["faces"].as_array().map(Vec::len), Some(5), "{}", rebuild.params);
}

/// L1: every `-32603` from `mesh.setMaterial` is decided before `undo_open`, so nothing ran: the
/// tool must not claim an unknown outcome and must not bump the unsaved counter (`setGeometry` does,
/// which is why it keeps the conservative mapping).
#[tokio::test]
async fn a_mesh_set_material_internal_error_is_not_maybe_executed() {
    let tree = Tree::new("l1-material-error");
    // the mock's pre-seeded mesh node has no geometry: exactly the live bridge's
    // `-32603 {"reason":"mesh has no faces"}`
    let mb = hammer_mock(vec![MockNode::world(), mesh_node(9)]).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": 9, "material": NODRAW}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "MeshEmpty", "{err:?}");
    assert_eq!(err.code, Some(-32603));
    assert!(err.retry_safe, "nothing was written: {err:?}");
    assert!(!err.outcome_unknown, "{err:?}");
    assert!(
        err.data.get("unsavedBridgeEdits").is_none(),
        "no edit was counted: {err:?}"
    );
    assert_eq!(err.data["nodeID"], json!(9));

    // the counter really is untouched: the next edit reports 1 (negative evidence for a wrong bump)
    let out = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [8, 8, 8]}),
    )
    .await
    .unwrap();
    assert_eq!(out["unsavedBridgeEdits"], json!(1), "{out}");
}

/// L2/L3: the face and coordinate checks run on what the bridge will read — the 4-decimal values and
/// their f32 widening — not on the raw input. A face that only exists in f64 (a sliver, or a small
/// face where the float step is coarse) makes Hammer create fewer polygons than sent (M1), and a
/// coordinate the rounding pushes onto the +-1e6 limit would come back as the bridge's own refusal.
#[tokio::test]
async fn geometry_is_validated_after_rounding_and_as_float() {
    let tree = Tree::new("l2-l3-rounded");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);

    // L3: 999999.99996 is inside the bridge's range, but `round(c, 4)` moves it onto the limit
    let err = call(
        &server,
        "create_mesh",
        json!({"vertices": [[999_999.999_96, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(
        err.message.contains("outside") && err.message.contains("1000000"),
        "our own message, not the bridge's: {}", err.message
    );
    assert!(err.retry_safe);
    assert!(
        mb.method_calls("mesh.create").is_empty(),
        "refused before the request"
    );

    // L2(a): a 1e-5-wide triangle round-trips to a zero-area one (two vertices become the same)
    let err = call(
        &server,
        "create_mesh",
        json!({"vertices": [[0, 0, 0], [0.00001, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(err.message.contains("faces[0]"), "{}", err.message);

    // L2(b): a 0.01-wide triangle at |x| ~ 5e5 still has area in f64 and in the rounded values, but
    // the bridge's float step there is ~0.03, so two of its vertices become the same float and the
    // face Hammer would see is degenerate
    let err = call(
        &server,
        "create_mesh",
        json!({"vertices": [[500_000.0, 500_000.0, 0], [500_000.01, 500_000.0, 0],
               [500_000.0, 500_001.0, 0]], "faces": [[0, 1, 2]]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(
        err.message.contains("float"),
        "the refusal names the float widening: {}",
        err.message
    );
    assert!(
        mb.method_calls("mesh.create").is_empty(),
        "nothing was dispatched"
    );
}

/// L5: the registry does not enforce `required`/`additionalProperties`, so the handler does. A
/// missing `material` used to paint every face with the default dev material, and a typo like
/// `face` for `faces` used to take the all-faces route silently.
#[tokio::test]
async fn mesh_mutators_refuse_a_missing_material_and_unknown_keys() {
    let tree = Tree::new("l5-arguments");
    let mb = hammer_mock(vec![MockNode::world(), mesh_node(9)]).await;
    let server = server(&tree, &mb);

    let err = call(&server, "set_mesh_material", json!({"nodeID": 9}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(err.message.contains("material is required"), "{}", err.message);

    let err = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": 9, "material": NODRAW, "face": [0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(err.message.contains("unknown argument"), "{}", err.message);
    assert!(err.message.contains("faces"), "the message lists the keys: {}", err.message);

    // the geometry tools too (a mistyped `vertices`/`faces` is a different edit)
    let err = call(
        &server,
        "set_mesh_geometry",
        json!({"nodeID": 9, "vertex": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(
        err.message.contains("unknown argument"),
        "{}",
        err.message
    );
    for method in ["mesh.setMaterial", "mesh.setGeometry"] {
        assert!(mb.method_calls(method).is_empty(), "{method}");
    }
}

/// N2: a create reply with no usable node id (the live `geo_mesh_summary` reports `-1` when its own
/// lookup failed) passes through, and the tool says how to find the mesh instead of leaving the
/// caller with a `-1` and no way on.
#[tokio::test]
async fn a_negative_reported_node_id_gets_a_find_it_note() {
    let tree = Tree::new("n2-negative-id");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    mb.add_responder_value(
        "mesh.create",
        json!({"nodeID": -1, "class": "mesh", "faces": 6, "vertexSlots": 8,
               "undoLabel": "MCP: create mesh"}),
    );
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [64, 64, 64]}),
    )
    .await
    .unwrap();
    assert_eq!(out["nodeID"], json!(-1), "the reply passes through: {out}");
    assert!(
        notes(&out)
            .iter()
            .any(|note| note.contains("no usable nodeID") && note.contains("find_entities")),
        "the tool says how to find the mesh: {out}"
    );
}
