//! Pure polygon-mesh geometry: a 1:1 port of the Python POC's `geometry.py`
//! (`../idabins/worknotes/mcpserver/geometry.py`), the behaviour oracle for Hammer's mesh tools
//! (workflows/03-hammer-tools.md §6).
//!
//! A mesh is `(vertices, faces, face_materials)`:
//!
//! * `vertices`: world-space `[x, y, z]`;
//! * `faces`: vertex-index loops, **counter-clockwise seen from the visible side** — that is how
//!   Hammer orients faces: a box whose faces are listed this way is solid, the reverse renders
//!   inside-out (evidence `idabins/worknotes/evidence/2026-10-07-geometry-winding.png`);
//! * `face_materials`: one material path (or `null` = the mesh's base material) per face.
//!
//! Everything here returns new values; inputs are never mutated. There is no bridge and no I/O,
//! so every function is unit-testable (see the tests at the bottom, ported from the POC's
//! `tests/test_contract.py::test_geometry_unit_primitives_and_ops` plus the operator cases).
//!
//! Bounds follow `geometry.py` and the bridge's `geom_parse` (`bridge/mcp_bridge.c`, `GEO_MAX_*`):
//! at most [`MAX_VERTICES`] vertices, at most [`MAX_FACES`] faces, |coordinate| strictly below
//! [`MAX_COORD`], at least 3 vertices per face, integer indices without repeats, at most 64
//! distinct materials. Degenerate (zero-area) faces are refused by [`face_normal`], which every
//! tool calls before sending anything.
use serde_json::Value;
use std::fmt;

/// Vertices per mesh (bridge `GEO_MAX_VERTS`).
pub const MAX_VERTICES: usize = 65536;
/// Faces per mesh (bridge `GEO_MAX_FACES`).
pub const MAX_FACES: usize = 65536;
/// A coordinate's magnitude must be *strictly* below this (bridge: `c > -1e6 && c < 1e6`).
pub const MAX_COORD: f64 = 1e6;
/// Distinct materials per mesh (bridge `GEO_MAX_MATS`).
pub const MAX_MATERIALS: usize = 64;
/// A face normal shorter than this is a degenerate face (zero area).
const MIN_NORMAL: f64 = 1e-9;

/// `[x, y, z]` in world units.
pub type Vec3 = [f64; 3];

/// `geometry.py`'s `GeometryError`: every message is the POC's own text (the tools wrap it as
/// `InvalidArguments`, which the POC raised as a plain `ValueError` too).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeometryError(pub String);

impl fmt::Display for GeometryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GeometryError {}

fn err<T>(message: impl Into<String>) -> Result<T, GeometryError> {
    Err(GeometryError(message.into()))
}

/// A validated mesh: world vertices, index loops and the per-face materials ([`None`] = the
/// caller's base material, or `faceMaterials` was not given at all).
#[derive(Debug, Clone, PartialEq)]
pub struct Mesh {
    pub vertices: Vec<Vec3>,
    pub faces: Vec<Vec<usize>>,
    pub face_materials: Option<Vec<Option<String>>>,
}

/// A primitive (`box`, `wedge`, `cylinder`) plus its face names in face order (`["bottom", …]`,
/// what `create_block` reports as `faceNames`).
#[derive(Debug, Clone, PartialEq)]
pub struct Primitive {
    pub mesh: Mesh,
    pub names: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// validation
// ---------------------------------------------------------------------------------------------

/// `_num`: a finite JSON number (a bool is not a number, exactly like the POC's `isinstance`
/// check).
fn num(value: &Value) -> Option<f64> {
    value.as_f64().filter(|n| n.is_finite())
}

/// `validate(vertices, faces, face_materials, default_material=...)`: check shapes and ranges and
/// return the cleaned mesh. `vertices`/`faces` are the raw JSON values (absent = refused with the
/// POC's message); `face_materials` entries are trimmed, a `null` entry becomes
/// `default_material`.
pub fn validate(
    vertices: Option<&Value>,
    faces: Option<&Value>,
    face_materials: Option<&Value>,
    default_material: Option<&str>,
) -> Result<Mesh, GeometryError> {
    let Some(vertices) = vertices.and_then(Value::as_array) else {
        return err("vertices must be a list of at least 3 [x,y,z]");
    };
    if vertices.len() < 3 {
        return err("vertices must be a list of at least 3 [x,y,z]");
    }
    if vertices.len() > MAX_VERTICES {
        return err(format!("at most {MAX_VERTICES} vertices"));
    }
    let mut vs: Vec<Vec3> = Vec::with_capacity(vertices.len());
    for (i, vertex) in vertices.iter().enumerate() {
        let axes = vertex.as_array().filter(|axes| axes.len() == 3);
        let Some(axes) = axes.filter(|axes| axes.iter().all(|c| num(c).is_some())) else {
            return err(format!("vertices[{i}] must be [x,y,z] (finite numbers)"));
        };
        let point: Vec3 = [
            num(&axes[0]).expect("checked"),
            num(&axes[1]).expect("checked"),
            num(&axes[2]).expect("checked"),
        ];
        if point.iter().any(|c| c.abs() >= MAX_COORD) {
            return err(format!("vertices[{i}] is outside +-{MAX_COORD}"));
        }
        vs.push(point);
    }

    let Some(faces) = faces.and_then(Value::as_array) else {
        return err("faces must be a non-empty list of vertex-index loops");
    };
    if faces.is_empty() {
        return err("faces must be a non-empty list of vertex-index loops");
    }
    if faces.len() > MAX_FACES {
        return err(format!("at most {MAX_FACES} faces"));
    }
    let mut fs: Vec<Vec<usize>> = Vec::with_capacity(faces.len());
    for (i, face) in faces.iter().enumerate() {
        let Some(loop_) = face.as_array().filter(|loop_| loop_.len() >= 3) else {
            return err(format!("faces[{i}] needs at least 3 vertex indices"));
        };
        let mut out: Vec<usize> = Vec::with_capacity(loop_.len());
        for index in loop_ {
            // the POC requires an int: JSON `1.5` (and `true`) is refused, `3.0` as well
            let Some(index) = index.as_u64().map(|index| index as usize) else {
                return err(format!(
                    "faces[{i}] has an index outside 0..{}",
                    vs.len() - 1
                ));
            };
            if index >= vs.len() {
                return err(format!(
                    "faces[{i}] has an index outside 0..{}",
                    vs.len() - 1
                ));
            }
            out.push(index);
        }
        if out.iter().enumerate().any(|(a, index)| out[..a].contains(index)) {
            return err(format!("faces[{i}] repeats a vertex"));
        }
        fs.push(out);
    }

    let Some(face_materials) = face_materials else {
        return Ok(Mesh {
            vertices: vs,
            faces: fs,
            face_materials: None,
        });
    };
    let Some(face_materials) = face_materials.as_array() else {
        return err("faceMaterials must have one entry (string or null) per face");
    };
    if face_materials.len() != fs.len() {
        return err("faceMaterials must have one entry (string or null) per face");
    }
    let mut fm: Vec<Option<String>> = Vec::with_capacity(face_materials.len());
    for (i, material) in face_materials.iter().enumerate() {
        match material {
            Value::Null => fm.push(default_material.map(str::to_string)),
            Value::String(text) if !text.trim().is_empty() => {
                fm.push(Some(text.trim().to_string()))
            }
            _ => return err(format!("faceMaterials[{i}] must be a material path or null")),
        }
    }
    let distinct: std::collections::BTreeSet<&str> = fm.iter().flatten().map(String::as_str).collect();
    if distinct.len() > MAX_MATERIALS {
        return err(format!("at most {MAX_MATERIALS} distinct materials per mesh"));
    }
    Ok(Mesh {
        vertices: vs,
        faces: fs,
        face_materials: Some(fm),
    })
}

/// `material_params(face_materials, default_material)`: the bridge params' material half — one base
/// material (the most common one; ties keep the first seen, like Python's `max` over a dict) plus
/// per-face overrides (`None` = the base) when the mesh uses more than one. `None` as the override
/// list means "one material for every face", which is what `mesh.create`/`mesh.setGeometry` want
/// when the reply should carry no `faceMaterials` at all.
pub fn material_params(
    face_materials: Option<&[Option<String>]>,
    default_material: &str,
) -> (String, Option<Vec<Option<String>>>) {
    let Some(face_materials) = face_materials.filter(|fm| !fm.is_empty()) else {
        return (default_material.to_string(), None);
    };
    // insertion-ordered counts (Python dict order: the first material seen wins a tie)
    let mut counts: Vec<(String, usize)> = Vec::new();
    for material in face_materials {
        let key = material
            .clone()
            .unwrap_or_else(|| default_material.to_string());
        match counts.iter_mut().find(|(seen, _)| *seen == key) {
            Some((_, count)) => *count += 1,
            None => counts.push((key, 1)),
        }
    }
    let mut base = counts[0].0.clone();
    let mut best = 0;
    for (material, count) in &counts {
        if *count > best {
            best = *count;
            base = material.clone();
        }
    }
    if counts.len() == 1 {
        return (base, None);
    }
    let overrides: Vec<Option<String>> = face_materials
        .iter()
        .map(|material| {
            let material = material
                .clone()
                .unwrap_or_else(|| default_material.to_string());
            (material != base).then_some(material)
        })
        .collect();
    (base, Some(overrides))
}

// ---------------------------------------------------------------------------------------------
// vector helpers (POC `_sub`/`_add`/`_mul`/`_dot`/`_len`)
// ---------------------------------------------------------------------------------------------

fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn mul(a: Vec3, s: f64) -> Vec3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn dot(a: Vec3, b: Vec3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn length(a: Vec3) -> f64 {
    dot(a, a).sqrt()
}

/// The points of one face loop, checked against the vertex list (never panics on a bad index;
/// every tool validates first, so this only guards direct callers).
fn face_points(vertices: &[Vec3], face: &[usize]) -> Result<Vec<Vec3>, GeometryError> {
    face.iter()
        .map(|index| {
            vertices
                .get(*index)
                .copied()
                .ok_or_else(|| GeometryError(format!("face index {index} is outside the vertex list")))
        })
        .collect()
}

/// `face_normal`: the Newell normal (unit, `CCW -> visible side`). A zero-area face is refused —
/// Hammer would create nothing useful for it, so the tools reject it before dispatching.
pub fn face_normal(vertices: &[Vec3], face: &[usize]) -> Result<Vec3, GeometryError> {
    if face.is_empty() {
        return err("degenerate face (zero area)");
    }
    let mut n: Vec3 = [0.0, 0.0, 0.0];
    for i in 0..face.len() {
        let a = vertices
            .get(face[i])
            .copied()
            .ok_or_else(|| GeometryError(format!("face index {} is outside the vertex list", face[i])))?;
        let b = vertices
            .get(face[(i + 1) % face.len()])
            .copied()
            .ok_or_else(|| GeometryError(format!("face index {} is outside the vertex list", face[i])))?;
        n[0] += (a[1] - b[1]) * (a[2] + b[2]);
        n[1] += (a[2] - b[2]) * (a[0] + b[0]);
        n[2] += (a[0] - b[0]) * (a[1] + b[1]);
    }
    let ln = length(n);
    if ln < MIN_NORMAL {
        return err("degenerate face (zero area)");
    }
    Ok([n[0] / ln, n[1] / ln, n[2] / ln])
}

/// `centroid`: the mean of the points (`[0,0,0]` for an empty list, where the POC's `max(1, 0)`
/// divides by one).
pub fn centroid(vertices: &[Vec3]) -> Vec3 {
    let n = vertices.len().max(1) as f64;
    let mut out: Vec3 = [0.0, 0.0, 0.0];
    for vertex in vertices {
        out = add(out, *vertex);
    }
    [out[0] / n, out[1] / n, out[2] / n]
}

/// `bounds`: the axis-aligned min/max (the POC's `min`/`max` over the points; an empty list has no
/// bounds, so both ends read as the origin).
pub fn bounds(vertices: &[Vec3]) -> (Vec3, Vec3) {
    let mut mn: Vec3 = [0.0, 0.0, 0.0];
    let mut mx: Vec3 = [0.0, 0.0, 0.0];
    for (i, vertex) in vertices.iter().enumerate() {
        if i == 0 {
            mn = *vertex;
            mx = *vertex;
            continue;
        }
        for axis in 0..3 {
            mn[axis] = mn[axis].min(vertex[axis]);
            mx[axis] = mx[axis].max(vertex[axis]);
        }
    }
    (mn, mx)
}

/// `orient_outward`: for a closed convex-ish solid, flip every face whose normal points *toward*
/// the centroid. Returns the new loops and how many were flipped.
pub fn orient_outward(
    vertices: &[Vec3],
    faces: &[Vec<usize>],
) -> Result<(Vec<Vec<usize>>, usize), GeometryError> {
    let center = centroid(vertices);
    let mut out: Vec<Vec<usize>> = Vec::with_capacity(faces.len());
    let mut flipped = 0usize;
    for face in faces {
        let points = face_points(vertices, face)?;
        let face_center = centroid(&points);
        if dot(face_normal(vertices, face)?, sub(face_center, center)) < 0.0 {
            let mut reversed = face.clone();
            reversed.reverse();
            out.push(reversed);
            flipped += 1;
        } else {
            out.push(face.clone());
        }
    }
    Ok((out, flipped))
}

// ---------------------------------------------------------------------------------------------
// primitives (all oriented outward)
// ---------------------------------------------------------------------------------------------

/// `box`: an axis-aligned box. Face order: bottom, top, -y, +y, -x, +x. (`r#box`: `box` is a
/// reserved word in Rust, so the POC name is kept as a raw identifier.)
pub fn r#box(mins: Vec3, maxs: Vec3) -> Result<Primitive, GeometryError> {
    let [x0, y0, z0] = mins;
    let [x1, y1, z1] = maxs;
    if !(x1 > x0 && y1 > y0 && z1 > z0) {
        return err("box: max must be greater than min on every axis");
    }
    let v = vec![
        [x0, y0, z0],
        [x1, y0, z0],
        [x0, y1, z0],
        [x1, y1, z0],
        [x0, y0, z1],
        [x1, y0, z1],
        [x0, y1, z1],
        [x1, y1, z1],
    ];
    let f = vec![
        vec![0, 2, 3, 1],
        vec![4, 5, 7, 6],
        vec![0, 1, 5, 4],
        vec![2, 6, 7, 3],
        vec![0, 4, 6, 2],
        vec![1, 3, 7, 5],
    ];
    let (faces, _) = orient_outward(&v, &f)?;
    Ok(Primitive {
        mesh: Mesh {
            vertices: v,
            faces,
            face_materials: None,
        },
        names: ["bottom", "top", "-y", "+y", "-x", "+x"]
            .iter()
            .map(|name| name.to_string())
            .collect(),
    })
}

/// `wedge`: a ramp inside `mins..maxs` — full height at the `rise_toward` end, zero at the other.
/// Faces: bottom, two side triangles, high end, slope (oriented outward).
pub fn wedge(mins: Vec3, maxs: Vec3, rise_toward: &str) -> Result<Primitive, GeometryError> {
    let [x0, y0, z0] = mins;
    let [x1, y1, z1] = maxs;
    if !(x1 > x0 && y1 > y0 && z1 > z0) {
        return err("wedge: max must be greater than min on every axis");
    }
    if !matches!(rise_toward, "+x" | "-x" | "+y" | "-y") {
        return err("wedge: riseToward must be +x, -x, +y or -y");
    }
    // a: 0 low end .. 1 high end along the rise axis; b: 0..1 across
    let pt = |a: usize, b: usize, top: bool| -> Vec3 {
        let (x, y) = match rise_toward {
            "+x" => ([x0, x1][a], [y0, y1][b]),
            "-x" => ([x1, x0][a], [y0, y1][b]),
            "+y" => ([x0, x1][b], [y0, y1][a]),
            _ => ([x0, x1][b], [y1, y0][a]),
        };
        [x, y, if top { z1 } else { z0 }]
    };
    let v = vec![
        pt(0, 0, false),
        pt(1, 0, false),
        pt(0, 1, false),
        pt(1, 1, false),
        pt(1, 0, true),
        pt(1, 1, true),
    ];
    let f = vec![
        vec![0, 2, 3, 1],
        vec![0, 1, 4],
        vec![2, 5, 3],
        vec![1, 3, 5, 4],
        vec![0, 4, 5, 2],
    ];
    let (faces, _) = orient_outward(&v, &f)?;
    Ok(Primitive {
        mesh: Mesh {
            vertices: v,
            faces,
            face_materials: None,
        },
        names: ["bottom", "side_a", "side_b", "high", "slope"]
            .iter()
            .map(|name| name.to_string())
            .collect(),
    })
}

/// `cylinder`: a Z-up prism with `sides` segments (3..=64) standing on `center_bottom`.
pub fn cylinder(
    center_bottom: Vec3,
    radius: f64,
    height: f64,
    sides: usize,
) -> Result<Primitive, GeometryError> {
    if !(radius > 0.0 && height > 0.0) {
        return err("cylinder: radius and height must be > 0");
    }
    if !(3..=64).contains(&sides) {
        return err("cylinder: sides must be 3..64");
    }
    let [cx, cy, cz] = center_bottom;
    let mut v: Vec<Vec3> = Vec::with_capacity(sides * 2);
    for z in [cz, cz + height] {
        for i in 0..sides {
            let a = 2.0 * std::f64::consts::PI * i as f64 / sides as f64;
            v.push([cx + radius * a.cos(), cy + radius * a.sin(), z]);
        }
    }
    let bottom: Vec<usize> = (0..sides).rev().collect();
    let top: Vec<usize> = (0..sides).map(|i| sides + i).collect();
    let mut f: Vec<Vec<usize>> = vec![bottom, top];
    let mut names: Vec<String> = vec!["bottom".to_string(), "top".to_string()];
    for i in 0..sides {
        let j = (i + 1) % sides;
        f.push(vec![i, j, sides + j, sides + i]);
        names.push(format!("side{i}"));
    }
    let (faces, _) = orient_outward(&v, &f)?;
    Ok(Primitive {
        mesh: Mesh {
            vertices: v,
            faces,
            face_materials: None,
        },
        names,
    })
}

// ---------------------------------------------------------------------------------------------
// operations
// ---------------------------------------------------------------------------------------------

/// `rotate_z`: yaw `degrees` about `pivot` (Z is untouched).
pub fn rotate_z(vertices: &[Vec3], degrees: f64, pivot: Vec3) -> Vec<Vec3> {
    let a = degrees.to_radians();
    let (c, s) = (a.cos(), a.sin());
    vertices
        .iter()
        .map(|p| {
            let x = p[0] - pivot[0];
            let y = p[1] - pivot[1];
            [
                pivot[0] + c * x - s * y,
                pivot[1] + s * x + c * y,
                p[2],
            ]
        })
        .collect()
}

/// `transform`: scale about `pivot`, then yaw-rotate about `pivot`, then translate. `pivot` defaults
/// to the bounds centre. The POC's `scale` is a number or `[sx, sy, sz]`; the tool normalises it to
/// three axes before calling this.
pub fn transform(
    vertices: &[Vec3],
    translate: Option<Vec3>,
    rotate_yaw: f64,
    scale: Option<Vec3>,
    pivot: Option<Vec3>,
) -> Result<Vec<Vec3>, GeometryError> {
    let pivot = match pivot {
        Some(pivot) => pivot,
        None => {
            let (mn, mx) = bounds(vertices);
            [(mn[0] + mx[0]) / 2.0, (mn[1] + mx[1]) / 2.0, (mn[2] + mx[2]) / 2.0]
        }
    };
    let mut out: Vec<Vec3> = vertices.to_vec();
    if let Some(scale) = scale {
        if scale.iter().any(|s| !s.is_finite() || *s == 0.0) {
            return err("scale must be non-zero numbers");
        }
        out = out
            .iter()
            .map(|p| {
                [
                    pivot[0] + (p[0] - pivot[0]) * scale[0],
                    pivot[1] + (p[1] - pivot[1]) * scale[1],
                    pivot[2] + (p[2] - pivot[2]) * scale[2],
                ]
            })
            .collect();
    }
    if rotate_yaw != 0.0 {
        out = rotate_z(&out, rotate_yaw, pivot);
    }
    if let Some(translate) = translate {
        out = out.iter().map(|p| add(*p, translate)).collect();
    }
    Ok(out)
}

/// `mirrored`: a negative scale on an odd number of axes flips the winding (the caller then flips
/// every face back).
pub fn mirrored(scale: Vec3) -> bool {
    scale.iter().filter(|s| **s < 0.0).count() % 2 == 1
}

/// `extrude_face`'s result: the new vertices, faces and per-face materials (fully resolved).
pub type Extrusion = (Vec<Vec3>, Vec<Vec<usize>>, Vec<Option<String>>);

/// `extrude_face`: push face `face_index` out along its normal by `distance`, adding one quad per
/// edge. A negative distance pushes inward (a recess). The cap keeps its face slot and material,
/// the new side quads take `side_material` (or the cap's). Returns the new mesh lists with
/// `face_materials` fully resolved ([`None`] entries = the caller's base material).
pub fn extrude_face(
    vertices: &[Vec3],
    faces: &[Vec<usize>],
    face_materials: Option<&[Option<String>]>,
    face_index: usize,
    distance: f64,
    side_material: Option<&str>,
) -> Result<Extrusion, GeometryError> {
    if face_index >= faces.len() {
        return err(format!("face index outside 0..{}", faces.len() as i64 - 1));
    }
    if !distance.is_finite() || distance == 0.0 {
        return err("distance must be a non-zero number");
    }
    let loop_ = faces[face_index].clone();
    let normal = face_normal(vertices, &loop_)?;
    let off = mul(normal, distance);
    let mut verts: Vec<Vec3> = vertices.to_vec();
    let mut new_idx: Vec<usize> = Vec::with_capacity(loop_.len());
    for i in &loop_ {
        let point = *vertices
            .get(*i)
            .ok_or_else(|| GeometryError(format!("face index {i} is outside the vertex list")))?;
        new_idx.push(verts.len());
        verts.push(add(point, off));
    }
    let mut new_faces: Vec<Vec<usize>> = faces.to_vec();
    let mut mats: Vec<Option<String>> = match face_materials {
        Some(mats) if !mats.is_empty() => mats.to_vec(),
        _ => vec![None; faces.len()],
    };
    let cap_mat = mats.get(face_index).cloned().flatten();
    // the POC's `side_material or cap_mat`: an empty string is falsy there, so it falls back to the
    // cap's material too (review N3 — `.map(..).or(cap_mat)` would keep `Some("")`)
    let side = side_material
        .filter(|material| !material.trim().is_empty())
        .map(str::to_string)
        .or(cap_mat);
    new_faces[face_index] = new_idx.clone();
    let k = loop_.len();
    for e in 0..k {
        let (a, b) = (loop_[e], loop_[(e + 1) % k]);
        let (na, nb) = (new_idx[e], new_idx[(e + 1) % k]);
        let mut quad = vec![a, b, nb, na];
        if distance < 0.0 {
            quad.reverse();
        }
        new_faces.push(quad);
        mats.push(side.clone());
    }
    Ok((verts, new_faces, mats))
}

/// `move_vertices`: offset the listed vertex indices by `offset` (the POC's "drag the top edge of a
/// box to make a slope").
pub fn move_vertices(
    vertices: &[Vec3],
    indices: &[usize],
    offset: Vec3,
) -> Result<Vec<Vec3>, GeometryError> {
    if indices.is_empty() || indices.iter().any(|index| *index >= vertices.len()) {
        return err(format!(
            "vertex indices must be in 0..{}",
            vertices.len() as i64 - 1
        ));
    }
    if offset.iter().any(|c| !c.is_finite()) {
        return err("offset must be [dx, dy, dz]");
    }
    // review-2 L5: a flat lookup table instead of `indices.contains` per vertex — the POC's
    // duplicate-as-a-set reading is a `Vec<bool>` mask away, and a 65k-vertex mesh stops costing
    // O(vertices x indices).
    let mut picked = vec![false; vertices.len()];
    for index in indices {
        picked[*index] = true;
    }
    Ok(vertices
        .iter()
        .zip(picked)
        .map(|(point, picked)| if picked { add(*point, offset) } else { *point })
        .collect())
}

/// `delete_faces`: drop the faces at `indices` and the vertices they left unused; the face indices
/// are renumbered (callers say so). Deleting every face is refused — delete the node instead.
pub fn delete_faces(
    vertices: &[Vec3],
    faces: &[Vec<usize>],
    face_materials: Option<&[Option<String>]>,
    indices: &[usize],
) -> Result<Mesh, GeometryError> {
    let mut drop: Vec<usize> = indices.to_vec();
    drop.sort_unstable();
    drop.dedup();
    if drop.is_empty() || drop.iter().any(|i| *i >= faces.len()) {
        return err(format!("face indices must be in 0..{}", faces.len() as i64 - 1));
    }
    if drop.len() >= faces.len() {
        return err("that would delete every face; delete the node instead");
    }
    let mut drop_mask = vec![false; faces.len()];
    for index in drop {
        drop_mask[index] = true;
    }
    // review-2 L5: the keep filter and the renumbering run off lookup tables, not the quadratic
    // `contains`/linear `find` the POC's list code did (a 20k-face terrain mesh through
    // delete_mesh_faces used to cost order-1e9 comparisons per call)
    let keep: Vec<usize> = (0..faces.len()).filter(|i| !drop_mask[*i]).collect();
    let mut used: Vec<usize> = keep.iter().flat_map(|i| faces[*i].clone()).collect();
    used.sort_unstable();
    used.dedup();
    let mut remap = vec![usize::MAX; used.last().map_or(0, |m| *m + 1)];
    for (new, old) in used.iter().enumerate() {
        remap[*old] = new;
    }
    let verts: Vec<Vec3> = used.iter().map(|i| vertices[*i]).collect();
    let fs: Vec<Vec<usize>> = keep
        .iter()
        .map(|i| {
            faces[*i]
                .iter()
                .map(|v| {
                    remap
                        .get(*v)
                        .copied()
                        .filter(|new| *new != usize::MAX)
                        .expect("every kept face index is in `used`")
                })
                .collect()
        })
        .collect();
    let mats = face_materials.map(|mats| keep.iter().map(|i| mats[*i].clone()).collect());
    Ok(Mesh {
        vertices: verts,
        faces: fs,
        face_materials: mats,
    })
}

/// `flip_faces`: reverse the listed loops (`None` = every face).
pub fn flip_faces(faces: &[Vec<usize>], indices: Option<&[usize]>) -> Vec<Vec<usize>> {
    faces
        .iter()
        .enumerate()
        .map(|(i, face)| {
            let hit = match indices {
                None => true,
                Some(indices) => indices.contains(&i),
            };
            if hit {
                let mut reversed = face.clone();
                reversed.reverse();
                reversed
            } else {
                face.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v3(points: &[[f64; 3]]) -> Vec<Vec3> {
        points.to_vec()
    }

    fn json_vertices(vertices: &[Vec3]) -> Value {
        Value::Array(
            vertices
                .iter()
                .map(|p| json!([p[0], p[1], p[2]]))
                .collect(),
        )
    }

    fn json_faces(faces: &[Vec<usize>]) -> Value {
        Value::Array(faces.iter().map(|f| json!(f)).collect())
    }

    /// `tests/test_contract.py::test_geometry_unit_primitives_and_ops` plus the operator cases.
    #[test]
    fn primitives_are_oriented_outward_and_the_box_winding_is_the_live_one() {
        let prims = [
            r#box([0.0, 0.0, 0.0], [64.0, 32.0, 16.0]).unwrap(),
            wedge([0.0, 0.0, 0.0], [256.0, 64.0, 32.0], "-y").unwrap(),
            cylinder([0.0, 0.0, 0.0], 32.0, 64.0, 8).unwrap(),
        ];
        for prim in &prims {
            assert_eq!(
                orient_outward(&prim.mesh.vertices, &prim.mesh.faces).unwrap().1,
                0,
                "primitives are built outward already: {:?}",
                prim.names
            );
        }

        // the winding verified live (worknotes/evidence/2026-10-07-geometry-winding.png)
        let b = r#box([256.0, -512.0, 0.0], [384.0, -384.0, 128.0]).unwrap();
        assert_eq!(
            b.mesh.faces,
            vec![
                vec![0, 2, 3, 1],
                vec![4, 5, 7, 6],
                vec![0, 1, 5, 4],
                vec![2, 6, 7, 3],
                vec![0, 4, 6, 2],
                vec![1, 3, 7, 5],
            ]
        );
        assert_eq!(
            b.names,
            vec!["bottom", "top", "-y", "+y", "-x", "+x"]
        );
        // every face normal points away from the centre
        let center = centroid(&b.mesh.vertices);
        for face in &b.mesh.faces {
            let points: Vec<Vec3> = face.iter().map(|i| b.mesh.vertices[*i]).collect();
            assert!(dot(face_normal(&b.mesh.vertices, face).unwrap(), sub(centroid(&points), center)) > 0.0);
        }
    }

    #[test]
    fn extrude_delete_and_transform_match_the_poc_numbers() {
        let b = r#box([256.0, -512.0, 0.0], [384.0, -384.0, 128.0]).unwrap();
        let (v, f) = (&b.mesh.vertices, &b.mesh.faces);
        let fm = vec![Some("a".to_string()); 6];
        let (v2, f2, _) = extrude_face(v, f, Some(&fm), 1, 64.0, None).unwrap();
        assert_eq!((v2.len(), f2.len()), (12, 10));
        assert_eq!(orient_outward(&v2, &f2).unwrap().1, 0);
        assert_eq!(max_axis(&v2, 2), 192.0);

        let mesh = delete_faces(v, f, Some(&fm), &[1]).unwrap();
        assert_eq!((mesh.vertices.len(), mesh.faces.len()), (8, 5));
        assert_eq!(mesh.face_materials.as_ref().expect("materials").len(), 5);
        assert!(delete_faces(v, f, None, &[0, 1, 2, 3, 4, 5]).is_err());

        let moved = transform(v, Some([10.0, 0.0, 0.0]), 90.0, None, None).unwrap();
        assert!((centroid(&moved)[0] - (centroid(v)[0] + 10.0)).abs() < 1e-4);
        // yaw 90 about the bounds centre really is a yaw (z is untouched)
        assert_eq!(
            moved.iter().map(|p| p[2]).collect::<Vec<f64>>(),
            v.iter().map(|p| p[2]).collect::<Vec<f64>>()
        );

        // mirrored: an odd number of negative axes flips the winding
        assert!(mirrored([-1.0, 1.0, 1.0]));
        assert!(!mirrored([-1.0, -1.0, 1.0]));
        assert!(!mirrored([1.0, 1.0, 1.0]));
        assert_eq!(
            flip_faces(f, None)[0],
            f[0].iter().copied().rev().collect::<Vec<usize>>()
        );
        assert_eq!(flip_faces(f, Some(&[1]))[0], f[0]);
    }

    fn max_axis(vertices: &[Vec3], axis: usize) -> f64 {
        vertices
            .iter()
            .map(|p| p[axis])
            .fold(f64::NEG_INFINITY, f64::max)
    }

    #[test]
    fn material_params_picks_the_most_common_base_and_keeps_first_on_a_tie() {
        assert_eq!(
            material_params(None, "a"),
            ("a".to_string(), None),
            "no faceMaterials -> one base material"
        );
        assert_eq!(material_params(Some(&[]), "a"), ("a".to_string(), None));
        assert_eq!(
            material_params(Some(&[Some("a".into()), Some("a".into())]), "a"),
            ("a".to_string(), None)
        );
        // the POC's contract test: ["a", "a", "b"] with base "a"
        let (base, overrides) = material_params(
            Some(&[Some("a".into()), Some("a".into()), Some("b".into())]),
            "a",
        );
        assert_eq!(base, "a");
        assert_eq!(
            overrides,
            Some(vec![None, None, Some("b".to_string())])
        );
        // a tie keeps the first material seen (Python's max over a dict); null entries are the base
        let (base, overrides) = material_params(Some(&[None, Some("b".into())]), "a");
        assert_eq!(base, "a");
        assert_eq!(overrides, Some(vec![None, Some("b".to_string())]));
        let (base, overrides) = material_params(Some(&[Some("b".into()), None]), "a");
        assert_eq!(base, "b");
        assert_eq!(overrides, Some(vec![None, Some("a".to_string())]));
    }

    #[test]
    fn validate_cleans_trims_and_resolves_face_materials() {
        let vertices = json!([[0, 0, 0], [1, 0, 0], [0, 1, 0]]);
        let faces = json!([[0, 1, 2]]);
        let mesh = validate(
            Some(&vertices),
            Some(&faces),
            Some(&json!([null])),
            Some("materials/dev/reflectivity_30.vmat"),
        )
        .unwrap();
        assert_eq!(mesh.vertices[0], [0.0, 0.0, 0.0]);
        assert_eq!(
            mesh.face_materials,
            Some(vec![Some("materials/dev/reflectivity_30.vmat".to_string())])
        );
        let mesh = validate(
            Some(&vertices),
            Some(&faces),
            Some(&json!([" materials/x.vmat "])),
            None,
        )
        .unwrap();
        assert_eq!(mesh.face_materials, Some(vec![Some("materials/x.vmat".into())]));
        // without faceMaterials the field stays absent
        let mesh = validate(Some(&vertices), Some(&faces), None, Some("m")).unwrap();
        assert_eq!(mesh.face_materials, None);
    }

    #[test]
    fn validate_refuses_what_the_bridge_would_refuse() {
        // the POC contract test's bad case: two vertices cannot carry a face
        assert!(validate(
            Some(&json!([[0, 0, 0], [1, 0, 0]])),
            Some(&json!([[0, 1, 0]])),
            None,
            None
        )
        .is_err());
        for (vertices, faces) in [
            (json!([[0, 0, 0], [1, 0, 0], [0, 1, 0]]), json!([[0, 1, 5]])),
            (json!([[0, 0, 0], [1, 0, 0], [0, 1, 0]]), json!([[0, 1]])),
            (json!([[0, 0, 0], [1, 0, 0], [0, 1, 0]]), json!([[0, 1, 1]])),
            (json!([[0, 0, 0], [1, 0, 0], [0, 1, 0]]), json!([[0, 1, 1.5]])),
            (json!([[0, 0, 0], [1, 0, 0], [0, 1, 0]]), json!([[0, 1, true]])),
            (json!([[0, 0, 0], [1, 0, 0], [1e6, 0, 0]]), json!([[0, 1, 2]])),
            (json!([[0, 0, 0], [1, 0, 0], [0, 1, "2"]]), json!([[0, 1, 2]])),
            (json!([[0, 0, 0], [1, 0, 0], [0, 1, null]]), json!([[0, 1, 2]])),
            (json!([[0, 0, 0], [1, 0, 0], [0, 1, 0]]), json!([])),
            (json!([[0, 0, 0], [1, 0, 0], [0, 1, 0]]), json!("x")),
        ] {
            assert!(
                validate(Some(&vertices), Some(&faces), None, None).is_err(),
                "{vertices} / {faces}"
            );
        }
        // missing vertices/faces are the same refusal, never a panic
        assert!(validate(None, Some(&json!([[0, 1, 2]])), None, None).is_err());
        assert!(validate(Some(&json!([[0, 0, 0], [1, 0, 0], [0, 1, 0]])), None, None, None).is_err());
        // one face material too few / one too many
        let vertices = json!([[0, 0, 0], [1, 0, 0], [0, 1, 0]]);
        let faces = json!([[0, 1, 2]]);
        assert!(validate(Some(&vertices), Some(&faces), Some(&json!([])), None).is_err());
        assert!(
            validate(
                Some(&vertices),
                Some(&faces),
                Some(&json!(["a", "b"])),
                None
            )
            .is_err()
        );
        assert!(
            validate(
                Some(&vertices),
                Some(&faces),
                Some(&json!(["  "])),
                None
            )
            .is_err()
        );
        // at most 64 distinct materials
        let vertices = json!([
            [0, 0, 0],
            [1, 0, 0],
            [0, 1, 0],
            [1, 1, 0],
            [0, 0, 1],
            [1, 0, 1],
            [0, 1, 1],
            [1, 1, 1]
        ]);
        let faces = json!([[0, 2, 3, 1], [4, 5, 7, 6]]);
        let many: Vec<String> = (0..=MAX_MATERIALS).map(|i| format!("m{i}.vmat")).collect();
        assert!(
            validate(
                Some(&vertices),
                Some(&faces),
                Some(&json!(many)),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn validate_bounds_and_limits() {
        // 65536 vertices pass, 65537 do not
        let at_cap: Vec<Vec3> = (0..MAX_VERTICES).map(|i| [i as f64, 0.0, 0.0]).collect();
        let faces: Vec<Vec<usize>> = vec![vec![0, 1, 2]];
        let mesh = validate(
            Some(&json_vertices(&at_cap)),
            Some(&json_faces(&faces)),
            None,
            None,
        )
        .unwrap();
        assert_eq!(mesh.vertices.len(), MAX_VERTICES);
        let over: Vec<Vec3> = (0..=MAX_VERTICES).map(|i| [i as f64, 0.0, 0.0]).collect();
        assert!(validate(
            Some(&json_vertices(&over)),
            Some(&json_faces(&faces)),
            None,
            None
        )
        .is_err());
        // |coordinate| must be strictly below 1e6 (the bridge reads `c > -1e6 && c < 1e6`)
        let ok = validate(
            Some(&json!([[-999999.0, 0, 0], [1, 0, 0], [0, 1, 0]])),
            Some(&json!([[0, 1, 2]])),
            None,
            None,
        );
        assert!(ok.is_ok());
        let (mn, mx) = bounds(&ok.unwrap().vertices);
        assert_eq!((mn, mx), ([-999999.0, 0.0, 0.0], [1.0, 1.0, 0.0]));
        assert!(validate(
            Some(&json!([[1e6, 0, 0], [1, 0, 0], [0, 1, 0]])),
            Some(&json!([[0, 1, 2]])),
            None,
            None
        )
        .is_err());
    }

    #[test]
    fn face_normal_is_a_unit_newell_normal_and_refuses_degenerates() {
        let vertices = v3(&[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
        let n = face_normal(&vertices, &[0, 1, 2]).unwrap();
        assert_eq!(n, [0.0, 0.0, 1.0], "CCW seen from +z");
        let flipped = face_normal(&vertices, &[2, 1, 0]).unwrap();
        assert_eq!(flipped, [0.0, 0.0, -1.0]);
        // a zero-area face (three collinear points) is refused
        let flat = v3(&[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]]);
        assert_eq!(
            face_normal(&flat, &[0, 1, 2]).unwrap_err().0,
            "degenerate face (zero area)"
        );
        assert!(face_normal(&vertices, &[]).is_err());
        // an out-of-range index is an error, never a panic
        assert!(face_normal(&vertices, &[0, 1, 9]).is_err());
    }

    #[test]
    fn primitive_bounds_are_checked_and_the_cylinder_takes_3_to_64_sides() {
        assert!(r#box([0.0, 0.0, 0.0], [0.0, 10.0, 10.0]).is_err());
        assert!(wedge([0.0, 0.0, 0.0], [0.0, 10.0, 10.0], "+x").is_err());
        assert!(wedge([0.0, 0.0, 0.0], [1.0, 1.0, 1.0], "up").is_err());
        assert!(cylinder([0.0, 0.0, 0.0], 0.0, 1.0, 16).is_err());
        assert!(cylinder([0.0, 0.0, 0.0], 1.0, 0.0, 16).is_err());
        assert!(cylinder([0.0, 0.0, 0.0], 1.0, 1.0, 2).is_err());
        assert!(cylinder([0.0, 0.0, 0.0], 1.0, 1.0, 65).is_err());
        let c = cylinder([10.0, 20.0, 30.0], 5.0, 40.0, 12).unwrap();
        assert_eq!(c.mesh.vertices.len(), 24);
        assert_eq!(c.mesh.faces.len(), 14, "bottom + top + 12 sides");
        assert_eq!(c.names[0], "bottom");
        assert_eq!(c.names[13], "side11");
        let (mn, mx) = bounds(&c.mesh.vertices);
        assert!((mn[2] - 30.0).abs() < 1e-9 && (mx[2] - 70.0).abs() < 1e-9);
        assert!((mn[0] - 5.0).abs() < 1e-9 && (mx[0] - 15.0).abs() < 1e-9);
        // wedge: full height at the rise end, zero at the other
        let w = wedge([0.0, 0.0, 0.0], [10.0, 4.0, 8.0], "+x").unwrap();
        assert_eq!(w.mesh.vertices.len(), 6);
        assert_eq!(w.mesh.faces.len(), 5);
        let (mn, mx) = bounds(&w.mesh.vertices);
        assert_eq!((mn, mx), ([0.0, 0.0, 0.0], [10.0, 4.0, 8.0]));
    }

    #[test]
    fn orient_outward_flips_a_reversed_solid() {
        let b = r#box([0.0, 0.0, 0.0], [8.0, 8.0, 8.0]).unwrap();
        let reversed: Vec<Vec<usize>> = b
            .mesh
            .faces
            .iter()
            .map(|f| f.iter().copied().rev().collect())
            .collect();
        let (fixed, flipped) = orient_outward(&b.mesh.vertices, &reversed).unwrap();
        assert_eq!(flipped, 6, "every face of the reversed box is flipped back");
        assert_eq!(fixed, b.mesh.faces);
    }

    #[test]
    fn extrude_negative_distance_recesses_and_keeps_the_cap_slot() {
        let b = r#box([0.0, 0.0, 0.0], [4.0, 4.0, 4.0]).unwrap();
        let v = &b.mesh.vertices;
        let f = &b.mesh.faces;
        let fm = vec![Some("cap".to_string()); 6];
        let (verts, faces, mats) = extrude_face(v, f, Some(&fm), 1, -1.0, Some("side")).unwrap();
        assert_eq!(verts.len(), 12);
        assert_eq!(faces.len(), 10);
        assert_eq!(faces[1].len(), 4, "the cap keeps its slot");
        assert_eq!(mats[1], Some("cap".to_string()));
        assert_eq!(mats[6], Some("side".to_string()));
        // the pushed cap is below the top of the box now
        assert_eq!(max_axis(&verts[..], 2), 4.0);
        assert!(extrude_face(v, f, None, 9, 1.0, None).is_err());
        assert!(extrude_face(v, f, None, 0, 0.0, None).is_err());
    }

    /// Review N3: the POC's `side_material or cap_mat` treats an empty string as "not given" (Python
    /// falsily), so `Some("")` must fall back to the cap's material rather than assign "" to the new
    /// side quads.
    #[test]
    fn extrude_treats_an_empty_side_material_as_the_caps() {
        let b = r#box([0.0, 0.0, 0.0], [4.0, 4.0, 4.0]).unwrap();
        let v = &b.mesh.vertices;
        let f = &b.mesh.faces;
        let fm = vec![Some("cap".to_string()); 6];
        let (_, _, mats) = extrude_face(v, f, Some(&fm), 1, 1.0, Some("")).unwrap();
        assert_eq!(mats[1], Some("cap".to_string()), "the cap keeps its own");
        for side in &mats[6..] {
            assert_eq!(
                side,
                &Some("cap".to_string()),
                "an empty sideMaterial is 'not given': the sides take the cap's material"
            );
        }
        // whitespace-only counts as "not given" too (the tool layer normalises it before this)
        let (_, _, mats) = extrude_face(v, f, Some(&fm), 1, 1.0, Some("  ")).unwrap();
        assert_eq!(mats[6], Some("cap".to_string()));
    }

    #[test]
    fn move_vertices_offsets_exactly_the_listed_indices() {
        let b = r#box([0.0, 0.0, 0.0], [8.0, 8.0, 8.0]).unwrap();
        let high: Vec<usize> = (0..8).filter(|i| b.mesh.vertices[*i][2] == 8.0).collect();
        assert_eq!(high.len(), 4);
        let moved = move_vertices(&b.mesh.vertices, &high[..2], [0.0, 0.0, -3.0]).unwrap();
        assert_eq!(moved.iter().filter(|p| p[2] == 5.0).count(), 2);
        assert_eq!(moved.iter().filter(|p| p[2] == 8.0).count(), 2);
        assert!(move_vertices(&b.mesh.vertices, &[], [0.0, 0.0, 1.0]).is_err());
        assert!(move_vertices(&b.mesh.vertices, &[99], [0.0, 0.0, 1.0]).is_err());
    }

    #[test]
    fn delete_faces_renumbers_and_drops_unused_vertices() {
        let b = r#box([0.0, 0.0, 0.0], [8.0, 8.0, 8.0]).unwrap();
        // keeping four faces still uses every vertex of the box
        let mesh = delete_faces(&b.mesh.vertices, &b.mesh.faces, None, &[0, 2]).unwrap();
        assert_eq!(mesh.faces.len(), 4);
        assert_eq!(mesh.vertices.len(), 8);
        assert!(mesh.faces.iter().flatten().all(|i| *i < mesh.vertices.len()));
        assert_eq!(mesh.face_materials, None);
        // keeping one face drops the vertices only it used and renumbers the loop
        let mesh = delete_faces(&b.mesh.vertices, &b.mesh.faces, None, &[1, 2, 3, 4, 5]).unwrap();
        assert_eq!(mesh.faces.len(), 1);
        assert_eq!(mesh.vertices.len(), 4);
        assert!(mesh.faces[0].iter().all(|i| *i < 4));
        // materials follow their faces
        let materials: Vec<Option<String>> = (0..6).map(|i| Some(format!("m{i}"))).collect();
        let mesh = delete_faces(
            &b.mesh.vertices,
            &b.mesh.faces,
            Some(&materials),
            &[1, 2, 3, 4, 5],
        )
        .unwrap();
        assert_eq!(mesh.face_materials, Some(vec![Some("m0".to_string())]));
        assert!(delete_faces(&b.mesh.vertices, &b.mesh.faces, None, &[]).is_err());
        assert!(delete_faces(&b.mesh.vertices, &b.mesh.faces, None, &[7]).is_err());
        assert!(delete_faces(&b.mesh.vertices, &b.mesh.faces, None, &[0, 1, 2, 3, 4, 5]).is_err());
    }

    #[test]
    fn rotate_z_and_transform_default_the_pivot_to_the_bounds_centre() {
        let vertices = v3(&[[0.0, 0.0, 0.0], [4.0, 0.0, 0.0], [0.0, 4.0, 0.0]]);
        let rotated = rotate_z(&vertices, 90.0, [0.0, 0.0, 0.0]);
        assert!((rotated[1][0] - 0.0).abs() < 1e-9 && (rotated[1][1] - 4.0).abs() < 1e-9);
        // scaling about the bounds centre doubles the extent but keeps the centre
        let scaled = transform(&vertices, None, 0.0, Some([2.0, 2.0, 2.0]), None).unwrap();
        let (mn, mx) = bounds(&scaled);
        assert_eq!((mn, mx), ([-2.0, -2.0, 0.0], [6.0, 6.0, 0.0]));
        assert!(transform(&vertices, None, 0.0, Some([1.0, 0.0, 1.0]), None).is_err());
        // the pivot is honoured
        let moved = transform(&vertices, None, 0.0, None, Some([10.0, 0.0, 0.0]))
            .unwrap()
            .iter()
            .map(|p| p[0])
            .collect::<Vec<f64>>();
        assert_eq!(moved, vec![0.0, 4.0, 0.0], "no rotate/translate/scale -> identity");
    }
}
