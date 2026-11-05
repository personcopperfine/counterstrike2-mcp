//! Mock semantics of the six `modeldoc.*` methods (m2.11) against the real `BridgeClient`
//! (pattern of `tests/bridge_client.rs`), plus advertisement and knob-override checks.
//! Semantics are exactly the bridge method table in `target/orch/modeldoc-plan.md` §2 WP4.
use cs2_mcp_common::bridge::{BridgeClient, ClientOptions, RequestOpts};
use cs2_mcp_common::mock::{make_model_doc, MockBridge, MODELDOC_METHODS};
use serde_json::{json, Value};

fn client(mb: &MockBridge) -> BridgeClient {
    BridgeClient::new(ClientOptions {
        discovery_path: mb.discovery_path.clone(),
        ..Default::default()
    })
}

async fn call(
    mb: &MockBridge,
    method: &str,
    params: Value,
) -> Result<Value, cs2_mcp_common::bridge::BridgeError> {
    let mut c = client(mb);
    c.connect(false).await.unwrap();
    let r = c
        .request(method, params, RequestOpts::default())
        .await
        .expect("request");
    Ok(r.result)
}

async fn call_err(
    mb: &MockBridge,
    method: &str,
    params: Value,
) -> cs2_mcp_common::bridge::BridgeError {
    let mut c = client(mb);
    c.connect(false).await.unwrap();
    c.request(method, params, RequestOpts::default())
        .await
        .expect_err("expected a wire error")
}

// ---------------------------------------------------------------- advertisement

/// The six methods are advertised by hello only when `.modeldoc()` was set.
#[tokio::test]
async fn modeldoc_methods_are_not_advertised_by_default() {
    let mb = MockBridge::start().await.unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    let caps = c.capabilities().await.unwrap();
    for m in MODELDOC_METHODS {
        assert!(!caps["methods"].as_array().unwrap().contains(&json!(m)));
    }
    // the baseline hammer methods stay untouched
    assert!(caps["methods"]
        .as_array()
        .unwrap()
        .contains(&json!("doc.active")));
}

#[tokio::test]
async fn modeldoc_appends_the_six_methods_to_hello() {
    let mb = MockBridge::builder().modeldoc().start().await.unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    let caps = c.capabilities().await.unwrap();
    for m in MODELDOC_METHODS {
        assert!(
            caps["methods"].as_array().unwrap().contains(&json!(m)),
            "{m}"
        );
    }
}

// ---------------------------------------------------------------- modeldoc.active

#[tokio::test]
async fn active_reports_no_session_without_a_document() {
    let mb = MockBridge::builder().modeldoc().start().await.unwrap();
    assert_eq!(
        call(&mb, "modeldoc.active", json!({})).await.unwrap(),
        json!({"doc": null, "reason": "no_session"})
    );
}

#[tokio::test]
async fn active_reports_the_document_built_by_the_knob() {
    let doc = make_model_doc("Z:\\app\\content\\csgo_addons\\mcp_test\\models\\x.vmdl", false);
    let mb = MockBridge::builder()
        .modeldoc()
        .model_doc(Some(doc.clone()))
        .start()
        .await
        .unwrap();
    assert_eq!(call(&mb, "modeldoc.active", json!({})).await.unwrap(), json!({"doc": doc}));
}

// ---------------------------------------------------------------- modeldoc.open

#[tokio::test]
async fn open_resolves_an_addon_relative_path_like_openfile() {
    let mb = MockBridge::builder().modeldoc().start().await.unwrap();
    let path = "models/mcp_probe/glass_probe.vmdl";
    let res = call(&mb, "modeldoc.open", json!({"path": path})).await.unwrap();
    let doc = make_model_doc(
        "Z:\\app\\content\\csgo_addons\\mcp_test\\models\\mcp_probe\\glass_probe.vmdl",
        false,
    );
    assert_eq!(
        res,
        json!({"opened": true, "path": path, "active": doc})
    );
    // the state was switched: `active` serves the new doc afterwards
    let now = call(&mb, "modeldoc.active", json!({})).await.unwrap();
    assert!(now["doc"]["path"]
        .as_str()
        .unwrap()
        .ends_with("\\mcp_probe\\glass_probe.vmdl"));
    assert_eq!(now["doc"]["dirty"], json!(false));
}

#[tokio::test]
async fn open_refuses_a_missing_or_empty_path_with_32602() {
    let mb = MockBridge::builder().modeldoc().start().await.unwrap();
    for params in [json!({}), json!({"path": ""})] {
        let err = call_err(&mb, "modeldoc.open", params).await;
        assert_eq!(err.code, Some(-32602), "{err:?}");
    }
    // nothing was opened
    assert!(mb.with_state(|s| s.model_doc.is_none()));
}

// ---------------------------------------------------------------- modeldoc.reload

#[tokio::test]
async fn reload_refuses_without_a_document_with_32003() {
    let mb = MockBridge::builder().modeldoc().start().await.unwrap();
    let err = call_err(&mb, "modeldoc.reload", json!({})).await;
    assert_eq!(err.code, Some(-32003));
    assert_eq!(err.data["reason"], json!("no_session"));
}

#[tokio::test]
async fn reload_refuses_a_dirty_document_without_force() {
    let mb = MockBridge::builder()
        .modeldoc()
        .model_doc(Some(make_model_doc("Z:\\app\\content\\a.vmdl", true)))
        .start()
        .await
        .unwrap();
    let err = call_err(&mb, "modeldoc.reload", json!({})).await;
    assert_eq!(err.code, Some(-32016));
    assert_eq!(
        err.data["reason"],
        json!("document is dirty; pass force:true to discard")
    );
}

#[tokio::test]
async fn reload_with_force_discards_and_clears_dirty() {
    let mb = MockBridge::builder()
        .modeldoc()
        .model_doc(Some(make_model_doc("Z:\\app\\content\\a.vmdl", true)))
        .start()
        .await
        .unwrap();
    let res = call(&mb, "modeldoc.reload", json!({"force": true}))
        .await
        .unwrap();
    assert_eq!(
        res,
        json!({"reloaded": true, "path": "Z:\\app\\content\\a.vmdl", "discardedChanges": true})
    );
    assert_eq!(
        mb.with_state(|s| s.model_doc.as_ref().unwrap()["dirty"].clone()),
        json!(false)
    );
}

#[tokio::test]
async fn reload_of_a_clean_document_discards_nothing() {
    let mb = MockBridge::builder()
        .modeldoc()
        .model_doc(Some(make_model_doc("Z:\\app\\content\\a.vmdl", false)))
        .start()
        .await
        .unwrap();
    let res = call(&mb, "modeldoc.reload", json!({})).await.unwrap();
    assert_eq!(
        res,
        json!({"reloaded": true, "path": "Z:\\app\\content\\a.vmdl", "discardedChanges": false})
    );
}

// ---------------------------------------------------------------- modeldoc.save

#[tokio::test]
async fn save_refuses_without_a_document_with_32003() {
    let mb = MockBridge::builder().modeldoc().start().await.unwrap();
    let err = call_err(&mb, "modeldoc.save", json!({})).await;
    assert_eq!(err.code, Some(-32003));
}

#[tokio::test]
async fn save_refuses_a_new_file_with_32015() {
    let mut doc = make_model_doc("Z:\\app\\content\\new.vmdl", false);
    doc["isNewFile"] = json!(true);
    let mb = MockBridge::builder()
        .modeldoc()
        .model_doc(Some(doc))
        .start()
        .await
        .unwrap();
    let err = call_err(&mb, "modeldoc.save", json!({})).await;
    assert_eq!(err.code, Some(-32015));
    assert_eq!(err.kind.name(), "UntitledDocument");
}

#[tokio::test]
async fn save_reports_skipped_clean_and_clears_dirty() {
    let mb = MockBridge::builder()
        .modeldoc()
        .model_doc(Some(make_model_doc("Z:\\app\\content\\a.vmdl", false)))
        .start()
        .await
        .unwrap();
    assert_eq!(
        call(&mb, "modeldoc.save", json!({})).await.unwrap(),
        json!({"saved": true, "skippedClean": true, "path": "Z:\\app\\content\\a.vmdl"})
    );

    mb.with_state(|s| {
        if let Some(d) = s.model_doc.as_mut() {
            d["dirty"] = json!(true);
        }
    });
    assert_eq!(
        call(&mb, "modeldoc.save", json!({})).await.unwrap(),
        json!({"saved": true, "skippedClean": false, "path": "Z:\\app\\content\\a.vmdl"})
    );
    assert_eq!(
        mb.with_state(|s| s.model_doc.as_ref().unwrap()["dirty"].clone()),
        json!(false)
    );
}

// ---------------------------------------------------------------- modeldoc.compile

#[tokio::test]
async fn compile_refuses_without_a_document_with_32003() {
    let mb = MockBridge::builder().modeldoc().start().await.unwrap();
    let err = call_err(&mb, "modeldoc.compile", json!({})).await;
    assert_eq!(err.code, Some(-32003));
}

#[tokio::test]
async fn compile_refuses_without_an_asset_with_32017() {
    let mut doc = make_model_doc("Z:\\app\\content\\a.vmdl", false);
    doc["hasAsset"] = json!(false);
    let mb = MockBridge::builder()
        .modeldoc()
        .model_doc(Some(doc))
        .start()
        .await
        .unwrap();
    let err = call_err(&mb, "modeldoc.compile", json!({})).await;
    assert_eq!(err.code, Some(-32017));
    assert_eq!(err.data["reason"], json!("no_asset"));
}

#[tokio::test]
async fn compile_status_is_always_null_and_reports_saved_first() {
    // like the live bridge: the status sink is never read, the tool checks the `.vmdl_c` mtime
    for dirty in [true, false] {
        let mb = MockBridge::builder()
            .modeldoc()
            .model_doc(Some(make_model_doc("Z:\\app\\content\\a.vmdl", dirty)))
            .start()
            .await
            .unwrap();
        assert_eq!(
            call(&mb, "modeldoc.compile", json!({})).await.unwrap(),
            json!({"status": null, "failed": null, "savedFirst": dirty})
        );
        assert_eq!(
            mb.with_state(|s| s.model_doc.as_ref().unwrap()["dirty"].clone()),
            json!(false)
        );
    }
}

// ---------------------------------------------------------------- modeldoc.nodeTypes

#[tokio::test]
async fn node_types_are_indexed_in_order() {
    let mb = MockBridge::builder()
        .modeldoc()
        .node_types(vec!["RootNode".into(), "RenderMeshFile".into()])
        .start()
        .await
        .unwrap();
    assert_eq!(
        call(&mb, "modeldoc.nodeTypes", json!({})).await.unwrap(),
        json!({
            "count": 2,
            "types": [
                {"typeId": 0, "className": "RootNode"},
                {"typeId": 1, "className": "RenderMeshFile"},
            ]
        })
    );
}

// ---------------------------------------------------------------- knobs override the built-ins

/// The existing failure knobs must keep overriding the new built-ins (dispatch order).
#[tokio::test]
async fn error_responder_silent_delay_and_drop_override_the_modeldoc_builtins() {
    // `error` wins over the built-in result
    let mb = MockBridge::builder()
        .modeldoc()
        .model_doc(Some(make_model_doc("Z:\\app\\content\\a.vmdl", false)))
        .error("modeldoc.save", -32014, "AssetNotFound", None)
        .start()
        .await
        .unwrap();
    let err = call_err(&mb, "modeldoc.save", json!({})).await;
    assert_eq!(err.code, Some(-32014));

    // so does `responder`
    let mb = MockBridge::builder()
        .modeldoc()
        .responder_value("modeldoc.active", json!({"doc": "scripted"}))
        .start()
        .await
        .unwrap();
    assert_eq!(
        call(&mb, "modeldoc.active", json!({})).await.unwrap(),
        json!({"doc": "scripted"})
    );

    // `delay` past the deadline -> QueueTimeout (the built-in never runs)
    let mb = MockBridge::builder()
        .modeldoc()
        .delay("modeldoc.active", 500)
        .start()
        .await
        .unwrap();
    let mut c = client_with_deadline(&mb, 100);
    c.connect(false).await.unwrap();
    let err = c
        .request("modeldoc.active", json!({}), RequestOpts::default())
        .await
        .unwrap_err();
    assert_eq!(err.code, Some(-32008));

    // `silent` never answers (client timeout), `drop` closes before replying
    let mb = MockBridge::builder()
        .modeldoc()
        .silent("modeldoc.active")
        .start()
        .await
        .unwrap();
    let mut c = client_with_deadline(&mb, 100);
    c.connect(false).await.unwrap();
    let err = c
        .request("modeldoc.active", json!({}), RequestOpts::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, cs2_mcp_common::bridge::Kind::OutcomeUnknown);

    let mb = MockBridge::builder()
        .modeldoc()
        .drop_method("modeldoc.active")
        .start()
        .await
        .unwrap();
    let mut c = client_with_deadline(&mb, 5000);
    c.connect(false).await.unwrap();
    let err = c
        .request("modeldoc.active", json!({}), RequestOpts::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, cs2_mcp_common::bridge::Kind::ConnectionLost);
}

/// A client whose `deadlineMs` is overridden (like `tests/bridge_client.rs::client_with`).
fn client_with_deadline(mb: &MockBridge, deadline_ms: u64) -> BridgeClient {
    BridgeClient::new(ClientOptions {
        discovery_path: mb.discovery_path.clone(),
        deadline_ms,
        reply_grace_ms: 200,
        ..Default::default()
    })
}

/// Every modeldoc request the mock saw carried an explicit `deadlineMs > 0`.
#[tokio::test]
async fn every_modeldoc_request_carries_an_explicit_deadline() {
    let mb = MockBridge::builder()
        .modeldoc()
        .model_doc(Some(make_model_doc("Z:\\app\\content\\a.vmdl", false)))
        .start()
        .await
        .unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    for m in MODELDOC_METHODS {
        let _ = c
            .request(m, json!({"path": "csgo_addons/x.vmdl", "force": true}), RequestOpts::default())
            .await;
    }
    for r in mb.requests().into_iter().filter(|r| r.method.starts_with("modeldoc.")) {
        let dm = r.params["deadlineMs"].as_u64().unwrap_or(0);
        assert!(dm > 0, "{r:?}");
    }
    // and the mock state was not corrupted by the odd param shapes
    assert!(mb.with_state(|s| s.model_doc.is_some()));
}
