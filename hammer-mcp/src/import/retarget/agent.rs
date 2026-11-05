//! Weapon attachments and hitboxes for a player import, copied from a shipped agent model.
//!
//! Agents ship only compiled `agents/models/**/*.vmdl_c`. Every `MDAT` block carries the model's
//! `m_attachments` (`[{key, value}]`, each value `m_name`, `m_nInfluences`, `m_influenceNames`,
//! `m_vInfluenceOffsets`, `m_vInfluenceRotations` `[x,y,z,w]`, `m_influenceWeights`,
//! `m_bInfluenceRootTransform`, `m_bIgnoreRotation`) and `m_hitboxsets` (`[{key, value:{m_name,
//! m_HitBoxes:[{m_name, m_sBoneName, m_nShapeType, m_flShapeRadius, m_vMinBounds, m_vMaxBounds,
//! m_sSurfaceProperty, m_nGroupId, m_bTranslationOnly}]}]}`); `DATA.m_modelSkeleton` holds the
//! agent's own bones (`m_boneName`, `m_nParent`, `m_bonePosParent`, `m_boneRotParent`).
//!
//! Measured on build 2000930 (all 92 `agents/models/**` files): the hitbox set (`cstrike`, 19
//! capsules) is identical in all 80 agents; the attachment names come in two sets (CT 17 with
//! `defusekit`/`primary_smg`, T 15) and only the holster offsets differ per agent; `weapon` (on
//! `wpn`), `weapon_hand_r/l`, `weapon_center` (root transform) and `clip_limit` are the same
//! everywhere. Every agent bone shared with `worldmodel.vnmskel` has the vnmskel's parent-space
//! rest pose (within 1e-3) except the root's children (`pelvis`, `wpnPivot`) and `wpn`: the agent's
//! `root_motion` rest is turned 120 degrees about (1,1,1) and those children compensate (the world
//! frame of `pelvis` is the same). Offsets are bone-relative, so they mean the same on the import;
//! a fold is never allowed to end on the root bone (its frame differs).
//!
//! [`fit`] maps the agent's markup onto the import's skeleton: a bone the vnmskel has keeps its
//! offset; a bone it lacks (the agent's `jiggle_primary`, parent `spine_3`) is folded into its
//! nearest vnmskel ancestor by composing the agent's rest transforms (rest pose: a jiggle bone's
//! motion is lost); markup with no such ancestor (or only the root) is dropped and reported. Bone names are written
//! lowercase: the SMD importer lowercases the import's bones (I6 live).
use super::skeleton::NmSkeleton;
use crate::resource::kv3bin::{self, Kv3Value};
use crate::resource::{Resource, ResourceError};
use serde_json::{json, Value};

/// The default reference: a CT agent, so the superset of attachment names (17, with `defusekit`).
pub const DEFAULT_AGENT: &str = "agents/models/ctm_sas/ctm_sas.vmdl";

#[derive(Debug, Clone, PartialEq)]
pub struct Influence {
    /// Agent bone name; empty with `root_transform`.
    pub bone: String,
    pub offset: [f64; 3],
    /// `[x, y, z, w]`.
    pub rotation: [f64; 4],
    pub weight: f64,
    pub root_transform: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    pub name: String,
    pub ignore_rotation: bool,
    pub influences: Vec<Influence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Box,
    Sphere,
    Capsule,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hitbox {
    pub name: String,
    pub bone: String,
    pub shape: Shape,
    pub radius: f64,
    pub min: [f64; 3],
    pub max: [f64; 3],
    pub surface: String,
    pub group: i64,
    pub translation_only: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HitboxSet {
    pub name: String,
    pub hitboxes: Vec<Hitbox>,
}

/// One agent bone's parent-space rest transform.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentBone {
    pub name: String,
    pub parent: Option<usize>,
    pub pos: [f64; 3],
    pub rot: [f64; 4],
}

/// What the reference agent carries.
#[derive(Debug, Clone, PartialEq)]
pub struct Markup {
    pub attachments: Vec<Attachment>,
    pub hitbox_sets: Vec<HitboxSet>,
    pub bones: Vec<AgentBone>,
}

#[derive(Debug, thiserror::Error)]
pub enum MarkupError {
    #[error("the agent model could not be read: {0}")]
    Resource(#[from] ResourceError),
    #[error("the agent model is malformed: {0}")]
    Malformed(String),
}

fn bad(what: impl Into<String>) -> MarkupError {
    MarkupError::Malformed(what.into())
}

fn numbers<const N: usize>(v: Option<&Kv3Value>, what: &str) -> Result<[f64; N], MarkupError> {
    let list = v.map(Kv3Value::resolve).and_then(Kv3Value::as_array).ok_or_else(|| bad(format!("{what} is missing")))?;
    if list.len() != N {
        return Err(bad(format!("{what} has {} numbers, expected {N}", list.len())));
    }
    let mut out = [0.0; N];
    for (slot, x) in out.iter_mut().zip(list) {
        *slot = x.as_f64().filter(|x| x.is_finite()).ok_or_else(|| bad(format!("{what} holds a non-number")))?;
    }
    Ok(out)
}

fn text(v: &Kv3Value, key: &str) -> String {
    v.get_str(key).unwrap_or("").to_string()
}

fn flag(v: Option<&Kv3Value>) -> bool {
    v.and_then(Kv3Value::as_bool).unwrap_or(false)
}

/// `[{key, value}]` -> the values (a missing list is empty).
fn keyed<'a>(v: &'a Kv3Value, key: &str) -> Vec<&'a Kv3Value> {
    v.get(key)
        .and_then(Kv3Value::as_array)
        .map(|items| items.iter().map(|e| e.get("value").unwrap_or(e)).collect())
        .unwrap_or_default()
}

fn attachment(v: &Kv3Value) -> Result<Attachment, MarkupError> {
    let name = text(v, "m_name");
    let count = v.get("m_nInfluences").and_then(Kv3Value::as_f64).unwrap_or(1.0).max(0.0) as usize;
    let at = |key: &str, i: usize| v.get(key).and_then(Kv3Value::as_array).and_then(|a| a.get(i));
    let mut influences = Vec::with_capacity(count);
    for i in 0..count {
        let what = format!("attachment {name:?} influence {i}");
        influences.push(Influence {
            bone: at("m_influenceNames", i).and_then(Kv3Value::as_str).unwrap_or("").to_string(),
            offset: numbers::<3>(at("m_vInfluenceOffsets", i), &format!("{what} offset"))?,
            rotation: numbers::<4>(at("m_vInfluenceRotations", i), &format!("{what} rotation"))?,
            weight: at("m_influenceWeights", i).and_then(Kv3Value::as_f64).unwrap_or(1.0),
            root_transform: flag(at("m_bInfluenceRootTransform", i)),
        });
    }
    Ok(Attachment { name, ignore_rotation: flag(v.get("m_bIgnoreRotation")), influences })
}

fn hitbox(v: &Kv3Value) -> Result<Hitbox, MarkupError> {
    let name = text(v, "m_name");
    let shape = match v.get("m_nShapeType").and_then(Kv3Value::as_f64).filter(|s| s.fract() == 0.0).map(|s| s as i64) {
        Some(0) => Shape::Box,
        Some(1) => Shape::Sphere,
        Some(2) => Shape::Capsule,
        other => return Err(bad(format!("hitbox {name:?} has shape type {other:?}"))),
    };
    Ok(Hitbox {
        bone: text(v, "m_sBoneName"),
        shape,
        radius: v.get("m_flShapeRadius").and_then(Kv3Value::as_f64).unwrap_or(0.0),
        min: numbers::<3>(v.get("m_vMinBounds"), &format!("hitbox {name:?} m_vMinBounds"))?,
        max: numbers::<3>(v.get("m_vMaxBounds"), &format!("hitbox {name:?} m_vMaxBounds"))?,
        surface: text(v, "m_sSurfaceProperty"),
        group: v.get("m_nGroupId").and_then(Kv3Value::as_f64).map_or(0, |g| g as i64),
        translation_only: flag(v.get("m_bTranslationOnly")),
        name,
    })
}

fn agent_bones(data: &Kv3Value) -> Result<Vec<AgentBone>, MarkupError> {
    let Some(skel) = data.get("m_modelSkeleton") else {
        return Ok(Vec::new());
    };
    let list = |key: &str| skel.get(key).and_then(Kv3Value::as_array).unwrap_or(&[]);
    let (names, parents, pos, rot) = (list("m_boneName"), list("m_nParent"), list("m_bonePosParent"), list("m_boneRotParent"));
    if parents.len() != names.len() || pos.len() != names.len() || rot.len() != names.len() {
        return Err(bad("DATA.m_modelSkeleton lists differ in length"));
    }
    (0..names.len())
        .map(|i| {
            let parent = parents[i].as_f64().ok_or_else(|| bad(format!("m_nParent[{i}] is not a number")))?;
            Ok(AgentBone {
                name: names[i].as_str().ok_or_else(|| bad(format!("m_boneName[{i}] is not a string")))?.to_string(),
                parent: (parent >= 0.0 && (parent as usize) < i).then_some(parent as usize),
                pos: numbers::<3>(Some(&pos[i]), &format!("m_bonePosParent[{i}]"))?,
                rot: numbers::<4>(Some(&rot[i]), &format!("m_boneRotParent[{i}]"))?,
            })
        })
        .collect()
}

/// Read the markup of a compiled agent model: the first `MDAT` block that decodes (an agent has one
/// per LOD, all equal) and `DATA.m_modelSkeleton`.
pub fn from_resource_bytes(bytes: &[u8]) -> Result<Markup, MarkupError> {
    let resource = Resource::parse(bytes)?;
    let mdat = resource
        .blocks_of(b"MDAT")
        .find_map(|b| kv3bin::decode(resource.block_bytes(b)).ok())
        .map(|d| d.root)
        .ok_or_else(|| bad("no decodable MDAT block (is this a model?)"))?;
    // DATA carries the bones markup on agent-only bones is folded through
    let bones = match resource.blocks_of(b"DATA").next() {
        Some(block) => {
            let data = kv3bin::decode(resource.block_bytes(block)).map_err(|e| bad(format!("DATA: {e}")))?;
            agent_bones(&data.root)?
        }
        None => Vec::new(),
    };
    let attachments = keyed(&mdat, "m_attachments").into_iter().map(attachment).collect::<Result<_, _>>()?;
    let hitbox_sets = keyed(&mdat, "m_hitboxsets")
        .into_iter()
        .map(|set| {
            let hitboxes = set
                .get("m_HitBoxes")
                .and_then(Kv3Value::as_array)
                .unwrap_or(&[])
                .iter()
                .map(hitbox)
                .collect::<Result<_, _>>()?;
            Ok(HitboxSet { name: text(set, "m_name"), hitboxes })
        })
        .collect::<Result<_, MarkupError>>()?;
    Ok(Markup { attachments, hitbox_sets, bones })
}

fn qmul(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    let [ax, ay, az, aw] = a;
    let [bx, by, bz, bw] = b;
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

fn qrot(q: [f64; 4], v: [f64; 3]) -> [f64; 3] {
    let p = qmul(qmul(q, [v[0], v[1], v[2], 0.0]), [-q[0], -q[1], -q[2], q[3]]);
    [p[0], p[1], p[2]]
}

/// A quaternion `[x,y,z,w]` as ModelDoc `relative_angles` `[pitch, yaw, roll]` degrees (Source
/// QAngle; the formula of ValveResourceFormat's `EntityTransformHelper.ToEulerAngles`, MIT).
pub fn quat_to_angles(q: [f64; 4]) -> [f64; 3] {
    let [x, y, z, w] = q;
    let fx = 1.0 - 2.0 * (y * y + z * z);
    let fy = 2.0 * (x * y + w * z);
    let fz = 2.0 * (x * z - w * y);
    let xy = (fx * fx + fy * fy).sqrt();
    let pitch = (-fz).atan2(xy);
    let (yaw, roll) = if xy > 0.001 {
        let lz = 2.0 * (y * z + w * x);
        let uz = 1.0 - 2.0 * (x * x + y * y);
        (fy.atan2(fx), lz.atan2(uz))
    } else {
        let lx = 2.0 * (x * y - w * z);
        let ly = 1.0 - 2.0 * (x * x + z * z);
        ((-lx).atan2(ly), 0.0)
    };
    [pitch.to_degrees(), yaw.to_degrees(), roll.to_degrees()]
}

/// One influence placed on an import bone.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    /// Lowercase import bone; empty = model origin (root transform).
    pub bone: String,
    pub offset: [f64; 3],
    pub rotation: [f64; 4],
    pub weight: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FitAttachment {
    pub name: String,
    pub ignore_rotation: bool,
    pub influences: Vec<Placed>,
}

/// `(attachment, agent bone, import bone)`: an influence folded into an ancestor.
pub type Moved = (String, String, String);

/// The markup on the import's skeleton.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Fitted {
    pub attachments: Vec<FitAttachment>,
    pub hitbox_sets: Vec<HitboxSet>,
    pub moved: Vec<Moved>,
    /// Human-readable lines for what could not be placed.
    pub dropped: Vec<String>,
}

/// Place `markup` on `skel` (see the module docs).
pub fn fit(markup: &Markup, skel: &NmSkeleton) -> Fitted {
    let ours = |name: &str| skel.names.iter().find(|n| n.eq_ignore_ascii_case(name)).map(|n| n.to_ascii_lowercase());
    let mut out = Fitted::default();
    'attachments: for a in &markup.attachments {
        if a.influences.is_empty() {
            out.dropped.push(format!("attachment {:?}: no influences", a.name));
            continue;
        }
        let mut influences = Vec::with_capacity(a.influences.len());
        let mut moved = Vec::new();
        for inf in &a.influences {
            if inf.root_transform || inf.bone.is_empty() {
                influences.push(Placed { bone: String::new(), offset: inf.offset, rotation: inf.rotation, weight: inf.weight });
                continue;
            }
            if let Some(bone) = ours(&inf.bone) {
                influences.push(Placed { bone, offset: inf.offset, rotation: inf.rotation, weight: inf.weight });
                continue;
            }
            // fold into the nearest ancestor the import has, through the agent's rest pose
            let (mut offset, mut rotation) = (inf.offset, inf.rotation);
            let mut at = markup.bones.iter().position(|b| b.name.eq_ignore_ascii_case(&inf.bone));
            let mut target = None;
            while let Some(i) = at {
                let b = &markup.bones[i];
                if let Some(bone) = ours(&b.name) {
                    target = Some(bone);
                    break;
                }
                let r = qrot(b.rot, offset);
                offset = [r[0] + b.pos[0], r[1] + b.pos[1], r[2] + b.pos[2]];
                rotation = qmul(b.rot, rotation);
                at = b.parent;
            }
            // the agent's root frame is turned against the vnmskel's (agents: 120 degrees about
            // (1,1,1)), so a fold that ends on a root bone would land in the wrong frame
            let at_root = target.as_deref().and_then(|t| skel.index_ci(t)).is_some_and(|i| skel.parents[i].is_none());
            match target {
                Some(bone) if !at_root => {
                    moved.push((a.name.clone(), inf.bone.clone(), bone.clone()));
                    influences.push(Placed { bone, offset, rotation, weight: inf.weight });
                }
                _ => {
                    out.dropped.push(format!(
                        "attachment {:?}: bone {:?} is neither in the CS2 skeleton nor below one of its non-root bones in the agent",
                        a.name, inf.bone
                    ));
                    continue 'attachments;
                }
            }
        }
        out.moved.extend(moved);
        out.attachments.push(FitAttachment { name: a.name.clone(), ignore_rotation: a.ignore_rotation, influences });
    }
    for set in &markup.hitbox_sets {
        let mut hitboxes = Vec::with_capacity(set.hitboxes.len());
        for h in &set.hitboxes {
            match ours(&h.bone) {
                Some(bone) => hitboxes.push(Hitbox { bone, ..h.clone() }),
                None => out.dropped.push(format!("hitbox {:?} of set {:?}: bone {:?} is not in the CS2 skeleton", h.name, set.name, h.bone)),
            }
        }
        out.hitbox_sets.push(HitboxSet { name: set.name.clone(), hitboxes });
    }
    out
}

fn r4(v: f64) -> f64 {
    let r = (v * 10000.0).round() / 10000.0;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

fn r4s<const N: usize>(v: [f64; N]) -> Value {
    json!(v.map(r4).to_vec())
}

impl Fitted {
    /// The `.vmdl` template's `attachments` parameter (ModelDoc `Attachment` keys; the last
    /// influence is the node itself, earlier ones `AttachmentInfluence` children, as
    /// ValveResourceFormat's `ModelExtract.MeshLists` writes them).
    pub fn attachments_param(&self) -> Value {
        let node = |p: &Placed| {
            json!({
                "parent_bone": p.bone,
                "relative_origin": r4s(p.offset),
                "relative_angles": r4s(quat_to_angles(p.rotation)),
                "weight": r4(p.weight),
            })
        };
        Value::Array(
            self.attachments
                .iter()
                .filter_map(|a| {
                    let (main, rest) = a.influences.split_last()?;
                    let mut entry = node(main);
                    entry["name"] = json!(a.name);
                    entry["ignore_rotation"] = json!(a.ignore_rotation);
                    if !rest.is_empty() {
                        entry["influences"] = Value::Array(rest.iter().map(node).collect());
                    }
                    Some(entry)
                })
                .collect(),
        )
    }

    /// The `.vmdl` template's `hitboxSets` parameter (keys of ValveResourceFormat's
    /// `ModelExtract.Hitboxes`).
    pub fn hitbox_sets_param(&self) -> Value {
        Value::Array(
            self.hitbox_sets
                .iter()
                .map(|set| {
                    let hitboxes: Vec<Value> = set
                        .hitboxes
                        .iter()
                        .map(|h| {
                            let mut node = match h.shape {
                                Shape::Capsule => json!({"shape": "capsule", "radius": r4(h.radius), "point0": r4s(h.min), "point1": r4s(h.max)}),
                                Shape::Sphere => json!({"shape": "sphere", "radius": r4(h.radius), "center": r4s(h.min)}),
                                Shape::Box => json!({"shape": "box", "hitbox_mins": r4s(h.min), "hitbox_maxs": r4s(h.max)}),
                            };
                            node["name"] = json!(h.name);
                            node["parent_bone"] = json!(h.bone);
                            node["surface_property"] = json!(h.surface);
                            node["translation_only"] = json!(h.translation_only);
                            node["group_id"] = json!(h.group);
                            node
                        })
                        .collect();
                    json!({"name": set.name, "hitboxes": hitboxes})
                })
                .collect(),
        )
    }
}
