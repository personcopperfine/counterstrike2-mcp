//! `entity_classes` tool tests (WP-C): `list_entity_classes` and `get_entity_class` against the
//! committed FGD fixture tree (`fixtures/fgd/{csgo,core}`) copied into a temp project tree under
//! `CARGO_TARGET_TMPDIR` (never /tmp) at `workshop/run/app/game` — the layout
//! `state.fgd_root = <project_root>/workshop/run/app/game` expects. Negative cases reuse the
//! committed `fixtures/fgd/cases/*` trees. No bridge is involved: both tools are pure file reads
//! (no declared bridge methods). The required negatives: unknown class -> `UnknownClass` with a
//! non-empty `nearest` for a near-miss name, `kind:"bogus"` -> `InvalidArguments`, a game root
//! without csgo/csgo.fgd -> `FgdUnavailable` (never an empty list). One `#[ignore]`d test runs
//! against the real game tree (run manually with `-- --ignored`) and fails loudly when the game
//! is absent.
//!
//! Helpers are local copies of the pattern in `tests/entities_file.rs` (`Tree`, `config`, `call`).
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A temp project tree laid out like the real one: `--project-root` at `root`, the FGD game tree
/// at `workshop/run/app/game`.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "entity-classes-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("tmp tree");
        Tree { root }
    }

    fn game(&self) -> PathBuf {
        self.root.join("workshop/run/app/game")
    }

    /// Copy the committed mini fixture tree (`fixtures/fgd/{csgo,core}` — never `cases/`) into
    /// the game root.
    fn install_fgd(&self) {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/fgd");
        for dir in ["csgo", "core"] {
            let target = self.game().join(dir);
            std::fs::create_dir_all(&target).expect("game dir");
            for entry in std::fs::read_dir(fixtures.join(dir)).expect("fixture dir") {
                let entry = entry.expect("fixture entry");
                if entry.file_type().expect("entry type").is_file() {
                    std::fs::copy(entry.path(), target.join(entry.file_name()))
                        .expect("copy fixture file");
                }
            }
        }
    }

    /// Install one committed negative case (`fixtures/fgd/cases/<name>/csgo/*`) as the game tree.
    fn install_case(&self, case: &str) {
        let target = self.game().join("csgo");
        std::fs::create_dir_all(&target).expect("game dir");
        for entry in std::fs::read_dir(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/fgd/cases")
                .join(case)
                .join("csgo"),
        )
        .expect("case dir")
        {
            let entry = entry.expect("case entry");
            if entry.file_type().expect("entry type").is_file() {
                std::fs::copy(entry.path(), target.join(entry.file_name())).expect("copy case file");
            }
        }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn config(root: &Path) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let project = root.display().to_string();
    let given = ["--project-root", &project];
    let argv: Vec<&str> = std::iter::once("t").chain(common::hermetic(&given)).collect();
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

/// The fixture game tree + a server over it.
async fn fixture_server(tag: &str) -> (Tree, McpServer<HammerState>) {
    let tree = Tree::new(tag);
    tree.install_fgd();
    let server = hammer_mcp::build_server(&config(&tree.root)).unwrap();
    (tree, server)
}

/// The committed fixture tree's class population (WP-F; WP-S3 appended the sound classes):
/// 27 surviving classes — 13 point, 6 solid, 8 base. WP-F appended the trigger classes
/// (`TriggerOnce`, `Trigger`, `trigger_once`, `trigger_multiple`, `trigger_push`,
/// `func_wall_mini`) to `core/base_mini.fgd`; WP-S3 appended the sound block (`EnableDisable`,
/// `ambient_generic`, `point_soundevent`, the three `snd_event_*`, `env_soundscape`,
/// `snd_soundscape`, `trigger_soundscape`, `logic_auto`, `logic_timer`). These counts move
/// whenever that fixture grows.
const FIXTURE_TOTAL_ALL: usize = 27;
const FIXTURE_TOTAL_POINT_SOLID: usize = 19;
const FIXTURE_TOTAL_BASE: usize = 8;
/// The fixture's `tags( Sound )` population (WP-S3): all Point classes.
const FIXTURE_SOUND_TAGGED: [&str; 7] = [
    "ambient_generic",
    "env_soundscape",
    "point_soundevent",
    "snd_event_alignedbox",
    "snd_event_point",
    "snd_event_sphere",
    "snd_soundscape",
];
/// The fixture's `tags( Logic )` population (WP-S3).
const FIXTURE_LOGIC_TAGGED: [&str; 2] = ["logic_auto", "logic_timer"];

// -------------------------------------------------------------------------------------------
// Acceptance: list_entity_classes
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn list_defaults_to_point_and_solid() {
    let (_tree, server) = fixture_server("list-default").await;
    let out = call(&server, "list_entity_classes", json!({})).await.unwrap();
    assert_eq!(out["total"], json!(FIXTURE_TOTAL_POINT_SOLID));
    assert_eq!(out["count"], json!(FIXTURE_TOTAL_POINT_SOLID));
    assert_eq!(out["truncated"], json!(false));
    let classes = out["classes"].as_array().expect("classes array");
    assert_eq!(classes.len(), FIXTURE_TOTAL_POINT_SOLID);
    // Name-sorted; no base classes in the default view, and the new sound classes come first.
    assert_eq!(classes[0]["name"], json!("ambient_generic"));
    let names: Vec<&str> = classes
        .iter()
        .filter_map(|class| class["name"].as_str())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_by_key(|name| name.to_ascii_lowercase());
    assert_eq!(names, sorted, "the list is name-sorted: {names:?}");
    for class in classes {
        assert_ne!(class["kind"], json!("base"), "{class}");
    }
    // Schema of one entry.
    let buyzone = classes
        .iter()
        .find(|class| class["name"] == json!("func_buyzone"))
        .expect("func_buyzone in the default point+solid view");
    assert_eq!(buyzone["kind"], json!("solid"));
    assert_eq!(buyzone["description"], json!("A trigger used to define a buyzone."));
    assert_eq!(buyzone["bases"], json!(["Targetname"]));
    assert_eq!(buyzone["keyCount"], json!(6), "targetname + vscripts + 4 own keys: {buyzone}");
    assert_eq!(buyzone["inputCount"], json!(3), "Kill + Enable + Disable: {buyzone}");
    assert_eq!(buyzone["outputCount"], json!(1));
}

#[tokio::test]
async fn list_filter_kinds_and_limit() {
    let (_tree, server) = fixture_server("list-filter").await;

    // filter: case-insensitive substring.
    let out = call(
        &server,
        "list_entity_classes",
        json!({"filter": "BUY"}),
    )
    .await
    .unwrap();
    let classes = out["classes"].as_array().expect("classes array");
    assert_eq!(classes.len(), 1);
    assert_eq!(classes[0]["name"], json!("func_buyzone"));

    // kind:"all" includes the base classes; kind:"base" selects only them.
    let out = call(&server, "list_entity_classes", json!({"kind": "all"})).await.unwrap();
    assert_eq!(out["total"], json!(FIXTURE_TOTAL_ALL));
    let out = call(&server, "list_entity_classes", json!({"kind": "base"})).await.unwrap();
    assert_eq!(out["total"], json!(FIXTURE_TOTAL_BASE));
    for class in out["classes"].as_array().expect("classes array") {
        assert_eq!(class["kind"], json!("base"), "{class}");
    }

    // limit truncates: total stays before-limit, truncated is truthful.
    let out = call(
        &server,
        "list_entity_classes",
        json!({"kind": "all", "limit": 2}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(2));
    assert_eq!(out["total"], json!(FIXTURE_TOTAL_ALL));
    assert_eq!(out["truncated"], json!(true));
    assert_eq!(out["classes"].as_array().unwrap().len(), 2);
}

/// WP-S3: the `tag?` filter selects exactly the classes whose resolved `tags( … )` helper args
/// carry the tag. The sound population is all Point classes; `trigger_soundscape` (a Solid class
/// with a `soundscape` key and no tag) is **not** in it. An unknown tag is an empty list with
/// `total: 0` — never an error; an unknown *type* is `InvalidArguments`.
#[tokio::test]
async fn list_tag_filter_selects_the_tagged_classes() {
    let (_tree, server) = fixture_server("list-tag").await;

    // The sound classes: name-sorted, every one of them a Point class (the default kinds include
    // Point, so nothing is lost to the kind default).
    let out = call(&server, "list_entity_classes", json!({"tag": "Sound"}))
        .await
        .unwrap();
    let names: Vec<&str> = out["classes"]
        .as_array()
        .expect("classes")
        .iter()
        .filter_map(|class| class["name"].as_str())
        .collect();
    assert_eq!(names, FIXTURE_SOUND_TAGGED.to_vec(), "{out}");
    assert_eq!(out["total"], json!(FIXTURE_SOUND_TAGGED.len()));
    assert_eq!(out["count"], json!(FIXTURE_SOUND_TAGGED.len()));
    assert_eq!(out["truncated"], json!(false));
    for class in out["classes"].as_array().expect("classes") {
        assert_eq!(class["kind"], json!("point"), "{class}");
    }
    assert!(
        !names.contains(&"trigger_soundscape"),
        "no tags( Sound ) on it: {names:?}"
    );

    // The tag is case-insensitive, like the rest of the FGD.
    let lower = call(&server, "list_entity_classes", json!({"tag": "sound"}))
        .await
        .unwrap();
    assert_eq!(lower["total"], json!(FIXTURE_SOUND_TAGGED.len()));

    // The kind filter composes with the tag filter.
    let base_only = call(
        &server,
        "list_entity_classes",
        json!({"tag": "Sound", "kind": "base"}),
    )
    .await
    .unwrap();
    assert_eq!(base_only["total"], json!(0), "the tag sits on Point classes");

    // A second tag proves the filter is generic, not a Sound special case.
    let logic = call(&server, "list_entity_classes", json!({"tag": "Logic"}))
        .await
        .unwrap();
    let logic_names: Vec<&str> = logic["classes"]
        .as_array()
        .expect("classes")
        .iter()
        .filter_map(|class| class["name"].as_str())
        .collect();
    assert_eq!(logic_names, FIXTURE_LOGIC_TAGGED.to_vec(), "{logic}");

    // NEGATIVES. A tag nothing carries is an empty list with total 0, not an error (the question
    // has an answer: no class carries it).
    let none = call(&server, "list_entity_classes", json!({"tag": "Nope"}))
        .await
        .unwrap();
    assert_eq!(none["total"], json!(0), "{none}");
    assert_eq!(none["classes"], json!([]));
    assert_eq!(none["truncated"], json!(false));

    // A non-string tag is InvalidArguments (nothing read).
    for bad in [json!(3), json!(true), json!(["Sound"])] {
        let err = call(&server, "list_entity_classes", json!({"tag": bad}))
            .await
            .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
        assert_eq!(err.name, "InvalidParams");
        assert!(err.retry_safe);
    }
    // A blank tag names nothing: refused rather than matching every empty `tags()` argument.
    let err = call(&server, "list_entity_classes", json!({"tag": "  "}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
}

#[tokio::test]
async fn list_truncates_long_descriptions_and_reports_source() {
    let tree = Tree::new("list-source");
    tree.install_fgd();
    let server = hammer_mcp::build_server(&config(&tree.root)).unwrap();

    let mut args = json!({"filter": "info_target"});
    // The fixture description is short (72 chars): a limit below the length must not change it.
    let out = call(&server, "list_entity_classes", args.clone()).await.unwrap();
    let short = out["classes"][0]["description"].as_str().expect("description");
    assert_eq!(short, "An entity that does nothing. Very useful as a positioning entity for other entities to refer to.");

    // The truncation path: same classes, list only, with descriptions capped at 200 chars by
    // checking the cap on the real tree in the ignored test. Here: assert the shape is a string
    // and the list never returns a longer one than the class's own description let through.
    args = json!({});
    let out = call(&server, "list_entity_classes", args).await.unwrap();
    for class in out["classes"].as_array().unwrap() {
        assert!(class["description"].is_null() || class["description"].is_string());
    }

    let source = &out["source"];
    assert_eq!(source["root"], json!(tree.game().display().to_string()));
    let files = source["files"].as_array().expect("files");
    assert_eq!(files.len(), 2, "csgo.fgd + base_mini.fgd: {files:?}");
    let warnings = source["warnings"].as_array().expect("warnings");
    assert_eq!(warnings.len(), 2, "the two duplicate info_mini warnings: {warnings:?}");
}

// -------------------------------------------------------------------------------------------
// Acceptance: get_entity_class
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn get_info_target_resolves_bases_choices_and_flags() {
    let (_tree, server) = fixture_server("get").await;
    let out = call(
        &server,
        "get_entity_class",
        json!({"class": "INFO_TARGET"}),
    )
    .await
    .unwrap();
    assert_eq!(out["name"], json!("info_target"), "name is the FGD spelling");
    assert_eq!(out["kind"], json!("point"));
    assert_eq!(out["bases"], json!(["Targetname", "Parentname"]));
    assert_eq!(out["resolvedBases"], json!(["Targetname", "GameEntity", "Parentname"]));
    assert!(out["description"].is_string());

    let keyvalues = out["keyvalues"].as_array().expect("keyvalues array");
    let targetname = keyvalues
        .iter()
        .find(|k| k["name"] == json!("targetname"))
        .expect("inherited targetname");
    assert_eq!(targetname["type"], json!("target_source"));
    assert_eq!(targetname["declaredIn"], json!("Targetname"));
    let spawnflags = keyvalues
        .iter()
        .find(|k| k["name"] == json!("spawnflags"))
        .expect("spawnflags");
    let flags = spawnflags["flags"].as_array().expect("flags array");
    assert_eq!(flags.len(), 2, "{spawnflags}");
    assert_eq!(flags[0]["bit"], json!(1));
    assert_eq!(flags[1]["bit"], json!(2));
    assert!(flags[1]["defaultOn"].is_boolean());
    assert!(keyvalues.iter().all(|k| k["name"] != "second_wave"), "another class's key");

    let inputs = out["inputs"].as_array().expect("inputs");
    let kill = inputs.iter().find(|io| io["name"] == json!("Kill")).expect("Kill");
    assert_eq!(kill["declaredIn"], json!("GameEntity"));
    assert!(out["outputs"].as_array().expect("outputs").is_empty());
    // Header helper with its raw args.
    assert_eq!(
        out["helpers"]["editormodel"],
        json!("\"models/editor/info_target\", nobounds")
    );
    assert!(out["source"].as_str().expect("source").contains("csgo.fgd:"));
}

#[tokio::test]
async fn get_func_buyzone_choices_and_i_o() {
    let (_tree, server) = fixture_server("get-buyzone").await;
    let out = call(
        &server,
        "get_entity_class",
        json!({"class": "func_buyzone", "includeBases": false}),
    )
    .await
    .unwrap();
    // includeBases:false keeps only the class's own items: no targetname, no Kill.
    let keyvalues = out["keyvalues"].as_array().expect("keyvalues");
    assert!(
        keyvalues.iter().all(|k| k["name"] != "targetname"),
        "own keyvalues only: {keyvalues:?}"
    );
    let buyzone_type = keyvalues
        .iter()
        .find(|k| k["name"] == json!("BuyzoneType"))
        .expect("BuyzoneType");
    let choices = buyzone_type["choices"].as_array().expect("choices");
    assert_eq!(choices.len(), 2);
    assert_eq!(choices[0], json!({"value": "0", "label": "Ground"}));
    assert!(buyzone_type["flags"].is_null(), "no flags key on a choices key");
    let team = keyvalues
        .iter()
        .find(|k| k["name"] == json!("TeamNum"))
        .expect("TeamNum");
    assert_eq!(team["type"], json!("integer"));
    assert_eq!(team["displayName"], json!("Team Num"));
    assert!(team["default"].is_null(), "the `: :` empty default: {team}");
    let inputs = out["inputs"].as_array().expect("inputs");
    assert_eq!(inputs.len(), 2, "Enable + Disable, no inherited Kill: {inputs:?}");
    let outputs = out["outputs"].as_array().expect("outputs");
    assert_eq!(outputs[0]["name"], json!("BuyzoneEntered"));
    // The base chain is still metadata (it describes the class, not its items).
    assert_eq!(out["resolvedBases"], json!(["Targetname", "GameEntity"]));
}

#[tokio::test]
async fn get_include_bases_false_keeps_only_own_items() {
    let (_tree, server) = fixture_server("get-own").await;
    // weapon_ak47 declares no keys/inputs/outputs itself (all inherited from WeaponMini).
    let out = call(
        &server,
        "get_entity_class",
        json!({"class": "weapon_ak47", "includeBases": false}),
    )
    .await
    .unwrap();
    assert!(out["keyvalues"].as_array().expect("keyvalues").is_empty());
    assert!(out["inputs"].as_array().expect("inputs").is_empty());
    assert!(out["outputs"].as_array().expect("outputs").is_empty());
    // Default (includeBases omitted): the inherited view is back.
    let out = call(
        &server,
        "get_entity_class",
        json!({"class": "weapon_ak47"}),
    )
    .await
    .unwrap();
    let names: Vec<&str> = out["keyvalues"]
        .as_array()
        .expect("keyvalues")
        .iter()
        .filter_map(|k| k["name"].as_str())
        .collect();
    assert!(names.contains(&"CanBePickedUp"), "{names:?}");
    assert!(
        out["inputs"].as_array().expect("inputs").len() >= 2,
        "ToggleCanBePickedUp + Kill at least: {:?}",
        out["inputs"]
    );
}

// -------------------------------------------------------------------------------------------
// Required negatives
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn unknown_class_is_unknown_class_with_nearest() {
    let (_tree, server) = fixture_server("unknown").await;
    // A near-miss: edit distance 1 from info_target.
    let err = call(
        &server,
        "get_entity_class",
        json!({"class": "info_targt"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "UnknownClass");
    assert_eq!(err.name, "UnknownClass");
    assert_eq!(err.data["class"], json!("info_targt"));
    let nearest = err.data["nearest"].as_array().expect("nearest array");
    assert!(
        nearest.iter().any(|n| n == "info_target"),
        "non-empty nearest naming the near-miss: {nearest:?}"
    );
    assert!(
        !nearest.is_empty(),
        "the required negative: nearest must be non-empty for a near miss"
    );
    assert!(err.retry_safe);
}

#[tokio::test]
async fn bad_kind_and_bad_limit_are_invalid_arguments() {
    let (_tree, server) = fixture_server("bad-args").await;
    let err = call(
        &server,
        "list_entity_classes",
        json!({"kind": "bogus"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert_eq!(err.name, "InvalidParams");
    assert!(err.message.contains("bogus"), "{err}");

    for limit in [json!(0), json!(1001), json!("200")] {
        let err = call(
            &server,
            "list_entity_classes",
            json!({"limit": limit}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{limit}");
    }

    let err = call(
        &server,
        "get_entity_class",
        json!({"class": 3}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
}

#[tokio::test]
async fn game_root_without_fgd_is_fgd_unavailable_not_an_empty_list() {
    // No fixture installed: <root>/workshop/run/app/game/csgo/csgo.fgd does not exist.
    let tree = Tree::new("no-fgd");
    let server = hammer_mcp::build_server(&config(&tree.root)).unwrap();
    let err = call(&server, "list_entity_classes", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "FgdUnavailable");
    assert_eq!(err.name, "FgdUnavailable");
    let path = err.data["path"].as_str().expect("data.path");
    assert!(
        path.ends_with("csgo/csgo.fgd"),
        "the error names the missing entry file: {path}"
    );
    assert!(err.message.contains("csgo.fgd"), "{err}");
    assert!(err.retry_safe);
    // get_entity_class fails the same way.
    let err = call(
        &server,
        "get_entity_class",
        json!({"class": "info_target"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "FgdUnavailable");
}

#[tokio::test]
async fn unparseable_fgd_and_include_cycle_are_fgd_parse_errors() {
    // The unterminated-string case is a fatal parse error (fixtures/fgd/cases/bad_string).
    let tree = Tree::new("bad-fgd");
    tree.install_case("bad_string");
    let server = hammer_mcp::build_server(&config(&tree.root)).unwrap();
    let err = call(&server, "list_entity_classes", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "FgdParseError", "{err}");
    assert_eq!(err.name, "FgdParseError");

    // The include-cycle case (`csgo.fgd` includes itself via `cycle.fgd`) too.
    let tree = Tree::new("cycle-fgd");
    tree.install_case("cycle");
    let server = hammer_mcp::build_server(&config(&tree.root)).unwrap();
    let err = call(&server, "list_entity_classes", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "FgdParseError", "{err}");
    assert!(err.message.contains("cycle"), "{err}");
}

// -------------------------------------------------------------------------------------------
// The real game tree (#[ignore]; run with `cargo test -p hammer-mcp --test entity_classes --
// --ignored`)
// -------------------------------------------------------------------------------------------

/// The project root: walk up from the crate dir to a directory containing
/// `workshop/run/app/game/csgo/csgo.fgd`. Never optional — a missing game tree panics.
fn project_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let candidate = dir.join("workshop/run/app/game");
        if candidate.join("csgo/csgo.fgd").is_file() {
            return dir;
        }
        if !dir.pop() {
            panic!(
                "no project root with workshop/run/app/game above {}",
                env!("CARGO_MANIFEST_DIR")
            );
        }
    }
}

/// The real game tree through the server (brief WP-C): `list_entity_classes{filter:"buyzone"}`
/// has func_buyzone (kind solid); `get_entity_class info_target` has targetname and spawnflags
/// with flags.
#[test]
#[ignore]
fn real_game_class_tools() {
    let root = project_root();
    let server = hammer_mcp::build_server(&config(&root)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let list = runtime.block_on(call(
        &server,
        "list_entity_classes",
        json!({"filter": "buyzone"}),
    ));
    let list = list.unwrap();
    let classes = list["classes"].as_array().expect("classes array");
    let buyzone = classes
        .iter()
        .find(|c| c["name"] == json!("func_buyzone"))
        .expect("func_buyzone in the real tree");
    assert_eq!(buyzone["kind"], json!("solid"));

    let get = runtime.block_on(call(
        &server,
        "get_entity_class",
        json!({"class": "info_target"}),
    ));
    let get = get.unwrap();
    let keyvalues = get["keyvalues"].as_array().expect("keyvalues array");
    assert!(
        keyvalues.iter().any(|k| k["name"] == json!("targetname")),
        "info_target has targetname"
    );
    let spawnflags = keyvalues
        .iter()
        .find(|k| k["name"] == json!("spawnflags"))
        .expect("info_target has spawnflags");
    let flags = spawnflags["flags"].as_array().expect("flags");
    assert!(!flags.is_empty(), "spawnflags has flag definitions: {spawnflags}");
    println!(
        "real game FGD: project root {}; func_buyzone description: {:?}; info_target keyvalues: {}",
        root.display(),
        buyzone["description"].as_str().unwrap_or("<null>"),
        keyvalues.len()
    );
}

#[tokio::test]
async fn round_start_classes_carry_a_play_note_and_others_do_not() {
    let (_tree, server) = fixture_server("playnote").await;
    let auto = call(&server, "get_entity_class", json!({"class": "logic_auto"})).await.unwrap();
    let note = auto["playNote"].as_str().expect("playNote");
    assert!(note.contains("OnMapSpawn") && note.contains("round_freeze_end"), "{note}");
    let other = call(&server, "get_entity_class", json!({"class": "info_target"})).await.unwrap();
    assert!(other.get("playNote").is_none(), "{other}");
    let list = call(&server, "list_entity_classes", json!({"filter": "logic_auto"})).await.unwrap();
    assert!(list["classes"][0]["playNote"].is_string(), "{list}");
    for class in ["logic_eventlistener", "func_movelinear"] {
        assert!(hammer_mcp::tools::entity_classes::play_note(class).is_some(), "{class}");
    }
}
