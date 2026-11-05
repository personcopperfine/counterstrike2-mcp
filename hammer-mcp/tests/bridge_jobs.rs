//! The bridge job route (`tools/bridge_job.rs`): `compile_map`, `model_compile` and the generic
//! `compile_asset` / `job_status` / `job_cancel` tools against the mock bridge's scriptable
//! `job.*` (`cs2_mcp_common::mock::jobs`). The podman half of the compile_map contract (fallback
//! when `job.start` is not advertised, cancel by tagged kill) lives in `tests/compile_map.rs`
//! next to the fake podman.
//!
//! Nothing here runs a compiler: the mock answers `job.start`/`job.status`/`job.cancel` per the
//! bridge's wire contract (`target/orch/unblock-rows/jobs-contract.md`) and, when a test asks, writes
//! the compiled output at the moment a job turns `ok`.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::jobs::MockJobOutcome;
use cs2_mcp_common::mock::{make_doc, MockBridge};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";
const MAP_ARG: &str = "maps/probe.vmap";

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "bridge-jobs-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let app = root.join("workshop/run/app");
        std::fs::create_dir_all(app.join("content/csgo_addons/mcp_test/maps")).expect("tmp tree");
        Tree { root, app }
    }

    fn addon(&self) -> PathBuf {
        self.app.join("content/csgo_addons/mcp_test")
    }

    /// A source file inside the addon content tree.
    fn install(&self, rel: &str) -> PathBuf {
        let path = self.addon().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"placeholder").unwrap();
        path
    }

    fn vpk(&self) -> PathBuf {
        self.app.join("game/csgo_addons/mcp_test/maps/probe.vpk")
    }

    fn vmdl_c(&self, rel: &str) -> PathBuf {
        self.app.join("game/csgo_addons/mcp_test").join(format!("{rel}_c"))
    }

    fn config(&self, discovery: Option<&std::path::Path>, tool_budget_ms: u64) -> CommonConfig {
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: CommonConfig,
        }
        let budget = tool_budget_ms.to_string();
        let root = self.root.display().to_string();
        let map = format!("z:\\app={}", self.app.display());
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
            args.extend(["--discovery".into(), discovery.display().to_string()]);
        }
        let given: Vec<&str> = args.iter().map(String::as_str).collect();
        let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
        W::parse_from(argv).c
    }

    fn server(&self, mb: &MockBridge) -> McpServer<HammerState> {
        hammer_mcp::build_server(&self.config(Some(&mb.discovery_path), 45_000)).expect("server")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
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

fn job_starts(mb: &MockBridge) -> Vec<Value> {
    mb.method_calls("job.start").into_iter().map(|r| r.params).collect()
}

// -------------------------------------------------------------------------------------------
// compile_map on the bridge route
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn compile_map_runs_as_a_bridge_job_and_the_verdict_is_host_side() {
    let tree = Tree::new("map-ok");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Ok, 1)
        .job_writes([tree.vpk()])
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);

    let started = call(&server, "compile_map", json!({"path": MAP_ARG, "timeoutMs": 600_000}))
        .await
        .expect("start");
    assert_eq!(started["state"], json!("running"));
    assert_eq!(started["route"], json!("bridge"));
    assert_eq!(started["jobId"], json!("c1"), "compile_map keeps its own id space");
    let bridge_id = started["bridgeJobId"].as_str().expect("bridge id").to_string();
    assert!(bridge_id.starts_with('j'), "{bridge_id}");
    // the log is the bridge's, mapped to the host (<app>/<relative>)
    let log = started["log"].as_str().expect("log");
    assert!(
        log.starts_with(&tree.app.join("cs2mcp_jobs/").display().to_string()),
        "{log}"
    );
    assert!(started["command"].is_array());

    // the wire request: recipe flags as tokens, game false, the addon-relative input
    let starts = job_starts(&mb);
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0]["kind"], json!("resourcecompiler"));
    assert_eq!(starts[0]["input"], json!("csgo_addons/mcp_test/maps/probe.vmap"));
    assert_eq!(starts[0]["game"], json!(false));
    assert_eq!(
        starts[0]["flags"],
        json!(["-threads", "8", "-fshallow", "-maxtextureres", "256", "-nop4", "-quiet", "-unbufferedio", "-noassert"])
    );
    assert_eq!(starts[0]["timeoutMs"], json!(600_000));

    let running = call(&server, "compile_map", json!({"jobId": "c1", "waitMs": 0})).await.unwrap();
    assert_eq!(running["state"], json!("running"), "{running}");
    assert_eq!(running["route"], json!("bridge"));
    assert_eq!(running["vpkUpdated"], json!(false));

    let done = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap();
    assert_eq!(done["state"], json!("ok"), "{done}");
    assert_eq!(done["compiled"], json!(3));
    assert_eq!(done["failed"], json!(0));
    assert_eq!(done["skipped"], json!(1));
    assert_eq!(done["vpkUpdated"], json!(true), "the vpk verdict is the host's mtime check");
    assert_eq!(done["vpkPath"], json!(tree.vpk().display().to_string()));
    assert_eq!(done["sourceChanged"], json!(false));
    assert!(done["okLine"].as_str().unwrap().starts_with("OK: 3 compiled"));
    assert!(done["logTail"].as_str().unwrap().contains("OK: 3 compiled"));
    assert_eq!(done["container"], Value::Null);
    // the finished job answers from the store: no further job.status once it is terminal
    let statuses = mb.method_calls("job.status").len();
    let again = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap();
    assert_eq!(again["state"], json!("ok"));
    assert_eq!(mb.method_calls("job.status").len(), statuses);
}

#[tokio::test]
async fn an_ok_job_without_the_vpk_is_failed_and_failures_are_reported() {
    // OK line but no compiled output on the host: the same "output missing" verdict as podman
    let tree = Tree::new("map-novpk");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Ok, 0)
        .job_script(MockJobOutcome::Failed, 0)
        .job_script(MockJobOutcome::Fatal, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);

    call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap();
    let out = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap();
    assert_eq!(out["state"], json!("failed"), "{out}");
    assert!(out["reason"].as_str().unwrap().contains("missing"), "{out}");

    call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap();
    let out = call(&server, "compile_map", json!({"jobId": "c2"})).await.unwrap();
    assert_eq!(out["state"], json!("failed"), "{out}");
    assert_eq!(out["failed"], json!(2));
    assert_eq!(out["fatal"], json!(false));
    assert!(out["failLines"][0].as_str().unwrap().contains("[FAIL]"), "{out}");

    // the bridge's `fatal` lands in compile_map's `fatal` member
    call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap();
    let out = call(&server, "compile_map", json!({"jobId": "c3"})).await.unwrap();
    assert_eq!(out["state"], json!("failed"), "{out}");
    assert_eq!(out["fatal"], json!(true), "{out}");
}

#[tokio::test]
async fn a_bridge_timeout_is_the_timeout_state() {
    let tree = Tree::new("map-timeout");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Timeout, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap();
    let out = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap();
    assert_eq!(out["state"], json!("timeout"), "{out}");
    assert!(out["reason"].as_str().unwrap().contains("deadline"));
}

#[tokio::test]
async fn cancel_ends_a_bridge_job_and_a_finished_one_answers_cancelled_false() {
    let tree = Tree::new("map-cancel");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let started = call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap();
    let bridge_id = started["bridgeJobId"].as_str().unwrap().to_string();

    let out = call(&server, "compile_map", json!({"jobId": "c1", "cancel": true}))
        .await
        .unwrap();
    assert_eq!(out["cancelled"], json!(true), "{out}");
    assert_eq!(out["state"], json!("cancelled"));
    assert_eq!(out["route"], json!("bridge"));
    let cancels = mb.method_calls("job.cancel");
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0].params["jobId"], json!(bridge_id));

    // already finished: its final state, nothing cancelled, no second job.cancel
    let out = call(&server, "compile_map", json!({"jobId": "c1", "cancel": true}))
        .await
        .unwrap();
    assert_eq!(out["cancelled"], json!(false));
    assert_eq!(out["state"], json!("cancelled"));
    assert_eq!(mb.method_calls("job.cancel").len(), 1);

    // the same through job_cancel, by either id
    let out = call(&server, "job_cancel", json!({"jobId": "c1"})).await.unwrap();
    assert_eq!(out["cancelled"], json!(false));
    let out = call(&server, "job_cancel", json!({"jobId": bridge_id})).await.unwrap();
    assert_eq!(out["cancelled"], json!(false), "routed through the owning compile_map job: {out}");
    assert_eq!(out["state"], json!("cancelled"));

    // unknown ids keep the existing unknown-job error
    let err = call(&server, "compile_map", json!({"jobId": "c99", "cancel": true}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    assert!(err.message.contains("unknown jobId"), "{err:?}");
    // cancel without a jobId is a usage error
    let err = call(&server, "compile_map", json!({"cancel": true})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
}

#[tokio::test]
async fn a_running_job_refuses_the_next_start_as_engine_busy() {
    let tree = Tree::new("map-busy");
    tree.install("maps/probe.vmap");
    tree.install("materials/x.vmat");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap();

    // a second compile_map: the store refuses (after syncing: the job is still running)
    let err = call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap_err();
    assert_eq!(err.name, "EngineBusy", "{err:?}");
    assert_eq!(err.data["jobId"], json!("c1"));
    assert_eq!(job_starts(&mb).len(), 1, "no second job.start");

    // compile_asset goes to the bridge, whose one-job rule answers with the running job
    let err = call(&server, "compile_asset", json!({"path": "materials/x.vmat"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "EngineBusy", "{err:?}");
    assert_eq!(err.data["reason"], json!("bridge_job_running"));
    assert!(err.data["jobId"].as_str().unwrap().starts_with('j'));
    assert_eq!(job_starts(&mb).len(), 2, "the bridge was asked and refused");
}

#[tokio::test]
async fn a_job_that_ended_on_the_bridge_does_not_block_the_next_start() {
    let tree = Tree::new("map-resync");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Failed, 0)
        .job_script(MockJobOutcome::Hang, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap();
    // never polled: the start syncs the store entry first, so it no longer counts as running
    let second = call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap();
    assert_eq!(second["jobId"], json!("c2"));
    let first = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap();
    assert_eq!(first["state"], json!("failed"));
}

#[tokio::test]
async fn nav_adds_the_flag_and_the_host_gates_run_before_any_job_starts() {
    let tree = Tree::new("map-gates");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .job_script(MockJobOutcome::Ok, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);

    // the dirty gate (the target is the active document)
    let err = call(&server, "compile_map", json!({})).await.unwrap_err();
    assert_eq!(err.name, "UnsavedChanges", "{err:?}");
    assert!(job_starts(&mb).is_empty(), "the dirty gate precedes job.start");

    // nav without a compiled vpk (a bridge whose active document is another map)
    let mb2 = MockBridge::builder()
        .job_script(MockJobOutcome::Ok, 0)
        .start()
        .await
        .unwrap();
    let server2 = tree.server(&mb2);
    let err = call(&server2, "compile_map", json!({"path": MAP_ARG, "mode": "nav"}))
        .await
        .unwrap_err();
    assert_eq!(err.data["reason"], json!("no_compiled_vpk"));
    assert!(job_starts(&mb2).is_empty(), "refusals precede job.start");

    // with the vpk present, nav passes -nav
    std::fs::create_dir_all(tree.vpk().parent().unwrap()).unwrap();
    std::fs::write(tree.vpk(), b"old").unwrap();
    call(&server2, "compile_map", json!({"path": MAP_ARG, "mode": "nav"}))
        .await
        .unwrap();
    let flags = job_starts(&mb2)[0]["flags"].clone();
    assert_eq!(flags.as_array().unwrap().last().unwrap(), &json!("-nav"));
}

#[tokio::test]
async fn the_in_game_refusal_still_precedes_the_bridge_job() {
    let tree = Tree::new("map-ingame");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .responder_value("game.local", json!({"inGame": true, "map": "probe"}))
        .job_script(MockJobOutcome::Ok, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let err = call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap_err();
    assert_eq!(err.name, "EngineBusy");
    assert_eq!(err.data["reason"], json!("map_in_game"));
    assert!(job_starts(&mb).is_empty());
}

#[tokio::test]
async fn a_start_the_bridge_refuses_leaves_no_job_behind() {
    // a bridge that cannot run compile jobs HERE, with no podman either: the podman route's own
    // RuntimeUnavailable is the answer, and the store has nothing left over
    let tree = Tree::new("map-refused");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_unavailable("job: resourcecompiler is Windows-only")
        .start()
        .await
        .unwrap();
    let cfg = tree.config(Some(&mb.discovery_path), 45_000);
    let server = hammer_mcp::build_server(&cfg).unwrap();
    let err = call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable", "{err:?}");
    // (the podman fallback was tried: nothing is running in this test's container runtime)
    assert!(err.message.contains("tools container") || err.message.contains("podman"), "{err:?}");
    let err = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap_err();
    assert!(err.message.contains("unknown jobId"), "{err:?}");
}

#[tokio::test]
async fn shutdown_cancels_the_running_bridge_jobs() {
    let tree = Tree::new("shutdown");
    tree.install("maps/probe.vmap");
    tree.install("materials/x.vmat");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let started = call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap();
    let bridge_id = started["bridgeJobId"].as_str().unwrap().to_string();

    let report = hammer_mcp::shutdown_cleanup(server.state()).await;
    let cancels = mb.method_calls("job.cancel");
    assert_eq!(cancels.len(), 1, "{report}");
    assert_eq!(cancels[0].params["jobId"], json!(bridge_id));
    assert_eq!(report["bridge"]["cancelled"][0]["cancelled"], json!(true), "{report}");
    // the tagged-kill machinery is the podman route's: none ran for the bridge job
    assert_eq!(report["jobs"], json!([]), "{report}");
    let polled = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap();
    assert_eq!(polled["state"], json!("cancelled"), "{polled}");

    // a compile_asset job is cancelled at shutdown too (the job must not outlive the server)
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let asset = call(&server, "compile_asset", json!({"path": "materials/x.vmat"})).await.unwrap();
    let report = hammer_mcp::shutdown_cleanup(server.state()).await;
    let cancels = mb.method_calls("job.cancel");
    assert_eq!(cancels.len(), 1, "{report}");
    assert_eq!(cancels[0].params["jobId"], asset["jobId"]);
}

// -------------------------------------------------------------------------------------------
// model_compile / particle_compile (compile_resourcecompiler) on the bridge route
// -------------------------------------------------------------------------------------------

const VMDL: &str = "csgo_addons/mcp_test/models/x/x.vmdl";

#[tokio::test]
async fn model_compile_runs_as_a_game_true_bridge_job() {
    let tree = Tree::new("model-ok");
    tree.install("models/x/x.vmdl");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Ok, 1)
        .job_writes([tree.vmdl_c("models/x/x.vmdl")])
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let out = call(&server, "model_compile", json!({"path": VMDL})).await.unwrap();
    assert_eq!(out["route"], json!("resourcecompiler"));
    assert_eq!(out["compiler"], json!("bridge"));
    assert!(out["jobId"].as_str().unwrap().starts_with('j'), "{out}");
    assert_eq!(out["state"], json!("ok"));
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["outputUpdated"], json!(true));
    assert_eq!(out["compiled"], json!(3));
    assert_eq!(out["timedOut"], json!(false));
    assert_eq!(out["container"], Value::Null);
    assert!(out["log"].as_str().unwrap().contains("cs2mcp_jobs"));
    let notes = out["notes"].to_string();
    assert!(notes.contains("restart the tools session") && notes.contains("recompile the map"), "{notes}");

    let starts = job_starts(&mb);
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0]["input"], json!("csgo_addons/mcp_test/models/x/x.vmdl"));
    assert_eq!(starts[0]["game"], json!(true));
    assert_eq!(starts[0]["flags"], json!(["-nop4"]));
    assert!(mb.method_calls("job.status").len() >= 2, "polled until terminal");
}

/// A tool without a `force` option (`model_compile`) must not tell the caller to pass `force:true`
/// on the bridge route either: the reason says to touch the source.
#[tokio::test]
async fn a_bridge_skip_in_a_tool_without_force_does_not_suggest_force() {
    let tree = Tree::new("model-skip");
    tree.install("models/x/x.vmdl");
    let out_c = tree.vmdl_c("models/x/x.vmdl");
    std::fs::create_dir_all(out_c.parent().unwrap()).unwrap();
    std::fs::write(&out_c, b"old").unwrap();
    // the source is newer than the compiled output the compiler claims is current
    let src = tree.app.join("content/csgo_addons/mcp_test/models/x/x.vmdl");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    std::fs::File::options().write(true).open(&src).unwrap().set_modified(later).unwrap();
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Skipped, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let out = call(&server, "model_compile", json!({"path": VMDL})).await.unwrap();
    assert_eq!(out["compiler"], json!("bridge"), "{out}");
    assert_eq!(out["ok"], json!(false), "{out}");
    assert_eq!(out["skippedByCompiler"], json!(true), "{out}");
    let reason = out["reason"].as_str().unwrap();
    assert!(reason.contains("touch the source") && !reason.contains("force"), "{reason}");
}

#[tokio::test]
async fn a_failed_bridge_job_is_a_result_with_ok_false_not_an_error() {
    let tree = Tree::new("model-fail");
    tree.install("models/x/x.vmdl");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Failed, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let out = call(&server, "model_compile", json!({"path": VMDL})).await.unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["state"], json!("failed"));
    assert_eq!(out["failed"], json!(2));
    assert!(out["failLines"][0].as_str().unwrap().contains("[FAIL]"));
}

#[tokio::test]
async fn the_tool_timeout_cancels_the_bridge_job() {
    let tree = Tree::new("model-timeout");
    tree.install("models/x/x.vmdl");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let out = call(&server, "model_compile", json!({"path": VMDL, "timeoutMs": 400}))
        .await
        .unwrap();
    assert_eq!(out["ok"], json!(false), "{out}");
    assert_eq!(out["timedOut"], json!(true));
    assert_eq!(out["state"], json!("cancelled"), "the job did not outlive the call: {out}");
    assert_eq!(mb.method_calls("job.cancel").len(), 1);
    // the bridge got a backstop deadline beyond ours
    assert!(job_starts(&mb)[0]["timeoutMs"].as_u64().unwrap() >= 1_000);
}

#[tokio::test]
async fn a_bridge_that_cannot_run_jobs_here_falls_back_to_the_podman_route_with_a_note() {
    let tree = Tree::new("model-fallback");
    tree.install("models/x/x.vmdl");
    let mb = MockBridge::builder()
        .job_unavailable("job: AssignProcessToJobObject failed: 5")
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    // no podman in this test: the fallback is tried (RuntimeUnavailable from the runtime), which
    // is exactly the proof that the podman route took over
    let err = call(&server, "model_compile", json!({"path": VMDL})).await.unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable", "{err:?}");
    assert!(err.message.contains("tools container"), "{err:?}");
    assert_eq!(job_starts(&mb).len(), 1);
}

// -------------------------------------------------------------------------------------------
// compile_asset / job_status / job_cancel
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn compile_asset_starts_a_job_and_job_status_follows_it() {
    let tree = Tree::new("asset-ok");
    tree.install("materials/x.vmat");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Ok, 1)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);

    let out = call(&server, "compile_asset", json!({"path": "materials/x.vmat"}))
        .await
        .unwrap();
    assert_eq!(out["state"], json!("running"));
    assert_eq!(out["route"], json!("bridge"));
    assert_eq!(out["path"], json!("materials/x.vmat"));
    assert_eq!(out["input"], json!("csgo_addons/mcp_test/materials/x.vmat"));
    assert_eq!(out["game"], json!(true), "non-map assets default to game:true");
    let job_id = out["jobId"].as_str().unwrap().to_string();
    assert!(
        out["log"].as_str().unwrap().starts_with(&tree.app.join("cs2mcp_jobs/").display().to_string()),
        "{out}"
    );
    assert!(out["command"].is_array());
    assert_eq!(out["next"][0]["tool"], json!("job_status"));

    let status = call(&server, "job_status", json!({"jobId": job_id, "waitMs": 0})).await.unwrap();
    assert_eq!(status["state"], json!("running"));
    assert_eq!(status["route"], json!("bridge"));
    assert!(status["log"]["host"].as_str().unwrap().contains("cs2mcp_jobs"), "{status}");
    let status = call(&server, "job_status", json!({"jobId": job_id})).await.unwrap();
    assert_eq!(status["state"], json!("ok"));
    assert_eq!(status["summary"]["compiled"], json!(3));

    // no id: both lists
    let all = call(&server, "job_status", json!({})).await.unwrap();
    assert_eq!(all["bridge"]["jobs"][0]["jobId"], json!(job_id));
    assert!(all["bridge"]["jobs"][0]["log"]["host"].is_string());
    assert_eq!(all["compileMap"], json!([]));
}

#[tokio::test]
async fn compile_asset_defaults_game_false_for_maps_and_passes_flags_and_timeout() {
    let tree = Tree::new("asset-map");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Ok, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let out = call(
        &server,
        "compile_asset",
        json!({
            "path": "csgo_addons/mcp_test/maps/probe.vmap",
            "flags": ["-fshallow", "-threads", "4"],
            "timeoutMs": 90_000
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["game"], json!(false));
    assert_eq!(out["next"][0]["tool"], json!("job_status"));
    assert_eq!(out["next"][1]["tool"], json!("compile_map"), "a map: the vpk verdict");
    let starts = job_starts(&mb);
    assert_eq!(starts[0]["flags"], json!(["-fshallow", "-threads", "4"]));
    assert_eq!(starts[0]["timeoutMs"], json!(90_000));
    assert_eq!(starts[0]["game"], json!(false));
}

#[tokio::test]
async fn compile_asset_refusals() {
    let tree = Tree::new("asset-refuse");
    tree.install("materials/x.vmat");
    // a file OUTSIDE the addon: the content root's models/ dir
    let outside = tree.app.join("content/models/y.vmdl");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::write(&outside, b"x").unwrap();
    let mb = MockBridge::builder().jobs().start().await.unwrap();
    let server = tree.server(&mb);

    for (args, name) in [
        (json!({}), "InvalidParams"),
        (json!({"path": "materials/x.txt"}), "InvalidParams"),
        (json!({"path": "materials/missing.vmat"}), "AssetNotFound"),
        (json!({"path": "../x.vmat"}), "InvalidPath"),
        (json!({"path": outside.display().to_string()}), "WriteOutsideAddon"),
        (json!({"path": "materials/x.vmat", "game": "yes"}), "InvalidParams"),
        (json!({"path": "materials/x.vmat", "flags": "-nav"}), "InvalidParams"),
        (json!({"path": "materials/x.vmat", "flags": [1]}), "InvalidParams"),
        (json!({"path": "materials/x.vmat", "timeoutMs": 5}), "InvalidParams"),
    ] {
        let err = call(&server, "compile_asset", args.clone()).await.unwrap_err();
        assert_eq!(err.name, name, "{args}: {err:?}");
    }
    assert!(job_starts(&mb).is_empty(), "every refusal precedes job.start");

    // a flag the bridge does not allowlist is its -32602, mapped to InvalidParams
    let err = call(
        &server,
        "compile_asset",
        json!({"path": "materials/x.vmat", "flags": ["-evil"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(err.message.contains("-evil"), "{err:?}");
}

#[tokio::test]
async fn compile_asset_needs_the_bridge_job_route_and_names_job_start() {
    let tree = Tree::new("asset-nobridge-jobs");
    tree.install("materials/x.vmat");
    // a bridge without job.*
    let mb = MockBridge::builder().start().await.unwrap();
    let server = tree.server(&mb);
    let err = call(&server, "compile_asset", json!({"path": "materials/x.vmat"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable");
    assert!(err.message.contains("job.start"), "{err:?}");
    assert_eq!(err.data["missing"], json!(["job.start"]));
    // job_status on a bridge id names it too; a compile_map id and the lists still work
    let err = call(&server, "job_status", json!({"jobId": "j1-abc"})).await.unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable");
    let all = call(&server, "job_status", json!({})).await.unwrap();
    assert_eq!(all["bridge"], Value::Null);
    assert!(all["bridgeNote"].as_str().unwrap().contains("job.start"));

    // a bridge that cannot run the compiler here (Windows-only): refused, not routed elsewhere
    let mb = MockBridge::builder()
        .job_unavailable("job: resourcecompiler is Windows-only")
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let err = call(&server, "compile_asset", json!({"path": "materials/x.vmat"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable");
    assert_eq!(err.data["blockedOn"], json!("job: resourcecompiler is Windows-only"));

    // no bridge at all
    let server = hammer_mcp::build_server(&tree.config(None, 45_000)).unwrap();
    let err = call(&server, "compile_asset", json!({"path": "materials/x.vmat"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable");
    assert_eq!(err.data["reason"], json!("no_bridge"));
}

#[tokio::test]
async fn job_cancel_and_job_status_edge_cases() {
    let tree = Tree::new("asset-cancel");
    tree.install("materials/x.vmat");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let job_id = call(&server, "compile_asset", json!({"path": "materials/x.vmat"}))
        .await
        .unwrap()["jobId"]
        .as_str()
        .unwrap()
        .to_string();

    let out = call(&server, "job_cancel", json!({"jobId": job_id})).await.unwrap();
    assert_eq!(out["cancelled"], json!(true), "{out}");
    assert_eq!(out["state"], json!("cancelled"));
    assert!(out["log"]["host"].is_string());
    let out = call(&server, "job_cancel", json!({"jobId": job_id})).await.unwrap();
    assert_eq!(out["cancelled"], json!(false), "a finished job: its final state, not cancelled");
    assert_eq!(out["state"], json!("cancelled"));

    // unknown / malformed ids
    let err = call(&server, "job_cancel", json!({"jobId": "j99-zzz"})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    assert!(err.message.contains("unknown jobId"), "{err:?}");
    let err = call(&server, "job_status", json!({"jobId": "x7"})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    let err = call(&server, "job_status", json!({"jobId": "c7"})).await.unwrap_err();
    assert!(err.message.contains("unknown jobId"), "{err:?}");
    let err = call(&server, "job_cancel", json!({})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
}

#[tokio::test]
async fn job_status_of_a_compile_map_id_is_the_compile_map_answer() {
    let tree = Tree::new("status-map");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Ok, 5)
        .job_writes([tree.vpk()])
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let started = call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap();
    let bridge_id = started["bridgeJobId"].as_str().unwrap().to_string();

    let via_tool = call(&server, "job_status", json!({"jobId": "c1"})).await.unwrap();
    let via_map = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap();
    for key in ["jobId", "state", "route", "path", "vpkPath", "bridgeJobId", "failLines"] {
        assert_eq!(via_tool[key], via_map[key], "{key}");
    }
    // the bridge id of that job answers the bridge's status and names its compile_map job
    let raw = call(&server, "job_status", json!({"jobId": bridge_id})).await.unwrap();
    assert_eq!(raw["compileMapJobId"], json!("c1"));
    assert_eq!(raw["kind"], json!("resourcecompiler"));
    // the list shows the compile_map job with its route
    let all = call(&server, "job_status", json!({})).await.unwrap();
    assert_eq!(all["compileMap"][0]["jobId"], json!("c1"));
    assert_eq!(all["compileMap"][0]["route"], json!("bridge"));
}

#[test]
fn the_job_tools_are_registered_with_their_mutability() {
    use cs2_mcp_common::tools::{Mutability, Registry};
    let mut registry = Registry::new();
    hammer_mcp::tools::jobs::register(&mut registry);
    for (name, mutability) in [
        ("compile_asset", Mutability::Writer),
        ("job_status", Mutability::ReadOnly),
        ("job_cancel", Mutability::Writer),
    ] {
        let spec = registry.spec(name).expect(name);
        assert_eq!(spec.mutability, mutability, "{name}");
    }
    assert_eq!(registry.names(), hammer_mcp::tools::jobs::JOB_TOOL_ORDER.to_vec());
}

// -------------------------------------------------------------------------------------------
// review: lost replies, the shutdown race, the start window, starting-busy, lost bridge
// -------------------------------------------------------------------------------------------

/// A `job.start` whose reply never arrives (the mock answers QueueTimeout AFTER it spawned the
/// job): `compile_asset` finds the job in `job.status {}` and adopts it instead of losing it.
#[tokio::test]
async fn a_lost_start_reply_is_adopted_for_compile_asset() {
    let tree = Tree::new("lost-asset");
    tree.install("materials/x.vmat");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .delay("job.start", 60_000)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let out = call(&server, "compile_asset", json!({"path": "materials/x.vmat"}))
        .await
        .expect("adopted");
    let spawned = mb.bridge_jobs();
    assert_eq!(spawned.len(), 1, "the job was spawned despite the lost reply");
    assert_eq!(out["jobId"], json!(spawned[0].id));
    assert!(!mb.method_calls("job.status").is_empty(), "found via job.status {{}}");
    // and it is cancellable: the id was recorded for the shutdown
    let report = hammer_mcp::shutdown_cleanup(server.state()).await;
    assert_eq!(report["bridge"]["cancelled"][0]["cancelled"], json!(true), "{report}");
}

/// A refusal is definitive (nothing spawned); a lost reply that cannot be matched is an error for
/// callers without a store entry.
#[tokio::test]
async fn a_lost_start_reply_without_a_matching_job_is_an_error_for_compile_asset() {
    let tree = Tree::new("lost-asset-nomatch");
    tree.install("materials/x.vmat");
    let mb = MockBridge::builder()
        .jobs()
        .delay("job.start", 60_000)
        .delay("job.status", 60_000)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let err = call(&server, "compile_asset", json!({"path": "materials/x.vmat"}))
        .await
        .unwrap_err();
    assert!(err.outcome_unknown || err.name == "QueueTimeout", "{err:?}");
}

/// compile_map keeps the entry of a start whose reply was lost and adopts the job on a later poll.
#[tokio::test]
async fn a_lost_start_reply_keeps_the_compile_map_entry_and_a_poll_adopts_the_job() {
    let tree = Tree::new("lost-map");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .delay("job.start", 60_000)
        .delay("job.status", 60_000)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let started = call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap();
    assert_eq!(started["jobId"], json!("c1"));
    assert_eq!(started["bridgeJobId"], Value::Null, "{started}");
    assert!(started["notes"][0].as_str().unwrap().contains("lost"), "{started}");
    // a second start is refused: the id-less entry still counts as running
    mb.clear_delays();
    mb.add_delay("job.start", 60_000);
    let err = call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap_err();
    assert_eq!(err.name, "EngineBusy", "{err:?}");
    // the bridge answers again: the poll adopts the spawned job
    mb.clear_delays();
    let polled = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap();
    assert_eq!(polled["state"], json!("running"), "{polled}");
    assert_eq!(polled["bridgeJobId"], json!(mb.bridge_jobs()[0].id));
    let out = call(&server, "compile_map", json!({"jobId": "c1", "cancel": true})).await.unwrap();
    assert_eq!(out["cancelled"], json!(true), "{out}");
}

/// A transport error while WAITING must not leave the job running: one cancel is tried first.
#[tokio::test]
async fn a_status_transport_error_in_a_file_route_compile_cancels_the_job() {
    let tree = Tree::new("status-lost");
    tree.install("models/x/x.vmdl");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .delay("job.status", 60_000)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let err = call(&server, "model_compile", json!({"path": VMDL})).await.unwrap_err();
    assert!(err.outcome_unknown || err.name == "QueueTimeout", "{err:?}");
    assert_eq!(mb.method_calls("job.cancel").len(), 1);
    assert_eq!(mb.bridge_jobs()[0].state, "cancelled");
}

/// A failed cancel after the tool's deadline is reported.
#[tokio::test]
async fn a_failed_timeout_cancel_is_reported() {
    let tree = Tree::new("cancel-error");
    tree.install("models/x/x.vmdl");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .error("job.cancel", -32603, "boom", None)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let out = call(&server, "model_compile", json!({"path": VMDL, "timeoutMs": 300}))
        .await
        .unwrap();
    assert_eq!(out["timedOut"], json!(true));
    assert!(out["cancelError"].is_string(), "{out}");
}

/// A client-cancelled tool call (the future is dropped) does not leave the compile running.
#[tokio::test]
async fn a_dropped_file_route_compile_cancels_its_job() {
    let tree = Tree::new("dropped");
    tree.install("models/x/x.vmdl");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let dropped = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        call(&server, "model_compile", json!({"path": VMDL, "timeoutMs": 60_000})),
    )
    .await;
    assert!(dropped.is_err(), "the call was still waiting when it was dropped");
    let at = std::time::Instant::now();
    while mb.bridge_jobs()[0].state != "cancelled" {
        assert!(at.elapsed() < std::time::Duration::from_secs(5), "never cancelled");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// The shutdown begins while compile_map's `job.start` is in flight: the shutdown waits for the
/// reply and the job is cancelled, never orphaned.
#[tokio::test]
async fn a_shutdown_during_an_in_flight_compile_map_start_cancels_the_new_job() {
    let tree = Tree::new("race-map");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .delay("job.start", 600)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let (started, _) = tokio::join!(
        call(&server, "compile_map", json!({"path": MAP_ARG})),
        async {
            let at = std::time::Instant::now();
            while mb.method_calls("job.start").is_empty() {
                assert!(at.elapsed() < std::time::Duration::from_secs(5));
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            hammer_mcp::shutdown_cleanup(server.state()).await
        }
    );
    let _ = started;
    assert_eq!(mb.bridge_jobs()[0].state, "cancelled", "the job did not outlive the server");
}

#[tokio::test]
async fn a_shutdown_during_an_in_flight_compile_asset_start_cancels_the_new_job() {
    let tree = Tree::new("race-asset");
    tree.install("materials/x.vmat");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .delay("job.start", 600)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let (started, _) = tokio::join!(
        call(&server, "compile_asset", json!({"path": "materials/x.vmat"})),
        async {
            let at = std::time::Instant::now();
            while mb.method_calls("job.start").is_empty() {
                assert!(at.elapsed() < std::time::Duration::from_secs(5));
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            hammer_mcp::shutdown_cleanup(server.state()).await
        }
    );
    let err = started.unwrap_err();
    assert_eq!(err.data["reason"], json!("shutting_down"), "{err:?}");
    assert_eq!(mb.bridge_jobs()[0].state, "cancelled");
    // and once the shutdown began, a new start is refused outright
    let err = call(&server, "compile_asset", json!({"path": "materials/x.vmat"})).await.unwrap_err();
    assert_eq!(err.data["reason"], json!("shutting_down"));
}

/// A cancel that arrives while the start's reply is still in flight (and the bridge cannot be
/// asked): remembered, answered `cancelRequested`, applied the moment the id is known.
#[tokio::test]
async fn a_cancel_in_the_start_window_is_remembered_and_applied() {
    let tree = Tree::new("window");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .delay("job.start", 600)
        .delay("job.status", 60_000)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let (started, cancel) = tokio::join!(
        call(&server, "compile_map", json!({"path": MAP_ARG})),
        async {
            // the entry is inserted before the request goes out and the mock holds the reply for 600 ms;
            // polling the bridge here would queue behind that request, so just let the call get going
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            call(&server, "compile_map", json!({"jobId": "c1", "cancel": true})).await
        }
    );
    started.expect("start");
    let cancel = cancel.expect("cancel");
    // the bridge client serialises requests, so the cancel may queue behind the start and find
    // the id (cancelled now) or find the window open (remembered): either way it is applied
    assert!(
        cancel["cancelRequested"] == json!(true) || cancel["cancelled"] == json!(true),
        "{cancel}"
    );
    assert_eq!(cancel["route"], json!("bridge"), "never the podman branch: {cancel}");
    mb.clear_delays();
    assert_eq!(mb.bridge_jobs()[0].state, "cancelled", "applied when the reply arrived");
    let polled = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap();
    assert_eq!(polled["state"], json!("cancelled"), "{polled}");
}

#[tokio::test]
async fn a_start_that_collides_with_another_start_is_engine_busy_starting() {
    let tree = Tree::new("starting");
    tree.install("maps/probe.vmap");
    tree.install("materials/x.vmat");
    let mb = MockBridge::builder().jobs().start().await.unwrap();
    mb.set_job_starting(true);
    let server = tree.server(&mb);
    let err = call(&server, "compile_asset", json!({"path": "materials/x.vmat"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "EngineBusy", "{err:?}");
    assert_eq!(err.data["reason"], json!("bridge_job_starting"));
    assert!(err.retry_safe);
    assert!(err.data.get("jobId").is_none());
    let err = call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap_err();
    assert_eq!(err.data["reason"], json!("bridge_job_starting"), "{err:?}");
    // the refused compile_map left no entry behind
    let err = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap_err();
    assert!(err.message.contains("unknown jobId"));
}

#[tokio::test]
async fn concurrent_starts_run_one_job() {
    let tree = Tree::new("concurrent");
    tree.install("maps/probe.vmap");
    tree.install("materials/x.vmat");
    tree.install("materials/y.vmat");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let (a, b) = tokio::join!(
        call(&server, "compile_map", json!({"path": MAP_ARG})),
        call(&server, "compile_map", json!({"path": MAP_ARG})),
    );
    assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(), 1, "{a:?} {b:?}");
    assert_eq!(mb.bridge_jobs().len(), 1);

    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let (a, b) = tokio::join!(
        call(&server, "compile_asset", json!({"path": "materials/x.vmat"})),
        call(&server, "compile_asset", json!({"path": "materials/y.vmat"})),
    );
    assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(), 1, "{a:?} {b:?}");
    assert_eq!(mb.bridge_jobs().len(), 1);
}

/// `job_status` on the bridge id of a compile_map job folds the status into that job like a
/// compile_map poll: the store entry follows.
#[tokio::test]
async fn job_status_on_a_compile_map_bridge_id_folds_into_the_store() {
    let tree = Tree::new("fold");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Failed, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let started = call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap();
    let bridge_id = started["bridgeJobId"].as_str().unwrap().to_string();
    let raw = call(&server, "job_status", json!({"jobId": bridge_id})).await.unwrap();
    assert_eq!(raw["state"], json!("failed"));
    let statuses = mb.method_calls("job.status").len();
    let polled = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap();
    assert_eq!(polled["state"], json!("failed"), "{polled}");
    assert_eq!(mb.method_calls("job.status").len(), statuses, "answered from the folded entry");
}

/// The mock validates like the real bridge: duplicate flags, ranges, missing `input` file, and
/// `control.status` is fed from the job table with `{id, kind, state}`.
#[tokio::test]
async fn the_mock_validates_like_the_bridge_and_feeds_control_status() {
    let tree = Tree::new("mock-fidelity");
    tree.install("materials/x.vmat");
    let mb = MockBridge::builder()
        .job_content_root(tree.app.join("content"))
        .job_script(MockJobOutcome::Hang, 0)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    for (flags, reason) in [
        (json!(["-nav", "-nav"]), "repeated"),
        (json!(["-threads", "65"]), "needs an integer"),
        (json!(["-maxtextureres", "0"]), "needs an integer"),
        (json!(["-threads"]), "needs a value"),
    ] {
        let err = call(&server, "compile_asset", json!({"path": "materials/x.vmat", "flags": flags}))
            .await
            .unwrap_err();
        assert!(err.message.contains(reason), "{flags}: {err:?}");
    }
    let out = call(&server, "compile_asset", json!({"path": "materials/x.vmat"})).await.unwrap();
    let bridge_id = out["jobId"].as_str().unwrap();
    // the log file is <id>.log
    assert!(out["log"].as_str().unwrap().ends_with(&format!("cs2mcp_jobs/{bridge_id}.log")), "{out}");
    let status = call(&server, "job_status", json!({"jobId": bridge_id})).await.unwrap();
    assert_eq!(status["input"], json!("csgo_addons/mcp_test/materials/x.vmat"));
    let jobs = mb.with_state(|s| {
        let mut listed = s.jobs.clone();
        listed.extend(s.bridge_jobs.summaries());
        listed
    });
    assert_eq!(jobs[0], json!({"id": bridge_id, "kind": "resourcecompiler", "state": "running"}));
}

// -------------------------------------------------------------------------------------------
// round 2: clientRef adoption, unreachable bridges, placeholders, the start window
// -------------------------------------------------------------------------------------------

/// N1: only the job carrying OUR clientRef is adopted: a same-input job of another caller (no
/// clientRef, or another caller's) is never taken, no matter how recent it is.
#[tokio::test]
async fn adopt_takes_only_the_job_with_our_client_ref() {
    use hammer_mcp::tools::bridge_job;
    let tree = Tree::new("adopt-ref");
    let mb = MockBridge::builder().jobs().start().await.unwrap();
    let server = tree.server(&mb);
    let state = server.state();
    let input = "csgo_addons/mcp_test/maps/probe.vmap";
    mb.add_foreign_job(input, None, "running");
    let theirs = mb.add_foreign_job(input, Some("other-1"), "running");
    assert!(bridge_job::adopt(state, "hm1-1").await.unwrap().is_none(), "same input is not ours");
    let found = bridge_job::adopt(state, "other-1").await.unwrap().expect("matched by ref");
    assert_eq!(found["jobId"], json!(theirs));
    // and a start sends the clientRef the mock echoes
    let ours = bridge_job::new_client_ref();
    assert!(ours.len() <= 64 && ours.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)));
    assert_ne!(ours, bridge_job::new_client_ref(), "unique per start");
}

/// The lost reply of a compile_map start is adopted by clientRef even with a same-input foreign
/// job on the table (finished, so the bridge accepted our start).
#[tokio::test]
async fn a_lost_start_reply_ignores_a_foreign_same_input_job() {
    let tree = Tree::new("lost-foreign");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .delay("job.start", 60_000)
        .delay("job.status", 60_000)
        .start()
        .await
        .unwrap();
    let foreign = mb.add_foreign_job("csgo_addons/mcp_test/maps/probe.vmap", None, "failed");
    let server = tree.server(&mb);
    call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap();
    mb.clear_delays();
    let polled = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap();
    let ours = mb.bridge_jobs().into_iter().find(|j| j.client_ref.is_some()).expect("ours");
    assert_eq!(polled["bridgeJobId"], json!(ours.id));
    assert_ne!(polled["bridgeJobId"], json!(foreign));
    assert!(ours.client_ref.unwrap().starts_with("hm"));
}

/// N2: an error before the request was written (no bridge) is a refusal, not a lost reply.
#[tokio::test]
async fn a_start_with_no_bridge_is_refused_not_lost() {
    use hammer_mcp::tools::bridge_job::{self, StartFailure};
    let tree = Tree::new("no-bridge");
    let gone = tree.root.join("no-such-discovery.json");
    let server = hammer_mcp::build_server(&tree.config(Some(&gone), 45_000)).expect("server");
    let state = server.state();
    let refusal = bridge_job::start_raw(state, "csgo_addons/mcp_test/maps/probe.vmap", false, &[], None, "hm1-1")
        .await
        .unwrap_err();
    assert!(matches!(refusal, StartFailure::Refused(_)), "{refusal:?}");
    // and compile_asset drops its placeholder: nothing was sent
    tree.install("materials/x.vmat");
    let err = call(&server, "compile_asset", json!({"path": "materials/x.vmat"})).await.unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable", "{err:?}");
    assert!(state.compile_jobs.pending_refs().is_empty());
}

/// #3: a compile_asset whose start reply was lost AND whose adoption failed leaves a placeholder;
/// the shutdown resolves it by clientRef and cancels the job.
#[tokio::test]
async fn a_lost_compile_asset_start_is_cancelled_by_the_shutdown() {
    let tree = Tree::new("placeholder");
    tree.install("materials/x.vmat");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .delay("job.start", 60_000)
        .delay("job.status", 60_000)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    call(&server, "compile_asset", json!({"path": "materials/x.vmat"})).await.unwrap_err();
    assert_eq!(server.state().compile_jobs.pending_refs().len(), 1, "the placeholder stays");
    assert_eq!(mb.bridge_jobs()[0].state, "running", "the job exists, nobody holds its id");
    mb.clear_delays();
    let report = hammer_mcp::shutdown_cleanup(server.state()).await;
    assert_eq!(mb.bridge_jobs()[0].state, "cancelled", "{report}");
    assert!(server.state().compile_jobs.pending_refs().is_empty());
}

/// #3: a dropped compile_asset start future is covered by the same placeholder.
#[tokio::test]
async fn a_dropped_compile_asset_start_is_cancelled_by_the_shutdown() {
    let tree = Tree::new("placeholder-drop");
    tree.install("materials/x.vmat");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .delay("job.start", 500)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let dropped = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        call(&server, "compile_asset", json!({"path": "materials/x.vmat"})),
    )
    .await;
    assert!(dropped.is_err(), "the start was still in flight when the future was dropped");
    let at = std::time::Instant::now();
    while mb.bridge_jobs().is_empty() {
        assert!(at.elapsed() < std::time::Duration::from_secs(5), "the job never appeared");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let report = hammer_mcp::shutdown_cleanup(server.state()).await;
    assert_eq!(mb.bridge_jobs()[0].state, "cancelled", "{report}");
}

/// N5 (#7): the cancel arrives while the job's id is genuinely unknown (reply lost, the bridge
/// cannot be asked): remembered; the next poll adopts by clientRef and applies it.
#[tokio::test]
async fn a_pending_cancel_is_applied_when_a_poll_adopts_the_lost_job() {
    let tree = Tree::new("pending-cancel");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .delay("job.start", 60_000)
        .delay("job.status", 60_000)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let started = call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap();
    assert_eq!(started["bridgeJobId"], Value::Null, "{started}");
    assert_eq!(mb.bridge_jobs()[0].state, "running");
    let cancel = call(&server, "compile_map", json!({"jobId": "c1", "cancel": true})).await.unwrap();
    assert_eq!(cancel["cancelRequested"], json!(true), "{cancel}");
    assert_eq!(cancel["cancelled"], json!(false));
    assert_eq!(cancel["state"], json!("running"));
    assert_eq!(mb.bridge_jobs()[0].state, "running", "nothing could be sent yet");
    mb.clear_delays();
    let polled = call(&server, "compile_map", json!({"jobId": "c1"})).await.unwrap();
    assert_eq!(mb.bridge_jobs()[0].state, "cancelled", "applied on adoption");
    assert_eq!(polled["state"], json!("cancelled"), "{polled}");
}

/// #2: a start reply that is slower than the shutdown's start-wait does not eat the cancels'
/// budget; the report says the start was still in flight, and its job is cancelled when the reply
/// lands.
#[tokio::test]
async fn a_slow_start_reply_does_not_consume_the_cancel_budget() {
    let tree = Tree::new("start-wait");
    tree.install("materials/x.vmat");
    tree.install("maps/probe.vmap");
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Hang, 0)
        .delay("job.start", 4_000)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let (started, report) = tokio::join!(
        call(&server, "compile_asset", json!({"path": "materials/x.vmat"})),
        async {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            hammer_mcp::shutdown_cleanup(server.state()).await
        }
    );
    started.unwrap_err();
    assert_eq!(report["bridge"]["startsStillInFlight"], json!(1), "{report}");
    assert_eq!(report["bridge"]["startWaitMs"], json!(3_000));
    assert_eq!(mb.bridge_jobs()[0].state, "cancelled", "cancelled when the reply landed");
}

#[tokio::test]
async fn particle_compile_set_keeps_earlier_verdicts_when_a_later_job_start_errors() {
    let tree = Tree::new("pcf-midloop");
    let vpcf = |refs: &[&str]| {
        let children: String = refs
            .iter()
            .map(|r| format!("\t\t{{\n\t\t\tm_ChildRef = resource:\"{r}\"\n\t\t}},\n"))
            .collect();
        format!("<!-- kv3 encoding:text:version{{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}} format:vpcf66:version{{dd571be4-586e-4f41-8b2d-5bbc969e1c53}} -->\n{{\n\t_class = \"CParticleSystemDefinition\"\n\tm_Children = \n\t[\n{children}\t]\n}}")
    };
    std::fs::create_dir_all(tree.addon().join("particles/t")).unwrap();
    for (name, refs) in [
        ("parent", vec!["particles/t/child.vpcf"]),
        ("child", vec!["particles/t/leaf.vpcf"]),
        ("leaf", vec![]),
    ] {
        std::fs::write(tree.addon().join(format!("particles/t/{name}.vpcf")), vpcf(&refs)).unwrap();
    }
    // the mock bridge only "knows" the leaf: the second job.start (child) is refused with an
    // InvalidParams error, the third (parent) is never attempted
    let known = tree.root.join("bridge-content");
    std::fs::create_dir_all(known.join("csgo_addons/mcp_test/particles/t")).unwrap();
    std::fs::write(known.join("csgo_addons/mcp_test/particles/t/leaf.vpcf"), b"x").unwrap();
    let mb = MockBridge::builder()
        .job_script(MockJobOutcome::Ok, 0)
        .job_writes([tree.vmdl_c("particles/t/leaf.vpcf")])
        .job_content_root(&known)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb);
    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/parent.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(job_starts(&mb).len(), 2, "leaf ok, child refused, parent never started");
    assert_eq!(out["ok"], json!(false), "{out}");
    let files = out["files"].as_array().unwrap();
    let by = |p: &str| files.iter().find(|f| f["path"] == json!(p)).unwrap_or_else(|| panic!("{p}: {out}"));
    assert_eq!(by("particles/t/leaf.vpcf")["ok"], json!(true), "{out}");
    assert_eq!(by("particles/t/child.vpcf")["ok"], json!(false));
    assert_ne!(by("particles/t/child.vpcf")["reason"], json!("not attempted"));
    assert_eq!(by("particles/t/parent.vpcf")["ok"], json!(false));
    assert_eq!(by("particles/t/parent.vpcf")["reason"], json!("not attempted"));
    assert!(out["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("stopped at")), "{out}");
    assert_eq!(out["compiler"], json!("bridge"));
}
