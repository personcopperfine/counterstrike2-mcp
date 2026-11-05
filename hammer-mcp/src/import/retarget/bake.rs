//! Rest-pose baking (I6): the glTF mesh re-posed onto the CS2 player skeleton's rest pose.
//!
//! # Math
//!
//! Everything is computed in **vnmskel model space** (inches, +Z up) with the source in its own
//! bind pose. Let `P_s` be a source joint's bind position (`inverse(IBM)` translation, glTF space)
//! and `m(c)` the model-space reference pose of CS2 bone `c` (from the file, not recomposed).
//!
//! 1. **Axes.** For the CS2 skeleton: `up_c = head_0 - pelvis`, `left_raw = clavicle_L -
//!    clavicle_R` (`_L` is the character's left), `left_c = normalise(left_raw - (left_raw.up_c)
//!    up_c)`, `forward_c = left_c x up_c` (right-handed: with left +Y and up +Z that is +X). The
//!    same recipe on the *mapped source joints* gives `(up_s, left_s, forward_s)`, so a rig that
//!    faces anywhere is oriented by its own data, not by a convention. (For a glTF with +Y up and
//!    +Z front, left is +X and the derived forward is +Z, the glTF front; a test checks it.)
//!    A source whose clavicles coincide falls back to `arm_upper_L - arm_upper_R`.
//! 2. **Conform.** `R = [left_c up_c forward_c] [left_s up_s forward_s]^T` (a proper rotation, no
//!    mirroring). Scale `k` (CS2 units per source unit): `k = |head_0 - pelvis|_c / |head - pelvis|_s`
//!    unless `scale` is given. A point `p` becomes `conf(p) = m(pelvis).t + k R (p - P_pelvis)`; a
//!    normal `R n`. Conformed joint positions: `ps_j = conf(P_src(j))`.
//! 3. **Per bone.** For a CS2 bone `j` with source joint `s`: the *successors* `T_j` are the next
//!    mapped bone(s) along its chain (torso `pelvis spine_0..3 neck_0 head_0`; arm `clavicle
//!    arm_upper arm_lower hand`; leg `leg_upper leg_lower ankle ball`; hand -> the first mapped
//!    bone of each finger; a finger chain base to tip). `d_c = normalise(mean(pc_t) - pc_j)`,
//!    `d_s = normalise(mean(ps_t) - ps_j)` (the *source* positions of the same pairs),
//!    `len_c`, `len_s` their lengths, `r = clamp(len_c / len_s, 0.1, 10)`; a source length under
//!    `max(0.05 len_c, 0.002 |head_0 - pelvis|_c)` counts as coincident joints, not a direction. The rotation `Q_j`
//!    takes `d_s` onto `d_c` **and** resolves the roll: both directions get a secondary axis from
//!    the same global reference (the first of `forward_c, left_c, up_c` that is under 0.7 along
//!    `d_c`), `Q_j = F_c F_s^T` with `F = [d, a, d x a]`; a degenerate source frame falls back to
//!    the minimal arc. A bone with no mapped successor (head, finger tips, ball, a hand or foot
//!    whose next bones are not mapped) has no length to measure: its CS2 direction is its own rest
//!    frame's +X axis (verified on the file: every vnmskel bone points along its frame's +X, the
//!    right side mirrored so -X; the sign is taken to agree with the chain), its source direction
//!    points to the source child that lies closest to it after the predecessor's `Q`, `r = 1`.
//!    With no such child within 45 degrees (hair, jaw or prop joints) it inherits `Q` of its chain
//!    predecessor; with no predecessor (the pelvis) `Q = I`.
//! 4. **Skin.** `M_j(v) = pc_j + Q_j (v' + (r_j - 1) clamp(v'.d_s, 0, len_s) d_s)` with `v' =
//!    conf(v) - ps_j`: the joint lands on the CS2 joint and only the segment itself is stretched
//!    to the CS2 bone length; behind the joint the mesh stays rigid and past the successor it is
//!    carried along (CS2 lengths win; neighbouring bones agree on the shared joint). Normals get
//!    the inverse transpose of the stretch inside the segment, then `Q_j R`, renormalised. Vertex `v_out = sum w_c M_c(v)`
//!    (linear blend) with the weights of each source joint moved to the CS2 bone it maps to, or
//!    that of its nearest mapped ancestor, or the pelvis when it has none.
//! 5. **SMD.** The skeleton block is the vnmskel parent-space pose verbatim (all bones, order and
//!    names of the file), Euler from the quaternion through the matrix with the gimbal-safe
//!    `Rz Ry Rx` conversion; no glTF basis change. The root bones, every vertex and every normal
//!    get the prop path's `Rz(-90)` pre-turn ([`crate::import::smd::to_smd`]) so the compiler's
//!    Rz(+90) turn puts the compiled bind back exactly on the vnmskel reference pose. Without it
//!    (I6 first live run, 2026-11-03) the compiled bind faced +Y while the vnmskel faces +X, and
//!    in game the graph-driven body came out turned a quarter against a real agent beside it.
use super::mapping::{Mapping, Pair};
use super::skeleton::{NmSkeleton, Pose};
use crate::import::gltf_read::Scene;
use crate::import::gltf_rig::Rig;
use crate::import::mat::{apply3, mul3, transpose3, Mat3};
use crate::import::skeleton::euler_of;
use crate::import::smd::to_smd;
use std::collections::HashMap;
use std::fmt::Write;

type V3 = [f64; 3];

pub fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
pub fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
pub fn mulv(a: V3, k: f64) -> V3 {
    [a[0] * k, a[1] * k, a[2] * k]
}
pub fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
pub fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
pub fn len(a: V3) -> f64 {
    dot(a, a).sqrt()
}
pub fn unit(a: V3) -> V3 {
    let l = len(a);
    if l < 1e-12 { [0.0; 3] } else { mulv(a, 1.0 / l) }
}

/// Columns `a b c` as a matrix.
fn cols(a: V3, b: V3, c: V3) -> Mat3 {
    [[a[0], b[0], c[0]], [a[1], b[1], c[1]], [a[2], b[2], c[2]]]
}

/// Rotation matrix of a unit quaternion `[x, y, z, w]`.
pub fn quat_mat(q: [f64; 4]) -> Mat3 {
    let [x, y, z, w] = q;
    [
        [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - z * w), 2.0 * (x * z + y * w)],
        [2.0 * (x * y + z * w), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - x * w)],
        [2.0 * (x * z - y * w), 2.0 * (y * z + x * w), 1.0 - 2.0 * (x * x + y * y)],
    ]
}

/// Character frame: orthonormal, right-handed (`left x up = forward`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Axes {
    pub up: V3,
    pub left: V3,
    pub forward: V3,
}

fn axes_of(up_raw: V3, left_raw: V3) -> Option<Axes> {
    let up = unit(up_raw);
    let left = unit(sub(left_raw, mulv(up, dot(left_raw, up))));
    if len(up) < 0.5 || len(left) < 0.5 {
        return None;
    }
    Some(Axes { up, left, forward: cross(left, up) })
}

/// One bone's transform (module docs, step 3-4).
#[derive(Debug, Clone, Copy)]
pub struct BoneXf {
    /// Conformed source joint position.
    pub from: V3,
    /// CS2 joint position (model space).
    pub to: V3,
    pub q: Mat3,
    /// Source bone axis, the conformed source segment length along it and the stretch of that
    /// segment.
    pub axis: V3,
    pub seg: f64,
    pub ratio: f64,
}

impl BoneXf {
    /// Only the segment `0..seg` along the axis stretches; behind the joint stays rigid and past
    /// the successor is carried along, so neighbouring bones agree at the shared joint.
    pub fn apply(&self, conformed: V3) -> V3 {
        let v = sub(conformed, self.from);
        let t = dot(v, self.axis).clamp(0.0, self.seg);
        let v = add(v, mulv(self.axis, (self.ratio - 1.0) * t));
        add(self.to, apply3(&self.q, v))
    }

    /// A conformed normal at `conformed`: inside the stretched segment the inverse transpose of
    /// the stretch, then the rotation (not renormalised).
    pub fn normal(&self, conformed: V3, n: V3) -> V3 {
        let t = dot(sub(conformed, self.from), self.axis);
        let n = if self.ratio != 1.0 && t > 0.0 && t < self.seg {
            sub(n, mulv(self.axis, (1.0 - 1.0 / self.ratio) * dot(n, self.axis)))
        } else {
            n
        };
        apply3(&self.q, unit(n))
    }
}

#[derive(Debug, Clone)]
pub struct Baked {
    pub scale: f64,
    pub scale_from_arg: bool,
    pub cs2_axes: Axes,
    pub src_axes: Axes,
    /// The conform rotation.
    pub rotation: Mat3,
    /// Per CS2 bone: its transform (mapped bones only).
    pub xf: Vec<Option<BoneXf>>,
    /// Per source joint: the CS2 bone its weights go to.
    pub assign: Vec<usize>,
    pub warnings: Vec<String>,
    conform: Conform,
}

/// `conf(p) = origin_cs2 + scale * rotation * (p - origin_src)` (module docs, step 2).
#[derive(Debug, Clone, Copy)]
struct Conform {
    rotation: Mat3,
    scale: f64,
    origin_src: V3,
    origin_cs2: V3,
}

impl Conform {
    fn apply(&self, p: V3) -> V3 {
        add(self.origin_cs2, mulv(apply3(&self.rotation, sub(p, self.origin_src)), self.scale))
    }
}

impl Baked {
    pub fn conform(&self, p: V3) -> V3 {
        self.conform.apply(p)
    }
}

pub(super) fn chain_names() -> Vec<Vec<String>> {
    let mut chains: Vec<Vec<String>> = vec![["pelvis", "spine_0", "spine_1", "spine_2", "spine_3", "neck_0", "head_0"].iter().map(|s| s.to_string()).collect()];
    for s in ["L", "R"] {
        chains.push(["clavicle", "arm_upper", "arm_lower", "hand"].iter().map(|b| format!("{b}_{s}")).collect());
        chains.push(["leg_upper", "leg_lower", "ankle", "ball"].iter().map(|b| format!("{b}_{s}")).collect());
        for f in ["thumb", "index", "middle", "ring", "pinky"] {
            let mut chain = Vec::new();
            if f != "thumb" {
                chain.push(format!("finger_{f}_meta_{s}"));
            }
            chain.extend((0..3).map(|i| format!("finger_{f}_{i}_{s}")));
            chains.push(chain);
        }
    }
    chains
}

/// Everything the baking needs about the rig, resolved once.
pub fn bake(rig: &Rig, skel: &NmSkeleton, mapping: &Mapping, scale_arg: Option<f64>) -> Result<Baked, String> {
    let n = skel.names.len();
    let mut src_of: Vec<Option<usize>> = vec![None; n];
    for Pair { src, cs2, .. } in &mapping.pairs {
        src_of[*cs2] = Some(*src);
    }
    let named = |name: &str| -> Result<usize, String> {
        let c = skel.index(name).ok_or_else(|| format!("the CS2 skeleton has no bone {name}"))?;
        src_of[c].map(|_| c).ok_or_else(|| format!("{name} is not mapped"))
    };
    let pos_c = |c: usize| skel.model[c].t;
    let pos_s = |c: usize| {
        let m = &rig.joints[src_of[c].unwrap()].bind;
        [m[3][0], m[3][1], m[3][2]]
    };
    let (pelvis, head) = (named("pelvis")?, named("head_0")?);
    let lateral = |a: &str, b: &str| -> Result<(V3, V3), String> {
        let (a, b) = (named(a)?, named(b)?);
        Ok((sub(pos_c(a), pos_c(b)), sub(pos_s(a), pos_s(b))))
    };
    let up_s = sub(pos_s(head), pos_s(pelvis));
    let (mut left_c, mut left_s) = lateral("clavicle_L", "clavicle_R")?;
    if len(left_c) < 1e-6 || len(left_s) < 0.01 * len(up_s) {
        (left_c, left_s) = lateral("arm_upper_L", "arm_upper_R")?;
    }
    let cs2_axes = axes_of(sub(pos_c(head), pos_c(pelvis)), left_c).ok_or("the CS2 skeleton has no usable up/left axes")?;
    let src_axes = axes_of(up_s, left_s).ok_or_else(|| {
        "the source rig has no usable up/left axes: head and pelvis (or the left and right clavicles) coincide".to_string()
    })?;
    let rotation = mul3(
        &cols(cs2_axes.left, cs2_axes.up, cs2_axes.forward),
        &transpose3(&cols(src_axes.left, src_axes.up, src_axes.forward)),
    );
    let fit = len(sub(pos_c(head), pos_c(pelvis))) / len(up_s);
    let scale = scale_arg.unwrap_or(fit);
    let mut baked = Baked {
        scale,
        scale_from_arg: scale_arg.is_some(),
        cs2_axes,
        src_axes,
        rotation,
        xf: vec![None; n],
        assign: vec![],
        warnings: vec![],
        conform: Conform { rotation, scale, origin_src: pos_s(pelvis), origin_cs2: pos_c(pelvis) },
    };
    let conf = baked.conform;
    let ps = |c: usize| conf.apply(pos_s(c));
    // a conformed source length under this is a coincident joint, not a direction
    let tiny = 0.002 * len(sub(pos_c(head), pos_c(pelvis)));

    // chain successors / predecessors among the mapped bones
    let mut succ: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut pred: HashMap<usize, usize> = HashMap::new();
    let mapped_in = |chain: &[String], from: usize| -> Option<usize> {
        chain[from..].iter().filter_map(|nm| skel.index(nm)).find(|c| src_of[*c].is_some())
    };
    for chain in chain_names() {
        let mut prev: Option<usize> = None;
        for name in chain.iter() {
            let Some(c) = skel.index(name).filter(|c| src_of[*c].is_some()) else { continue };
            if let Some(p) = prev {
                succ.entry(p).or_default().push(c);
                pred.insert(c, p);
            }
            prev = Some(c);
        }
    }
    // a hand's successors are the first mapped bone of each finger, and those fingers' predecessor is the hand
    for s in ["L", "R"] {
        let Some(hand) = skel.index(&format!("hand_{s}")).filter(|c| src_of[*c].is_some()) else { continue };
        for chain in chain_names().iter().filter(|c| c[0].starts_with("finger_") && c[0].ends_with(&format!("_{s}"))) {
            if let Some(first) = mapped_in(chain, 0) {
                succ.entry(hand).or_default().push(first);
                pred.entry(first).or_insert(hand);
            }
        }
    }

    let mut warned_ratio: Vec<String> = Vec::new();
    let mut warned_roll: Vec<String> = Vec::new();
    #[allow(clippy::needless_range_loop)] // `c` indexes several parallel per-bone tables
    for c in 0..n {
        let Some(_) = src_of[c] else { continue };
        let (from, to) = (ps(c), pos_c(c));
        let targets = succ.get(&c).cloned().unwrap_or_default();
        let mut own: Option<(Mat3, V3, f64, f64)> = None;
        if !targets.is_empty() {
            let k = targets.len() as f64;
            let mean = |f: &dyn Fn(usize) -> V3| mulv(targets.iter().fold([0.0; 3], |a, t| add(a, f(*t))), 1.0 / k);
            let (vc, vs) = (sub(mean(&pos_c), to), sub(mean(&|t| ps(t)), from));
            if len(vc) > 1e-9 && len(vs) > tiny.max(0.05 * len(vc)) {
                let (dc, ds) = (unit(vc), unit(vs));
                let raw = len(vc) / len(vs);
                let ratio = raw.clamp(0.1, 10.0);
                if ratio != raw {
                    warned_ratio.push(skel.names[c].clone());
                }
                let (q, ok) = roll_rotation(ds, dc, &cs2_axes);
                if !ok {
                    warned_roll.push(skel.names[c].clone());
                }
                own = Some((q, ds, len(vs), ratio));
            }
        }
        // a terminal (or a bone whose successor coincides with it): the rest frame's +X axis
        // against the source child that already points closest to it once the predecessor's
        // rotation is applied; hair, jaw or prop joints that point elsewhere (over 45 degrees
        // off) leave the predecessor's rotation in place. Without a predecessor the conform
        // alone is kept.
        let pred_xf = pred.get(&c).and_then(|p| baked.xf[*p]);
        if let (None, Some(px)) = (own, pred_xf) {
            let s_joint = src_of[c].unwrap();
            let m = quat_mat(skel.model[c].q);
            let mut dc = [m[0][0], m[1][0], m[2][0]];
            if dot(dc, sub(to, px.to)) < 0.0 {
                dc = mulv(dc, -1.0);
            }
            let child = (0..rig.joints.len())
                .filter(|k| rig.joints[*k].parent == Some(s_joint) && !rig.joints[*k].synthetic)
                .map(|k| {
                    let m = &rig.joints[k].bind;
                    sub(conf.apply([m[3][0], m[3][1], m[3][2]]), from)
                })
                .filter(|v| len(*v) > tiny)
                .map(|v| (dot(unit(apply3(&px.q, unit(v))), dc), v))
                .max_by(|a, b| a.0.total_cmp(&b.0));
            if let Some((_, vs)) = child.filter(|(cos, _)| *cos > std::f64::consts::FRAC_1_SQRT_2) {
                let ds = unit(vs);
                let (q, ok) = roll_rotation(ds, dc, &cs2_axes);
                if !ok {
                    warned_roll.push(skel.names[c].clone());
                }
                own = Some((q, ds, 0.0, 1.0));
            }
        }
        let (q, axis, seg, ratio) = match own {
            Some(v) => v,
            None => match pred_xf {
                Some(p) => (p.q, p.axis, 0.0, 1.0),
                None => (rotation_id(), [1.0, 0.0, 0.0], 0.0, 1.0),
            },
        };
        baked.xf[c] = Some(BoneXf { from, to, q, axis, seg, ratio });
    }
    if !warned_ratio.is_empty() {
        baked.warnings.push(format!(
            "bone length ratio clamped to 0.1..10 for {} (the source proportions are far from the CS2 skeleton's)",
            warned_ratio.join(", ")
        ));
    }
    if !warned_roll.is_empty() {
        baked.warnings.push(format!("roll not resolved (direction parallel to the reference axis) for {}: minimal arc used", warned_roll.join(", ")));
    }

    // weight targets: mapped -> own CS2 bone, else nearest mapped ancestor, else the pelvis
    let cs2_of_src: HashMap<usize, usize> = mapping.pairs.iter().map(|p| (p.src, p.cs2)).collect();
    baked.assign = (0..rig.joints.len())
        .map(|j| {
            let mut cur = Some(j);
            let mut guard = 0;
            while let Some(k) = cur {
                if let Some(c) = cs2_of_src.get(&k) {
                    return *c;
                }
                cur = rig.joints[k].parent;
                guard += 1;
                if guard > rig.joints.len() {
                    break;
                }
            }
            pelvis
        })
        .collect();
    Ok(baked)
}

/// The rotation taking source direction `ds` onto CS2 direction `dc` with the roll resolved by a
/// shared global reference axis (module docs, step 3); `false` when it had to fall back to the
/// minimal arc.
fn roll_rotation(ds: V3, dc: V3, axes: &Axes) -> (Mat3, bool) {
    let reference = [axes.forward, axes.left, axes.up].into_iter().find(|r| dot(*r, dc).abs() < 0.7).unwrap_or(axes.forward);
    let sec = |d: V3| unit(sub(reference, mulv(d, dot(reference, d))));
    let (ac, as_) = (sec(dc), sec(ds));
    if len(ac) > 0.5 && len(as_) > 0.5 {
        (mul3(&cols(dc, ac, cross(dc, ac)), &transpose3(&cols(ds, as_, cross(ds, as_)))), true)
    } else {
        (arc(ds, dc), false)
    }
}

fn rotation_id() -> Mat3 {
    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
}

/// Minimal-arc rotation taking unit `a` onto unit `b`.
fn arc(a: V3, b: V3) -> Mat3 {
    let c = dot(a, b);
    let v = cross(a, b);
    let s2 = dot(v, v);
    let id = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    if s2 < 1e-18 {
        if c > 0.0 {
            return id;
        }
        // opposite: half turn about any axis perpendicular to a
        let p = if a[0].abs() < 0.9 { cross(a, [1.0, 0.0, 0.0]) } else { cross(a, [0.0, 1.0, 0.0]) };
        let p = unit(p);
        return [
            [2.0 * p[0] * p[0] - 1.0, 2.0 * p[0] * p[1], 2.0 * p[0] * p[2]],
            [2.0 * p[1] * p[0], 2.0 * p[1] * p[1] - 1.0, 2.0 * p[1] * p[2]],
            [2.0 * p[2] * p[0], 2.0 * p[2] * p[1], 2.0 * p[2] * p[2] - 1.0],
        ];
    }
    let k = (1.0 - c) / s2;
    let vx = [[0.0, -v[2], v[1]], [v[2], 0.0, -v[0]], [-v[1], v[0], 0.0]];
    let vx2 = mul3(&vx, &vx);
    let mut out = id;
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] += vx[i][j] + vx2[i][j] * k;
        }
    }
    out
}

/// One output vertex: position, normal and `(CS2 bone, weight)` links (at most four, by weight).
pub struct OutVertex {
    pub p: V3,
    pub n: V3,
    pub links: Vec<(usize, f64)>,
}

/// Re-pose one source vertex (module docs, step 4).
pub fn skin_vertex(b: &Baked, pos: V3, normal: V3, links: &[(u16, f32)], fallback: usize) -> OutVertex {
    let mut merged: Vec<(usize, f64)> = Vec::new();
    for &(joint, w) in links {
        if w <= 0.0 {
            continue;
        }
        let c = b.assign.get(joint as usize).copied().unwrap_or(fallback);
        match merged.iter_mut().find(|(k, _)| *k == c) {
            Some(e) => e.1 += w as f64,
            None => merged.push((c, w as f64)),
        }
    }
    if merged.is_empty() {
        merged.push((fallback, 1.0));
    }
    let total: f64 = merged.iter().map(|(_, w)| w).sum();
    merged.iter_mut().for_each(|(_, w)| *w /= total);
    merged.sort_by(|a, b| b.1.total_cmp(&a.1));
    let conformed = b.conform(pos);
    let rn = apply3(&b.rotation, normal);
    let (mut p, mut n) = ([0.0; 3], [0.0; 3]);
    for (c, w) in &merged {
        let xf = b.xf[*c].as_ref().or(b.xf[fallback].as_ref()).expect("pelvis is mapped");
        p = add(p, mulv(xf.apply(conformed), *w));
        n = add(n, mulv(xf.normal(conformed, rn), *w));
    }
    OutVertex { p, n: unit(n), links: merged }
}

/// The retargeted reference SMD: the vnmskel skeleton verbatim, the mesh in model space.
pub fn write_smd(scene: &Scene, names: &[String], skel: &NmSkeleton, b: &Baked) -> (String, usize, usize) {
    let mut out = String::from("version 1\nnodes\n");
    for (i, name) in skel.names.iter().enumerate() {
        let _ = writeln!(out, "{i} \"{name}\" {}", skel.parents[i].map_or(-1, |p| p as i64));
    }
    out.push_str("end\nskeleton\ntime 0\n");
    out.push_str(&pose_lines_turned(&skel.local, &skel.parents));
    out.push_str("end\ntriangles\n");
    let pelvis = skel.index("pelvis").expect("checked on load");
    let mut tris = 0;
    for mesh in &scene.meshes {
        let skinned: Vec<OutVertex> = (0..mesh.positions.len())
            .map(|i| {
                skin_vertex(b, mesh.positions[i].map(f64::from), mesh.normals[i].map(f64::from), &mesh.links[i], pelvis)
            })
            .collect();
        for tri in mesh.indices.as_chunks::<3>().0 {
            tris += 1;
            out.push_str(&names[mesh.material]);
            out.push('\n');
            for &i in tri {
                let i = i as usize;
                let v = &skinned[i];
                let (vp, vn) = (to_smd(v.p), to_smd(v.n));
                let uv = mesh.uv0[i];
                let _ = write!(
                    out,
                    "{} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {}",
                    v.links[0].0,
                    vp[0],
                    vp[1],
                    vp[2],
                    vn[0],
                    vn[1],
                    vn[2],
                    uv[0],
                    1.0 - uv[1] as f64,
                    v.links.len()
                );
                for (c, w) in &v.links {
                    let _ = write!(out, " {c} {w:.6}");
                }
                out.push('\n');
            }
        }
    }
    out.push_str("end\n");
    (out, tris, scene.vert_count())
}

/// [`pose_lines`] with the `Rz(-90)` compiler pre-turn applied to the root bones (children are
/// parent-relative and unaffected).
pub fn pose_lines_turned(poses: &[Pose], parents: &[Option<usize>]) -> String {
    let turn: Mat3 = [[0.0, 1.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 0.0, 1.0]]; // to_smd as a matrix
    let mut out = String::new();
    for (i, p) in poses.iter().enumerate() {
        let (t, r) = if parents.get(i).copied().flatten().is_none() {
            (to_smd(p.t), mul3(&turn, &quat_mat(p.q)))
        } else {
            (p.t, quat_mat(p.q))
        };
        let e = euler_of(&r);
        let _ = writeln!(out, "{i} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6}", t[0], t[1], t[2], e[0], e[1], e[2]);
    }
    out
}

/// `skeleton` lines of a pose list: `bone tx ty tz rx ry rz` (Source `RadianEuler`).
pub fn pose_lines(poses: &[Pose]) -> String {
    let mut out = String::new();
    for (i, p) in poses.iter().enumerate() {
        let e = euler_of(&quat_mat(p.q));
        let _ = writeln!(out, "{i} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6}", p.t[0], p.t[1], p.t[2], e[0], e[1], e[2]);
    }
    out
}
