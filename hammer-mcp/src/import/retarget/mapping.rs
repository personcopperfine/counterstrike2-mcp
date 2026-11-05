//! Source joint -> CS2 bone mapping (I6): explicit `boneMap` > name tables > structural fallback.
//!
//! # Order
//!
//! 1. **Explicit** `boneMap {sourceJoint: cs2Bone}`: validated (an unknown name on either side is
//!    refused with near misses, a CS2 bone that never carries weights - `wpn*`, `attach*`,
//!    `camera`, `root_motion` - is refused, two joints on one bone are refused). Always wins.
//! 2. **Name tables** (`mixamo`, `rigify`, `ue`, `vrm`): each joint name is canonicalised
//!    ([`canon`]: after the last `:`/`|`, lowercased, `mixamorig<digits>` and (rigify) `DEF-`
//!    stripped, split at `_ . - space`, a `l/r/left/right` first or last token - or a `left`/`right`
//!    prefix - becomes the side, the rest is joined) and looked up in the table's rows. A row is a
//!    role (CS2 bone or chain) and the ordered canonical names that fill it. Chains: the source
//!    spine joints found are spread over `spine_0..3` by even spacing (`n = 1` -> `spine_0`;
//!    `2..=4` -> slot `round(i * 3 / (n - 1))`; more than 4 -> 4 evenly spaced ones), finger
//!    chains are aligned to the fingertip end by table row (3 names -> `0,1,2`, 4 -> `meta,0,1,2`; a
//!    joint keeps its row position when others are missing, but at least its rank among the found). `rig:"auto"`
//!    tries every table and keeps the one with the most required roles (then the most bones).
//! 3. **Structural**, only for the **core set** and only when 1+2 left a required bone open
//!    ([`structural`]): the pelvis is the shallowest joint with three or more children of which
//!    one subtree (the trunk) is at least twice the other two (the legs, similar in size); the
//!    spine follows single children up to the first branching joint (the top spine); there the
//!    child pointing most along `chest - pelvis` is the neck chain and the two remaining
//!    biggest subtrees are the arms; arm and leg chains follow single children; left/right by
//!    the sign of `dot(end - chest, up x Z)` (glTF: front +Z, so the character's left is +X when
//!    up is +Y).
//!
//! Source joints that stay unmapped hand their weights to the nearest mapped ancestor
//! ([`super::bake`]).
use super::skeleton::NmSkeleton;
use crate::import::gltf_rig::Rig;
use crate::import::mat::Mat4;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    L,
    R,
}

impl Side {
    fn suffix(self) -> &'static str {
        match self {
            Side::L => "L",
            Side::R => "R",
        }
    }
}

/// How a pair was found (the report's `how`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    Explicit,
    Table(&'static str),
    Structural,
}

impl How {
    pub fn text(&self) -> String {
        match self {
            How::Explicit => "explicit".to_string(),
            How::Table(t) => format!("table:{t}"),
            How::Structural => "structural".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pair {
    /// Index into `Rig::joints`.
    pub src: usize,
    /// Index into the CS2 skeleton.
    pub cs2: usize,
    pub how: How,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mapping {
    pub pairs: Vec<Pair>,
    /// The table that supplied the name matches (`None` = explicit/structural only).
    pub rig: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RigHint {
    #[default]
    Auto,
    Mixamo,
    Rigify,
    Ue,
    Vrm,
}

impl RigHint {
    pub const NAMES: [&'static str; 5] = ["auto", "mixamo", "rigify", "ue", "vrm"];
    pub fn parse(text: &str) -> Option<RigHint> {
        Some(match text {
            "auto" => RigHint::Auto,
            "mixamo" => RigHint::Mixamo,
            "rigify" => RigHint::Rigify,
            "ue" => RigHint::Ue,
            "vrm" => RigHint::Vrm,
            _ => return None,
        })
    }
}

#[derive(Debug)]
pub enum MapError {
    /// A `boneMap` entry that does not hold up (detail, near misses).
    Invalid { detail: String, near: Vec<String> },
    /// Required CS2 bones with no source joint.
    Incomplete { unmapped: Vec<String>, required: Vec<String> },
}

// ------------------------------------------------------------------------------ roles

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Limb {
    Clavicle,
    ArmUpper,
    ArmLower,
    Hand,
    LegUpper,
    LegLower,
    Ankle,
    Ball,
}

impl Limb {
    fn base(self) -> &'static str {
        match self {
            Limb::Clavicle => "clavicle",
            Limb::ArmUpper => "arm_upper",
            Limb::ArmLower => "arm_lower",
            Limb::Hand => "hand",
            Limb::LegUpper => "leg_upper",
            Limb::LegLower => "leg_lower",
            Limb::Ankle => "ankle",
            Limb::Ball => "ball",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Finger {
    Thumb,
    Index,
    Middle,
    Ring,
    Pinky,
}

impl Finger {
    fn name(self) -> &'static str {
        match self {
            Finger::Thumb => "thumb",
            Finger::Index => "index",
            Finger::Middle => "middle",
            Finger::Ring => "ring",
            Finger::Pinky => "pinky",
        }
    }
    /// CS2 bone names of the chain, base to tip.
    fn slots(self, side: Side) -> Vec<String> {
        let s = side.suffix();
        let f = self.name();
        if self == Finger::Thumb {
            (0..3).map(|i| format!("finger_{f}_{i}_{s}")).collect()
        } else {
            std::iter::once(format!("finger_{f}_meta_{s}")).chain((0..3).map(|i| format!("finger_{f}_{i}_{s}"))).collect()
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Role {
    Pelvis,
    Spine,
    Neck,
    Head,
    Limb(Limb),
    Finger(Finger),
}

type Row = (Role, &'static [&'static str]);

use Finger::*;
use Limb::*;
use Role::{Finger as Fg, Head, Limb as Lb, Neck, Pelvis, Spine};

const MIXAMO: &[Row] = &[
    (Pelvis, &["hips"]),
    (Spine, &["spine", "spine1", "spine2"]),
    (Neck, &["neck"]),
    (Head, &["head"]),
    (Lb(Clavicle), &["shoulder"]),
    (Lb(ArmUpper), &["arm"]),
    (Lb(ArmLower), &["forearm"]),
    (Lb(Hand), &["hand"]),
    (Lb(LegUpper), &["upleg"]),
    (Lb(LegLower), &["leg"]),
    (Lb(Ankle), &["foot"]),
    (Lb(Ball), &["toebase"]),
    (Fg(Thumb), &["handthumb1", "handthumb2", "handthumb3"]),
    (Fg(Index), &["handindex1", "handindex2", "handindex3"]),
    (Fg(Middle), &["handmiddle1", "handmiddle2", "handmiddle3"]),
    (Fg(Ring), &["handring1", "handring2", "handring3"]),
    (Fg(Pinky), &["handpinky1", "handpinky2", "handpinky3"]),
];

const RIGIFY: &[Row] = &[
    (Pelvis, &["spine"]),
    (Spine, &["spine001", "spine002", "spine003"]),
    (Neck, &["spine004"]),
    (Head, &["spine006", "spine005"]),
    (Lb(Clavicle), &["shoulder"]),
    (Lb(ArmUpper), &["upperarm"]),
    (Lb(ArmLower), &["forearm"]),
    (Lb(Hand), &["hand"]),
    (Lb(LegUpper), &["thigh"]),
    (Lb(LegLower), &["shin"]),
    (Lb(Ankle), &["foot"]),
    (Lb(Ball), &["toe"]),
    (Fg(Thumb), &["thumb01", "thumb02", "thumb03"]),
    (Fg(Index), &["findex01", "findex02", "findex03"]),
    (Fg(Middle), &["fmiddle01", "fmiddle02", "fmiddle03"]),
    (Fg(Ring), &["fring01", "fring02", "fring03"]),
    (Fg(Pinky), &["fpinky01", "fpinky02", "fpinky03"]),
];

const UE: &[Row] = &[
    (Pelvis, &["pelvis"]),
    (Spine, &["spine01", "spine02", "spine03", "spine04", "spine05"]),
    (Neck, &["neck01"]),
    (Head, &["head"]),
    (Lb(Clavicle), &["clavicle"]),
    (Lb(ArmUpper), &["upperarm"]),
    (Lb(ArmLower), &["lowerarm"]),
    (Lb(Hand), &["hand"]),
    (Lb(LegUpper), &["thigh"]),
    (Lb(LegLower), &["calf"]),
    (Lb(Ankle), &["foot"]),
    (Lb(Ball), &["ball"]),
    (Fg(Thumb), &["thumb01", "thumb02", "thumb03"]),
    (Fg(Index), &["indexmetacarpal", "index01", "index02", "index03"]),
    (Fg(Middle), &["middlemetacarpal", "middle01", "middle02", "middle03"]),
    (Fg(Ring), &["ringmetacarpal", "ring01", "ring02", "ring03"]),
    (Fg(Pinky), &["pinkymetacarpal", "pinky01", "pinky02", "pinky03"]),
];

const VRM: &[Row] = &[
    (Pelvis, &["hips"]),
    (Spine, &["spine", "chest", "upperchest"]),
    (Neck, &["neck"]),
    (Head, &["head"]),
    (Lb(Clavicle), &["shoulder"]),
    (Lb(ArmUpper), &["upperarm"]),
    (Lb(ArmLower), &["lowerarm"]),
    (Lb(Hand), &["hand"]),
    (Lb(LegUpper), &["upperleg"]),
    (Lb(LegLower), &["lowerleg"]),
    (Lb(Ankle), &["foot"]),
    (Lb(Ball), &["toes"]),
    // VRM 0.x: Proximal/Intermediate/Distal; VRM 1.0 thumb: Metacarpal/Proximal/Distal
    (Fg(Thumb), &["thumbmetacarpal", "thumbproximal", "thumbintermediate", "thumbdistal"]),
    (Fg(Index), &["indexproximal", "indexintermediate", "indexdistal"]),
    (Fg(Middle), &["middleproximal", "middleintermediate", "middledistal"]),
    (Fg(Ring), &["ringproximal", "ringintermediate", "ringdistal"]),
    (Fg(Pinky), &["littleproximal", "littleintermediate", "littledistal"]),
];

const TABLES: &[(&str, &[Row])] = &[("mixamo", MIXAMO), ("rigify", RIGIFY), ("ue", UE), ("vrm", VRM)];

// ------------------------------------------------------------------------------ canonical names

/// `(key, side)` of a joint name for a table (module docs). `rigify` also strips `DEF-`.
pub fn canon(name: &str, rigify: bool) -> (String, Option<Side>) {
    let mut s = name.trim();
    if let Some(p) = s.rfind([':', '|']) {
        s = &s[p + 1..];
    }
    let mut lower = s.to_lowercase();
    if lower.starts_with("mixamorig") {
        lower = lower["mixamorig".len()..].trim_start_matches(|c: char| c.is_ascii_digit()).to_string();
    }
    if rigify {
        for prefix in ["def-", "def_", "def."] {
            if let Some(rest) = lower.strip_prefix(prefix) {
                lower = rest.to_string();
                break;
            }
        }
    }
    let mut tokens: Vec<String> = lower.split(['_', '.', '-', ' ']).filter(|t| !t.is_empty()).map(str::to_string).collect();
    let side_of = |t: &str| match t {
        "l" | "left" => Some(Side::L),
        "r" | "right" => Some(Side::R),
        _ => None,
    };
    let mut side = None;
    if tokens.len() > 1 {
        if let Some(sd) = tokens.last().and_then(|t| side_of(t)) {
            side = Some(sd);
            tokens.pop();
        } else if let Some(sd) = side_of(&tokens[0]) {
            side = Some(sd);
            tokens.remove(0);
        }
    }
    if side.is_none() {
        if let Some(first) = tokens.first_mut() {
            for (prefix, sd) in [("left", Side::L), ("right", Side::R)] {
                if first.len() > prefix.len() && first.starts_with(prefix) {
                    side = Some(sd);
                    *first = first[prefix.len()..].to_string();
                    break;
                }
            }
        }
    }
    (tokens.concat(), side)
}

/// Spread `n` ordered sources over the four spine slots (module docs): `(source index, slot)`.
fn spine_slots(n: usize) -> Vec<(usize, usize)> {
    match n {
        0 => vec![],
        1 => vec![(0, 0)],
        2..=4 => (0..n).map(|i| (i, (i as f64 * 3.0 / (n - 1) as f64).round() as usize)).collect(),
        _ => (0..4).map(|j| ((j as f64 * (n - 1) as f64 / 3.0).round() as usize, j)).collect(),
    }
}

/// What one table maps in this rig: `(joint, CS2 bone index)`.
fn run_table(rig: &Rig, skel: &NmSkeleton, name: &'static str, rows: &[Row]) -> Vec<(usize, usize)> {
    let rigify = name == "rigify";
    let mut index: HashMap<(String, Option<Side>), usize> = HashMap::new();
    for (j, joint) in rig.joints.iter().enumerate() {
        if joint.synthetic {
            continue;
        }
        index.entry(canon(&joint.name, rigify)).or_insert(j);
    }
    let found = |keys: &[&str], side: Option<Side>| -> Vec<usize> {
        keys.iter().filter_map(|k| index.get(&(k.to_string(), side)).copied()).collect()
    };
    let bone = |n: &str| skel.index(n);
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut push = |j: usize, cs2: Option<usize>| {
        if let Some(c) = cs2 {
            out.push((j, c));
        }
    };
    for (role, keys) in rows {
        match role {
            Role::Pelvis | Role::Neck | Role::Head => {
                let cs2 = match role {
                    Role::Pelvis => "pelvis",
                    Role::Neck => "neck_0",
                    _ => "head_0",
                };
                if let Some(&j) = found(keys, None).first() {
                    push(j, bone(cs2));
                }
            }
            Role::Spine => {
                let js = found(keys, None);
                for (i, slot) in spine_slots(js.len()) {
                    push(js[i], bone(&format!("spine_{slot}")));
                }
            }
            Role::Limb(limb) => {
                for side in [Side::L, Side::R] {
                    if let Some(&j) = found(keys, Some(side)).first() {
                        push(j, bone(&format!("{}_{}", limb.base(), side.suffix())));
                    }
                }
            }
            Role::Finger(finger) => {
                for side in [Side::L, Side::R] {
                    // A table row is tip-aligned with the CS2 chain. Each found joint takes its row
                    // position, but at least its rank among the found joints: a partial chain (only
                    // `HandMiddle1`) keeps its own segment, and a merged row (VRM 0 + VRM 1 thumb
                    // names) still fills the chain from its base.
                    let slots = finger.slots(side);
                    let hits = keys.iter().enumerate().filter_map(|(ki, key)| {
                        found(std::slice::from_ref(key), Some(side)).first().map(|&j| (ki, j))
                    });
                    for (rank, (ki, j)) in hits.enumerate() {
                        let slot = (slots.len() + ki).saturating_sub(keys.len()).max(rank);
                        if let Some(name) = slots.get(slot) {
                            push(j, bone(name));
                        }
                    }
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------------------------ required set

/// The bones a player model needs a source for. `spine_*` stands for "at least one spine bone".
pub fn required_names() -> Vec<String> {
    let mut v: Vec<String> = vec!["pelvis".into(), "spine_*".into(), "neck_0".into(), "head_0".into()];
    for side in ["L", "R"] {
        for limb in [Clavicle, ArmUpper, ArmLower, Hand, LegUpper, LegLower, Ankle] {
            v.push(format!("{}_{side}", limb.base()));
        }
    }
    v
}

/// Which required names `have` (a set of CS2 bone names) leaves open.
pub fn missing_required(have: &dyn Fn(&str) -> bool) -> Vec<String> {
    required_names()
        .into_iter()
        .filter(|n| if n == "spine_*" { !(0..4).any(|i| have(&format!("spine_{i}"))) } else { !have(n) })
        .collect()
}

// ------------------------------------------------------------------------------ build

/// The whole mapping. `bone_map` entries are `(source joint name, CS2 bone name)`.
pub fn build(rig: &Rig, skel: &NmSkeleton, bone_map: &[(String, String)], hint: RigHint) -> Result<Mapping, MapError> {
    let mut by_cs2: HashMap<usize, Pair> = HashMap::new();
    let mut used_src: HashMap<usize, usize> = HashMap::new();

    // 1. explicit
    let real_joints: Vec<usize> = (0..rig.joints.len()).filter(|j| !rig.joints[*j].synthetic).collect();
    for (src_name, cs2_name) in bone_map {
        let src = find_joint(rig, &real_joints, src_name)?;
        let cs2 = find_cs2(skel, cs2_name)?;
        if let Some(prev) = used_src.get(&src) {
            return Err(MapError::Invalid {
                detail: format!("boneMap maps {src_name:?} twice (already to {:?})", skel.names[*prev]),
                near: vec![],
            });
        }
        if let Some(other) = by_cs2.get(&cs2) {
            return Err(MapError::Invalid {
                detail: format!(
                    "boneMap maps both {:?} and {src_name:?} to {:?}: a CS2 bone takes one source joint",
                    rig.joints[other.src].name, skel.names[cs2]
                ),
                near: vec![],
            });
        }
        used_src.insert(src, cs2);
        by_cs2.insert(cs2, Pair { src, cs2, how: How::Explicit });
    }

    // 2. tables
    let required_have = |pairs: &[(usize, usize)]| -> (usize, usize) {
        let names: Vec<&str> = pairs.iter().map(|(_, c)| skel.names[*c].as_str()).collect();
        let open = missing_required(&|n| names.contains(&n));
        (required_names().len() - open.len(), pairs.len())
    };
    let candidates: Vec<(&'static str, &'static [Row])> = TABLES
        .iter()
        .filter(|(n, _)| match hint {
            RigHint::Auto => true,
            RigHint::Mixamo => *n == "mixamo",
            RigHint::Rigify => *n == "rigify",
            RigHint::Ue => *n == "ue",
            RigHint::Vrm => *n == "vrm",
        })
        .copied()
        .collect();
    let mut best: Option<(&'static str, Vec<(usize, usize)>)> = None;
    for (name, rows) in candidates {
        // explicit entries already own their joints and bones: score what the table adds on top
        let mut got = run_table(rig, skel, name, rows);
        got.retain(|(j, c)| !used_src.contains_key(j) && !by_cs2.contains_key(c));
        let mut with_explicit: Vec<(usize, usize)> = by_cs2.values().map(|p| (p.src, p.cs2)).collect();
        with_explicit.extend(got.iter().copied());
        let score = required_have(&with_explicit);
        let better = match &best {
            None => true,
            Some((_, b)) => {
                let mut b_all: Vec<(usize, usize)> = by_cs2.values().map(|p| (p.src, p.cs2)).collect();
                b_all.extend(b.iter().copied());
                score > required_have(&b_all)
            }
        };
        if better {
            best = Some((name, got));
        }
    }
    let mut rig_name = None;
    if let Some((name, got)) = best {
        if !got.is_empty() {
            rig_name = Some(name);
        }
        for (j, c) in got {
            if !used_src.contains_key(&j) && !by_cs2.contains_key(&c) {
                used_src.insert(j, c);
                by_cs2.insert(c, Pair { src: j, cs2: c, how: How::Table(name) });
            }
        }
    }

    // 3. structural, core set only
    let open = |by_cs2: &HashMap<usize, Pair>| missing_required(&|n| skel.index(n).is_some_and(|c| by_cs2.contains_key(&c)));
    if !open(&by_cs2).is_empty() {
        for (cs2_name, j) in structural(rig, &real_joints) {
            let Some(c) = skel.index(&cs2_name) else { continue };
            if !by_cs2.contains_key(&c) && !used_src.contains_key(&j) {
                used_src.insert(j, c);
                by_cs2.insert(c, Pair { src: j, cs2: c, how: How::Structural });
            }
        }
    }
    let missing = open(&by_cs2);
    if !missing.is_empty() {
        return Err(MapError::Incomplete { unmapped: missing, required: required_names() });
    }
    check_chain_order(rig, skel, &by_cs2)?;
    let mut pairs: Vec<Pair> = by_cs2.into_values().collect();
    pairs.sort_by_key(|p| p.cs2);
    Ok(Mapping { pairs, rig: rig_name })
}

/// Along every CS2 chain the mapped source joints must not run up the source hierarchy: a joint
/// that is an ancestor of the previous one is refused. `boneMap {"Spine": "spine_3"}` on a Mixamo rig would otherwise
/// leave the table's `Spine1` on `spine_2` below it and turn the torso upside down.
fn check_chain_order(rig: &Rig, skel: &NmSkeleton, by_cs2: &HashMap<usize, Pair>) -> Result<(), MapError> {
    let descends = |j: usize, anc: usize| {
        let mut cur = rig.joints[j].parent;
        for _ in 0..rig.joints.len() {
            match cur {
                Some(k) if k == anc => return true,
                Some(k) => cur = rig.joints[k].parent,
                None => return false,
            }
        }
        false
    };
    for chain in super::bake::chain_names() {
        let mut prev: Option<&Pair> = None;
        for name in &chain {
            let Some(p) = skel.index(name).and_then(|c| by_cs2.get(&c)) else { continue };
            if let Some(q) = prev {
                if descends(q.src, p.src) {
                    return Err(MapError::Invalid {
                        detail: format!(
                            "{:?} ({}) is above {:?} ({}) in the source hierarchy, but {} follows {} in the CS2 chain; fix boneMap",
                            rig.joints[p.src].name,
                            skel.names[p.cs2],
                            rig.joints[q.src].name,
                            skel.names[q.cs2],
                            skel.names[p.cs2],
                            skel.names[q.cs2]
                        ),
                        near: vec![],
                    });
                }
            }
            prev = Some(p);
        }
    }
    Ok(())
}

fn find_joint(rig: &Rig, real: &[usize], name: &str) -> Result<usize, MapError> {
    if let Some(&j) = real.iter().find(|j| rig.joints[**j].name == name) {
        return Ok(j);
    }
    let ci: Vec<usize> = real.iter().copied().filter(|j| rig.joints[*j].name.eq_ignore_ascii_case(name)).collect();
    if ci.len() == 1 {
        return Ok(ci[0]);
    }
    let near = cs2_mcp_common::tools::nearest_names(name, real.iter().map(|j| rig.joints[*j].name.as_str()), 3);
    Err(MapError::Invalid { detail: format!("boneMap source joint {name:?} is not in the file's skin"), near })
}

fn find_cs2(skel: &NmSkeleton, name: &str) -> Result<usize, MapError> {
    let exact = skel.index(name).or_else(|| skel.names.iter().position(|n| n.eq_ignore_ascii_case(name)));
    match exact {
        Some(c) if NmSkeleton::is_weightless(&skel.names[c]) => Err(MapError::Invalid {
            detail: format!(
                "boneMap target {name:?} never carries weights (wpn*, attach*, camera and root_motion are driven by the game)"
            ),
            near: vec![],
        }),
        Some(c) => Ok(c),
        None => {
            let weightable: Vec<&str> = skel.weightable().map(|i| skel.names[i].as_str()).collect();
            let near = cs2_mcp_common::tools::nearest_names(name, weightable, 3);
            Err(MapError::Invalid { detail: format!("boneMap target {name:?} is not a CS2 player bone"), near })
        }
    }
}

// ------------------------------------------------------------------------------ structural

fn pos(m: &Mat4) -> [f64; 3] {
    [m[3][0], m[3][1], m[3][2]]
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn unit(v: [f64; 3]) -> [f64; 3] {
    let l = dot(v, v).sqrt();
    if l < 1e-12 { [0.0; 3] } else { [v[0] / l, v[1] / l, v[2] / l] }
}

/// The structural guess (module docs): `(CS2 bone name, joint)` for the core set only.
pub fn structural(rig: &Rig, real: &[usize]) -> Vec<(String, usize)> {
    let n = rig.joints.len();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &j in real {
        if let Some(p) = rig.joints[j].parent {
            if !rig.joints[p].synthetic {
                children[p].push(j);
            }
        }
    }
    let roots: Vec<usize> = real.iter().copied().filter(|j| rig.joints[*j].parent.is_none_or(|p| rig.joints[p].synthetic)).collect();
    // subtree sizes and depth by an iterative walk (parents are not ordered before children here)
    let mut size = vec![1usize; n];
    let mut depth = vec![0usize; n];
    let mut order: Vec<usize> = Vec::new();
    let mut stack = roots.clone();
    while let Some(j) = stack.pop() {
        order.push(j);
        for &c in &children[j] {
            depth[c] = depth[j] + 1;
            stack.push(c);
        }
    }
    for &j in order.iter().rev() {
        for &c in &children[j] {
            size[j] += size[c];
        }
    }
    let p = |j: usize| pos(&rig.joints[j].bind);
    // pelvis: the shallowest joint with a trunk child at least twice the next, and two similar legs
    let mut pelvis = None;
    for &j in &order {
        if children[j].len() < 3 {
            continue;
        }
        let mut by_size = children[j].clone();
        by_size.sort_by(|a, b| size[*b].cmp(&size[*a]));
        let (trunk, leg_a, leg_b) = (size[by_size[0]], size[by_size[1]], size[by_size[2]]);
        if trunk >= 2 * leg_a && leg_a <= 2 * leg_b && leg_a >= 3 {
            let better = pelvis.as_ref().is_none_or(|(best, _): &(usize, Vec<usize>)| depth[j] < depth[*best]);
            if better {
                pelvis = Some((j, by_size));
            }
        }
    }
    let Some((pelvis, by_size)) = pelvis else { return vec![] };
    let mut out: Vec<(String, usize)> = vec![("pelvis".to_string(), pelvis)];
    // spine: single children up to the first branching joint
    let mut spine = vec![by_size[0]];
    while children[*spine.last().unwrap()].len() == 1 {
        let next = children[*spine.last().unwrap()][0];
        spine.push(next);
    }
    let chest = *spine.last().unwrap();
    if children[chest].len() < 3 {
        // the trunk never branches into neck + arms: only what is certain
        return out;
    }
    for (i, slot) in spine_slots(spine.len()) {
        out.push((format!("spine_{slot}"), spine[i]));
    }
    let up = unit(sub(p(chest), p(pelvis)));
    // the neck chain points most along the trunk; the two next-biggest subtrees are the arms
    let mut kids = children[chest].clone();
    kids.sort_by(|a, b| dot(unit(sub(p(*b), p(chest))), up).total_cmp(&dot(unit(sub(p(*a), p(chest))), up)));
    let neck_root = kids[0];
    let mut arms: Vec<usize> = kids[1..].to_vec();
    arms.sort_by(|a, b| size[*b].cmp(&size[*a]));
    arms.truncate(2);
    // neck chain
    let mut path = vec![neck_root];
    while children[*path.last().unwrap()].len() == 1 {
        let next = children[*path.last().unwrap()][0];
        path.push(next);
    }
    if children[*path.last().unwrap()].len() > 1 {
        // a branching head (eyes, jaw, top)
    } else if path.len() >= 3 {
        let (a, b, c) = (p(path[path.len() - 3]), p(path[path.len() - 2]), p(path[path.len() - 1]));
        let (prev, last) = (dot(sub(b, a), sub(b, a)).sqrt(), dot(sub(c, b), sub(c, b)).sqrt());
        if last < 0.5 * prev {
            path.pop(); // the last joint is a head-top end marker
        }
    }
    out.push(("neck_0".to_string(), path[0]));
    if path.len() >= 2 {
        out.push(("head_0".to_string(), *path.last().unwrap()));
    }
    // sides: character's left = up x forward, forward assumed +Z (glTF front)
    let forward = if up[2].abs() > 0.9 { [0.0, 1.0, 0.0] } else { [0.0, 0.0, 1.0] };
    let left = unit(cross(up, forward));
    let chain_of = |root: usize| -> Vec<usize> {
        let mut chain = vec![root];
        while children[*chain.last().unwrap()].len() == 1 {
            let next = children[*chain.last().unwrap()][0];
            chain.push(next);
        }
        chain
    };
    let sided = |roots: Vec<usize>, origin: [f64; 3], names: [&str; 4], out: &mut Vec<(String, usize)>| {
        let chains: Vec<Vec<usize>> = roots.into_iter().map(chain_of).collect();
        if chains.len() != 2 {
            return;
        }
        let end = |c: &Vec<usize>| dot(sub(p(*c.last().unwrap()), origin), left);
        let (l, r) = if end(&chains[0]) >= end(&chains[1]) { (&chains[0], &chains[1]) } else { (&chains[1], &chains[0]) };
        for (chain, side) in [(l, "L"), (r, "R")] {
            let len = chain.len();
            // names: [first, second, second-last (arms) / third (legs), last / unused]
            let pick: Vec<Option<usize>> = if names[0] == "clavicle" {
                if len >= 4 { vec![Some(0), Some(1), Some(len - 2), Some(len - 1)] } else { vec![None, None, None, None] }
            } else if len >= 3 {
                vec![Some(0), Some(1), Some(2), None]
            } else {
                vec![None; 4]
            };
            for (name, at) in names.iter().zip(pick) {
                if let (false, Some(at)) = (name.is_empty(), at) {
                    out.push((format!("{name}_{side}"), chain[at]));
                }
            }
        }
    };
    sided(arms, p(chest), ["clavicle", "arm_upper", "arm_lower", "hand"], &mut out);
    let legs: Vec<usize> = by_size[1..3].to_vec();
    sided(legs, p(pelvis), ["leg_upper", "leg_lower", "ankle", ""], &mut out);
    out
}
