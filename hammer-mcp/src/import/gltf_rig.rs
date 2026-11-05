//! glTF skin + animation reading for [`super::gltf_read`] (I5): the first skin's joints, bind
//! matrices and hierarchy, and every animation sampled at [`FPS`] into **joint global matrices**
//! (glTF axes, metres). Everything here is still in glTF space; [`super::skeleton`] turns it into
//! SMD frames.
//!
//! # Math
//!
//! * Bind world of joint `j` = `inverse(IBM_j)` (identity when the skin has no
//!   `inverseBindMatrices`, per the spec). Skinned vertices are kept exactly as given (skin
//!   space); the mesh node's own transform is ignored, as glTF defines for skinned meshes.
//! * Animated world `W_j(t)` = the joint node's global matrix at `t`: a node's rest local is its
//!   exact `matrix()` (so shear and zero axes survive); a node with an animation channel uses
//!   `T*R*S` (decomposed rest, sampled channels replacing it; the spec requires animated nodes
//!   to be TRS), a missing channel keeps the rest value, chained
//!   through **all** ancestors, joint or not (a non-joint ancestor's transform, animated or not,
//!   is thereby baked into every joint below it). A node with two parents, or in a cycle, is
//!   refused.
//! * A joint's parent is its nearest *joint* ancestor. More than one root joint adds a
//!   synthetic root bone (`synthetic`, identity bind, identity at every frame).
//! * An unskinned mesh in a skinned scene is attached rigidly to its nearest joint ancestor
//!   (itself included), else to the root bone `r`: its vertices become `bind_j * inverse(rest_j)
//!   * node_world * v`, so they sit where the rest pose puts them relative to the joint and then
//!   follow it. `rest_j` is the joint node's global matrix with no animation applied.
//! * Clips: frame `k` samples `t = min(t_min + k/30, t_max)` and the last frame samples exactly
//!   `t_max`; `t_min..t_max` is the span of every kept channel's keyframes; frames =
//!   `round((t_max - t_min) * 30) + 1` (a clip with no span is one frame). `frames * joints`
//!   summed over all clips is capped at [`MAX_SAMPLES`]. Keyframe times and values must be
//!   finite; rotation keys are normalised when read. LINEAR = lerp (slerp for rotation, shortest arc); STEP = hold the previous
//!   key; CUBICSPLINE uses each key's value component (tangents ignored, warning); before the
//!   first/after the last key the end value is held. `weights` (morph) channels are dropped.
use super::mat::{from_f32, from_trs, invert, mul, Mat4, IDENTITY};
use super::ImportError;
use gltf::animation::util::ReadOutputs;
use gltf::animation::{Interpolation, Property};
use std::collections::HashMap;

/// Clip sampling rate (frames per second).
pub const FPS: f64 = 30.0;
/// `frames * joints` summed over all clips above this is refused (memory for the sampled matrices).
const MAX_SAMPLES: usize = 2_000_000;
/// Skin weights below this (after normalising) are pruned.
pub const PRUNE_WEIGHT: f32 = 0.001;

#[derive(Debug, Clone, PartialEq)]
pub struct Joint {
    /// The node's name, or `joint_<node>` when it has none; `root` for the synthetic root.
    pub name: String,
    /// Index into [`Rig::joints`] of the nearest joint ancestor.
    pub parent: Option<usize>,
    /// Bind world transform in skin space: `inverse(IBM)`.
    pub bind: Mat4,
    /// The synthetic root added over several root joints.
    pub synthetic: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub name: Option<String>,
    /// Index in the file's `animations`.
    pub index: usize,
    pub frames: usize,
    /// `globals[frame][joint]`: the joint's global matrix, glTF axes and metres.
    pub globals: Vec<Vec<Mat4>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rig {
    pub joints: Vec<Joint>,
    pub clips: Vec<Clip>,
}

/// What the mesh walk needs besides the [`Rig`].
pub(super) struct RigRead {
    pub rig: Rig,
    pub skin_index: usize,
    /// Node index -> joint index (synthetic root not included).
    pub joint_of_node: HashMap<usize, usize>,
    pub node_parent: Vec<Option<usize>>,
    /// Rest global matrix per joint (identity for the synthetic root).
    pub rest: Vec<Mat4>,
    /// Index of the root bone.
    pub root: usize,
}

#[derive(Clone, Copy)]
struct Trs {
    t: [f64; 3],
    q: [f64; 4],
    s: [f64; 3],
}

fn trs_matrix(x: &Trs) -> Mat4 {
    from_trs(x.t, x.q, x.s)
}

fn parse_err(detail: impl Into<String>) -> ImportError {
    ImportError::Parse(detail.into())
}

struct Track {
    node: usize,
    property: Property,
    times: Vec<f32>,
    values: Vec<[f64; 4]>,
    step: bool,
}

fn push_unique(list: &mut Vec<String>, text: String) {
    if !list.contains(&text) {
        list.push(text);
    }
}

pub(super) fn read(
    doc: &gltf::Document,
    buffers: &[Vec<u8>],
    warnings: &mut Vec<String>,
    dropped: &mut Vec<String>,
) -> Result<Option<RigRead>, ImportError> {
    let count = doc.nodes().len();
    let mut node_parent: Vec<Option<usize>> = vec![None; count];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut two_parents: Option<usize> = None;
    for node in doc.nodes() {
        for child in node.children() {
            if node_parent[child.index()].is_some() && two_parents.is_none() {
                two_parents = Some(child.index());
            }
            node_parent[child.index()] = Some(node.index());
            children[node.index()].push(child.index());
        }
    }
    let Some(skin) = doc.skins().next() else {
        if doc.animations().len() > 0 {
            push_unique(
                dropped,
                "animations dropped: the file has no skin (node animation of unskinned meshes is not imported)".to_string(),
            );
        }
        return Ok(None);
    };
    if doc.skins().len() > 1 {
        push_unique(
            warnings,
            format!(
                "{} skins in the file: only the first is imported; meshes using the others are attached rigidly",
                doc.skins().len()
            ),
        );
        push_unique(dropped, format!("{} extra skin(s) dropped", doc.skins().len() - 1));
    }
    let label_of = |n: usize| -> String {
        doc.nodes().nth(n).and_then(|x| x.name().map(str::to_string)).filter(|s| !s.trim().is_empty()).unwrap_or_else(|| format!("#{n}"))
    };
    if let Some(n) = two_parents {
        return Err(parse_err(format!("node {} has more than one parent", label_of(n))));
    }
    // a decomposed TRS is only ever used for the nodes an animation targets; a non-finite one
    // (zero-axis matrix) falls back to the identity pieces
    let rest_trs: Vec<Trs> = doc
        .nodes()
        .map(|n| {
            let (t, q, s) = n.transform().decomposed();
            let (t, q, s) = (t.map(f64::from), q.map(f64::from), s.map(f64::from));
            if t.iter().chain(&q).chain(&s).all(|v| v.is_finite()) {
                Trs { t, q, s }
            } else {
                Trs { t: [0.0; 3], q: [0.0, 0.0, 0.0, 1.0], s: [1.0; 3] }
            }
        })
        .collect();
    let rest_locals: Vec<Mat4> = doc.nodes().map(|n| from_f32(n.transform().matrix())).collect();

    // parents first; every node must be reached exactly once (a cycle has no root to reach it from)
    let mut order: Vec<usize> = Vec::with_capacity(count);
    let mut seen = vec![false; count];
    let mut stack: Vec<usize> = (0..count).filter(|i| node_parent[*i].is_none()).rev().collect();
    while let Some(n) = stack.pop() {
        if std::mem::replace(&mut seen[n], true) {
            return Err(parse_err(format!("node {} is reached twice (cycle in the node hierarchy)", label_of(n))));
        }
        order.push(n);
        stack.extend(children[n].iter().rev());
    }
    if let Some(n) = seen.iter().position(|s| !s) {
        return Err(parse_err(format!("node {} is part of a cycle in the node hierarchy", label_of(n))));
    }
    let globals_of = |local_of: &dyn Fn(usize) -> Mat4, nodes: &[usize], out: &mut Vec<Mat4>| {
        for &n in nodes {
            let local = local_of(n);
            out[n] = match node_parent[n] {
                Some(p) => mul(&out[p], &local),
                None => local,
            };
        }
    };
    let mut rest_globals = vec![IDENTITY; count];
    globals_of(&|n| rest_locals[n], &order, &mut rest_globals);

    let joint_nodes: Vec<usize> = skin.joints().map(|j| j.index()).collect();
    if joint_nodes.is_empty() {
        return Err(parse_err("the skin has no joints"));
    }
    let mut joint_of_node: HashMap<usize, usize> = HashMap::new();
    for (j, n) in joint_nodes.iter().enumerate() {
        joint_of_node.entry(*n).or_insert(j);
    }
    let reader = skin.reader(|b| buffers.get(b.index()).map(|v| v.as_slice()));
    let ibms: Vec<Mat4> = match reader.read_inverse_bind_matrices() {
        Some(it) => it.map(from_f32).collect(),
        None => vec![IDENTITY; joint_nodes.len()],
    };
    if ibms.len() != joint_nodes.len() {
        return Err(parse_err(format!(
            "the skin has {} joints but {} inverse bind matrices",
            joint_nodes.len(),
            ibms.len()
        )));
    }
    let names_of: Vec<String> = doc.nodes().map(|n| n.name().map(str::to_string).unwrap_or_default()).collect();
    let mut joints: Vec<Joint> = Vec::new();
    let mut rest: Vec<Mat4> = Vec::new();
    for (j, &n) in joint_nodes.iter().enumerate() {
        let mut parent = node_parent[n];
        let mut guard = 0;
        while let Some(p) = parent {
            if joint_of_node.contains_key(&p) {
                break;
            }
            parent = node_parent[p];
            guard += 1;
            if guard > 4096 {
                return Err(parse_err("node hierarchy too deep (cycle?)"));
            }
        }
        let bind = invert(&ibms[j]).ok_or_else(|| {
            parse_err(format!("the inverse bind matrix of joint {:?} is singular", names_of[n]))
        })?;
        let name = if names_of[n].trim().is_empty() { format!("joint_{n}") } else { names_of[n].clone() };
        joints.push(Joint { name, parent: parent.map(|p| joint_of_node[&p]), bind, synthetic: false });
        rest.push(rest_globals[n]);
    }
    // a joint listed twice would be its own parent chain's problem: keep the first listing only
    let roots: Vec<usize> = (0..joints.len()).filter(|j| joints[*j].parent.is_none()).collect();
    if roots.is_empty() {
        return Err(parse_err("the skin's joints form a cycle"));
    }
    let root = if roots.len() > 1 {
        let synthetic = joints.len();
        for r in &roots {
            joints[*r].parent = Some(synthetic);
        }
        joints.push(Joint { name: "root".to_string(), parent: None, bind: IDENTITY, synthetic: true });
        rest.push(IDENTITY);
        push_unique(
            warnings,
            format!("{} root joints: a synthetic `root` bone was added as their parent", roots.len()),
        );
        synthetic
    } else {
        roots[0]
    };

    let mut needed = vec![false; count];
    for &n in &joint_nodes {
        let mut cur = Some(n);
        while let Some(c) = cur {
            if needed[c] {
                break;
            }
            needed[c] = true;
            cur = node_parent[c];
        }
    }
    let needed_order: Vec<usize> = order.iter().copied().filter(|n| needed[*n]).collect();

    // animations
    let mut clips: Vec<Clip> = Vec::new();
    let mut samples = 0usize;
    for animation in doc.animations() {
        let label = animation.name().map(str::to_string).unwrap_or_else(|| format!("#{}", animation.index()));
        let mut tracks: Vec<Track> = Vec::new();
        for channel in animation.channels() {
            let target = channel.target();
            let node = target.node().index();
            let property = target.property();
            if property == Property::MorphTargetWeights {
                push_unique(dropped, format!("animation {label}: morph target weight channels dropped"));
                continue;
            }
            if !needed[node] {
                push_unique(
                    dropped,
                    format!(
                        "animation {label}: channels on node {:?}, which is not part of the skeleton, dropped",
                        names_of[node]
                    ),
                );
                continue;
            }
            let interpolation = channel.sampler().interpolation();
            let cubic = interpolation == Interpolation::CubicSpline;
            if cubic {
                push_unique(
                    warnings,
                    format!("animation {label}: CUBICSPLINE sampled with the key values only (tangents ignored)"),
                );
            }
            let reader = channel.reader(|b| buffers.get(b.index()).map(|v| v.as_slice()));
            let times: Vec<f32> = match reader.read_inputs() {
                Some(it) => it.collect(),
                None => continue,
            };
            let raw: Vec<[f64; 4]> = match reader.read_outputs() {
                Some(ReadOutputs::Translations(it)) => it.map(|v| [v[0] as f64, v[1] as f64, v[2] as f64, 0.0]).collect(),
                Some(ReadOutputs::Scales(it)) => it.map(|v| [v[0] as f64, v[1] as f64, v[2] as f64, 0.0]).collect(),
                Some(ReadOutputs::Rotations(it)) => it.into_f32().map(|v| normalised(v.map(f64::from))).collect(),
                _ => continue,
            };
            let values: Vec<[f64; 4]> = if cubic {
                raw.iter().skip(1).step_by(3).copied().collect()
            } else {
                raw
            };
            if times.is_empty() || values.len() != times.len() {
                return Err(parse_err(format!(
                    "animation {label}: a channel has {} keyframe times but {} values",
                    times.len(),
                    values.len()
                )));
            }
            if times.windows(2).any(|w| w[1] < w[0]) || times.iter().any(|t| !t.is_finite()) {
                return Err(parse_err(format!("animation {label}: keyframe times are not increasing")));
            }
            if values.iter().any(|v| v.iter().any(|x| !x.is_finite())) {
                return Err(parse_err(format!("animation {label}: a channel has a non-finite keyframe value")));
            }
            tracks.push(Track { node, property, times, values, step: interpolation == Interpolation::Step });
        }
        let (mut t_min, mut t_max) = (f64::MAX, f64::MIN);
        for track in &tracks {
            t_min = t_min.min(track.times[0] as f64);
            t_max = t_max.max(*track.times.last().unwrap() as f64);
        }
        if tracks.is_empty() {
            (t_min, t_max) = (0.0, 0.0);
            push_unique(warnings, format!("animation {label} has no usable channels: a single rest-pose frame"));
        }
        // range-check in f64 before the cast: a key at 1e30 would saturate it
        let frames_f = ((t_max - t_min) * FPS).round() + 1.0;
        let bones = joints.len();
        if !frames_f.is_finite() || frames_f * bones as f64 + samples as f64 > MAX_SAMPLES as f64 {
            return Err(parse_err(format!(
                "animation {label}: {frames_f:e} frames x {bones} bones (+ {samples} samples from earlier clips) is over the {MAX_SAMPLES} sample cap for all clips together; shorten or decimate it"
            )));
        }
        let frames = frames_f as usize;
        samples += frames * bones;
        let mut animated = vec![false; count];
        for track in &tracks {
            animated[track.node] = true;
        }
        let mut cur = rest_trs.clone();
        let mut globals = vec![IDENTITY; count];
        let mut out: Vec<Vec<Mat4>> = Vec::with_capacity(frames);
        for k in 0..frames {
            // the last frame lands exactly on the last key
            let t = if k + 1 == frames { t_max } else { (t_min + k as f64 / FPS).min(t_max) };
            for track in &tracks {
                let v = sample(track, t);
                match track.property {
                    Property::Translation => cur[track.node].t = [v[0], v[1], v[2]],
                    Property::Scale => cur[track.node].s = [v[0], v[1], v[2]],
                    _ => cur[track.node].q = v,
                }
            }
            globals_of(
                &|n| if animated[n] { trs_matrix(&cur[n]) } else { rest_locals[n] },
                &needed_order,
                &mut globals,
            );
            out.push(
                joints
                    .iter()
                    .enumerate()
                    .map(|(j, joint)| if joint.synthetic { IDENTITY } else { globals[joint_nodes[j]] })
                    .collect(),
            );
        }
        clips.push(Clip { name: animation.name().map(str::to_string), index: animation.index(), frames, globals: out });
    }
    Ok(Some(RigRead {
        rig: Rig { joints, clips },
        skin_index: skin.index(),
        joint_of_node,
        node_parent,
        rest,
        root,
    }))
}

/// A quaternion scaled to unit length (the identity for a zero one); non-finite stays non-finite
/// so the caller's finiteness check refuses it.
fn normalised(q: [f64; 4]) -> [f64; 4] {
    let len = q.iter().map(|v| v * v).sum::<f64>().sqrt();
    if !len.is_finite() {
        [f64::NAN; 4]
    } else if len < 1e-12 {
        [0.0, 0.0, 0.0, 1.0]
    } else {
        q.map(|v| v / len)
    }
}

/// A track's value at `t` (see the module docs for the interpolation rules).
fn sample(track: &Track, t: f64) -> [f64; 4] {
    let times = &track.times;
    let last = times.len() - 1;
    if t <= times[0] as f64 {
        return track.values[0];
    }
    if t >= times[last] as f64 {
        return track.values[last];
    }
    // first key strictly after t
    let hi = times.partition_point(|k| (*k as f64) <= t);
    let lo = hi - 1;
    let (t0, t1) = (times[lo] as f64, times[hi] as f64);
    if track.step || t1 <= t0 {
        return track.values[lo];
    }
    let u = (t - t0) / (t1 - t0);
    let (a, b) = (track.values[lo], track.values[hi]);
    if track.property == Property::Rotation {
        slerp(a, b, u)
    } else {
        [a[0] + (b[0] - a[0]) * u, a[1] + (b[1] - a[1]) * u, a[2] + (b[2] - a[2]) * u, 0.0]
    }
}

fn slerp(a: [f64; 4], mut b: [f64; 4], u: f64) -> [f64; 4] {
    let mut dot: f64 = (0..4).map(|i| a[i] * b[i]).sum();
    if dot < 0.0 {
        b = b.map(|v| -v);
        dot = -dot;
    }
    let (wa, wb) = if dot > 0.9995 {
        (1.0 - u, u)
    } else {
        let theta = dot.clamp(-1.0, 1.0).acos();
        let s = theta.sin();
        (((1.0 - u) * theta).sin() / s, (u * theta).sin() / s)
    };
    let q = [0, 1, 2, 3].map(|i| a[i] * wa + b[i] * wb);
    let len = q.iter().map(|v| v * v).sum::<f64>().sqrt();
    if len < 1e-12 {
        [0.0, 0.0, 0.0, 1.0]
    } else {
        q.map(|v| v / len)
    }
}
