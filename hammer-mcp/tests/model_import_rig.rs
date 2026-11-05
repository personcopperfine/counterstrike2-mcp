//! `model_import` I5 (skinned glTF, own rig): the pure conversion against rigged in-test fixtures.
//! The expected numbers are computed here from the glTF side with their own small helpers; only
//! `matrix_of_euler` (the inverse of the writer's Euler step) and the glTF matrix helpers come from
//! the crate. No game needed.
#![allow(clippy::needless_range_loop)]

use hammer_mcp::import::mat::{apply3, from_trs, invert, mul, mul3, transpose3, Mat3, Mat4};
use hammer_mcp::import::skeleton::matrix_of_euler;
use hammer_mcp::import::{self, ImportError, ImportOptions, ImportPlan, Kind, Physics};
use serde_json::json;
use std::path::Path;

mod common;
use common::gltf_fixture::{RigAnim, RigChannel, RigFixture, RigMesh};

const SCALE: f64 = 39.37;

fn opts(name: &str) -> ImportOptions {
    ImportOptions { name: name.into(), kind: Kind::Prop, scale: None, up: None, generate_materials: true, existing: None, physics: None, player: None }
}

fn plan_of(f: &RigFixture, name: &str) -> ImportPlan {
    import::plan(Path::new("rig.glb"), f.glb(), &opts(name)).expect("plan")
}

fn text(plan: &ImportPlan, path: &str) -> String {
    let f = plan.files.iter().find(|f| f.path == path).unwrap_or_else(|| panic!("no {path} in {:?}", plan.files.iter().map(|f| &f.path).collect::<Vec<_>>()));
    String::from_utf8(f.bytes.clone()).unwrap()
}

// ------------------------------------------------------------------------------ SMD parsing

struct Vert {
    bone: usize,
    pos: [f64; 3],
    links: Vec<(usize, f64)>,
}

struct Smd {
    nodes: Vec<(String, i64)>,
    nodes_text: String,
    /// `frames[k][bone] = (translation, euler)`
    frames: Vec<Vec<([f64; 3], [f64; 3])>>,
    verts: Vec<Vert>,
}

fn parse(text: &str) -> Smd {
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("version 1"));
    assert_eq!(lines.next(), Some("nodes"));
    let mut nodes = Vec::new();
    let mut nodes_text = String::from("nodes\n");
    for l in lines.by_ref() {
        nodes_text.push_str(l);
        nodes_text.push('\n');
        if l == "end" {
            break;
        }
        let (id, rest) = l.split_once(' ').unwrap();
        assert_eq!(id.parse::<usize>().unwrap(), nodes.len());
        let (name, parent) = rest.rsplit_once(' ').unwrap();
        nodes.push((name.trim_matches('"').to_string(), parent.parse().unwrap()));
    }
    assert_eq!(lines.next(), Some("skeleton"));
    let mut frames: Vec<Vec<([f64; 3], [f64; 3])>> = Vec::new();
    let mut verts = Vec::new();
    while let Some(l) = lines.next() {
        if let Some(k) = l.strip_prefix("time ") {
            assert_eq!(k.parse::<usize>().unwrap(), frames.len());
            frames.push(Vec::new());
        } else if l == "triangles" {
            while let Some(m) = lines.next() {
                if m == "end" {
                    break;
                }
                for _ in 0..3 {
                    let f: Vec<&str> = lines.next().unwrap().split(' ').collect();
                    let n: usize = f[9].parse().unwrap();
                    assert_eq!(f.len(), 10 + 2 * n, "{f:?}");
                    verts.push(Vert {
                        bone: f[0].parse().unwrap(),
                        pos: [f[1].parse().unwrap(), f[2].parse().unwrap(), f[3].parse().unwrap()],
                        links: (0..n).map(|i| (f[10 + 2 * i].parse().unwrap(), f[11 + 2 * i].parse().unwrap())).collect(),
                    });
                }
            }
        } else if l != "end" {
            let v: Vec<f64> = l.split(' ').map(|x| x.parse().unwrap()).collect();
            assert_eq!(v.len(), 7, "{l}");
            assert_eq!(v[0] as usize, frames.last().unwrap().len());
            frames.last_mut().unwrap().push(([v[1], v[2], v[3]], [v[4], v[5], v[6]]));
        }
    }
    Smd { nodes, nodes_text, frames, verts }
}

/// World `(rotation, translation)` of every bone of frame `k`, composed from the written locals.
fn worlds(smd: &Smd, k: usize) -> Vec<(Mat3, [f64; 3])> {
    let mut out: Vec<(Mat3, [f64; 3])> = Vec::new();
    for (i, (t, e)) in smd.frames[k].iter().enumerate() {
        let r = matrix_of_euler(*e);
        let parent = smd.nodes[i].1;
        out.push(if parent < 0 {
            (r, *t)
        } else {
            let (pr, pt) = out[parent as usize];
            let moved = apply3(&pr, *t);
            (mul3(&pr, &r), [pt[0] + moved[0], pt[1] + moved[1], pt[2] + moved[2]])
        });
    }
    out
}

// ------------------------------------------------------------------------ expected values

/// glTF `(x,y,z) -> (z,x,y)`.
const P: Mat3 = [[0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
/// The compiler pre-turn Rz(-90): `v -> [v1, -v0, v2]`.
const TURN: Mat3 = [[0.0, 1.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];

fn expected_point(p: [f64; 3]) -> [f64; 3] {
    apply3(&TURN, apply3(&P, p).map(|v| v * SCALE))
}

/// The SMD world frame a glTF matrix (unit scale) must come out as.
fn expected_frame(m: &Mat4) -> (Mat3, [f64; 3]) {
    let mut r = [[0.0; 3]; 3];
    for row in 0..3 {
        for col in 0..3 {
            r[row][col] = m[col][row];
        }
    }
    let r = mul3(&TURN, &mul3(&P, &mul3(&r, &transpose3(&P))));
    (r, expected_point([m[3][0], m[3][1], m[3][2]]))
}

fn assert_frame(got: &(Mat3, [f64; 3]), want: &(Mat3, [f64; 3]), what: &str) {
    for i in 0..3 {
        for j in 0..3 {
            assert!((got.0[i][j] - want.0[i][j]).abs() < 1e-4, "{what}: rotation {:?} vs {:?}", got.0, want.0);
        }
        assert!((got.1[i] - want.1[i]).abs() < 2e-3, "{what}: translation {:?} vs {:?}", got.1, want.1);
    }
}

fn quat(axis: [f64; 3], degrees: f64) -> [f64; 4] {
    let h = degrees.to_radians() / 2.0;
    [axis[0] * h.sin(), axis[1] * h.sin(), axis[2] * h.sin(), h.cos()]
}

fn qmul(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}

fn f32s<const N: usize>(v: [f64; N]) -> Vec<f32> {
    v.iter().map(|x| *x as f32).collect()
}

fn trs(t: [f64; 3], q: [f64; 4]) -> Mat4 {
    from_trs(t, q, [1.0; 3])
}

fn ibm_of(m: &Mat4) -> [f32; 16] {
    let inv = invert(m).unwrap();
    let mut out = [0.0f32; 16];
    for c in 0..4 {
        for r in 0..4 {
            out[c * 4 + r] = inv[c][r] as f32;
        }
    }
    out
}

fn rot_z(deg: f64) -> Mat4 {
    trs([0.0; 3], quat([0.0, 0.0, 1.0], deg))
}

fn tube_w_tip(frame: usize) -> Mat4 {
    mul(&trs([0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]), &rot_z(90.0 * frame as f64 / 30.0))
}

// --------------------------------------------------------------------------------- tests

#[test]
fn the_tube_has_the_reference_rig_and_a_31_frame_clip() {
    let plan = plan_of(&RigFixture::tube(), "tube");
    let reference = parse(&text(&plan, "models/tube/tube.smd"));
    assert_eq!(reference.nodes, [("root".to_string(), -1), ("tip".to_string(), 0)]);
    assert_eq!(reference.frames.len(), 1);
    // bind pose: the root carries the pre-turn Rz(-90), the tip sits 1 m (39.37) up the parent's Z
    let (root_t, root_e) = reference.frames[0][0];
    assert!(root_t.iter().all(|v| v.abs() < 1e-9));
    assert!((root_e[2] + std::f64::consts::FRAC_PI_2).abs() < 1e-5 && root_e[0].abs() < 1e-5 && root_e[1].abs() < 1e-5, "{root_e:?}");
    let (tip_t, tip_e) = reference.frames[0][1];
    assert!((tip_t[2] - SCALE).abs() < 1e-4 && tip_t[0].abs() < 1e-4 && tip_t[1].abs() < 1e-4, "{tip_t:?}");
    assert!(tip_e.iter().all(|v| v.abs() < 1e-5), "{tip_e:?}");

    let anim = parse(&text(&plan, "models/tube/bend.smd"));
    assert_eq!(anim.nodes_text, reference.nodes_text);
    assert_eq!(anim.frames.len(), 31);
    assert_eq!(plan.rig.as_ref().unwrap().animations, [("bend".to_string(), 31)]);
    // the tip's frame-30 local rotation is the keyframe through the basis change
    let local = matrix_of_euler(anim.frames[30][1].1);
    let key = {
        let r = rot_z(90.0);
        let mut m = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] = r[j][i];
            }
        }
        mul3(&P, &mul3(&m, &transpose3(&P)))
    };
    for i in 0..3 {
        for j in 0..3 {
            assert!((local[i][j] - key[i][j]).abs() < 1e-4, "{local:?} vs {key:?}");
        }
    }
    // every frame: the rebuilt world of both bones is W_j(t)
    for k in 0..31 {
        let w = worlds(&anim, k);
        assert_frame(&w[0], &expected_frame(&trs([0.0; 3], [0.0, 0.0, 0.0, 1.0])), "root");
        assert_frame(&w[1], &expected_frame(&tube_w_tip(k)), &format!("tip frame {k}"));
    }
}

#[test]
fn vertex_links_are_pruned_normalised_and_in_bone_order() {
    let plan = plan_of(&RigFixture::tube(), "tube");
    let smd = parse(&text(&plan, "models/tube/tube.smd"));
    assert_eq!(smd.verts.len(), 6);
    assert_eq!(smd.verts[0].links, [(0, 1.0)]);
    assert_eq!(smd.verts[2].links.len(), 2);
    assert!(smd.verts[2].links.iter().all(|l| (l.1 - 0.5).abs() < 1e-6));
    // vertex 5: 0.9996 / 0.0004 -> the tiny root weight is pruned
    assert_eq!(smd.verts[5].links, [(1, 1.0)]);
    assert_eq!(smd.verts[5].bone, 1);
    assert_eq!(smd.verts[3].bone, 1);
}

#[test]
fn a_rigidly_bound_vertex_keeps_its_bone_local_offset_in_every_frame() {
    // the pre-turn (and axes, scale) must hit the skeleton and the mesh identically
    let f = RigFixture::tube();
    let plan = plan_of(&f, "tube");
    let reference = parse(&text(&plan, "models/tube/tube.smd"));
    let anim = parse(&text(&plan, "models/tube/bend.smd"));
    let bind = worlds(&reference, 0);
    let ibm_tip = {
        let m = trs([0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]);
        invert(&m).unwrap()
    };
    for (index, v) in [(3usize, [0.0, 1.0, 0.0]), (4, [0.5, 1.0, 0.0]), (5, [0.0, 1.5, 0.25])] {
        let vert = &reference.verts[index];
        let (br, bt) = bind[1];
        let d = [vert.pos[0] - bt[0], vert.pos[1] - bt[1], vert.pos[2] - bt[2]];
        let local = apply3(&transpose3(&br), d);
        for k in 0..31 {
            let (r, t) = worlds(&anim, k)[1];
            let moved = apply3(&r, local);
            let got = [t[0] + moved[0], t[1] + moved[1], t[2] + moved[2]];
            // glTF: W(t) * IBM * v
            let w = mul(&tube_w_tip(k), &ibm_tip);
            let g = [
                w[0][0] * v[0] + w[1][0] * v[1] + w[2][0] * v[2] + w[3][0],
                w[0][1] * v[0] + w[1][1] * v[1] + w[2][1] * v[2] + w[3][1],
                w[0][2] * v[0] + w[1][2] * v[1] + w[2][2] * v[2] + w[3][2],
            ];
            let want = expected_point(g);
            for i in 0..3 {
                assert!((got[i] - want[i]).abs() < 2e-3, "vertex {index} frame {k}: {got:?} vs {want:?}");
            }
        }
    }
}

/// holder (not a joint) -> A -> B -> C, with a rest rotation of B at pitch ~90 and of C at exactly 90.
fn chain() -> (RigFixture, Vec<Mat4>) {
    let holder = trs([1.0, 0.5, -0.25], quat([0.0, 1.0, 0.0], 30.0));
    let a = trs([0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]);
    let q_b = quat([1.0, 0.0, 0.0], 89.99);
    let b = trs([0.25, 1.0, 0.0], q_b);
    let q_c = qmul(quat([1.0, 0.0, 0.0], 90.0), quat([0.0, 0.0, 1.0], 20.0));
    let c = trs([0.0, 1.0, 0.5], q_c);
    let wa = mul(&holder, &a);
    let wb = mul(&wa, &b);
    let wc = mul(&wb, &c);
    let q_b1 = qmul(quat([1.0, 0.0, 0.0], 90.0), quat([0.0, 1.0, 0.0], 15.0));
    let q_c1 = qmul(quat([1.0, 0.0, 0.0], -90.0), quat([0.0, 0.0, 1.0], 33.0));
    let f = RigFixture {
        nodes: vec![
            json!({"name": "Skin", "mesh": 0, "skin": 0}),
            json!({"name": "Holder", "translation": [1.0, 0.5, -0.25], "rotation": quat([0.0, 1.0, 0.0], 30.0), "children": [2]}),
            json!({"name": "A", "translation": [0.0, 1.0, 0.0], "children": [3]}),
            json!({"name": "B", "translation": [0.25, 1.0, 0.0], "rotation": q_b, "children": [4]}),
            json!({"name": "C", "translation": [0.0, 1.0, 0.5], "rotation": q_c}),
        ],
        scene: vec![0, 1],
        meshes: vec![RigMesh {
            positions: vec![[1.0, 1.5, -0.25], [1.5, 2.5, 0.0], [1.0, 3.0, 0.5]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            joints: Some(vec![[0, 0, 0, 0], [1, 2, 0, 0], [2, 0, 0, 0]]),
            weights: Some(vec![[1.0, 0.0, 0.0, 0.0], [0.7, 0.3, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]]),
            indices: vec![0, 1, 2],
            ..RigMesh::default()
        }],
        skin_joints: vec![2, 3, 4],
        ibms: vec![ibm_of(&wa), ibm_of(&wb), ibm_of(&wc)],
        anims: vec![RigAnim {
            name: Some("walk cycle".into()),
            channels: vec![
                RigChannel { node: 3, path: "rotation", interpolation: "LINEAR", times: vec![0.0, 1.0], values: [f32s(q_b), f32s(q_b1)].concat() },
                RigChannel { node: 4, path: "rotation", interpolation: "LINEAR", times: vec![0.0, 1.0], values: [f32s(q_c), f32s(q_c1)].concat() },
                RigChannel {
                    node: 2,
                    path: "translation",
                    interpolation: "STEP",
                    times: vec![0.0, 0.5, 1.0],
                    values: vec![0.0, 1.0, 0.0, 0.0, 1.5, 0.0, 0.0, 2.0, 0.0],
                },
            ],
        }],
        ..RigFixture::default()
    };
    // rest globals (frame 0 values) are wa, wb, wc
    (f, vec![wa, wb, wc])
}

#[test]
fn a_chain_with_a_non_joint_ancestor_and_near_gimbal_poses_reproduces_the_world_matrices() {
    let (f, _) = chain();
    let plan = plan_of(&f, "chain");
    assert_eq!(plan.rig.as_ref().unwrap().animations, [("walk_cycle".to_string(), 31)]);
    let reference = parse(&text(&plan, "models/chain/chain.smd"));
    // A is the only root joint (the holder is not a bone): no synthetic root
    assert_eq!(reference.nodes, [("a".to_string(), -1), ("b".to_string(), 0), ("c".to_string(), 1)]);
    let anim = parse(&text(&plan, "models/chain/walk_cycle.smd"));
    let holder = trs([1.0, 0.5, -0.25], quat([0.0, 1.0, 0.0], 30.0));
    let q_b = quat([1.0, 0.0, 0.0], 89.99);
    let q_c = qmul(quat([1.0, 0.0, 0.0], 90.0), quat([0.0, 0.0, 1.0], 20.0));
    let q_b1 = qmul(quat([1.0, 0.0, 0.0], 90.0), quat([0.0, 1.0, 0.0], 15.0));
    let q_c1 = qmul(quat([1.0, 0.0, 0.0], -90.0), quat([0.0, 0.0, 1.0], 33.0));
    let a_t = |frame: usize| -> [f64; 3] {
        if frame < 15 {
            [0.0, 1.0, 0.0]
        } else if frame < 30 {
            [0.0, 1.5, 0.0]
        } else {
            [0.0, 2.0, 0.0]
        }
    };
    let b_rest = |q| trs([0.25, 1.0, 0.0], q);
    let c_rest = |q| trs([0.0, 1.0, 0.5], q);
    let mut worst_pitch: f64 = 0.0;
    for (k, qb, qc) in [(0usize, q_b, q_c), (30, q_b1, q_c1)] {
        let wa = mul(&holder, &trs(a_t(k), [0.0, 0.0, 0.0, 1.0]));
        let wb = mul(&wa, &b_rest(qb));
        let wc = mul(&wb, &c_rest(qc));
        let w = worlds(&anim, k);
        assert_frame(&w[0], &expected_frame(&wa), &format!("a frame {k}"));
        assert_frame(&w[1], &expected_frame(&wb), &format!("b frame {k}"));
        assert_frame(&w[2], &expected_frame(&wc), &format!("c frame {k}"));
        for bone in &anim.frames[k] {
            worst_pitch = worst_pitch.max(bone.1[1].abs());
        }
    }
    assert!(worst_pitch > 1.5, "the fixture must reach a near-gimbal pitch, got {worst_pitch}");
    // STEP translation of A: held until the key, then the new value
    for (k, y) in [(0usize, 1.0), (14, 1.0), (15, 1.5), (29, 1.5), (30, 2.0)] {
        let wa = mul(&holder, &trs([0.0, y, 0.0], [0.0, 0.0, 0.0, 1.0]));
        assert_frame(&worlds(&anim, k)[0], &expected_frame(&wa), &format!("a step frame {k}"));
    }
    // the bind pose of the reference is the rest pose (frame 0 of the clip, apart from nothing)
    let bind = worlds(&reference, 0);
    let wa = mul(&holder, &trs([0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]));
    assert_frame(&bind[0], &expected_frame(&wa), "bind a");
}

#[test]
fn bone_name_collisions_are_renamed_and_reported() {
    let mut f = RigFixture::tube();
    f.nodes[1]["name"] = json!("Bone");
    f.nodes[2]["name"] = json!("bone");
    let plan = plan_of(&f, "tube");
    let rig = plan.rig.as_ref().unwrap();
    assert_eq!(rig.bones, 2);
    assert_eq!(rig.root, "bone");
    assert_eq!(
        rig.names,
        [(Some("Bone".to_string()), "bone".to_string()), (Some("bone".to_string()), "bone_2".to_string())]
    );
    assert!(plan.warnings.iter().any(|w| w.contains("collide after lowercasing") && w.contains("bone_2")), "{:?}", plan.warnings);
    let smd = parse(&text(&plan, "models/tube/tube.smd"));
    assert_eq!(smd.nodes, [("bone".to_string(), -1), ("bone_2".to_string(), 0)]);
}

#[test]
fn an_unskinned_mesh_in_a_skinned_scene_rides_its_joint_or_the_root() {
    let mut f = RigFixture::tube();
    // mesh 1: a triangle under the Tip joint (node 3) and one at the scene root (node 4)
    f.meshes.push(RigMesh {
        positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
        normals: vec![[0.0, 1.0, 0.0]; 3],
        indices: vec![0, 1, 2],
        ..RigMesh::default()
    });
    f.nodes[2]["children"] = json!([3]);
    f.nodes.push(json!({"name": "Hat", "mesh": 1, "translation": [0.0, 0.5, 0.0]}));
    f.nodes.push(json!({"name": "Floor", "mesh": 1, "translation": [5.0, 0.0, 0.0]}));
    f.scene.push(4);
    let plan = plan_of(&f, "tube");
    assert!(plan.warnings.iter().any(|w| w.contains("attached rigidly") && w.contains("Hat") && w.contains("Floor")), "{:?}", plan.warnings);
    let smd = parse(&text(&plan, "models/tube/tube.smd"));
    assert_eq!(smd.verts.len(), 12);
    // Hat: tip joint; bind * inverse(rest) * node world = identity * T(0,1.5,0) (rest == bind here)
    let hat = &smd.verts[6];
    assert_eq!((hat.bone, hat.links.clone()), (1, vec![(1, 1.0)]));
    let want = expected_point([0.0, 1.5, 0.0]);
    for i in 0..3 {
        assert!((hat.pos[i] - want[i]).abs() < 1e-3, "{:?} vs {want:?}", hat.pos);
    }
    // Floor: the root bone (the first joint)
    let floor = &smd.verts[9];
    assert_eq!((floor.bone, floor.links.clone()), (0, vec![(0, 1.0)]));
    let want = expected_point([5.0, 0.0, 0.0]);
    for i in 0..3 {
        assert!((floor.pos[i] - want[i]).abs() < 1e-3, "{:?} vs {want:?}", floor.pos);
    }
}

#[test]
fn dropped_and_approximated_features_are_reported() {
    let mut f = RigFixture::tube();
    {
        let m = &mut f.meshes[0];
        m.joints1 = Some(vec![[0, 0, 0, 0]; 6]);
        m.weights1 = Some(vec![[0.0; 4]; 6]);
    }
    f.extra_skin_joints = vec![vec![1]];
    f.anims[0].channels.push(RigChannel {
        node: 0,
        path: "weights",
        interpolation: "LINEAR",
        times: vec![0.0, 1.0],
        values: vec![0.0, 1.0, 0.0, 1.0, 0.0, 1.0],
    });
    f.anims[0].channels.push(RigChannel {
        node: 1,
        path: "translation",
        interpolation: "CUBICSPLINE",
        times: vec![0.0, 1.0],
        // in-tangent, value, out-tangent per key
        values: vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 0.0],
    });
    let plan = plan_of(&f, "tube");
    let dropped = plan.dropped.join(" | ");
    assert!(dropped.contains("JOINTS_1"), "{dropped}");
    assert!(dropped.contains("morph target weight"), "{dropped}");
    assert!(dropped.contains("extra skin"), "{dropped}");
    let warnings = plan.warnings.join(" | ");
    assert!(warnings.contains("CUBICSPLINE"), "{warnings}");
    // the cubic channel's key values (0 -> 2 m up) are used
    let anim = parse(&text(&plan, "models/tube/bend.smd"));
    let last = worlds(&anim, 30);
    assert!((last[0].1[2] - 2.0 * SCALE).abs() < 2e-3, "{:?}", last[0].1);
}

#[test]
fn the_vmdl_lists_one_anim_file_per_clip_and_sequence_names_are_unique() {
    let mut f = RigFixture::tube();
    let clip = f.anims[0].clone();
    f.anims[0].name = Some("Tube".into()); // the model stem: the mesh already owns tube.smd
    f.anims.push(RigAnim { name: None, ..clip.clone() });
    f.anims.push(RigAnim { name: Some("tube".into()), ..clip });
    let plan = plan_of(&f, "tube");
    let names: Vec<&str> = plan.rig.as_ref().unwrap().animations.iter().map(|a| a.0.as_str()).collect();
    assert_eq!(names, ["tube_2", "anim_1", "tube_3"]);
    let vmdl = plan.vmdl.write();
    for n in names {
        assert!(vmdl.contains(&format!("models/tube/{n}.smd")), "{vmdl}");
        assert!(plan.files.iter().any(|f| f.path == format!("models/tube/{n}.smd") && f.role == "animation"));
    }
    assert_eq!(vmdl.matches("_class = \"AnimFile\"").count(), 3, "{vmdl}");
    assert!(vmdl.contains("source_filename = \"models/tube/anim_1.smd\""), "{vmdl}");
    assert!(plan.warnings.iter().any(|w| w.contains("rigid hull")), "{:?}", plan.warnings);
}

#[test]
fn zero_length_and_empty_clips_are_one_frame() {
    let mut f = RigFixture::tube();
    f.anims[0].channels[0].times = vec![0.5];
    f.anims[0].channels[0].values = f32s(quat([0.0, 0.0, 1.0], 40.0));
    f.anims.push(RigAnim { name: Some("empty".into()), channels: vec![] });
    let plan = plan_of(&f, "tube");
    assert_eq!(plan.rig.as_ref().unwrap().animations, [("bend".to_string(), 1), ("empty".to_string(), 1)]);
    assert_eq!(parse(&text(&plan, "models/tube/bend.smd")).frames.len(), 1);
    assert!(plan.warnings.iter().any(|w| w.contains("no usable channels")), "{:?}", plan.warnings);
}

#[test]
fn skinned_files_are_props_with_hull_or_no_collision() {
    let f = RigFixture::tube();
    let mut o = opts("tube");
    o.kind = Kind::World;
    assert!(matches!(import::plan(Path::new("r.glb"), f.glb(), &o), Err(ImportError::Invalid(_))));
    let mut o = opts("tube");
    o.physics = Some(Physics::Mesh);
    assert!(matches!(import::plan(Path::new("r.glb"), f.glb(), &o), Err(ImportError::Invalid(_))));
    let mut o = opts("tube");
    o.physics = Some(Physics::None);
    let plan = import::plan(Path::new("r.glb"), f.glb(), &o).unwrap();
    assert!(!plan.vmdl.write().contains("physicsFile") && !plan.warnings.iter().any(|w| w.contains("rigid hull")));
}

#[test]
fn several_root_joints_get_a_synthetic_root() {
    let mut f = RigFixture::tube();
    // detach Tip from Root: two root joints
    f.nodes[1] = json!({"name": "Root"});
    f.scene = vec![0, 1, 2];
    let plan = plan_of(&f, "tube");
    let smd = parse(&text(&plan, "models/tube/tube.smd"));
    assert_eq!(smd.nodes[0], ("root_2".to_string(), -1), "{:?}", smd.nodes); // "Root" is a real joint: the synthetic one yields
    assert_eq!(smd.nodes.iter().filter(|n| n.1 == 0).count(), 2);
    assert!(plan.warnings.iter().any(|w| w.contains("synthetic")), "{:?}", plan.warnings);
    assert_eq!(plan.rig.as_ref().unwrap().names[0].0, None);
}

// ------------------------------------------------------------------------- review fixes (I5fix)

fn plan_err(f: &RigFixture) -> String {
    match import::plan(Path::new("rig.glb"), f.glb(), &opts("tube")) {
        Err(ImportError::Parse(m)) => m,
        Err(e) => panic!("expected a Parse error, got {e}"),
        Ok(_) => panic!("expected the import to be refused"),
    }
}

#[test]
fn node_cycles_and_two_parents_are_refused_not_looped() {
    // A.children=[B], B.children=[C], C.children=[B], reachable from a scene root
    let mut f = RigFixture::tube();
    f.nodes[1] = json!({"name": "Root", "children": [2, 3]});
    f.nodes.push(json!({"name": "B", "children": [4]}));
    f.nodes.push(json!({"name": "C", "children": [3]}));
    let msg = plan_err(&f);
    assert!(msg.contains("more than one parent"), "{msg}");

    // a joint-only cycle beside the real root joint: unreachable from any root
    let mut f = RigFixture::tube();
    f.nodes.push(json!({"name": "X", "children": [4]}));
    f.nodes.push(json!({"name": "Y", "children": [3]}));
    f.skin_joints = vec![1, 2, 3, 4];
    f.ibms = vec![f.ibms[0], f.ibms[1], f.ibms[0], f.ibms[0]];
    let msg = plan_err(&f);
    assert!(msg.contains("cycle"), "{msg}");
}

#[test]
fn overflowing_and_tiny_weights_do_not_panic() {
    let mut f = RigFixture::tube();
    {
        let w = f.meshes[0].weights.as_mut().unwrap();
        w[0] = [3e38, 3e38, 0.0, 0.0];
        w[1] = [1e-9, 0.0, 0.0, 0.0];
        f.meshes[0].joints.as_mut().unwrap()[0] = [0, 1, 0, 0];
    }
    let plan = plan_of(&f, "tube");
    let smd = parse(&text(&plan, "models/tube/tube.smd"));
    assert_eq!(smd.verts[0].links.len(), 2);
    assert!(smd.verts[0].links.iter().all(|l| (l.1 - 0.5).abs() < 1e-5), "{:?}", smd.verts[0].links);
    // a vanishing weight sum is unweighted: bound rigidly to the root bone
    assert_eq!(smd.verts[1].links, [(0, 1.0)]);
    assert!(plan.warnings.iter().any(|w| w.contains("without weights")), "{:?}", plan.warnings);
}

fn long_clip(name: &str, seconds: f32) -> RigAnim {
    RigAnim {
        name: Some(name.into()),
        channels: vec![RigChannel {
            node: 2,
            path: "translation",
            interpolation: "LINEAR",
            times: vec![0.0, seconds],
            values: vec![0.0, 1.0, 0.0, 0.0, 2.0, 0.0],
        }],
    }
}

#[test]
fn the_sample_cap_counts_every_clip_together() {
    // 2 bones x 300001 frames = 600k samples a clip: three fit under 2M, the fourth does not
    let mut f = RigFixture::tube();
    f.anims = (0..3).map(|i| long_clip(&format!("c{i}"), 10_000.0)).collect();
    import::plan(Path::new("rig.glb"), f.glb(), &opts("tube")).expect("three clips fit");
    f.anims.push(long_clip("c3", 10_000.0));
    let msg = plan_err(&f);
    assert!(msg.contains("sample cap") && msg.contains("earlier clips"), "{msg}");
}

#[test]
fn a_keyframe_time_far_out_is_refused_before_the_frame_count_cast() {
    let mut f = RigFixture::tube();
    f.anims = vec![long_clip("far", 1e30)];
    let msg = plan_err(&f);
    assert!(msg.contains("sample cap"), "{msg}");
}

#[test]
fn non_finite_key_values_are_refused_and_rotation_keys_are_normalised() {
    let mut f = RigFixture::tube();
    f.anims[0].channels[0].values[6] = f32::NAN;
    let msg = plan_err(&f);
    assert!(msg.contains("non-finite"), "{msg}");

    let h = std::f32::consts::FRAC_1_SQRT_2;
    let mut f2 = RigFixture::tube();
    f2.anims[0].channels[0].values = vec![0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 2.0 * h, 2.0 * h];
    let a = text(&plan_of(&f2, "tube"), "models/tube/bend.smd");
    let b = text(&plan_of(&RigFixture::tube(), "tube"), "models/tube/bend.smd");
    assert_eq!(a, b);
}

#[test]
fn a_zero_axis_matrix_ancestor_gives_finite_output() {
    let mut f = RigFixture::tube();
    // a non-joint holder above Root whose matrix squashes the Y axis to zero
    f.nodes.push(json!({"name": "Holder", "children": [1],
        "matrix": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]}));
    f.scene = vec![0, 3];
    let plan = plan_of(&f, "tube");
    for file in &plan.files {
        if file.path.ends_with(".smd") {
            let t = String::from_utf8(file.bytes.clone()).unwrap().to_lowercase();
            assert!(!t.contains("nan") && !t.contains("inf"), "{}", file.path);
        }
    }
    assert!(plan.dropped.iter().any(|d| d.contains("scale dropped")), "{:?}", plan.dropped);
}

#[test]
fn a_reflection_that_appears_only_after_bind_is_reported() {
    let mut f = RigFixture::tube();
    f.anims[0].channels.push(RigChannel {
        node: 2,
        path: "scale",
        interpolation: "LINEAR",
        times: vec![0.0, 1.0],
        values: vec![1.0, 1.0, 1.0, -1.0, 1.0, 1.0],
    });
    let plan = plan_of(&f, "tube");
    assert!(plan.dropped.iter().any(|d| d.contains("scale dropped")), "{:?}", plan.dropped);
}

#[test]
fn the_last_frame_samples_the_last_key() {
    let h = std::f32::consts::FRAC_1_SQRT_2;
    let mut f = RigFixture::tube();
    f.anims[0].channels[0].times = vec![0.0, 0.04];
    f.anims[0].channels[0].values = vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, h, h];
    let short = parse(&text(&plan_of(&f, "tube"), "models/tube/bend.smd"));
    assert_eq!(short.frames.len(), 2);
    let full = parse(&text(&plan_of(&RigFixture::tube(), "tube"), "models/tube/bend.smd"));
    for (a, b) in short.frames[1].iter().zip(&full.frames[30]) {
        for i in 0..3 {
            assert!((a.0[i] - b.0[i]).abs() < 1e-5 && (a.1[i] - b.1[i]).abs() < 1e-5, "{a:?} vs {b:?}");
        }
    }
}

#[test]
fn a_joint_index_past_the_skin_is_refused_even_with_a_synthetic_root() {
    let mut f = RigFixture::tube();
    f.nodes[1] = json!({"name": "Root"});
    f.scene = vec![0, 1, 2];
    // two real joints + the synthetic root: index 2 is the synthetic one, not a skin joint
    f.meshes[0].joints.as_mut().unwrap()[0] = [2, 0, 0, 0];
    let msg = plan_err(&f);
    assert!(msg.contains("joint 2") && msg.contains("2 joints"), "{msg}");
}
