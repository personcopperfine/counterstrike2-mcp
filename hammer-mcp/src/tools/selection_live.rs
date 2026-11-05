//! `get_selection` / `set_selection`: the LIVE document's selection through the bridge's
//! `selection.get` / `selection.set` (bridge m3.12).
//!
//! The selection is what the user sees highlighted in Hammer: nodes in the groups / objects /
//! meshes modes, faces in the faces mode. Reading it lets a model act on "the thing I selected"
//! (`get_mesh`, `set_mesh_material {faces}`); writing it shows the user what a tool is about to
//! work on, or what it just made.
//!
//! * **Face indices** are the indices `get_mesh` reports: the bridge numbers a mesh's faces in
//!   Hammer's own face order (`CollectHandles`), which is also the order the saved `.vmap` stores.
//! * **Not an edit.** Selection opens no undo group and does not mark the document dirty, so
//!   nothing here touches the unsaved-edit counter or `undo_last`. `set_selection` still takes the
//!   server-wide write gate (it must not interleave with a mutator that reads the document first)
//!   and sends `expectDoc`, so it refuses when the document moved since it was read.
//! * Hammer may filter a selection (locked or hidden nodes, meshes that ignore face selection):
//!   the answer's `applied` is then smaller than `requested`, which is not an error; `notApplied`
//!   names what was left out.
//! * `set_selection` switches Hammer's selection mode when it has to (`modeChanged`), and after a
//!   switch `add`/`remove` are relative to an empty selection.
use super::live_common::{
    expect_doc, invalid, maybe_executed, reject_unknown_keys, require_document, MAX_NODE_ID,
};
use crate::state::HammerState;
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};

/// The selection tools' order, after [`super::sounds::SOUND_TOOL_ORDER`] (contract tests pin it).
pub const SELECTION_TOOL_ORDER: [&str; 2] = ["get_selection", "set_selection"];

/// The bridge's per-call bounds (`cs2-bridge` `ops::selection`): refused locally first.
const MAX_NODE_IDS: usize = 4096;
const MAX_FACE_INDICES: usize = 65_536;

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "get_selection",
            "What is selected in the live Hammer document: `mode`, `nodeIDs`, `count`, and in faces mode `faces: [{nodeID, faces:[i]}]` per mesh (indices as get_mesh reports). Other modes report only the count. `truncated:true` = lists cut at 16384 (count is still true). Read-only.",
            ReadOnly,
            &["selection.get"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })),
        get_selection,
    );
    r.register(
        ToolSpec::new(
            "set_selection",
            "Change the selection in the live Hammer document so the user sees what you work on. `op` replace|add|remove|clear; give exactly one of `nodeIDs` or `faces` ([{nodeID, faces:[i]}], switches to faces mode); clear takes neither. After a mode switch add/remove start empty (`modeChanged`). Not an edit or undo step. `applied` can be below `requested` (locked/hidden nodes; see `notApplied`); unknown nodes or bad face indices refuse with nothing changed. Max 4096 nodeIDs, 65536 face indices.",
            Mutating,
            &["selection.set"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["replace", "add", "remove", "clear"],
                    "description": "clear takes no nodeIDs/faces"
                },
                "nodeIDs": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_NODE_IDS,
                    "items": {"type": "integer", "minimum": 0, "maximum": MAX_NODE_ID},
                    "description": "not with `faces` or op clear"
                },
                "faces": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_NODE_IDS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "nodeID": {"type": "integer", "minimum": 0, "maximum": MAX_NODE_ID},
                            "faces": {
                                "type": "array",
                                "minItems": 1,
                                "items": {"type": "integer", "minimum": 0},
                                "description": "as get_mesh reports"
                            }
                        },
                        "required": ["nodeID", "faces"],
                        "additionalProperties": false
                    },
                    "description": "per mesh; not with `nodeIDs` or op clear"
                }
            },
            "required": ["op"],
            "additionalProperties": false
        })),
        set_selection,
    );
}

/// `get_selection {}`: `selection.get` passthrough, tagged `source:"live"`.
pub async fn get_selection(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    reject_unknown_keys(&args, &[])?;
    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "selection.get",
                json!({}),
                RequestOpts::deadline(ctx.budget.deadline_ms()?),
            )
            .await
    };
    let mut out = match reply {
        Ok(reply) => reply.result,
        Err(error) => return Err(ToolError::from(error)),
    };
    if !out.is_object() {
        return Err(ToolError::internal(format!(
            "selection.get answered a non-object result: {out}"
        )));
    }
    out["source"] = json!("live");
    Ok(out.into())
}

/// What a `set_selection` asks for, as the bridge's params carry it.
struct SetRequest {
    op: String,
    nodes: Option<Vec<i64>>,
    faces: Option<Vec<(i64, Vec<i64>)>>,
}

fn node_id(value: &Value, what: &str) -> Result<i64, ToolError> {
    match value.as_i64() {
        Some(id) if (0..=MAX_NODE_ID).contains(&id) => Ok(id),
        _ => Err(invalid(format!(
            "{what} must be an integer in 0..={MAX_NODE_ID}, got {value}"
        ))),
    }
}

/// Validate the arguments with the bridge's own bounds (nothing is sent on a refusal).
fn parse_request(args: &Value) -> Result<SetRequest, ToolError> {
    reject_unknown_keys(args, &["op", "nodeIDs", "faces"])?;
    let op = match args.get("op").and_then(Value::as_str) {
        Some(op @ ("replace" | "add" | "remove" | "clear")) => op.to_string(),
        _ => {
            return Err(invalid(
                "op is required: one of replace, add, remove, clear".to_string(),
            ))
        }
    };
    let present = |key: &str| args.get(key).is_some_and(|value| !value.is_null());
    let (has_nodes, has_faces) = (present("nodeIDs"), present("faces"));
    if op == "clear" {
        if has_nodes || has_faces {
            return Err(invalid("op clear takes neither nodeIDs nor faces"));
        }
        return Ok(SetRequest { op, nodes: None, faces: None });
    }
    if has_nodes == has_faces {
        return Err(invalid(
            "give exactly one of nodeIDs or faces (op clear takes neither)",
        ));
    }
    if has_nodes {
        let list = args["nodeIDs"]
            .as_array()
            .filter(|list| !list.is_empty())
            .ok_or_else(|| invalid("nodeIDs must be a non-empty array of node ids"))?;
        if list.len() > MAX_NODE_IDS {
            return Err(invalid(format!(
                "nodeIDs: at most {MAX_NODE_IDS} ids per call, got {}",
                list.len()
            )));
        }
        let ids = list
            .iter()
            .map(|value| node_id(value, "nodeIDs[]"))
            .collect::<Result<Vec<i64>, ToolError>>()?;
        return Ok(SetRequest { op, nodes: Some(ids), faces: None });
    }
    let list = args["faces"]
        .as_array()
        .filter(|list| !list.is_empty())
        .ok_or_else(|| invalid("faces must be a non-empty array of {nodeID, faces:[i, ...]}"))?;
    if list.len() > MAX_NODE_IDS {
        return Err(invalid(format!(
            "faces: at most {MAX_NODE_IDS} entries per call, got {}",
            list.len()
        )));
    }
    let mut total = 0usize;
    let mut entries = Vec::with_capacity(list.len());
    for entry in list {
        let object = entry
            .as_object()
            .ok_or_else(|| invalid(format!("faces[] must be {{nodeID, faces:[i, ...]}}, got {entry}")))?;
        if let Some(extra) = object.keys().find(|key| !["nodeID", "faces"].contains(&key.as_str())) {
            return Err(invalid(format!("faces[]: unknown member {extra:?}")));
        }
        let id = node_id(object.get("nodeID").unwrap_or(&Value::Null), "faces[].nodeID")?;
        let indices = object
            .get("faces")
            .and_then(Value::as_array)
            .filter(|indices| !indices.is_empty())
            .ok_or_else(|| invalid("faces[].faces must be a non-empty array of face indices"))?;
        total += indices.len();
        if total > MAX_FACE_INDICES {
            return Err(invalid(format!(
                "faces: at most {MAX_FACE_INDICES} face indices per call"
            )));
        }
        let indices = indices
            .iter()
            .map(|value| match value.as_i64() {
                Some(index) if (0..=i64::from(i32::MAX)).contains(&index) => Ok(index),
                _ => Err(invalid(format!(
                    "faces[].faces must be integers >= 0, got {value}"
                ))),
            })
            .collect::<Result<Vec<i64>, ToolError>>()?;
        entries.push((id, indices));
    }
    Ok(SetRequest { op, nodes: None, faces: Some(entries) })
}

/// The bridge's params for a validated request.
fn bridge_params(request: &SetRequest) -> Value {
    let mut params = Map::new();
    params.insert("op".into(), json!(request.op));
    if let Some(nodes) = &request.nodes {
        params.insert("nodeIDs".into(), json!(nodes));
    }
    if let Some(faces) = &request.faces {
        params.insert(
            "faces".into(),
            Value::Array(
                faces
                    .iter()
                    .map(|(id, indices)| json!({"nodeID": id, "faces": indices}))
                    .collect(),
            ),
        );
    }
    Value::Object(params)
}

/// What a replace/add asked for that the reply's `applied` does not hold (Hammer filtered it).
/// `None` for a remove/clear, whose "not applied" is the opposite reading, and for a reply whose
/// mode cannot list what was asked.
fn not_applied(request: &SetRequest, applied: &Value) -> Option<Value> {
    if request.op != "replace" && request.op != "add" {
        return None;
    }
    if let Some(nodes) = &request.nodes {
        if !matches!(applied.get("mode").and_then(Value::as_str), Some("groups" | "objects" | "meshes")) {
            return None;
        }
        let have: Vec<i64> = applied["nodeIDs"]
            .as_array()
            .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        let missing: Vec<i64> = nodes.iter().copied().filter(|id| !have.contains(id)).collect();
        return (!missing.is_empty()).then(|| json!({"nodeIDs": missing}));
    }
    let faces = request.faces.as_ref()?;
    if applied.get("mode").and_then(Value::as_str) != Some("faces") {
        return None;
    }
    let held = |id: i64| -> Vec<i64> {
        applied["faces"]
            .as_array()
            .and_then(|meshes| meshes.iter().find(|mesh| mesh["nodeID"].as_i64() == Some(id)))
            .and_then(|mesh| mesh["faces"].as_array())
            .map(|list| list.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default()
    };
    let mut missing = Vec::new();
    for (id, indices) in faces {
        let have = held(*id);
        let mut left: Vec<i64> = indices.iter().copied().filter(|i| !have.contains(i)).collect();
        left.sort_unstable();
        left.dedup();
        if !left.is_empty() {
            missing.push(json!({"nodeID": id, "faces": left}));
        }
    }
    (!missing.is_empty()).then(|| json!({"faces": missing}))
}

/// `set_selection {op, nodeIDs? | faces?}`: validate, take the write gate, read the document for
/// `expectDoc`, send `selection.set`.
pub async fn set_selection(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    let request = parse_request(&args)?;
    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(&state, deadline).await?;
    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "selection.set",
                bridge_params(&request),
                RequestOpts {
                    deadline_ms: Some(deadline),
                    expect_doc: Some(expect_doc(&doc)),
                    gate: None,
                },
            )
            .await
    };
    let mut out = match reply {
        Ok(reply) => reply.result,
        Err(error) => {
            // `-32602` / `-32004` / `-32018` are refusals before anything ran; a selection write
            // keeps no counter, so only the outcome flag differs for the other classes.
            let maybe = maybe_executed(&error);
            let mut tool = ToolError::from(error);
            if maybe {
                tool.outcome_unknown = true;
            } else if tool.code != Some(-32017) {
                // the bridge decided before it touched the selection: retry-safe
                tool.retry_safe = true;
            }
            return Err(tool);
        }
    };
    let Some(object) = out.as_object_mut() else {
        return Err(ToolError::internal(format!(
            "selection.set answered a non-object result: {out}"
        )));
    };
    // The reply's `doc` is the document state after a write; selection is not one (no undo group),
    // and nothing here consumes it.
    object.remove("doc");
    if let Some(missing) = object.get("applied").and_then(|applied| not_applied(&request, applied)) {
        object.insert("notApplied".into(), missing);
        object.insert(
            "note".into(),
            json!("Hammer filtered part of the request (locked or hidden nodes, or meshes that \
                   ignore face selection); applied is what is selected now"),
        );
    }
    object.insert("source".into(), json!("live"));
    object.insert("saved".into(), json!(false));
    Ok(out.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: Value) -> Result<SetRequest, ToolError> {
        parse_request(&args)
    }

    #[test]
    fn the_one_of_rule_and_the_bounds_are_local_refusals() {
        for bad in [
            json!({}),
            json!({"op": "toggle", "nodeIDs": [1]}),
            json!({"op": "replace"}),
            json!({"op": "replace", "nodeIDs": [1], "faces": [{"nodeID": 2, "faces": [0]}]}),
            json!({"op": "replace", "nodeIDs": []}),
            json!({"op": "replace", "nodeIDs": [-1]}),
            json!({"op": "replace", "nodeIDs": [1.5]}),
            json!({"op": "replace", "nodeIDs": ["1"]}),
            json!({"op": "replace", "nodeIDs": [2_147_483_648i64]}),
            json!({"op": "clear", "nodeIDs": [1]}),
            json!({"op": "clear", "faces": []}),
            json!({"op": "add", "faces": []}),
            json!({"op": "add", "faces": [{"nodeID": 1, "faces": []}]}),
            json!({"op": "add", "faces": [{"nodeID": 1, "faces": [-1]}]}),
            json!({"op": "add", "faces": [{"faces": [0]}]}),
            json!({"op": "add", "faces": [{"nodeID": 1, "faces": [0], "x": 1}]}),
            json!({"op": "add", "nodeIDs": [1], "typo": 1}),
        ] {
            assert!(parse(bad.clone()).is_err(), "{bad}");
        }
        let too_many: Vec<i64> = (0..4097).collect();
        assert!(parse(json!({"op": "add", "nodeIDs": too_many})).is_err());
        let ok: Vec<i64> = (0..4096).collect();
        assert!(parse(json!({"op": "add", "nodeIDs": ok})).is_ok());
        let indices: Vec<i64> = (0..65_537).collect();
        assert!(parse(json!({"op": "add", "faces": [{"nodeID": 1, "faces": indices}]})).is_err());
        assert!(parse(json!({"op": "clear"})).is_ok());
    }

    #[test]
    fn not_applied_names_what_hammer_left_out() {
        let nodes = parse(json!({"op": "replace", "nodeIDs": [3, 4]})).unwrap();
        let applied = json!({"mode": "objects", "nodeIDs": [3]});
        assert_eq!(not_applied(&nodes, &applied), Some(json!({"nodeIDs": [4]})));
        let faces = parse(json!({"op": "add", "faces": [{"nodeID": 3, "faces": [0, 1]}]})).unwrap();
        let applied = json!({"mode": "faces", "nodeIDs": [3], "faces": [{"nodeID": 3, "faces": [0]}]});
        assert_eq!(
            not_applied(&faces, &applied),
            Some(json!({"faces": [{"nodeID": 3, "faces": [1]}]}))
        );
        // a failed re-read (applied: null) names nothing
        assert_eq!(not_applied(&nodes, &Value::Null), None);
        let remove = parse(json!({"op": "remove", "nodeIDs": [3]})).unwrap();
        assert_eq!(not_applied(&remove, &json!({"mode": "objects", "nodeIDs": [3]})), None);
    }
}
