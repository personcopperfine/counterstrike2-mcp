//! `console_tail` / `console_wait` through `build_server` (no bridge: the discovery path is absent).
#![allow(clippy::result_large_err)]
use client_mcp::state::ClientConfig;
use cs2_mcp_common::mock::podman::FakePodman;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{CallFailure, Mutability, ToolError};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct TempLog(PathBuf);

impl TempLog {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        TempLog(std::env::temp_dir().join(format!(
            "cs2mcp-tools-{tag}-{}-{nanos}.log",
            std::process::id()
        )))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempLog {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

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

type Server = cs2_mcp_common::mcp::McpServer<client_mcp::state::ClientState>;

fn server_for(log: &TempLog, extra: &[&str]) -> Server {
    let mut args = vec![
        "--discovery",
        "/tmp/none/session.json",
        "--console-log",
        log.path().to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    client_mcp::build_server(&config(&args)).unwrap()
}

/// Calls a tool. A `NotImplemented*` answer is a failure of the test: these tools are real.
async fn call(server: &Server, name: &str, args: Value) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => {
            assert!(
                !e.name.starts_with("NotImplemented") && !e.type_.starts_with("NotImplemented"),
                "{name} answered NotImplemented: {e:?}"
            );
            Err(e)
        }
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

/// Appends `text` to the log after `delay` from a concurrent task.
fn append_later(path: &Path, delay: Duration, text: &'static str) {
    let path = path.to_path_buf();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    });
}

#[test]
fn console_tools_are_readonly_and_bridge_free() {
    let log = TempLog::new("registry");
    let server = server_for(&log, &[]);
    for name in ["console_tail", "console_wait"] {
        let spec = server.registry().spec(name).unwrap();
        assert_eq!(spec.mutability, Mutability::ReadOnly, "{name}");
        assert!(spec.bridge_methods.is_empty(), "{name}");
    }
}

#[tokio::test]
async fn tail_returns_last_lines_with_shape() {
    let log = TempLog::new("tail");
    std::fs::write(log.path(), "one\ntwo\nthree\nfour\nfive\n").unwrap();
    let server = server_for(&log, &[]);
    let out = call(&server, "console_tail", json!({"lines": 2}))
        .await
        .unwrap();
    assert_eq!(out["lines"], json!(["four", "five"]));
    assert_eq!(out["reachedStartOfFile"], json!(false));
    assert_eq!(out["linesScanned"], json!(2));
    assert_eq!(out["bytesScanned"], json!(24)); // whole 24-byte file in one step
    assert_eq!(out["path"], json!(log.path().display().to_string()));
    assert_eq!(out["cursor"]["offset"], json!(24));
    assert!(out["cursor"]["inode"].is_u64());

    let all = call(&server, "console_tail", json!({})).await.unwrap();
    assert_eq!(all["lines"].as_array().unwrap().len(), 5);
    assert_eq!(all["reachedStartOfFile"], json!(true));
    assert_eq!(all["linesScanned"], json!(5));
    assert_eq!(all["bytesScanned"], json!(24));
}

#[tokio::test]
async fn tail_grep_and_argument_validation() {
    let log = TempLog::new("tail-args");
    std::fs::write(log.path(), "ok 1\nERR a\nok 2\nERR b\n").unwrap();
    let server = server_for(&log, &[]);
    let out = call(&server, "console_tail", json!({"lines": 5, "grep": "^ERR"}))
        .await
        .unwrap();
    assert_eq!(out["lines"], json!(["ERR a", "ERR b"]));
    assert_eq!(out["linesScanned"], json!(4));

    for bad in [
        json!({"lines": 0}),
        json!({"lines": 2001}),
        json!({"lines": "x"}),
    ] {
        let err = call(&server, "console_tail", bad).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams");
    }
    let err = call(&server, "console_tail", json!({"grep": "("}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
}

#[tokio::test]
async fn tail_missing_file_is_typed_and_hinted() {
    let log = TempLog::new("missing");
    let server = server_for(&log, &[]);
    let err = call(&server, "console_tail", json!({})).await.unwrap_err();
    assert_eq!(err.name, "ConsoleLogMissing");
    assert_eq!(err.type_, "ConsoleLogMissing");
    assert!(err.retry_safe);
    assert_eq!(err.data["path"], json!(log.path().display().to_string()));
    assert!(err.data["hint"].as_str().unwrap().contains("--console-log"));
}

#[tokio::test]
async fn tail_then_wait_since_cursor_matches_appended_line() {
    let log = TempLog::new("since");
    std::fs::write(log.path(), "a\nb\n").unwrap();
    let server = server_for(&log, &[]);
    let tail = call(&server, "console_tail", json!({"lines": 1}))
        .await
        .unwrap();
    let cursor = tail["cursor"].clone();
    append_later(log.path(), Duration::from_millis(100), "MARK line\n");
    let out = call(
        &server,
        "console_wait",
        json!({"regex": "MARK", "timeoutMs": 5000, "since": cursor}),
    )
    .await
    .unwrap();
    assert_eq!(out["line"], json!("MARK line"));
}

#[tokio::test]
async fn since_with_other_inode_falls_back_to_start() {
    let log = TempLog::new("since-fallback");
    std::fs::write(log.path(), "EXISTING line\n").unwrap();
    let server = server_for(&log, &[]);
    let tail = call(&server, "console_tail", json!({"lines": 1}))
        .await
        .unwrap();
    let inode = tail["cursor"]["inode"].as_u64().unwrap();
    let out = call(
        &server,
        "console_wait",
        json!({"regex": "EXISTING", "timeoutMs": 2000,
               "since": {"inode": inode.wrapping_add(1), "offset": 14}}),
    )
    .await
    .unwrap();
    assert_eq!(out["line"], json!("EXISTING line"));
}

#[tokio::test]
async fn since_and_from_start_are_exclusive_and_zero_timeout_is_rejected() {
    let log = TempLog::new("wait-rules");
    std::fs::write(log.path(), "x\n").unwrap();
    let server = server_for(&log, &[]);
    let since = json!({"inode": 1, "offset": 0});
    let err = call(
        &server,
        "console_wait",
        json!({"regex": "x", "fromStart": true, "since": since}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    let err = call(
        &server,
        "console_wait",
        json!({"regex": "x", "timeoutMs": 0}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    let err = call(
        &server,
        "console_wait",
        json!({"regex": "x", "since": "nope"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    // Zero is fine when it only looks at what is already there.
    let ok = call(
        &server,
        "console_wait",
        json!({"regex": "x", "timeoutMs": 0, "fromStart": true}),
    )
    .await
    .unwrap();
    assert_eq!(ok["line"], json!("x"));
}

#[tokio::test]
async fn wait_returns_match_with_captures() {
    let log = TempLog::new("wait");
    std::fs::write(log.path(), "HP 1 of 100\n").unwrap(); // pre-existing: must be ignored
    let server = server_for(&log, &[]);
    append_later(log.path(), Duration::from_millis(100), "HP 42 of 100\n");
    let out = call(
        &server,
        "console_wait",
        json!({"regex": r"HP (?P<cur>\d+) of (\d+)", "timeoutMs": 5000}),
    )
    .await
    .unwrap();
    assert_eq!(out["matched"], json!(true));
    assert_eq!(out["line"], json!("HP 42 of 100"));
    assert_eq!(out["captures"]["cur"], json!("42"));
    assert_eq!(out["captures"]["2"], json!("100"));
    assert!(out["waitedMs"].as_u64().unwrap() >= 50);
}

#[tokio::test]
async fn wait_timeout_is_typed_with_last_lines_and_file_state() {
    let log = TempLog::new("wait-timeout");
    let body: String = (0..15).map(|i| format!("line {i}\n")).collect();
    std::fs::write(log.path(), body).unwrap();
    let server = server_for(&log, &[]);
    let err = call(
        &server,
        "console_wait",
        json!({"regex": "never", "timeoutMs": 150}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(err.retry_safe);
    assert_eq!(err.data["regex"], json!("never"));
    assert_eq!(err.data["path"], json!(log.path().display().to_string()));
    assert_eq!(err.data["fileExists"], json!(true));
    assert_eq!(err.data["requestedTimeoutMs"], json!(150));
    assert_eq!(err.data["clampedToBudget"], json!(false));
    assert!(err.data["waitedMs"].as_u64().unwrap() >= 150);
    let last = err.data["lastLines"].as_array().unwrap();
    assert_eq!(last.len(), 10);
    assert_eq!(last[0], json!("line 5"));
    assert_eq!(last[9], json!("line 14"));
}

#[tokio::test]
async fn file_missing_for_whole_wait_is_console_log_missing() {
    let log = TempLog::new("never-appears");
    let server = server_for(&log, &[]);
    let err = call(
        &server,
        "console_wait",
        json!({"regex": "x", "timeoutMs": 200}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "ConsoleLogMissing");
    assert!(err.retry_safe);
    assert_eq!(err.data["path"], json!(log.path().display().to_string()));
    assert!(err.data["hint"].as_str().unwrap().contains("--console-log"));
}

#[tokio::test]
async fn wait_timeout_is_clamped_to_call_budget() {
    let log = TempLog::new("wait-budget");
    std::fs::write(log.path(), "").unwrap();
    // 1500 ms budget: wait at most ~1000 ms (budget minus the 500 ms margin), not 60 s.
    let server = server_for(&log, &["--tool-budget-ms", "1500"]);
    let started = Instant::now();
    let err = call(
        &server,
        "console_wait",
        json!({"regex": "never", "timeoutMs": 60000}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(started.elapsed() < Duration::from_millis(3000));
    assert!(err.data["timeoutMs"].as_u64().unwrap() <= 1000);
    assert_eq!(err.data["requestedTimeoutMs"], json!(60000));
    assert_eq!(err.data["clampedToBudget"], json!(true));
}

#[tokio::test]
async fn less_than_margin_budget_is_budget_exceeded_not_timeout() {
    let log = TempLog::new("wait-no-budget");
    std::fs::write(log.path(), "").unwrap();
    let server = server_for(&log, &["--tool-budget-ms", "400"]);
    let err = call(
        &server,
        "console_wait",
        json!({"regex": "never", "timeoutMs": 60000}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "ToolBudgetExceeded");
}

#[tokio::test]
async fn wait_argument_validation_and_file_appears_later() {
    let log = TempLog::new("wait-args");
    let server = server_for(&log, &[]);
    let err = call(&server, "console_wait", json!({})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    let err = call(
        &server,
        "console_wait",
        json!({"regex": "(", "timeoutMs": 1}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    let err = call(
        &server,
        "console_wait",
        json!({"regex": "x", "fromStart": "yes"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams");

    // The file does not exist yet: console_wait waits for it to appear.
    tokio::spawn({
        let path = log.path().to_path_buf();
        async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            std::fs::write(&path, "BOOT OK\n").unwrap();
        }
    });
    let out = call(
        &server,
        "console_wait",
        json!({"regex": "BOOT OK", "timeoutMs": 5000, "fromStart": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["line"], json!("BOOT OK"));
}

// ---- which console.log: flag > session arg > single running session > default ----------------

/// Server with a fake podman and a temp project root, and NO `--console-log` unless `extra` has one.
fn server_with_podman(fake: &FakePodman, proj: &Path, extra: &[&str]) -> Server {
    let bin = fake.bin.display().to_string();
    let proj = proj.display().to_string();
    let mut args = vec![
        "--discovery",
        "/tmp/none/session.json",
        "--podman",
        bin.as_str(),
        "--project-root",
        proj.as_str(),
    ];
    args.extend_from_slice(extra);
    client_mcp::build_server(&config(&args)).unwrap()
}

/// A discovered (not launched by this server) Linux session whose artifacts dir is mounted at /work.
fn running_linux(fake: &FakePodman, name: &str, artifacts: &Path) {
    std::fs::create_dir_all(artifacts).unwrap();
    fake.add_running(name);
    let mounts = json!([{
        "Type": "bind",
        "Name": "",
        "Source": artifacts.display().to_string(),
        "Destination": "/work"
    }]);
    fake.set_mounts(name, &mounts.to_string());
}

/// The default console.log under the project root.
fn default_log(proj: &Path) -> PathBuf {
    proj.join("workshop/run/app/game/csgo/console.log")
}

fn write_log(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn flag_wins_over_running_sessions_and_session_arg() {
    let fake = FakePodman::new("log-flag");
    let proj = fake.dir().join("project");
    let flag = fake.dir().join("flag.log");
    std::fs::write(&flag, "from the flag\n").unwrap();
    let artifacts = proj.join("artifacts/cs2mcp-linux-client-aaaaaaaa-1");
    write_log(&artifacts.join("console.log"), "from the session\n");
    running_linux(&fake, "cs2mcp-linux-client-aaaaaaaa", &artifacts);
    let server = server_with_podman(&fake, &proj, &["--console-log", flag.to_str().unwrap()]);

    let out = call(
        &server,
        "console_tail",
        json!({"session": "cs2mcp-linux-client-aaaaaaaa"}),
    )
    .await
    .unwrap();
    assert_eq!(out["source"], json!("flag"));
    assert_eq!(out["path"], json!(flag.display().to_string()));
    assert_eq!(out["lines"], json!(["from the flag"]));
    assert!(out.get("session").is_none());
    assert!(out["sourceNote"].as_str().unwrap().contains("ignored"));
}

#[tokio::test]
async fn single_running_session_is_read_instead_of_the_stale_default() {
    let fake = FakePodman::new("log-single");
    let proj = fake.dir().join("project");
    write_log(&default_log(&proj), "stale line from an old run\n");
    let artifacts = proj.join("artifacts/cs2mcp-linux-client-bbbbbbbb-1");
    write_log(&artifacts.join("console.log"), "live line\n");
    running_linux(&fake, "cs2mcp-linux-client-bbbbbbbb", &artifacts);
    let server = server_with_podman(&fake, &proj, &[]);

    let out = call(&server, "console_tail", json!({})).await.unwrap();
    assert_eq!(out["source"], json!("single-session"));
    assert_eq!(out["session"], json!("cs2mcp-linux-client-bbbbbbbb"));
    assert_eq!(
        out["path"],
        json!(artifacts.join("console.log").display().to_string())
    );
    assert_eq!(out["lines"], json!(["live line"]));

    // console_wait resolves the same way and reports the same source
    let waited = call(
        &server,
        "console_wait",
        json!({"regex": "live", "timeoutMs": 2000, "fromStart": true}),
    )
    .await
    .unwrap();
    assert_eq!(waited["source"], json!("single-session"));
    assert_eq!(waited["line"], json!("live line"));
}

#[tokio::test]
async fn several_running_sessions_need_a_session_argument() {
    let fake = FakePodman::new("log-several");
    let proj = fake.dir().join("project");
    let a = proj.join("artifacts/cs2mcp-linux-client-cccccccc-1");
    let b = proj.join("artifacts/cs2mcp-linux-client-dddddddd-1");
    write_log(&a.join("console.log"), "A says hi\n");
    write_log(&b.join("console.log"), "B says hi\n");
    running_linux(&fake, "cs2mcp-linux-client-cccccccc", &a);
    running_linux(&fake, "cs2mcp-linux-client-dddddddd", &b);
    let server = server_with_podman(&fake, &proj, &[]);

    let err = call(&server, "console_tail", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    let msg = &err.message;
    assert!(msg.contains("cs2mcp-linux-client-cccccccc"), "{msg}");
    assert!(msg.contains("cs2mcp-linux-client-dddddddd"), "{msg}");
    let err = call(
        &server,
        "console_wait",
        json!({"regex": "x", "timeoutMs": 0, "fromStart": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");

    let out = call(
        &server,
        "console_tail",
        json!({"session": "cs2mcp-linux-client-dddddddd"}),
    )
    .await
    .unwrap();
    assert_eq!(out["source"], json!("session"));
    assert_eq!(out["session"], json!("cs2mcp-linux-client-dddddddd"));
    assert_eq!(out["lines"], json!(["B says hi"]));
}

#[tokio::test]
async fn session_argument_beats_the_single_running_session_and_may_name_a_stopped_one() {
    let fake = FakePodman::new("log-named");
    let proj = fake.dir().join("project");
    let live = proj.join("artifacts/cs2mcp-linux-client-eeeeeeee-1");
    let old = proj.join("artifacts/cs2mcp-linux-client-ffffffff-1");
    write_log(&live.join("console.log"), "live\n");
    write_log(&old.join("console.log"), "last words of the old run\n");
    running_linux(&fake, "cs2mcp-linux-client-eeeeeeee", &live);
    running_linux(&fake, "cs2mcp-linux-client-ffffffff", &old);
    fake.set_state("cs2mcp-linux-client-ffffffff", "exited 0");
    let server = server_with_podman(&fake, &proj, &[]);

    // without the name: only eeeeeeee runs, so it is the pick
    let auto = call(&server, "console_tail", json!({})).await.unwrap();
    assert_eq!(auto["source"], json!("single-session"));
    // with the name: the stopped container's log, its logs survive the game
    let named = call(
        &server,
        "console_tail",
        json!({"session": "cs2mcp-linux-client-ffffffff"}),
    )
    .await
    .unwrap();
    assert_eq!(named["source"], json!("session"));
    assert_eq!(named["lines"], json!(["last words of the old run"]));
}

#[tokio::test]
async fn unknown_session_name_is_invalid_arguments() {
    let fake = FakePodman::new("log-unknown");
    let proj = fake.dir().join("project");
    let server = server_with_podman(&fake, &proj, &[]);
    let err = call(
        &server,
        "console_tail",
        json!({"session": "cs2mcp-linux-client-00000000"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    let err = call(&server, "console_tail", json!({"session": "cs2vtest"}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
}

#[tokio::test]
async fn no_running_session_falls_back_to_the_default_path() {
    let fake = FakePodman::new("log-default");
    let proj = fake.dir().join("project");
    write_log(&default_log(&proj), "default line\n");
    let server = server_with_podman(&fake, &proj, &[]);
    let out = call(&server, "console_tail", json!({})).await.unwrap();
    assert_eq!(out["source"], json!("default"));
    assert_eq!(out["path"], json!(default_log(&proj).display().to_string()));
    assert_eq!(out["lines"], json!(["default line"]));
    assert!(out.get("session").is_none());
}

/// Server on a mock bridge's discovery file, with the fake podman and an optional flag.
fn server_on_bridge(fake: &FakePodman, proj: &Path, discovery: &Path, extra: &[&str]) -> Server {
    let bin = fake.bin.display().to_string();
    let proj = proj.display().to_string();
    let disc = discovery.display().to_string();
    let mut args = vec![
        "--discovery",
        disc.as_str(),
        "--podman",
        bin.as_str(),
        "--project-root",
        proj.as_str(),
    ];
    args.extend_from_slice(extra);
    client_mcp::build_server(&config(&args)).unwrap()
}

const BRIDGE_CONSOLE_METHODS: &[&str] = &[
    "control.ping",
    "control.status",
    "control.capabilities",
    "control.version",
    "doc.active",
    "console.exec",
];

/// The wine-tools container of the bridge: its app dir at /app, with a console.log in it.
fn running_wine_with_log(fake: &FakePodman, proj: &Path, name: &str, text: &str) -> PathBuf {
    let app = proj.join("workshop/run/app");
    write_log(&app.join("game/csgo/console.log"), text);
    fake.add_running(name);
    fake.set_mounts(
        name,
        &json!([{"Type": "bind", "Name": "", "Source": app.display().to_string(), "Destination": "/app"}])
            .to_string(),
    );
    app.join("game/csgo/console.log")
}

#[tokio::test]
async fn console_exec_cursor_comes_from_the_bridge_session_not_the_single_pick() {
    let fake = FakePodman::new("exec-bridge");
    let proj = fake.dir().join("project");
    let mb = MockBridge::builder()
        .methods(BRIDGE_CONSOLE_METHODS.iter().copied())
        .responder_value("console.exec", json!({"queued": true}))
        .start()
        .await
        .unwrap();
    let linux = proj.join("artifacts/cs2mcp-linux-client-11111111-1");
    write_log(&linux.join("console.log"), "linux\n");
    running_linux(&fake, "cs2mcp-linux-client-11111111", &linux);
    let wine_log = running_wine_with_log(&fake, &proj, "cs2mcp-wine-tools-0badf00d", "wine\n");
    let server = server_on_bridge(&fake, &proj, &mb.discovery_path, &[]);

    // no session argument, one linux and one wine running: the bridge's (wine) log, never an error
    let out = call(&server, "console_exec", json!({"command": "echo hi"}))
        .await
        .unwrap();
    assert_eq!(out["source"], json!("wine-session"));
    assert_eq!(
        out["consoleCursor"]["path"],
        json!(wine_log.display().to_string())
    );
    assert_eq!(out["consoleCursor"]["offset"], json!(5));
    assert_eq!(mb.method_calls("console.exec").len(), 1);

    // a named session: its own log
    let out = call(
        &server,
        "console_exec",
        json!({"command": "echo hi", "session": "cs2mcp-linux-client-11111111"}),
    )
    .await
    .unwrap();
    assert_eq!(out["source"], json!("session"));
    assert_eq!(out["session"], json!("cs2mcp-linux-client-11111111"));
    assert_eq!(
        out["consoleCursor"]["path"],
        json!(linux.join("console.log").display().to_string())
    );
    assert_eq!(mb.method_calls("console.exec").len(), 2);

    // a named session that does not exist: an error, and nothing is queued
    let err = call(
        &server,
        "console_exec",
        json!({"command": "echo hi", "session": "cs2mcp-linux-client-99999999"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert_eq!(mb.method_calls("console.exec").len(), 2);
}

#[tokio::test]
async fn console_exec_ambiguous_linux_sessions_use_the_default_log_and_still_send() {
    let fake = FakePodman::new("exec-ambiguous");
    let proj = fake.dir().join("project");
    let mb = MockBridge::builder()
        .methods(BRIDGE_CONSOLE_METHODS.iter().copied())
        .responder_value("console.exec", json!({"queued": true}))
        .start()
        .await
        .unwrap();
    for id in [
        "cs2mcp-linux-client-aaaa0001",
        "cs2mcp-linux-client-aaaa0002",
    ] {
        let dir = proj.join(format!("artifacts/{id}-1"));
        write_log(&dir.join("console.log"), "x\n");
        running_linux(&fake, id, &dir);
    }
    write_log(&default_log(&proj), "default\n");
    let server = server_on_bridge(&fake, &proj, &mb.discovery_path, &[]);
    let out = call(&server, "console_exec", json!({"command": "echo hi"}))
        .await
        .unwrap();
    assert_eq!(out["source"], json!("default"));
    assert_eq!(
        out["consoleCursor"]["path"],
        json!(default_log(&proj).display().to_string())
    );
    assert!(out.get("error").is_none());
    assert_eq!(mb.method_calls("console.exec").len(), 1);
}

#[tokio::test]
async fn a_cursor_names_its_file_and_a_rotated_inode_restarts_at_zero() {
    let fake = FakePodman::new("log-cursor");
    let proj = fake.dir().join("project");
    let a = proj.join("a.log");
    let b = proj.join("b.log");
    write_log(&a, "one\n");
    write_log(&b, "two\n");
    let tail_a = server_with_podman(&fake, &proj, &["--console-log", a.to_str().unwrap()]);
    // the reader is pointed at b, but the cursor was taken from a
    let flag_b = server_with_podman(&fake, &proj, &["--console-log", b.to_str().unwrap()]);
    let tailed = call(&tail_a, "console_tail", json!({"lines": 1}))
        .await
        .unwrap();
    let cursor = tailed["cursor"].clone();
    assert_eq!(cursor["path"], json!(a.display().to_string()));

    let mut f = std::fs::OpenOptions::new().append(true).open(&a).unwrap();
    f.write_all(b"appended\n").unwrap();
    drop(f);
    let out = call(
        &flag_b,
        "console_wait",
        json!({"regex": "appended", "since": cursor, "timeoutMs": 3000}),
    )
    .await
    .unwrap();
    assert_eq!(out["source"], json!("cursor"));
    assert_eq!(out["path"], json!(a.display().to_string()));
    assert_eq!(out["line"], json!("appended"));
    assert_eq!(out["cursorReset"], json!(false));

    // same file, but the inode is not the one the cursor names: restart at 0 and say so
    let mut bogus = cursor.clone();
    bogus["inode"] = json!(cursor["inode"].as_u64().unwrap() + 1);
    let out = call(
        &tail_a,
        "console_wait",
        json!({"regex": "one", "since": bogus, "timeoutMs": 3000}),
    )
    .await
    .unwrap();
    assert_eq!(out["source"], json!("flag"));
    assert_eq!(out["cursorReset"], json!(true));
    assert_eq!(out["line"], json!("one"));
}

#[tokio::test]
async fn console_tail_is_bounded_by_the_call_budget() {
    let fake = FakePodman::new("log-budget");
    let proj = fake.dir().join("project");
    // 400 ms of budget is less than the 500 ms margin: nothing may be resolved or read
    let server = server_with_podman(&fake, &proj, &["--tool-budget-ms", "400"]);
    let err = call(&server, "console_tail", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "ToolBudgetExceeded", "{err:?}");
    assert!(fake.calls_to("ps").is_empty());
}

#[tokio::test]
async fn unnamed_resolution_inspects_only_running_containers_in_one_call_and_caches_the_list() {
    let fake = FakePodman::new("log-batch");
    let proj = fake.dir().join("project");
    for id in [
        "cs2mcp-linux-client-bbbb0001",
        "cs2mcp-linux-client-bbbb0002",
    ] {
        let dir = proj.join(format!("artifacts/{id}-1"));
        write_log(&dir.join("console.log"), "x\n");
        running_linux(&fake, id, &dir);
    }
    fake.set_state("cs2mcp-linux-client-bbbb0003", "exited 0");
    fake.set_mounts("cs2mcp-linux-client-bbbb0003", "[]");
    let server = server_with_podman(&fake, &proj, &[]);

    // two running: an error that names both, from one batch inspect
    let err = call(&server, "console_tail", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    let inspects = fake.calls_to("inspect");
    assert_eq!(
        inspects.len(),
        1,
        "one podman inspect for every running container"
    );
    assert!(inspects[0]
        .argv
        .iter()
        .any(|a| a == "cs2mcp-linux-client-bbbb0001"));
    assert!(inspects[0]
        .argv
        .iter()
        .any(|a| a == "cs2mcp-linux-client-bbbb0002"));
    assert!(
        inspects[0]
            .argv
            .iter()
            .all(|a| a != "cs2mcp-linux-client-bbbb0003"),
        "the stopped container is not inspected when nothing names it"
    );

    // within the cache window a second resolution asks podman nothing new
    let _ = call(&server, "console_tail", json!({})).await.unwrap_err();
    assert_eq!(fake.calls_to("inspect").len(), 1);
    assert_eq!(fake.calls_to("ps").len(), 1);
}

#[tokio::test]
async fn one_unreadable_session_is_noted_and_does_not_fail_the_list() {
    let fake = FakePodman::new("log-noted");
    let proj = fake.dir().join("project");
    let good = proj.join("artifacts/cs2mcp-linux-client-cccc0001-1");
    write_log(&good.join("console.log"), "good line\n");
    running_linux(&fake, "cs2mcp-linux-client-cccc0001", &good);
    // running, but its mounts cannot be read: listed by ps, inspect fails for it alone
    fake.add_running("cs2mcp-linux-client-cccc0002");
    fake.fail_inspect("cs2mcp-linux-client-cccc0002");
    let server = server_with_podman(&fake, &proj, &[]);
    // the list still answers (no podman failure), the bad entry carries a note, and the good
    // session is the one the tools pick: the unreadable one has no log path to pick
    let session_status = call(&server, "session_status", json!({})).await.unwrap();
    assert!(session_status["sessionsNote"].is_null());
    assert_eq!(
        session_status["runtime"]["consoleLogSource"],
        json!("single-session"),
        "{session_status}"
    );
    let bad = session_status["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["container"] == json!("cs2mcp-linux-client-cccc0002"))
        .unwrap()
        .clone();
    assert!(
        bad["note"].as_str().unwrap().contains("inspect failed"),
        "{bad}"
    );
    assert!(bad["consoleLog"].is_null());
}

#[tokio::test]
async fn session_status_reports_what_the_tools_would_pick() {
    let fake = FakePodman::new("log-status-pick");
    let proj = fake.dir().join("project");
    let one = proj.join("artifacts/cs2mcp-linux-client-dddd0001-1");
    let two = proj.join("artifacts/cs2mcp-linux-client-dddd0002-1");
    write_log(&one.join("console.log"), "one\n");
    write_log(&two.join("console.log"), "two\n");
    running_linux(&fake, "cs2mcp-linux-client-dddd0001", &one);
    let server = server_with_podman(&fake, &proj, &[]);
    let p = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(p["runtime"]["consoleLogSource"], json!("single-session"));
    assert_eq!(
        p["runtime"]["consoleLog"],
        json!(one.join("console.log").display().to_string())
    );
    assert_eq!(p["runtime"]["consoleLogExists"], json!(true));

    running_linux(&fake, "cs2mcp-linux-client-dddd0002", &two);
    // the list is cached for 1.5 s per runtime: a new server (its own runtime) sees the new state
    let server = server_with_podman(&fake, &proj, &[]);
    let p = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(p["runtime"]["consoleLogSource"], json!("ambiguous"), "{p}");
    assert!(p["runtime"]["consoleLog"].is_null());
    assert!(p["runtime"]["consoleLogNote"]
        .as_str()
        .unwrap()
        .contains("cs2mcp-linux-client-dddd0001"));
}
