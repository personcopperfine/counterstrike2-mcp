//! Mock semantics of the twelve `pet.*` methods (m2.12 + the m2.13 second pass) against the
//! real `BridgeClient` (pattern of `tests/modeldoc_mock.rs`), plus advertisement and
//! knob-override checks. Semantics are the bridge method tables (pet plan §0.2 / plan2 §0.1,
//! §0.3; WP-M).
use cs2_mcp_common::bridge::{BridgeClient, ClientOptions, RequestOpts};
use cs2_mcp_common::mock::{
    make_pet_doc, MockBridge, MOCK_PROBE_TEXT, PET_METHODS, PET_POST_LOAD_METHODS, PET_TEXT_MAX,
};
use serde_json::{json, Value};

/// The mock has no document to load text from: a doc master of the win path like
/// [`make_pet_doc`], whose `arrays` the apply/undo/redo replies echo unchanged.
const WIN: &str = "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\probe.vpcf";
const PROBE_LEN: usize = MOCK_PROBE_TEXT.len();

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

async fn advertised(mb: &MockBridge) -> Vec<String> {
    let mut c = client(mb);
    c.connect(false).await.unwrap();
    c.capabilities().await.unwrap()["methods"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn contains(methods: &[String], m: &str) -> bool {
    methods.iter().any(|x| x == m)
}

// ---------------------------------------------------------------- advertisement

/// Negative: without `.pet()` none of the seven methods is advertised, and the wire answers
/// them with MethodNotFound (the built-ins must not leak through unadvertised).
#[tokio::test]
async fn pet_methods_are_not_advertised_without_pet() {
    let mb = MockBridge::start().await.unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    let caps = c.capabilities().await.unwrap();
    for m in PET_METHODS {
        assert!(!caps["methods"].as_array().unwrap().contains(&json!(m)), "{m}");
    }
    // the baseline hammer methods stay untouched
    assert!(caps["methods"].as_array().unwrap().contains(&json!("doc.active")));
    let err = c
        .request("pet.load", json!({}), RequestOpts::default())
        .await
        .expect_err("unadvertised method must answer MethodNotFound");
    assert_eq!(err.code, Some(-32601), "{err:?}");
}

/// With `.pet()` only `pet.load` is advertised before the load; the fresh load adds the eleven
/// `NEED_PET` methods and bumps the revision.
#[tokio::test]
async fn pet_load_adds_the_eleven() {
    assert_eq!(PET_METHODS.len(), 12, "{PET_METHODS:?}");
    assert_eq!(PET_POST_LOAD_METHODS.len(), 11, "{PET_POST_LOAD_METHODS:?}");
    let mb = MockBridge::builder().pet().start().await.unwrap();
    let before = advertised(&mb).await;
    assert!(contains(&before, "pet.load"), "{before:?}");
    for m in PET_POST_LOAD_METHODS {
        assert!(!contains(&before, m), "{m} advertised before the load");
    }

    let res = call(&mb, "pet.load", json!({})).await.unwrap();
    assert_eq!(
        res,
        json!({"loaded": true, "alreadyLoaded": false, "toolIndex": 3, "module": "verified"})
    );
    let after = advertised(&mb).await;
    for m in PET_METHODS {
        assert!(contains(&after, m), "{m} not advertised after the load");
    }
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    let caps = c.capabilities().await.unwrap();
    assert_eq!(caps["revision"], json!(1));
}

/// `pet_loaded(true)` advertises all twelve from the start (mock state: already loaded).
#[tokio::test]
async fn pet_loaded_true_advertises_all_twelve_from_the_start() {
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let methods = advertised(&mb).await;
    for m in PET_METHODS {
        assert!(contains(&methods, m), "{m}");
    }
    // already loaded: the load answers without a tool index
    let res = call(&mb, "pet.load", json!({})).await.unwrap();
    assert_eq!(
        res,
        json!({"loaded": true, "alreadyLoaded": true, "toolIndex": null, "module": "verified"})
    );
}

// ---------------------------------------------------------------- pet.load

/// Negative: the second `pet.load` reports `alreadyLoaded:true` with `toolIndex:null`.
#[tokio::test]
async fn pet_load_twice_reports_already_loaded() {
    let mb = MockBridge::builder().pet().start().await.unwrap();
    let first = call(&mb, "pet.load", json!({})).await.unwrap();
    assert_eq!(first["alreadyLoaded"], json!(false));
    assert_eq!(first["toolIndex"], json!(3));
    let second = call(&mb, "pet.load", json!({})).await.unwrap();
    assert_eq!(
        second,
        json!({"loaded": true, "alreadyLoaded": true, "toolIndex": null, "module": "verified"})
    );
}

// ---------------------------------------------------------------- pet.active

/// Negative: `pet.active` before any doc -> `{doc:null, reason:"no_session"}`.
#[tokio::test]
async fn pet_active_before_a_doc_reports_no_session() {
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    assert_eq!(
        call(&mb, "pet.active", json!({})).await.unwrap(),
        json!({"doc": null, "reason": "no_session"})
    );
}

#[tokio::test]
async fn pet_active_reports_the_document_built_by_the_knob() {
    let doc = make_pet_doc("Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\x.vpcf", false);
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(doc.clone()))
        .start()
        .await
        .unwrap();
    assert_eq!(
        call(&mb, "pet.active", json!({})).await.unwrap(),
        json!({"doc": doc})
    );
}

// ---------------------------------------------------------------- pet.open

#[tokio::test]
async fn pet_open_resolves_an_addon_relative_path_like_openfile() {
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let path = "particles/mcp_probe/probe.vpcf";
    let res = call(&mb, "pet.open", json!({"path": path})).await.unwrap();
    let doc = make_pet_doc(
        "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\mcp_probe\\probe.vpcf",
        false,
    );
    assert_eq!(res, json!({"opened": true, "path": path, "active": doc}));
    // the state was switched: `active` serves the new doc afterwards
    let now = call(&mb, "pet.active", json!({})).await.unwrap();
    assert!(now["doc"]["path"]
        .as_str()
        .unwrap()
        .ends_with("\\mcp_probe\\probe.vpcf"));
    // the request was recorded like every other arm
    assert_eq!(mb.method_calls("pet.open").len(), 1);
}

/// Negative: `pet.open` with an empty path -> `-32602`, nothing opened.
#[tokio::test]
async fn pet_open_refuses_an_empty_path_with_32602() {
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    for params in [json!({}), json!({"path": ""})] {
        let err = call_err(&mb, "pet.open", params).await;
        assert_eq!(err.code, Some(-32602), "{err:?}");
    }
    assert!(mb.with_state(|s| s.pet_doc.is_none()));
}

// ---------------------------------------------------------------- pet.reload

/// Negative: `pet.reload` without a doc -> `-32003 NoActiveDocument`.
#[tokio::test]
async fn pet_reload_without_a_doc_refuses_with_32003() {
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let err = call_err(&mb, "pet.reload", json!({})).await;
    assert_eq!(err.code, Some(-32003));
    assert_eq!(err.kind.name(), "NoDocument");
    assert_eq!(err.data["reason"], json!("no_session"));
}

/// Negative: `pet.reload` on a dirty doc without force -> `-32016 UnsavedChanges`;
/// with `force:true` it discards and clears the dirty flag.
#[tokio::test]
async fn pet_reload_refuses_a_dirty_doc_without_force() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc("Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\a.vpcf", true)))
        .start()
        .await
        .unwrap();
    let err = call_err(&mb, "pet.reload", json!({})).await;
    assert_eq!(err.code, Some(-32016));
    assert_eq!(
        err.data["reason"],
        json!("document is dirty; pass force:true to discard")
    );

    let res = call(&mb, "pet.reload", json!({"force": true})).await.unwrap();
    assert_eq!(res["reloaded"], json!(true));
    assert_eq!(res["hasRoot"], json!(true));
    assert_eq!(res["discardedChanges"], json!(true));
    assert_eq!(
        mb.with_state(|s| s.pet_doc.as_ref().unwrap()["dirty"].clone()),
        json!(false)
    );
}

/// `hasRoot` comes from the doc: a doc with `hasRoot:false` simulates the failed-load state.
#[tokio::test]
async fn pet_reload_reports_has_root_from_the_doc() {
    let mut doc = make_pet_doc("Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\a.vpcf", false);
    doc["hasRoot"] = json!(false);
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(doc))
        .start()
        .await
        .unwrap();
    let res = call(&mb, "pet.reload", json!({})).await.unwrap();
    assert_eq!(
        res,
        json!({"reloaded": true, "hasRoot": false,
               "path": "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\a.vpcf",
               "discardedChanges": false})
    );
}

// ---------------------------------------------------------------- pet.save

/// Negative: `pet.save` without a doc -> `-32003 NoActiveDocument`.
#[tokio::test]
async fn pet_save_without_a_doc_refuses_with_32003() {
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let err = call_err(&mb, "pet.save", json!({})).await;
    assert_eq!(err.code, Some(-32003));
}

/// Negative: `pet.save` on an untitled doc (isNewFile) -> `-32015 UntitledDocument`.
#[tokio::test]
async fn pet_save_refuses_a_new_file_with_32015() {
    let mut doc = make_pet_doc("Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\new.vpcf", false);
    doc["isNewFile"] = json!(true);
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(doc))
        .start()
        .await
        .unwrap();
    let err = call_err(&mb, "pet.save", json!({})).await;
    assert_eq!(err.code, Some(-32015));
    assert_eq!(err.kind.name(), "UntitledDocument");
}

#[tokio::test]
async fn pet_save_reports_skipped_clean_and_clears_dirty() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(
            "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\a.vpcf",
            false,
        )))
        .start()
        .await
        .unwrap();
    assert_eq!(
        call(&mb, "pet.save", json!({})).await.unwrap(),
        json!({"saved": true, "skippedClean": true,
               "path": "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\a.vpcf"})
    );

    mb.with_state(|s| {
        if let Some(d) = s.pet_doc.as_mut() {
            d["dirty"] = json!(true);
        }
    });
    assert_eq!(
        call(&mb, "pet.save", json!({})).await.unwrap(),
        json!({"saved": true, "skippedClean": false,
               "path": "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\a.vpcf"})
    );
    assert_eq!(
        mb.with_state(|s| s.pet_doc.as_ref().unwrap()["dirty"].clone()),
        json!(false)
    );
}

/// `force:true` re-saves a clean document (`skippedClean:false`); the call log has the params, and
/// without it the log carries no `force` key at all.
#[tokio::test]
async fn pet_save_force_saves_a_clean_document() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(
            "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\a.vpcf",
            false,
        )))
        .start()
        .await
        .unwrap();
    assert_eq!(
        call(&mb, "pet.save", json!({"force": true})).await.unwrap(),
        json!({"saved": true, "skippedClean": false,
               "path": "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\a.vpcf"})
    );
    let calls = mb.method_calls("pet.save");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].params["force"], json!(true));

    // Negative: the same clean document without force is skipped and sends no force key
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(
            "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\a.vpcf",
            false,
        )))
        .start()
        .await
        .unwrap();
    let res = call(&mb, "pet.save", json!({})).await.unwrap();
    assert_eq!(res["skippedClean"], json!(true));
    let calls = mb.method_calls("pet.save");
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].params.get("force").is_none(),
        "{:?}",
        calls[0].params
    );
}

// ---------------------------------------------------------------- pet.functionTypes

#[tokio::test]
async fn pet_function_types_lists_the_default_rows() {
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let res = call(&mb, "pet.functionTypes", json!({})).await.unwrap();
    assert_eq!(res["count"], json!(6));
    let types = res["types"].as_array().unwrap();
    // row shape exactly like 0.2 types[]
    for row in types {
        for key in ["type", "index", "name", "className", "metadata", "obsolete", "singleton"] {
            assert!(row.get(key).is_some(), "missing {key} in {row}");
        }
    }
    // the default list: the probe classes plus one obsolete operator
    let classes: Vec<&str> = types
        .iter()
        .filter_map(|r| r["className"].as_str())
        .collect();
    for c in [
        "C_OP_ContinuousEmitter",
        "C_INIT_CreateWithinSphereTransform",
        "C_OP_BasicMovement",
        "C_OP_Decay",
        "C_OP_RenderSprites",
        "C_OP_OldStyleObsolete",
    ] {
        assert!(classes.contains(&c), "{c} missing");
    }
    let obsolete = types.iter().find(|r| r["className"] == "C_OP_OldStyleObsolete").unwrap();
    assert_eq!(obsolete["obsolete"], json!(true));
    assert_eq!(obsolete["metadata"], json!(["MObsoleteParticleFunction"]));
    // both default operators sit in the type-3 array, indexed in order
    let type3: Vec<i64> = types
        .iter()
        .filter(|r| r["type"] == json!(3))
        .map(|r| r["index"].as_i64().unwrap())
        .collect();
    assert_eq!(type3, vec![0, 1, 2]);
}

#[tokio::test]
async fn pet_function_types_honours_the_override() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_types(vec![json!({
            "type": 0, "index": 0, "name": "PreEmission", "className": "C_OP_TestPre",
            "metadata": [], "obsolete": false, "singleton": null,
        })])
        .start()
        .await
        .unwrap();
    assert_eq!(
        call(&mb, "pet.functionTypes", json!({})).await.unwrap(),
        json!({"count": 1, "types": [{"type": 0, "index": 0, "name": "PreEmission",
            "className": "C_OP_TestPre", "metadata": [], "obsolete": false, "singleton": null}]})
    );
}

// ---------------------------------------------------------------- pet.schema

#[tokio::test]
async fn pet_schema_serves_the_default_schemas() {
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let emitter = call(&mb, "pet.schema", json!({"class": "C_OP_ContinuousEmitter"}))
        .await
        .unwrap();
    assert_eq!(emitter["found"], json!(true));
    assert_eq!(emitter["class"], json!("C_OP_ContinuousEmitter"));
    assert!(emitter["fields"].as_array().unwrap().iter().any(|f| f["name"] == "m_flEmitRate"));
    assert!(emitter["bases"]
        .as_array()
        .unwrap()
        .contains(&json!("CParticleFunction")));

    let root = call(&mb, "pet.schema", json!({"class": "CParticleSystemDefinition"}))
        .await
        .unwrap();
    assert_eq!(root["found"], json!(true));
    assert!(root["fields"].as_array().unwrap().iter().any(|f| f["name"] == "m_nMaxParticles"));

    let children = call(&mb, "pet.schema", json!({"class": "ParticleChildrenInfo_t"}))
        .await
        .unwrap();
    let names: Vec<&str> = children["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert!(names.contains(&"m_ChildRef") && names.contains(&"m_flDelay"), "{children}");

    let locals = call(&mb, "pet.schema", json!({"class": "ParticleNamedValueSource_t"}))
        .await
        .unwrap();
    let local_fields: Vec<&str> = locals["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert_eq!(
        local_fields,
        vec!["m_Name", "m_IsPublic", "m_ValueType", "m_DefaultConfig"]
    );
    let config = call(&mb, "pet.schema", json!({"class": "ParticleNamedValueConfiguration_t"}))
        .await
        .unwrap();
    let config_fields: Vec<&str> = config["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert_eq!(
        config_fields,
        vec![
            "m_ConfigName",
            "m_ConfigValue",
            "m_BoundValuePath",
            "m_iAttachType",
            "m_strEntityScope",
            "m_strAttachmentName"
        ]
    );

    // a builder override wins over the default
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_schema("C_OP_Decay", json!({"class": "C_OP_Decay", "found": true, "size": 1}))
        .start()
        .await
        .unwrap();
    assert_eq!(
        call(&mb, "pet.schema", json!({"class": "C_OP_Decay"})).await.unwrap(),
        json!({"class": "C_OP_Decay", "found": true, "size": 1})
    );
}

/// Negative: an unknown class -> `found:false` with null/empty everything else.
#[tokio::test]
async fn pet_schema_unknown_class_reports_found_false() {
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    assert_eq!(
        call(&mb, "pet.schema", json!({"class": "NoSuchClass_x"})).await.unwrap(),
        json!({"class": "NoSuchClass_x", "found": false, "size": null, "project": null,
               "bases": [], "fields": [], "metadata": []})
    );
}

/// Negative: a class name with anything but `[A-Za-z0-9_]` -> `-32602 InvalidParams`
/// (also over 127 chars, and a missing `class`).
#[tokio::test]
async fn pet_schema_refuses_a_bad_class_name_with_32602() {
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    for params in [
        json!({}),
        json!({"class": ""}),
        json!({"class": "C_OP_Bad-Dash"}),
        json!({"class": "C_OP_Space Name"}),
        json!({"class": "C_OP_Dot.Name"}),
        json!({"class": format!("A{}", "b".repeat(127))}),
    ] {
        let err = call_err(&mb, "pet.schema", params).await;
        assert_eq!(err.code, Some(-32602), "{err:?}");
    }
    // 127 chars exactly is fine
    let ok = call(
        &mb,
        "pet.schema",
        json!({"class": format!("A{}", "b".repeat(126))}),
    )
    .await
    .unwrap();
    assert_eq!(ok["found"], json!(false));
}

// ---------------------------------------------------------------- pet.text / pet.apply / pet.undo / pet.redo / pet.render (m2.13 §0.3)

/// `pet.text` serves the probe text (CRLF, tabs, `m_nBehaviorVersion = 12`, no final newline)
/// with the document's flags; `canUndo`/`canRedo` come from the stacks (still empty here).
#[tokio::test]
async fn pet_text_serves_the_probe_text_and_flags() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(WIN, false)))
        .start()
        .await
        .unwrap();
    assert!(MOCK_PROBE_TEXT.starts_with("<!-- kv3"), "{MOCK_PROBE_TEXT}");
    assert!(MOCK_PROBE_TEXT.ends_with('}'), "no final newline");
    assert!(MOCK_PROBE_TEXT.contains("\r\n") && MOCK_PROBE_TEXT.contains('\t'));

    let res = call(&mb, "pet.text", json!({})).await.unwrap();
    assert_eq!(
        res,
        json!({
            "text": MOCK_PROBE_TEXT,
            "bytes": PROBE_LEN,
            "path": WIN,
            "dirty": false,
            "isNewFile": false,
            "canUndo": false,
            "canRedo": false,
        })
    );
    assert!(MOCK_PROBE_TEXT.contains("m_nBehaviorVersion = 12"));
    assert_eq!(res["bytes"], res["text"].as_str().unwrap().len());
}

/// Negative: `pet.text` without a document -> `-32003 NoActiveDocument {reason:"no_session"}`.
#[tokio::test]
async fn pet_text_without_a_doc_is_32003() {
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let err = call_err(&mb, "pet.text", json!({})).await;
    assert_eq!(err.code, Some(-32003), "{err:?}");
    assert_eq!(err.kind.name(), "NoDocument");
    assert_eq!(err.data["reason"], json!("no_session"));
}

/// `pet.apply` accepts the probe text: the old text lands on the undo stack, the redo stack is
/// cleared, the document turns dirty, and the doctored text is what `pet.text` serves next.
/// A default label reads "MCP apply". `arrays` stay the knob's (the mock does not parse KV3).
#[tokio::test]
async fn pet_apply_pushes_undo_and_marks_dirty() {
    let doc = make_pet_doc(WIN, false);
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(doc.clone()))
        .start()
        .await
        .unwrap();
    let text = MOCK_PROBE_TEXT.replacen(
        "\t_class = \"CParticleSystemDefinition\"\r\n",
        "\t_class = \"CParticleSystemDefinition\"\r\n\tm_mcpTest = \"applied\"\r\n",
        1,
    );
    let res = call(&mb, "pet.apply", json!({"text": text, "label": "MCP: test"}))
        .await
        .unwrap();
    assert_eq!(
        res,
        json!({
            "applied": true,
            "label": "MCP: test",
            "bytes": text.len(),
            "dirty": true,
            "canUndo": true,
            "canRedo": false,
            "arrays": doc["arrays"],
        })
    );
    mb.with_state(|s| {
        assert_eq!(s.pet_text, text, "the doctored text is now the document");
        assert_eq!(s.pet_undo, vec![MOCK_PROBE_TEXT.to_string()]);
        assert!(s.pet_redo.is_empty());
        assert_eq!(s.pet_doc.as_ref().unwrap()["dirty"], json!(true));
    });
    let now = call(&mb, "pet.text", json!({})).await.unwrap();
    assert_eq!(now["text"], json!(text));

    // a second apply without a label uses the bridge's default label
    let res = call(&mb, "pet.apply", json!({"text": MOCK_PROBE_TEXT})).await.unwrap();
    assert_eq!(res["label"], json!("MCP apply"));
    assert_eq!(res["canUndo"], json!(true));
    mb.with_state(|s| assert_eq!(s.pet_undo.len(), 2));
}

/// Negative: `pet.apply` without `text`, with an empty or non-string `text` -> `-32602`.
#[tokio::test]
async fn pet_apply_refuses_missing_text_32602() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(WIN, false)))
        .start()
        .await
        .unwrap();
    for params in [json!({}), json!({"text": ""}), json!({"text": 5})] {
        let err = call_err(&mb, "pet.apply", params).await;
        assert_eq!(err.code, Some(-32602), "{err:?}");
        assert_eq!(err.wire_name.as_deref(), Some("InvalidParams"));
        assert_eq!(err.data["reason"], json!("text: a non-empty string is required"));
    }
    // nothing was accepted
    mb.with_state(|s| {
        assert_eq!(s.pet_text, MOCK_PROBE_TEXT);
        assert!(s.pet_undo.is_empty() && s.pet_redo.is_empty());
    });
}

/// Negative: a label outside 1..64 printable ASCII, or with a quote/backslash -> `-32602`,
/// nothing applied (the bridge's label gate).
#[tokio::test]
async fn pet_apply_refuses_bad_label_32602() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(WIN, false)))
        .start()
        .await
        .unwrap();
    let text = MOCK_PROBE_TEXT.to_string();
    for label in [json!(""), json!("x".repeat(65)), json!("a\"b"), json!("a\\b"), json!(7)] {
        let err = call_err(&mb, "pet.apply", json!({"text": text, "label": label})).await;
        assert_eq!(err.code, Some(-32602), "{label}: {err:?}");
    }
    mb.with_state(|s| assert!(s.pet_undo.is_empty()));
}

/// Negative: `text` over the 384 KB gate -> `-32602 InvalidParams
/// {reason:"text: 1..393216 bytes"}`; exactly `PET_TEXT_MAX` bytes is accepted.
#[tokio::test]
async fn pet_apply_refuses_oversize_text_32602() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(WIN, false)))
        .start()
        .await
        .unwrap();
    let over = format!("<!-- kv3 {}", "a".repeat(393208));
    assert_eq!(over.len(), PET_TEXT_MAX + 1, "{:?}", over.len());
    let err = call_err(&mb, "pet.apply", json!({"text": over})).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(err.data["reason"], json!("text: 1..393216 bytes"));
    assert!(mb.with_state(|s| s.pet_undo.is_empty()));

    // the boundary: a valid text padded to exactly PET_TEXT_MAX bytes goes through
    let padded = format!("{MOCK_PROBE_TEXT}{}", " ".repeat(PET_TEXT_MAX - PROBE_LEN));
    assert_eq!(padded.len(), PET_TEXT_MAX);
    let res = call(&mb, "pet.apply", json!({"text": padded})).await.unwrap();
    assert_eq!(res["applied"], json!(true));
    assert_eq!(res["bytes"], json!(PET_TEXT_MAX));
}

/// Negative: text without the `<!-- kv3` header -> `-32602 UnloadableFile
/// {reason:"no_kv3_header", path:"pet.apply:text"}` (the bridge's vpcf_check gate), state
/// untouched.
#[tokio::test]
async fn pet_apply_refuses_no_header_as_unloadable() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(WIN, false)))
        .start()
        .await
        .unwrap();
    let err = call_err(
        &mb,
        "pet.apply",
        json!({"text": "hello, no kv3 header\n\t_class = \"CParticleSystemDefinition\""}),
    )
    .await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(err.wire_name.as_deref(), Some("UnloadableFile"));
    assert_eq!(err.data["reason"], json!("no_kv3_header"));
    assert_eq!(err.data["path"], json!("pet.apply:text"));
    mb.with_state(|s| {
        assert_eq!(s.pet_text, MOCK_PROBE_TEXT, "the text was not replaced");
        assert!(s.pet_undo.is_empty(), "no undo step was pushed");
    });
}

/// Negative: text with the header but without the root `_class` -> `{reason:
/// "wrong_root_class"}`.
#[tokio::test]
async fn pet_apply_refuses_wrong_root_class_as_unloadable() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(WIN, false)))
        .start()
        .await
        .unwrap();
    let bad = format!("{}\r\n{{\r\n\tm_nMaxParticles = 8\r\n}}", MOCK_PROBE_TEXT.lines().next().unwrap());
    assert!(bad.starts_with("<!-- kv3"));
    let err = call_err(&mb, "pet.apply", json!({"text": bad})).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(err.wire_name.as_deref(), Some("UnloadableFile"));
    assert_eq!(err.data["reason"], json!("wrong_root_class"));
    assert_eq!(err.data["path"], json!("pet.apply:text"));
    assert!(mb.with_state(|s| s.pet_undo.is_empty()));
}

/// `pet_unloadable(Some(reason))` also fronts `pet.apply` (the bridge lex-gates both routes
/// before anything is dispatched): `-32602 UnloadableFile {reason, path:"pet.apply:text"}` and
/// the stacks untouched; `None` runs the built-in.
#[tokio::test]
async fn pet_apply_honours_the_unloadable_knob() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(WIN, false)))
        .pet_unloadable(Some("unbalanced"))
        .start()
        .await
        .unwrap();
    let err = call_err(&mb, "pet.apply", json!({"text": MOCK_PROBE_TEXT})).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(err.wire_name.as_deref(), Some("UnloadableFile"));
    assert_eq!(err.data["reason"], json!("unbalanced"));
    assert_eq!(err.data["path"], json!("pet.apply:text"));
    mb.with_state(|s| {
        assert_eq!(s.pet_text, MOCK_PROBE_TEXT);
        assert!(s.pet_undo.is_empty() && s.pet_redo.is_empty());
        assert_eq!(s.pet_doc.as_ref().unwrap()["dirty"], json!(false));
    });

    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(WIN, false)))
        .pet_unloadable(None::<String>)
        .start()
        .await
        .unwrap();
    let res = call(&mb, "pet.apply", json!({"text": MOCK_PROBE_TEXT})).await.unwrap();
    assert_eq!(res["applied"], json!(true));
}

/// apply -> undo restores the old text, enables redo, keeps the document dirty (undoing back
/// to the saved state leaves it modified, like pet) -> redo re-applies.
#[tokio::test]
async fn pet_undo_restores_and_enables_redo() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(WIN, false)))
        .start()
        .await
        .unwrap();
    let text = format!("{MOCK_PROBE_TEXT}\r\n");
    call(&mb, "pet.apply", json!({"text": text})).await.unwrap();

    let undo = call(&mb, "pet.undo", json!({})).await.unwrap();
    assert_eq!(
        undo,
        json!({
            "done": true,
            "dirty": true,
            "canUndo": false,
            "canRedo": true,
            "arrays": make_pet_doc(WIN, false)["arrays"],
        })
    );
    mb.with_state(|s| {
        assert_eq!(s.pet_text, MOCK_PROBE_TEXT, "the old text was restored");
        assert!(s.pet_redo.contains(&text), "{:?}", s.pet_redo);
        assert_eq!(s.pet_doc.as_ref().unwrap()["dirty"], json!(true));
    });
    let now = call(&mb, "pet.text", json!({})).await.unwrap();
    assert_eq!(now["text"], json!(MOCK_PROBE_TEXT));

    let redo = call(&mb, "pet.redo", json!({})).await.unwrap();
    assert_eq!(redo["done"], json!(true));
    assert_eq!(redo["canUndo"], json!(true));
    assert_eq!(redo["canRedo"], json!(false));
    mb.with_state(|s| assert_eq!(s.pet_text, text));
}

/// Negative: `pet.undo` on an empty history is a result (`done:false`), not an error; the
/// document is untouched.
#[tokio::test]
async fn pet_undo_on_empty_history_reports_done_false() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(WIN, false)))
        .start()
        .await
        .unwrap();
    let res = call(&mb, "pet.undo", json!({})).await.unwrap();
    assert_eq!(
        res,
        json!({
            "done": false,
            "reason": "nothing_to_undo",
            "dirty": false,
            "canUndo": false,
            "canRedo": false,
            "arrays": make_pet_doc(WIN, false)["arrays"],
        })
    );
    mb.with_state(|s| {
        assert_eq!(s.pet_text, MOCK_PROBE_TEXT);
        assert_eq!(s.pet_doc.as_ref().unwrap()["dirty"], json!(false));
    });
    let active = call(&mb, "pet.active", json!({})).await.unwrap();
    assert_eq!(active["doc"]["dirty"], json!(false));
}

/// `pet.redo` right after an apply is `done:false` until an undo happened.
#[tokio::test]
async fn pet_redo_after_apply_is_done_false_until_an_undo() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(WIN, false)))
        .start()
        .await
        .unwrap();
    let text = format!("{MOCK_PROBE_TEXT}\r\n");
    call(&mb, "pet.apply", json!({"text": text})).await.unwrap();

    let res = call(&mb, "pet.redo", json!({})).await.unwrap();
    assert_eq!(res["done"], json!(false));
    assert_eq!(res["reason"], json!("nothing_to_redo"));
    mb.with_state(|s| assert_eq!(s.pet_text, text, "the redo did not touch the text"));

    let undo = call(&mb, "pet.undo", json!({})).await.unwrap();
    assert_eq!(undo["done"], json!(true));
    let redo = call(&mb, "pet.redo", json!({})).await.unwrap();
    assert_eq!(redo["done"], json!(true));
    mb.with_state(|s| assert_eq!(s.pet_text, text, "the redo re-applied"));
}

/// `pet.render` returns the bridge's result shape (default 1024x768, stem from `name` with a
/// trailing `.png` dropped, default-stem sequence otherwise) and honours an explicit size; no
/// file is written (`bytes:0`). Every call is recorded.
#[tokio::test]
async fn pet_render_returns_the_renders_path_and_records_the_call() {
    let doc = make_pet_doc(WIN, false);
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(doc))
        .start()
        .await
        .unwrap();
    let first = call(
        &mb,
        "pet.render",
        json!({"width": 320, "height": 240, "name": "pet_live_test.png"}),
    )
    .await
    .unwrap();
    assert_eq!(
        first,
        json!({
            "path": "Z:\\app\\renders\\pet_live_test.png",
            "file": "pet_live_test.png",
            "width": 320,
            "height": 240,
            "bytes": 0,
            "format": "RGBA8888",
            "hasDocument": true,
            "stats": {
                "untouchedPixels": 0,
                "nonBlackPixels": 76800,
                "meanLuma": 100,
                "minLuma": 0,
                "maxLuma": 255,
                "crc32": "00000000",
            },
            "durationMs": 1,
        })
    );

    // defaults + default stem sequence; a doc-less render still renders (`hasDocument:false`)
    let second = call(&mb, "pet.render", json!({})).await.unwrap();
    assert_eq!(second["width"], json!(1024));
    assert_eq!(second["height"], json!(768));
    assert_eq!(second["path"], json!("Z:\\app\\renders\\render_1.png"));
    assert_eq!(second["file"], json!("render_1.png"));
    let mb2 = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let bare = call(&mb2, "pet.render", json!({})).await.unwrap();
    assert_eq!(bare["hasDocument"], json!(false));
    assert_eq!(bare["file"], json!("render_1.png"));

    // every call was recorded with the params as sent
    let calls = mb.method_calls("pet.render");
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0].params["name"], json!("pet_live_test.png"));
    assert!(calls[1].params.get("name").is_none());
}

/// Negative: size bounds (16..4096 each, product <= 8388608) -> `-32602` before anything else;
/// the product limit is inclusive.
#[tokio::test]
async fn pet_render_refuses_bad_size_32602() {
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    for params in [
        json!({"width": 8}),
        json!({"width": 15}),
        json!({"width": 4097}),
        json!({"height": 0}),
        json!({"width": "big"}),
        json!({"width": 4096, "height": 4096}),
        json!({"width": 4096, "height": 2049}),
    ] {
        let err = call_err(&mb, "pet.render", params.clone()).await;
        assert_eq!(err.code, Some(-32602), "{err:?} {params}");
        assert_eq!(
            err.data["reason"],
            json!("width/height: 16..4096 each, at most 8388608 pixels")
        );
    }
    // the boundary product is fine
    let ok = call(&mb, "pet.render", json!({"width": 4096, "height": 2048})).await.unwrap();
    assert_eq!(ok["width"], json!(4096));
}

/// Negative: a name with anything but `[A-Za-z0-9._-]`, a leading dot or over 64 chars ->
/// `-32602` (`render_name`'s rule).
#[tokio::test]
async fn pet_render_refuses_bad_name_32602() {
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    for params in [
        json!({"name": ".x"}),
        json!({"name": "bad name"}),
        json!({"name": "a/b"}),
        json!({"name": "a\u{00e9}"}),
        json!({"name": format!("a{}", "b".repeat(64))}),
        json!({"name": ".png"}),
    ] {
        let err = call_err(&mb, "pet.render", params.clone()).await;
        assert_eq!(err.code, Some(-32602), "{err:?} {params}");
    }
    // 64 chars exactly, dots/dashes/underscores, and a case-insensitive trailing .png are fine
    for params in [
        json!({"name": format!("a{}", "b".repeat(63))}),
        json!({"name": "a.b-c_d"}),
        json!({"name": "ok.PNG"}),
    ] {
        let res = call(&mb, "pet.render", params).await.unwrap();
        assert!(res["file"].as_str().unwrap().ends_with(".png"), "{res}");
    }
}

/// `pet_render_fail(Some(reason))`: `pet.render` answers `-32020 RenderFailed {reason}`;
/// `None` restores the built-in.
#[tokio::test]
async fn pet_render_fail_knob_is_32020() {
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_render_fail(Some("bad_camera"))
        .start()
        .await
        .unwrap();
    let err = call_err(&mb, "pet.render", json!({})).await;
    assert_eq!(err.code, Some(-32020), "{err:?}");
    assert_eq!(err.kind.name(), "RenderFailed");
    assert_eq!(err.wire_name.as_deref(), Some("RenderFailed"));
    assert_eq!(err.data["reason"], json!("bad_camera"));

    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let res = call(&mb, "pet.render", json!({})).await.unwrap();
    assert_eq!(res["format"], json!("RGBA8888"));
}

// ---------------------------------------------------------------- pet_unloadable knob

/// `pet_unloadable(Some(reason))`: `pet.open` and `pet.reload` answer the bridge's lex refusal
/// `-32602 UnloadableFile {reason, path}` without running their built-in semantics (nothing is
/// opened, no dirty check applies); `pet_unloadable(None)` restores the built-ins.
#[tokio::test]
async fn pet_unloadable_knob_answers_the_bridge_lex_refusal() {
    let win = "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\a.vpcf";
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(win, true)))
        .pet_unloadable(Some("unbalanced"))
        .start()
        .await
        .unwrap();
    // reload: the refusal carries the doc's win path, and the dirty document is untouched
    let err = call_err(&mb, "pet.reload", json!({})).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(err.wire_name.as_deref(), Some("UnloadableFile"));
    assert_eq!(err.data["reason"], json!("unbalanced"));
    assert_eq!(err.data["path"], json!(win));
    assert_eq!(
        mb.with_state(|s| s.pet_doc.as_ref().unwrap()["dirty"].clone()),
        json!(true),
        "the document was not modified"
    );

    // open: same shape, path from the request params (OpenFile's resolved windows form)
    let err = call_err(&mb, "pet.open", json!({"path": "particles/b.vpcf"})).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(err.wire_name.as_deref(), Some("UnloadableFile"));
    assert_eq!(err.data["reason"], json!("unbalanced"));
    assert_eq!(
        err.data["path"],
        json!("Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\b.vpcf")
    );
    // no document was opened by the refused call
    assert_eq!(mb.with_state(|s| s.pet_doc.as_ref().unwrap()["path"].clone()), json!(win));

    // without the knob the built-ins run again (dirty -> UnsavedChanges on reload)
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(win, true)))
        .pet_unloadable(None::<String>)
        .start()
        .await
        .unwrap();
    let err = call_err(&mb, "pet.reload", json!({})).await;
    assert_eq!(err.code, Some(-32016), "{err:?}");
    assert_eq!(err.wire_name.as_deref(), Some("UnsavedChanges"));
}

// ---------------------------------------------------------------- knobs override the built-ins

/// The existing failure knobs must keep overriding the new built-ins (dispatch order).
#[tokio::test]
async fn error_responder_silent_delay_and_drop_override_the_pet_builtins() {
    // `error` wins over the built-in result
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc("Z:\\app\\content\\a.vpcf", false)))
        .error("pet.save", -32014, "AssetNotFound", None)
        .start()
        .await
        .unwrap();
    let err = call_err(&mb, "pet.save", json!({})).await;
    assert_eq!(err.code, Some(-32014));

    // so does `responder`
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .responder_value("pet.active", json!({"doc": "scripted"}))
        .start()
        .await
        .unwrap();
    assert_eq!(
        call(&mb, "pet.active", json!({})).await.unwrap(),
        json!({"doc": "scripted"})
    );

    // `delay` past the deadline -> QueueTimeout (the built-in never runs)
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .delay("pet.active", 500)
        .start()
        .await
        .unwrap();
    let mut c = client_with_deadline(&mb, 100);
    c.connect(false).await.unwrap();
    let err = c
        .request("pet.active", json!({}), RequestOpts::default())
        .await
        .unwrap_err();
    assert_eq!(err.code, Some(-32008));

    // `silent` never answers (client timeout), `drop` closes before replying
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .silent("pet.active")
        .start()
        .await
        .unwrap();
    let mut c = client_with_deadline(&mb, 100);
    c.connect(false).await.unwrap();
    let err = c
        .request("pet.active", json!({}), RequestOpts::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, cs2_mcp_common::bridge::Kind::OutcomeUnknown);

    let mb = MockBridge::builder()
        .pet_loaded(true)
        .drop_method("pet.active")
        .start()
        .await
        .unwrap();
    let mut c = client_with_deadline(&mb, 5000);
    c.connect(false).await.unwrap();
    let err = c
        .request("pet.active", json!({}), RequestOpts::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, cs2_mcp_common::bridge::Kind::ConnectionLost);
}

/// A client whose `deadlineMs` is overridden (like `tests/modeldoc_mock.rs`).
fn client_with_deadline(mb: &MockBridge, deadline_ms: u64) -> BridgeClient {
    BridgeClient::new(ClientOptions {
        discovery_path: mb.discovery_path.clone(),
        deadline_ms,
        reply_grace_ms: 200,
        ..Default::default()
    })
}

/// `set_pet_loaded` withdraws/re-advertises the six `NEED_PET` methods and bumps the revision.
#[tokio::test]
async fn set_pet_loaded_flips_the_advertisement() {
    let mb = MockBridge::builder().pet().start().await.unwrap();
    mb.set_pet_loaded(true);
    let methods = advertised(&mb).await;
    for m in PET_METHODS {
        assert!(contains(&methods, m), "{m}");
    }
    mb.set_pet_loaded(false);
    let methods = advertised(&mb).await;
    for m in PET_POST_LOAD_METHODS {
        assert!(!contains(&methods, m), "{m} still advertised");
    }
    assert!(contains(&methods, "pet.load"));
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    let caps = c.capabilities().await.unwrap();
    assert_eq!(caps["revision"], json!(2));
}

/// Every pet request the mock saw carried an explicit `deadlineMs > 0` (like the modeldoc check).
#[tokio::test]
async fn every_pet_request_carries_an_explicit_deadline() {
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    for m in PET_METHODS {
        let _ = c
            .request(m, json!({"path": "particles/x.vpcf", "class": "C_OP_Decay", "force": true}), RequestOpts::default())
            .await;
    }
    for r in mb.requests().into_iter().filter(|r| r.method.starts_with("pet.")) {
        let dm = r.params["deadlineMs"].as_u64().unwrap_or(0);
        assert!(dm > 0, "{r:?}");
    }
}
