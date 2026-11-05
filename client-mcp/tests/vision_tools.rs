//! Vision tools against the mock bridge: `render_world` (params forwarded, Windows path mapped to
//! the host, stats + inline image block, capability gating) and `ocr` (argument validation, last
//! capture, tesseract when installed), plus the reusable `capture_output` (R/B swap, downscale).
#![allow(clippy::result_large_err)]
use client_mcp::state::{ClientConfig, ClientState};
use client_mcp::tools::vision;
use cs2_mcp_common::capture::frame::{load_png_rgb, RgbImage};
use cs2_mcp_common::capture::ocr::tesseract_available;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{CallFailure, ToolError, ToolOutput};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

const M28_CLIENT_METHODS: [&str; 13] = [
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
    server: &cs2_mcp_common::mcp::McpServer<ClientState>,
    name: &str,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cs2mcp-vision-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("renders")).unwrap();
    d
}

fn gradient(w: u32, h: u32) -> RgbImage {
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&[(x * 255 / w.max(1)) as u8, (y * 255 / h.max(1)) as u8, 40]);
        }
    }
    RgbImage::new(w, h, px).unwrap()
}

fn write_png(path: &Path, img: &RgbImage) {
    std::fs::write(path, img.to_png().unwrap()).unwrap();
}

/// Bridge-shaped `world.render` result for a file the bridge wrote at `win_path`.
fn render_result(win_path: &str, w: u32, h: u32) -> Value {
    json!({
        "path": win_path,
        "file": "t.png",
        "width": w,
        "height": h,
        "bytes": 1234,
        "camera": {"origin": [0, 0, 0], "angles": [0, 90, 0], "fov": 90.0, "znear": 4.0, "zfar": 30000.0},
        "pipeline": "Tools->CSGO",
        "srgb": false,
        "stats": {"untouchedPixels": 0, "nonBlackPixels": 10, "meanLuma": 12.5, "minLuma": 0, "maxLuma": 200, "crc32": "00000000"},
        "ms": {"render": 1, "readback": 1, "encode": 1}
    })
}

#[tokio::test]
async fn render_world_maps_path_attaches_image_and_forwards_params() {
    let dir = scratch("render");
    let host_png = dir.join("renders").join("t.png");
    write_png(&host_png, &gradient(64, 32));
    let mb = MockBridge::builder()
        .methods(M28_CLIENT_METHODS)
        .responder("world.render", |_| {
            render_result("Z:\\app\\renders\\t.png", 64, 32)
        })
        .start()
        .await
        .unwrap();
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--deadline-ms",
        "7000",
        "--path-map",
        &format!("z:\\app={}", dir.display()),
    ]))
    .unwrap();

    let out = call(
        &server,
        "render_world",
        json!({"origin": [-120.5, 300, 64], "angles": [-10, 90, 0], "width": 64, "height": 32, "name": "t", "fov": 75}),
    )
    .await
    .unwrap();

    // params as sent: camera + size + name forwarded, deadline from the budget
    let calls = mb.method_calls("world.render");
    assert_eq!(calls.len(), 1);
    let p = &calls[0].params;
    assert_eq!(p["origin"], json!([-120.5, 300, 64]));
    assert_eq!(p["angles"], json!([-10, 90, 0]));
    assert_eq!(p["width"], json!(64));
    assert_eq!(p["name"], json!("t"));
    assert_eq!(p["fov"], json!(75));
    assert_eq!(p["deadlineMs"], json!(7000));
    assert!(p.get("srgb").is_none());

    // result: host path, windows path kept, our stats, bridge stats under their own key
    assert_eq!(out.json["path"], json!(host_png.display().to_string()));
    assert_eq!(out.json["windowsPath"], json!("Z:\\app\\renders\\t.png"));
    assert_eq!(out.json["stats"]["width"], json!(64));
    assert_eq!(out.json["stats"]["height"], json!(32));
    assert!(out.json["stats"]["nonblackFrac"].as_f64().unwrap() > 0.9);
    assert_eq!(out.json["bridgeStats"]["nonBlackPixels"], json!(10));
    assert!(out
        .json
        .get("stats")
        .is_some_and(|s| s.get("nonBlackPixels").is_none()));
    assert_eq!(out.json["inline"]["downscaled"], json!(false));
    assert!(out.json.get("handle").is_none());

    // one inline PNG block that decodes to the rendered frame
    assert_eq!(out.images.len(), 1);
    assert_eq!(out.images[0].mime, "image/png");
    let inline_path = dir.join("inline.png");
    std::fs::write(&inline_path, &out.images[0].bytes).unwrap();
    assert_eq!(load_png_rgb(&inline_path).unwrap(), gradient(64, 32));

    // the capture is remembered for ocr {last: true}
    if tesseract_available().await {
        let o = call(&server, "ocr", json!({"last": true})).await.unwrap();
        assert_eq!(o.json["png"], json!(host_png.display().to_string()));
        assert_eq!(o.json["psm"], json!(6));
        assert_eq!(o.json["engine"], json!("tesseract-cli"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn render_world_unmapped_path_is_reported_not_guessed() {
    let mb = MockBridge::builder()
        .methods(M28_CLIENT_METHODS)
        .responder("world.render", |_| {
            render_result("D:\\elsewhere\\t.png", 64, 32)
        })
        .start()
        .await
        .unwrap();
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--path-map",
        "z:\\app=/nonexistent-cs2mcp-tree",
    ]))
    .unwrap();
    let err = call(
        &server,
        "render_world",
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PathNotMapped");
    assert_eq!(err.data["windowsPath"], json!("D:\\elsewhere\\t.png"));
}

#[tokio::test]
async fn render_world_missing_file_is_reported() {
    let dir = scratch("missing");
    let mb = MockBridge::builder()
        .methods(M28_CLIENT_METHODS)
        .responder("world.render", |_| {
            render_result("Z:\\app\\renders\\gone.png", 64, 32)
        })
        .start()
        .await
        .unwrap();
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--path-map",
        &format!("z:\\app={}", dir.display()),
    ]))
    .unwrap();
    let err = call(
        &server,
        "render_world",
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "ArtifactMissing");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn render_world_validates_before_dispatch() {
    let mb = MockBridge::builder()
        .methods(M28_CLIENT_METHODS)
        .start()
        .await
        .unwrap();
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
    ]))
    .unwrap();
    for bad in [
        json!({}),
        json!({"origin": [0, 0, 0]}),
        json!({"origin": [0, 0], "angles": [0, 0, 0]}),
        json!({"origin": [0, 0, "x"], "angles": [0, 0, 0]}),
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "width": 4}),
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "width": 1.5}),
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "srgb": 1}),
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "deadlineMs": 5}),
    ] {
        let err = call(&server, "render_world", bad.clone())
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    assert!(mb.method_calls("world.render").is_empty());
}

#[tokio::test]
async fn render_world_is_gated_when_bridge_lacks_it() {
    let mb = MockBridge::builder()
        .methods(M28_CLIENT_METHODS[..12].to_vec())
        .start()
        .await
        .unwrap();
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
    ]))
    .unwrap();
    let err = call(
        &server,
        "render_world",
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "NotImplemented");
    assert_eq!(err.data["missing"], json!(["world.render"]));
    assert!(mb.method_calls("world.render").is_empty());
}

#[tokio::test]
async fn ocr_argument_validation_needs_no_bridge() {
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        "/tmp/cs2mcp-no-bridge/session.json",
    ]))
    .unwrap();
    // no capture yet
    let err = call(&server, "ocr", json!({"last": true}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    assert!(err.message.contains("no capture"));
    for bad in [
        json!({}),
        json!({"png": "/nonexistent-cs2mcp.png", "last": true}),
        json!({"png": "/nonexistent-cs2mcp.png", "psm": 3}),
        json!({"png": "/nonexistent-cs2mcp.png", "crop": [0, 0, 10]}),
        json!({"png": "/nonexistent-cs2mcp.png", "crop": [0, 0, 0, 10]}),
        json!({"png": "/nonexistent-cs2mcp.png", "unknown": 1}),
    ] {
        let err = call(&server, "ocr", bad.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    // a missing file is a capture problem (CaptureFailed), not an OCR engine run
    let err = call(&server, "ocr", json!({"png": "/nonexistent-cs2mcp.png"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "CaptureFailed");
}

#[tokio::test]
async fn ocr_white_png_has_no_text_and_crop_is_reported() {
    if !tesseract_available().await {
        eprintln!("skipping: tesseract not installed");
        return;
    }
    let dir = scratch("ocr");
    let png = dir.join("white.png");
    write_png(
        &png,
        &RgbImage::new(160, 60, vec![255; 160 * 60 * 3]).unwrap(),
    );
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        "/tmp/cs2mcp-no-bridge/session.json",
    ]))
    .unwrap();
    let out = call(&server, "ocr", json!({"png": png.display().to_string()}))
        .await
        .unwrap();
    assert_eq!(out.json["psm"], json!(6));
    assert_eq!(out.json["engine"], json!("tesseract-cli"));
    assert!(out.json["text"].as_str().unwrap().trim().is_empty());
    assert!(out.json.get("crop").is_none());

    let out = call(
        &server,
        "ocr",
        json!({"png": png.display().to_string(), "crop": [10, 10, 100, 30], "psm": 11}),
    )
    .await
    .unwrap();
    assert_eq!(out.json["psm"], json!(11));
    assert_eq!(out.json["crop"], json!([10, 10, 100, 30]));
    assert_eq!(out.json["upscale"], json!(2));

    // crop outside the image: an argument error, nothing is run on a bogus region
    let err = call(
        &server,
        "ocr",
        json!({"png": png.display().to_string(), "crop": [150, 50, 100, 100]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn capture_output_swaps_rb_and_downscales_large_frames() {
    let dir = scratch("capture");
    // game png_screenshot order: pure red in the file is blue in the picture after the swap
    let png = dir.join("snap.png");
    write_png(
        &png,
        &RgbImage::new(2, 1, vec![255, 0, 0, 0, 255, 0]).unwrap(),
    );
    let state = ClientState::from_config(&config(&["--discovery", "/tmp/none"])).unwrap();
    let out = vision::capture_output(&state, &png, true, Map::new())
        .await
        .unwrap();
    assert_eq!(out.json["stats"]["uniqueColors"], json!(2));
    let inline = dir.join("swapped.png");
    std::fs::write(&inline, &out.images[0].bytes).unwrap();
    assert_eq!(
        load_png_rgb(&inline).unwrap().pixels,
        vec![0, 0, 255, 0, 255, 0]
    );

    // without the swap the colours are untouched
    let out = vision::capture_output(&state, &png, false, Map::new())
        .await
        .unwrap();
    // no swap and no downscale: the file's own bytes are inlined, not a re-encode
    assert_eq!(out.images[0].bytes, std::fs::read(&png).unwrap());
    std::fs::write(&inline, &out.images[0].bytes).unwrap();
    assert_eq!(
        load_png_rgb(&inline).unwrap().pixels,
        vec![255, 0, 0, 0, 255, 0]
    );

    // a 2400x1000 frame is inlined downscaled, the path is still the full file
    let big = dir.join("big.png");
    write_png(
        &big,
        &RgbImage::new(2400, 1000, vec![90; 2400 * 1000 * 3]).unwrap(),
    );
    let out = vision::capture_output(&state, &big, false, Map::new())
        .await
        .unwrap();
    assert_eq!(out.json["inline"]["downscaled"], json!(true));
    assert_eq!(out.json["path"], json!(big.display().to_string()));
    assert_eq!(out.json["stats"]["width"], json!(2400));
    let w = out.json["inline"]["width"].as_u64().unwrap();
    let h = out.json["inline"]["height"].as_u64().unwrap();
    assert!(w * h <= 1920 * 1080);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn ocr_golden_fixture_reads_the_number() {
    if !tesseract_available().await {
        eprintln!("skipping: tesseract not installed");
        return;
    }
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/cs2-mcp-common/fixtures/ocr/health_100.png"
    );
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        "/tmp/cs2mcp-no-bridge/session.json",
    ]))
    .unwrap();
    let out = call(&server, "ocr", json!({"png": fixture})).await.unwrap();
    let text = out.json["text"].as_str().unwrap();
    assert!(text.contains("100"), "OCR read {text:?}");
}

#[tokio::test]
async fn path_not_mapped_is_not_retryable_and_hints_at_path_map() {
    let mb = MockBridge::builder()
        .methods(M28_CLIENT_METHODS)
        .responder("world.render", |_| {
            render_result("Q:\\nowhere\\t.png", 64, 32)
        })
        .start()
        .await
        .unwrap();
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
    ]))
    .unwrap();
    let err = call(
        &server,
        "render_world",
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PathNotMapped");
    assert!(!err.retry_safe);
    assert!(err.data["hint"].as_str().unwrap().contains("--path-map"));
}

#[tokio::test]
async fn render_world_refuses_oversized_frames_before_dispatch() {
    let mb = MockBridge::builder()
        .methods(M28_CLIENT_METHODS)
        .start()
        .await
        .unwrap();
    let server = client_mcp::build_server(&config(&[
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
    ]))
    .unwrap();
    let err = call(
        &server,
        "render_world",
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "width": 8192, "height": 8192}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    assert!(mb.method_calls("world.render").is_empty());
}
