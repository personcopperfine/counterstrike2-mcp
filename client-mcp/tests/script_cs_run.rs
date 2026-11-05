//! `script_run {kind: "cs_script"}` against a fake podman and a mock bridge: a compiled addon map is
//! launched (launch_session map_workshop), an optional entry is fired with a confirmed console
//! command, and the expects are judged on the console lines. A run that does not match is a result
//! with passed false, not an error; a wait the call budget cut short is pending.
#![allow(clippy::result_large_err)]
use client_mcp::state::ClientConfig;
use cs2_mcp_common::mock::podman::FakePodman;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Server = cs2_mcp_common::mcp::McpServer<client_mcp::state::ClientState>;

mod common;

fn config(args: &[&str]) -> ClientConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: ClientConfig,
    }
    let mut argv = vec!["t"];
    argv.extend(common::hermetic(args));
    W::parse_from(argv).c
}

fn server_with(fake: &FakePodman, project: &Path, budget_ms: Option<u64>) -> Server {
    let bin = fake.bin.display().to_string();
    let proj = project.display().to_string();
    let disc = wine_bridge_dir(project)
        .join("session.json")
        .display()
        .to_string();
    let budget = budget_ms.map(|b| b.to_string());
    let mut argv = vec![
        "--discovery",
        disc.as_str(),
        "--podman",
        bin.as_str(),
        "--project-root",
        proj.as_str(),
        "--max-sessions",
        "3",
    ];
    if let Some(b) = &budget {
        argv.extend(["--tool-budget-ms", b.as_str()]);
    }
    client_mcp::build_server(&config(&argv)).unwrap()
}

async fn call(server: &Server, name: &str, args: Value) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

/// The fake engine's console.log once the harness parked, with one script line written at load.
const HELLO_LOG: &[u8] =
    b"TESTSCRIPT: CheckPoint -> 'VTEST_PARKED'\n10/12 08:23:56 [cs_script] Hello cs_script!\n";
/// Same, but the only script line is printed while the map loads (before any entry fires).
const LOAD_LOG: &[u8] =
    b"TESTSCRIPT: CheckPoint -> 'VTEST_PARKED'\n10/12 08:23:56 [cs_script] Loaded setup\n";

/// `cs_script_demo`'s own map, where `map_workshop` loads it from.
fn own_addon_map(proj: &Path) {
    let dir = proj.join("workshop/run/app/game/csgo_addons/cs_script_demo/maps");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cs_script_demo.vpk"), "vpk").unwrap();
}

fn wine_bridge_dir(proj: &Path) -> PathBuf {
    proj.join(
        "workshop/run/app/compatdata/730/pfx/drive_c/users/steamuser/AppData/Local/cs2mcpbridge",
    )
}

fn console_path(proj: &Path) -> PathBuf {
    proj.join("workshop/run/app/game/csgo/console.log")
}

/// The wine-tools container the fake podman started, once its state file exists.
fn started_wine(state: &Path) -> Option<String> {
    std::fs::read_dir(state)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("cs2mcp-wine-tools-") && !n.ends_with(".mounts"))
}

/// The fake engine, played once the fake podman has started the container: it records the app's
/// mount (the entry gate reads the session's app dir from it), writes console.log when there is
/// none (a fresh launch's rotation is what a real engine does; a stale file is left alone), and
/// starts a mock bridge in the app's discovery dir. The bridge's `game.local` reports `local_map`
/// in game. Its console.exec writes the engine's lines: the confirmation marker prints its
/// `Unknown command` warning, an `unknown_*` command prints the same warning (the engine does not
/// know it), and any other command prints `fire_prints` (the script's output).
fn engine(
    proj: &Path,
    state: PathBuf,
    console_text: &'static [u8],
    fire_prints: Option<&'static str>,
    local_map: Arc<Mutex<String>>,
) -> tokio::task::JoinHandle<Option<MockBridge>> {
    let app = proj.join("workshop/run/app");
    let console = console_path(proj);
    let disc = wine_bridge_dir(proj);
    tokio::spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(30);
        let name = loop {
            if let Some(name) = started_wine(&state) {
                break name;
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let mount = json!([{
            "Type": "bind",
            "Name": "",
            "Source": app.display().to_string(),
            "Destination": "/app"
        }]);
        std::fs::write(state.join(format!("{name}.mounts")), mount.to_string()).unwrap();
        std::fs::create_dir_all(console.parent().unwrap()).unwrap();
        if !console.exists() {
            std::fs::write(&console, console_text).unwrap();
        }
        let exec_log = console.clone();
        let builder = MockBridge::builder()
            .methods([
                "control.ping",
                "control.status",
                "control.capabilities",
                "control.version",
                "game.local",
                "console.exec",
            ])
            .responder("game.local", move |_| {
                let map = local_map.lock().unwrap().clone();
                json!({"tick": 5, "inGame": true, "map": map})
            })
            .responder("console.exec", move |params| {
                let command = params["command"].as_str().unwrap_or_default().to_string();
                let printed =
                    if command.starts_with("cs2mcp_mark_") || command.starts_with("unknown_") {
                        Some(format!("Unknown command: {command}\n"))
                    } else {
                        fire_prints.map(|t| format!("{t}\n"))
                    };
                if let Some(text) = printed {
                    let mut f = std::fs::OpenOptions::new()
                        .append(true)
                        .open(&exec_log)
                        .unwrap();
                    f.write_all(text.as_bytes()).unwrap();
                }
                json!({"queued": true})
            });
        Some(builder.discovery_dir(disc).start().await.unwrap())
    })
}

fn local_on(map: &str) -> Arc<Mutex<String>> {
    Arc::new(Mutex::new(map.to_string()))
}

/// A project with the addon's map and the fake engine's app tree, and a server for it.
fn setup(tag: &str, budget_ms: Option<u64>) -> (FakePodman, PathBuf, Server) {
    let fake = FakePodman::new(tag);
    let proj = fake.dir().join("project");
    std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
    own_addon_map(&proj);
    let srv = server_with(&fake, &proj, budget_ms);
    fake.set_exec_default("vtmsend: creator version 1\n", 0);
    (fake, proj, srv)
}

fn run_args(extra: Value) -> Value {
    let mut v = json!({"kind": "cs_script", "addon": "cs_script_demo", "expect": ["Hello cs_script!"], "timeoutMs": 20000});
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    v
}

#[tokio::test]
async fn cs_script_run_passes_when_the_expects_match_the_addons_lines() {
    let (fake, proj, srv) = setup("script-run-pass", None);
    let engine = engine(
        &proj,
        fake.dir().join("state"),
        HELLO_LOG,
        None,
        local_on("cs_script_demo"),
    );
    let out = call(&srv, "script_run", run_args(json!({}))).await.unwrap();
    let mock = engine.await.unwrap().expect("the mock bridge came up");
    assert_eq!(out["passed"], json!(true), "{out}");
    assert_eq!(out["reason"], json!(null));
    assert_eq!(out["map"], json!("cs_script_demo"));
    assert_eq!(out["mapVerified"], json!(true), "{out}");
    assert_eq!(out["segmentFrom"], json!("session-start"));
    assert_eq!(out["matched"][0]["regex"], json!("Hello cs_script!"));
    assert_eq!(
        out["matched"][0]["line"],
        json!("10/12 08:23:56 [cs_script] Hello cs_script!")
    );
    assert_eq!(
        out["scriptLines"],
        json!(["10/12 08:23:56 [cs_script] Hello cs_script!"])
    );
    assert_eq!(out["scriptLinesTruncated"], json!(0));
    assert!(out["errors"].as_array().unwrap().is_empty());
    assert_eq!(out["timedOut"], json!(false));
    assert_eq!(out["sessionLeftRunning"], json!(true));
    assert!(out["session"]
        .as_str()
        .unwrap()
        .starts_with("cs2mcp-wine-tools-"));
    assert!(!mock.method_calls("game.local").is_empty());
}

#[tokio::test]
async fn cs_script_run_that_never_matches_is_passed_false_with_the_script_lines_not_an_error() {
    let (fake, proj, srv) = setup("script-run-miss", None);
    let engine = engine(
        &proj,
        fake.dir().join("state"),
        HELLO_LOG,
        None,
        local_on("cs_script_demo"),
    );
    let out = call(
        &srv,
        "script_run",
        run_args(json!({"expect": ["Goodbye"], "timeoutMs": 1500})),
    )
    .await
    .unwrap();
    let _bridge = engine.await.unwrap().expect("the mock bridge came up");
    assert_eq!(out["passed"], json!(false), "{out}");
    assert_eq!(out["pending"], json!(false));
    assert_eq!(
        out["reason"],
        json!("timeoutMs expired before every expect matched")
    );
    assert_eq!(out["unmatched"], json!(["Goodbye"]));
    assert_eq!(out["matched"], json!([]));
    assert_eq!(out["timedOut"], json!(true));
    assert_eq!(
        out["scriptLines"],
        json!(["10/12 08:23:56 [cs_script] Hello cs_script!"])
    );
}

#[tokio::test]
async fn cs_script_entry_fires_after_load_and_only_its_output_is_judged() {
    let (fake, proj, srv) = setup("script-run-entry", None);
    let engine = engine(
        &proj,
        fake.dir().join("state"),
        LOAD_LOG,
        Some("10/12 08:23:56 [cs_script] Hello cs_script!"),
        local_on("cs_script_demo"),
    );
    let entry = json!({"target": "hello", "input": "ScaleUp"});
    let fired = call(
        &srv,
        "script_run",
        run_args(json!({"entry": entry, "timeoutMs": 20000})),
    )
    .await
    .unwrap();
    let mock = engine.await.unwrap().expect("the mock bridge came up");
    assert_eq!(fired["passed"], json!(true), "{fired}");
    assert_eq!(fired["segmentFrom"], json!("entry"));
    assert_eq!(
        fired["entry"]["command"],
        json!("ent_fire hello RunScriptInput ScaleUp")
    );
    assert_eq!(fired["entry"]["confirmed"], json!(true));
    assert_eq!(
        fired["scriptLines"],
        json!(["10/12 08:23:56 [cs_script] Hello cs_script!"])
    );
    assert_eq!(mock.method_calls("console.exec").len(), 2, "fire + marker");

    // the load-time line is before the trigger: an expect only it satisfies does not match
    let before = call(
        &srv,
        "script_run",
        run_args(json!({"entry": entry, "expect": ["Loaded setup"], "timeoutMs": 1500})),
    )
    .await
    .unwrap();
    assert_eq!(before["passed"], json!(false), "{before}");
    assert_eq!(before["reused"], json!(true));
    assert_eq!(before["unmatched"], json!(["Loaded setup"]));
}

#[tokio::test]
async fn cs_script_entry_can_be_a_registered_console_command() {
    let (fake, proj, srv) = setup("script-run-command", None);
    let engine = engine(
        &proj,
        fake.dir().join("state"),
        LOAD_LOG,
        Some("10/12 08:23:56 [cs_script] Usage: script_zoo_trace <type> - traces"),
        local_on("cs_script_demo"),
    );
    let out = call(
        &srv,
        "script_run",
        run_args(json!({"entry": {"command": "script_zoo_trace"}, "expect": ["Usage: script_zoo_trace"]})),
    )
    .await
    .unwrap();
    let mock = engine.await.unwrap().expect("the mock bridge came up");
    assert_eq!(out["passed"], json!(true), "{out}");
    assert_eq!(out["entry"]["command"], json!("script_zoo_trace"));
    assert_eq!(out["entry"]["confirmed"], json!(true));
    assert_eq!(
        mock.method_calls("console.exec").len(),
        2,
        "command + marker"
    );
}

#[tokio::test]
async fn an_entry_the_engine_does_not_know_fails_at_once_with_its_warning() {
    let (fake, proj, srv) = setup("script-run-unknown", None);
    let engine = engine(
        &proj,
        fake.dir().join("state"),
        HELLO_LOG,
        None,
        local_on("cs_script_demo"),
    );
    let started = Instant::now();
    let out = call(
        &srv,
        "script_run",
        run_args(json!({"entry": {"command": "unknown_cheat"}, "timeoutMs": 60000})),
    )
    .await
    .unwrap();
    let _bridge = engine.await.unwrap().expect("the mock bridge came up");
    assert_eq!(out["passed"], json!(false), "{out}");
    assert!(out["reason"]
        .as_str()
        .unwrap()
        .contains("did not know the entry command"));
    assert_eq!(out["timedOut"], json!(false));
    assert!(out["entry"]["warnings"][0]
        .as_str()
        .unwrap()
        .contains("Unknown command: unknown_cheat"));
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "must not wait out timeoutMs"
    );
}

#[tokio::test]
async fn a_forbidden_entry_command_is_refused_before_anything_is_launched() {
    let (fake, proj, srv) = setup("script-run-forbidden", None);
    let err = call(
        &srv,
        "script_run",
        run_args(json!({"entry": {"command": "quit"}})),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments", "{err:?}");
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    assert!(!console_path(&proj).exists());
}

#[tokio::test]
async fn a_passing_run_on_a_reused_session_without_an_entry_sees_the_load_lines() {
    // the first run launches the session; the second reuses it, and its segment is still the whole
    // log since the load, so the load-time line is judged again and passes
    let (fake, proj, srv) = setup("script-run-reuse", None);
    let engine = engine(
        &proj,
        fake.dir().join("state"),
        HELLO_LOG,
        None,
        local_on("cs_script_demo"),
    );
    let first = call(&srv, "script_run", run_args(json!({}))).await.unwrap();
    let _bridge = engine.await.unwrap().expect("the mock bridge came up");
    assert_eq!(first["passed"], json!(true), "{first}");
    let second = call(&srv, "script_run", run_args(json!({}))).await.unwrap();
    assert_eq!(second["reused"], json!(true), "{second}");
    assert_eq!(second["passed"], json!(true), "{second}");
    assert_eq!(second["segmentFrom"], json!("session-start"));
}

#[tokio::test]
async fn a_run_that_is_pending_then_retried_passes_once_the_session_is_ready() {
    let (fake, proj, srv) = setup("script-run-pending", None);
    // no engine yet: the session is not ready inside timeoutMs, so nothing is fired
    let args = run_args(json!({"timeoutMs": 1500}));
    let pending = call(&srv, "script_run", args.clone()).await.unwrap();
    assert_eq!(pending["pending"], json!(true), "{pending}");
    assert_eq!(pending["passed"], json!(null));
    assert!(pending["next"]
        .as_str()
        .unwrap()
        .contains("script_run again"));
    let engine = engine(
        &proj,
        fake.dir().join("state"),
        HELLO_LOG,
        None,
        local_on("cs_script_demo"),
    );
    let retry = call(&srv, "script_run", run_args(json!({}))).await.unwrap();
    let _bridge = engine.await.unwrap().expect("the mock bridge came up");
    assert_eq!(retry["reused"], json!(true), "{retry}");
    assert_eq!(retry["passed"], json!(true), "{retry}");
}

#[tokio::test]
async fn a_reused_session_on_another_map_is_map_load_failed_not_judged() {
    let (fake, proj, srv) = setup("script-run-map", None);
    let local = local_on("cs_script_demo");
    let engine = engine(
        &proj,
        fake.dir().join("state"),
        HELLO_LOG,
        None,
        Arc::clone(&local),
    );
    let first = call(&srv, "script_run", run_args(json!({}))).await.unwrap();
    let _bridge = engine.await.unwrap().expect("the mock bridge came up");
    assert_eq!(first["passed"], json!(true), "{first}");
    // the game moved on: the reused session no longer shows the addon's map in game
    *local.lock().unwrap() = "de_dust2".to_string();
    let err = call(&srv, "script_run", run_args(json!({})))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "MapLoadFailed", "{err:?}");
    assert_eq!(err.data["map"], json!("cs_script_demo"));
    assert_eq!(err.data["commandsSent"], json!(false));
}

#[tokio::test]
async fn a_console_log_the_launch_cannot_clear_fails_the_launch_naming_the_path() {
    // the launch must not run with an earlier session's log: it moves console.log aside, removes
    // it when the rename fails, and only fails the launch when it can do neither — here the log
    // is a directory (cannot be renamed or removed) and a directory holds the .prev name too
    let (_fake, proj, srv) = setup("script-run-stale", None);
    std::fs::create_dir_all(console_path(&proj).parent().unwrap()).unwrap();
    // a non-empty directory holds the .prev name (an empty one would be rename(2)'s target), and
    // the log itself is a directory: not a rename target, not remove_file-able
    std::fs::create_dir_all(console_path(&proj).with_extension("log.prev")).unwrap();
    std::fs::write(console_path(&proj).with_extension("log.prev").join("old"), "x").unwrap();
    std::fs::create_dir_all(console_path(&proj)).unwrap();
    let err = call(&srv, "script_run", run_args(json!({}))).await.unwrap_err();
    assert_eq!(err.type_, "RuntimeError", "{err:?}");
    assert!(
        err.message.contains("console.log"),
        "the error must name the path: {err:?}"
    );
    assert!(!err.retry_safe, "{err:?}");
}

#[tokio::test]
async fn a_wait_the_call_budget_cuts_short_is_pending_not_failed() {
    // a budget too small for timeoutMs: the expects never match, and the wait ends at the budget
    let (fake, proj, srv) = setup("script-run-budget", Some(3000));
    let engine = engine(
        &proj,
        fake.dir().join("state"),
        HELLO_LOG,
        None,
        local_on("cs_script_demo"),
    );
    let out = call(
        &srv,
        "script_run",
        run_args(json!({"expect": ["Goodbye"], "timeoutMs": 60000})),
    )
    .await
    .unwrap();
    let _bridge = engine.await.unwrap().expect("the mock bridge came up");
    assert_eq!(out["pending"], json!(true), "{out}");
    assert_eq!(out["passed"], json!(null), "{out}");
    assert_eq!(out["clampedToBudget"], json!(true));
    assert!(out["next"].as_str().unwrap().contains("script_run again"));
}

#[tokio::test]
async fn cs_script_run_without_an_addon_or_with_inline_source_launches_nothing() {
    let fake = FakePodman::new("script-run-refused");
    let proj = fake.dir().join("project");
    std::fs::create_dir_all(&proj).unwrap();
    let srv = server_with(&fake, &proj, None);
    let err = call(
        &srv,
        "script_run",
        json!({"kind": "cs_script", "expect": ["x"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    let err = call(
        &srv,
        "script_run",
        json!({"kind": "cs_script", "source": "Instance.Msg('x')", "expect": ["x"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "NotImplementedYet");
    assert!(err.data["missing"]
        .as_str()
        .unwrap()
        .contains("resourcecompiler"));
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}
