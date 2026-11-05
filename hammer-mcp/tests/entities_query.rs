//! `entities_query` tool tests (WP-T2b): `get_entity_live` (`entity.get`) and
//! `find_entities_live` (`entity.find`) against the mock bridge's Hammer world (WP-M built-ins).
//!
//! Helpers (`Tree`, `config`, `call`) are local copies of the pattern in `tests/entities_file.rs`;
//! the mock world is built with `MockNode::world()` / `MockNode::entity(..)` plus a hand-built
//! `class:"mesh"` node for the not-an-entity refusal.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{MockBridge, MockNode};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "entities-query-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("tmp tree");
        Tree { root }
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
    let discovery = discovery.display().to_string();
    let given: Vec<&str> = vec![
        "--deadline-ms",
        "5000",
        "--discovery",
        &discovery,
        "--project-root",
        &root,
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

fn kv(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A `class:"mesh"` node: not an entity, the `-32602 not an entity` case.
fn mesh(id: i64, origin: [f64; 3]) -> MockNode {
    MockNode {
        node_id: id,
        class: "mesh".into(),
        classname: String::new(),
        targetname: None,
        origin,
        angles: [0.0; 3],
        keyvalues: BTreeMap::new(),
        locked: false,
        parent: None,
    }
}

/// The mock world: world + 30 (info_map_parameters) + 45 (light_omni2) + 67/70 (func_buyzone) +
/// mesh 9, mirroring the probe-map classes the plan's live steps use.
async fn world() -> MockBridge {
    MockBridge::builder()
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(
                30,
                "info_map_parameters",
                [1.0, 2.0, 3.0],
                kv(&[("bombradius", "500"), ("buying", "0")]),
            ),
            MockNode::entity(45, "light_omni2", [500.0, 1000.0, 0.0], kv(&[("_light", "255 255 255 200")])),
            MockNode::entity(67, "func_buyzone", [100.0, 0.0, 0.0], kv(&[("targetname", "bz_a")])),
            MockNode::entity(70, "func_buyzone", [200.0, 0.0, 0.0], kv(&[("targetname", "bz_b")])),
            mesh(9, [10.0, 0.0, 0.0]),
        ])
        .start()
        .await
        .unwrap()
}

fn found_ids(out: &Value) -> Vec<i64> {
    let mut ids: Vec<i64> = out["entities"]
        .as_array()
        .expect("entities array")
        .iter()
        .map(|entity| entity["nodeID"].as_i64().expect("nodeID"))
        .collect();
    ids.sort();
    ids
}

// -------------------------------------------------------------------------------------------
// Acceptance
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn get_entity_live_passes_the_live_entity_through_with_a_note() {
    let tree = Tree::new("get");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let out = call(&server, "get_entity_live", json!({"nodeID": 30}))
        .await
        .unwrap();
    assert_eq!(out["nodeID"], json!(30));
    assert_eq!(out["classname"], json!("info_map_parameters"));
    assert_eq!(out["origin"], json!([1.0, 2.0, 3.0]));
    assert_eq!(out["angles"], json!([0.0, 0.0, 0.0]));
    assert_eq!(out["keyValues"]["bombradius"], json!("500"));
    assert_eq!(out["keyValues"]["buying"], json!("0"));
    assert_eq!(out["keyValueCount"], json!(2));
    assert_eq!(out["unsupported"], json!([]));
    // m2.18: the live connection vector reaches the result (an entity without any is an empty
    // array, not the old hard-coded null — see tests/entity_io.rs for the populated shape)
    assert_eq!(out["connections"], json!([]), "the wire reports the live vector");
    assert_eq!(out["source"], json!("live"));
    let note = out["note"].as_str().expect("note");
    assert!(note.contains("get_entity"), "points at the file route: {note}");
    assert!(note.contains("connections") || note.contains("I/O"), "{note}");

    let calls = mb.method_calls("entity.get");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].params["nodeID"], json!(30));
    assert!(calls[0].params["deadlineMs"].as_u64().unwrap_or(0) > 0);
}

#[tokio::test]
async fn find_entities_live_filters_by_class_targetname_and_sphere() {
    let tree = Tree::new("find");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    // no filter: everything (entities only — the world and the mesh are not entities)
    let out = call(&server, "find_entities_live", json!({})).await.unwrap();
    assert_eq!(found_ids(&out), vec![30, 45, 67, 70]);
    assert_eq!(out["count"], json!(4));
    assert_eq!(out["visited"], json!(6), "every node walked");
    assert_eq!(out["truncated"], json!(false));
    assert_eq!(out["source"], json!("live"));
    // absent filters are not sent as nulls
    let params = mb.method_calls("entity.find")[0].params.clone();
    assert!(params.get("class").is_none() && params.get("targetname").is_none());
    assert!(params.get("within").is_none() && params.get("limit").is_none());

    // class: exact, case-insensitive
    let out = call(&server, "find_entities_live", json!({"class": "FUNC_BUYZONE"}))
        .await
        .unwrap();
    assert_eq!(found_ids(&out), vec![67, 70]);
    assert_eq!(out["count"], json!(2));

    // targetname: exact, case-insensitive
    let out = call(&server, "find_entities_live", json!({"targetname": "BZ_A"}))
        .await
        .unwrap();
    assert_eq!(found_ids(&out), vec![67]);
    assert_eq!(out["entities"][0]["classname"], json!("func_buyzone"));

    // within: sphere on the origin (only 67 sits within 50 of [100,0,0])
    let out = call(
        &server,
        "find_entities_live",
        json!({"within": {"origin": [100.0, 0.0, 0.0], "radius": 50.0}}),
    )
    .await
    .unwrap();
    assert_eq!(found_ids(&out), vec![67]);
    let sent = mb.method_calls("entity.find").last().unwrap().params.clone();
    assert_eq!(sent["within"]["origin"], json!([100.0, 0.0, 0.0]));
    assert_eq!(sent["within"]["radius"], json!(50.0));

    // limit: truncates and says so; the limit is forwarded
    let out = call(&server, "find_entities_live", json!({"limit": 1}))
        .await
        .unwrap();
    assert_eq!(out["count"], json!(1));
    assert_eq!(out["truncated"], json!(true));
    assert_eq!(
        mb.method_calls("entity.find").last().unwrap().params["limit"],
        json!(1)
    );
}

// -------------------------------------------------------------------------------------------
// Required negatives
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn get_entity_live_on_a_mesh_is_not_an_entity() {
    let tree = Tree::new("mesh");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let err = call(&server, "get_entity_live", json!({"nodeID": 9}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "NotAnEntity");
    assert_eq!(err.name, "NotAnEntity");
    assert_eq!(err.code, Some(-32602));
    assert_eq!(err.data["nodeID"], json!(9));
    assert!(err.retry_safe, "nothing was executed");
    // the bridge was asked: this refusal is its answer, not a local guess
    assert_eq!(mb.method_calls("entity.get").len(), 1);
}

#[tokio::test]
async fn get_entity_live_without_a_node_id_is_invalid_arguments_and_sends_nothing() {
    let tree = Tree::new("get-args");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    for bad in [
        json!({}),
        json!({"nodeID": null}),
        json!({"nodeID": -1}),
        json!({"nodeID": 1.5}),
        json!({"nodeID": "30"}),
    ] {
        let err = call(&server, "get_entity_live", bad.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
        assert_eq!(err.name, "InvalidParams", "{bad}");
        assert!(err.retry_safe, "{bad}");
    }
    assert!(
        mb.method_calls("entity.get").is_empty(),
        "local validation refuses before dispatch"
    );
}

#[tokio::test]
async fn get_entity_live_on_an_unknown_node_is_entity_not_found() {
    let tree = Tree::new("not-found");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let err = call(&server, "get_entity_live", json!({"nodeID": 999999}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "EntityNotFound");
    assert_eq!(err.name, "EntityNotFound");
    assert_eq!(err.code, Some(-32022));
    assert_eq!(err.data["nodeID"], json!(999999));
    assert!(err.retry_safe);
}

#[tokio::test]
async fn find_entities_live_with_bad_filters_is_invalid_arguments_and_sends_nothing() {
    let tree = Tree::new("find-args");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    for bad in [
        json!({"within": {"origin": [1.0, 2.0], "radius": 5.0}}),
        json!({"within": {"origin": [1.0, 2.0, 3.0], "radius": 0.0}}),
        json!({"within": {"origin": [1.0, 2.0, 3.0], "radius": -1.0}}),
        json!({"within": {"origin": [1.0, 2.0, "x"], "radius": 5.0}}),
        json!({"within": "here"}),
        json!({"limit": 0}),
        json!({"limit": 2001}),
        json!({"limit": "500"}),
        json!({"class": 3}),
        // an empty filter would list the whole document: refused (brief item 13)
        json!({"class": ""}),
        json!({"targetname": ""}),
        // outside the range the bridge reads exactly: `1e20` serializes in exponent notation
        json!({"within": {"origin": [1e20, 0.0, 0.0], "radius": 5.0}}),
    ] {
        let err = call(&server, "find_entities_live", bad.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
        assert!(err.retry_safe, "{bad}");
    }
    assert!(
        mb.method_calls("entity.find").is_empty(),
        "local validation refuses before dispatch"
    );
}

/// Brief item 5: a nodeID above the bridge's `int` range must not reach it (it would truncate to a
/// different node and read the wrong entity).
#[tokio::test]
async fn get_entity_live_refuses_a_node_id_above_the_int_range() {
    let tree = Tree::new("nodeid-range");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    for bad in [json!(2_147_483_648i64), json!(4_294_967_298i64)] {
        let err = call(&server, "get_entity_live", json!({"nodeID": bad}))
            .await
            .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
        assert_eq!(err.name, "InvalidParams", "{bad}");
        assert!(err.message.contains("2147483647"), "{}", err.message);
    }
    // the highest legal id still goes out
    let out = call(&server, "get_entity_live", json!({"nodeID": 2_147_483_647}))
        .await
        .unwrap_err();
    assert_eq!(out.name, "EntityNotFound", "the bound itself is allowed through");
    assert_eq!(mb.method_calls("entity.get").len(), 1);
}

/// Brief item 7: a coordinate below 1e-4 is snapped to 0 before it is serialized (serde_json would
/// write `5.5e-17`, which the bridge's number scan stops at) and nothing in the frame is in
/// exponent notation.
#[tokio::test]
async fn sub_1e_4_coordinates_are_snapped_and_never_sent_in_exponent_form() {
    let tree = Tree::new("exponent");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let out = call(
        &server,
        "find_entities_live",
        json!({"within": {"origin": [100.0, 5.5e-17, 0.0], "radius": 50.0}}),
    )
    .await
    .unwrap();
    assert_eq!(found_ids(&out), vec![67]);
    let sent = mb.method_calls("entity.find").last().unwrap().params.clone();
    assert_eq!(sent["within"]["origin"], json!([100.0, 0.0, 0.0]));
    let within_text = sent["within"].to_string();
    assert!(
        !within_text.contains('e') && !within_text.contains('E'),
        "the sphere must carry no exponent-form number: {within_text}"
    );
}
