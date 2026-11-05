//! `create_trigger` / `tie_to_entity` / `move_to_world` tests (WP-T, bridge m2.19
//! `entity.tieMeshes`/`entity.untie`): the trigger tools over the mock bridge's Hammer world plus
//! this suite's own FGD fixture — `core/base.fgd`'s `TriggerOnce` -> `Trigger` chain (the
//! `metadata` `auto_apply_material`, the real spawnflags, `trigger_once`/`trigger_multiple`),
//! `func_buyzone` as a Solid class that is not a trigger, `info_target` as a Point class and
//! `Targetname` as a Base class.
//!
//! Covered: the **exact request JSON** of the three steps, the FGD defaults (spawnflags `4097`
//! sent explicitly, the material from `auto_apply_material`), the volume matrix (box / geometry /
//! meshNodeIDs), the retexture path, `tie_to_entity` and `move_to_world` end to end, `undo_last`
//! reverting the groups, and the negative list of target/WP.md: unknown/Point/Base class, the
//! volume and keyvalue arguments, unknown spawnflags, an output the class lacks (nothing sent,
//! not even the mesh), the bridge's `-32022` / `already tied` / `not a brush entity` refusals, the
//! partial failure after the volume was created, a maybe-executed
//! `-32603`, `deleteEntity:false`, the missing FGD and the old-bridge gate.
//!
//! WP-T2 (the FINAL wire contract, plan §5): `tie_to_entity`'s `entityID` form (with the class-form
//! arguments refused, `alreadyTied`, a point entity `NotABrushEntity`), `allowRetie` (the pinned
//! refusal without it, `removedEmptyEntities` and one undo restoring both with it), the
//! non-trigger solid class getting the `create_mesh` default instead of `toolstrigger`, and the
//! per-call capability gating (a reduced bridge serves the calls that need none of the optional
//! methods and refuses the others before the first send).
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockBuilder, MockMesh, MockNode, HAMMER_METHODS};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod common;

const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";
/// The material every trigger class in this fixture declares as its `auto_apply_material`.
const TOOLSTRIGGER: &str = "materials/tools/toolstrigger.vmat";
/// An explicit material the tests pass to override it.
const NODRAW: &str = "materials/tools/toolsnodraw.vmat";

// -------------------------------------------------------------------------------------------
// The fixture tree: `common::FgdTree`'s copy of `fixtures/fgd` plus this suite's trigger classes
// appended to `csgo/csgo.fgd`. `tests/common/mod.rs`'s `PROPS_FGD` (which declares
// trigger_multiple as a PointClass) is deliberately not used: this suite needs `core/base.fgd`'s
// real trigger chain, and it is the only suite that does.
// -------------------------------------------------------------------------------------------

/// The trigger classes, mirroring `core/base.fgd` (shapes, defaults and the `metadata` block).
const TRIGGER_FGD: &str = "

// ---- WP-T test fixture: the core/base.fgd trigger chain -----------------------------------
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
";

/// A temp project tree with the FGD fixture + [`TRIGGER_FGD`]; removed on drop, never under `/tmp`.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "trigger-fgd-{tag}-{}-{}",
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
        Tree { root: dir }
    }

    /// A tree with no `csgo/csgo.fgd`: the `FgdUnavailable` path.
    fn without_fgd(tag: &str) -> Tree {
        let tree = Tree::new(tag);
        std::fs::remove_file(tree.game().join("csgo/csgo.fgd")).expect("remove the entry FGD");
        tree
    }

    fn game(&self) -> PathBuf {
        self.root.join("workshop/run/app/game")
    }

    fn app(&self) -> PathBuf {
        self.root.join("workshop/run/app")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

static TRAIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// -------------------------------------------------------------------------------------------
// The server + the mock bridge
// -------------------------------------------------------------------------------------------

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

fn config(tree: &Tree, discovery: &Path) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let app = tree.app().display().to_string();
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

fn build_server(tree: &Tree, mb: &MockBridge) -> McpServer<HammerState> {
    hammer_mcp::build_server(&config(tree, &mb.discovery_path)).unwrap()
}

fn kv(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A world mesh node (untied).
fn mesh(id: i64) -> MockNode {
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

/// The world the trigger tests start from: nodeID 60 `func_buyzone` named `ta_kill` (the
/// connection target — a Solid class, not a trigger, with `Enable`/`Disable` inputs and no
/// spawnflags of its own) and the two untied world meshes 41 and 42.
fn nodes() -> Vec<MockNode> {
    vec![
        MockNode::world(),
        MockNode::entity(
            60,
            "func_buyzone",
            [0.0, 0.0, 0.0],
            kv(&[("targetname", "ta_kill")]),
        ),
        mesh(41),
        mesh(42),
    ]
}

async fn hammer_mock(world: Vec<MockNode>) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(world)
        .start()
        .await
        .unwrap()
}

async fn hammer_mock_with(
    world: Vec<MockNode>,
    tweak: impl FnOnce(MockBuilder) -> MockBuilder,
) -> MockBridge {
    tweak(
        MockBridge::builder()
            .doc(Some(make_doc(DOC_WIN, Some(true))))
            .hammer_nodes(world),
    )
    .start()
    .await
    .unwrap()
}

/// One recorded request's params, with the per-call budget fields removed: `deadlineMs` is
/// derived from this call's budget (asserted to be inside it), `expectDoc` is the fingerprint the
/// tool read and **is** pinned. Everything else is the tool's own request.
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

/// The exact `mesh.create` params a `box` volume must send (the geometry comes from the same
/// public builder the tool uses, so the pin is exact without a hand-copied face list).
fn expected_box(min: [f64; 3], max: [f64; 3], material: &str) -> Value {
    let primitive = hammer_mcp::geometry::r#box(min, max).expect("box");
    json!({
        "vertices": primitive.mesh.vertices,
        "faces": primitive.mesh.faces,
        "material": material,
    })
}

/// Give a seeded mesh node real geometry (the mock's `mesh.setMaterial` needs faces to paint; a
/// node the world holds no geometry for is the bridge's `-32603 "mesh has no faces"`). The box is
/// centred on the origin, so a seeded node's own origin stays `[0, 0, 0]`.
fn seed_box(mb: &MockBridge, id: i64) {
    let primitive = hammer_mcp::geometry::r#box([-8.0; 3], [8.0; 3]).expect("box");
    let faces = primitive.mesh.faces.len();
    mb.with_state(|state| {
        let world = state.hammer.as_mut().expect("the hammer world");
        if let Some(node) = world.nodes.iter_mut().find(|n| n.node_id == id) {
            node.origin = [0.0; 3];
        }
        world.meshes.insert(
            id,
            MockMesh {
                vertices: primitive.mesh.vertices.clone(),
                faces: primitive.mesh.faces.clone(),
                materials: vec!["materials/tools/toolstrigger.vmat".to_string(); faces],
            },
        );
    });
}

/// The mock world's nodes as `(nodeID, classname, parent)` rows.
fn tree_rows(mb: &MockBridge) -> Vec<(i64, String, Option<i64>)> {
    mb.hammer_nodes()
        .into_iter()
        .map(|n| (n.node_id, n.classname, n.parent))
        .collect()
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

/// One `{output, target, input}` connection object.
fn conn(output: &str, target: &str, input: &str) -> Value {
    json!({"output": output, "target": target, "input": input})
}

// -------------------------------------------------------------------------------------------
// create_trigger: the positive flow, with the exact requests pinned
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn create_trigger_pins_the_three_requests_and_the_fgd_defaults() {
    let tree = Tree::new("create");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_once",
            "targetname": "ta_once",
            "box": {"min": [0, 0, 0], "max": [64, 64, 96]},
            "connections": [conn("OnStartTouch", "ta_kill", "Enable")],
        }),
    )
    .await
    .unwrap();

    // --- step (a): the volume, with the FGD's own material
    assert_eq!(
        wire(&mb, "mesh.create", 0),
        {
            let mut params = expected_box([0.0, 0.0, 0.0], [64.0, 64.0, 96.0], TOOLSTRIGGER);
            params["expectDoc"] = json!({"id": "d1", "epoch": 1});
            params
        },
        "the box volume is sent as mesh.create with the FGD auto_apply_material"
    );
    let mesh_id = out["meshes"][0]["nodeID"].as_i64().expect("created mesh");

    // --- step (b): the tie, with the FGD default spawnflags as text
    assert_eq!(
        wire(&mb, "entity.tieMeshes", 0),
        json!({
            "class": "trigger_once",
            "nodeIDs": [mesh_id],
            "keyValues": {"spawnflags": "4097", "targetname": "ta_once"},
            "expectDoc": {"id": "d1", "epoch": 2},
        }),
        "the tie carries spawnflags/targetname as text"
    );
    let entity_id = out["nodeID"].as_i64().expect("created entity");

    // --- step (c): the connection, on the new node
    assert_eq!(
        wire(&mb, "entity.addConnections", 0),
        json!({
            "nodeID": entity_id,
            "connections": [conn("OnStartTouch", "ta_kill", "Enable")],
            "expectDoc": {"id": "d1", "epoch": 3},
        })
    );

    // --- the result
    assert_eq!(out["classname"], json!("trigger_once"));
    assert_eq!(out["targetname"], json!("ta_once"));
    assert_eq!(
        out["origin"],
        json!([32.0, 32.0, 48.0]),
        "the volume's centre: {out}"
    );
    assert_eq!(out["meshes"], json!([{"nodeID": mesh_id, "created": true}]));
    assert_eq!(out["material"], json!(TOOLSTRIGGER));
    assert_eq!(out["materialSource"], json!("fgd-metadata"));
    assert_eq!(out["spawnflags"]["value"], json!(4097));
    assert_eq!(out["spawnflags"]["raw"], json!("4097"));
    assert_eq!(
        out["spawnflags"]["flags"],
        json!([
            {"bit": 1, "label": "Clients", "on": true},
            {"bit": 2, "label": "NPCs", "on": false},
            {"bit": 4096, "label": "Correctly account for object mass", "on": true}
        ])
    );
    assert_eq!(out["spawnflags"]["unknownBits"], json!(0));
    assert_eq!(out["connections"]["count"], json!(1));
    assert_eq!(
        out["connections"]["added"][0]["outputName"],
        json!("OnStartTouch")
    );
    assert_eq!(
        out["connections"]["added"][0]["targetName"],
        json!("ta_kill")
    );
    assert_eq!(out["connections"]["added"][0]["inputName"], json!("Enable"));
    assert_eq!(out["connections"]["unconfirmed"], json!([]));
    // A4: the tie reply's keyValues echo is reported, and every key it wrote is `applied`
    assert_eq!(
        out["applied"],
        json!({"spawnflags": true, "targetname": true}),
        "{out}"
    );
    assert_eq!(out["keyValuesEcho"], json!(true));
    assert_eq!(out["undoGroups"], json!(3));
    assert_eq!(
        out["undoLabels"],
        json!([
            "MCP: create mesh",
            "MCP: tie meshes to entity",
            "MCP: add connections"
        ])
    );
    assert_eq!(out["unsavedBridgeEdits"], json!(3));
    assert_eq!(out["saved"], json!(false));
    assert_eq!(out["fgd"]["files"], json!(2));
    assert!(
        out["warnings"].as_array().expect("warnings").is_empty(),
        "{out}"
    );
    let said = notes(&out);
    assert!(said.contains("undo_last"), "{said}");
    assert!(said.contains("3 time(s)"), "{said}");
    assert!(
        said.contains("the FGD defaults of 'trigger_once' are sent explicitly as \"4097\""),
        "{said}"
    );
    assert!(said.contains("auto_apply_material"), "{said}");

    // --- the document really holds what the result says
    let rows = tree_rows(&mb);
    assert!(
        rows.contains(&(entity_id, "trigger_once".to_string(), None)),
        "{rows:?}"
    );
    assert!(
        rows.contains(&(mesh_id, String::new(), Some(entity_id))),
        "{rows:?}"
    );
    let read = call(&server, "get_entity_live", json!({"nodeID": entity_id}))
        .await
        .unwrap();
    assert_eq!(read["keyValues"]["spawnflags"], json!("4097"));
    assert_eq!(read["targetname"], json!("ta_once"));
    assert_eq!(
        read["children"],
        json!([{"nodeID": mesh_id, "class": "mesh"}])
    );
    assert_eq!(read["connections"][0]["outputName"], json!("OnStartTouch"));
}

#[tokio::test]
async fn an_explicit_material_overrides_the_fgd_metadata() {
    let tree = Tree::new("material-arg");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_multiple",
            "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
            "material": NODRAW,
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["material"], json!(NODRAW));
    assert_eq!(out["materialSource"], json!("argument"));
    assert_eq!(
        wire(&mb, "mesh.create", 0)["material"],
        json!(NODRAW),
        "the argument wins over auto_apply_material"
    );
    // trigger_multiple inherits TriggerOnce's flags: the same 4097 default
    assert_eq!(out["spawnflags"]["value"], json!(4097));
    assert_eq!(
        wire(&mb, "entity.tieMeshes", 0)["keyValues"],
        json!({"spawnflags": "4097"}),
        "no targetname given: only the flags go out"
    );
    assert_eq!(out["undoGroups"], json!(2), "no connections: two groups");
}

#[tokio::test]
async fn an_explicit_spawnflags_list_is_sent_verbatim() {
    let tree = Tree::new("spawnflags");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    // labels (case-insensitive) or bit numbers; the FGD defaults are NOT added on top, and every
    // default-on bit the list leaves OFF is named in `warnings` (A2)
    let out = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_once",
            "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
            "spawnflags": ["clients", 4096],
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["spawnflags"]["value"], json!(4097));
    assert_eq!(
        wire(&mb, "entity.tieMeshes", 0)["keyValues"],
        json!({"spawnflags": "4097"})
    );
    assert!(
        !notes(&out).contains("omitted"),
        "an explicit list is not the default path"
    );
    assert!(
        out["warnings"].as_array().expect("warnings").is_empty(),
        "4097 has both default-on flags on: {out}"
    );

    // only one flag: exactly that bit, and the two default-on bits it leaves off are named
    let out = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_once",
            "meshNodeIDs": [41],
            "spawnflags": ["NPCs"],
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["spawnflags"]["value"], json!(2));
    assert_eq!(out["spawnflags"]["flags"][0]["on"], json!(false));
    assert_eq!(out["spawnflags"]["flags"][1]["on"], json!(true));
    assert_eq!(
        wire(&mb, "entity.tieMeshes", 1)["keyValues"],
        json!({"spawnflags": "2"}),
        "the complete set is sent verbatim, defaults are never OR'd in"
    );
    let warnings = out["warnings"].as_array().expect("warnings");
    for (bit, label) in [(1, "Clients"), (4096, "Correctly account for object mass")] {
        assert!(
            warnings.iter().any(|warning| {
                let text = warning.as_str().unwrap_or_default();
                text.contains(label) && text.contains(&bit.to_string()) && text.contains("OFF")
            }),
            "bit {bit} ({label}) is reported OFF: {warnings:?}"
        );
    }
}

#[tokio::test]
async fn create_trigger_geometry_volume_is_sent_rounded() {
    let tree = Tree::new("geometry");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    // the same box, as explicit geometry with an extra decimal the bridge rounds away
    let out = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_once",
            "geometry": {
                "vertices": [[0, 0, 0], [8.00004, 0, 0], [0, 8, 0], [8, 8, 0],
                             [0, 0, 8], [8, 0, 8], [0, 8, 8], [8, 8, 8]],
                "faces": [[0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4],
                          [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5]]
            },
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        wire(&mb, "mesh.create", 0)["vertices"][1],
        json!([8.0, 0.0, 0.0]),
        "the coordinates go out rounded to 4 decimals"
    );
    assert_eq!(out["meshes"].as_array().expect("meshes").len(), 1);
    assert_eq!(out["spawnflags"]["value"], json!(4097));
}

#[tokio::test]
async fn existing_meshes_are_tied_without_creating_a_volume() {
    let tree = Tree::new("mesh-ids");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "create_trigger",
        json!({"class": "trigger_multiple", "meshNodeIDs": [42, 41]}),
    )
    .await
    .unwrap();
    assert!(
        mb.method_calls("mesh.create").is_empty(),
        "no volume is built"
    );
    assert_eq!(
        wire(&mb, "entity.tieMeshes", 0)["nodeIDs"],
        json!([42, 41]),
        "the caller's order is kept"
    );
    assert_eq!(
        out["meshes"],
        json!([{"nodeID": 42, "created": false}, {"nodeID": 41, "created": false}])
    );
    assert_eq!(out["undoGroups"], json!(1), "one tie group");
    assert_eq!(
        out["materialSource"],
        json!("fgd-metadata"),
        "unused, still reported"
    );

    // retexture:true adds one mesh.setMaterial group per mesh (the seeded meshes need geometry
    // for the mock's all-faces setMaterial)
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    seed_box(&mb, 41);
    seed_box(&mb, 42);
    let out = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_multiple",
            "meshNodeIDs": [41, 42],
            "material": NODRAW,
            "retexture": true,
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["undoGroups"], json!(3));
    assert_eq!(
        out["undoLabels"],
        json!([
            "MCP: tie meshes to entity",
            "MCP: set mesh material",
            "MCP: set mesh material"
        ])
    );
    assert_eq!(
        wire(&mb, "mesh.setMaterial", 0),
        json!({"nodeID": 41, "material": NODRAW, "expectDoc": {"id": "d1", "epoch": 2}})
    );
    assert_eq!(wire(&mb, "mesh.setMaterial", 1)["nodeID"], json!(42));
    assert!(notes(&out).contains("retextured"), "{}", notes(&out));
}

#[tokio::test]
async fn the_created_volume_can_be_re_materialed_per_face_from_the_live_read() {
    let tree = Tree::new("live-volume");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "create_trigger",
        json!({"class": "trigger_once", "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap();
    let mesh_id = out["meshes"][0]["nodeID"].as_i64().expect("mesh");

    // the per-face material route rebuilds from the geometry Hammer holds (`mesh.get`)
    let rebuilt = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": mesh_id, "material": NODRAW, "faces": [0]}),
    )
    .await
    .unwrap();
    assert_eq!(rebuilt["nodeID"], json!(mesh_id));
    assert_eq!(rebuilt["geometrySource"], json!("live"), "{rebuilt}");

    // ... and with a connection in the same call too: nothing is remembered, so the counted
    // connections write does not matter to the later read
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    let out = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_once",
            "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
            "connections": [conn("OnStartTouch", "ta_kill", "Enable")],
        }),
    )
    .await
    .unwrap();
    let mesh_id = out["meshes"][0]["nodeID"].as_i64().expect("mesh");
    let rebuilt = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": mesh_id, "material": NODRAW, "faces": [0]}),
    )
    .await
    .unwrap();
    assert_eq!(rebuilt["geometrySource"], json!("live"), "{rebuilt}");
}

#[tokio::test]
async fn undo_last_reverts_the_three_groups() {
    let tree = Tree::new("undo");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_once",
            "targetname": "ta_once",
            "box": {"min": [0, 0, 0], "max": [64, 64, 96]},
            "connections": [conn("OnStartTouch", "ta_kill", "Enable")],
        }),
    )
    .await
    .unwrap();
    let mesh_id = out["meshes"][0]["nodeID"].as_i64().expect("mesh");
    let entity_id = out["nodeID"].as_i64().expect("entity");
    assert_eq!(out["undoGroups"], json!(3));

    // the notes' recipe: `undo_last` once per group the call opened
    for label in [
        "MCP: add connections",
        "MCP: tie meshes to entity",
        "MCP: create mesh",
    ] {
        let undone = call(&server, "undo_last", json!({})).await.unwrap();
        assert_eq!(undone["undone"], json!(true), "{undone}");
        assert_eq!(undone["undoName"], json!(label), "{undone}");
    }
    assert_eq!(
        tree_rows(&mb),
        vec![
            (1, "worldspawn".to_string(), None),
            (60, "func_buyzone".to_string(), None),
            (41, String::new(), None),
            (42, String::new(), None),
        ],
        "the document is back to the world the call started from"
    );
    let _ = mesh_id;
    assert_eq!(
        mb.hammer_connections(entity_id),
        Vec::new(),
        "the connections went with the entity"
    );
}

// -------------------------------------------------------------------------------------------
// tie_to_entity / move_to_world
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn tie_to_entity_creates_the_brush_entity() {
    let tree = Tree::new("tie");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "tie_to_entity",
        json!({
            "class": "trigger_once",
            "meshNodeIDs": [41, 42],
            "targetname": "ta_tie",
            "spawnflags": ["Clients"],
            "keyValues": {"wait": -1},
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        wire(&mb, "entity.tieMeshes", 0),
        json!({
            "class": "trigger_once",
            "nodeIDs": [41, 42],
            "keyValues": {"spawnflags": "1", "targetname": "ta_tie", "wait": "-1"},
            "expectDoc": {"id": "d1", "epoch": 1},
        })
    );
    let entity_id = out["nodeID"].as_i64().expect("entity");
    assert_eq!(
        out["origin"],
        json!([0.0, 0.0, 0.0]),
        "seeded meshes sit at the origin"
    );
    assert_eq!(
        out["meshes"],
        json!([{"nodeID": 41, "created": false},
                                     {"nodeID": 42, "created": false}])
    );
    assert_eq!(
        out["material"],
        json!(null),
        "no material given: no retexture"
    );
    assert_eq!(out["undoGroups"], json!(1));
    assert!(mb.method_calls("mesh.setMaterial").is_empty());
    assert_eq!(
        tree_rows(&mb),
        vec![
            (1, "worldspawn".to_string(), None),
            (60, "func_buyzone".to_string(), None),
            (41, String::new(), Some(entity_id)),
            (42, String::new(), Some(entity_id)),
            (entity_id, "trigger_once".to_string(), None),
        ]
    );

    // the material turns it into a retexture of every tied mesh
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    seed_box(&mb, 41);
    let out = call(
        &server,
        "tie_to_entity",
        json!({"class": "trigger_multiple", "meshNodeIDs": [41], "material": NODRAW}),
    )
    .await
    .unwrap();
    assert_eq!(out["undoGroups"], json!(2));
    assert_eq!(
        out["undoLabels"],
        json!(["MCP: tie meshes to entity", "MCP: set mesh material"])
    );
    assert_eq!(
        wire(&mb, "mesh.setMaterial", 0),
        json!({"nodeID": 41, "material": NODRAW, "expectDoc": {"id": "d1", "epoch": 2}})
    );
}

#[tokio::test]
async fn move_to_world_returns_the_meshes_and_deletes_the_entity() {
    let tree = Tree::new("untie");
    let mut tied = mesh(41);
    tied.parent = Some(60);
    let mb = hammer_mock(vec![MockNode::world(), nodes()[1].clone(), tied]).await;
    let server = build_server(&tree, &mb);

    let out = call(&server, "move_to_world", json!({"nodeID": 60}))
        .await
        .unwrap();
    assert_eq!(
        wire(&mb, "entity.untie", 0),
        json!({"nodeID": 60, "expectDoc": {"id": "d1", "epoch": 1}})
    );
    assert_eq!(out["nodeID"], json!(60));
    assert_eq!(out["meshes"], json!([41]));
    assert_eq!(out["entityDeleted"], json!(true));
    assert_eq!(out["undoLabel"], json!("Untie from entity"));
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
    assert_eq!(
        tree_rows(&mb),
        vec![
            (1, "worldspawn".to_string(), None),
            (41, String::new(), None),
        ],
        "the entity is gone and the mesh is a world node again"
    );

    // Hammer's own "Untie from entity" group is not one this server labelled `MCP: ...`: the
    // plain `undo_last` refuses it ...
    let err = call(&server, "undo_last", json!({})).await.unwrap_err();
    assert_eq!(err.name, "UndoGroupNotOurs", "{err:?}");
    // ... and `acceptUserEdits:true` pops it: one step restores the tie
    let undone = call(&server, "undo_last", json!({"acceptUserEdits": true}))
        .await
        .unwrap();
    assert_eq!(undone["undoName"], json!("Untie from entity"));
    assert!(tree_rows(&mb).contains(&(41, String::new(), Some(60))));
    assert_eq!(tree_rows(&mb).len(), 3);

    // a point entity (no mesh children) is refused by the bridge, in the tool's vocabulary
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    let err = call(&server, "move_to_world", json!({"nodeID": 60}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "NotABrushEntity", "{err:?}");
    assert_eq!(err.type_, "NotABrushEntity");
    assert_eq!(err.code, Some(-32602));
    assert!(err.retry_safe);
    assert_eq!(err.data["nodeID"], json!(60));
    assert!(
        mb.method_calls("entity.untie").len() == 1,
        "the refusal was the bridge's own"
    );

    // the world is not an entity, a missing id is NodeNotFound
    let err = call(&server, "move_to_world", json!({"nodeID": 1}))
        .await
        .unwrap_err();
    assert_eq!(err.code, Some(-32602), "{err:?}");
    let err = call(&server, "move_to_world", json!({"nodeID": 999}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "NodeNotFound", "{err:?}");
    assert_eq!(err.code, Some(-32022));
}

#[tokio::test]
async fn move_to_world_delete_entity_false_is_refused() {
    let tree = Tree::new("delete-false");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    let err = call(
        &server,
        "move_to_world",
        json!({"nodeID": 60, "deleteEntity": false}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(
        err.message.contains("deleteEntity:false"),
        "{}",
        err.message
    );
    assert!(mb.requests().is_empty(), "nothing was sent");
}

// -------------------------------------------------------------------------------------------
// Negatives: the class, the volume, the arguments
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn class_gates_send_nothing() {
    let tree = Tree::new("class-gates");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    // an unknown class: UnknownClass with the database's own suggestions
    let err = call(
        &server,
        "create_trigger",
        json!({"class": "triger_once", "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnknownClass", "{err:?}");
    assert_eq!(err.code, Some(-32006));
    assert_eq!(err.data["class"], json!("triger_once"));
    assert!(err.retry_safe);

    // a Point class and a Base class: NotASolidClass with the kind
    for (class, kind) in [("info_target", "point"), ("Targetname", "base")] {
        for tool in ["create_trigger", "tie_to_entity"] {
            let args = if tool == "create_trigger" {
                json!({"class": class, "box": {"min": [0, 0, 0], "max": [8, 8, 8]}})
            } else {
                json!({"class": class, "meshNodeIDs": [41]})
            };
            let err = call(&server, tool, args).await.unwrap_err();
            assert_eq!(err.name, "NotASolidClass", "{tool}/{class}: {err:?}");
            assert_eq!(err.data["kind"], json!(kind), "{tool}/{class}");
            assert_eq!(err.code, Some(-32602));
            // A7: the message spells the FGD keyword the way the FGD does
            let fgd_kind = match kind {
                "point" => "@PointClass",
                "base" => "@BaseClass",
                other => panic!("unexpected kind {other}"),
            };
            assert!(
                err.message.contains(fgd_kind),
                "{tool}/{class}: {fgd_kind} in {}",
                err.message
            );
        }
    }
    // a Solid class that is not a trigger is allowed (func_buyzone has no spawnflags, so the
    // omitted-argument path sends none at all)
    let out = call(
        &server,
        "create_trigger",
        json!({"class": "func_buyzone", "meshNodeIDs": [41]}),
    )
    .await
    .unwrap();
    assert_eq!(out["spawnflags"]["value"], json!(null));
    assert_eq!(wire(&mb, "entity.tieMeshes", 0)["keyValues"], Value::Null);
    assert!(mb.method_calls("mesh.create").is_empty());

    assert_eq!(
        mb.method_calls("mesh.create").len(),
        0,
        "every class refusal sent nothing"
    );
}

#[tokio::test]
async fn volume_and_argument_gates_send_nothing() {
    let tree = Tree::new("arguments");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    let cases: Vec<(&str, Value)> = vec![
        // the volume matrix
        (
            "two volumes",
            json!({"box": {"min": [0, 0, 0], "max": [8, 8, 8]}, "meshNodeIDs": [41]}),
        ),
        ("no volume", json!({"class": "trigger_once"})),
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
        (
            "geometry index",
            json!({"geometry": {"vertices": [[0, 0, 0], [8, 0, 0], [0, 8, 0]],
                                               "faces": [[0, 1, 9]]}}),
        ),
        ("meshNodeIDs empty", json!({"meshNodeIDs": []})),
        (
            "meshNodeIDs 65",
            json!({"meshNodeIDs": (0..65).collect::<Vec<i64>>()}),
        ),
        ("meshNodeIDs duplicate", json!({"meshNodeIDs": [41, 41]})),
        ("meshNodeIDs non-int", json!({"meshNodeIDs": ["41"]})),
        // the arguments
        (
            "unknown argument",
            json!({"box": {"min": [0, 0, 0], "max": [8, 8, 8]}, "volume": 1}),
        ),
        ("class type", json!({"class": 7, "meshNodeIDs": [41]})),
        (
            "targetname type",
            json!({"meshNodeIDs": [41], "targetname": 7}),
        ),
        (
            "spawnflags type",
            json!({"meshNodeIDs": [41], "spawnflags": "Clients"}),
        ),
        ("material type", json!({"meshNodeIDs": [41], "material": 7})),
        (
            "material empty",
            json!({"meshNodeIDs": [41], "material": "  "}),
        ),
        (
            "retexture type",
            json!({"meshNodeIDs": [41], "retexture": 1}),
        ),
        ("force type", json!({"meshNodeIDs": [41], "force": 1})),
        (
            "keyValues type",
            json!({"meshNodeIDs": [41], "keyValues": 7}),
        ),
        (
            "keyValues classname",
            json!({"meshNodeIDs": [41], "keyValues": {"classname": "x"}}),
        ),
        (
            "keyValues spawnflags",
            json!({"meshNodeIDs": [41], "keyValues": {"spawnflags": "1"}}),
        ),
        (
            "keyValues targetname",
            json!({"meshNodeIDs": [41], "keyValues": {"targetname": "t"}}),
        ),
        (
            "keyValues origin",
            json!({"meshNodeIDs": [41], "keyValues": {"origin": "0 0 0"}}),
        ),
        (
            "keyValues angles",
            json!({"meshNodeIDs": [41], "keyValues": {"angles": "0 0 0"}}),
        ),
        (
            "keyValues scales",
            json!({"meshNodeIDs": [41], "keyValues": {"scales": "1 1 1"}}),
        ),
        (
            "keyValues value type",
            json!({"meshNodeIDs": [41], "keyValues": {"wait": [1]}}),
        ),
        (
            "connections empty",
            json!({"meshNodeIDs": [41], "connections": []}),
        ),
        (
            "connections shape",
            json!({"meshNodeIDs": [41], "connections": [{"output": "OnStartTouch",
                                                                          "target": "ta_kill"}]}),
        ),
        (
            "connections delay",
            json!({"meshNodeIDs": [41],
                                     "connections": [{"output": "OnStartTouch", "target": "ta_kill",
                                                      "input": "Enable", "delay": 3601}]}),
        ),
        (
            "connections member",
            json!({"meshNodeIDs": [41],
                                      "connections": [{"output": "OnStartTouch", "target": "ta_kill",
                                                       "input": "Enable", "delayTime": 1}]}),
        ),
    ];
    for (label, args) in cases {
        let before = mb.requests().len();
        let err = call(&server, "create_trigger", args.clone())
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
        "every argument refusal sent nothing at all: {:?}",
        mb.requests()
    );
}

/// The volume's own gates moved into `mesh_live::send_geometry` (A1): a geometry whose face
/// Hammer's own rules reject, and one whose request does not fit the bridge's line budget, are
/// both refused by that path — **no `mesh.create` and no tie goes out**, and the oversized one is
/// refused by our own budget check instead of the bridge's `-32600 "line too long"` (which closes
/// the connection and takes every later call down with it).
#[tokio::test]
async fn an_oversized_or_degenerate_volume_is_refused_before_the_mesh_is_sent() {
    let tree = Tree::new("volume-budget");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    // a zero-area face (the old local gate's case): still InvalidParams, and nothing of the call
    // was written
    let err = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_once",
            "geometry": {"vertices": [[0, 0, 0], [8, 0, 0], [4, 0, 0]], "faces": [[0, 1, 2]]},
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(err.message.contains("faces[0]"), "{}", err.message);
    assert!(mb.method_calls("mesh.create").is_empty());

    // ~40k vertices: `geometry::validate` accepts them (cap 65536) but the request serialises to
    // over 512 KiB
    let mut vertices: Vec<[f64; 3]> = Vec::with_capacity(40_000);
    for i in 0..40_000i64 {
        let x = (i % 200) as f64 + 0.5;
        let y = (i / 200) as f64 + 0.5;
        vertices.push([x, y, 0.25]);
    }
    let err = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_once",
            // two real (non-collinear) faces, so the face gate passes and the byte budget is what
            // refuses the request
            "geometry": {"vertices": vertices, "faces": [[0, 1, 200], [0, 200, 201]]},
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(
        err.message.contains("512") || err.message.contains("budget"),
        "the refusal names the request budget: {}",
        err.message
    );
    assert!(
        err.message.contains("this server's") && err.message.contains("budget"),
        "our own budget refused it: {}",
        err.message
    );
    assert!(
        mb.method_calls("mesh.create").is_empty(),
        "the oversized request never left: {:?}",
        mb.requests()
    );
    assert!(mb.method_calls("entity.tieMeshes").is_empty());
}

/// A1's second half: when the bridge reports fewer faces than were sent (Hammer welded or dropped
/// a polygon), the call says so; a later per-face `set_mesh_material` rebuilds from the faces
/// Hammer really holds (the live read), whose indices are the ones the document has.
#[tokio::test]
async fn a_face_count_mismatch_is_reported_and_the_later_rebuild_reads_the_live_faces() {
    let tree = Tree::new("volume-weld");
    let mb = hammer_mock(nodes()).await;
    // Hammer "welds away" one face of the six the box sends
    mb.set_hammer_drop_faces(1);
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "create_trigger",
        json!({"class": "trigger_once", "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap();
    let mesh_id = out["meshes"][0]["nodeID"].as_i64().expect("mesh");
    assert_eq!(
        wire(&mb, "mesh.create", 0)["faces"]
            .as_array()
            .expect("faces")
            .len(),
        6,
        "six faces were sent"
    );
    assert!(
        notes(&out).contains("Hammer created 5 of 6 face(s)"),
        "the mismatch is reported: {}",
        notes(&out)
    );
    assert_eq!(
        out["undoGroups"],
        json!(2),
        "the volume and the tie still ran"
    );

    // the per-face route rebuilds from the 5 faces Hammer holds, not the 6 that were sent
    let rebuilt = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": mesh_id, "material": NODRAW, "faces": [0]}),
    )
    .await
    .unwrap();
    assert_eq!(rebuilt["geometrySource"], json!("live"), "{rebuilt}");
    assert_eq!(wire(&mb, "mesh.setGeometry", 0)["faces"].as_array().map(Vec::len), Some(5));
}

#[tokio::test]
async fn unknown_spawnflags_send_nothing() {
    let tree = Tree::new("spawnflag-neg");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    for bad in [json!(["Dogs"]), json!([8]), json!(["8"])] {
        let err = call(
            &server,
            "create_trigger",
            json!({"class": "trigger_once", "meshNodeIDs": [41], "spawnflags": bad}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.name, "UnknownSpawnflag", "{bad}: {err:?}");
        assert_eq!(err.code, Some(-32602));
        assert_eq!(
            err.data["valid"],
            json!(["Clients", "NPCs", "Correctly account for object mass"])
        );
        assert!(err.retry_safe);
    }
    // a class with no spawnflags keyvalue at all: naming one is a refusal, omitting it sends none
    let err = call(
        &server,
        "tie_to_entity",
        json!({"class": "func_buyzone", "meshNodeIDs": [41], "spawnflags": ["Clients"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnknownSpawnflag", "{err:?}");
    assert_eq!(err.data["class"], json!("func_buyzone"));
    assert!(mb.requests().is_empty(), "nothing was sent");
}

#[tokio::test]
async fn a_connection_output_the_class_lacks_sends_nothing_at_all() {
    let tree = Tree::new("unknown-output");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    let err = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_once",
            "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
            "connections": [conn("OnNope", "ta_kill", "Enable")],
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnknownOutput", "{err:?}");
    assert_eq!(err.code, Some(-32602));
    assert!(err.message.contains("OnStartTouch"), "{}", err.message);
    assert!(err.retry_safe);
    // THE point of the pre-flight: the volume was not built either
    assert!(
        mb.method_calls("mesh.create").is_empty(),
        "not even the mesh"
    );
    assert!(mb.method_calls("entity.tieMeshes").is_empty());
    assert!(mb.method_calls("entity.addConnections").is_empty());

    // an unknown input on a live target too (the same pre-flight)
    let err = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_once",
            "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
            "connections": [conn("OnStartTouch", "ta_kill", "Nope")],
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnknownInput", "{err:?}");
    assert!(mb.method_calls("mesh.create").is_empty());
    // `force:true` skips the FGD checks and the call goes through
    let out = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_once",
            "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
            "connections": [conn("OnStartTouch", "ta_kill", "Nope")],
            "force": true,
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["undoGroups"], json!(3));
}

// -------------------------------------------------------------------------------------------
// Negatives: the bridge's own refusals, the partial failure, the gates
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_bridges_tie_refusals_pass_through() {
    let tree = Tree::new("tie-refusals");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    // an id that is not in the document: -32022, naming the node the bridge named (A3)
    let err = call(
        &server,
        "tie_to_entity",
        json!({"class": "trigger_once", "meshNodeIDs": [999]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NodeNotFound", "{err:?}");
    assert_eq!(err.code, Some(-32022));
    assert_eq!(err.data["meshNodeIDs"], json!([999]));
    assert_eq!(err.data["nodeID"], json!(999));
    assert!(
        err.message.contains("mesh nodeID 999"),
        "the refusal names the mesh: {}",
        err.message
    );
    assert!(err.retry_safe, "nothing of this call exists");
    assert!(
        err.message.contains("the document is unchanged")
            && !err.message.contains("nothing was sent"),
        "the request WAS sent (A7): {}",
        err.message
    );

    // a node that is not a mesh
    let err = call(
        &server,
        "tie_to_entity",
        json!({"class": "trigger_once", "meshNodeIDs": [60]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NotAMesh", "{err:?}");
    assert_eq!(err.code, Some(-32602));
    assert_eq!(err.data["nodeID"], json!(60));
    assert!(err.message.contains("nodeID 60"), "{}", err.message);

    // a mesh that is already tied: MeshAlreadyTied with its parent
    let mut tied = mesh(41);
    tied.parent = Some(60);
    let mb = hammer_mock(vec![MockNode::world(), nodes()[1].clone(), tied]).await;
    let server = build_server(&tree, &mb);
    let err = call(
        &server,
        "tie_to_entity",
        json!({"class": "trigger_once", "meshNodeIDs": [41]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "MeshAlreadyTied", "{err:?}");
    assert_eq!(err.code, Some(-32602));
    assert_eq!(err.data["parentNodeID"], json!(60));
    assert_eq!(err.data["nodeID"], json!(41));
    assert!(err.retry_safe);
    assert!(
        err.message.contains("nodeID 41"),
        "the already-tied mesh is named: {}",
        err.message
    );
    assert_eq!(
        mb.hammer_nodes().len(),
        3,
        "the refused tie changed nothing"
    );

    // create_trigger with a nonexistent mesh id: the same refusal, and no volume was built either
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    let err = call(
        &server,
        "create_trigger",
        json!({"class": "trigger_once", "meshNodeIDs": [41, 999]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NodeNotFound", "{err:?}");
    assert!(err.retry_safe, "{err:?}");
}

#[tokio::test]
async fn a_tie_failure_after_the_volume_is_a_partial_failure() {
    let tree = Tree::new("partial");
    // the tie refuses everything (the bridge's pre-write -32602 shape), so the volume of the call
    // below is created and then the tie fails
    let mb = hammer_mock_with(nodes(), |builder| {
        builder.error(
            "entity.tieMeshes",
            -32602,
            "InvalidParams",
            // the BRIDGE's `tie_err_node` shape (the mock now emits the same): the tool names
            // the node instead of saying the bridge does not say which one (A3/A10)
            Some(json!({"reason": "not a mesh", "nodeID": 7})),
        )
    })
    .await;
    let server = build_server(&tree, &mb);

    let err = call(
        &server,
        "create_trigger",
        json!({"class": "trigger_once", "box": {"min": [0, 0, 0], "max": [64, 64, 96]}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PartialFailure", "{err:?}");
    assert_eq!(err.type_, "PartialFailure");
    assert_eq!(err.code, Some(-32012));
    assert!(!err.retry_safe, "a retry would create a second volume");
    let mesh_id = err.data["meshNodeID"].as_i64().expect("the created mesh");
    assert_eq!(err.data["nodeID"], Value::Null, "no entity was created");
    assert_eq!(err.data["undoGroups"], json!(1));
    assert!(
        err.message.contains(&mesh_id.to_string()),
        "the message names the mesh: {}",
        err.message
    );
    assert!(err.message.contains("undo_last 1"), "{}", err.message);
    assert_eq!(err.data["bridgeError"]["name"], json!("NotAMesh"));
    assert_eq!(err.data["bridgeError"]["code"], json!(-32602));

    // the volume really is in the document ...
    assert!(tree_rows(&mb).contains(&(mesh_id, String::new(), None)));
    // ... and the live read still serves it: a later per-face edit works on what Hammer holds
    let rebuilt = call(
        &server,
        "set_mesh_material",
        json!({"nodeID": mesh_id, "material": NODRAW, "faces": [0]}),
    )
    .await
    .unwrap();
    assert_eq!(rebuilt["geometrySource"], json!("live"), "{rebuilt}");
}

#[tokio::test]
async fn a_maybe_executed_tie_error_is_outcome_unknown() {
    let tree = Tree::new("maybe-executed");
    // -32603 is answered *after* the op ran (the mock applies it too), so the tool must report a
    // maybe-executed half-build: outcomeUnknown, not a clean refusal
    let mb = hammer_mock_with(nodes(), |builder| {
        builder.error(
            "entity.tieMeshes",
            -32603,
            "InternalError",
            Some(json!({"reason": "tie failed after the group opened"})),
        )
    })
    .await;
    let server = build_server(&tree, &mb);

    let err = call(
        &server,
        "create_trigger",
        json!({"class": "trigger_once", "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PartialFailure", "{err:?}");
    assert!(err.outcome_unknown, "a -32603 may have landed: {err:?}");
    assert!(!err.retry_safe);
    assert_eq!(err.data["bridgeError"]["code"], json!(-32603));
    // A5: the mock really applied the tie, so the document's undo depth moved by 2 — the labels
    // alone (1: the volume) undercount exactly when the revert instruction matters most
    assert_eq!(err.data["undoGroups"], json!(2), "{err:?}");
    assert_eq!(err.data["undoGroupsMinimum"], json!(1));
    assert!(
        err.message.contains("moved by 2 group(s)") && err.message.contains("at least 1"),
        "the instruction names the real depth and its minimum: {}",
        err.message
    );
    assert!(err.message.contains("session_status"), "{}", err.message);
    // the counter counted the volume and the maybe-executed tie
    assert_eq!(err.data["unsavedBridgeEdits"], json!(2));
    // the mock really applied the tie (its -32603 runs the op): the entity exists
    assert!(
        tree_rows(&mb)
            .iter()
            .any(|(_, class, _)| class == "trigger_once"),
        "the error's own effect landed: {:?}",
        tree_rows(&mb)
    );
}

#[tokio::test]
async fn the_old_bridge_gate_names_the_m2_19_method() {
    let tree = Tree::new("old-bridge");
    // an m2.18 bridge: everything but the m2.19 pair is advertised (the built-ins still answer,
    // only the advertisement gates what the tools check). The withdrawal has to go through
    // `set_methods`: `MockBuilder` appends every `HAMMER_METHODS` row for an enabled hammer world.
    let mb = hammer_mock(nodes()).await;
    let mut methods: Vec<String> = HAMMER_METHODS.iter().map(|m| m.to_string()).collect();
    methods.retain(|m| m != "entity.tieMeshes" && m != "entity.untie");
    mb.set_methods(methods);
    let server = build_server(&tree, &mb);

    for (tool, args) in [
        (
            "create_trigger",
            json!({"class": "trigger_once", "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
        ),
        (
            "tie_to_entity",
            json!({"class": "trigger_once", "meshNodeIDs": [41]}),
        ),
        ("move_to_world", json!({"nodeID": 60})),
    ] {
        let err = call(&server, tool, args).await.unwrap_err();
        assert_eq!(err.name, "NotImplemented", "{tool}: {err:?}");
        assert_eq!(err.type_, "CapabilityMissing", "{tool}");
        assert_eq!(err.code, Some(-32011), "{tool}");
        assert!(err.retry_safe, "{tool}");
        let missing = err.data["missing"].as_array().expect("missing").clone();
        assert!(
            missing
                .iter()
                .any(|m| m == "entity.tieMeshes" || m == "entity.untie"),
            "{tool}: {missing:?} (the m2.19 methods)"
        );
    }
    // the gate itself connects (the bridge client's hello/liveness), but no tool request goes out
    for method in [
        "mesh.create",
        "entity.tieMeshes",
        "entity.untie",
        "entity.addConnections",
        "doc.active",
    ] {
        assert!(
            mb.method_calls(method).is_empty(),
            "{method} was dispatched: {:?}",
            mb.requests()
        );
    }
}

#[tokio::test]
async fn a_missing_fgd_is_a_refusal_not_a_guess() {
    let tree = Tree::without_fgd("no-fgd");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    for (tool, args) in [
        (
            "create_trigger",
            json!({"box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
        ),
        (
            "tie_to_entity",
            json!({"class": "trigger_once", "meshNodeIDs": [41]}),
        ),
    ] {
        let err = call(&server, tool, args).await.unwrap_err();
        assert_eq!(err.name, "FgdUnavailable", "{tool}: {err:?}");
        assert_eq!(err.code, Some(-32011));
        assert!(err.retry_safe);
    }
    assert!(mb.requests().is_empty());
    // move_to_world needs no FGD: it is a pure bridge call
    let mut tied = mesh(41);
    tied.parent = Some(60);
    let mb = hammer_mock(vec![MockNode::world(), nodes()[1].clone(), tied]).await;
    let server = build_server(&tree, &mb);
    let out = call(&server, "move_to_world", json!({"nodeID": 60}))
        .await
        .unwrap();
    assert_eq!(out["meshes"], json!([41]));
}

// -------------------------------------------------------------------------------------------
// WP-T2: the FINAL wire contract (plan §5) — entityID / allowRetie / the material rule / per-call
// capability gating
// -------------------------------------------------------------------------------------------

/// `tie_to_entity`'s `entityID` form: tie into an existing brush entity (the class-form arguments
/// are refused, no FGD needed), a mesh already under it is skipped (`alreadyTied`), and a point
/// entity is `NotABrushEntity` — all before anything is sent.
#[tokio::test]
async fn tie_to_entity_uses_an_existing_brush_entity() {
    let tree = Tree::new("tie-existing");
    // nodeID 60 is the `func_buyzone` brush entity; 41 is already under it, 42 is free
    let mut tied = mesh(41);
    tied.parent = Some(60);
    let mb = hammer_mock(vec![MockNode::world(), nodes()[1].clone(), mesh(42), tied]).await;
    let server = build_server(&tree, &mb);
    let before = mb.requests().len();

    // --- entityID with class-form arguments: every one is refused with nothing sent
    for (key, extra) in [
        ("class", json!({"class": "trigger_once"})),
        ("targetname", json!({"targetname": "t"})),
        ("spawnflags", json!({"spawnflags": ["Clients"]})),
        ("keyValues", json!({"keyValues": {"wait": "1"}})),
    ] {
        let mut args = json!({"entityID": 60, "meshNodeIDs": [42]});
        let map = args.as_object_mut().expect("object");
        for (k, v) in extra.as_object().expect("object") {
            map.insert(k.clone(), v.clone());
        }
        let err = call(&server, "tie_to_entity", args).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{key}: {err:?}");
        assert!(err.message.contains(key), "{key}: {}", err.message);
    }
    // class + entityID / neither
    for args in [
        json!({"class": "trigger_once", "entityID": 60, "meshNodeIDs": [42]}),
        json!({"meshNodeIDs": [42]}),
    ] {
        let err = call(&server, "tie_to_entity", args).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{err:?}");
        assert!(err.message.contains("class"), "{err:?}");
    }
    assert_eq!(
        mb.requests().len(),
        before,
        "every argument refusal sent nothing"
    );

    // entityID of a point entity: the bridge's own "not a brush entity", in the tool's vocabulary
    let mb = hammer_mock(vec![
        MockNode::world(),
        MockNode::entity(61, "info_target", [0.0; 3], kv(&[("targetname", "p")])),
        mesh(41),
    ])
    .await;
    let server = build_server(&tree, &mb);
    let err = call(
        &server,
        "tie_to_entity",
        json!({"entityID": 61, "meshNodeIDs": [41]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NotABrushEntity", "{err:?}");
    assert_eq!(err.code, Some(-32602));
    assert!(err.retry_safe);
    let err = call(
        &server,
        "tie_to_entity",
        json!({"entityID": 999, "meshNodeIDs": [41]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NodeNotFound", "{err:?}");
    assert_eq!(err.code, Some(-32022));
    // A3: a missing entityID carries no `data.nodeID`, so the tool names the ENTITY (not the
    // meshes that were sent)
    assert_eq!(err.data["entityID"], json!(999));
    assert!(
        err.message
            .contains("entityID 999 is not in the live document"),
        "{}",
        err.message
    );
    assert!(
        !err.message.contains("one of the meshes"),
        "the meshes are not what is missing: {}",
        err.message
    );

    // --- the positive form: 41 is already there (skipped), 42 is tied, one group, no FGD needed
    let tree_no_fgd = Tree::without_fgd("tie-existing-nofgd");
    let mut tied = mesh(41);
    tied.parent = Some(60);
    let mb = hammer_mock(vec![MockNode::world(), nodes()[1].clone(), mesh(42), tied]).await;
    let server = build_server(&tree_no_fgd, &mb);
    let out = call(
        &server,
        "tie_to_entity",
        json!({"entityID": 60, "meshNodeIDs": [41, 42]}),
    )
    .await
    .unwrap();
    assert_eq!(
        wire(&mb, "entity.tieMeshes", 0),
        json!({
            "entityID": 60,
            "nodeIDs": [41, 42],
            "expectDoc": {"id": "d1", "epoch": 1},
        }),
        "the entityID form sends no class and no keyValues"
    );
    assert_eq!(out["nodeID"], json!(60));
    assert_eq!(out["classname"], json!("func_buyzone"));
    assert_eq!(out["entityCreated"], json!(false));
    assert_eq!(out["alreadyTied"], json!([41]));
    assert_eq!(out["removedEmptyEntities"], json!([]));
    assert_eq!(out["targetname"], Value::Null);
    assert_eq!(out["spawnflags"]["value"], Value::Null);
    assert_eq!(out["undoGroups"], json!(1));
    assert!(mb.method_calls("mesh.setMaterial").is_empty());
    let rows = tree_rows(&mb);
    assert!(rows.contains(&(42, String::new(), Some(60))), "{rows:?}");
    assert!(rows.contains(&(41, String::new(), Some(60))), "{rows:?}");
}

/// A mesh under ANOTHER brush entity needs `allowRetie`: without it the request is pinned and the
/// bridge refuses (`MeshAlreadyTied`, nothing else sent); with it the emptied old entity is deleted
/// in the tie's own group (`removedEmptyEntities`) and one `undo_last` restores BOTH.
#[tokio::test]
async fn retie_needs_allow_retie_and_the_undo_restores_both_entities() {
    let tree = Tree::new("retie");
    // 70 trigger_multiple owns mesh 41; 42 is a free world mesh
    let mut tied = mesh(41);
    tied.parent = Some(70);
    let world = || {
        vec![
            MockNode::world(),
            MockNode::entity(70, "trigger_multiple", [0.0; 3], BTreeMap::new()),
            mesh(42),
            tied.clone(),
        ]
    };

    // --- without allowRetie: refused, and the request is the plain one
    let mb = hammer_mock(world()).await;
    let server = build_server(&tree, &mb);
    let err = call(
        &server,
        "tie_to_entity",
        json!({"class": "trigger_once", "meshNodeIDs": [41, 42]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "MeshAlreadyTied", "{err:?}");
    assert_eq!(err.code, Some(-32602));
    assert_eq!(err.data["nodeID"], json!(41));
    assert_eq!(err.data["parentNodeID"], json!(70));
    assert_eq!(
        wire(&mb, "entity.tieMeshes", 0),
        json!({
            "class": "trigger_once",
            "nodeIDs": [41, 42],
            "keyValues": {"spawnflags": "4097"},
            "expectDoc": {"id": "d1", "epoch": 1},
        }),
        "no allowRetie key on the request"
    );
    assert!(
        mb.method_calls("mesh.setMaterial").is_empty(),
        "nothing else was sent"
    );
    assert_eq!(tree_rows(&mb).len(), 4, "the refused tie changed nothing");

    // --- with allowRetie: the mesh moves, 70 is deleted in the same group
    let mb = hammer_mock(world()).await;
    let server = build_server(&tree, &mb);
    let out = call(
        &server,
        "tie_to_entity",
        json!({"class": "trigger_once", "meshNodeIDs": [41, 42], "allowRetie": true}),
    )
    .await
    .unwrap();
    assert_eq!(wire(&mb, "entity.tieMeshes", 0)["allowRetie"], json!(true));
    assert_eq!(out["alreadyTied"], json!([]));
    assert_eq!(out["removedEmptyEntities"], json!([70]));
    let entity_id = out["nodeID"].as_i64().expect("entity");
    let rows = tree_rows(&mb);
    assert!(!rows.iter().any(|(id, _, _)| *id == 70), "{rows:?}");
    assert!(
        rows.contains(&(41, String::new(), Some(entity_id))),
        "{rows:?}"
    );
    assert!(
        rows.contains(&(42, String::new(), Some(entity_id))),
        "{rows:?}"
    );
    assert_eq!(out["undoGroups"], json!(1), "one tie group");

    // one undo restores BOTH: 70 back (none) and 41 under it again
    let undone = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(undone["undoName"], json!("MCP: tie meshes to entity"));
    let rows = tree_rows(&mb);
    assert!(
        rows.contains(&(70, "trigger_multiple".to_string(), None)),
        "{rows:?}"
    );
    assert!(rows.contains(&(41, String::new(), Some(70))), "{rows:?}");
    assert!(rows.contains(&(42, String::new(), None)), "{rows:?}");
}

/// A non-trigger `@SolidClass` (`func_buyzone`) does not get `toolstrigger`: the created volume
/// takes the `create_mesh` default and `materialSource` says why.
#[tokio::test]
async fn a_non_trigger_solid_class_gets_no_toolstrigger() {
    let tree = Tree::new("non-trigger-material");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "create_trigger",
        json!({"class": "func_buyzone", "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap();
    assert_eq!(
        out["material"],
        json!(hammer_mcp::tools::mesh_live::DEFAULT_MATERIAL)
    );
    assert_ne!(out["material"], json!(TOOLSTRIGGER));
    let source = out["materialSource"].as_str().expect("materialSource");
    assert!(source.contains("create_mesh default"), "{source}");
    assert!(!source.contains("toolstrigger"), "{source}");
    assert_eq!(
        wire(&mb, "mesh.create", 0)["material"],
        json!(hammer_mcp::tools::mesh_live::DEFAULT_MATERIAL)
    );
    assert!(!notes(&out).contains("toolstrigger"), "{}", notes(&out));
}

/// The optional bridge methods are gated **per call**: a bridge without `entity.find`/`entity.get`/
/// `entity.addConnections`/`mesh.setMaterial` still serves a call that uses none of them and refuses
/// one that does, before the first send.
#[tokio::test]
async fn optional_methods_are_gated_per_call() {
    let tree = Tree::new("per-call-caps");
    let reduced = |mb: &MockBridge| {
        let mut methods: Vec<String> = HAMMER_METHODS.iter().map(|m| m.to_string()).collect();
        methods.retain(|m| {
            ![
                "entity.addConnections",
                "entity.find",
                "entity.get",
                "mesh.setMaterial",
            ]
            .contains(&m.as_str())
        });
        mb.set_methods(methods);
    };

    // a box-only create_trigger without connections needs neither: it goes through
    let mb = hammer_mock(nodes()).await;
    reduced(&mb);
    let server = build_server(&tree, &mb);
    let out = call(
        &server,
        "create_trigger",
        json!({"class": "trigger_once", "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap();
    assert_eq!(out["undoGroups"], json!(2), "the volume and the tie");
    assert_eq!(mb.method_calls("mesh.create").len(), 1);
    assert_eq!(mb.method_calls("entity.tieMeshes").len(), 1);

    // a tie without `material` needs neither: it goes through
    let mb = hammer_mock(nodes()).await;
    reduced(&mb);
    let server = build_server(&tree, &mb);
    let out = call(
        &server,
        "tie_to_entity",
        json!({"class": "trigger_once", "meshNodeIDs": [41]}),
    )
    .await
    .unwrap();
    assert_eq!(out["undoGroups"], json!(1));
    assert_eq!(mb.method_calls("entity.tieMeshes").len(), 1);

    // a `connections` call needs entity.find/entity.get/entity.addConnections: refused before the
    // mesh, naming every missing method
    let mb = hammer_mock(nodes()).await;
    reduced(&mb);
    let server = build_server(&tree, &mb);
    let err = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_once",
            "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
            "connections": [conn("OnStartTouch", "ta_kill", "Enable")],
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NotImplemented", "{err:?}");
    assert_eq!(err.type_, "CapabilityMissing");
    assert_eq!(err.code, Some(-32011));
    assert!(err.retry_safe);
    let missing = err.data["missing"].as_array().expect("missing");
    for method in ["entity.find", "entity.get", "entity.addConnections"] {
        assert!(missing.iter().any(|m| m == method), "{method}: {missing:?}");
    }
    assert!(
        mb.method_calls("mesh.create").is_empty(),
        "the capability gate runs before the volume: {:?}",
        mb.requests()
    );

    // `retexture:true` on existing meshes needs mesh.setMaterial
    let mb = hammer_mock(nodes()).await;
    reduced(&mb);
    let server = build_server(&tree, &mb);
    let err = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_multiple",
            "meshNodeIDs": [41],
            "material": NODRAW,
            "retexture": true,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NotImplemented", "{err:?}");
    assert_eq!(err.data["missing"], json!(["mesh.setMaterial"]));
    // the capability refresh is the only traffic: no trigger-block method went out
    for method in ["mesh.create", "entity.tieMeshes", "mesh.setMaterial"] {
        assert!(
            mb.method_calls(method).is_empty(),
            "{method} was dispatched: {:?}",
            mb.requests()
        );
    }

    // ... and the same call without `material`/`retexture` still works on that bridge
    let out = call(
        &server,
        "create_trigger",
        json!({"class": "trigger_multiple", "meshNodeIDs": [41]}),
    )
    .await
    .unwrap();
    assert_eq!(out["undoGroups"], json!(1));
}

// -------------------------------------------------------------------------------------------
// FIX-A: the tie reply's echo, the no-op tie, the volume's own undo label, and the failures of
// every step the three-step flow can fail in
// -------------------------------------------------------------------------------------------

/// A4: the bridge's `keyValues` echo is the only signal for a key it refused. `applied:false` must
/// become a warning (the entity does NOT carry the key), not a silent success — and an absent echo
/// is reported as "not verified" instead of as success.
#[tokio::test]
async fn a_refused_keyvalue_is_warned_about_and_an_absent_echo_is_reported() {
    let tree = Tree::new("kv-echo");
    // a responder that answers the tie with the bridge's own shape, refusing one key
    let mb = hammer_mock_with(nodes(), |builder| {
        builder.responder("entity.tieMeshes", |_| {
            json!({
                "nodeID": 60,
                "handle": "n9",
                "class": "entity",
                "classname": "trigger_once",
                "created": false,
                "origin": [0.0, 0.0, 0.0],
                "meshes": [{"nodeID": 41, "origin": [0.0, 0.0, 0.0]}],
                "alreadyTied": [],
                "removedEmptyEntities": [],
                "undoLabel": "MCP: tie meshes to entity",
                "keyValues": {
                    "applied": {"targetname": false, "spawnflags": true},
                    "warnings": ["targetname: the attribute is not readable"]
                },
            })
        })
    })
    .await;
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "tie_to_entity",
        json!({
            "class": "trigger_once",
            "meshNodeIDs": [41],
            "targetname": "ta_tie",
            "spawnflags": ["Clients"],
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        out["applied"],
        json!({"targetname": false, "spawnflags": true})
    );
    assert_eq!(out["keyValuesEcho"], json!(true));
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().any(|warning| {
            let text = warning.as_str().unwrap_or_default();
            text.contains("\"targetname\"") && text.contains("did NOT write")
        }),
        "the refused key is a warning: {warnings:?}"
    );
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .unwrap_or_default()
            .contains("the attribute is not readable")),
        "the bridge's own warning text is kept: {warnings:?}"
    );
    assert!(
        !warnings.iter().any(|warning| warning
            .as_str()
            .unwrap_or_default()
            .contains("\"spawnflags\"")),
        "spawnflags was applied: {warnings:?}"
    );

    // the same call against a reply with NO echo at all: the keys are not verified, and the call
    // says so instead of reporting success
    let mb = hammer_mock_with(nodes(), |builder| {
        builder.responder("entity.tieMeshes", |_| {
            json!({
                "nodeID": 60,
                "class": "entity",
                "classname": "trigger_once",
                "created": false,
                "origin": [0.0, 0.0, 0.0],
                "meshes": [{"nodeID": 41, "origin": [0.0, 0.0, 0.0]}],
                "alreadyTied": [],
                "removedEmptyEntities": [],
                "undoLabel": "MCP: tie meshes to entity",
            })
        })
    })
    .await;
    let server = build_server(&tree, &mb);
    let out = call(
        &server,
        "tie_to_entity",
        json!({"class": "trigger_once", "meshNodeIDs": [41], "targetname": "ta_tie"}),
    )
    .await
    .unwrap();
    assert_eq!(out["keyValuesEcho"], json!(false));
    assert_eq!(out["applied"], json!({}));
    assert!(
        notes(&out).contains("no `keyValues` echo"),
        "{}",
        notes(&out)
    );
}

/// A8: an all-`alreadyTied` tie is a success no-op — the bridge opens no group, so the tool
/// reports `undoGroups:0`, counts no unsaved edit, says "nothing to do".
#[tokio::test]
async fn an_all_already_tied_tie_is_a_noop_with_no_undo_group() {
    let tree = Tree::new("all-already-tied");
    // 41 is already tied to the brush entity 60; 42 is a free world mesh
    let mut tied = mesh(41);
    tied.parent = Some(60);
    let mb = hammer_mock(vec![MockNode::world(), nodes()[1].clone(), mesh(42), tied]).await;
    let server = build_server(&tree, &mb);

    let out = call(
        &server,
        "tie_to_entity",
        json!({"entityID": 60, "meshNodeIDs": [41]}),
    )
    .await
    .unwrap();
    assert_eq!(out["entityCreated"], json!(false));
    assert_eq!(out["alreadyTied"], json!([41]));
    assert_eq!(out["undoGroups"], json!(0), "{out}");
    assert_eq!(out["undoLabels"], json!([]));
    assert_eq!(out["unsavedBridgeEdits"], json!(0), "no edit was counted");
    assert!(out.get("next").is_none(), "nothing changed, so nothing to save: {out}");
    assert!(notes(&out).contains("nothing to do"), "{}", notes(&out));
    assert!(notes(&out).contains("no undo group"), "{}", notes(&out));
    assert!(mb.method_calls("mesh.setMaterial").is_empty());

    // a mixed list is a real tie again: one group, one label
    let out = call(
        &server,
        "tie_to_entity",
        json!({"entityID": 60, "meshNodeIDs": [41, 42]}),
    )
    .await
    .unwrap();
    assert_eq!(out["alreadyTied"], json!([41]));
    assert_eq!(out["undoGroups"], json!(1));
    assert_eq!(
        out["undoLabels"],
        json!(["MCP: tie meshes to entity"]),
        "the alreadyTied mesh contributes no label"
    );
    assert_eq!(out["next"][0]["tool"], json!("save_map"), "a real tie is an unsaved edit: {out}");
}

/// A6: the volume's undo label is the bridge's own (`mesh.create`'s reply), not a string this tool
/// invented — the responder answers a label the tool could not have guessed, and the group the
/// call reports carries it.
#[tokio::test]
async fn the_volume_step_reports_the_bridges_own_undo_label() {
    let tree = Tree::new("mesh-label");
    // a reply with a label this server never spells itself: the tie then fails (the mesh is not in
    // the mock's world), which turns the call into the PartialFailure that reports `undoLabels`
    let mb = hammer_mock_with(nodes(), |builder| {
        builder.responder("mesh.create", |_| {
            json!({
                "nodeID": 900,
                "class": "mesh",
                "origin": [0.0, 0.0, 0.0],
                "vertexSlots": 8,
                "faceSlots": 6,
                "faces": 6,
                "materials": 1,
                "undoLabel": "Create Polygon (Hammer)",
            })
        })
    })
    .await;
    let server = build_server(&tree, &mb);

    let err = call(
        &server,
        "create_trigger",
        json!({"class": "trigger_once", "box": {"min": [0, 0, 0], "max": [8, 8, 8]}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PartialFailure", "{err:?}");
    assert_eq!(
        err.data["undoLabels"],
        json!(["Create Polygon (Hammer)"]),
        "the bridge's own label, not a hard-coded one: {err:?}"
    );
}

/// A10: a failure in step (c) (the connections) is a `PartialFailure` naming BOTH nodes this call
/// created (the volume and the entity) and the two groups it opened.
#[tokio::test]
async fn a_connections_failure_names_the_mesh_and_the_entity() {
    let tree = Tree::new("connections-fail");
    let mb = hammer_mock_with(nodes(), |builder| {
        builder.error(
            "entity.addConnections",
            -32602,
            "InvalidParams",
            Some(json!({"reason": "no such input"})),
        )
    })
    .await;
    let server = build_server(&tree, &mb);

    let err = call(
        &server,
        "create_trigger",
        json!({
            "class": "trigger_once",
            "box": {"min": [0, 0, 0], "max": [8, 8, 8]},
            "connections": [conn("OnStartTouch", "ta_kill", "Enable")],
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PartialFailure", "{err:?}");
    assert_eq!(err.data["step"], json!("entity.addConnections"));
    assert_eq!(err.data["undoGroups"], json!(2), "{err:?}");
    assert_eq!(
        err.data["undoLabels"],
        json!(["MCP: create mesh", "MCP: tie meshes to entity"])
    );
    let mesh_id = err.data["meshNodeID"].as_i64().expect("the created mesh");
    let entity_id = err.data["nodeID"].as_i64().expect("the created entity");
    assert_eq!(err.data["createdNodes"], json!([mesh_id, entity_id]));
    assert!(
        err.message.contains(&mesh_id.to_string()),
        "{}",
        err.message
    );
    assert!(
        err.message.contains(&entity_id.to_string()),
        "{}",
        err.message
    );
    assert!(!err.retry_safe, "a retry would create a second entity");
    // the entity and its volume really are in the document (only the connections step failed)
    let rows = tree_rows(&mb);
    assert!(
        rows.contains(&(entity_id, "trigger_once".to_string(), None)),
        "{rows:?}"
    );
    assert!(
        rows.contains(&(mesh_id, String::new(), Some(entity_id))),
        "{rows:?}"
    );
}

/// A10: a failure in step (b2) (the retexture) is a `PartialFailure` naming the entity the tie
/// created, with the tie's one group — the retexture step never opened its own.
#[tokio::test]
async fn a_retexture_failure_names_the_entity_it_created() {
    let tree = Tree::new("retexture-fail");
    let mb = hammer_mock_with(nodes(), |builder| {
        builder.error(
            "mesh.setMaterial",
            -32602,
            "InvalidParams",
            Some(json!({"reason": "not a mesh", "nodeID": 41})),
        )
    })
    .await;
    let server = build_server(&tree, &mb);

    let err = call(
        &server,
        "tie_to_entity",
        json!({
            "class": "trigger_once",
            "meshNodeIDs": [41],
            "material": NODRAW,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PartialFailure", "{err:?}");
    assert_eq!(err.data["step"], json!("mesh.setMaterial"));
    assert_eq!(err.data["undoGroups"], json!(1), "{err:?}");
    assert_eq!(err.data["undoLabels"], json!(["MCP: tie meshes to entity"]));
    let entity_id = err.data["nodeID"].as_i64().expect("the created entity");
    assert_eq!(err.data["createdNodes"], json!([entity_id]));
    assert_eq!(err.data["meshNodeID"], Value::Null);
    // the tie landed: the mesh is the entity's child, and the entity exists
    let rows = tree_rows(&mb);
    assert!(
        rows.contains(&(entity_id, "trigger_once".to_string(), None)),
        "{rows:?}"
    );
    assert!(
        rows.contains(&(41, String::new(), Some(entity_id))),
        "{rows:?}"
    );
}
