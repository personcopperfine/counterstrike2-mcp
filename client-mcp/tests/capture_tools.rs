//! `screenshot` and `observe` against a fake podman (session table and mounts) and a fake capture
//! layer: a thread that answers `request.json` the way cs2-vk-capture does (it takes the request by
//! rename, writes each result to a temp name and renames it into place; a PNG, then the result json,
//! or `{id, error}`). No game, no X server.
#![allow(clippy::result_large_err)]
use client_mcp::state::ClientConfig;
use cs2_mcp_common::capture::frame::RgbImage;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::podman::FakePodman;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{CallFailure, ToolError, ToolOutput};
use serde_json::{json, Value};
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const SESSION: &str = "cs2mcp-linux-client-00000001";
const OTHER: &str = "cs2mcp-linux-client-00000002";
const WINE: &str = "cs2mcp-wine-tools-00000003";
const CLIENT_METHODS: [&str; 13] = [
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
    "world.render",
];

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

async fn call(
    server: &McpServer<client_mcp::state::ClientState>,
    name: &str,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

/// What the fake layer answers.
#[derive(Clone, Copy)]
enum Answer {
    /// a 64x32 frame with a colour gradient (many colours, not a loading screen)
    Gradient,
    /// a 1280x720 gradient: large enough to need downscaling for the small inline image
    Big,
    /// a 64x32 frame of one colour (a loading or blank screen)
    Solid([u8; 3]),
    /// `{id, error}`, as the layer answers a capture it cannot make
    Error(&'static str),
}

fn frame(answer: Answer) -> RgbImage {
    match answer {
        Answer::Solid(c) => {
            let px: Vec<u8> = (0..64 * 32).flat_map(|_| c).collect();
            RgbImage::new(64, 32, px).unwrap()
        }
        Answer::Big => gradient(1280, 720),
        _ => gradient(64, 32),
    }
}

fn gradient(w: u32, h: u32) -> RgbImage {
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 40]);
        }
    }
    RgbImage::new(w, h, px).unwrap()
}

/// The layer's own write discipline: temp name, then rename into place.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) {
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, bytes).unwrap();
    std::fs::rename(&tmp, dir.join(name)).unwrap();
}

/// Answer `count` requests in `dir`, then stop. Each request is taken by renaming `request.json`
/// first, so a request published meanwhile is never read twice. Returns the ids answered.
fn spawn_layer(dir: PathBuf, answer: Answer, count: usize) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut answered = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        while answered.len() < count && Instant::now() < deadline {
            let taken = dir.join(format!(".taken-{}", answered.len()));
            if std::fs::rename(dir.join("request.json"), &taken).is_err() {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            let req: Value = serde_json::from_slice(&std::fs::read(&taken).unwrap()).unwrap();
            let id = req["id"].as_str().unwrap().to_string();
            let meta = match answer {
                Answer::Error(msg) => {
                    json!({"id": id, "error": msg, "pid": 4242, "exe": "/usr/bin/cs2"})
                }
                _ => {
                    let img = frame(answer);
                    write_atomic(&dir, &format!("{id}.png"), &img.to_png().unwrap());
                    json!({
                        "id": id,
                        "width": img.width,
                        "height": img.height,
                        "format": "B8G8R8A8_SRGB",
                        "presentIndex": 812,
                        "ms": 1,
                        "pid": 4242,
                        "exe": "/usr/bin/cs2",
                    })
                }
            };
            write_atomic(&dir, &format!("{id}.json"), meta.to_string().as_bytes());
            std::fs::remove_file(&taken).unwrap();
            answered.push(id);
        }
        answered
    })
}

fn bind_mounts(pairs: &[(&Path, &str)]) -> String {
    let v: Vec<Value> = pairs
        .iter()
        .map(|(src, dest)| {
            json!({"Type": "bind", "Name": "", "Source": src.display().to_string(), "Destination": dest})
        })
        .collect();
    Value::Array(v).to_string()
}

/// A container dir with a `capture/` subdir (0700), as a launch with the layer makes it.
fn artifacts_with_capture(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let artifacts = root.join("artifacts").join(format!("{name}-1"));
    std::fs::create_dir_all(&artifacts).unwrap();
    let capture = artifacts.join("capture");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&capture)
        .unwrap();
    (artifacts, capture)
}

struct World {
    fake: FakePodman,
    /// `<artifacts>/capture` of SESSION (0700)
    capture: PathBuf,
    console_log: PathBuf,
    /// a bridge discovery file that does not exist: no bridge
    no_bridge: PathBuf,
    layer: PathBuf,
}

/// One running Linux session with a capture dir, a valid layer dir and a console.log.
fn world(tag: &str) -> World {
    let fake = FakePodman::new(tag);
    let root = fake.dir().join("project");
    let (artifacts, capture) = artifacts_with_capture(&root, SESSION);
    let layer = fake.dir().join("layer");
    std::fs::create_dir_all(&layer).unwrap();
    std::fs::write(layer.join("libcs2_vk_capture.so"), b"so").unwrap();
    std::fs::write(layer.join("VkLayer_cs2mcp_capture.json"), b"{}").unwrap();
    let console_log = fake.dir().join("console.log");
    std::fs::write(
        &console_log,
        "boot line\nTESTSCRIPT: CheckPoint -> 'gt_begin_move'\n",
    )
    .unwrap();
    fake.add_running(SESSION);
    fake.set_mounts(SESSION, &bind_mounts(&[(&artifacts, "/work")]));
    World {
        no_bridge: fake.dir().join("no-bridge").join("session.json"),
        fake,
        capture,
        console_log,
        layer,
    }
}

fn make_server(
    w: &World,
    discovery: &Path,
    extra: &[&str],
) -> McpServer<client_mcp::state::ClientState> {
    let bin = w.fake.bin.display().to_string();
    let layer = w.layer.display().to_string();
    let disc = discovery.display().to_string();
    let console = w.console_log.display().to_string();
    let proj = w.fake.dir().join("project").display().to_string();
    let mut argv: Vec<&str> = vec![
        "--discovery",
        &disc,
        "--console-log",
        &console,
        "--podman",
        &bin,
        "--project-root",
        &proj,
        "--vk-layer-dir",
        &layer,
        "--max-sessions",
        "4",
    ];
    argv.extend_from_slice(extra);
    client_mcp::build_server(&config(&argv)).unwrap()
}

/// Set the mtime of a result file to `secs` seconds before now.
fn age(path: &Path, secs: u64) {
    let t = SystemTime::now() - Duration::from_secs(secs);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

fn json_ids(dir: &Path) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .filter_map(|n| n.strip_suffix(".json").map(str::to_string))
        .collect();
    ids.sort();
    ids
}

// ----- screenshot -------------------------------------------------------------------------------

#[tokio::test]
async fn screenshot_returns_the_layer_frame_with_its_metadata_and_prunes_old_captures() {
    let w = world("shot-ok");
    // 52 captures from earlier calls, oldest first: the cleanup keeps the newest 50 overall
    for i in 0..52u64 {
        let id = format!("old-{i:02}");
        std::fs::write(w.capture.join(format!("{id}.png")), b"p").unwrap();
        std::fs::write(w.capture.join(format!("{id}.json")), "{}").unwrap();
        age(&w.capture.join(format!("{id}.json")), 10_000 - i);
        age(&w.capture.join(format!("{id}.png")), 10_000 - i);
    }
    let layer = spawn_layer(w.capture.clone(), Answer::Gradient, 1);
    let server = make_server(&w, &w.no_bridge, &[]);
    let out = call(&server, "screenshot", json!({})).await.unwrap();
    let id = layer.join().unwrap().pop().expect("the layer answered");

    assert_eq!(out.json["session"], json!(SESSION));
    assert_eq!(out.json["pid"], json!(4242));
    assert_eq!(out.json["exe"], json!("/usr/bin/cs2"));
    assert_eq!(out.json["presentIndex"], json!(812));
    assert_eq!(out.json["requestId"], json!(id));
    assert_eq!(out.json["stats"]["width"], json!(64));
    assert_eq!(out.json["stats"]["height"], json!(32));
    assert!(out.json["stats"]["nonblackFrac"].as_f64().unwrap() > 0.9);
    assert_eq!(out.images.len(), 1);
    assert_eq!(out.images[0].mime, "image/png");
    assert!(out.json["path"]
        .as_str()
        .unwrap()
        .ends_with(&format!("{id}.png")));

    // 53 ids existed (52 old + this one); the 3 oldest are gone, the new one is kept
    let ids = json_ids(&w.capture);
    assert_eq!(ids.len(), 50, "{ids:?}");
    assert!(ids.contains(&id));
    assert!(!ids.contains(&"old-00".to_string()));
    assert!(ids.contains(&"old-51".to_string()));
}

#[tokio::test]
async fn screenshot_without_an_answer_is_capture_unavailable_and_withdraws_the_request() {
    let w = world("shot-timeout");
    let server = make_server(&w, &w.no_bridge, &[]);
    let err = call(&server, "screenshot", json!({"timeoutMs": 300}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "CaptureUnavailable");
    assert_eq!(err.data["timeoutMs"], json!(300));
    assert!(err.data["hint"]
        .as_str()
        .unwrap()
        .contains("session_status"));
    assert!(err.data["hint"].as_str().unwrap().contains("rootless"));
    // no request left behind for a layer that answers later
    assert!(!w.capture.join("request.json").exists());
}

#[tokio::test]
async fn timeout_is_clamped_to_the_call_budget() {
    let w = world("shot-clamp");
    // a 3 s tool budget leaves at most 2 s after the 1 s margin
    let server = make_server(&w, &w.no_bridge, &["--tool-budget-ms", "3000"]);
    let started = Instant::now();
    let err = call(&server, "screenshot", json!({"timeoutMs": 60000}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "CaptureUnavailable", "{}", err.message);
    let asked = err.data["timeoutMs"].as_u64().unwrap();
    assert!(asked <= 2000, "asked {asked} ms of a 3 s budget");
    assert!(
        started.elapsed() < Duration::from_millis(2900),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn concurrent_screenshots_of_one_session_are_serialised_and_both_answer() {
    let w = world("shot-concurrent-same");
    let layer = spawn_layer(w.capture.clone(), Answer::Gradient, 2);
    let server = make_server(&w, &w.no_bridge, &[]);
    let (a, b) = tokio::join!(
        call(&server, "screenshot", json!({})),
        call(&server, "screenshot", json!({})),
    );
    let ids = layer.join().unwrap();
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(ids.len(), 2);
    assert_ne!(a.json["requestId"], b.json["requestId"]);
    assert!(!w.capture.join("request.json").exists());
}

#[tokio::test]
async fn concurrent_screenshots_of_two_sessions_use_their_own_dirs() {
    let w = world("shot-concurrent-two");
    let root = w.capture.parent().unwrap().parent().unwrap().to_path_buf();
    let (artifacts_b, capture_b) = artifacts_with_capture(&root, OTHER);
    w.fake.add_running(OTHER);
    w.fake
        .set_mounts(OTHER, &bind_mounts(&[(&artifacts_b, "/work")]));
    let layer_a = spawn_layer(w.capture.clone(), Answer::Gradient, 1);
    let layer_b = spawn_layer(capture_b.clone(), Answer::Solid([9, 9, 9]), 1);
    let server = make_server(&w, &w.no_bridge, &[]);
    let (a, b) = tokio::join!(
        call(&server, "screenshot", json!({"session": SESSION})),
        call(&server, "screenshot", json!({"session": OTHER})),
    );
    layer_a.join().unwrap();
    layer_b.join().unwrap();
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.json["session"], json!(SESSION));
    assert_eq!(b.json["session"], json!(OTHER));
    assert!(a.json["stats"]["uniqueColors"].as_u64().unwrap() > 1);
    assert_eq!(b.json["stats"]["uniqueColors"], json!(1));
}

#[tokio::test]
async fn layer_error_is_capture_failed_with_the_layers_message() {
    let w = world("shot-layer-error");
    let layer = spawn_layer(w.capture.clone(), Answer::Error("no swapchain"), 1);
    let server = make_server(&w, &w.no_bridge, &[]);
    let err = call(&server, "screenshot", json!({})).await.unwrap_err();
    layer.join().unwrap();
    assert_eq!(err.type_, "CaptureFailed");
    assert!(err.message.contains("no swapchain"), "{}", err.message);
    assert_eq!(err.data["layerError"], json!(err.message));
}

#[tokio::test]
async fn image_modes_full_maxwidth_and_small() {
    let w = world("shot-image");
    let layer = spawn_layer(w.capture.clone(), Answer::Big, 3);
    let server = make_server(&w, &w.no_bridge, &[]);
    // default: full size inline (1280 wide is under the 1920x1080 policy)
    let full = call(&server, "screenshot", json!({})).await.unwrap();
    assert_eq!(full.json["inline"]["width"], json!(1280));
    assert_eq!(full.json["stats"]["width"], json!(1280));
    // maxWidth: narrower inline image, the path stays the full frame
    let narrow = call(&server, "screenshot", json!({"maxWidth": 320}))
        .await
        .unwrap();
    assert_eq!(narrow.json["inline"]["width"], json!(320));
    assert_eq!(narrow.json["inline"]["downscaled"], json!(true));
    assert_eq!(narrow.json["stats"]["width"], json!(1280));
    // observe default: small (<= 640 px)
    let small = call(&server, "observe", json!({})).await.unwrap();
    layer.join().unwrap();
    assert_eq!(small.json["capture"]["inline"]["width"], json!(640));
    assert_eq!(small.json["capture"]["inline"]["height"], json!(360));
    assert_eq!(small.images.len(), 1);
    assert!(small.json["capture"]["path"]
        .as_str()
        .unwrap()
        .ends_with(".png"));
    assert_eq!(small.json["capture"]["stats"]["width"], json!(1280));
}

#[tokio::test]
async fn observe_image_none_takes_the_frame_but_inlines_nothing() {
    let w = world("observe-image-none");
    let layer = spawn_layer(w.capture.clone(), Answer::Gradient, 1);
    let server = make_server(&w, &w.no_bridge, &[]);
    let out = call(&server, "observe", json!({"image": "none"}))
        .await
        .unwrap();
    layer.join().unwrap();
    assert!(out.images.is_empty());
    assert_eq!(out.json["capture"]["inline"], Value::Null);
    assert!(out.json["capture"]["path"]
        .as_str()
        .unwrap()
        .ends_with(".png"));
    let err = call(&server, "observe", json!({"image": "huge"}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
}

#[tokio::test]
async fn sessions_that_cannot_be_captured_are_named_before_any_request() {
    // no capture dir: the session was launched with capture: false (or without a layer dir)
    let w = world("shot-no-capture-dir");
    std::fs::remove_dir(&w.capture).unwrap();
    let server = make_server(&w, &w.no_bridge, &[]);
    let err = call(&server, "screenshot", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "CaptureUnavailable");
    assert!(err.message.contains("without capture"), "{}", err.message);

    // stopped session, named: the layer is not loaded
    let w = world("shot-stopped");
    w.fake.set_state(SESSION, "exited 0");
    let server = make_server(&w, &w.no_bridge, &[]);
    let err = call(&server, "screenshot", json!({"session": SESSION}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "CaptureUnavailable");
    assert!(err.message.contains("not running"), "{}", err.message);

    // nothing running and no name
    let w = world("shot-none");
    w.fake.remove(SESSION);
    let server = make_server(&w, &w.no_bridge, &[]);
    let err = call(&server, "screenshot", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "CaptureUnavailable");
    assert!(
        err.message.contains("no running session"),
        "{}",
        err.message
    );

    // a named session that does not exist is an argument error
    let err = call(&server, "screenshot", json!({"session": "cs2mcp-nope-1"}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
}

#[tokio::test]
async fn several_running_sessions_need_a_name_unless_only_one_can_capture() {
    let w = world("shot-two");
    let root = w.capture.parent().unwrap().parent().unwrap().to_path_buf();
    let (artifacts_b, _capture_b) = artifacts_with_capture(&root, OTHER);
    w.fake.add_running(OTHER);
    w.fake
        .set_mounts(OTHER, &bind_mounts(&[(&artifacts_b, "/work")]));
    let server = make_server(&w, &w.no_bridge, &[]);
    // both have capture dirs: ambiguous
    let err = call(&server, "screenshot", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(
        err.message.contains(SESSION) && err.message.contains(OTHER),
        "{}",
        err.message
    );

    // only SESSION has a capture dir now: it is the one
    std::fs::remove_dir_all(artifacts_b.join("capture")).unwrap();
    let layer = spawn_layer(w.capture.clone(), Answer::Gradient, 1);
    let out = call(&server, "screenshot", json!({})).await.unwrap();
    layer.join().unwrap();
    assert_eq!(out.json["session"], json!(SESSION));
}

#[tokio::test]
async fn a_wine_session_with_app_and_work_mounted_captures_from_its_work_dir() {
    let w = world("shot-wine");
    let root = w.capture.parent().unwrap().parent().unwrap().to_path_buf();
    let (artifacts, capture) = artifacts_with_capture(&root, WINE);
    let app = root.join("app");
    std::fs::create_dir_all(app.join("game/csgo")).unwrap();
    w.fake.add_running(WINE);
    w.fake
        .set_mounts(WINE, &bind_mounts(&[(&app, "/app"), (&artifacts, "/work")]));
    let layer = spawn_layer(capture.clone(), Answer::Gradient, 1);
    let server = make_server(&w, &w.no_bridge, &[]);
    let out = call(&server, "screenshot", json!({"session": WINE}))
        .await
        .unwrap();
    layer.join().unwrap();
    assert_eq!(out.json["session"], json!(WINE));
    assert!(out.json["path"]
        .as_str()
        .unwrap()
        .starts_with(capture.to_str().unwrap()));
}

// ----- observe ----------------------------------------------------------------------------------

fn append(path: &Path, text: &str) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

#[tokio::test]
async fn observe_keeps_the_other_parts_when_the_capture_fails() {
    let w = world("observe-partial");
    std::fs::remove_dir(&w.capture).unwrap();
    let server = make_server(&w, &w.no_bridge, &[]);
    let out = call(&server, "observe", json!({"consoleLines": 5}))
        .await
        .unwrap();

    let errors = out.json["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0]["part"], json!("capture"));
    assert_eq!(errors[0]["type"], json!("CaptureUnavailable"));
    // the console part still answered: lines, checkpoint events, cursor
    let lines = out.json["console"]["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(out.json["events"][0]["name"], json!("gt_begin_move"));
    assert!(out.json["console"]["cursor"]["offset"].as_u64().unwrap() > 0);
    // no screenshot, no image, no tick (no session to tick): each with a note
    assert_eq!(out.json["capture"], Value::Null);
    assert!(out.images.is_empty());
    assert_eq!(out.json["tick"], Value::Null);
    assert!(out.json["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n.as_str().unwrap().contains("game.tick skipped")));
    assert_eq!(out.json["screen"], json!("unknown"));
}

#[tokio::test]
async fn observe_reads_only_the_lines_since_the_cursor_and_sees_a_blank_frame_as_loading() {
    let w = world("observe-since");
    let layer = spawn_layer(w.capture.clone(), Answer::Solid([0, 0, 0]), 1);
    let server = make_server(&w, &w.no_bridge, &[]);
    let first = call(&server, "observe", json!({})).await.unwrap();
    layer.join().unwrap();
    assert!(
        first.json["errors"].as_array().unwrap().is_empty(),
        "{}",
        first.json
    );
    assert_eq!(first.json["screen"], json!("loading"));
    assert_eq!(first.json["screenBasis"]["uniqueColors"], json!(1));
    assert_eq!(first.images.len(), 1);
    assert_eq!(first.json["session"], json!(SESSION));
    let cursor = first.json["console"]["cursor"].clone();

    append(&w.console_log, "TESTSCRIPT: CheckPoint -> 'gt_pass_move'\n");
    let second = call(
        &server,
        "observe",
        json!({"capture": false, "since": cursor}),
    )
    .await
    .unwrap();
    assert_eq!(
        second.json["console"]["lines"],
        json!(["TESTSCRIPT: CheckPoint -> 'gt_pass_move'"])
    );
    assert_eq!(second.json["console"]["newLines"], json!(1));
    assert_eq!(second.json["events"][0]["name"], json!("gt_pass_move"));
    assert_eq!(second.json["capture"], Value::Null);
    assert!(second.json["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n.as_str().unwrap().contains("capture: false")));
}

/// A wine-tools session whose /app holds the bridge's discovery file (the mock writes it there),
/// with /work for its captures.
struct WineWorld {
    w: World,
    app: PathBuf,
}

fn wine_world(tag: &str) -> WineWorld {
    let w = world(tag);
    let root = w.capture.parent().unwrap().parent().unwrap().to_path_buf();
    let (artifacts, _capture) = artifacts_with_capture(&root, WINE);
    let app = root.join("app");
    std::fs::create_dir_all(app.join("game/csgo")).unwrap();
    w.fake.add_running(WINE);
    w.fake
        .set_mounts(WINE, &bind_mounts(&[(&app, "/app"), (&artifacts, "/work")]));
    WineWorld { w, app }
}

#[tokio::test]
async fn observe_reports_the_bridge_tick_for_the_bridge_session_only() {
    let ww = wine_world("observe-tick");
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .responder("game.tick", |_| json!({"tick": 4321, "frames": 9}))
        .discovery_dir(ww.app.clone())
        .start()
        .await
        .unwrap();
    let server = make_server(&ww.w, &mb.discovery_path, &[]);
    let out = call(
        &server,
        "observe",
        json!({"capture": false, "session": WINE}),
    )
    .await
    .unwrap();
    assert_eq!(out.json["tick"]["tick"], json!(4321));
    assert_eq!(out.json["tick"]["source"], json!("bridge"));
    assert_eq!(out.json["tick"]["session"], json!(WINE));
    assert_eq!(mb.method_calls("game.tick").len(), 1);
    assert!(
        out.json["errors"].as_array().unwrap().is_empty(),
        "{}",
        out.json
    );
}

#[tokio::test]
async fn observe_never_ticks_a_session_the_bridge_does_not_belong_to() {
    // the bridge discovery lives elsewhere (no app mount holds it): a linux session and a wine
    // session with a foreign bridge are both skipped, with a note, and game.tick is never sent
    let ww = wine_world("observe-foreign");
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS)
        .start()
        .await
        .unwrap();
    let server = make_server(&ww.w, &mb.discovery_path, &[]);
    for target in [SESSION, WINE] {
        let out = call(
            &server,
            "observe",
            json!({"capture": false, "session": target}),
        )
        .await
        .unwrap();
        assert_eq!(out.json["tick"], Value::Null, "{target}");
        assert!(
            out.json["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap().contains("not the bridge's session")),
            "{target}"
        );
    }
    assert!(mb.method_calls("game.tick").is_empty());
}

#[tokio::test]
async fn observe_skips_the_tick_when_the_bridge_does_not_advertise_it() {
    let ww = wine_world("observe-no-tick");
    let mb = MockBridge::builder()
        .methods(CLIENT_METHODS[..8].iter().copied())
        .discovery_dir(ww.app.clone())
        .start()
        .await
        .unwrap();
    let server = make_server(&ww.w, &mb.discovery_path, &[]);
    let out = call(
        &server,
        "observe",
        json!({"capture": false, "session": WINE}),
    )
    .await
    .unwrap();
    assert_eq!(out.json["tick"], Value::Null);
    assert!(out.json["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n.as_str().unwrap().contains("does not advertise game.tick")));
    assert!(out.json["errors"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn bad_session_names_are_rejected_before_any_part_runs() {
    let w = world("observe-args");
    let server = make_server(&w, &w.no_bridge, &[]);
    for args in [
        json!({"session": "../etc"}),
        json!({"session": "cs2mcp-Bad_Name"}),
    ] {
        let err = call(&server, "observe", args).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments");
        // nothing was captured or requested
        assert!(!w.capture.join("request.json").exists());
    }
    let err = call(&server, "observe", json!({"consoleLines": 100000}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    let err = call(&server, "observe", json!({"since": {"inode": "x"}}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
}
