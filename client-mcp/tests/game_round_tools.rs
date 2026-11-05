//! `round_start` against the mock bridge. ensure: a live round sends no command; warmup is ended
//! with mp_warmup_end. restart: the cvars and mp_restartgame 1 go out, and the roundRestarted wait
//! matches against the rules read before anything was sent, even when the restart already landed.
//! Invalid arguments, a player who is not in game and a missing console.log send nothing. A timeout
//! after a command is sent is not retry-safe. The confirmation marker comes from a temp console.log
//! that the mock appends to, as in game_actions_tools.rs.
#![allow(clippy::result_large_err)]
use client_mcp::state::ClientConfig;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const METHODS: [&str; 10] = [
    "control.ping",
    "control.status",
    "control.capabilities",
    "control.version",
    "doc.active",
    "game.local",
    "game.rules",
    "game.weapons",
    "console.exec",
    "input.clear",
];

struct TempLog(PathBuf);

impl TempLog {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        TempLog(std::env::temp_dir().join(format!(
            "cs2mcp-round-{tag}-{}-{nanos}.log",
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

async fn server_for(mb: &MockBridge, log: &Path) -> Server {
    let args = [
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--deadline-ms",
        "7000",
        "--console-log",
        log.to_str().unwrap(),
    ];
    client_mcp::build_server(&config(&args)).unwrap()
}

async fn server_for_budget(mb: &MockBridge, log: &Path, budget_ms: &str) -> Server {
    let args = [
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--deadline-ms",
        "7000",
        "--tool-budget-ms",
        budget_ms,
        "--console-log",
        log.to_str().unwrap(),
    ];
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

/// Flags the console.exec mock sets, read by the game.rules mock.
#[derive(Default)]
struct Flags {
    warm_end: AtomicBool,
    restart: AtomicBool,
    reads_after_restart: AtomicU64,
}

/// The console.exec reply: commands set the flags the rules mock reads; a mistyped command and the
/// marker get an `Unknown command` line in the log, as the engine writes them.
fn exec_reply(log: &Path, flags: &Flags, params: &Value) -> Value {
    let command = params["command"].as_str().unwrap_or_default();
    match command {
        "mp_warmup_end" => flags.warm_end.store(true, Ordering::SeqCst),
        "mp_restartgame 1" => flags.restart.store(true, Ordering::SeqCst),
        _ => {}
    }
    if command.starts_with("bogus_") || command.starts_with("cs2mcp_mark_") {
        let mut f = std::fs::OpenOptions::new().append(true).open(log).unwrap();
        writeln!(f, "Unknown command: {command}").unwrap();
    }
    json!({"queued": true})
}

/// A local player alive (or dead) in game on `tick`, with an active weapon.
fn local(tick: u64, alive: bool) -> Value {
    json!({
        "tick": tick,
        "inGame": true,
        "map": "editor/zoo/smartprop_zoo",
        "playerSlot": 0,
        "viewAngles": [0.0, 0.0, 0.0],
        "player": {
            "alive": alive,
            "health": 100,
            "origin": [-6176.0, -2192.0, 0.031],
            "velocity": [0.0, 0.0, 0.0],
            "activeWeapon": {"name": "weapon_hkp2000", "entindex": 32, "defIndex": 1, "clip1": 13}
        }
    })
}

/// A local player on a tick that advances on every read (`counter`), alive.
fn ticking(counter: &Arc<AtomicU64>) -> Value {
    local(100 + counter.fetch_add(8, Ordering::SeqCst), true)
}

fn weapons() -> Value {
    json!({
        "tick": 100,
        "inGame": true,
        "map": "editor/zoo/smartprop_zoo",
        "active": {"weapon": {"name": "weapon_hkp2000", "entindex": 32}},
        "count": 2,
        "weapons": [{"name": "weapon_knife", "entindex": 42}, {"name": "weapon_hkp2000", "entindex": 32}]
    })
}

/// `game.rules` for a live round (no warmup/freeze/intro/restart, no winner) that started at `start`.
fn rules_live(start: f64, played: i64) -> Value {
    json!({"tick": 9, "inGame": true, "map": "editor/zoo/smartprop_zoo", "proxyIndex": 70,
        "warmup": false, "freeze": false, "teamIntro": false, "gameRestart": false,
        "hasMatchStarted": true, "gamePhase": 2, "roundsPlayed": played, "roundStartTime": start,
        "restartRoundTime": 0.0, "roundTimeSec": 3600, "roundWinStatus": 0, "roundEndReason": 0,
        "bombPlanted": false, "schemaMissing": []})
}

fn rules_warmup(start: f64) -> Value {
    let mut v = rules_live(start, 0);
    v["warmup"] = json!(true);
    v
}

/// The base builder: game.weapons, input.clear and the console.exec mock wired to `flags` and `log`.
fn base(log: &Path, flags: &Arc<Flags>) -> cs2_mcp_common::mock::MockBuilder {
    let (f, lp) = (flags.clone(), log.to_path_buf());
    MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.weapons", weapons())
        .responder_value("input.clear", json!({"cleared": true}))
        .responder("console.exec", move |params| exec_reply(&lp, &f, params))
}

#[tokio::test]
async fn ensure_on_a_live_round_sends_no_command() {
    let log = TempLog::new("live");
    std::fs::write(log.path(), "").unwrap();
    let ticks = Arc::new(AtomicU64::new(0));
    let t = ticks.clone();
    let flags = Arc::new(Flags::default());
    let mb = base(log.path(), &flags)
        .responder("game.local", move |_| ticking(&t))
        .responder_value("game.rules", rules_live(12.5, 0))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, log.path()).await;
    let out = call(&server, "round_start", json!({})).await.unwrap();
    assert_eq!(out["mode"], json!("ensure"));
    assert_eq!(out["changed"], json!(false));
    assert_eq!(out["commands"], json!([]));
    assert_eq!(out["warnings"], json!([]));
    assert_eq!(out["before"]["rules"]["roundStartTime"], json!(12.5));
    assert_eq!(out["after"]["rules"], out["before"]["rules"]);
    assert_eq!(out["after"]["player"]["alive"], json!(true));
    assert_eq!(out["after"]["player"]["health"], json!(100));
    assert_eq!(out["after"]["player"]["weapons"]["count"], json!(2));
    assert_eq!(
        out["after"]["player"]["weapons"]["names"],
        json!(["weapon_knife", "weapon_hkp2000"])
    );
    assert_eq!(out["ticks"], json!(0));
    assert!(sent(&mb, "console.exec").is_empty());
    // the input hook is released first, even when nothing else happens
    assert_eq!(sent(&mb, "input.clear").len(), 1);
}

#[tokio::test]
async fn ensure_during_warmup_sets_the_cvars_ends_warmup_and_waits_for_the_round() {
    let log = TempLog::new("warmup");
    std::fs::write(log.path(), "").unwrap();
    let flags = Arc::new(Flags::default());
    let ticks = Arc::new(AtomicU64::new(0));
    let (f, t) = (flags.clone(), ticks.clone());
    let mb = base(log.path(), &flags)
        .responder("game.local", move |_| ticking(&t))
        .responder("game.rules", move |_| {
            if f.warm_end.load(Ordering::SeqCst) {
                // the warmup end starts the round: the start time moves
                rules_live(14.0, 1)
            } else {
                rules_warmup(12.5)
            }
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, log.path()).await;
    let out = call(
        &server,
        "round_start",
        json!({"freezetime": 0, "roundtime": 5, "respawn": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["changed"], json!(true));
    assert_eq!(
        out["commands"],
        json!([
            "mp_freezetime 0",
            "mp_roundtime 5",
            "mp_respawn_on_death_ct 1",
            "mp_respawn_on_death_t 1",
            "mp_warmup_end"
        ])
    );
    assert_eq!(out["before"]["rules"]["warmup"], json!(true));
    assert_eq!(out["after"]["rules"]["warmup"], json!(false));
    assert_eq!(out["after"]["rules"]["roundStartTime"], json!(14.0));
    assert_eq!(out["fallback"], json!(null));
    // the commands in order, then the marker that proves they ran
    let calls = sent(&mb, "console.exec");
    assert_eq!(calls.len(), 6);
    assert_eq!(calls[4]["command"], json!("mp_warmup_end"));
    assert!(calls[5]["command"]
        .as_str()
        .unwrap()
        .starts_with("cs2mcp_mark_"));
}

#[tokio::test]
async fn restart_sends_mp_restartgame_and_matches_when_the_round_start_grows() {
    let log = TempLog::new("restart");
    std::fs::write(log.path(), "").unwrap();
    let flags = Arc::new(Flags::default());
    let ticks = Arc::new(AtomicU64::new(0));
    let (f, t) = (flags.clone(), ticks.clone());
    let mb = base(log.path(), &flags)
        .responder("game.local", move |_| ticking(&t))
        .responder("game.rules", move |_| {
            if !f.restart.load(Ordering::SeqCst) {
                return rules_live(12.5, 0);
            }
            // the old round for two more reads, then the new one
            if f.reads_after_restart.fetch_add(1, Ordering::SeqCst) < 2 {
                rules_live(12.5, 0)
            } else {
                rules_live(14.0, 1)
            }
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, log.path()).await;
    let out = call(
        &server,
        "round_start",
        json!({"mode": "restart", "cvars": {"bot_quota": 0, "bogus_typo": 1}}),
    )
    .await
    .unwrap();
    assert_eq!(out["mode"], json!("restart"));
    assert_eq!(out["changed"], json!(true));
    assert_eq!(
        out["commands"],
        // cvars go out in name order (the JSON object's order)
        json!(["bogus_typo 1", "bot_quota 0", "mp_restartgame 1"])
    );
    // the engine's refusal of the mistyped cvar, not the marker's own line
    assert_eq!(out["warnings"], json!(["Unknown command: bogus_typo 1"]));
    assert_eq!(out["before"]["rules"]["roundStartTime"], json!(12.5));
    assert_eq!(out["after"]["rules"]["roundStartTime"], json!(14.0));
    assert_eq!(out["after"]["rules"]["roundsPlayed"], json!(1));
    assert!(out["ticks"].as_u64().unwrap() > 0);
    assert_eq!(
        out["after"]["player"]["weapons"]["active"],
        json!("weapon_hkp2000")
    );
    let calls = sent(&mb, "console.exec");
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[2]["command"], json!("mp_restartgame 1"));
    assert!(calls[3]["command"]
        .as_str()
        .unwrap()
        .starts_with("cs2mcp_mark_"));
}

#[tokio::test]
async fn restart_that_landed_before_the_first_sample_still_matches() {
    // the mock flips the round the moment the restart command is queued: the first wait sample is
    // already past it, so only the baseline read before the send can see the change
    let log = TempLog::new("restart-early");
    std::fs::write(log.path(), "").unwrap();
    let flags = Arc::new(Flags::default());
    let ticks = Arc::new(AtomicU64::new(0));
    let (f, t) = (flags.clone(), ticks.clone());
    let mb = base(log.path(), &flags)
        .responder("game.local", move |_| ticking(&t))
        .responder("game.rules", move |_| {
            if f.restart.load(Ordering::SeqCst) {
                rules_live(14.0, 1)
            } else {
                rules_live(12.5, 0)
            }
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, log.path()).await;
    let out = call(&server, "round_start", json!({"mode": "restart"}))
        .await
        .unwrap();
    assert_eq!(out["samples"], json!(1));
    assert_eq!(out["before"]["rules"]["roundStartTime"], json!(12.5));
    assert_eq!(out["after"]["rules"]["roundStartTime"], json!(14.0));
    // one read before the send (the baseline) and one wait sample
    assert_eq!(sent(&mb, "game.rules").len(), 2);
}

#[tokio::test]
async fn invalid_arguments_send_nothing() {
    let log = TempLog::new("invalid");
    std::fs::write(log.path(), "").unwrap();
    let flags = Arc::new(Flags::default());
    let mb = base(log.path(), &flags)
        .responder_value("game.local", local(100, true))
        .responder_value("game.rules", rules_live(12.5, 0))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, log.path()).await;
    for bad in [
        json!({"mode": "reset"}),
        json!({"cvars": {"Bot_Quota": 1}}),
        json!({"cvars": {"bot quota": 1}}),
        json!({"cvars": {"bot_quota": "1; quit"}}),
        json!({"cvars": {"bot_quota": true}}),
        json!({"timeoutMs": 0}),
        json!({"roundtime": 0}),
        json!({"surprise": 1}),
    ] {
        let err = call(&server, "round_start", bad.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    assert!(sent(&mb, "game.local").is_empty());
    assert!(sent(&mb, "game.rules").is_empty());
    assert!(sent(&mb, "input.clear").is_empty());
    assert!(sent(&mb, "console.exec").is_empty());
}

#[tokio::test]
async fn not_in_game_is_prerequisite_failed_and_nothing_is_sent() {
    let log = TempLog::new("not-in-game");
    std::fs::write(log.path(), "").unwrap();
    let flags = Arc::new(Flags::default());
    let mb = base(log.path(), &flags)
        .responder_value(
            "game.local",
            json!({"tick": 1, "inGame": false, "map": "", "player": null}),
        )
        .responder_value("game.rules", rules_live(12.5, 0))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, log.path()).await;
    let err = call(&server, "round_start", json!({"mode": "restart"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert!(err.retry_safe);
    assert!(sent(&mb, "input.clear").is_empty());
    assert!(sent(&mb, "console.exec").is_empty());
}

#[tokio::test]
async fn restart_without_a_console_log_sends_nothing() {
    let log = TempLog::new("no-log"); // never created
    let flags = Arc::new(Flags::default());
    let mb = base(log.path(), &flags)
        .responder_value("game.local", local(100, true))
        .responder_value("game.rules", rules_live(12.5, 0))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, log.path()).await;
    let err = call(&server, "round_start", json!({"mode": "restart"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "ConsoleLogMissing");
    assert!(err.retry_safe);
    assert!(sent(&mb, "console.exec").is_empty());
}

#[tokio::test]
async fn restart_that_never_lands_is_a_non_retry_safe_timeout() {
    let log = TempLog::new("restart-timeout");
    std::fs::write(log.path(), "").unwrap();
    let flags = Arc::new(Flags::default());
    let ticks = Arc::new(AtomicU64::new(0));
    let t = ticks.clone();
    let mb = base(log.path(), &flags)
        .responder("game.local", move |_| ticking(&t))
        .responder_value("game.rules", rules_live(12.5, 0))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, log.path()).await;
    let err = call(
        &server,
        "round_start",
        json!({"mode": "restart", "timeoutMs": 300}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(!err.retry_safe);
    assert_eq!(err.data["commandsSent"], json!(true));
    assert_eq!(err.data["commands"], json!(["mp_restartgame 1"]));
    assert_eq!(err.data["reason"], json!("timeout"));
    assert_eq!(sent(&mb, "console.exec").len(), 2);
}

#[tokio::test]
async fn ensure_that_cannot_go_live_without_a_command_is_retry_safe() {
    // frozen, not warming up: nothing to queue, so the wait times out and nothing was sent
    let log = TempLog::new("frozen");
    std::fs::write(log.path(), "").unwrap();
    let flags = Arc::new(Flags::default());
    let mut frozen = rules_live(12.5, 0);
    frozen["freeze"] = json!(true);
    let mb = base(log.path(), &flags)
        .responder_value("game.local", local(100, true))
        .responder_value("game.rules", frozen)
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, log.path()).await;
    let err = call(&server, "round_start", json!({"timeoutMs": 200}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(err.retry_safe);
    assert_eq!(err.data["commandsSent"], json!(false));
    assert!(sent(&mb, "console.exec").is_empty());
}

/// The mock for the ensure fallbacks: until a restart the player is `alive` on `team` and the rules
/// are `rules`; after it the player is alive on a team and a new round starts at 14.0.
async fn fallback_bridge(
    log: &Path,
    alive: bool,
    team: i64,
    rules: Value,
) -> (Arc<Flags>, MockBridge) {
    let flags = Arc::new(Flags::default());
    let ticks = Arc::new(AtomicU64::new(0));
    let (f, t) = (flags.clone(), ticks.clone());
    let f2 = flags.clone();
    let restarted_rules = rules_live(14.0, 1);
    let mb = base(log, &flags)
        .responder("game.local", move |_| {
            let restarted = f.restart.load(Ordering::SeqCst);
            let mut v = local(100 + t.fetch_add(8, Ordering::SeqCst), alive || restarted);
            v["player"]["team"] = json!(if restarted { 3 } else { team });
            v
        })
        .responder("game.rules", move |_| {
            if f2.restart.load(Ordering::SeqCst) {
                restarted_rules.clone()
            } else {
                rules.clone()
            }
        })
        .start()
        .await
        .unwrap();
    (flags, mb)
}

#[tokio::test]
async fn ensure_with_a_dead_player_falls_back_to_a_restart() {
    // a dead player never gets a live round without a respawn: the restart path, with the reason
    let log = TempLog::new("dead");
    std::fs::write(log.path(), "").unwrap();
    let (_flags, mb) = fallback_bridge(log.path(), false, 3, rules_live(12.5, 0)).await;
    let server = server_for(&mb, log.path()).await;
    let out = call(&server, "round_start", json!({"timeoutMs": 3000}))
        .await
        .unwrap();
    assert_eq!(out["fallback"], json!("restart"));
    assert_eq!(out["fallbackReason"], json!("player dead"));
    assert_eq!(out["commands"], json!(["mp_restartgame 1"]));
    assert_eq!(out["changed"], json!(true));
    assert_eq!(out["after"]["rules"]["roundStartTime"], json!(14.0));
    assert_eq!(out["after"]["player"]["alive"], json!(true));
    assert_eq!(sent(&mb, "console.exec").len(), 2);
}

#[tokio::test]
async fn ensure_with_a_spectator_or_a_round_winner_falls_back_to_a_restart() {
    let log = TempLog::new("spectator");
    std::fs::write(log.path(), "").unwrap();
    // alive, but on the spectator team: the round is not live for this player
    let (_f, mb) = fallback_bridge(log.path(), true, 1, rules_live(12.5, 0)).await;
    let server = server_for(&mb, log.path()).await;
    let out = call(&server, "round_start", json!({"timeoutMs": 3000}))
        .await
        .unwrap();
    assert_eq!(out["fallback"], json!("restart"));
    assert_eq!(
        out["fallbackReason"],
        json!("player is not on a team (spectator)")
    );

    let log2 = TempLog::new("winner");
    std::fs::write(log2.path(), "").unwrap();
    let mut winner = rules_live(12.5, 0);
    winner["roundWinStatus"] = json!(2);
    let (_f2, mb2) = fallback_bridge(log2.path(), true, 3, winner).await;
    let server2 = server_for(&mb2, log2.path()).await;
    let out2 = call(&server2, "round_start", json!({"timeoutMs": 3000}))
        .await
        .unwrap();
    assert_eq!(out2["fallback"], json!("restart"));
    assert_eq!(
        out2["fallbackReason"],
        json!("round has a winner (roundWinStatus set)")
    );
}

#[tokio::test]
async fn ensure_on_a_live_round_with_settings_sends_them_and_waits_for_nothing() {
    let log = TempLog::new("live-settings");
    std::fs::write(log.path(), "").unwrap();
    let flags = Arc::new(Flags::default());
    let ticks = Arc::new(AtomicU64::new(0));
    let t = ticks.clone();
    let mb = base(log.path(), &flags)
        .responder("game.local", move |_| ticking(&t))
        .responder_value("game.rules", rules_live(12.5, 0))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, log.path()).await;
    let out = call(
        &server,
        "round_start",
        json!({"freezetime": 0, "respawn": true, "cvars": {"bot_quota": 0}}),
    )
    .await
    .unwrap();
    assert_eq!(out["changed"], json!(true));
    assert_eq!(out["fallback"], json!(null));
    assert_eq!(
        out["commands"],
        json!([
            "mp_freezetime 0",
            "mp_respawn_on_death_ct 1",
            "mp_respawn_on_death_t 1",
            "bot_quota 0"
        ])
    );
    // no warmup end, no restart: the round stays live, and nothing waited for it
    assert_eq!(out["samples"], json!(0));
    assert_eq!(out["after"]["rules"]["roundStartTime"], json!(12.5));
    let calls = sent(&mb, "console.exec");
    assert_eq!(calls.len(), 5);
    assert!(calls.iter().all(|c| c["command"] != json!("mp_warmup_end")));
    assert!(calls
        .iter()
        .all(|c| c["command"] != json!("mp_restartgame 1")));
}

#[tokio::test]
async fn cvars_that_change_the_map_or_run_commands_send_nothing() {
    let log = TempLog::new("refused");
    std::fs::write(log.path(), "").unwrap();
    let flags = Arc::new(Flags::default());
    let mb = base(log.path(), &flags)
        .responder_value("game.local", local(100, true))
        .responder_value("game.rules", rules_live(12.5, 0))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, log.path()).await;
    for name in [
        "map",
        "map_workshop",
        "changelevel",
        "exec",
        "alias",
        "mp_restartgame",
        "mp_warmup_end",
        "quit",
        "host_timescale",
    ] {
        let err = call(&server, "round_start", json!({"cvars": {name: 1}}))
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{name}");
    }
    assert!(sent(&mb, "game.local").is_empty());
    assert!(sent(&mb, "input.clear").is_empty());
    assert!(sent(&mb, "console.exec").is_empty());
}

#[tokio::test]
async fn an_error_after_input_clear_says_the_input_was_cleared() {
    // a restart needs a baseline; the rules have none: refused after input.clear, nothing queued
    let log = TempLog::new("no-baseline");
    std::fs::write(log.path(), "").unwrap();
    let flags = Arc::new(Flags::default());
    let mb = base(log.path(), &flags)
        .responder_value("game.local", local(100, true))
        .responder_value(
            "game.rules",
            json!({"warmup": false, "freeze": false, "teamIntro": false, "gameRestart": false,
                   "roundWinStatus": 0, "roundStartTime": null, "roundsPlayed": null}),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, log.path()).await;
    let err = call(&server, "round_start", json!({"mode": "restart"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert_eq!(err.data["inputCleared"], json!(true));
    assert_eq!(err.data["commandsSent"], json!(false));
    assert_eq!(sent(&mb, "input.clear").len(), 1);
    assert!(sent(&mb, "console.exec").is_empty());
}

#[tokio::test]
async fn a_first_command_that_fails_is_retry_safe_and_reports_nothing_queued() {
    let log = TempLog::new("first-exec");
    std::fs::write(log.path(), "").unwrap();
    let flags = Arc::new(Flags::default());
    let mb = base(log.path(), &flags)
        .responder_value("game.local", local(100, true))
        .responder_value("game.rules", rules_live(12.5, 0))
        .error("console.exec", -32007, "Busy", None)
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, log.path()).await;
    let err = call(&server, "round_start", json!({"mode": "restart"}))
        .await
        .unwrap_err();
    assert!(err.retry_safe, "{err:?}");
    assert_eq!(err.data["commandsSent"], json!(false));
    assert_eq!(err.data["inputCleared"], json!(true));
}

#[tokio::test]
async fn the_round_wait_is_clamped_to_the_call_budget() {
    // the budget is 1500 ms: the restart never lands and the wait is cut to the budget, not 8 s
    let log = TempLog::new("budget");
    std::fs::write(log.path(), "").unwrap();
    let flags = Arc::new(Flags::default());
    let ticks = Arc::new(AtomicU64::new(0));
    let t = ticks.clone();
    let mb = base(log.path(), &flags)
        .responder("game.local", move |_| ticking(&t))
        .responder_value("game.rules", rules_live(12.5, 0))
        .start()
        .await
        .unwrap();
    let server = server_for_budget(&mb, log.path(), "1500").await;
    let started = std::time::Instant::now();
    let err = call(&server, "round_start", json!({"mode": "restart"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(!err.retry_safe);
    assert_eq!(err.data["clampedToBudget"], json!(true));
    assert_eq!(err.data["commandsSent"], json!(true));
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn timeout_ms_is_one_deadline_for_the_confirmation_and_the_wait() {
    let log = TempLog::new("deadline");
    std::fs::write(log.path(), "").unwrap();
    let flags = Arc::new(Flags::default());
    let ticks = Arc::new(AtomicU64::new(0));
    let t = ticks.clone();
    let mb = base(log.path(), &flags)
        .responder("game.local", move |_| ticking(&t))
        .responder_value("game.rules", rules_live(12.5, 0))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, log.path()).await;
    let started = std::time::Instant::now();
    let err = call(
        &server,
        "round_start",
        json!({"mode": "restart", "timeoutMs": 400}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    // the wait got what the confirmation left of the 400 ms
    assert!(err.data["timeoutMs"].as_u64().unwrap() <= 400);
    assert!(
        started.elapsed() < Duration::from_millis(1400),
        "{:?}",
        started.elapsed()
    );
}
