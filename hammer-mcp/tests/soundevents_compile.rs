//! `compile_soundevents` tests (WP-S2): the shared `resourcecompiler` route over a `.vsndevts`
//! plus the `.vsndevts`-specific facts (RE R1): `RESOURCE COMPILE ERROR` lines become
//! `errors:[{line, col, message}]`, `events` counts the fresh `_c`, `ok:false` is a result.
//!
//! The fake podman harness is the particle/model file-route tests' (a `ps` that reports a
//! container, an `exec` that writes the `game/<rel>_c` output and prints the summary line); the
//! `ok` kind copies a real compiled `.vsndevts_c` (a synthetic resource container built with
//! `tests/common`'s KV3 v4 encoder) so the event count is a real number. No container, no game,
//! no bridge anywhere.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::{McpServer, ServerIdentity};
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
use cs2_mcp_common::tools::{CallFailure, Registry, ToolError};
use hammer_mcp::resource::kv3bin::Kv3Value;
use hammer_mcp::soundevents::SoundEventIndex;
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A temp project tree like the real install (`workshop/run/app`).
struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "snd-compile-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("tmp tree");
        let app = root.join("workshop/run/app");
        Tree { root, app }
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let target = self.app.join("content/csgo_addons/mcp_test").join(rel);
        std::fs::create_dir_all(target.parent().expect("rel has a parent")).expect("write dir");
        std::fs::write(&target, text).expect("write file");
        target
    }

    /// The path `compile_soundevents` reports as `outputPath`
    /// (`<app>/game/csgo_addons/mcp_test/<rel>_c`).
    fn compile_output(&self, rel: &str) -> PathBuf {
        self.app.join("game").join(format!("csgo_addons/mcp_test/{rel}_c"))
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A synthetic compiled `.vsndevts_c`: one resource container whose `DATA` binary-KV3 root
/// object holds `count` events.
fn compiled_events_c(count: usize) -> Vec<u8> {
    let mut root: Vec<(&str, Kv3Value)> = Vec::new();
    for index in 0..count {
        root.push((
            Box::leak(format!("mcp.ev{index}").into_boxed_str()) as &str,
            common::kv_object(vec![
                ("type", common::kv_text("csgo_mega")),
                ("volume", Kv3Value::Double(0.5)),
            ]),
        ));
    }
    let src = common::kv_object(root);
    common::model_container(&[("DATA", &common::encode_kv3_v4(&src))])
}

/// A fake `podman` binary under the tree:
/// * `ok` — `ps` reports a container; `exec` maps `-i /app/content/<rel>` to `<app>/game/<rel>_c`,
///   copies the pre-built `_c` fixture there and prints the OK summary;
/// * `ok-touch` — same, but the output is an empty file (the event re-read then fails);
/// * `fail` — prints R1's failure shape (two `RESOURCE COMPILE ERROR` lines, a `[FAIL]`, the
///   ` ERROR: 0 compiled, …` summary) and does NOT touch the output;
/// * `no-container` — `ps` reports nothing.
fn fake_podman(tree: &Tree, kind: &str) -> PathBuf {
    let bin_dir = tree.root.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    let body = match kind {
        "ok" => r#"#!/bin/bash
APP='__APP__'
FIXTURE='__FIXTURE__'
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
    cp "$FIXTURE" "$out"
    echo "Compiling $rel"
    echo "OK: 1 compiled, 0 failed, 0 skipped, 0m:01s"
    ;;
  *)
    echo "fake podman: unsupported verb $1" >&2
    exit 2
    ;;
esac
"#,
        "ok-touch" => r#"#!/bin/bash
APP='__APP__'
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    last="${!#}"
    rel="${last##* -i }"
    rel="${rel#/app/content/}"
    out="$APP/game/${rel}_c"
    mkdir -p "$(dirname "$out")"
    touch "$out"
    echo "OK: 1 compiled, 0 failed, 0 skipped, 0m:00s"
    ;;
  *) echo "fake podman: unsupported verb $1" >&2; exit 2 ;;
esac
"#,
        "fail" => r#"#!/bin/bash
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    printf '%s\n' 'Z:\app\content\csgo_addons\mcp_test\soundevents\bad.vsndevts(5,12): RESOURCE COMPILE ERROR: Invalid value "="'
    printf '%s\n' 'Z:\app\content\csgo_addons\mcp_test\soundevents\bad.vsndevts(7,3): RESOURCE COMPILE ERROR: Missing member "type"'
    printf '%s\n' 'Z:\app\content\csgo_addons\mcp_test\soundevents\bad.vsndevts(5,12)'
    printf '%s\n' '[FAIL]'
    printf '%s\n' ' ERROR: 0 compiled, 1 failed, 0 skipped, 0m:00s'
    exit 1
    ;;
  *) echo "fake podman: unsupported verb $1" >&2; exit 2 ;;
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
    let body = body.replace("__FIXTURE__", &fixture_path(tree, kind).display().to_string());
    let script = bin_dir.join(format!("podman-{kind}"));
    std::fs::write(&script, body).expect("fake podman script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    script
}

/// The pre-built `_c` the `ok` fake copies over the real output.
fn fixture_path(tree: &Tree, kind: &str) -> PathBuf {
    let dir = tree.root.join("fixtures");
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let path = dir.join("soundevents_addon.vsndevts_c");
    if kind == "ok" && !path.exists() {
        std::fs::write(&path, compiled_events_c(3)).expect("fixture _c");
    }
    path
}

fn server_for(tree: &Tree, podman: Option<&Path>) -> (McpServer<HammerState>, HammerState) {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let mut args: Vec<String> = vec![
        "--deadline-ms".into(),
        "5000".into(),
        "--tool-budget-ms".into(),
        "45000".into(),
        "--project-root".into(),
        tree.root.display().to_string(),
        "--path-map".into(),
        format!("z:\\app={}", tree.app.display()),
    ];
    if let Some(podman) = podman {
        args.extend(["--podman".into(), podman.display().to_string()]);
    }
    let given: Vec<&str> = args.iter().map(String::as_str).collect();
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    let cfg = W::parse_from(argv).c;
    let state = HammerState::from_config(&cfg).expect("state");
    let mut registry = Registry::new();
    hammer_mcp::tools::soundevents_edit::register(&mut registry);
    let server = McpServer::new(
        registry,
        state.clone(),
        ServerIdentity { name: "hammer-mcp", version: "test", instructions: String::new() },
        cfg.tool_budget_ms,
        cfg.deadline_ms,
    );
    (server, state)
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

const SOURCE_REL: &str = "soundevents/soundevents_addon.vsndevts";
const SOURCE_TEXT: &str = concat!(
    "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\r\n",
    "{\r\n",
    "\t\"mcp.ev0\" = \r\n",
    "\t{\r\n",
    "\t\ttype = \"csgo_mega\"\r\n",
    "\t}\r\n",
    "}"
);

#[tokio::test]
async fn a_compiling_run_counts_the_fresh_events_and_invalidates_the_index() {
    let tree = Tree::new("ok");
    tree.write(SOURCE_REL, SOURCE_TEXT);
    let podman = fake_podman(&tree, "ok");
    let (server, state) = server_for(&tree, Some(&podman));
    *state.soundevent_index.lock().unwrap() = Some(Arc::new(SoundEventIndex::default()));

    let out = call(&server, "compile_soundevents", json!({})).await.unwrap();
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["outputUpdated"], json!(true));
    assert_eq!(out["outputPath"], json!(tree.compile_output(SOURCE_REL).display().to_string()));
    assert_eq!(out["events"], json!(3), "the event count from the fresh _c");
    assert_eq!(out["compiled"], json!(1));
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes.iter().any(|n| n.as_str().map(|s| s.contains("run_map")).unwrap_or(false)),
        "the run_map note (RE R2): {notes:?}"
    );
    assert!(state.soundevent_index.lock().unwrap().is_none(), "the compile invalidated the index");
}

#[tokio::test]
async fn compile_errors_are_a_result_with_structured_errors() {
    let tree = Tree::new("fail");
    tree.write(SOURCE_REL, SOURCE_TEXT);
    let podman = fake_podman(&tree, "fail");
    let (server, state) = server_for(&tree, Some(&podman));
    *state.soundevent_index.lock().unwrap() = Some(Arc::new(SoundEventIndex::default()));

    let out = call(&server, "compile_soundevents", json!({})).await.unwrap();
    assert_eq!(out["ok"], json!(false), "a failed compile is a RESULT, not an error");
    assert_eq!(out["events"], Value::Null);
    assert_eq!(
        out["errors"],
        json!([
            {"line": 5, "col": 12, "message": "Invalid value \"=\""},
            {"line": 7, "col": 3, "message": "Missing member \"type\""},
        ]),
        "every RESOURCE COMPILE ERROR line, the Windows path stripped"
    );
    assert!(
        state.soundevent_index.lock().unwrap().is_some(),
        "a failed compile wrote nothing, so the index stays"
    );
}

#[tokio::test]
async fn an_unreadable_output_is_a_null_count_with_a_note() {
    let tree = Tree::new("no-read");
    tree.write(SOURCE_REL, SOURCE_TEXT);
    let podman = fake_podman(&tree, "ok-touch");
    let (server, _state) = server_for(&tree, Some(&podman));

    let out = call(&server, "compile_soundevents", json!({})).await.unwrap();
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["events"], Value::Null);
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes.iter().any(|n| n.as_str().map(|s| s.contains("event count")).unwrap_or(false)),
        "the empty-output note: {notes:?}"
    );
}

#[tokio::test]
async fn a_missing_file_refuses_before_any_exec() {
    let tree = Tree::new("missing");
    let podman = fake_podman(&tree, "ok");
    let (server, _state) = server_for(&tree, Some(&podman));

    let err = call(&server, "compile_soundevents", json!({})).await.unwrap_err();
    assert_eq!(err.name, "SoundEventsFileNotFound");
    assert_eq!(err.data["hint"], json!("createFile:true"));
}

#[tokio::test]
async fn no_tools_container_is_a_runtime_error() {
    let tree = Tree::new("none");
    tree.write(SOURCE_REL, SOURCE_TEXT);
    let podman = fake_podman(&tree, "no-container");
    let (server, _state) = server_for(&tree, Some(&podman));

    let err = call(&server, "compile_soundevents", json!({})).await.unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable");
}

#[tokio::test]
async fn a_modpath_and_bad_timeout_refuse() {
    let tree = Tree::new("args");
    tree.write(SOURCE_REL, SOURCE_TEXT);
    let podman = fake_podman(&tree, "ok");
    let (server, _state) = server_for(&tree, Some(&podman));

    let err = call(
        &server,
        "compile_soundevents",
        json!({"path": "csgo_addons/other/soundevents/x.vsndevts"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "WriteOutsideAddon");

    let err = call(&server, "compile_soundevents", json!({"timeoutMs": 0})).await.unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");

    let err = call(&server, "compile_soundevents", json!({"bogus": 1})).await.unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
}

// -------------------------------------------------------------------------------------------
// FIX-E (review M4): `events` counts only the object-valued root members — the
// `_auto_calc_vnsd_duration_` scalar the real blocks carry is not an event (index.rs's rule).
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_event_count_counts_only_object_root_members() {
    let tree = Tree::new("scalar");
    tree.write(SOURCE_REL, SOURCE_TEXT);

    // Pre-write the fixture the "ok" podman copies over the output: 2 event objects PLUS the
    // scalar `_auto_calc_vnsd_duration_` member (the shared fixture_path() keeps a file that
    // already exists).
    let dir = tree.root.join("fixtures");
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let root = vec![
        ("_auto_calc_vnsd_duration_", Kv3Value::Double(4.0)),
        (
            "mcp.ev0",
            common::kv_object(vec![("type", common::kv_text("csgo_mega"))]),
        ),
        (
            "mcp.ev1",
            common::kv_object(vec![("type", common::kv_text("csgo_mega"))]),
        ),
    ];
    let src = common::kv_object(root);
    std::fs::write(
        dir.join("soundevents_addon.vsndevts_c"),
        common::model_container(&[("DATA", &common::encode_kv3_v4(&src))]),
    )
    .expect("fixture _c");

    let podman = fake_podman(&tree, "ok");
    let (server, _state) = server_for(&tree, Some(&podman));

    let out = call(&server, "compile_soundevents", json!({})).await.unwrap();
    assert_eq!(out["ok"], json!(true));
    assert_eq!(
        out["events"],
        json!(2),
        "only the object-valued members count (the duration scalar is not an event)"
    );
}

// -------------------------------------------------------------------------------------------
// FIX-K review N2: the compile holds the dedicated `soundevents_gate`, NOT the server-wide
// write gate — a bridge mutator (`set_entity_keyvalues` under `write_gate`, against the mock
// bridge) completes while a compile is in flight; the mutator finishing first is pinned by the
// compile's exec still waiting on the release file when the mutator's result is on hand.
// -------------------------------------------------------------------------------------------

/// The `z:\app` document path the mock serves (the same shape `tests/entities_live.rs` uses).
const GATE_DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";

/// A fake `podman` like `fake_podman(.., "ok")`, but the exec BLOCKS: it touches the started
/// file, then waits for the release file before producing the output — the seam that holds the
/// compile in flight while the test runs the mutator.
fn fake_gate_podman(tree: &Tree) -> PathBuf {
    let bin_dir = tree.root.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    let fixture = fixture_path(tree, "ok");
    let body = r#"#!/bin/bash
APP='__APP__'
FIXTURE='__FIXTURE__'
STARTED='__STARTED__'
RELEASE='__RELEASE__'
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    last="${!#}"
    rel="${last##* -i }"
    rel="${rel#/app/content/}"
    touch "$STARTED"
    n=0
    while [ ! -f "$RELEASE" ]; do
      sleep 0.05
      n=$((n + 1))
      [ "$n" -gt 1200 ] && break
    done
    out="$APP/game/${rel}_c"
    mkdir -p "$(dirname "$out")"
    cp "$FIXTURE" "$out"
    echo "OK: 1 compiled, 0 failed, 0 skipped, 0m:01s"
    ;;
  *)
    echo "fake gate podman: unsupported verb $1" >&2
    exit 2
    ;;
esac
"#
    .replace("__APP__", &tree.app.display().to_string())
    .replace("__FIXTURE__", &fixture.display().to_string())
    .replace("__STARTED__", &tree.root.join("compile-started").display().to_string())
    .replace("__RELEASE__", &tree.root.join("compile-release").display().to_string());
    let script = bin_dir.join("podman-gate");
    std::fs::write(&script, body).expect("fake gate podman script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    script
}

/// The mock world the mutator edits: node 30 `info_target` (built into the mock's Hammer world,
/// which also serves `doc.active` — no responder needed, like `tests/entities_live.rs`).
fn gate_nodes() -> Vec<MockNode> {
    vec![
        MockNode::world(),
        MockNode::entity(
            30,
            "info_target",
            [1.0, 2.0, 3.0],
            [("targetname".to_string(), "t30".to_string())].into_iter().collect(),
        ),
    ]
}

/// The full config for the gate test: the mock bridge's `--discovery` (the real tool suite) and
/// the fake blocking podman.
fn gate_config(tree: &Tree, mb: &MockBridge, podman: &Path) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let args: Vec<String> = vec![
        "--deadline-ms".into(),
        "5000".into(),
        "--discovery".into(),
        mb.discovery_path.display().to_string(),
        "--project-root".into(),
        tree.root.display().to_string(),
        "--path-map".into(),
        format!("z:\\app={}", tree.app.display()),
        "--podman".into(),
        podman.display().to_string(),
    ];
    let given: Vec<&str> = args.iter().map(String::as_str).collect();
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

/// Bounded poll of a file's existence (the compile's exec readiness signal), no blind sleep
/// beyond the tick. **Async** — a blocking sleep here ran INSIDE the same join! task as the
/// compile future and starved it; the exec would never even spawn.
async fn wait_for(path: &Path, seconds: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while !path.exists() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    true
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bridge_mutator_completes_while_a_compile_is_in_flight() {
    let tree = Tree::new("gate");
    tree.write(SOURCE_REL, SOURCE_TEXT);
    let podman = fake_gate_podman(&tree);
    let mb = MockBridge::builder()
        .doc(Some(make_doc(GATE_DOC_WIN, Some(true))))
        .hammer_nodes(gate_nodes())
        .start()
        .await
        .expect("mock bridge");
    let cfg = gate_config(&tree, &mb, &podman);
    let server = hammer_mcp::build_server(&cfg).expect("the real tool suite");
    let started = tree.root.join("compile-started");
    let release = tree.root.join("compile-release");
    assert!(!release.exists());

    // The compile future: blocked in its container exec until the mutator releases it.
    let compile = async {
        call(&server, "compile_soundevents", json!({})).await
            .expect("the compile completes once released")
    };
    // The mutator future: waits for the compile's exec to be running (the started file, which
    // only appears AFTER `compile_soundevents` acquired the gate), then takes the write gate for
    // `set_entity_keyvalues` — which must complete while the compile is still in flight; the
    // ordering is pinned by the release file not existing when the mutator's result is on hand.
    let mutator = async {
        let ok = wait_for(&started, 10).await;
        assert!(
            ok,
            "the compile's exec never started; nothing else in this test means anything"
        );
        let out = call(
            &server,
            "set_entity_keyvalues",
            json!({"nodeID": 30, "keyValues": {"targetname": "gate_probe"}}),
        )
        .await
        .expect("the bridge mutator completes while the compile holds the soundevents gate");
        assert_eq!(out["nodeID"], json!(30));
        assert_eq!(out["applied"]["targetname"], json!(true));
        assert!(
            !release.exists(),
            "the compile is still awaited in its exec — the mutator was not serialized behind it"
        );
        std::fs::write(&release, b"").expect("release the compile");
    };
    let (compiled, _mutator) = tokio::join!(compile, mutator);

    // With the compile released it finished; a shared gate (write_gate for both tools) would
    // have deadlocked this pair until the exec timeout instead of the mutator winning first.
    assert_eq!(compiled["ok"], json!(true));
    assert_eq!(compiled["events"], json!(3), "the fresh _c was re-read for the event count");
}
