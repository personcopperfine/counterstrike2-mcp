//! `apply_layout` tests (WP1.2) against the mock bridge's Hammer world: one JSON layout of blocks,
//! meshes and entities, validated whole, then sent as one `mesh.create` per mesh plus ONE
//! `edit.batch` for the entities (the bridge's batch runs only entity ops, so the meshes keep one
//! undo group each; the result's `undoGroups` says so).
//!
//! Negative cases pinned: every bad item is reported with its index/id and NOTHING is sent;
//! `dryRun` sends nothing; a `path` outside the project root (absolute, `..`, symlink) and
//! both/neither of path/layout are refused; a layout over the mesh limit is refused up front; a
//! failure mid-way reports the created nodeIDs and the maybe-executed class of the failed request.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);
const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";

struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "layout-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("workshop/run/app/content/csgo_addons/mcp_test/maps"))
            .expect("tmp tree");
        Tree { root }
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn config(tree: &Tree, discovery: &Path) -> CommonConfig {
    config_with(tree, discovery, &[])
}

fn config_with(tree: &Tree, discovery: &Path, extra: &[&str]) -> CommonConfig {
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
    let mut given: Vec<&str> = if extra.contains(&"--deadline-ms") {
        vec![]
    } else {
        vec!["--deadline-ms", "5000"]
    };
    given.extend_from_slice(&[
        "--discovery",
        &discovery,
        "--project-root",
        &root,
        "--path-map",
        &map,
    ]);
    given.extend_from_slice(extra);
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

async fn call(server: &McpServer<HammerState>, name: &str, args: Value) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

async fn hammer_mock() -> MockBridge {
    mock_with(MockBridge::builder()).await
}

async fn mock_with(builder: cs2_mcp_common::mock::MockBuilder) -> MockBridge {
    let mb = builder
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(vec![MockNode::world()])
        .start()
        .await
        .unwrap();
    // the real fingerprint is structural: an edit does not move the epoch (one expectDoc serves the layout)
    mb.with_state(|s| {
        if let Some(world) = s.hammer.as_mut() {
            world.edits_keep_epoch = true;
        }
    });
    mb
}

fn server(tree: &Tree, mb: &MockBridge) -> McpServer<HammerState> {
    hammer_mcp::build_server(&config(tree, &mb.discovery_path)).unwrap()
}

/// Every method the layout tool may NOT call while it only validates.
fn assert_nothing_sent(mb: &MockBridge) {
    // (doc.active may be read by the server-wide addon pre-check, outside this tool)
    for method in ["mesh.create", "edit.batch", "entity.create"] {
        assert!(
            mb.method_calls(method).is_empty(),
            "{method} was sent: {:?}",
            mb.method_calls(method)
        );
    }
}

fn block(id: &str, x: f64) -> Value {
    json!({"kind": "block", "id": id, "min": [x, 0, 0], "max": [x + 64.0, 64, 32]})
}

fn quad_mesh(id: &str) -> Value {
    json!({"kind": "mesh", "id": id,
           "vertices": [[0, 0, 0], [64, 0, 0], [64, 64, 0], [0, 64, 0]],
           "faces": [[0, 1, 2, 3]]})
}

fn three_blocks_a_mesh_an_entity() -> Value {
    json!({
        "meshes": [block("a", 0.0), block("b", 100.0), block("c", 200.0), quad_mesh("floor")],
        "entities": [{"id": "spawn", "class": "info_target", "origin": [10, 20, 30],
                      "keyValues": {"targetname": "spawn_1"}}]
    })
}

#[tokio::test]
async fn a_layout_of_three_blocks_a_mesh_and_an_entity_maps_ids_and_batches_the_entities() {
    let tree = Tree::new("happy");
    let mb = hammer_mock().await;
    let server = server(&tree, &mb);

    let out = call(&server, "apply_layout", json!({"layout": three_blocks_a_mesh_an_entity()}))
        .await
        .unwrap();

    // four meshes: one mesh.create each; the entity: ONE edit.batch
    assert_eq!(mb.method_calls("mesh.create").len(), 4);
    let batches = mb.method_calls("edit.batch");
    assert_eq!(batches.len(), 1, "all entities share one edit.batch");
    assert_eq!(batches[0].params["ops"].as_array().unwrap().len(), 1);
    assert_eq!(out["undoGroups"], json!(5), "{out}");
    assert_eq!(out["bridgeRequests"], json!(5));
    assert_eq!(out["unsavedBridgeEdits"], json!(5));
    assert_eq!(out["saved"], json!(false));
    assert_eq!(out["next"][0]["tool"], json!("save_map"));

    let created = out["created"].as_array().unwrap();
    assert_eq!(created.len(), 5);
    let ids: Vec<&str> = created.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["a", "b", "c", "floor", "spawn"]);
    let kinds: Vec<&str> = created.iter().map(|c| c["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["block", "block", "block", "mesh", "entity"]);
    // every id maps to a node the mock world really holds
    let live: Vec<i64> = mb.hammer_nodes().iter().map(|n| n.node_id).collect();
    for item in created {
        let node_id = item["nodeID"].as_i64().expect("a nodeID per item");
        assert!(live.contains(&node_id), "{item} is not in the world {live:?}");
    }
    let spawn = mb
        .hammer_nodes()
        .into_iter()
        .find(|n| Some(n.node_id) == created[4]["nodeID"].as_i64())
        .unwrap();
    assert_eq!(spawn.classname, "info_target");
    assert_eq!(spawn.origin, [10.0, 20.0, 30.0]);
    // the bridge got exactly what create_block builds for the first block
    let first = &mb.method_calls("mesh.create")[0];
    assert_eq!(first.params["vertices"].as_array().unwrap().len(), 8);
    assert!(first.params["expectDoc"].is_object(), "every request carries expectDoc");
}

#[tokio::test]
async fn a_layout_file_inside_the_project_is_read() {
    let tree = Tree::new("file");
    tree.write(
        "workshop/run/app/content/csgo_addons/mcp_test/layouts/l.json",
        &json!({"meshes": [block("only", 0.0)]}).to_string(),
    );
    let mb = hammer_mock().await;
    let server = server(&tree, &mb);

    // relative to the active addon's content dir
    let out = call(&server, "apply_layout", json!({"path": "layouts/l.json"})).await.unwrap();
    assert_eq!(out["created"][0]["id"], json!("only"));
    // the Windows spelling of the same file goes through the path map
    let out = call(
        &server,
        "apply_layout",
        json!({"path": "z:\\app\\content\\csgo_addons\\mcp_test\\layouts\\l.json"}),
    )
    .await
    .unwrap();
    assert_eq!(out["undoGroups"], json!(1));
    assert_eq!(mb.method_calls("mesh.create").len(), 2);
}

#[tokio::test]
async fn every_bad_item_is_reported_and_nothing_is_sent() {
    let tree = Tree::new("bad");
    let mb = hammer_mock().await;
    let server = server(&tree, &mb);
    let layout = json!({
        "meshes": [
            block("fine", 0.0),
            // min is not below max on every axis
            {"kind": "block", "id": "flat", "min": [0, 0, 0], "max": [0, 64, 32]},
            // an unknown argument of the single tool
            {"kind": "block", "id": "typo", "min": [0, 0, 0], "max": [8, 8, 8], "scales": [1, 1, 1]},
            {"kind": "pyramid", "id": "shape"},
            {"kind": "mesh", "id": "dup", "vertices": [[0, 0, 0], [1, 0, 0]], "faces": [[0, 1, 2]]},
        ],
        "entities": [
            {"id": "ok", "class": "info_target", "origin": [0, 0, 0]},
            {"id": "noorigin", "class": "info_target"},
            {"id": "extra", "class": "info_target", "origin": [0, 0, 0], "scales": [1, 1, 1]},
        ]
    });
    let err = call(&server, "apply_layout", json!({"layout": layout})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(err.retry_safe);
    assert!(!err.outcome_unknown);
    let errors = err.data["errors"].as_array().unwrap();
    let found: Vec<(String, i64, String)> = errors
        .iter()
        .map(|e| {
            (
                e["section"].as_str().unwrap().to_string(),
                e["index"].as_i64().unwrap(),
                e["id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        found,
        vec![
            ("meshes".to_string(), 1, "flat".to_string()),
            ("meshes".to_string(), 2, "typo".to_string()),
            ("meshes".to_string(), 3, "shape".to_string()),
            ("meshes".to_string(), 4, "dup".to_string()),
            ("entities".to_string(), 1, "noorigin".to_string()),
            ("entities".to_string(), 2, "extra".to_string()),
        ]
    );
    assert_eq!(err.data["errorCount"], json!(6));
    assert_eq!(err.data["sent"], json!(false));
    assert!(err.message.contains("meshes[1]") && err.message.contains("entities[2]"), "{}", err.message);
    assert_nothing_sent(&mb);
}

#[tokio::test]
async fn two_bad_items_are_two_errors_with_their_indices() {
    let tree = Tree::new("two-bad");
    let mb = hammer_mock().await;
    let server = server(&tree, &mb);
    let layout = json!({"meshes": [
        block("good", 0.0),
        {"kind": "block", "id": "x", "min": [5, 5, 5], "max": [1, 1, 1]},
        block("good2", 64.0),
        {"kind": "block", "id": "y", "max": [1, 1, 1]},
    ]});
    let err = call(&server, "apply_layout", json!({"layout": layout})).await.unwrap_err();
    let errors = err.data["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0]["index"], json!(1));
    assert_eq!(errors[1]["index"], json!(3));
    assert_nothing_sent(&mb);
}

#[tokio::test]
async fn duplicate_ids_are_an_error() {
    let tree = Tree::new("dup");
    let mb = hammer_mock().await;
    let server = server(&tree, &mb);
    let layout = json!({"meshes": [block("same", 0.0), block("same", 100.0)]});
    let err = call(&server, "apply_layout", json!({"layout": layout})).await.unwrap_err();
    assert_eq!(err.data["errors"].as_array().unwrap().len(), 1);
    assert_eq!(err.data["errors"][0]["index"], json!(1));
    assert_nothing_sent(&mb);
}

#[tokio::test]
async fn dry_run_validates_reports_and_sends_nothing() {
    let tree = Tree::new("dry");
    let mb = hammer_mock().await;
    let server = server(&tree, &mb);
    let out = call(
        &server,
        "apply_layout",
        json!({"layout": three_blocks_a_mesh_an_entity(), "dryRun": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["dryRun"], json!(true));
    assert_eq!(out["valid"], json!(true));
    assert_eq!(out["sent"], json!(false));
    assert_eq!(out["wouldCreate"], json!({"meshes": 4, "entities": 1, "total": 5}));
    assert_eq!(out["undoGroups"], json!(5));
    assert_eq!(out["bounds"]["min"], json!([0.0, 0.0, 0.0]));
    assert_eq!(out["bounds"]["max"], json!([264.0, 64.0, 32.0]));
    assert_eq!(out["items"].as_array().unwrap().len(), 5);
    assert_eq!(out["items"][3]["vertexCount"], json!(4));
    assert_nothing_sent(&mb);
    assert_eq!(mb.hammer_nodes().len(), 1, "only the world node");

    // a dry run of a bad layout still refuses with the errors
    let err = call(
        &server,
        "apply_layout",
        json!({"layout": {"meshes": [{"kind": "block"}]}, "dryRun": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.data["errors"].as_array().unwrap().len(), 1);
    assert_nothing_sent(&mb);
}

#[tokio::test]
async fn a_path_outside_the_project_root_is_refused() {
    let tree = Tree::new("outside");
    let other = Tree::new("outside-other");
    let foreign = other.write("evil.json", &json!({"meshes": [block("x", 0.0)]}).to_string());
    let mb = hammer_mock().await;
    let server = server(&tree, &mb);

    // absolute path in another tree
    let err = call(&server, "apply_layout", json!({"path": foreign.display().to_string()}))
        .await
        .unwrap_err();
    assert!(err.message.contains("outside the project root"), "{}", err.message);
    // `..` segments are refused outright
    let err = call(&server, "apply_layout", json!({"path": "../evil.json"})).await.unwrap_err();
    assert!(err.message.contains(".."), "{}", err.message);
    // a symlink inside the project that points out of it
    #[cfg(unix)]
    {
        let link = tree.root.join("link.json");
        std::os::unix::fs::symlink(&foreign, &link).unwrap();
        let err = call(&server, "apply_layout", json!({"path": link.display().to_string()}))
            .await
            .unwrap_err();
        assert!(err.message.contains("outside the project root"), "{}", err.message);
    }
    // a file that is inside but not .json
    tree.write("layout.txt", "{}");
    let err = call(&server, "apply_layout", json!({"path": "layout.txt"})).await.unwrap_err();
    assert!(err.message.contains(".json"), "{}", err.message);
    // an unmapped Windows drive
    let err = call(&server, "apply_layout", json!({"path": "q:\\x\\l.json"})).await.unwrap_err();
    assert!(err.message.contains("no host mapping"), "{}", err.message);
    assert_nothing_sent(&mb);
}

#[tokio::test]
async fn path_and_layout_are_exclusive_and_one_is_required() {
    let tree = Tree::new("xor");
    tree.write("l.json", &json!({"meshes": [block("x", 0.0)]}).to_string());
    let mb = hammer_mock().await;
    let server = server(&tree, &mb);
    let both = call(
        &server,
        "apply_layout",
        json!({"path": "l.json", "layout": {"meshes": [block("x", 0.0)]}}),
    )
    .await
    .unwrap_err();
    assert!(both.message.contains("exactly one"), "{}", both.message);
    let neither = call(&server, "apply_layout", json!({})).await.unwrap_err();
    assert!(neither.message.contains("exactly one"), "{}", neither.message);
    let empty = call(&server, "apply_layout", json!({"layout": {}})).await.unwrap_err();
    assert!(empty.message.contains("empty"), "{}", empty.message);
    let unknown = call(&server, "apply_layout", json!({"layout": {"walls": []}})).await.unwrap_err();
    assert!(unknown.message.contains("walls"), "{}", unknown.message);
    assert_nothing_sent(&mb);
}

#[tokio::test]
async fn a_layout_over_the_mesh_limit_is_refused_up_front() {
    let tree = Tree::new("limit");
    let mb = hammer_mock().await;
    let server = server(&tree, &mb);
    let meshes: Vec<Value> = (0..33).map(|i| block(&format!("b{i}"), i as f64 * 100.0)).collect();
    let err = call(&server, "apply_layout", json!({"layout": {"meshes": meshes}}))
        .await
        .unwrap_err();
    assert!(err.message.contains("33") && err.message.contains("32"), "{}", err.message);
    assert_nothing_sent(&mb);

    let entities: Vec<Value> = (0..65)
        .map(|i| json!({"class": "info_target", "origin": [i, 0, 0]}))
        .collect();
    let err = call(&server, "apply_layout", json!({"layout": {"entities": entities}}))
        .await
        .unwrap_err();
    assert!(err.message.contains("65") && err.message.contains("64"), "{}", err.message);
    assert_nothing_sent(&mb);
}

#[tokio::test]
async fn a_failure_mid_layout_reports_what_was_created_and_maybe_executed() {
    let tree = Tree::new("mid-unknown");
    // the third mesh.create answers -32603: the bridge can only say that after it started writing
    let mb = mock_with(MockBridge::builder().error_on_call(
        "mesh.create",
        3,
        -32603,
        "InternalError",
        Some(json!({"reason": "CreateElement<CMapMesh> failed"})),
    ))
    .await;
    let server = server(&tree, &mb);
    let err = call(&server, "apply_layout", json!({"layout": three_blocks_a_mesh_an_entity()}))
        .await
        .unwrap_err();
    assert!(err.outcome_unknown, "{err:?}");
    assert!(!err.retry_safe);
    let created = err.data["created"].as_array().unwrap();
    assert_eq!(created.len(), 2);
    assert_eq!(created[0]["id"], json!("a"));
    assert_eq!(created[1]["id"], json!("b"));
    for item in created {
        assert!(item["nodeID"].as_i64().is_some());
    }
    assert_eq!(err.data["failedItem"]["id"], json!("c"));
    assert_eq!(err.data["notSent"], json!(2), "floor and the entity; c was sent");
    assert_eq!(err.data["undoGroups"], json!(2), "only the certain groups");
    assert_eq!(err.data["failedItemMayExist"], json!(true));
    let recovery = err.data["recovery"].as_str().unwrap();
    assert!(recovery.contains("undo_last 2 time"), "{recovery}");
    assert!(recovery.contains("one more time"), "{recovery}");
    // the counter covers the two groups plus the maybe-executed one
    assert_eq!(err.data["unsavedBridgeEdits"], json!(3));
    // the run stopped: the fourth mesh and the entity batch were never sent
    assert_eq!(mb.method_calls("mesh.create").len(), 3);
    assert!(mb.method_calls("edit.batch").is_empty());
}

#[tokio::test]
async fn a_pre_write_refusal_mid_layout_is_not_maybe_executed() {
    let tree = Tree::new("mid-refused");
    // -32602 is the bridge refusing the params before it opens an undo group
    let mb = mock_with(MockBridge::builder().error_on_call(
        "mesh.create",
        2,
        -32602,
        "InvalidParams",
        Some(json!({"reason": "bad vertices"})),
    ))
    .await;
    let server = server(&tree, &mb);
    let err = call(&server, "apply_layout", json!({"layout": three_blocks_a_mesh_an_entity()}))
        .await
        .unwrap_err();
    assert!(!err.outcome_unknown, "{err:?}");
    assert_eq!(err.data["created"].as_array().unwrap().len(), 1);
    assert_eq!(err.data["undoGroups"], json!(1));
    assert_eq!(err.data["failedItemMayExist"], json!(false));
    assert_eq!(err.data["notSent"], json!(3), "c, floor and the entity; b was sent and refused");
    assert_eq!(err.data["failedItem"]["id"], json!("b"));
    assert_eq!(err.data["unsavedBridgeEdits"], json!(1));
    assert!(err.data["recovery"].as_str().unwrap().contains("undo_last 1 time"));
}

#[tokio::test]
async fn an_entity_batch_failure_still_reports_the_meshes_that_exist() {
    let tree = Tree::new("mid-batch");
    let mb = mock_with(MockBridge::builder().error_on_call(
        "edit.batch",
        1,
        -32603,
        "InternalError",
        Some(json!({"reason": "batch blew up"})),
    ))
    .await;
    let server = server(&tree, &mb);
    let err = call(&server, "apply_layout", json!({"layout": three_blocks_a_mesh_an_entity()}))
        .await
        .unwrap_err();
    assert!(err.outcome_unknown, "{err:?}");
    assert_eq!(err.data["created"].as_array().unwrap().len(), 4);
    assert_eq!(err.data["undoGroups"], json!(4), "four meshes; the batch is only maybe");
    assert_eq!(err.data["failedItemMayExist"], json!(true));
    assert_eq!(err.data["failedItem"]["kind"], json!("entity"));
    assert_eq!(mb.method_calls("mesh.create").len(), 4);
}

fn server_with(tree: &Tree, mb: &MockBridge, extra: &[&str]) -> McpServer<HammerState> {
    hammer_mcp::build_server(&config_with(tree, &mb.discovery_path, extra)).unwrap()
}

#[tokio::test]
async fn a_transport_timeout_mid_layout_is_outcome_unknown() {
    let tree = Tree::new("timeout");
    // the 2nd mesh.create is never answered: the request times out and may have landed
    let mb = hammer_mock().await;
    let server = server_with(&tree, &mb, &["--deadline-ms", "400"]);
    let first = call(&server, "apply_layout", json!({"layout": {"meshes": [block("warm", 900.0)]}}))
        .await
        .unwrap();
    assert_eq!(first["count"], json!(1));
    mb.with_state(|s| {
        s.silent_methods.insert("mesh.create".into());
    });
    let err = call(&server, "apply_layout", json!({"layout": {"meshes": [block("a", 0.0), block("b", 100.0)]}}))
        .await
        .unwrap_err();
    assert!(err.outcome_unknown, "{err:?}");
    assert_eq!(err.data["created"].as_array().unwrap().len(), 0);
    assert_eq!(err.data["failedItemMayExist"], json!(true));
    assert_eq!(err.data["undoGroups"], json!(0));
    assert_eq!(err.data["notSent"], json!(1));
}

#[tokio::test]
async fn a_document_switch_mid_layout_is_a_clean_refusal_with_progress() {
    let tree = Tree::new("doc-changed");
    let mb = mock_with(MockBridge::builder().error_on_call(
        "mesh.create",
        2,
        -32004,
        "DocumentChanged",
        Some(json!({"reason": "doc_id"})),
    ))
    .await;
    let server = server(&tree, &mb);
    let err = call(&server, "apply_layout", json!({"layout": three_blocks_a_mesh_an_entity()}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "DocumentChanged");
    assert!(!err.outcome_unknown);
    assert_eq!(err.data["created"].as_array().unwrap().len(), 1);
    assert_eq!(err.data["undoGroups"], json!(1));
    assert_eq!(err.data["failedItemMayExist"], json!(false));
    assert_eq!(mb.method_calls("mesh.create").len(), 2);
    assert!(mb.method_calls("edit.batch").is_empty());
}

#[tokio::test]
async fn an_exhausted_budget_mid_layout_reports_progress_and_stops() {
    let tree = Tree::new("budget");
    let mb = hammer_mock().await;
    mb.with_state(|s| {
        s.delay_methods.insert("mesh.create".into(), 300);
    });
    let server = server_with(&tree, &mb, &["--tool-budget-ms", "800"]);
    let err = call(&server, "apply_layout", json!({"layout": three_blocks_a_mesh_an_entity()}))
        .await
        .unwrap_err();
    // the budget ran out before the layout finished: a timeout-class error, never a success
    let created = err.data["created"].as_array().unwrap().len();
    assert!((1..4).contains(&created), "{created}: {err:?}");
    assert_eq!(err.data["undoGroups"], json!(created));
    assert_eq!(err.data["failedItemMayExist"], json!(err.outcome_unknown));
    assert!(err.data["recovery"].as_str().unwrap().contains("undo_last"));
    assert!(mb.method_calls("edit.batch").is_empty(), "the entity batch was never sent");
}

#[tokio::test]
async fn a_partial_entity_batch_keeps_the_entities_created_before_the_failure() {
    let tree = Tree::new("partial");
    let results = json!([
        {"class": "info_target", "origin": [0, 0, 0], "nodeID": 41},
        {"error": {"code": -32011, "message": "NotImplemented", "data": {}}}
    ]);
    let mb = mock_with(MockBridge::builder().error_on_call(
        "edit.batch",
        1,
        -32012,
        "PartialFailure",
        Some(json!({"results": results, "failedIndex": 1})),
    ))
    .await;
    let server = server(&tree, &mb);
    let layout = json!({
        "meshes": [block("a", 0.0)],
        "entities": [
            {"id": "e0", "class": "info_target", "origin": [0, 0, 0]},
            {"id": "e1", "class": "info_target", "origin": [1, 0, 0]},
            {"id": "e2", "class": "info_target", "origin": [2, 0, 0]},
        ]
    });
    let err = call(&server, "apply_layout", json!({"layout": layout})).await.unwrap_err();
    // the bridge stops at the failing op: e1 failed, e2 never ran
    assert_eq!(err.data["failedItem"]["id"], json!("e1"), "{err:?}");
    assert_eq!(err.data["notSent"], json!(1));
    let created = err.data["created"].as_array().unwrap();
    assert_eq!(created.len(), 2, "{created:?}");
    assert_eq!(created[1]["id"], json!("e0"));
    assert_eq!(created[1]["nodeID"], json!(41));
    assert_eq!(err.data["undoGroups"], json!(2), "the mesh group + the batch group");
    assert_eq!(err.data["failedItemMayExist"], json!(false));
    assert!(err.outcome_unknown);
}

#[tokio::test]
async fn a_reply_without_a_node_id_is_listed_and_gets_a_find_step() {
    let tree = Tree::new("no-node-id");
    // a real-shaped mesh.create reply (mesh summary + counts) whose own lookup failed: nodeID -1
    let mb = mock_with(MockBridge::builder().responder_value(
        "mesh.create",
        json!({"nodeID": -1, "origin": [0, 0, 0], "inputVertices": 8, "faces": 6,
               "materials": 1, "undoLabel": "MCP: create mesh"}),
    ))
    .await;
    let server = server(&tree, &mb);
    let out = call(
        &server,
        "apply_layout",
        json!({"layout": {"meshes": [block("lost", 0.0), block("also", 100.0)]}}),
    )
    .await
    .unwrap();
    assert_eq!(out["missingNodeIDs"], json!(["lost", "also"]));
    assert_eq!(out["created"][0]["nodeID"], Value::Null);
    // bare meshes are not entities: save, then find them in the saved .vmap
    assert_eq!(out["next"][0]["tool"], json!("save_map"));
    assert_eq!(out["next"][1]["tool"], json!("list_meshes"));
}

#[tokio::test]
async fn a_dry_run_after_an_edit_does_not_point_at_save_map() {
    let tree = Tree::new("dry-after-edit");
    let mb = hammer_mock().await;
    let server = server(&tree, &mb);
    let layout = json!({"meshes": [block("a", 0.0)]});
    let out = call(&server, "apply_layout", json!({"layout": layout})).await.unwrap();
    assert_eq!(out["next"][0]["tool"], json!("save_map"));
    let out = call(&server, "apply_layout", json!({"layout": layout, "dryRun": true}))
        .await
        .unwrap();
    assert_eq!(out["next"][0]["tool"], json!("apply_layout"), "{out}");
    // the suggested call is valid as written: the same layout, dryRun false
    assert_eq!(out["next"][0]["args"], json!({"layout": layout, "dryRun": false}), "{out}");
}
