//! game_state / weapons / radar / select_weapon / sounds_* against the mock bridge: happy paths
//! assert the exact wire params and the overlay the tool adds; validation failures send nothing;
//! capability gating returns NotImplemented; the two new wire codes map retry-safe.
#![allow(clippy::result_large_err)]
use client_mcp::state::ClientConfig;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const METHODS: [&str; 15] = [
    "control.ping",
    "control.status",
    "control.capabilities",
    "control.version",
    "doc.active",
    "game.tick",
    "game.local",
    "game.weapons",
    "game.radar",
    "game.rules",
    "game.entities",
    "game.selectWeapon",
    "sound.drain",
    "sound.status",
    "sound.subscribe",
];

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

fn overviews_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cs2mcp-overviews-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("de_dust2.txt"), "\"de_dust2\"\n{\n\t\"material\"\t\"overviews/de_dust2_v2\"\n\t\"pos_x\"\t\"-2476\"\n\t\"pos_y\"\t\"3239\"\n\t\"scale\"\t\"4.4\"\n}\n").unwrap();
    dir
}

async fn server_for(
    mb: &MockBridge,
) -> cs2_mcp_common::mcp::McpServer<client_mcp::state::ClientState> {
    let dir = overviews_dir();
    client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--deadline-ms",
        "7000",
        "--overviews-dir",
        dir.to_str().unwrap(),
    ]))
    .unwrap()
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

fn sent(mb: &MockBridge, method: &str) -> Vec<Value> {
    mb.method_calls(method)
        .into_iter()
        .map(|r| r.params)
        .collect()
}

#[tokio::test]
async fn game_state_passes_through_with_deadline() {
    let local = json!({"tick": 100, "inGame": true, "map": "de_dust2", "playerSlot": 0, "viewAngles": [0.0, 90.0, 0.0], "player": {"controllerIndex": 1, "origin": [1.0, 2.0, 3.0]}});
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.local", local.clone())
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "game_state", json!({})).await.unwrap();
    assert_eq!(out["player"]["origin"], json!([1.0, 2.0, 3.0]));
    assert_eq!(out["viewAngles"], json!([0.0, 90.0, 0.0]));
    assert!(out["source"].as_str().unwrap().contains("game.local"));
    assert_eq!(sent(&mb, "game.local"), vec![json!({"deadlineMs": 7000})]);
}

#[tokio::test]
async fn weapons_passes_through() {
    let mb = MockBridge::builder().methods(METHODS).responder_value("game.weapons", json!({"active": {"name": "weapon_ak47", "clip1": 30}, "count": 1, "weapons": [{"name": "weapon_ak47"}]})).start().await.unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "weapons", json!({})).await.unwrap();
    assert_eq!(out["active"]["clip1"], json!(30));
    assert_eq!(out["count"], json!(1));
}

#[tokio::test]
async fn radar_projects_onto_the_overview() {
    let radar = json!({"tick": 5, "inGame": true, "map": "de_dust2",
        "players": [{"controllerIndex": 1, "origin": [-2476.0, 3239.0, 0.0], "team": 3}, {"controllerIndex": 2, "origin": null}],
        "bomb": [{"state": "dropped", "origin": [-2476.0 + 440.0, 3239.0 - 440.0, 10.0]}],
        "hostages": []});
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.radar", radar)
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "radar", json!({})).await.unwrap();
    assert_eq!(out["overview"]["posX"], json!(-2476.0));
    assert_eq!(out["overview"]["scale"], json!(4.4));
    assert_eq!(out["players"][0]["radar"], json!([0.0, 0.0]));
    assert!(out["players"][1].get("radar").is_none());
    let b = out["bomb"][0]["radar"].as_array().unwrap();
    assert!(
        (b[0].as_f64().unwrap() - 100.0).abs() < 1e-9
            && (b[1].as_f64().unwrap() - 100.0).abs() < 1e-9
    );
    // unknown map: no projection, a note, no error
    mb.add_responder_value("game.radar", json!({"tick": 5, "inGame": true, "map": "de_nowhere", "players": [], "bomb": [], "hostages": []}));
    let out = call(&server, "radar", json!({})).await.unwrap();
    assert_eq!(out["overview"], Value::Null);
    assert!(out["overviewNote"].as_str().unwrap().contains("de_nowhere"));
    // explicit map override wins
    let out = call(&server, "radar", json!({"map": "de_dust2"}))
        .await
        .unwrap();
    assert_eq!(out["overview"]["map"], json!("de_dust2"));
}

#[tokio::test]
async fn select_weapon_validates_then_sends_the_allowed_form() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value(
            "game.selectWeapon",
            json!({"queued": true, "command": "slot2"}),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    for bad in [
        json!({}),
        json!({"slot": 99}),
        json!({"command": "quit"}),
        json!({"weapon": "ak47"}),
        json!({"slot": 1, "weapon": "weapon_ak47"}),
    ] {
        let err = call(&server, "select_weapon", bad.clone())
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    assert!(sent(&mb, "game.selectWeapon").is_empty());
    let out = call(&server, "select_weapon", json!({"slot": 2}))
        .await
        .unwrap();
    assert_eq!(out["queued"], json!(true));
    assert_eq!(
        sent(&mb, "game.selectWeapon"),
        vec![json!({"slot": 2, "deadlineMs": 7000})]
    );
    call(&server, "select_weapon", json!({"weapon": "weapon_ak47"}))
        .await
        .unwrap();
    assert_eq!(
        sent(&mb, "game.selectWeapon")[1]["weapon"],
        json!("weapon_ak47")
    );
}

#[tokio::test]
async fn sounds_drain_and_status() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("sound.drain", json!({"events": [{"seq": 1, "hash": 123, "name": "Player.Footstep", "entindex": 7, "via": 11, "origin": [1.0, 2.0, 3.0]}], "dropped": 0, "total": 1, "pending": 0}))
        .responder_value("sound.status", json!({"installed": true, "mode": "wrap", "pending": 0, "total": 1, "dropped": 0, "ringSize": 256}))
        .responder_value("sound.subscribe", json!({"installed": true, "mode": "listener", "pending": 0, "total": 1, "dropped": 0, "ringSize": 256}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "sounds_drain", json!({"max": 0}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    let out = call(
        &server,
        "sounds_drain",
        json!({"max": 50, "resolve": false}),
    )
    .await
    .unwrap();
    assert_eq!(out["events"][0]["name"], json!("Player.Footstep"));
    assert_eq!(
        sent(&mb, "sound.drain"),
        vec![json!({"max": 50, "resolve": false, "deadlineMs": 7000})]
    );
    let st = call(&server, "sounds_status", json!({})).await.unwrap();
    assert_eq!(st["installed"], json!(true));
    assert_eq!(st["mode"], json!("wrap"));
    assert!(sent(&mb, "sound.subscribe").is_empty());
    let st = call(&server, "sounds_status", json!({"mode": "listener"}))
        .await
        .unwrap();
    assert_eq!(st["mode"], json!("listener"));
    assert_eq!(
        sent(&mb, "sound.subscribe"),
        vec![json!({"mode": "listener", "deadlineMs": 7000})]
    );
    let err = call(&server, "sounds_status", json!({"mode": "hook"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
}

#[tokio::test]
async fn gated_when_the_bridge_predates_m2_9() {
    let mb = MockBridge::start().await.unwrap(); // recorded Hammer caps: none of the game.* reads
    let server = server_for(&mb).await;
    for (tool, missing) in [
        ("game_state", "game.local"),
        ("radar", "game.radar"),
        ("sounds_drain", "sound.drain"),
    ] {
        let err = call(&server, tool, json!({})).await.unwrap_err();
        assert_eq!(err.name, "NotImplemented", "{tool}");
        assert_eq!(err.data["missing"], json!([missing]));
    }
}

#[tokio::test]
async fn new_wire_codes_are_retry_safe() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .error(
            "game.local",
            -32024,
            "NoLocalPlayer",
            Some(json!({"reason": "no local controller (not in a game?)"})),
        )
        .error(
            "game.radar",
            -32023,
            "SchemaUnavailable",
            Some(json!({"reason": "client.dll has not registered its schema scope yet"})),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "game_state", json!({})).await.unwrap_err();
    assert_eq!(err.name, "NoLocalPlayer");
    assert!(err.retry_safe);
    let err = call(&server, "radar", json!({})).await.unwrap_err();
    assert_eq!(err.name, "SchemaUnavailable");
    assert!(err.retry_safe);
}

/// `game.local` for a live local player at `tick` / origin x, holding `weapon` (defIndex 7).
fn local_at(tick: u64, x: f64, weapon: &str) -> Value {
    json!({"tick": tick, "inGame": true, "map": "de_dust2", "playerSlot": 0,
        "player": {"alive": true, "origin": [x, 0.0, 0.0], "velocity": [0.0, 0.0, 0.0],
            "activeWeapon": {"name": weapon, "defIndex": 7, "handle": 1}}})
}

#[tokio::test]
async fn game_wait_moved_matches_on_the_second_sample() {
    let n = Arc::new(AtomicU64::new(0));
    let c = n.clone();
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("game.local", move |_| {
            let i = c.fetch_add(1, Ordering::SeqCst);
            local_at(100 + 8 * i, 5.0 * i as f64, "weapon_glock")
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "game_wait", json!({"until": "moved"}))
        .await
        .unwrap();
    assert_eq!(out["matched"], json!(true));
    assert_eq!(out["until"], json!("moved"));
    assert_eq!(out["samples"], json!(2));
    assert_eq!(out["before"]["tick"], json!(100));
    assert_eq!(out["after"]["tick"], json!(108));
    assert_eq!(out["delta"]["ticks"], json!(8));
    assert!(out["delta"]["distance"].as_f64().unwrap() >= 1.0);
    assert_eq!(out["after"]["activeWeapon"]["name"], json!("weapon_glock"));
    assert_eq!(sent(&mb, "game.local").len(), 2);
}

#[tokio::test]
async fn game_wait_frozen_tick_times_out_paced_by_poll() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.local", local_at(100, 0.0, "weapon_glock"))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let started = Instant::now();
    let err = call(
        &server,
        "game_wait",
        json!({"until": "tick", "timeoutMs": 400, "pollMs": 100}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(err.retry_safe);
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert!(started.elapsed() < Duration::from_millis(1000));
    assert_eq!(err.data["reason"], json!("timeout"));
    assert_eq!(err.data["requestedTimeoutMs"], json!(400));
    assert_eq!(err.data["timeoutMs"], json!(400));
    assert_eq!(err.data["clampedToBudget"], json!(false));
    assert_eq!(err.data["before"]["tick"], json!(100));
    assert_eq!(err.data["last"]["tick"], json!(100));
    // one real sample per pollMs, not a sample loop and not one blind sleep
    let samples = sent(&mb, "game.local").len();
    assert!((3..=6).contains(&samples), "samples {samples}");
    assert_eq!(err.data["samples"], json!(samples));
}

#[tokio::test]
async fn game_wait_weapon_flip_matches_on_the_third_sample() {
    let n = Arc::new(AtomicU64::new(0));
    let c = n.clone();
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("game.local", move |_| {
            let i = c.fetch_add(1, Ordering::SeqCst);
            let weapon = if i < 2 { "weapon_glock" } else { "weapon_ak47" };
            local_at(100 + i, 0.0, weapon)
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(
        &server,
        "game_wait",
        json!({"until": "weapon", "weapon": "weapon_ak47", "pollMs": 50}),
    )
    .await
    .unwrap();
    assert_eq!(out["samples"], json!(3));
    assert_eq!(out["after"]["activeWeapon"]["name"], json!("weapon_ak47"));
    assert_eq!(out["before"]["activeWeapon"]["name"], json!("weapon_glock"));
    assert_eq!(sent(&mb, "game.local").len(), 3);
}

#[tokio::test]
async fn game_wait_weapon_by_def_index_and_tick_from_baseline() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.local", local_at(104, 0.0, "weapon_glock"))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    // defIndex 7 is the glock in local_at: matches on the first sample
    let out = call(
        &server,
        "game_wait",
        json!({"until": "weapon", "defIndex": 7}),
    )
    .await
    .unwrap();
    assert_eq!(out["samples"], json!(1));
    // tick from an explicit baseline: 104 >= 100 + 4
    let out = call(
        &server,
        "game_wait",
        json!({"until": "tick", "ticks": 4, "from": {"tick": 100, "origin": [0.0, 0.0, 0.0]}}),
    )
    .await
    .unwrap();
    assert_eq!(out["samples"], json!(1));
    assert_eq!(out["before"]["tick"], json!(100));
    assert_eq!(out["delta"]["ticks"], json!(4));
}

#[tokio::test]
async fn game_wait_stopped_needs_two_still_samples_with_ticks() {
    let n = Arc::new(AtomicU64::new(0));
    let c = n.clone();
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("game.local", move |_| {
            let i = c.fetch_add(1, Ordering::SeqCst);
            let mut v = local_at(10 + i, 0.0, "weapon_glock");
            if i == 0 {
                v["player"]["velocity"] = json!([250.0, 0.0, 0.0]);
            }
            v
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "game_wait", json!({"until": "stopped"}))
        .await
        .unwrap();
    assert_eq!(out["samples"], json!(3));
    assert_eq!(out["after"]["tick"], json!(12));
}

#[tokio::test]
async fn game_wait_budget_clamps_the_timeout() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.local", local_at(100, 0.0, "weapon_glock"))
        .start()
        .await
        .unwrap();
    let dir = overviews_dir();
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--deadline-ms",
        "7000",
        "--overviews-dir",
        dir.to_str().unwrap(),
        "--tool-budget-ms",
        "1500",
    ]))
    .unwrap();
    let started = Instant::now();
    let err = call(
        &server,
        "game_wait",
        json!({"until": "tick", "timeoutMs": 10000}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(started.elapsed() < Duration::from_millis(3000));
    assert!(err.data["timeoutMs"].as_u64().unwrap() <= 1000);
    assert_eq!(err.data["requestedTimeoutMs"], json!(10000));
    assert_eq!(err.data["clampedToBudget"], json!(true));
}

#[tokio::test]
async fn game_wait_fails_fast_when_not_in_game_or_dead() {
    let not_in_game = json!({"tick": 1, "inGame": false, "map": null, "player": null});
    let mut dead = local_at(1, 0.0, "weapon_glock");
    dead["player"]["alive"] = json!(false);
    for local in [not_in_game, dead] {
        let mb = MockBridge::builder()
            .methods(METHODS)
            .responder_value("game.local", local)
            .start()
            .await
            .unwrap();
        let server = server_for(&mb).await;
        let started = Instant::now();
        let err = call(
            &server,
            "game_wait",
            json!({"until": "moved", "timeoutMs": 5000}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.name, "PrerequisiteFailed");
        assert!(err.retry_safe);
        assert!(started.elapsed() < Duration::from_millis(1000));
        assert_eq!(sent(&mb, "game.local").len(), 1);
    }
}

#[tokio::test]
async fn game_wait_validates_before_sending_anything() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.local", local_at(1, 0.0, "weapon_glock"))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    for bad in [
        json!({}),
        json!({"until": "nope"}),
        json!({"until": "tick", "ticks": 0}),
        json!({"until": "tick", "timeoutMs": 0}),
        json!({"until": "tick", "pollMs": 10}),
        json!({"until": "tick", "pollMs": 5000}),
        json!({"until": "tick", "bogus": 1}),
        json!({"until": "tick", "weapon": "weapon_ak47"}),
        json!({"until": "moved", "minDistance": -1}),
        json!({"until": "moved", "from": {"tick": 1}}),
        json!({"until": "stopped", "from": {"tick": 1, "origin": [0, 0, 0]}}),
        json!({"until": "weapon"}),
        json!({"until": "weapon", "weapon": "ak47"}),
        json!({"until": "weapon", "weapon": "weapon_ak47", "defIndex": 7}),
        json!({"until": "weapon", "defIndex": 70000}),
    ] {
        assert!(
            call(&server, "game_wait", bad.clone()).await.is_err(),
            "{bad}"
        );
    }
    assert!(sent(&mb, "game.local").is_empty());
}

#[tokio::test]
async fn move_and_weapon_hints_point_at_game_wait() {
    let mb = MockBridge::builder()
        .methods(METHODS.iter().copied().chain(["input.set"]))
        .responder_value("input.set", json!({"queued": true}))
        .responder_value(
            "game.selectWeapon",
            json!({"queued": true, "via": "MakeWeaponSelection", "weapon": {"name": "weapon_knife_t", "defIndex": 59, "handle": 5}}),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "input_set", json!({"press": ["forward"]}))
        .await
        .unwrap();
    assert_eq!(out["next"], json!("game_wait {\"until\":\"moved\"}"));
    let out = call(&server, "input_set", json!({"press": ["attack"]}))
        .await
        .unwrap();
    assert!(out.get("next").is_none());
    // the hint follows the weapon the bridge picked, not the requested name: weapon_knife -> a T's knife
    let out = call(&server, "select_weapon", json!({"weapon": "weapon_knife"}))
        .await
        .unwrap();
    assert_eq!(
        out["next"],
        json!("game_wait {\"until\":\"weapon\",\"defIndex\":59}")
    );
    // no usable defIndex on the picked weapon: its name
    mb.add_responder_value(
        "game.selectWeapon",
        json!({"queued": true, "via": "MakeWeaponSelection", "weapon": {"name": "weapon_ak47", "defIndex": -1, "handle": 7}}),
    );
    let out = call(&server, "select_weapon", json!({"defIndex": 7}))
        .await
        .unwrap();
    assert_eq!(
        out["next"],
        json!("game_wait {\"until\":\"weapon\",\"weapon\":\"weapon_ak47\"}")
    );
    // no weapon in the reply: the argument stands in (weapon name, then defIndex)
    mb.add_responder_value(
        "game.selectWeapon",
        json!({"queued": true, "via": "MakeWeaponSelection"}),
    );
    let out = call(&server, "select_weapon", json!({"weapon": "weapon_ak47"}))
        .await
        .unwrap();
    assert_eq!(
        out["next"],
        json!("game_wait {\"until\":\"weapon\",\"weapon\":\"weapon_ak47\"}")
    );
    let out = call(&server, "select_weapon", json!({"defIndex": 7}))
        .await
        .unwrap();
    assert_eq!(
        out["next"],
        json!("game_wait {\"until\":\"weapon\",\"defIndex\":7}")
    );
    // a handle has no name or defIndex to hint with; slot/command select no weapon at all
    let out = call(&server, "select_weapon", json!({"handle": 303}))
        .await
        .unwrap();
    assert!(out.get("next").is_none());
    let out = call(&server, "select_weapon", json!({"slot": 2}))
        .await
        .unwrap();
    assert!(out.get("next").is_none());
}

/// Each game.local deadline is min(budget, left + 250), where left is the wait's remainder when the
/// sample is sent: the first sample gets timeoutMs + 250 (the budget is 7000 here), and a stalled
/// bridge cannot hold the call past the wait's end plus the slack.
#[tokio::test]
async fn game_wait_sends_the_budget_deadline_on_every_sample() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.local", local_at(100, 0.0, "weapon_glock"))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(
        &server,
        "game_wait",
        json!({"until": "tick", "timeoutMs": 350, "pollMs": 100}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    let sent_samples = sent(&mb, "game.local");
    assert!(sent_samples.len() >= 3, "{}", sent_samples.len());
    let first = sent_samples[0]["deadlineMs"].as_u64().unwrap();
    assert!((550..=600).contains(&first), "first deadlineMs {first}");
    for p in &sent_samples {
        let d = p["deadlineMs"].as_u64().unwrap();
        assert!((1..=600).contains(&d), "deadlineMs {d}");
    }
    // timeoutMs 1: one sample, deadline = 1 + 250 (not the 7000 budget, not a shrunk value)
    mb.reset_trace();
    let err = call(
        &server,
        "game_wait",
        json!({"until": "tick", "timeoutMs": 1}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    let sent_samples = sent(&mb, "game.local");
    assert_eq!(sent_samples.len(), 1);
    let d = sent_samples[0]["deadlineMs"].as_u64().unwrap();
    assert!((245..=251).contains(&d), "deadlineMs {d}");
}

/// With a 1500 ms budget and a 1000 ms wait, the first sample gets timeoutMs + 250 (1250) and the
/// late samples get the wait's remainder + 250 (about 250..450 ms near the end), never the ~500 ms
/// of budget that is left.
#[tokio::test]
async fn game_wait_late_samples_keep_the_budget_deadline() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.local", local_at(100, 0.0, "weapon_glock"))
        .start()
        .await
        .unwrap();
    let dir = overviews_dir();
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--overviews-dir",
        dir.to_str().unwrap(),
        "--tool-budget-ms",
        "1500",
    ]))
    .unwrap();
    let _ = call(
        &server,
        "game_wait",
        json!({"until": "tick", "timeoutMs": 1000, "pollMs": 100}),
    )
    .await
    .unwrap_err();
    let sent_samples = sent(&mb, "game.local");
    assert!(sent_samples.len() >= 5, "{}", sent_samples.len());
    let first = sent_samples[0]["deadlineMs"].as_u64().unwrap();
    assert!((1150..=1250).contains(&first), "first deadlineMs {first}");
    // the last sample starts within the last ~200 ms of the wait: left <= 200, so deadline <= 450
    let last = sent_samples.last().unwrap()["deadlineMs"].as_u64().unwrap();
    assert!((250..=460).contains(&last), "last deadlineMs {last}");
}

/// A later sample that the bridge queue drops (QueueTimeout) ends the wait as WaitTimeout with the
/// last good sample, not as a QueueTimeout error.
#[tokio::test]
async fn game_wait_queue_timeout_on_a_later_sample_is_wait_timeout() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.local", local_at(100, 0.0, "weapon_glock"))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    // the first sample is fast; from 50 ms on the bridge delay exceeds the deadline (7000)
    let (res, _) = tokio::join!(
        call(
            &server,
            "game_wait",
            json!({"until": "tick", "timeoutMs": 1000, "pollMs": 100}),
        ),
        async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            mb.add_delay("game.local", 10_000);
        }
    );
    let err = res.unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(err.retry_safe);
    assert_eq!(err.data["samples"], json!(1));
    assert_eq!(err.data["reason"], json!("bridgeQueueTimeout"));
    assert_eq!(err.data["bridgeError"]["code"], json!(-32008));
    assert!(
        err.message.contains("did not serve game.local"),
        "{}",
        err.message
    );
    assert_eq!(err.data["last"]["tick"], json!(100));
}

/// velocity null (a failed schema read) must not stop until:tick or until:weapon; only stopped
/// needs velocity, and it fails with a retry-safe SchemaUnavailable.
#[tokio::test]
async fn game_wait_null_fields_fail_only_the_predicate_that_needs_them() {
    let mut local = local_at(10, 0.0, "weapon_glock");
    local["player"]["velocity"] = Value::Null;
    local["player"]["origin"] = Value::Null;
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.local", local)
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(
        &server,
        "game_wait",
        json!({"until": "tick", "from": {"tick": 9, "origin": [0.0, 0.0, 0.0]}, "timeoutMs": 500}),
    )
    .await
    .unwrap();
    assert_eq!(out["samples"], json!(1));
    assert_eq!(out["after"]["origin"], Value::Null);
    let out = call(
        &server,
        "game_wait",
        json!({"until": "weapon", "defIndex": 7}),
    )
    .await
    .unwrap();
    assert_eq!(out["matched"], json!(true));
    let err = call(
        &server,
        "game_wait",
        json!({"until": "stopped", "timeoutMs": 500}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "SchemaUnavailable");
    assert!(err.retry_safe);
    let err = call(
        &server,
        "game_wait",
        json!({"until": "moved", "timeoutMs": 500}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "SchemaUnavailable");
}

/// `game.local` at `origin` with a still velocity and `view_angles` (top-level `viewAngles`).
fn local_at_xyz(tick: u64, origin: [f64; 3], view_angles: [f64; 3]) -> Value {
    let mut v = local_at(tick, origin[0], "weapon_glock");
    v["player"]["origin"] = json!(origin);
    v["viewAngles"] = json!(view_angles);
    v
}

#[tokio::test]
async fn game_wait_at_matches_when_the_origin_converges() {
    // x walks 100 -> 50 -> 0 and stays; still every sample, so the match needs two still samples
    let n = Arc::new(AtomicU64::new(0));
    let c = n.clone();
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("game.local", move |_| {
            let i = c.fetch_add(1, Ordering::SeqCst);
            let x = (100.0 - 50.0 * i as f64).max(0.0);
            local_at_xyz(100 + i, [x, 0.0, 64.0], [0.0, 0.0, 0.0])
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(
        &server,
        "game_wait",
        json!({"until": "at", "origin": [0.0, 0.0, 64.0]}),
    )
    .await
    .unwrap();
    assert_eq!(out["matched"], json!(true));
    assert_eq!(out["until"], json!("at"));
    // samples 0 (x 100) and 1 (x 50) are out; sample 2 is on target but its previous sample was not,
    // so the match is sample 3 (on target, previous sample on target, tick advanced)
    assert_eq!(out["samples"], json!(4));
    assert_eq!(out["after"]["origin"], json!([0.0, 0.0, 64.0]));
    assert_eq!(out["delta"]["toTarget"]["xy"], json!(0.0));
    assert_eq!(out["delta"]["toTarget"]["z"], json!(0.0));
    assert_eq!(sent(&mb, "game.local").len(), 4);
}

#[tokio::test]
async fn game_wait_at_yaw_wraps_across_180() {
    // target yaw 179, actual -179: 2 degrees apart, inside the 3 degree tolerance
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value(
            "game.local",
            local_at_xyz(100, [1.0, 0.0, 0.0], [0.0, -179.0, 0.0]),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(
        &server,
        "game_wait",
        json!({"until": "at", "origin": [1.0, 0.0, 0.0], "angles": [0.0, 179.0, 0.0],
            "angleTolerance": 3.0, "requireStopped": false}),
    )
    .await
    .unwrap();
    assert_eq!(out["samples"], json!(1));
    assert_eq!(out["after"]["viewAngles"], json!([0.0, -179.0, 0.0]));
    // 170 against -179 is 11 degrees apart: a 1 degree tolerance times out
    let err = call(
        &server,
        "game_wait",
        json!({"until": "at", "origin": [1.0, 0.0, 0.0], "angles": [0.0, 170.0, 0.0],
            "angleTolerance": 1.0, "requireStopped": false, "timeoutMs": 150}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
}

#[tokio::test]
async fn game_wait_at_require_stopped_waits_for_zero_velocity() {
    // moving (250 u/s) on samples 0 and 1, then still: the match needs this and the previous sample still
    let n = Arc::new(AtomicU64::new(0));
    let c = n.clone();
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("game.local", move |_| {
            let i = c.fetch_add(1, Ordering::SeqCst);
            let mut v = local_at_xyz(100 + i, [0.0, 0.0, 0.0], [0.0, 0.0, 0.0]);
            if i < 2 {
                v["player"]["velocity"] = json!([250.0, 0.0, 0.0]);
            }
            v
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(
        &server,
        "game_wait",
        json!({"until": "at", "origin": [0.0, 0.0, 0.0], "pollMs": 50}),
    )
    .await
    .unwrap();
    // sample 2 is on target but its previous sample (1) still moved; sample 3 has two still samples
    assert_eq!(out["samples"], json!(4));
    assert_eq!(out["after"]["tick"], json!(103));
}

#[tokio::test]
async fn game_wait_at_out_of_tolerance_times_out_with_the_last_sample() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value(
            "game.local",
            local_at_xyz(100, [50.0, 0.0, 0.0], [0.0, 0.0, 0.0]),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let started = Instant::now();
    let err = call(
        &server,
        "game_wait",
        json!({"until": "at", "origin": [0.0, 0.0, 0.0], "timeoutMs": 300, "pollMs": 100}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert!(err.retry_safe);
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(err.data["until"], json!("at"));
    assert_eq!(err.data["reason"], json!("timeout"));
    assert_eq!(err.data["last"]["origin"], json!([50.0, 0.0, 0.0]));
    assert_eq!(err.data["last"]["viewAngles"], json!([0.0, 0.0, 0.0]));
    assert_eq!(err.data["last"]["toTarget"]["xy"], json!(50.0));
    assert_eq!(err.data["last"]["toTarget"]["z"], json!(0.0));
    assert!(sent(&mb, "game.local").len() >= 2);
}

#[tokio::test]
async fn game_wait_at_previous_sample_must_be_on_target_and_tick_must_advance() {
    // sample 0 is off target and still, sample 1 is on target and still: no match yet (its previous
    // sample was off). Sample 2 is on target, still, at a later tick: match.
    let n = Arc::new(AtomicU64::new(0));
    let c = n.clone();
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("game.local", move |_| {
            let i = c.fetch_add(1, Ordering::SeqCst);
            let x = if i == 0 { 50.0 } else { 0.0 };
            local_at_xyz(100 + i, [x, 0.0, 0.0], [0.0; 3])
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(
        &server,
        "game_wait",
        json!({"until": "at", "origin": [0.0, 0.0, 0.0], "pollMs": 50}),
    )
    .await
    .unwrap();
    assert_eq!(out["samples"], json!(3));
    assert_eq!(out["after"]["tick"], json!(102));
}

#[tokio::test]
async fn game_wait_at_frozen_tick_on_target_does_not_match() {
    // on target and still, but the tick never advances: the stopped check cannot be satisfied
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.local", local_at_xyz(100, [0.0; 3], [0.0; 3]))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(
        &server,
        "game_wait",
        json!({"until": "at", "origin": [0.0, 0.0, 0.0], "timeoutMs": 250, "pollMs": 50}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WaitTimeout");
    assert_eq!(err.data["reason"], json!("timeout"));
    assert!(sent(&mb, "game.local").len() >= 2);
}

#[tokio::test]
async fn game_wait_at_invalid_arguments_send_nothing() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.local", local_at_xyz(100, [0.0; 3], [0.0; 3]))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    for bad in [
        json!({"until": "at"}),
        json!({"until": "at", "origin": [0, 0]}),
        json!({"until": "at", "origin": [0, 0, "x"]}),
        json!({"until": "at", "origin": [0, 0, 0], "tolerance": -1}),
        json!({"until": "at", "origin": [0, 0, 0], "zTolerance": -0.5}),
        json!({"until": "at", "origin": [0, 0, 0], "angles": [0, 0]}),
        json!({"until": "at", "origin": [0, 0, 0], "angleTolerance": 2}),
        json!({"until": "at", "origin": [0, 0, 0], "angles": [0, 0, 0], "angleTolerance": -2}),
        json!({"until": "at", "origin": [0, 0, 0], "requireStopped": 1}),
        json!({"until": "at", "origin": [0, 0, 0], "ticks": 4}),
        json!({"until": "at", "origin": [0, 0, 0], "from": {"tick": 1, "origin": [0, 0, 0]}}),
        json!({"until": "tick", "origin": [0, 0, 0]}),
    ] {
        let err = call(&server, "game_wait", bad.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    assert!(sent(&mb, "game.local").is_empty());
}

/// `game.rules` for a live round (no warmup/freeze/intro/restart, no winner) that started at `start`.
fn rules_live(start: f64, played: i64) -> Value {
    json!({"tick": 9, "inGame": true, "map": "de_dust2", "proxyIndex": 65,
        "warmup": false, "freeze": false, "teamIntro": false, "gameRestart": false,
        "hasMatchStarted": true, "gamePhase": 3, "roundsPlayed": played, "roundStartTime": start,
        "restartRoundTime": 0.0, "roundTimeSec": 60, "roundWinStatus": 0, "roundEndReason": 0,
        "bombPlanted": false, "schemaMissing": []})
}

/// `rules_live` with the warmup flag set: the round has not started.
fn rules_warmup(start: f64) -> Value {
    let mut v = rules_live(start, 0);
    v["warmup"] = json!(true);
    v
}

/// `game.local` with an active weapon carrying `clip` rounds in clip1.
fn local_with_clip(tick: u64, clip: i64) -> Value {
    let mut v = local_at(tick, 0.0, "weapon_hkp2000");
    v["player"]["activeWeapon"]["clip1"] = json!(clip);
    v
}

#[tokio::test]
async fn game_rules_passes_through_and_is_gated() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.rules", rules_live(12.5, 0))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "game_rules", json!({})).await.unwrap();
    assert_eq!(out["warmup"], json!(false));
    assert_eq!(out["roundStartTime"], json!(12.5));
    assert_eq!(out["schemaMissing"], json!([]));
    assert!(out["source"].as_str().unwrap().contains("game.rules"));
    assert_eq!(sent(&mb, "game.rules"), vec![json!({"deadlineMs": 7000})]);

    // a bridge without game.rules (m2.9 or older): NotImplemented before anything is sent
    let old = MockBridge::builder()
        .methods(["game.local", "game.radar"])
        .start()
        .await
        .unwrap();
    let server = server_for(&old).await;
    let err = call(&server, "game_rules", json!({})).await.unwrap_err();
    assert_eq!(err.name, "NotImplemented");
    assert_eq!(err.data["missing"], json!(["game.rules"]));
    assert!(sent(&old, "game.rules").is_empty());
}

#[tokio::test]
async fn entity_find_validates_then_sends_the_filter() {
    let entities = json!({"tick": 9, "inGame": true, "map": "editor/zoo/smartprop_zoo",
        "entities": [{"entindex": 412, "class": "func_button", "name": "mdlchange_button_scaleup",
            "origin": [1.0, 2.0, 3.0], "angles": [0.0, 90.0, 0.0], "health": 100, "toggleState": null}],
        "count": 1, "truncated": false});
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.entities", entities)
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    for bad in [
        json!({}),
        json!({"class": "func button"}),
        json!({"class": ""}),
        json!({"class": 5}),
        json!({"name": ""}),
        json!({"name": "bad\nname"}),
        json!({"name": "x".repeat(128)}),
        json!({"class": "func_button", "max": 0}),
        json!({"class": "func_button", "max": 257}),
    ] {
        let err = call(&server, "entity_find", bad.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    assert!(
        sent(&mb, "game.entities").is_empty(),
        "validation sends nothing"
    );

    let out = call(
        &server,
        "entity_find",
        json!({"class": "func_button", "name": "mdlchange_button_scaleup", "max": 8}),
    )
    .await
    .unwrap();
    assert_eq!(
        sent(&mb, "game.entities"),
        vec![
            json!({"class": "func_button", "name": "mdlchange_button_scaleup", "max": 8, "deadlineMs": 7000})
        ]
    );
    assert_eq!(out["count"], json!(1));
    assert_eq!(out["entities"][0]["entindex"], json!(412));
    assert_eq!(out["entities"][0]["origin"], json!([1.0, 2.0, 3.0]));
    assert_eq!(out["entities"][0]["toggleState"], Value::Null);
    assert_eq!(out["truncated"], json!(false));
    assert!(out["source"].as_str().unwrap().contains("game.entities"));

    // name alone is a valid filter
    call(&server, "entity_find", json!({"name": "door_1"}))
        .await
        .unwrap();
    assert_eq!(
        sent(&mb, "game.entities")[1],
        json!({"name": "door_1", "deadlineMs": 7000})
    );
}

#[tokio::test]
async fn game_wait_round_live_waits_for_warmup_to_end() {
    let rules_n = Arc::new(AtomicU64::new(0));
    let rc = rules_n.clone();
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("game.local", |_| local_at(100, 0.0, "weapon_glock"))
        .responder("game.rules", move |_| {
            // warmup on the first two samples, live from the third
            if rc.fetch_add(1, Ordering::SeqCst) < 2 {
                rules_warmup(0.0)
            } else {
                rules_live(12.5, 0)
            }
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "game_wait", json!({"until": "roundLive"}))
        .await
        .unwrap();
    assert_eq!(out["matched"], json!(true));
    assert_eq!(out["until"], json!("roundLive"));
    assert_eq!(out["samples"], json!(3));
    assert_eq!(out["rules"]["warmup"], json!(false));
    // every sample reads both: one game.local and one game.rules each
    assert_eq!(sent(&mb, "game.local").len(), 3);
    assert_eq!(sent(&mb, "game.rules").len(), 3);
}

#[tokio::test]
async fn game_wait_round_live_waits_through_a_death() {
    // dead for the first two samples (a round wait does not fail fast on a death), alive and live after
    let n = Arc::new(AtomicU64::new(0));
    let c = n.clone();
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("game.local", move |_| {
            let i = c.fetch_add(1, Ordering::SeqCst);
            let mut v = local_at(100 + i, 0.0, "weapon_glock");
            v["player"]["alive"] = json!(i >= 2);
            v
        })
        .responder_value("game.rules", rules_live(12.5, 0))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "game_wait", json!({"until": "roundLive"}))
        .await
        .unwrap();
    assert_eq!(out["samples"], json!(3));
    assert_eq!(out["after"]["tick"], json!(102));
}

#[tokio::test]
async fn game_wait_round_restarted_needs_the_new_round_to_be_live() {
    // baseline roundStartTime 12.5; the restart shows as 14.0 in the restart window, live after that
    let mut restart_window = rules_live(14.0, 0);
    restart_window["gameRestart"] = json!(true);
    let seq = [
        rules_live(12.5, 0),
        rules_live(12.5, 0),
        restart_window,
        rules_live(14.0, 0),
    ];
    let n = Arc::new(AtomicU64::new(0));
    let c = n.clone();
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("game.local", |_| local_at(100, 0.0, "weapon_glock"))
        .responder("game.rules", move |_| {
            let i = c.fetch_add(1, Ordering::SeqCst) as usize;
            seq[i.min(seq.len() - 1)].clone()
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "game_wait", json!({"until": "roundRestarted"}))
        .await
        .unwrap();
    assert_eq!(out["samples"], json!(4));
    assert_eq!(out["rules"]["roundStartTime"], json!(14.0));
    assert_eq!(out["rules"]["gameRestart"], json!(false));

    // a caller baseline of roundsPlayed 2 against 3 rounds played: already a restart, so it matches at once
    let mb2 = MockBridge::builder()
        .methods(METHODS)
        .responder("game.local", |_| local_at(100, 0.0, "weapon_glock"))
        .responder_value("game.rules", rules_live(12.5, 3))
        .start()
        .await
        .unwrap();
    let server2 = server_for(&mb2).await;
    let out = call(
        &server2,
        "game_wait",
        json!({"until": "roundRestarted", "roundsPlayed": 2}),
    )
    .await
    .unwrap();
    assert_eq!(out["samples"], json!(1));
}

#[tokio::test]
async fn game_wait_ammo_matches_after_one_shot() {
    let n = Arc::new(AtomicU64::new(0));
    let c = n.clone();
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("game.local", move |_| {
            // 13 rounds, then one shot (12) from the third sample on
            let i = c.fetch_add(1, Ordering::SeqCst);
            local_with_clip(100 + i, if i < 2 { 13 } else { 12 })
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(
        &server,
        "game_wait",
        json!({"until": "ammo", "clip1Delta": -1}),
    )
    .await
    .unwrap();
    assert_eq!(out["samples"], json!(3));
    assert_eq!(out["delta"]["clip1"], json!(-1));
    assert_eq!(out["after"]["activeWeapon"]["clip1"], json!(12));
    // ammo reads game.local only
    assert!(sent(&mb, "game.rules").is_empty());
}

#[tokio::test]
async fn game_wait_round_predicates_need_game_rules_and_other_waits_do_not() {
    let mut methods: Vec<&str> = METHODS.to_vec();
    methods.retain(|m| *m != "game.rules");
    let n = Arc::new(AtomicU64::new(0));
    let c = n.clone();
    let mb = MockBridge::builder()
        .methods(methods)
        .responder("game.local", move |_| {
            let i = c.fetch_add(1, Ordering::SeqCst);
            local_at(100 + 8 * i, 5.0 * i as f64, "weapon_glock")
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "game_wait", json!({"until": "roundLive"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "NotImplemented");
    assert_eq!(err.data["missing"], json!(["game.rules"]));
    assert!(
        sent(&mb, "game.local").is_empty(),
        "the capability gate sends nothing"
    );
    // moved keeps working on a bridge without game.rules
    let out = call(&server, "game_wait", json!({"until": "moved"}))
        .await
        .unwrap();
    assert_eq!(out["matched"], json!(true));
}

#[tokio::test]
async fn session_status_keeps_game_state_on_an_m2_9_bridge() {
    // the m2.9 method set: no game.rules / game.entities. game_state (and its tools) stays available;
    // only the m2.10 group is reported missing
    let m29: Vec<&str> = METHODS
        .iter()
        .copied()
        .filter(|m| !matches!(*m, "game.rules" | "game.entities"))
        .collect();
    let mb = MockBridge::builder().methods(m29).start().await.unwrap();
    let server = server_for(&mb).await;
    let status = call(&server, "session_status", json!({})).await.unwrap();
    let groups = &status["toolGroups"];
    assert_eq!(groups["game_state"]["available"], json!(true), "{status}");
    assert_eq!(groups["game_rules"]["available"], json!(false), "{status}");
    assert_eq!(
        groups["game_rules"]["missing"],
        json!(["game.rules", "game.entities"]),
        "{status}"
    );
}

fn local_weapon(tick: u64, name: &str, entindex: i64, clip: i64) -> Value {
    let mut v = local_at(tick, 0.0, name);
    v["player"]["activeWeapon"]["entindex"] = json!(entindex);
    v["player"]["activeWeapon"]["clip1"] = json!(clip);
    v
}

#[tokio::test]
async fn game_wait_ammo_waits_out_a_weapon_switch() {
    // pistol 13 -> switched to a glock at 10 (not our shot) -> back on the pistol at 12 (one shot)
    let n = Arc::new(AtomicU64::new(0));
    let c = n.clone();
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("game.local", move |_| {
            let i = c.fetch_add(1, Ordering::SeqCst);
            match i {
                0 => local_weapon(100, "weapon_hkp2000", 300, 13),
                1 => local_weapon(101, "weapon_glock", 301, 10),
                _ => local_weapon(102, "weapon_hkp2000", 300, 12),
            }
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "game_wait", json!({"until": "ammo"}))
        .await
        .unwrap();
    assert_eq!(out["samples"], json!(3));
    assert_eq!(out["delta"]["clip1"], json!(-1));
    assert_eq!(out["after"]["activeWeapon"]["entindex"], json!(300));
}

#[tokio::test]
async fn game_wait_ammo_caller_baseline_matches_a_shot_before_the_first_sample() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.local", local_weapon(100, "weapon_hkp2000", 300, 12))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(
        &server,
        "game_wait",
        json!({"until": "ammo", "clip1": 13, "entindex": 300}),
    )
    .await
    .unwrap();
    assert_eq!(out["samples"], json!(1));
    assert_eq!(out["delta"]["clip1"], json!(-1));
}

#[tokio::test]
async fn game_wait_ammo_without_an_active_weapon_fails_fast() {
    let mut local = local_at(100, 0.0, "weapon_glock");
    local["player"]["activeWeapon"] = Value::Null;
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("game.local", local)
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "game_wait", json!({"until": "ammo"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert_eq!(sent(&mb, "game.local").len(), 1);
}
