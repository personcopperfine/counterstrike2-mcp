//! Mock-backed tests for the live-edit particle tools
//! (`particle_live_read/live_edit/undo/redo`, WP-T1) against the mock bridge's `pet.*` built-ins
//! (m2.13 semantics, plan2 §0.3). Harness copied from `tests/particle_live.rs`.
//!
//! The temp project tree mimics the real layout: `--project-root` is the temp root, so the schema
//! snapshot lives at `<root>/hammer-mcp/research/pet-schema.json` and the content at
//! `<root>/workshop/run/app/content/…` (PathMap's default `z:\app` mapping).
//!
//! The tools are registered directly here (`particle_live_edit::register`) because the orchestrator
//! wires `tools/particles.rs` (PET_TOOL_ORDER) only at merge time; the registry under test is the
//! real one, so capability gating is exercised exactly as in production.
//!
//! Mock fidelity note (plan2 §4.12): the mock does not parse KV3, so its `pet.apply` `arrays` are
//! the document knob's, unchanged — the tests below assert the *sent text*, not the arrays (the
//! undo/redo `document` assertions pin the knob's stale rows on purpose).
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mock::{make_pet_doc, MockBridge, MOCK_PROBE_TEXT};
use cs2_mcp_common::tools::{Budget, CallFailure, Registry, ToolError};
use hammer_mcp::state::HammerState;
use hammer_mcp::tools::particle_live_edit::{self, PARTICLE_LIVE_EDIT_TOOL_ORDER};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- fixtures

/// The live-verified probe `.vpcf` (Valve KV3 layout, CRLF) — what the mock's `pet.text`
/// serves, modulo pet's normalization (`m_nBehaviorVersion = 12`, CRLF, tabs, no final newline).
const SNAPSHOT: &str = include_str!("../fixtures/vpcf/schema-test.json");
/// The probe window path the mock's `pet.open`/`pet.active`/`pet.text` report.
const DOC_WIN: &str = "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\mcp_probe\\probe.vpcf";

/// A scratch project tree under `target/tmp` (never `/tmp`), unique per test.
struct Host {
    dir: PathBuf,
}
impl Host {
    fn new(tag: &str) -> Host {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("particle-live-edit-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Host { dir }
    }
    fn addon(&self) -> PathBuf {
        self.dir.join("workshop/run/app/content/csgo_addons/mcp_test")
    }
    /// The host path of the probe the mock's document path reports.
    fn probe_host(&self) -> PathBuf {
        self.addon().join("particles/mcp_probe/probe.vpcf")
    }
    /// The snapshot the tools read under `--project-root`'s checkout.
    fn snapshot(&self) -> PathBuf {
        self.dir.join("hammer-mcp/research/pet-schema.json")
    }
    /// Install the test snapshot where the validator looks for it.
    fn write_snapshot(&self) -> PathBuf {
        let path = self.snapshot();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, SNAPSHOT).unwrap();
        path
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn config(discovery: &Path, host_dir: &Path) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let mut argv = vec![
        "t".to_string(),
        "--discovery".to_string(),
        discovery.display().to_string(),
        "--project-root".to_string(),
        host_dir.display().to_string(),
    ];
    // hermetic podman/vk-layer (the pattern of tests/common/mod.rs, copied, not imported):
    // nothing here may touch the host's real podman or project tree.
    argv.extend([
        "--podman".into(),
        "/nonexistent/cs2mcp-test-podman".into(),
        "--vk-layer-dir".into(),
        "/nonexistent/cs2mcp-test-vklayer".into(),
    ]);
    W::parse_from(argv).c
}

/// A real registry (capability gating included) with the four live-edit tools registered.
struct Server {
    state: HammerState,
    registry: Registry<HammerState>,
}

impl Server {
    fn new(discovery: &Path, host_dir: &Path) -> Server {
        let state = HammerState::from_config(&config(discovery, host_dir)).unwrap();
        let mut registry = Registry::new();
        particle_live_edit::register(&mut registry);
        Server { state, registry }
    }
    async fn call(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        let budget = Budget::new(45_000, 30_000);
        match self.registry.call(&self.state, name, args, budget).await {
            Ok(out) => Ok(out.json),
            Err(CallFailure::Tool(e)) => Err(e),
            Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
        }
    }
}

/// The `deadlineMs` of every recorded `pet.*` request (asserted > 0 across all tests).
fn pet_deadlines(mb: &MockBridge) -> Vec<u64> {
    mb.requests()
        .into_iter()
        .filter(|r| r.method.starts_with("pet."))
        .map(|r| r.params["deadlineMs"].as_u64().unwrap_or(0))
        .collect()
}

/// The one-op args adding a function to `m_Operators`.
fn add_operator(class: &str) -> Value {
    json!({"ops": [{"op": "add_function", "array": "m_Operators", "function": {"_class": class}}]})
}

// ---------------------------------------------------------------- registration sanity

#[test]
fn the_four_tools_are_registered_in_order() {
    let mut registry = Registry::<HammerState>::new();
    particle_live_edit::register(&mut registry);
    assert_eq!(registry.names(), PARTICLE_LIVE_EDIT_TOOL_ORDER);
    // live_edit's input schema: ops required, the three knobs documented
    let edit = registry.spec("particle_live_edit").unwrap();
    assert_eq!(edit.input_schema["required"], json!(["ops"]));
    assert_eq!(edit.input_schema["properties"]["ops"]["type"], json!("array"));
    assert_eq!(edit.input_schema["properties"]["label"]["type"], json!("string"));
    assert_eq!(edit.input_schema["properties"]["skipSchema"]["type"], json!("boolean"));
    assert_eq!(edit.input_schema["properties"]["allowObsolete"]["type"], json!("boolean"));
    // live_read's one optional knob
    let read = registry.spec("particle_live_read").unwrap();
    assert_eq!(read.input_schema["properties"]["function"]["type"], json!("string"));
    assert_eq!(read.input_schema["required"], Value::Null);
    // undo/redo take no arguments (open object schema)
    for name in ["particle_undo", "particle_redo"] {
        assert_eq!(registry.spec(name).unwrap().input_schema["required"], Value::Null);
    }
}

// ---------------------------------------------------------------- particle_live_read

#[tokio::test]
async fn live_read_parses_pet_text_counts_and_flags() {
    let host = Host::new("read-happy");
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server.call("particle_live_read", json!({})).await.unwrap();
    assert_eq!(p["source"], json!("live"));
    assert_eq!(p["path"], json!(DOC_WIN));
    assert_eq!(p["hostPath"], json!(host.probe_host().display().to_string()));
    assert_eq!(p["dirty"], json!(false));
    assert_eq!(p["isNewFile"], json!(false));
    assert_eq!(p["canUndo"], json!(false));
    assert_eq!(p["canRedo"], json!(false));
    assert_eq!(p["bytes"], json!(MOCK_PROBE_TEXT.len()));
    assert!(
        p["header"].as_str().unwrap().starts_with("<!-- kv3"),
        "{}",
        p["header"]
    );
    // the root's non-array fields (no array keys in root)
    assert_eq!(p["root"]["_class"], json!("CParticleSystemDefinition"));
    assert_eq!(p["root"]["m_nBehaviorVersion"], json!(12));
    assert_eq!(p["root"]["m_nMaxParticles"], json!(32));
    // the per-array entry counts (only the arrays the document has)
    assert_eq!(
        p["counts"],
        json!({"m_Emitters": 1, "m_Initializers": 1, "m_Operators": 2, "m_Renderers": 1})
    );
    assert!(mb.method_calls("pet.text").len() == 1, "one pet.text round trip");
    assert!(pet_deadlines(&mb).iter().all(|d| *d > 0));
}

#[tokio::test]
async fn live_read_function_ref_returns_the_entry() {
    let host = Host::new("read-function");
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server
        .call("particle_live_read", json!({"function": "m_Operators/1"}))
        .await
        .unwrap();
    assert_eq!(p["function"]["path"], json!("m_Operators/1"));
    assert_eq!(p["function"]["array"], json!("m_Operators"));
    assert_eq!(p["function"]["type"], json!(3));
    assert_eq!(p["function"]["index"], json!(1));
    assert_eq!(p["function"]["_class"], json!("C_OP_Decay"));
}

#[tokio::test]
async fn live_read_unknown_function_is_function_not_found() {
    let host = Host::new("read-function-missing");
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call("particle_live_read", json!({"function": "m_Operators/9"}))
        .await
        .unwrap_err();
    assert_eq!(e.name, "FunctionNotFound", "{e:?}");
    assert_eq!(e.code, Some(-32602));
    assert_eq!(e.data["path"], json!("m_Operators/9"));
    assert!(e.retry_safe);
    // and an unparseable reference is an argument error before anything is sent
    let e = server
        .call("particle_live_read", json!({"function": "not-a-ref"}))
        .await
        .unwrap_err();
    assert_eq!(e.name, "InvalidParams");
    // only the first (valid) call reached the bridge; the bad reference was refused locally
    assert_eq!(mb.method_calls("pet.text").len(), 1);
}

/// NEGATIVE: `pet.text`'s text must parse on the host. Pet's own serialization parses, so garbage
/// flags a parser gap: `LiveTextUnparseable` with the parse position, nothing else is sent.
#[tokio::test]
async fn live_read_garbage_text_is_live_text_unparseable() {
    let host = Host::new("read-garbage");
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .responder("pet.text", |_| {
            json!({
                "text": "{{{", "bytes": 3, "path": DOC_WIN,
                "dirty": false, "isNewFile": false, "canUndo": false, "canRedo": false,
            })
        })
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("particle_live_read", json!({})).await.unwrap_err();
    assert_eq!(e.name, "LiveTextUnparseable", "{e:?}");
    assert_eq!(e.code, Some(-32602));
    assert!(e.retry_safe);
    assert!(e.data["line"].as_u64().is_some());
    assert!(e.data["col"].as_u64().is_some());
    assert!(!e.data["msg"].as_str().unwrap().is_empty());
    assert!(mb.method_calls("pet.apply").is_empty());
}

// ---------------------------------------------------------------- particle_live_edit

#[tokio::test]
async fn live_edit_happy_sends_apply_once_with_the_new_class() {
    let host = Host::new("edit-happy");
    host.write_snapshot();
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server
        .call("particle_live_edit", add_operator("C_OP_BasicMovement"))
        .await
        .unwrap();
    assert_eq!(p["applied"], json!([{"op": "add_function", "path": "m_Operators/2"}]));
    assert_eq!(p["appliedCount"], json!(1));
    assert!(p["label"].as_str().unwrap().starts_with("MCP: add_function"), "{}", p["label"]);
    assert_eq!(p["dirty"], json!(true));
    assert_eq!(p["canUndo"], json!(true));
    assert_eq!(p["canRedo"], json!(false));
    assert_eq!(p["schemaChecked"], json!(true));
    assert!(p["schemaSource"].as_str().is_some());
    assert!(p["warnings"].as_array().unwrap().is_empty());

    // exactly one pet.text then one pet.apply, both with deadlines; the apply carries the
    // re-serialized text with the new class and the computed label
    let requests = mb.requests();
    let pet: Vec<&str> = requests
        .iter()
        .filter(|r| r.method.starts_with("pet."))
        .map(|r| r.method.as_str())
        .collect();
    assert_eq!(pet, vec!["pet.text", "pet.apply"], "{requests:?}");
    let calls = mb.method_calls("pet.apply");
    assert_eq!(calls.len(), 1);
    let sent = calls[0].params["text"].as_str().unwrap();
    assert!(sent.contains("C_OP_BasicMovement"), "the sent text must carry the new class");
    assert!(sent.contains("_class = \"CParticleSystemDefinition\""), "{sent:?}");
    assert_eq!(calls[0].params["label"], p["label"]);
    assert!(pet_deadlines(&mb).iter().all(|d| *d > 0));

    // the document view: the live path mapped, dirty now, the (stale, knob) arrays mapped by key
    assert_eq!(p["document"]["path"], json!(DOC_WIN));
    assert_eq!(p["document"]["isNewFile"], json!(false));
    assert_eq!(p["document"]["dirty"], json!(true));
    assert_eq!(p["document"]["inAddon"], json!(true));
    assert_eq!(p["document"]["arrays"]["m_Operators"], json!(["C_OP_BasicMovement", "C_OP_Decay"]));
}

#[tokio::test]
async fn live_edit_unknown_class_is_validation_failed_and_sends_no_apply() {
    let host = Host::new("edit-unknown-class");
    host.write_snapshot();
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call("particle_live_edit", add_operator("C_OP_NotAThing"))
        .await
        .unwrap_err();
    assert_eq!(e.name, "ValidationFailed", "{e:?}");
    assert_eq!(e.code, Some(-32602));
    assert!(e.retry_safe);
    let issues = e.data["issues"].as_array().unwrap();
    assert!(
        issues
            .iter()
            .any(|i| i["path"] == json!("m_Operators/2")
                && i["msg"].as_str().unwrap().contains("C_OP_NotAThing")),
        "{issues:?}"
    );
    assert!(mb.method_calls("pet.apply").is_empty(), "nothing was sent");
    assert!(mb.method_calls("pet.text").len() == 1, "the text was read, then refused");
}

#[tokio::test]
async fn live_edit_bad_index_is_invalid_arguments_and_sends_no_apply() {
    let host = Host::new("edit-bad-index");
    host.write_snapshot();
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call(
            "particle_live_edit",
            json!({"ops": [{"op": "remove_function", "array": "m_Operators", "index": 9}]}),
        )
        .await
        .unwrap_err();
    // a BadOp surfaces through `vpcf_error` as InvalidOp (the particle_edit mapping), -32602
    assert_eq!(e.name, "InvalidOp", "{e:?}");
    assert_eq!(e.code, Some(-32602));
    assert!(e.retry_safe);
    assert!(mb.method_calls("pet.apply").is_empty(), "nothing was sent");
}

/// NEGATIVE (structure is never skippable): with `skipSchema:true` the structural check still
/// refuses the apply. The plan's literal op for this (`add_function` without `_class`) is refused
/// by the op layer itself (`InvalidOp`), asserted below; the structural error an op *can* leave
/// behind is a non-integer `m_nBehaviorVersion`, which `validate_structure` errors on.
#[tokio::test]
async fn live_edit_skip_schema_keeps_structure() {
    let host = Host::new("edit-skip-schema");
    host.write_snapshot();
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);

    // a structural error the op layer lets through: m_nBehaviorVersion = "twelve"
    let e = server
        .call(
            "particle_live_edit",
            json!({"ops": [{"op": "set_root_field", "key": "m_nBehaviorVersion", "value": "twelve"}]}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.name, "ValidationFailed", "{e:?}");
    assert_eq!(e.code, Some(-32602));
    assert!(e.retry_safe);
    assert!(
        e.data["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["path"] == json!("m_nBehaviorVersion")),
        "{issues:?}",
        issues = e.data["issues"]
    );
    assert!(mb.method_calls("pet.apply").is_empty(), "nothing was sent");

    // the plan's literal op: add_function without `_class` — refused by the op layer (BadOp),
    // never sent, also with skipSchema
    let e = server
        .call(
            "particle_live_edit",
            json!({"ops": [{"op": "add_function", "array": "m_Operators", "function": {}}]}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.name, "InvalidOp", "{e:?}");
    assert_eq!(e.code, Some(-32602));
    assert!(mb.method_calls("pet.apply").is_empty());
}

#[tokio::test]
async fn live_edit_bad_label_is_invalid_arguments_and_sends_nothing() {
    let host = Host::new("edit-bad-label");
    host.write_snapshot();
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    // the bridge finds params by substring, so a quote in the label could fake a "text" key
    for label in ["", &"x".repeat(65), "a\"text\":\"x", "back\\slash", "tab\there"] {
        let mut args = add_operator("C_OP_BasicMovement");
        args["label"] = json!(label);
        let e = server.call("particle_live_edit", args).await.unwrap_err();
        assert_eq!(e.type_, "InvalidArguments", "{label:?}: {e:?}");
        assert_eq!(e.code, Some(-32602));
    }
    assert!(mb.method_calls("pet.text").is_empty() && mb.method_calls("pet.apply").is_empty());
}

#[tokio::test]
async fn live_edit_oversize_text_is_text_too_large() {
    let host = Host::new("edit-oversize");
    host.write_snapshot();
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    // a 400 KB root field: with the schema check skipped the structural check passes, but the
    // bridge's 384 KB pet.apply gate trips on the host first (an unknown root field would be a
    // schema error, and the size gate is what is under test here)
    let big = "a".repeat(400 * 1024);
    let e = server
        .call(
            "particle_live_edit",
            json!({
                "ops": [{"op": "set_root_field", "key": "m_mcpBig", "value": big}],
                "skipSchema": true
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.name, "TextTooLarge", "{e:?}");
    assert_eq!(e.code, Some(-32602));
    assert!(e.retry_safe);
    assert_eq!(e.data["max"], json!(393216));
    assert!(mb.method_calls("pet.apply").is_empty(), "nothing was sent");
}

#[tokio::test]
async fn live_edit_without_a_document_is_no_active_document() {
    let host = Host::new("edit-nodoc");
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call("particle_live_edit", add_operator("C_OP_BasicMovement"))
        .await
        .unwrap_err();
    assert_eq!(e.code, Some(-32003), "{e:?}");
    assert_eq!(e.wire_name.as_deref(), Some("NoActiveDocument"));
    assert!(mb.method_calls("pet.apply").is_empty());
}

/// NEGATIVE: a bridge lex refusal (`-32602 UnloadableFile`) passes through `ToolError::from`
/// with its wire name, code and data intact (the host checks passed; the refusal is the bridge's).
#[tokio::test]
async fn live_edit_bridge_unloadable_keeps_the_wire_name() {
    let host = Host::new("edit-unloadable");
    host.write_snapshot();
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .pet_unloadable(Some("unbalanced"))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call("particle_live_edit", add_operator("C_OP_BasicMovement"))
        .await
        .unwrap_err();
    assert_eq!(e.code, Some(-32602), "{e:?}");
    assert_eq!(e.wire_name.as_deref(), Some("UnloadableFile"));
    assert_eq!(e.data["reason"], json!("unbalanced"));
    assert_eq!(e.data["path"], json!("pet.apply:text"));
    assert_eq!(mb.method_calls("pet.apply").len(), 1, "the request was dispatched once");
}

#[tokio::test]
async fn live_edit_without_capabilities_is_not_implemented_missing_both() {
    let host = Host::new("edit-gated");
    // .pet() before a load: only pet.load is advertised
    let mb = MockBridge::builder().pet().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call(
            "particle_live_edit",
            json!([{"op": "set_root_field", "key": "m_nMaxParticles", "value": 64}]),
        )
        .await
        .unwrap_err();
    assert_eq!(e.name, "NotImplemented", "{e:?}");
    assert_eq!(e.code, Some(-32011));
    assert_eq!(e.data["missing"], json!(["pet.text", "pet.apply"]));
    assert!(e.retry_safe);
    assert!(mb.method_calls("pet.text").is_empty());
    assert!(mb.method_calls("pet.apply").is_empty());
}

// ---------------------------------------------------------------- particle_undo / particle_redo

#[tokio::test]
async fn undo_happy_then_nothing_to_undo() {
    let host = Host::new("undo-happy");
    host.write_snapshot();
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);

    // an empty history is a retry-safe error, not a crash
    let e = server.call("particle_undo", json!({})).await.unwrap_err();
    assert_eq!(e.name, "NothingToUndo", "{e:?}");
    assert!(e.retry_safe);
    assert_eq!(e.data["reason"], json!("nothing_to_undo"));

    // an apply pushes the undo step; the undo restores the previous text
    let _ = server
        .call("particle_live_edit", add_operator("C_OP_BasicMovement"))
        .await
        .unwrap();
    let p = server.call("particle_undo", json!({})).await.unwrap();
    assert_eq!(p["done"], json!(true));
    assert_eq!(p["dirty"], json!(true));
    assert_eq!(p["canUndo"], json!(false));
    assert_eq!(p["canRedo"], json!(true));
    assert!(p["document"].is_object());
    // the (stale, knob) arrays are passed through mapped by key
    assert_eq!(
        p["document"]["arrays"]["m_Operators"],
        json!(["C_OP_BasicMovement", "C_OP_Decay"])
    );

    // the text is back to the probe (observable through pet.text)
    let read = server.call("particle_live_read", json!({})).await.unwrap();
    assert_eq!(read["root"]["m_nMaxParticles"], json!(32));

    // and the history is empty again
    let e = server.call("particle_undo", json!({})).await.unwrap_err();
    assert_eq!(e.name, "NothingToUndo", "{e:?}");
    assert_eq!(mb.method_calls("pet.undo").len(), 3, "three undo round trips");
    assert!(pet_deadlines(&mb).iter().all(|d| *d > 0));
}

#[tokio::test]
async fn redo_after_undo() {
    let host = Host::new("redo-happy");
    host.write_snapshot();
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);

    // nothing to redo before any undo
    let e = server.call("particle_redo", json!({})).await.unwrap_err();
    assert_eq!(e.name, "NothingToRedo", "{e:?}");
    assert!(e.retry_safe);
    assert_eq!(mb.method_calls("pet.redo").len(), 1, "the refusal was the bridge's");

    // apply -> undo -> redo: the undone edit comes back
    let _ = server
        .call("particle_live_edit", add_operator("C_OP_BasicMovement"))
        .await
        .unwrap();
    let _ = server.call("particle_undo", json!({})).await.unwrap();
    let p = server.call("particle_redo", json!({})).await.unwrap();
    assert_eq!(p["done"], json!(true));
    assert_eq!(p["dirty"], json!(true));
    assert_eq!(p["canUndo"], json!(true));
    assert_eq!(p["canRedo"], json!(false));
    assert!(p["document"].is_object());

    // the redone edit is live again (the appended operator is at m_Operators/2)
    let read = server
        .call("particle_live_read", json!({"function": "m_Operators/2"}))
        .await
        .unwrap();
    assert_eq!(read["function"]["_class"], json!("C_OP_BasicMovement"));
    assert_eq!(mb.method_calls("pet.redo").len(), 2, "two redo round trips");
}

#[tokio::test]
async fn undo_without_a_document_is_no_active_document() {
    let host = Host::new("undo-nodoc");
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("particle_undo", json!({})).await.unwrap_err();
    assert_eq!(e.code, Some(-32003), "{e:?}");
    assert_eq!(e.wire_name.as_deref(), Some("NoActiveDocument"));
}

// ---------------------------------------------------------------- deadlines

/// Every `pet.*` request the four tools sent carried an explicit `deadlineMs > 0`.
#[tokio::test]
async fn every_request_carries_a_deadline() {
    let host = Host::new("deadlines");
    host.write_snapshot();
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let _ = server.call("particle_live_read", json!({})).await;
    let _ = server
        .call("particle_live_edit", add_operator("C_OP_BasicMovement"))
        .await;
    let _ = server.call("particle_undo", json!({})).await;
    let _ = server.call("particle_redo", json!({})).await;
    let deadlines = pet_deadlines(&mb);
    assert!(!deadlines.is_empty());
    assert!(deadlines.iter().all(|d| *d > 0), "{deadlines:?}");
}
