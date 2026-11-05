//! Live entity mutators through the bridge: `create_entities` (`edit.batch` of `entity.create`),
//! `set_entity_keyvalues` (`entity.setKeyValues` by nodeID, m2.15) and `undo_last`. Keyvalues are
//! always sent as strings (Hammer stores every FGD keyvalue as a DME string, m2.15 §3.4) and are
//! checked against the FGD class database first. Plan: target/entities-plan.md WP-T2a.
//!
//! Policies ported from the POC `h_create_entities` / `h_undo_last`
//! (`idabins/worknotes/mcpserver/server.py`), plus this file's own additions (FGD precheck,
//! string coercion, `set_entity_keyvalues`):
//!
//! * **`create_entities`**: 1..=64 entities, locally validated (and FGD-checked) **before** the
//!   single `edit.batch` — an unknown class, a `classname` key, a scalar-coercion failure, a
//!   size-limit violation or a `strict` FGD issue refuses with **nothing sent**. One undo group
//!   (the bridge labels a batch with a create op `"MCP: batch"` and a set-only one
//!   `"MCP: set keyvalues"` — its `batch_has_create`, decided on the listed ops before the group
//!   opens (crates/cs2-bridge/PROTOCOL.md §10); this tool sends only creates, so the label
//!   is `"MCP: batch"`, and the m2.15 `edit.batch` arm takes no `undoLabel` — the requested label
//!   is echoed as `requestedUndoLabel` and the bridge's own label as `undoLabel`).
//!   Raw `handle`s from the result are replaced by `handleIssued:true`, and the bridge's create
//!   reply now also carries the created node's `nodeID` (the id `find_entities`/`entity.get`
//!   address it by) which the result keeps; the batch is never saved.
//!   `-32012 PartialFailure` passes through with `results`/`failedIndex` (sanitized of handles) and
//!   the `_batch_maybe_executed` rule bumps the unsaved counter, because some ops may have run.
//! * **`set_entity_keyvalues`**: the same coercion/limits, then `entity.get` (declared on the spec)
//!   for the live classname, the FGD check on it, and `entity.setKeyValues {nodeID, values,
//!   expectDoc}`. The result carries `previous` and the `valueUndo` marker the bridge answered:
//!   the m2.15..m2.17 gap is `valueUndo:"group-only"` (`undo_last` pops the `"MCP: set
//!   keyvalues"` group **without** reverting the values, `revertHint` says how to revert), and
//!   since m2.18 (`RecordNodeTree`) the reply can say `valueUndo:"recorded"` — `previous` is
//!   returned in both shapes and `revertHint` follows the marker, so `undo_last` restoring the
//!   values is the documented path on a build that records. An FGD class the live entity's
//!   class is missing from is a warning, not a refusal: the entity exists, only the keyvalue
//!   precheck is unavailable. An **empty** value (`""`) is an ordinary value: it is sent as
//!   `""`, the bridge writes Hammer's empty symbol (a NULL `CUtlSymbolLarge` — the same thing
//!   `entity.get` reads back as `""`) and answers `applied:true`; before m2.16 a NULL symbol
//!   silently skipped the write (`applied:false` with no warning at all).
//!   **WP-R1 model check**: a `model` keyvalue on a prop-tagged class is normalized and
//!   catalog-looked-up exactly like `get_model_info`/`create_props` do, **before** anything is
//!   written — an unknown model is `ModelNotFound` (with the catalog's suggestions, props_live's
//!   shared `resolve`), because a prop whose `model` the catalog never heard of draws Hammer's
//!   white placeholder box. `force:true` skips the check (a class/tag the FGD cannot name, or a
//!   model this catalog does not carry on purpose); a revert (the key's submitted value equals
//!   the live value) is exempt with `strict` and skips the lookup too — it writes nothing new.
//!   **WP-S3 sound check**: a keyvalue whose FGD type is `sound` (the class's `sound_keys`) goes
//!   through `sound_check::check_sound_keyvalues` before anything is written: an unknown
//!   soundevent name is `SoundEventNotFound` (a misspelt name is silent at runtime), an
//!   unbuildable index is `VpkMissing`; `force:true` writes either with a warning, and a revert
//!   (`exempt`) skips the lookup.
//! * **`create_entities`** (WP-S3, additive): `allowUnknownSoundEvent?` is the sound check's
//!   bypass (`set_entity_keyvalues` spells it `force`), off by default; the check's warnings land
//!   in the entity's own `warnings`.
//! * **`undo_last`**: `doc.active` first; an empty stack answers `undone:false` without sending
//!   anything, a top group this server did not create (an `undoName` without the `MCP:` prefix,
//!   including a missing or empty name) is refused as `UndoGroupNotOurs` unless `acceptUserEdits`,
//!   and an undo **decrements** the unsaved counter only for a group of ours that actually
//!   restores state (a popped `"MCP: set keyvalues"` group reverts nothing, so the counter stays
//!   up — the map still differs from disk; brief item 3). A popped `"MCP: set keyvalues"` group
//!   adds the note that the values were not reverted; a popped group this server did not create
//!   gets a note worded by `acceptUserEdits` — with it the user's own group was popped knowingly,
//!   without it the top group changed between the check and the undo (review L7: do not claim
//!   `acceptUserEdits` then).
//!
//! No addresses, no auto-save, no `expectDoc` invented: every mutator sends the fingerprint read
//! off `doc.active` in the same call (`-32004 DocumentChanged` otherwise).
//!
//! Bounds (brief items 1, 5–8): a keyvalue value is at most [`MAX_VALUE_BYTES`] bytes (this
//! server's own conservative cap; the bridge's `previous` value buffer is `KV_VALUE_MAX` 1024 and
//! a `previous` value over it comes back null, listed in `previousTruncated`; `entity.get` cuts
//! its live *values* at a separate, larger cap, `ENTITY_STR_CAP` 4096, listed in
//! `truncatedValues`), `nodeID`/`nodeIDs` are 0..=2147483647 (the bridge casts to
//! `int`), `origin`/`angles`/`within`/`radius` components outside `1e-4..=1e9` are snapped to 0 /
//! refused (the bridge's number scan stops at `e`), keys absent from the bridge's `applied` map
//! count as not applied, and a write that failed with a protocol/internal error is reported as
//! maybe-executed (`outcomeUnknown` + the counter bumped).
use crate::fgd;
use crate::state::HammerState;
use crate::tools::entities_query;
use cs2_mcp_common::bridge::{BridgeError, Kind, RequestOpts};
use cs2_mcp_common::tools::{
    strip_handles, CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Map, Value};

/// Review L7: the plumbing shared with `lifecycle` and `mesh_live` (the active-document read with
/// its `expectDoc` fingerprint, the unsaved-edit counter and the maybe-executed taxonomy) has one
/// home, so a cross-cutting fix reaches every live tool.
pub(crate) use super::live_common::{
    bool_arg, doc_id_of, expect_doc, invalid, maybe_executed, mutator_failure, node_id_arg,
    note_edit, note_undo, require_document, MAX_NODE_ID,
};
use super::sound_check;

/// One `edit.batch` fits at most this many entities (POC `h_create_entities`).
pub(crate) const MAX_ENTITIES: usize = 64;
/// At most this many keyvalues per entity/request (POC + plan §4.2 limits).
const MAX_KEYS: usize = 32;
/// A keyvalue name is at most this many bytes.
pub(crate) const MAX_KEY_BYTES: usize = 95;
/// A keyvalue value this server accepts is at most this many bytes: our own conservative cap
/// (opus #7), well below the bridge's current value buffer (`KV_VALUE_MAX` 1024). The bridge
/// reports a `previous` value above that buffer as null, listed in `previousTruncated`;
/// `entity.get`'s `truncatedValues` is a *different* cap — `ENTITY_STR_CAP` 4096
/// (`mcp_bridge.c`), not the 1024-byte value buffer — so nothing is silently cut.
pub(crate) const MAX_VALUE_BYTES: usize = 511;
/// A coordinate/radius magnitude the bridge's plain-decimal number scan reads exactly; anything
/// larger is refused instead of being sent as `1e20` (brief item 7).
const COORD_MAX_ABS: f64 = 1e9;
/// Below this magnitude `serde_json` writes exponent notation (`5.5e-17`), which the bridge's
/// number scan stops at (a silently partly-zero origin): such values snap to 0 (opus #8).
const COORD_MIN_ABS: f64 = 1e-4;
/// The bridge's batch op scratch is 8192 bytes (`batch_op_next` in `mcp_bridge.c`): keep each op's
/// params clearly below it.
pub(crate) const MAX_OP_BYTES: usize = 6000;

/// The undo label the m2.15 `edit.batch` arm opens for a batch with an `entity.create` op
/// (`undoLabel` is not a bridge parameter; a set-only batch is labelled `KEYVALUE_UNDO_LABEL`
/// — `batch_has_create`, 35528e8 — but this tool sends only creates).
pub(crate) const BATCH_UNDO_LABEL: &str = "MCP: batch";
/// The undo label the standalone/batch `entity.setKeyValues` opens.
const KEYVALUE_UNDO_LABEL: &str = "MCP: set keyvalues";
/// The POC's default for the (dropped) `undoLabel` argument.
const DEFAULT_UNDO_LABEL: &str = "MCP: create entities";
/// The documented undo gap (m2.15 §7 / plan F2), echoed by `set_entity_keyvalues`. A `null`
/// previous value means the key did not exist before; the bridge has no "remove keyvalue" call, so
/// the hint points at the FGD default instead of telling the caller to set `""` (which typed keys
/// would refuse) — brief item 11.
const REVERT_HINT: &str = "undo_last does not restore keyvalue values: re-apply `previous` to \
                           revert. A key whose `previous` is null did not exist before and cannot \
                           be removed through the bridge — set it to the FGD default from \
                           get_entity_class, or clear it in Hammer. A value equal to the entity's \
                           current value is exempt from the FGD type check.";
/// The hint when the reply says the bridge **does** record value undo (`valueUndo:"recorded",
/// the m2.18 RecordNodeTree path): the same scope as [`REVERT_HINT`], the other verdict.
const RECORDED_HINT: &str = "undo_last restores these keyvalue values (the bridge records value \
                             undo; the reply said valueUndo:\"recorded\"), so call undo_last to \
                             revert. A key whose `previous` is null did not exist before: undo \
                             removes it again. A value equal to the entity's current value is \
                             exempt from the FGD type check.";
/// Added to `undo_last`'s notes when the popped group was a keyvalue group on a bridge that
/// does NOT record value undo (`valueUndo:"group-only"` up to m2.17).
const KEYVALUE_UNDO_NOTE: &str = "keyvalue values are NOT reverted by undo; re-apply `previous`";
/// Added to `undo_last`'s notes when the popped group was a keyvalue group on an m2.18 build
/// (the `node.setTransform` marker is advertised = RecordNodeTree): the pop really restores
/// the values — the live fact the m2.18 bridge verified, so the tool must claim it.
const KEYVALUE_UNDO_RESTORED_NOTE: &str =
    "keyvalue values ARE restored by this undo (the bridge records value undo, m2.18 \
     RecordNodeTree): the entity — its `model` keyvalue and the drawn model included — is \
     back to the pre-write state";

pub fn register_create(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "create_entities",
            "Create entities in one undo group (never saves). Keyvalues are checked against the FGD; an unknown class, a `classname` key or a strict FGD issue refuses with nothing sent. An unknown soundevent name is refused unless allowUnknownSoundEvent.",
            Mutating,
            &["edit.batch", "entity.create"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "entities": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_ENTITIES,
                    "items": {
                        "type": "object",
                        "properties": {
                            "class": {
                                "type": "string",
                                "description": "FGD classname, e.g. info_target"
                            },
                            "origin": {
                                "type": "array",
                                "items": {"type": "number"},
                                "minItems": 3,
                                "maxItems": 3,
                                "description": "world units"
                            },
                            "angles": {
                                "type": "array",
                                "items": {"type": "number"},
                                "minItems": 3,
                                "maxItems": 3,
                                "description": "pitch, yaw, roll"
                            },
                            "keyValues": {
                                "type": "object",
                                "description": "max 32 keys, values max 511 bytes; not classname/origin/angles/scales",
                                "additionalProperties": {"type": ["string", "number", "boolean"]}
                            }
                        },
                        "required": ["class", "origin"],
                        "additionalProperties": false
                    }
                },
                "strict": {
                    "type": "boolean",
                    "default": false,
                    "description": "FGD errors refuse instead of warn"
                },
                "undoLabel": {
                    "type": "string",
                    "description": "echoed only; the bridge labels the group itself"
                },
                "allowUnknownSoundEvent": {
                    "type": "boolean",
                    "default": false,
                    "description": "write unknown soundevent names (warning only)"
                }
            },
            "required": ["entities"],
            "additionalProperties": false
        })),
        create_entities,
    );
}

pub fn register_undo(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "undo_last",
            "Undo one MCP-created undo group. Keyvalue changes are not always reverted; check the result notes. Refuses a user group unless acceptUserEdits.",
            Mutating,
            &["undo.undo"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "acceptUserEdits": {
                    "type": "boolean",
                    "default": false,
                    "description": "allow undoing a group not created by this server"
                }
            },
            "additionalProperties": false
        })),
        undo_last,
    );
}

pub fn register_set(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "set_entity_keyvalues",
            "Set keyvalues on an existing entity by nodeID (live, not saved; returns previous values). Values are checked against the FGD; an unknown sound or model name is refused unless force.",
            Mutating,
            &["entity.setKeyValues", "entity.get"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "nodeID": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_NODE_ID,
                    "description": "node id from find_entities"
                },
                "keyValues": {
                    "type": "object",
                    "description": "max 32 keys, values max 511 bytes; not classname/origin/angles/scales",
                    "additionalProperties": {"type": ["string", "number", "boolean"]}
                },
                "strict": {
                    "type": "boolean",
                    "default": false,
                    "description": "FGD errors refuse instead of warn; unchanged values are exempt"
                },
                "force": {
                    "type": "boolean",
                    "default": false,
                    "description": "skip the model/soundevent name checks (warning only)"
                }
            },
            "required": ["nodeID", "keyValues"],
            "additionalProperties": false
        })),
        set_entity_keyvalues,
    );
}

// -------------------------------------------------------------------------------------------
// Shared plumbing (the parts every live mutator has in common live in `live_common`, review L7;
// what is left here is this file's own: arguments, FGD, batch results)
// -------------------------------------------------------------------------------------------

/// The write's target check in the shared entity taxonomy (review L5): the bridge re-resolves
/// `nodeID` on `entity.setKeyValues` itself (`set_keyvalues_target` runs before `undo_open`), so
/// a node deleted or replaced between our `entity.get` and the write fails right there.
/// `read_error` maps the bridge's not-found / not-an-entity conditions to `EntityNotFound` /
/// `NotAnEntity`; "was it one of those" is checked on the mapped error's name, which a raw wire
/// error can never carry (the wire names are `NodeNotFound` / `ProtocolError`). Such a failure
/// ran nothing, so no `mutator_failure` counter/`outcomeUnknown` handling either.
fn set_target_failure(error: &BridgeError, node_id: i64) -> Option<ToolError> {
    let mapped = entities_query::read_error(error.clone(), node_id);
    matches!(mapped.name.as_str(), "EntityNotFound" | "NotAnEntity").then_some(mapped)
}

/// `undoLabel?`: a non-empty string, defaulting to the POC's `"MCP: create entities"`. The m2.15
/// bridge does not read it (a batch with a create op is `"MCP: batch"`, a set-only one
/// `"MCP: set keyvalues"`), so it is only echoed back.
fn undo_label_arg(args: &Value) -> Result<String, ToolError> {
    match args.get("undoLabel") {
        None | Some(Value::Null) => Ok(DEFAULT_UNDO_LABEL.to_string()),
        Some(Value::String(label)) if !label.trim().is_empty() => Ok(label.clone()),
        Some(other) => Err(invalid(format!(
            "undoLabel must be a non-empty string, got {other}"
        ))),
    }
}

/// One coordinate component the bridge can read back exactly: finite, `|x| <= `[`COORD_MAX_ABS`]
/// (anything larger would go out as `1e20`, which the bridge's number scan stops at), and snapped
/// to 0 below [`COORD_MIN_ABS`] (where `serde_json` switches to exponent notation).
pub(crate) fn coord_axis(label: &str, axis: usize, value: &Value) -> Result<f64, ToolError> {
    let number = value
        .as_f64()
        .filter(|axis| axis.is_finite())
        .ok_or_else(|| invalid(format!("{label}[{axis}] must be a finite number, got {value}")))?;
    if number.abs() > COORD_MAX_ABS {
        return Err(invalid(format!(
            "{label}[{axis}] is {number}: |value| > {COORD_MAX_ABS} cannot be sent (serde_json \
             would write it in exponent notation, which the bridge's number scan misreads)"
        )));
    }
    Ok(if number.abs() < COORD_MIN_ABS { 0.0 } else { number })
}

/// `[x, y, z]` of finite numbers within the bridge's exact range (see [`coord_axis`]).
pub(crate) fn vec3_arg(label: &str, value: Option<&Value>) -> Result<[f64; 3], ToolError> {
    let values = value
        .and_then(Value::as_array)
        .filter(|values| values.len() == 3)
        .ok_or_else(|| invalid(format!("{label} must be [x, y, z] numbers")))?;
    let mut out = [0.0f64; 3];
    for (axis, value) in values.iter().enumerate() {
        out[axis] = coord_axis(label, axis, value)?;
    }
    Ok(out)
}

/// One keyvalue value coerced into the text `apply_keyvalues` writes (m2.15 §3.4): a string as-is,
/// a number as the JSON text the caller wrote, a bool as `"1"`/`"0"`. `None` = not writable.
pub(crate) fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(true) => Some("1".to_string()),
        Value::Bool(false) => Some("0".to_string()),
        _ => None,
    }
}

/// `{name: text}` from caller JSON. Bounds (plan §4.2): at most [`MAX_KEYS`] keys, key names at
/// most [`MAX_KEY_BYTES`] bytes, values at most [`MAX_VALUE_BYTES`] bytes; `classname` is refused
/// (the class is `class` for a create and the entity itself for a set) and so are the
/// engine-managed `origin`/`angles`/`scales` (brief item 1: the bridge's first-match JSON scan
/// reads a `keyValues.origin` string as the entity origin and leaves the node at [0,0,0]), and
/// arrays/objects/null are not writable keyvalues.
pub(crate) fn keyvalues_arg(label: &str, value: &Value) -> Result<Map<String, Value>, ToolError> {
    let Some(object) = value.as_object() else {
        return Err(invalid(format!(
            "{label} must be an object of keyvalue scalars"
        )));
    };
    if object.len() > MAX_KEYS {
        return Err(invalid(format!(
            "{label} has {} keys, at most {MAX_KEYS} fit in one request",
            object.len()
        )));
    }
    let mut out = Map::new();
    for (key, value) in object {
        let lower = key.to_ascii_lowercase();
        if lower == "classname" {
            return Err(invalid(format!(
                "{label}.classname cannot be set: the class is the `class` field (create) or the \
                 live entity's own classname (set)"
            )));
        }
        if matches!(lower.as_str(), "origin" | "angles" | "scales") {
            return Err(invalid(format!(
                "{label}.{key} cannot be set as a keyvalue: use the origin/angles parameters \
                 (Hammer and the bridge manage those node fields)"
            )));
        }
        if key.is_empty() {
            return Err(invalid(format!("{label}: an empty key cannot be set")));
        }
        if key.chars().any(char::is_control) {
            return Err(invalid(format!(
                "{label}[{key:?}]: a key cannot contain control characters"
            )));
        }
        if key.len() > MAX_KEY_BYTES {
            return Err(invalid(format!(
                "{label}[{key:?}]: the key is {} bytes, at most {MAX_KEY_BYTES} fit",
                key.len()
            )));
        }
        let Some(text) = value_text(value) else {
            return Err(invalid(format!(
                "{label}[{key:?}] must be a string, number or bool, got {value} (arrays, objects \
                 and null are not writable keyvalues)"
            )));
        };
        if text.len() > MAX_VALUE_BYTES {
            return Err(invalid(format!(
                "{label}[{key:?}]: the value is {} bytes, at most {MAX_VALUE_BYTES} fit",
                text.len()
            )));
        }
        if text.contains('\0') {
            return Err(invalid(format!(
                "{label}[{key:?}]: a value cannot contain a NUL character (the engine stores C \
                 strings, so it would be cut there)"
            )));
        }
        out.insert(key.clone(), Value::String(text));
    }
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// FGD precheck
// -------------------------------------------------------------------------------------------

/// The FGD class database for this state, or the warning that stands in for it. An unavailable
/// database is **never** a refusal (plan §4.2): the edit proceeds with `"fgd unavailable: ..."` and
/// the result says so under `fgd.unavailable`.
pub(crate) enum FgdSource {
    Loaded(std::sync::Arc<fgd::Database>),
    Unavailable(String),
}

impl FgdSource {
    pub(crate) fn load(state: &HammerState) -> FgdSource {
        match fgd::load_for(state) {
            Ok(db) => FgdSource::Loaded(db),
            Err(error) => FgdSource::Unavailable(format!("fgd unavailable: {error}")),
        }
    }

    pub(crate) fn db(&self) -> Option<&fgd::Database> {
        match self {
            FgdSource::Loaded(db) => Some(db),
            FgdSource::Unavailable(_) => None,
        }
    }

    /// The top-level warning when the database is missing.
    pub(crate) fn warning(&self) -> Option<String> {
        match self {
            FgdSource::Loaded(_) => None,
            FgdSource::Unavailable(message) => Some(message.clone()),
        }
    }

    /// `{files, warnings}` or `{files:0, unavailable}` (plan §4.2: "source files count |
    /// unavailable"). The parser's own warnings are capped: they are not this tool's subject.
    pub(crate) fn json(&self) -> Value {
        match self {
            FgdSource::Loaded(db) => json!({
                "files": db.files.len(),
                "warnings": db.warnings.iter().take(10).collect::<Vec<_>>(),
                "warningCount": db.warnings.len(),
            }),
            FgdSource::Unavailable(message) => json!({"files": 0, "unavailable": message}),
        }
    }
}

/// The FGD issues of one entity, split the way the plan names them (`unknownKeys`/`typeIssues`)
/// with the parser's own message text kept for the warning list. `errors` holds the
/// error-severity messages only: `strict` refuses on those, warnings pass (brief item 12).
pub(crate) struct FgdNotes {
    pub(crate) messages: Vec<String>,
    pub(crate) unknown_keys: Vec<String>,
    pub(crate) type_issues: Vec<String>,
    pub(crate) errors: Vec<String>,
}

impl FgdNotes {
    pub(crate) fn from_issues(issues: &[fgd::Issue]) -> FgdNotes {
        let mut notes = FgdNotes {
            messages: Vec::new(),
            unknown_keys: Vec::new(),
            type_issues: Vec::new(),
            errors: Vec::new(),
        };
        for issue in issues {
            notes.messages.push(issue.message.clone());
            if issue.severity == fgd::Severity::Error {
                notes.errors.push(issue.message.clone());
            }
            match issue.kind {
                fgd::IssueKind::UnknownKey => notes.unknown_keys.push(issue.key.clone()),
                fgd::IssueKind::BadType => notes.type_issues.push(issue.message.clone()),
                fgd::IssueKind::BadChoice | fgd::IssueKind::Forbidden => {}
            }
        }
        notes
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// True when `strict` must refuse: at least one error-severity issue.
    pub(crate) fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// An unknown FGD class: refused locally, nothing sent (plan §4.2 / brief WP-T2a).
pub(crate) fn unknown_class(index: usize, class: &str, nearest: &[String]) -> ToolError {
    ToolError::new(
        "UnknownClass",
        "UnknownClass",
        format!(
            "entities[{index}].class '{class}' is not a class in the FGD database; nothing was sent \
             (the bridge's class registry would refuse it anyway)"
        ),
    )
    .with_code(-32006)
    .with_data(json!({"index": index, "class": class, "nearest": nearest}))
    .did_you_mean(nearest.iter().cloned())
    .retry_safe(true)
}

// -------------------------------------------------------------------------------------------
// Batch results and failures
// -------------------------------------------------------------------------------------------

/// The `keyValues` object a `create_entities` op carries (the keys sent for that entity), if any.
pub(crate) fn op_keyvalues(op: &Value) -> Option<&Map<String, Value>> {
    op.get("params")?.get("keyValues")?.as_object()
}

/// One `edit.batch` result entry as this server reports it: `index` plus the fields the POC keeps,
/// the raw `handle` replaced by `handleIssued:true`, and the bridge's truncation flags passed
/// through. Never contains a handle.
///
/// `nodeID` (m2.18 W2: the bridge's `entity.create` reply carries the created node's id) is kept
/// when the reply has it, so a caller can address the new entity without a `find_entities`
/// round trip; a reply from a bridge without the field simply has no `nodeID` here (the tool does
/// not invent one).
///
/// `sent` are the keyvalues this server actually sent for that op: **a key the bridge did not
/// report as `applied:true` counts as not applied** (brief item 6). The bridge refuses a value
/// over its 1024-byte buffer with `applied:false` and keeps parsing (so later keys are still
/// listed), and a shortened reply can omit later keys entirely; only flagging explicit `false`
/// entries would report a missing key as applied.
pub(crate) fn result_item(index: usize, raw: &Value, sent: Option<&Map<String, Value>>) -> Value {
    let Some(object) = raw.as_object() else {
        return json!({"index": index});
    };
    if let Some(error) = object.get("error") {
        return json!({"index": index, "error": strip_handles(error.clone())});
    }
    let mut item = Map::new();
    item.insert("index".into(), json!(index));
    for key in [
        "class",
        "origin",
        "angles",
        "scales",
        "nodeID",
        "truncated",
        "previousTruncated",
        "truncatedValues",
    ] {
        if let Some(value) = object.get(key) {
            item.insert(key.to_string(), value.clone());
        }
    }
    if object
        .get("handle")
        .and_then(Value::as_str)
        .is_some_and(|handle| !handle.is_empty())
    {
        // the handle is only valid inside this batch: report that one was issued, never its value
        item.insert("handleIssued".into(), json!(true));
    }
    let applied = object
        .get("applied")
        .or_else(|| object.get("keyValues").and_then(|kv| kv.get("applied")))
        .filter(|value| value.is_object());
    if let Some(applied) = applied {
        item.insert("applied".into(), applied.clone());
    }
    let applied_map = applied.and_then(Value::as_object);
    let mut missed: Vec<String> = applied_map
        .map(|map| {
            map.iter()
                .filter(|(_, value)| value.as_bool() != Some(true))
                .map(|(key, _)| key.clone())
                .collect()
        })
        .unwrap_or_default();
    if let Some(sent) = sent {
        for key in sent.keys() {
            let applied_true = applied_map
                .and_then(|map| map.get(key))
                .and_then(Value::as_bool)
                == Some(true);
            if !applied_true && !missed.iter().any(|seen| seen == key) {
                missed.push(key.clone());
            }
        }
    }
    missed.sort();
    if !missed.is_empty() {
        item.insert("keyValuesNotApplied".into(), json!(missed));
    }
    if let Some(warnings) = object
        .get("warnings")
        .or_else(|| object.get("keyValues").and_then(|kv| kv.get("warnings")))
        .filter(|value| value.is_array())
    {
        item.insert("warnings".into(), warnings.clone());
    }
    Value::Object(item)
}

/// The `results` array of a batch reply or a `-32012` error payload, sanitized entry by entry and
/// matched against the ops that produced them (for the not-applied check).
pub(crate) fn result_items(results: &Value, ops: &[Value]) -> Vec<Value> {
    results
        .as_array()
        .map(|results| {
            results
                .iter()
                .enumerate()
                .map(|(index, raw)| result_item(index, raw, ops.get(index).and_then(op_keyvalues)))
                .collect()
        })
        .unwrap_or_default()
}

/// `_batch_maybe_executed` (POC `h_create_entities`): conservative — did the failed `edit.batch`
/// possibly leave edits behind? True for an outcome-unknown class, `PartialFailure`/
/// `NotCancellable`, a protocol/internal error the bridge can only answer after running the ops
/// (`-32603`/`-32700`), or a payload that already carries results/failedIndex.
pub(crate) fn batch_maybe_executed(error: &BridgeError) -> bool {
    if error.outcome_unknown() || error.kind == Kind::NotCancellable {
        return true;
    }
    if maybe_executed(error) {
        return true;
    }
    let has_results = error
        .data
        .get("results")
        .and_then(Value::as_array)
        .is_some_and(|results| !results.is_empty());
    let failed = error
        .data
        .get("failedIndex")
        .is_some_and(|index| !index.is_null());
    has_results || failed
}

/// The batch failure in the tool's vocabulary: the taxonomy (`PartialFailure` keeps code -32012 and
/// `retrySafe:false`) plus the sanitized `results` and, when the batch may have executed, the
/// `outcomeUnknown` flag and the updated unsaved counter. `ops` are the operations that were sent,
/// so a result entry can report keys the bridge never confirmed.
pub(crate) fn batch_failure(error: BridgeError, state: &HammerState, ops: &[Value]) -> ToolError {
    let maybe = batch_maybe_executed(&error);
    let mut tool = ToolError::from(error);
    if let Some(results) = tool.data.get("results").cloned() {
        tool.data
            .insert("results".into(), Value::Array(result_items(&results, ops)));
    }
    tool.data
        .insert("requested".into(), json!(ops.len()));
    if maybe {
        tool.outcome_unknown = true;
        tool.data
            .insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    }
    tool
}

/// `edit.batch` answered fewer results than ops sent: the bridge stopped early, so an unknown
/// number of ops really ran. Never a success (brief item 2); the counter is already bumped.
pub(crate) fn short_batch_failure(
    state: &HammerState,
    created: Vec<Value>,
    requested: usize,
    failed_index: Value,
) -> ToolError {
    let mut tool = ToolError::new(
        "PartialFailure",
        "PartialFailure",
        format!(
            "edit.batch answered {} result(s) for {requested} op(s): the remaining ops may or may \
             not have run. Verify the live document (get_entity / find_entities) before \
             retrying anything",
            created.len()
        ),
    )
    .with_code(-32012);
    tool.retry_safe = false;
    tool.outcome_unknown = true;
    tool.data.insert("results".into(), Value::Array(created));
    tool.data.insert("requested".into(), json!(requested));
    tool.data.insert("failedIndex".into(), failed_index);
    tool.data
        .insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    tool
}

// -------------------------------------------------------------------------------------------
// create_entities
// -------------------------------------------------------------------------------------------

/// `entities[i]` as one `entity.create` op, FGD-checked. Returns the op and the FGD notes (empty
/// when the database is unavailable or the keyvalues are clean).
pub(crate) fn create_op(
    entities: &[Value],
    index: usize,
    fgd: &FgdSource,
) -> Result<(Value, FgdNotes), ToolError> {
    let label = format!("entities[{index}]");
    let Some(entity) = entities.get(index).and_then(Value::as_object) else {
        return Err(invalid(format!("{label} must be an object")));
    };
    let class = entity
        .get("class")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|class| !class.is_empty())
        .ok_or_else(|| invalid(format!("{label}.class must be a non-empty string")))?
        .to_string();

    let mut params = Map::new();
    params.insert("class".into(), json!(class));
    params.insert(
        "origin".into(),
        json!(vec3_arg(
            &format!("{label}.origin"),
            entity.get("origin")
        )?),
    );
    if let Some(angles) = entity.get("angles").filter(|value| !value.is_null()) {
        params.insert(
            "angles".into(),
            json!(vec3_arg(&format!("{label}.angles"), Some(angles))?),
        );
    }
    let keyvalues = match entity.get("keyValues").filter(|value| !value.is_null()) {
        Some(keyvalues) => keyvalues_arg(&format!("{label}.keyValues"), keyvalues)?,
        None => Map::new(),
    };

    let mut notes = FgdNotes::from_issues(&[]);
    if let Some(db) = fgd.db() {
        let validation = db.validate(&class, &keyvalues);
        if validation.unknown_class {
            return Err(unknown_class(index, &class, &validation.nearest));
        }
        notes = FgdNotes::from_issues(&validation.issues);
    }
    if !keyvalues.is_empty() {
        params.insert("keyValues".into(), Value::Object(keyvalues));
    }

    let op = json!({"method": "entity.create", "params": Value::Object(params)});
    let bytes = op["params"].to_string().len();
    if bytes >= MAX_OP_BYTES {
        return Err(invalid(format!(
            "{label} serializes to {bytes} bytes of op params; the bridge cuts op params at 8192, \
             keep each entity under {MAX_OP_BYTES} bytes"
        )));
    }
    Ok((op, notes))
}

/// `create_entities {entities, strict?, allowUnknownSoundEvent?, undoLabel?}`: one `edit.batch` of
/// `entity.create` ops. Everything is validated locally first, so an unknown class / bad keyvalue /
/// strict FGD issue / sound-typed keyvalue that names an unknown soundevent sends **nothing**. On
/// `-32012` the partial-failure payload passes through (handles dropped) and the unsaved counter is
/// bumped: some ops may have run.
pub async fn create_entities(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    let Some(entities) = args.get("entities").and_then(Value::as_array) else {
        return Err(invalid("entities: an array of 1..=64 entity objects is required"));
    };
    if entities.is_empty() {
        return Err(invalid("entities is empty, there is nothing to create"));
    }
    if entities.len() > MAX_ENTITIES {
        return Err(invalid(format!(
            "entities has {} entries, at most {MAX_ENTITIES} fit in one edit.batch",
            entities.len()
        )));
    }
    let strict = bool_arg(&args, "strict")?.unwrap_or(false);
    let allow_unknown_sound = bool_arg(&args, "allowUnknownSoundEvent")?.unwrap_or(false);
    let requested_label = undo_label_arg(&args)?;

    let fgd = FgdSource::load(&state);
    let mut ops: Vec<Value> = Vec::with_capacity(entities.len());
    let mut notes: Vec<FgdNotes> = Vec::with_capacity(entities.len());
    let warnings: Vec<String> = fgd.warning().into_iter().collect();
    // 1. local validation of every entity (nothing is sent for a bad argument)
    for index in 0..entities.len() {
        let (op, entity_notes) = create_op(entities, index, &fgd)?;
        ops.push(op);
        notes.push(entity_notes);
    }

    // 2. the document read: it also refreshes the addon the soundevent index below is built for
    //    (the addon follows the active document)
    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(&state, deadline).await?;
    let doc_id = doc_id_of(&doc);

    // 3. WP-S3: every `sound`-typed keyvalue is checked against the soundevent index BEFORE the
    //    batch is built (an unknown name is silent at runtime, so it is a refusal — the model
    //    check's argument). The warnings ride the entity's own `warnings`.
    for index in 0..entities.len() {
        let class = ops[index]["params"]["class"].as_str().unwrap_or_default().to_string();
        let keyvalues = op_keyvalues(&ops[index]).cloned().unwrap_or_default();
        for warning in sound_check::check_sound_keyvalues(
            &state,
            &fgd,
            &class,
            &keyvalues,
            &[],
            allow_unknown_sound,
        )
        .await?
        {
            notes[index].messages.push(warning);
        }
        if strict && notes[index].has_errors() {
            return Err(invalid(format!(
                "entities[{index}] failed the FGD keyvalue check (strict): {}",
                notes[index].errors.join("; ")
            )));
        }
    }

    let mut params = Map::new();
    // the m2.15 edit.batch arm takes no undoLabel (a batch with a create op opens "MCP: batch",
    // a set-only one "MCP: set keyvalues" — 35528e8); the POC sent it anyway and the bridge
    // ignores unknown members, so it is kept for parity and echoed back.
    params.insert("undoLabel".into(), json!(requested_label));
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
            if batch_maybe_executed(&error) {
                note_edit(&state, doc_id.as_deref());
            }
            return Err(batch_failure(error, &state, &ops));
        }
    };
    note_edit(&state, doc_id.as_deref());

    if !reply.result.is_object() {
        return Err(ToolError::internal(format!(
            "edit.batch returned a non-object result: {} (crates/cs2-bridge/PROTOCOL.md §10)",
            reply.result
        )));
    }
    // fewer results than ops sent: an unknown number of ops ran (brief item 2) — never a success.
    let created = result_items(&reply.result["results"], &ops);
    if created.len() != ops.len() {
        return Err(short_batch_failure(
            &state,
            created,
            ops.len(),
            reply.result.get("failedIndex").cloned().unwrap_or(Value::Null),
        ));
    }
    let mut created = created;
    for (index, item) in created.iter_mut().enumerate() {
        let Some(entity_notes) = notes.get(index) else {
            continue;
        };
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        if !entity_notes.is_empty() {
            let mut merged = object
                .get("warnings")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            merged.extend(entity_notes.messages.iter().map(|m| json!(m)));
            object.insert("warnings".into(), Value::Array(merged));
            object.insert("unknownKeys".into(), json!(entity_notes.unknown_keys));
            object.insert("typeIssues".into(), json!(entity_notes.type_issues));
        }
    }

    let not_applied: Vec<String> = created
        .iter()
        .filter_map(|item| item.get("keyValuesNotApplied").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();

    let mut out = Map::new();
    out.insert("count".into(), json!(created.len()));
    out.insert("requested".into(), json!(ops.len()));
    out.insert("entities".into(), Value::Array(created));
    // the bridge's own group label (it never stores undoLabel, m2.15 §7)
    out.insert("undoLabel".into(), json!(BATCH_UNDO_LABEL));
    out.insert("requestedUndoLabel".into(), json!(requested_label));
    out.insert(
        "failedIndex".into(),
        reply.result.get("failedIndex").cloned().unwrap_or(Value::Null),
    );
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("saved".into(), json!(false));
    out.insert("fgd".into(), fgd.json());
    out.insert("warnings".into(), json!(warnings));
    out.insert(
        "notes".into(),
        json!([
            "no auto-save: run save_map to write the changes; file-backed reads remain stale until then",
            "the bridge labels the batch undo group \"MCP: batch\" for a batch with a create op \
             (a set-only one is \"MCP: set keyvalues\", crates/cs2-bridge/PROTOCOL.md §10; \
             edit.batch takes no undoLabel; see requestedUndoLabel)"
        ]),
    );
    if !not_applied.is_empty() {
        let unique: std::collections::BTreeSet<&str> = not_applied.iter().map(String::as_str).collect();
        out.insert(
            "keyValuesWarning".into(),
            json!(format!(
                "some keyvalues were not applied (a type mismatch or a bridge refusal): {}",
                unique.into_iter().collect::<Vec<_>>().join(", ")
            )),
        );
    }
    Ok(Value::Object(out).into())
}

// -------------------------------------------------------------------------------------------
// set_entity_keyvalues
// -------------------------------------------------------------------------------------------

/// `set_entity_keyvalues {nodeID, keyValues, strict?, force?}`: coerce/validate locally, read the
/// live classname and values with `entity.get` (declared on this spec, so a bridge without it
/// gates the tool out), FGD-check the values (a value equal to the entity's current value is a
/// revert and is exempt: `strict` never refuses one), then `entity.setKeyValues {nodeID, values,
/// expectDoc}`. WP-S3: also the **sound check** — a keyvalue whose FGD type is `sound` (e.g.
/// `soundName`) is looked up in the soundevent index first (unknown → `SoundEventNotFound` with
/// suggestions; an unbuildable index → `VpkMissing`; `force:true` writes either with a warning,
/// a revert skips the lookup). WP-R1: also the **model check** — a `model` keyvalue on a
/// prop-tagged class is normalized + catalog-looked-up first (unknown → `ModelNotFound` with
/// suggestions; `force:true` skips), so an un-placable model never gets written. A write failure
/// on the bridge's not-found / not-an-entity conditions is mapped through the shared read
/// taxonomy
/// (`EntityNotFound`/`NotAnEntity`, review L5 — the node may have changed between the get and
/// the set); every other write error keeps the maybe-executed handling. The result carries
/// `previous` and the `valueUndo` marker the bridge answered (`"group-only"` = undo pops the
/// group without restoring, `revertHint` says to re-apply; `"recorded"` = the m2.18 real value
/// undo, `revertHint` points at `undo_last`).
pub async fn set_entity_keyvalues(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    let node_id = node_id_arg(&args)?;
    let Some(raw_values) = args.get("keyValues").filter(|value| !value.is_null()) else {
        return Err(invalid("keyValues is required (an object of keyvalue scalars)"));
    };
    let keyvalues = keyvalues_arg("keyValues", raw_values)?;
    if keyvalues.is_empty() {
        return Err(invalid("keyValues is empty, there is nothing to set"));
    }
    let strict = bool_arg(&args, "strict")?.unwrap_or(false);
    let force = bool_arg(&args, "force")?.unwrap_or(false);

    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(&state, deadline).await?;
    let doc_id = doc_id_of(&doc);

    // the entity's own classname drives the FGD check and its live values tell a revert from an
    // edit (nothing is written by entity.get)
    let (classname, live_values, truncated_values) = {
        let mut bridge = state.bridge.lock().await;
        match bridge
            .request(
                "entity.get",
                json!({"nodeID": node_id}),
                RequestOpts::deadline(deadline),
            )
            .await
        {
            Ok(reply) => (
                reply
                    .result
                    .get("classname")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                reply
                    .result
                    .get("keyValues")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default(),
                reply
                    .result
                    .get("truncatedValues")
                    .filter(|values| values.as_array().is_some_and(|values| !values.is_empty()))
                    .cloned(),
            ),
            Err(error) => return Err(entities_query::read_error(error, node_id)),
        }
    };

    let fgd = FgdSource::load(&state);
    let mut warnings: Vec<String> = fgd.warning().into_iter().collect();
    // A value that equals the entity's current value is a revert: its *type* check is exempt so a
    // `strict` revert is never refused (brief item 11). The key must still be declared by the class
    // (an unknown key stays an error).
    let mut exempt: Vec<String> = Vec::new();
    for (key, value) in &keyvalues {
        if live_values.get(key).and_then(Value::as_str) == value.as_str() {
            exempt.push(key.clone());
        }
    }
    if let Some(db) = fgd.db() {
        let validation = db.validate(&classname, &keyvalues);
        if validation.unknown_class {
            // the entity exists: an FGD that does not describe its class is a warning, not a refusal
            warnings.push(format!(
                "fgd: class '{classname}' is not in the FGD database; the keyvalues were not validated"
            ));
        } else {
            let issues: Vec<fgd::Issue> = validation
                .issues
                .iter()
                .filter(|issue| {
                    !(exempt.iter().any(|key| key == &issue.key)
                        && matches!(
                            issue.kind,
                            fgd::IssueKind::BadType | fgd::IssueKind::BadChoice
                        ))
                })
                .cloned()
                .collect();
            let notes = FgdNotes::from_issues(&issues);
            if strict && notes.has_errors() {
                return Err(invalid(format!(
                    "keyValues for nodeID {node_id} (class {classname}) failed the FGD keyvalue \
                     check (strict): {}",
                    notes.errors.join("; ")
                )));
            }
            warnings.extend(notes.messages);
        }
    }
    // WP-S3: a keyvalue whose FGD type is `sound` is looked up in the soundevent index (the
    // sound's model-check counterpart): an unknown name is `SoundEventNotFound` (silent at
    // runtime), an unbuildable index is `VpkMissing` — the write never happens. `force:true`
    // writes either unchecked with a warning; the `exempt` reverts (the submitted value equals the
    // live one) skip the lookup entirely.
    warnings.extend(
        sound_check::check_sound_keyvalues(&state, &fgd, &classname, &keyvalues, &exempt, force)
            .await?,
    );
    // WP-R1: a `model` keyvalue on a prop-tagged class is looked up the way every prop tool does
    // (the shared `model_info::resolve`), BEFORE anything is written — a model the catalog never
    // heard of stays Hammer's white placeholder box, so the check refuses it. `force:true`
    // skips the check; a revert (the submitted value equals the live value) skips it too,
    // because writing the entity's own value back must not need catalog-able input. An FGD
    // that does not tag the live class keeps the ordinary FGD type check and no model lookup.
    if !force {
        // review w2b #11: with the FGD unavailable the generic warning does not say what
        // follow from it here — the tag is unknown, so the model check never ran
        if fgd.db().is_none() {
            if let Some(key) = keyvalues
                .keys()
                .find(|key| key.eq_ignore_ascii_case("model"))
            {
                warnings.push(format!(
                    "fgd unavailable: whether class '{classname}' is a prop (and with it the \
                     model catalog check) could not be determined — the {key:?} value was NOT \
                     catalog-checked"
                ));
            }
        }
        let prop_tagged = fgd.db().and_then(|db| db.prop_tag(&classname)).is_some();
        if prop_tagged {
            for (key, value) in &keyvalues {
                if !key.eq_ignore_ascii_case("model") || exempt.iter().any(|seen| seen == key) {
                    continue;
                }
                let Some(model_path) = value.as_str() else {
                    continue;
                };
                // "" clears the model (re-props.md Q1's own documented workaround for an older
                // bridge's raw write — "set \"\" first, then the new model"): nothing to look
                // up, the write goes through (review w2b #3)
                if model_path.trim().is_empty() {
                    warnings.push(
                        "the model keyvalue is set to \"\" (clear the model): the catalog check \
                         was skipped for the empty path"
                            .to_string(),
                    );
                    continue;
                }
                // The resolve errors name the model (`ModelNotFound` + the catalog's
                // suggestions, `ModelNotCompiled`'s note is the create_props level); any other
                // refusal (a bad spelling, an unavailable catalog) is explicit about why the
                // check could not pass — nothing was written either way, and the refusal says
                // how the caller can bypass the check.
                if let Some(mut refused) =
                    super::model_info::resolve(&state, model_path).await.err()
                {
                    refused.message = format!(
                        "{} (pass force:true to write it unchecked)",
                        refused.message
                    );
                    return Err(refused);
                }
            }
        }
    }

    if let Some(values) = &truncated_values {
        warnings.push(format!(
            "the bridge truncated the live value(s) {values}: the `previous`/revert comparison for \
             those keys may be incomplete; read them back in Hammer"
        ));
    }

    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "entity.setKeyValues",
                json!({"nodeID": node_id, "values": Value::Object(keyvalues.clone())}),
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
            // the target is re-resolved on the write (review L5): a node deleted or replaced
            // between the `entity.get` and the write fails right there — the shared entity
            // taxonomy, nothing executed
            if let Some(tool) = set_target_failure(&error, node_id) {
                return Err(tool);
            }
            // a write that failed on a protocol/internal error may already have landed (brief item 8)
            return Err(mutator_failure(error, &state, doc_id.as_deref()));
        }
    };
    note_edit(&state, doc_id.as_deref());

    let Value::Object(mut out) = reply.result else {
        return Err(ToolError::internal(format!(
            "entity.setKeyValues returned a non-object result: {} \
             (crates/cs2-bridge/PROTOCOL.md §9.4)",
            reply.result
        )));
    };
    // m2.17: the reply carries the document state the write left behind (`doc`); this tool does not
    // consume it, so it is not echoed as a result field.
    out.remove("doc");
    // a cut `previous` must never be re-applied as if it were the whole value (brief item 6)
    let previous_truncated = out
        .get("previousTruncated")
        .filter(|values| values.as_array().is_some_and(|values| !values.is_empty()))
        .cloned();
    if let Some(values) = &previous_truncated {
        warnings.push(format!(
            "the bridge reports these `previous` value(s) as null — over its 1024-byte value \
             buffer it gives no truncated prefix, only a listing: {values} — read the full \
             value in Hammer before reverting"
        ));
    }
    let mut merged: Vec<Value> = out
        .get("warnings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    merged.extend(warnings.iter().map(|warning| json!(warning)));
    out.insert("nodeID".into(), json!(node_id));
    out.insert("classname".into(), json!(classname));
    // the value-undo verdict the bridge itself answered ("group-only" up to m2.17, "recorded"
    // on an m2.18 build that went through RecordNodeTree); an older reply without the marker
    // keeps the gap the m2.15 bridge documented
    out.entry("valueUndo")
        .or_insert_with(|| json!("group-only"));
    let value_undo = out
        .get("valueUndo")
        .and_then(Value::as_str)
        .unwrap_or("group-only");
    out.insert(
        "revertHint".into(),
        json!(match value_undo {
            "recorded" => RECORDED_HINT,
            _ => REVERT_HINT,
        }),
    );
    if let Some(values) = truncated_values {
        out.insert("truncatedValues".into(), values);
    }
    if !exempt.is_empty() {
        out.insert("exemptFromFgdCheck".into(), json!(exempt));
    }
    // keys sent but missing from `applied` count as not applied (brief item 6); a reply with no
    // `applied` map at all confirms nothing
    let applied_map = out
        .get("applied")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if out.get("applied").is_none() {
        merged.push(json!(
            "the bridge answered no `applied` map: no keyvalue could be confirmed as written"
        ));
    }
    let mut missed: Vec<String> = applied_map
        .iter()
        .filter(|(_, value)| value.as_bool() != Some(true))
        .map(|(key, _)| key.clone())
        .collect();
    for key in keyvalues.keys() {
        if !missed.iter().any(|seen| seen == key)
            && applied_map.get(key).and_then(Value::as_bool) != Some(true)
        {
            missed.push(key.clone());
        }
    }
    missed.sort();
    if !missed.is_empty() {
        out.insert("keyValuesNotApplied".into(), json!(missed));
    }
    out.insert("warnings".into(), Value::Array(merged));
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("saved".into(), json!(false));
    out.insert("fgd".into(), fgd.json());
    Ok(Value::Object(out).into())
}

// -------------------------------------------------------------------------------------------
// undo_last
// -------------------------------------------------------------------------------------------

/// `undo_last {acceptUserEdits?}`: `doc.active` first. An empty stack answers `undone:false` with a
/// reason and sends nothing; a top group this server did not create (an `undoName` that does not
/// start with `MCP:` — empty or missing included) is `UndoGroupNotOurs` unless `acceptUserEdits`
/// (brief item 9). Otherwise `undo.undo`; the unsaved counter goes down only for a group of ours
/// that really undid something. A popped keyvalue group counts down **only** when the bridge
/// records value undo (the m2.18 marker `node.setTransform` is advertised = RecordNodeTree,
/// review w2b #2: the live bridge really restores the values and the drawn model); on a build
/// without the marker a `"MCP: set keyvalues"` pop reverts nothing and the map still differs from
/// disk (brief item 3). The popped keyvalue group adds the note that matches what the pop did
/// (our groups only); a popped group this server did not create gets a note worded by
/// `acceptUserEdits` (review N4/L7: with it the user consented; without it the top group changed
/// between the check and the undo, since the fingerprint does not move on user edits).
pub async fn undo_last(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    let accept = bool_arg(&args, "acceptUserEdits")?.unwrap_or(false);
    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(&state, deadline).await?;
    let doc_id = doc_id_of(&doc);

    let depth = doc.get("undoDepth").and_then(Value::as_i64).unwrap_or(0);
    if depth <= 0 {
        return Ok(json!({
            "undone": false,
            "undoName": Value::Null,
            "undoDepth": 0,
            "reason": "the document has no undo group (undoDepth 0); nothing was sent",
            "unsavedBridgeEdits": state.unsaved_edits(),
            "saved": false,
        })
        .into());
    }

    let undo_name = doc.get("undoName").and_then(Value::as_str).unwrap_or("");
    if !accept && !undo_name.starts_with("MCP:") {
        return Err(ToolError::new(
            "UndoGroupNotOurs",
            "UndoGroupNotOurs",
            format!(
                "the top undo group is {undo_name:?}, which this server did not create (only \
                 groups named \"MCP: ...\" are ours); pass acceptUserEdits=true to undo it anyway \
                 (nothing was sent)"
            ),
        )
        .with_data(json!({"undoName": undo_name, "undoDepth": depth}))
        .retry_safe(true));
    }

    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "undo.undo",
                json!({}),
                RequestOpts {
                    deadline_ms: Some(deadline),
                    expect_doc: Some(expect_doc(&doc)),
                    gate: None,
                },
            )
            .await
    };
    let reply = match reply {
        // the pop may have run even when the reply failed (brief item 8)
        Err(error) => return Err(mutator_failure(error, &state, doc_id.as_deref())),
        Ok(reply) => reply,
    };

    let undone = reply
        .result
        .get("undone")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let name = reply.result.get("name").cloned().unwrap_or(Value::Null);
    let ours = name.as_str().is_some_and(|name| name.starts_with("MCP:"));
    let keyvalue_group = name.as_str() == Some(KEYVALUE_UNDO_LABEL);
    // m2.18: a popped keyvalue group on a bridge that advertises `node.setTransform` really
    // restored the values (RecordNodeTree; live facts say the drawn model goes back too). The
    // marker check never fails an undo that succeeded: a dead bridge or a failed refresh just
    // answers the older (group-only) wording.
    let value_undo_recorded = if undone && ours && keyvalue_group {
        bridge_records_value_undo(&state).await
    } else {
        false
    };
    let mut notes: Vec<Value> = vec![json!(
        "undo groups close implicitly: the top group may also contain the user's own edits"
    )];
    if undone && ours && (!keyvalue_group || value_undo_recorded) {
        note_undo(&state, doc_id.as_deref());
    } else if undone && ours {
        // a group of ours that reverts nothing ("MCP: set keyvalues" on a pre-m2.18 bridge;
        // review N4 keeps this text for our own groups only: the values stay, so the live map
        // still differs from disk)
        notes.push(json!(
            "the unsaved-edit counter stays up: this undo did not restore keyvalue values, so the \
             live map still differs from the saved file"
        ));
    } else if undone {
        // a group this server did not create got popped. With `acceptUserEdits` that is the
        // group the gate saw and the user consented to (review N4); without it the top group
        // changed between the `doc.active` check and the undo — the fingerprint epoch only moves
        // on doc/root/path changes, not user edits, so `expectDoc` does not catch this (review
        // L7). The wording follows `accept` because that is what is known: Hammer restored the
        // state either way, and only the wording changes, never the counter.
        if accept {
            notes.push(json!(
                "this was the user's own undo group: it was popped with acceptUserEdits and Hammer \
                 restored its state; the unsaved-edit counter is unchanged (this server did not \
                 count those edits)"
            ));
        } else {
            notes.push(json!(
                "this was not one of this server's undo groups: the top group changed between the \
                 doc.active check and the undo; Hammer restored its state and the unsaved-edit \
                 counter is unchanged"
            ));
        }
    }
    if keyvalue_group {
        notes.push(json!(if value_undo_recorded {
            KEYVALUE_UNDO_RESTORED_NOTE
        } else {
            KEYVALUE_UNDO_NOTE
        }));
    }
    let mut out = Map::new();
    out.insert("undone".into(), json!(undone));
    out.insert("undoName".into(), name);
    out.insert(
        "undoDepth".into(),
        reply
            .result
            .get("undoDepth")
            .cloned()
            .unwrap_or_else(|| json!(depth)),
    );
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("saved".into(), json!(false));
    out.insert("notes".into(), Value::Array(notes));
    Ok(Value::Object(out).into())
}

/// Does the bridge advertise the m2.18 marker (`node.setTransform`)? A connected bridge whose
/// capabilities are fresh answers without any extra round trip; a stale list triggers one
/// refresh (the same pattern `create_props` uses). Any failure — a dead bridge, a refused
/// refresh — answers `false`: this check only picks the wording of an undo that already
/// succeeded, it must never turn it into an error.
async fn bridge_records_value_undo(state: &HammerState) -> bool {
    let mut bridge = state.bridge.lock().await;
    if bridge.ensure_connected().await.is_err() {
        return false;
    }
    let mut missing = bridge.missing(&[crate::tools::props_live::M2_18_MARKER]);
    if !missing.is_empty() {
        let _ = bridge.refresh_capabilities().await;
        missing = bridge.missing(&[crate::tools::props_live::M2_18_MARKER]);
    }
    missing.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_coerce_to_the_bridge_text() {
        assert_eq!(value_text(&json!("x")), Some("x".to_string()));
        assert_eq!(value_text(&json!(7)), Some("7".to_string()));
        assert_eq!(value_text(&json!(1.5)), Some("1.5".to_string()));
        assert_eq!(value_text(&json!(true)), Some("1".to_string()));
        assert_eq!(value_text(&json!(false)), Some("0".to_string()));
        for bad in [json!(null), json!([]), json!({})] {
            assert_eq!(value_text(&bad), None, "{bad}");
        }
    }

    #[test]
    fn keyvalues_arg_bounds_and_coercion() {
        let kv = keyvalues_arg("keyValues", &json!({"a": 1, "b": true, "c": "x"})).unwrap();
        assert_eq!(kv["a"], json!("1"));
        assert_eq!(kv["b"], json!("1"));
        assert_eq!(kv["c"], json!("x"));
        // the empty value is an ordinary value (m2.16): it is sent as "" and the bridge writes
        // Hammer's empty symbol (a NULL CUtlSymbolLarge) and answers applied:true
        let empty = keyvalues_arg("keyValues", &json!({"a": "", "b": "x"})).unwrap();
        assert_eq!(empty["a"], json!(""), "{empty:?}");

        for bad in [
            json!("x"),
            json!([]),
            json!(null),
            json!({"classname": "info_target"}),
            json!({"Classname": "x"}),
            // engine-managed node fields are not keyvalues (brief item 1)
            json!({"origin": "0 0 0"}),
            json!({"ORIGIN": "0 0 0"}),
            json!({"angles": "0 0 0"}),
            json!({"scales": "1 1 1"}),
            json!({"a": null}),
            json!({"a": [1]}),
            json!({"a": {}}),
            json!({"": "x"}),
            json!({"classname\u{0}x": "prop_dynamic"}),
            json!({"origin\n": "1 2 3"}),
            json!({"targetname": "a\u{0}b"}),
        ] {
            let err = keyvalues_arg("keyValues", &bad).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}");
            if bad.get("origin").is_some() || bad.get("angles").is_some() || bad.get("scales").is_some()
            {
                assert!(
                    err.message.contains("origin/angles parameters"),
                    "{bad}: {}",
                    err.message
                );
            }
        }

        // 511 bytes pass, 512 do not (this server's own cap; the bridge's value buffer is
        // KV_VALUE_MAX 1024)
        let mut at_cap = Map::new();
        at_cap.insert("k".to_string(), json!("v".repeat(MAX_VALUE_BYTES)));
        assert!(keyvalues_arg("keyValues", &Value::Object(at_cap)).is_ok());
        let long_key = "k".repeat(MAX_KEY_BYTES + 1);
        let long_value = "v".repeat(MAX_VALUE_BYTES + 1);
        let mut many = Map::new();
        for i in 0..=MAX_KEYS {
            many.insert(format!("k{i}"), json!("v"));
        }
        let mut long_key_map = Map::new();
        long_key_map.insert(long_key, json!("v"));
        let mut long_value_map = Map::new();
        long_value_map.insert("k".to_string(), json!(long_value));
        for bad in [
            json!({"keyValues": Value::Object(many)}),
            json!({"keyValues": Value::Object(long_key_map)}),
            json!({"keyValues": Value::Object(long_value_map)}),
        ] {
            assert!(keyvalues_arg("keyValues", &bad["keyValues"]).is_err(), "{bad}");
        }
    }

    #[test]
    fn vec3_arg_needs_three_finite_numbers() {
        assert_eq!(vec3_arg("o", Some(&json!([1, 2, 3]))).unwrap(), [1.0, 2.0, 3.0]);
        // a sub-1e-4 component would serialize as `5.5e-17`, which the bridge misreads: snapped to 0
        assert_eq!(
            vec3_arg("o", Some(&json!([5.5e-17, 2, 3]))).unwrap(),
            [0.0, 2.0, 3.0]
        );
        for bad in [
            json!([]),
            json!([1, 2]),
            json!([1, 2, 3, 4]),
            json!([1, 2, "3"]),
            json!(null),
            // above the exact range: `1e20` goes out in exponent notation
            json!([1e20, 2, 3]),
            json!([1, 2, -1e20]),
        ] {
            assert!(vec3_arg("o", Some(&bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn batch_maybe_executed_is_conservative() {
        let outcome = BridgeError::new(Kind::OutcomeUnknown, "x");
        assert!(batch_maybe_executed(&outcome));
        let partial = BridgeError::new(Kind::PartialFailure, "x")
            .with_data(json!({"results": [], "failedIndex": 1}));
        assert!(batch_maybe_executed(&partial));
        let too_large = BridgeError::new(Kind::PartialFailure, "x")
            .with_data(json!({"failedIndex": null, "reason": "result too large"}));
        assert!(batch_maybe_executed(&too_large));
        let changed = BridgeError::new(Kind::DocumentChanged, "x")
            .with_data(json!({"reason": "epoch"}));
        assert!(!batch_maybe_executed(&changed));
        // a bare protocol error (a dropped or malformed reply after dispatch) may have run the ops
        let protocol = BridgeError::new(Kind::ProtocolError, "x");
        assert!(batch_maybe_executed(&protocol));
        assert!(maybe_executed(&protocol));
        // ... but a -32602 refusal is decided before the ops run
        let params = BridgeError::new(Kind::ProtocolError, "x")
            .with_code(-32602)
            .with_data(json!({"results": [], "failedIndex": null}));
        assert!(!batch_maybe_executed(&params));
        assert!(!maybe_executed(&params));
        // an internal error after dispatch may have run the ops (brief item 8)
        let internal = BridgeError::new(Kind::ProtocolError, "x").with_code(-32603);
        assert!(batch_maybe_executed(&internal));
        assert!(maybe_executed(&internal));
    }

    #[test]
    fn result_item_replaces_the_handle_and_keeps_the_rest() {
        let raw = json!({
            "handle": "n3",
            "class": "info_target",
            "origin": [1, 2, 3],
            "undoLabel": "MCP: create entity",
            "keyValues": {"applied": {"a": true, "b": false}, "warnings": ["b: nope"]}
        });
        let sent: Map<String, Value> = serde_json::from_value(json!({"a": "1", "b": "2"})).unwrap();
        let item = result_item(2, &raw, Some(&sent));
        assert_eq!(item["index"], json!(2));
        assert_eq!(item["class"], json!("info_target"));
        assert_eq!(item["handleIssued"], json!(true));
        assert_eq!(item["keyValuesNotApplied"], json!(["b"]));
        assert!(item.get("handle").is_none());
        assert!(!item.to_string().contains("\"handle\""));
        let error = result_item(0, &json!({"error": {"code": -32006, "handle": "n9"}}), None);
        assert!(error.get("error").and_then(|e| e.get("handle")).is_none());
    }

    #[test]
    fn keys_missing_from_applied_count_as_not_applied() {
        // the bridge's parser stops mid-request on an overflowing value: `b` and `c` never appear
        let raw = json!({"keyValues": {"applied": {"a": true}, "warnings": []}});
        let sent: Map<String, Value> =
            serde_json::from_value(json!({"a": "1", "b": "2", "c": "3"})).unwrap();
        let item = result_item(0, &raw, Some(&sent));
        assert_eq!(item["keyValuesNotApplied"], json!(["b", "c"]));
        // nothing sent: no flag at all
        let clean = json!({"keyValues": {"applied": {"a": true}, "warnings": []}});
        assert!(result_item(0, &clean, None).get("keyValuesNotApplied").is_none());
        // the bridge's truncation flags are surfaced
        let truncated = json!({
            "keyValues": {"applied": {"a": true}, "warnings": ["a: cut"]},
            "previousTruncated": ["vscripts"]
        });
        let item = result_item(0, &truncated, None);
        assert_eq!(item["previousTruncated"], json!(["vscripts"]));
        assert_eq!(item["warnings"], json!(["a: cut"]));
    }
}
