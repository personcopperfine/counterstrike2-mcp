//! `game_test` and `game_test_results` against the mock bridge. Validation sends nothing; a passing
//! three-step run; a failing assertion stops the later steps and still runs teardown (input_clear is
//! appended); `$ref` into an earlier result; the JUnit and stored-result readers; the call budget
//! running out returns a partial pending result; a spent test timeout is a failure.
#![allow(clippy::result_large_err)]
use client_mcp::state::ClientConfig;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

mod common;

const METHODS: [&str; 14] = [
    "control.ping",
    "control.status",
    "control.capabilities",
    "control.version",
    "doc.active",
    "game.tick",
    "game.local",
    "console.exec",
    "input.set",
    "input.clear",
    "input.status",
    "sound.drain",
    "sound.status",
    "test.checkpoint",
];

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

fn local(tick: u64, origin: [f64; 3]) -> Value {
    json!({
        "tick": tick,
        "inGame": true,
        "map": "editor/zoo/smartprop_zoo",
        "playerSlot": 0,
        "viewAngles": [0.0, 0.0, 0.0],
        "player": {"alive": true, "origin": origin, "velocity": [0.0, 0.0, 0.0], "activeWeapon": null}
    })
}

/// A player whose origin's x advances 10 per game.local read while `forward` is held by the last
/// input.set (ticks advance 8 per read).
async fn walk_bridge() -> MockBridge {
    let tick = Arc::new(AtomicU64::new(100));
    let forward = Arc::new(AtomicBool::new(false));
    let x = Arc::new(Mutex::new(-500.0_f64));
    let (set_forward, clear_forward) = (forward.clone(), forward.clone());
    MockBridge::builder()
        .methods(METHODS)
        .responder("input.set", move |params| {
            if params["press"]
                .as_array()
                .is_some_and(|a| a.iter().any(|b| b == "forward"))
            {
                set_forward.store(true, Ordering::SeqCst);
            }
            if params["release"]
                .as_array()
                .is_some_and(|a| a.iter().any(|b| b == "forward"))
            {
                set_forward.store(false, Ordering::SeqCst);
            }
            json!({"applied": true})
        })
        .responder("input.clear", move |_| {
            clear_forward.store(false, Ordering::SeqCst);
            json!({"cleared": true})
        })
        .responder_value("input.status", json!({"angles": null}))
        .responder("game.local", move |_| {
            let n = tick.fetch_add(8, Ordering::SeqCst) + 8;
            if forward.load(Ordering::SeqCst) {
                *x.lock().unwrap() += 10.0;
            }
            let xv = *x.lock().unwrap();
            local(n, [xv, 0.0, 0.0])
        })
        .start()
        .await
        .unwrap()
}

/// A player standing still at (-500, 0, 0) (the tick advances per read); console.exec is accepted.
async fn still_bridge() -> MockBridge {
    let tick = Arc::new(AtomicU64::new(100));
    MockBridge::builder()
        .methods(METHODS)
        .responder("console.exec", |_| json!({"queued": true}))
        .responder("input.clear", |_| json!({"cleared": true}))
        .responder("game.local", move |_| {
            local(tick.fetch_add(8, Ordering::SeqCst) + 8, [-500.0, 0.0, 0.0])
        })
        .start()
        .await
        .unwrap()
}

fn walk_spec() -> Value {
    json!({
        "name": "walk_test",
        "steps": [
            {"label": "before", "tool": "game_state", "expect": [
                {"path": "inGame", "op": "eq", "value": true},
                {"path": "player.alive", "op": "eq", "value": true}
            ]},
            {"label": "walk", "tool": "input_sequence",
             "args": {"steps": [{"press": ["forward"], "holdTicks": 160}]},
             "expect": [{"path": "totals.distance", "op": "ge", "value": 100}]},
            {"label": "after", "tool": "game_state", "expect": [
                {"path": "player.origin", "op": "ne", "value": {"$ref": "before.player.origin"}},
                {"path": "player.origin.0", "op": "gt", "value": {"$ref": "before.player.origin.0"}}
            ]}
        ]
    })
}

/// Records as JSON: the labels in order, with their ok flags.
fn labels_ok(out: &Value) -> Vec<(String, bool)> {
    out["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["label"].as_str().unwrap().to_string(),
                s["ok"] == json!(true),
            )
        })
        .collect()
}

// ---- validation: nothing is sent --------------------------------------------------------------

#[tokio::test]
async fn invalid_specs_are_refused_before_anything_is_sent() {
    let mb = walk_bridge().await;
    let server = server_for(&mb, &[]).await;
    let bad = [
        json!({"spec": {"name": "t", "steps": [{"tool": "game_test"}]}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "launch_session"}]}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "input_clear"}]}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "script_run"}]}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "fire_weapon", "args": {"shots": 0}}]}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "fire_weapon", "args": {"bogus": 1}}]}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "fire_weapon", "args": {"shots": "1"}}]}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "setpos"}]}, "verbose": "yes"}),
        json!({"spec": {"name": "t", "steps": [{"tool": "game_state", "expect": [{"path": "a", "op": "between", "value": 1}]}]}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "game_state", "expect": [{"path": "a..b", "op": "eq", "value": 1}]}]}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "game_state", "expect": [{"path": "a", "op": "eq"}]}]}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "game_state", "args": {"x": {"$ref": "nope.a"}}}]}}),
        json!({"spec": {"name": "t", "steps": [{"label": "a", "tool": "game_state"}, {"label": "a", "tool": "weapons"}]}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "game_state", "expect": [{"path": "a", "op": "matches", "value": "("}]}]}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "game_state", "expect": [{"path": "a", "op": "eq", "value": 1, "tolerance": 0.5}]}]}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "game_state", "expect": [{"path": "a", "op": "lt", "value": "x"}]}]}}),
        json!({"spec": {"name": "t", "expect": [{"path": "a", "op": "eq", "value": 1, "from": "result"}], "steps": [{"tool": "game_state"}]}}),
        json!({"spec": {"name": "t", "steps": []}}),
        json!({"spec": {"name": "t"}}),
        json!({"spec": {"name": "bad name", "steps": [{"tool": "game_state"}]}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "game_state"}], "bogus": 1}}),
        json!({"spec": {"name": "t", "steps": [{"tool": "game_state", "extra": 1}]}}),
        json!({"spec": {"name": "t", "timeoutMs": 0, "steps": [{"tool": "game_state"}]}}),
        json!({"name": "t", "steps": [{"tool": "game_state"}]}),
        json!({"spec": {"name": "t", "expect": [{"path": "a", "op": "eq", "value": {"$ref": "y.a"}}], "steps": [{"label": "x", "tool": "game_state"}], "teardown": [{"label": "y", "tool": "game_state"}]}}),
    ];
    for (i, args) in bad.iter().enumerate() {
        let err = call(&server, "game_test", args.clone())
            .await
            .expect_err(&format!("bad spec {i} must be refused"));
        assert_eq!(err.type_, "InvalidArguments", "spec {i}: {}", err.message);
    }
    let non_control: Vec<String> = mb
        .requests()
        .into_iter()
        .map(|r| r.method)
        .filter(|m| !m.starts_with("control."))
        .collect();
    assert!(non_control.is_empty(), "sent {non_control:?}");
}

// ---- runs --------------------------------------------------------------------------------------

#[tokio::test]
async fn a_three_step_spec_passes_with_refs_and_teardown() {
    let mb = walk_bridge().await;
    let server = server_for(&mb, &[]).await;
    let out = call(&server, "game_test", json!({"spec": walk_spec()}))
        .await
        .unwrap();
    assert_eq!(out["name"], json!("walk_test"));
    assert_eq!(out["passed"], json!(true), "{out}");
    assert_eq!(out["pending"], json!(false));
    let markers: Vec<&str> = out["markers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(markers, vec!["gt_begin_walk_test", "gt_pass_walk_test"]);
    assert_eq!(
        labels_ok(&out),
        vec![
            ("before".into(), true),
            ("walk".into(), true),
            ("after".into(), true),
            ("input_clear".into(), true),
        ]
    );
    let walk = &out["steps"][1];
    assert_eq!(walk["phase"], json!("steps"));
    assert_eq!(walk["assertions"][0]["op"], json!("ge"));
    assert_eq!(walk["assertions"][0]["wanted"], json!(100));
    // the $ref in the last step's assertion resolved to the first step's origin
    assert_eq!(out["steps"][2]["assertions"][1]["wanted"], json!(-500.0));
    assert!(out["ticks"]["start"].as_u64().is_some());
    assert!(out["ticks"]["end"].as_u64() > out["ticks"]["start"].as_u64());
    // input_sequence clears after its own steps, then the appended teardown clears again
    assert_eq!(sent(&mb, "input.clear").len(), 2);
    // the step's press and its release at the end of the hold
    assert!(!sent(&mb, "input.set").is_empty());
}

#[tokio::test]
async fn a_failing_assertion_stops_later_steps_and_runs_teardown() {
    let mb = walk_bridge().await;
    let server = server_for(&mb, &[]).await;
    let spec = json!({
        "name": "stops_here",
        "steps": [
            {"label": "before", "tool": "game_state", "expect": [{"path": "inGame", "op": "eq", "value": false}]},
            {"label": "walk", "tool": "input_sequence", "args": {"steps": [{"press": ["forward"], "holdTicks": 16}]}},
            {"label": "after", "tool": "game_state"}
        ],
        "teardown": [{"label": "look", "tool": "game_state"}]
    });
    let out = call(&server, "game_test", json!({"spec": spec}))
        .await
        .unwrap();
    assert_eq!(out["passed"], json!(false));
    assert_eq!(out["pending"], json!(false));
    let steps = out["steps"].as_array().unwrap();
    assert_eq!(steps[0]["ok"], json!(false));
    assert_eq!(steps[0]["assertions"][0]["got"], json!(true));
    assert_eq!(steps[0]["assertions"][0]["wanted"], json!(false));
    // the two later steps were not run
    assert_eq!(steps[1]["skipped"], json!(true));
    assert_eq!(steps[2]["skipped"], json!(true));
    assert!(sent(&mb, "input.set").is_empty(), "walk must not run");
    // teardown ran, input_clear first, then the spec's own teardown step
    assert_eq!(steps[3]["phase"], json!("teardown"));
    assert_eq!(steps[3]["label"], json!("input_clear"));
    assert_eq!(steps[3]["ok"], json!(true));
    assert_eq!(steps[4]["label"], json!("look"));
    assert_eq!(steps[4]["ok"], json!(true));
    assert_eq!(sent(&mb, "input.clear").len(), 1);
}

#[tokio::test]
async fn a_ref_in_step_args_is_resolved_from_an_earlier_result() {
    let mb = still_bridge().await;
    let server = server_for(&mb, &[]).await;
    let spec = json!({
        "name": "ref_args",
        "steps": [
            {"label": "snap", "tool": "game_state"},
            {"label": "place", "tool": "setpos", "args": {"origin": {"$ref": "snap.player.origin"}}}
        ]
    });
    let out = call(&server, "game_test", json!({"spec": spec}))
        .await
        .unwrap();
    assert_eq!(out["passed"], json!(true), "{out}");
    let commands: Vec<String> = sent(&mb, "console.exec")
        .into_iter()
        .map(|p| p["command"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        commands
            .iter()
            .any(|c| c.contains("-500") && c.contains("setpos")),
        "{commands:?}"
    );
}

#[tokio::test]
async fn spec_expects_read_fresh_state_and_run_before_teardown() {
    let mb = still_bridge().await;
    let server = server_for(&mb, &[]).await;
    let spec = json!({
        "name": "fresh",
        "steps": [{"label": "idle", "tool": "game_state"}],
        "expect": [
            {"path": "player.origin", "op": "approx", "value": [-500.0, 0.0, 0.0]},
            {"path": "player.alive", "op": "eq", "value": true},
            {"path": "player.health", "op": "eq", "value": 100}
        ]
    });
    let out = call(&server, "game_test", json!({"spec": spec}))
        .await
        .unwrap();
    assert_eq!(out["passed"], json!(false));
    let expect = out["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["phase"] == json!("expect"))
        .unwrap();
    assert_eq!(expect["assertions"][0]["ok"], json!(true));
    assert_eq!(expect["assertions"][1]["ok"], json!(true));
    // no health in the mock's game.local: a missing path fails with an error, not a panic
    assert_eq!(expect["assertions"][2]["ok"], json!(false));
    assert!(expect["assertions"][2]["error"]
        .as_str()
        .unwrap()
        .contains("not found"));
}

// ---- budget and timeout -----------------------------------------------------------------------

#[tokio::test]
async fn a_budget_that_cannot_fit_the_steps_is_refused_up_front() {
    let mb = walk_bridge().await;
    // the walk alone estimates ~3 s, and the cleanup reserve adds 1 s: 1100 ms cannot hold it
    let server = server_for(&mb, &["--tool-budget-ms", "1100"]).await;
    let err = call(&server, "game_test", json!({"spec": walk_spec()}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "BudgetTooSmall");
    assert!(err.data["neededMs"].as_u64().unwrap() > 1100, "{err:?}");
    assert!(non_control(&mb).is_empty(), "nothing was sent");
}

#[tokio::test]
async fn the_call_budget_running_out_during_the_steps_is_budget_too_small_and_still_tears_down() {
    // every game.local read takes 400 ms; ten 300 ms estimated steps and a 1 s cleanup reserve fit a
    // 4.1 s budget at the start, but the reads use it up partway through the steps
    let tick = Arc::new(AtomicU64::new(100));
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("input.clear", |_| json!({"cleared": true}))
        .responder("game.local", move |_| {
            local(tick.fetch_add(8, Ordering::SeqCst) + 8, [-500.0, 0.0, 0.0])
        })
        .delay("game.local", 400)
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, &["--tool-budget-ms", "4100"]).await;
    let steps: Vec<Value> = (0..10)
        .map(|i| json!({"label": format!("s{i}"), "tool": "game_state"}))
        .collect();
    let out = call(
        &server,
        "game_test",
        json!({"spec": {"name": "running_out", "steps": steps}}),
    )
    .await
    .unwrap();
    assert_eq!(out["passed"], json!(false));
    assert_eq!(out["pending"], json!(false), "{out}");
    assert!(out.get("next").is_none());
    let records = out["steps"].as_array().unwrap();
    let refused = records
        .iter()
        .find(|s| s["error"]["type"] == json!("BudgetTooSmall"))
        .unwrap_or_else(|| panic!("no BudgetTooSmall record: {out}"));
    assert_eq!(refused["error"]["data"]["neededMs"], json!(4000));
    // steps before the refusal ran; the ones after it did not
    assert!(records
        .iter()
        .any(|s| s["label"] == json!("s0") && s["ok"] == json!(true)));
    let last_step = records
        .iter()
        .rposition(|s| s["phase"] == json!("steps"))
        .unwrap();
    assert_eq!(records[last_step]["label"], json!("s9"));
    assert_eq!(records[last_step]["skipped"], json!(true));
    // cleanup ran from its reserve
    let clear = records
        .iter()
        .find(|s| s["label"] == json!("input_clear"))
        .unwrap();
    assert_eq!(clear["ok"], json!(true), "{out}");
    assert_eq!(sent(&mb, "input.clear").len(), 1);
}

#[tokio::test]
async fn a_tool_wait_timeout_under_the_cap_keeps_its_own_type_and_message() {
    let mb = still_bridge().await;
    let server = server_for(&mb, &[]).await;
    // the player never moves: game_wait gives up at its own timeoutMs, well under the 30 s cap
    let out = call(
        &server,
        "game_test",
        json!({"spec": {"name": "waits", "steps": [
            {"label": "wait", "tool": "game_wait", "args": {"until": "moved", "timeoutMs": 300}}
        ]}}),
    )
    .await
    .unwrap();
    assert_eq!(out["passed"], json!(false));
    let error = &out["steps"][0]["error"];
    assert_eq!(error["type"], json!("WaitTimeout"), "{out}");
    assert!(!error["message"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn a_spent_test_timeout_is_a_failure_not_pending() {
    let mb = walk_bridge().await;
    let server = server_for(&mb, &[]).await;
    let spec = json!({"name": "too_slow", "timeoutMs": 1, "steps": [{"label": "idle", "tool": "game_state"}]});
    let out = call(&server, "game_test", json!({"spec": spec}))
        .await
        .unwrap();
    assert_eq!(out["passed"], json!(false));
    assert_eq!(out["pending"], json!(false));
    assert!(out.get("next").is_none());
    let idle = &out["steps"][0];
    assert_eq!(idle["ok"], json!(false));
    assert_eq!(idle["error"]["type"], json!("TestTimeout"));
}

// ---- stored results -----------------------------------------------------------------------------

#[tokio::test]
async fn results_are_stored_and_readable_as_json_and_junit() {
    let mb = walk_bridge().await;
    let server = server_for(&mb, &[]).await;
    call(&server, "game_test", json!({"spec": walk_spec()}))
        .await
        .unwrap();
    let failing = json!({"name": "stops-here", "steps": [{"label": "b", "tool": "game_state", "expect": [{"path": "inGame", "op": "eq", "value": "<x & 'y'>"}]}]});
    let out = call(&server, "game_test", json!({"spec": failing}))
        .await
        .unwrap();
    assert_eq!(out["passed"], json!(false));

    let all = call(&server, "game_test_results", json!({})).await.unwrap();
    assert_eq!(all["results"].as_array().unwrap().len(), 2);

    let one = call(&server, "game_test_results", json!({"name": "walk_test"}))
        .await
        .unwrap();
    assert_eq!(one["results"][0]["passed"], json!(true));

    let missing = call(&server, "game_test_results", json!({"name": "nope"}))
        .await
        .unwrap_err();
    assert_eq!(missing.type_, "NotFound");
    let bad_format = call(&server, "game_test_results", json!({"format": "yaml"}))
        .await
        .unwrap_err();
    assert_eq!(bad_format.type_, "InvalidArguments");

    let junit = call(&server, "game_test_results", json!({"format": "junit"}))
        .await
        .unwrap();
    let xml = junit["xml"].as_str().unwrap();
    assert!(xml.starts_with("<?xml version=\"1.0\""));
    assert!(
        xml.contains("<testsuite name=\"walk_test\" tests=\"4\" failures=\"0\""),
        "{xml}"
    );
    assert!(xml.contains("<testcase classname=\"walk_test\" name=\"steps.walk\""));
    assert!(xml.contains("<testsuite name=\"stops-here\""));
    assert!(
        xml.contains("wanted &lt;x &amp; &apos;y&apos;&gt; got true"),
        "{xml}"
    );
    assert!(!xml.contains("<x & "), "raw markup leaked: {xml}");
}

#[tokio::test]
async fn game_test_is_not_a_step_tool() {
    // a spec cannot reach game_test_results or game_test itself through the allowlist
    let mb = walk_bridge().await;
    let server = server_for(&mb, &[]).await;
    let err = call(
        &server,
        "game_test",
        json!({"spec": {"name": "t", "steps": [{"tool": "game_test_results"}]}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
}

fn non_control(mb: &MockBridge) -> Vec<String> {
    mb.requests()
        .into_iter()
        .map(|r| r.method)
        .filter(|m| !m.starts_with("control."))
        .collect()
}

#[tokio::test]
async fn a_spec_that_cannot_fit_its_setup_is_budget_too_small_and_sends_nothing() {
    let mb = walk_bridge().await;
    let server = server_for(&mb, &["--tool-budget-ms", "2000"]).await;
    let spec = json!({
        "name": "heavy",
        "setup": [{"label": "walk", "tool": "input_sequence", "args": {"steps": [{"press": ["forward"], "holdTicks": 640}]}}],
        "steps": [{"tool": "game_state"}]
    });
    let err = call(&server, "game_test", json!({"spec": spec}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "BudgetTooSmall");
    assert!(err.data["neededMs"].as_u64().unwrap() > 2000, "{err:?}");
    assert!(non_control(&mb).is_empty());
}

#[tokio::test]
async fn a_result_saying_pending_or_failed_fails_the_step_unless_it_asserts_on_that_key() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("input.clear", |_| json!({"cleared": true}))
        .responder("game.local", |_| {
            let mut v = local(100, [0.0, 0.0, 0.0]);
            v["pending"] = json!(true);
            v["passed"] = json!(false);
            v
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, &[]).await;
    let bare = call(
        &server,
        "game_test",
        json!({"spec": {"name": "bare", "steps": [{"label": "a", "tool": "game_state"}]}}),
    )
    .await
    .unwrap();
    assert_eq!(bare["passed"], json!(false));
    assert_eq!(bare["steps"][0]["error"]["type"], json!("StepPending"));
    assert_eq!(
        bare["pending"],
        json!(false),
        "a step verdict is a failure, not a budget pending"
    );

    let asserted = call(
        &server,
        "game_test",
        json!({"spec": {"name": "asserted", "steps": [{"label": "a", "tool": "game_state", "expect": [
            {"path": "pending", "op": "eq", "value": true},
            {"path": "passed", "op": "eq", "value": false}
        ]}]}}),
    )
    .await
    .unwrap();
    assert_eq!(asserted["passed"], json!(true), "{asserted}");
}

#[tokio::test]
async fn input_clear_runs_first_and_a_slow_teardown_step_cannot_starve_it() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder("input.clear", |_| json!({"cleared": true}))
        .responder("game.local", |_| local(100, [-500.0, 0.0, 0.0]))
        .responder_value("sound.drain", json!({"events": [], "dropped": 0}))
        .delay("sound.drain", 2000)
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, &["--tool-budget-ms", "3500"]).await;
    let spec = json!({
        "name": "slow_tail",
        "steps": [{"label": "idle", "tool": "game_state"}],
        "teardown": [
            {"label": "slow", "tool": "sounds_drain"},
            {"label": "later", "tool": "sounds_drain"}
        ]
    });
    let out = call(&server, "game_test", json!({"spec": spec}))
        .await
        .unwrap();
    let teardown: Vec<&Value> = out["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["phase"] == json!("teardown"))
        .collect();
    assert_eq!(teardown[0]["label"], json!("input_clear"));
    assert_eq!(teardown[0]["ok"], json!(true), "{out}");
    assert_eq!(sent(&mb, "input.clear").len(), 1);
    assert_eq!(
        out["passed"],
        json!(false),
        "the later teardown step ran out of its share"
    );
}

#[tokio::test]
async fn verbose_false_drops_passing_results_and_failures_keep_theirs() {
    let mb = walk_bridge().await;
    let server = server_for(&mb, &[]).await;
    let mut spec = walk_spec();
    spec["verbose"] = json!(false);
    let out = call(&server, "game_test", json!({"spec": spec}))
        .await
        .unwrap();
    assert_eq!(out["passed"], json!(true), "{out}");
    for step in out["steps"].as_array().unwrap() {
        assert!(
            step.get("result").is_none(),
            "passing step kept a result: {step}"
        );
    }
    let failing = json!({"name": "verbose_fail", "verbose": false, "steps": [
        {"label": "before", "tool": "game_state", "expect": [{"path": "inGame", "op": "eq", "value": false}]}
    ]});
    let out = call(&server, "game_test", json!({"spec": failing}))
        .await
        .unwrap();
    assert_eq!(out["steps"][0]["result"]["inGame"], json!(true));
}

#[tokio::test]
async fn a_genuine_failure_with_the_budget_nearly_spent_is_a_failure_not_pending() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .error("game.local", -32000, "boom", None)
        .start()
        .await
        .unwrap();
    // 1400 ms: the 1 s cleanup reserve leaves a step just enough (250 ms) to start
    let server = server_for(&mb, &["--tool-budget-ms", "1400"]).await;
    let out = call(
        &server,
        "game_test",
        json!({"spec": {"name": "boom", "steps": [{"label": "idle", "tool": "game_state"}]}}),
    )
    .await
    .unwrap();
    assert_eq!(out["passed"], json!(false));
    assert_eq!(out["pending"], json!(false), "{out}");
    assert!(out.get("next").is_none());
    let idle = &out["steps"][0];
    assert_eq!(idle["ok"], json!(false));
    assert!(idle["error"].is_object(), "{idle}");
    assert_ne!(idle["error"]["type"], json!("ToolBudgetExceeded"));
}
