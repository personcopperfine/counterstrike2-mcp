//! `lifecycle` tool tests (WP-T2b): `save_map` (the only writer) and `delete_nodes` against the
//! mock bridge's Hammer world (WP-M built-ins: `doc.active`, `map.save`, `node.delete`).
//!
//! The temp project tree lives under `CARGO_TARGET_TMPDIR` (never /tmp); the document path is the
//! mapped Windows form `z:\app\content\csgo_addons\mcp_test\maps\probe.vmap`, so `save_map`'s
//! host-path mapping and its read-only refusal are exercised for real.
//!
//! **`doc.active`.** The tools read the active document through
//! `BridgeClient::active_document`, i.e. the bridge's `{"doc":{...}}` result (mcp_bridge.c); the
//! mock's Hammer world serves exactly that wrapped shape and is the live document (its own
//! `hammer_doc_active_serves_the_world_fingerprint` test pins that), so the mocks here enable the
//! world and let it answer `doc.active`. Only a test that needs a document the world cannot
//! express (dirty *unknown*, a canned reply) installs a `doc.active` responder.
//!
//! Helpers (`Tree`, `config`, `call`) are local copies of the pattern in `tests/entities_file.rs`.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
use cs2_mcp_common::tools::{Budget, CallCtx, CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The document path the mock serves (Windows form; `--path-map` maps `z:\app` at the tree).
const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "lifecycle-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let app = root.join("workshop/run/app");
        std::fs::create_dir_all(app.join("content/csgo_addons/mcp_test/maps")).expect("tmp tree");
        Tree { root, app }
    }

    /// The mapped host path of [`DOC_WIN`].
    fn map(&self) -> PathBuf {
        self.app
            .join("content/csgo_addons/mcp_test/maps/probe.vmap")
    }

    /// Write a file under the addon's `maps/` directory: what `map.open`'s H1 pre-parse and the
    /// bridge's host/header gates read.
    fn write_map(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.app.join("content/csgo_addons/mcp_test/maps").join(name);
        std::fs::create_dir_all(path.parent().expect("maps dir")).expect("maps dir");
        std::fs::write(&path, bytes).expect("write map");
        path
    }

    /// A committed real map (`hammer-mcp/fixtures/vmap/<name>`).
    fn fixture_map(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/vmap")
            .join(name)
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
    let map = format!("z:\\app={}", tree.app.display());
    let discovery = discovery.display().to_string();
    let mut given: Vec<&str> = vec![
        "--deadline-ms",
        "5000",
        "--discovery",
        &discovery,
        "--project-root",
        &root,
        "--path-map",
        &map,
    ];
    given.extend_from_slice(extra);
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

/// A `class:"mesh"` node (anything that is not an entity).
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

/// The world node, entity 30 and mesh 9 — the fixture every test below starts from.
fn nodes() -> Vec<MockNode> {
    vec![
        MockNode::world(),
        MockNode::entity(
            30,
            "info_map_parameters",
            [1.0, 2.0, 3.0],
            kv(&[("bombradius", "500")]),
        ),
        mesh(9, [10.0, 0.0, 0.0]),
    ]
}

/// A Hammer world (WP-M) with the built-in `doc.active`: the world serves the **live** document
/// (epoch/dirty/undo state, path/isNewFile/editable from the builder), so a tool that reads
/// `doc.active` before a mutator sees the fingerprint it must send back as `expectDoc` — exactly
/// what the real bridge does (`doc.active` calls `doc_fingerprint()`, mcp_bridge.c). Tests that
/// need a shape the world cannot express (a dirty-*unknown* document) install their own responder.
async fn hammer_mock(
    world_nodes: Vec<MockNode>,
    dirty: bool,
    editable: bool,
    new_file: bool,
) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(dirty))))
        .hammer_nodes(world_nodes)
        .hammer_editable(editable)
        .hammer_new_file(new_file)
        .start()
        .await
        .unwrap()
}

fn node_ids(mb: &MockBridge) -> Vec<i64> {
    let mut ids: Vec<i64> = mb.hammer_nodes().iter().map(|n| n.node_id).collect();
    ids.sort();
    ids
}

// -------------------------------------------------------------------------------------------
// Acceptance
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn save_map_writes_and_reports_the_mapped_host_path() {
    let tree = Tree::new("save");
    std::fs::write(tree.map(), b"vmap").unwrap();
    let mb = hammer_mock(nodes(), true, true, false).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let out = call(&server, "save_map", json!({})).await.unwrap();
    assert_eq!(out["saved"], json!(true));
    assert_eq!(out["skippedClean"], json!(false), "the document was dirty");
    assert_eq!(out["path"], json!(DOC_WIN));
    assert_eq!(out["hostPath"], json!(tree.map().display().to_string()));
    assert_eq!(out["unsavedBridgeEdits"], json!(0));
    assert_eq!(mb.method_calls("map.save").len(), 1);
    assert_eq!(out["next"][0]["tool"], json!("compile_map"), "{out}");
    // the mock's document is clean now: a second save skips the write
    let out = call(&server, "save_map", json!({})).await.unwrap();
    assert_eq!(out["saved"], json!(true));
    assert_eq!(out["skippedClean"], json!(true));
    assert_eq!(mb.method_calls("map.save").len(), 2);
}

#[tokio::test]
async fn delete_nodes_removes_the_node_bumps_the_counter_and_save_resets_it() {
    let tree = Tree::new("delete");
    std::fs::write(tree.map(), b"vmap").unwrap();
    let mb = hammer_mock(nodes(), true, true, false).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let out = call(&server, "delete_nodes", json!({"nodeIDs": [30]}))
        .await
        .unwrap();
    assert_eq!(out["deleted"], json!([30]));
    assert_eq!(out["undoLabel"], json!("MCP: delete nodes"));
    assert_eq!(out["saved"], json!(false));
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
    assert_eq!(out["next"][0]["tool"], json!("save_map"), "a live mutator points at save_map");
    assert_eq!(node_ids(&mb), vec![1, 9], "node 30 is gone from the mock world");

    // expectDoc was sent and carries the document fingerprint
    let call0 = &mb.method_calls("node.delete")[0];
    assert_eq!(call0.params["expectDoc"]["id"], json!("d1"));
    assert!(call0.params["expectDoc"]["epoch"].is_i64());
    assert!(call0.params["deadlineMs"].as_u64().unwrap_or(0) > 0);

    // save_map resets the counter only on saved:true
    let out = call(&server, "save_map", json!({})).await.unwrap();
    assert_eq!(out["saved"], json!(true));
    assert_eq!(out["unsavedBridgeEdits"], json!(0));
}

// -------------------------------------------------------------------------------------------
// Required negatives
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn save_map_with_editable_false_is_read_only_and_sends_nothing() {
    let tree = Tree::new("readonly-doc");
    let mb = hammer_mock(vec![MockNode::world()], true, false, false).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let err = call(&server, "save_map", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "ReadOnlyMap");
    assert_eq!(err.name, "ReadOnlyMap");
    assert!(err.retry_safe, "nothing was dispatched");
    assert_eq!(err.data["notEditableReason"], json!("not_editable"));
    assert_eq!(err.data["document"]["id"], json!("d1"));
    assert!(
        mb.method_calls("map.save").is_empty(),
        "the bridge must not be asked to save"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn save_map_with_a_read_only_host_file_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let tree = Tree::new("readonly-host");
    std::fs::write(tree.map(), b"vmap").unwrap();
    std::fs::set_permissions(tree.map(), std::fs::Permissions::from_mode(0o444)).unwrap();
    let mb = hammer_mock(nodes(), true, true, false).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let err = call(&server, "save_map", json!({})).await.unwrap_err();
    assert_eq!(err.name, "ReadOnlyMap");
    assert!(err.retry_safe);
    assert_eq!(err.data["hostPath"], json!(tree.map().display().to_string()));
    assert_eq!(err.data["windowsPath"], json!(DOC_WIN));
    assert!(mb.method_calls("map.save").is_empty());
}

#[tokio::test]
async fn save_map_on_an_untitled_document_passes_untitled_document_through() {
    let tree = Tree::new("untitled");
    let mb = hammer_mock(vec![MockNode::world()], true, true, true).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let err = call(&server, "save_map", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "UntitledDocument");
    assert_eq!(err.name, "UntitledDocument");
    assert_eq!(err.code, Some(-32015));
    assert!(err.retry_safe);
    // the bridge is the one that decides this (Save-As is modal): the call was made
    assert_eq!(mb.method_calls("map.save").len(), 1);
}

#[tokio::test]
async fn delete_nodes_with_no_ids_is_invalid_arguments_and_sends_nothing() {
    let tree = Tree::new("delete-args");
    let mb = hammer_mock(nodes(), true, true, false).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    for bad in [
        json!({}),
        json!({"nodeIDs": []}),
        json!({"nodeIDs": [-1]}),
        json!({"nodeIDs": [30, 30]}),
        json!({"nodeIDs": [1.5]}),
        json!({"nodeIDs": [true]}),
        json!({"nodeIDs": "30"}),
        // above the bridge's `int`: it would truncate and delete a different node (brief item 5)
        json!({"nodeIDs": [2_147_483_648i64]}),
        json!({"nodeIDs": [4_294_967_298i64]}),
    ] {
        let err = call(&server, "delete_nodes", bad.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
        assert_eq!(err.name, "InvalidParams", "{bad}");
        assert!(err.retry_safe, "{bad}");
    }
    let oversized: Vec<i64> = (0..257).collect();
    let err = call(&server, "delete_nodes", json!({"nodeIDs": oversized}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");

    assert!(
        mb.method_calls("node.delete").is_empty(),
        "local validation refuses before dispatch"
    );
    assert_eq!(node_ids(&mb), vec![1, 9, 30], "nothing was deleted");
}

#[tokio::test]
async fn delete_of_the_world_surfaces_the_bridge_error_and_deletes_nothing() {
    let tree = Tree::new("delete-world");
    let mb = hammer_mock(nodes(), true, true, false).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let err = call(&server, "delete_nodes", json!({"nodeIDs": [1]}))
        .await
        .unwrap_err();
    assert_eq!(err.code, Some(-32602));
    assert!(
        err.data["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("cannot be deleted"),
        "the bridge's refusal is surfaced: {err:?}"
    );
    assert_eq!(node_ids(&mb), vec![1, 9, 30], "the world is still there");
    assert_eq!(mb.method_calls("node.delete").len(), 1);

    // the failed delete did NOT bump the counter: the next successful one reports 1
    let out = call(&server, "delete_nodes", json!({"nodeIDs": [30]}))
        .await
        .unwrap();
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
}

/// Brief item 10: a missing node id is `EntityNotFound` with the ids in the data, the same name
/// `get_entity_live`/`set_entity_keyvalues` use.
#[tokio::test]
async fn delete_of_a_missing_node_is_entity_not_found() {
    let tree = Tree::new("delete-missing");
    let mb = hammer_mock(nodes(), true, true, false).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let err = call(&server, "delete_nodes", json!({"nodeIDs": [999999]}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "EntityNotFound");
    assert_eq!(err.name, "EntityNotFound");
    assert_eq!(err.code, Some(-32022));
    assert_eq!(err.data["nodeIDs"], json!([999999]));
    assert!(err.retry_safe, "the bridge validates every id before deleting: {err:?}");
    assert_eq!(node_ids(&mb), vec![1, 9, 30], "nothing was deleted");
}

/// Brief item 8: a `-32603 InternalError` on a mutating call may have executed — the counter moves
/// and the error says so.
#[tokio::test]
async fn a_protocol_error_on_delete_nodes_is_maybe_executed() {
    let tree = Tree::new("delete-protocol");
    let mb = hammer_mock(nodes(), true, true, false).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    // the mock runs the built-in and then answers the error, like the bridge after a write
    mb.add_error("node.delete", -32603, "InternalError", None);

    let err = call(&server, "delete_nodes", json!({"nodeIDs": [30]}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "ProtocolError");
    assert!(err.outcome_unknown, "the delete may have landed: {err:?}");
    assert!(!err.retry_safe);
    assert_eq!(err.data["unsavedBridgeEdits"], json!(1));
    assert_eq!(node_ids(&mb), vec![1, 9], "the node really went away");
}

// -------------------------------------------------------------------------------------------
// open_map (map.open; live behaviour spec: bridge/protocol-diff-m2.16.md)
// -------------------------------------------------------------------------------------------

/// A Hammer world whose `doc.active` is served by the world itself (no responder): `open_map`
/// re-reads the active document after the open, so the document has to be the live one.
async fn hammer_mock_live(dirty: bool, map_files: Vec<&str>) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(dirty))))
        .hammer_nodes(nodes())
        .hammer_map_files(map_files)
        .start()
        .await
        .unwrap()
}

#[tokio::test]
async fn open_map_opens_a_new_file_and_points_the_counter_at_the_new_document() {
    let tree = Tree::new("open-new");
    let mb = hammer_mock_live(false, vec!["maps/b.vmap"]).await;
    // review-5 L4: the pre-parse reads the host file now, so the map has to be on the host too
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.write_map("b.vmap", b"")).unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    // one counted edit on the document that is open now
    call(&server, "delete_nodes", json!({"nodeIDs": [30]})).await.unwrap();
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["server"]["unsavedBridgeEdits"], json!(1));
    assert_eq!(status["document"]["id"], json!("d1"));

    let out = call(&server, "open_map", json!({"path": "maps/b.vmap"}))
        .await
        .unwrap();
    assert_eq!(out["opened"], json!(true), "{out}");
    assert_eq!(out["existing"], json!(false), "{out}");
    assert_eq!(out["contentPath"], json!("maps/b.vmap"));
    assert_eq!(
        out["path"],
        json!("Z:\\app\\content\\csgo_addons\\mcp_test\\maps\\b.vmap"),
        "{out}"
    );
    assert_eq!(
        out["hostPath"],
        json!(tree
            .app
            .join("content/csgo_addons/mcp_test/maps/b.vmap")
            .display()
            .to_string()),
        "{out}"
    );
    // the new document has no MCP edits yet: the counter belongs to the NEW id
    assert_eq!(out["unsavedBridgeEdits"], json!(0), "{out}");
    assert_eq!(out["document"]["path"], out["path"], "{out}");
    assert_ne!(out["document"]["id"], json!("d1"), "{out}");
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["server"]["unsavedBridgeEdits"], json!(0), "{status}");
    assert_eq!(status["document"]["id"], out["document"]["id"], "{status}");

    let sent = &mb.method_calls("map.open")[0].params;
    assert_eq!(sent["path"], json!("maps/b.vmap"), "{sent}");
    assert!(sent["deadlineMs"].as_u64().unwrap_or(0) > 0, "{sent}");
    assert!(sent.get("expectDoc").is_none(), "map.open takes no expectDoc: {sent}");
}

/// The bridge's session-leak gate: a file that is already open is `existing:true` — a RESULT with a
/// note, not an error — and nothing about this server's document state moves (the bridge does not
/// call `OpenFile`, the counter stays with the document that is still open). This is the
/// `active:true` note: it is the one case where the note may point at `reload_map` (review-5 M1),
/// because `reload_map` acts on the active document.
#[tokio::test]
async fn open_map_of_an_already_open_file_is_a_result_and_changes_nothing() {
    let tree = Tree::new("open-existing");
    let mb = hammer_mock_live(false, vec![]).await;
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.map()).unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    call(&server, "delete_nodes", json!({"nodeIDs": [30]})).await.unwrap();

    let out = call(&server, "open_map", json!({"path": "maps/probe.vmap"}))
        .await
        .unwrap();
    assert_eq!(out["opened"], json!(false), "{out}");
    assert_eq!(out["existing"], json!(true), "{out}");
    assert_eq!(out["active"], json!(true), "{out}");
    assert_eq!(out["path"], json!(DOC_WIN), "{out}");
    assert_eq!(out["contentPath"], json!("maps/probe.vmap"), "{out}");
    let note = out["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("reload_map") && note.contains("active"),
        "the active-session note still says how to re-read the file (review-5 M1 keeps the \
         reload_map suggestion for the active case only): {out}"
    );
    // still the same document, and the counter still counts its edit
    assert_eq!(out["unsavedBridgeEdits"], json!(1), "{out}");
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["document"]["id"], json!("d1"), "{status}");
    assert_eq!(status["document"]["path"], json!(DOC_WIN), "{status}");
    assert_eq!(mb.method_calls("map.open").len(), 1);
}

/// Every path shape `bridge/map_path.h` refuses is refused here first: nothing is sent, so a garbled
/// path can never reach `OpenFile`'s modal boxes (which block the editor's main thread). Review M1:
/// only the canonical spelling gets through.
#[tokio::test]
async fn open_map_validates_the_path_locally_and_sends_nothing() {
    let tree = Tree::new("open-badpath");
    let mb = hammer_mock_live(false, vec!["maps/b.vmap"]).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let long = format!("maps/{}.vmap", "a".repeat(505));
    for path in [
        "",
        "   ",
        "probe.vmap",
        "models/probe.vmap",
        "maps/probe.vmdl",
        "maps/.vmap",
        "maps/../probe.vmap",
        "maps/a..b.vmap",
        "/maps/probe.vmap",
        "\\maps\\probe.vmap",
        "C:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap",
        "maps\\probe.vmap",
        "maps/a:b.vmap",
        "maps/a\"b.vmap",
        "maps/a\nb.vmap",
        // review M1: canonical form only (Win32 opens each of these for `maps/probe.vmap`)
        "maps//probe.vmap",
        "maps/a//probe.vmap",
        "maps/./probe.vmap",
        "maps/a/./probe.vmap",
        "maps/a./probe.vmap",
        "maps/a /probe.vmap",
        "maps/ /probe.vmap",
        // review L6: no trimming, so a padded path is a different (refused) path
        " maps/probe.vmap",
        "maps/probe.vmap ",
        long.as_str(),
    ] {
        let err = call(&server, "open_map", json!({"path": path}))
            .await
            .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{path:?}: {err:?}");
        assert_eq!(err.name, "InvalidParams", "{path:?}");
        assert!(err.retry_safe, "{path:?}: nothing was dispatched");
    }
    // a missing `path` is the same refusal, not a `null` sent to the bridge
    let err = call(&server, "open_map", json!({})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");

    assert!(
        mb.method_calls("map.open").is_empty(),
        "local validation refuses before dispatch"
    );
}

/// REVIEW H1: a file this server cannot parse as a binary-v9 `.vmap` never reaches `OpenFile` —
/// live, a load failure pops the modal "Error opening file" on the tools main thread, which wedges
/// the bridge's drain (nobody clicks in the headless setup). The tool parses the whole file with its
/// own reader before anything is sent (`InvalidMapFile`); a real map is dispatched unchanged.
#[tokio::test]
async fn open_map_refuses_a_malformed_vmap_locally() {
    let tree = Tree::new("open-malformed");
    let mb = hammer_mock_live(
        false,
        vec!["maps/keyvalues.vmap", "maps/truncated.vmap", "maps/real.vmap"],
    )
    .await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    // a header a 16-byte prefix cannot tell from the accepted one (`<!-- dmx encodin` is shared)
    tree.write_map(
        "keyvalues.vmap",
        b"<!-- dmx encoding keyvalues2 1 format vmap 40 -->\0{}",
    );
    // the accepted header, but the body is not a readable map
    tree.write_map(
        "truncated.vmap",
        b"<!-- dmx encoding binary 9 format vmap 40 -->\0\x01\x02",
    );
    for path in ["maps/keyvalues.vmap", "maps/truncated.vmap"] {
        let err = call(&server, "open_map", json!({"path": path}))
            .await
            .unwrap_err();
        assert_eq!(err.type_, "InvalidMapFile", "{path}: {err:?}");
        assert_eq!(err.name, "InvalidMapFile", "{path}");
        assert_eq!(err.data["path"], json!(path), "{path}");
        assert!(
            err.retry_safe,
            "nothing was dispatched, so a fixed file can be retried: {path}: {err:?}"
        );
    }
    assert!(
        mb.method_calls("map.open").is_empty(),
        "the local parse refuses before dispatch"
    );

    // a real map passes the same check and opens (the mock has it in `map_files`)
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.write_map("real.vmap", b"")).unwrap();
    let out = call(&server, "open_map", json!({"path": "maps/real.vmap"}))
        .await
        .unwrap();
    assert_eq!(out["opened"], json!(true), "{out}");
    assert_eq!(
        out["path"],
        json!("Z:\\app\\content\\csgo_addons\\mcp_test\\maps\\real.vmap"),
        "{out}"
    );
    assert_eq!(mb.method_calls("map.open").len(), 1);
}

/// review-5 L4 (the H1 residual): a shape-valid `.vmap` the host CANNOT read is refused locally
/// (`AssetNotFound`, nothing dispatched) — the old behaviour silently skipped the whole-file parse
/// for a case spelling difference (`MAPS/Probe.vmap`) or a stale `--path-map`, leaving only the
/// bridge's 512-byte header sniff between a valid-header/truncated-body file and the modal. The
/// bridge resolves the Windows side case-insensitively and the host does not, so the refusal says
/// so.
#[tokio::test]
async fn open_map_refuses_a_map_the_host_cannot_read_locally() {
    let tree = Tree::new("open-missing-host");
    // the mock would open maps/b.vmap, but the tree has NO host file for any path
    let mb = hammer_mock_live(false, vec!["maps/b.vmap", "maps/probe.vmap"]).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    for path in ["maps/b.vmap", "MAPS/PROBE.VMAP"] {
        let err = call(&server, "open_map", json!({"path": path}))
            .await
            .unwrap_err();
        assert_eq!(err.type_, "AssetNotFound", "{path}: {err:?}");
        assert_eq!(err.name, "AssetNotFound", "{path}");
        assert!(err.retry_safe, "nothing was dispatched: {path}: {err:?}");
        assert!(err.message.contains("not readable under the mapped host addon root"), "{path}: {err:?}");
        assert_eq!(err.data["reason"], json!("missing_host_file"), "{path}: {err:?}");
        assert_eq!(err.data["path"], json!(path), "{path}: {err:?}");
    }
    assert!(
        mb.method_calls("map.open").is_empty(),
        "the local host-file gate refuses before dispatch"
    );
}

/// A fake `podman` for the first-map route: one running tools container, a `vtmsend` exec that
/// succeeds (the payload it posts is the `vtm_payload.kv3` file hammer-mcp wrote).
fn first_map_podman(tree: &Tree) -> String {
    let bin = tree.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        "#!/bin/bash\ncase \"$1\" in\n  ps) printf '[{\"Names\":[\"cs2tools\"],\"State\":\"running\"}]' ;;\n  inspect) echo 'mcp_test|null' ;;\n  exec) echo 'vtmsend: creator version 1' ;;\n  *) exit 2 ;;\nesac\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755)).unwrap();
    podman.display().to_string()
}

/// LIVE-P1b finding A: on a fresh session hammer.dll is not loaded, the bridge does not advertise
/// `map.open`, and `open_map` used to answer NotImplemented with no way forward. It now posts
/// `open_asset` (canonical addon-relative path) over the VToolMessage channel and waits for the
/// capability and the document to show up.
#[tokio::test]
async fn open_map_opens_the_first_map_over_the_vtm_post_when_map_open_is_missing() {
    let tree = Tree::new("open-first-vtm");
    let mb = hammer_mock_live(false, vec!["maps/b.vmap"]).await;
    mb.hammer_unloaded();
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.write_map("b.vmap", b"")).unwrap();
    let podman = first_map_podman(&tree);
    let server =
        hammer_mcp::build_server(&config_with(&tree, &mb.discovery_path, &["--podman", &podman])).unwrap();

    // what the engine does once it handled the post: hammer.dll loads and the map opens (a beat
    // after the payload appears, so the first probes see no capability yet)
    let payload = tree.app.join("vtm_payload.kv3");
    let engine = {
        let mb_state = &mb;
        async move {
            let started = std::time::Instant::now();
            while !payload.exists() {
                assert!(started.elapsed() < std::time::Duration::from_secs(10), "no payload was posted");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let text = std::fs::read_to_string(&payload).unwrap();
            assert!(text.contains("_message = \"open_asset\""), "{text}");
            assert!(text.contains("_data = \"maps/b.vmap\""), "canonical addon-relative path: {text}");
            assert!(!text.contains("csgo_addons"), "{text}");
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            mb_state.hammer_open_asset("maps/b.vmap").expect("the mock opens the map");
        }
    };
    let (out, ()) = tokio::join!(call(&server, "open_map", json!({"path": "maps/b.vmap"})), engine);
    let out = out.unwrap();
    assert_eq!(out["opened"], json!(true), "{out}");
    assert_eq!(out["openedVia"], json!("vtm"), "{out}");
    assert_eq!(out["existing"], json!(false), "{out}");
    assert_eq!(out["contentPath"], json!("maps/b.vmap"), "{out}");
    assert!(out["document"]["path"].as_str().unwrap().ends_with("maps\\b.vmap"), "{out}");
    assert_eq!(out["post"]["delivered"], json!(true), "{out}");
    assert!(mb.method_calls("map.open").is_empty(), "the bridge method was never needed: {out}");

    // the next open goes over map.open now that it is advertised
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.write_map("c.vmap", b"")).unwrap();
    mb.with_state(|s| s.hammer.as_mut().unwrap().map_files.push("maps/c.vmap".into()));
    let again = call(&server, "open_map", json!({"path": "maps/c.vmap"})).await.unwrap();
    assert_eq!(again["opened"], json!(true), "{again}");
    assert!(again.get("openedVia").is_none(), "{again}");
    assert_eq!(mb.method_calls("map.open").len(), 1);
}

/// The document never shows up within the call budget: a clear error, the outcome unknown. A second
/// call while the first post is unconfirmed is `OpenPending`: nothing is posted blind again.
#[tokio::test]
async fn open_map_first_map_budget_exhausted_is_an_unknown_outcome_and_blocks_a_second_post() {
    let tree = Tree::new("open-first-budget");
    let mb = hammer_mock_live(false, vec!["maps/b.vmap"]).await;
    mb.hammer_unloaded();
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.write_map("b.vmap", b"")).unwrap();
    let podman = first_map_podman(&tree);
    let mut cfg = config_with(&tree, &mb.discovery_path, &["--podman", &podman]);
    cfg.tool_budget_ms = 5_500;
    let server = hammer_mcp::build_server(&cfg).unwrap();

    let started = std::time::Instant::now();
    let err = call(&server, "open_map", json!({"path": "maps/b.vmap"})).await.unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(10), "bounded by the budget");
    assert_eq!(err.type_, "OpenUnconfirmed", "{err:?}");
    assert!(err.outcome_unknown && !err.retry_safe, "{err:?}");
    assert!(err.message.contains("outcome is unknown") && err.message.contains("modal"), "{err:?}");
    assert!(err.message.contains("session_status"), "{err:?}");
    assert_eq!(err.data["retryWith"], json!("session_status"), "{err:?}");
    let payload = tree.app.join("vtm_payload.kv3");
    assert!(payload.exists(), "the post did go out");

    // REVIEW 4: an agent that sees no document yet and calls open_map again posts nothing
    std::fs::remove_file(&payload).unwrap();
    let again = call(&server, "open_map", json!({"path": "maps/b.vmap"})).await.unwrap_err();
    assert_eq!(again.type_, "OpenPending", "{again:?}");
    assert!(again.retry_safe && !again.outcome_unknown, "{again:?}");
    assert_eq!(again.data["pendingPath"], json!("maps/b.vmap"), "{again:?}");
    assert_eq!(again.data["retryWith"], json!("session_status"), "{again:?}");
    assert!(!payload.exists(), "nothing was posted the second time");
}

/// REVIEW 1 (the blocker): before hammer.dll loads, the REAL bridge answers `doc.active` with
/// `-32017 PrerequisiteFailed {module:"hammer.dll", state:"not_loaded"}` (LIVE-P1b), which the
/// pre-call addon check used to turn into `AddonCheckFailed` for every open_map without a
/// `set_addon` pin. The mock now answers exactly that; no `set_addon` is called here.
#[tokio::test]
async fn open_map_with_no_addon_pin_reaches_the_vtm_route_on_the_real_not_loaded_answer() {
    let tree = Tree::new("open-first-nopin");
    let mb = hammer_mock_live(false, vec!["maps/b.vmap"]).await;
    mb.hammer_unloaded();
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.write_map("b.vmap", b"")).unwrap();
    let podman = first_map_podman(&tree);
    let server =
        hammer_mcp::build_server(&config_with(&tree, &mb.discovery_path, &["--podman", &podman])).unwrap();

    // the mock's answer is the real one
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["documentReason"], json!("hammer not loaded"), "{status}");
    assert_eq!(status["documentError"]["type"], json!("HammerNotLoaded"), "{status}");
    assert_eq!(status["documentError"]["data"]["module"], json!("hammer.dll"), "{status}");

    let payload = tree.app.join("vtm_payload.kv3");
    let engine = {
        let mb = &mb;
        async move {
            let started = std::time::Instant::now();
            while !payload.exists() {
                assert!(started.elapsed() < std::time::Duration::from_secs(10), "no payload was posted");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            mb.hammer_open_asset("maps/b.vmap").expect("the mock opens the map");
        }
    };
    let (out, ()) = tokio::join!(call(&server, "open_map", json!({"path": "maps/b.vmap"})), engine);
    let out = out.unwrap();
    assert_eq!(out["openedVia"], json!("vtm"), "{out}");
    assert_eq!(out["opened"], json!(true), "{out}");
}

/// REVIEW 1: other tools that reach `before_call` while hammer.dll is not loaded behave sensibly:
/// a mutator is told what is missing (NotImplemented / hammer not loaded), never `AddonCheckFailed`,
/// and read-only tools do not crash or mislead.
#[tokio::test]
async fn other_tools_before_the_first_map_do_not_blame_the_addon_check() {
    let tree = Tree::new("unloaded-others");
    let mb = hammer_mock_live(false, vec!["maps/b.vmap"]).await;
    mb.hammer_unloaded();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    for (tool, args) in [
        ("save_map", json!({})),
        ("reload_map", json!({})),
        ("list_models", json!({})),
        ("list_soundevents", json!({})),
        ("get_entity_live", json!({"nodeID": 1})),
        ("list_entities", json!({})),
        ("get_mesh", json!({"nodeID": 1})),
    ] {
        match call(&server, tool, args).await {
            Ok(out) => assert!(
                out.get("error").is_none() && out.get("type").is_none(),
                "{tool} answered an error-shaped success: {out}"
            ),
            Err(err) => {
                assert_ne!(err.type_, "AddonCheckFailed", "{tool}: {err:?}");
                assert_ne!(err.type_, "ProtocolError", "{tool}: {err:?}");
            }
        }
    }
}

/// REVIEW 3: the vtm route is for "hammer.dll is not loaded" only. A LOADED Hammer on a bridge
/// without `map.open` keeps the NotImplemented answer and nothing is posted (`open_asset` would
/// bypass the session-leak guard).
#[tokio::test]
async fn open_map_with_hammer_loaded_but_no_map_open_does_not_take_the_vtm_route() {
    let tree = Tree::new("open-first-loaded");
    let mb = hammer_mock_live(false, vec!["maps/b.vmap"]).await;
    mb.with_state(|s| s.methods.retain(|m| m != "map.open"));
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.write_map("b.vmap", b"")).unwrap();
    let podman = first_map_podman(&tree);
    let server =
        hammer_mcp::build_server(&config_with(&tree, &mb.discovery_path, &["--podman", &podman])).unwrap();
    let err = call(&server, "open_map", json!({"path": "maps/b.vmap"})).await.unwrap_err();
    assert_eq!(err.name, "NotImplemented", "{err:?}");
    assert_eq!(err.data["missing"], json!(["map.open"]), "{err:?}");
    assert!(err.data["hint"].as_str().unwrap().contains("session-leak"), "{err:?}");
    assert!(!tree.app.join("vtm_payload.kv3").exists(), "nothing was posted");
}

/// REVIEW 2: a load of this server that has no verdict yet is the documented crash trigger for a
/// console post; the vtm open takes the same check under the gate and sends nothing.
#[tokio::test]
async fn open_map_first_map_refuses_while_a_run_map_load_is_in_flight() {
    let tree = Tree::new("open-first-busy");
    let mb = hammer_mock_live(false, vec!["maps/b.vmap"]).await;
    mb.hammer_unloaded();
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.write_map("b.vmap", b"")).unwrap();
    let podman = first_map_podman(&tree);
    let mut cfg = config_with(&tree, &mb.discovery_path, &["--podman", &podman]);
    cfg.tool_budget_ms = 8_000;
    let server = hammer_mcp::build_server(&cfg).unwrap();
    *server.state().last_load.lock().unwrap() = Some(hammer_mcp::tools::run_map::LastLoad {
        name: "probe".into(),
        vpk_mtime_ms: None,
        console_offset: 0,
        resolved: false,
        state: "timeout".into(),
        idle: false,
        in_flight_until: std::time::Instant::now() + std::time::Duration::from_secs(600),
    });
    let err = call(&server, "open_map", json!({"path": "maps/b.vmap"})).await.unwrap_err();
    assert_eq!(err.type_, "EngineBusy", "{err:?}");
    assert!(err.retry_safe, "{err:?}");
    assert!(!tree.app.join("vtm_payload.kv3").exists(), "nothing was posted");
}

/// REVIEW 5: the budget bounds. Too little budget left to see the open through is refused with
/// nothing sent; a vtm gate held for the whole budget is a retry-safe BridgeBusy, not a hang.
#[tokio::test]
async fn open_map_first_map_budget_bounds() {
    let tree = Tree::new("open-first-bounds");
    let mb = hammer_mock_live(false, vec!["maps/b.vmap"]).await;
    mb.hammer_unloaded();
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.write_map("b.vmap", b"")).unwrap();
    let podman = first_map_podman(&tree);
    let mut cfg = config_with(&tree, &mb.discovery_path, &["--podman", &podman]);
    cfg.tool_budget_ms = 2_000;
    let server = hammer_mcp::build_server(&cfg).unwrap();
    let err = call(&server, "open_map", json!({"path": "maps/b.vmap"})).await.unwrap_err();
    assert_eq!(err.type_, "BudgetTooLow", "{err:?}");
    assert!(err.retry_safe, "{err:?}");
    assert!(!tree.app.join("vtm_payload.kv3").exists(), "nothing was posted");
    drop(server); // the mock bridge serves one client at a time

    let mut cfg = config_with(&tree, &mb.discovery_path, &["--podman", &podman]);
    cfg.tool_budget_ms = 5_500;
    let server = hammer_mcp::build_server(&cfg).unwrap();
    let held = server.state().vtm_gate.clone().lock_owned().await;
    let started = std::time::Instant::now();
    let err = call(&server, "open_map", json!({"path": "maps/b.vmap"})).await.unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(9), "bounded by the budget");
    drop(held);
    assert!(
        matches!(err.type_.as_str(), "BridgeBusy" | "BudgetTooLow"),
        "{err:?}"
    );
    assert!(err.retry_safe, "{err:?}");
    assert!(!tree.app.join("vtm_payload.kv3").exists(), "nothing was posted");
}

/// No tools container to post through: the old NotImplemented, now with the way forward.
#[tokio::test]
async fn open_map_without_map_open_and_without_a_container_says_how_to_open_the_first_map() {
    let tree = Tree::new("open-first-nocontainer");
    let mb = hammer_mock_live(false, vec!["maps/b.vmap"]).await;
    mb.hammer_unloaded();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let err = call(&server, "open_map", json!({"path": "maps/b.vmap"})).await.unwrap_err();
    assert_eq!(err.name, "NotImplemented", "{err:?}");
    assert_eq!(err.data["missing"], json!(["map.open"]), "{err:?}");
    assert!(err.data["hint"].as_str().unwrap().contains("runtime.sh"), "{err:?}");
}

/// LIVE-P1 bug 4: a map of ANOTHER addon passed the precheck (it looked under the selected addon)
/// while the bridge resolves under the engine's `-addon`. Now refused up front, naming the
/// engine's addon and how to get a session on the other one; nothing is sent.
#[tokio::test]
async fn open_map_of_another_addon_is_refused_up_front_naming_the_engine_addon() {
    let tree = Tree::new("open-other-addon");
    let other = tree.app.join("content/csgo_addons/other_addon/maps");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::copy(Tree::fixture_map("probe.vmap"), other.join("x.vmap")).unwrap();
    let mb = hammer_mock_live(false, vec!["maps/x.vmap"]).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    call(&server, "set_addon", json!({"addon": "other_addon"})).await.unwrap();
    let err = call(&server, "open_map", json!({"path": "maps/x.vmap"})).await.unwrap_err();
    assert_eq!(err.type_, "WrongEngineAddon", "{err:?}");
    assert!(err.retry_safe, "{err:?}");
    assert!(err.message.contains("\"mcp_test\"") && err.message.contains("ADDON=other_addon"), "{err:?}");
    assert_eq!(err.data["engineAddon"], json!("mcp_test"), "{err:?}");
    assert_eq!(err.data["selectedAddon"], json!("other_addon"), "{err:?}");
    assert_eq!(err.data["engineAddonSource"], json!("flag"), "no container to ask: {err:?}");
    assert!(mb.method_calls("map.open").is_empty(), "refused before dispatch");

    // a map the engine's addon does have still opens although another addon is selected
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.write_map("x.vmap", b"")).unwrap();
    let out = call(&server, "open_map", json!({"path": "maps/x.vmap"})).await.unwrap();
    assert_eq!(out["opened"], json!(true), "{out}");
}

/// REVIEW: the engine's addon is the tools container's `cs2mcp.addon` label, not `--addon`.
/// Here `--addon` says `mcp_test` but the runtime was started on `other_addon`.
#[tokio::test]
async fn open_map_takes_the_engine_addon_from_the_container_not_the_flag() {
    let tree = Tree::new("open-engine-label");
    let other = tree.app.join("content/csgo_addons/other_addon/maps");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::copy(Tree::fixture_map("probe.vmap"), other.join("x.vmap")).unwrap();
    let bin = tree.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        "#!/bin/bash\ncase \"$1\" in\n  ps) printf '[{\"Names\":[\"cs2tools\"],\"State\":\"running\"}]' ;;\n  inspect) echo 'other_addon|null' ;;\n  *) exit 2 ;;\nesac\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755)).unwrap();
    let podman = podman.display().to_string();
    let mb = hammer_mock_live(false, vec!["maps/x.vmap"]).await;
    let server = hammer_mcp::build_server(&config_with(&tree, &mb.discovery_path, &["--podman", &podman])).unwrap();

    // a map only the engine's addon has opens although --addon names another one
    let out = call(&server, "open_map", json!({"path": "maps/x.vmap"})).await.unwrap();
    assert_eq!(out["opened"], json!(true), "{out}");
    assert_eq!(out["engineAddon"], json!("other_addon"), "{out}");
    assert_eq!(out["engineAddonSource"], json!("container"), "{out}");
    assert!(out["notes"][0].as_str().unwrap().contains("other_addon"), "{out}");

    // a map only in the --addon tree is refused up front (the bridge would not find it)
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.write_map("y.vmap", b"")).unwrap();
    let err = call(&server, "open_map", json!({"path": "maps/y.vmap"})).await.unwrap_err();
    assert_eq!(err.type_, "WrongEngineAddon", "{err:?}");
    assert_eq!(err.data["engineAddon"], json!("other_addon"), "{err:?}");
    assert_eq!(err.data["engineAddonSource"], json!("container"), "{err:?}");
}

/// `opened:false` without `existing` is `OpenFile` answering 0 (the re-entrancy guard, or a load
/// failure): the same `OpenFailed` error `model_open`/`particle_open` raise, but **not** retry-safe
/// (review H1: the modal is what a retry pops again, and the tool cannot tell the two causes apart).
#[tokio::test]
async fn open_map_reports_open_failed_when_the_editor_did_not_open() {
    let tree = Tree::new("open-failed");
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.write_map("b.vmap", b"")).unwrap();
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(nodes())
        .hammer_map_files(vec!["maps/b.vmap"])
        .responder_value(
            "map.open",
            json!({"opened": false, "existing": false, "path": "maps/b.vmap",
                   "reason": "open_failed", "epoch": 3}),
        )
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let err = call(&server, "open_map", json!({"path": "maps/b.vmap"}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "OpenFailed");
    assert_eq!(err.name, "OpenFailed");
    assert!(
        !err.retry_safe,
        "a retry would pop the same modal: {err:?}"
    );
    assert!(
        err.message.contains("OpenFile returned 0"),
        "the refusal names the cause: {}",
        err.message
    );
    assert_eq!(err.data["reason"], json!("open_failed"), "{err:?}");
}

/// REVIEW M2: live, the app keeps EVERY session in its vector, so opening A, then B, then A again
/// answers `existing:true, active:false` (A's session is in the background) and nothing in this
/// server moves — the active document is still B and the counter still belongs to it. The mock used
/// to model a single session and answered `opened:true` for the second A, so this branch was never
/// tested. A is the document the world starts with (`probe.vmap`), B is a second file.
#[tokio::test]
async fn open_map_of_a_background_session_reports_existing_and_not_active() {
    let tree = Tree::new("open-background");
    let mb = hammer_mock_live(false, vec!["maps/b.vmap"]).await;
    // review-5 L4: the pre-parse reads the host file now, so both maps have to be on the host
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.write_map("b.vmap", b"")).unwrap();
    std::fs::copy(Tree::fixture_map("probe.vmap"), tree.map()).unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    // A -> B: a new session, B is now the active document
    let out = call(&server, "open_map", json!({"path": "maps/b.vmap"}))
        .await
        .unwrap();
    assert_eq!(out["opened"], json!(true), "{out}");
    let b_id = out["document"]["id"].clone();
    assert_ne!(b_id, json!("d1"), "{out}");

    // B -> A: A's session is still there, in the background
    let out = call(&server, "open_map", json!({"path": "maps/probe.vmap"}))
        .await
        .unwrap();
    assert_eq!(out["opened"], json!(false), "{out}");
    assert_eq!(out["existing"], json!(true), "{out}");
    assert_eq!(out["active"], json!(false), "A's session is not the active one: {out}");
    assert_eq!(out["path"], json!(DOC_WIN), "{out}");
    assert_eq!(out["contentPath"], json!("maps/probe.vmap"), "{out}");
    // review-5 M1, NEGATIVE evidence: the background-session note must not name reload_map at all
    // (it acts on the active document — a different map — and a forced reload there discards its
    // edits), and must say what to do instead
    let note = out["note"].as_str().unwrap_or_default();
    assert!(
        !note.contains("reload_map"),
        "the {{active:false}} note must not mention reload_map (it acts on the active document, \
         a different map, and a forced reload there discards its edits): {note}"
    );
    assert!(
        note.contains("background") && (note.contains("switch") || note.contains("tabs")),
        "the note tells the caller what to do instead: {note}"
    );
    // live-fix C item D: plain statement, the active document named, and a next step that only
    // uses tools that exist (file reads by path; the user switches tabs)
    assert!(note.contains("NOT the active document") && note.contains("list_entities"), "{note}");
    assert!(note.contains("ask the user") && note.contains("PASS path"), "{note}");
    // the steps are structured (the registry checks the tool names), and carry the path
    let next = out["next"].as_array().unwrap_or_else(|| panic!("structured next: {out}"));
    assert_eq!(next[0]["tool"], json!("list_entities"), "{out}");
    assert_eq!(next[0]["args"]["path"], json!("maps/probe.vmap"), "{out}");
    assert!(next.iter().any(|step| step["tool"] == json!("session_status")), "{out}");
    assert!(out["activeDocument"].as_str().unwrap().ends_with("maps\\b.vmap"), "{out}");
    assert!(
        out.get("document").is_none(),
        "nothing opened, so nothing is re-read: {out}"
    );
    assert_eq!(out["unsavedBridgeEdits"], json!(0), "{out}");
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["document"]["id"], b_id, "B is still the active document: {status}");

    // and B's session is still the active one (active:true, not false)
    let out = call(&server, "open_map", json!({"path": "maps/b.vmap"}))
        .await
        .unwrap();
    assert_eq!(out["existing"], json!(true), "{out}");
    assert_eq!(out["active"], json!(true), "{out}");
    assert_eq!(out["path"], json!("Z:\\app\\content\\csgo_addons\\mcp_test\\maps\\b.vmap"), "{out}");
    assert!(
        out["note"].as_str().unwrap_or_default().contains("reload_map"),
        "B's own session is the active one, so THIS note still suggests reload_map: {out}"
    );
}

// -------------------------------------------------------------------------------------------
// reload_map (map.reload; live behaviour spec: bridge/test_reload.py)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn reload_map_on_a_clean_document_reloads_and_reports_the_host_path() {
    let tree = Tree::new("reload-clean");
    std::fs::write(tree.map(), b"vmap").unwrap();
    let mb = hammer_mock(nodes(), false, true, false).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let out = call(&server, "reload_map", json!({})).await.unwrap();
    assert_eq!(out["reloaded"], json!(true));
    assert_eq!(out["discardedChanges"], json!(false), "the document was clean");
    assert_eq!(out["path"], json!(DOC_WIN));
    assert_eq!(out["hostPath"], json!(tree.map().display().to_string()));
    assert!(out["epoch"].is_i64(), "bridge result fields pass through: {out}");
    assert_eq!(out["unsavedBridgeEdits"], json!(0));

    let sent = &mb.method_calls("map.reload")[0].params;
    assert_eq!(sent["force"], json!(false), "{sent}");
    assert_eq!(
        sent["expectDoc"],
        json!({"id": "d1", "epoch": 1}),
        "map.reload sends the doc.active fingerprint (review L2): {sent}"
    );
    assert!(sent["deadlineMs"].as_u64().unwrap_or(0) > 0, "{sent}");
}

/// FIX-3B L2: the request carries the `doc.active` fingerprint, so when the document moved between
/// the check and the reload the bridge answers `-32004 DocumentChanged` — the tool surfaces it as
/// its own error (retry-safe: the reload did not run) instead of reporting a reload that never
/// happened. Negative evidence: the unsaved-edit counter is not reset (nothing was reloaded), so
/// the map still differs from the file.
#[tokio::test]
async fn reload_map_surfaces_a_document_changed_refusal_and_keeps_the_counter() {
    let tree = Tree::new("reload-changed");
    std::fs::write(tree.map(), b"vmap").unwrap();
    let mb = hammer_mock(nodes(), false, true, false).await;
    // the bridge's own expectDoc refusal (the arm checks it after editable, before untitled/dirty)
    mb.add_error(
        "map.reload",
        -32004,
        "DocumentChanged",
        Some(json!({"reason": "doc_id"})),
    );
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    // one counted edit, so a wrong `reset_edits` would be visible
    call(&server, "delete_nodes", json!({"nodeIDs": [30]})).await.unwrap();
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["server"]["unsavedBridgeEdits"], json!(1));

    let err = call(&server, "reload_map", json!({"force": true})).await.unwrap_err();
    assert_eq!(err.type_, "DocumentChanged");
    assert_eq!(err.name, "DocumentChanged");
    assert_eq!(err.code, Some(-32004));
    assert!(err.retry_safe, "the reload did not run: {err:?}");
    assert_eq!(err.data["reason"], json!("doc_id"));

    let sent = &mb.method_calls("map.reload")[0].params;
    assert!(sent["expectDoc"].is_object(), "the gate needs a fingerprint to check: {sent}");
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(
        status["server"]["unsavedBridgeEdits"],
        json!(1),
        "a refused reload does not reset the counter: {status}"
    );
}

/// The bridge answers `-32016 UnsavedChanges` for `dirty && !force`; the tool refuses locally
/// first (nothing is sent, the document state is untouched) and mentions `force:true`.
#[tokio::test]
async fn reload_map_refuses_a_dirty_document_without_force_and_sends_nothing() {
    let tree = Tree::new("reload-dirty");
    std::fs::write(tree.map(), b"vmap").unwrap();
    let mb = hammer_mock(nodes(), true, true, false).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    // one counted edit so the refusal can name the counter
    call(&server, "delete_nodes", json!({"nodeIDs": [30]})).await.unwrap();
    assert_eq!(node_ids(&mb), vec![1, 9], "setup: node 30 deleted");

    let err = call(&server, "reload_map", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "UnsavedChanges");
    assert_eq!(err.name, "UnsavedChanges");
    assert_eq!(err.code, Some(-32016));
    assert!(err.retry_safe, "nothing was dispatched: {err:?}");
    assert!(
        err.message.contains("force:true"),
        "the refusal says how to proceed: {}",
        err.message
    );
    assert!(
        err.message.contains("1 unsaved bridge edits"),
        "the refusal mentions the MCP unsaved-edit counter: {}",
        err.message
    );
    assert_eq!(err.data["unsavedBridgeEdits"], json!(1));
    assert_eq!(err.data["reason"], json!("document is dirty; pass force:true to discard"));

    // nothing was sent and nothing changed: the world keeps its state
    assert!(
        mb.method_calls("map.reload").is_empty(),
        "the refusal must happen before dispatch"
    );
    assert_eq!(node_ids(&mb), vec![1, 9], "refusing discarded nothing");
}

/// `dirty` is unreported (missing/non-boolean) counts as unknown: refuse without `force` (the
/// tool description's "refuses while dirty *or dirty unknown*"), `force:true` unlocks it.
#[tokio::test]
async fn reload_map_refuses_a_dirty_unknown_document_and_force_unlocks_it() {
    let tree = Tree::new("reload-unknown");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, None)))
        .hammer_nodes(vec![MockNode::world()])
        .responder_value("doc.active", json!({"doc": make_doc(DOC_WIN, None)}))
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let err = call(&server, "reload_map", json!({})).await.unwrap_err();
    assert_eq!(err.name, "UnsavedChanges");
    assert_eq!(err.code, Some(-32016));
    assert!(
        err.data["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("dirty state unreported"),
        "{err:?}"
    );
    assert!(mb.method_calls("map.reload").is_empty());

    let out = call(&server, "reload_map", json!({"force": true})).await.unwrap();
    assert_eq!(out["reloaded"], json!(true), "{out}");
    assert_eq!(mb.method_calls("map.reload").len(), 1);
}

/// On success the map equals the file again: the unsaved-edit counter is reset to 0 and
/// `session_status` reports it (negative evidence: this test fails without the reset).
#[tokio::test]
async fn forced_reload_discards_the_changes_and_resets_the_counter() {
    let tree = Tree::new("reload-force");
    std::fs::write(tree.map(), b"vmap").unwrap();
    let mb = hammer_mock(nodes(), true, true, false).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let delete = call(&server, "delete_nodes", json!({"nodeIDs": [30]})).await.unwrap();
    assert_eq!(delete["unsavedBridgeEdits"], json!(1));

    let out = call(&server, "reload_map", json!({"force": true})).await.unwrap();
    assert_eq!(out["reloaded"], json!(true));
    assert_eq!(out["discardedChanges"], json!(true), "the document was dirty");
    assert!(out["epoch"].is_i64());
    assert_eq!(out["unsavedBridgeEdits"], json!(0), "{out}");

    // the reset is server state, not just the reply field
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["server"]["unsavedBridgeEdits"], json!(0), "{status}");
    assert_eq!(status["unsavedWarning"], Value::Null);
}

/// FIX-3R M1: a new/untitled document (`isNewFile:true` or an empty/missing `path`) has no file
/// behind it, so `reload_map` refuses locally with `-32015 UntitledDocument` — the same error the
/// bridge/mock answer for `map.save` — and sends nothing, `force` or not.
#[tokio::test]
async fn reload_map_refuses_an_untitled_document_and_sends_nothing() {
    let tree = Tree::new("reload-untitled");
    let mb = hammer_mock(nodes(), false, true, true).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    // one counted edit so the refusal can also name the counter
    let deleted = call(&server, "delete_nodes", json!({"nodeIDs": [30]})).await.unwrap();
    assert_eq!(deleted["unsavedBridgeEdits"], json!(1));

    for args in [json!({}), json!({"force": true})] {
        let err = call(&server, "reload_map", args.clone()).await.unwrap_err();
        assert_eq!(err.type_, "UntitledDocument", "{args}: {err:?}");
        assert_eq!(err.name, "UntitledDocument");
        assert_eq!(err.code, Some(-32015));
        assert!(err.retry_safe, "nothing was dispatched: {err:?}");
        assert!(
            err.message.contains("untitled") && err.message.contains("save"),
            "the refusal says what to do instead: {}",
            err.message
        );
        assert_eq!(err.data["path"], Value::Null, "the document has no path");
        assert_eq!(err.data["unsavedBridgeEdits"], json!(1));

        // refused before dispatch, again even with force, and the counter was not reset either
        assert!(
            mb.method_calls("map.reload").is_empty(),
            "{args}: the refusal must happen before dispatch"
        );
        let status = call(&server, "session_status", json!({})).await.unwrap();
        assert_eq!(status["server"]["unsavedBridgeEdits"], json!(1), "{status}");
    }
}

/// FIX-3R M1: the unsaved-edit counter resets only when the reply says `reloaded:true`; any other
/// success shape is treated as maybe-executed — the counter is kept and a note says the reload
/// may not have taken.
#[tokio::test]
async fn reload_map_keeps_the_counter_when_the_reply_has_no_reloaded() {
    let tree = Tree::new("reload-shape");
    std::fs::write(tree.map(), b"vmap").unwrap();
    // a success reply with no `reloaded` at all (a mock-only shape; the real bridge answers it
    // today — that is the M1 finding itself — but the tool must not depend on it)
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .hammer_editable(true)
        .responder_value("map.reload", json!({"epoch": 9}))
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let delete = call(&server, "delete_nodes", json!({"nodeIDs": [30]})).await.unwrap();
    assert_eq!(delete["unsavedBridgeEdits"], json!(1));

    let out = call(&server, "reload_map", json!({"force": true})).await.unwrap();
    assert!(out["reloaded"].is_null(), "the reply had no reloaded flag: {out}");
    assert!(out["epoch"].is_i64(), "the other reply fields pass through: {out}");
    assert_eq!(
        out["unsavedBridgeEdits"],
        json!(1),
        "an unconfirmed reload keeps the counter: {out}"
    );
    let notes: Vec<String> = out["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .filter_map(|note| note.as_str().map(str::to_string))
        .collect();
    assert!(
        notes.iter().any(|note| note.contains("reloaded:true")
            && note.contains("counter is kept")),
        "{notes:?}"
    );
    // the counter is server state, not just the output field
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["server"]["unsavedBridgeEdits"], json!(1), "{status}");
}

/// FIX-3R L3: `unsaved` is read **after** `require_document` — reading `doc.active` already
/// resets the server's counter on a document switch (`note_document`), so the refusal must name
/// this document's count, not the previous document's. The server's state is not reachable
/// through `build_server`, so the state is built directly (the pattern of
/// `tests/entities_file.rs`) with a planted stale counter + document id.
#[tokio::test]
async fn reload_map_refusal_carries_the_current_documents_counter() {
    let tree = Tree::new("reload-unsaved");
    std::fs::write(tree.map(), b"vmap").unwrap();
    let mb = hammer_mock(nodes(), true, true, false).await;
    let config = config(&tree, &mb.discovery_path);
    let state = HammerState::from_config(&config).unwrap();
    // a counter left over from another document: require_document -> note_document("d1") resets it
    {
        let mut edits = state.edits.lock().unwrap();
        edits.since_save = 3;
        edits.doc_id = Some("other".to_string());
    }

    let err = hammer_mcp::tools::lifecycle::reload_map(
        state,
        CallCtx {
            budget: Budget::new(45_000, 5_000),
        },
        json!({}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, Some(-32016), "{err:?}");
    assert_eq!(
        err.data["unsavedBridgeEdits"],
        json!(0),
        "the stale count of the *previous* document must not be reported: {err:?}"
    );
    assert!(
        !err.message.contains("3 unsaved"),
        "the refusal must not carry the previous document's count: {}",
        err.message
    );
}

/// No active document: the same lifecycle taxonomy as `save_map` (`NoDocument`, -32003, the
/// bridge's wire error name is `NoActiveDocument`), nothing dispatched.
#[tokio::test]
async fn reload_map_with_no_active_document_is_no_document() {
    let tree = Tree::new("reload-nodoc");
    let mb = MockBridge::builder()
        .hammer_nodes(vec![MockNode::world()])
        .responder_value("doc.active", json!({"doc": null, "reason": "no_document"}))
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let err = call(&server, "reload_map", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "NoDocument");
    assert_eq!(err.name, "NoDocument");
    assert_eq!(err.code, Some(-32003));
    assert!(err.retry_safe);
    assert!(mb.method_calls("map.reload").is_empty());
}

// -------------------------------------------------------------------------------------------
// Review-1 L2: a save/reload while a compile of the SAME map runs
// -------------------------------------------------------------------------------------------

/// A `compile_map` job entry: what the save/reload note's lookup reads (`state`, `path`, `id`).
fn running_compile(state: &HammerState, id: &str, path: &str) {
    state
        .compile_jobs
        .insert(hammer_mcp::tools::compile_map::CompileJob {
            id: id.to_string(),
            tag: hammer_mcp::tools::compile_map::tag_value(id),
            state: hammer_mcp::tools::compile_map::JobState::Running,
            mode: hammer_mcp::tools::compile_map::CompileMode::Full,
            path: path.to_string(),
            addon: "mcp_test".to_string(),
            container: "cs2tools".to_string(),
            log: PathBuf::from("/nonexistent/rc.log"),
            vpk: vec![],
            vmap_before: None,
            started: std::time::Instant::now(),
            deadline: std::time::Duration::from_secs(1500),
            attempts: 1,
            summary: None,
            ok_line: None,
            reason: None,
            cleanup: None,
        });
}

/// Review-1 L2: a `save_map` while a `compile_map` job of that map runs is a NOTE — the compiler
/// may reread the `.vmap` for its later passes (lighting), so the job's output need not match the
/// file this save wrote. It is never a refusal: the save is the caller's right.
#[tokio::test]
async fn save_map_notes_a_running_compile_of_the_same_map() {
    let tree = Tree::new("save-during-compile");
    std::fs::write(tree.map(), b"vmap").unwrap();
    let mb = hammer_mock(nodes(), true, true, false).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    // no job at all: no note
    let out = call(&server, "save_map", json!({})).await.unwrap();
    assert_eq!(out["saved"], json!(true));
    assert!(out["notes"].is_null(), "{out}");
    // a compile of ANOTHER map: still no note (only the same map is at risk)
    running_compile(server.state(), "c1", "maps/other.vmap");
    let out = call(&server, "save_map", json!({})).await.unwrap();
    assert!(out["notes"].is_null(), "{out}");
    // a compile of THIS map: the note, and the save still happened
    running_compile(server.state(), "c2", "maps/probe.vmap");
    let out = call(&server, "save_map", json!({})).await.unwrap();
    assert_eq!(out["saved"], json!(true), "the save is not refused: {out}");
    assert_eq!(out["unsavedBridgeEdits"], json!(0));
    let notes = out["notes"].as_array().expect("the L2 note");
    let note = notes[0].as_str().unwrap();
    assert!(
        note.contains("compile_map job (c2)") && note.contains("maps/probe.vmap"),
        "the note names the job and the map: {note}"
    );
    assert!(
        note.contains("re-run compile_map"),
        "and what to do about it: {note}"
    );
    // the id is the RUNNING one (c1 is a different map)
    assert!(!note.contains("c1"), "{note}");
}

/// Review-1 L2 for `reload_map`: the same note (the document is replaced from the file the running
/// compiler reads).
#[tokio::test]
async fn reload_map_notes_a_running_compile_of_the_same_map() {
    let tree = Tree::new("reload-during-compile");
    std::fs::write(tree.map(), b"vmap").unwrap();
    let mb = hammer_mock(nodes(), false, true, false).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();

    let out = call(&server, "reload_map", json!({})).await.unwrap();
    assert_eq!(out["reloaded"], json!(true));
    assert!(out["notes"].is_null(), "{out}");

    running_compile(server.state(), "c5", "maps/probe.vmap");
    let out = call(&server, "reload_map", json!({})).await.unwrap();
    assert_eq!(out["reloaded"], json!(true), "the reload is not refused: {out}");
    let notes = out["notes"].as_array().expect("the L2 note");
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap().contains("compile_map job (c5)")),
        "{notes:?}"
    );
}


#[tokio::test]
async fn a_save_the_bridge_reports_as_not_saved_does_not_point_at_compile_map() {
    let tree = Tree::new("save-not-saved");
    std::fs::write(tree.map(), b"vmap").unwrap();
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .responder_value("map.save", json!({"saved": false, "skippedClean": false, "path": DOC_WIN}))
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let out = call(&server, "save_map", json!({})).await.unwrap();
    assert_eq!(out["saved"], json!(false));
    assert!(out.get("next").is_none(), "nothing was saved, so nothing to compile: {out}");
}

#[tokio::test]
async fn an_edit_in_an_untitled_document_hints_instead_of_pointing_at_save_map() {
    let tree = Tree::new("edit-untitled");
    let mb = hammer_mock(nodes(), true, true, true).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let out = call(&server, "delete_nodes", json!({"nodeIDs": [30]})).await.unwrap();
    assert_eq!(out["deleted"], json!([30]));
    assert!(out.get("next").is_none(), "save_map always refuses a new document: {out}");
    assert!(out["hint"].as_str().unwrap().contains("untitled"), "{out}");
}
