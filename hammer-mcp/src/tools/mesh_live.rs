//! Live mesh mutators through the bridge: `create_mesh` (`mesh.create`), `create_block`
//! (`mesh.create` of a primitive), `set_mesh_geometry` (`mesh.setGeometry`), `set_mesh_material`
//! (`mesh.setMaterial`, or a `mesh.setGeometry` rebuild for the per-face form) and the four rebuild
//! operators `transform_mesh` / `move_mesh_vertices` / `extrude_mesh_face` / `delete_mesh_faces`
//! (`mesh.setGeometry` on the mesh's current geometry). Policies ported from the POC's
//! `h_create_mesh` / `h_create_block` / `h_set_mesh_geometry` / `h_set_mesh_material` /
//! `h_transform_mesh` / `h_move_mesh_vertices` / `h_extrude_mesh_face` / `h_delete_mesh_faces`
//! (`../idabins/worknotes/mcpserver/server.py`), with the geometry itself from
//! [`crate::geometry`] (the port of the POC's `geometry.py`).
//!
//! What every tool here shares with `entities_live`:
//!
//! * the active document is read first (`doc.active`) and its `{id, epoch}` fingerprint is sent as
//!   the mutating `expectDoc` (`-32004 DocumentChanged` when the document moved in between);
//! * every request carries an explicit `deadlineMs` derived from the per-call budget;
//! * a failure is mapped like `entities_live`'s mutators: the bridge's target conditions
//!   (`-32022 NodeNotFound`, `-32602 "node is not a mesh"`) are decided **before** anything is
//!   written and are retry-safe, while an outcome-unknown class or a protocol/internal error may
//!   have landed (a `mesh.setGeometry` clears the mesh before it rebuilds it) — those set
//!   `outcomeUnknown` and bump the unsaved-edit counter;
//! * a success bumps the unsaved-edit counter, never saves, and reports the bridge's own undo label
//!   (`"MCP: create mesh"`, `"MCP: set mesh geometry"`, `"MCP: set mesh material"` — the tool sends
//!   no label, the bridge decides it) plus the `undoNote` that says what `undo_last` restores;
//! * nothing is invented locally: the geometry the tools *send* is what they report (bounds are the
//!   sent vertices' bounds, rounded to 4 decimals exactly like the POC's `round(c, 4)`), and the
//!   bridge's `vertexSlots`/`faceSlots` (its live slot counts, i.e. **after** Hammer's welding) pass
//!   through when it reports them.
//!
//! `transform_mesh` / `move_mesh_vertices` / `extrude_mesh_face` / `delete_mesh_faces` (WP-C) are
//! the four POC rebuild operators (`h_transform_mesh` / `h_move_mesh_vertices` /
//! `h_extrude_mesh_face` / `h_delete_mesh_faces`): read the mesh's current geometry, apply the
//! operator from [`crate::geometry`], then one `mesh.setGeometry` through the very send/error path
//! [`set_mesh_geometry`] uses (see [`rebuild`]). The geometry comes from
//!
//! 1. the live document through the bridge's `mesh.get` ([`crate::tools::mesh_read`],
//!    [`GeometrySource::Live`], `geometrySource:"live"`) whenever the bridge advertises it — what
//!    Hammer holds now, so an undo or a UI edit since the last call is simply seen; no dirty
//!    gate. (The server-side geometry cache that stood in for this read until m3.12 is gone.) The
//!    reply's `parentChain` is checked like the file route's ancestors: `mesh.get`'s
//!    origin/angles are the node's own, so a mesh under an instance/prefab/unknown container is
//!    refused (`MeshGeometryUnavailable`, [`unproven_parent_refusal`]). A mesh too large for one
//!    reply (`MeshTooLarge`) takes route 2 while the document is clean
//!    ([`GeometrySource::FileAfterTooLarge`]) and is refused with its counts otherwise.
//! 2. otherwise (a bridge older than m3.12) the SAVED `.vmap` (`crate::vmap::mesh`,
//!    [`GeometrySource::File`]) — but only while the live document is clean: a dirty (or
//!    dirty-unknown, or MCP-edited) document is refused with `-32016 UnsavedChanges` and a hint to
//!    `save_map` first, because the rebuild would replace the mesh's in-editor geometry with the
//!    file's version. The file route also refuses a mesh whose ancestor chain is not a proven
//!    container (review-3 NEW-5, see [`unproven_parent_refusal`] and
//!    `crate::vmap::mesh::MeshRecord::parent_problem`); a node `scales` needs no refusal (the
//!    file's vertices are world-scaled and the rebuild resets the node's scales to identity);
//! 3. otherwise `MeshGeometryUnavailable` (the document is untitled: there is no file to read at
//!    all) or `MeshNotFound` (the file has no such mesh). The nodeIDs of the two halves are the
//!    same id space: the bridge's `mesh.setGeometry` resolves the *live* node id
//!    (`geo_target_mesh` -> `geo_find`) and the file route the file's `nodeID` attribute, and a
//!    save keeps node ids.
//!
//! Every operator's *result* geometry is validated locally before dispatch ([`check_result`]:
//! coordinates inside the bridge's range and no degenerate face — `move_mesh_vertices` is exactly
//! the operator that can flatten one) so a rebuild never reaches the bridge's `geo_reset_mesh`,
//! which clears the mesh before it rebuilds it.
//!
//! Deliberate refinements over the POC:
//!
//! * the request size is checked locally. The bridge's line buffer is 1 MiB (`MAX_LINE`) and an
//!   oversized line answers `-32600 "line too long"` and **closes the connection**, so a huge mesh
//!   would take every later tool call down with it. A payload over [`MAX_REQUEST_BYTES`] is refused
//!   before dispatch instead.
//! * `set_mesh_material {faces}` rebuilds the mesh from its current geometry (the live `mesh.get`
//!   read, else the saved file under the rebuild operators' rules), with those faces' materials
//!   replaced.
//! * on the file route the rebuild operators refuse a dirty/unknown document *before* they read
//!   anything (the POC reads the file and warns), and they check the operator's result instead of
//!   only the input.
use crate::geometry::{self, GeometryError, Mesh, Vec3};
use crate::state::HammerState;
use cs2_mcp_common::bridge::{BridgeError, Kind, RequestOpts};
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};

/// Review L7: the plumbing shared with `entities_live`/`lifecycle` (the document read, the
/// `expectDoc` fingerprint, the counter, the maybe-executed taxonomy, the argument shapes) has one
/// home.
pub(crate) use super::live_common::{
    bool_arg, doc_id_of, expect_doc, invalid, mutator_failure, node_id_arg, note_edit,
    require_document, MAX_NODE_ID,
};

/// The POC's default material (`DEFAULT_MATERIAL` in `server.py`); also the bridge's own example.
pub const DEFAULT_MATERIAL: &str = "materials/dev/reflectivity_30.vmat";
/// The bridge's index buffer (`GEO_MAX_IDX`) — the total number of face indices in one mesh.
const MAX_INDEX_COUNT: usize = 1 << 20;
/// The bridge's line buffer is `MAX_LINE` (1 MiB); keep the payload well under it so an oversized
/// mesh is a local refusal instead of `-32600 "line too long"` (which closes the connection).
pub(crate) const MAX_REQUEST_BYTES: usize = 512 * 1024;
/// The bridge's material buffer: `char mats[64][260]` / `defmat[260]` (the NUL included).
const MAX_MATERIAL_BYTES: usize = 259;

/// `_send_geometry`'s note: the bridge re-centres the node's pivot on the geometry's bounds and
/// initialises texture alignment in world space (Hammer's Polygon tool does the same).
const REBUILD_NOTE: &str =
    "pivot re-centred on the bounds; texture alignment re-initialised (world-aligned)";
/// Every mutator says it (this repo's convention, `entities_live::create_entities`).
const NO_SAVE_NOTE: &str = "no auto-save: run save_map to write the changes; file-backed reads \
                            (list_meshes, get_mesh with `path`) stay stale until then";
/// `undoNote` per method: what `undo_last` does with the group the bridge opened.
const UNDO_NOTE_CREATE: &str = "undo_last removes the mesh";
const UNDO_NOTE_GEOMETRY: &str = "undo_last restores the previous geometry";
const UNDO_NOTE_MATERIAL: &str = "undo_last restores the previous materials";
/// The all-faces `set_mesh_material` note (POC `h_set_mesh_material`).
const MATERIAL_ONLY_NOTE: &str = "texture alignment is kept (material only)";
/// `delete_mesh_faces`'s extra note (POC `h_delete_mesh_faces`): the rebuild renumbers the loops.
/// `get_mesh` without `path` reads the live document, so "call get_mesh again" is true: the answer
/// is the renumbered geometry of this result, not the saved file's pre-delete numbering.
const RENUMBERED_NOTE: &str = "face indices (and their vertex loops) are renumbered; call \
                               get_mesh again before more face edits - it answers this result's \
                               geometry (this result's vertexCount/faceCount are the rebuilt \
                               counts), not the saved file's pre-delete numbering";

pub fn register_create(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "create_mesh",
            "Create a mesh in the LIVE map from WORLD-space vertices and faces (vertex-index loops). orientOutward=true auto-fixes the winding of a closed convex solid. Undoable; not saved (save_map).",
            Mutating,
            &["mesh.create"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "vertices": {
                    "type": "array",
                    "minItems": 3,
                    "description": "WORLD-space [x, y, z] points",
                    "items": {
                        "type": "array",
                        "items": {"type": "number"},
                        "minItems": 3,
                        "maxItems": 3
                    }
                },
                "faces": {
                    "type": "array",
                    "minItems": 1,
                    "description": "vertex-index loops, counter-clockwise seen from the visible side",
                    "items": {
                        "type": "array",
                        "items": {"type": "integer", "minimum": 0},
                        "minItems": 3
                    }
                },
                "material": {
                    "type": "string",
                    "default": DEFAULT_MATERIAL
                },
                "faceMaterials": {
                    "type": "array",
                    "description": "material path or null (= base material) per face",
                    "items": {"type": ["string", "null"]}
                },
                "orientOutward": {
                    "type": "boolean",
                    "default": false,
                    "description": "flip faces pointing at the centroid (closed convex solid)"
                }
            },
            "required": ["vertices", "faces"],
            "additionalProperties": false
        })),
        create_mesh,
    );
}

pub fn register_block(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "create_block",
            "Create a box, wedge (ramp) or cylinder mesh in the LIVE map. box/wedge: min,max; wedge reaches full height at riseToward; cylinder: center (bottom centre), radius, height, sides. top/bottom/sideMaterial override material per face by normal z. Undoable; not saved.",
            Mutating,
            &["mesh.create"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "shape": {
                    "type": "string",
                    "enum": ["box", "wedge", "cylinder"],
                    "default": "box"
                },
                "min": {
                    "type": "array",
                    "items": {"type": "number"},
                    "minItems": 3,
                    "maxItems": 3,
                    "description": "box/wedge: low corner"
                },
                "max": {
                    "type": "array",
                    "items": {"type": "number"},
                    "minItems": 3,
                    "maxItems": 3,
                    "description": "box/wedge: high corner"
                },
                "riseToward": {
                    "type": "string",
                    "enum": ["+x", "-x", "+y", "-y"],
                    "default": "+x",
                    "description": "end that reaches max.z"
                },
                "center": {
                    "type": "array",
                    "items": {"type": "number"},
                    "minItems": 3,
                    "maxItems": 3,
                    "description": "bottom centre"
                },
                "radius": {"type": "number", "exclusiveMinimum": 0},
                "height": {"type": "number", "exclusiveMinimum": 0},
                "sides": {"type": "integer", "minimum": 3, "maximum": 64, "default": 16},
                "material": {"type": "string", "default": DEFAULT_MATERIAL},
                "topMaterial": {
                    "type": "string",
                    "description": "faces with normal z > 0.5"
                },
                "bottomMaterial": {
                    "type": "string",
                    "description": "faces with normal z < -0.5"
                },
                "sideMaterial": {
                    "type": "string",
                    "description": "vertical faces"
                }
            },
            "additionalProperties": false
        })),
        create_block,
    );
}

pub fn register_geometry(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "set_mesh_geometry",
            "Replace ALL polygons of a LIVE mesh (nodeID) with new WORLD-space vertices and faces. The node keeps its id. Undoable; not saved.",
            Mutating,
            &["mesh.setGeometry"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "nodeID": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_NODE_ID
                },
                "vertices": {
                    "type": "array",
                    "minItems": 3,
                    "description": "WORLD-space [x, y, z] points",
                    "items": {
                        "type": "array",
                        "items": {"type": "number"},
                        "minItems": 3,
                        "maxItems": 3
                    }
                },
                "faces": {
                    "type": "array",
                    "minItems": 1,
                    "description": "vertex-index loops, counter-clockwise seen from the visible side",
                    "items": {
                        "type": "array",
                        "items": {"type": "integer", "minimum": 0},
                        "minItems": 3
                    }
                },
                "material": {"type": "string", "default": DEFAULT_MATERIAL},
                "faceMaterials": {
                    "type": "array",
                    "description": "material path or null (= base material) per face",
                    "items": {"type": ["string", "null"]}
                }
            },
            "required": ["nodeID", "vertices", "faces"],
            "additionalProperties": false
        })),
        set_mesh_geometry,
    );
}

pub fn register_material(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "set_mesh_material",
            "Assign a material to a LIVE mesh's faces (all faces if `faces` omitted). With `faces` the mesh is rebuilt from geometry this server wrote this session (live geometry can't be read), so use it on meshes created or rewritten here. Undoable; not saved.",
            Mutating,
            &["mesh.setMaterial", "mesh.setGeometry"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "nodeID": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_NODE_ID
                },
                "material": {
                    "type": "string"
                },
                "faces": {
                    "type": "array",
                    "minItems": 1,
                    "uniqueItems": true,
                    "items": {"type": "integer", "minimum": 0},
                    "description": "face indices from get_mesh; omit for all"
                }
            },
            "required": ["nodeID", "material"],
            "additionalProperties": false
        })),
        set_mesh_material,
    );
}

// -------------------------------------------------------------------------------------------
// The four rebuild operators (WP-C): registrations
// -------------------------------------------------------------------------------------------

/// The `nodeID` schema of the four operators (the WP-B tools inline the same shape).
fn node_id_schema() -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "maximum": MAX_NODE_ID
    })
}

/// A `[x, y, z]` argument schema.
fn vec3_schema(description: &str) -> Value {
    json!({
        "type": "array",
        "items": {"type": "number"},
        "minItems": 3,
        "maxItems": 3,
        "description": description
    })
}

pub fn register_transform(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "transform_mesh",
            "Translate, yaw-rotate (degrees about the pivot) and/or scale a LIVE mesh (nodeID). Works from the geometry this server wrote this session, else the SAVED .vmap (refused while the live document is dirty: save_map first); `geometrySource` says which. An odd number of negative scale axes mirrors the mesh. Undoable; not saved.",
            Mutating,
            &["mesh.setGeometry"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "nodeID": node_id_schema(),
                "translate": vec3_schema("world-space [dx, dy, dz]"),
                "rotateYaw": {
                    "type": "number",
                    "description": "degrees, counter-clockwise seen from above"
                },
                "scale": {
                    "oneOf": [
                        {"type": "number"},
                        {"type": "array", "items": {"type": "number"},
                         "minItems": 3, "maxItems": 3}
                    ],
                    "description": "uniform factor or [sx, sy, sz]; zero refused"
                },
                "pivot": vec3_schema("scale/rotate centre; default bounds centre")
            },
            "required": ["nodeID"],
            "additionalProperties": false
        })),
        transform_mesh,
    );
}

pub fn register_move_vertices(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "move_mesh_vertices",
            "Offset vertices (indices from get_mesh) of a LIVE mesh by [dx, dy, dz], e.g. to make a slope. Faces must stay planar; a move that flattens a face is refused. Geometry source as transform_mesh (own writes, else saved .vmap; dirty document refused). Undoable; not saved.",
            Mutating,
            &["mesh.setGeometry"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "nodeID": node_id_schema(),
                "vertices": {
                    "type": "array",
                    "minItems": 1,
                    "items": {"type": "integer", "minimum": 0}
                },
                "offset": vec3_schema("world-space [dx, dy, dz]")
            },
            "required": ["nodeID", "vertices", "offset"],
            "additionalProperties": false
        })),
        move_mesh_vertices,
    );
}

pub fn register_extrude(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "extrude_mesh_face",
            "Push one face (index from get_mesh) of a LIVE mesh out along its normal by `distance` (negative = recess), adding side quads. Geometry source as transform_mesh. Undoable; not saved.",
            Mutating,
            &["mesh.setGeometry"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "nodeID": node_id_schema(),
                "face": {
                    "type": "integer", "minimum": 0
                },
                "distance": {
                    "type": "number",
                    "description": "negative = inward; zero refused"
                },
                "sideMaterial": {
                    "type": "string",
                    "description": "default: the cap's"
                }
            },
            "required": ["nodeID", "face", "distance"],
            "additionalProperties": false
        })),
        extrude_mesh_face,
    );
}

pub fn register_delete_faces(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "delete_mesh_faces",
            "Remove faces (indices from get_mesh) of a LIVE mesh; remaining indices are renumbered, so call get_mesh again. Not for: removing the whole mesh (delete_nodes). Geometry source as transform_mesh. Undoable; not saved.",
            Mutating,
            &["mesh.setGeometry"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "nodeID": node_id_schema(),
                "faces": {
                    "type": "array",
                    "minItems": 1,
                    "uniqueItems": true,
                    "items": {"type": "integer", "minimum": 0}
                }
            },
            "required": ["nodeID", "faces"],
            "additionalProperties": false
        })),
        delete_mesh_faces,
    );
}

// -------------------------------------------------------------------------------------------
// Shared plumbing (`live_common` holds what every live mutator shares, review L7; what is left
// here is this file's own: materials, geometry gates, the request payload)
// -------------------------------------------------------------------------------------------

/// A [`GeometryError`] is an argument problem: the POC raised `GeometryError` as `ValueError`,
/// which the tool layer turns into `InvalidArguments` with nothing sent.
fn geometry_invalid(error: GeometryError) -> ToolError {
    invalid(error.0)
}

/// A `mesh.setGeometry`/`mesh.setMaterial` failure on the **target** node in this file's vocabulary.
/// The bridge's `geo_target_mesh` runs before the undo group opens, so nothing was written:
/// `-32022 NodeNotFound` -> `MeshNotFound`, `-32602 {reason:"node is not a mesh"}` -> `NotAMesh`,
/// both retry-safe. Everything else keeps the wire mapping (a raw wire error can never carry these
/// tool-level names).
pub(crate) fn target_failure(error: &BridgeError, node_id: i64) -> Option<ToolError> {
    let reason = error
        .data
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match error.code {
        Some(-32022) => Some(
            ToolError::new(
                "MeshNotFound",
                "MeshNotFound",
                format!("nodeID {node_id} is not in the live document"),
            )
            .with_code(-32022)
            .with_data(json!({"nodeID": node_id, "reason": reason}))
            .retry_safe(true),
        ),
        Some(-32602) if reason.contains("not a mesh") => Some(
            ToolError::new(
                "NotAMesh",
                "NotAMesh",
                format!(
                    "nodeID {node_id} is not a mesh (a world, entity, group, prefab or instance \
                     node): only CMapMesh nodes take geometry"
                ),
            )
            .with_code(-32602)
            .with_data(json!({"nodeID": node_id, "reason": reason}))
            .retry_safe(true),
        ),
        _ => None,
    }
}

/// One material path: non-empty, at most [`MAX_MATERIAL_BYTES`] bytes, no control characters (the
/// bridge's `jstr` refuses a `\n` escape and its buffer is 260 bytes; a `"` or `\` is escaped by
/// serde_json into a form `jstr` accepts).
pub(crate) fn material_text(label: &str, text: &str, default: &str) -> Result<String, ToolError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(invalid(format!(
            "{label} must be a material path (e.g. {default}), got an empty string"
        )));
    }
    if text.chars().any(char::is_control) {
        return Err(invalid(format!(
            "{label}: a material path cannot contain control characters ({text:?})"
        )));
    }
    if text.len() > MAX_MATERIAL_BYTES {
        return Err(invalid(format!(
            "{label}: the material path is {} bytes, the bridge's buffer holds \
             {MAX_MATERIAL_BYTES}",
            text.len()
        )));
    }
    Ok(text.to_string())
}

/// `material` (POC `_material_arg`): a non-empty string, default [`DEFAULT_MATERIAL`].
fn material_arg(args: &Value, key: &str) -> Result<String, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(DEFAULT_MATERIAL.to_string()),
        Some(Value::String(text)) => material_text(key, text, DEFAULT_MATERIAL),
        Some(other) => Err(invalid(format!(
            "{key} must be a material path (e.g. {DEFAULT_MATERIAL}), got {other}"
        ))),
    }
}

/// `material` where the tool's schema marks it **required** (`set_mesh_material`). The registry
/// does not enforce `required`/`additionalProperties` (review L5), so the handler does: without it
/// a missing key silently painted every face with [`DEFAULT_MATERIAL`].
fn required_material_arg(args: &Value) -> Result<String, ToolError> {
    match args.get("material") {
        None | Some(Value::Null) => Err(invalid(
            "material is required: the material path to assign (e.g. \
             materials/tools/toolsnodraw.vmat)",
        )),
        Some(Value::String(text)) => material_text("material", text, DEFAULT_MATERIAL),
        Some(other) => Err(invalid(format!(
            "material must be a material path (e.g. {DEFAULT_MATERIAL}), got {other}"
        ))),
    }
}

/// Refuse an argument the tool's schema does not list (review L5: the registry does not enforce
/// `additionalProperties: false`, so a typo like `"face": [1]` for `faces` used to take the
/// all-faces route silently — and that route writes a different edit than the caller asked for).
fn reject_unknown_keys(args: &Value, allowed: &[&str]) -> Result<(), ToolError> {
    let Some(object) = args.as_object() else {
        return Err(invalid(format!(
            "the arguments must be an object, got {args}"
        )));
    };
    if let Some(unknown) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid(format!(
            "unknown argument {unknown:?}: this tool takes {}",
            allowed
                .iter()
                .map(|key| format!("{key:?}"))
                .collect::<Vec<String>>()
                .join(", ")
        )));
    }
    Ok(())
}

/// An optional per-side material (`topMaterial`/`bottomMaterial`/`sideMaterial`): `None` when
/// absent, null or empty (the POC treats an empty string as "not given"), otherwise a validated
/// path.
fn optional_material_arg(args: &Value, key: &str) -> Result<Option<String>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => material_text(key, text, DEFAULT_MATERIAL).map(Some),
        Some(other) => Err(invalid(format!(
            "{key} must be a material path (e.g. {DEFAULT_MATERIAL}), got {other}"
        ))),
    }
}

/// `[x, y, z]` of finite numbers strictly inside the bridge's coordinate range. Absent = `None`
/// (the caller decides whether that is an error).
fn vec3_arg(args: &Value, key: &str) -> Result<Option<Vec3>, ToolError> {
    let Some(value) = args.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(axes) = value.as_array().filter(|axes| axes.len() == 3) else {
        return Err(invalid(format!(
            "{key} must be [x, y, z] (three numbers), got {value}"
        )));
    };
    let mut out: Vec3 = [0.0; 3];
    for (axis, value) in axes.iter().enumerate() {
        let number = value.as_f64().filter(|n| n.is_finite()).ok_or_else(|| {
            invalid(format!("{key}[{axis}] must be a finite number, got {value}"))
        })?;
        if number.abs() >= geometry::MAX_COORD {
            return Err(invalid(format!(
                "{key}[{axis}] is {number}, outside +-{} (the bridge refuses it)",
                geometry::MAX_COORD
            )));
        }
        out[axis] = number;
    }
    Ok(Some(out))
}

/// The POC's `round(c, 4)`: the bridge reads plain decimals, and 4 decimals are well past what
/// Hammer keeps. The rounding itself is [`crate::vmap::mesh::round4`] (one copy, Python-faithful:
/// review N4); `-0.0` becomes `0.0` (both are exact for the bridge; one spelling is enough).
fn round4(point: Vec3) -> Vec3 {
    point.map(|c| {
        let rounded = crate::vmap::mesh::round4(c);
        if rounded == 0.0 {
            0.0
        } else {
            rounded
        }
    })
}

/// Every face must have a non-degenerate normal *before* Hammer sees it (POC: `for loop in f:
/// geo.face_normal(v, loop)`); Hammer would create nothing for a zero-area polygon and the bridge
/// reports the whole call as an internal error.
fn check_face_normals(vertices: &[Vec3], faces: &[Vec<usize>]) -> Result<(), ToolError> {
    for (index, face) in faces.iter().enumerate() {
        geometry::face_normal(vertices, face).map_err(|error| {
            invalid(format!(
                "faces[{index}]: {error} (Hammer would create nothing for a zero-area face)"
            ))
        })?;
    }
    Ok(())
}

/// Every coordinate the bridge will read must be inside its range (the primitives build their own
/// vertices, so they are checked here rather than by [`geometry::validate`]).
fn check_coords(vertices: &[Vec3]) -> Result<(), ToolError> {
    for (index, point) in vertices.iter().enumerate() {
        if point.iter().any(|c| c.abs() >= geometry::MAX_COORD) {
            return Err(invalid(format!(
                "vertices[{index}] is outside +-{} (the bridge refuses it)",
                geometry::MAX_COORD
            )));
        }
    }
    Ok(())
}

/// One geometry payload, exactly what `_send_geometry` serialises. `pub(crate)` since WP-V
/// (`trigger_volume.rs` builds one for its `mesh.setGeometry`); visibility only.
pub(crate) struct Payload {
    /// The world vertices to send (`send_geometry` rounds them itself).
    pub(crate) vertices: Vec<Vec3>,
    /// The face loops (vertex-index lists), as sent.
    pub(crate) faces: Vec<Vec<usize>>,
    /// `None` = one material for every face (`faceMaterials` is omitted entirely), otherwise one
    /// entry per face where `None` means "the caller's `material` argument".
    pub(crate) face_materials: Option<Vec<Option<String>>>,
}

/// The f64 -> f32 -> f64 round trip the bridge applies to every coordinate it parses
/// (`geom_parse`: `(float)c[k]`) before Hammer reads the mesh. It is the last lossy step on the way
/// (review L2).
fn as_float(point: Vec3) -> Vec3 {
    point.map(|c| c as f32 as f64)
}

/// The Newell vector length of a face = twice its area (the POC's `face_normal` normalises exactly
/// this vector). `None` for a face with an index outside the vertex list.
fn newell_length(vertices: &[Vec3], face: &[usize]) -> Option<f64> {
    let mut n = [0.0f64; 3];
    for i in 0..face.len() {
        let a = vertices.get(face[i])?;
        let b = vertices.get(face[(i + 1) % face.len()])?;
        n[0] += (a[1] - b[1]) * (a[2] + b[2]);
        n[1] += (a[2] - b[2]) * (a[0] + b[0]);
        n[2] += (a[0] - b[0]) * (a[1] + b[1]);
    }
    Some((n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt())
}

/// The f32 step (2^-24) at a coordinate: the smallest representable difference there.
const F32_EPSILON: f64 = 1.0 / (1u64 << 24) as f64;

/// Every face must still be a real polygon on the numbers the bridge reads (review L2): the sent
/// vertices are rounded to 4 decimals (`round4`, the POC's `round(c, 4)`) and the bridge widens
/// them to `float` for Hammer, so a sliver face that only exists in f64 collapses on the way and
/// Hammer creates *fewer* polygons than we sent (the case M1 reports). Refusing it here keeps the
/// tool's own report honest and saves a rebuild that would lose the mesh.
fn check_face_areas(vertices: &[Vec3], faces: &[Vec<usize>]) -> Result<(), ToolError> {
    for (index, face) in faces.iter().enumerate() {
        let Some(length) = newell_length(vertices, face) else {
            continue; // a bad index is `check_face_normals`'s (or the wire's) refusal
        };
        // the f32 step at the face's largest coordinate: a face whose twice-area is below
        // step^2 is not representable above the float noise floor there (a 1e-5-wide sliver, or a
        // small face around |x| ~ 5e5 where the step is ~0.03)
        let magnitude = face
            .iter()
            .filter_map(|vertex| vertices.get(*vertex))
            .flat_map(|point| point.iter())
            .fold(0.0f64, |max, c| max.max(c.abs()));
        let step = magnitude * F32_EPSILON;
        if length < step * step {
            return Err(invalid(format!(
                "faces[{index}]: the face is {length:.3e} wide in the bridge's units (twice its \
                 area), below the {:.3e} the bridge's float coordinates can carry at |x| ~ \
                 {magnitude} — Hammer would drop it and create fewer polygons than sent. Give it \
                 real area or split it.",
                step * step
            )));
        }
    }
    Ok(())
}

/// [`check_face_normals`] on the f32 round trip the bridge puts every coordinate through
/// ([`as_float`]): what is a valid face in f64 and a line in float is a face Hammer never creates
/// (review L2).
fn check_float_faces(vertices: &[Vec3], faces: &[Vec<usize>]) -> Result<(), ToolError> {
    let floats: Vec<Vec3> = vertices.iter().copied().map(as_float).collect();
    for (index, face) in faces.iter().enumerate() {
        geometry::face_normal(&floats, face).map_err(|error| {
            invalid(format!(
                "faces[{index}]: {error} once the bridge widens the coordinates to float (the \
                 bridge sends Hammer float coordinates, so Hammer would create nothing for it)"
            ))
        })?;
    }
    Ok(())
}

/// What a validated geometry payload becomes on the wire: the rounded vertices, the request
/// parameters (size-checked), the face count sent and whether per-face materials ride along.
pub(crate) struct Wire {
    pub(crate) vertices: Vec<Vec3>,
    pub(crate) params: Map<String, Value>,
    pub(crate) sent_faces: usize,
    pub(crate) per_face: bool,
}

/// Every local gate `send_geometry` runs before it touches the bridge (index budget, coordinate
/// range on the rounded vertices, face normals/areas, the request-size budget), as a function of
/// its own so `apply_layout` validates a whole layout without sending anything.
pub(crate) fn prepare_wire(
    payload: &Payload,
    base_material: &str,
    node_id: Option<i64>,
) -> Result<Wire, ToolError> {
    let vertices: Vec<Vec3> = payload.vertices.iter().copied().map(round4).collect();
    let index_count: usize = payload.faces.iter().map(Vec::len).sum();
    if index_count > MAX_INDEX_COUNT {
        return Err(invalid(format!(
            "the mesh has {index_count} face indices, over the bridge's index buffer \
             ({MAX_INDEX_COUNT})"
        )));
    }
    // L3: the coordinate gate runs on the *rounded* vertices, so a value the rounding pushes onto
    // the bridge's +-1e6 limit (`999999.99996` -> `1000000.0`) is refused with our own wording
    // instead of coming back as the bridge's `-32602 "each must be [x,y,z] with |coord| < 1e6"`
    // (which blames the input the caller sent).
    check_coords(&vertices)?;
    // L2: faces are checked on exactly the numbers the bridge reads (the rounded f64s and their
    // f32 widening), not on the raw input the caller sent.
    check_face_normals(&vertices, &payload.faces)?;
    check_face_areas(&vertices, &payload.faces)?;
    check_float_faces(&vertices, &payload.faces)?;

    let sent_faces = payload.faces.len();
    // H2: `base_material` is the CALLER's material; `wire_base` is the most common one
    // `material_params` picked for the wire (they differ whenever the overrides outnumber it)
    let (wire_base, overrides) = geometry::material_params(payload.face_materials.as_deref(), base_material);
    let per_face = overrides.is_some();
    let mut params = Map::new();
    params.insert("vertices".into(), json!(vertices));
    params.insert("faces".into(), json!(payload.faces));
    params.insert("material".into(), json!(wire_base));
    if let Some(overrides) = overrides {
        params.insert("faceMaterials".into(), json!(overrides));
    }
    if let Some(node_id) = node_id {
        params.insert("nodeID".into(), json!(node_id));
    }
    let bytes = Value::Object(params.clone()).to_string().len();
    if bytes > MAX_REQUEST_BYTES {
        return Err(invalid(format!(
            "the mesh serialises to {bytes} bytes of parameters, over this server's \
             {MAX_REQUEST_BYTES}-byte budget: the bridge's line buffer is 1 MiB and a longer line \
             answers -32600 \"line too long\" and closes the connection. Split the mesh into \
             smaller ones."
        )));
    }
    Ok(Wire {
        vertices,
        params,
        sent_faces,
        per_face,
    })
}

/// `_send_geometry`: one `mesh.create`/`mesh.setGeometry` request. Everything local (rounding,
/// material split, size) happens first, then the document + `expectDoc` + the request, then the
/// result the POC's `_send_geometry` builds.
///
/// Review-2 L2: a caller that already read the active **document** for its own gating (the rebuild
/// operators' `current_mesh`, the per-face `set_mesh_material`, and WP-V's `set_trigger_volume`)
/// passes it here — the write then carries *that* read's fingerprint and makes no second
/// `doc.active` of its own, so the large `.vmap` parse between the two reads of the other shape
/// cannot land doc A's geometry in doc B (a switched-away document is refused by the bridge's own
/// `expectDoc` gate, pre-write).
///
/// `pub(crate)` since WP-V: `trigger_volume.rs` sends its volume through this exact path
/// (`Result = set_mesh_geometry's`), with visibility only — no behaviour change.
#[allow(clippy::too_many_arguments)] // the pipeline's own shape (method/node/payload/notes); the args are one Value
pub(crate) async fn send_geometry(
    state: &HammerState,
    ctx: &CallCtx,
    method: &str,
    node_id: Option<i64>,
    payload: Payload,
    base_material: &str,
    undo_note: &str,
    read_doc: Option<Value>,
) -> Result<Map<String, Value>, ToolError> {
    let Wire {
        vertices,
        params,
        sent_faces,
        per_face,
    } = prepare_wire(&payload, base_material, node_id)?;

    let deadline = ctx.budget.deadline_ms()?;
    let doc = match read_doc {
        Some(doc) => doc,
        None => require_document(state, deadline).await?,
    };
    let doc_id = doc_id_of(&doc);
    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                method,
                Value::Object(params),
                RequestOpts {
                    deadline_ms: Some(deadline),
                    expect_doc: Some(expect_doc(&doc)),
                    gate: None,
                },
            )
            .await
    };
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            // the target is resolved before the undo group opens: a missing or wrong-class node
            // exits without touching the document
            if let Some(node_id) = node_id {
                if let Some(tool) = target_failure(&error, node_id) {
                    return Err(tool);
                }
            }
            // H1 (review H1 fix 4): the write may have landed after all (an outcome-unknown class,
            // or a `-32603` the bridge can only answer after `geo_reset_mesh` cleared the mesh).
            return Err(mutator_failure(error, state, doc_id.as_deref()));
        }
    };
    note_edit(state, doc_id.as_deref());

    let reported = reply
        .result
        .get("nodeID")
        .and_then(Value::as_i64)
        .or(node_id);
    let created_faces = reply.result.get("faces").and_then(Value::as_i64);
    // a create reply with no usable node id (the live `geo_mesh_summary` reports -1 when its own
    // lookup failed): the mesh exists, but the follow-up tools need its id
    let missing_id = node_id.is_none() && !matches!(reported, Some(id) if id >= 0);

    let (mn, mx) = geometry::bounds(&vertices);
    let mut out = Map::new();
    out.insert(
        "nodeID".into(),
        reported.map(|id| json!(id)).unwrap_or(Value::Null),
    );
    out.insert(
        "origin".into(),
        reply.result.get("origin").cloned().unwrap_or(Value::Null),
    );
    out.insert("vertexCount".into(), json!(vertices.len()));
    out.insert(
        "faceCount".into(),
        reply
            .result
            .get("faces")
            .cloned()
            .unwrap_or_else(|| json!(sent_faces)),
    );
    for key in ["vertexSlots", "faceSlots"] {
        if let Some(value) = reply.result.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    out.insert("bounds".into(), json!({"min": mn, "max": mx}));
    out.insert(
        "undoLabel".into(),
        reply
            .result
            .get("undoLabel")
            .cloned()
            .unwrap_or(Value::Null),
    );
    out.insert("undoNote".into(), json!(undo_note));
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("saved".into(), json!(false));
    out.insert("notes".into(), json!([NO_SAVE_NOTE, REBUILD_NOTE]));

    // M1: the bridge applies `faceMaterials` only when Hammer's face count equals the one sent
    // (`if (g->nmat && out.size == g->nf)` in `geo_build`); on a mismatch it built a geometry that
    // is not the one we sent (Hammer welded or dropped polygons), so the per-face overrides may
    // have been skipped entirely and the face indices no longer line up. Report it.
    if missing_id {
        push_note(
            &mut out,
            "the bridge reported no usable nodeID for the mesh it built: it exists in the \
             document, but find its id (find_entities / the bridge's node.list tree walk, or list_meshes \
             after save_map) before the per-face set_mesh_material route or \
             the rebuild operators",
        );
    }
    let mismatch = matches!(created_faces, Some(created) if created >= 0 && created as usize != sent_faces);
    if mismatch {
        let created = created_faces.expect("mismatch is only set with a count");
        out.insert("faceMaterialsApplied".into(), json!(false));
        push_note(
            &mut out,
            &format!(
                "Hammer created {created} of {sent_faces} face(s): the geometry differs from what \
                 was sent (welding, or a polygon degenerate once the coordinates were widened to \
                 float), so {} — read the mesh back with get_mesh before more face edits",
                if per_face {
                    "the per-face materials were NOT applied and every face uses the base material"
                } else {
                    "every face uses the material that was sent"
                }
            ),
        );
    }
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// create_mesh
// -------------------------------------------------------------------------------------------

/// `create_mesh {vertices, faces, material?, faceMaterials?, orientOutward?}`: validate locally
/// (nothing is sent for a bad mesh, an out-of-range coordinate, a repeated index or a zero-area
/// face), optionally fix the winding, then one `mesh.create`.
pub async fn create_mesh(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    let (payload, material, flipped) = prepare_create_mesh(&args)?;

    let mut out = send_geometry(
        &state,
        &ctx,
        "mesh.create",
        None,
        payload,
        &material,
        UNDO_NOTE_CREATE,
        None,
    )
    .await?;
    if flipped > 0 {
        push_note(&mut out, &format!("orientOutward flipped {flipped} face(s)"));
    }
    Ok(Value::Object(out).into())
}

/// `create_mesh`'s argument validation, with nothing sent: the geometry payload, the base material
/// and how many faces `orientOutward` flipped. `apply_layout` validates its `kind:"mesh"` items
/// through this exact function.
pub(crate) fn prepare_create_mesh(args: &Value) -> Result<(Payload, String, usize), ToolError> {
    reject_unknown_keys(
        args,
        &["vertices", "faces", "material", "faceMaterials", "orientOutward"],
    )?;
    let material = material_arg(args, "material")?;
    let mesh = geometry::validate(
        args.get("vertices"),
        args.get("faces"),
        args.get("faceMaterials").filter(|value| !value.is_null()),
        Some(&material),
    )
    .map_err(geometry_invalid)?;
    check_face_material_texts(&mesh)?;

    let mut faces = mesh.faces.clone();
    let mut flipped = 0usize;
    if bool_arg(args, "orientOutward")?.unwrap_or(false) {
        let (fix, count) =
            geometry::orient_outward(&mesh.vertices, &faces).map_err(geometry_invalid)?;
        faces = fix;
        flipped = count;
    }
    check_face_normals(&mesh.vertices, &faces)?;
    Ok((
        Payload {
            vertices: mesh.vertices,
            faces,
            face_materials: mesh.face_materials,
        },
        material,
        flipped,
    ))
}

/// Every `faceMaterials` path goes through the same bound check as `material`.
fn check_face_material_texts(mesh: &Mesh) -> Result<(), ToolError> {
    for (index, material) in mesh.face_materials.iter().flatten().enumerate() {
        if let Some(material) = material {
            material_text(&format!("faceMaterials[{index}]"), material, DEFAULT_MATERIAL)?;
        }
    }
    Ok(())
}

/// Append one note to a result's `notes` array.
fn push_note(out: &mut Map<String, Value>, note: &str) {
    let notes = out
        .entry("notes".to_string())
        .or_insert_with(|| json!([]));
    if let Some(notes) = notes.as_array_mut() {
        notes.push(json!(note));
    }
}

// -------------------------------------------------------------------------------------------
// create_block
// -------------------------------------------------------------------------------------------

/// `create_block {shape, min?, max?, riseToward?, center?, radius?, height?, sides?, material?,
/// topMaterial?, bottomMaterial?, sideMaterial?}`: build a box/wedge/cylinder, pick each face's
/// material from its normal (top/bottom/side), then one `mesh.create`.
pub async fn create_block(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    let (payload, material, shape, names) = prepare_create_block(&args)?;
    let mut out = send_geometry(
        &state,
        &ctx,
        "mesh.create",
        None,
        payload,
        &material,
        UNDO_NOTE_CREATE,
        None,
    )
    .await?;
    out.insert("shape".into(), json!(shape));
    out.insert("faceNames".into(), json!(names));
    Ok(Value::Object(out).into())
}

/// `create_block`'s argument validation, with nothing sent: the geometry payload, the base
/// material, the shape and the face names. `apply_layout` validates its `kind:"block"` items
/// through this exact function.
pub(crate) fn prepare_create_block(
    args: &Value,
) -> Result<(Payload, String, &'static str, Vec<String>), ToolError> {
    reject_unknown_keys(
        args,
        &[
            "shape",
            "min",
            "max",
            "riseToward",
            "center",
            "radius",
            "height",
            "sides",
            "material",
            "topMaterial",
            "bottomMaterial",
            "sideMaterial",
        ],
    )?;
    let shape = match args.get("shape") {
        None | Some(Value::Null) => "box",
        Some(Value::String(shape)) => match shape.as_str() {
            "box" => "box",
            "wedge" => "wedge",
            "cylinder" => "cylinder",
            other => {
                return Err(invalid(format!(
                    "shape must be box, wedge or cylinder, got \"{other}\""
                )))
            }
        },
        Some(other) => {
            return Err(invalid(format!(
                "shape must be box, wedge or cylinder, got {other}"
            )))
        }
    };
    let material = material_arg(args, "material")?;
    let (vertices, faces, names) = match shape {
        "cylinder" => {
            let Some(center) = vec3_arg(args, "center")? else {
                return Err(invalid(
                    "cylinder needs center (bottom centre), radius and height",
                ));
            };
            let number = |key: &str| -> Result<f64, ToolError> {
                args.get(key)
                    .and_then(Value::as_f64)
                    .filter(|n| n.is_finite())
                    .ok_or_else(|| {
                        invalid(format!(
                            "cylinder needs center (bottom centre), radius and height ({key} must \
                             be a finite number)"
                        ))
                    })
            };
            let radius = number("radius")?;
            let height = number("height")?;
            let sides = match args.get("sides") {
                None | Some(Value::Null) => 16usize,
                Some(Value::Number(number)) => number.as_u64().map(|n| n as usize).ok_or_else(
                    || invalid(format!("sides must be an integer in 3..=64, got {number}")),
                )?,
                Some(other) => {
                    return Err(invalid(format!(
                        "sides must be an integer in 3..=64, got {other}"
                    )))
                }
            };
            let primitive = geometry::cylinder(center, radius, height, sides).map_err(geometry_invalid)?;
            (
                primitive.mesh.vertices,
                primitive.mesh.faces,
                primitive.names,
            )
        }
        other => {
            let Some(mins) = vec3_arg(args, "min")? else {
                return Err(invalid(format!("{other} needs min and max")));
            };
            let Some(maxs) = vec3_arg(args, "max")? else {
                return Err(invalid(format!("{other} needs min and max")));
            };
            let primitive = if other == "box" {
                geometry::r#box(mins, maxs)
            } else {
                let rise = match args.get("riseToward") {
                    None | Some(Value::Null) => "+x",
                    Some(Value::String(rise)) => rise.as_str(),
                    Some(other) => {
                        return Err(invalid(format!(
                            "riseToward must be +x, -x, +y or -y, got {other}"
                        )))
                    }
                };
                geometry::wedge(mins, maxs, rise)
            }
            .map_err(geometry_invalid)?;
            (
                primitive.mesh.vertices,
                primitive.mesh.faces,
                primitive.names,
            )
        }
    };
    check_coords(&vertices)?;
    check_face_normals(&vertices, &faces)?;

    // per-face material by the normal's z: top (> 0.5), bottom (< -0.5), side (|z| <= 0.5)
    let top = optional_material_arg(args, "topMaterial")?;
    let bottom = optional_material_arg(args, "bottomMaterial")?;
    let side = optional_material_arg(args, "sideMaterial")?;
    let mut face_materials: Vec<Option<String>> = Vec::with_capacity(faces.len());
    for face in &faces {
        let normal = geometry::face_normal(&vertices, face).map_err(geometry_invalid)?;
        let chosen = if normal[2] > 0.5 {
            top.clone()
        } else if normal[2] < -0.5 {
            bottom.clone()
        } else {
            side.clone()
        };
        face_materials.push(chosen);
    }

    Ok((
        Payload {
            vertices,
            faces,
            face_materials: Some(face_materials),
        },
        material,
        shape,
        names,
    ))
}

// -------------------------------------------------------------------------------------------
// set_mesh_geometry
// -------------------------------------------------------------------------------------------

/// `set_mesh_geometry {nodeID, vertices, faces, material?, faceMaterials?}`: the same local
/// validation as `create_mesh`, then one `mesh.setGeometry` on a live mesh.
pub async fn set_mesh_geometry(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(
        &args,
        &["nodeID", "vertices", "faces", "material", "faceMaterials"],
    )?;
    let node_id = node_id_arg(&args)?;
    let material = material_arg(&args, "material")?;
    let mesh = geometry::validate(
        args.get("vertices"),
        args.get("faces"),
        args.get("faceMaterials").filter(|value| !value.is_null()),
        Some(&material),
    )
    .map_err(geometry_invalid)?;
    check_face_material_texts(&mesh)?;
    check_face_normals(&mesh.vertices, &mesh.faces)?;

    let out = send_geometry(
        &state,
        &ctx,
        "mesh.setGeometry",
        Some(node_id),
        Payload {
            vertices: mesh.vertices,
            faces: mesh.faces,
            face_materials: mesh.face_materials,
        },
        &material,
        UNDO_NOTE_GEOMETRY,
        None,
    )
    .await?;
    Ok(Value::Object(out).into())
}

// -------------------------------------------------------------------------------------------
// set_mesh_material
// -------------------------------------------------------------------------------------------

/// `set_mesh_material {nodeID, material, faces?}`: without `faces`, one `mesh.setMaterial` on every
/// face (the bridge keeps texture alignment); with `faces`, a `mesh.setGeometry` rebuild of the
/// current geometry with those faces' materials replaced (the POC's route; the geometry is the live
/// `mesh.get` read, else the saved file — [`current_mesh`]).
pub async fn set_mesh_material(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(&args, &["nodeID", "material", "faces"])?;
    let node_id = node_id_arg(&args)?;
    let material = required_material_arg(&args)?;
    let faces = match args.get("faces") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let Some(indices) = value.as_array().filter(|indices| !indices.is_empty()) else {
                return Err(invalid(format!(
                    "faces must be a non-empty array of face indices, got {value}"
                )));
            };
            let mut out: Vec<usize> = Vec::with_capacity(indices.len());
            for index in indices {
                let Some(index) = index.as_u64().map(|index| index as usize) else {
                    return Err(invalid(format!(
                        "faces must be non-negative integers, got {index}"
                    )));
                };
                out.push(index);
            }
            out.sort_unstable();
            out.dedup();
            Some(out)
        }
    };

    let Some(faces) = faces else {
        return set_all_faces_material(&state, &ctx, node_id, &material).await;
    };

    // the per-face route rebuilds the mesh from its current geometry: the live `mesh.get` read
    // when the bridge has it, else the saved file under the rebuild operators' rules
    // ([`current_mesh`]); the document is read once and handed to the write.
    let (cached, source, doc) = current_mesh(&state, &ctx, node_id).await?;
    let count = cached.faces.len();
    if let Some(bad) = faces.iter().find(|index| **index >= count) {
        return Err(invalid(format!(
            "faces must be indices in 0..{} (the mesh has {count} faces), got {bad}",
            count as i64 - 1
        )));
    }
    // a face without a material keeps the mesh's base one (the first resolved material, else the
    // default), decided on the geometry as it is now — before the chosen faces change
    let before = source_materials(&cached);
    let current_base = base_material(&before);
    let mut materials: Vec<Option<String>> = before
        .into_iter()
        .map(|material| Some(material.unwrap_or_else(|| current_base.clone())))
        .collect();
    for index in &faces {
        materials[*index] = Some(material.clone());
    }
    // the rebuild's base material is the first resolved one (POC `_rebuild`)
    let base = materials
        .iter()
        .flatten()
        .next()
        .cloned()
        .unwrap_or_else(|| DEFAULT_MATERIAL.to_string());

    let mut out = send_geometry(
        &state,
        &ctx,
        "mesh.setGeometry",
        Some(node_id),
        Payload {
            vertices: cached.vertices,
            faces: cached.faces,
            face_materials: Some(materials),
        },
        &base,
        UNDO_NOTE_MATERIAL,
        Some(doc),
    )
    .await?;
    out.insert("geometrySource".into(), json!(source.label()));
    if let Some(note) = source.note() {
        push_note(&mut out, note);
    }
    Ok(Value::Object(out).into())
}

/// The `faces`-less route: `mesh.setMaterial {nodeID, material, expectDoc}` on every face.
async fn set_all_faces_material(
    state: &HammerState,
    ctx: &CallCtx,
    node_id: i64,
    material: &str,
) -> Result<ToolOutput, ToolError> {
    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(state, deadline).await?;
    let doc_id = doc_id_of(&doc);
    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "mesh.setMaterial",
                json!({"nodeID": node_id, "material": material}),
                RequestOpts {
                    deadline_ms: Some(deadline),
                    expect_doc: Some(expect_doc(&doc)),
                    gate: None,
                },
            )
            .await
    };
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            if let Some(tool) = target_failure(&error, node_id) {
                return Err(tool);
            }
            // L1: every `-32603` exit of `mesh.setMaterial` ("mesh has no faces", "out of memory",
            // "could not collect face handles") comes *before* `undo_open`, and nothing after
            // `undo_open` can fail — so this call wrote nothing and a retry is safe. The
            // conservative `maybe_executed` handling stays for `mesh.setGeometry`, which clears the
            // mesh before it rebuilds it.
            if error.kind == Kind::ProtocolError && error.code == Some(-32603) {
                let reason = error
                    .data
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let tool = if reason.contains("mesh has no faces") {
                    ToolError::new(
                        "MeshEmpty",
                        "MeshEmpty",
                        format!(
                            "nodeID {node_id} has no faces to assign a material to (a mesh whose \
                             geometry was emptied — a failed mesh.setGeometry clears the mesh \
                             before it rebuilds it — has none); nothing was written, and \
                             set_mesh_geometry can give it polygons again"
                        ),
                    )
                    .with_code(-32603)
                    .with_data(json!({"nodeID": node_id, "reason": reason}))
                } else {
                    ToolError::from(error)
                };
                return Err(tool.retry_safe(true));
            }
            // H1 (review H1 fix 4): the write may have run anyway.
            return Err(mutator_failure(error, state, doc_id.as_deref()));
        }
    };
    note_edit(state, doc_id.as_deref());

    let mut out = Map::new();
    out.insert("nodeID".into(), json!(node_id));
    out.insert("material".into(), json!(material));
    out.insert(
        "faceCount".into(),
        reply.result.get("faces").cloned().unwrap_or(Value::Null),
    );
    for key in ["origin", "vertexSlots", "faceSlots"] {
        if let Some(value) = reply.result.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    out.insert(
        "undoLabel".into(),
        reply
            .result
            .get("undoLabel")
            .cloned()
            .unwrap_or(Value::Null),
    );
    out.insert("undoNote".into(), json!(UNDO_NOTE_MATERIAL));
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("saved".into(), json!(false));
    out.insert(
        "notes".into(),
        json!([NO_SAVE_NOTE, MATERIAL_ONLY_NOTE]),
    );
    Ok(Value::Object(out).into())
}

// -------------------------------------------------------------------------------------------
// align_texture
// -------------------------------------------------------------------------------------------

/// The JUSTIFY choices `mesh.setFaceTexture` knows.
const JUSTIFY_CHOICES: [&str; 8] =
    ["left", "right", "top", "bottom", "center", "fit", "fitU", "fitV"];
/// Face indices per call: the bridge's mesh cap (`GEO_MAX_FACES * 4`).
const MAX_TEXTURE_FACES: usize = 262_144;
/// Most faces one `mode:"axes"` call lists (the bridge's `TEX_AXES_MAX_FACES`).
const MAX_AXES_FACES: usize = 65_536;

pub fn register_align_texture(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "align_texture",
            "Set the texture mapping (UV) of a LIVE mesh's faces. mode world = snap to the world grid; face = align to each face's plane; justify = fit to the face (needs `justify`); set = only shift/scale/rotate; axes = absolute per-face axes (faces:[{index,axisU,axisV,scale}], bridge m3.18). shift/scale/rotate apply after world/face/set, not with justify or axes. Answers each face's axisU/axisV [x,y,z,offset] and scale (axes: exact). Undoable; not saved.",
            Mutating,
            &["mesh.setFaceTexture"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "nodeID": node_id_schema(),
                "faces": {
                    "type": "array",
                    "minItems": 1,
                    "items": {"type": ["integer", "object"], "minimum": 0},
                    "description": "face indices; axes: {index, axisU:[x,y,z,texel offset], axisV, scale:[su,sv] world units per texel}"
                },
                "mode": {
                    "type": "string",
                    "enum": ["world", "face", "justify", "set", "axes"]
                },
                "justify": {"type": "string", "enum": JUSTIFY_CHOICES},
                "keepScale": {"type": "boolean", "description": "world/face: keep the current scale"},
                "shift": {
                    "type": "array", "items": {"type": "number"}, "minItems": 2, "maxItems": 2,
                    "description": "[u, v] texels, absolute"
                },
                "scale": {
                    "type": "array", "items": {"type": "number", "exclusiveMinimum": 0},
                    "minItems": 2, "maxItems": 2,
                    "description": "[u, v] world units per texel, absolute"
                },
                "rotate": {
                    "type": "number",
                    "description": "degrees added to the current orientation"
                },
                "texSet": {"type": "integer", "enum": [0, 1], "default": 0}
            },
            "required": ["nodeID", "mode"],
            "additionalProperties": false
        })),
        align_texture,
    );
}

/// The validated `align_texture` arguments as the bridge's params.
fn align_params(args: &Value) -> Result<Value, ToolError> {
    reject_unknown_keys(
        args,
        &["nodeID", "faces", "mode", "justify", "keepScale", "shift", "scale", "rotate", "texSet"],
    )?;
    let node_id = node_id_arg(args)?;
    let present = |key: &str| args.get(key).filter(|value| !value.is_null());
    let mode = match present("mode").and_then(Value::as_str) {
        Some(mode @ ("world" | "face" | "justify" | "set" | "axes")) => mode,
        _ => return Err(invalid("mode is required: one of world, face, justify, set, axes")),
    };
    if mode == "axes" {
        return axes_params(args, node_id);
    }
    let mut params = Map::new();
    params.insert("nodeID".into(), json!(node_id));
    params.insert("mode".into(), json!(mode));
    match (mode, present("justify")) {
        ("justify", None) => {
            return Err(invalid(format!(
                "mode justify needs `justify`: one of {}",
                JUSTIFY_CHOICES.join(", ")
            )))
        }
        ("justify", Some(value)) => match value.as_str().filter(|j| JUSTIFY_CHOICES.contains(j)) {
            Some(j) => params.insert("justify".into(), json!(j)),
            None => {
                return Err(invalid(format!(
                    "justify must be one of {}, got {value}",
                    JUSTIFY_CHOICES.join(", ")
                )))
            }
        },
        (_, Some(_)) => return Err(invalid("justify is only valid with mode justify")),
        (_, None) => None,
    };
    if let Some(value) = present("keepScale") {
        let Some(keep) = value.as_bool() else {
            return Err(invalid(format!("keepScale must be a boolean, got {value}")));
        };
        if !matches!(mode, "world" | "face") {
            return Err(invalid("keepScale is only valid with mode world or face"));
        }
        params.insert("keepScale".into(), json!(keep));
    }
    if let Some(value) = present("faces") {
        let Some(list) = value.as_array().filter(|l| !l.is_empty() && l.len() <= MAX_TEXTURE_FACES)
        else {
            return Err(invalid(format!(
                "faces must be a non-empty array of face indices (at most {MAX_TEXTURE_FACES}), got {value}"
            )));
        };
        let mut seen = std::collections::BTreeSet::new();
        for index in list {
            match index.as_u64().filter(|i| *i <= u64::from(u32::MAX)) {
                Some(i) if seen.insert(i) => {}
                Some(i) => return Err(invalid(format!("faces lists index {i} twice"))),
                None => {
                    return Err(invalid(format!(
                        "faces must be non-negative integers, got {index}"
                    )))
                }
            }
        }
        params.insert("faces".into(), value.clone());
    }
    let mut adjusts = false;
    for key in ["shift", "scale"] {
        let Some(value) = present(key) else { continue };
        let pair: Option<Vec<f64>> = value
            .as_array()
            .filter(|a| a.len() == 2)
            .and_then(|a| a.iter().map(|x| x.as_f64().filter(|x| x.is_finite() && x.abs() < 1e30)).collect());
        let Some(pair) = pair else {
            return Err(invalid(format!("{key} must be [u, v], two finite numbers, got {value}")));
        };
        if key == "scale" && pair.iter().any(|x| *x <= 0.0) {
            return Err(invalid(format!("scale must be > 0 in both components, got {value}")));
        }
        adjusts = true;
        params.insert(key.into(), json!(pair));
    }
    if let Some(value) = present("rotate") {
        match value.as_f64().filter(|r| r.is_finite() && r.abs() <= 36_000.0) {
            Some(r) => {
                adjusts = true;
                params.insert("rotate".into(), json!(r));
            }
            None => return Err(invalid(format!("rotate must be degrees (|value| <= 36000), got {value}"))),
        }
    }
    if mode == "justify" && adjusts {
        return Err(invalid(
            "shift, scale and rotate are not valid with mode justify (they would undo it): \
             justify first, then call again with mode set",
        ));
    }
    if mode == "set" && !adjusts {
        return Err(invalid("mode set needs at least one of shift, scale, rotate"));
    }
    if let Some(value) = present("texSet") {
        match value.as_u64().filter(|t| *t <= 1) {
            Some(t) => params.insert("texSet".into(), json!(t)),
            None => return Err(invalid(format!("texSet must be 0 or 1, got {value}"))),
        };
    }
    Ok(Value::Object(params))
}

/// One `[x, y, z, offset]` of `mode:"axes"`: four finite numbers.
fn axes_quad(value: Option<&Value>, label: &str) -> Result<[f64; 4], ToolError> {
    let quad: Option<Vec<f64>> = value.and_then(Value::as_array).filter(|a| a.len() == 4).and_then(|a| {
        a.iter().map(|x| x.as_f64().filter(|x| x.is_finite() && x.abs() < 1e30)).collect()
    });
    match quad {
        Some(q) => Ok([q[0], q[1], q[2], q[3]]),
        None => Err(invalid(format!("{label} must be [x, y, z, offset], four finite numbers, got {}", value.unwrap_or(&Value::Null)))),
    }
}

/// `mode:"axes"`: `faces:[{index, axisU, axisV, scale}]`, validated like the bridge does (finite
/// numbers, scale > 0, axis xyz not zero-length and not parallel), so nothing bad is sent.
fn axes_params(args: &Value, node_id: i64) -> Result<Value, ToolError> {
    let present = |key: &str| args.get(key).filter(|value| !value.is_null());
    for key in ["justify", "keepScale", "shift", "scale", "rotate"] {
        if present(key).is_some() {
            return Err(invalid(format!(
                "{key} is not valid with mode axes (put axisU, axisV and scale inside each faces entry)"
            )));
        }
    }
    let list = present("faces")
        .and_then(Value::as_array)
        .filter(|l| !l.is_empty() && l.len() <= MAX_AXES_FACES)
        .ok_or_else(|| {
            invalid(format!(
                "mode axes needs faces: [{{index, axisU, axisV, scale}}], 1 to {MAX_AXES_FACES} entries"
            ))
        })?;
    let mut seen = std::collections::BTreeSet::new();
    let mut faces = Vec::with_capacity(list.len());
    for (k, entry) in list.iter().enumerate() {
        let label = |what: &str| format!("faces[{k}].{what}");
        let Some(object) = entry.as_object() else {
            return Err(invalid(format!("faces[{k}] must be an object {{index, axisU, axisV, scale}}, got {entry}")));
        };
        if let Some(extra) = object.keys().find(|key| !["index", "axisU", "axisV", "scale"].contains(&key.as_str())) {
            return Err(invalid(format!("faces[{k}] has unknown key {extra:?} (index, axisU, axisV, scale)")));
        }
        let index = object
            .get("index")
            .and_then(Value::as_u64)
            .filter(|i| *i <= u64::from(u32::MAX))
            .ok_or_else(|| invalid(format!("{} must be a non-negative integer", label("index"))))?;
        if !seen.insert(index) {
            return Err(invalid(format!("faces lists index {index} twice")));
        }
        let u = axes_quad(object.get("axisU"), &label("axisU"))?;
        let v = axes_quad(object.get("axisV"), &label("axisV"))?;
        let scale: Option<Vec<f64>> = object.get("scale").and_then(Value::as_array).filter(|a| a.len() == 2).and_then(|a| {
            a.iter().map(|x| x.as_f64().filter(|x| x.is_finite() && *x > 0.0 && *x < 1e30)).collect()
        });
        let Some(scale) = scale else {
            return Err(invalid(format!(
                "{} must be [su, sv], two finite numbers > 0 (world units per texel), got {}",
                label("scale"),
                object.get("scale").unwrap_or(&Value::Null)
            )));
        };
        let (ux, vx) = ([u[0], u[1], u[2]], [v[0], v[1], v[2]]);
        let norm = |a: [f64; 3]| (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
        let (lu, lv) = (norm(ux), norm(vx));
        if lu == 0.0 || lv == 0.0 {
            return Err(invalid(format!("{}: the xyz axis must not be zero-length", label(if lu == 0.0 { "axisU" } else { "axisV" }))));
        }
        let cross = [ux[1] * vx[2] - ux[2] * vx[1], ux[2] * vx[0] - ux[0] * vx[2], ux[0] * vx[1] - ux[1] * vx[0]];
        if norm(cross) <= 1e-6 * lu * lv {
            return Err(invalid(format!("faces[{k}]: axisU and axisV must not be parallel")));
        }
        faces.push(json!({"index": index, "axisU": u, "axisV": v, "scale": scale}));
    }
    let mut params = Map::new();
    params.insert("nodeID".into(), json!(node_id));
    params.insert("mode".into(), json!("axes"));
    if let Some(value) = present("texSet") {
        match value.as_u64().filter(|t| *t <= 1) {
            Some(t) => params.insert("texSet".into(), json!(t)),
            None => return Err(invalid(format!("texSet must be 0 or 1, got {value}"))),
        };
    }
    params.insert("faces".into(), Value::Array(faces));
    let bytes = Value::Object(params.clone()).to_string().len();
    if bytes > MAX_REQUEST_BYTES {
        return Err(invalid(format!(
            "the axes request is {bytes} bytes, over this server's {MAX_REQUEST_BYTES}-byte budget: send fewer faces per call"
        )));
    }
    Ok(Value::Object(params))
}

/// Errors the bridge raises before it opens the undo group: `-32602`, `TextureSetUnknown` (the
/// pre-write read) and the two known pre-write `-32603` reasons. Anything else (a panic after the
/// write, "exec panicked", a read-back mismatch, `maybeExecuted`) may have changed the mesh.
fn align_refusal(error: &BridgeError) -> bool {
    if error.kind != Kind::ProtocolError {
        return false;
    }
    if error.code == Some(-32602) {
        return true;
    }
    if error.code != Some(-32603) || error.data.get("maybeExecuted").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    let reason = error.data.get("reason").and_then(Value::as_str).unwrap_or("");
    error.wire_name.as_deref() == Some("TextureSetUnknown")
        || reason == "mesh has no faces"
        || reason == "could not collect face handles"
}

/// `align_texture {nodeID, mode, ...}`: validate, take the write gate, read the document for
/// `expectDoc`, send `mesh.setFaceTexture`.
pub async fn align_texture(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    let params = align_params(&args)?;
    let node_id = node_id_arg(&args)?;
    Ok(Value::Object(send_align(&state, &ctx, params, node_id).await?).into())
}

/// One validated `mesh.setFaceTexture` request (the caller holds the write gate): the document
/// read, the send, the error mapping and the result map. `model_import hammer_mesh` aligns each
/// chunk through this.
pub(crate) async fn send_align(
    state: &HammerState,
    ctx: &CallCtx,
    params: Value,
    node_id: i64,
) -> Result<Map<String, Value>, ToolError> {
    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(state, deadline).await?;
    let doc_id = doc_id_of(&doc);
    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "mesh.setFaceTexture",
                params,
                RequestOpts {
                    deadline_ms: Some(deadline),
                    expect_doc: Some(expect_doc(&doc)),
                    gate: None,
                },
            )
            .await
    };
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            if let Some(tool) = target_failure(&error, node_id) {
                return Err(tool);
            }
            if align_refusal(&error) {
                return Err(ToolError::from(error).retry_safe(true));
            }
            return Err(mutator_failure(error, state, doc_id.as_deref()));
        }
    };
    note_edit(state, doc_id.as_deref());

    let mut out = Map::new();
    out.insert("nodeID".into(), json!(node_id));
    for key in ["mode", "texSet", "touched", "exact", "readBackTruncated", "faces", "origin", "vertexSlots", "faceSlots", "undoLabel"] {
        if let Some(value) = reply.result.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    out.insert("undoNote".into(), json!("undo_last restores the previous texture mapping"));
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("saved".into(), json!(false));
    out.insert("notes".into(), json!([NO_SAVE_NOTE]));
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// The four rebuild operators (WP-C): source resolution, result checks, shared rebuild
// -------------------------------------------------------------------------------------------

/// Where the geometry being edited came from (the POC's `_current_mesh` source).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeometrySource {
    /// The live document, read through the bridge's `mesh.get` (m3.12): what Hammer holds right
    /// now, undo/redo/UI edits included.
    Live,
    /// The SAVED `.vmap` (`crate::vmap`), readable only while the live document is clean — the
    /// route for a bridge that does not advertise `mesh.get`.
    File,
    /// The SAVED `.vmap` read because `mesh.get` refused this mesh as too large for one reply.
    FileAfterTooLarge,
}

impl GeometrySource {
    /// The POC's `geometrySource` value (`_rebuild`).
    fn label(self) -> &'static str {
        match self {
            GeometrySource::Live => "live",
            GeometrySource::File | GeometrySource::FileAfterTooLarge => "file",
        }
    }

    /// The note this source adds to the result. The file route says where the geometry came from
    /// (and that its nodeIDs are the live document's); the live route needs none.
    fn note(self) -> Option<&'static str> {
        match self {
            GeometrySource::Live => None,
            GeometrySource::File => Some(
                "geometry read from the SAVED .vmap (the live document is clean: no unsaved MCP \
                 edits and Hammer reports it not dirty); its nodeIDs are the live document's node \
                 ids",
            ),
            GeometrySource::FileAfterTooLarge => Some(
                "geometry read from the SAVED .vmap because the mesh is too large for one live \
                 mesh.get reply (the live document is clean: no unsaved MCP edits and Hammer \
                 reports it not dirty); its nodeIDs are the live document's node ids",
            ),
        }
    }
}

/// `_current_mesh`: the geometry to edit and where it came from, plus the document the caller
/// must hand to [`send_geometry`].
///
/// 1. **Live** (the bridge advertises `mesh.get`): the mesh as Hammer holds it now, converted to
///    world space ([`crate::tools::mesh_read`]). No dirty gate, no scale/parent refusal: the read
///    is the document's own state, so there is nothing a stale file could contradict. A rebuild
///    (`mesh.setGeometry`, bridge `geo_reset_mesh`) deletes the faces and puts the node's scales
///    and angles back to identity and its pivot to zero, then builds the polygons from WORLD
///    points — so the world soup sent here reproduces the mesh whatever scale, rotation or pivot
///    it had (the file route's scaled-node refusal exists only because a *file* read could not say
///    what the live node's scale was; `mesh.get` reports it and the vertices carry it).
/// 2. **File** (an older bridge): the SAVED `.vmap`, only while the live document is clean —
///    `-32016 UnsavedChanges` otherwise, because the rebuild would replace the mesh's in-editor
///    geometry with the file's version — and refusing the two kinds of mesh the file route cannot
///    place correctly ([`scaled_mesh_refusal`], [`unproven_parent_refusal`]). An untitled
///    document and an unmappable Windows path are refused instead of guessed at.
///
/// The document is read here and handed through to [`send_geometry`] (review-2 L2), so the write
/// carries *this* read's fingerprint and no second `doc.active` runs between the read and the
/// write: a document that changed in between is refused by the bridge's own `expectDoc` gate.
async fn current_mesh(
    state: &HammerState,
    ctx: &CallCtx,
    node_id: i64,
) -> Result<(Mesh, GeometrySource, Value), ToolError> {
    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(state, deadline).await?;
    let too_large =
        match super::mesh_read::read_live_mesh(state, deadline, &doc, node_id).await {
            Ok(Some(record)) => {
                // `mesh.get`'s origin/angles are the node's own: a container whose class is not
                // proven to store its children in world space would make the "world" vertices
                // parent-space (the file route's review-3 NEW-5 gate, on the live ancestors).
                if let Some(problem) = record.parent_problem() {
                    return Err(unproven_parent_refusal(node_id, problem, None));
                }
                return Ok((super::mesh_read::mesh_of(&record), GeometrySource::Live, doc));
            }
            Ok(None) => None,
            // too large for one reply: the saved file may still answer (clean document only);
            // when it cannot, the refusal below is this error, with the counts and a save hint
            Err(error) if super::mesh_read::is_too_large(&error) => Some(error),
            Err(error) => return Err(error),
        };
    match file_geometry(state, &doc, node_id).await {
        Ok(mesh) => {
            let source = if too_large.is_some() {
                GeometrySource::FileAfterTooLarge
            } else {
                GeometrySource::File
            };
            Ok((mesh, source, doc))
        }
        Err(error) => Err(too_large.unwrap_or(error)),
    }
}

/// The SAVED-file route of [`current_mesh`]: the gates, then the mesh's world geometry.
async fn file_geometry(
    state: &HammerState,
    doc: &Value,
    node_id: i64,
) -> Result<Mesh, ToolError> {
    if super::lifecycle::is_untitled(doc) {
        return Err(geometry_unavailable_untitled(doc, node_id));
    }
    // read after `require_document`: a document switch has already reset the counter
    let unsaved = state.unsaved_edits();
    let dirty = super::lifecycle::dirty_state(doc);
    if dirty != Some(false) || unsaved > 0 {
        return Err(source_unsaved_changes(doc, node_id, dirty, unsaved));
    }
    let windows = doc.get("path").and_then(Value::as_str).unwrap_or_default();
    let Some(host) = state.path_map.windows_to_host(windows) else {
        return Err(path_not_mapped(windows));
    };
    let document = crate::vmap::load(&host)
        .map_err(|error| super::entities_file::vmap_error(error, Some(windows)))?;
    let Some(record) = document.mesh_by_id(node_id) else {
        return Err(mesh_not_in_file(node_id, windows));
    };
    // review-3 NEW-5: the extraction applies only the mesh's *own* origin/angles/scales; a parent
    // whose class we have not proven to be a plain container (a deformer, an instance, a prefab,
    // anything unknown) or a mesh outside the world tree could hold its geometry in another space.
    if let Some(problem) = record.parent_problem() {
        return Err(unproven_parent_refusal(node_id, problem, Some(windows)));
    }
    // (A node `scales` other than the identity needs no refusal: the file's vertices are already
    // world-scaled by the extraction, and the bridge's rebuild puts the node's scales and angles
    // back to identity before it builds from those world points — `geo_reset_mesh`, m2.18.)
    Ok(super::mesh_read::mesh_of(record))
}

/// review-3 NEW-5: the file route's parent gate — the recorded ancestor chain is not one whose
/// stored geometry this crate can prove is the live world geometry
/// ([`crate::vmap::mesh::MeshRecord::parent_problem`]; the M1 gate above is the same family, for
/// the node's own scale).
fn unproven_parent_refusal(
    node_id: i64,
    problem: crate::vmap::mesh::ParentProblem,
    windows: Option<&str>,
) -> ToolError {
    // the live route reads the ancestors from `mesh.get`; the file route from the saved tree
    let place = if windows.is_some() { "in the saved .vmap" } else { "in the live document" };
    use crate::vmap::mesh::ParentProblem;
    let (reason, parent, message) = match problem {
        ParentProblem::Unrooted => (
            "unrooted mesh",
            Value::Null,
            format!(
                "the mesh with nodeID {node_id} {place} is not inside the map's world \
                 tree (no parent chain reaches CMapWorld), so its vertices are not proven \
                 to be the live node's world geometry: nothing was sent. Send the geometry you \
                 want with set_mesh_geometry instead."
            ),
        ),
        ParentProblem::Unsupported(class) => (
            "unsupported parent",
            json!(class),
            format!(
                "the mesh with nodeID {node_id} {place} sits under a {class} ancestor: \
                 only CMapWorld/CMapGroup/CMapEntity containers are proven to store their children \
                 in world space (only the node's own origin/angles/scales are applied, not a \
                 parent transform), so a deformer, an instance, a prefab or any other container \
                 could hold this geometry in another space. Nothing was sent: send the geometry \
                 you want with set_mesh_geometry instead."
            ),
        ),
    };
    ToolError::new("MeshGeometryUnavailable", "MeshGeometryUnavailable", message)
        .with_data(json!({
            "nodeID": node_id,
            "geometrySource": "unavailable",
            "reason": reason,
            "parent": parent,
            "path": windows,
        }))
        .retry_safe(true)
}

/// The source geometry's per-face materials, one entry per face (`None` = the base material); a
/// missing or mis-sized list reads as "every face takes the base material".
fn source_materials(mesh: &Mesh) -> Vec<Option<String>> {
    match &mesh.face_materials {
        Some(materials) if materials.len() == mesh.faces.len() => materials.clone(),
        _ => vec![None; mesh.faces.len()],
    }
}

/// `_rebuild`'s base material: the first *resolved* per-face material, else the default (the POC's
/// `next((m for m in (fm or []) if m), DEFAULT_MATERIAL)`).
fn base_material(materials: &[Option<String>]) -> String {
    materials
        .iter()
        .flatten()
        .next()
        .cloned()
        .unwrap_or_else(|| DEFAULT_MATERIAL.to_string())
}

/// `MeshGeometryUnavailable`: the bridge has no `mesh.get` and there is no SAVED `.vmap` to read
/// the geometry from, because the active document is untitled.
fn geometry_unavailable_untitled(doc: &Value, node_id: i64) -> ToolError {
    ToolError::new(
        "MeshGeometryUnavailable",
        "MeshGeometryUnavailable",
        format!(
            "nodeID {node_id}'s geometry cannot be read live (this bridge does not advertise \
             mesh.get) and the active document is untitled, so \
             there is no SAVED .vmap to read it from: run save_map first (the file route then \
             supplies the geometry), or send the geometry you want with set_mesh_geometry. \
             Nothing was sent."
        ),
    )
    .with_data(json!({
        "nodeID": node_id,
        "geometrySource": "unavailable",
        "reason": "untitled",
        "path": doc.get("path").cloned().unwrap_or(Value::Null),
    }))
    .retry_safe(true)
}

/// The dirty/unknown refusal (`-32016 UnsavedChanges`, the code `reload_map` uses for the same
/// hazard): the geometry would have to come from the SAVED file, and the rebuild would replace the
/// mesh's in-editor geometry with it.
fn source_unsaved_changes(
    doc: &Value,
    node_id: i64,
    dirty: Option<bool>,
    unsaved: u64,
) -> ToolError {
    let mut causes: Vec<String> = Vec::new();
    match dirty {
        Some(true) => causes.push("Hammer reports the document dirty".to_string()),
        Some(false) => {}
        None => causes.push("the document does not report its dirty state".to_string()),
    }
    if unsaved > 0 {
        causes.push(format!("{unsaved} unsaved bridge edit(s) by this server"));
    }
    let reason = match (dirty, unsaved > 0) {
        (Some(false), _) => "unsaved bridge edits",
        (Some(true), true) => "dirty document and unsaved bridge edits",
        (Some(true), false) => "dirty document",
        (None, false) => "dirty state unreported",
        (None, true) => "dirty state unreported and unsaved bridge edits",
    };
    // review-2 N6: once Hammer's UI saved the file (`dirty:false`), this conservative counter
    // gate keeps refusing no matter how clean the document looks — say where the reset lives,
    // so the caller does not retry a refusal that never lifts until our save_map runs.
    let hint = if dirty == Some(false) {
        "; a save made in Hammer's UI is not seen by this server: the counter resets on our \
         save_map only"
    } else {
        ""
    };
    ToolError::new(
        "UnsavedChanges",
        "UnsavedChanges",
        format!(
            "the geometry of nodeID {node_id} would have to come from the SAVED .vmap ({}), but \
             the rebuild would replace this mesh with that file version: run save_map first, or \
             send the geometry you want with set_mesh_geometry{hint}. Nothing was sent.",
            causes.join(" and ")
        ),
    )
    .with_code(-32016)
    .with_data(json!({
        "nodeID": node_id,
        "geometrySource": "unavailable",
        "reason": reason,
        "path": doc.get("path").cloned().unwrap_or(Value::Null),
        "unsavedBridgeEdits": unsaved,
    }))
    .retry_safe(true)
}

/// The document's Windows path has no host mapping: the SAVED `.vmap` cannot be read (the POC's
/// `PathNotMapped`).
fn path_not_mapped(windows: &str) -> ToolError {
    ToolError::new(
        "PathNotMapped",
        "PathNotMapped",
        format!(
            "cannot map the Windows path {windows:?} to a host path, so the SAVED .vmap cannot be \
             read: set CS2_MCP_PATH_MAP (e.g. 'z:\\app=<host path>'). Nothing was sent."
        ),
    )
    .with_code(-32602)
    .with_data(json!({"windowsPath": windows, "geometrySource": "unavailable"}))
    .retry_safe(true)
}

/// The file was read and holds no such mesh (the POC's `MeshNotFound`, the message `get_mesh`
/// uses too: the mesh may simply have been created since the last save).
fn mesh_not_in_file(node_id: i64, windows: &str) -> ToolError {
    ToolError::new(
        "MeshNotFound",
        "MeshNotFound",
        format!(
            "no mesh with nodeID {node_id} in the saved .vmap; save_map first if it was created \
             since the last save"
        ),
    )
    .with_data(json!({"nodeID": node_id, "path": windows, "geometrySource": "file"}))
    .retry_safe(true)
}

/// The gate every operator's **result** goes through before it is dispatched: the counts the bridge
/// parses (`geom_parse`), coordinates inside its range, and no degenerate face — Hammer would
/// create nothing for a zero-area polygon, and a `mesh.setGeometry` that fails after `geo_reset_mesh`
/// has already cleared the mesh would lose it.
fn check_result(vertices: &[Vec3], faces: &[Vec<usize>]) -> Result<(), ToolError> {
    if vertices.len() < 3 || vertices.len() > geometry::MAX_VERTICES {
        return Err(invalid(format!(
            "the rebuilt mesh would have {} vertices (the bridge takes 3..={})",
            vertices.len(),
            geometry::MAX_VERTICES
        )));
    }
    if faces.is_empty() || faces.len() > geometry::MAX_FACES {
        return Err(invalid(format!(
            "the rebuilt mesh would have {} faces (the bridge takes 1..={})",
            faces.len(),
            geometry::MAX_FACES
        )));
    }
    check_coords(vertices)?;
    check_face_normals(vertices, faces)
}

/// `_rebuild`: one `mesh.setGeometry` through [`send_geometry`] (the same send/error path
/// `set_mesh_geometry` uses, so the target gate, the request limits, the unsaved counter, the
/// bridge's own undo label all behave identically) plus the POC's
/// `geometrySource` and the notes: this server's, the source's, then the operator's own. `doc`
/// is the document `current_mesh` already read (review-2 L2): the write carries its fingerprint,
/// with no second `doc.active` between the gate and the write.
#[allow(clippy::too_many_arguments)] // the POC `_rebuild`'s own parameter list plus the shared send path's
async fn rebuild(
    state: &HammerState,
    ctx: &CallCtx,
    node_id: i64,
    payload: Payload,
    base: &str,
    source: GeometrySource,
    extra_notes: &[&str],
    doc: Value,
) -> Result<Map<String, Value>, ToolError> {
    let mut out = send_geometry(
        state,
        ctx,
        "mesh.setGeometry",
        Some(node_id),
        payload,
        base,
        UNDO_NOTE_GEOMETRY,
        Some(doc),
    )
    .await?;
    out.insert("geometrySource".into(), json!(source.label()));
    let mut notes = match out.remove("notes") {
        Some(Value::Array(notes)) => notes,
        _ => Vec::new(),
    };
    if let Some(note) = source.note() {
        notes.push(json!(note));
    }
    notes.extend(extra_notes.iter().map(|note| json!(note)));
    out.insert("notes".into(), Value::Array(notes));
    Ok(out)
}

/// `translate`/`pivot` (optional): `[x, y, z]` of finite numbers (the POC's `_vec3`). Unlike
/// [`vec3_arg`] these are not coordinates the bridge parses — the *result* is what has to stay in
/// range ([`check_result`]) — so the ±1e6 bound does not apply to them.
fn finite_vec3_arg(args: &Value, key: &str) -> Result<Option<Vec3>, ToolError> {
    let Some(value) = args.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let message = || invalid(format!("{key} must be [x, y, z] (3 finite numbers)"));
    let Some(axes) = value.as_array().filter(|axes| axes.len() == 3) else {
        return Err(message());
    };
    let mut out: Vec3 = [0.0; 3];
    for (axis, value) in axes.iter().enumerate() {
        out[axis] = value
            .as_f64()
            .filter(|number| number.is_finite())
            .ok_or_else(message)?;
    }
    Ok(Some(out))
}

/// `rotateYaw` (optional, 0 by default): a finite number of degrees.
fn yaw_arg(args: &Value) -> Result<f64, ToolError> {
    match args.get("rotateYaw") {
        None | Some(Value::Null) => Ok(0.0),
        Some(value) => value
            .as_f64()
            .filter(|yaw| yaw.is_finite())
            .ok_or_else(|| invalid(format!("rotateYaw must be a number, got {value}"))),
    }
}

/// `scale` (optional): a number (uniform) or `[sx, sy, sz]`, normalised to three axes for
/// [`geometry::transform`]. Non-finite axes and zeros are the geometry layer's own refusal
/// ("scale must be non-zero numbers"), exactly as in the POC.
fn scale_arg(args: &Value) -> Result<Option<Vec3>, ToolError> {
    let Some(value) = args.get("scale").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    if let Some(number) = value.as_f64().filter(|number| number.is_finite()) {
        return Ok(Some([number; 3]));
    }
    let Some(axes) = value.as_array().filter(|axes| axes.len() == 3) else {
        return Err(invalid(format!(
            "scale must be a number or [sx, sy, sz], got {value}"
        )));
    };
    let mut out: Vec3 = [0.0; 3];
    for (axis, value) in axes.iter().enumerate() {
        out[axis] = value
            .as_f64()
            .filter(|number| number.is_finite())
            .ok_or_else(|| invalid("scale must be non-zero numbers"))?;
    }
    Ok(Some(out))
}

/// A non-empty list of integer indices in `0..bound` (`move_mesh_vertices`' `vertices`,
/// `delete_mesh_faces`' `faces`). The POC hands the raw value to `geometry.py`, whose refusals are
/// these messages ("vertex indices must be in 0..N" / "face indices must be in 0..N"); a wrong
/// shape (absent, empty, not a list, a float, a bool) is the same refusal here.
fn index_list_arg(args: &Value, key: &str, what: &str, bound: usize) -> Result<Vec<usize>, ToolError> {
    let message = || invalid(format!("{what} indices must be in 0..{}", bound.saturating_sub(1)));
    let Some(items) = args
        .get(key)
        .filter(|value| !value.is_null())
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
    else {
        return Err(message());
    };
    // review-2 L5: a list longer than the bound necessarily carries duplicate entries (the POC
    // treats them as a set), and the operator walks every listed index — so cap the work at the
    // mesh's own size, before any of the geometry math runs.
    if items.len() > bound {
        return Err(invalid(format!(
            "{what} indices: at most one entry per {what} (the mesh has {bound}), got {} entries",
            items.len()
        )));
    }
    let mut out = Vec::with_capacity(items.len());
    for index in items {
        let Some(index) = index
            .as_u64()
            .map(|index| index as usize)
            .filter(|index| *index < bound)
        else {
            return Err(message());
        };
        out.push(index);
    }
    Ok(out)
}

/// `offset` (required): `[dx, dy, dz]` of finite numbers. The POC hands the raw value to
/// `geometry.move_vertices`, so a wrong shape is the geometry layer's wording ("offset must be
/// [dx, dy, dz]") — not `_vec3`'s.
fn offset_arg(args: &Value) -> Result<Vec3, ToolError> {
    let message = || invalid("offset must be [dx, dy, dz]");
    let Some(axes) = args
        .get("offset")
        .filter(|value| !value.is_null())
        .and_then(Value::as_array)
        .filter(|axes| axes.len() == 3)
    else {
        return Err(message());
    };
    let mut out: Vec3 = [0.0; 3];
    for (axis, value) in axes.iter().enumerate() {
        out[axis] = value
            .as_f64()
            .filter(|number| number.is_finite())
            .ok_or_else(message)?;
    }
    Ok(out)
}

/// `face` (required): a non-negative integer index. The range is `geometry::extrude_face`'s own
/// refusal ("face index outside 0..N"), the POC's wording.
fn face_arg(args: &Value) -> Result<usize, ToolError> {
    args.get("face")
        .and_then(Value::as_u64)
        .map(|face| face as usize)
        .ok_or_else(|| {
            invalid("face must be a non-negative integer index (see get_mesh for the face list)")
        })
}

/// `distance` (required): a finite number. The POC's `_num(distance)` refusal is the geometry
/// layer's text; a zero falls out of `geometry::extrude_face` as well.
fn distance_arg(args: &Value) -> Result<f64, ToolError> {
    args.get("distance")
        .and_then(Value::as_f64)
        .filter(|distance| distance.is_finite())
        .ok_or_else(|| invalid("distance must be a non-zero number"))
}

// -------------------------------------------------------------------------------------------
// transform_mesh
// -------------------------------------------------------------------------------------------

/// `transform_mesh {nodeID, translate?, rotateYaw?, scale?, pivot?}` (POC `h_transform_mesh`):
/// scale about the pivot, then yaw-rotate about it, then translate; a mirroring scale flips every
/// face's winding; then one `mesh.setGeometry` rebuild.
pub async fn transform_mesh(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(
        &args,
        &["nodeID", "translate", "rotateYaw", "scale", "pivot"],
    )?;
    let node_id = node_id_arg(&args)?;
    // review-2 N1: parse and refuse the arguments before the geometry is resolved (two bridge
    // round trips plus a file parse) — `{nodeID, scale: 0}` on a dirty document answers
    // InvalidParams, not the source route's UnsavedChanges. Only the index bounds need the mesh,
    // and transform has none.
    let translate = finite_vec3_arg(&args, "translate")?;
    let pivot = finite_vec3_arg(&args, "pivot")?;
    let yaw = yaw_arg(&args)?;
    let scale = scale_arg(&args)?;
    if translate.is_none() && yaw == 0.0 && scale.is_none() {
        return Err(invalid("give translate, rotateYaw and/or scale"));
    }
    // ... and the zero-scale refusal is the geometry layer's own, so `geometry::transform`
    // answers with the same message when it runs
    if let Some(scale) = scale {
        if scale.contains(&0.0) {
            return Err(invalid("scale must be non-zero numbers"));
        }
    }
    let (mesh, source, doc) = current_mesh(&state, &ctx, node_id).await?;
    let vertices =
        geometry::transform(&mesh.vertices, translate, yaw, scale, pivot).map_err(geometry_invalid)?;
    let faces = match scale {
        Some(scale) if geometry::mirrored(scale) => geometry::flip_faces(&mesh.faces, None),
        _ => mesh.faces.clone(),
    };
    check_result(&vertices, &faces)?;

    let materials = source_materials(&mesh);
    let base = base_material(&materials);
    let out = rebuild(
        &state,
        &ctx,
        node_id,
        Payload {
            vertices,
            faces,
            face_materials: Some(materials),
        },
        &base,
        source,
        &[],
        doc,
    )
    .await?;
    Ok(Value::Object(out).into())
}

// -------------------------------------------------------------------------------------------
// move_mesh_vertices
// -------------------------------------------------------------------------------------------

/// `move_mesh_vertices {nodeID, vertices, offset}` (POC `h_move_mesh_vertices`): offset the listed
/// vertex indices, refuse a result that flattens any face, then one `mesh.setGeometry` rebuild.
pub async fn move_mesh_vertices(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(&args, &["nodeID", "vertices", "offset"])?;
    let node_id = node_id_arg(&args)?;
    let offset = offset_arg(&args)?;
    let (mesh, source, doc) = current_mesh(&state, &ctx, node_id).await?;
    let indices = index_list_arg(&args, "vertices", "vertex", mesh.vertices.len())?;
    let vertices =
        geometry::move_vertices(&mesh.vertices, &indices, offset).map_err(geometry_invalid)?;
    // the POC checks every face of the *moved* geometry (drag a vertex and a face can collapse)
    check_result(&vertices, &mesh.faces)?;

    let materials = source_materials(&mesh);
    let base = base_material(&materials);
    let out = rebuild(
        &state,
        &ctx,
        node_id,
        Payload {
            vertices,
            faces: mesh.faces.clone(),
            face_materials: Some(materials),
        },
        &base,
        source,
        &[],
        doc,
    )
    .await?;
    Ok(Value::Object(out).into())
}

// -------------------------------------------------------------------------------------------
// extrude_mesh_face
// -------------------------------------------------------------------------------------------

/// `extrude_mesh_face {nodeID, face, distance, sideMaterial?}` (POC `h_extrude_mesh_face`): push
/// one face along its normal (negative = inward), add one quad per edge, then one
/// `mesh.setGeometry` rebuild. The result names the new faces and the cap's slot.
pub async fn extrude_mesh_face(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(&args, &["nodeID", "face", "distance", "sideMaterial"])?;
    let node_id = node_id_arg(&args)?;
    // review-2 N1: the distance (and a bad sideMaterial) is refused before the geometry route
    // runs — only `face` needs the resolved mesh's face count
    let distance = distance_arg(&args)?;
    let side = optional_material_arg(&args, "sideMaterial")?;
    let (mesh, source, doc) = current_mesh(&state, &ctx, node_id).await?;
    let face = face_arg(&args)?;
    let materials = source_materials(&mesh);
    let (vertices, faces, face_materials) = geometry::extrude_face(
        &mesh.vertices,
        &mesh.faces,
        Some(&materials),
        face,
        distance,
        side.as_deref(),
    )
    .map_err(geometry_invalid)?;
    check_result(&vertices, &faces)?;

    let new_faces: Vec<usize> = (mesh.faces.len()..faces.len()).collect();
    let base = base_material(&face_materials);
    let mut out = rebuild(
        &state,
        &ctx,
        node_id,
        Payload {
            vertices,
            faces,
            face_materials: Some(face_materials),
        },
        &base,
        source,
        &[],
        doc,
    )
    .await?;
    out.insert("newFaces".into(), json!(new_faces));
    out.insert("capFace".into(), json!(face));
    Ok(Value::Object(out).into())
}

// -------------------------------------------------------------------------------------------
// delete_mesh_faces
// -------------------------------------------------------------------------------------------

/// `delete_mesh_faces {nodeID, faces}` (POC `h_delete_mesh_faces`): drop the listed faces and the
/// vertices they leave unused, then one `mesh.setGeometry` rebuild. Deleting every face is refused
/// (delete the node instead).
pub async fn delete_mesh_faces(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(&args, &["nodeID", "faces"])?;
    let node_id = node_id_arg(&args)?;
    let (mesh, source, doc) = current_mesh(&state, &ctx, node_id).await?;
    let indices = index_list_arg(&args, "faces", "face", mesh.faces.len())?;
    let materials = source_materials(&mesh);
    let kept = geometry::delete_faces(&mesh.vertices, &mesh.faces, Some(&materials), &indices)
        .map_err(geometry_invalid)?;
    check_result(&kept.vertices, &kept.faces)?;

    let materials = source_materials(&kept);
    let base = base_material(&materials);
    let out = rebuild(
        &state,
        &ctx,
        node_id,
        Payload {
            vertices: kept.vertices,
            faces: kept.faces,
            face_materials: Some(materials),
        },
        &base,
        source,
        &[RENUMBERED_NOTE],
        doc,
    )
    .await?;
    Ok(Value::Object(out).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round4_matches_the_pocs_rounding_and_normalises_negative_zero() {
        assert_eq!(round4([0.000049, 1.0, -0.0]), [0.0, 1.0, 0.0]);
        assert_eq!(round4([256.123456, -0.00005, 3.0]), [256.1235, -0.0001, 3.0]);
        assert_eq!(round4([5.5e-17, 1.0, 2.0]), [0.0, 1.0, 2.0]);
        // review N4: Python's `round` rounds the *binary* value (0.00035 is 3.4999999999999997e-4,
        // below the .00035 midpoint), so it gives 0.0003. `(x * 1e4).round() / 1e4` rounds the
        // product, which lands on 3.5 here, half away from zero -> 0.0004: a last-digit difference
        // the oracle does not have. Checked against `python3 -c "print(round(0.00035, 4))"`.
        assert_eq!(round4([0.00035, 0.0, 0.0])[0], 0.0003);
        assert_eq!(
            crate::vmap::mesh::round4(0.00035),
            0.0003,
            "the one shared implementation is the Python-faithful one"
        );
        assert_eq!(crate::vmap::mesh::round4(123_456.000_05), 123_456.000_1);
    }

    #[test]
    fn target_failure_names_the_bridge_conditions() {
        let missing = BridgeError::new(Kind::NodeNotFound, "no node").with_code(-32022);
        let mapped = target_failure(&missing, 7).expect("mapped");
        assert_eq!(mapped.name, "MeshNotFound");
        assert_eq!(mapped.code, Some(-32022));
        assert!(mapped.retry_safe);

        let wrong_class = BridgeError::new(Kind::ProtocolError, "node is not a mesh")
            .with_code(-32602)
            .with_data(json!({"reason": "node is not a mesh"}));
        let mapped = target_failure(&wrong_class, 7).expect("mapped");
        assert_eq!(mapped.name, "NotAMesh");
        assert!(mapped.retry_safe);

        // any other protocol error keeps the wire mapping (and the maybe-executed handling)
        let other = BridgeError::new(Kind::ProtocolError, "mesh has no faces").with_code(-32603);
        assert!(target_failure(&other, 7).is_none());
    }

    #[test]
    fn material_and_vec3_arguments_are_bounded() {
        assert_eq!(
            material_arg(&json!({}), "material").unwrap(),
            DEFAULT_MATERIAL
        );
        assert_eq!(
            material_arg(&json!({"material": " materials/x.vmat "}), "material").unwrap(),
            "materials/x.vmat"
        );
        for bad in [
            json!({"material": ""}),
            json!({"material": "  "}),
            json!({"material": 7}),
            json!({"material": "a\nb"}),
        ] {
            assert!(material_arg(&bad, "material").is_err(), "{bad}");
        }
        // the bridge's material buffer is 260 bytes
        let long = "m".repeat(MAX_MATERIAL_BYTES + 1);
        assert!(material_arg(&json!({"material": long}), "material").is_err());

        assert_eq!(vec3_arg(&json!({"min": [1, 2, 3]}), "min").unwrap(), Some([1.0, 2.0, 3.0]));
        assert_eq!(vec3_arg(&json!({}), "min").unwrap(), None);
        assert_eq!(vec3_arg(&json!({"min": null}), "min").unwrap(), None);
        for bad in [
            json!({"min": [1, 2]}),
            json!({"min": [1, 2, "3"]}),
            json!({"min": "0 0 0"}),
            json!({"min": [1e6, 0, 0]}),
        ] {
            assert!(vec3_arg(&bad, "min").is_err(), "{bad}");
        }
    }

    // ---------------------------------------------------------------------------------------
    // The rebuild operators' argument and source helpers (WP-C)
    // ---------------------------------------------------------------------------------------

    #[test]
    fn transform_arguments_are_the_pocs() {
        // translate/pivot: finite [x, y, z] or absent — and, unlike `vec3_arg`, no ±1e6 bound:
        // these are not coordinates the bridge parses, the *result* is what `check_result` gates
        assert_eq!(finite_vec3_arg(&json!({}), "translate").unwrap(), None);
        assert_eq!(
            finite_vec3_arg(&json!({"translate": null}), "translate").unwrap(),
            None
        );
        assert_eq!(
            finite_vec3_arg(&json!({"translate": [1, 2, 3]}), "translate").unwrap(),
            Some([1.0, 2.0, 3.0])
        );
        assert_eq!(
            finite_vec3_arg(&json!({"translate": [2e6, 0, 0]}), "translate").unwrap(),
            Some([2e6, 0.0, 0.0])
        );
        for bad in [
            json!({"translate": [1, 2]}),
            json!({"translate": [1, 2, "3"]}),
            json!({"translate": "1 2 3"}),
            json!({"translate": true}),
        ] {
            assert!(finite_vec3_arg(&bad, "translate").is_err(), "{bad}");
        }

        // rotateYaw: absent/null = 0, a number otherwise (JSON cannot carry NaN/Infinity)
        assert_eq!(yaw_arg(&json!({})).unwrap(), 0.0);
        assert_eq!(yaw_arg(&json!({"rotateYaw": null})).unwrap(), 0.0);
        assert_eq!(yaw_arg(&json!({"rotateYaw": 0})).unwrap(), 0.0);
        assert_eq!(yaw_arg(&json!({"rotateYaw": -90.5})).unwrap(), -90.5);
        for bad in [json!({"rotateYaw": "90"}), json!({"rotateYaw": true})] {
            assert!(yaw_arg(&bad).is_err(), "{bad}");
        }

        // scale: a number is uniform, [sx, sy, sz] is taken as given; a wrong shape is the POC's
        // shape message, a non-numeric/zero axis is the geometry layer's own message
        assert_eq!(scale_arg(&json!({})).unwrap(), None);
        assert_eq!(scale_arg(&json!({"scale": null})).unwrap(), None);
        assert_eq!(scale_arg(&json!({"scale": 2})).unwrap(), Some([2.0; 3]));
        assert_eq!(
            scale_arg(&json!({"scale": [1, -1, 1]})).unwrap(),
            Some([1.0, -1.0, 1.0])
        );
        for bad in [
            json!({"scale": [1, 2]}),
            json!({"scale": [1, 2, 3, 4]}),
            json!({"scale": "2"}),
            json!({"scale": true}),
            json!({"scale": {"x": 1}}),
        ] {
            let err = scale_arg(&bad).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}");
            assert!(err.message.contains("scale must be a number"), "{bad}: {err:?}");
        }
        assert_eq!(
            scale_arg(&json!({"scale": [1, "x", 1]})).unwrap_err().message,
            "scale must be non-zero numbers"
        );
        // a zero axis passes here and is refused by `geometry::transform` (unit-tested there)
        assert_eq!(
            geometry::transform(
                &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                None,
                0.0,
                scale_arg(&json!({"scale": [1, 0, 1]})).unwrap(),
                None
            )
            .unwrap_err()
            .0,
            "scale must be non-zero numbers"
        );
    }

    #[test]
    fn index_face_and_distance_arguments_are_bounded() {
        assert_eq!(
            index_list_arg(&json!({"vertices": [0, 7]}), "vertices", "vertex", 8).unwrap(),
            vec![0, 7]
        );
        for bad in [
            json!({}),
            json!({"vertices": null}),
            json!({"vertices": []}),
            json!({"vertices": "0"}),
            json!({"vertices": [0, -1]}),
            json!({"vertices": [0, 8]}),
            json!({"vertices": [0, 1.5]}),
            json!({"vertices": [false]}),
        ] {
            let err = index_list_arg(&bad, "vertices", "vertex", 8).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}");
            assert_eq!(err.message, "vertex indices must be in 0..7", "{bad}");
            assert!(err.retry_safe, "{bad}");
        }
        // the same helper is the POC's `face indices must be in 0..N` for delete_mesh_faces
        assert_eq!(
            index_list_arg(&json!({"faces": []}), "faces", "face", 6)
                .unwrap_err()
                .message,
            "face indices must be in 0..5"
        );

        assert_eq!(face_arg(&json!({"face": 0})).unwrap(), 0);
        // `offset` (move_mesh_vertices): the geometry layer's wording, not `_vec3`'s
        assert_eq!(offset_arg(&json!({"offset": [1, -2, 3]})).unwrap(), [1.0, -2.0, 3.0]);
        assert_eq!(offset_arg(&json!({"offset": [0, 0, 0]})).unwrap(), [0.0; 3]);
        for bad in [
            json!({}),
            json!({"offset": null}),
            json!({"offset": [0, 0]}),
            json!({"offset": [0, 0, "1"]}),
            json!({"offset": true}),
        ] {
            let err = offset_arg(&bad).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}");
            assert_eq!(err.message, "offset must be [dx, dy, dz]", "{bad}");
        }        for bad in [json!({}), json!({"face": null}), json!({"face": -1}), json!({"face": 1.5})] {
            assert_eq!(face_arg(&bad).unwrap_err().name, "InvalidParams", "{bad}");
        }
        assert_eq!(distance_arg(&json!({"distance": -32.5})).unwrap(), -32.5);
        // a zero is a *finite* number here: the refusal is the geometry layer's own wording
        assert_eq!(distance_arg(&json!({"distance": 0})).unwrap(), 0.0);
        assert_eq!(
            geometry::extrude_face(
                &[[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                &[vec![0, 1, 2]],
                None,
                0,
                0.0,
                None
            )
            .unwrap_err()
            .0,
            "distance must be a non-zero number"
        );
        for bad in [json!({}), json!({"distance": "1"}), json!({"distance": null})] {
            let err = distance_arg(&bad).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}");
            assert_eq!(err.message, "distance must be a non-zero number", "{bad}");
        }
    }

    #[test]
    fn the_source_geometry_helpers_follow_the_poc() {
        // `_rebuild`'s base material: the first resolved entry, else the POC's default
        assert_eq!(base_material(&[]), DEFAULT_MATERIAL);
        assert_eq!(base_material(&[None, None]), DEFAULT_MATERIAL);
        assert_eq!(base_material(&[None, Some("b".into())]), "b");
        assert_eq!(base_material(&[Some("a".into()), Some("b".into())]), "a");

        // a source whose faceMaterials do not line up reads as "every face takes the base"
        let mesh = Mesh {
            vertices: vec![[0.0; 3], [1.0; 3], [2.0; 3]],
            faces: vec![vec![0, 1, 2], vec![0, 2, 1]],
            face_materials: Some(vec![Some("a".into())]),
        };
        assert_eq!(source_materials(&mesh), vec![None, None]);
        let mesh = Mesh {
            face_materials: Some(vec![Some("a".into()), Some("b".into())]),
            ..mesh
        };
        assert_eq!(
            source_materials(&mesh),
            vec![Some("a".to_string()), Some("b".to_string())]
        );

        // the file route's empty material (a mesh that lists none) is the base material
        let record = crate::vmap::mesh::MeshRecord {
            id: Some(3),
            origin: None,
            angles: None,
            scales: None,
            vertices: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            faces: vec![vec![0, 1, 2]],
            face_materials: vec![String::new()],
            materials: vec![],
            ancestors: Some(vec!["CMapWorld".to_string()]),
        };
        let mesh = super::super::mesh_read::mesh_of(&record);
        assert_eq!(mesh.face_materials, Some(vec![None]));
        assert_eq!(mesh.vertices, record.vertices);
        assert_eq!(mesh.faces, record.faces);

        // the POC's source strings and the file source's own note
        assert_eq!(GeometrySource::Live.label(), "live");
        assert_eq!(GeometrySource::File.label(), "file");
        assert_eq!(GeometrySource::FileAfterTooLarge.label(), "file");
        assert_eq!(GeometrySource::Live.note(), None);
        assert!(GeometrySource::File
            .note()
            .expect("file note")
            .contains("SAVED .vmap"));
    }

    #[test]
    fn check_result_refuses_what_the_bridge_would_drop() {
        let tri = vec![[0.0, 0.0, 0.0], [64.0, 0.0, 0.0], [0.0, 64.0, 0.0]];
        assert!(check_result(&tri, &[vec![0, 1, 2]]).is_ok());
        // a flattened face (a drag that puts a vertex on the others' line)
        let flat = vec![[0.0, 0.0, 0.0], [64.0, 0.0, 0.0], [128.0, 0.0, 0.0]];
        let err = check_result(&flat, &[vec![0, 1, 2]]).unwrap_err();
        assert_eq!(err.name, "InvalidParams");
        assert!(err.message.contains("zero-area"), "{err:?}");
        // a coordinate the bridge refuses
        let far = vec![[1e6, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        assert!(check_result(&far, &[vec![0, 1, 2]])
            .unwrap_err()
            .message
            .contains("outside"));
        // counts the bridge cannot take (an extrude or a file mesh can reach them)
        assert!(check_result(&tri[..2], &[vec![0, 1, 2]]).is_err());
        assert!(check_result(&tri, &[]).is_err());
    }
}
