//! `entities_live` tool tests (WP-T2a): `create_entities` (`edit.batch` of `entity.create`),
//! `set_entity_keyvalues` (`entity.get` + `entity.setKeyValues`) and `undo_last` against the mock
//! bridge's Hammer world (WP-M built-ins) plus a real FGD database.
//!
//! **FGD fixture.** The tools load the class database from `<project_root>/workshop/run/app/game`
//! (`fgd::load_for`), so every tree here copies `hammer-mcp/fixtures/fgd` into
//! `<CARGO_TARGET_TMPDIR>/<tag>/workshop/run/app/game`. A missing fixture fails loudly. The classes
//! used in tests exist in **both** the fixture FGD (`csgo/csgo.fgd`) and the mock's
//! `ENTITY_CLASSES` (`info_target`, `func_buyzone`); `weapon_ak47` is in the FGD only, which is how
//! the mock's `-32006 UnknownClass` -> `-32012 PartialFailure` case is produced.
//!
//! **`doc.active`.** Unlike the sibling suites this one installs no responder: the mock's Hammer
//! world serves `{"doc": {...}}` itself, with the live `epoch`/`dirty`/`undoName`/`undoDepth`
//! (`crates/cs2-mcp-common/tests/mock_entities.rs` pins that shape). The one test that needs a foreign undo
//! group overrides `doc.active` with a `responder_value`, which the mock's knobs apply first.
//!
//! Helpers (`Tree`, `config`, `call`) follow the pattern in `tests/lifecycle.rs`.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The document path the mock serves (Windows form).
const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";

/// The FGD fixture tree copied into every test's project root.
const FIXTURE_FGD: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/fgd");

/// The default `undoLabel` label mock/bridge use for a batch group (what the bridge stores).
const BATCH_UNDO_LABEL: &str = "MCP: batch";

struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let tree = Tree::empty(tag);
        copy_tree(Path::new(FIXTURE_FGD), &tree.game());
        assert!(
            tree.game().join("csgo/csgo.fgd").is_file(),
            "the FGD fixture copy is incomplete: {}",
            tree.game().display()
        );
        tree
    }

    /// A tree whose `<project>/workshop/run/app/game` has no `csgo/csgo.fgd`: the
    /// "fgd unavailable" warning path (never a refusal).
    fn without_fgd(tag: &str) -> Tree {
        let tree = Tree::empty(tag);
        assert!(
            !tree.game().join("csgo/csgo.fgd").exists(),
            "the no-FGD tree must not contain a database"
        );
        tree
    }

    fn empty(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "entities-live-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("tmp tree");
        Tree { root }
    }

    /// `<project>/workshop/run/app/game`: what `fgd::load_for` reads.
    fn game(&self) -> PathBuf {
        self.root.join("workshop/run/app/game")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Recursive copy (fixture or tree); a missing source fails the test loudly.
fn copy_tree(from: &Path, to: &Path) {
    let entries = std::fs::read_dir(from)
        .unwrap_or_else(|e| panic!("fixture {} unreadable: {e}", from.display()));
    std::fs::create_dir_all(to).unwrap_or_else(|e| panic!("{}: {e}", to.display()));
    for entry in entries {
        let entry = entry.expect("fixture entry");
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &dest);
        } else {
            std::fs::copy(&path, &dest)
                .unwrap_or_else(|e| panic!("copy {} -> {}: {e}", path.display(), dest.display()));
        }
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
    let app = tree.root.join("workshop/run/app").display().to_string();
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

/// The world every test starts from: nodeID 30 `info_target`, nodeID 45 `func_buyzone` (both in the
/// fixture FGD and in the mock's class registry), plus a mesh that is not an entity.
fn nodes() -> Vec<MockNode> {
    vec![
        MockNode::world(),
        MockNode::entity(
            30,
            "info_target",
            [1.0, 2.0, 3.0],
            kv(&[("targetname", "t30")]),
        ),
        MockNode::entity(
            45,
            "func_buyzone",
            [4.0, 5.0, 6.0],
            kv(&[("TeamNum", "2")]),
        ),
        mesh(9),
    ]
}

/// A Hammer world (WP-M) with the built-in `doc.active` (live epoch/dirty/undo state) and a dirty
/// editable document; no `doc.active` responder.
async fn hammer_mock(world_nodes: Vec<MockNode>) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(world_nodes)
        .start()
        .await
        .unwrap()
}

fn node_ids(mb: &MockBridge) -> Vec<i64> {
    let mut ids: Vec<i64> = mb.hammer_nodes().iter().map(|n| n.node_id).collect();
    ids.sort();
    ids
}

fn server(tree: &Tree, mb: &MockBridge) -> McpServer<HammerState> {
    hammer_mcp::build_server(&config(tree, &mb.discovery_path)).unwrap()
}

// -------------------------------------------------------------------------------------------
// Acceptance: create_entities
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn create_entities_batches_creates_and_never_returns_a_handle() {
    let tree = Tree::new("create");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_entities",
        json!({
            "entities": [
                {"class": "info_target", "origin": [10, 20, 30],
                 "keyValues": {"targetname": "spawn_a", "spawnflags": 1}},
                {"class": "func_buyzone", "origin": [1.5, 2.5, 3.5], "angles": [0, 90, 0],
                 "keyValues": {"BuyzoneRadius": 64}}
            ],
            "undoLabel": "unit test"
        }),
    )
    .await
    .unwrap();

    assert_eq!(out["count"], json!(2));
    assert_eq!(out["requested"], json!(2));
    // the bridge's own group label; the requested one is only echoed (m2.15 edit.batch takes none)
    assert_eq!(out["undoLabel"], json!(BATCH_UNDO_LABEL));
    assert_eq!(out["requestedUndoLabel"], json!("unit test"));
    assert_eq!(out["failedIndex"], Value::Null);
    assert_eq!(out["saved"], json!(false));
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
    assert_eq!(out["fgd"]["files"], json!(2), "csgo.fgd + base_mini.fgd");
    assert_eq!(out["warnings"], json!([]));

    let entities = out["entities"].as_array().expect("entities");
    assert_eq!(entities.len(), 2);
    assert_eq!(entities[0]["index"], json!(0));
    assert_eq!(entities[0]["class"], json!("info_target"));
    assert_eq!(entities[0]["origin"], json!([10.0, 20.0, 30.0]));
    assert_eq!(entities[0]["handleIssued"], json!(true));
    // review W2 3a: the bridge's create reply carries the created node's id, so the result names
    // the new entities instead of leaving the caller to find them by a unique targetname
    assert_eq!(entities[0]["nodeID"], json!(46), "{out}");
    assert_eq!(entities[1]["index"], json!(1));
    assert_eq!(entities[1]["class"], json!("func_buyzone"));
    assert_eq!(entities[1]["handleIssued"], json!(true));
    assert_eq!(entities[1]["nodeID"], json!(47), "{out}");
    for entity in entities {
        assert!(entity.get("handle").is_none(), "{entity}");
    }
    assert!(!out.to_string().contains("\"handle\""), "{out}");

    // the mock world really has them (next free id after the fixture's max is 46, then 47)
    assert_eq!(node_ids(&mb), vec![1, 9, 30, 45, 46, 47]);
    let created = mb
        .hammer_nodes()
        .into_iter()
        .find(|n| n.node_id == 46)
        .expect("node 46");
    assert_eq!(created.classname, "info_target");
    assert_eq!(created.keyvalues["targetname"], "spawn_a");
    assert_eq!(created.origin, [10.0, 20.0, 30.0]);

    // exactly one edit.batch, with the document fingerprint and a real deadline
    let calls = mb.method_calls("edit.batch");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].params["expectDoc"]["id"], json!("d1"));
    assert!(calls[0].params["expectDoc"]["epoch"].is_i64());
    assert!(calls[0].params["deadlineMs"].as_u64().unwrap_or(0) > 0);
    // angles and coerced keyvalues are sent (the m2.15 create result does not echo them back)
    assert_eq!(
        calls[0].params["ops"][1]["params"]["angles"],
        json!([0.0, 90.0, 0.0])
    );
    assert_eq!(
        calls[0].params["ops"][0]["params"]["keyValues"]["spawnflags"],
        json!("1")
    );
}

#[tokio::test]
async fn create_entities_sends_numbers_as_strings() {
    let tree = Tree::new("coerce");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "func_buyzone", "origin": [0, 0, 0],
                             "keyValues": {"TeamNum": 3, "BuyzoneRadius": 96.5,
                                           "fadefactor": true, "buying": "0"}}]}),
    )
    .await
    .unwrap();

    let params = &mb.method_calls("edit.batch")[0].params["ops"][0]["params"];
    assert_eq!(params["class"], json!("func_buyzone"));
    assert_eq!(params["keyValues"]["TeamNum"], json!("3"));
    assert_eq!(params["keyValues"]["BuyzoneRadius"], json!("96.5"));
    assert_eq!(params["keyValues"]["fadefactor"], json!("1"));
    assert_eq!(params["keyValues"]["buying"], json!("0"));
    for (key, value) in params["keyValues"].as_object().unwrap() {
        assert!(value.is_string(), "keyValues[{key}] is not a string: {value}");
    }
    // the entity's origin stays numeric (it is not a keyvalue)
    assert_eq!(params["origin"], json!([0.0, 0.0, 0.0]));
}

#[tokio::test]
async fn create_entities_snaps_sub_1e_4_coordinates_and_never_sends_exponents() {
    let tree = Tree::new("create-exponent");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "info_target", "origin": [5.5e-17, 20, 30],
                             "angles": [0, 5.5e-17, 90]}]}),
    )
    .await
    .unwrap();
    let params = &mb.method_calls("edit.batch")[0].params["ops"][0]["params"];
    assert_eq!(params["origin"], json!([0.0, 20.0, 30.0]));
    assert_eq!(params["angles"], json!([0.0, 0.0, 90.0]));
    assert!(
        !params["origin"].to_string().contains('e')
            && !params["angles"].to_string().contains('e'),
        "the wire must carry plain decimals: {params}"
    );
    // 1e20 would go out as `1e20` and be misread as 1: refused locally (brief item 7)
    let err = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "info_target", "origin": [1e20, 0, 0]}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(err.message.contains("exponent"), "{}", err.message);
    assert_eq!(mb.method_calls("edit.batch").len(), 1);
}

#[tokio::test]
async fn create_entities_warns_on_fgd_issues_and_strict_refuses_without_sending() {
    let tree = Tree::new("strict");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    // non-strict: the unknown key is reported, the entity is still created
    let out = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0],
                             "keyValues": {"nope": "1"}}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1));
    assert_eq!(out["entities"][0]["unknownKeys"], json!(["nope"]));
    let warnings = out["entities"][0]["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("nope")),
        "{warnings:?}"
    );
    assert_eq!(mb.method_calls("edit.batch").len(), 1);

    // strict: the same call is refused locally, nothing else is sent
    let err = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0],
                             "keyValues": {"nope": "1"}}], "strict": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert_eq!(err.name, "InvalidParams");
    assert!(err.message.contains("strict"), "{}", err.message);
    assert!(err.message.contains("nope"), "{}", err.message);
    assert!(err.retry_safe);
    assert_eq!(
        mb.method_calls("edit.batch").len(),
        1,
        "the strict call must not reach the bridge"
    );
    assert_eq!(node_ids(&mb), vec![1, 9, 30, 45, 46], "only the first create ran");
}

#[tokio::test]
async fn create_entities_without_a_database_warns_and_proceeds() {
    let tree = Tree::without_fgd("no-fgd");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1), "no FGD is a warning, never a refusal");
    assert_eq!(out["fgd"]["files"], json!(0));
    let unavailable = out["fgd"]["unavailable"].as_str().expect("unavailable");
    assert!(
        unavailable.starts_with("fgd unavailable:"),
        "{unavailable}"
    );
    let warnings = out["warnings"].as_array().expect("warnings");
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].as_str().unwrap_or_default().starts_with("fgd unavailable:"),
        "{warnings:?}"
    );
    assert_eq!(mb.method_calls("edit.batch").len(), 1);
}

// -------------------------------------------------------------------------------------------
// Acceptance: set_entity_keyvalues
// -------------------------------------------------------------------------------------------

/// Brief items 6/11: the write reports `applied`/`previous` and the undo gap a pre-m2.18 bridge
/// has. `hammer_value_undo(false)` models the m2.15..m2.17 build (group-only reply, no marker
/// advertised); the m2.18 opposite is the `set_entity_keyvalues_records_value_undo` test.
#[tokio::test]
async fn set_entity_keyvalues_reports_applied_previous_and_the_undo_gap() {
    let tree = Tree::new("set");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .hammer_value_undo(false)
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"TeamNum": 3, "BuyzoneRadius": 96.5}}),
    )
    .await
    .unwrap();

    assert_eq!(out["nodeID"], json!(45));
    assert_eq!(out["classname"], json!("func_buyzone"));
    assert_eq!(out["applied"]["TeamNum"], json!(true));
    assert_eq!(out["applied"]["BuyzoneRadius"], json!(true));
    assert_eq!(out["previous"]["TeamNum"], json!("2"));
    assert_eq!(out["previous"]["BuyzoneRadius"], Value::Null);
    assert_eq!(out["valueUndo"], json!("group-only"));
    let hint = out["revertHint"].as_str().expect("revertHint");
    assert!(hint.contains("undo_last does not restore"), "{hint}");
    assert!(hint.contains("previous"), "{hint}");
    // brief item 11: a null previous cannot be removed through the bridge, and the hint points at
    // the FGD default instead of telling the caller to set ""
    assert!(hint.contains("cannot be removed"), "{hint}");
    assert!(hint.contains("get_entity_class"), "{hint}");
    assert!(
        !hint.contains("set it to \"\""),
        "a typed key would refuse \"\": {hint}"
    );
    assert_eq!(out["saved"], json!(false));
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
    assert_eq!(out["warnings"], json!([]), "the FGD check is clean");

    // entity.get read the class, setKeyValues carried strings and the document fingerprint
    let reads = mb.method_calls("entity.get");
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].params["nodeID"], json!(45));
    let writes = mb.method_calls("entity.setKeyValues");
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].params["nodeID"], json!(45));
    assert_eq!(writes[0].params["values"]["TeamNum"], json!("3"));
    assert_eq!(writes[0].params["values"]["BuyzoneRadius"], json!("96.5"));
    assert_eq!(writes[0].params["expectDoc"]["id"], json!("d1"));
    assert!(writes[0].params["deadlineMs"].as_u64().unwrap_or(0) > 0);

    let entity = mb
        .hammer_nodes()
        .into_iter()
        .find(|n| n.node_id == 45)
        .expect("node 45");
    assert_eq!(entity.keyvalues["TeamNum"], "3");
}

#[tokio::test]
async fn set_entity_keyvalues_warns_non_strict_and_refuses_strict_without_writing() {
    let tree = Tree::new("set-strict");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"nope": "1"}}),
    )
    .await
    .unwrap();
    assert!(
        out["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("nope")),
        "{out}"
    );
    assert_eq!(mb.method_calls("entity.setKeyValues").len(), 1);

    let err = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"nope": "1"}, "strict": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(err.message.contains("strict"), "{}", err.message);
    assert!(err.retry_safe);
    assert_eq!(
        mb.method_calls("entity.setKeyValues").len(),
        1,
        "the strict call must not reach the bridge"
    );
}

// -------------------------------------------------------------------------------------------
// Acceptance: undo_last
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn undo_last_pops_the_batch_group_and_decrements_the_counter() {
    let tree = Tree::new("undo-batch");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
    assert_eq!(node_ids(&mb), vec![1, 9, 30, 45, 46]);

    let out = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(out["undone"], json!(true));
    assert_eq!(out["undoName"], json!(BATCH_UNDO_LABEL));
    assert_eq!(out["undoDepth"], json!(0));
    assert_eq!(out["unsavedBridgeEdits"], json!(0), "undo takes the edit back");
    assert_eq!(out["saved"], json!(false));
    assert_eq!(node_ids(&mb), vec![1, 9, 30, 45], "the created node is gone");

    let calls = mb.method_calls("undo.undo");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].params["expectDoc"]["id"], json!("d1"));
    assert!(calls[0].params["deadlineMs"].as_u64().unwrap_or(0) > 0);

    // a second undo with an empty stack answers without sending anything
    let out = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(out["undone"], json!(false));
    assert!(
        out["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("undoDepth 0"),
        "{out}"
    );
    assert_eq!(out["unsavedBridgeEdits"], json!(0));
    assert_eq!(mb.method_calls("undo.undo").len(), 1);
}

/// `undo_last` on a PRE-m2.18 bridge: the popped keyvalue group reverts nothing, the wordings
/// say so, and the counter stays up. `hammer_value_undo(false)` models the m2.15..m2.17 build
/// (group-only reply, no `node.setTransform` advertised); the m2.18 counterpart pin is
/// `undo_last_on_an_m2_18_bridge_restores_the_keyvalues`.
#[tokio::test]
async fn undo_last_notes_that_a_keyvalue_group_did_not_revert_the_values() {
    let tree = Tree::new("undo-keyvalues");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .hammer_value_undo(false)
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"TeamNum": 3}}),
    )
    .await
    .unwrap();
    assert_eq!(out["unsavedBridgeEdits"], json!(1));

    let out = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(out["undone"], json!(true));
    assert_eq!(out["undoName"], json!("MCP: set keyvalues"));
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes.iter().any(|note| note
            .as_str()
            .unwrap_or_default()
            .contains("NOT reverted")),
        "{notes:?}"
    );
    // the counter must NOT go down: the pop reverted nothing, so live still differs from disk
    // (brief item 3 / opus #3 / ds #7)
    assert_eq!(out["unsavedBridgeEdits"], json!(1), "undo reverted nothing: {out}");
    assert!(
        notes.iter().any(|note| note
            .as_str()
            .unwrap_or_default()
            .contains("counter stays up")),
        "the note explains the counter: {notes:?}"
    );

    // the documented gap: the group popped, the value is still there
    let entity = mb
        .hammer_nodes()
        .into_iter()
        .find(|n| n.node_id == 45)
        .expect("node 45");
    assert_eq!(entity.keyvalues["TeamNum"], "3");

    // the counter is really still up: the next (empty-stack) undo_last reports it
    let out = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(out["undone"], json!(false));
    assert_eq!(
        out["unsavedBridgeEdits"],
        json!(1),
        "the counter stayed up: {out}"
    );
}

/// The m2.18 pair of `undo_last_notes_that_a_keyvalue_group_did_not_revert_the_values`
/// (review w2b #2): the default mock is an m2.18 build (`node.setTransform` advertised,
/// `value_undo_recorded` on), so the popped `"MCP: set keyvalues"` group REALLY restores the
/// values — the note says so (no NOT-reverted wording), and the unsaved counter goes back
/// down like any other really-undone group of ours.
#[tokio::test]
async fn undo_last_on_an_m2_18_bridge_restores_the_keyvalues() {
    let tree = Tree::new("undo-keyvalues-m218");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"TeamNum": 3}}),
    )
    .await
    .unwrap();
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
    assert_eq!(out["valueUndo"], json!("recorded"), "{out}");

    let out = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(out["undone"], json!(true));
    assert_eq!(out["undoName"], json!("MCP: set keyvalues"));
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        !notes
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("NOT reverted")),
        "the stale wording is gone on m2.18: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("ARE restored")),
        "{notes:?}"
    );
    // the pop really undid the write: the counter went back down
    assert_eq!(
        out["unsavedBridgeEdits"],
        json!(0),
        "the group really restored: {out}"
    );
    // the mock world really restored the value (RecordNodeTree)
    let entity = mb
        .hammer_nodes()
        .into_iter()
        .find(|n| n.node_id == 45)
        .expect("node 45");
    assert_eq!(entity.keyvalues["TeamNum"], "2");
}

/// `set_entity_keyvalues` on the default (m2.18-advertising) mock: the reply's own
/// `valueUndo:"recorded"` verdict drives `revertHint` to the undo-restores wording (review
/// w2b #5: the RECORDED_HINT switch is the regression test the mock knob existed for).
#[tokio::test]
async fn set_entity_keyvalues_records_value_undo() {
    let tree = Tree::new("set-recorded");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"TeamNum": 3}}),
    )
    .await
    .unwrap();
    assert_eq!(out["valueUndo"], json!("recorded"), "{out}");
    let hint = out["revertHint"].as_str().expect("revertHint");
    assert!(hint.contains("undo_last restores"), "{hint}");
    assert!(!hint.contains("does not restore"), "{hint}");
    // and the undo really restores the value (the mock's RecordNodeTree): the counterpart of
    // undo_last_on_an_m2_18_bridge_restores_the_keyvalues, right through the reply verdict
    let out = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(out["undone"], json!(true));
    assert_eq!(out["unsavedBridgeEdits"], json!(0), "{out}");
}

#[tokio::test]
async fn undo_last_of_a_user_group_with_accept_user_edits_gets_its_own_note() {
    let tree = Tree::new("undo-user");
    // doc.active reports the user group ("Move"), like the UndoGroupNotOurs test; the world's
    // own undo stack stays empty, so `undo.undo` is answered directly (mock responders run
    // before the built-ins) with the popped user group (review N4).
    let user_doc = json!({"doc": {
        "id": "d1", "path": DOC_WIN, "dirty": true, "active": true, "editable": true,
        "notEditableReason": "ok", "epoch": 1, "docType": "vmap", "isNewFile": false,
        "undoName": "Move", "undoDepth": 3, "topSerial": 3
    }});
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .responder_value("doc.active", user_doc)
        .responder_value("undo.undo", json!({"undone": true, "name": "Move", "undoDepth": 2}))
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    // one of this server's own edits so the counter is non-trivial: the user group's pop changes
    // nothing this server counted
    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"TeamNum": 3}}),
    )
    .await
    .unwrap();
    assert_eq!(out["unsavedBridgeEdits"], json!(1));

    let out = call(&server, "undo_last", json!({"acceptUserEdits": true}))
        .await
        .unwrap();
    assert_eq!(out["undone"], json!(true));
    assert_eq!(out["undoName"], json!("Move"));
    let notes: Vec<String> = out["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .filter_map(|note| note.as_str().map(str::to_string))
        .collect();
    assert!(
        notes.iter().any(|note| note.contains("user's own undo group")
            && note.contains("counter is unchanged")),
        "{notes:?}"
    );
    assert!(
        !notes.iter().any(|note| note.contains("did not restore")),
        "that note is only for groups of ours that revert nothing: {notes:?}"
    );
    // the counter stays up: the popped group was not this server's, but reverting it changed
    // nothing this server counted either
    assert_eq!(
        out["unsavedBridgeEdits"],
        json!(1),
        "a user group pop leaves the counter unchanged: {out}"
    );
}

/// FIX-3R L7: the undoName gate passes on the *top* group at `doc.active` ("MCP: batch"), but the
/// pop answered a user group ("Move") — the top changed between the check and the undo (the
/// fingerprint epoch only moves on doc/root/path changes, not user edits, so `expectDoc` does not
/// catch it). The note must then NOT claim `acceptUserEdits` (it was not passed).
#[tokio::test]
async fn undo_last_of_a_user_group_raced_in_after_the_check_wording() {
    let tree = Tree::new("undo-race");
    // `doc.active` answers one of this server's groups (so the `UndoGroupNotOurs` gate passes
    // without acceptUserEdits); `undo.undo` is answered directly (mock responders run before the
    // built-ins) with the user group that raced in.
    let check_doc = json!({"doc": {
        "id": "d1", "path": DOC_WIN, "dirty": true, "active": true, "editable": true,
        "notEditableReason": "ok", "epoch": 1, "docType": "vmap", "isNewFile": false,
        "undoName": "MCP: batch", "undoDepth": 3, "topSerial": 3
    }});
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .responder_value("doc.active", check_doc)
        .responder_value("undo.undo", json!({"undone": true, "name": "Move", "undoDepth": 2}))
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    // one of this server's own edits so the counter is non-trivial
    let edit = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"TeamNum": 3}}),
    )
    .await
    .unwrap();
    assert_eq!(edit["unsavedBridgeEdits"], json!(1));

    let out = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(out["undone"], json!(true));
    assert_eq!(out["undoName"], json!("Move"));
    let notes: Vec<String> = out["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .filter_map(|note| note.as_str().map(str::to_string))
        .collect();
    assert!(
        !notes.iter().any(|note| note.contains("acceptUserEdits")),
        "no note may claim acceptUserEdits when it was not passed: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.contains("not one of this server's undo groups")
                && note.contains("changed between the doc.active check")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.contains("Hammer restored its state")
                && note.contains("counter is unchanged")),
        "{notes:?}"
    );
    assert_eq!(out["unsavedBridgeEdits"], json!(1));
}

/// FIX-3R L4 end-to-end, on a PRE-m2.18 build (the capability set without
/// `node.setTransform`): a set-only `edit.batch` (raw — the tools only send creates in
/// batches, this is the m2.8 whitelist case) is labelled `MCP: set keyvalues` by the bridge
/// (`batch_has_create`, 35528e8), so `undo_last` pops it as a keyvalue group and — an older
/// bridge records no value undo — the unsaved-edit counter stays up and the notes say so.
/// Negative evidence: on the pre-FIX-3R mock the label was `"MCP: batch"` and the counter
/// went down; on an m2.18 bridge the counter goes back down for a keyvalue pop instead
/// (the w2b #2 pair). The mock serves one client at a time, so the raw batch happens first
/// and the handler runs on a directly built state (the pattern of `tests/entities_file.rs`).
#[tokio::test]
async fn undo_last_after_a_raw_set_only_batch_keeps_the_counter() {
    use cs2_mcp_common::bridge::{BridgeClient, ClientOptions, RequestOpts};
    use cs2_mcp_common::tools::{Budget, CallCtx};

    let tree = Tree::new("undo-setonly");
    // the PRE-m2.18 build: `hammer_value_undo(false)` models the whole pair — the reply says
    // "group-only" AND the advertised set drops `node.setTransform`, so `undo_last` keeps
    // treating the popped keyvalue-labelled group as the documented no-restore gap (review
    // w2b #2 pins both wordings: the m2.18-with-marker case is the other test)
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .hammer_value_undo(false)
        .start()
        .await
        .unwrap();

    // a raw set-only edit.batch through the bridge (expectDoc read off doc.active)
    let mut client = BridgeClient::new(ClientOptions {
        discovery_path: mb.discovery_path.clone(),
        ..Default::default()
    });
    client.connect(false).await.unwrap();
    let epoch = client
        .doc_active(None)
        .await
        .unwrap()["doc"]["epoch"]
        .as_i64()
        .unwrap();
    let opts = RequestOpts {
        expect_doc: Some(json!({"id": "d1", "epoch": epoch})),
        ..Default::default()
    };
    let r = client
        .request(
            "edit.batch",
            json!({"ops": [{
                "method": "entity.setKeyValues",
                "params": {"nodeID": 30, "values": {"targetname": "raw_setonly"}}
            }]}),
            opts,
        )
        .await
        .expect("set-only batch");
    assert!(r.result["failedIndex"].is_null(), "{r:?}");
    assert_eq!(
        mb.hammer_nodes()
            .into_iter()
            .find(|n| n.node_id == 30)
            .unwrap()
            .keyvalues
            .get("targetname"),
        Some(&"raw_setonly".to_string()),
        "setup: the batch ran"
    );
    drop(client); // the mock allows one client: free the bridge for the handler

    // the same state shape the server would carry: one counted edit on this document (doc "d1")
    let state = HammerState::from_config(&config(&tree, &mb.discovery_path)).unwrap();
    {
        let mut edits = state.edits.lock().unwrap();
        edits.since_save = 1;
        edits.doc_id = Some("d1".to_string());
    }

    // the top group is the set-only batch: popping it is a keyvalue pop, the counter stays up
    let out = hammer_mcp::tools::entities_live::undo_last(
        state,
        CallCtx {
            budget: Budget::new(45_000, 5_000),
        },
        json!({}),
    )
    .await
    .unwrap()
    .json;
    assert_eq!(out["undone"], json!(true), "{out}");
    assert_eq!(out["undoName"], json!("MCP: set keyvalues"));
    let notes: Vec<String> = out["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .filter_map(|note| note.as_str().map(str::to_string))
        .collect();
    assert!(
        notes.iter().any(|note| note.contains("did not restore")),
        "{notes:?}"
    );
    assert_eq!(
        out["unsavedBridgeEdits"],
        json!(1),
        "a set-only batch pop reverts nothing: {out}"
    );
}

// -------------------------------------------------------------------------------------------
// Required negatives
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn create_entities_with_an_unknown_class_refuses_before_anything_is_sent() {
    let tree = Tree::new("unknown-class");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "info_targe", "origin": [0, 0, 0]}]}),
    )
    .await
    .unwrap_err();

    assert_eq!(err.type_, "UnknownClass");
    assert_eq!(err.name, "UnknownClass");
    assert_eq!(err.code, Some(-32006));
    assert_eq!(err.data["index"], json!(0));
    assert_eq!(err.data["class"], json!("info_targe"));
    let nearest = err.data["nearest"].as_array().expect("nearest");
    assert!(
        nearest.iter().any(|n| n == "info_target"),
        "nearest should suggest info_target: {nearest:?}"
    );
    assert!(err.retry_safe, "nothing was dispatched");
    assert!(mb.method_calls("edit.batch").is_empty());
    assert!(
        mb.method_calls("doc.active").is_empty(),
        "local validation runs before the document read"
    );
    assert!(!err.to_json().to_string().contains("\"handle\""));
    assert_eq!(node_ids(&mb), vec![1, 9, 30, 45]);
}

#[tokio::test]
async fn a_classname_keyvalue_is_refused_by_both_mutators() {
    let tree = Tree::new("classname");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0],
                             "keyValues": {"classname": "func_buyzone"}}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert_eq!(err.name, "InvalidParams");
    assert!(err.message.contains("classname"), "{}", err.message);

    let err = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"Classname": "info_target"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(err.message.contains("classname"), "{}", err.message);

    assert!(mb.method_calls("edit.batch").is_empty(), "nothing sent");
    assert!(mb.method_calls("entity.setKeyValues").is_empty(), "nothing sent");
    assert!(
        mb.method_calls("entity.get").is_empty(),
        "the classname refusal is local, before the class read"
    );
}

#[tokio::test]
async fn a_partial_failure_passes_through_with_failed_index_and_bumps_the_counter() {
    let tree = Tree::new("partial");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    // `weapon_ak47` is in the fixture FGD but not in the mock's class registry -> op 1 fails and the
    // mock answers -32012 with the executed op 0 in `results` (no rollback).
    let err = call(
        &server,
        "create_entities",
        json!({"entities": [
            {"class": "info_target", "origin": [0, 0, 0], "keyValues": {"targetname": "first"}},
            {"class": "weapon_ak47", "origin": [1, 1, 1]}
        ]}),
    )
    .await
    .unwrap_err();

    assert_eq!(err.type_, "PartialFailure");
    assert_eq!(err.name, "PartialFailure");
    assert_eq!(err.code, Some(-32012));
    assert_eq!(err.data["failedIndex"], json!(1));
    assert!(!err.retry_safe, "some ops may have run: verify, never retry");
    assert!(err.outcome_unknown);
    assert_eq!(
        err.data["unsavedBridgeEdits"],
        json!(1),
        "the batch may have executed, so the edit is counted"
    );
    let results = err.data["results"].as_array().expect("results");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["index"], json!(0));
    assert_eq!(results[0]["handleIssued"], json!(true));
    assert_eq!(
        results[0]["nodeID"],
        json!(46),
        "the sanitized result still names the created node (review W2 3a)"
    );
    assert!(results[0].get("handle").is_none());
    assert_eq!(results[1]["error"]["code"], json!(-32006));
    assert!(
        !Value::Object(err.data.clone()).to_string().contains("\"handle\""),
        "{:?}",
        err.data
    );
    assert!(!err.to_json().to_string().contains("\"handle\""));
    assert_eq!(mb.method_calls("edit.batch").len(), 1);
    assert!(
        node_ids(&mb).contains(&46),
        "op 0 really ran: {}",
        node_ids(&mb).len()
    );

    // the counter is really up: the next successful create reports 2
    let out = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["unsavedBridgeEdits"], json!(2));
}

#[tokio::test]
async fn undo_last_refuses_a_group_this_server_did_not_create() {
    let tree = Tree::new("undo-foreign");
    // the mock's own doc.active would report the world's (empty) undo stack: answer a foreign group
    // instead (the mock applies responders before the built-ins).
    let user_doc = json!({"doc": {
        "id": "d1", "path": DOC_WIN, "dirty": true, "active": true, "editable": true,
        "notEditableReason": "ok", "epoch": 1, "docType": "vmap", "isNewFile": false,
        "undoName": "Move", "undoDepth": 3, "topSerial": 3
    }});
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .responder_value("doc.active", user_doc)
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let err = call(&server, "undo_last", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "UndoGroupNotOurs");
    assert_eq!(err.name, "UndoGroupNotOurs");
    assert_eq!(err.data["undoName"], json!("Move"));
    assert_eq!(err.data["undoDepth"], json!(3));
    assert!(err.retry_safe, "nothing was dispatched");
    assert!(mb.method_calls("undo.undo").is_empty());
    assert!(!err.to_json().to_string().contains("\"handle\""));

    // acceptUserEdits lets the very same call through
    let out = call(&server, "undo_last", json!({"acceptUserEdits": true}))
        .await
        .unwrap();
    assert_eq!(out["undone"], json!(false), "the world's undo stack is empty");
    assert_eq!(mb.method_calls("undo.undo").len(), 1);
}

#[tokio::test]
async fn create_entities_with_bad_arguments_sends_nothing() {
    let tree = Tree::new("create-args");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let mut bad = vec![
        json!({}),
        json!({"entities": []}),
        json!({"entities": "info_target"}),
        json!({"entities": [{"origin": [0, 0, 0]}]}),
        json!({"entities": [{"class": "", "origin": [0, 0, 0]}]}),
        json!({"entities": [{"class": "info_target"}]}),
        json!({"entities": [{"class": "info_target", "origin": [0, 0]}]}),
        json!({"entities": [{"class": "info_target", "origin": [0, 0, "z"]}]}),
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0], "angles": [0, 0]}]}),
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0], "keyValues": {"a": null}}]}),
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0], "keyValues": {"a": [1]}}]}),
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0], "keyValues": {"a": {}}}]}),
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0], "keyValues": {"": "x"}}]}),
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0]}], "strict": "yes"}),
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0]}], "undoLabel": ""}),
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0]}], "undoLabel": 3}),
    ];
    // 65 entities (the POC cap)
    bad.push(json!({"entities": (0..65)
        .map(|_| json!({"class": "info_target", "origin": [0, 0, 0]}))
        .collect::<Vec<Value>>()}));
    // a 96-byte key and a 513-byte value
    let mut long_key: Map<String, Value> = Map::new();
    long_key.insert("k".repeat(96), json!("v"));
    bad.push(json!({"entities": [{"class": "info_target", "origin": [0, 0, 0],
                                  "keyValues": Value::Object(long_key)}]}));
    let mut long_value: Map<String, Value> = Map::new();
    long_value.insert("k".to_string(), json!("v".repeat(513)));
    bad.push(json!({"entities": [{"class": "info_target", "origin": [0, 0, 0],
                                  "keyValues": Value::Object(long_value)}]}));
    // 33 keys
    let many: Map<String, Value> = (0..33).map(|i| (format!("k{i}"), json!("v"))).collect();
    bad.push(json!({"entities": [{"class": "info_target", "origin": [0, 0, 0],
                                  "keyValues": Value::Object(many)}]}));
    // one entity whose op params exceed the 6000-byte budget (20 * 500 bytes, all within the limits)
    let wide: Map<String, Value> = (0..20).map(|i| (format!("k{i}"), json!("v".repeat(500)))).collect();
    let too_big = json!({"entities": [{"class": "info_target", "origin": [0, 0, 0],
                                       "keyValues": Value::Object(wide)}]});

    for args in bad {
        let err = call(&server, "create_entities", args.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{args}");
        assert_eq!(err.name, "InvalidParams", "{args}");
        assert!(err.retry_safe, "{args}");
        assert!(!err.to_json().to_string().contains("\"handle\""), "{args}");
    }
    let err = call(&server, "create_entities", too_big).await.unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert_eq!(err.data.get("index"), None, "the message names the index");
    assert!(err.message.contains("entities[0]"), "{}", err.message);
    assert!(err.message.contains("6000"), "{}", err.message);

    assert!(mb.method_calls("edit.batch").is_empty(), "local validation only");
    assert!(mb.method_calls("doc.active").is_empty(), "nothing at all was sent");
    assert_eq!(node_ids(&mb), vec![1, 9, 30, 45]);
}

#[tokio::test]
async fn set_entity_keyvalues_with_bad_arguments_sends_nothing() {
    let tree = Tree::new("set-args");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    for bad in [
        json!({}),
        json!({"nodeID": 45}),
        json!({"nodeID": -1, "keyValues": {"a": "b"}}),
        json!({"nodeID": "45", "keyValues": {"a": "b"}}),
        json!({"nodeID": 45, "keyValues": {}}),
        json!({"nodeID": 45, "keyValues": {}}),
        json!({"nodeID": 45, "keyValues": {"a": 1}, "strict": 1}),
        json!({"nodeID": 45, "keyValues": {"classname": "x"}}),
        json!({"nodeID": 45, "keyValues": {"a": null}}),
    ] {
        let err = call(&server, "set_entity_keyvalues", bad.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
        assert!(err.retry_safe, "{bad}");
    }
    assert!(mb.method_calls("entity.setKeyValues").is_empty());
    assert!(mb.method_calls("entity.get").is_empty(), "nothing at all was sent");
    assert!(mb.method_calls("doc.active").is_empty());
}

#[tokio::test]
async fn set_entity_keyvalues_on_a_non_entity_is_refused_and_writes_nothing() {
    let tree = Tree::new("set-mesh");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    // the shared entity taxonomy (brief item 10): -32602 "not an entity" -> NotAnEntity
    let err = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 9, "keyValues": {"a": "b"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "NotAnEntity");
    assert_eq!(err.name, "NotAnEntity");
    assert_eq!(err.code, Some(-32602));
    assert_eq!(err.data["nodeID"], json!(9));
    assert!(err.retry_safe);
    assert_eq!(mb.method_calls("entity.get").len(), 1);
    assert!(mb.method_calls("entity.setKeyValues").is_empty());

    // -32022 -> EntityNotFound, the same name get_entity_live and the file route use
    let err = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 999999, "keyValues": {"a": "b"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "EntityNotFound");
    assert_eq!(err.name, "EntityNotFound");
    assert_eq!(err.code, Some(-32022));
    assert_eq!(err.data["nodeID"], json!(999999));
    assert!(mb.method_calls("entity.setKeyValues").is_empty());
}

/// Review L5, race path: a node deleted (or replaced by a non-entity) between the `entity.get` and
/// the write fails on `entity.setKeyValues` — the bridge re-resolves the target there — and the
/// caller gets the shared entity taxonomy (`EntityNotFound`/`NotAnEntity`), not the wire one.
#[tokio::test]
async fn a_set_write_failure_on_a_deleted_node_is_entity_not_found() {
    let tree = Tree::new("set-gone");
    // the bridge's not-found on the write (mock: answered on the first entity.setKeyValues call)
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .error_on_call(
            "entity.setKeyValues",
            1,
            -32022,
            "NodeNotFound",
            Some(json!({"reason": "nodeID 45 not found"})),
        )
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"TeamNum": 3}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "EntityNotFound", "{}", err.message);
    assert_eq!(err.name, "EntityNotFound");
    assert_eq!(err.code, Some(-32022));
    assert_eq!(err.data["nodeID"], json!(45));
    assert!(err.retry_safe, "the write never landed");
    assert!(!err.outcome_unknown, "nothing was executed: {}", err.to_json());
    // both calls went out: the node vanished between them
    assert_eq!(mb.method_calls("entity.get").len(), 1);
    assert_eq!(mb.method_calls("entity.setKeyValues").len(), 1);
}

/// Review L5, the other half: the write target became a non-entity -> `NotAnEntity`.
#[tokio::test]
async fn a_set_write_failure_on_a_non_entity_is_not_an_entity() {
    let tree = Tree::new("set-replaced");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .error_on_call(
            "entity.setKeyValues",
            1,
            -32602,
            "InvalidParams",
            Some(json!({"reason": "not an entity"})),
        )
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"TeamNum": 3}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "NotAnEntity", "{}", err.message);
    assert_eq!(err.name, "NotAnEntity");
    assert_eq!(err.code, Some(-32602));
    assert_eq!(err.data["nodeID"], json!(45));
    assert!(err.retry_safe);
    assert_eq!(mb.method_calls("entity.setKeyValues").len(), 1);
}

#[tokio::test]
async fn engine_managed_keyvalues_are_refused_by_both_mutators() {
    let tree = Tree::new("engine-keys");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    for key in ["origin", "ORIGIN", "angles", "Angles", "scales", "SCALES"] {
        let err = call(
            &server,
            "set_entity_keyvalues",
            json!({"nodeID": 45, "keyValues": {key: "1 2 3"}}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{key}");
        assert_eq!(err.name, "InvalidParams", "{key}");
        assert!(
            err.message.contains("origin/angles parameters"),
            "{key}: {}",
            err.message
        );

        let mut kv = Map::new();
        kv.insert(key.to_string(), json!("1 2 3"));
        let err = call(
            &server,
            "create_entities",
            json!({"entities": [{"class": "info_target", "origin": [0, 0, 0],
                                 "keyValues": Value::Object(kv)}]}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{key}");
        assert_eq!(err.name, "InvalidParams", "{key}");
    }

    // nothing reached the bridge: not even the class read or the document
    assert!(mb.method_calls("edit.batch").is_empty());
    assert!(mb.method_calls("entity.setKeyValues").is_empty());
    assert!(mb.method_calls("entity.get").is_empty());
    assert!(mb.method_calls("doc.active").is_empty());
    assert_eq!(node_ids(&mb), vec![1, 9, 30, 45]);
}

#[tokio::test]
async fn a_value_at_the_511_byte_tool_limit_is_accepted_and_512_is_refused() {
    let tree = Tree::new("value-cap");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let at_cap = "v".repeat(511);
    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"vscripts": at_cap}}),
    )
    .await
    .unwrap();
    assert_eq!(out["applied"]["vscripts"], json!(true));
    let sent = &mb.method_calls("entity.setKeyValues")[0].params["values"]["vscripts"];
    assert_eq!(sent.as_str().map(str::len), Some(511));

    for over in ["v".repeat(512), "v".repeat(600)] {
        let err = call(
            &server,
            "set_entity_keyvalues",
            json!({"nodeID": 45, "keyValues": {"vscripts": over}}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments");
        assert!(err.message.contains("511"), "{}", err.message);
        let err = call(
            &server,
            "create_entities",
            json!({"entities": [{"class": "info_target", "origin": [0, 0, 0],
                                 "keyValues": {"vscripts": over}}]}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments");
    }
    // only the accepted call was sent
    assert_eq!(mb.method_calls("entity.setKeyValues").len(), 1);
    assert!(mb.method_calls("edit.batch").is_empty());
}

#[tokio::test]
async fn a_node_id_above_the_bridges_int_range_is_refused_locally() {
    let tree = Tree::new("nodeid-range");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    // 2^32 + 2 would truncate to node 2 in the bridge: refused before dispatch (brief item 5)
    for bad in [json!(4_294_967_298i64), json!(2_147_483_648i64)] {
        let err = call(
            &server,
            "set_entity_keyvalues",
            json!({"nodeID": bad, "keyValues": {"a": "b"}}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
        assert_eq!(err.name, "InvalidParams", "{bad}");
        assert!(err.message.contains("2147483647"), "{}", err.message);
    }
    assert!(mb.method_calls("entity.get").is_empty());
    assert!(mb.method_calls("entity.setKeyValues").is_empty());
    assert!(mb.method_calls("doc.active").is_empty());
}

/// Brief item 2: an `edit.batch` result with fewer entries than ops sent is a `PartialFailure`
/// (an unknown number of ops ran), never a success.
#[tokio::test]
async fn a_short_batch_result_is_a_partial_failure_not_a_success() {
    let tree = Tree::new("short-batch");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        // the mock really applied op 0 (a node exists) but only echoes one result
        .responder_value(
            "edit.batch",
            json!({"results": [{"handle": "n1", "class": "info_target",
                                "origin": [0.0, 0.0, 0.0]}], "failedIndex": null}),
        )
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let err = call(
        &server,
        "create_entities",
        json!({"entities": [
            {"class": "info_target", "origin": [0, 0, 0]},
            {"class": "info_target", "origin": [1, 1, 1]}
        ]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "PartialFailure");
    assert_eq!(err.name, "PartialFailure");
    assert_eq!(err.code, Some(-32012));
    assert!(!err.retry_safe, "an unknown number of ops ran: verify, never retry");
    assert!(err.outcome_unknown);
    assert_eq!(err.data["requested"], json!(2));
    assert_eq!(err.data["results"].as_array().unwrap().len(), 1);
    assert_eq!(err.data["unsavedBridgeEdits"], json!(1));
    assert!(!err.to_json().to_string().contains("\"handle\""));
}

/// Brief item 8: a write that failed on `-32603 InternalError` may already have landed — the mock
/// applies the write and only then answers the error, exactly like the bridge.
#[tokio::test]
async fn a_protocol_error_on_a_write_is_reported_as_maybe_executed() {
    let tree = Tree::new("maybe-executed");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);
    mb.add_error("entity.setKeyValues", -32603, "InternalError", None);

    let err = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"TeamNum": "9"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "ProtocolError");
    assert_eq!(err.code, Some(-32603));
    assert!(err.outcome_unknown, "the write may have landed: {err:?}");
    assert!(!err.retry_safe);
    assert_eq!(err.data["unsavedBridgeEdits"], json!(1));
    // the value really changed in the mock world (that is why the counter must move)
    let entity = mb
        .hammer_nodes()
        .into_iter()
        .find(|n| n.node_id == 45)
        .expect("node 45");
    assert_eq!(entity.keyvalues["TeamNum"], "9");
}

/// Brief item 9: an empty or missing undo name is not ours, `acceptUserEdits` is required.
#[tokio::test]
async fn undo_last_refuses_a_group_with_no_name_without_accept_user_edits() {
    let tree = Tree::new("undo-noname");
    let user_doc = json!({"doc": {
        "id": "d1", "path": DOC_WIN, "dirty": true, "active": true, "editable": true,
        "notEditableReason": "ok", "epoch": 1, "docType": "vmap", "isNewFile": false,
        "undoName": Value::Null, "undoDepth": 2, "topSerial": 2
    }});
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .responder_value("doc.active", user_doc)
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let err = call(&server, "undo_last", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "UndoGroupNotOurs");
    assert_eq!(err.name, "UndoGroupNotOurs");
    assert_eq!(err.data["undoDepth"], json!(2));
    assert!(err.retry_safe, "nothing was dispatched");
    assert!(mb.method_calls("undo.undo").is_empty());

    let out = call(&server, "undo_last", json!({"acceptUserEdits": true}))
        .await
        .unwrap();
    assert_eq!(out["undone"], json!(false), "the world's undo stack is empty");
    assert_eq!(mb.method_calls("undo.undo").len(), 1);
}

/// Brief item 11: a `strict` revert (the value equals the entity's live value) is exempt from the
/// FGD type check, while a different bad value is still refused.
#[tokio::test]
async fn a_strict_revert_is_exempt_from_the_fgd_check() {
    let tree = Tree::new("strict-revert");
    // live TeamNum is "x": a BadType for the integer key, but setting it back to "x" is a revert
    let mb = hammer_mock(vec![
        MockNode::world(),
        MockNode::entity(45, "func_buyzone", [1.0, 2.0, 3.0], kv(&[("TeamNum", "x")])),
    ])
    .await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"TeamNum": "x"}, "strict": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["applied"]["TeamNum"], json!(true));
    assert_eq!(out["exemptFromFgdCheck"], json!(["TeamNum"]));
    assert_eq!(out["warnings"], json!([]), "the exemption produced no issue");

    // a *different* bad value is still refused under strict, and nothing is written
    let err = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 45, "keyValues": {"TeamNum": "abc"}, "strict": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(err.message.contains("strict"), "{}", err.message);
    assert!(err.message.contains("integer"), "{}", err.message);
    assert_eq!(mb.method_calls("entity.setKeyValues").len(), 1, "nothing else was written");
}

/// Brief item 12: an integral float on an integer key is a warning, not an error — `strict` passes.
#[tokio::test]
async fn an_integral_float_on_an_integer_key_is_a_warning_under_strict() {
    let tree = Tree::new("integral-float");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "func_buyzone", "origin": [0, 0, 0],
                             "keyValues": {"TeamNum": 4.0}}], "strict": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1));
    let warnings = out["entities"][0]["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("integral float")),
        "{warnings:?}"
    );

    // a non-integral value is still an error under strict
    let err = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "func_buyzone", "origin": [0, 0, 0],
                             "keyValues": {"TeamNum": 4.5}}], "strict": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert_eq!(mb.method_calls("edit.batch").len(), 1, "the strict call sent nothing");
}

/// Brief item 6/11: the bridge's truncation arrays are surfaced and a cut `previous` is called out.
#[tokio::test]
async fn truncated_previous_values_are_surfaced() {    let tree = Tree::new("truncated");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .responder_value(
            "entity.setKeyValues",
            json!({
                "applied": {"vscripts": Value::Null},
                "warnings": [],
                "previous": {"vscripts": "aaaa"},
                "previousTruncated": ["vscripts"],
                "valueUndo": "group-only"
            }),
        )
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 30, "keyValues": {"vscripts": "bbbb"}}),
    )
    .await
    .unwrap();
    assert_eq!(out["previousTruncated"], json!(["vscripts"]));
    // the key is not in `applied` as true: reported as not applied (brief item 6)
    assert_eq!(out["keyValuesNotApplied"], json!(["vscripts"]));
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("truncated")),
        "{warnings:?}"
    );
}

/// Brief item 6: a reply with no `applied` map confirms nothing, so every key sent is reported as
/// not applied.
#[tokio::test]
async fn a_reply_without_an_applied_map_confirms_nothing() {
    let tree = Tree::new("no-applied");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .responder_value("entity.setKeyValues", json!({"warnings": []}))
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 30, "keyValues": {"targetname": "t31", "spawnflags": "1"}}),
    )
    .await
    .unwrap();
    assert_eq!(
        out["keyValuesNotApplied"],
        json!(["spawnflags", "targetname"]),
        "{out}"
    );
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("no `applied` map")),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn no_tool_payload_ever_carries_a_raw_handle() {
    let tree = Tree::new("handles");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    let created = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0],
                             "keyValues": {"targetname": "h"}}]}),
    )
    .await
    .unwrap();
    assert!(!created.to_string().contains("\"handle\""), "{created}");

    let partial = call(
        &server,
        "create_entities",
        json!({"entities": [
            {"class": "info_target", "origin": [0, 0, 0]},
            {"class": "weapon_ak47", "origin": [0, 0, 0]}
        ]}),
    )
    .await
    .unwrap_err();
    assert!(
        !Value::Object(partial.data.clone()).to_string().contains("\"handle\""),
        "{:?}",
        partial.data
    );
    assert!(!partial.to_json().to_string().contains("\"handle\""));

    let set = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 30, "keyValues": {"targetname": "h2"}}),
    )
    .await
    .unwrap();
    assert!(!set.to_string().contains("\"handle\""), "{set}");

    let undone = call(&server, "undo_last", json!({})).await.unwrap();
    assert!(!undone.to_string().contains("\"handle\""), "{undone}");
}

// -------------------------------------------------------------------------------------------
// WP-S3: the sound-key check in create_entities / set_entity_keyvalues
//
// The check needs a soundevent index, which WP-S1 fills; these tests inject fixed verdicts
// through `HammerState::set_sound_check_source` (`SoundCheckSource::Fixed`) instead of waiting for
// it, and the refusal path uses the same seam. The `FixedVerdicts` guard below restores the real
// index when the test ends. Review L1: the override lives on the **state**, not in a thread local
// — `the_sound_check_seam_is_the_state_not_the_thread` is the test that pins it.
// -------------------------------------------------------------------------------------------

use hammer_mcp::soundevents::{SoundNameVerdict, SoundSource as NameSource};
use hammer_mcp::tools::sound_check::SoundCheckSource;

/// The sound-key world: the shared nodes plus nodeID 60 `snd_event_point` (with a live
/// `soundName`) and nodeID 61 `ambient_generic` (with a live `message`). The classes come from
/// the fixture FGD; `entity.create` would refuse them (the mock's registry has no sound classes),
/// so the create tests that really send a batch answer `edit.batch` with a responder.
fn sound_nodes() -> Vec<MockNode> {
    let mut world = nodes();
    world.push(MockNode::entity(
        60,
        "snd_event_point",
        [10.0, 20.0, 30.0],
        kv(&[("soundName", "mcp.snd.point")]),
    ));
    world.push(MockNode::entity(
        61,
        "ambient_generic",
        [4.0, 5.0, 6.0],
        kv(&[("message", "snd.good")]),
    ));
    world
}

/// Installs a `SoundCheckSource` on the server's **state** (`HammerState::set_sound_check_source`,
/// review L1) and restores the real index on drop.
struct FixedVerdicts {
    state: HammerState,
}

impl FixedVerdicts {
    /// `name -> verdict`; a name the map does not carry is `Unknown { suggestions: [] }`.
    fn fixed(state: &HammerState, pairs: &[(&str, SoundNameVerdict)]) -> FixedVerdicts {
        let map = pairs
            .iter()
            .map(|(name, verdict)| ((*name).to_string(), verdict.clone()))
            .collect();
        state.set_sound_check_source(Some(SoundCheckSource::Fixed(map)));
        FixedVerdicts {
            state: state.clone(),
        }
    }

    /// The index cannot be built (`VpkMissing` on the real path).
    fn unbuildable(state: &HammerState, detail: &str) -> FixedVerdicts {
        state.set_sound_check_source(Some(SoundCheckSource::Unbuildable(detail.to_string())));
        FixedVerdicts {
            state: state.clone(),
        }
    }
}

impl Drop for FixedVerdicts {
    fn drop(&mut self) {
        self.state.set_sound_check_source(None);
    }
}

/// A `Known` verdict for `name` in the addon's compiled file.
fn known(name: &str, source: NameSource) -> SoundNameVerdict {
    SoundNameVerdict::Known {
        name: name.to_string(),
        source,
        file: "game/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts_c".to_string(),
    }
}

/// The reply a create-only `edit.batch` answers when the mock's class registry has no sound
/// classes: one applied result per op, with the created node ids the tool reports.
fn batch_reply(class: &str) -> Value {
    json!({
        "results": [{
            "handle": "n1",
            "class": class,
            "origin": [0.0, 0.0, 0.0],
            "nodeID": 60,
            "applied": {"soundName": true}
        }],
        "failedIndex": null
    })
}

/// NEGATIVE (WP-S3): `create_entities` of a `snd_event_point` whose `soundName` is not in the
/// index is `SoundEventNotFound` with the index's suggestions — and **nothing is sent** (not even
/// `doc.active`: the refusal is local).
#[tokio::test]
async fn create_entities_refuses_an_unknown_sound_event_and_sends_nothing() {
    let tree = Tree::new("sound-refuse");
    let mb = hammer_mock(sound_nodes()).await;
    let server = server(&tree, &mb);
    let _source = FixedVerdicts::fixed(server.state(), &[(
        "Nope.Event",
        SoundNameVerdict::Unknown {
            suggestions: vec!["Nope.Event2".into(), "mcp.snd.point".into()],
        },
    )]);

    let err = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "snd_event_point", "origin": [0, 0, 0],
                             "keyValues": {"soundName": "Nope.Event"}}]}),
    )
    .await
    .unwrap_err();

    assert_eq!(err.type_, "SoundEventNotFound");
    assert_eq!(err.name, "SoundEventNotFound");
    assert!(err.retry_safe, "nothing was dispatched: {err:?}");
    assert!(!err.outcome_unknown);
    assert!(err.code.is_none(), "a local refusal: {err:?}");
    assert_eq!(err.data["name"], json!("Nope.Event"));
    assert_eq!(err.data["key"], json!("soundName"));
    assert_eq!(
        err.data["didYouMean"],
        json!(["Nope.Event2", "mcp.snd.point"]),
        "the index's suggestions ride the refusal"
    );
    assert_eq!(
        err.data["sourcesChecked"],
        json!(["addon-game", "addon-source", "vpk:csgo", "vpk:core"])
    );
    assert!(err.message.contains("silent at runtime"), "{err}");
    assert!(mb.method_calls("edit.batch").is_empty(), "nothing sent");
    // the document read (which also tells the soundevent index which addon to use) is the only
    // thing that went out
    assert!(
        mb.requests().iter().all(|r| r.method.starts_with("control.") || r.method == "doc.active"),
        "nothing sent but the document read: {:?}", mb.requests()
    );
}

/// WP-S3: `allowUnknownSoundEvent:true` writes the same value unchecked and reports it on the
/// entity's own `warnings` (the entity is created).
#[tokio::test]
async fn create_entities_writes_an_unknown_sound_event_with_the_flag_and_warns() {
    let tree = Tree::new("sound-allow");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(sound_nodes())
        .responder_value("edit.batch", batch_reply("snd_event_point"))
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);
    let _source = FixedVerdicts::fixed(server.state(), &[("mcp.snd.known", known("mcp.snd.known", NameSource::AddonGame))]);

    let out = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "snd_event_point", "origin": [0, 0, 0],
                             "keyValues": {"soundName": "Nope.Event"}}],
               "allowUnknownSoundEvent": true}),
    )
    .await
    .unwrap();

    assert_eq!(out["count"], json!(1));
    assert_eq!(out["entities"][0]["nodeID"], json!(60));
    let warnings = out["entities"][0]["warnings"]
        .as_array()
        .expect("per-entity warnings");
    assert!(
        warnings.iter().any(|w| w
            .as_str()
            .unwrap_or_default()
            .contains("not in the soundevent index")),
        "{warnings:?}"
    );
    let sent = &mb.method_calls("edit.batch")[0].params["ops"][0]["params"];
    assert_eq!(sent["class"], json!("snd_event_point"));
    assert_eq!(sent["keyValues"]["soundName"], json!("Nope.Event"), "written as sent");
}

/// WP-S3 positive: a `Known` name writes with no sound warning at all (the check is not noise on
/// a valid write), and the value reaches the wire unchanged.
#[tokio::test]
async fn create_entities_accepts_a_known_sound_event_without_a_warning() {
    let tree = Tree::new("sound-known");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(sound_nodes())
        .responder_value("edit.batch", batch_reply("snd_event_point"))
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);
    let _source = FixedVerdicts::fixed(server.state(), &[(
        "mcp.snd.point",
        known("mcp.snd.point", NameSource::AddonGame),
    )]);

    let out = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "snd_event_point", "origin": [0, 0, 0],
                             "keyValues": {"soundName": "mcp.snd.point"}}]}),
    )
    .await
    .unwrap();

    assert_eq!(out["count"], json!(1));
    assert!(
        out["entities"][0].get("warnings").is_none(),
        "a known name adds no warning: {out}"
    );
    let sent = &mb.method_calls("edit.batch")[0].params["ops"][0]["params"];
    assert_eq!(sent["keyValues"]["soundName"], json!("mcp.snd.point"));
}

/// WP-S3: a `SourceOnly` name (in the addon `.vsndevts` but not compiled yet) is a **warning**,
/// not a refusal — the value is written so `compile_soundevents` can pick it up.
#[tokio::test]
async fn create_entities_warns_on_a_source_only_sound_event() {
    let tree = Tree::new("sound-source-only");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(sound_nodes())
        .responder_value("edit.batch", batch_reply("snd_event_point"))
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);
    let _source = FixedVerdicts::fixed(server.state(), &[(
        "mcp.src.only",
        SoundNameVerdict::SourceOnly {
            file: "content/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts".into(),
        },
    )]);

    let out = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "snd_event_point", "origin": [0, 0, 0],
                             "keyValues": {"soundName": "mcp.src.only"}}]}),
    )
    .await
    .unwrap();

    assert_eq!(out["count"], json!(1));
    let warnings = out["entities"][0]["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("compile_soundevents")),
        "{warnings:?}"
    );
}

/// WP-S3 NEGATIVE: `set_entity_keyvalues` refuses an unknown `soundName` locally (the FGD names
/// `soundName` a `sound` key on the live entity's class) and writes nothing.
#[tokio::test]
async fn set_entity_keyvalues_refuses_an_unknown_sound_event_without_writing() {
    let tree = Tree::new("set-sound-refuse");
    let mb = hammer_mock(sound_nodes()).await;
    let server = server(&tree, &mb);
    let _source = FixedVerdicts::fixed(server.state(), &[]);

    let err = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 60, "keyValues": {"soundName": "Nope.Event"}}),
    )
    .await
    .unwrap_err();

    assert_eq!(err.type_, "SoundEventNotFound");
    assert_eq!(err.data["name"], json!("Nope.Event"));
    assert!(err.data.get("didYouMean").is_none(), "the index had none: no empty list");
    assert!(err.retry_safe);
    assert_eq!(mb.method_calls("entity.get").len(), 1, "the live read happened");
    assert!(
        mb.method_calls("entity.setKeyValues").is_empty(),
        "nothing was written"
    );
}

/// WP-S3 NEGATIVE: `force:true` writes the same unknown name and reports it in `warnings`.
#[tokio::test]
async fn set_entity_keyvalues_force_writes_an_unknown_sound_event_with_a_warning() {
    let tree = Tree::new("set-sound-force");
    let mb = hammer_mock(sound_nodes()).await;
    let server = server(&tree, &mb);
    let _source = FixedVerdicts::fixed(server.state(), &[]);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 60, "keyValues": {"soundName": "Nope.Event"}, "force": true}),
    )
    .await
    .unwrap();

    assert_eq!(out["applied"]["soundName"], json!(true));
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().any(|w| w
            .as_str()
            .unwrap_or_default()
            .contains("written unchecked")),
        "{warnings:?}"
    );
    assert_eq!(
        mb.method_calls("entity.setKeyValues")[0].params["values"]["soundName"],
        json!("Nope.Event")
    );
}

/// WP-S3 NEGATIVE: a revert — the submitted value equals the live one — skips the lookup
/// entirely. The seam is `Unbuildable`, so *any* lookup would refuse: passing proves none ran.
/// `strict:true` is the harder version of the same rule (the revert stays exempt).
#[tokio::test]
async fn set_entity_keyvalues_skips_the_sound_lookup_for_a_revert() {
    let tree = Tree::new("set-sound-revert");
    let mb = hammer_mock(sound_nodes()).await;
    let server = server(&tree, &mb);
    let _source = FixedVerdicts::unbuildable(server.state(), "csgo/pak01_dir.vpk is missing");

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 60, "keyValues": {"soundName": "mcp.snd.point"}, "strict": true}),
    )
    .await
    .unwrap();

    assert_eq!(out["exemptFromFgdCheck"], json!(["soundName"]));
    assert_eq!(out["applied"]["soundName"], json!(true));
    assert_eq!(
        out["warnings"],
        json!([]),
        "no warning: the check did not run at all"
    );
    assert!(
        out["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .all(|w| !w.as_str().unwrap_or_default().contains("could not be built")),
        "{out}"
    );

    // The control: the same unbuildable index refuses a *changed* value (so the test above is
    // proving the exemption, not an accidental pass).
    let err = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 60, "keyValues": {"soundName": "mcp.snd.other"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "VpkMissing");
}

/// WP-S3 NEGATIVE: `ambient_generic.message` accepts a direct sound file path — it is a warning
/// (the legacy form is not a soundevent name), never a refusal.
#[tokio::test]
async fn set_entity_keyvalues_warns_on_a_sound_file_path() {
    let tree = Tree::new("set-sound-path");
    let mb = hammer_mock(sound_nodes()).await;
    let server = server(&tree, &mb);
    let _source = FixedVerdicts::fixed(server.state(), &[("sounds/foo.wav", SoundNameVerdict::FilePath)]);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 61, "keyValues": {"message": "sounds/foo.wav"}}),
    )
    .await
    .unwrap();

    assert_eq!(out["applied"]["message"], json!(true));
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().any(|w| {
            let text = w.as_str().unwrap_or_default();
            text.contains("sound file path") && text.contains("sounds/foo.wav")
        }),
        "{warnings:?}"
    );
    assert_eq!(
        mb.method_calls("entity.setKeyValues")[0].params["values"]["message"],
        json!("sounds/foo.wav")
    );
}

/// WP-S3 NEGATIVE: an empty sound keyvalue clears the event — a warning ("nothing was looked
/// up"), never a refusal.
#[tokio::test]
async fn set_entity_keyvalues_warns_on_an_empty_sound_event() {
    let tree = Tree::new("set-sound-empty");
    let mb = hammer_mock(sound_nodes()).await;
    let server = server(&tree, &mb);
    let _source = FixedVerdicts::fixed(server.state(), &[("", SoundNameVerdict::Empty)]);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 60, "keyValues": {"soundName": ""}}),
    )
    .await
    .unwrap();

    assert_eq!(out["applied"]["soundName"], json!(true));
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().any(|w| w
            .as_str()
            .unwrap_or_default()
            .contains("nothing was looked up")),
        "{warnings:?}"
    );
    assert_eq!(
        mb.method_calls("entity.setKeyValues")[0].params["values"]["soundName"],
        json!("")
    );
}

/// WP-S3 NEGATIVE: an index that cannot be built is a refusal naming it (never a silent pass);
/// `force:true` writes with a warning.
#[tokio::test]
async fn set_entity_keyvalues_refuses_when_the_sound_index_cannot_be_built() {
    let tree = Tree::new("set-sound-vpk");
    let mb = hammer_mock(sound_nodes()).await;
    let server = server(&tree, &mb);
    let _source = FixedVerdicts::unbuildable(server.state(), "game/csgo/pak01_dir.vpk is missing");

    let err = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 60, "keyValues": {"soundName": "mcp.snd.point2"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "VpkMissing");
    assert_eq!(err.name, "VpkMissing");
    assert!(
        err.message.contains("game/csgo/pak01_dir.vpk is missing"),
        "{err}"
    );
    assert!(err.retry_safe, "nothing was written: {err:?}");
    assert!(mb.method_calls("entity.setKeyValues").is_empty());

    // force: the same call goes through with a warning naming the reason.
    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 60, "keyValues": {"soundName": "mcp.snd.point2"}, "force": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["applied"]["soundName"], json!(true));
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().any(|w| {
            let text = w.as_str().unwrap_or_default();
            text.contains("mcp.snd.point2") && text.contains("could not be built")
        }),
        "{warnings:?}"
    );
    assert_eq!(mb.method_calls("entity.setKeyValues").len(), 1);
}

/// WP-S3 NEGATIVE + review L8: without an FGD the check cannot know which keys are sound-typed, so
/// it does not run — and says so on the entity's warnings instead of reporting a silent pass. The
/// warning is **only** about sound when a key the real FGD types as `sound` is present (L8):
/// `targetname` is not one, so that edit stays silent.
#[tokio::test]
async fn the_sound_check_says_it_did_not_run_without_an_fgd() {
    let tree = Tree::without_fgd("sound-no-fgd");
    let mb = hammer_mock(nodes()).await;
    let server = server(&tree, &mb);

    // L8: a non-sound key (`targetname`) is not a sound edit: no warning at all.
    let out = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "info_target", "origin": [0, 0, 0],
                             "keyValues": {"targetname": "no_fgd"}}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1));
    assert!(
        out["entities"][0]["warnings"]
            .as_array()
            .is_none_or(|warnings| warnings.is_empty()),
        "a `targetname` only is not a sound edit (review L8): {out}"
    );

    // A key the real FGD types as `sound` is: the warning fires and names the key.
    let out = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "info_target", "origin": [1, 1, 1],
                             "keyValues": {"soundName": "mcp.snd.point"}}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1));
    let warnings = out["entities"][0]["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().any(|w| w
            .as_str()
            .unwrap_or_default()
            .contains("the sound-event check did not run")),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("\"soundName\"")),
        "the warning names the key it could not type: {warnings:?}"
    );

    // No keyvalues: nothing could be a sound event, so no sound warning is added.
    let out = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "info_target", "origin": [2, 2, 2]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1));
    assert!(
        out["entities"][0].get("warnings").is_none(),
        "an edit without keyvalues is not warned about: {out}"
    );
}

/// The FGD-free sound-key list is the FGD's own, not just the three event keys: `core/base.fgd`
/// types `noise1`/`noise2` (`func_door`), `startsound`, `use_sound`, `locked_sound`, … as
/// `sound`, so an edit of one of them is a sound edit and the "the check did not run" warning
/// names it — while a non-sound key in the same edit is not named.
#[tokio::test]
async fn the_fgd_free_sound_key_list_covers_the_real_sound_keys() {
    let tree = Tree::without_fgd("sound-no-fgd-keys");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(nodes())
        .responder_value("edit.batch", batch_reply("func_door"))
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);

    let out = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "func_door", "origin": [0, 0, 0],
                             "keyValues": {"noise1": "sounds/door_move.wav",
                                           "noise2": "sounds/door_stop.wav",
                                           "health": "1"}}]}),
    )
    .await
    .unwrap();

    let warnings = out["entities"][0]["warnings"].as_array().expect("warnings");
    let text = warnings
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(text.contains("the sound-event check did not run"), "{text}");
    assert!(text.contains("\"noise1\""), "the warning names noise1: {text}");
    assert!(text.contains("\"noise2\""), "the warning names noise2: {text}");
    assert!(
        !text.contains("\"health\""),
        "health is not a sound key and is not named: {text}"
    );

    let sent = &mb.method_calls("edit.batch")[0].params["ops"][0]["params"];
    assert_eq!(sent["class"], json!("func_door"));
    assert_eq!(sent["keyValues"]["noise1"], json!("sounds/door_move.wav"));
}

/// WP-S3: the check is scoped to the `sound`-typed keys — a `soundscape(string)` (a legacy `.vsc`
/// name on the same class) and any other key pass untouched, with no lookup and no warning.
#[tokio::test]
async fn the_sound_check_ignores_keys_that_are_not_sound_typed() {
    let tree = Tree::new("sound-scope");
    let mb = hammer_mock(sound_nodes()).await;
    let server = server(&tree, &mb);
    // Nothing is configured: any lookup of a sound key would be `Unknown` and refuse.
    let _source = FixedVerdicts::fixed(server.state(), &[]);

    let out = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 61, "keyValues": {"radius": "300", "health": "5"}}),
    )
    .await
    .unwrap();
    assert_eq!(out["applied"]["radius"], json!(true));
    assert_eq!(out["warnings"], json!([]), "{out}");
    assert_eq!(mb.method_calls("entity.setKeyValues").len(), 1);
}

/// FIX-I review M1: a `Stale` name — the addon's compiled `_c` still carries it while the
/// `.vsndevts` source removed or renamed it — is a **warning**, never a refusal: the value may be
/// the name the user is about to add back. The index-side rule that produces the verdict is
/// `tests/soundevents_query.rs`.
#[tokio::test]
async fn create_entities_warns_on_a_stale_sound_event_and_writes_it() {
    let tree = Tree::new("sound-stale");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(sound_nodes())
        .responder_value("edit.batch", batch_reply("snd_event_point"))
        .start()
        .await
        .unwrap();
    let server = server(&tree, &mb);
    let _source = FixedVerdicts::fixed(
        server.state(),
        &[(
            "mcp.snd.gone",
            SoundNameVerdict::Stale {
                file: "content/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts"
                    .to_string(),
            },
        )],
    );

    let out = call(
        &server,
        "create_entities",
        json!({"entities": [{"class": "snd_event_point", "origin": [0, 0, 0],
                             "keyValues": {"soundName": "mcp.snd.gone"}}]}),
    )
    .await
    .unwrap();

    assert_eq!(out["count"], json!(1), "a stale name is not a refusal");
    let warnings = out["entities"][0]["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().any(|w| {
            let text = w.as_str().unwrap_or_default();
            text.contains("removed or renamed") && text.contains("compile_soundevents")
        }),
        "{warnings:?}"
    );
    let sent = &mb.method_calls("edit.batch")[0].params["ops"][0]["params"];
    assert_eq!(sent["keyValues"]["soundName"], json!("mcp.snd.gone"));
}

/// FIX-I review L2: a whitespace-padded name is not the engine's name — the refusal says so and
/// suggests the trimmed spelling the index knows.
#[tokio::test]
async fn a_whitespace_padded_name_is_refused_with_the_trimmed_suggestion() {
    let tree = Tree::new("sound-padded");
    let mb = hammer_mock(sound_nodes()).await;
    let server = server(&tree, &mb);
    let _source = FixedVerdicts::fixed(
        server.state(),
        &[(
            "mcp.snd.point ",
            SoundNameVerdict::Unknown {
                suggestions: vec!["mcp.snd.point".into()],
            },
        )],
    );

    let err = call(
        &server,
        "set_entity_keyvalues",
        json!({"nodeID": 60, "keyValues": {"soundName": "mcp.snd.point "}}),
    )
    .await
    .unwrap_err();

    assert_eq!(err.type_, "SoundEventNotFound", "{err}");
    assert_eq!(err.data["didYouMean"], json!(["mcp.snd.point"]));
    assert!(err.message.contains("leading/trailing whitespace"), "{err}");
    assert!(err.message.contains("'mcp.snd.point'"), "{err}");
    assert!(mb.method_calls("entity.setKeyValues").is_empty(), "nothing written");
}

/// FIX-I review L1: the sound-check seam is the **state**, not a thread local. The tool call runs
/// in a `tokio::spawn` on a multi-thread runtime — always a worker thread, never this test's —
/// so with the old `sound_check::set_source` thread-local the worker read
/// `SoundCheckSource::Index`: the real index on this fixture tree fails to build (`VpkMissing`),
/// and this call refused instead of writing. With the state-keyed source it sees the pinned
/// `Known` verdict and applies the value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sound_check_seam_is_the_state_not_the_thread() {
    let tree = Tree::new("sound-seam-thread");
    let mb = hammer_mock(sound_nodes()).await;
    let server = std::sync::Arc::new(server(&tree, &mb));
    let _source = FixedVerdicts::fixed(
        server.state(),
        &[(
            "mcp.snd.other",
            known("mcp.snd.other", NameSource::AddonGame),
        )],
    );

    let handle = {
        let server = std::sync::Arc::clone(&server);
        tokio::spawn(async move {
            call(
                &server,
                "set_entity_keyvalues",
                // NOT the node's live value: a revert is exempt from the lookup and would prove
                // nothing here.
                json!({"nodeID": 60, "keyValues": {"soundName": "mcp.snd.other"}}),
            )
            .await
        })
    };
    let out = handle
        .await
        .expect("the spawned tool call must not panic")
        .unwrap_or_else(|error| panic!("the worker thread must see the state's verdicts: {error}"));

    assert_eq!(out["applied"]["soundName"], json!(true));
    assert_eq!(out["warnings"], json!([]), "a known name warns about nothing: {out}");
    assert_eq!(mb.method_calls("entity.setKeyValues").len(), 1);
    assert_eq!(
        mb.method_calls("entity.setKeyValues")[0].params["values"]["soundName"],
        json!("mcp.snd.other")
    );
}

