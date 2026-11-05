//! `import::planar` and `import::chunk` (I3): every emitted face is planar and convex, merges
//! only where the UV mapping agrees, and the computed texture axes reproduce the source UVs at
//! every corner (including rotated / sheared / mirrored mappings on non-axis-aligned planes).
use hammer_mcp::import::chunk::{self, Limits};
use hammer_mcp::import::gltf_read::{MeshData, Material, Scene};
use hammer_mcp::import::planar::{self, Face, Options};
use hammer_mcp::import::smd::UpAxis;

type V3 = [f64; 3];

fn material() -> Material {
    Material {
        name: "m".into(),
        base_color: [1.0; 4],
        metallic: 1.0,
        roughness: 1.0,
        base_color_tex: None,
        normal_tex: None,
        metal_rough_tex: None,
        dropped: vec![],
    }
}

fn scene(meshes: Vec<MeshData>, materials: usize) -> Scene {
    Scene { meshes, materials: (0..materials).map(|_| material()).collect(), dropped: vec![], warnings: vec![], rig: None }
}

fn mesh(positions: Vec<[f32; 3]>, uv0: Vec<[f32; 2]>, indices: Vec<u32>, material: usize) -> MeshData {
    let normals = vec![[0.0, 1.0, 0.0]; positions.len()];
    MeshData { name: "p".into(), positions, normals, uv0, indices, material, links: vec![] }
}

fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn len(a: V3) -> f64 {
    dot(a, a).sqrt()
}

/// A `n x n` grid of quads (two triangles each) on the plane `o + a*A + b*B` (glTF space), UV =
/// `m * (a, b) + t`. Shared vertices (a smooth, continuous mapping).
fn grid(o: V3, ax: V3, bx: V3, n: usize, m: [[f64; 2]; 2], t: [f64; 2], material: usize) -> MeshData {
    let mut positions = Vec::new();
    let mut uvs = Vec::new();
    for j in 0..=n {
        for i in 0..=n {
            let (a, b) = (i as f64, j as f64);
            positions.push([0, 1, 2].map(|k| (o[k] + a * ax[k] + b * bx[k]) as f32));
            uvs.push([(m[0][0] * a + m[0][1] * b + t[0]) as f32, (m[1][0] * a + m[1][1] * b + t[1]) as f32]);
        }
    }
    let mut idx = Vec::new();
    for j in 0..n {
        for i in 0..n {
            let q = (j * (n + 1) + i) as u32;
            let w = (n + 1) as u32;
            // counter-clockwise seen from A x B
            idx.extend([q, q + 1, q + w + 1, q, q + w + 1, q + w]);
        }
    }
    mesh(positions, uvs, idx, material)
}

fn check_face_invariants(f: &Face) {
    // planar: every vertex within 1e-3 of the Newell plane through corner 0
    let n = {
        let mut c = [0.0; 3];
        for i in 0..f.verts.len() {
            let (a, b) = (f.verts[i], f.verts[(i + 1) % f.verts.len()]);
            c[0] += (a[1] - b[1]) * (a[2] + b[2]);
            c[1] += (a[2] - b[2]) * (a[0] + b[0]);
            c[2] += (a[0] - b[0]) * (a[1] + b[1]);
        }
        let l = len(c);
        [c[0] / l, c[1] / l, c[2] / l]
    };
    for v in &f.verts {
        assert!(dot(sub(*v, f.verts[0]), n).abs() < 1e-3, "not planar: {f:?}");
    }
    // convex, counter-clockwise about the normal
    let c = f.verts.len();
    for i in 0..c {
        let (a, b, d) = (f.verts[i], f.verts[(i + 1) % c], f.verts[(i + 2) % c]);
        assert!(dot(cross(sub(b, a), sub(d, b)), n) > -1e-4 * len(sub(b, a)) * len(sub(d, b)), "concave or clockwise: {f:?}");
    }
    assert_eq!(f.verts.len(), f.uvs.len());
}

/// The axes reproduce the face's source UV at every corner (normalised, tolerance 1e-4).
fn check_uv(f: &Face, size: [f64; 2]) {
    for (p, uv) in f.verts.iter().zip(&f.uvs) {
        let got = f.axes.uv(*p, size);
        assert!((got[0] - uv[0]).abs() < 1e-4 && (got[1] - uv[1]).abs() < 1e-4, "uv {got:?} != {uv:?} for {f:?}");
    }
    let (u, v) = ([f.axes.axis_u[0], f.axes.axis_u[1], f.axes.axis_u[2]], [f.axes.axis_v[0], f.axes.axis_v[1], f.axes.axis_v[2]]);
    assert!(len(cross(u, v)) > 1e-6 * len(u) * len(v), "parallel axes");
    assert!(f.axes.scale.iter().all(|s| *s > 0.0 && s.is_finite()));
}

fn opts() -> Options {
    Options::new(39.37, UpAxis::Y)
}

#[test]
fn a_flat_grid_merges_into_convex_faces_with_exact_axes() {
    let m = mesh_grid_flat();
    let p = planar::build(&scene(vec![m], 1), &opts());
    assert_eq!(p.tris, 2 * 6 * 6);
    assert_eq!(p.degenerate_tris, 0);
    assert!(p.faces.len() < p.tris, "coplanar triangles merged: {} faces", p.faces.len());
    assert!(p.faces.iter().any(|f| f.verts.len() >= 4));
    assert_eq!(p.faces.iter().map(|f| f.tris).sum::<usize>(), p.tris, "every triangle in exactly one face");
    for f in &p.faces {
        check_face_invariants(f);
        check_uv(f, [512.0, 512.0]);
        assert!(!f.defaulted);
    }
}

fn mesh_grid_flat() -> MeshData {
    grid([0.0; 3], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0], 6, [[0.25, 0.0], [0.0, 0.25]], [0.0, 0.0], 0)
}

#[test]
fn rotated_sheared_mirrored_uvs_on_tilted_planes_are_reproduced() {
    let a = [1.0, 0.3, -0.2];
    let b = [-0.1, 0.4, 1.1];
    type Mapping = ([[f64; 2]; 2], [f64; 2], &'static str);
    let mappings: [Mapping; 5] = [
        ([[0.2, 0.0], [0.0, 0.2]], [0.1, 0.9], "plain"),
        ([[0.0, 0.2], [-0.2, 0.0]], [0.3, 0.3], "rotated 90"),
        ([[0.17, 0.09], [-0.09, 0.17]], [-1.5, 2.25], "rotated + offset beyond 0..1"),
        ([[0.3, 0.12], [0.0, 0.2]], [0.0, 0.0], "sheared"),
        ([[-0.25, 0.0], [0.0, 0.25]], [1.0, 0.0], "mirrored"),
    ];
    for (m, t, what) in mappings {
        for size in [[512.0, 512.0], [256.0, 1024.0]] {
            let g = grid([0.5, -0.25, 2.0], a, b, 5, m, t, 0);
            let mut o = opts();
            o.tex_sizes = vec![size];
            let p = planar::build(&scene(vec![g], 1), &o);
            assert!(!p.faces.is_empty());
            for f in &p.faces {
                check_face_invariants(f);
                check_uv(f, size);
                assert!(!f.defaulted, "{what}");
            }
            assert!(p.faces.len() < p.tris, "{what}: merged");
        }
    }
}

#[test]
fn the_axes_match_the_generating_mapping_independently_of_the_faces_own_uvs() {
    // oracle: the glTF generating function, evaluated at the CS2 corner mapped back to glTF space
    let (o, a, b) = ([0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.5]);
    let (m, t) = ([[0.1, 0.05], [-0.2, 0.15]], [0.25, 0.75]);
    let g = grid(o, a, b, 3, m, t, 0);
    let p = planar::build(&scene(vec![g], 1), &opts());
    let s = 39.37;
    for f in &p.faces {
        for v in &f.verts {
            let gl = [v[1] / s, v[2] / s, v[0] / s]; // inverse of (x,y,z) -> (z,x,y)
            // solve gl = o + u*a + w*b (least squares on the 2 basis vectors)
            let (aa, ab, bb) = (dot(a, a), dot(a, b), dot(b, b));
            let (ra, rb) = (dot(sub(gl, o), a), dot(sub(gl, o), b));
            let det = aa * bb - ab * ab;
            let (i, j) = ((ra * bb - rb * ab) / det, (aa * rb - ab * ra) / det);
            let want = [m[0][0] * i + m[0][1] * j + t[0], m[1][0] * i + m[1][1] * j + t[1]];
            let got = f.axes.uv(*v, [512.0, 512.0]);
            assert!((got[0] - want[0]).abs() < 1e-4 && (got[1] - want[1]).abs() < 1e-4, "{got:?} vs {want:?}");
        }
    }
}

#[test]
fn an_l_shape_is_never_one_face_and_every_face_is_convex() {
    // three quads in an L: a convex merge of all would be impossible
    let pos: Vec<[f32; 3]> = vec![
        [0., 0., 0.], [1., 0., 0.], [2., 0., 0.],
        [0., 0., -1.], [1., 0., -1.], [2., 0., -1.],
        [0., 0., -2.], [1., 0., -2.],
    ];
    let uv: Vec<[f32; 2]> = pos.iter().map(|p| [p[0] * 0.5, -p[2] * 0.5]).collect();
    // quads: (0,1,4,3) (1,2,5,4) (3,4,7,6) as CCW seen from +Y... emit triangles
    let q = |a: u32, b: u32, c: u32, d: u32| [a, d, c, a, c, b];
    let mut idx = Vec::new();
    idx.extend(q(0, 1, 4, 3));
    idx.extend(q(1, 2, 5, 4));
    idx.extend(q(3, 4, 7, 6));
    let p = planar::build(&scene(vec![mesh(pos, uv, idx, 0)], 1), &opts());
    assert!(p.faces.iter().all(|f| f.tris < 6), "the L (6 triangles) must not be one face");
    for f in &p.faces {
        check_face_invariants(f);
        check_uv(f, [512.0, 512.0]);
    }
}

#[test]
fn different_uv_mapping_or_material_across_an_edge_is_not_merged() {
    // two coplanar quads sharing an edge, the second mapped differently
    let pos: Vec<[f32; 3]> = vec![[0., 0., 0.], [1., 0., 0.], [1., 0., -1.], [0., 0., -1.], [2., 0., 0.], [2., 0., -1.]];
    let uv1: Vec<[f32; 2]> = vec![[0., 0.], [1., 0.], [1., 1.], [0., 1.], [3., 0.], [3., 1.]];
    let idx = vec![0, 1, 2, 0, 2, 3, 1, 4, 5, 1, 5, 2];
    let p = planar::build(&scene(vec![mesh(pos.clone(), uv1, idx.clone(), 0)], 1), &opts());
    assert_eq!(p.faces.len(), 2, "UV scale jump at the shared edge splits the faces");
    for f in &p.faces {
        assert_eq!(f.tris, 2);
        check_uv(f, [512.0, 512.0]);
    }
    // same UVs, two materials
    let uv2: Vec<[f32; 2]> = vec![[0., 0.], [1., 0.], [1., 1.], [0., 1.], [2., 0.], [2., 1.]];
    let a = mesh(pos.clone(), uv2.clone(), idx[..6].to_vec(), 0);
    let b = mesh(pos, uv2, idx[6..].to_vec(), 1);
    let p = planar::build(&scene(vec![a, b], 2), &opts());
    assert_eq!(p.faces.len(), 2);
    assert_ne!(p.faces[0].material, p.faces[1].material);
    // same material and mapping: one quad-pair merges into a single convex face
    let uv3: Vec<[f32; 2]> = vec![[0., 0.], [1., 0.], [1., 1.], [0., 1.], [2., 0.], [2., 1.]];
    let idx3 = vec![0, 1, 2, 0, 2, 3, 1, 4, 5, 1, 5, 2];
    let pos3: Vec<[f32; 3]> = vec![[0., 0., 0.], [1., 0., 0.], [1., 0., -1.], [0., 0., -1.], [2., 0., 0.], [2., 0., -1.]];
    let p = planar::build(&scene(vec![mesh(pos3, uv3, idx3, 0)], 1), &opts());
    assert_eq!(p.faces.len(), 1);
    assert_eq!(p.faces[0].verts.len(), 4, "collinear shared-edge corners dropped");
    check_uv(&p.faces[0], [512.0, 512.0]);
}

#[test]
fn zero_area_uvs_get_the_default_projection_and_are_counted() {
    let pos: Vec<[f32; 3]> = vec![[0., 0., 0.], [1., 0., 0.], [0., 0., -1.]];
    let p = planar::build(&scene(vec![mesh(pos, vec![[0.0; 2]; 3], vec![0, 1, 2], 0)], 1), &opts());
    assert_eq!(p.faces.len(), 1);
    assert!(p.faces[0].defaulted);
    assert_eq!(p.defaulted_faces, 1);
    let a = p.faces[0].axes;
    assert_eq!(a.scale, [0.25, 0.25]);
    // normal is +Z in CS2 (glTF +Y up): floor axes U=+x, V=-y
    assert_eq!(a.axis_u, [1.0, 0.0, 0.0, 0.0]);
    assert_eq!(a.axis_v, [0.0, -1.0, 0.0, 0.0]);
}

#[test]
fn degenerate_triangles_are_dropped_and_counted() {
    let pos: Vec<[f32; 3]> = vec![[0., 0., 0.], [1., 0., 0.], [2., 0., 0.], [0., 1., 0.]];
    let uv = vec![[0.0, 0.0], [1.0, 0.0], [2.0, 0.0], [0.0, 1.0]];
    // 0,1,2 are collinear: zero area
    let p = planar::build(&scene(vec![mesh(pos, uv, vec![0, 1, 2, 0, 1, 3], 0)], 1), &opts());
    assert_eq!(p.degenerate_tris, 1);
    assert_eq!(p.faces.len(), 1);
}

#[test]
fn axis_map_scale_rotation_and_origin_are_applied() {
    // the asymmetric corner triangle of the SMD tests: glTF (1,0,0),(0,2,0),(0,0,3) -> CS2 (0,1,0),(0,0,2),(3,0,0) * scale
    let pos: Vec<[f32; 3]> = vec![[1., 0., 0.], [0., 2., 0.], [0., 0., 3.]];
    let uv = vec![[0., 0.], [1., 0.], [0., 1.]];
    let sc = scene(vec![mesh(pos, uv, vec![0, 1, 2], 0)], 1);
    let mut o = Options::new(2.0, UpAxis::Y);
    let p = planar::build(&sc, &o);
    assert_eq!(p.faces[0].verts, vec![[0., 2., 0.], [0., 0., 4.], [6., 0., 0.]]);
    check_uv(&p.faces[0], [512.0, 512.0]);
    // yaw 90 about Z: (x, y) -> (-y, x); then origin
    o.rotation = hammer_mcp::vmap::mesh::angle_matrix([0.0, 90.0, 0.0]);
    o.origin = [100.0, 200.0, 300.0];
    let p = planar::build(&sc, &o);
    let v = &p.faces[0].verts;
    let want = [[98.0, 200.0, 300.0], [100.0, 200.0, 304.0], [100.0, 206.0, 300.0]];
    for (g, w) in v.iter().zip(want) {
        assert!(len(sub(*g, w)) < 1e-9, "{g:?} vs {w:?}");
    }
    check_uv(&p.faces[0], [512.0, 512.0]);
    // Z-up passes through unchanged
    let z = planar::build(&sc, &Options::new(1.0, UpAxis::Z));
    assert_eq!(z.faces[0].verts, vec![[1., 0., 0.], [0., 2., 0.], [0., 0., 3.]]);
}

// ------------------------------------------------------------------------------------------
// chunk
// ------------------------------------------------------------------------------------------

/// `count` separated unit triangles in a row, alternating over `materials`.
fn many(count: usize, materials: usize) -> Vec<Face> {
    let mut pos = Vec::new();
    let mut uv = Vec::new();
    let mut idx = Vec::new();
    let mut metas = Vec::new();
    for i in 0..count {
        let x = (i % 100) as f32 * 3.0;
        let y = (i / 100) as f32 * 3.0;
        let b = pos.len() as u32;
        pos.extend([[x, 0.0, y], [x + 1.0, 0.0, y], [x, 0.0, y - 1.0]]);
        uv.extend([[0., 0.], [1., 0.], [0., 1.]]);
        idx.extend([b, b + 1, b + 2]);
        metas.push(i % materials);
    }
    // one mesh per material so Scene material indices differ
    let mut meshes = Vec::new();
    for m in 0..materials {
        let mut ix = Vec::new();
        for (t, &mm) in metas.iter().enumerate() {
            if mm == m {
                ix.extend_from_slice(&idx[t * 3..t * 3 + 3]);
            }
        }
        meshes.push(mesh(pos.clone(), uv.clone(), ix, m));
    }
    planar::build(&scene(meshes, materials), &opts()).faces
}

#[test]
fn chunks_respect_every_limit_and_hold_each_face_once() {
    let faces = many(5000, 3);
    assert_eq!(faces.len(), 5000);
    let limits = Limits { max_tris: 300, max_request_bytes: 40_000 };
    let split = chunk::split(&faces, &limits);
    assert!(split.dropped.is_empty());
    assert!(split.chunks.len() >= 5000 / 300);
    let mut seen = vec![0u32; faces.len()];
    for c in &split.chunks {
        assert!(c.tris <= 300, "{} tris", c.tris);
        assert!(c.create_bytes <= limits.max_request_bytes && c.align_bytes <= limits.max_request_bytes);
        assert_eq!(c.faces.len(), c.face_ids.len());
        for (id, f) in c.face_ids.iter().zip(&c.faces) {
            seen[*id] += 1;
            assert_eq!(faces[*id].material, c.material, "one material per chunk");
            // the loop indexes the chunk's vertices: positions equal the (rounded) face corners
            for (k, vi) in f.iter().enumerate() {
                for a in 0..3 {
                    assert!((c.vertices[*vi][a] - faces[*id].verts[k][a]).abs() < 1e-4);
                }
            }
        }
        // the real request is under the cap too
        let req = serde_json::json!({"vertices": c.vertices, "faces": c.faces, "material": "materials/x.vmat"}).to_string();
        assert!(req.len() < limits.max_request_bytes);
    }
    assert!(seen.iter().all(|n| *n == 1), "every face exactly once");
}

#[test]
fn a_realistic_chunk_stays_under_the_bridge_budget() {
    let faces = many(3000, 1);
    let limits = Limits { max_tris: 2000, max_request_bytes: hammer_mcp::import::hammer_mesh::REQUEST_BUDGET };
    let split = chunk::split(&faces, &limits);
    assert_eq!(split.chunks.len(), 2, "3000 tris at 2000 per mesh");
    for c in &split.chunks {
        assert!(c.create_bytes < 512 * 1024 && c.align_bytes < 512 * 1024);
    }
    // chunks are spatially compact: the two halves do not interleave along the long axis
    let xs: Vec<(f64, f64)> = split
        .chunks
        .iter()
        .map(|c| c.vertices.iter().fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v[1]), hi.max(v[1]))))
        .collect();
    assert!(xs[0].1 <= xs[1].0 + 3.5 || xs[1].1 <= xs[0].0 + 3.5, "{xs:?}");
}

#[test]
fn faces_that_collapse_when_rounded_are_dropped_not_sent() {
    let mut faces = many(3, 1);
    // 1e-5 wide sliver: valid in f64, a line at 4 decimals
    faces.push(Face { verts: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 0.00001, 0.0]], uvs: vec![[0.0; 2]; 3], ..faces[0].clone() });
    let split = chunk::split(&faces, &Limits { max_tris: 2000, max_request_bytes: 400_000 });
    assert_eq!(split.dropped, vec![3]);
    assert_eq!(split.chunks.iter().map(|c| c.faces.len()).sum::<usize>(), 3);
}

#[test]
fn match_faces_pairs_by_position_whatever_the_order() {
    let tri = |x: f64| vec![[x, 0.0, 0.0], [x + 1.0, 0.0, 0.0], [x, 1.0, 0.0]];
    let planned = vec![tri(0.0), tri(5.0), tri(9.0)];
    let actual = vec![tri(9.00001), tri(0.0), vec![[7.0, 7.0, 7.0], [8.0, 7.0, 7.0], [7.0, 8.0, 7.0]], tri(5.0)];
    assert_eq!(chunk::match_faces(&planned, &actual), vec![Some(1), Some(3), Some(0)]);
    // nothing near -> None; a loop is used once
    assert_eq!(chunk::match_faces(&[tri(0.0), tri(0.0)], &[tri(0.0)]), vec![Some(0), None]);
    assert_eq!(chunk::match_faces(&[tri(100.0)], &[tri(0.0)]), vec![None]);
}

#[test]
fn match_faces_tells_crossed_and_back_to_back_faces_apart() {
    // an X billboard: two quads through the same centre in different planes
    let xz = vec![[-1.0, 0.0, -1.0], [1.0, 0.0, -1.0], [1.0, 0.0, 1.0], [-1.0, 0.0, 1.0]];
    let yz = vec![[0.0, -1.0, -1.0], [0.0, -1.0, 1.0], [0.0, 1.0, 1.0], [0.0, 1.0, -1.0]];
    assert_eq!(chunk::match_faces(&[xz.clone(), yz.clone()], &[yz.clone(), xz.clone()]), vec![Some(1), Some(0)]);
    // back to back: same corners, opposite winding; Hammer rotated the corner order of the first
    let back: Vec<_> = xz.iter().rev().copied().collect();
    let rotated = vec![xz[2], xz[3], xz[0], xz[1]];
    assert_eq!(chunk::match_faces(&[xz.clone(), back.clone()], &[back, rotated]), vec![Some(1), Some(0)]);
}

#[test]
fn json_numbers_for_the_wire_are_short_f32_decimals() {
    assert_eq!(chunk::f32_json(0.1f32 as f64), 0.1);
    assert_eq!(chunk::f32_json(1.0 / 3.0), 0.33333334);
}

#[test]
fn image_sizes_come_from_the_png_and_jpeg_headers() {
    use hammer_mcp::import::materials::image_size;
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
    png.extend(256u32.to_be_bytes());
    png.extend(128u32.to_be_bytes());
    assert_eq!(image_size(&png), Some((256, 128)));
    // JPEG: SOI, APP0 (16 bytes), SOF0 with 480 x 640
    let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    jpg.extend([0u8; 14]);
    jpg.extend([0xFF, 0xC0, 0x00, 0x11, 0x08]);
    jpg.extend(480u16.to_be_bytes());
    jpg.extend(640u16.to_be_bytes());
    jpg.extend([3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    assert_eq!(image_size(&jpg), Some((640, 480)));
    assert_eq!(image_size(b"not an image"), None);
    assert_eq!(image_size(&png[..18]), None);
}
