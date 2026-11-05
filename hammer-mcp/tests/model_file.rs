//! `model_file` tool tests (WP5): read/list/edit/create/compile against a temp content tree (under
//! `CARGO_TARGET_TMPDIR`, never /tmp) and the mock bridge for the editor compile route. The
//! resourcecompiler route runs through a fake podman script; the mock supplies canned
//! `OK: N compiled…`/FATAL output and touches (or not) the `.vmdl_c` the tool then stats.
//!
//! Helpers are local copies of the pattern in `tests/server.rs` (`config()`, `call`) — the
//! shared `tests/common/` module is reused as-is (its hermetic defaults keep podman/layer/xauth
//! off the host).
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_model_doc, MockBridge};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use hammer_mcp::tools::model_file::parse_compile_output;
use hammer_mcp::vmdl::TEMPLATE_CLASSES;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A temp project tree laid out like the real one: `--project-root` at `root`, app dir at
/// `workshop/run/app`, addon content under its `content/csgo_addons/mcp_test`.
struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "model-file-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("tmp tree");
        let app = root.join("workshop/run/app");
        Tree { root, app }
    }

    fn addon(&self) -> PathBuf {
        self.app.join("content/csgo_addons/mcp_test")
    }

    /// Copy a synthetic fixture from `hammer-mcp/fixtures/vmdl/` into the addon at `rel`.
    fn install(&self, fixture: &str, rel: &str) -> PathBuf {
        let target = self.addon().join(rel);
        std::fs::create_dir_all(target.parent().expect("rel has a parent")).expect("install dir");
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/vmdl").join(fixture);
        std::fs::copy(&source, &target).expect("install fixture");
        target
    }

    /// The path `model_compile` at addin-relative `rel` must report as `outputPath`
    /// (`<app>/game/csgo_addons/mcp_test/<rel>_c`, same rule `ContentPaths::compiled_output` uses).
    fn compile_output(&self, rel: &str) -> PathBuf {
        self.app
            .join("game")
            .join(format!("csgo_addons/mcp_test/{rel}_c"))
    }

    /// Write a node-type snapshot where the server looks for it (`<project>/hammer-mcp/research/…`).
    fn write_snapshot(&self, classes: &[&str]) {
        let dir = self.root.join("hammer-mcp/research");
        std::fs::create_dir_all(&dir).expect("snapshot dir");
        let classes: Vec<Value> = classes.iter().map(|c| json!(c)).collect();
        std::fs::write(
            dir.join("modeldoc-nodetypes.json"),
            json!({"build": "test", "classes": classes}).to_string(),
        )
        .expect("snapshot file");
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A fake `podman` binary under the tree (mode 0755). Kinds:
/// * `ok` — `ps` reports the ModelDoc container; `exec` maps `-i /app/content/<rel>` to
///   `<app>/game/<rel>_c`, touches it and prints the OK summary;
/// * `fail` — prints a `[FAIL]` line plus a `FATAL ERROR` and does NOT touch the output;
/// * `skipped` — prints `OK: 0 compiled, 0 failed, 1 skipped` and does NOT touch the output;
/// * `no-container` — `ps` reports nothing (label not found).
fn fake_podman(tree: &Tree, kind: &str) -> PathBuf {
    let bin_dir = tree.root.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    let body = match kind {
        "ok" => r#"#!/bin/bash
APP='__APP__'
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    last="${!#}"
    rel="${last##* -i }"
    case "$rel" in
      /app/content/*) rel="${rel#/app/content/}" ;;
      *) echo "fake podman: unexpected command: $last" >&2
         exit 3 ;;
    esac
    out="$APP/game/${rel}_c"
    mkdir -p "$(dirname "$out")"
    touch "$out"
    echo "Compiling $rel"
    echo "(compiler noise goes to stderr)" >&2
    echo "OK: 1 compiled, 0 failed, 0 skipped, 0m:01s"
    ;;
  *)
    echo "fake podman: unsupported verb $1" >&2
    exit 2
    ;;
esac
"#,
        "fail" => r#"#!/bin/bash
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    echo "[FAIL] dependency failed: materials/missing/missing.vmat"
    echo "FATAL ERROR: unable to load gameinfo.gi (bad -game path)" >&2
    exit 1
    ;;
esac
"#,
        "skipped" => r#"#!/bin/bash
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    echo "OK: 0 compiled, 0 failed, 1 skipped, 0m:00s"
    ;;
esac
"#,
        "no-container" => r#"#!/bin/bash
case "$1" in
  ps) echo '[]' ;;
  *) echo "fake podman: unsupported verb $1" >&2; exit 2 ;;
esac
"#,
        other => panic!("unknown fake podman kind {other}"),
    }
    .replace("__APP__", &tree.app.display().to_string());
    let script = bin_dir.join(format!("podman-{kind}"));
    std::fs::write(&script, body).expect("fake podman script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    script
}

fn config(tree: &Tree, discovery: Option<String>, podman: Option<String>, tool_budget_ms: u64) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let budget = tool_budget_ms.to_string();
    let root = tree.root.display().to_string();
    let map = format!("z:\\app={}", tree.app.display());
    let mut args: Vec<String> = vec![
        "--deadline-ms".into(),
        "5000".into(),
        "--tool-budget-ms".into(),
        budget,
        "--project-root".into(),
        root,
        "--path-map".into(),
        map,
    ];
    if let Some(discovery) = discovery {
        args.extend(["--discovery".into(), discovery]);
    }
    if let Some(podman) = podman {
        args.extend(["--podman".into(), podman]);
    }
    let given: Vec<&str> = args.iter().map(String::as_str).collect();
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

async fn call(
    server: &McpServer<HammerState>,
    name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

const PROBE_REL: &str = "models/mcp_probe/probe.vmdl";
/// Addon-relative form of [`PROBE_REL`] (what the tools accept).
const PROBE_ARG: &str = "csgo_addons/mcp_test/models/mcp_probe/probe.vmdl";
/// Windows path the mock's ModelDoc document reports for the probe file.
const PROBE_DOC_PATH: &str = "Z:\\app\\content\\csgo_addons\\mcp_test\\models\\mcp_probe\\probe.vmdl";

// -------------------------------------------------------------------------------------------
// read / list
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn read_returns_the_node_subtree_from_disk() {
    let tree = Tree::new("read");
    let file = tree.install("static_prop.vmdl", PROBE_REL);
    let cfg = config(&tree, None, None, 45_000);
    let server = hammer_mcp::build_server(&cfg).unwrap();

    let out = call(&server, "model_read", json!({"path": PROBE_ARG, "node": "/0/0"}))
        .await
        .unwrap();
    assert_eq!(out["source"], json!("file"));
    assert_eq!(out["hostPath"], json!(file.display().to_string()));
    assert_eq!(out["format"], json!("modeldoc36"));
    assert_eq!(out["node"]["path"], json!("/0/0"));
    assert_eq!(out["node"]["_class"], json!("RenderMeshFile"));
    assert_eq!(out["node"]["name"], json!("body"));
    assert_eq!(
        out["node"]["attrs"]["filename"],
        json!("models/mcp_probe/body.fbx")
    );

    // Windows path form + depth truncation.
    let out = call(
        &server,
        "model_read",
        json!({"path": "z:\\app\\content\\csgo_addons\\mcp_test\\models\\mcp_probe\\probe.vmdl", "depth": 0}),
    )
    .await
    .unwrap();
    assert!(out["node"]["children"].as_array().unwrap().is_empty());
    assert_eq!(out["node"]["_class"], json!("RootNode"));

    // Errors: a dangling node path, and an unmapped drive.
    let err = call(&server, "model_read", json!({"path": PROBE_ARG, "node": "/0/9"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "NodeNotFound");
    assert_eq!(err.data["path"], json!("/0/9"));
    let err = call(&server, "model_read", json!({"path": "q:\\nowhere\\probe.vmdl"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "PathNotMapped");
    assert!(err.retry_safe);
}

#[tokio::test]
async fn list_nodes_reports_the_flat_tree_and_filters() {
    let tree = Tree::new("list");
    tree.install("static_prop.vmdl", PROBE_REL);
    let server = hammer_mcp::build_server(&config(&tree, None, None, 45_000)).unwrap();

    let out = call(&server, "model_list_nodes", json!({"path": PROBE_ARG})).await.unwrap();
    let nodes = out["nodes"].as_array().unwrap();
    assert_eq!(out["count"], json!(nodes.len() as u64));
    assert_eq!(nodes[0]["path"], json!("/"));
    assert_eq!(nodes[0]["_class"], json!("RootNode"));
    assert_eq!(out["source"], json!("file"));
    // RenderMeshList > RenderMeshFile > Mesh Operation Stack, MaterialGroupList, GameDataList.
    assert_eq!(nodes[1]["childCount"], json!(1));
    assert_eq!(nodes[2]["_class"], json!("RenderMeshFile"));
    assert_eq!(nodes[2]["name"], json!("body"));
    assert!(nodes[2]["attrKeys"]
        .as_array()
        .unwrap()
        .contains(&json!("filename")));

    let out = call(
        &server,
        "model_list_nodes",
        json!({"path": PROBE_ARG, "class": "RenderMeshFile"}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1));
    assert_eq!(out["nodes"][0]["path"], json!("/0/0"));
}

// -------------------------------------------------------------------------------------------
// edit
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn edit_writes_with_backup() {
    let tree = Tree::new("edit");
    tree.install("static_prop.vmdl", PROBE_REL);
    let server = hammer_mcp::build_server(&config(&tree, None, None, 45_000)).unwrap();

    let out = call(
        &server,
        "model_edit",
        json!({
            "path": PROBE_ARG,
            "ops": [{"op": "set_attr", "path": "/0/0", "key": "import_scale", "value": 2.0}],
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["written"], json!(true));
    assert_eq!(out["applied"], json!(1));
    assert_eq!(out["nextStep"], "model_reload if the file is open in ModelDoc");
    let backup = out["backup"].as_str().expect("backup path");
    assert!(backup.contains(".bak-"), "{backup}");
    assert!(Path::new(backup).is_file());

    // The file on disk now holds the new value.
    let read = call(&server, "model_read", json!({"path": PROBE_ARG, "node": "/0/0"}))
        .await
        .unwrap();
    assert_eq!(read["node"]["attrs"]["import_scale"], json!(2.0));
}

#[tokio::test]
async fn edit_failure_is_atomic_no_backup_no_change() {
    let tree = Tree::new("edit-atomic");
    let file = tree.install("static_prop.vmdl", PROBE_REL);
    let before = std::fs::read(&file).unwrap();
    let server = hammer_mcp::build_server(&config(&tree, None, None, 45_000)).unwrap();

    // The third op targets a missing node: everything rolls back.
    let err = call(
        &server,
        "model_edit",
        json!({
            "path": PROBE_ARG,
            "ops": [
                {"op": "set_attr", "path": "/0/0", "key": "import_scale", "value": 3.0},
                {"op": "set_attr", "path": "/1/0", "key": "remaps", "value": []},
                {"op": "set_attr", "path": "/1/9", "key": "x", "value": 1},
            ],
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidOp");
    assert_eq!(err.code, Some(-32602));
    assert_eq!(err.data["opIndex"], json!(2));
    assert!(err.retry_safe, "nothing was written");
    assert_eq!(std::fs::read(&file).unwrap(), before, "file byte-identical");
    let bak: Vec<_> = std::fs::read_dir(file.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().into_encoded_bytes().to_vec())
        .collect();
    assert!(
        bak.iter().all(|name| std::str::from_utf8(name).unwrap() == "probe.vmdl"),
        "no backup written: {bak:?}"
    );
}

#[tokio::test]
async fn edit_dry_run_changes_nothing() {
    let tree = Tree::new("edit-dry");
    let file = tree.install("static_prop.vmdl", PROBE_REL);
    let before = std::fs::read(&file).unwrap();
    let server = hammer_mcp::build_server(&config(&tree, None, None, 45_000)).unwrap();

    let out = call(
        &server,
        "model_edit",
        json!({
            "path": PROBE_ARG,
            "dryRun": true,
            "ops": [{"op": "set_attr", "path": "/", "key": "model_archetype", "value": "prop"}],
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["written"], json!(false));
    assert_eq!(out["backup"], Value::Null);
    assert_eq!(out["applied"], json!(1));
    assert_eq!(std::fs::read(&file).unwrap(), before, "nothing on disk");
}

#[tokio::test]
async fn edit_outside_the_addon_is_refused() {
    let tree = Tree::new("edit-outside");
    let server = hammer_mcp::build_server(&config(&tree, None, None, 45_000)).unwrap();
    // Core content is under content/, but not under content/csgo_addons/mcp_test/.
    let core = tree
        .app
        .join("content/csgo/workshop/content_examples")
        .join("breakable_glass");
    std::fs::create_dir_all(&core).unwrap();
    let core_file = core.join("breakable_glass_01.vmdl");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/vmdl/static_prop.vmdl"),
        &core_file,
    )
    .unwrap();
    let err = call(
        &server,
        "model_edit",
        json!({
            "path": "csgo/workshop/content_examples/breakable_glass/breakable_glass_01.vmdl",
            "ops": [{"op": "set_attr", "path": "/", "key": "x", "value": 1}],
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WriteOutsideAddon");
    assert_eq!(err.code, Some(-32602));
    assert!(err.retry_safe);
}

#[tokio::test]
async fn class_check_with_and_without_snapshot() {
    let tree = Tree::new("classcheck");
    tree.install("static_prop.vmdl", PROBE_REL);
    let server = hammer_mcp::build_server(&config(&tree, None, None, 45_000)).unwrap();
    let edit_args = json!({
        "path": PROBE_ARG,
        "ops": [{"op": "set_attr", "path": "/" , "key": "model_archetype", "value": "prop"}],
    });

    // No snapshot in the tree: the built-in snapshot checks the classes (no warning).
    let out = call(&server, "model_edit", edit_args.clone()).await.unwrap();
    assert_eq!(out["written"], json!(true));
    assert_eq!(out["warnings"], json!([]));

    // A snapshot that lacks classes of the file: UnknownClass, nothing verified written.
    tree.write_snapshot(&["RootNode"]);
    let err = call(&server, "model_edit", edit_args.clone()).await.unwrap_err();
    assert_eq!(err.name, "UnknownClass");
    assert!(!err.data["class"].as_str().unwrap().is_empty());
    assert!(err.data["snapshot"].as_str().unwrap().ends_with("modeldoc-nodetypes.json"));
    assert!(err.retry_safe);

    // classCheck off: no snapshot check; the result still notes the opt-out.
    let out = call(
        &server,
        "model_edit",
        json!({"path": PROBE_ARG, "classCheck": "off", "ops": edit_args["ops"].as_array().unwrap().clone()}),
    )
    .await
    .unwrap();
    assert_eq!(out["written"], json!(true));
    let warnings = out["warnings"].as_array().unwrap();
    assert!(warnings.iter().any(|w| {
        w.as_str().unwrap().contains("class names not checked")
    }), "{warnings:?}");

    // A complete snapshot: clean pass.
    let fixture_classes: Vec<&str> = vec![
        "RootNode",
        "RenderMeshList",
        "RenderMeshFile",
        "Mesh Operation Stack",
        "MaterialGroupList",
        "DefaultMaterialGroup",
        "GameDataList",
        "GenericGameData",
    ];
    tree.write_snapshot(&fixture_classes);
    let out = call(&server, "model_edit", edit_args.clone()).await.unwrap();
    assert!(out["warnings"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn bad_arguments_are_invalid_params() {
    let tree = Tree::new("args");
    tree.install("static_prop.vmdl", PROBE_REL);
    let server = hammer_mcp::build_server(&config(&tree, None, None, 45_000)).unwrap();
    for (args, wrong) in [
        (json!({}), "missing path"),
        (json!({"path": PROBE_ARG}), "missing ops"),
        (json!({"path": PROBE_ARG, "ops": []}), "empty ops"),
        (
            json!({"path": PROBE_ARG, "ops": [{"op": "bogus"}]}),
            "unknown op",
        ),
        (
            json!({"path": PROBE_ARG, "classCheck": "maybe", "ops": [{"op": "set_attr", "path": "/", "key": "x", "value": 1}]}),
            "bad classCheck",
        ),
        (
            json!({"path": PROBE_ARG, "depth": -1}),
            "bad depth",
        ),
    ] {
        let err = call(&server, "model_edit" , args.clone()).await.unwrap_err();
        assert_eq!(err.code, Some(-32602), "case {wrong}: {err:?}");
    }
}

// -------------------------------------------------------------------------------------------
// create
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn create_then_read_and_refuse_existing() {
    let tree = Tree::new("create");
    let server = hammer_mcp::build_server(&config(&tree, None, None, 45_000)).unwrap();
    let new_arg = "csgo_addons/mcp_test/models/mcp_new/probe.vmdl";

    let out = call(
        &server,
        "model_create",
        json!({
            "path": new_arg,
            "template": "static_prop",
            "params": {"meshFile": "models/probe/mesh.fbx"},
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["written"], json!(true));
    assert_eq!(out["classesUnverified"], json!(false), "the built-in snapshot verified them");
    assert!(out["backup"].is_null(), "nothing to back up");
    assert!(out["nodes"][0]["_class"] == json!("RootNode"));
    assert!(out["nextStep"].as_str().unwrap().contains("resourcecompiler"));
    let file = tree.addon().join("models/mcp_new/probe.vmdl");
    assert!(file.is_file(), "parent directories were created");

    let read = call(&server, "model_read", json!({"path": new_arg, "node": "/"})).await.unwrap();
    assert_eq!(read["source"], json!("file"));

    // Second create refuses; overwrite is a backup + rewrite.
    let err = call(
        &server,
        "model_create",
        json!({"path": new_arg, "template": "static_prop", "params": {"meshFile": "models/probe/mesh.fbx"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "FileExists");
    assert_eq!(err.code, Some(-32602));
    assert!(err.retry_safe);
    let out = call(
        &server,
        "model_create",
        json!({
            "path": new_arg,
            "template": "static_prop",
            "params": {"meshFile": "models/probe/mesh.fbx"},
            "overwrite": true,
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["written"], json!(true));
    assert!(out["backup"].as_str().expect("overwrite backup").contains(".bak-"));
}

#[tokio::test]
async fn create_with_snapshot_reports_classes_verified() {
    let tree = Tree::new("create-snapshot");
    tree.write_snapshot(TEMPLATE_CLASSES);
    let server = hammer_mcp::build_server(&config(&tree, None, None, 45_000)).unwrap();
    let out = call(
        &server,
        "model_create",
        json!({
            "path": "csgo_addons/mcp_test/models/mcp_new/verified.vmdl",
            "template": "skeletal",
            "params": {"meshFile": "models/probe/mesh.fbx", "animFiles": ["anim/idle.fbx"]},
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["classesUnverified"], json!(false));
    assert!(out["warnings"].as_array().unwrap().is_empty());

    // A template class missing from the snapshot is refused.
    let other = Tree::new("create-snapshot-bad");
    let mut without_anim = TEMPLATE_CLASSES.to_vec();
    remove_matches(&mut without_anim, "AnimFile");
    other.write_snapshot(&without_anim);
    let server = hammer_mcp::build_server(&config(&other, None, None, 45_000)).unwrap();
    let err = call(
        &server,
        "model_create",
        json!({
            "path": "csgo_addons/mcp_test/models/mcp_new/bad.vmdl",
            "template": "skeletal",
            "params": {"meshFile": "models/probe/mesh.fbx", "animFiles": ["anim/idle.fbx"]},
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnknownClass");
    assert_eq!(err.data["class"], json!("AnimFile"));
}

fn remove_matches(slice: &mut Vec<&'static str>, drop: &str) {
    slice.retain(|entry| *entry != drop);
}

// -------------------------------------------------------------------------------------------
// compile: resourcecompiler (fake podman)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn compile_resourcecompiler_ok() {
    let tree = Tree::new("rc-ok");
    tree.install("static_prop.vmdl", PROBE_REL);
    let podman = fake_podman(&tree, "ok");
    let server = hammer_mcp::build_server(&config(&tree, None, Some(podman.display().to_string()), 45_000)).unwrap();

    let out = call(&server, "model_compile", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(out["route"], json!("resourcecompiler"));
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["outputUpdated"], json!(true));
    assert_eq!(out["compiled"], json!(1));
    assert_eq!(out["failed"], json!(0));
    assert_eq!(out["skipped"], json!(0));
    assert_eq!(out["fatal"], json!(false));
    assert_eq!(out["container"], json!("cs2tools"));
    assert_eq!(
        out["outputPath"],
        json!(tree.compile_output(PROBE_REL).display().to_string())
    );
    assert!(out["stdoutTail"].as_str().unwrap().contains("OK: 1 compiled"));
    assert!(tree.compile_output(PROBE_REL).is_file(), "the fake compiler touched it");
}

#[tokio::test]
async fn compile_resourcecompiler_skip_is_ok_only_when_the_output_is_up_to_date() {
    let tree = Tree::new("rc-skip");
    tree.install("static_prop.vmdl", PROBE_REL);
    let podman = fake_podman(&tree, "skipped");
    let server = hammer_mcp::build_server(&config(&tree, None, Some(podman.display().to_string()), 45_000)).unwrap();

    // no output at all: a skip cannot be "up to date"
    let out = call(&server, "model_compile", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["upToDate"], json!(false));

    // an output newer than the source: the skip is a success
    let output = tree.compile_output(PROBE_REL);
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, b"compiled").unwrap();
    let out = call(&server, "model_compile", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["upToDate"], json!(true));
    assert_eq!(out["outputUpdated"], json!(false));
}

#[tokio::test]
async fn compile_resourcecompiler_fail_output_is_a_result_not_an_error() {
    let tree = Tree::new("rc-fail");
    tree.install("static_prop.vmdl", PROBE_REL);
    let podman = fake_podman(&tree, "fail");
    let server = hammer_mcp::build_server(&config(&tree, None, Some(podman.display().to_string()), 45_000)).unwrap();

    let out = call(&server, "model_compile", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["fatal"], json!(true));
    assert_eq!(out["compiled"], Value::Null, "no OK summary");
    assert_eq!(
        out["failLines"],
        json!(["[FAIL] dependency failed: materials/missing/missing.vmat"])
    );
    assert_eq!(out["outputUpdated"], json!(false), "FATAL run never wrote the output");
    assert_eq!(
        out["outputPath"],
        json!(tree.compile_output(PROBE_REL).display().to_string())
    );
}

#[tokio::test]
async fn compile_without_a_modeldoc_container_is_runtime_unavailable() {
    let tree = Tree::new("rc-none");
    tree.install("static_prop.vmdl", PROBE_REL);

    // Hermetic default: podman does not exist at all.
    let server = hammer_mcp::build_server(&config(&tree, None, None, 45_000)).unwrap();
    let err = call(&server, "model_compile", json!({"path": PROBE_ARG})).await.unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable");
    assert_eq!(err.data["label"], json!("cs2mcp.tool=modeldoc"));
    assert!(err.retry_safe);

    // A running podman that finds no container with the label.
    let podman = fake_podman(&tree, "no-container");
    let server = hammer_mcp::build_server(&config(&tree, None, Some(podman.display().to_string()), 45_000)).unwrap();
    let err = call(&server, "model_compile", json!({"path": PROBE_ARG})).await.unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable");
    assert!(err.message.contains("TOOL=modeldoc") || err.message.contains("label"));
}

#[tokio::test]
async fn compile_refuses_a_container_path_that_could_be_shell_code() {
    let tree = Tree::new("rc-inject");
    tree.install("static_prop.vmdl", "models/w$ird/probe.vmdl");
    let podman = fake_podman(&tree, "ok");
    let server = hammer_mcp::build_server(&config(&tree, None, Some(podman.display().to_string()), 45_000)).unwrap();
    let err = call(
        &server,
        "model_compile",
        json!({"path": "csgo_addons/mcp_test/models/w$ird/probe.vmdl"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, Some(-32602));
    assert!(err.message.contains("shell"));
    assert!(
        !tree.compile_output("models/w$ird/probe.vmdl").exists(),
        "no compile ran"
    );
}

// -------------------------------------------------------------------------------------------
// compile: editor route (mock bridge)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn compile_editor_refuses_doc_without_asset() {
    let tree = Tree::new("ed-noasset");
    tree.install("static_prop.vmdl", PROBE_REL);
    let mut doc = make_model_doc(PROBE_DOC_PATH, false);
    doc["hasAsset"] = json!(false);
    let mb = MockBridge::builder()
        .modeldoc()
        .model_doc(Some(doc))
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(
        &tree,
        Some(mb.discovery_path.display().to_string()),
        None,
        45_000,
    ))
    .unwrap();
    let err = call(
        &server,
        "model_compile",
        json!({"path": PROBE_ARG, "route": "editor"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert_eq!(err.code, Some(-32017));
    assert_eq!(err.data["reason"], json!("no_asset"));
    assert!(err.message.contains("resourcecompiler"));
    assert!(err.retry_safe);
    assert!(
        mb.method_calls("modeldoc.compile").is_empty(),
        "refusal must precede the compile"
    );
}

#[tokio::test]
async fn compile_editor_refuses_dirty_without_save_first() {
    let tree = Tree::new("ed-dirty");
    tree.install("static_prop.vmdl", PROBE_REL);
    let output = tree.compile_output(PROBE_REL);
    let mb = MockBridge::builder()
        .modeldoc()
        .model_doc(Some(make_model_doc(PROBE_DOC_PATH, true)))
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(
        &tree,
        Some(mb.discovery_path.display().to_string()),
        None,
        45_000,
    ))
    .unwrap();
    let err = call(
        &server,
        "model_compile",
        json!({"path": PROBE_ARG, "route": "editor"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnsavedChanges");
    assert_eq!(err.code, Some(-32016));
    assert!(err.retry_safe);
    assert!(mb.method_calls("modeldoc.compile").is_empty());
    assert!(!output.exists());

    // saveFirst clears the refusal; the mock stands in for the editor (it writes the output
    // before answering, the live bridge's `savedFirst` arrives with the reply).
    let outp = output;
    let mb = MockBridge::builder()
        .modeldoc()
        .model_doc(Some(make_model_doc(PROBE_DOC_PATH, true)))
        .responder("modeldoc.compile", move |_| {
            if let Some(parent) = outp.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&outp, b"compiled");
            json!({"status": null, "failed": null, "savedFirst": true})
        })
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(
        &tree,
        Some(mb.discovery_path.display().to_string()),
        None,
        45_000,
    ))
    .unwrap();
    let out = call(
        &server,
        "model_compile",
        json!({"path": PROBE_ARG, "route": "editor", "saveFirst": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["route"], json!("editor"));
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["savedFirst"], json!(true));
    assert_eq!(out["outputUpdated"], json!(true));
    assert_eq!(out["compiled"], Value::Null);
    for request in mb.requests() {
        assert!(
            request.params["deadlineMs"].as_u64().unwrap_or(0) > 0,
            "{request:?}"
        );
    }
}

#[tokio::test]
async fn compile_editor_refuses_a_file_that_is_not_the_active_document() {
    let tree = Tree::new("ed-notactive");
    tree.install("static_prop.vmdl", PROBE_REL);
    let mb = MockBridge::builder()
        .modeldoc()
        .model_doc(Some(make_model_doc(PROBE_DOC_PATH, false)))
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(
        &tree,
        Some(mb.discovery_path.display().to_string()),
        None,
        45_000,
    ))
    .unwrap();
    // Active doc: probe.vmdl; requested: another (existing) file in the addon.
    tree.install("static_prop.vmdl", "models/mcp_probe/other.vmdl");
    tree.write_snapshot(&["RootNode"]); // keep the verifier out of the way
    let err = call(
        &server,
        "model_compile",
        json!({"path": "csgo_addons/mcp_test/models/mcp_probe/other.vmdl", "route": "editor"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NotActiveDocument");
    assert!(err.retry_safe, "nothing was sent, nothing changed");
    assert!(mb.method_calls("modeldoc.compile").is_empty());
}

#[tokio::test]
async fn compile_editor_without_an_active_document_refuses() {
    let tree = Tree::new("ed-nodoc");
    tree.install("static_prop.vmdl", PROBE_REL);
    let mb = MockBridge::builder().modeldoc().model_doc(None).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(
        &tree,
        Some(mb.discovery_path.display().to_string()),
        None,
        45_000,
    ))
    .unwrap();
    let err = call(
        &server,
        "model_compile",
        json!({"path": PROBE_ARG, "route": "editor"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NoActiveDocument");
    assert!(err.retry_safe);
    assert!(mb.method_calls("modeldoc.compile").is_empty());
}

#[tokio::test]
async fn compile_editor_without_the_bridge_methods_is_not_implemented() {
    let tree = Tree::new("ed-caps");
    tree.install("static_prop.vmdl", PROBE_REL);
    // No `.modeldoc()`: the methods are not advertised, nothing may be sent.
    let mb = MockBridge::builder().start().await.unwrap();
    let server = hammer_mcp::build_server(&config(
        &tree,
        Some(mb.discovery_path.display().to_string()),
        None,
        45_000,
    ))
    .unwrap();
    let err = call(
        &server,
        "model_compile",
        json!({"path": PROBE_ARG, "route": "editor"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NotImplemented");
    assert_eq!(
        err.data["missing"],
        json!(["modeldoc.compile", "modeldoc.active"])
    );
    assert!(mb.method_calls("modeldoc.active").is_empty());
}

#[tokio::test]
async fn compile_editor_timeout_is_an_ok_false_result() {
    let tree = Tree::new("ed-timeout");
    tree.install("static_prop.vmdl", PROBE_REL);
    // The editor never writes the output: the tool polls until its (small) budget is gone.
    let mb = MockBridge::builder()
        .modeldoc()
        .model_doc(Some(make_model_doc(PROBE_DOC_PATH, false)))
        .responder("modeldoc.compile", |_| {
            json!({"status": null, "failed": null, "savedFirst": false})
        })
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(
        &tree,
        Some(mb.discovery_path.display().to_string()),
        None,
        1_000,
    ))
    .unwrap();
    let out = call(
        &server,
        "model_compile",
        json!({"path": PROBE_ARG, "route": "editor"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["outputUpdated"], json!(false));
    assert_eq!(out["savedFirst"], json!(false));
    assert!(out["okNote"].as_str().is_some());
}

// -------------------------------------------------------------------------------------------
// stdout parser (unit)
// -------------------------------------------------------------------------------------------

#[test]
fn parse_compile_output_cases() {
    let ok = parse_compile_output(
        "  ...\nOK: 2 compiled, 0 failed, 0 skipped, 0m:01s\n",
    );
    assert_eq!(ok.compiled, Some(2));
    assert_eq!(ok.failed, Some(0));
    assert_eq!(ok.skipped, Some(0));
    assert!(ok.fail_lines.is_empty());
    assert!(!ok.fatal);

    // Dependency failures do not count in `failed`; they show up as fail lines.
    let mixed = parse_compile_output(
        "[FAIL] dependency: materials/a.vmat\nOK: 1 compiled, 0 failed, 2 skipped, 0m:02s",
    );
    assert_eq!(mixed.compiled, Some(1));
    assert_eq!(mixed.failed, Some(0));
    assert_eq!(mixed.skipped, Some(2));
    assert_eq!(mixed.fail_lines.len(), 1);
    assert!(mixed.fail_lines[0].starts_with("[FAIL]"));
    // a bare right-aligned [FAIL] carries the error line before it
    let bare = parse_compile_output(
        "Error reading texture \"a.tga\"\n\n                                  [FAIL]\nOK: 1 compiled, 0 failed, 0 skipped",
    );
    assert_eq!(bare.fail_lines, vec!["[FAIL] Error reading texture \"a.tga\"".to_string()]);

    // FATAL without a summary (like the FATAL gameinfo.gi error under -game /app/game/<addon>).
    let fatal = parse_compile_output("[FAIL] x\nFATAL ERROR: unable to load gameinfo.gi\n");
    assert_eq!(fatal.compiled, None);
    assert!(fatal.fatal);
    assert_eq!(fatal.fail_lines.len(), 1);

    assert_eq!(
        parse_compile_output(""),
        hammer_mcp::tools::model_file::CompileSummary::default()
    );
}
