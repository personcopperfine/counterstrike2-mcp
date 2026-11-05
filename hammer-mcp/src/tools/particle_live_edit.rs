//! Live particle *editing* through the running pet (`pet.text`/`pet.apply`/`pet.undo`/`pet.redo`,
//! bridge m2.13, plan2 §0.2), next to the live read/lifecycle tools in [`crate::tools::particle_live`].
//!
//! How the round trip works (the pet plan's dirty/undo semantics, §4.3):
//!
//! * `particle_live_read` asks `pet.text` for the document's own serialization and parses it on
//!   the host. Pet's own serialization must parse, so a parse failure (`LiveTextUnparseable`)
//!   flags a `cs2-kv3` gap, never a pet bug — it is reported with the parse position.
//! * `particle_live_edit` takes the bridge lock once, reads the live text, applies the ops
//!   in memory ([`particle_file::edit_in_memory`]: structure always, schema unless `skipSchema`,
//!   all-or-nothing), re-serializes and hands the whole text to `pet.apply` — one request, one
//!   undo step labelled `label` (default `"MCP: <op names>"`). The edit never refuses a dirty or
//!   untitled document (the apply is undoable; nothing modal is involved), but the document is
//!   dirty afterwards: `particle_reload`/`particle_open` then need `force:true`, or a
//!   `particle_save` first. Nothing is sent when the ops or the checks fail locally, and the
//!   384 KB `pet.apply` size gate is applied on the host before the request.
//! * `particle_undo`/`particle_redo` are single `pet.undo`/`pet.redo` requests; the bridge's
//!   `done:false` (nothing to undo/redo) becomes a retry-safe error, never a crash.
//!
//! The mock bridge's `pet.*` built-ins are the contract for the tests
//! (`tests/particle_live_edit.rs`); the mock does not parse KV3, so its `arrays` are stale after
//! an apply — the tests assert the *sent text*, not the arrays.
use crate::state::HammerState;
use crate::tools::particle_file::{self, MAX_OPS};
use crate::tools::particle_live::document_view;
use crate::vpcf::{FunctionRef, Op, Particle, VpcfError};
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{
    CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Map, Value};

/// `pet.apply`'s size gate (bridge m2.13: 384 KB of raw text, both directions; the same bound
/// `cs2_mcp_common::mock::PET_TEXT_MAX` tests against). Applied on the host so an oversized text
/// is refused before anything is sent.
const PET_TEXT_MAX: usize = 393216;

/// The undo-step label's bound and the printable-ASCII range the bridge enforces (`m2.13 §0.1`);
/// checked here so a bad label is refused before anything is sent.
const LABEL_MAX: usize = 64;

/// The four live-edit pet tools this module registers, in registration order. The orchestrator
/// places these between `particle_live.rs`'s seven and `particle_render` in `PET_TOOL_ORDER`.
pub const PARTICLE_LIVE_EDIT_TOOL_ORDER: [&str; 4] = [
    "particle_live_read",
    "particle_live_edit",
    "particle_undo",
    "particle_redo",
];

// ---------------------------------------------------------------- registration

/// Register the four live-edit tools (see the module header). Gating: the registry refuses a tool
/// whose declared `pet.*` methods are not advertised, without sending anything.
pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "particle_live_read",
            "Read the live pet document as pet serialized it: path, dirty/canUndo/canRedo, kv3 header, root non-array fields, per-array entry counts; function:\"m_Operators/2\" returns that entry.",
            ReadOnly,
            &["pet.text"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "function": {
                    "type": "string",
                    "description": "\"<array>/<index>\", e.g. m_Operators/2; \"/\" = the root object"
                }
            },
            "additionalProperties": true
        })),
        particle_live_read,
    );
    r.register(
        ToolSpec::new(
            "particle_live_edit",
            "Edit the live pet document in-editor with undo: ops add_function/remove_function/move_function/set_field/remove_field/set_root_field/remove_root_field/add_child/remove_child/add_variable/remove_variable, all-or-nothing, schema-checked (skipSchema skips only that). The document is dirty afterwards: particle_save, or force on reload/open.",
            Mutating,
            &["pet.text", "pet.apply"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "ops": {
                    "type": "array",
                    "minItems": 1,
                    "description": "applied in order (the particle_edit op set)"
                },
                "label": {
                    "type": "string",
                    "description": "undo label, 1..64 printable ASCII; default \"MCP: <ops>\""
                },
                "allowObsolete": {
                    "type": "boolean",
                    "description": "obsolete classes warn instead of error (default true)"
                },
                "skipSchema": {
                    "type": "boolean",
                    "description": "default false"
                }
            },
            "required": ["ops"],
            "additionalProperties": true
        })),
        particle_live_edit,
    );
    r.register(
        ToolSpec::new(
            "particle_undo",
            "Undo the last live pet edit (same history as the editor's Ctrl+Z). Nothing to undo is a retry-safe error.",
            Mutating,
            &["pet.undo"],
        ),
        particle_undo,
    );
    r.register(
        ToolSpec::new(
            "particle_redo",
            "Redo the last undone live pet edit. Nothing to redo is a retry-safe error.",
            Mutating,
            &["pet.redo"],
        ),
        particle_redo,
    );
}

// ---------------------------------------------------------------- small helpers (own file)

/// `function`/`label`-style optional string argument (the `particle_file` helper, untrimmed).
use particle_file::optional_str as opt_str;

/// `LiveTextUnparseable`: the text `pet.text` served does not parse on the host. Pet's own
/// serialization must parse, so this flags a cs2-kv3 gap; the parse position is reported.
fn live_text_unparseable(error: &VpcfError) -> ToolError {
    let (line, col, msg) = match error {
        VpcfError::Parse(parse) => (parse.line, parse.col, parse.msg.clone()),
        other => (0, 0, other.to_string()),
    };
    ToolError::new(
        "LiveTextUnparseable",
        "LiveTextUnparseable",
        format!(
            "the live document text does not parse ({line}:{col}: {msg}); pet's own serialization \
             must parse, so this is a parser gap — file a cs2-kv3 issue with the text"
        ),
    )
    .with_code(-32602)
    .retry_safe(true)
    .with_data(json!({"line": line, "col": col, "msg": msg}))
}

/// `TextTooLarge`: the edited document's text exceeds the bridge's `pet.apply` gate. Nothing was
/// sent; the file route (`particle_edit` + `particle_save`) handles documents this large.
fn text_too_large(bytes: usize) -> ToolError {
    ToolError::new(
        "TextTooLarge",
        "TextTooLarge",
        format!(
            "the edited document's text is {bytes} bytes, past the {PET_TEXT_MAX}-byte pet.apply \
             gate; nothing was sent — edit the file on disk instead (particle_edit + particle_save)"
        ),
    )
    .with_code(-32602)
    .retry_safe(true)
    .with_data(json!({"bytes": bytes, "max": PET_TEXT_MAX}))
}

/// `NothingToUndo`/`NothingToRedo`: the bridge answered `done:false` (empty history) — a result
/// on the wire, an error for the caller (retry-safe: retrying stays a no-op).
fn nothing_to_do(method: &str, result: &Value) -> ToolError {
    let (name, fallback) = if method == "pet.undo" {
        ("NothingToUndo", "nothing_to_undo")
    } else {
        ("NothingToRedo", "nothing_to_redo")
    };
    let reason = result
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or(fallback);
    ToolError::new(
        name,
        name,
        format!(
            "the live document's history has nothing to {} (bridge done:false)",
            name[9..].to_lowercase()
        ),
    )
    .retry_safe(true)
    .with_data(json!({"reason": reason}))
}

/// The undo-step label: the caller's (`1..64` printable ASCII), or `"MCP: "` + the op names.
fn edit_label(args: &Value, ops: &[Op]) -> Result<String, ToolError> {
    match opt_str(args, "label")? {
        Some(label) => {
            if label.is_empty()
                || label.len() > LABEL_MAX
                || !label.bytes().all(|b| (0x20..=0x7E).contains(&b) && b != b'"' && b != b'\\')
            {
                return Err(ToolError::invalid_arguments(format!(
                    "label: 1..{LABEL_MAX} printable ASCII characters without quote or backslash, got {} bytes",
                    label.len()
                )));
            }
            Ok(label.to_string())
        }
        None => {
            let names = ops.iter().map(Op::name).collect::<Vec<_>>().join(", ");
            let mut label = format!("MCP: {names}");
            if label.len() > LABEL_MAX {
                // op names are static ASCII, but cut on a char boundary regardless
                while !label.is_char_boundary(LABEL_MAX) {
                    label.pop();
                }
                label.truncate(LABEL_MAX);
            }
            Ok(label)
        }
    }
}

/// The `ops` argument, deserialized exactly as `particle_edit` does (same bound, same texts).
fn ops_arg(args: &Value) -> Result<Vec<Op>, ToolError> {
    let ops: Vec<Op> = match args.get("ops") {
        Some(ops) => serde_json::from_value(ops.clone())
            .map_err(|error| ToolError::invalid_arguments(format!("ops: {error}")))?,
        None => return Err(particle_file::missing_arg("ops")),
    };
    if ops.is_empty() || ops.len() > MAX_OPS {
        return Err(ToolError::invalid_arguments(format!(
            "ops must hold 1..{MAX_OPS} entries, got {}",
            ops.len()
        )));
    }
    Ok(ops)
}

// ---------------------------------------------------------------- particle_live_read

/// `particle_live_read {function?}`: `pet.text`, parsed on the host — the document view, the
/// header, the root's non-array fields, the per-array counts and optionally one function entry
/// (the same shapes the file route's `particle_read` reports, with `source:"live"`).
pub async fn particle_live_read(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let function = match opt_str(&args, "function")? {
        None => None,
        Some(reference) => Some(reference.parse::<FunctionRef>().map_err(|error| {
            ToolError::invalid_arguments(format!("function: {error}"))
        })?),
    };

    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(
            "pet.text",
            json!({}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let result = match reply {
        Ok(reply) => reply.result,
        Err(e) => return Err(ToolError::from(e)),
    };
    let text = result
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::internal("pet.text replied without a text string"))?;
    let particle = Particle::parse(text).map_err(|error| live_text_unparseable(&error))?;

    let win_path = result.get("path").and_then(Value::as_str);
    let host = win_path.and_then(|p| state.path_map.windows_to_host(p));

    let mut out = Map::new();
    out.insert("source".into(), json!("live"));
    out.insert("path".into(), result.get("path").cloned().unwrap_or(Value::Null));
    out.insert(
        "hostPath".into(),
        host.map(|h| json!(h.display().to_string())).unwrap_or(Value::Null),
    );
    out.insert("dirty".into(), result.get("dirty").cloned().unwrap_or(Value::Null));
    out.insert("isNewFile".into(), result.get("isNewFile").cloned().unwrap_or(Value::Null));
    out.insert("canUndo".into(), result.get("canUndo").cloned().unwrap_or(Value::Null));
    out.insert("canRedo".into(), result.get("canRedo").cloned().unwrap_or(Value::Null));
    out.insert(
        "bytes".into(),
        result.get("bytes").cloned().unwrap_or_else(|| json!(text.len())),
    );
    out.insert("header".into(), json!(particle.doc.header.raw));
    out.insert("root".into(), particle_file::root_fields(&particle));
    out.insert("counts".into(), particle_file::counts(&particle));
    if let Some(function) = function {
        out.insert(
            "function".into(),
            particle
                .function_json(&function)
                .map_err(particle_file::vpcf_error)?,
        );
    }
    Ok(Value::Object(out).into())
}

// ---------------------------------------------------------------- particle_live_edit

/// `particle_live_edit {ops, label?, allowObsolete?, skipSchema?}`: read the live text, apply the
/// ops in memory (all-or-nothing, structure always, schema unless `skipSchema`), then `pet.apply`
/// the re-serialized text as one labelled undo step. Nothing is sent when the ops fail or the
/// result does not validate, or when the text is over the bridge's size gate.
pub async fn particle_live_edit(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let ops = ops_arg(&args)?;
    let label = edit_label(&args, &ops)?;
    let skip_schema = particle_file::bool_arg(&args, "skipSchema", false)?;
    let allow_obsolete = particle_file::bool_arg(&args, "allowObsolete", true)?;

    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(
            "pet.text",
            json!({}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let live = match reply {
        Ok(reply) => reply.result,
        Err(e) => return Err(ToolError::from(e)),
    };
    let live_text = live
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::internal("pet.text replied without a text string"))?;
    let mut particle = Particle::parse(live_text).map_err(|error| live_text_unparseable(&error))?;
    let outcome =
        particle_file::edit_in_memory(&state, &mut particle, &ops, skip_schema, allow_obsolete)?;

    let text = particle.write();
    if text.len() > PET_TEXT_MAX {
        return Err(text_too_large(text.len()));
    }

    let reply = bridge
        .request(
            "pet.apply",
            json!({"text": text, "label": label}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let result = match reply {
        Ok(reply) => reply.result,
        Err(e) => return Err(ToolError::from(e)),
    };

    // The document view: the apply reply's arrays (bridge rows) on a pet.active-shaped object
    // with path/isNewFile from pet.text and dirty/canUndo/canRedo from the apply.
    let doc_obj = json!({
        "path": live.get("path").cloned().unwrap_or(Value::Null),
        "isNewFile": live.get("isNewFile").cloned().unwrap_or(Value::Null),
        "dirty": result.get("dirty").cloned().unwrap_or(Value::Null),
        "arrays": result.get("arrays").cloned().unwrap_or(Value::Null),
    });
    let document = document_view(&state, &doc_obj);
    Ok(json!({
        "applied": outcome.applied,
        "appliedCount": outcome.applied.len(),
        "label": result.get("label").cloned().unwrap_or_else(|| json!(label)),
        "dirty": result.get("dirty").cloned().unwrap_or(Value::Null),
        "canUndo": result.get("canUndo").cloned().unwrap_or(Value::Null),
        "canRedo": result.get("canRedo").cloned().unwrap_or(Value::Null),
        "warnings": outcome.warnings,
        "schemaChecked": !skip_schema,
        "schemaSource": outcome.schema_source,
        "bytes": result.get("bytes").cloned().unwrap_or_else(|| json!(text.len())),
        "document": document,
    })
    .into())
}

// ---------------------------------------------------------------- particle_undo / particle_redo

/// `particle_undo {}`: one `pet.undo`; `done:false` is a retry-safe error, the document view is
/// built from the reply (the arrays as reported, `path` when the reply carries one).
pub async fn particle_undo(
    state: HammerState,
    ctx: CallCtx,
    _args: Value,
) -> Result<ToolOutput, ToolError> {
    undo_redo(state, ctx, "pet.undo").await
}

/// `particle_redo {}`: one `pet.redo` (same shape as [`particle_undo`]).
pub async fn particle_redo(
    state: HammerState,
    ctx: CallCtx,
    _args: Value,
) -> Result<ToolOutput, ToolError> {
    undo_redo(state, ctx, "pet.redo").await
}

async fn undo_redo(state: HammerState, ctx: CallCtx, method: &str) -> Result<ToolOutput, ToolError> {
    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(method, json!({}), RequestOpts::deadline(ctx.budget.deadline_ms()?))
        .await;
    let result = match reply {
        Ok(reply) => reply.result,
        Err(e) => return Err(ToolError::from(e)),
    };
    if result.get("done").and_then(Value::as_bool) != Some(true) {
        return Err(nothing_to_do(method, &result));
    }
    let doc_obj = json!({
        "path": result.get("path").cloned().unwrap_or(Value::Null),
        "dirty": result.get("dirty").cloned().unwrap_or(Value::Null),
        "arrays": result.get("arrays").cloned().unwrap_or(Value::Null),
    });
    let document = document_view(&state, &doc_obj);
    Ok(json!({
        "done": true,
        "dirty": result.get("dirty").cloned().unwrap_or(Value::Null),
        "canUndo": result.get("canUndo").cloned().unwrap_or(Value::Null),
        "canRedo": result.get("canRedo").cloned().unwrap_or(Value::Null),
        "document": document,
    })
    .into())
}
