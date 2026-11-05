//! Bridge-backed tools against the mock bridge: game_tick, checkpoint, input_set (incl. turn and
//! the clear-on-error policy), input_clear. Each happy path asserts the exact params sent; each
//! validation failure asserts nothing was sent; capability gating asserts `NotImplemented`.
#![allow(clippy::result_large_err)]
use client_mcp::state::ClientConfig;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use serde_json::{json, Value};

const DEADLINE: u64 = 7000;

const CLIENT_METHODS: [&str; 12] = [
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

async fn server_for(
    mb: &MockBridge,
) -> cs2_mcp_common::mcp::McpServer<client_mcp::state::ClientState> {
    server_with_deadline(mb, DEADLINE).await
}

async fn server_with_deadline(
    mb: &MockBridge,
    deadline_ms: u64,
) -> cs2_mcp_common::mcp::McpServer<client_mcp::state::ClientState> {
    let deadline = deadline_ms.to_string();
    client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--deadline-ms",
        &deadline,
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
async fn game_tick_returns_bridge_result_as_is() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .responder_value("game.tick", json!({"tick": 1234, "frames": 56}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "game_tick", json!({})).await.unwrap();
    assert_eq!(out, json!({"tick": 1234, "frames": 56}));
    assert_eq!(
        sent(&mb, "game.tick"),
        vec![json!({"deadlineMs": DEADLINE})]
    );
}

#[tokio::test]
async fn game_tick_gated_when_bridge_lacks_it() {
    let mb = MockBridge::builder()
        .methods(["control.ping", "input.set"])
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "game_tick", json!({})).await.unwrap_err();
    assert_eq!(err.name, "NotImplemented");
    assert_eq!(err.data["missing"], json!(["game.tick"]));
    assert!(sent(&mb, "game.tick").is_empty());
}

#[tokio::test]
async fn checkpoint_sends_name_and_notes_barriers() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .responder_value("test.checkpoint", json!({"active": true}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "checkpoint", json!({"name": "vtest_release"}))
        .await
        .unwrap();
    assert_eq!(out["active"], json!(true));
    assert_eq!(out["name"], json!("vtest_release"));
    assert!(out["note"]
        .as_str()
        .unwrap()
        .contains("Test_WaitForCheckPoint"));
    assert_eq!(
        sent(&mb, "test.checkpoint"),
        vec![json!({"name": "vtest_release", "deadlineMs": DEADLINE})]
    );
}

#[tokio::test]
async fn checkpoint_inactive_reports_no_listener() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .responder_value("test.checkpoint", json!({"active": false}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "checkpoint", json!({"name": "x"}))
        .await
        .unwrap();
    assert_eq!(out["active"], json!(false));
    assert!(out["note"].as_str().unwrap().contains("no listener"));
}

#[tokio::test]
async fn checkpoint_rejects_bad_names_without_sending() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let long = "x".repeat(128);
    for args in [
        json!({}),
        json!({"name": 3}),
        json!({"name": ""}),
        json!({"name": "gt begin"}),
        json!({"name": "a;quit"}),
        json!({"name": "tab\tname"}),
        json!({"name": long}),
    ] {
        let err = call(&server, "checkpoint", args.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{args}");
    }
    assert!(sent(&mb, "test.checkpoint").is_empty());
}

#[tokio::test]
async fn checkpoint_gated_when_bridge_lacks_it() {
    let mb = MockBridge::builder()
        .methods(["control.ping", "game.tick"])
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "checkpoint", json!({"name": "x"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "NotImplemented");
    assert_eq!(err.data["missing"], json!(["test.checkpoint"]));
    assert!(sent(&mb, "test.checkpoint").is_empty());
}

#[tokio::test]
async fn input_set_sends_exact_params() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .responder_value("input.set", json!({"installed": true, "held": ["forward"], "angles": null, "frames": 10, "edges": 2, "angleTicks": 0, "buttons": []}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(
        &server,
        "input_set",
        json!({"press": ["forward"], "tap": ["jump"], "frames": 10}),
    )
    .await
    .unwrap();
    assert_eq!(out["held"], json!(["forward"]));
    assert_eq!(
        sent(&mb, "input.set"),
        vec![json!({"press": ["forward"], "tap": ["jump"], "frames": 10, "deadlineMs": DEADLINE})]
    );
    assert!(sent(&mb, "input.clear").is_empty());
}

#[tokio::test]
async fn input_set_absolute_angles_are_wire_safe_and_null_clears() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .responder_value("input.set", json!({"installed": true}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    call(&server, "input_set", json!({"angles": [-10.5, 90, 0]}))
        .await
        .unwrap();
    // yaw wraps into [-180, 180): 270 -> -90; tiny components round to zero, no exponent
    call(&server, "input_set", json!({"angles": [1e-7, 270, -1e-9]}))
        .await
        .unwrap();
    call(
        &server,
        "input_set",
        json!({"release": ["attack"], "angles": null}),
    )
    .await
    .unwrap();
    call(&server, "input_set", json!({"release": ["attack"]}))
        .await
        .unwrap();
    assert_eq!(
        sent(&mb, "input.set"),
        vec![
            json!({"angles": [-10.5, 90.0, 0.0], "deadlineMs": DEADLINE}),
            json!({"angles": [0.0, -90.0, 0.0], "deadlineMs": DEADLINE}),
            json!({"release": ["attack"], "angles": null, "deadlineMs": DEADLINE}),
            json!({"release": ["attack"], "deadlineMs": DEADLINE}),
        ]
    );
}

#[tokio::test]
async fn input_set_button_names_are_exact_or_rejected() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .responder_value("input.set", json!({"installed": true}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let max_name = "n".repeat(47);
    call(&server, "input_set", json!({"press": [max_name.clone()]}))
        .await
        .unwrap();
    assert_eq!(
        sent(&mb, "input.set"),
        vec![json!({"press": [max_name], "deadlineMs": DEADLINE})]
    );
    for bad in [
        json!({"press": ["forward "]}),
        json!({"press": [" forward"]}),
        json!({"press": ["a b"]}),
        json!({"press": ["x\"y"]}),
        json!({"press": ["back\\slash"]}),
        json!({"press": ["n".repeat(48)]}),
    ] {
        let err = call(&server, "input_set", bad.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
    }
    assert_eq!(sent(&mb, "input.set").len(), 1);
}

#[tokio::test]
async fn input_set_validation_sends_nothing() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .responder_value(
            "input.status",
            json!({"installed": true, "angles": [0, 0, 0]}),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let bad = [
        json!({}),
        json!({"press": "forward"}),
        json!({"press": [""]}),
        json!({"press": [7]}),
        json!({"press": ["forward"], "frames": 0}),
        json!({"press": ["forward"], "frames": 641}),
        json!({"tap": ["jump"], "frames": "10"}),
        json!({"press": ["forward"], "frames": 10}),
        json!({"press": ["forward"], "bogus": 1}),
        json!({"press": ["forward"], "turn": {"dyaw": 1, "extra": 2}}),
        json!({"angles": [0, 0]}),
        json!({"angles": "up"}),
        json!({"angles": [90.5, 0, 0]}),
        json!({"angles": [-89.5, 0, 0]}),
        json!({"angles": [0, 0, 180.5]}),
        json!({"angles": [0, 0, -181]}),
        json!({"angles": [0, 0, 0], "turn": {"dyaw": 1}}),
        json!({"turn": 5}),
        json!({"turn": {"dyaw": "1"}}),
    ];
    for args in bad {
        mb.reset_trace();
        let err = call(&server, "input_set", args.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{args}");
        assert!(sent(&mb, "input.status").is_empty(), "{args}");
        assert!(sent(&mb, "input.set").is_empty(), "{args}");
        assert!(sent(&mb, "input.clear").is_empty(), "{args}");
    }
}

#[tokio::test]
async fn input_set_gated_when_bridge_lacks_it() {
    let mb = MockBridge::builder()
        .methods(["control.ping", "game.tick"])
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "input_set", json!({"press": ["forward"]}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "NotImplemented");
    assert_eq!(err.data["missing"], json!(["input.set"]));
    assert!(sent(&mb, "input.set").is_empty());
}

#[tokio::test]
async fn input_set_turn_resolves_against_override_angles() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .responder_value(
            "input.status",
            json!({"installed": true, "angles": [10.0, 170.0, 0.0]}),
        )
        .responder_value(
            "input.set",
            json!({"installed": true, "angles": [5.0, -160.0, 0.0]}),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(
        &server,
        "input_set",
        json!({"press": ["forward"], "turn": {"dyaw": 30, "dpitch": -5}}),
    )
    .await
    .unwrap();
    assert_eq!(out["resolvedAngles"], json!([5.0, -160.0, 0.0]));
    assert_eq!(
        sent(&mb, "input.status"),
        vec![json!({"deadlineMs": DEADLINE})]
    );
    assert_eq!(
        sent(&mb, "input.set"),
        vec![json!({"press": ["forward"], "angles": [5.0, -160.0, 0.0], "deadlineMs": DEADLINE})]
    );
}

#[tokio::test]
async fn input_set_turn_without_override_is_invalid_and_sends_nothing_else() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .responder_value("input.status", json!({"installed": true, "angles": null}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "input_set", json!({"turn": {"dyaw": 10}}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(err.message.contains("absolute angle source"));
    assert_eq!(sent(&mb, "input.status").len(), 1);
    assert!(sent(&mb, "input.set").is_empty());
    assert!(sent(&mb, "input.clear").is_empty());
}

#[tokio::test]
async fn input_set_turn_pitch_out_of_range_sends_nothing() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .responder_value(
            "input.status",
            json!({"installed": true, "angles": [80.0, 0.0, 0.0]}),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "input_set", json!({"turn": {"dpitch": 10}}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert_eq!(sent(&mb, "input.status").len(), 1);
    assert!(sent(&mb, "input.set").is_empty());
    assert!(sent(&mb, "input.clear").is_empty());
}

#[tokio::test]
async fn input_set_turn_gated_when_status_not_advertised() {
    let mb = MockBridge::builder()
        .methods(["control.ping", "input.set", "input.clear"])
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "input_set", json!({"turn": {"dyaw": 10}}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "NotImplemented");
    assert_eq!(err.data["missing"], json!(["input.status"]));
    assert!(sent(&mb, "input.set").is_empty());
    assert!(sent(&mb, "input.status").is_empty());
}

#[tokio::test]
async fn input_status_error_sends_neither_set_nor_clear() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .error("input.status", -32017, "PrerequisiteFailed", None)
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "input_set", json!({"turn": {"dyaw": 10}}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "PrerequisiteFailed");
    assert_eq!(err.data["inputCleared"], json!(false));
    assert_eq!(sent(&mb, "input.status").len(), 1);
    assert!(sent(&mb, "input.set").is_empty());
    assert!(sent(&mb, "input.clear").is_empty());
}

#[tokio::test]
async fn input_set_invalid_params_error_does_not_clear() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .error("input.set", -32602, "InvalidParams", None)
        .responder_value("input.clear", json!({"installed": true}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "input_set", json!({"press": ["forward"]}))
        .await
        .unwrap_err();
    assert_eq!(err.code, Some(-32602));
    assert_eq!(err.data["inputCleared"], json!(false));
    assert_eq!(sent(&mb, "input.set").len(), 1);
    assert!(sent(&mb, "input.clear").is_empty());
}

#[tokio::test]
async fn input_set_retry_safe_error_does_not_clear() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .error("input.set", -32017, "PrerequisiteFailed", None)
        .responder_value("input.clear", json!({"installed": true}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "input_set", json!({"press": ["forward"]}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "PrerequisiteFailed");
    assert_eq!(err.data["inputCleared"], json!(false));
    assert!(sent(&mb, "input.clear").is_empty());
}

#[tokio::test]
async fn input_set_timeout_outcome_unknown_clears() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .silent("input.set")
        .responder_value("input.clear", json!({"installed": true}))
        .start()
        .await
        .unwrap();
    let server = server_with_deadline(&mb, 300).await;
    let err = call(&server, "input_set", json!({"press": ["forward"]}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "OutcomeUnknown", "{err:?}");
    assert_eq!(err.data["inputCleared"], json!(true));
    assert_eq!(sent(&mb, "input.clear"), vec![json!({"deadlineMs": 300})]);
}

#[tokio::test]
async fn input_set_connection_lost_clears() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .drop_method("input.set")
        .responder_value("input.clear", json!({"installed": true}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "input_set", json!({"press": ["forward"]}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "ConnectionLost", "{err:?}");
    assert!(err.outcome_unknown);
    assert_eq!(err.data["inputCleared"], json!(true));
    assert_eq!(sent(&mb, "input.clear").len(), 1);
}

#[tokio::test]
async fn input_set_failed_clear_is_reported() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .silent("input.set")
        .error("input.clear", -32017, "PrerequisiteFailed", None)
        .start()
        .await
        .unwrap();
    let server = server_with_deadline(&mb, 300).await;
    let err = call(&server, "input_set", json!({"press": ["forward"]}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "OutcomeUnknown", "{err:?}");
    assert_eq!(err.data["inputCleared"], json!("failed"));
    assert_eq!(sent(&mb, "input.clear").len(), 1);
}

#[tokio::test]
async fn input_set_clear_gated_when_bridge_lacks_input_clear() {
    let mb = MockBridge::builder()
        .methods(["control.ping", "input.set", "input.status", "game.tick"])
        .silent("input.set")
        .start()
        .await
        .unwrap();
    let server = server_with_deadline(&mb, 300).await;
    let err = call(&server, "input_set", json!({"press": ["forward"]}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "OutcomeUnknown", "{err:?}");
    assert_eq!(err.data["inputCleared"], json!(false));
    assert_eq!(sent(&mb, "input.set").len(), 1);
    assert!(sent(&mb, "input.clear").is_empty());
}

#[tokio::test]
async fn input_clear_forwards() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .responder_value(
            "input.clear",
            json!({"installed": true, "held": [], "angles": null}),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "input_clear", json!({})).await.unwrap();
    assert_eq!(out["angles"], Value::Null);
    assert_eq!(
        sent(&mb, "input.clear"),
        vec![json!({"deadlineMs": DEADLINE})]
    );
}

#[tokio::test]
async fn input_clear_answers_held_empty_and_lists_what_it_released() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .responder_value(
            "input.clear",
            json!({"installed": true, "held": ["w", "mouse1"], "angles": null}),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(&server, "input_clear", json!({})).await.unwrap();
    assert_eq!(out["held"], json!([]), "{out}");
    assert_eq!(out["releasedHeld"], json!(["w", "mouse1"]), "{out}");
}

#[tokio::test]
async fn cvar_get_redacts_secret_names_and_leaves_missing_ones_alone() {
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS.iter().copied().chain(["cvar.get"]))
        .responder_value(
            "cvar.get",
            json!({"cvars": [
                {"name": "rcon_password", "found": true, "value": "hunter2", "default": "", "flags": []},
                {"name": "mp_c4timer", "found": true, "value": "40", "default": "40", "flags": []},
                {"name": "gone_token", "found": false}
            ]}),
        )
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let out = call(
        &server,
        "cvar_get",
        json!({"names": ["rcon_password", "mp_c4timer", "gone_token"]}),
    )
    .await
    .unwrap();
    let c = &out["cvars"];
    assert_eq!(c[0]["value"], Value::Null, "{out}");
    assert_eq!(c[0]["redacted"], json!(true), "{out}");
    assert_eq!(c[0]["redactedBy"], json!("name"), "{out}");
    assert_eq!(c[1]["value"], json!("40"), "{out}");
    assert!(c[2].get("redacted").is_none(), "{out}");
    assert!(!out.to_string().contains("hunter2"));
}

#[tokio::test]
async fn input_clear_gated_when_bridge_lacks_it() {
    let mb = MockBridge::builder()
        .methods(["control.ping", "input.set"])
        .start()
        .await
        .unwrap();
    let server = server_for(&mb).await;
    let err = call(&server, "input_clear", json!({})).await.unwrap_err();
    assert_eq!(err.name, "NotImplemented");
    assert_eq!(err.data["missing"], json!(["input.clear"]));
    assert!(sent(&mb, "input.clear").is_empty());
}
