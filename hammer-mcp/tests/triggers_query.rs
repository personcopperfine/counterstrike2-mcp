//! `list_triggers` tool tests (WP-L).
//!
//! File route: a fixture `.vmap` is GENERATED in the test from the committed `probe.vmap`
//! fixture by same-length byte patches (the props_query pattern; binary-DMX strings are
//! length-prefixed, so a same-length replace is layout-neutral):
//!
//! * `func_buyzone` -> `trigger_push` (4 occurrences, both brush entities) — the "kind trigger
//!   finds nothing unless the map has one" fixture becomes a two-trigger map;
//! * `func_bomb_target` -> `unknown_bombzone` (3 occurrences) — an entity whose class the FGD
//!   does not carry, confirmed as a brush entity only by its own mesh children;
//! * the string-table entry `parentname` -> `spawnflags` (1 occurrence: attribute names live in
//!   the DMX string table, so ONE byte patch renames the keyvalue on every entity that carries
//!   it) — the empty `""` text decodes to value 0 with all declared flags off.
//!
//! A hand-written fixture FGD (written into the temp game tree) carries the trigger base
//! classes and a `@SolidClass` for every other probe classname, so `kind:"solid"` stays
//! assertion-friendly. The committed fixtures' goldens (`probe.entities.json` etc.) are read,
//! never written.
//!
//! Live route: the mock bridge (tests/props_query.rs pattern) with hand-built Hammer worlds.
//! The mock carries the m2.19 `entity.get.children` (WP-T), so the plain mock models the
//! children-present path (an entity with no tied meshes answers `children: []`); the
//! children-ABSENT path (an m2.18 bridge) is modelled with a mock `responder` on `entity.get`,
//! and the volumes come from the live `mesh.get` read or the SAVED file at the mock document's
//! path, exactly the two sources the tool may use.
//!
//! The `#[ignore]`d corpus test runs `list_triggers` against the REAL game tree (real FGD +
//! the installed `template_defuse.vmap`) and fails loudly when it is missing.
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

/// The FGD fixture: the trigger base classes (`Trigger` -> `TriggerOnce`, each with its own
/// `spawnflags(flags)` def), the trigger classes, the Solid-but-not-trigger brush classes and
/// every other probe.vmap classname as a `@PointClass` (so `kind:"solid"` lists exactly the
/// brush entities and the deliberately-unknown ones).
const FIXTURE_FGD: &str = r#"// Triggers fixture FGD (WP-L tests). Shape mirrors the real game's core/base.fgd:
// Trigger/TriggerOnce base classes, children inherit through base().

@BaseClass = Targetname
[
	targetname(target_source) : "Name"
]

@BaseClass = Trigger
[
	spawnflags(flags) : "Trigger flags" : 0 =
	[
		1 : "Clients" : 0
		2 : "NPCs" : 0
		4 : "Pushables" : 0
	]
]

@BaseClass base(Trigger) = TriggerOnce
[
	spawnflags(flags) : "Once flags" : 0 =
	[
		1 : "Clients" : 0
		4096 : "Initial State Off" : 0
	]
]

@SolidClass base(Targetname, Trigger) = trigger_multiple : "Fires while touched."
@SolidClass base(Targetname, Trigger) = trigger_push : "Pushes what touches it."
@SolidClass base(Targetname, TriggerOnce) = trigger_once : "Fires once."

// Solid but NOT a trigger: kind "solid" lists it, kind "trigger" (default) does not.
@SolidClass base(Targetname) = func_buyzone : "Buy zone."
[
	spawnflags(flags) : "Buyzone flags" : 0 =
	[
		1 : "Clients" : 0
	]
	TeamNum(integer) : "Team" : 0
]
@SolidClass base(Targetname) = func_bomb_target : "Bomb target."
[
	spawnflags(flags) : "Bomb target flags" : 0 =
	[
		1 : "Clients" : 0
		2 : "NPCs" : 0
	]
]
@SolidClass base(Targetname) = post_processing_volume : "Post processing."
@SolidClass base(Targetname) = end_of_match : "End of match (a Solid class without brushes)."

// The point classes of probe.vmap: never brush entities on either kind.
@PointClass base(Targetname) = info_target : "Target."
@PointClass base(Targetname) = point_worldtext : "Text."
@PointClass base(Targetname) = info_player_terrorist : "T spawn."
@PointClass base(Targetname) = info_player_counterterrorist : "CT spawn."
@PointClass base(Targetname) = light_environment : "Light."
@PointClass base(Targetname) = env_sky : "Sky."
@PointClass base(Targetname) = env_cubemap_fog : "Cubemap fog."
@PointClass base(Targetname) = info_map_parameters : "Map parameters."
@PointClass base(Targetname) = counterterrorist_team_intro : "CT intro."
@PointClass base(Targetname) = terrorist_team_intro : "T intro."
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
            "triggers-{tag}-{}-{}",
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

    /// Write probe.vmap (optionally byte-patched) into the addon and return its addon-relative
    /// path. Same-length replacement is layout-neutral for binary DMX (length-prefixed
    /// strings); the occurrence counts are pinned to the committed fixture so an unexpected
    /// fixture change fails loudly.
    fn install_map(&self, name: &str, patches: &[(&[u8], &[u8], usize)]) -> String {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vmap/probe.vmap");
        let mut data = std::fs::read(&source).expect("committed probe.vmap fixture");
        for (from, to, expected) in patches {
            assert_eq!(from.len(), to.len(), "the patch must be same-length");
            let at: Vec<usize> = data
                .windows(from.len())
                .enumerate()
                .filter(|(_, window)| *window == *from)
                .map(|(index, _)| index)
                .collect();
            assert_eq!(
                at.len(),
                *expected,
                "the {} occurrence count changed (fixture update?): the patched map is not \
                 what these tests describe",
                std::str::from_utf8(from).expect("ascii")
            );
            for index in at {
                data[index..index + from.len()].copy_from_slice(to);
            }
        }
        let target = self.addon().join("maps").join(name);
        std::fs::create_dir_all(target.parent().expect("maps dir")).expect("maps dir");
        std::fs::write(&target, &data).expect("write patched vmap");
        format!("maps/{name}")
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

fn trigger_ids(out: &Value) -> Vec<i64> {
    out["triggers"]
        .as_array()
        .expect("triggers array")
        .iter()
        .map(|trigger| trigger["nodeID"].as_i64().expect("nodeID"))
        .collect()
}

/// The probe fixture's brush entities in element order: 33, 34 (func_bomb_target), 70, 67
/// (func_buyzone), 74 (post_processing_volume). The file route keeps element order, so the
/// expected id order is read from the document itself (an order change in the fixture fails
/// loudly instead of silently matching a wrong expectation).
fn document_brush_order(path: &Path, classes: &[&str]) -> Vec<i64> {
    hammer_mcp::vmap::load(path)
        .expect("probe.vmap loads")
        .entities()
        .iter()
        .filter(|entity| classes.contains(&entity.classname.as_str()))
        .filter_map(|entity| entity.node_id)
        .collect()
}

// -------------------------------------------------------------------------------------------
// File route: acceptance
// -------------------------------------------------------------------------------------------

const PROBE_ARG: &str = "maps/trig_probe.vmap";

#[tokio::test]
async fn file_kind_solid_lists_the_probe_brush_entities_with_meshes_and_volumes() {
    let tree = Tree::new("file-solid");
    tree.install_fgd();
    let path = tree.install_map("trig_probe.vmap", &[]);
    let expected = document_brush_order(
        &tree.addon().join(&path),
        &[
            "func_bomb_target",
            "func_buyzone",
            "post_processing_volume",
            "end_of_match",
        ],
    );
    assert_eq!(
        expected,
        vec![33, 34, 70, 67, 74, 78],
        "the probe brushes in element order (oracle-verified)"
    );

    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "kind": "solid"}),
    )
    .await
    .unwrap();
    assert_eq!(out["source"], json!("file"));
    assert_eq!(out["path"], json!(PROBE_ARG));
    assert_eq!(out["total"], json!(6), "{out}");
    assert_eq!(out["returned"], json!(6));
    assert_eq!(out["offset"], json!(0));
    assert_eq!(out["truncated"], json!(false));
    assert_eq!(trigger_ids(&out), expected);
    assert_eq!(
        out["summary"]["byClass"],
        json!({
            "end_of_match": 1,
            "func_bomb_target": 2,
            "func_buyzone": 2,
            "post_processing_volume": 1,
        })
    );

    let by_id = |id: i64| -> Value {
        out["triggers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|trigger| trigger["nodeID"] == json!(id))
            .expect("trigger row")
            .clone()
    };
    // The two func_buyzone with meshes 66/71 and the mesh bounds as the volume (WP fact).
    let buyzone = by_id(67);
    assert_eq!(buyzone["classname"], json!("func_buyzone"));
    assert_eq!(buyzone["meshes"], json!([66]));
    assert_eq!(
        buyzone["volume"],
        json!({
            "min": [-128.0, -384.0, 0.0],
            "max": [320.0, -256.0, 64.0],
            "size": [448.0, 128.0, 64.0],
            "center": [96.0, -320.0, 32.0],
            "geometrySource": Value::Null,
        })
    );
    assert_eq!(buyzone["origin"], json!([96.0, -320.0, 32.0]));
    assert_eq!(buyzone["problems"], json!([]));
    assert_eq!(by_id(70)["meshes"], json!([71]));
    assert_eq!(
        by_id(70)["volume"]["min"],
        json!([720.0, -384.0, 0.0]),
        "volume = the mesh bounds"
    );
    assert_eq!(by_id(33)["meshes"], json!([31]));
    assert_eq!(by_id(34)["meshes"], json!([32]));
    assert_eq!(by_id(74)["meshes"], json!([75]));

    // end_of_match is a Solid class whose entity has no mesh children: named, not dropped.
    let childless = by_id(78);
    assert_eq!(childless["meshes"], json!([]));
    assert_eq!(childless["volume"], Value::Null);
    assert_eq!(childless["problems"], json!(["no mesh children"]));

    // The envelope: the entities-file facts (the mock-less file route is not stale).
    assert_eq!(out["file"]["fromDisk"], json!(true));
    assert_eq!(
        out["file"]["hostPath"],
        json!(tree.addon().join(&path).display().to_string())
    );
    assert_eq!(out["file"]["entityCount"], json!(69));
    assert_eq!(out["staleRead"], json!(false));
    assert_eq!(out["staleReason"], Value::Null);
    assert_eq!(out["unsavedBridgeEdits"], json!(0));
    assert!(out["notes"].as_array().expect("notes").is_empty());

    // includeConnections:false omits the field; the default carries the (empty) array.
    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "kind": "solid", "includeConnections": false, "limit": 1}),
    )
    .await
    .unwrap();
    assert!(out["triggers"][0].get("connections").is_none(), "{out}");
    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "kind": "solid", "limit": 1}),
    )
    .await
    .unwrap();
    assert_eq!(out["triggers"][0]["connections"], json!([]));
}

/// The default kind is `trigger`: the unpatched probe map has no trigger-class entity, and the
/// tool answers an honest empty page — never a silent something.
#[tokio::test]
async fn file_default_kind_trigger_finds_nothing_on_the_unpatched_probe() {
    let tree = Tree::new("file-empty");
    tree.install_fgd();
    tree.install_map("trig_probe.vmap", &[]);
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    for args in [
        json!({"path": PROBE_ARG}),
        json!({"path": PROBE_ARG, "kind": "trigger"}),
    ] {
        let out = call(&server, "list_triggers", args).await.unwrap();
        assert_eq!(out["total"], json!(0), "{out}");
        assert_eq!(out["triggers"], json!([]));
        assert_eq!(out["summary"]["byClass"], json!({}));
        assert_eq!(out["truncated"], json!(false));
    }
}

/// The patched map (`func_buyzone` -> `trigger_push`, same length): kind trigger now lists the
/// two generated triggers with their mesh volumes, in element order.
#[tokio::test]
async fn file_patched_map_lists_the_generated_triggers() {
    let tree = Tree::new("file-triggers");
    tree.install_fgd();
    tree.install_map(
        "trig_probe.vmap",
        &[(b"func_buyzone", b"trigger_push", 4usize)],
    );
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    let out = call(&server, "list_triggers", json!({"path": PROBE_ARG}))
        .await
        .unwrap();
    assert_eq!(out["total"], json!(2), "{out}");
    assert_eq!(trigger_ids(&out), vec![70, 67], "element order (70 first)");
    assert_eq!(out["summary"]["byClass"], json!({"trigger_push": 2}));

    let trigger = &out["triggers"][0];
    assert_eq!(trigger["classname"], json!("trigger_push"));
    assert_eq!(trigger["meshes"], json!([71]));
    assert_eq!(trigger["volume"]["max"], json!([1168.0, -256.0, 64.0]));
    assert_eq!(trigger["problems"], json!([]));
    // No spawnflags keyvalue on the entity: value/raw null and NO invented flag list.
    assert_eq!(
        trigger["spawnflags"],
        json!({"value": Value::Null, "raw": Value::Null, "flags": [], "unknownBits": 0})
    );
    assert!(trigger["keyvalues"].is_null(), "default: no keyvalues");

    // includeKeyValues adds the entity's keyvalues (DMX strings, incl. the patched classname).
    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "includeKeyValues": true, "limit": 1}),
    )
    .await
    .unwrap();
    assert_eq!(
        out["triggers"][0]["keyvalues"]["classname"],
        json!("trigger_push")
    );
}

#[tokio::test]
async fn file_class_targetname_within_and_paging() {
    let tree = Tree::new("file-filters");
    tree.install_fgd();
    tree.install_map(
        "trig_probe.vmap",
        &[(b"func_buyzone", b"trigger_push", 4usize)],
    );
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    // class: exact, case-insensitive.
    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "kind": "solid", "class": "TRIGGER_PUSH"}),
    )
    .await
    .unwrap();
    assert_eq!(trigger_ids(&out), vec![70, 67]);

    // targetname: exact, case-insensitive (the probe brushes are unnamed: an empty page).
    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "kind": "solid", "targetname": "NOPE"}),
    )
    .await
    .unwrap();
    assert_eq!(out["total"], json!(0));

    // within: the trigger must lie INSIDE the box by its VOLUME bounds, not its origin. The
    // trigger at nodeID 67 has origin [96,-320,32] (inside the first box) but the volume
    // [-128..320]x[-384..-256]x[0..64] pokes out over x=100 — origin-inside is not enough.
    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "within": {"min": [-200.0, -450.0, -10.0], "max": [100.0, -200.0, 70.0]}}),
    )
    .await
    .unwrap();
    assert_eq!(out["total"], json!(0), "{out}");
    // A box that contains the whole volume matches (only trigger 67; 70 starts at x=720).
    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "within": {"min": [-140.0, -390.0, -10.0], "max": [330.0, -250.0, 70.0]}}),
    )
    .await
    .unwrap();
    assert_eq!(trigger_ids(&out), vec![67]);

    // withinMode (review C5): "intersects" accepts the trigger STRADDLING the box edge that
    // "contains" (the default, above) excludes; trigger 67's volume pokes out over x=100.
    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "within": {"min": [-200.0, -450.0, -10.0], "max": [100.0, -200.0, 70.0]},
               "withinMode": "intersects"}),
    )
    .await
    .unwrap();
    assert_eq!(trigger_ids(&out), vec![67], "{out}");
    // explicit "contains" is the same rule as the default
    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "within": {"min": [-200.0, -450.0, -10.0], "max": [100.0, -200.0, 70.0]},
               "withinMode": "contains"}),
    )
    .await
    .unwrap();
    assert_eq!(out["total"], json!(0), "{out}");

    // Paging: limit truncates with total untouched; offset slices, truncated stays truthful.
    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "kind": "solid", "limit": 4}),
    )
    .await
    .unwrap();
    assert_eq!(out["total"], json!(6));
    assert_eq!(out["returned"], json!(4));
    assert_eq!(out["truncated"], json!(true));
    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "kind": "solid", "limit": 4, "offset": 4}),
    )
    .await
    .unwrap();
    assert_eq!(out["returned"], json!(2));
    assert_eq!(out["truncated"], json!(false));
    assert_eq!(trigger_ids(&out), vec![74, 78]);
    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "kind": "solid", "limit": 4, "offset": 10}),
    )
    .await
    .unwrap();
    assert_eq!(out["returned"], json!(0));
    assert_eq!(out["truncated"], json!(false));
    assert_eq!(out["triggers"], json!([]));
}

/// An unknown class is confirmed only by the entity's own mesh children: listed under kind
/// solid with `problems:["unknown class"]` (the volumes still compute), dropped from kind
/// trigger — a stale/typoed class never vanishes silently but never becomes a trigger either.
#[tokio::test]
async fn file_unknown_class_lists_only_under_kind_solid() {
    let tree = Tree::new("file-unknown");
    tree.install_fgd();
    tree.install_map(
        "trig_probe.vmap",
        &[(b"func_bomb_target", b"unknown_bombzone", 3usize)],
    );
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    let out = call(&server, "list_triggers", json!({"path": PROBE_ARG}))
        .await
        .unwrap();
    assert_eq!(out["total"], json!(0), "{out}");

    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "kind": "solid", "class": "unknown_bombzone"}),
    )
    .await
    .unwrap();
    assert_eq!(trigger_ids(&out), vec![33, 34], "{out}");
    for trigger in out["triggers"].as_array().unwrap() {
        assert_eq!(trigger["problems"], json!(["unknown class"]), "{trigger}");
        assert!(trigger["meshes"].as_array().unwrap().len() == 1);
        assert!(trigger["volume"].is_object());
    }
}

/// An empty `spawnflags` keyvalue text decodes to value 0 with every declared flag off (the
/// WP-F `""` -> 0 rule): the string-table rename `parentname` -> `spawnflags` gives the
/// func_bomb_targets such a keyvalue without touching the layout.
#[tokio::test]
async fn file_spawnflags_empty_text_decodes_to_zero() {
    let tree = Tree::new("file-spawnflags");
    tree.install_fgd();
    tree.install_map("trig_probe.vmap", &[(b"parentname", b"spawnflags", 1usize)]);
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "kind": "solid", "class": "func_bomb_target"}),
    )
    .await
    .unwrap();
    assert_eq!(trigger_ids(&out), vec![33, 34], "{out}");
    for trigger in out["triggers"].as_array().unwrap() {
        assert_eq!(trigger["problems"], json!([]), "{trigger}");
        assert_eq!(
            trigger["spawnflags"],
            json!({
                "value": 0,
                "raw": "",
                "flags": [
                    {"bit": 1, "label": "Clients", "on": false},
                    {"bit": 2, "label": "NPCs", "on": false},
                ],
                "unknownBits": 0,
            }),
            "{trigger}"
        );
    }
}

/// The file route's I/O connections come from the entity's own `connectionsData` (review C1 —
/// before the fix the field was always `[]` on the file route, while the live route named the
/// same connection: target/a3.log nodes 120/122). `probe_conn.vmap` is the committed
/// `probe.vmap` plus one synthesized `DmeConnectionData` on the tested buyzone (generated by
/// `fixtures/vmap/patch_connections.py`, byte-reproducible): nodeID 67 lists the connection
/// with the real writer's attribute shape, every connectionless entity stays `[]`, and
/// `includeConnections:false` omits the field.
#[tokio::test]
async fn file_connections_come_from_connections_data() {
    let tree = Tree::new("file-connections");
    tree.install_fgd();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vmap/probe_conn.vmap");
    std::fs::create_dir_all(tree.addon().join("maps")).expect("maps dir");
    std::fs::copy(&source, tree.addon().join("maps/trig_probe.vmap")).expect("fixture copy");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();

    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "kind": "solid", "class": "func_buyzone"}),
    )
    .await
    .unwrap();
    assert_eq!(trigger_ids(&out), vec![70, 67], "{out}");
    let by_id = |id: i64| -> Value {
        out["triggers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|trigger| trigger["nodeID"] == json!(id))
            .expect("trigger row")
            .clone()
    };
    assert_eq!(
        by_id(67)["connections"],
        json!([{
            "outputName": "OnStartTouch",
            "targetType": 7,
            "targetName": "ta_kill_t",
            "inputName": "Kill",
            "overrideParam": "",
            "delay": 0.0,
            "timesToFire": -1,
        }]),
        "{}",
        by_id(67)
    );
    assert_eq!(by_id(70)["connections"], json!([]));

    let out = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "kind": "solid", "class": "func_buyzone",
               "includeConnections": false}),
    )
    .await
    .unwrap();
    assert!(
        out["triggers"][0].get("connections").is_none(),
        "{}",
        out["triggers"][0]
    );
}

// -------------------------------------------------------------------------------------------
// File route: required negatives
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn bad_arguments_are_invalid_arguments_before_any_work() {
    let tree = Tree::new("bad-args");
    tree.install_fgd();
    tree.install_map("trig_probe.vmap", &[]);
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    for bad in [
        json!({"path": PROBE_ARG, "limit": 0}),
        json!({"path": PROBE_ARG, "limit": 2001}),
        json!({"path": PROBE_ARG, "limit": "5"}),
        json!({"path": PROBE_ARG, "offset": -1}),
        json!({"path": PROBE_ARG, "offset": 1.5}),
        json!({"path": PROBE_ARG, "kind": "any"}),
        json!({"path": PROBE_ARG, "kind": 3}),
        json!({"path": PROBE_ARG, "source": "disk"}),
        json!({"path": PROBE_ARG, "source": 3}),
        json!({"path": PROBE_ARG, "includeKeyValues": "yes"}),
        json!({"path": PROBE_ARG, "includeConnections": 1}),
        // an empty filter matches everything while pretending to filter
        json!({"path": PROBE_ARG, "class": ""}),
        json!({"path": PROBE_ARG, "targetname": ""}),
        // box shape
        json!({"path": PROBE_ARG, "within": {"min": [1.0, 2.0, 3.0]}}),
        json!({"path": PROBE_ARG, "within": {"max": [1.0, 2.0, 3.0]}}),
        json!({"path": PROBE_ARG, "within": {"min": [1.0, 2.0], "max": [1.0, 2.0, 3.0]}}),
        json!({"path": PROBE_ARG, "within": {"min": [1.0, 2.0, "x"], "max": [1.0, 2.0, 3.0]}}),
        // an unknown key INSIDE within
        json!({"path": PROBE_ARG, "within": {"min": [1.0, 2.0, 3.0], "max": [1.0, 2.0, 3.0], "pad": 1}}),
        // min > max on one axis
        json!({"path": PROBE_ARG, "within": {"min": [500.0, 100.0, -1.0], "max": [100.0, 500.0, 1.0]}}),
        // withinMode
        json!({"path": PROBE_ARG, "withinMode": "overlap"}),
        json!({"path": PROBE_ARG, "withinMode": 3}),
        // unknown argument (top level)
        json!({"path": PROBE_ARG, "modelContains": "box"}),
        // contradictory route
        json!({"source": "live", "path": PROBE_ARG}),
        // a non-string path is refused in the argument phase, not silently treated as
        // "no path" (which would reroute the call to live / to the active document)
        json!({"path": 123}),
        json!({"source": "live", "path": 123}),
    ] {
        let err = call(&server, "list_triggers", bad.clone())
            .await
            .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}: {}", err.type_);
        assert_eq!(err.name, "InvalidParams", "{bad}");
        assert!(err.retry_safe, "{bad}");
    }
}

/// Arguments are validated before the FGD (or any file) is touched: a tree without a game root
/// answers the argument problem, not FgdUnavailable — and the same call with valid args then
/// answers FgdUnavailable, never an empty success.
#[tokio::test]
async fn bad_arguments_precede_the_fgd_load() {
    let tree = Tree::new("bad-args-first");
    let server = hammer_mcp::build_server(&config(&tree)).unwrap();
    let err = call(
        &server,
        "list_triggers",
        json!({"path": PROBE_ARG, "kind": "bogus"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments", "{}", err.type_);

    let err = call(&server, "list_triggers", json!({"path": PROBE_ARG}))
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
    let err = call(&server, "list_triggers", json!({"path": "maps/nope.vmap"}))
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
    let err = call(&server, "list_triggers", json!({"source": "file"}))
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
    let err = call(&server, "list_triggers", json!({})).await.unwrap_err();
    assert_eq!(err.name, "BridgeNotRunning", "{}", err.name);
    assert_eq!(err.type_, "BridgeNotRunning");
}

/// The file route's staleRead follows the same unsaved counter as the entity file tools
/// (handler called directly: the registry holds the server's private state).
#[tokio::test]
async fn file_stale_read_follows_the_unsaved_counter() {
    let tree = Tree::new("file-stale");
    tree.install_fgd();
    tree.install_map("trig_probe.vmap", &[]);
    let cfg = config(&tree);
    let state = HammerState::from_config(&cfg).unwrap();
    let args = json!({"path": PROBE_ARG, "kind": "solid"});

    let out = hammer_mcp::tools::triggers_query::list_triggers(state.clone(), ctx(), args.clone())
        .await
        .unwrap();
    assert_eq!(out.json["staleRead"], json!(false));
    assert_eq!(out.json["staleReason"], Value::Null);

    {
        let mut edits = state.edits.lock().unwrap();
        edits.since_save += 1;
    }
    let out = hammer_mcp::tools::triggers_query::list_triggers(state, ctx(), args)
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
// Live route (mock bridge)
// -------------------------------------------------------------------------------------------

/// The mock Hammer world: worldspawn + trigger entities (with spawnflags texts covering the
/// decode paths), a Solid-but-not-trigger class, a point class and an unknown class. The FGD
/// is the fixture above.
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
                "trigger_multiple",
                [10.0, 0.0, 0.0],
                kv(&[("targetname", "trig"), ("spawnflags", "5")]),
            ),
            MockNode::entity(
                6,
                "trigger_multiple",
                [20.0, 0.0, 0.0],
                kv(&[("spawnflags", "x")]),
            ),
            MockNode::entity(7, "func_buyzone", [30.0, 0.0, 0.0], kv(&[])),
            MockNode::entity(8, "info_target", [40.0, 0.0, 0.0], kv(&[])),
            MockNode::entity(9, "gone_trigger", [50.0, 0.0, 0.0], kv(&[])),
            MockNode::entity(
                10,
                "trigger_multiple",
                [60.0, 0.0, 0.0],
                kv(&[("spawnflags", "9")]),
            ),
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
async fn live_lists_triggers_and_decodes_spawnflags() {
    let (_tree, server, mb) = live_server("live-list").await;

    let out = call(&server, "list_triggers", json!({})).await.unwrap();
    assert_eq!(out["source"], json!("live"), "{out}");
    assert_eq!(
        out["total"],
        json!(3),
        "the trigger-class entities only: {out}"
    );
    assert_eq!(trigger_ids(&out), vec![5, 6, 10]);
    assert_eq!(out["summary"]["byClass"], json!({"trigger_multiple": 3}));

    // entity.find was scanned at full width, one entity.get per candidate.
    let find = mb.method_calls("entity.find");
    assert_eq!(find.len(), 1);
    assert_eq!(find[0].params["limit"], json!(2000));
    assert!(find[0].params.get("class").is_none());
    assert_eq!(mb.method_calls("entity.get").len(), 3);

    // The decoded spawnflags (value 5 = bits 1+4 of the Trigger def).
    let first = &out["triggers"][0];
    assert_eq!(first["classname"], json!("trigger_multiple"));
    assert_eq!(first["targetname"], json!("trig"));
    assert_eq!(first["origin"], json!([10.0, 0.0, 0.0]));
    assert_eq!(first["angles"], json!([0.0, 0.0, 0.0]), "from entity.get");
    assert_eq!(
        first["spawnflags"],
        json!({
            "value": 5,
            "raw": "5",
            "flags": [
                {"bit": 1, "label": "Clients", "on": true},
                {"bit": 2, "label": "NPCs", "on": false},
                {"bit": 4, "label": "Pushables", "on": true},
            ],
            "unknownBits": 0,
        }),
        "{first}"
    );
    assert_eq!(
        first["connections"],
        json!([]),
        "no live I/O on this entity"
    );

    // The unparseable text is a problems entry, not a crash; the raw text is kept. (The m2.19
    // children also say "no mesh children" for every childless mock entity — the loop below.)
    let broken = &out["triggers"][1];
    assert_eq!(broken["nodeID"], json!(6));
    assert_eq!(broken["spawnflags"]["value"], Value::Null);
    assert_eq!(broken["spawnflags"]["raw"], json!("x"));
    assert_eq!(
        broken["problems"],
        json!(["no mesh children", "spawnflags not an integer"])
    );

    // Bits no declared flag covers are named.
    let unknown = &out["triggers"][2];
    assert_eq!(unknown["nodeID"], json!(10));
    assert_eq!(unknown["spawnflags"]["value"], json!(9));
    assert_eq!(unknown["spawnflags"]["unknownBits"], json!(8));

    // The mock carries the m2.19 `children`: these world entities have no tied meshes, so
    // the route now KNOWS they are childless — meshes `[]` (never an invented null), the
    // "no mesh children" problem, and no volume.
    for trigger in out["triggers"].as_array().unwrap() {
        assert_eq!(trigger["meshes"], json!([]), "{trigger}");
        assert_eq!(trigger["volume"], Value::Null, "{trigger}");
        assert!(
            trigger["problems"]
                .as_array()
                .expect("problems")
                .iter()
                .any(|p| p.as_str() == Some("no mesh children")),
            "{trigger}"
        );
    }
    // The children-absent note belongs to the m2.18 test, not this one.
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        !notes
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("children")),
        "no stale children note: {notes:?}"
    );
}

/// kind solid lists the Solid classes too — and the childless unknown class stays out (with a
/// note), because without mesh children nothing confirms it as a brush entity.
#[tokio::test]
async fn live_kind_solid_and_the_childless_unknown() {
    let (_tree, server, _mb) = live_server("live-solid").await;

    let out = call(&server, "list_triggers", json!({"kind": "solid"}))
        .await
        .unwrap();
    assert_eq!(trigger_ids(&out), vec![5, 6, 7, 10], "{out}");
    assert_eq!(
        out["summary"]["byClass"],
        json!({"func_buyzone": 1, "trigger_multiple": 3})
    );
    // The buyzone is a Solid class whose mock entity has no tied meshes: the m2.19 children
    // say so, so meshes `[]` and the "no mesh children" problem — the file route's rule.
    let buyzone = out["triggers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|trigger| trigger["nodeID"] == json!(7))
        .expect("buyzone");
    assert_eq!(buyzone["meshes"], json!([]));
    assert_eq!(buyzone["problems"], json!(["no mesh children"]));

    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("stayed out")),
        "the dropped unknown class is named: {notes:?}"
    );
}

/// An m2.18 bridge (no `children` member in `entity.get` at all, modelled with a responder):
/// the route cannot say anything about the meshes — `meshes`/`volume` stay null (never
/// invented empty lists), a note says so, and NO "no mesh children" problem is invented
/// (review C1's second route; the plain mock above models the children-present side).
#[tokio::test]
async fn live_m2_18_bridge_without_children_keeps_meshes_null() {
    use cs2_mcp_common::mock::{MockBridge, MockNode};
    let tree = Tree::new("live-m218");
    let mb = MockBridge::builder()
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(5, "trigger_multiple", [10.0, 0.0, 0.0], Default::default()),
            MockNode::entity(6, "trigger_multiple", [20.0, 0.0, 0.0], Default::default()),
        ])
        .responder("entity.get", |params| {
            m2_18_entity_get(params["nodeID"].as_i64().unwrap_or(0))
        })
        .start()
        .await
        .unwrap();
    tree.install_fgd();
    let server = hammer_mcp::build_server(&config_with_bridge(&tree, &mb.discovery_path)).unwrap();

    let out = call(&server, "list_triggers", json!({})).await.unwrap();
    assert_eq!(out["total"], json!(2), "{out}");
    for trigger in out["triggers"].as_array().unwrap() {
        assert_eq!(trigger["meshes"], Value::Null, "{trigger}");
        assert_eq!(trigger["volume"], Value::Null, "{trigger}");
        assert_eq!(trigger["problems"], json!([]), "{trigger}");
        assert_eq!(trigger["connections"], json!([]), "{trigger}");
    }
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("children")),
        "the note names the missing children: {notes:?}"
    );
}

#[tokio::test]
async fn live_filters_and_paging() {
    let (_tree, server, mb) = live_server("live-filters").await;

    // class: forwarded to the bridge's own exact case-insensitive filter.
    let out = call(
        &server,
        "list_triggers",
        json!({"class": "trigger_MULTIPLE"}),
    )
    .await
    .unwrap();
    assert_eq!(trigger_ids(&out), vec![5, 6, 10]);
    let find = mb.method_calls("entity.find");
    assert_eq!(
        find.last().expect("find").params["class"],
        json!("trigger_MULTIPLE")
    );

    // targetname: forwarded too.
    let out = call(&server, "list_triggers", json!({"targetname": "TRIG"}))
        .await
        .unwrap();
    assert_eq!(trigger_ids(&out), vec![5]);

    // within: computed locally on the origin (no volume on this bridge).
    let out = call(
        &server,
        "list_triggers",
        json!({"within": {"min": [-5.0, -5.0, -5.0], "max": [15.0, 15.0, 15.0]}}),
    )
    .await
    .unwrap();
    assert_eq!(trigger_ids(&out), vec![5]);
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("ORIGIN only")),
        "the origin-only matching is said once: {notes:?}"
    );

    // paging.
    let out = call(&server, "list_triggers", json!({"limit": 1}))
        .await
        .unwrap();
    assert_eq!(out["total"], json!(3));
    assert_eq!(out["returned"], json!(1));
    assert_eq!(out["truncated"], json!(true));
    assert_eq!(trigger_ids(&out), vec![5]);
    let out = call(&server, "list_triggers", json!({"limit": 2, "offset": 1}))
        .await
        .unwrap();
    assert_eq!(out["total"], json!(3));
    assert_eq!(out["returned"], json!(2));
    assert_eq!(out["truncated"], json!(false), "the page covers the rest");
    assert_eq!(trigger_ids(&out), vec![6, 10]);
    let out = call(&server, "list_triggers", json!({"limit": 2, "offset": 2}))
        .await
        .unwrap();
    assert_eq!(out["returned"], json!(1));
    assert_eq!(out["truncated"], json!(false));
    assert_eq!(trigger_ids(&out), vec![10]);
}

#[tokio::test]
async fn bad_arguments_live_are_refused_before_anything_is_sent() {
    let (_tree, server, mb) = live_server("live-bad-args").await;
    for bad in [
        json!({"kind": "any"}),
        json!({"limit": 0}),
        json!({"unknown": 1}),
        json!({"within": {"min": [2.0, 2.0, 2.0], "max": [1.0, 1.0, 1.0]}}),
        json!({"class": ""}),
    ] {
        let err = call(&server, "list_triggers", bad.clone())
            .await
            .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
    }
    assert!(mb.method_calls("entity.find").is_empty());
    assert!(mb.method_calls("entity.get").is_empty());
}

#[tokio::test]
async fn live_entity_find_error_is_passed_through() {
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

    let err = call(&server, "list_triggers", json!({})).await.unwrap_err();
    assert_eq!(err.code, Some(-32004), "{}", err.name);
    assert_eq!(err.name, "DocumentChanged");
    assert!(
        err.retry_safe,
        "nothing was executed (expectDoc refused it)"
    );
}

/// A failed per-trigger read does NOT invent fields: the trigger stays a trigger with a
/// problem entry naming the failure, and meshes/volume stay null.
#[tokio::test]
async fn live_entity_get_failure_is_a_problem_not_a_silent_empty() {
    use cs2_mcp_common::mock::{MockBridge, MockNode};
    let tree = Tree::new("live-get-error");
    let mb = MockBridge::builder()
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(5, "trigger_multiple", [10.0, 0.0, 0.0], Default::default()),
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

    let out = call(&server, "list_triggers", json!({})).await.unwrap();
    assert_eq!(out["total"], json!(1), "the trigger is still a trigger");
    let trigger = &out["triggers"][0];
    assert_eq!(trigger["nodeID"], json!(5));
    assert_eq!(trigger["meshes"], Value::Null, "never invented");
    assert_eq!(trigger["volume"], Value::Null);
    let problems = trigger["problems"].as_array().expect("problems");
    assert!(
        problems
            .iter()
            .any(|p| p.as_str().unwrap_or_default().contains("live read failed")),
        "{problems:?}"
    );
}

// -------------------------------------------------------------------------------------------
// Live route with `entity.get.children` (bridge m2.19 shape, modelled with a responder)
// -------------------------------------------------------------------------------------------

/// A full m2.19-shaped `entity.get` reply for one node: `children` rows plus the
/// `childrenTruncated` flag (review C4 keeps the flag on the record) and the entity's live
/// I/O `connections` array (review C1 keeps the live echo as the bridge reports it).
fn m219_entity_get(id: i64, children: Value, connections: Value) -> Value {
    json!({
        "nodeID": id,
        "classname": "trigger_multiple",
        "targetname": Value::Null,
        "origin": [if id == 5 { 96.0 } else { 944.0 }, -320.0, 32.0],
        "angles": [0.0, 0.0, 0.0],
        "scales": [1.0, 1.0, 1.0],
        "keyValues": {},
        "keyValueCount": 0,
        "unsupported": [],
        "connections": connections,
        "children": children,
        "childrenTruncated": false,
        "source": "live",
    })
}

/// The m2.18 shape (review C1's second route): the same reply WITHOUT the `children` /
/// `childrenTruncated` members — what a pre-m2.19 bridge answers.
fn m2_18_entity_get(id: i64) -> Value {
    json!({
        "nodeID": id,
        "classname": "trigger_multiple",
        "targetname": Value::Null,
        "origin": [10.0, 0.0, 0.0],
        "angles": [0.0, 0.0, 0.0],
        "scales": [1.0, 1.0, 1.0],
        "keyValues": {},
        "keyValueCount": 0,
        "unsupported": [],
        "connections": [],
        "source": "live",
    })
}

/// Children present -> the mesh nodeIDs are listed and the volume comes from the SAVED file at
/// the mock document's path (`geometrySource:"saved-file"`). The file is the committed
/// probe.vmap (its mesh 66 carries the buyzone bounds); a mesh the file does not carry gets no
/// volume — with a note, never a silent null.
#[tokio::test]
async fn live_children_present_volume_from_the_saved_file() {
    use cs2_mcp_common::mock::{MockBridge, MockNode};
    let tree = Tree::new("live-children");
    // The mock document's default path is z:/app/content/csgo_addons/mcp_test/maps/probe.vmap;
    // the path map sends z:\app to the temp app dir, so the saved file lives there.
    let mb = MockBridge::builder()
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(
                5,
                "trigger_multiple",
                [96.0, -320.0, 32.0],
                Default::default(),
            ),
            MockNode::entity(
                6,
                "trigger_multiple",
                [944.0, -320.0, 32.0],
                Default::default(),
            ),
        ])
        .responder("entity.get", |params| {
            let id = params["nodeID"].as_i64().unwrap_or(0);
            let children = match id {
                5 => json!([{"nodeID": 66, "class": "mesh"}]),
                6 => json!([{"nodeID": 999, "class": "mesh"}]),
                _ => json!([]),
            };
            // the live I/O echo the bridge reports (review C1's live side): node 5 carries the
            // OnStartTouch row the a3.log wire shows for the same shape
            let connections = if id == 5 {
                json!([{
                    "outputName": "OnStartTouch", "targetType": 7,
                    "targetName": "ta_kill_t", "inputName": "Kill",
                    "overrideParam": "", "delay": 0.0, "timesToFire": -1,
                }])
            } else {
                json!([])
            };
            m219_entity_get(id, children, connections)
        })
        .start()
        .await
        .unwrap();
    tree.install_fgd();
    let probe = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vmap/probe.vmap");
    std::fs::create_dir_all(tree.addon().join("maps")).expect("maps dir");
    std::fs::copy(&probe, tree.addon().join("maps/probe.vmap")).expect("saved file");
    let server = hammer_mcp::build_server(&config_with_bridge(&tree, &mb.discovery_path)).unwrap();

    let out = call(&server, "list_triggers", json!({})).await.unwrap();
    assert_eq!(out["total"], json!(2), "{out}");
    let by_id = |id: i64| -> Value {
        out["triggers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|trigger| trigger["nodeID"] == json!(id))
            .expect("trigger row")
            .clone()
    };
    let first = by_id(5);
    assert_eq!(first["meshes"], json!([66]));
    assert_eq!(
        first["volume"],
        json!({
            "min": [-128.0, -384.0, 0.0],
            "max": [320.0, -256.0, 64.0],
            "size": [448.0, 128.0, 64.0],
            "center": [96.0, -320.0, 32.0],
            "geometrySource": "saved-file",
        }),
        "{first}"
    );
    assert_eq!(first["problems"], json!([]));
    // the live route keeps the reply's connections echo (review C1's live side)
    assert_eq!(
        first["connections"],
        json!([{
            "outputName": "OnStartTouch", "targetType": 7,
            "targetName": "ta_kill_t", "inputName": "Kill",
            "overrideParam": "", "delay": 0.0, "timesToFire": -1,
        }]),
        "{first}"
    );

    // Mesh 999 is in neither the live document nor the saved file: no volume, an explicit note.
    let second = by_id(6);
    assert_eq!(second["meshes"], json!([999]));
    assert_eq!(second["volume"], Value::Null);
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("999")),
        "the note names the unplaceable mesh: {notes:?}"
    );
}

/// A dirty document refuses the saved-file geometry (it is stale): the meshes stay listed, the
/// volumes go null with the note (the mesh-rebuild rule, mirrored).
#[tokio::test]
async fn live_dirty_document_refuses_the_saved_file_volume() {
    use cs2_mcp_common::mock::{MockBridge, MockNode};
    let tree = Tree::new("live-dirty");
    let mb = MockBridge::builder()
        .doc(Some(json!({
            "id": "d1",
            "path": "z:/app/content/csgo_addons/mcp_test/maps/probe.vmap",
            "dirty": true,
        })))
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(
                5,
                "trigger_multiple",
                [96.0, -320.0, 32.0],
                Default::default(),
            ),
        ])
        .responder("entity.get", |params| {
            let id = params["nodeID"].as_i64().unwrap_or(0);
            m219_entity_get(id, json!([{"nodeID": 66, "class": "mesh"}]), json!([]))
        })
        .start()
        .await
        .unwrap();
    tree.install_fgd();
    let server = hammer_mcp::build_server(&config_with_bridge(&tree, &mb.discovery_path)).unwrap();

    let out = call(&server, "list_triggers", json!({})).await.unwrap();
    let trigger = &out["triggers"][0];
    assert_eq!(trigger["meshes"], json!([66]), "{trigger}");
    assert_eq!(trigger["volume"], Value::Null);
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("unsaved edits")),
        "{notes:?}"
    );
}

/// The live `mesh.get` read is the FIRST volume source: a mesh created through `create_block` is
/// read back from the document and the volume carries `geometrySource:"live"` — no saved file
/// needed.
#[tokio::test]
async fn live_volume_from_the_live_mesh_read() {
    use cs2_mcp_common::mock::{MockBridge, MockNode};
    use std::sync::{Arc, Mutex};
    let tree = Tree::new("live-mesh-get");
    let created: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&created);
    let mb = MockBridge::builder()
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(5, "trigger_multiple", [0.0, 0.0, 0.0], Default::default()),
        ])
        .responder("entity.get", move |params| {
            let id = params["nodeID"].as_i64().unwrap_or(0);
            let children = match (id, *slot.lock().unwrap()) {
                (5, Some(mesh)) => json!([{"nodeID": mesh, "class": "mesh"}]),
                _ => json!([]),
            };
            m219_entity_get(id, children, json!([]))
        })
        .start()
        .await
        .unwrap();
    tree.install_fgd();
    let server = hammer_mcp::build_server(&config_with_bridge(&tree, &mb.discovery_path)).unwrap();

    // A mesh in the live document (the mock's `mesh.get` reads back what was written).
    let out = call(
        &server,
        "create_block",
        json!({"shape": "box", "min": [0.0, 0.0, 0.0], "max": [64.0, 64.0, 32.0]}),
    )
    .await
    .unwrap();
    let mesh_id = out["nodeID"].as_i64().expect("created mesh nodeID");
    *created.lock().unwrap() = Some(mesh_id);

    let out = call(&server, "list_triggers", json!({})).await.unwrap();
    let trigger = &out["triggers"][0];
    assert_eq!(trigger["meshes"], json!([mesh_id]), "{trigger}");
    assert_eq!(
        trigger["volume"],
        json!({
            "min": [0.0, 0.0, 0.0],
            "max": [64.0, 64.0, 32.0],
            "size": [64.0, 64.0, 32.0],
            "center": [32.0, 32.0, 16.0],
            "geometrySource": "live",
        }),
        "{trigger}"
    );
}

/// A document that does not REPORT `dirty` at all is not a clean one (review C3, the
/// `mesh_live`/`trigger_volume` rule): the saved-file volume source is refused — with a saved
/// file present that WOULD answer (the test fails without the fix: the volume would come back
/// as `geometrySource:"saved-file"`), the meshes stay listed, and the note names the
/// unreported state. The mock's hammer built-in always reports `dirty` (its world default),
/// so the unreported shape is modelled with a `doc.active` RESPONDER.
#[tokio::test]
async fn live_unreported_dirty_state_refuses_the_saved_file_volume() {
    use cs2_mcp_common::mock::{MockBridge, MockNode};
    let tree = Tree::new("live-dirty-unknown");
    // the mock document's default path; NO `dirty` key in the doc object
    let mb = MockBridge::builder()
        .doc(Some(json!({
            "id": "d1",
            "path": "z:/app/content/csgo_addons/mcp_test/maps/probe.vmap",
        })))
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(
                5,
                "trigger_multiple",
                [96.0, -320.0, 32.0],
                Default::default(),
            ),
        ])
        .responder("doc.active", |_| {
            json!({
                "doc": {
                    "id": "d1",
                    "path": "z:/app/content/csgo_addons/mcp_test/maps/probe.vmap",
                    "undoName": null,
                    "undoDepth": 0,
                    "topSerial": 0,
                }
            })
        })
        .responder("entity.get", |params| {
            let id = params["nodeID"].as_i64().unwrap_or(0);
            m219_entity_get(id, json!([{"nodeID": 66, "class": "mesh"}]), json!([]))
        })
        .start()
        .await
        .unwrap();
    tree.install_fgd();
    let probe = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vmap/probe.vmap");
    std::fs::create_dir_all(tree.addon().join("maps")).expect("maps dir");
    std::fs::copy(&probe, tree.addon().join("maps/probe.vmap")).expect("saved file");
    let server = hammer_mcp::build_server(&config_with_bridge(&tree, &mb.discovery_path)).unwrap();

    let out = call(&server, "list_triggers", json!({})).await.unwrap();
    let trigger = &out["triggers"][0];
    assert_eq!(trigger["meshes"], json!([66]), "{trigger}");
    assert_eq!(
        trigger["volume"],
        Value::Null,
        "an unreported dirty state is not clean: {trigger}"
    );
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes.iter().any(|note| note
            .as_str()
            .unwrap_or_default()
            .contains("does not report")),
        "{notes:?}"
    );
}

/// `childrenTruncated:true` keeps the meshes the reply listed but refuses the volume: a
/// partial union would read as a shrunken box of the real one (review C4). With a saved file
/// present that WOULD answer for the listed mesh (the test fails without the fix), the
/// problem names the truncation and the volume stays null.
#[tokio::test]
async fn live_truncated_children_list_refuses_the_volume() {
    use cs2_mcp_common::mock::{MockBridge, MockNode};
    let tree = Tree::new("live-truncated");
    let mb = MockBridge::builder()
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(
                5,
                "trigger_multiple",
                [96.0, -320.0, 32.0],
                Default::default(),
            ),
        ])
        .responder("entity.get", |params| {
            let id = params["nodeID"].as_i64().unwrap_or(0);
            let mut reply =
                m219_entity_get(id, json!([{"nodeID": 66, "class": "mesh"}]), json!([]));
            reply["childrenTruncated"] = json!(true);
            reply
        })
        .start()
        .await
        .unwrap();
    tree.install_fgd();
    let probe = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vmap/probe.vmap");
    std::fs::create_dir_all(tree.addon().join("maps")).expect("maps dir");
    std::fs::copy(&probe, tree.addon().join("maps/probe.vmap")).expect("saved file");
    let server = hammer_mcp::build_server(&config_with_bridge(&tree, &mb.discovery_path)).unwrap();

    let out = call(&server, "list_triggers", json!({})).await.unwrap();
    let trigger = &out["triggers"][0];
    assert_eq!(trigger["meshes"], json!([66]), "{trigger}");
    assert_eq!(
        trigger["volume"],
        Value::Null,
        "a truncated list is never a volume: {trigger}"
    );
    assert_eq!(
        trigger["problems"],
        json!(["children list truncated by the bridge"]),
        "{trigger}"
    );
}

// -------------------------------------------------------------------------------------------
// The real game tree (#[ignore]; run with `cargo test -p hammer-mcp --test triggers_query -- --ignored`)
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

const TEMPLATE_DEFUSE: &str = "workshop/run/app/content/csgo/maps/templates/template_defuse.vmap";

/// The REAL FGD classifies the REAL template map (oracle-verified shape: `func_bomb_target`
/// nodeIDs 33/34 with meshes 31/32, `func_buyzone` 67/70 with 66/71,
/// `post_processing_volume` 74 with 75 — every one with a non-null volume).
#[tokio::test]
#[ignore]
async fn real_tree_template_defuse_lists_its_brush_entities_with_volumes() {
    let root = project_root();
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

    let defuse = root.join(TEMPLATE_DEFUSE).display().to_string();
    let out = call(
        &server,
        "list_triggers",
        json!({"path": defuse, "kind": "solid"}),
    )
    .await
    .unwrap();
    assert_eq!(out["source"], json!("file"));
    let by_id = |id: i64| -> Option<Value> {
        out["triggers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|trigger| trigger["nodeID"] == json!(id))
            .cloned()
    };
    for (id, mesh) in [(33, 31), (34, 32), (67, 66), (70, 71), (74, 75)] {
        let trigger =
            by_id(id).unwrap_or_else(|| panic!("template_defuse lists nodeID {id}: {}", out));
        assert_eq!(trigger["meshes"], json!([mesh]), "{trigger}");
        assert!(trigger["volume"].is_object(), "{trigger}");
        assert!(trigger["volume"]["min"].is_array());
        assert!(
            trigger["volume"]["size"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s.as_f64().unwrap() > 0.0),
            "{trigger}"
        );
        assert_eq!(trigger["problems"], json!([]), "{}", trigger);
    }
    assert_eq!(out["staleRead"], json!(false));
    let notes = out["notes"].as_array().expect("notes");
    assert!(notes.is_empty(), "{notes:?}");
}

/// The REAL `cs_script_demo.vmap` addon map carries I/O connections (the fixtures README's
/// real-tree table): kind trigger lists `trigger.grenade` (nodeID 296) with its
/// `OnTrigger -> scriptent RunScriptInput "GrenadeHit"` — the file-route connections path
/// (review C1) against the real writer. Read-only against the REAL game root (the real FGD
/// classifies; the install's read path the entities_file real-tree tests use); the test
/// fails loudly when the game tree is missing.
#[tokio::test]
#[ignore]
async fn real_tree_cs_script_demo_trigger_lists_its_connection() {
    let root = project_root();
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

    // content-relative form: `csgo_addons/<addon>/maps/x.vmap` under the mapped app content
    let out = call(
        &server,
        "list_triggers",
        json!({"path": "csgo_addons/cs_script_demo/maps/cs_script_demo.vmap", "class":
               "trigger_multiple"}),
    )
    .await
    .unwrap();
    assert_eq!(out["source"], json!("file"), "{out}");
    let grenade = out["triggers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|trigger| trigger["nodeID"] == json!(296))
        .unwrap_or_else(|| panic!("trigger.grenade (296) listed: {}", out));
    assert_eq!(grenade["classname"], json!("trigger_multiple"));
    assert_eq!(grenade["targetname"], json!("trigger.grenade"));
    assert_eq!(
        grenade["connections"],
        json!([{
            "outputName": "OnTrigger",
            "targetType": 7,
            "targetName": "scriptent",
            "inputName": "RunScriptInput",
            "overrideParam": "GrenadeHit",
            "delay": 0.0,
            "timesToFire": -1,
        }]),
        "{grenade}"
    );
    // the kind trigger route keeps the REAL FGD's trigger set: the other trigger_multiples
    // stay in (their empty connectionsData reads as [])
    for trigger in out["triggers"].as_array().unwrap() {
        assert!(
            trigger["connections"].is_array(),
            "every record carries the array: {trigger}"
        );
    }
}
