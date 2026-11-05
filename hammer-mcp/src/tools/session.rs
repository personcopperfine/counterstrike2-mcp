//! `session_status` (port of the POC `h_session_status`): never raises.
use crate::state::HammerState;
use cs2_mcp_common::bridge::Kind;
use cs2_mcp_common::tools::{CallCtx, ToolError, ToolOutput};
use serde_json::{json, Map, Value};

pub async fn session_status(
    state: HammerState,
    ctx: CallCtx,
    _args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut out = Map::new();
    out.insert(
        "server".into(),
        json!({"name": crate::NAME, "version": crate::VERSION, "unsavedBridgeEdits": state.unsaved_edits(), "startedAt": state.started_at, "expectedBuild": state.expect_build}),
    );
    let mut bridge = state.bridge.lock().await;
    let section = cs2_mcp_common::status::collect(&mut bridge, &state.launch_command).await;
    out.extend(section.fields);
    out.insert("maps".into(), maps_view(&state));
    if !section.connected {
        out.insert("addon".into(), crate::tools::addon::status_view(&state));
        return Ok(Value::Object(out).into());
    }
    drop(bridge);
    let deadline = ctx.budget.deadline_ms().unwrap_or(5000);
    // the shared read: session_status is a `doc.active` read like any other, so it runs the mesh
    // cache's sticky comparison too (review-4 R4-1) — a client that only ever calls this tool must
    // not keep a stale entry alive.
    match crate::tools::live_common::read_active_document(&state, Some(deadline)).await {
        Ok(doc) => {
            state.note_document(
                doc.as_ref()
                    .and_then(|d| d.get("id"))
                    .and_then(Value::as_str),
            );
            out.insert("document".into(), doc_view(&state, doc.as_ref()));
            if doc.is_none() {
                out.insert("documentReason".into(), json!("no active document"));
            }
        }
        Err(e) if e.kind == Kind::HammerNotLoaded => {
            out.insert("document".into(), Value::Null);
            out.insert("documentReason".into(), json!("hammer not loaded"));
            out.insert("documentError".into(), e.to_json());
        }
        Err(e) => {
            out.insert("document".into(), Value::Null);
            out.insert("documentError".into(), e.to_json());
        }
    }
    out["server"]["unsavedBridgeEdits"] = json!(state.unsaved_edits());
    out.insert("unsavedWarning".into(), json!(state.unsaved_warning()));
    out.insert("addon".into(), crate::tools::addon::status_view(&state));
    out.insert(
        "lockstep".into(),
        json!("one client at a time; this server holds the bridge while it runs"),
    );
    let no_document = out.get("document").is_some_and(Value::is_null)
        && out
            .get("documentError")
            .is_none_or(|e| e.get("type").and_then(Value::as_str) == Some("HammerNotLoaded"));
    let output = ToolOutput::from(Value::Object(out));
    if !no_document {
        return Ok(output);
    }
    // a first-map open still loading: following open_map/new_map now would be refused (OpenPending)
    let pending = state
        .pending_vtm_open
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .filter(|(_, at)| at.elapsed() < crate::tools::lifecycle::OPEN_PENDING_WINDOW);
    if let Some((path, at)) = pending {
        let mut output = output;
        output.json["pendingOpen"] = json!({"path": path, "ageMs": at.elapsed().as_millis() as u64});
        return Ok(output.with_next(
            "session_status",
            "an open of the map is in progress (Hammer is loading it, or a modal is up): wait, then call \
             session_status again; do not open another map yet",
        ));
    }
    // no document: the way in depends on whether the addon has a map to open
    let (names, total) = crate::tools::new_map::addon_maps(&state, 1);
    let (engine_addon, _) = crate::tools::lifecycle::engine_addon(&state).await;
    let selected = state.addon();
    if !selected.eq_ignore_ascii_case(&engine_addon) {
        // open_map/new_map(open:true) resolve under the ENGINE's addon, not the selected one
        let mut output = output;
        output.json["engineAddon"] = json!(engine_addon);
        let why = format!(
            "no document is open, and the selected addon {selected:?} is not the engine's addon \
             {engine_addon:?}: open_map / new_map open under the engine's addon (WrongEngineAddon \
             otherwise); select it, then {}",
            if total > 0 { "open_map" } else { "new_map" }
        );
        return Ok(output.with_next_args("set_addon", &why, json!({"addon": engine_addon})));
    }
    Ok(match names.first() {
        Some(first) if total > 0 => output.with_next_args(
            "open_map",
            "no document is open: open one of the addon's maps (see `maps`); the first map of a \
             fresh session opens over the VToolMessage post",
            json!({"path": first}),
        ),
        _ => output.with_next_args(
            "new_map",
            "no document is open and the addon has no map yet: create one from Valve's template \
             and open it",
            json!({"path": "maps/main.vmap"}),
        ),
    })
}

/// `session_status.maps`: the selected addon's `.vmap` files (names only, capped).
fn maps_view(state: &HammerState) -> Value {
    let (names, total) = crate::tools::new_map::addon_maps(state, crate::tools::new_map::STATUS_MAP_CAP);
    json!({"addon": state.addon(), "names": names, "total": total, "capped": total > names.len()})
}

/// The document as the model sees it: the bridge object plus the mapped host path.
pub fn doc_view(state: &HammerState, doc: Option<&Value>) -> Value {
    let Some(doc) = doc else { return Value::Null };
    let mut view = doc.clone();
    let host = doc
        .get("path")
        .and_then(Value::as_str)
        .and_then(|p| state.path_map.windows_to_host(p));
    view["hostPath"] = json!(host.as_ref().map(|p| p.display().to_string()));
    view["pathMapped"] = json!(host.is_some());
    // The addon-scoped tools follow the document's addon unless set_addon pinned another one;
    // say which, and flag the pinned-elsewhere case.
    view["serverAddon"] = json!(state.addon());
    if let Some(host) = host.as_ref() {
        view["documentAddon"] = json!(state.content().addon_of(host));
        if let Some(hint) = state.content().other_addon_hint(host, state.addon_selection().1) {
            view["addonMismatch"] = json!(format!("the active document: {hint}"));
        }
    }
    if host.is_none() {
        view["pathNote"] = json!("PathNotMapped: extend --path-map / CS2_MCP_PATH_MAP");
    }
    view
}
