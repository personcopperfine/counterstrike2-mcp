//! `model_import kind:"hammer_mesh"`: a glTF/GLB as Hammer meshes in the OPEN map. The conversion
//! is [`crate::import::hammer_mesh`] (pure); this file is the live half:
//!
//! 1. refuse before touching anything: the bridge must advertise `mesh.create` and
//!    `mesh.setFaceTexture` and be m3.18 or newer (the `axes` mode; a bridge that answers no
//!    `bridgeVersion` is trusted);
//! 2. write the materials (`materials/models/<name>/`) and compile them (`compile`), so a failure
//!    costs nothing in the map;
//! 3. per chunk: `mesh.create` ([`crate::tools::mesh_live::send_geometry`], the `create_mesh`
//!    pipeline), pair the sent faces with Hammer's face indices (`mesh.get`, centroid match;
//!    creation order when the bridge has no `mesh.get`), then ONE `mesh.setFaceTexture` `axes`
//!    call ([`crate::tools::mesh_live::send_align`]).
//!
//! A chunk is two undo steps (the create, then the align): `undo_last` twice removes the last
//! chunk, `delete_nodes` removes any set. A failure part-way is `ChunkFailed` with
//! `data.created` (every node already in the map).
use crate::import::chunk::{self, Chunk};
use crate::import::hammer_mesh::{self, MeshOptions, MeshPlan, DEFAULT_TRIS_PER_MESH, MAX_TRIS_PER_MESH};

/// The smallest `maxTrisPerMesh`: one merged face holds up to `MAX_LOOP - 2` triangles, and a face
/// is never split across chunks.
const MIN_TRIS_PER_MESH: usize = crate::import::planar::MAX_LOOP - 2;
use crate::import::smd::UpAxis;
use crate::state::HammerState;
use crate::tools::asset_resolve::Paths;
use crate::tools::compile::{compile_resourcecompiler_set, compile_timeout, CompileFile, CompileTarget};
use crate::tools::mesh_live::{self, send_align, send_geometry, Payload};
use crate::tools::model_import::{
    bool_arg, default_name, import_error, io_fail, material_map_arg, one_of, resolve_material_targets, resolve_source,
    str_arg, valid_name,
};
use cs2_mcp_common::tools::{require_caps, CallCtx, ToolError, ToolOutput};
use serde_json::{json, Map, Value};

/// The first bridge build with `mesh.setFaceTexture` `mode:"axes"`.
const AXES_BUILD: (u32, u32) = (3, 18);
const NODE_NOTE: &str = "each chunk is two undo steps (create, then align): undo_last twice removes the last chunk";

fn vec3(args: &Value, key: &str) -> Result<[f64; 3], ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok([0.0; 3]),
        Some(v) => {
            let list: Option<Vec<f64>> = v.as_array().filter(|a| a.len() == 3).and_then(|a| {
                a.iter().map(|x| x.as_f64().filter(|x| x.is_finite() && x.abs() < 1e6)).collect()
            });
            list.map(|l| [l[0], l[1], l[2]])
                .ok_or_else(|| ToolError::invalid_arguments(format!("{key} must be [x, y, z], three finite numbers, got {v}")))
        }
    }
}

/// `"m3.18"` -> `(3, 18)`; `None` for anything that is not `m<major>.<minor>`.
fn parse_build(text: &str) -> Option<(u32, u32)> {
    let (major, minor) = text.strip_prefix('m')?.split_once('.')?;
    Some((major.parse().ok()?, minor.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?))
}

/// The capability gate: methods advertised, and (when the bridge names its build) m3.18+.
async fn gate(state: &HammerState) -> Result<(), ToolError> {
    require_caps(&state.bridge, &["mesh.create", "mesh.setFaceTexture"]).await?;
    let version = {
        let bridge = state.bridge.lock().await;
        bridge
            .hello_result
            .as_ref()
            .and_then(|h| h.get("bridgeVersion"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    if let Some(text) = version {
        if parse_build(&text).is_some_and(|v| v < AXES_BUILD) {
            return Err(ToolError::not_implemented(
                "CapabilityMissing",
                format!(
                    "the bridge is {text}; kind hammer_mesh needs m3.18 (mesh.setFaceTexture mode axes). \
                     Nothing was sent. Reinstall the bridge DLL (`cargo bridge install`)"
                ),
                json!({"missing": ["mesh.setFaceTexture mode axes"], "bridgeVersion": text, "needs": "m3.18"}),
            ));
        }
    }
    Ok(())
}

fn chunk_failed(created: &[i64], index: usize, total: usize, stage: &str, cause: ToolError) -> ToolError {
    let mut error = ToolError::new(
        "ChunkFailed",
        "ChunkFailed",
        format!(
            "chunk {}/{total} failed at {stage}: {}. {} node(s) are already in the open map {created:?} (not saved); \
             delete_nodes removes them ({NODE_NOTE})",
            index + 1,
            cause.message,
            created.len(),
        ),
    )
    .with_data(json!({
        "created": created,
        "failedChunk": index,
        "chunks": total,
        "stage": stage,
        "cause": cause.to_json(),
    }));
    error.outcome_unknown = cause.outcome_unknown;
    if !created.is_empty() {
        error = error.use_instead("delete_nodes", "remove the chunks already created", Some(json!({"nodeIDs": created})));
    }
    error
}

/// Hammer's face index for each face of `chunk` (`None` = could not be paired), and a note when
/// the pairing is not the plain creation order.
async fn face_indices(
    state: &HammerState,
    ctx: &CallCtx,
    node_id: i64,
    chunk: &Chunk,
    created_faces: usize,
) -> Result<(Vec<Option<usize>>, Option<String>), ToolError> {
    let sent = chunk.faces.len();
    let identity = |why: Option<String>| {
        if created_faces == sent {
            ((0..sent).map(Some).collect::<Vec<_>>(), why)
        } else {
            (
                vec![None; sent],
                Some(format!(
                    "node {node_id}: Hammer created {created_faces} of {sent} faces and mesh.get could not pair them: textures left at Hammer's default"
                )),
            )
        }
    };
    let deadline = ctx.budget.deadline_ms()?;
    let doc = mesh_live::require_document(state, deadline).await?;
    match crate::tools::mesh_read::read_live_mesh(state, deadline, &doc, node_id).await {
        Ok(Some(record)) => {
            let loops = |vertices: &[[f64; 3]], faces: &[Vec<usize>]| -> Vec<Vec<[f64; 3]>> {
                faces.iter().map(|f| f.iter().filter_map(|&i| vertices.get(i).copied()).collect()).collect()
            };
            let planned = loops(&chunk.vertices, &chunk.faces);
            let actual = loops(&record.vertices, &record.faces);
            let paired = chunk::match_faces(&planned, &actual);
            let unmatched = paired.iter().filter(|p| p.is_none()).count();
            let reordered = paired.iter().enumerate().any(|(i, p)| *p != Some(i));
            let note = if unmatched > 0 {
                Some(format!("node {node_id}: {unmatched} of {sent} face(s) could not be paired with Hammer's faces (welded or dropped): left at the default texture"))
            } else if reordered {
                Some(format!("node {node_id}: Hammer numbers the faces in another order than sent; paired by position"))
            } else {
                None
            };
            Ok((paired, note))
        }
        Ok(None) => Ok(identity(Some(format!("node {node_id}: the bridge has no mesh.get; assumed Hammer's face order = creation order")))),
        Err(error) if crate::tools::mesh_read::is_too_large(&error) => {
            Ok(identity(Some(format!("node {node_id}: too large for mesh.get; assumed Hammer's face order = creation order"))))
        }
        Err(error) => Err(error),
    }
}

pub(super) async fn run(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let Some(source_arg) = str_arg(&args, "source")? else {
        return Err(ToolError::invalid_arguments("source (string) is required"));
    };
    if args.get("physics").is_some_and(|v| !v.is_null()) {
        return Err(ToolError::invalid_arguments("physics does not apply to kind hammer_mesh (meshes carry their own collision)"));
    }
    let up = match one_of(&args, "upAxis", &["y", "z"])? {
        None => None,
        Some("y") => Some(UpAxis::Y),
        Some(_) => Some(UpAxis::Z),
    };
    let material_mode = one_of(&args, "materials", &["generate", "none", "existing"])?;
    let material_map = material_map_arg(&args, material_mode == Some("existing"))?;
    let scale = match args.get("scale") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_f64() {
            Some(s) if s.is_finite() && s > 0.0 => Some(s),
            _ => return Err(ToolError::invalid_arguments(format!("scale must be a positive number, got {v}"))),
        },
    };
    let compile = bool_arg(&args, "compile", true)?;
    let dry_run = bool_arg(&args, "dryRun", false)?;
    let overwrite = bool_arg(&args, "overwrite", false)?;
    let origin = vec3(&args, "origin")?;
    let angles = vec3(&args, "angles")?;
    let max_tris = match args.get("maxTrisPerMesh") {
        None | Some(Value::Null) => DEFAULT_TRIS_PER_MESH,
        Some(v) => match v.as_u64().filter(|n| (MIN_TRIS_PER_MESH as u64..=MAX_TRIS_PER_MESH as u64).contains(n)) {
            Some(n) => n as usize,
            None => {
                return Err(ToolError::invalid_arguments(format!(
                    "maxTrisPerMesh must be an integer {MIN_TRIS_PER_MESH}..={MAX_TRIS_PER_MESH}, got {v}"
                )))
            }
        },
    };

    let source = resolve_source(&state, source_arg)?;
    let ext = crate::import::extension_of(&source);
    if !crate::import::CONVERTED.contains(&ext.as_str()) {
        return Err(ToolError::new(
            "UnsupportedFormat",
            "UnsupportedFormat",
            format!("kind hammer_mesh reads glTF/GLB only, not .{ext} (fbx/obj/smd/dmx pass through as props: kind prop or world)"),
        )
        .with_code(-32602)
        .retry_safe(true)
        .with_data(json!({"ext": ext, "supported": crate::import::CONVERTED})));
    }
    let name = match str_arg(&args, "name")? {
        Some(n) if valid_name(n) => n.to_string(),
        Some(n) => return Err(ToolError::invalid_arguments(format!("name {n:?} must be 1-64 characters of [a-z0-9_-]"))),
        None => default_name(&source),
    };

    let mut warnings: Vec<String> = Vec::new();
    let existing = match material_map {
        Some(map) => {
            let paths = Paths::of(&state);
            let (resolved, notes) = tokio::task::spawn_blocking(move || resolve_material_targets(&paths, map))
                .await
                .map_err(|e| ToolError::new("IoError", "IoError", format!("material lookup failed: {e}")))??;
            warnings.extend(notes);
            Some(resolved)
        }
        None if material_mode == Some("existing") => Some(Vec::new()),
        None => None,
    };
    let opts = MeshOptions {
        name: name.clone(),
        scale,
        up,
        generate_materials: !matches!(material_mode, Some("none") | Some("existing")),
        existing,
        origin,
        angles,
        max_tris,
    };
    let plan: MeshPlan = {
        let source = source.clone();
        let read = io_fail(&format!("cannot read {}", source.display()), std::fs::read(&source))?;
        tokio::task::spawn_blocking(move || hammer_mesh::plan(&source, &read, &opts))
            .await
            .map_err(|e| ToolError::new("IoError", "IoError", format!("import task failed: {e}")))?
            .map_err(import_error)?
    };
    warnings.extend(plan.warnings.iter().cloned());
    if plan.chunks.is_empty() {
        return Err(ToolError::new(
            "ImportParse",
            "ImportParse",
            "the file has no usable triangles (all degenerate or collapsed)".to_string(),
        )
        .with_code(-32602)
        .retry_safe(true));
    }
    for path in &plan.material_paths {
        mesh_live::material_text("material", path, mesh_live::DEFAULT_MATERIAL)?;
    }

    let content = state.content();
    let addon_root = content.addon_root();
    let mut hosts = Vec::new();
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
        hosts.push(host);
    }
    // The materials share `materials/models/<name>/` with a prop import of that name: never replace
    // another import's files unasked (the prop kind's ModelExists rule).
    let existing: Vec<&str> = plan.files.iter().zip(&hosts).filter(|(_, h)| h.exists()).map(|(f, _)| f.path.as_str()).collect();
    let differs = |path: &str| {
        plan.files.iter().zip(&hosts).any(|(f, h)| f.path == path && std::fs::read(h).map_or(true, |old| old != f.bytes))
    };
    if !existing.is_empty() && !overwrite {
        return Err(ToolError::new(
            "ModelExists",
            "ModelExists",
            format!("{} already exist(s) in addon {} (pass overwrite:true to replace them)", existing.join(", "), state.addon()),
        )
        .with_code(-32602)
        .retry_safe(true)
        .with_data(json!({"files": existing, "hint": "overwrite:true, or pick another name"})));
    }
    for path in existing.iter().filter(|p| differs(p)) {
        warnings.push(format!("replaced existing {path} (backup kept beside it)"));
    }

    let faces_per_chunk: Vec<usize> = plan.chunks.iter().map(|c| c.faces.len()).collect();
    let tris_per_chunk: Vec<usize> = plan.chunks.iter().map(|c| c.tris).collect();
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
    let base = |written: bool, warnings: &[String]| {
        let mut result = json!({
            "kind": "hammer_mesh",
            "addon": state.addon(),
            "dryRun": dry_run,
            "written": written,
            "scale": plan.scale,
            "origin": origin,
            "angles": angles,
            "chunks": plan.chunks.len(),
            "facesPerChunk": faces_per_chunk,
            "trisPerChunk": tris_per_chunk,
            "uvMode": plan.uv_mode,
            "materials": materials,
            "mesh": {"tris": plan.tris(), "faces": plan.planar.faces.len() - plan.dropped_faces, "verts": plan.verts},
            "dropped": plan.dropped,
            "warnings": warnings,
        });
        if !plan.material_names.is_empty() {
            result["materialNames"] = json!(plan.material_names);
        }
        if material_mode == Some("existing") {
            result["remapped"] = json!(plan.remapped.iter().map(|(from, to)| (from.clone(), json!(to))).collect::<Map<_, _>>());
        }
        result
    };

    if dry_run {
        let mut result = base(false, &warnings);
        result["files"] = json!(plan.files.iter().map(|f| json!({"path": f.path, "role": f.role, "bytes": f.bytes.len()})).collect::<Vec<_>>());
        let mut again = json!({"source": source_arg, "kind": "hammer_mesh", "name": name});
        for key in ["materials", "materialMap", "scale", "origin", "angles", "maxTrisPerMesh", "upAxis", "compile", "overwrite"] {
            if let Some(v) = args.get(key).filter(|v| !v.is_null()) {
                again[key] = v.clone();
            }
        }
        return Ok(ToolOutput::from(result).with_next_args("model_import", "create the meshes for real (needs the live bridge m3.18)", again));
    }

    gate(&state).await?;

    // materials first: a failure here costs nothing in the map
    for (file, host) in plan.files.iter().zip(&hosts) {
        super::model_import::write_generated(host, &file.bytes)?;
    }
    let mut compile_json = Value::Null;
    let vmats: Vec<CompileFile> = plan
        .files
        .iter()
        .zip(&hosts)
        .filter(|(f, _)| f.role == "material")
        .map(|(f, h)| CompileFile { rel: f.path.clone(), host: h.clone(), referenced_by: None })
        .collect();
    if compile && !vmats.is_empty() {
        let timeout = compile_timeout(&json!({}), &ctx.budget)?;
        let out = compile_resourcecompiler_set(state.clone(), vmats, Vec::new(), CompileTarget::ANY_TOOLS, timeout, overwrite.then_some(true)).await?;
        let ok = out.json.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let fail_lines = out.json.get("failLines").cloned().unwrap_or_else(|| json!([]));
        compile_json = json!({"ok": ok, "files": out.json.get("files").cloned().unwrap_or(Value::Null), "failLines": fail_lines});
        if !ok {
            return Err(ToolError::new(
                "CompileFailed",
                "CompileFailed",
                "the generated material(s) did not compile; nothing was added to the map (the files stay on disk)".to_string(),
            )
            .retry_safe(true)
            .with_data(json!({"compile": compile_json, "filesKept": true})));
        }
    }

    let _gate = state.write_gate().await;
    let total = plan.chunks.len();
    let mut created: Vec<i64> = Vec::new();
    let mut exact = true;
    let mut inexact: Vec<i64> = Vec::new();
    let mut unaligned: Vec<i64> = Vec::new();
    let mut single_step: Vec<i64> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for (ci, chunk) in plan.chunks.iter().enumerate() {
        let payload = Payload { vertices: chunk.vertices.clone(), faces: chunk.faces.clone(), face_materials: None };
        let material = &plan.material_paths[chunk.material];
        let made = send_geometry(&state, &ctx, "mesh.create", None, payload, material, "undo_last removes the mesh", None)
            .await
            .map_err(|e| chunk_failed(&created, ci, total, "create", e))?;
        let Some(node_id) = made.get("nodeID").and_then(Value::as_i64).filter(|id| *id >= 0) else {
            return Err(chunk_failed(
                &created,
                ci,
                total,
                "create",
                ToolError::internal("the bridge reported no usable nodeID for the mesh it built (it exists in the document: find it with list_meshes after save_map)"),
            ));
        };
        created.push(node_id);
        let created_faces = made.get("faceCount").and_then(Value::as_u64).map_or(chunk.faces.len(), |n| n as usize);
        let (indices, note) = face_indices(&state, &ctx, node_id, chunk, created_faces)
            .await
            .map_err(|e| chunk_failed(&created, ci, total, "pair faces", e))?;
        notes.extend(note);
        let entries: Vec<Value> = chunk
            .face_ids
            .iter()
            .zip(&indices)
            .filter_map(|(&id, index)| index.map(|i| chunk::align_entry(i, &plan.planar.faces[id].axes)))
            .collect();
        if entries.len() < chunk.faces.len() {
            unaligned.push(node_id);
        }
        if entries.is_empty() {
            single_step.push(node_id);
            continue;
        }
        let params = json!({"nodeID": node_id, "mode": "axes", "faces": entries});
        let aligned = send_align(&state, &ctx, params, node_id)
            .await
            .map_err(|e| chunk_failed(&created, ci, total, "align", e))?;
        if aligned.get("exact").and_then(Value::as_bool) == Some(false) {
            exact = false;
            inexact.push(node_id);
        }
    }
    warnings.extend(notes);
    if !inexact.is_empty() {
        warnings.push(format!("Hammer stored different texture axes than requested on node(s) {inexact:?} (read-back not exact)"));
    }
    if !unaligned.is_empty() {
        warnings.push(format!("faces left at the default texture on node(s) {unaligned:?}"));
    }

    let mut result = base(true, &warnings);
    result["nodeIDs"] = json!(created);
    result["uvExact"] = json!(exact);
    result["undoNote"] = if single_step.is_empty() {
        json!(NODE_NOTE)
    } else {
        json!(format!("{NODE_NOTE}, except node(s) {single_step:?}: one step (create only, nothing aligned)"))
    };
    result["unsavedBridgeEdits"] = json!(state.unsaved_edits());
    result["saved"] = json!(false);
    result["files"] = json!(plan.files.iter().map(|f| json!({"path": f.path, "role": f.role, "bytes": f.bytes.len()})).collect::<Vec<_>>());
    if !compile_json.is_null() {
        result["compile"] = compile_json;
    }
    let first = created.first().copied().unwrap_or(0);
    Ok(ToolOutput::from(result)
        .with_next_args("look", "check the textures in the viewport", json!({"target": first}))
        .with_next("save_map", "write the meshes into the .vmap (nothing is auto-saved)")
        .with_next("compile_map", "build the map (run_map {unload:true} first if it is loaded in game)")
        .with_next("run_map", "load it and look at it in game"))
}
