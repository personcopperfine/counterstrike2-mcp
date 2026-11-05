//! Document lifecycle tools needed by the entity work: `save_map` (the only writer) and
//! `delete_nodes` (`node.delete`, undoable). Plan: target/entities-plan.md WP-T2b; policies ported
//! from the POC `h_save_map` / `h_delete_nodes` (`idabins/worknotes/mcpserver/server.py`).
//!
//! `save_map {}`: read `doc.active` first and refuse locally (nothing sent) when the document is
//! `editable:false` or when the mapped host file exists and is read-only — asking Hammer to save a
//! published/workshop map is the one thing this tool must never do by itself. Otherwise `map.save`
//! (the bridge checks editable/untitled itself, and takes no `expectDoc`). `-32015
//! UntitledDocument` / `-32018 DocumentNotEditable` pass through unchanged; the unsaved-edit
//! counter is reset to 0 only when the bridge reports `saved:true`. Nothing else is ever saved.
//!
//! `delete_nodes {nodeIDs}`: 1..=256 unique integers in 0..=2147483647 (the bridge casts to `int`),
//! validated locally (nothing is sent otherwise), then `node.delete` with the active document's
//! `expectDoc` fingerprint. Every id is validated by the bridge before anything is deleted (world
//! -> `-32602`, missing -> `-32022 EntityNotFound`, locked -> `-32018`) and the whole request is one
//! undo group. The unsaved-edit counter is bumped when the request executed (success) **and** when
//! it may have (`OutcomeUnknown`/`ConnectionLost`, or a protocol/internal error after dispatch:
//! the nodes may be gone), never when nothing was dispatched.
//!
//! `reload_map {force?}`: the bridge's `map.reload` (`ReloadFromDisk` + fingerprint epoch bump).
//! The tool reads `doc.active` first and refuses locally (nothing is sent) while the document is
//! **untitled** (a new document: `isNewFile` or no `path` — there is no file to reload from, the
//! `-32015 UntitledDocument` `map.save` refuses with too, and the bridge's `map.reload` arm now
//! answers with the same gate, m2.16), while the document is dirty or its dirty state is
//! unreported, unless `force` — the bridge answers `-32016 UnsavedChanges` for `dirty && !force`,
//! the tool pre-checks so dirty-*unknown* is refused too. The request carries the `doc.active`
//! `expectDoc` fingerprint (m2.16, review L2): a document switch between the check and the reload is
//! the bridge's `-32004 DocumentChanged` instead of a reload of the other document.
//! `-32003 NoActiveDocument` / `-32018 DocumentNotEditable` pass through unchanged; the
//! unsaved-edit counter is read *after* `require_document` (a document switch resets it) and reset
//! to 0 only on a reply that confirms `reloaded:true` — any other success shape is treated as
//! maybe-executed: the counter is kept and a note says the reload may not have taken (review M1;
//! `reloaded:true` is "the reload call was made", because `ReloadFromDisk` has no return value to
//! check — bridge/s2sdk.h, protocol-diff-m2.16.md).
//!
//! `open_map {path}`: the bridge's `map.open` (m2.16, `CBaseEditorApp::OpenFile` with flags 1 —
//! the call the UI makes). A dirty document is *not* a refusal: opening adds a session, it does not
//! replace one, so the current session keeps its unsaved state. The path shape is validated here
//! (`maps/…vmap`, 1..512, canonical: forward slashes, no `..`/`.`/`//` segment, no segment ending in
//! `.` or a space, no `:`/controls) so nothing garbled reaches `OpenFile`'s modal boxes; the map
//! file itself is parsed here with this crate's own reader (review H1: `InvalidMapFile`, nothing
//! sent) because a load failure pops the modal "Error opening file" on the tools main thread and
//! wedges the bridge — and a file that is not readable on the host at all is refused first with
//! `-32014 AssetNotFound` (review-5 L4: the bridge only sniffs 512 header bytes, so a
//! valid-header/truncated-body file would otherwise reach the modal). The bridge then gates the
//! host file, the DMX header,
//! and answers `{opened:false, existing:true}` for a file that is already open in a session —
//! Hammer's own session match is a `xor al,al; ret` stub, so calling `OpenFile` again would create
//! a SECOND session for the same file and leak the first. That answer is a result with a note (not
//! an error); the note points at `reload_map` for the active session only (review-5 M1: for a
//! background session `reload_map` would act on the active document, a different map).
use crate::state::HammerState;
use cs2_mcp_common::bridge::{BridgeError, RequestOpts};
use cs2_mcp_common::tools::{
    require_caps_within, CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec, CAPS_LOCK_WAIT,
};
use serde_json::{json, Map, Value};
use std::path::Path;

/// Review L7: the plumbing shared with `entities_live` and `mesh_live` (the active-document read
/// with its `expectDoc` fingerprint, the unsaved-edit counter, the maybe-executed taxonomy) has one
/// home.
pub(crate) use super::live_common::{
    expect_doc, maybe_executed, note_edit, require_document, MAX_NODE_ID,
};

/// The POC's `nodeIDs` bound (`op_node_delete`: up to 256 ids per request).
pub(crate) const MAX_NODE_IDS: usize = 256;
/// `map.open`'s path bound (the same 512 modeldoc.open/pet.open use; `bridge/map_path.h`).
const MAP_PATH_MAX: usize = 512;

pub fn register_save(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "save_map",
            "Save the live map to its path (the only tool that writes the .vmap). Refuses a read-only file. Check saved:true.",
            Writer,
            &["map.save"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })),
        save_map,
    );
}

pub fn register_delete(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "delete_nodes",
            "Delete meshes/entities/groups by nodeID in the live document (one undo group, undo_last restores; never saves).",
            Mutating,
            &["node.delete"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "nodeIDs": {
                    "type": "array",
                    "items": {"type": "integer", "minimum": 0, "maximum": MAX_NODE_ID},
                    "minItems": 1,
                    "maxItems": MAX_NODE_IDS,
                    "uniqueItems": true,
                    "description": "unique node ids; world cannot be deleted"
                }
            },
            "required": ["nodeIDs"],
            "additionalProperties": false
        })),
        delete_nodes,
    );
}

pub fn register_open(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "open_map",
            "Open a .vmap in Hammer. Path must be a canonical maps/... .vmap of the addon that parses as binary v9 (else InvalidMapFile). Already open = existing:true, not an error; if it is in a background tab the reply says active:false (no tool switches tabs).",
            Mutating,
            // not gated by the registry: a bridge without map.open is the first-map route, which
            // `open_map` decides itself (the gate would answer NotImplemented with no way forward)
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "addon-relative, e.g. maps/de_probe.vmap"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })),
        open_map,
    );
}

pub fn register_reload(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "reload_map",
            "Reload the live map from disk, discarding unsaved edits and the undo history. Refuses an untitled document, and a dirty one unless force.",
            Mutating,
            &["map.reload"],
        )
        .destructive(true)
        .with_schema(json!({
            "type": "object",
            "properties": {
                "force": {
                    "type": "boolean",
                    "description": "discard unsaved edits"
                }
            },
            "additionalProperties": false
        })),
        reload_map,
    );
}

// -------------------------------------------------------------------------------------------
// Shared plumbing (`live_common` holds what every live mutator shares, review L7; what is left
// here is this file's own: map paths, dirty/untitled gates, save/delete/reload replies)
// -------------------------------------------------------------------------------------------

/// `ReadOnlyMap`: refused before dispatch, so retrying changes nothing (POC `h_save_map`).
fn read_only_map(message: String, data: Value) -> ToolError {
    ToolError::new("ReadOnlyMap", "ReadOnlyMap", message)
        .with_data(data)
        .retry_safe(true)
}

/// `_reset_edits`: the document is clean again (and its identity is recorded).
fn reset_edits(state: &HammerState, doc_id: Option<&str>) {
    if let Ok(mut edits) = state.edits.lock() {
        edits.since_save = 0;
        edits.doc_id = doc_id.map(str::to_string);
    }
}

/// Review-1 L2, a **note** (never a refusal): a `compile_map` job is running for the map this
/// `save_map`/`reload_map` just touched. The compiler rereads the `.vmap` for its later passes
/// (`vrad3 -map maps/<x>.vmap` starts minutes in), so what the job ends with need not be what the
/// document holds now; the caller's move is to re-run `compile_map` once the job ends. `path` is the
/// document's Windows path; a path outside the addon (or no path) names no compilable map and gets
/// no note.
fn running_compile_note(state: &HammerState, path: Option<&str>, what: &str) -> Option<Value> {
    let host = state.path_map.windows_to_host(path?)?;
    let paths = state.content();
    let content = paths.addon_rel(&host)?;
    let job = state.compile_jobs.running_for_path(&paths.addon, &content)?;
    Some(json!(format!(
        "{what}: a compile_map job ({job}) is running for this map ({content}), and the compiler \
         may reread the .vmap for its later passes (lighting), so the job's output need not match \
         the file this call left — re-run compile_map after the job ends"
    )))
}

/// The host file is read-only (`chmod`-style): asking Hammer to save would fail (or write through
/// a published map's read-only mount) — refuse instead. A file that does not exist yet is not
/// read-only: `map.save` may create it.
fn host_is_read_only(host: &Path) -> bool {
    match std::fs::metadata(host) {
        Ok(metadata) if metadata.is_file() => metadata.permissions().readonly(),
        _ => false,
    }
}

/// `nodeIDs`: 1..=[`MAX_NODE_IDS`] unique integers in `0..=`[`MAX_NODE_ID`] (the bridge checks the
/// same bounds again; duplicates would delete the same node twice in one undo group, and an id
/// above the bridge's `int` range truncates to a different node).
fn node_ids_arg(args: &Value) -> Result<Vec<i64>, ToolError> {
    let Some(values) = args.get("nodeIDs").and_then(Value::as_array) else {
        return Err(ToolError::invalid_arguments(format!(
            "nodeIDs: an array of 1..{MAX_NODE_IDS} unique integers in 0..={MAX_NODE_ID} is required"
        )));
    };
    if values.is_empty() {
        return Err(ToolError::invalid_arguments(
            "nodeIDs: empty, there is nothing to delete",
        ));
    }
    if values.len() > MAX_NODE_IDS {
        return Err(ToolError::invalid_arguments(format!(
            "nodeIDs: {} ids given, at most {MAX_NODE_IDS} fit in one request",
            values.len()
        )));
    }
    let mut ids: Vec<i64> = Vec::with_capacity(values.len());
    for value in values {
        let id = value
            .as_i64()
            .filter(|id| (0..=MAX_NODE_ID).contains(id))
            .ok_or_else(|| {
                ToolError::invalid_arguments(format!(
                    "nodeIDs: must be integers in 0..={MAX_NODE_ID}, got {value}"
                ))
            })?;
        if ids.contains(&id) {
            return Err(ToolError::invalid_arguments(format!(
                "nodeIDs: duplicate nodeID {id}"
            )));
        }
        ids.push(id);
    }
    Ok(ids)
}

// -------------------------------------------------------------------------------------------
// save_map
// -------------------------------------------------------------------------------------------

/// The step after a save: compile the map, but only when the bridge actually saved (`saved:true`).
fn save_next(output: ToolOutput, saved: bool) -> ToolOutput {
    if saved {
        output.with_next("compile_map", "build the saved map into a .vpk (returns a jobId to poll)")
    } else {
        output.no_next()
    }
}

#[cfg(test)]
mod next_tests {
    use super::*;

    #[test]
    fn a_save_that_did_not_save_does_not_point_at_compile_map() {
        let unsaved = save_next(json!({"saved": false}).into(), false);
        assert_eq!(unsaved.next, Some(Vec::new()));
        let saved = save_next(json!({"saved": true}).into(), true);
        assert_eq!(saved.next.unwrap()[0].tool, "compile_map");
    }
}

/// `save_map {}`: the only writer. Local read-only refusals first (nothing sent), then
/// `map.save`; the unsaved-edit counter is reset to 0 only on `saved:true`.
pub async fn save_map(
    state: HammerState,
    ctx: CallCtx,
    _args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    let doc = require_document(&state, ctx.budget.deadline_ms()?).await?;

    if doc.get("editable").and_then(Value::as_bool) == Some(false) {
        return Err(read_only_map(
            format!(
                "the active document is not editable ({}); refusing to ask Hammer to save",
                doc.get("notEditableReason")
                    .and_then(Value::as_str)
                    .unwrap_or("reason unknown")
            ),
            json!({
                "notEditableReason": doc.get("notEditableReason").cloned().unwrap_or(Value::Null),
                "document": expect_doc(&doc),
            }),
        ));
    }

    let windows = doc.get("path").and_then(Value::as_str).unwrap_or("");
    let host = state.path_map.windows_to_host(windows);
    if let Some(host) = host.as_ref().filter(|host| host_is_read_only(host)) {
        return Err(read_only_map(
            format!(
                "the map file is read-only on the host: {} (published/workshop map?); refusing to \
                 ask Hammer to save",
                host.display()
            ),
            json!({
                "hostPath": host.display().to_string(),
                "windowsPath": windows,
                "document": expect_doc(&doc),
            }),
        ));
    }

    let deadline = ctx.budget.deadline_ms()?;
    let mut bridge = state.bridge.lock().await;
    // `map.save` takes no `expectDoc` (mcp_bridge.c: it checks editable/untitled itself)
    let reply = bridge
        .request("map.save", json!({}), RequestOpts::deadline(deadline))
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => return Err(ToolError::from(error)),
    };
    drop(bridge);

    let saved = reply.result.get("saved").and_then(Value::as_bool).unwrap_or(false);
    if saved {
        reset_edits(&state, doc.get("id").and_then(Value::as_str));
    }
    let path = reply
        .result
        .get("path")
        .filter(|path| path.as_str().is_some_and(|p| !p.is_empty()))
        .or_else(|| doc.get("path"))
        .cloned()
        .unwrap_or(Value::Null);
    let host_path = path
        .as_str()
        .and_then(|path| state.path_map.windows_to_host(path));

    let mut out = Map::new();
    out.insert("saved".into(), json!(saved));
    out.insert(
        "skippedClean".into(),
        reply
            .result
            .get("skippedClean")
            .cloned()
            .unwrap_or(Value::Null),
    );
    out.insert("path".into(), path.clone());
    out.insert(
        "hostPath".into(),
        json!(host_path.as_ref().map(|path| path.display().to_string())),
    );
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    // L2: the compile of this very map (if one is running) rereads the file this save wrote
    if saved {
        if let Some(note) = running_compile_note(&state, path.as_str(), "save_map") {
            out.insert("notes".into(), json!([note]));
        }
    }
    Ok(save_next(Value::Object(out).into(), saved))
}

// -------------------------------------------------------------------------------------------
// delete_nodes
// -------------------------------------------------------------------------------------------

/// The `node.delete` failure in the entity tools' taxonomy (brief item 10): `-32022` is
/// `EntityNotFound` naming the ids, everything else keeps the wire mapping. When the delete may
/// have run (`live_common::maybe_executed`), the error is marked `outcomeUnknown` and the counter is
/// bumped.
fn delete_error(error: BridgeError, ids: &[i64], state: &HammerState) -> ToolError {
    let maybe = maybe_executed(&error);
    let mut tool = if error.code == Some(-32022) {
        let reason = error
            .data
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("not found")
            .to_string();
        ToolError::new(
            "EntityNotFound",
            "EntityNotFound",
            format!(
                "no live node with nodeIDs {ids:?} ({reason}); nothing was deleted by this call \
                 (the bridge validates every id before deleting any)"
            ),
        )
        .with_code(-32022)
        .with_data(json!({"nodeIDs": ids, "reason": reason}))
        .retry_safe(true)
    } else {
        ToolError::from(error)
    };
    if maybe {
        tool.outcome_unknown = true;
        tool.data
            .insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    }
    tool
}

/// `delete_nodes {nodeIDs}`: local bounds check, then `node.delete` with `expectDoc`; the counter
/// is bumped when the request executed and when the outcome is unknown.
pub async fn delete_nodes(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    let ids = node_ids_arg(&args)?;
    delete_nodes_locked(&state, &ctx, ids).await
}

/// [`delete_nodes`]'s body for a caller that already holds [`HammerState::write_gate`] and has
/// validated the ids (`new_map {clear}` deletes the template's nodes under its own gate): one
/// `node.delete` request, so one undo group.
pub(crate) async fn delete_nodes_locked(
    state: &HammerState,
    ctx: &CallCtx,
    ids: Vec<i64>,
) -> Result<ToolOutput, ToolError> {
    let state = state.clone();
    let doc = require_document(&state, ctx.budget.deadline_ms()?).await?;
    let doc_id = doc.get("id").and_then(Value::as_str).map(str::to_string);

    let deadline = ctx.budget.deadline_ms()?;
    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(
            "node.delete",
            json!({"nodeIDs": ids}),
            RequestOpts {
                deadline_ms: Some(deadline),
                expect_doc: Some(expect_doc(&doc)),
                gate: None,
            },
        )
        .await;
    drop(bridge);

    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            // OutcomeUnknown/ConnectionLost/protocol: the delete may have run on Hammer's side
            if maybe_executed(&error) {
                note_edit(&state, doc_id.as_deref());
            }
            return Err(delete_error(error, &ids, &state));
        }
    };
    note_edit(&state, doc_id.as_deref());

    let mut out = match reply.result {
        Value::Object(result) => result,
        other => {
            return Err(ToolError::internal(format!(
                "node.delete returned a non-object result: {other} \
                 (bridge/protocol-diff-m2.15.md §7)"
            )))
        }
    };
    // m2.17: the reply carries the document state the delete left behind (`doc`); this tool does
    // not consume it, so it is not echoed as a result field.
    out.remove("doc");
    out.insert("saved".into(), json!(false));
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    Ok(Value::Object(out).into())
}

// -------------------------------------------------------------------------------------------
// reload_map
// -------------------------------------------------------------------------------------------

/// `Some(true|false)` when the document reports `dirty`, `None` when it does not say (a missing or
/// non-boolean `dirty` counts as unknown: refuse rather than risk discarding edits — same rule as
/// `model_reload`/`particle_reload`). Shared with the mesh rebuild operators (`tools/mesh_live`),
/// which refuse a dirty-*unknown* document for the same reason.
pub(crate) fn dirty_state(doc: &Value) -> Option<bool> {
    doc.get("dirty").and_then(Value::as_bool)
}

/// `-32016 UnsavedChanges` before reload (for `dirty && !force` the bridge answers the same code,
/// `{"reason":"document is dirty; pass force:true to discard"}`; the tool pre-checks so
/// dirty-*unknown* is refused as well). Data carries the reason, the document path and the
/// server's unsaved-edit counter.
fn unsaved_changes(doc: &Value, unsaved: u64) -> ToolError {
    match dirty_state(doc) {
        Some(true) => dirty_document(doc, unsaved),
        _ => dirty_unknown_document(doc, unsaved),
    }
}

/// `-32016` with the dirty wording (review L1): the bridge only refuses `map.reload` with it when
/// the reload arm sees `DocOrDependencyChanged` — a *wider* predicate than `doc.active`'s
/// `ChangedSinceSave`, so a document whose dependency changed passes the pre-check with
/// `dirty:false` and is then refused on the wire. That wording (which the tool builds for
/// `dirty_state(doc) == Some(true)`) is the right one for the wire too.
fn dirty_document(doc: &Value, unsaved: u64) -> ToolError {
    let message = "the active Hammer document is dirty (reloading from disk would discard the \
                   unsaved in-editor edits); pass force:true to discard them";
    let reason = "document is dirty; pass force:true to discard";
    unsaved_changes_error(doc, unsaved, message, reason)
}

/// `-32016` with the dirty-*unknown* wording (the document does not report `dirty` at all). Used
/// only for the local pre-check refusal: the wire path always takes [`dirty_document`] and keeps
/// the bridge's own `data.reason` (review L1).
fn dirty_unknown_document(doc: &Value, unsaved: u64) -> ToolError {
    let message = "the active document's dirty state is not reported; refusing to risk unsaved \
                   edits, pass force:true to discard";
    let reason = "dirty state unreported; pass force:true to discard";
    unsaved_changes_error(doc, unsaved, message, reason)
}

/// The `UnsavedChanges` refusal body shared by [`dirty_document`] / [`dirty_unknown_document`].
fn unsaved_changes_error(doc: &Value, unsaved: u64, message: &str, reason: &str) -> ToolError {
    let message = if unsaved > 0 {
        format!("{message} ({unsaved} unsaved bridge edits counted on this server)")
    } else {
        message.to_string()
    };
    ToolError::new("UnsavedChanges", "UnsavedChanges", message)
        .with_code(-32016)
        .retry_safe(true)
        .with_data(json!({
            "reason": reason,
            "path": doc.get("path").cloned().unwrap_or(Value::Null),
            "unsavedBridgeEdits": unsaved,
        }))
}

/// `-32015 UntitledDocument` refused locally, before anything is sent (review M1): a document with
/// no file behind it (`isNewFile` or an empty/missing `path`) has nothing to reload from disk.
/// This is the same error the bridge/mock answer for `map.save`; the bridge's `map.reload` arm
/// answers with the same gate (m2.16). `force` does not unlock it.
fn untitled_document(doc: &Value, unsaved: u64) -> ToolError {
    let message = if unsaved > 0 {
        format!(
            "the active document is untitled (no path on disk): reloading from disk has no file \
             to load — save it first ({unsaved} unsaved bridge edits counted on this server, \
             they are not affected)"
        )
    } else {
        "the active document is untitled (no path on disk): reloading from disk has no file to \
         load — save it first"
            .to_string()
    };
    ToolError::new("UntitledDocument", "UntitledDocument", message)
        .with_code(-32015)
        .retry_safe(true)
        .with_data(json!({
            "path": doc.get("path").cloned().unwrap_or(Value::Null),
            "unsavedBridgeEdits": unsaved,
        }))
}

/// `true` for a new/untitled document: `isNewFile` stated, or no `path` at all (the mock's
/// `map.save` gate: `w.new_file || w.path.is_empty()`). Shared with the mesh rebuild operators
/// (`tools/mesh_live`), whose SAVED-`.vmap` geometry source needs a file behind the document.
pub(crate) fn is_untitled(doc: &Value) -> bool {
    doc.get("isNewFile")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || doc
            .get("path")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
}

/// `map.reload` answered `-32016` on the wire (the document became dirty — or a dependency
/// changed — between `doc.active` and the request): surface the refusal with the dirty wording and
/// keep the bridge's own `data.reason` (`insert`, not `or_insert`: the bridge's predicate decided
/// the refusal, its text wins — review L1; the dirty-*unknown* wording is only for the local
/// pre-check refusal).
fn wire_unsaved_changes(doc: &Value, unsaved: u64, error: &BridgeError) -> ToolError {
    let mut tool = dirty_document(doc, unsaved);
    for (k, v) in &error.data {
        tool.data.insert(k.clone(), v.clone());
    }
    tool
}

/// `reload_map {force?}`: refuse a new/untitled document (review M1, `-32015`, nothing is sent)
/// and a dirty/unknown document without `force` (nothing is sent), then `map.reload` with the
/// active document's `expectDoc` (review L2: the bridge answers `-32004 DocumentChanged` when the
/// document moved since the read, instead of reloading the other one); the
/// unsaved-edit counter is read after `require_document` (L3: a document switch resets it) and
/// reset to 0 only when the reply confirms `reloaded:true` (M1) — the map now equals the file.
pub async fn reload_map(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
    let doc = require_document(&state, ctx.budget.deadline_ms()?).await?;
    // after require_document: note_document (the document switch reset) has already run, so this
    // is this document's count, not the previous document's (review L3)
    let unsaved = state.unsaved_edits();
    if is_untitled(&doc) {
        return Err(untitled_document(&doc, unsaved));
    }
    if dirty_state(&doc).unwrap_or(true) && !force {
        return Err(unsaved_changes(&doc, unsaved));
    }

    let deadline = ctx.budget.deadline_ms()?;
    let mut bridge = state.bridge.lock().await;
    // `map.reload` takes the same `expectDoc` fingerprint the other mutators send (the bridge's arm
    // checks it after editable and before the untitled/dirty gates): a tab switch between the
    // `doc.active` above and this call is `-32004 DocumentChanged` instead of reloading - and, with
    // `force`, discarding - the *other* document (review L2).
    let reply = bridge
        .request(
            "map.reload",
            json!({"force": force}),
            RequestOpts {
                deadline_ms: Some(deadline),
                expect_doc: Some(expect_doc(&doc)),
                gate: None,
            },
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            return Err(match error.code {
                Some(-32016) => wire_unsaved_changes(&doc, unsaved, &error),
                // -32004 DocumentChanged (the expectDoc gate, review L2) and -32015
                // UntitledDocument (M1) pass through as their own error types, retry-safe:
                // the reload did not run
                _ => ToolError::from(error),
            });
        }
    };
    drop(bridge);
    let reloaded = reply
        .result
        .get("reloaded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // the reload discarded (or had nothing to discard): the map equals the file again, exactly
    // like save_map on saved:true — but only when the bridge confirmed the reload (M1): an
    // unconfirmed success shape ("reloaded":null/absent/false) may have done nothing, so the
    // counter is kept and the caller is told to verify
    if reloaded {
        reset_edits(&state, doc.get("id").and_then(Value::as_str));
    }
    let mut notes: Vec<Value> = vec![];
    if !reloaded {
        notes.push(json!(
            "map.reload did not confirm reloaded:true: the reload may not have taken, so the \
             unsaved-edit counter is kept — verify the document (get_entity / the file) \
             before trusting this call"
        ));
    }

    let path = reply
        .result
        .get("path")
        .filter(|path| path.as_str().is_some_and(|p| !p.is_empty()))
        .or_else(|| doc.get("path"))
        .cloned()
        .unwrap_or(Value::Null);
    let host_path = path
        .as_str()
        .and_then(|path| state.path_map.windows_to_host(path));

    // L2: a compile of this very map that is running now rereads the file this reload took the
    // document from
    if reloaded {
        if let Some(note) = running_compile_note(&state, path.as_str(), "reload_map") {
            notes.push(note);
        }
    }

    let mut out = Map::new();
    out.insert(
        "reloaded".into(),
        reply
            .result
            .get("reloaded")
            .cloned()
            .unwrap_or(Value::Null),
    );
    out.insert("path".into(), path);
    out.insert(
        "hostPath".into(),
        json!(host_path.as_ref().map(|path| path.display().to_string())),
    );
    out.insert(
        "discardedChanges".into(),
        reply
            .result
            .get("discardedChanges")
            .cloned()
            .unwrap_or(Value::Null),
    );
    out.insert(
        "epoch".into(),
        reply.result.get("epoch").cloned().unwrap_or(Value::Null),
    );
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    if !notes.is_empty() {
        out.insert("notes".into(), Value::Array(notes));
    }
    Ok(Value::Object(out).into())
}

// -------------------------------------------------------------------------------------------
// open_map
// -------------------------------------------------------------------------------------------

/// The `map.open` path shape, refused locally so a garbled path is **never sent** (the bridge
/// re-checks all of it; `OpenFile` pops a modal box on a bad one and blocks the editor's main
/// thread until a person clicks it). One rule per reason, in the bridge's order
/// (`bridge/map_path.h`): addon-content-relative, `maps/…vmap`, at most 512 bytes, printable, no
/// `:` (a Windows name cannot carry one — that would be an alternate data stream), no `\` (the
/// canonical spelling is forward slashes), no `"` (serde escapes it and the bridge's arm then
/// refuses the raw parameter), no `..`, and — review M1 — no empty segment (`//`), no `.`/`..`
/// segment and no segment ending in `.` or a space: Win32 opens every one of those for the same
/// file, so a second spelling would miss the bridge's session-leak gate and leak a session.
/// Shared by `open_map`, `run_map` and `compile_map`; `run_map` derives the engine's map name (a
/// console argument) from this same canonical spelling.
pub(crate) fn map_content_path(args: &Value) -> Result<String, ToolError> {
    let bad = |why: &str| ToolError::invalid_arguments(format!("path: {why}"));
    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
    if path.is_empty() {
        return Err(bad(
            "a non-empty path relative to the addon content root (maps/... .vmap) is required",
        ));
    }
    if path.len() > MAP_PATH_MAX {
        return Err(bad("1..512 chars"));
    }
    if path.starts_with(['/', '\\']) || path.as_bytes().get(1) == Some(&b':') {
        return Err(bad("relative to the addon content root (maps/...), not absolute"));
    }
    // one pass over the bytes, exactly like the bridge's loop: the FIRST offending byte decides
    for b in path.bytes() {
        if b < 0x20 || b == 0x7f {
            return Err(bad("no control characters"));
        }
        if b == b':' {
            return Err(bad("no ':'"));
        }
        if b == b'\\' {
            return Err(bad("no '\\' (use '/' separators)"));
        }
    }
    if path.contains('"') {
        return Err(bad("a quote"));
    }
    let lower = path.to_ascii_lowercase();
    if !lower.ends_with(".vmap") {
        return Err(bad("a .vmap file"));
    }
    if !lower.starts_with("maps/") {
        return Err(bad("under maps/ (the addon's map directory)"));
    }
    if lower.len() <= "maps/".len() + ".vmap".len() {
        return Err(bad("a map file name under maps/"));
    }
    if lower.contains("..") {
        return Err(bad("no '..'"));
    }
    for segment in path["maps/".len()..].split('/') {
        if segment.is_empty() {
            return Err(bad("no empty path segment ('//')"));
        }
        if segment == "." || segment == ".." {
            return Err(bad("no '.' or '..' path segment"));
        }
        if segment.ends_with('.') || segment.ends_with(' ') {
            return Err(bad("no path segment ending in '.' or a space"));
        }
    }
    Ok(path.to_string())
}

/// `WrongEngineAddon`: `map.open` resolves maps under the engine's `-addon`, not the selected one.
pub(crate) fn wrong_engine_addon(
    content_path: &str,
    selected: &str,
    engine_addon: &str,
    engine_source: &str,
) -> ToolError {
    ToolError::new(
        "WrongEngineAddon",
        "WrongEngineAddon",
        format!(
            "{content_path} is a path of addon {selected:?}, but Hammer's engine runs with \
             -addon {engine_addon:?} and map.open resolves maps under that addon only: \
             opening a map of another addon is not possible in this session. Start the \
             tools session on that addon (runtime ADDON={selected}, e.g. \
             `ADDON={selected} bridge/runtime.sh up`) and start hammer-mcp with --addon \
             {selected}, or open a map of {engine_addon:?}"
        ),
    )
    .with_code(-32602)
    .with_data(json!({
        "path": content_path,
        "reason": "wrong_engine_addon",
        "engineAddon": engine_addon,
        "engineAddonSource": engine_source,
        "selectedAddon": selected,
    }))
    .retry_safe(true)
}

/// REVIEW H1(a): parse the map with this crate's own reader **before** anything is sent. A file
/// that fails to load makes Hammer's `OpenFile` pop the modal "Error opening file" QMessageBox on
/// the tools main thread; the drain's re-entrancy guard then blocks every queued request and nobody
/// clicks in the headless setup. The bridge's DMX header sniff (H1(b)) is the second line of
/// defence; this one parses the whole file.
///
/// review-5 L4: a file that is not readable on the host is refused locally (`-32014
/// AssetNotFound`, nothing sent) instead of skipping the parse and sending — the old behaviour
/// meant a case spelling difference (`MAPS/Probe.vmap`) or a stale host mapping had only the
/// bridge's 512-byte header sniff standing between a valid-header/truncated-body `.vmap` and the
/// modal. The shape gate has already accepted the path, so here a missing host file can only be
/// a wrong name (the bridge resolves the Windows side **case-insensitively**, the host does not)
/// or a stale `--path-map`; both surface as the same answer the bridge would have given
/// for a name it cannot resolve (`-32014 AssetNotFound`), and the whole-file pre-parse is what
/// makes the local refusal the safer shape.
async fn precheck_map_file(
    state: &HammerState,
    content_path: &str,
) -> Result<(String, &'static str), ToolError> {
    // The bridge's `map.open` resolves `maps/...` under the ENGINE's own `-addon`, not under
    // `set_addon`'s selection: the precheck looks where the bridge will. The runtime labels the
    // tools container `cs2mcp.addon=<ADDON>`; `--addon` is only the fallback when that cannot be
    // read (no podman, no label), because it can differ from what the runtime was started with.
    let (engine_addon, engine_source) = engine_addon(state).await;
    let engine_content = crate::vmdl::ContentPaths::new(state.app_root(), engine_addon.clone());
    let host = engine_content.addon_root().join(content_path);
    if !host.is_file() {
        let selected = state.addon();
        if !selected.eq_ignore_ascii_case(&engine_addon)
            && state.content().addon_root().join(content_path).is_file()
        {
            return Err(wrong_engine_addon(content_path, &selected, &engine_addon, engine_source));
        }
        let variant = super::new_map::case_variant(&engine_content.addon_root(), content_path);
        let error = ToolError::new(
            "AssetNotFound",
            "AssetNotFound",
            format!(
                "{content_path} is not readable under the mapped host addon root \
                 ({}): the local pre-parse (review H1) refuses a map file it cannot \
                 read instead of sending it — Hammer's OpenFile would pop its \"Error opening \
                 file\" modal on the tools main thread for a valid-header/truncated-body file \
                 the bridge's header sniff alone cannot reject. If the map does not exist yet, \
                 new_map {{path}} creates it from a template and opens it; otherwise check the \
                 mapping and the spelling (Windows compares case-insensitively, this host \
                 does not)",
                host.display()
            ),
        )
        .with_code(-32014)
        .with_data(json!({"path": content_path, "reason": "missing_host_file",
                          "engineAddon": engine_addon, "engineAddonSource": engine_source}))
        .retry_safe(true);
        return Err(match variant {
            // the file exists under another spelling: this host is case-sensitive, Windows is not
            Some(real) => error.did_you_mean([real]),
            None => error.use_instead(
                "new_map",
                "the file does not exist in any spelling: create it from a template (and open it)",
                Some(json!({"path": content_path})),
            ),
        });
    }
    crate::vmap::load(&host).map(|_| (engine_addon, engine_source)).map_err(|error| {
        ToolError::new(
            "InvalidMapFile",
            "InvalidMapFile",
            format!(
                "{content_path} is not a readable binary-v9 .vmap ({error}): Hammer's OpenFile \
                 would pop its \"Error opening file\" modal on the tools main thread and wedge the \
                 bridge, so nothing was sent"
            ),
        )
        .with_data(json!({"path": content_path, "reason": "InvalidMapFile"}))
        .retry_safe(true)
    })
}

/// The addon the Hammer tools container's engine runs with, and where that came from
/// (`"container"` label / Cmd, or the `--addon` `"flag"` fallback).
pub(crate) async fn engine_addon(state: &HammerState) -> (String, &'static str) {
    let container = match &state.tools_container {
        Some(name) => Some(name.clone()),
        None => state
            .runtime
            .container_by_label("cs2mcp.tool=hammer")
            .await
            .ok()
            .flatten(),
    };
    if let Some(container) = container {
        if let Ok(Some(addon)) = state.runtime.container_addon(&container).await {
            return (addon, "container");
        }
    }
    let flag = state
        .addon_sel
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .flag
        .clone();
    (flag, "flag")
}

/// `open_map {path}`: the local shape check and the local vmap parse, then `map.open`. A map file
/// this host cannot read is refused first (`-32014 AssetNotFound`, nothing sent — review-5 L4: the
/// old "send and let the bridge gate it" path left a valid-header/truncated-body file reachable,
/// because the bridge only sniffs 512 bytes of header), then the bridge's arm does the DMX header
/// gate (`-32014 ... {reason:"InvalidMapFile"}`) and the session-leak gate: a file Hammer already
/// has open answers `{opened:false, existing:true}` and this tool reports it as a result with a
/// note instead of an error (nothing was opened twice; for the ACTIVE session `reload_map` is the
/// tool that re-reads the file — an `existing:true, active:false` background session does not get
/// the `reload_map` suggestion, review-5 M1). `opened:false` without `existing` is the bridge's
/// `OpenFile` answering 0: `OpenFailed` (**not** retry-safe — the load failure that popped the modal
/// is what a retry pops again), the same error `model_open`/`particle_open` raise for it.
pub async fn open_map(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    open_map_locked(&state, &ctx, &args).await
}

/// [`open_map`]'s body, for a caller that already holds [`HammerState::write_gate`] (`new_map`
/// creates the file and opens it under one gate).
pub(crate) async fn open_map_locked(
    state: &HammerState,
    ctx: &CallCtx,
    args: &Value,
) -> Result<ToolOutput, ToolError> {
    let content_path = map_content_path(args)?;
    // the capability gate the registry used to apply, done here so a bridge without `map.open`
    // can take the first-map route instead of ending in NotImplemented
    let via_vtm = match require_caps_within(
        &state.bridge,
        &["map.open"],
        CAPS_LOCK_WAIT.min(ctx.budget.remaining()),
    )
    .await
    {
        Ok(()) => None,
        Err(error) if error.name == "NotImplemented" => {
            // Only "hammer.dll is not loaded yet" may take the vtm route. With hammer.dll loaded
            // (an older bridge, a module mismatch) `open_asset` would bypass the bridge's
            // session-leak guard and could open a second session of a map that is already open.
            if !hammer_module_not_loaded(state, CAPS_LOCK_WAIT.min(ctx.budget.remaining())).await {
                return Err(error.with_data(json!({
                    "hint": "hammer.dll is loaded (or its state is unknown) but the bridge does not \
                             advertise map.open: an older bridge build or a module mismatch. The \
                             VToolMessage open_asset route is only used while hammer.dll is not \
                             loaded, because it bypasses the bridge's session-leak guard. Check \
                             session_status (bridge version, modules)",
                })));
            }
            match super::run_map::tools_container(state).await {
                Ok(container) => Some(container),
                Err(_) => {
                    return Err(error.with_data(json!({
                        "hint": "hammer.dll is not loaded until a document opens, and no running \
                                 tools container was found to open the first map over the \
                                 VToolMessage channel. Start the session (runtime.sh up), or \
                                 `bridge/runtime.sh open <maps/x.vmap>`",
                    })))
                }
            }
        }
        Err(error) => return Err(error),
    };
    let (engine_addon, engine_source) = precheck_map_file(state, &content_path).await?;
    if let Some(container) = via_vtm {
        return open_first_map(state, ctx, &container, &content_path, &engine_addon, engine_source)
            .await;
    }

    let deadline = ctx.budget.deadline_ms()?;
    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(
            "map.open",
            json!({"path": content_path}),
            RequestOpts::deadline(deadline),
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => return Err(ToolError::from(error)),
    };
    drop(bridge);

    let result = reply.result;
    let opened = result.get("opened").and_then(Value::as_bool).unwrap_or(false);
    let existing = result.get("existing").and_then(Value::as_bool).unwrap_or(false);
    if !opened && !existing {
        return Err(ToolError::new(
            "OpenFailed",
            "OpenFailed",
            format!(
                "Hammer reported opened:false for {content_path:?} (OpenFile returned 0: the editor \
                 was already opening another file, or the load failed)"
            ),
        )
        .retry_safe(false)
        .with_data(result));
    }

    let path = result
        .get("path")
        .filter(|path| path.as_str().is_some_and(|p| !p.is_empty()))
        .cloned()
        .unwrap_or(Value::Null);
    let host_path = path
        .as_str()
        .and_then(|path| state.path_map.windows_to_host(path));

    let mut background_tab_steps = false;
    let mut out = Map::new();
    out.insert("opened".into(), json!(opened));
    out.insert("existing".into(), json!(existing));
    out.insert("path".into(), path);
    out.insert("contentPath".into(), json!(content_path));
    out.insert(
        "hostPath".into(),
        json!(host_path.as_ref().map(|path| path.display().to_string())),
    );
    if existing {
        // nothing was opened, so nothing about this server's document state changed either
        let active = result.get("active").and_then(Value::as_bool).unwrap_or(false);
        out.insert("active".into(), json!(active));
        out.insert("epoch".into(), result.get("epoch").cloned().unwrap_or(Value::Null));
        // review-5 M1: `reload_map` acts on the ACTIVE document (`require_document` -> `doc.active`),
        // so it may only be suggested when this session IS the active one. For a background session
        // it would re-read a different map and risk discarding that map's unsaved edits
        // (`UnsavedChanges` even suggests force:true).
        if active {
            out.insert(
                "note".into(),
                json!("the file is already open in Hammer and is the active document (the bridge did \
                       not call OpenFile: a second session for the same file would leak the first); \
                       use reload_map to re-read it from disk"),
            );
        } else {
            // live-fix C item D: say plainly that this map is NOT the one the tools act on, and
            // what can be done with the tools that exist (none switches or closes a tab)
            // best effort: a spent budget only drops `activeDocument`, never the `existing:true` result
            let current = match ctx.budget.deadline_ms() {
                Ok(deadline) => crate::tools::live_common::read_active_document(state, Some(deadline))
                    .await
                    .ok()
                    .flatten(),
                Err(_) => None,
            };
            let current_path = current
                .as_ref()
                .and_then(|doc| doc.get("path"))
                .and_then(Value::as_str)
                .map(str::to_string);
            out.insert("activeDocument".into(), json!(current_path));
            out.insert(
                "note".into(),
                json!(format!(
                    "{content_path} is already open in a Hammer background tab, and it is NOT the \
                     active document{}: every live tool (get_entity, create_entities, \
                     save_map, ...) acts on the active document only, and no tool \
                     switches or closes Hammer tabs. open_map did not call OpenFile (a second \
                     session for the same file would leak the first). What can be done now: (1) \
                     read it without activating it: list_entities / find_entities / get_entity / \
                     list_meshes / get_mesh read the SAVED file when you PASS path={content_path:?} \
                     (without path they read the active document); (2) ask the user to switch \
                     to this map's tab (or close the active one) in Hammer, then call \
                     session_status to confirm the active document.",
                    current_path.as_deref().map_or(String::new(), |p| format!(" (the active one is {p})")),
                )),
            );
            background_tab_steps = true;
        }
    } else {
        // a new session: the document identity changed, so the new document's id is what the
        // unsaved-edit counter has to belong to (a fresh document has no MCP edits yet), and the
        // active document is re-read for the reply
        let active =
            crate::tools::live_common::read_active_document(state, Some(ctx.budget.deadline_ms()?))
                .await;
        match active {
            Ok(doc) => {
                let doc_id = doc
                    .as_ref()
                    .and_then(|doc| doc.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                reset_edits(state, doc_id.as_deref());
                out.insert(
                    "document".into(),
                    crate::tools::session::doc_view(state, doc.as_ref()),
                );
            }
            Err(error) => {
                // the open itself succeeded; a failed re-read must not look like a failed open
                out.insert("document".into(), Value::Null);
                out.insert("documentError".into(), error.to_json());
            }
        }
        out.insert("epoch".into(), result.get("epoch").cloned().unwrap_or(Value::Null));
    }
    out.insert("engineAddon".into(), json!(engine_addon));
    out.insert("engineAddonSource".into(), json!(engine_source));
    let selected = state.addon();
    if !selected.eq_ignore_ascii_case(&engine_addon) {
        out.insert(
            "notes".into(),
            json!([format!(
                "the engine's addon ({engine_addon:?}) copy of {content_path} was opened: the \
                 selected addon is {selected:?}, but map.open resolves under the engine's -addon"
            )]),
        );
    }
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    let output = ToolOutput::from(Value::Object(out));
    if background_tab_steps {
        // structured, so the registry checks the tool names
        return Ok(output
            .with_next_args(
                "list_entities",
                "read the background map's entities from the saved file",
                json!({"path": content_path}),
            )
            .with_next("session_status", "confirm which document is active after a tab switch"));
    }
    Ok(output)
}

/// How often the first-map route re-reads the capabilities and the active document.
const FIRST_MAP_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// How long a posted first-map `open_asset` blocks a second one while no document has shown up
/// (Hammer may still be loading, or a modal such as "Error opening file" may be up).
pub const OPEN_PENDING_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// The least budget a first-map open starts with. `open_asset` is posted blind and Hammer takes
/// seconds to load hammer.dll and the map: a post with less left than this would report
/// `OpenUnconfirmed` for an open that then succeeds.
pub const OPEN_MIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Is `doc` (a `doc.active` document) the map `content_path` names? The document path is a
/// Windows path (`z:\\app\\content\\csgo_addons\\<addon>\\maps\\x.vmap`); the content path is
/// addon-relative. Compared case-insensitively on the tail, like Windows does.
pub(crate) fn document_is(doc: &Value, content_path: &str) -> bool {
    let Some(path) = doc.get("path").and_then(Value::as_str) else { return false };
    let path = path.replace('\\', "/").to_ascii_lowercase();
    let want = content_path.to_ascii_lowercase();
    path == want || path.ends_with(&format!("/{want}"))
}

/// `OpenPending`: an earlier first-map `open_asset` was posted inside [`OPEN_PENDING_WINDOW`] and no
/// document has shown up since. `None` when nothing is pending. Nothing is sent by the caller then.
/// `new_map` asks this BEFORE it writes a file.
pub(crate) fn open_pending_refusal(state: &HammerState) -> Option<ToolError> {
    let pending = state
        .pending_vtm_open
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let (path, at) = pending.filter(|(_, at)| at.elapsed() < OPEN_PENDING_WINDOW)?;
    let age = at.elapsed();
    Some(
        ToolError::new(
            "OpenPending",
            "OpenPending",
            format!(
                "open_asset {path} was posted {} s ago and no document has shown up yet; Hammer \
                 may still be loading it or a modal (\"Error opening file\") may be up. A second \
                 post could queue a second open or modal, so nothing was sent. Call session_status \
                 and look at the desktop; retry open_map after {} s if there is still no document",
                age.as_secs(),
                OPEN_PENDING_WINDOW.saturating_sub(age).as_secs() + 1
            ),
        )
        .retry_safe(true)
        .with_data(json!({"pendingPath": path, "pendingAgeMs": age.as_millis() as u64, "retryWith": "session_status"})),
    )
}

/// The bridge says hammer.dll is not loaded (or still being identity-checked):
/// `control.capabilities.modules["hammer.dll"].state`. Anything else (loaded, unknown, no
/// `modules` member) is `false`. A bridge that cannot be reached within `wait` is `false` too.
async fn hammer_module_not_loaded(state: &HammerState, wait: std::time::Duration) -> bool {
    let Ok(bridge) = tokio::time::timeout(wait, state.bridge.lock()).await else {
        return false;
    };
    matches!(
        bridge.module_state("hammer.dll").as_deref(),
        Some("not_loaded" | "verifying")
    )
}

/// One probe of the first-map route, bounded as a whole by `limit_ms` (the connect and the
/// capability refresh included): refresh the capabilities, and once `map.open` and `doc.active`
/// are advertised read the active document. `Some(doc)` only when it is the map.
async fn first_map_probe(state: &HammerState, content_path: &str, limit_ms: u64) -> Option<Value> {
    let limit = std::time::Duration::from_millis(limit_ms.max(1));
    tokio::time::timeout(limit, first_map_probe_inner(state, content_path, limit_ms))
        .await
        .ok()
        .flatten()
}

async fn first_map_probe_inner(state: &HammerState, content_path: &str, limit_ms: u64) -> Option<Value> {
    let mut bridge = tokio::time::timeout(
        std::time::Duration::from_millis(limit_ms.max(1)),
        state.bridge.lock(),
    )
    .await
    .ok()?;
    bridge.ensure_connected().await.ok()?;
    let _ = bridge.refresh_capabilities().await;
    if !(bridge.has("map.open") && bridge.has("doc.active")) {
        return None;
    }
    let doc = crate::tools::live_common::read_active_document_locked(
        state,
        &mut bridge,
        Some(limit_ms.clamp(1, 3_000)),
    )
    .await
    .ok()??;
    document_is(&doc, content_path).then_some(doc)
}

/// The first map of a fresh tools session (live-fix C item A). hammer.dll is not loaded until a
/// document opens, so the bridge does not advertise `map.open`; the editor's own way in is the
/// VToolMessage `open_asset` post (`bridge/runtime.sh open`), which loads the DLL and opens the
/// map. The canonical addon-relative `maps/...vmap` path is the only spelling used: the
/// `csgo_addons/<addon>/...` form raises a blocking "Unrecognized Asset" modal. The call then polls
/// the capabilities and `doc.active` within the budget until the document is this map.
async fn open_first_map(
    state: &HammerState,
    ctx: &CallCtx,
    container: &str,
    content_path: &str,
    engine_addon: &str,
    engine_source: &str,
) -> Result<ToolOutput, ToolError> {
    // the payload lives inside a quoted kv3 string; `map_content_path` (the validator every caller
    // goes through) already refuses `"`, `\`, control characters, `:` and `..`. Escape like
    // `vtm_command_payload` anyway, so the safety does not rest on a validator two hops away.
    debug_assert!(!content_path.contains(['"', '\\', '\n', '\r']), "{content_path:?}");
    let escaped = content_path.replace('\\', "\\\\").replace('"', "\\\"");
    let payload = cs2_mcp_common::runtime::docker::vtm_payload(&format!(
        "\t_message = \"open_asset\"\n\t_data = \"{escaped}\""
    ));
    let too_little = |left: std::time::Duration| {
        ToolError::new(
            "BudgetTooLow",
            "BudgetTooLow",
            format!(
                "only {} ms of the call budget are left, and opening the first map needs at least \
                 {} ms to see the document appear; nothing was sent. Retry open_map",
                left.as_millis(),
                OPEN_MIN_BUDGET.as_millis()
            ),
        )
        .retry_safe(true)
    };
    if ctx.budget.remaining() < OPEN_MIN_BUDGET {
        return Err(too_little(ctx.budget.remaining()));
    }
    let mut notes: Vec<String> = Vec::new();
    let post = {
        let _vtm = match tokio::time::timeout(ctx.budget.remaining(), state.vtm_gate.clone().lock_owned()).await {
            Ok(gate) => gate,
            Err(_) => {
                return Err(ToolError::new(
                    "BridgeBusy",
                    "BridgeBusy",
                    "another call (run_map or console_exec) held the VToolMessage post gate for the \
                     whole call budget; nothing was sent. Retry when it has finished",
                )
                .retry_safe(true))
            }
        };
        if ctx.budget.remaining() < OPEN_MIN_BUDGET {
            return Err(too_little(ctx.budget.remaining()));
        }
        // an earlier first-map post whose document never showed up: Hammer may still be loading
        // or sit behind a modal, and a second open_asset can queue a second open or modal
        if let Some(error) = open_pending_refusal(state) {
            return Err(error);
        }
        // the documented crash trigger is a console post into a loading engine; a `-tools`
        // session's run_map can be loading before any Hammer document exists
        let console_path = super::run_map::console_log_path(state);
        let local = super::run_map::read_game_local(state, ctx.budget.deadline_ms()?.min(2000)).await;
        super::run_map::settle_unresolved_previous(state, &console_path, local.as_ref(), ctx, &mut notes)
            .await?;
        let post = super::run_map::post_payload(state, container, &payload, &format!("open_asset {content_path}"))
            .await?;
        *state
            .pending_vtm_open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((content_path.to_string(), std::time::Instant::now()));
        post
    };
    let give_up = std::time::Instant::now() + ctx.budget.remaining().saturating_sub(std::time::Duration::from_millis(300));
    let doc = loop {
        let left = give_up.saturating_duration_since(std::time::Instant::now());
        let found = first_map_probe(state, content_path, (left.as_millis() as u64).min(3_000)).await;
        if found.is_some() || left.is_zero() {
            break found;
        }
        tokio::time::sleep(FIRST_MAP_POLL.min(left)).await;
    };
    let Some(doc) = doc else {
        let mut error = ToolError::new(
            "OpenUnconfirmed",
            "OpenUnconfirmed",
            format!(
                "open_asset {content_path} was posted over the VToolMessage channel, but within the \
                 call budget Hammer did not show it as the active document (hammer.dll still not \
                 loaded, or a modal such as \"Error opening file\" is up, or the load is slow). The \
                 outcome is unknown. Call session_status and wait until it reports the document; \
                 do not post another open until you know, and check the desktop for a modal"
            ),
        )
        .retry_safe(false)
        .with_data(json!({
            "path": content_path, "route": "vtm", "post": post,
            "retryWith": "session_status",
        }));
        error.outcome_unknown = true;
        return Err(error);
    };
    *state
        .pending_vtm_open
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    let doc_id = doc.get("id").and_then(Value::as_str).map(str::to_string);
    reset_edits(state, doc_id.as_deref());
    let path = doc.get("path").cloned().unwrap_or(Value::Null);
    let host_path = path.as_str().and_then(|path| state.path_map.windows_to_host(path));
    let mut out = Map::new();
    out.insert("opened".into(), json!(true));
    out.insert("existing".into(), json!(false));
    out.insert("openedVia".into(), json!("vtm"));
    out.insert("path".into(), path);
    out.insert("contentPath".into(), json!(content_path));
    out.insert(
        "hostPath".into(),
        json!(host_path.as_ref().map(|path| path.display().to_string())),
    );
    out.insert("epoch".into(), doc.get("epoch").cloned().unwrap_or(Value::Null));
    out.insert("document".into(), crate::tools::session::doc_view(state, Some(&doc)));
    out.insert("post".into(), post);
    out.insert("engineAddon".into(), json!(engine_addon));
    out.insert("engineAddonSource".into(), json!(engine_source));
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    notes.push(
        "the bridge did not advertise map.open yet (hammer.dll loads with the first document), so \
         the map was opened over the VToolMessage open_asset post; later open_map calls use map.open"
            .into(),
    );
    out.insert("notes".into(), json!(notes));
    Ok(Value::Object(out).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cs2_mcp_common::bridge::Kind;

    #[test]
    fn the_document_is_the_map_by_its_addon_relative_tail() {
        let doc = json!({"path": "Z:\\app\\content\\csgo_addons\\mcp_test\\maps\\Sub\\P1.vmap"});
        assert!(document_is(&doc, "maps/sub/p1.vmap"));
        assert!(document_is(&doc, "maps/Sub/P1.vmap"));
        assert!(!document_is(&doc, "maps/p1.vmap"), "a different map with the same stem");
        assert!(!document_is(&json!({"path": null}), "maps/p1.vmap"));
        assert!(!document_is(&json!({}), "maps/p1.vmap"));
    }

    /// REVIEW M1: the local shape gate refuses every non-canonical spelling with the same sentence
    /// the bridge's `map_path.h` uses (`bridge/test_map_path.c` pins the other side), and it never
    /// trims (review L6: `contentPath` is exactly what the caller sent).
    #[test]
    fn map_content_path_reasons_match_the_bridge() {
        for path in [
            "maps/probe.vmap",
            "MAPS/Probe.VMAP",
            "maps/a/probe.vmap",
            "maps/a-b_c.1.vmap",
        ] {
            assert_eq!(map_content_path(&json!({"path": path})).unwrap(), path, "{path}");
        }
        for (path, reason) in [
            (
                "",
                "path: a non-empty path relative to the addon content root (maps/... .vmap) is required",
            ),
            ("probe.vmap", "path: under maps/ (the addon's map directory)"),
            ("maps\\probe.vmap", "path: no '\\' (use '/' separators)"),
            ("maps/probe.vmdl", "path: a .vmap file"),
            ("maps/.vmap", "path: a map file name under maps/"),
            ("maps/../probe.vmap", "path: no '..'"),
            ("maps/a..b.vmap", "path: no '..'"),
            ("maps//probe.vmap", "path: no empty path segment ('//')"),
            ("maps/a//probe.vmap", "path: no empty path segment ('//')"),
            ("maps/./probe.vmap", "path: no '.' or '..' path segment"),
            ("maps/a/./probe.vmap", "path: no '.' or '..' path segment"),
            (
                "maps/a./probe.vmap",
                "path: no path segment ending in '.' or a space",
            ),
            (
                "maps/a /probe.vmap",
                "path: no path segment ending in '.' or a space",
            ),
            (" maps/probe.vmap", "path: under maps/ (the addon's map directory)"),
            ("maps/probe.vmap ", "path: a .vmap file"),
            ("maps/a:b.vmap", "path: no ':'"),
        ] {
            let error = map_content_path(&json!({"path": path})).unwrap_err();
            assert_eq!(error.message, reason, "{path:?}");
        }
    }

    #[test]
    fn wire_unsaved_changes_keeps_the_bridges_own_reason() {
        // L1: the bridge's wire -32016 carries its own reason (its `DocOrDependencyChanged`
        // decided the refusal); the tool must surface it, not overwrite it with the
        // dirty-*unknown* wording (reachable without a race: the dirty pre-check uses
        // `ChangedSinceSave`, a narrower predicate)
        let doc = json!({"id": "d1", "epoch": 3, "path": "z:\\x.vmap", "dirty": false});
        let error = BridgeError::new(Kind::UnsavedChanges, "UnsavedChanges")
            .with_code(-32016)
            .with_data(json!({"reason": "dependency changed; pass force:true to discard"}));
        let tool = wire_unsaved_changes(&doc, 2, &error);
        assert_eq!(tool.name, "UnsavedChanges");
        assert_eq!(tool.type_, "UnsavedChanges");
        assert_eq!(tool.code, Some(-32016));
        assert!(
            tool.message.contains("is dirty") && tool.message.contains("force:true"),
            "the dirty wording, not the dirty-unknown one: {}",
            tool.message
        );
        assert!(
            tool.message.contains("2 unsaved bridge edits"),
            "{}",
            tool.message
        );
        assert_eq!(
            tool.data["reason"],
            "dependency changed; pass force:true to discard",
            "the bridge's own reason wins (insert, not or_insert): {tool:?}"
        );
        assert_eq!(tool.data["unsavedBridgeEdits"], json!(2), "{tool:?}");
    }

    #[test]
    fn untitled_document_gate_and_refusal() {
        for doc in [
            json!({"isNewFile": true, "path": Value::Null}),
            json!({"path": ""}),
            json!({}),
        ] {
            assert!(is_untitled(&doc), "{doc}");
        }
        let titled = json!({"isNewFile": false, "path": "z:\\x.vmap"});
        assert!(!is_untitled(&titled));
        let untitled_with_new = json!({"isNewFile": true, "path": "z:\\x.vmap"});
        assert!(is_untitled(&untitled_with_new), "isNewFile is enough");

        let err = untitled_document(&untitled_with_new, 4);
        assert_eq!(err.name, "UntitledDocument");
        assert_eq!(err.code, Some(-32015));
        assert!(
            err.message.contains("untitled") && err.message.contains("save it first"),
            "{}",
            err.message
        );
        assert_eq!(
            err.data["unsavedBridgeEdits"],
            json!(4),
            "the counter is named but not touched: {err:?}"
        );
    }

    #[test]
    fn node_ids_arg_bounds() {
        assert_eq!(node_ids_arg(&json!({"nodeIDs": [1]})).unwrap(), vec![1]);
        assert_eq!(
            node_ids_arg(&json!({"nodeIDs": [0, 4, 7]})).unwrap(),
            vec![0, 4, 7]
        );
        assert_eq!(
            node_ids_arg(&json!({"nodeIDs": [MAX_NODE_ID]})).unwrap(),
            vec![MAX_NODE_ID]
        );
        let oversized: Vec<i64> = (0..MAX_NODE_IDS as i64 + 1).collect();
        for bad in [
            json!({}),
            json!({"nodeIDs": []}),
            json!({"nodeIDs": "1"}),
            json!({"nodeIDs": [-1]}),
            json!({"nodeIDs": [1.5]}),
            json!({"nodeIDs": [true]}),
            json!({"nodeIDs": [5, 5]}),
            json!({"nodeIDs": oversized}),
            // above the bridge's `int`: it would truncate and delete a different node
            json!({"nodeIDs": [MAX_NODE_ID + 1]}),
            json!({"nodeIDs": [4_294_967_298i64]}),
        ] {
            assert_eq!(node_ids_arg(&bad).unwrap_err().name, "InvalidParams", "{bad}");
        }
    }

    #[test]
    fn expect_doc_carries_the_fingerprint() {
        assert_eq!(
            expect_doc(&json!({"id": "d1", "epoch": 7, "path": "z:\\x.vmap"})),
            json!({"id": "d1", "epoch": 7})
        );
        assert_eq!(
            expect_doc(&json!({})),
            json!({"id": Value::Null, "epoch": Value::Null})
        );
    }
}
