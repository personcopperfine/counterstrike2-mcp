//! Live ModelDoc tools: the running editor through the bridge (`modeldoc.*`, tools main thread).
//!
//! Policies (modeldoc-plan §1 "Policies" as amended by `facts-round2.md`; RE:
//! `hammer-mcp/research/modeldoc-re.md`):
//!
//! * `model_status` never raises (the [`crate::tools::session::session_status`] pattern): the
//!   bridge status section, `modeldocLoaded` = the bridge advertises `modeldoc.active` after one
//!   capabilities refresh, `capabilities.missing` = the `modeldoc.*` methods absent, the active
//!   document (+ mapped host path, `inAddon`), a reason when there is none, and a launch/open
//!   hint when the editor is not running.
//! * `model_open` validates on the host first (exists, parses, classes vs the node-type snapshot
//!   when present) — **nothing is sent on any failure**: a broken file pops a modal in the
//!   editor's `OpenFile` that blocks its main thread. It then refuses while the live document is
//!   `dirty` *or dirty-unknown* unless `force`, and hands the bridge the addon-relative path the
//!   bridge's `OpenFile` search paths are rooted at (`models/x.vmdl`, facts-round2 §3).
//! * `model_reload` refuses dirty/unknown unless `force`; `model_save` refuses untitled documents
//!   (FileSave would open the modal Save As) and paths outside the addon (opened `[ READ ONLY ]`).
//! * `model_node_types` lists the editor's `_class` names sorted + deduped; `writeSnapshot`
//!   stores them in `<repo>/hammer-mcp/research/modeldoc-nodetypes.json`
//!   (read back by `model_edit`'s `classCheck:"snapshot"`).
//!
//! Every bridge request carries `RequestOpts::deadline(ctx.budget.deadline_ms()?)`; `-32003`,
//! `-32015`, `-32016` and `-32017` surface with their wire names through `ToolError::from`.
use crate::state::HammerState;
use crate::vmdl::{Model, VmdlError};
use cs2_mcp_common::bridge::{BridgeClient, BridgeError, Kind, RequestOpts};
use cs2_mcp_common::tools::{CallCtx, ToolError, ToolOutput};
use serde_json::{json, Map, Value};
use std::path::Path;

/// The six `modeldoc.*` bridge methods (m2.11). `model_status` reports which of these the bridge
/// does not advertise; the tools below declare their own subset in `modeldoc.rs` and are gated on
/// it by the registry before the handler runs (nothing is sent when one is missing).
const MODELDOC_METHODS: [&str; 7] = [
    "modeldoc.active",
    "modeldoc.open",
    "modeldoc.reload",
    "modeldoc.save",
    "modeldoc.compile",
    "modeldoc.nodeTypes",
    "modeldoc.render",
];

/// The `_class` snapshot `model_node_types` writes and `model_edit` reads: the repo's checkout of
/// `hammer-mcp/research/modeldoc-nodetypes.json` under `--project-root` (an installed binary has
/// moved away from `CARGO_MANIFEST_DIR`, so the path comes from state at runtime).
fn snapshot_path(state: &HammerState) -> std::path::PathBuf {
    cs2_mcp_common::mcp_repo_dir(&state.project_root)
        .join("hammer-mcp/research/modeldoc-nodetypes.json")
}

// ---------------------------------------------------------------- small helpers (own file)

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ToolError::invalid_arguments(format!("{key}: a non-empty string is required"))
        })
}

fn bool_arg(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// `Some(true|false)` when the document reports it, `None` when it does not say (a missing or
/// non-boolean `dirty` counts as unknown: refuse rather than risk discarding edits).
fn dirty_state(doc: &Value) -> Option<bool> {
    doc.get("dirty").and_then(Value::as_bool)
}

/// The document as the client sees it: the `modeldoc.active` object plus the mapped host path
/// (`session::doc_view`) and the `inAddon` check (outside the addon the editor shows the model
/// as `[ READ ONLY ]`).
fn document_view(state: &HammerState, doc: &Value) -> Value {
    let mut view = crate::tools::session::doc_view(state, Some(doc));
    if view.is_object() {
        let host = doc
            .get("path")
            .and_then(Value::as_str)
            .and_then(|p| state.path_map.windows_to_host(p));
        view["inAddon"] = json!(host.is_some_and(|h| state.content().in_addon(&h)));
    }
    view
}

/// `-32016 UnsavedChanges` before open/reload (for `reload` the bridge answers the same code when
/// `dirty && !force`; the tool pre-checks so dirty-*unknown* is refused as well).
fn unsaved_changes(doc: &Value) -> ToolError {
    let (message, reason) = match dirty_state(doc) {
        Some(true) => (
            "the active ModelDoc document is dirty (unsaved in-editor edits); pass force:true to discard them",
            "document is dirty; pass force:true to discard",
        ),
        _ => (
            "the active document's dirty state is not reported; refusing to risk unsaved edits, pass force:true",
            "dirty state unreported; pass force:true to discard",
        ),
    };
    ToolError::new("UnsavedChanges", "UnsavedChanges", message)
        .with_code(-32016)
        .retry_safe(true)
        .with_data(json!({"reason": reason, "path": doc.get("path").unwrap_or(&Value::Null)}))
}

/// `modeldoc.active` resolved to a document, or a no-document error shaped exactly like the wire's
/// `-32003 NoActiveDocument`: built through `BridgeError -> ToolError` (the existing mapping), so
/// `type_`/`name`/`wireName`/`code` are indistinguishable from a bridge-reported one.
async fn require_document(bridge: &mut BridgeClient, deadline: u64) -> Result<Value, ToolError> {
    let resp = bridge
        .request("modeldoc.active", json!({}), RequestOpts::deadline(deadline))
        .await;
    let result = match resp {
        Ok(resp) => resp.result,
        Err(e) => return Err(ToolError::from(e)),
    };
    match result.get("doc") {
        Some(doc) if doc.is_object() => Ok(doc.clone()),
        _ => {
            let reason = result
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("no_document");
            Err(ToolError::from(
                BridgeError::new(
                    Kind::NoDocument,
                    format!(
                        "no active ModelDoc document ({reason}): open one with model_open first"
                    ),
                )
                .with_code(-32003)
                .with_wire_name("NoActiveDocument")
                .with_data(json!({"reason": reason})),
            ))
        }
    }
}

/// A host-side validation failure (`VmdlError`) as a `ToolError`. Nothing has been dispatched at
/// this point, so every arm is retry-safe (retrying cannot corrupt anything).
fn validation_error(error: VmdlError, snapshot: &Path) -> ToolError {
    match error {
        VmdlError::Parse(e) => ToolError::new("InvalidModel", "ParseError", e.to_string())
            .retry_safe(true)
            .with_data(json!({"line": e.line, "col": e.col})),
        VmdlError::Invalid(ref issues) => {
            let issues: Vec<Value> = issues
                .iter()
                .map(|issue| json!({"path": issue.path, "msg": issue.msg}))
                .collect();
            ToolError::new("InvalidModel", "InvalidModel", format!("{error}"))
                .retry_safe(true)
                .with_data(json!({"issues": issues}))
        }
        VmdlError::UnknownClass { path, class } => ToolError::new(
            "InvalidModel",
            "UnknownClass",
            format!("unknown _class {class:?} at {path} (not in the node-type snapshot)"),
        )
        .retry_safe(true)
        .with_data(json!({
            "class": class, "nodePath": path, "snapshot": snapshot.display().to_string(),
        })),
        VmdlError::Path(msg) => {
            if msg.contains("only models of the active addon") {
                ToolError::new("InvalidParams", "WriteOutsideAddon", msg)
                    .with_code(-32602)
                    .retry_safe(true)
            } else if msg.contains("has no host mapping") {
                ToolError::new("PathMap", "PathNotMapped", msg)
                    .with_code(-32602)
                    .retry_safe(true)
                    .with_data(json!({"note": "extend --path-map / CS2_MCP_PATH_MAP"}))
            } else {
                ToolError::invalid_arguments(msg)
            }
        }
        // model_open never applies ops and never addresses nodes: not reachable, be honest if so
        VmdlError::BadOp { .. } | VmdlError::NodeNotFound(_) => {
            ToolError::internal(format!("unexpected model-layer error: {error}"))
        }
        VmdlError::Io(e) => {
            ToolError::new("InvalidModel", "ParseError", format!("cannot read the file: {e}"))
                .retry_safe(true)
        }
    }
}

/// `value` as pretty JSON with a trailing newline, written `path.tmp-<pid>` then renamed
/// (atomic; the parent directory is created).
fn write_json_atomic(path: &Path, value: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into());
    text.push('\n');
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
}

/// Host-side pre-validation for `model_open`: resolve in the addon, require existence, parse and
/// structurally validate, check `_class` names against the node-type snapshot when one exists.
/// Returns the host path. A failing file must produce an error **before anything is sent**: a
/// broken file pops a modal in the editor's `OpenFile` that blocks the tools main thread.
fn validate_file(state: &HammerState, path: &str) -> Result<std::path::PathBuf, ToolError> {
    let snapshot = snapshot_path(state);
    let invalid = |error: VmdlError| validation_error(error, &snapshot);

    let host = state
        .content()
        .resolve_write(path, &state.path_map)
        .map_err(invalid)?;
    if !host.is_file() {
        return Err(ToolError::invalid_arguments(format!(
            "{path:?} does not exist on the host ({})",
            host.display()
        )));
    }
    let text = std::fs::read_to_string(&host).map_err(|e| {
        ToolError::new("InvalidModel", "ParseError", format!("cannot read {}: {e}", host.display()))
            .retry_safe(true)
    })?;
    let model = Model::parse(&text).map_err(invalid)?;
    // classes vs the committed snapshot (facts §8): a missing snapshot is a warning-shaped
    // omission, not a failure — class names are simply not checked.
    let (classes, _source) = crate::vmdl::known_classes(&snapshot);
    if let Err(e) = model.validate_classes(&classes) {
        return Err(invalid(e));
    }
    Ok(host)
}

// ---------------------------------------------------------------- model_status

/// `model_status`: the ModelDoc half of `session_status`. Never raises.
pub async fn model_status(
    state: HammerState,
    ctx: CallCtx,
    _args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut out = Map::new();
    let mut bridge = state.bridge.lock().await;
    let section = cs2_mcp_common::status::collect(&mut bridge, &state.launch_command).await;
    out.extend(section.fields);
    let connected = section.connected;

    // `modeldocLoaded` is the bridge advertising `modeldoc.active` after one capabilities refresh
    // (the late-loaded modeldoc_editor.dll appears in capabilities once its identity check passes).
    let missing = if connected {
        match bridge.refresh_capabilities().await {
            Ok(_) => bridge.missing(&MODELDOC_METHODS),
            Err(_) => MODELDOC_METHODS.iter().map(|m| m.to_string()).collect(),
        }
    } else {
        MODELDOC_METHODS.iter().map(|m| m.to_string()).collect()
    };
    match out.get_mut("capabilities").and_then(Value::as_object_mut) {
        Some(caps) => {
            caps.insert("missing".into(), json!(missing));
        }
        None => {
            out.insert("capabilities".into(), json!({"missing": missing}));
        }
    }
    // When not loaded, the document probe must not send an unadvertised method: the bridge would
    // answer MethodNotFound (or a NEED_MODELDOC method the module never loaded).
    let loaded = connected && bridge.has("modeldoc.active");
    out.insert("modeldocLoaded".into(), json!(loaded));

    // hint: not connected -> the launch command; connected but not loaded -> up then open
    // (addon-relative form: `vtmsend open_asset csgo_addons/<addon>/models/...` raises the modal
    // "Unrecognized Asset" and blocks the tools main thread, seen live 2026-10-13).
    let open_base = state.launch_command.trim_end_matches(" up").to_string();
    let hint = if !connected {
        json!(state.launch_command)
    } else if !loaded && cfg!(windows) {
        // native: no ModelDoc container; an open_asset post loads ModelDoc into the running
        // -tools session (runtime.sh `open`, the same post)
        json!(format!(
            "{}; then cs2mcp-native open models/<file>.vmdl (relative to the addon {}; run it on the \
             game's desktop, the VToolMessage channel is per logon session)",
            state.launch_command, state.addon()
        ))
    } else if !loaded {
        json!(format!(
            "TOOL=modeldoc {}; then {} open models/<file>.vmdl (relative to the addon {})",
            state.launch_command, open_base, state.addon()
        ))
    } else {
        Value::Null
    };
    out.insert("hint".into(), hint);
    out.insert("fileNewerThanEditor".into(), Value::Null);

    if loaded {
        let deadline = ctx.budget.deadline_ms().unwrap_or(5_000);
        match bridge
            .request("modeldoc.active", json!({}), RequestOpts::deadline(deadline))
            .await
        {
            Ok(resp) => match resp.result.get("doc").filter(|d| d.is_object()) {
                Some(doc) => out.insert("document".into(), document_view(&state, doc)),
                None => {
                    out.insert("document".into(), Value::Null);
                    out.insert(
                        "documentReason".into(),
                        resp.result
                            .get("reason")
                            .cloned()
                            .unwrap_or_else(|| json!("no_document")),
                    );
                    None
                }
            },
            // a blocked gate means a modal dialog owns the main thread: report, never raise
            Err(e) => {
                out.insert("document".into(), Value::Null);
                out.insert("documentError".into(), e.to_json());
                None
            }
        };
    }
    drop(bridge);
    Ok(Value::Object(out).into())
}

// ---------------------------------------------------------------- model_open

/// `model_open {path, force?}`: pre-validate the file on the host, refuse a dirty/unknown live
/// document unless `force`, then send `modeldoc.open` with the addon-relative path.
pub async fn model_open(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    crate::tools::addon::modeldoc_addon_guard(&state).await?;
    let path = str_arg(&args, "path")?;
    let force = bool_arg(&args, "force");
    let host = validate_file(&state, path)?;

    let mut bridge = state.bridge.lock().await;
    let active = bridge
        .request(
            "modeldoc.active",
            json!({}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let active = match active {
        Ok(resp) => resp.result,
        Err(e) => return Err(ToolError::from(e)),
    };
    if let Some(doc) = active.get("doc").filter(|d| d.is_object()) {
        if dirty_state(doc).unwrap_or(true) && !force {
            return Err(unsaved_changes(doc));
        }
    }
    // resolve_write already pinned `host` under `content/csgo_addons/<addon>/`, so this cannot fail
    let Some(addon_path) = state.content().addon_rel(&host) else {
        return Err(ToolError::internal(format!(
            "{} resolved in the addon but has no addon-relative form",
            host.display()
        )));
    };
    let reply = bridge
        .request(
            "modeldoc.open",
            json!({"path": addon_path.clone()}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => return Err(ToolError::from(e)),
    };
    if reply.result.get("opened").and_then(Value::as_bool) != Some(true) {
        return Err(ToolError::new(
            "OpenFailed",
            "OpenFailed",
            format!(
                "the editor reported opened:false for {addon_path:?} (OpenFile returned 0: re-entrancy guard or load failure)"
            ),
        )
        .retry_safe(true)
        .with_data(reply.result.clone()));
    }
    // the editor must have opened THE file asked for: a ModelDoc on another addon resolves the
    // addon-relative path in its own addon and can open a different file (or none)
    let opened_host = reply
        .result
        .get("active")
        .and_then(|d| d.get("path"))
        .and_then(Value::as_str)
        .and_then(|p| state.path_map.windows_to_host(p));
    let mut warnings: Vec<String> = Vec::new();
    match &opened_host {
        Some(opened) if !super::entities_file::same_file(opened, &host) => {
            return Err(ToolError::new(
                "WrongDocumentOpened",
                "WrongDocumentOpened",
                format!(
                    "asked ModelDoc to open {} but the editor now holds {}: it is probably running \
                     on another addon than {:?}",
                    host.display(),
                    opened.display(),
                    state.addon()
                ),
            )
            // the editor already switched documents (with force it discarded the dirty one)
            .retry_safe(false)
            .with_data(json!({
                "requested": host.display().to_string(),
                "opened": opened.display().to_string(),
                "hint": "model_status shows the open document; restart ModelDoc on the selected \
                         addon (TOOL=modeldoc ADDON=<addon> bridge/runtime.sh up)",
            })));
        }
        Some(_) => {}
        None => warnings.push(
            "the editor's reply carried no mappable document path, so the opened file could not be verified"
                .to_string(),
        ),
    }
    let document = match reply.result.get("active").filter(|d| d.is_object()) {
        Some(active) => document_view(&state, active),
        None => Value::Null,
    };
    Ok(json!({
        "opened": true,
        "path": path,
        "contentPath": addon_path,
        "document": document,
        "warnings": warnings,
    })
    .into())
}

// ---------------------------------------------------------------- model_reload

/// `model_reload {force?}`: refuse a dirty/unknown document unless `force`, then reload from
/// disk (the editor drops its undo history and replaces the `CModelDoc`).
pub async fn model_reload(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let force = bool_arg(&args, "force");
    let mut bridge = state.bridge.lock().await;
    let doc = require_document(&mut bridge, ctx.budget.deadline_ms()?).await?;
    if dirty_state(&doc).unwrap_or(true) && !force {
        return Err(unsaved_changes(&doc));
    }
    let reply = bridge
        .request(
            "modeldoc.reload",
            json!({"force": force}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => return Err(ToolError::from(e)),
    };
    let path = reply
        .result
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let host = if path.is_empty() {
        doc.get("path")
            .and_then(Value::as_str)
            .and_then(|p| state.path_map.windows_to_host(p))
    } else {
        state.path_map.windows_to_host(path)
    };
    Ok(json!({
        "reloaded": reply.result.get("reloaded"),
        "path": reply.result.get("path"),
        "hostPath": host.as_ref().map(|p| p.display().to_string()),
        "discardedChanges": reply.result.get("discardedChanges"),
    })
    .into())
}

// ---------------------------------------------------------------- model_save

/// `model_save {}`: refuse untitled documents (modal Save As) and read-only (outside-addon)
/// paths, then `modeldoc.save` on the document's own path.
pub async fn model_save(
    state: HammerState,
    ctx: CallCtx,
    _args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut bridge = state.bridge.lock().await;
    let doc = require_document(&mut bridge, ctx.budget.deadline_ms()?).await?;
    if doc.get("isNewFile").and_then(Value::as_bool) == Some(true) {
        let path = doc.get("path").cloned().unwrap_or(Value::Null);
        return Err(ToolError::new(
            "UntitledDocument",
            "UntitledDocument",
            "the active document is untitled: FileSave would open the modal Save As; open the file with model_open first",
        )
        .with_code(-32015)
        .retry_safe(true)
        .with_data(json!({"path": path})));
    }
    let win_path = doc.get("path").and_then(Value::as_str).unwrap_or("");
    let host = state.path_map.windows_to_host(win_path);
    if !host.as_ref().is_some_and(|h| state.content().in_addon(h)) {
        let note = if host.is_none() {
            "the document's path has no --path-map mapping, so it cannot be shown editable"
        } else {
            "a model outside content/csgo_addons/<addon> is [ READ ONLY ] in the editor"
        };
        return Err(ToolError::new(
            "ReadOnlyModel",
            "ReadOnlyModel",
            format!("refusing to save {win_path:?}: {note}; nothing was sent"),
        )
        .retry_safe(true)
        .with_data(json!({"path": win_path, "note": note})));
    }
    let reply = bridge
        .request(
            "modeldoc.save",
            json!({}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => return Err(ToolError::from(e)),
    };
    Ok(json!({
        "saved": reply.result.get("saved"),
        "skippedClean": reply.result.get("skippedClean"),
        "path": reply
            .result
            .get("path")
            .filter(|p| !p.as_str().unwrap_or_default().is_empty())
            .or(doc.get("path")),
        "hostPath": host.as_ref().map(|p| p.display().to_string()),
    })
    .into())
}

// ---------------------------------------------------------------- model_node_types

/// `model_node_types {writeSnapshot?}`: every `_class` the running editor knows (sorted,
/// deduped); `writeSnapshot` stores them (with the game build) in the research snapshot.
pub async fn model_node_types(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let write_snapshot = bool_arg(&args, "writeSnapshot");
    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(
            "modeldoc.nodeTypes",
            json!({}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => return Err(ToolError::from(e)),
    };
    let types = reply
        .result
        .get("types")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut classes: Vec<String> = types
        .iter()
        .filter_map(|t| t.get("className").and_then(Value::as_str).map(str::to_string))
        .collect();
    let count = reply
        .result
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(types.len() as u64);
    classes.sort();
    classes.dedup();
    let mut out = json!({
        "count": count,
        "classes": classes,
        "note": "classes sorted + deduped; `count` is the editor's descriptor count",
    });
    if write_snapshot {
        let build = bridge
            .version()
            .await
            .ok()
            .and_then(|v| v.get("gameBuild").cloned())
            .filter(|b| b.is_string())
            .unwrap_or(Value::Null);
        let path = snapshot_path(&state);
        write_json_atomic(&path, &json!({"build": build, "classes": classes})).map_err(
            |error| {
                ToolError::internal(format!(
                    "cannot write {}: {error}",
                    path.display()
                ))
            },
        )?;
        out["snapshotPath"] = json!(path.display().to_string());
    }
    Ok(out.into())
}
