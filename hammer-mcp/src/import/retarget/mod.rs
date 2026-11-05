//! Player retarget (I6): an arbitrary-rig humanoid glTF becomes a model on the CS2 player
//! skeleton (`worldmodel.vnmskel`), posed by the game's own AnimGraph2.
//!
//! * [`skeleton`]: the vnmskel (75 bones, reference poses) read from the game's paks;
//! * [`mapping`]: source joint -> CS2 bone (explicit `boneMap` > name tables > structural);
//! * [`bake`]: the axes, scale and per-bone re-posing math, and the SMD writer.
//!
//! The result is a rigged SMD whose skeleton is the vnmskel verbatim and whose mesh sits in
//! vnmskel model space, plus a `.vmdl` with `NmSkeletonReference` + `DefaultAnimGraph2` (the
//! I0 spike's `spike_agent.vmdl` layout). Source animations are not used: the graph animates it.
pub mod agent;
pub mod bake;
pub mod mapping;
pub mod skeleton;

use super::gltf_read::Scene;
use super::ImportError;
use mapping::{How, MapError, RigHint};
use serde_json::{json, Value};
use skeleton::NmSkeleton;

/// What `kind:"player"` needs besides the file.
#[derive(Debug, Clone)]
pub struct PlayerOptions {
    pub skeleton: NmSkeleton,
    /// `(source joint, CS2 bone)`.
    pub bone_map: Vec<(String, String)>,
    pub rig: RigHint,
    /// The reference agent's attachments and hitboxes (`agent`), with the model path it was read
    /// from; `None` = an empty `AttachmentList` / `HitboxSet` (tests, or no agent wanted).
    pub markup: Option<(String, agent::Markup)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MappedBone {
    pub source: String,
    pub cs2: String,
    pub how: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnmappedBone {
    pub bone: String,
    /// The CS2 bone its weights went to.
    pub weights_moved_to: String,
}

/// The result's `skeleton` / `retarget` blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub bones: usize,
    pub mapped: Vec<MappedBone>,
    /// Unmapped source joints that carried weight.
    pub unmapped: Vec<UnmappedBone>,
    /// Unmapped joints with no weight at all.
    pub unweighted: usize,
    pub confidence: f64,
    /// The name table used (`none` = explicit/structural only).
    pub rig: String,
    pub scale: f64,
    pub scale_from_arg: bool,
    /// `[up, left, forward]` of the CS2 skeleton and of the source, each as a vector.
    pub cs2_axes: [[f64; 3]; 3],
    pub src_axes: [[f64; 3]; 3],
}

impl Report {
    pub fn json(&self) -> (Value, Value) {
        let r4 = |v: [f64; 3]| json!(v.map(|x| (x * 10000.0).round() / 10000.0));
        let skeleton = json!({
            "bones": self.bones,
            "mapped": self.mapped.iter().map(|m| json!({"source": m.source, "cs2": m.cs2, "how": m.how})).collect::<Vec<_>>(),
            "unmapped": self.unmapped.iter().map(|u| json!({"bone": u.bone, "weightsMovedTo": u.weights_moved_to})).collect::<Vec<_>>(),
            "unmappedUnweighted": self.unweighted,
            "confidence": self.confidence,
        });
        let retarget = json!({
            "scale": (self.scale * 1e6).round() / 1e6,
            "scaleFrom": if self.scale_from_arg { "scale argument" } else { "pelvis-to-head fit" },
            "rig": self.rig,
            "facing": {
                "space": "vnmskel model space (+X front, +Y left, +Z up)",
                "cs2": {"up": r4(self.cs2_axes[0]), "left": r4(self.cs2_axes[1]), "forward": r4(self.cs2_axes[2])},
                "source": {"up": r4(self.src_axes[0]), "left": r4(self.src_axes[1]), "forward": r4(self.src_axes[2])},
            },
        });
        (skeleton, retarget)
    }
}

pub struct Output {
    /// The agent markup placed on the import (`None` without [`PlayerOptions::markup`]).
    pub markup: Option<(String, agent::Fitted)>,
    pub reference: String,
    pub tris: usize,
    pub verts: usize,
    pub report: Report,
    pub warnings: Vec<String>,
}

/// Map, bake and write the player SMD of a skinned `scene`.
pub fn retarget(scene: &Scene, names: &[String], opts: &PlayerOptions, scale: Option<f64>) -> Result<Output, ImportError> {
    let rig = scene.rig.as_ref().ok_or(ImportError::NoSkin)?;
    let skel = &opts.skeleton;
    let mapping = mapping::build(rig, skel, &opts.bone_map, opts.rig).map_err(|e| match e {
        MapError::Invalid { detail, near } => ImportError::BoneMap { detail, near },
        MapError::Incomplete { unmapped, required } => ImportError::BoneMapIncomplete { unmapped, required },
    })?;
    let baked = bake::bake(rig, skel, &mapping, scale).map_err(ImportError::Invalid)?;
    let (reference, tris, verts) = bake::write_smd(scene, names, skel, &baked);

    let mapped: Vec<MappedBone> = mapping
        .pairs
        .iter()
        .map(|p| MappedBone { source: rig.joints[p.src].name.clone(), cs2: skel.names[p.cs2].clone(), how: p.how.text() })
        .collect();
    let mut weighted = vec![false; rig.joints.len()];
    for mesh in &scene.meshes {
        for links in &mesh.links {
            for &(j, w) in links {
                if w > 0.0 {
                    weighted[j as usize] = true;
                }
            }
        }
    }
    let is_mapped: Vec<bool> = (0..rig.joints.len()).map(|j| mapping.pairs.iter().any(|p| p.src == j)).collect();
    let mut unmapped = Vec::new();
    let mut unweighted = 0;
    for (j, joint) in rig.joints.iter().enumerate() {
        if joint.synthetic || is_mapped[j] {
            continue;
        }
        if weighted[j] {
            unmapped.push(UnmappedBone { bone: joint.name.clone(), weights_moved_to: skel.names[baked.assign[j]].clone() });
        } else {
            unweighted += 1;
        }
    }
    // confidence: required bones found by name (explicit / table) count 1, structural 0.7
    let required = mapping::required_names();
    let mut score = 0.0;
    for name in &required {
        let hit = |n: &str| mapping.pairs.iter().find(|p| skel.names[p.cs2] == n).map(|p| &p.how);
        let how = if name == "spine_*" { (0..4).find_map(|i| hit(&format!("spine_{i}"))) } else { hit(name) };
        score += match how {
            Some(How::Structural) => 0.7,
            Some(_) => 1.0,
            None => 0.0,
        };
    }
    let confidence = (score / required.len() as f64 * 100.0).round() / 100.0;
    let mut warnings = baked.warnings.clone();
    if !unmapped.is_empty() {
        warnings.push(format!(
            "{} source joint(s) with weights are not mapped; their weights moved to the nearest mapped ancestor (result skeleton.unmapped)",
            unmapped.len()
        ));
    }
    let report = Report {
        bones: skel.names.len(),
        mapped,
        unmapped,
        unweighted,
        confidence,
        rig: mapping.rig.unwrap_or("none").to_string(),
        scale: baked.scale,
        scale_from_arg: baked.scale_from_arg,
        cs2_axes: [baked.cs2_axes.up, baked.cs2_axes.left, baked.cs2_axes.forward],
        src_axes: [baked.src_axes.up, baked.src_axes.left, baked.src_axes.forward],
    };
    let markup = opts.markup.as_ref().map(|(path, m)| (path.clone(), agent::fit(m, skel)));
    if let Some((path, fitted)) = &markup {
        warnings.extend(fitted.dropped.iter().map(|d| format!("{path}: {d} (dropped)")));
    }
    Ok(Output { markup, reference, tris, verts, report, warnings })
}
