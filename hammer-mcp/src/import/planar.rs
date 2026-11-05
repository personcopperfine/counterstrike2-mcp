//! Triangles -> planar faces with exact per-face texture axes (`kind:"hammer_mesh"`).
//!
//! # Space
//!
//! Hammer meshes are placed directly in the map (no compiler turn), so the plain glTF -> CS2 map
//! is used: [`UpAxis::to_cs2`] `(x, y, z) -> (z, x, y)` (a rotation, det +1, winding stays
//! counter-clockwise) times `scale`, then the caller's rotation (`angles`, Source `AngleMatrix`)
//! and `origin`. The Rz(-90) pre-turn of [`super::smd::to_smd`] is for the SMD compiler only and
//! is NOT applied here.
//!
//! # Merging
//!
//! Adjacent triangles (shared edge, found by position) merge into one face only when all hold:
//! same material, the same plane (normals within `1e-6` of parallel, every vertex within
//! [`PLANE_TOL`] of the seed's plane), a consistent UV mapping (the seed's affine world -> UV map
//! predicts the other triangle's corner UVs within [`UV_TOL`]), and the union is ONE simple
//! boundary loop that is **convex**. Anything else stays a triangle. Regions are capped at
//! [`MAX_REGION`] triangles and [`MAX_LOOP`] corners. Collinear corners left on the boundary are
//! dropped (a T-junction of float noise, invisible).
//!
//! # Texture axes
//!
//! Hammer stores per face `textureAxisU/V` (xyz + offset w) and `textureScale` (world units per
//! texel). The texel coordinate of a world point `p` is (RE of hammer.dll `sub_180AE4480`,
//! `workflows/streamline/re/wp15-re.md`, marked verified there):
//!
//! ```text
//! s = dot(p, axisU.xyz) / scaleU + axisU.w        t = dot(p, axisV.xyz) / scaleV + axisV.w
//! ```
//!
//! and the normalised UV is `s / texW`, `t / texH` (the divide by the texture size is INFERRED,
//! [I]: the RE marks only the texel part verified; it matches Source 1's `texinfo`). V runs DOWN
//! the image in Hammer (default floor axes are `U=+x`, `V=-y`), the same as glTF, so the glTF
//! UVs are used as they are (no `1 - v`; that flip is for the bottom-up SMD only) [I].
//!
//! Per face the affine map world -> (u, v) on the face plane is solved from three corners (the
//! best-conditioned triple through corner 0); each gradient is a 3D vector in the plane:
//! `axis = normalize(grad * texSize)`, `scale = 1 / |grad * texSize|`, `w = const * texSize`.
//! Mirrored UVs just flip the axis (scale stays positive). A face whose UVs have no usable map
//! (zero area, collinear, a scale outside `1e-4..1e6`) gets Hammer's default world projection
//! (`U/V` by the dominant normal axis, scale 0.25, offset 0) and counts in
//! [`Planar::defaulted_faces`].
use super::gltf_read::Scene;
use super::smd::UpAxis;
use std::collections::HashMap;

type V3 = [f64; 3];
type Key = [i64; 3];

/// A triangle (or merged face) with twice its area below this is dropped (CS2 units squared).
pub const MIN_TWICE_AREA: f64 = 1e-4;
/// A merged vertex may sit this far (CS2 units) off the seed triangle's plane.
pub const PLANE_TOL: f64 = 5e-4;
/// Normalised-UV tolerance for "same mapping" when merging.
pub const UV_TOL: f64 = 2e-5;
pub const MAX_REGION: usize = 64;
pub const MAX_LOOP: usize = 32;
/// Texture size assumed when a material has none (normalised-UV -> texel).
pub const DEFAULT_TEX_SIZE: [f64; 2] = [512.0, 512.0];
/// Hammer's default world-projection scale (world units per texel).
pub const DEFAULT_SCALE: f64 = 0.25;
const NORMAL_COS: f64 = 1.0 - 1e-6;
/// Sine of the turn at a corner below which it counts as straight (f32 source positions carry
/// ~1e-6 of relative noise): such a corner is not a convexity violation and is dropped.
const COLLINEAR: f64 = 1e-5;

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn len(a: V3) -> f64 {
    dot(a, a).sqrt()
}
fn mul(a: V3, k: f64) -> V3 {
    [a[0] * k, a[1] * k, a[2] * k]
}
fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn unit(a: V3) -> V3 {
    let l = len(a);
    if l > 0.0 { mul(a, 1.0 / l) } else { a }
}
fn key(p: V3) -> Key {
    [(p[0] * 1e4).round() as i64, (p[1] * 1e4).round() as i64, (p[2] * 1e4).round() as i64]
}

/// `textureAxisU/V` + `textureScale` of one face, in Hammer's own terms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TexAxes {
    pub axis_u: [f64; 4],
    pub axis_v: [f64; 4],
    pub scale: [f64; 2],
}

impl TexAxes {
    /// The texel coordinate Hammer computes for world point `p`.
    pub fn texel(&self, p: V3) -> [f64; 2] {
        let a = |ax: &[f64; 4], s: f64| dot(p, [ax[0], ax[1], ax[2]]) / s + ax[3];
        [a(&self.axis_u, self.scale[0]), a(&self.axis_v, self.scale[1])]
    }
    /// Normalised UV of `p` for a texture of `size` texels.
    pub fn uv(&self, p: V3, size: [f64; 2]) -> [f64; 2] {
        let t = self.texel(p);
        [t[0] / size[0], t[1] / size[1]]
    }
}

/// One output face: a convex planar loop (counter-clockwise from the visible side).
#[derive(Debug, Clone, PartialEq)]
pub struct Face {
    /// Index into [`Scene::materials`].
    pub material: usize,
    /// Final CS2/world positions of the loop.
    pub verts: Vec<V3>,
    /// The source (glTF-convention) UV of each loop corner.
    pub uvs: Vec<[f64; 2]>,
    /// Source triangles merged into this face.
    pub tris: usize,
    pub axes: TexAxes,
    /// `axes` is Hammer's default projection (the UVs were unusable).
    pub defaulted: bool,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub scale: f64,
    pub up: UpAxis,
    /// Row-major `world = rotation * p + origin` (Source `AngleMatrix`).
    pub rotation: [[f64; 3]; 3],
    pub origin: V3,
    /// Texture size per `Scene::materials` index; missing = [`DEFAULT_TEX_SIZE`].
    pub tex_sizes: Vec<[f64; 2]>,
}

impl Options {
    pub fn new(scale: f64, up: UpAxis) -> Options {
        Options {
            scale,
            up,
            rotation: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            origin: [0.0; 3],
            tex_sizes: Vec::new(),
        }
    }
    pub fn tex_size(&self, material: usize) -> [f64; 2] {
        self.tex_sizes.get(material).copied().unwrap_or(DEFAULT_TEX_SIZE)
    }
}

#[derive(Debug, Clone)]
pub struct Planar {
    pub faces: Vec<Face>,
    /// Non-degenerate source triangles.
    pub tris: usize,
    /// Triangles dropped (zero area / collapsed).
    pub degenerate_tris: usize,
    pub defaulted_faces: usize,
}

#[derive(Clone, Copy)]
struct UvMap {
    gu: V3,
    gv: V3,
    cu: f64,
    cv: f64,
}

impl UvMap {
    fn at(&self, p: V3) -> [f64; 2] {
        [dot(self.gu, p) + self.cu, dot(self.gv, p) + self.cv]
    }
}

struct Tri {
    p: [V3; 3],
    uv: [[f64; 2]; 3],
    mat: usize,
    n: V3,
    k: [Key; 3],
    map: Option<UvMap>,
}

/// The affine map world -> UV on the plane of `p` (unit normal `n`) from three corners.
fn solve(p: [V3; 3], uv: [[f64; 2]; 3], n: V3) -> Option<UvMap> {
    let d1 = sub(p[1], p[0]);
    let d2 = sub(p[2], p[0]);
    let e1 = unit(d1);
    let e2 = cross(n, e1);
    let (a1, b1) = (dot(d1, e1), dot(d1, e2));
    let (a2, b2) = (dot(d2, e1), dot(d2, e2));
    let det = a1 * b2 - a2 * b1;
    if det.abs() < 1e-9 {
        return None;
    }
    let grad = |c: usize| -> (V3, f64) {
        let (du1, du2) = (uv[1][c] - uv[0][c], uv[2][c] - uv[0][c]);
        let ga = (du1 * b2 - du2 * b1) / det;
        let gb = (a1 * du2 - a2 * du1) / det;
        let g = add(mul(e1, ga), mul(e2, gb));
        (g, uv[0][c] - dot(g, p[0]))
    };
    let ((gu, cu), (gv, cv)) = (grad(0), grad(1));
    let (lu, lv) = (len(gu), len(gv));
    if !(lu > 1e-12 && lv > 1e-12) || len(cross(gu, gv)) <= 1e-4 * lu * lv {
        return None;
    }
    if ![cu, cv].iter().chain(gu.iter()).chain(gv.iter()).all(|x| x.is_finite()) {
        return None;
    }
    Some(UvMap { gu, gv, cu, cv })
}

fn axes_of(map: &UvMap, size: [f64; 2]) -> Option<TexAxes> {
    let (gu, gv) = (mul(map.gu, size[0]), mul(map.gv, size[1]));
    let (lu, lv) = (len(gu), len(gv));
    if !(lu > 0.0 && lv > 0.0) {
        return None;
    }
    let scale = [1.0 / lu, 1.0 / lv];
    if scale.iter().any(|s| !(1e-4..=1e6).contains(s)) {
        return None;
    }
    let (au, av) = (mul(gu, 1.0 / lu), mul(gv, 1.0 / lv));
    Some(TexAxes {
        axis_u: [au[0], au[1], au[2], map.cu * size[0]],
        axis_v: [av[0], av[1], av[2], map.cv * size[1]],
        scale,
    })
}

/// Hammer's default world projection for a face with normal `n` (dominant axis).
pub fn default_axes(n: V3) -> TexAxes {
    let a = [n[0].abs(), n[1].abs(), n[2].abs()];
    let (u, v) = if a[2] >= a[0] && a[2] >= a[1] {
        ([1.0, 0.0, 0.0], [0.0, -1.0, 0.0])
    } else if a[0] >= a[1] {
        ([0.0, 1.0, 0.0], [0.0, 0.0, -1.0])
    } else {
        ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0])
    };
    TexAxes {
        axis_u: [u[0], u[1], u[2], 0.0],
        axis_v: [v[0], v[1], v[2], 0.0],
        scale: [DEFAULT_SCALE, DEFAULT_SCALE],
    }
}

/// Newell normal (not normalised) of a loop.
fn newell(p: &[V3]) -> V3 {
    let mut n = [0.0; 3];
    for i in 0..p.len() {
        let (a, b) = (p[i], p[(i + 1) % p.len()]);
        n[0] += (a[1] - b[1]) * (a[2] + b[2]);
        n[1] += (a[2] - b[2]) * (a[0] + b[0]);
        n[2] += (a[0] - b[0]) * (a[1] + b[1]);
    }
    n
}

/// Build the faces of `scene` (see the module docs).
pub fn build(scene: &Scene, opts: &Options) -> Planar {
    let mut tris: Vec<Tri> = Vec::new();
    let mut degenerate = 0usize;
    for mesh in &scene.meshes {
        let world: Vec<V3> = mesh
            .positions
            .iter()
            .map(|p| {
                let c = mul(opts.up.to_cs2(p.map(f64::from)), opts.scale);
                let r = opts.rotation;
                add([dot(r[0], c), dot(r[1], c), dot(r[2], c)], opts.origin)
            })
            .collect();
        for t in mesh.indices.as_chunks::<3>().0 {
            let idx = [t[0] as usize, t[1] as usize, t[2] as usize];
            let (Some(&a), Some(&b), Some(&c)) = (world.get(idx[0]), world.get(idx[1]), world.get(idx[2])) else {
                degenerate += 1;
                continue;
            };
            let nv = cross(sub(b, a), sub(c, a));
            let k = [key(a), key(b), key(c)];
            if len(nv) < MIN_TWICE_AREA || k[0] == k[1] || k[1] == k[2] || k[0] == k[2] {
                degenerate += 1;
                continue;
            }
            let uv = idx.map(|i| mesh.uv0.get(i).map_or([0.0; 2], |u| [f64::from(u[0]), f64::from(u[1])]));
            let n = unit(nv);
            tris.push(Tri { p: [a, b, c], uv, mat: mesh.material, n, k, map: solve([a, b, c], uv, n) });
        }
    }

    // directed edge -> triangles having it
    let mut edges: HashMap<(Key, Key), Vec<usize>> = HashMap::new();
    for (i, t) in tris.iter().enumerate() {
        for e in 0..3 {
            edges.entry((t.k[e], t.k[(e + 1) % 3])).or_default().push(i);
        }
    }

    let mut used = vec![false; tris.len()];
    let mut faces = Vec::new();
    let mut defaulted_faces = 0usize;
    for seed in 0..tris.len() {
        if used[seed] {
            continue;
        }
        used[seed] = true;
        let mut region = vec![seed];
        let mut changed = true;
        while changed && region.len() < MAX_REGION {
            changed = false;
            let mut r = 0;
            while r < region.len() && region.len() < MAX_REGION {
                let t = region[r];
                for e in 0..3 {
                    let (a, b) = (tris[t].k[e], tris[t].k[(e + 1) % 3]);
                    let single = |list: Option<&Vec<usize>>| list.filter(|l| l.len() == 1).map(|l| l[0]);
                    if single(edges.get(&(a, b))).is_none() {
                        continue;
                    }
                    let Some(c) = single(edges.get(&(b, a))) else { continue };
                    if used[c] || !compatible(&tris[seed], &tris[c]) {
                        continue;
                    }
                    region.push(c);
                    let ok = boundary(&region, &tris)
                        .is_some_and(|l| l.len() <= MAX_LOOP && convex(&l, tris[seed].n));
                    if ok {
                        used[c] = true;
                        changed = true;
                    } else {
                        region.pop();
                    }
                    if region.len() >= MAX_REGION {
                        break;
                    }
                }
                r += 1;
            }
        }
        let s = &tris[seed];
        let mut corners = boundary(&region, &tris).unwrap_or_else(|| {
            (0..3).map(|i| (s.k[i], s.p[i], s.uv[i])).collect()
        });
        drop_collinear(&mut corners);
        let verts: Vec<V3> = corners.iter().map(|c| c.1).collect();
        let uvs: Vec<[f64; 2]> = corners.iter().map(|c| c.2).collect();
        let size = opts.tex_size(s.mat);
        let solved = s
            .map
            // the seed's map fits every corner by the merge rule; the refit through corner 0 can
            // extrapolate its tolerance on a long sliver, so fall back to the seed's map
            .and_then(|seed| solve_polygon(&verts, &uvs, s.n).or(Some(seed)))
            .and_then(|m| axes_of(&m, size));
        let (axes, defaulted) = match solved {
            Some(a) => (a, false),
            None => (default_axes(s.n), true),
        };
        defaulted_faces += usize::from(defaulted);
        faces.push(Face { material: s.mat, verts, uvs, tris: region.len(), axes, defaulted });
    }
    Planar { faces, tris: tris.len(), degenerate_tris: degenerate, defaulted_faces }
}

fn compatible(seed: &Tri, cand: &Tri) -> bool {
    if seed.mat != cand.mat || dot(seed.n, cand.n) < NORMAL_COS {
        return false;
    }
    if cand.p.iter().any(|&p| dot(sub(p, seed.p[0]), seed.n).abs() > PLANE_TOL) {
        return false;
    }
    match (&seed.map, &cand.map) {
        (None, None) => true,
        (Some(m), Some(_)) => (0..3).all(|i| {
            let uv = m.at(cand.p[i]);
            (uv[0] - cand.uv[i][0]).abs() <= UV_TOL && (uv[1] - cand.uv[i][1]).abs() <= UV_TOL
        }),
        _ => false,
    }
}

type Corner = (Key, V3, [f64; 2]);

/// The single outer boundary loop of the union of `region`, counter-clockwise, or `None` when it
/// is not one simple loop (a pinch, a hole, overlapping triangles).
fn boundary(region: &[usize], tris: &[Tri]) -> Option<Vec<Corner>> {
    let mut directed: HashMap<(Key, Key), usize> = HashMap::new();
    let mut at: HashMap<Key, (V3, [f64; 2])> = HashMap::new();
    for &t in region {
        let tri = &tris[t];
        for e in 0..3 {
            let (a, b) = (tri.k[e], tri.k[(e + 1) % 3]);
            if directed.insert((a, b), t).is_some() {
                return None;
            }
            at.entry(a).or_insert((tri.p[e], tri.uv[e]));
        }
    }
    let mut next: HashMap<Key, Key> = HashMap::new();
    for &(a, b) in directed.keys() {
        if !directed.contains_key(&(b, a)) && next.insert(a, b).is_some() {
            return None;
        }
    }
    // start at the seed's first corner when it is on the boundary (a lone triangle keeps its order)
    let seed = &tris[region[0]];
    let first = seed.k.iter().copied().find(|k| next.contains_key(k)).or_else(|| next.keys().min().copied())?;
    let mut out = Vec::new();
    let mut cur = first;
    loop {
        let (p, uv) = at[&cur];
        out.push((cur, p, uv));
        cur = *next.get(&cur)?;
        if cur == first {
            break;
        }
        if out.len() > next.len() {
            return None;
        }
    }
    (out.len() == next.len() && out.len() >= 3).then_some(out)
}

/// Every turn of the loop is left (or straight) about `n`.
fn convex(loop_: &[Corner], n: V3) -> bool {
    let c = loop_.len();
    (0..c).all(|i| {
        let (a, b, d) = (loop_[i].1, loop_[(i + 1) % c].1, loop_[(i + 2) % c].1);
        let (e1, e2) = (sub(b, a), sub(d, b));
        dot(cross(e1, e2), n) >= -COLLINEAR * len(e1) * len(e2)
    })
}

/// Remove corners that lie on the straight line between their neighbours.
fn drop_collinear(loop_: &mut Vec<Corner>) {
    let mut i = 0;
    while loop_.len() > 3 && i < loop_.len() {
        let c = loop_.len();
        let (a, b, d) = (loop_[(i + c - 1) % c].1, loop_[i].1, loop_[(i + 1) % c].1);
        let (e1, e2) = (sub(b, a), sub(d, b));
        if len(cross(e1, e2)) <= COLLINEAR * len(e1) * len(e2) && dot(e1, e2) > 0.0 {
            loop_.remove(i);
        } else {
            i += 1;
        }
    }
}

/// The map of a convex loop from its best-conditioned corner triple through corner 0.
fn solve_polygon(verts: &[V3], uvs: &[[f64; 2]], n: V3) -> Option<UvMap> {
    let mut best: Option<(f64, usize, usize)> = None;
    for j in 1..verts.len() - 1 {
        for k in j + 1..verts.len() {
            let a = len(cross(sub(verts[j], verts[0]), sub(verts[k], verts[0])));
            if best.is_none_or(|(b, _, _)| a > b) {
                best = Some((a, j, k));
            }
        }
    }
    let (_, j, k) = best?;
    let pn = unit(newell(verts));
    let pn = if len(pn) > 0.5 { pn } else { n };
    let map = solve([verts[0], verts[j], verts[k]], [uvs[0], uvs[j], uvs[k]], pn)?;
    // the loop must reproduce every corner (a merge kept them within UV_TOL of the seed's map)
    verts
        .iter()
        .zip(uvs)
        .all(|(p, uv)| {
            let at = map.at(*p);
            (at[0] - uv[0]).abs() <= 5e-5 && (at[1] - uv[1]).abs() <= 5e-5
        })
        .then_some(map)
}
