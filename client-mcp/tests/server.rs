//! Server-level tests against the mock bridge: session_status (flavour + tool groups),
//! console_exec guard/gating/dispatch, stubs.
#![allow(clippy::result_large_err)]
use client_mcp::state::ClientConfig;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use serde_json::{json, Value};

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

async fn call(
    server: &cs2_mcp_common::mcp::McpServer<client_mcp::state::ClientState>,
    name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

const M28_CLIENT_METHODS: [&str; 12] = [
    "control.ping",
    "control.status",
    "control.capabilities",
    "control.version",
    "doc.active",
    "input.set",
    "input.clear",
    "input.status",
    "input.uninstall",
    "test.checkpoint",
    "console.exec",
    "game.tick",
];

#[test]
fn tool_list_and_forbidden_names() {
    let server =
        client_mcp::build_server(&config(&["--discovery", "/tmp/none/session.json"])).unwrap();
    let names = server.registry().names();
    assert_eq!(names[0], "session_status");
    for n in [
        "console_exec",
        "run_testscript",
        "checkpoint",
        "game_tick",
        "input_set",
        "input_clear",
        "observe",
        "screenshot",
        "ocr",
        "render_world",
        "demo_record_start",
        "demo_to_video",
        "server_up",
        "client_connect",
        "script_run",
        "launch_session",
        "stop_session",
    ] {
        assert!(names.contains(&n), "{n}");
    }
    for forbidden in [
        "console.command",
        "console_command",
        "game.saveRestore",
        "game_save_restore",
        "control.shutdown",
        "undo.redo",
    ] {
        assert!(!names.contains(&forbidden));
    }
}

#[tokio::test]
async fn session_status_dead_bridge_never_raises() {
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        "/tmp/cs2mcp-no-bridge/session.json",
        "--console-log",
        "/tmp/cs2mcp-no-console.log",
        // no real podman or project tree: the test must not see real containers or logs
        "--podman",
        concat!(env!("CARGO_TARGET_TMPDIR"), "/no-such-podman"),
        "--project-root",
        concat!(env!("CARGO_TARGET_TMPDIR"), "/no-such-project"),
    ]))
    .unwrap();
    let p = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(p["bridge"]["running"], json!(false));
    assert_eq!(p["bridge"]["error"]["name"], json!("BridgeNotRunning"));
    assert_eq!(p["runtime"]["flavour"], json!("unknown"));
    assert_eq!(p["toolGroups"]["input"]["available"], json!(false));
    assert_eq!(p["toolGroups"]["console_log"]["available"], json!(false));
    assert!(p["launch"]["command"]
        .as_str()
        .unwrap()
        .ends_with("cargo bridge runtime up"));
}

#[tokio::test]
async fn session_status_live_reports_flavour_and_groups() {
    let mb = MockBridge::builder()
        .methods(M28_CLIENT_METHODS)
        .start()
        .await
        .unwrap();
    let log = std::env::temp_dir().join(format!("cs2mcp-console-{}.log", std::process::id()));
    std::fs::write(&log, "TESTSCRIPT: CheckPoint -> 'VTEST_PARKED'\n").unwrap();
    // a podman that does not exist: the session group must report unavailable, not guess
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--console-log",
        log.to_str().unwrap(),
        "--podman",
        "/nonexistent/podman",
    ]))
    .unwrap();
    let p = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(p["bridge"]["running"], json!(true));
    assert_eq!(p["runtime"]["flavour"], json!("wine-tools"));
    assert_eq!(p["toolGroups"]["console_log"]["available"], json!(true));
    assert_eq!(p["toolGroups"]["console_exec"]["available"], json!(true));
    assert_eq!(p["toolGroups"]["test"]["available"], json!(true));
    assert_eq!(p["toolGroups"]["input"]["available"], json!(true));
    assert_eq!(p["toolGroups"]["render_world"]["available"], json!(false));
    assert_eq!(
        p["toolGroups"]["render_world"]["missing"],
        json!(["world.render"])
    );
    assert_eq!(p["toolGroups"]["session"]["available"], json!(false));
    let _ = std::fs::remove_file(&log);
}

#[tokio::test]
async fn explicit_flavour_wins() {
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        "/tmp/cs2mcp-no-bridge/session.json",
        "--flavour",
        "linux-client",
    ]))
    .unwrap();
    let p = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(p["runtime"]["flavour"], json!("linux-client"));
    assert!(p["toolGroups"]["input"]["reason"]
        .as_str()
        .unwrap()
        .contains("bridge not reachable"));
}

#[tokio::test]
async fn console_exec_refuses_before_dispatch() {
    let mb = MockBridge::builder()
        .methods(M28_CLIENT_METHODS)
        .responder_value("console.exec", json!({"queued": true}))
        .start()
        .await
        .unwrap();
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
    ]))
    .unwrap();
    for cmd in ["quit", "echo hi; quit", "Test_ExitProcess 3", "disconnect"] {
        let err = call(&server, "console_exec", json!({"command": cmd}))
            .await
            .unwrap_err();
        assert_eq!(err.name, "ForbiddenCommand", "{cmd}");
        assert!(err.retry_safe);
    }
    let err = call(&server, "console_exec", json!({})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    assert!(mb.method_calls("console.exec").is_empty());
}

/// REVIEW 9: the engine drops any single command over 510 bytes (name over 128) silently; the
/// client console_exec refuses it up front like hammer-mcp's.
#[tokio::test]
async fn console_exec_refuses_commands_over_the_engine_limits_before_dispatch() {
    let mb = MockBridge::builder()
        .methods(M28_CLIENT_METHODS)
        .responder_value("console.exec", json!({"queued": true}))
        .start()
        .await
        .unwrap();
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
    ]))
    .unwrap();
    for cmd in [
        format!("echo {}", "a".repeat(510)),
        "x".repeat(129),
        format!("echo ok; echo {}", "b".repeat(520)),
    ] {
        let err = call(&server, "console_exec", json!({"command": cmd})).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{cmd:.20}: {err:?}");
        assert!(err.message.contains("Nothing was sent"), "{err:?}");
    }
    assert!(mb.method_calls("console.exec").is_empty());
}

#[tokio::test]
async fn console_exec_dispatches_with_budget_deadline() {
    let mb = MockBridge::builder()
        .methods(M28_CLIENT_METHODS)
        .responder_value("console.exec", json!({"queued": true}))
        .start()
        .await
        .unwrap();
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--deadline-ms",
        "7000",
    ]))
    .unwrap();
    let out = call(&server, "console_exec", json!({"command": "getpos"}))
        .await
        .unwrap();
    assert_eq!(out["queued"], json!(true));
    assert_eq!(out["route"], json!("bridge"));
    let calls = mb.method_calls("console.exec");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].params["command"], json!("getpos"));
    assert_eq!(calls[0].params["deadlineMs"], json!(7000));
}

#[tokio::test]
async fn console_exec_gated_when_bridge_lacks_it() {
    let mb = MockBridge::start().await.unwrap(); // recorded Hammer caps: no console.exec
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
    ]))
    .unwrap();
    let err = call(&server, "console_exec", json!({"command": "getpos"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "NotImplemented");
    assert_eq!(err.data["missing"], json!(["console.exec"]));
    assert!(mb.method_calls("console.exec").is_empty());
}

#[tokio::test]
async fn console_exec_surfaces_bridge_not_implemented_outside_testscript_sessions() {
    let mb = MockBridge::builder()
        .methods(M28_CLIENT_METHODS)
        .error(
            "console.exec",
            -32011,
            "NotImplemented",
            Some(json!({"blockedOn": "console.exec needs a -testscript session"})),
        )
        .start()
        .await
        .unwrap();
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
    ]))
    .unwrap();
    let err = call(&server, "console_exec", json!({"command": "getpos"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "NotImplemented");
    assert!(err.data["blockedOn"]
        .as_str()
        .unwrap()
        .contains("-testscript"));
}

#[tokio::test]
async fn stubs_are_gated_then_not_implemented_yet() {
    let mb = MockBridge::builder()
        .methods(M28_CLIENT_METHODS)
        .start()
        .await
        .unwrap();
    // isolated: no real console.log and no real containers (a live session on the host would change the answer)
    let log = std::env::temp_dir().join(format!("cs2mcp-stubs-{}.log", std::process::id()));
    std::fs::write(&log, "").unwrap();
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--console-log",
        log.to_str().unwrap(),
        "--podman",
        "/nonexistent/podman",
    ]))
    .unwrap();
    // observe is implemented: without a capture it answers, and its game.tick part is the mock's
    let out = call(&server, "observe", json!({"capture": false}))
        .await
        .unwrap();
    // no session named: the tick is never sent to an arbitrary bridge; the call answers with a note
    assert!(mb.method_calls("game.tick").is_empty());
    assert!(
        out["errors"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["part"] != "tick"),
        "{out}"
    );
    assert!(
        out["notes"].to_string().contains("no session to tick"),
        "{out}"
    );
    let err = call(&server, "render_world", json!({})).await.unwrap_err();
    assert_eq!(err.data["missing"], json!(["world.render"]));
    assert!(mb.method_calls("input.set").is_empty());
}
