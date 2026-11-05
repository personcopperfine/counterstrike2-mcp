//! `set_trigger_volume` tests (WP-V): replacing a live brush entity's volume through
//! `mesh.setGeometry` on one of its tied meshes, over the mock bridge's Hammer world.
//!
//! Covered: the exact `mesh.setGeometry` request JSON (world-space box, `expectDoc`, the
//! material), the result shape (`set_mesh_geometry`'s plus `entityNodeID` and
//! `material`/`materialSource`), the mesh's own material from the live `mesh.get` read (`live`,
//! the `create_trigger` flow) and from the SAVED `.vmap` (`file`, only while the document is
//! clean), and the negative list of target/WP.md: not a brush entity (point entity, a mesh node,
//! a dead id), several meshes without `meshNodeID`, a `meshNodeID` not among the children, the
//! dirty / dirty-unknown / unsaved-edits document refusing the file route with `UnsavedChanges`
//! for a NON-trigger class (same as the rebuild operators; an explicit `material` needs no
//! source), the box/geometry matrix, unknown or ill-typed arguments, `children` absent from the
//! reply -> `BridgeTooOld` (unless `meshNodeID` names the mesh — the m2.18 path), the bridge's
//! geometry-write failure (`MeshNotFound`, and the maybe-executed `-32603` -> `outcomeUnknown`),
//! and `undo_last` restoring the previous volume.
//!
//! The FIX-V additions (review-rust.md B1/B2): a trigger class instead of the `UnsavedChanges`
//! refusal falls back to the FGD `auto_apply_material` (its own, else the constant trigger
//! default) with `materialSource:"fgd-default (mesh material unknown)"`, for the dirty gate, a
//! mesh the saved file does not hold and an untitled document alike — while a non-trigger class
//! and a class the FGD does not carry keep every refusal; and a truncated `children` list no
//! longer refuses a `meshNodeID` that is not among the arrived rows (it is sent with a note).
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockBuilder, MockMesh, MockNode};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod common;

const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";
/// The trigger material (`core/base.fgd`'s `auto_apply_material`; the tools' trigger default).
const TOOLSTRIGGER: &str = "materials/tools/toolstrigger.vmat";
/// An explicit material the tests pass.
const NODRAW: &str = "materials/tools/toolsnodraw.vmat";
/// `probe.vmap`'s room-shell mesh (WP-A's fixture): nodeID 3, one material.
const FILE_MESH_NODE: i64 = 3;
/// What `probe.vmap`'s room shell is painted with (its only material).
const FILE_MATERIAL: &str = "materials/dev/reflectivity_30.vmat";

// -------------------------------------------------------------------------------------------
// The project tree (the FGD fixture + `TRIGGER_FGD` — only the `create_trigger` flow of the
// live-read test needs it; this tool itself never reads the FGD) and the mock bridge
// -------------------------------------------------------------------------------------------

/// The trigger classes, mirroring `core/base.fgd` (the same fixture `tests/triggers_live.rs`
/// appends; copied here so this suite stays self-contained).
const TRIGGER_FGD: &str = "

// ---- WP-V test fixture: the core/base.fgd trigger chain -----------------------------------
@BaseClass base(Targetname) tags( Trigger )
	metadata
	{
		auto_apply_material = \"materials/tools/toolstrigger.vmat\"
		collision_group = \"trigger\"
		physics_only_model = true
	}
= TriggerOnce
[
	spawnflags(flags) =
	[
		1: \"Clients\" : 1
		2: \"NPCs\" : 0
		4096: \"Correctly account for object mass\" : 1
	]
	input Toggle(void) : \"Toggles this trigger between enabled and disabled states.\"
	output OnStartTouch(void) : \"Fired when an entity starts touching this trigger.\"
]

@BaseClass base(TriggerOnce) = Trigger []

@SolidClass base(TriggerOnce) = trigger_once
	\"A trigger volume that removes itself after it is triggered once.\"
[
	output OnTrigger(void) : \"Fired whenever the trigger is activated.\"
]

@SolidClass base(Trigger) = trigger_multiple
	\"A trigger volume that can be triggered multiple times.\"
[
	wait(float) : \"Delay Before Reset\" : 1 : \"Seconds before it can trigger again.\"
	output OnTrigger(void) : \"Fired whenever the trigger is activated.\"
]

// A trigger class with its OWN auto_apply_material: the FGD fallback must use it, not the
// constant. Resolves through TriggerOnce, so `is_trigger_class` is true.
@SolidClass base(TriggerOnce) tags( Trigger )
	metadata
	{
		auto_apply_material = \"materials/mcp_test/custom_trigger.vmat\"
	}
= trigger_custom_material_mini
	\"A trigger class with a material of its own.\"
[
	output OnTrigger(void) : \"Fired whenever the trigger is activated.\"
]

// A trigger class with NO auto_apply_material anywhere in its chain: the constant trigger default
// is the fallback.
@SolidClass base(Targetname) tags( Trigger ) = trigger_tagged_mini
	\"A trigger class without a FGD material (the constant default applies).\"
[
	output OnStartTouch(void) : \"Fired when an entity starts touching this trigger.\"
]
";

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "trigger-volume-{tag}-{}-{}",
            std::process::id(),
            TRAIL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("workshop/run/app")).expect("temp tree");
        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/fgd"));
        common::copy_tree(fixture, &dir.join("workshop/run/app/game"));
        let entry = dir.join("workshop/run/app/game/csgo/csgo.fgd");
        let mut text = std::fs::read_to_string(&entry).expect("fixture csgo.fgd");
        text.push_str(TRIGGER_FGD);
        std::fs::write(&entry, text).expect("csgo.fgd with the trigger classes");
        let app = dir.join("workshop/run/app");
        Tree { root: dir, app }
    }

    /// Copy a committed fixture into the addon at the addon-relative path (`maps/probe.vmap`).
    fn install(&self, fixture: &str, rel: &str) -> PathBuf {
        let target = self.app.join("content/csgo_addons/mcp_test").join(rel);
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
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

static TRAIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

fn build_server(tree: &Tree, mb: &MockBridge) -> McpServer<HammerState> {
    hammer_mcp::build_server(&config(tree, &mb.discovery_path)).unwrap()
}

fn kv(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A live mesh node (optionally tied to a brush entity).
fn mesh(id: i64, parent: Option<i64>) -> MockNode {
    MockNode {
        node_id: id,
        class: "mesh".into(),
        classname: String::new(),
        targetname: None,
        origin: [0.0; 3],
        angles: [0.0; 3],
        keyvalues: BTreeMap::new(),
        locked: false,
        parent,
    }
}

/// A brush entity `entity_id` of `classname` whose tied meshes are `mesh_ids`.
fn brush_world(entity_id: i64, classname: &str, mesh_ids: &[i64]) -> Vec<MockNode> {
    let mut nodes = vec![
        MockNode::world(),
        MockNode::entity(entity_id, classname, [0.0; 3], kv(&[])),
    ];
    for id in mesh_ids {
        nodes.push(mesh(*id, Some(entity_id)));
    }
    nodes
}

async fn hammer_mock(nodes: Vec<MockNode>, dirty: Option<bool>) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, dirty)))
        .hammer_nodes(nodes)
        .start()
        .await
        .unwrap()
}

/// [`hammer_mock`] for a bridge older than m3.12 (no `mesh.get`): the volume's material source is
/// the SAVED `.vmap` and its clean-document gate.
async fn hammer_mock_file(nodes: Vec<MockNode>, dirty: Option<bool>) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, dirty)))
        .hammer_nodes(nodes)
        .hammer_mesh_get(false)
        .start()
        .await
        .unwrap()
}

async fn hammer_mock_with(
    nodes: Vec<MockNode>,
    tweak: impl FnOnce(MockBuilder) -> MockBuilder,
) -> MockBridge {
    tweak(
        MockBridge::builder()
            .doc(Some(make_doc(DOC_WIN, Some(false))))
            .hammer_nodes(nodes),
    )
    .start()
    .await
    .unwrap()
}

/// One recorded request's params, with `deadlineMs` removed (its range is asserted instead).
fn wire(mb: &MockBridge, method: &str, index: usize) -> Value {
    let calls = mb.method_calls(method);
    let mut params = calls
        .get(index)
        .unwrap_or_else(|| panic!("{method} call {index} of {}", calls.len()))
        .params
        .clone();
    let deadline = params.get("deadlineMs").cloned().unwrap_or(Value::Null);
    assert!(
        deadline.as_u64().is_some_and(|ms| (1..=5000).contains(&ms)),
        "{method}: {params}"
    );
    params
        .as_object_mut()
        .expect("params object")
        .remove("deadlineMs");
    params
}

/// The exact geometry params a `box` volume must send (built with the same public builder the
/// tool uses, so the pin is exact without a hand-copied face list).
fn expected_box(min: [f64; 3], max: [f64; 3], material: &str) -> Value {
    let primitive = hammer_mcp::geometry::r#box(min, max).expect("box");
    json!({
        "vertices": primitive.mesh.vertices,
        "faces": primitive.mesh.faces,
        "material": material,
    })
}

/// Give a live mesh node real geometry (so the mock's undo snapshot restores something readable).
fn seed_box(mb: &MockBridge, id: i64) {
    let primitive = hammer_mcp::geometry::r#box([-8.0; 3], [8.0; 3]).expect("box");
    let faces = primitive.mesh.faces.len();
    mb.with_state(|state| {
        let world = state.hammer.as_mut().expect("the hammer world");
        world.meshes.insert(
            id,
            MockMesh {
                vertices: primitive.mesh.vertices.clone(),
                faces: primitive.mesh.faces.clone(),
                materials: vec![TOOLSTRIGGER.to_string(); faces],
            },
        );
    });
}

/// The geometry the mock world holds for `node_id`.
fn mock_mesh(mb: &MockBridge, node_id: i64) -> Option<MockMesh> {
    mb.with_state(|state| {
        state
            .hammer
            .as_ref()
            .and_then(|world| world.meshes.get(&node_id).cloned())
    })
}

/// A result's `notes` as one string (the assertions look for the wording the tool promises).
fn notes(out: &Value) -> String {
    out["notes"]
        .as_array()
        .map(|notes| {
            notes
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .unwrap_or_default()
}

// -------------------------------------------------------------------------------------------
// The happy paths
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_single_child_volume_is_replaced_world_space() {
    let tree = Tree::new("happy");
    let mb = hammer_mock(brush_world(60, "trigger_once", &[41]), Some(false)).await;
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "set_trigger_volume",
        json!({
            "nodeID": 60,
            "box": {"min": [0, 0, 0], "max": [64, 64, 96]},
            "material": NODRAW,
        }),
    )
    .await
    .unwrap();

    // the exact request: the caller's box on the CHILD mesh, world coordinates, expectDoc
    assert_eq!(
        wire(&mb, "mesh.setGeometry", 0),
        {
            let mut params = expected_box([0.0, 0.0, 0.0], [64.0, 64.0, 96.0], NODRAW);
            params["nodeID"] = json!(41);
            params["expectDoc"] = json!({"id": "d1", "epoch": 1});
            params
        },
        "the box is sent as mesh.setGeometry on the tied mesh"
    );

    // the result is set_mesh_geometry's, naming the entity and the material decision
    assert_eq!(out["nodeID"], json!(41), "the MESH's id");
    assert_eq!(out["entityNodeID"], json!(60));
    assert_eq!(out["material"], json!(NODRAW));
    assert_eq!(out["materialSource"], json!("argument"));
    assert_eq!(out["vertexCount"], json!(8));
    assert_eq!(out["faceCount"], json!(6));
    assert_eq!(
        out["bounds"],
        json!({"min": [0.0, 0.0, 0.0], "max": [64.0, 64.0, 96.0]})
    );
    assert_eq!(out["undoLabel"], json!("MCP: set mesh geometry"));
    assert_eq!(
        out["undoNote"],
        json!("undo_last restores the previous geometry")
    );
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
    assert_eq!(out["saved"], json!(false));

    // the document really holds it: the mesh keeps its parent, its pivot is re-centred
    let nodes = mb.hammer_nodes();
    let mesh_node = nodes.iter().find(|n| n.node_id == 41).expect("mesh 41");
    assert_eq!(mesh_node.parent, Some(60), "the mesh stays tied");
    assert_eq!(mesh_node.origin, [32.0, 32.0, 48.0], "pivot re-centred");
    let geometry = mock_mesh(&mb, 41).expect("geometry");
    let primitive = hammer_mcp::geometry::r#box([0.0; 3], [64.0, 64.0, 96.0]).expect("box");
    assert_eq!(geometry.vertices, primitive.mesh.vertices);
    assert_eq!(geometry.faces, primitive.mesh.faces);
    assert_eq!(geometry.materials, vec![NODRAW.to_string(); 6]);
}

#[tokio::test]
async fn the_meshs_own_material_is_kept_from_the_live_read() {
    let tree = Tree::new("cache");
    // the create_trigger flow's world (tests/triggers_live.rs `nodes()`), so the volume this test
    // resizes is one this server created — its material is read back live from the document, the
    // acceptance flow's own sequence (plan §3 step 4)
    let world = vec![
        MockNode::world(),
        MockNode::entity(
            60,
            "func_buyzone",
            [0.0; 3],
            kv(&[("targetname", "ta_kill")]),
        ),
        mesh(41, None),
        mesh(42, None),
    ];
    let mb = hammer_mock(world.clone(), Some(false)).await;
    let server = build_server(&tree, &mb);

    let created = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_once",
            "targetname": "ta_once",
            "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
        }),
    )
    .await
    .unwrap();
    let entity_id = created["nodeID"].as_i64().expect("entity");
    let mesh_id = created["meshes"][0]["nodeID"].as_i64().expect("mesh");
    assert_eq!(
        created["material"],
        json!(TOOLSTRIGGER),
        "the volume was painted toolstrigger at create time"
    );

    let out = call(
        &server,
        "set_trigger_volume",
        json!({
            "nodeID": entity_id,
            "box": {"min": [0, 0, 0], "max": [16, 16, 16]},
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["nodeID"], json!(mesh_id));
    assert_eq!(out["entityNodeID"], json!(entity_id));
    assert_eq!(
        out["material"],
        json!(TOOLSTRIGGER),
        "the mesh's own material was kept"
    );
    assert_eq!(out["materialSource"], json!("live"));
    let said = notes(&out);
    assert!(said.contains("read live"), "{said}");
    assert!(said.contains(TOOLSTRIGGER), "{said}");
    // the write went to the child mesh with the kept material
    let sent = wire(&mb, "mesh.setGeometry", 0);
    assert_eq!(sent["nodeID"], json!(mesh_id));
    assert_eq!(sent["material"], json!(TOOLSTRIGGER));
    assert_eq!(sent["expectDoc"], json!({"id": "d1", "epoch": 3}));
    // and the entity still holds exactly that child
    let read = call(&server, "get_entity_live", json!({"nodeID": entity_id}))
        .await
        .unwrap();
    assert_eq!(
        read["children"],
        json!([{"nodeID": mesh_id, "class": "mesh"}])
    );
}

#[tokio::test]
async fn the_meshs_own_material_is_kept_from_the_saved_vmap() {
    let tree = Tree::new("file");
    tree.install("probe.vmap", "maps/probe.vmap");
    let mb = hammer_mock_file(
        brush_world(60, "trigger_once", &[FILE_MESH_NODE]),
        Some(false),
    )
    .await;
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "set_trigger_volume",
        json!({
            "nodeID": 60,
            "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["nodeID"], json!(FILE_MESH_NODE));
    assert_eq!(
        out["material"],
        json!(FILE_MATERIAL),
        "the SAVED .vmap's room-shell material"
    );
    assert_eq!(out["materialSource"], json!("file"));
    let said = notes(&out);
    assert!(said.contains("SAVED .vmap"), "{said}");
    assert_eq!(
        wire(&mb, "mesh.setGeometry", 0)["material"],
        json!(FILE_MATERIAL)
    );
}

#[tokio::test]
async fn an_explicit_material_needs_no_source_even_when_the_document_is_dirty() {
    let tree = Tree::new("dirty-arg");
    tree.install("probe.vmap", "maps/probe.vmap");
    let mb = hammer_mock(
        brush_world(60, "trigger_once", &[FILE_MESH_NODE]),
        Some(true),
    )
    .await;
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "set_trigger_volume",
        json!({
            "nodeID": 60,
            "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
            "material": NODRAW,
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["material"], json!(NODRAW));
    assert_eq!(out["materialSource"], json!("argument"));
    assert_eq!(wire(&mb, "mesh.setGeometry", 0)["material"], json!(NODRAW));
    assert!(
        !notes(&out).contains("SAVED .vmap"),
        "the file was never read: {}",
        notes(&out)
    );
}

// -------------------------------------------------------------------------------------------
// Mesh selection (the WP-V negatives)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn not_a_brush_entity_is_refused() {
    let tree = Tree::new("not-brush");
    // nodeID 60 is a POINT entity (no mesh children), 41 is an untied mesh node, 999 nothing
    let mb = hammer_mock(
        vec![
            MockNode::world(),
            MockNode::entity(60, "info_target", [0.0; 3], kv(&[])),
            mesh(41, None),
        ],
        Some(false),
    )
    .await;
    let server = build_server(&tree, &mb);

    for (node_id, why) in [
        (60, "the point entity"),
        (41, "the mesh node (the bridge's not-an-entity read)"),
    ] {
        let err = call(
            &server,
            "set_trigger_volume",
            json!({"nodeID": node_id, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.name, "NotABrushEntity", "{node_id}: {err:?}");
        assert_eq!(err.type_, "NotABrushEntity", "{node_id}");
        assert_eq!(err.code, Some(-32602), "{node_id}");
        assert!(err.retry_safe, "{node_id}: no write was sent");
        assert!(
            err.message.contains("no write was sent"),
            "{node_id}: a read was sent, so the wording must say no WRITE was: {}",
            err.message
        );
        assert_eq!(err.data["nodeID"], json!(node_id), "{why}");
    }
    let err = call(
        &server,
        "set_trigger_volume",
        json!({"nodeID": 999, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NodeNotFound", "{err:?}");
    assert_eq!(err.code, Some(-32022));

    assert!(
        mb.method_calls("mesh.setGeometry").is_empty(),
        "no volume was written"
    );
}

#[tokio::test]
async fn several_mesh_children_without_a_selector_are_refused() {
    let tree = Tree::new("multiple");
    let mb = hammer_mock(brush_world(60, "trigger_once", &[41, 42]), Some(false)).await;
    let server = build_server(&tree, &mb);

    let err = call(
        &server,
        "set_trigger_volume",
        json!({"nodeID": 60, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "MultipleMeshes", "{err:?}");
    assert_eq!(err.code, Some(-32602));
    assert!(err.retry_safe);
    assert_eq!(err.data["nodeID"], json!(60));
    assert_eq!(err.data["meshes"], json!([41, 42]));
    assert!(err.message.contains("meshNodeID"), "{}", err.message);
    assert!(
        err.message.contains("No write was sent"),
        "the refusals after the entity.get read say no WRITE was sent: {}",
        err.message
    );
    assert!(
        mb.method_calls("mesh.setGeometry").is_empty(),
        "nothing was sent"
    );

    // naming one of them proceeds: the selector is the volume
    let out = call(
        &server,
        "set_trigger_volume",
        json!({
            "nodeID": 60,
            "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
            "meshNodeID": 41,
            "material": NODRAW,
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["nodeID"], json!(41));
    assert_eq!(out["entityNodeID"], json!(60));
    assert_eq!(wire(&mb, "mesh.setGeometry", 0)["nodeID"], json!(41));
}

#[tokio::test]
async fn a_mesh_node_id_not_among_the_children_is_refused() {
    let tree = Tree::new("not-child");
    // 41 is the entity's child; 42 is an untied world mesh; 43 does not exist
    let mb = hammer_mock(
        vec![
            MockNode::world(),
            MockNode::entity(60, "trigger_once", [0.0; 3], kv(&[])),
            mesh(41, Some(60)),
            mesh(42, None),
        ],
        Some(false),
    )
    .await;
    let server = build_server(&tree, &mb);

    for selector in [42, 43] {
        let err = call(
            &server,
            "set_trigger_volume",
            json!({
                "nodeID": 60,
                "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
                "meshNodeID": selector,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.name, "MeshNotAChild", "{selector}: {err:?}");
        assert_eq!(err.code, Some(-32602), "{selector}");
        assert!(err.retry_safe, "{selector}: no write was sent");
        assert!(
            err.message.contains("no write was sent"),
            "{selector}: {}",
            err.message
        );
        assert!(
            !err.data.get("truncated").is_some_and(|v| v == &json!(true)),
            "{selector}: the list was not truncated, so it is proven absent"
        );
        assert_eq!(err.data["meshNodeID"], json!(selector), "{selector}");
        assert_eq!(err.data["meshes"], json!([41]), "{selector}");
    }
    assert!(
        mb.method_calls("mesh.setGeometry").is_empty(),
        "the write would have reshaped the wrong mesh"
    );
}

/// FIX (review-rust.md B2): a TRUNCATED `children` list cannot prove that an unnamed
/// `meshNodeID` is absent (the bridge caps the list at 1024 rows), so a selector that is not in
/// the arrived rows is sent anyway, with a note saying it went out unvalidated — refusing it would
/// reject a real child that merely fell off the list. Fails without the fix: `MeshNotAChild`.
#[tokio::test]
async fn a_truncated_children_list_sends_the_unchecked_selector() {
    let tree = Tree::new("truncated");
    let mb = hammer_mock_with(
        vec![
            MockNode::world(),
            MockNode::entity(60, "trigger_once", [0.0; 3], kv(&[])),
            mesh(41, Some(60)),
            mesh(42, Some(60)),
        ],
        // only mesh 41 arrived, and the bridge says the list was truncated: 42 may be a child
        |builder| {
            builder.responder("entity.get", |params| {
                json!({
                    "nodeID": params["nodeID"],
                    "classname": "trigger_once",
                    "children": [{"nodeID": 41, "class": "mesh"}],
                    "childrenTruncated": true,
                })
            })
        },
    )
    .await;
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "set_trigger_volume",
        json!({
            "nodeID": 60,
            "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
            "meshNodeID": 42,
            "material": NODRAW,
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["nodeID"], json!(42));
    assert_eq!(out["entityNodeID"], json!(60));
    assert_eq!(wire(&mb, "mesh.setGeometry", 0)["nodeID"], json!(42));
    let said = notes(&out);
    assert!(said.contains("TRUNCATED"), "{said}");
    assert!(said.contains("unvalidated"), "{said}");
    assert!(said.contains("`meshNodeID` 42"), "{said}");
}

#[tokio::test]
async fn children_absent_from_the_reply_is_bridge_too_old_unless_the_selector_names_the_mesh() {
    let tree = Tree::new("too-old");
    // an entity.get reply without `children` (the pre-m2.19 shape): the tool cannot enumerate
    let mb = hammer_mock_with(brush_world(60, "trigger_once", &[41]), |builder| {
        builder.responder("entity.get", |params| {
            json!({
                "nodeID": params["nodeID"],
                "classname": "trigger_once",
                "targetname": Value::Null,
                "origin": [0.0, 0.0, 0.0],
                "angles": [0.0, 0.0, 0.0],
                "scales": [1.0, 1.0, 1.0],
                "keyValues": {},
                "keyValueCount": 0,
                "unsupported": [],
                "connections": [],
                "source": "live",
            })
        })
    })
    .await;
    let server = build_server(&tree, &mb);

    let err = call(
        &server,
        "set_trigger_volume",
        json!({"nodeID": 60, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "BridgeTooOld", "{err:?}");
    assert_eq!(err.type_, "BridgeTooOld");
    assert_eq!(err.code, Some(-32011));
    assert!(err.retry_safe);
    assert_eq!(err.data["nodeID"], json!(60));
    assert_eq!(err.data["missing"], json!(["entity.get children"]));
    assert_eq!(
        err.data["blockedOn"],
        json!("bridge m2.19 (entity.get children)")
    );
    assert!(
        mb.method_calls("mesh.setGeometry").is_empty(),
        "nothing was sent"
    );

    // with `meshNodeID` the read is not needed: the m2.18 path works
    let out = call(
        &server,
        "set_trigger_volume",
        json!({
            "nodeID": 60,
            "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
            "meshNodeID": 41,
            "material": NODRAW,
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["nodeID"], json!(41));
    assert_eq!(out["materialSource"], json!("argument"));
    let said = notes(&out);
    assert!(said.contains("meshNodeID"), "{said}");
    assert!(said.contains("unvalidated"), "{said}");
    assert_eq!(wire(&mb, "mesh.setGeometry", 0)["nodeID"], json!(41));
}

// -------------------------------------------------------------------------------------------
// The material source's clean-document gate (the rebuild operators' rule), and the trigger-class
// fallback (review-rust.md B1)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_dirty_document_refuses_the_file_route_with_unsaved_changes() {
    let tree = Tree::new("dirty");
    tree.install("probe.vmap", "maps/probe.vmap");
    // a NON-trigger solid class: its material shows in game, so the gate still refuses (the
    // trigger-class fallback below covers the create-undo-resize case)
    let mb = hammer_mock_file(
        brush_world(60, "func_buyzone", &[FILE_MESH_NODE]),
        Some(true),
    )
    .await;
    let server = build_server(&tree, &mb);

    let err = call(
        &server,
        "set_trigger_volume",
        json!({"nodeID": 60, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnsavedChanges", "{err:?}");
    assert_eq!(err.type_, "UnsavedChanges");
    assert_eq!(err.code, Some(-32016));
    assert!(err.retry_safe, "no write was sent");
    assert_eq!(err.data["nodeID"], json!(FILE_MESH_NODE));
    assert_eq!(err.data["geometrySource"], json!("unavailable"));
    assert_eq!(err.data["reason"], json!("dirty document"));
    assert_eq!(err.data["unsavedBridgeEdits"], json!(0));
    assert!(
        err.message.contains("save_map") && err.message.contains("`material`"),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("No write was sent"),
        "the entity.get read happened, so the refusal says no WRITE was sent: {}",
        err.message
    );
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
    // one `doc.active` per call: the refusal happens before anything is read or sent
    assert_eq!(mb.method_calls("doc.active").len(), 1);

    // a document that does not report `dirty` at all is refused the same way (the `reload_map`
    // rule the rebuild operators share)
    let mb = MockBridge::builder()
        .hammer_mesh_get(false)
        .doc(Some(make_doc(DOC_WIN, None)))
        .responder_value("doc.active", json!({"doc": make_doc(DOC_WIN, None)}))
        .hammer_nodes(brush_world(60, "func_buyzone", &[FILE_MESH_NODE]))
        .start()
        .await
        .unwrap();
    let server = build_server(&tree, &mb);
    let err = call(
        &server,
        "set_trigger_volume",
        json!({"nodeID": 60, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnsavedChanges");
    assert_eq!(err.data["reason"], json!("dirty state unreported"));
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
}

/// FIX (review-rust.md B1): a trigger class does NOT get the UnsavedChanges refusal — its material
/// is invisible in game, so the mesh's own material comes from the class's FGD
/// `auto_apply_material` instead, with `materialSource:"fgd-default (mesh material unknown)"` and a
/// note naming the refusal. This is the `create_trigger` -> `undo_last` (the volume is read live, or from the file on an older bridge) ->
/// resize sequence the pure gate refused. Fails without the fix: `UnsavedChanges`.
#[tokio::test]
async fn a_trigger_class_falls_back_to_the_fgd_material_when_the_document_is_dirty() {
    let tree = Tree::new("dirty-trigger");
    tree.install("probe.vmap", "maps/probe.vmap");
    let mb = hammer_mock_file(brush_world(60, "trigger_once", &[41]), Some(true)).await;
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "set_trigger_volume",
        json!({"nodeID": 60, "box": {"min": [0, 0, 0], "max": [16, 16, 16]}}),
    )
    .await
    .unwrap();
    assert_eq!(out["nodeID"], json!(41));
    assert_eq!(out["entityNodeID"], json!(60));
    assert_eq!(
        out["material"],
        json!(TOOLSTRIGGER),
        "the class's FGD auto_apply_material"
    );
    assert_eq!(
        out["materialSource"],
        json!("fgd-default (mesh material unknown)")
    );
    let said = notes(&out);
    assert!(said.contains("mesh's own material is unknown"), "{said}");
    assert!(said.contains("UnsavedChanges"), "{said}");
    assert!(said.contains("dirty document"), "{said}");
    assert!(said.contains("FGD auto_apply_material"), "{said}");
    assert!(said.contains(TOOLSTRIGGER), "{said}");
    // the write really went out, with the FGD material, and the document holds it
    let sent = wire(&mb, "mesh.setGeometry", 0);
    assert_eq!(sent["nodeID"], json!(41));
    assert_eq!(sent["material"], json!(TOOLSTRIGGER));
    assert_eq!(
        mock_mesh(&mb, 41).expect("the rebuilt geometry").materials,
        vec![TOOLSTRIGGER.to_string(); 6]
    );
}

/// The fallback uses the class's OWN `auto_apply_material` when the FGD carries one (not the
/// constant), and the constant trigger default when no class in the chain carries it. Both are
/// `materialSource:"fgd-default (mesh material unknown)"`.
#[tokio::test]
async fn the_trigger_fallback_prefers_the_fgd_material_over_the_constant() {
    let tree = Tree::new("trigger-material");
    tree.install("probe.vmap", "maps/probe.vmap");
    for (class, material, origin) in [
        (
            "trigger_custom_material_mini",
            "materials/mcp_test/custom_trigger.vmat",
            "FGD auto_apply_material",
        ),
        (
            "trigger_tagged_mini",
            TOOLSTRIGGER,
            "trigger default material",
        ),
    ] {
        let mb = hammer_mock(brush_world(60, class, &[41]), Some(true)).await;
        let server = build_server(&tree, &mb);
        let out = call(
            &server,
            "set_trigger_volume",
            json!({"nodeID": 60, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
        )
        .await
        .unwrap_or_else(|error| panic!("{class}: {error:?}"));
        assert_eq!(out["material"], json!(material), "{class}");
        assert_eq!(
            out["materialSource"],
            json!("fgd-default (mesh material unknown)"),
            "{class}"
        );
        let said = notes(&out);
        assert!(said.contains(origin), "{class}: {said}");
        assert_eq!(
            wire(&mb, "mesh.setGeometry", 0)["material"],
            json!(material),
            "{class}"
        );
    }
}

/// The fallback also covers a trigger class whose mesh the clean SAVED file does not hold
/// (`MeshNotFound` for a non-trigger class, see the test below) and the untitled-document case.
/// Fails without the fix: the refusal.
#[tokio::test]
async fn a_trigger_class_falls_back_when_the_saved_file_cannot_supply_the_material() {
    let tree = Tree::new("trigger-file");
    tree.install("probe.vmap", "maps/probe.vmap");
    // (a) the clean file does not hold the live mesh 77
    let mb = hammer_mock_file(brush_world(60, "trigger_once", &[77]), Some(false)).await;
    let server = build_server(&tree, &mb);
    let out = call(
        &server,
        "set_trigger_volume",
        json!({"nodeID": 60, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap();
    assert_eq!(out["nodeID"], json!(77));
    assert_eq!(
        out["materialSource"],
        json!("fgd-default (mesh material unknown)")
    );
    let said = notes(&out);
    assert!(said.contains("MeshNotFound"), "{said}");
    assert!(said.contains("no mesh with nodeID 77"), "{said}");

    // (b) an untitled document: no SAVED .vmap to read at all
    let mb = hammer_mock_with(brush_world(60, "trigger_once", &[41]), |builder| {
        builder.hammer_mesh_get(false).responder_value(
            "doc.active",
            json!({"doc": {"id": "d1", "epoch": 1, "dirty": false, "path": "", "untitled": true}}),
        )
    })
    .await;
    let server = build_server(&tree, &mb);
    let out = call(
        &server,
        "set_trigger_volume",
        json!({"nodeID": 60, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap();
    assert_eq!(out["material"], json!(TOOLSTRIGGER));
    assert_eq!(
        out["materialSource"],
        json!("fgd-default (mesh material unknown)")
    );
    let said = notes(&out);
    assert!(said.contains("MeshGeometryUnavailable"), "{said}");
    assert!(said.contains("untitled"), "{said}");
}

/// NEGATIVE: the fallback is exactly the trigger classes' — a NON-trigger solid class keeps the
/// `UnsavedChanges` refusal, and so does a class the FGD does not carry (no invented material).
#[tokio::test]
async fn the_fgd_fallback_does_not_apply_to_non_trigger_classes() {
    let tree = Tree::new("no-fallback");
    tree.install("probe.vmap", "maps/probe.vmap");
    for (class, why) in [
        ("func_buyzone", "a Solid class that is not a trigger"),
        ("trigger_potato_mini", "a class the FGD does not carry"),
        ("info_target", "a Point class"),
    ] {
        let mb = hammer_mock_file(brush_world(60, class, &[FILE_MESH_NODE]), Some(true)).await;
        let server = build_server(&tree, &mb);
        let err = call(
            &server,
            "set_trigger_volume",
            json!({"nodeID": 60, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.name, "UnsavedChanges", "{class} ({why}): {err:?}");
        assert_eq!(err.code, Some(-32016), "{class}");
        assert!(
            mb.method_calls("mesh.setGeometry").is_empty(),
            "{class}: no write"
        );
    }
}

#[tokio::test]
async fn unsaved_mcp_edits_refuse_the_file_route_too() {
    let tree = Tree::new("unsaved");
    tree.install("probe.vmap", "maps/probe.vmap");
    let clean = make_doc(DOC_WIN, Some(false));
    let mb = MockBridge::builder()
        .hammer_mesh_get(false)
        .doc(Some(clean.clone()))
        .responder_value("doc.active", json!({"doc": clean}))
        .hammer_nodes(brush_world(60, "func_buyzone", &[FILE_MESH_NODE]))
        .start()
        .await
        .unwrap();
    let server = build_server(&tree, &mb);

    // one MCP edit (a world box this server created — NOT the volume mesh, whose live read
    // is not at issue here) is enough: the file would not contain it
    let created = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [32, 32, 32]}),
    )
    .await
    .unwrap();
    assert!(created["nodeID"].as_i64().unwrap_or(0) > 0);
    let err = call(
        &server,
        "set_trigger_volume",
        json!({"nodeID": 60, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnsavedChanges", "{err:?}");
    assert_eq!(err.data["reason"], json!("unsaved bridge edits"));
    assert_eq!(err.data["unsavedBridgeEdits"], json!(1));
    assert!(err.retry_safe);
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
}

#[tokio::test]
async fn a_mesh_the_saved_file_does_not_hold_refuses_the_material_route() {
    let tree = Tree::new("not-in-file");
    tree.install("probe.vmap", "maps/probe.vmap");
    // the live mesh 77 exists but the SAVED probe.vmap has no such node; the document is clean
    let mb = hammer_mock_file(brush_world(60, "func_buyzone", &[77]), Some(false)).await;
    let server = build_server(&tree, &mb);

    let err = call(
        &server,
        "set_trigger_volume",
        json!({"nodeID": 60, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "MeshNotFound", "{err:?}");
    assert!(err.retry_safe);
    assert_eq!(err.data["nodeID"], json!(77));
    assert!(err.message.contains("save_map"), "{}", err.message);
    assert!(mb.method_calls("mesh.setGeometry").is_empty());
}

// -------------------------------------------------------------------------------------------
// Argument negatives (nothing sent)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn bad_volume_arguments_send_nothing() {
    let tree = Tree::new("volume-args");
    let mb = hammer_mock(brush_world(60, "trigger_once", &[41]), Some(false)).await;
    let server = build_server(&tree, &mb);

    let cases: Vec<(&str, Value)> = vec![
        (
            "two volumes",
            json!({"box": {"min": [0, 0, 0], "max": [8, 8, 8]}, "geometry": {"vertices": [[0,0,0],[1,0,0],[0,1,0]], "faces": [[0,1,2]]}}),
        ),
        ("no volume", json!({"nodeID": 60})),
        ("null volume", json!({"nodeID": 60, "box": null})),
        (
            "min >= max",
            json!({"box": {"min": [8, 0, 0], "max": [0, 8, 8]}}),
        ),
        (
            "min == max",
            json!({"box": {"min": [0, 0, 0], "max": [0, 8, 8]}}),
        ),
        (
            "too small",
            json!({"box": {"min": [0, 0, 0], "max": [0.5, 8, 8]}}),
        ),
        (
            "non-finite",
            json!({"box": {"min": ["inf", 0, 0], "max": [8, 8, 8]}}),
        ),
        (
            "over 1048576",
            json!({"box": {"min": [0, 0, 0], "max": [8, 8, 1048577]}}),
        ),
        (
            "box axes",
            json!({"box": {"min": [0, 0], "max": [8, 8, 8]}}),
        ),
        (
            "box member",
            json!({"box": {"min": [0, 0, 0], "max": [8, 8, 8], "nope": 1}}),
        ),
        ("box missing max", json!({"box": {"min": [0, 0, 0]}})),
        ("box wrong type", json!({"box": [0, 0, 0, 8, 8, 8]})),
        (
            "degenerate geometry",
            json!({"geometry": {"vertices": [[0, 0, 0], [8, 0, 0], [4, 0, 0]], "faces": [[0, 1, 2]]}}),
        ),
        (
            "geometry index",
            json!({"geometry": {"vertices": [[0, 0, 0], [8, 0, 0], [0, 8, 0]], "faces": [[0, 1, 9]]}}),
        ),
        (
            "geometry member",
            json!({"geometry": {"vertices": [[0, 0, 0], [8, 0, 0], [0, 8, 0]], "faces": [[0, 1, 2]], "nope": 1}}),
        ),
    ];
    for (label, mut args) in cases {
        if let Some(object) = args.as_object_mut() {
            if !object.contains_key("nodeID") {
                object.insert("nodeID".into(), json!(60));
            }
        }
        let before = mb.requests().len();
        let err = call(&server, "set_trigger_volume", args.clone())
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{label}: {err:?}");
        assert_eq!(err.code, Some(-32602), "{label}");
        assert!(!err.message.is_empty(), "{label}");
        if mb.requests().len() != before {
            panic!("{label} sent {:?}", &mb.requests()[before..]);
        }
    }
    assert!(
        mb.requests().is_empty(),
        "every volume refusal sent nothing at all: {:?}",
        mb.requests()
    );
}

#[tokio::test]
async fn unknown_or_ill_typed_arguments_send_nothing() {
    let tree = Tree::new("arg-types");
    let mb = hammer_mock(brush_world(60, "trigger_once", &[41]), Some(false)).await;
    let server = build_server(&tree, &mb);

    let box_ = json!({"min": [0, 0, 0], "max": [8, 8, 8]});
    let cases: Vec<(&str, Value)> = vec![
        (
            "unknown argument",
            json!({"nodeID": 60, "box": box_, "volume": 1}),
        ),
        ("nodeID missing", json!({"box": box_})),
        ("nodeID negative", json!({"nodeID": -1, "box": box_})),
        (
            "nodeID above int",
            json!({"nodeID": 2_147_483_648i64, "box": box_}),
        ),
        ("nodeID string", json!({"nodeID": "60", "box": box_})),
        ("nodeID float", json!({"nodeID": 60.5, "box": box_})),
        (
            "meshNodeID negative",
            json!({"nodeID": 60, "box": box_, "meshNodeID": -1}),
        ),
        (
            "meshNodeID string",
            json!({"nodeID": 60, "box": box_, "meshNodeID": "41"}),
        ),
        (
            "meshNodeID float",
            json!({"nodeID": 60, "box": box_, "meshNodeID": 41.5}),
        ),
        (
            "material number",
            json!({"nodeID": 60, "box": box_, "material": 7}),
        ),
        (
            "material empty",
            json!({"nodeID": 60, "box": box_, "material": "  "}),
        ),
        (
            "material control char",
            json!({"nodeID": 60, "box": box_, "material": "materials/a\nb.vmat"}),
        ),
        (
            "material over the buffer",
            json!({"nodeID": 60, "box": box_, "material": "m/".to_string() + &"x".repeat(300)}),
        ),
    ];
    for (label, args) in cases {
        let before = mb.requests().len();
        let err = call(&server, "set_trigger_volume", args.clone())
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{label}: {err:?}");
        assert_eq!(err.code, Some(-32602), "{label}");
        if mb.requests().len() != before {
            panic!("{label} sent {:?}", &mb.requests()[before..]);
        }
    }
    assert!(
        mb.requests().is_empty(),
        "every argument refusal sent nothing at all: {:?}",
        mb.requests()
    );
}

// -------------------------------------------------------------------------------------------
// The write's failure mapping (Result = set_mesh_geometry's, errors included)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_geometry_write_failure_maps_like_set_mesh_geometry() {
    let tree = Tree::new("write-fail");
    let mb = hammer_mock(brush_world(60, "trigger_once", &[41]), Some(false)).await;
    mb.add_error(
        "mesh.setGeometry",
        -32022,
        "NodeNotFound",
        Some(json!({"reason": "nodeID 41 not found"})),
    );
    let server = build_server(&tree, &mb);

    let err = call(
        &server,
        "set_trigger_volume",
        json!({"nodeID": 60, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}, "material": NODRAW}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "MeshNotFound", "{err:?}");
    assert_eq!(err.code, Some(-32022));
    assert!(
        err.retry_safe,
        "the target is refused before the undo group"
    );

    // a -32603 the bridge answers only after the op ran: outcomeUnknown (maybe-executed)
    let mb = hammer_mock_with(brush_world(60, "trigger_once", &[41]), |builder| {
        builder.error(
            "mesh.setGeometry",
            -32603,
            "InternalError",
            Some(json!({"reason": "failed after the mesh was cleared"})),
        )
    })
    .await;
    let server = build_server(&tree, &mb);
    let err = call(
        &server,
        "set_trigger_volume",
        json!({"nodeID": 60, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}, "material": NODRAW}),
    )
    .await
    .unwrap_err();
    assert!(err.outcome_unknown, "{err:?}");
    assert!(!err.retry_safe);
    assert!(err.data["unsavedBridgeEdits"].is_u64(), "{err:?}");
}

#[tokio::test]
async fn undo_last_restores_the_previous_volume() {
    let tree = Tree::new("undo");
    let mb = hammer_mock(brush_world(60, "trigger_once", &[41]), Some(false)).await;
    seed_box(&mb, 41);
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "set_trigger_volume",
        json!({
            "nodeID": 60,
            "box": {"min": [0, 0, 0], "max": [32, 32, 32]},
            "material": NODRAW,
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["undoLabel"], json!("MCP: set mesh geometry"));
    assert_eq!(
        mock_mesh(&mb, 41).expect("geometry").vertices.len(),
        8,
        "the new volume is in the document"
    );

    let undone = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(undone["undone"], json!(true), "{undone}");
    assert_eq!(
        undone["undoName"],
        json!("MCP: set mesh geometry"),
        "{undone}"
    );
    let restored = mock_mesh(&mb, 41).expect("restored geometry");
    let seed = hammer_mcp::geometry::r#box([-8.0; 3], [8.0; 3]).expect("seed box");
    assert_eq!(restored.vertices, seed.mesh.vertices, "the seed is back");
    assert_eq!(
        restored.materials[0], TOOLSTRIGGER,
        "the seed material is back"
    );
}
