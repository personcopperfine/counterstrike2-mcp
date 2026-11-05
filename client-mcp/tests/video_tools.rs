//! demo_record_start / demo_record_stop over a MockBridge (exact console.exec commands, gating, the
//! console.log confirmation from the pre-send cursor, the .dem freshness rule) and demo_to_video
//! against a fake podman with fake ffprobe/ffmpeg scripts (pass, fail, skip, refusals, unstaging).
#![allow(clippy::result_large_err)]
use client_mcp::state::{ClientConfig, ClientState};
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::podman::FakePodman;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use serde_json::{json, Value};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const M_CONSOLE_EXEC: [&str; 12] = [
    "control.ping",
    "control.status",
    "control.capabilities",
    "control.version",
    "doc.active",
    "input.set",
    "input.clear",
    "input.status",
    "input.uninstall",
    "test.checkpoint",
    "console.exec",
    "game.tick",
];

const HLTV_START: &str =
    "10/10 18:50:49 [HLTV Server] Recording SourceTV replay from HLTVServerAsync:\n";
const HLTV_DONE: &str = "10/10 18:50:51 [HLTV Server] Completed SourceTV demo \"demo1.dem\":\n";

const PROBE_JSON: &str = r#"{"streams":[
    {"codec_type":"video","codec_name":"av1","width":1024,"height":720,"r_frame_rate":"30/1","nb_read_packets":"5"},
    {"codec_type":"audio","codec_name":"vorbis"}]}"#;

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

fn temp(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let d = std::env::temp_dir().join(format!("video-tools-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// An executable `#!/bin/sh` script.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

async fn call(
    server: &McpServer<ClientState>,
    name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

/// Append `text` to `path` (creating it).
fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

/// A WineTools-like layout with a console.log the test controls.
struct Recorder {
    root: PathBuf,
    csgo: PathBuf,
    console: PathBuf,
}

impl Recorder {
    fn new(tag: &str) -> Recorder {
        let root = temp(tag);
        let csgo = root.join("game/csgo");
        std::fs::create_dir_all(&csgo).unwrap();
        let console = csgo.join("console.log");
        std::fs::write(&console, "").unwrap();
        Recorder {
            root,
            csgo,
            console,
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn bridge_with_console_exec() -> MockBridge {
    MockBridge::builder()
        .methods(M_CONSOLE_EXEC)
        .responder_value("console.exec", json!({"queued": true}))
        .start()
        .await
        .unwrap()
}

// ---- demo_record_start / demo_record_stop -------------------------------------------------------

#[tokio::test]
async fn record_tools_are_gated_on_console_exec_and_send_nothing() {
    let mb = MockBridge::start().await.unwrap(); // recorded caps without console.exec
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
    ]))
    .unwrap();
    let err = call(&server, "demo_record_start", json!({"name": "demo1"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "NotImplemented");
    assert_eq!(err.data["missing"], json!(["console.exec"]));
    let err = call(&server, "demo_record_stop", json!({"name": "demo1"}))
        .await
        .unwrap_err();
    assert_eq!(err.data["missing"], json!(["console.exec"]));
    assert!(mb.method_calls("console.exec").is_empty());
}

#[tokio::test]
async fn record_start_sends_the_exact_command_and_is_confirmed_from_the_cursor() {
    let rec = Recorder::new("start-ok");
    let mb = bridge_with_console_exec().await;
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--console-log",
        rec.console.to_str().unwrap(),
    ]))
    .unwrap();
    // the engine logs the start line shortly after the command arrives
    let console = rec.console.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        append(&console, HLTV_START);
    });
    let out = call(&server, "demo_record_start", json!({"name": "demo1"}))
        .await
        .unwrap();
    let calls = mb.method_calls("console.exec");
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].params["command"],
        json!("tv_delay 0; tv_record demo1")
    );
    assert_eq!(out["recording"], json!(true));
    assert!(out["confirmedLine"]
        .as_str()
        .unwrap()
        .contains("Recording SourceTV replay from"));
    assert!(out["tvEnableBeforeMap"]
        .as_str()
        .unwrap()
        .contains("tv_enable 1"));
    assert!(out["demoPath"]
        .as_str()
        .unwrap()
        .ends_with("game/csgo/demo1.dem"));
}

#[tokio::test]
async fn record_start_reads_the_cursor_before_the_command_is_sent() {
    // a start line that is already in the log is not a confirmation of this command
    let rec = Recorder::new("start-cursor");
    append(&rec.console, HLTV_START);
    let mb = bridge_with_console_exec().await;
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--console-log",
        rec.console.to_str().unwrap(),
    ]))
    .unwrap();
    let err = call(&server, "demo_record_start", json!({"name": "demo1"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "RecordingNotStarted");
    assert!(err.retry_safe);
    assert_eq!(
        mb.method_calls("console.exec").len(),
        1,
        "the command was still sent"
    );
}

#[tokio::test]
async fn record_start_without_the_line_is_recording_not_started_with_the_tv_enable_note() {
    let rec = Recorder::new("start-timeout");
    let mb = bridge_with_console_exec().await;
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--console-log",
        rec.console.to_str().unwrap(),
    ]))
    .unwrap();
    let err = call(&server, "demo_record_start", json!({"name": "demo1"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "RecordingNotStarted");
    assert!(err.retry_safe);
    assert!(err.message.contains("tv_enable 1"), "{}", err.message);
    assert!(err.data["tvEnableBeforeMap"]
        .as_str()
        .unwrap()
        .contains("tv_enable 1"));
}

#[tokio::test]
async fn record_start_without_a_resolvable_log_is_unconfirmed() {
    // no console.log flag, and the default project tree has no console.log
    let proj = temp("start-nolog");
    let mb = bridge_with_console_exec().await;
    // no podman: a running cs2mcp-* session on the host must not supply a console.log
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--project-root",
        proj.to_str().unwrap(),
        "--podman",
        "/nonexistent/podman",
    ]))
    .unwrap();
    let out = call(&server, "demo_record_start", json!({"name": "demo1"}))
        .await
        .unwrap();
    assert_eq!(out["recording"], json!("unconfirmed"));
    assert_eq!(mb.method_calls("console.exec").len(), 1);
    let _ = std::fs::remove_dir_all(&proj);
}

#[tokio::test]
async fn record_names_must_start_alphanumeric_and_fit_in_64_characters() {
    let mb = bridge_with_console_exec().await;
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
    ]))
    .unwrap();
    for bad in [
        "",
        "a;quit",
        "a b",
        "../x",
        "a//b",
        "a.dem",
        "-leading",
        "_leading",
        &"a".repeat(65),
    ] {
        let err = call(&server, "demo_record_start", json!({"name": bad}))
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad:?}");
        let err = call(&server, "demo_record_stop", json!({"name": bad}))
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "stop {bad:?}");
    }
    assert!(mb.method_calls("console.exec").is_empty());
}

#[tokio::test]
async fn record_stop_is_confirmed_by_the_completed_line_and_the_fresh_dem() {
    let rec = Recorder::new("stop-log");
    let mb = bridge_with_console_exec().await;
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--console-log",
        rec.console.to_str().unwrap(),
    ]))
    .unwrap();
    let (console, dem) = (rec.console.clone(), rec.csgo.join("demo1.dem"));
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(&dem, vec![7u8; 5000]).unwrap();
        append(&console, HLTV_DONE);
    });
    let out = call(&server, "demo_record_stop", json!({"name": "demo1"}))
        .await
        .unwrap();
    let calls = mb.method_calls("console.exec");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].params["command"], json!("tv_stoprecord"));
    assert_eq!(out["confirmedBy"], json!("log"));
    assert_eq!(out["bytes"], json!(5000));
    assert!(out["demoPath"]
        .as_str()
        .unwrap()
        .ends_with("game/csgo/demo1.dem"));
}

#[tokio::test]
async fn record_stop_without_the_completed_line_is_demo_not_completed() {
    let rec = Recorder::new("stop-nodone");
    std::fs::write(rec.csgo.join("demo1.dem"), vec![7u8; 5000]).unwrap();
    let mb = bridge_with_console_exec().await;
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--console-log",
        rec.console.to_str().unwrap(),
        "--tool-budget-ms",
        "3000",
    ]))
    .unwrap();
    let err = call(&server, "demo_record_stop", json!({"name": "demo1"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "DemoNotCompleted");
    assert!(err.retry_safe);
    assert_eq!(mb.method_calls("console.exec").len(), 1);
}

#[tokio::test]
async fn record_stop_without_a_log_falls_back_to_the_size_but_rejects_an_old_dem() {
    let proj = temp("stop-stale");
    let csgo = proj.join("workshop/run/app/game/csgo");
    std::fs::create_dir_all(&csgo).unwrap();
    let dem = csgo.join("demo1.dem");
    std::fs::write(&dem, vec![7u8; 5000]).unwrap();
    // make the file look like an earlier recording
    std::fs::OpenOptions::new()
        .write(true)
        .open(&dem)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    let mb = bridge_with_console_exec().await;
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--project-root",
        proj.to_str().unwrap(),
        "--tool-budget-ms",
        "2000",
        // no podman: a running cs2mcp-* session on the host must not supply a console.log
        "--podman",
        "/nonexistent/podman",
    ]))
    .unwrap();
    let err = call(&server, "demo_record_stop", json!({"name": "demo1"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "DemoStale", "{err:?}");
    // a fresh file in the same place is accepted on the size path
    let dem2 = csgo.join("demo2.dem");
    let writer = dem2.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(&writer, vec![1u8; 4000]).unwrap();
    });
    let out = call(&server, "demo_record_stop", json!({"name": "demo2"}))
        .await
        .unwrap();
    assert_eq!(out["confirmedBy"], json!("size"));
    assert_eq!(out["bytes"], json!(4000));
    let _ = std::fs::remove_dir_all(&proj);
}

// ---- demo_to_video against a fake podman -------------------------------------------------------

/// A project with a demo, a fake podman, and optional fake ffprobe/ffmpeg injected by flag.
struct Video {
    fake: FakePodman,
    app: PathBuf,
    csgo: PathBuf,
    bins: PathBuf,
    server: McpServer<ClientState>,
}

impl Video {
    /// `extra`: more config flags. The fake ffprobe/ffmpeg scripts live in `bins` (removed on drop).
    fn new(tag: &str, probe_json: bool, ffmpeg: bool, extra: &[&str]) -> Video {
        let fake = FakePodman::new(tag);
        let proj = fake.dir().join("project");
        let app = proj.join("workshop/run/app");
        let csgo = app.join("game/csgo");
        std::fs::create_dir_all(&csgo).unwrap();
        std::fs::write(csgo.join("vtest_src.dem"), vec![3u8; 20_000]).unwrap();
        let bins = fake.dir().join("bins");
        std::fs::create_dir_all(&bins).unwrap();
        std::fs::write(bins.join("probe.json"), PROBE_JSON).unwrap();
        let ffprobe = script(
            &bins,
            "ffprobe",
            &format!("cat {}", bins.join("probe.json").display()),
        );
        // the fake ffmpeg writes its last argument (the contact sheet path)
        let ffmpeg_bin = script(
            &bins,
            "ffmpeg",
            "for a; do out=$a; done; printf 'PNG' > \"$out\"",
        );
        let cookie = proj.join("cookie");
        std::fs::write(&cookie, "x").unwrap();
        let mut args: Vec<String> = vec![
            "--discovery".into(),
            "/tmp/cs2mcp-no-bridge/session.json".into(),
            "--console-log".into(),
            "/tmp/cs2mcp-no-console.log".into(),
            "--podman".into(),
            fake.bin.display().to_string(),
            "--project-root".into(),
            proj.display().to_string(),
            "--max-sessions".into(),
            "3".into(),
            "--xauthority".into(),
            cookie.display().to_string(),
        ];
        args.push("--ffprobe".into());
        args.push(if probe_json {
            ffprobe.display().to_string()
        } else {
            "/nonexistent/ffprobe".into()
        });
        args.push("--ffmpeg".into());
        args.push(if ffmpeg {
            ffmpeg_bin.display().to_string()
        } else {
            "/nonexistent/ffmpeg".into()
        });
        args.extend(extra.iter().map(|s| s.to_string()));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let server = client_mcp::build_server(&config(&refs)).unwrap();
        Video {
            fake,
            app,
            csgo,
            bins,
            server,
        }
    }

    async fn call(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        call(&self.server, name, args).await
    }

    fn state_file(&self, container: &str) -> PathBuf {
        self.fake.dir().join("state").join(container)
    }

    /// The fake game: the recorder's lines, the webm (when asked), then a clean exit.
    fn play(&self, container: &str, webm: bool) {
        let csgo = self.csgo.clone();
        let console = csgo.join("console.log");
        let state = self.state_file(container);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let lines = "TESTSCRIPT: CheckPoint -> 'VTEST_BEGIN'.\n\
                 [Movie] Started recording movie: vtest_movie.*: framerate 30.000000\n\
                 TESTSCRIPT: CheckPoint -> 'DemoPlaybackStarted'.\n\
                 TESTSCRIPT: CheckPoint -> 'DemoPlaybackFinished'.\n\
                 [Movie] Finishing movie: vtest_movie.* [5 frames @ 30.000000 == 0.166667 seconds]\n\
                 TESTSCRIPT: CheckPoint -> 'VTEST_DONE'.\n\
                 quit\n";
            for chunk in lines.as_bytes().chunks(41) {
                append(&console, std::str::from_utf8(chunk).unwrap());
                tokio::time::sleep(Duration::from_millis(3)).await;
            }
            if webm {
                let mut bytes = vec![0x1A, 0x45, 0xDF, 0xA3];
                bytes.resize(12_000, 0);
                std::fs::write(csgo.join("vtest_movie.webm"), bytes).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            std::fs::write(state, "exited 0").unwrap();
        });
    }

    /// The staged movie suites, by name.
    fn staged_movies(&self) -> Vec<String> {
        std::fs::read_dir(self.app.join("vtest"))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with("movie_") && n.ends_with(".txt"))
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn finish(&self, run_id: &str) -> Value {
        self.call("test_wait", json!({"runId": run_id, "timeoutMs": 20000}))
            .await
            .expect("test_wait")
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.bins);
    }
}

fn find<'a>(status: &'a Value, prefix: &str) -> &'a Value {
    status["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["name"].as_str().unwrap_or("").starts_with(prefix))
        .unwrap_or_else(|| panic!("no check {prefix}: {status}"))
}

#[tokio::test]
async fn demo_to_video_with_ffprobe_and_ffmpeg_passes_and_collects_the_contact_sheet() {
    let v = Video::new("v-pass", true, true, &[]);
    let out = v
        .call(
            "demo_to_video",
            json!({"demo": "vtest_src", "name": "vtest_movie"}),
        )
        .await
        .expect("demo_to_video");
    let run_id = out["runId"].as_str().unwrap().to_string();
    let container = out["container"].as_str().unwrap().to_string();
    assert_eq!(out["timeoutMs"], json!(300_000));
    assert_eq!(
        out["movie"]["playdemo"],
        json!("vtest_src --startmovie vtest_movie webm framerate 30 no_unique_dir")
    );
    v.play(&container, true);
    let status = v.finish(&run_id).await;
    assert_eq!(status["state"], json!("passed"), "{status}");
    assert_eq!(status["skipped"], json!([]));
    for prefix in [
        "marker DemoPlaybackStarted",
        "marker DemoPlaybackFinished",
        "movie recorder finished",
        "vtest_movie.webm written",
        "vtest_movie.webm has the EBML magic",
        "video packets match recorder count",
        "video stream count",
        "r_frame_rate == 30/1",
        "contact sheet",
    ] {
        assert_eq!(
            find(&status, prefix)["passed"],
            json!(true),
            "{prefix}: {status}"
        );
    }
    let artifacts: Vec<&str> = status["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert!(artifacts.contains(&"vtest_movie.webm"), "{artifacts:?}");
    assert!(artifacts.contains(&"contact.png"), "{artifacts:?}");
}

#[tokio::test]
async fn ffprobe_present_with_the_file_missing_fails_the_probe_checks() {
    let v = Video::new("v-nofile", true, true, &[]);
    let out = v
        .call(
            "demo_to_video",
            json!({"demo": "vtest_src", "name": "vtest_movie"}),
        )
        .await
        .unwrap();
    let run_id = out["runId"].as_str().unwrap().to_string();
    v.play(out["container"].as_str().unwrap(), false);
    let status = v.finish(&run_id).await;
    assert_eq!(status["state"], json!("failed"), "{status}");
    for prefix in [
        "video stream count",
        "video packets",
        "r_frame_rate",
        "vtest_movie.webm written",
    ] {
        let c = find(&status, prefix);
        assert_eq!(c["passed"], json!(false), "{prefix}: {status}");
        assert_eq!(c["skipped"], json!(null), "{prefix} is failed, not skipped");
    }
    assert_eq!(status["skipped"], json!([]));
}

#[tokio::test]
async fn absent_ffprobe_and_ffmpeg_skip_their_checks_and_the_run_still_passes() {
    let v = Video::new("v-skip", false, false, &[]);
    let out = v
        .call(
            "demo_to_video",
            json!({"demo": "vtest_src", "name": "vtest_movie"}),
        )
        .await
        .unwrap();
    let run_id = out["runId"].as_str().unwrap().to_string();
    assert_eq!(out["ffprobe"], json!(null));
    v.play(out["container"].as_str().unwrap(), true);
    let status = v.finish(&run_id).await;
    assert_eq!(status["state"], json!("passed"), "{status}");
    let skipped: Vec<&str> = status["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    for prefix in [
        "video stream count",
        "video packets",
        "r_frame_rate",
        "contact sheet",
    ] {
        assert!(
            skipped.iter().any(|s| s.starts_with(prefix)),
            "{prefix}: {skipped:?}"
        );
        let c = find(&status, prefix);
        assert_eq!(c["skipped"], json!(true));
        assert_eq!(c["passed"], json!(false), "a skipped check is never passed");
    }
}

#[tokio::test]
async fn a_second_movie_while_the_first_runs_is_kind_busy_with_the_run_id_and_stages_nothing() {
    let v = Video::new("v-busy", true, true, &[]);
    let first = v
        .call("demo_to_video", json!({"demo": "vtest_src", "name": "m1"}))
        .await
        .unwrap();
    let run_id = first["runId"].as_str().unwrap().to_string();
    let err = v
        .call("demo_to_video", json!({"demo": "vtest_src", "name": "m2"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "KindBusy", "{err:?}");
    assert_eq!(err.data["runId"], json!(run_id));
    assert!(
        v.staged_movies()
            .iter()
            .all(|n| !n.starts_with("movie_m2_")),
        "a refused call writes nothing: {:?}",
        v.staged_movies()
    );
    // end the first run cleanly
    let container = first["container"].as_str().unwrap().to_string();
    std::fs::write(v.state_file(&container), "exited 0").unwrap();
    let status = v.finish(&run_id).await;
    assert_ne!(status["state"], json!("running"), "{status}");
}

#[tokio::test]
async fn demo_paths_are_accepted_inside_csgo_and_refused_elsewhere() {
    let v = Video::new("v-paths", true, true, &[]);
    let dem = v.csgo.join("vtest_src.dem");
    // accepted: the absolute path directly in csgo/
    let out = v
        .call(
            "demo_to_video",
            json!({"demo": dem.display().to_string(), "name": "vtest_movie"}),
        )
        .await
        .expect("an absolute path directly in csgo is accepted");
    let run_id = out["runId"].as_str().unwrap().to_string();
    v.play(out["container"].as_str().unwrap(), true);
    v.finish(&run_id).await;
    // refused: a path that leaves csgo and comes back
    let escape = format!("{}/../csgo/vtest_src.dem", v.csgo.display());
    let err = v
        .call(
            "demo_to_video",
            json!({"demo": escape, "name": "vtest_movie"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
    // refused: a relative .dem path
    let err = v
        .call(
            "demo_to_video",
            json!({"demo": "vtest_src.dem", "name": "vtest_movie"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidParams", "{err:?}");
}

#[tokio::test]
async fn demo_to_video_validates_its_arguments_before_anything_is_staged() {
    let v = Video::new("v-args", true, true, &[]);
    let cases = [
        json!({"demo": "vtest_src", "name": "m", "format": "mp4"}),
        json!({"demo": "vtest_src", "name": "m;quit"}),
        json!({"demo": "vtest_src", "name": "m", "framerate": 0}),
        json!({"demo": "vtest_src", "name": "m", "width": 640}),
        json!({"demo": "vtest_src", "name": "m", "bitrate": 10, "format": "png"}),
        json!({"demo": "vtest_src", "name": "m", "timeoutMs": 0}),
        json!({"demo": "vtest_src", "name": "m", "timeoutMs": 900001}),
        json!({"demo": "a/b", "name": "m"}),
        json!({"demo": "vtest_src", "name": "-m"}),
        json!({"name": "m"}),
    ];
    for args in cases {
        let err = v.call("demo_to_video", args.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{args}");
    }
    assert!(v.staged_movies().is_empty());
}

#[tokio::test]
async fn demo_to_video_runs_under_wine_tools_even_for_the_linux_flavour() {
    // webm is no longer refused on linux-client: the run still goes through the wine-tools launch
    let v = Video::new("v-flavour", true, true, &["--flavour", "linux-client"]);
    let out = v
        .call(
            "demo_to_video",
            json!({"demo": "vtest_src", "name": "vtest_movie"}),
        )
        .await
        .expect("accepted");
    assert_eq!(
        out["runsUnder"],
        json!("wine-tools (every flavour: the engine encoder path)")
    );
    let container = out["container"].as_str().unwrap().to_string();
    assert!(container.starts_with("cs2mcp-wine-tools-"), "{container}");
    v.play(&container, true);
    v.finish(out["runId"].as_str().unwrap()).await;
}

#[tokio::test]
async fn a_refused_launch_unstages_the_movie_suite_but_keeps_a_library_suite() {
    // the launch fails after the suite was staged: the movie suite is removed, the library one stays
    let v = Video::new("v-unstage", true, true, &[]);
    v.fake.fail_verb("run");
    let err = v
        .call(
            "demo_to_video",
            json!({"demo": "vtest_src", "name": "vtest_movie"}),
        )
        .await
        .unwrap_err();
    assert!(err.name != "InvalidParams", "{err:?}");
    assert!(v.staged_movies().is_empty(), "{:?}", v.staged_movies());

    let err = v
        .call("run_testscript", json!({"suite": "move_jump"}))
        .await
        .unwrap_err();
    assert!(err.name != "InvalidParams", "{err:?}");
    assert!(
        v.app.join("vtest/move_jump.txt").is_file(),
        "library suites are kept"
    );
}

#[tokio::test]
async fn test_status_names_its_skipped_checks_at_the_top_level() {
    let v = Video::new("v-skipped-top", false, true, &[]);
    let out = v
        .call(
            "demo_to_video",
            json!({"demo": "vtest_src", "name": "vtest_movie"}),
        )
        .await
        .unwrap();
    let run_id = out["runId"].as_str().unwrap().to_string();
    v.play(out["container"].as_str().unwrap(), true);
    let status = v.finish(&run_id).await;
    let skipped = status["skipped"].as_array().unwrap();
    assert!(!skipped.is_empty());
    assert!(
        skipped
            .iter()
            .all(|s| s.as_str().unwrap().contains("(added)")),
        "{skipped:?}"
    );
}
