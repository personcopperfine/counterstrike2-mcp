//! glTF 2.0 / GLB reader: a file -> [`Scene`], our own plain types (no `gltf` types leak out).
//!
//! Static geometry, plus (I5) the first skin and the animations ([`super::gltf_rig`], the math is
//! documented there). Every mesh primitive becomes one [`MeshData`] with the node's **world**
//! transform already applied (positions by the matrix, normals by its cofactor matrix; a mirrored
//! node, determinant < 0, gets its winding flipped), still in glTF space (Y up, metres). The axis
//! change and the scale happen in [`super::smd`]. Texture bytes are passed through (PNG/JPEG
//! only); the `image` crate is not used.
//!
//! What is read: `POSITION`, `NORMAL` (generated smooth when absent), `TEXCOORD_0`, indices
//! (absent = sequential), modes TRIANGLES / TRIANGLE_STRIP / TRIANGLE_FAN, and the PBR
//! metallic-roughness material. What is not, and is reported in [`Scene::dropped`]: `COLOR_n`,
//! `TEXCOORD_n` (n > 0), morph targets, point/line primitives, emissive/occlusion textures, a
//! texture that is neither PNG nor JPEG, `JOINTS_1`/`WEIGHTS_1`, morph weight channels.
//!
//! With a skin ([`Scene::rig`]) the skinned primitives keep their vertices as given (skin space, no
//! node transform) with per-vertex [`MeshData::links`]; unskinned primitives are attached rigidly to
//! a bone (positions already moved to the bind pose). Without one, animations are dropped.
//!
//! Required extensions (`extensionsRequired`) outside [`TOLERATED_EXTENSIONS`] (Draco, meshopt,
//! KTX2/basisu, quantization, ...) are refused: [`ImportError::UnsupportedExtension`].
use super::gltf_rig::{self, Rig, RigRead, PRUNE_WEIGHT};
use super::mat::{from_f32, invert, mul, Mat4, IDENTITY};
use super::ImportError;
use gltf::mesh::Mode;
use std::path::Path;

/// More triangles than this is refused ([`ImportError::TooLarge`]): the compiler and the 512 KB
/// request/viewport budgets were not made for it, and an agent should decimate first.
pub const MAX_TRIS: usize = 500_000;

/// `extensionsRequired` entries that change nothing we read, so the file still imports (the
/// material ones only add shading terms the PBR subset ignores; `KHR_texture_transform` is
/// reported per material instead).
const TOLERATED_EXTENSIONS: &[&str] = &["KHR_lights_punctual", "KHR_texture_transform"];

fn tolerated(name: &str) -> bool {
    TOLERATED_EXTENSIONS.contains(&name) || name.starts_with("KHR_materials_")
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    pub meshes: Vec<MeshData>,
    pub materials: Vec<Material>,
    /// Things in the file that did not make it (one short sentence each).
    pub dropped: Vec<String>,
    /// Things the importer did that the caller should know about.
    pub warnings: Vec<String>,
    /// The first skin and the animations (glTF axes, metres); `None` for a static file.
    pub rig: Option<Rig>,
}

/// Up to four bone influences of one vertex: `(joint index into Rig::joints, weight)`, weights
/// sum to 1, unused slots have weight 0.
pub type Links = [(u16, f32); 4];

/// One glTF primitive, node transform applied, glTF axes and metres.
#[derive(Debug, Clone, PartialEq)]
pub struct MeshData {
    pub name: String,
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    /// `TEXCOORD_0` in glTF convention (V down); zeros when absent.
    pub uv0: Vec<[f32; 2]>,
    /// Triangle list, counter-clockwise.
    pub indices: Vec<u32>,
    /// Index into [`Scene::materials`].
    pub material: usize,
    /// Per-vertex bone links; empty for a static scene, one entry per vertex with a rig.
    pub links: Vec<Links>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Material {
    pub name: String,
    pub base_color: [f32; 4],
    pub metallic: f32,
    pub roughness: f32,
    pub base_color_tex: Option<Texture>,
    pub normal_tex: Option<Texture>,
    pub metal_rough_tex: Option<Texture>,
    /// Per-material losses (`alphaMode`, emissive, ...), also summarised in [`Scene::dropped`].
    pub dropped: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Texture {
    pub bytes: Vec<u8>,
    /// `png` or `jpg`: the file extension to write.
    pub ext: &'static str,
}

impl Scene {
    pub fn tri_count(&self) -> usize {
        self.meshes.iter().map(|m| m.indices.len() / 3).sum()
    }
    pub fn vert_count(&self) -> usize {
        self.meshes.iter().map(|m| m.positions.len()).sum()
    }
}

fn parse_err(detail: impl Into<String>) -> ImportError {
    ImportError::Parse(detail.into())
}

/// Read `bytes` (the content of `path`, which only locates external `.bin`/image files).
pub fn read(path: &Path, bytes: &[u8]) -> Result<Scene, ImportError> {
    // The raw JSON first: required extensions must be refused before the typed parser sees them.
    let json_bytes: Vec<u8> = if bytes.starts_with(b"glTF") {
        gltf::Glb::from_slice(bytes)
            .map_err(|e| parse_err(format!("GLB: {e}")))?
            .json
            .into_owned()
    } else {
        bytes.to_vec()
    };
    let raw: serde_json::Value =
        serde_json::from_slice(&json_bytes).map_err(|e| parse_err(format!("glTF JSON: {e}")))?;
    let required: Vec<String> = raw
        .get("extensionsRequired")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let unsupported: Vec<String> = required.into_iter().filter(|e| !tolerated(e)).collect();
    if !unsupported.is_empty() {
        return Err(ImportError::UnsupportedExtension(unsupported));
    }

    let parsed = gltf::Gltf::from_slice(bytes).map_err(|e| parse_err(format!("glTF: {e}")))?;
    let doc = &parsed.document;
    let base = path.parent().unwrap_or_else(|| Path::new("."));

    let mut buffers: Vec<Vec<u8>> = Vec::new();
    for buffer in doc.buffers() {
        let data = match buffer.source() {
            gltf::buffer::Source::Bin => parsed
                .blob
                .clone()
                .ok_or_else(|| parse_err("buffer without uri and no GLB binary chunk"))?,
            gltf::buffer::Source::Uri(uri) => load_uri(base, uri)?.0,
        };
        if data.len() < buffer.length() {
            return Err(parse_err(format!(
                "buffer {} is {} bytes, its byteLength says {}",
                buffer.index(),
                data.len(),
                buffer.length()
            )));
        }
        buffers.push(data);
    }

    let mut scene =
        Scene { meshes: Vec::new(), materials: Vec::new(), dropped: Vec::new(), warnings: Vec::new(), rig: None };
    // glTF material index -> Scene::materials index, filled on first use
    let mut material_map: Vec<Option<usize>> = vec![None; doc.materials().len()];
    let mut default_material: Option<usize> = None;
    let mut image_cache: Vec<Option<Option<Texture>>> = vec![None; doc.images().len()];

    let rig_read = gltf_rig::read(doc, &buffers, &mut scene.warnings, &mut scene.dropped)?;

    let roots: Vec<gltf::Node> = match doc.default_scene().or_else(|| doc.scenes().next()) {
        Some(s) => s.nodes().collect(),
        None => {
            let children: std::collections::HashSet<usize> =
                doc.nodes().flat_map(|n| n.children().map(|c| c.index()).collect::<Vec<_>>()).collect();
            doc.nodes().filter(|n| !children.contains(&n.index())).collect()
        }
    };

    let mut ctx = Ctx {
        buffers: &buffers,
        base,
        scene: &mut scene,
        material_map: &mut material_map,
        default_material: &mut default_material,
        image_cache: &mut image_cache,
        rig: rig_read.as_ref(),
        attached: Vec::new(),
        unweighted: 0,
    };
    for root in roots {
        ctx.visit(&root, IDENTITY, 0)?;
    }
    let (attached, unweighted) = (ctx.attached, ctx.unweighted);
    if scene.meshes.is_empty() {
        return Err(parse_err("the file has no triangle geometry in its scene"));
    }
    if let Some(read) = rig_read {
        if !attached.is_empty() {
            scene.warnings.push(format!(
                "unskinned mesh(es) attached rigidly to the nearest joint ancestor (or the root bone): {}",
                attached.join(", ")
            ));
        }
        if unweighted > 0 {
            scene.warnings.push(format!("{unweighted} skinned vertex(es) without weights bound to the root bone"));
        }
        scene.rig = Some(read.rig);
    }
    Ok(scene)
}

fn point(m: &Mat4, p: [f32; 3]) -> [f32; 3] {
    let p = [p[0] as f64, p[1] as f64, p[2] as f64];
    let mut out = [0f32; 3];
    for (r, cell) in out.iter_mut().enumerate() {
        *cell = (m[0][r] * p[0] + m[1][r] * p[1] + m[2][r] * p[2] + m[3][r]) as f32;
    }
    out
}

/// Cofactor matrix of the upper 3x3 (= inverse transpose up to a positive scale for a proper
/// matrix) and the determinant's sign.
fn normal_matrix(m: &Mat4) -> ([[f64; 3]; 3], f64) {
    // a[row][col]
    let a = |r: usize, c: usize| m[c][r];
    let cof = |r: usize, c: usize| {
        let (r1, r2) = ((r + 1) % 3, (r + 2) % 3);
        let (c1, c2) = ((c + 1) % 3, (c + 2) % 3);
        a(r1, c1) * a(r2, c2) - a(r1, c2) * a(r2, c1)
    };
    let mut out = [[0.0; 3]; 3];
    for (r, row) in out.iter_mut().enumerate() {
        for (c, cell) in row.iter_mut().enumerate() {
            *cell = cof(r, c);
        }
    }
    let det = a(0, 0) * cof(0, 0) + a(0, 1) * cof(0, 1) + a(0, 2) * cof(0, 2);
    (out, det)
}

fn normalize(v: [f64; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len < 1e-12 {
        [0.0, 0.0, 1.0]
    } else {
        [(v[0] / len) as f32, (v[1] / len) as f32, (v[2] / len) as f32]
    }
}

struct Ctx<'a> {
    buffers: &'a [Vec<u8>],
    base: &'a Path,
    scene: &'a mut Scene,
    material_map: &'a mut Vec<Option<usize>>,
    default_material: &'a mut Option<usize>,
    image_cache: &'a mut Vec<Option<Option<Texture>>>,
    rig: Option<&'a RigRead>,
    /// Names of the unskinned meshes attached to a bone.
    attached: Vec<String>,
    unweighted: usize,
}

/// How a primitive follows the skeleton.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Binding {
    /// No rig in the file.
    Static,
    /// `JOINTS_0`/`WEIGHTS_0` of the file's skin.
    Skinned,
    /// Rigid on this joint.
    Rigid(usize),
}

impl Ctx<'_> {
    fn note_dropped(&mut self, text: &str) {
        if !self.scene.dropped.iter().any(|d| d == text) {
            self.scene.dropped.push(text.to_string());
        }
    }

    fn visit(&mut self, node: &gltf::Node, parent: Mat4, depth: usize) -> Result<(), ImportError> {
        if depth > 256 {
            return Err(parse_err("node hierarchy deeper than 256 (cycle?)"));
        }
        let world = mul(&parent, &from_f32(node.transform().matrix()));
        if let Some(mesh) = node.mesh() {
            for primitive in mesh.primitives() {
                let (binding, matrix) = self.binding_for(node, &mesh, &primitive, &world);
                self.primitive(&mesh, &primitive, &matrix, binding)?;
            }
        }
        for child in node.children() {
            self.visit(&child, world, depth + 1)?;
        }
        Ok(())
    }

    /// How `primitive` (on `node`, with global matrix `world`) follows the skeleton, and the matrix
    /// its positions are transformed by: the node's world for a static file, identity for a
    /// skinned primitive (skin space as given), `bind_j * inverse(rest_j) * world` for a rigid one.
    fn binding_for(
        &mut self,
        node: &gltf::Node,
        mesh: &gltf::Mesh,
        primitive: &gltf::Primitive,
        world: &Mat4,
    ) -> (Binding, Mat4) {
        let Some(read) = self.rig else {
            return (Binding::Static, *world);
        };
        let has_links =
            primitive.get(&gltf::Semantic::Joints(0)).is_some() && primitive.get(&gltf::Semantic::Weights(0)).is_some();
        if node.skin().is_some_and(|s| s.index() == read.skin_index) {
            if has_links {
                return (Binding::Skinned, IDENTITY);
            }
            let msg = "a primitive of the skin's mesh has no JOINTS_0/WEIGHTS_0: attached rigidly instead";
            if !self.scene.warnings.iter().any(|w| w == msg) {
                self.scene.warnings.push(msg.to_string());
            }
        }
        let mut joint = None;
        let mut cur = Some(node.index());
        while let Some(n) = cur {
            if let Some(j) = read.joint_of_node.get(&n) {
                joint = Some(*j);
                break;
            }
            cur = read.node_parent[n];
        }
        let joint = joint.unwrap_or(read.root);
        let matrix = match invert(&read.rest[joint]) {
            Some(inv) => mul(&read.rig.joints[joint].bind, &mul(&inv, world)),
            None => *world,
        };
        let label = node.name().or(mesh.name()).unwrap_or("mesh").to_string();
        if !self.attached.contains(&label) {
            self.attached.push(label);
        }
        (Binding::Rigid(joint), matrix)
    }

    fn primitive(
        &mut self,
        mesh: &gltf::Mesh,
        primitive: &gltf::Primitive,
        world: &Mat4,
        binding: Binding,
    ) -> Result<(), ImportError> {
        let mode = primitive.mode();
        if !matches!(mode, Mode::Triangles | Mode::TriangleStrip | Mode::TriangleFan) {
            self.note_dropped("point/line primitives dropped (only triangles are imported)");
            return Ok(());
        }
        if primitive.morph_targets().len() > 0 {
            self.note_dropped("morph targets dropped");
        }
        for (semantic, _) in primitive.attributes() {
            match semantic {
                gltf::Semantic::Colors(_) => self.note_dropped("vertex colors (COLOR_n) dropped"),
                gltf::Semantic::TexCoords(n) if n > 0 => {
                    self.note_dropped("extra UV sets (TEXCOORD_1+) dropped; only TEXCOORD_0 is used")
                }
                gltf::Semantic::Joints(n) | gltf::Semantic::Weights(n) if n > 0 => self.note_dropped(
                    "JOINTS_1/WEIGHTS_1 (more than 4 bone influences per vertex) dropped; the 4 strongest of set 0 are kept",
                ),
                _ => {}
            }
        }
        let buffers = self.buffers;
        let reader = primitive.reader(|b| buffers.get(b.index()).map(|v| v.as_slice()));
        let Some(positions) = reader.read_positions() else {
            self.note_dropped("a primitive without POSITION dropped");
            return Ok(());
        };
        let local_positions: Vec<[f32; 3]> = positions.collect();
        let count = local_positions.len();
        let (ncof, det) = normal_matrix(world);
        let positions: Vec<[f32; 3]> = local_positions.iter().map(|p| point(world, *p)).collect();

        let mut indices: Vec<u32> = match reader.read_indices() {
            Some(i) => i.into_u32().collect(),
            None => (0..count as u32).collect(),
        };
        if let Some(bad) = indices.iter().find(|i| **i as usize >= count) {
            return Err(parse_err(format!("index {bad} out of range ({count} vertices)")));
        }
        indices = match mode {
            Mode::TriangleStrip => (0..indices.len().saturating_sub(2))
                .flat_map(|i| {
                    // every second triangle flips so the strip stays counter-clockwise
                    if i % 2 == 0 {
                        [indices[i], indices[i + 1], indices[i + 2]]
                    } else {
                        [indices[i + 1], indices[i], indices[i + 2]]
                    }
                })
                .collect(),
            Mode::TriangleFan => (1..indices.len().saturating_sub(1))
                .flat_map(|i| [indices[0], indices[i], indices[i + 1]])
                .collect(),
            _ => indices,
        };
        indices.truncate(indices.len() / 3 * 3);
        if det < 0.0 {
            for tri in indices.as_chunks_mut::<3>().0 {
                tri.swap(1, 2);
            }
        }
        let tris = self.scene.tri_count() + indices.len() / 3;
        if tris > MAX_TRIS {
            return Err(ImportError::TooLarge { tris, cap: MAX_TRIS });
        }

        let normals: Vec<[f32; 3]> = match reader.read_normals() {
            Some(n) => n
                .map(|n| {
                    let n = [n[0] as f64, n[1] as f64, n[2] as f64];
                    let mut out = [0.0; 3];
                    for (r, cell) in out.iter_mut().enumerate() {
                        *cell = ncof[r][0] * n[0] + ncof[r][1] * n[1] + ncof[r][2] * n[2];
                    }
                    let mut v = normalize(out);
                    if det < 0.0 {
                        // the cofactor of a mirror points inward: keep the geometric outside
                        v = [-v[0], -v[1], -v[2]];
                    }
                    v
                })
                .collect(),
            None => {
                let msg = "normals generated (smooth) for a primitive without NORMAL";
                if !self.scene.warnings.iter().any(|w| w == msg) {
                    self.scene.warnings.push(msg.to_string());
                }
                smooth_normals(&positions, &indices)
            }
        };
        if normals.len() != count {
            return Err(parse_err("NORMAL count differs from POSITION count"));
        }
        let uv0: Vec<[f32; 2]> = match reader.read_tex_coords(0) {
            Some(uv) => uv.into_f32().collect(),
            None => vec![[0.0, 0.0]; count],
        };
        if uv0.len() != count {
            return Err(parse_err("TEXCOORD_0 count differs from POSITION count"));
        }

        let links: Vec<Links> = match binding {
            Binding::Static => Vec::new(),
            Binding::Rigid(j) => vec![[(j as u16, 1.0), (0, 0.0), (0, 0.0), (0, 0.0)]; count],
            Binding::Skinned => self.skin_links(&reader, count)?,
        };
        let material = self.material_index(&primitive.material())?;
        self.scene.meshes.push(MeshData {
            name: mesh.name().unwrap_or("mesh").to_string(),
            positions,
            normals,
            uv0,
            indices,
            material,
            links,
        });
        Ok(())
    }

    /// `JOINTS_0`/`WEIGHTS_0` -> at most 4 normalised links per vertex (weights under
    /// [`PRUNE_WEIGHT`] pruned, the strongest four kept, renormalised). A vertex without any
    /// weight is bound to the root bone and counted.
    fn skin_links<'b, 's, F>(
        &mut self,
        reader: &gltf::mesh::Reader<'b, 's, F>,
        count: usize,
    ) -> Result<Vec<Links>, ImportError>
    where
        F: Clone + Fn(gltf::Buffer<'b>) -> Option<&'s [u8]>,
    {
        let read = self.rig.expect("skinned binding implies a rig");
        let joints: Vec<[u16; 4]> = reader.read_joints(0).map(|j| j.into_u16().collect()).unwrap_or_default();
        let weights: Vec<[f32; 4]> = reader.read_weights(0).map(|w| w.into_f32().collect()).unwrap_or_default();
        if joints.len() != count || weights.len() != count {
            return Err(parse_err("JOINTS_0/WEIGHTS_0 count differs from POSITION count"));
        }
        // the skin's own joints; the synthetic root (last) is not addressable from JOINTS_0
        let skin_joints = read.rig.joints.iter().filter(|j| !j.synthetic).count();
        let mut out = Vec::with_capacity(count);
        for (js, ws) in joints.iter().zip(&weights) {
            let mut pairs: Vec<(u16, f64)> = Vec::new();
            for k in 0..4 {
                let w = ws[k] as f64;
                if w.is_finite() && w > 0.0 {
                    if js[k] as usize >= skin_joints {
                        return Err(parse_err(format!(
                            "JOINTS_0 refers to joint {} but the skin has {} joints",
                            js[k], skin_joints
                        )));
                    }
                    pairs.push((js[k], w));
                }
            }
            // summed in f64: four f32 weights near f32::MAX overflow an f32 sum
            let sum: f64 = pairs.iter().map(|p| p.1).sum();
            if pairs.is_empty() || !sum.is_finite() || sum <= 1e-6 {
                self.unweighted += 1;
                out.push([(read.root as u16, 1.0), (0, 0.0), (0, 0.0), (0, 0.0)]);
                continue;
            }
            for p in &mut pairs {
                p.1 /= sum;
            }
            let strongest = pairs.iter().map(|p| p.1).fold(0.0f64, f64::max);
            pairs.retain(|p| p.1 >= PRUNE_WEIGHT as f64 || p.1 >= strongest);
            pairs.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            // the same joint listed twice adds up
            let mut merged: Vec<(u16, f64)> = Vec::new();
            for (j, w) in pairs {
                match merged.iter_mut().find(|m| m.0 == j) {
                    Some(m) => m.1 += w,
                    None => merged.push((j, w)),
                }
            }
            let total: f64 = merged.iter().map(|p| p.1).sum();
            let mut links: Links = [(0, 0.0); 4];
            for (slot, (j, w)) in merged.into_iter().enumerate() {
                links[slot] = (j, (w / total) as f32);
            }
            out.push(links);
        }
        Ok(out)
    }

    fn material_index(&mut self, material: &gltf::Material) -> Result<usize, ImportError> {
        let Some(gltf_index) = material.index() else {
            if let Some(i) = *self.default_material {
                return Ok(i);
            }
            self.scene.materials.push(Material {
                name: "default".to_string(),
                base_color: [1.0, 1.0, 1.0, 1.0],
                metallic: 0.0,
                roughness: 1.0,
                base_color_tex: None,
                normal_tex: None,
                metal_rough_tex: None,
                dropped: Vec::new(),
            });
            let i = self.scene.materials.len() - 1;
            *self.default_material = Some(i);
            return Ok(i);
        };
        if let Some(i) = self.material_map[gltf_index] {
            return Ok(i);
        }
        let pbr = material.pbr_metallic_roughness();
        let mut dropped = Vec::new();
        let mut texture = |ctx: &mut Self, t: Option<gltf::Texture>, tex_coord: u32, what: &str| {
            let t = t?;
            if tex_coord != 0 {
                dropped.push(format!("{what} texture uses TEXCOORD_{tex_coord}; only set 0 is imported"));
                return None;
            }
            match ctx.image(&t.source()) {
                Ok(Some(tex)) => Some(tex),
                Ok(None) => {
                    dropped.push(format!("{what} texture is not PNG/JPEG; dropped"));
                    None
                }
                Err(e) => {
                    dropped.push(format!("{what} texture unreadable ({e}); dropped"));
                    None
                }
            }
        };
        let base_info = pbr.base_color_texture();
        let base_color_tex = texture(
            self,
            base_info.as_ref().map(|i| i.texture()),
            base_info.as_ref().map_or(0, |i| i.tex_coord()),
            "baseColor",
        );
        let normal_info = material.normal_texture();
        let normal_tex = texture(
            self,
            normal_info.as_ref().map(|i| i.texture()),
            normal_info.as_ref().map_or(0, |i| i.tex_coord()),
            "normal",
        );
        let mr_info = pbr.metallic_roughness_texture();
        let metal_rough_tex = texture(
            self,
            mr_info.as_ref().map(|i| i.texture()),
            mr_info.as_ref().map_or(0, |i| i.tex_coord()),
            "metallicRoughness",
        );
        if material.emissive_texture().is_some() || material.emissive_factor() != [0.0; 3] {
            dropped.push("emissive dropped".to_string());
        }
        if material.occlusion_texture().is_some() {
            dropped.push("occlusion texture dropped".to_string());
        }
        match material.alpha_mode() {
            gltf::material::AlphaMode::Opaque => {}
            gltf::material::AlphaMode::Mask => dropped.push("alphaMode MASK dropped (opaque)".to_string()),
            gltf::material::AlphaMode::Blend => dropped.push("alphaMode BLEND dropped (opaque)".to_string()),
        }
        if material.double_sided() {
            dropped.push("doubleSided dropped (single-sided)".to_string());
        }
        for d in &dropped {
            let line = format!("material {}: {d}", material.name().unwrap_or("(unnamed)"));
            self.note_dropped(&line);
        }
        self.scene.materials.push(Material {
            name: material.name().map(str::to_string).unwrap_or_else(|| format!("material_{gltf_index}")),
            base_color: pbr.base_color_factor(),
            metallic: pbr.metallic_factor(),
            roughness: pbr.roughness_factor(),
            base_color_tex,
            normal_tex,
            metal_rough_tex,
            dropped,
        });
        let i = self.scene.materials.len() - 1;
        self.material_map[gltf_index] = Some(i);
        Ok(i)
    }

    /// The image's bytes when PNG/JPEG, `Ok(None)` for any other format (cached per image).
    fn image(&mut self, image: &gltf::Image) -> Result<Option<Texture>, ImportError> {
        let index = image.index();
        if let Some(cached) = &self.image_cache[index] {
            return Ok(cached.clone());
        }
        let bytes = match image.source() {
            gltf::image::Source::View { view, .. } => {
                let buffer = self
                    .buffers
                    .get(view.buffer().index())
                    .ok_or_else(|| parse_err("image bufferView points at a missing buffer"))?;
                buffer
                    .get(view.offset()..view.offset() + view.length())
                    .ok_or_else(|| parse_err("image bufferView out of range"))?
                    .to_vec()
            }
            gltf::image::Source::Uri { uri, .. } => load_uri(self.base, uri)?.0,
        };
        let texture = sniff(&bytes).map(|ext| Texture { bytes, ext });
        self.image_cache[index] = Some(texture.clone());
        Ok(texture)
    }
}

fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else {
        None
    }
}

/// Area-weighted smooth vertex normals.
fn smooth_normals(positions: &[[f32; 3]], indices: &[u32]) -> Vec<[f32; 3]> {
    let mut acc = vec![[0f64; 3]; positions.len()];
    for tri in indices.as_chunks::<3>().0 {
        let p = |i: u32| positions[i as usize].map(|v| v as f64);
        let (a, b, c) = (p(tri[0]), p(tri[1]), p(tri[2]));
        let (u, v) = ([b[0] - a[0], b[1] - a[1], b[2] - a[2]], [c[0] - a[0], c[1] - a[1], c[2] - a[2]]);
        let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        for i in tri {
            for k in 0..3 {
                acc[*i as usize][k] += n[k];
            }
        }
    }
    acc.into_iter().map(normalize).collect()
}

/// A buffer/image `uri`: `data:` (base64) or a relative file next to the glTF. Absolute paths,
/// `..` and URL schemes are refused (a glTF must not make us copy arbitrary host files into an
/// addon). Returns the bytes and the data-URI mime type when there was one.
fn load_uri(base: &Path, uri: &str) -> Result<(Vec<u8>, Option<String>), ImportError> {
    if let Some(rest) = uri.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',').ok_or_else(|| parse_err("malformed data: URI"))?;
        let mime = meta.split(';').next().filter(|m| !m.is_empty()).map(str::to_string);
        if !meta.split(';').any(|p| p == "base64") {
            return Err(parse_err("data: URI without base64"));
        }
        return Ok((base64_decode(data).ok_or_else(|| parse_err("invalid base64 in data: URI"))?, mime));
    }
    if uri.contains("://") || uri.starts_with('/') || uri.starts_with('\\') {
        return Err(parse_err(format!("external uri {uri:?} refused (only data: and relative files)")));
    }
    let decoded = percent_decode(uri);
    if decoded.split(['/', '\\']).any(|s| s == "..") || decoded.contains(':') {
        return Err(parse_err(format!("uri {uri:?} refused (no `..` or drive letters)")));
    }
    let path = base.join(&decoded);
    let bytes = std::fs::read(&path).map_err(|e| parse_err(format!("cannot read {}: {e}", path.display())))?;
    Ok((bytes, None))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Standard-alphabet base64 (padding optional, whitespace ignored).
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_known_values() {
        assert_eq!(base64_decode("TWFu").unwrap(), b"Man");
        assert_eq!(base64_decode("TWE=").unwrap(), b"Ma");
        assert_eq!(base64_decode("TQ").unwrap(), b"M");
        assert!(base64_decode("T!Q=").is_none());
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b.bin"), "a b.bin");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("x%zz"), "x%zz");
    }
}
