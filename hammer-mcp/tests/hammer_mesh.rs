//! `model_import kind:"hammer_mesh"` against the mock bridge (I3): call order (create, pair,
//! align per chunk), the requests' content (materials, axes), dryRun without bridge calls,
//! capability gating, and the partial-failure error that lists the chunks already created.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

mod common;
use common::gltf_fixture::{Fixture, Mat, Prim};

static SEQ: AtomicU64 = AtomicU64::new(0);
const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";

/// A PNG header of `w x h` (only the signature and IHDR are read: the importer passes the bytes
/// through and the compiler is not run).
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    b
}

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "hammer-mesh-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let app = root.join("workshop/run/app");
        std::fs::create_dir_all(app.join("content/csgo_addons/mcp_test")).unwrap();
        Tree { root, app }
    }
    fn addon(&self) -> PathBuf {
        self.app.join("content/csgo_addons/mcp_test")
    }
    fn server(&self, mb: &MockBridge, podman: Option<&std::path::Path>) -> McpServer<HammerState> {
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: CommonConfig,
        }
        let root = self.root.display().to_string();
        let map = format!("z:\\app={}", self.app.display());
        let discovery = mb.discovery_path.display().to_string();
        let mut args: Vec<String> =
            ["--deadline-ms", "5000", "--tool-budget-ms", "45000", "--project-root", &root, "--path-map", &map, "--discovery", &discovery]
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
    fn podman_ok(&self) -> PathBuf {
        let dir = self.root.join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let body = r#"#!/bin/bash
APP='__APP__'
case "$1" in
  ps) printf '[{"Names":["cs2tools"],"State":"running"}]' ;;
  exec)
    out_all=$(printf '%s' "$*" | grep -o -- '-i [^ ]*' | sed 's/-i //')
    for rel in $out_all; do rel="${rel#/app/content/}"; out="$APP/game/${rel}_c"; mkdir -p "$(dirname "$out")"; touch "$out"; done
    n=$(printf '%s\n' "$out_all" | wc -l)
    echo "OK: $n compiled, 0 failed, 0 skipped, 0m:01s" ;;
esac
"#
        .replace("__APP__", &self.app.display().to_string());
        let script = dir.join("podman-ok");
        std::fs::write(&script, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn mock(build: &str) -> MockBridge {
    MockBridge::builder()
        .build(build)
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .start()
        .await
        .unwrap()
}

async fn call(server: &McpServer<HammerState>, name: &str, args: Value) -> Result<(Value, Vec<String>), ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok((out.json, out.next.iter().flatten().map(|s| s.tool.clone()).collect())),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

/// Two 1x1 m walls (XY plane, facing +Z in glTF) with different materials: a 256x128 texture and
/// a plain tint. UV: u = x, v = 1 - y.
fn two_walls() -> Fixture {
    let mut f = Fixture::textured_quad();
    f.images = vec![png(256, 128)];
    let mut second = f.prims[0].clone();
    for p in &mut second.positions {
        p[0] += 3.0;
    }
    second.material = Some(1);
    f.prims.push(second);
    f.materials.push(Mat { name: "Plain".into(), base_color: [1.0, 0.0, 0.0, 1.0], ..Mat::default() });
    f
}

/// `n` separate triangles (no merge possible), one material with a 64x64 texture.
fn triangles(n: usize) -> Fixture {
    let mut prim = Prim { normals: Some(vec![]), uvs: Some(vec![]), indices: Some(vec![]), material: Some(0), ..Prim::default() };
    for i in 0..n {
        let x = i as f32 * 3.0;
        let b = prim.positions.len() as u32;
        prim.positions.extend([[x, 0.0, 0.0], [x + 1.0, 0.0, 0.0], [x, 1.0, 0.0]]);
        prim.normals.as_mut().unwrap().extend([[0.0, 0.0, 1.0]; 3]);
        prim.uvs.as_mut().unwrap().extend([[0.0, 1.0], [1.0, 1.0], [0.0, 0.0]]);
        prim.indices.as_mut().unwrap().extend([b, b + 1, b + 2]);
    }
    Fixture {
        prims: vec![prim],
        materials: vec![Mat { name: "Tile".into(), base_color: [1.0; 4], base_tex: Some(0), ..Mat::default() }],
        images: vec![png(64, 64)],
        ..Fixture::default()
    }
}

fn write(tree: &Tree, f: &Fixture, stem: &str) -> String {
    f.write_glb(&tree.root.join("src"), stem).display().to_string()
}

fn order(mb: &MockBridge) -> Vec<String> {
    mb.requests()
        .into_iter()
        .map(|r| r.method)
        .filter(|m| ["mesh.create", "mesh.get", "mesh.setFaceTexture"].contains(&m.as_str()))
        .collect()
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[tokio::test]
async fn creates_one_mesh_per_material_and_aligns_each_with_the_exact_axes() {
    let tree = Tree::new("happy");
    let mb = mock("m3.18").await;
    let server = tree.server(&mb, None);
    let source = write(&tree, &two_walls(), "walls");
    let (out, next) = call(&server, "model_import", json!({"source": source, "kind": "hammer_mesh", "compile": false})).await.unwrap();
    assert_eq!(out["kind"], "hammer_mesh");
    assert_eq!(out["chunks"], 2);
    assert_eq!(out["nodeIDs"].as_array().unwrap().len(), 2);
    assert_eq!(out["facesPerChunk"], json!([1, 1]), "each wall's two triangles merged into one quad");
    assert_eq!(out["uvMode"], "axes");
    assert_eq!(out["uvExact"], true);
    assert_eq!(out["mesh"]["tris"], 4);
    assert_eq!(out["mesh"]["faces"], 2);
    assert_eq!(out["mesh"]["verts"], 8);
    assert_eq!(out["saved"], false);
    assert_eq!(next, ["look", "save_map", "compile_map", "run_map"]);
    assert_eq!(out["next"][0]["args"]["target"], out["nodeIDs"][0]);

    // order: create, pair (mesh.get), align; per chunk
    assert_eq!(
        order(&mb),
        ["mesh.create", "mesh.get", "mesh.setFaceTexture", "mesh.create", "mesh.get", "mesh.setFaceTexture"]
    );
    // materials: the generated vmat of each glTF material
    let creates = mb.method_calls("mesh.create");
    assert_eq!(creates[0].params["material"], "materials/models/walls/brick_wall.vmat");
    assert_eq!(creates[1].params["material"], "materials/models/walls/plain.vmat");
    assert!(tree.addon().join("materials/models/walls/brick_wall.vmat").is_file());
    assert!(tree.addon().join("materials/models/walls/brick_wall_color.png").is_file());

    // the axes sent reproduce u = Y/39.37 (minus 3 for the wall moved 3 m), v = 1 - Z/39.37 at every corner
    // (CS2: glTF x -> Y, glTF y -> Z), for the 256x128 texture; the tint-only wall assumes 512x512
    let aligns = mb.method_calls("mesh.setFaceTexture");
    for (k, size) in [(0usize, [256.0, 128.0]), (1, [512.0, 512.0])] {
        let p = &aligns[k].params;
        assert_eq!(p["mode"], "axes");
        assert_eq!(p["nodeID"], out["nodeIDs"][k]);
        let face = &p["faces"][0];
        assert_eq!(face["index"], 0);
        let nums = |v: &Value| v.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect::<Vec<_>>();
        let (u, v, s) = (nums(&face["axisU"]), nums(&face["axisV"]), nums(&face["scale"]));
        let verts = creates[k].params["vertices"].as_array().unwrap().iter().map(nums).collect::<Vec<_>>();
        for q in verts {
            let tu = (dot(&q, &u[..3]) / s[0] + u[3]) / size[0];
            let tv = (dot(&q, &v[..3]) / s[1] + v[3]) / size[1];
            let (eu, ev) = (q[1] / 39.37 - 3.0 * k as f64, 1.0 - q[2] / 39.37);
            assert!((tu - eu).abs() < 1e-4 && (tv - ev).abs() < 1e-4, "wall {k}: ({tu}, {tv}) vs ({eu}, {ev}) at {q:?}");
        }
    }
    // 1 m over a 256 px texture: 39.37 / 256 world units per texel
    assert!((aligns[0].params["faces"][0]["scale"][0].as_f64().unwrap() - 39.37 / 256.0).abs() < 1e-4);
    // the plain material has no texture: the 512 default applies silently
    assert!(!out["warnings"].to_string().contains("texture size"), "{}", out["warnings"]);
    assert_eq!(server.state().unsaved_edits(), 4, "two creates and two aligns");
}

#[tokio::test]
async fn many_triangles_split_by_max_tris_with_every_face_aligned_once() {
    let tree = Tree::new("split");
    let mb = mock("m3.18").await;
    let server = tree.server(&mb, None);
    let source = write(&tree, &triangles(75), "tiles");
    let (out, _) = call(
        &server,
        "model_import",
        json!({"source": source, "kind": "hammer_mesh", "compile": false, "maxTrisPerMesh": 30, "origin": [100, 0, 0]}),
    )
    .await
    .unwrap();
    assert_eq!(out["chunks"], 4, "75 faces halve to 18/19/19/19 under the 30 cap");
    assert_eq!(out["facesPerChunk"].as_array().unwrap().iter().map(|n| n.as_u64().unwrap()).sum::<u64>(), 75);
    assert!(out["facesPerChunk"].as_array().unwrap().iter().all(|n| n.as_u64().unwrap() <= 30));
    let creates = mb.method_calls("mesh.create");
    assert_eq!(creates.len(), 4);
    // the origin is applied to the vertices (glTF x -> CS2 y, so x stays 0 + 100)
    for c in &creates {
        assert!(c.params["vertices"].as_array().unwrap().iter().all(|v| v[0].as_f64().unwrap() >= 100.0 - 1e-6));
    }
    let total: usize = mb.method_calls("mesh.setFaceTexture").iter().map(|r| r.params["faces"].as_array().unwrap().len()).sum();
    assert_eq!(total, 75);
    // 64x64 texture: 1 m over 64 texels
    assert!((mb.method_calls("mesh.setFaceTexture")[0].params["faces"][0]["scale"][0].as_f64().unwrap() - 39.37 / 64.0).abs() < 1e-4);
}

#[tokio::test]
async fn existing_material_files_need_overwrite() {
    let tree = Tree::new("overwrite");
    let mb = mock("m3.18").await;
    let server = tree.server(&mb, None);
    let source = write(&tree, &triangles(3), "tiles");
    let args = json!({"source": source, "kind": "hammer_mesh", "compile": false});
    call(&server, "model_import", args.clone()).await.unwrap();
    let creates = mb.method_calls("mesh.create").len();
    let err = call(&server, "model_import", args.clone()).await.unwrap_err();
    assert_eq!(err.name, "ModelExists", "{err:?}");
    assert_eq!(mb.method_calls("mesh.create").len(), creates, "refused before the bridge");
    let mut again = args;
    again["overwrite"] = json!(true);
    // byte-identical files are left alone (a rewrite would only bump the mtime) and not reported
    let (out, _) = call(&server, "model_import", again.clone()).await.unwrap();
    assert!(!out["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("replaced existing")), "{out}");
    // a changed file is replaced and reported
    let vmat = std::fs::read_dir(tree.addon().join("materials/models/tiles")).unwrap().flatten().find(|e| e.path().extension().is_some_and(|x| x == "vmat")).unwrap().path();
    std::fs::write(&vmat, b"// edited by hand\n").unwrap();
    let (out, _) = call(&server, "model_import", again).await.unwrap();
    assert!(out["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("replaced existing")), "{out}");
}

#[tokio::test]
async fn dry_run_plans_without_a_single_bridge_call() {
    let tree = Tree::new("dry");
    let mb = mock("m3.18").await;
    let server = tree.server(&mb, None);
    let source = write(&tree, &triangles(75), "tiles");
    let (out, next) = call(&server, "model_import", json!({"source": source, "kind": "hammer_mesh", "dryRun": true, "maxTrisPerMesh": 30})).await.unwrap();
    assert_eq!(out["dryRun"], true);
    assert_eq!(out["written"], false);
    assert_eq!(out["chunks"], 4);
    assert_eq!(out["uvMode"], "axes");
    assert_eq!(next, ["model_import"]);
    assert!(out.get("nodeIDs").is_none());
    for method in ["mesh.create", "mesh.get", "mesh.setFaceTexture"] {
        assert!(mb.method_calls(method).is_empty(), "{method} was called");
    }
    assert!(!tree.addon().join("materials").exists(), "nothing written");
    assert_eq!(server.state().unsaved_edits(), 0);
}

#[tokio::test]
async fn an_older_bridge_or_a_missing_method_is_refused_before_anything_is_written() {
    // m3.17 has the methods but not mode axes
    let tree = Tree::new("old");
    let mb = mock("m3.17").await;
    let server = tree.server(&mb, None);
    let source = write(&tree, &two_walls(), "walls");
    let err = call(&server, "model_import", json!({"source": source, "kind": "hammer_mesh", "compile": false})).await.unwrap_err();
    assert_eq!(err.name, "NotImplemented", "{err:?}");
    assert_eq!(err.data["needs"], "m3.18");
    assert!(mb.method_calls("mesh.create").is_empty());
    assert!(!tree.addon().join("materials").exists());

    // no mesh.setFaceTexture at all
    let tree = Tree::new("nomethod");
    let mb = MockBridge::builder()
        .build("m3.18")
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .hammer_face_texture(false)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb, None);
    let source = write(&tree, &two_walls(), "walls");
    let err = call(&server, "model_import", json!({"source": source, "kind": "hammer_mesh", "compile": false})).await.unwrap_err();
    assert_eq!(err.name, "NotImplemented", "{err:?}");
    assert_eq!(err.data["missing"], json!(["mesh.setFaceTexture"]));
    assert!(mb.method_calls("mesh.create").is_empty());
}

#[tokio::test]
async fn a_failure_part_way_lists_the_chunks_already_created() {
    // the second create is refused
    let tree = Tree::new("partial");
    let mb = MockBridge::builder()
        .build("m3.18")
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .error_on_call("mesh.create", 2, -32602, "InvalidParams", Some(json!({"reason": "boom"})))
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb, None);
    let source = write(&tree, &two_walls(), "walls");
    let err = call(&server, "model_import", json!({"source": source, "kind": "hammer_mesh", "compile": false})).await.unwrap_err();
    assert_eq!(err.name, "ChunkFailed", "{err:?}");
    assert_eq!(err.data["failedChunk"], 1);
    assert_eq!(err.data["stage"], "create");
    assert_eq!(err.data["created"].as_array().unwrap().len(), 1);
    assert_eq!(err.data["useInstead"]["tool"], "delete_nodes");
    assert_eq!(err.data["useInstead"]["args"]["nodeIDs"], err.data["created"]);
    assert!(err.message.contains("undo_last twice"), "{}", err.message);

    // the align of the first chunk fails: that node is listed too
    let tree = Tree::new("partial2");
    let mb = mock("m3.18").await;
    mb.set_hammer_face_texture_fail(Some("pre"));
    let server = tree.server(&mb, None);
    let source = write(&tree, &two_walls(), "walls");
    let err = call(&server, "model_import", json!({"source": source, "kind": "hammer_mesh", "compile": false})).await.unwrap_err();
    assert_eq!(err.name, "ChunkFailed", "{err:?}");
    assert_eq!(err.data["stage"], "align");
    assert_eq!(err.data["failedChunk"], 0);
    assert_eq!(err.data["created"].as_array().unwrap().len(), 1);
    assert_eq!(mb.method_calls("mesh.create").len(), 1, "stopped at the failure");
}

#[tokio::test]
async fn without_mesh_get_the_creation_order_is_assumed_and_said() {
    let tree = Tree::new("noget");
    let mb = MockBridge::builder()
        .build("m3.18")
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .hammer_mesh_get(false)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb, None);
    let source = write(&tree, &triangles(4), "tiles");
    let (out, _) = call(&server, "model_import", json!({"source": source, "kind": "hammer_mesh", "compile": false})).await.unwrap();
    assert_eq!(out["chunks"], 1);
    assert!(out["warnings"].to_string().contains("creation order"), "{}", out["warnings"]);
    assert!(mb.method_calls("mesh.get").is_empty());
    assert_eq!(mb.method_calls("mesh.setFaceTexture")[0].params["faces"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn faces_hammer_dropped_are_left_unaligned_and_reported() {
    let tree = Tree::new("drop");
    let mb = MockBridge::builder()
        .build("m3.18")
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .hammer_drop_faces(1)
        .start()
        .await
        .unwrap();
    let server = tree.server(&mb, None);
    let source = write(&tree, &triangles(4), "tiles");
    let (out, _) = call(&server, "model_import", json!({"source": source, "kind": "hammer_mesh", "compile": false})).await.unwrap();
    let sent = mb.method_calls("mesh.setFaceTexture")[0].params["faces"].as_array().unwrap().len();
    assert_eq!(sent, 3, "only the faces Hammer kept are aligned");
    assert!(out["warnings"].to_string().contains("could not be paired"), "{}", out["warnings"]);
}

#[tokio::test]
async fn materials_are_compiled_before_anything_reaches_the_map() {
    let tree = Tree::new("compile");
    let mb = mock("m3.18").await;
    let podman = tree.podman_ok();
    let server = tree.server(&mb, Some(&podman));
    let source = write(&tree, &two_walls(), "walls");
    let (out, _) = call(&server, "model_import", json!({"source": source, "kind": "hammer_mesh"})).await.unwrap();
    assert_eq!(out["compile"]["ok"], true, "{out}");
    assert!(tree.app.join("game/csgo_addons/mcp_test/materials/models/walls/brick_wall.vmat_c").is_file());
    assert_eq!(out["chunks"], 2);
}

#[tokio::test]
async fn argument_errors_are_typed_and_send_nothing() {
    let tree = Tree::new("args");
    let mb = mock("m3.18").await;
    let server = tree.server(&mb, None);
    let source = write(&tree, &two_walls(), "walls");
    for bad in [
        json!({"source": source, "kind": "hammer_mesh", "physics": "hull"}),
        json!({"source": source, "kind": "hammer_mesh", "origin": [1, 2]}),
        json!({"source": source, "kind": "hammer_mesh", "angles": ["a", 0, 0]}),
        json!({"source": source, "kind": "hammer_mesh", "maxTrisPerMesh": 0}),
        json!({"source": source, "kind": "hammer_mesh", "maxTrisPerMesh": 99999999}),
        json!({"source": source, "kind": "prop", "origin": [0, 0, 0]}),
        json!({"source": source, "kind": "prop", "maxTrisPerMesh": 5}),
    ] {
        let err = call(&server, "model_import", bad.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}: {err:?}");
    }
    // not a glTF
    let fbx = tree.root.join("src/a.fbx");
    std::fs::write(&fbx, b"x").unwrap();
    let err = call(&server, "model_import", json!({"source": fbx.display().to_string(), "kind": "hammer_mesh"})).await.unwrap_err();
    assert_eq!(err.name, "UnsupportedFormat", "{err:?}");
    assert!(mb.method_calls("mesh.create").is_empty());
}

#[tokio::test]
async fn materials_none_writes_no_files_and_warns_about_the_texture_size() {
    let tree = Tree::new("existing");
    let mb = mock("m3.18").await;
    let server = tree.server(&mb, None);
    let source = write(&tree, &two_walls(), "walls");
    let (out, _) = call(
        &server,
        "model_import",
        json!({"source": source, "kind": "hammer_mesh", "compile": false, "materials": "none"}),
    )
    .await
    .unwrap();
    assert!(out["warnings"].to_string().contains("512x512"), "{}", out["warnings"]);
    assert_eq!(mb.method_calls("mesh.create")[0].params["material"], "materials/dev/reflectivity_50.vmat");
    assert!(!tree.addon().join("materials").exists());
}

#[tokio::test]
async fn existing_materials_are_used_by_name_and_unmapped_ones_fall_back() {
    let tree = Tree::new("existing");
    std::fs::create_dir_all(tree.addon().join("materials/mine")).unwrap();
    std::fs::write(tree.addon().join("materials/mine/a.vmat"), "x").unwrap();
    let mb = mock("m3.18").await;
    let server = tree.server(&mb, None);
    let source = write(&tree, &two_walls(), "walls");
    let (out, _) = call(
        &server,
        "model_import",
        json!({"source": source, "kind": "hammer_mesh", "compile": false, "materials": "existing",
               "materialMap": {"Brick Wall": "materials/mine/a.vmat"}}),
    )
    .await
    .unwrap();
    let creates = mb.method_calls("mesh.create");
    assert_eq!(creates[0].params["material"], "materials/mine/a.vmat");
    assert_eq!(creates[1].params["material"], "materials/dev/reflectivity_50.vmat");
    assert_eq!(out["remapped"]["brick_wall"], "materials/mine/a.vmat");
    assert!(out["warnings"].to_string().contains("not in materialMap"), "{}", out["warnings"]);
}
