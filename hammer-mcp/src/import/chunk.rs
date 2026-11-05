//! Faces -> `mesh.create` batches (`kind:"hammer_mesh"`).
//!
//! A chunk is ONE material, at most [`Limits::max_tris`] triangles (a merged face counts
//! `n - 2`), and small enough that both of its requests stay under
//! [`Limits::max_request_bytes`]: the `mesh.create` params and the `mesh.setFaceTexture`
//! `axes` params (measured by serialising them, the way the tool sends them). Faces are grouped
//! by material, then split recursively at the median of the longest axis of their centroids, so
//! each chunk is spatially compact (a Hammer mesh pivots on its bounds; one mesh over a whole
//! level would be one giant culling unit).
//!
//! Vertices are rounded to 4 decimals (what `create_mesh` sends) and welded per chunk. A face
//! that is no longer a polygon after the rounding or the f32 widening the bridge applies
//! (repeated corner, area under the float noise) is not emitted: it is listed in
//! [`Split::dropped`] (Hammer would refuse the whole chunk for it).
//!
//! Face order: `Chunk::faces[i]` is `Chunk::face_ids[i]` of the input, in the order they are sent
//! to `mesh.create`. The bridge's `mesh.setFaceTexture` indices are `get_mesh`'s face order
//! (`CollectHandles`), which for a fresh mesh is the creation order; the tool verifies this
//! against `mesh.get` instead of assuming it (see `tools::hammer_mesh`).
use super::planar::{Face, TexAxes, MIN_TWICE_AREA};
use crate::vmap::mesh::round4;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

type V3 = [f64; 3];

/// The bridge's per-mesh face cap.
pub const MAX_FACES_PER_MESH: usize = 65_536;
/// Slack for the request envelope (`expectDoc`, ids, the material path).
const ENVELOPE: usize = 1024;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_tris: usize,
    pub max_request_bytes: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// Index into `Scene::materials`.
    pub material: usize,
    /// Rounded, welded vertices.
    pub vertices: Vec<V3>,
    pub faces: Vec<Vec<usize>>,
    /// Index into the input faces of each of `faces`.
    pub face_ids: Vec<usize>,
    pub tris: usize,
    /// Serialised size of the `mesh.create` params (without a material path).
    pub create_bytes: usize,
    /// Serialised size of the `mesh.setFaceTexture` axes params.
    pub align_bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Split {
    pub chunks: Vec<Chunk>,
    /// Input face ids that are not polygons once rounded to what the bridge sends.
    pub dropped: Vec<usize>,
}

/// An f32-exact value printed in its shortest decimal (the bridge stores f32; this halves the
/// request size and the bridge reads back the same f32).
pub fn f32_json(x: f64) -> f64 {
    (x as f32).to_string().parse().unwrap_or(x)
}

/// One `faces[]` entry of a `mesh.setFaceTexture` `axes` request.
pub fn align_entry(index: usize, a: &TexAxes) -> Value {
    let q = |v: &[f64; 4]| json!([f32_json(v[0]), f32_json(v[1]), f32_json(v[2]), f32_json(v[3])]);
    json!({
        "index": index,
        "axisU": q(&a.axis_u),
        "axisV": q(&a.axis_v),
        "scale": [f32_json(a.scale[0]), f32_json(a.scale[1])],
    })
}

fn round_point(p: V3) -> V3 {
    p.map(|c| {
        let r = round4(c);
        if r == 0.0 { 0.0 } else { r }
    })
}

fn newell_vec(p: &[V3]) -> V3 {
    let mut n = [0.0; 3];
    for i in 0..p.len() {
        let (a, b) = (p[i], p[(i + 1) % p.len()]);
        n[0] += (a[1] - b[1]) * (a[2] + b[2]);
        n[1] += (a[2] - b[2]) * (a[0] + b[0]);
        n[2] += (a[0] - b[0]) * (a[1] + b[1]);
    }
    n
}

fn newell(p: &[V3]) -> f64 {
    let n = newell_vec(p);
    (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt()
}

/// Whether two loops are the same face up to corner order: every corner of `a` lies within `tol`
/// of a corner of `b` (same count checked by the caller), and both wind the same way (a
/// back-to-back pair shares its corners but faces the other side).
fn same_face(a: &[V3], b: &[V3], tol: f64) -> bool {
    let near = |p: &V3, q: &V3| (0..3).all(|k| (p[k] - q[k]).abs() <= tol);
    if !a.iter().all(|p| b.iter().any(|q| near(p, q))) {
        return false;
    }
    let (na, nb) = (newell_vec(a), newell_vec(b));
    let dot = na[0] * nb[0] + na[1] * nb[1] + na[2] * nb[2];
    dot > 0.999 * newell(a) * newell(b)
}

/// The rounded loop when it is still a polygon on the numbers the bridge reads, else `None`.
fn rounded_loop(face: &Face) -> Option<Vec<V3>> {
    let pts: Vec<V3> = face.verts.iter().copied().map(round_point).collect();
    let mut seen = std::collections::HashSet::new();
    if pts.len() < 3 || !pts.iter().all(|p| seen.insert(p.map(f64::to_bits))) {
        return None;
    }
    let floats: Vec<V3> = pts.iter().map(|p| p.map(|c| c as f32 as f64)).collect();
    let mag = pts.iter().flatten().fold(0.0f64, |m, c| m.max(c.abs()));
    let step = mag * (1.0 / (1u64 << 24) as f64);
    let floor = MIN_TWICE_AREA.max(2.0 * step * step);
    (newell(&pts) >= floor && newell(&floats) >= floor).then_some(pts)
}

fn centroid(p: &[V3]) -> V3 {
    let n = p.len() as f64;
    let mut c = [0.0; 3];
    for q in p {
        for k in 0..3 {
            c[k] += q[k] / n;
        }
    }
    c
}

fn build(material: usize, ids: &[usize], loops: &HashMap<usize, Vec<V3>>, faces: &[Face]) -> Chunk {
    let mut index: HashMap<[i64; 3], usize> = HashMap::new();
    let mut vertices: Vec<V3> = Vec::new();
    let mut out_faces: Vec<Vec<usize>> = Vec::with_capacity(ids.len());
    let mut tris = 0;
    let mut align_bytes = 128;
    for (n, &id) in ids.iter().enumerate() {
        let l = &loops[&id];
        tris += l.len() - 2;
        out_faces.push(
            l.iter()
                .map(|p| {
                    let k = p.map(|c| (c * 1e4).round() as i64);
                    *index.entry(k).or_insert_with(|| {
                        vertices.push(*p);
                        vertices.len() - 1
                    })
                })
                .collect(),
        );
        align_bytes += align_entry(n, &faces[id].axes).to_string().len() + 1;
    }
    let create_bytes = json!({"vertices": vertices, "faces": out_faces, "material": ""}).to_string().len() + ENVELOPE;
    Chunk { material, vertices, faces: out_faces, face_ids: ids.to_vec(), tris, create_bytes, align_bytes }
}

fn fits(c: &Chunk, limits: &Limits) -> bool {
    c.tris <= limits.max_tris
        && c.faces.len() <= MAX_FACES_PER_MESH
        && c.create_bytes <= limits.max_request_bytes
        && c.align_bytes + ENVELOPE <= limits.max_request_bytes
}

fn partition(
    material: usize,
    mut ids: Vec<usize>,
    loops: &HashMap<usize, Vec<V3>>,
    faces: &[Face],
    limits: &Limits,
    out: &mut Vec<Chunk>,
) {
    let chunk = build(material, &ids, loops, faces);
    if ids.len() == 1 || fits(&chunk, limits) {
        out.push(chunk);
        return;
    }
    let cs: HashMap<usize, V3> = ids.iter().map(|&i| (i, centroid(&loops[&i]))).collect();
    let (mut lo, mut hi) = ([f64::MAX; 3], [f64::MIN; 3]);
    for c in cs.values() {
        for k in 0..3 {
            lo[k] = lo[k].min(c[k]);
            hi[k] = hi[k].max(c[k]);
        }
    }
    let axis = (0..3).max_by(|&a, &b| (hi[a] - lo[a]).total_cmp(&(hi[b] - lo[b]))).unwrap_or(0);
    ids.sort_by(|a, b| cs[a][axis].total_cmp(&cs[b][axis]).then(a.cmp(b)));
    let right = ids.split_off(ids.len() / 2);
    partition(material, ids, loops, faces, limits, out);
    partition(material, right, loops, faces, limits, out);
}

/// Split `faces` into chunks (see the module docs).
pub fn split(faces: &[Face], limits: &Limits) -> Split {
    let mut loops: HashMap<usize, Vec<V3>> = HashMap::new();
    let mut by_material: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    let mut dropped = Vec::new();
    for (id, face) in faces.iter().enumerate() {
        match rounded_loop(face) {
            Some(l) => {
                loops.insert(id, l);
                by_material.entry(face.material).or_default().push(id);
            }
            None => dropped.push(id),
        }
    }
    let mut chunks = Vec::new();
    for (material, ids) in by_material {
        partition(material, ids, &loops, faces, limits, &mut chunks);
    }
    Split { chunks, dropped }
}

/// For each planned loop, the index of the loop in `actual` that is the same face (same corner
/// count, the same corners within 0.02 units in any order, the same winding) with the nearest
/// centroid, each `actual` loop used once (a tie keeps the creation index); `None` when nothing
/// matches. Crossed or back-to-back faces share a centroid, so the centroid alone never pairs. Pairs the faces a chunk sent to `mesh.create` with the faces `mesh.get` reports, so
/// the texture indices do not depend on Hammer keeping the input order.
pub fn match_faces(planned: &[Vec<V3>], actual: &[Vec<V3>]) -> Vec<Option<usize>> {
    const CELL: f64 = 0.05;
    const TOL: f64 = 0.02;
    let cell = |c: V3| c.map(|x| (x / CELL).floor() as i64);
    let mut grid: HashMap<[i64; 3], Vec<(usize, V3)>> = HashMap::new();
    for (i, l) in actual.iter().enumerate() {
        let c = centroid(l);
        grid.entry(cell(c)).or_default().push((i, c));
    }
    let mut taken = vec![false; actual.len()];
    planned
        .iter()
        .enumerate()
        .map(|(pi, l)| {
            let c = centroid(l);
            let at = cell(c);
            let mut best: Option<(f64, usize)> = None;
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for dz in -1..=1 {
                        let Some(list) = grid.get(&[at[0] + dx, at[1] + dy, at[2] + dz]) else { continue };
                        for &(i, ac) in list {
                            if taken[i] || actual[i].len() != l.len() || !same_face(l, &actual[i], TOL) {
                                continue;
                            }
                            let d = ((c[0] - ac[0]).powi(2) + (c[1] - ac[1]).powi(2) + (c[2] - ac[2]).powi(2)).sqrt();
                            let better = |(b, bi): (f64, usize)| d < b - 1e-9 || (d <= b + 1e-9 && i == pi && bi != pi);
                            if d <= TOL && best.is_none_or(better) {
                                best = Some((d, i));
                            }
                        }
                    }
                }
            }
            best.map(|(_, i)| {
                taken[i] = true;
                i
            })
        })
        .collect()
}
