//! CMapMesh extraction from the SAVED `.vmap` (POC `vmap_reader.py` `_mesh_from_element` /
//! `MeshRecord`, ported 1:1). Built on the DMX tree [`super::dmx`] already decodes; the map-view
//! layer ([`super`]) stores the resulting records on every [`super::Document`].
//!
//! Ported from the Python oracle (`../idabins/worknotes/mcpserver/vmap_reader.py`):
//!
//! * a mesh is an element of class `CMapMesh` whose `meshData` carries a `position` stream
//!   (`name == "position:0"` **or** `standardAttributeName == "position"`); anything else is
//!   skipped, exactly like the oracle returning `None`;
//! * vertices are the position stream data indexed by `vertexDataIndices` (the oracle's
//!   `pos[vdi[i] if i < len(vdi) else i]` rule), scaled and moved to WORLD space through
//!   `angle_matrix([pitch, yaw, roll]) * local + origin`;
//! * faces are edge walks: one loop per `faceEdgeIndices` entry, following `edgeNextIndices`
//!   through `edgeVertexIndices` until the start edge is reached (guard 4096, as the oracle);
//! * per-face materials come from the `faceData` `materialindex` stream indexed by
//!   `faceDataIndices`; an index beyond the material list falls back to `materials[0]` (or `""`
//!   when the mesh lists no materials), as in the oracle.
//!
//! Deviations forced by Rust (the oracle would raise `IndexError`): an out-of-range index
//! anywhere in the walk (position, edge, next-edge, a negative edge-vertex index) makes the mesh
//! skip-able ([`mesh_from_element`] returns `None`) instead of crashing the server. A missing
//! `origin`/`angles`/`scales` attribute falls back to the identity (the oracle's `or (0,0,0)`),
//! and an absent index array (`vertexDataIndices`, `edgeVertexIndices`, `edgeNextIndices`,
//! `faceEdgeIndices`, `vertexEdgeIndices`) reads as the empty list, exactly like the oracle's
//! `list(md.get(...) or [])` — a file that omits `vertexDataIndices` (the identity mapping) is
//! still listed (review L6).
//!
//! JSON views round like the oracle's `summary`/`to_dict`: bounds to 3 decimals, vertices to 4;
//! `origin`/`angles`/`scales` are printed raw (f32 widened to f64, shortest round-trip form —
//! the same digits Python's `json.dumps` prints).
//!
//! Review-3 NEW-5 adds one thing the oracle does not have: each record also carries its
//! `children`-edge ancestor classes ([`children_parents`] / [`ancestor_chain`]), which the tool
//! layer's file route uses to refuse a mesh whose stored geometry it cannot prove is world-space
//! ([`MeshRecord::parent_problem`]). JSON views never print them.

use serde_json::{json, Value as Json};

use super::dmx::{DmxDocument, DmxElement, Value};

/// The container classes whose children are stored in world space, so the extraction's own
/// `origin/angles/scales` math is the whole story (review-3 NEW-5). A `CMapGroup` is a
/// grouping/organisational node and a `CMapEntity` a brush entity: their own transform is a pivot
/// the file also mirrors onto each child, not a transform composed with the child's geometry (see
/// [`MeshRecord::parent_problem`], which cites the evidence).
const CONTAINER_CLASSES: [&str; 2] = ["CMapGroup", "CMapEntity"];
/// The class a saved map's `children` chain ends at (the map's world root).
const WORLD_CLASS: &str = "CMapWorld";
/// The longest accepted ancestor walk: a deeper chain (a cycle, a malformed file) is refused.
const MAX_ANCESTORS: usize = 64;

/// Why the file route refuses a mesh whose place in the file's node tree is not one whose stored
/// geometry this crate can prove is the live world geometry (review-3 NEW-5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentProblem {
    /// No `children` chain reaches the map's world root: the mesh is orphaned, sits under a
    /// container that is not the world's descendant, or is only referenced outside the tree.
    Unrooted,
    /// The nearest ancestor whose class is not one this crate has proven (a deformer, an instance,
    /// a prefab, a selection set, anything unknown).
    Unsupported(String),
}

/// The `children`-edge tree parents of every element (`parents[child]`), in element order and
/// first-owner-wins — the order the oracle's `_link_parents` uses for its tree edges.
///
/// Only `children` arrays are followed. The oracle's other tree edges are *single-element*
/// attributes (`world`, `entity_properties`, `meshData`, …), which point out of a node into its
/// parts and never into a `CMapMesh` (a mesh is referenced by its container's `children` and by
/// nothing else), so an ancestor walk that needs only the mesh's ancestors is complete without
/// them. The one edge this drops is `CMapRootElement.world -> CMapWorld`, which is why the walk in
/// [`ancestor_chain`] ends at `CMapWorld` rather than at the root element.
fn children_parents(elements: &[DmxElement]) -> Vec<Option<usize>> {
    let mut parents: Vec<Option<usize>> = vec![None; elements.len()];
    for parent in elements {
        let Some(Value::Array(children)) = parent.get("children") else {
            continue;
        };
        for child in children {
            if let Value::Element(index) = child {
                if let Some(slot) = parents.get_mut(*index) {
                    if slot.is_none() {
                        *slot = Some(parent.index);
                    }
                }
            }
        }
    }
    parents
}

/// The classes of `index`'s `children`-edge ancestors, nearest first, up to the element the walk
/// stops at (an element with no parent). `None` when there is no parent at all (an orphan), when
/// the walk revisits an element (a cycle), or when it runs past [`MAX_ANCESTORS`].
fn ancestor_chain(
    elements: &[DmxElement],
    parents: &[Option<usize>],
    index: usize,
) -> Option<Vec<String>> {
    let mut chain = Vec::new();
    let mut seen = vec![index];
    let mut next = parents.get(index).copied().flatten();
    while let Some(current) = next {
        if chain.len() >= MAX_ANCESTORS || seen.contains(&current) {
            return None;
        }
        seen.push(current);
        chain.push(elements.get(current)?.class_name.clone());
        next = parents.get(current).copied().flatten();
    }
    (!chain.is_empty()).then_some(chain)
}

/// One CMapMesh (POC `MeshRecord`): WORLD-space vertices, faces as vertex-index loops
/// (counter-clockwise seen from the visible side — the order `mesh.create`/`setGeometry`
/// expect) and per-face materials.
#[derive(Debug, Clone, PartialEq)]
pub struct MeshRecord {
    /// The node id (`nodeID`), the id `list_meshes`/`get_mesh` address.
    pub id: Option<i64>,
    pub origin: Option<[f64; 3]>,
    pub angles: Option<[f64; 3]>,
    pub scales: Option<[f64; 3]>,
    /// World-space `[x, y, z]` vertices.
    pub vertices: Vec<[f64; 3]>,
    /// Faces as vertex-index loops into [`MeshRecord::vertices`].
    pub faces: Vec<Vec<usize>>,
    /// Material path per face (same length as [`MeshRecord::faces`]).
    pub face_materials: Vec<String>,
    /// The mesh's distinct materials in file order (`meshData.materials`).
    pub materials: Vec<String>,
    /// The classes of the mesh element's `children`-edge ancestors, nearest first, up to the map's
    /// world root ([`ancestor_chain`]); `None` when the walk cannot reach one. Only
    /// [`MeshRecord::parent_problem`] reads it: JSON views do not print it.
    pub ancestors: Option<Vec<String>>,
}

impl MeshRecord {
    /// `None` when the file route may use this mesh's geometry; `Some` names what the route cannot
    /// prove (review-3 NEW-5).
    ///
    /// The extraction ([`mesh_from_element`]) applies only the mesh's *own* `origin`/`angles`/
    /// `scales`. Containers are deliberately not composed, and the saved files back that up: in
    /// every `.vmap` of the installed content tree (23 files, 4059 meshes) a mesh's ancestor chain
    /// is only `CMapGroup`/`CMapEntity` under `CMapWorld`, and the children's stored origins track
    /// their container's world origin rather than `(0,0,0)` — e.g. `toolscene_default.vmap` has a
    /// yaw-180 brush `CMapEntity` at `(32,80,64)` whose child mesh stores exactly `(32,80,64)`; if
    /// the child were stored parent-local it would be `(0,0,0)` and would land at a nonsense world
    /// point under that yaw. So a group's or entity's origin is a pivot, not a transform applied to
    /// the stored geometry. That is file-verified, not live-verified (one live check is owed: a
    /// `transform_mesh` on a mesh inside a translated `CMapGroup`).
    ///
    /// Anything else is *not* proven: a deformer, an instance, a prefab, a selection set, any
    /// unknown class, or a mesh outside the world tree, where the stored geometry could live in
    /// another space (and no file in the corpus has one, so nothing is refused in practice).
    pub fn parent_problem(&self) -> Option<ParentProblem> {
        let Some(chain) = self.ancestors.as_deref() else {
            return Some(ParentProblem::Unrooted);
        };
        // the walk must end at the map's world root ...
        let Some((terminal, inner)) = chain.split_last() else {
            return Some(ParentProblem::Unrooted);
        };
        if terminal != WORLD_CLASS {
            // ... otherwise the nearest ancestor that stopped it is what we cannot place
            return Some(ParentProblem::Unsupported(terminal.clone()));
        }
        // ... and every container between the mesh and the root must be one we have proven
        for class in inner {
            if !CONTAINER_CLASSES.contains(&class.as_str()) {
                return Some(ParentProblem::Unsupported(class.clone()));
            }
        }
        None
    }

    /// A world-space axis-aligned bounding box (oracle `MeshRecord.bounds`; an empty mesh
    /// bounds to `[0, 0]` on every axis, as in the oracle).
    pub fn bounds(&self) -> ([f64; 3], [f64; 3]) {
        if self.vertices.is_empty() {
            return ([0.0; 3], [0.0; 3]);
        }
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        for vertex in &self.vertices {
            for axis in 0..3 {
                min[axis] = min[axis].min(vertex[axis]);
                max[axis] = max[axis].max(vertex[axis]);
            }
        }
        (min, max)
    }

    /// `list_meshes` summary (oracle `MeshRecord.summary`): nodeID, origin/angles (raw f32
    /// values), counts, materials and the 3-decimal bounds.
    pub fn summary_json(&self) -> Json {
        let (min, max) = self.bounds();
        json!({
            "nodeID": self.id,
            "origin": vec3_json(self.origin),
            "angles": vec3_json(self.angles),
            "vertexCount": self.vertices.len(),
            "faceCount": self.faces.len(),
            "materials": self.materials,
            "bounds": {"min": rounded3(&min), "max": rounded3(&max)},
        })
    }

    /// `get_mesh` full record (oracle `MeshRecord.to_dict`): the summary keys plus vertices
    /// (rounded to 4 decimals), faces and per-face materials.
    pub fn to_json(&self) -> Json {
        let mut out = self.summary_json();
        let map = out.as_object_mut().expect("summary is an object");
        map.insert(
            "vertices".into(),
            Json::Array(
                self.vertices
                    .iter()
                    .map(|v| {
                        Json::Array(v.iter().map(|c| Json::from(round4(*c))).collect())
                    })
                    .collect(),
            ),
        );
        map.insert("faces".into(), json!(self.faces));
        map.insert("faceMaterials".into(), json!(self.face_materials));
        out
    }

    /// `get_mesh`'s `faceInfo` array (oracle `h_get_mesh`): per-face centre (3 decimals) and Newell
    /// normal (4 decimals, `null` for a degenerate face), from POC `geometry.face_normal` /
    /// `geometry.centroid`.
    pub fn face_info_json(&self) -> Json {
        face_info_json(&self.vertices, &self.faces)
    }
}

/// `faceInfo` for any vertex/face pair (the POC's `h_get_mesh` loop): per-face centre (3 decimals)
/// and Newell normal (4 decimals, `null` for a degenerate face).
pub fn face_info_json(vertices: &[[f64; 3]], faces: &[Vec<usize>]) -> Json {
    Json::Array(
        faces
            .iter()
            .map(|face| {
                let centre: [f64; 3] = {
                    let mut sum = [0.0f64; 3];
                    for &index in face {
                        if let Some(vertex) = vertices.get(index) {
                            for axis in 0..3 {
                                sum[axis] += vertex[axis];
                            }
                        }
                    }
                    let n = face.len().max(1) as f64;
                    [sum[0] / n, sum[1] / n, sum[2] / n]
                };
                let normal = face_normal(vertices, face).map(|normal| {
                    normal.iter().map(|c| Json::from(round4(*c))).collect::<Vec<Json>>()
                });
                json!({
                    "centre": rounded3(&centre),
                    "normal": normal,
                })
            })
            .collect(),
    )
}

/// Newell normal (POC `geometry.face_normal`), normalised; `None` for a zero-area face. The
/// normal points out of the visible side for the counter-clockwise winding Hammer expects.
fn face_normal(vertices: &[[f64; 3]], face: &[usize]) -> Option<[f64; 3]> {
    let mut n = [0.0f64; 3];
    for i in 0..face.len() {
        let a = vertices.get(face[i])?;
        let b = vertices.get(face[(i + 1) % face.len()])?;
        n[0] += (a[1] - b[1]) * (a[2] + b[2]);
        n[1] += (a[2] - b[2]) * (a[0] + b[0]);
        n[2] += (a[0] - b[0]) * (a[1] + b[1]);
    }
    let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if length < 1e-9 {
        return None;
    }
    Some([n[0] / length, n[1] / length, n[2] / length])
}

/// Every usable CMapMesh of a parsed file, in element order (oracle `_extract_meshes`).
pub fn mesh_view(document: &DmxDocument) -> Vec<MeshRecord> {
    let elements = &document.elements;
    let parents = children_parents(elements);
    elements
        .iter()
        .filter(|element| element.class_name == "CMapMesh")
        .filter_map(|mesh| mesh_from_element_with(mesh, elements, &parents))
        .collect()
}

/// Extract one mesh record (oracle `_mesh_from_element`); `None` when the element has no
/// `meshData`, no position stream, or a broken walk (see the module header).
pub fn mesh_from_element(element: &DmxElement, elements: &[DmxElement]) -> Option<MeshRecord> {
    mesh_from_element_with(element, elements, &children_parents(elements))
}

/// [`mesh_from_element`] with the element tree's `children` parent map already built
/// ([`mesh_view`] needs it for every mesh, so it pays for the pass once).
fn mesh_from_element_with(
    element: &DmxElement,
    elements: &[DmxElement],
    parents: &[Option<usize>],
) -> Option<MeshRecord> {
    let md = match element.get("meshData") {
        Some(Value::Element(index)) => elements.get(*index)?,
        _ => return None,
    };

    // the position stream of the vertex data
    let pos: Vec<[f64; 3]> = data_array(md, "vertexData", elements)
        .and_then(|array| stream_data(array, elements, "position:0", "position"))
        .and_then(pos_list)?;

    // the material-index stream of the face data (absent on some meshes: the oracle defaults to 0)
    let material_indices = data_array(md, "faceData", elements)
        .and_then(|array| stream_data(array, elements, "materialindex:0", "materialindex"))
        .and_then(|value| int_list(Some(value)));

    let vdi = int_list_or_empty(md.get("vertexDataIndices"))?;
    let ev = int_list_or_empty(md.get("edgeVertexIndices"))?;
    let nxt = int_list_or_empty(md.get("edgeNextIndices"))?;
    let fe = int_list_or_empty(md.get("faceEdgeIndices"))?;
    let fdi = int_list(md.get("faceDataIndices")).unwrap_or_default();
    let vertex_edges = int_list_or_empty(md.get("vertexEdgeIndices"))?;
    let mats: Vec<String> = match md.get("materials") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::Str(text) => Some(text.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };

    let scale = vec3_raw(element.get("scales")).unwrap_or([1.0; 3]);
    let origin = vec3_raw(element.get("origin")).unwrap_or([0.0; 3]);
    let angles = vec3_raw(element.get("angles")).unwrap_or([0.0; 3]);
    let matrix = angle_matrix(angles);

    // WORLD vertices: position data indexed by vertexDataIndices (the oracle's per-vertex fallback
    // to the vertex's own index when the index list is shorter)
    let mut vertices = Vec::with_capacity(vertex_edges.len());
    for i in 0..vertex_edges.len() {
        let index = vdi.get(i).copied().unwrap_or(i as i64);
        let local = *pos.get(usize::try_from(index).ok()?)?;
        vertices.push(apply_transform(local, scale, &matrix, origin));
    }

    // faces: single-loop edge walks (oracle guard: 4096 steps max)
    let mut faces = Vec::with_capacity(fe.len());
    let mut face_materials = Vec::with_capacity(fe.len());
    for (fi, &start) in fe.iter().enumerate() {
        let start = usize::try_from(start).ok()?;
        let mut loop_indices = Vec::new();
        let mut edge = start;
        let mut guard = 0usize;
        loop {
            loop_indices.push(usize::try_from(*ev.get(edge)?).ok()?);
            edge = usize::try_from(*nxt.get(edge)?).ok()?;
            guard += 1;
            if edge == start || guard > 4096 {
                break;
            }
        }
        faces.push(loop_indices);

        // per-face material (oracle: faceDataIndices -> materialindex stream data, 0 out of range)
        let mut material_index = 0usize;
        if let Some(indices) = material_indices.as_deref() {
            let di = fdi.get(fi).copied().unwrap_or(fi as i64);
            if let Some(value) = signed_get(indices, di) {
                material_index = usize::try_from(value).unwrap_or(0);
            }
        }
        let material = mats
            .get(material_index)
            .or_else(|| mats.first())
            .cloned()
            .unwrap_or_default();
        face_materials.push(material);
    }

    Some(MeshRecord {
        id: element.get("nodeID").and_then(Value::as_i64),
        origin: vec3_raw(element.get("origin")),
        angles: vec3_raw(element.get("angles")),
        scales: vec3_raw(element.get("scales")),
        vertices,
        faces,
        face_materials,
        materials: mats,
        ancestors: ancestor_chain(elements, parents, element.index),
    })
}

/// Python-style single lookup with negative indexing (`lst[di]`): `di < 0` reads from the end.
/// Never happens on a Hammer-written file; the oracle would do the same thing.
fn signed_get(list: &[i64], index: i64) -> Option<i64> {
    let index = if index < 0 { list.len() as i64 + index } else { index };
    usize::try_from(index).ok().and_then(|index| list.get(index).copied())
}

/// The `data` array of the first matching stream of a `CDmePolygonMeshDataArray` element (oracle:
/// the stream with `name == "<name0>"` **or** `standardAttributeName == "<std>"`).
fn stream_data<'a>(
    array: &'a DmxElement,
    elements: &'a [DmxElement],
    name0: &str,
    std: &str,
) -> Option<&'a Value> {
    let streams = match array.get("streams") {
        Some(Value::Array(items)) => items,
        _ => return None,
    };
    streams.iter().find_map(|item| {
        let stream = match item {
            Value::Element(index) => elements.get(*index)?,
            _ => return None,
        };
        let name = matches!(stream.get("name"), Some(Value::Str(text)) if text == name0);
        let standard =
            matches!(stream.get("standardAttributeName"), Some(Value::Str(text)) if text == std);
        if name || standard {
            stream.get("data")
        } else {
            None
        }
    })
}

/// The `CDmePolygonMeshDataArray` element of a mesh attribute (`vertexData`, `faceData`).
fn data_array<'a>(
    md: &'a DmxElement,
    attribute: &str,
    elements: &'a [DmxElement],
) -> Option<&'a DmxElement> {
    match md.get(attribute) {
        Some(Value::Element(index)) => elements.get(*index),
        _ => None,
    }
}

/// The position stream `data` as world-local `[x, y, z]` triples widened to f64.
fn pos_list(value: &Value) -> Option<Vec<[f64; 3]>> {
    match value {
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::Vec3(vector) => {
                        out.push([vector[0] as f64, vector[1] as f64, vector[2] as f64])
                    }
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// An integer-array attribute as `Vec<i64>` (the oracle reads it as a plain sequence).
fn int_list(value: Option<&Value>) -> Option<Vec<i64>> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Int(v) => Some(*v as i64),
                Value::Byte(v) => Some(*v as i64),
                Value::Time(v) => Some(*v as i64),
                Value::U64(v) => i64::try_from(*v).ok(),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// `list(md.get(key) or [])` (the oracle's default, review L6): an absent or `null` attribute reads
/// as the empty list — a Hammer file that omits `vertexDataIndices` (the identity mapping) is then
/// still *listed*, exactly like the oracle. A present-but-unreadable array still returns `None` and
/// skips the mesh ([`mesh_from_element`]'s documented deviation: the oracle would raise on it).
fn int_list_or_empty(value: Option<&Value>) -> Option<Vec<i64>> {
    match value {
        None | Some(Value::Null) => Some(Vec::new()),
        other => int_list(other),
    }
}

/// The node's `origin`/`angles`/`scales` as f64 (unrounded, like the oracle's `float(c)`).
fn vec3_raw(value: Option<&Value>) -> Option<[f64; 3]> {
    value
        .and_then(Value::as_vec3)
        .map(|vector| [vector[0] as f64, vector[1] as f64, vector[2] as f64])
}

/// One node-local point to world: scale, rotate by the angle matrix, translate (the file route's
/// extraction applies this to every vertex; `mesh.get`'s live route calls it too so both routes
/// give identical world vertices).
fn apply_transform(
    local: [f64; 3],
    scale: [f64; 3],
    matrix: &[[f64; 3]; 3],
    origin: [f64; 3],
) -> [f64; 3] {
    let lx = local[0] * scale[0];
    let ly = local[1] * scale[1];
    let lz = local[2] * scale[2];
    let mut world = [0.0f64; 3];
    for k in 0..3 {
        world[k] = matrix[k][0] * lx + matrix[k][1] * ly + matrix[k][2] * lz + origin[k];
    }
    world
}

/// A node-local point to WORLD with the mesh's own `scales`, `angles` (`[pitch, yaw, roll]`
/// degrees) and `origin` — the exact transform [`mesh_from_element`] applies to a saved mesh.
pub fn local_to_world(
    local: [f64; 3],
    scales: [f64; 3],
    angles: [f64; 3],
    origin: [f64; 3],
) -> [f64; 3] {
    apply_transform(local, scales, &angle_matrix(angles), origin)
}

/// Source `AngleMatrix` (POC `angle_matrix`): columns forward/left/up for QAngle
/// `[pitch, yaw, roll]` in degrees.
pub fn angle_matrix(angles: [f64; 3]) -> [[f64; 3]; 3] {
    let (pitch, yaw, roll) = (
        angles[0].to_radians(),
        angles[1].to_radians(),
        angles[2].to_radians(),
    );
    let (sp, cp, sy, cy, sr, cr) = (
        pitch.sin(),
        pitch.cos(),
        yaw.sin(),
        yaw.cos(),
        roll.sin(),
        roll.cos(),
    );
    [
        [cp * cy, sr * sp * cy - cr * sy, cr * sp * cy + sr * sy],
        [cp * sy, sr * sp * sy + cr * cy, cr * sp * sy - sr * cy],
        [-sp, sr * cp, cr * cp],
    ]
}

fn vec3_json(vector: Option<[f64; 3]>) -> Json {
    match vector {
        None => Json::Null,
        Some([x, y, z]) => json!([x, y, z]),
    }
}

/// Python `round(x, 3)` on the box values — the shared, correctly rounded form
/// ([`round3`]; review C6: `list_triggers`' volume boxes re-implemented this private fn).
fn rounded3(vector: &[f64; 3]) -> Vec<Json> {
    vector.iter().map(|c| Json::from(round3(*c))).collect()
}

/// Python `round(x, 3)` on one value: the correctly rounded 3-decimal double
/// (`{:.3}` formatting is the same decimal conversion as Python's `round`), not the
/// `(x * 1000).round() / 1000` form whose intermediate product is itself rounded — the
/// review-N4 rule that already governs [`round4`]. Shared with the trigger tools' volume
/// boxes (`tools/triggers_query`).
pub(crate) fn round3(value: f64) -> f64 {
    format!("{value:.3}").parse().unwrap_or(value)
}

/// Python `round(x, 4)` (oracle `to_dict` vertices / `face_normal`), shared with the live tools'
/// cached-geometry view (`tools/mesh_file`).
///
/// Review N4: this is the *correctly rounded* 4-decimal value of the binary double (ties to even),
/// exactly what Python's `round` computes, rather than `(x * 1e4).round() / 1e4` — whose
/// intermediate product is itself rounded and can push a value that sits on a .00005 boundary the
/// other way. Rust's `{:.4}` formatting is the same correctly rounded decimal conversion.
pub fn round4(value: f64) -> f64 {
    format!("{value:.4}").parse().unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmap::dmx::{DmxElement, Value as Dmx};

    /// Angles are `[pitch, yaw, roll]` and the matrix's columns are forward/left/up
    /// (`AngleMatrix`), so `world = M * local`. Hand-checked: every one of the twelve entries
    /// below is decided by one of the three axis maps. A swapped pitch/yaw, a flipped sign, a
    /// transposed matrix or a `world = M^T * local` reading fails at least one of them (review M3:
    /// `face_normal` normalises, so a unit-normal assertion passes for *any* rotation).
    #[test]
    fn angle_matrix_maps_the_three_axes_as_the_oracle_does() {
        let identity = angle_matrix([0.0, 0.0, 0.0]);
        for row in 0..3 {
            for column in 0..3 {
                let want = if row == column { 1.0 } else { 0.0 };
                assert!(
                    (identity[row][column] - want).abs() < 1e-12,
                    "identity[{row}][{column}]: {identity:?}"
                );
            }
        }

        // yaw 90: +x (forward) -> +y, +y (left) -> -x, z untouched
        let yaw = angle_matrix([0.0, 90.0, 0.0]);
        assert_close(mul_matrix(yaw, [1.0, 0.0, 0.0]), [0.0, 1.0, 0.0], "yaw 90: +x");
        assert_close(mul_matrix(yaw, [0.0, 1.0, 0.0]), [-1.0, 0.0, 0.0], "yaw 90: +y");
        assert_close(mul_matrix(yaw, [0.0, 0.0, 1.0]), [0.0, 0.0, 1.0], "yaw 90: +z");

        // pitch 90: +x (forward) -> -z (looking straight down), +y (left) untouched
        let pitch = angle_matrix([90.0, 0.0, 0.0]);
        assert_close(mul_matrix(pitch, [1.0, 0.0, 0.0]), [0.0, 0.0, -1.0], "pitch 90: +x");
        assert_close(mul_matrix(pitch, [0.0, 1.0, 0.0]), [0.0, 1.0, 0.0], "pitch 90: +y");
        assert_close(mul_matrix(pitch, [0.0, 0.0, 1.0]), [1.0, 0.0, 0.0], "pitch 90: +z");

        // roll 90: +y (left) -> +z (up), +x (forward) untouched
        let roll = angle_matrix([0.0, 0.0, 90.0]);
        assert_close(mul_matrix(roll, [0.0, 1.0, 0.0]), [0.0, 0.0, 1.0], "roll 90: +y");
        assert_close(mul_matrix(roll, [1.0, 0.0, 0.0]), [1.0, 0.0, 0.0], "roll 90: +x");
        assert_close(mul_matrix(roll, [0.0, 0.0, 1.0]), [0.0, -1.0, 0.0], "roll 90: +z");
    }

    /// A rotated + scaled + moved mesh, end to end: the world vertices are computed here from the
    /// oracle's own rule (`angle_matrix(angles) * (scales * local) + origin`) with numbers worked
    /// out by hand, so a wrong rotation, a wrong scale order or a wrong translation order fails.
    /// The values: `angles [0,90,0]`, `scales [2,3,1]`, `origin [10,20,30]` and the local triangle
    /// `(1,0,0) (0,1,0) (0,0,1)` give `(10,22,30) (7,20,30) (10,20,31)`:
    ///
    /// * `(1,0,0)` scales to `(2,0,0)` and yaw 90 sends `+x` to `+y` -> `(0,2,0)`;
    /// * `(0,1,0)` scales to `(0,3,0)` and yaw 90 sends `+y` to `-x` -> `(-3,0,0)`;
    /// * `(0,0,1)` is on the yaw axis -> `(0,0,1)`.
    ///
    /// Scaling *after* the rotation would give `(0,3,0)` for the first vertex instead of
    /// `(0,2,0)` (the yaw moves the 2 onto the axis the 3 scales), so the scale-then-rotate order
    /// is pinned too.
    #[test]
    fn rotated_scaled_and_moved_mesh_has_the_hand_computed_world_vertices() {
        let (elements, root) = synthetic_rotated_mesh(true);
        let record = mesh_from_element(&root, &elements).expect("the mesh is extracted");
        assert_eq!(record.faces, vec![vec![0, 1, 2]]);
        assert_eq!(record.face_materials, vec!["materials/dev/a.vmat".to_string()]);
        assert_eq!(record.vertices.len(), 3);
        assert_close(record.vertices[0], [10.0, 22.0, 30.0], "vertex 0");
        assert_close(record.vertices[1], [7.0, 20.0, 30.0], "vertex 1");
        assert_close(record.vertices[2], [10.0, 20.0, 31.0], "vertex 2");
        // the record reports the raw attributes, not the world result
        assert_eq!(record.angles, Some([0.0, 90.0, 0.0]));
        assert_eq!(record.scales, Some([2.0, 3.0, 1.0]));
        assert_eq!(record.origin, Some([10.0, 20.0, 30.0]));
    }

    /// `vertexDataIndices` is optional in the oracle (`list(md.get(...) or [])` with a per-vertex
    /// fallback to the vertex's own index): a Hammer file that omits it must still be *listed*, not
    /// dropped (review L6).
    #[test]
    fn a_mesh_without_vertex_data_indices_is_still_extracted() {
        let (elements, root) = synthetic_rotated_mesh(false);
        let record = mesh_from_element(&root, &elements)
            .expect("a missing vertexDataIndices is the identity mapping, not a reason to skip");
        assert_eq!(record.vertices.len(), 3);
        assert_close(record.vertices[0], [10.0, 22.0, 30.0], "vertex 0");
        assert_close(record.vertices[1], [7.0, 20.0, 30.0], "vertex 1");
        assert_close(record.vertices[2], [10.0, 20.0, 31.0], "vertex 2");
        assert_eq!(record.faces, vec![vec![0, 1, 2]]);
    }

    /// review-3 NEW-5: the file route may use a mesh only when its `children` chain is one this
    /// crate has proven — a mesh under `CMapWorld`, optionally through `CMapGroup`/`CMapEntity`
    /// containers (what every mesh of the content tree looks like), and nothing else: a deformer,
    /// an instance, a prefab or a mesh outside the world tree could store its geometry in another
    /// space, so the route refuses it and names the ancestor.
    #[test]
    fn the_parent_gate_accepts_proven_containers_only() {
        // the shape every mesh of the corpus has: containers then the world root
        for chain in [
            vec!["CMapWorld"],
            vec!["CMapGroup", "CMapWorld"],
            vec!["CMapEntity", "CMapWorld"],
            vec!["CMapEntity", "CMapGroup", "CMapGroup", "CMapWorld"],
        ] {
            let (elements, root) = synthetic_mesh_under(&chain);
            let record = mesh_from_element(&root, &elements).expect("extracted");
            assert_eq!(record.parent_problem(), None, "{chain:?}");
        }

        // a deformer, an instance, a prefab, a selection set: unproven, whichever way they nest
        for (chain, bad) in [
            (vec!["CMapDeformerLattice", "CMapWorld"], "CMapDeformerLattice"),
            (vec!["CMapInstance", "CMapWorld"], "CMapInstance"),
            (vec!["CMapGroup", "CMapPrefab", "CMapWorld"], "CMapPrefab"),
            (vec!["CMapSelectionSet"], "CMapSelectionSet"),
        ] {
            let (elements, root) = synthetic_mesh_under(&chain);
            let record = mesh_from_element(&root, &elements).expect("extracted");
            assert_eq!(
                record.parent_problem(),
                Some(ParentProblem::Unsupported(bad.to_string())),
                "{chain:?}"
            );
        }

        // a mesh with no `children` parent at all: outside the tree, refused
        let (elements, root) = synthetic_rotated_mesh(true);
        let record = mesh_from_element(&root, &elements).expect("extracted");
        assert_eq!(record.parent_problem(), Some(ParentProblem::Unrooted));

        // a cycle (`A` owns the mesh and `B`; `B` owns `A`) is refused, never walked forever
        let (mut elements, root) = synthetic_rotated_mesh(true);
        let a = elements.len();
        elements.push(element(
            "CMapGroup",
            a,
            vec![(
                "children",
                Dmx::Array(vec![Dmx::Element(root.index), Dmx::Element(a + 1)]),
            )],
        ));
        elements.push(element(
            "CMapGroup",
            a + 1,
            vec![("children", Dmx::Array(vec![Dmx::Element(a)]))],
        ));
        let record = mesh_from_element(&root, &elements).expect("extracted");
        assert_eq!(record.parent_problem(), Some(ParentProblem::Unrooted));
    }

    /// `synthetic_rotated_mesh`'s mesh inside the given ancestor classes, **innermost first**
    /// (`["CMapGroup", "CMapWorld"]` = the mesh under a group under the world root). The last
    /// entry has no parent: the walk stops there.
    fn synthetic_mesh_under(outer: &[&str]) -> (Vec<DmxElement>, DmxElement) {
        let (mut elements, mesh) = synthetic_rotated_mesh(true);
        let mut child = mesh.index;
        for class in outer {
            let index = elements.len();
            elements.push(element(
                class,
                index,
                vec![("children", Dmx::Array(vec![Dmx::Element(child)]))],
            ));
            child = index;
        }
        (elements, mesh)
    }

    fn assert_close(got: [f64; 3], want: [f64; 3], what: &str) {        for axis in 0..3 {
            assert!(
                (got[axis] - want[axis]).abs() < 1e-6,
                "{what}: got {got:?}, want {want:?}"
            );
        }
    }

    fn mul_matrix(matrix: [[f64; 3]; 3], local: [f64; 3]) -> [f64; 3] {
        let mut out = [0.0f64; 3];
        for k in 0..3 {
            out[k] = matrix[k][0] * local[0] + matrix[k][1] * local[1] + matrix[k][2] * local[2];
        }
        out
    }

    fn element(class: &str, index: usize, attributes: Vec<(&str, Dmx)>) -> DmxElement {
        DmxElement {
            index,
            class_name: class.to_string(),
            name: String::new(),
            guid: format!("synthetic-{index}"),
            stub: false,
            attributes: attributes
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect(),
        }
    }

    /// A `CMapMesh` element tree: the root (index 0), its `meshData` (1), the vertex-data array (2)
    /// with the position stream (3) and the empty face-data array (4) — the smallest shape
    /// `mesh_from_element` accepts. `with_vdi` controls whether `vertexDataIndices` is present.
    fn synthetic_rotated_mesh(with_vdi: bool) -> (Vec<DmxElement>, DmxElement) {
        let positions = Dmx::Array(vec![
            Dmx::Vec3([1.0, 0.0, 0.0]),
            Dmx::Vec3([0.0, 1.0, 0.0]),
            Dmx::Vec3([0.0, 0.0, 1.0]),
        ]);
        let stream = element(
            "CDmePolygonMeshDataStream",
            3,
            vec![
                ("name", Dmx::Str("position:0".into())),
                ("standardAttributeName", Dmx::Str("position".into())),
                ("data", positions),
            ],
        );
        let vertex_data = element(
            "CDmePolygonMeshDataArray",
            2,
            vec![("streams", Dmx::Array(vec![Dmx::Element(3)]))],
        );
        let face_data = element(
            "CDmePolygonMeshDataArray",
            4,
            vec![("streams", Dmx::Array(vec![]))],
        );
        let mut md_attributes: Vec<(&str, Dmx)> = vec![
            ("vertexData", Dmx::Element(2)),
            ("faceData", Dmx::Element(4)),
            // 3 vertices, 3 edges; one face walking edges 0 -> 1 -> 2 -> 0
            ("edgeVertexIndices", Dmx::Array(vec![Dmx::Int(0), Dmx::Int(1), Dmx::Int(2)])),
            ("edgeNextIndices", Dmx::Array(vec![Dmx::Int(1), Dmx::Int(2), Dmx::Int(0)])),
            ("faceEdgeIndices", Dmx::Array(vec![Dmx::Int(0)])),
            ("vertexEdgeIndices", Dmx::Array(vec![Dmx::Int(0), Dmx::Int(1), Dmx::Int(2)])),
            (
                "materials",
                Dmx::Array(vec![Dmx::Str("materials/dev/a.vmat".into())]),
            ),
        ];
        if with_vdi {
            md_attributes.push((
                "vertexDataIndices",
                Dmx::Array(vec![Dmx::Int(0), Dmx::Int(1), Dmx::Int(2)]),
            ));
        }
        let md = element("CDmePolygonMesh", 1, md_attributes);
        let root = element(
            "CMapMesh",
            0,
            vec![
                ("nodeID", Dmx::Int(7)),
                ("origin", Dmx::Vec3([10.0, 20.0, 30.0])),
                ("angles", Dmx::Vec3([0.0, 90.0, 0.0])),
                ("scales", Dmx::Vec3([2.0, 3.0, 1.0])),
                ("meshData", Dmx::Element(1)),
            ],
        );
        let elements = vec![root.clone(), md, vertex_data, stream, face_data];
        (elements, root)
    }
}
