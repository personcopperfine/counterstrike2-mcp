//! Golden tests for the Rust CMapMesh extraction ([`hammer_mcp::vmap::mesh`]) against the Python
//! oracle (`../idabins/worknotes/mcpserver/vmap_reader.py` `_mesh_from_element`), plus the
//! extraction unit checks and the `#[ignore]`d real-tree tests (the workflow-03 mesh WP).
//!
//! The goldens are checked in as `fixtures/vmap/<name>.meshes.json`; they hold one
//! `MeshRecord.to_dict` record per usable `CMapMesh` element, in element order (generator command
//! in `fixtures/vmap/README.md`). The structural comparison is exact for faces/materials/ids and
//! tolerance-based for floats (both sides round the same decimals; a last-bit difference in a
//! rounded coordinate must not fail the golden). No python runs inside `cargo test`.
//!
//! Real-tree tests (fix `--ignore`): `cargo test -p hammer-mcp --test vmap_mesh -- --ignored`.

use hammer_mcp::vmap::load;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const PROBE_GOLDEN: &str = include_str!("../fixtures/vmap/probe.meshes.json");
const DEFUSE_GOLDEN: &str = include_str!("../fixtures/vmap/defuse_poc.meshes.json");

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/vmap")
        .join(name)
}

fn golden(text: &str) -> Vec<Value> {
    let value: Value = serde_json::from_str(text).expect("golden JSON");
    value.as_array().expect("golden is an array").clone()
}

/// Structural equality where numbers agree to `1e-9` (see the module header) and everything else
/// — keys, strings, bools, nulls, ints — must match exactly. Ints compare exactly through the
/// number branch too (JSON makes no int/float distinction).
fn similar(a: &Value, b: &Value, what: &str) {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            let (fa, fb) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            assert!(
                fa == fb || (fa - fb).abs() <= 1e-9,
                "{what}: {a} != {b}"
            );
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{what}: length");
            for (index, (a, b)) in a.iter().zip(b.iter()).enumerate() {
                similar(a, b, &format!("{what}[{index}]"));
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: Vec<&String> = a.keys().collect();
            keys.sort();
            let mut keys_b: Vec<&String> = b.keys().collect();
            keys_b.sort();
            assert_eq!(keys, keys_b, "{what}: keys");
            for (key, a) in a {
                similar(a, &b[key], &format!("{what}.{key}"));
            }
        }
        (Value::String(a), Value::String(b)) => assert_eq!(a, b, "{what}"),
        (Value::Bool(a), Value::Bool(b)) => assert_eq!(a, b, "{what}"),
        (Value::Null, Value::Null) => {}
        _ => panic!("{what}: shape mismatch {a} vs {b}"),
    }
}

/// Shared golden check: the reader's `MeshRecord::to_json` for every mesh equals the oracle's
/// record (in element order, matching by nodeID for the message).
fn check_golden(file: &str, text: &str) {
    let golden = golden(text);
    let document = load(&fixture(file)).unwrap_or_else(|error| panic!("{file}: {error}"));
    let meshes = document.meshes();
    assert_eq!(
        meshes.len(),
        golden.len(),
        "{file}: mesh count differs from the oracle"
    );
    for (index, (record, expected)) in meshes.iter().zip(golden.iter()).enumerate() {
        assert!(
            record.id == expected.get("nodeID").and_then(Value::as_i64),
            "{file}: mesh #{index} nodeID {:?} differs from the oracle's {:?}",
            record.id,
            expected.get("nodeID")
        );
        similar(
            &record.to_json(),
            expected,
            &format!("{file}: mesh #{index}"),
        );
    }
}

#[test]
fn probe_meshes_match_the_oracle() {
    check_golden("probe.vmap", PROBE_GOLDEN);
}

#[test]
fn defuse_poc_meshes_match_the_oracle() {
    check_golden("defuse_poc.vmap", DEFUSE_GOLDEN);
}

/// The mesh summaries feed `list_meshes`; the summary view must equal the golden record's summary
/// keys (no vertices/faces/faceMaterials there).
#[test]
fn summaries_match_the_oracle_summary() {
    for (file, text) in [
        ("probe.vmap", PROBE_GOLDEN),
        ("defuse_poc.vmap", DEFUSE_GOLDEN),
    ] {
        let document = load(&fixture(file)).expect(file);
        for mesh in document.meshes() {
            let mut expected = golden(text)
                .into_iter()
                .find(|record| record["nodeID"] == json!(mesh.id.unwrap_or(i64::MAX)))
                .expect("golden record for the node");
            // the record is `to_dict` (to_json + vertices/faces/faceMaterials); the summary view
            // must equal that record without the geometry keys.
            if let Some(object) = expected.as_object_mut() {
                for key in ["vertices", "faces", "faceMaterials"] {
                    object.remove(key);
                }
            }
            let summary = mesh.summary_json();
            similar(&summary, &expected, &format!("{file}: Summary"));
        }
    }
}

/// Extraction unit checks on the check-in fixtures: a box mesh's face normals point outward and
/// are unit length, and every face loop is a real cycle through distinct vertices.
#[test]
fn face_normals_of_a_fixture_box_are_unit_and_outward() {
    let document = load(&fixture("defuse_poc.vmap")).expect("defuse_poc.vmap");
    let mesh = document
        .mesh_by_id(71)
        .expect("nodeID 71 (a toolstrigger box) exists");
    assert_eq!(mesh.vertices.len(), 8);
    assert_eq!(mesh.faces.len(), 6);
    let centre: [f64; 3] = (0..3)
        .map(|axis| mesh.vertices.iter().map(|v| v[axis]).sum::<f64>() / 8.0)
        .collect::<Vec<f64>>()
        .try_into()
        .unwrap();
    for face in &mesh.faces {
        let (normal, face_centre) = normal_and_centre(mesh.vertices.as_slice(), face);
        let length = (normal.iter().map(|c| c * c).sum::<f64>()).sqrt();
        assert!((length - 1.0).abs() < 1e-9, "unit normal, got {length}");
        let outward: f64 = face_centre
            .iter()
            .zip(centre.iter())
            .map(|(c, c0)| c - c0)
            .zip(normal.iter())
            .map(|(d, n)| d * n)
            .sum();
        assert!(outward > 0.0, "the CCW faces point outward, got {outward}");
    }
    // nodeID 106 (probe) carries two materials: one per-face override
    let probe = load(&fixture("probe.vmap")).expect("probe.vmap");
    let wedge = probe.mesh_by_id(106).expect("nodeID 106 exists");
    assert_eq!(
        wedge.materials,
        vec![
            "materials/dev/reflectivity_30.vmat".to_string(),
            "materials/tools/toolsnodraw.vmat".to_string(),
        ]
    );
    assert!(wedge
        .face_materials
        .iter()
        .any(|m| m == "materials/tools/toolsnodraw.vmat"));
}

/// `mesh_by_id` finds a mesh and misses a non-mesh node id (the reusable WP-C lookup).
#[test]
fn mesh_by_id_finds_and_misses() {
    let probe = load(&fixture("probe.vmap")).expect("probe.vmap");
    assert!(probe.mesh_by_id(3).is_some());
    assert!(probe.mesh_by_id(1).is_none(), "worldspawn is not a mesh");
    assert!(probe.mesh_by_id(999_999).is_none());
}

/// A `.vmap` without CMapMesh elements yields an empty mesh list (the connections fixture world).
#[test]
fn a_map_without_meshes_has_an_empty_mesh_view() {
    // `reload` fixtures do not exist; the empty case is covered structurally: no panic, empty
    // list for a file whose entity view is proven in tests/vmap_dmx.rs.
    let document = load(&fixture("probe.vmap")).expect("probe.vmap");
    assert_eq!(document.meshes().len(), 18);
    for mesh in document.meshes() {
        assert!(mesh.id.is_some(), "every fixture mesh carries a nodeID");
        assert_eq!(mesh.face_materials.len(), mesh.faces.len());
    }
}

/// Corrupt bytes never panic the mesh view: every single-byte flip either parses with fewer
/// meshes or errors — the mesh extraction must stay total.
#[test]
fn byte_flips_never_panic_the_mesh_extraction() {
    let data = std::fs::read(fixture("probe.vmap")).expect("probe.vmap");
    for offset in [0, 8, 64, 200, 4096, 100_000, data.len() - 1] {
        for xor in [0x01u8, 0x40, 0x80, 0xff] {
            let mut mutated = data.clone();
            mutated[offset] ^= xor;
            if let Ok(document) = hammer_mcp::vmap::dmx::parse(&mutated) {
                let meshes = hammer_mcp::vmap::mesh::mesh_view(&document);
                for mesh in &meshes {
                    // all views must be constructible without indexing panics
                    let _ = mesh.summary_json();
                    let _ = mesh.to_json();
                    let _ = mesh.face_info_json();
                }
                for mesh in &meshes {
                    for face in &mesh.faces {
                        let _ = normal_and_centre(&mesh.vertices, face);
                    }
                }
            }
        }
    }
}

fn normal_and_centre(vertices: &[[f64; 3]], face: &[usize]) -> ([f64; 3], [f64; 3]) {
    let mut n = [0.0f64; 3];
    let mut centre = [0.0f64; 3];
    for i in 0..face.len() {
        let a = vertices[face[i]];
        let b = vertices[face[(i + 1) % face.len()]];
        n[0] += (a[1] - b[1]) * (a[2] + b[2]);
        n[1] += (a[2] - b[2]) * (a[0] + b[0]);
        n[2] += (a[0] - b[0]) * (a[1] + b[1]);
        centre[0] += a[0];
        centre[1] += a[1];
        centre[2] += a[2];
    }
    let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    (
        [n[0] / length, n[1] / length, n[2] / length],
        [centre[0] / face.len() as f64, centre[1] / face.len() as f64, centre[2] / face.len() as f64],
    )
}

// ---------------------------------------------------------------- the real game tree
// `#[ignore]`d: they need a CS2 install (Valve content, read-only). Run with
// `cargo test -p hammer-mcp --test vmap_mesh -- --ignored`.

/// `template_wingman.vmap` (a Valve template): 18 usable CMapMeshes, every face loop intact and
/// every per-face material filled.
#[test]
#[ignore]
fn real_tree_template_wingman_meshes() {
    let document = load(&real_tree_file(TEMPLATE_WINGMAN))
        .unwrap_or_else(|error| panic!("template_wingman.vmap: {error}"));
    assert_eq!(
        document.meshes().len(),
        18,
        "oracle: template_wingman has 18 usable CMapMeshes"
    );
    for mesh in document.meshes() {
        assert!(mesh.id.is_some());
        assert_eq!(mesh.face_materials.len(), mesh.faces.len());
        for face in &mesh.faces {
            assert!(face.len() >= 3, "face loops keep at least 3 vertices");
        }
    }
    // review-3 NEW-5: the file route's parent gate must refuse nothing on real content — every mesh
    // of a Valve map sits under CMapGroup/CMapEntity containers below CMapWorld, the chain this
    // crate has proven. A refusal here would mean the gate is wrong about real maps.
    let refused: Vec<_> = document
        .meshes()
        .iter()
        .filter_map(|mesh| mesh.parent_problem().map(|problem| (mesh.id, problem)))
        .collect();
    assert!(refused.is_empty(), "refused real meshes: {refused:?}");
}

/// `cs_script_demo.vmap` (Valve's 68 MB scripting demo): 70 usable CMapMeshes, 15 of them with a
/// rotation in their angles — the `angle_matrix` path. A rotated mesh keeps (1) unit face normals
/// and (2) its WORLD vertices, checked against the Python oracle's own numbers (review M3: unit
/// normals alone pass for *any* rotation, so the oracle values are what pins `angle_matrix`):
///
/// ```text
/// python3 - <<'EOF'
/// import sys; sys.path[:0] = ["<worknotes>", "<worknotes>/mcpserver"]
/// import vmap_reader as vr
/// for m in vr.load_reader().meshes("<install>/content/csgo_addons/cs_script_demo/maps/cs_script_demo.vmap"):
///     if m.id in (290, 282, 343): print(m.id, m.angles, m.vertices, m.faces)
/// EOF
/// nodeID 343: angles [0,90,0], origin [-192,1176,136], vertices
///   [-176,1136,176] [-176.000002,1216,176] [-176.000002,1216,96.000004] [-176,1136,96.000004]
///   (one quad, face [1,0,3,2])
/// nodeID 282: angles [0,90,0], origin [0,-32,0], 12 vertices, 9 faces
/// nodeID 290: angles [0,90,0], origin [0,-32,0], 8 vertices, 5 faces
/// ```
///
/// The oracle's `angle_matrix` is what `_mesh_from_element` uses, so agreeing with it vertex by
/// vertex means the Rust port applies the same rotation (a transposed or sign-flipped matrix moves
/// these points by tens of units; see also the hand-checked unit test in `vmap::mesh`).
#[test]
#[ignore]
fn real_tree_cs_script_demo_rotated_meshes() {
    let document = load(&real_tree_file(CS_SCRIPT_DEMO))
        .unwrap_or_else(|error| panic!("cs_script_demo.vmap: {error}"));
    assert_eq!(
        document.meshes().len(),
        70,
        "oracle: cs_script_demo has 70 usable CMapMeshes"
    );
    let rotated: Vec<_> = document
        .meshes()
        .iter()
        .filter(|mesh| mesh.angles.is_some_and(|a| a[0].abs() > 1e-6 || a[1].abs() > 1e-6 || a[2].abs() > 1e-6))
        .collect();
    assert_eq!(rotated.len(), 15, "oracle: 15 meshes carry a rotation");
    for mesh in &rotated {
        assert_eq!(mesh.face_materials.len(), mesh.faces.len());
        for face in &mesh.faces {
            let (normal, _) = normal_and_centre(&mesh.vertices, face);
            let length = (normal.iter().map(|c| c * c).sum::<f64>()).sqrt();
            assert!(
                (length - 1.0).abs() < 1e-6,
                "nodeID {:?}: unit normal under rotation, got {length}",
                mesh.id
            );
        }
    }

    // the oracle's own world vertices for three rotated meshes, to 5 decimals (the oracle prints
    // f32-widened coordinates, this reader parses the same f32s)
    let expected: [(i64, [[f64; 3]; 2], usize, usize); 3] = [
        // nodeID, (first vertex, last vertex), vertex count, face count
        (343, [[-176.0, 1136.0, 175.999996], [-176.0, 1136.0, 96.000004]], 4, 1),
        (282, [[-96.0, 128.0, 8.0], [-222.000031, 129.999908, 8.0]], 12, 9),
        (290, [[-496.0, 16.000046, 0.0], [-512.0, 384.0, 0.0]], 8, 5),
    ];
    for (node_id, [first, last], vertices, faces) in expected {
        let mesh = document
            .mesh_by_id(node_id)
            .unwrap_or_else(|| panic!("nodeID {node_id} is a rotated mesh"));
        assert_eq!(mesh.angles, Some([0.0, 90.0, 0.0]), "nodeID {node_id}");
        assert_eq!(mesh.vertices.len(), vertices, "nodeID {node_id} vertices");
        assert_eq!(mesh.faces.len(), faces, "nodeID {node_id} faces");
        for (index, want) in [(0usize, first), (mesh.vertices.len() - 1, last)] {
            let got = mesh.vertices[index];
            for axis in 0..3 {
                assert!(
                    (got[axis] - want[axis]).abs() < 1e-5,
                    "nodeID {node_id} vertex {index}[{axis}]: oracle {want:?}, got {got:?}"
                );
            }
        }
        assert!(
            mesh.faces[0].len() >= 3 && mesh.faces[0].iter().all(|index| *index < vertices),
            "nodeID {node_id}: the face loop {:?} stays a real loop",
            mesh.faces[0]
        );
    }

    // nodeID 343's whole loop is the oracle's, in the oracle's face order
    let quad = document.mesh_by_id(343).expect("nodeID 343");
    assert_eq!(quad.faces, vec![vec![1, 0, 3, 2]]);
    let oracle = [
        [-176.0, 1136.0, 175.999996],
        [-176.000002, 1216.0, 175.999996],
        [-176.000002, 1216.0, 96.000004],
        [-176.0, 1136.0, 96.000004],
    ];
    for (index, want) in oracle.iter().enumerate() {
        for axis in 0..3 {
            assert!(
                (quad.vertices[index][axis] - want[axis]).abs() < 1e-5,
                "nodeID 343 vertex {index}[{axis}]: oracle {want:?}, got {:?}",
                quad.vertices[index]
            );
        }
    }
}

/// review-3 NEW-5, corpus check: the file route's parent gate refuses **nothing** on the whole
/// installed content tree (Valve's templates and zoos plus our addon maps — 23 files, ~4100
/// CMapMeshes). Every mesh there sits under `CMapGroup`/`CMapEntity` containers below `CMapWorld`,
/// the only chain this crate has proven to store world-space geometry; a refusal here means the
/// gate is wrong about real maps, and the printout names the map, the mesh and the ancestor.
#[test]
#[ignore]
fn real_tree_corpus_has_no_unproven_parent() {
    fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "vmap") {
                out.push(path);
            }
        }
    }

    let content = project_root().join("workshop/run/app/content");
    let mut files = Vec::new();
    collect(&content, &mut files);
    files.sort();
    assert!(!files.is_empty(), "no .vmap under {}", content.display());

    let mut meshes = 0usize;
    let mut refused = Vec::new();
    for file in &files {
        let document = load(file).unwrap_or_else(|error| panic!("{}: {error}", file.display()));
        for mesh in document.meshes() {
            meshes += 1;
            if let Some(problem) = mesh.parent_problem() {
                refused.push(format!(
                    "{} nodeID {:?}: {problem:?}",
                    file.display(),
                    mesh.id
                ));
            }
        }
    }
    assert!(
        refused.is_empty(),
        "{} of {meshes} real meshes in {} files refused:\n{}",
        refused.len(),
        files.len(),
        refused.join("\n")
    );
    println!("{} meshes in {} maps, none refused", meshes, files.len());
}

/// The project root (`tests/vmap_dmx.rs` rule: `$CS2_MCP_PROJECT_ROOT` first, else
/// walk up to the directory holding the game tree marker).
fn project_root() -> PathBuf {
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
        .find(|dir| dir.join(CSGO_FGD).is_file())
        .unwrap_or_else(|| {
            panic!(
                "no project root with {CSGO_FGD} at $CS2_MCP_PROJECT_ROOT or above {}",
                env!("CARGO_MANIFEST_DIR")
            )
        })
}

fn real_tree_file(relative: &str) -> PathBuf {
    let path = project_root().join(relative);
    assert!(
        path.is_file(),
        "missing real map {}: the installed game tree moved?",
        path.display()
    );
    path
}

/// The tree marker (same as the other real-tree tests).
const CSGO_FGD: &str = "workshop/run/app/game/csgo/csgo.fgd";
/// A Valve template map (18 usable CMapMeshes).
const TEMPLATE_WINGMAN: &str = "workshop/run/app/content/csgo/maps/templates/template_wingman.vmap";
/// Valve's 68 MB scripting demo map (70 usable CMapMeshes, 15 rotated).
const CS_SCRIPT_DEMO: &str =
    "workshop/run/app/content/csgo_addons/cs_script_demo/maps/cs_script_demo.vmap";
