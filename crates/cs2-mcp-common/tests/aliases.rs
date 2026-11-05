//! Registry additions of WP2.1: aliases, `next`, `didYouMean` / `useInstead`.
use cs2_mcp_common::bridge::{BridgeClient, ClientOptions, SharedBridge};
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{
    Budget, CallFailure, Mutability, Registry, ToolError, ToolOutput, ToolSpec, ToolState,
};
use serde_json::{json, Value};

#[derive(Clone)]
struct St {
    bridge: SharedBridge,
}
impl ToolState for St {
    fn bridge(&self) -> &SharedBridge {
        &self.bridge
    }
}

fn state(mb: &MockBridge) -> St {
    St {
        bridge: BridgeClient::shared(ClientOptions {
            discovery_path: mb.discovery_path.clone(),
            deadline_ms: 5000,
            reply_grace_ms: 200,
            ..Default::default()
        }),
    }
}

fn registry() -> Registry<St> {
    let mut r = Registry::new();
    // echoes its arguments
    r.register(
        ToolSpec::new("get_entity", "echo", Mutability::ReadOnly, &[]),
        |_s: St, _c, a| async move { Ok(ToolOutput::from(json!({"args": a}))) },
    );
    r.register(
        ToolSpec::new("edit_thing", "mutates", Mutability::Mutating, &[])
            .idempotent(true)
            .then("save_map", "persist"),
        |_s: St, _c, a| async move {
            if a.get("fail").is_some() {
                return Err(ToolError::internal("nope"));
            }
            Ok(ToolOutput::from(json!({"done": true})))
        },
    );
    r.register(
        ToolSpec::new("own_next", "sets its own next", Mutability::ReadOnly, &[])
            .then("save_map", "spec-level, must lose"),
        |_s: St, _c, _a| async move {
            Ok(ToolOutput::from(json!({"x": 1})).with_next_args(
                "compile_map",
                "poll",
                json!({"jobId": "c1"}),
            ))
        },
    );
    r.register(
        ToolSpec::new("save_map", "gated writer", Mutability::Writer, &["map.save"]),
        |_s: St, _c, _a| async move { Err(ToolError::internal("should have been gated")) },
    );
    r.alias("get_entity_live", "get_entity", json!({"source": "live"}));
    r.alias("save_map_old", "save_map", json!({}));
    r.alias("save_thing_old", "get_entity", json!({}));
    r.register(
        ToolSpec::new("fails_with_alias", "fails", Mutability::ReadOnly, &[]),
        |_s: St, _c, _a| async move { Err(ToolError::internal("boom")) },
    );
    r.alias("fails_old", "fails_with_alias", json!({"mode": "x"}));
    r.alias("fails_plain_old", "fails_with_alias", json!({}));
    r
}

fn budget() -> Budget {
    Budget::new(45000, 5000)
}

#[tokio::test]
async fn an_alias_dispatches_with_fixed_args_winning_and_says_it_is_deprecated() {
    let mb = MockBridge::start().await.unwrap();
    let out = registry()
        .call(
            &state(&mb),
            "get_entity_live",
            json!({"id": 7, "source": "saved"}),
            budget(),
        )
        .await
        .unwrap();
    // the fixed argument wins over the caller's
    assert_eq!(out.json["args"], json!({"id": 7, "source": "live"}));
    assert_eq!(
        out.json["deprecated"],
        json!({"alias": "get_entity_live", "use": "get_entity", "args": {"source": "live"}})
    );
    // an alias without fixed args carries no `args` in the marker
    let plain = registry()
        .call(&state(&mb), "save_thing_old", json!({}), budget())
        .await
        .unwrap();
    assert_eq!(
        plain.json["deprecated"],
        json!({"alias": "save_thing_old", "use": "get_entity"})
    );
    // calling the new name carries no deprecation
    let direct = registry()
        .call(&state(&mb), "get_entity", json!({"id": 7}), budget())
        .await
        .unwrap();
    assert!(direct.json.get("deprecated").is_none());
}

#[tokio::test]
async fn an_alias_is_listed_cheaply_with_the_targets_policy() {
    let r = registry();
    let spec = r.spec("get_entity_live").expect("listed");
    // fixed args are named in the description: it must not claim "same arguments"
    assert_eq!(
        spec.description,
        "Deprecated: use `get_entity` {\"source\":\"live\"}."
    );
    assert_eq!(
        r.spec("save_thing_old").unwrap().description,
        "Deprecated: use `get_entity`."
    );
    assert_eq!(spec.input_schema, cs2_mcp_common::tools::open_object_schema());
    assert_eq!(spec.mutability, Mutability::ReadOnly);
    assert!(r.names().contains(&"get_entity_live"));
    assert_eq!(r.describe().len(), r.names().len());
    let wr = r.spec("save_map_old").unwrap();
    assert_eq!(wr.mutability, Mutability::Writer);
    assert_eq!(wr.bridge_methods, &["map.save"]);
}

#[tokio::test]
async fn an_alias_of_a_gated_tool_is_gated_the_same_way() {
    let mb = MockBridge::builder()
        .methods(["control.ping", "control.status", "control.capabilities", "control.version", "doc.active"])
        .start()
        .await
        .unwrap();
    let st = state(&mb);
    for name in ["save_map", "save_map_old"] {
        let CallFailure::Tool(err) = registry()
            .call(&st, name, json!({}), budget())
            .await
            .unwrap_err()
        else {
            panic!("a tool error")
        };
        assert_eq!(err.name, "NotImplemented", "{name}");
        assert_eq!(err.data["missing"], json!(["map.save"]), "{name}");
    }
    assert!(mb.method_calls("map.save").is_empty());
}

#[tokio::test]
async fn an_alias_refuses_non_object_arguments_when_it_has_fixed_args() {
    let mb = MockBridge::start().await.unwrap();
    let CallFailure::Tool(err) = registry()
        .call(&state(&mb), "get_entity_live", json!([1]), budget())
        .await
        .unwrap_err()
    else {
        panic!("a tool error")
    };
    assert_eq!(err.type_, "InvalidArguments");
}

#[test]
#[should_panic(expected = "unknown target")]
fn an_alias_of_an_unregistered_tool_is_a_programming_error() {
    let mut r = registry();
    r.alias("old", "no_such_tool", json!({}));
}

#[test]
#[should_panic(expected = "duplicate tool")]
fn an_alias_cannot_shadow_a_tool() {
    let mut r = registry();
    r.alias("get_entity", "edit_thing", json!({}));
}

#[tokio::test]
async fn next_rides_a_success_and_never_a_failure() {
    let mb = MockBridge::start().await.unwrap();
    let st = state(&mb);
    let r = registry();
    let ok = r.call(&st, "edit_thing", json!({}), budget()).await.unwrap();
    assert_eq!(ok.json["next"][0]["tool"], json!("save_map"));
    assert_eq!(ok.json["next"][0]["why"], json!("persist"));
    assert!(ok.json["next"][0].get("args").is_none());
    let CallFailure::Tool(err) = r
        .call(&st, "edit_thing", json!({"fail": 1}), budget())
        .await
        .unwrap_err()
    else {
        panic!("a tool error")
    };
    assert!(err.data.get("next").is_none());
    // a tool with no steps has no `next` key at all
    let plain = r.call(&st, "get_entity", json!({}), budget()).await.unwrap();
    assert!(plain.json.get("next").is_none());
    // the handler's own steps win over the spec's, and carry args
    let own = r.call(&st, "own_next", json!({}), budget()).await.unwrap();
    assert_eq!(own.json["next"].as_array().unwrap().len(), 1);
    assert_eq!(own.json["next"][0]["tool"], json!("compile_map"));
    assert_eq!(own.json["next"][0]["args"], json!({"jobId": "c1"}));
}

#[test]
fn unknown_tool_names_get_close_matches_only() {
    let r = registry();
    assert_eq!(r.did_you_mean("save_mpa"), vec!["save_map".to_string()]);
    assert!(r.did_you_mean("frobnicate_widgets").is_empty());
}

#[test]
fn error_builders_put_the_standard_fields_in_data_and_omit_empty_lists() {
    let e = ToolError::new("ModelNotFound", "ModelNotFound", "x")
        .did_you_mean(["a", "b"])
        .use_instead("list_models", "see the catalog", Some(json!({"limit": 5})));
    let j = e.to_json();
    assert_eq!(j["data"]["didYouMean"], json!(["a", "b"]));
    assert_eq!(
        j["data"]["useInstead"],
        json!({"tool": "list_models", "why": "see the catalog", "args": {"limit": 5}})
    );
    let none = ToolError::new("T", "T", "x").did_you_mean(Vec::<String>::new());
    assert!(none.to_json()["data"].get("didYouMean").is_none());
    let keys: Vec<&str> = j.as_object().unwrap().keys().map(String::as_str).collect();
    for k in ["type", "name", "code", "wireName", "message", "data", "retrySafe", "outcomeUnknown"] {
        assert!(keys.contains(&k), "{k}");
    }
    let _: Value = j;
}

#[tokio::test]
async fn an_error_from_an_alias_call_says_which_tool_to_use() {
    let mb = MockBridge::start().await.unwrap();
    let st = state(&mb);
    let r = registry();
    let CallFailure::Tool(err) = r.call(&st, "fails_old", json!({}), budget()).await.unwrap_err()
    else {
        panic!("a tool error")
    };
    assert_eq!(
        err.data["useInstead"],
        json!({"tool": "fails_with_alias", "why": "deprecated alias", "args": {"mode": "x"}})
    );
    assert_eq!(err.data["deprecated"]["alias"], json!("fails_old"));
    let CallFailure::Tool(err) = r.call(&st, "fails_plain_old", json!({}), budget()).await.unwrap_err()
    else {
        panic!("a tool error")
    };
    assert_eq!(err.data["useInstead"]["tool"], json!("fails_with_alias"));
    assert!(err.data["useInstead"].get("args").is_none());
    // the target called directly carries neither
    let CallFailure::Tool(err) = r.call(&st, "fails_with_alias", json!({}), budget()).await.unwrap_err()
    else {
        panic!("a tool error")
    };
    assert!(err.data.get("useInstead").is_none() && err.data.get("deprecated").is_none());
}

#[tokio::test]
async fn a_handler_can_suppress_the_registration_time_step() {
    let mut r = Registry::<St>::new();
    r.register(
        ToolSpec::new("edit", "m", Mutability::Mutating, &[]).then("save_map", "persist"),
        |_s: St, _c, a| async move {
            let out = ToolOutput::from(json!({"done": true}));
            Ok(if a.get("quiet").is_some() { out.no_next() } else { out })
        },
    );
    let mb = MockBridge::start().await.unwrap();
    let st = state(&mb);
    let loud = r.call(&st, "edit", json!({}), budget()).await.unwrap();
    assert_eq!(loud.json["next"][0]["tool"], json!("save_map"));
    let quiet = r.call(&st, "edit", json!({"quiet": 1}), budget()).await.unwrap();
    assert!(quiet.json.get("next").is_none());
}

// The check is a debug_assert: compiled out under `cargo test --release`.
#[cfg(debug_assertions)]
#[tokio::test]
#[should_panic(expected = "next names an unregistered tool")]
async fn a_strict_registry_refuses_a_step_naming_a_missing_tool() {
    let mut r = Registry::<St>::new();
    r.register(
        ToolSpec::new("edit", "m", Mutability::Mutating, &[]).then("no_such_tool", "x"),
        |_s: St, _c, _a| async move { Ok(ToolOutput::from(json!({}))) },
    );
    r.require_known_steps();
    let mb = MockBridge::start().await.unwrap();
    let _ = r.call(&state(&mb), "edit", json!({}), budget()).await;
}
