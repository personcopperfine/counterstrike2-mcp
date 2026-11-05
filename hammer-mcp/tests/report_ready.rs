//! `report_ready` (streamline WP3.2) against a temp project tree (under `CARGO_TARGET_TMPDIR`, never
//! /tmp): the committed `probe.vmap`, fake VPKs built with `common::write_vpk`, loose files, and a
//! mock bridge for the "Hammer says the document is clean" route. The pure geometry of the mover
//! check (a spawn inside / just outside a `func_movelinear` sweep) is unit-tested next to the code.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::tools::{Budget, CallCtx, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

mod common;
use common::{vmat_c, vmat_c_claiming, write_vpk, BuildFile};
use hammer_mcp::tools::report_ready::{check_prop_materials_in, Status};
use hammer_mcp::vmap::{Document, Entity};

static SEQ: AtomicU64 = AtomicU64::new(0);
const MAP: &str = "maps/probe.vmap";

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    /// A tree with `probe.vmap` installed in the `mcp_test` addon.
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "report-ready-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let app = root.join("workshop/run/app");
        let tree = Tree { root, app };
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vmap/probe.vmap");
        let target = tree.content().join(MAP);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(source, target).unwrap();
        tree
    }

    fn content(&self) -> PathBuf {
        self.app.join("content/csgo_addons/mcp_test")
    }

    fn game(&self) -> PathBuf {
        self.app.join("game/csgo_addons/mcp_test")
    }

    fn vmap(&self) -> PathBuf {
        self.content().join(MAP)
    }

    fn vpk(&self) -> PathBuf {
        self.game().join("maps/probe.vpk")
    }

    /// The compiled map: a real (small) VPK at the path `compile_map` writes, with `files` in it.
    fn compile(&self, files: &[BuildFile]) {
        let dir = self.game().join("maps");
        let built = write_vpk(&dir, "probe", 2, files);
        std::fs::rename(built.dir, self.vpk()).unwrap();
    }

    /// `mtime` of the vmap and the vpk: the vpk is `vpk_after` seconds newer (negative = older).
    fn set_times(&self, vpk_after: i64) {
        let base = SystemTime::now() - Duration::from_secs(3600);
        let at = |secs: i64| {
            if secs >= 0 {
                base + Duration::from_secs(secs as u64)
            } else {
                base - Duration::from_secs((-secs) as u64)
            }
        };
        set_mtime(&self.vmap(), at(0));
        set_mtime(&self.vpk(), at(vpk_after));
    }

    /// The game VPKs: `csgo` holds each material as a real compiled material whose texture list is
    /// `textures`, plus the compiled textures named in `present`; `core` holds one unrelated file.
    fn game_paks_with(&self, materials: &[String], textures: &[&str], present: &[&str]) {
        let body = vmat_c(textures);
        let mut files: Vec<BuildFile> = materials
            .iter()
            .map(|m| BuildFile::loaded(&format!("{m}_c"), &body, b"", true))
            .collect();
        files.extend(present.iter().map(|t| BuildFile::preload_only(&format!("{t}_c"), b"")));
        write_vpk(&self.app.join("game/csgo"), "pak01", 2, &files);
        write_vpk(
            &self.app.join("game/core"),
            "pak01",
            2,
            &[BuildFile::preload_only("materials/other.vmat_c", b"")],
        );
    }

    /// Materials with no textures at all (nothing that can go missing).
    fn game_paks(&self, materials: &[String]) {
        self.game_paks_with(materials, &[], &[]);
    }

    fn write(&self, under: &Path, rel: &str, text: &str) {
        let path = under.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn set_mtime(path: &Path, time: SystemTime) {
    std::fs::File::options().write(true).open(path).unwrap().set_modified(time).unwrap();
}

/// The materials `probe.vmap`'s meshes use (what the tool will look for).
fn probe_materials(tree: &Tree) -> Vec<String> {
    let doc = hammer_mcp::vmap::load(&tree.vmap()).unwrap();
    let mut set = BTreeSet::new();
    for mesh in doc.meshes() {
        set.extend(mesh.face_materials.iter().chain(&mesh.materials).map(|m| m.to_ascii_lowercase()));
    }
    set.into_iter().filter(|m| !m.is_empty()).collect()
}

fn config(tree: &Tree, discovery: Option<&Path>) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let map = format!("z:\\app={}", tree.app.display());
    let discovery = discovery.map(|d| d.display().to_string());
    let mut given: Vec<&str> =
        vec!["--deadline-ms", "5000", "--tool-budget-ms", "45000", "--project-root", &root, "--path-map", &map];
    if let Some(d) = &discovery {
        given.extend(["--discovery", d]);
    }
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

fn ctx() -> CallCtx {
    CallCtx { budget: Budget::new(45_000, 5_000) }
}

async fn ready(state: &HammerState, args: Value) -> Result<Value, ToolError> {
    let mut args = args;
    args["map"] = json!(MAP);
    hammer_mcp::tools::report_ready::report_ready(state.clone(), ctx(), args).await.map(|o| o.json)
}

fn state(tree: &Tree) -> HammerState {
    HammerState::from_config(&config(tree, None)).unwrap()
}

fn check<'a>(out: &'a Value, name: &str) -> &'a Value {
    out["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!(name))
        .unwrap_or_else(|| panic!("no check {name} in {out}"))
}

#[tokio::test]
async fn a_map_with_nothing_checkable_is_incomplete_never_pass() {
    let tree = Tree::new("nothing");
    // no bridge: `saved` cannot be confirmed, so the only check that ran was skipped
    let out = ready(&state(&tree), json!({"checks": ["saved"]})).await.unwrap();
    assert_eq!(check(&out, "saved")["status"], json!("skipped"), "{out}");
    assert_eq!(out["verdict"], json!("incomplete"));
    assert_eq!(out["counts"], json!({"pass": 0, "warn": 0, "fail": 0, "skipped": 1, "total": 1}));
    assert!(out["note"].as_str().unwrap().contains("NOT a pass"));
    // the whole map profile without a bridge, a compile or game paks is not a pass either
    let out = ready(&state(&tree), json!({})).await.unwrap();
    assert_ne!(out["verdict"], json!("pass"), "{out}");
}

#[tokio::test]
async fn unsaved_edits_fail_saved_with_a_save_map_fix() {
    let tree = Tree::new("unsaved");
    let state = state(&tree);
    state.edits.lock().unwrap().since_save = 2;
    let out = ready(&state, json!({"checks": ["saved"]})).await.unwrap();
    let saved = check(&out, "saved");
    assert_eq!(saved["status"], json!("fail"), "{out}");
    assert_eq!(saved["fix"]["tool"], json!("save_map"));
    assert_eq!(out["verdict"], json!("fail"));
    // the fix is also the `next` step
    let o = hammer_mcp::tools::report_ready::report_ready(state.clone(), ctx(), json!({"map": MAP, "checks": ["saved"]}))
        .await
        .unwrap();
    assert_eq!(o.next.as_ref().unwrap()[0].tool, "save_map");
}

#[tokio::test]
async fn a_missing_or_older_vpk_fails_compiled_and_a_current_one_passes() {
    let tree = Tree::new("compiled");
    let state = state(&tree);
    // never compiled
    let out = ready(&state, json!({"checks": ["compiled"]})).await.unwrap();
    let c = check(&out, "compiled");
    assert_eq!(c["status"], json!("fail"));
    assert!(c["detail"].as_str().unwrap().contains("maps/probe.vpk"), "{c}");
    assert_eq!(c["fix"]["tool"], json!("compile_map"));
    assert_eq!(c["fix"]["args"], json!({"path": MAP}));
    // compiled BEFORE the last save: stale
    tree.compile(&[BuildFile::preload_only("materials/x.vmat_c", b"")]);
    tree.set_times(-60);
    let out = ready(&state, json!({"checks": ["compiled"]})).await.unwrap();
    let c = check(&out, "compiled");
    assert_eq!(c["status"], json!("fail"), "{out}");
    assert!(c["detail"].as_str().unwrap().contains("older than the .vmap"), "{c}");
    assert_eq!(out["verdict"], json!("fail"));
    // newer than the vmap: pass
    tree.set_times(60);
    let out = ready(&state, json!({"checks": ["compiled"]})).await.unwrap();
    assert_eq!(check(&out, "compiled")["status"], json!("pass"), "{out}");
    assert_eq!(out["verdict"], json!("pass"));
    // junk that is not a VPK fails too (the load would FATAL the engine)
    std::fs::write(tree.vpk(), vec![7u8; 4096]).unwrap();
    tree.set_times(60);
    let out = ready(&state, json!({"checks": ["compiled"]})).await.unwrap();
    assert_eq!(check(&out, "compiled")["status"], json!("fail"), "{out}");
    assert!(check(&out, "compiled")["detail"].as_str().unwrap().contains("signature"));
}

#[tokio::test]
async fn a_missing_material_fails_and_names_it() {
    let tree = Tree::new("materials");
    let state = state(&tree);
    let mut materials = probe_materials(&tree);
    assert!(!materials.is_empty(), "the probe map uses materials");
    // without the game paks the tool cannot tell a game material from a missing one
    let out = ready(&state, json!({"checks": ["materials"]})).await.unwrap();
    assert_eq!(check(&out, "materials")["status"], json!("skipped"), "{out}");
    // all there: pass
    tree.game_paks(&materials);
    let out = ready(&state, json!({"checks": ["materials"]})).await.unwrap();
    assert_eq!(check(&out, "materials")["status"], json!("pass"), "{out}");
    // one gone: fail, naming it. The default fix supplies the material; the replacement is the
    // alternative and repaints only the faces that use it
    let gone = materials.remove(0);
    tree.game_paks(&materials);
    let out = ready(&state, json!({"checks": ["materials"]})).await.unwrap();
    let c = check(&out, "materials");
    assert_eq!(c["status"], json!("fail"), "{out}");
    assert!(c["detail"].as_str().unwrap().contains(&gone), "{c}");
    assert_eq!(c["evidence"]["missing"][0]["material"], json!(gone));
    assert!(c["fix"]["do"].as_str().unwrap().contains(&gone), "supply it first: {c}");
    let alt = &c["fix"]["alternative"];
    assert_eq!(alt["tool"], json!("set_mesh_material"));
    let node = alt["args"]["nodeID"].as_i64().unwrap();
    let doc = hammer_mcp::vmap::load(&tree.vmap()).unwrap();
    let mesh = doc.mesh_by_id(node).unwrap();
    let bad: Vec<usize> = mesh
        .face_materials
        .iter()
        .enumerate()
        .filter(|(_, m)| m.eq_ignore_ascii_case(&gone))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(alt["args"]["faces"], json!(bad), "only the faces that use the bad material");
    // a .vmat source that was never compiled: the fix is to compile it
    tree.write(&tree.content(), &gone, "x");
    let out = ready(&state, json!({"checks": ["materials"]})).await.unwrap();
    let c = check(&out, "materials");
    assert_eq!(c["fix"]["tool"], json!("compile_asset"), "{c}");
    assert_eq!(c["fix"]["args"], json!({"path": gone}));
    // a compiled copy in the addon resolves it again (the source stays: it is now compiled)
    tree.write(&tree.game(), &format!("{gone}_c"), "");
    let out = ready(&state, json!({"checks": ["materials"]})).await.unwrap();
    assert_eq!(check(&out, "materials")["status"], json!("skipped"), "an empty _c has no texture list: {out}");
    // an uncompiled, unreferenced .vmat source in the addon is a note, not a warning
    tree.write(&tree.content(), "materials/mine/unused.vmat", "x");
    std::fs::write(tree.game().join(format!("{gone}_c")), vmat_c(&[])).unwrap();
    let out = ready(&state, json!({"checks": ["materials"]})).await.unwrap();
    let c = check(&out, "materials");
    assert_eq!(c["status"], json!("pass"), "{out}");
    assert!(c["detail"].as_str().unwrap().contains("materials/mine/unused.vmat"), "{c}");
}

#[tokio::test]
async fn a_material_whose_texture_is_missing_fails_even_though_the_vmat_c_exists() {
    let tree = Tree::new("textures");
    let state = state(&tree);
    let materials = probe_materials(&tree);
    let tex = "materials/cbble/cobblestone_ground_6/cobblestone_color_psd_be5d6a7d.vtex";
    // the materials exist; their texture is in neither pak: renders magenta
    tree.game_paks_with(&materials, &[tex], &[]);
    let out = ready(&state, json!({"checks": ["materials"]})).await.unwrap();
    let c = check(&out, "materials");
    assert_eq!(c["status"], json!("fail"), "{out}");
    assert!(c["detail"].as_str().unwrap().contains(tex), "{c}");
    assert!(c["detail"].as_str().unwrap().contains(&materials[0]), "names the material too: {c}");
    assert_eq!(c["evidence"]["missingTextures"][0]["texture"], json!(tex));
    assert!(c["fix"]["do"].as_str().unwrap().contains(tex));
    // the texture present in the csgo pak: pass
    tree.game_paks_with(&materials, &[tex], &[tex]);
    let out = ready(&state, json!({"checks": ["materials"]})).await.unwrap();
    assert_eq!(check(&out, "materials")["status"], json!("pass"), "{out}");
    // materials that cannot be read (preload-only, no RERL): resolves, textures NOT checked: skipped
    let blank: Vec<BuildFile> = materials.iter().map(|m| BuildFile::preload_only(&format!("{m}_c"), b"")).collect();
    write_vpk(&tree.app.join("game/csgo"), "pak01", 2, &blank);
    let out = ready(&state, json!({"checks": ["materials"]})).await.unwrap();
    let c = check(&out, "materials");
    assert_eq!(c["status"], json!("skipped"), "{out}");
    assert!(c["detail"].as_str().unwrap().contains("not a pass"), "{c}");
}

#[tokio::test]
async fn materials_resolve_case_insensitively_on_disk() {
    let tree = Tree::new("case");
    let state = state(&tree);
    let mut materials = probe_materials(&tree);
    let gone = materials.remove(0);
    tree.game_paks(&materials);
    // the compiled file is on disk under another case than the map spells it
    let mixed: String = gone
        .split('/')
        .map(|part| {
            let mut c = part.chars();
            c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("/");
    assert_ne!(mixed, gone);
    let path = tree.game().join(format!("{mixed}_c"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vmat_c(&[])).unwrap();
    let out = ready(&state, json!({"checks": ["materials"]})).await.unwrap();
    assert_eq!(check(&out, "materials")["status"], json!("pass"), "{out}");
}

#[tokio::test]
async fn the_probe_map_has_spawns_and_objectives() {
    let tree = Tree::new("spawns");
    let out = ready(&state(&tree), json!({"checks": ["spawns", "spawn_in_mover_path", "objectives"]}))
        .await
        .unwrap();
    for name in ["spawns", "spawn_in_mover_path", "objectives"] {
        assert_eq!(check(&out, name)["status"], json!("pass"), "{name}: {out}");
    }
    assert_eq!(out["verdict"], json!("pass"));
    assert!(check(&out, "objectives")["detail"].as_str().unwrap().contains("func_bomb_target"));
}

#[tokio::test]
async fn objectives_follow_the_mode_the_caller_names() {
    let tree = Tree::new("objmode");
    let st = state(&tree);
    let run = |mode: Value| {
        let st = st.clone();
        async move { ready(&st, json!({"checks": ["objectives"], "mode": mode})).await }
    };
    let none = run(json!("none")).await.unwrap();
    assert_eq!(check(&none, "objectives")["status"], json!("pass"), "{none}");
    let defuse = run(json!("defuse")).await.unwrap();
    assert_ne!(check(&defuse, "objectives")["status"], json!("fail"), "the probe map has bomb sites: {defuse}");
    let hostage = run(json!("hostage")).await.unwrap();
    assert_eq!(check(&hostage, "objectives")["status"], json!("fail"), "no hostages / rescue zone: {hostage}");
    assert!(run(json!("deathmatch")).await.is_err(), "an unknown mode is refused");
}

#[tokio::test]
async fn the_release_profile_skips_metamod_so_it_is_never_a_pass() {
    let tree = Tree::new("release");
    let materials = probe_materials(&tree);
    tree.game_paks(&materials);
    tree.compile(&[BuildFile::preload_only("materials/x.vmat_c", b"")]);
    tree.set_times(60);
    let out = ready(&state(&tree), json!({"profile": "release"})).await.unwrap();
    let names: Vec<&str> = out["checks"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        ["saved", "compiled", "materials", "prop_materials", "spawns", "spawn_in_mover_path", "objectives",
         "loose_compiled", "debug_patterns", "no_metamod_load", "test_cvars"]
    );
    let metamod = check(&out, "no_metamod_load");
    assert_eq!(metamod["status"], json!("skipped"));
    assert!(metamod["detail"].as_str().unwrap().contains("Metamod"));
    assert!(metamod["fix"]["do"].is_string(), "the manual step is named: {metamod}");
    assert_eq!(check(&out, "test_cvars")["status"], json!("skipped"), "no game is running");
    assert_ne!(out["verdict"], json!("pass"));
    assert_eq!(out["profile"], json!("release"));
}

#[tokio::test]
async fn debug_leftovers_and_loose_compiled_files_warn_with_locations() {
    let tree = Tree::new("release-files");
    // a script with a debug flag and a cheat command, and a particle the script references that
    // lives loose next to the vpk instead of inside it
    tree.write(
        &tree.content(),
        "scripts/config.ts",
        "export const CFG = {\n  debug: true,\n};\nRegisterCheatCommand(\"ti_win\");\nconst fx = \"particles/mine/boom.vpcf\";\nmyFlag = on\n",
    );
    // Valve's declaration file and tsconfig name the same things on purpose: not leftovers
    tree.write(&tree.content(), "maps/scripts/point_script.d.ts", "declare function RegisterCheatCommand(n: string): void; // sv_cheats\n");
    tree.write(&tree.content(), "maps/scripts/tsconfig.json", "{\"note\": \"debug: true sv_cheats\"}\n");
    // a script shipped loose under maps/scripts (not the map's own output): referenced, so it warns
    tree.write(&tree.content(), "scripts/boot.ts", "load(\"maps/scripts/boot.vjs\");\n");
    tree.write(&tree.game(), "maps/scripts/boot.vjs_c", "x");
    // the map's own build output under maps/probe/ is skipped
    tree.write(&tree.game(), "maps/probe/world.vmat_c", "x");
    tree.write(&tree.game(), "particles/mine/boom.vpcf_c", "x");
    tree.write(&tree.game(), "particles/mine/unused.vpcf_c", "x");
    tree.compile(&[BuildFile::preload_only("materials/x.vmat_c", b"")]);
    tree.set_times(60);
    let state = state(&tree);
    let out = ready(
        &state,
        json!({"profile": "release", "checks": ["debug_patterns", "loose_compiled"], "debugPatterns": ["myFlag\\s*=\\s*on"]}),
    )
    .await
    .unwrap();
    let debug = check(&out, "debug_patterns");
    assert_eq!(debug["status"], json!("warn"), "{out}");
    let hits = debug["evidence"]["hits"].as_array().unwrap();
    let at = |line: u64| hits.iter().any(|h| h["line"] == json!(line) && h["file"].as_str().unwrap().ends_with("config.ts"));
    assert!(at(2), "debug: true at config.ts:2 in {hits:?}");
    assert!(at(4), "RegisterCheatCommand at config.ts:4");
    assert!(at(6), "the caller's pattern at config.ts:6");
    assert!(hits.iter().all(|h| !h["file"].as_str().unwrap().ends_with(".d.ts")), "{hits:?}");
    assert!(hits.iter().all(|h| !h["file"].as_str().unwrap().contains("tsconfig")), "{hits:?}");
    assert!(debug["fix"]["do"].is_string());
    let loose = check(&out, "loose_compiled");
    assert_eq!(loose["status"], json!("warn"), "{out}");
    assert_eq!(loose["evidence"]["looseReferenced"], json!(["maps/scripts/boot.vjs_c", "particles/mine/boom.vpcf_c"]));
    assert_eq!(loose["evidence"]["looseOther"], json!(["particles/mine/unused.vpcf_c"]));
    // a vpk that carries the referenced file passes
    tree.compile(&[
        BuildFile::preload_only("particles/mine/boom.vpcf_c", b""),
        BuildFile::preload_only("maps/scripts/boot.vjs_c", b""),
    ]);
    tree.set_times(60);
    let out = ready(&state, json!({"checks": ["loose_compiled"]})).await.unwrap();
    assert_eq!(check(&out, "loose_compiled")["status"], json!("pass"), "{out}");
    // no map vpk at all: nothing to compare against
    std::fs::remove_file(tree.vpk()).unwrap();
    let out = ready(&state, json!({"checks": ["loose_compiled"]})).await.unwrap();
    assert_eq!(check(&out, "loose_compiled")["status"], json!("skipped"), "{out}");
    assert_eq!(check(&out, "loose_compiled")["fix"]["args"], json!({"path": MAP}), "compile_map needs the path");
}

#[tokio::test]
async fn unknown_check_names_are_an_error_with_suggestions() {
    let tree = Tree::new("names");
    let err = ready(&state(&tree), json!({"checks": ["spawn"]})).await.unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(err.data["didYouMean"].as_array().unwrap().contains(&json!("spawns")), "{err:?}");
    assert!(err.data["valid"].as_array().unwrap().len() == 11);
}

#[tokio::test]
async fn a_clean_document_a_current_vpk_and_resolved_materials_pass() {
    use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
    let tree = Tree::new("pass");
    tree.game_paks(&probe_materials(&tree));
    tree.compile(&[BuildFile::preload_only("materials/x.vmat_c", b"")]);
    tree.set_times(60);
    let mb = MockBridge::builder()
        .doc(Some(make_doc("z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap", Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .start()
        .await
        .unwrap();
    let state = HammerState::from_config(&config(&tree, Some(&mb.discovery_path))).unwrap();
    // no `map`: the active document
    let out = hammer_mcp::tools::report_ready::report_ready(state.clone(), ctx(), json!({})).await.unwrap().json;
    assert_eq!(out["verdict"], json!("pass"), "{out}");
    assert_eq!(out["counts"]["pass"], json!(7));
    assert_eq!(out["map"], json!(MAP));
    // Hammer's own dirty flag fails `saved` even with no MCP edits
    let dirty = MockBridge::builder()
        .doc(Some(make_doc("z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap", Some(true))))
        .hammer_nodes(vec![MockNode::world()])
        .start()
        .await
        .unwrap();
    let state = HammerState::from_config(&config(&tree, Some(&dirty.discovery_path))).unwrap();
    let out = hammer_mcp::tools::report_ready::report_ready(state, ctx(), json!({})).await.unwrap().json;
    assert_eq!(out["verdict"], json!("fail"));
    assert_eq!(check(&out, "saved")["fix"]["tool"], json!("save_map"));
}

/// Real install: the `RERL` reader on a compiled material from the csgo pak (read-only).
/// `cargo test -p hammer-mcp --test report_ready -- --ignored --nocapture`
#[test]
#[ignore]
fn real_tree_cobblestone_material_lists_textures_that_are_in_no_pak() {
    let app = std::env::var("CS2_APP_DIR")
        .unwrap_or_else(|_| {
            let root = std::env::var("CS2_MCP_PROJECT_ROOT").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/..").to_string());
            format!("{root}/workshop/run/app")
        });
    let csgo = hammer_mcp::vpk::VpkDir::open(&Path::new(&app).join("game/csgo/pak01_dir.vpk")).unwrap();
    let core = hammer_mcp::vpk::VpkDir::open(&Path::new(&app).join("game/core/pak01_dir.vpk")).unwrap();
    let bytes = csgo.read("materials/dev/cobblestone_001.vmat_c").unwrap();
    let refs = hammer_mcp::resource::Resource::parse(&bytes).unwrap().external_refs().expect("RERL block");
    println!("refs: {refs:#?}");
    let textures: Vec<&String> = refs.iter().filter(|r| r.ends_with(".vtex")).collect();
    assert!(!textures.is_empty());
    let missing: Vec<_> = textures
        .iter()
        .filter(|t| !csgo.contains(&format!("{t}_c")) && !core.contains(&format!("{t}_c")))
        .collect();
    println!("missing: {missing:?}");
    assert!(!missing.is_empty(), "FIXME 12: the textures are in neither pak");
}

/// A mock whose engine has a map up and answers `cvar.get` with `cvars`.
async fn mock_with_game(cvars: Value) -> cs2_mcp_common::mock::MockBridge {
    use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
    MockBridge::builder()
        .doc(Some(make_doc("z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap", Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .responder_value("game.local", json!({"inGame": true, "map": "probe"}))
        .responder_value("cvar.get", json!({"cvars": cvars}))
        .start()
        .await
        .unwrap()
}

#[tokio::test]
async fn test_cvars_reads_sv_cheats_and_a_null_value_is_not_off() {
    let tree = Tree::new("cvars");
    let run = |cvars: Value, extra: Value| {
        let tree = &tree;
        async move {
            let mb = mock_with_game(cvars).await;
            let state = HammerState::from_config(&config(tree, Some(&mb.discovery_path))).unwrap();
            let out = ready(&state, json!({"checks": ["test_cvars"], "debugPatterns": extra})).await.unwrap();
            check(&out, "test_cvars").clone()
        }
    };
    let c = run(json!([{"name": "sv_cheats", "found": true, "value": "1"}]), json!([])).await;
    assert_eq!(c["status"], json!("warn"), "{c}");
    assert_eq!(c["fix"]["tool"], json!("console_exec"));
    let c = run(json!([{"name": "sv_cheats", "found": true, "value": "0"}]), json!([])).await;
    assert_eq!(c["status"], json!("pass"), "{c}");
    // null / missing / not found: unknown, not off
    for entry in [json!({"name": "sv_cheats", "found": true, "value": null}), json!({"name": "sv_cheats", "found": false})] {
        let c = run(json!([entry]), json!([])).await;
        assert_eq!(c["status"], json!("skipped"), "{c}");
    }
    assert_eq!(run(json!([]), json!([])).await["status"], json!("skipped"));
    // a cvar named through debugPatterns that is set warns even with sv_cheats off
    let cvars = json!([{"name": "sv_cheats", "found": true, "value": "0"}, {"name": "ti_debug", "found": true, "value": "1"}]);
    let c = run(cvars, json!(["ti_debug"])).await;
    assert_eq!(c["status"], json!("warn"), "{c}");
    assert!(c["detail"].as_str().unwrap().contains("ti_debug 1"), "{c}");
}

#[tokio::test]
async fn a_running_compile_of_the_map_is_reported_instead_of_suggesting_another() {
    use hammer_mcp::tools::compile_map::{CompileJob, CompileMode, JobState};
    let tree = Tree::new("running");
    let state = state(&tree);
    state.compile_jobs.insert(CompileJob {
        id: "c9".into(),
        tag: hammer_mcp::tools::compile_map::tag_value("c9"),
        state: JobState::Running,
        mode: CompileMode::Full,
        path: MAP.into(),
        addon: state.addon(),
        container: "c".into(),
        log: "/log".into(),
        vpk: Vec::new(),
        vmap_before: None,
        started: std::time::Instant::now(),
        deadline: Duration::from_secs(1500),
        attempts: 1,
        summary: None,
        ok_line: None,
        reason: None,
        cleanup: None,
    });
    let out = ready(&state, json!({"checks": ["compiled"]})).await.unwrap();
    let c = check(&out, "compiled");
    assert_eq!(c["status"], json!("skipped"), "{out}");
    assert!(c["detail"].as_str().unwrap().contains("c9"));
    assert_eq!(c["fix"]["tool"], json!("job_status"));
    assert_eq!(c["fix"]["args"]["jobId"], json!("c9"));
}

// ---- every emitted fix {tool, args} is a valid call of that tool --------------------------

/// The subset of JSON Schema the tool schemas use.
fn schema_errors(schema: &Value, v: &Value, at: &str, errs: &mut Vec<String>) {
    if let Some(types) = schema.get("type") {
        let ok = |t: &str| match t {
            "string" => v.is_string(),
            "integer" => v.is_i64() || v.is_u64(),
            "number" => v.is_number(),
            "boolean" => v.is_boolean(),
            "array" => v.is_array(),
            "object" => v.is_object(),
            "null" => v.is_null(),
            _ => true,
        };
        let allowed: Vec<&str> = match types {
            Value::String(t) => vec![t.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        if !allowed.is_empty() && !allowed.iter().any(|t| ok(t)) {
            errs.push(format!("{at}: {v} is not {allowed:?}"));
            return;
        }
    }
    if let Some(e) = schema.get("enum").and_then(Value::as_array) {
        if !e.contains(v) {
            errs.push(format!("{at}: {v} not in {e:?}"));
        }
    }
    if let Some(n) = v.as_f64() {
        if schema.get("minimum").and_then(Value::as_f64).is_some_and(|m| n < m) {
            errs.push(format!("{at}: {n} below minimum"));
        }
        if schema.get("maximum").and_then(Value::as_f64).is_some_and(|m| n > m) {
            errs.push(format!("{at}: {n} above maximum"));
        }
    }
    if let Some(s) = v.as_str() {
        if schema.get("minLength").and_then(Value::as_u64).is_some_and(|m| (s.chars().count() as u64) < m) {
            errs.push(format!("{at}: string too short"));
        }
    }
    if let Some(a) = v.as_array() {
        if schema.get("minItems").and_then(Value::as_u64).is_some_and(|m| (a.len() as u64) < m) {
            errs.push(format!("{at}: too few items"));
        }
        if schema.get("maxItems").and_then(Value::as_u64).is_some_and(|m| (a.len() as u64) > m) {
            errs.push(format!("{at}: too many items"));
        }
        if let Some(items) = schema.get("items") {
            for (i, item) in a.iter().enumerate() {
                schema_errors(items, item, &format!("{at}[{i}]"), errs);
            }
        }
    }
    if let Some(o) = v.as_object() {
        for r in schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !o.contains_key(r) {
                errs.push(format!("{at}: missing required {r}"));
            }
        }
        let props = schema.get("properties").and_then(Value::as_object);
        for (k, x) in o {
            match props.and_then(|p| p.get(k)) {
                Some(s) => schema_errors(s, x, &format!("{at}.{k}"), errs),
                None if schema.get("additionalProperties") == Some(&json!(false)) => {
                    errs.push(format!("{at}: unknown property {k}"))
                }
                None => {}
            }
        }
    }
}

fn assert_valid_call(registry: &cs2_mcp_common::tools::Registry<HammerState>, call: &Value, what: &str) {
    let tool = call["tool"].as_str().unwrap_or_else(|| panic!("{what}: no tool in {call}"));
    let spec = registry.spec(tool).unwrap_or_else(|| panic!("{what}: {tool} is not a registered tool"));
    let mut errs = Vec::new();
    schema_errors(&spec.input_schema, &call["args"], tool, &mut errs);
    assert!(errs.is_empty(), "{what}: {tool} {} is not a valid call: {errs:?}", call["args"]);
}

#[tokio::test]
async fn every_fix_the_tool_emits_is_a_valid_call_of_a_registered_tool() {
    use hammer_mcp::tools::report_ready::{check_spawn_in_mover_path, check_spawns};
    let registry = hammer_mcp::build_registry();
    let mut fixes: Vec<(String, Value)> = Vec::new();
    let mut take = |what: &str, fix: Option<Value>| {
        let fix = fix.unwrap_or_else(|| panic!("{what}: no fix"));
        if fix.get("tool").is_some() {
            fixes.push((what.to_string(), fix.clone()));
        }
        if let Some(alt) = fix.get("alternative") {
            fixes.push((format!("{what} (alternative)"), alt.clone()));
        }
    };
    // state-driven checks on the probe tree
    let tree = Tree::new("fixes");
    let st = state(&tree);
    st.edits.lock().unwrap().since_save = 1;
    let out = ready(&st, json!({"profile": "release"})).await.unwrap();
    for c in out["checks"].as_array().unwrap() {
        if matches!(c["status"].as_str(), Some("fail") | Some("warn") | Some("skipped")) && c.get("fix").is_some() {
            take(c["name"].as_str().unwrap(), Some(c["fix"].clone()));
        }
    }
    // a missing, uncompiled and a missing-texture material
    let mut materials = probe_materials(&tree);
    let gone = materials.remove(0);
    tree.game_paks(&materials);
    tree.write(&tree.content(), &gone, "x");
    let out = ready(&st, json!({"checks": ["materials"]})).await.unwrap();
    take("materials/source", Some(check(&out, "materials")["fix"].clone()));
    tree.game_paks_with(&probe_materials(&tree), &["materials/t/a.vtex"], &[]);
    let out = ready(&st, json!({"checks": ["materials"]})).await.unwrap();
    take("materials/texture", Some(check(&out, "materials")["fix"].clone()));
    // a running compile, sv_cheats on
    let mb = mock_with_game(json!([{"name": "sv_cheats", "found": true, "value": "1"}])).await;
    let live = HammerState::from_config(&config(&tree, Some(&mb.discovery_path))).unwrap();
    let out = ready(&live, json!({"checks": ["test_cvars"]})).await.unwrap();
    take("test_cvars", Some(check(&out, "test_cvars")["fix"].clone()));
    // spawns and a mover lane on synthetic documents
    let one = hammer_mcp::vmap::Document::from_entities(
        PathBuf::from("m.vmap"),
        vec![hammer_mcp::vmap::Entity {
            guid: "g".into(), node_class: "CMapEntity".into(), node_id: Some(5), reference_id: None,
            classname: "info_player_terrorist".into(), targetname: None, origin: Some([0.0, 0.0, 0.0]),
            angles: None, scales: None, keyvalues: vec![], connections: vec![],
        }],
    );
    take("spawns", check_spawns(&one, MAP).fix);
    let _ = check_spawn_in_mover_path; // exercised below through a hit
    let mover = hammer_mcp::vmap::Entity {
        guid: "m".into(), node_class: "CMapEntity".into(), node_id: Some(10), reference_id: None,
        classname: "func_movelinear".into(), targetname: None, origin: Some([0.0; 3]), angles: None, scales: None,
        keyvalues: vec![("movedistance".into(), json!("200"))], connections: vec![],
    };
    let mut spawn = one.entities()[0].clone();
    spawn.origin = Some([150.0, 50.0, 0.0]);
    let mesh = hammer_mcp::vmap::mesh::MeshRecord {
        id: Some(100), origin: None, angles: None, scales: None, vertices: vec![[0.0; 3], [100.0; 3]],
        faces: vec![], face_materials: vec![], materials: vec![], ancestors: None,
    };
    let doc = hammer_mcp::vmap::Document::from_parts(PathBuf::from("t.vmap"), vec![mover, spawn], vec![mesh])
        .with_children([(10, vec![("CMapMesh".to_string(), Some(100))])].into_iter().collect());
    take("spawn_in_mover_path", check_spawn_in_mover_path(&doc).fix);
    assert!(fixes.len() >= 8, "{} fixes collected: {:?}", fixes.len(), fixes.iter().map(|(w, _)| w).collect::<Vec<_>>());
    for (what, fix) in &fixes {
        assert_valid_call(&registry, fix, what);
    }
}

/// A texture list cut short (the header claims more entries than the block holds) is "not
/// checked", never read as the textures it did list or as none: skipped, with the material named.
#[tokio::test]
async fn a_truncated_texture_list_is_not_a_pass() {
    let tree = Tree::new("truncated-rerl");
    let state = state(&tree);
    let materials = probe_materials(&tree);
    let tex = "materials/t/a.vtex";
    let cut = vmat_c_claiming(&[tex], 3);
    let files: Vec<BuildFile> = materials
        .iter()
        .map(|m| BuildFile::loaded(&format!("{m}_c"), &cut, b"", true))
        .chain([BuildFile::preload_only(&format!("{tex}_c"), b"")])
        .collect();
    write_vpk(&tree.app.join("game/csgo"), "pak01", 2, &files);
    write_vpk(&tree.app.join("game/core"), "pak01", 2, &[BuildFile::preload_only("materials/other.vmat_c", b"")]);
    let out = ready(&state, json!({"checks": ["materials"]})).await.unwrap();
    let c = check(&out, "materials");
    assert_eq!(c["status"], json!("skipped"), "{out}");
    assert!(c["detail"].as_str().unwrap().contains("not a pass"), "{c}");
    let unchecked = c["evidence"]["texturesUnchecked"][0].as_str().unwrap();
    assert!(unchecked.contains("malformed at entry 1 of 3"), "{unchecked}");
}

/// A compiled resource with no `RERL` block references nothing (the compiler omits an empty list:
/// texture-less shader materials such as `materials/dev/black_cheap.vmat_c` in the real paks), so its
/// texture list is empty, not unknown; a list that is present but cut short stays an error.
#[test]
fn a_resource_without_an_rerl_block_references_nothing() {
    let none = common::model_container(&[("DATA", &[0u8; 16])]);
    let res = hammer_mcp::resource::Resource::parse(&none).unwrap();
    assert_eq!(res.external_refs_checked(), Ok(Vec::new()));
    let cut = common::vmat_c_claiming(&["materials/x/y_color.vtex"], 3);
    assert!(hammer_mcp::resource::Resource::parse(&cut).unwrap().external_refs_checked().is_err());
}

// ------------------------------------------------------------------------------ prop_materials

fn prop(id: i64, name: &str, class: &str, model: &str) -> Entity {
    Entity {
        guid: format!("g{id}"),
        node_class: "CMapEntity".into(),
        node_id: Some(id),
        reference_id: None,
        classname: class.into(),
        targetname: (!name.is_empty()).then(|| name.to_string()),
        origin: None,
        angles: None,
        scales: None,
        keyvalues: vec![("model".to_string(), json!(model))],
        connections: Vec::new(),
    }
}

fn doc_of(props: Vec<Entity>) -> Document {
    Document::from_entities(PathBuf::from("maps/x.vmap"), props)
}

impl Tree {
    /// An addon model: compiled `models/x/x.vmdl_c` whose reference list is `refs`.
    fn addon_file(&self, rel: &str, bytes: &[u8]) {
        let path = self.game().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// The csgo pak holds `files` (path, bytes); the core pak one unrelated file.
    fn paks(&self, files: &[(&str, Vec<u8>)]) {
        let files: Vec<BuildFile> = files
            .iter()
            .map(|(path, data)| BuildFile::loaded(path, data, b"", true))
            .collect();
        write_vpk(&self.app.join("game/csgo"), "pak01", 2, &files);
        write_vpk(&self.app.join("game/core"), "pak01", 2, &[BuildFile::preload_only("materials/other.vmat_c", b"")]);
    }

    fn props(&self, props: Vec<Entity>) -> hammer_mcp::tools::report_ready::Check {
        check_prop_materials_in(&self.app, "mcp_test", &doc_of(props), None)
    }
}

const MODEL: &str = "models/mine/crate/crate.vmdl";
const MAT: &str = "materials/models/mine/crate.vmat";
const TEX: &str = "materials/models/mine/crate_color.vtex";

/// A complete addon model: vmdl_c -> vmat_c -> vtex_c, all loose in the addon game dir.
fn complete_model(tree: &Tree) {
    tree.addon_file(&format!("{MODEL}_c"), &vmat_c(&[MAT]));
    tree.addon_file(&format!("{MAT}_c"), &vmat_c(&[TEX]));
    tree.addon_file(&format!("{TEX}_c"), b"tex");
    tree.paks(&[]);
}

#[test]
fn prop_materials_passes_when_model_material_and_texture_resolve() {
    let tree = Tree::new("pm-pass");
    complete_model(&tree);
    let c = tree.props(vec![prop(1, "a", "prop_static", MODEL), prop(2, "", "prop_dynamic", &MODEL.to_uppercase())]);
    assert_eq!(c.status, Status::Pass, "{}", c.detail);
    assert!(c.detail.contains("1 model(s) of 2 prop(s)"), "{}", c.detail);
}

#[test]
fn prop_materials_with_no_model_keyvalue_is_a_pass_not_a_skip() {
    let tree = Tree::new("pm-none");
    tree.paks(&[]);
    let c = tree.props(vec![prop(1, "a", "info_target", "")]);
    assert_eq!(c.status, Status::Pass);
    assert!(c.detail.contains("nothing to check"));
}

#[test]
fn prop_materials_fails_on_a_model_that_is_not_compiled() {
    let tree = Tree::new("pm-nomodel");
    tree.paks(&[]);
    // a source in the addon: the fix is model_compile
    tree.write(&tree.content(), MODEL, "x");
    let c = tree.props(vec![prop(7, "crate1", "prop_static", MODEL)]);
    assert_eq!(c.status, Status::Fail);
    assert_eq!(hammer_mcp::tools::report_ready::verdict(std::slice::from_ref(&c)), "fail");
    assert!(c.detail.contains("node 7 'crate1'") && c.detail.contains(MODEL) && c.detail.contains("model not compiled"), "{}", c.detail);
    let fix = c.fix.unwrap();
    assert_eq!(fix["tool"], json!("model_compile"));
    assert_eq!(fix["args"], json!({"path": MODEL}));
    // no source anywhere: the path is wrong
    let c = tree.props(vec![prop(8, "", "prop_static", "models/props/typo.vmdl")]);
    assert_eq!(c.status, Status::Fail);
    let fix = c.fix.unwrap();
    assert!(fix["do"].as_str().unwrap().contains("model path models/props/typo.vmdl is wrong"), "{fix}");
}

#[test]
fn prop_materials_fails_on_a_missing_vmat_c_and_a_missing_vtex_c() {
    let tree = Tree::new("pm-missing");
    complete_model(&tree);
    std::fs::remove_file(tree.game().join(format!("{TEX}_c"))).unwrap();
    let c = tree.props(vec![prop(3, "", "prop_physics", MODEL)]);
    assert_eq!(c.status, Status::Fail);
    assert!(c.detail.contains("node 3") && c.detail.contains(&format!("texture {TEX} missing")) && c.detail.contains(MAT), "{}", c.detail);
    assert!(c.fix.as_ref().unwrap()["do"].as_str().unwrap().contains("model_import"), "{:?}", c.fix);
    assert_eq!(c.evidence.as_ref().unwrap()["failing"][0]["props"][0]["nodeID"], json!(3));
    // the material itself gone; its source in the addon makes the fix compile_asset
    std::fs::remove_file(tree.game().join(format!("{MAT}_c"))).unwrap();
    let c = tree.props(vec![prop(3, "", "prop_physics", MODEL)]);
    assert_eq!(c.status, Status::Fail);
    assert!(c.detail.contains(&format!("material {MAT} missing")), "{}", c.detail);
    assert!(c.fix.as_ref().unwrap()["do"].is_string());
    tree.write(&tree.content(), MAT, "x");
    let c = tree.props(vec![prop(3, "", "prop_physics", MODEL)]);
    assert_eq!(c.fix.unwrap()["tool"], json!("compile_asset"));
}

#[test]
fn prop_materials_checks_shipped_models_to_the_model_level_only() {
    let tree = Tree::new("pm-shipped");
    // the shipped model references a material the paks do NOT hold: not looked at
    tree.paks(&[("models/props/crate.vmdl_c", vmat_c(&["materials/props/not_in_paks.vmat"]))]);
    let c = tree.props(vec![prop(1, "", "prop_static", "models/props/crate.vmdl")]);
    assert_eq!(c.status, Status::Pass, "{}", c.detail);
    assert!(c.detail.contains("1 shipped model(s) checked to the model only"), "{}", c.detail);
    // a model path that is in no pak fails
    let c = tree.props(vec![prop(2, "", "prop_static", "models/props/crat.vmdl")]);
    assert_eq!(c.status, Status::Fail);
    // a model in the map vpk is checked in full (it is the addon's)
    let dir = tree.game().join("maps");
    let built = write_vpk(&dir, "probe", 2, &[BuildFile::loaded("models/in/vpk.vmdl_c", &vmat_c(&["materials/in/gone.vmat"]), b"", true)]);
    let c = check_prop_materials_in(&tree.app, "mcp_test", &doc_of(vec![prop(5, "", "prop_static", "models/in/vpk.vmdl")]), Some(&built.dir));
    assert_eq!(c.status, Status::Fail);
    assert!(c.detail.contains("material materials/in/gone.vmat missing"), "{}", c.detail);
}

#[test]
fn prop_materials_is_skipped_never_pass_when_it_cannot_look() {
    let tree = Tree::new("pm-skip");
    complete_model(&tree);
    // an unreadable compiled model (not a resource): cannot be said
    tree.addon_file("models/mine/bad/bad.vmdl_c", b"junk junk junk junk");
    let c = tree.props(vec![prop(1, "", "prop_static", MODEL), prop(2, "", "prop_static", "models/mine/bad/bad.vmdl")]);
    assert_eq!(c.status, Status::Skipped, "{}", c.detail);
    // a model not in the addon with the game paks unreadable
    std::fs::remove_dir_all(tree.app.join("game/csgo")).unwrap();
    let c = tree.props(vec![prop(3, "", "prop_static", "models/props/crate.vmdl")]);
    assert_eq!(c.status, Status::Skipped, "{}", c.detail);
}

#[tokio::test]
async fn prop_materials_is_in_the_map_profile_and_the_verdict_counts_it() {
    let tree = Tree::new("pm-verdict");
    let state = state(&tree);
    tree.paks(&[]);
    let out = ready(&state, json!({"checks": ["prop_materials"]})).await.unwrap();
    // the probe map's entities name no model, so the check passes and the verdict follows it
    assert_eq!(check(&out, "prop_materials")["status"], json!("pass"), "{out}");
    assert_eq!(out["verdict"], json!("pass"), "{out}");
    assert_eq!(out["counts"]["total"], json!(1));
    let full = ready(&state, json!({})).await.unwrap();
    let names: Vec<&str> = full["checks"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(&names[..4], ["saved", "compiled", "materials", "prop_materials"]);
}
