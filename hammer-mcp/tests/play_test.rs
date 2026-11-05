//! The play-test tools of the tools session (`game_state`, `input_sequence`, `play_test`, and the
//! `game_local` alias) against the mock bridge: a stateful `game.local` (the tick advances per read,
//! x grows while `forward` is held) and a temp `game/csgo/console.log` the tests append to.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{MockBridge, MockBuilder};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "play-test-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("workshop/run/app/game/csgo")).expect("tmp tree");
        Tree { root }
    }
    fn console(&self) -> PathBuf {
        self.root.join("workshop/run/app/game/csgo/console.log")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn build(tree: &Tree, mb: &MockBridge, budget_ms: u64) -> McpServer<HammerState> {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let discovery = mb.discovery_path.display().to_string();
    let budget = budget_ms.to_string();
    let args = [
        "--deadline-ms", "5000", "--tool-budget-ms", &budget, "--discovery", &discovery,
        "--project-root", &root,
    ];
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&args)].concat();
    hammer_mcp::build_server(&W::parse_from(argv).c).expect("server")
}

async fn call(server: &McpServer<HammerState>, name: &str, args: Value) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

fn names_contain(v: &Value, name: &str) -> bool {
    v.as_array().is_some_and(|a| a.iter().any(|x| x == name))
}

/// A mock engine: `game.local`'s tick advances 8 per read; x grows 10 per read while `forward` is
/// held by the input.set calls; the player dies once x passes `die_at`.
fn engine(die_at: f64, customize: impl FnOnce(MockBuilder) -> MockBuilder) -> MockBuilder {
    let tick = Arc::new(AtomicU64::new(100));
    let forward = Arc::new(AtomicBool::new(false));
    let x = Arc::new(Mutex::new(0.0_f64));
    let (set_fwd, clear_fwd) = (forward.clone(), forward.clone());
    let b = MockBridge::builder()
        .responder("input.set", move |p| {
            if names_contain(&p["press"], "forward") {
                set_fwd.store(true, Ordering::SeqCst);
            }
            if names_contain(&p["release"], "forward") {
                set_fwd.store(false, Ordering::SeqCst);
            }
            json!({"applied": true})
        })
        .responder("input.clear", move |_| {
            clear_fwd.store(false, Ordering::SeqCst);
            json!({"cleared": true})
        })
        .responder("input.status", |_| json!({"angles": null}))
        .responder("game.local", move |_| {
            let n = tick.fetch_add(8, Ordering::SeqCst) + 8;
            if forward.load(Ordering::SeqCst) {
                *x.lock().unwrap() += 10.0;
            }
            let xv = *x.lock().unwrap();
            json!({
                "tick": n, "inGame": true, "map": "probe", "playerSlot": 0,
                "viewAngles": [0.0, 90.0, 0.0],
                "player": {"alive": xv <= die_at, "origin": [xv, 0.0, 0.0], "velocity": [0.0, 0.0, 0.0], "activeWeapon": null}
            })
        });
    customize(b)
}

fn input_calls(mb: &MockBridge) -> Vec<String> {
    mb.requests().into_iter().map(|r| r.method).filter(|m| m.starts_with("input.")).collect()
}

#[tokio::test]
async fn play_test_runs_every_step_kind_and_keeps_the_key_held_between_steps() {
    let tree = Tree::new("all");
    let mb = engine(f64::INFINITY, |b| b).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    // the engine prints the line while the run waits for it
    let writer = {
        let path = tree.console();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
            writeln!(f, "10/26 12:00:00 [ti] package planted at the pad").unwrap();
        })
    };
    let out = call(
        &server,
        "play_test",
        json!({"steps": [
            {"kind": "input", "label": "go", "steps": [{"press": ["forward"], "angles": [0, 90, 0], "holdTicks": 8}]},
            {"kind": "wait_ms", "ms": 30},
            {"kind": "wait_state", "path": "player.origin[0]", "gt": 200, "timeoutMs": 5000, "pollMs": 20},
            {"kind": "wait_log", "pattern": "package (planted|defused)", "since": "start", "timeoutMs": 5000},
            {"kind": "expect_state", "path": "player.alive", "eq": true}
        ]}),
    )
    .await
    .unwrap();
    writer.await.unwrap();
    assert_eq!(out["passed"], json!(true), "{out}");
    assert_eq!(out["endedBy"], json!("complete"));
    assert_eq!(out["stepsRun"], json!(5));
    let kinds: Vec<&str> = out["steps"].as_array().unwrap().iter().map(|s| s["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["input", "wait_ms", "wait_state", "wait_log", "expect_state"]);
    assert_eq!(out["steps"][0]["label"], json!("go"));
    assert!(out["steps"][3]["result"]["line"].as_str().unwrap().contains("package planted"), "{out}");
    assert!(out["steps"][2]["result"]["got"].as_f64().unwrap() > 200.0);
    // the key stayed held across the later waits (x kept growing) and was cleared once, at the end
    assert_eq!(input_calls(&mb), ["input.set", "input.clear"]);
    assert_eq!(out["inputCleared"], json!(true));
}

#[tokio::test]
async fn keep_input_leaves_the_keys_held_after_a_pass() {
    let tree = Tree::new("keep");
    let mb = engine(f64::INFINITY, |b| b).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(
        &server,
        "play_test",
        json!({"keepInput": true, "steps": [{"kind": "input", "steps": [{"press": ["forward"], "holdTicks": 8}]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["passed"], json!(true));
    assert_eq!(out["inputCleared"], json!(false));
    assert_eq!(input_calls(&mb), ["input.set"]);
}

#[tokio::test]
async fn a_failed_wait_ends_the_run_and_clears_the_input() {
    let tree = Tree::new("fail");
    let mb = engine(f64::INFINITY, |b| b).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(
        &server,
        "play_test",
        json!({"steps": [
            {"kind": "input", "steps": [{"press": ["forward"], "holdTicks": 8}]},
            {"kind": "wait_state", "path": "player.alive", "eq": false, "timeoutMs": 300, "pollMs": 20},
            {"kind": "expect_state", "path": "player.alive", "eq": true}
        ]}),
    )
    .await
    .unwrap();
    assert_eq!(out["passed"], json!(false), "{out}");
    assert_eq!(out["endedBy"], json!("failed"));
    assert_eq!(out["failedStep"], json!(1));
    assert_eq!(out["stepsRun"], json!(2), "the step after the failure did not run");
    assert_eq!(out["steps"][1]["ok"], json!(false));
    assert!(out["steps"][1]["error"]["message"].as_str().unwrap().contains("player.alive"));
    assert_eq!(out["inputCleared"], json!(true));
    assert_eq!(input_calls(&mb).last().map(String::as_str), Some("input.clear"));
}

#[tokio::test]
async fn a_failed_expectation_reports_what_it_saw() {
    let tree = Tree::new("expect");
    let mb = engine(f64::INFINITY, |b| b).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(&server, "play_test", json!({"steps": [{"kind": "expect_state", "path": "player.origin[0]", "gt": 1000}]}))
        .await
        .unwrap();
    assert_eq!(out["passed"], json!(false));
    assert_eq!(out["steps"][0]["error"]["got"], json!(0.0));
    assert_eq!(out["steps"][0]["error"]["op"], json!("gt"));
    // nothing sent input, so nothing was cleared
    assert_eq!(out["inputCleared"], json!(false));
    assert!(input_calls(&mb).is_empty());
}

#[tokio::test]
async fn a_step_that_overruns_the_call_budget_ends_the_run_with_budget() {
    let tree = Tree::new("budget");
    let mb = engine(f64::INFINITY, |b| b).start().await.unwrap();
    let server = build(&tree, &mb, 6_000);
    let out = call(
        &server,
        "play_test",
        json!({"steps": [
            {"kind": "input", "steps": [{"press": ["forward"], "holdTicks": 4}]},
            {"kind": "wait_ms", "ms": 60000},
            {"kind": "expect_state", "path": "player.alive", "eq": true}
        ]}),
    )
    .await
    .unwrap();
    assert_eq!(out["passed"], json!(false), "{out}");
    assert_eq!(out["endedBy"], json!("budget"));
    assert_eq!(out["failedStep"], json!(1));
    assert_eq!(out["stepsRun"], json!(2));
    assert_eq!(out["inputCleared"], json!(true));

    // a wait whose timeout the budget cuts short ends the same way, after waiting what was left
    let started = std::time::Instant::now();
    let out = call(
        &server,
        "play_test",
        json!({"steps": [{"kind": "wait_state", "path": "player.origin[0]", "gt": 99999, "timeoutMs": 100000, "pollMs": 50}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["endedBy"], json!("budget"), "{out}");
    assert!(started.elapsed() < std::time::Duration::from_secs(6));
}

#[tokio::test]
async fn an_input_sequence_that_cannot_fit_the_budget_is_a_budget_end_and_sends_nothing() {
    let tree = Tree::new("seqbudget");
    let mb = engine(f64::INFINITY, |b| b).start().await.unwrap();
    let server = build(&tree, &mb, 3_000);
    let out = call(
        &server,
        "play_test",
        json!({"steps": [{"kind": "input", "steps": [{"press": ["forward"], "holdTicks": 640}]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["endedBy"], json!("budget"), "{out}");
    assert!(input_calls(&mb).is_empty(), "{:?}", input_calls(&mb));
}

#[tokio::test]
async fn wait_log_since_step_ignores_older_lines_and_since_start_sees_them() {
    let tree = Tree::new("log");
    let mb = engine(f64::INFINITY, |b| b).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let writer = {
        let path = tree.console();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
            writeln!(f, "early line").unwrap();
        })
    };
    let out = call(
        &server,
        "play_test",
        json!({"steps": [
            {"kind": "wait_ms", "ms": 500},
            {"kind": "wait_log", "pattern": "early line", "since": "start", "timeoutMs": 500},
            {"kind": "wait_log", "pattern": "early line", "timeoutMs": 200}
        ]}),
    )
    .await
    .unwrap();
    writer.await.unwrap();
    assert_eq!(out["steps"][1]["ok"], json!(true), "{out}");
    assert_eq!(out["steps"][2]["ok"], json!(false), "{out}");
    assert_eq!(out["endedBy"], json!("failed"));
    assert_eq!(out["steps"][2]["error"]["consoleLogFound"], json!(true));
}

#[tokio::test]
async fn no_map_running_is_no_map_loaded_with_run_map_and_sends_no_input() {
    let tree = Tree::new("nomap");
    let mb = engine(f64::INFINITY, |b| b.error("game.local", -32024, "no local player", None))
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb, 30_000);
    for (tool, args) in [
        ("play_test", json!({"steps": [{"kind": "input", "steps": [{"press": ["forward"]}]}]})),
        ("input_sequence", json!({"steps": [{"press": ["forward"]}]})),
        ("game_state", json!({})),
    ] {
        let e = call(&server, tool, args).await.unwrap_err();
        assert_eq!(e.name, "NoMapLoaded", "{tool}: {e:?}");
        assert_eq!(e.data["useInstead"]["tool"], json!("run_map"), "{tool}");
    }
    assert!(input_calls(&mb).is_empty());
}

#[tokio::test]
async fn bad_steps_are_refused_before_anything_is_sent() {
    let tree = Tree::new("invalid");
    let mb = engine(f64::INFINITY, |b| b).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    for bad in [
        json!({"steps": []}),
        json!({"steps": [{"kind": "dance"}]}),
        json!({"steps": [{"kind": "wait_ms"}]}),
        json!({"steps": [{"kind": "wait_ms", "ms": 5, "path": "x"}]}),
        json!({"steps": [{"kind": "wait_state", "path": "player.alive"}]}),
        json!({"steps": [{"kind": "wait_state", "path": "a", "eq": 1, "gt": 0}]}),
        json!({"steps": [{"kind": "wait_log", "pattern": "("}]}),
        json!({"steps": [{"kind": "wait_log", "pattern": "x", "since": "never"}]}),
        json!({"steps": [{"kind": "expect_state", "path": "a", "eq": 1, "timeoutMs": 5}]}),
        json!({"steps": [{"kind": "input", "steps": [{"press": ["forward"], "holdTicks": 0}]}]}),
        json!({"steps": [{"kind": "input"}]}),
        json!({"steps": [{"kind": "expect_state", "path": "a", "eq": 1}], "bogus": 1}),
    ] {
        let e = call(&server, "play_test", bad.clone()).await.unwrap_err();
        assert_eq!(e.name, "InvalidParams", "{bad}: {e:?}");
    }
    assert!(mb.requests().iter().all(|r| !r.method.starts_with("input.") && r.method != "game.local"));
}

#[tokio::test]
async fn an_input_set_interleaves_with_a_sampling_run() {
    let tree = Tree::new("interleave");
    let mb = engine(f64::INFINITY, |b| b).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    // 40 samples 25 ms apart (about a second); the input_set lands mid-run
    let sampling = call(&server, "game_state", json!({"samples": 40, "intervalMs": 25}));
    let steer = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        call(&server, "input_set", json!({"press": ["forward"]})).await
    };
    let (many, set) = tokio::join!(sampling, steer);
    assert_eq!(many.unwrap()["count"], json!(40));
    set.unwrap();
    let methods: Vec<String> = mb.requests().into_iter().map(|r| r.method).collect();
    let set_at = methods.iter().position(|m| m == "input.set").expect("input.set sent");
    let last_read = methods.iter().rposition(|m| m == "game.local").unwrap();
    assert!(set_at < last_read, "input.set waited for the whole run: {methods:?}");
}

#[tokio::test]
async fn game_state_reads_once_samples_and_game_local_is_its_alias() {
    let tree = Tree::new("state");
    let mb = engine(f64::INFINITY, |b| b).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let one = call(&server, "game_state", json!({})).await.unwrap();
    assert_eq!(one["inGame"], json!(true));
    assert!(one["source"].as_str().unwrap().contains("game.local"));
    let many = call(&server, "game_state", json!({"samples": 5, "intervalMs": 0})).await.unwrap();
    assert_eq!(many["count"], json!(5));
    assert!(many["originStepStats"].is_object(), "{many}");
    assert!(call(&server, "game_state", json!({"bogus": 1})).await.is_err());
    assert!(call(&server, "game_state", json!({"samples": 99999})).await.is_err());
    // the old name still works and says so
    let old = call(&server, "game_local", json!({"samples": 3, "intervalMs": 0})).await.unwrap();
    assert_eq!(old["count"], json!(3));
    assert_eq!(old["deprecated"], json!({"alias": "game_local", "use": "game_state"}));
    let r = server.registry();
    assert!(r.is_alias("game_local"));
    let names = r.names();
    assert!(names.iter().position(|n| *n == "game_state") < names.iter().position(|n| *n == "game_local"));
}

#[tokio::test]
async fn input_sequence_runs_steps_then_clears() {
    let tree = Tree::new("seq");
    let mb = engine(f64::INFINITY, |b| b).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(
        &server,
        "input_sequence",
        json!({"steps": [{"press": ["forward"], "holdTicks": 16, "label": "walk"}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["inputCleared"], json!(true), "{out}");
    assert!(out["totals"]["distance"].as_f64().unwrap() > 10.0);
    assert_eq!(out["steps"][0]["label"], json!("walk"));
    assert_eq!(input_calls(&mb), ["input.set", "input.clear"]);
}

/// `game.entities` mock: a func_movelinear whose x grows 400 per read (so "origin[0] > 1500" is
/// reached on the 5th read); anything else matches nothing.
fn mover(b: MockBuilder) -> MockBuilder {
    let x = Arc::new(Mutex::new(0.0_f64));
    b.responder("game.entities", move |p| {
        if p["class"] != json!("func_movelinear") {
            return json!({"entities": [], "truncated": false});
        }
        let mut x = x.lock().unwrap();
        *x += 400.0;
        let ent = |i: u64, ox: f64| json!({"entindex": 40 + i, "class": "func_movelinear", "name": format!("m{i}"), "origin": [ox, 0.0, 8.0], "angles": [0.0, 0.0, 0.0], "health": 0, "toggleState": null});
        let max = p["max"].as_u64().unwrap_or(64);
        let all = [ent(0, *x), ent(1, -5.0)];
        json!({"entities": all.iter().take(max as usize).collect::<Vec<_>>(), "truncated": false})
    })
}

#[tokio::test]
async fn entity_find_reads_the_running_game_and_is_a_play_tool() {
    let tree = Tree::new("entfind");
    let mb = engine(f64::INFINITY, mover).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(&server, "entity_find", json!({"class": "func_movelinear"})).await.unwrap();
    assert_eq!(out["entities"][0]["origin"][0], json!(400.0), "{out}");
    assert!(out["source"].as_str().unwrap().contains("game.entities"));
    assert!(call(&server, "entity_find", json!({})).await.is_err());
    assert!(call(&server, "entity_find", json!({"class": "x", "bogus": 1})).await.is_err());
    let r = server.registry();
    assert!(hammer_mcp::state::ADDON_EXEMPT_TOOLS.contains(&"entity_find"));
    assert!(r.spec("entity_find").unwrap().description.contains("Not for: the Hammer document (find_entities)"));
    assert!(r.spec("find_entities").unwrap().description.contains("entity_find"));
}

#[tokio::test]
async fn entity_find_with_no_map_is_no_map_loaded() {
    let tree = Tree::new("entnomap");
    let mb = engine(f64::INFINITY, |b| b.error("game.entities", -32024, "no local player", None))
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb, 30_000);
    let e = call(&server, "entity_find", json!({"class": "func_movelinear"})).await.unwrap_err();
    assert_eq!(e.name, "NoMapLoaded", "{e:?}");
    assert_eq!(e.data["useInstead"]["tool"], json!("run_map"));
}

#[tokio::test]
async fn wait_entity_and_expect_entity_check_a_game_entity() {
    let tree = Tree::new("entsteps");
    let mb = engine(f64::INFINITY, mover).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(
        &server,
        "play_test",
        json!({"steps": [
            {"kind": "wait_entity", "class": "func_movelinear", "path": "origin[0]", "gt": 1500, "timeoutMs": 5000, "pollMs": 20},
            {"kind": "expect_entity", "class": "func_movelinear", "index": 1, "path": "origin[0]", "lt": 0},
            {"kind": "expect_entity", "class": "func_movelinear", "path": "name", "eq": "m0"}
        ]}),
    )
    .await
    .unwrap();
    assert_eq!(out["passed"], json!(true), "{out}");
    assert!(out["steps"][0]["result"]["got"].as_f64().unwrap() > 1500.0, "{out}");
    assert_eq!(out["steps"][0]["result"]["entity"]["class"], json!("func_movelinear"));
    assert_eq!(out["steps"][1]["result"]["entity"]["entindex"], json!(41));
}

#[tokio::test]
async fn wait_entity_times_out_and_a_missing_entity_fails_expect_entity() {
    let tree = Tree::new("entfail");
    let mb = engine(f64::INFINITY, mover).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(
        &server,
        "play_test",
        json!({"steps": [
            {"kind": "wait_entity", "class": "func_door", "path": "origin[0]", "gt": 1, "timeoutMs": 150, "pollMs": 20}
        ]}),
    )
    .await
    .unwrap();
    assert_eq!(out["endedBy"], json!("failed"), "{out}");
    assert!(out["steps"][0]["error"]["message"].as_str().unwrap().contains("no entity matched"), "{out}");
    let out = call(
        &server,
        "play_test",
        json!({"steps": [{"kind": "expect_entity", "name": "nothing", "path": "health", "eq": 0}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["passed"], json!(false));
    assert!(out["steps"][0]["error"]["message"].as_str().unwrap().contains("no entity"), "{out}");
    for bad in [
        json!({"steps": [{"kind": "expect_entity", "path": "health", "eq": 0}]}),
        json!({"steps": [{"kind": "expect_entity", "class": "a b", "path": "health", "eq": 0}]}),
        json!({"steps": [{"kind": "expect_entity", "class": "a", "index": 999, "path": "health", "eq": 0}]}),
        json!({"steps": [{"kind": "expect_entity", "class": "a", "path": "health", "eq": 0, "timeoutMs": 5}]}),
    ] {
        let e = call(&server, "play_test", bad.clone()).await.unwrap_err();
        assert_eq!(e.name, "InvalidParams", "{bad}: {e:?}");
    }
}

#[tokio::test]
async fn an_input_step_waits_out_the_freeze_period_and_says_so() {
    let tree = Tree::new("freeze");
    let reads = Arc::new(AtomicU64::new(0));
    let r = reads.clone();
    let mb = engine(f64::INFINITY, move |b| {
        b.responder("game.rules", move |_| {
            // frozen for the first 4 reads, then the round is live
            let n = r.fetch_add(1, Ordering::SeqCst);
            json!({"inGame": true, "freeze": n < 4, "teamIntro": false, "gameRestart": false, "warmup": false})
        })
    })
    .start()
    .await
    .unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(
        &server,
        "play_test",
        json!({"steps": [{"kind": "input", "steps": [{"press": ["forward"], "holdTicks": 16}]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["passed"], json!(true), "{out}");
    let res = &out["steps"][0]["result"];
    assert_eq!(res["movementLock"]["phase"], json!("freeze period"), "{out}");
    assert_eq!(res["movementLock"]["stillLocked"], json!(false));
    assert!(res["note"].as_str().unwrap().contains("waited"), "{res}");
    // input.set came only after the rules read said the freeze was over
    let methods: Vec<String> = mb.requests().into_iter().map(|r| r.method).collect();
    let first_set = methods.iter().position(|m| m == "input.set").unwrap();
    assert!(methods[..first_set].iter().filter(|m| *m == "game.rules").count() >= 5, "{methods:?}");
}

#[tokio::test]
async fn a_move_step_that_moves_nothing_gets_a_note_naming_the_reason() {
    let tree = Tree::new("stuck");
    // input.set is accepted but the player never moves;
    let mb = engine(f64::INFINITY, |b| {
        b.responder("input.set", |_| json!({"applied": true}))
            .responder("game.rules", |_| json!({"inGame": true, "freeze": false, "teamIntro": false, "gameRestart": false}))
    })
    .start()
    .await
    .unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(
        &server,
        "play_test",
        json!({"steps": [{"kind": "input", "steps": [{"press": ["forward"], "holdTicks": 16}]}]}),
    )
    .await
    .unwrap();
    let res = &out["steps"][0]["result"];
    assert!(res.get("movementLock").is_none(), "{res}");
    assert!(res["note"].as_str().unwrap().contains("moved 0.0 units"), "{res}");
}

#[tokio::test]
async fn game_state_hints_the_fix_for_a_player_on_team_0_or_dead() {
    let tree = Tree::new("hint");
    let team = Arc::new(AtomicU64::new(0));
    let alive = Arc::new(AtomicBool::new(false));
    let (t, a) = (team.clone(), alive.clone());
    let mb = engine(f64::INFINITY, move |b| {
        b.responder("game.local", move |_| {
            json!({"tick": 1, "inGame": true, "map": "probe", "playerSlot": 0, "viewAngles": [0, 0, 0],
                   "player": {"alive": a.load(Ordering::SeqCst), "team": t.load(Ordering::SeqCst), "origin": [0, 0, 0]}})
        })
    })
    .start()
    .await
    .unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(&server, "game_state", json!({})).await.unwrap();
    assert_eq!(out["hint"]["fix"]["args"]["command"], json!("jointeam 3 1"), "{out}");
    team.store(3, Ordering::SeqCst);
    let out = call(&server, "game_state", json!({})).await.unwrap();
    assert_eq!(out["hint"]["fix"]["args"]["command"], json!("mp_restartgame 1"), "{out}");
    alive.store(true, Ordering::SeqCst);
    let out = call(&server, "game_state", json!({})).await.unwrap();
    assert!(out.get("hint").is_none(), "{out}");
    let sampled = call(&server, "game_state", json!({"samples": 2, "intervalMs": 0})).await.unwrap();
    assert!(sampled.get("hint").is_none(), "{sampled}");
    team.store(0, Ordering::SeqCst);
    let sampled = call(&server, "game_state", json!({"samples": 2, "intervalMs": 0})).await.unwrap();
    assert!(sampled["hint"]["problem"].as_str().unwrap().contains("team 0"), "{sampled}");
}

fn locked_rules(b: MockBuilder) -> MockBuilder {
    b.responder("game.rules", |_| json!({"inGame": true, "freeze": true, "teamIntro": false, "gameRestart": false}))
}

fn rules_reads(mb: &MockBridge) -> usize {
    mb.requests().iter().filter(|r| r.method == "game.rules").count()
}

#[tokio::test]
async fn a_lock_that_never_ends_fails_the_step_without_sending_keys() {
    let tree = Tree::new("stucklock");
    let mb = engine(f64::INFINITY, locked_rules).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(
        &server,
        "play_test",
        json!({"steps": [{"kind": "input", "lockWaitMs": 400, "steps": [{"press": ["forward"], "holdTicks": 16}]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["passed"], json!(false), "{out}");
    assert_eq!(out["endedBy"], json!("failed"), "{out}");
    let err = &out["steps"][0]["error"];
    assert_eq!(err["movementLocked"], json!(true), "{err}");
    assert_eq!(err["inputSent"], json!(false));
    assert_eq!(err["movementLock"]["stillLocked"], json!(true));
    assert!(input_calls(&mb).is_empty(), "nothing was sent: {:?}", input_calls(&mb));
}

#[tokio::test]
async fn only_a_key_pressing_step_waits_for_the_lock_and_only_once_per_run() {
    let tree = Tree::new("lockonce");
    let mb = engine(f64::INFINITY, locked_rules).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    // release-only and angles-only steps do not wait at all
    let out = call(
        &server,
        "play_test",
        json!({"steps": [
            {"kind": "input", "steps": [{"release": ["forward"]}]},
            {"kind": "input", "steps": [{"angles": [0, 90, 0], "holdTicks": 1}]}
        ]}),
    )
    .await
    .unwrap();
    assert_eq!(out["passed"], json!(true), "{out}");
    assert_eq!(rules_reads(&mb), 0, "no key pressed, no lock wait");

    // lockWaitMs:0 never waits either
    let out = call(
        &server,
        "play_test",
        json!({"steps": [{"kind": "input", "lockWaitMs": 0, "steps": [{"press": ["forward"], "holdTicks": 8}]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["passed"], json!(true), "{out}");
    assert_eq!(rules_reads(&mb), 0);

    // a lock that clears: the first pressing step waits, the second does not look again
    let tree = Tree::new("lockonce2");
    let reads = Arc::new(AtomicU64::new(0));
    let r = reads.clone();
    let mb = engine(f64::INFINITY, move |b| {
        b.responder("game.rules", move |_| {
            let n = r.fetch_add(1, Ordering::SeqCst);
            json!({"inGame": true, "freeze": n < 2, "teamIntro": false, "gameRestart": false})
        })
    })
    .start()
    .await
    .unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(
        &server,
        "play_test",
        json!({"steps": [
            {"kind": "input", "steps": [{"press": ["forward"], "holdTicks": 8}]},
            {"kind": "input", "steps": [{"press": ["jump"], "holdTicks": 8}]}
        ]}),
    )
    .await
    .unwrap();
    assert_eq!(out["passed"], json!(true), "{out}");
    assert!(out["steps"][0]["result"].get("movementLock").is_some(), "{out}");
    assert!(out["steps"][1]["result"].get("movementLock").is_none(), "{out}");
    // the stuck-move note read for step 0 (if any) aside, step 1 added no wait of its own
    assert!(rules_reads(&mb) <= 4, "{}", rules_reads(&mb));
}

#[tokio::test]
async fn lock_wait_ms_is_validated_per_step() {
    let tree = Tree::new("lockarg");
    let mb = engine(f64::INFINITY, |b| b).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    for bad in [json!(-1), json!(60_001), json!("x")] {
        let e = call(
            &server,
            "play_test",
            json!({"steps": [{"kind": "input", "lockWaitMs": bad, "steps": [{"press": ["forward"], "holdTicks": 1}]}]}),
        )
        .await
        .unwrap_err();
        assert_eq!(e.name, "InvalidParams", "{bad}");
    }
    assert!(input_calls(&mb).is_empty());
}

#[tokio::test]
async fn an_entity_step_on_a_bridge_without_game_entities_is_refused_before_any_input() {
    let tree = Tree::new("noentcap");
    let mb = engine(f64::INFINITY, |b| b).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let e = call(
        &server,
        "play_test",
        json!({"steps": [
            {"kind": "input", "steps": [{"press": ["forward"], "holdTicks": 8}]},
            {"kind": "expect_entity", "class": "func_movelinear", "path": "origin[0]", "gt": 0}
        ]}),
    )
    .await
    .unwrap_err();
    assert_eq!(e.name, "NotImplemented", "{e:?}");
    assert_eq!(e.data["missing"], json!(["game.entities"]));
    assert!(input_calls(&mb).is_empty(), "nothing was sent: {:?}", input_calls(&mb));
}

#[tokio::test]
async fn game_rules_reads_the_running_game_and_is_a_play_tool() {
    let tree = Tree::new("rules");
    let mb = engine(f64::INFINITY, |b| {
        b.responder("game.rules", |_| json!({"inGame": true, "freeze": true, "roundsPlayed": 3}))
    })
    .start()
    .await
    .unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(&server, "game_rules", json!({})).await.unwrap();
    assert_eq!(out["freeze"], json!(true), "{out}");
    assert!(out["source"].as_str().unwrap().contains("game.rules"));
    assert!(call(&server, "game_rules", json!({"bogus": 1})).await.is_err());
    let r = server.registry();
    assert!(hammer_mcp::state::ADDON_EXEMPT_TOOLS.contains(&"game_rules"));
    assert_eq!(r.spec("game_rules").unwrap().mutability, cs2_mcp_common::tools::Mutability::ReadOnly);
}

#[tokio::test]
async fn game_rules_with_no_map_is_no_map_loaded() {
    let tree = Tree::new("rulesnomap");
    let mb = engine(f64::INFINITY, |b| b.error("game.rules", -32024, "no local player", None))
        .start()
        .await
        .unwrap();
    let server = build(&tree, &mb, 30_000);
    let e = call(&server, "game_rules", json!({})).await.unwrap_err();
    assert_eq!(e.name, "NoMapLoaded", "{e:?}");
    assert_eq!(e.data["useInstead"]["tool"], json!("run_map"));
}

fn frozen_for(reads: u64, freezetime: &'static str) -> impl FnOnce(MockBuilder) -> MockBuilder {
    move |b| {
        let n = Arc::new(AtomicU64::new(0));
        b.responder("game.rules", move |_| {
            let i = n.fetch_add(1, Ordering::SeqCst);
            json!({"inGame": true, "freeze": i < reads, "teamIntro": false, "gameRestart": false})
        })
        .responder("cvar.get", move |_| {
            json!({"cvars": [{"name": "mp_freezetime", "found": true, "value": freezetime}]})
        })
    }
}

#[tokio::test]
async fn the_default_lock_wait_grows_to_cover_mp_freezetime_and_reports_it() {
    let tree = Tree::new("freezecover");
    let mb = engine(f64::INFINITY, frozen_for(2, "30")).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(
        &server,
        "play_test",
        json!({"steps": [{"kind": "input", "steps": [{"press": ["forward"], "holdTicks": 8}]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["passed"], json!(true), "{out}");
    let lock = &out["steps"][0]["result"]["movementLock"];
    assert_eq!(lock["lockWaitMs"], json!(35_000), "{lock}");
    assert_eq!(lock["lockWaitSource"], json!("mp_freezetime"), "{lock}");
}

#[tokio::test]
async fn a_short_freezetime_keeps_the_default_and_a_huge_one_is_capped() {
    let tree = Tree::new("freezedefault");
    let mb = engine(f64::INFINITY, frozen_for(1, "3")).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(
        &server,
        "play_test",
        json!({"steps": [{"kind": "input", "steps": [{"press": ["forward"], "holdTicks": 8}]}]}),
    )
    .await
    .unwrap();
    let lock = &out["steps"][0]["result"]["movementLock"];
    assert_eq!((lock["lockWaitMs"].clone(), lock["lockWaitSource"].clone()), (json!(20_000), json!("default")), "{lock}");

    let tree = Tree::new("freezecap");
    let mb = engine(f64::INFINITY, frozen_for(1, "600")).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(
        &server,
        "play_test",
        json!({"steps": [{"kind": "input", "steps": [{"press": ["forward"], "holdTicks": 8}]}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["steps"][0]["result"]["movementLock"]["lockWaitMs"], json!(60_000), "{out}");
}

#[tokio::test]
async fn an_explicit_lock_wait_ms_wins_over_mp_freezetime() {
    let tree = Tree::new("freezeexplicit");
    // locked for good, freezetime 30: the explicit 400 ms is the limit and the cvar is not read
    let mb = engine(f64::INFINITY, frozen_for(u64::MAX, "30")).start().await.unwrap();
    let server = build(&tree, &mb, 30_000);
    let out = call(
        &server,
        "play_test",
        json!({"steps": [{"kind": "input", "lockWaitMs": 400, "steps": [{"press": ["forward"], "holdTicks": 8}]}]}),
    )
    .await
    .unwrap();
    let err = &out["steps"][0]["error"];
    assert_eq!(err["movementLock"]["lockWaitMs"], json!(400), "{err}");
    assert_eq!(err["movementLock"]["lockWaitSource"], json!("lockWaitMs"), "{err}");
    assert!(!mb.requests().iter().any(|r| r.method == "cvar.get"));
}
