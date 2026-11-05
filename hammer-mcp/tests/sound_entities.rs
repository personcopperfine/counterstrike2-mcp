//! `create_sound_entities` tests (WP-S4, plan §1 over the mock bridge): the kind→class map, the
//! exact key spellings (`soundName`, `startOnSpawn:"1"`, `radius:"256"` float text for the sphere
//! vs `"512"` for the soundscape, `box_mins:"-64 -64 -64"`, `enablesoundevent:"1"`+`soundevent`,
//! `position0..`), the helper entities (`logic_auto` with the `OnMapSpawn` delay connections,
//! `logic_timer` with `RefireTime`/`InitialDelay` text and `OnTimer -> StartSound`), the single
//! batch + per-entity connection groups, `undoGroups`, `PartialFailure` with the created ids —
//! and the negative list of target/WP.md, each pinned with nothing written.
//!
//! The FGD fixture is this suite's own string appended to `fixtures/fgd`'s `csgo/csgo.fgd` (the
//! triggers precedent; `tests/common/mod.rs`'s `PROPS_FGD` is untouched). The sound-name
//! verdicts are pinned per test server through the shared `SoundCheckSource` state seam
//! (`HammerState::set_sound_check_source`, the same seam `create_entities` /
//! `set_entity_keyvalues` use): the known/SourceOnly/FilePath/Stale shapes the real index only
//! produces against a game tree, with `Unknown` for every name the map does not carry.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockBuilder, MockNode, HAMMER_METHODS};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::soundevents::{SoundNameVerdict, SoundSource};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

mod common;

const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";

static TRAIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// -------------------------------------------------------------------------------------------
// The fixture tree: `common::FgdTree`'s copy of `fixtures/fgd` plus this suite's sound classes
// appended to `csgo/csgo.fgd` (the real FGD's shapes: `tags( Sound )` on the header helper, the
// `sound`-typed key, the snd_event inputs/outputs, the soundscape keys, the logic helpers).
// -------------------------------------------------------------------------------------------

const SOUND_FGD: &str = "

// ---- WP-S4 test fixture: the sound classes ------------------------------------------------
@BaseClass base(Targetname) tags( Sound )
= point_soundevent
[
	soundName(sound) : \"Sound event name\" : \"\" : \"The name of the soundevent to play.\"
	input StartSound(void) : \"Starts the sound event.\"
	input StopSound(void) : \"Stops the sound event.\"
	input PauseSound(void) : \"Pauses the sound event.\"
	input UnPauseSound(void) : \"Unpauses the sound event.\"
	input SetSoundEventName(string) : \"Sets the sound event name.\"
	input SetSourceEntity(string) : \"Sets the source entity.\"
	output SoundEventGuid(integer) : \"The sound event's GUID.\"
	output OnSoundFinished(void) : \"Fired when the sound event finishes.\"
]

@PointClass base(point_soundevent) tags( Sound ) = snd_event_point : \"Point sound event.\" []

@PointClass base(point_soundevent) tags( Sound ) = snd_event_sphere
[
	radius(float) : \"Radius\" : \"5.0\" : \"Radius of the sphere.\"
]

@PointClass base(point_soundevent) tags( Sound ) = snd_event_alignedbox
[
	box_mins(vector) : \"Box Mins\" : \"-64 -64 -64\"
	box_maxs(vector) : \"Box Maxs\" : \"64 64 64\"
]

@PointClass base(point_soundevent) tags( Sound ) = snd_event_orientedbox
[
	box_mins(vector) : \"Box Mins\" : \"-64 -64 -64\"
	box_maxs(vector) : \"Box Maxs\" : \"64 64 64\"
]

@PointClass base(Targetname) tags( Sound ) = env_soundscape : \"Soundscape\"
[
	radius(integer) : \"Radius\" : -1 : \"Radius to play the soundscape.\"
	soundscape(string) : \"Soundscape\" : \"Nothing\" : \"The legacy .vsc soundscape.\"
	enablesoundevent(boolean) : \"Enable Sound Event\" : 0
	soundevent(sound) : \"Sound Event\" : \"\" : \"The soundevent to play.\"
	position0(target_destination) : \"Position 0\" : \"\" : \"\"
	position1(target_destination) : \"Position 1\" : \"\" : \"\"
	input ToggleEnabled(void) : \"Toggles the soundscape.\"
	output OnPlay(void) : \"Fired when the soundscape plays.\"
]

@PointClass base(Targetname) tags( Sound ) = env_soundscape_proxy : \"Soundscape proxy\"
[
	MainSoundscapeName(target_destination) : \"Soundscape Entity\" : \"\" : \"The soundscape to get all sound parameters from.\"
	radius(integer) : \"Radius\" : 128 : \"Radius to act in.\"
]

@PointClass base(env_soundscape) tags( Sound ) = snd_soundscape : \"Soundscape entity\"
[
]

@PointClass base(env_soundscape) tags( Sound ) = env_soundscape_triggerable : \"Triggerable soundscape\"
[
]

@PointClass base(env_soundscape_triggerable) tags( Sound ) = snd_soundscape_triggerable : \"Triggerable soundscape entity\"
[
]

@PointClass base(Targetname) tags( Sound ) = snd_event_box_helper : \"Box helper\"
[
	box_mins(vector) : \"Box Mins\" : \"-64 -64 -64\"
	box_maxs(vector) : \"Box Maxs\" : \"64 64 64\"
]

@PointClass base(Targetname) tags( Sound ) = snd_event_param : \"Set a public parameter of a playing soundevent.\"
[
	parameterName(string) : \"Parameter Name\" : \"\" : \"Name of the parameter to set\"
	floatValue(float) : \"Float Value\" : \"\" : \"Single float value to set on parameter.\"
	input SetParamName(string) : \"Set the name of the parameter.\"
]

// the other tags(Sound) classes of the real FGD that declare no sound-typed key at all: a
// soundEvent on them has nowhere to go and is refused (their shapes are copied from base.fgd)
@PointClass base(env_soundscape_proxy) tags( Sound ) = snd_soundscape_proxy : \"Soundscape proxy\"
[
]

@PointClass base(Targetname) tags( Sound ) = snd_stack_save : \"Save/Load the fields of a specific operator stack.\"
[
	stackToSave(string) : \"Stack Name\" : \"\" : \"Name of the stack to save op fields.\"
]

@PointClass base(Targetname, Parentname) tags( Sound ) = snd_opvar_set_point : \"Sets an opvar value on a library stack\"
[
	opvarName(string) : \"OpVar Name\" : \"\" : \"The name of the OpVar to change\"
	input SetSourceEntity(string) : \"Sets the source entity.\"
]

@PointClass base(Targetname) = logic_auto : \"Logic Auto\"
[
	output OnMapSpawn(void) : \"Fired when the map spawns.\"
]

@PointClass base(Targetname) = logic_timer : \"Logic Timer\"
[
	RefireTime(float) : \"Refire time in seconds\" : \"0\"
	InitialDelay(float) : \"Initial delay in seconds\" : \"-1\"
	UseRandomTime(boolean) : \"Use random time\" : 0
	input FireTimer(void) : \"Fires the timer.\"
	output OnTimer(void) : \"Fires when the timer expires.\"
]
";

/// A temp project tree with the FGD fixture + [`SOUND_FGD`]; removed on drop, never under `/tmp`.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        // the fixture FGD copied here (NOT through `common::FgdTree`: its `Drop` removes the
        // tree it built — the triggers precedent copies the fixture dir itself)
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "snd-fgd-{tag}-{}-{}",
            std::process::id(),
            TRAIL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("workshop/run/app")).expect("temp tree");
        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/fgd"));
        common::copy_tree(fixture, &dir.join("workshop/run/app/game"));
        let entry = dir.join("workshop/run/app/game/csgo/csgo.fgd");
        let mut text = std::fs::read_to_string(&entry).expect("fixture csgo.fgd");
        text.push_str(SOUND_FGD);
        std::fs::write(&entry, text).expect("csgo.fgd with the sound classes");
        Tree { root: dir }
    }

    /// A tree with no `csgo/csgo.fgd`: the FGD-less warning path.
    fn without_fgd(tag: &str) -> Tree {
        let tree = Tree::new(tag);
        std::fs::remove_file(tree.root.join("workshop/run/app/game/csgo/csgo.fgd"))
            .expect("remove the entry FGD");
        tree
    }

    fn app(&self) -> PathBuf {
        self.root.join("workshop/run/app")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// -------------------------------------------------------------------------------------------
// The server + the mock bridge
// -------------------------------------------------------------------------------------------

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

fn config(tree: &Tree, discovery: &Path) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let app = tree.app().display().to_string();
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

fn build_server(tree: &Tree, mb: &MockBridge) -> McpServer<HammerState> {
    hammer_mcp::build_server(&config(tree, &mb.discovery_path)).unwrap()
}

/// The mock world the sound tests start from: nodeID 60 `info_target` named `taken` (an existing
/// NON-sound class for the class checks and as a connection target) and the world.
fn nodes() -> Vec<MockNode> {
    vec![
        MockNode::world(),
        MockNode::entity(60, "info_target", [0.0; 3], kv(&[("targetname", "taken")])),
    ]
}

async fn hammer_mock(world: Vec<MockNode>) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .hammer_nodes(world)
        .start()
        .await
        .unwrap()
}

async fn hammer_mock_with(
    world: Vec<MockNode>,
    tweak: impl FnOnce(MockBuilder) -> MockBuilder,
) -> MockBridge {
    tweak(
        MockBridge::builder()
            .doc(Some(make_doc(DOC_WIN, Some(true))))
            .hammer_nodes(world),
    )
    .start()
    .await
    .unwrap()
}

/// A result's `notes`/`warnings` array as one string (the assertions look for wordings).
fn notes(value: &Value, key: &str) -> String {
    value[key]
        .as_array()
        .map(|notes| {
            notes
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .unwrap_or_default()
}

/// A `{targetname: text}` keyvalue map for a seeded node (the mock's `MockNode::entity` takes the
/// map directly).
fn kv(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// One recorded request's params, with the per-call budget field removed (`deadlineMs` is
/// derived from this call's budget and asserted to be inside it; everything else is the tool's
/// own request).
fn wire(mb: &MockBridge, method: &str, index: usize) -> Value {
    let calls = mb.method_calls(method);
    let mut params = calls
        .get(index)
        .unwrap_or_else(|| panic!("{method} call {index} of {}", calls.len()))
        .params
        .clone();
    let deadline = params.get("deadlineMs").cloned().unwrap_or(Value::Null);
    assert!(
        deadline.as_u64().is_some_and(|ms| (1..=5000).contains(&ms)),
        "{method}: {params}"
    );
    params
        .as_object_mut()
        .expect("params object")
        .remove("deadlineMs");
    params
}

/// The mock world's nodes as `(nodeID, classname, targetname)` rows.
fn rows(mb: &MockBridge) -> Vec<(i64, String, Option<String>)> {
    mb.hammer_nodes()
        .into_iter()
        .map(|n| (n.node_id, n.classname, n.targetname))
        .collect()
}

/// Install the known verdicts the positive tests pin on this server's **state** — the shared
/// `SoundCheckSource` seam (`HammerState::set_sound_check_source`, production reads the real
/// index; a test server reads the fixed map, and any name it does not carry answers `Unknown`).
fn inject_known(server: &McpServer<HammerState>) {
    inject_verdicts(
        server,
        &[
            (
                "mcp.snd.point",
                SoundNameVerdict::Known {
                    name: "mcp.snd.point".into(),
                    source: SoundSource::AddonGame,
                    file: "game/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts_c"
                        .into(),
                },
            ),
            (
                "mcp.snd.scape",
                SoundNameVerdict::Known {
                    name: "mcp.snd.scape".into(),
                    source: SoundSource::AddonGame,
                    file: "game/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts_c"
                        .into(),
                },
            ),
            (
                "mcp.snd.src.only",
                SoundNameVerdict::SourceOnly {
                    file: "content/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts"
                        .into(),
                },
            ),
            ("sounds/foo.wav", SoundNameVerdict::FilePath),
        ],
    );
}

/// Write `name -> verdict` pairs into this server's fixed verdict map (the state seam, a second
/// installation merges into the map: each test owns its server, so there is no cross-talk).
fn inject_verdicts(server: &McpServer<HammerState>, pairs: &[(&str, SoundNameVerdict)]) {
    use hammer_mcp::tools::sound_check::SoundCheckSource;
    let state = server.state();
    let mut map = match state.sound_check_source() {
        SoundCheckSource::Fixed(map) => map,
        _ => Default::default(),
    };
    for (name, verdict) in pairs {
        map.insert((*name).to_string(), verdict.clone());
    }
    state.set_sound_check_source(Some(SoundCheckSource::Fixed(map)));
}

// -------------------------------------------------------------------------------------------
// Positive: the exact requests, the key spellings, the helpers, the undo groups
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn three_kinds_and_a_soundscape_pin_the_batch_and_the_connection_groups() {
    let tree = Tree::new("pos");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    let out = call(
        &server,
        "create_sound_entities",
        json!({
            "entities": [
                {"kind": "point", "soundEvent": "mcp.snd.point", "targetname": "snd_p",
                 "origin": [0, 0, 64], "startOnSpawn": true},
                {"kind": "sphere", "soundEvent": "mcp.snd.point", "targetname": "snd_s",
                 "origin": [256, 0, 64], "radius": 256,
                 "schedule": {"startDelay": 3}},
                {"kind": "alignedbox", "soundEvent": "mcp.snd.point", "targetname": "snd_b",
                 "origin": [0, 256, 64], "box": {"min": [-128, -128, -64], "max": [128, 128, 64]},
                 "schedule": {"repeatEvery": 5, "repeatInitialDelay": 1}},
                {"kind": "soundscape", "soundEvent": "mcp.snd.scape", "targetname": "snd_sc",
                 "origin": [16, 16, 16], "radius": 512,
                 "positions": ["pos_a", "pos_b"]}
            ],
            "undoLabel": "MCP: sounds"
        }),
    )
    .await
    .expect("created");

    assert_eq!(out["count"], 4);
    // ONE batch for the 4 entities + 2 helpers (sphere auto, box timer) — the soundscape has
    // neither — then one addConnections per scheduled entity (its helper's connections)
    assert_eq!(mb.method_calls("edit.batch").len(), 1);
    assert_eq!(mb.method_calls("entity.addConnections").len(), 2);
    assert_eq!(out["undoGroups"], 3);
    assert_eq!(
        out["undoLabels"],
        json!(["MCP: batch", "MCP: add connections", "MCP: add connections"])
    );
    assert_eq!(out["requestedUndoLabel"], "MCP: sounds");

    // the exact batch: every op's params, keyvalue spellings included
    let batch = wire(&mb, "edit.batch", 0);
    assert_eq!(batch["undoLabel"], "MCP: sounds");
    assert_eq!(batch["expectDoc"], json!({"id": "d1", "epoch": 1}));
    let ops = batch["ops"].as_array().expect("ops");
    assert_eq!(ops.len(), 6);
    assert_eq!(
        ops[0],
        json!({"method": "entity.create", "params": {
            "class": "snd_event_point",
            "origin": [0.0, 0.0, 64.0],
            "keyValues": {
                "startOnSpawn": "1",
                "soundName": "mcp.snd.point",
                "targetname": "snd_p",
            }
        }})
    );
    assert_eq!(
        ops[1],
        json!({"method": "entity.create", "params": {
            "class": "snd_event_sphere",
            "origin": [256.0, 0.0, 64.0],
            "keyValues": {
                "radius": "256",
                "soundName": "mcp.snd.point",
                "targetname": "snd_s",
            }
        }})
    );
    // the batch order is entity-then-its-helpers: the sphere's logic_auto comes before the
    // alignedbox, the alignedbox's logic_timer before the soundscape
    assert_eq!(
        ops[2],
        json!({"method": "entity.create", "params": {
            "class": "logic_auto",
            "origin": [256.0, 0.0, 64.0],
            "keyValues": {"targetname": "snd_s_auto"}
        }})
    );
    assert_eq!(
        ops[3],
        json!({"method": "entity.create", "params": {
            "class": "snd_event_alignedbox",
            "origin": [0.0, 256.0, 64.0],
            "keyValues": {
                "box_mins": "-128 -128 -64",
                "box_maxs": "128 128 64",
                "soundName": "mcp.snd.point",
                "targetname": "snd_b",
            }
        }})
    );
    assert_eq!(
        ops[4],
        json!({"method": "entity.create", "params": {
            "class": "logic_timer",
            "origin": [0.0, 256.0, 64.0],
            "keyValues": {
                "InitialDelay": "1",
                "RefireTime": "5",
                "UseRandomTime": "0",
                "targetname": "snd_b_timer",
            }
        }})
    );
    assert_eq!(
        ops[5],
        json!({"method": "entity.create", "params": {
            "class": "env_soundscape",
            "origin": [16.0, 16.0, 16.0],
            "keyValues": {
                "enablesoundevent": "1",
                "position0": "pos_a",
                "position1": "pos_b",
                "radius": "512",
                "soundevent": "mcp.snd.scape",
                "targetname": "snd_sc",
            }
        }})
    );

    // the two connection groups: the helpers' connections onto the HELPER nodes, targets
    // resolved after the batch (the wire delays and the RefireTime/InitialDelay text above)
    let add0 = wire(&mb, "entity.addConnections", 0);
    let auto_id = out["entities"][1]["helpers"][0]["nodeID"].as_i64().unwrap();
    assert_eq!(add0["nodeID"], auto_id);
    assert_eq!(
        add0["connections"],
        json!([
            {"output": "OnMapSpawn", "target": "snd_s", "input": "StartSound", "delay": 3.0}
        ])
    );
    let add1 = wire(&mb, "entity.addConnections", 1);
    let timer_id = out["entities"][2]["helpers"][0]["nodeID"].as_i64().unwrap();
    assert_eq!(add1["nodeID"], timer_id);
    assert_eq!(
        add1["connections"],
        json!([{"output": "OnTimer", "target": "snd_b", "input": "StartSound"}])
    );

    // the result shape: per-entity verdicts, helpers, the notes
    assert_eq!(out["entities"][0]["soundEvent"], json!({"name": "mcp.snd.point", "verdict": "known"}));
    assert_eq!(out["entities"][3]["soundEvent"], json!({"name": "mcp.snd.scape", "verdict": "known"}));
    assert_eq!(out["entities"][0]["helpers"], json!([]));
    assert_eq!(out["entities"][1]["helpers"][0]["class"], "logic_auto");
    assert_eq!(out["entities"][1]["helpers"][0]["targetname"], "snd_s_auto");
    assert_eq!(out["entities"][2]["helpers"][0]["class"], "logic_timer");
    assert_eq!(out["entities"][2]["helpers"][0]["targetname"], "snd_b_timer");
    assert_eq!(out["saved"], false);
    let notes = notes(&out, "notes");
    assert!(notes.contains("undo_last` 3 time(s) reverts"), "{notes}");
    assert!(notes.contains("no auto-save"), "{notes}");
    assert!(notes.contains("startOnSpawn:false entities wait for a StartSound input"), "{notes}");

    // the mock world really holds everything (the mock applied the batch and the connections)
    let world = rows(&mb);
    for (class, targetname) in [
        ("snd_event_point", Some("snd_p")),
        ("snd_event_sphere", Some("snd_s")),
        ("snd_event_alignedbox", Some("snd_b")),
        ("env_soundscape", Some("snd_sc")),
        ("logic_auto", Some("snd_s_auto")),
        ("logic_timer", Some("snd_b_timer")),
    ] {
        assert!(
            world
                .iter()
                .any(|(_, c, t)| c == class && *t == targetname.map(str::to_string)),
            "{class} missing: {world:?}"
        );
    }
    assert_eq!(
        world
            .iter()
            .find(|(_, _, t)| t.as_deref() == Some("snd_s"))
            .map(|(_, _, _)| ()),
        Some(()),
        "the sphere exists"
    );
    // the soundscape's keyvalues landed as sent (the mock stores the applied text)
    let scape = mb
        .hammer_nodes()
        .into_iter()
        .find(|n| n.classname == "env_soundscape")
        .expect("scape");
    assert_eq!(scape.keyvalues.get("radius").map(String::as_str), Some("512"));
    assert_eq!(scape.keyvalues.get("position0").map(String::as_str), Some("pos_a"));
}

#[tokio::test]
async fn undo_last_reverts_the_batch_and_every_connection_group() {
    let tree = Tree::new("undo");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    let out = call(
        &server,
        "create_sound_entities",
        json!({"entities": [
            {"kind": "point", "soundEvent": "mcp.snd.point", "targetname": "snd_u",
             "origin": [0, 0, 64], "schedule": {"startDelay": 1, "stopDelay": 4}},
        ]}),
    )
    .await
    .expect("created");
    assert_eq!(out["undoGroups"], 2, "the batch + the auto helper's connections");
    assert!(rows(&mb).iter().any(|(_, c, _)| c == "snd_event_point"));

    for _ in 0..out["undoGroups"].as_u64().unwrap() {
        call(&server, "undo_last", json!({})).await.expect("undo");
    }
    let world = rows(&mb);
    assert!(
        !world.iter().any(|(_, c, t)| c == "snd_event_point" || t.as_deref() == Some("snd_u_auto")),
        "the revert removed the entity and its helper: {world:?}"
    );
    assert!(
        world.iter().any(|(_, c, _)| c == "info_target"),
        "the pre-existing node stays: {world:?}"
    );
}

#[tokio::test]
async fn a_source_only_soundevent_creates_with_the_compile_note() {
    let tree = Tree::new("src-only");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    let out = call(
        &server,
        "create_sound_entities",
        json!({"entities": [
            {"kind": "point", "soundEvent": "mcp.snd.src.only", "origin": [0, 0, 64]},
        ]}),
    )
    .await
    .expect("created");
    assert_eq!(out["entities"][0]["soundEvent"]["verdict"], "sourceOnly");
    let notes = notes(&out["entities"][0], "notes");
    assert!(
        notes.contains("not compiled") && notes.contains("compile_soundevents"),
        "{notes}"
    );
    assert_eq!(mb.method_calls("edit.batch").len(), 1, "created anyway");
}

#[tokio::test]
async fn a_file_path_soundevent_is_written_verbatim_with_a_note() {
    let tree = Tree::new("filepath");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    let out = call(
        &server,
        "create_sound_entities",
        json!({"entities": [
            {"kind": "point", "soundEvent": "sounds/foo.wav", "origin": [0, 0, 64]},
        ]}),
    )
    .await
    .expect("created");
    assert_eq!(out["entities"][0]["soundEvent"]["verdict"], "filePath");
    let notes = notes(&out["entities"][0], "notes");
    assert!(notes.contains("file path") && notes.contains("never looked up"), "{notes}");
}

#[tokio::test]
async fn an_fgd_less_tree_creates_with_a_warning_instead_of_refusing() {
    let tree = Tree::without_fgd("nofgd");
    // no FGD: the verdict still gates, so inject it
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    let out = call(
        &server,
        "create_sound_entities",
        json!({"entities": [
            {"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, 64]},
        ]}),
    )
    .await
    .expect("created");
    let warnings = notes(&out, "warnings");
    assert!(warnings.contains("fgd unavailable"), "{warnings}");
    assert_eq!(out["fgd"]["files"], 0);
    assert_eq!(mb.method_calls("edit.batch").len(), 1);
    // without the FGD the family default sound key stands in
    assert_eq!(out["entities"][0]["keyValuesSent"]["soundName"], "mcp.snd.point");
}

// -------------------------------------------------------------------------------------------
// Negatives: every refusal pins that NOTHING was sent
// -------------------------------------------------------------------------------------------

async fn refused(
    server: &McpServer<HammerState>,
    mb: &MockBridge,
    args: Value,
    name: &str,
    message_part: &str,
) -> ToolError {
    let err = call(server, "create_sound_entities", args).await.unwrap_err();
    assert_eq!(err.name, name, "expected {name}, got {err:?}");
    assert!(err.retry_safe, "{name} is a local refusal: {err:?}");
    assert!(
        err.message.contains(message_part),
        "{name}: '{}' should contain '{message_part}': {err:?}",
        err.message
    );
    assert_eq!(mb.method_calls("edit.batch").len(), 0, "{name}: nothing written");
    assert_eq!(mb.method_calls("entity.addConnections").len(), 0, "{name}");
    assert_eq!(mb.method_calls("entity.find").len(), 0, "{name}: not even a read");
    err
}

fn one(entity: Value) -> Value {
    json!({"entities": [entity]})
}

#[tokio::test]
async fn kind_and_class_together_or_neither_are_refused() {
    let tree = Tree::new("xor");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    refused(
        &server,
        &mb,
        one(json!({"kind": "point", "class": "snd_event_point", "origin": [0, 0, 0],
                   "soundEvent": "mcp.snd.point"})),
        "InvalidParams",
        "exactly one of kind and class, got both",
    )
    .await;
    refused(
        &server,
        &mb,
        one(json!({"origin": [0, 0, 0], "soundEvent": "mcp.snd.point"})),
        "InvalidParams",
        "exactly one of kind and class is required",
    )
    .await;
}

#[tokio::test]
async fn a_non_sound_class_is_refused_not_a_sound_class() {
    let tree = Tree::new("notsound");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    let err = refused(
        &server,
        &mb,
        one(json!({"class": "info_target", "origin": [0, 0, 0], "soundEvent": "mcp.snd.point"})),
        "NotASoundClass",
        "no tags(Sound)",
    )
    .await;
    assert_eq!(err.data["class"], "info_target");
}

#[tokio::test]
async fn an_unknown_class_is_refused_with_suggestions() {
    let tree = Tree::new("unknownclass");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    let err = refused(
        &server,
        &mb,
        one(json!({"class": "snd_event_pont", "origin": [0, 0, 0], "soundEvent": "x"})),
        "UnknownClass",
        "not a class in the FGD database",
    )
    .await;
    assert!(
        err.data["nearest"].as_array().is_some_and(|n| !n.is_empty()),
        "suggestions present: {err:?}"
    );
}

#[tokio::test]
async fn a_sphere_radius_must_be_a_positive_number() {
    let tree = Tree::new("radius");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);
    for (radius, what) in [
        (json!(0), "must be > 0"),
        (json!(-4), "must be > 0"),
        (json!("256"), "must be a finite number"),
        (json!(1.5), "must be an integer"),
    ] {
        let kind = if radius.as_f64() == Some(1.5) { "soundscape" } else { "sphere" };
        let event = if radius.as_f64() == Some(1.5) { "mcp.snd.scape" } else { "mcp.snd.point" };
        refused(
            &server,
            &mb,
            one(json!({"kind": kind, "soundEvent": event, "origin": [0, 0, 0],
                       "radius": radius})),
            "InvalidParams",
            what,
        )
        .await;
    }
}

#[tokio::test]
async fn a_box_on_a_non_box_kind_and_a_bad_box_are_refused() {
    let tree = Tree::new("box");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);
    // kind:"point" with box
    refused(
        &server,
        &mb,
        one(json!({"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, 0],
                   "box": {"min": [0, 0, 0], "max": [8, 8, 8]}})),
        "InvalidParams",
        "only valid for the classes whose FGD declares box_mins/box_maxs",
    )
    .await;
    // min >= max on one axis
    refused(
        &server,
        &mb,
        one(json!({"kind": "alignedbox", "soundEvent": "mcp.snd.point", "origin": [0, 0, 0],
                   "box": {"min": [8, 0, 0], "max": [8, 8, 8]}})),
        "InvalidParams",
        "min 8 must be < max 8",
    )
    .await;
}

#[tokio::test]
async fn a_soundscape_needs_sound_event_or_legacy_soundscape() {
    let tree = Tree::new("scape-req");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    refused(
        &server,
        &mb,
        one(json!({"kind": "soundscape", "origin": [0, 0, 0]})),
        "InvalidParams",
        "soundEvent is required for 'env_soundscape'",
    )
    .await;
}

#[tokio::test]
async fn a_soundscape_refuses_a_schedule() {
    let tree = Tree::new("scape-sched");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);
    refused(
        &server,
        &mb,
        one(json!({"kind": "soundscape", "soundEvent": "mcp.snd.scape", "origin": [0, 0, 0],
                   "schedule": {"startDelay": 1}})),
        "InvalidParams",
        "needs a StartSound input",
    )
    .await;
}

#[tokio::test]
async fn a_schedule_needs_a_targetname() {
    let tree = Tree::new("sched-tn");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);
    refused(
        &server,
        &mb,
        one(json!({"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, 0],
                   "schedule": {"startDelay": 1}})),
        "InvalidParams",
        "schedule needs targetname",
    )
    .await;
}

#[tokio::test]
async fn a_stop_delay_at_or_before_the_start_delay_is_refused() {
    let tree = Tree::new("delays");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);
    refused(
        &server,
        &mb,
        one(json!({"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, 0],
                   "targetname": "snd_x", "schedule": {"startDelay": 3, "stopDelay": 3}})),
        "InvalidParams",
        "stopDelay (3) must be > startDelay (3)",
    )
    .await;
    refused(
        &server,
        &mb,
        one(json!({"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, 0],
                   "targetname": "snd_x", "schedule": {"startDelay": 3, "stopDelay": 1}})),
        "InvalidParams",
        "stopDelay (1) must be > startDelay (3)",
    )
    .await;
}

#[tokio::test]
async fn nine_positions_are_refused() {
    let tree = Tree::new("positions");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);
    refused(
        &server,
        &mb,
        one(json!({"kind": "soundscape", "soundEvent": "mcp.snd.scape", "origin": [0, 0, 0],
                   "positions": ["a", "b", "c", "d", "e", "f", "g", "h", "i"]})),
        "InvalidParams",
        "has 9 entries, 1..=8 fit",
    )
    .await;
}

#[tokio::test]
async fn the_owned_spellings_are_refused_inside_key_values() {
    let tree = Tree::new("reserved");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);
    for (key, value, part) in [
        ("soundName", json!("mcp.snd.point"), "use the field"),
        ("enablesoundevent", json!("1"), "use the field"),
        ("classname", json!("snd_event_point"), "cannot be set"),
        ("position0", json!("pos_a"), "use the field"),
        ("radius", json!("512"), "use the field"),
    ] {
        refused(
            &server,
            &mb,
            one(json!({"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, 0],
                       "keyValues": {key: value}})),
            "InvalidParams",
            part,
        )
        .await;
    }
}

#[tokio::test]
async fn a_whitespace_padded_sound_event_is_refused_naming_the_trimmed_name() {
    // seen live: `"mcp.snd.final "` was refused with the right suggestion but no word on why
    let tree = Tree::new("padded-snd");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);
    let err = refused(
        &server,
        &mb,
        one(json!({"kind": "point", "soundEvent": "mcp.snd.point ", "origin": [0, 0, 0]})),
        "SoundEventNotFound",
        "leading/trailing whitespace; the trimmed name is 'mcp.snd.point'",
    )
    .await;
    assert_eq!(err.data["name"], json!("mcp.snd.point "));

    // an unpadded unknown name says nothing about whitespace
    let err = refused(
        &server,
        &mb,
        one(json!({"kind": "point", "soundEvent": "mcp.snd.nope", "origin": [0, 0, 0]})),
        "SoundEventNotFound",
        "is not in the soundevent index",
    )
    .await;
    assert!(!err.message.contains("whitespace"), "{}", err.message);
}

#[tokio::test]
async fn an_unknown_sound_event_refuses_everything_with_suggestions() {
    let tree = Tree::new("unknown-snd");
    // The fixed seam answers Unknown for any name it does not carry; one test pins the
    // suggestion surface by merging an Unknown pair that carries suggestions into the map
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);
    inject_verdicts(
        &server,
        &[(
            "mcp.snd.poin",
            SoundNameVerdict::Unknown {
                suggestions: vec!["mcp.snd.point".into()],
            },
        )],
    );
    let err = refused(
        &server,
        &mb,
        one(json!({"kind": "point", "soundEvent": "mcp.snd.poin", "origin": [0, 0, 0]})),
        "SoundEventNotFound",
        "is not in the soundevent index",
    )
    .await;
    assert_eq!(err.data["didYouMean"], json!(["mcp.snd.point"]));
    assert_eq!(err.data["sourcesChecked"], json!(["addon-game", "addon-source", "vpk:csgo", "vpk:core"]));

    // allowUnknownSoundEvent:true creates it with the warning and the unchecked verdict
    let out = call(
        &server,
        "create_sound_entities",
        json!({"allowUnknownSoundEvent": true, "entities": [
            {"kind": "point", "soundEvent": "mcp.snd.poin", "origin": [0, 0, 64]},
        ]}),
    )
    .await
    .expect("created with the flag");
    assert_eq!(out["entities"][0]["soundEvent"]["verdict"], "unchecked");
    let warnings = notes(&out["entities"][0], "warnings");
    assert!(warnings.contains("not in the soundevent index"), "{warnings}");
    assert_eq!(mb.method_calls("edit.batch").len(), 1);
}

#[tokio::test]
async fn a_connection_output_the_class_lacks_refuses_everything() {
    let tree = Tree::new("unknown-output");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);
    let err = refused(
        &server,
        &mb,
        one(json!({"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, 0],
                   "connections": [{"output": "OnStartTouch", "target": "taken",
                                    "input": "Toggle"}]})),
        "UnknownOutput",
        "is not an output of the source class",
    )
    .await;
    assert_eq!(err.data["output"], "OnStartTouch");
}

#[tokio::test]
async fn a_helper_targetname_already_in_the_document_is_refused() {
    let tree = Tree::new("helper-clash");
    // the world already holds `taken` — name the entity so its helper collides
    let world = vec![
        MockNode::world(),
        MockNode::entity(
            60,
            "info_target",
            [0.0; 3],
            kv(&[("targetname", "snd_x_auto")]),
        ),
    ];
    let mb = hammer_mock(world).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);
    let err = call(
        &server,
        "create_sound_entities",
        one(json!({"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, 0],
                   "targetname": "snd_x", "schedule": {"startDelay": 1}})),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(
        err.message.contains("'snd_x_auto' is already in the live document"),
        "{err:?}"
    );
    assert_eq!(mb.method_calls("edit.batch").len(), 0, "nothing written");
    assert_eq!(mb.method_calls("entity.addConnections").len(), 0);
    // the clash check itself IS the read: exactly one find
    assert_eq!(mb.method_calls("entity.find").len(), 1);
    assert_eq!(wire(&mb, "entity.find", 0)["targetname"], "snd_x_auto");
}

#[tokio::test]
async fn a_maybe_executed_batch_is_outcome_unknown() {
    let tree = Tree::new("batch-32603");
    // the mock's -32603 on edit.batch runs the op and then fails (the recorded bridge shape)
    let mb = hammer_mock_with(nodes(), |b| {
        b.error("edit.batch", -32603, "internal error after applying", None)
    })
    .await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    let err = call(
        &server,
        "create_sound_entities",
        one(json!({"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, 64]})),
    )
    .await
    .unwrap_err();
    // create_entities' convention: the bridge error's own name stands (results pass through
    // sanitized), outcomeUnknown + the counter mark the maybe-executed write
    assert_eq!(err.name, "ProtocolError", "{err:?}");
    assert!(err.outcome_unknown, "maybe-executed: {err:?}");
    assert_eq!(err.data["unsavedBridgeEdits"], 1, "the batch counted itself: {err:?}");
    assert_eq!(err.data["requested"], 1);
    // the mock's -32603 runs the op: the entity really landed
    assert!(rows(&mb).iter().any(|(_, c, _)| c == "snd_event_point"));
}

#[tokio::test]
async fn a_connections_failure_after_the_batch_is_a_partial_failure_naming_the_created_ids() {
    let tree = Tree::new("conn-fail");
    let mb = hammer_mock_with(nodes(), |b| {
        b.error("entity.addConnections", -32602, "bad target", None)
    })
    .await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    let err = call(
        &server,
        "create_sound_entities",
        one(json!({"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, 64],
                   "targetname": "snd_f", "schedule": {"startDelay": 1}})),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PartialFailure", "{err:?}");
    assert!(!err.retry_safe, "a retry would duplicate the entities");
    let created = err.data["createdNodeIDs"].as_array().expect("ids");
    assert_eq!(created.len(), 2, "the entity + its helper: {err:?}");
    assert_eq!(err.data["undoGroups"], 1, "only the batch had opened");
    assert_eq!(err.data["undoLabels"], json!(["MCP: batch"]));
    assert_eq!(err.data["step"], "entity.addConnections");
    assert!(err.message.contains("undo_last` 1 time(s)"), "{err:?}");
    // the batch itself landed
    assert!(rows(&mb).iter().any(|(_, c, _)| c == "logic_auto"));
}

#[tokio::test]
async fn an_old_bridge_gates_the_connection_methods_per_call() {
    let tree = Tree::new("old-bridge");
    let mb = hammer_mock(nodes()).await;
    let mut methods: Vec<String> = HAMMER_METHODS.iter().map(|m| m.to_string()).collect();
    methods.retain(|m| m != "entity.addConnections");
    mb.set_methods(methods);
    let server = build_server(&tree, &mb);
    inject_known(&server);

    // connections present: NotImplemented BEFORE any send
    let err = call(
        &server,
        "create_sound_entities",
        one(json!({"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, 0],
                   "connections": [{"output": "OnSoundFinished", "target": "taken",
                                    "input": "Toggle"}]})),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NotImplemented", "{err:?}");
    assert_eq!(err.type_, "CapabilityMissing");
    assert_eq!(err.code, Some(-32011));
    assert!(err.data["missing"]
        .as_array()
        .expect("missing")
        .iter()
        .any(|m| m == "entity.addConnections"));
    assert_eq!(mb.method_calls("edit.batch").len(), 0);
    assert!(
        mb.requests().iter().all(|r| r.method.starts_with("control.") || r.method == "doc.active"),
        "before any send (bar the addon pre-check read): {:?}", mb.requests()
    );

    // the same refusal for a schedule
    let err = call(
        &server,
        "create_sound_entities",
        one(json!({"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, 0],
                   "targetname": "snd_o", "schedule": {"startDelay": 1}})),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NotImplemented", "{err:?}");
    assert!(
        mb.requests().iter().all(|r| r.method.starts_with("control.") || r.method == "doc.active"),
        "still nothing sent (bar the addon pre-check reads): {:?}", mb.requests()
    );

    // a plain create still works without the connection methods
    let out = call(
        &server,
        "create_sound_entities",
        one(json!({"kind": "point", "soundEvent": "mcp.snd.point",
                   "origin": [0, 0, 64], "startOnSpawn": true})),
    )
    .await
    .expect("plain create works");
    assert_eq!(out["undoGroups"], 1);
    assert_eq!(mb.method_calls("edit.batch").len(), 1);
    assert!(rows(&mb).iter().any(|(_, c, _)| c == "snd_event_point"));
}

#[tokio::test]
async fn unknown_arguments_are_refused() {
    let tree = Tree::new("unknown-args");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);
    refused(
        &server,
        &mb,
        json!({"entities": [{"kind": "point", "origin": [0, 0, 0], "soundEvent": "x"}],
               "undoLabels": []}),
        "InvalidParams",
        "unknown argument",
    )
    .await;
    refused(
        &server,
        &mb,
        one(json!({"kind": "point", "origin": [0, 0, 0], "soundEvent": "x", "soundname": "y"})),
        "InvalidParams",
        "unknown member",
    )
    .await;
    // bounds
    refused(
        &server,
        &mb,
        json!({"entities": []}),
        "InvalidParams",
        "entities is empty",
    )
    .await;
    let too_many: Vec<Value> = (0..33)
        .map(|i| json!({"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, i]}))
        .collect();
    refused(
        &server,
        &mb,
        json!({"entities": too_many}),
        "InvalidParams",
        "at most 32 fit",
    )
    .await;
    // schedule with an unknown member
    refused(
        &server,
        &mb,
        one(json!({"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, 0],
                   "targetname": "t", "schedule": {"startDelay": 1, "every": 2}})),
        "InvalidParams",
        "unknown member \"every\"",
    )
    .await;
    // repeatInitialDelay without repeatEvery
    refused(
        &server,
        &mb,
        one(json!({"kind": "point", "soundEvent": "mcp.snd.point", "origin": [0, 0, 0],
                   "targetname": "t", "schedule": {"repeatInitialDelay": 1}})),
        "InvalidParams",
        "repeatInitialDelay needs repeatEvery",
    )
    .await;
}

#[tokio::test]
async fn the_orientedbox_and_the_default_box_and_legacy_soundscape_spellings() {
    let tree = Tree::new("spellings");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    let out = call(
        &server,
        "create_sound_entities",
        json!({"entities": [
            {"kind": "orientedbox", "soundEvent": "mcp.snd.point", "origin": [0, 0, 64]},
            {"kind": "soundscape", "legacySoundscape": "my_scape", "origin": [1, 2, 3]},
            {"kind": "sphere", "soundEvent": "mcp.snd.point", "origin": [4, 5, 6],
             "radius": 2.5},
            {"kind": "soundscape_proxy", "origin": [7, 8, 9], "radius": 128},
        ]}),
    )
    .await
    .expect("created");
    assert_eq!(out["count"], 4);
    let ops = wire(&mb, "edit.batch", 0)["ops"].as_array().expect("ops").clone();
    // the omitted box sends the FGD default explicitly, with the note
    assert_eq!(ops[0]["params"]["keyValues"]["box_mins"], "-64 -64 -64");
    assert_eq!(ops[0]["params"]["keyValues"]["box_maxs"], "64 64 64");
    let box_notes = notes(&out["entities"][0], "notes");
    assert!(box_notes.contains("FGD default box"), "{box_notes}");
    // the legacy route: soundscape + enablesoundevent "0", no soundevent key
    assert_eq!(ops[1]["params"]["keyValues"]["soundscape"], "my_scape");
    assert_eq!(ops[1]["params"]["keyValues"]["enablesoundevent"], "0");
    assert!(ops[1]["params"]["keyValues"].get("soundevent").is_none());
    assert_eq!(ops[1]["params"]["keyValues"].get("soundName"), None);
    // the sphere's float text keeps the fraction
    assert_eq!(ops[2]["params"]["keyValues"]["radius"], "2.5");
    // the legacy verdict is reported with its note
    let legacy_notes = notes(&out["entities"][1], "notes");
    assert!(legacy_notes.contains("legacySoundscape") && legacy_notes.contains("unvalidated"), "{legacy_notes}");
    assert_eq!(out["entities"][1]["soundEvent"], Value::Null);
    // the proxy is NOT the soundscape family (review H2): no sound keys and no
    // enablesoundevent — MainSoundscapeName would go through keyValues — but its own
    // radius(integer) is accepted (integer text, no fraction)
    let proxy_kv = &out["entities"][3]["keyValuesSent"];
    assert_eq!(proxy_kv.get("soundName"), None);
    assert_eq!(proxy_kv.get("soundevent"), None);
    assert_eq!(proxy_kv.get("enablesoundevent"), None);
    assert_eq!(proxy_kv.get("soundscape"), None);
    assert_eq!(proxy_kv["radius"], "128");
}

// -------------------------------------------------------------------------------------------
// FIX-E (review H2): the entity family derives from the FGD's resolved base chain, not from
// the class-name prefix — snd_soundscape / *_triggerable are the soundscape family
// (env_soundscape in the chain), snd_event_box_helper / snd_event_param / the proxy are
// "other" tags(Sound) classes.
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn snd_soundscape_is_the_soundscape_family_through_its_base_chain() {
    let tree = Tree::new("snd-scape");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    let out = call(
        &server,
        "create_sound_entities",
        one(json!({
            "class": "snd_soundscape", "soundEvent": "mcp.snd.scape",
            "origin": [0, 0, 0], "radius": 512,
        })),
    )
    .await
    .expect("created");
    let ops = wire(&mb, "edit.batch", 0)["ops"].as_array().expect("ops").clone();
    assert_eq!(ops[0]["params"]["class"], "snd_soundscape");
    // the soundscape family ALWAYS writes enablesoundevent "1" with the soundevent key
    assert_eq!(ops[0]["params"]["keyValues"]["soundevent"], "mcp.snd.scape");
    assert_eq!(ops[0]["params"]["keyValues"]["enablesoundevent"], "1");
    assert_eq!(ops[0]["params"]["keyValues"].get("soundName"), None);
    // the inherited radius(integer) accepts the integer text
    assert_eq!(ops[0]["params"]["keyValues"]["radius"], "512");
    assert_eq!(out["entities"][0]["soundEvent"]["verdict"], "known");
}

#[tokio::test]
async fn env_soundscape_triggerable_is_the_soundscape_family() {
    let tree = Tree::new("trig-scape");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    let out = call(
        &server,
        "create_sound_entities",
        one(json!({
            "class": "env_soundscape_triggerable", "soundEvent": "mcp.snd.scape",
            "origin": [0, 0, 0], "radius": -1,
        })),
    )
    .await
    .expect("created");
    let ops = wire(&mb, "edit.batch", 0)["ops"].as_array().expect("ops").clone();
    assert_eq!(ops[0]["params"]["keyValues"]["soundevent"], "mcp.snd.scape");
    assert_eq!(ops[0]["params"]["keyValues"]["enablesoundevent"], "1");
    assert_eq!(ops[0]["params"]["keyValues"]["radius"], "-1");
    assert_eq!(out["count"], 1);

    // and, like every class without a StartSound input in the FGD, it refuses a schedule
    let tree2 = Tree::new("trig-scape-sched");
    let mb2 = hammer_mock(nodes()).await;
    let server2 = build_server(&tree2, &mb2);
    inject_known(&server2);
    refused(
        &server2,
        &mb2,
        one(json!({
            "class": "env_soundscape_triggerable", "soundEvent": "mcp.snd.scape",
            "origin": [0, 0, 0], "schedule": {"startDelay": 1},
        })),
        "InvalidParams",
        "needs a StartSound input",
    )
    .await;
}

#[tokio::test]
async fn snd_event_box_helper_takes_a_box_without_any_sound_event() {
    let tree = Tree::new("box-helper");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    let out = call(
        &server,
        "create_sound_entities",
        one(json!({
            "class": "snd_event_box_helper",
            "origin": [0, 0, 0],
            "box": {"min": [-32, -32, -16], "max": [32, 32, 16]},
        })),
    )
    .await
    .expect("created");
    let ops = wire(&mb, "edit.batch", 0)["ops"].as_array().expect("ops").clone();
    assert_eq!(ops[0]["params"]["class"], "snd_event_box_helper");
    // no soundName key (the class declares no sound-typed key and needs no soundEvent),
    // no family keys, the box keys verbatim
    let sent = &out["entities"][0]["keyValuesSent"];
    assert_eq!(sent.get("soundName"), None);
    assert_eq!(sent.get("soundevent"), None);
    assert_eq!(sent.get("enablesoundevent"), None);
    assert_eq!(sent["box_mins"], "-32 -32 -16");
    assert_eq!(sent["box_maxs"], "32 32 16");
    assert_eq!(out["entities"][0]["soundEvent"], Value::Null);
}

#[tokio::test]
async fn snd_event_param_is_created_without_a_sound_event() {
    let tree = Tree::new("param");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    let out = call(
        &server,
        "create_sound_entities",
        one(json!({
            "class": "snd_event_param",
            "origin": [0, 0, 0],
            "keyValues": {"parameterName": "volume_atten", "floatValue": "0.5"},
        })),
    )
    .await
    .expect("created");
    let sent = &out["entities"][0]["keyValuesSent"];
    assert_eq!(sent.get("soundName"), None);
    assert_eq!(sent.get("soundevent"), None);
    assert_eq!(sent["parameterName"], "volume_atten");
    assert_eq!(out["count"], 1);
}

#[tokio::test]
async fn schedule_on_a_class_without_a_start_sound_input_is_refused() {
    let tree = Tree::new("no-start");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    // the opvar setter has no StartSound input either (and no schedule of any kind)
    refused(
        &server,
        &mb,
        one(json!({
            "class": "snd_event_param", "origin": [0, 0, 0],
            "schedule": {"startDelay": 1},
        })),
        "InvalidParams",
        "needs a StartSound input",
    )
    .await;
    // and the box helper (base Targetname only) equally
    refused(
        &server,
        &mb,
        one(json!({
            "class": "snd_event_box_helper", "origin": [0, 0, 0],
            "schedule": {"repeatEvery": 2},
        })),
        "InvalidParams",
        "needs a StartSound input",
    )
    .await;
}

/// NEGATIVE: a `soundEvent` on an "other" `tags( Sound )` class whose FGD declares **no**
/// sound-typed key is refused — writing the name nowhere would create an entity that plays
/// nothing and run no verdict, so the caller's intent would be silently lost. Nothing is sent
/// (not even `doc.active`); the same class *without* a `soundEvent` still creates.
#[tokio::test]
async fn a_sound_event_on_a_class_without_a_sound_key_is_refused() {
    let tree = Tree::new("no-key");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    for class in [
        "snd_event_param",     // the opvar setter
        "snd_event_box_helper", // the box helper (a box, no sound key)
        "snd_opvar_set_point",
        "snd_stack_save",
        "snd_soundscape_proxy", // a proxy, not the soundscape family
    ] {
        let err = call(
            &server,
            "create_sound_entities",
            one(json!({"class": class, "soundEvent": "mcp.snd.point", "origin": [0, 0, 0]})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{class}: {err:?}");
        assert_eq!(err.type_, "InvalidArguments", "{class}");
        assert!(err.retry_safe, "{class}: a local refusal: {err:?}");
        assert!(
            err.message.contains("declares no sound-typed key in the FGD"),
            "{class}: {err:?}"
        );
        assert!(err.message.contains(class), "{class}: names the class: {err:?}");
        assert!(
            err.message.contains("keyValues"),
            "{class}: points at the keyValues escape hatch: {err:?}"
        );
        assert_eq!(mb.method_calls("edit.batch").len(), 0, "{class}: nothing written");
        // the only bridge use is the addon pre-check read (the soundevent index follows the
        // active document's addon); no write, no connection method
        assert!(
            mb.requests().iter().all(|r| r.method.starts_with("control.") || r.method == "doc.active"),
            "{class}: nothing sent but the addon check read: {:?}", mb.requests()
        );
    }

    // the control: the same class with no `soundEvent` is created, as before (the class takes
    // no family keys and needs no sound event)
    let out = call(
        &server,
        "create_sound_entities",
        one(json!({"class": "snd_event_param", "origin": [0, 0, 64],
                   "keyValues": {"parameterName": "volume_atten"}})),
    )
    .await
    .expect("created");
    assert_eq!(out["entities"][0]["soundEvent"], Value::Null);
    assert_eq!(out["entities"][0]["keyValuesSent"]["parameterName"], "volume_atten");
    assert_eq!(mb.method_calls("edit.batch").len(), 1);
}

/// The FGD-free tree cannot derive the family from a base chain, so the real `core/base.fgd`'s
/// class list stands in: `snd_soundscape`, `env_soundscape_triggerable` and
/// `snd_soundscape_triggerable` are the **soundscape** family (they write `soundevent` +
/// `enablesoundevent "1"`), while `snd_event_param` — which shares the `snd_event_` prefix but
/// declares no sound key — stays "other" (a `soundEvent` on it is refused, and without one it
/// creates with no sound keys at all).
#[tokio::test]
async fn the_fgd_free_family_list_covers_the_real_class_names() {
    let tree = Tree::without_fgd("nofgd-family");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    inject_known(&server);

    for class in [
        "snd_soundscape",
        "env_soundscape_triggerable",
        "snd_soundscape_triggerable",
    ] {
        let out = call(
            &server,
            "create_sound_entities",
            one(json!({"class": class, "soundEvent": "mcp.snd.scape", "origin": [0, 0, 64],
                       "radius": 512})),
        )
        .await
        .expect("the soundscape family writes the event key");
        let sent = &out["entities"][0]["keyValuesSent"];
        assert_eq!(sent["soundevent"], "mcp.snd.scape", "{class}");
        assert_eq!(sent["enablesoundevent"], "1", "{class}");
        assert_eq!(sent.get("soundName"), None, "{class}");
        assert_eq!(sent["radius"], "512", "{class}: the integer radius text");
        assert_eq!(out["entities"][0]["soundEvent"]["verdict"], "known", "{class}");
    }

    let err = call(
        &server,
        "create_sound_entities",
        one(json!({"class": "snd_event_param", "soundEvent": "mcp.snd.point",
                   "origin": [0, 0, 0]})),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(err.message.contains("declares no sound-typed key in the FGD"), "{err:?}");

    let out = call(
        &server,
        "create_sound_entities",
        one(json!({"class": "snd_event_param", "origin": [0, 0, 64]})),
    )
    .await
    .expect("created");
    let sent = &out["entities"][0]["keyValuesSent"];
    assert_eq!(sent.get("soundName"), None);
    assert_eq!(sent.get("soundevent"), None);
    assert_eq!(sent.get("enablesoundevent"), None);

    // without an FGD there is no key list at all, so the refusal covers classes the FGD would
    // name a sound key for (`ambient_generic`'s `message`) too — nothing is sent
    let err = call(
        &server,
        "create_sound_entities",
        one(json!({"class": "ambient_generic", "soundEvent": "mcp.snd.point",
                   "origin": [0, 0, 0]})),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(err.message.contains("declares no sound-typed key in the FGD"), "{err:?}");
    assert_eq!(
        mb.method_calls("edit.batch").len(),
        4,
        "the three family creates and the soundEvent-less one, and nothing from the refusal"
    );

    // the escape hatch the refusal points at, exactly as it works with an FGD: the key through
    // `keyValues` (`message` is not one of this tool's reserved spellings)
    let out = call(
        &server,
        "create_sound_entities",
        one(json!({"class": "snd_event_param", "origin": [0, 0, 64],
                   "keyValues": {"message": "mcp.snd.point"}})),
    )
    .await
    .expect("created through keyValues");
    assert_eq!(out["entities"][0]["keyValuesSent"]["message"], "mcp.snd.point");
}

#[tokio::test]
async fn legacy_soundscape_is_only_valid_for_the_soundscape_family() {
    let tree = Tree::new("legacy-no");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);

    refused(
        &server,
        &mb,
        one(json!({
            "class": "snd_event_param", "legacySoundscape": "my_scape", "origin": [0, 0, 0],
        })),
        "InvalidParams",
        "only valid for the soundscape family",
    )
    .await;
    refused(
        &server,
        &mb,
        one(json!({
            "kind": "point", "legacySoundscape": "my_scape", "origin": [0, 0, 0],
        })),
        "InvalidParams",
        "only valid for the soundscape family",
    )
    .await;
}

#[tokio::test]
async fn a_stale_soundevent_creates_with_a_warning_and_its_own_verdict() {
    let tree = Tree::new("stale");
    let mb = hammer_mock(nodes()).await;
    let server = build_server(&tree, &mb);
    // the shared state seam, a shape the index can only produce when the addon's `_c` is the
    // name's sole holder while its `.vsndevts` source dropped it
    inject_verdicts(
        &server,
        &[(
            "mcp.snd.old",
            SoundNameVerdict::Stale {
                file: "content/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts".into(),
            },
        )],
    );

    let out = call(
        &server,
        "create_sound_entities",
        json!({"entities": [
            {"kind": "point", "soundEvent": "mcp.snd.old", "origin": [0, 0, 64]},
        ]}),
    )
    .await
    .expect("created: stale warns, never refuses");
    let sound = &out["entities"][0]["soundEvent"];
    assert_eq!(sound["verdict"], "stale", "{sound}");
    assert!(sound["file"].as_str().unwrap().ends_with(".vsndevts"), "{sound}");
    assert!(out.to_string().contains("removed or renamed"), "{out}");
    assert_eq!(mb.method_calls("edit.batch").len(), 1, "created anyway");
}
