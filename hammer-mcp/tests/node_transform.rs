//! `set_node_transform` tests (WP-R2, bridge m2.18 `node.setTransform`): validation negatives,
//! the entity and mesh happy paths, relative mode (including the fields the read cannot supply),
//! the not-found passthrough, the capability gate, the exact request JSON, and the two
//! follow-up reads (after a successful transform, or a maybe-executed failure, the next mesh
//! operator reads the live geometry again: nothing is remembered between calls).
//!
//! The mock's Hammer world serves `node.setTransform` and `entity.get` (WP-R2's mock handlers);
//! the FGD comes from `common::FgdTree` (the fixture plus `props_mini.fgd`), which is what makes
//! the `prop_static` compile note testable.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
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

/// A `class:"mesh"` node (the mock's geometry world owns the ids it creates; a hand-made node is
/// enough for the transform tests).
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

/// nodeID 30 `prop_static` (the compile note), 45 `prop_dynamic`, 60 `func_buyzone`, 9 a mesh.
fn nodes() -> Vec<MockNode> {
    vec![
        MockNode::world(),
        MockNode::entity(
            30,
            "prop_static",
            [1.0, 2.0, 3.0],
            kv(&[("targetname", "wall")]),
        ),
        MockNode::entity(
            45,
            "prop_dynamic",
            [4.0, 5.0, 6.0],
            kv(&[("targetname", "door")]),
        ),
        MockNode::entity(60, "func_buyzone", [7.0, 8.0, 9.0], kv(&[("TeamNum", "2")])),
        mesh(9, [100.0, 0.0, 0.0]),
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

fn server(tree: &common::FgdTree, mb: &MockBridge) -> McpServer<HammerState> {
    hammer_mcp::build_server(&config(tree, &mb.discovery_path)).unwrap()
}

fn node_of(mb: &MockBridge, id: i64) -> MockNode {
    mb.hammer_nodes()
        .into_iter()
        .find(|node| node.node_id == id)
        .unwrap_or_else(|| panic!("node {id}"))
}

// -------------------------------------------------------------------------------------------
// Acceptance
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_partial_absolute_transform_writes_one_exact_request() {
    let tree = common::FgdTree::new("abs");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 45, "origin": [10, 20, 30]}),
    )
    .await
    .unwrap();

    assert_eq!(out["nodeID"], json!(45));
    assert_eq!(out["class"], json!("entity"));
    assert_eq!(out["origin"], json!([10.0, 20.0, 30.0]));
    // the members the caller did not ask for are the live values the bridge read back
    assert_eq!(out["angles"], json!([0.0, 0.0, 0.0]));
    assert_eq!(out["scales"], json!([1.0, 1.0, 1.0]));
    assert_eq!(out["classname"], json!("prop_dynamic"));
    assert_eq!(out["undoLabel"], json!("MCP: set transform"));
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
    assert_eq!(out["saved"], json!(false));
    assert!(
        out.get("doc").is_none(),
        "the m2.17 tail is not a result field: {out}"
    );
    assert!(
        out.get("previous").is_none(),
        "absolute mode invents no `previous`"
    );
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("save_map")),
        "{notes:?}"
    );
    // prop_dynamic is not a compile-time prop: no compile note
    assert!(
        !out["notes"].to_string().contains("compile_map"),
        "{}",
        out["notes"]
    );

    // the mock world really moved
    assert_eq!(node_of(&mb, 45).origin, [10.0, 20.0, 30.0]);

    // the read first (`entity.get`), then exactly one `node.setTransform` with the given member and
    // nothing else
    assert_eq!(mb.method_calls("entity.get").len(), 1);
    let calls = mb.method_calls("node.setTransform");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].params["nodeID"], json!(45));
    assert_eq!(calls[0].params["origin"], json!([10.0, 20.0, 30.0]));
    assert!(
        calls[0].params.get("angles").is_none(),
        "{:?}",
        calls[0].params
    );
    assert!(
        calls[0].params.get("scales").is_none(),
        "{:?}",
        calls[0].params
    );
    assert_eq!(calls[0].params["expectDoc"]["id"], json!("d1"));
    assert!(calls[0].params["expectDoc"]["epoch"].is_i64());
    assert!(calls[0].params["deadlineMs"].as_u64().unwrap_or(0) > 0);
}

#[tokio::test]
async fn a_mutator_points_at_save_map() {
    let tree = common::FgdTree::new("next");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 45, "origin": [1, 2, 3]}),
    )
    .await
    .unwrap();
    assert_eq!(out["next"][0]["tool"], json!("save_map"), "{out}");
    assert!(out["next"][0]["why"].is_string());
}

#[tokio::test]
async fn a_static_prop_move_carries_the_compile_note() {
    let tree = common::FgdTree::new("static-note");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 30, "scales": [2.0, 2.0, 2.0]}),
    )
    .await
    .unwrap();
    assert_eq!(out["classname"], json!("prop_static"));
    assert_eq!(out["scales"], json!([2.0, 2.0, 2.0]));
    assert_eq!(mb.hammer_scales(30), [2.0, 2.0, 2.0]);
    assert!(
        out["notes"]
            .to_string()
            .contains("compiled into the world (resourcecompiler bakes it)"),
        "{}",
        out["notes"]
    );
    assert!(
        out["notes"].to_string().contains("compile_map"),
        "{}",
        out["notes"]
    );
}

#[tokio::test]
async fn a_mesh_transform_notes_the_unreadable_transform_and_writes() {
    let tree = common::FgdTree::new("mesh");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 9, "origin": [1, 2, 3], "angles": [0, 45, 0]}),
    )
    .await
    .unwrap();
    assert_eq!(out["class"], json!("mesh"));
    assert_eq!(out["origin"], json!([1.0, 2.0, 3.0]));
    assert_eq!(out["angles"], json!([0.0, 45.0, 0.0]));
    assert_eq!(out["scales"], json!([1.0, 1.0, 1.0]));
    // the node is not an entity: the read's refusal is expected and named in the notes
    assert!(
        out["notes"].to_string().contains("not an entity"),
        "{}",
        out["notes"]
    );
    assert!(
        out.get("classname").is_none(),
        "a mesh has no FGD class here: {out}"
    );
    assert_eq!(node_of(&mb, 9).origin, [1.0, 2.0, 3.0]);
    assert!(mb.method_calls("node.setTransform").len() == 1);
}

#[tokio::test]
async fn relative_mode_adds_deltas_to_the_read_values() {
    let tree = common::FgdTree::new("relative");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 60, "origin": [1, 2, 3], "angles": [0, 10, 0], "relative": true}),
    )
    .await
    .unwrap();

    // 7,8,9 + 1,2,3 and 0,0,0 + 0,10,0 — the composed absolute values reach the bridge
    let calls = mb.method_calls("node.setTransform");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].params["origin"], json!([8.0, 10.0, 12.0]));
    assert_eq!(calls[0].params["angles"], json!([0.0, 10.0, 0.0]));
    assert_eq!(out["origin"], json!([8.0, 10.0, 12.0]));
    assert_eq!(out["angles"], json!([0.0, 10.0, 0.0]));
    // `previous` reports what the tool read before the write
    assert_eq!(out["previous"]["origin"], json!([7.0, 8.0, 9.0]));
    assert_eq!(out["previous"]["angles"], json!([0.0, 0.0, 0.0]));
    assert_eq!(
        out["previous"]["scales"],
        json!([1.0, 1.0, 1.0]),
        "the m2.18 read reports the node's scales (review W2 #4); node 60 never had any written, \
         so they are the identity"
    );
    assert_eq!(node_of(&mb, 60).origin, [8.0, 10.0, 12.0]);
}

#[tokio::test]
async fn relative_scales_use_a_read_that_reports_them() {
    // the m2.18 entity.get reports the node scales (review W2 #4: the mock's read omitted the
    // field, so this test had to hand-roll a responder): the tool multiplies with what the read
    // carries
    let tree = common::FgdTree::new("relative-scales");
    let mb = hammer_mock(nodes()).await;
    // the state a scaled create/transform leaves behind: node 45 carries [2,2,2]
    mb.set_hammer_scales(45, [2.0, 2.0, 2.0]);
    let server = server(&tree, &mb);

    // the mock's own read reports the field (the shape the tool consumes)
    let read = call(&server, "get_entity_live", json!({"nodeID": 45}))
        .await
        .unwrap();
    assert_eq!(read["scales"], json!([2.0, 2.0, 2.0]), "{read}");

    let out = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 45, "scales": [2.0, 1.0, 0.5], "relative": true}),
    )
    .await
    .unwrap();
    let params = &mb.method_calls("node.setTransform")[0].params;
    assert_eq!(params["scales"], json!([4.0, 2.0, 1.0]));
    assert_eq!(out["previous"]["scales"], json!([2.0, 2.0, 2.0]));
    assert_eq!(out["scales"], json!([4.0, 2.0, 1.0]));
}

// -------------------------------------------------------------------------------------------
// Negatives
// -------------------------------------------------------------------------------------------

/// Every local refusal: `InvalidArguments`, nothing dispatched (not even the `entity.get` read),
/// so the mock's world and undo stack are untouched.
#[tokio::test]
async fn bad_arguments_are_refused_before_anything_is_sent() {
    let tree = common::FgdTree::new("negatives");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    for bad in [
        // no member at all
        json!({"nodeID": 45}),
        json!({"nodeID": 45, "origin": null, "angles": null, "scales": null}),
        // nodeID shapes
        json!({}),
        json!({"nodeID": -1, "origin": [0, 0, 0]}),
        json!({"nodeID": 1.5, "origin": [0, 0, 0]}),
        json!({"nodeID": 2_147_483_648i64, "origin": [0, 0, 0]}),
        // member shapes
        json!({"nodeID": 45, "origin": [1, 2]}),
        json!({"nodeID": 45, "origin": [1, 2, "3"]}),
        json!({"nodeID": 45, "origin": "1 2 3"}),
        json!({"nodeID": 45, "angles": [1, 2, 3, 4]}),
        // finite in f64 but outside the bridge's 32-bit float range
        json!({"nodeID": 45, "angles": [1e39, 0, 0]}),
        // the bridge's own ranges
        json!({"nodeID": 45, "origin": [1048576.5, 0, 0]}),
        json!({"nodeID": 45, "origin": [0, -2000000, 0]}),
        json!({"nodeID": 45, "scales": [0, 1, 1]}),
        json!({"nodeID": 45, "scales": [-1, 1, 1]}),
        json!({"nodeID": 45, "scales": [1, 1, 1000.5]}),
        // relative must be a boolean
        json!({"nodeID": 45, "origin": [0, 0, 0], "relative": 1}),
        // a typo'd argument is not silently ignored
        json!({"nodeID": 45, "origin": [0, 0, 0], "rotate": [0, 90, 0]}),
        json!({"nodeID": 45, "scale": [2, 2, 2]}),
    ] {
        let err = call(&server, "set_node_transform", bad.clone())
            .await
            .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
        assert_eq!(err.name, "InvalidParams", "{bad}");
        assert!(err.retry_safe, "{bad}");
    }
    assert!(mb.method_calls("node.setTransform").is_empty());
    assert!(mb.method_calls("entity.get").is_empty());
    assert_eq!(node_of(&mb, 45).origin, [4.0, 5.0, 6.0]);
}

/// A relative field the first read cannot supply is refused with nothing sent — never composed
/// from an invented base.
#[tokio::test]
async fn relative_fields_without_a_readable_base_are_refused() {
    let tree = common::FgdTree::new("relative-refusals");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    // scales on a bridge whose entity.get does not carry the field (older than m2.18, or a field
    // that did not read): the read has nothing to multiply with, so the call is refused. The
    // m2.18 read DOES report scales (review W2 #4) — that path is
    // `relative_scales_use_a_read_that_reports_them`.
    let no_scales = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .responder("entity.get", |_: &Value| {
            json!({
                "nodeID": 45,
                "classname": "prop_dynamic",
                "origin": [10.0, 20.0, 30.0],
                "angles": [0.0, 90.0, 0.0],
                "keyValues": {},
            })
        })
        .start()
        .await
        .unwrap();
    let no_scales_server =
        hammer_mcp::build_server(&config(&tree, &no_scales.discovery_path)).unwrap();
    let err = call(
        &no_scales_server,
        "set_node_transform",
        json!({"nodeID": 45, "scales": [2, 2, 2], "relative": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(err.message.contains("relative scales"), "{}", err.message);
    assert!(err.message.contains("scales"), "{}", err.message);
    assert!(
        no_scales.method_calls("node.setTransform").is_empty(),
        "nothing was sent"
    );

    // the plain mock reports scales now: a relative scales call composes instead of refusing
    // (the negative of the case above)
    let out = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 45, "scales": [2, 2, 2], "relative": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["previous"]["scales"], json!([1.0, 1.0, 1.0]), "{out}");
    assert_eq!(out["scales"], json!([2.0, 2.0, 2.0]), "{out}");

    // a mesh node: no readable transform at all
    for field in [
        json!({"origin": [1, 0, 0]}),
        json!({"angles": [0, 10, 0]}),
        json!({"scales": [2, 2, 2]}),
    ] {
        let mut args = json!({"nodeID": 9, "relative": true});
        args.as_object_mut()
            .unwrap()
            .extend(field.as_object().unwrap().clone());
        let err = call(&server, "set_node_transform", args.clone())
            .await
            .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{args}");
        assert!(
            err.message.contains("relative") && err.message.contains("not implemented"),
            "{args}: {}",
            err.message
        );
    }
    // one call reached the bridge in total: the plain mock's composed relative transform (the
    // refusals - the scales-less bridge above and every mesh field - sent nothing)
    let calls = mb.method_calls("node.setTransform");
    assert_eq!(
        calls.len(),
        1,
        "only the composing call was sent: {calls:?}"
    );
    assert_eq!(calls[0].params["nodeID"], json!(45));
    assert_eq!(calls[0].params["scales"], json!([2.0, 2.0, 2.0]));
    assert_eq!(mb.hammer_scales(45), [2.0, 2.0, 2.0]);
}

/// A composed value its own gate would refuse is refused locally (the delta, not the base, is at
/// fault).
#[tokio::test]
async fn relative_composition_respects_the_bridge_ranges() {
    let tree = common::FgdTree::new("relative-ranges");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .responder("entity.get", |_: &Value| {
            json!({
                "nodeID": 45, "classname": "prop_dynamic",
                "origin": [1048575.0, 0.0, 0.0], "angles": [0.0, 0.0, 0.0],
                "scales": [1000.0, 1.0, 1.0], "keyValues": {},
            })
        })
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 45, "origin": [2, 0, 0], "relative": true}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("outside"), "{}", err.message);
    let err = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 45, "scales": [2, 1, 1], "relative": true}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("resulting scales"), "{}", err.message);
    assert!(mb.method_calls("node.setTransform").is_empty());
}

#[tokio::test]
async fn a_missing_node_is_the_reads_entity_not_found() {
    let tree = common::FgdTree::new("not-found");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    // the tool's own read catches it: the shared read taxonomy, nothing sent to the writer
    let err = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 999, "origin": [0, 0, 0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "EntityNotFound", "{err:?}");
    assert_eq!(err.code, Some(-32022));
    assert!(
        err.retry_safe,
        "the refusal precedes the undo group: {err:?}"
    );
    assert!(!err.outcome_unknown);
    assert!(mb.method_calls("node.setTransform").is_empty());

    // a node that vanishes between the read and the write comes back as the bridge's own -32022
    // passthrough (`NodeNotFound`, retry-safe: the bridge re-resolves the id before undo_open)
    mb.add_error(
        "node.setTransform",
        -32022,
        "NodeNotFound",
        Some(json!({"reason": "nodeID 45 not found"})),
    );
    let err = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 45, "origin": [0, 0, 0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NodeNotFound", "{err:?}");
    assert_eq!(err.code, Some(-32022));
    assert!(err.retry_safe);
    assert!(!err.outcome_unknown);
    assert_eq!(node_of(&mb, 45).origin, [4.0, 5.0, 6.0]);
}

#[tokio::test]
async fn an_untransformable_node_class_is_the_bridges_invalid_params() {
    let tree = common::FgdTree::new("world-node");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 1, "origin": [1, 1, 1]}),
    )
    .await
    .unwrap_err();
    // the wire taxonomy's shape for a bridge `-32602` (its kind is ProtocolError, the bridge's
    // own name is in `wireName`); the class gate runs before undo_open, so nothing was written
    assert_eq!(err.type_, "ProtocolError", "{err:?}");
    assert_eq!(err.wire_name.as_deref(), Some("InvalidParams"));
    assert_eq!(err.code, Some(-32602));
    assert!(!err.outcome_unknown);
    // the class gate refused after the write was sent (the mock validates before its undo group)
    assert_eq!(mb.method_calls("node.setTransform").len(), 1);
}

#[tokio::test]
async fn a_document_switch_is_document_changed_with_nothing_written() {
    let tree = common::FgdTree::new("doc-switch");
    let mb = hammer_mock(nodes()).await;
    // the write's `expectDoc` is read off the tool's own `doc.active`; a switch right after that
    // read makes the write's fingerprint stale, and the mock compares it like the bridge
    mb.set_hammer_doc_switch_after(Some(1));
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 45, "origin": [1, 1, 1]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "DocumentChanged", "{err:?}");
    assert_eq!(err.code, Some(-32004));
    assert!(err.retry_safe);
    assert!(!err.outcome_unknown);
    // the request was sent and refused before the undo group opened: nothing moved
    assert_eq!(mb.method_calls("node.setTransform").len(), 1);
    assert_eq!(node_of(&mb, 45).origin, [4.0, 5.0, 6.0]);
}

#[tokio::test]
async fn without_the_bridge_method_the_tool_is_not_implemented() {
    let tree = common::FgdTree::new("bridge-too-old");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .methods(["control.ping", "doc.active", "entity.get"])
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 45, "origin": [0, 0, 0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NotImplemented", "{err:?}");
    assert_eq!(err.code, Some(-32011));
    assert_eq!(err.data["missing"], json!(["node.setTransform"]));
    assert!(err.retry_safe);
    assert!(mb.method_calls("node.setTransform").is_empty());
    assert!(mb.method_calls("entity.get").is_empty());
}

/// The tool is gated on the bridge's `node.setTransform` (an agent that sees `CapabilityMissing`
/// can tell the bridge is older) and its description names the prop_static caveat.
#[tokio::test]
async fn the_tool_is_gated_on_node_set_transform() {
    let tree = common::FgdTree::new("description");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);
    let spec = server
        .registry()
        .spec("set_node_transform")
        .expect("registered");
    assert!(
        spec.bridge_methods.contains(&"node.setTransform") && spec.description.contains("prop_static"),
        "{}",
        spec.description
    );
    assert_eq!(spec.mutability, cs2_mcp_common::tools::Mutability::Mutating);
}

// -------------------------------------------------------------------------------------------
// The mesh follow-up reads (nothing is remembered between calls)
// -------------------------------------------------------------------------------------------

/// Create one mesh, move its node, then rebuild it: the operator reads Hammer's geometry again
/// (`mesh.get`, `geometrySource:"live"`) instead of anything remembered from before the move.
#[tokio::test]
async fn a_rebuild_after_a_successful_transform_reads_live() {
    let tree = common::FgdTree::new("cache-success");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);

    let created = call(
        &server,
        "create_block",
        json!({"shape": "box", "min": [0, 0, 0], "max": [64, 64, 64]}),
    )
    .await
    .unwrap();
    let node_id = created["nodeID"].as_i64().expect("nodeID");

    let out = call(
        &server,
        "set_node_transform",
        json!({"nodeID": node_id, "origin": [100, 0, 0]}),
    )
    .await
    .unwrap();
    assert_eq!(out["origin"], json!([100.0, 0.0, 0.0]));
    assert_eq!(out["unsavedBridgeEdits"], json!(2));

    let reads = mb.method_calls("mesh.get").len();
    let rebuilt = call(
        &server,
        "transform_mesh",
        json!({"nodeID": node_id, "translate": [8, 0, 0]}),
    )
    .await
    .unwrap();
    assert_eq!(rebuilt["geometrySource"], json!("live"), "{rebuilt}");
    assert_eq!(mb.method_calls("mesh.get").len(), reads + 1);
    assert_eq!(mb.method_calls("mesh.setGeometry").len(), 1);
}

/// A failure that may have landed (`-32603` after dispatch) counts the edit; the next mesh
/// operator reads the live geometry again — the mock really runs the transform behind that error.
#[tokio::test]
async fn a_maybe_executed_failure_is_followed_by_a_live_read() {
    let tree = common::FgdTree::new("cache-maybe");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);

    let created = call(
        &server,
        "create_block",
        json!({"shape": "box", "min": [0, 0, 0], "max": [64, 64, 64]}),
    )
    .await
    .unwrap();
    let node_id = created["nodeID"].as_i64().expect("nodeID");

    mb.add_error("node.setTransform", -32603, "InternalError", None);
    let err = call(
        &server,
        "set_node_transform",
        json!({"nodeID": node_id, "origin": [50, 0, 0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "ProtocolError");
    assert_eq!(err.code, Some(-32603));
    assert!(err.outcome_unknown, "the write may have landed: {err:?}");
    assert!(!err.retry_safe);
    assert_eq!(err.data["unsavedBridgeEdits"], json!(2));
    // the mock runs the op behind a -32603 (the reply can only come after it applied), so the
    // transform really landed
    assert_eq!(node_of(&mb, node_id).origin[0], 50.0);

    mb.with_state(|state| state.errors.clear());
    let rebuilt = call(
        &server,
        "transform_mesh",
        json!({"nodeID": node_id, "translate": [8, 0, 0]}),
    )
    .await
    .unwrap();
    assert_eq!(rebuilt["geometrySource"], json!("live"), "{rebuilt}");
    assert_eq!(mb.method_calls("mesh.setGeometry").len(), 1);
}

/// A refusal that ran nothing (`-32602`, decided before the undo group) must not count an edit or
/// touch the geometry.
#[tokio::test]
async fn a_pre_write_refusal_counts_nothing() {
    let tree = common::FgdTree::new("cache-pre");
    let mb = hammer_mock(vec![MockNode::world()]).await;
    let server = server(&tree, &mb);

    let created = call(
        &server,
        "create_block",
        json!({"shape": "box", "min": [0, 0, 0], "max": [64, 64, 64]}),
    )
    .await
    .unwrap();
    let node_id = created["nodeID"].as_i64().expect("nodeID");

    // the bridge's own -32602 (its gates run before the undo group, so the failed write counts
    // nothing and the document state is still the one this server verified)
    mb.add_error(
        "node.setTransform",
        -32602,
        "InvalidParams",
        Some(json!({"reason": "origin: [x,y,z] required"})),
    );
    let err = call(
        &server,
        "set_node_transform",
        json!({"nodeID": node_id, "origin": [1, 2, 3]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.wire_name.as_deref(), Some("InvalidParams"), "{err:?}");
    assert_eq!(err.code, Some(-32602));
    assert!(!err.outcome_unknown);
    assert!(err.data.get("unsavedBridgeEdits").is_none(), "{err:?}");
    mb.with_state(|state| state.errors.clear());

    // the rebuild reads the live geometry (the document is dirty, which only gates the file route)
    let out = call(
        &server,
        "transform_mesh",
        json!({"nodeID": node_id, "translate": [8, 0, 0]}),
    )
    .await
    .unwrap();
    assert_eq!(mb.method_calls("mesh.setGeometry").len(), 1, "{out}");
}

// -------------------------------------------------------------------------------------------
// The FGD-less tree: an unavailable database is a warning, never a refusal
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_unavailable_fgd_only_costs_the_compile_note() {
    let tree = common::FgdTree::without_fgd("no-fgd");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_node_transform",
        json!({"nodeID": 30, "origin": [1, 1, 1]}),
    )
    .await
    .unwrap();
    assert_eq!(out["origin"], json!([1.0, 1.0, 1.0]));
    assert_eq!(out["classname"], json!("prop_static"));
    assert!(
        !out["notes"].to_string().contains("compiled into the world"),
        "an unavailable FGD cannot prove the tag: {}",
        out["notes"]
    );
}
