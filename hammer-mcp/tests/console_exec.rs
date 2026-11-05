//! `console_exec` / `cvar_get` tool tests: the guard (refused lines are never sent), the two
//! routes (bridge `console.exec`, VToolMessage fallback through a fake podman), and the console.log
//! readback (offset, truncation, waitFor, maxLines). No container, game or real bridge.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

const OLD_CONSOLE: &str = "10/13 20:00:00 an old line\r\n10/13 20:00:01 another old line\r\n";

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "console-exec-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let app = root.join("workshop/run/app");
        std::fs::create_dir_all(app.join("game/csgo")).expect("tmp tree");
        Tree { root, app }
    }

    fn console(&self) -> PathBuf {
        self.app.join("game/csgo/console.log")
    }

    fn old_console(&self, text: &str) {
        std::fs::write(self.console(), text).expect("console");
    }

    /// What the fake engine prints when any command arrives over the channel.
    fn engine_says(&self, lines: &str) {
        std::fs::write(self.root.join("engine-lines"), lines).expect("engine lines");
    }

    /// The fake engine never prints the marker's `Unknown command:` line (the vtm route may not).
    fn engine_skips_marker(&self) {
        std::fs::write(self.root.join("no-marker-echo"), "1").expect("no-marker-echo");
    }

    /// The fake engine replaces console.log instead of appending (a restart/truncation).
    fn engine_rotates(&self) {
        std::fs::write(self.root.join("rotate"), "1").expect("rotate");
    }

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
        files.iter().map(|p| std::fs::read_to_string(p).expect("payload")).collect()
    }

    fn podman_calls(&self) -> usize {
        std::fs::read_to_string(self.root.join("podman-calls.log"))
            .map(|t| t.lines().count())
            .unwrap_or(0)
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// `kind`: `ok` (prints the engine lines for a command, `Unknown command: <marker>` for a
/// marker), `slow` (each exec takes 350 ms), `exec-timeout` (an exec that never returns).
fn fake_podman(tree: &Tree, kind: &str) -> PathBuf {
    let bin_dir = tree.root.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    let exec_body = match kind {
        // exec: the SIGKILL of the cut-off lands on the sleeper itself (no orphan, no late writes)
        "exec-timeout" => "    exec sleep 600\n".to_string(),
        _ => format!(
            r#"{slow}    echo "vtmsend: creator version 1"
    data=$(grep -o '_data = "[^"]*"' "$APP/vtm_payload.kv3" | head -1 | sed 's/_data = "//; s/"$//')
    case "$data" in
      cs2mcp_mark_*)
        if [ ! -f "$TREE/no-marker-echo" ]; then
          echo "10/13 21:00:00 Unknown command: $data" >> "$APP/game/csgo/console.log"
        fi
        ;;
      *)
        if [ -f "$TREE/engine-lines" ]; then
          if [ -f "$TREE/rotate" ]; then
            cat "$TREE/engine-lines" > "$APP/game/csgo/console.log"
          else
            cat "$TREE/engine-lines" >> "$APP/game/csgo/console.log"
          fi
        fi
        ;;
    esac
    exit 0
"#,
            slow = if kind == "slow" { "    sleep 0.35\n" } else { "" }
        ),
    };
    let body = format!(
        r#"#!/bin/bash
APP='{app}'
TREE='{tree}'
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
{exec_body}    ;;
esac
"#,
        app = tree.app.display(),
        tree = tree.root.display()
    );
    let script = bin_dir.join(format!("podman-{kind}"));
    std::fs::write(&script, body).expect("fake podman");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    script
}

fn config(tree: &Tree, discovery: &Path, podman: PathBuf) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let args: Vec<String> = vec![
        "--deadline-ms".into(),
        "5000".into(),
        "--tool-budget-ms".into(),
        "30000".into(),
        "--discovery".into(),
        discovery.display().to_string(),
        "--project-root".into(),
        tree.root.display().to_string(),
        // The fake podman is a forked bash script whose cost grows with host load: its exec
        // timeout is generous, except for the tests about the cut-off itself (`exec-timeout`).
        "--podman-short-timeout-ms".into(),
        if podman.file_name().is_some_and(|n| n.to_string_lossy().ends_with("exec-timeout")) { "2000" } else { "120000" }.into(),
        // known container: the short-timeout knob then bounds only the exec under test, not a `ps`
        "--tools-container".into(),
        "cs2tools".into(),
        "--podman".into(),
        podman.display().to_string(),
    ];
    let given: Vec<&str> = args.iter().map(String::as_str).collect();
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

fn build(tree: &Tree, mb: &MockBridge) -> McpServer<HammerState> {
    build_with(tree, mb, "ok", 30_000)
}

fn build_with(tree: &Tree, mb: &MockBridge, podman: &str, budget_ms: u64) -> McpServer<HammerState> {
    let mut cfg = config(tree, &mb.discovery_path, fake_podman(tree, podman));
    cfg.tool_budget_ms = budget_ms;
    hammer_mcp::build_server(&cfg).expect("server")
}

async fn call(server: &McpServer<HammerState>, name: &str, args: Value) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

/// A bridge that has no `console.exec` (the fallback route) and a map up.
async fn plain_bridge() -> MockBridge {
    MockBridge::builder()
        .responder_value("game.local", json!({"inGame": true, "map": "probe"}))
        .start()
        .await
        .unwrap()
}

/// A bridge whose `console.exec` appends `lines` to the tree's console.log when a command arrives
/// (and the engine's `Unknown command: <marker>` when the marker does), with a map up.
async fn console_bridge(tree: &Tree, lines: &'static str) -> MockBridge {
    console_bridge_with(tree, lines, json!({"inGame": true, "map": "probe"})).await
}

async fn console_bridge_with(tree: &Tree, lines: &'static str, local: Value) -> MockBridge {
    let console = tree.console();
    MockBridge::builder()
        .responder_value("game.local", local)
        .responder("console.exec", move |params| {
            use std::io::Write;
            let command = params["command"].as_str().unwrap_or("");
            let mut f = std::fs::OpenOptions::new().append(true).create(true).open(&console).unwrap();
            if command.starts_with("cs2mcp_mark_") {
                writeln!(f, "10/13 21:00:00 Unknown command: {command}\r").unwrap();
            } else {
                f.write_all(lines.as_bytes()).unwrap();
            }
            json!({"queued": true})
        })
        .start()
        .await
        .unwrap()
}

#[tokio::test]
async fn refused_lines_are_never_sent() {
    let tree = Tree::new("refused");
    let mb = console_bridge(&tree, "x\r\n").await;
    let server = build(&tree, &mb);
    for cmd in [
        "quit",
        "echo a; quit",
        "alias x quit",
        "maps *",
        "disconnect",
        "quit()",
        "crash{}",
        "bind f1 quit",
    ] {
        let err = call(&server, "console_exec", json!({"command": cmd})).await.unwrap_err();
        assert_eq!(err.type_, "ConsoleCommandRefused", "{cmd}: {err:?}");
        assert!(err.retry_safe, "{cmd}");
        assert!(err.data.get("useInstead").is_none(), "{cmd}: only map changes point at run_map");
        assert!(!format!("{err:?}").contains("stop_session"), "{cmd}: hammer-mcp has no stop_session: {err:?}");
    }
    assert!(mb.method_calls("console.exec").is_empty(), "nothing reached the bridge");
    assert_eq!(tree.podman_calls(), 0, "nothing reached the channel");
    assert!(tree.payloads().is_empty());
}

#[tokio::test]
async fn map_changing_lines_are_refused_with_run_map() {
    let tree = Tree::new("mapchange");
    let mb = console_bridge(&tree, "x\r\n").await;
    let server = build(&tree, &mb);
    for cmd in [
        "map de_dust2",
        "changelevel de_dust2",
        "map_workshop 1 de_x",
        "host_workshop_map 1",
        "ds_workshop_changelevel x",
        "connect 1.2.3.4",
        "retry",
        "restart",
        "playdemo x",
        "startupmenu",
        "echo a; map x",
        "alias go \"map x\"",
        "map(x)",
    ] {
        let err = call(&server, "console_exec", json!({"command": cmd})).await.unwrap_err();
        assert_eq!(err.type_, "ConsoleCommandRefused", "{cmd}: {err:?}");
        assert_eq!(err.data["useInstead"]["tool"], json!("run_map"), "{cmd}: {err:?}");
    }
    assert!(mb.method_calls("console.exec").is_empty());
    assert_eq!(tree.podman_calls(), 0);
}

#[tokio::test]
async fn an_allowed_chain_goes_over_the_channel_and_reads_back_only_new_lines() {
    let tree = Tree::new("vtm");
    tree.old_console(OLD_CONSOLE);
    tree.engine_says("10/13 20:30:00 ok\r\n");
    let mb = plain_bridge().await;
    let server = build(&tree, &mb);

    let out = call(&server, "console_exec", json!({"command": "sv_cheats 1; echo ok"})).await.unwrap();
    assert_eq!(out["route"], json!("vtm"), "{out}");
    assert_eq!(out["lines"], json!(["10/13 20:30:00 ok"]), "old lines are not returned: {out}");
    assert_eq!(out["truncated"], json!(false));
    assert_eq!(out["endedBy"], json!("marker"), "{out}");
    assert_eq!(out["markerSent"], json!(true));
    assert!(out.get("ok").is_none(), "no hard-coded ok: {out}");
    assert_eq!(out["console"]["rotated"], json!(false));
    let payloads = tree.payloads();
    assert_eq!(payloads.len(), 2, "the line, then the marker: {payloads:?}");
    assert!(payloads[0].contains(r#"_data = "sv_cheats 1; echo ok""#), "{payloads:?}");
    assert!(payloads[1].contains("_data = \"cs2mcp_mark_"), "{payloads:?}");
    assert!(mb.method_calls("console.exec").is_empty());
}

#[tokio::test]
async fn the_bridge_route_is_preferred_when_advertised() {
    let tree = Tree::new("bridge");
    tree.old_console(OLD_CONSOLE);
    let mb = console_bridge(&tree, "10/13 20:31:00 hello\r\n").await;
    let server = build(&tree, &mb);

    let out = call(&server, "console_exec", json!({"command": "echo hello"})).await.unwrap();
    assert_eq!(out["route"], json!("bridge"), "{out}");
    assert_eq!(out["lines"], json!(["10/13 20:31:00 hello"]));
    assert_eq!(out["endedBy"], json!("marker"), "{out}");
    let calls = mb.method_calls("console.exec");
    assert_eq!(calls.len(), 2, "the line and the marker");
    assert!(calls[1].params["command"].as_str().unwrap().starts_with("cs2mcp_mark_"));
    assert_eq!(tree.podman_calls(), 0);
}

#[tokio::test]
async fn the_engine_limit_is_per_command_and_the_bridge_limit_per_line() {
    for (tag, bridge_route) in [("long-bridge", true), ("long-vtm", false)] {
        let tree = Tree::new(tag);
        tree.engine_says("10/13 20:34:00 long done\r\n");
        let mb = if bridge_route { console_bridge(&tree, "never\r\n").await } else { plain_bridge().await };
        let server = build(&tree, &mb);
        let route = if bridge_route { "bridge" } else { "vtm" };
        // one 511-byte command: the engine would drop it (live: the hostname probe)
        let single_over = format!("hostname \"{}\"", "z".repeat(500));
        let err = call(&server, "console_exec", json!({"command": single_over})).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{tag}: {err:?}");
        assert!(err.message.contains("511 bytes") && err.message.contains("510"), "{tag}: {}", err.message);
        // ... also as the second command of a chain
        let chain_over = format!("echo a; {single_over}");
        let err = call(&server, "console_exec", json!({"command": chain_over})).await.unwrap_err();
        assert!(err.message.contains("510"), "{tag}: {}", err.message);
        // a command name over 128 bytes
        let long_name = format!("{} 1", "q".repeat(129));
        let err = call(&server, "console_exec", json!({"command": long_name})).await.unwrap_err();
        assert!(err.message.contains("129 bytes") && err.message.contains("128"), "{tag}: {}", err.message);
        // over the bridge's whole-line buffer, every command short: the other limit, named as such
        let long_chain = vec!["echo abcdefghij"; 80].join("; ");
        assert!(long_chain.len() > 1023);
        let err = call(&server, "console_exec", json!({"command": long_chain})).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{tag}: {err:?}");
        assert!(err.message.contains("1023") && err.message.contains("line buffer"), "{tag}: {}", err.message);
        assert!(mb.method_calls("console.exec").is_empty(), "{tag}");
        assert!(tree.payloads().is_empty(), "{tag}: nothing went over the channel");
        // exactly 510 bytes in one command is sent, and a 702-byte chain of short commands too
        let at_limit = format!("echo {}", "x".repeat(510 - 5));
        let out = call(&server, "console_exec", json!({"command": at_limit, "waitMs": 300})).await.unwrap();
        assert_eq!(out["route"], json!(route), "{tag}: {out}");
        let chain = vec!["echo abcdefghij"; 44].join("; ");
        assert!(chain.len() > 510 && chain.len() <= 1023, "{}", chain.len());
        let out = call(&server, "console_exec", json!({"command": chain, "waitMs": 300})).await.unwrap();
        assert_eq!(out["route"], json!(route), "{tag}: {out}");
    }
}

/// The engine logs every handled VToolMessage as this block (live, 2000933).
fn echo_block(data: &str) -> String {
    format!(
        "10/26 19:05:25 [ToolFramework2] Handling VToolMessage:\r\n{{\r\n\t_message = \"command\"\r\n\t_data = \"{data}\"\r\n}}\r\n"
    )
}

#[tokio::test]
async fn wait_for_skips_the_engines_echo_of_the_command_on_the_vtm_route() {
    let tree = Tree::new("echo-skip");
    // the echo block repeats the command text; the real output never contains the pattern
    let text: &'static str = Box::leak(echo_block("echo p1_wait_target").into_boxed_str());
    tree.engine_says(text);
    let mb = plain_bridge().await;
    let server = build(&tree, &mb);
    let out = call(
        &server,
        "console_exec",
        json!({"command": "echo p1_wait_target", "waitMs": 600, "waitFor": "p1_wait_target"}),
    )
    .await
    .unwrap();
    assert_eq!(out["route"], json!("vtm"), "{out}");
    assert_eq!(out["matched"], json!(false), "the echo is not output: {out}");
    assert_eq!(out["endedBy"], json!("timeout"), "{out}");

    // a genuine line after the echo still matches, and is the matched line
    let tree = Tree::new("echo-real");
    let both: &'static str = Box::leak(
        format!("{}10/26 19:05:26 p1_wait_target\r\n", echo_block("echo p1_wait_target")).into_boxed_str(),
    );
    tree.engine_says(both);
    let server = build(&tree, &mb);
    let out = call(
        &server,
        "console_exec",
        json!({"command": "echo p1_wait_target", "waitMs": 3000, "waitFor": "p1_wait_target"}),
    )
    .await
    .unwrap();
    assert_eq!(out["endedBy"], json!("match"), "{out}");
    assert_eq!(out["matchedLine"], json!("10/26 19:05:26 p1_wait_target"), "{out}");
    // the echo block stays in the returned lines (it is what the engine printed)
    assert!(out["lines"].as_array().unwrap().len() >= 6, "{out}");
}

#[test]
fn echo_blocks_are_masked_whole() {
    use hammer_mcp::tools::console_live::echo_block_mask;
    let lines: Vec<String> = [
        "a",
        "10/26 19:05:25 [ToolFramework2] Handling VToolMessage:",
        "{",
        "\t_message = \"command\"",
        "\t_data = \"x\"",
        "}",
        "b",
        "10/26 19:05:26 [ToolFramework2] Handling VToolMessage:",
        "{",
        "\t_data = \"unterminated\"",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        echo_block_mask(&lines),
        [false, true, true, true, true, true, false, true, true, true]
    );
}

#[test]
fn an_unterminated_or_malformed_echo_block_does_not_hide_real_lines() {
    use hammer_mcp::tools::console_live::echo_block_mask;
    let lines: Vec<String> = [
        "10/26 19:05:25 [ToolFramework2] Handling VToolMessage:",
        "{",
        "\t_data = \"x\"",
        "10/26 19:05:26 real output",
        "10/26 19:05:27 more output",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(echo_block_mask(&lines), [true, true, true, false, false]);
    // tab-indented lines beyond the known body size stop masking too
    let mut long: Vec<String> = vec!["h [ToolFramework2] Handling VToolMessage:".into(), "{".into()];
    long.extend((0..8).map(|i| format!("\tk{i} = v")));
    long.push("}".into());
    let mask = echo_block_mask(&long);
    assert!(mask[..6].iter().all(|m| *m) && mask[6..].iter().all(|m| !*m), "{mask:?}");
    // the untimestamped copy after a dropped-command warning is masked
    let lines: Vec<String> =
        ["t WARNING: Command too long... ignoring!", "echo secret_text", "t real"].iter().map(|s| s.to_string()).collect();
    assert_eq!(echo_block_mask(&lines), [false, true, false]);
    // REVIEW 6: `Console command too long.` has no copy after it; the next line is real output
    let lines: Vec<String> = ["t Console command too long.", "t real output", "t more"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(echo_block_mask(&lines), [false, false, false]);
}

#[tokio::test]
async fn a_command_too_long_warning_in_the_readback_is_a_delivery_warning() {
    let tree = Tree::new("too-long-warning");
    let mb = console_bridge(&tree, "10/26 19:05:25 WARNING: Command too long... ignoring!\r\n").await;
    let server = build(&tree, &mb);
    let out = call(&server, "console_exec", json!({"command": "echo x"})).await.unwrap();
    assert!(out["deliveryWarning"].as_str().unwrap().contains("Command too long"), "{out}");
}

#[tokio::test]
async fn a_console_command_too_long_line_is_a_delivery_warning_too() {
    let tree = Tree::new("name-too-long-warning");
    let mb = console_bridge(&tree, "10/26 19:05:25 Console command too long.\r\n").await;
    let server = build(&tree, &mb);
    let out = call(&server, "console_exec", json!({"command": "echo x"})).await.unwrap();
    assert!(out["deliveryWarning"].as_str().unwrap().contains("Console command too long"), "{out}");
}

#[tokio::test]
async fn echo_and_bare_cvars_on_the_bridge_route_get_a_steering_note() {
    let tree = Tree::new("steer");
    let mb = console_bridge(&tree, "").await;
    let server = build(&tree, &mb);
    for cmd in ["echo hi", "mp_c4timer"] {
        let out = call(&server, "console_exec", json!({"command": cmd, "waitMs": 300})).await.unwrap();
        assert_eq!(out["route"], json!("bridge"), "{out}");
        assert!(out["notes"].to_string().contains("cvar_get"), "{cmd}: {out}");
    }
    let out = call(&server, "console_exec", json!({"command": "status", "waitMs": 300})).await.unwrap();
    assert!(!out.get("notes").map(|n| n.to_string()).unwrap_or_default().contains("echo and cvar"), "{out}");
}

#[tokio::test]
async fn cvar_get_redacts_secret_names_whatever_the_bridge_says() {
    let tree = Tree::new("cvar-secret");
    let reply = json!({"cvars": [
        {"name": "rcon_password", "found": true, "value": "hunter2", "default": "", "type": "string", "flags": []},
        {"name": "mp_c4timer", "found": true, "value": "40", "default": "40", "type": "int32", "flags": []}]});
    let mb = MockBridge::builder().responder_value("cvar.get", reply).start().await.unwrap();
    let server = build(&tree, &mb);
    let out = call(&server, "cvar_get", json!({"names": ["rcon_password", "mp_c4timer"]})).await.unwrap();
    assert_eq!(out["cvars"][0]["value"], json!(null), "{out}");
    assert_eq!(out["cvars"][0]["default"], json!(null), "{out}");
    assert_eq!(out["cvars"][0]["redacted"], json!(true), "{out}");
    assert_eq!(out["cvars"][1]["value"], json!("40"), "{out}");
    assert!(!out.to_string().contains("hunter2"));
}

#[tokio::test]
async fn a_gated_bridge_falls_back_to_the_channel() {
    let tree = Tree::new("gated");
    tree.engine_says("10/13 20:32:00 viaChannel\r\n");
    let mb = MockBridge::builder()
        .responder_value("game.local", json!({"inGame": true, "map": "probe"}))
        .error("console.exec", -32011, "console.exec is only enabled in -tools or -testscript sessions", None)
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb);

    let out = call(&server, "console_exec", json!({"command": "echo x"})).await.unwrap();
    assert_eq!(out["route"], json!("vtm"), "{out}");
    assert!(out["bridgeRefused"].as_str().unwrap().contains("console.exec"), "{out}");
    assert_eq!(out["lines"], json!(["10/13 20:32:00 viaChannel"]));
    assert_eq!(tree.payloads().len(), 2);
}

#[tokio::test]
async fn an_unreachable_bridge_is_noted_and_the_channel_is_used() {
    let tree = Tree::new("nobridge");
    tree.engine_says("10/13 20:35:00 viaChannel\r\n");
    let mb = plain_bridge().await;
    let mut cfg = config(&tree, &mb.discovery_path, fake_podman(&tree, "ok"));
    cfg.discovery = Some(tree.root.join("no-such-discovery.json"));
    let server = hammer_mcp::build_server(&cfg).expect("server");
    let out = call(&server, "console_exec", json!({"command": "echo x"})).await.unwrap();
    assert_eq!(out["route"], json!("vtm"), "{out}");
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes.iter().any(|n| n.as_str().unwrap().contains("bridge could not be reached")),
        "{out}"
    );
}

#[tokio::test]
async fn wait_for_stops_the_wait_early() {
    let tree = Tree::new("waitfor");
    tree.engine_says("10/13 20:33:00 mp_c4timer = 40\r\n");
    let mb = plain_bridge().await;
    let server = build(&tree, &mb);

    let out = call(
        &server,
        "console_exec",
        json!({"command": "mp_c4timer", "waitMs": 10000, "waitFor": "mp_c4timer = \\d+"}),
    )
    .await
    .unwrap();
    assert_eq!(out["matched"], json!(true), "{out}");
    assert_eq!(out["endedBy"], json!("match"));
    assert_eq!(out["matchedLine"], json!("10/13 20:33:00 mp_c4timer = 40"));
    assert!(out["waitedMs"].as_u64().unwrap() < 3000, "it did not wait the full 10 s: {out}");

    // no match: the marker does not end a waitFor wait; it runs to waitMs, with the marker seen
    let out = call(&server, "console_exec", json!({"command": "x", "waitMs": 300, "waitFor": "never-printed"}))
        .await
        .unwrap();
    assert_eq!(out["matched"], json!(false), "{out}");
    assert_eq!(out["endedBy"], json!("timeout"), "{out}");
    assert_eq!(out["markerSeen"], json!(true), "{out}");
    let notes = out.get("notes").map(|n| n.to_string()).unwrap_or_default();
    assert!(!notes.contains("marker was not seen"), "the marker WAS seen: {out}");
}

#[tokio::test]
async fn wait_for_matches_output_printed_after_the_marker() {
    // the command returns (marker echoed), then the engine prints a late line (a timer, a round start)
    let tree = Tree::new("late-waitfor");
    let console = tree.console();
    let mb = MockBridge::builder()
        .responder_value("game.local", json!({"inGame": true, "map": "probe"}))
        .responder("console.exec", move |params| {
            use std::io::Write;
            let command = params["command"].as_str().unwrap_or("");
            let mut f = std::fs::OpenOptions::new().append(true).create(true).open(&console).unwrap();
            if command.starts_with("cs2mcp_mark_") {
                write!(f, "10/13 21:00:00 Unknown command: {command}\r\n10/13 21:00:05 round_start fired\r\n").unwrap();
            } else {
                f.write_all(b"10/13 20:59:59 timer armed\r\n").unwrap();
            }
            json!({"queued": true})
        })
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb);
    let out = call(
        &server,
        "console_exec",
        json!({"command": "mp_timer 5", "waitMs": 5000, "waitFor": "round_start"}),
    )
    .await
    .unwrap();
    assert_eq!(out["endedBy"], json!("match"), "{out}");
    assert_eq!(out["matched"], json!(true), "{out}");
    assert_eq!(out["markerSeen"], json!(true), "{out}");
    assert_eq!(out["matchedLine"], json!("10/13 21:00:05 round_start fired"));
    // the marker's own line is not output
    assert_eq!(
        out["lines"],
        json!(["10/13 20:59:59 timer armed", "10/13 21:00:05 round_start fired"]),
        "{out}"
    );
}

#[tokio::test]
async fn the_vtm_route_falls_back_to_quiet_when_the_marker_is_never_echoed() {
    let tree = Tree::new("vtm-quiet");
    tree.engine_says("10/13 20:40:00 value = 3\r\n");
    tree.engine_skips_marker();
    let mb = plain_bridge().await;
    let server = build(&tree, &mb);
    let out = call(&server, "console_exec", json!({"command": "echo x", "waitMs": 8000})).await.unwrap();
    assert_eq!(out["route"], json!("vtm"), "{out}");
    assert_eq!(out["markerSent"], json!(true), "{out}");
    assert_eq!(out["markerSeen"], json!(false), "{out}");
    assert_eq!(out["endedBy"], json!("quiet"), "{out}");
    assert_eq!(out["lines"], json!(["10/13 20:40:00 value = 3"]));
    assert!(out["waitedMs"].as_u64().unwrap() < 4000, "did not run to waitMs: {out}");
}

#[tokio::test]
async fn a_silent_vtm_call_gets_one_unverified_echo_note() {
    let tree = Tree::new("vtm-silent");
    tree.old_console(OLD_CONSOLE);
    tree.engine_skips_marker();
    let mb = plain_bridge().await;
    let server = build(&tree, &mb);
    let out = call(&server, "console_exec", json!({"command": "echo x", "waitMs": 700})).await.unwrap();
    assert_eq!(out["endedBy"], json!("timeout"), "{out}");
    assert_eq!(out["lineCount"], json!(0));
    let notes = out["notes"].as_array().expect("notes");
    assert_eq!(notes.len(), 1, "{out}");
    assert!(notes[0].as_str().unwrap().contains("echo is unverified"), "{out}");
}

#[tokio::test]
async fn without_a_marker_the_wait_ends_when_the_output_goes_quiet() {
    // a bridge that dies on the marker: the line went out, the marker could not
    let tree = Tree::new("quiet");
    tree.old_console(OLD_CONSOLE);
    let console = tree.console();
    let mb = MockBridge::builder()
        .responder_value("game.local", json!({"inGame": true, "map": "probe"}))
        .responder("console.exec", move |params| {
            use std::io::Write;
            if !params["command"].as_str().unwrap_or("").starts_with("cs2mcp_mark_") {
                let mut f = std::fs::OpenOptions::new().append(true).open(&console).unwrap();
                f.write_all(b"10/13 20:36:00 one\r\n").unwrap();
            }
            json!({"queued": true})
        })
        .error_on_call("console.exec", 2, -32603, "boom", None)
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb);
    let out = call(&server, "console_exec", json!({"command": "echo one", "waitMs": 5000})).await.unwrap();
    assert_eq!(out["markerSent"], json!(false), "{out}");
    assert_eq!(out["endedBy"], json!("quiet"), "{out}");
    assert_eq!(out["lines"], json!(["10/13 20:36:00 one"]));
    assert!(out["waitedMs"].as_u64().unwrap() < 4000, "{out}");
}

#[tokio::test]
async fn a_cut_off_exec_is_a_delivery_warning_and_never_repeated() {
    let tree = Tree::new("delivered");
    let mb = plain_bridge().await;
    let server = build_with(&tree, &mb, "exec-timeout", 60_000);
    let out = call(&server, "console_exec", json!({"command": "echo x", "waitMs": 200})).await.unwrap();
    assert_eq!(out["vtmsend"]["delivered"], json!(false), "{out}");
    assert!(out["deliveryWarning"].as_str().unwrap().contains("cut off"), "{out}");
    assert_eq!(out["markerSent"], json!(false), "no second post to a stuck channel: {out}");
    // server-side proof of "posted once, never repeated" (the fake's own side effects race the kill)
    assert!(out.get("vtmsendMarker").is_none(), "no second post: {out}");
    assert!(tree.podman_calls() <= 1, "podman calls: {}", tree.podman_calls());
    let posted = std::fs::read_to_string(tree.app.join("vtm_payload.kv3")).expect("payload written before the exec");
    assert!(posted.contains("echo x"), "{posted}");
    // the missing-console.log note is a separate key from the delivery warning
    assert!(out["consoleNote"].as_str().unwrap().contains("console.log"), "{out}");
    assert_eq!(out["endedBy"], json!("noFile"));
}

#[tokio::test]
async fn a_truncated_console_log_returns_the_new_file_not_the_old_lines() {
    let tree = Tree::new("rotate");
    tree.old_console(&OLD_CONSOLE.repeat(20));
    tree.engine_rotates();
    tree.engine_says("new session line\r\n");
    let mb = plain_bridge().await;
    let server = build(&tree, &mb);

    let out = call(&server, "console_exec", json!({"command": "echo x", "waitMs": 400})).await.unwrap();
    assert_eq!(out["lines"], json!(["new session line"]), "{out}");
    assert_eq!(out["console"]["rotated"], json!(true));
}

#[tokio::test]
async fn a_missing_console_log_is_a_note_not_a_failure() {
    let tree = Tree::new("nolog");
    let mb = MockBridge::builder()
        .responder_value("game.local", json!({"inGame": true, "map": "probe"}))
        .responder_value("console.exec", json!({"queued": true}))
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb);
    let out = call(&server, "console_exec", json!({"command": "echo x", "waitMs": 3000})).await.unwrap();
    assert_eq!(out["lines"], json!([]));
    assert_eq!(out["console"]["exists"], json!(false));
    assert_eq!(out["endedBy"], json!("noFile"), "{out}");
    assert!(out["consoleNote"].as_str().unwrap().contains("console.log"), "{out}");
    assert!(out["waitedMs"].as_u64().unwrap() < 1000, "no waiting on a file that is not there: {out}");
}

#[tokio::test]
async fn max_lines_keeps_the_last_lines_and_says_so() {
    let tree = Tree::new("maxlines");
    tree.engine_says("l1\r\nl2\r\nl3\r\nl4\r\n");
    let mb = plain_bridge().await;
    let server = build(&tree, &mb);
    let out = call(&server, "console_exec", json!({"command": "cvarlist", "maxLines": 2})).await.unwrap();
    assert_eq!(out["lines"], json!(["l3", "l4"]));
    assert_eq!(out["truncated"], json!(true));
    assert_eq!(out["lineCount"], json!(4));
}

#[tokio::test]
async fn a_partial_last_line_is_neither_matched_nor_returned() {
    let tree = Tree::new("partial");
    tree.old_console(OLD_CONSOLE);
    let console = tree.console();
    // the engine is mid-write (no trailing newline) and the marker cannot be sent (2nd call fails)
    let mb = MockBridge::builder()
        .responder_value("game.local", json!({"inGame": true, "map": "probe"}))
        .responder("console.exec", move |_| {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&console).unwrap();
            f.write_all(b"whole line\r\nhalf a li").unwrap();
            json!({"queued": true})
        })
        .error_on_call("console.exec", 2, -32603, "boom", None)
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb);
    let out = call(
        &server,
        "console_exec",
        json!({"command": "echo x", "waitMs": 1500, "waitFor": "half a li"}),
    )
    .await
    .unwrap();
    assert_eq!(out["matched"], json!(false), "{out}");
    assert_eq!(out["lines"], json!(["whole line"]), "{out}");
    assert_eq!(out["console"]["partialTail"], json!(true));
}

#[tokio::test]
async fn an_idle_level_runs_console_lines_with_a_note() {
    let tree = Tree::new("idle");
    tree.old_console("11/02 10:00:00 Spawn Server: <empty>\r\n");
    tree.engine_says("11/02 10:01:00 game_type = 0\r\n");
    let mb = MockBridge::builder()
        .responder_value("game.local", json!({"inGame": false, "map": "<empty>"}))
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb);
    let out = call(&server, "console_exec", json!({"command": "game_type 0"})).await.unwrap();
    assert_eq!(out["route"], json!("vtm"), "{out}");
    assert_eq!(out["lines"], json!(["11/02 10:01:00 game_type = 0"]));
    let notes = out["notes"].as_array().expect("notes");
    assert!(notes.iter().any(|n| n.as_str().unwrap().contains("idle level")), "{out}");
}

#[tokio::test]
async fn a_level_that_is_still_loading_is_engine_busy_and_nothing_is_sent() {
    let tree = Tree::new("loading");
    tree.old_console("11/02 10:00:00 Spawn Server: <empty>\r\n11/02 10:00:05 Loading map \"probe\"\r\n");
    let mb = MockBridge::builder()
        .responder_value("game.local", json!({"inGame": false, "map": "<empty>"}))
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb);
    let err = call(&server, "console_exec", json!({"command": "echo x"})).await.unwrap_err();
    assert_eq!(err.type_, "EngineBusy", "{err:?}");
    assert!(err.retry_safe);
    assert!(err.message.contains("run_map"), "the way out is named: {err:?}");
    assert_eq!(tree.podman_calls(), 0);
}

#[tokio::test]
async fn a_stale_loading_line_is_taken_as_stale_with_a_note() {
    // a `Loading map` nothing followed (an aborted load, a failure line outside the regexes): once
    // console.log has been silent for the stale window the line runs, with a note
    let tree = Tree::new("stale-loading");
    tree.old_console("11/02 10:00:00 Spawn Server: <empty>\r\n11/02 10:00:05 Loading map \"probe\"\r\n");
    let old = std::time::SystemTime::now()
        - std::time::Duration::from_millis(hammer_mcp::tools::run_map::SETTLE_STALE_MS + 30_000);
    std::fs::OpenOptions::new()
        .write(true)
        .open(tree.console())
        .unwrap()
        .set_modified(old)
        .unwrap();
    tree.engine_says("11/02 10:05:00 fine\r\n");
    let mb = MockBridge::builder()
        .responder_value("game.local", json!({"inGame": false, "map": "<empty>"}))
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb);
    let out = call(&server, "console_exec", json!({"command": "echo x"})).await.unwrap();
    assert_eq!(out["lines"], json!(["11/02 10:05:00 fine"]), "{out}");
    let notes = out["notes"].as_array().expect("notes");
    assert!(notes.iter().any(|n| n.as_str().unwrap().contains("stale")), "{out}");
}

#[tokio::test]
async fn game_local_failing_proceeds() {
    let tree = Tree::new("local-error");
    tree.engine_says("11/02 10:02:00 fine\r\n");
    let mb = MockBridge::builder()
        .error("game.local", -32603, "internal", None)
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb);
    let out = call(&server, "console_exec", json!({"command": "echo x"})).await.unwrap();
    assert_eq!(out["lines"], json!(["11/02 10:02:00 fine"]), "{out}");
}

fn unresolved_load(tree: &Tree, offset: u64) -> hammer_mcp::tools::run_map::LastLoad {
    let _ = tree;
    hammer_mcp::tools::run_map::LastLoad {
        name: "probe".into(),
        vpk_mtime_ms: None,
        console_offset: offset,
        resolved: false,
        state: "timeout".into(),
        idle: false,
        in_flight_until: std::time::Instant::now() + std::time::Duration::from_secs(60),
    }
}

#[tokio::test]
async fn a_previous_load_without_a_verdict_is_engine_busy() {
    let tree = Tree::new("busy");
    tree.old_console("11/02 10:00:00 Loading map \"probe\"\r\n");
    let mb = MockBridge::builder()
        .responder_value("game.local", json!({"inGame": false, "map": "<empty>"}))
        .start()
        .await
        .unwrap();
    let server = build_with(&tree, &mb, "ok", 2_500);
    *server.state().last_load.lock().unwrap() = Some(unresolved_load(&tree, 0));
    let err = call(&server, "console_exec", json!({"command": "echo x"})).await.unwrap_err();
    assert_eq!(err.type_, "EngineBusy", "{err:?}");
    assert!(err.data.get("previousLoad").is_some(), "{err:?}");
    assert_eq!(tree.podman_calls(), 0);
}

#[tokio::test]
async fn a_load_that_finished_is_settled_instead_of_refused() {
    let tree = Tree::new("settled");
    tree.old_console(
        "11/02 10:00:00 Loading map \"probe\"\r\n11/02 10:00:09 OnSwitchLoopModeFinished( game : success )\r\n",
    );
    tree.engine_says("11/02 10:03:00 fine\r\n");
    // game.local has not caught up (inGame false) but the console window carries the verdict
    let mb = MockBridge::builder()
        .responder_value("game.local", json!({"inGame": false, "map": "<empty>"}))
        .start()
        .await
        .unwrap();
    let server = build_with(&tree, &mb, "ok", 5_000);
    *server.state().last_load.lock().unwrap() = Some(unresolved_load(&tree, 0));
    let out = call(&server, "console_exec", json!({"command": "echo x"})).await.unwrap();
    assert_eq!(out["lines"], json!(["11/02 10:03:00 fine"]), "{out}");
    assert_eq!(out["settledPreviousLoad"]["settled"], json!(true), "{out}");
    assert!(server.state().last_load.lock().unwrap().as_ref().unwrap().resolved);
}

#[tokio::test]
async fn a_send_waits_for_the_gate_run_map_holds() {
    let tree = Tree::new("gate");
    tree.engine_says("11/02 10:04:00 fine\r\n");
    let mb = plain_bridge().await;
    let server = std::sync::Arc::new(build(&tree, &mb));
    let held = server.state().vtm_gate.clone().lock_owned().await;
    let pending = tokio::spawn({
        let server = server.clone();
        async move { call(&server, "console_exec", json!({"command": "echo x"})).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!pending.is_finished(), "console_exec must wait for the vtm gate");
    assert_eq!(tree.podman_calls(), 0, "nothing sent while run_map holds the gate");
    drop(held);
    let out = tokio::time::timeout(std::time::Duration::from_secs(20), pending)
        .await
        .expect("finishes once the gate is free")
        .unwrap()
        .unwrap();
    assert_eq!(out["lines"], json!(["11/02 10:04:00 fine"]), "{out}");
}

#[tokio::test]
async fn concurrent_calls_do_not_interleave_a_line_and_its_marker() {
    let tree = Tree::new("concurrent");
    tree.engine_says("l\r\n");
    let mb = plain_bridge().await;
    let server = build_with(&tree, &mb, "slow", 60_000);
    let (a, b) = tokio::join!(
        call(&server, "console_exec", json!({"command": "echo aaa"})),
        call(&server, "console_exec", json!({"command": "echo bbb"})),
    );
    a.unwrap();
    b.unwrap();
    let payloads = tree.payloads();
    assert_eq!(payloads.len(), 4, "{payloads:?}");
    for pair in payloads.chunks(2) {
        assert!(pair[0].contains("echo "), "{payloads:?}");
        assert!(pair[1].contains("cs2mcp_mark_"), "a marker follows its own line: {payloads:?}");
    }
}

#[tokio::test]
async fn bad_arguments_send_nothing() {
    let tree = Tree::new("badargs");
    let mb = plain_bridge().await;
    let server = build(&tree, &mb);
    for bad in [
        json!({}),
        json!({"command": "a\nquit"}),
        json!({"command": "a", "waitMs": 10001}),
        json!({"command": "a", "waitFor": "("}),
    ] {
        let err = call(&server, "console_exec", bad.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}: {err:?}");
    }
    assert_eq!(tree.podman_calls(), 0);
}

#[tokio::test]
async fn cvar_get_is_not_implemented_against_a_bridge_without_cvar_get() {
    let tree = Tree::new("cvar-missing");
    let mb = plain_bridge().await;
    let server = build(&tree, &mb);
    let err = call(&server, "cvar_get", json!({"names": ["mp_c4timer"]})).await.unwrap_err();
    assert_eq!(err.name, "NotImplemented", "{err:?}");
    assert_eq!(err.data.get("missing"), Some(&json!(["cvar.get"])));
}

#[tokio::test]
async fn cvar_get_forwards_valid_names_and_rejects_bad_ones() {
    let tree = Tree::new("cvar");
    let reply = json!({"cvars": [{"name": "mp_c4timer", "found": true, "value": "40", "type": "int32",
                                  "default": "40", "flags": ["replicated"]}]});
    let mb = MockBridge::builder().responder_value("cvar.get", reply.clone()).start().await.unwrap();
    let server = build(&tree, &mb);
    let out = call(&server, "cvar_get", json!({"names": ["mp_c4timer"]})).await.unwrap();
    assert_eq!(out, reply);
    assert_eq!(mb.method_calls("cvar.get").len(), 1);

    let many: Vec<String> = (0..65).map(|i| format!("c{i}")).collect();
    for bad in [json!({"names": []}), json!({"names": many}), json!({"names": [1]}), json!({})] {
        let err = call(&server, "cvar_get", bad.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
    }
    assert_eq!(mb.method_calls("cvar.get").len(), 1, "invalid calls were not sent");
}
