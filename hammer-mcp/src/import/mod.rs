//! Model import (`model_import`): turn a glTF/GLB (converted) or an FBX/SMD/OBJ/DMX (passed
//! through) into files of the active addon: `models/<name>/<name>.vmdl` plus its source mesh,
//! materials and textures. Pure and offline: [`plan`] builds every file in memory, the tool
//! ([`crate::tools::model_import`]) validates paths, writes and compiles.
//!
//! * [`gltf_read`]: file -> [`gltf_read::Scene`];
//! * [`smd`]: scene -> reference/physics SMD (axes, scale, V flip documented there);
//! * [`materials`]: PBR -> `.vmat` + textures.
//!
//! Plan: workflows/import/PLAN.md (I1); live facts: workflows/import/SPIKE-I0.md.
pub mod chunk;
pub mod gltf_read;
pub mod gltf_rig;
pub mod hammer_mesh;
pub mod mat;
pub mod materials;
pub mod planar;
pub mod retarget;
pub mod skeleton;
pub mod smd;

use crate::vmdl::{self, material_remap, Model};
use serde_json::json;
use smd::UpAxis;
use std::path::Path;

/// Formats converted to SMD.
pub const CONVERTED: &[&str] = &["gltf", "glb"];
/// Formats copied as they are and referenced by the `.vmdl`.
pub const PASS_THROUGH: &[&str] = &["fbx", "smd", "obj", "dmx"];
/// Shipped neutral material used when `materials:"none"` (the I0 spike's).
pub const FALLBACK_MATERIAL: &str = "materials/dev/reflectivity_50.vmat";
/// glTF metres -> Source inch units.
pub const GLTF_SCALE: f64 = 39.37;

#[derive(Debug)]
pub enum ImportError {
    UnsupportedFormat { ext: String },
    UnsupportedExtension(Vec<String>),
    Parse(String),
    TooLarge { tris: usize, cap: usize },
    /// A coordinate would leave the bridge's +-1e6 range (scale / origin too big).
    Range(String),
    /// An option combination the file cannot honour (a skinned file as a `world` model, ...).
    Invalid(String),
    /// The `.vmdl` the plan built did not validate (a bug, reported as is).
    Model(vmdl::VmdlError),
    /// `kind:"player"` on a file without a skin.
    NoSkin,
    /// A `boneMap` entry that does not hold up (detail, near misses).
    BoneMap { detail: String, near: Vec<String> },
    /// Required CS2 bones no source joint was found for.
    BoneMapIncomplete { unmapped: Vec<String>, required: Vec<String> },
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::UnsupportedFormat { ext } => write!(f, "unsupported format .{ext}"),
            ImportError::UnsupportedExtension(e) => write!(f, "unsupported glTF extensions {}", e.join(", ")),
            ImportError::Parse(detail) | ImportError::Range(detail) | ImportError::Invalid(detail) => f.write_str(detail),
            ImportError::TooLarge { tris, cap } => write!(f, "{tris} triangles (cap {cap})"),
            ImportError::Model(e) => write!(f, "{e:?}"),
            ImportError::NoSkin => f.write_str("the file has no skin"),
            ImportError::BoneMap { detail, .. } => f.write_str(detail),
            ImportError::BoneMapIncomplete { unmapped, .. } => write!(f, "no source joint for {}", unmapped.join(", ")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Prop,
    World,
    /// A humanoid retargeted onto the CS2 player skeleton (I6, needs [`ImportOptions::player`]).
    Player,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Physics {
    Hull,
    Mesh,
    None,
}

impl Physics {
    pub fn as_str(self) -> &'static str {
        match self {
            Physics::Hull => "hull",
            Physics::Mesh => "mesh",
            Physics::None => "none",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// Model stem, already validated (`models/<name>/<name>.vmdl`).
    pub name: String,
    pub kind: Kind,
    pub scale: Option<f64>,
    pub up: Option<UpAxis>,
    /// `true` = generate `.vmat` files (`materials:"generate"`).
    pub generate_materials: bool,
    /// `materials:"existing"`: source material name -> `materials/..vmat` (targets already
    /// resolved by the tool). `Some` implies `generate_materials == false`; nothing is written.
    pub existing: Option<Vec<(String, String)>>,
    pub physics: Option<Physics>,
    /// `kind:"player"`: the CS2 skeleton and the mapping options.
    pub player: Option<retarget::PlayerOptions>,
}

#[derive(Debug, Clone)]
pub struct PlannedFile {
    /// Addon-relative, forward slashes.
    pub path: String,
    pub role: &'static str,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct MaterialReport {
    pub vmat: String,
    pub textures: Vec<String>,
    pub dropped: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ImportPlan {
    /// `models/<name>/<name>.vmdl`
    pub model: String,
    pub files: Vec<PlannedFile>,
    pub vmdl: Model,
    pub materials: Vec<MaterialReport>,
    /// `None` for a pass-through (the mesh is not read).
    pub mesh: Option<MeshStats>,
    pub physics: Physics,
    pub scale: f64,
    pub dropped: Vec<String>,
    pub warnings: Vec<String>,
    /// `Some` for a skinned glTF (I5).
    pub rig: Option<RigPlan>,
    /// The material names the file's SMD uses (glTF: sanitized, unique; what `materialMap` keys
    /// answer to). Empty for a pass-through.
    pub material_names: Vec<String>,
    /// `materials:"existing"`: (material name, target) for every remap written.
    pub remapped: Vec<(String, String)>,
    /// `Some` for `kind:"player"`.
    pub player: Option<retarget::Report>,
    /// `kind:"player"` with a reference agent: `(agent model, its markup on the import)`.
    pub markup: Option<(String, retarget::agent::Fitted)>,
}

/// What a skinned import produced (result `skeleton` / `animations`).
#[derive(Debug, Clone, PartialEq)]
pub struct RigPlan {
    pub bones: usize,
    pub root: String,
    /// `(glTF node name or None for the synthetic root, SMD bone name)` in bone order.
    pub names: Vec<(Option<String>, String)>,
    /// `(sequence name, frames)`; the file is `models/<name>/<sequence>.smd`.
    pub animations: Vec<(String, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshStats {
    pub tris: usize,
    pub verts: usize,
    pub materials: usize,
}

/// Lowercase extension of `path`, `""` when none.
pub fn extension_of(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// Build the import of `source` (its `bytes`) in memory.
pub fn plan(source: &Path, bytes: Vec<u8>, opts: &ImportOptions) -> Result<ImportPlan, ImportError> {
    let ext = extension_of(source);
    let converted = CONVERTED.contains(&ext.as_str());
    if !converted && !PASS_THROUGH.contains(&ext.as_str()) {
        return Err(ImportError::UnsupportedFormat { ext });
    }
    let name = &opts.name;
    if opts.kind == Kind::Player && !converted {
        return Err(ImportError::Invalid(format!(
            "kind \"player\" retargets a skinned glTF/GLB; a .{ext} file cannot be mapped onto the CS2 skeleton here"
        )));
    }
    let dir = format!("models/{name}");
    let physics = opts.physics.unwrap_or(match opts.kind {
        Kind::Prop => Physics::Hull,
        Kind::World => Physics::Mesh,
        Kind::Player => Physics::None,
    });
    let mut files: Vec<PlannedFile> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    let mut reports: Vec<MaterialReport> = Vec::new();
    let mut remaps: Vec<serde_json::Value> = Vec::new();
    let mut mesh_stats = None;
    let mut material_names: Vec<String> = Vec::new();
    let mut remapped: Vec<(String, String)> = Vec::new();
    let mut anim_files: Vec<String> = Vec::new();
    let mut rig_plan: Option<RigPlan> = None;
    let mut player_report: Option<retarget::Report> = None;
    let mut player_markup: Option<Option<(String, retarget::agent::Fitted)>> = None;
    // `.vmat` / case / path noise off a user-given name
    let strip = |k: &str| k.trim().strip_suffix(".vmat").unwrap_or(k.trim()).to_string();

    let (mesh_path, scale) = if converted {
        let scale = opts.scale.unwrap_or(GLTF_SCALE);
        let up = opts.up.unwrap_or(UpAxis::Y);
        let scene = gltf_read::read(source, &bytes)?;
        let smd_names = materials::smd_names(&scene);
        let mesh_path = format!("{dir}/{name}.smd");
        let stats = if opts.kind == Kind::Player {
            let player = opts
                .player
                .as_ref()
                .ok_or_else(|| ImportError::Invalid("kind \"player\" needs the CS2 skeleton (internal: not loaded)".to_string()))?;
            if physics == Physics::Mesh {
                return Err(ImportError::Invalid("physics \"mesh\" (concave) is for static geometry; a player model uses \"hull\" or \"none\"".to_string()));
            }
            let out = retarget::retarget(&scene, &smd_names, player, opts.scale)?;
            files.push(PlannedFile { path: mesh_path.clone(), role: "mesh", bytes: out.reference.into_bytes() });
            warnings.extend(out.warnings);
            if scene.rig.as_ref().is_some_and(|r| !r.clips.is_empty()) {
                dropped.push("animations dropped: the game's AnimGraph2 animates a player model".to_string());
            }
            if physics == Physics::Hull {
                warnings.push("physics: one rigid hull of the CS2 rest pose (it does not follow the bones)".to_string());
            }
            player_report = Some(out.report);
            player_markup = Some(out.markup);
            smd::SmdStats { tris: out.tris, verts: out.verts }
        } else if scene.rig.is_some() {
            if opts.kind == Kind::World {
                return Err(ImportError::Invalid(
                    "the file is skinned: kind \"world\" is static geometry; import it as kind \"prop\" (prop_dynamic)".to_string(),
                ));
            }
            if physics == Physics::Mesh {
                return Err(ImportError::Invalid(
                    "physics \"mesh\" (concave) is for static geometry; a skinned model uses \"hull\" or \"none\"".to_string(),
                ));
            }
            let rigged = smd::write_rigged(&scene, &smd_names, scale, up, &[name.as_str()]);
            files.push(PlannedFile { path: mesh_path.clone(), role: "mesh", bytes: rigged.reference.into_bytes() });
            let mut anim_report = Vec::new();
            for anim in rigged.anims {
                let path = format!("{dir}/{}.smd", anim.name);
                anim_files.push(path.clone());
                anim_report.push((anim.name.clone(), anim.frames));
                files.push(PlannedFile { path, role: "animation", bytes: anim.text.into_bytes() });
            }
            warnings.extend(rigged.warnings);
            dropped.extend(rigged.dropped);
            if physics == Physics::Hull {
                warnings.push(
                    "physics: one rigid hull of the bind pose (it does not follow the bones or the animation)".to_string(),
                );
            }
            let sk = &rigged.skeleton;
            rig_plan = Some(RigPlan {
                bones: sk.bones.len(),
                root: sk.bones.iter().find(|b| b.parent.is_none()).map(|b| b.name.clone()).unwrap_or_default(),
                names: sk.bones.iter().map(|b| (b.source.clone(), b.name.clone())).collect(),
                animations: anim_report,
            });
            rigged.stats
        } else {
            let (text, stats) = smd::write_reference(&scene, &smd_names, scale, up);
            files.push(PlannedFile { path: mesh_path.clone(), role: "mesh", bytes: text.into_bytes() });
            stats
        };
        material_names = smd_names.clone();
        mesh_stats = Some(MeshStats { tris: stats.tris, verts: stats.verts, materials: scene.materials.len() });
        dropped.extend(scene.dropped.iter().cloned());
        warnings.extend(scene.warnings.iter().cloned());
        if opts.generate_materials {
            for vmat in materials::build(&scene, &smd_names, name) {
                remaps.push(material_remap(&vmat.smd_name, &vmat.vmat));
                for (path, data) in &vmat.textures {
                    files.push(PlannedFile { path: path.clone(), role: "texture", bytes: data.clone() });
                }
                files.push(PlannedFile { path: vmat.vmat.clone(), role: "material", bytes: vmat.text.clone().into_bytes() });
                reports.push(MaterialReport {
                    vmat: vmat.vmat,
                    textures: vmat.textures.iter().map(|(p, _)| p.clone()).collect(),
                    dropped: vmat.dropped,
                });
            }
            if scene.materials.iter().any(|m| m.normal_tex.is_some()) {
                warnings.push(materials::NORMAL_WARNING.to_string());
            }
            if scene.materials.iter().any(|m| m.metal_rough_tex.is_some()) {
                warnings.push(materials::PACKED_WARNING.to_string());
            }
        }
        if let Some(map) = &opts.existing {
            for (name, target) in materials::match_existing(map, &smd_names, FALLBACK_MATERIAL, &mut warnings) {
                remaps.push(material_remap(&name, &target));
                remapped.push((name, target));
            }
        }
        (mesh_path, 1.0)
    } else {
        // pass-through: the file keeps its own name next to the .vmdl
        let file_name = source.file_name().and_then(|f| f.to_str()).unwrap_or("mesh");
        let mesh_path = format!("{dir}/{file_name}");
        files.push(PlannedFile { path: mesh_path.clone(), role: "mesh", bytes });
        if opts.generate_materials {
            warnings.push(format!(
                "materials come from the {ext} file; nothing generated (edit remaps with model_edit, or materials:\"none\")"
            ));
        }
        if let Some(map) = &opts.existing {
            for (from, to) in map {
                remaps.push(material_remap(&strip(from), to));
                remapped.push((strip(from), to.clone()));
            }
            warnings.push(if map.is_empty() {
                format!("materialMap is empty: the {ext} file keeps its own material names (nothing remapped)")
            } else {
                format!("materialMap names are not checked against the {ext} file (pass-through): a name that is not in it does nothing")
            });
        }
        (mesh_path, opts.scale.unwrap_or(1.0))
    };

    let mut params = json!({"meshFile": mesh_path, "importScale": scale});
    if physics != Physics::None {
        params["physicsFile"] = json!(mesh_path);
        params["physicsKind"] = json!(physics.as_str());
    }
    if !remaps.is_empty() {
        params["materialRemaps"] = json!(remaps);
    } else if !(!converted && (opts.generate_materials || opts.existing.is_some())) {
        // no generated materials: one neutral shipped material for everything. A pass-through
        // that keeps its own materials (generate) gets no global default.
        params["material"] = json!(FALLBACK_MATERIAL);
    }
    if rig_plan.is_some() {
        params["animFiles"] = json!(anim_files);
    }
    if player_report.is_some() {
        params["nmSkeleton"] = json!(retarget::skeleton::SKELETON_PATH);
        params["animGraph2"] = json!(retarget::skeleton::GRAPH_PATH);
        // keep all 75 bones: without it the compiler drops the unweighted ones (root_motion, wpnPivot, ...;
        // live 2026-11-03: the box humanoid compiled to 20 bones); `do_not_discard_skeleton_bones` on the
        // reference fails to compile (the compiler cannot load the pak's vnmskel_c as a dependency)
        if let Some(player) = &opts.player {
            let names = &player.skeleton.names;
            params["keepBones"] = json!(names.iter().map(|n| n.to_ascii_lowercase()).collect::<Vec<_>>());
            // the SMD importer lowercases bone names; the vnmskel and the agents use mixed case. Restored
            // so the model's bones are named exactly as the graph's (live 2026-11-03 the pose came with
            // the renames plus the `worldmodel` identifier; the renames alone were not enough, whether
            // they are needed at all is untested)
            params["renameBones"] = json!(names
                .iter()
                .filter(|n| n.to_ascii_lowercase() != **n)
                .map(|n| json!({"from": n.to_ascii_lowercase(), "to": n}))
                .collect::<Vec<_>>());
        }
        // the player pawn asks for the graph by the identifier `worldmodel` (the agents carry it);
        // with only the default one the pawn stood in the bind pose (live 2026-11-03)
        params["animGraph2Identifiers"] = json!(["worldmodel"]);
    }
    if let Some(Some((_, fitted))) = &player_markup {
        params["attachments"] = fitted.attachments_param();
        params["hitboxSets"] = fitted.hitbox_sets_param();
    }
    let skeletal = rig_plan.is_some() || player_report.is_some();
    let vmdl = vmdl::template(if skeletal { "skeletal" } else { "static_prop" }, &params)
        .map_err(ImportError::Model)?;
    let model = format!("{dir}/{name}.vmdl");
    files.insert(
        0,
        PlannedFile { path: model.clone(), role: "model", bytes: vmdl.write().into_bytes() },
    );
    Ok(ImportPlan {
        model,
        files,
        vmdl,
        materials: reports,
        mesh: mesh_stats,
        physics,
        scale: match &player_report {
            Some(r) => r.scale,
            None if converted => opts.scale.unwrap_or(GLTF_SCALE),
            None => scale,
        },
        dropped,
        warnings,
        rig: rig_plan,
        material_names,
        remapped,
        player: player_report,
        markup: player_markup.flatten(),
    })
}
