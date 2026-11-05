//! `mesh_live` rebuild-operator tests (WP-C): `transform_mesh`, `move_mesh_vertices`,
//! `extrude_mesh_face`, `delete_mesh_faces` against the mock bridge's Hammer world
//! (`mesh.setGeometry`, WP-B's arm), with the geometry itself from `hammer_mcp::geometry` (the port
//! of the POC's `geometry.py`).
//!
//! What these tests pin, beyond the happy paths:
//!
//! * the **geometry source**, in the POC's order and reported as the POC's `geometrySource`: the
//!   the live document through `mesh.get` (`"live"`, no dirty gate: a read of Hammer itself; an
//!   undo, a failed write or a UI edit between two calls is simply seen by the next read) — else,
//!   for a bridge without `mesh.get`, the
//!   SAVED `.vmap` (`"file"`, `crate::vmap::mesh`), which a scaled file mesh refuses (review-2
//!   M1) and which is only read while the live document is clean — this is the WP-C refinement
//!   of the POC's read-anyway-and-warn — so a dirty (or dirty-unknown, or MCP-edited) document is
//!   `-32016 UnsavedChanges` with the `save_map` hint and **nothing** is read or sent;
//! * the request the bridge gets: the operator applied to the source geometry, rounded vertices,
//!   the rebuilt loops and the POC's material split, `expectDoc` (the *source* read's document,
//!   review-2 L2) and a positive `deadlineMs`;
//! * the POC's result shape: `geometrySource`, the rebuilt counts (`vertexCount`/`faceCount`),
//!   the `undoNote`, the unsaved counter, plus `extrude_mesh_face`'s `newFaces`/`capFace` and
//!   `delete_mesh_faces`'s renumbered note;
//! * local refusals that send nothing: a face/vertex index out of range, an empty or over-length
//!   index list (review-2 L5's cap), a zero scale, a zero distance, `transform_mesh` without one
//!   of translate/rotateYaw/scale (all before the source route runs, review-2 N1), and the result
//!   checks (a move that flattens a face, a coordinate the bridge would refuse);
//! * the error mapping the shared send path already has: the bridge's target conditions
//!   (`MeshNotFound`, `NotAMesh`) and `DocumentChanged` are retry-safe and do not touch the
//!   counter, while a protocol/internal error after dispatch is `outcomeUnknown` and bumps it;
//! * live-read scenarios (m3.12, replacing the review-4 sticky-cache ones): an operator after a
//!   write and an `undo_last` starts from the restored geometry, a maybe-executed failure is
//!   followed by a fresh read, consecutive writes are each read live;
//! * capability gating and the document gates (`NoDocument`, an untitled document).
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
use cs2_mcp_common::tools::{CallFailure, ToolError};
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
/// The bridge's label for a geometry rebuild (`mcp_bridge.c`).
const GEOMETRY_LABEL: &str = "MCP: set mesh geometry";
/// `probe.vmap`'s room shell: 12 vertices / 10 faces, one material (WP-A's fixtures).
const FILE_MESH_NODE: i64 = 3;

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "mesh-ops-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let app = root.join("workshop/run/app");
        std::fs::create_dir_all(app.join("content/csgo_addons/mcp_test/maps")).expect("tmp tree");
        Tree { root, app }
    }

    /// Copy a committed fixture into the addon at the addon-relative path (`maps/probe.vmap`).
    fn install(&self, fixture: &str, rel: &str) -> PathBuf {
        let target = self
            .app
            .join("content/csgo_addons/mcp_test")
            .join(rel);
        std::fs::create_dir_all(target.parent().expect("parent")).expect("install dir");
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/vmap")
                .join(fixture),
            &target,
        )
        .expect("install fixture");
        target
    }

    /// The host path `DOC_WIN` resolves to.
    fn probe(&self) -> PathBuf {
        self.app
            .join("content/csgo_addons/mcp_test/maps/probe.vmap")
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
    let app = tree.app.display().to_string();
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

/// A Hammer world with the built-in `doc.active`; `dirty` is what Hammer reports, `nodes` the live
/// document (mesh nodes the operators can address by node id).
async fn hammer_mock(nodes: Vec<MockNode>, dirty: Option<bool>) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, dirty)))
        .hammer_nodes(nodes)
        .start()
        .await
        .unwrap()
}

/// [`hammer_mock`] for a bridge older than m3.12: no `mesh.get`, so the operators take the saved
/// `.vmap` route and its gates.
async fn hammer_mock_file(nodes: Vec<MockNode>, dirty: Option<bool>) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, dirty)))
        .hammer_nodes(nodes)
        .hammer_mesh_get(false)
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

/// The POC's `round(c, 4)`, as `_send_geometry` rounds every coordinate it sends.
fn round4(point: [f64; 3]) -> [f64; 3] {
    point.map(|c| {
        let rounded = (c * 10_000.0).round() / 10_000.0;
        if rounded == 0.0 {
            0.0
        } else {
            rounded
        }
    })
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

/// The sent vertices as `[f64; 3]`.
fn sent_vertices(params: &Value) -> Vec<[f64; 3]> {
    params["vertices"]
        .as_array()
        .expect("vertices")
        .iter()
        .map(|vertex| {
            let axes = vertex.as_array().expect("axes");
            [
                axes[0].as_f64().expect("x"),
                axes[1].as_f64().expect("y"),
                axes[2].as_f64().expect("z"),
            ]
        })
        .collect()
}

/// The `faceMaterials` override at `index` of a sent request (`null` = the base material).
fn override_at(params: &Value, index: usize) -> Value {
    params
        .get("faceMaterials")
        .and_then(|materials| materials.get(index))
        .cloned()
        .unwrap_or(Value::Null)
}

/// A box in the live document through `create_block` (the live read's source).
async fn create_box(server: &McpServer<HammerState>, size: f64) -> i64 {
    let out = call(
        server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [size, size, size]}),
    )
    .await
    .unwrap();
    out["nodeID"].as_i64().expect("new nodeID")
}

// -------------------------------------------------------------------------------------------
// The geometry source: the live read, else (a bridge without mesh.get) the SAVED .vmap
// -------------------------------------------------------------------------------------------

/// The POC's `"file"` source, now the fallback of a bridge without `mesh.get`: the mesh is read from
/// the SAVED `.vmap` — its nodeID is the live document's node id — and the operator is applied to
/// that geometry.
#[tokio::test]
async fn transform_mesh_reads_a_foreign_mesh_from_the_saved_vmap() {
    let tree = Tree::new("file-source");
    tree.install("probe.vmap", "maps/probe.vmap");
    // the file's own nodeID 3 is the live node the bridge would resolve (`geo_find`)
    let mb = hammer_mock_file(vec![MockNode::world(), mesh_node(FILE_MESH_NODE)], Some(false)).await;
    let server = server(&tree, &mb);

    let document = hammer_mcp::vmap::load(&tree.probe()).expect("probe.vmap parses");
    let record = document
        .mesh_by_id(FILE_MESH_NODE)
        .expect("the fixture holds nodeID 3");
    assert_eq!(record.id, Some(FILE_MESH_NODE), "the same id space");
    assert_eq!((record.vertices.len(), record.faces.len()), (12, 10));

    let out = call(
        &server,
        "transform_mesh",
        json!({"nodeID": FILE_MESH_NODE, "translate": [0, 0, 64]}),
    )
    .await
    .unwrap();

    let sent = mb.method_calls("mesh.setGeometry");
    assert_eq!(sent.len(), 1);
    let params = &sent[0].params;
    assert_eq!(params["nodeID"], json!(FILE_MESH_NODE));
    assert!(params["expectDoc"].is_object(), "{params}");
    assert!(params["deadlineMs"].as_u64().unwrap_or(0) > 0, "{params}");
    // the file's world vertices, each translated by [0, 0, 64] and rounded like `_send_geometry`
    let expected: Vec<[f64; 3]> = record
        .vertices
        .iter()
        .map(|point| round4(add(*point, [0.0, 0.0, 64.0])))
        .collect();
    assert_eq!(sent_vertices(params), expected);
    assert_eq!(params["faces"], json!(record.faces), "loops are kept");
    assert_eq!(params["material"], json!(DEFAULT_MATERIAL));
    assert!(
        params.get("faceMaterials").is_none(),
        "one material for every face: {params}"
    );

    assert_eq!(out["nodeID"], json!(FILE_MESH_NODE));
    assert_eq!(out["geometrySource"], json!("file"));
    assert_eq!(out["vertexCount"], json!(12));
    assert_eq!(out["faceCount"], json!(10));
    assert_eq!(out["undoLabel"], json!(GEOMETRY_LABEL));
    assert_eq!(
        out["undoNote"],
        json!("undo_last restores the previous geometry")
    );
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
    assert_eq!(out["saved"], json!(false));
    // a pure translate moves the *sent* (rounded) bounds by exactly the offset
    let (min, max) = expected.iter().fold(
        ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]),
        |(mut min, mut max), point| {
            for axis in 0..3 {
                min[axis] = min[axis].min(point[axis]);
                max[axis] = max[axis].max(point[axis]);
            }
            (min, max)
        },
    );
    assert_eq!(out["bounds"], json!({"min": min, "max": max}));
    assert!(
        notes(&out).iter().any(|note| note.contains("SAVED .vmap")),
        "the file source says where the geometry came from: {out}"
    );
    assert!(
        notes(&out).iter().any(|note| note.contains("pivot re-centred")),
        "{out}"
    );
    // one `doc.active` read: the source resolution. m2.17: the post-write verification state comes
    // from the write's OWN reply (`doc`), so the extra read review-4 R4-1 step 3 used to make is
    // gone — and with it the window a user edit could land in between (review-5 L2)
    assert_eq!(mb.method_calls("doc.active").len(), 1);

    // the world really holds the transformed geometry (rounded, as sent)
    let stored = mock_mesh(&mb, FILE_MESH_NODE).expect("geometry stored");
    assert_eq!((stored.vertices.len(), stored.faces.len()), (12, 10));
    assert_eq!(stored.vertices, expected);
}
/// The live source: the mesh is rebuilt from exactly the geometry Hammer holds (`mesh.get`), dirty
/// document or not — the read is of the document itself, so there is no dirty gate.
#[tokio::test]
async fn move_mesh_vertices_rebuilds_a_session_mesh_from_the_live_geometry() {
    let tree = Tree::new("live-source");
    // dirty and unsaved: the live read does not care
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;
    let before = mb.method_calls("doc.active").len();

    let out = call(
        &server,
        "move_mesh_vertices",
        json!({"nodeID": node_id, "vertices": [4, 5], "offset": [0, 0, -32]}),
    )
    .await
    .unwrap();

    assert_eq!(out["geometrySource"], json!("live"));
    // one `doc.active` (the source resolution; its fingerprint rides `mesh.get` and the write) and
    // one `mesh.get`: the post-write state comes from the write's own reply
    assert_eq!(mb.method_calls("doc.active").len(), before + 1);
    assert_eq!(mb.method_calls("mesh.get").len(), 1);
    let created = mb.method_calls("mesh.create")[0].params["vertices"].clone();
    let sent = &mb.method_calls("mesh.setGeometry")[0].params;
    assert_eq!(sent["nodeID"], json!(node_id));
    let mut expected: Vec<[f64; 3]> = created
        .as_array()
        .unwrap()
        .iter()
        .map(|vertex| {
            let axes = vertex.as_array().unwrap();
            [
                axes[0].as_f64().unwrap(),
                axes[1].as_f64().unwrap(),
                axes[2].as_f64().unwrap(),
            ]
        })
        .collect();
    for (index, point) in expected.iter_mut().enumerate() {
        if index == 4 || index == 5 {
            point[2] -= 32.0;
        }
    }
    assert_eq!(
        sent_vertices(sent),
        expected,
        "only the listed vertices move: {sent}"
    );
    assert_eq!(sent["faces"].as_array().map(Vec::len), Some(6));
    assert_eq!(out["vertexCount"], json!(8));
    assert_eq!(out["faceCount"], json!(6));

    let stored = mock_mesh(&mb, node_id).expect("geometry stored");
    assert_eq!(stored.vertices[4][2], 32.0);
    assert_eq!(stored.vertices[5][2], 32.0);
    assert_eq!(stored.vertices[6][2], 64.0);
}

#[tokio::test]
async fn transform_mesh_reads_live_geometry_without_touching_the_disk() {
    let tree = Tree::new("live-no-file");
    // no probe.vmap on disk at all: the live read must not read the file
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;

    let out = call(
        &server,
        "transform_mesh",
        json!({"nodeID": node_id, "translate": [8, 0, 0], "rotateYaw": 90}),
    )
    .await
    .unwrap();
    assert_eq!(out["geometrySource"], json!("live"));
    assert_eq!(out["vertexCount"], json!(8));
    assert_eq!(out["faceCount"], json!(6));

    // review-2 N5: literal vertices, worked out by hand, not `geometry::transform` itself —
    // box size 64, pivot = the bounds centre [32, 32, 32], yaw 90 CCW about it
    // (x' = c·x − s·y: the point's offset (dx, dy) rotates to (−dy, dx)), then translate
    // [8, 0, 0]. Vertex 1 [64, 0, 0]: its offset (+32, −32, z) rotates to (+32, +32, z),
    // back to [64, 64, 0] + [8, 0, 0] = [72, 64, 0]; an inverted rotation sign gives
    // [8, 0, 0], and a transform that ignores the yaw gives [72, 0, 0] — neither is accepted below.
    let expected: Vec<[f64; 3]> = vec![
        [72.0, 0.0, 0.0],
        [72.0, 64.0, 0.0],
        [8.0, 0.0, 0.0],
        [8.0, 64.0, 0.0],
        [72.0, 0.0, 64.0],
        [72.0, 64.0, 64.0],
        [8.0, 0.0, 64.0],
        [8.0, 64.0, 64.0],
    ];
    assert_eq!(
        sent_vertices(&mb.method_calls("mesh.setGeometry")[0].params),
        expected
    );
}

/// m2.17 (review-5 L2): a mesh write's post-write state is the one its **own reply** carries, read
/// on the bridge's main thread inside the write's dispatch — so there is no extra `doc.active`
/// read after it. A bridge **older than m2.17** answers no `doc`: the
/// same write still succeeds; the next live read sees the written geometry either way.
#[tokio::test]
async fn a_write_without_the_reply_doc_still_succeeds() {
    let tree = Tree::new("write-doc-fallback");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;
    let quad = json!({"nodeID": node_id,
                      "vertices": [[0, 0, 0], [64, 0, 0], [64, 64, 0], [0, 64, 0]],
                      "faces": [[0, 1, 2, 3]],
                      "material": NODRAW});

    // the m2.17 bridge: one `doc.active` for the whole write (its own pre-write `expectDoc` read)
    let before = mb.method_calls("doc.active").len();
    let out = call(&server, "set_mesh_geometry", quad.clone()).await.unwrap();
    assert_eq!(
        mb.method_calls("doc.active").len(),
        before + 1,
        "the reply's `doc` replaced the post-write read: {out}"
    );
    let fresh = call(&server, "get_mesh", json!({"nodeID": node_id})).await.unwrap();
    assert_eq!(fresh["source"], json!("live"), "{fresh}");
    assert_eq!(fresh["faceCount"], json!(1), "the live read sees the write: {fresh}");

    // an older bridge: the reply has no `doc` (nothing here needs one)
    mb.set_hammer_write_doc(false);
    let before = mb.method_calls("doc.active").len();
    let out = call(&server, "set_mesh_geometry", quad).await.unwrap();
    assert_eq!(
        mb.method_calls("doc.active").len(),
        before + 1,
        "an older bridge's reply has no `doc`, and nothing remembered needs one: {out}"
    );
    let fresh = call(&server, "get_mesh", json!({"nodeID": node_id})).await.unwrap();
    assert_eq!(fresh["source"], json!("live"), "{fresh}");
}

// -------------------------------------------------------------------------------------------
// The dirty-document refusal (WP-C's refinement of the POC)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_dirty_document_is_refused_before_the_file_is_read() {
    let tree = Tree::new("dirty");
    tree.install("probe.vmap", "maps/probe.vmap");
    let mb = hammer_mock_file(vec![MockNode::world(), mesh_node(FILE_MESH_NODE)], Some(true)).await;
    let server = server(&tree, &mb);

    for (tool, args) in [
        (
            "transform_mesh",
            json!({"nodeID": FILE_MESH_NODE, "translate": [0, 0, 64]}),
        ),
        (
            "move_mesh_vertices",
            json!({"nodeID": FILE_MESH_NODE, "vertices": [0], "offset": [0, 0, 8]}),
        ),
        (
            "extrude_mesh_face",
            json!({"nodeID": FILE_MESH_NODE, "face": 0, "distance": 16}),
        ),
        ("delete_mesh_faces", json!({"nodeID": FILE_MESH_NODE, "faces": [0]})),
    ] {
        let err = call(&server, tool, args).await.unwrap_err();
        assert_eq!(err.name, "UnsavedChanges", "{tool}: {err:?}");
        assert_eq!(err.type_, "UnsavedChanges", "{tool}");
        assert_eq!(err.code, Some(-32016), "{tool}");
        assert!(err.retry_safe, "{tool}: nothing was sent");
        assert_eq!(err.data["nodeID"], json!(FILE_MESH_NODE), "{tool}");
        assert_eq!(err.data["geometrySource"], json!("unavailable"), "{tool}");
        assert_eq!(err.data["reason"], json!("dirty document"), "{tool}");
        assert_eq!(err.data["unsavedBridgeEdits"], json!(0), "{tool}");
        assert!(
            err.message.contains("save_map") && err.message.contains("set_mesh_geometry"),
            "{tool}: the refusal names the way out: {}",
            err.message
        );
    }
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
    // one `doc.active` per call: the refusal happens before anything is read or sent
    assert_eq!(mb.method_calls("doc.active").len(), 4);
}

/// The other half of the clean-document rule: this server's own unsaved edits gate the file route
/// too — the file would not contain them. (Hammer's own flag goes up with every bridge mutation,
/// so the responder holds it at `false` to isolate the counter.)
#[tokio::test]
async fn unsaved_mcp_edits_refuse_the_file_route_too() {
    let tree = Tree::new("unsaved");
    tree.install("probe.vmap", "maps/probe.vmap");
    let clean = make_doc(DOC_WIN, Some(false));
    let mb = MockBridge::builder()
        .doc(Some(clean.clone()))
        .responder_value("doc.active", json!({"doc": clean}))
        .hammer_nodes(vec![MockNode::world(), mesh_node(FILE_MESH_NODE)])
        .hammer_mesh_get(false)
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let created = create_box(&server, 32.0).await;
    assert!(
        created > 0,
        "the box creation itself is unaffected by the dirty flag"
    );
    let err = call(
        &server,
        "transform_mesh",
        json!({"nodeID": FILE_MESH_NODE, "translate": [0, 0, 64]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnsavedChanges");
    assert_eq!(err.data["unsavedBridgeEdits"], json!(1));
    assert_eq!(err.data["reason"], json!("unsaved bridge edits"));
    assert_eq!(err.data["geometrySource"], json!("unavailable"));
    assert!(err.retry_safe);
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
}

/// A document that does not report `dirty` at all is refused as well (the `reload_map` rule).
#[tokio::test]
async fn an_unknown_dirty_state_refuses_the_file_route() {
    let tree = Tree::new("dirty-unknown");
    tree.install("probe.vmap", "maps/probe.vmap");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, None)))
        // the mock's world doc always reports a bool, so the responder supplies the null one
        .responder_value("doc.active", json!({"doc": make_doc(DOC_WIN, None)}))
        .hammer_nodes(vec![MockNode::world(), mesh_node(FILE_MESH_NODE)])
        .hammer_mesh_get(false)
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "transform_mesh",
        json!({"nodeID": FILE_MESH_NODE, "translate": [0, 0, 64]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnsavedChanges");
    assert_eq!(err.code, Some(-32016));
    assert_eq!(err.data["reason"], json!("dirty state unreported"));
    assert!(
        err.message.contains("does not report its dirty state"),
        "{}",
        err.message
    );
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
}

// -------------------------------------------------------------------------------------------
// The other source refusals
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_mesh_missing_from_the_saved_vmap_is_mesh_not_found() {
    let tree = Tree::new("not-in-file");
    tree.install("probe.vmap", "maps/probe.vmap");
    let mb = hammer_mock_file(vec![MockNode::world(), mesh_node(999)], Some(false)).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "transform_mesh",
        json!({"nodeID": 999, "translate": [0, 0, 64]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "MeshNotFound");
    assert_eq!(err.type_, "MeshNotFound");
    assert_eq!(err.data["nodeID"], json!(999));
    assert_eq!(err.data["path"], json!(DOC_WIN));
    assert_eq!(err.data["geometrySource"], json!("file"));
    assert!(
        err.message.contains("save_map"),
        "the hint names save_map first: {}",
        err.message
    );
    assert!(err.retry_safe);
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
}

#[tokio::test]
async fn an_untitled_document_has_no_geometry_source() {
    let tree = Tree::new("untitled");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_new_file(true)
        .hammer_nodes(vec![MockNode::world(), mesh_node(FILE_MESH_NODE)])
        .hammer_mesh_get(false)
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "extrude_mesh_face",
        json!({"nodeID": FILE_MESH_NODE, "face": 0, "distance": 16}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "MeshGeometryUnavailable");
    assert_eq!(err.type_, "MeshGeometryUnavailable");
    assert!(err.retry_safe);
    assert_eq!(err.data["reason"], json!("untitled"));
    assert_eq!(err.data["geometrySource"], json!("unavailable"));
    assert!(
        err.message.contains("save_map") && err.message.contains("set_mesh_geometry"),
        "{}",
        err.message
    );
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
}

#[tokio::test]
async fn an_unmapped_windows_path_and_a_missing_file_are_reported() {
    let tree = Tree::new("paths");
    let unmapped = MockBridge::builder()
        .doc(Some(make_doc("y:\\maps\\probe.vmap", Some(false))))
        .hammer_nodes(vec![MockNode::world(), mesh_node(FILE_MESH_NODE)])
        .hammer_mesh_get(false)
        .start()
        .await
        .unwrap();
    let unmapped_server = server(&tree, &unmapped);
    let err = call(
        &unmapped_server,
        "delete_mesh_faces",
        json!({"nodeID": FILE_MESH_NODE, "faces": [0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PathNotMapped");
    assert_eq!(err.code, Some(-32602));
    assert_eq!(err.data["windowsPath"], json!("y:\\maps\\probe.vmap"));
    assert!(
        err.message.contains("CS2_MCP_PATH_MAP"),
        "{}",
        err.message
    );
    assert!(err.retry_safe);
    assert!(unmapped.method_calls("mesh.setGeometry").is_empty());

    // a mapped path whose file does not exist (nothing installed in this tree)
    let missing = hammer_mock_file(vec![MockNode::world(), mesh_node(FILE_MESH_NODE)], Some(false)).await;
    let missing_server = server(&tree, &missing);
    let err = call(
        &missing_server,
        "delete_mesh_faces",
        json!({"nodeID": FILE_MESH_NODE, "faces": [0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "FileNotFound");
    assert_eq!(err.data["path"], json!(DOC_WIN));
    assert!(err.retry_safe);
    assert!(missing.method_calls("mesh.setGeometry").is_empty());
}

// -------------------------------------------------------------------------------------------
// Local refusals (nothing is dispatched)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn transform_mesh_argument_bounds() {
    let tree = Tree::new("transform-args");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;
    let created = mb.method_calls("mesh.setGeometry").len();

    // the POC's "give translate, rotateYaw and/or scale"
    for args in [
        json!({"nodeID": node_id}),
        json!({"nodeID": node_id, "rotateYaw": 0}),
    ] {
        let err = call(&server, "transform_mesh", args.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{args}: {err:?}");
        assert_eq!(err.message, "give translate, rotateYaw and/or scale", "{args}");
    }
    for (args, needle) in [
        (json!({"nodeID": node_id, "scale": 0}), "scale must be non-zero"),
        (json!({"nodeID": node_id, "scale": [1, 0, 1]}), "scale must be non-zero"),
        (json!({"nodeID": node_id, "scale": [1, 2]}), "scale must be a number"),
        (json!({"nodeID": node_id, "translate": [0, 0]}), "[x, y, z]"),
        (json!({"nodeID": node_id, "rotateYaw": "90"}), "rotateYaw must be a number"),
        (json!({"nodeID": node_id, "pivot": [0, 0, "x"]}), "pivot must be [x, y, z]"),
        (json!({"nodeID": -1, "translate": [0, 0, 1]}), "nodeID must be an integer"),
    ] {
        let err = call(&server, "transform_mesh", args.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{args}: {err:?}");
        assert!(err.message.contains(needle), "{args}: {}", err.message);
        assert!(err.retry_safe, "{args}");
    }
    // a translate that pushes the result outside the bridge's coordinate range is caught on the
    // *result* (the argument itself is fine, and these are not coordinates the bridge parses)
    let err = call(
        &server,
        "transform_mesh",
        json!({"nodeID": node_id, "translate": [1e6, 0, 0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    assert!(err.message.contains("outside"), "{}", err.message);
    assert_eq!(
        mb.method_calls("mesh.setGeometry").len(),
        created,
        "nothing was dispatched"
    );
}

/// The POC's mirroring rule: a scale on an odd number of negative axes flips every face's winding.
#[tokio::test]
async fn transform_mesh_mirrors_the_winding_on_an_odd_negative_scale() {
    let tree = Tree::new("mirror");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;

    let out = call(
        &server,
        "transform_mesh",
        json!({"nodeID": node_id, "scale": [-1, 1, 1], "pivot": [32, 32, 32]}),
    )
    .await
    .unwrap();
    let created = mb.method_calls("mesh.create")[0].params.clone();
    let sent = &mb.method_calls("mesh.setGeometry")[0].params;
    let reversed: Vec<Value> = created["faces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|face| {
            let mut loop_: Vec<Value> = face.as_array().unwrap().clone();
            loop_.reverse();
            Value::Array(loop_)
        })
        .collect();
    assert_eq!(sent["faces"], json!(reversed), "every loop is flipped");
    // review-2 N5: literal folded vertices (the mirror about the centre sends x to 64 - x),
    // so a transform that flipped the faces but ignored the scale cannot pass
    let expected: Vec<[f64; 3]> = vec![
        [64.0, 0.0, 0.0],
        [0.0, 0.0, 0.0],
        [64.0, 64.0, 0.0],
        [0.0, 64.0, 0.0],
        [64.0, 0.0, 64.0],
        [0.0, 0.0, 64.0],
        [64.0, 64.0, 64.0],
        [0.0, 64.0, 64.0],
    ];
    assert_eq!(sent_vertices(sent), expected);
    assert_eq!(out["faceCount"], json!(6));
    assert_eq!(
        out["bounds"],
        json!({"min": [0.0, 0.0, 0.0], "max": [64.0, 64.0, 64.0]}),
        "a mirror about the box's own centre maps the box onto itself"
    );

    // an even number of negative axes keeps the winding XX
    let mirrored = create_box(&server, 64.0).await;
    let out = call(
        &server,
        "transform_mesh",
        json!({"nodeID": mirrored, "scale": [-1, -1, 1], "pivot": [32, 32, 32]}),
    )
    .await
    .unwrap();
    let created = mb.method_calls("mesh.create")[1].params.clone();
    let sent = &mb.method_calls("mesh.setGeometry")[1].params;
    assert_eq!(
        sent["faces"], created["faces"],
        "an even mirror keeps the winding"
    );
    assert_eq!(out["faceCount"], json!(6));
    assert_eq!(
        out["bounds"],
        json!({"min": [0.0, 0.0, 0.0], "max": [64.0, 64.0, 64.0]})
    );
}

#[tokio::test]
async fn move_mesh_vertices_argument_bounds_and_a_flattened_face() {
    let tree = Tree::new("move-args");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;

    for (args, needle) in [
        (
            json!({"nodeID": node_id, "vertices": [], "offset": [0, 0, 1]}),
            "vertex indices must be in 0..7",
        ),
        (
            json!({"nodeID": node_id, "vertices": [8], "offset": [0, 0, 1]}),
            "vertex indices must be in 0..7",
        ),
        (
            json!({"nodeID": node_id, "vertices": [1.5], "offset": [0, 0, 1]}),
            "vertex indices must be in 0..7",
        ),
        (
            json!({"nodeID": node_id, "vertices": [0], "offset": [0, 0]}),
            "offset must be [dx, dy, dz]",
        ),
        (
            json!({"nodeID": node_id, "vertices": [0]}),
            "offset must be [dx, dy, dz]",
        ),
    ] {
        let err = call(&server, "move_mesh_vertices", args.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{args}: {err:?}");
        assert!(err.message.contains(needle), "{args}: {}", err.message);
    }
    assert!(mb.method_calls("mesh.setGeometry").is_empty());

    // a move that flattens a face is refused before dispatch (the POC checks the moved geometry)
    let triangle = call(
        &server,
        "create_mesh",
        json!({"vertices": [[0, 0, 0], [64, 0, 0], [0, 64, 0]], "faces": [[0, 1, 2]]}),
    )
    .await
    .unwrap();
    let triangle = triangle["nodeID"].as_i64().expect("nodeID");
    let err = call(
        &server,
        "move_mesh_vertices",
        json!({"nodeID": triangle, "vertices": [2], "offset": [64, -64, 0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    assert!(
        err.message.contains("zero-area") && err.message.contains("faces[0]"),
        "{}",
        err.message
    );
    assert!(
        mb.method_calls("mesh.setGeometry").is_empty(),
        "the flattened result never reached the bridge"
    );
}

#[tokio::test]
async fn extrude_mesh_face_adds_quads_and_keeps_the_cap_slot() {
    let tree = Tree::new("extrude");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;

    // face 1 is the top (geometry::r#box order: bottom, top, -y, +y, -x, +x)
    let out = call(
        &server,
        "extrude_mesh_face",
        json!({"nodeID": node_id, "face": 1, "distance": 32, "sideMaterial": NODRAW}),
    )
    .await
    .unwrap();

    let sent = &mb.method_calls("mesh.setGeometry")[0].params;
    assert_eq!(sent["nodeID"], json!(node_id));
    let faces = sent["faces"].as_array().expect("faces");
    assert_eq!(faces.len(), 10, "6 + one quad per edge: {sent}");
    assert_eq!(faces[1], json!([8, 9, 10, 11]), "the cap keeps its slot");
    assert_eq!(faces[6], json!([4, 5, 9, 8]));
    assert_eq!(faces[7], json!([5, 7, 10, 9]));
    assert_eq!(faces[9], json!([6, 4, 8, 11]));
    assert_eq!(sent_vertices(sent).len(), 12);
    assert_eq!(
        sent_vertices(sent)[8],
        [0.0, 0.0, 96.0],
        "the new cap is the old loop pushed +32 in z"
    );
    assert_eq!(sent["material"], json!(DEFAULT_MATERIAL));
    assert_eq!(override_at(sent, 1), Value::Null, "the cap keeps its material");
    assert_eq!(override_at(sent, 6), json!(NODRAW), "the new quads take sideMaterial");

    assert_eq!(out["nodeID"], json!(node_id));
    assert_eq!(out["geometrySource"], json!("live"));
    assert_eq!(out["vertexCount"], json!(12));
    assert_eq!(out["faceCount"], json!(10));
    assert_eq!(out["newFaces"], json!([6, 7, 8, 9]));
    assert_eq!(out["capFace"], json!(1));
    assert_eq!(out["undoLabel"], json!(GEOMETRY_LABEL));
    assert_eq!(
        out["undoNote"],
        json!("undo_last restores the previous geometry")
    );

    // a negative distance pushes inward and reverses the new quads (a fresh box: the operator works
    // on live geometry, which the extrude above rewrote)
    let recess = create_box(&server, 64.0).await;
    let out = call(
        &server,
        "extrude_mesh_face",
        json!({"nodeID": recess, "face": 1, "distance": -16}),
    )
    .await
    .unwrap();
    let sent = &mb.method_calls("mesh.setGeometry")[1].params;
    assert_eq!(sent["faces"][6], json!([8, 9, 5, 4]), "a recess flips the quads");
    assert_eq!(out["faceCount"], json!(10));
    assert_eq!(out["newFaces"], json!([6, 7, 8, 9]));
    // without sideMaterial the cap's own material is used for the new faces (the POC's default)
    assert!(
        sent.get("faceMaterials").is_none(),
        "one material for every face: {sent}"
    );

    // local refusals on a third, untouched box: an out-of-range face and a zero/missing distance
    let plain = create_box(&server, 64.0).await;
    let before = mb.method_calls("mesh.setGeometry").len();
    for (args, needle) in [
        (
            json!({"nodeID": plain, "face": 9, "distance": 16}),
            "face index outside 0..5",
        ),
        (json!({"nodeID": plain, "face": 0, "distance": 0}), "distance must be a non-zero number"),
        (json!({"nodeID": plain, "face": 0}), "distance must be a non-zero number"),
    ] {
        let err = call(&server, "extrude_mesh_face", args.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{args}: {err:?}");
        assert!(err.message.contains(needle), "{args}: {}", err.message);
        assert!(err.retry_safe, "{args}");
    }
    assert_eq!(mb.method_calls("mesh.setGeometry").len(), before);
    assert_eq!(mock_mesh(&mb, plain).map(|mesh| mesh.faces.len()), Some(6));
}

#[tokio::test]
async fn delete_mesh_faces_drops_loops_and_renumbers() {
    let tree = Tree::new("delete");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;

    let out = call(
        &server,
        "delete_mesh_faces",
        json!({"nodeID": node_id, "faces": [0, 2]}),
    )
    .await
    .unwrap();

    let sent = &mb.method_calls("mesh.setGeometry")[0].params;
    let created = &mb.method_calls("mesh.create")[0].params;
    let kept: Vec<Value> = [1, 3, 4, 5]
        .iter()
        .map(|index| created["faces"][*index].clone())
        .collect();
    assert_eq!(sent["faces"], json!(kept), "only the kept loops are sent");
    assert_eq!(sent_vertices(sent).len(), 8, "every vertex is still used");
    assert_eq!(out["faceCount"], json!(4));
    assert_eq!(out["vertexCount"], json!(8));
    assert_eq!(out["geometrySource"], json!("live"));
    assert_eq!(
        out["undoNote"],
        json!("undo_last restores the previous geometry")
    );
    assert!(
        notes(&out).iter().any(|note| note.contains("renumbered")),
        "the renumbering note is the POC's: {out}"
    );
    let stored = mock_mesh(&mb, node_id).expect("geometry stored");
    assert_eq!(stored.faces.len(), 4);

    // deleting every face is refused (delete the node instead), as are bad/empty index sets
    for (args, needle) in [
        (
            json!({"nodeID": node_id, "faces": [0, 1, 2, 3]}),
            "delete every face; delete the node instead",
        ),
        (
            json!({"nodeID": node_id, "faces": [4]}),
            "face indices must be in 0..3",
        ),
        (json!({"nodeID": node_id, "faces": []}), "face indices must be in 0..3"),
        (json!({"nodeID": node_id}), "face indices must be in 0..3"),
    ] {
        let err = call(&server, "delete_mesh_faces", args.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{args}: {err:?}");
        assert!(err.message.contains(needle), "{args}: {}", err.message);
    }
    assert_eq!(mb.method_calls("mesh.setGeometry").len(), 1);
}

// -------------------------------------------------------------------------------------------
// The error mapping the shared send path already has
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_bridge_target_errors_are_retry_safe_and_keep_the_counter() {
    let tree = Tree::new("target-errors");
    // no probe.vmap installed: the mesh comes from the live read
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;

    mb.add_error(
        "mesh.setGeometry",
        -32022,
        "NodeNotFound",
        Some(json!({"reason": "no node"})),
    );
    let err = call(
        &server,
        "transform_mesh",
        json!({"nodeID": node_id, "translate": [0, 0, 8]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "MeshNotFound", "{err:?}");
    assert_eq!(err.code, Some(-32022));
    assert!(err.retry_safe);
    assert!(
        err.data.get("unsavedBridgeEdits").is_none(),
        "a refused target does not touch the counter: {err:?}"
    );

    // ... and the counter really is still the create's 1 (the error above may not bump it)
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["server"]["unsavedBridgeEdits"], json!(1), "{status}");
    assert_eq!(
        status["unsavedWarning"],
        json!("1 unsaved bridge edits since last save"),
        "{status}"
    );
}

#[tokio::test]
async fn a_wrong_class_node_and_a_document_switch_pass_through() {
    let tree = Tree::new("wire-errors");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;

    mb.add_error(
        "mesh.setGeometry",
        -32602,
        "InvalidParams",
        Some(json!({"reason": "node is not a mesh"})),
    );
    let err = call(
        &server,
        "move_mesh_vertices",
        json!({"nodeID": node_id, "vertices": [0], "offset": [0, 0, 8]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NotAMesh", "{err:?}");
    assert!(err.retry_safe);
    assert!(err.data.get("unsavedBridgeEdits").is_none());

    mb.add_error(
        "mesh.setGeometry",
        -32004,
        "DocumentChanged",
        Some(json!({"reason": "doc_id"})),
    );
    let err = call(
        &server,
        "delete_mesh_faces",
        json!({"nodeID": node_id, "faces": [0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "DocumentChanged");
    assert_eq!(err.code, Some(-32004));
    assert!(err.retry_safe);
    assert!(!err.outcome_unknown);
    assert!(err.data.get("unsavedBridgeEdits").is_none());

    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["server"]["unsavedBridgeEdits"], json!(1), "{status}");
}

/// A `-32603` after dispatch may have left the mesh half-written (the bridge clears it before it
/// rebuilds): `outcomeUnknown` and the counter move.
#[tokio::test]
async fn an_internal_error_on_a_rebuild_is_maybe_executed() {
    let tree = Tree::new("internal");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;
    mb.add_error(
        "mesh.setGeometry",
        -32603,
        "InternalError",
        Some(json!({"reason": "could not clear the mesh"})),
    );

    let err = call(
        &server,
        "extrude_mesh_face",
        json!({"nodeID": node_id, "face": 1, "distance": 16}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "ProtocolError");
    assert!(err.outcome_unknown, "{err:?}");
    assert!(!err.retry_safe);
    assert_eq!(
        err.data["unsavedBridgeEdits"],
        json!(2),
        "the create plus this maybe-executed rebuild"
    );
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["server"]["unsavedBridgeEdits"], json!(2), "{status}");
}

// -------------------------------------------------------------------------------------------
// Capability gating and the document gates
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_operators_are_gated_on_mesh_set_geometry() {
    let tree = Tree::new("gating");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .methods(["control.ping", "doc.active"])
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    for (tool, args) in [
        ("transform_mesh", json!({"nodeID": 2, "translate": [0, 0, 1]})),
        (
            "move_mesh_vertices",
            json!({"nodeID": 2, "vertices": [0], "offset": [0, 0, 1]}),
        ),
        (
            "extrude_mesh_face",
            json!({"nodeID": 2, "face": 0, "distance": 8}),
        ),
        ("delete_mesh_faces", json!({"nodeID": 2, "faces": [0]})),
    ] {
        let err = call(&server, tool, args).await.unwrap_err();
        assert_eq!(err.name, "NotImplemented", "{tool}: {err:?}");
        assert_eq!(err.code, Some(-32011), "{tool}");
        assert_eq!(err.data["missing"], json!(["mesh.setGeometry"]), "{tool}");
        assert!(err.retry_safe, "{tool}");
    }
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
}

#[tokio::test]
async fn the_operators_without_an_active_document_are_no_document() {
    let tree = Tree::new("no-document");
    let mb = MockBridge::builder()
        .hammer_nodes(vec![MockNode::world()])
        .responder_value("doc.active", json!({"doc": null, "reason": "no_document"}))
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    for (tool, args) in [
        ("transform_mesh", json!({"nodeID": 2, "translate": [0, 0, 1]})),
        (
            "move_mesh_vertices",
            json!({"nodeID": 2, "vertices": [0], "offset": [0, 0, 1]}),
        ),
        (
            "extrude_mesh_face",
            json!({"nodeID": 2, "face": 0, "distance": 8}),
        ),
        ("delete_mesh_faces", json!({"nodeID": 2, "faces": [0]})),
    ] {
        let err = call(&server, tool, args).await.unwrap_err();
        assert_eq!(err.name, "NoDocument", "{tool}: {err:?}");
        assert_eq!(err.code, Some(-32003), "{tool}");
        assert!(err.retry_safe, "{tool}");
    }
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
}

/// The operators' tool descriptions say what the geometry source is (workflow 03's "SAVED .vmap"
/// vs "live" rule) and the four are registered as the POC's tools with the rebuild method.
#[tokio::test]
async fn the_operator_specs_describe_the_source_and_the_method() {
    let tree = Tree::new("specs");
    let no_bridge = tree.root.join("no-such-session.json");
    let server = hammer_mcp::build_server(&config(&tree, &no_bridge)).unwrap();
    for name in [
        "transform_mesh",
        "move_mesh_vertices",
        "extrude_mesh_face",
        "delete_mesh_faces",
    ] {
        let spec = server
            .registry()
            .describe()
            .into_iter()
            .find(|spec| spec.name == name)
            .unwrap_or_else(|| panic!("{name} is registered"));
        assert_eq!(spec.bridge_methods.len(), 1, "{name}");
        assert_eq!(spec.bridge_methods[0], "mesh.setGeometry", "{name}");
        let description = spec.description.to_lowercase();
        assert!(description.contains("live"), "{name}: {}", spec.description);
        // transform_mesh spells the geometry-source rule out; the others point at it
        if name == "transform_mesh" {
            assert!(
                description.contains("saved .vmap"),
                "{name}: {}", spec.description
            );
            assert!(
                description.contains("geometrysource"),
                "{name} names the result key: {}", spec.description
            );
        } else {
            assert!(
                description.contains("transform_mesh"),
                "{name} points at the rule: {}", spec.description
            );
        }
        assert!(
            description.contains("not saved"),
            "{name}: {}", spec.description
        );
    }
    // review-2 N2: `rotateYaw`'s schema names its sign like the render tools name theirs
    let spec = server
        .registry()
        .describe()
        .into_iter()
        .find(|spec| spec.name == "transform_mesh")
        .expect("transform_mesh registered");
    let yaw = &spec.input_schema["properties"]["rotateYaw"]["description"];
    let yaw = yaw.as_str().expect("rotateYaw has a description");
    assert!(
        yaw.to_lowercase().contains("counter-clockwise"),
        "the sign is documented (review-2 N2): {yaw}"
    );
}

// -------------------------------------------------------------------------------------------
// The review-2 fixes, kept on the live/file routes: the
// scaled file mesh (M1), the save-gate pin (L1), the argument order (N1), the argument caps (L5)
// and the doc-switch window (L2)
// -------------------------------------------------------------------------------------------

/// review-2 M1, re-checked for m3.12: the SAVED .vmap's mesh with an object-properties scale
/// carries WORLD vertices the extraction already scaled (`vmap/mesh`). `geo_reset_mesh` resets the
/// node's scales (and angles, pivot) since m2.18, so a rebuild from those world points is correct
/// and the file route no longer refuses it (the old refusal assumed a second scaling).
#[tokio::test]
async fn a_scaled_file_mesh_is_rebuilt_from_its_world_vertices() {
    let tree = Tree::new("scaled-file");
    // the fixture (nodeID 3 with scales [2, 2, 1]) is the *active* document file the mock's
    // doc.active names (maps/probe.vmap), and once more under its own name
    tree.install("probe_scaled.vmap", "maps/probe.vmap");
    tree.install("probe_scaled.vmap", "maps/probe_scaled.vmap");
    let mb = hammer_mock_file(vec![MockNode::world(), mesh_node(FILE_MESH_NODE)], Some(false)).await;
    let server = server(&tree, &mb);

    // the fixture is readable as any other (18 meshes; nodeID 3 there carries scales [2, 2, 1])
    let document = hammer_mcp::vmap::load(&tree.probe()).expect("probe.vmap parses");
    let record = document
        .mesh_by_id(FILE_MESH_NODE)
        .expect("nodeID 3 in the scaled fixture");
    assert_eq!(record.scales, Some([2.0, 2.0, 1.0]), "the fixture is the scaled one");
    assert_eq!(record.vertices.len(), 12);
    assert!(
        document.meshes().len() == 18,
        "the other 17 meshes are untouched: {}",
        document.meshes().len()
    );

    let world = record.vertices.clone();
    let out = call(
        &server,
        "transform_mesh",
        json!({"nodeID": FILE_MESH_NODE, "translate": [0, 0, 64]}),
    )
    .await
    .unwrap();
    assert_eq!(out["geometrySource"], json!("file"), "{out}");
    let sent = sent_vertices(&mb.method_calls("mesh.setGeometry")[0].params);
    let expected: Vec<[f64; 3]> =
        world.iter().map(|p| round4(add(*p, [0.0, 0.0, 64.0]))).collect();
    assert_eq!(sent, expected, "the world vertices, translated once, never scaled again");
}

/// Consecutive writes: every operator reads the live geometry, so an operator on a mesh written
/// *before* another write sees exactly what Hammer holds for it (`geometrySource: "live"`).
#[tokio::test]
async fn consecutive_writes_are_each_read_live() {
    let tree = Tree::new("live-batch");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let first = create_box(&server, 64.0).await;
    let second = create_box(&server, 48.0).await;

    let out = call(
        &server,
        "transform_mesh",
        json!({"nodeID": first, "translate": [8, 0, 0]}),
    )
    .await
    .unwrap();
    assert_eq!(out["geometrySource"], json!("live"), "{out}");

    // and the same on the mesh the second write just rewrote
    let out = call(
        &server,
        "move_mesh_vertices",
        json!({"nodeID": second, "vertices": [0], "offset": [0, 0, 8]}),
    )
    .await
    .unwrap();
    assert_eq!(out["geometrySource"], json!("live"));
}

/// Write, undo, rebuild: an operator after `undo_last` works on the geometry Hammer holds NOW. The
/// box is translated, the translate undone, and a vertex move then starts from the original box —
/// not from the translated shape the undone write sent.
#[tokio::test]
async fn an_operator_after_an_undo_starts_from_the_restored_geometry() {
    let tree = Tree::new("live-op-undo");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;
    let created = sent_vertices(&mb.method_calls("mesh.create")[0].params);

    call(
        &server,
        "transform_mesh",
        json!({"nodeID": node_id, "translate": [100, 0, 0]}),
    )
    .await
    .unwrap();
    let undone = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(undone["undone"], json!(true), "{undone}");

    let out = call(
        &server,
        "move_mesh_vertices",
        json!({"nodeID": node_id, "vertices": [0], "offset": [0, 0, 8]}),
    )
    .await
    .unwrap();
    assert_eq!(out["geometrySource"], json!("live"), "{out}");
    let sent = sent_vertices(&mb.method_calls("mesh.setGeometry").pop().expect("rebuild").params);
    let mut expected = created;
    expected[0][2] += 8.0;
    assert_eq!(sent, expected, "the undone translate did not come back");
}

/// A failed (maybe-executed) rebuild leaves nothing to go stale: the next operator reads Hammer
/// again (`mesh.get` once per call).
#[tokio::test]
async fn a_maybe_executed_failure_is_followed_by_a_fresh_live_read() {
    let tree = Tree::new("live-op-unknown");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;

    mb.add_error(
        "mesh.setGeometry",
        -32603,
        "InternalError",
        Some(json!({"reason": "could not clear the mesh"})),
    );
    let err = call(
        &server,
        "transform_mesh",
        json!({"nodeID": node_id, "translate": [0, 0, 8]}),
    )
    .await
    .unwrap_err();
    assert!(err.outcome_unknown, "{err:?}");
    mb.with_state(|state| state.errors.clear());
    let reads = mb.method_calls("mesh.get").len();
    let out = call(
        &server,
        "delete_mesh_faces",
        json!({"nodeID": node_id, "faces": [0]}),
    )
    .await
    .unwrap();
    assert_eq!(out["geometrySource"], json!("live"), "{out}");
    assert_eq!(mb.method_calls("mesh.get").len(), reads + 1);
}

/// review-1 L1 pinned to the mock (the orchestrator's live verdict: the dirty gate does cover an
/// undo past the save point — the counter saturates at 0 and `ChangedSinceSave` is the guard).
/// create (counter 1) -> save_map (0) -> undo_last past the save (counter still 0) ->
/// transform: refused by the dirty document, with an unsaved counter of 0.
#[tokio::test]
async fn undo_last_past_the_save_gate_is_the_dirty_flag() {
    let tree = Tree::new("l1-undo-past-save");
    let mb = hammer_mock_file(vec![MockNode::world()], Some(false)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;

    call(&server, "save_map", json!({})).await.unwrap();
    let undo = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(undo["undone"], json!(true), "{undo}");
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(
        status["server"]["unsavedBridgeEdits"],
        json!(0),
        "the counter saturates at 0 (the old guard is gone here)"
    );

    let err = call(
        &server,
        "transform_mesh",
        json!({"nodeID": node_id, "translate": [8, 0, 0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnsavedChanges", "{err:?}");
    assert_eq!(err.data["reason"], json!("dirty document"));
    assert_eq!(
        err.data["unsavedBridgeEdits"],
        json!(0),
        "the only guard left is Hammer's dirty flag, and it fired"
    );
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
}

/// review-2 L2: the document `current_mesh` reads is the one the write fingerprints — the tool
/// makes no second `doc.active` between its gate and its request, so a tab switch that lands in
/// the old second-read window is refused by the bridge's own `expectDoc` gate and nothing is
/// written (the seam switches the document identity right after read 1: the switch happens where
/// the file parse would be running live).
#[tokio::test]
async fn a_document_switch_between_the_read_and_the_write_is_refused_by_the_bridge() {
    let tree = Tree::new("l2-switch");
    tree.install("probe.vmap", "maps/probe.vmap");
    let mb = hammer_mock(vec![MockNode::world(), mesh_node(FILE_MESH_NODE)], Some(false)).await;
    let server = server(&tree, &mb);
    mb.set_hammer_doc_switch_after(Some(1));

    let err = call(
        &server,
        "transform_mesh",
        json!({"nodeID": FILE_MESH_NODE, "translate": [0, 0, 64]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "DocumentChanged", "{err:?}");
    assert_eq!(err.code, Some(-32004));
    assert!(err.retry_safe);
    assert!(!err.outcome_unknown);
    // the write reached the bridge and was refused there, pre-write: the document holds no mesh
    let stored = mb.with_state(|state| {
        state
            .hammer
            .as_ref()
            .expect("hammer world")
            .meshes
            .get(&FILE_MESH_NODE)
            .cloned()
    });
    assert!(
        stored.is_none(),
        "doc A's file geometry must not land in the switched-to document: {stored:?}"
    );
}

/// review-2 N1: the operator arguments are parsed before the geometry route runs — `{nodeID,
/// scale: 0}` on a dirty document answers `InvalidParams` with the geometry layer's message,
/// not the source route's `UnsavedChanges`.
#[tokio::test]
async fn transform_arguments_are_refused_before_the_source_route() {
    let tree = Tree::new("n1-args-first");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "transform_mesh",
        json!({"nodeID": 3, "scale": 0}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert_eq!(err.message, "scale must be non-zero numbers");
    assert!(
        mb.method_calls("doc.active").is_empty(),
        "the source route never ran: no document read, no live read, no file"
    );

    let err = call(
        &server,
        "transform_mesh",
        json!({"nodeID": 3, "rotateYaw": "not a number"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(mb.method_calls("doc.active").is_empty());
}

/// review-2 L5: an index list longer than the mesh's own index space necessarily repeats
/// entries and is refused locally (the POC's duplicate-as-a-set reading would walk them all).
#[tokio::test]
async fn index_lists_over_the_meshs_own_size_are_refused() {
    let tree = Tree::new("l5-cap");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;

    let err = call(
        &server,
        "move_mesh_vertices",
        json!({"nodeID": node_id, "vertices": [0, 1, 2, 3, 4, 5, 6, 7, 0, 1], "offset": [1, 0, 0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(
        err.message.contains("at most one entry per vertex"),
        "{}",
        err.message
    );
    assert!(
        mb.method_calls("mesh.setGeometry").is_empty(),
        "the cap runs before anything is sent"
    );

    // the same cap on delete_mesh_faces' `faces`
    let err = call(
        &server,
        "delete_mesh_faces",
        json!({"nodeID": node_id, "faces": [0, 1, 2, 3, 4, 5, 0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(err.message.contains("at most one entry per face"), "{}", err.message);
}

/// review-2 M2: the operator results carry the rebuilt counts (`send_geometry`'s
/// `vertexCount`/`faceCount`), and `get_mesh` (no path) — FIX-M1's answer, now the live read — reports the same numbering the operator used,
/// so the client need not re-read and the "indices from get_mesh" rule stays true.
#[tokio::test]
async fn operator_results_and_get_mesh_agree_on_the_rebuilt_geometry() {
    let tree = Tree::new("m2-agree");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;

    let out = call(
        &server,
        "delete_mesh_faces",
        json!({"nodeID": node_id, "faces": [0, 2]}),
    )
    .await
    .unwrap();
    assert_eq!(out["vertexCount"], json!(8), "the rebuilt count: {out}");
    assert_eq!(out["faceCount"], json!(4));
    let sent = &mb.method_calls("mesh.setGeometry")[0].params;
    assert_eq!(sent["faces"].as_array().map(Vec::len), Some(4));

    // the same read the caller would do next (the note's "call get_mesh again")
    let mesh = call(&server, "get_mesh", json!({"nodeID": node_id})).await.unwrap();
    assert_eq!(mesh["source"], json!("live"), "{mesh}");
    assert_eq!(mesh["vertexCount"], out["vertexCount"]);
    assert_eq!(mesh["faceCount"], out["faceCount"]);
    assert_eq!(mesh["faces"], sent["faces"]);

    // ... and the extrude's result counts match what a later face index means
    let caps = call(
        &server,
        "extrude_mesh_face",
        json!({"nodeID": node_id, "face": 0, "distance": 16}),
    )
    .await
    .unwrap();
    assert_eq!(caps["vertexCount"], json!(12));
    assert_eq!(caps["faceCount"], json!(8));
    let mesh = call(&server, "get_mesh", json!({"nodeID": node_id})).await.unwrap();
    assert_eq!(mesh["faces"], mb.method_calls("mesh.setGeometry")[1].params["faces"]);
}

/// review-2 N6: after a save made in Hammer's UI, the file route's counter gate refuses until
/// *this server's* `save_map` — the refusal says so.
#[tokio::test]
async fn the_unsaved_edits_refusal_names_the_hammer_ui_save() {
    let tree = Tree::new("n6-hint");
    tree.install("probe.vmap", "maps/probe.vmap");
    let clean = make_doc(DOC_WIN, Some(false));
    let mb = MockBridge::builder()
        .doc(Some(clean.clone()))
        .responder_value("doc.active", json!({"doc": clean}))
        .hammer_nodes(vec![MockNode::world(), mesh_node(FILE_MESH_NODE)])
        .hammer_mesh_get(false)
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);
    create_box(&server, 32.0).await; // counter 1

    let err = call(
        &server,
        "transform_mesh",
        json!({"nodeID": FILE_MESH_NODE, "translate": [0, 0, 64]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnsavedChanges");
    assert!(
        err.message.contains("not seen by this server"),
        "review-2 N6's wording: {}",
        err.message
    );
}

// -------------------------------------------------------------------------------------------
// m3.12 follow-ups: a mesh too large for one reply, parent containers, a transformed node
// -------------------------------------------------------------------------------------------

/// A container node of `class` with `parent` (a `MockNode` with no geometry).
fn container(id: i64, class: &str, parent: Option<i64>) -> MockNode {
    MockNode { class: class.into(), parent, ..mesh_node(id) }
}

/// A mesh `mesh.get` refuses as too large for one reply: with the document clean the operators and
/// `get_mesh` take the SAVED file (and say why); with unsaved changes they refuse with the counts
/// and the `save_map` hint, sending nothing.
#[tokio::test]
async fn a_mesh_too_large_for_one_reply_falls_back_to_the_clean_file_or_refuses() {
    let tree = Tree::new("too-large");
    tree.install("probe.vmap", "maps/probe.vmap");
    let clean = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world(), mesh_node(FILE_MESH_NODE)])
        .hammer_mesh_too_large(true)
        .start()
        .await
        .unwrap();
    let clean_server = server(&tree, &clean);

    let read = call(&clean_server, "get_mesh", json!({"nodeID": FILE_MESH_NODE})).await.unwrap();
    assert_eq!(read["source"], json!("file"), "{read}");
    assert!(
        read["notes"].to_string().contains("too large for one live mesh.get reply"),
        "{read}"
    );

    let out = call(
        &clean_server,
        "transform_mesh",
        json!({"nodeID": FILE_MESH_NODE, "translate": [0, 0, 64]}),
    )
    .await
    .unwrap();
    assert_eq!(out["geometrySource"], json!("file"), "{out}");
    assert!(
        notes(&out).iter().any(|note| note.contains("too large for one live mesh.get reply")),
        "{out}"
    );
    assert_eq!(clean.method_calls("mesh.setGeometry").len(), 1);

    // an unsaved document: the file would be stale, so refuse with the counts and the hint
    let dirty = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(vec![MockNode::world(), mesh_node(FILE_MESH_NODE)])
        .hammer_mesh_too_large(true)
        .start()
        .await
        .unwrap();
    let dirty_server = server(&tree, &dirty);
    for (tool, args) in [
        ("transform_mesh", json!({"nodeID": FILE_MESH_NODE, "translate": [0, 0, 64]})),
        ("get_mesh", json!({"nodeID": FILE_MESH_NODE})),
    ] {
        let err = call(&dirty_server, tool, args).await.unwrap_err();
        assert_eq!(err.name, "MeshTooLarge", "{tool}: {err:?}");
        assert!(err.retry_safe, "{tool}");
        assert_eq!(err.data["maxLineBytes"], json!(1_048_576), "{tool}");
        assert!(err.data.get("vertexCount").is_some() && err.data.get("faceCount").is_some());
        assert!(err.message.contains("save_map"), "{tool}: {}", err.message);
    }
    assert!(dirty.method_calls("mesh.setGeometry").is_empty());
}

/// The live route's parent gate (the file route's review-3 NEW-5, on `mesh.get`'s `parentChain`):
/// a mesh under an instance is refused by the rebuild operators and flagged by `get_mesh`; one
/// under a group is fine.
#[tokio::test]
async fn a_live_mesh_under_an_unproven_container_is_refused_by_the_operators() {
    let tree = Tree::new("live-parent");
    let mb = hammer_mock(
        vec![
            MockNode::world(),
            container(20, "instance", None),
            container(21, "mesh", Some(20)),
            container(30, "group", None),
            container(31, "mesh", Some(30)),
        ],
        Some(true),
    )
    .await;
    let server = server(&tree, &mb);

    let err = call(&server, "transform_mesh", json!({"nodeID": 21, "translate": [0, 0, 8]}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "MeshGeometryUnavailable", "{err:?}");
    assert_eq!(err.data["reason"], json!("unsupported parent"));
    assert_eq!(err.data["parent"], json!("CMapInstance"));
    assert!(err.message.contains("in the live document"), "{}", err.message);
    assert!(mb.method_calls("mesh.setGeometry").is_empty());

    let read = call(&server, "get_mesh", json!({"nodeID": 21})).await.unwrap();
    assert_eq!(read["worldSpace"], json!(false), "{read}");
    assert!(read["notes"].to_string().contains("CMapInstance"), "{read}");

    let read = call(&server, "get_mesh", json!({"nodeID": 31})).await.unwrap();
    assert_eq!(read["worldSpace"], json!(true), "{read}");
}

/// End to end on a transformed node: `mesh.get` reports node-LOCAL vertices plus the node's own
/// origin/angles/scales, `get_mesh` converts them to the world vertices that were sent, and a
/// rebuild operator sends the right world soup.
#[tokio::test]
async fn a_node_with_angles_and_scales_reads_back_world_and_rebuilds_from_the_world_soup() {
    use cs2_mcp_common::mock::MeshTransform;
    let tree = Tree::new("live-transformed");
    let mb = hammer_mock(vec![MockNode::world()], Some(true)).await;
    let server = server(&tree, &mb);
    let node_id = create_box(&server, 64.0).await;
    let world = sent_vertices(&mb.method_calls("mesh.create")[0].params);
    mb.set_hammer_mesh_transform(
        node_id,
        MeshTransform {
            origin: [10.0, 20.0, 30.0],
            angles: [0.0, 90.0, 0.0],
            scales: [2.0, 1.0, 0.5],
        },
    );

    // the bridge's raw reply is local; the tool's answer is the world geometry again
    let raw = {
        let mut bridge = mb.method_calls("mesh.get");
        assert!(bridge.is_empty(), "nothing read yet");
        bridge.clear();
        call(&server, "get_mesh", json!({"nodeID": node_id})).await.unwrap()
    };
    assert_eq!(raw["source"], json!("live"));
    let read: Vec<[f64; 3]> = raw["vertices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| [v[0].as_f64().unwrap(), v[1].as_f64().unwrap(), v[2].as_f64().unwrap()])
        .collect();
    for (got, want) in read.iter().zip(&world) {
        for axis in 0..3 {
            assert!((got[axis] - want[axis]).abs() < 1e-6, "{got:?} vs {want:?}");
        }
    }
    let local = &mb.method_calls("mesh.get")[0];
    assert_eq!(local.params["nodeID"], json!(node_id));

    // a rebuild: the sent soup is the world geometry moved by the operator, whatever the node's
    // own transform was (the bridge resets scales/angles/pivot before it builds from it)
    call(&server, "transform_mesh", json!({"nodeID": node_id, "translate": [0, 0, 8]}))
        .await
        .unwrap();
    let sent = sent_vertices(&mb.method_calls("mesh.setGeometry")[0].params);
    for (got, want) in sent.iter().zip(&world) {
        let want = [want[0], want[1], want[2] + 8.0];
        for axis in 0..3 {
            assert!((got[axis] - want[axis]).abs() < 1e-3, "{got:?} vs {want:?}");
        }
    }
}
