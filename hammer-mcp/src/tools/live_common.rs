//! Plumbing the live Hammer mutators share: the active-document read and the `expectDoc`
//! fingerprint it feeds, the `_note_edit`/`_note_undo` counter bookkeeping, the error taxonomy for
//! a write that may have landed, and the two argument shapes every mutator takes.
//!
//! Review L7: `entities_live.rs`, `lifecycle.rs` and `mesh_live.rs` each carried their own copy of
//! these, which made every cross-cutting fix a three-way edit (the retry-safe `-32602` set, opus
//! #9). They live here once now; each module re-exports what it uses (`pub(crate) use`) so its
//! unit tests keep the same names.
use crate::state::{DocState, HammerState};
use cs2_mcp_common::bridge::{BridgeError, Kind};
use cs2_mcp_common::tools::ToolError;
use serde_json::{json, Value};
use std::sync::Arc;

/// The FGD database for `state.fgd_root`, mapped onto the tool taxonomy. The FGD-required
/// tools (`list_props`, `list_triggers`, the entity-class list) share this mapping — it used
/// to be duplicated per module (review C6), differing only in one word of the message.
pub(crate) fn fgd_or_error(state: &HammerState) -> Result<Arc<crate::fgd::Database>, ToolError> {
    use crate::fgd::FgdError;
    crate::fgd::load_for(state).map_err(|error| match error {
        FgdError::Unavailable { path } => ToolError::new(
            "FgdUnavailable",
            "FgdUnavailable",
            format!(
                "the game's FGD tree is not available at {} (csgo/csgo.fgd not found); the \
                 classification is an FGD read",
                path.display()
            ),
        )
        .with_data(json!({ "path": path.display().to_string() }))
        .retry_safe(true),
        FgdError::Io { path, source } => ToolError::new(
            "FgdUnavailable",
            "FgdUnavailable",
            format!(
                "the game's FGD tree is not readable at {}: {source}",
                path.display()
            ),
        )
        .with_data(json!({ "path": path.display().to_string() }))
        .retry_safe(true),
        FgdError::Parse { file, line, reason } => ToolError::new(
            "FgdParseError",
            "FgdParseError",
            format!("{}:{line}: {reason}", file.display()),
        )
        .with_data(json!({ "file": file.display().to_string(), "line": line }))
        .retry_safe(true),
        FgdError::IncludeCycle { chain } => ToolError::new(
            "FgdParseError",
            "FgdParseError",
            format!("@include cycle in the FGD tree: {chain}"),
        )
        .with_data(json!({ "chain": chain }))
        .retry_safe(true),
        other => ToolError::internal(format!("unexpected FGD error: {other}")),
    })
}

/// The bridge parses a node id into an `int`: above this it truncates and edits a *different* node.
pub(crate) const MAX_NODE_ID: i64 = 2_147_483_647;

/// A bad argument: nothing was sent, and the refusal is retry-safe.
pub(crate) fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(message)
}

/// `NoDocument`: the route needs the active document (POC `_current_doc(require=True)`).
pub(crate) fn no_document() -> ToolError {
    ToolError::new(
        "NoDocument",
        "NoDocument",
        "no active Hammer document through the bridge; open a map first",
    )
    .with_code(-32003)
    .retry_safe(true)
}

/// The mutating `expectDoc` fingerprint (`{id, epoch}` off `doc.active`); the bridge answers
/// `-32004 DocumentChanged` when the document moved since this read.
pub(crate) fn expect_doc(doc: &Value) -> Value {
    json!({
        "id": doc.get("id").cloned().unwrap_or(Value::Null),
        "epoch": doc.get("epoch").cloned().unwrap_or(Value::Null),
    })
}

/// The document identity (`doc.active`'s `id`) the unsaved-edit counter is scoped to.
pub(crate) fn doc_id_of(doc: &Value) -> Option<String> {
    doc.get("id").and_then(Value::as_str).map(str::to_string)
}

/// The document's undo state ([`DocState`]: `topSerial` and `undoDepth` of the same reply).
/// `None` when the reply carries no usable pair.
pub(crate) fn doc_state(doc: &Value) -> Option<DocState> {
    DocState::read(doc)
}

/// **The** `doc.active` read of this server: `Ok(None)` = no active document; the error is the
/// bridge's own, which each caller maps as it always did.
pub(crate) async fn read_active_document(
    state: &HammerState,
    deadline: Option<u64>,
) -> Result<Option<Value>, BridgeError> {
    let mut bridge = state.bridge.lock().await;
    read_active_document_locked(state, &mut bridge, deadline).await
}

/// [`read_active_document`] on a bridge the caller already holds (the pre-call addon check waits
/// for the lock itself, with a bounded wait).
pub(crate) async fn read_active_document_locked(
    state: &HammerState,
    bridge: &mut cs2_mcp_common::bridge::client::BridgeClient,
    deadline: Option<u64>,
) -> Result<Option<Value>, BridgeError> {
    let result = bridge.active_document(false, deadline).await;
    if let Ok(doc) = &result {
        if let Some(doc) = doc {
            if let Ok(mut edits) = state.edits.lock() {
                edits.untitled = super::lifecycle::is_untitled(doc);
            }
        }
        // the addon follows the document (WP1.1): every tool that reads `doc.active` refreshes it
        state.note_document_addon(doc.as_ref());
    }
    result
}

/// The document state after a write or failure: a fresh `doc.active`, interpreted by the caller.
/// `None` when the bridge cannot answer, or answers another document.
///
/// m2.17: only reached for a bridge whose write replies carry no `doc` ([`post_write_document`]).
pub(crate) async fn read_document_after_write(
    state: &HammerState,
    deadline: u64,
    doc_id: Option<&str>,
) -> Option<DocState> {
    let mut bridge = state.bridge.lock().await;
    let doc = bridge.active_document(false, Some(deadline)).await.ok()??;
    if doc.get("id").and_then(Value::as_str) != doc_id {
        return None;
    }
    doc_state(&doc)
}

/// `_note_edit`: a document switch resets the counter ([`HammerState::note_document`]), then the
/// edit is counted.
pub(crate) fn note_edit(state: &HammerState, doc_id: Option<&str>) {
    state.note_document(doc_id);
    if let Ok(mut edits) = state.edits.lock() {
        edits.since_save += 1;
    }
}

/// One undone edit: the counter goes back down (never below zero).
pub(crate) fn note_undo(state: &HammerState, doc_id: Option<&str>) {
    state.note_document(doc_id);
    if let Ok(mut edits) = state.edits.lock() {
        edits.since_save = edits.since_save.saturating_sub(1);
    }
}

/// A mutator that failed may still have run: an outcome-unknown class, or a protocol/internal error
/// the bridge can only answer *after* it applied (or cleared) something — `-32603` InternalError,
/// `-32700` parse error, `-32600` invalid request. `-32602` InvalidParams is a pre-dispatch refusal
/// (brief item 8 / opus #9). A call site that knows its own method cannot fail after the write (a
/// `mesh.setMaterial` whose every error exit precedes the undo group) may treat it as not executed
/// instead; see `mesh_live::set_all_faces_material` (review L1).
pub(crate) fn maybe_executed(error: &BridgeError) -> bool {
    if error.outcome_unknown() {
        return true;
    }
    error.kind == Kind::ProtocolError && !matches!(error.code, Some(-32602))
}

/// The mutator failure as the tool reports it: `outcomeUnknown` set and the unsaved counter included
/// when the write may have landed.
pub(crate) fn mutator_failure(
    error: BridgeError,
    state: &HammerState,
    doc_id: Option<&str>,
) -> ToolError {
    let maybe = maybe_executed(&error);
    if maybe {
        note_edit(state, doc_id);
    }
    let mut tool = ToolError::from(error);
    if maybe {
        tool.outcome_unknown = true;
        tool.data
            .insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    }
    tool
}

/// The active document, `NoDocument` when there is none (`_current_doc` also notes the identity,
/// which resets the unsaved counter on a document switch).
pub(crate) async fn require_document(
    state: &HammerState,
    deadline: u64,
) -> Result<Value, ToolError> {
    match read_active_document(state, Some(deadline)).await {
        Ok(Some(doc)) => {
            state.note_document(doc.get("id").and_then(Value::as_str));
            Ok(doc)
        }
        Ok(None) => Err(no_document()),
        Err(error) => Err(ToolError::from(error)),
    }
}

/// `nodeID` (required): an integer in `0..=`[`MAX_NODE_ID`], refused locally when absent/garbage or
/// above the bridge's `int` range (a bigger id truncates and hits a different node).
pub(crate) fn node_id_arg(args: &Value) -> Result<i64, ToolError> {
    match args.get("nodeID") {
        Some(Value::Number(n)) => n
            .as_i64()
            .filter(|id| (0..=MAX_NODE_ID).contains(id))
            .ok_or_else(|| {
                invalid(format!(
                    "nodeID must be an integer in 0..={MAX_NODE_ID}, got {n}"
                ))
            }),
        Some(other) => Err(invalid(format!(
            "nodeID must be an integer in 0..={MAX_NODE_ID}, got {other}"
        ))),
        None => Err(invalid(format!(
            "nodeID is required (an integer in 0..={MAX_NODE_ID})"
        ))),
    }
}

/// Optional boolean argument: `None` when absent or null.
pub(crate) fn bool_arg(args: &Value, name: &str) -> Result<Option<bool>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(other) => Err(invalid(format!("{name} must be a boolean, got {other}"))),
    }
}

/// Refuse an argument the tool's schema does not list (the registry does not enforce
/// `additionalProperties: false`, so a typo like `"scales"` for `scale` used to be ignored and the
/// edit that ran was not the one the caller asked for). The mesh tools carry their own copy
/// (`mesh_live::reject_unknown_keys`); this one is for the m2.18 prop write tools.
pub(crate) fn reject_unknown_keys(args: &Value, allowed: &[&str]) -> Result<(), ToolError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The classification every mutator's error mapping hangs on: a `-32602` refusal happens before
    /// the write, an outcome-unknown class and every other protocol error may have landed, and a
    /// `-32004`/`-32022` decision was made before the write too (opus #9).
    #[test]
    fn maybe_executed_only_for_what_may_have_landed() {
        assert!(maybe_executed(&BridgeError::new(Kind::OutcomeUnknown, "x")));
        assert!(maybe_executed(&BridgeError::new(
            Kind::ConnectionLost,
            "x"
        )));
        for code in [-32603, -32700, -32600] {
            assert!(
                maybe_executed(&BridgeError::new(Kind::ProtocolError, "x").with_code(code)),
                "{code}"
            );
        }
        assert!(!maybe_executed(
            &BridgeError::new(Kind::ProtocolError, "x").with_code(-32602)
        ));
        assert!(!maybe_executed(&BridgeError::new(
            Kind::DocumentChanged,
            "x"
        )));
        assert!(!maybe_executed(&BridgeError::new(Kind::NodeNotFound, "x")));
    }

    /// `bool_arg`: absent/null is `None`, a real boolean passes, anything else is an argument error.
    #[test]
    fn bool_arg_shape() {        assert_eq!(bool_arg(&json!({}), "orientOutward").unwrap(), None);
        assert_eq!(
            bool_arg(&json!({"orientOutward": null}), "orientOutward").unwrap(),
            None
        );
        assert_eq!(
            bool_arg(&json!({"orientOutward": true}), "orientOutward").unwrap(),
            Some(true)
        );
        assert_eq!(
            bool_arg(&json!({"orientOutward": false}), "orientOutward").unwrap(),
            Some(false)
        );
        for bad in [json!({"orientOutward": 1}), json!({"orientOutward": "yes"})] {
            let err = bool_arg(&bad, "orientOutward").unwrap_err();
            assert!(err.message.contains("must be a boolean"), "{err:?}");
        }
    }

    /// `node_id_arg`'s bound is the bridge's own `int` range: a larger id truncates to another node.
    #[test]
    fn node_id_arg_bounds() {
        assert_eq!(node_id_arg(&json!({"nodeID": 7})).unwrap(), 7);
        for bad in [
            json!({}),
            json!({"nodeID": -1}),
            json!({"nodeID": 2_147_483_648i64}),
            json!({"nodeID": "7"}),
            json!({"nodeID": 7.5}),
        ] {
            assert!(node_id_arg(&bad).is_err(), "{bad}");
        }
    }

    /// The parse of a reply's undo fields is shared (`doc_state` -> `DocState::read`); a reply
    /// that cannot be compared (either undo field missing or malformed) reads as no state at all.
    #[test]
    fn doc_state_is_the_shared_parse_of_the_undo_fields() {
        let doc = json!({"id": "d1", "topSerial": 4, "undoDepth": 3});
        assert_eq!(
            doc_state(&doc),
            Some(DocState { doc_id: Some("d1".into()), serial: 4, depth: 3 })
        );
        for bad in [
            json!({"id": "d1", "topSerial": 4}),
            json!({"id": "d1", "undoDepth": 3}),
            json!({"id": "d1", "topSerial": 4, "undoDepth": -1}),
        ] {
            assert_eq!(doc_state(&bad), None, "{bad}");
        }
    }

    /// `reject_unknown_keys`: the schema's `additionalProperties:false` is the client's contract,
    /// the handler is the gate — a typo'd argument is refused before anything is sent.
    #[test]
    fn reject_unknown_keys_names_the_accepted_arguments() {
        let allowed = ["nodeID", "origin"];
        assert!(reject_unknown_keys(&json!({}), &allowed).is_ok());
        assert!(reject_unknown_keys(&json!({"nodeID": 3, "origin": [0, 0, 0]}), &allowed).is_ok());
        for bad in [json!({"teleport": true}), json!({"nodeid": 3})] {
            let err = reject_unknown_keys(&bad, &allowed).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}");
            assert!(err.message.contains("unknown argument"), "{err:?}");
            assert!(err.message.contains("\"nodeID\""), "{err:?}");
        }
        let err = reject_unknown_keys(&json!([1]), &allowed).unwrap_err();
        assert!(err.message.contains("must be an object"), "{err:?}");
    }
}
