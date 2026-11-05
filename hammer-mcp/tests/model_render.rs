//! Mock-backed tests for `model_render` (ModelDoc vision pass, WP-T) against the mock bridge's
//! `modeldoc.render` arm and the shared render pipeline (`particle_render::run_render`, the same
//! one `particle_render` uses).
//!
//! The temp project tree mimics the real layout: `--project-root` is the temp root, so
//! `Z:\app\renders\<file>.png` maps to `<root>/workshop/run/app/renders/<file>.png` through
//! `PathMap`'s default `z:\app` entry. The test writes the PNG the bridge would have written (the
//! mock writes no file) and then asserts the tool's result, including the image block.
//!
//! The tool is registered through `modeldoc::register` (the real first-pass ModelDoc register,
//! with `model_render` appended last), so capability gating and the order pin are exercised
//! exactly as in production.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::capture::frame::{self, RgbImage};
use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mock::{make_model_doc, MockBridge, MockBuilder};
use cs2_mcp_common::tools::{Budget, CallFailure, Registry, ToolError, ToolOutput};
use hammer_mcp::state::HammerState;
use hammer_mcp::tools::modeldoc::{self, MODELDOC_TOOL_ORDER};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- fixtures

/// A scratch project tree under `target/tmp` (never `/tmp`), unique per test.
struct Host {
    dir: PathBuf,
}

impl Host {
    fn new(tag: &str) -> Host {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("model-render-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Host { dir }
    }

    /// `<app>/renders`, where `modeldoc.render` writes (`Z:\app\renders` on the wire).
    fn renders(&self) -> PathBuf {
        self.dir.join("workshop/run/app/renders")
    }

    /// Write the PNG the mock's `modeldoc.render` will report (`Z:\app\renders\<name>` -> this
    /// file), with a gradient so the decoded stats are non-trivial.
    fn write_png(&self, name: &str, width: u32, height: u32) -> PathBuf {
        let path = self.renders().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 3);
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&[
                    (x % 256) as u8,
                    (y % 256) as u8,
                    ((x + y) % 256) as u8,
                ]);
            }
        }
        let img = RgbImage::new(width, height, pixels).unwrap();
        std::fs::write(&path, img.to_png().unwrap()).unwrap();
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

/// A real registry (capability gating included) with the full ModelDoc set registered.
struct Server {
    state: HammerState,
    registry: Registry<HammerState>,
}

impl Server {
    fn new(discovery: &Path, host_dir: &Path) -> Server {
        let state = HammerState::from_config(&config(discovery, host_dir)).unwrap();
        let mut registry = Registry::new();
        modeldoc::register(&mut registry);
        Server { state, registry }
    }

    async fn call_output(&self, name: &str, args: Value) -> Result<ToolOutput, ToolError> {
        let budget = Budget::new(45_000, 30_000);
        match self.registry.call(&self.state, name, args, budget).await {
            Ok(out) => Ok(out),
            Err(CallFailure::Tool(e)) => Err(e),
            Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
        }
    }

    async fn call(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        self.call_output(name, args).await.map(|out| out.json)
    }
}

/// The bridge's own `modeldoc.render` result for a render at `<stem>.png` (the mock's shape).
fn expected_win_path(stem: &str) -> String {
    format!("Z:\\app\\renders\\{stem}.png")
}

// ---------------------------------------------------------------- registration sanity

#[test]
fn the_model_render_tool_is_registered() {
    let mut registry = Registry::<HammerState>::new();
    modeldoc::register(&mut registry);
    assert_eq!(registry.names(), MODELDOC_TOOL_ORDER);
    assert_eq!(MODELDOC_TOOL_ORDER.last(), Some(&"model_render"));
    let spec = registry.spec("model_render").unwrap();
    assert_eq!(spec.bridge_methods.to_vec(), vec!["modeldoc.render"]);
    for key in ["width", "height", "name", "inline"] {
        assert!(
            spec.input_schema["properties"][key].is_object(),
            "{key} is in the input schema"
        );
    }
    assert_eq!(spec.input_schema["additionalProperties"], json!(false));
}

/// A mock with an active ModelDoc session (the mock's session = an open `model_doc`): without one
/// `modeldoc.render` answers `NoViewport no_session`, like `modeldoc.active`'s reason.
fn session() -> MockBuilder {
    MockBridge::builder()
        .modeldoc()
        .model_doc(Some(make_model_doc(
            "Z:\\app\\content\\csgo_addons\\mcp_test\\models\\x.vmdl",
            false,
        )))
}

// ---------------------------------------------------------------- model_render

#[tokio::test]
async fn render_happy_returns_json_and_one_png_block() {
    let host = Host::new("happy");
    let mb = session().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let png = host.write_png("happy.png", 64, 48);

    let out = server
        .call_output(
            "model_render",
            json!({"name": "happy", "width": 64, "height": 48}),
        )
        .await
        .unwrap();

    // the bridge's object passed through, with the mapped host path
    assert_eq!(out.json["path"], json!(expected_win_path("happy")));
    assert_eq!(out.json["hostPath"], json!(png.display().to_string()));
    assert_eq!(out.json["file"], json!("happy.png"));
    assert_eq!(out.json["width"], json!(64));
    assert_eq!(out.json["height"], json!(48));
    assert_eq!(out.json["format"], json!("RGBA8888"));
    assert_eq!(out.json["hasDocument"], json!(true));
    assert_eq!(out.json["stats"]["untouchedPixels"], json!(0));
    assert_eq!(out.json["stats"]["maxLuma"], json!(255));
    // the explicit width/height reached the bridge (the mock reports width*height)
    assert_eq!(out.json["stats"]["nonBlackPixels"], json!(64 * 48));
    // the decoded pixels' own statistics
    assert_eq!(out.json["localStats"]["width"], json!(64));
    assert_eq!(out.json["localStats"]["height"], json!(48));

    // one image block, inlined at full size and carrying the source bytes
    assert_eq!(out.json["inline"]["mime"], json!("image/png"));
    assert_eq!(out.json["inline"]["width"], json!(64));
    assert_eq!(out.json["inline"]["downscaled"], json!(false));
    assert_eq!(out.images.len(), 1);
    assert_eq!(out.images[0].mime, "image/png");
    assert_eq!(out.images[0].bytes, std::fs::read(&png).unwrap());
    assert_eq!(frame::decode_png(&out.images[0].bytes).unwrap().width, 64);
}

#[tokio::test]
async fn render_without_a_model_doc_is_no_session_like_modeldoc_active() {
    // the mock models one state for both arms: no model_doc = no session, so modeldoc.active's
    // `reason:"no_session"` and modeldoc.render's NoViewport agree (no knob needed)
    let host = Host::new("no-doc");
    let mb = MockBridge::builder().modeldoc().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call("model_render", json!({"name": "x"}))
        .await
        .unwrap_err();
    assert_eq!(e.name, "NoViewport", "{e:?}");
    assert_eq!(e.data["reason"], json!("no_session"));
    assert_eq!(mb.method_calls("modeldoc.render").len(), 1);
}

#[tokio::test]
async fn render_large_png_is_downscaled_inline() {
    let host = Host::new("large");
    let mb = session().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    // 2200x1300 = 2.86 Mpx > 1920x1080 (2.07 Mpx): the inline copy must be downscaled
    host.write_png("big.png", 2200, 1300);

    let out = server
        .call_output(
            "model_render",
            json!({"name": "big.png", "width": 2200, "height": 1300}),
        )
        .await
        .unwrap();

    assert_eq!(
        out.json["file"],
        json!("big.png"),
        "a trailing .png is dropped"
    );
    assert_eq!(out.json["inline"]["downscaled"], json!(true));
    let w = out.json["inline"]["width"].as_u64().unwrap();
    let h = out.json["inline"]["height"].as_u64().unwrap();
    // the cap is the pixel count (1920x1080), not a per-side limit
    assert!(w * h <= 1920 * 1080, "{w}x{h} exceeds the inline pixel cap");
    assert!(w < 2200, "the inline copy was not downscaled");
    // the local stats still describe the full-size decoded frame
    assert_eq!(out.json["localStats"]["width"], json!(2200));
    let decoded = frame::decode_png(&out.images[0].bytes).unwrap();
    assert_eq!((decoded.width, decoded.height), (w as u32, h as u32));
}

#[tokio::test]
async fn render_inline_false_has_no_image_block() {
    let host = Host::new("no-inline");
    let mb = session().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let png = host.write_png("plain.png", 32, 16);

    let out = server
        .call_output("model_render", json!({"name": "plain", "inline": false}))
        .await
        .unwrap();

    assert_eq!(out.json["hostPath"], json!(png.display().to_string()));
    assert_eq!(out.json["inline"], Value::Null);
    assert!(out.images.is_empty(), "no image block was asked for");
    // the file is still checked and the pixels still measured
    assert_eq!(out.json["localStats"]["width"], json!(32));
}

#[tokio::test]
async fn render_missing_host_file_is_render_file_missing() {
    let host = Host::new("missing");
    let mb = session().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    // the mock reports Z:\app\renders\absent.png but no host file was written
    let e = server
        .call("model_render", json!({"name": "absent"}))
        .await
        .unwrap_err();
    assert_eq!(e.name, "RenderFileMissing", "{e:?}");
    assert!(e.retry_safe);
    assert_eq!(e.data["path"], json!(expected_win_path("absent")));
    let expected = host.renders().join("absent.png");
    assert_eq!(e.data["hostPath"], json!(expected.display().to_string()));
    // the render itself ran (the file-missing check is after the bridge replied)
    assert_eq!(mb.method_calls("modeldoc.render").len(), 1);
}

#[tokio::test]
async fn render_bad_size_is_invalid_arguments_and_sends_nothing() {
    let host = Host::new("bad-size");
    let mb = session().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    for bad in [
        json!({"width": 8}),
        json!({"width": 4097}),
        json!({"height": 0}),
        json!({"width": "640"}),
        // 4096*4096 = 16777216 > 8388608
        json!({"width": 4096, "height": 4096}),
        json!({"width": 3000, "height": 3000}),
        json!({"width": 640, "height": 480, "bogus": true}),
        json!({"width": 640, "height": 480, "inline": "yes"}),
    ] {
        let e = server.call("model_render", bad.clone()).await.unwrap_err();
        assert_eq!(e.name, "InvalidParams", "{bad}");
        assert_eq!(e.code, Some(-32602));
        assert!(e.retry_safe, "{bad}");
    }
    assert!(
        mb.method_calls("modeldoc.render").is_empty(),
        "nothing was sent"
    );
}

#[tokio::test]
async fn render_bad_name_is_invalid_arguments_and_sends_nothing() {
    let host = Host::new("bad-name");
    let mb = session().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let long = "a".repeat(65);
    for bad in [
        json!({"name": ".hidden"}),
        json!({"name": "a/b"}),
        json!({"name": "a b"}),
        json!({"name": ""}),
        json!({"name": long}),
        json!({"name": 12}),
        json!({"name": "naïve"}),
    ] {
        let e = server.call("model_render", bad.clone()).await.unwrap_err();
        assert_eq!(e.name, "InvalidParams", "{bad}");
        assert_eq!(e.code, Some(-32602));
    }
    assert!(
        mb.method_calls("modeldoc.render").is_empty(),
        "nothing was sent"
    );
}

#[tokio::test]
async fn render_bridge_failure_is_render_failed() {
    let host = Host::new("render-failed");
    let mb = session()
        .modeldoc_render_fail(Some("bad_camera"))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call("model_render", json!({"name": "x"}))
        .await
        .unwrap_err();
    assert_eq!(e.name, "RenderFailed", "{e:?}");
    assert_eq!(e.code, Some(-32020));
    assert_eq!(e.data["reason"], json!("bad_camera"));
    assert_eq!(mb.method_calls("modeldoc.render").len(), 1);
}

#[tokio::test]
async fn render_without_a_session_is_no_viewport_no_session() {
    let host = Host::new("no-session");
    let mb = MockBridge::builder()
        .modeldoc()
        .modeldoc_no_session(true)
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call("model_render", json!({"name": "x"}))
        .await
        .unwrap_err();
    assert_eq!(e.name, "NoViewport", "{e:?}");
    assert_eq!(e.code, Some(-32019));
    assert_eq!(e.data["reason"], json!("no_session"));
    assert_eq!(mb.method_calls("modeldoc.render").len(), 1);
}

#[tokio::test]
async fn render_without_capability_is_not_implemented() {
    let host = Host::new("no-cap");
    // without `.modeldoc()` no modeldoc.* method is advertised
    let mb = MockBridge::start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call("model_render", json!({"name": "x"}))
        .await
        .unwrap_err();
    assert_eq!(e.name, "NotImplemented", "{e:?}");
    assert_eq!(e.code, Some(-32011));
    assert_eq!(e.data["missing"], json!(["modeldoc.render"]));
    assert!(
        mb.method_calls("modeldoc.render").is_empty(),
        "nothing was sent"
    );
}

#[tokio::test]
async fn render_unmapped_path_is_path_not_mapped() {
    let host = Host::new("unmapped");
    // a responder stands in for a bridge whose renders directory is not under Z:\app
    let mb = MockBridge::builder()
        .modeldoc()
        .responder("modeldoc.render", |_| {
            json!({
                "path": "Q:\\x\\y.png", "file": "y.png", "width": 64, "height": 48,
                "bytes": 1, "format": "RGBA8888", "hasDocument": true,
                "stats": {"untouchedPixels": 0}, "durationMs": 1,
            })
        })
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call("model_render", json!({"width": 64, "height": 48}))
        .await
        .unwrap_err();
    assert_eq!(e.name, "PathNotMapped", "{e:?}");
    assert_eq!(e.code, Some(-32602));
    assert!(e.retry_safe);
    assert_eq!(e.data["path"], json!("Q:\\x\\y.png"));
    assert!(e.data.contains_key("hint"), "the hint names --path-map");
}

#[tokio::test]
async fn request_carries_a_deadline() {
    let host = Host::new("deadline");
    let mb = session().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    host.write_png("sized.png", 64, 48);

    server
        .call(
            "model_render",
            json!({"name": "sized", "width": 64, "height": 48}),
        )
        .await
        .unwrap();

    let calls = mb.method_calls("modeldoc.render");
    assert_eq!(calls.len(), 1);
    let deadline = calls[0].params["deadlineMs"].as_u64().unwrap_or(0);
    assert!(deadline > 0 && deadline <= 30_000, "deadlineMs={deadline}");
}
