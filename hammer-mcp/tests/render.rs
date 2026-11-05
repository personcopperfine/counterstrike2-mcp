//! Mock-backed tests for `render_view` / `render_world` (WP-D) against the mock bridge's
//! `view.render`/`world.render` arms (mock.rs `hammer_render_reply`) and the shared render
//! pipeline (`particle_render::render_pixels`, the one `model_render`/`particle_render` use).
//!
//! The temp project tree mimics the real layout: `--path-map z:\app=<host>` (the pattern of
//! tests/server.rs), so `Z:\app\renders\<file>.png` reaches `<host>\<file>.png`. With
//! `.render_dir(..)` the mock writes the tiny valid PNG the bridge would have written (8x4
//! solid colour, the POC `tests/support.py::make_png` equivalent) — `bytes` is then its real
//! length; tests that want a "the file does not exist" run leave the knob unset. Camera semantics
//! are ports of the POC `test_contract.py` render section (`support.py::render_responder` /
//! `world_render_responder` shape), with the C bridge's param bounds mirrored in the mock.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{Budget, CallFailure, Registry, ToolError, ToolOutput};
use hammer_mcp::state::HammerState;
use hammer_mcp::tools::hammer;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- fixtures

/// A scratch directory under `target/tmp` (never `/tmp`), unique per test.
struct Host {
    dir: PathBuf,
}

impl Host {
    fn new(tag: &str) -> Host {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("render-wpd-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Host { dir }
    }

    /// `Z:\app\renders\<file>` maps here through the `z:\app` path-map entry that `config` sets.
    fn renders(&self) -> PathBuf {
        self.dir.join("renders")
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
        "--path-map".to_string(),
        format!("z:\\app={}", host_dir.display()),
    ];
    // hermetic podman/vk-layer (the pattern of tests/common/mod.rs, copied, not imported):
    // nothing here may touch the host's real podman or project tree.
    argv.extend([
        "--podman".into(),
        "/nonexistent/cs2mcp-test-podman".into(),
        "--vk-layer-dir".into(),
        "/nonexistent/cs2mcp-test-vklayer".into(),
        "--project-root".into(),
        "/nonexistent/cs2mcp-test-project".into(),
    ]);
    W::parse_from(argv).c
}

/// The production registration path (the 24 POC tools through `tools/hammer::register`), so
/// capability gating and the order are exercised exactly as in production.
struct Server {
    state: HammerState,
    registry: Registry<HammerState>,
}

impl Server {
    fn new(discovery: &Path, host_dir: &Path) -> Server {
        let state = HammerState::from_config(&config(discovery, host_dir)).unwrap();
        let mut registry = Registry::new();
        hammer::register(&mut registry);
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

/// The mock with both render methods advertised and the tiny-PNG directory wired to the host's
/// `renders` dir.
fn render_mock(host: &Host) -> MockBuilder {
    MockBridge::builder().renders().render_dir(host.renders())
}

use cs2_mcp_common::mock::MockBuilder;

// ---------------------------------------------------------------- registration sanity

#[test]
fn the_render_tools_are_registered_in_the_poc_order() {
    let mut registry = Registry::<HammerState>::new();
    hammer::register(&mut registry);
    let order: Vec<&str> = registry.names()[..24].to_vec();
    assert_eq!(
        order.iter().position(|n| *n == "render_view"),
        Some(10),
        "render_view stays between get_entity and the mesh reads"
    );
    assert_eq!(
        order.iter().position(|n| *n == "render_world"),
        Some(11),
        "render_world right after render_view"
    );
    let spec = registry.spec("render_view").unwrap();
    assert_eq!(spec.bridge_methods.to_vec(), vec!["view.render"]);
    let spec = registry.spec("render_world").unwrap();
    assert_eq!(spec.bridge_methods.to_vec(), vec!["world.render"]);
    for key in [
        "origin", "angles", "lookAt", "distance", "pitch", "yaw", "fov", "width", "height",
        "name", "inline",
    ] {
        assert!(
            spec.input_schema["properties"][key].is_object(),
            "{key} is in the input schema"
        );
    }
    assert_eq!(spec.input_schema["additionalProperties"], json!(false));
    // the file reads say SAVED .vmap; the renders say live/engine (workflows/03 definition of done)
    let names: Vec<&str> = registry.names();
    assert!(names.contains(&"list_meshes") && names.contains(&"get_mesh"));
}

// ---------------------------------------------------------------- render_view

#[tokio::test]
async fn render_view_explicit_camera_inlines_the_png() {
    let host = Host::new("view-explicit");
    let mb = render_mock(&host).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);

    let out = server
        .call_output(
            "render_view",
            json!({
                "origin": [300, 300, 150], "angles": [32.9, 45, 0], "fov": 70,
                "width": 320, "height": 180, "name": "unit_a",
            }),
        )
        .await
        .unwrap();

    // the request reached the bridge with the POC's exact param shape and a deadline
    let calls = mb.method_calls("view.render");
    assert_eq!(calls.len(), 1);
    let sent = &calls[0].params;
    assert_eq!(sent["origin"], json!([300.0, 300.0, 150.0]));
    assert_eq!(sent["angles"], json!([32.9, 45.0, 0.0]));
    assert_eq!(sent["fov"], json!(70.0));
    assert_eq!((sent["width"].clone(), sent["height"].clone(), sent["name"].clone()),
               (json!(320), json!(180), json!("unit_a")));
    assert!(sent["deadlineMs"].as_u64().unwrap_or(0) > 0);

    // the bridge's object passes through and the mock's tiny PNG was written and attached
    assert_eq!(out.json["path"], json!("Z:\\app\\renders\\unit_a.png"));
    assert_eq!(out.json["file"], json!("unit_a.png"));
    assert_eq!(out.json["width"], json!(320));
    let png_path = host.renders().join("unit_a.png");
    assert_eq!(out.json["hostPath"], json!(png_path.display().to_string()));
    assert!(png_path.is_file(), "the mock's tiny PNG landed in renders/");
    assert_eq!(out.json["camera"]["mode"], json!("explicit"));
    assert_eq!(out.json["camera"]["source"], json!("explicit"));
    assert_eq!(out.json["view"], json!({"type": 3, "renderMode": 1, "views": 2}));
    assert_eq!(out.json["toolMaterials"], json!("shown"));
    assert_eq!(out.json["renderMs"], json!(12));
    assert_eq!(out.json["notes"], Value::Null, "nothing to say on a clean render");
    assert_eq!(out.json["localStats"]["width"], json!(8), "the tiny PNG is 8x4");
    assert_eq!(out.json["inline"]["downscaled"], json!(false));
    assert_eq!(out.images.len(), 1);
    assert_eq!(out.images[0].mime, "image/png");
}

#[tokio::test]
async fn render_view_look_at_and_orbit() {
    let host = Host::new("view-orbit");
    let mb = render_mock(&host).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);

    let out = server
        .call("render_view", json!({"origin": [0, 0, 0], "lookAt": [100, 0, -100], "inline": false}))
        .await
        .unwrap();
    let calls = mb.method_calls("view.render");
    let sent = &calls.last().unwrap().params;
    assert_eq!(sent["angles"][0], json!(45.0), "pitch: looking down");
    assert_eq!(sent["angles"][1], json!(0.0), "yaw: +X");
    assert_eq!(out["camera"]["mode"], json!("lookAt"));
    assert_eq!(out["camera"]["lookAt"], json!([100.0, 0.0, -100.0]));

    let out = server
        .call(
            "render_view",
            json!({"lookAt": [464, 464, 0], "distance": 200, "pitch": 30, "yaw": 90, "inline": false}),
        )
        .await
        .unwrap();
    let calls = mb.method_calls("view.render");
    let sent = &calls.last().unwrap().params;
    let (ox, oy, oz) = (
        sent["origin"][0].as_f64().unwrap(),
        sent["origin"][1].as_f64().unwrap(),
        sent["origin"][2].as_f64().unwrap(),
    );
    // 200 units back along yaw 90 (looking +Y), 30 degrees above the target
    assert!((ox - 464.0).abs() < 0.01, "ox={ox}");
    assert!(
        (oy - (464.0 - 200.0 * f64::cos(30f64.to_radians()))).abs() < 0.01,
        "oy={oy}"
    );
    assert!((oz - 200.0 * f64::sin(30f64.to_radians())).abs() < 0.01, "oz={oz}");
    assert_eq!(sent["angles"][0], json!(30.0));
    assert_eq!(sent["angles"][1], json!(90.0));
    assert_eq!(out["camera"]["mode"], json!("orbit"));
    assert_eq!(out["inline"], Value::Null, "inline:false inlines nothing");
}

#[tokio::test]
async fn render_view_defaults_to_the_viewport_camera_and_honours_hide_tool_materials() {
    let host = Host::new("view-viewport");
    let mb = render_mock(&host).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);

    let out = server
        .call("render_view", json!({"hideToolMaterials": true, "inline": false}))
        .await
        .unwrap();
    let calls = mb.method_calls("view.render");
    let sent = &calls.last().unwrap().params;
    assert!(sent.get("origin").is_none(), "the viewport camera is the default");
    assert!(sent.get("angles").is_none());
    assert_eq!(sent["hideToolMaterials"], json!(true));
    // review-2 L3: no client-side default stem — the caller gave no `name`, so the request does
    // not carry one and the *bridge* names the file (`render_<tick>_<seq>`, its global sequence)
    assert!(sent.get("name").is_none(), "the bridge names the file: {sent}");
    assert_eq!(out["camera"]["mode"], json!("viewport"));
    assert_eq!(out["toolMaterials"], json!("hidden"));
    assert_eq!(out["width"], json!(640), "the POC's default size");
    assert_eq!(out["height"], json!(360));
    assert!(out["file"].as_str().unwrap_or_default().starts_with("render_"), "{out}");
}

#[tokio::test]
async fn render_view_argument_validation_never_dispatches() {
    let host = Host::new("view-bad");
    let mb = render_mock(&host).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let long = "a".repeat(65);
    for args in [
        json!({"angles": [0, 0, 0], "lookAt": [1, 1, 1]}),
        json!({"width": 8}),
        json!({"width": 4096, "height": 4096}),
        json!({"fov": 180}),
        json!({"fov": 0}),
        json!({"znear": 0}),
        json!({"znear": 10, "zfar": 5}),
        json!({"origin": [1, 2]}),
        json!({"distance": 50}),
        json!({"origin": [5, 5, 5], "lookAt": [5, 5, 5]}),
        json!({"name": ".hidden"}),
        json!({"name": "a/b"}),
        json!({"name": long}),
        json!({"name": 12}),
        json!({"width": "640", "inline": false}),
        json!({"hideToolMaterials": 1, "inline": false}),
        json!({"bogus": true}),
    ] {
        let e = server.call("render_view", args.clone()).await.unwrap_err();
        assert_eq!(e.name, "InvalidParams", "{args}");
        assert_eq!(e.code, Some(-32602), "{args}");
        assert!(e.retry_safe, "{args}");
    }
    assert!(
        mb.method_calls("view.render").is_empty(),
        "validation happens before anything is sent"
    );
}

#[tokio::test]
async fn render_view_reports_a_blank_image_and_unsaved_edits_when_there_are_any() {
    let host = Host::new("view-blank");
    // a responder stands in for a bridge whose render came out uniform black
    let mb = MockBridge::builder()
        .renders()
        .render_dir(host.renders())
        .responder("view.render", |params| {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("x");
            json!({
                "path": format!("Z:\\app\\renders\\{name}.png"), "file": format!("{name}.png"),
                "width": 640, "height": 360, "bytes": 1,
                "camera": {"origin": [0, 0, 512], "angles": [30, 0, 0], "fov": 90, "source": "viewport"},
                "view": {"type": 3, "renderMode": 1, "views": 1},
                "toolMaterials": "shown", "format": 0, "srgb": true,
                "stats": {"untouchedPixels": 0, "nonBlackPixels": 0, "meanLuma": 0, "minLuma": 0,
                          "maxLuma": 0, "crc32": "00000000"},
                "ms": {"render": 5, "encode": 1},
            })
        })
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    server.state.edits.lock().unwrap().since_save = 2;

    let out = server
        .call("render_view", json!({"name": "blank", "inline": false}))
        .await
        .unwrap();
    let mut found_blank = false;
    let mut found_live = false;
    for note in out["notes"].as_array().unwrap() {
        found_blank |= note.as_str().unwrap_or("").contains("blank");
        found_live |= note.as_str().unwrap_or("").contains("LIVE document");
    }
    assert!(found_blank, "{:?}", out["notes"]);
    assert!(found_live, "{:?}", out["notes"]);
    // the POC's low-contrast rule fires even when some pixels are lit
    let mb2 = MockBridge::builder()
        .renders()
        .render_dir(host.renders())
        .responder("view.render", |params| {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("x");
            json!({
                "path": format!("Z:\\app\\renders\\{name}.png"), "file": format!("{name}.png"),
                "width": 640, "height": 360, "bytes": 1,
                "camera": {"origin": [0, 0, 512], "angles": [30, 0, 0], "fov": 90, "source": "viewport"},
                "toolMaterials": "shown", "format": 0, "srgb": true,
                "stats": {"nonBlackPixels": 100, "maxLuma": 2, "minLuma": 0},
                "ms": {"render": 1},
            })
        })
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb2.discovery_path, &host.dir);
    let out = server
        .call("render_view", json!({"name": "flat", "inline": false}))
        .await
        .unwrap();
    assert!(
        out["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap_or("").contains("blank")),
        "maxLuma - minLuma < 4 reads as blank: {:?}",
        out["notes"]
    );
}

#[tokio::test]
async fn render_view_bridge_failures_surface() {
    let host = Host::new("view-chainfail");
    // the per-hop chain gate (the bridge's NoViewport reasons)...
    let mb = render_mock(&host)
        .view_render_fail(Some("no_3d_view"))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("render_view", json!({})).await.unwrap_err();
    assert_eq!(e.name, "NoViewport", "{e:?}");
    assert_eq!(e.code, Some(-32019));
    assert_eq!(e.data["reason"], json!("no_3d_view"));
    assert!(e.retry_safe);
    assert_eq!(mb.method_calls("view.render").len(), 1, "the refusal was sent");
    // ...and the dispatch's own gate: no active Hammer document at all
    let mb = MockBridge::builder()
        .renders()
        .render_dir(host.renders())
        .doc(None)
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("render_view", json!({})).await.unwrap_err();
    // the error taxonomy maps wire -32003 NoActiveDocument to ToolError name "NoDocument"
    assert_eq!(e.name, "NoDocument", "{e:?}");
    assert_eq!(e.wire_name.as_deref(), Some("NoActiveDocument"));
    assert_eq!(e.code, Some(-32003));
    assert_eq!(
        mb.method_calls("view.render").len(),
        1,
        "the dispatch gate refusal was sent"
    );
}

#[tokio::test]
async fn render_view_missing_host_file_is_render_file_missing() {
    let host = Host::new("view-missing");
    // no render_dir: the mock reports the path but writes nothing
    let mb = MockBridge::builder().renders().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("render_view", json!({"name": "absent"})).await.unwrap_err();
    assert_eq!(e.name, "RenderFileMissing", "{e:?}");
    // review-2 L4: the render itself succeeded - the missing host file is a path-map problem,
    // and a retry would render the same image again for the same error
    assert!(!e.retry_safe);
    assert!(
        e.message.contains("the render itself succeeded") && e.message.contains("path map"),
        "the wording says where the problem is, not 'a retry may succeed': {}",
        e.message
    );
    assert_eq!(e.data["path"], json!("Z:\\app\\renders\\absent.png"));
    assert_eq!(
        e.data["hostPath"],
        json!(host.renders().join("absent.png").display().to_string())
    );
    assert_eq!(mb.method_calls("view.render").len(), 1, "the render itself ran");
}

#[tokio::test]
async fn render_view_unmapped_path_is_path_not_mapped() {
    let host = Host::new("view-unmapped");
    let mb = MockBridge::builder()
        .renders()
        .responder("view.render", |_| {
            json!({
                "path": "Q:\\x\\y.png", "file": "y.png", "width": 64, "height": 48, "bytes": 1,
                "stats": {"nonBlackPixels": 1}, "ms": {"render": 1},
            })
        })
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("render_view", json!({})).await.unwrap_err();
    assert_eq!(e.name, "PathNotMapped", "{e:?}");
    // review-2 L4: an unmapped path after a successful render is a configuration problem,
    // not a retryable one
    assert!(!e.retry_safe);
    assert_eq!(e.data["path"], json!("Q:\\x\\y.png"));
    assert!(e.data.contains_key("hint"), "the hint names --path-map");
}

/// review-2 L3: two hammer-mcp processes can make their first unnamed render in the same second;
/// the client side therefore proposes no stem at all (the request carries no `name` and the
/// bridge's global `render_<tick>_<seq>` names the file), so nothing collides and the two replies
/// still name two different files.
#[tokio::test]
async fn render_defaults_name_nothing_on_the_wire_so_the_bridge_names_the_file() {
    let host = Host::new("no-name");
    let mb = render_mock(&host).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);

    let views = mb.method_calls("view.render");
    assert!(views.is_empty(), "no request yet");
    let first = server.call("render_view", json!({"inline": false})).await.unwrap();
    let second = server.call("render_view", json!({"inline": false})).await.unwrap();
    let world = server
        .call(
            "render_world",
            json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "inline": false}),
        )
        .await
        .unwrap();

    // no `name` on any wire request for the (unnamed) calls
    for sent in mb.method_calls("view.render").iter().chain(mb.method_calls("world.render").iter()) {
        assert!(sent.params.get("name").is_none(), "no default stem is proposed: {sent:?}");
    }
    // the files are the bridge's `render_<seq>` stems, written, and unique per call
    let files: Vec<String> = [
        first["file"].as_str().expect("file"),
        second["file"].as_str().expect("file"),
        world["file"].as_str().expect("file"),
    ]
    .map(str::to_string)
    .to_vec();
    for file in &files {
        assert!(file.starts_with("render_"), "the bridge's stem, not a client default: {file}");
        assert!(host.renders().join(file).is_file(), "{file} was written");
    }
    assert_ne!(files[0], files[1], "two unnamed renders never overwrite each other");
}

/// review-2 L4: with `inline:false` a render whose host path does not map (or maps to nothing)
/// still succeeds — the render ran, the PNG is on the Windows side, and the bridge's statistics
/// are already in the reply; the result carries `hostPath: null` plus a note and shoots down no
/// client retry that re-renders multi-second images for the same path result.
#[tokio::test]
async fn a_render_without_inline_answers_the_path_problem_softly() {
    let host = Host::new("soft-path");

    // (a) the path maps but the file is not there (the mock stands in for the bridge's path
    // without writing it)
    let mb = MockBridge::builder().renders().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let out = server
        .call(
            "render_world",
            json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "inline": false}),
        )
        .await
        .unwrap();
    assert_eq!(out["hostPath"], Value::Null, "{out}");
    assert_eq!(out["localStats"], Value::Null, "no local decode for inline:false");
    let notes: Vec<String> = out["notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    assert!(
        notes.iter().any(|note| note.contains("was not read back")),
        "the note says the image was not read back: {notes:?}"
    );
    assert_eq!(mb.method_calls("world.render").len(), 1);

    // (b) the path does not map at all
    let mb = MockBridge::builder()
        .renders()
        .responder("world.render", |_| {
            json!({"path": "Q:\\x\\y.png", "file": "y.png", "width": 64, "height": 48, "bytes": 1,
                   "stats": {"nonBlackPixels": 1}, "ms": {"render": 1}})
        })
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let out = server
        .call(
            "render_world",
            json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "inline": false}),
        )
        .await
        .unwrap();
    assert_eq!(out["hostPath"], Value::Null);

    // (c) with `inline:true` the two are still hard errors, worded for the path-map problem
    let mb = MockBridge::builder().renders().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call(
            "render_world",
            json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "inline": true}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.name, "RenderFileMissing");
    assert!(!e.retry_safe);
}

// ---------------------------------------------------------------- render_world

#[tokio::test]
async fn render_world_4k_is_not_inlined_by_default_and_a_small_one_is() {
    let host = Host::new("world-4k");
    let mb = render_mock(&host).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);

    let out = server
        .call_output(
            "render_world",
            json!({
                "origin": [-1400, 2500, 400], "angles": [10, -45, 0],
                "width": 3840, "height": 2160, "name": "w4k",
            }),
        )
        .await
        .unwrap();

    let calls = mb.method_calls("world.render");
    let sent = &calls.last().unwrap().params;
    assert_eq!((sent["width"].clone(), sent["height"].clone(), sent["name"].clone()),
               (json!(3840), json!(2160), json!("w4k")));
    assert_eq!(sent["origin"], json!([-1400.0, 2500.0, 400.0]));
    assert_eq!(sent["angles"], json!([10.0, -45.0, 0.0]));
    assert_eq!(out.json["camera"]["mode"], json!("explicit"));
    assert_eq!(out.json["pipeline"], json!("Tools->CSGO"));
    assert_eq!(out.json["renderMs"], json!(19));
    assert_eq!(out.json["readbackMs"], json!(180));
    assert_eq!(out.json["inline"], Value::Null, "> 1080p: not inlined unless asked");
    assert!(out.images.is_empty());

    let out = server
        .call_output(
            "render_world",
            json!({"origin": [0, 0, 600], "angles": [89, 0, 0], "width": 640, "height": 360}),
        )
        .await
        .unwrap();
    assert_eq!(out.json["inline"]["mime"], json!("image/png"), "small: inlined by default");
    assert_eq!(out.images.len(), 1);
}

#[tokio::test]
async fn render_world_orbit_and_validation_never_dispatches() {
    let host = Host::new("world-bad");
    let mb = render_mock(&host).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);

    // orbit works (lookAt-only, no origin)
    let out = server
        .call(
            "render_world",
            json!({"lookAt": [0, 0, 0], "distance": 100, "pitch": 45, "yaw": 0, "inline": false}),
        )
        .await
        .unwrap();
    let calls = mb.method_calls("world.render");
    let sent = &calls.last().unwrap().params;
    assert_eq!(sent["angles"][0], json!(45.0));
    assert_eq!(out["camera"]["mode"], json!("orbit"));
    let n = mb.method_calls("world.render").len();

    for args in [
        json!({}),
        json!({"origin": [1, 2, 3]}),
        json!({"angles": [0, 0, 0]}),
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "width": 8193}),
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "width": 8192, "height": 8192}),
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "fov": 0.5}),
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "fov": 179}),
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "znear": 0}),
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "name": "x y"}),
        json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "hideToolMaterials": true}),
        json!({"distance": 100}),
    ] {
        let e = server.call("render_world", args.clone()).await.unwrap_err();
        assert_eq!(e.name, "InvalidParams", "{args}");
        assert_eq!(e.code, Some(-32602), "{args}");
    }
    assert_eq!(
        mb.method_calls("world.render").len(),
        n,
        "validation happens before anything is sent"
    );
}

#[tokio::test]
async fn render_world_no_game_world() {
    let host = Host::new("world-noworld");
    // review-2 L6: NoGameWorld is the rare case (the engine has no scene world object at all
    // and `GetGameSceneWorld` is null); the knob stands in for it
    let mb = render_mock(&host)
        .world_no_scene_world(true)
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call("render_world", json!({"origin": [0, 0, 0], "angles": [0, 0, 0]}))
        .await
        .unwrap_err();
    assert_eq!(e.name, "NoGameWorld", "{e:?}");
    assert_eq!(e.code, Some(-32021));
    assert!(
        e.retry_safe,
        "the bridge never started the render (nothing loaded - check first)"
    );
    assert_eq!(
        e.data["reason"],
        json!("no map is loaded in the engine (map <name>)")
    );
    assert_eq!(mb.method_calls("world.render").len(), 1);
}

/// review-2 L6: an engine with **no map loaded** (after `disconnect`) still renders its empty
/// scene world: `render_world {lookAt}` succeeds with a uniform-black image and the blank note
/// names the engine-side cause — NOT a -32021 error. The mock's `world_no_map` arm models it
/// (black PNG, all-zero stats).
#[tokio::test]
async fn render_world_no_map_renders_the_empty_scene_world_black() {
    let host = Host::new("world-no-mmap");
    let mb = render_mock(&host).world_no_map(true).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let out = server
        .call(
            "render_world",
            json!({"lookAt": [0, 0, 0], "distance": 100, "pitch": 30, "yaw": 0, "inline": false}),
        )
        .await
        .unwrap();
    assert_eq!(out["stats"]["nonBlackPixels"], json!(0), "black: {out}");
    assert_eq!(out["stats"]["maxLuma"], json!(0));
    let notes: Vec<String> = out["notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    assert!(
        notes.iter().any(|note| note.contains("blank")),
        "the blank note fires: {notes:?}"
    );
    assert!(
        notes.iter().any(|note| note.contains("no map is loaded in the engine")),
        "the note names the engine-side cause: {notes:?}"
    );
    // the black PNG really was written (the file the bridge would have written)
    let file = out["file"].as_str().expect("file name");
    assert!(host.renders().join(file).is_file(), "{} was written", file);
}

#[tokio::test]
async fn render_world_unsaved_edits_note_and_blank_note() {
    let host = Host::new("world-notes");
    let mb = render_mock(&host).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    server.state.edits.lock().unwrap().since_save = 1;
    let out = server
        .call(
            "render_world",
            json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "inline": false}),
        )
        .await
        .unwrap();
    assert!(
        out["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap_or("").contains("COMPILED map")),
        "{:?}",
        out["notes"]
    );

    // a blank engine render names the engine-sides causes too
    let mb = render_mock(&host)
        .responder("world.render", |params| {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("x");
            json!({
                "path": format!("Z:\\app\\renders\\{name}.png"), "file": format!("{name}.png"),
                "width": 1920, "height": 1080, "bytes": 1,
                "camera": {"origin": [0, 0, 0], "angles": [0, 0, 0], "fov": 90},
                "pipeline": "Tools->CSGO", "srgb": false,
                "stats": {"nonBlackPixels": 0, "minLuma": 0, "maxLuma": 0},
                "ms": {"render": 1, "readback": 1},
            })
        })
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let out = server
        .call(
            "render_world",
            json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "name": "black", "inline": false}),
        )
        .await
        .unwrap();
    let notes = out["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or("").contains("no map is loaded in the engine")),
        "{:?}",
        notes
    );
}

/// review-2 N3: `zfar` alone no longer reaches the wire out of bounds — the tool checks it
/// against `(0, 1e6]` (and `render_world` against its own default `znear` of 4, the pair rule the
/// bridge applies to a `znear`-less call), keeping the module's "nothing out of bounds reaches
/// the wire" claim as close to true as a live viewport's `znear` allows.
#[tokio::test]
async fn a_zfar_alone_is_refused_locally() {
    let host = Host::new("zfar-n3");
    let mb = render_mock(&host).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);

    for (method, args) in [
        ("render_view", json!({"zfar": 2_000_000.0})),
        ("render_view", json!({"zfar": 0})),
        ("render_world", json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "zfar": 2_000_000.0})),
        // the bridge's own pair rule against the *default* znear (4 for world)
        ("render_world", json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "zfar": 3.0})),
    ] {
        let e = server.call(method, args.clone()).await.unwrap_err();
        assert_eq!(e.name, "InvalidParams", "{method}: {args:?} -> {e:?}");
    }
    assert!(
        mb.method_calls("view.render").is_empty() && mb.method_calls("world.render").is_empty(),
        "the out-of-bounds values never reach the wire (review-2 N3)"
    );

    // the bound itself stays open: `zfar` at the bridge's own 1e6 limit passes and dispatches
    let out = server
        .call("render_view", json!({"znear": 10.0, "zfar": 1_000_000.0, "inline": false}))
        .await
        .unwrap();
    assert_eq!(out["camera"]["zfar"], json!(1_000_000.0), "{out}");
    assert_eq!(mb.method_calls("view.render").len(), 1);
}

/// review-2 N4: the camera echoes follow the real replies — the world camera has no `aspect`
/// (only the view's does), the echo defaults are the bridge's own fallbacks (view 1/9000, world
/// 4/30000), and `world.render`'s required-camera checks come first (a call with several bad
/// arguments gets the bridge's own first message, not a later one).
#[tokio::test]
async fn the_camera_echoes_follow_the_bridge_replies() {
    let host = Host::new("n4-echo");
    let mb = render_mock(&host).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);

    // world: no `aspect`, defaults 4/30000
    let out = server
        .call(
            "render_world",
            json!({"origin": [0, 0, 0], "angles": [0, 0, 0], "inline": false}),
        )
        .await
        .unwrap();
    assert!(
        out["camera"].get("aspect").is_none(),
        "op_world_render's reply has no aspect: {:?}",
        out["camera"]
    );
    assert_eq!(out["camera"]["znear"], json!(4.0));
    assert_eq!(out["camera"]["zfar"], json!(30000.0));

    // view: the fallback pair is the bridge's 1/9000
    let out = server.call("render_view", json!({})).await.unwrap();
    assert_eq!(out["camera"]["znear"], json!(1.0));
    assert_eq!(out["camera"]["zfar"], json!(9000.0));

    // world's required-camera parse order: origin first, despite later bad arguments
    for (args, needle) in [
        (json!({"width": 99999}), "origin"),
        (json!({"origin": [0, 0, 0], "width": 99999}), "angles"),
    ] {
        let e = server.call("render_world", args.clone()).await.unwrap_err();
        assert!(
            e.message.contains(needle),
            "the bridge's own parse order: {args:?} -> {}",
            e.message
        );
    }
}

// ---------------------------------------------------------------- gating

#[tokio::test]
async fn render_tools_without_the_capability_are_not_implemented() {
    let host = Host::new("no-cap");
    // the mock's baseline advertises no view.render/world.render
    let mb = MockBridge::start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("render_view", json!({})).await.unwrap_err();
    assert_eq!(e.name, "NotImplemented", "{e:?}");
    assert_eq!(e.code, Some(-32011));
    assert_eq!(e.data["missing"], json!(["view.render"]));
    assert!(
        mb.method_calls("view.render").is_empty(),
        "nothing was sent"
    );
    let e = server
        .call("render_world", json!({"origin": [0, 0, 0], "angles": [0, 0, 0]}))
        .await
        .unwrap_err();
    assert_eq!(e.name, "NotImplemented", "{e:?}");
    assert_eq!(e.data["missing"], json!(["world.render"]));
    assert!(mb.method_calls("world.render").is_empty(), "nothing was sent");
}
