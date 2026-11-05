//! Mock-backed tests for the live ModelDoc tools (`model_status/open/reload/save/node_types`),
//! against the mock bridge's `modeldoc.*` built-ins (m2.11 semantics).
//!
//! The temp project tree mimics the real layout: `--project-root` is the temp root, so the
//! content lives at `<root>/workshop/run/app/content/…` (PathMap's default `z:\app` mapping) and
//! the node-type snapshot at `<root>/hammer-mcp/research/modeldoc-nodetypes.json`.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mock::{make_model_doc, MockBridge};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::tools::modeldoc::MODELDOC_TOOL_ORDER;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- fixtures

const STATIC_PROP: &str = include_str!("../fixtures/vmdl/static_prop.vmdl");
const BAD_TREE: &str = include_str!("../fixtures/vmdl/bad_tree.vmdl");
const BROKEN: &str = "not kv3 at all {{{";
const DOC_WIN: &str = "Z:\\app\\content\\csgo_addons\\mcp_test\\models\\mcp_probe\\glass_probe.vmdl";

/// A scratch project tree under `target/tmp` (never `/tmp`), unique per test.
struct Host {
    dir: PathBuf,
}
impl Host {
    fn new(tag: &str) -> Host {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("model-live-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        let model_dir = dir.join("workshop/run/app/content/csgo_addons/mcp_test/models/mcp_probe");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(model_dir.join("glass_probe.vmdl"), STATIC_PROP).unwrap();
        Host { dir }
    }
    /// A second probe model (path only: tools that read it must copy `Host::new`'s setup; tests
    /// that only need the path for refusals can leave it missing).
    fn write_model(&self, name: &str, text: &str) -> PathBuf {
        let path = self
            .dir
            .join("workshop/run/app/content/csgo_addons/mcp_test/models/mcp_probe")
            .join(format!("{name}.vmdl"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }
    /// A `.vmdl` inside the content root but outside the active addon (core content).
    fn write_core_model(&self, name: &str, text: &str) -> PathBuf {
        let path = self
            .dir
            .join("workshop/run/app/content/csgo/models")
            .join(format!("{name}.vmdl"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }
    /// The node-type snapshot the tools write/read under `--project-root`'s checkout.
    fn snapshot(&self) -> PathBuf {
        self.dir
            .join("hammer-mcp/research/modeldoc-nodetypes.json")
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

async fn call(
    server: &cs2_mcp_common::mcp::McpServer<hammer_mcp::state::HammerState>,
    name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

async fn mock(model_doc: Value, modeldoc: bool) -> MockBridge {
    let mut b = MockBridge::builder().model_doc(Some(model_doc));
    if modeldoc {
        b = b.modeldoc();
    }
    b.start().await.unwrap()
}

/// Doc the mock serves, with `dirty` replaced and extra keys added (the builder's
/// `make_model_doc` is fixed-shape: `isNewFile`/`hasAsset` always true/false).
fn doc_with(win_path: &str, dirty: Value, extra: &[(&str, Value)]) -> Value {
    let mut doc = json!({
        "path": win_path, "dirty": dirty, "isNewFile": false, "hasAsset": true,
        "compileStatus": null, "docType": "vmdl",
    });
    for (k, v) in extra {
        doc[k] = v.clone();
    }
    doc
}

/// The deadlineMs of every recorded `modeldoc.*` request (asserted > 0 across all tests).
fn modeldoc_deadlines(mb: &MockBridge) -> Vec<u64> {
    mb.requests()
        .into_iter()
        .filter(|r| r.method.starts_with("modeldoc."))
        .map(|r| r.params["deadlineMs"].as_u64().unwrap_or(0))
        .collect()
}

// ---------------------------------------------------------------- registration sanity

#[test]
fn the_ten_modeldoc_tools_are_registered() {
    let list = hammer_mcp::build_server(&config(
        Path::new("/nonexistent/cs2mcp-no-bridge/session.json"),
        Path::new("/nonexistent/cs2mcp-no-host"),
    ))
    .unwrap()
    .registry()
    .names();
    let start = 24 + hammer_mcp::tools::hammer::ENTITY_TOOL_ORDER.len();
    assert_eq!(&list[start..start + MODELDOC_TOOL_ORDER.len()], MODELDOC_TOOL_ORDER);
}

// ---------------------------------------------------------------- model_status

#[tokio::test]
async fn status_loaded_shows_the_document_and_no_missing_caps() {
    let host = Host::new("status-loaded");
    let mb = mock(make_model_doc(DOC_WIN, false), true).await;
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let p = call(&server, "model_status", json!({})).await.unwrap();
    assert_eq!(p["bridge"]["running"], json!(true));
    assert_eq!(p["modeldocLoaded"], json!(true));
    assert_eq!(p["capabilities"]["missing"], json!([]));
    assert_eq!(p["document"]["path"], json!(DOC_WIN));
    assert_eq!(
        p["document"]["hostPath"],
        json!(host
            .dir
            .join("workshop/run/app/content/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl")
            .display()
            .to_string())
    );
    assert_eq!(p["document"]["pathMapped"], json!(true));
    assert_eq!(p["document"]["inAddon"], json!(true));
    assert_eq!(p["fileNewerThanEditor"], Value::Null);
    assert_eq!(p["hint"], Value::Null);
    let deadlines = modeldoc_deadlines(&mb);
    assert!(!deadlines.is_empty());
    assert!(deadlines.iter().all(|d| *d > 0));
}

#[tokio::test]
async fn status_without_the_module_reports_the_missing_seven_and_a_hint() {
    let host = Host::new("status-notloaded");
    // capabilities exist (hammer loads) but no `.modeldoc()` and no document: not loaded
    let mb = mock(doc_with(DOC_WIN, json!(false), &[]), false).await;
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let p = call(&server, "model_status", json!({})).await.unwrap();
    assert_eq!(p["bridge"]["running"], json!(true));
    assert_eq!(p["modeldocLoaded"], json!(false));
    let missing: Vec<Value> = [
        "modeldoc.active",
        "modeldoc.open",
        "modeldoc.reload",
        "modeldoc.save",
        "modeldoc.compile",
        "modeldoc.nodeTypes",
        "modeldoc.render",
    ]
    .iter()
    .map(|m| json!(m))
    .collect();
    assert_eq!(p["capabilities"]["missing"], json!(missing));
    assert!(p["hint"]
        .as_str()
        .unwrap()
        .contains("TOOL=modeldoc")
        && p["hint"].as_str().unwrap().contains("runtime up")
        && p["hint"].as_str().unwrap().contains("runtime open"));
    // addon-relative: `open csgo_addons/...` raises the "Unrecognized Asset" modal live
    let hint = p["hint"].as_str().unwrap();
    assert!(hint.contains("open models/<file>.vmdl") && !hint.contains("csgo_addons"), "{hint}");
    // no unadvertised method was probed: nothing modeldoc.* was sent
    assert!(modeldoc_deadlines(&mb).is_empty());
}

#[tokio::test]
async fn status_not_connected_never_raises() {
    let host = Host::new("status-dead");
    let server = hammer_mcp::build_server(&config(
        Path::new("/nonexistent/cs2mcp-no-bridge/session.json"),
        &host.dir,
    ))
    .unwrap();
    let p = call(&server, "model_status", json!({})).await.unwrap();
    assert_eq!(p["bridge"]["running"], json!(false));
    assert_eq!(p["modeldocLoaded"], json!(false));
    let hint = p["hint"].as_str().unwrap();
    assert!(hint.ends_with("cargo bridge runtime up"), "{hint}");
    assert!(p.get("document").is_none());
    // the seven methods are reported missing even without a connection
    assert_eq!(p["capabilities"]["missing"].as_array().unwrap().len(), 7);
}

#[tokio::test]
async fn status_with_no_document_reports_the_bridge_reason() {
    let host = Host::new("status-nodoc");
    let mb = MockBridge::builder().modeldoc().start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let p = call(&server, "model_status", json!({})).await.unwrap();
    assert_eq!(p["modeldocLoaded"], json!(true));
    assert_eq!(p["document"], Value::Null);
    assert_eq!(p["documentReason"], json!("no_session"));
    assert_eq!(p["fileNewerThanEditor"], Value::Null);
}

#[tokio::test]
async fn status_unmapped_or_outside_addon_path_is_reported_not_guessed() {
    let host = Host::new("status-unmapped");
    let mb = mock(make_model_doc("q:\\nowhere\\x.vmdl", false), true).await;
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let p = call(&server, "model_status", json!({})).await.unwrap();
    assert_eq!(p["document"]["pathMapped"], json!(false));
    assert_eq!(p["document"]["hostPath"], Value::Null);
    assert_eq!(p["document"]["inAddon"], json!(false));
}

// ---------------------------------------------------------------- model_open

#[tokio::test]
async fn open_happy_path_sends_the_addon_relative_path() {
    let host = Host::new("open-happy");
    let mb = mock(make_model_doc(DOC_WIN, false), true).await;
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let p = call(
        &server,
        "model_open",
        json!({"path": "csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl"}),
    )
    .await
    .unwrap();
    assert_eq!(p["opened"], json!(true));
    assert_eq!(
        p["path"],
        json!("csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl")
    );
    assert_eq!(p["contentPath"], json!("models/mcp_probe/glass_probe.vmdl"));
    assert_eq!(p["document"]["path"], json!(DOC_WIN));
    assert_eq!(p["document"]["inAddon"], json!(true));
    let calls = mb.method_calls("modeldoc.open");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].params["path"], json!("models/mcp_probe/glass_probe.vmdl"));
    assert!(modeldoc_deadlines(&mb).iter().all(|d| *d > 0));
}

#[tokio::test]
async fn open_refuses_a_dirty_document_and_sends_nothing() {
    let host = Host::new("open-dirty");
    let mb = mock(make_model_doc(DOC_WIN, true), true).await;
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let e = call(&server, "model_open", json!({"path": "csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl"}))
        .await
        .unwrap_err();
    assert_eq!(e.name, "UnsavedChanges");
    assert_eq!(e.code, Some(-32016));
    assert!(e.retry_safe);
    assert!(mb.method_calls("modeldoc.open").is_empty());
    // force proceeds
    let p = call(
        &server,
        "model_open",
        json!({"path": "csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl", "force": true}),
    )
    .await
    .unwrap();
    assert_eq!(p["opened"], json!(true));
    assert_eq!(mb.method_calls("modeldoc.open").len(), 1);
}

#[tokio::test]
async fn open_refuses_a_dirty_unknown_document_unless_force() {
    for (force, opened) in [(false, false), (true, true)] {
        let host = Host::new("open-dirty-null");
        let mb = mock(doc_with(DOC_WIN, json!(null), &[]), true).await;
        let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
        let mut args = json!({"path": "csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl"});
        if force {
            args["force"] = json!(true);
        }
        let result = call(&server, "model_open", args).await;
        if opened {
            assert!(result.is_ok(), "force must proceed");
            assert_eq!(mb.method_calls("modeldoc.open").len(), 1);
        } else {
            let e = result.unwrap_err();
            assert_eq!(e.name, "UnsavedChanges");
            assert_eq!(e.code, Some(-32016));
            assert!(mb.method_calls("modeldoc.open").is_empty());
        }
    }
}

#[tokio::test]
async fn open_refuses_a_broken_file_and_sends_nothing() {
    let host = Host::new("open-broken");
    let mb = mock(make_model_doc(DOC_WIN, false), true).await;
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    for (name, text, expected) in [
        ("syntax.vmdl", BROKEN, "ParseError"),
        ("tree.vmdl", BAD_TREE, "InvalidModel"),
    ] {
        host.write_model(name, text);
        let e = call(
            &server,
            "model_open",
            json!({"path": format!("csgo_addons/mcp_test/models/mcp_probe/{name}.vmdl")}),
        )
        .await
        .unwrap_err();
        assert_eq!(e.name, expected, "broken case {name}: {e:?}");
        assert!(e.retry_safe);
    }
    assert!(mb.method_calls("modeldoc.open").is_empty());
    assert!(mb.method_calls("modeldoc.active").is_empty());
}

#[tokio::test]
async fn open_refuses_a_missing_file() {
    let host = Host::new("open-missing");
    let mb = mock(make_model_doc(DOC_WIN, false), true).await;
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let e = call(
        &server,
        "model_open",
        json!({"path": "csgo_addons/mcp_test/models/mcp_probe/gone.vmdl"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, Some(-32602));
    assert!(mb.method_calls("modeldoc.open").is_empty());
}

#[tokio::test]
async fn open_refuses_paths_outside_the_addon() {
    let host = Host::new("open-outside");
    let core = host.write_core_model("core.vmdl", STATIC_PROP);
    assert!(core.is_file());
    let mb = mock(make_model_doc(DOC_WIN, false), true).await;
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    for path in [
        "csgo/models/core.vmdl",
        "csgo_addons/some_other_addon/models/x.vmdl",
    ] {
        let e = call(&server, "model_open", json!({"path": path}))
            .await
            .unwrap_err();
        assert_eq!(e.name, "WriteOutsideAddon", "{path}");
        assert_eq!(e.code, Some(-32602));
    }
    assert!(mb.method_calls("modeldoc.open").is_empty());
}

// ---------------------------------------------------------------- model_reload

#[tokio::test]
async fn reload_refuses_dirty_then_forces() {
    let host = Host::new("reload-dirty");
    let mb = mock(make_model_doc(DOC_WIN, true), true).await;
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let e = call(&server, "model_reload", json!({})).await.unwrap_err();
    assert_eq!(e.name, "UnsavedChanges");
    assert_eq!(e.code, Some(-32016));
    assert!(mb.method_calls("modeldoc.reload").is_empty());
    let p = call(&server, "model_reload", json!({"force": true})).await.unwrap();
    assert_eq!(p["reloaded"], json!(true));
    assert_eq!(p["discardedChanges"], json!(true));
    assert_eq!(
        p["hostPath"],
        json!(host
            .dir
            .join("workshop/run/app/content/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl")
            .display()
            .to_string())
    );
    assert_eq!(mb.method_calls("modeldoc.reload").len(), 1);
    assert_eq!(mb.method_calls("modeldoc.reload")[0].params["force"], json!(true));
}

#[tokio::test]
async fn reload_on_a_clean_document_discards_nothing() {
    let host = Host::new("reload-clean");
    let mb = mock(make_model_doc(DOC_WIN, false), true).await;
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let p = call(&server, "model_reload", json!({})).await.unwrap();
    assert_eq!(p["reloaded"], json!(true));
    assert_eq!(p["discardedChanges"], json!(false));
    assert!(modeldoc_deadlines(&mb).iter().all(|d| *d > 0));
}

#[tokio::test]
async fn reload_without_a_document_is_no_active_document() {
    let host = Host::new("reload-nodoc");
    let mb = MockBridge::builder().modeldoc().start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let e = call(&server, "model_reload", json!({})).await.unwrap_err();
    assert_eq!(e.code, Some(-32003));
    assert_eq!(e.name, "NoDocument"); // wire name NoActiveDocument (Kind::name keeps its own)
    assert_eq!(e.wire_name.as_deref(), Some("NoActiveDocument"));
    // not retry_safe by taxonomy: a retry may race a document the editor opens meanwhile
    assert!(!e.retry_safe);
    assert!(mb.method_calls("modeldoc.reload").is_empty());
}

// ---------------------------------------------------------------- model_save

#[tokio::test]
async fn save_refuses_untitled_documents_and_sends_nothing() {
    let host = Host::new("save-untitled");
    let mb = mock(
        doc_with(DOC_WIN, json!(false), &[("isNewFile", json!(true))]),
        true,
    )
    .await;
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let e = call(&server, "model_save", json!({})).await.unwrap_err();
    assert_eq!(e.name, "UntitledDocument");
    assert_eq!(e.code, Some(-32015));
    assert!(e.retry_safe);
    assert!(mb.method_calls("modeldoc.save").is_empty());
}

#[tokio::test]
async fn save_of_a_clean_document_reports_skipped_clean() {
    let host = Host::new("save-clean");
    let mb = mock(make_model_doc(DOC_WIN, false), true).await;
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let p = call(&server, "model_save", json!({})).await.unwrap();
    assert_eq!(p["saved"], json!(true));
    assert_eq!(p["skippedClean"], json!(true));
    assert_eq!(
        p["hostPath"],
        json!(host
            .dir
            .join("workshop/run/app/content/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl")
            .display()
            .to_string())
    );
    // the mock cleared dirty on save; nothing else was sent
    assert_eq!(mb.method_calls("modeldoc.save").len(), 1);
}

#[tokio::test]
async fn save_of_a_dirty_document_saves() {
    let host = Host::new("save-dirty");
    let mb = mock(make_model_doc(DOC_WIN, true), true).await;
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let p = call(&server, "model_save", json!({})).await.unwrap();
    assert_eq!(p["saved"], json!(true));
    assert_eq!(p["skippedClean"], json!(false));
    // the doc went clean
    mb.with_state(|s| {
        assert_eq!(s.model_doc.as_ref().unwrap()["dirty"], json!(false));
    });
}

#[tokio::test]
async fn save_refuses_outside_addon_paths() {
    // both a mapped-but-core path and an unmapped one must refuse without sending
    for (tag, win) in [
        ("save-core", "Z:\\app\\content\\csgo\\models\\core.vmdl"),
        ("save-unmapped", "q:\\nowhere\\x.vmdl"),
    ] {
        let host = Host::new(tag);
        let mb = mock(make_model_doc(win, false), true).await;
        let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
        let e = call(&server, "model_save", json!({})).await.unwrap_err();
        assert_eq!(e.name, "ReadOnlyModel", "{tag}: {e:?}");
        assert!(e.retry_safe);
        assert!(mb.method_calls("modeldoc.save").is_empty());
    }
}

#[tokio::test]
async fn save_without_a_document_is_no_active_document() {
    let host = Host::new("save-nodoc");
    let mb = MockBridge::builder().modeldoc().start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let e = call(&server, "model_save", json!({})).await.unwrap_err();
    assert_eq!(e.code, Some(-32003));
    assert_eq!(e.wire_name.as_deref(), Some("NoActiveDocument"));
    assert!(mb.method_calls("modeldoc.save").is_empty());
}

// ---------------------------------------------------------------- model_node_types

#[tokio::test]
async fn node_types_are_sorted_and_deduped() {
    let host = Host::new("types-sorted");
    let mb = MockBridge::builder()
        .modeldoc()
        .node_types(vec![
            "RenderMeshList".into(),
            "RootNode".into(),
            "AttachmentList".into(),
            "RootNode".into(),
        ])
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let p = call(&server, "model_node_types", json!({})).await.unwrap();
    assert_eq!(p["count"], json!(4));
    assert_eq!(
        p["classes"],
        json!(["AttachmentList", "RenderMeshList", "RootNode"])
    );
    assert!(!host.snapshot().exists(), "no snapshot without writeSnapshot");
    assert!(modeldoc_deadlines(&mb).iter().all(|d| *d > 0));
}

#[tokio::test]
async fn node_types_snapshot_is_written_atomically_under_the_temp_tree() {
    let host = Host::new("types-snapshot");
    let mb = MockBridge::builder()
        .modeldoc()
        .game_build("2000930")
        .node_types(vec!["RootNode".into(), "AttachmentList".into(), "RootNode".into()])
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    let p = call(
        &server,
        "model_node_types",
        json!({"writeSnapshot": true}),
    )
    .await
    .unwrap();
    let snapshot = host.snapshot();
    assert_eq!(p["snapshotPath"], json!(snapshot.display().to_string()));
    let text = std::fs::read_to_string(&snapshot).unwrap();
    assert!(text.ends_with("}\n"), "trailing newline: {text:?}");
    let json: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["build"], json!("2000930"));
    assert_eq!(json["classes"], json!(["AttachmentList", "RootNode"]));
}

// ---------------------------------------------------------------- capability gating

#[tokio::test]
async fn gated_tools_without_the_module_answer_not_implemented_and_send_nothing() {
    let host = Host::new("gating");
    let mb = mock(make_model_doc(DOC_WIN, false), false).await;
    let server = hammer_mcp::build_server(&config(&mb.discovery_path, &host.dir)).unwrap();
    for (tool, args, first_missing) in [
        ("model_open", json!({"path": "csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl"}), "modeldoc.open"),
        ("model_reload", json!({}), "modeldoc.reload"),
        ("model_save", json!({}), "modeldoc.save"),
        ("model_node_types", json!({}), "modeldoc.nodeTypes"),
    ] {
        let e = call(&server, tool, args).await.unwrap_err();
        assert_eq!(e.name, "NotImplemented", "{tool}");
        assert_eq!(e.code, Some(-32011), "{tool}");
        assert_eq!(e.data["missing"].as_array().unwrap()[0], json!(first_missing));
        assert!(e.retry_safe, "{tool}");
    }
    for m in ["modeldoc.open", "modeldoc.reload", "modeldoc.save", "modeldoc.nodeTypes"] {
        assert!(mb.method_calls(m).is_empty(), "{m}");
    }
}
