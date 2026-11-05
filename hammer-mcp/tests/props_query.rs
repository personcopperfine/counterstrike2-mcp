//! `list_props` tool tests (WP-P).
//!
//! File route: a fixture `.vmap` is GENERATED in the test from the committed `probe.vmap`
//! fixture by same-length classname patches ("func_buyzone" -> "prop_dynamic", "info_target" ->
//! "prop_static", "end_of_match" -> "prop_physics"; all occurrences, verified by count) into a
//! temp content tree under `CARGO_TARGET_TMPDIR` — the entities-file pattern; no Valve map is
//! committed or copied into the repo. Same-length replacement is layout-neutral for binary DMX
//! (length-prefixed strings), verified by the oracle reader during this WP's development: the
//! patched copy carries 17 props (14 `prop_static`, 2 `prop_dynamic`, 1 `prop_physics`).
//! A fixture FGD (written into the temp game tree) carries the `tags( Prop* )` classes; kind
//! classification through `base()` inheritance is asserted on it.
//!
//! Live route: the mock bridge (tests/entities_query.rs pattern) with a hand-built Hammer world;
//! the same fixture FGD proves the classification against live classnames; mock `error_on_call`
//! knobs are the negative passthrough cases.
//!
//! Every per-assert numeric expectation here was cross-checked against the Python oracle
//! reader on the patched copy and the real maps (WP-P facts); the `#[ignore]`d corpus tests run
//! `list_props` against the REAL game tree (real FGD + the real maps from the install, copied
//! into the temp tree at test time) and fail loudly when it is missing.
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

/// The FGD fixture: tags on base classes (`prop_dynamic_base`), a chain through
/// `base()` (`prop_dynamic_ornament` -> `prop_dynamic` -> `prop_dynamic_base`), a class that
/// places a model without a prop tag (`env_model_showoff`), and non-prop classes.
const FIXTURE_FGD: &str = r#"// Props fixture FGD (WP-P tests). Shape mirrors the real game's core/base.fgd:
// tags(Prop*) on base classes, children inherit the tag through base().

@BaseClass = Targetname
[
	targetname(target_source) : "Name"
]

@BaseClass base(Targetname) tags( PropDynamic ) = prop_dynamic_base
[
	skin(materialgroup) : "Skin" : "default"
	solid(intchoices) : "Collisions" : "6"
]

@PointClass base(prop_dynamic_base) = prop_dynamic : "Dynamic prop."

@PointClass base(prop_dynamic) = prop_dynamic_ornament : "Ornament prop."

@PointClass base(Targetname) tags( PropStatic ) = prop_static : "Static prop."
[
	solid(intchoices) : "Collisions" : "6"
]

@PointClass base(Targetname) tags( PropPhysics ) = prop_physics : "Physics prop."

// Places a model but is not a prop: the includeOtherModelEntities "other" path.
@PointClass base(Targetname) = env_model_showoff : "Showoff."
[
	model(studio) : "World Model"
]

// Non-prop declared classes of the fixture maps: excluded on both routes.
@SolidClass base(Targetname) = func_bomb_target : "Bomb target."
@PointClass = point_worldtext : "Text."
"#;

/// A temp project tree laid out like the real one: `--project-root` at `root`, app at
/// `workshop/run/app`, the FGD game tree at `workshop/run/app/game`, the addon under
/// `content/csgo_addons/mcp_test`.
struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "props-{tag}-{}-{}",
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

    fn game(&self) -> PathBuf {
        self.app.join("game")
    }

    /// Write the fixture FGD (`csgo/csgo.fgd` — the database's entry point).
    fn install_fgd(&self) {
        let target = self.game().join("csgo/csgo.fgd");
        std::fs::create_dir_all(target.parent().expect("csgo dir")).expect("fgd dir");
        std::fs::write(target, FIXTURE_FGD).expect("write fixture fgd");
    }

    /// Write the patched probe fixture into the addon and return its addon-relative path.
    /// Same-length classname replacement: binary-DMX strings are length-prefixed, so a
    /// same-length byte replace is layout-neutral. The occurrence counts are pinned to the
    /// committed fixture so an unexpected fixture change fails loudly.
    fn install_prop_map(&self) -> &'static str {
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/vmap/probe.vmap");
        let mut data = std::fs::read(&source).expect("committed probe.vmap fixture");
        for (from, to, expected) in [
            (
                b"func_buyzone".as_slice(),
                b"prop_dynamic".as_slice(),
                4usize,
            ),
            (b"info_target", b"prop_static", 3usize),
            (b"end_of_match", b"prop_physics", 4usize),
        ] {
            assert_eq!(from.len(), to.len(), "the patch must be same-length");
            let at: Vec<usize> = data
                .windows(from.len())
                .enumerate()
                .filter(|(_, window)| *window == from)
                .map(|(index, _)| index)
                .collect();
            assert_eq!(
                at.len(),
                expected,
                "the {} occurrence count changed (fixture update?): the patched map is not what these tests describe",
                std::str::from_utf8(from).expect("ascii")
            );
            for index in at {
                data[index..index + from.len()].copy_from_slice(to);
            }
        }
        let target = self.addon().join("maps/props_probe.vmap");
        std::fs::create_dir_all(target.parent().expect("maps dir")).expect("maps dir");
        std::fs::write(&target, &data).expect("write patched vmap");
        "maps/props_probe.vmap"
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

/// [`config`] plus a mock bridge's discovery file (the live-route tests).
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

fn prop_ids(out: &Value) -> Vec<i64> {
    out["props"]
        .as_array()
        .expect("props array")
        .iter()
        .map(|prop| prop["nodeID"].as_i64().expect("nodeID"))
        .collect()
}

/// The patched probe fixture's 17 props by kind: static 87..107, dynamic 67/70, physics 78 (the
/// oracle's records on the patched copy).
fn expect_by_kind(
    out: &Value,
    statics: u64,
    dynamics: u64,
    physics: u64,
    other: u64,
    unknown: u64,
) {
    let by_kind = out["summary"]["byKind"].as_object().expect("byKind");
    assert_eq!(by_kind["static"], json!(statics), "{out}");
    assert_eq!(by_kind["dynamic"], json!(dynamics), "{out}");
    assert_eq!(by_kind["physics"], json!(physics), "{out}");
    assert_eq!(by_kind["other"], json!(other), "{out}");
    assert_eq!(by_kind["unknown"], json!(unknown), "{out}");
}

// -------------------------------------------------------------------------------------------
// File route: acceptance
// -------------------------------------------------------------------------------------------

const PROPS_ARG: &str = "maps/props_probe.vmap";

#[tokio::test]
async fn file_list_reports_all_17_props_in_stable_order() {
    let tree = Tree::new("file-list");
    tree.install_fgd();
    tree.install_prop_map();
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    let out = call(&server, "list_props", json!({"path": PROPS_ARG}))
        .await
        .unwrap();
    assert_eq!(out["source"], json!("file"));
    assert_eq!(out["path"], json!(PROPS_ARG));
    assert_eq!(out["total"], json!(17), "{out}");
    assert_eq!(out["returned"], json!(17));
    assert_eq!(out["offset"], json!(0));
    assert_eq!(out["truncated"], json!(false));
    expect_by_kind(&out, 14, 2, 1, 0, 0);
    assert!(out["props"].is_array());

    // Scan order is the document element order (probe.vmap holds 70 before 67) — stable, and
    // the props keep it.
    let ids = prop_ids(&out);
    assert_eq!(
        ids,
        vec![70, 67, 78, 87, 88, 89, 90, 91, 92, 93, 94, 96, 97, 98, 99, 101, 107]
    );

    // The default view carries no keyvalues; every field is named even when empty.
    let first = &out["props"][0];
    assert_eq!(first["nodeID"], json!(70));
    assert_eq!(first["classname"], json!("prop_dynamic"));
    assert_eq!(first["kind"], json!("dynamic"));
    assert_eq!(first["model"], Value::Null, "no model keyvalue -> null");
    assert_eq!(first["skin"], Value::Null);
    assert_eq!(first["solid"], Value::Null);
    assert_eq!(first["targetname"], Value::Null);
    assert_eq!(first["origin"], json!([944.0, -320.0, 32.0]));
    assert!(first["angles"].is_array());
    assert!(first["scales"].is_array());
    assert_eq!(first["problems"], json!(["no model"]));
    assert!(first["keyvalues"].is_null(), "default: no keyvalues");

    // The file envelope separates the file facts from the route name.
    assert_eq!(out["file"]["fromDisk"], json!(true));
    assert_eq!(
        out["file"]["hostPath"],
        json!(tree
            .addon()
            .join("maps/props_probe.vmap")
            .display()
            .to_string())
    );
    assert!(out["file"]["mtime"].is_u64());
    assert_eq!(out["file"]["entityCount"], json!(69));
    assert_eq!(out["staleRead"], json!(false));
    assert_eq!(out["staleReason"], Value::Null);
    assert_eq!(out["unsavedBridgeEdits"], json!(0));

    // No model in the fixture means no byModel rows, and notes stays an array.
    assert_eq!(out["summary"]["byModel"], json!([]));
    assert!(out["notes"].as_array().expect("notes").is_empty());
}

#[tokio::test]
async fn file_classifies_kind_through_fgd_inheritance_and_paging() {
    let tree = Tree::new("file-kind");
    tree.install_fgd();
    tree.install_prop_map();
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    // kind filter (the classification itself is by FGD tag: prop_dynamic's tag lives on
    // prop_dynamic_base, one base() hop up).
    let out = call(
        &server,
        "list_props",
        json!({"path": PROPS_ARG, "kind": "dynamic"}),
    )
    .await
    .unwrap();
    assert_eq!(prop_ids(&out), vec![70, 67], "scan order, not sorted order");
    assert_eq!(out["total"], json!(2));
    let out = call(
        &server,
        "list_props",
        json!({"path": PROPS_ARG, "kind": "physics"}),
    )
    .await
    .unwrap();
    assert_eq!(prop_ids(&out), vec![78]);
    let out = call(
        &server,
        "list_props",
        json!({"path": PROPS_ARG, "kind": "static"}),
    )
    .await
    .unwrap();
    assert_eq!(
        prop_ids(&out),
        vec![87, 88, 89, 90, 91, 92, 93, 94, 96, 97, 98, 99, 101, 107]
    );

    // Paging: limit truncates with total untouched; offset slices, truncated stays truthful.
    let out = call(
        &server,
        "list_props",
        json!({"path": PROPS_ARG, "limit": 5}),
    )
    .await
    .unwrap();
    assert_eq!(out["total"], json!(17));
    assert_eq!(out["returned"], json!(5));
    assert_eq!(out["truncated"], json!(true));
    let out = call(
        &server,
        "list_props",
        json!({"path": PROPS_ARG, "limit": 5, "offset": 15}),
    )
    .await
    .unwrap();
    assert_eq!(out["returned"], json!(2));
    assert_eq!(out["truncated"], json!(false));
    assert_eq!(prop_ids(&out), vec![101, 107]);
    let out = call(
        &server,
        "list_props",
        json!({"path": PROPS_ARG, "limit": 5, "offset": 20}),
    )
    .await
    .unwrap();
    assert_eq!(out["returned"], json!(0));
    assert_eq!(out["truncated"], json!(false));
    assert_eq!(out["props"], json!([]));
}

#[tokio::test]
async fn file_class_targetname_within_and_keyvalue_filters() {
    let tree = Tree::new("file-filters");
    tree.install_fgd();
    tree.install_prop_map();
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    // class: exact, case-insensitive.
    let out = call(
        &server,
        "list_props",
        json!({"path": PROPS_ARG, "class": "PROP_STATIC"}),
    )
    .await
    .unwrap();
    assert_eq!(out["total"], json!(14));

    // targetname: exact, case-insensitive (fixture: mcp_kv_1 on three prop_static).
    let out = call(
        &server,
        "list_props",
        json!({"path": PROPS_ARG, "targetname": "MCP_KV_1"}),
    )
    .await
    .unwrap();
    assert_eq!(prop_ids(&out), vec![91, 92, 107], "{out}");

    // within: an axis-aligned box on the origin (oracle: ten props at [128..480, 128..480]).
    let out = call(
        &server,
        "list_props",
        json!({
            "path": PROPS_ARG,
            "within": {"min": [100.0, 100.0, -1.0], "max": [500.0, 500.0, 1.0]}
        }),
    )
    .await
    .unwrap();
    let mut ids = prop_ids(&out);
    ids.sort();
    assert_eq!(ids, vec![90, 91, 92, 93, 94, 96, 97, 98, 99, 107]);
    assert_eq!(out["total"], json!(10));

    // includeKeyValues adds the entity's keyvalues (DMX strings).
    let out = call(
        &server,
        "list_props",
        json!({"path": PROPS_ARG, "includeKeyValues": true, "limit": 2}),
    )
    .await
    .unwrap();
    let keyvalues = out["props"][0]["keyvalues"].as_object().expect("keyvalues");
    assert_eq!(keyvalues["classname"], json!("prop_dynamic"), "{out}");
    assert!(out["props"][0]["keyvalues"].is_object());

    // includeOtherModelEntities changes nothing here: the fixture's non-prop classes carry no
    // `model` keyvalue and its unknown classes stay out either way.
    let out = call(
        &server,
        "list_props",
        json!({"path": PROPS_ARG, "includeOtherModelEntities": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["total"], json!(17));
    expect_by_kind(&out, 14, 2, 1, 0, 0);
}

/// The file route's staleRead follows the same unsaved counter as the entity file tools
/// (handler called directly: the registry holds the server's private state).
#[tokio::test]
async fn file_stale_read_follows_the_unsaved_counter() {
    let tree = Tree::new("file-stale");
    tree.install_fgd();
    tree.install_prop_map();
    let cfg = config(&tree);
    let state = HammerState::from_config(&cfg).unwrap();
    let args = json!({"path": PROPS_ARG});

    let out = hammer_mcp::tools::props_query::list_props(state.clone(), ctx(), args.clone())
        .await
        .unwrap();
    assert_eq!(out.json["staleRead"], json!(false));
    assert_eq!(out.json["staleReason"], Value::Null);

    {
        let mut edits = state.edits.lock().unwrap();
        edits.since_save += 1;
    }
    let out = hammer_mcp::tools::props_query::list_props(state, ctx(), args)
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
}

// -------------------------------------------------------------------------------------------
// File route: required negatives
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn bad_arguments_are_invalid_arguments_before_any_work() {
    let tree = Tree::new("bad-args");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    for bad in [
        json!({"path": PROPS_ARG, "limit": 0}),
        json!({"path": PROPS_ARG, "limit": 2001}),
        json!({"path": PROPS_ARG, "limit": "5"}),
        json!({"path": PROPS_ARG, "offset": -1}),
        json!({"path": PROPS_ARG, "offset": 1.5}),
        json!({"path": PROPS_ARG, "kind": "statics"}),
        json!({"path": PROPS_ARG, "kind": 3}),
        json!({"path": PROPS_ARG, "source": "disk"}),
        json!({"path": PROPS_ARG, "source": 3}),
        json!({"path": PROPS_ARG, "includeKeyValues": "yes"}),
        json!({"path": PROPS_ARG, "includeOtherModelEntities": 1}),
        // an empty filter matches everything while pretending to filter
        json!({"path": PROPS_ARG, "class": ""}),
        json!({"path": PROPS_ARG, "targetname": ""}),
        json!({"path": PROPS_ARG, "model": ""}),
        // box shape
        json!({"path": PROPS_ARG, "within": {"min": [1.0, 2.0, 3.0]}}),
        json!({"path": PROPS_ARG, "within": {"max": [1.0, 2.0, 3.0]}}),
        json!({"path": PROPS_ARG, "within": {"min": [1.0, 2.0], "max": [1.0, 2.0, 3.0]}}),
        json!({"path": PROPS_ARG, "within": {"min": [1.0, 2.0, "x"], "max": [1.0, 2.0, 3.0]}}),
        json!({"path": PROPS_ARG, "within": {"min": [1.0, 2.0, 3.0], "max": [1.0, 2.0, 3.0], "pad": 1}}),
        // min > max on one axis
        json!({"path": PROPS_ARG, "within": {"min": [500.0, 100.0, -1.0], "max": [100.0, 500.0, 1.0]}}),
        // unknown argument
        json!({"path": PROPS_ARG, "modelContains": "box"}),
        // contradictory route
        json!({"source": "live", "path": PROPS_ARG}),
        // a non-string path is refused in the argument phase, not silently treated as
        // "no path" (which would reroute the call to live / to the active document)
        json!({"path": 123}),
        json!({"source": "live", "path": 123}),
    ] {
        let err = call(&server, "list_props", bad.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}: {}", err.type_);
        assert_eq!(err.name, "InvalidParams", "{bad}");
        assert!(err.retry_safe, "{bad}");
    }
}

/// Arguments are validated before the FGD (or any file) is touched: a tree without a game root
/// answers the argument problem, not FgdUnavailable.
#[tokio::test]
async fn bad_arguments_precede_the_fgd_load() {
    let tree = Tree::new("bad-args-first");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let err = call(
        &server,
        "list_props",
        json!({"path": PROPS_ARG, "kind": "bogus"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments", "{}", err.type_);

    // Same call with valid args → the FGD problem, never an empty success.
    let err = call(&server, "list_props", json!({"path": PROPS_ARG}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "FgdUnavailable", "{}", err.type_);
    assert!(err.message.contains("FGD"), "{}", err.message);
}

#[tokio::test]
async fn file_missing_is_file_not_found() {
    let tree = Tree::new("file-missing");
    tree.install_fgd();
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let err = call(&server, "list_props", json!({"path": "maps/nope.vmap"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "FileNotFound");
    assert_eq!(err.data["path"], json!("maps/nope.vmap"));
    assert!(err.retry_safe);
}

/// source:"file" without a path needs the bridge's active document; without a bridge that is
/// NoDocument (the file-route rule).
#[tokio::test]
async fn file_without_a_path_needs_a_document() {
    let tree = Tree::new("file-no-doc");
    tree.install_fgd();
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let err = call(&server, "list_props", json!({"source": "file"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "NoDocument", "{}", err.name);
    assert!(err.message.contains("path"), "{}", err.message);
    assert!(err.retry_safe);
}

/// Default source without a path is live: a dead bridge surfaces as BridgeNotRunning (the
/// request itself, no silent empty success).
#[tokio::test]
async fn default_live_without_a_bridge_is_bridge_not_running() {
    let tree = Tree::new("live-no-bridge");
    tree.install_fgd();
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let err = call(&server, "list_props", json!({})).await.unwrap_err();
    assert_eq!(err.name, "BridgeNotRunning", "{}", err.name);
    assert_eq!(err.type_, "BridgeNotRunning");
}

// -------------------------------------------------------------------------------------------
// Live route (mock bridge)
// -------------------------------------------------------------------------------------------

/// The mock Hammer world: worldspawn + props (tagged classes), a non-tagged class that places a
/// model (env_model_showoff), an unknown class with a model (gone_class), the same unknown class
/// without, and a declared non-prop. The FGD is the fixture above.
async fn live_world() -> cs2_mcp_common::mock::MockBridge {
    use cs2_mcp_common::mock::{MockBridge, MockNode};
    let kv = |pairs: &[(&str, &str)]| -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    MockBridge::builder()
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(
                5,
                "prop_static",
                [0.0, 0.0, 0.0],
                kv(&[
                    ("model", "models/props/box.vmdl"),
                    ("skin", "default"),
                    ("targetname", "box_a"),
                ]),
            ),
            MockNode::entity(
                6,
                "prop_dynamic",
                [10.0, 0.0, 0.0],
                kv(&[
                    ("model", "MODELS/props/box.vmdl"),
                    ("skin", "2"),
                    ("solid", "6"),
                    ("targetname", "dyn"),
                ]),
            ),
            MockNode::entity(
                7,
                "prop_physics",
                [20.0, 0.0, 0.0],
                kv(&[("solid", "6"), ("targetname", "phys")]),
            ),
            MockNode::entity(
                8,
                "prop_dynamic",
                [6.0, 0.0, 0.0],
                kv(&[("model", "models/props/box.vmdl")]),
            ),
            MockNode::entity(
                9,
                "prop_static",
                [3.0, 0.0, 0.0],
                kv(&[("model", "models/props/alpha.vmdl")]),
            ),
            MockNode::entity(
                10,
                "prop_static",
                [4.0, 0.0, 0.0],
                kv(&[("model", "models/props/alpha.vmdl")]),
            ),
            MockNode::entity(
                11,
                "env_model_showoff",
                [30.0, 0.0, 0.0],
                kv(&[("model", "models/props/env.vmdl")]),
            ),
            MockNode::entity(
                12,
                "gone_class",
                [40.0, 0.0, 0.0],
                kv(&[("model", "models/props/gone.vmdl")]),
            ),
            MockNode::entity(13, "gone_class", [41.0, 0.0, 0.0], kv(&[])),
            MockNode::entity(14, "point_worldtext", [50.0, 0.0, 0.0], kv(&[])),
        ])
        .start()
        .await
        .expect("mock bridge")
}

/// A live-route server over [`live_world`] (with the fixture FGD) and its bridge handle.
async fn live_server(
    tag: &str,
) -> (
    Tree,
    McpServer<HammerState>,
    cs2_mcp_common::mock::MockBridge,
) {
    let tree = Tree::new(tag);
    let mb = live_world().await;
    tree.install_fgd();
    let server = hammer_mcp::build_server(&config_with_bridge(&tree, &mb.discovery_path)).unwrap();
    (tree, server, mb)
}

#[tokio::test]
async fn live_lists_the_tagged_props_and_classifies_by_fgd() {
    let (_tree, server, mb) = live_server("live-list").await;

    let out = call(&server, "list_props", json!({})).await.unwrap();
    assert_eq!(out["source"], json!("live"), "{out}");
    assert_eq!(out["total"], json!(6), "the tagged classes only");
    expect_by_kind(&out, 3, 2, 1, 0, 0);
    // entity.find was scanned at full width and the model filter is over the read keyvalues.
    let find = mb.method_calls("entity.find");
    assert_eq!(find.len(), 1);
    assert_eq!(find[0].params["limit"], json!(2000));
    assert!(find[0].params.get("class").is_none());
    assert!(find[0].params.get("targetname").is_none());
    assert_eq!(
        mb.method_calls("entity.get").len(),
        6,
        "one read per examined prop"
    );

    let ids = prop_ids(&out);
    assert_eq!(ids, vec![5, 6, 7, 8, 9, 10]);

    let dyn_prop = out["props"]
        .as_array()
        .unwrap()
        .iter()
        .find(|prop| prop["nodeID"] == json!(6))
        .expect("prop 6");
    assert_eq!(
        dyn_prop["model"],
        json!("MODELS/props/box.vmdl"),
        "the entity's own keyvalue"
    );
    assert_eq!(dyn_prop["skin"], json!("2"));
    assert_eq!(dyn_prop["solid"], json!("6"));
    assert_eq!(dyn_prop["targetname"], json!("dyn"));
    assert_eq!(dyn_prop["kind"], json!("dynamic"));
    assert_eq!(
        dyn_prop["angles"],
        json!([0.0, 0.0, 0.0]),
        "from entity.get"
    );
    assert_eq!(
        dyn_prop["scales"],
        json!([1.0, 1.0, 1.0]),
        "the m2.18 read reports the node's scales (identity here): the tool passes the live value \
         through (review W2 3c)"
    );
    assert_eq!(dyn_prop["problems"], json!([]));

    // The no-model prop is named honestly.
    let no_model = out["props"]
        .as_array()
        .unwrap()
        .iter()
        .find(|prop| prop["nodeID"] == json!(7))
        .expect("prop 7");
    assert_eq!(no_model["model"], Value::Null);
    assert_eq!(no_model["problems"], json!(["no model"]));

    // Live reports the real node scales, never an invented identity: the mock's stored scales for
    // prop 5 are [2, 1, 0.5] here, so the tool must report exactly those.
    mb.set_hammer_scales(5, [2.0, 1.0, 0.5]);
    let out = call(&server, "list_props", json!({})).await.unwrap();
    let scaled = out["props"]
        .as_array()
        .unwrap()
        .iter()
        .find(|prop| prop["nodeID"] == json!(5))
        .expect("prop 5");
    assert_eq!(scaled["scales"], json!([2.0, 1.0, 0.5]), "{scaled}");
    // ... and the note about scales only appears when a read did not carry the field
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        !notes
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("scales")),
        "the m2.18 read reports scales: no missing-scales note: {notes:?}"
    );
    assert!(
        !notes
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("first")),
        "no scan-cap note below the cap: {notes:?}"
    );
}

#[tokio::test]
async fn live_include_other_and_kind_unknown() {
    let (_tree, server, _mb) = live_server("live-other").await;

    // Default: the non-tagged classes are not props.
    let out = call(&server, "list_props", json!({})).await.unwrap();
    assert_eq!(out["total"], json!(6));

    // includeOtherModelEntities adds the model-studio class and the unknown WITH a model.
    let out = call(
        &server,
        "list_props",
        json!({"includeOtherModelEntities": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["total"], json!(8), "{out}");
    expect_by_kind(&out, 3, 2, 1, 1, 1);
    let other = out["props"]
        .as_array()
        .unwrap()
        .iter()
        .find(|prop| prop["nodeID"] == json!(11))
        .expect("other");
    assert_eq!(other["kind"], json!("other"));
    assert_eq!(other["classname"], json!("env_model_showoff"));
    assert_eq!(other["model"], json!("models/props/env.vmdl"));
    let unknown = out["props"]
        .as_array()
        .unwrap()
        .iter()
        .find(|prop| prop["nodeID"] == json!(12))
        .expect("unknown with model");
    assert_eq!(unknown["kind"], json!("unknown"));
    assert_eq!(unknown["problems"], json!(["unknown class"]));

    // A non-FGD class WITHOUT a model keyvalue is not a model entity and stays out.
    assert!(!prop_ids(&out).contains(&13));
    // A declared non-prop class (no tag, no model(studio)) stays out too.
    assert!(!prop_ids(&out).contains(&14));

    // kind filters the kinds even with includeOther.
    let out = call(
        &server,
        "list_props",
        json!({"includeOtherModelEntities": true, "kind": "static"}),
    )
    .await
    .unwrap();
    assert_eq!(prop_ids(&out), vec![5, 9, 10]);

    // "other"/"unknown" are filterable kinds directly now (finding: they were not in the enum).
    let out = call(
        &server,
        "list_props",
        json!({"includeOtherModelEntities": true, "kind": "other"}),
    )
    .await
    .unwrap();
    assert_eq!(prop_ids(&out), vec![11]);
    let out = call(
        &server,
        "list_props",
        json!({"includeOtherModelEntities": true, "kind": "unknown"}),
    )
    .await
    .unwrap();
    assert_eq!(prop_ids(&out), vec![12], "13 has no model and stays out");
    // without includeOtherModelEntities there is nothing for those kinds to match.
    let out = call(&server, "list_props", json!({"kind": "other"}))
        .await
        .unwrap();
    assert_eq!(out["total"], json!(0), "{out}");
}

#[tokio::test]
async fn live_filters_paging_and_summary_ordering() {
    let (_tree, server, _mb) = live_server("live-filters").await;

    // model: case-insensitive substring over the keyvalue.
    let out = call(&server, "list_props", json!({"model": "BOX"}))
        .await
        .unwrap();
    assert_eq!(prop_ids(&out), vec![5, 6, 8]);
    assert_eq!(out["total"], json!(3));

    // class / targetname (exactly, case-insensitively).
    let out = call(&server, "list_props", json!({"class": "PROP_DYNAMIC"}))
        .await
        .unwrap();
    assert_eq!(prop_ids(&out), vec![6, 8]);
    let out = call(&server, "list_props", json!({"targetname": "BOX_A"}))
        .await
        .unwrap();
    assert_eq!(prop_ids(&out), vec![5]);

    // within: box on the origin.
    let out = call(
        &server,
        "list_props",
        json!({"within": {"min": [2.0, -5.0, -5.0], "max": [25.0, 5.0, 5.0]}}),
    )
    .await
    .unwrap();
    assert_eq!(prop_ids(&out), vec![6, 7, 8, 9, 10]);
    assert_eq!(out["total"], json!(5));

    // summary.byModel: top by count then name — box(3), alpha(2), gone? default no others.
    let out = call(&server, "list_props", json!({})).await.unwrap();
    let by_model = out["summary"]["byModel"].as_array().expect("byModel");
    assert_eq!(
        by_model,
        &vec![
            json!({"model": "models/props/box.vmdl", "count": 3}),
            json!({"model": "models/props/alpha.vmdl", "count": 2})
        ]
    );
    // includeKeyValues shows the live keyvalues verbatim.
    let out = call(
        &server,
        "list_props",
        json!({"includeKeyValues": true, "limit": 1}),
    )
    .await
    .unwrap();
    assert_eq!(
        out["props"][0]["keyvalues"]["model"],
        json!("models/props/box.vmdl")
    );

    // paging.
    let out = call(&server, "list_props", json!({"limit": 2, "offset": 3}))
        .await
        .unwrap();
    assert_eq!(out["total"], json!(6));
    assert_eq!(out["returned"], json!(2));
    assert_eq!(out["truncated"], json!(true));
    assert_eq!(prop_ids(&out), vec!(8, 9));
    let out = call(&server, "list_props", json!({"limit": 2, "offset": 5}))
        .await
        .unwrap();
    assert_eq!(out["returned"], json!(1));
    assert_eq!(out["truncated"], json!(false));
}

#[tokio::test]
async fn bad_arguments_live_are_refused_before_anything_is_sent() {
    let (_tree, server, mb) = live_server("live-bad-args").await;
    for bad in [
        json!({"kind": "all"}),
        json!({"limit": 0}),
        json!({"unknown": 1}),
        json!({"within": {"min": [2.0, 2.0, 2.0], "max": [1.0, 1.0, 1.0]}}),
        json!({"class": ""}),
    ] {
        let err = call(&server, "list_props", bad.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
    }
    assert!(mb.method_calls("entity.find").is_empty());
    assert!(mb.method_calls("entity.get").is_empty());
}

/// Finding: the SCAN_CAP examine window is filled only by candidates the caller's filters
/// keep — `kind` and `within` are decidable from the `entity.find` row, so they apply BEFORE
/// the window (a map with more than SCAN_CAP statics ahead of a few physics props used to
/// answer `total: 0` with the physics props cut by the window, and burned up to 512 bridge
/// reads on rows the caller had excluded).
#[tokio::test]
async fn live_scan_cap_spends_no_window_slots_on_excluded_candidates() {
    use cs2_mcp_common::mock::{MockBridge, MockNode};
    let kv = |pairs: &[(&str, &str)]| -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    // 600 prop_statics far from the origin, then 3 prop_physics: over SCAN_CAP excluded
    // candidates before any candidate the filters want.
    let mut nodes = vec![MockNode::world()];
    for index in 0..600i64 {
        nodes.push(MockNode::entity(
            10 + index,
            "prop_static",
            [1000.0 + index as f64, 0.0, 0.0],
            kv(&[("model", "models/props/static.vmdl")]),
        ));
    }
    for (index, id) in [700i64, 701, 702].into_iter().enumerate() {
        nodes.push(MockNode::entity(
            id,
            "prop_physics",
            [1.0 + index as f64, 0.0, 0.0],
            kv(&[("model", "models/props/physics.vmdl")]),
        ));
    }
    let tree = Tree::new("scan-cap");
    let mb = MockBridge::builder()
        .hammer_nodes(nodes)
        .start()
        .await
        .unwrap();
    tree.install_fgd();
    let server = hammer_mcp::build_server(&config_with_bridge(&tree, &mb.discovery_path)).unwrap();

    // kind: the physics props are in the result, no scan-cap note, and only 3 reads.
    let out = call(&server, "list_props", json!({"kind": "physics"}))
        .await
        .unwrap();
    assert_eq!(out["total"], json!(3), "{out}");
    assert_eq!(prop_ids(&out), vec![700, 701, 702]);
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        !notes
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("first")),
        "no scan-cap note: {notes:?}"
    );
    let reads = mb.method_calls("entity.get").len();
    assert_eq!(reads, 3, "one read per kept candidate, none for statics");

    // within: the same fixture, box around the origin — the statics (x >= 1000) are outside.
    let out = call(
        &server,
        "list_props",
        json!({"within": {"min": [0.0, -1.0, -1.0], "max": [10.0, 1.0, 1.0]}}),
    )
    .await
    .unwrap();
    assert_eq!(out["total"], json!(3), "{out}");
    assert_eq!(prop_ids(&out), vec![700, 701, 702]);
    assert_eq!(
        mb.method_calls("entity.get").len(),
        reads + 3,
        "still no reads on the excluded statics"
    );
}

#[tokio::test]
async fn live_find_error_is_passed_through() {
    use cs2_mcp_common::mock::{MockBridge, MockNode};
    let tree = Tree::new("live-find-error");
    let mb = MockBridge::builder()
        .hammer_nodes(vec![MockNode::world()])
        .error(
            "entity.find",
            -32004,
            "DocumentChanged",
            Some(json!({"reason": "expectDoc"})),
        )
        .start()
        .await
        .unwrap();
    tree.install_fgd();
    let server = hammer_mcp::build_server(&config_with_bridge(&tree, &mb.discovery_path)).unwrap();

    let err = call(&server, "list_props", json!({})).await.unwrap_err();
    assert_eq!(err.code, Some(-32004), "{}", err.name);
    assert_eq!(err.name, "DocumentChanged");
    assert!(
        err.retry_safe,
        "nothing was executed (expectDoc refused it)"
    );
}

/// A failed per-prop read does NOT invent fields: the prop stays a prop with a problem entry
/// naming the failure, and the model stays null.
#[tokio::test]
async fn live_entity_get_failure_is_a_problem_not_a_silent_empty() {
    use cs2_mcp_common::mock::{MockBridge, MockNode};
    let tree = Tree::new("live-get-error");
    let mb = MockBridge::builder()
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(5, "prop_static", [0.0, 0.0, 0.0], {
                let mut map = std::collections::BTreeMap::new();
                map.insert("model".to_string(), "models/props/box.vmdl".to_string());
                map
            }),
        ])
        .error(
            "entity.get",
            -32022,
            "NodeNotFound",
            Some(json!({"nodeID": 5})),
        )
        .start()
        .await
        .unwrap();
    tree.install_fgd();
    let server = hammer_mcp::build_server(&config_with_bridge(&tree, &mb.discovery_path)).unwrap();

    let out = call(&server, "list_props", json!({})).await.unwrap();
    assert_eq!(out["total"], json!(1), "the prop is still a prop");
    let prop = &out["props"][0];
    assert_eq!(prop["nodeID"], json!(5));
    assert_eq!(prop["model"], Value::Null, "never invented");
    let problems = prop["problems"].as_array().expect("problems");
    assert!(
        problems
            .iter()
            .any(|p| p.as_str().unwrap_or_default().contains("live read failed")),
        "{problems:?}"
    );
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        !notes.is_empty(),
        "the notes carry what happened (no silent fallback)"
    );
    assert_eq!(
        prop["scales"],
        Value::Null,
        "a failed read supplies no scales (review W2 3c: null, never an invented identity)"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("scales")),
        "the scales note covers the failed read: {notes:?}"
    );
}

// -------------------------------------------------------------------------------------------
// The real game tree (#[ignore]; run with `cargo test -p hammer-mcp --test props_query -- --ignored`)
// -------------------------------------------------------------------------------------------

/// `$CS2_MCP_PROJECT_ROOT` first, else walk up from the crate dir (the other real-tree tests'
/// rule); fails loudly rather than skipping.
fn project_root() -> PathBuf {
    let marker = "workshop/run/app/game/csgo/csgo.fgd";
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
        .find(|dir| dir.join(marker).is_file())
        .unwrap_or_else(|| {
            panic!(
                "no project root with {marker} at $CS2_MCP_PROJECT_ROOT or above {}",
                env!("CARGO_MANIFEST_DIR")
            )
        })
}

fn real_tree_file(relative: &str) -> PathBuf {
    let path = project_root().join(relative);
    assert!(
        path.is_file(),
        "missing real file {}: the installed game tree moved?",
        path.display()
    );
    path
}

const SMARTPROP_ZOO: &str = "workshop/run/app/content/csgo/maps/editor/zoo/smartprop_zoo.vmap";
const CS_SCRIPT_DEMO: &str =
    "workshop/run/app/content/csgo_addons/cs_script_demo/maps/cs_script_demo.vmap";

/// The REAL FGD tags classify the REAL maps: smartprop_zoo (the oracle: 164 entities, 102 props:
/// 41 prop_static / 4 prop_dynamic / 57 prop_physics_multiplayer — the physics props inherit
/// their tag from BasePropPhysics several bases deep) and cs_script_demo (183 entities,
/// 17 props: 7 static + 10 dynamic). None carry a "no model" problem.
#[tokio::test]
#[ignore]
async fn real_tree_maps_classify_through_the_real_fgd() {
    let root = project_root();
    // The real tree IS the project root (its game/ provides the FGD); the maps are read IN PLACE
    // over their absolute host paths (read-only Valve content; nothing is copied and nothing
    // enters the repo).
    let config = {
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: CommonConfig,
        }
        let root_text = root.display().to_string();
        let map = format!("z:\\app={}", root.join("workshop/run/app").display());
        let given: Vec<&str> = vec![
            "--deadline-ms",
            "15000",
            "--tool-budget-ms",
            "60000",
            "--project-root",
            &root_text,
            "--path-map",
            &map,
        ];
        let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
        W::parse_from(argv).c
    };
    let server = hammer_mcp::build_server(&config).unwrap();

    let zoo = real_tree_file(SMARTPROP_ZOO).display().to_string();
    let out = call(&server, "list_props", json!({"path": zoo}))
        .await
        .unwrap();
    assert_eq!(out["total"], json!(102), "{}", out["total"]);
    expect_by_kind(&out, 41, 4, 57, 0, 0);
    for prop in out["props"].as_array().unwrap() {
        assert_eq!(prop["problems"], json!([]), "{}", prop["nodeID"]);
        assert!(prop["model"].is_string(), "{}", prop["nodeID"]);
    }
    let notes = out["notes"].as_array().expect("notes");
    assert!(notes.is_empty(), "{notes:?}");
    // byModel's top entries are real model paths, counts 2+ (several props share a model).
    let by_model = out["summary"]["byModel"].as_array().expect("byModel");
    assert!(by_model
        .first()
        .is_some_and(|row| row["count"].as_u64().unwrap_or(0) >= 2));

    let demo = real_tree_file(CS_SCRIPT_DEMO).display().to_string();
    let out = call(&server, "list_props", json!({"path": demo}))
        .await
        .unwrap();
    assert_eq!(out["total"], json!(17), "{}", out["total"]);
    expect_by_kind(&out, 7, 10, 0, 0, 0);
    // The model filter pins the case-insensitive substring rule on real models.
    let out = call(
        &server,
        "list_props",
        json!({"path": demo, "model": ".VMDL"}),
    )
    .await
    .unwrap();
    assert_eq!(
        out["total"],
        json!(17),
        "every real prop's model ends in .vmdl"
    );
}
