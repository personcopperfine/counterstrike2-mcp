//! Model catalog tests (WP-V): [`ModelCatalog`] and the `list_models` tool against a fixture
//! app tree under `CARGO_TARGET_TMPDIR` (never /tmp) with a **synthetic** small VPK pair
//! (shared builder in `tests/common`) and real files in the two addon directories.
//!
//! Covered: source precedence and the sources merge, canonical-name normalization,
//! `query`/`prefix`/`source`/`compiledOnly` filters, paging and its bounds, unknown arguments,
//! the missing-game-VPK error (naming the file, both paks), the absent-addon note instead of
//! an empty success, the depth-bound walk, symlinked entries not followed, and the
//! lookup-miss → addon-only rescan → cached-catalog flow. No bridge is involved: every tool
//! call lands straight on the filesystem.
#![allow(clippy::result_large_err)]

mod common;

use common::{write_vpk, BuildFile};
use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::tools::{Budget, CallCtx};
use hammer_mcp::state::HammerState;
use hammer_mcp::tools::model_catalog::{
    normalize_model_path, CatalogError, ModelCatalog, ModelSource,
};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A temp app tree laid out like the real one (`workshop/run/app`), with the addon `mcp_test`.
struct Tree {
    root: PathBuf,
    app: PathBuf,
}
impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "model-catalog-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("tree");
        let app = root.join("workshop/run/app");
        Tree { root, app }
    }

    /// The game VPKs: csgo (v2, with a sibling-data model, a preload-only one, an in-dir one,
    /// plus files that must NOT make the catalog) and core (v1).
    fn build_vpks(&self) {
        write_vpk(
            &self.app.join("game/csgo"),
            "pak01",
            2,
            &[
                BuildFile::loaded("models/props/dust/box.vmdl_c", b"boxdata", b"boxpre", false),
                BuildFile::preload_only("models/props/dust/pipes.vmdl_c", b"pipes-preload"),
                BuildFile::loaded("models/deco/sign.vmdl_c", b"sign-data", b"", true),
                // not a model (wrong extension) and not under models/: both stay out
                BuildFile::preload_only("models/dev/etc.vtex_c", b""),
                BuildFile::preload_only("materials/table.vmdl_c", b""),
            ],
        );
        write_vpk(
            &self.app.join("game/core"),
            "pak01",
            1,
            &[BuildFile::loaded(
                "models/dev/axis.vmdl_c",
                b"axisdata",
                b"axispre",
                false,
            )],
        );
    }

    /// The compiled + uncompiled addon files (the paths are the point, not the bytes).
    fn build_addon(&self) {
        let game = self
            .app
            .join("game/csgo_addons/mcp_test/models/props/chair.vmdl_c");
        std::fs::create_dir_all(game.parent().unwrap()).expect("addon game dir");
        std::fs::write(&game, b"chairc\n").expect("compiled addon model");
        let content = self.app.join("content/csgo_addons/mcp_test/models/props");
        std::fs::create_dir_all(&content).expect("addon content dir");
        std::fs::write(content.join("chair.vmdl"), b"chairsrc\n").expect("addon source");
        std::fs::write(content.join("unbuilt.vmdl"), b"unbuilt\n").expect("uncompiled source");
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A complete default fixture: the two game VPKs + both addon parts.
fn full_tree(tag: &str) -> Tree {
    let tree = Tree::new(tag);
    tree.build_vpks();
    tree.build_addon();
    tree
}

fn config(tree: &Tree) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let given: Vec<&str> = vec![
        "--deadline-ms",
        "5000",
        "--tool-budget-ms",
        "45000",
        "--project-root",
        &root,
    ];
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

fn ctx() -> CallCtx {
    CallCtx {
        budget: Budget::new(45_000, 5_000),
    }
}

/// The fixture's six canonical model paths, sorted.
const SIX: [&str; 6] = [
    "models/deco/sign.vmdl",
    "models/dev/axis.vmdl",
    "models/props/chair.vmdl",
    "models/props/dust/box.vmdl",
    "models/props/dust/pipes.vmdl",
    "models/props/unbuilt.vmdl",
];

fn paths_of(catalog: &ModelCatalog) -> Vec<&str> {
    catalog.entries().map(|entry| entry.path.as_str()).collect()
}

// -------------------------------------------------------------------------------------------
// ModelCatalog API
// -------------------------------------------------------------------------------------------

#[test]
fn catalog_precedence_merge_and_compilation() {
    let tree = full_tree("precedence");
    let catalog = ModelCatalog::build(&tree.app, "mcp_test").expect("build");

    assert_eq!(paths_of(&catalog), SIX, "sorted canonical paths");
    let chair = catalog.get("models/props/chair.vmdl").expect("chair");
    assert_eq!(
        chair.sources,
        [
            ModelSource::AddonGame {
                file: PathBuf::from("props/chair.vmdl_c")
            },
            ModelSource::AddonContent {
                file: PathBuf::from("props/chair.vmdl")
            },
        ],
        "sources in precedence order"
    );
    assert!(chair.compiled);
    let unbuilt = catalog.get("models/props/unbuilt.vmdl").unwrap();
    assert_eq!(
        unbuilt.sources,
        [ModelSource::AddonContent {
            file: PathBuf::from("props/unbuilt.vmdl")
        }]
    );
    assert!(
        !unbuilt.compiled,
        "uncompiled source reported compiled:false"
    );
    assert_eq!(
        catalog.get("models/props/dust/box.vmdl").unwrap().sources,
        [ModelSource::Vpk { pak: "csgo" }]
    );
    assert_eq!(
        catalog.get("models/dev/axis.vmdl").unwrap().sources,
        [ModelSource::Vpk { pak: "core" }]
    );
    assert!(
        catalog.get("models/dev/etc.vtex_c").is_none(),
        "the .vtex_c never entered"
    );
    assert!(
        catalog.get("materials/table.vmdl").is_none(),
        "outside models/: not a model"
    );
    assert!(catalog.note().is_none(), "a complete scan notes nothing");
}

#[test]
fn catalog_get_normalizes_the_lookup() {
    let tree = full_tree("normalize");
    let catalog = ModelCatalog::build(&tree.app, "mcp_test").expect("build");
    // case, backslashes and the compiled extension all spell the same path
    assert!(catalog.get("MODELS\\PROPS\\CHAIR.VMDL_C").is_some());
    assert!(
        catalog.get("models/dev/axis.vmdl_c").is_some(),
        ".vmdl_c form"
    );
    assert!(
        catalog.get("  models/props/unbuilt.vmdl_c  ").is_some(),
        "trimmed"
    );
    // a lookup that misses still normalizes: `models/dev/etc.vtex_c` is not a model at all
    assert!(catalog.get("models/dev/etc.vtex_c").is_none());
}

#[test]
fn read_compiled_from_the_addon_then_the_pak() {
    let tree = full_tree("read");
    let catalog = ModelCatalog::build(&tree.app, "mcp_test").expect("build");
    // the addon game file (highest precedence)
    assert_eq!(
        catalog.read_compiled("models/props/chair.vmdl").unwrap(),
        b"chairc\n".to_vec()
    );
    // the VPK route: sibling archive data + its preload bytes
    assert_eq!(
        catalog.read_compiled("models/props/dust/box.vmdl").unwrap(),
        b"boxpreboxdata".to_vec()
    );
    // the in-dir route
    assert_eq!(
        catalog.read_compiled("models/deco/sign.vmdl").unwrap(),
        b"sign-data".to_vec()
    );
    // the core VPK (second in precedence; nothing else carries the model)
    assert_eq!(
        catalog.read_compiled("models/dev/axis.vmdl").unwrap(),
        b"axispreaxisdata".to_vec()
    );
    // a preload-only entry answers its preload bytes alone
    assert_eq!(
        catalog
            .read_compiled("models/props/dust/pipes.vmdl")
            .unwrap(),
        b"pipes-preload".to_vec()
    );
}

/// Finding: an addon recompile of a stock model outranks the game VPK — the entry's `sources`
/// are in precedence order and `read_compiled` walks that order, so the caller reads the
/// addon's bytes, never the stock model's (the "override a stock prop" workflow).
#[test]
fn an_addon_copy_outranks_the_game_vpk() {
    let tree = full_tree("override");
    std::fs::create_dir_all(tree.app.join("game/csgo_addons/mcp_test/models/props/dust"))
        .expect("override dir");
    std::fs::write(
        tree.app
            .join("game/csgo_addons/mcp_test/models/props/dust/box.vmdl_c"),
        b"addon box",
    )
    .expect("override model");
    let catalog = ModelCatalog::build(&tree.app, "mcp_test").expect("build");
    // the override maps onto the VPK's canonical entry: the set of models does not change
    assert_eq!(paths_of(&catalog), SIX);
    let box_entry = catalog.get("models/props/dust/box.vmdl").expect("box");
    assert_eq!(
        box_entry.sources,
        [
            ModelSource::AddonGame {
                file: PathBuf::from("props/dust/box.vmdl_c")
            },
            ModelSource::Vpk { pak: "csgo" },
        ],
        "the addon first, then the game VPK — not the reverse"
    );
    assert!(box_entry.compiled);
    assert_eq!(
        catalog.read_compiled("models/props/dust/box.vmdl").unwrap(),
        b"addon box".to_vec(),
        "the read walks the precedence order: the addon's bytes win"
    );
}

/// Finding: the addon walks take exactly their own source's extension (compiled dir
/// `.vmdl_c`, content dir `.vmdl`) — case-insensitively — and the entry keeps the on-disk
/// spelling so a read opens the file the walk saw.
#[test]
fn the_addon_walks_take_their_own_extensions_and_any_case() {
    let tree = full_tree("extensions");
    let game = tree.app.join("game/csgo_addons/mcp_test/models/props");
    std::fs::create_dir_all(&game).expect("game models dir");
    std::fs::write(game.join("Mixed.VMDL_C"), b"mixed").expect("mixed-case compiled model");
    // a source-file extension in the compiled dir is never mis-labeled as a compiled model
    std::fs::write(game.join("stray.vmdl"), b"stray").expect("stray source");
    // a compiled extension in the content dir is not a source-file model either
    let content = tree.app.join("content/csgo_addons/mcp_test/models/props");
    std::fs::create_dir_all(&content).expect("content models dir");
    std::fs::write(content.join("opaque.vmdl_c"), b"opaque").expect("stray compiled file");

    let catalog = ModelCatalog::build(&tree.app, "mcp_test").expect("build");
    let mixed = catalog
        .get("models/props/mixed.vmdl")
        .expect("the mixed-case file is cataloged");
    assert_eq!(
        mixed.sources,
        [ModelSource::AddonGame {
            file: PathBuf::from("props/Mixed.VMDL_C")
        }],
        "the entry carries the on-disk spelling"
    );
    assert!(mixed.compiled);
    assert_eq!(
        catalog.read_compiled("models/props/mixed.vmdl").unwrap(),
        b"mixed".to_vec(),
        "the read opens the walked path, not one rebuilt from the lowercase canonical"
    );
    assert!(
        catalog.get("models/props/stray.vmdl").is_none(),
        "a .vmdl in the game dir is not a compiled-model source"
    );
    assert!(
        catalog.get("models/props/opaque.vmdl").is_none(),
        "a .vmdl_c in the content dir is not a source-file model"
    );
}

#[test]
fn read_compiled_refuses_uncompiled_and_unknown() {
    let tree = full_tree("refuse");
    let catalog = ModelCatalog::build(&tree.app, "mcp_test").expect("build");
    let error = catalog
        .read_compiled("models/props/unbuilt.vmdl")
        .unwrap_err();
    assert!(error.to_string().contains("no compiled .vmdl_c"), "{error}");
    let error = catalog.read_compiled("models/props/nope.vmdl").unwrap_err();
    assert!(error.to_string().contains("no such model"), "{error}");
}

/// A missing required VPK fails the build and names the file (never a half catalog).
#[test]
fn a_missing_game_vpk_names_the_file() {
    let tree = full_tree("missing-csgo");
    std::fs::remove_file(tree.app.join("game/csgo/pak01_dir.vpk")).unwrap();
    let error = ModelCatalog::build(&tree.app, "mcp_test").unwrap_err();
    let CatalogError::MissingVpk { path } = error else {
        panic!("expected MissingVpk, got {error:?}");
    };
    assert!(path.ends_with("game/csgo/pak01_dir.vpk"), "{path:?}");

    let tree = Tree::new("missing-core");
    tree.build_vpks();
    tree.build_addon();
    std::fs::remove_file(tree.app.join("game/core/pak01_dir.vpk")).unwrap();
    let error = ModelCatalog::build(&tree.app, "mcp_test").unwrap_err();
    let CatalogError::MissingVpk { path } = error else {
        panic!("expected MissingVpk, got {error:?}");
    };
    assert!(path.ends_with("game/core/pak01_dir.vpk"), "{path:?}");
}

/// An absent addon `models` directory is an explicit note, never a silent success or an error
/// (the addon can legitimately be empty).
#[test]
fn an_absent_addon_directory_is_a_note() {
    let tree = Tree::new("no-addon");
    tree.build_vpks();
    let catalog = ModelCatalog::build(&tree.app, "mcp_test").expect("build");
    assert_eq!(paths_of(&catalog).len(), 4, "only VPK sources remain");
    let note = catalog.note().expect("the scan said what it did");
    assert!(note.contains("addon-game"), "{note}");
    assert!(note.contains("does not exist"), "{note}");
}

/// The walk's depth bound: a model file 20 directories deep past the bound is skipped **with
/// a note** (never a hang, never a silent miss). A symlinked model file is never followed.
#[test]
fn the_walk_depth_bound_notes_and_symlinks_are_not_followed() {
    let tree = Tree::new("deep");
    tree.build_vpks();
    let addon_models = tree.app.join("game/csgo_addons/mcp_test/models");
    std::fs::create_dir_all(&addon_models).expect("addon models dir");
    let mut dir = addon_models.join("lvl0");
    for level in 1..20 {
        dir = dir.join(format!("lvl{level}"));
        std::fs::create_dir_all(&dir).expect("deep");
    }
    std::fs::write(dir.join("too_deep.vmdl_c"), b"deep").expect("deep model");
    // a symlinked model file pointing outside the addon tree
    let outside = tree.app.join("outside.vmdl_c");
    std::fs::write(&outside, b"escape").expect("outside");
    std::fs::create_dir_all(tree.app.join("game/csgo_addons/mcp_test/models/props"))
        .expect("symlink parent");
    std::os::unix::fs::symlink(
        &outside,
        tree.app
            .join("game/csgo_addons/mcp_test/models/props/link.vmdl_c"),
    )
    .expect("symlink parent must exist");

    let catalog = ModelCatalog::build(&tree.app, "mcp_test").expect("build");
    assert!(
        !paths_of(&catalog)
            .iter()
            .any(|path| path.contains("too_deep")),
        "the deep file is not cataloged"
    );
    let note = catalog.note().expect("the stop is on the record");
    assert!(note.contains("16-directory bound"), "{note}");
    assert!(
        !paths_of(&catalog).iter().any(|path| path.contains("link")),
        "symlinked models are never followed"
    );
}

// -------------------------------------------------------------------------------------------
// the tool through the state cache (direct handler calls, as tests/entities_file.rs does)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn list_models_defaults_sorted_sources_and_caches() {
    let tree = full_tree("list");
    let state = HammerState::from_config(&config(&tree)).unwrap();
    let out = hammer_mcp::tools::model_catalog::list_models(state.clone(), ctx(), json!({}))
        .await
        .unwrap()
        .json;
    assert_eq!(out["total"], json!(6));
    assert_eq!(out["offset"], json!(0));
    assert_eq!(out["returned"], json!(6));
    assert_eq!(out["truncated"], json!(false));
    assert_eq!(out["addon"], json!("mcp_test"));
    assert!(
        out.get("note").is_none(),
        "a complete scan has nothing to note"
    );
    let models = out["models"].as_array().unwrap();
    let listed: Vec<&str> = models
        .iter()
        .map(|entry| entry["model"].as_str().unwrap())
        .collect();
    assert_eq!(listed, SIX);
    assert_eq!(
        models[5],
        json!({
            "model": "models/props/unbuilt.vmdl",
            "sources": ["addon-content"],
            "compiled": false,
        }),
        "the content-only model reports compiled:false"
    );
    assert_eq!(
        models[2],
        json!({
            "model": "models/props/chair.vmdl",
            "sources": ["addon-game", "addon-content"],
            "compiled": true,
        })
    );
    // the built catalog is cached in the state
    assert!(state.model_catalog.lock().unwrap().is_some());
    {
        let cached = state.model_catalog.lock().unwrap().clone().unwrap();
        assert_eq!(cached.len(), 6);
    }
}

#[tokio::test]
async fn list_models_filters_by_query_glob_prefix_source_compiled() {
    let tree = full_tree("filters");
    let state = HammerState::from_config(&config(&tree)).unwrap();

    let models_of = |out: &Value| -> Vec<String> {
        out["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["model"].as_str().unwrap().to_string())
            .collect()
    };
    #[allow(clippy::redundant_closure)]
    async fn call(state: HammerState, args: Value) -> Value {
        hammer_mcp::tools::model_catalog::list_models(state, ctx(), args)
            .await
            .unwrap()
            .json
    }

    // substring (case-insensitive)
    let out = call(state.clone(), json!({"query": "DUST"})).await;
    assert_eq!(
        models_of(&out),
        ["models/props/dust/box.vmdl", "models/props/dust/pipes.vmdl"]
    );

    // a query carrying '*' or '?' is a whole-pattern glob
    let out = call(state.clone(), json!({"query": "models/props/*box*"})).await;
    assert_eq!(models_of(&out), ["models/props/dust/box.vmdl"]);
    let out = call(state.clone(), json!({"query": "models/props/?h?ir.vmdl"})).await;
    assert_eq!(models_of(&out), ["models/props/chair.vmdl"]);

    // prefix (case-insensitive, `models/` root only)
    let out = call(state.clone(), json!({"prefix": "MODELS\\dev\\"})).await;
    assert_eq!(models_of(&out), ["models/dev/axis.vmdl"]);

    // source filters
    let out = call(state.clone(), json!({"source": "addon"})).await;
    assert_eq!(
        models_of(&out),
        ["models/props/chair.vmdl", "models/props/unbuilt.vmdl"]
    );
    let out = call(state.clone(), json!({"source": "game"})).await;
    assert_eq!(
        models_of(&out),
        [
            "models/deco/sign.vmdl",
            "models/dev/axis.vmdl",
            "models/props/dust/box.vmdl",
            "models/props/dust/pipes.vmdl"
        ]
    );

    // compiledOnly drops the content-only model
    let out = call(
        state.clone(),
        json!({"compiledOnly": true, "source": "addon"}),
    )
    .await;
    assert_eq!(models_of(&out), ["models/props/chair.vmdl"]);

    // combinations compose
    let out = call(
        state.clone(),
        json!({"source": "game", "prefix": "models/props/", "query": "pipe"}),
    )
    .await;
    assert_eq!(models_of(&out), ["models/props/dust/pipes.vmdl"]);
}

#[tokio::test]
async fn list_models_pages() {
    let tree = full_tree("paging");
    let state = HammerState::from_config(&config(&tree)).unwrap();
    let out = hammer_mcp::tools::model_catalog::list_models(
        state.clone(),
        ctx(),
        json!({"limit": 2, "offset": 2}),
    )
    .await
    .unwrap()
    .json;
    assert_eq!(out["total"], json!(6), "the total counts the matches");
    assert_eq!(out["returned"], json!(2));
    assert_eq!(out["truncated"], json!(true));
    assert_eq!(out["offset"], json!(2));
    let models: Vec<&str> = out["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["model"].as_str().unwrap())
        .collect();
    assert_eq!(
        models,
        ["models/props/chair.vmdl", "models/props/dust/box.vmdl"]
    );

    // an offset past the total is an empty page, honestly reported
    let out =
        hammer_mcp::tools::model_catalog::list_models(state.clone(), ctx(), json!({"offset": 100}))
            .await
            .unwrap()
            .json;
    assert_eq!(out["total"], json!(6));
    assert_eq!(out["returned"], json!(0));
    assert_eq!(out["truncated"], json!(false));
    assert!(out["models"].as_array().unwrap().is_empty());
    // the last page reports truncated:false
    let out = hammer_mcp::tools::model_catalog::list_models(
        state.clone(),
        ctx(),
        json!({"limit": 100, "offset": 5}),
    )
    .await
    .unwrap()
    .json;
    assert_eq!(out["returned"], json!(1));
    assert_eq!(out["truncated"], json!(false));
}

/// The required argument negatives: bounds and type errors (`InvalidArguments`), nothing
/// dispatched.
#[tokio::test]
async fn list_models_refuses_bad_arguments() {
    let tree = full_tree("badargs");
    let state = HammerState::from_config(&config(&tree)).unwrap();
    for (args, why) in [
        (json!({"limit": 0}), "limit 0"),
        (json!({"limit": 1001}), "limit over max"),
        (json!({"limit": "5"}), "limit wrong type"),
        (json!({"limit": -3}), "limit negative"),
        (json!({"offset": -4}), "offset negative"),
        (json!({"offset": 1.5}), "offset non-integer"),
        (json!({"query": ""}), "empty query"),
        (json!({"prefix": "props/"}), "prefix outside models/"),
        (json!({"prefix": ""}), "empty prefix"),
        (json!({"source": "none"}), "unknown source"),
        (json!({"source": 3}), "source wrong type"),
        (json!({"compiledOnly": "yes"}), "compiledOnly wrong type"),
        (json!({"sort": "asc"}), "unknown argument"),
    ] {
        let error =
            hammer_mcp::tools::model_catalog::list_models(state.clone(), ctx(), args.clone())
                .await
                .unwrap_err();
        assert_eq!(error.type_, "InvalidArguments", "{args} in {why}: {error}");
        assert_eq!(error.name, "InvalidParams", "{args} in {why}");
        assert!(error.retry_safe, "{args} in {why}");
    }
}

/// The game VPK is required: a tree without it errors the tool naming the file — never an
/// empty success that looks like "nothing found".
#[tokio::test]
async fn a_missing_game_vpk_errors_the_tool() {
    let tree = Tree::new("tool-missing");
    tree.build_addon();
    let state = HammerState::from_config(&config(&tree)).unwrap();
    let error = hammer_mcp::tools::model_catalog::list_models(state, ctx(), json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.name, "VpkMissing", "{error}");
    assert!(
        error.message.contains("game/csgo/pak01_dir.vpk"),
        "the file is named: {error}"
    );
    assert!(error.data["path"].is_string());
}

/// The lookup helpers: normalized refusals before any scan, and the addon rescan on a miss
/// (new addon files become visible; the rescan is stored as the catalog).
#[tokio::test]
async fn lookup_refusal_and_the_addon_miss_rescan() {
    let tree = full_tree("rescan");
    let state = HammerState::from_config(&config(&tree)).unwrap();

    // invalid model arguments are refused before any rescan
    for bad in [
        "materials/steel.vmat",
        "models/x.vmat",
        "C:/x.vmdl",
        "models/../x.vmdl",
        "",
    ] {
        let error = hammer_mcp::tools::model_catalog::lookup_model(&state, bad)
            .await
            .expect_err(bad);
        assert_eq!(error.type_, "InvalidArguments", "{bad}");
    }

    // before: 2 addon models
    let out = hammer_mcp::tools::model_catalog::list_models(
        state.clone(),
        ctx(),
        json!({"source": "addon"}),
    )
    .await
    .unwrap()
    .json;
    assert_eq!(out["total"], json!(2));

    // the addon gains a compiled model during the session
    std::fs::write(
        tree.app
            .join("game/csgo_addons/mcp_test/models/props/lamp.vmdl_c"),
        b"lamp",
    )
    .unwrap();

    let entry = hammer_mcp::tools::model_catalog::lookup_model(&state, "models/props/lamp.vmdl")
        .await
        .unwrap()
        .expect("the miss rescanned the addon dirs");
    assert!(entry.compiled);
    assert_eq!(
        entry.sources,
        [ModelSource::AddonGame {
            file: PathBuf::from("props/lamp.vmdl_c")
        }]
    );

    // the rescan is stored: the (cached) list sees it too
    let out = hammer_mcp::tools::model_catalog::list_models(
        state.clone(),
        ctx(),
        json!({"source": "addon"}),
    )
    .await
    .unwrap()
    .json;
    assert_eq!(out["total"], json!(3));

    // an uncompiled source gained later is found compiled:false by the same route
    std::fs::write(
        tree.app
            .join("content/csgo_addons/mcp_test/models/props/late.vmdl"),
        b"late",
    )
    .unwrap();
    let entry = hammer_mcp::tools::model_catalog::lookup_model(&state, "models/props/late.vmdl")
        .await
        .unwrap()
        .expect("the source is rescan-visible too");
    assert!(!entry.compiled);
    // a still-missing model stays None after the retry
    assert!(
        hammer_mcp::tools::model_catalog::lookup_model(&state, "models/props/ghost.vmdl")
            .await
            .unwrap()
            .is_none()
    );
}

/// `read_compiled_model` takes the same miss-rescan retry and reads binary bytes for both the
/// addon and the VPK routes, refusing an uncompiled model.
#[tokio::test]
async fn read_compiled_model_reads_addon_and_vpk_and_refuses_uncompiled() {
    let tree = full_tree("readstate");
    let state = HammerState::from_config(&config(&tree)).unwrap();
    let read = |state: HammerState, model: String| async move {
        hammer_mcp::tools::model_catalog::read_compiled_model(&state, &model).await
    };
    assert_eq!(
        read(state.clone(), "models/props/chair.vmdl".into())
            .await
            .unwrap(),
        b"chairc\n".to_vec()
    );
    assert_eq!(
        read(state.clone(), "MODELS\\DECO\\sign.VMDL".into())
            .await
            .unwrap(),
        b"sign-data".to_vec()
    );
    let error = read(state.clone(), "models/props/unbuilt.vmdl".into())
        .await
        .unwrap_err();
    assert_eq!(error.name, "ModelNotCompiled", "{error}");
    let error = read(state.clone(), "models/props/ghost.vmdl".into())
        .await
        .unwrap_err();
    assert_eq!(error.name, "ModelNotFound", "{error}");
    // a model created THIS moment is found after the rescan and readable
    std::fs::write(
        tree.app
            .join("game/csgo_addons/mcp_test/models/props/later.vmdl_c"),
        b"later",
    )
    .unwrap();
    assert_eq!(
        read(state.clone(), "models/props/later.vmdl".into())
            .await
            .unwrap(),
        b"later".to_vec()
    );
}

/// The normalize rejections at the API surface (WP-V's "non-models path rejected").
#[test]
fn normalize_model_path_rejects_non_model_paths() {
    for bad in [
        "materials/steel.vmat",
        "models/x.vmat",
        "textures/x.vtex",
        "models/",
        "models",
        "models/props/x.exe",
    ] {
        assert!(normalize_model_path(bad).is_err(), "{bad}");
    }
    assert_eq!(
        normalize_model_path("models/props/left.vmdl").unwrap(),
        "models/props/left.vmdl"
    );
}

/// I6: the real player agents are `agents/models/**`; only that second root is added.
#[test]
fn normalize_model_path_accepts_the_agents_root() {
    assert_eq!(
        normalize_model_path("Agents\\Models\\TM_Phoenix\\tm_phoenix.vmdl_c").unwrap(),
        "agents/models/tm_phoenix/tm_phoenix.vmdl"
    );
    for bad in ["agents/models", "agents/models/", "agents/x.vmdl", "agents/model/x.vmdl", "characters/models/x/x.vmdl", "agents/models/../x.vmdl"] {
        assert!(normalize_model_path(bad).is_err(), "{bad}");
    }
}

/// The source labels are the wire contract.
#[test]
fn the_source_labels() {
    assert_eq!(
        ModelSource::AddonGame {
            file: PathBuf::new()
        }
        .label(),
        "addon-game"
    );
    assert_eq!(
        ModelSource::AddonContent {
            file: PathBuf::new()
        }
        .label(),
        "addon-content"
    );
    assert_eq!(ModelSource::Vpk { pak: "csgo" }.label(), "vpk:csgo");
    assert_eq!(ModelSource::Vpk { pak: "core" }.label(), "vpk:core");
}

// --------------------------------------------------------------------------- suggestions

/// Review w2b #10 (the regression): cross-directory near-misses are ranked by closeness, not
/// alphabetically, and a 2-character entry stem ("ax") no longer shares with every request.
/// On the old code this exact tree answered `["dev/ax", "dev/axis", "tools/axis_rem"]` — the
/// alphabetical order with the short stem first.
#[tokio::test]
async fn cross_directory_suggestions_rank_by_closeness() {
    let tree = full_tree("suggestions");
    // two addon models for the shared-stem scenario: a 2-char stem (the old rule let it share
    // anything) and a near-name next to a same-prefix longer neighbour
    let game = tree.app.join("game/csgo_addons/mcp_test/models");
    for (rel, bytes) in [
        ("dev/ax.vmdl_c", &b"ax"[..]),
        ("tools/axis_rem.vmdl_c", &b"axisrem"[..]),
    ] {
        let path = game.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
    }
    let state = HammerState::from_config(&config(&tree)).unwrap();
    let error = hammer_mcp::tools::model_info::get_model_info(
        state,
        CallCtx {
            budget: Budget::new(45_000, 5_000),
        },
        json!({"model": "models/dust/axis_remake.vmdl"}),
    )
    .await
    .expect_err("cross-directory miss");
    assert_eq!(error.type_, "ModelNotFound", "{error:?}");
    let suggestions = error.data["didYouMean"].as_array().expect("suggestions");
    assert_eq!(
        suggestions,
        &vec!(
            json!("models/tools/axis_rem.vmdl"),
            json!("models/dev/axis.vmdl"),
        ),
        "8-prefix before 4-prefix, 'ax' filtered out: {error:?}"
    );
}

#[tokio::test]
async fn the_cached_catalog_is_keyed_by_addon() {
    let tree = full_tree("keyed");
    let other = tree.app.join("content/csgo_addons/other/models");
    std::fs::create_dir_all(&other).expect("other addon models");
    std::fs::write(other.join("other_only.vmdl"), "x").expect("other model");
    let state = HammerState::from_config(&config(&tree)).unwrap();

    let first = hammer_mcp::tools::model_catalog::list_models(state.clone(), ctx(), json!({"source": "addon"}))
        .await
        .unwrap()
        .json;
    assert_eq!(first["addon"], json!("mcp_test"));
    let listed = |out: &Value| -> Vec<String> {
        out["models"].as_array().unwrap().iter().map(|m| m["model"].as_str().unwrap().to_string()).collect()
    };
    assert!(!listed(&first).contains(&"models/other_only.vmdl".to_string()));
    assert!(state.model_catalog.lock().unwrap().is_some(), "cached for mcp_test");

    // the selection moves: the catalog filled for mcp_test must not answer for `other`
    state.set_addon(Some("other".to_string()));
    let second = hammer_mcp::tools::model_catalog::list_models(state.clone(), ctx(), json!({"source": "addon"}))
        .await
        .unwrap()
        .json;
    assert_eq!(second["addon"], json!("other"));
    assert_eq!(listed(&second), vec!["models/other_only.vmdl".to_string()]);

    // and back: rebuilt again for mcp_test, not left on `other`
    state.set_addon(Some("mcp_test".to_string()));
    let third = hammer_mcp::tools::model_catalog::list_models(state, ctx(), json!({"source": "addon"}))
        .await
        .unwrap()
        .json;
    assert_eq!(third["addon"], json!("mcp_test"));
    assert!(!listed(&third).contains(&"models/other_only.vmdl".to_string()));
}
