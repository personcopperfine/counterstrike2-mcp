//! `new_map` (create a .vmap in an empty addon and open it) and the next steps that point at it.
//! The temp project tree lives under `CARGO_TARGET_TMPDIR`; the real addon_template and every real
//! addon are never touched (the template is a copy of the committed `probe.vmap` fixture).
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
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
    /// An EMPTY addon `mcp_test` (no maps/) and Valve's template addon with one map.
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "newmap-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let app = root.join("workshop/run/app");
        std::fs::create_dir_all(app.join("content/csgo_addons/mcp_test")).unwrap();
        let tpl = app.join("content/csgo_addons/addon_template/maps");
        std::fs::create_dir_all(&tpl).unwrap();
        std::fs::copy(Tree::fixture("probe.vmap"), tpl.join("xxx_mapname_xxx.vmap")).unwrap();
        Tree { root, app }
    }
    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/vmap").join(name)
    }
    fn addon(&self) -> PathBuf {
        self.app.join("content/csgo_addons/mcp_test")
    }
    fn template(&self) -> PathBuf {
        self.app.join("content/csgo_addons/addon_template/maps/xxx_mapname_xxx.vmap")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn config(tree: &Tree, discovery: &Path, extra: &[&str]) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let map = format!("z:\\app={}", tree.app.display());
    let discovery = discovery.display().to_string();
    let mut given: Vec<&str> = vec![
        "--deadline-ms", "5000", "--discovery", &discovery, "--project-root", &root, "--path-map", &map,
    ];
    given.extend_from_slice(extra);
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

async fn call(server: &McpServer<HammerState>, name: &str, args: Value) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

/// Like [`call`], but also the structured `next` steps.
async fn call_next(server: &McpServer<HammerState>, name: &str, args: Value) -> (Value, Vec<String>) {
    let out = server.call(name, args).await.unwrap_or_else(|_| panic!("{name} failed"));
    let next = out.next.unwrap_or_default().into_iter().map(|s| s.tool).collect();
    (out.json, next)
}

async fn mock(map_files: Vec<&str>) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![])
        .hammer_map_files(map_files)
        .start()
        .await
        .unwrap()
}

fn first_map_podman(tree: &Tree) -> String {
    let bin = tree.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        "#!/bin/bash\ncase \"$1\" in\n  ps) printf '[{\"Names\":[\"cs2tools\"],\"State\":\"running\"}]' ;;\n  inspect) echo 'mcp_test|null' ;;\n  exec) echo 'vtmsend: creator version 1' ;;\n  *) exit 2 ;;\nesac\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755)).unwrap();
    podman.display().to_string()
}

/// The blocker of the acceptance run: an empty addon, hammer.dll not loaded, no map anywhere.
/// `new_map` writes the template copy and opens it over the first-map vtm route.
#[tokio::test]
async fn new_map_creates_and_opens_in_an_empty_addon_with_hammer_not_loaded() {
    let tree = Tree::new("create-open");
    let mb = mock(vec!["maps/first.vmap"]).await;
    mb.hammer_unloaded();
    let podman = first_map_podman(&tree);
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path, &["--podman", &podman])).unwrap();

    // the status of the empty addon says new_map, not open_map
    let (status, next) = call_next(&server, "session_status", json!({})).await;
    assert_eq!(status["maps"]["total"], json!(0), "{status}");
    assert_eq!(next, vec!["new_map".to_string()], "{status}");

    let payload = tree.app.join("vtm_payload.kv3");
    let engine = {
        let mb = &mb;
        async move {
            let started = std::time::Instant::now();
            while !payload.exists() {
                assert!(started.elapsed() < std::time::Duration::from_secs(10), "no payload was posted");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let text = std::fs::read_to_string(&payload).unwrap();
            assert!(text.contains("_data = \"maps/first.vmap\""), "{text}");
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            mb.hammer_open_asset("maps/first.vmap").expect("the mock opens the map");
        }
    };
    let ((out, next), ()) = tokio::join!(call_next(&server, "new_map", json!({"path": "maps/first.vmap"})), engine);

    let file = tree.addon().join("maps/first.vmap");
    assert_eq!(std::fs::read(&file).unwrap(), std::fs::read(tree.template()).unwrap(), "byte copy of the template");
    assert_eq!(out["created"], json!(true), "{out}");
    assert_eq!(out["opened"], json!(true), "{out}");
    assert_eq!(out["openedVia"], json!("vtm"), "{out}");
    assert_eq!(out["contentPath"], json!("maps/first.vmap"), "{out}");
    assert!(out["document"]["path"].as_str().unwrap().ends_with("maps\\first.vmap"), "{out}");
    assert_eq!(out["template"]["name"], json!("addon_template"), "{out}");
    assert!(out["contents"]["entities"].as_u64().unwrap() > 0, "{out}");
    assert!(out["contents"]["byClass"].is_object() && out["contents"]["spawns"].is_object(), "{out}");
    assert_eq!(next, vec!["apply_layout".to_string(), "save_map".to_string()], "{out}");
    // no temporary file is left behind
    let names: Vec<String> = std::fs::read_dir(tree.addon().join("maps"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, vec!["first.vmap".to_string()], "{names:?}");

    // now the addon has a map: the status points at open_map
    let (status, _next) = call_next(&server, "session_status", json!({})).await;
    assert_eq!(status["maps"]["names"], json!(["maps/first.vmap"]), "{status}");
}

/// A second `new_map` on the same path is refused; `overwrite:true` replaces it and keeps a backup.
#[tokio::test]
async fn new_map_refuses_to_overwrite_unless_asked() {
    let tree = Tree::new("overwrite");
    let mb = mock(vec![]).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path, &[])).unwrap();
    let out = call(&server, "new_map", json!({"path": "maps/a.vmap", "open": false})).await.unwrap();
    assert_eq!(out["created"], json!(true), "{out}");
    assert_eq!(out["opened"], json!(false), "{out}");
    let file = tree.addon().join("maps/a.vmap");
    std::fs::write(&file, b"mine").unwrap();

    let err = call(&server, "new_map", json!({"path": "maps/a.vmap", "open": false})).await.unwrap_err();
    assert_eq!(err.type_, "MapExists", "{err:?}");
    assert_eq!(err.data["useInstead"]["tool"], json!("open_map"), "{err:?}");
    assert_eq!(std::fs::read(&file).unwrap(), b"mine", "the existing file is untouched");

    let out = call(&server, "new_map", json!({"path": "maps/a.vmap", "open": false, "overwrite": true}))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), std::fs::read(tree.template()).unwrap());
    let backup = out["replaced"]["backup"].as_str().expect("a backup path");
    assert_eq!(std::fs::read(backup).unwrap(), b"mine");
}

/// Nothing is written outside the active addon's maps/: bad shapes, and a symlink that leads out.
#[tokio::test]
async fn new_map_refuses_paths_outside_the_addon_maps() {
    let tree = Tree::new("outside");
    let mb = mock(vec![]).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path, &[])).unwrap();
    for path in ["../x.vmap", "maps/../x.vmap", "x.vmap", "/etc/x.vmap", "maps\\x.vmap", "maps/x.txt", "csgo_addons/other/maps/x.vmap"] {
        let err = call(&server, "new_map", json!({"path": path, "open": false})).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{path}: {err:?}");
    }
    // maps/escape -> a directory outside the addon
    let outside = tree.root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(tree.addon().join("maps")).unwrap();
    std::os::unix::fs::symlink(&outside, tree.addon().join("maps/escape")).unwrap();
    let err = call(&server, "new_map", json!({"path": "maps/escape/x.vmap", "open": false})).await.unwrap_err();
    assert_eq!(err.type_, "PathOutsideAddon", "{err:?}");
    assert!(!outside.join("x.vmap").exists(), "nothing was written through the symlink");
    // a template can be a map of the addon only, never a path outside it
    let err = call(&server, "new_map", json!({"path": "maps/y.vmap", "template": "../../addon_template/maps/xxx_mapname_xxx.vmap", "open": false}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments", "{err:?}");
    assert!(!tree.addon().join("maps/y.vmap").exists());
}

/// A template that does not exist (or is not a map) writes nothing.
#[tokio::test]
async fn new_map_with_a_missing_template_writes_nothing() {
    let tree = Tree::new("missing-template");
    let mb = mock(vec![]).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path, &[])).unwrap();
    std::fs::create_dir_all(tree.addon().join("maps")).unwrap();
    std::fs::copy(Tree::fixture("probe.vmap"), tree.addon().join("maps/base.vmap")).unwrap();

    let err = call(&server, "new_map", json!({"path": "maps/n.vmap", "template": "maps/bse.vmap", "open": false}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "AssetNotFound", "{err:?}");
    assert_eq!(err.data["reason"], json!("missing_template"), "{err:?}");
    assert_eq!(err.data["didYouMean"], json!(["maps/base.vmap"]), "{err:?}");
    assert!(!tree.addon().join("maps/n.vmap").exists());

    // a template that is not a readable vmap
    std::fs::write(tree.addon().join("maps/junk.vmap"), b"not a dmx file").unwrap();
    let err = call(&server, "new_map", json!({"path": "maps/n.vmap", "template": "maps/junk.vmap", "open": false}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidMapFile", "{err:?}");
    assert!(!tree.addon().join("maps/n.vmap").exists());

    // `empty` is not offered
    let err = call(&server, "new_map", json!({"path": "maps/n.vmap", "template": "empty", "open": false}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments", "{err:?}");

    // the game's template is gone
    std::fs::remove_file(tree.template()).unwrap();
    let err = call(&server, "new_map", json!({"path": "maps/n.vmap", "open": false})).await.unwrap_err();
    assert_eq!(err.type_, "AssetNotFound", "{err:?}");
    assert_eq!(err.data["template"], json!("addon_template"), "{err:?}");
    assert!(!tree.addon().join("maps/n.vmap").exists());

    // an addon map as the template works, and the copy is exact
    let out = call(&server, "new_map", json!({"path": "maps/sub/copy.vmap", "template": "maps/base.vmap", "open": false}))
        .await
        .unwrap();
    assert_eq!(out["template"]["name"], json!("maps/base.vmap"), "{out}");
    assert_eq!(
        std::fs::read(tree.addon().join("maps/sub/copy.vmap")).unwrap(),
        std::fs::read(tree.addon().join("maps/base.vmap")).unwrap()
    );
}

/// An addon with maps: the status lists them (capped) and points at open_map; names only.
#[tokio::test]
async fn session_status_lists_the_addons_maps_and_points_at_open_map() {
    let tree = Tree::new("status-maps");
    let mb = MockBridge::builder().doc(None).start().await.unwrap();
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path, &[])).unwrap();
    let maps = tree.addon().join("maps");
    std::fs::create_dir_all(maps.join("sub")).unwrap();
    for n in 0..25 {
        std::fs::write(maps.join(format!("m{n:02}.vmap")), b"x").unwrap();
    }
    std::fs::write(maps.join("sub/deep.VMAP"), b"x").unwrap();
    std::fs::write(maps.join("m00.vmap.bak-1"), b"x").unwrap();
    let (status, next) = call_next(&server, "session_status", json!({})).await;
    assert_eq!(status["maps"]["total"], json!(26), "{status}");
    assert_eq!(status["maps"]["names"].as_array().unwrap().len(), 20, "{status}");
    assert_eq!(status["maps"]["capped"], json!(true), "{status}");
    assert_eq!(status["maps"]["names"][0], json!("maps/m00.vmap"), "{status}");
    assert_eq!(next, vec!["open_map".to_string()], "{status}");
}

/// A map that does not exist: open_map, compile_map and run_map all point at new_map.
#[tokio::test]
async fn missing_map_errors_point_at_new_map() {
    let tree = Tree::new("use-instead");
    let mb = mock(vec![]).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path, &[])).unwrap();
    for tool in ["open_map", "compile_map", "run_map"] {
        let err = call(&server, tool, json!({"path": "maps/ghost.vmap"})).await.unwrap_err();
        assert_eq!(err.data.get("useInstead").map(|u| &u["tool"]), Some(&json!("new_map")), "{tool}: {err:?}");
        assert_eq!(err.data["useInstead"]["args"], json!({"path": "maps/ghost.vmap"}), "{tool}: {err:?}");
    }
}

/// REVIEW 1: a case-only duplicate is refused (Hammer under Wine resolves `Foo.vmap` and `foo.vmap`
/// to one file), and the "missing map" errors answer `didYouMean` with the real spelling instead of
/// `useInstead new_map`.
#[tokio::test]
async fn case_only_duplicates_are_refused_and_the_missing_errors_name_the_real_spelling() {
    let tree = Tree::new("case");
    let mb = mock(vec![]).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path, &[])).unwrap();
    std::fs::create_dir_all(tree.addon().join("maps/sub")).unwrap();
    std::fs::copy(Tree::fixture("probe.vmap"), tree.addon().join("maps/foo.vmap")).unwrap();
    std::fs::copy(Tree::fixture("probe.vmap"), tree.addon().join("maps/sub/bar.vmap")).unwrap();

    for (asked, real) in [("maps/Foo.vmap", "maps/foo.vmap"), ("maps/Sub/bar.vmap", "maps/sub/bar.vmap"), ("maps/Sub/new.vmap", "maps/sub/new.vmap")] {
        for overwrite in [false, true] {
            let err = call(&server, "new_map", json!({"path": asked, "open": false, "overwrite": overwrite}))
                .await
                .unwrap_err();
            assert_eq!(err.type_, "MapExists", "{asked}: {err:?}");
            assert_eq!(err.data["didYouMean"], json!([real]), "{asked}: {err:?}");
            assert!(err.data.get("useInstead").is_none(), "{asked}: {err:?}");
        }
    }
    let names: Vec<String> = std::fs::read_dir(tree.addon().join("maps"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.contains(&"foo.vmap".to_string()) && !names.contains(&"Foo.vmap".to_string()), "{names:?}");
    assert!(!tree.addon().join("maps/Sub").exists());

    for tool in ["open_map", "compile_map", "run_map"] {
        let err = call(&server, tool, json!({"path": "maps/Foo.vmap"})).await.unwrap_err();
        assert_eq!(err.data["didYouMean"], json!(["maps/foo.vmap"]), "{tool}: {err:?}");
        assert!(err.data.get("useInstead").is_none(), "{tool}: {err:?}");
    }
}

/// REVIEW 2: `overwrite:true` on the map Hammer has open would leave the editor on the old document
/// (and `save_map` would write it back): refused, nothing written.
#[tokio::test]
async fn overwriting_the_active_document_is_refused() {
    let tree = Tree::new("overwrite-active");
    let mb = mock(vec!["maps/probe.vmap"]).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path, &[])).unwrap();
    std::fs::create_dir_all(tree.addon().join("maps")).unwrap();
    let file = tree.addon().join("maps/probe.vmap");
    std::fs::write(&file, b"the user's map").unwrap();
    for open in [true, false] {
        let err = call(&server, "new_map", json!({"path": "maps/probe.vmap", "overwrite": true, "open": open}))
            .await
            .unwrap_err();
        assert_eq!(err.type_, "MapOpenInHammer", "{err:?}");
        assert_eq!(std::fs::read(&file).unwrap(), b"the user's map");
    }
    let names: Vec<String> = std::fs::read_dir(tree.addon().join("maps")).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(names, vec!["probe.vmap".to_string()], "no backup or temporary: {names:?}");
}

/// REVIEW 3 + 5: a first-map open still pending is refused BEFORE the file is written (no contradicting
/// `useInstead open_map`), and `session_status` says to wait.
#[tokio::test]
async fn a_pending_first_map_open_refuses_new_map_before_writing_and_status_says_wait() {
    let tree = Tree::new("pending");
    let mb = mock(vec![]).await;
    mb.hammer_unloaded();
    let podman = first_map_podman(&tree);
    let mut cfg = config(&tree, &mb.discovery_path, &["--podman", &podman]);
    cfg.tool_budget_ms = 5_500;
    let server = hammer_mcp::build_server(&cfg).unwrap();

    // nobody opens the posted map: the outcome is unknown, the file stays
    let err = call(&server, "new_map", json!({"path": "maps/first.vmap"})).await.unwrap_err();
    assert_eq!(err.type_, "OpenUnconfirmed", "{err:?}");
    assert!(tree.addon().join("maps/first.vmap").exists());

    let err = call(&server, "new_map", json!({"path": "maps/second.vmap"})).await.unwrap_err();
    assert_eq!(err.type_, "OpenPending", "{err:?}");
    assert!(!tree.addon().join("maps/second.vmap").exists(), "nothing was written");
    assert!(err.data.get("useInstead").is_none(), "no contradicting retry hint: {err:?}");
    assert_eq!(err.data["retryWith"], json!("session_status"), "{err:?}");
    // open:false is not blocked by the pending open
    call(&server, "new_map", json!({"path": "maps/third.vmap", "open": false})).await.unwrap();

    let (status, next) = call_next(&server, "session_status", json!({})).await;
    assert_eq!(status["pendingOpen"]["path"], json!("maps/first.vmap"), "{status}");
    assert_eq!(next, vec!["session_status".to_string()], "{status}");
}

/// REVIEW 4: the template's `.vpost` (and its compiled twin) and soundscape events are copied
/// into the addon, nothing is replaced, nothing is written into the template addon, and the
/// result lists copied/missing references.
#[tokio::test]
async fn template_assets_are_copied_without_clobbering_and_reported() {
    let tree = Tree::new("assets");
    let mb = mock(vec![]).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path, &[])).unwrap();
    let post = "lighting/postprocessing/basepostprocess_filmic.vpost"; // referenced by the fixture
    let tpl_content = tree.app.join("content/csgo_addons/addon_template");
    let tpl_game = tree.app.join("game/csgo_addons/addon_template");
    std::fs::create_dir_all(tpl_content.join("lighting/postprocessing")).unwrap();
    std::fs::write(tpl_content.join(post), b"vpost source").unwrap();
    std::fs::create_dir_all(tpl_game.join("lighting/postprocessing")).unwrap();
    std::fs::write(tpl_game.join(format!("{post}_c")), b"vpost compiled").unwrap();
    // a soundscape whose name appears in the template map (same-length patch of a string)
    let old = b"materials/tools/toolsnodraw.vmat";
    let name = format!("{:_<width$}", "ambient_zz.outdoors", width = old.len());
    let mut vmap = std::fs::read(tree.template()).unwrap();
    let at = vmap.windows(old.len()).position(|w| w == old).expect("the fixture has the string");
    vmap[at..at + old.len()].copy_from_slice(name.as_bytes());
    std::fs::write(tree.template(), &vmap).unwrap();
    std::fs::create_dir_all(tpl_content.join("soundevents")).unwrap();
    std::fs::write(
        tpl_content.join("soundevents/soundevents_addon.vsndevts"),
        format!("{{\n\t\"{name}\" = \n\t{{\n\t\tbase = \"amb.soundscapeParent.base\"\n\t\ttrack = \"sounds/a.vsnd\"\n\t}}\n}}\n"),
    )
    .unwrap();
    let template_listing = listing(&tree.app.join("content/csgo_addons/addon_template"));
    let game_listing = listing(&tpl_game);
    std::fs::create_dir_all(tree.app.join("game/csgo_addons/mcp_test")).unwrap();

    let out = call(&server, "new_map", json!({"path": "maps/a.vmap", "open": false})).await.unwrap();
    let assets = &out["templateAssets"];
    let refs = |key: &str| -> Vec<String> {
        assets[key].as_array().unwrap().iter().map(|e| e["ref"].to_string()).collect()
    };
    assert!(refs("copied").contains(&format!("\"{post}\"")), "{assets}");
    assert!(refs("copied").contains(&format!("\"{post}_c\"")), "{assets}");
    assert!(refs("copied").contains(&"\"soundevents/soundevents_addon.vsndevts\"".to_string()), "{assets}");
    assert_eq!(std::fs::read(tree.addon().join(post)).unwrap(), b"vpost source");
    assert_eq!(std::fs::read(tree.app.join("game/csgo_addons/mcp_test").join(format!("{post}_c"))).unwrap(), b"vpost compiled");
    assert!(tree.addon().join("soundevents/soundevents_addon.vsndevts").is_file());
    assert!(assets["missing"].as_array().unwrap().iter().any(|e| e["kind"] == json!("sounds")), "{assets}");

    // the second map: nothing is replaced (the user's edits to the addon's copies survive)
    std::fs::write(tree.addon().join(post), b"edited").unwrap();
    std::fs::write(tree.addon().join("soundevents/soundevents_addon.vsndevts"), b"mine").unwrap();
    let out = call(&server, "new_map", json!({"path": "maps/b.vmap", "open": false})).await.unwrap();
    let assets = &out["templateAssets"];
    assert_eq!(std::fs::read(tree.addon().join(post)).unwrap(), b"edited");
    assert_eq!(std::fs::read(tree.addon().join("soundevents/soundevents_addon.vsndevts")).unwrap(), b"mine");
    assert!(assets["copied"].as_array().unwrap().is_empty(), "{assets}");
    assert!(assets["missing"].as_array().unwrap().iter().any(|e| e["kind"] == json!("soundscape")), "{assets}");
    assert!(assets["alreadyPresent"].as_array().unwrap().iter().any(|e| e["ref"] == json!(post)), "{assets}");

    // the template addon was never written
    assert_eq!(listing(&tree.app.join("content/csgo_addons/addon_template")), template_listing);
    assert_eq!(listing(&tpl_game), game_listing);
    // an addon-map template reports no template assets
    let out = call(&server, "new_map", json!({"path": "maps/c.vmap", "template": "maps/a.vmap", "open": false})).await.unwrap();
    assert!(out.get("templateAssets").is_none(), "{out}");
}

fn listing(dir: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let meta = e.metadata().unwrap();
            if meta.is_dir() {
                stack.push(e.path());
            } else {
                out.push((e.path().display().to_string(), meta.len()));
            }
        }
    }
    out.sort();
    out
}

/// REVIEW 7 + 8: `write_atomic_bytes_new` never replaces; the selected addon `addon_template` is refused.
#[tokio::test]
async fn nothing_is_replaced_and_the_template_addon_is_not_writable() {
    let tree = Tree::new("noclobber");
    let file = tree.root.join("x.bin");
    hammer_mcp::vmdl::fsutil::write_atomic_bytes_new(&file, b"one").unwrap();
    let err = hammer_mcp::vmdl::fsutil::write_atomic_bytes_new(&file, b"two").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&file).unwrap(), b"one");
    let leftovers: Vec<_> = std::fs::read_dir(&tree.root).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains(".tmp-")).collect();
    assert!(leftovers.is_empty());

    let mb = mock(vec![]).await;
    let server = hammer_mcp::build_server(&config(&tree, &mb.discovery_path, &[])).unwrap();
    call(&server, "set_addon", json!({"addon": "addon_template"})).await.unwrap();
    let before = listing(&tree.app.join("content/csgo_addons/addon_template"));
    let err = call(&server, "new_map", json!({"path": "maps/x.vmap", "open": false})).await.unwrap_err();
    assert!(err.message.contains("never writes into it"), "{err:?}");
    assert_eq!(listing(&tree.app.join("content/csgo_addons/addon_template")), before);
}

/// The live nodes the mock should hold for the template file `path`: the world and one node per
/// entity/mesh with the file's own node ids (a freshly opened copy has the file's ids).
fn live_nodes_of(path: &Path) -> Vec<cs2_mcp_common::mock::MockNode> {
    use cs2_mcp_common::mock::MockNode;
    let doc = hammer_mcp::vmap::load(path).unwrap();
    let mut nodes = vec![MockNode::world()];
    for e in doc.entities() {
        if let ("CMapEntity", Some(id)) = (e.node_class.as_str(), e.node_id) {
            nodes.push(MockNode::entity(id, e.classname.clone(), [0.0; 3], Default::default()));
        }
    }
    for m in doc.meshes() {
        let mut node = MockNode::entity(m.id.unwrap(), "", [0.0; 3], Default::default());
        node.class = "mesh".into();
        nodes.push(node);
    }
    nodes
}

async fn clear_server(tree: &Tree, map: &str) -> (MockBridge, McpServer<HammerState>) {
    let mb = MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(live_nodes_of(&tree.template()))
        .hammer_map_files(vec![map])
        .start()
        .await
        .unwrap();
    let server = hammer_mcp::build_server(&config(tree, &mb.discovery_path, &[])).unwrap();
    (mb, server)
}

fn classes_of(mb: &MockBridge) -> std::collections::BTreeMap<String, usize> {
    let mut by: std::collections::BTreeMap<String, usize> = Default::default();
    for n in mb.hammer_nodes() {
        let class = if n.class == "mesh" { "mesh".to_string() } else { n.classname.clone() };
        *by.entry(class).or_default() += 1;
    }
    by
}

#[tokio::test]
async fn clear_gameplay_removes_meshes_and_spawns_and_keeps_the_environment() {
    let tree = Tree::new("clear-gameplay");
    let (mb, server) = clear_server(&tree, "maps/a.vmap").await;
    let before = classes_of(&mb);
    assert!(before["mesh"] > 0 && before["info_player_terrorist"] > 0, "{before:?}");

    let (out, next) = call_next(&server, "new_map", json!({"path": "maps/a.vmap", "clear": "gameplay"})).await;
    let clear = &out["clear"];
    assert_eq!(clear["mode"], json!("gameplay"), "{out}");
    assert!(clear.get("error").is_none(), "{clear}");
    assert_eq!(clear["undoGroups"], json!(1), "{clear}");
    let removed = clear["removed"]["byClass"].as_object().unwrap();
    for gone in ["mesh", "info_player_terrorist", "info_player_counterterrorist", "func_buyzone", "func_bomb_target", "point_worldtext", "info_target"] {
        assert!(removed.contains_key(gone), "{gone} should be removed: {clear}");
    }
    let after = classes_of(&mb);
    for env in ["worldspawn", "env_sky", "light_environment", "env_cubemap_fog", "post_processing_volume", "info_map_parameters"] {
        assert_eq!(after.get(env), before.get(env), "{env} must stay: {after:?}");
    }
    for removed in ["mesh", "info_player_terrorist", "func_buyzone", "point_worldtext"] {
        assert!(!after.contains_key(removed), "{removed} is gone: {after:?}");
    }
    // the match-flow cameras are kept and reported as kept
    assert_eq!(after.get("end_of_match"), before.get("end_of_match"));
    assert!(clear["kept"]["byClass"].as_object().unwrap().contains_key("env_sky"), "{clear}");
    // one request, so one undo group; the call still points at apply_layout then save_map
    assert_eq!(mb.method_calls("node.delete").len(), 1);
    assert_eq!(next, vec!["apply_layout".to_string(), "save_map".to_string()], "{out}");
    // the edits are unsaved edits
    let undo = call(&server, "undo_last", json!({})).await.unwrap();
    assert_eq!(undo["undone"], json!(true), "{undo}");
    assert_eq!(undo["undoName"], json!("MCP: delete nodes"), "{undo}");
    assert!(classes_of(&mb)["mesh"] > 0, "one undo_last brings the template's meshes back");
}

#[tokio::test]
async fn clear_is_skipped_when_the_active_document_is_not_the_one_just_opened() {
    let tree = Tree::new("clear-switched");
    // how many doc.active reads the open itself makes
    let (mb, server) = clear_server(&tree, "maps/a.vmap").await;
    call(&server, "new_map", json!({"path": "maps/a.vmap"})).await.unwrap();
    let reads = mb.method_calls("doc.active").len() as u64;
    let tree = Tree::new("clear-switched2");
    let (mb, server) = clear_server(&tree, "maps/a.vmap").await;
    // the document identity switches right after the open's last read: the clear's own read sees it
    mb.set_hammer_doc_switch_after(Some(reads));
    let n = mb.hammer_nodes().len();
    let out = call(&server, "new_map", json!({"path": "maps/a.vmap", "clear": "gameplay"})).await.unwrap();
    assert!(out["clear"]["skipped"].as_str().unwrap().contains("epoch"), "{out}");
    assert_eq!(out["clear"]["removed"]["total"], json!(0));
    assert_eq!(mb.hammer_nodes().len(), n, "nothing was deleted");
    assert!(mb.method_calls("node.delete").is_empty());
}

#[tokio::test]
async fn clear_reports_what_was_planned_and_what_was_removed_separately() {
    let tree = Tree::new("clear-planned");
    let (_mb, server) = clear_server(&tree, "maps/a.vmap").await;
    let out = call(&server, "new_map", json!({"path": "maps/a.vmap", "clear": "gameplay"})).await.unwrap();
    assert_eq!(out["clear"]["planned"]["total"], out["clear"]["removed"]["total"], "{out}");
    assert_eq!(out["clear"]["deleted"], out["clear"]["removed"]["total"], "{out}");
}

#[tokio::test]
async fn clear_all_keeps_only_worldspawn() {
    let tree = Tree::new("clear-all");
    let (mb, server) = clear_server(&tree, "maps/a.vmap").await;
    let out = call(&server, "new_map", json!({"path": "maps/a.vmap", "clear": "all"})).await.unwrap();
    assert_eq!(out["clear"]["mode"], json!("all"), "{out}");
    assert_eq!(out["clear"]["kept"]["total"], json!(0), "{out}");
    let after = classes_of(&mb);
    assert_eq!(after.keys().collect::<Vec<_>>(), vec!["worldspawn"], "{after:?}");
}

#[tokio::test]
async fn clear_none_default_and_bad_values_and_open_false() {
    let tree = Tree::new("clear-none");
    let (mb, server) = clear_server(&tree, "maps/a.vmap").await;
    let n = mb.hammer_nodes().len();
    let out = call(&server, "new_map", json!({"path": "maps/a.vmap"})).await.unwrap();
    assert!(out.get("clear").is_none(), "{out}");
    assert_eq!(mb.hammer_nodes().len(), n);
    assert!(mb.method_calls("node.delete").is_empty());

    for bad in [json!({"path": "maps/b.vmap", "clear": "everything"}), json!({"path": "maps/b.vmap", "clear": 1}), json!({"path": "maps/b.vmap", "clear": "all", "open": false})] {
        let e = call(&server, "new_map", bad.clone()).await.unwrap_err();
        assert_eq!(e.name, "InvalidParams", "{bad}: {e:?}");
    }
    assert!(!tree.addon().join("maps/b.vmap").exists(), "refused before anything was written");
}

#[test]
fn the_class_lists_fit_the_real_templates_contents() {
    use hammer_mcp::tools::new_map::{removed_by_gameplay, ENVIRONMENT_CLASSES};
    // what Valve's addon_template holds (read from the 2000933 file with the crate's own reader)
    let removed = [
        "point_worldtext", "info_player_terrorist", "info_player_counterterrorist", "info_player_start",
        "func_buyzone", "func_bomb_target", "point_nav_walkable",
    ];
    let kept = [
        "env_sky", "light_environment", "env_cubemap_fog", "post_processing_volume", "info_map_parameters",
        "env_soundscape", "env_combined_light_probe_volume", "cs_minimap_boundary", "end_of_match",
        "team_select", "terrorist_team_intro", "counterterrorist_team_intro", "worldspawn",
    ];
    for class in removed {
        assert!(removed_by_gameplay(class), "{class}");
    }
    for class in kept {
        assert!(!removed_by_gameplay(class), "{class}");
    }
    for class in ENVIRONMENT_CLASSES {
        assert!(!removed_by_gameplay(class), "{class} is environment");
    }
    // and, when this machine has the real template, every class in it falls on the documented side
    // CS2_MCP_PROJECT_ROOT, else the first ancestor of this crate that has the template; absent
    // on a machine without the game install: skipped
    const TEMPLATE: &str = "workshop/run/app/content/csgo_addons/addon_template/maps/xxx_mapname_xxx.vmap";
    let mut roots: Vec<std::path::PathBuf> = std::env::var_os("CS2_MCP_PROJECT_ROOT").map(Into::into).into_iter().collect();
    roots.extend(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(Into::into));
    let Some(real) = roots.into_iter().map(|r| r.join(TEMPLATE)).find(|p| p.is_file()) else {
        println!("skipped: the real addon_template is not on this machine");
        return;
    };
    if let Ok(doc) = hammer_mcp::vmap::load(&real) {
        for e in doc.entities().iter().filter(|e| e.node_class == "CMapEntity") {
            let c = e.classname.as_str();
            assert!(removed.contains(&c) || kept.contains(&c), "template class {c} is not classified in this test");
        }
    }
}
