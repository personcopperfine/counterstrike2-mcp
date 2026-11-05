//! Mesh reads from the SAVED `.vmap` (file route, `crate::vmap::mesh`): `list_meshes`,
//! `get_mesh`. Same contract as `entities_file.rs` — path handling, `source.fromDisk`, `staleRead`
//! — with the mesh summaries/geometry of POC `h_list_meshes` / `h_get_mesh`.
//!
//! Differences from the POC, following the Rust file-route precedent:
//! * the optional `path` argument (no active document → `NoDocument` naming `path`, as
//!   `entities_file` does; the POC only reads the active document);
//! * the caught-up envelope ([`read_envelope`]: `staleRead` boolean + `staleReason`) instead of
//!   the POC's collected `staleRead` object;
//! * a mesh whose element corrupts one of the edge-walk arrays is skipped by the extraction
//!   (the POC's `IndexError` would crash the server; see `vmap::mesh`).
//!
//! `get_mesh` without `path` reads the live document through the bridge's `mesh.get` when it is
//! advertised (`source:"live"`, [`crate::tools::mesh_read`]: node-local geometry converted to world
//! with the mesh's own transform, the same one the file extraction applies), else — and always
//! with `path` — the saved file (`source:"file"`). Unknown nodeID is `MeshNotFound` naming the
//! id, with the POC's save_map hint. Per-face `faceInfo` reuses the POC's Newell
//! normal / centroid formulas (`vmap::mesh::face_info_json`); ints in `faces` stay ints and the
//! per-face material always matches by face index.

use crate::state::HammerState;
use crate::tools::live_common::read_active_document;
use crate::vmap::mesh::MeshRecord;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};

/// A world-space bounds box (`within={min,max}` on `list_meshes`).
type BoundsBox = ([f64; 3], [f64; 3]);

/// Registration bound shared with the POC tool schema (`limit` is 1..5000 for `list_meshes`).
const LIMIT_MAX: u64 = 5000;
const LIMIT_DEFAULT: u64 = 500;
/// Same bound as the live route / `entities_file` (the bridge parses a live nodeID as `int`).
const MAX_NODE_ID: i64 = 2_147_483_647;

/// Register one of the two mesh file-route tools (called at its POC position by `hammer.rs`).
pub fn register_one(r: &mut Registry<HammerState>, name: &str) {
    match name {
        "list_meshes" => r.register(
            ToolSpec::new(
                "list_meshes",
                "List mesh (brush) nodes of a .vmap: nodeID, origin, bounds, vertex/face counts, materials. Reads the SAVED file (reports staleRead if unsaved edits). Filter with within={min,max} (bounds overlap).",
                ReadOnly,
                &[],
            )
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "path": path_schema(),
                    "within": {
                        "type": "object",
                        "description": "meshes whose bounds overlap this box",
                        "properties": {
                            "min": {"type": "array", "items": {"type": "number"},
                                    "minItems": 3, "maxItems": 3},
                            "max": {"type": "array", "items": {"type": "number"},
                                    "minItems": 3, "maxItems": 3}
                        },
                        "required": ["min", "max"],
                        "additionalProperties": false
                    },
                    "limit": {
                        "type": "integer", "minimum": 1, "maximum": LIMIT_MAX
                    }
                },
                "additionalProperties": false
            })),
            list_meshes,
        ),
        "get_mesh" => r.register(
            ToolSpec::new(
                "get_mesh",
                "Full geometry of one mesh by nodeID: WORLD vertices, faces (counter-clockwise loops), faceMaterials, faceInfo (centre/normal), bounds. Without `path` reads the live document (source \"live\", unsaved edits included; face order matches the mesh edit tools), else the saved file. A very large live mesh is refused with MeshTooLarge: save_map and retry.",
                ReadOnly,
                &[],
            )
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "path": path_schema(),
                    "nodeID": {
                        "type": "integer", "minimum": 0, "maximum": MAX_NODE_ID
                    }
                },
                "required": ["nodeID"],
                "additionalProperties": false
            })),
            get_mesh,
        ),
        other => panic!("mesh_file: unknown tool {other}"),
    }
}

fn path_schema() -> Value {
    json!({
        "type": "string",
        "description": "SAVED .vmap path (addon-relative or absolute); omitted = active document"
    })
}

// -------------------------------------------------------------------------------------------
// list_meshes
// -------------------------------------------------------------------------------------------

/// Every CMapMesh of the SAVED `.vmap` (summaries; `within={min,max}` keeps the meshes whose
/// bounds overlap the box, all three axes at once), at most `limit` summaries.
pub async fn list_meshes(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let within = within_box(&args)?;
    let limit = limit_arg(&args)?;
    let (host, document, is_active, dirty) =
        super::entities_file::resolve_document(&state, &ctx, &args).await?;
    let (stale, reason) = super::entities_file::stale_read(&state, is_active, dirty);
    let mut out = super::entities_file::read_envelope(
        &state,
        super::entities_file::source_json(&host, &document),
        stale,
        reason,
    );
    let picked = document.meshes().iter().filter(|mesh| match &within {
        Some((min, max)) => {
            let (bmin, bmax) = mesh.bounds();
            (0..3).all(|axis| bmin[axis] <= max[axis] && bmax[axis] >= min[axis])
        }
        None => true,
    });
    let mut total = 0usize;
    let mut meshes = Vec::new();
    for mesh in picked {
        total += 1;
        if meshes.len() < limit {
            meshes.push(mesh.summary_json());
        }
    }
    out.insert("count".into(), json!(total));
    out.insert("meshes".into(), Value::Array(meshes));
    out.insert("truncated".into(), json!(total > limit));
    Ok(Value::Object(out).into())
}

// -------------------------------------------------------------------------------------------
// get_mesh
// -------------------------------------------------------------------------------------------

/// One mesh's full geometry by `nodeID`: for the **active document** (an omitted `path`) the live
/// `mesh.get` read (`source:"live"`) when the bridge advertises it, else the SAVED `.vmap`
/// (`source:"file"`, `staleRead`): vertices (4 decimals), faces (vertex-index loops),
/// faceMaterials, per-face centre/normal (`faceInfo`) and bounds; not found is
/// `MeshNotFound {data.nodeID, data.path}` with the save_map hint (the POC wording).
pub async fn get_mesh(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let Some(node_id) = super::entities_file::optional_i64(&args, "nodeID")? else {
        return Err(ToolError::invalid_arguments("nodeID is required"));
    };
    // Only the active-document form (no `path`) can be read live.
    let mut too_large: Option<ToolError> = None;
    if args.get("path").filter(|path| !path.is_null()).is_none() {
        match live_mesh(&state, &ctx, node_id).await {
            Ok(Some(out)) => return Ok(Value::Object(out).into()),
            Ok(None) => {}
            // too large for one reply: the SAVED file answers while it is current (below)
            Err(error) if crate::tools::mesh_read::is_too_large(&error) => too_large = Some(error),
            Err(error) => return Err(error),
        }
    }
    let (host, document, is_active, dirty) =
        super::entities_file::resolve_document(&state, &ctx, &args).await?;
    let input = args.get("path").and_then(Value::as_str);
    let path_label = input
        .map(str::to_string)
        .unwrap_or_else(|| host.display().to_string());

    let Some(mesh) = document.mesh_by_id(node_id) else {
        return Err(ToolError::new(
            "MeshNotFound",
            "MeshNotFound",
            format!(
                "no mesh with nodeID {node_id} in the saved .vmap; save_map first if it was \
                 created since the last save"
            ),
        )
        .with_data(json!({"nodeID": node_id, "path": path_label}))
        .retry_safe(true));
    };

    let (stale, reason) = super::entities_file::stale_read(&state, is_active, dirty);
    // the live read refused this mesh: a stale file would be a different geometry than the
    // document's, so refuse with the counts and the save hint instead of serving it
    if stale {
        if let Some(error) = too_large.take() {
            return Err(error);
        }
    }
    let mut out = super::entities_file::read_envelope(
        &state,
        super::entities_file::source_json(&host, &document),
        stale,
        reason,
    );
    let geometry = mesh.to_json();
    let mut geometry = match geometry {
        Value::Object(map) => map,
        _ => unreachable!("to_json is an object"),
    };
    geometry.insert("faceInfo".into(), mesh.face_info_json());
    // bounds printed raw here (the POC's h_get_mesh does not round its bounds; only the summary does)
    let (min, max) = mesh.bounds();
    geometry.insert("bounds".into(), json!({"min": min, "max": max}));
    geometry.insert("source".into(), json!("file"));
    geometry.remove("nodeID");
    out.insert("nodeID".into(), json!(node_id));
    out.extend(geometry);

    // the POC's `_current_mesh` note for a dirty document (geometry from the SAVED file,
    // unsaved in-editor edits are not included). L4: the note is about the *active* document's
    // unsaved edits, so it is only added when this file IS the active document (`dirty` is the
    // active document's flag even when `path` names another file) and it says the edits are not
    // *included* — `get_mesh` replaces nothing (the POC's "are replaced" wording belongs to the
    // editing tools' `_current_mesh`).
    let mut notes: Vec<Value> = Vec::new();
    if dirty && is_active != Some(false) {
        notes.push(json!(
            "geometry taken from the SAVED file while the document has unsaved changes: unsaved \
             Hammer edits to this mesh (if any) are not included"
        ));
    }
    if too_large.is_some() {
        notes.push(json!(
            "the mesh is too large for one live mesh.get reply, so this is the SAVED file's \
             geometry (the document has no unsaved changes)"
        ));
    }
    if !notes.is_empty() {
        out.insert("notes".into(), Value::Array(notes));
    }
    Ok(Value::Object(out).into())
}

/// The ACTIVE document's mesh read live, or `None` when the file route has to answer: the bridge
/// cannot say (no active document / unreachable: the file route reports that the way it always has,
/// `NoDocument` naming `path`) or does not advertise `mesh.get`. A node the live document does not
/// have is `MeshNotFound` — the live truth, not a stale file's.
async fn live_mesh(
    state: &HammerState,
    ctx: &CallCtx,
    node_id: i64,
) -> Result<Option<Map<String, Value>>, ToolError> {
    let deadline = ctx.budget.deadline_ms()?;
    let Ok(Some(doc)) = read_active_document(state, Some(deadline)).await else {
        return Ok(None);
    };
    state.note_document(doc.get("id").and_then(Value::as_str));
    let Some(record) =
        crate::tools::mesh_read::read_live_mesh(state, deadline, &doc, node_id).await?
    else {
        return Ok(None);
    };
    Ok(Some(live_output(state, node_id, &record)))
}

/// The live route's result: the file route's shape (summary keys, vertices, faces, per-face
/// materials, `faceInfo`, raw `bounds`) with `source:"live"` and an always-fresh envelope.
fn live_output(state: &HammerState, node_id: i64, record: &MeshRecord) -> Map<String, Value> {
    let mut out = super::entities_file::read_envelope(state, json!("live"), false, None);
    let Value::Object(mut geometry) = record.to_json() else {
        unreachable!("to_json is an object")
    };
    geometry.insert("faceInfo".into(), record.face_info_json());
    let (min, max) = record.bounds();
    geometry.insert("bounds".into(), json!({"min": min, "max": max}));
    geometry.remove("nodeID");
    out.insert("nodeID".into(), json!(node_id));
    out.extend(geometry);
    // the vertices are the node-local geometry transformed by the node's OWN origin/angles/scales
    // only: under a container that is not proven to store its children in world space (an
    // instance, a prefab, anything unknown) they are parent-space, and the rebuild operators
    // refuse such a mesh — say so here instead of silently handing out unproven "world" numbers
    let world_space = record.parent_problem().is_none();
    out.insert("worldSpace".into(), json!(world_space));
    if !world_space {
        out.insert(
            "notes".into(),
            json!([format!(
                "the mesh sits under an ancestor chain that is not proven world-space ({}): the \
                 vertices apply only the node's own transform, so they may be parent-space; the \
                 rebuild operators refuse this mesh",
                record
                    .ancestors
                    .as_ref()
                    .map(|chain| chain.join(" <- "))
                    .unwrap_or_else(|| "unreadable".to_string())
            )]),
        );
    }
    out
}

// -------------------------------------------------------------------------------------------
// Argument helpers (limit/within shapes are the POC's, nodeID the shared route bound)
// -------------------------------------------------------------------------------------------

/// `limit?: integer` — [`LIMIT_DEFAULT`] when absent, 1..=[`LIMIT_MAX`] when given.
fn limit_arg(args: &Value) -> Result<usize, ToolError> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(LIMIT_DEFAULT as usize),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(limit) if (1..=LIMIT_MAX).contains(&limit) => Ok(limit as usize),
            _ => Err(ToolError::invalid_arguments(format!(
                "limit must be an integer in 1..={LIMIT_MAX}, got {n}"
            ))),
        },
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "limit must be an integer in 1..={LIMIT_MAX}, got {other}"
        ))),
    }
}

/// `within?: {min: [x,y,z], max: [x,y,z]}` (bounds-overlap box).
fn within_box(args: &Value) -> Result<Option<BoundsBox>, ToolError> {
    let Some(within) = args.get("within").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let invalid = |msg: String| ToolError::invalid_arguments(format!("within: {msg}"));
    let mut corners = [([0.0f64; 3]); 2];
    for (index, name) in ["min", "max"].iter().enumerate() {
        let value = within
            .get(*name)
            .filter(|value| !value.is_null())
            .ok_or_else(|| invalid("needs min and max".to_string()))?;
        let values = value
            .as_array()
            .filter(|values| values.len() == 3)
            .ok_or_else(|| invalid(format!("{name} must be [x, y, z]")))?;
        let mut out = [0.0f64; 3];
        for (axis, value) in values.iter().enumerate() {
            out[axis] = value
                .as_f64()
                .filter(|axis| axis.is_finite())
                .ok_or_else(|| {
                    invalid(format!("{name}[{axis}] must be a finite number, got {value}"))
                })?;
        }
        corners[index] = out;
    }
    Ok(Some((corners[0], corners[1])))
}
