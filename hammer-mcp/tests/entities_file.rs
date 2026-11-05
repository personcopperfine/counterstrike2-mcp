//! `entities_file` tool tests (WP-T1): the three file-route entity tools against a temp content
//! tree (under `CARGO_TARGET_TMPDIR`, never /tmp) with the committed `.vmap` fixtures and their
//! `*.entities.json` goldens. No bridge is configured: every acceptance case passes `path`, and
//! the `NoDocument` negative covers the omitted-path route. The `staleRead` acceptance bumps
//! `state.edits` on a locally built [`HammerState`] (the server's state is not reachable) and
//! calls the handler directly — the same functions the registry runs.
//!
//! Helpers are local copies of the pattern in `tests/model_file.rs` (`Tree`, `config`, `call`).
//!
//! The `#[ignore]`d test at the bottom
//! ([`real_tree_template_wingman_connections_through_get_entity`]) runs `get_entity` against a real
//! Valve map's one `logic_auto` I/O connection, copied at test time into the temp app tree under
//! `target/` — the only case that exercises `connections` on real data
//! (`cargo test -p hammer-mcp --test entities_file -- --ignored`).
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::tools::{Budget, CallCtx, CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A temp project tree laid out like the real one: `--project-root` at `root`, app dir at
/// `workshop/run/app`, addon content under its `content/csgo_addons/mcp_test`.
struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "entities-file-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("tmp tree");
        let app = root.join("workshop/run/app");
        Tree { root, app }
    }

    fn addon(&self) -> PathBuf {
        self.app.join("content/csgo_addons/mcp_test")
    }

    /// Copy a committed fixture from `hammer-mcp/fixtures/vmap/` into the addon at `rel`.
    fn install(&self, fixture: &str, rel: &str) -> PathBuf {
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/vmap").join(fixture);
        self.install_from(&source, rel)
    }

    /// Copy any existing file (the real game tree's map) into the addon at `rel`. The copy lives
    /// under `CARGO_TARGET_TMPDIR` (never /tmp, never the repo): Valve content is read-only and
    /// never committed, and the tools only read files under `<app_root>/content/`.
    fn install_from(&self, source: &Path, rel: &str) -> PathBuf {
        let target = self.addon().join(rel);
        std::fs::create_dir_all(target.parent().expect("rel has a parent")).expect("install dir");
        std::fs::copy(source, &target).expect("install file");
        target
    }

    /// Read a committed golden (`hammer-mcp/fixtures/vmap/<name>`).
    fn golden(&self, name: &str) -> Value {
        let text = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/vmap").join(name),
        )
        .expect("golden file");
        serde_json::from_str(&text).expect("golden JSON")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn config(tree: &Tree) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let map = format!("z:\\app={}", tree.app.display());
    let given: Vec<&str> = vec![
        "--deadline-ms",
        "5000",
        "--tool-budget-ms",
        "45000",
        "--project-root",
        &root,
        "--path-map",
        &map,
    ];
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

/// [`config`] plus a mock bridge's discovery file (the `staleRead`/`doc.active.dirty` tests).
fn config_with_bridge(tree: &Tree, discovery: &Path) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let map = format!("z:\\app={}", tree.app.display());
    let discovery = discovery.display().to_string();
    let given: Vec<&str> = vec![
        "--deadline-ms",
        "5000",
        "--tool-budget-ms",
        "45000",
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

fn ctx() -> CallCtx {
    CallCtx {
        budget: Budget::new(45_000, 5_000),
    }
}

/// The addon-relative path form the tools document (`maps/x.vmap`).
const PROBE_ARG: &str = "maps/probe.vmap";
const DEFUSE_ARG: &str = "maps/defuse_poc.vmap";
/// The same files, content-relative (`csgo_addons/<addon>/maps/x.vmap`).
const PROBE_CONTENT_REL: &str = "csgo_addons/mcp_test/maps/probe.vmap";

fn entity_node_ids(out: &Value) -> Vec<i64> {
    out["entities"]
        .as_array()
        .expect("entities array")
        .iter()
        .map(|entity| entity["nodeID"].as_i64().expect("nodeID"))
        .collect()
}

// -------------------------------------------------------------------------------------------
// Acceptance
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn list_reports_all_probe_entities() {
    let tree = Tree::new("list");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    let out = call(&server, "list_entities", json!({"path": PROBE_ARG}))
        .await
        .unwrap();
    assert_eq!(out["total"], json!(69), "brief: probe has 69 entities");
    assert_eq!(out["count"], json!(69));
    assert_eq!(out["truncated"], json!(false));
    assert_eq!(out["source"]["fromDisk"], json!(true));
    assert_eq!(out["source"]["entityCount"], json!(69));
    assert_eq!(
        out["source"]["hostPath"],
        json!(tree.addon().join("maps/probe.vmap").display().to_string())
    );
    assert!(out["source"]["mtime"].is_u64(), "mtime is unix secs");
    assert_eq!(out["staleRead"], json!(false));
    assert_eq!(out["unsavedBridgeEdits"], json!(0));

    // limit truncates; truncated + total stay truthful.
    let out = call(&server, "list_entities", json!({"path": PROBE_ARG, "limit": 5}))
        .await
        .unwrap();
    assert_eq!(out["count"], json!(5));
    assert_eq!(out["total"], json!(69));
    assert_eq!(out["truncated"], json!(true));
    assert_eq!(out["entities"].as_array().unwrap().len(), 5);

    // classnames:true -> the histogram instead of the entity list.
    let out = call(
        &server,
        "list_entities",
        json!({"path": PROBE_ARG, "classnames": true}),
    )
    .await
    .unwrap();
    assert!(out["entities"].is_null(), "no entity list with classnames");
    let histogram = out["classnames"].as_object().expect("classnames map");
    assert_eq!(out["count"], json!(histogram.len()));
    assert_eq!(out["total"], json!(69));
    let listed: u64 = histogram
        .values()
        .map(|count| count.as_u64().unwrap_or(0))
        .sum();
    assert_eq!(listed, 69, "histogram counts sum to the entity total");
}

#[tokio::test]
async fn find_func_buyzone_on_probe() {
    let tree = Tree::new("find");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    let out = call(
        &server,
        "find_entities",
        json!({"path": PROBE_ARG, "class": "func_buyzone"}),
    )
    .await
    .unwrap();
    let mut node_ids = entity_node_ids(&out);
    node_ids.sort();
    assert_eq!(node_ids, vec![67, 70], "brief: func_buyzone nodeIDs {{67,70}}");
    assert_eq!(out["total"], json!(2));
    assert_eq!(out["count"], json!(2));
    assert_eq!(out["truncated"], json!(false));
    // Default view: no keyvalues.
    assert!(out["entities"][0]["keyvalues"].is_null());

    // withKeyvalues adds them; case-insensitive class; content-relative path form.
    let out = call(
        &server,
        "find_entities",
        json!({
            "path": PROBE_CONTENT_REL,
            "class": "FUNC_BUYZONE",
            "withKeyvalues": true
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["total"], json!(2));
    for entity in out["entities"].as_array().unwrap() {
        assert!(entity["keyvalues"].is_object(), "keyvalues requested");
        assert_eq!(entity["keyvalues"]["classname"], json!("func_buyzone"));
    }

    // nameContains: substring over classname OR targetname, case-insensitive.
    let out = call(
        &server,
        "find_entities",
        json!({"path": PROBE_ARG, "nameContains": "BUY"}),
    )
    .await
    .unwrap();
    assert_eq!(out["total"], json!(2), "nameContains buys matches both buyzones");
}

#[tokio::test]
async fn get_defuse_node_30_matches_the_golden() {
    let tree = Tree::new("get");
    tree.install("defuse_poc.vmap", "maps/defuse_poc.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    let out = call(
        &server,
        "get_entity",
        json!({"path": DEFUSE_ARG, "nodeID": 30}),
    )
    .await
    .unwrap();
    let entity = &out["entity"];
    assert_eq!(entity["nodeID"], json!(30));
    assert_eq!(entity["classname"], json!("info_map_parameters"));

    let golden = tree.golden("defuse_poc.entities.json");
    let golden = golden
        .as_array()
        .unwrap()
        .iter()
        .find(|entity| entity["nodeID"] == json!(30))
        .expect("golden entry for nodeID 30");
    assert_eq!(
        entity["keyvalues"], golden["keyvalues"],
        "keyvalues equal the golden's (bombradius \"500\" et al.)"
    );
    assert_eq!(entity["connections"], golden["connections"]);
}

#[tokio::test]
async fn stale_read_follows_the_unsaved_counter() {
    // The brief's staleRead acceptance: unsaved bridge edits > 0 => staleRead true for a
    // file-route read (no bridge here, so the keep-it-simple rule applies). The server's state
    // is not reachable, so build HammerState with the same config and call the handler directly.
    let tree = Tree::new("stale");
    tree.install("probe.vmap", "maps/probe.vmap");
    let cfg = config(&tree);
    let state = HammerState::from_config(&cfg).unwrap();
    let args = json!({"path": PROBE_ARG});

    let out = hammer_mcp::tools::entities_file::list_entities(state.clone(), ctx(), args.clone())
        .await
        .unwrap();
    assert_eq!(out.json["staleRead"], json!(false));
    assert_eq!(out.json["staleReason"], Value::Null, "fresh reads have no reason");

    {
        let mut edits = state.edits.lock().unwrap();
        edits.since_save += 1;
    }
    let out = hammer_mcp::tools::entities_file::list_entities(state, ctx(), args)
        .await
        .unwrap();
    assert_eq!(out.json["staleRead"], json!(true));
    assert_eq!(out.json["unsavedBridgeEdits"], json!(1));
    assert!(
        out.json["staleReason"]
            .as_str()
            .unwrap_or_default()
            .contains("unsaved bridge edit"),
        "the reason names the counter: {}",
        out.json["staleReason"]
    );
    assert!(
        out.json["warning"].is_string(),
        "the unsaved-warning string is present"
    );
}

/// Brief item 4: Hammer's own `doc.active.dirty` flag makes a file read stale even when this
/// server has no unsaved edits (the user edited in Hammer, or undid a keyvalue group).
#[tokio::test]
async fn stale_read_follows_hammers_own_dirty_flag() {
    use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};

    let tree = Tree::new("dirty");
    tree.install("probe.vmap", "maps/probe.vmap");
    tree.install("defuse_poc.vmap", "maps/defuse_poc.vmap");
    // the ACTIVE document is the installed probe map and Hammer says it is dirty
    let mb = MockBridge::builder()
        .doc(Some(make_doc(
            "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap",
            Some(true),
        )))
        .hammer_nodes(vec![MockNode::world()])
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config_with_bridge(&tree, &mb.discovery_path)).unwrap();

    let out = call(&server, "list_entities", json!({"path": PROBE_ARG}))
        .await
        .unwrap();
    assert_eq!(out["unsavedBridgeEdits"], json!(0), "no MCP edits at all");
    assert_eq!(out["staleRead"], json!(true), "Hammer's dirty flag makes it stale");
    let reason = out["staleReason"].as_str().expect("staleReason");
    assert!(reason.contains("dirty"), "{reason}");
    assert_eq!(mb.method_calls("doc.active").len(), 1, "doc.active is the only bridge use");

    // the omitted-path form reads the same active document, with the same answer
    let out = call(&server, "list_entities", json!({})).await.unwrap();
    assert_eq!(out["staleRead"], json!(true));
    assert_eq!(out["source"]["entityCount"], json!(69));

    // get_entity carries the same envelope
    let out = call(&server, "get_entity", json!({"path": PROBE_ARG, "nodeID": 1}))
        .await
        .unwrap();
    assert_eq!(out["staleRead"], json!(true));
    assert!(out["staleReason"].as_str().unwrap_or_default().contains("dirty"));

    // a file that is NOT the active document is never stale, dirty or not
    let out = call(&server, "list_entities", json!({"path": DEFUSE_ARG}))
        .await
        .unwrap();
    assert_eq!(out["staleRead"], json!(false), "not the active document");
    assert_eq!(out["staleReason"], Value::Null);
    assert!(out["source"]["entityCount"].as_u64().unwrap_or(0) > 0);
}

// -------------------------------------------------------------------------------------------
// Required negatives
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn missing_file_is_file_not_found() {
    let tree = Tree::new("missing");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let err = call(
        &server,
        "list_entities",
        json!({"path": "maps/nope.vmap"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "FileNotFound");
    assert_eq!(err.data["path"], json!("maps/nope.vmap"));
    assert!(err.retry_safe);
}

#[tokio::test]
async fn find_without_filters_is_invalid_arguments() {
    let tree = Tree::new("find-args");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let err = call(&server, "find_entities", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert_eq!(err.name, "InvalidParams");
    assert!(err.retry_safe);
}

/// Brief item 13: `class: ""` (or targetname/nameContains) matches everything, so it is refused
/// instead of being read as "no filter" (or, in the file route, as a substring that matches all).
#[tokio::test]
async fn find_with_an_empty_filter_is_invalid_arguments() {
    let tree = Tree::new("find-empty");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    for bad in [
        json!({"path": PROBE_ARG, "class": ""}),
        json!({"path": PROBE_ARG, "targetname": ""}),
        json!({"path": PROBE_ARG, "nameContains": ""}),
    ] {
        let err = call(&server, "find_entities", bad.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
        assert_eq!(err.name, "InvalidParams", "{bad}");
        assert!(err.retry_safe, "{bad}");
        assert!(err.message.contains("empty"), "{bad}: {}", err.message);
    }
}

/// Brief item 5: the file route accepts the same nodeID range as the live route.
#[tokio::test]
async fn get_entity_refuses_a_node_id_above_the_int_range() {
    let tree = Tree::new("nodeid-range");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    for bad in [json!(-1), json!(2_147_483_648i64), json!(4_294_967_298i64)] {
        let err = call(&server, "get_entity", json!({"path": PROBE_ARG, "nodeID": bad}))
            .await
            .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
}

#[tokio::test]
async fn get_without_selector_is_invalid_arguments() {
    let tree = Tree::new("get-args");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let err = call(&server, "get_entity", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert_eq!(err.name, "InvalidParams");
}

#[tokio::test]
async fn unknown_node_id_is_entity_not_found() {
    let tree = Tree::new("not-found");
    tree.install("defuse_poc.vmap", "maps/defuse_poc.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let err = call(
        &server,
        "get_entity",
        json!({"path": DEFUSE_ARG, "nodeID": 999999}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "EntityNotFound");
    assert_eq!(err.data["nodeID"], json!(999999));
    assert_eq!(err.data["path"], json!(DEFUSE_ARG));
    assert!(err.retry_safe);
}

#[tokio::test]
async fn path_escaping_the_content_root_is_refused_without_a_read() {
    let tree = Tree::new("escape");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let escape = "maps/../../etc/passwd.vmap";
    let err = call(&server, "get_entity", json!({"path": escape, "nodeID": 1}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidPath");
    assert!(err.message.contains(".."), "the `..` refusal is named");
    assert_eq!(err.data["path"], json!(escape));
    // Nothing outside was read: /etc/passwd.vmap does not exist and no FileNotFound for it —
    // the `..` refusal fires before any filesystem access.
    assert_ne!(err.name, "FileNotFound");
    assert!(!Path::new("/etc/passwd.vmap").exists());
}

#[tokio::test]
async fn no_path_without_a_bridge_is_no_document() {
    let tree = Tree::new("no-doc");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    for (tool, args) in [
        ("list_entities", json!({})),
        ("find_entities", json!({"class": "func_buyzone"})),
        ("get_entity", json!({"nodeID": 1})),
    ] {
        let err = call(&server, tool, args).await.unwrap_err();
        assert_eq!(err.name, "NoDocument", "{tool}");
        assert!(
            err.message.contains("path"),
            "{tool}: the message names the `path` argument"
        );
        assert!(err.retry_safe);
    }
}

// -------------------------------------------------------------------------------------------
// Path forms and Windows mapping
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn windows_path_form_resolves_through_the_path_map() {
    let tree = Tree::new("win-path");
    tree.install("probe.vmap", "maps/probe.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let out = call(
        &server,
        "list_entities",
        json!({"path": "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap"}),
    )
    .await
    .unwrap();
    assert_eq!(out["total"], json!(69));
    assert_eq!(out["source"]["hostPath"],
        json!(tree.addon().join("maps/probe.vmap").display().to_string()));
}

// -------------------------------------------------------------------------------------------
// The real game tree (#[ignore]; run with
// `cargo test -p hammer-mcp --test entities_file -- --ignored`)
// -------------------------------------------------------------------------------------------

/// `get_entity` on `template_wingman.vmap`'s `logic_auto` (nodeID 73) through the tool: the
/// `connections` array reaches the JSON result. Every checked-in fixture has empty
/// `connectionsData`, so this is the only case that exercises the connection path end to end.
///
/// The map is Valve content under the install's `content/csgo/` (not an addon), so it is copied at
/// test time into the temp app tree's addon — under `CARGO_TARGET_TMPDIR`, never /tmp and never the
/// repo, and the tool's read stays under `<app_root>/content/`. The expected record is the oracle's
/// (the retired oracle `vmap_dmx.py`, `Entity.connections`); data values only, no file bytes.
#[tokio::test]
#[ignore]
async fn real_tree_template_wingman_connections_through_get_entity() {
    let source = real_tree_file(TEMPLATE_WINGMAN);
    let tree = Tree::new("real-connections");
    tree.install_from(&source, "maps/template_wingman.vmap");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    let out = call(
        &server,
        "get_entity",
        json!({"path": "maps/template_wingman.vmap", "nodeID": 73}),
    )
    .await
    .unwrap();
    assert_eq!(out["source"]["fromDisk"], json!(true));
    assert_eq!(out["source"]["entityCount"], json!(40), "oracle: 40 entities");
    assert_eq!(out["staleRead"], json!(false));

    let entity = &out["entity"];
    assert_eq!(entity["nodeID"], json!(73));
    assert_eq!(entity["classname"], json!("logic_auto"));
    let connections = entity["connections"].as_array().expect("connections array");
    assert_eq!(
        connections,
        &vec![json!({
            "outputName": "OnMapSpawn",
            "targetType": 7,
            "targetName": "script.2v2",
            "inputName": "RunScriptCode",
            "overrideParam": "EnableWingman()",
            "delay": 0.0,
            "timesToFire": -1,
        })],
        "the oracle's single OnMapSpawn connection"
    );
    assert!(connections[0]["targetType"].is_i64(), "ints are ints");
    assert!(connections[0]["delay"].is_f64(), "delay is a float");
    println!("get_entity nodeID 73 connections: {}", entity["connections"]);
}

/// The project root: `$CS2_MCP_PROJECT_ROOT` first when it holds the tree, otherwise
/// found by walking up from the crate directory (a worktree sits several levels below the main
/// checkout — the `tests/entity_classes.rs` / `tests/fgd.rs` rule). Never optional: a missing game
/// tree panics loudly instead of skipping the test.
fn project_root() -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("CS2_MCP_PROJECT_ROOT") {
        candidates.push(PathBuf::from(root));
    }
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        candidates.push(dir.clone());
        if !dir.pop() {
            break;
        }
    }
    candidates
        .into_iter()
        .find(|dir| dir.join(CSGO_FGD).is_file())
        .unwrap_or_else(|| {
            panic!(
                "no project root with {CSGO_FGD} at $CS2_MCP_PROJECT_ROOT or above {}",
                env!("CARGO_MANIFEST_DIR")
            )
        })
}

/// One read-only file of the real game tree; panics loudly when it is missing.
fn real_tree_file(relative: &str) -> PathBuf {
    let path = project_root().join(relative);
    assert!(
        path.is_file(),
        "missing real map {}: the installed game tree moved?",
        path.display()
    );
    path
}

/// The tree marker (same as the other real-tree tests).
const CSGO_FGD: &str = "workshop/run/app/game/csgo/csgo.fgd";
/// A Valve template map with exactly one I/O connection (nodeID 73, `logic_auto`).
const TEMPLATE_WINGMAN: &str = "workshop/run/app/content/csgo/maps/templates/template_wingman.vmap";
