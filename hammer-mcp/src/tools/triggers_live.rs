//! Live trigger mutators through the bridge: `create_trigger`, `tie_to_entity` (the m2.19
//! `entity.tieMeshes` tie) and `move_to_world` (m2.19 `entity.untie`). Plan
//! `target/plan-triggers.md` — §1 for the tool surface, **§5 for the FINAL m2.19 wire contract**
//! (`target/WP.md` WP-T2) — and `target/re-triggers.md` for the RE evidence.
//!
//! A trigger is a **brush entity**: a `CMapEntity` whose FGD class is a `@SolidClass` with tied
//! `CMapMesh` children (the volume — normally `materials/tools/toolstrigger.vmat`, the class's own
//! FGD `auto_apply_material`). Nothing else marks one: the FGD kind ([`ClassKind::Solid`]) plus
//! the mesh children is the definition, and the class must declare the `output`s the `connections`
//! fire (`OnStartTouch`, `OnTrigger`, ...).
//!
//! **`create_trigger {class? = trigger_multiple, exactly one of box|geometry|meshNodeIDs, ...}`**:
//! everything local (the class kind, the spawnflags, the keyvalues, the volume's geometry, the
//! connection FGD checks) runs before the first write, then these counted writes run under **one**
//! write gate:
//!
//! 1. (a) `mesh.create` of the volume (`box`/`geometry` forms only; the material is the argument,
//!    else the class's FGD `auto_apply_material`, else `materials/tools/toolstrigger.vmat` for a
//!    trigger class / the `create_mesh` default for any other solid class — the result's
//!    `materialSource` says which). The volume goes through `mesh_live::send_geometry`, the one
//!    path `create_mesh` uses (A1): the request-size budget, the coordinate/face gates and the M1
//!    face-count check are its, and the label the result reports is the bridge's own `undoLabel`
//!    (A6),
//! 2. (b) `entity.tieMeshes {class, nodeIDs, keyValues, allowRetie?, expectDoc}` (spawnflags/
//!    targetname/keyValues as text), which creates the entity **and** ties the meshes,
//! 3. (b2) `mesh.setMaterial` per named mesh when `retexture:true` (existing meshes only — a
//!    created mesh always carries the material already),
//! 4. (c) `entity.addConnections` on the new node.
//!
//! The result reports `undoGroups`/`undoLabels` (each step is its own Hammer group; a
//! one-group bridge composite is a later optimisation, plan §4) and the notes say
//! `undo_last <n> times reverts this call`. A failure **after step (a)** is a `PartialFailure`
//! naming the mesh nodeID this call created, `retrySafe:false` (a retry would create a second
//! volume): the tool cannot vouch for the document state a
//! half-finished sequence left behind.
//!
//! The **connection pre-flight** is `entity_io::connection_run` in its
//! [`entity_io::ConnectionSource::Class`] form: the class the caller is about to create stands in
//! for the source entity, so the FGD output/input checks and the live target resolution (read-only
//! `entity.find` calls) run **before the mesh exists** — an unknown output refuses the call with
//! nothing sent, not even the mesh. The write step re-runs the same core against the new nodeID
//! with the same parsed list.
//!
//! **`tie_to_entity {class? | entityID?, meshNodeIDs, targetname?, spawnflags?, keyValues?,
//! material?, allowRetie?}`** is the bare primitive `create_trigger`'s step (b) is built on, for
//! meshes that already exist. Exactly one of `class` (create the brush entity) and `entityID`
//! (tie into an existing brush entity of the live document — §5) is required; with `entityID` the
//! `class`/`targetname`/`spawnflags`/`keyValues` arguments are `InvalidArguments` (the entity
//! already exists), and `material` (when given) retextures every tied mesh (`mesh.setMaterial` per
//! mesh, one group each). `allowRetie:true` moves a mesh that already belongs to another brush
//! entity and deletes the emptied one in the tie's own group (`removedEmptyEntities`).
//!
//! **`move_to_world {nodeID, deleteEntity?}`** is `entity.untie`: the tied meshes go back under
//! the entity's PARENT (the world, or the group/prefab the entity itself sits in), the entity is
//! deleted, one `"Untie from entity"` group (Hammer's own label). A mesh already under the target
//! entity is skipped by the tie (`alreadyTied`); `deleteEntity:false` is refused: Hammer's own
//! move-to-world removes the emptied entity (plan §1/§5) and this tool does not invent a second
//! recipe.
//!
//! Bridge refusals pass through in this module's vocabulary: `-32022` -> `NodeNotFound` (naming
//! the `data.nodeID` mesh the bridge named, else the `entityID` of an `entityID`-form tie, else the
//! ids that were sent), `-32602 "not a mesh"` -> `NotAMesh {nodeID}`, `-32602 "already tied"` ->
//! `MeshAlreadyTied {nodeID, parentNodeID}`, `-32602 "not a brush entity"` -> `NotABrushEntity`,
//! `-32602 "not a solid (brush) class"` -> `NotASolidClass` counted with the undo group the bridge
//! opened and left behind (`data.undoGroupOpened`); everything else keeps the wire taxonomy
//! (`mutator_failure`, so a maybe-executed `-32603` sets `outcomeUnknown`). Every one of them was
//! decided before the bridge's `undo_open`, so the wording says the bridge refused the request (not
//! that nothing was sent).
//!
//! The tie's `keyValues` echo is read (A4): `keyValues.applied`/`warnings` become the result's
//! `applied` map plus one warning per key the bridge did not apply, and a reply with no echo at all
//! is reported as "not verified" rather than as success. An all-`alreadyTied` tie is a success
//! no-op: the bridge opens no group (`undoLabel:null`/`noop:true`), so it contributes no
//! `undoGroups` entry, counts no unsaved edit and carries the "nothing to do" note (A8).
//!
//! **Capability gate**: the three tools declare their m2.19 marker (`entity.tieMeshes` /
//! `entity.untie`), so a bridge that does not advertise them (m2.18 and older) answers
//! `NotImplemented` with `data.missing` **before anything is sent** — the registry's own gate
//! (`cs2_mcp_common::tools::require_caps`), like every other tool here. The *optional* methods
//! are gated **per call** instead ([`require_call_caps`]): `create_trigger` does not need
//! `entity.addConnections`/`entity.find`/`entity.get` without `connections`, nor `mesh.create`
//! without a `box`/`geometry` volume, nor `mesh.setMaterial` without `retexture` on existing
//! meshes — so a bridge missing one of them still serves the calls that do not use it (the
//! registry can only declare what *every* call needs).
//!
//! **FGD**: the class kind, `is_trigger_class`, the spawnflags (`spawnflags_def` /
//! `default_spawnflags` / `encode_spawnflags` / `decode_spawnflags`) and `auto_apply_material` all
//! come from WP-F's `crate::fgd::Database` API (plan §2). `tie_to_entity`'s `entityID` form needs
//! none of it (there is no class to resolve), so it works without an FGD.
use crate::fgd::{self, ClassKind};
use crate::geometry::Vec3;
use crate::state::{DocState, HammerState};
use crate::tools::entities_live::FgdSource;
use crate::tools::entity_io::{Connection, ConnectionRun, ConnectionSource};
use crate::tools::trigger_args::{
    box_volume, geometry_volume, material_arg, reason_of, TRIGGER_MATERIAL,
};
use cs2_mcp_common::bridge::{BridgeError, RequestOpts};
use cs2_mcp_common::tools::{
    require_caps, CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Map, Value};

/// The live plumbing shared with `entity_io`/`mesh_live` (`super::live_common`): the document read
/// with its `expectDoc` fingerprint, the unsaved-edit counter and
/// the maybe-executed taxonomy.
pub(crate) use super::live_common::{
    bool_arg, doc_id_of, doc_state, expect_doc, invalid, mutator_failure, node_id_arg, note_edit,
    reject_unknown_keys, require_document, MAX_NODE_ID,
};

/// The default class (plan §1's `trigger_multiple`).
const DEFAULT_CLASS: &str = "trigger_multiple";
/// One `meshNodeIDs` list carries at most this many meshes (the m2.19 spec's `1..64`), the same
/// bound the bridge enforces.
const MAX_MESH_NODE_IDS: usize = 64;
/// The undo label the m2.19 tie opens (`entity.tieMeshes`' own label, §5); the fallback when a
/// reply does not carry one.
const TIE_LABEL: &str = "MCP: tie meshes to entity";
/// The undo label Hammer's own `MoveToWorld` opens (§5: `"Untie from entity"`, not the bridge's
/// own `"MCP: …"` spelling).
const UNTIE_LABEL: &str = "Untie from entity";
/// The `undoNote` `mesh_live::send_geometry` reports for step (a)'s volume write (A1): what
/// `undo_last` does with the group the volume created.
const UNDO_NOTE_CREATE: &str = "undo_last removes the created volume mesh";
/// A `targetname`/keyvalue text is at most this many bytes (the bound `entities_live` uses).
const MAX_VALUE_BYTES: usize = 511;
/// The no-save note every mutator carries.
const NO_SAVE_NOTE: &str = "no auto-save: run save_map to write the changes; file-backed reads \
                            (get_entity) stay stale until then";

/// The argument names each tool accepts (`additionalProperties:false` is the schema's contract;
/// the handler is the gate).
const CREATE_ARGS: [&str; 12] = [
    "class",
    "box",
    "geometry",
    "meshNodeIDs",
    "material",
    "retexture",
    "targetname",
    "spawnflags",
    "keyValues",
    "connections",
    "force",
    "allowRetie",
];
const TIE_ARGS: [&str; 8] = [
    "class",
    "entityID",
    "meshNodeIDs",
    "targetname",
    "spawnflags",
    "keyValues",
    "material",
    "allowRetie",
];
const UNTIE_ARGS: [&str; 2] = ["nodeID", "deleteEntity"];

/// What one multi-step call has done so far: the groups it opened (the result's `undoLabels`) and
/// what it created (what a later failure has to name).
struct Progress {
    labels: Vec<String>,
    mesh: Option<i64>,
    entity: Option<i64>,
    /// The document's state **before this call's first write** ([`DocState`]): the undo-depth
    /// baseline a maybe-executed failure is measured against (A5).
    start: Option<DocState>,
}

impl Progress {
    fn new(start: Option<DocState>) -> Progress {
        Progress {
            labels: Vec::new(),
            mesh: None,
            entity: None,
            start,
        }
    }

    /// One successful step's undo group: the bridge's own label, never invented here.
    fn push(&mut self, result: &Value, fallback: &str) {
        let label = result
            .get("undoLabel")
            .and_then(Value::as_str)
            .unwrap_or(fallback);
        self.labels.push(label.to_string());
    }

    /// A failure **after** the first write, as [`PartialFailure`]: the document holds what this
    /// call created, so it is not retry-safe and the caller must pop the groups it opened (or
    /// delete the nodes). Nothing of this call exists yet -> the bridge's own refusal stands.
    ///
    /// The unsaved-edit counter is untouched: each **successful** write counted itself, and the
    /// failed step wrote nothing (a maybe-executed one already counted through
    /// [`mutator_failure`]).
    ///
    /// A bridge refusal that names an opened group (`data.undoGroupOpened:true`, the in-group
    /// solid-class path FIX-B reports) is counted as one more group even when nothing of this call
    /// landed, and a maybe-executed failure ([`ToolError::outcome_unknown`]) cannot be counted from
    /// the labels alone: `post` is the document state read after the failure (A5) and its undo
    /// depth, when it is comparable, is what the revert instruction names.
    fn failure(
        &self,
        state: &HammerState,
        step: &str,
        error: ToolError,
        post: Option<DocState>,
    ) -> ToolError {
        let opened = error
            .data
            .get("undoGroupOpened")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if self.mesh.is_none() && self.entity.is_none() && !opened {
            return error;
        }
        let minimum = self.labels.len() + usize::from(opened);
        // A5: the labels count only the steps that returned OK; a maybe-executed write may have
        // opened its own group on top. The document's undo depth is the honest count then, and
        // when it moved by more than the labels the instruction says "at least" instead of
        // promising an exact number the caller can trust.
        let mut groups = minimum;
        let mut exact = !error.outcome_unknown;
        if error.outcome_unknown {
            if let (Some(start), Some(post)) = (self.start.as_ref(), post.as_ref()) {
                if start.doc_id == post.doc_id && post.depth >= start.depth {
                    let moved = usize::try_from(post.depth - start.depth).unwrap_or(minimum);
                    groups = moved.max(minimum);
                    exact = moved == minimum;
                }
            }
        }
        let mut created: Vec<i64> = Vec::new();
        if let Some(mesh) = self.mesh {
            created.push(mesh);
        }
        if let Some(entity) = self.entity {
            created.push(entity);
        }
        let counted = if exact {
            format!("{groups}")
        } else {
            format!("at least {minimum}")
        };
        let revert = if exact {
            match self.mesh {
                Some(mesh) => format!(
                    "undo_last {groups} time(s) reverts this call, or delete_nodes [{mesh}] for the \
                     created volume"
                ),
                None => format!("undo_last {groups} time(s) reverts this call"),
            }
        } else {
            format!(
                "the document's undo stack moved by {groups} group(s) (at least {minimum} of them \
                 are this call's; anything beyond that is a concurrent edit of the user's): read \
                 session_status (or `doc.active`'s undoName/undoDepth) and `undo_last \
                 {{acceptUserEdits:true}}` them before retrying"
            )
        };
        let what = if created.is_empty() {
            format!(
                "the call failed after {counted} undo group(s) had opened and left nothing of this \
                 call in the document"
            )
        } else {
            format!(
                "the trigger was only partly built: {step} failed after {counted} undo group(s) had \
                 opened (this call created {created:?})"
            )
        };
        let retry = if created.is_empty() {
            format!("retrying is safe, but the group(s) stay: {}", revert)
        } else {
            format!("{revert} — a retry of the whole call would create a second volume/entity")
        };
        let mut data = json!({
            "step": step,
            "meshNodeID": self.mesh,
            "nodeID": self.entity,
            "createdNodes": created,
            "undoGroups": groups,
            "undoLabels": self.labels,
            "undoGroupOpened": opened,
            "bridgeError": {
                "name": error.name,
                "code": error.code,
                "message": error.message,
                "data": Value::Object(error.data.clone()),
            },
            "unsavedBridgeEdits": state.unsaved_edits(),
        });
        if !exact || error.outcome_unknown {
            data["undoGroupsMinimum"] = json!(minimum);
        }
        let mut tool = ToolError::new(
            "PartialFailure",
            "PartialFailure",
            format!("{what}: {retry}. Bridge error: {}", error.message),
        )
        .with_code(-32012)
        .with_data(data);
        // nothing of this call exists: a retry cannot duplicate anything (it only leaves the
        // group(s) the bridge already opened behind), so the call is retry-safe
        tool.retry_safe = created.is_empty();
        tool.outcome_unknown = error.outcome_unknown;
        tool
    }
}

/// One step's failure as [`Progress::failure`] reports it, with the post-failure document state
/// (A5) read when the error may have landed: a maybe-executed write may have opened a group the
/// success labels do not know about, and the depth difference is what the revert instruction has
/// to name. The read is a fresh `doc.active` (compared against this call's own document id): a bridge that cannot answer leaves the instruction with
/// the label count as a minimum.
async fn partial_failure(
    state: &HammerState,
    progress: &Progress,
    deadline: u64,
    doc_id: Option<&str>,
    step: &str,
    error: ToolError,
) -> ToolError {
    let post = if error.outcome_unknown {
        super::live_common::read_document_after_write(state, deadline, doc_id).await
    } else {
        None
    };
    progress.failure(state, step, error, post)
}

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "create_trigger",
            "Create a trigger brush entity in the LIVE map from exactly one volume (`box`, `geometry` \
             or `meshNodeIDs`) and tie it to the new entity. Class must be an FGD @SolidClass. \
             `spawnflags` is the COMPLETE flag set (omit for FGD defaults). `connections` are checked \
             against the FGD before anything is sent. Several undo groups; a failure after the volume \
             exists is PartialFailure (not retry-safe). Never saves.",
            Mutating,
            &["entity.tieMeshes"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "class": {
                    "type": "string",
                    "default": DEFAULT_CLASS,
                    "description": "FGD @SolidClass"
                },
                "box": {
                    "type": "object",
                    "description": "axis-aligned box; exactly one of box, geometry, meshNodeIDs",
                    "properties": {
                        "min": {
                            "type": "array", "items": {"type": "number"},
                            "minItems": 3, "maxItems": 3,
                            "description": "low corner [x, y, z]"
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
                "meshNodeIDs": {
                    "type": "array", "minItems": 1, "maxItems": MAX_MESH_NODE_IDS,
                    "items": {"type": "integer", "minimum": 0, "maximum": MAX_NODE_ID},
                    "description": "existing live world meshes to tie"
                },
                "material": {
                    "type": "string",
                    "description": "default: the class's auto_apply_material, else toolstrigger"
                },
                "retexture": {
                    "type": "boolean",
                    "default": false,
                    "description": "also apply `material` to the meshes in `meshNodeIDs`"
                },
                "targetname": {
                    "type": "string"
                },
                "spawnflags": {
                    "type": "array",
                    "items": {"type": ["string", "integer"]},
                    "description": "COMPLETE set: labels or bit numbers; unlisted bits OFF; omit = FGD defaults"
                },
                "keyValues": {
                    "type": "object",
                    "description": "other FGD keyvalues; not classname/spawnflags/targetname/origin/angles/scales",
                    "additionalProperties": {"type": ["string", "number", "boolean"]}
                },
                "connections": {
                    "type": "array", "minItems": 1, "maxItems": 64,
                    "items": {
                        "type": "object",
                        "properties": {
                            "output": {"type": "string", "minLength": 1},
                            "target": {
                                "type": "string", "minLength": 1,
                                "description": "targetname or !activator/!self"
                            },
                            "input": {"type": "string", "minLength": 1},
                            "param": {"type": "string"},
                            "delay": {
                                "type": "number", "minimum": 0, "maximum": 3600
                            },
                            "timesToFire": {
                                "type": "integer", "minimum": -1,
                                "description": "-1 = every time"
                            }
                        },
                        "required": ["output", "target", "input"],
                        "additionalProperties": false
                    },
                    "description": "I/O added to the new entity (as add_entity_connections)"
                },
                "force": {
                    "type": "boolean",
                    "default": false,
                    "description": "skip the FGD output/input checks"
                },
                "allowRetie": {
                    "type": "boolean",
                    "default": false,
                    "description": "meshNodeIDs only: move meshes that belong to another brush entity"
                }
            },
            "additionalProperties": false
        })),
        create_trigger,
    );

    r.register(
        ToolSpec::new(
            "tie_to_entity",
            "Tie existing live world meshes to a brush entity: a NEW one (`class`) or an existing \
             one (`entityID`). Already-tied meshes are skipped; `material` retextures the tied \
             meshes. Not for: making a volume from scratch (create_trigger). Never saves.",
            Mutating,
            &["entity.tieMeshes"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "class": {
                    "type": "string",
                    "description": "FGD @SolidClass of a new entity; exactly one of class, entityID"
                },
                "entityID": {
                    "type": "integer", "minimum": 0, "maximum": MAX_NODE_ID,
                    "description": "existing live brush entity; refuses class/targetname/spawnflags/keyValues"
                },
                "meshNodeIDs": {
                    "type": "array", "minItems": 1, "maxItems": MAX_MESH_NODE_IDS,
                    "items": {"type": "integer", "minimum": 0, "maximum": MAX_NODE_ID},
                    "description": "live world meshes to tie"
                },
                "targetname": {
                    "type": "string"
                },
                "spawnflags": {
                    "type": "array",
                    "items": {"type": ["string", "integer"]},
                    "description": "COMPLETE set: labels or bit numbers; unlisted bits OFF; omit = FGD defaults"
                },
                "keyValues": {
                    "type": "object",
                    "description": "other FGD keyvalues; not classname/spawnflags/targetname/origin/angles/scales",
                    "additionalProperties": {"type": ["string", "number", "boolean"]}
                },
                "material": {
                    "type": "string",
                    "description": "retexture every tied mesh (default: keep their own)"
                },
                "allowRetie": {
                    "type": "boolean",
                    "default": false,
                    "description": "move meshes that belong to another brush entity"
                }
            },
            "required": ["meshNodeIDs"],
            "additionalProperties": false
        })),
        tie_to_entity,
    );

    r.register(
        ToolSpec::new(
            "move_to_world",
            "Move a brush entity's meshes back under its parent (world/group) and delete the \
             entity. One undo group; never saves.",
            Mutating,
            &["entity.untie"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "nodeID": {
                    "type": "integer", "minimum": 0, "maximum": MAX_NODE_ID,
                    "description": "brush entity node id (list_triggers / get_entity)"
                },
                "deleteEntity": {
                    "type": "boolean", "default": true,
                    "description": "false is refused"
                }
            },
            "required": ["nodeID"],
            "additionalProperties": false
        })),
        move_to_world,
    );
}

// -------------------------------------------------------------------------------------------
// Arguments
// -------------------------------------------------------------------------------------------

/// `class?`: a non-empty string, default [`DEFAULT_CLASS`] for `create_trigger` (required for
/// `tie_to_entity`).
fn class_arg(args: &Value, default: Option<&str>) -> Result<String, ToolError> {
    match args.get("class").filter(|value| !value.is_null()) {
        None => default
            .map(str::to_string)
            .ok_or_else(|| invalid("class is required (an FGD @SolidClass name)")),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() {
                return Err(invalid("class must be a non-empty FGD class name"));
            }
            Ok(text.to_string())
        }
        Some(other) => Err(invalid(format!(
            "class must be a non-empty FGD class name, got {other}"
        ))),
    }
}

/// The `kind` spelling (`ClassKind` has no `as_str`).
fn kind_str(kind: ClassKind) -> &'static str {
    match kind {
        ClassKind::Point => "point",
        ClassKind::Solid => "solid",
        ClassKind::Base => "base",
        ClassKind::Other => "other",
    }
}

/// The FGD spelling of a `ClassKind` (`@PointClass`, `@SolidClass`, ...): the FGD keywords are
/// capitalised, unlike [`kind_str`]'s lower-case data value (A7).
fn fgd_kind_str(kind: ClassKind) -> &'static str {
    match kind {
        ClassKind::Point => "Point",
        ClassKind::Solid => "Solid",
        ClassKind::Base => "Base",
        ClassKind::Other => "Other",
    }
}

/// An unknown FGD class: refused locally, nothing sent. The suggestions are the database's own
/// (`Validation::nearest`, the same list `UnknownClass` carries everywhere else).
fn unknown_class(db: &fgd::Database, class: &str) -> ToolError {
    let nearest = db.validate(class, &Map::new()).nearest;
    ToolError::new(
        "UnknownClass",
        "UnknownClass",
        format!(
            "class '{class}' is not a class in the FGD database; a trigger is an FGD @SolidClass \
             (nothing was sent)"
        ),
    )
    .with_code(-32006)
    .with_data(json!({"class": class, "nearest": nearest}))
    .did_you_mean(nearest)
    .retry_safe(true)
}

/// A class the FGD carries but that is not a brush entity (a `@PointClass`, a `@BaseClass`): the
/// tie can only build a `@SolidClass` (plan §1's `NotASolidClass {class, kind}`), nothing sent.
fn not_a_solid_class(class: &str, kind: ClassKind) -> ToolError {
    ToolError::new(
        "NotASolidClass",
        "NotASolidClass",
        format!(
            "class '{class}' is a @{}Class in the FGD: a trigger volume is a @SolidClass \
             (func_buyzone-like), so nothing was sent (use create_entities for a point entity)",
            fgd_kind_str(kind)
        ),
    )
    .with_code(-32602)
    .with_data(json!({"class": class, "kind": kind_str(kind)}))
    .retry_safe(true)
}

/// The FGD database is required: without it neither the class kind, the spawnflags, the outputs nor
/// `auto_apply_material` can be checked, and a trigger built on a guess is worse than a refusal.
fn fgd_required(warning: &str) -> ToolError {
    ToolError::new(
        "FgdUnavailable",
        "FgdUnavailable",
        format!(
            "{warning}; the trigger tools need the class database (class kind, spawnflags, \
             outputs, auto_apply_material) and refuse rather than guess (nothing was sent)"
        ),
    )
    .with_code(-32011)
    .retry_safe(true)
}

/// `targetname?`: a non-empty string of at most [`MAX_VALUE_BYTES`] bytes without NUL.
fn targetname_arg(args: &Value) -> Result<Option<String>, ToolError> {
    let Some(value) = args.get("targetname").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(text) = value.as_str() else {
        return Err(invalid(format!("targetname must be a string, got {value}")));
    };
    let text = text.trim();
    if text.is_empty() {
        return Err(invalid(
            "targetname must not be empty (pass no targetname for an unnamed entity)",
        ));
    }
    if text.len() > MAX_VALUE_BYTES {
        return Err(invalid(format!(
            "targetname is {} bytes, at most {MAX_VALUE_BYTES} fit",
            text.len()
        )));
    }
    if text.contains('\0') {
        return Err(invalid(
            "targetname cannot contain a NUL character (the engine stores C strings)",
        ));
    }
    Ok(Some(text.to_string()))
}

/// `keyValues?` for a trigger: the scalar coercion `entities_live::keyvalues_arg` uses, but the
/// spellings the tool has fields for are refused — `classname` (the `class` argument),
/// `spawnflags`/`targetname` (their arguments) and `origin` (the volume) — plus the
/// engine-managed `angles`/`scales`, which a brush entity does not carry at all.
fn trigger_keyvalues_arg(value: Option<&Value>) -> Result<Map<String, Value>, ToolError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Map::new());
    };
    let Some(object) = value.as_object() else {
        return Err(invalid("keyValues must be an object of keyvalue scalars"));
    };
    if object.len() > 32 {
        return Err(invalid(format!(
            "keyValues has {} keys, at most 32 fit in one request",
            object.len()
        )));
    }
    let mut out = Map::new();
    for (key, value) in object {
        let lower = key.to_ascii_lowercase();
        if let Some(field) = match lower.as_str() {
            "classname" => Some("class"),
            "spawnflags" => Some("spawnflags"),
            "targetname" => Some("targetname"),
            "origin" => Some("box/geometry/meshNodeIDs"),
            _ => None,
        } {
            return Err(invalid(format!(
                "keyValues.{key} cannot be set: the field above is its spelling (use `{field}`)"
            )));
        }
        if matches!(lower.as_str(), "angles" | "scales") {
            return Err(invalid(format!(
                "keyValues.{key} cannot be set: a brush entity's geometry is its meshes, not a \
                 node field (Hammer manages it)"
            )));
        }
        if key.is_empty() || key.chars().any(char::is_control) {
            return Err(invalid(format!(
                "keyValues[{key:?}]: a key must be non-empty and without control characters"
            )));
        }
        let Some(text) = crate::tools::entities_live::value_text(value) else {
            return Err(invalid(format!(
                "keyValues[{key:?}] must be a string, number or bool, got {value} (arrays, \
                 objects and null are not writable keyvalues)"
            )));
        };
        if text.len() > MAX_VALUE_BYTES {
            return Err(invalid(format!(
                "keyValues[{key:?}]: the value is {} bytes, at most {MAX_VALUE_BYTES} fit",
                text.len()
            )));
        }
        if text.contains('\0') {
            return Err(invalid(format!(
                "keyValues[{key:?}]: a value cannot contain a NUL character"
            )));
        }
        out.insert(key.clone(), Value::String(text));
    }
    Ok(out)
}

// `material_arg` (D1) is the shared `trigger_args::material_arg` — the same rules and messages
// `set_trigger_volume` uses.

/// `meshNodeIDs`: 1..=[`MAX_MESH_NODE_IDS`] distinct ids in the bridge's `int` range. Duplicates
/// are refused here (the bridge would tie the mesh once and then read the second mention as
/// "already tied"); the live existence/class/tie-state checks are the bridge's own.
fn mesh_node_ids_arg(value: &Value) -> Result<Vec<i64>, ToolError> {
    let Some(list) = value.as_array() else {
        return Err(invalid(
            "meshNodeIDs must be an array of 1..=64 live mesh node ids",
        ));
    };
    if list.is_empty() {
        return Err(invalid(
            "meshNodeIDs is empty: a brush entity needs at least one mesh",
        ));
    }
    if list.len() > MAX_MESH_NODE_IDS {
        return Err(invalid(format!(
            "meshNodeIDs has {} entries, at most {MAX_MESH_NODE_IDS} fit in one tie",
            list.len()
        )));
    }
    let mut out: Vec<i64> = Vec::with_capacity(list.len());
    for value in list {
        let Some(id) = value.as_i64().filter(|id| (0..=MAX_NODE_ID).contains(id)) else {
            return Err(invalid(format!(
                "meshNodeIDs entries must be integers in 0..={MAX_NODE_ID}, got {value}"
            )));
        };
        if out.contains(&id) {
            return Err(invalid(format!(
                "meshNodeIDs lists nodeID {id} twice: one tie per mesh"
            )));
        }
        out.push(id);
    }
    Ok(out)
}

/// `entityID?` (`tie_to_entity`): a live node id in the bridge's `int` range. The existence and
/// brush-entity checks are the bridge's own (`-32022` / `-32602 "not a brush entity"`).
fn entity_id_arg(args: &Value) -> Result<Option<i64>, ToolError> {
    let Some(value) = args.get("entityID").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    value
        .as_i64()
        .filter(|id| (0..=MAX_NODE_ID).contains(id))
        .map(Some)
        .ok_or_else(|| {
            invalid(format!(
                "entityID must be an integer in 0..={MAX_NODE_ID}, got {value}"
            ))
        })
}

/// The volume: exactly one of `box`, `geometry` and `meshNodeIDs`.
#[derive(Debug)]
enum Volume {
    /// A volume this call creates: the ready-to-send world vertices and faces (the box/geometry
    /// forms, RAW — unrounded, the face gates are `mesh_live::send_geometry`'s — everything local
    /// happens in the argument phase, before the first bridge request).
    Built(Vec<Vec3>, Vec<Vec<usize>>),
    /// Existing world meshes to tie.
    Meshes(Vec<i64>),
}

impl Volume {
    /// The mesh ids that already exist (the `Meshes` form's tie input).
    fn existing(&self) -> &[i64] {
        match self {
            Volume::Meshes(ids) => ids,
            _ => &[],
        }
    }
}

/// `box {min, max}`: the shared [`box_volume`] (finite corners, `min < max`, at least one unit on
/// every axis, built through `crate::geometry`) adopted as this module's `Volume::Built` — the
/// vertices go to `mesh_live::send_geometry` unrounded: it rounds them and runs the
/// coordinate/face gates itself (A1).
fn box_arg(value: &Value) -> Result<Volume, ToolError> {
    box_volume(value).map(|(vertices, faces)| Volume::Built(vertices, faces))
}

/// `geometry {vertices, faces}`: the shared [`geometry_volume`] (the `mesh.create` rules) adopted
/// as this module's `Volume::Built`; the rounding and the face gates are
/// `mesh_live::send_geometry`'s (A1).
fn geometry_arg(value: &Value) -> Result<Volume, ToolError> {
    geometry_volume(value).map(|(vertices, faces)| Volume::Built(vertices, faces))
}

/// Exactly one volume form (plan §1: "two volumes given / none" are both argument errors).
fn volume_arg(args: &Value) -> Result<Volume, ToolError> {
    let box_ = args.get("box").filter(|value| !value.is_null());
    let geometry_ = args.get("geometry").filter(|value| !value.is_null());
    let meshes = args.get("meshNodeIDs").filter(|value| !value.is_null());
    let given = [box_, geometry_, meshes]
        .iter()
        .filter(|value| value.is_some())
        .count();
    if given != 1 {
        return Err(invalid(format!(
            "exactly one volume is required: `box` (min/max), `geometry` (vertices/faces) or \
             `meshNodeIDs` (existing world meshes) — got {given} of 3"
        )));
    }
    match (box_, geometry_, meshes) {
        (Some(value), _, _) => box_arg(value),
        (_, Some(value), _) => geometry_arg(value),
        (_, _, Some(value)) => mesh_node_ids_arg(value).map(Volume::Meshes),
        _ => unreachable!("exactly one volume was checked above"),
    }
}

// The former local `round4`/`as_float`/`check_geometry` helpers are gone (review A1/D1): the
// volume now goes through `mesh_live::send_geometry`, which rounds the vertices, runs the bridge's
// coordinate gate, the face-normal/area gates on the rounded numbers **and** on the f32 widening
// Hammer reads, and refuses an over-budget request before it is sent — one implementation, the one
// `create_mesh` uses.

// -------------------------------------------------------------------------------------------
// Capability gating (per call — see the module doc)
// -------------------------------------------------------------------------------------------

/// The capability check for the bridge methods **this call** actually dispatches. The registry's
/// declared list ([`ToolSpec::bridge_methods`]) can only gate what every call needs, so the
/// optional methods (`mesh.create`, `mesh.setMaterial`, `entity.find`/`entity.get`/
/// `entity.addConnections`) are checked here, at the point the call decides to use them and always
/// **before the first send**. The error is [`require_caps`]'s own: `NotImplemented`
/// (`CapabilityMissing`, `data.missing`, `retrySafe:true`).
async fn require_call_caps(state: &HammerState, methods: &[&str]) -> Result<(), ToolError> {
    if methods.is_empty() {
        return Ok(());
    }
    require_caps(&state.bridge, methods).await
}

// -------------------------------------------------------------------------------------------
// FGD helpers (WP-F's `crate::fgd::Database` API)
// -------------------------------------------------------------------------------------------

/// The `spawnflags` view the results report (`{value:int|null, flags:[{bit,label,on}],
/// unknownBits}`) from WP-F's [`fgd::SpawnflagsView`]: one row per declared flag in FGD order,
/// `unknownBits` the bits set in `value` that no declared flag covers (a bitmask, `0` = all known).
fn spawnflags_view(view: &fgd::SpawnflagsView) -> Value {
    json!({
        "value": view.value,
        "flags": view.flags.iter()
            .map(|flag| json!({"bit": flag.bit, "label": flag.label, "on": flag.on}))
            .collect::<Vec<Value>>(),
        "unknownBits": view.unknown_bits,
    })
}

/// The "no spawnflags keyvalue at all" view: `value` null, no flag rows.
fn null_spawnflags_view() -> Value {
    json!({"value": Value::Null, "flags": [], "unknownBits": 0})
}

/// Decode one already-encoded `spawnflags` value for the result. WP-F's
/// [`fgd::Database::decode_spawnflags`] takes the stored text, so the value is passed as its own
/// decimal spelling; the class's `spawnflags_def` was checked, so this cannot fail (a class with no
/// keyvalue is never decoded here).
fn decode_value(db: &fgd::Database, class: &str, value: u64) -> Value {
    db.decode_spawnflags(class, &value.to_string())
        .map(|view| spawnflags_view(&view))
        .unwrap_or_else(|_| null_spawnflags_view())
}

/// WP-F's [`fgd::UnknownSpawnflag`] in this module's vocabulary: `UnknownSpawnflag` (`-32602`,
/// nothing sent) with the class's valid labels and the offending spelling; `known` carries the
/// declared bit/label rows for a caller that wants to retry.
fn unknown_spawnflag(error: fgd::UnknownSpawnflag, db: &fgd::Database) -> ToolError {
    let known: Vec<Value> = db
        .spawnflags_def(&error.class)
        .map(|def| {
            def.flags
                .iter()
                .map(|flag| json!({"bit": flag.bit, "label": flag.label}))
                .collect()
        })
        .unwrap_or_default();
    let valid = error.valid.clone();
    ToolError::new(
        "UnknownSpawnflag",
        "UnknownSpawnflag",
        format!(
            "spawnflags entry {:?} is not a flag of class '{}' (FGD): the valid ones are {} \
             (nothing was sent)",
            error.name,
            error.class,
            if valid.is_empty() {
                "none".to_string()
            } else {
                valid.join(", ")
            }
        ),
    )
    .with_code(-32602)
    .with_data(json!({
        "class": error.class,
        "name": error.name,
        "valid": valid,
        "known": known,
    }))
    .retry_safe(true)
}

/// One tie's spawnflags: the text to send, the decoded view for the result and the note for an
/// omitted argument.
#[derive(Debug)]
struct SpawnflagsPlan {
    /// The text for the entity's `spawnflags` keyvalue; `None` = send none at all (the class
    /// declares no `spawnflags` and the caller named none).
    text: Option<String>,
    /// The result's `spawnflags` view.
    view: Value,
    /// The note for an omitted argument whose FGD defaults were sent explicitly.
    note: Option<String>,
    /// A2: one warning per FGD `default_on` flag an explicit list leaves OFF (the list is the
    /// **complete** set, so a caller naming only the touch class silently turns the class's own
    /// default bits off — `trigger_push`'s 4096 changes the push physics for props).
    warnings: Vec<String>,
}

/// `spawnflags?` -> the text to send plus the decoded view. Omitted: the FGD defaults are sent
/// **explicitly** (the tool does not rely on the node factory applying them) and the note says so;
/// a class without a `spawnflags` keyvalue keeps none, and naming a flag there is
/// `UnknownSpawnflag`. Both directions are WP-F's [`fgd::Database`] lookups.
///
/// An explicit list is the COMPLETE set (the decision recorded in `target/WP.md`): the bits are
/// sent verbatim, and every `default_on` flag the list turns off gets a warning naming it and its
/// bit — the tool never silently ORs the defaults back in.
fn spawnflags_plan(
    db: &fgd::Database,
    class: &str,
    value: Option<&Value>,
) -> Result<SpawnflagsPlan, ToolError> {
    let declared = db.spawnflags_def(class).is_some();
    match value.filter(|value| !value.is_null()) {
        None => {
            if !declared {
                return Ok(SpawnflagsPlan {
                    text: None,
                    view: null_spawnflags_view(),
                    note: None,
                    warnings: Vec::new(),
                });
            }
            let bits = db.default_spawnflags(class);
            let mut view = decode_value(db, class, bits);
            view["raw"] = json!(bits.to_string());
            let labels: Vec<String> = db
                .spawnflags_def(class)
                .map(|def| {
                    def.flags
                        .iter()
                        .filter(|flag| flag.default_on)
                        .map(|flag| format!("{} (bit {})", flag.label, flag.bit))
                        .collect()
                })
                .unwrap_or_default();
            Ok(SpawnflagsPlan {
                text: Some(bits.to_string()),
                view,
                note: Some(format!(
                    "spawnflags omitted: the FGD defaults of '{class}' are sent explicitly as \
                     \"{bits}\" ({}) so the compile does not depend on the node factory \
                     applying them",
                    if labels.is_empty() {
                        "no flag is on by default".to_string()
                    } else {
                        labels.join(", ")
                    }
                )),
                warnings: Vec::new(),
            })
        }
        Some(value) => {
            let Some(list) = value.as_array() else {
                return Err(invalid(
                    "spawnflags must be an array of flag labels or bit numbers",
                ));
            };
            let bits = db
                .encode_spawnflags(class, list)
                .map_err(|error| unknown_spawnflag(error, db))?;
            let mut view = decode_value(db, class, bits);
            view["raw"] = json!(bits.to_string());
            let warnings = db
                .spawnflags_def(class)
                .map(|def| {
                    def.flags
                        .iter()
                        .filter(|flag| flag.default_on && (bits & flag.bit) == 0)
                        .map(|flag| {
                            format!(
                                "spawnflags lists the complete set of '{class}' and leaves the FGD \
                                 default flag {} (bit {}) OFF: add {:?} (or {}) to the list if you \
                                 did not mean to disable it",
                                flag.label, flag.bit, flag.label, flag.bit
                            )
                        })
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default();
            Ok(SpawnflagsPlan {
                text: Some(bits.to_string()),
                view,
                note: None,
                warnings,
            })
        }
    }
}

/// The `keyValues` object one tie sends: the caller's keys plus `targetname`/`spawnflags` as text
/// (plan §1: "keyValues (incl. spawnflags/targetname as text)").
fn tie_keyvalues(
    keyvalues: &Map<String, Value>,
    targetname: Option<&str>,
    spawnflags: Option<&str>,
) -> Map<String, Value> {
    let mut out = keyvalues.clone();
    if let Some(targetname) = targetname {
        out.insert("targetname".into(), json!(targetname));
    }
    if let Some(spawnflags) = spawnflags {
        out.insert("spawnflags".into(), json!(spawnflags));
    }
    out
}

/// The ids a tie report names (`"1, 2"`).
fn id_list(ids: &[i64]) -> String {
    ids.iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The un-mapped bridge refusals' wording: the request **was** sent; the bridge decided the refusal
/// before it opened an undo group, so the document is unchanged. Only a local gate may say "nothing
/// was sent" (A7).
const REFUSED_BEFORE_GROUP: &str =
    "the bridge refused it before opening an undo group: the document is unchanged";

/// The bridge's tie refusals in this module's vocabulary. All of them are decided before the write,
/// so they are retry-safe — **as long as nothing of this call exists yet**; [`Progress::failure`]
/// turns them into a `PartialFailure` once a mesh or entity has landed (and counts the group the
/// in-group solid-class refusal leaves behind).
///
/// A3: the bridge names the node it refused (`data.nodeID`) for a mesh; a missing `entityID` carries
/// none, so the `Existing(id)` target is what the message can name there.
fn tie_refusal(error: &BridgeError, target: TieTarget<'_>, sent: &[i64]) -> Option<ToolError> {
    let reason = reason_of(error);
    // the id the bridge put in the data (the mesh it refused), if any
    let named = error.data.get("nodeID").and_then(Value::as_i64);
    let named_list = match named {
        Some(node) => format!("nodeID {node}"),
        None => format!("one of the meshes ({})", id_list(sent)),
    };
    match error.code {
        Some(-32602) if reason.contains("not a solid") => {
            let class = match target {
                TieTarget::Class(class) => class.to_string(),
                TieTarget::Existing(id) => format!("nodeID {id}"),
            };
            Some(
                ToolError::new(
                    "NotASolidClass",
                    "NotASolidClass",
                    format!(
                        "Hammer's node factory built '{class}' as a NON-SOLID class: the FGD and \
                         Hammer's own gamedata disagree, so the bridge deleted the node it created \
                         in the same undo group — that group is left on the stack (it is empty), \
                         so `undo_last {{acceptUserEdits:true}}` once before retrying, and fix the \
                         FGD/Hammer mismatch. Nothing of this call is in the document. {reason}"
                    ),
                )
                .with_code(-32602)
                .with_data(json!({
                    "class": class,
                    "meshNodeIDs": sent,
                    "undoGroupOpened": true,
                    "reason": reason,
                }))
                .retry_safe(true),
            )
        }
        Some(-32602) if reason.contains("already tied") => Some(
            ToolError::new(
                "MeshAlreadyTied",
                "MeshAlreadyTied",
                format!(
                    "{} is already tied to a brush entity (parentNodeID {}): untie it first \
                     (move_to_world) or pass an untied mesh; {REFUSED_BEFORE_GROUP}. {reason}",
                    named_list,
                    error
                        .data
                        .get("parentNodeID")
                        .map(Value::to_string)
                        .unwrap_or_else(|| "?".to_string())
                ),
            )
            .with_code(-32602)
            .with_data(json!({
                "meshNodeIDs": sent,
                "nodeID": error.data.get("nodeID").cloned().unwrap_or(Value::Null),
                "parentNodeID": error.data.get("parentNodeID").cloned().unwrap_or(Value::Null),
                "reason": reason,
            }))
            .retry_safe(true),
        ),
        Some(-32602) if reason.contains("not a mesh") => Some(
            ToolError::new(
                "NotAMesh",
                "NotAMesh",
                format!(
                    "{} is not a mesh (a world, entity, group, prefab or instance node): only \
                     CMapMesh nodes can be tied; {REFUSED_BEFORE_GROUP}. {reason}",
                    named_list
                ),
            )
            .with_code(-32602)
            .with_data(json!({
                "meshNodeIDs": sent,
                "nodeID": named.map(Value::from).unwrap_or(Value::Null),
                "reason": reason,
            }))
            .retry_safe(true),
        ),
        Some(-32602) if reason.contains("not a brush entity") => Some(
            ToolError::new(
                "NotABrushEntity",
                "NotABrushEntity",
                format!(
                    "the `entityID` is not a brush entity (a @SolidClass entity): a point entity \
                     (or a non-entity) has no tied mesh volume to add these meshes to; \
                     {REFUSED_BEFORE_GROUP}. {reason}"
                ),
            )
            .with_code(-32602)
            .with_data(json!({
                "meshNodeIDs": sent,
                "entityID": error.data.get("entityID").cloned().unwrap_or(Value::Null),
                "reason": reason,
            }))
            .retry_safe(true),
        ),
        Some(-32022) => Some(match (named, target) {
            // the bridge named the mesh it could not find
            (Some(node), _) => ToolError::new(
                "NodeNotFound",
                "NodeNotFound",
                format!(
                    "mesh nodeID {node} is not in the live document; {REFUSED_BEFORE_GROUP}. \
                     {reason}"
                ),
            )
            .with_code(-32022)
            .with_data(json!({"meshNodeIDs": sent, "nodeID": node, "reason": reason}))
            .retry_safe(true),
            // no node in the data and an entityID was addressed: it is the entity that is missing
            (None, TieTarget::Existing(id)) => ToolError::new(
                "NodeNotFound",
                "NodeNotFound",
                format!(
                    "entityID {id} is not in the live document (nothing in the document has that \
                     node id); {REFUSED_BEFORE_GROUP}. {reason}"
                ),
            )
            .with_code(-32022)
            .with_data(json!({"meshNodeIDs": sent, "entityID": id, "reason": reason}))
            .retry_safe(true),
            // the bridge named nothing (an older build, or a class-form tie)
            (None, TieTarget::Class(_)) => ToolError::new(
                "NodeNotFound",
                "NodeNotFound",
                format!(
                    "one of the meshes ({}) is not in the live document (the bridge does not say \
                     which one); {REFUSED_BEFORE_GROUP}. {reason}",
                    id_list(sent)
                ),
            )
            .with_code(-32022)
            .with_data(json!({"meshNodeIDs": sent, "reason": reason}))
            .retry_safe(true),
        }),
        _ => None,
    }
}

/// The `entity.untie` refusals: the empty-entity one is this module's own name, everything else
/// keeps the wire taxonomy (the bridge refused it before its group opened, so the document is
/// unchanged — see [`REFUSED_BEFORE_GROUP`]).
fn untie_refusal(error: &BridgeError, node_id: i64) -> Option<ToolError> {
    let reason = reason_of(error);
    match error.code {
        Some(-32602) if reason.contains("not a brush entity") => Some(
            ToolError::new(
                "NotABrushEntity",
                "NotABrushEntity",
                format!(
                    "nodeID {node_id} has no tied mesh children (a point entity, not a brush \
                     entity): {REFUSED_BEFORE_GROUP} ({reason})"
                ),
            )
            .with_code(-32602)
            .with_data(json!({"nodeID": node_id, "reason": reason}))
            .retry_safe(true),
        ),
        _ => None,
    }
}

/// A `mesh.create`/`mesh.setMaterial` failure on a **target** node in `mesh_live`'s vocabulary
/// (both resolve their target before the undo group opens, so nothing was written). That module's
/// `target_failure` is private, so this is its two cases.
fn mesh_target_failure(error: &BridgeError, node_id: Option<i64>) -> Option<ToolError> {
    let reason = reason_of(error);
    match error.code {
        Some(-32022) => node_id.map(|node_id| {
            ToolError::new(
                "MeshNotFound",
                "MeshNotFound",
                format!("nodeID {node_id} is not in the live document"),
            )
            .with_code(-32022)
            .with_data(json!({"nodeID": node_id, "reason": reason}))
            .retry_safe(true)
        }),
        Some(-32602) if reason.contains("not a mesh") => Some(
            ToolError::new(
                "NotAMesh",
                "NotAMesh",
                match node_id {
                    Some(node) => format!(
                        "nodeID {node} is not a mesh (only CMapMesh nodes take geometry): the \
                         bridge refused it before opening an undo group: the document is unchanged"
                    ),
                    None => "the node to write is not a mesh (only CMapMesh nodes take geometry): \
                             the bridge refused it before opening an undo group: the document is \
                             unchanged"
                        .to_string(),
                },
            )
            .with_code(-32602)
            .with_data(json!({"nodeID": node_id, "reason": reason}))
            .retry_safe(true),
        ),
        _ => None,
    }
}

// -------------------------------------------------------------------------------------------
// create_trigger
// -------------------------------------------------------------------------------------------

/// `create_trigger`: see the module doc for the flow. Everything local (the class kind, the
/// spawnflags, the keyvalues, the volume's geometry, the connection FGD checks) runs before the
/// first write; the volume goes through `mesh_live::send_geometry` (A1) and a failure after it is a
/// `PartialFailure` naming what had already landed.
pub async fn create_trigger(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(&args, &CREATE_ARGS)?;
    let class = class_arg(&args, Some(DEFAULT_CLASS))?;

    // the FGD is required: the class kind, its spawnflags, its outputs and auto_apply_material are
    // all decisions this tool makes from it
    let fgd = FgdSource::load(&state);
    let Some(db) = fgd.db() else {
        return Err(fgd_required(
            &fgd.warning()
                .unwrap_or_else(|| "fgd unavailable".to_string()),
        ));
    };
    let Some(def) = db.class(&class) else {
        return Err(unknown_class(db, &class));
    };
    if def.kind != ClassKind::Solid {
        return Err(not_a_solid_class(&class, def.kind));
    }

    let spawnflags = spawnflags_plan(db, &class, args.get("spawnflags"))?;
    let keyvalues = trigger_keyvalues_arg(args.get("keyValues"))?;
    let targetname = targetname_arg(&args)?;
    let force = bool_arg(&args, "force")?.unwrap_or(false);
    let volume = volume_arg(&args)?;
    let retexture = bool_arg(&args, "retexture")?.unwrap_or(false);
    let connections: Option<Vec<Connection>> =
        match args.get("connections").filter(|value| !value.is_null()) {
            None => None,
            Some(value) => Some(crate::tools::entity_io::connections_list(value)?),
        };

    let mut warnings: Vec<String> = fgd.warning().into_iter().collect();
    let mut notes: Vec<String> = vec![NO_SAVE_NOTE.to_string()];
    // the FGD keyvalue check (`create_entities`' policy: FGD *issues* are warnings — the entity
    // is the caller's to build, and the bridge writes what it is given)
    for issue in db.validate(&class, &keyvalues).issues {
        warnings.push(issue.message);
    }
    // A2: an explicit spawnflags list is the complete set, so every FGD default-on bit it leaves
    // off gets its own warning (the tool never ORs the defaults back in silently)
    warnings.extend(spawnflags.warnings.iter().cloned());
    if let Some(note) = spawnflags.note {
        notes.push(note);
    }

    let allow_retie = bool_arg(&args, "allowRetie")?.unwrap_or(false);

    // the material of the created volume: the argument, else the class's FGD
    // `auto_apply_material`, else `materials/tools/toolstrigger.vmat` for a trigger class, else the
    // `create_mesh` default — the result's `materialSource` says which
    let (material, material_source) = match material_arg(&args, "material")? {
        Some(material) => (material, "argument"),
        None => match db.auto_apply_material(&class) {
            Some(material) => {
                notes.push(format!(
                    "material: the class's FGD auto_apply_material '{material}' is used for the \
                     created mesh"
                ));
                (material, "fgd-metadata")
            }
            None if db.is_trigger_class(&class) => (
                TRIGGER_MATERIAL.to_string(),
                "default trigger material (no FGD auto_apply_material on this trigger class)",
            ),
            None => (
                crate::tools::mesh_live::DEFAULT_MATERIAL.to_string(),
                "create_mesh default (a non-trigger solid class with no FGD auto_apply_material)",
            ),
        },
    };
    if retexture && volume.existing().is_empty() {
        notes.push(
            "retexture:true with no `meshNodeIDs`: only the created mesh takes the material, so \
             there is nothing to retexture"
                .to_string(),
        );
    }

    // the bridge methods **this call** dispatches, checked before the first send (see the module
    // doc): the registry can only declare what every call needs
    if matches!(volume, Volume::Built(..)) {
        require_call_caps(&state, &["mesh.create"]).await?;
    }
    if retexture && !volume.existing().is_empty() {
        require_call_caps(&state, &["mesh.setMaterial"]).await?;
    }
    if connections.is_some() {
        require_call_caps(
            &state,
            &["entity.find", "entity.get", "entity.addConnections"],
        )
        .await?;
    }

    let deadline = ctx.budget.deadline_ms()?;
    // the connection pre-flight, BEFORE the mesh exists: the class stands in for the source
    // entity, so an unknown output (or a prop_static target) refuses the call with nothing sent
    let pre_checked: Option<ConnectionRun> = match &connections {
        None => None,
        Some(list) => Some(
            crate::tools::entity_io::connection_run(
                &state,
                deadline,
                ConnectionSource::Class(&class),
                list,
                force,
            )
            .await?,
        ),
    };
    if let Some(run) = &pre_checked {
        for warning in &run.warnings {
            if !warnings.contains(warning) {
                warnings.push(warning.clone());
            }
        }
        for note in &run.notes {
            if !notes.contains(note) {
                notes.push(note.clone());
            }
        }
    }

    let doc = require_document(&state, deadline).await?;
    let doc_id = doc_id_of(&doc);
    let mut progress = Progress::new(doc_state(&doc));
    let mut mesh_rows: Vec<Value> = Vec::new();

    // ---- step (a): the volume's mesh (the box/geometry forms; the meshes form ties existing ones)
    //
    // A1: the volume goes through `mesh_live::send_geometry` — the exact path `create_mesh` uses,
    // with its request-size budget (an oversized payload is refused here instead of coming back as
    // the bridge's `-32600 "line too long"`, which closes the connection) and its M1 face-count
    // check (Hammer welding a face away). It also reads the bridge's own `undoLabel` (A6) and
    // reports the write's own notes.
    if let Volume::Built(vertices, faces) = &volume {
        let created = Value::Object(
            crate::tools::mesh_live::send_geometry(
                &state,
                &ctx,
                "mesh.create",
                None,
                crate::tools::mesh_live::Payload {
                    vertices: vertices.clone(),
                    faces: faces.clone(),
                    face_materials: None,
                },
                &material,
                UNDO_NOTE_CREATE,
                Some(doc.clone()),
            )
            .await?,
        );
        let Some(id) = created.get("nodeID").and_then(Value::as_i64) else {
            return Err(ToolError::internal(format!(
                "mesh.create created nothing this server can address (no nodeID in {created}); \
                 read the live document (node.list) before retrying"
            )));
        };
        progress.mesh = Some(id);
        progress.push(&created, "MCP: create mesh");
        mesh_rows.push(json!({"nodeID": id, "created": true}));
        // the mesh write's own notes: the M1 face-count mismatch
        // (NO_SAVE_NOTE is this module's own, added below)
        for note in created
            .get("notes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if note != NO_SAVE_NOTE {
                notes.push(note.to_string());
            }
        }
    }
    for id in volume.existing() {
        mesh_rows.push(json!({"nodeID": id, "created": false}));
    }
    let mesh_ids: Vec<i64> = mesh_rows
        .iter()
        .filter_map(|row| row.get("nodeID").and_then(Value::as_i64))
        .collect();

    // ---- step (b): the tie (creates the entity and re-parents the meshes)
    let tie_values = tie_keyvalues(
        &keyvalues,
        targetname.as_deref(),
        spawnflags.text.as_deref(),
    );
    // every step re-reads `doc.active` for its **own** `expectDoc`: a write moves the document's
    // fingerprint (the mock bumps its epoch per edit; live it moves on an identity change), and no
    // step of this call may carry another step's stale fingerprint.
    let doc = if progress.mesh.is_some() {
        require_document(&state, deadline).await?
    } else {
        doc
    };
    let reply = tie_request(
        &state,
        deadline,
        &doc,
        TieTarget::Class(&class),
        &mesh_ids,
        &tie_values,
        allow_retie,
    )
    .await;
    let result = match reply {
        Ok(result) => result,
        Err(error) => {
            let mapped = tie_refusal(&error, TieTarget::Class(&class), &mesh_ids)
                .unwrap_or_else(|| mutator_failure(error, &state, doc_id.as_deref()));
            return Err(partial_failure(
                &state,
                &progress,
                deadline,
                doc_id.as_deref(),
                "entity.tieMeshes",
                mapped,
            )
            .await);
        }
    };
    let tie = match read_tie(&result, &class) {
        Ok(tie) => tie,
        Err(error) => {
            return Err(partial_failure(
                &state,
                &progress,
                deadline,
                doc_id.as_deref(),
                "entity.tieMeshes",
                error,
            )
            .await)
        }
    };
    // A8: an all-`alreadyTied` reply is a no-op — the bridge opened no group, so there is nothing
    // to settle or count. (create_trigger always builds a NEW entity, so nothing of its meshes can
    // already be tied to it; this is the shared read's defensive branch.)
    let tie_noop = tie.noop || tie.label.is_none();
    if !tie_noop {
        note_edit(&state, doc_id.as_deref());
        if let Some(label) = tie.label.clone() {
            progress.labels.push(label);
        }
    }
    if !tie.echoed && !tie_values.is_empty() {
        notes.push(no_keyvalues_echo_note().to_string());
    }
    warnings.extend(unapplied_keyvalue_warnings(&tie));
    progress.entity = Some(tie.entity);
    let entity_id = tie.entity;
    let origin = tie.origin.clone();

    // ---- step (b2): retexture the meshes that already existed
    if retexture {
        for id in volume.existing() {
            // each mesh.setMaterial is its own counted write: it carries its own fresh `expectDoc`
            // and its own counter tick
            let doc = require_document(&state, deadline).await?;
            let result = material_request(&state, deadline, &doc, *id, &material).await;
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    let mapped = mesh_target_failure(&error, Some(*id))
                        .unwrap_or_else(|| mutator_failure(error, &state, doc_id.as_deref()));
                    return Err(partial_failure(
                        &state,
                        &progress,
                        deadline,
                        doc_id.as_deref(),
                        "mesh.setMaterial",
                        mapped,
                    )
                    .await);
                }
            };
            super::live_common::note_edit(&state, doc_id.as_deref());
            progress.push(&result, "MCP: set mesh material");
        }
        notes.push(format!(
            "the meshes named in `meshNodeIDs` were retextured with '{material}' (one \
             mesh.setMaterial group each)"
        ));
    }

    // ---- step (c): the connections of the new entity
    let connections_out = match &connections {
        None => json!({"count": 0, "added": [], "unconfirmed": [], "checked": []}),
        Some(list) => {
            let run = crate::tools::entity_io::connection_run(
                &state,
                deadline,
                ConnectionSource::Node(entity_id),
                list,
                force,
            )
            .await;
            let run = match run {
                Ok(run) => run,
                Err(error) => {
                    return Err(partial_failure(
                        &state,
                        &progress,
                        deadline,
                        doc_id.as_deref(),
                        "entity.addConnections",
                        error,
                    )
                    .await)
                }
            };
            let write = run.write.expect("the Node form always performs the write");
            progress.labels.push(
                write
                    .result
                    .get("undoLabel")
                    .and_then(Value::as_str)
                    .unwrap_or("MCP: add connections")
                    .to_string(),
            );
            let mut out = Map::new();
            out.insert("count".into(), json!(write.count));
            out.insert("added".into(), Value::Array(write.added));
            out.insert("unconfirmed".into(), json!(write.unconfirmed));
            if write.unconfirmed_note {
                out.insert("outcomeUnknown".into(), json!(true));
            }
            // what the write step re-checked (the same FGD resolution as the pre-flight)
            if !run.checked.is_empty() {
                out.insert("checked".into(), Value::Array(run.checked));
            }
            Value::Object(out)
        }
    };

    let undo_groups = progress.labels.len();
    notes.push(format!(
        "this call opened {undo_groups} undo group(s) ({}): `undo_last` {undo_groups} time(s) \
         reverts it (each step is its own Hammer group)",
        progress
            .labels
            .iter()
            .map(|label| format!("\"{label}\""))
            .collect::<Vec<_>>()
            .join(", ")
    ));

    let mut out = Map::new();
    out.insert("nodeID".into(), json!(entity_id));
    out.insert("classname".into(), json!(class));
    out.insert("targetname".into(), json!(targetname));
    out.insert("origin".into(), origin);
    out.insert("meshes".into(), Value::Array(mesh_rows));
    out.insert("entityCreated".into(), json!(tie.created));
    out.insert("alreadyTied".into(), json!(tie.already_tied));
    out.insert("removedEmptyEntities".into(), json!(tie.removed_empty));
    out.insert("keyValuesEcho".into(), json!(tie.echoed));
    out.insert("applied".into(), Value::Object(tie.applied.clone()));
    out.insert("material".into(), json!(material));
    out.insert("materialSource".into(), json!(material_source));
    out.insert("spawnflags".into(), spawnflags.view);
    out.insert("retextured".into(), json!(retexture));
    out.insert("connections".into(), connections_out);
    out.insert("undoGroups".into(), json!(undo_groups));
    out.insert("undoLabels".into(), json!(progress.labels));
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("saved".into(), json!(false));
    out.insert("fgd".into(), fgd.json());
    out.insert("warnings".into(), json!(warnings));
    out.insert("notes".into(), json!(notes));
    Ok(Value::Object(out).into())
}

/// Which entity one `entity.tieMeshes` request ties into (§5's `class? | entityID?`).
#[derive(Debug, Clone, Copy)]
enum TieTarget<'a> {
    /// Create a new brush entity of this FGD class — the only form that carries `keyValues`.
    Class(&'a str),
    /// Tie into an existing live brush entity (the class's arguments are refused from the tool).
    Existing(i64),
}

/// One `entity.tieMeshes` request: the wire contract `add`-style (the tool builds the params, the
/// bridge's own label/state come back in the result). `allowRetie` is sent only when true, so the
/// default request is byte-for-byte the class-only shape.
async fn tie_request(
    state: &HammerState,
    deadline: u64,
    doc: &Value,
    target: TieTarget<'_>,
    mesh_ids: &[i64],
    keyvalues: &Map<String, Value>,
    allow_retie: bool,
) -> Result<Value, BridgeError> {
    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(
            "entity.tieMeshes",
            {
                let mut params = Map::new();
                match target {
                    TieTarget::Class(class) => {
                        params.insert("class".into(), json!(class));
                        if !keyvalues.is_empty() {
                            params.insert("keyValues".into(), Value::Object(keyvalues.clone()));
                        }
                    }
                    TieTarget::Existing(id) => {
                        params.insert("entityID".into(), json!(id));
                    }
                }
                params.insert("nodeIDs".into(), json!(mesh_ids));
                if allow_retie {
                    params.insert("allowRetie".into(), json!(true));
                }
                Value::Object(params)
            },
            RequestOpts {
                deadline_ms: Some(deadline),
                expect_doc: Some(expect_doc(doc)),
                gate: None,
            },
        )
        .await?;
    Ok(reply.result)
}

/// What one `entity.tieMeshes` reply says (§5), in this module's vocabulary: the entity it created
/// or addressed plus the two id lists the result echoes.
struct Tie {
    entity: i64,
    classname: String,
    created: bool,
    origin: Value,
    /// The meshes that were already under the target entity and were skipped.
    already_tied: Vec<i64>,
    /// The old brush entities the tie emptied and deleted (only with `allowRetie`).
    removed_empty: Vec<i64>,
    /// The bridge's own undo label (None = the bridge says it opened no group: `undoLabel:null` or
    /// `noop:true`, the all-`alreadyTied` no-op of §5/FIX-B).
    label: Option<String>,
    /// The bridge's own `noop:true` marker, if it sent one.
    noop: bool,
    /// A4: the bridge's `keyValues` echo — `{key: applied}` for every key it was given.
    applied: Map<String, Value>,
    /// A4: the bridge's own warnings for the keys it did not write.
    keyvalue_warnings: Vec<String>,
    /// A4: whether the reply carried a `keyValues` echo at all (`false` = the keys are not
    /// verified, and the call says so instead of reporting success).
    echoed: bool,
}

/// The `nodeID` list of one reply field (`[]` when absent, a non-int row is skipped).
fn ids_of(value: Option<&Value>) -> Vec<i64> {
    value
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// One reply's string list (`[]` when absent).
fn strings_of(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A4: the tie reply's `keyValues` echo (`apply_keyvalues`' `{applied, warnings}`), read like
/// `entities_live` reads `entity.create`'s. `None` = the reply carried none.
fn keyvalues_echo(map: &Map<String, Value>) -> Option<(Map<String, Value>, Vec<String>)> {
    let echo = map.get("keyValues").filter(|value| value.is_object())?;
    let applied = echo
        .get("applied")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    Some((applied, strings_of(echo.get("warnings"))))
}

/// Read one `entity.tieMeshes` reply. A missing `nodeID` is an `InternalError`: the write may have
/// landed, so the caller turns it into a `PartialFailure` when anything of this call exists.
///
/// A8: `undoLabel` is kept **optional** — the bridge answers `undoLabel:null` (or `noop:true`) when
/// every mesh was already under the target and it opened no group, and a caller must not push a
/// label for a group that does not exist.
fn read_tie(result: &Value, class_fallback: &str) -> Result<Tie, ToolError> {
    let Some(map) = result.as_object() else {
        return Err(ToolError::internal(format!(
            "entity.tieMeshes returned a non-object result: {result} \
             (crates/cs2-bridge/PROTOCOL.md §9.8)"
        )));
    };
    let Some(entity) = map.get("nodeID").and_then(Value::as_i64) else {
        return Err(ToolError::internal(format!(
            "entity.tieMeshes created nothing this server can address (no nodeID in {result}); \
             read the live document (find_entities / node.list) before retrying"
        )));
    };
    let label = match map.get("undoLabel") {
        // the bridge explicitly says it opened nothing
        Some(Value::Null) => None,
        Some(Value::String(label)) => Some(label.clone()),
        // absent: an older bridge, which opened a group; the fallback label is its own wording
        Some(_) => Some(TIE_LABEL.to_string()),
        None => Some(TIE_LABEL.to_string()),
    };
    let (applied, keyvalue_warnings) = keyvalues_echo(map).unwrap_or_default();
    // A4: "the bridge sent no echo" is about the member, not about its contents: an empty echo
    // (`applied:{}`) is the bridge saying "there was nothing to write"
    let echoed = map.get("keyValues").is_some();
    Ok(Tie {
        entity,
        classname: map
            .get("classname")
            .and_then(Value::as_str)
            .unwrap_or(class_fallback)
            .to_string(),
        created: map.get("created").and_then(Value::as_bool).unwrap_or(false),
        origin: map.get("origin").cloned().unwrap_or(Value::Null),
        already_tied: ids_of(map.get("alreadyTied")),
        removed_empty: ids_of(map.get("removedEmptyEntities")),
        label,
        noop: map.get("noop").and_then(Value::as_bool).unwrap_or(false),
        applied,
        keyvalue_warnings,
        echoed,
    })
}

/// A4: one warning per key the bridge's echo says it did not write (`applied:false`) — the caller
/// asked for a targetname/spawnflags/keyvalue the entity does not carry.
fn unapplied_keyvalue_warnings(tie: &Tie) -> Vec<String> {
    let mut out: Vec<String> = tie
        .applied
        .iter()
        .filter(|(_, applied)| applied.as_bool() != Some(true))
        .map(|(key, applied)| {
            format!(
                "the bridge did NOT write keyValues[{key:?}] (applied:{applied}): the entity does \
                 not carry it — read it back with get_entity before relying on it"
            )
        })
        .collect();
    out.extend(
        tie.keyvalue_warnings
            .iter()
            .map(|warning| format!("the bridge's keyValues warning: {warning}")),
    );
    out
}

/// A4: the note for a reply that carried no `keyValues` echo at all while this call sent keys.
fn no_keyvalues_echo_note() -> &'static str {
    "the bridge returned no `keyValues` echo: the keys this call wrote (spawnflags/targetname/\
     keyValues) are NOT verified — read them back with get_entity"
}

/// One `mesh.setMaterial` request (the retexture route: every face of a mesh that already exists).
async fn material_request(
    state: &HammerState,
    deadline: u64,
    doc: &Value,
    node_id: i64,
    material: &str,
) -> Result<Value, BridgeError> {
    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(
            "mesh.setMaterial",
            json!({"nodeID": node_id, "material": material}),
            RequestOpts {
                deadline_ms: Some(deadline),
                expect_doc: Some(expect_doc(doc)),
                gate: None,
            },
        )
        .await?;
    Ok(reply.result)
}

// -------------------------------------------------------------------------------------------
// tie_to_entity
// -------------------------------------------------------------------------------------------

/// `tie_to_entity`: the bare tie primitive (one `entity.tieMeshes` group, plus one
/// `mesh.setMaterial` group per mesh when `material` is given). Exactly one of `class` (create) and
/// `entityID` (tie into an existing brush entity); with `entityID` the class-form arguments are
/// refused and no FGD is needed.
pub async fn tie_to_entity(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(&args, &TIE_ARGS)?;
    let Some(mesh_value) = args.get("meshNodeIDs") else {
        return Err(invalid(
            "meshNodeIDs is required (an array of 1..=64 live world meshes)",
        ));
    };
    let mesh_ids = mesh_node_ids_arg(mesh_value)?;
    let existing = entity_id_arg(&args)?;
    // exactly one of class / entityID (§5); with entityID the class-form arguments are refused
    let class = match existing {
        Some(_) => {
            for key in ["class", "targetname", "spawnflags", "keyValues"] {
                if args.get(key).is_some_and(|value| !value.is_null()) {
                    return Err(invalid(format!(
                        "`{key}` cannot be given with entityID: the brush entity already exists \
                         (pass `class` to create a new one), so nothing was sent"
                    )));
                }
            }
            None
        }
        None => Some(class_arg(&args, None)?),
    };
    let allow_retie = bool_arg(&args, "allowRetie")?.unwrap_or(false);
    let material = material_arg(&args, "material")?;

    // the class form resolves the class, the flags and the keyvalues against the FGD; the entityID
    // form has no class to resolve, so a missing FGD is a warning there, not a refusal
    let fgd = FgdSource::load(&state);
    let mut warnings: Vec<String> = Vec::new();
    let mut notes: Vec<String> = vec![NO_SAVE_NOTE.to_string()];
    let mut spawnflags = SpawnflagsPlan {
        text: None,
        view: null_spawnflags_view(),
        note: None,
        warnings: Vec::new(),
    };
    let mut keyvalues = Map::new();
    let mut targetname: Option<String> = None;
    if let Some(class) = &class {
        let Some(db) = fgd.db() else {
            return Err(fgd_required(
                &fgd.warning()
                    .unwrap_or_else(|| "fgd unavailable".to_string()),
            ));
        };
        let Some(def) = db.class(class) else {
            return Err(unknown_class(db, class));
        };
        if def.kind != ClassKind::Solid {
            return Err(not_a_solid_class(class, def.kind));
        }
        spawnflags = spawnflags_plan(db, class, args.get("spawnflags"))?;
        keyvalues = trigger_keyvalues_arg(args.get("keyValues"))?;
        targetname = targetname_arg(&args)?;
        // the FGD keyvalue check (`create_entities`' policy: issues are warnings)
        for issue in db.validate(class, &keyvalues).issues {
            warnings.push(issue.message);
        }
    } else if let Some(warning) = fgd.warning() {
        warnings.push(warning);
    }
    if let Some(note) = spawnflags.note.clone() {
        notes.push(note);
    }
    // A2: an explicit spawnflags list is the complete set (see `spawnflags_plan`)
    warnings.extend(spawnflags.warnings.iter().cloned());
    // the optional retexture is the only method this call may need beyond the m2.19 marker
    if material.is_some() {
        require_call_caps(&state, &["mesh.setMaterial"]).await?;
    }

    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(&state, deadline).await?;
    let doc_id = doc_id_of(&doc);
    let mut progress = Progress::new(doc_state(&doc));

    let tie_values = tie_keyvalues(
        &keyvalues,
        targetname.as_deref(),
        spawnflags.text.as_deref(),
    );
    let target = match &class {
        Some(class) => TieTarget::Class(class),
        None => TieTarget::Existing(existing.expect("the entityID form parsed an id")),
    };
    let result = tie_request(
        &state,
        deadline,
        &doc,
        target,
        &mesh_ids,
        &tie_values,
        allow_retie,
    )
    .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            let mapped = tie_refusal(&error, target, &mesh_ids)
                .unwrap_or_else(|| mutator_failure(error, &state, doc_id.as_deref()));
            return Err(partial_failure(
                &state,
                &progress,
                deadline,
                doc_id.as_deref(),
                "entity.tieMeshes",
                mapped,
            )
            .await);
        }
    };

    let tie = match read_tie(&result, class.as_deref().unwrap_or("")) {
        Ok(tie) => tie,
        Err(error) => {
            return Err(partial_failure(
                &state,
                &progress,
                deadline,
                doc_id.as_deref(),
                "entity.tieMeshes",
                error,
            )
            .await)
        }
    };
    // A8: every mesh was already under the target — the bridge opened no undo group (`undoLabel`
    // null / `noop:true`), so this is a success no-op: no label, no unsaved edit and no cache
    // invalidation, only the note.
    let all_already_tied =
        !tie.created && !tie.already_tied.is_empty() && tie.already_tied.len() == mesh_ids.len();
    let noop = tie.noop || (tie.label.is_none() && all_already_tied);
    if noop {
        notes.push(format!(
            "nothing to do: every mesh named ({}) was already tied to entity {}{}: the bridge \
             opened no undo group and the document is unchanged",
            id_list(&mesh_ids),
            tie.entity,
            if tie.noop {
                ""
            } else {
                " (the reply carried no undo label)"
            }
        ));
    } else {
        super::live_common::note_edit(&state, doc_id.as_deref());
        if let Some(label) = tie.label.clone() {
            progress.labels.push(label);
        }
        progress.entity = Some(tie.entity);
    }
    if !tie.echoed && !tie_values.is_empty() {
        notes.push(no_keyvalues_echo_note().to_string());
    }
    warnings.extend(unapplied_keyvalue_warnings(&tie));
    let entity_id = tie.entity;

    // the optional retexture: one mesh.setMaterial group per mesh
    if let Some(material) = &material {
        for id in &mesh_ids {
            // each mesh.setMaterial is its own counted write: it carries its own fresh `expectDoc`
            // and its own counter tick
            let doc = require_document(&state, deadline).await?;
            let result = material_request(&state, deadline, &doc, *id, material).await;
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    let mapped = mesh_target_failure(&error, Some(*id))
                        .unwrap_or_else(|| mutator_failure(error, &state, doc_id.as_deref()));
                    return Err(partial_failure(
                        &state,
                        &progress,
                        deadline,
                        doc_id.as_deref(),
                        "mesh.setMaterial",
                        mapped,
                    )
                    .await);
                }
            };
            super::live_common::note_edit(&state, doc_id.as_deref());
            progress.push(&result, "MCP: set mesh material");
        }
        notes.push(format!(
            "every tied mesh was retextured with '{material}' (one mesh.setMaterial group each)"
        ));
    }

    let undo_groups = progress.labels.len();
    if undo_groups == 0 {
        notes.push(
            "this call opened no undo group: the document is unchanged and there is nothing to \
             revert"
                .to_string(),
        );
    } else {
        notes.push(format!(
            "this call opened {undo_groups} undo group(s) ({}): `undo_last` {undo_groups} time(s) \
             reverts it",
            progress
                .labels
                .iter()
                .map(|label| format!("\"{label}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let mut out = Map::new();
    out.insert("nodeID".into(), json!(entity_id));
    out.insert("classname".into(), json!(tie.classname));
    out.insert("targetname".into(), json!(targetname));
    out.insert("origin".into(), tie.origin.clone());
    out.insert(
        "meshes".into(),
        Value::Array(
            mesh_ids
                .iter()
                .map(|id| json!({"nodeID": id, "created": false}))
                .collect(),
        ),
    );
    out.insert("entityCreated".into(), json!(tie.created));
    out.insert("alreadyTied".into(), json!(tie.already_tied));
    out.insert("removedEmptyEntities".into(), json!(tie.removed_empty));
    out.insert("keyValuesEcho".into(), json!(tie.echoed));
    out.insert("applied".into(), Value::Object(tie.applied.clone()));
    out.insert("material".into(), json!(material));
    out.insert("spawnflags".into(), spawnflags.view);
    out.insert("undoGroups".into(), json!(undo_groups));
    out.insert("undoLabels".into(), json!(progress.labels));
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("saved".into(), json!(false));
    out.insert("fgd".into(), fgd.json());
    out.insert("warnings".into(), json!(warnings));
    out.insert("notes".into(), json!(notes));
    Ok(Value::Object(out).into())
}

// -------------------------------------------------------------------------------------------
// move_to_world
// -------------------------------------------------------------------------------------------

/// `move_to_world {nodeID, deleteEntity?}`: `entity.untie`, one group, never saves. The meshes go
/// back under the entity's PARENT (the world, or the group/prefab the entity sits in, §5). The
/// group is Hammer's own `"Untie from entity"` (not an `MCP: …` label this server put on it), so
/// reverting it is `undo_last {acceptUserEdits:true}`.
pub async fn move_to_world(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(&args, &UNTIE_ARGS)?;
    let node_id = node_id_arg(&args)?;
    let delete_entity = bool_arg(&args, "deleteEntity")?.unwrap_or(true);
    if !delete_entity {
        return Err(invalid(
            "deleteEntity:false is refused: Hammer's own move-to-world recipe puts the meshes back \
             under the world and removes the emptied entity, and keeping an empty brush entity is \
             not something this bridge can do yet (nothing was sent)",
        ));
    }

    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(&state, deadline).await?;
    let doc_id = doc_id_of(&doc);

    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "entity.untie",
                json!({"nodeID": node_id}),
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
            if let Some(tool) = untie_refusal(&error, node_id) {
                return Err(tool);
            }
            return Err(mutator_failure(error, &state, doc_id.as_deref()));
        }
    };
    super::live_common::note_edit(&state, doc_id.as_deref());

    let mut untie = match reply.result {
        Value::Object(map) => map,
        other => {
            return Err(ToolError::internal(format!(
                "entity.untie returned a non-object result: {other} \
                 (crates/cs2-bridge/PROTOCOL.md §9.9)"
            )))
        }
    };
    untie.remove("doc");
    let from_bridge = matches!(untie.get("meshes"), Some(Value::Array(_)));
    let mut meshes: Vec<i64> = untie
        .get("meshes")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    meshes.sort_unstable();

    let mut notes: Vec<String> = vec![
        NO_SAVE_NOTE.to_string(),
        "the meshes are back under the entity's PARENT (the world, or the group/prefab the entity \
         sat in) and the entity is gone; Hammer's own \"Untie from entity\" group restores the \
         tie — `undo_last {acceptUserEdits:true}` once (the label is not one this server put on a \
         group, so the plain `undo_last` refuses it)"
            .to_string(),
    ];
    if !from_bridge {
        notes.push(
            "the bridge's reply carried no `meshes` list: which meshes were returned to the world \
             is unknown — read the document (list_meshes) to confirm"
                .to_string(),
        );
    }

    let mut out = Map::new();
    out.insert("nodeID".into(), json!(node_id));
    out.insert("meshes".into(), json!(meshes));
    out.insert("entityDeleted".into(), json!(true));
    out.insert(
        "undoLabel".into(),
        untie
            .get("undoLabel")
            .cloned()
            .unwrap_or_else(|| json!(UNTIE_LABEL)),
    );
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("saved".into(), json!(false));
    out.insert("notes".into(), json!(notes));
    Ok(Value::Object(out).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The WP-F [`fgd::SpawnflagsView`] -> the result's `{value, flags, unknownBits}` mapping:
    /// `unknownBits` is WP-F's undeclared-bit **mask** (`unknown_bits`), one row per declared flag.
    #[test]
    fn spawnflags_view_is_the_wp_f_lookup() {
        let view = fgd::SpawnflagsView {
            value: 1 | 4 | 4096,
            flags: vec![
                fgd::FlagState {
                    bit: 1,
                    label: "Clients".into(),
                    on: true,
                },
                fgd::FlagState {
                    bit: 2,
                    label: "NPCs".into(),
                    on: false,
                },
                fgd::FlagState {
                    bit: 4096,
                    label: "Everything".into(),
                    on: true,
                },
            ],
            unknown_bits: 4,
        };
        assert_eq!(
            spawnflags_view(&view),
            json!({
                "value": 4101,
                "flags": [
                    {"bit": 1, "label": "Clients", "on": true},
                    {"bit": 2, "label": "NPCs", "on": false},
                    {"bit": 4096, "label": "Everything", "on": true}
                ],
                "unknownBits": 4
            })
        );
        assert_eq!(null_spawnflags_view()["value"], Value::Null);
        assert_eq!(null_spawnflags_view()["flags"], json!([]));
    }

    /// WP-F's [`fgd::UnknownSpawnflag`] in this module's vocabulary: `UnknownSpawnflag`
    /// (`-32602`, retry-safe) carrying the class, the offending spelling and the valid labels.
    #[test]
    fn unknown_spawnflag_carries_the_valid_labels() {
        let db = fgd::Database::default();
        let err = unknown_spawnflag(
            fgd::UnknownSpawnflag {
                class: "trigger_once".into(),
                name: "Dogs".into(),
                valid: vec!["Clients".into(), "NPCs".into()],
            },
            &db,
        );
        assert_eq!(err.name, "UnknownSpawnflag");
        assert_eq!(err.code, Some(-32602));
        assert!(err.retry_safe);
        assert_eq!(err.data["class"], json!("trigger_once"));
        assert_eq!(err.data["name"], json!("Dogs"));
        assert_eq!(err.data["valid"], json!(["Clients", "NPCs"]));
        assert!(err.message.contains("Clients"), "{}", err.message);
    }

    /// `entityID`: an int in the bridge's range; absent/`null` = the class form; anything else is
    /// `InvalidParams`.
    #[test]
    fn entity_id_is_closed() {
        assert_eq!(entity_id_arg(&json!({})).unwrap(), None);
        assert_eq!(entity_id_arg(&json!({"entityID": null})).unwrap(), None);
        assert_eq!(entity_id_arg(&json!({"entityID": 41})).unwrap(), Some(41));
        for bad in [json!(7.5), json!("41"), json!(-1), json!(MAX_NODE_ID + 1)] {
            assert!(entity_id_arg(&json!({"entityID": bad})).is_err(), "{bad}");
        }
    }

    /// The targetname bounds and the keyvalue refusals.
    #[test]
    fn arguments_are_closed() {
        assert_eq!(
            targetname_arg(&json!({"targetname": " t "}))
                .unwrap()
                .as_deref(),
            Some("t")
        );
        for bad in [
            json!({"targetname": ""}),
            json!({"targetname": 7}),
            json!({"targetname": "x".repeat(MAX_VALUE_BYTES + 1)}),
        ] {
            assert!(targetname_arg(&bad).is_err(), "{bad}");
        }
        for bad in [
            json!({"classname": "trigger_once"}),
            json!({"spawnflags": "1"}),
            json!({"targetname": "t"}),
            json!({"origin": "0 0 0"}),
            json!({"angles": "0 0 0"}),
            json!({"scales": "1 1 1"}),
            json!({"wait": [1]}),
        ] {
            let err = trigger_keyvalues_arg(Some(&bad)).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}");
        }
        // a real keyvalue passes and is coerced to text
        assert_eq!(
            trigger_keyvalues_arg(Some(&json!({"wait": -1, "flag": true}))).unwrap(),
            Map::from_iter([
                ("wait".to_string(), json!("-1")),
                ("flag".to_string(), json!("1")),
            ])
        );
    }

    /// The volume argument: exactly one form, the box bounds, the duplicate/oversized id lists.
    #[test]
    fn volume_arguments() {
        assert!(matches!(
            volume_arg(&json!({"box": {"min": [0, 0, 0], "max": [8, 8, 8]}})).unwrap(),
            Volume::Built(..)
        ));
        for bad in [
            json!({}),
            json!({"box": {"min": [0, 0, 0], "max": [8, 8, 8]}, "meshNodeIDs": [1]}),
            json!({"box": {"min": [1, 0, 0], "max": [0, 8, 8]}}),
            json!({"box": {"min": [0, 0, 0], "max": [0.5, 8, 8]}}),
            json!({"box": {"min": [0, 0, 0], "max": [8, 8, 2000000]}}),
            json!({"box": {"min": [0, 0, 0], "max": [8, 8, "8"]}}),
            json!({"box": {"min": [0, 0, 0], "max": [8, 8]}}),
            json!({"box": {"min": [0, 0, 0], "max": [8, 8, 8], "nope": 1}}),
            json!({"meshNodeIDs": []}),
            json!({"meshNodeIDs": [1, 1]}),
            json!({"meshNodeIDs": ["1"]}),
            json!({"meshNodeIDs": (0..=64).collect::<Vec<i64>>()}),
        ] {
            let err = volume_arg(&bad).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}: {err:?}");
        }
        // the rounding and the face gates moved to `mesh_live::send_geometry` (A1): `volume_arg`
        // only adopts the geometry (the degenerate geometry is refused there, before any send)
        assert!(matches!(
            geometry_arg(&json!({
                "vertices": [[0, 0, 0], [8, 0, 0], [0, 8, 0], [0, 0, 8]],
                "faces": [[0, 2, 1], [0, 1, 3], [1, 2, 3], [0, 3, 2]]
            }))
            .unwrap(),
            Volume::Built(..)
        ));
    }

    /// The bridge's tie/untie refusals in this module's own vocabulary.
    #[test]
    fn tie_refusals_are_mapped() {
        let already =
            BridgeError::new(cs2_mcp_common::bridge::Kind::ProtocolError, "InvalidParams")
                .with_code(-32602)
                .with_data(
                    json!({"reason": "nodeID 7 is already tied to nodeID 30", "nodeID": 7,
                              "parentNodeID": 30}),
                );
        let tool = tie_refusal(&already, TieTarget::Class("trigger_once"), &[7]).expect("mapped");
        assert_eq!(tool.name, "MeshAlreadyTied");
        assert_eq!(tool.data["parentNodeID"], json!(30));
        assert_eq!(tool.data["nodeID"], json!(7));
        assert!(tool.retry_safe);
        assert!(
            tool.message.contains("nodeID 7"),
            "the bridge's own id is named: {}",
            tool.message
        );
        assert!(
            !tool.message.contains("nothing was sent"),
            "the request WAS sent: {}",
            tool.message
        );

        // A3: the bridge's `{reason, nodeID}` shape (the mock's now, too) names the node
        let not_mesh =
            BridgeError::new(cs2_mcp_common::bridge::Kind::ProtocolError, "InvalidParams")
                .with_code(-32602)
                .with_data(json!({"reason": "not a mesh", "nodeID": 3}));
        let tool = tie_refusal(&not_mesh, TieTarget::Class("trigger_once"), &[3]).unwrap();
        assert_eq!(tool.name, "NotAMesh");
        assert_eq!(tool.data["nodeID"], json!(3));
        assert!(tool.message.contains("nodeID 3"), "{}", tool.message);

        // a named mesh that is not in the document
        let missing = BridgeError::new(cs2_mcp_common::bridge::Kind::NodeNotFound, "NodeNotFound")
            .with_code(-32022)
            .with_data(json!({"reason": "nodeID 999 not found", "nodeID": 999}));
        let tool = tie_refusal(&missing, TieTarget::Class("trigger_once"), &[999]).unwrap();
        assert_eq!(tool.name, "NodeNotFound");
        assert_eq!(tool.data["meshNodeIDs"], json!([999]));
        assert_eq!(tool.data["nodeID"], json!(999));
        assert!(tool.message.contains("mesh nodeID 999"), "{}", tool.message);

        // A3: an entityID the bridge could not find carries no nodeID, so the target is what gets
        // named
        let missing_entity =
            BridgeError::new(cs2_mcp_common::bridge::Kind::NodeNotFound, "NodeNotFound")
                .with_code(-32022)
                .with_data(json!({"reason": "nodeID 999 not found"}));
        let tool = tie_refusal(&missing_entity, TieTarget::Existing(999), &[41]).unwrap();
        assert_eq!(tool.name, "NodeNotFound");
        assert_eq!(tool.data["entityID"], json!(999));
        assert!(
            tool.data.get("nodeID").is_none(),
            "no mesh is named for a missing entityID: {tool:?}"
        );
        assert!(
            tool.message
                .contains("entityID 999 is not in the live document"),
            "{}",
            tool.message
        );
        // ... and with no data at all the class form still names the ids that were sent
        let tool = tie_refusal(&missing_entity, TieTarget::Class("trigger_once"), &[41]).unwrap();
        assert!(
            tool.message.contains("one of the meshes (41)"),
            "{}",
            tool.message
        );

        // an unmapped error keeps the wire taxonomy
        let other =
            BridgeError::new(cs2_mcp_common::bridge::Kind::ProtocolError, "x").with_code(-32603);
        assert!(tie_refusal(&other, TieTarget::Class("trigger_once"), &[1]).is_none());

        let point = BridgeError::new(cs2_mcp_common::bridge::Kind::ProtocolError, "InvalidParams")
            .with_code(-32602)
            .with_data(json!({"reason": "not a brush entity: the node has no mesh children"}));
        assert_eq!(untie_refusal(&point, 30).unwrap().name, "NotABrushEntity");
        // the tie's own "not a brush entity" (an entityID of a point entity, §5) maps the same way
        let point_tie =
            BridgeError::new(cs2_mcp_common::bridge::Kind::ProtocolError, "InvalidParams")
                .with_code(-32602)
                .with_data(json!({"reason": "entityID 61 is not a brush entity"}));
        let tool = tie_refusal(&point_tie, TieTarget::Existing(61), &[41]).unwrap();
        assert_eq!(tool.name, "NotABrushEntity");
        assert_eq!(tool.code, Some(-32602));
        assert!(tool.retry_safe);
        assert_eq!(tool.data["meshNodeIDs"], json!([41]));

        // the in-group solid-class refusal (FIX-B's `data.undoGroupOpened`) is counted
        let gdtype = BridgeError::new(cs2_mcp_common::bridge::Kind::ProtocolError, "InvalidParams")
            .with_code(-32602)
            .with_data(json!({"reason": "not a solid (brush) class", "undoGroupOpened": true}));
        let tool = tie_refusal(&gdtype, TieTarget::Class("func_vip_zone"), &[41]).unwrap();
        assert_eq!(tool.name, "NotASolidClass");
        assert_eq!(tool.data["undoGroupOpened"], json!(true));
        assert_eq!(tool.data["class"], json!("func_vip_zone"));
        assert!(tool.message.contains("func_vip_zone"), "{}", tool.message);
    }

    /// The half-built call's error: the bridge's own refusal stands when nothing landed, and
    /// everything after the first write becomes a `PartialFailure` naming what did land. A bridge
    /// refusal that names an opened group counts too (FIX-B's in-group failure), and an
    /// outcome-unknown failure reports the document's own undo depth with the label count as its
    /// minimum (A5).
    #[test]
    fn progress_failure_is_partial_only_after_a_write() {
        let state = test_state();
        let mut progress = Progress::new(None);
        let plain = ToolError::new("NodeNotFound", "NodeNotFound", "nothing yet");
        let mapped = progress.failure(&state, "entity.tieMeshes", plain.clone(), None);
        assert_eq!(
            mapped.name, "NodeNotFound",
            "nothing landed: the wire error stands"
        );

        progress.mesh = Some(7);
        progress.labels.push("MCP: create mesh".to_string());
        let partial = progress.failure(&state, "entity.tieMeshes", plain.clone(), None);
        assert_eq!(partial.name, "PartialFailure");
        assert_eq!(partial.code, Some(-32012));
        assert!(!partial.retry_safe, "a retry would create a second volume");
        assert_eq!(partial.data["meshNodeID"], json!(7));
        assert_eq!(partial.data["undoGroups"], json!(1));
        assert!(partial.data.get("undoGroupsMinimum").is_none());
        assert!(partial.message.contains("[7]"), "{}", partial.message);
        assert!(
            partial.message.contains("undo_last 1"),
            "{}",
            partial.message
        );

        // an outcome-unknown failure with the document's depth read: the exact count is reported
        // and the label count stays as the minimum
        let mut unknown = ToolError::new("InternalError", "InternalError", "may have landed");
        unknown.outcome_unknown = true;
        let mut progress = Progress::new(Some(DocState {
            doc_id: Some("d1".into()),
            serial: 0,
            depth: 0,
        }));
        progress.mesh = Some(7);
        progress.labels.push("MCP: create mesh".to_string());
        let post = DocState {
            doc_id: Some("d1".into()),
            serial: 2,
            depth: 2,
        };
        let partial = progress.failure(&state, "entity.tieMeshes", unknown, Some(post));
        assert_eq!(partial.data["undoGroups"], json!(2), "{partial:?}");
        assert_eq!(partial.data["undoGroupsMinimum"], json!(1));
        assert!(partial.outcome_unknown);
        assert!(
            partial.message.contains("moved by 2"),
            "the revert instruction names the real depth: {}",
            partial.message
        );

        // ... and without a readable depth it says "at least N"
        let mut unknown = ToolError::new("InternalError", "InternalError", "may have landed");
        unknown.outcome_unknown = true;
        let mut progress = Progress::new(Some(DocState {
            doc_id: Some("d1".into()),
            serial: 0,
            depth: 0,
        }));
        progress.mesh = Some(7);
        progress.labels.push("MCP: create mesh".to_string());
        let partial = progress.failure(&state, "entity.tieMeshes", unknown, None);
        assert_eq!(partial.data["undoGroups"], json!(1));
        assert_eq!(partial.data["undoGroupsMinimum"], json!(1));
        assert!(
            partial.message.contains("at least 1"),
            "{}",
            partial.message
        );
        assert!(
            partial.message.contains("session_status"),
            "{}",
            partial.message
        );

        // an in-group refusal with nothing of this call in the document still counts its group
        let progress = Progress::new(None);
        let opened = ToolError::new("NotASolidClass", "NotASolidClass", "the group was opened")
            .with_data(json!({"undoGroupOpened": true}));
        let partial = progress.failure(&state, "entity.tieMeshes", opened, None);
        assert_eq!(partial.name, "PartialFailure");
        assert_eq!(partial.data["undoGroups"], json!(1));
        assert_eq!(partial.data["undoGroupOpened"], json!(true));
        assert_eq!(partial.data["createdNodes"], json!([]));
        assert!(partial.retry_safe, "nothing of the call exists: retrySafe");
    }

    fn test_state() -> HammerState {
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: cs2_mcp_common::config::CommonConfig,
        }
        let argv: Vec<&str> = vec![
            "t",
            "--project-root",
            "/nonexistent/cs2mcp-trigger-test",
            "--podman",
            "/nonexistent/cs2mcp-trigger-test-podman",
            "--vk-layer-dir",
            "/nonexistent/cs2mcp-trigger-test-vklayer",
            "--xauthority",
            "/nonexistent/cs2mcp-trigger-test-xauth",
        ];
        HammerState::from_config(&W::parse_from(argv).c).expect("state")
    }
}
