//! `set_trigger_volume` — replace a live brush entity's volume mesh (`mesh.setGeometry` on one of
//! its tied `CMapMesh` children, in WORLD coordinates). Plan `target/plan-triggers.md` §1
//! (`set_trigger_volume` + §5 wire contract), work package WP-V (`target/WP.md`).
//!
//! A trigger is a brush entity: a `CMapEntity` with tied mesh children (the trigger tools' module
//! doc in `triggers.rs`). `create_trigger`/`tie_to_entity` build one; this tool RESIZES one: the
//! caller names the entity (`nodeID`), the tool reads its children (`entity.get`, m2.19) to find
//! the volume mesh, and one `mesh.setGeometry` puts the caller's `box`/`geometry` there. The tied
//! mesh takes world coordinates and keeps its parent — the POC's proven path, verified live
//! (re-triggers.md Q2: "`mesh.setGeometry` on the tied mesh: works unchanged, mesh stays a child;
//! pivot re-centred to the new bounds centre, entity origin NOT updated — harmless. World
//! coordinates, as the POC said").
//!
//! **Mesh selection** (`entity.get` -> `children`, `[{nodeID, class}]` rows, only `class:"mesh"`
//! counts): none -> `NotABrushEntity`; exactly one -> it is the volume; several ->
//! `MultipleMeshes {meshes}` unless `meshNodeID` names one of them (a `meshNodeID` that is not
//! among the children is refused — the write would silently reshape the wrong mesh). A reply with
//! **no readable `children`** (the field absent — an older bridge — or the bridge's unreadable
//! `null`) cannot enumerate the entity at all: `BridgeTooOld` naming m2.19, unless `meshNodeID`
//! was given — then the read is not needed (the plan: "m2.18 suffices for set_trigger_volume ONLY
//! IF the caller passes `meshNodeID`; without `children` it is `BridgeTooOld`"), and the
//! bridge's own `mesh.setGeometry` target gates (`MeshNotFound`/`NotAMesh`) still answer.
//! A **truncated** children list (the bridge caps it at 1024 rows, `childrenTruncated`) cannot
//! prove that an unnamed `meshNodeID` is absent: the selector is then sent anyway, with a note
//! saying it went out unvalidated (review-rust.md B2) — refusing it would reject a real child
//! that merely fell off the list.
//!
//! **Material**: `mesh.setGeometry` re-initialises every face with the base material it is sent
//! (the bridge's `MeshInitFromPolygonGroup`; `material` is a required wire field), so the tool
//! must decide one. The argument wins (`materialSource:"argument"`). Without it the mesh's OWN
//! material is kept, resolved exactly like the rebuild operators resolve their geometry
//! (`mesh_live::current_mesh`): the live `mesh.get` read first (`"live"`), else the SAVED
//! `.vmap` (`"file"`, `vmap::mesh::mesh_by_id` — its per-face materials do not depend on the
//! node's transform, so the operators' scaled-node / unproven-parent gates do not apply here) —
//! and the file route only while the live document is clean: a dirty, dirty-unknown or
//! MCP-edited document is refused `-32016 UnsavedChanges` with the `save_map` hint, exactly the
//! rebuild operators' gate (an explicit `material` needs no source and skips the gate, like a live
//! read does for the operators).
//!
//! **The trigger fallback (review-rust.md B1)**: the clean-document gate exists to protect
//! GEOMETRY read from a possibly stale file; here only a material string is at stake, and for a
//! trigger class that string is invisible in game. So for a class whose `classname` (from the
//! `entity.get` reply) is a trigger class (`fgd::Database::is_trigger_class`) every file-route
//! refusal — the dirty/unsaved gate, an untitled document, an unmapped path, a mesh the clean
//! file does not hold, and a saved mesh that lists no material — falls back to the class's FGD
//! `auto_apply_material` (else `materials/tools/toolstrigger.vmat`) with
//! `materialSource:"fgd-default (mesh material unknown)"` and a note saying what was refused;
//! this is the "create, undo, resize" flow (`create_trigger` -> `undo_last` -> resize) for a bridge
//! without `mesh.get`, where the pure gate refused with a "save the half-undone map" hint. A **non-trigger**
//! solid class (`func_brush`, `func_buyzone`, … — the material shows) keeps every refusal, and a
//! clean file that holds the mesh unchanged still wins over the fallback. A saved mesh that lists
//! no material and is not a trigger class gets `materialSource:"default (the saved mesh lists no
//! material)"` — never a silent guess at a material.
//!
//! The geometry itself is fully caller-supplied, so the volume forms are the argument matrix of
//! `create_trigger`'s `box`/`geometry` (built and face-checked locally, nothing sent on a bad
//! one); `meshNodeIDs` is NOT a volume form here — the mesh always exists, `meshNodeID` only
//! selects among the entity's children.
//!
//! The write is `mesh_live::send_geometry`'s exact path (`pub(crate)` since WP-V, visibility
//! only): same local gates, request limits, `expectDoc` from this call's own `doc.active` read,
//! unsaved counter, undo label (`"MCP: set mesh geometry"`) and maybe-executed taxonomy. The result is `set_mesh_geometry`'s shape plus `entityNodeID` (the
//! caller's id — the result's `nodeID` is the MESH's) and `material`/`materialSource`.
use crate::state::HammerState;
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};

use super::entities_live::FgdSource;
use super::lifecycle::{dirty_state, is_untitled};
use super::live_common::{
    invalid, node_id_arg, reject_unknown_keys, require_document, MAX_NODE_ID,
};
use super::mesh_live::{send_geometry, Payload};
use super::trigger_args::{material_arg, reason_of, volume_arg, TRIGGER_MATERIAL};

/// `materialSource` when a trigger class's FGD material stands in for a mesh material this server
/// could not read (review-rust.md B1): the mesh's own material is unknown, the class's is used.
const FGD_DEFAULT_SOURCE: &str = "fgd-default (mesh material unknown)";
/// `undoNote` per method: what `undo_last` does with the group the bridge opened.
const UNDO_NOTE_GEOMETRY: &str = "undo_last restores the previous geometry";

/// The argument names this tool accepts (the handler is the `additionalProperties:false` gate).
const ARGS: [&str; 5] = ["nodeID", "box", "geometry", "material", "meshNodeID"];

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "set_trigger_volume",
            "Replace a LIVE brush entity's volume with a new `box` or explicit `geometry` (WORLD \
             coordinates), rebuilding one of its tied meshes in place. Several tied meshes: name one \
             with `meshNodeID`. Material defaults to the mesh's own, else the trigger default. One \
             undo group; never saves.",
            Mutating,
            &["entity.get", "mesh.setGeometry"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "nodeID": {
                    "type": "integer", "minimum": 0, "maximum": MAX_NODE_ID,
                    "description": "brush entity node id (list_triggers / get_entity)"
                },
                "box": {
                    "type": "object",
                    "description": "axis-aligned box; exactly one of box, geometry",
                    "properties": {
                        "min": {
                            "type": "array", "items": {"type": "number"},
                            "minItems": 3, "maxItems": 3,
                            "description": "low corner [x, y, z], world space"
                        },
                        "max": {
                            "type": "array", "items": {"type": "number"},
                            "minItems": 3, "maxItems": 3,
                            "description": "high corner; each side at least 1 unit"
                        }
                    },
                    "required": ["min", "max"],
                    "additionalProperties": false
                },
                "geometry": {
                    "type": "object",
                    "description": "WORLD-space mesh; faces counter-clockwise seen from the visible side",
                    "properties": {
                        "vertices": {
                            "type": "array", "minItems": 3,
                            "items": {
                                "type": "array", "items": {"type": "number"},
                                "minItems": 3, "maxItems": 3
                            },
                            "description": "world-space [x, y, z] points"
                        },
                        "faces": {
                            "type": "array", "minItems": 1,
                            "items": {
                                "type": "array", "items": {"type": "integer", "minimum": 0},
                                "minItems": 3
                            },
                            "description": "vertex-index loops"
                        }
                    },
                    "required": ["vertices", "faces"],
                    "additionalProperties": false
                },
                "material": {
                    "type": "string",
                    "description": "default: the mesh's own material, else the trigger default"
                },
                "meshNodeID": {
                    "type": "integer", "minimum": 0, "maximum": MAX_NODE_ID,
                    "description": "which tied mesh to rebuild; required if the entity has several"
                }
            },
            "required": ["nodeID"],
            "additionalProperties": false
        })),
        set_trigger_volume,
    );
}

// -------------------------------------------------------------------------------------------
// Arguments
// -------------------------------------------------------------------------------------------

// `material_arg` and the volume forms (`volume_arg` = exactly one of `box` | `geometry`, built,
// rounded and face-checked) are the shared `trigger_args` module (review-rust.md D1) — the same
// rules and messages `create_trigger`'s volume arguments use; this module's `meshNodeID` selector
// is its own (`mesh_selector` below).

/// `meshNodeID?` (the child selector): an integer in `0..=`[`MAX_NODE_ID`], or absent.
fn mesh_selector(args: &Value) -> Result<Option<i64>, ToolError> {
    match args.get("meshNodeID").filter(|value| !value.is_null()) {
        None => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .filter(|id| (0..=MAX_NODE_ID).contains(id))
            .map(Some)
            .ok_or_else(|| {
                invalid(format!(
                    "meshNodeID must be an integer in 0..={MAX_NODE_ID}, got {n}"
                ))
            }),
        Some(other) => Err(invalid(format!(
            "meshNodeID must be an integer in 0..={MAX_NODE_ID}, got {other}"
        ))),
    }
}

// -------------------------------------------------------------------------------------------
// The tool
// -------------------------------------------------------------------------------------------

/// `set_trigger_volume {nodeID, box|geometry, material?, meshNodeID?}`: read the entity's
/// children, resolve the material, then one `mesh.setGeometry` through `send_geometry`.
pub async fn set_trigger_volume(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(&args, &ARGS)?;
    let node_id = node_id_arg(&args)?;
    let (vertices, faces) = volume_arg(&args)?;
    let material = material_arg(&args, "material")?;
    let selector = mesh_selector(&args)?;

    let deadline = ctx.budget.deadline_ms()?;
    // the one document read of this call: it carries the write's `expectDoc` (review-2 L2) and is
    // what the file route's clean gate reads.
    let doc = require_document(&state, deadline).await?;

    // which mesh is the volume (and the entity's classname, for the material fallback)
    let (mesh_id, classname, mut notes) = resolve_mesh(&state, deadline, node_id, selector).await?;

    // the material the rebuild unifies on
    let (material, material_source, mut material_notes) = resolve_material(
        &state,
        deadline,
        &doc,
        mesh_id,
        classname.as_deref(),
        material,
    )
    .await?;
    notes.append(&mut material_notes);

    let mut out = send_geometry(
        &state,
        &ctx,
        "mesh.setGeometry",
        Some(mesh_id),
        Payload {
            vertices,
            faces,
            face_materials: None, // one material for every face of the new volume
        },
        &material,
        UNDO_NOTE_GEOMETRY,
        Some(doc),
    )
    .await?;
    out.insert("entityNodeID".into(), json!(node_id));
    out.insert("material".into(), json!(material));
    out.insert("materialSource".into(), json!(material_source));
    for note in notes {
        push_note(&mut out, &note);
    }
    Ok(Value::Object(out).into())
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

// The `reason` the bridge put into an error's data (`""` when it sent none) is the shared
// `trigger_args::reason_of` (D1).

// -------------------------------------------------------------------------------------------
// Mesh selection (entity.get -> children)
// -------------------------------------------------------------------------------------------

/// One `entity.get` on the brush entity: the reply decides which mesh the volume is (and names the
/// entity's class, which the trigger material fallback resolves against the FGD). Returns the
/// mesh's node id, the reply's `classname` (when it carried one) plus optional notes (the
/// unvalidated-selector cases: a bridge without `children`, a truncated children list). The read
/// carries no `expectDoc` (it is a read, like every other `entity.get`).
async fn resolve_mesh(
    state: &HammerState,
    deadline: u64,
    node_id: i64,
    selector: Option<i64>,
) -> Result<(i64, Option<String>, Vec<String>), ToolError> {
    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "entity.get",
                json!({"nodeID": node_id}),
                RequestOpts::deadline(deadline),
            )
            .await
    };
    let result = match reply {
        Ok(reply) => reply.result,
        Err(error) => {
            // a mesh / world / group node named here is the same refusal as zero mesh children:
            // the caller was asked for a brush entity and named something else
            if error.code == Some(-32602) && reason_of(&error).contains("not an entity") {
                return Err(not_a_brush_entity(
                    node_id,
                    "nodeID is not an entity (a mesh, the world, a group...): set_trigger_volume \
                     replaces a brush entity's volume",
                ));
            }
            return Err(ToolError::from(error));
        }
    };
    let truncated = result
        .get("childrenTruncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let classname = result
        .get("classname")
        .and_then(Value::as_str)
        .map(str::to_string);
    let children: Option<Vec<i64>> = match result.get("children") {
        Some(Value::Array(rows)) => Some(
            rows.iter()
                .filter(|row| row.get("class").and_then(Value::as_str) == Some("mesh"))
                .filter_map(|row| row.get("nodeID").and_then(Value::as_i64))
                .collect(),
        ),
        // absent (an older bridge) or the spec's unreadable-array `null`: not enumerable
        _ => None,
    };
    let children = match children {
        Some(children) => children,
        None => {
            let Some(selector) = selector else {
                return Err(bridge_too_old(node_id));
            };
            return Ok((
                selector,
                classname,
                vec![
                    "the entity.get reply carried no readable `children` (an older bridge, or \
                     the bridge could not read the child array): the caller's `meshNodeID` \
                     selects the volume unvalidated — the bridge's own mesh.setGeometry target \
                     gate answers if it is wrong"
                        .to_string(),
                ],
            ));
        }
    };
    if children.is_empty() {
        return Err(not_a_brush_entity(
            node_id,
            "the entity has no tied mesh children (a point entity, not a brush entity)",
        ));
    }
    if let Some(selector) = selector {
        if children.contains(&selector) {
            return Ok((selector, classname, Vec::new()));
        }
        // a truncated list cannot prove the selector absent: send it anyway, with the note
        if truncated {
            return Ok((
                selector,
                classname,
                vec![format!(
                    "the entity.get children list of nodeID {node_id} was TRUNCATED by the \
                     bridge ({} mesh rows arrived), so `meshNodeID` {selector} could not be \
                     checked against it: the selector went out unvalidated — if it is not one of \
                     the entity's meshes, the write reshapes a mesh this entity does not hold \
                     (the bridge's own mesh.setGeometry target gate only checks that the mesh \
                     exists)",
                    children.len()
                )],
            ));
        }
        return Err(not_a_child(node_id, selector, &children));
    }
    if children.len() > 1 {
        return Err(multiple_meshes(node_id, &children, truncated));
    }
    Ok((children[0], classname, Vec::new()))
}

/// `NotABrushEntity` (the same name `move_to_world`'s bridge refusal maps to): the caller named
/// something that has no tied mesh volume. Retry-safe, no write was sent (the `entity.get` read
/// that found this out changed nothing).
fn not_a_brush_entity(node_id: i64, why: &str) -> ToolError {
    ToolError::new(
        "NotABrushEntity",
        "NotABrushEntity",
        format!(
            "nodeID {node_id}: {why}. set_trigger_volume replaces the volume mesh of a brush \
             entity; no write was sent (the document is unchanged)"
        ),
    )
    .with_code(-32602)
    .with_data(json!({"nodeID": node_id}))
    .retry_safe(true)
}

/// `MultipleMeshes {meshes}`: the entity carries several volume meshes and no `meshNodeID` named
/// one; a blind rebuild would reshape the wrong one. No write was sent.
fn multiple_meshes(node_id: i64, meshes: &[i64], truncated: bool) -> ToolError {
    let list = meshes
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    ToolError::new(
        "MultipleMeshes",
        "MultipleMeshes",
        format!(
            "nodeID {node_id} has {} tied mesh children ({list}): one call replaces ONE volume — \
             name the mesh with `meshNodeID` (get_entity / list_triggers show the children), \
             or untie the extras with move_to_world first.{} No write was sent (the document is \
             unchanged).",
            meshes.len(),
            if truncated {
                " (the bridge's children list was truncated, so more children may exist)"
            } else {
                ""
            }
        ),
    )
    .with_code(-32602)
    .with_data(json!({"nodeID": node_id, "meshes": meshes, "truncated": truncated}))
    .retry_safe(true)
}

/// The `meshNodeID` selector is not one of the entity's mesh children (and the list was not
/// truncated, so that is proven): the write would reshape a mesh that is not this entity's volume
/// (or does not exist). No write was sent.
fn not_a_child(node_id: i64, selector: i64, meshes: &[i64]) -> ToolError {
    let list = meshes
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    ToolError::new(
        "MeshNotAChild",
        "MeshNotAChild",
        format!(
            "meshNodeID {selector} is not one of nodeID {node_id}'s mesh children ({list}): the \
             volume must be rebuilt on a mesh the entity actually holds; no write was sent (the \
             document is unchanged)"
        ),
    )
    .with_code(-32602)
    .with_data(json!({
        "nodeID": node_id,
        "meshNodeID": selector,
        "meshes": meshes,
    }))
    .retry_safe(true)
}

/// The reply had no readable `children`: an older bridge cannot be enumerated for the volume
/// (`BridgeTooOld`, the props tools' name for a too-old build), unless the caller named the mesh
/// themselves. Retry-safe, no write was sent.
fn bridge_too_old(node_id: i64) -> ToolError {
    ToolError::new(
        "BridgeTooOld",
        "BridgeTooOld",
        format!(
            "the entity.get reply for nodeID {node_id} carried no readable `children` list \
             (absent, or the bridge reported the child array unreadable): this bridge predates \
             m2.19 (entity.get children) and set_trigger_volume cannot enumerate a brush \
             entity's meshes. Pass `meshNodeID` (the mesh's live id from list_meshes / \
             get_entity), or update the bridge to m2.19 (no write was sent)"
        ),
    )
    .with_code(-32011)
    .with_data(json!({
        "nodeID": node_id,
        "missing": ["entity.get children"],
        "blockedOn": "bridge m2.19 (entity.get children)",
    }))
    .retry_safe(true)
}

// -------------------------------------------------------------------------------------------
// Material resolution (argument > live mesh.get > SAVED .vmap, the rebuild operators' rules)
// -------------------------------------------------------------------------------------------

/// The material the rebuild unifies on and where it came from. `FgdDefault` is the trigger-class
/// fallback; `Default` carries the reason a non-trigger class's mesh material was not used (the
/// result's `materialSource` spells it out and a note says it — never a silent guess).
enum MaterialSource {
    Argument,
    Live,
    File,
    FgdDefault,
    Default(&'static str),
}

impl MaterialSource {
    fn label(self) -> String {
        match self {
            MaterialSource::Argument => "argument".to_string(),
            MaterialSource::Live => "live".to_string(),
            MaterialSource::File => "file".to_string(),
            MaterialSource::FgdDefault => FGD_DEFAULT_SOURCE.to_string(),
            MaterialSource::Default(why) => format!("default ({why})"),
        }
    }
}

/// `material?` -> the base material the `mesh.setGeometry` request carries and the volume's faces
/// all take. The argument wins; without it the mesh's own material is kept from the live
/// `mesh.get` read, else the SAVED `.vmap` — under the rebuild operators' rules, which gate the
/// file route on a clean document ([`source_unsaved_changes`]) and refuse
/// an untitled document, an unmapped path and a mesh the file does not hold instead of guessing.
/// For a **trigger** class (`classname`, from the `entity.get` reply: `is_trigger_class`) those
/// refusals — and a clean file whose mesh lists no material — fall back to the class's FGD
/// `auto_apply_material` (else [`TRIGGER_MATERIAL`]) with [`FGD_DEFAULT_SOURCE`] and a note
/// (review B1: the "create, undo, resize" sequence). A non-trigger class keeps every refusal and
/// falls to [`TRIGGER_MATERIAL`] only for a clean file whose mesh lists no material
/// (`materialSource:"default …"`) — an explicit, stated default, not a silent one.
async fn resolve_material(
    state: &HammerState,
    deadline: u64,
    doc: &Value,
    mesh_id: i64,
    classname: Option<&str>,
    given: Option<String>,
) -> Result<(String, String, Vec<String>), ToolError> {
    let mut notes: Vec<String> = Vec::new();
    if let Some(material) = given {
        return Ok((material, MaterialSource::Argument.label(), notes));
    }
    // the class's own material, for a trigger class: the fallback wherever the mesh's own material
    // cannot be read (a trigger's material is invisible in game — see the module doc)
    let fallback = trigger_fallback(state, classname);
    // the live read first (a refusal of it, e.g. a node the document no longer has, is handled
    // like a file refusal below); a bridge without `mesh.get` answers `None` and the file decides
    let found = match crate::tools::mesh_read::read_live_mesh(state, deadline, doc, mesh_id).await {
        Ok(Some(record)) => Ok(record
            .face_materials
            .iter()
            .find(|material| !material.is_empty())
            .map(|material| {
                (
                    material.clone(),
                    vec![format!(
                        "the volume's material: the mesh's own '{material}' was kept (read live \
                         from the document); the rebuilt volume's faces all take it (the \
                         mesh.setGeometry rebuild does not map the old per-face split onto the \
                         new volume)"
                    )],
                    MaterialSource::Live,
                )
            })),
        Ok(None) => saved_material(state, doc, mesh_id)
            .map(|found| found.map(|(material, said)| (material, said, MaterialSource::File))),
        Err(error) => Err(error),
    };
    match found {
        Ok(Some((material, mut said, source))) => {
            notes.append(&mut said);
            Ok((material, source.label(), notes))
        }
        // a clean file that holds the mesh but lists no material of its own
        Ok(None) => {
            let (material, source) = match fallback {
                Some((material, origin)) => {
                    notes.push(format!(
                        "the volume's material: the saved mesh lists no material of its own, so \
                         the class's {origin} '{material}' is sent ({FGD_DEFAULT_SOURCE})"
                    ));
                    (material, MaterialSource::FgdDefault.label())
                }
                None => {
                    notes.push(format!(
                        "the volume's material: the saved mesh lists no material of its own, so \
                         the trigger default '{TRIGGER_MATERIAL}' is sent (stated here, not \
                         guessed silently)"
                    ));
                    (
                        TRIGGER_MATERIAL.to_string(),
                        MaterialSource::Default("the saved mesh lists no material").label(),
                    )
                }
            };
            Ok((material, source, notes))
        }
        // the file route refused: a trigger class takes its FGD material instead, anything else
        // keeps the refusal (the material shows there)
        Err(error) => match fallback {
            Some((material, origin)) => {
                notes.push(format!(
                    "the mesh's own material is unknown (reading the SAVED .vmap was refused: {} \
                     — {}), so the class's {origin} '{material}' is sent for the rebuilt volume \
                     ({FGD_DEFAULT_SOURCE}); a trigger's material is invisible in game, so the \
                     rebuild is not worth refusing over it",
                    error.name,
                    short_reason(&error),
                ));
                Ok((material, MaterialSource::FgdDefault.label(), notes))
            }
            None => Err(error),
        },
    }
}

/// The FGD material a **trigger** class falls back to when the mesh's own material cannot be
/// read: the class chain's `metadata` `auto_apply_material`, else [`TRIGGER_MATERIAL`]. `None`
/// for a non-trigger class (every refusal stands, the material shows), for a class the FGD does
/// not carry, for a reply without a `classname`, and when the FGD is unavailable — the caller
/// then has no better material than the mesh's own. The second element names where the material
/// came from, for the note.
fn trigger_fallback(
    state: &HammerState,
    classname: Option<&str>,
) -> Option<(String, &'static str)> {
    let classname = classname?;
    let fgd = FgdSource::load(state);
    let db = fgd.db()?;
    if !db.is_trigger_class(classname) {
        return None;
    }
    Some(match db.auto_apply_material(classname) {
        Some(material) => (material, "FGD auto_apply_material"),
        None => (TRIGGER_MATERIAL.to_string(), "trigger default material"),
    })
}

/// The mesh's material from the SAVED `.vmap`, under the rebuild operators' rules: the file is
/// read only while the live document is clean (`-32016 UnsavedChanges`), an untitled document and
/// an unmapped path are their own refusals, and a mesh the file does not hold is `MeshNotFound`.
/// `Ok(None)` is the clean file holding the mesh with no material listed (the caller states a
/// default). Every refusal is returned as-is: the trigger fallback decides whether it stands.
fn saved_material(
    state: &HammerState,
    doc: &Value,
    mesh_id: i64,
) -> Result<Option<(String, Vec<String>)>, ToolError> {
    let unsaved = state.unsaved_edits();
    let dirty = dirty_state(doc);
    if dirty != Some(false) || unsaved > 0 {
        return Err(source_unsaved_changes(doc, mesh_id, dirty, unsaved));
    }
    if is_untitled(doc) {
        return Err(material_source_unavailable(
            mesh_id,
            "untitled",
            Value::Null,
            "nodeID {NODE} has no material this server knows (this bridge does not advertise \
             mesh.get) and the active document is untitled, so \
             there is no SAVED .vmap to read them from: run save_map first, or pass the \
             `material` argument explicitly. No write was sent (the document is unchanged).",
        ));
    }
    let windows = doc.get("path").and_then(Value::as_str).unwrap_or_default();
    let Some(host) = state.path_map.windows_to_host(windows) else {
        return Err(path_not_mapped(windows, mesh_id));
    };
    let document = crate::vmap::load(&host)
        .map_err(|error| super::entities_file::vmap_error(error, Some(windows)))?;
    let Some(record) = document.mesh_by_id(mesh_id) else {
        return Err(ToolError::new(
            "MeshNotFound",
            "MeshNotFound",
            format!(
                "no mesh with nodeID {mesh_id} in the saved .vmap; save_map first if it was \
                 created since the last save, or pass the `material` argument explicitly"
            ),
        )
        .with_data(json!({"nodeID": mesh_id, "path": windows, "geometrySource": "file"}))
        .retry_safe(true));
    };
    match record.face_materials.iter().find(|m| !m.is_empty()) {
        Some(material) => Ok(Some((
            material.clone(),
            vec![format!(
                "the volume's material: the mesh's own '{material}' was kept (read from the \
                 SAVED .vmap, the live document is clean); the rebuilt volume's faces all take \
                 it"
            )],
        ))),
        None => Ok(None),
    }
}

/// Why a file-route refusal happened, for the trigger fallback's note: the `reason` the bridge /
/// the tool put into the error's data when there is one (`"dirty document"`, `"untitled"`, …),
/// else the first sentence of the message (MeshNotFound, PathNotMapped start with it).
fn short_reason(error: &ToolError) -> String {
    if let Some(reason) = error.data.get("reason").and_then(Value::as_str) {
        return reason.to_string();
    }
    let first = error.message.split(". ").next().unwrap_or(&error.message);
    first.trim_end_matches('.').to_string()
}

/// The dirty/unknown refusal (`-32016 UnsavedChanges`, the code the rebuild operators use): the
/// mesh's material would have to come from the SAVED file, and the file does not hold what the
/// live document holds. Retry-safe, nothing sent.
fn source_unsaved_changes(
    doc: &Value,
    mesh_id: i64,
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
            "the material of nodeID {mesh_id} would have to come from the SAVED .vmap ({}), and \
             that file does not hold what the live document holds: run save_map first, or pass \
             the `material` argument explicitly (the volume is then rebuilt without reading the \
             file){hint}. No write was sent (the document is unchanged).",
            causes.join(" and ")
        ),
    )
    .with_code(-32016)
    .with_data(json!({
        "nodeID": mesh_id,
        "geometrySource": "unavailable",
        "reason": reason,
        "path": doc.get("path").cloned().unwrap_or(Value::Null),
        "unsavedBridgeEdits": unsaved,
    }))
    .retry_safe(true)
}

/// The `MeshGeometryUnavailable` family (the untitled case; the other refusals carry their own
/// names): the material source is out, no write was sent.
fn material_source_unavailable(mesh_id: i64, reason: &str, path: Value, message: &str) -> ToolError {
    ToolError::new(
        "MeshGeometryUnavailable",
        "MeshGeometryUnavailable",
        message.replace("{NODE}", &mesh_id.to_string()),
    )
    .with_data(json!({
        "nodeID": mesh_id,
        "geometrySource": "unavailable",
        "reason": reason,
        "path": path,
    }))
    .retry_safe(true)
}

/// The document's Windows path has no host mapping: the SAVED `.vmap` cannot be read (the POC's
/// `PathNotMapped`).
fn path_not_mapped(windows: &str, mesh_id: i64) -> ToolError {
    ToolError::new(
        "PathNotMapped",
        "PathNotMapped",
        format!(
            "cannot map the Windows path {windows:?} to a host path, so the SAVED .vmap cannot \
             be read for the mesh's material: set CS2_MCP_PATH_MAP (e.g. \
             'z:\\app=<host path>'), or pass the `material` argument explicitly. No write was \
             sent (the document is unchanged)."
        ),
    )
    .with_code(-32602)
    .with_data(json!({
        "windowsPath": windows,
        "nodeID": mesh_id,
        "geometrySource": "unavailable",
    }))
    .retry_safe(true)
}
