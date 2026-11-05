//! `compile_map` tool tests (WP-C compile): start/poll over the fake podman harness (the shape
//! `tests/particle_file.rs` uses), with the mock bridge only when the document route needs it.
//!
//! Pinned from the proven recipe (`~/.tmp/defuse-poc/compile_map.sh` + HANDOFF.md "Compile +
//! playtest"):
//!
//! * start returns `{jobId, state:"running", log}` at once and never runs the compile inside the
//!   tool call; the log lands at `<project>/target/compile-<map>/rc-<jobId>.log`;
//! * the poll walks `running -> ok/failed/timeout`, reports the `OK: N compiled, M failed,
//!   K skipped` summary, the vpk path + mtime-advanced, the log tail and `[FAIL]` lines;
//! * the LINGER quirk: the fake prints the OK line and *sleeps* (the real Wine process lingers) —
//!   the job must still end `ok` quickly after the kill, without waiting for exec exit 137;
//! * a process exit WITHOUT an OK line is `failed`; a deadline with no OK is `timeout`;
//! * the transient `crun: … status: No such file or directory` exec failure is retried exactly
//!   once and the retry decides;
//! * `mode:"nav"` puts `-nav` into the compiler command and refuses when the .vpk does not exist;
//! * the dirty gate (-32016) refuses the active document (and the path-equals-active form);
//! * one compile at a time: a second start while one runs is `EngineBusy` naming the jobId;
//! * the recipe's `pkill -f resourcecompiler.exe` is GONE (review M3): the compile command
//!   exports `CS2MCP_COMPILE_JOB=<jobId>` and the finalizer runs the tagged-kill script, so a
//!   concurrent `model_compile`/`particle_compile` (no tag) is never swept and the job's own
//!   `vrad3.exe` children are reached;
//! * M8: a sub-directory map reports BOTH output candidates and the refreshed one decides
//!   (`vpkForm`), so the flattened `maps/<stem>.vpk` of the recipe script is not a false failure;
//! * M9: a map the bridge's `game.local` reports as in game is refused with a disconnect hint;
//! * local refusals (bad path shape, missing host file, unknown jobId) send nothing to podman.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::{McpServer, ServerIdentity};
use cs2_mcp_common::mock::{make_doc, MockBridge};
use cs2_mcp_common::tools::{CallFailure, Mutability, Registry, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The document path the mock serves (Windows form; `--path-map` maps `z:\app` at the tree).
const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";
const MAP_ARG: &str = "maps/probe.vmap";

struct Tree {
    root: PathBuf,
    did_podman: bool,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "compile-map-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("workshop/run/app/content/csgo_addons/mcp_test/maps"))
            .expect("tmp tree");
        Tree { root, did_podman: false }
    }

    /// A source map file on the host (content is irrelevant: the tool compiles the SAVED file).
    fn install_map(&self, name: &str) -> PathBuf {
        let path = self.root
            .join("workshop/run/app/content/csgo_addons/mcp_test/maps")
            .join(format!("{name}.vmap"));
        std::fs::create_dir_all(path.parent().unwrap()).expect("sub-directory map");
        std::fs::write(&path, b"placeholder").expect("map file");
        path
    }

    /// The path `compile_map` must report as the compile's output (M8 candidates may differ).
    fn vpk(&self) -> PathBuf {
        self.root
            .join("workshop/run/app/game/csgo_addons/mcp_test/maps/probe.vpk")
    }

    /// The path the SUB-directory map's candidates report: the flattened one (the recipe
    /// script's `OUT`) and the compiler's content→game shape.
    fn flat_vpk(&self, name: &str) -> PathBuf {
        self.root
            .join("workshop/run/app/game/csgo_addons/mcp_test/maps")
            .join(format!("{name}.vpk"))
    }

    /// The log `compile_map` reports
    /// (`<project>/target/compile-probe/rc-<jobId>.log`, L3 one log per job).
    fn log(&self, job_id: &str) -> PathBuf {
        self.root
            .join("target/compile-probe")
            .join(format!("rc-{job_id}.log"))
    }

    /// Write the fake podman (scratch `bin/`) whose exec behaves per `plan` line, one per COMPILE
    /// exec call (`ok`, `fail`, `crun`, `slow`, `linger` …), and record each exec's command string.
    ///
    /// The two helper execs `compile_map` runs are recognised by their script text and never
    /// consume a plan line: the orphan scan (`tagged-scan`, its report is `<tree>/scan-out` or the
    /// empty one) and the tagged kill (`tagged-kill`, its report is `<tree>/kill-out` or
    /// `matched=0`). They are logged separately (`bin/scan-argv.log`, `bin/kill-argv.log`).
    fn fake_podman(&mut self, plan: &[&str]) -> PathBuf {
        assert!(!self.did_podman, "one fake podman per tree");
        self.did_podman = true;
        let bin_dir = self.root.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let dir = bin_dir.display().to_string();
        let app = self
            .root
            .join("workshop/run/app")
            .display()
            .to_string();
        let plan = plan.join("\n");
        std::fs::write(bin_dir.join("plan"), format!("{plan}\n")).expect("plan");
        let body = r#"#!/bin/bash
DIR='__DIR__'; APP='__APP__'
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0 ;;
  exec)
    last="${!#}"
    # the helper execs (the orphan scan, the tagged kill): never a plan line
    case "$last" in
      *tagged-scan*)
        n=$(( $(cat "$DIR/scan-count" 2>/dev/null || echo 0) + 1 )); echo $n > "$DIR/scan-count"
        printf '%s' "$last" > "$DIR/scan-argv-$n"
        if [ -f "$DIR/scan-out" ]; then cat "$DIR/scan-out"; else printf 'tagged-scan: jobs=\n'; fi
        exit 0 ;;
      *tagged-kill*)
        n=$(( $(cat "$DIR/kill-count" 2>/dev/null || echo 0) + 1 )); echo $n > "$DIR/kill-count"
        printf '%s' "$last" > "$DIR/kill-argv-$n"
        if [ -f "$DIR/kill-out" ]; then cat "$DIR/kill-out"; else printf 'tagged-kill: matched=0 killed=0 remaining=0\n'; fi
        exit 0 ;;
    esac
    idx=1
    [ -f "$DIR/exec-count" ] && idx=$(( $(cat "$DIR/exec-count") + 1 ))
    echo $idx > "$DIR/exec-count"
    step=$(sed -n "${idx}p" "$DIR/plan" 2>/dev/null)
    : "${step:=noop}"
    printf '%s\n' "$last" > "$DIR/exec-argv-$idx"
    case "$step" in
      noop)
        echo "fake podman: unexpected exec: $last" >&2
        exit 2 ;;
      ok)
        rel="${last##* -i }"
        rel="${rel#/app/content/}"
        out="$APP/game/${rel%.vmap}.vpk"
        mkdir -p "$(dirname "$out")"
        touch "$out"
        printf 'Compiling %s\r\n[cs2-workshop] launching resourcecompiler\r\n' "$rel"
        printf ' OK: 3 compiled, 0 failed, 1 skipped, 0m:05s\r\n'
        printf '-----------------------------------------------------------------\r\n'
        ;;
      oksub)
        # M8: the compiler's own content→game output shape for a sub-directory map
        out="$APP/game/csgo_addons/mcp_test/maps/sub/arena.vpk"
        mkdir -p "$(dirname "$out")"; touch "$out"
        printf ' OK: 3 compiled, 0 failed, 0 skipped, 0m:12s\r\n'
        ;;
      okflat)
        # M8: the recipe script's FLATTENED output shape (compile_map.sh's OUT)
        out="$APP/game/csgo_addons/mcp_test/maps/arena.vpk"
        mkdir -p "$(dirname "$out")"; touch "$out"
        printf ' OK: 3 compiled, 0 failed, 0 skipped, 0m:12s\r\n'
        ;;
      okbadbyte)
        # M1: one raw non-UTF-8 byte in the log (a Wine CP1252 line) must not hide the OK line
        out="$APP/game/csgo_addons/mcp_test/maps/probe.vpk"
        mkdir -p "$(dirname "$out")"; touch "$out"
        printf ' OK: 2 compiled, 0 failed, 1 skipped, 0m:05s\r\n'
        printf '\xff\r\n'
        ;;
      oknovpk)
        printf 'Compiling …\r\n OK: 2 compiled, 0 failed, 1 skipped, 0m:04s\r\n'
        ;;
      fail)
        printf '[FAIL] dependency failed: maps/missing.vmap\r\n'
        printf ' OK: 1 compiled, 2 failed, 0 skipped, 0m:00s\r\n'
        echo "FATAL ERROR: something fatal" >&2
        exit 1 ;;
      crun)
        echo "crun: error opening file /run/user/1000/crun/xyz/crun/status: No such file or directory" >&2
        exit 1 ;;
      death)
        printf 'Compiling maps/probe.vmap\r\n'
        echo "compiler died (exit 3): something killed it" >&2
        exit 3 ;;
      slow)
        sleep 30 ;;
      linger)
        printf 'Compiling …\r\n OK: 3 compiled, 0 failed, 1 skipped, 0m:05s\r\n'
        rel="${last##* -i }"
        out="$APP/game/${rel#/app/content/}"
        out="${out%.vmap}.vpk"
        mkdir -p "$(dirname "$out")"; touch "$out"
        sleep 30 ;;
    esac
    ;;
  *)
    echo "fake podman: unsupported verb $1" >&2
    exit 2 ;;
esac
"#
            .replace("__DIR__", &dir)
            .replace("__APP__", &app);
        let script = bin_dir.join("podman");
        std::fs::write(&script, body).expect("fake podman script");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        script
    }

    /// The recorded bash -c command of the n-th (1-based) COMPILE exec call (the helper execs are
    /// in `bin/scan-argv.log` / `bin/kill-argv.log`).
    fn exec_command(&self, n: usize) -> String {
        std::fs::read_to_string(self.root.join(format!("bin/exec-argv-{n}")))
            .expect("exec argv recording")
    }

    /// How many compile execs the fake saw so far (the `slow`/`ok`/… plan lines).
    fn exec_count(&self) -> usize {
        std::fs::read_to_string(self.root.join("bin/exec-count"))
            .map(|c| c.trim().parse().unwrap_or(0))
            .unwrap_or(0)
    }

    /// The orphan scans the fake saw, in order (the full `bash -c` script, which carries
    /// `tagged-scan`).
    fn scans(&self) -> Vec<String> {
        self.records("bin/scan-count", "bin/scan-argv-")
    }

    /// The tagged kills the fake saw, in order (the full `bash -c` script, which carries
    /// `tagged-kill`).
    fn kills(&self) -> Vec<String> {
        self.records("bin/kill-count", "bin/kill-argv-")
    }

    /// The stored per-call records of one helper exec kind (`bin/<kind>-count` says how many).
    fn records(&self, count_file: &str, prefix: &str) -> Vec<String> {
        let count: usize = std::fs::read_to_string(self.root.join(count_file))
            .ok()
            .and_then(|count| count.trim().parse().ok())
            .unwrap_or(0);
        (1..=count)
            .map(|n| {
                std::fs::read_to_string(self.root.join(format!("{prefix}{n}")))
                    .unwrap_or_default()
            })
            .collect()
    }

    /// What the fake scan reports (the `/proc` walk is not really run): e.g.
    /// `tagged-scan: jobs=c9` for a foreign tag, or any text without the report line to stand in
    /// for a scan that could not run.
    fn scan_says(&self, text: &str) {
        std::fs::write(self.root.join("bin/scan-out"), text).expect("scan report");
    }

    /// What the fake tagged kill reports (`tagged-kill: matched=1 killed=1 remaining=0`).
    fn kill_says(&self, text: &str) {
        std::fs::write(self.root.join("bin/kill-out"), text).expect("kill report");
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn config(tree: &Tree, podman: Option<&Path>, discovery: Option<&Path>) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let app = tree.root.join("workshop/run/app").display().to_string();
    let map = format!("z:\\app={app}");
    let mut args: Vec<String> = vec![
        "--deadline-ms".into(),
        "5000".into(),
        "--tool-budget-ms".into(),
        "5000".into(),
        "--project-root".into(),
        root,
        "--path-map".into(),
        map,
    ];
    if let Some(podman) = podman {
        args.extend(["--podman".into(), podman.display().to_string()]);
    }
    if let Some(discovery) = discovery {
        args.extend(["--discovery".into(), discovery.display().to_string()]);
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

/// Poll `compile_map {jobId}` until the job's state is `want` (bounded waits, no fixed sleeps).
async fn poll_until(
    server: &McpServer<HammerState>,
    job_id: &str,
    want: &str,
    deadline: Duration,
) -> Value {
    let at = std::time::Instant::now();
    loop {
        let out = call(server, "compile_map", json!({"jobId": job_id}))
            .await
            .expect("poll");
        if out["state"] == want {
            return out;
        }
        assert!(
            at.elapsed() < deadline,
            "job {job_id:?} never reached {want}: {out}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A server with only `compile_map` registered (its POC order is pinned elsewhere).
fn compile_server(cfg: &CommonConfig) -> McpServer<HammerState> {
    let state = HammerState::from_config(cfg).expect("state");
    let mut registry = Registry::new();
    hammer_mcp::tools::compile_map::register(&mut registry);
    hammer_mcp::tools::jobs::register(&mut registry);
    McpServer::new(
        registry,
        state,
        ServerIdentity {
            name: "hammer-mcp",
            version: "test",
            instructions: String::new(),
        },
        cfg.tool_budget_ms,
        cfg.deadline_ms,
    )
}

// -------------------------------------------------------------------------------------------
// registration
// -------------------------------------------------------------------------------------------

#[test]
fn registration_declares_the_writer_mutability_and_no_bridge_methods() {
    let mut registry = Registry::new();
    hammer_mcp::tools::compile_map::register(&mut registry);
    let spec = registry.spec("compile_map").expect("registered");
    assert_eq!(spec.mutability, Mutability::Writer, "it writes the vpk (like model_compile)");
    assert!(
        spec.bridge_methods.is_empty(),
        "the compile runs in the container, not through the bridge"
    );
}

// -------------------------------------------------------------------------------------------
// start + poll
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn background_start_then_poll_reaches_ok() {
    let mut tree = Tree::new("ok");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));

    let started = std::time::Instant::now();
    let out = call(
        &server,
        "compile_map",
        json!({"path": MAP_ARG, "timeoutMs": 60_000}),
    )
    .await
    .expect("start");
    // the start did NOT run the compile inside the call: no exit-wait anything
    assert_eq!(out["state"], json!("running"), "the start returns at once");
    assert!(out["jobId"].is_string());
    let job_id = out["jobId"].as_str().unwrap().to_string();
    assert_eq!(out["mode"], json!("full"));
    assert_eq!(out["container"], json!("cs2tools"));
    assert_eq!(out["log"], json!(tree.log(&job_id).display().to_string()));
    assert_eq!(
        out["vpkPath"],
        json!(tree.vpk().display().to_string()),
        "game/csgo_addons/<addon>/maps/<map>.vpk"
    );
    assert_eq!(out["vpkForm"], json!("content-relative"));
    assert!(
        out["vpkCandidates"].as_array().unwrap().len() == 1,
        "a top-level map has one deduped candidate (M8): {out}"
    );
    assert!(out["deadlineMs"].is_u64());
    assert_eq!(
        out["next"][0],
        json!({"tool": "compile_map", "why": out["next"][0]["why"], "args": {"jobId": job_id, "waitMs": 30_000}}),
        "the start says to wait for the job with waitMs"
    );
    let before = started.elapsed();

    let out = poll_until(&server, &job_id, "ok", Duration::from_secs(10)).await;
    assert!(before < Duration::from_secs(2), "the start stayed a start");
    assert_eq!(out["next"][0]["tool"], json!("run_map"), "a successful compile points at run_map");
    assert_eq!(out["next"][0]["args"], json!({"path": out["path"]}), "run_map gets the compiled map");
    assert_eq!(out["compiled"], json!(3));
    assert_eq!(out["failed"], json!(0));
    assert_eq!(out["skipped"], json!(1));
    assert_eq!(out["fatal"], json!(false));
    assert_eq!(out["failLines"], json!([]), "no [FAIL] lines here");
    assert_eq!(out["vpkUpdated"], json!(true), "the fake touched the vpk");
    assert_eq!(out["vpkExists"], json!(true));
    assert!(out["cleanup"].is_object(), "the tagged-kill report: {out}");
    assert!(out["okLine"].as_str().unwrap().contains("OK: 3"), "{out}");
    assert!(
        out["logTail"].as_str().unwrap().contains("OK: 3 compiled"),
        "the tail shows the summary: {out}"
    );
    assert_eq!(
        out["sourceChanged"],
        json!(false),
        "nobody moved the source map under the job"
    );
    // the log is where the recipe kept it, ONE FILE PER JOB (L3)
    assert!(tree.log(&job_id).is_file(), "{}", tree.log(&job_id).display());
    assert!(
        tree.log(&job_id)
            .starts_with(tree.root.join("target/compile-probe")),
        "log under <project>/target/compile-<map>/"
    );
    // the container command is the recipe verbatim, prefixed by the M3 job tag — whose VALUE is
    // unique per process (N1: it starts with the job's id but is not the poll handle itself)
    let command = tree.exec_command(1);
    let exported = command
        .split_once("export CS2MCP_COMPILE_JOB=")
        .and_then(|(_, rest)| rest.split(';').next())
        .unwrap_or_default()
        .to_string();
    assert!(
        exported.starts_with(&format!("{job_id}-")) && exported.len() > job_id.len() + 2,
        "the tag value is per-process unique, prefixed by the jobId: {exported:?} in {command}"
    );
    assert!(
        command.starts_with("export CS2MCP_COMPILE_JOB=") && command.contains("cd /app && ./entrypoint.sh resourcecompiler.exe"),
        "{command}"
    );
    assert!(command.contains("-threads 8 -fshallow -maxtextureres 256 -nop4 -quiet -unbufferedio -noassert "), "{command}");
    assert!(!command.contains(" -game "), "the recipe leaves -game out: {command}");
    assert!(command.contains(" -noassert -i /app/content/csgo_addons/mcp_test/maps/probe.vmap"), "{command}");
    assert!(
        !command.contains("pkill"),
        "M3: no pkill anywhere in the compile command: {command}"
    );
    // the finalizer ran the TAGGED kill (never a bare pkill sweep), and the start first ran the
    // orphan scan (one bounded exec whose report says nothing carries a tag)
    let kills = tree.kills();
    assert_eq!(kills.len(), 1, "one tagged kill for this job: {kills:?}");
    let cleanup = &kills[0];
    assert!(cleanup.contains("tagged-kill"), "{cleanup}");
    // N1: the kill targets exactly the tag the compile exported (one script per tag)
    assert!(
        cleanup.contains(&format!("for want in {exported}")),
        "the kill targets exactly this job's per-process tag: {exported:?} in {cleanup}"
    );
    assert!(
        !cleanup.contains("pkill"),
        "M3: the bare pkill is gone: {cleanup}"
    );
    let scans = tree.scans();
    assert_eq!(scans.len(), 1, "one orphan scan at the start: {scans:?}");
    assert!(
        scans[0].contains("tagged-scan") && scans[0].contains("/proc/[0-9]*"),
        "the scan reads /proc: {scans:?}"
    );
    assert!(
        out["orphanKills"].is_null(),
        "nothing was killed as an orphan: {out}"
    );
    assert!(
        out["notes"].is_null(),
        "an empty scan report is not worth a note: {out}"
    );
}

#[tokio::test]
async fn an_explicit_path_without_a_bridge_compiles_with_a_note() {
    let mut tree = Tree::new("no-bridge");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    // no --discovery: the bridge client cannot connect at all
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(&server, "compile_map", json!({"path": MAP_ARG})).await.expect("start");
    let notes = out["notes"].as_array().expect("the skip note");
    assert!(
        notes.iter().any(|n| n.as_str().unwrap().contains("doc.active")),
        "{notes:?}"
    );
    poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(10)).await;
}

#[tokio::test]
async fn the_active_document_is_the_default_target() {
    let mut tree = Tree::new("doc-default");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .start()
        .await
        .unwrap();
    let server = compile_server(&config(&tree, Some(&podman), Some(&mb.discovery_path)));
    let out = call(&server, "compile_map", json!({})).await.expect("start");
    assert_eq!(out["path"], json!(MAP_ARG), "the active document's map");
    assert!(out["notes"].is_null(), "a clean doc needs no note");
    let out = poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(10)).await;
    assert_eq!(out["compiled"], json!(3));
}

// -------------------------------------------------------------------------------------------
// verdicts
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn reported_failures_are_a_failed_result_with_fail_lines() {
    let mut tree = Tree::new("fail");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["fail"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(&server, "compile_map", json!({"path": MAP_ARG})).await.expect("start");
    let out = poll_until(&server, out["jobId"].as_str().unwrap(), "failed", Duration::from_secs(10)).await;
    assert!(out.get("next").is_none(), "a failed compile does not point at run_map: {out}");
    assert_eq!(out["failed"], json!(2));
    assert_eq!(out["fatal"], json!(true));
    let fail_lines = out["failLines"].as_array().expect("fail lines");
    assert!(
        fail_lines.iter().any(|l| l.as_str().unwrap().contains("[FAIL] dependency failed")),
        "{fail_lines:?}"
    );
    assert!(out["vpkUpdated"] == json!(false), "no vpk was written: {out}");
    assert!(out["vpkExists"] == json!(false));
    // the fake exits right away: the job never starts a new attempt
    assert_eq!(out["attempts"], json!(1));
}

#[tokio::test]
async fn death_without_an_ok_line_is_failed() {
    let mut tree = Tree::new("death");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["death"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(&server, "compile_map", json!({"path": MAP_ARG})).await.expect("start");
    let out = poll_until(&server, out["jobId"].as_str().unwrap(), "failed", Duration::from_secs(10)).await;
    assert!(
        out["reason"].as_str().unwrap().contains("without an OK line"),
        "{out}"
    );
    assert_eq!(out["compiled"], Value::Null, "there was no summary at all");
}

#[tokio::test]
async fn waitms_on_a_start_starts_and_waits_in_one_call() {
    let mut tree = Tree::new("startwait");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(
        &server,
        "compile_map",
        json!({"path": MAP_ARG, "timeoutMs": 60_000, "waitMs": 10_000}),
    )
    .await
    .expect("start+wait");
    assert!(out["jobId"].is_string(), "{out}");
    assert_eq!(out["state"], json!("ok"), "{out}");
    assert_eq!(out["done"], json!(true), "{out}");
}

#[tokio::test]
async fn a_start_with_a_short_wait_returns_not_done_with_the_wait_next() {
    let mut tree = Tree::new("startshort");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["slow", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(
        &server,
        "compile_map",
        json!({"path": MAP_ARG, "timeoutMs": 60_000, "waitMs": 0}),
    )
    .await
    .expect("start+wait");
    let id = out["jobId"].as_str().expect("jobId").to_string();
    assert_eq!(out["state"], json!("running"), "{out}");
    assert_eq!(out["done"], json!(false), "{out}");
    assert_eq!(out["next"][0]["args"], json!({"jobId": id, "waitMs": 30_000}), "{out}");
    let bad = call(&server, "compile_map", json!({"path": MAP_ARG, "waitMs": -1})).await;
    assert!(bad.is_err(), "a bad waitMs on a start is refused before anything starts");
}

#[tokio::test]
async fn deadline_without_ok_is_timeout() {
    let mut tree = Tree::new("timeout");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["slow", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(
        &server,
        "compile_map",
        json!({"path": MAP_ARG, "timeoutMs": 300}),
    )
    .await
    .expect("start");
    let out = poll_until(&server, out["jobId"].as_str().unwrap(), "timeout", Duration::from_secs(10)).await;
    assert!(
        out["reason"].as_str().unwrap().contains("no OK line"),
        "{out}"
    );
    assert!(out["vpkExists"] == json!(false));
}

#[tokio::test]
async fn the_lingering_wine_process_does_not_hold_the_ok_hostage() {
    let mut tree = Tree::new("linger");
    tree.install_map("probe");
    // the fake prints the OK line and sleeps 30 s (the real Wine process lingers, exec exit 137)
    let podman = tree.fake_podman(&["linger", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let at = std::time::Instant::now();
    let out = call(&server, "compile_map", json!({"path": MAP_ARG})).await.expect("start");
    assert_eq!(out["state"], json!("running"));
    let out = poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(10)).await;
    assert_eq!(out["failed"], json!(0));
    let took = at.elapsed();
    assert!(
        took < Duration::from_secs(10),
        "the OK line decided the verdict in {took:?}, the lingering exec did not"
    );
}

#[tokio::test]
async fn the_transient_crun_failure_is_retried_exactly_once() {
    let mut tree = Tree::new("crun");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["crun", "ok", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(&server, "compile_map", json!({"path": MAP_ARG})).await.expect("start");
    let out = poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(10)).await;
    assert_eq!(out["attempts"], json!(2), "the retry is visible");
    assert_eq!(tree.exec_count(), 2, "the compile and the bounded retry");
    assert_eq!(tree.kills().len(), 1, "the finalizer still cleaned up");
    assert!(
        std::fs::read_to_string(tree.root.join("bin/exec-argv-1"))
            .unwrap_or_default()
            .contains("resourcecompiler.exe"),
        "exec 1 was the compile that hit the transient crun failure"
    );
}

// -------------------------------------------------------------------------------------------
// nav mode, one-at-a-time, local refusals
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn nav_without_a_vpk_is_refused_locally() {
    let mut tree = Tree::new("nav-missing");
    tree.install_map("probe");
    let podman = tree.fake_podman(&[]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let err = call(&server, "compile_map", json!({"path": MAP_ARG, "mode": "nav"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "AssetNotFound", "{err:?}");
    assert_eq!(err.code, Some(-32014));
    assert!(err.message.contains("mode full"), "{err:?}");
    assert!(err.retry_safe);
    assert_eq!(tree.exec_count(), 0, "nothing was sent");
    assert!(
        tree.scans().is_empty(),
        "the refusal precedes the container: no orphan scan either"
    );
}

#[tokio::test]
async fn nav_with_a_vpk_passes_the_nav_flag() {
    let mut tree = Tree::new("nav");
    tree.install_map("probe");
    // an existing (possibly stale) vpk is the mode's prerequisite
    std::fs::create_dir_all(tree.vpk().parent().unwrap()).expect("vpk dir");
    std::fs::write(tree.vpk(), b"old").expect("existing vpk");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(&server, "compile_map", json!({"path": MAP_ARG, "mode": "nav"}))
        .await
        .expect("start");
    assert_eq!(out["mode"], json!("nav"));
    poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(10)).await;
    assert!(
        tree.exec_command(1).contains(" -noassert -nav -i "),
        "EXTRA_FLAGS -nav: {}",
        tree.exec_command(1)
    );
}

#[tokio::test]
async fn stages_resolve_to_compiler_order_and_run_with_those_flags() {
    let mut tree = Tree::new("stages");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(&server, "compile_map", json!({"path": MAP_ARG, "stages": ["nav", "world"]}))
        .await
        .expect("start (world is in the list: no vpk needed)");
    assert_eq!(out["mode"], json!("stages"));
    assert_eq!(out["stages"], json!(["world", "nav"]));
    poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(10)).await;
    assert!(
        tree.exec_command(1).contains(" -noassert -world -nav -i "),
        "compiler order: {}",
        tree.exec_command(1)
    );
}

#[tokio::test]
async fn stages_typo_mode_clash_and_missing_vpk_send_nothing() {
    let mut tree = Tree::new("stages-refusals");
    tree.install_map("probe");
    let podman = tree.fake_podman(&[]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let err = call(&server, "compile_map", json!({"path": MAP_ARG, "stages": ["vsi"]}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert_eq!(err.data["didYouMean"], json!("vis"), "{err:?}");
    let err = call(&server, "compile_map", json!({"path": MAP_ARG, "mode": "nav", "stages": ["nav"]}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    assert!(err.message.contains("mutually exclusive"), "{err:?}");
    // no world stage, no existing vpk: refused like nav
    let err = call(&server, "compile_map", json!({"path": MAP_ARG, "stages": ["phys", "nav"]}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "AssetNotFound", "{err:?}");
    assert_eq!(err.data["reason"], json!("no_compiled_vpk"));
    assert_eq!(err.data["stages"], json!(["phys", "nav"]));
    assert_eq!(tree.exec_count(), 0, "nothing was sent");
}

#[tokio::test]
async fn a_second_start_while_one_runs_is_engine_busy() {
    let mut tree = Tree::new("busy");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["slow", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let first = call(&server, "compile_map", json!({"path": MAP_ARG, "timeoutMs": 5000}))
        .await
        .expect("start");
    let first_id = first["jobId"].as_str().unwrap().to_string();
    let err = call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap_err();
    assert_eq!(err.name, "EngineBusy", "{err:?}");
    assert_eq!(err.data["jobId"], json!(first_id), "the refusal names the running job");
    // the first job must still end (the timeout) so the test ends with the store clean
    poll_until(&server, &first_id, "timeout", Duration::from_secs(15)).await;
}

#[tokio::test]
async fn two_concurrent_starts_start_one_compile() {
    // rmcp runs one task per request: two starts racing each other must not pass the
    // running-check together (L7: the check-and-insert is one operation on the job store's own
    // mutex — `CompileJobs::start` — no write gate needed)
    let mut tree = Tree::new("race");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["slow", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let args = || json!({"path": MAP_ARG, "timeoutMs": 3000});
    let (a, b) = tokio::join!(call(&server, "compile_map", args()), call(&server, "compile_map", args()));
    let (start, refused) = match (&a, &b) {
        (Ok(s), Err(e)) | (Err(e), Ok(s)) => (s, e),
        // both refused by something else / both started: either way the invariant broke
        _ => panic!("exactly one start must win: {a:?} / {b:?}"),
    };
    assert_eq!(refused.name, "EngineBusy", "{refused:?}");
    assert_eq!(
        refused.data["jobId"],
        start["jobId"],
        "the refusal names the winner"
    );
    assert_eq!(start["state"], json!("running"), "{start}");
    // exactly one compile exec ran (the spawned task starts it asynchronously: bounded wait)
    let at = std::time::Instant::now();
    while tree.exec_count() == 0 {
        assert!(at.elapsed() < Duration::from_secs(4), "no compile exec ran");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    poll_until(&server, start["jobId"].as_str().unwrap(), "timeout", Duration::from_secs(15)).await;
}

#[tokio::test]
async fn local_refusals_send_nothing() {
    let mut tree = Tree::new("refusals");
    tree.install_map("probe");
    let podman = tree.fake_podman(&[]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    for (args, name) in [
        (json!({"path": "probe.vmap"}), "InvalidParams"),      // the open_map shape gate
        (json!({"path": "maps/../probe.vmap"}), "InvalidParams"),
        (json!({"path": "maps/probe.vmap", "mode": "bogus"}), "InvalidParams"),
        (json!({"path": "maps/probe.vmap", "timeoutMs": 0}), "InvalidParams"),
        (json!({"path": "maps/missing.vmap"}), "AssetNotFound"),  // no host file
        (json!({"jobId": "c77"}), "InvalidParams"),            // unknown job
        (json!({"jobId": "c1", "path": MAP_ARG}), "InvalidParams"), // mixed
    ] {
        let err = call(&server, "compile_map", args.clone()).await.unwrap_err();
        assert_eq!(err.name, name, "{args}");
        assert!(err.retry_safe, "{args}");
    }
    // a known job id polls even after the job is over (idempotent views)
    assert!(tree.exec_count() == 0, "no compile ran: {}", tree.exec_count());
}

#[tokio::test]
async fn dirty_active_document_refusals_match_the_file_route() {
    let mut tree = Tree::new("dirty");
    tree.install_map("probe");
    tree.install_map("other");
    // nothing runs during the refusals, so exec 1 will be the other map's compile
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(true))))
        .start()
        .await
        .unwrap();
    let server = compile_server(&config(&tree, Some(&podman), Some(&mb.discovery_path)));
    // the default target is the active document -> refused, nothing sent
    let err = call(&server, "compile_map", json!({})).await.unwrap_err();
    assert_eq!(err.name, "UnsavedChanges");
    assert_eq!(err.code, Some(-32016));
    assert!(err.message.contains("save_map"), "{err:?}");
    assert_eq!(err.data["path"], json!(MAP_ARG));
    // an explicit path naming the active document is refused the same way (save_map first)
    let err = call(&server, "compile_map", json!({"path": MAP_ARG})).await.unwrap_err();
    assert_eq!(err.name, "UnsavedChanges", "{err:?}");
    assert_eq!(tree.exec_count(), 0, "no compile ran");
    // a DIFFERENT map compiles while the active document is dirty: the compiler reads the SAVED
    // file of a map nobody is editing
    let out = call(&server, "compile_map", json!({"path": "maps/other.vmap"}))
        .await
        .expect("a different map compiles");
    assert_eq!(out["path"], json!("maps/other.vmap"));
    poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(10)).await;
}

#[tokio::test]
async fn a_finished_job_still_polls() {
    let mut tree = Tree::new("finished");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(&server, "compile_map", json!({"path": MAP_ARG})).await.expect("start");
    let id = out["jobId"].as_str().unwrap().to_string();
    poll_until(&server, &id, "ok", Duration::from_secs(10)).await;
    // an unknown id is an argument error that names the known ones
    let err = call(&server, "compile_map", json!({"jobId": "zz"})).await.unwrap_err();
    assert!(err.message.contains(&id), "the known id is listed: {err:?}");
    let again = call(&server, "compile_map", json!({"jobId": id})).await.expect("poll");
    assert_eq!(again["state"], json!("ok"));
    assert_eq!(again["vpkUpdated"], json!(true));
}

// -------------------------------------------------------------------------------------------
// M8: the vpk candidates (the flattened recipe output is NOT a failure; run_map shares the helper)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_subdirectory_map_reports_the_flattened_output_when_the_compiler_writes_it() {
    let mut tree = Tree::new("sub-flatten");
    tree.install_map("sub/arena");
    let podman = tree.fake_podman(&["okflat", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(&server, "compile_map", json!({"path": "maps/sub/arena.vmap"}))
        .await
        .expect("start");
    // the start names both candidates (content-relative first); the flattened is NOT in the way
    assert_eq!(
        out["vpkCandidates"],
        json!([
            {"form": "content-relative", "path": tree.root.join("workshop/run/app/game/csgo_addons/mcp_test/maps/sub/arena.vpk").display().to_string()},
            {"form": "flattened", "path": tree.flat_vpk("arena").display().to_string()},
        ]),
        "the start names both candidates"
    );
    let out = poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(10)).await;
    assert_eq!(out["state"], json!("ok"), "a flattened output is not 'vpk missing'");
    assert_eq!(out["vpkForm"], json!("flattened"), "the refreshed candidate wins");
    assert_eq!(out["vpkPath"], json!(tree.flat_vpk("arena").display().to_string()));
    assert_eq!(out["vpkUpdated"], json!(true));
    assert_eq!(
        out["vpkCandidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["form"] == json!("content-relative"))
            .and_then(|c| c["mtimeAdvanced"].as_bool()),
        Some(false),
        "the stale sibling stays honest"
    );
}

#[tokio::test]
async fn a_subdirectory_map_reports_the_content_relative_output_when_that_is_refreshed() {
    let mut tree = Tree::new("sub-nested");
    tree.install_map("sub/arena");
    let podman = tree.fake_podman(&["oksub", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(&server, "compile_map", json!({"path": "maps/sub/arena.vmap"}))
        .await
        .expect("start");
    let out = poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(10)).await;
    assert_eq!(out["vpkForm"], json!("content-relative"));
    assert_eq!(
        out["vpkPath"],
        json!(tree
            .root
            .join("workshop/run/app/game/csgo_addons/mcp_test/maps/sub/arena.vpk")
            .display()
            .to_string())
    );
}

#[tokio::test]
async fn nav_accepts_either_subdirectory_candidate() {
    let mut tree = Tree::new("nav-flat");
    tree.install_map("sub/arena");
    // only the FLATTENED output exists (the M8 unknown-location case)
    std::fs::create_dir_all(tree.flat_vpk("arena").parent().unwrap()).expect("flat dir");
    std::fs::write(tree.flat_vpk("arena"), b"old").expect("existing flattened vpk");
    let podman = tree.fake_podman(&[]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(&server, "compile_map", json!({"path": "maps/sub/arena.vmap", "mode": "nav"}))
        .await
        .expect("nav must not be refused when a candidate exists");
    assert_eq!(out["mode"], json!("nav"));
    assert_eq!(
        out["vpkForm"],
        json!("content-relative"),
        "the start reports the first candidate; the compile decides the form later"
    );
}

/// M1: one raw non-UTF-8 byte in the log (a Wine CP1252 debug line) must not blank the log —
/// the read is lossy, so the OK line still ends the job `ok`, never a 25-minute false timeout.
#[tokio::test]
async fn a_non_utf8_log_byte_does_not_hide_the_ok_line() {
    let mut tree = Tree::new("bad-byte");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["okbadbyte", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(&server, "compile_map", json!({"path": MAP_ARG})).await.expect("start");
    let out = poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(10)).await;
    assert_eq!(out["state"], json!("ok"));
    assert_eq!(out["compiled"], json!(2));
    assert!(out["okLine"].as_str().unwrap().contains("OK: 2"), "{out}");
}

// -------------------------------------------------------------------------------------------
// M9: compiling a map the engine has mounted
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_map_in_game_is_refused_with_a_disconnect_hint() {
    let mut tree = Tree::new("mounted");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    let mb = MockBridge::builder()
        .responder_value("game.local", json!({"inGame": true, "map": "probe"}))
        .start()
        .await
        .unwrap();
    let server = compile_server(&config(&tree, Some(&podman), Some(&mb.discovery_path)));
    let err = call(&server, "compile_map", json!({"path": MAP_ARG}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "EngineBusy", "{err:?}");
    assert!(
        err.message.contains("disconnect"),
        "the hint is in the error: {err:?}"
    );
    assert_eq!(err.data["reason"], json!("map_in_game"));
    assert_eq!(err.data["gameLocal"]["map"], json!("probe"));
    assert_eq!(
        err.data["useInstead"],
        json!({"tool": "run_map", "why": "unload the engine from the map, then compile", "args": {"unload": true}}),
        "the refusal points at the unload primitive (there is a way to disconnect: run_map unload)"
    );
    assert!(
        err.message.contains("run_map {unload: true}"),
        "and it is named in the message: {err:?}"
    );
    assert_eq!(tree.exec_count(), 0, "nothing ran: the refusal precedes the job");
    // resetting the engine state (any other map, or none) lifts the refusal
    let mb2 = MockBridge::builder()
        .responder_value("game.local", json!({"inGame": false, "map": null}))
        .start()
        .await
        .unwrap();
    let server2 = compile_server(&config(&tree, Some(&podman), Some(&mb2.discovery_path)));
    let out = call(&server2, "compile_map", json!({"path": MAP_ARG, "timeoutMs": 60_000}))
        .await
        .expect("compile when the engine is not in this map");
    poll_until(&server2, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(15)).await;
}

#[tokio::test]
async fn a_game_local_read_failure_is_a_note_not_a_refusal() {
    let mut tree = Tree::new("mounted-blur");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    let mb = MockBridge::builder()
        .responder_value("game.local", json!({"inGame": true, "map": "probe"}))
        .error("game.local", -32008, "QueueTimeout", Some(json!({"reason": "frame frozen"})))
        .start()
        .await
        .unwrap();
    let server = compile_server(&config(&tree, Some(&podman), Some(&mb.discovery_path)));
    let out = call(&server, "compile_map", json!({"path": MAP_ARG, "timeoutMs": 60_000}))
        .await
        .expect("an unreadable game.local is not a refusal");
    let notes = out["notes"].as_array().expect("the skip note");
    assert!(
        notes.iter().any(|n| n.as_str().unwrap().contains("game.local")),
        "{notes:?}"
    );
    poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(15)).await;
}

// -------------------------------------------------------------------------------------------
// L2: a source map that moves under a running job is flagged in the poll
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_source_map_that_moves_under_the_job_is_flagged() {
    let mut tree = Tree::new("moved-vmap");
    let map = tree.install_map("probe");
    let podman = tree.fake_podman(&["slow", "cleanup"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(
        &server,
        "compile_map",
        json!({"path": MAP_ARG, "timeoutMs": 800}),
    )
    .await
    .expect("start");
    // the compile is still running when the source map is written again (a save_map during the
    // job would do this): the poll must flag it
    tokio::time::sleep(Duration::from_millis(120)).await;
    std::fs::write(&map, b"placeholder, second save").expect("a second save");
    let out = poll_until(&server, out["jobId"].as_str().unwrap(), "timeout", Duration::from_secs(15)).await;
    assert_eq!(
        out["sourceChanged"],
        json!(true),
        "the .vmap's mtime moved past the start: {out}"
    );
    assert!(out.get("next").is_none(), "a timed-out compile suggests nothing: {out}");
}

// -------------------------------------------------------------------------------------------
// (a) the orphan check at start, and the shutdown cleanup
// -------------------------------------------------------------------------------------------

/// A tagged tree from a server that is NOT this one (the live case: hammer-mcp exited mid-compile
/// and its `CS2MCP_COMPILE_JOB` tree kept running): the start REPORTS the tag and refuses, instead
/// of putting a second compiler on top of it. Nothing is killed — the job behind the tag is
/// unknowable from here.
#[tokio::test]
async fn a_foreign_tagged_compile_tree_refuses_the_start_and_is_not_killed() {
    let mut tree = Tree::new("foreign-tag");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    // the container already has an orphan from another server (this server's store is empty)
    tree.scan_says("tagged-scan: jobs=c7\n");
    let server = compile_server(&config(&tree, Some(&podman), None));

    let err = call(&server, "compile_map", json!({"path": MAP_ARG}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "EngineBusy", "{err:?}");
    assert!(err.retry_safe);
    assert_eq!(err.data["reason"], json!("foreign_compile_job"));
    assert_eq!(err.data["foreignJobs"], json!(["c7"]));
    assert_eq!(err.data["tags"], json!(["CS2MCP_COMPILE_JOB=c7"]));
    assert!(
        err.message.contains("CS2MCP_COMPILE_JOB=c7"),
        "the refusal names the tag: {err:?}"
    );
    assert!(
        err.data["hint"].as_str().unwrap().contains("wait for the other compile"),
        "the way out is named: {err:?}"
    );
    assert_eq!(tree.exec_count(), 0, "no compiler was started on top of it");
    assert!(
        tree.kills().is_empty(),
        "a foreign job is reported, never auto-killed: {:?}",
        tree.kills()
    );
    assert!(
        server
            .state()
            .compile_jobs
            .ids()
            .is_empty(),
        "the refused start inserted no job"
    );
}

/// A tagged tree whose job THIS server knows (a finished or reaped job whose tree outlived its
/// verdict) IS ended — the id is ours — and the start proceeds.
#[tokio::test]
async fn a_tagged_tree_of_this_servers_own_dead_job_is_ended_and_the_start_proceeds() {
    let mut tree = Tree::new("own-tag");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    tree.scan_says("tagged-scan: jobs=c9\n");
    tree.kill_says("tagged-kill: matched=1 killed=1 remaining=0\n");
    let server = compile_server(&config(&tree, Some(&podman), None));
    // a job this server knows, whose tree outlived it: the id has to be in the store ("c9": the
    // store's own counter still assigns "c1" to the new start)
    server.state().compile_jobs.insert(finished_job(
        "c9",
        "c9",
        "maps/probe.vmap",
        hammer_mcp::tools::compile_map::JobState::Ok,
    ));

    let out = call(
        &server,
        "compile_map",
        json!({"path": MAP_ARG, "timeoutMs": 60_000}),
    )
    .await
    .expect("the orphan of our own job is not a refusal");
    let kills = tree.kills();
    assert_eq!(kills.len(), 1, "{kills:?}");
    assert!(
        kills[0].contains("for want in c9"),
        "the kill targets the orphan's tag exactly: {kills:?}"
    );
    let orphan_kills = out["orphanKills"].as_array().expect("the kill report");
    assert_eq!(orphan_kills[0]["matched"], json!(1));
    assert!(
        out["notes"]
            .as_array()
            .expect("the note")
            .iter()
            .any(|n| n.as_str().unwrap().contains("c9")),
        "{out}"
    );
    poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(10)).await;
}

/// A scan that cannot report (podman error, unexpected output) is a NOTE: the check is
/// best-effort and the start proceeds.
#[tokio::test]
async fn an_unreadable_orphan_scan_is_a_note_not_a_refusal() {
    let mut tree = Tree::new("scan-broken");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    tree.scan_says("bash: /proc: no such file\n");
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(
        &server,
        "compile_map",
        json!({"path": MAP_ARG, "timeoutMs": 60_000}),
    )
    .await
    .expect("the scan's failure is not a refusal");
    let notes = out["notes"].as_array().expect("the note");
    assert!(
        notes.iter().any(|n| n.as_str().unwrap().contains("could not")),
        "{notes:?}"
    );
    poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(10)).await;
}

/// The shutdown cleanup: every job the store leaves WITHOUT a cleanup report gets the tagged
/// kill, bounded, before the process exits (what `main` runs on stdin EOF / SIGTERM) — a
/// `Running` one, and (N5) a job another path closed `failed` but never killed for.
#[tokio::test]
async fn the_shutdown_cleanup_kills_the_tree_of_a_running_job() {
    let mut tree = Tree::new("shutdown");
    let podman = tree.fake_podman(&[]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    // nothing running: no exec at all
    let report = hammer_mcp::shutdown_cleanup(server.state()).await;
    assert_eq!(report["jobs"], json!([]));
    assert_eq!(tree.kills().len(), 0, "{report}");
    assert!(report["note"].as_str().unwrap().contains("nothing to kill"));

    // a running job (a real compile is not needed: the store entry is what the cleanup reads)
    server.state().compile_jobs.insert(finished_job(
        "c1",
        "c1",
        "maps/probe.vmap",
        hammer_mcp::tools::compile_map::JobState::Running,
    ));
    tree.kill_says("tagged-kill: matched=2 killed=2 remaining=0\n");
    let report = hammer_mcp::shutdown_cleanup(server.state()).await;
    assert_eq!(report["jobs"], json!(["c1"]));
    let kills = tree.kills();
    assert_eq!(kills.len(), 1, "{kills:?}");
    assert!(kills[0].contains("for want in c1"), "{kills:?}");
    assert_eq!(report["killed"][0]["matched"], json!(2));
    assert!(report["budgetMs"].as_u64().unwrap() > 0);
    // the store entry is closed, so a poll answers with what happened
    let polled = call(&server, "compile_map", json!({"jobId": "c1"}))
        .await
        .expect("poll");
    assert_eq!(polled["state"], json!("failed"));
    assert!(
        polled["reason"].as_str().unwrap().contains("shut down"),
        "{polled}"
    );

    // N5: a job already closed without a cleanup report gets the SAME kill on shutdown — its
    // tagged tree may still be alive in the container (probe-error/exit-fold/supervisor paths),
    // and after the process is gone nobody could end it as "own"
    let mut tree = Tree::new("shutdown-closed");
    let podman = tree.fake_podman(&[]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    server.state().compile_jobs.insert(finished_job(
        "c3",
        "c3",
        "maps/probe.vmap",
        hammer_mcp::tools::compile_map::JobState::Failed,
    ));
    let report = hammer_mcp::shutdown_cleanup(server.state()).await;
    assert_eq!(report["jobs"], json!(["c3"]));
    let kills = tree.kills();
    assert_eq!(kills.len(), 1, "{kills:?}");
    assert!(kills[0].contains("for want in c3"), "{kills:?}");
    // the closed job's entry is untouched (it already had its verdict)
    let polled = call(&server, "compile_map", json!({"jobId": "c3"}))
        .await
        .expect("poll");
    assert_eq!(polled["state"], json!("failed"));
    assert_eq!(polled["reason"], Value::Null, "not cleaned up again");
}

/// The shutdown cleanup batches same-container jobs into ONE kill script (N5/N6): several jobs
/// whose tags must all be ended do not burn the bounded shutdown budget with one sleep each.
#[tokio::test]
async fn the_shutdown_cleanup_batches_the_kills_of_one_container() {
    let mut tree = Tree::new("shutdown-batch");
    let podman = tree.fake_podman(&[]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    server.state().compile_jobs.insert(finished_job(
        "c1",
        "c1",
        "maps/probe.vmap",
        hammer_mcp::tools::compile_map::JobState::Running,
    ));
    server.state().compile_jobs.insert(finished_job(
        "c2",
        "c2",
        "maps/other.vmap",
        hammer_mcp::tools::compile_map::JobState::Failed,
    ));
    let report = hammer_mcp::shutdown_cleanup(server.state()).await;
    assert_eq!(report["jobs"], json!(["c1", "c2"]), "{report}");
    let kills = tree.kills();
    assert_eq!(
        kills.len(),
        1,
        "ONE batched kill exec for both tags: {kills:?}"
    );
    assert!(
        kills[0].contains("for want in c1 c2") || kills[0].contains("for want in c2 c1"),
        "the batch carries ALL of the container's tags: {kills:?}"
    );
    for entry in report["killed"].as_array().unwrap() {
        assert_eq!(entry["batched"], json!(true), "{entry}");
        assert!(
            entry["note"].as_str().unwrap().contains("batched with"),
            "{entry}"
        );
    }
}

/// A job entry for the store: the fields the cleanup and the poll read are what matter
/// (`container`, `id`, `tag`, `path`, `state`).
fn finished_job(
    id: &str,
    tag: &str,
    path: &str,
    state: hammer_mcp::tools::compile_map::JobState,
) -> hammer_mcp::tools::compile_map::CompileJob {
    hammer_mcp::tools::compile_map::CompileJob {
        id: id.to_string(),
        tag: tag.to_string(),
        state,
        mode: hammer_mcp::tools::compile_map::CompileMode::Full,
        path: path.to_string(),
        addon: "mcp_test".to_string(),
        container: "cs2tools".to_string(),
        log: std::path::PathBuf::from("/nonexistent/compile.log"),
        vpk: vec![],
        vmap_before: None,
        started: std::time::Instant::now(),
        deadline: Duration::from_secs(1500),
        attempts: 1,
        summary: None,
        ok_line: None,
        reason: None,
        cleanup: None,
    }
}



// -------------------------------------------------------------------------------------------
// review-2 (FIX-3): the live NoLocalPlayer semantics and the load-memory fallback (M9 partial
// completion), the per-process tag (N1) and the shutdown for closed jobs (N5)
// -------------------------------------------------------------------------------------------

/// The LIVE semantics (2026-10-15, live/run-3.log): the engine answers `-32024 NoLocalPlayer`
/// when it is in no map at all — that is `inGame:false`, not an "unreadable game.local", so a
/// `compile_map` start proceeds with NO "could not be read" note (the live TODO).
#[tokio::test]
async fn the_no_local_player_answer_is_not_an_unreadable_game_local() {
    let mut tree = Tree::new("no-local-player");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    let mb = MockBridge::builder()
        .error("game.local", -32024, "the engine has no local player", None)
        .start()
        .await
        .unwrap();
    let server = compile_server(&config(&tree, Some(&podman), Some(&mb.discovery_path)));

    let out = call(&server, "compile_map", json!({"path": MAP_ARG, "timeoutMs": 60_000}))
        .await
        .expect("the compile proceeds (the engine is in no map)");
    if let Some(notes) = out["notes"].as_array() {
        assert!(
            !notes.iter().any(|n| n.as_str().unwrap().contains("could not be read")),
            "NoLocalPlayer is a definitive not-in-game answer, not an unread one: {notes:?}"
        );
    }
    poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(15)).await;
}

/// **M9** (the H1 fallback on the compile side): with NO `game.local` on the bridge, this
/// server's own last OK load of the map is enough evidence that the engine holds the compiled
/// `.vpk` mounted — the start is refused with the same `run_map {unload:true}` hint.
#[tokio::test]
async fn a_map_this_server_loaded_is_refused_without_a_bridge_game_local() {
    let mut tree = Tree::new("memory-mounted");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    // a bridge that does NOT expose game.local at all (the H1 case)
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .start()
        .await
        .unwrap();
    let server = compile_server(&config(&tree, Some(&podman), Some(&mb.discovery_path)));
    server.state().last_load.lock().unwrap().replace(
        hammer_mcp::tools::run_map::LastLoad {
            name: "probe".into(),
            vpk_mtime_ms: None,
            console_offset: 0,
            resolved: true,
            state: "ok".into(),
            idle: false,
            in_flight_until: std::time::Instant::now(),
        },
    );

    let err = call(&server, "compile_map", json!({"path": MAP_ARG}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "EngineBusy", "{err:?}");
    assert!(
        err.message.contains("last run_map load"),
        "the memory fallback names its own source: {err:?}"
    );
    assert_eq!(err.data["reason"], json!("map_in_game"));
    assert!(tree.exec_count() == 0, "nothing ran: the refusal precedes the job");
}

// -------------------------------------------------------------------------------------------
// the job abstraction: route fallback and podman-route cancel (the bridge route is
// `tests/bridge_jobs.rs`)
// -------------------------------------------------------------------------------------------

/// A bridge without `job.start` keeps the proven podman route, untouched, and says so.
#[tokio::test]
async fn without_job_start_the_podman_route_runs_and_reports_its_route() {
    let mut tree = Tree::new("route-podman");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    let mb = MockBridge::builder().start().await.unwrap();
    let server = compile_server(&config(&tree, Some(&podman), Some(&mb.discovery_path)));
    let out = call(&server, "compile_map", json!({"path": MAP_ARG, "timeoutMs": 60_000}))
        .await
        .expect("start");
    assert_eq!(out["route"], json!("podman"), "{out}");
    assert_eq!(out["container"], json!("cs2tools"));
    let done = poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(10)).await;
    assert_eq!(done["route"], json!("podman"));
    assert!(done.get("bridgeJobId").is_none());
    assert!(mb.method_calls("job.start").is_empty());
}

/// A bridge that advertises job.start but cannot run the compiler here (Windows-only, compiler
/// missing, job object refused) hands the compile to the podman route, with a note.
#[tokio::test]
async fn a_bridge_that_cannot_run_the_compiler_here_falls_back_to_podman_with_a_note() {
    let mut tree = Tree::new("route-fallback");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["ok", "cleanup"]);
    let mb = MockBridge::builder()
        .job_unavailable("job: resourcecompiler is Windows-only")
        .start()
        .await
        .unwrap();
    let server = compile_server(&config(&tree, Some(&podman), Some(&mb.discovery_path)));
    let out = call(&server, "compile_map", json!({"path": MAP_ARG, "timeoutMs": 60_000}))
        .await
        .expect("start");
    assert_eq!(out["route"], json!("podman"), "{out}");
    let notes = out["notes"].as_array().expect("the fallback note");
    assert!(
        notes.iter().any(|n| n.as_str().unwrap().contains("Windows-only")),
        "{notes:?}"
    );
    assert_eq!(mb.method_calls("job.start").len(), 1, "the bridge was tried first");
    poll_until(&server, out["jobId"].as_str().unwrap(), "ok", Duration::from_secs(10)).await;
}

/// `compile_map {jobId, cancel:true}` on the podman route: the exec is ended, the TAGGED kill runs
/// for that job and the job closes `cancelled` with its cleanup report.
#[tokio::test]
async fn cancel_on_the_podman_route_runs_the_tagged_kill_and_closes_the_job_cancelled() {
    let mut tree = Tree::new("cancel-podman");
    tree.install_map("probe");
    let podman = tree.fake_podman(&["slow"]);
    let server = compile_server(&config(&tree, Some(&podman), None));
    let out = call(&server, "compile_map", json!({"path": MAP_ARG, "timeoutMs": 600_000}))
        .await
        .expect("start");
    let id = out["jobId"].as_str().unwrap().to_string();
    // wait for the compile exec to be running (a signal, not a sleep: the fake records its argv)
    let at = std::time::Instant::now();
    while tree.exec_count() == 0 {
        assert!(at.elapsed() < Duration::from_secs(10), "the exec never started");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tree.kill_says("tagged-kill: matched=2 killed=2 remaining=0\n");

    let cancelled = call(&server, "compile_map", json!({"jobId": id, "cancel": true}))
        .await
        .expect("cancel");
    assert_eq!(cancelled["cancelled"], json!(true), "{cancelled}");
    assert_eq!(cancelled["state"], json!("cancelled"));
    assert_eq!(cancelled["route"], json!("podman"));
    assert_eq!(cancelled["cleanup"]["matched"], json!(2), "{cancelled}");
    assert!(cancelled["reason"].as_str().unwrap().contains("cancelled"));
    let kills = tree.kills();
    assert_eq!(kills.len(), 1, "{kills:?}");
    assert!(kills[0].contains(&format!("for want in {id}-")), "the job's own tag: {kills:?}");

    // finished: the final state again, cancelled:false, no second kill
    let again = call(&server, "job_cancel", json!({"jobId": id})).await.expect("cancel again");
    assert_eq!(again["cancelled"], json!(false));
    assert_eq!(again["state"], json!("cancelled"));
    assert_eq!(tree.kills().len(), 1);
    // job_status of the c-id is the compile_map answer
    let status = call(&server, "compile_map", json!({"jobId": id})).await.unwrap();
    assert_eq!(status["state"], json!("cancelled"));
    assert!(status.get("next").is_none(), "a cancelled compile does not point at run_map: {status}");
    assert!(cancelled.get("next").is_none());
}
