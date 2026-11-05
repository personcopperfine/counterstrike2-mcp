//! Source SMD writer (reference mesh; the same file doubles as the physics source).
//!
//! One root bone `root`, every vertex rigid on it (`0 x y z nx ny nz u v`, the form the I0 spike
//! compiled live), one triangle = a material line + three vertex lines. A skinned file goes
//! through [`write_rigged`] instead (real skeleton, per-vertex links, one animation SMD per clip).
//!
//! # Conventions
//!
//! * **Axes.** glTF is right-handed, +Y up, an asset's front faces +Z. Source 2 is right-handed,
//!   +Z up, front is +X (+Y is left). [`UpAxis::Y`] maps `(x, y, z)_gltf -> (z, x, y)_cs2`: a
//!   cyclic permutation (a pure rotation, determinant +1: no mirroring, winding stays
//!   counter-clockwise), so glTF +Y (up) -> CS2 +Z (up), glTF +Z (front) -> CS2 +X (front) and
//!   glTF +X (the model's left when seen from the front) -> CS2 +Y (left). [`UpAxis::Z`] is the
//!   identity.
//! * **Compiler turn.** The model compiler turns SMD content by Rz(+90°) (`(x, y) -> (-y, x)`;
//!   live 2026-10-29: an import whose front was written at +X came out facing +Y, and SPIKE-I0's
//!   SMD roots got `[0,0,.707,.707]`), so [`to_smd`] writes every position and normal turned by
//!   Rz(−90°) first and the compiled model faces +X (yaw 0) as the axes above say.
//! * **Scale.** Positions are multiplied by `scale` (glTF metres -> 39.37 inch units by default);
//!   normals are rotated only.
//! * **V flip.** glTF's V runs down from the top, Source's up from the bottom: `v = 1 - v`.
use super::gltf_read::Scene;
use super::skeleton::{self, Basis, Skeleton};
use std::collections::BTreeSet;
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpAxis {
    Y,
    Z,
}

impl UpAxis {
    /// A glTF-space vector in CS2 axes (see the module docs).
    pub fn to_cs2(self, v: [f64; 3]) -> [f64; 3] {
        match self {
            UpAxis::Y => [v[2], v[0], v[1]],
            UpAxis::Z => v,
        }
    }
}

/// A CS2-axes vector as written into the SMD: turned by Rz(−90°) so the compiler's Rz(+90°)
/// (module docs) lands it back where [`UpAxis::to_cs2`] put it.
pub fn to_smd(v: [f64; 3]) -> [f64; 3] {
    [v[1], -v[0], v[2]]
}

#[derive(Debug, Clone, PartialEq)]
pub struct SmdStats {
    pub tris: usize,
    pub verts: usize,
}

/// `names[i]` is the SMD material name of `scene.materials[i]`.
pub fn write_reference(scene: &Scene, names: &[String], scale: f64, up: UpAxis) -> (String, SmdStats) {
    let mut out = String::new();
    out.push_str("version 1\nnodes\n0 \"root\" -1\nend\nskeleton\ntime 0\n0 0 0 0 0 0 0\nend\ntriangles\n");
    for mesh in &scene.meshes {
        let material = &names[mesh.material];
        for tri in mesh.indices.as_chunks::<3>().0 {
            out.push_str(material);
            out.push('\n');
            for &i in tri {
                let i = i as usize;
                let p = to_smd(up.to_cs2(mesh.positions[i].map(|v| v as f64)));
                let n = to_smd(up.to_cs2(mesh.normals[i].map(|v| v as f64)));
                let uv = mesh.uv0[i];
                let _ = writeln!(
                    out,
                    "0 {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6}",
                    p[0] * scale,
                    p[1] * scale,
                    p[2] * scale,
                    n[0],
                    n[1],
                    n[2],
                    uv[0],
                    1.0 - uv[1] as f64
                );
            }
        }
    }
    out.push_str("end\n");
    (out, SmdStats { tris: scene.tri_count(), verts: scene.vert_count() })
}

/// One animation as its own SMD.
#[derive(Debug, Clone, PartialEq)]
pub struct AnimSmd {
    /// The sequence name (unique, file-name safe); the file is `<name>.smd`.
    pub name: String,
    pub frames: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiggedSmd {
    pub reference: String,
    pub stats: SmdStats,
    pub skeleton: Skeleton,
    pub anims: Vec<AnimSmd>,
    pub warnings: Vec<String>,
    pub dropped: Vec<String>,
}

/// The skinned counterpart of [`write_reference`]: the reference SMD with the real skeleton
/// (`nodes`, the bind pose as `skeleton time 0`, every vertex with its bone links) and one
/// animation SMD per clip (`nodes` identical to the reference, `time 0..frames-1` at 30 fps).
/// The math is in [`super::skeleton`]; vertices and normals get the same axes/scale/pre-turn as
/// [`write_reference`] gives static ones. `reserved` sequence names are skipped (the model
/// stem).
pub fn write_rigged(scene: &Scene, names: &[String], scale: f64, up: UpAxis, reserved: &[&str]) -> RiggedSmd {
    let rig = scene.rig.as_ref().expect("write_rigged needs a rig");
    let mut warnings = Vec::new();
    let mut dropped = Vec::new();
    let sk = skeleton::build(rig, &mut warnings);
    let basis = Basis::new(up, scale);

    let mut nodes = String::from("version 1\nnodes\n");
    for (i, bone) in sk.bones.iter().enumerate() {
        let _ = writeln!(nodes, "{i} \"{}\" {}", bone.name, bone.parent.map_or(-1, |p| p as i64));
    }
    nodes.push_str("end\nskeleton\n");

    // bind pose
    let mut bad: BTreeSet<usize> = BTreeSet::new();
    let bind_globals: Vec<_> = rig.joints.iter().map(|j| j.bind).collect();
    let bind_scale: Vec<Option<f64>> = bind_globals.iter().map(|m| skeleton::frame_of(m, &basis).1).collect();
    for (j, s) in bind_scale.iter().enumerate() {
        if s.is_none() {
            bad.insert(j);
        }
    }
    let bind_worlds = skeleton::worlds(&sk, &bind_globals, &basis, &bind_scale, &mut bad);
    let mut reference = nodes.clone();
    reference.push_str("time 0\n");
    reference.push_str(&skeleton::pose_lines(&sk, &bind_worlds));
    reference.push_str("end\ntriangles\n");

    let mut tris = 0;
    for mesh in &scene.meshes {
        let material = &names[mesh.material];
        for tri in mesh.indices.as_chunks::<3>().0 {
            tris += 1;
            reference.push_str(material);
            reference.push('\n');
            for &i in tri {
                let i = i as usize;
                let p = to_smd(up.to_cs2(mesh.positions[i].map(|v| v as f64)));
                let n = to_smd(up.to_cs2(mesh.normals[i].map(|v| v as f64)));
                let uv = mesh.uv0[i];
                let links = &mesh.links[i];
                // a vertex with no positive link (cannot come from the reader) rides the root bone
                let mut used: Vec<(usize, f32)> =
                    links.iter().filter(|l| l.1 > 0.0).map(|l| (sk.new_of_old[l.0 as usize], l.1)).collect();
                if used.is_empty() {
                    used.push((0, 1.0));
                }
                let primary = used[0].0;
                let _ = write!(
                    reference,
                    "{primary} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {}",
                    p[0] * scale,
                    p[1] * scale,
                    p[2] * scale,
                    n[0],
                    n[1],
                    n[2],
                    uv[0],
                    1.0 - uv[1] as f64,
                    used.len()
                );
                for (bone, weight) in used {
                    let _ = write!(reference, " {bone} {weight:.6}");
                }
                reference.push('\n');
            }
        }
    }
    reference.push_str("end\n");

    // animations
    let seq = skeleton::sequence_names(&rig.clips, reserved);
    let mut anims = Vec::new();
    for (clip, name) in rig.clips.iter().zip(&seq) {
        let mut text = nodes.clone();
        for (k, globals) in clip.globals.iter().enumerate() {
            let worlds = skeleton::worlds(&sk, globals, &basis, &bind_scale, &mut bad);
            let _ = writeln!(text, "time {k}");
            text.push_str(&skeleton::pose_lines(&sk, &worlds));
        }
        text.push_str("end\n");
        anims.push(AnimSmd { name: name.clone(), frames: clip.frames, text });
    }
    if !bad.is_empty() {
        let list: Vec<&str> = bad.iter().take(8).map(|j| sk.bones[sk.new_of_old[*j]].name.as_str()).collect();
        dropped.push(format!(
            "joint scale dropped on {} bone(s) (SMD has no scale; non-uniform, or different from the bind scale in some frame; rotation and translation are kept): {}{}",
            bad.len(),
            list.join(", "),
            if bad.len() > 8 { ", ..." } else { "" }
        ));
    }
    RiggedSmd {
        reference,
        stats: SmdStats { tris, verts: scene.vert_count() },
        skeleton: sk,
        anims,
        warnings,
        dropped,
    }
}
