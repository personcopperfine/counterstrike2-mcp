//! The `player_model` tool (`src/tools/player_model.rs`, I7): the generated cs_script, the same
//! build/compile pipeline as `cs_script` (fake npm/node, mock job bridge), and the sync of the open
//! map (a `point_script` + hidden precache `prop_dynamic`s) against the mock bridge's Hammer world.
#![allow(clippy::result_large_err)]

use clap::Parser;
use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::jobs::MockJobOutcome;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
use cs2_mcp_common::tools::{CallFailure, ToolError, ToolOutput};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);
/// The fake npm/node selection is process-global (env vars).
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";
const GAME_D_TS: &str = "declare module \"cs_script/point_script\" {\n    export const Instance: { Msg(text: string): void };\n}\n";
const HERO: &str = "models/hero/hero.vmdl";
const VILLAIN: &str = "models/villain/villain.vmdl";
const AGENT: &str = "agents/models/tm_test/tm_test.vmdl";

/// The classes this suite needs, with the keyvalues the tool sends declared (the real
/// `core/base.fgd` / `csgo.fgd` declare them: rendermode, solid, disableshadows, cs_script).
const FGD: &str = "\
@PointClass base(Targetname) = point_script
[
	cs_script(string) : \"cs_script\"
]
@PointClass base(Targetname) tags( PropDynamic ) = prop_dynamic : \"A dynamic prop.\"
[
	model(studio) : \"Model\" : \"\"
	solid(integer) : \"Collisions\" : 6
	rendermode(string) : \"Render Mode\" : \"kRenderNormal\"
	disableshadows(integer) : \"Cast Shadows\" : 0
]
";

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "player-model-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let app = root.join("workshop/run/app");
        std::fs::create_dir_all(app.join("content/csgo_addons/mcp_test/maps")).expect("tmp tree");
        // the model catalog: both game VPKs, a compiled addon model, an uncompiled addon source
        common::write_vpk(
            &app.join("game/csgo"),
            "pak01",
            2,
            &[common::BuildFile::loaded(&format!("{}_c", AGENT), b"vmdl", b"", false)],
        );
        common::write_vpk(&app.join("game/core"), "pak01", 1, &[common::BuildFile::loaded("models/dev/axis.vmdl_c", b"vmdl", b"", false)]);
        for model in [HERO, VILLAIN] {
            let compiled = app.join("game/csgo_addons/mcp_test").join(format!("{model}_c"));
            std::fs::create_dir_all(compiled.parent().unwrap()).unwrap();
            std::fs::write(compiled, b"vmdl").unwrap();
        }
        let raw = app.join("content/csgo_addons/mcp_test/models/raw/raw.vmdl");
        std::fs::create_dir_all(raw.parent().unwrap()).unwrap();
        std::fs::write(raw, "<!-- dmx -->").unwrap();
        std::fs::create_dir_all(app.join("game/csgo")).unwrap();
        std::fs::write(app.join("game/csgo/csgo.fgd"), FGD).unwrap();
        Tree { root, app }
    }

    fn addon(&self) -> PathBuf {
        self.app.join("content/csgo_addons/mcp_test")
    }

    fn script_src(&self) -> PathBuf {
        self.addon().join("cs_script/src/player_model/index.ts")
    }

    fn vjs_c(&self) -> PathBuf {
        self.app.join("game/csgo_addons/mcp_test/maps/scripts/player_model.vjs_c")
    }

    fn config(&self, discovery: Option<&Path>) -> CommonConfig {
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: CommonConfig,
        }
        let mut args: Vec<String> = vec![
            "--deadline-ms".into(),
            "5000".into(),
            "--tool-budget-ms".into(),
            "60000".into(),
            "--project-root".into(),
            self.root.display().to_string(),
            "--path-map".into(),
            format!("z:\\app={}", self.app.display()),
        ];
        if let Some(discovery) = discovery {
            args.extend(["--discovery".into(), discovery.display().to_string()]);
        }
        let given: Vec<&str> = args.iter().map(String::as_str).collect();
        let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
        W::parse_from(argv).c
    }

    /// Fake npm and node (the `tests/cs_script.rs` pattern): install makes node_modules, tsc passes,
    /// rollup writes a JS file.
    fn fakes(&self) {
        let bin = self.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let root = self.root.display().to_string();
        let npm = format!(
            r#"#!/bin/bash
case "$1" in
  install)
    mkdir -p node_modules/.bin node_modules/@s2ze/types/build/autoimport
    for p in debug math prototype-extend scheduler tsconfig types; do
      mkdir -p node_modules/@s2ze/$p; echo '{{"version":"9.9.9"}}' > node_modules/@s2ze/$p/package.json
    done
    cp '{root}/npm-types.d.ts' node_modules/@s2ze/types/build/autoimport/index.d.ts
    echo fake > node_modules/.bin/tsc; echo fake > node_modules/.bin/rollup
    echo "added 7 packages"
    ;;
  view) echo 9.9.9 ;;
esac
"#
        );
        let node = format!(
            r#"#!/bin/bash
case "$1" in
  --input-type=*) echo '{{}}' ;;
  */tsc) exit 0 ;;
  */rollup)
    out=$(sed -n "s/^const OUTPUT_DIR = \"\(.*\)\";/\1/p" rollup.config.js)
    mkdir -p "$out"
    echo "// GENERATED" > "$out/$CS_SCRIPT_ONLY.js"
    cat '{root}/rollup.js' >> "$out/$CS_SCRIPT_ONLY.js"
    ;;
esac
"#
        );
        for (name, text) in [("npm", npm), ("node", node)] {
            let path = bin.join(name);
            std::fs::write(&path, text).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(self.root.join("npm-types.d.ts"), GAME_D_TS).unwrap();
        std::fs::write(self.root.join("rollup.js"), "export {};\n").unwrap();
        std::env::set_var("CS_SCRIPT_NPM", bin.join("npm"));
        std::env::set_var("CS_SCRIPT_NODE", bin.join("node"));
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn kv(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

async fn bridge(tree: &Tree, nodes: Vec<MockNode>) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(nodes)
        .job_script(MockJobOutcome::Ok, 0)
        .job_writes([tree.vjs_c()])
        .start()
        .await
        .unwrap()
}

fn server(tree: &Tree, mb: &MockBridge) -> McpServer<HammerState> {
    hammer_mcp::build_server(&tree.config(Some(&mb.discovery_path))).expect("server")
}

async fn tool(server: &McpServer<HammerState>, name: &str, args: Value) -> Result<ToolOutput, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

async fn pm(server: &McpServer<HammerState>, args: Value) -> Result<ToolOutput, ToolError> {
    tool(server, "player_model", args).await
}

async fn init(server: &McpServer<HammerState>) {
    let out = tool(server, "cs_script", json!({"action": "init"})).await.expect("init");
    assert_eq!(out.json["ok"], json!(true), "{}", out.json);
}

fn next_tools(out: &ToolOutput) -> Vec<String> {
    out.next.iter().flatten().map(|s| s.tool.clone()).collect()
}

/// The entities of one class in the mock world, as (nodeID, keyvalues).
fn entities(mb: &MockBridge, class: &str) -> Vec<MockNode> {
    mb.hammer_nodes().into_iter().filter(|n| n.class == "entity" && n.classname == class).collect()
}

// -------------------------------------------------------------------------------------------

#[test]
fn the_generated_source_is_deterministic_and_reads_back() {
    use hammer_mcp::tools::player_model::{parse_source, render_source, Rules};
    let rules = Rules { any: Some(HERO.into()), ct: None, t: Some(VILLAIN.into()), bots: true };
    let source = render_source(&rules);
    assert_eq!(source, render_source(&rules));
    assert_eq!(parse_source(&source), Some(rules.clone()));
    // the fixed template: the game API the spike verified, and nothing model-specific outside the const
    for line in [
        "import { Instance } from \"cs_script/point_script\";",
        "Instance.OnPlayerReset(({ player }) => {",
        "Instance.QueueAfterThinks(() => {",
        "player.SetModel(model);",
        "if (team === 3) return RULES.ct !== null ? RULES.ct : RULES.any;",
        "if (team === 2) return RULES.t !== null ? RULES.t : RULES.any;",
        "Instance.Msg(\"[player_model] loaded\");",
    ] {
        assert!(source.contains(line), "{line}\n{source}");
    }
    assert_eq!(source.matches(HERO).count(), 1);
    // two rule sets differ only in the const line
    let other = render_source(&Rules { bots: false, ..rules });
    let differing: Vec<_> = source.lines().zip(other.lines()).filter(|(a, b)| a != b).collect();
    assert_eq!(differing.len(), 1, "{differing:?}");
    assert!(differing[0].0.starts_with("const RULES: Rules = "));
}

#[tokio::test]
async fn set_writes_builds_compiles_and_creates_the_map_entities() {
    let _env = ENV.lock().await;
    let tree = Tree::new("set");
    tree.fakes();
    let mb = bridge(&tree, vec![]).await;
    let server = server(&tree, &mb);
    init(&server).await;
    // 'player_model' was not a target yet: the tool adds it
    let out = pm(&server, json!({"action": "set", "model": "Models\\Hero\\Hero.vmdl_c"})).await.expect("set");
    let j = &out.json;
    assert_eq!(j["ok"], json!(true), "{j}");
    assert_eq!(j["rules"], json!({"any": HERO, "ct": null, "t": null, "bots": true}));
    assert_eq!(j["add"]["ok"], json!(true));
    assert_eq!(j["write"]["changed"], json!(true));
    assert_eq!(j["build"]["ok"], json!(true), "{j}");
    assert_eq!(j["compile"]["ok"], json!(true), "{j}");
    assert_eq!(j["saved"], json!(false));
    assert!(j["verify"].as_str().unwrap().contains("launch_session"));
    assert!(j["verify"].as_str().unwrap().contains("run_map is NOT a valid check"));
    assert_eq!(next_tools(&out), ["save_map", "compile_map"]);
    // the files
    let source = std::fs::read_to_string(tree.script_src()).unwrap();
    assert!(source.contains("player.SetModel(model)") && source.contains(HERO));
    assert!(tree.addon().join("maps/scripts/player_model.js").is_file());
    let starts: Vec<Value> = mb.method_calls("job.start").into_iter().map(|r| r.params).collect();
    assert_eq!(starts[0]["input"], json!("csgo_addons/mcp_test/maps/scripts/player_model.js"));
    // the map: one point_script and one hidden precache prop at the default origin
    let scripts = entities(&mb, "point_script");
    assert_eq!(scripts.len(), 1);
    assert_eq!(scripts[0].keyvalues["targetname"], "player_model_script");
    assert_eq!(scripts[0].keyvalues["cs_script"], "maps/scripts/player_model.vjs");
    assert_eq!(scripts[0].origin, [0.0, 0.0, 0.0]);
    let props = entities(&mb, "prop_dynamic");
    assert_eq!(props.len(), 1);
    let p = &props[0].keyvalues;
    assert_eq!(p["targetname"], "player_model_precache_0");
    assert_eq!(p["model"], HERO);
    assert_eq!((p["solid"].as_str(), p["rendermode"].as_str(), p["disableshadows"].as_str()), ("0", "kRenderNone", "1"));
    assert_eq!(j["map"]["created"], json!(["player_model_script", "player_model_precache_0"]));

    // the same call again changes nothing
    let before = mb.hammer_nodes().len();
    let again = pm(&server, json!({"action": "set", "model": HERO})).await.unwrap();
    assert_eq!(again.json["write"]["changed"], json!(false));
    assert_eq!(again.json["map"]["created"], json!([]));
    assert_eq!(again.json["map"]["fixed"], json!([]));
    assert_eq!(again.json["map"]["deleted"], json!([]));
    assert_eq!(mb.hammer_nodes().len(), before);
}

#[tokio::test]
async fn agent_models_are_accepted_and_slots_share_or_split_precache_props() {
    let _env = ENV.lock().await;
    let tree = Tree::new("slots");
    tree.fakes();
    let mb = bridge(&tree, vec![]).await;
    let server = server(&tree, &mb);
    init(&server).await;
    pm(&server, json!({"action": "set", "model": AGENT})).await.expect("an agents/models/** model");
    assert_eq!(entities(&mb, "prop_dynamic")[0].keyvalues["model"], AGENT);
    // the same model in a second slot: still one prop
    pm(&server, json!({"action": "set", "model": AGENT, "team": "ct", "bots": false})).await.unwrap();
    assert_eq!(entities(&mb, "prop_dynamic").len(), 1);
    // another model for t: a second prop; the stored rules and the bots flag read back
    let out = pm(&server, json!({"action": "set", "model": HERO, "team": "t"})).await.unwrap();
    assert_eq!(out.json["rules"], json!({"any": AGENT, "ct": AGENT, "t": HERO, "bots": false}));
    let mut names: Vec<String> = entities(&mb, "prop_dynamic").iter().map(|n| n.keyvalues["targetname"].clone()).collect();
    names.sort();
    assert_eq!(names, ["player_model_precache_0", "player_model_precache_1"]);
    let source = std::fs::read_to_string(tree.script_src()).unwrap();
    assert!(source.contains("\"bots\":false"));
}

#[tokio::test]
async fn the_sync_fixes_reuses_the_origin_and_touches_only_its_own_entities() {
    let _env = ENV.lock().await;
    let tree = Tree::new("sync");
    tree.fakes();
    let nodes = vec![
        MockNode::world(),
        // ours, wrong script keyvalue, at a chosen origin
        MockNode::entity(10, "point_script", [5.0, 6.0, 7.0], kv(&[("targetname", "player_model_script"), ("cs_script", "maps/scripts/wrong.vjs")])),
        // a duplicate script of ours: removed
        MockNode::entity(11, "point_script", [0.0; 3], kv(&[("targetname", "player_model_script"), ("cs_script", "maps/scripts/player_model.vjs")])),
        // someone else's point_script and prop: never touched
        MockNode::entity(12, "point_script", [0.0; 3], kv(&[("targetname", "other_script"), ("cs_script", "maps/scripts/main.vjs")])),
        MockNode::entity(13, "prop_dynamic", [0.0; 3], kv(&[("targetname", "keepme"), ("model", HERO)])),
        // ours, wrong model and not hidden
        MockNode::entity(14, "prop_dynamic", [1.0, 1.0, 1.0], kv(&[("targetname", "player_model_precache_0"), ("model", VILLAIN)])),
        // ours, no longer needed
        MockNode::entity(15, "prop_dynamic", [0.0; 3], kv(&[("targetname", "player_model_precache_7"), ("model", HERO)])),
    ];
    let mb = bridge(&tree, nodes).await;
    let server = server(&tree, &mb);
    init(&server).await;
    let out = pm(&server, json!({"action": "set", "model": HERO})).await.expect("set");
    assert_eq!(out.json["ok"], json!(true), "{}", out.json);
    let all = mb.hammer_nodes();
    let find = |id: i64| all.iter().find(|n| n.node_id == id);
    assert_eq!(find(10).unwrap().keyvalues["cs_script"], "maps/scripts/player_model.vjs");
    assert_eq!(find(10).unwrap().origin, [5.0, 6.0, 7.0], "the script keeps its origin");
    assert!(find(11).is_none(), "the duplicate is gone");
    assert!(find(12).is_some() && find(13).is_some(), "foreign entities stay");
    let fixed = find(14).unwrap();
    assert_eq!(fixed.keyvalues["model"], HERO);
    assert_eq!(fixed.keyvalues["rendermode"], "kRenderNone");
    assert_eq!(fixed.origin, [1.0, 1.0, 1.0], "an existing prop is not moved");
    assert!(find(15).is_none(), "an unused precache prop is removed");
    assert_eq!(entities(&mb, "prop_dynamic").len(), 2);
}

#[tokio::test]
async fn model_changes_and_clear_keep_the_props_in_step() {
    let _env = ENV.lock().await;
    let tree = Tree::new("clear");
    tree.fakes();
    let mb = bridge(&tree, vec![MockNode::world(), MockNode::entity(13, "prop_dynamic", [0.0; 3], kv(&[("targetname", "keepme"), ("model", HERO)]))]).await;
    let server = server(&tree, &mb);
    init(&server).await;
    pm(&server, json!({"action": "set", "model": HERO})).await.unwrap();
    // a new model for the same slot rewrites the prop instead of adding one
    pm(&server, json!({"action": "set", "model": VILLAIN})).await.unwrap();
    let props: Vec<_> = entities(&mb, "prop_dynamic").into_iter().filter(|n| n.keyvalues["targetname"].starts_with("player_model_")).collect();
    assert_eq!(props.len(), 1);
    assert_eq!(props[0].keyvalues["model"], VILLAIN);
    pm(&server, json!({"action": "set", "model": HERO, "team": "ct"})).await.unwrap();
    assert_eq!(entities(&mb, "prop_dynamic").len(), 3);
    // clearing a slot drops the prop only when no rule needs the model any more
    let out = pm(&server, json!({"action": "clear", "team": "ct"})).await.unwrap();
    assert_eq!(out.json["rules"], json!({"any": VILLAIN, "ct": null, "t": null, "bots": true}));
    assert_eq!(entities(&mb, "prop_dynamic").len(), 2);
    // clearing everything removes the script and the props, and only those
    let out = pm(&server, json!({"action": "clear"})).await.unwrap();
    assert_eq!(out.json["ok"], json!(true), "{}", out.json);
    assert!(entities(&mb, "point_script").is_empty());
    let left = entities(&mb, "prop_dynamic");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].keyvalues["targetname"], "keepme");
    let source = std::fs::read_to_string(tree.script_src()).unwrap();
    assert!(source.contains("{\"any\":null,\"bots\":true,\"ct\":null,\"t\":null}"), "{source}");
}

#[tokio::test]
async fn status_reads_the_rules_back_and_sees_a_missing_entity() {
    let _env = ENV.lock().await;
    let tree = Tree::new("status");
    tree.fakes();
    let mb = bridge(&tree, vec![]).await;
    let server = server(&tree, &mb);
    // no project yet
    let out = pm(&server, json!({"action": "status"})).await.unwrap();
    assert_eq!(out.json["project"]["present"], json!(false));
    assert_eq!(next_tools(&out), ["cs_script"]);
    init(&server).await;
    let out = pm(&server, json!({"action": "status"})).await.unwrap();
    assert_eq!(out.json["script"]["present"], json!(false));
    pm(&server, json!({"action": "set", "model": HERO, "team": "ct"})).await.unwrap();
    let out = pm(&server, json!({"action": "status"})).await.unwrap();
    let j = &out.json;
    assert_eq!(j["rules"], json!({"any": null, "ct": HERO, "t": null, "bots": true}));
    assert_eq!(j["script"]["built"], json!("fresh"));
    assert_eq!(j["script"]["compiled"], json!("fresh"));
    assert_eq!(j["map"]["inSync"], json!(true), "{j}");
    assert_eq!(j["map"]["pointScripts"][0]["cs_script"], json!("maps/scripts/player_model.vjs"));
    assert_eq!(j["map"]["precacheProps"][0]["model"], json!(HERO));
    // somebody deletes the prop: status says so and points at a resync
    let prop = entities(&mb, "prop_dynamic")[0].node_id;
    tool(&server, "delete_nodes", json!({"nodeIDs": [prop]})).await.unwrap();
    let out = pm(&server, json!({"action": "status"})).await.unwrap();
    assert_eq!(out.json["map"]["inSync"], json!(false));
    assert_eq!(out.json["map"]["missingPrecache"], json!(["player_model_precache_0"]));
    assert_eq!(next_tools(&out), ["player_model"]);
    assert_eq!(out.next.as_ref().unwrap()[0].args, Some(json!({"action": "set", "model": HERO, "team": "ct"})));
    // and the repair puts it back
    pm(&server, json!({"action": "set", "model": HERO, "team": "ct"})).await.unwrap();
    assert_eq!(pm(&server, json!({"action": "status"})).await.unwrap().json["map"]["inSync"], json!(true));
}

#[tokio::test]
async fn refusals_are_typed_and_change_nothing() {
    let _env = ENV.lock().await;
    let tree = Tree::new("refuse");
    tree.fakes();
    let mb = bridge(&tree, vec![]).await;
    let server = server(&tree, &mb);
    // no cs_script project: refused with the way forward, no npm started
    let e = pm(&server, json!({"action": "set", "model": HERO})).await.unwrap_err();
    assert_eq!(e.name, "NoProject", "{e:?}");
    assert!(e.to_json().to_string().contains("init"));
    assert!(!tree.addon().join("cs_script").exists());
    init(&server).await;
    // model problems
    let e = pm(&server, json!({"action": "set", "model": "models/raw/raw.vmdl"})).await.unwrap_err();
    assert_eq!(e.name, "ModelNotCompiled", "{e:?}");
    assert!(e.to_json().to_string().contains("model_compile"));
    let e = pm(&server, json!({"action": "set", "model": "models/nope/nope.vmdl"})).await.unwrap_err();
    assert_eq!(e.name, "ModelNotFound", "{e:?}");
    assert!(e.to_json().to_string().contains("model_import"));
    for bad in ["../x.vmdl", "C:/x.vmdl", "props/x.vmdl", "models/x.txt", "", "models/a\"b/x.vmdl"] {
        let e = pm(&server, json!({"action": "set", "model": bad})).await.unwrap_err();
        assert_eq!(e.name, "InvalidParams", "{bad:?}: {e:?}");
    }
    // arguments
    for args in [
        json!({"action": "set"}),
        json!({"action": "set", "model": HERO, "team": "spec"}),
        json!({"action": "set", "model": HERO, "bots": "yes"}),
        json!({"action": "set", "model": HERO, "extra": 1}),
        json!({"action": "clear", "model": HERO}),
        json!({"action": "status", "team": "ct"}),
        json!({"action": "wear"}),
    ] {
        let e = pm(&server, args.clone()).await.unwrap_err();
        assert_eq!(e.name, "InvalidParams", "{args}: {e:?}");
    }
    assert!(!tree.script_src().exists(), "refusals write nothing");
    assert!(mb.hammer_nodes().iter().all(|n| n.class != "entity"));
    // a hand-written player_model script is never overwritten
    tool(&server, "cs_script", json!({"action": "add", "name": "player_model"})).await.unwrap();
    tool(&server, "cs_script", json!({"action": "write", "name": "player_model", "content": "// mine\n"})).await.unwrap();
    let e = pm(&server, json!({"action": "set", "model": HERO})).await.unwrap_err();
    assert_eq!(e.name, "ScriptNotOurs", "{e:?}");
    assert_eq!(std::fs::read_to_string(tree.script_src()).unwrap(), "// mine\n");
}

#[tokio::test]
async fn no_open_map_leaves_the_script_built_and_says_so() {
    let _env = ENV.lock().await;
    let tree = Tree::new("nomap");
    tree.fakes();
    let mb = MockBridge::builder().doc(None).hammer_nodes(vec![MockNode::world()]).responder_value("doc.active", json!({"doc": null, "reason": "no_session"})).job_script(MockJobOutcome::Ok, 0).job_writes([tree.vjs_c()]).start().await.unwrap();
    let server = server(&tree, &mb);
    init(&server).await;
    let out = pm(&server, json!({"action": "set", "model": HERO})).await.expect("the script half still works");
    assert_eq!(out.json["ok"], json!(false));
    assert_eq!(out.json["stage"], json!("map"));
    assert_eq!(out.json["map"]["synced"], json!(false));
    assert_eq!(out.json["compile"]["ok"], json!(true));
    assert_eq!(next_tools(&out), ["open_map", "player_model"]);
    assert!(tree.script_src().is_file());
}

// -------------------------------------------------------------------------------------------
// review fixes
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_failure_part_way_reports_what_was_done_and_does_not_blame_the_document() {
    let _env = ENV.lock().await;
    let tree = Tree::new("partial");
    tree.fakes();
    // our prop exists with a wrong model: the sync creates the script, then fails fixing the prop
    let nodes = vec![MockNode::world(), MockNode::entity(14, "prop_dynamic", [0.0; 3], kv(&[("targetname", "player_model_precache_0"), ("model", VILLAIN)]))];
    let mb = bridge(&tree, nodes).await;
    let server = server(&tree, &mb);
    init(&server).await;
    mb.add_error("entity.setKeyValues", -32000, "injected failure", None);
    let out = pm(&server, json!({"action": "set", "model": HERO})).await.expect("a reply, not an error");
    let j = &out.json;
    assert_eq!(j["ok"], json!(false));
    assert_eq!(j["stage"], json!("map"));
    assert_eq!(j["map"]["synced"], json!(false));
    assert_eq!(j["map"]["created"], json!(["player_model_script"]));
    assert_eq!(j["map"]["createdNodes"].as_array().unwrap().len(), 1);
    assert!(j["map"]["note"].as_str().unwrap().contains("part way"), "{j}");
    assert_eq!(next_tools(&out), ["player_model"], "the map is open: no open_map");
    assert_eq!(entities(&mb, "point_script").len(), 1, "the script it created stays");
    // the retry converges once the bridge works: no duplicate script
    mb.with_state(|s| {
        s.errors.remove("entity.setKeyValues");
    });
    let out = pm(&server, json!({"action": "set", "model": HERO})).await.unwrap();
    assert_eq!(out.json["ok"], json!(true), "{}", out.json);
    assert_eq!(entities(&mb, "point_script").len(), 1);
    assert_eq!(entities(&mb, "prop_dynamic")[0].keyvalues["model"], HERO);
}

#[tokio::test]
async fn clear_removes_the_entities_even_when_the_toolchain_fails() {
    let _env = ENV.lock().await;
    let tree = Tree::new("clearfail");
    tree.fakes();
    let mb = bridge(&tree, vec![]).await;
    let server = server(&tree, &mb);
    init(&server).await;
    pm(&server, json!({"action": "set", "model": HERO})).await.unwrap();
    assert_eq!(entities(&mb, "point_script").len(), 1);
    // the toolchain breaks: every node/tsc call fails
    let broken = tree.root.join("bin/node_broken");
    std::fs::write(&broken, "#!/bin/bash\nexit 1\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::env::set_var("CS_SCRIPT_NODE", &broken);
    let out = pm(&server, json!({"action": "clear"})).await.expect("clear still answers");
    assert_eq!(out.json["ok"], json!(true), "{}", out.json);
    assert_eq!(out.json["scriptSteps"]["ok"], json!(false), "the script half is reported, not hidden");
    assert!(entities(&mb, "point_script").is_empty() && entities(&mb, "prop_dynamic").is_empty());
    assert!(out.json["note"].as_str().unwrap().contains("removed"));
    assert_eq!(next_tools(&out), ["save_map", "compile_map"]);
    assert!(out.json.get("verify").is_none());
}

#[tokio::test]
async fn our_entities_are_found_by_exact_targetname_never_by_a_class_scan() {
    let _env = ENV.lock().await;
    let tree = Tree::new("exact");
    tree.fakes();
    let mb = bridge(&tree, vec![]).await;
    let server = server(&tree, &mb);
    init(&server).await;
    pm(&server, json!({"action": "set", "model": HERO})).await.unwrap();
    pm(&server, json!({"action": "status"})).await.unwrap();
    let finds = mb.method_calls("entity.find");
    assert!(!finds.is_empty());
    for f in &finds {
        let name = f.params["targetname"].as_str().unwrap_or_else(|| panic!("a find without targetname: {}", f.params));
        assert!(name == "player_model_script" || name.starts_with("player_model_precache_"), "{name}");
    }
}

#[tokio::test]
async fn a_damaged_rules_const_is_reported_by_status_and_rewritten_by_set() {
    let _env = ENV.lock().await;
    let tree = Tree::new("damaged");
    tree.fakes();
    let mb = bridge(&tree, vec![]).await;
    let server = server(&tree, &mb);
    init(&server).await;
    pm(&server, json!({"action": "set", "model": HERO})).await.unwrap();
    let source = std::fs::read_to_string(tree.script_src()).unwrap();
    std::fs::write(tree.script_src(), source.replace("\"bots\":true", "\"bots\":")).unwrap();
    let e = pm(&server, json!({"action": "status"})).await.unwrap_err();
    assert_eq!(e.name, "ScriptDamaged", "{e:?}");
    assert!(e.message.contains("set") && e.message.contains("clear"));
    let out = pm(&server, json!({"action": "set", "model": VILLAIN, "team": "t"})).await.expect("marked file is ours to rewrite");
    assert_eq!(out.json["ok"], json!(true), "{}", out.json);
    assert!(out.json["warning"].as_str().unwrap().contains("damaged"));
    assert_eq!(out.json["rules"], json!({"any": null, "ct": null, "t": VILLAIN, "bots": true}));
    assert_eq!(pm(&server, json!({"action": "status"})).await.unwrap().json["rules"]["t"], json!(VILLAIN));
}

#[tokio::test]
async fn a_marked_source_without_its_rollup_target_gets_only_the_target_back() {
    let _env = ENV.lock().await;
    let tree = Tree::new("target");
    tree.fakes();
    let mb = bridge(&tree, vec![]).await;
    let server = server(&tree, &mb);
    init(&server).await;
    pm(&server, json!({"action": "set", "model": HERO})).await.unwrap();
    let rollup_path = tree.addon().join("cs_script/rollup.config.js");
    let rollup = std::fs::read_to_string(&rollup_path).unwrap();
    let stripped: String = rollup.lines().filter(|l| !l.contains("name: 'player_model'")).map(|l| format!("{l}\n")).collect();
    assert_ne!(stripped, rollup);
    std::fs::write(&rollup_path, stripped).unwrap();
    let source = std::fs::read_to_string(tree.script_src()).unwrap();
    let out = pm(&server, json!({"action": "set", "model": HERO})).await.expect("recovers");
    assert_eq!(out.json["ok"], json!(true), "{}", out.json);
    assert_eq!(out.json["add"]["written"], json!(["rollup.config.js"]));
    assert!(std::fs::read_to_string(&rollup_path).unwrap().contains("name: 'player_model'"));
    assert_eq!(std::fs::read_to_string(tree.script_src()).unwrap(), source);
    // a folder with a file that is not ours keeps the refusal
    std::fs::write(&rollup_path, rollup.lines().filter(|l| !l.contains("name: 'player_model'")).map(|l| format!("{l}\n")).collect::<String>()).unwrap();
    std::fs::write(tree.script_src(), "// mine\n").unwrap();
    let e = pm(&server, json!({"action": "set", "model": HERO})).await.unwrap_err();
    assert_eq!(e.name, "ScriptNotOurs");
}

#[tokio::test]
async fn clear_without_a_script_is_honest_about_what_it_did() {
    let _env = ENV.lock().await;
    let tree = Tree::new("honest");
    tree.fakes();
    // our entities in the map but no script file: status says clear
    let nodes = vec![
        MockNode::world(),
        MockNode::entity(10, "point_script", [0.0; 3], kv(&[("targetname", "player_model_script"), ("cs_script", "maps/scripts/player_model.vjs")])),
    ];
    let mb = bridge(&tree, nodes).await;
    let server = server(&tree, &mb);
    init(&server).await;
    let out = pm(&server, json!({"action": "status"})).await.unwrap();
    assert_eq!(out.next.as_ref().unwrap()[0].args, Some(json!({"action": "clear"})));
    let out = pm(&server, json!({"action": "clear"})).await.unwrap();
    assert_eq!(out.json["ok"], json!(true));
    assert!(entities(&mb, "point_script").is_empty());
    assert_eq!(next_tools(&out), ["save_map", "compile_map"]);
    // nothing to remove: no save/compile suggestion
    let out = pm(&server, json!({"action": "clear"})).await.unwrap();
    assert_eq!(out.json["ok"], json!(true));
    assert!(next_tools(&out).is_empty(), "{:?}", next_tools(&out));
    // no open map: no save/compile either, and the reply is not ok
    let tree2 = Tree::new("honest-nomap");
    let mb2 = MockBridge::builder().doc(None).start().await.unwrap();
    let server2 = hammer_mcp::build_server(&tree2.config(Some(&mb2.discovery_path))).unwrap();
    init(&server2).await;
    let out = pm(&server2, json!({"action": "clear"})).await.unwrap();
    assert_eq!(out.json["ok"], json!(false), "{}", out.json);
    assert!(!next_tools(&out).contains(&"save_map".to_string()));
    assert!(!next_tools(&out).contains(&"compile_map".to_string()));
}
