//! `apply_layout`: build many meshes (and entities) from ONE JSON layout, validated as a whole
//! before the first bridge request. The agent used to paste a generator script's output into
//! `create_block` / `create_mesh` by hand, one call per mesh (ti_convoy FIXME 8, 13).
//!
//! Format (documented fully in `hammer-mcp/AGENT.md`):
//!
//! ```json
//! {"meshes":   [{"kind":"block", "id":"road", ...create_block args...},
//!               {"kind":"mesh",  "id":"ramp", ...create_mesh args...}],
//!  "entities": [{"id":"spawn", ...create_entities item...}]}
//! ```
//!
//! Every item takes exactly the arguments the single tool takes: the validators are the single
//! tools' own ([`mesh_live::prepare_create_block`], [`mesh_live::prepare_create_mesh`],
//! [`mesh_live::prepare_wire`] for the local geometry gates, [`entities_live::create_op`] for an
//! entity), plus an optional caller-chosen `id` that the result maps to the created `nodeID`.
//!
//! **Batching, as the bridge has it.** `edit.batch` runs only `entity.create` and
//! `entity.setKeyValues` ops (`crates/cs2-bridge/src/ops/edit.rs`, the per-op dispatch), and
//! `mesh.create` opens its own undo group per call (`ops/mesh.rs`, `op_mesh_create`). So the
//! meshes cannot share a group today: each is one `mesh.create` request (its own group, labelled
//! `MCP: create mesh` by the bridge) and all the entities are ONE `edit.batch` (one group,
//! `MCP: batch`). The result reports `undoGroups` (= meshes + 1 when there are entities) and
//! `undo_last` pops one group per call. `name` is echoed as `requestedUndoLabel`: the bridge takes
//! no label from the request (workflows/streamline/FIXME.md, "apply_layout undo groups").
//!
//! Everything is validated first and every bad item is reported (index + id); a bad layout, a
//! `dryRun`, or a layout over the limits sends nothing. A failure mid-way reports what was
//! created and whether the failed request may have landed (`live_common`'s taxonomy).
use super::entities_live::{
    batch_failure, batch_maybe_executed, create_op, op_keyvalues, result_items,
    short_batch_failure, FgdSource,
    BATCH_UNDO_LABEL, MAX_ENTITIES,
};
use super::live_common::{
    bool_arg, doc_id_of, expect_doc, invalid, mutator_failure, note_edit, reject_unknown_keys,
    require_document,
};
use super::mesh_live::{self, Wire};
use super::sound_check;
use crate::geometry::{self, Vec3};
use crate::state::HammerState;
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// The layout tool's order, after [`super::game_live::GAME_TOOL_ORDER`] (contract tests pin it).
pub const LAYOUT_TOOL_ORDER: [&str; 1] = ["apply_layout"];

/// Most meshes one layout may hold: each is one bridge request inside one tool call's budget, so a
/// bigger layout is refused up front (split it into several `apply_layout` calls).
pub const MAX_LAYOUT_MESHES: usize = 32;
/// Most bytes of `mesh.create` parameters one layout may send in total.
const MAX_LAYOUT_BYTES: usize = 4 * 1024 * 1024;
/// A layout file bigger than this is refused unread.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// The default `name`.
const DEFAULT_NAME: &str = "MCP: apply layout";
const MAX_ID_BYTES: usize = 64;
const MAX_REPORTED_ERRORS: usize = 12;

const TOP_KEYS: &[&str] = &["meshes", "entities"];
const ENTITY_KEYS: &[&str] = &["id", "class", "origin", "angles", "keyValues"];

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "apply_layout",
            "Build many meshes and entities in the LIVE map from ONE JSON layout: path (a .json inside the project) or layout (inline), exactly one. \
             Format: {\"meshes\":[{\"kind\":\"block\"|\"mesh\", ...create_block / create_mesh args}], \"entities\":[create_entities items]}; any item may carry an `id` that the result maps to its nodeID. \
             Everything is validated first (errors list index/id; nothing is sent on any error); dryRun returns counts and bounds only. Max 32 meshes, 64 entities, 4 MiB mesh parameters per call. \
             Each mesh is its own undo group plus one for the entities (result undoGroups = undo_last calls to remove it). A mid-way failure reports what was created. Never saves.",
            Mutating,
            &["mesh.create", "edit.batch", "entity.create"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "a .json layout inside the project (absolute, or relative to the addon / project root)"
                },
                "layout": {
                    "type": "object",
                    "description": "the same JSON inline",
                    "properties": {
                        "meshes": {
                            "type": "array",
                            "maxItems": MAX_LAYOUT_MESHES,
                            "items": {
                                "type": "object",
                                "description": "kind block|mesh plus its create_* args; optional `id`"
                            }
                        },
                        "entities": {
                            "type": "array",
                            "maxItems": MAX_ENTITIES,
                            "items": {
                                "type": "object",
                                "description": "create_entities item; optional `id`"
                            }
                        }
                    },
                    "additionalProperties": false
                },
                "dryRun": {
                    "type": "boolean",
                    "default": false,
                    "description": "validate only"
                },
                "name": {
                    "type": "string",
                    "default": DEFAULT_NAME,
                    "description": "echoed as requestedUndoLabel"
                },
                "strict": {
                    "type": "boolean",
                    "default": false
                },
                "allowUnknownSoundEvent": {
                    "type": "boolean",
                    "default": false
                }
            },
            "additionalProperties": false
        })),
        apply_layout,
    );
}

// -------------------------------------------------------------------------------------------
// Reading the layout
// -------------------------------------------------------------------------------------------

/// `path` -> a host file inside the project root. Windows paths go through the shared path map; a
/// relative path is tried under the active addon's content dir, then the project root. Nothing
/// outside the (canonical) project root is ever read, symlinks included.
fn resolve_layout_path(state: &HammerState, input: &str) -> Result<PathBuf, ToolError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(invalid("path is empty"));
    }
    if input.split(['/', '\\']).any(|segment| segment == "..") {
        return Err(invalid(format!("path contains `..`: {input:?}")));
    }
    let bytes = input.as_bytes();
    let windows = input.starts_with("\\\\")
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':');
    let candidate = if windows {
        state.path_map.windows_to_host(input).ok_or_else(|| {
            invalid(format!(
                "windows path {input:?} has no host mapping (check --path-map)"
            ))
        })?
    } else if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        let rel = input.replace('\\', "/");
        let in_addon = state.content().addon_root().join(&rel);
        if in_addon.exists() {
            in_addon
        } else {
            state.project_root.join(&rel)
        }
    };
    let root = std::fs::canonicalize(&state.project_root).map_err(|error| {
        ToolError::internal(format!(
            "the project root {} cannot be resolved: {error}",
            state.project_root.display()
        ))
    })?;
    let real = std::fs::canonicalize(&candidate)
        .map_err(|error| invalid(format!("{} cannot be read: {error}", candidate.display())))?;
    if !real.starts_with(&root) {
        return Err(invalid(format!(
            "{} is outside the project root {}: a layout file must live inside the project",
            real.display(),
            root.display()
        )));
    }
    let is_json = real
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
    if !is_json {
        return Err(invalid(format!("{} is not a .json file", real.display())));
    }
    Ok(real)
}

fn load_layout(state: &HammerState, args: &Value) -> Result<Value, ToolError> {
    let path = args.get("path").filter(|value| !value.is_null());
    let layout = args.get("layout").filter(|value| !value.is_null());
    match (path, layout) {
        (Some(_), Some(_)) => Err(invalid(
            "give exactly one of `path` and `layout`, not both",
        )),
        (None, None) => Err(invalid(
            "give exactly one of `path` (a .json file in the project) and `layout` (the JSON inline)",
        )),
        (None, Some(layout)) => {
            if layout.is_object() {
                Ok(layout.clone())
            } else {
                Err(invalid(format!("layout must be an object, got {layout}")))
            }
        }
        (Some(path), None) => {
            let Some(path) = path.as_str() else {
                return Err(invalid(format!("path must be a string, got {path}")));
            };
            let real = resolve_layout_path(state, path)?;
            let size = std::fs::metadata(&real)
                .map_err(|error| invalid(format!("{} cannot be read: {error}", real.display())))?
                .len();
            if size > MAX_FILE_BYTES {
                return Err(invalid(format!(
                    "{} is {size} bytes, over the {MAX_FILE_BYTES}-byte layout limit",
                    real.display()
                )));
            }
            let text = std::fs::read_to_string(&real)
                .map_err(|error| invalid(format!("{} cannot be read: {error}", real.display())))?;
            let layout: Value = serde_json::from_str(&text)
                .map_err(|error| invalid(format!("{} is not valid JSON: {error}", real.display())))?;
            if layout.is_object() {
                Ok(layout)
            } else {
                Err(invalid(format!(
                    "{} must hold a JSON object {{\"meshes\":[..],\"entities\":[..]}}",
                    real.display()
                )))
            }
        }
    }
}

// -------------------------------------------------------------------------------------------
// Validation
// -------------------------------------------------------------------------------------------

struct MeshPlan {
    index: usize,
    id: Option<String>,
    kind: &'static str,
    shape: Option<&'static str>,
    wire: Wire,
    bounds: (Vec3, Vec3),
    material: String,
}

struct EntityPlan {
    index: usize,
    id: Option<String>,
    op: Value,
}

struct Problem {
    section: &'static str,
    index: usize,
    id: Option<String>,
    message: String,
}

impl Problem {
    fn json(&self) -> Value {
        json!({"section": self.section, "index": self.index, "id": self.id, "message": self.message})
    }
}

/// An item's optional `id`: a non-empty string of at most [`MAX_ID_BYTES`] bytes.
fn item_id(item: &Map<String, Value>) -> Result<Option<String>, String> {
    match item.get("id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) if !id.trim().is_empty() && id.len() <= MAX_ID_BYTES => {
            Ok(Some(id.clone()))
        }
        Some(other) => Err(format!(
            "id must be a non-empty string of at most {MAX_ID_BYTES} bytes, got {other}"
        )),
    }
}

struct Plan {
    meshes: Vec<MeshPlan>,
    entities: Vec<EntityPlan>,
}

/// Validate the whole layout with the single tools' own validators; every bad item is collected.
async fn validate(
    state: &HammerState,
    layout: &Value,
    strict: bool,
    allow_unknown_sound: bool,
) -> Result<(Plan, Vec<String>), ToolError> {
    let Some(top) = layout.as_object() else {
        return Err(invalid("the layout must be an object"));
    };
    if let Some(unknown) = top.keys().find(|key| !TOP_KEYS.contains(&key.as_str())) {
        return Err(invalid(format!(
            "unknown layout key {unknown:?}: a layout has \"meshes\" and \"entities\""
        )));
    }
    let list = |key: &str| -> Result<Vec<Value>, ToolError> {
        match top.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => Ok(items.clone()),
            Some(other) => Err(invalid(format!("{key} must be an array, got {other}"))),
        }
    };
    let meshes = list("meshes")?;
    let entities = list("entities")?;
    if meshes.is_empty() && entities.is_empty() {
        return Err(invalid("the layout is empty: it has no meshes and no entities"));
    }
    // limits first: a layout that cannot fit one call is refused whole, never half applied
    if meshes.len() > MAX_LAYOUT_MESHES {
        return Err(invalid(format!(
            "the layout has {} meshes, at most {MAX_LAYOUT_MESHES} fit in one apply_layout call \
             (one mesh.create request each, inside one call's budget): split it into several calls",
            meshes.len()
        )));
    }
    if entities.len() > MAX_ENTITIES {
        return Err(invalid(format!(
            "the layout has {} entities, at most {MAX_ENTITIES} fit in one edit.batch: split it \
             into several calls",
            entities.len()
        )));
    }

    let mut problems: Vec<Problem> = Vec::new();
    let mut seen_ids: Vec<String> = Vec::new();
    let mut claim_id = |section: &'static str, index: usize, id: &Option<String>, problems: &mut Vec<Problem>| {
        if let Some(id) = id {
            if seen_ids.contains(id) {
                problems.push(Problem {
                    section,
                    index,
                    id: Some(id.clone()),
                    message: format!("id {id:?} is used more than once in this layout"),
                });
            } else {
                seen_ids.push(id.clone());
            }
        }
    };

    let mut mesh_plans: Vec<MeshPlan> = Vec::new();
    let mut total_bytes = 0usize;
    for (index, item) in meshes.iter().enumerate() {
        let Some(object) = item.as_object() else {
            problems.push(Problem {
                section: "meshes",
                index,
                id: None,
                message: format!("meshes[{index}] must be an object, got {item}"),
            });
            continue;
        };
        let id = match item_id(object) {
            Ok(id) => id,
            Err(message) => {
                problems.push(Problem {
                    section: "meshes",
                    index,
                    id: None,
                    message: format!("meshes[{index}]: {message}"),
                });
                None
            }
        };
        claim_id("meshes", index, &id, &mut problems);
        let kind = match object.get("kind").and_then(Value::as_str) {
            Some("block") => "block",
            Some("mesh") => "mesh",
            other => {
                problems.push(Problem {
                    section: "meshes",
                    index,
                    id,
                    message: format!(
                        "meshes[{index}].kind must be \"block\" or \"mesh\", got {}",
                        other.map_or_else(|| "nothing".to_string(), |kind| format!("{kind:?}"))
                    ),
                });
                continue;
            }
        };
        // the item minus `kind`/`id` is exactly the single tool's argument object
        let mut args = object.clone();
        args.remove("kind");
        args.remove("id");
        let args = Value::Object(args);
        let prepared = if kind == "block" {
            mesh_live::prepare_create_block(&args)
                .map(|(payload, material, shape, _names)| (payload, material, Some(shape)))
        } else {
            mesh_live::prepare_create_mesh(&args)
                .map(|(payload, material, _flipped)| (payload, material, None))
        };
        let (payload, material, shape) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                problems.push(Problem {
                    section: "meshes",
                    index,
                    id,
                    message: format!("meshes[{index}] ({kind}): {}", error.message),
                });
                continue;
            }
        };
        match mesh_live::prepare_wire(&payload, &material, None) {
            Ok(wire) => {
                total_bytes += Value::Object(wire.params.clone()).to_string().len();
                let bounds = geometry::bounds(&wire.vertices);
                mesh_plans.push(MeshPlan {
                    index,
                    id,
                    kind,
                    shape,
                    wire,
                    bounds,
                    material,
                });
            }
            Err(error) => problems.push(Problem {
                section: "meshes",
                index,
                id,
                message: format!("meshes[{index}] ({kind}): {}", error.message),
            }),
        }
    }
    if total_bytes > MAX_LAYOUT_BYTES && problems.is_empty() {
        return Err(invalid(format!(
            "the layout's meshes serialise to {total_bytes} bytes of bridge parameters, over the \
             {MAX_LAYOUT_BYTES}-byte limit of one apply_layout call: split it into several calls"
        )));
    }

    let fgd = FgdSource::load(state);
    let mut warnings: Vec<String> = fgd.warning().into_iter().collect();
    let mut entity_plans: Vec<EntityPlan> = Vec::new();
    for (index, item) in entities.iter().enumerate() {
        let Some(object) = item.as_object() else {
            problems.push(Problem {
                section: "entities",
                index,
                id: None,
                message: format!("entities[{index}] must be an object, got {item}"),
            });
            continue;
        };
        let id = match item_id(object) {
            Ok(id) => id,
            Err(message) => {
                problems.push(Problem {
                    section: "entities",
                    index,
                    id: None,
                    message: format!("entities[{index}]: {message}"),
                });
                None
            }
        };
        claim_id("entities", index, &id, &mut problems);
        if let Err(error) = reject_unknown_keys(item, ENTITY_KEYS) {
            problems.push(Problem {
                section: "entities",
                index,
                id,
                message: format!("entities[{index}]: {}", error.message),
            });
            continue;
        }
        let (op, mut notes) = match create_op(&entities, index, &fgd) {
            Ok(pair) => pair,
            Err(error) => {
                problems.push(Problem {
                    section: "entities",
                    index,
                    id,
                    message: error.message,
                });
                continue;
            }
        };
        let class = op["params"]["class"].as_str().unwrap_or_default().to_string();
        let keyvalues = op_keyvalues(&op).cloned().unwrap_or_default();
        match sound_check::check_sound_keyvalues(
            state,
            &fgd,
            &class,
            &keyvalues,
            &[],
            allow_unknown_sound,
        )
        .await
        {
            Ok(messages) => notes.messages.extend(messages),
            Err(error) => {
                problems.push(Problem {
                    section: "entities",
                    index,
                    id,
                    message: format!("entities[{index}]: {}", error.message),
                });
                continue;
            }
        }
        if strict && notes.has_errors() {
            problems.push(Problem {
                section: "entities",
                index,
                id,
                message: format!(
                    "entities[{index}] failed the FGD keyvalue check (strict): {}",
                    notes.errors.join("; ")
                ),
            });
            continue;
        }
        warnings.extend(
            notes
                .messages
                .iter()
                .map(|message| format!("entities[{index}]: {message}")),
        );
        entity_plans.push(EntityPlan { index, id, op });
    }

    if !problems.is_empty() {
        problems.sort_by_key(|problem| (problem.section != "meshes", problem.index));
        let shown: Vec<String> = problems
            .iter()
            .take(MAX_REPORTED_ERRORS)
            .map(|problem| problem.message.clone())
            .collect();
        let more = problems.len().saturating_sub(MAX_REPORTED_ERRORS);
        let mut message = format!(
            "the layout has {} invalid item(s); nothing was sent: {}",
            problems.len(),
            shown.join(" | ")
        );
        if more > 0 {
            message.push_str(&format!(" | ... and {more} more (see data.errors)"));
        }
        return Err(invalid(message).with_data(json!({
            "errors": problems.iter().map(Problem::json).collect::<Vec<_>>(),
            "errorCount": problems.len(),
            "meshesChecked": meshes.len(),
            "entitiesChecked": entities.len(),
            "sent": false,
        })));
    }
    Ok((
        Plan {
            meshes: mesh_plans,
            entities: entity_plans,
        },
        warnings,
    ))
}

/// The union bounds of every mesh's sent vertices and every entity origin.
fn plan_bounds(plan: &Plan) -> Value {
    let mut min: Option<Vec3> = None;
    let mut max: Option<Vec3> = None;
    let mut add = |lo: Vec3, hi: Vec3| {
        let mn = min.get_or_insert(lo);
        let mx = max.get_or_insert(hi);
        for axis in 0..3 {
            mn[axis] = mn[axis].min(lo[axis]);
            mx[axis] = mx[axis].max(hi[axis]);
        }
    };
    for mesh in &plan.meshes {
        add(mesh.bounds.0, mesh.bounds.1);
    }
    for entity in &plan.entities {
        if let Some(origin) = entity.op["params"]["origin"].as_array() {
            let point: Vec<f64> = origin.iter().filter_map(Value::as_f64).collect();
            if point.len() == 3 {
                add([point[0], point[1], point[2]], [point[0], point[1], point[2]]);
            }
        }
    }
    match (min, max) {
        (Some(min), Some(max)) => json!({"min": min, "max": max}),
        _ => Value::Null,
    }
}

fn mesh_item_json(mesh: &MeshPlan) -> Value {
    json!({
        "section": "meshes", "index": mesh.index, "id": mesh.id, "kind": mesh.kind,
        "shape": mesh.shape, "material": mesh.material,
        "vertexCount": mesh.wire.vertices.len(), "faceCount": mesh.wire.sent_faces,
        "bounds": {"min": mesh.bounds.0, "max": mesh.bounds.1},
    })
}

fn undo_groups_of(meshes: usize, entities: usize) -> usize {
    meshes + usize::from(entities > 0)
}

// -------------------------------------------------------------------------------------------
// The tool
// -------------------------------------------------------------------------------------------

fn name_arg(args: &Value) -> Result<String, ToolError> {
    match args.get("name") {
        None | Some(Value::Null) => Ok(DEFAULT_NAME.to_string()),
        Some(Value::String(name)) if !name.trim().is_empty() && name.len() <= 100 => {
            Ok(name.trim().to_string())
        }
        Some(other) => Err(invalid(format!(
            "name must be a non-empty string of at most 100 bytes, got {other}"
        ))),
    }
}

/// What a mid-way failure reports: what exists already, what was not attempted, and how to take
/// the layout back out. `undo_groups` counts only groups that certainly exist; a failed request
/// that may have landed is `failed_may_exist` instead (and is not in `not_sent`).
struct Progress {
    created: Vec<Value>,
    undo_groups: usize,
    not_sent: usize,
    failed: Value,
    failed_may_exist: bool,
}

fn with_progress(mut error: ToolError, state: &HammerState, progress: &Progress) -> ToolError {
    let groups = progress.undo_groups;
    error.data.insert("created".into(), Value::Array(progress.created.clone()));
    error.data.insert("createdCount".into(), json!(progress.created.len()));
    error.data.insert("failedItem".into(), progress.failed.clone());
    error
        .data
        .insert("failedItemMayExist".into(), json!(progress.failed_may_exist));
    error.data.insert("notSent".into(), json!(progress.not_sent));
    error.data.insert("undoGroups".into(), json!(groups));
    error
        .data
        .insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    let mut recovery = if groups == 0 {
        "nothing from this layout is certain to exist".to_string()
    } else {
        format!(
            "call undo_last {groups} time(s) to remove what was created (or delete_nodes on the \
             listed nodeIDs)"
        )
    };
    if progress.failed_may_exist {
        recovery.push_str(
            "; the failed request may also have landed (outcome unknown, do not blindly retry): \
             check whether failedItem exists (find_entities / list_meshes) and if so call \
             undo_last one more time",
        );
    }
    error.data.insert("recovery".into(), json!(recovery));
    error
}

/// The `next` args of a dry run: the same call with `dryRun:false` (the layout/path and the other
/// options it was given), so the suggested call is valid as written.
fn dry_run_apply_args(args: &Value) -> Value {
    let mut next = args.as_object().cloned().unwrap_or_default();
    next.insert("dryRun".into(), json!(false));
    Value::Object(next)
}

pub async fn apply_layout(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(
        &args,
        &["path", "layout", "dryRun", "name", "strict", "allowUnknownSoundEvent"],
    )?;
    let dry_run = bool_arg(&args, "dryRun")?.unwrap_or(false);
    let strict = bool_arg(&args, "strict")?.unwrap_or(false);
    let allow_unknown_sound = bool_arg(&args, "allowUnknownSoundEvent")?.unwrap_or(false);
    let name = name_arg(&args)?;
    let layout = load_layout(&state, &args)?;
    let (plan, warnings) = validate(&state, &layout, strict, allow_unknown_sound).await?;

    let mesh_count = plan.meshes.len();
    let entity_count = plan.entities.len();
    let undo_groups = undo_groups_of(mesh_count, entity_count);
    let bridge_requests = undo_groups;

    if dry_run {
        let mut items: Vec<Value> = plan.meshes.iter().map(mesh_item_json).collect();
        items.extend(plan.entities.iter().map(|entity| {
            json!({
                "section": "entities", "index": entity.index, "id": entity.id, "kind": "entity",
                "class": entity.op["params"]["class"], "origin": entity.op["params"]["origin"],
            })
        }));
        return Ok(ToolOutput::from(json!({
            "dryRun": true,
            "valid": true,
            "sent": false,
            "wouldCreate": {
                "meshes": mesh_count,
                "entities": entity_count,
                "total": mesh_count + entity_count,
            },
            "bridgeRequests": bridge_requests,
            "undoGroups": undo_groups,
            "bounds": plan_bounds(&plan),
            "items": items,
            "requestedUndoLabel": name,
            "warnings": warnings,
            "notes": ["dryRun: nothing was sent to the bridge; run again without dryRun to apply"],
        }))
        // explicit, so the mutator hook doesn't add `save_map` for edits made before the dry run
        .with_next_args(
            "apply_layout",
            "apply the layout for real",
            dry_run_apply_args(&args),
        ));
    }

    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(&state, deadline).await?;
    let doc_id = doc_id_of(&doc);

    let mut created: Vec<Value> = Vec::new();
    let mut result_warnings = warnings;
    let mut missing: Vec<String> = Vec::new();
    let mut missing_mesh = false;
    let mut missing_entity = false;
    let total_items = mesh_count + entity_count;

    // One fingerprint for the whole layout: the bridge's fingerprint (`doc_fingerprint`) is
    // structural (session, root, path), so a mesh.create does not move it; a document switch
    // mid-way is refused by the bridge's own `expectDoc` gate (`DocumentChanged`, nothing written).
    for mesh in &plan.meshes {
        let item_ref = json!({"section": "meshes", "index": mesh.index, "id": mesh.id, "kind": mesh.kind});
        // `sent`: the failed request was dispatched (so it is not "not sent"); `maybe`: it may
        // have landed
        let progress = |created: &Vec<Value>, sent: bool, maybe: bool| Progress {
            created: created.clone(),
            undo_groups: created.len(),
            not_sent: total_items - created.len() - usize::from(sent),
            failed: item_ref.clone(),
            failed_may_exist: maybe,
        };
        let deadline = match ctx.budget.deadline_ms() {
            Ok(deadline) => deadline,
            Err(error) => {
                return Err(with_progress(error, &state, &progress(&created, false, false)));
            }
        };
        let reply = {
            let mut bridge = state.bridge.lock().await;
            bridge
                .request(
                    "mesh.create",
                    Value::Object(mesh.wire.params.clone()),
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
                let error = mutator_failure(error, &state, doc_id.as_deref());
                let maybe = error.outcome_unknown;
                return Err(with_progress(error, &state, &progress(&created, true, maybe)));
            }
        };
        note_edit(&state, doc_id.as_deref());
        let node_id = reply
            .result
            .get("nodeID")
            .and_then(Value::as_i64)
            .filter(|id| *id >= 0);
        let faces = reply.result.get("faces").and_then(Value::as_i64);
        let label = mesh.id.clone().unwrap_or_else(|| format!("meshes[{}]", mesh.index));
        if node_id.is_none() {
            missing.push(label.clone());
            missing_mesh = true;
            result_warnings.push(format!(
                "{label}: the bridge reported no usable nodeID; the mesh exists, find it with \
                 find_entities / list_meshes after save_map"
            ));
        }
        if let Some(faces) = faces {
            if faces >= 0 && faces as usize != mesh.wire.sent_faces {
                result_warnings.push(format!(
                    "{label}: Hammer created {faces} of {} face(s) sent (welding or a polygon \
                     degenerate in float): the geometry differs, per-face materials may not have \
                     applied; read it back with get_mesh",
                    mesh.wire.sent_faces
                ));
            }
        }
        created.push(json!({
            "id": mesh.id,
            "kind": mesh.kind,
            "section": "meshes",
            "index": mesh.index,
            "nodeID": node_id,
            "faceCount": faces.unwrap_or(mesh.wire.sent_faces as i64),
        }));
    }

    if entity_count > 0 {
        let ops: Vec<Value> = plan.entities.iter().map(|entity| entity.op.clone()).collect();
        let first = &plan.entities[0];
        let item_ref = json!({"section": "entities", "index": first.index, "id": first.id,
                              "kind": "entity", "note": "the entity batch (all entities, one request)"});
        let progress = |created: &Vec<Value>, sent: bool, maybe: bool| Progress {
            created: created.clone(),
            undo_groups: created.iter().filter(|c| c["kind"] != "entity").count()
                + usize::from(created.iter().any(|c| c["kind"] == "entity")),
            not_sent: if sent { 0 } else { entity_count },
            failed: item_ref.clone(),
            failed_may_exist: maybe,
        };
        let deadline = match ctx.budget.deadline_ms() {
            Ok(deadline) => deadline,
            Err(error) => {
                return Err(with_progress(error, &state, &progress(&created, false, false)));
            }
        };
        let mut params = Map::new();
        params.insert("undoLabel".into(), json!(name));
        params.insert("ops".into(), Value::Array(ops.clone()));
        let reply = {
            let mut bridge = state.bridge.lock().await;
            bridge
                .request(
                    "edit.batch",
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
                let maybe = batch_maybe_executed(&error);
                if maybe {
                    note_edit(&state, doc_id.as_deref());
                }
                let error = batch_failure(error, &state, &ops);
                // entities the batch did create before it stopped (`PartialFailure` rows)
                let rows = error
                    .data
                    .get("results")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut kept = created.clone();
                for (entity, row) in plan.entities.iter().zip(rows.iter()) {
                    if row.get("error").is_some() {
                        break;
                    }
                    kept.push(entity_created(entity, row));
                }
                let landed = kept.len() > created.len();
                // rows prove the batch's group exists; with no rows a maybe-executed batch may
                // have opened one
                let may_exist = maybe && !landed;
                let mut progress = progress(&kept, true, may_exist);
                // the bridge stops at the first failing op: the ops after it were never run
                let failed_index = error
                    .data
                    .get("failedIndex")
                    .and_then(Value::as_u64)
                    .and_then(|i| usize::try_from(i).ok())
                    .filter(|&i| i < entity_count);
                if let Some(i) = failed_index {
                    let entity = &plan.entities[i];
                    progress.failed = json!({"section": "entities", "index": entity.index,
                                             "id": entity.id, "kind": "entity"});
                    progress.not_sent = entity_count - i - 1;
                }
                return Err(with_progress(error, &state, &progress));
            }
        };
        note_edit(&state, doc_id.as_deref());
        if !reply.result.is_object() {
            let error = ToolError::internal(format!(
                "edit.batch returned a non-object result: {}",
                reply.result
            ));
            return Err(with_progress(error, &state, &progress(&created, true, true)));
        }
        let results = result_items(&reply.result["results"], &ops);
        if results.len() != ops.len() {
            let mut kept = created.clone();
            for (entity, row) in plan.entities.iter().zip(results.iter()) {
                if row.get("error").is_none() {
                    kept.push(entity_created(entity, row));
                }
            }
            let landed = kept.len() > created.len();
            let error = short_batch_failure(
                &state,
                results,
                ops.len(),
                reply.result.get("failedIndex").cloned().unwrap_or(Value::Null),
            );
            return Err(with_progress(error, &state, &progress(&kept, true, !landed)));
        }
        for (entity, result) in plan.entities.iter().zip(results.iter()) {
            let label = entity.id.clone().unwrap_or_else(|| format!("entities[{}]", entity.index));
            if result.get("nodeID").and_then(Value::as_i64).is_none() {
                missing.push(label.clone());
                missing_entity = true;
                result_warnings.push(format!("{label}: the bridge reply carries no nodeID"));
            }
            if let Some(not_applied) = result.get("keyValuesNotApplied").and_then(Value::as_array) {
                if !not_applied.is_empty() {
                    result_warnings.push(format!(
                        "{label}: keyvalues not applied: {}",
                        not_applied
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            created.push(entity_created(entity, result));
        }
    }

    let output = ToolOutput::from(json!({
        "created": created,
        "count": created.len(),
        "meshes": mesh_count,
        "entities": entity_count,
        "bridgeRequests": bridge_requests,
        "undoGroups": undo_groups,
        "undoNote": format!(
            "undo_last pops one group per call: {undo_groups} call(s) remove the whole layout \
             (the bridge labels the groups \"MCP: create mesh\" per mesh and \"{BATCH_UNDO_LABEL}\" \
             for the entities; `name` is only echoed)"
        ),
        "requestedUndoLabel": name,
        "unsavedBridgeEdits": state.unsaved_edits(),
        "saved": false,
        "missingNodeIDs": missing,
        "warnings": result_warnings,
    }));
    if missing.is_empty() {
        // the registry's mutator hook adds `next: save_map`
        Ok(output)
    } else {
        let mut output = output;
        if missing_entity {
            output = output.with_next(
                "find_entities",
                "the bridge reported no nodeID for entities in missingNodeIDs: find them (by \
                 class/origin) before editing them",
            );
        }
        if state.document_untitled() {
            // save_map refuses an untitled document (same hint as the mutator hook)
            // an explicit (possibly empty) list keeps the hook from adding save_map
            output.next.get_or_insert_with(Vec::new);
            if let Some(m) = output.json.as_object_mut() {
                m.entry("hint").or_insert(json!(
                    "the document is untitled: save_map refuses it; save it once from Hammer's \
                     File menu before save_map can persist these edits"
                ));
            }
        } else {
            output = output.with_next("save_map", "write the layout into the .vmap");
            if missing_mesh {
                output = output.with_next(
                    "list_meshes",
                    "after save_map: find the meshes in missingNodeIDs in the saved .vmap",
                );
            }
        }
        Ok(output)
    }
}

/// One created entity as the result lists it.
fn entity_created(entity: &EntityPlan, row: &Value) -> Value {
    json!({
        "id": entity.id,
        "kind": "entity",
        "section": "entities",
        "index": entity.index,
        "nodeID": row.get("nodeID").and_then(Value::as_i64),
        "class": entity.op["params"]["class"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_groups_count_one_per_mesh_plus_one_entity_batch() {
        assert_eq!(undo_groups_of(0, 0), 0);
        assert_eq!(undo_groups_of(3, 0), 3);
        assert_eq!(undo_groups_of(3, 1), 4);
        assert_eq!(undo_groups_of(0, 5), 1);
    }

    #[test]
    fn ids_are_short_non_empty_strings() {
        let ok = json!({"id": "road"});
        assert_eq!(item_id(ok.as_object().unwrap()).unwrap(), Some("road".to_string()));
        assert_eq!(item_id(json!({}).as_object().unwrap()).unwrap(), None);
        for bad in [json!({"id": ""}), json!({"id": 7}), json!({"id": "x".repeat(65)})] {
            assert!(item_id(bad.as_object().unwrap()).is_err(), "{bad}");
        }
    }
}
