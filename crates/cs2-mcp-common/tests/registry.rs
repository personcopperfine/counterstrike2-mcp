//! Registry policies through a toy state: capability gating, budgets, the modal gate, timeout
//! classes, handle stripping, forbidden methods.
use cs2_mcp_common::bridge::{BridgeClient, ClientOptions, RequestOpts, SharedBridge};
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{
    Budget, CallFailure, Mutability, Registry, ToolError, ToolOutput, ToolSpec, ToolState,
};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
struct St {
    bridge: SharedBridge,
}
impl ToolState for St {
    fn bridge(&self) -> &SharedBridge {
        &self.bridge
    }
}

fn state(mb: &MockBridge, deadline_ms: u64) -> St {
    St {
        bridge: BridgeClient::shared(ClientOptions {
            discovery_path: mb.discovery_path.clone(),
            deadline_ms,
            reply_grace_ms: 200,
            ..Default::default()
        }),
    }
}

fn registry() -> Registry<St> {
    let mut r = Registry::new();
    // one bridge round trip, deadline from the budget
    r.register(
        ToolSpec::new(
            "probe",
            "doc.active once",
            Mutability::ReadOnly,
            &["doc.active"],
        ),
        |s: St, c, _a| async move {
            let dl = c.budget.deadline_ms()?;
            let mut b = s.bridge.lock().await;
            let r = b
                .request("doc.active", json!({}), RequestOpts::deadline(dl))
                .await?;
            Ok(ToolOutput::from(r.result))
        },
    );
    // two round trips: the second deadline must shrink with the budget
    r.register(
        ToolSpec::new(
            "two_step",
            "doc.active twice",
            Mutability::ReadOnly,
            &["doc.active"],
        ),
        |s: St, c, _a| async move {
            let mut b = s.bridge.lock().await;
            b.request(
                "doc.active",
                json!({}),
                RequestOpts::deadline(c.budget.deadline_ms()?),
            )
            .await?;
            let r = b
                .request(
                    "doc.active",
                    json!({}),
                    RequestOpts::deadline(c.budget.deadline_ms()?),
                )
                .await?;
            Ok(ToolOutput::from(r.result))
        },
    );
    r.register(
        ToolSpec::new(
            "gated_save",
            "never dispatches",
            Mutability::Writer,
            &["map.save"],
        ),
        |_s: St, _c, _a| async move { Err(ToolError::internal("should have been gated")) },
    );
    r.register(
        ToolSpec::new("leaky", "returns handles", Mutability::ReadOnly, &[]),
        |_s: St, _c, _a| async move {
            Ok(ToolOutput::from(
                json!({"handle": "n1", "nested": [{"nodeHandle": "x", "ok": 1}], "node_handle": 2}),
            ))
        },
    );
    r.register(
        ToolSpec::new(
            "slow_then_ask",
            "sleeps then asks the budget",
            Mutability::ReadOnly,
            &[],
        ),
        |_s: St, c, _a| async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let dl = c.budget.deadline_ms()?;
            Ok(ToolOutput::from(json!({"deadlineMs": dl})))
        },
    );
    r
}

fn tool_err(f: CallFailure) -> ToolError {
    match f {
        CallFailure::Tool(e) => e,
        CallFailure::UnknownTool(n) => panic!("unexpected unknown tool {n}"),
    }
}

#[tokio::test]
async fn capability_gating_on_restricted_caps() {
    let mb = MockBridge::builder()
        .methods([
            "control.ping",
            "control.status",
            "control.capabilities",
            "control.version",
            "control.cancel",
            "control.shutdown",
            "doc.active",
        ])
        .start()
        .await
        .unwrap();
    let st = state(&mb, 5000);
    let err = tool_err(
        registry()
            .call(&st, "gated_save", json!({}), Budget::new(45000, 5000))
            .await
            .unwrap_err(),
    );
    assert_eq!(err.name, "NotImplemented");
    assert_eq!(err.code, Some(-32011));
    assert_eq!(err.data["missing"], json!(["map.save"]));
    assert!(err.retry_safe);
    assert!(mb.method_calls("map.save").is_empty());
    // the gate re-fetched capabilities once before refusing
    assert_eq!(mb.method_calls("control.capabilities").len(), 1);
}

#[tokio::test]
async fn dead_bridge_is_not_reported_as_missing_capability() {
    let st = St {
        bridge: BridgeClient::shared(ClientOptions {
            discovery_path: "/tmp/cs2mcp-no-bridge/session.json".into(),
            ..Default::default()
        }),
    };
    let err = tool_err(
        registry()
            .call(&st, "gated_save", json!({}), Budget::new(45000, 5000))
            .await
            .unwrap_err(),
    );
    assert_eq!(err.name, "BridgeNotRunning");
    assert!(err.retry_safe);
}

#[tokio::test]
async fn unknown_tool_is_a_protocol_error_not_a_tool_result() {
    let mb = MockBridge::start().await.unwrap();
    match registry()
        .call(
            &state(&mb, 5000),
            "undo.redo",
            json!({}),
            Budget::new(45000, 5000),
        )
        .await
        .unwrap_err()
    {
        CallFailure::UnknownTool(n) => assert_eq!(n, "undo.redo"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn explicit_deadline_on_every_tool_request() {
    let mb = MockBridge::start().await.unwrap();
    let st = state(&mb, 5000);
    registry()
        .call(&st, "probe", json!({}), Budget::new(45000, 5000))
        .await
        .unwrap();
    for r in mb.requests() {
        assert!(r.params["deadlineMs"].as_u64().unwrap_or(0) > 0, "{r:?}");
    }
    assert_eq!(
        mb.method_calls("doc.active")[0].params["deadlineMs"],
        json!(5000)
    );
}

#[tokio::test]
async fn tool_budget_bounds_deadlines() {
    let mb = MockBridge::builder()
        .delay("doc.active", 300)
        .start()
        .await
        .unwrap();
    let st = state(&mb, 5000);
    registry()
        .call(&st, "two_step", json!({}), Budget::new(1500, 5000))
        .await
        .unwrap();
    let calls = mb.method_calls("doc.active");
    assert_eq!(calls.len(), 2);
    let second = calls[1].params["deadlineMs"].as_u64().unwrap();
    assert!((1000..1500).contains(&second), "second deadline {second}");
}

#[tokio::test]
async fn tool_budget_exhaustion_is_retry_safe() {
    let mb = MockBridge::start().await.unwrap();
    let st = state(&mb, 5000);
    let err = tool_err(
        registry()
            .call(&st, "slow_then_ask", json!({}), Budget::new(10, 5000))
            .await
            .unwrap_err(),
    );
    assert_eq!(err.name, "ToolBudgetExceeded");
    assert!(err.retry_safe);
    assert!(mb.requests().is_empty());
}

#[tokio::test]
async fn blocked_gate_surfaces_before_dispatch() {
    let mb = MockBridge::builder()
        .main_thread_age_ms(Some(60000))
        .start()
        .await
        .unwrap();
    let st = state(&mb, 5000);
    let err = tool_err(
        registry()
            .call(&st, "probe", json!({}), Budget::new(45000, 5000))
            .await
            .unwrap_err(),
    );
    assert_eq!(err.name, "HammerBlocked");
    assert!(err.retry_safe);
    assert!(!err.outcome_unknown);
    assert!(mb.method_calls("doc.active").is_empty());
}

#[tokio::test]
async fn queue_timeout_vs_outcome_unknown_through_a_tool() {
    let mb = MockBridge::builder()
        .delay("doc.active", 900)
        .start()
        .await
        .unwrap();
    let st = state(&mb, 200);
    let err = tool_err(
        registry()
            .call(&st, "probe", json!({}), Budget::new(45000, 200))
            .await
            .unwrap_err(),
    );
    assert_eq!(err.name, "QueueTimeout");
    assert!(err.retry_safe);
    assert!(!err.outcome_unknown);
    mb.clear_delays();
    mb.add_silent("doc.active");
    let err = tool_err(
        registry()
            .call(&st, "probe", json!({}), Budget::new(45000, 200))
            .await
            .unwrap_err(),
    );
    assert_eq!(err.name, "OutcomeUnknown");
    assert!(!err.retry_safe);
    assert!(err.outcome_unknown);
}

#[tokio::test]
async fn error_mapping_per_wire_code_through_a_tool() {
    let mb = MockBridge::start().await.unwrap();
    let st = state(&mb, 5000);
    let r = registry();
    for (code, name, safe) in [
        (-32015, "UntitledDocument", true),
        (-32004, "DocumentChanged", true),
        (-32012, "PartialFailure", false),
        (-32017, "PrerequisiteFailed", true),
        (-32099, "BridgeError", false),
    ] {
        mb.add_error("doc.active", code, name, Some(json!({"failedIndex": 1})));
        let err = tool_err(
            r.call(&st, "probe", json!({}), Budget::new(45000, 5000))
                .await
                .unwrap_err(),
        );
        assert_eq!(err.name, name, "{code}");
        assert_eq!(err.code, Some(code));
        assert_eq!(err.retry_safe, safe, "{code}");
        let payload = err.to_json();
        assert_eq!(payload["data"]["failedIndex"], json!(1));
        assert_eq!(payload["data"]["method"], json!("doc.active"));
    }
}

#[tokio::test]
async fn no_raw_handles_in_outputs_or_errors() {
    let mb = MockBridge::start().await.unwrap();
    let st = state(&mb, 5000);
    let out = registry()
        .call(&st, "leaky", json!({}), Budget::new(45000, 5000))
        .await
        .unwrap();
    assert_eq!(out.json, json!({"nested": [{"ok": 1}]}));
    let e = ToolError::internal("x")
        .with_data(json!({"handle": "n1", "results": [{"nodeHandle": "y", "applied": true}]}));
    assert_eq!(e.to_json()["data"], json!({"results": [{"applied": true}]}));
}

#[test]
#[should_panic(expected = "forbidden bridge method")]
fn forbidden_methods_cannot_be_declared() {
    let mut r: Registry<St> = Registry::new();
    r.register(
        ToolSpec::new("bad", "x", Mutability::Mutating, &["undo.redo"]),
        |_s: St, _c, _a: Value| async move { Ok(ToolOutput::default()) },
    );
}

#[test]
fn registration_order_is_preserved() {
    let r = registry();
    assert_eq!(
        r.names(),
        vec!["probe", "two_step", "gated_save", "leaky", "slow_then_ask"]
    );
}

#[tokio::test]
async fn a_busy_bridge_is_a_bounded_retry_safe_bridge_busy_not_an_unbounded_queue() {
    let mb = MockBridge::start().await.unwrap();
    let st = state(&mb, 5000);
    // another call holds the bridge for the whole test
    let held = st.bridge.lock().await;
    let started = std::time::Instant::now();
    // the budget (400 ms) bounds the capability gate's wait, which is below the 10 s cap
    let err = tool_err(
        registry()
            .call(&st, "probe", json!({}), Budget::new(400, 5000))
            .await
            .unwrap_err(),
    );
    assert_eq!(err.name, "BridgeBusy", "{err:?}");
    assert!(err.retry_safe, "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    drop(held);
    // free again: the same call goes through
    registry()
        .call(&st, "probe", json!({}), Budget::new(45000, 5000))
        .await
        .unwrap();
}

#[tokio::test]
async fn unknown_argument_names_are_refused_with_a_suggestion() {
    let mut r: Registry<St> = Registry::new();
    r.register(
        ToolSpec::new("find", "filters by class", Mutability::ReadOnly, &[]).with_schema(json!({
            "type": "object",
            "properties": {"class": {"type": "string"}, "limit": {"type": "integer"}},
            "oneOf": [{"properties": {"path": {"type": "string"}}}]
        })),
        |_s: St, _c, a| async move { Ok(ToolOutput::from(a)) },
    );
    r.register(
        ToolSpec::new("open", "any arguments", Mutability::ReadOnly, &[]),
        |_s: St, _c, a| async move { Ok(ToolOutput::from(a)) },
    );
    let st = St {
        bridge: BridgeClient::shared(ClientOptions::default()),
    };
    let budget = || Budget::new(45000, 5000);
    let err = tool_err(
        r.call(&st, "find", json!({"classname": "point_script"}), budget())
            .await
            .unwrap_err(),
    );
    assert_eq!(err.name, "InvalidParams");
    assert_eq!(err.data["unknownArguments"], json!(["classname"]));
    assert_eq!(err.data["didYouMean"], json!(["class"]));
    assert!(err.retry_safe);
    // declared keys, keys of a oneOf branch, and open schemas pass
    r.call(&st, "find", json!({"class": "x", "path": "maps/a.vmap"}), budget())
        .await
        .unwrap();
    r.call(&st, "open", json!({"anything": 1}), budget()).await.unwrap();
}
