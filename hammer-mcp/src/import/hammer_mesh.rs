//! `model_import kind:"hammer_mesh"`: a glTF/GLB as Hammer meshes placed in the open map.
//! Pure and offline: [`plan`] reads the scene, builds the materials the prop import builds
//! ([`super::materials`]), the planar faces with exact texture axes ([`super::planar`]) and the
//! `mesh.create` chunks ([`super::chunk`]). The bridge calls are
//! [`crate::tools::hammer_mesh`].
use super::chunk::{self, Chunk, Limits};
use super::gltf_read;
use super::materials;
use super::planar::{self, Planar};
use super::smd::UpAxis;
use super::{ImportError, MaterialReport, PlannedFile, FALLBACK_MATERIAL, GLTF_SCALE};
use std::path::Path;

/// More source triangles than this is refused: two bridge calls per chunk, and a chunk is
/// `maxTrisPerMesh` triangles.
pub const MAX_TRIS: usize = 100_000;
/// Default and largest `maxTrisPerMesh`.
pub const DEFAULT_TRIS_PER_MESH: usize = 2000;
pub const MAX_TRIS_PER_MESH: usize = 20_000;
/// Request budget per bridge call (the tool's `create_mesh` budget is 512 KB; leave room).
pub const REQUEST_BUDGET: usize = 400_000;

#[derive(Debug, Clone)]
pub struct MeshOptions {
    /// Folder name of the generated materials (`materials/models/<name>/`), already validated.
    pub name: String,
    pub scale: Option<f64>,
    pub up: Option<UpAxis>,
    pub generate_materials: bool,
    /// `materials:"existing"`: source material name -> resolved `.vmat` path.
    pub existing: Option<Vec<(String, String)>>,
    pub origin: [f64; 3],
    /// `[pitch, yaw, roll]` degrees.
    pub angles: [f64; 3],
    pub max_tris: usize,
}

#[derive(Debug, Clone)]
pub struct MeshPlan {
    /// Materials and textures to write (addon-relative).
    pub files: Vec<PlannedFile>,
    pub materials: Vec<MaterialReport>,
    pub material_names: Vec<String>,
    pub remapped: Vec<(String, String)>,
    /// The Hammer material path for each `Scene::materials` index.
    pub material_paths: Vec<String>,
    pub planar: Planar,
    pub chunks: Vec<Chunk>,
    /// Faces left out because they are not polygons once rounded for the bridge.
    pub dropped_faces: usize,
    /// `"axes"` unless no face got usable UVs (`"default"`).
    pub uv_mode: &'static str,
    pub verts: usize,
    pub scale: f64,
    pub dropped: Vec<String>,
    pub warnings: Vec<String>,
}

impl MeshPlan {
    pub fn tris(&self) -> usize {
        self.planar.tris
    }
}

pub fn plan(source: &Path, bytes: &[u8], opts: &MeshOptions) -> Result<MeshPlan, ImportError> {
    let scale = opts.scale.unwrap_or(GLTF_SCALE);
    let scene = gltf_read::read(source, bytes)?;
    if scene.tri_count() > MAX_TRIS {
        return Err(ImportError::TooLarge { tris: scene.tri_count(), cap: MAX_TRIS });
    }
    let mut warnings = scene.warnings.clone();
    let mut dropped = scene.dropped.clone();
    if scene.rig.is_some() {
        dropped.push("skin and animations dropped: Hammer meshes are static (the bind pose is built)".to_string());
    }
    let names = materials::smd_names(&scene);

    let mut files = Vec::new();
    let mut reports = Vec::new();
    let mut remapped = Vec::new();
    let mut tex_sizes: Vec<[f64; 2]> = Vec::new();
    let mut size_unknown = false;
    let material_paths: Vec<String> = if opts.generate_materials {
        let built = materials::build(&scene, &names, &opts.name);
        for (material, vmat) in scene.materials.iter().zip(&built) {
            match &material.base_color_tex {
                Some(tex) => match materials::image_size(&tex.bytes) {
                    Some((w, h)) => tex_sizes.push([f64::from(w), f64::from(h)]),
                    None => {
                        size_unknown = true;
                        tex_sizes.push(planar::DEFAULT_TEX_SIZE);
                    }
                },
                None => tex_sizes.push(planar::DEFAULT_TEX_SIZE),
            }
            for (path, data) in &vmat.textures {
                files.push(PlannedFile { path: path.clone(), role: "texture", bytes: data.clone() });
            }
            files.push(PlannedFile { path: vmat.vmat.clone(), role: "material", bytes: vmat.text.clone().into_bytes() });
            reports.push(MaterialReport {
                vmat: vmat.vmat.clone(),
                textures: vmat.textures.iter().map(|(p, _)| p.clone()).collect(),
                dropped: vmat.dropped.clone(),
            });
        }
        if scene.materials.iter().any(|m| m.normal_tex.is_some()) {
            warnings.push(materials::NORMAL_WARNING.to_string());
        }
        if scene.materials.iter().any(|m| m.metal_rough_tex.is_some()) {
            warnings.push(materials::PACKED_WARNING.to_string());
        }
        built.iter().map(|v| v.vmat.clone()).collect()
    } else if let Some(map) = &opts.existing {
        size_unknown = true;
        let matched = materials::match_existing(map, &names, FALLBACK_MATERIAL, &mut warnings);
        let paths = matched.iter().map(|(_, to)| to.clone()).collect();
        remapped = matched;
        paths
    } else {
        size_unknown = true;
        names.iter().map(|_| FALLBACK_MATERIAL.to_string()).collect()
    };
    if size_unknown {
        warnings.push(format!(
            "texture size of the material(s) is unknown (shipped/none materials or an unreadable image): \
             the texture axes assume {}x{} texels, so the tiling is right only for a texture of that size",
            planar::DEFAULT_TEX_SIZE[0],
            planar::DEFAULT_TEX_SIZE[1]
        ));
    }

    let mut popts = planar::Options::new(scale, opts.up.unwrap_or(UpAxis::Y));
    popts.rotation = crate::vmap::mesh::angle_matrix(opts.angles);
    popts.origin = opts.origin;
    popts.tex_sizes = tex_sizes;
    let planar = planar::build(&scene, &popts);
    let limit = crate::geometry::MAX_COORD - 10.0;
    if planar.faces.iter().flat_map(|f| &f.verts).flatten().any(|c| c.abs() >= limit) {
        return Err(ImportError::Range(format!(
            "the model leaves the bridge's +-{} range at scale {scale} and origin {:?}: lower scale or move origin",
            crate::geometry::MAX_COORD,
            opts.origin
        )));
    }
    let split = chunk::split(
        &planar.faces,
        &Limits { max_tris: opts.max_tris, max_request_bytes: REQUEST_BUDGET },
    );
    if planar.degenerate_tris > 0 {
        warnings.push(format!("{} degenerate triangle(s) (zero area) left out", planar.degenerate_tris));
    }
    if !split.dropped.is_empty() {
        warnings.push(format!(
            "{} face(s) collapse when rounded to the bridge's 4 decimals / float coordinates and were left out",
            split.dropped.len()
        ));
    }
    if planar.defaulted_faces > 0 {
        warnings.push(format!(
            "{} of {} face(s) have no usable UVs (none in the file, or zero-area/collinear mapping): \
             they got Hammer's default world projection",
            planar.defaulted_faces,
            planar.faces.len()
        ));
    }
    let uv_mode = if planar.faces.is_empty() || planar.defaulted_faces == planar.faces.len() { "default" } else { "axes" };
    let verts = split.chunks.iter().map(|c| c.vertices.len()).sum();
    Ok(MeshPlan {
        files,
        materials: reports,
        material_names: names,
        remapped,
        material_paths,
        planar,
        chunks: split.chunks,
        dropped_faces: split.dropped.len(),
        uv_mode,
        verts,
        scale,
        dropped,
        warnings,
    })
}
