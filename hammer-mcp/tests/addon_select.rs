//! WP1.1: the addon follows the active Hammer document, `set_addon` overrides it, and the
//! addon-scoped caches and the ModelDoc guard respect the selection. Everything runs through the
//! real server (`McpServer::call`, so the pre-call refresh runs) against the mock bridge and the
//! synthetic [`common::SoundTree`] (addon `mcp_test`) plus a second addon `other` that carries a
//! different soundevent.
#![allow(clippy::result_large_err)]

mod common;

use common::SoundTree;
use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::PathBuf;

const OTHER_SOURCE: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{\n\t\"other.only\" =\n\t{\n\t\ttype = \"csgo_mega\"\n\t\tvolume = 0.5\n\t}\n}\n";

/// `SoundTree` (addon `mcp_test`) plus the addon `other`, whose only soundevent is `other.only`.
fn tree_with_other(tag: &str) -> SoundTree {
    let tree = SoundTree::new(tag);
    let dir = tree.app.join("content/csgo_addons/other/soundevents");
    std::fs::create_dir_all(&dir).expect("other addon dir");
    std::fs::write(dir.join("soundevents_addon.vsndevts"), OTHER_SOURCE).expect("other source");
    std::fs::create_dir_all(tree.app.join("content/csgo_addons/other/maps")).expect("maps dir");
    tree
}

fn doc_path(addon: &str) -> String {
    format!("Z:\\app\\content\\csgo_addons\\{addon}\\maps\\x.vmap")
}

fn config(tree: &SoundTree, discovery: Option<String>, extra: &[&str]) -> CommonConfig {
    config_with(tree, discovery, extra, 45000, &tree.app)
}

/// [`config`] with the tool budget and the directory the `z:\app` path-map prefix points at.
fn config_with(
    tree: &SoundTree,
    discovery: Option<String>,
    extra: &[&str],
    budget_ms: u64,
    map_dir: &std::path::Path,
) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let map = format!("z:\\app={}", map_dir.display());
    let budget = budget_ms.to_string();
    let mut given: Vec<&str> = vec![
        "--deadline-ms", "5000", "--tool-budget-ms", &budget,
        "--project-root", &root, "--path-map", &map,
    ];
    let disc;
    if let Some(d) = discovery {
        disc = d;
        given.extend(["--discovery", &disc]);
    }
    given.extend_from_slice(extra);
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

async fn call(server: &McpServer<HammerState>, name: &str, args: Value) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

fn event_names(out: &Value) -> Vec<String> {
    out["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect()
}

async fn server_with_doc(tree: &SoundTree, doc_addon: &str) -> (MockBridge, McpServer<HammerState>) {
    let mb = MockBridge::builder()
        .doc(Some(make_doc(&doc_path(doc_addon), Some(false))))
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(
        tree,
        Some(mb.discovery_path.display().to_string()),
        &["--addon", "mcp_test"],
    ))
    .unwrap();
    (mb, server)
}

#[tokio::test]
async fn a_file_route_follows_the_documents_addon_not_the_flag() {
    let tree = tree_with_other("follow");
    let (_mb, server) = server_with_doc(&tree, "other").await;

    let out = call(&server, "list_soundevents", json!({"source": "addon"})).await.unwrap();
    let names = event_names(&out);
    assert!(names.contains(&"other.only".to_string()), "{names:?}");
    assert!(!names.contains(&"mcp.src.only".to_string()), "flag addon must not leak: {names:?}");

    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["addon"]["name"], json!("other"));
    assert_eq!(status["addon"]["source"], json!("document"));
    assert_eq!(status["addon"]["flag"], json!("mcp_test"));
    assert_eq!(status["document"]["documentAddon"], json!("other"));
}

#[tokio::test]
async fn set_addon_overrides_and_null_returns_to_the_document() {
    let tree = tree_with_other("override");
    let (mb, server) = server_with_doc(&tree, "other").await;

    // follow the document first: this fills the soundevent cache for `other`
    let out = call(&server, "list_soundevents", json!({"source": "addon"})).await.unwrap();
    assert!(event_names(&out).contains(&"other.only".to_string()));

    let set = call(&server, "set_addon", json!({"addon": "mcp_test"})).await.unwrap();
    assert_eq!(set["addon"], json!("mcp_test"));
    assert_eq!(set["source"], json!("set_addon"));
    assert_eq!(set["previous"], json!({"addon": "other", "source": "document"}));

    // the cache filled for `other` is not served for `mcp_test`
    let out = call(&server, "list_soundevents", json!({"source": "addon"})).await.unwrap();
    let names = event_names(&out);
    assert!(names.contains(&"mcp.src.only".to_string()), "{names:?}");
    assert!(!names.contains(&"other.only".to_string()), "stale cache across addons: {names:?}");
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["addon"]["source"], json!("set_addon"));
    assert_eq!(status["addon"]["pinned"], json!("mcp_test"));
    assert_eq!(status["addon"]["documentAddon"], json!("other"));

    // no argument: report only
    let report = call(&server, "set_addon", json!({})).await.unwrap();
    assert_eq!(report["addon"], json!("mcp_test"));
    assert_eq!(report["available"], json!(["mcp_test", "other"]));

    // null: follow the document again, and the next read is `other` again
    let back = call(&server, "set_addon", json!({"addon": null})).await.unwrap();
    assert_eq!(back["addon"], json!("other"));
    assert_eq!(back["source"], json!("document"));
    assert_eq!(back["previous"], json!({"addon": "mcp_test", "source": "set_addon"}));
    let out = call(&server, "list_soundevents", json!({"source": "addon"})).await.unwrap();
    let names = event_names(&out);
    assert!(names.contains(&"other.only".to_string()) && !names.contains(&"mcp.src.only".to_string()));

    // the document switches addon: the next call follows it without any set_addon
    mb.set_doc(Some(make_doc(&doc_path("mcp_test"), Some(false))));
    let status = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(status["addon"]["name"], json!("mcp_test"));
    assert_eq!(status["addon"]["source"], json!("document"));
}

#[tokio::test]
async fn set_addon_refuses_an_unknown_addon_with_suggestions_and_changes_nothing() {
    let tree = tree_with_other("unknown");
    let (_mb, server) = server_with_doc(&tree, "other").await;
    call(&server, "set_addon", json!({"addon": "mcp_test"})).await.unwrap();

    let err = call(&server, "set_addon", json!({"addon": "othr"})).await.unwrap_err();
    assert_eq!(err.name, "AddonNotFound");
    assert_eq!(err.data["didYouMean"][0], json!("other"), "{:?}", err.data);
    assert_eq!(err.data["available"], json!(["mcp_test", "other"]));
    assert!(err.message.contains("unchanged"), "{}", err.message);

    let err = call(&server, "set_addon", json!({"addon": "../x"})).await.unwrap_err();
    assert_eq!(err.name, "AddonNotFound");
    let err = call(&server, "set_addon", json!({"addon": 7})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");

    let still = call(&server, "set_addon", json!({})).await.unwrap();
    assert_eq!(still["addon"], json!("mcp_test"), "the pin survived the failed calls");
    assert_eq!(still["source"], json!("set_addon"));
}

/// A fake podman: `ps` finds the ModelDoc container, `inspect` reports the addon it was started with.
fn fake_podman(tree: &SoundTree, addon_line: &str) -> PathBuf {
    let bin = tree.root.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let script = bin.join("podman");
    let body = format!(
        "#!/bin/bash\ncase \"$1\" in\n  ps) printf '[{{\"Names\":[\"cs2tools\"],\"State\":\"running\"}}]' ;;\n  inspect) echo '{addon_line}' ;;\n  *) echo \"fake podman: unsupported $1\" >&2; exit 2 ;;\nesac\n"
    );
    std::fs::write(&script, body).expect("script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    script
}

fn create_args(addon: &str) -> Value {
    json!({
        "path": format!("csgo_addons/{addon}/models/mcp_new/probe.vmdl"),
        "template": "static_prop",
        "params": {"meshFile": "models/probe/mesh.fbx"},
    })
}

const LAUNCHED_ON_OTHER: &str = "|[\"/bin/bash\",\"-c\",\"cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -tools -addon other +x\"]";

/// A server with a mock bridge that has ModelDoc (it opens files under `mcp_test`, like the real
/// runtime started with `-addon mcp_test`) and a fake podman.
async fn modeldoc_server(tree: &SoundTree, podman: &std::path::Path, flag: &str) -> (MockBridge, McpServer<HammerState>) {
    let mb = MockBridge::builder().modeldoc().start().await.unwrap();
    let podman = podman.display().to_string();
    let server = hammer_mcp::build_server(&config(
        tree,
        Some(mb.discovery_path.display().to_string()),
        &["--addon", flag, "--podman", &podman],
    ))
    .unwrap();
    (mb, server)
}

/// Write a minimal valid `.vmdl` into `addon` and return its `csgo_addons/...` path.
fn seed_vmdl(tree: &SoundTree, addon: &str) -> String {
    let rel = "models/mcp_new/probe.vmdl";
    let host = tree.app.join("content/csgo_addons").join(addon).join(rel);
    std::fs::create_dir_all(host.parent().unwrap()).expect("models dir");
    std::fs::write(&host, "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:modeldoc29:version{3cec427c-1b0e-4d48-a90c-0036f42c786e} -->\n{\n\trootNode = {\n\t\t_class = \"RootNode\"\n\t\tchildren = []\n\t}\n}\n").expect("vmdl");
    format!("csgo_addons/{addon}/{rel}")
}

#[tokio::test]
async fn a_vmdl_write_is_allowed_with_a_warning_when_modeldoc_runs_on_another_addon() {
    let tree = tree_with_other("modeldoc");
    let podman = fake_podman(&tree, LAUNCHED_ON_OTHER);
    let (_mb, server) = modeldoc_server(&tree, &podman, "mcp_test").await;

    // the file write is correct for the selected addon, so it is NOT refused ...
    let out = call(&server, "model_create", create_args("mcp_test")).await.unwrap();
    assert_eq!(out["written"], json!(true));
    assert!(tree.app.join("content/csgo_addons/mcp_test/models/mcp_new/probe.vmdl").is_file());
    // ... the result says the running ModelDoc will not show it
    let warnings = out["warnings"].to_string();
    assert!(warnings.contains("ModelDoc") && warnings.contains("other") && warnings.contains("runtime.sh"), "{warnings}");
    assert!(!warnings.contains("set_addon"), "the hint is to restart ModelDoc: {warnings}");

    // model_edit warns the same way (the file written is the one asked for)
    let edit = call(
        &server,
        "model_edit",
        json!({
            "path": "csgo_addons/mcp_test/models/mcp_new/probe.vmdl",
            "ops": [{"op": "set_attr", "path": "/", "key": "model_archetype", "value": "prop"}],
            "classCheck": "off",
        }),
    )
    .await
    .unwrap();
    assert_eq!(edit["written"], json!(true));
    assert!(edit["warnings"].to_string().contains("ModelDoc"), "{edit}");
}

#[tokio::test]
async fn model_open_is_refused_when_modeldoc_runs_on_another_addon() {
    let tree = tree_with_other("modeldoc-open");
    let podman = fake_podman(&tree, LAUNCHED_ON_OTHER);
    let (mb, server) = modeldoc_server(&tree, &podman, "mcp_test").await;
    let path = seed_vmdl(&tree, "mcp_test");

    let err = call(&server, "model_open", json!({"path": path})).await.unwrap_err();
    assert_eq!(err.name, "AddonMismatch");
    assert_eq!(err.data["selectedAddon"], json!("mcp_test"));
    assert_eq!(err.data["modeldocAddon"], json!("other"));
    let hint = err.data["hint"].as_str().unwrap();
    assert!(hint.contains("runtime.sh") && hint.contains("ADDON=mcp_test"), "{hint}");
    // useInstead is the common {tool, why, args} object, never a shell command
    assert_eq!(err.data["useInstead"]["tool"], json!("set_addon"));
    assert_eq!(err.data["useInstead"]["args"], json!({"addon": "other"}));
    assert!(err.message.contains("chosen by"), "names how the addon was chosen: {}", err.message);
    assert!(mb.method_calls("modeldoc.open").is_empty(), "nothing was sent to the editor");
}

#[tokio::test]
async fn the_modeldoc_guard_reads_the_launch_label_first() {
    let tree = tree_with_other("modeldoc-label");
    let podman = fake_podman(&tree, "other|null");
    let (_mb, server) = modeldoc_server(&tree, &podman, "mcp_test").await;
    let path = seed_vmdl(&tree, "mcp_test");
    let err = call(&server, "model_open", json!({"path": path})).await.unwrap_err();
    assert_eq!(err.name, "AddonMismatch");
    assert_eq!(err.data["modeldocAddon"], json!("other"));
}

#[tokio::test]
async fn the_modeldoc_guard_passes_when_podman_fails_or_the_label_is_missing() {
    // podman exits non-zero on inspect
    let tree = tree_with_other("modeldoc-podman-fails");
    let podman = tree.root.join("bin/podman");
    std::fs::create_dir_all(podman.parent().unwrap()).unwrap();
    std::fs::write(&podman, "#!/bin/bash\ncase \"$1\" in\n  ps) printf '[{\"Names\":[\"cs2tools\"],\"State\":\"running\"}]' ;;\n  *) echo boom >&2; exit 125 ;;\nesac\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (_mb, server) = modeldoc_server(&tree, &podman, "mcp_test").await;
    let path = seed_vmdl(&tree, "mcp_test");
    let out = call(&server, "model_open", json!({"path": path})).await.unwrap();
    assert_eq!(out["opened"], json!(true), "an unreadable container addon contradicts nothing");

    // neither a label nor an -addon in the launch command
    let tree = tree_with_other("modeldoc-no-label");
    let podman = fake_podman(&tree, "<no value>|null");
    let (_mb, server) = modeldoc_server(&tree, &podman, "mcp_test").await;
    let path = seed_vmdl(&tree, "mcp_test");
    let out = call(&server, "model_open", json!({"path": path})).await.unwrap();
    assert_eq!(out["opened"], json!(true));
    assert_eq!(out["warnings"], json!([]));
}

#[tokio::test]
async fn model_open_checks_that_the_editor_opened_the_requested_file() {
    let tree = tree_with_other("modeldoc-opened");
    // no podman at all (the guard passes); the mock editor resolves every open under mcp_test,
    // like a ModelDoc started on mcp_test, while the selected addon is `other`
    let (mb, server) = modeldoc_server(&tree, std::path::Path::new("/nonexistent/podman"), "other").await;
    let path = seed_vmdl(&tree, "other");
    call(&server, "set_addon", json!({"addon": "other"})).await.unwrap();
    let err = call(&server, "model_open", json!({"path": path})).await.unwrap_err();
    assert_eq!(err.name, "WrongDocumentOpened");
    assert!(err.data["opened"].as_str().unwrap().contains("csgo_addons/mcp_test"), "{:?}", err.data);
    assert!(err.data["requested"].as_str().unwrap().contains("csgo_addons/other"), "{:?}", err.data);
    assert_eq!(mb.method_calls("modeldoc.open").len(), 1);

    // the same open on the matching addon verifies cleanly
    let path = seed_vmdl(&tree, "mcp_test");
    call(&server, "set_addon", json!({"addon": "mcp_test"})).await.unwrap();
    assert_eq!(mb.method_calls("modeldoc.open").len(), 1);
    let out = call(&server, "model_open", json!({"path": path})).await.unwrap();
    assert_eq!(out["opened"], json!(true));
    assert_eq!(out["warnings"], json!([]));
}

fn message_names(err: &ToolError, addon: &str) -> bool {
    let text = format!("{} {:?}", err.message, err.data);
    text.contains(&format!("csgo_addons/{addon}"))
}

#[tokio::test]
async fn a_tool_outside_the_old_include_list_follows_the_document_on_a_fresh_server() {
    let tree = tree_with_other("outside-list");
    let (mb, server) = server_with_doc(&tree, "other").await;
    // list_entities {path} was never in the hand-kept list: its very first call (fresh server,
    // no session_status before it) must already resolve in the document's addon
    let err = call(&server, "list_entities", json!({"path": "maps/missing.vmap"})).await.unwrap_err();
    assert!(message_names(&err, "other"), "resolved in the wrong addon: {err:?}");
    assert!(!message_names(&err, "mcp_test"), "{err:?}");

    // and right after a document switch, with no throttle window to hide behind
    mb.set_doc(Some(make_doc(&doc_path("mcp_test"), Some(false))));
    let err = call(&server, "list_entities", json!({"path": "maps/missing.vmap"})).await.unwrap_err();
    assert!(message_names(&err, "mcp_test"), "stale addon after the switch: {err:?}");
}

#[tokio::test]
async fn a_file_tool_called_right_after_a_document_switch_sees_the_new_addon() {
    let tree = tree_with_other("switch");
    let (mb, server) = server_with_doc(&tree, "other").await;
    let out = call(&server, "list_soundevents", json!({"source": "addon"})).await.unwrap();
    assert!(event_names(&out).contains(&"other.only".to_string()));
    for (addon, expect, absent) in [
        ("mcp_test", "mcp.src.only", "other.only"),
        ("other", "other.only", "mcp.src.only"),
        ("mcp_test", "mcp.src.only", "other.only"),
    ] {
        mb.set_doc(Some(make_doc(&doc_path(addon), Some(false))));
        // immediately: list_soundevents, not session_status (which re-reads the document itself)
        let out = call(&server, "list_soundevents", json!({"source": "addon"})).await.unwrap();
        let names = event_names(&out);
        assert!(names.contains(&expect.to_string()), "{addon}: {names:?}");
        assert!(!names.contains(&absent.to_string()), "{addon}: stale: {names:?}");
    }
}

#[tokio::test]
async fn a_tool_waits_for_the_bridge_lock_instead_of_skipping_the_addon_check() {
    let tree = tree_with_other("lock");
    let (mb, server) = server_with_doc(&tree, "other").await;
    let server = std::sync::Arc::new(server);
    call(&server, "list_soundevents", json!({"source": "addon"})).await.unwrap();
    mb.set_doc(Some(make_doc(&doc_path("mcp_test"), Some(false))));
    // another call's bridge sequence holds the lock when the next call starts
    let held = server.state().bridge.clone();
    let guard = held.lock().await;
    let pending = {
        let server = server.clone();
        tokio::spawn(async move { call(&server, "list_soundevents", json!({"source": "addon"})).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!pending.is_finished(), "the call must wait for the lock, not skip the check");
    drop(guard);
    let out = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
        .await
        .expect("finishes once the lock is free")
        .expect("task")
        .unwrap();
    let names = event_names(&out);
    assert!(names.contains(&"mcp.src.only".to_string()), "{names:?}");
    assert!(!names.contains(&"other.only".to_string()), "checked too late: {names:?}");
}

#[tokio::test]
async fn compile_map_and_run_map_targets_follow_the_document() {
    let tree = tree_with_other("targets");
    // the active document is maps/x.vmap of `other`; the file is not saved on disk, so both
    // tools stop at the file check and name the addon root they looked in
    let (mb, server) = server_with_doc(&tree, "other").await;
    let err = call(&server, "compile_map", json!({})).await.unwrap_err();
    assert!(message_names(&err, "other") && !message_names(&err, "mcp_test"), "{err:?}");
    let err = call(&server, "run_map", json!({})).await.unwrap_err();
    assert!(message_names(&err, "other") && !message_names(&err, "mcp_test"), "{err:?}");

    mb.set_doc(Some(make_doc(&doc_path("mcp_test"), Some(false))));
    let err = call(&server, "compile_map", json!({})).await.unwrap_err();
    assert!(message_names(&err, "mcp_test") && !message_names(&err, "other"), "{err:?}");
    let err = call(&server, "run_map", json!({})).await.unwrap_err();
    assert!(message_names(&err, "mcp_test") && !message_names(&err, "other"), "{err:?}");
}

#[tokio::test]
async fn every_exempt_name_is_a_registered_tool_and_the_addon_tools_are_not_exempt() {
    let tree = tree_with_other("exempt");
    let (_mb, server) = server_with_doc(&tree, "other").await;
    let names: Vec<&str> = server.registry().names();
    for exempt in hammer_mcp::state::ADDON_EXEMPT_TOOLS {
        assert!(names.contains(exempt), "{exempt} is exempt but not a registered tool (typo?)");
    }
    // the tools that resolve paths/catalogs/indexes in the addon get the pre-call check
    for scoped in [
        "list_soundevents", "edit_soundevents", "compile_soundevents", "create_sound_entities",
        "list_models", "get_model_info", "list_props", "compile_map", "run_map", "compile_asset",
        "open_map", "save_map", "reload_map", "model_open", "model_edit", "particle_compile",
    ] {
        assert!(!hammer_mcp::state::ADDON_EXEMPT_TOOLS.contains(&scoped), "{scoped} must not be exempt");
    }
}

#[tokio::test]
async fn a_non_exempt_mutator_outside_the_old_list_follows_the_document_via_before_call() {
    let tree = tree_with_other("sound-entities");
    let (mb, server) = server_with_doc(&tree, "other").await;
    // create_sound_entities checks the sound against the selected addon's index; `other.only`
    // exists only in `other`. On a fresh server (flag = mcp_test, nothing read yet) the call
    // succeeds only if before_call already switched to the document's addon.
    let result = call(
        &server,
        "create_sound_entities",
        json!({"entities": [{"kind": "point", "soundEvent": "other.only", "targetname": "snd_x", "origin": [0, 0, 0]}]}),
    )
    .await;
    // the mock has no edit.batch responder: reaching it IS the pass (the sound check is first)
    assert_eq!(mb.method_calls("edit.batch").len(), 1, "refused before writing: {result:?}");
}

/// A finished compile job of addon `addon` for `maps/x.vmap`, whose source map (in that addon)
/// is newer than the job's start.
fn insert_finished_job(server: &McpServer<HammerState>, addon: &str) {
    use hammer_mcp::tools::compile_map::{CompileJob, CompileMode, JobState};
    server.state().compile_jobs.insert(CompileJob {
        id: "c1".into(),
        tag: "t-c1".into(),
        state: JobState::Ok,
        mode: CompileMode::Full,
        path: "maps/x.vmap".into(),
        addon: addon.into(),
        container: "cs2tools".into(),
        log: PathBuf::from("/nonexistent/compile.log"),
        vpk: vec![],
        vmap_before: Some(std::time::UNIX_EPOCH),
        started: std::time::Instant::now(),
        deadline: std::time::Duration::from_secs(1500),
        attempts: 1,
        summary: None,
        ok_line: None,
        reason: None,
        cleanup: None,
    });
}

#[tokio::test]
async fn a_compile_job_polls_in_its_own_addon_after_the_document_switched() {
    let tree = tree_with_other("job-addon");
    // the source map exists only in `other`
    std::fs::write(tree.app.join("content/csgo_addons/other/maps/x.vmap"), "x").unwrap();
    let (mb, server) = server_with_doc(&tree, "other").await;
    insert_finished_job(&server, "other");

    // a poll by id skips the pre-call check: read the document's addon first
    call(&server, "list_soundevents", json!({"source": "addon"})).await.unwrap();
    // same addon: the job's own steps, and its source is seen as changed (looked up in `other`)
    let out = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap();
    assert_eq!(out["sourceChanged"], json!(true), "{out}");
    assert_eq!(out["next"][0]["tool"], json!("compile_map"), "{out}");
    assert_eq!(out["next"][0]["args"], json!({"path": "maps/x.vmap"}));

    // the document moves to the other addon: the job still reads `other`'s files, and its
    // addon-relative next steps are replaced by a set_addon that selects the job's addon first
    mb.set_doc(Some(make_doc(&doc_path("mcp_test"), Some(false))));
    call(&server, "list_soundevents", json!({"source": "addon"})).await.unwrap();
    assert_eq!(server.state().addon(), "mcp_test");
    let out = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap();
    assert_eq!(out["sourceChanged"], json!(true), "the job's addon, not the current one: {out}");
    let next = out["next"].as_array().unwrap();
    assert_eq!(next.len(), 1, "{out}");
    assert_eq!(next[0]["tool"], json!("set_addon"));
    assert_eq!(next[0]["args"], json!({"addon": "other"}));
    assert!(next[0]["why"].as_str().unwrap().contains("maps/x.vmap"), "{out}");
}

#[tokio::test]
async fn model_open_accepts_a_symlinked_addon_path() {
    let tree = tree_with_other("symlink");
    // the editor reports its paths through a symlink to the app tree: another spelling of the
    // same file, which a display-string comparison would call a different document
    let link = tree.root.join("applink");
    std::os::unix::fs::symlink(&tree.app, &link).expect("symlink");
    let mb = MockBridge::builder().modeldoc().start().await.unwrap();
    let server = hammer_mcp::build_server(&config_with(
        &tree,
        Some(mb.discovery_path.display().to_string()),
        &["--addon", "mcp_test"],
        45000,
        &link,
    ))
    .unwrap();
    let path = seed_vmdl(&tree, "mcp_test");
    let out = call(&server, "model_open", json!({"path": path})).await.unwrap();
    assert_eq!(out["opened"], json!(true));
    assert_eq!(out["warnings"], json!([]));
}

#[tokio::test]
async fn a_wrong_document_opened_is_not_retry_safe() {
    let tree = tree_with_other("wrongdoc-retry");
    let (_mb, server) = modeldoc_server(&tree, std::path::Path::new("/nonexistent/podman"), "other").await;
    let path = seed_vmdl(&tree, "other");
    call(&server, "set_addon", json!({"addon": "other"})).await.unwrap();
    let err = call(&server, "model_open", json!({"path": path})).await.unwrap_err();
    assert_eq!(err.name, "WrongDocumentOpened");
    assert!(!err.retry_safe, "the editor already switched documents: {err:?}");
}

#[tokio::test]
async fn a_lock_wait_timeout_refuses_a_mutator_and_lets_a_read_proceed_with_a_note() {
    let tree = tree_with_other("lock-timeout");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(&doc_path("other"), Some(false))))
        .start()
        .await
        .unwrap();
    // a 1.5 s tool budget bounds the pre-call lock wait (the default wait is 10 s)
    let server = hammer_mcp::build_server(&config_with(
        &tree,
        Some(mb.discovery_path.display().to_string()),
        &["--addon", "mcp_test"],
        1500,
        &tree.app,
    ))
    .unwrap();
    // another call's bridge sequence holds the lock for the whole test
    let bridge = server.state().bridge.clone();
    let guard = bridge.lock().await;

    let started = std::time::Instant::now();
    let err = call(&server, "edit_soundevents", json!({"ops": []})).await.unwrap_err();
    assert_eq!(err.name, "AddonCheckFailed", "{err:?}");
    assert!(err.retry_safe, "nothing ran: {err:?}");
    assert!(err.message.contains("could not be confirmed") && err.message.contains("retry"), "{}", err.message);
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "the wait is clamped to the budget");

    // a read-only file tool runs on the last known addon and says so
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        call(&server, "list_soundevents", json!({"source": "addon"})),
    )
    .await
    .expect("does not hang")
    .unwrap();
    assert!(out["addonNote"].as_str().unwrap().contains("could not be confirmed"), "{out}");
    drop(guard);
}

#[tokio::test]
async fn file_writers_report_the_addon_they_wrote_in() {
    let tree = tree_with_other("writer-addon");
    let podman = fake_podman(&tree, LAUNCHED_ON_OTHER);
    let (_mb, server) = modeldoc_server(&tree, &podman, "mcp_test").await;
    let out = call(&server, "model_create", create_args("mcp_test")).await.unwrap();
    assert_eq!(out["addon"], json!("mcp_test"));
    assert_eq!(out["addonSource"], json!(server.state().addon_selection().1.as_str()));
}

#[tokio::test]
async fn set_addon_is_annotated_idempotent_and_not_destructive() {
    let tree = tree_with_other("annotations");
    let (_mb, server) = server_with_doc(&tree, "other").await;
    let spec = server.registry().spec("set_addon").expect("registered");
    assert_eq!(spec.idempotent, Some(true));
    assert_eq!(spec.destructive, Some(false));
}
