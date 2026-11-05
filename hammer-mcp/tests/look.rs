//! WP3.1: `look` against the mock bridge (render built-ins, a Hammer world, a stateful
//! `game.local`). The tools are the registered production ones; the pure framing math is unit
//! tested in `src/tools/look.rs`.
#![allow(clippy::result_large_err)]

use cs2_dmx::{codec::binary, Attribute, Datamodel, Element, ElementId, Guid};
use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
use cs2_mcp_common::tools::{CallFailure, ToolError, ToolOutput};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);
const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "look-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let app = root.join("workshop/run/app");
        let target = app.join("content/csgo_addons/mcp_test/maps/probe.vmap");
        std::fs::create_dir_all(target.parent().unwrap()).expect("tmp tree");
        std::fs::copy(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/vmap/probe.vmap"), &target)
            .expect("install probe");
        Tree { root, app }
    }
    fn renders(&self) -> PathBuf {
        self.app.join("renders")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn config(tree: &Tree, discovery: &Path) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let map = format!("z:\\app={}", tree.app.display());
    let discovery = discovery.display().to_string();
    let given: Vec<&str> = vec![
        "--deadline-ms", "5000", "--tool-budget-ms", "45000", "--discovery", &discovery,
        "--project-root", &root, "--path-map", &map,
    ];
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

async fn call_out(server: &McpServer<HammerState>, args: Value) -> Result<ToolOutput, ToolError> {
    match server.call("look", args).await {
        Ok(out) => Ok(out),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

async fn call(server: &McpServer<HammerState>, args: Value) -> Result<Value, ToolError> {
    call_out(server, args).await.map(|o| o.json)
}

fn kv(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn builder(tree: &Tree) -> cs2_mcp_common::mock::MockBuilder {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .renders()
        .render_dir(tree.renders())
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(67, "func_buyzone", [100.0, 0.0, 0.0], kv(&[("targetname", "bz_a")])),
            MockNode::entity(70, "func_buyzone", [200.0, 0.0, 0.0], kv(&[("targetname", "bz_b")])),
        ])
}

fn game_local(b: cs2_mcp_common::mock::MockBuilder) -> cs2_mcp_common::mock::MockBuilder {
    b.responder("game.local", |_| {
        json!({
            "tick": 1, "inGame": true, "map": "probe", "playerSlot": 0,
            "viewAngles": [10.0, 90.0, 0.0],
            "player": {"alive": true, "origin": [500.0, 600.0, 0.0], "eye": [500.0, 600.0, 64.0],
                       "velocity": [0.0, 0.0, 0.0], "activeWeapon": null}
        })
    })
}

fn render_calls(mb: &MockBridge) -> Vec<(String, Value)> {
    mb.requests()
        .into_iter()
        .filter(|r| r.method == "world.render" || r.method == "view.render")
        .map(|r| (r.method, r.params))
        .collect()
}

/// Nothing `look` does may touch the document or any camera: only reads and render calls.
fn assert_render_only(mb: &MockBridge) {
    for r in mb.requests() {
        let m = r.method.as_str();
        assert!(
            ["hello", "doc.active", "entity.get", "entity.find", "node.list", "game.local", "world.render", "view.render"]
                .contains(&m)
                || m.starts_with("control.")
                || m.starts_with("caps")
                || m.starts_with("session"),
            "look sent a non-read, non-render request: {m}"
        );
    }
}

#[tokio::test]
async fn angles_on_an_entity_renders_three_framed_views_and_moves_no_camera() {
    let tree = Tree::new("angles");
    let mb = builder(&tree).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let out = call_out(&server, json!({"target": "BZ_A"})).await.unwrap();
    let v = &out.json;
    assert_eq!(v["mode"], "angles");
    assert_eq!(v["renderer"], "engine (render_world)");
    assert_eq!(v["target"]["kind"], "entity");
    assert_eq!(v["target"]["box"]["min"], json!([36.0, -64.0, -64.0]));
    assert_eq!(v["target"]["box"]["max"], json!([164.0, 64.0, 64.0]));
    assert_eq!(v["cameraRestored"], true);
    assert_eq!(v["cameraMoved"], false);
    let views = v["views"].as_array().unwrap();
    let labels: Vec<&str> = views.iter().map(|x| x["label"].as_str().unwrap()).collect();
    assert_eq!(labels, ["corner_sw", "corner_ne", "top"]);
    assert_eq!(out.images.len(), 3, "one inline image per view");
    assert_eq!(views[2]["imageIndex"], 2);
    // the wire: explicit cameras on world.render, default small size, nothing else
    let calls = render_calls(&mb);
    assert_eq!(calls.len(), 3);
    for (method, p) in &calls {
        assert_eq!(method, "world.render");
        assert_eq!((p["width"].clone(), p["height"].clone()), (json!(640), json!(360)));
        assert_eq!(p["fov"], json!(70.0));
        assert!(p["origin"].is_array() && p["angles"].is_array());
        assert!(p["zfar"].as_f64().unwrap() >= 30000.0);
    }
    // the first view looks at the entity (yaw 45: from -x,-y), the top view looks nearly down
    assert_eq!(calls[0].1["angles"][1], json!(45.0));
    assert!(calls[2].1["angles"][0].as_f64().unwrap() > 70.0);
    assert!(views[0]["hostPath"].as_str().unwrap().ends_with(".png"));
    assert_render_only(&mb);
}

#[tokio::test]
async fn max_size_is_applied_and_the_name_stem_gets_the_view_label() {
    let tree = Tree::new("size");
    let mb = builder(&tree).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let v = call(&server, json!({"target": 67, "maxSize": 256, "name": "shot"})).await.unwrap();
    assert_eq!(v["target"]["nodeID"], 67);
    assert_eq!(v["maxSize"], 256);
    let calls = render_calls(&mb);
    assert_eq!((calls[0].1["width"].clone(), calls[0].1["height"].clone()), (json!(256), json!(144)));
    assert_eq!(calls[0].1["name"], "shot_corner_sw");
    assert_eq!(v["views"][0]["size"], json!([256, 144]));
}

#[tokio::test]
async fn point_and_box_targets_and_tour_and_the_editor_source() {
    let tree = Tree::new("shapes");
    let mb = builder(&tree).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let v = call(&server, json!({"target": [10, 20, 30], "mode": "tour", "count": 4})).await.unwrap();
    assert_eq!(v["target"]["kind"], "point");
    assert_eq!(v["target"]["center"], json!([10.0, 20.0, 30.0]));
    assert_eq!(v["views"].as_array().unwrap().len(), 4);
    assert_eq!(v["views"][1]["label"], "tour_2_yaw135");
    let v = call(&server, json!({"target": {"min": [0, 0, 0], "max": [100, 200, 50]}, "source": "editor"}))
        .await
        .unwrap();
    assert_eq!(v["target"]["kind"], "box");
    assert_eq!(v["renderer"], "editor (render_view)");
    assert_eq!(mb.method_calls("view.render").len(), 3);
    for bad in [
        json!({"target": {"min": [5, 0, 0], "max": [1, 1, 1]}}),
        json!({"target": [1, 2]}),
        json!({"target": true}),
        json!({"target": 1.5}),
    ] {
        let e = call(&server, bad.clone()).await.unwrap_err();
        assert_eq!(e.name, "InvalidParams", "{bad}");
    }
}

#[tokio::test]
async fn the_map_target_is_the_saved_maps_bounds() {
    let tree = Tree::new("map");
    let mb = builder(&tree).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let scene = server.call("get_scene_info", json!({"source": "saved", "limit": 1})).await.ok().unwrap().json;
    let v = call(&server, json!({"target": "map"})).await.unwrap();
    assert_eq!(v["target"]["kind"], "map");
    // the world geometry frames the map when there is any; the entity origins only otherwise
    let (bounds, used) = if scene["meshBounds"].is_object() {
        ("meshBounds", "mesh extents")
    } else {
        ("bounds", "entity origins")
    };
    assert_eq!(v["target"]["framedOn"], used, "{v}");
    assert_eq!(v["target"]["box"]["min"], scene[bounds]["min"]);
    assert_eq!(v["target"]["box"]["max"], scene[bounds]["max"]);
    // a map this big needs a far camera: the planes follow
    assert!(render_calls(&mb)[0].1["zfar"].as_f64().unwrap() >= 30000.0);
}

#[tokio::test]
async fn an_unknown_target_says_what_it_could_be() {
    let tree = Tree::new("unknown");
    let mb = builder(&tree).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let e = call(&server, json!({"target": "bz_c"})).await.unwrap_err();
    assert_eq!(e.name, "EntityNotFound");
    let near: Vec<&str> = e.data["didYouMean"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
    assert!(near.contains(&"bz_a") && near.contains(&"bz_b"), "{near:?}");
    let e = call(&server, json!({"target": 99999})).await.unwrap_err();
    assert_eq!(e.name, "EntityNotFound");
    assert!(render_calls(&mb).is_empty(), "nothing rendered for an unknown target");
}

#[tokio::test]
async fn viewport_mode_is_render_view_with_the_camera_left_alone() {
    let tree = Tree::new("viewport");
    let mb = builder(&tree).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let out = call_out(&server, json!({})).await.unwrap();
    assert_eq!(out.json["mode"], "viewport");
    assert_eq!(out.json["views"][0]["pose"]["source"], "viewport");
    assert_eq!(out.images.len(), 1);
    let calls = render_calls(&mb);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "view.render");
    assert!(calls[0].1.get("origin").is_none(), "no camera is sent: the viewport as it is");
    assert_render_only(&mb);
}

#[tokio::test]
async fn a_render_failing_midway_returns_the_good_views_and_leaves_the_camera_alone() {
    let tree = Tree::new("midway");
    let mb = builder(&tree).error_on_call("world.render", 2, -32021, "NoGameWorld", None).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let v = call(&server, json!({"target": "bz_a"})).await.unwrap();
    assert_eq!(v["cameraRestored"], true);
    assert_eq!(v["cameraMoved"], false);
    assert_eq!(v["views"].as_array().unwrap().len(), 2, "view 1 and 3 rendered");
    let failed = v["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["label"], "corner_ne");
    assert!(v["notes"].to_string().contains("1 of 3 views failed"), "{v}");
    assert_render_only(&mb);
}

#[tokio::test]
async fn nothing_rendered_is_an_error_not_an_empty_success() {
    let tree = Tree::new("allfail");
    let mb = builder(&tree).world_no_scene_world(true).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let e = call(&server, json!({"target": [0, 0, 0]})).await.unwrap_err();
    assert!(!e.message.is_empty());
    assert_render_only(&mb);
}

#[tokio::test]
async fn frames_without_a_running_game_is_refused_with_use_instead_and_renders_nothing() {
    let tree = Tree::new("frames-nogame");
    let mb = builder(&tree).error("game.local", -32024, "no local player", None).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let e = call(&server, json!({"target": "bz_a", "mode": "frames"})).await.unwrap_err();
    assert_eq!(e.name, "NoMapLoaded");
    assert_eq!(e.data["useInstead"]["tool"], "run_map");
    assert!(render_calls(&mb).is_empty());
}

#[tokio::test]
async fn frames_take_the_same_pose_repeatedly_when_the_game_runs() {
    let tree = Tree::new("frames");
    let mb = game_local(builder(&tree)).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let out = call_out(&server, json!({"target": "bz_a", "mode": "frames", "count": 3, "intervalMs": 10}))
        .await
        .unwrap();
    assert_eq!(out.json["views"].as_array().unwrap().len(), 3);
    assert_eq!(out.json["intervalMs"], 10);
    assert_eq!(out.images.len(), 3);
    let calls = render_calls(&mb);
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].1["origin"], calls[2].1["origin"], "same pose every frame");
    assert_eq!(calls[0].1["angles"], calls[2].1["angles"]);
    assert_render_only(&mb);
}

#[tokio::test]
async fn player_eye_uses_the_running_players_eye_or_an_explicit_pose() {
    let tree = Tree::new("eye");
    let mb = game_local(builder(&tree)).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let v = call(&server, json!({"mode": "player_eye"})).await.unwrap();
    assert_eq!(v["poseFrom"], "game.local player.eye");
    let calls = render_calls(&mb);
    assert_eq!(calls[0].1["origin"], json!([500.0, 600.0, 64.0]));
    assert_eq!(calls[0].1["angles"], json!([10.0, 90.0, 0.0]));
    // an explicit pose needs no game
    let v = call(
        &server,
        json!({"mode": "player_eye", "pose": {"origin": [1, 2, 3], "angles": [4, 5, 6]}, "maxSize": 128}),
    )
    .await
    .unwrap();
    assert_eq!(v["poseFrom"], "pose argument");
    let calls = render_calls(&mb);
    assert_eq!(calls[1].1["origin"], json!([1.0, 2.0, 3.0]));
    assert_eq!(calls[1].1["width"], 128);
}

#[tokio::test]
async fn player_eye_without_a_game_or_pose_points_at_run_map() {
    let tree = Tree::new("eye-nogame");
    let mb = builder(&tree).error("game.local", -32024, "no local player", None).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let e = call(&server, json!({"mode": "player_eye"})).await.unwrap_err();
    assert_eq!(e.data["useInstead"]["tool"], "run_map");
}

#[tokio::test]
async fn look_is_read_only_and_listed_with_a_bounded_schema() {
    let r = hammer_mcp::build_registry();
    let spec = r.spec("look").unwrap();
    assert_eq!(spec.mutability, cs2_mcp_common::tools::Mutability::ReadOnly);
    assert_eq!(spec.input_schema["additionalProperties"], json!(false));
    assert!(r.names().contains(&"render_view") && r.names().contains(&"render_world"));
}

// ------------------------------------------------------------------ review fixes (WP3.1b)

/// A `world.render` stand-in: call n answers per `plan(n)` = `Some(blank)` for a render whose PNG
/// exists (blank or not) or `None` for a path with no file behind it (`RenderFileMissing`).
fn scripted_world(
    tree: &Tree,
    b: cs2_mcp_common::mock::MockBuilder,
    plan: fn(usize) -> Option<bool>,
) -> cs2_mcp_common::mock::MockBuilder {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    let png = cs2_mcp_common::capture::frame::RgbImage::new(8, 4, vec![120u8; 96]).unwrap().to_png().unwrap();
    std::fs::create_dir_all(tree.renders()).unwrap();
    std::fs::write(tree.renders().join("ok.png"), &png).unwrap();
    let n = Arc::new(AtomicUsize::new(0));
    b.responder("world.render", move |_| {
        let i = n.fetch_add(1, Ordering::SeqCst) + 1;
        let (file, blank) = match plan(i) {
            Some(blank) => ("ok.png", blank),
            None => ("gone.png", false),
        };
        let stats = if blank {
            json!({"untouchedPixels": 0, "nonBlackPixels": 0, "meanLuma": 0, "minLuma": 0, "maxLuma": 0, "crc32": "0"})
        } else {
            json!({"untouchedPixels": 0, "nonBlackPixels": 32, "meanLuma": 120, "minLuma": 10, "maxLuma": 200, "crc32": "1"})
        };
        json!({
            "path": format!("Z:\\app\\renders\\{file}"), "file": file, "width": 8, "height": 4, "bytes": 1,
            "camera": {"origin": [0, 0, 0], "angles": [0, 0, 0], "fov": 70, "source": "explicit"},
            "stats": stats, "ms": {"render": 1, "readback": 1},
        })
    })
}

#[tokio::test]
async fn all_blank_engine_views_fail_with_use_instead_editor() {
    let tree = Tree::new("blank-all");
    let mb = builder(&tree).world_no_map(true).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let e = call(&server, json!({"target": "bz_a"})).await.unwrap_err();
    assert_eq!(e.name, "BlankRender");
    assert_eq!(e.data["useInstead"]["tool"], "look");
    assert_eq!(e.data["useInstead"]["args"]["source"], "editor");
    assert_eq!(e.data["useInstead"]["args"]["target"], "bz_a");
    assert_eq!(e.data["blankViews"].as_array().unwrap().len(), 3);
    // the suggested call works: the editor renders the live document
    let v = call(&server, e.data["useInstead"]["args"].clone()).await.unwrap();
    assert_eq!(v["renderer"], "editor (render_view)");
    assert!(v.get("blankViews").is_none());
}

#[tokio::test]
async fn a_partly_blank_set_is_flagged_first_in_the_notes() {
    let tree = Tree::new("blank-part");
    let mb = scripted_world(&tree, builder(&tree), |i| Some(i == 2)).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let v = call(&server, json!({"target": "bz_a"})).await.unwrap();
    assert_eq!(v["blankViews"], json!(["corner_ne"]));
    assert!(v["notes"][0].as_str().unwrap().starts_with("WARNING: 1 of 3 views are blank (corner_ne)"), "{v}");
    assert_eq!(v["views"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn next_says_how_to_see_document_edits() {
    let tree = Tree::new("next");
    let mb = builder(&tree).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let out = call_out(&server, json!({"target": "bz_a"})).await.unwrap();
    let next = out.next.unwrap();
    let look = next.iter().find(|s| s.tool == "look").expect("a look step");
    assert!(look.why.contains("COMPILED"), "{}", look.why);
    assert_eq!(look.args.as_ref().unwrap()["source"], "editor");
    assert!(next.iter().any(|s| s.tool == "compile_map"));
}

#[tokio::test]
async fn a_stop_error_midway_lists_every_unrendered_view() {
    let tree = Tree::new("stop");
    // render 1 fine, render 2 reports a PNG that is not there (RenderFileMissing: deterministic)
    let mb = scripted_world(&tree, builder(&tree), |i| (i == 1).then_some(false)).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let v = call(&server, json!({"target": "bz_a", "mode": "tour", "count": 5})).await.unwrap();
    assert_eq!(render_calls(&mb).len(), 2, "the loop stopped at the stop error");
    assert_eq!(v["views"].as_array().unwrap().len(), 1);
    let failed = v["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 4, "1 failed + 3 not rendered");
    assert_eq!(failed[0]["type"], "RenderFileMissing");
    assert!(failed[1]["error"].as_str().unwrap().starts_with("not rendered: RenderFileMissing"), "{failed:?}");
    assert!(v["notes"].to_string().contains("4 of 5 views failed or were not rendered"), "{v}");
}

#[tokio::test]
async fn a_bridge_without_the_render_methods_is_not_implemented_and_sends_no_render() {
    let tree = Tree::new("norender");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(67, "func_buyzone", [100.0, 0.0, 0.0], kv(&[("targetname", "bz_a")])),
        ])
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    for args in [json!({"target": "bz_a"}), json!({"target": "bz_a", "source": "editor"}), json!({})] {
        let e = call(&server, args.clone()).await.unwrap_err();
        assert_eq!(e.name, "NotImplemented", "{args}");
    }
    assert!(render_calls(&mb).is_empty());
}

#[tokio::test]
async fn name_too_long_for_the_stem_is_refused() {
    let tree = Tree::new("name");
    let mb = builder(&tree).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let e = call(&server, json!({"target": "bz_a", "name": "a".repeat(60)})).await.unwrap_err();
    assert_eq!(e.name, "InvalidParams");
    let ok = call(&server, json!({"target": "bz_a", "mode": "tour", "count": 2, "name": "a".repeat(49)})).await.unwrap();
    assert_eq!(ok["views"].as_array().unwrap().len(), 2);
    assert!(render_calls(&mb).iter().all(|(_, p)| p["name"].as_str().unwrap().len() <= 64));
}

#[tokio::test]
async fn a_numeric_string_target_is_tried_as_a_node_id_and_suggestions_come_from_named_entities() {
    let tree = Tree::new("numeric");
    let mut nodes = vec![MockNode::world()];
    for i in 0..600 {
        nodes.push(MockNode::entity(1000 + i, "info_target", [0.0; 3], kv(&[])));
    }
    nodes.push(MockNode::entity(67, "func_buyzone", [100.0, 0.0, 0.0], kv(&[("targetname", "bz_alpha")])));
    nodes.push(MockNode::entity(5000, "func_buyzone", [9.0, 0.0, 0.0], kv(&[("targetname", "bz_beta")])));
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .renders()
        .render_dir(tree.renders())
        .hammer_nodes(nodes)
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let v = call(&server, json!({"target": "67"})).await.unwrap();
    assert_eq!(v["target"]["nodeID"], 67);
    let e = call(&server, json!({"target": "bz_alfa"})).await.unwrap_err();
    let near: Vec<&str> = e.data["didYouMean"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
    assert!(near.contains(&"bz_alpha"), "{near:?}");
}

#[tokio::test]
async fn player_eye_reports_the_fov_and_the_angle_fallback_honestly() {
    let tree = Tree::new("eye-honest");
    let mb = builder(&tree)
        .responder("game.local", |_| {
            json!({"tick": 1, "inGame": true, "map": "p", "playerSlot": 0,
                   "player": {"alive": true, "origin": [1.0, 2.0, 3.0], "velocity": [0, 0, 0], "activeWeapon": null}})
        })
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    let v = call(&server, json!({"mode": "player_eye"})).await.unwrap();
    let from = v["poseFrom"].as_str().unwrap();
    assert!(from.contains("origin + 64") && from.contains("NO view angles"), "{from}");
    assert_eq!(v["views"][0]["pose"]["fov"], 90.0, "engine default");
    let v = call(&server, json!({"mode": "player_eye", "source": "editor"})).await.unwrap();
    assert_eq!(v["views"][0]["pose"]["fov"], "the viewport's");
}

// ------------------------------------------------------------------------------------ prefab target

fn guid(n: u8) -> Guid {
    let mut bytes = [0u8; 16];
    bytes[0] = n;
    bytes[15] = 0xA0;
    Guid(bytes)
}

fn element(n: u8, class: &str, attributes: Vec<(&str, Attribute)>) -> Element {
    Element {
        class_name: class.into(),
        name: String::new(),
        id: guid(n),
        stub: false,
        attributes: attributes.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
    }
}

/// root(0) -> world(1) -> [prefab(2), entity(3)]; entity_properties(4).
fn map_with_a_prefab() -> Vec<u8> {
    let elements = vec![
        element(1, "CMapRootElement", vec![("world", Attribute::Element(Some(ElementId(1))))]),
        element(
            2,
            "CMapWorld",
            vec![
                ("nodeID", Attribute::Int(1)),
                ("children", Attribute::ElementArray(vec![Some(ElementId(2)), Some(ElementId(3))])),
            ],
        ),
        element(
            3,
            "CMapPrefab",
            vec![
                ("nodeID", Attribute::Int(7)),
                ("origin", Attribute::Vector3([128.0, 64.0, 0.0])),
                ("angles", Attribute::QAngle([0.0, 45.0, 0.0])),
                ("scales", Attribute::Vector3([1.0, 1.0, 1.0])),
                ("targetMapPath", Attribute::String("maps/prefabs/crate.vmap".into())),
                ("fixupEntityNames", Attribute::Bool(true)),
                ("loadAtRuntime", Attribute::Bool(false)),
            ],
        ),
        element(
            4,
            "CMapEntity",
            vec![
                ("nodeID", Attribute::Int(9)),
                ("origin", Attribute::Vector3([1.0, 2.0, 3.0])),
                ("entity_properties", Attribute::Element(Some(ElementId(4)))),
            ],
        ),
        element(
            5,
            "EditGameClassProps",
            vec![
                ("classname", Attribute::String("info_target".into())),
                ("targetname", Attribute::String("marker".into())),
            ],
        ),
    ];
    let dm = Datamodel {
        encoding: "binary".into(),
        encoding_version: 9,
        format: "vmap".into(),
        format_version: 29,
        elements,
        root: Some(ElementId(0)),
        ..Default::default()
    };
    binary::encode(&dm).expect("encode the fixture")
}


#[tokio::test]
async fn a_prefab_node_is_a_target_box_around_its_origin() {
    let tree = Tree::new("prefab");
    // the active document is a saved map with a CMapPrefab node (nodeID 7, origin [128,64,0])
    std::fs::write(tree.app.join("content/csgo_addons/mcp_test/maps/probe.vmap"), map_with_a_prefab()).unwrap();
    let mb = builder(&tree).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path)).unwrap();
    for target in [json!(7), json!("7")] {
        let v = call(&server, json!({"target": target})).await.unwrap();
        assert_eq!(v["target"]["kind"], "prefab", "{v}");
        assert_eq!(v["target"]["nodeID"], 7);
        assert_eq!(v["target"]["box"], json!({"min": [64.0, 0.0, -64.0], "max": [192.0, 128.0, 64.0]}));
        assert_eq!(v["target"]["center"], json!([128.0, 64.0, 0.0]));
        assert_eq!(v["target"]["framedOn"], "prefab origin +-64");
        assert_eq!(v["views"].as_array().unwrap().len(), 3);
    }
    // an entity still wins, and an id that is neither entity nor prefab stays EntityNotFound
    assert_eq!(call(&server, json!({"target": 67})).await.unwrap()["target"]["kind"], "entity");
    let e = call(&server, json!({"target": 4242})).await.unwrap_err();
    assert_eq!(e.name, "EntityNotFound", "{e:?}");
    // the world node is not an entity either: the refusal now says a prefab is read from the saved file
    let e = call(&server, json!({"target": 1})).await.unwrap_err();
    assert!(e.name == "NotAnEntity" && e.message.contains("save_map"), "{e:?}");
    assert_render_only(&mb);
}
