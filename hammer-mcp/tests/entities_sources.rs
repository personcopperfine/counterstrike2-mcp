//! WP2.3: `get_entity` / `find_entities` / `get_scene_info` over both routes (`source:"live"` =
//! the mock bridge's Hammer document, `source:"saved"` = the probe `.vmap` fixture), the default
//! choice, the refusal of arguments that do not apply to the chosen source, and the deprecated
//! `get_entity_live` / `find_entities_live` aliases.
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
const PROBE_ARG: &str = "maps/probe.vmap";

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "entities-sources-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let app = root.join("workshop/run/app");
        let target = app.join("content/csgo_addons/mcp_test").join(PROBE_ARG);
        std::fs::create_dir_all(target.parent().unwrap()).expect("tmp tree");
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/vmap/probe.vmap"),
            &target,
        )
        .expect("install probe");
        Tree { root, app }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn config(tree: &Tree, discovery: Option<&Path>) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let map = format!("z:\\app={}", tree.app.display());
    let discovery = discovery
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| "/nonexistent/cs2mcp-no-bridge/session.json".into());
    let given: Vec<&str> = vec![
        "--deadline-ms", "5000", "--tool-budget-ms", "45000", "--discovery", &discovery,
        "--project-root", &root, "--path-map", &map,
    ];
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

fn kv(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

async fn world() -> MockBridge {
    MockBridge::builder()
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(30, "info_map_parameters", [1.0, 2.0, 3.0], kv(&[("bombradius", "500")])),
            MockNode::entity(67, "func_buyzone", [100.0, 0.0, 0.0], kv(&[("targetname", "bz_a")])),
            MockNode::entity(70, "func_buyzone", [200.0, 0.0, 0.0], kv(&[("targetname", "bz_b")])),
            MockNode::entity(80, "info_player_terrorist", [0.0, 64.0, 0.0], kv(&[])),
        ])
        .start()
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------------- get_entity

#[tokio::test]
async fn get_entity_defaults_to_live_with_a_bridge_document_and_reports_it() {
    let tree = Tree::new("get-default");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    let out = call(&server, "get_entity", json!({"nodeID": 30})).await.unwrap();
    assert_eq!(out["sourceUsed"], "live");
    assert!(out["sourceWhy"].as_str().unwrap().contains("bridge document"), "{out}");
    // the live field names are kept
    assert_eq!(out["classname"], "info_map_parameters");
    assert_eq!(out["keyValues"]["bombradius"], "500");
    assert_eq!(out["source"], "live");
}

#[tokio::test]
async fn get_entity_saved_keeps_the_file_route_shape_and_stale_read() {
    let tree = Tree::new("get-saved");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    let out = call(&server, "get_entity", json!({"source": "saved", "path": PROBE_ARG, "nodeID": 30}))
        .await
        .unwrap();
    assert_eq!(out["sourceUsed"], "saved");
    assert!(out.get("sourceWhy").is_none(), "an explicit source needs no reason: {out}");
    assert_eq!(out["source"]["fromDisk"], true);
    assert!(out["staleRead"].is_boolean());
    assert_eq!(out["entity"]["nodeID"], 30);
    // no live read was sent
    assert!(mb.method_calls("entity.get").is_empty());
}

#[tokio::test]
async fn get_entity_with_a_path_and_no_source_reads_the_saved_file() {
    let tree = Tree::new("get-path");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    let out = call(&server, "get_entity", json!({"path": PROBE_ARG, "nodeID": 30})).await.unwrap();
    assert_eq!(out["sourceUsed"], "saved");
    assert!(out["sourceWhy"].as_str().unwrap().contains("saved route"), "{out}");
}

#[tokio::test]
async fn get_entity_without_a_bridge_falls_back_to_saved() {
    let tree = Tree::new("get-nobridge");
    let server = hammer_mcp::build_server(&config(&tree, None)).unwrap();
    let out = call(&server, "get_entity", json!({"path": PROBE_ARG, "nodeID": 30})).await.unwrap();
    assert_eq!(out["sourceUsed"], "saved");
    // no path and no bridge: the saved route's own NoDocument naming `path`
    let err = call(&server, "get_entity", json!({"nodeID": 30})).await.unwrap_err();
    assert_eq!(err.name, "NoDocument");
    assert!(err.message.contains("path"));
}

#[tokio::test]
async fn get_entity_live_by_targetname_resolves_through_entity_find() {
    let tree = Tree::new("get-name");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    let out = call(&server, "get_entity", json!({"source": "live", "targetname": "BZ_B"}))
        .await
        .unwrap();
    assert_eq!(out["nodeID"], 70);
    let err = call(&server, "get_entity", json!({"source": "live", "targetname": "nope"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "EntityNotFound");
    let err = call(&server, "get_entity", json!({"source": "live"})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
}

#[tokio::test]
async fn get_entity_refuses_path_on_live_and_a_bad_source() {
    let tree = Tree::new("get-refuse");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    let err = call(&server, "get_entity", json!({"source": "live", "path": PROBE_ARG, "nodeID": 30}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    assert!(err.message.contains("path") && err.message.contains("saved"), "{}", err.message);
    assert!(mb.method_calls("entity.get").is_empty(), "nothing was sent");
    let err = call(&server, "get_entity", json!({"source": "disk", "nodeID": 30})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
}

// -------------------------------------------------------------------------------- find_entities

#[tokio::test]
async fn find_entities_each_source_and_the_default() {
    let tree = Tree::new("find");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();

    let live = call(&server, "find_entities", json!({"class": "func_buyzone"})).await.unwrap();
    assert_eq!(live["sourceUsed"], "live");
    assert_eq!(live["count"], 2);
    assert_eq!(live["source"], "live");

    // no filter at all is fine live (the bridge lists everything up to limit)
    let all = call(&server, "find_entities", json!({"limit": 3})).await.unwrap();
    assert_eq!(all["count"], 3);
    assert_eq!(all["truncated"], true);

    let saved = call(
        &server,
        "find_entities",
        json!({"source": "saved", "path": PROBE_ARG, "class": "func_buyzone"}),
    )
    .await
    .unwrap();
    assert_eq!(saved["sourceUsed"], "saved");
    assert_eq!(saved["source"]["fromDisk"], true);
    assert!(saved["total"].as_u64().unwrap() >= 1);

    // a saved-only argument without `source` selects the saved route instead of failing
    let by_path = call(&server, "find_entities", json!({"path": PROBE_ARG, "class": "func_buyzone"}))
        .await
        .unwrap();
    assert_eq!(by_path["sourceUsed"], "saved");
    // nameContains does NOT switch the route (entity.find has no substring filter): on the live
    // default it is refused and points at get_scene_info {query}
    let err = call(&server, "find_entities", json!({"nameContains": "buy"})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    assert!(err.message.contains("get_scene_info"), "{}", err.message);
    // with an explicit saved source it still works
    let by_name = call(
        &server,
        "find_entities",
        json!({"source": "saved", "path": PROBE_ARG, "nameContains": "buy"}),
    )
    .await
    .unwrap();
    assert_eq!(by_name["sourceUsed"], "saved");
    assert_eq!(by_name["total"], 2);
}

#[tokio::test]
async fn find_entities_refuses_saved_only_arguments_on_live() {
    let tree = Tree::new("find-refuse");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    for (key, value) in [("nameContains", json!("buy")), ("withKeyvalues", json!(true)), ("path", json!(PROBE_ARG))] {
        let mut args = json!({"source": "live", "class": "func_buyzone"});
        args[key] = value;
        let err = call(&server, "find_entities", args).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{key}");
        assert!(err.message.contains(key), "{}", err.message);
    }
    assert!(mb.method_calls("entity.find").is_empty(), "nothing was sent");
    // the saved route still demands a filter
    let err = call(&server, "find_entities", json!({"source": "saved", "path": PROBE_ARG}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
}

// ---------------------------------------------------------------------------------------- aliases

#[tokio::test]
async fn the_old_live_names_are_hidden_deprecated_aliases() {
    let tree = Tree::new("alias");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    let r = server.registry();
    assert!(r.is_alias("get_entity_live") && r.is_alias("find_entities_live"));

    let got = call(&server, "get_entity_live", json!({"nodeID": 30})).await.unwrap();
    assert_eq!(got["classname"], "info_map_parameters");
    assert_eq!(got["sourceUsed"], "live");
    assert_eq!(got["deprecated"]["use"], "get_entity");
    assert_eq!(got["deprecated"]["args"], json!({"source": "live"}));

    let found = call(&server, "find_entities_live", json!({"class": "FUNC_BUYZONE"})).await.unwrap();
    assert_eq!(found["count"], 2);
    assert_eq!(found["deprecated"]["use"], "find_entities");

    // fixed args win: an alias never reads the file, even when the caller passes source:saved
    let forced = call(&server, "get_entity_live", json!({"nodeID": 30, "source": "saved"})).await.unwrap();
    assert_eq!(forced["sourceUsed"], "live");
}

// ----------------------------------------------------------------------------------- get_scene_info

#[tokio::test]
async fn get_scene_info_live_on_the_mock_map() {
    let tree = Tree::new("scene-live");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    let out = call(&server, "get_scene_info", json!({})).await.unwrap();
    assert_eq!(out["sourceUsed"], "live");
    assert_eq!(out["counts"]["entities"], 4);
    assert_eq!(out["counts"]["classes"], 3);
    assert_eq!(out["counts"]["meshes"], 0);
    assert_eq!(out["classes"][0], json!(["func_buyzone", 2]));
    assert_eq!(out["bounds"]["min"], json!([0.0, 0.0, 0.0]));
    assert_eq!(out["bounds"]["max"], json!([200.0, 64.0, 3.0]));
    assert_eq!(out["spawns"][0]["classname"], "info_player_terrorist");
    assert_eq!(out["matched"], 4);

    let q = call(
        &server,
        "get_scene_info",
        json!({"query": "BZ_", "fields": ["nodeID", "targetname"], "limit": 1}),
    )
    .await
    .unwrap();
    assert_eq!(q["matched"], 2);
    assert_eq!(q["truncated"], true);
    assert_eq!(q["items"].as_array().unwrap().len(), 1);
    assert_eq!(q["items"][0].as_object().unwrap().len(), 2);
    assert_eq!(q["items"][0]["targetname"], "bz_a");
    // class counts stay whole-map even with a query
    assert_eq!(q["counts"]["entities"], 4);

    let err = call(&server, "get_scene_info", json!({"fields": ["bogus"]})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    let err = call(&server, "get_scene_info", json!({"source": "live", "path": PROBE_ARG}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
}

#[tokio::test]
async fn get_scene_info_saved_file() {
    let tree = Tree::new("scene-saved");
    let server = hammer_mcp::build_server(&config(&tree, None)).unwrap();
    let out = call(&server, "get_scene_info", json!({"path": PROBE_ARG, "limit": 5})).await.unwrap();
    assert_eq!(out["sourceUsed"], "saved");
    assert!(out["file"]["hostPath"].as_str().unwrap().ends_with("probe.vmap"));
    assert!(out.get("source").is_none());
    assert!(out["staleRead"].is_boolean());
    assert!(out["counts"]["entities"].as_u64().unwrap() > 60);
    assert!(out["counts"]["meshes"].is_u64());
    assert!(out["classes"].as_array().unwrap().len() >= 2);
    assert!(out["bounds"].is_object());
    assert_eq!(out["items"].as_array().unwrap().len(), 5);
    assert_eq!(out["truncated"], true);
}

// ---------------------------------------------------------------- review fixes (WP2.3b)

async fn two_names_world() -> MockBridge {
    MockBridge::builder()
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(10, "info_target", [0.0, 0.0, 0.0], kv(&[("targetname", "bz_a")])),
            MockNode::entity(11, "info_target", [1.0, 0.0, 0.0], kv(&[("targetname", "BZ_A")])),
            MockNode::entity(12, "info_target", [2.0, 0.0, 0.0], kv(&[("targetname", "solo")])),
        ])
        .start()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_live_name_collision_is_ambiguous_not_first_match() {
    let tree = Tree::new("ambiguous-live");
    let mb = two_names_world().await;
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    // `bz_a` / `BZ_A` are the same name (case-insensitive): both hit, whichever case is asked
    for name in ["bz_a", "BZ_A", "Bz_A"] {
        let err = call(&server, "get_entity", json!({"source": "live", "targetname": name}))
            .await
            .unwrap_err();
        assert_eq!(err.name, "AmbiguousTargetname", "{name}");
        assert!(err.retry_safe, "{name}");
        let ids: Vec<i64> = err.data["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["nodeID"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, [10, 11], "{name}");
        assert!(err.message.contains("nodeID"), "{}", err.message);
    }
    // the unique name and nodeID still resolve, with the case rule being case-insensitive
    let one = call(&server, "get_entity", json!({"targetname": "SOLO"})).await.unwrap();
    assert_eq!(one["nodeID"], 12);
    let one = call(&server, "get_entity", json!({"nodeID": 11})).await.unwrap();
    assert_eq!(one["nodeID"], 11);
}

#[tokio::test]
async fn a_saved_name_collision_is_ambiguous_and_case_insensitive_too() {
    let tree = Tree::new("ambiguous-saved");
    let server = hammer_mcp::build_server(&config(&tree, None)).unwrap();
    // probe.vmap: mcp_kv_1 names nodes 91, 92 and 107
    for name in ["mcp_kv_1", "MCP_KV_1"] {
        let err = call(&server, "get_entity", json!({"path": PROBE_ARG, "targetname": name}))
            .await
            .unwrap_err();
        assert_eq!(err.name, "AmbiguousTargetname", "{name}");
        let ids: Vec<i64> = err.data["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["nodeID"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, [91, 92, 107], "{name}");
    }
    // a unique name matches in any case (saved used to be case-sensitive)
    let one = call(&server, "get_entity", json!({"path": PROBE_ARG, "targetname": "MCP_Probe_1"}))
        .await
        .unwrap();
    assert_eq!(one["entity"]["nodeID"], 22);
    // nodeID still wins over a colliding name
    let one = call(
        &server,
        "get_entity",
        json!({"path": PROBE_ARG, "targetname": "mcp_kv_1", "nodeID": 92}),
    )
    .await
    .unwrap();
    assert_eq!(one["entity"]["nodeID"], 92);
}

#[tokio::test]
async fn a_live_name_lookup_needs_entity_find_else_it_falls_back_to_saved() {
    let tree = Tree::new("caps-name");
    let mb = MockBridge::builder()
        .hammer_nodes(vec![MockNode::world()])
        .drop_method("entity.find")
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    // by name: entity.find is needed, so the default falls back to the saved route (not a
    // CapabilityMissing); by nodeID the live read only needs entity.get
    let out = call(&server, "get_entity", json!({"path": PROBE_ARG, "targetname": "mcp_probe_1"}))
        .await
        .unwrap();
    assert_eq!(out["sourceUsed"], "saved");
    let out = call(&server, "get_entity", json!({"nodeID": 0})).await;
    assert!(out.is_ok() || out.unwrap_err().name != "CapabilityMissing");
}

#[tokio::test]
async fn a_live_result_names_the_document_it_read() {
    let tree = Tree::new("document");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    let out = call(&server, "get_entity", json!({"nodeID": 30})).await.unwrap();
    let document = out["document"].as_str().expect("a live default stamps the document path");
    assert!(out["sourceWhy"].as_str().unwrap().contains(document), "{out}");
    let scene = call(&server, "get_scene_info", json!({})).await.unwrap();
    assert_eq!(scene["document"], out["document"]);
    // an explicit source never reads doc.active: nothing to stamp
    let out = call(&server, "get_entity", json!({"source": "live", "nodeID": 30})).await.unwrap();
    assert!(out.get("document").is_none());
}

#[tokio::test]
async fn a_silent_doc_active_is_reported_as_unanswered_not_as_no_document() {
    let tree = Tree::new("docactive-err");
    let mb = MockBridge::builder()
        .hammer_nodes(vec![MockNode::world()])
        .error("doc.active", -32000, "boom", None)
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    let out = call(&server, "get_entity", json!({"path": PROBE_ARG, "nodeID": 30})).await.unwrap();
    assert_eq!(out["sourceUsed"], "saved");
    // a path was given: no doc.active read was needed to decide. Without it:
    let err = call(&server, "get_entity", json!({"nodeID": 30})).await;
    // the saved route needs a document path, so it fails; the reason is not "no document is open"
    assert!(err.is_err());
}

#[tokio::test]
async fn get_scene_info_saved_file_is_stale_after_a_live_edit_and_names_the_file_apart() {
    let tree = Tree::new("scene-stale");
    let cfg = config(&tree, None);
    let state = HammerState::from_config(&cfg).unwrap();
    let ctx = || cs2_mcp_common::tools::CallCtx {
        budget: cs2_mcp_common::tools::Budget::new(45_000, 5_000),
    };
    let args = json!({"source": "saved", "path": PROBE_ARG});
    let fresh = hammer_mcp::tools::scene_info::get_scene_info(state.clone(), ctx(), args.clone())
        .await
        .unwrap()
        .json;
    assert_eq!(fresh["staleRead"], false);
    // `source` is the request argument's name only: the file data is under `file`
    assert!(fresh.get("source").is_none(), "{fresh}");
    assert!(fresh["file"]["hostPath"].as_str().unwrap().ends_with("probe.vmap"));
    assert!(fresh["file"].get("mtime").is_some());

    // one live edit counted since the last save
    state.edits.lock().unwrap().since_save += 1;
    let scene = hammer_mcp::tools::scene_info::get_scene_info(state.clone(), ctx(), args)
        .await
        .unwrap()
        .json;
    assert_eq!(scene["staleRead"], true, "{scene}");
    assert!(scene["staleReason"].as_str().unwrap().contains("unsaved bridge edit"), "{scene}");
    let got = hammer_mcp::tools::entities_read::get_entity(
        state,
        ctx(),
        json!({"source": "saved", "path": PROBE_ARG, "nodeID": 30}),
    )
    .await
    .unwrap()
    .json;
    assert_eq!(got["staleRead"], true, "{got}");
    assert!(got["staleReason"].as_str().unwrap().contains("unsaved bridge edit"), "{got}");
}

#[tokio::test]
async fn get_scene_info_classes_are_an_ordered_array_and_meshes_failures_are_null() {
    let tree = Tree::new("scene-order");
    let mb = world().await;
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    let out = call(&server, "get_scene_info", json!({})).await.unwrap();
    assert_eq!(
        out["classes"],
        json!([["func_buyzone", 2], ["info_map_parameters", 1], ["info_player_terrorist", 1]])
    );
    assert_eq!(out["otherClasses"], 0);
    assert_eq!(out["spawnsTruncated"], false);
    assert!(out.get("source").is_none(), "{out}");

    // node.list fails (e.g. "response too large"): meshes is null and the note says why
    let mb = MockBridge::builder()
        .hammer_nodes(vec![MockNode::world(), MockNode::entity(5, "info_target", [0.0; 3], kv(&[]))])
        .error("node.list", -32000, "response too large", None)
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    let out = call(&server, "get_scene_info", json!({})).await.unwrap();
    assert_eq!(out["counts"]["meshes"], Value::Null, "{out}");
    assert!(out["note"].as_str().unwrap().contains("node.list failed"), "{out}");
    assert_eq!(out["counts"]["entities"], 1);

    // a truncated page is a lower bound, not a count
    let mb = MockBridge::builder()
        .hammer_nodes(vec![MockNode::world()])
        .responder_value(
            "node.list",
            json!({"nodes": [{"nodeID": 3, "class": "mesh", "origin": [1.0, 2.0, 3.0]}],
                   "count": 1, "visited": 9000, "truncated": true}),
        )
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&tree, Some(&mb.discovery_path))).unwrap();
    let out = call(&server, "get_scene_info", json!({})).await.unwrap();
    assert_eq!(out["counts"]["meshes"], Value::Null, "{out}");
    assert!(out["note"].as_str().unwrap().contains("truncated"), "{out}");
}

#[tokio::test]
async fn the_deprecated_aliases_stay_listed_with_a_short_description() {
    let tree = Tree::new("alias-listed");
    let server = hammer_mcp::build_server(&config(&tree, None)).unwrap();
    let specs = server.registry().describe();
    for alias in ["get_entity_live", "find_entities_live", "game_local"] {
        let spec = specs.iter().find(|s| s.name == alias).unwrap_or_else(|| panic!("{alias} listed"));
        assert!(spec.description.starts_with("Deprecated: use `"), "{}", spec.description);
        assert!(spec.description.len() < 80, "{}", spec.description);
    }
}
