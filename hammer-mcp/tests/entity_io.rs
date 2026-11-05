//! `add_entity_connections` / `remove_entity_connections` tests (WP-R2, bridge m2.18
//! `entity.addConnections` / `entity.removeConnections`): the FGD output/input pre-checks and
//! `force`, the `prop_static` target refusal, the target-not-found note, the runtime-keyword
//! targets, the shared-targetname case, the argument negatives, the bridge's own refusals passed
//! through (duplicate/out-of-range indices, a zero-hit `match`), the capability gate, the exact
//! request JSON, and the m2.18 `connections` array on `get_entity_live` (and that `list_props`'
//! live keyvalues are unaffected).
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockBuilder, MockNode};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

mod common;

const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";

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

fn config(tree: &common::FgdTree, discovery: &Path) -> CommonConfig {
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

fn kv(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A `class:"mesh"` node (not an entity).
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

/// The world the connection tests start from (classes all from `common::FgdTree`'s fixture FGD):
/// nodeID 30 `func_buyzone` (output `BuyzoneEntered`, inputs `Enable`/`Disable`/`Kill` through the
/// base chain), 45 `func_buyzone` named `door`, 50 `prop_dynamic` named `door_prop` (input
/// `SetAnimation`), 55 `prop_static` named `wall` (no inputs at all), 60 `trigger_multiple` named
/// `trig` (output `OnStartTouch`), 70 `info_target` named `twin` (no outputs), 71 `func_buyzone`
/// named `twin` (the shared targetname), 72 a class the FGD does not carry, 9 a mesh.
fn nodes() -> Vec<MockNode> {
    vec![
        MockNode::world(),
        MockNode::entity(
            30,
            "func_buyzone",
            [0.0, 0.0, 0.0],
            kv(&[("targetname", "source_buyzone")]),
        ),
        MockNode::entity(
            45,
            "func_buyzone",
            [1.0, 2.0, 3.0],
            kv(&[("targetname", "door")]),
        ),
        MockNode::entity(
            50,
            "prop_dynamic",
            [4.0, 5.0, 6.0],
            kv(&[
                ("targetname", "door_prop"),
                ("model", "models/mcp_test/probe.vmdl"),
            ]),
        ),
        MockNode::entity(
            55,
            "prop_static",
            [7.0, 8.0, 9.0],
            kv(&[("targetname", "wall")]),
        ),
        MockNode::entity(
            60,
            "trigger_multiple",
            [0.0, 0.0, 0.0],
            kv(&[("targetname", "trig")]),
        ),
        MockNode::entity(
            70,
            "info_target",
            [0.0, 0.0, 0.0],
            kv(&[("targetname", "twin")]),
        ),
        MockNode::entity(
            71,
            "func_buyzone",
            [0.0, 0.0, 0.0],
            kv(&[("targetname", "twin")]),
        ),
        MockNode::entity(
            72,
            "not_in_fgd",
            [0.0, 0.0, 0.0],
            kv(&[("targetname", "odd_target")]),
        ),
        mesh(9),
    ]
}

async fn hammer_mock(world_nodes: Vec<MockNode>) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(world_nodes)
        .start()
        .await
        .unwrap()
}

/// The same world with the `MockBuilder`'s own knobs (the mock's failure seams).
async fn hammer_mock_with(
    world_nodes: Vec<MockNode>,
    tweak: impl FnOnce(MockBuilder) -> MockBuilder,
) -> MockBridge {
    tweak(
        MockBridge::builder()
            .doc(Some(make_doc(DOC_WIN, Some(true))))
            .hammer_nodes(world_nodes),
    )
    .start()
    .await
    .unwrap()
}

fn server(tree: &common::FgdTree, mb: &MockBridge) -> McpServer<HammerState> {
    hammer_mcp::build_server(&config(tree, &mb.discovery_path)).unwrap()
}

/// One `{output, target, input}` connection object.
fn conn(output: &str, target: &str, input: &str) -> Value {
    json!({"output": output, "target": target, "input": input})
}

// -------------------------------------------------------------------------------------------
// Acceptance: add
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn add_writes_the_exact_request_and_reports_the_stored_connections() {
    let tree = common::FgdTree::new("add");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "add_entity_connections",
        json!({
            "nodeID": 60,
            "connections": [
                {"output": "OnStartTouch", "target": "door_prop", "input": "SetAnimation",
                 "param": "open", "delay": 0.5, "timesToFire": -1},
                conn("OnStartTouch", "door", "Enable")
            ]
        }),
    )
    .await
    .unwrap();

    assert_eq!(out["nodeID"], json!(60));
    assert_eq!(out["count"], json!(2));
    assert_eq!(
        out["landed"],
        json!([{"index": 0, "landed": true}, {"index": 1, "landed": true}]),
        "the bridge's positional per-entry outcome passes through"
    );
    assert_eq!(out["unconfirmed"], json!([]));
    assert_eq!(out["undoLabel"], json!("MCP: add connections"));
    assert_eq!(out["classname"], json!("trigger_multiple"));
    assert_eq!(out["forced"], json!(false));
    assert_eq!(out["saved"], json!(false));
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
    assert_eq!(
        out["fgd"]["files"],
        json!(3),
        "csgo.fgd + base_mini.fgd + props_mini.fgd"
    );
    assert_eq!(out["warnings"], json!([]));
    assert!(
        out.get("doc").is_none(),
        "the m2.17 tail is not a result field: {out}"
    );

    let added = out["added"].as_array().expect("added");
    assert_eq!(added.len(), 2);
    // the bridge's own `added` shape (review W2 #2: the mock used to emit
    // output/target/input/param, a shape no live reply ever has)
    assert_eq!(
        added[0],
        json!({"index": 0, "outputName": "OnStartTouch", "targetType": 7,
               "targetName": "door_prop", "inputName": "SetAnimation", "overrideParam": "open",
               "delay": 0.5, "timesToFire": -1})
    );
    // the second connection was sent without param/delay/timesToFire: the store's defaults show
    assert_eq!(
        added[1],
        json!({"index": 1, "outputName": "OnStartTouch", "targetType": 7,
               "targetName": "door", "inputName": "Enable", "overrideParam": "", "delay": 0.0,
               "timesToFire": -1})
    );
    // `count` == the confirmed adds (not the vector's size: that is entity.get's job) and the
    // positional landed[] confirms every wire entry
    assert_eq!(out["addedCount"], json!(2));
    assert_eq!(
        out["landed"],
        json!([{"index": 0, "landed": true}, {"index": 1, "landed": true}])
    );
    assert_eq!(out["unconfirmed"], json!([]));
    assert!(
        out.get("outcomeUnknown").is_none(),
        "a confirmed add is not outcome-unknown: {out}"
    );
    assert_eq!(mb.hammer_connections(60).len(), 2);
    assert_eq!(mb.hammer_connections(60)[0].param, "open");

    // exactly one write, carrying the given members only (no defaults invented on the wire)
    let calls = mb.method_calls("entity.addConnections");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].params["nodeID"], json!(60));
    assert_eq!(
        calls[0].params["connections"][0],
        json!({"output": "OnStartTouch", "target": "door_prop", "input": "SetAnimation",
               "param": "open", "delay": 0.5, "timesToFire": -1})
    );
    assert_eq!(
        calls[0].params["connections"][1],
        json!({"output": "OnStartTouch", "target": "door", "input": "Enable"})
    );
    assert_eq!(calls[0].params["expectDoc"]["id"], json!("d1"));
    assert!(calls[0].params["expectDoc"]["epoch"].is_i64());
    assert!(calls[0].params["deadlineMs"].as_u64().unwrap_or(0) > 0);
    // both targets were resolved in the live document
    let finds: Vec<String> = mb
        .method_calls("entity.find")
        .iter()
        .map(|call| {
            call.params["targetname"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(finds, vec!["door_prop".to_string(), "door".to_string()]);
}

#[tokio::test]
async fn add_checks_the_fgd_output_and_input() {
    let tree = common::FgdTree::new("fgd");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    // a real FGD pair: the trigger's OnStartTouch -> the dynamic prop's SetAnimation
    let out = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [conn("OnStartTouch", "door_prop", "SetAnimation")]}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1));
    // an input declared by the FGD base class is found through the base chain. `count` is the
    // entries THIS call confirmed (review W2 #2: never the vector size after the add), so the
    // second call answers 1 as well while the appended entry sits at live index 1
    let out = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [conn("OnStartTouch", "door", "Kill")]}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1), "{out}");
    assert_eq!(
        out["added"][0]["index"],
        json!(1),
        "the appended entry's live index"
    );
    assert_eq!(mb.hammer_connections(60).len(), 2, "two adds, two entries");

    // the source class does not declare the output
    let err = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 30, "connections": [conn("OnStartTouch", "door", "Enable")]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnknownOutput", "{err:?}");
    assert_eq!(err.code, Some(-32602));
    assert!(err.retry_safe);
    assert_eq!(err.data["class"], json!("func_buyzone"));
    assert_eq!(err.data["valid"], json!(["BuyzoneEntered"]));
    assert!(err.message.contains("BuyzoneEntered"), "{}", err.message);

    // info_target has no outputs at all (only the base's input Kill)
    let err = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 70, "connections": [conn("OnUser1", "door", "Enable")]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnknownOutput", "{err:?}");
    assert_eq!(err.data["valid"], json!([]));
    assert!(err.message.contains("none"), "{}", err.message);

    // the target's class does not declare the input
    let err = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [conn("OnStartTouch", "door", "SetAnimation")]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnknownInput", "{err:?}");
    assert_eq!(err.code, Some(-32602));
    assert!(err.retry_safe);
    assert_eq!(err.data["target"], json!("door"));
    assert_eq!(err.data["classes"][0]["class"], json!("func_buyzone"));
    assert!(
        err.message.contains("Enable") && err.message.contains("Disable"),
        "{}",
        err.message
    );

    // nothing was written by the refused calls
    assert!(mb.hammer_connections(30).is_empty());
    assert!(mb.hammer_connections(70).is_empty());
    assert_eq!(mb.method_calls("entity.addConnections").len(), 2);
}

#[tokio::test]
async fn force_skips_the_fgd_checks_but_never_the_static_prop_refusal() {
    let tree = common::FgdTree::new("force");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 30, "connections": [conn("Nope", "door", "AlsoNope")], "force": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["forced"], json!(true));
    assert_eq!(out["count"], json!(1));
    assert!(
        out["notes"].to_string().contains("force:true"),
        "{}",
        out["notes"]
    );
    assert_eq!(mb.hammer_connections(30).len(), 1);

    // the prop_static refusal is not skippable
    let err = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [conn("OnStartTouch", "wall", "TurnOn")],
               "force": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "StaticPropTarget", "{err:?}");
    assert_eq!(err.code, Some(-32602));
    assert!(err.retry_safe);
    assert_eq!(err.data["staticNodes"][0]["nodeID"], json!(55));
    assert_eq!(err.data["staticNodes"][0]["class"], json!("prop_static"));
    assert!(err.message.contains("prop_static"), "{}", err.message);
    assert!(mb.hammer_connections(60).is_empty());
}

#[tokio::test]
async fn unresolvable_and_keyword_targets_are_allowed_with_a_note() {
    let tree = common::FgdTree::new("targets");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [
            conn("OnStartTouch", "not_built_yet", "Enable"),
            conn("OnStartTouch", "!activator", "Enable"),
            conn("OnStartTouch", "!self", "Kill")
        ]}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(3));
    let notes = out["notes"].to_string();
    assert!(notes.contains("not_built_yet"), "{notes}");
    assert!(notes.contains("not in the live document"), "{notes}");
    assert!(
        notes.contains("!activator") && notes.contains("runtime keyword"),
        "{notes}"
    );
    // a `!` name never triggers a document lookup
    let finds: Vec<String> = mb
        .method_calls("entity.find")
        .iter()
        .map(|call| {
            call.params["targetname"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(finds, vec!["not_built_yet".to_string()]);
}

#[tokio::test]
async fn a_shared_targetname_is_checked_against_every_matching_class() {
    let tree = common::FgdTree::new("twin");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    // `Enable` is declared by the func_buyzone half of the name pair
    let out = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [conn("OnStartTouch", "twin", "Enable")]}),
    )
    .await
    .unwrap();
    let notes = out["notes"].to_string();
    assert!(notes.contains("names 2 live entities"), "{notes}");
    assert!(
        notes.contains("nodeID 70") && notes.contains("nodeID 71"),
        "{notes}"
    );

    // neither class declares this one
    let err = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [conn("OnStartTouch", "twin", "SetAnimation")]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnknownInput", "{err:?}");
    assert_eq!(err.data["classes"].as_array().unwrap().len(), 2);
    assert!(err.message.contains("info_target: Kill"), "{}", err.message);
}

#[tokio::test]
async fn a_target_class_outside_the_fgd_is_a_warning_not_a_refusal() {
    let tree = common::FgdTree::new("unknown-class");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [conn("OnStartTouch", "odd_target", "Whatever")]}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1));
    assert!(
        out["warnings"].to_string().contains("not in the FGD"),
        "{}",
        out["warnings"]
    );
    assert_eq!(mb.hammer_connections(60).len(), 1);
}

#[tokio::test]
async fn an_unavailable_fgd_only_warns() {
    let tree = common::FgdTree::without_fgd("no-fgd");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [conn("OnStartTouch", "wall", "TurnOn")]}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1));
    assert!(
        out["warnings"].to_string().contains("fgd unavailable"),
        "{}",
        out["warnings"]
    );
    assert_eq!(out["fgd"]["files"], json!(0));
    assert!(out["fgd"]["unavailable"].is_string(), "{}", out["fgd"]);
    // a database that cannot prove the class is static cannot refuse the target either
    assert_eq!(mb.hammer_connections(60).len(), 1);
    assert_eq!(
        mb.method_calls("entity.find").len(),
        0,
        "no FGD means no target checks at all"
    );
}

#[tokio::test]
async fn a_mesh_source_is_not_an_entity() {
    let tree = common::FgdTree::new("mesh-source");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 9, "connections": [conn("OnStartTouch", "door", "Enable")]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NotAnEntity", "{err:?}");
    assert_eq!(err.data["nodeID"], json!(9));
    assert!(err.retry_safe);
    assert!(mb.method_calls("entity.addConnections").is_empty());
}

// -------------------------------------------------------------------------------------------
// Negatives: arguments
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn bad_arguments_are_refused_before_anything_is_sent() {
    let tree = common::FgdTree::new("add-negatives");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    for bad in [
        json!({}),
        json!({"nodeID": 60}),
        json!({"connections": [conn("OnStartTouch", "door", "Enable")]}),
        json!({"nodeID": -1, "connections": [conn("o", "t", "i")]}),
        json!({"nodeID": 60, "connections": []}),
        json!({"nodeID": 60, "connections": "x"}),
        json!({"nodeID": 60, "connections": [1]}),
        json!({"nodeID": 60, "connections": [{}]}),
        json!({"nodeID": 60, "connections": [{"output": "o", "target": "t"}]}),
        json!({"nodeID": 60, "connections": [{"output": "", "target": "t", "input": "i"}]}),
        json!({"nodeID": 60, "connections": [{"output": "o", "target": "", "input": "i"}]}),
        json!({"nodeID": 60, "connections": [{"output": "o", "target": "t", "input": null}]}),
        json!({"nodeID": 60, "connections": [{"output": "o", "target": "t", "input": "i",
                                              "delay": -0.5}]}),
        json!({"nodeID": 60, "connections": [{"output": "o", "target": "t", "input": "i",
                                              "delay": 3600.5}]}),
        json!({"nodeID": 60, "connections": [{"output": "o", "target": "t", "input": "i",
                                              "timesToFire": 0}]}),
        json!({"nodeID": 60, "connections": [{"output": "o", "target": "t", "input": "i",
                                              "timesToFire": -2}]}),
        json!({"nodeID": 60, "connections": [{"output": "o", "target": "t", "input": "i",
                                              "timesToFire": 2.5}]}),
        // a typo'd member would silently drop an edit
        json!({"nodeID": 60, "connections": [{"output": "o", "target": "t", "input": "i",
                                              "delayTime": 1}]}),
        // a typo'd top-level argument is refused, not ignored
        json!({"nodeID": 60, "connections": [conn("o", "t", "i")], "strict": true}),
        json!({"nodeID": 60, "connections": [conn("o", "t", "i")], "force": 1}),
    ] {
        let err = call(&server, "add_entity_connections", bad.clone())
            .await
            .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
        assert_eq!(err.name, "InvalidParams", "{bad}");
        assert!(err.retry_safe, "{bad}");
    }
    assert_eq!(mb.hammer_connections(60).len(), 0);
    assert!(mb.method_calls("entity.addConnections").is_empty());
    assert!(mb.method_calls("entity.get").is_empty());
    assert!(mb.method_calls("entity.find").is_empty());
}

/// 64 connections pass the shape gate (and are written in one request); 65 are refused locally.
#[tokio::test]
async fn the_sixty_four_connection_cap() {
    let tree = common::FgdTree::new("cap");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let many: Vec<Value> = (0..=64)
        .map(|_| conn("OnStartTouch", "door", "Enable"))
        .collect();
    let err = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": many}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments", "{err:?}");
    assert!(err.message.contains("at most 64"), "{}", err.message);
    assert!(mb.method_calls("entity.addConnections").is_empty());

    let at_cap: Vec<Value> = (0..64)
        .map(|_| conn("OnStartTouch", "door", "Enable"))
        .collect();
    let out = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": at_cap}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(64));
    assert_eq!(out["added"].as_array().unwrap().len(), 64);
    assert_eq!(mb.hammer_connections(60).len(), 64);
}

#[tokio::test]
async fn long_and_control_character_fields_are_refused() {
    let tree = common::FgdTree::new("add-strings");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let at_cap = "x".repeat(255);
    let too_long = "x".repeat(256);
    for bad in [
        json!({"nodeID": 60, "connections": [{"output": too_long, "target": "t", "input": "i"}]}),
        json!({"nodeID": 60, "connections": [{"output": "o", "target": too_long, "input": "i"}]}),
        json!({"nodeID": 60, "connections": [{"output": "o", "target": "t", "input": too_long}]}),
        json!({"nodeID": 60, "connections": [{"output": "o", "target": "t", "input": "i",
                                              "param": too_long}]}),
        json!({"nodeID": 60, "connections": [{"output": "o\u{0}", "target": "t", "input": "i"}]}),
        json!({"nodeID": 60, "connections": [{"output": "o", "target": "t\n", "input": "i"}]}),
    ] {
        let err = call(&server, "add_entity_connections", bad.clone())
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    // a 255-byte field is accepted (and then refused by the FGD, which proves it reached the check)
    let err = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [{"output": at_cap, "target": "door", "input": "Enable"}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnknownOutput", "{err:?}");
    assert!(mb.method_calls("entity.addConnections").is_empty());
}

#[tokio::test]
async fn a_maybe_executed_add_is_reported_not_passed_through() {
    // review W2 #3: the bridge's read-back could not confirm the second entry (it was handed to
    // AddConnection but the vector did not prove it landed). The old code returned Ok with a bare
    // {"readback":false} in `added` and `count` one short — a model would retry and duplicate the
    // connection. The tool must flag it as outcome-unknown instead.
    let tree = common::FgdTree::new("readback");
    let mb = hammer_mock_with(nodes(), |builder| {
        builder.hammer_drop_conn_readback(Some(1))
    })
    .await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [
            conn("OnStartTouch", "door", "Enable"),
            conn("OnStartTouch", "door_prop", "SetAnimation")
        ]}),
    )
    .await
    .unwrap();

    // the unconfirmed entry is named by its wire position and the result is outcome-unknown
    assert_eq!(out["unconfirmed"], json!([1]), "{out}");
    assert_eq!(out["outcomeUnknown"], json!(true), "{out}");
    assert_eq!(
        out["count"],
        json!(1),
        "the bridge's confirmed-add count: {out}"
    );
    assert_eq!(out["addedCount"], json!(1));
    let notes = out["notes"].to_string();
    assert!(notes.contains("maybe-executed"), "{notes}");
    assert!(notes.contains("do not retry"), "{notes}");
    // the entry itself carries the bridge's marker (never rewritten into a success)
    assert_eq!(out["added"][1]["readback"], json!(false), "{out}");
    assert_eq!(
        out["landed"],
        json!([{"index": 0, "landed": true}, {"index": 1, "landed": false}]),
        "{out}"
    );
    // both entries reached the document (that is the point of "maybe-executed")
    assert_eq!(mb.hammer_connections(60).len(), 2);
    // the edit is counted like any other write
    assert_eq!(out["unsavedBridgeEdits"], json!(1));

    // a fully confirmed add says the opposite (the negative control), on its own world so the
    // helper name is not shadowed by this test's `server` binding
    let confirmed_mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .start()
        .await
        .unwrap();
    let confirmed_server =
        hammer_mcp::build_server(&config(&tree, &confirmed_mb.discovery_path)).unwrap();
    let out = call(
        &confirmed_server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [conn("OnStartTouch", "door", "Enable")]}),
    )
    .await
    .unwrap();
    assert_eq!(out["unconfirmed"], json!([]));
    assert!(out.get("outcomeUnknown").is_none(), "{out}");
}

#[tokio::test]
async fn without_the_bridge_method_the_tools_are_not_implemented() {
    let tree = common::FgdTree::new("add-too-old");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .methods(["control.ping", "doc.active", "entity.get", "entity.find"])
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    for (tool, args, missing) in [
        (
            "add_entity_connections",
            json!({"nodeID": 60, "connections": [conn("OnStartTouch", "door", "Enable")]}),
            "entity.addConnections",
        ),
        (
            "remove_entity_connections",
            json!({"nodeID": 60, "indices": [0]}),
            "entity.removeConnections",
        ),
    ] {
        let err = call(&server, tool, args).await.unwrap_err();
        assert_eq!(err.name, "NotImplemented", "{tool}: {err:?}");
        assert_eq!(err.code, Some(-32011));
        assert_eq!(err.data["missing"], json!([missing]));
        assert!(err.retry_safe);
    }
    assert!(mb.method_calls("entity.addConnections").is_empty());
    assert!(mb.method_calls("entity.removeConnections").is_empty());
}

// -------------------------------------------------------------------------------------------
// Acceptance: remove
// -------------------------------------------------------------------------------------------

/// Three connections on node 60, then the two removal forms.
#[tokio::test]
async fn remove_by_indices_and_by_match() {
    let tree = common::FgdTree::new("remove");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [
            conn("OnStartTouch", "door", "Enable"),
            json!({"output": "OnStartTouch", "target": "door_prop", "input": "SetAnimation",
                   "param": "open"}),
            conn("OnStartTouch", "!activator", "Enable")
        ]}),
    )
    .await
    .unwrap();
    assert_eq!(mb.hammer_connections(60).len(), 3);

    let out = call(
        &server,
        "remove_entity_connections",
        json!({"nodeID": 60, "indices": [2, 0]}),
    )
    .await
    .unwrap();
    assert_eq!(out["nodeID"], json!(60));
    // the removed set comes back ascending; removedCount is the bridge's count (the removals) and
    // `remaining` the bridge's own post-removal count - the two are different fields (review W2
    // #1: reading `count` as the remainder under-reported what the entity holds)
    assert_eq!(out["removed"], json!([0, 2]));
    assert_eq!(out["removedCount"], json!(2));
    assert_eq!(out["remaining"], json!(1));
    assert_eq!(out["undoLabel"], json!("MCP: remove connections"));
    assert_eq!(out["unsavedBridgeEdits"], json!(2), "the add + the remove");
    assert!(out.get("doc").is_none(), "{out}");
    assert!(
        out["notes"].to_string().contains("positional"),
        "{}",
        out["notes"]
    );
    let left = mb.hammer_connections(60);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].input, "SetAnimation");

    let calls = mb.method_calls("entity.removeConnections");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].params["nodeID"], json!(60));
    assert_eq!(calls[0].params["indices"], json!([2, 0]));
    assert_eq!(calls[0].params["expectDoc"]["id"], json!("d1"));
    assert!(
        calls[0].params.get("match").is_none(),
        "{:?}",
        calls[0].params
    );

    // the match form: the one SetAnimation connection is left
    let out = call(
        &server,
        "remove_entity_connections",
        json!({"nodeID": 60, "match": {"output": "OnStartTouch", "input": "SetAnimation"}}),
    )
    .await
    .unwrap();
    assert_eq!(out["removed"], json!([0]));
    assert_eq!(out["remaining"], json!(0));
    assert!(mb.hammer_connections(60).is_empty());
    let calls = mb.method_calls("entity.removeConnections");
    assert_eq!(
        calls[1].params["match"],
        json!({"output": "OnStartTouch", "input": "SetAnimation"})
    );
    assert!(
        calls[1].params.get("indices").is_none(),
        "{:?}",
        calls[1].params
    );
}

#[tokio::test]
async fn remove_passes_the_bridges_own_refusals_through() {
    let tree = common::FgdTree::new("remove-refusals");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [conn("OnStartTouch", "door", "Enable")]}),
    )
    .await
    .unwrap();

    // duplicate indices: the bridge resolves them before opening anything
    let err = call(
        &server,
        "remove_entity_connections",
        json!({"nodeID": 60, "indices": [0, 0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.wire_name.as_deref(), Some("InvalidParams"), "{err:?}");
    assert_eq!(err.code, Some(-32602));
    assert!(!err.outcome_unknown);
    assert!(err.data.get("unsavedBridgeEdits").is_none(), "{err:?}");

    // out of range
    let err = call(
        &server,
        "remove_entity_connections",
        json!({"nodeID": 60, "indices": [7]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert!(
        err.data["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("out of range"),
        "{err:?}"
    );

    // a match with no hit: the bridge's -32026 ConnectionNotFound (m2.18, live)
    let err = call(
        &server,
        "remove_entity_connections",
        json!({"nodeID": 60, "match": {"input": "SetAnimation"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "ConnectionNotFound", "{err:?}");
    assert_eq!(err.code, Some(-32026));
    assert!(err.retry_safe);
    assert_eq!(err.data["nodeID"], json!(60));
    assert!(
        err.message.contains("nothing was removed"),
        "{}",
        err.message
    );

    // a mesh is not an entity (the bridge's own class gate)
    let err = call(
        &server,
        "remove_entity_connections",
        json!({"nodeID": 9, "indices": [0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(err.data["reason"], json!("not an entity"), "{err:?}");

    // every refusal ran nothing
    assert_eq!(mb.hammer_connections(60).len(), 1);
    assert_eq!(
        mb.method_calls("entity.removeConnections").len(),
        4,
        "the four selector calls all reached the bridge and were refused there"
    );
    assert_eq!(
        mb.hammer_connections(60)[0].input,
        "Enable",
        "the surviving connection is the one the adds wrote"
    );
}

/// Review W2 #1's regression: 5 connections, remove 3 -> `remaining` is 2, not the removed count.
/// The old code read the bridge's `count` (the number removed) as the remainder, so it answered 3
/// here and 0 after removing the last one; the live run could not distinguish it (2 connections,
/// remove 1: removed == remaining). The mock used to answer the other semantics, which is what let
/// the wrong reading pass its tests.
#[tokio::test]
async fn remove_reports_the_remaining_count_not_the_removed_one() {
    let tree = common::FgdTree::new("remove-remaining");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let five: Vec<Value> = (0..5)
        .map(|i| conn("OnStartTouch", &format!("door{i}"), "Enable"))
        .collect();
    call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": five, "force": true}),
    )
    .await
    .unwrap();
    assert_eq!(mb.hammer_connections(60).len(), 5);

    let out = call(
        &server,
        "remove_entity_connections",
        json!({"nodeID": 60, "indices": [0, 1, 2]}),
    )
    .await
    .unwrap();
    assert_eq!(out["removed"], json!([0, 1, 2]));
    assert_eq!(out["removedCount"], json!(3), "{out}");
    assert_eq!(
        out["remaining"],
        json!(2),
        "2 connections are left after removing 3 of 5: {out}"
    );
    // the mock's stored vector agrees with the reported remainder (the assertion above is the
    // tool's reading of the bridge's own `remaining`, this one is the document)
    assert_eq!(mb.hammer_connections(60).len(), 2);

    // and the wire carries both numbers, so a reader can tell them apart
    let params = &mb.method_calls("entity.removeConnections")[0].params;
    assert_eq!(params["indices"], json!([0, 1, 2]));

    // removing the last two: `remaining` is 0 and `count`/`removedCount` is 2
    let out = call(
        &server,
        "remove_entity_connections",
        json!({"nodeID": 60, "match": {"input": "Enable"}}),
    )
    .await
    .unwrap();
    assert_eq!(out["removed"].as_array().map(Vec::len), Some(2));
    assert_eq!(out["remaining"], json!(0), "{out}");
    assert!(mb.hammer_connections(60).is_empty());
}
#[tokio::test]
async fn remove_arguments_are_validated_before_anything_is_sent() {
    let tree = common::FgdTree::new("remove-args");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    for bad in [
        json!({}),
        json!({"nodeID": 60}),
        json!({"indices": [0]}),
        // exactly one form
        json!({"nodeID": 60, "indices": [0], "match": {"output": "o"}}),
        json!({"nodeID": 60, "indices": null, "match": null}),
        // indices shapes
        json!({"nodeID": 60, "indices": []}),
        json!({"nodeID": 60, "indices": "0"}),
        json!({"nodeID": 60, "indices": [-1]}),
        json!({"nodeID": 60, "indices": [1.5]}),
        json!({"nodeID": 60, "indices": ["0"]}),
        // review W2 #8: the tool-side cap is the bridge's 512, not the 64 of `connections` — 65
        // indices are accepted locally (and refused by the bridge when the entity has fewer)
        json!({"nodeID": 60, "indices": (0..513).collect::<Vec<i64>>()}),
        // match shapes
        json!({"nodeID": 60, "match": {}}),
        json!({"nodeID": 60, "match": "output"}),
        json!({"nodeID": 60, "match": {"targets": "door"}}),
        json!({"nodeID": 60, "match": {"output": ""}}),
        json!({"nodeID": 60, "match": {"output": 1}}),
        json!({"nodeID": 60, "match": {"output": "o\u{0}"}}),
        // a typo'd top-level argument
        json!({"nodeID": 60, "index": [0]}),
    ] {
        let err = call(&server, "remove_entity_connections", bad.clone())
            .await
            .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
        assert_eq!(err.name, "InvalidParams", "{bad}");
        assert!(err.retry_safe, "{bad}");
    }
    let too_long = "x".repeat(256);
    let err = call(
        &server,
        "remove_entity_connections",
        json!({"nodeID": 60, "match": {"output": too_long}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(mb.method_calls("entity.removeConnections").is_empty());
}

// -------------------------------------------------------------------------------------------
// The m2.18 `connections` array on the live read (and the neighbours it must not disturb)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn get_entity_live_carries_the_live_connections() {
    let tree = common::FgdTree::new("read");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    // an entity without connections answers an empty array (not null)
    let out = call(&server, "get_entity_live", json!({"nodeID": 45}))
        .await
        .unwrap();
    assert_eq!(out["connections"], json!([]), "{out}");

    call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [
            {"output": "OnStartTouch", "target": "door_prop", "input": "SetAnimation",
             "param": "open", "delay": 0.5, "timesToFire": 2}
        ]}),
    )
    .await
    .unwrap();

    let out = call(&server, "get_entity_live", json!({"nodeID": 60}))
        .await
        .unwrap();
    assert_eq!(
        out["connections"],
        json!([{
            "outputName": "OnStartTouch",
            "targetType": 7,
            "targetName": "door_prop",
            "inputName": "SetAnimation",
            "overrideParam": "open",
            "delay": 0.5,
            "timesToFire": 2
        }]),
        "the .vmap reader's names, targetType 7 (re-props.md Q3)"
    );
    assert_eq!(
        out["keyValues"],
        json!({"targetname": "trig"}),
        "the read is unchanged otherwise"
    );

    // the file route's own shape is untouched (its `connections` come from the saved file)
    let note = out["note"].as_str().expect("note");
    assert!(note.contains("get_entity"), "{note}");
    assert!(note.contains("connections"), "{note}");
}

#[tokio::test]
async fn list_props_live_keyvalues_are_unaffected() {
    let tree = common::FgdTree::new("list-props");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "list_props",
        json!({"source": "live", "kind": "dynamic", "includeKeyValues": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["total"], json!(1), "{out}");
    let prop = &out["props"][0];
    assert_eq!(prop["nodeID"], json!(50));
    assert_eq!(prop["classname"], json!("prop_dynamic"));
    assert_eq!(prop["kind"], json!("dynamic"));
    assert_eq!(
        prop["keyvalues"]["model"],
        json!("models/mcp_test/probe.vmdl"),
        "the live keyvalues still come through"
    );
    assert_eq!(prop["keyvalues"]["targetname"], json!("door_prop"));
}

// -------------------------------------------------------------------------------------------
// undo (the mock's snapshot must restore the connections, like Hammer does)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn undo_last_restores_the_connection_vector() {
    let tree = common::FgdTree::new("undo");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    call(
        &server,
        "add_entity_connections",
        json!({"nodeID": 60, "connections": [conn("OnStartTouch", "door", "Enable")]}),
    )
    .await
    .unwrap();
    assert_eq!(mb.hammer_connections(60).len(), 1);

    let out = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(out["undone"], json!(true));
    assert_eq!(out["undoName"], json!("MCP: add connections"));
    assert!(
        mb.hammer_connections(60).is_empty(),
        "the snapshot restores the vector"
    );
}
