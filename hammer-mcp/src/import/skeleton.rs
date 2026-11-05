//! Bones for the skinned SMD (I5): order, names, and the glTF -> SMD frame math.
//!
//! # Math (all of it, so a live check knows what to doubt)
//!
//! A joint's world matrix `W` (glTF axes, metres; the bind pose `inverse(IBM)` or a sampled
//! animation frame, see [`super::gltf_rig`]) becomes an SMD **world frame** `F = (R, t)`:
//!
//! 1. **Rotation/scale split.** `W`'s upper 3x3 is `s * Rot` when its columns are orthogonal and
//!    equally long (`s` = their length, negative when the determinant is). `Rot` comes from
//!    Gram-Schmidt on the columns (always a proper rotation, so a reflection is not representable
//!    and shows up as a scale mismatch against the bind). `W`'s translation is `t`.
//! 2. **Basis change** `C` (a rotation, [`UpAxis::to_cs2`]: `(x,y,z) -> (z,x,y)` for Y-up files):
//!    `R = C Rot C^T`, `t = import_scale * C t`. Only translations get the import scale; rotations
//!    do not.
//! 3. **Compiler pre-turn** `T = Rz(-90 deg)` ([`super::smd::to_smd`]): `F = T * (R, t)`, applied
//!    to *every* world frame exactly as it is applied to every vertex and normal, so the
//!    compiler's Rz(+90 deg) puts both back (SPIKE-I0). Parent-relative values are unaffected by
//!    `T` (it cancels), only root bones carry it.
//! 4. **SMD local** = `inverse(F_parent) * F_j`: `R_l = R_p^T R`, `t_l = R_p^T (t - t_p)`; a root
//!    bone's local is its world frame.
//! 5. **Euler** (Source `RadianEuler`, X = roll, Y = pitch, Z = yaw): `R_l = Rz(z) Ry(y) Rx(x)`.
//!    From the matrix: `y = asin(-r20)`; `x = atan2(r21, r22)`, `z = atan2(r10, r00)`; when
//!    `cos(y) < 1e-6` (gimbal lock, ±90 deg pitch) `z = 0` and `x = atan2(-r12, r11)`. Never goes
//!    through a quaternion-to-Euler shortcut (those scramble poses near ±90 deg pitch, the I0 bug).
//!
//! SMD has no scale. Skinning with rigid frames is **exact** when each joint's uniform scale `s`
//! equals its bind scale `s_b` (the deformation `W * inverse(W_bind)` then equals the rigid
//! `F * inverse(F_bind)`; the ratio `s / s_b` is all a scale can contribute). A joint whose
//! scale is not uniform, or differs from its bind scale in any frame, loses that scale: reported
//! in `dropped`, the pose keeps rotation and translation.
use super::gltf_rig::Rig;
use super::mat::{apply3, mul3, transpose3, Mat3, Mat4};
use super::materials::sanitize;
use super::smd::{to_smd, UpAxis};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq)]
pub struct Bone {
    /// The glTF node name; `None` for the synthetic root.
    pub source: Option<String>,
    /// The name written to the SMD (lowercase, unique).
    pub name: String,
    /// Index in [`Skeleton::bones`] (always smaller than this bone's own).
    pub parent: Option<usize>,
    /// Index in [`Rig::joints`].
    pub old: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Skeleton {
    pub bones: Vec<Bone>,
    /// `new_of_old[joint]` = the bone index written to the SMD.
    pub new_of_old: Vec<usize>,
}

/// Parents-before-children order (depth first, siblings in joint order), unique lowercase names.
///
/// **Name policy.** The SMD importer lowercases bone names, so two names that differ only by case
/// would merge. Names are lowercased here (what the compiled model will show), `"` and control
/// characters become `_`, an empty name becomes `bone_<joint>`, and a name taken by an earlier
/// joint gets `_2`, `_3`, ... (the later joint in `skin.joints` order is the one renamed; the
/// synthetic root is named last). Every rename is a warning; `source` keeps the original.
pub fn build(rig: &Rig, warnings: &mut Vec<String>) -> Skeleton {
    let n = rig.joints.len();
    let mut names: Vec<String> = vec![String::new(); n];
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut collisions: Vec<String> = Vec::new();
    for pass in [false, true] {
        for (i, j) in rig.joints.iter().enumerate().filter(|(_, j)| j.synthetic == pass) {
            let cleaned: String = j.name.chars().map(|c| if c == '"' || c.is_control() { '_' } else { c }).collect();
            let cleaned = cleaned.trim();
            let base = if cleaned.is_empty() { format!("bone_{i}") } else { cleaned.to_lowercase() };
            let mut name = base.clone();
            let mut k = 2;
            while !used.insert(name.clone()) {
                name = format!("{base}_{k}");
                k += 1;
            }
            if name != base {
                collisions.push(format!("{:?} -> {name:?}", j.name));
            }
            names[i] = name;
        }
    }
    if !collisions.is_empty() {
        warnings.push(format!(
            "bone names that collide after lowercasing were renamed (later joint gets _2, _3, ...): {}",
            collisions.join(", ")
        ));
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots = Vec::new();
    for (i, j) in rig.joints.iter().enumerate() {
        match j.parent {
            Some(p) => children[p].push(i),
            None => roots.push(i),
        }
    }
    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut stack: Vec<usize> = roots.into_iter().rev().collect();
    while let Some(j) = stack.pop() {
        order.push(j);
        stack.extend(children[j].iter().rev());
    }
    let mut new_of_old = vec![0usize; n];
    for (new, old) in order.iter().enumerate() {
        new_of_old[*old] = new;
    }
    let bones = order
        .iter()
        .map(|&old| {
            let j = &rig.joints[old];
            Bone {
                source: (!j.synthetic).then(|| j.name.clone()),
                name: names[old].clone(),
                parent: j.parent.map(|p| new_of_old[p]),
                old,
            }
        })
        .collect();
    Skeleton { bones, new_of_old }
}

/// The glTF -> SMD basis: axes, import scale (module docs, steps 2-3).
#[derive(Debug, Clone, Copy)]
pub struct Basis {
    c: Mat3,
    scale: f64,
}

impl Basis {
    pub fn new(up: UpAxis, scale: f64) -> Basis {
        let cols = [up.to_cs2([1.0, 0.0, 0.0]), up.to_cs2([0.0, 1.0, 0.0]), up.to_cs2([0.0, 0.0, 1.0])];
        let mut c = [[0.0; 3]; 3];
        for (j, col) in cols.iter().enumerate() {
            for (i, v) in col.iter().enumerate() {
                c[i][j] = *v;
            }
        }
        Basis { c, scale }
    }
}

/// A rigid frame: rotation `r[row][col]` and translation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub r: Mat3,
    pub t: [f64; 3],
}

/// The SMD world frame of glTF matrix `m` (module docs, steps 1-3) and the uniform scale it
/// carried, `None` when its 3x3 is not a uniform scale times a rotation (non-uniform or skewed).
/// The scale is signed: negative when the 3x3 is a reflection (determinant < 0), which the
/// rotation (always proper) cannot express, so a bind/pose sign mismatch fails the ratio test.
pub fn frame_of(m: &Mat4, basis: &Basis) -> (Frame, Option<f64>) {
    let col = |c: usize| [m[c][0], m[c][1], m[c][2]];
    let len = |v: [f64; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let (a0, a1, a2) = (col(0), col(1), col(2));
    let lens = [len(a0), len(a1), len(a2)];
    let s = (lens[0] + lens[1] + lens[2]) / 3.0;
    let tol = 1e-3 * s.max(1e-12);
    let uniform = s > 1e-12
        && lens.iter().all(|l| (l - s).abs() <= tol)
        && dot(a0, a1).abs() <= tol * s
        && dot(a0, a2).abs() <= tol * s
        && dot(a1, a2).abs() <= tol * s;
    let norm = |v: [f64; 3]| {
        let l = len(v);
        if l < 1e-12 { [1.0, 0.0, 0.0] } else { [v[0] / l, v[1] / l, v[2] / l] }
    };
    let e0 = norm(a0);
    let d = dot(a1, e0);
    let e1 = norm([a1[0] - d * e0[0], a1[1] - d * e0[1], a1[2] - d * e0[2]]);
    let e2 = [e0[1] * e1[2] - e0[2] * e1[1], e0[2] * e1[0] - e0[0] * e1[2], e0[0] * e1[1] - e0[1] * e1[0]];
    let rot: Mat3 = [[e0[0], e1[0], e2[0]], [e0[1], e1[1], e2[1]], [e0[2], e1[2], e2[2]]];
    let r = mul3(&mul3(&basis.c, &rot), &transpose3(&basis.c));
    let t = apply3(&basis.c, [m[3][0], m[3][1], m[3][2]]).map(|v| v * basis.scale);
    // pre-turn T = Rz(-90): to_smd on every column of r and on t
    let mut turned = [[0.0; 3]; 3];
    for j in 0..3 {
        let c = to_smd([r[0][j], r[1][j], r[2][j]]);
        for i in 0..3 {
            turned[i][j] = c[i];
        }
    }
    let cross = [a1[1] * a2[2] - a1[2] * a2[1], a1[2] * a2[0] - a1[0] * a2[2], a1[0] * a2[1] - a1[1] * a2[0]];
    let signed = if dot(a0, cross) < 0.0 { -s } else { s };
    (Frame { r: turned, t: to_smd(t) }, uniform.then_some(signed))
}

/// Source `RadianEuler` `[x, y, z]` of rotation `r` (module docs, step 5).
pub fn euler_of(r: &Mat3) -> [f64; 3] {
    let y = (-r[2][0]).clamp(-1.0, 1.0).asin();
    if y.cos() > 1e-6 {
        [r[2][1].atan2(r[2][2]), y, r[1][0].atan2(r[0][0])]
    } else {
        [(-r[1][2]).atan2(r[1][1]), y, 0.0]
    }
}

/// The matrix `Rz(z) Ry(y) Rx(x)` of a `RadianEuler` (the inverse of [`euler_of`]; tests use it
/// to rebuild poses from the written numbers).
pub fn matrix_of_euler(e: [f64; 3]) -> Mat3 {
    let (sx, cx) = e[0].sin_cos();
    let (sy, cy) = e[1].sin_cos();
    let (sz, cz) = e[2].sin_cos();
    [
        [cz * cy, cz * sy * sx - sz * cx, cz * sy * cx + sz * sx],
        [sz * cy, sz * sy * sx + cz * cx, sz * sy * cx - cz * sx],
        [-sy, cy * sx, cy * cx],
    ]
}

/// Per-bone `(translation, euler)` locals from world frames given in bone order.
pub fn locals(sk: &Skeleton, worlds: &[Frame]) -> Vec<([f64; 3], [f64; 3])> {
    sk.bones
        .iter()
        .zip(worlds)
        .map(|(bone, f)| match bone.parent {
            None => (f.t, euler_of(&f.r)),
            Some(p) => {
                let parent = &worlds[p];
                let pt = transpose3(&parent.r);
                let d = [f.t[0] - parent.t[0], f.t[1] - parent.t[1], f.t[2] - parent.t[2]];
                (apply3(&pt, d), euler_of(&mul3(&pt, &f.r)))
            }
        })
        .collect()
}

/// `skeleton` section lines for one frame (`time N` is written by the caller).
pub fn pose_lines(sk: &Skeleton, worlds: &[Frame]) -> String {
    let mut out = String::new();
    for (i, (t, e)) in locals(sk, worlds).iter().enumerate() {
        out.push_str(&format!("{i} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6}\n", t[0], t[1], t[2], e[0], e[1], e[2]));
    }
    out
}

/// World frames of one pose (`globals[joint]` in glTF space), in bone order. A joint whose
/// uniform scale differs from `bind_scale` (or is not uniform) is added to `bad`.
pub fn worlds(
    sk: &Skeleton,
    globals: &[Mat4],
    basis: &Basis,
    bind_scale: &[Option<f64>],
    bad: &mut BTreeSet<usize>,
) -> Vec<Frame> {
    sk.bones
        .iter()
        .map(|bone| {
            let (frame, s) = frame_of(&globals[bone.old], basis);
            let same = matches!((s, bind_scale[bone.old]), (Some(a), Some(b)) if (a / b - 1.0).abs() <= 1e-3);
            if !same {
                bad.insert(bone.old);
            }
            frame
        })
        .collect()
}

/// A unique sequence name per clip: the sanitised animation name (`anim_<n>` when it has none),
/// `_2`, `_3`, ... on a repeat, and never one of `reserved`.
pub fn sequence_names(clips: &[super::gltf_rig::Clip], reserved: &[&str]) -> Vec<String> {
    let mut used: BTreeSet<String> = reserved.iter().map(|s| s.to_string()).collect();
    clips
        .iter()
        .map(|clip| {
            let named = clip
                .name
                .as_deref()
                .filter(|n| n.chars().any(|c| c.is_ascii_alphanumeric()))
                .map(sanitize)
                .unwrap_or_else(|| format!("anim_{}", clip.index));
            let mut name = named.clone();
            let mut k = 2;
            while !used.insert(name.clone()) {
                name = format!("{named}_{k}");
                k += 1;
            }
            name
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rx(a: f64) -> Mat3 {
        let (s, c) = a.sin_cos();
        [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]]
    }
    fn ry(a: f64) -> Mat3 {
        let (s, c) = a.sin_cos();
        [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
    }
    fn rz(a: f64) -> Mat3 {
        let (s, c) = a.sin_cos();
        [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
    }

    #[test]
    fn euler_round_trips_everywhere_including_gimbal_lock() {
        let mut worst: f64 = 0.0;
        let degs = [-180.0, -135.0, -90.0, -89.9999, -45.0, 0.0, 30.0, 89.0, 89.99999, 90.0, 90.0001, 135.0, 179.0];
        for &x in &degs {
            for &y in &degs {
                for &z in &degs {
                    let r = mul3(&rz(f64::to_radians(z)), &mul3(&ry(f64::to_radians(y)), &rx(f64::to_radians(x))));
                    let back = matrix_of_euler(euler_of(&r));
                    for i in 0..3 {
                        for j in 0..3 {
                            worst = worst.max((back[i][j] - r[i][j]).abs());
                        }
                    }
                }
            }
        }
        // the lock branch drops z, so the error is at most cos(pitch) < 1e-6 (the SMD text has 6 decimals)
        assert!(worst < 1e-6, "worst {worst}");
    }

    #[test]
    fn matrix_of_euler_is_rz_ry_rx() {
        let e = [0.3, -0.7, 1.1];
        let want = mul3(&rz(e[2]), &mul3(&ry(e[1]), &rx(e[0])));
        let got = matrix_of_euler(e);
        for i in 0..3 {
            for j in 0..3 {
                assert!((want[i][j] - got[i][j]).abs() < 1e-12);
            }
        }
    }
}
