//! `create_props` tests (WP-R1): the tool against the mock bridge's Hammer world plus a real
//! model catalog and FGD database.
//!
//! **The fixture tree** (`common::FgdTree` + the synthetic VPKs of `tests/model_info.rs`'s
//! approach): `props_mini.fgd` is rewritten with the prop classes this suite needs — `tags(
//! Prop* )` plus the `model(studio)` / `skin(string)` keys the merged keyvalues are validated
//! against — and one propless class for the `NotAPropClass` negative. The VPKs carry `chair`
//! (a full model: render bounds [−10,−10,−2]..[10,10,46] → floorOffset 2, no render bounds in
//! the addon), `broken` (a decoy `.vmdl_c` the decoder refuses) and the addon pair
//! `stool.vmdl_c` (compiled, DATA-only) / `unbuilt.vmdl` (source, uncompiled).
//!
//! **The bridge.** The mock's Hammer world serves `doc.active`/`edit.batch`/`entity.create`;
//! an old (pre-m2.18) bridge is simulated with `.methods(...)` advertising everything but
//! `node.setTransform` — the built-ins still answer (WP-R2's mock models them), only the
//! advertisement gates what `create_props` checks. The exact `edit.batch` ops JSON is asserted.
//!
//! Covered: the negative list of target/WP.md (bad kind/class combos, non-prop class, unknown
//! model with suggestions, uncompiled model, scales out of range, forbidden keyValues, 0 and 65
//! props, old bridge with scales, FGD-invalid keyvalue), the live batch shape, snapToFloor, the
//! maybe-executed error path, and the WP-R1 model check of `set_entity_keyvalues`.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::resource::kv3bin::Kv3Value;
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod common;

use common::{kv_object, kv_strings, kv_text, kv_vec3, model_vmdl_c};

/// The document path the mock serves (Windows form).
const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";

/// The fixture model's canonical spelling (the catalog's normalization is the test's too).
const CHAIR: &str = "models/props/fixture/chair.vmdl";
/// The uncompiled addon model.
const UNBUILT: &str = "models/props/fixture/unbuilt.vmdl";

/// The prop classes this suite validates against (replaces `common::PROPS_FGD` in the copied
/// tree: exactly the same idea, plus `model(studio)`/`skin(string)` — the keyvalues a prop
/// create sends are declared — a prop-tagged class the mock's registry does NOT carry
/// (`prop_rope`, the batch partial-failure producer) and a class with no prop tag for the
/// `NotAPropClass` negative).
const PROPS_FGD: &str = "\
// WP-R1 test fixture: prop classes for create_props (tags mirror core/base.fgd).
@PointClass base(Targetname) tags( PropStatic ) = prop_static : \"A compile-time static prop.\"
[
	model(studio) : \"Model\" : \"\"
	skin(string) : \"Skin\" : \"0\"
]

@PointClass base(Targetname) tags( PropDynamic ) = prop_dynamic : \"A dynamic prop.\"
[
	model(studio) : \"Model\" : \"\"
	skin(string) : \"Skin\" : \"0\"
	solid(integer) : \"VPhysics mode\" : 0
]

@PointClass base(Targetname) tags( PropPhysics ) = prop_physics : \"A physics prop.\"
[
	model(studio) : \"Model\" : \"\"
	skin(string) : \"Skin\" : \"0\"
]

// FGD-tagged but NOT in the mock bridge's entity.create registry (the -32012 producer).
@PointClass base(Targetname) tags( PropDynamic ) = prop_rope : \"A rope prop (registry gap).\"
[
	model(studio) : \"Model\" : \"\"
	skin(string) : \"Skin\" : \"0\"
]

@PointClass base(Targetname) = trigger_multiple : \"A trigger: in the FGD, but no prop tag.\"
[
	input Enable(void) : \"\"
	output OnStartTouch(void) : \"\"
]
";

struct Tree(PathBuf);

impl Tree {
    /// The fixture FGD copy with this suite's `props_mini.fgd` (the `@include` line the
    /// fixture added to `csgo.fgd` already points at the file, so overwriting it is enough —
    /// `common::FgdTree`'s own prop text is replaced wholesale).
    fn tree_new(tag: &str) -> Tree {
        let fixture = common::FgdTree::new(tag);
        std::fs::write(fixture.game().join("csgo/props_mini.fgd"), PROPS_FGD).expect("props fgd");
        // the FgdTree's drop must not fire on the moved-out root: forget it, Tree::drop cleans up
        let root = fixture.root.clone();
        std::mem::forget(fixture);
        Tree(root)
    }

    fn app(&self) -> PathBuf {
        self.0.join("workshop/run/app")
    }

    /// The csgo VPK (chair with full facts, broken decoy) + the core VPK (axis), the addon
    /// pair and the content tree.
    fn build_models(&self) {
        common::write_vpk(
            &self.app().join("game/csgo"),
            "pak01",
            2,
            &[
                common::BuildFile::loaded(
                    "models/props/fixture/chair.vmdl_c",
                    &chair_bytes(),
                    b"",
                    false,
                ),
                common::BuildFile::loaded(
                    "models/props/fixture/broken.vmdl_c",
                    &broken_bytes(),
                    b"",
                    false,
                ),
                common::BuildFile::loaded(
                    "agents/models/tm_test/tm_test.vmdl_c",
                    &chair_bytes(),
                    b"",
                    false,
                ),
                common::BuildFile::preload_only("models/dev/etc.vtex_c", b""),
            ],
        );
        common::write_vpk(
            &self.app().join("game/core"),
            "pak01",
            1,
            &[common::BuildFile::loaded(
                "models/dev/axis.vmdl_c",
                &model_vmdl_c(&[("DATA", axis_data())]),
                b"",
                true,
            )],
        );
        let game = self
            .app()
            .join("game/csgo_addons/mcp_test/models/props/fixture/stool.vmdl_c");
        std::fs::create_dir_all(game.parent().unwrap()).expect("addon game dir");
        std::fs::write(&game, model_vmdl_c(&[("DATA", stool_data())])).expect("compiled addon");
        let content = self
            .app()
            .join("content/csgo_addons/mcp_test/models/props/fixture");
        std::fs::create_dir_all(&content).expect("addon content dir");
        std::fs::write(
            content.join("unbuilt.vmdl"),
            model_vmdl_c(&[("DATA", stool_data())]),
        )
        .expect("uncompiled source");
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------- fixture model bytes

/// The chair's `DATA` block (named model, two skins) + the render box and the bodygroup the
/// snapToFloor/read tests use. No `m_modelInfo` boxes: the zeros stay honest.
fn chair_data() -> Kv3Value {
    kv_object(vec![
        ("m_name", kv_text("models/props/fixture/chair.vmdl")),
        (
            "m_materialGroups",
            Kv3Value::Array(vec![
                kv_object(vec![("m_name", kv_text("default"))]),
                kv_object(vec![("m_name", kv_text("tint"))]),
            ]),
        ),
        (
            "m_meshGroups",
            kv_strings(&["chair_@0_#&open", "chair_@1_#&folded"]),
        ),
    ])
}

/// One MDAT scene object: `[-10,-10,-2]..[10,10,46]` (floorOffset 2.0, footprint [20, 20]).
fn chair_mdat() -> Kv3Value {
    kv_object(vec![(
        "m_sceneObjects",
        Kv3Value::Array(vec![kv_object(vec![
            ("m_vMinBounds", kv_vec3(-10.0, -10.0, -2.0)),
            ("m_vMaxBounds", kv_vec3(10.0, 10.0, 46.0)),
        ])]),
    )])
}

/// The addon's compiled stool: DATA only, no render bounds at all (ModelBounds negative).
fn stool_data() -> Kv3Value {
    kv_object(vec![
        ("m_name", kv_text("models/props/fixture/stool.vmdl")),
        ("m_materialGroups", Kv3Value::Array(Vec::new())),
    ])
}

fn axis_data() -> Kv3Value {
    kv_object(vec![
        ("m_name", kv_text("models/dev/axis.vmdl")),
        (
            "m_modelInfo",
            kv_object(vec![("m_flMass", Kv3Value::Double(12.5))]),
        ),
    ])
}

fn chair_bytes() -> Vec<u8> {
    model_vmdl_c(&[("DATA", chair_data()), ("MDAT", chair_mdat())])
}

/// Decoy bytes: a valid container shape whose `DATA` body is not a KV3 document at all.
fn broken_bytes() -> Vec<u8> {
    model_vmdl_c(&[("DATA", kv_text("not a kv3 document"))])
}

// ---------------------------------------------------------------- harness

/// The Hammer world every test starts from: just the world node (created props get nodeIDs 2, 3,
/// ...) so the mock's `hammer_scales`/`hammer_nodes` assertions are predictable.
fn nodes() -> Vec<MockNode> {
    vec![MockNode::world()]
}

/// The set_entity_keyvalues tests need an existing prop (nodeID 30, FGD class from the fixture).
fn prop_nodes() -> Vec<MockNode> {
    vec![
        MockNode::world(),
        MockNode::entity(
            30,
            "prop_static",
            [1.0, 2.0, 3.0],
            kv(&[("model", CHAIR), ("targetname", "wall")]),
        ),
        MockNode::entity(
            45,
            "prop_dynamic",
            [4.0, 5.0, 6.0],
            kv(&[("targetname", "door")]),
        ),
    ]
}

fn kv(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

async fn hammer_mock(world_nodes: Vec<MockNode>) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(world_nodes)
        .start()
        .await
        .unwrap()
}

/// An old (pre-m2.18) bridge: the built-ins are NOT there (no hammer world — an old bridge has
/// no `node.setTransform` advertise), `doc.active` and the batch come from scripted responders
/// that emulate the m2.15 create semantics (results per op, no scales knowledge). The tool's
/// preflight still passes (`edit.batch`/`entity.create` are advertised — every m2.15 bridge has
/// them) and the reply's own `valueUndo`-style shape is what `create_props` renders.
async fn old_bridge() -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .methods(["control.ping", "doc.active", "entity.create", "edit.batch"])
        .responder("edit.batch", move |params| {
            let ops = params["ops"].as_array().cloned().unwrap_or_default();
            let results: Vec<Value> = ops
                .iter()
                .enumerate()
                .map(|(index, op)| {
                    json!({
                        "handle": format!("n{}", index + 1),
                        "class": op["params"]["class"],
                        "origin": op["params"]["origin"],
                    })
                })
                .collect();
            json!({"results": results, "failedIndex": Value::Null})
        })
        .start()
        .await
        .unwrap()
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

fn server(tree: &Tree, mb: &MockBridge) -> McpServer<HammerState> {
    hammer_mcp::build_server(&config(tree, &mb.discovery_path)).unwrap()
}

fn config(tree: &Tree, discovery: &Path) -> CommonConfig {
    use clap::Parser;
    #[derive(clap::Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.0.display().to_string();
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

/// The (sorted) node ids the mock world holds after a test.
fn node_ids(mb: &MockBridge) -> Vec<i64> {
    let mut ids: Vec<i64> = mb.hammer_nodes().iter().map(|n| n.node_id).collect();
    ids.sort();
    ids
}

fn node_of(mb: &MockBridge, id: i64) -> MockNode {
    mb.hammer_nodes()
        .into_iter()
        .find(|n| n.node_id == id)
        .unwrap_or_else(|| panic!("node {id} gone"))
}

fn keyvalues_of(mb: &MockBridge, id: i64) -> BTreeMap<String, String> {
    node_of(mb, id).keyvalues
}

// -------------------------------------------------------------------------------------------
// Acceptance
// -------------------------------------------------------------------------------------------

/// A static and a dynamic prop, fields + keyValues: the batch is exactly the two
/// `entity.create` ops of BRIDGE-M2.18-SPEC §1/§2 (`{class, origin?, angles?, scales?,
/// keyValues}`), one `edit.batch` request, one undo group the bridge labels itself.
#[tokio::test]
async fn create_props_sends_the_exact_ops_json() {
    let tree = Tree::tree_new("ops");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_props",
        json!({
            "props": [
                {"kind": "static", "model": CHAIR,
                 "origin": [10.0, 20.0, 30.0],
                 "skin": "default", "targetname": "chair_a"},
                {"kind": "dynamic", "model": CHAIR,
                 "origin": [1.5, 2.5, 3.5], "angles": [0.0, 90.0, 0.0], "scales": [2.0, 2.0, 2.0],
                 "targetname": "chair_b"}
            ]
        }),
    )
    .await
    .unwrap();

    // the exact ops the bridge received
    let batches = mb.method_calls("edit.batch");
    assert_eq!(batches.len(), 1, "{out}");
    let batch = &batches[0].params;
    assert_eq!(
        batch["undoLabel"],
        json!("MCP: create props"),
        "the echoed label (the bridge opens 'MCP: batch' itself)"
    );
    let ops = batch["ops"].as_array().expect("ops");
    let expected_ops = json!([
        {"method": "entity.create", "params": {
            "class": "prop_static",
            "origin": [10.0, 20.0, 30.0],
            "keyValues": {
                "model": CHAIR, "skin": "default", "targetname": "chair_a"
            }
        }},
        {"method": "entity.create", "params": {
            "class": "prop_dynamic",
            "origin": [1.5, 2.5, 3.5],
            "angles": [0.0, 90.0, 0.0],
            "scales": [2.0, 2.0, 2.0],
            "keyValues": {
                "model": CHAIR, "targetname": "chair_b"
            }
        }}
    ]);
    assert_eq!(
        ops.as_slice(),
        expected_ops.as_array().expect("literal ops").as_slice(),
        "the exact op JSON of the contract (defaults omitted, never sent empty)"
    );
    assert!(
        !ops[0]["params"].to_string().contains("angles"),
        "an absent field is never sent: {}",
        ops[0]
    );
    assert!(
        !ops[0]["params"].to_string().contains("scales"),
        "an absent field is never sent: {}",
        ops[0]
    );
    assert_eq!(batch["expectDoc"]["id"], json!("d1"));
    assert!(batch["expectDoc"]["epoch"].is_i64(), "{batch}");
    assert!(batches[0].params["deadlineMs"].as_u64().unwrap_or(0) > 0);

    // the result
    assert_eq!(out["count"], json!(2));
    assert_eq!(out["requested"], json!(2));
    assert_eq!(out["undoLabel"], json!("MCP: batch"));
    assert_eq!(out["requestedUndoLabel"], json!("MCP: create props"));
    assert_eq!(out["saved"], json!(false));
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
    assert!(out["fgd"]["files"].as_i64().unwrap_or(0) >= 1, "{out}");
    let created = out["created"].as_array().expect("created");
    assert_eq!(created.len(), 2);
    assert_eq!(created[0]["index"], json!(0));
    assert_eq!(created[0]["class"], json!("prop_static"));
    assert_eq!(created[0]["kind"], json!("static"));
    assert_eq!(created[0]["model"], json!(CHAIR));
    assert_eq!(created[0]["origin"], json!([10.0, 20.0, 30.0]));
    assert_eq!(created[0]["handleIssued"], json!(true));
    assert!(
        created[0].get("handle").is_none(),
        "handles are not returned"
    );
    // the created node's id (bridge m2.18 create reply; review W2 3a) and the transform THIS call
    // requested: `angles`/`scales` echo the request, `null` when the caller omitted the field
    // (review W2 3b)
    assert_eq!(created[0]["nodeID"], json!(2), "{out}");
    assert_eq!(created[0]["angles"], Value::Null);
    assert_eq!(created[0]["scales"], Value::Null);
    let static_notes = created[0]["notes"].as_array().expect("static notes");
    assert!(
        static_notes
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("compile_map")),
        "static props carry the compile-time note: {}",
        created[0]
    );
    assert_eq!(created[1]["kind"], json!("dynamic"));
    assert_eq!(created[1]["nodeID"], json!(3), "{out}");
    assert_eq!(
        created[1]["angles"],
        json!([0.0, 90.0, 0.0]),
        "the request's own angles (review W2 3b): {out}"
    );
    assert_eq!(
        created[1]["scales"],
        json!([2.0, 2.0, 2.0]),
        "the request's own scales, alongside the bridge's applied echo: {out}"
    );
    let dynamic_notes = created[1]["notes"].as_array().expect("dynamic notes");
    assert!(
        dynamic_notes
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("runtime entity")),
        "{}",
        created[1]
    );
    assert!(
        !created[1]["notes"].to_string().contains("compile_map"),
        "a dynamic prop is not baked: {}",
        created[1]["notes"]
    );
    // the whole call's notes: no auto-save
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("save_map")),
        "{notes:?}"
    );

    // the mock world really holds the two entities (text keyvalues, scales in the map)
    assert_eq!(node_ids(&mb), vec![1, 2, 3]);
    assert_eq!(
        keyvalues_of(&mb, 2).get("model").map(String::as_str),
        Some(CHAIR)
    );
    assert_eq!(
        keyvalues_of(&mb, 2).get("targetname").map(String::as_str),
        Some("chair_a")
    );
    assert_eq!(mb.hammer_scales(3), [2.0, 2.0, 2.0]);
    assert_eq!(node_of(&mb, 3).angles, [0.0, 90.0, 0.0]);
}

/// Any spelling the catalog normalizes (case, backslashes, the `.vmdl_c` form) places the model
/// under its canonical spelling.
#[tokio::test]
async fn a_win_uppercase_model_spelling_places_the_canonical_path() {
    let tree = Tree::tree_new("spelling");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_props",
        json!({"props": [{"kind": "physics",
                          "model": "MODELS\\PROPS\\FIXTURE\\CHAIR.VMDL",
                          "origin": [0, 0, 0]}]}),
    )
    .await
    .unwrap();

    assert_eq!(out["count"], json!(1));
    let created = out["created"].as_array().expect("created");
    assert_eq!(created[0]["model"], json!(CHAIR));
    assert_eq!(created[0]["class"], json!("prop_physics"));
    let ops = mb.method_calls("edit.batch");
    assert_eq!(
        ops[0].params["ops"][0]["params"]["keyValues"]["model"],
        json!(CHAIR)
    );
}

/// I6: the shipped player agents live under `agents/models/`; `create_props` places them (the
/// other roots stay refused).
#[tokio::test]
async fn an_agent_model_is_placeable() {
    let tree = Tree::tree_new("agent-model");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_props",
        json!({"props": [{"class": "prop_dynamic", "model": "Agents\\Models\\TM_Test\\tm_test.vmdl", "origin": [0, 0, 0]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["created"][0]["model"], json!("agents/models/tm_test/tm_test.vmdl"));
    let err = call(
        &server,
        "create_props",
        json!({"props": [{"class": "prop_dynamic", "model": "characters/models/tm_test/tm_test.vmdl", "origin": [0, 0, 0]}]}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("models/"), "{}", err.message);
}

/// The model catalog's near misses ride `ModelNotFound` (up to 5), and index-labeled
/// multi-prop refusals point at their prop. Nothing is sent either way.
#[tokio::test]
async fn an_unknown_model_is_refused_with_suggestions_and_the_index() {
    let tree = Tree::tree_new("unknown-model");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "create_props",
        json!({"props": [
            {"kind": "static", "model": CHAIR, "origin": [0, 0, 0]},
            {"kind": "static", "model": "models/props/fixture/chare.vmdl", "origin": [1, 1, 1]}
        ]}),
    )
    .await
    .unwrap_err();

    assert_eq!(err.type_, "ModelNotFound");
    assert_eq!(err.name, "ModelNotFound");
    assert!(
        err.message.contains("props[1]"),
        "the multi-prop refusal names its prop: {}",
        err.message
    );
    let suggestions = err.data["didYouMean"].as_array().expect("suggestions");
    assert!(
        suggestions.iter().any(|s| s.as_str() == Some(CHAIR)),
        "the one-letter miss suggests chair: {suggestions:?}"
    );
    assert!(suggestions.len() <= 5);
    assert_eq!(err.data["model"], json!("models/props/fixture/chare.vmdl"));
    assert!(mb.method_calls("edit.batch").is_empty(), "nothing sent");
}

/// An uncompiled addon model is refused `ModelNotCompiled` (with the `model_compile` hint):
/// the compiled form is what Hammer draws. `allowUncompiled:true` places it with a note.
#[tokio::test]
async fn an_uncompiled_addon_model_is_refused_unless_allowed() {
    let tree = Tree::tree_new("uncompiled");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "create_props",
        json!({"props": [{"kind": "dynamic", "model": UNBUILT, "origin": [0, 0, 0]}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "ModelNotCompiled");
    assert_eq!(err.name, "ModelNotCompiled");
    assert!(err.message.contains(UNBUILT), "{}", err.message);
    assert_eq!(err.data["useInstead"]["tool"], json!("model_compile"));
    assert!(mb.method_calls("edit.batch").is_empty());

    // allowed: it is placed, with the "does not draw until compiled" note
    let out = call(
        &server,
        "create_props",
        json!({"allowUncompiled": true,
               "props": [{"kind": "dynamic", "model": UNBUILT, "origin": [0, 0, 0]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1));
    let created = out["created"].as_array().expect("created");
    let notes = created[0]["notes"].as_array().expect("notes");
    assert!(
        notes.iter().any(|note| note
            .as_str()
            .unwrap_or_default()
            .contains("no compiled .vmdl_c")),
        "{}",
        created[0]
    );
    assert_eq!(mb.method_calls("edit.batch").len(), 1);
}

/// Exactly one of kind/class: both, neither, an unknown spelling and a non-string are all
/// refused locally. Nothing is sent.
#[tokio::test]
async fn the_kind_class_args_are_strict() {
    let tree = Tree::tree_new("kind-class");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    for (args, needle) in [
        // both
        (
            json!({"props": [{"kind": "static", "class": "prop_dynamic", "model": CHAIR,
                              "origin": [0, 0, 0]}]}),
            "exactly one of kind or class",
        ),
        // neither
        (
            json!({"props": [{"model": CHAIR, "origin": [0, 0, 0]}]}),
            "exactly one of kind",
        ),
        // unknown kind
        (
            json!({"props": [{"kind": "animated", "model": CHAIR, "origin": [0, 0, 0]}]}),
            "\"static\", \"dynamic\" or \"physics\"",
        ),
        // non-string kind
        (
            json!({"props": [{"kind": 3, "model": CHAIR, "origin": [0, 0, 0]}]}),
            "\"static\", \"dynamic\" or \"physics\"",
        ),
        // unknown prop field
        (
            json!({"props": [{"kind": "static", "model": CHAIR, "origin": [0, 0, 0],
                              "scale": [1, 2, 3]}]}),
            "unknown props[0]",
        ),
        // unknown call argument
        (
            json!({"props": [{"kind": "static", "model": CHAIR, "origin": [0, 0, 0]}],
                   "undoLabel": "nope"}),
            "unknown argument",
        ),
    ] {
        let err = call(&server, "create_props", args).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{needle}: {}", err.message);
        assert!(err.message.contains(needle), "{needle}: {}", err.message);
    }
    assert!(
        mb.method_calls("edit.batch").is_empty(),
        "the shape pass runs before anything touches the bridge"
    );
    assert!(
        mb.method_calls("doc.active").is_empty(),
        "not even the document was read"
    );
}

/// An explicit class is checked against the FGD: unknown -> `UnknownClass`, present but without
/// a prop tag (itself or through its bases) -> `NotAPropClass`. Nothing is sent.
#[tokio::test]
async fn an_explicit_class_must_be_known_and_prop_tagged() {
    let tree = Tree::tree_new("explicit-class");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "create_props",
        json!({"props": [{"class": "mister_relay", "model": CHAIR, "origin": [0, 0, 0]}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "UnknownClass");
    assert_eq!(err.code, Some(-32006));
    assert!(mb.method_calls("edit.batch").is_empty());

    let err = call(
        &server,
        "create_props",
        json!({"props": [{"class": "trigger_multiple", "model": CHAIR, "origin": [0, 0, 0]}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "NotAPropClass", "{}", err.message);
    assert_eq!(err.name, "NotAPropClass");
    assert!(err.message.contains("prop tag"), "{}", err.message);
    assert!(
        err.message.contains("create_entities"),
        "the refusal points at the general tool: {}",
        err.message
    );
    assert!(mb.method_calls("edit.batch").is_empty());
}

/// The scales bound is the m2.18 builder's own `(0, 1000]`, refused locally per prop
/// (0, negative, non-finite, wrong arity, a non-number) before anything is read or sent.
#[tokio::test]
async fn scales_out_of_bound_are_refused_locally() {
    let tree = Tree::tree_new("scales-bounds");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    for (label, scales) in [
        ("zero", json!([0.0, 1.0, 1.0])),
        ("negative", json!([-1.0, 1.0, 1.0])),
        ("above", json!([1.0, 1.0, 1000.5])),
        ("not a number", json!([1.0, "x", 1.0])),
        ("wrong arity", json!([1.0, 1.0])),
    ] {
        let err = call(
            &server,
            "create_props",
            json!({"props": [{"kind": "dynamic", "model": CHAIR,
                              "origin": [0, 0, 0], "scales": scales}]}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{label}: {}", err.message);
        assert!(
            err.message.contains("(0, 1000]") || err.message.contains("scales"),
            "{label}: {}",
            err.message
        );
    }
    assert!(
        mb.method_calls("doc.active").is_empty(),
        "the scales refusal beats the bridge"
    );
    assert!(mb.method_calls("edit.batch").is_empty());
}

/// `model`/`classname`/`origin`/`angles`/`scales`/`skin`/`targetname` cannot ride `keyValues` —
/// those are the tool's own validated fields (any spelling of the key).
#[tokio::test]
async fn the_identity_keys_cannot_ride_keyvalues() {
    let tree = Tree::tree_new("forbidden-kv");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    for key in ["model", "origin", "angles", "scales", "skin", "targetname"] {
        let err = call(
            &server,
            "create_props",
            json!({"props": [{"kind": "dynamic", "model": CHAIR, "origin": [0, 0, 0],
                              "keyValues": {key: "x"}}]}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{key}: {}", err.message);
        assert!(
            err.message.contains("cannot be set as a keyvalue"),
            "{key}: {}",
            err.message
        );
    }
    // `classname` is refused by the shared keyvalue reader itself (either spelling): the class
    // is the `class` field on a create
    for spelling in ["classname", "Classname"] {
        let err = call(
            &server,
            "create_props",
            json!({"props": [{"kind": "dynamic", "model": CHAIR, "origin": [0, 0, 0],
                              "keyValues": {spelling: "x"}}]}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{spelling}: {}", err.message);
        assert!(
            err.message.contains("the class is the `class` field"),
            "{spelling}: {}",
            err.message
        );
    }
    assert!(mb.method_calls("edit.batch").is_empty());
}

/// An FGD-invalid keyvalue refuses the whole call (an invalid entity is never knowingly
/// created, unlike create_entities' warning-default). Nothing is sent.
#[tokio::test]
async fn an_fgd_invalid_keyvalue_refuses_the_call() {
    let tree = Tree::tree_new("fgd-invalid");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    // an undeclared key is an UnknownKey *error* for prop creates
    let err = call(
        &server,
        "create_props",
        json!({"props": [{"kind": "dynamic", "model": CHAIR, "origin": [0, 0, 0],
                          "keyValues": {"bakeshadows": 1}}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(
        err.message.contains("is not declared by class"),
        "{}",
        err.message
    );
    assert!(mb.method_calls("edit.batch").is_empty());

    // a declared key of the wrong type equally refuses (solid is an FGD integer)
    let err = call(
        &server,
        "create_props",
        json!({"props": [{"kind": "dynamic", "model": CHAIR, "origin": [0, 0, 0],
                          "keyValues": {"solid": "maybe"}}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(
        err.message.contains("expects an integer value"),
        "{}",
        err.message
    );
    assert!(mb.method_calls("edit.batch").is_empty());
}

/// The declared keys ride through: `skin` plus one FGD keyvalue land in the op's keyValues.
#[tokio::test]
async fn fgd_keyvalues_from_keyvalues_land_in_the_op() {
    let tree = Tree::tree_new("fgd-ok");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_props",
        json!({"props": [{"kind": "physics", "model": CHAIR, "origin": [0, 0, 0],
                          "skin": "tint", "targetname": "phys_a"}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1));
    let ops = mb.method_calls("edit.batch");
    assert_eq!(
        ops[0].params["ops"][0]["params"]["keyValues"],
        json!({"model": CHAIR, "skin": "tint", "targetname": "phys_a"}),
        "skin is a first-class field: {out}"
    );
    assert_eq!(
        keyvalues_of(&mb, 2).get("skin").map(String::as_str),
        Some("tint")
    );
}

/// `props: []` (nothing to do), `missing props` and 65 entries (one over the batch bound) are
/// refused before anything is sent.
#[tokio::test]
async fn the_props_array_bound_is_enforced() {
    let tree = Tree::tree_new("bounds");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let err = call(&server, "create_props", json!({"props": []}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(err.message.contains("empty"), "{}", err.message);

    let err = call(&server, "create_props", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(err.message.contains("props is required"), "{}", err.message);

    let capacity = 64;
    let props: Vec<Value> = (0..capacity + 1)
        .map(|i| json!({"kind": "static", "model": CHAIR, "origin": [i, i, i]}))
        .collect();
    let err = call(&server, "create_props", json!({"props": props}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(err.message.contains("at most 64"), "{}", err.message);
    assert!(mb.method_calls("edit.batch").is_empty());

    // exactly 64 go through (the batch bound)
    let props: Vec<Value> = (0..capacity)
        .map(|i| json!({"kind": "static", "model": CHAIR, "origin": [i, i, i]}))
        .collect();
    let out = call(&server, "create_props", json!({"props": props}))
        .await
        .unwrap();
    assert_eq!(out["count"], json!(capacity));
}

/// A bridge that does not advertise the m2.18 marker (`node.setTransform`) refuses any
/// scales-bearing prop `BridgeTooOld` before the catalog is consulted; without scales the same
/// bridge still creates the props, with the pre-m2.18 note that the model does not draw until
/// the document reloads (re-props.md Q1).
#[tokio::test]
async fn an_old_bridge_refuses_scales_and_notes_the_scaleless_ones() {
    let tree = Tree::tree_new("old-bridge");
    tree.build_models();
    let mb = old_bridge().await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "create_props",
        json!({"props": [
            {"kind": "dynamic", "model": CHAIR, "origin": [0, 0, 0], "scales": [2, 2, 2]},
            {"kind": "static", "model": CHAIR, "origin": [5, 0, 0]}
        ]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "BridgeTooOld");
    assert_eq!(err.name, "BridgeTooOld");
    assert_eq!(err.code, Some(-32011));
    assert_eq!(err.data["missing"], json!(["node.setTransform"]));
    assert!(err.retry_safe, "nothing was sent");
    assert!(err.message.contains("scales"), "{}", err.message);
    assert!(mb.method_calls("edit.batch").is_empty());

    // scale-less props go through, and the reply names the reload consequence
    let out = call(
        &server,
        "create_props",
        json!({"props": [{"kind": "dynamic", "model": CHAIR, "origin": [0, 0, 0]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1));
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes.iter().any(|note| note
            .as_str()
            .unwrap_or_default()
            .contains("older than m2.18")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("reload")),
        "the reload consequence is named: {notes:?}"
    );
    assert_eq!(mb.method_calls("edit.batch").len(), 1);
}

/// The m2.18 happy path: the scales ride the op's params (asserted by
/// `create_props_sends_the_exact_ops_json`) and the mock world stores them.
#[tokio::test]
async fn scales_are_sent_and_stored_in_the_world() {
    let tree = Tree::tree_new("scales-ok");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    call(
        &server,
        "create_props",
        json!({"props": [{"kind": "dynamic", "model": CHAIR,
                          "origin": [0, 0, 0], "scales": [2, 1.5, 2]}]}),
    )
    .await
    .expect("the scaled create goes through");
    assert_eq!(mb.hammer_scales(2), [2.0, 1.5, 2.0]);
    let ops = mb.method_calls("edit.batch");
    assert_eq!(
        ops[0].params["ops"][0]["params"]["scales"],
        json!([2.0, 1.5, 2.0])
    );
}

// -------------------------------------------------------------------------------------------
// snapToFloor
// -------------------------------------------------------------------------------------------

/// `snapToFloor` lifts the origin by the model's `get_model_info` floorOffset, scaled by
/// `scales.z`; the applied offset is reported per prop and the op carries the raised origin.
#[tokio::test]
async fn snap_to_floor_lifts_the_origin_by_the_floor_offset_scaled() {
    let tree = Tree::tree_new("snap");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_props",
        json!({"props": [
            // chair floorOffset 2.0, z scale 3 → lift 6
            {"kind": "static", "model": CHAIR, "origin": [0.0, 0.0, 10.0],
             "scales": [2.0, 2.0, 3.0], "snapToFloor": true},
            // no scales → lift 2
            {"kind": "static", "model": CHAIR, "origin": [50.0, 0.0, 4.0],
             "snapToFloor": true}
        ]}),
    )
    .await
    .unwrap();

    let batches = mb.method_calls("edit.batch");
    assert_eq!(
        batches[0].params["ops"][0]["params"]["origin"],
        json!([0.0, 0.0, 16.0]),
        "2.0 floorOffset × 3 = 6 above the given z"
    );
    assert_eq!(
        batches[0].params["ops"][1]["params"]["origin"],
        json!([50.0, 0.0, 6.0])
    );
    let created = out["created"].as_array().expect("created");
    assert_eq!(
        created[0]["floorOffsetApplied"],
        json!(6.0),
        "{}",
        created[0]
    );
    assert_eq!(
        created[1]["floorOffsetApplied"],
        json!(2.0),
        "{}",
        created[1]
    );
}

/// `snapToFloor` on a TILTED prop (review w2b #1, the regression): pitch/roll rotate the
/// render box, so the lift is no longer `placement.floorOffset · scales.z` — it is the lowest
/// of the 8 box corners scaled per axis and rotated by the Source QAngle convention. All
/// numbers are hand-worked in `model_info::tests::snap_floor_lift_matches_the_hand_worked_angles`
/// (the fixture chair `[-10,-10,-2]..[10,10,46]`).
#[tokio::test]
async fn snap_to_floor_lifts_a_tilted_prop_by_the_rotated_box() {
    let tree = Tree::tree_new("snap-rotated");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_props",
        json!({"props": [
            // roll 90°: z_rot = y → the y extent [-10, 10] becomes vertical, lift 10 (the old
            // code answered floorOffset × scales.z = 2, ~8 units into the floor)
            {"kind": "static", "model": CHAIR, "origin": [0.0, 0.0, 10.0],
             "angles": [0.0, 0.0, 90.0], "snapToFloor": true},
            // pitch 180° (upside down): z_rot = -z → bottom -46, lift 46 (old: 2)
            {"kind": "static", "model": CHAIR, "origin": [30.0, 0.0, 10.0],
             "angles": [180.0, 0.0, 0.0], "snapToFloor": true},
            // pitch 90° + non-uniform scale [2,1,1]: z_rot = -x with x ∈ [-20, 20], lift 20
            // (old: scales.z related 2 — the rotated scaled box is what counts)
            {"kind": "static", "model": CHAIR, "origin": [60.0, 0.0, 10.0],
             "angles": [90.0, 0.0, 0.0], "scales": [2.0, 1.0, 1.0], "snapToFloor": true},
            // yaw-only is unchanged: floorOffset 2 × scales.z 3 = 6
            {"kind": "static", "model": CHAIR, "origin": [90.0, 0.0, 10.0],
             "angles": [0.0, 37.0, 0.0], "scales": [2.0, 2.0, 3.0], "snapToFloor": true}
        ]}),
    )
    .await
    .unwrap();

    let batches = mb.method_calls("edit.batch");
    assert_eq!(
        batches[0].params["ops"][0]["params"]["origin"],
        json!([0.0, 0.0, 20.0]),
        "roll 90 lifts the y extent: 10 + 10 = 20"
    );
    assert_eq!(
        batches[0].params["ops"][1]["params"]["origin"],
        json!([30.0, 0.0, 56.0]),
        "upside-down: -46 bottom, 10 + 46 = 56"
    );
    assert_eq!(
        batches[0].params["ops"][2]["params"]["origin"],
        json!([60.0, 0.0, 30.0]),
        "pitch 90 + x-scale 2: -20 bottom, 10 + 20 = 30"
    );
    assert_eq!(
        batches[0].params["ops"][3]["params"]["origin"],
        json!([90.0, 0.0, 16.0]),
        "yaw-only: the old floorOffset × scales.z case, unchanged"
    );
    let created = out["created"].as_array().expect("created");
    assert_eq!(created[0]["floorOffsetApplied"], json!(10.0), "{}", created[0]);
    assert_eq!(created[1]["floorOffsetApplied"], json!(46.0), "{}", created[1]);
    assert_eq!(created[2]["floorOffsetApplied"], json!(20.0), "{}", created[2]);
    assert_eq!(created[3]["floorOffsetApplied"], json!(6.0), "{}", created[3]);
}

/// A model without render bounds has no measurable floor: `snapToFloor` refuses `ModelBounds`
/// (nothing measurable) — never a silent skip.
#[tokio::test]
async fn snap_to_floor_refuses_a_model_without_render_bounds() {
    let tree = Tree::tree_new("snap-nobounds");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "create_props",
        json!({"props": [{"kind": "static",
                          "model": "models/props/fixture/stool.vmdl",
                          "origin": [0, 0, 0], "snapToFloor": true}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "ModelBounds");
    assert_eq!(err.name, "ModelBounds");
    assert!(err.message.contains("no render bounds"), "{}", err.message);
    assert!(mb.method_calls("edit.batch").is_empty());
}

/// `snapToFloor` on an `allowUncompiled`-placed model is refused: there is nothing to measure.
#[tokio::test]
async fn snap_to_floor_refuses_an_uncompiled_model() {
    let tree = Tree::tree_new("snap-uncompiled");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "create_props",
        json!({"allowUncompiled": true,
               "props": [{"kind": "static", "model": UNBUILT,
                          "origin": [0, 0, 0], "snapToFloor": true}]}),
    )
    .await
    .unwrap_err();
    // review w2b #11: the two "nothing to measure" refusals share one Kind (ModelBounds) — the
    // uncompiled model has no compiled placement bounds, exactly like one without render bounds
    assert_eq!(err.type_, "ModelBounds");
    assert!(
        err.message.contains("no compiled .vmdl_c"),
        "{}",
        err.message
    );
    assert!(mb.method_calls("edit.batch").is_empty());
}

/// An undecodable compiled model (the decoy bytes) is a clear `ModelRead` naming the model —
/// the snap pass is what actually opens the bytes here.
#[tokio::test]
async fn a_decoy_model_is_a_model_read_error_not_an_empty_success() {
    let tree = Tree::tree_new("snap-broken");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "create_props",
        json!({"props": [{"kind": "static",
                          "model": "models/props/fixture/broken.vmdl",
                          "origin": [0, 0, 0], "snapToFloor": true}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "ModelRead");
    assert!(
        err.message.contains("broken.vmdl"),
        "the error names the model: {}",
        err.message
    );
    assert!(mb.method_calls("edit.batch").is_empty());
}

// -------------------------------------------------------------------------------------------
// Batch failure taxonomy
// -------------------------------------------------------------------------------------------

/// The mock's partial-failure shape: the FGD tags `prop_rope`, so this server lets both props
/// through, and the mock's `entity.create` registry refuses op 1 — `-32012` with the executed
/// op 0 in `results` (no rollback), not retry-safe, unknown outcome, counter up.
#[tokio::test]
async fn a_partial_failure_passes_through_with_the_counter_bumped() {
    let tree = Tree::tree_new("partial");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "create_props",
        json!({"props": [
            {"kind": "dynamic", "model": CHAIR, "origin": [0, 0, 0]},
            {"class": "prop_rope", "model": CHAIR, "origin": [1, 1, 1]}
        ]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "PartialFailure");
    assert_eq!(err.code, Some(-32012));
    assert_eq!(err.data["failedIndex"], json!(1));
    assert!(
        !err.retry_safe,
        "some ops may have run: verify, never retry"
    );
    assert!(err.outcome_unknown);
    assert_eq!(err.data["requested"], json!(2));
    assert_eq!(err.data["unsavedBridgeEdits"], json!(1));
    let results = err.data["results"].as_array().expect("results");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["index"], json!(0));
    assert_eq!(results[0]["handleIssued"], json!(true));
    assert!(results[0].get("handle").is_none(), "handles stay sanitized");
    assert_eq!(results[1]["error"]["code"], json!(-32006));
    assert!(!err.to_json().to_string().contains("\"handle\""));
    assert_eq!(mb.method_calls("edit.batch").len(), 1);
    assert!(
        node_ids(&mb).contains(&2),
        "op 0 really ran: {:?}",
        node_ids(&mb)
    );
}

/// A wire error the bridge can only answer after running the ops (`-32603`, the mock really
/// applied the create before answering) is `outcomeUnknown`, not retry-safe, and counts the
/// edit: the mesh cache is dropped too.
#[tokio::test]
async fn a_protocol_error_on_the_batch_is_reported_as_maybe_executed() {
    let tree = Tree::tree_new("maybe-executed");
    tree.build_models();
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);
    mb.add_error("edit.batch", -32603, "InternalError", None);

    let err = call(
        &server,
        "create_props",
        json!({"props": [{"kind": "dynamic", "model": CHAIR, "origin": [0, 0, 0]}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "ProtocolError");
    assert_eq!(err.code, Some(-32603));
    assert!(err.outcome_unknown, "the create may have landed: {err:?}");
    assert!(!err.retry_safe);
    assert_eq!(err.data["unsavedBridgeEdits"], json!(1));
    // the op really ran (the mock applies it behind the -32603, like the bridge)
    assert_eq!(mb.hammer_nodes().len(), 2, "{:?}", node_ids(&mb));
    assert_eq!(node_ids(&mb), vec![1, 2]);
}

// -------------------------------------------------------------------------------------------
// The model check of set_entity_keyvalues (WP-R1's second bullet)
// -------------------------------------------------------------------------------------------

/// A `model` keyvalue on a prop-tagged class is catalog-checked before the write: an unknown
/// model is `ModelNotFound` with the catalog's suggestions, nothing written.
#[tokio::test]
async fn set_entity_keyvalues_refuses_an_unknown_model_on_a_prop() {
    let tree = Tree::tree_new("set-unknown-model");
    tree.build_models();
    let mb = hammer_mock(prop_nodes()).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 30, "keyValues": {"model": "models/nope/chair_missing.vmdl"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "ModelNotFound");
    assert_eq!(err.name, "ModelNotFound");
    assert!(
        err.message.contains("chair_missing.vmdl"),
        "{}",
        err.message
    );
    let suggestions = err.data["didYouMean"].as_array().expect("suggestions");
    assert!(
        suggestions.iter().any(|s| s.as_str() == Some(CHAIR)),
        "the same-directory model is suggested: {suggestions:?}"
    );
    // review w2b #3: the refusal says how to bypass the check
    assert!(
        err.message.contains("force:true"),
        "the model-check refusal mentions force: {}",
        err.message
    );
    assert!(mb.method_calls("entity.setKeyValues").is_empty(), "{err:?}");
    // the live values carry the entity's own model again later, unchanged
    assert_eq!(
        keyvalues_of(&mb, 30).get("model").map(String::as_str),
        Some(CHAIR)
    );
}

/// A known, compiled model writes through the normal path (`applied:true`).
#[tokio::test]
async fn set_entity_keyvalues_writes_a_known_model() {
    let tree = Tree::tree_new("set-known-model");
    tree.build_models();
    let mb = hammer_mock(prop_nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"model": CHAIR}}),
    )
    .await
    .unwrap();
    assert_eq!(out["applied"]["model"], json!(true), "{out}");
    assert_eq!(
        keyvalues_of(&mb, 45).get("model").map(String::as_str),
        Some(CHAIR)
    );
}

/// `force:true` skips the model check (the caller accepts an un-placable model spelling).
#[tokio::test]
async fn force_skips_the_model_check() {
    let tree = Tree::tree_new("set-force-model");
    tree.build_models();
    let mb = hammer_mock(prop_nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "force": true,
               "keyValues": {"model": "models/nope/not_a_model.vmdl"}}),
    )
    .await
    .unwrap();
    assert_eq!(out["applied"]["model"], json!(true), "{out}");
    assert_eq!(
        keyvalues_of(&mb, 45).get("model").map(String::as_str),
        Some("models/nope/not_a_model.vmdl")
    );
}

/// Review w2b #3 (the regression): `{"model": ""}` clears a prop's model — a legitimate write
/// (re-props.md Q1's own workaround for a node raw-written by an older bridge) the old code
/// refused with `bad model path: empty model path` and no `force` hint. It writes through, with
/// the note that the check was skipped, and the stored keyvalue is really empty.
#[tokio::test]
async fn set_entity_keyvalues_clears_the_model_with_an_empty_value() {
    let tree = Tree::tree_new("set-empty-model");
    tree.build_models();
    let mb = hammer_mock(prop_nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 30, "keyValues": {"model": ""}}),
    )
    .await
    .unwrap();
    assert_eq!(out["applied"]["model"], json!(true), "{out}");
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("catalog check")),
        "the skipped check is stated, never silent: {warnings:?}"
    );
    // the write really went through with the cleared spelling
    assert_eq!(
        keyvalues_of(&mb, 30).get("model").map(String::as_str),
        Some("")
    );
}

/// The FGD checks a `model` keyvalue on a non-prop class the ordinary way (`studio` type keys
/// accept any text; the tag check is what keeps the model check to props). Review w2b #8: the
/// observation that distinguishes "model check skipped" from "model check ran" is the write
/// GOING THROUGH — drop `strict` (it made the undeclared key a refusal for every class) and keep
/// `build_models()` off (an empty catalog would fail a running model check too).
#[tokio::test]
async fn a_non_prop_class_keeps_the_ordinary_keyvalue_checks() {
    let tree = Tree::tree_new("set-non-prop");

    // the fixture FGD (trigger_multiple has no prop tag): the model check does not run, and an
    // undeclared key rides as an ordinary note/warning, never a refusal
    let mb = hammer_mock(vec![
        MockNode::world(),
        MockNode::entity(60, "trigger_multiple", [0.0, 0.0, 0.0], BTreeMap::new()),
    ])
    .await;
    let server = server(&tree, &mb);
    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 60, "keyValues": {"model": "models/nothing/n.vmdl"}}),
    )
    .await
    .expect("a non-prop class writes through without the model check");
    assert_eq!(out["applied"]["model"], json!(true), "{out}");
    assert!(
        !out.to_string().contains("ModelNotFound"),
        "no model lookup ran against the (empty) catalog: {out}"
    );
    assert_eq!(mb.method_calls("entity.setKeyValues").len(), 1, "{out:?}");
}
