//! run_testscript / test_status / test_wait against a fake podman and a temp project. A test task
//! plays the game: it appends console.log in small chunks (so lines split across polls), writes the
//! screenshots and demo, and then sets the fake container to `exited <code>`. No real container runs.
#![allow(clippy::result_large_err)]
use client_mcp::console::ConsoleLog;
use client_mcp::state::ClientConfig;
use cs2_mcp_common::capture::frame::RgbImage;
use cs2_mcp_common::mock::podman::FakePodman;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

const PASS_LOG: &str = include_str!("../fixtures/testscript/move_jump_pass.console.log");
const NOJUMP_LOG: &str = include_str!("../fixtures/testscript/move_jump_nojump.console.log");

mod common;

fn config(args: &[&str]) -> ClientConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: ClientConfig,
    }
    let mut argv = vec!["t"];
    argv.extend(common::hermetic(args));
    W::parse_from(argv).c
}

struct Env {
    fake: FakePodman,
    proj: PathBuf,
    srv: cs2_mcp_common::mcp::McpServer<client_mcp::state::ClientState>,
}

impl Env {
    fn new(tag: &str) -> Env {
        let fake = FakePodman::new(tag);
        let proj = fake.dir().join("project");
        std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
        let cookie = proj.join("cookie");
        std::fs::write(&cookie, "x").unwrap();
        let bin = fake.bin.display().to_string();
        let proj_s = proj.display().to_string();
        let cookie_s = cookie.display().to_string();
        let cfg = config(&[
            "--discovery",
            "/tmp/cs2mcp-no-bridge/session.json",
            "--console-log",
            "/tmp/cs2mcp-no-console.log",
            "--podman",
            &bin,
            "--project-root",
            &proj_s,
            "--max-sessions",
            "3",
            "--xauthority",
            &cookie_s,
        ]);
        let srv = client_mcp::build_server(&cfg).unwrap();
        Env { fake, proj, srv }
    }

    fn csgo(&self) -> PathBuf {
        self.proj.join("workshop/run/app/game/csgo")
    }

    fn console(&self) -> PathBuf {
        self.csgo().join("console.log")
    }

    fn state_file(&self, container: &str) -> PathBuf {
        self.fake.dir().join("state").join(container)
    }

    async fn call(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        match self.srv.call(name, args).await {
            Ok(out) => Ok(out.json),
            Err(CallFailure::Tool(e)) => Err(e),
            Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
        }
    }

    /// Start a run and return (runId, container, artifactsDir).
    async fn start(&self, args: Value) -> (String, String, PathBuf) {
        let out = self
            .call("run_testscript", args)
            .await
            .expect("run_testscript");
        (
            out["runId"].as_str().unwrap().to_string(),
            out["container"].as_str().unwrap().to_string(),
            PathBuf::from(out["artifactsDir"].as_str().unwrap()),
        )
    }

    /// Wait for the run to finish (or the wait to time out) and return the status.
    async fn finish(&self, run_id: &str) -> Value {
        self.call("test_wait", json!({"runId": run_id, "timeoutMs": 20000}))
            .await
            .expect("test_wait")
    }

    /// The fake game: append `log` in odd-sized chunks, write evidence, then exit with `exit`.
    fn play(&self, container: &str, log: &'static str, exit: &'static str, evidence: bool) {
        let csgo = self.csgo();
        let console = self.console();
        let state = self.state_file(container);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            std::fs::create_dir_all(csgo.join("screenshots")).unwrap();
            for chunk in log.as_bytes().chunks(37) {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&console)
                    .unwrap();
                f.write_all(chunk).unwrap();
                tokio::time::sleep(Duration::from_millis(3)).await;
            }
            if evidence {
                for (i, tag) in ["spawn", "moved", "midjump"].iter().enumerate() {
                    std::fs::write(
                        csgo.join(format!("screenshots/vtest_{tag}_0000.png")),
                        noisy_png(i as u32 + 1),
                    )
                    .unwrap();
                }
                std::fs::write(csgo.join("vtest_move_jump.dem"), vec![3u8; 20_000]).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            std::fs::write(state, exit).unwrap();
        });
    }
}

/// A noisy frame: many colours, many nonblack pixels, a PNG far above the 20000-byte floor.
fn noisy_png(seed: u32) -> Vec<u8> {
    let (w, h) = (320u32, 200u32);
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(7);
    let pixels: Vec<u8> = (0..(w * h * 3))
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x & 0xff) as u8
        })
        .collect();
    RgbImage::new(w, h, pixels).unwrap().to_png().unwrap()
}

fn check_names(status: &Value) -> Vec<(String, bool)> {
    status["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap().to_string(),
                c["passed"].as_bool().unwrap(),
            )
        })
        .collect()
}

fn timeline_names(status: &Value) -> Vec<String> {
    status["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "checkpoint")
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn run_stages_launches_and_returns_before_the_game_finishes() {
    let env = Env::new("ts-start");
    let (run_id, container, artifacts) = env
        .start(json!({"suite": "move_jump", "judge": "move_jump", "timeoutMs": 1500}))
        .await;
    assert!(container.starts_with("cs2mcp-wine-tools-"), "{container}");
    assert!(artifacts.is_dir());

    let staged = env.proj.join("workshop/run/app/vtest/move_jump.txt");
    assert_eq!(
        std::fs::read_to_string(&staged).unwrap(),
        include_str!("../assets/vtest/move_jump.txt")
    );
    for frag in [
        "mark.vtest",
        "setup_player.vtest",
        "pos.vtest",
        "snap.vtest",
    ] {
        assert!(staged.with_file_name(frag).is_file(), "{frag}");
    }

    let runs = env.fake.calls_to("run");
    assert_eq!(runs.len(), 1);
    let cmd = runs[0].argv.last().unwrap();
    assert!(
        cmd.contains("-testscript /app/vtest/move_jump.txt"),
        "{cmd}"
    );
    assert!(!cmd.contains("-testmode"), "{cmd}");

    let status = env
        .call("test_status", json!({"runId": run_id}))
        .await
        .unwrap();
    assert_eq!(status["state"], json!("running"));
    assert!(status.get("checks").is_none());
    assert!(status.get("artifacts").is_none());

    // a second run while this one still runs is refused before anything is staged; the busy
    // container belongs to this run, so the error names it
    let err = env
        .call(
            "run_testscript",
            json!({"suite": "move_jump", "timeoutMs": 1000}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.type_, "KindBusy");
    assert_eq!(err.data["runId"], json!(run_id));
    assert_eq!(err.data["container"], json!(container));

    // the run ends by its own timeout (container stopped): state error, not pending
    let done = env.finish(&run_id).await;
    assert_eq!(done["state"], json!("error"), "{done}");
    assert_eq!(done["pending"], json!(false));
}

#[tokio::test]
async fn a_refused_inline_run_leaves_no_staged_file_behind() {
    let env = Env::new("ts-orphan");
    let (run_id, container, _) = env
        .start(json!({"suite": "sandbox", "timeoutMs": 60000}))
        .await;
    let vtest = env.proj.join("workshop/run/app/vtest");
    let before: Vec<String> = std::fs::read_dir(&vtest)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let err = env
        .call(
            "run_testscript",
            json!({"inline": "map de_dust2", "timeoutMs": 1000}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.type_, "KindBusy");
    assert_eq!(err.data["runId"], json!(run_id));
    let after: Vec<String> = std::fs::read_dir(&vtest)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(before, after, "no inline-*.txt left by a refused run");
    let _ = env
        .call("stop_session", json!({"container": container}))
        .await;
}

#[tokio::test]
async fn a_foreign_wine_session_on_the_app_dir_is_kind_busy_without_a_run_id() {
    let env = Env::new("ts-foreign");
    let app = env.proj.join("workshop/run/app");
    env.fake.add_running("cs2tools");
    env.fake.set_mounts(
        "cs2tools",
        &format!(
            r#"[{{"Type":"bind","Name":"","Source":"{}","Destination":"/app"}}]"#,
            app.display()
        ),
    );
    let err = env
        .call(
            "run_testscript",
            json!({"suite": "move_jump", "timeoutMs": 1000}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.type_, "KindBusy");
    assert_eq!(err.data["container"], json!("cs2tools"));
    assert!(err.data.get("runId").is_none(), "{:?}", err.data);
    assert!(env.fake.calls_to("run").is_empty());
}

#[tokio::test]
async fn stale_evidence_from_an_earlier_run_is_never_judged_or_collected() {
    let env = Env::new("ts-stale");
    // a screenshot and a demo left by an earlier run, older than this call
    let csgo = env.csgo();
    std::fs::create_dir_all(csgo.join("screenshots")).unwrap();
    let old = SystemTime::now() - Duration::from_secs(3600);
    for path in [
        csgo.join("screenshots/vtest_spawn_0000.png"),
        csgo.join("vtest_move_jump.dem"),
    ] {
        std::fs::write(&path, vec![9u8; 30_000]).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    // this run's game writes the log and exits without writing any evidence
    let (run_id, container, artifacts) = env
        .start(
            json!({"suite": "move_jump", "judge": "move_jump", "demo": true, "timeoutMs": 60000}),
        )
        .await;
    env.play(&container, NOJUMP_LOG, "exited 0", false);
    let s = env.finish(&run_id).await;
    assert_eq!(s["state"], json!("failed"), "{s}");
    let artifacts_list: Vec<&str> = s["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        !artifacts_list.iter().any(|n| n.starts_with("vtest_")),
        "{artifacts_list:?}"
    );
    assert!(!artifacts.join("vtest_spawn_0000.png").exists());
    let failing: Vec<String> = check_names(&s)
        .into_iter()
        .filter(|(_, ok)| !ok)
        .map(|(n, _)| n)
        .collect();
    assert!(
        failing
            .iter()
            .any(|n| n.contains("screenshot spawn (missing)")),
        "{failing:?}"
    );
    assert!(
        failing
            .iter()
            .any(|n| n.starts_with("demo recorded (added)")),
        "{failing:?}"
    );
    // the stale files were pruned from the game dir
    assert!(!csgo.join("vtest_move_jump.dem").exists());
    assert!(!csgo.join("screenshots/vtest_spawn_0000.png").exists());
}

#[tokio::test]
async fn the_pass_fixture_read_through_crlf_still_yields_every_position() {
    // a console.log written with CRLF line ends (a Windows engine) is read line by line the same way
    let dir = std::env::temp_dir().join(format!("ts-crlf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("console.log");
    // the POC console.log is CRLF (a Windows engine); the fixture keeps those endings
    assert!(PASS_LOG.contains("\r\n"), "fixture is CRLF");
    std::fs::write(&path, PASS_LOG).unwrap();
    let log = ConsoleLog::new(&path);
    let mut cursor = None;
    let mut lines = Vec::new();
    while let Some((got, next)) = log.read_new_lines(cursor.as_ref()).await.unwrap() {
        if got.is_empty() {
            break;
        }
        lines.extend(got);
        cursor = Some(next);
    }
    assert!(
        lines.iter().all(|l| !l.ends_with('\r')),
        "CR stripped: {:?}",
        lines.iter().find(|l| l.ends_with('\r'))
    );
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let pos = client_mcp::testscript::judge::position_samples(&refs);
    assert_eq!(pos.len(), 4, "{pos:?}");
    assert_eq!(pos["landed"][2], -42.892700);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_quit_that_never_exits_is_stopped_after_the_grace_and_is_an_error() {
    use client_mcp::testscript::run::{drive, Run, Session, Timing};
    use cs2_mcp_common::runtime::docker::{DockerOptions, DockerRuntime};
    let fake = FakePodman::new("ts-stall");
    let proj = fake.dir().join("project");
    std::fs::create_dir_all(proj.join("workshop/run/app/game/csgo")).unwrap();
    let runtime = Arc::new(DockerRuntime::new(DockerOptions::new(
        proj.clone(),
        fake.bin.clone(),
        3,
    )));
    let container = "cs2mcp-wine-tools-stall".to_string();
    fake.set_state(&container, "running 0");
    let csgo = proj.join("workshop/run/app/game/csgo");
    std::fs::write(
        csgo.join("console.log"),
        "TESTSCRIPT: CheckPoint -> 'VTEST_DONE'.\n[HostStateManager] CHostStateMgr::QueueNewRequest( Quitting, 3 )\n",
    )
    .unwrap();
    let run = Arc::new(Run::new(
        "run-stall".into(),
        container.clone(),
        "move_jump".into(),
        None,
        false,
        proj.join("staged.txt"),
        proj.join("artifacts"),
    ));
    let session = Session {
        runtime,
        container: container.clone(),
        console_log: csgo.join("console.log"),
        since: SystemTime::now(),
    };
    let timing = Timing {
        poll: Duration::from_millis(20),
        timeout: Duration::from_secs(60),
        quit_grace: Duration::from_millis(300),
    };
    tokio::time::timeout(Duration::from_secs(20), drive(run.clone(), session, timing))
        .await
        .expect("drive ends after the quit grace");
    let s = run.snapshot();
    assert_eq!(run.state(), client_mcp::testscript::run::RunState::Error);
    let err = s.error.unwrap();
    assert!(err.contains("did not exit"), "{err}");
    assert!(err.contains("stopped"), "{err}");
    assert!(!fake.exists(&container), "the stalled container is removed");
    assert_eq!(fake.calls_to("rm").len(), 1);
    assert!(s.timeline.iter().any(|e| e.kind == "quit"));
}

#[tokio::test]
async fn a_passing_move_jump_run_is_followed_collected_and_judged() {
    let env = Env::new("ts-pass");
    let (run_id, container, artifacts) = env
        .start(
            json!({"suite": "move_jump", "judge": "move_jump", "demo": true, "timeoutMs": 60000}),
        )
        .await;
    env.play(&container, PASS_LOG, "exited 0", true);

    let s = env.finish(&run_id).await;
    assert_eq!(s["state"], json!("passed"), "{s}");
    assert_eq!(s["exitCode"], json!(0));
    assert_eq!(
        timeline_names(&s),
        vec![
            "VTEST_BEGIN",
            "map_load",
            "VTEST_MAP_LOADED",
            "VTEST_SPAWNED",
            "VTEST_POS_spawn",
            "Wrote_Screenshot",
            "VTEST_POS_moved",
            "VTEST_POS_midjump",
            "VTEST_POS_landed",
            "VTEST_DONE",
        ]
    );
    let quit = s["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "quit");
    assert!(quit.is_some(), "quit line recorded");

    // 1 exit + 17 POC checks + 4 added (order, 3 non-blank) + 1 demo
    let checks = check_names(&s);
    assert_eq!(checks.len(), 23, "{checks:#?}");
    assert!(checks.iter().all(|(_, ok)| *ok), "{checks:#?}");

    let artifacts_list: Vec<&str> = s["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        artifacts_list.contains(&"console.log"),
        "{artifacts_list:?}"
    );
    assert!(
        artifacts_list.contains(&"vtest_spawn_0000.png"),
        "{artifacts_list:?}"
    );
    assert!(
        artifacts_list.contains(&"vtest_move_jump.dem"),
        "{artifacts_list:?}"
    );
    assert!(artifacts.join("vtest_move_jump.dem").is_file());
    assert_eq!(
        std::fs::read_to_string(artifacts.join("console.log")).unwrap(),
        PASS_LOG
    );
}

#[tokio::test]
async fn a_jump_that_never_leaves_the_ground_fails_with_the_check_named() {
    let env = Env::new("ts-nojump");
    let (run_id, container, _) = env
        .start(json!({"suite": "move_jump", "judge": "move_jump", "timeoutMs": 60000}))
        .await;
    env.play(&container, NOJUMP_LOG, "exited 0", true);

    let s = env.finish(&run_id).await;
    assert_eq!(s["state"], json!("failed"), "{s}");
    let failing: Vec<String> = check_names(&s)
        .into_iter()
        .filter(|(_, ok)| !ok)
        .map(|(n, _)| n)
        .collect();
    assert_eq!(failing.len(), 2, "{failing:?}");
    assert!(
        failing
            .iter()
            .any(|n| n.starts_with("jump: mid-air z +0.0")),
        "{failing:?}"
    );
    assert!(
        failing.iter().any(|n| n.starts_with("landed: z back down")),
        "{failing:?}"
    );
}

#[tokio::test]
async fn the_hang_guard_exit_code_fails_an_inline_run_without_a_judge() {
    let env = Env::new("ts-hang");
    let (run_id, container, _) = env
        .start(json!({"inline": "testscript_debug 1\nmap de_dust2; mark VTEST_BEGIN\n", "timeoutMs": 60000}))
        .await;
    let staged = env.proj.join("workshop/run/app/vtest");
    let name = std::fs::read_dir(&staged)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("inline-")
        })
        .expect("inline suite staged");
    let text = std::fs::read_to_string(&name).unwrap();
    assert!(
        text.ends_with("quit\nTest_Wait 30\nTest_ExitProcess 3\n"),
        "{text}"
    );
    assert!(text.contains("Test_Run mark VTEST_BEGIN"), "{text}");

    env.play(
        &container,
        "TESTSCRIPT: CheckPoint -> 'VTEST_BEGIN'.\n",
        "exited 3",
        false,
    );
    let s = env.finish(&run_id).await;
    assert_eq!(s["state"], json!("failed"), "{s}");
    assert_eq!(s["exitCode"], json!(3));
    let checks = check_names(&s);
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert!(!checks[0].1);
    assert!(s["checks"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("hang guard"));
}

#[tokio::test]
async fn a_run_that_never_exits_is_stopped_at_its_timeout_and_is_an_error() {
    let env = Env::new("ts-timeout");
    let (run_id, container, _) = env
        .start(json!({"suite": "sandbox", "timeoutMs": 1000}))
        .await;
    let s = env
        .call("test_wait", json!({"runId": run_id, "timeoutMs": 15000}))
        .await
        .unwrap();
    assert_eq!(s["state"], json!("error"), "{s}");
    assert!(s["error"].as_str().unwrap().contains("timed out"), "{s}");
    assert!(s.get("checks").is_none());
    assert!(
        !env.fake.exists(&container),
        "the hung container is removed"
    );
    assert_eq!(env.fake.calls_to("rm").len(), 1);
}

#[tokio::test]
async fn test_wait_is_clamped_to_the_budget_and_reports_pending() {
    let env = Env::new("ts-wait");
    let (run_id, _container, _) = env
        .start(json!({"suite": "sandbox", "timeoutMs": 60000}))
        .await;
    let s = env
        .call("test_wait", json!({"runId": run_id, "timeoutMs": 300}))
        .await
        .unwrap();
    assert_eq!(s["state"], json!("running"));
    assert_eq!(s["pending"], json!(true));
    assert_eq!(s["next"], json!("test_wait {runId}"));
    assert_eq!(s["timeoutMs"], json!(300));
    assert_eq!(s["clampedToBudget"], json!(false));
    assert!(s["waitedMs"].as_u64().unwrap() >= 250);
    let _ = env.call("stop_session", json!({})).await;
}

#[tokio::test]
async fn bad_arguments_and_unknown_runs_never_touch_podman() {
    let env = Env::new("ts-args");
    let cases = [
        json!({}),
        json!({"suite": "move_jump", "inline": "map x"}),
        json!({"suite": "nope"}),
        json!({"suite": "move_jump", "judge": "nope"}),
        json!({"suite": "move_jump", "timeoutMs": 0}),
        json!({"inline": "Test_Checkpoint x"}),
        json!({"inline": "map x", "demo": "yes"}),
    ];
    for args in cases {
        let err = env.call("run_testscript", args.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{args}: {err:?}");
    }
    let unknown = env
        .call("test_status", json!({"runId": "run-404"}))
        .await
        .unwrap_err();
    assert_eq!(unknown.type_, "UnknownRun");
    let missing = env.call("test_status", json!({})).await.unwrap_err();
    assert_eq!(missing.type_, "InvalidArguments");
    assert!(env.fake.calls_to("run").is_empty());
    assert!(!env.proj.join("workshop/run/app/vtest").exists());
}

#[tokio::test]
async fn console_reader_returns_only_complete_lines_and_restarts_on_truncation() {
    let dir = std::env::temp_dir().join(format!("ts-reader-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("console.log");
    let _ = std::fs::remove_file(&path);
    let log = ConsoleLog::new(&path);
    assert!(log.read_new_lines(None).await.unwrap().is_none());

    std::fs::write(&path, "a\nb").unwrap();
    let (lines, cur) = log.read_new_lines(None).await.unwrap().unwrap();
    assert_eq!(lines, vec!["a"]);
    assert_eq!(cur.offset, 2);

    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(b"c\r\n").unwrap();
    drop(f);
    let (lines, cur) = log.read_new_lines(Some(&cur)).await.unwrap().unwrap();
    assert_eq!(lines, vec!["bc"]);
    assert_eq!(cur.offset, 6);

    // truncated and rewritten shorter: the cursor is past the end, so the read starts at 0
    std::fs::write(&path, "x\n").unwrap();
    let (lines, _) = log.read_new_lines(Some(&cur)).await.unwrap().unwrap();
    assert_eq!(lines, vec!["x"]);
    let _ = std::fs::remove_dir_all(&dir);
}
