//! `get_selection` / `set_selection` against the mock bridge's selection model (bridge m3.12).
//!
//! Pinned here: the request the bridge gets (`expectDoc`, a positive `deadlineMs`, the validated
//! params), the answers (applied vs requested, `modeChanged`, `notApplied`), that every refusal
//! leaves the selection unchanged, that selection is not an edit (no undo group, no dirty, no
//! unsaved counter), the capability gate, and that `set_selection` holds the server-wide write
//! gate while `get_selection` does not.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::time::Duration;

mod common;

const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";

fn config(discovery: &std::path::Path) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let discovery = discovery.display().to_string();
    let given: Vec<&str> = vec!["--deadline-ms", "5000", "--discovery", &discovery];
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

fn entity(id: i64) -> MockNode {
    let mut node = MockNode::entity(id, "info_target", [0.0; 3], Default::default());
    node.node_id = id;
    node
}

/// A world with the world node, one entity (node 5) and two 6-face blocks; returns their ids.
async fn setup(selection: bool) -> (MockBridge, McpServer<HammerState>, i64, i64) {
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world(), entity(5)])
        .hammer_selection(selection)
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&mb.discovery_path)).unwrap();
    let a = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [64, 64, 64]}),
    )
    .await
    .unwrap()["nodeID"]
        .as_i64()
        .unwrap();
    let b = call(
        &server,
        "create_block",
        json!({"min": [128, 0, 0], "max": [192, 64, 64]}),
    )
    .await
    .unwrap()["nodeID"]
        .as_i64()
        .unwrap();
    (mb, server, a, b)
}

fn undo_depth(mb: &MockBridge) -> usize {
    mb.with_state(|s| s.hammer.as_ref().map_or(0, |w| w.undo.len()))
}

#[tokio::test]
async fn get_selection_reports_the_mode_nodes_and_faces() {
    let (mb, server, a, b) = setup(true).await;
    let empty = call(&server, "get_selection", json!({})).await.unwrap();
    assert_eq!(empty["mode"], "objects");
    assert_eq!(empty["count"], 0);
    assert_eq!(empty["nodeIDs"], json!([]));
    assert_eq!(empty["source"], "live");

    mb.hammer_ui_select_nodes(2, &[a, 5]);
    let nodes = call(&server, "get_selection", json!({})).await.unwrap();
    assert_eq!(nodes["mode"], "meshes");
    assert_eq!(nodes["nodeIDs"], json!([a, 5]));
    assert_eq!(nodes["count"], 2);

    mb.hammer_ui_select_faces(&[(b, vec![3, 1]), (a, vec![0])]);
    let faces = call(&server, "get_selection", json!({})).await.unwrap();
    assert_eq!(faces["mode"], "faces");
    assert_eq!(faces["count"], 3);
    assert_eq!(faces["nodeIDs"], json!([b, a]));
    assert_eq!(
        faces["faces"],
        json!([{"nodeID": b, "faces": [1, 3]}, {"nodeID": a, "faces": [0]}])
    );
    let sent = mb.method_calls("selection.get");
    assert_eq!(sent.len(), 3);
    assert!(sent[0].params["deadlineMs"].as_u64().unwrap_or(0) > 0);
    assert!(call(&server, "get_selection", json!({"x": 1})).await.is_err());
}

#[tokio::test]
async fn set_selection_nodes_replace_add_remove_clear() {
    let (mb, server, a, b) = setup(true).await;
    let depth = undo_depth(&mb);
    let out = call(
        &server,
        "set_selection",
        json!({"op": "replace", "nodeIDs": [a, b]}),
    )
    .await
    .unwrap();
    assert_eq!(out["requested"], json!({"nodeIDs": 2}));
    assert_eq!(out["applied"]["nodeIDs"], json!([a, b]));
    assert_eq!(out["applied"]["mode"], "objects");
    assert_eq!(out["modeChanged"], false);
    assert_eq!(out["undoGroupOpened"], false);
    assert_eq!(out["saved"], json!(false));
    assert!(out.get("doc").is_none(), "{out}");
    assert!(out.get("notApplied").is_none());

    let sent = mb.method_calls("selection.set");
    assert!(sent[0].params["expectDoc"].is_object(), "{}", sent[0].params);
    assert!(sent[0].params["deadlineMs"].as_u64().unwrap_or(0) > 0);
    assert_eq!(sent[0].params["op"], "replace");

    let out = call(&server, "set_selection", json!({"op": "remove", "nodeIDs": [a]}))
        .await
        .unwrap();
    assert_eq!(out["applied"]["nodeIDs"], json!([b]));
    let out = call(&server, "set_selection", json!({"op": "add", "nodeIDs": [5]}))
        .await
        .unwrap();
    assert_eq!(out["applied"]["nodeIDs"], json!([b, 5]));
    let out = call(&server, "set_selection", json!({"op": "clear"})).await.unwrap();
    assert_eq!(out["applied"]["count"], 0);
    assert_eq!(out["requested"], json!({}));

    // selection is not an edit: no undo group, no unsaved counter
    assert_eq!(undo_depth(&mb), depth);
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["server"]["unsavedBridgeEdits"], json!(2), "only the two blocks: {status}");
}

#[tokio::test]
async fn set_selection_faces_switches_mode_and_get_selection_reads_them_back() {
    let (mb, server, a, b) = setup(true).await;
    let out = call(
        &server,
        "set_selection",
        json!({"op": "replace", "faces": [{"nodeID": b, "faces": [2, 0]}, {"nodeID": a, "faces": [5]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["requested"], json!({"meshes": 2, "faces": 3}));
    assert_eq!(out["modeChanged"], true);
    assert_eq!(out["applied"]["mode"], "faces");
    assert_eq!(out["applied"]["count"], 3);
    let read = call(&server, "get_selection", json!({})).await.unwrap();
    assert_eq!(
        read["faces"],
        json!([{"nodeID": b, "faces": [0, 2]}, {"nodeID": a, "faces": [5]}])
    );
    // add / remove in face mode do not switch again
    let out = call(
        &server,
        "set_selection",
        json!({"op": "remove", "faces": [{"nodeID": b, "faces": [0]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["modeChanged"], false);
    assert_eq!(out["applied"]["count"], 2);
    // back to nodes: Hammer switches mode (no conversion), add starts from an empty selection
    let out = call(&server, "set_selection", json!({"op": "add", "nodeIDs": [a]}))
        .await
        .unwrap();
    assert_eq!(out["modeChanged"], true);
    assert_eq!(out["applied"]["mode"], "objects");
    assert_eq!(out["applied"]["nodeIDs"], json!([a]));
    assert_eq!(mb.hammer_selection_state().unwrap().0, 1);
}

#[tokio::test]
async fn filtering_is_reported_not_raised() {
    let (mb, server, a, b) = setup(true).await;
    mb.hammer_selection_filters(&[b], &[a]);
    let out = call(
        &server,
        "set_selection",
        json!({"op": "replace", "nodeIDs": [a, b]}),
    )
    .await
    .unwrap();
    assert_eq!(out["applied"]["nodeIDs"], json!([a]));
    assert_eq!(out["notApplied"], json!({"nodeIDs": [b]}));
    assert!(out["note"].as_str().unwrap().contains("filtered"));
    let out = call(
        &server,
        "set_selection",
        json!({"op": "replace", "faces": [{"nodeID": a, "faces": [0]}, {"nodeID": b, "faces": [1, 2]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["applied"]["faces"], json!([{"nodeID": b, "faces": [1, 2]}]));
    assert_eq!(out["notApplied"], json!({"faces": [{"nodeID": a, "faces": [0]}]}));
}

#[tokio::test]
async fn refusals_change_nothing() {
    let (mb, server, a, _b) = setup(true).await;
    call(&server, "set_selection", json!({"op": "replace", "nodeIDs": [a]}))
        .await
        .unwrap();
    let before = mb.hammer_selection_state();
    let sets_before = mb.method_calls("selection.set").len();

    // bridge-side refusals pass through (-32602 with the details), nothing changes
    let err = call(&server, "set_selection", json!({"op": "add", "nodeIDs": [a, 9999]}))
        .await
        .unwrap_err();
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(err.data["unknown"], json!([9999]));
    assert!(err.retry_safe);
    let err = call(
        &server,
        "set_selection",
        json!({"op": "add", "faces": [{"nodeID": a, "faces": [0, 6]}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, Some(-32602));
    assert_eq!(err.data["badIndices"], json!([6]));
    assert_eq!(err.data["faceCount"], json!(6));
    let err = call(
        &server,
        "set_selection",
        json!({"op": "add", "faces": [{"nodeID": 5, "faces": [0]}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.data["reason"], "not a mesh");
    assert_eq!(mb.hammer_selection_state(), before);
    assert_eq!(mb.method_calls("selection.set").len(), sets_before + 3);

    // local refusals send nothing at all
    for bad in [
        json!({}),
        json!({"op": "replace"}),
        json!({"op": "replace", "nodeIDs": [a], "faces": [{"nodeID": a, "faces": [0]}]}),
        json!({"op": "clear", "nodeIDs": [a]}),
        json!({"op": "add", "nodeIDs": []}),
        json!({"op": "add", "faces": [{"nodeID": a, "faces": [-1]}]}),
    ] {
        assert!(call(&server, "set_selection", bad.clone()).await.is_err(), "{bad}");
    }
    assert_eq!(mb.method_calls("selection.set").len(), sets_before + 3);
    assert_eq!(mb.hammer_selection_state(), before);
}

#[tokio::test]
async fn the_request_carries_the_fingerprint_the_tool_just_read() {
    let (mb, server, a, _b) = setup(true).await;
    // the document identity moved before the call: the tool reads it first, so the request
    // carries the current epoch (a move between read and write is the bridge's -32004, pinned by
    // the mutator tests and the bridge's own gate tests)
    mb.with_state(|s| {
        if let Some(w) = s.hammer.as_mut() {
            w.epoch += 1;
        }
    });
    call(&server, "set_selection", json!({"op": "add", "nodeIDs": [a]}))
        .await
        .unwrap();
    let epoch = mb.with_state(|s| s.hammer.as_ref().map(|w| w.epoch)).unwrap();
    let sent = mb.method_calls("selection.set");
    assert_eq!(sent.last().unwrap().params["expectDoc"]["epoch"], json!(epoch));
}

#[tokio::test]
async fn a_bridge_without_selection_rows_is_not_implemented_and_nothing_is_sent() {
    let (mb, server, a, _b) = setup(false).await;
    for (tool, args, missing) in [
        ("get_selection", json!({}), "selection.get"),
        ("set_selection", json!({"op": "add", "nodeIDs": [a]}), "selection.set"),
    ] {
        let err = call(&server, tool, args).await.unwrap_err();
        assert_eq!(err.name, "NotImplemented", "{tool}: {err:?}");
        assert_eq!(err.code, Some(-32011));
        assert_eq!(err.data["missing"], json!([missing]));
        assert!(err.retry_safe);
    }
    assert!(mb.method_calls("selection.get").is_empty());
    assert!(mb.method_calls("selection.set").is_empty());
}

#[tokio::test]
async fn set_selection_holds_the_write_gate_and_get_selection_does_not() {
    let (_mb, server, a, _b) = setup(true).await;
    let guard = server.state().write_gate().await;
    // a read is not gated
    tokio::time::timeout(
        Duration::from_secs(2),
        call(&server, "get_selection", json!({})),
    )
    .await
    .expect("get_selection ran while the write gate was held")
    .unwrap();
    // a write waits for the gate
    let pending = tokio::time::timeout(
        Duration::from_millis(300),
        call(&server, "set_selection", json!({"op": "add", "nodeIDs": [a]})),
    )
    .await;
    assert!(pending.is_err(), "set_selection must wait for the write gate");
    drop(guard);
    let out = tokio::time::timeout(
        Duration::from_secs(2),
        call(&server, "set_selection", json!({"op": "add", "nodeIDs": [a]})),
    )
    .await
    .expect("set_selection ran once the gate was free")
    .unwrap();
    assert_eq!(out["applied"]["nodeIDs"], json!([a]));
}
