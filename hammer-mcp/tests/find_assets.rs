//! `find_assets` on a synthetic app tree: game paks (csgo, core) with materials/particles/sounds/
//! textures/models, an addon with compiled and source-only files. Materials report `renders` from
//! their texture lists (the resolver `report_ready` uses).
#![allow(clippy::result_large_err)]

mod common;

use common::{kv_object, kv_text, model_vmdl_c, vmat_c, vmat_c_claiming, write_vpk, BuildFile};
use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::tools::{Budget, CallCtx, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn load(path: &str, bytes: &[u8]) -> BuildFile {
    BuildFile::loaded(path, bytes, b"", false)
}

fn tree(tag: &str) -> Tree {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "find-assets-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let app = root.join("workshop/run/app");
    std::fs::create_dir_all(&app).unwrap();
    let model = model_vmdl_c(&[("DATA", kv_object(vec![("m_name", kv_text("models/x.vmdl"))]))]);
    write_vpk(
        &app.join("game/csgo"),
        "pak01",
        2,
        &[
            load("materials/dev/good.vmat_c", &vmat_c(&["materials/dev/good_color.vtex"])),
            load("materials/dev/magenta.vmat_c", &vmat_c(&["materials/dev/nowhere_color.vtex"])),
            load("materials/dev/plain.vmat_c", &vmat_c(&[])),
            load("materials/dev/garbage.vmat_c", b"not a resource"),
            load("materials/dev/cut.vmat_c", &vmat_c_claiming(&["materials/dev/good_color.vtex"], 4)),
            BuildFile::preload_only("materials/dev/good_color.vtex_c", b""),
            BuildFile::preload_only("particles/fire/flame.vpcf_c", b""),
            BuildFile::preload_only("sounds/ui/click.vsnd_c", b""),
            load("models/props/box.vmdl_c", &model),
        ],
    );
    write_vpk(
        &app.join("game/core"),
        "pak01",
        1,
        &[
            load("materials/dev/good.vmat_c", &vmat_c(&[])),
            load("materials/core_only.vmat_c", &vmat_c(&[])),
        ],
    );
    let game = app.join("game/csgo_addons/mcp_test/materials/mine");
    std::fs::create_dir_all(&game).unwrap();
    std::fs::write(game.join("Wall.vmat_c"), vmat_c(&["materials/mine/wall_color.vtex"])).unwrap();
    let content = app.join("content/csgo_addons/mcp_test/materials/mine");
    std::fs::create_dir_all(&content).unwrap();
    std::fs::write(content.join("wall.vmat"), "x").unwrap();
    std::fs::write(content.join("draft.vmat"), "x").unwrap();
    Tree { root, app }
}

fn state(tree: &Tree) -> HammerState {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let given: Vec<&str> = vec!["--deadline-ms", "5000", "--tool-budget-ms", "45000", "--project-root", &root];
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    let _ = &tree.app;
    HammerState::from_config(&W::parse_from(argv).c).unwrap()
}

async fn find(state: &HammerState, args: Value) -> Result<Value, ToolError> {
    let ctx = CallCtx { budget: Budget::new(45_000, 5_000) };
    hammer_mcp::tools::assets::find_assets(state.clone(), ctx, args).await.map(|o| o.json)
}

fn hit<'a>(out: &'a Value, path: &str) -> &'a Value {
    out["hits"].as_array().unwrap().iter().find(|h| h["path"] == path).unwrap_or_else(|| panic!("{path} not in {out}"))
}

#[tokio::test]
async fn materials_report_whether_they_render() {
    let t = tree("renders");
    let s = state(&t);
    let out = find(&s, json!({"kind": "material", "limit": 200})).await.unwrap();
    assert_eq!(out["total"], 8, "{out}"); // good, magenta, plain, garbage, cut, core_only, mine/wall, mine/draft
    assert_eq!(hit(&out, "materials/dev/good.vmat")["renders"], "ok");
    assert_eq!(hit(&out, "materials/dev/good.vmat")["source"], "csgo");
    assert_eq!(hit(&out, "materials/dev/plain.vmat")["renders"], "ok");
    let magenta = hit(&out, "materials/dev/magenta.vmat");
    assert_eq!(magenta["renders"], "missing_textures");
    assert_eq!(magenta["missingTextures"], json!(["materials/dev/nowhere_color.vtex"]));
    // a truncated texture list is never "ok": the one texture it did list is present, the rest unseen
    let cut = hit(&out, "materials/dev/cut.vmat");
    assert_eq!(cut["renders"], "unknown", "{cut}");
    assert!(cut["reason"].as_str().unwrap().contains("malformed at entry 1 of 4"), "{cut}");
    let garbage = hit(&out, "materials/dev/garbage.vmat");
    assert_eq!(garbage["renders"], "unknown");
    assert!(garbage["reason"].as_str().unwrap().contains("not a valid resource"), "{garbage}");
    // addon: compiled Wall.vmat_c (mixed case on disk) with a texture nobody has
    let wall = hit(&out, "materials/mine/wall.vmat");
    assert_eq!((wall["source"].as_str(), wall["compiled"].as_bool()), (Some("addon"), Some(true)));
    assert_eq!(wall["renders"], "missing_textures");
    // source only: never "ok"
    let draft = hit(&out, "materials/mine/draft.vmat");
    assert_eq!((draft["source"].as_str(), draft["compiled"].as_bool()), (Some("content"), Some(false)));
    assert_eq!(draft["renders"], "unknown");
    assert_eq!(hit(&out, "materials/core_only.vmat")["source"], "core");
    // the first ok material is the next step
    let step = next_of(&s, json!({"kind": "material", "query": "good"})).await.remove(0);
    assert_eq!(step.tool, "set_mesh_material");
    assert!(step.args.is_none(), "no args: the tool needs a nodeID this one does not have");
    assert!(step.why.contains("materials/dev/good.vmat"), "{}", step.why);
}

async fn next_of(s: &HammerState, args: Value) -> Vec<cs2_mcp_common::tools::NextStep> {
    let ctx = CallCtx { budget: Budget::new(45_000, 5_000) };
    hammer_mcp::tools::assets::find_assets(s.clone(), ctx, args).await.unwrap().next.unwrap_or_default()
}

/// Every `next` step names a registered tool and its args (when any) satisfy that tool's schema:
/// known keys only, every required key present, strings where the schema says string.
fn assert_step_valid(step: &cs2_mcp_common::tools::NextStep) {
    let registry = hammer_mcp::build_registry();
    let spec = registry.spec(&step.tool).unwrap_or_else(|| panic!("{} is not a tool", step.tool));
    let props = spec.input_schema["properties"].as_object().cloned().unwrap_or_default();
    let args = step.args.clone().unwrap_or(json!({}));
    let args = args.as_object().expect("args are an object");
    for (key, value) in args {
        if props.is_empty() {
            continue; // a tool with a free-form schema (model_compile): nothing to check against
        }
        let prop = props.get(key).unwrap_or_else(|| panic!("{}: {key} is not an argument", step.tool));
        if prop["type"] == "string" {
            assert!(value.is_string(), "{}.{key}", step.tool);
        }
    }
    if step.args.is_some() {
        for required in spec.input_schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            assert!(args.contains_key(required), "{}: required {required} missing", step.tool);
        }
    }
}

#[tokio::test]
async fn query_prefix_source_and_pagination() {
    let t = tree("page");
    let s = state(&t);
    let all = find(&s, json!({"kind": "material", "limit": 200})).await.unwrap();
    let paths: Vec<&str> = all["hits"].as_array().unwrap().iter().map(|h| h["path"].as_str().unwrap()).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
    // pages of 3 reproduce the whole list, the cursor ends, total stays the match count
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut args = json!({"kind": "material", "limit": 3});
        if let Some(c) = &cursor {
            args["cursor"] = json!(c);
        }
        let out = find(&s, args).await.unwrap();
        assert_eq!(out["total"], 8);
        seen.extend(out["hits"].as_array().unwrap().iter().map(|h| h["path"].as_str().unwrap().to_string()));
        match out["nextCursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(seen, paths);
    let q = find(&s, json!({"kind": "material", "query": "MINE/"})).await.unwrap();
    assert_eq!(q["total"], 2, "{q}");
    let p = find(&s, json!({"kind": "material", "prefix": "materials\\dev\\"})).await.unwrap();
    assert_eq!(p["total"], 5, "{p}");
    let addon = find(&s, json!({"kind": "material", "source": "addon"})).await.unwrap();
    assert_eq!(addon["total"], 2, "{addon}");
    let game = find(&s, json!({"kind": "material", "source": "game"})).await.unwrap();
    assert_eq!(game["total"], 6, "{game}");
}

#[tokio::test]
async fn other_kinds_and_models() {
    let t = tree("kinds");
    let s = state(&t);
    let particle = find(&s, json!({"kind": "particle"})).await.unwrap();
    assert_eq!(particle["hits"][0]["path"], "particles/fire/flame.vpcf");
    assert_eq!(particle["hits"][0]["compiled"], true);
    assert!(particle["hits"][0].get("renders").is_none());
    assert_eq!(find(&s, json!({"kind": "sound"})).await.unwrap()["hits"][0]["path"], "sounds/ui/click.vsnd");
    assert_eq!(find(&s, json!({"kind": "texture"})).await.unwrap()["hits"][0]["path"], "materials/dev/good_color.vtex");
    let model = find(&s, json!({"kind": "model"})).await.unwrap();
    assert_eq!(model["hits"][0]["path"], "models/props/box.vmdl");
    assert_eq!(model["hits"][0]["source"], "csgo");
    let out = hammer_mcp::tools::assets::find_assets(s.clone(), CallCtx { budget: Budget::new(45_000, 5_000) }, json!({"kind": "model"})).await.unwrap();
    assert_eq!(out.next.unwrap()[0].tool, "get_model_info");
    // no match: total 0 with a note, never an error
    let none = find(&s, json!({"kind": "particle", "query": "zzz"})).await.unwrap();
    assert_eq!(none["total"], 0);
    assert!(none["note"].as_str().unwrap().contains("no asset"));
}

#[tokio::test]
async fn bad_arguments_are_refused() {
    let t = tree("bad");
    let s = state(&t);
    for args in [
        json!({}),
        json!({"kind": "shader"}),
        json!({"kind": "material", "limit": 0}),
        json!({"kind": "material", "limit": 201}),
        json!({"kind": "material", "source": "mars"}),
        json!({"kind": "material", "query": ""}),
        json!({"kind": "material", "bogus": 1}),
    ] {
        let err = find(&s, args.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{args}: {err:?}");
    }
}

#[tokio::test]
async fn a_missing_game_pak_is_an_error_and_an_addon_only_search_still_works() {
    let t = tree("nopak");
    std::fs::remove_file(t.app.join("game/core/pak01_dir.vpk")).unwrap();
    let s = state(&t);
    let err = find(&s, json!({"kind": "material"})).await.unwrap_err();
    assert_eq!(err.type_, "VpkMissing", "{err:?}");
    let addon = find(&s, json!({"kind": "material", "source": "addon"})).await.unwrap();
    assert_eq!(addon["total"], 2);
    // with the pak gone, no addon material can claim "ok"
    assert_ne!(hit(&addon, "materials/mine/wall.vmat")["renders"], "ok");
}

/// The real install (needs `workshop/run/app`): timings of the first (index build) and a cached
/// call, and the `renders` of a few dev materials. Run with `--ignored --nocapture`.
#[tokio::test]
#[ignore]
async fn real_install_timings_and_material_renders() {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while !dir.join("workshop/run/app/game/csgo/pak01_dir.vpk").is_file() {
        assert!(dir.pop(), "no project root above the manifest dir");
    }
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let r = dir.display().to_string();
    let given: Vec<&str> = vec!["--deadline-ms", "5000", "--project-root", &r];
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    let s = HammerState::from_config(&W::parse_from(argv).c).unwrap();
    for (label, args) in [
        ("first material", json!({"kind": "material", "query": "dev/reflectivity_30"})),
        ("cached material", json!({"kind": "material", "query": "dev/reflectivity", "limit": 200})),
        ("all materials page", json!({"kind": "material"})),
        ("particle", json!({"kind": "particle", "query": "explosion"})),
        ("sound", json!({"kind": "sound", "query": "ui/"})),
        ("texture", json!({"kind": "texture", "query": "cobblestone"})),
        ("model first", json!({"kind": "model", "query": "car_a"})),
        ("model cached", json!({"kind": "model", "query": "utility_truck"})),
    ] {
        let t = std::time::Instant::now();
        let out = find(&s, args).await.unwrap();
        println!("{label}: {:?} total={} returned={}", t.elapsed(), out["total"], out["returned"]);
        if label == "cached material" {
            let counts = out["hits"].as_array().unwrap().iter().fold(std::collections::BTreeMap::new(), |mut m, h| {
                *m.entry(h["renders"].as_str().unwrap_or("-").to_string()).or_insert(0) += 1;
                m
            });
            println!("renders: {counts:?}");
        }
    }
}

#[tokio::test]
async fn next_steps_validate_and_models_pick_a_compiled_hit() {
    let t = tree("next");
    // an addon model that exists only as source sorts before the pak's box
    let src = t.app.join("content/csgo_addons/mcp_test/models/props");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("aaa_draft.vmdl"), "x").unwrap();
    let s = state(&t);
    let steps = next_of(&s, json!({"kind": "model"})).await;
    assert_eq!(steps[0].tool, "get_model_info");
    assert_eq!(steps[0].args.as_ref().unwrap()["model"], "models/props/box.vmdl", "skips the uncompiled hit");
    for step in &steps {
        assert_step_valid(step);
    }
    // only uncompiled hits: model_compile on the first
    let steps = next_of(&s, json!({"kind": "model", "source": "addon"})).await;
    assert_eq!(steps[0].tool, "model_compile");
    assert_eq!(steps[0].args.as_ref().unwrap()["path"], "models/props/aaa_draft.vmdl");
    assert_step_valid(&steps[0]);
    for step in next_of(&s, json!({"kind": "material"})).await {
        assert_step_valid(&step);
    }
}

#[tokio::test]
async fn models_reuse_the_cached_catalog_and_still_see_new_addon_files() {
    let t = tree("cache");
    let s = state(&t);
    let first = find(&s, json!({"kind": "model"})).await.unwrap();
    assert_eq!(first["total"], 1);
    let cached = s.model_catalog.lock().unwrap().clone().expect("the catalog was built");
    let dir = t.app.join("game/csgo_addons/mcp_test/models/props");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Fresh.vmdl_c"), model_vmdl_c(&[("DATA", kv_object(vec![("m_name", kv_text("f"))]))])).unwrap();
    let second = find(&s, json!({"kind": "model"})).await.unwrap();
    assert_eq!(second["total"], 2, "{second}");
    assert_eq!(hit(&second, "models/props/fresh.vmdl")["source"], "addon");
    let after = s.model_catalog.lock().unwrap().clone().unwrap();
    assert!(std::sync::Arc::ptr_eq(&cached, &after), "the catalog was rebuilt or replaced");
}

#[tokio::test]
async fn the_source_filter_never_contradicts_the_labels() {
    let t = tree("scope");
    let s = state(&t);
    let addon = find(&s, json!({"kind": "material", "source": "addon"})).await.unwrap();
    let reason = hit(&addon, "materials/mine/draft.vmat")["reason"].as_str().unwrap().to_string();
    assert!(reason.contains("not compiled in the addon") && !reason.contains("anywhere"), "{reason}");
    let game = find(&s, json!({"kind": "material", "source": "game", "query": "good"})).await.unwrap();
    assert_eq!(hit(&game, "materials/dev/good.vmat")["source"], "csgo");
    // an addon override of a pak material: source=game checks and labels the pak copy
    let over = t.app.join("game/csgo_addons/mcp_test/materials/dev");
    std::fs::create_dir_all(&over).unwrap();
    std::fs::write(over.join("magenta.vmat_c"), vmat_c(&[])).unwrap();
    let game = find(&s, json!({"kind": "material", "source": "game", "query": "magenta"})).await.unwrap();
    let h = hit(&game, "materials/dev/magenta.vmat");
    assert_eq!((h["source"].as_str(), h["renders"].as_str()), (Some("csgo"), Some("missing_textures")), "{h}");
    let all = find(&s, json!({"kind": "material", "source": "all", "query": "magenta"})).await.unwrap();
    assert_eq!(hit(&all, "materials/dev/magenta.vmat")["source"], "addon");
    assert_eq!(hit(&all, "materials/dev/magenta.vmat")["renders"], "ok");
}
