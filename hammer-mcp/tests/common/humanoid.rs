//! A procedural humanoid glTF for the player-retarget tests (I6): Mixamo bone names, Y up, the
//! character faces +Z and its left is +X (the glTF convention). Bones are pure translations;
//! each bone with a child carries a box bound rigidly to it, and a three-vertex "probe" triangle
//! sits on the left forearm. Metres.
#![allow(dead_code)]

use super::gltf_fixture::{RigFixture, RigMesh};
use serde_json::json;

#[derive(Clone, Copy, PartialEq)]
pub enum Naming {
    Mixamo,
    /// `b00`, `b01`, ... in bone order: only the hierarchy tells what they are.
    Generic,
}

#[derive(Clone)]
pub struct Opts {
    /// 0 = T-pose; degrees the arms hang down from horizontal.
    pub arm_down_degrees: f64,
    pub naming: Naming,
    /// Mixamo short names (without the `mixamorig:` prefix) to drop; children move up.
    pub omit: Vec<&'static str>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { arm_down_degrees: 0.0, naming: Naming::Mixamo, omit: vec![] }
    }
}

pub struct Humanoid {
    pub fixture: RigFixture,
    /// Joint names in joint order (what the glTF calls them).
    pub names: Vec<String>,
    /// Bind world positions in joint order.
    pub world: Vec<[f64; 3]>,
    /// Vertex index range of the probe triangle in the single mesh.
    pub probe_first: usize,
    /// Index of the nose vertex.
    pub nose: usize,
}

impl Humanoid {
    pub fn index(&self, mixamo_short: &str) -> usize {
        let full = format!("mixamorig:{mixamo_short}");
        self.names.iter().position(|n| *n == full).unwrap_or_else(|| panic!("no joint {full}"))
    }
}

fn translation_ibm(w: [f64; 3]) -> [f32; 16] {
    let mut m = [0.0f32; 16];
    m[0] = 1.0;
    m[5] = 1.0;
    m[10] = 1.0;
    m[15] = 1.0;
    m[12] = -w[0] as f32;
    m[13] = -w[1] as f32;
    m[14] = -w[2] as f32;
    m
}

pub fn build(opts: &Opts) -> Humanoid {
    let a = opts.arm_down_degrees.to_radians();
    let dl = [a.cos(), -a.sin(), 0.0]; // left arm direction
    let dr = [-a.cos(), -a.sin(), 0.0];
    let along = |d: [f64; 3], l: f64| [d[0] * l, d[1] * l, d[2] * l];
    // (short name, parent short name, local offset in metres)
    let mut spec: Vec<(String, Option<String>, [f64; 3])> = Vec::new();
    let mut add = |name: &str, parent: Option<&str>, local: [f64; 3]| {
        spec.push((name.to_string(), parent.map(str::to_string), local));
    };
    add("Hips", None, [0.0, 1.0, 0.0]);
    add("Spine", Some("Hips"), [0.0, 0.1, 0.0]);
    add("Spine1", Some("Spine"), [0.0, 0.15, 0.0]);
    add("Spine2", Some("Spine1"), [0.0, 0.15, 0.0]);
    add("Neck", Some("Spine2"), [0.0, 0.15, 0.0]);
    add("Head", Some("Neck"), [0.0, 0.1, 0.0]);
    add("HeadTop_End", Some("Head"), [0.0, 0.2, 0.0]);
    add("LeftEye", Some("Head"), [0.03, 0.07, 0.08]);
    add("RightEye", Some("Head"), [-0.03, 0.07, 0.08]);
    for (side, d, sx) in [("Left", dl, 1.0), ("Right", dr, -1.0)] {
        let s = |n: &str| format!("{side}{n}");
        add(&s("Shoulder"), Some("Spine2"), [0.05 * sx, 0.12, 0.0]);
        add(&s("Arm"), Some(&s("Shoulder")), [0.12 * sx, 0.0, 0.0]);
        add(&s("ForeArm"), Some(&s("Arm")), along(d, 0.28));
        add(&s("Hand"), Some(&s("ForeArm")), along(d, 0.25));
        if side == "Left" {
            let mut prev = s("Hand");
            for i in 1..=3 {
                let name = format!("LeftHandIndex{i}");
                add(&name, Some(&prev), along(d, if i == 1 { 0.09 } else { 0.03 }));
                prev = name;
            }
            let mut prev = s("Hand");
            for i in 1..=3 {
                let name = format!("LeftHandThumb{i}");
                let l = if i == 1 { [d[0] * 0.03, d[1] * 0.03, 0.03] } else { [d[0] * 0.02, d[1] * 0.02, 0.015] };
                add(&name, Some(&prev), l);
                prev = name;
            }
        }
        add(&s("UpLeg"), Some("Hips"), [0.09 * sx, -0.05, 0.0]);
        add(&s("Leg"), Some(&s("UpLeg")), [0.0, -0.42, 0.0]);
        add(&s("Foot"), Some(&s("Leg")), [0.0, -0.42, 0.0]);
        add(&s("ToeBase"), Some(&s("Foot")), [0.0, -0.05, 0.12]);
        add(&s("Toe_End"), Some(&s("ToeBase")), [0.0, 0.0, 0.05]);
    }
    // drop omitted bones: their children keep their world position under the omitted bone's parent
    let mut kept: Vec<(String, Option<String>, [f64; 3])> = Vec::new();
    let mut folded: std::collections::HashMap<String, (Option<String>, [f64; 3])> = Default::default();
    for (name, parent, local) in spec {
        let (parent, local) = match parent.as_ref().and_then(|p| folded.get(p)) {
            Some((pp, off)) => (pp.clone(), [local[0] + off[0], local[1] + off[1], local[2] + off[2]]),
            None => (parent, local),
        };
        if opts.omit.contains(&name.as_str()) {
            folded.insert(name, (parent, local));
        } else {
            kept.push((name, parent, local));
        }
    }
    let n = kept.len();
    let index_of = |name: &str| kept.iter().position(|k| k.0 == name).unwrap();
    let mut world = vec![[0.0f64; 3]; n];
    for (i, (_, parent, local)) in kept.iter().enumerate() {
        let base = parent.as_ref().map_or([0.0; 3], |p| world[index_of(p)]);
        world[i] = [base[0] + local[0], base[1] + local[1], base[2] + local[2]];
    }
    let names: Vec<String> = kept
        .iter()
        .enumerate()
        .map(|(i, k)| match opts.naming {
            Naming::Mixamo => format!("mixamorig:{}", k.0),
            Naming::Generic => format!("b{i:02}"),
        })
        .collect();

    // nodes: 0 = the mesh node, 1.. = bones
    let mut nodes = vec![json!({"name": "Body", "mesh": 0, "skin": 0})];
    for (i, (_, parent, local)) in kept.iter().enumerate() {
        let _ = parent;
        let kids: Vec<usize> = (0..n).filter(|c| kept[*c].1.as_deref() == Some(kept[i].0.as_str())).map(|c| c + 1).collect();
        let mut node = json!({"name": names[i], "translation": local.map(|v| v as f32)});
        if !kids.is_empty() {
            node["children"] = json!(kids);
        }
        nodes.push(node);
    }

    // mesh: a box per bone with a first child, then the probe triangle, then the nose
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut joints: Vec<[u16; 4]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    for i in 0..n {
        let Some(child) = (0..n).find(|c| kept[*c].1.as_deref() == Some(kept[i].0.as_str())) else { continue };
        let (p, q) = (world[i], world[child]);
        let half = 0.03;
        let first = positions.len() as u32;
        for corner in 0..8u32 {
            let c = |axis: usize| {
                let (lo, hi) = (p[axis].min(q[axis]) - half, p[axis].max(q[axis]) + half);
                if corner >> axis & 1 == 1 { hi } else { lo }
            };
            let v = [c(0), c(1), c(2)];
            let mid = [(p[0] + q[0]) / 2.0, (p[1] + q[1]) / 2.0, (p[2] + q[2]) / 2.0];
            let d = [v[0] - mid[0], v[1] - mid[1], v[2] - mid[2]];
            let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-9);
            positions.push(v.map(|x| x as f32));
            normals.push([(d[0] / l) as f32, (d[1] / l) as f32, (d[2] / l) as f32]);
            joints.push([i as u16, 0, 0, 0]);
        }
        for quad in [[0, 1, 3, 2], [4, 6, 7, 5], [0, 4, 5, 1], [2, 3, 7, 6], [0, 2, 6, 4], [1, 5, 7, 3]] {
            for t in [[0, 1, 2], [0, 2, 3]] {
                indices.extend(t.iter().map(|k| first + quad[*k]));
            }
        }
    }
    // the probe: three vertices on the left forearm, at its middle and 2 cm off the axis
    let fore = kept.iter().position(|k| k.0 == "LeftForeArm").unwrap_or_else(|| index_of("LeftArm"));
    let hand = index_of("LeftHand");
    let mid = [
        (world[fore][0] + world[hand][0]) / 2.0,
        (world[fore][1] + world[hand][1]) / 2.0,
        (world[fore][2] + world[hand][2]) / 2.0,
    ];
    let axis = [world[hand][0] - world[fore][0], world[hand][1] - world[fore][1], world[hand][2] - world[fore][2]];
    // axis x Z (perpendicular to the arm, in its plane), then Z itself
    let perp = [axis[1], -axis[0], 0.0];
    let pl = (perp[0] * perp[0] + perp[1] * perp[1]).sqrt();
    let probe_first = positions.len();
    for off in [[perp[0] / pl * 0.02, perp[1] / pl * 0.02, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 0.02]] {
        positions.push([(mid[0] + off[0]) as f32, (mid[1] + off[1]) as f32, (mid[2] + off[2]) as f32]);
        normals.push([0.0, 0.0, 1.0]);
        joints.push([fore as u16, 0, 0, 0]);
    }
    let pf = probe_first as u32;
    indices.extend([pf, pf + 1, pf + 2]);
    // the nose: 12 cm in front of the head joint, on the head
    let head = index_of("Head");
    let nose = positions.len();
    for off in [[0.0, 0.0, 0.12], [0.01, 0.0, 0.12], [0.0, 0.01, 0.12]] {
        positions.push([world[head][0] as f32 + off[0], world[head][1] as f32 + off[1], world[head][2] as f32 + off[2]]);
        normals.push([0.0, 0.0, 1.0]);
        joints.push([head as u16, 0, 0, 0]);
    }
    let nf = nose as u32;
    indices.extend([nf, nf + 1, nf + 2]);
    let weights = vec![[1.0, 0.0, 0.0, 0.0]; positions.len()];

    let mut fixture = RigFixture {
        nodes,
        scene: vec![0, 1],
        meshes: vec![RigMesh { positions, normals, joints: Some(joints), weights: Some(weights), indices, ..RigMesh::default() }],
        skin_joints: (1..=n).collect(),
        ibms: world.iter().map(|w| translation_ibm(*w)).collect(),
        ..RigFixture::default()
    };
    // the first bone (Hips) is the only root; the scene lists the mesh node and the root bone
    fixture.scene = vec![0, 1];
    Humanoid { fixture, names, world, probe_first, nose }
}
