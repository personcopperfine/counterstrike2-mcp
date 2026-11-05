//! `model_import`: glTF/GLB (converted to SMD) or FBX/SMD/OBJ/DMX (copied) -> `.vmdl` + mesh +
//! materials in the active addon, optionally compiled. The conversion is [`crate::import`]
//! (pure, offline); this file is argument handling, path safety, the atomic writes
//! (everything lands in the active addon, existing files are backed up like `model_create`
//! does) and the compile through the shared resourcecompiler route
//! ([`crate::tools::compile`], target `ANY_TOOLS`: the bridge `job.*` route does not need a
//! ModelDoc container, SPIKE-I0 f).
//!
//! `kind` `hammer_mesh` (I3) is in [`super::hammer_mesh`]. `kind` `player` (I6) retargets a skinned
//! glTF onto the CS2 player skeleton (`import::retarget`): the tool reads `worldmodel.vnmskel_c`
//! from the game paks, the pure plan does the rest. There is no `agent` argument: the skeleton and
//! the AnimGraph2 are the same for every agent, so naming one is refused (the dispatcher's
//! unknown-argument check, which names `agent`) instead of ignored.
//!
//! `[I]` inferred, live-checked by the reviewer: whether the vmdl compile pulls in the generated
//! `.vmat`s (otherwise they need their own compile), the `remaps` key spelling
//! ([`crate::vmdl::material_remap`]) and the `.vmat` keys ([`crate::import::materials`]).
use crate::tools::asset_resolve::{find_ci, norm, Paths, Sources};
use crate::import::retarget::{self, mapping::RigHint, skeleton as nm, PlayerOptions};
use crate::import::{self, smd::UpAxis, ImportError, ImportOptions, ImportPlan, Kind, Physics};
use crate::state::HammerState;
use crate::tools::compile::{compile_resourcecompiler, compile_timeout, CompileTarget};
use crate::tools::model_file::{check_classes, vmdl_error};
use crate::vmdl::fsutil::write_atomic_bytes_with_backup;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const MODEL_IMPORT_TOOL_ORDER: [&str; 1] = ["model_import"];

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "model_import",
            "Import a 3D file as a model of the active addon: models/<name>/<name>.vmdl + mesh + materials. \
             glTF/GLB are converted (Y-up m -> Z-up inches, PBR -> .vmat; a skin + animations become a skeletal prop); fbx/smd/obj/dmx are copied. \
             kind prop (hull collision), world (concave; prop_static), hammer_mesh (glTF only, bridge m3.18: Hammer meshes in the open map) or player (skinned humanoid retargeted to the CS2 player skeleton + AnimGraph2 + an agent's attachments/hitboxes; boneMap, rig, scale, referenceAgent). \
             materials generate|none|existing (materialMap). Compiles unless compile:false. dryRun writes nothing. Result: files, materials, mesh, skeleton, retarget, markup, animations, dropped, warnings.",
            Writer,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "source": {"type": "string", "description": "host path, or inside the addon"},
                "kind": {"type": "string", "enum": ["prop", "world", "hammer_mesh", "player"], "description": "default prop; hammer_mesh = Hammer meshes in the open map; player = CS2 player skeleton"},
                "name": {"type": "string", "description": "model stem [a-z0-9_-]; default the file stem"},
                "scale": {"type": "number", "description": "default 39.37 (glTF), else 1"},
                "upAxis": {"type": "string", "enum": ["y", "z"], "description": "default y for glTF"},
                "materials": {"type": "string", "enum": ["generate", "none", "existing"], "description": "default generate (.vmat + textures); existing maps onto materialMap, writes none"},
                "materialMap": {"type": "object", "additionalProperties": {"type": "string"}, "description": "with existing: {source material: materials/x.vmat}; glTF names = materialNames (dryRun), unmapped -> fallback"},
                "physics": {"type": "string", "enum": ["hull", "mesh", "none"], "description": "default hull (prop), mesh (world)"},
                "compile": {"type": "boolean", "description": "default true"},
                "overwrite": {"type": "boolean"},
                "dryRun": {"type": "boolean"},
                "origin": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3, "description": "hammer_mesh: placement [x,y,z], default 0"},
                "angles": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3, "description": "hammer_mesh: [pitch,yaw,roll] degrees"},
                "maxTrisPerMesh": {"type": "integer", "minimum": 30, "description": "hammer_mesh: default 2000"},
                "boneMap": {"type": "object", "additionalProperties": {"type": "string"}, "description": "player: {source joint: CS2 bone}, wins over name tables"},
                "rig": {"type": "string", "enum": ["auto", "mixamo", "rigify", "ue", "vrm"], "description": "player: bone-name table, default auto"},
                "referenceAgent": {"type": "string", "description": "player: default agents/models/ctm_sas/ctm_sas.vmdl; none = no markup"}
            },
            "required": ["source"]
        })),
        model_import,
    );
}

pub(super) fn str_arg<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(ToolError::invalid_arguments(format!("{key} must be a string, got {other}"))),
    }
}

pub(super) fn bool_arg(args: &Value, key: &str, default: bool) -> Result<bool, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => Err(ToolError::invalid_arguments(format!("{key} must be a boolean, got {other}"))),
    }
}

pub(super) fn one_of<'a>(args: &'a Value, key: &str, allowed: &[&str]) -> Result<Option<&'a str>, ToolError> {
    match str_arg(args, key)? {
        Some(v) if !allowed.contains(&v) => Err(ToolError::invalid_arguments(format!(
            "{key} must be one of {allowed:?}, got {v:?}"
        ))),
        other => Ok(other),
    }
}

/// `models/<name>/...` folder name: what the compiler, `bash -c` and the catalog all accept.
pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && !name.starts_with('-')
}

/// The default model name: the file stem, lowercased, anything else `_`.
pub(super) fn default_name(source: &Path) -> String {
    let stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("model");
    crate::import::materials::sanitize(stem)
}

fn windows_like(input: &str) -> bool {
    let b = input.as_bytes();
    input.starts_with("\\\\") || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
}

/// `source`: an absolute host path, a Windows path through the path map, or relative to the
/// active addon's content dir.
pub(super) fn resolve_source(state: &HammerState, input: &str) -> Result<PathBuf, ToolError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(ToolError::invalid_arguments("source is empty"));
    }
    if input.split(['/', '\\']).any(|s| s == "..") {
        return Err(ToolError::invalid_arguments(format!("source contains `..`: {input:?}")));
    }
    let host = if windows_like(input) {
        state.path_map.windows_to_host(input).ok_or_else(|| {
            ToolError::new(
                "PathNotMapped",
                "PathNotMapped",
                format!("windows path {input:?} has no host mapping (check --path-map)"),
            )
            .with_code(-32602)
            .retry_safe(true)
        })?
    } else if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        state.content().addon_root().join(input.replace('\\', "/"))
    };
    if !host.is_file() {
        return Err(ToolError::new(
            "SourceNotFound",
            "SourceNotFound",
            format!("{} is not a file", host.display()),
        )
        .with_code(-32602)
        .retry_safe(true)
        .with_data(json!({"source": host.display().to_string()})));
    }
    Ok(host)
}

pub(super) fn import_error(error: ImportError) -> ToolError {
    match error {
        ImportError::UnsupportedFormat { ext } => ToolError::new(
            "UnsupportedFormat",
            "UnsupportedFormat",
            format!("cannot import .{ext} files; supported: {}", supported().join(", ")),
        )
        .with_code(-32602)
        .retry_safe(true)
        .with_data(json!({"ext": ext, "supported": supported()})),
        ImportError::UnsupportedExtension(extensions) => ToolError::new(
            "UnsupportedExtension",
            "UnsupportedExtension",
            format!(
                "the glTF requires extensions this importer cannot read: {}; re-export without them \
                 (no Draco/meshopt/KTX2/quantization)",
                extensions.join(", ")
            ),
        )
        .with_code(-32602)
        .retry_safe(true)
        .with_data(json!({"gltfExtensions": extensions})),
        ImportError::Parse(detail) => {
            ToolError::new("ImportParse", "ImportParse", format!("cannot read the file: {detail}"))
                .with_code(-32602)
                .retry_safe(true)
                .with_data(json!({"detail": detail}))
        }
        ImportError::TooLarge { tris, cap } => ToolError::new(
            "TooLarge",
            "TooLarge",
            format!("{tris} triangles is over the {cap} cap; decimate the model first"),
        )
        .with_code(-32602)
        .retry_safe(true)
        .with_data(json!({"tris": tris, "cap": cap})),
        ImportError::Range(detail) => ToolError::new("OutOfRange", "OutOfRange", detail.clone())
            .with_code(-32602)
            .retry_safe(true)
            .with_data(json!({"detail": detail})),
        ImportError::Invalid(detail) => ToolError::invalid_arguments(detail),
        ImportError::Model(e) => vmdl_error(e),
        ImportError::NoSkin => ToolError::new(
            "NoSkin",
            "NoSkin",
            "kind player needs a skinned glTF (a skin with joints and JOINTS_0/WEIGHTS_0); this file has none. Import it as kind prop, or rig it first",
        )
        .with_code(-32602)
        .retry_safe(true),
        ImportError::BoneMap { detail, near } => ToolError::invalid_arguments(detail).did_you_mean(near),
        ImportError::BoneMapIncomplete { unmapped, required } => ToolError::new(
            "BoneMapIncomplete",
            "BoneMapIncomplete",
            format!(
                "no source joint found for the required CS2 bone(s) {}; map them with boneMap {{\"<your joint>\": \"<cs2 bone>\"}} (or set rig)",
                unmapped.join(", ")
            ),
        )
        .with_code(-32602)
        .retry_safe(true)
        .with_data(json!({"unmapped": unmapped, "required": required, "hint": "boneMap {sourceJoint: cs2Bone}; joint names are the glTF node names"})),
    }
}

/// `materialMap`: `Some` only with `materials:"existing"` (given without it is a mistake, not
/// ignored); every value a string.
pub(super) fn material_map_arg(args: &Value, existing: bool) -> Result<Option<Vec<(String, String)>>, ToolError> {
    let given = match args.get("materialMap") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(map)) => map,
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "materialMap must be an object {{\"<source material>\": \"materials/x.vmat\"}}, got {other}"
            )))
        }
    };
    if !existing {
        return Err(ToolError::invalid_arguments(
            "materialMap needs materials:\"existing\" (generate writes its own .vmat, none uses one neutral material)",
        ));
    }
    given
        .iter()
        .map(|(k, v)| match v.as_str().filter(|s| !s.trim().is_empty()) {
            Some(s) => Ok((k.clone(), s.to_string())),
            None => Err(ToolError::invalid_arguments(format!(
                "materialMap[{k:?}] must be a material path string, got {v}"
            ))),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// `(source material, resolved target)` pairs.
type MaterialTargets = Vec<(String, String)>;

/// Every `materialMap` target must be a `.vmat` that resolves: compiled in the addon or the game
/// (`asset_resolve`, the engine's order), or a `.vmat` source of the addon (noted: compile it).
pub(super) fn resolve_material_targets(
    paths: &Paths,
    map: Vec<(String, String)>,
) -> Result<(MaterialTargets, Vec<String>), ToolError> {
    let sources = Sources::new(paths, None);
    let mut out = Vec::new();
    let mut notes = Vec::new();
    let mut unknown: Vec<(String, String)> = Vec::new();
    for (from, to) in map {
        let key = norm(&to);
        let key = key.strip_suffix("_c").unwrap_or(&key).to_string();
        if !key.ends_with(".vmat") {
            return Err(ToolError::invalid_arguments(format!(
                "materialMap[{from:?}] = {to:?} is not a .vmat path (e.g. materials/dev/reflectivity_30.vmat)"
            )));
        }
        if sources.has(&format!("{key}_c")) {
            out.push((from, key));
        } else if find_ci(&paths.content, &key).is_some() {
            notes.push(format!("{key} is a source in the addon with no compiled _c yet: compile_asset it before the model renders"));
            out.push((from, key));
        } else {
            unknown.push((from, key));
        }
    }
    if let Some((_, first)) = unknown.first() {
        let near = nearest_materials(&sources, paths, first);
        return Err(ToolError::new(
            "UnknownMaterial",
            "UnknownMaterial",
            format!(
                "materialMap target(s) resolve nowhere (addon, game dirs, game paks): {}",
                unknown.iter().map(|(f, t)| format!("{f} -> {t}")).collect::<Vec<_>>().join(", ")
            ),
        )
        .with_code(-32602)
        .retry_safe(true)
        .with_data(json!({"unknown": unknown.iter().map(|(f, t)| json!({"material": f, "target": t})).collect::<Vec<_>>()}))
        .did_you_mean(near));
    }
    if !sources.pak_errors.is_empty() {
        notes.push(format!("game paks unreadable ({}): targets were checked in the addon only", sources.pak_errors.join("; ")));
    }
    Ok((out, notes))
}

/// Up to three existing materials in the target's folder closest to its name.
fn nearest_materials(sources: &Sources, paths: &Paths, target: &str) -> Vec<String> {
    let (dir, _) = target.rsplit_once('/').unwrap_or(("", target));
    let prefix = format!("{dir}/");
    let mut names: Vec<String> = Vec::new();
    for pak in &sources.paks {
        names.extend(
            pak.paths()
                .filter(|p| p.starts_with(&prefix) && p.ends_with(".vmat_c") && !p[prefix.len()..].contains('/'))
                .take(5000)
                .map(|p| p.trim_end_matches("_c").to_string()),
        );
    }
    if let Ok(entries) = std::fs::read_dir(paths.game.join(dir)) {
        names.extend(entries.flatten().filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_ascii_lowercase();
            n.strip_suffix("_c").filter(|n| n.ends_with(".vmat")).map(|n| format!("{prefix}{n}"))
        }));
    }
    cs2_mcp_common::tools::nearest_names(target, names.iter().map(String::as_str), 3)
}

pub(super) fn supported() -> Vec<&'static str> {
    import::CONVERTED.iter().chain(import::PASS_THROUGH).copied().collect()
}

/// `boneMap`: `{source joint: CS2 bone}`, every value a string.
fn bone_map_arg(args: &Value) -> Result<Vec<(String, String)>, ToolError> {
    match args.get("boneMap") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| match v.as_str().filter(|s| !s.trim().is_empty()) {
                Some(s) => Ok((k.clone(), s.trim().to_string())),
                None => Err(ToolError::invalid_arguments(format!("boneMap[{k:?}] must be a CS2 bone name string, got {v}"))),
            })
            .collect(),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "boneMap must be an object {{\"<source joint>\": \"<cs2 bone>\"}}, got {other}"
        ))),
    }
}

/// The CS2 player skeleton from the game paks (`worldmodel.vnmskel_c`), typed errors naming the path.
async fn load_skeleton(state: &HammerState) -> Result<nm::NmSkeleton, ToolError> {
    let paths = Paths::of(state);
    let bytes = tokio::task::spawn_blocking(move || Sources::new(&paths, None).read(nm::SKELETON_COMPILED))
        .await
        .map_err(|e| ToolError::new("IoError", "IoError", format!("skeleton lookup failed: {e}")))?;
    let Some(bytes) = bytes else {
        return Err(ToolError::new(
            "SkeletonNotFound",
            "SkeletonNotFound",
            format!(
                "kind player needs {} from the game (csgo/core pak or loose files); it was not found. Is the game installed and updated?",
                nm::SKELETON_COMPILED
            ),
        )
        .with_data(json!({"path": nm::SKELETON_COMPILED}))
        .retry_safe(true));
    };
    nm::from_resource_bytes(&bytes).map_err(|e| {
        ToolError::new("SkeletonRead", "SkeletonRead", format!("{} could not be read: {e}", nm::SKELETON_COMPILED))
            .with_data(json!({"path": nm::SKELETON_COMPILED}))
    })
}

/// The reference agent's attachments and hitboxes (`<agent>.vmdl_c` from the game paks or loose
/// files), typed errors naming the path.
async fn load_agent_markup(state: &HammerState, agent: &str) -> Result<retarget::agent::Markup, ToolError> {
    let paths = Paths::of(state);
    let compiled = format!("{agent}_c");
    let read_path = compiled.clone();
    let bytes = tokio::task::spawn_blocking(move || Sources::new(&paths, None).read(&read_path))
        .await
        .map_err(|e| ToolError::new("IoError", "IoError", format!("agent lookup failed: {e}")))?;
    let Some(bytes) = bytes else {
        return Err(ToolError::new(
            "AgentNotFound",
            "AgentNotFound",
            format!("referenceAgent {agent}: {compiled} was not found in the game (paks or loose files)"),
        )
        .with_data(json!({"referenceAgent": agent, "hint": "list_models {prefix:\"agents/models/\"}"}))
        .retry_safe(true));
    };
    let markup = retarget::agent::from_resource_bytes(&bytes).map_err(|e| {
        ToolError::new("AgentRead", "AgentRead", format!("{compiled} could not be read: {e}")).with_data(json!({"referenceAgent": agent}))
    })?;
    if markup.attachments.is_empty() && markup.hitbox_sets.iter().all(|s| s.hitboxes.is_empty()) {
        return Err(ToolError::invalid_arguments(format!(
            "referenceAgent {agent} carries no attachments and no hitboxes; name a player agent (agents/models/<agent>/<agent>.vmdl)"
        )));
    }
    Ok(markup)
}

/// Write a generated file unless the disk already holds exactly these bytes. A byte-identical
/// rewrite would only bump the mtime: the compiler then skips the input as current (same content
/// hash) while its output looks older than the source, which reads as a failed compile.
pub(super) fn write_generated(host: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    if std::fs::read(host).is_ok_and(|old| old == bytes) {
        return Ok(());
    }
    io_fail(
        &format!("cannot create {}", host.parent().unwrap_or(host).display()),
        std::fs::create_dir_all(host.parent().unwrap_or_else(|| Path::new("/"))),
    )?;
    io_fail(&format!("cannot write {}", host.display()), write_atomic_bytes_with_backup(host, bytes)).map(|_| ())
}

pub(super) fn io_fail<T>(what: &str, result: std::io::Result<T>) -> Result<T, ToolError> {
    result.map_err(|error| ToolError::new("IoError", "IoError", format!("{what}: {error}")).retry_safe(true))
}

pub async fn model_import(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let Some(source_arg) = str_arg(&args, "source")? else {
        return Err(ToolError::invalid_arguments("source (string) is required"));
    };
    let kind = match one_of(&args, "kind", &["prop", "world", "hammer_mesh", "player"])? {
        None | Some("prop") => Kind::Prop,
        Some("world") => Kind::World,
        Some("hammer_mesh") => return super::hammer_mesh::run(state, ctx, args).await,
        Some(_) => Kind::Player,
    };
    for key in ["boneMap", "rig", "referenceAgent"] {
        if kind != Kind::Player && args.get(key).is_some_and(|v| !v.is_null()) {
            return Err(ToolError::invalid_arguments(format!("{key} is only valid with kind player")));
        }
    }
    if kind == Kind::Player && args.get("upAxis").is_some_and(|v| !v.is_null()) {
        return Err(ToolError::invalid_arguments(
            "upAxis is not used with kind player: the orientation is derived from the rig (result retarget.facing)",
        ));
    }
    let rig_hint = match one_of(&args, "rig", &RigHint::NAMES)? {
        None => RigHint::Auto,
        Some(text) => RigHint::parse(text).unwrap_or_default(),
    };
    let bone_map = bone_map_arg(&args)?;
    let up = match one_of(&args, "upAxis", &["y", "z"])? {
        None => None,
        Some("y") => Some(UpAxis::Y),
        Some(_) => Some(UpAxis::Z),
    };
    let material_mode = one_of(&args, "materials", &["generate", "none", "existing"])?;
    let generate_materials = !matches!(material_mode, Some("none") | Some("existing"));
    let material_map = material_map_arg(&args, material_mode == Some("existing"))?;
    let physics = match one_of(&args, "physics", &["hull", "mesh", "none"])? {
        None => None,
        Some("hull") => Some(Physics::Hull),
        Some("mesh") => Some(Physics::Mesh),
        Some(_) => Some(Physics::None),
    };
    let scale = match args.get("scale") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_f64() {
            Some(s) if s.is_finite() && s > 0.0 => Some(s),
            _ => return Err(ToolError::invalid_arguments(format!("scale must be a positive number, got {v}"))),
        },
    };
    for key in ["origin", "angles", "maxTrisPerMesh"] {
        if args.get(key).is_some_and(|v| !v.is_null()) {
            return Err(ToolError::invalid_arguments(format!("{key} is only valid with kind hammer_mesh")));
        }
    }
    let compile = bool_arg(&args, "compile", true)?;
    let overwrite = bool_arg(&args, "overwrite", false)?;
    let dry_run = bool_arg(&args, "dryRun", false)?;

    let source = resolve_source(&state, source_arg)?;
    let ext = import::extension_of(&source);
    if !supported().contains(&ext.as_str()) {
        return Err(import_error(ImportError::UnsupportedFormat { ext }));
    }
    let name = match str_arg(&args, "name")? {
        Some(n) if valid_name(n) => n.to_string(),
        Some(n) => {
            return Err(ToolError::invalid_arguments(format!(
                "name {n:?} must be 1-64 characters of [a-z0-9_-]"
            )))
        }
        None => default_name(&source),
    };

    let content = state.content();
    let addon_root = content.addon_root();
    let model_rel = format!("models/{name}/{name}.vmdl");
    let model_host = addon_root.join(&model_rel);
    if model_host.exists() && !overwrite {
        return Err(ToolError::new(
            "ModelExists",
            "ModelExists",
            format!("{model_rel} already exists in addon {} (pass overwrite:true to replace it)", state.addon()),
        )
        .with_code(-32602)
        .retry_safe(true)
        .with_data(json!({"model": model_rel, "hint": "overwrite:true, or pick another name"})));
    }

    let modeldoc_warning = crate::tools::addon::modeldoc_addon_warning(&state).await;
    let mut warnings_early: Vec<String> = Vec::new();
    let existing = match material_map {
        Some(map) => {
            let paths = Paths::of(&state);
            let (resolved, notes) = tokio::task::spawn_blocking(move || resolve_material_targets(&paths, map))
                .await
                .map_err(|e| ToolError::new("IoError", "IoError", format!("material lookup failed: {e}")))??;
            warnings_early.extend(notes);
            Some(resolved)
        }
        None if material_mode == Some("existing") => Some(Vec::new()),
        None => None,
    };
    let player = if kind == Kind::Player {
        let agent = match str_arg(&args, "referenceAgent")? {
            None => Some(retarget::agent::DEFAULT_AGENT.to_string()),
            Some("none") => None,
            Some(input) => Some(
                crate::tools::model_catalog::normalize_model_path(input)
                    .map_err(|e| ToolError::invalid_arguments(format!("referenceAgent: {e} (or \"none\")")))?,
            ),
        };
        let skeleton = load_skeleton(&state).await?;
        let markup = match agent {
            Some(agent) => {
                let markup = load_agent_markup(&state, &agent).await?;
                Some((agent, markup))
            }
            None => None,
        };
        Some(PlayerOptions { skeleton, bone_map, rig: rig_hint, markup })
    } else {
        None
    };
    let opts = ImportOptions { name: name.clone(), kind, scale, up, generate_materials, existing, physics, player };
    let plan: ImportPlan = {
        let source = source.clone();
        let read = io_fail(&format!("cannot read {}", source.display()), std::fs::read(&source))?;
        tokio::task::spawn_blocking(move || import::plan(&source, read, &opts))
            .await
            .map_err(|e| ToolError::new("IoError", "IoError", format!("import task failed: {e}")))?
            .map_err(import_error)?
    };
    let mut warnings = warnings_early;
    warnings.extend(plan.warnings.iter().cloned());
    warnings.extend(modeldoc_warning);
    let checked = check_classes(&plan.vmdl, &state, true)?;
    warnings.extend(checked.warnings);

    let mut hosts: Vec<PathBuf> = Vec::new();
    for file in &plan.files {
        let host = addon_root.join(&file.path);
        if !content.in_addon(&host) {
            return Err(ToolError::new(
                "WriteOutsideAddon",
                "WriteOutsideAddon",
                format!("{} would land outside the active addon", host.display()),
            )
            .retry_safe(true));
        }
        // byte-identical files are not rewritten (`write_generated`), so they are not "replaced"
        if file.role != "model" && std::fs::read(&host).is_ok_and(|old| old != file.bytes) {
            warnings.push(format!("replaced existing {} (backup kept beside it)", file.path));
        }
        hosts.push(host);
    }

    if !dry_run {
        for (file, host) in plan.files.iter().zip(&hosts) {
            write_generated(host, &file.bytes)?;
        }
    }

    let files: Vec<Value> = plan
        .files
        .iter()
        .map(|f| json!({"path": f.path, "role": f.role, "bytes": f.bytes.len()}))
        .collect();
    let materials: Vec<Value> = plan
        .materials
        .iter()
        .map(|m| {
            let mut entry = json!({"vmat": m.vmat, "textures": m.textures});
            if !m.dropped.is_empty() {
                entry["dropped"] = json!(m.dropped);
            }
            entry
        })
        .collect();
    let mesh = match &plan.mesh {
        Some(m) => json!({"tris": m.tris, "verts": m.verts, "materials": m.materials}),
        None => Value::Null,
    };

    let mut compile_json = Value::Null;
    let mut compiled = false;
    let mut compile_failure: Option<ToolError> = None;
    if compile && !dry_run {
        let timeout = compile_timeout(&json!({}), &ctx.budget)?;
        match compile_resourcecompiler(state.clone(), &model_rel, model_host.clone(), CompileTarget::ANY_TOOLS, timeout, overwrite.then_some(true)).await {
            Ok(out) => {
                let ok = out.json.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let fail_lines = out.json.get("failLines").cloned().unwrap_or_else(|| json!([]));
                compile_json = json!({"ok": ok, "failLines": fail_lines, "jobId": out.json.get("jobId").cloned().unwrap_or(Value::Null)});
                if ok {
                    compiled = true;
                } else {
                    compile_failure = Some(
                        ToolError::new(
                            "CompileFailed",
                            "CompileFailed",
                            format!("{model_rel} was written but did not compile: {}", first_line(&fail_lines)),
                        )
                        .retry_safe(true)
                        .with_data(json!({
                            "model": model_rel,
                            "failLines": fail_lines,
                            "reason": out.json.get("reason").cloned().unwrap_or(Value::Null),
                            "files": files,
                            "filesKept": true,
                        }))
                        .use_instead("model_compile", "retry after fixing the cause; the files stay on disk", Some(json!({"path": model_rel}))),
                    );
                }
            }
            Err(error) => {
                // the files are written: report that the compile could not run, not a bare error
                compile_json = json!({"ok": false, "failLines": [], "error": error.message, "errorType": error.type_});
                warnings.push(format!("not compiled: {}", error.message));
            }
        }
    }
    if let Some(failure) = compile_failure {
        return Err(failure);
    }

    // per-material detail lives in `materials[].dropped`; this is the whole-file list
    let dropped = plan.dropped.clone();
    let result = json!({
        "model": model_rel,
        "addon": state.addon(),
        "dryRun": dry_run,
        "written": !dry_run,
        "kind": match kind { Kind::World => "world", Kind::Player => "player", Kind::Prop => "prop" },
        "scale": plan.scale,
        "files": files,
        "materials": materials,
        "mesh": mesh,
        "physics": plan.physics.as_str(),
        "compile": compile_json,
        "dropped": dropped,
        "warnings": warnings,
    });
    let mut result = result;
    if !plan.material_names.is_empty() {
        result["materialNames"] = json!(plan.material_names);
    }
    if material_mode == Some("existing") {
        result["remapped"] = json!(plan.remapped.iter().map(|(from, to)| (from.clone(), json!(to))).collect::<serde_json::Map<_, _>>());
    }
    if let Some(rig) = &plan.rig {
        result["skeleton"] = json!({
            "bones": rig.bones,
            "root": rig.root,
            "names": rig.names.iter().map(|(source, smd)| json!({"source": source, "smd": smd})).collect::<Vec<_>>(),
        });
        result["animations"] = json!(rig
            .animations
            .iter()
            .map(|(name, frames)| json!({"name": name, "frames": frames, "fps": import::gltf_rig::FPS}))
            .collect::<Vec<_>>());
    }
    if let Some(report) = &plan.player {
        let (skeleton, retarget) = report.json();
        result["skeleton"] = skeleton;
        result["retarget"] = retarget;
    }
    if let Some((agent, fitted)) = &plan.markup {
        result["markup"] = json!({
            "referenceAgent": agent,
            "attachments": fitted.attachments.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
            "hitboxSets": fitted.hitbox_sets.iter().map(|s| json!({"name": s.name, "hitboxes": s.hitboxes.len()})).collect::<Vec<_>>(),
            "moved": fitted.moved.iter().map(|(a, from, to)| json!({"attachment": a, "from": from, "to": to})).collect::<Vec<_>>(),
            "dropped": fitted.dropped,
        });
    }
    let mut out = ToolOutput::from(result);
    if dry_run {
        let mut again = json!({"source": source_arg, "name": name, "dryRun": false});
        if kind == Kind::Player {
            again["kind"] = json!("player");
        }
        for key in ["materials", "materialMap", "boneMap", "rig", "scale", "referenceAgent"] {
            if let Some(v) = args.get(key).filter(|v| !v.is_null()) {
                again[key] = v.clone();
            }
        }
        return Ok(out.with_next_args("model_import", "write it for real", again));
    }
    if !compiled {
        out = out.with_next_args("model_compile", "build the .vmdl_c (not compiled yet)", json!({"path": model_rel}));
    }
    if kind == Kind::Player {
        out = out
            .with_next_args("get_model_info", "bounds of the compiled model", json!({"model": model_rel}))
            .with_next_args(
                "create_props",
                "preview it as prop_dynamic: the worldmodel AnimGraph2 poses it (use_animgraph defaults to on)",
                json!({"props": [{"class": "prop_dynamic", "model": model_rel, "keyValues": {}}]}),
            )
            .with_next_args(
                "player_model",
                "make the real player pawn wear it (cs_script; needs a cs_script project; then save_map, compile_map, check in a client-mcp session)",
                json!({"action": "set", "model": model_rel}),
            )
            .with_next("save_map", "then compile_map (run_map {unload:true} first if the map is loaded), then run_map")
            .with_next_args("look", "frames mode shows the pose; compare with a real agents/models/tm_phoenix/tm_phoenix.vmdl beside it (front is +X). A running engine keeps a model it already loaded: restart the tools session after re-importing", json!({"mode": "frames"}));
        return Ok(out);
    }
    let class = if kind == Kind::World {
        "prop_static"
    } else {
        "prop_dynamic"
    };
    // a skinned model: place it playing its first clip (key names from `props_live`'s `keyValues`)
    let first_clip = plan.rig.as_ref().and_then(|r| r.animations.first()).map(|a| a.0.clone());
    let place_kv = |prop: Value| match &first_clip {
        Some(clip) => {
            let mut prop = prop;
            prop["keyValues"] = json!({"StartingAnim": clip, "StartingAnimationLoopMode": "ANIM_LOOP_MODE_LOOPING"});
            prop
        }
        None => prop,
    };
    out = out
        .with_next_args("get_model_info", "bounds, placement, sequences of the compiled model", json!({"model": model_rel}))
        .with_next_args(
            "create_props",
            if kind == Kind::World {
                "place it (prop_static: baked into the map)"
            } else {
                "place it (prop_dynamic; prop_physics if it should fall)"
            },
            json!({"props": [place_kv(json!({"class": class, "model": model_rel}))]}),
        )
        .with_next("save_map", "then compile_map (run_map {unload:true} first if the map is loaded in game), then run_map");
    let look_why = "check it in game; look {target:<nodeID>} frames custom props with a generic box, so pass a pose from get_model_info bounds. A running engine keeps a model it already loaded: after re-importing, restart the tools session to see the change";
    out = match first_clip {
        Some(_) => out.with_next_args("look", &format!("frames mode shows the animation playing (+X is the model's front). {look_why}"), json!({"mode": "frames"})),
        None => out.with_next("look", look_why),
    };
    out = out
        .with_next("report_ready", "ship check: materials resolve to textures");
    Ok(out)
}

pub(super) fn first_line(fail_lines: &Value) -> String {
    fail_lines
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .unwrap_or("see failLines")
        .to_string()
}
