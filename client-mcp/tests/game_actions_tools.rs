//! `setpos` and `console_exec {confirm}` against the mock bridge. setpos: the exact console
//! command, the in-process `at` wait converging on game.local, the fail-fast and validation paths
//! sending nothing, and a never-arriving player timing out not retry-safe. confirm: the marker
//! `Unknown command` line in a temp console.log (appended by the mock when the marker arrives).
#![allow(clippy::result_large_err)]
use client_mcp::state::ClientConfig;
use cs2_mcp_common::mock::{MockBridge, MockBuilder};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const METHODS: [&str; 11] = [
    "control.ping",
    "control.status",
    "control.capabilities",
    "control.version",
    "doc.active",
    "game.tick",
    "game.local",
    "console.exec",
    "input.status",
    "sound.drain",
    "sound.status",
];

const TARGET: [f64; 3] = [1.0, 2.0, 3.0];

struct TempLog(PathBuf);

impl TempLog {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        TempLog(std::env::temp_dir().join(format!(
            "cs2mcp-actions-{tag}-{}-{nanos}.log",
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

async fn server_for(mb: &MockBridge, extra: &[&str]) -> Server {
    let mut args = vec![
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--deadline-ms",
        "7000",
    ];
    args.extend_from_slice(extra);
    client_mcp::build_server(&config(&args)).unwrap()
}

async fn call(server: &Server, name: &str, args: Value) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

fn sent(mb: &MockBridge, method: &str) -> Vec<Value> {
    mb.method_calls(method)
        .into_iter()
        .map(|r| r.params)
        .collect()
}

/// The params sent for `method`, without the call's `deadlineMs` (what the tool meant to send).
fn sent_bare(mb: &MockBridge, method: &str) -> Vec<Value> {
    sent(mb, method)
        .into_iter()
        .map(|mut p| {
            if let Value::Object(m) = &mut p {
                m.remove("deadlineMs");
            }
            p
        })
        .collect()
}

/// A local player at `origin` / `view`, alive and in game, on tick `tick`.
fn local(tick: u64, origin: [f64; 3], view: [f64; 3]) -> Value {
    json!({
        "tick": tick,
        "inGame": true,
        "map": "editor/zoo/smartprop_zoo",
        "playerSlot": 0,
        "viewAngles": view,
        "player": {"alive": true, "origin": origin, "velocity": [0.0, 0.0, 0.0], "activeWeapon": null}
    })
}

/// A bridge whose player moves to `target` / `view` once console.exec has been called with a setpos
/// (never, when `arrive` is false). The tick advances on every game.local read.
async fn setpos_bridge(arrive: bool, view: [f64; 3]) -> MockBridge {
    setpos_bridge_with_status(arrive, view, json!({"angles": null})).await
}

/// As `setpos_bridge`, with the `input.status` reply given (an active override is `angles`).
async fn setpos_bridge_with_status(arrive: bool, view: [f64; 3], status: Value) -> MockBridge {
    let arrived = Arc::new(AtomicBool::new(false));
    let tick = Arc::new(AtomicU64::new(100));
    let set_arrived = arrived.clone();
    let builder = MockBridge::builder()
        .methods(METHODS)
        .responder_value("input.status", status)
        .responder("console.exec", move |_| {
            if arrive {
                set_arrived.store(true, Ordering::SeqCst);
            }
            json!({"queued": true})
        })
        .responder("game.local", move |_| {
            let n = tick.fetch_add(1, Ordering::SeqCst) + 1;
            if arrived.load(Ordering::SeqCst) {
                local(n, TARGET, view)
            } else {
                local(n, [-500.0, 0.0, 0.0], [0.0, 0.0, 0.0])
            }
        });
    builder.start().await.unwrap()
}

#[tokio::test]
async fn setpos_sends_exact_commands_and_returns_when_converged() {
    let mb = setpos_bridge(true, [0.0, 90.0, 0.0]).await;
    let server = server_for(&mb, &[]).await;
    let out = call(
        &server,
        "setpos",
        json!({"origin": TARGET, "angles": [0.0, 90.0, 0.0]}),
    )
    .await
    .unwrap();
    assert_eq!(
        sent(&mb, "console.exec"),
        vec![
            json!({"command": "setpos_exact 1.000 2.000 3.000; setang_exact 0.000 90.000 0.000", "deadlineMs": 7000})
        ]
    );
    assert_eq!(
        out["command"],
        json!("setpos_exact 1.000 2.000 3.000; setang_exact 0.000 90.000 0.000")
    );
    assert_eq!(out["origin"], json!(TARGET));
    assert_eq!(out["angles"], json!([0.0, 90.0, 0.0]));
    assert_eq!(out["exact"], json!(true));
    assert_eq!(out["after"]["origin"], json!(TARGET));
    assert_eq!(out["toTarget"]["xy"], json!(0.0));
    assert_eq!(out["toTarget"]["z"], json!(0.0));
    assert_eq!(out["toTarget"]["angle"]["yaw"], json!(0.0));
    assert!(out["samples"].as_u64().unwrap() >= 2);
    assert!(out["before"]["origin"].is_array());
}

#[tokio::test]
async fn setpos_without_angles_and_non_exact_uses_plain_commands() {
    let mb = setpos_bridge(true, [0.0, 0.0, 0.0]).await;
    let server = server_for(&mb, &[]).await;
    let out = call(&server, "setpos", json!({"origin": TARGET, "exact": false}))
        .await
        .unwrap();
    let calls = sent(&mb, "console.exec");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["command"], json!("setpos 1.000 2.000 3.000"));
    assert_eq!(out["angles"], Value::Null);
    assert_eq!(out["toTarget"].get("angle"), None);
}

#[tokio::test]
async fn setpos_refuses_when_not_in_game_or_dead_before_any_command() {
    for player in [
        json!({"tick": 5, "inGame": false, "player": null, "map": ""}),
        json!({"tick": 5, "inGame": true, "map": "de_dust2", "player": {"alive": false, "origin": [0.0, 0.0, 0.0]}}),
    ] {
        let mb = MockBridge::builder()
            .methods(METHODS)
            .responder_value("game.local", player)
            .responder_value("console.exec", json!({"queued": true}))
            .start()
            .await
            .unwrap();
        let server = server_for(&mb, &[]).await;
        let err = call(&server, "setpos", json!({"origin": TARGET}))
            .await
            .unwrap_err();
        assert_eq!(err.name, "PrerequisiteFailed");
        assert!(err.retry_safe);
        assert!(sent(&mb, "console.exec").is_empty());
    }
}

#[tokio::test]
async fn setpos_invalid_arguments_send_nothing() {
    let mb = setpos_bridge(true, [0.0, 0.0, 0.0]).await;
    let server = server_for(&mb, &[]).await;
    for bad in [
        json!({}),
        json!({"origin": [1.0, 2.0]}),
        json!({"origin": [1.0, 2.0, "x"]}),
        json!({"origin": [1.0, 2.0, 3.0], "angles": [1.0, 2.0]}),
        json!({"origin": [1.0, 2.0, 3.0], "exact": "yes"}),
        json!({"origin": [1.0, 2.0, 3.0], "angleTolerance": 1.0}),
        json!({"origin": [1.0, 2.0, 3.0], "tolerance": -1.0}),
        json!({"origin": [1.0, 2.0, 3.0], "timeoutMs": 0}),
        json!({"origin": [1.0, 2.0, 3.0], "bogus": true}),
    ] {
        let err = call(&server, "setpos", bad.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    assert!(sent(&mb, "console.exec").is_empty());
    assert!(sent(&mb, "game.local").is_empty());
}

#[tokio::test]
async fn setpos_that_never_arrives_is_a_non_retry_safe_timeout() {
    let mb = setpos_bridge(false, [0.0, 0.0, 0.0]).await;
    let server = server_for(&mb, &[]).await;
    let err = call(
        &server,
        "setpos",
        json!({"origin": TARGET, "timeoutMs": 300}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(!err.retry_safe);
    assert_eq!(err.data["commandsSent"], json!(true));
    assert_eq!(err.data["reason"], json!("timeout"));
    assert_eq!(err.data["last"]["origin"], json!([-500.0, 0.0, 0.0]));
    // the command went out exactly once: no retry inside the tool
    assert_eq!(sent(&mb, "console.exec").len(), 1);
}

/// A local player with a flags value and a vertical speed, as `game.local` sends it.
fn falling_local(tick: u64, origin: [f64; 3], vz: f64, flags: i64) -> Value {
    json!({
        "tick": tick,
        "inGame": true,
        "map": "editor/zoo/smartprop_zoo",
        "playerSlot": 0,
        "viewAngles": [0.0, 0.0, 0.0],
        "player": {"alive": true, "origin": origin, "velocity": [0.0, 0.0, vz],
                   "flags": flags, "activeWeapon": null}
    })
}

/// A bridge for setpos `floor`: before the setpos the player stands at `start`; after it, each
/// game.local read takes the next `(z, vz, flags)` frame at the target xy (the last frame repeats).
async fn floor_bridge(frames: Vec<(f64, f64, i64)>) -> MockBridge {
    let armed = Arc::new(AtomicBool::new(false));
    let tick = Arc::new(AtomicU64::new(100));
    let next = Arc::new(AtomicU64::new(0));
    let set_armed = armed.clone();
    let frames = Arc::new(frames);
    let builder = MockBridge::builder()
        .methods(METHODS)
        .responder_value("input.status", json!({"angles": null}))
        .responder("console.exec", move |_| {
            set_armed.store(true, Ordering::SeqCst);
            json!({"queued": true})
        })
        .responder("game.local", move |_| {
            let n = tick.fetch_add(1, Ordering::SeqCst) + 1;
            if !armed.load(Ordering::SeqCst) {
                return falling_local(n, [-500.0, 0.0, 0.0], 0.0, 65665);
            }
            let i = (next.fetch_add(1, Ordering::SeqCst) as usize).min(frames.len() - 1);
            let (z, vz, flags) = frames[i];
            falling_local(n, [TARGET[0], TARGET[1], z], vz, flags)
        });
    builder.start().await.unwrap()
}

/// The player drops from 27 u above the requested z, lands at -20 (on the ground, stopped on two
/// samples), and stays there.
fn landing_frames() -> Vec<(f64, f64, i64)> {
    vec![
        (27.0, -300.0, 0),
        (10.0, -400.0, 0),
        (-15.0, -300.0, 0),
        (-20.0, 0.0, 65665),
        (-20.0, 0.0, 65665),
        (-20.0, 0.0, 65665),
    ]
}

#[tokio::test]
async fn setpos_floor_waits_for_the_landing_and_reports_the_drop() {
    let mb = floor_bridge(landing_frames()).await;
    let server = server_for(&mb, &[]).await;
    let out = call(
        &server,
        "setpos",
        json!({"origin": TARGET, "floor": true, "timeoutMs": 3000}),
    )
    .await
    .unwrap();
    assert_eq!(out["command"], json!("setpos_exact 1.000 2.000 3.000"));
    assert_eq!(out["floor"], json!(true));
    assert_eq!(out["landedZ"], json!(-20.0));
    assert_eq!(out["dropped"], json!(23.0));
    assert_eq!(out["after"]["origin"], json!([1.0, 2.0, -20.0]));
    assert_eq!(out["minZ"], json!(null));
    // the command went out once, and the wait saw the fall before the match
    assert_eq!(sent(&mb, "console.exec").len(), 1);
    assert!(out["samples"].as_u64().unwrap() >= 5);
}

#[tokio::test]
async fn setpos_floor_false_keeps_the_at_wait_and_the_reply_has_no_landing_keys() {
    // the same fall: the z check of the plain `at` wait never matches a landing 23 u below the target
    let mb = floor_bridge(landing_frames()).await;
    let server = server_for(&mb, &[]).await;
    let err = call(
        &server,
        "setpos",
        json!({"origin": TARGET, "timeoutMs": 300}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(!err.retry_safe);
    assert_eq!(err.data["commandsSent"], json!(true));
    assert_eq!(sent(&mb, "console.exec").len(), 1);
    // and a successful floor:false reply carries no floor keys
    let mb = setpos_bridge(true, [0.0, 0.0, 0.0]).await;
    let server = server_for(&mb, &[]).await;
    let out = call(&server, "setpos", json!({"origin": TARGET, "exact": true}))
        .await
        .unwrap();
    assert!(
        out.get("landedZ").is_none() && out.get("dropped").is_none() && out.get("floor").is_none()
    );
}

#[tokio::test]
async fn setpos_floor_fall_past_the_limit_is_fell_out_of_map() {
    // lands 603 u below the target: past the 512 u limit, so the wait ends on that sample
    let mb = floor_bridge(vec![
        (27.0, -300.0, 0),
        (-200.0, -900.0, 0),
        (-600.0, -900.0, 0),
    ])
    .await;
    let server = server_for(&mb, &[]).await;
    let err = call(
        &server,
        "setpos",
        json!({"origin": TARGET, "floor": true, "timeoutMs": 3000}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "FellOutOfMap");
    assert!(!err.retry_safe);
    assert_eq!(err.data["commandsSent"], json!(true));
    assert_eq!(err.data["command"], json!("setpos_exact 1.000 2.000 3.000"));
    assert_eq!(err.data["z"], json!(-600.0));
    assert_eq!(err.data["dropped"], json!(603.0));
    assert_eq!(err.data["maxDrop"], json!(512.0));
    assert!(err.message.contains("fell out of the map"));
}

#[tokio::test]
async fn setpos_floor_min_z_fails_a_landing_below_it() {
    let mb = floor_bridge(landing_frames()).await;
    let server = server_for(&mb, &[]).await;
    let err = call(
        &server,
        "setpos",
        json!({"origin": TARGET, "floor": true, "minZ": -10.0, "timeoutMs": 3000}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "FellOutOfMap");
    assert!(!err.retry_safe);
    assert_eq!(err.data["minZ"], json!(-10.0));
    // a minZ under the landing lets the same fall match
    let mb = floor_bridge(landing_frames()).await;
    let server = server_for(&mb, &[]).await;
    let out = call(
        &server,
        "setpos",
        json!({"origin": TARGET, "floor": true, "minZ": -30.0, "timeoutMs": 3000}),
    )
    .await
    .unwrap();
    assert_eq!(out["minZ"], json!(-30.0));
    assert_eq!(out["landedZ"], json!(-20.0));
}

#[tokio::test]
async fn setpos_floor_arguments_are_validated_and_send_nothing() {
    let mb = floor_bridge(landing_frames()).await;
    let server = server_for(&mb, &[]).await;
    for bad in [
        json!({"origin": TARGET, "floor": "yes"}),
        json!({"origin": TARGET, "minZ": -10.0}),
        json!({"origin": TARGET, "floor": true, "minZ": "low"}),
        json!({"origin": TARGET, "floor": true, "minZ": 5.0}),
    ] {
        let err = call(&server, "setpos", bad.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    assert!(sent(&mb, "console.exec").is_empty());
}

/// A bridge for setpos settling: the k-th setpos (console.exec) puts the player at the target plus
/// `offsets[k-1]` (the last offset repeats), standing still. Before any setpos the player is at
/// (-500, 0, 0), moving at 250 u/s for the first `moving_reads` game.local reads.
async fn drift_bridge(offsets: Vec<[f64; 3]>, moving_reads: u64) -> MockBridge {
    let sent = Arc::new(AtomicU64::new(0));
    let tick = Arc::new(AtomicU64::new(100));
    let set_sent = sent.clone();
    let offsets = Arc::new(offsets);
    let builder = MockBridge::builder()
        .methods(METHODS)
        .responder_value("input.status", json!({"angles": null}))
        .responder("console.exec", move |_| {
            set_sent.fetch_add(1, Ordering::SeqCst);
            json!({"queued": true})
        })
        .responder("game.local", move |_| {
            let n = tick.fetch_add(1, Ordering::SeqCst) + 1;
            let k = sent.load(Ordering::SeqCst) as usize;
            if k == 0 {
                let moving = n - 100 <= moving_reads;
                let velocity = if moving {
                    [250.0, 0.0, 0.0]
                } else {
                    [0.0, 0.0, 0.0]
                };
                let mut v = local(n, [-500.0, 0.0, 0.0], [0.0, 0.0, 0.0]);
                v["player"]["velocity"] = json!(velocity);
                return v;
            }
            let off = offsets[(k - 1).min(offsets.len() - 1)];
            local(
                n,
                [TARGET[0] + off[0], TARGET[1] + off[1], TARGET[2] + off[2]],
                [0.0, 0.0, 0.0],
            )
        });
    builder.start().await.unwrap()
}

#[tokio::test]
async fn setpos_that_drifts_off_and_rests_is_sent_again_and_matches() {
    // the first setpos lands 10 u off in x and stays there: the tool re-sends the same command
    let mb = drift_bridge(vec![[10.0, 0.0, 0.0], [0.0, 0.0, 0.0]], 0).await;
    let server = server_for(&mb, &[]).await;
    let out = call(
        &server,
        "setpos",
        json!({"origin": TARGET, "timeoutMs": 3000}),
    )
    .await
    .unwrap();
    assert_eq!(out["resends"], json!(1));
    assert_eq!(out["waitedForStop"], json!(false));
    assert_eq!(out["after"]["origin"], json!([1.0, 2.0, 3.0]));
    let cmds = sent(&mb, "console.exec");
    assert_eq!(cmds.len(), 2);
    assert_eq!(cmds[1]["command"], cmds[0]["command"]);
}

#[tokio::test]
async fn setpos_that_never_converges_stops_after_two_resends_not_retry_safe() {
    let mb = drift_bridge(vec![[10.0, 0.0, 0.0]], 0).await;
    let server = server_for(&mb, &[]).await;
    let err = call(
        &server,
        "setpos",
        json!({"origin": TARGET, "timeoutMs": 3000}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(!err.retry_safe);
    assert_eq!(err.data["resends"], json!(2));
    assert_eq!(err.data["commandsSent"], json!(true));
    // the command: once, then the two re-sends and no more
    assert_eq!(sent(&mb, "console.exec").len(), 3);
}

#[tokio::test]
async fn setpos_waits_for_a_moving_player_to_stop_before_sending() {
    // moving for the first three reads (the pre-check included): the command goes out at rest
    let mb = drift_bridge(vec![[0.0, 0.0, 0.0]], 3).await;
    let server = server_for(&mb, &[]).await;
    let out = call(
        &server,
        "setpos",
        json!({"origin": TARGET, "timeoutMs": 3000}),
    )
    .await
    .unwrap();
    assert_eq!(out["waitedForStop"], json!(true));
    assert_eq!(out["resends"], json!(0));
    assert_eq!(sent(&mb, "console.exec").len(), 1);
}

// ---- console_exec {confirm} ------------------------------------------------------------------

/// Mock whose console.exec appends `Unknown command: <name>` to `log` when the marker arrives
/// (the engine's warning for an unknown name), and nothing for other commands.
async fn confirm_bridge(log: &Path, warn_on_marker: bool) -> MockBridge {
    let log = log.to_path_buf();
    let builder = MockBridge::builder()
        .methods(METHODS)
        .responder("console.exec", move |params| {
            let command = params["command"].as_str().unwrap_or_default().to_string();
            // a mistyped command: the engine logs its warning as the command runs, before the marker
            if command.starts_with("bogus_") {
                let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
                writeln!(f, "Unknown command: {command}").unwrap();
            }
            if warn_on_marker && command.starts_with("cs2mcp_mark_") {
                let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
                writeln!(f, "Unknown command: {command}").unwrap();
            }
            json!({"queued": true})
        });
    builder.start().await.unwrap()
}

#[tokio::test]
async fn console_exec_confirm_waits_for_the_marker_warning() {
    let log = TempLog::new("confirm");
    std::fs::write(log.path(), "older line\n").unwrap();
    let mb = confirm_bridge(log.path(), true).await;
    let server = server_for(&mb, &["--console-log", log.path().to_str().unwrap()]).await;
    let out = call(
        &server,
        "console_exec",
        json!({"command": "setpos_exact 1 2 3", "confirm": true, "confirmTimeoutMs": 3000}),
    )
    .await
    .unwrap();
    assert_eq!(out["confirmed"], json!(true));
    assert!(out["confirmMs"].as_u64().is_some());
    assert_eq!(out["command"], json!("setpos_exact 1 2 3"));
    assert_eq!(out["route"], json!("bridge"));
    assert!(out.get("note").is_none());
    let marker = out["marker"].as_str().unwrap().to_string();
    assert!(marker.starts_with("cs2mcp_mark_"));
    // the command first, then the marker: FIFO proof
    let calls = sent(&mb, "console.exec");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["command"], json!("setpos_exact 1 2 3"));
    assert_eq!(calls[1]["command"], json!(marker));
}

#[tokio::test]
async fn console_exec_confirm_timeout_is_not_retry_safe() {
    let log = TempLog::new("confirm-timeout");
    std::fs::write(log.path(), "older line\n").unwrap();
    let mb = confirm_bridge(log.path(), false).await;
    let server = server_for(&mb, &["--console-log", log.path().to_str().unwrap()]).await;
    let err = call(
        &server,
        "console_exec",
        json!({"command": "setpos_exact 1 2 3", "confirm": true, "confirmTimeoutMs": 300}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(!err.retry_safe);
    assert_eq!(err.data["commandsSent"], json!(true));
    assert_eq!(err.data["lastLines"], json!(["older line"]));
    assert_eq!(sent(&mb, "console.exec").len(), 2);
}

#[tokio::test]
async fn console_exec_confirm_without_a_log_sends_nothing() {
    let log = TempLog::new("confirm-missing"); // never created
    let mb = confirm_bridge(log.path(), true).await;
    let server = server_for(&mb, &["--console-log", log.path().to_str().unwrap()]).await;
    let err = call(
        &server,
        "console_exec",
        json!({"command": "setpos_exact 1 2 3", "confirm": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "ConsoleLogMissing");
    assert!(err.retry_safe);
    assert!(sent(&mb, "console.exec").is_empty());
}

#[tokio::test]
async fn console_exec_confirm_arguments_are_validated_and_refused_commands_stay_refused() {
    let log = TempLog::new("confirm-args");
    std::fs::write(log.path(), "").unwrap();
    let mb = confirm_bridge(log.path(), true).await;
    let server = server_for(&mb, &["--console-log", log.path().to_str().unwrap()]).await;
    for bad in [
        json!({"command": "echo hi", "confirm": "yes"}),
        json!({"command": "echo hi", "confirm": true, "confirmTimeoutMs": 0}),
        json!({"command": "echo hi", "confirmTimeoutMs": 100}),
        json!({"command": "echo hi", "confirm": false, "confirmTimeoutMs": 100}),
    ] {
        let err = call(&server, "console_exec", bad.clone())
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    let err = call(
        &server,
        "console_exec",
        json!({"command": "quit", "confirm": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "ForbiddenCommand");
    assert!(sent(&mb, "console.exec").is_empty());
}

#[tokio::test]
async fn setpos_error_after_the_command_is_not_retry_safe() {
    // alive for the pre-check, dead from the first wait sample on: the command was already sent
    let reads = Arc::new(AtomicU64::new(0));
    let r = reads.clone();
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("game.local", move |_| {
            let n = r.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                local(100, [-500.0, 0.0, 0.0], [0.0, 0.0, 0.0])
            } else {
                json!({"tick": 100 + n, "inGame": true, "map": "de_dust2", "player": {"alive": false, "origin": [0.0, 0.0, 0.0]}})
            }
        })
        .responder_value("console.exec", json!({"queued": true}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, &[]).await;
    let err = call(&server, "setpos", json!({"origin": TARGET}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert!(!err.retry_safe);
    assert_eq!(err.data["commandsSent"], json!(true));
    assert_eq!(err.data["command"], json!("setpos_exact 1.000 2.000 3.000"));
    assert_eq!(sent(&mb, "console.exec").len(), 1);
}

#[tokio::test]
async fn setpos_refuses_an_active_input_angle_override_before_sending() {
    let mb = setpos_bridge_with_status(true, [0.0, 0.0, 0.0], json!({"angles": [10.0, 20.0, 0.0]}))
        .await;
    let server = server_for(&mb, &[]).await;
    let err = call(
        &server,
        "setpos",
        json!({"origin": TARGET, "angles": [0.0, 90.0, 0.0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert!(err.message.contains("input_clear first"));
    assert!(sent(&mb, "console.exec").is_empty());
    // without angles the override does not matter: the view is not touched
    let out = call(&server, "setpos", json!({"origin": TARGET}))
        .await
        .unwrap();
    assert_eq!(out["origin"], json!(TARGET));
    assert_eq!(sent(&mb, "console.exec").len(), 1);
}

#[tokio::test]
async fn setpos_normalises_angles_and_rejects_out_of_range_ones() {
    let mb = setpos_bridge(true, [0.0, -90.0, 0.0]).await;
    let server = server_for(&mb, &[]).await;
    let out = call(
        &server,
        "setpos",
        json!({"origin": TARGET, "angles": [0.0, 270.0, 0.0]}),
    )
    .await
    .unwrap();
    assert_eq!(out["angles"], json!([0.0, -90.0, 0.0]));
    assert_eq!(
        sent(&mb, "console.exec")[0]["command"],
        json!("setpos_exact 1.000 2.000 3.000; setang_exact 0.000 -90.000 0.000")
    );
    let before = sent(&mb, "console.exec").len();
    for bad in [
        json!({"origin": TARGET, "angles": [95.0, 0.0, 0.0]}),
        json!({"origin": TARGET, "angles": [0.0, 0.0, 181.0]}),
        json!({"origin": [40000.0, 0.0, 0.0]}),
        json!({"origin": [0.0, -32769.0, 0.0]}),
    ] {
        let err = call(&server, "setpos", bad.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    assert_eq!(sent(&mb, "console.exec").len(), before);
}

#[tokio::test]
async fn setpos_zero_tolerance_is_raised_to_the_minimum_and_still_matches() {
    let mb = setpos_bridge(true, [0.0, 0.0, 0.0]).await;
    let server = server_for(&mb, &[]).await;
    let out = call(
        &server,
        "setpos",
        json!({"origin": TARGET, "tolerance": 0, "zTolerance": 0}),
    )
    .await
    .unwrap();
    assert_eq!(out["origin"], json!(TARGET));
}

#[tokio::test]
async fn setpos_origin_reached_but_view_not_gives_a_hint() {
    // the player arrives at origin but keeps looking at [0, 0]: the wait cannot match the view
    let mb = setpos_bridge(true, [0.0, 0.0, 0.0]).await;
    let server = server_for(&mb, &[]).await;
    let err = call(
        &server,
        "setpos",
        json!({"origin": TARGET, "angles": [0.0, 90.0, 0.0], "timeoutMs": 300}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(!err.retry_safe);
    assert!(err.data["hint"].as_str().unwrap().contains("input_clear"));
    assert!(err.message.contains("input_clear"));
}

#[tokio::test]
async fn setpos_budget_too_small_fails_retry_safe_before_sending() {
    // a 700 ms call budget cannot cover the wait margin (500 ms) plus one sample
    let mb = setpos_bridge(true, [0.0, 0.0, 0.0]).await;
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--tool-budget-ms",
        "700",
    ]))
    .unwrap();
    let err = call(&server, "setpos", json!({"origin": TARGET}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "ToolBudgetExceeded");
    assert!(err.retry_safe);
    assert!(sent(&mb, "console.exec").is_empty());
}

#[tokio::test]
async fn console_exec_confirm_reports_warnings_and_clamping() {
    let log = TempLog::new("confirm-warn");
    std::fs::write(log.path(), "").unwrap();
    let mb = confirm_bridge(log.path(), true).await;
    let server = server_for(&mb, &["--console-log", log.path().to_str().unwrap()]).await;
    let out = call(
        &server,
        "console_exec",
        json!({"command": "bogus_typo 1", "confirm": true}),
    )
    .await
    .unwrap();
    // the mistyped command is visible; the marker's own warning is not a warning
    assert_eq!(out["warnings"], json!(["Unknown command: bogus_typo 1"]));
    assert_eq!(out["clampedToBudget"], json!(false));
    assert!(out["confirmed"].as_bool().unwrap());
}

// ---- input_sequence ----------------------------------------------------------------------------

/// The methods input_sequence needs on top of METHODS.
fn sequence_methods() -> Vec<&'static str> {
    METHODS
        .iter()
        .copied()
        .chain(["input.set", "input.clear"])
        .collect()
}

fn names_contain(v: &Value, name: &str) -> bool {
    v.as_array().is_some_and(|a| a.iter().any(|x| x == name))
}

/// A mock for input_sequence. game.local's tick advances 8 per read (the first read is 108). The
/// origin's x advances 10 per read while `forward` is held by the last input.set. input.status
/// reports the last absolute angles set. With `dead_after_sets`, the player is dead once that many
/// input.set calls have been made.
async fn sequence_bridge(dead_after_sets: Option<u64>) -> MockBridge {
    sequence_bridge_with(dead_after_sets, 8, |b| b).await
}

/// As `sequence_bridge`, with the tick advance per read given and extra mock configuration.
async fn sequence_bridge_with(
    dead_after_sets: Option<u64>,
    tick_step: u64,
    customize: impl FnOnce(MockBuilder) -> MockBuilder,
) -> MockBridge {
    let tick = Arc::new(AtomicU64::new(100));
    let forward = Arc::new(AtomicBool::new(false));
    let x = Arc::new(Mutex::new(-500.0_f64));
    let angles = Arc::new(Mutex::new(Value::Null));
    let sets = Arc::new(AtomicU64::new(0));
    let (set_forward, set_angles, set_sets) = (forward.clone(), angles.clone(), sets.clone());
    let (clear_forward, clear_angles) = (forward.clone(), angles.clone());
    let status_angles = angles.clone();
    let builder = MockBridge::builder()
        .methods(sequence_methods())
        .responder("input.set", move |params| {
            if names_contain(&params["press"], "forward") {
                set_forward.store(true, Ordering::SeqCst);
            }
            if names_contain(&params["release"], "forward") {
                set_forward.store(false, Ordering::SeqCst);
            }
            if params["angles"].is_array() {
                *set_angles.lock().unwrap() = params["angles"].clone();
            }
            set_sets.fetch_add(1, Ordering::SeqCst);
            json!({"applied": true})
        })
        .responder("input.clear", move |_| {
            clear_forward.store(false, Ordering::SeqCst);
            *clear_angles.lock().unwrap() = Value::Null;
            json!({"cleared": true})
        })
        .responder("input.status", move |_| {
            json!({"angles": status_angles.lock().unwrap().clone()})
        })
        .responder("game.local", move |_| {
            let n = tick.fetch_add(tick_step, Ordering::SeqCst) + tick_step;
            if forward.load(Ordering::SeqCst) {
                *x.lock().unwrap() += 10.0;
            }
            let xv = *x.lock().unwrap();
            let override_view = angles.lock().unwrap().clone();
            let view = if override_view.is_array() {
                override_view
            } else {
                json!([0.0, 0.0, 0.0])
            };
            let dead = dead_after_sets.is_some_and(|k| sets.load(Ordering::SeqCst) >= k);
            json!({
                "tick": n,
                "inGame": true,
                "map": "editor/zoo/smartprop_zoo",
                "playerSlot": 0,
                "viewAngles": view,
                "player": {"alive": !dead, "origin": [xv, 0.0, 0.0], "velocity": [0.0, 0.0, 0.0], "activeWeapon": null}
            })
        });
    customize(builder).start().await.unwrap()
}

fn input_methods(mb: &MockBridge) -> Vec<String> {
    mb.requests()
        .into_iter()
        .map(|r| r.method)
        .filter(|m| m.starts_with("input."))
        .collect()
}

#[tokio::test]
async fn input_sequence_sends_steps_in_order_then_clears() {
    let mb = sequence_bridge(None).await;
    let server = server_for(&mb, &[]).await;
    let out = call(
        &server,
        "input_sequence",
        json!({"steps": [
            {"press": ["forward"], "holdTicks": 16, "label": "run"},
            {"release": ["forward"], "holdTicks": 8}
        ]}),
    )
    .await
    .unwrap();
    assert_eq!(
        sent(&mb, "input.set"),
        vec![
            json!({"press": ["forward"], "deadlineMs": 7000}),
            json!({"release": ["forward"], "deadlineMs": 7000})
        ]
    );
    assert_eq!(sent(&mb, "input.clear").len(), 1);
    assert_eq!(
        input_methods(&mb),
        vec!["input.set", "input.set", "input.clear"]
    );
    // game.local reads go 108 (baseline), then 8 ticks per read. A step's startTick is the first
    // reading after its set: 116, then 140 (the set's own read comes 8 ticks after each end). Its
    // hold of 16 or 8 ticks counts from there: the steps end at 132 and 148.
    let steps = out["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["label"], json!("run"));
    assert_eq!(steps[0]["startTick"], json!(116));
    assert_eq!(steps[0]["endTick"], json!(132));
    assert_eq!(steps[1]["label"], Value::Null);
    assert_eq!(steps[1]["startTick"], json!(140));
    assert_eq!(steps[1]["endTick"], json!(148));
    // held forward for step 0 (x moves 10 per read), released for step 1 (x stays put)
    assert_eq!(steps[0]["origin"], json!([-470.0, 0.0, 0.0]));
    assert_eq!(steps[1]["origin"], json!([-470.0, 0.0, 0.0]));
    assert_eq!(out["totals"]["ticks"], json!(40));
    assert_eq!(out["totals"]["distance"], json!(30.0));
    assert_eq!(out["final"]["tick"], json!(148));
    assert_eq!(out["inputCleared"], json!(true));
}

#[tokio::test]
async fn input_sequence_turn_resolves_against_the_override() {
    let mb = sequence_bridge(None).await;
    let server = server_for(&mb, &[]).await;
    call(
        &server,
        "input_sequence",
        json!({"steps": [
            {"angles": [0.0, 90.0, 0.0]},
            {"turn": {"dyaw": 10.0}}
        ]}),
    )
    .await
    .unwrap();
    let sets = sent(&mb, "input.set");
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[1]["angles"], json!([0.0, 100.0, 0.0]));
    assert_eq!(sent(&mb, "input.status").len(), 1);
    assert_eq!(sent(&mb, "input.clear").len(), 1);
}

#[tokio::test]
async fn input_sequence_invalid_steps_send_nothing() {
    let mb = sequence_bridge(None).await;
    let server = server_for(&mb, &[]).await;
    let thirty_three: Vec<Value> = (0..33).map(|_| json!({"press": ["forward"]})).collect();
    for bad in [
        json!({}),
        json!({"steps": []}),
        json!({"steps": thirty_three}),
        json!({"steps": [{"press": ["forward"]}, {"press": []}]}),
        json!({"steps": [{"press": ["forward"], "holdTicks": 0}]}),
        json!({"steps": [{"press": ["forward"], "holdTicks": 641}]}),
        json!({"steps": [{"press": ["forward"], "bogus": true}]}),
        json!({"steps": [{"press": ["a b"]}]}),
        json!({"steps": [{"press": ["forward"], "label": "x".repeat(65)}]}),
        json!({"steps": [{"press": ["forward"]}], "clearAfter": false}),
    ] {
        let err = call(&server, "input_sequence", bad.clone())
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    assert!(sent(&mb, "input.set").is_empty());
    assert!(sent(&mb, "input.clear").is_empty());
    assert!(sent(&mb, "game.local").is_empty());
}

#[tokio::test]
async fn input_sequence_refuses_when_not_in_game_before_sending() {
    let mb = MockBridge::builder()
        .methods(sequence_methods())
        .responder_value(
            "game.local",
            json!({"tick": 5, "inGame": true, "map": "de_dust2", "player": {"alive": false, "origin": [0.0, 0.0, 0.0]}}),
        )
        .responder_value("input.set", json!({"applied": true}))
        .responder_value("input.clear", json!({"cleared": true}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, &[]).await;
    let err = call(
        &server,
        "input_sequence",
        json!({"steps": [{"press": ["forward"]}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert!(err.retry_safe);
    assert!(sent(&mb, "input.set").is_empty());
    assert!(sent(&mb, "input.clear").is_empty());
}

#[tokio::test]
async fn input_sequence_death_mid_sequence_is_not_retry_safe_and_still_clears() {
    // the player dies once the second input.set has been made
    let mb = sequence_bridge(Some(2)).await;
    let server = server_for(&mb, &[]).await;
    let err = call(
        &server,
        "input_sequence",
        json!({"steps": [
            {"press": ["forward"], "holdTicks": 4},
            {"release": ["forward"], "holdTicks": 4},
            {"press": ["forward"], "holdTicks": 4}
        ]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert!(!err.retry_safe);
    assert_eq!(err.data["stepsCompleted"], json!(1));
    assert_eq!(err.data["inputSent"], json!(true));
    assert_eq!(err.data["inputCleared"], json!(true));
    assert_eq!(sent(&mb, "input.set").len(), 2);
    assert_eq!(sent(&mb, "input.clear").len(), 1);
    assert_eq!(
        input_methods(&mb),
        vec!["input.set", "input.set", "input.clear"]
    );
}

#[tokio::test]
async fn input_sequence_over_budget_is_refused_before_sending() {
    let mb = sequence_bridge(None).await;
    let server = server_for(&mb, &[]).await;
    // 32 steps of 640 ticks is 320 s of game time, far beyond the 45 s call budget
    let steps: Vec<Value> = (0..32)
        .map(|_| json!({"press": ["forward"], "holdTicks": 640}))
        .collect();
    let err = call(&server, "input_sequence", json!({"steps": steps}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "ToolBudgetExceeded");
    assert!(err.retry_safe);
    assert_eq!(err.data["commandsSent"], json!(false));
    assert!(err.data["maxTotalTicks"].as_u64().is_some());
    assert!(input_methods(&mb).is_empty());
    assert!(sent(&mb, "game.local").is_empty());
}

#[tokio::test]
async fn input_sequence_set_error_mid_run_is_not_retry_safe_and_still_clears() {
    // the second input.set fails on the bridge
    let mb = sequence_bridge_with(None, 8, |b| {
        b.error_on_call("input.set", 2, -32000, "set broke", None)
    })
    .await;
    let server = server_for(&mb, &[]).await;
    let err = call(
        &server,
        "input_sequence",
        json!({"steps": [
            {"press": ["forward"], "holdTicks": 4},
            {"release": ["forward"], "holdTicks": 4},
            {"press": ["forward"], "holdTicks": 4}
        ]}),
    )
    .await
    .unwrap_err();
    assert!(!err.retry_safe);
    assert_eq!(err.data["stepsCompleted"], json!(1));
    assert_eq!(err.data["inputSent"], json!(true));
    assert_eq!(err.data["inputCleared"], json!(true));
    assert_eq!(sent(&mb, "input.set").len(), 2);
    // the third step was never sent; the clear still was
    assert_eq!(sent(&mb, "input.clear").len(), 1);
}

#[tokio::test]
async fn input_sequence_all_steps_ran_but_clear_failed_is_reported() {
    let mb = sequence_bridge_with(None, 8, |b| {
        b.error("input.clear", -32000, "clear broke", None)
    })
    .await;
    let server = server_for(&mb, &[]).await;
    let err = call(
        &server,
        "input_sequence",
        json!({"steps": [{"press": ["forward"], "holdTicks": 4}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "ClearFailed");
    assert!(!err.retry_safe);
    assert_eq!(err.data["inputCleared"], json!("failed"));
    assert_eq!(err.data["stepsCompleted"], json!(1));
    assert_eq!(err.data["inputSent"], json!(true));
    assert_eq!(sent(&mb, "input.clear").len(), 1);
}

#[tokio::test]
async fn input_sequence_budget_boundary_accepts_what_fits_and_refuses_one_more() {
    // 64 ticks per read keeps the real time short. 2688 ticks estimate to 42000 ms + 500 ms of step
    // overhead, plus the 1500 ms margin: 44000 ms, inside the 45 s call budget. 2800 ticks estimate
    // to 43750 + 500 + 1500 = 45750 ms: over it.
    let mb = sequence_bridge_with(None, 64, |b| b).await;
    let server = server_for(&mb, &[]).await;
    let fits: Vec<Value> = [640, 640, 640, 640, 128]
        .iter()
        .map(|h| json!({"press": ["forward"], "holdTicks": h}))
        .collect();
    let out = call(&server, "input_sequence", json!({"steps": fits}))
        .await
        .unwrap();
    assert_eq!(out["steps"].as_array().unwrap().len(), 5);
    assert!(out["totals"]["ticks"].as_u64().unwrap() >= 2688);

    let over: Vec<Value> = [640, 640, 640, 640, 240]
        .iter()
        .map(|h| json!({"press": ["forward"], "holdTicks": h}))
        .collect();
    let before_sets = sent(&mb, "input.set").len();
    let err = call(&server, "input_sequence", json!({"steps": over}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "ToolBudgetExceeded");
    assert!(err.retry_safe);
    let max = err.data["maxTotalTicks"].as_u64().unwrap();
    assert!((2700..=2752).contains(&max), "maxTotalTicks {max}");
    // the refused sequence reached the bridge with no input at all
    assert_eq!(sent(&mb, "input.set").len(), before_sets);
}

// ---- fire_weapon, ent_fire, look_at, use_entity -----------------------------------------------

/// The methods the new action tools use (the look/use/fire mocks need all of them).
fn action_methods() -> Vec<&'static str> {
    METHODS
        .iter()
        .copied()
        .chain(["input.set", "input.clear", "game.entities"])
        .collect()
}

fn arr3(v: &Value) -> [f64; 3] {
    [
        v[0].as_f64().unwrap(),
        v[1].as_f64().unwrap(),
        v[2].as_f64().unwrap(),
    ]
}

/// game.local with an active weapon (clip1 `clip`, `entindex`), the player's pawn 7 and eye z 64.
fn gun_local(tick: u64, weapon: &str, entindex: i64, clip: i64) -> Value {
    json!({
        "tick": tick,
        "inGame": true,
        "map": "editor/zoo/smartprop_zoo",
        "playerSlot": 0,
        "viewAngles": [0.0, 0.0, 0.0],
        "player": {
            "alive": true,
            "pawnIndex": 7,
            "origin": [0.0, 0.0, 0.0],
            "eye": [0.0, 0.0, 64.0],
            "velocity": [0.0, 0.0, 0.0],
            "activeWeapon": {"name": weapon, "defIndex": 32, "entindex": entindex, "clip1": clip, "reserve": 4}
        }
    })
}

/// A shot takes clip1 down by one and puts `Weapon_hkp2000.Single` (the shooter pawn, 7) and
/// `Default.BulletImpact` in the sound ring. `immediate`: the shot lands at the tap, before the first
/// wait sample; otherwise two game.local reads after it. `sounds`: whether sound.drain is advertised.
async fn fire_bridge(clip0: i64, immediate: bool, sounds: bool) -> MockBridge {
    fn land_shot(clip: &AtomicI64, ring: &Mutex<Vec<Value>>, seq: &AtomicU64) {
        clip.fetch_sub(1, Ordering::SeqCst);
        let n = seq.fetch_add(2, Ordering::SeqCst);
        let mut ring = ring.lock().unwrap();
        ring.push(json!({"seq": n + 1, "name": "Weapon_hkp2000.Single", "entindex": 7, "via": 11, "origin": [0.0, 0.0, 0.0]}));
        ring.push(json!({"seq": n + 2, "name": "Default.BulletImpact", "entindex": 0, "via": 11, "origin": [10.0, 0.0, 0.0]}));
    }
    let clip = Arc::new(AtomicI64::new(clip0));
    let reads = Arc::new(AtomicU64::new(0));
    let shot_at = Arc::new(AtomicU64::new(u64::MAX));
    let ring = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seq = Arc::new(AtomicU64::new(0));
    let mut methods = METHODS.to_vec();
    methods.push("input.set");
    if !sounds {
        methods.retain(|m| !m.starts_with("sound."));
    }
    let (c1, r1, s1, reads1, shot1) = (
        clip.clone(),
        ring.clone(),
        seq.clone(),
        reads.clone(),
        shot_at.clone(),
    );
    let (c2, r2, s2, reads2, shot2) = (
        clip.clone(),
        ring.clone(),
        seq.clone(),
        reads.clone(),
        shot_at.clone(),
    );
    let mut builder = MockBridge::builder()
        .methods(methods)
        .responder("input.set", move |params| {
            if names_contain(&params["tap"], "attack") {
                if immediate {
                    land_shot(&c1, &r1, &s1);
                } else {
                    shot1.store(reads1.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
                }
            }
            json!({"applied": true})
        })
        .responder("game.local", move |_| {
            let n = reads2.fetch_add(1, Ordering::SeqCst) + 1;
            if shot2.load(Ordering::SeqCst) == n {
                shot2.store(u64::MAX, Ordering::SeqCst);
                land_shot(&c2, &r2, &s2);
            }
            gun_local(100 + n * 8, "weapon_hkp2000", 42, c2.load(Ordering::SeqCst))
        });
    if sounds {
        let ring3 = ring.clone();
        builder = builder.responder("sound.drain", move |_| {
            let events = std::mem::take(&mut *ring3.lock().unwrap());
            let total = events.len();
            json!({"events": events, "dropped": 0, "total": total, "pending": 0})
        });
    }
    builder.start().await.unwrap()
}

#[tokio::test]
async fn fire_weapon_reports_the_clip_drop_and_the_shot_sounds() {
    let mb = fire_bridge(13, false, true).await;
    let server = server_for(&mb, &[]).await;
    let out = call(&server, "fire_weapon", json!({})).await.unwrap();
    assert_eq!(out["clip1Before"], json!(13));
    assert_eq!(out["clip1After"], json!(12));
    assert_eq!(out["clip1Delta"], json!(-1));
    assert_eq!(out["shotsFired"], json!(1));
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["weapon"], json!("weapon_hkp2000"));
    assert_eq!(
        out["sounds"]["shot"][0]["name"],
        json!("Weapon_hkp2000.Single")
    );
    assert_eq!(out["sounds"]["shot"][0]["entindex"], json!(7));
    assert_eq!(
        out["sounds"]["impact"][0]["name"],
        json!("Default.BulletImpact")
    );
    assert!(out["ticks"]["end"].as_u64().unwrap() > out["ticks"]["start"].as_u64().unwrap());
    // per shot a release then a tap; the call ends with attack released
    assert_eq!(
        sent_bare(&mb, "input.set"),
        vec![
            json!({"release": ["attack"]}),
            json!({"tap": ["attack"], "frames": 2}),
            json!({"release": ["attack"]}),
        ]
    );
    assert_eq!(out["released"], json!(true));
    assert_eq!(out["taps"], json!(1));
    // the baseline drain and the post-shot drain
    assert_eq!(sent_bare(&mb, "sound.drain").len(), 2);
}

#[tokio::test]
async fn fire_weapon_shot_that_lands_before_the_first_sample_still_counts() {
    // every shot lands at its tap: the first wait sample already shows the drop
    let mb = fire_bridge(13, true, true).await;
    let server = server_for(&mb, &[]).await;
    let out = call(&server, "fire_weapon", json!({"shots": 2}))
        .await
        .unwrap();
    assert_eq!(out["clip1Before"], json!(13));
    assert_eq!(out["clip1After"], json!(11));
    assert_eq!(out["shotsFired"], json!(2));
    assert_eq!(out["ok"], json!(true));
    // 2 shots: a release and a tap each, then the closing release
    let taps = sent_bare(&mb, "input.set")
        .iter()
        .filter(|p| p.get("tap").is_some())
        .count();
    assert_eq!(taps, 2);
    assert_eq!(out["sounds"]["shot"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn fire_weapon_refuses_knife_empty_clip_and_wrong_weapon_before_sending() {
    let cases = [
        (gun_local(50, "weapon_knife", 40, -1), json!({}), "knife"),
        (gun_local(50, "weapon_hkp2000", 42, 0), json!({}), "empty"),
        (
            gun_local(50, "weapon_hkp2000", 42, 1),
            json!({"shots": 2}),
            "too few",
        ),
        (
            gun_local(50, "weapon_hkp2000", 42, 13),
            json!({"weapon": "weapon_glock"}),
            "mismatch",
        ),
    ];
    for (local, args, why) in cases {
        let mb = MockBridge::builder()
            .methods(action_methods())
            .responder_value("game.local", local)
            .responder_value("input.set", json!({"applied": true}))
            .responder_value(
                "sound.drain",
                json!({"events": [], "dropped": 0, "total": 0, "pending": 0}),
            )
            .start()
            .await
            .unwrap();
        let server = server_for(&mb, &[]).await;
        let err = call(&server, "fire_weapon", args).await.unwrap_err();
        assert_eq!(err.name, "PrerequisiteFailed", "{why}");
        assert!(err.retry_safe, "{why}");
        assert_eq!(err.data["inputSent"], json!(false), "{why}");
        assert!(sent_bare(&mb, "input.set").is_empty(), "{why}");
        assert!(sent_bare(&mb, "sound.drain").is_empty(), "{why}");
    }
}

#[tokio::test]
async fn fire_weapon_invalid_arguments_send_nothing() {
    let mb = fire_bridge(13, false, true).await;
    let server = server_for(&mb, &[]).await;
    for bad in [
        json!({"shots": 0}),
        json!({"shots": 31}),
        json!({"frames": 0}),
        json!({"frames": 65}),
        json!({"weapon": "hkp2000"}),
        json!({"timeoutMs": 0}),
        json!({"bogus": 1}),
    ] {
        let err = call(&server, "fire_weapon", bad.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    assert!(input_methods_of(&mb).is_empty());
    assert!(sent_bare(&mb, "game.local").is_empty());
}

fn input_methods_of(mb: &MockBridge) -> Vec<String> {
    mb.requests()
        .into_iter()
        .map(|r| r.method)
        .filter(|m| m.starts_with("input."))
        .collect()
}

#[tokio::test]
async fn fire_weapon_without_sound_methods_reports_null_sounds_and_a_note() {
    let mb = fire_bridge(13, false, false).await;
    let server = server_for(&mb, &[]).await;
    let out = call(&server, "fire_weapon", json!({})).await.unwrap();
    assert_eq!(out["clip1Delta"], json!(-1));
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["sounds"], Value::Null);
    assert!(out["note"].as_str().unwrap().contains("not advertised"));
}

/// A console.log-backed mock for ent_fire: `ent_fire` and the marker go through console.exec.
#[tokio::test]
async fn ent_fire_sends_the_command_then_confirms_with_the_marker() {
    let log = TempLog::new("entfire");
    std::fs::write(log.path(), "").unwrap();
    let mb = confirm_bridge(log.path(), true).await;
    let server = server_for(&mb, &["--console-log", log.path().to_str().unwrap()]).await;
    let out = call(
        &server,
        "ent_fire",
        json!({"target": "!player", "input": "sethealth", "value": "50"}),
    )
    .await
    .unwrap();
    let calls = sent_bare(&mb, "console.exec");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["command"], json!("ent_fire !player sethealth 50"));
    assert!(calls[1]["command"]
        .as_str()
        .unwrap()
        .starts_with("cs2mcp_mark_"));
    assert_eq!(out["confirmed"], json!(true));
    assert_eq!(out["commands"], json!(["ent_fire !player sethealth 50"]));
    assert_eq!(out["warnings"], json!([]));
    assert!(out["effect"].as_str().unwrap().contains("not verified"));
}

#[tokio::test]
async fn ent_fire_refuses_unsafe_text_and_sends_nothing() {
    let log = TempLog::new("entfire-bad");
    std::fs::write(log.path(), "").unwrap();
    let mb = confirm_bridge(log.path(), true).await;
    let server = server_for(&mb, &["--console-log", log.path().to_str().unwrap()]).await;
    for bad in [
        json!({"target": "t", "input": "Kill", "value": "1; quit"}),
        json!({"target": "t", "input": "Kill", "value": "a\"b"}),
        json!({"target": "a b", "input": "Kill"}),
        json!({"target": "t", "input": "Kill!"}),
        json!({"target": "t", "input": "Kill", "delay": 61}),
        json!({"target": "t"}),
    ] {
        let err = call(&server, "ent_fire", bad.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    assert!(sent_bare(&mb, "console.exec").is_empty());
}

/// The view follows the input.set angle override (a null override leaves it where it was). The
/// bridge's game.entities has entindex 370 at `button`. When `responds`, a use tap plays a
/// Buttons.snd9 sound from 370 and turns the button (its angles change).
async fn aim_bridge(button: [f64; 3], responds: bool) -> MockBridge {
    let view = Arc::new(Mutex::new([0.0_f64; 3]));
    let tick = Arc::new(AtomicU64::new(200));
    let used = Arc::new(AtomicBool::new(false));
    let ring = Arc::new(Mutex::new(Vec::<Value>::new()));
    let (v_set, used_set, ring_set) = (view.clone(), used.clone(), ring.clone());
    let (v_local, used_ents) = (view.clone(), used.clone());
    let builder = MockBridge::builder()
        .methods(action_methods())
        .responder("input.set", move |params| {
            if params["angles"].is_array() {
                *v_set.lock().unwrap() = arr3(&params["angles"]);
            }
            if names_contain(&params["tap"], "use") && responds {
                used_set.store(true, Ordering::SeqCst);
                ring_set.lock().unwrap().push(json!({"seq": 1, "name": "Buttons.snd9", "entindex": 370, "via": 11, "origin": button}));
            }
            json!({"applied": true})
        })
        .responder("game.local", move |_| {
            let n = tick.fetch_add(8, Ordering::SeqCst) + 8;
            let v = *v_local.lock().unwrap();
            let mut local = gun_local(n, "weapon_hkp2000", 42, 13);
            local["viewAngles"] = json!(v);
            local
        })
        .responder("game.entities", move |params| {
            // honours the bridge's entindex filter: only slot 370 holds the button
            let angles = if used_ents.load(Ordering::SeqCst) { [0.0, 90.0, 0.0] } else { [0.0, 0.0, 0.0] };
            let slot = params["entindex"].as_i64();
            let list = if slot.is_none() || slot == Some(370) {
                json!([{"entindex": 370, "class": "func_button", "name": "mdlchange.button.scaleup", "origin": button, "angles": angles, "health": 0, "toggleState": null}])
            } else {
                json!([])
            };
            let count = list.as_array().map_or(0, Vec::len);
            json!({"entities": list, "count": count, "truncated": false})
        })
        .responder("sound.drain", move |_| {
            let events = std::mem::take(&mut *ring.lock().unwrap());
            let total = events.len();
            json!({"events": events, "dropped": 0, "total": total, "pending": 0})
        });
    builder.start().await.unwrap()
}

#[tokio::test]
async fn look_at_sends_the_aim_then_clears_and_reads_the_view_back() {
    let mb = aim_bridge([100.0, 100.0, 64.0], true).await;
    let server = server_for(&mb, &[]).await;
    let out = call(&server, "look_at", json!({"target": 370}))
        .await
        .unwrap();
    assert_eq!(out["angles"], json!([0.0, 45.0, 0.0]));
    assert_eq!(out["achieved"], json!([0.0, 45.0, 0.0]));
    assert_eq!(out["held"], json!(false));
    assert_eq!(out["inputCleared"], json!(true));
    // the mock keeps the view after a null override: no snap back
    assert_eq!(out["viewAfterClear"], json!([0.0, 45.0, 0.0]));
    assert_eq!(
        sent_bare(&mb, "input.set"),
        vec![json!({"angles": [0.0, 45.0, 0.0]}), json!({"angles": null})]
    );
    // the entity was looked up by index
    assert_eq!(sent_bare(&mb, "game.entities")[0]["entindex"], json!(370));
}

#[tokio::test]
async fn look_at_hold_keeps_the_override_and_a_point_aims_too() {
    let mb = aim_bridge([100.0, 100.0, 64.0], true).await;
    let server = server_for(&mb, &[]).await;
    let out = call(
        &server,
        "look_at",
        json!({"target": [100.0, 0.0, 64.0], "hold": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["angles"], json!([0.0, 0.0, 0.0]));
    assert_eq!(out["held"], json!(true));
    assert_eq!(out["inputCleared"], json!(false));
    assert_eq!(sent_bare(&mb, "input.set").len(), 1);
    assert!(sent_bare(&mb, "game.entities").is_empty());
}

#[tokio::test]
async fn look_at_view_that_never_settles_is_undone_and_not_retry_safe() {
    // the mock ignores angles entirely: the view stays at zero
    let mb = MockBridge::builder()
        .methods(action_methods())
        .responder_value("game.local", gun_local(100, "weapon_hkp2000", 42, 13))
        .responder_value("input.set", json!({"applied": true}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, &[]).await;
    let err = call(
        &server,
        "look_at",
        json!({"target": [100.0, 100.0, 64.0], "timeoutMs": 200}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(!err.retry_safe);
    assert_eq!(err.data["inputSent"], json!(true));
    assert_eq!(err.data["inputCleared"], json!(true));
    let calls = sent_bare(&mb, "input.set");
    assert_eq!(calls.first(), Some(&json!({"angles": [0.0, 45.0, 0.0]})));
    assert_eq!(calls.last(), Some(&json!({"angles": null})));
}

#[tokio::test]
async fn look_at_invalid_targets_send_nothing() {
    let mb = aim_bridge([100.0, 100.0, 64.0], true).await;
    let server = server_for(&mb, &[]).await;
    for bad in [
        json!({"target": 0}),
        // One past the last walkable slot: MAX_EDICTS = ENTINDEX_MAX + 1.
        json!({"target": s2sdk_sys::generated::MAX_EDICTS}),
        json!({"target": [1.0, 2.0]}),
        json!({"target": [1.0, 2.0, 99999.0]}),
        json!({"target": 370, "eye": "yes"}),
        json!({"target": 370, "extra": true}),
        json!({}),
    ] {
        let err = call(&server, "look_at", bad.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    assert!(input_methods_of(&mb).is_empty());
}

#[tokio::test]
async fn use_entity_refuses_a_target_beyond_max_distance_before_sending() {
    // the button is 500 units from the eye (z 64): far beyond the default 64
    let mb = aim_bridge([500.0, 0.0, 64.0], true).await;
    let server = server_for(&mb, &[]).await;
    let err = call(&server, "use_entity", json!({"target": 370}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert!(err.retry_safe);
    assert_eq!(err.data["inputSent"], json!(false));
    assert!(input_methods_of(&mb).is_empty());
    assert!(sent_bare(&mb, "sound.drain").is_empty());
}

#[tokio::test]
async fn use_entity_reports_sound_and_entity_evidence() {
    let mb = aim_bridge([40.0, 0.0, 64.0], true).await;
    let server = server_for(&mb, &[]).await;
    let out = call(
        &server,
        "use_entity",
        json!({"target": 370, "maxDistance": 64}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["changed"], json!(true));
    let evidence = out["evidence"].as_array().unwrap();
    assert!(evidence.iter().any(|e| e["kind"] == json!("sound")
        && e["name"] == json!("Buttons.snd9")
        && e["entindex"] == json!(370)));
    assert!(evidence.iter().any(|e| e["kind"] == json!("entity")));
    // aim (held), use tap, then the clear
    let calls = sent_bare(&mb, "input.set");
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0], json!({"angles": [0.0, 0.0, 0.0]}));
    assert_eq!(calls[1], json!({"tap": ["use"], "frames": 4}));
    assert_eq!(calls[2], json!({"angles": null}));
}

#[tokio::test]
async fn use_entity_without_evidence_does_not_claim_success() {
    // the tap does nothing: no sound from the button, no change to it
    let mb = aim_bridge([40.0, 0.0, 64.0], false).await;
    let server = server_for(&mb, &[]).await;
    let out = call(&server, "use_entity", json!({"target": 370}))
        .await
        .unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["changed"], json!(false));
    assert_eq!(out["evidence"], json!([]));
    // the aim is still cleared
    assert_eq!(
        sent_bare(&mb, "input.set").last(),
        Some(&json!({"angles": null}))
    );
}

#[tokio::test]
async fn look_at_an_empty_slot_is_entity_not_found_and_sends_no_input() {
    // slot 371 holds nothing: the entindex filter returns an empty list
    let mb = aim_bridge([100.0, 100.0, 64.0], true).await;
    let server = server_for(&mb, &[]).await;
    let err = call(&server, "look_at", json!({"target": 371}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "EntityNotFound");
    assert!(err.retry_safe);
    assert_eq!(err.data["inputSent"], json!(false));
    assert!(input_methods_of(&mb).is_empty());
    assert_eq!(sent(&mb, "game.entities")[0]["entindex"], json!(371));
}

// ---- review fixes: semi-auto retaps, undo on error, snap-back, removal, sound radius ----------

fn tick_of(reads: u64) -> u64 {
    100 + reads * 8
}

/// A semi-auto: a shot needs a fresh press (no press still held from an earlier tap) and `cooldown`
/// ticks since the last shot. A tap holds the press for two game.local reads; a release ends it.
async fn semi_bridge(clip0: i64, cooldown: u64) -> MockBridge {
    let clip = Arc::new(AtomicI64::new(clip0));
    let reads = Arc::new(AtomicU64::new(0));
    let held = Arc::new(AtomicBool::new(false));
    let release_at = Arc::new(AtomicU64::new(0));
    let last_shot = Arc::new(AtomicU64::new(u64::MAX));
    let ring = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seq = Arc::new(AtomicU64::new(0));
    let (c1, h1, ra1, ls1, rd1, r1, s1) = (
        clip.clone(),
        held.clone(),
        release_at.clone(),
        last_shot.clone(),
        reads.clone(),
        ring.clone(),
        seq.clone(),
    );
    let (c2, h2, ra2, rd2) = (
        clip.clone(),
        held.clone(),
        release_at.clone(),
        reads.clone(),
    );
    let ring3 = ring.clone();
    MockBridge::builder()
        .methods(action_methods())
        .responder("input.set", move |params| {
            if names_contain(&params["release"], "attack") {
                h1.store(false, Ordering::SeqCst);
            }
            if names_contain(&params["tap"], "attack") {
                // a press during an active one is not a fresh edge: ignored
                if !h1.swap(true, Ordering::SeqCst) {
                    let now = tick_of(rd1.load(Ordering::SeqCst));
                    let last = ls1.load(Ordering::SeqCst);
                    if (last == u64::MAX || now >= last + cooldown)
                        && c1.load(Ordering::SeqCst) > 0
                    {
                        c1.fetch_sub(1, Ordering::SeqCst);
                        let n = s1.fetch_add(2, Ordering::SeqCst);
                        let mut ring = r1.lock().unwrap();
                        ring.push(json!({"seq": n + 1, "name": "Weapon_hkp2000.Single", "entindex": 7, "via": 11, "origin": [0.0, 0.0, 0.0]}));
                        ring.push(json!({"seq": n + 2, "name": "Default.BulletImpact", "entindex": 0, "via": 11, "origin": [10.0, 0.0, 0.0]}));
                        ls1.store(now, Ordering::SeqCst);
                    }
                    ra1.store(rd1.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
                }
            }
            json!({"applied": true})
        })
        .responder("game.local", move |_| {
            let n = rd2.fetch_add(1, Ordering::SeqCst) + 1;
            if h2.load(Ordering::SeqCst) && n >= ra2.load(Ordering::SeqCst) {
                h2.store(false, Ordering::SeqCst);
            }
            gun_local(tick_of(n), "weapon_hkp2000", 42, c2.load(Ordering::SeqCst))
        })
        .responder("sound.drain", move |_| {
            let events = std::mem::take(&mut *ring3.lock().unwrap());
            let total = events.len();
            json!({"events": events, "dropped": 0, "total": total, "pending": 0})
        })
        .start()
        .await
        .unwrap()
}

#[tokio::test]
async fn fire_weapon_retaps_until_a_semi_auto_fires_on_a_fresh_press() {
    // the second and third taps arrive inside the 16-tick cooldown or during a held press: only
    // the re-taps (after a release) fire
    let mb = semi_bridge(13, 16).await;
    let server = server_for(&mb, &[]).await;
    let out = call(&server, "fire_weapon", json!({"shots": 3}))
        .await
        .unwrap();
    assert_eq!(out["clip1Before"], json!(13));
    assert_eq!(out["clip1After"], json!(10));
    assert_eq!(out["shotsFired"], json!(3));
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["sounds"]["shot"].as_array().unwrap().len(), 3);
    // more taps than shots: some were ignored by the semi-auto and retried
    assert!(out["taps"].as_u64().unwrap() > 3, "{out}");
    // every tap came after a release (a fresh press edge each time)
    let calls = sent_bare(&mb, "input.set");
    for pair in calls.windows(2) {
        if pair[1].get("tap").is_some() {
            assert_eq!(pair[0], json!({"release": ["attack"]}));
        }
    }
    assert_eq!(calls.last(), Some(&json!({"release": ["attack"]})));
}

#[tokio::test]
async fn fire_weapon_error_after_send_releases_attack() {
    // the clip never drops: every attempt misses until the shot's deadline
    let mb = MockBridge::builder()
        .methods(action_methods())
        .responder_value("game.local", gun_local(100, "weapon_hkp2000", 42, 13))
        .responder_value("input.set", json!({"applied": true}))
        .responder_value(
            "sound.drain",
            json!({"events": [], "dropped": 0, "total": 0, "pending": 0}),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, &[]).await;
    let err = call(&server, "fire_weapon", json!({"timeoutMs": 400}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(!err.retry_safe);
    assert_eq!(err.data["inputSent"], json!(true));
    assert_eq!(err.data["inputCleared"], json!(true));
    assert_eq!(
        sent_bare(&mb, "input.set").last(),
        Some(&json!({"release": ["attack"]}))
    );
}

/// A use probe: the view follows input.set angles; a use tap plays `event` (when given) into the sound
/// ring and, when `removed`, empties game.entities (the button is gone); otherwise entindex 370 sits at
/// (40, 0, 64).
async fn use_probe_bridge(event: Option<Value>, removed: bool) -> MockBridge {
    let view = Arc::new(Mutex::new([0.0_f64; 3]));
    let tick = Arc::new(AtomicU64::new(200));
    let used = Arc::new(AtomicBool::new(false));
    let ring = Arc::new(Mutex::new(Vec::<Value>::new()));
    let (v1, u1, r1) = (view.clone(), used.clone(), ring.clone());
    let (v2, t2) = (view.clone(), tick.clone());
    let (u3,) = (used.clone(),);
    let r3 = ring.clone();
    MockBridge::builder()
        .methods(action_methods())
        .responder("input.set", move |params| {
            if params["angles"].is_array() {
                *v1.lock().unwrap() = arr3(&params["angles"]);
            }
            if names_contain(&params["tap"], "use") {
                u1.store(true, Ordering::SeqCst);
                if let Some(e) = &event {
                    r1.lock().unwrap().push(e.clone());
                }
            }
            json!({"applied": true})
        })
        .responder("game.local", move |_| {
            let n = t2.fetch_add(8, Ordering::SeqCst) + 8;
            let mut local = gun_local(n, "weapon_hkp2000", 42, 13);
            local["viewAngles"] = json!(*v2.lock().unwrap());
            local
        })
        .responder("game.entities", move |_| {
            let gone = removed && u3.load(Ordering::SeqCst);
            let list = if gone {
                json!([])
            } else {
                json!([{"entindex": 370, "class": "func_button", "name": null, "origin": [40.0, 0.0, 64.0], "angles": [0.0, 0.0, 0.0], "health": 0, "toggleState": null}])
            };
            let count = list.as_array().map_or(0, Vec::len);
            json!({"entities": list, "count": count, "truncated": false})
        })
        .responder("sound.drain", move |_| {
            let events = std::mem::take(&mut *r3.lock().unwrap());
            let total = events.len();
            json!({"events": events, "dropped": 0, "total": total, "pending": 0})
        })
        .start()
        .await
        .unwrap()
}

#[tokio::test]
async fn use_entity_error_after_use_releases_use_and_clears_the_aim() {
    // the player vanishes from game.local right after the use tap: the effect window read fails
    let used = Arc::new(AtomicBool::new(false));
    let (u1, u2) = (used.clone(), used.clone());
    let mb = MockBridge::builder()
        .methods(action_methods())
        .responder("input.set", move |params| {
            if names_contain(&params["tap"], "use") {
                u1.store(true, Ordering::SeqCst);
            }
            json!({"applied": true})
        })
        .responder("game.local", move |_| {
            if u2.load(Ordering::SeqCst) {
                json!({"tick": 300, "inGame": false, "map": "", "player": null})
            } else {
                gun_local(200, "weapon_hkp2000", 42, 13)
            }
        })
        .responder_value(
            "game.entities",
            json!({"entities": [{"entindex": 370, "class": "func_button", "name": null, "origin": [40.0, 0.0, 64.0], "angles": [0.0, 0.0, 0.0]}], "count": 1, "truncated": false}),
        )
        .responder_value(
            "sound.drain",
            json!({"events": [], "dropped": 0, "total": 0, "pending": 0}),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, &[]).await;
    let err = call(&server, "use_entity", json!({"target": 370}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert!(!err.retry_safe);
    assert_eq!(err.data["inputSent"], json!(true));
    assert_eq!(err.data["inputCleared"], json!(true));
    let calls = sent_bare(&mb, "input.set");
    assert_eq!(calls[calls.len() - 2], json!({"release": ["use"]}));
    assert_eq!(calls[calls.len() - 1], json!({"angles": null}));
}

#[tokio::test]
async fn look_at_reports_a_view_that_snapped_back_after_the_clear() {
    let view = Arc::new(Mutex::new([0.0_f64; 3]));
    let tick = Arc::new(AtomicU64::new(100));
    let (v1, v2, t2) = (view.clone(), view.clone(), tick.clone());
    let mb = MockBridge::builder()
        .methods(action_methods())
        .responder("input.set", move |params| {
            if params["angles"].is_array() {
                *v1.lock().unwrap() = arr3(&params["angles"]);
            } else if params["angles"].is_null() {
                // the engine snaps the view back when the override goes
                *v1.lock().unwrap() = [0.0, 0.0, 0.0];
            }
            json!({"applied": true})
        })
        .responder("game.local", move |_| {
            let n = t2.fetch_add(8, Ordering::SeqCst) + 8;
            let mut local = gun_local(n, "weapon_hkp2000", 42, 13);
            local["viewAngles"] = json!(*v2.lock().unwrap());
            local
        })
        .responder_value(
            "game.entities",
            json!({"entities": [{"entindex": 370, "class": "func_button", "name": null, "origin": [100.0, 100.0, 64.0], "angles": [0.0, 0.0, 0.0]}], "count": 1, "truncated": false}),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, &[]).await;
    let out = call(&server, "look_at", json!({"target": 370}))
        .await
        .unwrap();
    assert_eq!(out["achieved"], json!([0.0, 45.0, 0.0]));
    assert_eq!(out["viewAfterClear"], json!([0.0, 0.0, 0.0]));
    assert_eq!(out["snappedBack"], json!(true));
    assert!(out["note"].as_str().unwrap().contains("snapped back"));
}

#[tokio::test]
async fn use_entity_target_removed_by_the_use_is_evidence() {
    let mb = use_probe_bridge(None, true).await;
    let server = server_for(&mb, &[]).await;
    let out = call(&server, "use_entity", json!({"target": 370}))
        .await
        .unwrap();
    assert_eq!(out["removed"], json!(true));
    assert_eq!(out["changed"], json!(true));
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["entityAfter"], Value::Null);
    let evidence = out["evidence"].as_array().unwrap();
    assert!(evidence
        .iter()
        .any(|e| e["kind"] == json!("entity") && e["removed"] == json!(true)));
}

#[tokio::test]
async fn use_entity_button_sound_far_from_the_target_is_not_evidence() {
    // a Button sound 500 units away from the target, from another entity: not the use
    let far = json!({"seq": 1, "name": "Buttons.snd9", "entindex": 9, "via": 11, "origin": [500.0, 0.0, 64.0]});
    let mb = use_probe_bridge(Some(far), false).await;
    let server = server_for(&mb, &[]).await;
    let out = call(&server, "use_entity", json!({"target": 370}))
        .await
        .unwrap();
    assert_eq!(out["ok"], json!(false));
    assert!(out["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["kind"] != json!("sound")));
    // the sound was seen, just not counted
    assert_eq!(out["soundsInWindow"], json!(["Buttons.snd9"]));
}

#[tokio::test]
async fn use_entity_button_sound_near_the_target_is_evidence() {
    let near = json!({"seq": 1, "name": "Buttons.snd9", "entindex": 9, "via": 11, "origin": [60.0, 0.0, 64.0]});
    let mb = use_probe_bridge(Some(near), false).await;
    let server = server_for(&mb, &[]).await;
    let out = call(&server, "use_entity", json!({"target": 370}))
        .await
        .unwrap();
    assert_eq!(out["ok"], json!(true));
    assert!(out["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == json!("sound") && e["name"] == json!("Buttons.snd9")));
}

#[tokio::test]
async fn ent_fire_refuses_star_and_destructive_wildcards_before_sending() {
    let log = TempLog::new("entfire-refuse");
    std::fs::write(log.path(), "").unwrap();
    let mb = confirm_bridge(log.path(), true).await;
    let server = server_for(&mb, &["--console-log", log.path().to_str().unwrap()]).await;
    for bad in [
        json!({"target": "*", "input": "Open"}),
        json!({"target": "prop_dynamic", "input": "Kill"}),
        json!({"target": "prop*", "input": "KillHierarchy"}),
    ] {
        let err = call(&server, "ent_fire", bad.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    assert!(sent(&mb, "console.exec").is_empty());
}
