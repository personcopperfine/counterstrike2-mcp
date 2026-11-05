//! `get_model_info` tests (WP-R1): the tool against a fixture app tree under
//! `CARGO_TARGET_TMPDIR` — a synthetic VPK pair (`tests/common`'s `write_vpk`) whose models are
//! synthetic `.vmdl_c` containers built here (the KV3 v4-uncompressed encoder now lives in
//! `tests/common`, factored out of `tests/resource.rs`'s approach), plus an addon directory
//! with a compiled and an uncompiled model.
//!
//! Covered: the fact shape (bounds + placement + skins/bodygroups + lists + truncation), the
//! `include`/`limit` argument work, keeping the m_modelInfo zeros honest (`hull` null with the
//! parser's note), and the negatives the brief demands — bad arguments, an unknown model with
//! the catalog's suggestions, the uncompiled addon model (with the `model_compile` hint), and
//! corrupt/decoy container bytes (a clear decode error naming the model, never empty facts).
//! The `#[ignore]`d corpus test reads the real install's `terrace_chair_01.vmdl_c`.
#![allow(clippy::result_large_err)]

mod common;

use common::{kv_object, kv_strings, kv_text, kv_vec3, model_vmdl_c};
use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::tools::{Budget, CallCtx};
use hammer_mcp::resource::kv3bin::Kv3Value;
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A temp app tree laid out like the real one (`workshop/run/app`, addon `mcp_test`).
struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "model-info-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("tree");
        let app = root.join("workshop/run/app");
        Tree { root, app }
    }

    /// The csgo + core paks: `chair.vmdl_c` (the fixture model, in-dir data), a decoy
    /// `broken.vmdl_c` (bytes the decoder must refuse), plus non-models the catalog ignores.
    fn build_vpks(&self, chair: &[u8], broken: &[u8]) {
        common::write_vpk(
            &self.app.join("game/csgo"),
            "pak01",
            2,
            &[
                // a preload would sit in front of the read bytes; a Resource must start at
                // the container header, so the fixture models carry none
                common::BuildFile::loaded("models/props/fixture/chair.vmdl_c", chair, b"", false),
                common::BuildFile::loaded(
                    "models/props/fixture/broken.vmdl_c",
                    broken,
                    b"",
                    false,
                ),
                common::BuildFile::preload_only("models/dev/etc.vtex_c", b""),
            ],
        );
        common::write_vpk(
            &self.app.join("game/core"),
            "pak01",
            1,
            &[common::BuildFile::loaded(
                "models/dev/axis.vmdl_c",
                &model_vmdl_c(&[("DATA", axis_data())]),
                b"",
                true,
            )],
        );
    }

    /// The addon: a compiled `.vmdl_c` (its own facts) and an uncompiled source.
    fn build_addon(&self) {
        let game = self
            .app
            .join("game/csgo_addons/mcp_test/models/props/fixture/stool.vmdl_c");
        std::fs::create_dir_all(game.parent().unwrap()).expect("addon game dir");
        std::fs::write(&game, model_vmdl_c(&[("DATA", stool_data())])).expect("compiled addon");
        let content = self.app.join("content/csgo_addons/mcp_test/models/props/fixture");
        std::fs::create_dir_all(&content).expect("addon content dir");
        std::fs::write(
            content.join("unbuilt.vmdl"),
            model_vmdl_c(&[("DATA", stool_data())]),
        )
        .expect("uncompiled source");
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// `m_vMinBounds`/`m_vMaxBounds` of one MDAT scene object (the SDK's spelling).
fn scene_object(min: [f64; 3], max: [f64; 3]) -> Kv3Value {
    kv_object(vec![
        ("m_vMinBounds", kv_vec3(min[0], min[1], min[2])),
        ("m_vMaxBounds", kv_vec3(max[0], max[1], max[2])),
    ])
}

fn box_object(min: [f64; 3], max: [f64; 3]) -> Kv3Value {
    kv_object(vec![
        ("m_vMinBounds", kv_vec3(min[0], min[1], min[2])),
        ("m_vMaxBounds", kv_vec3(max[0], max[1], max[2])),
    ])
}

/// The chair's `DATA` block: a named model with skins and a bodygroup, but the `m_modelInfo`
/// hull/view boxes the corpus says every CS2 prop carries as zeros, plus an embedded keyvalue
/// text (`prop_data`) nobody asked for by default.
fn chair_data() -> Kv3Value {
    kv_object(vec![
        ("m_name", kv_text("models/props/fixture/chair.vmdl")),
        (
            "m_modelInfo",
            kv_object(vec![
                ("m_vHullMin", kv_vec3(0.0, 0.0, 0.0)),
                ("m_vHullMax", kv_vec3(0.0, 0.0, 0.0)),
                ("m_flMass", Kv3Value::Double(0.0)),
                (
                    "m_keyValueText",
                    kv_text("<!-- kv3 encoding:text:version:{61} -->\nprop_data{}"),
                ),
            ]),
        ),
        (
            "m_materialGroups",
            Kv3Value::Array(vec![
                kv_object(vec![("m_name", kv_text("default"))]),
                kv_object(vec![("m_name", kv_text("tint"))]),
            ]),
        ),
        ("m_meshGroups", kv_strings(&["chair_@0_#&open", "chair_@1_#&folded"])),
        (
            "m_refMeshGroupMasks",
            Kv3Value::TypedArray(vec![Kv3Value::Int(1), Kv3Value::Int(2)]),
        ),
    ])
}

/// The chair's render block: one scene object whose box is `[-10,-10,-2]..[10,10,46]`, with an
/// attachment next to it (`floorOffset` = 2.0, `footprint` = [20, 20]).
fn chair_mdat() -> Kv3Value {
    kv_object(vec![
        (
            "m_sceneObjects",
            Kv3Value::Array(vec![scene_object([-10.0, -10.0, -2.0], [10.0, 10.0, 46.0])]),
        ),
        (
            "m_attachments",
            Kv3Value::Array(vec![kv_object(vec![
                ("key", kv_text("handle")),
                ("value", kv_object(vec![("m_name", kv_text("handle"))])),
            ])]),
        ),
    ])
}

/// A sequence block with two names (out of the array the real files' `ASEQ` blocks carry).
fn chair_aseq() -> Kv3Value {
    kv_object(vec![("m_localSequenceNameArray", kv_strings(&["idle", "wave"]))])
}

/// A physics block whose hull box says `[-9,-9,0]..[9,9,48]` (the physics bounds).
fn chair_phys() -> Kv3Value {
    kv_object(vec![
        (
            "m_parts",
            Kv3Value::Array(vec![kv_object(vec![(
                "m_rnShape",
                kv_object(vec![(
                    "m_hulls",
                    Kv3Value::Array(vec![kv_object(vec![(
                        "m_Hull",
                        kv_object(vec![(
                            "m_Bounds",
                            box_object([-9.0, -9.0, 0.0], [9.0, 9.0, 48.0]),
                        )]),
                    )])]),
                )]),
            )])]),
        ),
    ])
}

/// The axis (core pak) and the stool (addon): minimal `DATA`-only models, different names.
fn axis_data() -> Kv3Value {
    kv_object(vec![
        ("m_name", kv_text("models/dev/axis.vmdl")),
        (
            "m_modelInfo",
            kv_object(vec![("m_flMass", Kv3Value::Double(12.5))]),
        ),
    ])
}

fn stool_data() -> Kv3Value {
    kv_object(vec![
        ("m_name", kv_text("models/props/fixture/stool.vmdl")),
        ("m_materialGroups", Kv3Value::Array(Vec::new())),
    ])
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

/// The default fixture tree: the two VPKs (chair, broken) + the addon parts.
fn tree_with_models(tag: &str, chair: &[u8], broken: &[u8]) -> Tree {
    let tree = Tree::new(tag);
    tree.build_vpks(chair, broken);
    tree.build_addon();
    tree
}

/// The 3×3 + vectors of one chair model.
fn chair_bytes() -> Vec<u8> {
    model_vmdl_c(&[
        ("DATA", chair_data()),
        ("MDAT", chair_mdat()),
        ("ASEQ", chair_aseq()),
        ("PHYS", chair_phys()),
    ])
}

/// Decoy bytes: a valid resource header shape but a `DATA` block whose body is not KV3 at all.
fn broken_bytes() -> Vec<u8> {
    model_vmdl_c(&[("DATA", kv_text("not a kv3 document"))])
}

/// Two *distinct* authored bodygroups (the chair fixture merges its two entries into one named
/// group, so the limit cannot cut it): the review w2b #4 regression model.
fn duo_bytes() -> Vec<u8> {
    model_vmdl_c(&[(
        "DATA",
        kv_object(vec![
            ("m_name", kv_text("models/props/fixture/duo.vmdl")),
            (
                "m_meshGroups",
                kv_strings(&["wheel_@0_#&left", "handle_@0_#&brass"]),
            ),
        ]),
    )])
}

async fn call(
    state: HammerState,
    args: Value,
) -> Result<Value, cs2_mcp_common::tools::ToolError> {
    hammer_mcp::tools::model_info::get_model_info(state, ctx(), args)
        .await
        .map(|out| out.json)
}

// -------------------------------------------------------------------------------------------
// the happy path
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn get_model_info_reports_the_fixture_facts() {
    let tree = tree_with_models("facts", &chair_bytes(), &broken_bytes());
    let state = HammerState::from_config(&config(&tree)).unwrap();
    let out = call(state, json!({"model": "models/props/fixture/chair.vmdl"}))
        .await
        .expect("reads");

    assert_eq!(out["model"], json!("models/props/fixture/chair.vmdl"));
    assert_eq!(out["sources"], json!(["vpk:csgo"]));
    assert_eq!(out["compiled"], json!(true));
    assert_eq!(out["name"], json!("models/props/fixture/chair.vmdl"));
    // the usable boxes: render from MDAT, physics from the PHYS hull — each with size/centre
    assert_eq!(
        out["bounds"]["render"],
        json!({
            "min": [-10.0, -10.0, -2.0],
            "max": [10.0, 10.0, 46.0],
            "size": [20.0, 20.0, 48.0],
            "center": [0.0, 0.0, 22.0],
        })
    );
    assert_eq!(
        out["bounds"]["physics"]["min"],
        json!([-9.0, -9.0, 0.0]),
        "{out}"
    );
    assert_eq!(out["hasPhysics"], json!(true));
    // placement (create_props' snapToFloor data): floorOffset = -render.min.z, x/y footprint
    assert_eq!(
        out["placement"],
        json!({"floorOffset": 2.0, "footprint": [20.0, 20.0]})
    );
    assert_eq!(out["skins"], json!(["default", "tint"]));
    // bodygroups: the authored exporter names merged into named groups
    let bodygroups = out["bodygroups"].as_array().expect("bodygroups");
    assert_eq!(bodygroups.len(), 1, "{out}");
    assert_eq!(bodygroups[0]["name"], json!("chair"));
    assert_eq!(
        bodygroups[0]["choices"],
        json!(["open", "folded"]),
        "{out}"
    );
    // the default include: sequences on, the embedded keyvalue text off
    assert_eq!(out["sequences"], json!(["idle", "wave"]));
    assert_eq!(out["attachments"], json!(["handle"]));
    assert!(out.get("keyvalues").is_none(), "{out}");
    assert!(out.get("truncated").is_none(), "{out}");
    // the m_modelInfo hull/view are the corpus-honest zeros: null fields plus the parser note
    assert_eq!(out["hull"], Value::Null);
    assert_eq!(out["view"], Value::Null);
    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("hull and view boxes are all zero")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn get_model_info_serves_every_spelling_of_one_model() {
    let tree = tree_with_models("spelling", &chair_bytes(), &broken_bytes());
    let state = HammerState::from_config(&config(&tree)).unwrap();
    for spelling in [
        "models/props/fixture/chair.vmdl",
        "MODELS\\PROPS\\FIXTURE\\CHAIR.VMDL",
        "models/props/fixture/chair.vmdl_c",
    ] {
        let out = call(state.clone(), json!({"model": spelling}))
            .await
            .unwrap_or_else(|e| panic!("{spelling}: {e}"));
        assert_eq!(out["model"], json!("models/props/fixture/chair.vmdl"));
    }
}

/// Review w2b #4 end-to-end: a bodygroups list over the `limit` is cut AND flagged
/// (`truncated.bodygroups`), like sequences/attachments always were. Dedicated tree: the
/// same-directory suggestion exact-vec test above pins the whole fixture list.
#[tokio::test]
async fn a_bodygroup_list_over_the_limit_is_cut_and_flagged() {
    let tree = Tree::new("bodygroup-limit");
    common::write_vpk(
        &tree.app.join("game/csgo"),
        "pak01",
        2,
        &[common::BuildFile::loaded(
            "models/props/fixture/duo.vmdl_c",
            &duo_bytes(),
            b"",
            false,
        )],
    );
    // the catalog requires both game paks: the core one stays empty (the axis model of the
    // common fixture is not needed here)
    common::write_vpk(&tree.app.join("game/core"), "pak01", 1, &[]);
    let state = HammerState::from_config(&config(&tree)).unwrap();
    let out = call(
        state.clone(),
        json!({"model": "models/props/fixture/duo.vmdl", "limit": 1}),
    )
    .await
    .expect("reads");
    let bodygroups = out["bodygroups"].as_array().expect("bodygroups");
    assert_eq!(bodygroups.len(), 1, "{out}");
    assert_eq!(bodygroups[0]["name"], json!("wheel"), "{out}");
    assert_eq!(out["truncated"], json!({"bodygroups": true}), "{out}");
    let notes = out["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("2 named bodygroups")),
        "{notes:?}"
    );

    // limit 2: both groups fit, no flag
    let out = call(
        state,
        json!({"model": "models/props/fixture/duo.vmdl", "limit": 2}),
    )
    .await
    .unwrap();
    assert_eq!(out["bodygroups"].as_array().unwrap().len(), 2, "{out}");
    assert!(out.get("truncated").is_none(), "{out}");
}

#[tokio::test]
async fn get_model_info_include_and_limit_work_together() {
    let tree = tree_with_models("include", &chair_bytes(), &broken_bytes());
    let state = HammerState::from_config(&config(&tree)).unwrap();

    // keyvalues: the embedded text, only on request
    let out = call(
        state.clone(),
        json!({"model": "models/props/fixture/chair.vmdl", "include": ["keyvalues"]}),
    )
    .await
    .unwrap();
    assert!(out["keyvalues"].as_str().unwrap_or_default().contains("prop_data"), "{out}");
    assert!(out.get("sequences").is_none(), "{out}");
    assert!(out.get("bodygroups").is_none(), "{out}");

    // limit cuts every list and flags what was cut
    let out = call(
        state.clone(),
        json!({
            "model": "models/props/fixture/chair.vmdl",
            "include": ["sequences", "attachments"],
            "limit": 1
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["sequences"], json!(["idle"]));
    assert_eq!(out["attachments"], json!(["handle"]));
    // only the sequences ran over the limit (two of them)
    assert_eq!(out["truncated"], json!({"sequences": true}));
    let notes = out["notes"].as_array().unwrap();
    assert!(
        notes.iter().any(|n| n.as_str().unwrap_or_default().contains("cut")),
        "{notes:?}"
    );

    // limit 2: the sequences fit again, nothing truncated
    let out = call(
        state.clone(),
        json!({"model": "models/props/fixture/chair.vmdl", "limit": 2}),
    )
    .await
    .unwrap();
    assert_eq!(out["sequences"], json!(["idle", "wave"]));
    assert!(out.get("truncated").is_none(), "{out}");
}

// -------------------------------------------------------------------------------------------
// the negatives
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn get_model_info_refuses_bad_arguments() {
    let tree = tree_with_models("badargs", &chair_bytes(), &broken_bytes());
    let state = HammerState::from_config(&config(&tree)).unwrap();
    for (args, why) in [
        (json!({}), "model missing"),
        (json!({"model": ""}), "empty model"),
        (json!({"model": "   "}), "blank model"),
        (json!({"model": 7}), "model wrong type"),
        (json!({"model": "models/x.mdl"}), "not a model path"),
        (json!({"model": "materials/steel.vmat"}), "not under models/"),
        (json!({"model": "models/../props/x.vmdl"}), ".. segment"),
        (
            json!({"model": "models/props/fixture/chair.vmdl", "limit": 0}),
            "limit 0",
        ),
        (
            json!({"model": "models/props/fixture/chair.vmdl", "limit": 2001}),
            "limit over max",
        ),
        (
            json!({"model": "models/props/fixture/chair.vmdl", "limit": "2"}),
            "limit wrong type",
        ),
        (
            json!({"model": "models/props/fixture/chair.vmdl", "include": ["nope"]}),
            "unknown include",
        ),
        (
            json!({"model": "models/props/fixture/chair.vmdl", "include": "sequences"}),
            "include not an array",
        ),
        (
            json!({"model": "models/props/fixture/chair.vmdl", "wat": 1}),
            "unknown argument",
        ),
    ] {
        let error = call(state.clone(), args.clone())
            .await
            .expect_err(&format!("{why}: {args:?}"));
        assert_eq!(error.type_, "InvalidArguments", "{why}: {error:?}");
        assert!(error.retry_safe, "{why}: nothing was dispatched");
    }
}

#[tokio::test]
async fn an_unknown_model_refuses_with_the_catalogs_suggestions() {
    let tree = tree_with_models("unknown", &chair_bytes(), &broken_bytes());
    let state = HammerState::from_config(&config(&tree)).unwrap();

    // same-directory miss: the two other fixture models are suggested (sorted, capped)
    let error = call(
        state.clone(),
        json!({"model": "models/props/fixture/chair_missing.vmdl"}),
    )
    .await
    .unwrap_err();
    assert_eq!(error.type_, "ModelNotFound", "{error:?}");
    let suggestions = error.data["didYouMean"].as_array().expect("suggestions");
    // same directory: every fixture model is a candidate, in the catalog's sorted order
    assert_eq!(
        suggestions,
        &vec!(
            json!("models/props/fixture/broken.vmdl"),
            json!("models/props/fixture/chair.vmdl"),
            json!("models/props/fixture/stool.vmdl"),
            json!("models/props/fixture/unbuilt.vmdl"),
        ),
        "{error:?}"
    );
    // the error text names the model and the list too
    assert!(error.message.contains("chair_missing"), "{}", error.message);
    assert!(error.message.contains("models/props/fixture/broken.vmdl"), "{}", error.message);

    // a shared-name-stem suggestion reaches across directories (no same-dir hit at all)
    let error = call(
        state,
        json!({"model": "models/props/dust/chair_wood.vmdl"}),
    )
    .await
    .unwrap_err();
    let suggestions = error.data["didYouMean"].as_array().expect("suggestions");
    // the extension is not part of a name: `chair` sits inside `chair_wood`
    assert!(
        suggestions
            == &vec!(json!("models/props/fixture/chair.vmdl")),
        "{error:?}"
    );
}

#[tokio::test]
async fn an_uncompiled_addon_model_refuses_with_the_compile_hint() {
    let tree = tree_with_models("uncompiled", &chair_bytes(), &broken_bytes());
    let state = HammerState::from_config(&config(&tree)).unwrap();
    let error = call(
        state,
        json!({"model": "models/props/fixture/unbuilt.vmdl"}),
    )
    .await
    .unwrap_err();
    assert_eq!(error.type_, "ModelNotCompiled", "{error:?}");
    assert_eq!(error.data["useInstead"]["tool"], json!("model_compile"));
    assert_eq!(error.data["model"], json!("models/props/fixture/unbuilt.vmdl"));
    assert!(error.retry_safe);
}

#[tokio::test]
async fn corrupt_container_bytes_are_a_clear_error_naming_the_model() {
    let tree = tree_with_models("broken", &chair_bytes(), &broken_bytes());
    let state = HammerState::from_config(&config(&tree)).unwrap();
    let error = call(state, json!({"model": "models/props/fixture/broken.vmdl"}))
        .await
        .unwrap_err();
    assert_eq!(error.type_, "ModelRead", "{error:?}");
    assert!(
        error.message.contains("models/props/fixture/broken.vmdl"),
        "the error names the model: {}",
        error.message
    );
    // and it is not an empty success carrying no facts
    assert!(error.data["error"].is_string(), "{error:?}");
}

#[tokio::test]
async fn a_truncated_model_file_is_a_clear_error_naming_the_model() {
    let bytes = chair_bytes();
    // chop the last 24 bytes (part of the block bodies + trailer): the container boundary
    // checks must catch it, not panic
    let cut = &bytes[..bytes.len() - 24];
    let tree = tree_with_models("truncated", cut, &broken_bytes());
    let state = HammerState::from_config(&config(&tree)).unwrap();
    let error = call(state, json!({"model": "models/props/fixture/chair.vmdl"}))
        .await
        .unwrap_err();
    assert_eq!(error.type_, "ModelRead", "{error:?}");
    assert!(error.message.contains("chair.vmdl"), "{}", error.message);
}

#[tokio::test]
async fn a_catalog_that_cannot_be_built_is_the_recorded_error() {
    // no game VPK at all: the catalog build names the missing file (never an empty catalog)
    let tree = Tree::new("no-vpk");
    let state = HammerState::from_config(&config(&tree)).unwrap();
    let error = call(
        state,
        json!({"model": "models/props/fixture/chair.vmdl"}),
    )
    .await
    .unwrap_err();
    assert_eq!(error.type_, "VpkMissing", "{error:?}");
    assert!(error.data["path"].is_string(), "{error:?}");
}

// -------------------------------------------------------------------------------------------
// the real install (#[ignore]; the other real-tree tests' project-root rule)
// -------------------------------------------------------------------------------------------

fn project_root() -> PathBuf {
    const MARKER: &str = "workshop/run/app/game/csgo/csgo.fgd";
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("CS2_MCP_PROJECT_ROOT") {
        candidates.push(PathBuf::from(root));
    }
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        candidates.push(dir.clone());
        if !dir.pop() {
            break;
        }
    }
    candidates
        .into_iter()
        .find(|dir| dir.join(MARKER).is_file())
        .unwrap_or_else(|| {
            panic!(
                "no project root with {MARKER} at $CS2_MCP_PROJECT_ROOT or above {}",
                env!("CARGO_MANIFEST_DIR")
            )
        })
}

/// The real install: `models/generic/terrace_set_01/terrace_chair_01.vmdl` (the WP's corpus
/// model) decodes through the whole tool with usable boxes and a placement.
#[tokio::test]
#[ignore]
async fn real_install_terrace_chair_decodes() {
    let root = project_root();
    let cfg = {
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: CommonConfig,
        }
        let given: Vec<String> = vec![
            "--deadline-ms",
            "5000",
            "--project-root",
            &root.display().to_string(),
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let argv: Vec<String> = [vec!["t".to_string()], common::hermetic(&given.iter().map(String::as_str).collect::<Vec<&str>>())
            .iter()
            .map(|s| s.to_string())
            .collect()]
        .concat();
        W::parse_from(argv).c
    };
    let state = HammerState::from_config(&cfg).unwrap();
    let out = call(
        state,
        json!({"model": "models/generic/terrace_set_01/terrace_chair_01.vmdl"}),
    )
    .await
    .expect("the install carries the corpus model");
    assert_eq!(
        out["model"],
        json!("models/generic/terrace_set_01/terrace_chair_01.vmdl")
    );
    assert_eq!(out["compiled"], json!(true));
    let render = &out["bounds"]["render"];
    let min = render["min"].as_array().expect("render min");
    let max = render["max"].as_array().expect("render max");
    assert_eq!(min.len(), 3);
    assert_eq!(max.len(), 3);
    for axis in 0..3 {
        assert!(
            max[axis].as_f64().unwrap_or(f64::NEG_INFINITY) >= min[axis].as_f64().unwrap_or(f64::INFINITY),
            "a real box has max >= min per axis"
        );
    }
    let placement_min = min[2].as_f64().unwrap_or(0.0);
    assert!(
        (out["placement"]["floorOffset"].as_f64().unwrap_or(f64::NAN)
            + placement_min)
            .abs() < 1e-4,
        "floorOffset is -render.min.z: {out}"
    );
    assert!(out["placement"]["footprint"].is_array(), "{out}");
}

/// The fixture chair has one attachment (`handle`) that marks no front: the tool says `unknown`,
/// reports the long axis separately and points at `model_render` (it never guesses a sign).
#[tokio::test]
async fn facing_is_unknown_without_front_rear_attachments() {
    let tree = tree_with_models("facing", &chair_bytes(), &broken_bytes());
    let state = HammerState::from_config(&config(&tree)).unwrap();
    let out = hammer_mcp::tools::model_info::get_model_info(
        state,
        ctx(),
        json!({"model": "models/props/fixture/chair.vmdl"}),
    )
    .await
    .expect("reads");
    let facing = &out.json["facing"];
    assert_eq!(facing["forward"], "unknown", "{facing}");
    assert_eq!(facing["confidence"], "none");
    assert_eq!(facing["longAxis"], "x"); // the box is square (20x20): ties go to x
    assert!(facing["basis"].as_str().unwrap().contains("handle"), "{facing}");
    assert_eq!(out.next.as_ref().expect("a next step")[0].tool, "model_render");
}

/// A car model whose `wheel_fl`/`wheel_rl` attachments sit on bone `body`, whose rest rotation is
/// the quaternion `rot` (x, y, z, w); `DATA.m_modelSkeleton` carries it.
fn car_with_body_rotation(rot: [f64; 4]) -> Vec<u8> {
    let att = |name: &str, x: f64| {
        kv_object(vec![
            ("key", kv_text(name)),
            (
                "value",
                kv_object(vec![
                    ("m_name", kv_text(name)),
                    ("m_influenceNames", kv_strings(&["body"])),
                    ("m_vInfluenceOffsets", Kv3Value::TypedArray(vec![kv_vec3(x, 0.0, 10.0)])),
                    ("m_nInfluences", Kv3Value::Int(1)),
                ]),
            ),
        ])
    };
    let mdat = kv_object(vec![
        (
            "m_sceneObjects",
            Kv3Value::Array(vec![scene_object([-90.0, -30.0, 0.0], [90.0, 30.0, 50.0])]),
        ),
        (
            "m_attachments",
            Kv3Value::Array(vec![att("wheel_fl", -60.0), att("wheel_rl", 70.0)]),
        ),
    ]);
    let data = kv_object(vec![
        ("m_name", kv_text("models/props/fixture/chair.vmdl")),
        (
            "m_modelSkeleton",
            kv_object(vec![
                ("m_boneName", kv_strings(&["body"])),
                ("m_nParent", Kv3Value::Array(vec![Kv3Value::Int(-1)])),
                ("m_boneRotParent", Kv3Value::Array(vec![kv_quat(rot)])),
            ]),
        ),
    ]);
    model_vmdl_c(&[("DATA", data), ("MDAT", mdat)])
}

fn kv_quat(q: [f64; 4]) -> Kv3Value {
    Kv3Value::Array(q.iter().map(|v| Kv3Value::Double(*v)).collect())
}

async fn facing_of(tag: &str, car: Vec<u8>) -> Value {
    let tree = tree_with_models(tag, &car, &broken_bytes());
    let state = HammerState::from_config(&config(&tree)).unwrap();
    call(state, json!({"model": "models/props/fixture/chair.vmdl"}))
        .await
        .expect("reads")["facing"]
        .clone()
}

/// Front and rear attachments on a bone whose frame is the model's: the sign comes from them.
#[tokio::test]
async fn facing_reads_front_and_rear_attachments() {
    let facing = facing_of("facing-car", car_with_body_rotation([0.0, 0.0, 0.0, 1.0])).await;
    assert_eq!(facing["forward"], "-x", "{facing}");
    assert_eq!(facing["confidence"], "medium");
    assert_eq!(facing["longAxis"], "x");
}

/// The same offsets on a bone yawed 180 degrees are in the bone's frame, so the model-frame sign
/// is the opposite one: the tool must not answer -x (or anything) from them.
#[tokio::test]
async fn a_yawed_bone_gives_no_facing() {
    let facing = facing_of("facing-yawed", car_with_body_rotation([0.0, 0.0, 1.0, 0.0])).await;
    assert_eq!(facing["forward"], "unknown", "{facing}");
    assert!(facing["basis"].as_str().unwrap().contains("rotated or scaled"), "{facing}");
}

/// FIXME 21 (ti_convoy): `car_a` and `utility_truck` face -x. Prints what the file says; the
/// real models carry no attachments, so the honest answer is `unknown` (run with --nocapture).
#[tokio::test]
#[ignore]
async fn real_install_vehicle_facing_prints() {
    let root = project_root();
    let cfg = {
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: CommonConfig,
        }
        let r = root.display().to_string();
        let given: Vec<&str> = vec!["--deadline-ms", "5000", "--project-root", &r];
        let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
        W::parse_from(argv).c
    };
    let state = HammerState::from_config(&cfg).unwrap();
    for model in [
        "models/props/de_inferno/hr_i/car_a/car_a.vmdl",
        "models/props/de_train/utility_truck.vmdl",
    ] {
        let out = call(state.clone(), json!({"model": model})).await.expect("the install has it");
        println!("{model}: facing = {}", out["facing"]);
        println!("{model}: attachments = {}", out["attachments"]);
        assert!(out["facing"]["forward"].is_string());
    }
}

// -------------------------------------------------------------------------------------------
// I6: agents/models/** and animation/skeletons/**.vnmskel
// -------------------------------------------------------------------------------------------

fn agent_bytes() -> Vec<u8> {
    model_vmdl_c(&[(
        "DATA",
        kv_object(vec![
            ("m_name", kv_text("agents/models/tm_test/tm_test.vmdl")),
            (
                "m_modelSkeleton",
                kv_object(vec![
                    ("m_boneName", kv_strings(&["root_motion", "pelvis", "spine_0"])),
                    ("m_nParent", Kv3Value::TypedArray(vec![Kv3Value::Int(-1), Kv3Value::Int(5), Kv3Value::Int(5)])),
                ]),
            ),
        ]),
    )])
}

fn agent_tree(tag: &str) -> Tree {
    let tree = Tree::new(tag);
    common::write_vpk(
        &tree.app.join("game/csgo"),
        "pak01",
        2,
        &[
            common::BuildFile::loaded("agents/models/tm_test/tm_test.vmdl_c", &agent_bytes(), b"", false),
            common::BuildFile::loaded("animation/skeletons/characters/worldmodel.vnmskel_c", &common::vnmskel_c(), b"", false),
        ],
    );
    common::write_vpk(
        &tree.app.join("game/core"),
        "pak01",
        1,
        &[common::BuildFile::loaded("models/dev/axis.vmdl_c", &model_vmdl_c(&[("DATA", axis_data())]), b"", true)],
    );
    tree
}

#[tokio::test]
async fn an_agent_model_reports_its_bones_on_request() {
    let tree = agent_tree("agent");
    let state = HammerState::from_config(&config(&tree)).unwrap();
    let out = call(state.clone(), json!({"model": "agents/models/tm_test/tm_test.vmdl", "include": ["skeleton"]})).await.expect("reads");
    assert_eq!(out["model"], json!("agents/models/tm_test/tm_test.vmdl"));
    assert_eq!(out["boneCount"], json!(3));
    assert_eq!(out["skeleton"][1], json!({"index": 1, "name": "pelvis", "parent": 5}));
    // not in the default lists
    let plain = call(state, json!({"model": "agents/models/tm_test/tm_test.vmdl"})).await.expect("reads");
    assert!(plain.get("skeleton").is_none());
}

#[tokio::test]
async fn a_vnmskel_path_reads_the_skeleton_with_both_poses() {
    let tree = agent_tree("nmskel");
    let state = HammerState::from_config(&config(&tree)).unwrap();
    for spelling in ["animation/skeletons/characters/worldmodel.vnmskel", "Animation\\Skeletons\\Characters\\worldmodel.vnmskel_c"] {
        let out = call(state.clone(), json!({"model": spelling})).await.expect("reads");
        assert_eq!(out["kind"], json!("nmskeleton"));
        assert_eq!(out["boneCount"], json!(75));
        let pelvis = &out["bones"][1];
        assert_eq!(pelvis["name"], json!("pelvis"));
        assert_eq!(pelvis["parent"], json!(0));
        assert_eq!(pelvis["model"].as_array().unwrap().len(), 8);
        assert!((pelvis["model"][2].as_f64().unwrap() - 42.82).abs() < 0.01);
    }
    let err = call(state.clone(), json!({"model": "animation/skeletons/characters/nothing.vnmskel"})).await.unwrap_err();
    assert_eq!(err.type_, "ModelNotFound");
    // other roots stay refused
    let err = call(state, json!({"model": "characters/models/tm_test/tm_test.vmdl"})).await.unwrap_err();
    assert!(err.message.contains("models/"), "{}", err.message);
}
