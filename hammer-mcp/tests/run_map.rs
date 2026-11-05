//! `run_map` tool tests (WP-R): the VToolMessage `command` route against a fake podman
//! (`vtmsend`, whose exec stands in for the engine: it appends the "engine" console lines), a
//! fake `game/csgo/console.log` under a temp project tree, and the mock bridge for `doc.active`.
//!
//! No container, no game and no real bridge is involved anywhere in this file. The temp tree
//! lives under `CARGO_TARGET_TMPDIR` (never /tmp), and every test's fake podman logs its
//! invocations so "nothing was sent" can be asserted rather than assumed.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The document path the mock serves (Windows form; `--path-map` maps `z:\app` at the tree).
const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";
const DOC_ARG: &str = "maps/probe.vmap";

/// A console tail a previous session left behind: the offsets in this file must keep it out of a
/// new call's answer.
const OLD_CONSOLE: &str = "10/13 20:00:00 Loading map \"other\"\r\n\
                           10/13 20:00:01 [SignonState] CL:  \
                           CNetworkGameClient::OnSwitchLoopModeFinished( game : success )\r\n";

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "run-map-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let app = root.join("workshop/run/app");
        std::fs::create_dir_all(app.join("game/csgo")).expect("tmp tree");
        Tree { root, app }
    }

    fn maps_dir(&self) -> PathBuf {
        self.app.join("content/csgo_addons/mcp_test/maps")
    }

    /// The host path of the source map (`DOC_WIN` mapped through `z:\app`).
    fn vmap(&self) -> PathBuf {
        self.maps_dir().join("probe.vmap")
    }

    /// The compiled map the engine loads (`<app>/game/csgo_addons/<addon>/maps/probe.vpk`).
    fn vpk(&self) -> PathBuf {
        self.app
            .join("game/csgo_addons/mcp_test/maps/probe.vpk")
    }

    fn console(&self) -> PathBuf {
        self.app.join("game/csgo/console.log")
    }

    fn payload(&self) -> PathBuf {
        self.app.join("vtm_payload.kv3")
    }

    /// The source map on disk (a stub: `run_map` never parses the `.vmap`, it only stats it).
    fn install_vmap(&self) -> PathBuf {
        std::fs::create_dir_all(self.maps_dir()).expect("maps dir");
        std::fs::write(self.vmap(), b"fake vmap").expect("vmap");
        self.vmap()
    }

    fn install_vpk(&self) -> PathBuf {
        let vpk = self.vpk();
        std::fs::create_dir_all(vpk.parent().expect("maps dir")).expect("game maps dir");
        // a real VPK header (signature, version 2, tree size 4) + padding: run_map checks it
        let mut bytes = vec![0x34, 0x12, 0xAA, 0x55, 2, 0, 0, 0, 4, 0, 0, 0];
        bytes.extend([0u8; 40]);
        std::fs::write(&vpk, bytes).expect("vpk");
        vpk
    }

    /// What the fake engine appends to `console.log` when a `map …` command arrives.
    fn engine_says(&self, lines: &str) {
        std::fs::write(self.root.join("engine-lines"), lines).expect("engine lines");
    }

    /// What the fake engine appends to `console.log` when a `disconnect` arrives: the idle level
    /// (`[Server] SV:  Spawn Server: <empty>`). Without this the remount never sees its signal.
    fn idle_says(&self, lines: &str) {
        std::fs::write(self.root.join("idle-lines"), lines).expect("idle lines");
    }

    /// The console.log a previous session left (so offsets can be observed).
    fn old_console(&self, text: &str) {
        std::fs::write(self.console(), text).expect("console");
    }

    /// Every VToolMessage payload the fake `vtmsend` consumed, in order.
    fn payloads(&self) -> Vec<String> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&self.root)
            .expect("tree")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("vtm-") && n.ends_with(".kv3"))
            })
            .collect();
        files.sort();
        files
            .iter()
            .map(|p| std::fs::read_to_string(p).expect("payload"))
            .collect()
    }

    /// Every podman invocation the fake logged (`exec …` / `ps …`).
    fn podman_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("podman-calls.log"))
            .map(|text| text.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A fake `podman` under the tree (mode 0755). Kinds:
/// * `ok` — `ps` reports `cs2tools`; `exec` copies the VToolMessage payload aside, prints the
///   channel marker (`creator version`, what runtime.sh `wait_channel` waits for) and, for a
///   payload whose `_data` starts with `map `, appends `<tree>/engine-lines` to console.log (the
///   engine's own load signal), or `<tree>/idle-lines` for `disconnect` (the idle level);
/// * `slow` — `ok`, but each `exec` takes [`SLOW_EXEC_MS`]: two concurrent calls that are not
///   serialized by the payload gate would interleave their write-then-exec pairs (M7);
/// * `exec-timeout` — `ok` for `ps`, and an `exec` that never returns (the runtime kills the
///   podman client at the (test-shortened) short timeout: `PodmanTimeout`, the M6 delivery-unknown case);
/// * `no-container` — `ps` reports nothing (no container carries the label);
/// * `channel-down` — `exec` prints `OpenMutex failed` and exits 1 (no VToolMessage mutex yet).
fn fake_podman(tree: &Tree, kind: &str) -> PathBuf {
    let bin_dir = tree.root.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    let body = match kind {
        "ok" | "slow" => {
            let slow = if kind == "slow" {
                format!("sleep {SLOW_EXEC_SECS}\n")
            } else {
                String::new()
            };
            format!(
                r#"#!/bin/bash
APP='__APP__'
TREE='__TREE__'
case "$1" in
  ps)
    printf '[{{"Names":["cs2tools"],"State":"running"}}]'
    exit 0
    ;;
  exec)
    echo "$*" >> "$TREE/podman-calls.log"
    n=$(( $(cat "$TREE/vtm-count" 2>/dev/null || echo 0) + 1 ))
    echo "$n" > "$TREE/vtm-count"
    cp "$APP/vtm_payload.kv3" "$TREE/vtm-$n.kv3"
    echo "vtmsend: creator version 1"
    {slow}if grep -q '_data = "map ' "$APP/vtm_payload.kv3"; then
      if [ -f "$TREE/engine-lines" ]; then
        cat "$TREE/engine-lines" >> "$APP/game/csgo/console.log"
      fi
    elif grep -q '_data = "disconnect"' "$APP/vtm_payload.kv3"; then
      if [ -f "$TREE/idle-lines" ]; then
        cat "$TREE/idle-lines" >> "$APP/game/csgo/console.log"
      fi
    fi
    exit 0
    ;;
  *)
    echo "fake podman: unsupported verb $1" >&2
    exit 2
    ;;
esac
"#
            )
        }
        // the podman client is killed at the runtime's 10 s short timeout: `vtmsend.exe` may or
        // may not have delivered, and the engine prints nothing (the M6 unknown case)
        "exec-timeout" => r#"#!/bin/bash
APP='__APP__'
TREE='__TREE__'
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    echo "$*" >> "$TREE/podman-calls.log"
    n=$(( $(cat "$TREE/vtm-count" 2>/dev/null || echo 0) + 1 ))
    echo "$n" > "$TREE/vtm-count"
    cp "$APP/vtm_payload.kv3" "$TREE/vtm-$n.kv3"
    echo "vtmsend: creator version 1"
    sleep 30
    exit 0
    ;;
esac
"#
        .to_string(),
        "no-container" => r#"#!/bin/bash
case "$1" in
  ps) echo '[]' ;;
  *) echo "fake podman: unsupported verb $1" >&2; exit 2 ;;
esac
"#
        .to_string(),
        "channel-down" => r#"#!/bin/bash
TREE='__TREE__'
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    echo "$*" >> "$TREE/podman-calls.log"
    echo "vtmsend: OpenMutex failed - is a -tools session running?"
    exit 1
    ;;
esac
"#
        .to_string(),
        other => panic!("unknown fake podman kind {other}"),
    }
    .replace("__APP__", &tree.app.display().to_string())
    .replace("__TREE__", &tree.root.display().to_string());
    let script = bin_dir.join(format!("podman-{kind}"));
    std::fs::write(&script, body).expect("fake podman script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    script
}

/// One `slow` exec (M7's interleaving window).
const SLOW_EXEC_SECS: &str = "0.35";

fn config(tree: &Tree, discovery: &Path, podman: Option<PathBuf>, tool_budget_ms: u64) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let map = format!("z:\\app={}", tree.app.display());
    let discovery = discovery.display().to_string();
    let budget = tool_budget_ms.to_string();
    let mut args: Vec<String> = vec![
        "--deadline-ms".into(),
        "5000".into(),
        "--tool-budget-ms".into(),
        budget,
        "--discovery".into(),
        discovery,
        "--project-root".into(),
        root,
        "--path-map".into(),
        map,
    ];
    if let Some(podman) = podman {
        // The fake podman is a forked bash script whose cost grows with host load: its exec
        // timeout is generous, except for the tests about the cut-off itself (`exec-timeout`).
        let cut_off = podman.file_name().is_some_and(|n| n.to_string_lossy().ends_with("exec-timeout"));
        args.extend(["--podman-short-timeout-ms".into(), if cut_off { "2000" } else { "120000" }.into()]);
        args.extend(["--podman".into(), podman.display().to_string()]);
    }
    let given: Vec<&str> = args.iter().map(String::as_str).collect();
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

/// A mock bridge with the built-in `doc.active` world (path/dirty from `make_doc`), plus a
/// `game.local` responder when `game_local` is `Some` (the mock advertises the method then).
async fn mock_bridge(dirty: Option<bool>, game_local: Option<Value>) -> MockBridge {
    let mut builder = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, dirty)))
        .hammer_nodes(vec![MockNode::world()]);
    if let Some(reply) = game_local {
        builder = builder.responder_value("game.local", reply);
    }
    builder.start().await.unwrap()
}

/// The engine parked on its idle level (what a `disconnect` produces; playtest 3's preflight).
fn idle_level() -> &'static str {
    "10/13 21:04:08 [Server] SV:  Spawn Server: <empty>\r\n"
}

/// `game.local` as the engine answers it while a map is up (m2.9).
fn map_is_up(name: &str) -> Value {
    json!({"inTick": 122, "inGame": true, "map": name, "playerSlot": 0})
}

fn build(tree: &Tree, mb: &MockBridge, podman: Option<PathBuf>, budget_ms: u64) -> McpServer<HammerState> {
    let cfg = config(tree, &mb.discovery_path, podman, budget_ms);
    hammer_mcp::build_server(&cfg).expect("server")
}

async fn call(server: &McpServer<HammerState>, name: &str, args: Value) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

/// The engine lines a successful load leaves behind (the live playtest shapes, CRLF).
fn loaded(name: &str) -> String {
    format!(
        "10/13 20:24:00 Loading map \"{name}\"\r\n\
         10/13 20:24:08 [Server] SV:  Spawn Server: {name}\r\n\
         10/13 20:24:09 [SignonState] CL:  \
         CNetworkGameClient::OnSwitchLoopModeFinished( game : success )\r\n"
    )
}

// -------------------------------------------------------------------------------------------
// The happy path
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn run_map_loads_the_active_documents_map_and_reports_the_engine_lines() {
    let tree = Tree::new("ok");
    tree.install_vmap();
    tree.install_vpk();
    tree.old_console(OLD_CONSOLE);
    tree.engine_says(&loaded("probe"));
    let mb = mock_bridge(Some(false), None).await;
    // a budget that fits the default 60 s wait (the harness default is 45 s: the wait is then
    // clamped and budgetClamped says so)
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 120_000);

    let out = call(&server, "run_map", json!({})).await.unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["state"], json!("ok"));
    assert_eq!(out["map"], json!("probe"));
    let steps: Vec<&str> = out["next"].as_array().unwrap().iter().map(|n| n["tool"].as_str().unwrap()).collect();
    assert_eq!(steps, ["game_state", "render_world", "report_ready"], "the engine map is looked at with render_world, not the editor's render_view; report_ready is the ship check");
    assert_eq!(out["next"][2]["args"], json!({"map": out["contentPath"]}));
    assert_eq!(out["contentPath"], json!(DOC_ARG));
    assert_eq!(out["command"], json!("map probe nomapvalidation=1"));
    assert_eq!(out["modeCommands"], json!([]));
    assert_eq!(out["container"], json!("cs2tools"));
    // the signal is the new console window, not the file: the old session's own success line
    // (same text!) is before the send-time offset and must not be reported
    let console = &out["console"];
    assert!(
        console["loadingLine"].as_str().unwrap().contains("Loading map \"probe\""),
        "{console}"
    );
    assert!(
        console["successLine"].as_str().unwrap().contains("game : success"),
        "{console}"
    );
    assert_eq!(console["failLine"], Value::Null);
    assert_eq!(console["exists"], json!(true));
    assert!(
        console["tail"].as_array().unwrap().len() >= 3,
        "the tail is what the call saw: {console}"
    );
    assert!(
        !console["tail"].as_array().unwrap().iter().any(|l| l.as_str() == Some("")),
        "blank lines are dropped from the tail"
    );
    // the compiled map behind it
    assert_eq!(out["vpk"]["hostPath"], json!(tree.vpk().display().to_string()));
    assert_eq!(out["vpk"]["relative"], json!("csgo_addons/mcp_test/maps/probe.vpk"));
    assert_eq!(out["vpk"]["found"], json!(true));
    assert_eq!(out["vpk"]["form"], json!("content-relative"));
    assert_eq!(out["source"]["hostPath"], json!(tree.vmap().display().to_string()));
    assert_eq!(out["source"]["found"], json!(true));
    assert_eq!(out["source"]["staleCompile"], json!(false));
    assert!(out["elapsedMs"].as_u64().is_some());
    assert_eq!(out["budgetClamped"], json!(false), "the budget fits the default 60 s");
    assert_eq!(out["timeoutMs"], json!(60_000));
    // the document was read (the dirty note) and is reported as the other tools report it
    assert_eq!(out["document"]["id"], json!("d1"));
    assert_eq!(out["document"]["dirty"], json!(false));
    assert_eq!(out["gameLocal"], Value::Null, "the mock bridge does not expose game.local");
    assert!(!mb.method_calls("doc.active").is_empty());

    // exactly one post, and it is the map command
    let payloads = tree.payloads();
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    assert!(
        payloads[0].contains(r#"_data = "map probe nomapvalidation=1""#),
        "{payloads:?}"
    );
    assert_eq!(out["vtmsend"][0]["command"], json!("map probe nomapvalidation=1"));
    assert_eq!(out["vtmsend"][0]["confirmed"], json!(true));
    assert_eq!(out["vtmsend"][0]["retried"], json!(false));
    // never `maps`: that console command crashes the engine
    assert!(!payloads[0].contains("_data = \"maps"));
}

#[tokio::test]
async fn run_map_sends_the_mode_commands_before_the_map_command() {
    let tree = Tree::new("mode");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let out = call(&server, "run_map", json!({"gameType": 0, "gameMode": 1}))
        .await
        .unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    // L4: one post per mode line (the proven recipe), never `game_type 0; game_mode 1` in one
    assert_eq!(out["modeCommands"], json!(["game_type 0", "game_mode 1"]));
    let payloads = tree.payloads();
    assert_eq!(payloads.len(), 3, "{payloads:?}");
    assert!(
        payloads[0].contains(r#"_data = "game_type 0""#),
        "the mode lines are first (a command during ss_loading crashed the engine once): \
         {payloads:?}"
    );
    assert!(payloads[1].contains(r#"_data = "game_mode 1""#), "{payloads:?}");
    assert!(!payloads[0].contains(';'), "no `;`-joined command line: {payloads:?}");
    assert!(payloads[2].contains(r#"_data = "map probe nomapvalidation=1""#), "{payloads:?}");
    assert_eq!(
        out["vtmsend"].as_array().unwrap().len(),
        3,
        "one report per post: {out}"
    );
}

#[tokio::test]
async fn run_map_reports_game_local_when_the_bridge_exposes_it() {
    let tree = Tree::new("game-local");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    tree.idle_says(idle_level());
    // the engine already has a map up: the load must be preceded by a disconnect (H1). The reply
    // changes after the load (the mock is asked once before the posts and once after them).
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .responder("game.local", {
            let reads = Arc::new(AtomicUsize::new(0));
            move |_| {
                if reads.fetch_add(1, Ordering::Relaxed) == 0 {
                    map_is_up("other_map")
                } else {
                    map_is_up("probe")
                }
            }
        })
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let out = call(&server, "run_map", json!({})).await.unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["gameLocal"]["inGame"], json!(true));
    assert_eq!(out["gameLocal"]["map"], json!("probe"));
    assert_eq!(out["gameLocalBefore"]["map"], json!("other_map"));
    assert_eq!(out["remount"]["needed"], json!(true));
    assert_eq!(out["remount"]["source"], json!("game.local"));
    let payloads = tree.payloads();
    assert_eq!(payloads.len(), 2, "{payloads:?}");
    assert!(payloads[0].contains(r#"_data = "disconnect""#), "{payloads:?}");
    assert!(payloads[1].contains(r#"_data = "map probe nomapvalidation=1""#), "{payloads:?}");
}

/// **M5**: the console says success for this map, but the engine's own answer is another map — the
/// success line can only belong to another load, so the result is not `ok`.
#[tokio::test]
async fn a_console_success_for_another_map_is_not_reported_as_ok() {
    let tree = Tree::new("m5-mismatch");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    tree.idle_says(idle_level());
    // `game.local` keeps naming a different map: the engine is not in `probe`
    let mb = mock_bridge(Some(false), Some(map_is_up("other_map"))).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let out = call(&server, "run_map", json!({})).await.unwrap();
    assert_eq!(out["state"], json!("unconfirmed"), "{out}");
    assert_eq!(out["ok"], json!(false));
    // the console line is still reported as it was seen (the state carries the mismatch)
    assert!(
        out["console"]["successLine"]
            .as_str()
            .unwrap()
            .contains("game : success"),
        "{out}"
    );
    assert!(
        out["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("game.local reports")),
        "{out}"
    );
}

#[tokio::test]
async fn run_map_takes_an_explicit_path_without_a_document() {
    let tree = Tree::new("explicit");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    // no document at all (doc.active answers no_session). No hammer world: that arm always
    // answers a document, and this test is about the route that needs none.
    let mb = MockBridge::builder().doc(None).start().await.unwrap();
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let out = call(&server, "run_map", json!({"path": DOC_ARG})).await.unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["document"], Value::Null);
    assert_eq!(out["map"], json!("probe"));
    assert_eq!(out["vpk"]["found"], json!(true));

    // ... while the default target needs one
    let err = call(&server, "run_map", json!({})).await.unwrap_err();
    assert_eq!(err.name, "NoDocument");
    assert_eq!(err.code, Some(-32003));
    assert!(err.retry_safe);
}

#[tokio::test]
async fn run_map_picks_a_flattened_vpk_and_names_the_map_by_its_stem() {
    // a sub-directory map the compiler flattened to maps/<stem>.vpk (compile_map.sh's OUT)
    let tree = Tree::new("flattened");
    let sub = tree.maps_dir().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("probe.vmap"), b"fake vmap").unwrap();
    // M8: the flattened candidate is only accepted when it is NEWER than the .vmap it is being
    // attributed to (an unrelated top-level map of the same stem would otherwise be picked)
    std::thread::sleep(std::time::Duration::from_millis(20));
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let out = call(&server, "run_map", json!({"path": "maps/sub/probe.vmap"}))
        .await
        .unwrap();
    assert_eq!(out["state"], json!("ok"), "{out}");
    assert_eq!(out["vpk"]["form"], json!("flattened"));
    assert_eq!(out["vpk"]["relative"], json!("csgo_addons/mcp_test/maps/probe.vpk"));
    assert_eq!(
        out["command"],
        json!("map probe nomapvalidation=1"),
        "the flattened vpk is addressed by its bare stem"
    );
    assert!(
        out["notes"].as_array().unwrap().iter().any(|n| n
            .as_str()
            .unwrap()
            .contains("vpkForm")),
        "the fallback is explained: {out}"
    );
}

/// LIVE-P1: a corrupt vpk FATALs the engine and ends the tools session; the header check refuses
/// it before anything is sent.
#[tokio::test]
async fn a_corrupt_vpk_is_refused_before_anything_is_sent() {
    let tree = Tree::new("corrupt-vpk");
    tree.install_vmap();
    let vpk = tree.install_vpk();
    std::fs::write(&vpk, vec![0x5Au8; 4096]).unwrap();
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);
    let err = call(&server, "run_map", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "InvalidVpk", "{err:?}");
    assert_eq!(err.data["useInstead"]["tool"], json!("compile_map"), "{err:?}");
    assert!(err.message.contains("signature"), "{err:?}");
}

/// **M8**: a sub-directory map whose own `maps/sub/x.vpk` exists never falls back to the flattened
/// `maps/x.vpk` (which could belong to an unrelated top-level `maps/x.vmap`), and the engine name
/// keeps the `sub/x` spelling.
#[tokio::test]
async fn a_sub_directory_map_prefers_its_own_content_relative_vpk() {
    let tree = Tree::new("sub-own-vpk");
    let sub = tree.maps_dir().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("probe.vmap"), b"fake vmap").unwrap();
    // the unrelated top-level one, older than the sub map's own vpk
    tree.install_vpk();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let own = tree
        .app
        .join("game/csgo_addons/mcp_test/maps/sub/probe.vpk");
    std::fs::create_dir_all(own.parent().unwrap()).unwrap();
    let mut own_bytes = vec![0x34, 0x12, 0xAA, 0x55, 2, 0, 0, 0, 4, 0, 0, 0];
    own_bytes.extend([0u8; 40]);
    std::fs::write(&own, own_bytes).unwrap();
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);
    tree.engine_says(&loaded("sub/probe"));

    let out = call(&server, "run_map", json!({"path": "maps/sub/probe.vmap"}))
        .await
        .unwrap();
    assert_eq!(out["state"], json!("ok"), "{out}");
    assert_eq!(out["vpk"]["form"], json!("content-relative"));
    assert_eq!(
        out["vpk"]["relative"],
        json!("csgo_addons/mcp_test/maps/sub/probe.vpk")
    );
    assert_eq!(out["command"], json!("map sub/probe nomapvalidation=1"));
}

/// **M8**: the flattened fallback is refused when it is *older* than the `.vmap` (it is then
/// evidence of another map's compile, not this one's) — `VpkNotFound`, nothing sent.
#[tokio::test]
async fn a_sub_directory_map_refuses_a_stale_flattened_vpk() {
    let tree = Tree::new("sub-stale-flat");
    let sub = tree.maps_dir().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    // the unrelated top-level compile first ...
    tree.install_vpk();
    std::thread::sleep(std::time::Duration::from_millis(20));
    // ... then the sub map's source, which is newer than that vpk
    std::fs::write(sub.join("probe.vmap"), b"fake vmap").unwrap();
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let err = call(&server, "run_map", json!({"path": "maps/sub/probe.vmap"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "VpkNotFound");
    assert_eq!(err.data["flattenedRejected"], json!("older"));
    assert!(err.message.contains("OLDER"), "{err:?}");
    assert!(tree.podman_calls().is_empty(), "nothing was dispatched");
    assert_eq!(err.data["useInstead"]["tool"], json!("compile_map"));
}

// -------------------------------------------------------------------------------------------
// H1: the mounted map, M6: one load at a time, M7: one payload file
// -------------------------------------------------------------------------------------------

/// **H1** with no `game.local` on the bridge (an older build, or no client scope): this server's
/// own last successful load is the mount memory, so the second call disconnects before it posts.
#[tokio::test]
async fn a_map_this_server_loaded_is_disconnected_on_the_next_run() {
    let tree = Tree::new("remount-memory");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    tree.idle_says(idle_level());
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 120_000);

    let first = call(&server, "run_map", json!({})).await.unwrap();
    assert_eq!(first["state"], json!("ok"), "{first}");
    assert_eq!(first["remount"]["needed"], json!(false));
    assert!(
        !tree.payloads()[0].contains("disconnect"),
        "the first call has nothing to unmount"
    );

    let second = call(&server, "run_map", json!({})).await.unwrap();
    assert_eq!(second["state"], json!("ok"), "{second}");
    assert_eq!(second["remount"]["needed"], json!(true));
    assert_eq!(second["remount"]["source"], json!("previous-load"));
    assert_eq!(second["remount"]["disconnected"], json!(true));
    assert!(
        second["remount"]["signal"]
            .as_str()
            .unwrap()
            .contains("Spawn Server: <empty>"),
        "{second}"
    );
    assert!(
        second["notes"].as_array().unwrap().iter().any(|n| n
            .as_str()
            .unwrap()
            .contains("keeps serving its mounted copy")),
        "the reason is spelled out: {second}"
    );
    let payloads = tree.payloads();
    assert_eq!(payloads.len(), 3, "{payloads:?}");
    assert!(payloads[1].contains(r#"_data = "disconnect""#), "{payloads:?}");
    assert!(payloads[2].contains(r#"_data = "map probe nomapvalidation=1""#), "{payloads:?}");
    // the load record is the memory the next call uses
    assert_eq!(second["gameLocalBefore"], Value::Null, "no game.local on this bridge");
}

/// **H1**: a `disconnect` whose idle-level line never appears means the engine may still be
/// loading the idle level — the map command is NOT sent (a console command during a load is the
/// documented crash trigger), and the result says so.
#[tokio::test]
async fn a_disconnect_that_never_parks_the_engine_does_not_get_the_map_command() {
    let tree = Tree::new("remount-failed");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    // console.log exists (a -condebug session), so the idle level IS waited on ...
    tree.old_console("10/13 00:00:00 old session\r\n");
    // ... but the fake engine never prints `Spawn Server: <empty>` (no `idle_says`)
    let mb = mock_bridge(Some(false), Some(map_is_up("other_map"))).await;
    // a small budget: the disconnect wait is min(120 s, what is left)
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 2_500);

    let out = call(&server, "run_map", json!({"timeoutMs": 1_500})).await.unwrap();
    assert_eq!(out["state"], json!("failed"), "{out}");
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["remount"]["failed"], json!(true));
    assert_eq!(out["remount"]["signal"], Value::Null);
    let payloads = tree.payloads();
    assert_eq!(payloads.len(), 1, "only the disconnect was sent: {payloads:?}");
    assert!(payloads[0].contains(r#"_data = "disconnect""#), "{payloads:?}");
    assert!(
        out["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("NOT sent")),
        "{out}"
    );
}

/// **M6**: this server's previous load never showed a verdict, so the next call settles it — and
/// refuses `EngineBusy` when the console still has none. Nothing is sent.
#[tokio::test]
async fn a_second_call_while_the_previous_load_is_unresolved_is_engine_busy() {
    let tree = Tree::new("m6-busy");
    tree.install_vmap();
    tree.install_vpk();
    // the engine says it is loading and then nothing: the first call ends `timeout`
    tree.engine_says("11/02 10:00:00 Loading map \"probe\"\r\n");
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 2_000);

    let first = call(&server, "run_map", json!({"timeoutMs": 600})).await.unwrap();
    assert_eq!(first["state"], json!("timeout"), "{first}");

    let err = call(&server, "run_map", json!({"timeoutMs": 600}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "EngineBusy", "{err:?}");
    assert!(err.retry_safe);
    assert_eq!(err.data["previousLoad"]["map"], json!("probe"));
    assert_eq!(err.data["previousLoad"]["resolved"], json!(false));
    assert!(
        tree.payloads().len() == 1,
        "the refused call sent nothing: {:?}",
        tree.payloads()
    );
}

/// **M6**: a `podman exec` that never comes back may still have delivered (`vtmsend.exe` keeps
/// running in the container), so the post is reported `delivered:false`, never repeated, and the
/// undecided outcome is `state:"unknown"` — not a `timeout` that tells the caller to re-run.
#[tokio::test]
async fn a_post_that_never_comes_back_is_unknown_and_not_repeated() {
    let tree = Tree::new("m6-unknown");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    let mb = mock_bridge(Some(false), None).await;
    // the runtime kills the podman client at the (test-shortened) short timeout
    let server = build(&tree, &mb, Some(fake_podman(&tree, "exec-timeout")), 120_000);

    let out = call(&server, "run_map", json!({"timeoutMs": 1_000})).await.unwrap();
    assert_eq!(out["state"], json!("unknown"), "{out}");
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["vtmsend"][0]["delivered"], json!(false));
    assert_eq!(out["vtmsend"][0]["retried"], json!(false), "never repeated");
    assert_eq!(tree.payloads().len(), 1, "one post, no retry: {:?}", tree.payloads());
    assert!(
        out["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("do NOT re-run blindly")),
        "{out}"
    );
}

/// **M7**: the VToolMessage payload is ONE fixed file, so two calls must not interleave their
/// write-then-exec pairs. The mode lines of the first call stay contiguous.
#[tokio::test]
async fn concurrent_calls_do_not_interleave_their_posts() {
    let tree = Tree::new("m7-gate");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    tree.idle_says(idle_level());
    let mb = mock_bridge(Some(false), None).await;
    // every exec of the first call takes 350 ms: without a shared gate the second call's payload
    // write lands between them (and would be the one vtmsend reads)
    let server = build(&tree, &mb, Some(fake_podman(&tree, "slow")), 120_000);

    let (first, second) = tokio::join!(
        call(
            &server,
            "run_map",
            json!({"path": DOC_ARG, "gameType": 0, "gameMode": 1})
        ),
        call(&server, "run_map", json!({"path": DOC_ARG})),
    );
    let payloads = tree.payloads();
    let commands: Vec<&str> = payloads
        .iter()
        .map(|p| {
            p.lines()
                .find(|l| l.contains("_data ="))
                .unwrap_or("")
                .trim()
        })
        .collect();
    assert!(
        first.is_ok() || second.is_ok(),
        "at least one call must have gone through"
    );
    // the mode lines are contiguous and immediately followed by a map command
    if let Some(at) = commands.iter().position(|c| c.contains(r#"_data = "game_type 0""#)) {
        assert_eq!(
            commands[at + 1],
            r#"_data = "game_mode 1""#,
            "the first call's posts are not interleaved: {commands:?}"
        );
        assert!(
            commands[at + 2].contains(r#"_data = "map probe nomapvalidation=1""#),
            "another call's post slipped between them: {commands:?}"
        );
    } else {
        panic!("the mode lines were never posted: {commands:?}");
    }
}

/// **L1**: the load's window really is `EngineBusy` (not `HammerBlocked`), and it is released when
/// the load settles. `mainThreadAgeMs` is stale in this mock, so the frame loop looks frozen; the
/// load is the only excuse.
#[tokio::test]
async fn engine_busy_is_claimed_during_the_load_and_released_when_it_settles() {
    let tree = Tree::new("l1-busy");
    tree.install_vmap();
    tree.install_vpk();
    // no verdict at all: the call ends `timeout` with its load unresolved
    tree.engine_says("11/02 10:00:00 Loading map \"probe\"\r\n");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .main_thread_age_ms(Some(60_000))
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 4_000);

    let out = call(&server, "run_map", json!({"path": DOC_ARG, "timeoutMs": 700}))
        .await
        .unwrap();
    assert_eq!(out["state"], json!("timeout"), "{out}");
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(
        status["liveness"]["engineBusy"],
        json!(true),
        "an unresolved load keeps the window excused: {status}"
    );
    assert_eq!(status["liveness"]["hammerBlocked"], json!(false));

    // ... and a settled load releases it: the same stale main thread now reads as a modal dialog
    let second_tree = Tree::new("l1-released");
    second_tree.install_vmap();
    second_tree.install_vpk();
    second_tree.engine_says(&loaded("probe"));
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .main_thread_age_ms(Some(60_000))
        .start()
        .await
        .unwrap();
    let server = build(&second_tree, &mb, Some(fake_podman(&second_tree, "ok")), 4_000);
    let out = call(&server, "run_map", json!({"path": DOC_ARG, "timeoutMs": 1_000}))
        .await
        .unwrap();
    assert_eq!(out["state"], json!("ok"), "{out}");
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(
        status["liveness"]["engineBusy"],
        json!(false),
        "the grace is released when the load settles: {status}"
    );
    assert_eq!(status["liveness"]["hammerBlocked"], json!(true));
}

/// **L6**: the final `game.local` read gets what is left of the tool budget, not the deadline the
/// call started with.
#[tokio::test]
async fn the_final_game_local_read_uses_what_is_left_of_the_budget() {
    let tree = Tree::new("l6-deadline");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    let mb = mock_bridge(Some(false), Some(map_is_up("probe"))).await;
    tree.idle_says(idle_level());
    // every exec takes 350 ms: gameType+gameMode+disconnect+map consume ~1.4 s of the budget, so
    // the final `game.local` gets less than the first read. The deadline cap (60 s) is above the
    // budget (30 s), so each read gets the remaining budget and the order holds at any host load.
    let mut cfg = config(&tree, &mb.discovery_path, Some(fake_podman(&tree, "slow")), 30_000);
    cfg.deadline_ms = 60_000;
    let server = hammer_mcp::build_server(&cfg).expect("server");

    let out = call(
        &server,
        "run_map",
        json!({"path": DOC_ARG, "gameType": 0, "gameMode": 1, "timeoutMs": 2_000}),
    )
    .await
    .unwrap();
    assert_eq!(out["state"], json!("ok"), "{out}");
    let calls = mb.method_calls("game.local");
    assert_eq!(calls.len(), 2, "one read before and one after the load");
    let first = calls[0].params["deadlineMs"].as_u64().unwrap();
    let second = calls[1].params["deadlineMs"].as_u64().unwrap();
    assert!(
        second < first,
        "the read-back deadline follows the remaining budget: {first} then {second}"
    );
}

/// **M5** end to end: a success line with no `Loading map "<probe>"` of this call's own is not a
/// verdict for this load (it can only be another load's).
#[tokio::test]
async fn a_success_line_without_this_maps_loading_line_is_not_ok() {
    let tree = Tree::new("m5-unanchored");
    tree.install_vmap();
    tree.install_vpk();
    // the engine answered a *previous* map's load with success; this call's own window has no
    // `Loading map "probe"` line
    tree.engine_says(
        "11/02 10:00:00 [SignonState] CL:  CNetworkGameClient::OnSwitchLoopModeFinished( game : success )\r\n",
    );
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let out = call(&server, "run_map", json!({"timeoutMs": 400})).await.unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["state"], json!("timeout"), "{out}");
    assert_eq!(out["console"]["successLine"], Value::Null);
}


#[tokio::test]
async fn a_failure_line_is_a_result_with_the_console_tail() {
    let tree = Tree::new("failed");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says("11/02 10:00:00 Loading map \"probe\"\r\n11/02 10:00:01 Failed to load map\r\n");
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let out = call(&server, "run_map", json!({})).await.unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["state"], json!("failed"));
    assert!(out.get("next").is_none(), "a failed load suggests nothing: {out}");
    assert!(out["console"]["failLine"].as_str().unwrap().contains("Failed to load map"));
    assert_eq!(out["console"]["successLine"], Value::Null);
    assert!(
        out["notes"].as_array().unwrap().iter().any(|n| n
            .as_str()
            .unwrap()
            .contains("read failLine")),
        "{out}"
    );
}

#[tokio::test]
async fn a_load_that_never_signals_is_a_timeout_result_not_an_error() {
    let tree = Tree::new("timeout");
    tree.install_vmap();
    tree.install_vpk();
    // the engine accepted the command and said it was loading, then nothing
    tree.engine_says("11/02 10:00:00 Loading map \"probe\"\r\n");
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let out = call(&server, "run_map", json!({"timeoutMs": 400})).await.unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["state"], json!("timeout"));
    assert_eq!(out["timeoutMs"], json!(400));
    assert_eq!(out["budgetClamped"], json!(false));
    assert!(
        out["console"]["loadingLine"].as_str().unwrap().contains("probe"),
        "what was seen is reported: {out}"
    );
    assert_eq!(out["console"]["successLine"], Value::Null);
    assert!(out["elapsedMs"].as_u64().unwrap() >= 400, "{out}");
    assert!(
        out["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("re-run")),
        "{out}"
    );
}

#[tokio::test]
async fn a_stale_vpk_is_reported_and_still_run() {
    let tree = Tree::new("stale");
    // the vpk exists first, the .vmap is written after it: the compile is older than the source
    tree.install_vpk();
    std::thread::sleep(std::time::Duration::from_millis(20));
    tree.install_vmap();
    tree.engine_says(&loaded("probe"));
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let out = call(&server, "run_map", json!({})).await.unwrap();
    assert_eq!(out["state"], json!("ok"), "{out}");
    assert_eq!(out["source"]["staleCompile"], json!(true));
    let notes = out["notes"].as_array().unwrap();
    assert!(notes.iter().any(|n| n.as_str().unwrap().contains("staleCompile")), "{out}");
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap().contains("mounted")),
        "the recompile caveat is spelled out: {out}"
    );
}

#[tokio::test]
async fn a_dirty_document_is_noted_never_refused() {
    // a dirty document: the load is NOT refused (the engine loads the .vpk, the document is
    // untouched), and the note says the unsaved edits are not in what runs
    for (mb, expected) in [
        (mock_bridge(Some(true), None).await, "unsaved changes"),
        // `dirty:null`: the hammer world coerces a missing dirty to false, so this document
        // comes from a `doc.active` responder (its dirty state is genuinely unknown)
        (
            MockBridge::builder()
                .responder_value(
                    "doc.active",
                    json!({"doc": make_doc(DOC_WIN, None)}),
                )
                .start()
                .await
                .unwrap(),
            "unknown",
        ),
    ] {
        let tree = Tree::new("dirty");
        tree.install_vmap();
        tree.install_vpk();
        tree.engine_says(&loaded("probe"));
        let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

        let out = call(&server, "run_map", json!({})).await.unwrap();
        assert_eq!(out["state"], json!("ok"), "{out}");
        assert!(
            out["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap().contains(expected)),
            "{expected} -> {out}"
        );
    }
}

// -------------------------------------------------------------------------------------------
// Refusals (nothing is sent)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_map_that_was_never_compiled_is_refused_before_anything_is_sent() {
    let tree = Tree::new("uncompiled");
    tree.install_vmap();
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let err = call(&server, "run_map", json!({})).await.unwrap_err();
    assert_eq!(err.name, "VpkNotFound");
    assert_eq!(err.data["useInstead"]["tool"], json!("compile_map"));
    assert_eq!(err.data["path"], json!(DOC_ARG));
    assert_eq!(err.data["vpkHostPath"], json!(tree.vpk().display().to_string()));
    assert!(err.retry_safe);
    assert!(err.message.contains("compile_map"), "{err:?}");
    assert!(tree.podman_calls().is_empty(), "nothing was dispatched");
    assert!(!tree.payload().exists(), "no payload was written");
}

#[tokio::test]
async fn run_map_without_a_tools_container_is_runtime_unavailable() {
    let tree = Tree::new("no-container");
    tree.install_vmap();
    tree.install_vpk();

    // a running podman that finds no container with the label
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "no-container")), 45_000);
    let err = call(&server, "run_map", json!({})).await.unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable");
    assert_eq!(err.data["label"], json!("cs2mcp.tool"));
    assert_eq!(err.data["hint"], json!("bridge/runtime.sh up"));
    assert!(err.retry_safe);
    assert!(!tree.payload().exists());

    // and a podman that does not exist at all (the hermetic default). A second mock bridge:
    // the mock holds one authenticated client at a time, like the real bridge.
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb, None, 45_000);
    let err = call(&server, "run_map", json!({})).await.unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable");
    assert!(err.message.contains("cs2mcp.tool"), "{err:?}");
}

#[tokio::test]
async fn a_session_without_the_vtoolmessage_channel_is_runtime_unavailable() {
    let tree = Tree::new("channel-down");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "channel-down")), 45_000);

    let err = call(&server, "run_map", json!({})).await.unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable");
    assert!(err.message.contains("OpenMutex failed"), "{err:?}");
    assert_eq!(err.data["container"], json!("cs2tools"));
    assert!(err.retry_safe);
    assert!(
        std::fs::metadata(tree.console()).map(|m| m.len()).unwrap_or(0) == 0,
        "nothing reached the engine"
    );
}

#[tokio::test]
async fn a_path_or_name_that_cannot_be_a_console_argument_is_refused_locally() {
    let tree = Tree::new("bad-path");
    tree.install_vmap();
    tree.install_vpk();
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    for (path, why) in [
        // `open_map`'s shape gate (shared with `map_content_path`)
        ("csgo_addons/mcp_test/maps/probe.vmap", "maps/"),
        ("/abs/maps/probe.vmap", "relative"),
        ("maps/../x.vmap", ".."),
        ("maps/x.vpcf", "vmap"),
        ("maps/", ".vmap"),
        ("maps/.vmap", "map file name"),
        // the name gate: a console line takes no space, quote or ';'
        ("maps/sub dir/probe.vmap", "character"),
        ("maps/probe;quit.vmap", "character"),
        ("maps/probe\"x.vmap", "quote"),
    ] {
        let err = call(&server, "run_map", json!({"path": path})).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{path}");
        assert_eq!(err.name, "InvalidParams", "{path}");
        assert_eq!(err.code, Some(-32602), "{path}");
        assert!(err.retry_safe, "{path}");
        assert!(err.message.contains(why), "{path}: {err:?}");
    }
    for bad in [
        json!({"gameType": -1}),
        json!({"gameMode": 256}),
        json!({"gameType": "0"}),
        json!({"timeoutMs": 0}),
        json!({"timeoutMs": "1m"}),
    ] {
        let err = call(&server, "run_map", bad.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
    }
    assert!(tree.podman_calls().is_empty(), "nothing was dispatched");
    assert!(!tree.payload().exists());
}

#[tokio::test]
async fn a_document_outside_the_addon_or_without_a_path_names_no_map() {
    // a document path with no host mapping
    let tree = Tree::new("doc-unmapped");
    tree.install_vmap();
    tree.install_vpk();
    let mb = MockBridge::builder()
        .doc(Some(make_doc("q:\\elsewhere\\probe.vmap", Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);
    let err = call(&server, "run_map", json!({})).await.unwrap_err();
    assert_eq!(err.name, "PathNotMapped");
    assert!(err.message.contains("CS2_MCP_PATH_MAP"), "{err:?}");

    // an untitled document
    let mb = MockBridge::builder()
        .doc(Some(make_doc("", Some(true))))
        .hammer_nodes(vec![MockNode::world()])
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);
    let err = call(&server, "run_map", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(err.message.contains("untitled"), "{err:?}");
    assert!(tree.podman_calls().is_empty());
}

// -------------------------------------------------------------------------------------------
// Registration
// -------------------------------------------------------------------------------------------

#[test]
fn run_map_is_a_live_tool_at_its_poc_position() {
    use cs2_mcp_common::tools::{Mutability, Registry};
    let mut registry = Registry::new();
    hammer_mcp::tools::hammer::register(&mut registry);
    let names = registry.names();
    assert_eq!(names[5], "run_map", "POC position: {names:?}");
    assert_eq!(
        names[..hammer_mcp::tools::hammer::POC_TOOL_ORDER.len()],
        hammer_mcp::tools::hammer::POC_TOOL_ORDER,
        "the first 24 tools are the POC's, in the POC's order"
    );
    let spec = registry.spec("run_map").expect("registered");
    // it mutates the engine's state, and it dispatches no bridge method: the route is the
    // VToolMessage console channel, not `map.run` (still `-32011` in the bridge)
    assert_eq!(spec.mutability, Mutability::Mutating);
    assert!(spec.bridge_methods.is_empty(), "{:?}", spec.bridge_methods);
}

// -------------------------------------------------------------------------------------------
// (b) unload: the disconnect primitive compile_map's map-in-game refusal points at
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn unload_posts_only_disconnect_waits_for_the_idle_level_and_clears_the_load_memory() {
    let tree = Tree::new("unload");
    tree.install_vmap();
    tree.install_vpk();
    tree.old_console("10/13 00:00:00 old session\r\n");
    tree.idle_says(idle_level());
    // no game.local on this bridge: the unload still posts the disconnect (the caller asked for it)
    // and the idle-level line is the answer; a later run_map then relies on the CLEARED memory
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let out = call(&server, "run_map", json!({"unload": true})).await.unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["state"], json!("ok"));
    assert_eq!(out["action"], json!("unload"));
    assert_eq!(out["noop"], json!(false));
    assert_eq!(out["disconnected"], json!(true));
    assert_eq!(out["command"], json!("disconnect"));
    assert!(
        out["signal"]
            .as_str()
            .unwrap()
            .contains("Spawn Server: <empty>"),
        "{out}"
    );
    assert!(out["waitedMs"].as_u64().is_some());
    // exactly one post, and it is the disconnect: no map command, no mode lines
    let payloads = tree.payloads();
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    assert!(payloads[0].contains(r#"_data = "disconnect""#), "{payloads:?}");
    assert!(!payloads[0].contains("map "), "{payloads:?}");
    assert_eq!(out["vtmsend"][0]["command"], json!("disconnect"));
    // the load memory is cleared: the engine holds nothing, so the next run_map does not
    // disconnect first (H1's fallback would otherwise disconnect for a map that is not up)
    assert!(
        server.state().last_load.lock().unwrap().is_none(),
        "the unload cleared this server's own load memory"
    );
    tree.engine_says(&loaded("probe"));
    let run = call(&server, "run_map", json!({"path": DOC_ARG})).await.unwrap();
    assert_eq!(run["state"], json!("ok"), "{run}");
    assert_eq!(run["remount"]["needed"], json!(false), "{run}");
    let payloads = tree.payloads();
    assert_eq!(payloads.len(), 2, "only the map command followed: {payloads:?}");
    assert!(payloads[1].contains(r#"_data = "map probe nomapvalidation=1""#), "{payloads:?}");
}

#[tokio::test]
async fn unload_is_a_noop_with_a_note_when_the_engine_is_not_in_a_map() {
    let tree = Tree::new("unload-noop");
    tree.install_vmap();
    tree.install_vpk();
    let mb = mock_bridge(Some(false), Some(json!({"inGame": false, "map": "<empty>"}))).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let out = call(&server, "run_map", json!({"unload": true})).await.unwrap();
    assert_eq!(out["state"], json!("ok"), "{out}");
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["noop"], json!(true), "nothing was sent");
    assert_eq!(out["disconnected"], json!(false));
    assert_eq!(out["mapBefore"], json!("<empty>"));
    assert!(
        out["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("nothing to unload")),
        "{out}"
    );
    assert!(tree.payloads().is_empty(), "nothing was posted");
    assert!(!tree.payload().exists(), "no payload was even written");
}

#[tokio::test]
async fn unload_without_an_idle_signal_is_a_timeout_result() {
    let tree = Tree::new("unload-timeout");
    tree.install_vmap();
    tree.install_vpk();
    // console.log exists (a -condebug session) but the engine never prints the idle level
    tree.old_console("10/13 00:00:00 old session\r\n");
    let mb = mock_bridge(Some(false), Some(map_is_up("probe"))).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let out = call(&server, "run_map", json!({"unload": true, "timeoutMs": 400}))
        .await
        .unwrap();
    assert_eq!(out["state"], json!("timeout"), "{out}");
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["disconnected"], json!(true), "the disconnect was posted");
    assert_eq!(out["signal"], Value::Null);
    assert_eq!(out["timeoutMs"], json!(400), "the caller's own wait governs");
    assert!(
        out["elapsedMs"].as_u64().unwrap() >= 400,
        "it really waited: {out}"
    );
    assert!(
        out["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("no idle-level line")),
        "{out}"
    );
    assert_eq!(tree.payloads().len(), 1, "one disconnect, never repeated");
}

#[tokio::test]
async fn unload_that_never_comes_back_is_failed_and_never_repeated() {
    let tree = Tree::new("unload-unknown");
    tree.install_vmap();
    tree.install_vpk();
    let mb = mock_bridge(Some(false), Some(map_is_up("probe"))).await;
    // the runtime kills the podman client at the (test-shortened) short timeout: the post may or may not have
    // been delivered (M6), so this is a failure of the call, not a `timeout` that says "retry"
    let server = build(&tree, &mb, Some(fake_podman(&tree, "exec-timeout")), 45_000);
    // this server's own mount memory says a map is up (the H1 fallback): an unconfirmed unload must
    // KEEP it, so a later run_map still disconnects before its map command
    server
        .state()
        .last_load
        .lock()
        .unwrap()
        .replace(hammer_mcp::tools::run_map::LastLoad {
            name: "probe".into(),
            vpk_mtime_ms: None,
            console_offset: 0,
            resolved: true,
            state: "ok".into(),
            idle: false,
            in_flight_until: std::time::Instant::now(),
        });

    let out = call(&server, "run_map", json!({"unload": true, "timeoutMs": 500}))
        .await
        .unwrap();
    assert_eq!(out["state"], json!("failed"), "{out}");
    assert_eq!(out["vtmsend"][0]["delivered"], json!(false));
    assert_eq!(out["vtmsend"][0]["retried"], json!(false), "never repeated");
    assert_eq!(tree.payloads().len(), 1, "one post: {:?}", tree.payloads());
    assert!(
        out["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("NOT repeated")),
        "{out}"
    );
    assert!(
        server.state().last_load.lock().unwrap().is_some(),
        "an unconfirmed unload clears nothing"
    );
}

#[tokio::test]
async fn unload_rejects_a_path_or_a_mode_argument() {
    let tree = Tree::new("unload-args");
    tree.install_vmap();
    tree.install_vpk();
    let mb = mock_bridge(Some(false), Some(map_is_up("probe"))).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    for args in [
        json!({"unload": true, "path": DOC_ARG}),
        json!({"unload": true, "gameType": 0}),
        json!({"unload": true, "gameMode": 1}),
        json!({"unload": "yes"}),
    ] {
        let err = call(&server, "run_map", args.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{args}");
        assert_eq!(err.name, "InvalidParams", "{args}");
        assert_eq!(err.code, Some(-32602), "{args}");
        assert!(err.retry_safe, "{args}");
    }
    assert!(tree.payloads().is_empty(), "nothing was sent");
    assert!(tree.podman_calls().is_empty(), "not even a container lookup");
}

#[tokio::test]
async fn unload_false_is_a_normal_load() {
    let tree = Tree::new("unload-false");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let out = call(&server, "run_map", json!({"unload": false, "path": DOC_ARG}))
        .await
        .unwrap();
    assert_eq!(out["state"], json!("ok"), "{out}");
    assert_eq!(out["action"], Value::Null, "a load has no action field");
    assert_eq!(out["command"], json!("map probe nomapvalidation=1"));
    assert_eq!(tree.payloads().len(), 1, "{:?}", tree.payloads());
}

// -------------------------------------------------------------------------------------------
// (c) the default watch fits the tool budget silently
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_default_watch_fits_the_budget_without_a_clamp_note() {
    let tree = Tree::new("budget-default");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says("11/02 10:00:00 Loading map \"probe\"\r\n");
    let mb = mock_bridge(Some(false), None).await;
    // the harness budget (5 s) is smaller than the 60 s default: the default must be shortened
    // SILENTLY — `timeoutMs` reports what was really watched and there is no clamp note
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 5_000);

    let started = std::time::Instant::now();
    let out = call(&server, "run_map", json!({})).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(out["state"], json!("timeout"), "{out}");
    assert_eq!(out["budgetClamped"], json!(false), "{out}");
    let watched = out["timeoutMs"].as_u64().unwrap();
    // relative, not absolute: a slow fake podman eats into the budget before the watch starts,
    // so only "cut to the budget, and the call ran to the budget" holds at any host load
    assert!(watched <= 5_000, "the default was cut to the budget: {out}");
    // the call never overruns its budget; it may end before it (the server keeps a margin for the
    // final reads, and a slow fake podman shortens the watch), so no lower bound is asserted
    assert!(
        elapsed <= std::time::Duration::from_millis(5_000 + 1_500),
        "the call overran its 5 s budget: {elapsed:?}: {out}"
    );
    assert!(
        !out["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("cut short by the tool budget")),
        "a default call must not carry a budget note: {out}"
    );

    // ... while an EXPLICIT wait the budget cannot honour is still flagged (a fresh server: the
    // previous call's unresolved load would refuse this one)
    let tree = Tree::new("budget-explicit");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says("11/02 10:00:00 Loading map \"probe\"\r\n");
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 2_000);
    let out = call(&server, "run_map", json!({"timeoutMs": 600_000}))
        .await
        .unwrap();
    assert_eq!(out["state"], json!("timeout"), "{out}");
    assert_eq!(out["budgetClamped"], json!(true), "{out}");
    assert!(out["timeoutMs"].as_u64().unwrap() <= 2_000, "{out}");
    assert!(
        out["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("cut short by the tool budget")),
        "{out}"
    );
}

// -------------------------------------------------------------------------------------------
// review-2 (FIX-3): the fresh in-gate game.local / offset (N2), the never-posted-record rule
// (N3), the NoLocalPlayer answer (live TODO), the top-level-source flattened rule (M8), and the
// no-op that must not drop an unresolved record (N9)
// -------------------------------------------------------------------------------------------

/// The LIVE semantics (2026-10-15, live/run-3.log): the engine answers `-32024 NoLocalPlayer`
/// when it is in no map at all — right after a disconnect that is exactly what `game.local`
/// answers. It must read as `inGame:false`, not as an unknown: an unload is then a no-op.
#[tokio::test]
async fn the_no_local_player_answer_means_not_in_a_map() {
    let tree = Tree::new("no-local-player");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .error("game.local", -32024, "the engine has no local player", None)
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 120_000);

    {
        let out = call(&server, "run_map", json!({})).await.expect("the load proceeds");
        assert_eq!(out["state"], json!("ok"), "{out}");
        // both game.local reads RESOLVED to inGame:false, map null, with NO error payload
        for key in ["gameLocalBefore", "gameLocal"] {
            assert_eq!(out[key]["inGame"], json!(false), "{key}: {out}");
            assert_eq!(out[key]["map"], Value::Null, "{key}: {out}");
            assert_eq!(out[key].get("error"), None, "{key}: the error must not surface");
        }
        assert_eq!(out["remount"]["needed"], json!(false), "{out}");
        let payloads = tree.payloads();
        assert_eq!(payloads.len(), 1, "no remount on a NoLocalPlayer answer: {payloads:?}");
    }

    // ... and the unload is the no-op it says (the LIVE "2nd unload" case: game.local answered
    // -32024 and the disconnect re-sent needlessly — that was the old shape)
    let out = call(&server, "run_map", json!({"unload": true})).await.unwrap();
    assert_eq!(out["state"], json!("ok"), "{out}");
    assert_eq!(out["noop"], json!(true), "{out}");
    assert_eq!(out["disconnected"], json!(false), "nothing was sent: {out}");
    assert!(tree.payloads().len() <= 1, "{:?}", tree.payloads());
}

/// **N3**: a POST that fails BEFORE delivery (`OpenMutex failed`, the common just-started
/// session case) must leave NO unresolved load record behind: the old shape wrote one before the
/// first post, so every later `run_map` — including `run_map {unload:true}`, the recovery
/// primitive — was refused `EngineBusy` until the in-flight window ran out.
#[tokio::test]
async fn a_failed_map_post_leaves_no_unresolved_load_record() {
    let tree = Tree::new("n3-post-err");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "channel-down")), 45_000);

    let err = call(&server, "run_map", json!({})).await.unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable", "{err:?}");
    assert!(
        err.message.contains("OpenMutex failed"),
        "the definite non-delivery case: {err:?}"
    );
    assert!(
        server.state().last_load.lock().unwrap().is_none(),
        "nothing was sent, so no in-flight load record may exist"
    );
    // the very next call is NOT locked out by `EngineBusy`: it fails for the same concrete
    // reason, which is retry-safe — never an excuse-window instead of a message
    let err = call(&server, "run_map", json!({})).await.unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable", "the N3 lockout would say EngineBusy: {err:?}");
}

/// **N3** (failed remount) + the idle settlement (N2's fresh offset): a `disconnect` whose idle
/// line never SHOWED within the wait records the IDLE level as its settle target (no `map` was
/// ever posted, so a `Loading map "<name>"` line will never settle it). Once the engine then
/// prints its idle line, a later call settles at it and proceeds — instead of refusing
/// `EngineBusy` for the whole record window.
#[tokio::test]
async fn a_failed_remount_record_settles_at_the_idle_level() {
    let tree = Tree::new("n3-idle-record");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    // console.log exists (a -condebug session) but the engine never prints the idle level in
    // this call's wait
    tree.old_console("10/13 00:00:00 old session\r\n");
    // game.local answers `a map is up` for the FIRST call (its gate read + remount), then keeps
    // answering WITHOUT `inGame` (an unreadable post-disconnect bridge): the second call's
    // settle must then use the CONSOLE's idle line, never a `game.local` verdict
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .responder("game.local", {
            let reads = Arc::new(AtomicUsize::new(0));
            move |_| {
                if reads.fetch_add(1, Ordering::Relaxed) == 0 {
                    map_is_up("probe")
                } else {
                    json!({"note": "control ping hung after the disconnect"})
                }
            }
        })
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 2_500);

    let first = call(&server, "run_map", json!({"timeoutMs": 700})).await.unwrap();
    assert_eq!(first["state"], json!("failed"), "the remount never parked: {first}");
    {
        let record = server.state().last_load.lock().unwrap().clone();
        let record = record.expect("an unresolved record");
        assert!(record.idle, "the record's settle target is the idle level: {record:?}");
        assert!(!record.resolved, "{record:?}");
    }

    // the engine then FINISHES parking: the idle line lands in the console (out of band here,
    // exactly what the engine does a moment after the wait gave up)
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(tree.console())
            .expect("console.log");
        file.write_all(idle_level().as_bytes()).expect("the idle line");
    }

    // the next call settles the record AT THE IDLE LINE (never EngineBusy) and proceeds with
    // its own load
    let second = call(&server, "run_map", json!({}))
        .await
        .expect("the failed remount is settled at the idle level");
    assert_eq!(second["state"], json!("ok"), "{second}");
    let settled = second["settledPreviousLoad"].as_object().expect("it was settled");
    assert_eq!(settled["map"], json!("probe"));
    assert_eq!(settled["idle"], json!(true));
    assert_eq!(settled["settled"], json!(false), "idle-level = no verdict, but settled");
    let payloads = tree.payloads();
    assert!(
        payloads.iter().any(|p| p.contains(r#"_data = "map probe nomapvalidation=1""#)),
        "the settled record did not block the load: {payloads:?}"
    );
    // the store record is resolved afterwards
    let record = server.state().last_load.lock().unwrap().clone().unwrap();
    assert!(record.resolved, "{record:?}");
}

/// **M8** (the completed flattened rule): a flattened `maps/<stem>.vpk` that is even NEWER than
/// its `.vmap` is still refused when a top-level `maps/<stem>.vmap` SOURCE exists — that vpk
/// could be the top-level map's compile, and running it under this map's name would silently
/// load the wrong map.
#[tokio::test]
async fn a_newer_flattened_vpk_with_a_top_level_source_of_the_same_stem_is_refused() {
    let tree = Tree::new("m8-top-source");
    let sub = tree.maps_dir().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("probe.vmap"), b"fake vmap").unwrap();
    // the top-level SOURCE map (same stem) ...
    tree.install_vmap();
    // ... then the top-level compile, which is NEWER than the sub map's source
    std::thread::sleep(std::time::Duration::from_millis(20));
    tree.install_vpk();
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);

    let err = call(&server, "run_map", json!({"path": "maps/sub/probe.vmap"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "VpkNotFound", "{err:?}");
    assert_eq!(err.data["flattenedRejected"], json!("top-level-source"), "{err:?}");
    assert!(
        err.message.contains("top-level"),
        "the reason is spelled out: {err:?}"
    );
    assert!(tree.podman_calls().is_empty(), "nothing was dispatched");
}

/// **N9**: the no-op unload must not silently drop an unresolved record — `game.local` can
/// answer inGame=false while a load of this server is in a non-frozen phase. A record whose
/// window is still LIVE is settled first (which may refuse `EngineBusy`); a STALE one (its
/// window is over) settles (`Ok(None)`) and the no-op proceeds BUT the record is kept, so a
/// later `run_map` still settles it before it posts.
#[tokio::test]
async fn the_unload_noop_keeps_an_unresolved_record() {
    let tree = Tree::new("n9-noop-live");
    tree.install_vmap();
    tree.install_vpk();
    let mb = mock_bridge(Some(false), Some(json!({"inGame": false, "map": "<empty>"}))).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 2_001);
    // an unresolved record of a load that posted `map probe` and never showed a verdict
    server
        .state()
        .last_load
        .lock()
        .unwrap()
        .replace(hammer_mcp::tools::run_map::LastLoad {
            name: "probe".into(),
            vpk_mtime_ms: None,
            console_offset: 0,
            resolved: false,
            state: "unknown".into(),
            idle: false,
            in_flight_until: std::time::Instant::now() + std::time::Duration::from_secs(60),
        });

    let err = call(&server, "run_map", json!({"unload": true, "timeoutMs": 300}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "EngineBusy", "the live record is settled, then refused: {err:?}");
    assert!(
        server.state().last_load.lock().unwrap().is_some(),
        "the record survived the refused call"
    );

    // ... the same unanswered record once STALE (its in-flight window is over): the settle
    // answers stale instead of busy, the no-op proceeds — and the record is KEPT (a later
    // run_map settles it again rather than trusting anything). A fresh mock: one bridge client
    // per mock, and the first server still holds its socket.
    let mb2 = mock_bridge(Some(false), Some(json!({"inGame": false, "map": "<empty>"}))).await;
    let server = build(&tree, &mb2, Some(fake_podman(&tree, "ok")), 45_000);
    server
        .state()
        .last_load
        .lock()
        .unwrap()
        .replace(hammer_mcp::tools::run_map::LastLoad {
            name: "probe".into(),
            vpk_mtime_ms: None,
            console_offset: 0,
            resolved: false,
            state: "unknown".into(),
            idle: false,
            in_flight_until: std::time::Instant::now() - std::time::Duration::from_secs(1),
        });
    let out = call(&server, "run_map", json!({"unload": true, "timeoutMs": 300}))
        .await
        .unwrap();
    assert_eq!(out["state"], json!("ok"), "{out}");
    assert_eq!(out["noop"], json!(true), "{out}");
    assert_eq!(out["disconnected"], json!(false), "{out}");
    let settled = out["settledPreviousLoad"].as_object().expect("the stale settle is reported");
    assert_eq!(settled["settled"], Value::Null, "stale, not settled: {settled:?}");
    assert!(
        server.state().last_load.lock().unwrap().is_some(),
        "an unload that did not settle the record clears nothing (N9)"
    );
}

/// A mock engine for `team`: no map before the load, then a map with the local player on team 0
/// and dead; `jointeam N 1` (console.exec) puts it on the team and, when `spawns_on_join`, alive;
/// otherwise only `mp_restartgame 1` spawns it.
async fn team_engine(spawns_on_join: bool) -> MockBridge {
    let reads = Arc::new(AtomicUsize::new(0));
    let team = Arc::new(AtomicUsize::new(0));
    let alive = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (team_c, alive_c) = (team.clone(), alive.clone());
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .responder("console.exec", move |p| {
            let line = p["command"].as_str().unwrap_or("");
            if let Some(n) = line.strip_prefix("jointeam ").and_then(|r| r.split(' ').next()) {
                team_c.store(n.parse().unwrap(), Ordering::SeqCst);
                alive_c.store(spawns_on_join, Ordering::SeqCst);
            }
            if line.starts_with("mp_restartgame") {
                alive_c.store(true, Ordering::SeqCst);
            }
            json!({"queued": true})
        })
        .responder("game.local", move |_| {
            if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                return json!({"inGame": false});
            }
            json!({"inGame": true, "map": "probe", "playerSlot": 0,
                   "player": {"team": team.load(Ordering::SeqCst), "alive": alive.load(Ordering::SeqCst)}})
        })
        .start()
        .await
        .unwrap()
}

#[tokio::test]
async fn run_map_team_joins_and_restarts_until_the_player_is_alive() {
    let tree = Tree::new("team-restart");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    let mb = team_engine(false).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 120_000);

    let out = call(&server, "run_map", json!({"team": "ct"})).await.unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    let team = &out["team"];
    assert_eq!(team["requested"], json!("ct"), "{out}");
    assert_eq!(team["joined"], json!(true), "{team}");
    assert_eq!(team["alive"], json!(true), "{team}");
    assert_eq!(team["restartedGame"], json!(true), "{team}");
    let sent: Vec<String> = mb
        .requests()
        .into_iter()
        .filter(|r| r.method == "console.exec")
        .filter_map(|r| r.params["command"].as_str().map(str::to_string))
        .filter(|c| !c.starts_with("cs2mcp_mark"))
        .collect();
    assert_eq!(sent, ["jointeam 3 1", "mp_restartgame 1"], "same route as console_exec: {sent:?}");
    assert!(out["notes"].as_array().unwrap().iter().all(|n| !n.as_str().unwrap().contains("team 0")), "{out}");
}

#[tokio::test]
async fn run_map_restart_if_dead_false_never_restarts_the_game() {
    let tree = Tree::new("team-norestart");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    let mb = team_engine(false).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 120_000);

    let out = call(&server, "run_map", json!({"team": "ct", "restartIfDead": false})).await.unwrap();
    let team = &out["team"];
    assert_eq!((team["joined"].clone(), team["alive"].clone(), team["restartedGame"].clone()), (json!(true), json!(false), json!(false)), "{out}");
    assert!(team["note"].as_str().unwrap().contains("restartIfDead is false"), "{team}");
    let restarts = mb
        .requests()
        .into_iter()
        .filter(|r| r.method == "console.exec")
        .filter(|r| r.params["command"].as_str().is_some_and(|c| c.starts_with("mp_restartgame")))
        .count();
    assert_eq!(restarts, 0);
    // restartIfDead alone is refused
    let e = call(&server, "run_map", json!({"restartIfDead": false})).await.unwrap_err();
    assert_eq!(e.name, "InvalidParams");
    // and the schema names the restart
    let spec = server.registry().spec("run_map").unwrap();
    assert!(spec.input_schema["properties"]["restartIfDead"]["description"]
        .as_str()
        .is_some_and(|d| d.contains("mp_restartgame 1")));
}

#[tokio::test]
async fn run_map_team_with_an_unconfirmed_load_reports_the_skipped_join() {
    let tree = Tree::new("team-skipped");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says("11/02 10:00:00 Loading map \"probe\"\r\n11/02 10:00:01 Failed to load map\r\n");
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 45_000);
    let out = call(&server, "run_map", json!({"team": "t"})).await.unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["team"]["requested"], json!("t"), "{out}");
    assert_eq!(out["team"]["skipped"], json!("load not confirmed"), "{out}");
    assert!(mb.requests().iter().all(|r| r.params["command"].as_str().is_none_or(|c| !c.starts_with("jointeam"))));
}

#[tokio::test]
async fn run_map_team_t_that_spawns_on_join_is_not_restarted() {
    let tree = Tree::new("team-t");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    let mb = team_engine(true).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 120_000);

    let out = call(&server, "run_map", json!({"team": "t"})).await.unwrap();
    let team = &out["team"];
    assert_eq!((team["joined"].clone(), team["alive"].clone(), team["restartedGame"].clone()), (json!(true), json!(true), json!(false)), "{out}");
    let joins: Vec<String> = mb
        .requests()
        .into_iter()
        .filter(|r| r.method == "console.exec")
        .filter_map(|r| r.params["command"].as_str().map(str::to_string))
        .filter(|c| c.starts_with("jointeam"))
        .collect();
    assert_eq!(joins, ["jointeam 2 1"]);
}

#[tokio::test]
async fn run_map_without_team_says_where_the_player_stands_and_a_bad_team_is_refused_first() {
    let tree = Tree::new("team-none");
    tree.install_vmap();
    tree.install_vpk();
    tree.engine_says(&loaded("probe"));
    let mb = mock_bridge(Some(false), None).await;
    let server = build(&tree, &mb, Some(fake_podman(&tree, "ok")), 120_000);

    let bad = call(&server, "run_map", json!({"team": "spec"})).await.unwrap_err();
    assert_eq!(bad.name, "InvalidParams");
    let unload = call(&server, "run_map", json!({"unload": true, "team": "ct"})).await.unwrap_err();
    assert_eq!(unload.name, "InvalidParams");
    assert!(tree.payloads().is_empty(), "nothing sent for a refused call");

    let out = call(&server, "run_map", json!({})).await.unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    assert!(out.get("team").is_none());
    assert!(out["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("team 0")), "{out}");
}
