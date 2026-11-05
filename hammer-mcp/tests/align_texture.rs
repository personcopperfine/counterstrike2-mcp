//! `align_texture` against the mock bridge's `mesh.setFaceTexture` (bridge m3.14).
//!
//! Pinned: the request the bridge gets, the unsaved counter and `next: save_map`, the read-back,
//! local refusals that send nothing, the bridge's own range refusal that writes nothing, and the
//! `NotImplemented` answer (with `data.missing`) of a bridge that predates m3.14.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};

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

/// A world with one 6-face block; returns its node id.
async fn setup(face_texture: bool) -> (MockBridge, McpServer<HammerState>, i64) {
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .hammer_face_texture(face_texture)
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&mb.discovery_path)).unwrap();
    let block = call(
        &server,
        "create_block",
        json!({"min": [0, 0, 0], "max": [128, 128, 128]}),
    )
    .await
    .unwrap()["nodeID"]
        .as_i64()
        .unwrap();
    (mb, server, block)
}

fn undo_depth(mb: &MockBridge) -> usize {
    mb.with_state(|s| s.hammer.as_ref().map_or(0, |w| w.undo.len()))
}

#[tokio::test]
async fn a_call_counts_as_an_unsaved_edit_and_points_at_save_map() {
    let (mb, server, block) = setup(true).await;
    let before = server.state().unsaved_edits();
    let depth = undo_depth(&mb);
    let out = call(
        &server,
        "align_texture",
        json!({"nodeID": block, "mode": "world", "faces": [1, 4], "keepScale": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["mode"], "world");
    assert_eq!(out["touched"], 2);
    assert_eq!(out["saved"], json!(false));
    assert_eq!(out["next"][0]["tool"], json!("save_map"));
    assert_eq!(out["undoLabel"], "MCP: set face texture");
    let faces = out["faces"].as_array().unwrap();
    assert_eq!(faces.len(), 2);
    assert_eq!(faces[0]["index"], 1);
    assert_eq!(faces[0]["axisU"], json!([1.0, 0.0, 0.0, 0.0]));
    assert_eq!(faces[1]["index"], 4);
    assert_eq!(out["unsavedBridgeEdits"], json!(before + 1));
    assert_eq!(undo_depth(&mb), depth + 1, "one undo entry per call");

    // a second call bumps the counter again; the explicit fields are absolute
    let again = call(
        &server,
        "align_texture",
        json!({"nodeID": block, "mode": "set", "faces": [1], "shift": [8, 16], "scale": [0.5, 0.5]}),
    )
    .await
    .unwrap();
    assert_eq!(again["unsavedBridgeEdits"], json!(out["unsavedBridgeEdits"].as_u64().unwrap() + 1));
    assert_eq!(again["faces"][0]["axisU"][3], json!(8.0));
    assert_eq!(again["faces"][0]["axisV"][3], json!(16.0));
    assert_eq!(again["faces"][0]["scale"], json!([0.5, 0.5]));

    // what the bridge was sent
    let sent = mb.method_calls("mesh.setFaceTexture");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].params["nodeID"], json!(block));
    assert_eq!(sent[0].params["mode"], "world");
    assert_eq!(sent[0].params["faces"], json!([1, 4]));
    assert_eq!(sent[0].params["keepScale"], json!(true));
    assert!(sent[0].params["expectDoc"].is_object(), "{}", sent[0].params);
    assert!(sent[0].params["deadlineMs"].as_u64().unwrap_or(0) > 0);
}

#[tokio::test]
async fn bad_arguments_are_refused_locally_and_nothing_is_sent() {
    let (mb, server, block) = setup(true).await;
    for bad in [
        json!({"nodeID": block}),
        json!({"nodeID": block, "mode": "bend"}),
        json!({"nodeID": block, "mode": "justify"}),
        json!({"nodeID": block, "mode": "justify", "justify": "middle"}),
        json!({"nodeID": block, "mode": "world", "justify": "left"}),
        json!({"nodeID": block, "mode": "justify", "justify": "left", "keepScale": true}),
        json!({"nodeID": block, "mode": "set"}),
        json!({"nodeID": block, "mode": "justify", "justify": "fit", "scale": [1, 1]}),
        json!({"nodeID": block, "mode": "justify", "justify": "fit", "rotate": 5}),
        json!({"nodeID": block, "mode": "justify", "justify": "fit", "shift": [1, 1]}),
        json!({"nodeID": block, "mode": "set", "scale": [0, 1]}),
        json!({"nodeID": block, "mode": "set", "scale": [1]}),
        json!({"nodeID": block, "mode": "set", "shift": ["a", 1]}),
        json!({"nodeID": block, "mode": "set", "rotate": "ninety"}),
        json!({"nodeID": block, "mode": "set", "rotate": 1e9}),
        json!({"nodeID": block, "mode": "world", "faces": []}),
        json!({"nodeID": block, "mode": "world", "faces": [-1]}),
        json!({"nodeID": block, "mode": "world", "faces": [2, 2]}),
        json!({"nodeID": block, "mode": "world", "texSet": 3}),
        json!({"nodeID": block, "mode": "world", "extra": 1}),
        json!({"nodeID": -1, "mode": "world"}),
    ] {
        let err = call(&server, "align_texture", bad.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}: {err:?}");
    }
    assert!(mb.method_calls("mesh.setFaceTexture").is_empty());
}

#[tokio::test]
async fn an_out_of_range_face_is_refused_without_a_write() {
    let (mb, server, block) = setup(true).await;
    let depth = undo_depth(&mb);
    let before = server.state().unsaved_edits();
    let err = call(
        &server,
        "align_texture",
        json!({"nodeID": block, "mode": "world", "faces": [0, 6]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert!(err.retry_safe);
    assert!(!err.outcome_unknown);
    assert!(
        err.message.contains("index 6 out of range") || format!("{:?}", err.data).contains("index 6 out of range"),
        "{err:?}"
    );
    assert_eq!(undo_depth(&mb), depth, "a refusal opens no undo group");
    assert_eq!(server.state().unsaved_edits(), before);
}

#[tokio::test]
async fn a_bridge_without_the_method_is_not_implemented_and_nothing_is_sent() {
    let (mb, server, block) = setup(false).await;
    let err = call(&server, "align_texture", json!({"nodeID": block, "mode": "world"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "NotImplemented", "{err:?}");
    assert_eq!(err.code, Some(-32011));
    assert_eq!(err.data["missing"], json!(["mesh.setFaceTexture"]));
    assert!(err.retry_safe);
    assert!(mb.method_calls("mesh.setFaceTexture").is_empty());
}

/// Pre-write refusals (`TextureSetUnknown`, nothing written): retry-safe, no counter bump.
#[tokio::test]
async fn a_pre_write_bridge_failure_is_retry_safe_and_does_not_count() {
    let (mb, server, block) = setup(true).await;
    mb.set_hammer_face_texture_fail(Some("pre"));
    let depth = undo_depth(&mb);
    let before = server.state().unsaved_edits();
    let err = call(&server, "align_texture", json!({"nodeID": block, "mode": "world"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, Some(-32603), "{err:?}");
    assert!(err.retry_safe, "{err:?}");
    assert!(!err.outcome_unknown, "{err:?}");
    assert_eq!(undo_depth(&mb), depth);
    assert_eq!(server.state().unsaved_edits(), before);
}

/// A failure after the write ("exec panicked" has no `maybeExecuted`; a read-back mismatch has it):
/// both are outcome-unknown and count as an unsaved edit.
#[tokio::test]
async fn a_post_write_failure_is_outcome_unknown_and_counts() {
    for fail in ["panic", "mismatch"] {
        let (mb, server, block) = setup(true).await;
        mb.set_hammer_face_texture_fail(Some(fail));
        let depth = undo_depth(&mb);
        let before = server.state().unsaved_edits();
        let err = call(&server, "align_texture", json!({"nodeID": block, "mode": "world"}))
            .await
            .unwrap_err();
        assert_eq!(err.code, Some(-32603), "{fail}: {err:?}");
        assert!(err.outcome_unknown, "{fail}: {err:?}");
        assert!(!err.retry_safe, "{fail}: {err:?}");
        assert_eq!(undo_depth(&mb), depth + 1, "{fail}: the write landed");
        assert_eq!(server.state().unsaved_edits(), before + 1, "{fail}");
    }
}

fn axes_face(index: usize) -> Value {
    json!({"index": index, "axisU": [1, 0, 0, 5], "axisV": [0, -1, 0, 2], "scale": [0.25, 0.5]})
}

#[tokio::test]
async fn axes_mode_sends_absolute_axes_and_returns_the_read_back_and_exact() {
    let (mb, server, block) = setup(true).await;
    let depth = undo_depth(&mb);
    let out = call(
        &server,
        "align_texture",
        json!({"nodeID": block, "mode": "axes", "faces": [axes_face(1), axes_face(4)], "texSet": 1}),
    )
    .await
    .unwrap();
    assert_eq!(out["mode"], "axes");
    assert_eq!(out["exact"], json!(true));
    assert_eq!(out["touched"], 2);
    assert_eq!(out["faces"][1]["index"], 4);
    assert_eq!(out["faces"][0]["axisU"], json!([1.0, 0.0, 0.0, 5.0]));
    assert_eq!(out["faces"][0]["scale"], json!([0.25, 0.5]));
    assert_eq!(out["next"][0]["tool"], json!("save_map"));
    assert_eq!(undo_depth(&mb), depth + 1);
    let sent = mb.method_calls("mesh.setFaceTexture");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].params["mode"], "axes");
    assert_eq!(sent[0].params["texSet"], 1);
    assert_eq!(sent[0].params["faces"][0]["axisV"], json!([0.0, -1.0, 0.0, 2.0]));
    assert!(sent[0].params.get("shift").is_none() && sent[0].params.get("scale").is_none());
}

#[tokio::test]
async fn axes_mode_is_validated_locally_before_any_bridge_call() {
    let (mb, server, block) = setup(true).await;
    let f = |patch: Value| {
        let mut face = axes_face(0);
        for (k, v) in patch.as_object().unwrap() {
            face[k] = v.clone();
        }
        json!({"nodeID": block, "mode": "axes", "faces": [face]})
    };
    let mut bad = vec![
        json!({"nodeID": block, "mode": "axes"}),
        json!({"nodeID": block, "mode": "axes", "faces": []}),
        json!({"nodeID": block, "mode": "axes", "faces": [3]}),
        json!({"nodeID": block, "mode": "axes", "faces": [axes_face(2), axes_face(2)]}),
        json!({"nodeID": block, "mode": "axes", "faces": [axes_face(0)], "texSet": 2}),
        // relative-mode args are refused with it
        json!({"nodeID": block, "mode": "axes", "faces": [axes_face(0)], "shift": [1, 1]}),
        json!({"nodeID": block, "mode": "axes", "faces": [axes_face(0)], "scale": [1, 1]}),
        json!({"nodeID": block, "mode": "axes", "faces": [axes_face(0)], "rotate": 5}),
        json!({"nodeID": block, "mode": "axes", "faces": [axes_face(0)], "keepScale": true}),
        json!({"nodeID": block, "mode": "axes", "faces": [axes_face(0)], "justify": "fit"}),
        // and a plain mode refuses the axes entries
        json!({"nodeID": block, "mode": "world", "faces": [axes_face(0)]}),
    ];
    for patch in [
        json!({"index": -1}),
        json!({"index": "a"}),
        json!({"axisU": [1, 0, 0]}),
        json!({"axisU": [1, 0, 0, "x"]}),
        json!({"axisU": [1, 0, 0, 1e60]}),
        json!({"axisU": [0, 0, 0, 5]}),
        json!({"axisV": [0, 0, 0, 5]}),
        json!({"axisV": [-3, 0, 0, 9]}),
        json!({"scale": [0, 1]}),
        json!({"scale": [1, -1]}),
        json!({"scale": [1]}),
        json!({"scale": [1, 1e60]}),
        json!({"extra": 1}),
    ] {
        bad.push(f(patch));
    }
    for args in bad {
        let err = call(&server, "align_texture", args.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{args}: {err:?}");
    }
    assert!(mb.method_calls("mesh.setFaceTexture").is_empty(), "nothing reached the bridge");
}

#[tokio::test]
async fn axes_mode_out_of_range_face_is_the_bridges_refusal() {
    let (mb, server, block) = setup(true).await;
    let depth = undo_depth(&mb);
    let err = call(&server, "align_texture", json!({"nodeID": block, "mode": "axes", "faces": [axes_face(99)]}))
        .await
        .unwrap_err();
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert!(err.retry_safe);
    assert_eq!(undo_depth(&mb), depth);
}
