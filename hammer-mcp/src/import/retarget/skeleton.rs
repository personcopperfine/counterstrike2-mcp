//! The CS2 player skeleton: `animation/skeletons/characters/worldmodel.vnmskel_c` (SPIKE-I0 e).
//!
//! The compiled file is a resource container whose `DATA` block holds `m_ID`, `m_boneIDs` (75
//! names), `m_parentIndices` and the two reference poses `m_parentSpaceReferencePose` /
//! `m_modelSpaceReferencePose`, each bone `[tx, ty, tz, scale, qx, qy, qz, qw]` (inches, unit
//! quaternion, CS2 axes: +X front, +Y left, +Z up for the model-space pose).
use crate::resource::kv3bin::{self, Kv3Value};
use crate::resource::{Resource, ResourceError};

/// Where the skeleton lives in the game's paks (the `.vmdl` references it without `_c`).
pub const SKELETON_PATH: &str = "animation/skeletons/characters/worldmodel.vnmskel";
/// The compiled path to read.
pub const SKELETON_COMPILED: &str = "animation/skeletons/characters/worldmodel.vnmskel_c";
/// The AnimGraph2 the agents use (`m_animGraph2Refs[""]` of `tm_phoenix.vmdl_c`).
pub const GRAPH_PATH: &str = "animation/graphs/worldmodel/worldmodel.vnmgraph";

/// One bone pose: translation (inches), scale, quaternion `[x, y, z, w]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub t: [f64; 3],
    pub s: f64,
    pub q: [f64; 4],
}

#[derive(Debug, Clone, PartialEq)]
pub struct NmSkeleton {
    pub id: String,
    pub names: Vec<String>,
    /// Always smaller than the bone's own index (checked on load).
    pub parents: Vec<Option<usize>>,
    /// Parent-space reference pose (written verbatim into the SMD).
    pub local: Vec<Pose>,
    /// Model-space reference pose (where the mesh is placed).
    pub model: Vec<Pose>,
}

#[derive(Debug, thiserror::Error)]
pub enum SkeletonError {
    #[error("the skeleton resource could not be read: {0}")]
    Resource(#[from] ResourceError),
    #[error("the skeleton file is malformed: {0}")]
    Malformed(String),
}

fn bad(what: impl Into<String>) -> SkeletonError {
    SkeletonError::Malformed(what.into())
}

fn poses(root: &Kv3Value, key: &str, count: usize) -> Result<Vec<Pose>, SkeletonError> {
    let list = root.get(key).and_then(Kv3Value::as_array).ok_or_else(|| bad(format!("{key} is missing")))?;
    if list.len() != count {
        return Err(bad(format!("{key} has {} entries for {count} bones", list.len())));
    }
    list.iter()
        .enumerate()
        .map(|(i, p)| {
            let v: Vec<f64> = p
                .as_array()
                .ok_or_else(|| bad(format!("{key}[{i}] is not an array")))?
                .iter()
                .map(|x| x.as_f64().filter(|x| x.is_finite()))
                .collect::<Option<_>>()
                .ok_or_else(|| bad(format!("{key}[{i}] holds a non-number")))?;
            if v.len() != 8 {
                return Err(bad(format!("{key}[{i}] has {} numbers, expected 8", v.len())));
            }
            let len = v[4..8].iter().map(|x| x * x).sum::<f64>().sqrt();
            if len < 1e-6 {
                return Err(bad(format!("{key}[{i}] has a zero quaternion")));
            }
            Ok(Pose { t: [v[0], v[1], v[2]], s: v[3], q: [v[4] / len, v[5] / len, v[6] / len, v[7] / len] })
        })
        .collect()
}

/// Decode a `.vnmskel_c`.
pub fn from_resource_bytes(bytes: &[u8]) -> Result<NmSkeleton, SkeletonError> {
    let resource = Resource::parse(bytes)?;
    let block = resource.blocks_of(b"DATA").next().ok_or_else(|| {
        SkeletonError::Resource(ResourceError::MissingBlock { kind: "DATA", blocks: resource.block_types().join(", ") })
    })?;
    let document = kv3bin::decode(resource.block_bytes(block)).map_err(ResourceError::from)?;
    from_value(&document.root)
}

/// Build from the decoded `DATA` root (tests use this and the encoder).
pub fn from_value(root: &Kv3Value) -> Result<NmSkeleton, SkeletonError> {
    let names: Vec<String> = root
        .get("m_boneIDs")
        .and_then(Kv3Value::as_array)
        .ok_or_else(|| bad("m_boneIDs is missing"))?
        .iter()
        .map(|n| n.as_str().map(str::to_string))
        .collect::<Option<_>>()
        .ok_or_else(|| bad("m_boneIDs holds a non-string"))?;
    if names.is_empty() {
        return Err(bad("m_boneIDs is empty"));
    }
    let parents: Vec<Option<usize>> = root
        .get("m_parentIndices")
        .and_then(Kv3Value::as_array)
        .ok_or_else(|| bad("m_parentIndices is missing"))?
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let v = p.as_f64().filter(|v| v.is_finite() && v.fract() == 0.0).ok_or_else(|| bad(format!("m_parentIndices[{i}] is not an integer")))?;
            if v < 0.0 {
                Ok(None)
            } else if (v as usize) < i {
                Ok(Some(v as usize))
            } else {
                Err(bad(format!("bone {i} has parent {v}: parents must come first")))
            }
        })
        .collect::<Result<_, _>>()?;
    if parents.len() != names.len() {
        return Err(bad(format!("{} parent indices for {} bones", parents.len(), names.len())));
    }
    let local = poses(root, "m_parentSpaceReferencePose", names.len())?;
    let model = poses(root, "m_modelSpaceReferencePose", names.len())?;
    Ok(NmSkeleton { id: root.get_str("m_ID").unwrap_or("").to_string(), names, parents, local, model })
}

impl NmSkeleton {
    /// Bone index by exact name.
    pub fn index(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    /// Bone index by name, ignoring ASCII case (the compiler lowercases bone names).
    pub fn index_ci(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n.eq_ignore_ascii_case(name))
    }

    /// Bones that never carry weights: the weapon rig, attachments, the camera and the root.
    pub fn is_weightless(name: &str) -> bool {
        name == "root_motion" || name == "camera" || name.starts_with("wpn") || name.starts_with("attach")
    }

    /// Bones that may carry weights (every bone except [`NmSkeleton::is_weightless`]).
    pub fn weightable(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.names.len()).filter(|i| !Self::is_weightless(&self.names[*i]))
    }

    /// Direct children of `bone`.
    pub fn children(&self, bone: usize) -> Vec<usize> {
        (0..self.names.len()).filter(|i| self.parents[*i] == Some(bone)).collect()
    }
}
