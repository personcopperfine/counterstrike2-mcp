//! Addon selection (WP1.1): `set_addon`, the `session_status` view of the selection, and the
//! ModelDoc guard. The selection itself lives on [`HammerState`] ([`crate::state::AddonSel`]):
//! an explicit `set_addon` wins, else the active document's addon, else `--addon`.
use crate::state::{AddonSource, HammerState};
use cs2_mcp_common::tools::{
    nearest_names, CallCtx, Mutability::Mutating, Registry, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Value};

/// Registered last (contract tests pin the order).
pub const ADDON_TOOL_ORDER: [&str; 1] = ["set_addon"];

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "set_addon",
            "Choose the addon the file tools work in (compile_map, run_map, list_models, soundevents, model_*, particle_*, map file reads). Default: the active Hammer document's addon, else --addon. \
             {addon:\"x\"} pins x (must exist under content/csgo_addons); {addon:null} follows the document again; no argument reports the selection and available addons. \
             Server state only. A running ModelDoc stays on its addon: model_open refuses while it differs.",
            Mutating,
            &[],
        )
        .idempotent(true)
        .destructive(false)
        .with_schema(json!({
            "type": "object",
            "properties": {
                "addon": {
                    "type": ["string", "null"],
                    "description": "addon name; null = follow the active document"
                }
            }
        })),
        set_addon,
    );
}

/// The addons that exist: the directory names under `<app>/content/csgo_addons`, sorted.
pub fn available_addons(state: &HammerState) -> Vec<String> {
    let dir = state.app_root().join("content/csgo_addons");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// `session_status.addon`: the selection and what each input says.
pub fn status_view(state: &HammerState) -> Value {
    let sel = state
        .addon_sel
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let (name, source) = sel.current();
    json!({
        "name": name,
        "source": source.as_str(),
        "flag": sel.flag,
        "pinned": sel.explicit,
        "documentAddon": sel.document,
    })
}

pub async fn set_addon(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let available = available_addons(&state);
    match args.get("addon") {
        None => {
            let (name, source) = state.addon_selection();
            Ok(json!({"addon": name, "source": source.as_str(), "available": available}).into())
        }
        Some(Value::Null) => {
            let (previous, prev_source) = state.set_addon(None);
            let (name, source) = state.addon_selection();
            Ok(json!({
                "addon": name, "source": source.as_str(),
                "previous": {"addon": previous, "source": prev_source.as_str()},
                "note": "following the active document again (else --addon)",
            })
            .into())
        }
        Some(Value::String(wanted)) => {
            let wanted = wanted.trim();
            let known = available.iter().find(|a| a.as_str() == wanted);
            if wanted.is_empty() || known.is_none() {
                let suggestions = nearest_names(wanted, available.iter().map(String::as_str), 5);
                // nothing close: list them all, the caller still needs a real name
                let suggestions = if suggestions.is_empty() {
                    available.iter().take(5).cloned().collect()
                } else {
                    suggestions
                };
                return Err(ToolError::new(
                    "AddonNotFound",
                    "AddonNotFound",
                    format!(
                        "no addon {wanted:?} under content/csgo_addons (available: {}); the selection is unchanged",
                        if available.is_empty() { "none".to_string() } else { available.join(", ") }
                    ),
                )
                .with_code(-32602)
                .did_you_mean(suggestions)
                .with_data(json!({"available": available}))
                .retry_safe(true));
            }
            let (previous, prev_source) = state.set_addon(Some(wanted.to_string()));
            let (name, source) = state.addon_selection();
            Ok(json!({
                "addon": name, "source": source.as_str(),
                "previous": {"addon": previous, "source": prev_source.as_str()},
            })
            .into())
        }
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "addon must be a string or null, got {other}"
        ))),
    }
}

/// The ModelDoc container's addon when it differs from the selected one: `(container, running,
/// selected)`. `None` when no ModelDoc container is found, its addon cannot be read (no podman,
/// no label), or it matches: there is nothing to contradict then.
async fn modeldoc_addon_mismatch(state: &HammerState) -> Option<(String, String, String)> {
    let container = match &state.tools_container {
        Some(name) => name.clone(),
        None => match state
            .runtime
            .container_by_label(super::compile::CompileTarget::MODELDOC.label)
            .await
        {
            Ok(Some(name)) => name,
            _ => return None,
        },
    };
    let Ok(Some(running)) = state.runtime.container_addon(&container).await else {
        return None;
    };
    let selected = state.addon();
    (!running.eq_ignore_ascii_case(&selected)).then_some((container, running, selected))
}

/// How the addon was selected, for messages: "the active document", "set_addon" or "--addon".
fn source_phrase(state: &HammerState) -> &'static str {
    match state.addon_selection().1 {
        AddonSource::Document => "the active Hammer document",
        AddonSource::SetAddon => "an explicit set_addon",
        AddonSource::Flag => "the --addon flag",
    }
}

fn restart_hint(container: &str, selected: &str) -> String {
    format!(
        "restart ModelDoc on {selected:?}: CONTAINER={container} bridge/runtime.sh down, then \
         TOOL=modeldoc ADDON={selected} bridge/runtime.sh up"
    )
}

/// `model_open` only: refuse while the running ModelDoc was started with another addon than the
/// selected one (`bridge/runtime.sh` starts it with `-addon <addon>`; the server cannot switch
/// it, and an open now would load the wrong addon's file or fail). File writes are NOT refused
/// (they are correct file writes whatever ModelDoc runs): see [`modeldoc_addon_warning`].
pub async fn modeldoc_addon_guard(state: &HammerState) -> Result<(), ToolError> {
    let Some((container, running, selected)) = modeldoc_addon_mismatch(state).await else {
        return Ok(());
    };
    Err(ToolError::new(
        "AddonMismatch",
        "AddonMismatch",
        format!(
            "the running ModelDoc ({container}) was started with -addon {running:?} but the selected \
             addon is {selected:?} (chosen by {}): opening a model now would load the wrong \
             addon's file. Nothing was opened.",
            source_phrase(state)
        ),
    )
    .with_code(-32602)
    .with_data(json!({
        "selectedAddon": selected,
        "modeldocAddon": running,
        "container": container,
        "hint": format!(
            "{}; or, to inspect {running:?}'s models now, set_addon {{addon:\"{running}\"}}",
            restart_hint(&container, &selected)
        ),
    }))
    .use_instead(
        "set_addon",
        &format!("only to inspect {running:?}'s models while ModelDoc stays on it"),
        Some(json!({"addon": running})),
    )
    .retry_safe(true))
}

/// `model_edit` / `model_create`: the file write is correct for the selected addon, but the
/// running ModelDoc is on another one, so `model_reload`/`model_open` will not show it. A warning
/// for the result, never a refusal.
pub async fn modeldoc_addon_warning(state: &HammerState) -> Option<String> {
    let (container, running, selected) = modeldoc_addon_mismatch(state).await?;
    Some(format!(
        "the file was written in addon {selected:?}, but the running ModelDoc ({container}) is on \
         addon {running:?}, so it will not show this change; {}",
        restart_hint(&container, &selected)
    ))
}
