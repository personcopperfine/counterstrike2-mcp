//! `set_node_transform` (bridge m2.18 `node.setTransform`): move/rotate/scale one live node
//! (a `CMapEntity` or a `CMapMesh`) through Hammer's own virtual setters, so the helpers and the
//! view follow at once. Plan: `target/WP.md` WP-R2, wire contract `target/BRIDGE-M2.18-SPEC.md`
//! §3, ground truth `target/re-props.md` Q2.
//!
//! What the tool does, in order (nothing is sent before every local check passed):
//!
//! 1. `doc.active` ([`require_document`]) — the `expectDoc` fingerprint the write carries is read
//!    off that same reply, so a document switch between the read and the write is `-32004`.
//! 2. **read the node first** (`entity.get`). For a relative transform that read is what composes
//!    the absolute values; it is made in absolute mode too, because it is what lets the tool name
//!    the entity's FGD class (the `prop_static` compile note is only true for one), report
//!    `previous`, and fail a dead node id with the read taxonomy before an undo group opens. A
//!    mesh (or group) node answers `-32602 {reason:"not an entity"}`: that is *expected* — the
//!    tool notes it and proceeds, because `node.setTransform` accepts meshes and the bridge has no
//!    live mesh transform read.
//! 3. `node.setTransform {nodeID, origin?, angles?, scales?, expectDoc}` — only the members the
//!    caller asked for (missing members keep their live value).
//!
//! Relative mode (`relative:true`): `origin`/`angles` are **deltas added to the current values**
//! and `scales` are **factors multiplied with the current values**. The current values come from
//! step 2's read; a requested relative field the read cannot supply is refused with a clear error
//! rather than composed from an invented base. The m2.18 `entity.get` reports `origin`, `angles`
//! **and `scales`** (the C++ node fields, `re-props.md` Q2), so relative scaling of an entity
//! works; relative mode has no read of a mesh node's transform (`node.list` carries origins only;
//! `mesh.get` could supply it but this tool does not use it), so any relative field is refused for
//! it.
//!
//! A successful transform counts as an unsaved edit ([`note_edit`]); the m2.17 `doc` reply tail is
//! not echoed (`set_entity_keyvalues` strips it the same way).
use crate::state::HammerState;
use cs2_mcp_common::bridge::{BridgeError, RequestOpts};
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};

/// Review L7: the plumbing shared with `entities_live`/`mesh_live` (the document read with its
/// `expectDoc` fingerprint, the unsaved-edit counter, the maybe-executed taxonomy).
pub(crate) use super::live_common::{
    bool_arg, doc_id_of, expect_doc, invalid, mutator_failure, node_id_arg, note_edit,
    reject_unknown_keys, require_document, MAX_NODE_ID,
};

/// The undo label the m2.18 `node.setTransform` arm opens (spec §3; echoed as `undoLabel` — the
/// bridge's reply does not carry one today).
const SET_TRANSFORM_UNDO_LABEL: &str = "MCP: set transform";
/// `|origin|` per coordinate (the bridge refuses anything larger before `undo_open`, spec §3).
const ORIGIN_MAX_ABS: f64 = 1_048_576.0;
/// `scales` per axis: `0 < scale <= 1000` (spec §3).
const SCALE_MAX: f64 = 1000.0;
/// Every accepted argument (`additionalProperties:false` is the schema's contract; the handler is
/// the gate).
const KNOWN_ARGS: [&str; 5] = ["nodeID", "origin", "angles", "scales", "relative"];
/// The no-save note every mutator carries.
const NO_SAVE_NOTE: &str = "no auto-save: run save_map to write the changes; file-backed reads \
                            stay stale until then";
/// The compile-time note for a `prop_static` (its class carries the FGD `tags(PropStatic)` tag).
const STATIC_PROP_NOTE: &str = "prop_static is compiled into the world (resourcecompiler bakes \
                                it): this move changes the editor and the view only until the \
                                next compile_map";

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "set_node_transform",
            "Move/rotate/scale one live node (entity or mesh) by nodeID. Give at least one of origin/angles/scales; relative=true makes origin/angles deltas and scales factors (needs a readable current value; refused for mesh scales). A moved prop_static changes the editor only until compile_map. Undoable; not saved.",
            Mutating,
            &["node.setTransform", "entity.get"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "nodeID": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_NODE_ID
                },
                "origin": {
                    "type": "array",
                    "items": {"type": "number"},
                    "minItems": 3,
                    "maxItems": 3,
                    "description": "world units; a delta if relative"
                },
                "angles": {
                    "type": "array",
                    "items": {"type": "number"},
                    "minItems": 3,
                    "maxItems": 3,
                    "description": "[pitch, yaw, roll] degrees; a delta if relative"
                },
                "scales": {
                    "type": "array",
                    "items": {"type": "number"},
                    "minItems": 3,
                    "maxItems": 3,
                    "description": "each in (0, 1000]; a factor if relative"
                },
                "relative": {
                    "type": "boolean",
                    "default": false
                }
            },
            "required": ["nodeID"],
            "additionalProperties": false
        })),
        set_node_transform,
    );
}

// -------------------------------------------------------------------------------------------
// Arguments
// -------------------------------------------------------------------------------------------

/// One of the three transform members: `None` when absent/null, else exactly three finite numbers.
/// Shape only — the per-member ranges follow ([`range_ok`]).
fn vec3_arg(label: &str, args: &Value) -> Result<Option<[f64; 3]>, ToolError> {
    let Some(value) = args.get(label).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(values) = value.as_array().filter(|values| values.len() == 3) else {
        return Err(invalid(format!(
            "{label} must be [x, y, z] numbers ([pitch, yaw, roll] for angles), got {value}"
        )));
    };
    let mut out = [0.0f64; 3];
    for (axis, value) in values.iter().enumerate() {
        out[axis] = value
            .as_f64()
            .filter(|number| number.is_finite())
            .ok_or_else(|| {
                invalid(format!(
                    "{label}[{axis}] must be a finite number, got {value}"
                ))
            })?;
    }
    Ok(Some(out))
}

/// `origin`: each coordinate within [`ORIGIN_MAX_ABS`] (the bridge's own gate; a value the f64
/// round-trip cannot carry exactly is refused rather than sent).
fn origin_range_ok(origin: [f64; 3]) -> bool {
    origin
        .iter()
        .all(|axis| axis.is_finite() && axis.abs() <= ORIGIN_MAX_ABS)
}

/// `scales`: each in `(0, `[`SCALE_MAX`]`]`.
fn scales_range_ok(scales: [f64; 3]) -> bool {
    scales
        .iter()
        .all(|axis| axis.is_finite() && *axis > 0.0 && *axis <= SCALE_MAX)
}

/// Read one `[x, y, z]` array out of a **reply** (`entity.get`'s current values): a missing or
/// malformed field is `None` — the caller refuses the relative field, it never invents a base.
fn reply_vec3(value: Option<&Value>) -> Option<[f64; 3]> {
    let values = value?.as_array().filter(|values| values.len() == 3)?;
    let mut out = [0.0f64; 3];
    for (axis, value) in values.iter().enumerate() {
        out[axis] = value.as_f64().filter(|number| number.is_finite())?;
    }
    Some(out)
}

/// The error for a relative transform field the read could not supply: an explicit refusal naming
/// the reason and the way out, never an invented base (`brief: no silent fallbacks`).
fn relative_unreadable(field: &str, why: &str) -> ToolError {
    invalid(format!(
        "relative {field} needs the node's current values, which this read does not report \
         ({why}); send absolute {field} instead (or read them in Hammer), nothing was sent"
    ))
}

// -------------------------------------------------------------------------------------------
// The tool
// -------------------------------------------------------------------------------------------

/// `set_node_transform {nodeID, origin?, angles?, scales?, relative?}`: validate locally, read the
/// node (`entity.get`), compose the relative fields from that read, then one `node.setTransform`.
pub async fn set_node_transform(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(&args, &KNOWN_ARGS)?;
    let node_id = node_id_arg(&args)?;
    let origin = vec3_arg("origin", &args)?;
    let angles = vec3_arg("angles", &args)?;
    let scales = vec3_arg("scales", &args)?;
    let relative = bool_arg(&args, "relative")?.unwrap_or(false);
    if origin.is_none() && angles.is_none() && scales.is_none() {
        return Err(invalid(
            "at least one of origin, angles or scales is required (a call with none of them would \
             write nothing)",
        ));
    }
    if let Some(origin) = origin {
        if !origin_range_ok(origin) {
            return Err(invalid(format!(
                "origin must have each coordinate within +-{ORIGIN_MAX_ABS} (the bridge's own \
                 gate), got {origin:?}"
            )));
        }
    }
    if let Some(scales) = scales {
        if !scales_range_ok(scales) {
            return Err(invalid(format!(
                "scales must have each axis in (0, {SCALE_MAX}] (0, negative, NaN and infinity are \
                 refused), got {scales:?}"
            )));
        }
    }
    if let Some(angles) = angles {
        if angles.iter().any(|axis| axis.abs() > f64::from(f32::MAX)) {
            return Err(invalid(format!(
                "angles must be inside the bridge's 32-bit float range, got {angles:?}"
            )));
        }
    }

    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(&state, deadline).await?;
    let doc_id = doc_id_of(&doc);

    // Step 2: the node's own read. `entity.get` is declared on this spec, so a bridge without it
    // gates the tool out; the m2.18 bridge has it.
    let (classname, current_origin, current_angles, current_scales, not_an_entity) = {
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
                    .map(str::to_string),
                reply_vec3(reply.result.get("origin")),
                reply_vec3(reply.result.get("angles")),
                // the m2.18 entity.get reports the node scales (`re-props.md` Q2, live); a reply
                // that does not carry them (a mesh node's read, or an older bridge) leaves this
                // None, which refuses a relative scales call rather than inventing a base
                reply_vec3(reply.result.get("scales")),
                false,
            ),
            // a mesh/group/world node: expected for a valid target of this tool, not an error
            Err(error) if not_an_entity(&error) => (None, None, None, None, true),
            Err(error) => {
                // the node is gone (or the read failed): nothing was written, so this is the read
                // taxonomy (`EntityNotFound`/`NotAnEntity`), never a counted mutator failure
                return Err(crate::tools::entities_query::read_error(error, node_id));
            }
        }
    };

    let mut notes: Vec<String> = vec![NO_SAVE_NOTE.to_string()];
    if not_an_entity {
        notes.push(
            "the node is not an entity (a mesh or a group): no keyvalues/transform could be read \
             — node.setTransform accepts meshes, so the write continues; a relative field is \
             refused for these nodes (the bridge has no live mesh transform read)"
                .to_string(),
        );
    }

    // Step 3: compose what relative mode needs, then send only the requested members.
    let origin_out = match origin {
        None => None,
        Some(delta) if !relative => Some(delta),
        Some(delta) => {
            let current = current_origin.ok_or_else(|| {
                relative_unreadable(
                    "origin",
                    if not_an_entity {
                        "relative mode has no mesh transform read (node.list carries origins \
                         only; reading it through mesh.get is not implemented here)"
                    } else {
                        "the entity.get reply carried no usable origin"
                    },
                )
            })?;
            Some([
                current[0] + delta[0],
                current[1] + delta[1],
                current[2] + delta[2],
            ])
        }
    };
    let angles_out = match angles {
        None => None,
        Some(delta) if !relative => Some(delta),
        Some(delta) => {
            let current = current_angles.ok_or_else(|| {
                relative_unreadable(
                    "angles",
                    if not_an_entity {
                        "relative mode has no mesh transform read (node.list carries origins \
                         only; reading it through mesh.get is not implemented here)"
                    } else {
                        "the entity.get reply carried no usable angles"
                    },
                )
            })?;
            Some([
                current[0] + delta[0],
                current[1] + delta[1],
                current[2] + delta[2],
            ])
        }
    };
    let scales_out = match scales {
        None => None,
        Some(factor) if !relative => Some(factor),
        Some(factor) => {
            let current = current_scales.ok_or_else(|| {
                relative_unreadable(
                    "scales",
                    "the read carried no usable scales (m2.18's entity.get reports them; relative \
                     mode has no mesh transform read; reading it through mesh.get is not \
                     implemented here)",
                )
            })?;
            Some([
                current[0] * factor[0],
                current[1] * factor[1],
                current[2] * factor[2],
            ])
        }
    };
    // every value that goes out passes the bridge's own gate: the composed values can leave the
    // range even when the caller's delta did not (a relative delta past +-1048576, an angle past the
    // f32 range the bridge's `json_float3_at` refuses, a multiplied scale past 1000)
    if let Some(origin) = origin_out {
        if !origin_range_ok(origin) {
            return Err(invalid(format!(
                "the resulting origin {origin:?} is outside +-{ORIGIN_MAX_ABS} (the bridge's own \
                 gate): nothing was sent"
            )));
        }
    }
    if let Some(angles) = angles_out {
        if angles.iter().any(|axis| axis.abs() > f64::from(f32::MAX)) {
            return Err(invalid(format!(
                "the resulting angles {angles:?} are outside the bridge's 32-bit float range: \
                 nothing was sent"
            )));
        }
    }
    if let Some(scales) = scales_out {
        if !scales_range_ok(scales) {
            return Err(invalid(format!(
                "the resulting scales {scales:?} are outside (0, {SCALE_MAX}] (a relative factor \
                 multiplied with the current values): nothing was sent"
            )));
        }
    }

    let mut params = Map::new();
    params.insert("nodeID".into(), json!(node_id));
    if let Some(origin) = origin_out {
        params.insert("origin".into(), json!(origin));
    }
    if let Some(angles) = angles_out {
        params.insert("angles".into(), json!(angles));
    }
    if let Some(scales) = scales_out {
        params.insert("scales".into(), json!(scales));
    }

    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "node.setTransform",
                Value::Object(params.clone()),
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
        // the wire taxonomy: an `-32602`/`-32018`/`-32004` decision was made before `undo_open`
        // (nothing executed), while an internal/protocol error may have landed — that failure
        // counts the edit (`mutator_failure`)
        Err(error) => return Err(mutator_failure(error, &state, doc_id.as_deref())),
    };
    note_edit(&state, doc_id.as_deref());

    if !reply.result.is_object() {
        return Err(ToolError::internal(format!(
            "node.setTransform returned a non-object result: {} \
             (crates/cs2-bridge/PROTOCOL.md §11.3)",
            reply.result
        )));
    }
    let mut out = match reply.result {
        Value::Object(map) => map,
        _ => unreachable!("checked above"),
    };
    // the m2.17 `doc` tail is not a result field (see the module doc)
    out.remove("doc");
    // a reply that does not carry the state it was asked to write is reported, never papered over
    for (field, read) in [
        ("origin", current_origin.is_some()),
        ("angles", current_angles.is_some()),
        ("scales", current_scales.is_some()),
    ] {
        if relative && read && out.get(field).is_none() {
            notes.push(format!(
                "the bridge's reply carried no {field}: read it back in Hammer"
            ));
        }
    }
    // `previous` is what the tool read before the write; a field the read did not carry is null
    if relative {
        out.insert(
            "previous".into(),
            json!({
                "origin": current_origin.map(|v| json!(v)).unwrap_or(Value::Null),
                "angles": current_angles.map(|v| json!(v)).unwrap_or(Value::Null),
                "scales": current_scales.map(|v| json!(v)).unwrap_or(Value::Null),
            }),
        );
    }
    if let Some(classname) = &classname {
        out.insert("classname".into(), json!(classname));
        if is_compile_time_prop(&state, classname) {
            notes.push(STATIC_PROP_NOTE.to_string());
        }
    }
    out.entry("nodeID").or_insert(json!(node_id));
    out.entry("undoLabel")
        .or_insert(json!(SET_TRANSFORM_UNDO_LABEL));
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("saved".into(), json!(false));
    out.entry("notes").or_insert(json!(notes));
    Ok(Value::Object(out).into())
}

/// True when `classname` is a compile-time-only prop in this FGD: the class carries
/// `tags( PropStatic )` (or inherits it through its base chain). A class the database does not
/// carry (or an unavailable database) is not proven either way, so the note is not claimed.
fn is_compile_time_prop(state: &HammerState, classname: &str) -> bool {
    crate::fgd::load_for(state)
        .ok()
        .and_then(|db| db.prop_tag(classname))
        .is_some_and(|tag| tag == crate::fgd::PropTag::Static)
}

/// A raw `-32602 {reason:"not an entity"}` from the read: the node exists but is a mesh, a group
/// or the world — a valid target class for `node.setTransform`, so it is not an error here.
fn not_an_entity(error: &BridgeError) -> bool {
    error.code == Some(-32602)
        && error
            .data
            .get("reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason.contains("not an entity"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec3_arg_shape_and_finiteness() {
        assert_eq!(vec3_arg("origin", &json!({})).unwrap(), None);
        assert_eq!(vec3_arg("origin", &json!({"origin": null})).unwrap(), None);
        assert_eq!(
            vec3_arg("origin", &json!({"origin": [1, 2.5, -3]})).unwrap(),
            Some([1.0, 2.5, -3.0])
        );
        for bad in [
            json!({"origin": []}),
            json!({"origin": [1, 2]}),
            json!({"origin": [1, 2, 3, 4]}),
            json!({"origin": [1, 2, "3"]}),
            json!({"origin": "1 2 3"}),
        ] {
            assert!(vec3_arg("origin", &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_member_ranges_follow_the_spec() {
        assert!(origin_range_ok([1_048_576.0, -1_048_576.0, 0.0]));
        assert!(!origin_range_ok([1_048_576.5, 0.0, 0.0]));
        assert!(!origin_range_ok([f64::INFINITY, 0.0, 0.0]));

        assert!(scales_range_ok([0.001, 1.0, 1000.0]));
        for bad in [
            [0.0, 1.0, 1.0],
            [-1.0, 1.0, 1.0],
            [1.0, 1.0, 1000.5],
            [f64::NAN, 1.0, 1.0],
        ] {
            assert!(!scales_range_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn reply_vec3_needs_three_finite_numbers() {
        assert_eq!(reply_vec3(Some(&json!([1, 2, 3]))), Some([1.0, 2.0, 3.0]));
        for bad in [
            None,
            Some(&json!(null)),
            Some(&json!([1, 2])),
            Some(&json!([1, 2, "3"])),
            Some(&json!(4)),
        ] {
            assert_eq!(reply_vec3(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn not_an_entity_only_matches_the_reads_own_refusal() {
        let refusal = BridgeError::new(cs2_mcp_common::bridge::Kind::ProtocolError, "x")
            .with_code(-32602)
            .with_data(json!({"reason": "not an entity"}));
        assert!(not_an_entity(&refusal));
        for other in [
            BridgeError::new(cs2_mcp_common::bridge::Kind::ProtocolError, "x").with_code(-32602),
            BridgeError::new(cs2_mcp_common::bridge::Kind::NodeNotFound, "x").with_code(-32022),
        ] {
            assert!(!not_an_entity(&other), "{other:?}");
        }
    }
}
