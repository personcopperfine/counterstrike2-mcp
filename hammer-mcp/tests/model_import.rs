//! `model_import` tests (I1): the pure conversion (`hammer_mcp::import`) against in-test glTF/GLB
//! fixtures (`tests/common/gltf_fixture.rs`), then the tool against a temp content tree under
//! `CARGO_TARGET_TMPDIR` (never /tmp) with a fake podman for the compile. No game needed.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::import::{self, ImportError, ImportOptions, Kind, Physics};
use hammer_mcp::state::HammerState;
use hammer_mcp::vmdl::Model;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;
use common::gltf_fixture::{Fixture, Storage, TINY_PNG};

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "model-import-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("tmp tree");
        let app = root.join("workshop/run/app");
        std::fs::create_dir_all(app.join("content/csgo_addons/mcp_test")).unwrap();
        Tree { root, app }
    }
    fn addon(&self) -> PathBuf {
        self.app.join("content/csgo_addons/mcp_test")
    }
    /// A host directory for source files, outside the addon.
    fn src(&self) -> PathBuf {
        self.root.join("src")
    }
    fn compile_output(&self, rel: &str) -> PathBuf {
        self.app.join("game/csgo_addons/mcp_test").join(format!("{rel}_c"))
    }
    /// Fake podman: `ps` lists a container, `exec` touches the compiled output (`ok`) or prints a
    /// `[FAIL]` line and a failed summary (`fail`).
    fn podman(&self, kind: &str) -> PathBuf {
        let dir = self.root.join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let body = if kind == "ok" {
            r#"#!/bin/bash
APP='__APP__'
case "$1" in
  ps) printf '[{"Names":["cs2tools"],"State":"running"}]' ;;
  exec)
    last="${!#}"; rel="${last##* -i }"; rel="${rel#/app/content/}"
    out="$APP/game/${rel}_c"; mkdir -p "$(dirname "$out")"; touch "$out"
    echo "OK: 1 compiled, 0 failed, 0 skipped, 0m:01s" ;;
esac
"#
        } else {
            r#"#!/bin/bash
case "$1" in
  ps) printf '[{"Names":["cs2tools"],"State":"running"}]' ;;
  exec) echo "[FAIL] dependency failed: materials/models/x/x.vmat"; echo "OK: 0 compiled, 1 failed, 0 skipped, 0m:01s" ;;
esac
"#
        }
        .replace("__APP__", &self.app.display().to_string());
        let script = dir.join(format!("podman-{kind}"));
        std::fs::write(&script, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }
    fn server(&self, podman: Option<&Path>) -> McpServer<HammerState> {
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: CommonConfig,
        }
        let root = self.root.display().to_string();
        let map = format!("z:\\app={}", self.app.display());
        let mut args: Vec<String> = ["--deadline-ms", "5000", "--tool-budget-ms", "45000", "--project-root", &root, "--path-map", &map]
            .iter()
            .map(|s| s.to_string())
            .collect();
        if let Some(p) = podman {
            args.extend(["--podman".into(), p.display().to_string()]);
        }
        let given: Vec<&str> = args.iter().map(String::as_str).collect();
        let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
        hammer_mcp::build_server(&W::parse_from(argv).c).unwrap()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn call(server: &McpServer<HammerState>, args: Value) -> Result<(Value, Vec<String>), ToolError> {
    match server.call("model_import", args).await {
        Ok(out) => {
            let next = out.next.iter().flatten().map(|s| s.tool.clone()).collect();
            Ok((out.json, next))
        }
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

fn opts(name: &str, kind: Kind) -> ImportOptions {
    ImportOptions { name: name.into(), kind, scale: None, up: None, generate_materials: true, existing: None, physics: None, player: None }
}

fn plan_of(f: &Fixture, o: &ImportOptions) -> import::ImportPlan {
    import::plan(Path::new("scene.glb"), f.glb(), o).expect("plan")
}

fn file<'a>(plan: &'a import::ImportPlan, role: &str) -> &'a import::PlannedFile {
    plan.files.iter().find(|f| f.role == role).unwrap_or_else(|| panic!("no {role} file"))
}

type Tri = (String, [[f64; 8]; 3]);

/// Parse the triangles section back: (material, 3 x [x y z nx ny nz u v]).
fn parse_smd(text: &str) -> Vec<Tri> {
    let mut lines = text.lines().skip_while(|l| *l != "triangles").skip(1);
    let mut out = Vec::new();
    while let Some(material) = lines.next() {
        if material == "end" {
            break;
        }
        let mut tri = [[0.0; 8]; 3];
        for v in &mut tri {
            let nums: Vec<f64> = lines.next().unwrap().split_whitespace().skip(1).map(|n| n.parse().unwrap()).collect();
            v.copy_from_slice(&nums);
        }
        out.push((material.to_string(), tri));
    }
    out
}

fn assert_close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-4, "{a} != {b}");
}

fn tree_files(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, base, out);
                } else {
                    out.push(p.strip_prefix(base).unwrap().display().to_string());
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

// ---------------------------------------------------------------------------------- conversion

#[test]
fn the_unit_cube_becomes_12_triangles_in_inches() {
    let plan = plan_of(&Fixture::cube(), &opts("cube", Kind::Prop));
    let stats = plan.mesh.as_ref().unwrap();
    assert_eq!((stats.tris, stats.verts, stats.materials), (12, 24, 1));
    let tris = parse_smd(std::str::from_utf8(&file(&plan, "mesh").bytes).unwrap());
    assert_eq!(tris.len(), 12);
    assert!(tris.iter().all(|(m, _)| m == "default"));
    let max = tris.iter().flat_map(|(_, t)| t.iter().map(|v| v[0].abs().max(v[1].abs()).max(v[2].abs()))).fold(0.0, f64::max);
    assert_close(max, 0.5 * 39.37);
}

/// What the compiled model shows for an SMD vector: the compiler's Rz(+90) (`import::smd` docs).
fn compiled(v: [f64; 3]) -> [f64; 3] {
    [-v[1], v[0], v[2]]
}

#[test]
fn the_axis_convention_is_pinned_by_an_asymmetric_triangle() {
    // glTF (x,y,z) -> CS2 (z,x,y): +Y up -> +Z up, +Z front -> +X front, +X -> +Y; scale 1 here
    let mut o = opts("tri", Kind::Prop);
    o.scale = Some(1.0);
    let plan = plan_of(&Fixture::corner_triangle(), &o);
    let tris = parse_smd(std::str::from_utf8(&file(&plan, "mesh").bytes).unwrap());
    let t = &tris[0].1;
    // the SMD holds the vectors turned by Rz(-90); the compiler turns them back (compiled())
    let pos = |v: &[f64; 8]| compiled([v[0], v[1], v[2]]);
    // glTF (1,0,0) -> (0,1,0); (0,2,0) -> (0,0,2); (0,0,3) -> (3,0,0)
    for (got, want) in [(pos(&t[0]), [0., 1., 0.]), (pos(&t[1]), [0., 0., 2.]), (pos(&t[2]), [3., 0., 0.])] {
        for k in 0..3 {
            assert_close(got[k], want[k]);
        }
    }
    // proper rotation: the winding still agrees with the (rotated) normal
    let (a, b, c) = (pos(&t[0]), pos(&t[1]), pos(&t[2]));
    let (u, v) = ([b[0] - a[0], b[1] - a[1], b[2] - a[2]], [c[0] - a[0], c[1] - a[1], c[2] - a[2]]);
    let cross = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
    let n = compiled([t[0][3], t[0][4], t[0][5]]);
    assert!(cross[0] * n[0] + cross[1] * n[1] + cross[2] * n[2] > 0.0, "winding flipped");
    // the normal (6,3,2)/7 maps the same way: (2,6,3)/7
    assert_close(n[0], 2.0 / 7.0);
    assert_close(n[1], 6.0 / 7.0);
    assert_close(n[2], 3.0 / 7.0);
}

#[test]
fn up_axis_z_is_the_identity_and_scale_applies() {
    let mut o = opts("tri", Kind::Prop);
    o.up = Some(import::smd::UpAxis::Z);
    o.scale = Some(2.0);
    let plan = plan_of(&Fixture::corner_triangle(), &o);
    let tris = parse_smd(std::str::from_utf8(&file(&plan, "mesh").bytes).unwrap());
    let at = |i: usize| compiled([tris[0].1[i][0], tris[0].1[i][1], tris[0].1[i][2]]);
    assert_close(at(0)[0], 2.0);
    assert_close(at(1)[1], 4.0);
    assert_close(at(2)[2], 6.0);
}

#[test]
fn v_is_flipped() {
    let plan = plan_of(&Fixture::corner_triangle(), &opts("tri", Kind::Prop));
    let tris = parse_smd(std::str::from_utf8(&file(&plan, "mesh").bytes).unwrap());
    // uvs (0,0) (1,0) (0,1) -> v = 1, 1, 0
    assert_close(tris[0].1[0][7], 1.0);
    assert_close(tris[0].1[1][6], 1.0);
    assert_close(tris[0].1[2][7], 0.0);
}

#[test]
fn a_node_transform_is_applied_and_a_mirror_keeps_the_outside_out() {
    let mut f = Fixture::corner_triangle();
    // translate (10, 20, 30)
    f.node_matrix = Some([1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 10., 20., 30., 1.]);
    let mut o = opts("tri", Kind::Prop);
    o.scale = Some(1.0);
    let tris = parse_smd(std::str::from_utf8(&file(&plan_of(&f, &o), "mesh").bytes).unwrap());
    // glTF (11,20,30) -> (30,11,20)
    let first = compiled([tris[0].1[0][0], tris[0].1[0][1], tris[0].1[0][2]]);
    assert_close(first[0], 30.0);
    assert_close(first[1], 11.0);
    assert_close(first[2], 20.0);

    // mirror in X: the winding must be flipped so geometry and normals still agree
    let mut f = Fixture::corner_triangle();
    f.node_matrix = Some([-1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.]);
    let tris = parse_smd(std::str::from_utf8(&file(&plan_of(&f, &o), "mesh").bytes).unwrap());
    let t = &tris[0].1;
    let p = |i: usize| [t[i][0], t[i][1], t[i][2]];
    let (u, v) = ([p(1)[0] - p(0)[0], p(1)[1] - p(0)[1], p(1)[2] - p(0)[2]], [p(2)[0] - p(0)[0], p(2)[1] - p(0)[1], p(2)[2] - p(0)[2]]);
    let cross = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
    let n = [t[0][3], t[0][4], t[0][5]];
    assert!(cross[0] * n[0] + cross[1] * n[1] + cross[2] * n[2] > 0.0, "mirrored winding not repaired");
}

#[test]
fn glb_gltf_data_uri_and_external_files_read_the_same() {
    let tree = Tree::new("storage");
    let f = Fixture::textured_quad();
    let reference = plan_of(&f, &opts("q", Kind::Prop));
    let want = &file(&reference, "mesh").bytes;
    for (stem, storage) in [("uri", Storage::DataUri), ("ext", Storage::External)] {
        let path = f.write_gltf(&tree.src().join(stem), "scene", storage);
        let bytes = std::fs::read(&path).unwrap();
        let plan = import::plan(&path, bytes, &opts("q", Kind::Prop)).expect("plan");
        assert_eq!(&file(&plan, "mesh").bytes, want, "{stem}");
        assert_eq!(file(&plan, "texture").bytes, TINY_PNG, "{stem}");
    }
}

// ---------------------------------------------------------------------------------- materials

#[test]
fn a_textured_quad_gets_a_vmat_a_texture_and_a_remap() {
    let plan = plan_of(&Fixture::textured_quad(), &opts("brick", Kind::Prop));
    let paths: Vec<&str> = plan.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "models/brick/brick.vmdl",
            "models/brick/brick.smd",
            "materials/models/brick/brick_wall_color.png",
            "materials/models/brick/brick_wall.vmat"
        ]
    );
    // the vmat parses back with cs2-kv1
    let vmat = std::str::from_utf8(&file(&plan, "material").bytes).unwrap();
    let doc = cs2_kv1::text::parse(vmat).expect("vmat parses");
    assert_eq!(doc.key, "Layer0");
    let layer = doc.value.as_object().unwrap();
    assert_eq!(layer.get("shader").unwrap().as_str(), Some("csgo_complex.vfx"));
    assert_eq!(layer.get("TextureColor").unwrap().as_str(), Some("materials/models/brick/brick_wall_color.png"));
    assert_eq!(layer.get("g_vColorTint").unwrap().as_str(), Some("[0.500000 0.250000 1.000000 0.000000]"));
    assert!(layer.get("TextureNormal").is_none());
    // the vmdl validates and remaps the SMD material to it
    let text = std::str::from_utf8(&file(&plan, "model").bytes).unwrap();
    let model = Model::parse(text).expect("vmdl parses");
    assert!(model.validate(None).is_empty());
    let nodes = model.node_json(&"/2/0".parse().unwrap(), None).unwrap();
    assert_eq!(nodes["_class"], json!("DefaultMaterialGroup"));
    assert_eq!(
        nodes["attrs"]["remaps"],
        json!([{"from": "brick_wall.vmat", "to": "materials/models/brick/brick_wall.vmat"}])
    );
    assert_eq!(plan.materials.len(), 1);
    assert_eq!(plan.materials[0].textures, ["materials/models/brick/brick_wall_color.png"]);
    let smd = parse_smd(std::str::from_utf8(&file(&plan, "mesh").bytes).unwrap());
    assert!(smd.iter().all(|(m, _)| m == "brick_wall"));
}

#[test]
fn two_materials_make_two_vmats_and_the_smd_uses_both() {
    let plan = plan_of(&Fixture::two_materials(), &opts("two", Kind::Prop));
    assert_eq!(plan.files.iter().filter(|f| f.role == "material").count(), 2);
    let names: std::collections::BTreeSet<String> =
        parse_smd(std::str::from_utf8(&file(&plan, "mesh").bytes).unwrap()).into_iter().map(|(m, _)| m).collect();
    assert_eq!(names, ["brick_wall", "plain"].map(String::from).into());
    assert_eq!(plan.mesh.as_ref().unwrap().materials, 2);
}

#[test]
fn a_packed_metal_rough_texture_is_used_for_both_keys_with_a_warning() {
    let plan = plan_of(&Fixture::full_pbr(), &opts("pbr", Kind::Prop));
    let vmat = std::str::from_utf8(&file(&plan, "material").bytes).unwrap();
    let layer = cs2_kv1::text::parse(vmat).unwrap();
    let layer = layer.value.as_object().unwrap();
    let rough = layer.get("TextureRoughness").unwrap().as_str().unwrap();
    assert_eq!(layer.get("TextureMetalness").unwrap().as_str().unwrap(), rough);
    assert!(layer.get("TextureNormal").is_some());
    assert!(plan.warnings.iter().any(|w| w.contains("channels not split")), "{:?}", plan.warnings);
    assert!(plan.materials[0].dropped.iter().any(|w| w.contains("channels not split")));
}

// ---------------------------------------------------------------------------------- kinds / template

#[test]
fn physics_defaults_follow_the_kind_and_can_be_overridden() {
    let class_of = |plan: &import::ImportPlan| {
        let m = Model::parse(std::str::from_utf8(&file(plan, "model").bytes).unwrap()).unwrap();
        m.list(None, None, None).iter().find(|n| n.class.starts_with("Physics") && n.class.ends_with("File")).map(|n| n.class.clone())
    };
    let f = Fixture::cube();
    assert_eq!(class_of(&plan_of(&f, &opts("a", Kind::Prop))).as_deref(), Some("PhysicsHullFile"));
    assert_eq!(class_of(&plan_of(&f, &opts("a", Kind::World))).as_deref(), Some("PhysicsMeshFile"));
    let mut o = opts("a", Kind::World);
    o.physics = Some(Physics::Hull);
    assert_eq!(class_of(&plan_of(&f, &o)).as_deref(), Some("PhysicsHullFile"));
    o.physics = Some(Physics::None);
    assert_eq!(class_of(&plan_of(&f, &o)), None);
}

#[test]
fn materials_none_writes_no_vmat_and_uses_the_neutral_default() {
    let mut o = opts("n", Kind::Prop);
    o.generate_materials = false;
    let plan = plan_of(&Fixture::textured_quad(), &o);
    assert!(plan.files.iter().all(|f| f.role == "model" || f.role == "mesh"));
    let text = std::str::from_utf8(&file(&plan, "model").bytes).unwrap();
    assert!(text.contains("global_default_material = \"materials/dev/reflectivity_50.vmat\""), "{text}");
}

// ---------------------------------------------------------------------------------- reader errors

#[test]
fn a_required_draco_extension_is_refused() {
    let mut f = Fixture::cube();
    f.extensions_required = vec!["KHR_draco_mesh_compression".into()];
    let err = import::plan(Path::new("d.glb"), f.glb(), &opts("d", Kind::Prop)).unwrap_err();
    match err {
        ImportError::UnsupportedExtension(e) => assert_eq!(e, ["KHR_draco_mesh_compression"]),
        other => panic!("{other}"),
    }
}

#[test]
fn a_skin_without_joint_data_attaches_the_mesh_rigidly() {
    let mut f = Fixture::cube();
    f.skinned = true;
    let plan = plan_of(&f, &opts("s", Kind::Prop));
    assert!(plan.warnings.iter().any(|w| w.contains("attached rigidly")), "{:?}", plan.warnings);
    assert_eq!(plan.mesh.as_ref().unwrap().tris, 12);
    assert_eq!(plan.rig.as_ref().unwrap().bones, 1);
}

#[test]
fn garbage_and_oversized_files_are_typed_errors() {
    assert!(matches!(import::plan(Path::new("x.glb"), b"not a glb".to_vec(), &opts("x", Kind::Prop)), Err(ImportError::Parse(_))));
    let big = Fixture::many_triangles(import::gltf_read::MAX_TRIS + 1);
    match import::plan(Path::new("big.glb"), big.glb(), &opts("big", Kind::Prop)) {
        Err(ImportError::TooLarge { tris, cap }) => assert_eq!((tris, cap), (import::gltf_read::MAX_TRIS + 1, import::gltf_read::MAX_TRIS)),
        other => panic!("{other:?}"),
    }
    // external files may not escape the glTF's directory
    let tree = Tree::new("escape");
    std::fs::create_dir_all(tree.src()).unwrap();
    let path = tree.src().join("e.gltf");
    std::fs::write(&path, r#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":4,"uri":"../secret.bin"}]}"#).unwrap();
    let err = import::plan(&path, std::fs::read(&path).unwrap(), &opts("e", Kind::Prop)).unwrap_err();
    assert!(matches!(&err, ImportError::Parse(d) if d.contains("refused")), "{err}");
}

// ---------------------------------------------------------------------------------- the tool

fn quad_glb(tree: &Tree) -> String {
    Fixture::textured_quad().write_glb(&tree.src(), "Brick").display().to_string()
}

#[tokio::test]
async fn the_tool_writes_the_files_compiles_and_chains_next_steps() {
    let tree = Tree::new("tool");
    let source = quad_glb(&tree);
    let podman = tree.podman("ok");
    let server = tree.server(Some(&podman));
    let (out, next) = call(&server, json!({"source": source})).await.unwrap();
    assert_eq!(out["model"], json!("models/brick/brick.vmdl"));
    assert_eq!(out["written"], json!(true));
    assert_eq!(out["compile"]["ok"], json!(true), "{out}");
    assert_eq!(out["mesh"], json!({"tris": 2, "verts": 4, "materials": 1}));
    assert_eq!(out["physics"], json!("hull"));
    let roles: Vec<(&str, &str)> =
        out["files"].as_array().unwrap().iter().map(|f| (f["path"].as_str().unwrap(), f["role"].as_str().unwrap())).collect();
    assert!(roles.contains(&("models/brick/brick.vmdl", "model")));
    assert!(roles.contains(&("materials/models/brick/brick_wall.vmat", "material")));
    assert!(roles.contains(&("materials/models/brick/brick_wall_color.png", "texture")));
    for (path, _) in &roles {
        assert!(tree.addon().join(path).is_file(), "{path} on disk");
    }
    assert_eq!(std::fs::read(tree.addon().join("materials/models/brick/brick_wall_color.png")).unwrap(), TINY_PNG);
    assert!(tree.compile_output("models/brick/brick.vmdl").is_file());
    assert_eq!(out["materials"][0]["vmat"], json!("materials/models/brick/brick_wall.vmat"));
    assert_eq!(next, ["get_model_info", "create_props", "save_map", "look", "report_ready"]);
}

#[tokio::test]
async fn a_skinned_file_reports_skeleton_animations_and_places_with_its_first_clip() {
    let tree = Tree::new("rig");
    std::fs::create_dir_all(tree.src()).unwrap();
    let source = tree.src().join("Tube.glb");
    std::fs::write(&source, common::gltf_fixture::RigFixture::tube().glb()).unwrap();
    let podman = tree.podman("ok");
    let server = tree.server(Some(&podman));
    let args = json!({"source": source.display().to_string(), "materials": "none"});
    let (out, next_full) = match server.call("model_import", args).await {
        Ok(out) => {
            let next = out.next.clone();
            (out.json, next)
        }
        Err(_) => panic!("import failed"),
    };
    assert_eq!(out["compile"]["ok"], json!(true), "{out}");
    assert_eq!(out["skeleton"]["bones"], json!(2));
    assert_eq!(out["skeleton"]["root"], json!("root"));
    assert_eq!(out["skeleton"]["names"][1], json!({"source": "Tip", "smd": "tip"}));
    assert_eq!(out["animations"], json!([{"name": "bend", "frames": 31, "fps": 30.0}]));
    assert!(tree.addon().join("models/tube/bend.smd").is_file());
    let next = next_full.unwrap();
    let place = next.iter().find(|s| s.tool == "create_props").unwrap();
    assert_eq!(
        place.args.as_ref().unwrap()["props"][0]["keyValues"],
        json!({"StartingAnim": "bend", "StartingAnimationLoopMode": "ANIM_LOOP_MODE_LOOPING"})
    );
    let look = next.iter().find(|s| s.tool == "look").unwrap();
    assert_eq!(look.args.as_ref().unwrap(), &json!({"mode": "frames"}));
    // kind player needs the game's vnmskel (tests/model_import_player.rs covers the rest)
    let err = call(&server, json!({"source": source.display().to_string(), "kind": "player", "name": "player_x"})).await.unwrap_err();
    assert_eq!(err.name, "SkeletonNotFound");
}

#[tokio::test]
async fn compile_false_points_at_model_compile() {
    let tree = Tree::new("nocompile");
    let server = tree.server(None);
    let (out, next) = call(&server, json!({"source": quad_glb(&tree), "compile": false, "kind": "world", "name": "wall"})).await.unwrap();
    assert_eq!(out["compile"], Value::Null);
    assert_eq!(out["physics"], json!("mesh"));
    assert_eq!(next[0], "model_compile");
}

#[tokio::test]
async fn a_failed_compile_is_compile_failed_and_keeps_the_files() {
    let tree = Tree::new("fail");
    let podman = tree.podman("fail");
    let server = tree.server(Some(&podman));
    let err = call(&server, json!({"source": quad_glb(&tree)})).await.unwrap_err();
    assert_eq!(err.name, "CompileFailed");
    assert_eq!(err.data["failLines"], json!(["[FAIL] dependency failed: materials/models/x/x.vmat"]));
    assert_eq!(err.data["useInstead"]["tool"], json!("model_compile"));
    assert!(tree.addon().join("models/brick/brick.vmdl").is_file());
}

#[tokio::test]
async fn an_existing_model_needs_overwrite() {
    let tree = Tree::new("exists");
    let server = tree.server(None);
    let args = json!({"source": quad_glb(&tree), "compile": false});
    call(&server, args.clone()).await.unwrap();
    let err = call(&server, args.clone()).await.unwrap_err();
    assert_eq!(err.name, "ModelExists");
    assert!(err.data["hint"].as_str().unwrap().contains("overwrite"));
    let mut again = args;
    again["overwrite"] = json!(true);
    let replaced = |out: &Value| out["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("replaced existing"));
    // byte-identical files are not rewritten, so nothing is "replaced"
    let (out, _) = call(&server, again.clone()).await.unwrap();
    assert!(!replaced(&out), "{out}");
    // a changed generated file is replaced and reported
    let model_dir = tree.addon().join(out["model"].as_str().unwrap()).parent().unwrap().to_path_buf();
    let smd = std::fs::read_dir(&model_dir).unwrap().flatten().find(|e| e.path().extension().is_some_and(|x| x == "smd")).unwrap().path();
    std::fs::write(&smd, b"version 1\n").unwrap();
    let (out, _) = call(&server, again).await.unwrap();
    assert!(replaced(&out), "{out}");
}

#[tokio::test]
async fn dry_run_writes_nothing() {
    let tree = Tree::new("dry");
    let server = tree.server(None);
    let (out, next) = call(&server, json!({"source": quad_glb(&tree), "dryRun": true})).await.unwrap();
    assert_eq!(out["dryRun"], json!(true));
    assert_eq!(out["written"], json!(false));
    assert_eq!(out["files"].as_array().unwrap().len(), 4);
    assert_eq!(out["compile"], Value::Null);
    assert_eq!(next, ["model_import"]);
    assert!(tree_files(&tree.addon()).is_empty(), "{:?}", tree_files(&tree.addon()));
}

#[tokio::test]
async fn pass_through_formats_are_copied_and_referenced() {
    let tree = Tree::new("pass");
    let server = tree.server(None);
    std::fs::create_dir_all(tree.src()).unwrap();
    for ext in ["fbx", "smd", "obj", "dmx"] {
        let src = tree.src().join(format!("Thing.{ext}"));
        std::fs::write(&src, format!("payload {ext}")).unwrap();
        let (out, _) = call(&server, json!({"source": src.display().to_string(), "compile": false, "name": format!("t_{ext}"), "scale": 2.5})).await.unwrap();
        let copied = tree.addon().join(format!("models/t_{ext}/Thing.{ext}"));
        assert_eq!(std::fs::read_to_string(&copied).unwrap(), format!("payload {ext}"));
        assert_eq!(out["mesh"], Value::Null);
        let vmdl = std::fs::read_to_string(tree.addon().join(format!("models/t_{ext}/t_{ext}.vmdl"))).unwrap();
        assert!(vmdl.contains(&format!("filename = \"models/t_{ext}/Thing.{ext}\"")), "{vmdl}");
        assert!(vmdl.contains("import_scale = 2.5"), "{vmdl}");
        assert!(!vmdl.contains("global_default_material"), "pass-through keeps its own materials");
    }
}

#[tokio::test]
async fn typed_errors() {
    let tree = Tree::new("errors");
    let server = tree.server(None);
    std::fs::create_dir_all(tree.src()).unwrap();
    let stl = tree.src().join("a.stl");
    std::fs::write(&stl, "solid").unwrap();
    let err = call(&server, json!({"source": stl.display().to_string()})).await.unwrap_err();
    assert_eq!(err.name, "UnsupportedFormat");
    assert_eq!(err.data["ext"], json!("stl"));
    assert!(err.data["supported"].as_array().unwrap().contains(&json!("glb")));

    let mut draco = Fixture::cube();
    draco.extensions_required = vec!["KHR_draco_mesh_compression".into(), "KHR_materials_unlit".into()];
    let path = draco.write_glb(&tree.src(), "draco");
    let err = call(&server, json!({"source": path.display().to_string()})).await.unwrap_err();
    assert_eq!(err.name, "UnsupportedExtension");
    assert_eq!(err.data["gltfExtensions"], json!(["KHR_draco_mesh_compression"]));

    let bad = tree.src().join("bad.glb");
    std::fs::write(&bad, b"glTFnope").unwrap();
    let err = call(&server, json!({"source": bad.display().to_string()})).await.unwrap_err();
    assert_eq!(err.name, "ImportParse");
    assert!(err.data["detail"].is_string());

    let big = Fixture::many_triangles(import::gltf_read::MAX_TRIS + 1).write_glb(&tree.src(), "big");
    let err = call(&server, json!({"source": big.display().to_string()})).await.unwrap_err();
    assert_eq!(err.name, "TooLarge");
    assert_eq!(err.data["cap"], json!(import::gltf_read::MAX_TRIS));

    // hammer_mesh has its own, lower triangle cap (two bridge calls per chunk)
    let mid = Fixture::many_triangles(import::hammer_mesh::MAX_TRIS + 1).write_glb(&tree.src(), "mid");
    let err = call(&server, json!({"source": mid.display().to_string(), "kind": "hammer_mesh", "dryRun": true})).await.unwrap_err();
    assert_eq!(err.name, "TooLarge");
    assert_eq!(err.data["cap"], json!(import::hammer_mesh::MAX_TRIS));
    let err = call(&server, json!({"source": "/nonexistent/x.glb"})).await.unwrap_err();
    assert_eq!(err.name, "SourceNotFound");
    let err = call(&server, json!({"source": "q:\\nowhere\\x.glb"})).await.unwrap_err();
    assert_eq!(err.name, "PathNotMapped");
    let err = call(&server, json!({"source": path.display().to_string(), "name": "Bad Name"})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    let err = call(&server, json!({"source": path.display().to_string(), "bogus": 1})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    let err = call(&server, json!({"source": "../x.glb"})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
}

#[tokio::test]
async fn a_source_inside_the_addon_is_found_relative_to_it() {
    let tree = Tree::new("inaddon");
    let server = tree.server(None);
    Fixture::cube().write_glb(&tree.addon().join("incoming"), "box");
    let (out, _) = call(&server, json!({"source": "incoming/box.glb", "compile": false})).await.unwrap();
    assert_eq!(out["model"], json!("models/box/box.vmdl"));
}

// ------------------------------------------------------------------- materials:"existing" (I2)

/// Game paks holding the given compiled materials (`materials/x.vmat` -> `x.vmat_c`).
fn game_materials(tree: &Tree, materials: &[&str]) {
    let files: Vec<common::BuildFile> = materials
        .iter()
        .map(|m| common::BuildFile::preload_only(&format!("{m}_c"), b""))
        .collect();
    common::write_vpk(&tree.app.join("game/csgo"), "pak01", 2, &files);
    common::write_vpk(&tree.app.join("game/core"), "pak01", 2, &[common::BuildFile::preload_only("materials/other.vmat_c", b"")]);
}

fn two_quads(tree: &Tree) -> String {
    Fixture::two_materials().write_glb(&tree.src(), "Pair").display().to_string()
}

fn remaps_of(tree: &Tree, name: &str) -> Value {
    let text = std::fs::read_to_string(tree.addon().join(format!("models/{name}/{name}.vmdl"))).unwrap();
    let model = Model::parse(&text).expect("vmdl parses");
    assert!(model.validate(None).is_empty());
    model.node_json(&"/2/0".parse().unwrap(), None).unwrap()["attrs"]["remaps"].clone()
}

#[tokio::test]
async fn existing_materials_map_onto_shipped_ones_and_write_no_vmat_or_texture() {
    let tree = Tree::new("existing");
    game_materials(&tree, &["materials/dev/reflectivity_30.vmat", "materials/dev/reflectivity_50.vmat"]);
    let server = tree.server(None);
    let (out, _) = call(
        &server,
        json!({"source": two_quads(&tree), "compile": false, "materials": "existing",
               "materialMap": {"brick_wall": "materials/dev/reflectivity_30.vmat", "plain": "Materials/Dev/Reflectivity_50.vmat_c"}}),
    )
    .await
    .unwrap();
    assert_eq!(out["materialNames"], json!(["brick_wall", "plain"]));
    assert_eq!(out["remapped"], json!({"brick_wall": "materials/dev/reflectivity_30.vmat", "plain": "materials/dev/reflectivity_50.vmat"}));
    assert_eq!(out["materials"], json!([]), "nothing generated: {out}");
    assert_eq!(out["dropped"], json!([]));
    assert_eq!(out["warnings"], json!([]));
    assert_eq!(tree_files(&tree.addon()), ["models/pair/pair.smd", "models/pair/pair.vmdl"]);
    assert_eq!(
        remaps_of(&tree, "pair"),
        json!([
            {"from": "brick_wall.vmat", "to": "materials/dev/reflectivity_30.vmat"},
            {"from": "plain.vmat", "to": "materials/dev/reflectivity_50.vmat"}
        ])
    );
}

#[tokio::test]
async fn unmapped_materials_fall_back_with_a_warning_and_unknown_keys_warn() {
    let tree = Tree::new("existing-fallback");
    game_materials(&tree, &["materials/dev/reflectivity_30.vmat"]);
    let server = tree.server(None);
    // the key may be the raw glTF name: it is sanitized the way the SMD names are
    let (out, _) = call(
        &server,
        json!({"source": two_quads(&tree), "compile": false, "materials": "existing",
               "materialMap": {"Brick Wall": "materials/dev/reflectivity_30.vmat", "ghost": "materials/dev/reflectivity_30.vmat"}}),
    )
    .await
    .unwrap();
    let warnings: Vec<&str> = out["warnings"].as_array().unwrap().iter().map(|w| w.as_str().unwrap()).collect();
    assert!(warnings.iter().any(|w| w.contains("plain") && w.contains("fallback") && w.contains("reflectivity_50")), "{warnings:?}");
    assert!(warnings.iter().any(|w| w.contains("\"ghost\"") && w.contains("matches no material")), "{warnings:?}");
    assert_eq!(out["remapped"]["plain"], json!("materials/dev/reflectivity_50.vmat"));
    assert_eq!(out["remapped"]["brick_wall"], json!("materials/dev/reflectivity_30.vmat"));
}

#[tokio::test]
async fn an_unknown_target_is_a_typed_error_with_the_nearest_names_and_writes_nothing() {
    let tree = Tree::new("existing-unknown");
    game_materials(&tree, &["materials/dev/reflectivity_30.vmat", "materials/dev/reflectivity_50.vmat", "materials/other/x.vmat"]);
    let server = tree.server(None);
    let err = call(
        &server,
        json!({"source": two_quads(&tree), "materials": "existing",
               "materialMap": {"plain": "materials/dev/reflectivity_35.vmat"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "UnknownMaterial");
    assert_eq!(err.data["unknown"][0]["target"], json!("materials/dev/reflectivity_35.vmat"));
    let near: Vec<&str> = err.data["didYouMean"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(near.contains(&"materials/dev/reflectivity_30.vmat"), "{near:?}");
    assert!(!near.contains(&"materials/other/x.vmat"), "same folder only: {near:?}");
    assert!(tree_files(&tree.addon()).is_empty());
    // not a .vmat at all, and a map without existing: invalid arguments
    let err = call(&server, json!({"source": two_quads(&tree), "materials": "existing", "materialMap": {"plain": "textures/x.png"}})).await.unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    let err = call(&server, json!({"source": two_quads(&tree), "materialMap": {"plain": "materials/dev/reflectivity_30.vmat"}})).await.unwrap_err();
    assert!(err.message.contains("materials:\"existing\""), "{}", err.message);
}

#[tokio::test]
async fn an_addon_vmat_source_is_a_valid_target_with_a_compile_note() {
    let tree = Tree::new("existing-addon");
    game_materials(&tree, &["materials/dev/reflectivity_50.vmat"]);
    std::fs::create_dir_all(tree.addon().join("materials/mine")).unwrap();
    std::fs::write(tree.addon().join("materials/mine/paint.vmat"), "x").unwrap();
    let server = tree.server(None);
    let (out, _) = call(
        &server,
        json!({"source": two_quads(&tree), "compile": false, "materials": "existing",
               "materialMap": {"plain": "materials/mine/paint.vmat", "brick_wall": "materials/mine/paint.vmat"}}),
    )
    .await
    .unwrap();
    assert!(out["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("compile_asset")), "{out}");
    assert_eq!(out["remapped"]["plain"], json!("materials/mine/paint.vmat"));
}

#[tokio::test]
async fn dry_run_lists_the_material_names_and_the_next_step_keeps_the_map() {
    let tree = Tree::new("existing-dry");
    let server = tree.server(None);
    let (out, _) = call(&server, json!({"source": two_quads(&tree), "dryRun": true, "materials": "existing"})).await.unwrap();
    assert_eq!(out["materialNames"], json!(["brick_wall", "plain"]));
    assert_eq!(out["materials"], json!([]));
    assert!(out["warnings"].as_array().unwrap().iter().filter(|w| w.as_str().unwrap().contains("fallback")).count() == 2, "{out}");
    assert!(tree_files(&tree.addon()).is_empty());
    // generate dry runs list the names too
    let (out, _) = call(&server, json!({"source": two_quads(&tree), "dryRun": true})).await.unwrap();
    assert_eq!(out["materialNames"], json!(["brick_wall", "plain"]));
}

#[tokio::test]
async fn pass_through_existing_remaps_the_given_names_and_warns_they_are_unchecked() {
    let tree = Tree::new("existing-pass");
    game_materials(&tree, &["materials/dev/reflectivity_30.vmat"]);
    let server = tree.server(None);
    std::fs::create_dir_all(tree.src()).unwrap();
    let src = tree.src().join("Thing.fbx");
    std::fs::write(&src, "payload").unwrap();
    let (out, _) = call(
        &server,
        json!({"source": src.display().to_string(), "compile": false, "materials": "existing",
               "materialMap": {"Metal.vmat": "materials/dev/reflectivity_30.vmat"}}),
    )
    .await
    .unwrap();
    assert!(out["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("not checked")), "{out}");
    assert_eq!(out["materials"], json!([]));
    assert_eq!(remaps_of(&tree, "thing"), json!([{"from": "Metal.vmat", "to": "materials/dev/reflectivity_30.vmat"}]));
    assert_eq!(out["materialNames"], Value::Null);
}
