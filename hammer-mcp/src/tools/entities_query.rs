//! Live entity reads through the bridge (m2.15): `get_entity_live` (`entity.get`) and
//! `find_entities_live` (`entity.find`). Separate tools from the file route so a bridge without
//! these methods only gates these two. Plan: target/entities-plan.md WP-T2b.
//!
//! Both are read-only `geo_dispatch(..., mutating=0)` reads of the LIVE document (the world tree
//! Hammer currently holds): nothing is saved and nothing is validated against the saved `.vmap`,
//! so the results carry the bridge's own `source:"live"`. Arguments are validated locally first
//! (`nodeID` in 0..=2147483647 — the bridge casts to `int`, `within`/`limit` shaped like
//! `find_entities`, no empty `class`/`targetname` and no coordinate that would serialize in
//! exponent form) so a bad call never reaches the bridge.
//!
//! `get_entity_live` maps the bridge's refusals into the file route's vocabulary: `-32602
//! {reason:"not an entity"}` (world/mesh/group) is `NotAnEntity` with `data.nodeID`, `-32022` is
//! `EntityNotFound` with `data.nodeID` (both retry-safe: nothing was executed). Since bridge m2.18
//! `entity.get` reports the LIVE I/O connection vector (`connections`: `outputName`, `targetType`,
//! `targetName`, `inputName`, `overrideParam`, `delay`, `timesToFire` — an empty array for a node
//! without connections), so the result's `note` points at the file route for the SAVED
//! `connectionsData` instead of claiming there are none (the m2.18 write tools live in
//! `entity_io.rs`).
use crate::state::HammerState;
use cs2_mcp_common::bridge::{BridgeError, RequestOpts};
use cs2_mcp_common::tools::{CallCtx, Registry, ToolError, ToolOutput};
use serde_json::{json, Map, Value};

/// `entity.find`'s `limit` bound and default (plan §3.2: default 500, at most 2000).
const LIMIT_MAX: u64 = 2000;
/// The bridge parses `nodeID` into an `int`: above this it truncates and reads a different node
/// (brief item 5). Enforced locally before anything is sent.
const MAX_NODE_ID: i64 = 2_147_483_647;
/// A coordinate/radius magnitude the bridge's plain-decimal number scan reads exactly (brief
/// item 7).
const COORD_MAX_ABS: f64 = 1e9;
/// Below this magnitude `serde_json` writes exponent notation, which the bridge misreads: snap to
/// 0 (opus #8).
const COORD_MIN_ABS: f64 = 1e-4;

/// The `note` every live read carries when the file route can say more (brief WP-T2b; m2.18:
/// `connections` is the LIVE vector the bridge reports, and the file route reads the SAVED
/// `connectionsData` — the note says which is which).
const CONNECTIONS_NOTE: &str = "I/O connections: `connections` is the LIVE vector (bridge m2.18 \
                                entity.get); get_entity {source:saved} reads the SAVED .vmap's connectionsData";

/// `get_entity` / `find_entities` (their `source:"live"` route) live in `entities_read.rs`; the
/// old names are deprecated aliases registered here, after every real tool.
pub fn register_aliases(r: &mut Registry<HammerState>) {
    r.alias("get_entity_live", "get_entity", json!({"source": "live"}));
    r.alias("find_entities_live", "find_entities", json!({"source": "live"}));
}

/// The node id of the live entity named `name` (`entity.find`, case-insensitive: Hammer's own
/// targetname semantics). Asks for two hits so a collision is detected: 2+ is
/// `AmbiguousTargetname` with the candidates, retry-safe (nothing was executed).
pub(crate) async fn resolve_live_targetname(
    state: &HammerState,
    ctx: &CallCtx,
    name: &str,
) -> Result<i64, ToolError> {
    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(
            "entity.find",
            json!({"targetname": name, "limit": 2}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await
        .map_err(ToolError::from)?;
    let rows = reply
        .result
        .get("entities")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    match rows {
        [] => Err(ToolError::new(
            "EntityNotFound",
            "EntityNotFound",
            format!("no entity with targetname {name:?} in the live document"),
        )
        .with_data(json!({"targetname": name}))
        .retry_safe(true)),
        [one] => one.get("nodeID").and_then(Value::as_i64).ok_or_else(|| {
            ToolError::internal(format!("entity.find returned a row without nodeID: {one}"))
        }),
        many => {
            let candidates: Vec<Value> = many
                .iter()
                .map(|row| {
                    json!({
                        "nodeID": row.get("nodeID"),
                        "classname": row.get("classname"),
                        "targetname": row.get("targetname"),
                    })
                })
                .collect();
            Err(ambiguous_targetname(name, candidates))
        }
    }
}

/// `AmbiguousTargetname`: both routes of `get_entity {targetname}` answer it when two or more
/// entities carry the name (case-insensitive), instead of silently taking the first.
pub(crate) fn ambiguous_targetname(name: &str, candidates: Vec<Value>) -> ToolError {
    ToolError::new(
        "AmbiguousTargetname",
        "AmbiguousTargetname",
        format!(
            "targetname {name:?} matches more than one entity (case-insensitive): pass nodeID \
             (candidates in data.candidates)"
        ),
    )
    .with_data(json!({"targetname": name, "candidates": candidates}))
    .retry_safe(true)
}

// -------------------------------------------------------------------------------------------
// Argument helpers (local copies: the brief keeps shared helpers out of this file's scope)
// -------------------------------------------------------------------------------------------

/// `nodeID` (required): an integer in `0..=`[`MAX_NODE_ID`], refused locally when absent/garbage
/// or above the bridge's `int` range (a bigger id truncates and reads a different node).
fn node_id_arg(args: &Value) -> Result<i64, ToolError> {
    match args.get("nodeID") {
        Some(Value::Number(n)) => n
            .as_i64()
            .filter(|id| (0..=MAX_NODE_ID).contains(id))
            .ok_or_else(|| {
                ToolError::invalid_arguments(format!(
                    "nodeID must be an integer in 0..={MAX_NODE_ID}, got {n}"
                ))
            }),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "nodeID must be an integer in 0..={MAX_NODE_ID}, got {other}"
        ))),
        None => Err(ToolError::invalid_arguments(format!(
            "nodeID is required (an integer in 0..={MAX_NODE_ID})"
        ))),
    }
}

/// Optional string argument: `None` when absent or null, an error for any other type.
fn optional_str<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "{name} must be a string, got {other}"
        ))),
    }
}

/// A filter that is present but empty: the bridge treats `""` as *no filter* and would answer the
/// whole document, so it is refused locally instead (brief item 13).
fn filter_str<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match optional_str(args, name)? {
        Some("") => Err(ToolError::invalid_arguments(format!(
            "{name} is empty: an empty filter matches nothing while the bridge reads it as no \
             filter at all; omit {name} to list everything"
        ))),
        other => Ok(other),
    }
}

/// One coordinate component the bridge reads back exactly: finite, `|x| <= `[`COORD_MAX_ABS`], and
/// snapped to 0 below [`COORD_MIN_ABS`] (where `serde_json` switches to exponent notation).
fn coord_value(label: &str, axis: usize, number: f64) -> Result<f64, ToolError> {
    if !number.is_finite() {
        return Err(ToolError::invalid_arguments(format!(
            "{label}[{axis}] must be a finite number, got {number}"
        )));
    }
    if number.abs() > COORD_MAX_ABS {
        return Err(ToolError::invalid_arguments(format!(
            "{label}[{axis}] is {number}: |value| > {COORD_MAX_ABS} cannot be sent (serde_json \
             would write it in exponent notation, which the bridge's number scan misreads)"
        )));
    }
    Ok(if number.abs() < COORD_MIN_ABS { 0.0 } else { number })
}

/// [`coord_value`] for a JSON value, naming the component in the error.
fn coord_axis(label: &str, axis: usize, value: &Value) -> Result<f64, ToolError> {
    let number = value
        .as_f64()
        .filter(|number| number.is_finite())
        .ok_or_else(|| {
            ToolError::invalid_arguments(format!(
                "{label}[{axis}] must be a finite number, got {value}"
            ))
        })?;
    coord_value(label, axis, number)
}

/// `within?: {origin: [x,y,z], radius > 0}` -> `Some((origin, radius))`.
fn within_arg(args: &Value) -> Result<Option<([f64; 3], f64)>, ToolError> {
    let Some(within) = args.get("within").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let invalid = |msg: String| ToolError::invalid_arguments(format!("within: {msg}"));
    let values = within
        .get("origin")
        .and_then(Value::as_array)
        .filter(|values| values.len() == 3)
        .ok_or_else(|| invalid("origin must be [x, y, z]".to_string()))?;
    let mut origin = [0.0; 3];
    for (axis, value) in values.iter().enumerate() {
        origin[axis] = coord_axis("within.origin", axis, value)?;
    }
    let radius = within
        .get("radius")
        .and_then(Value::as_f64)
        .ok_or_else(|| invalid("radius must be a number > 0".to_string()))?;
    let radius = coord_value("within.radius", 0, radius)?;
    if radius <= 0.0 {
        return Err(invalid("radius must be a number > 0".to_string()));
    }
    Ok(Some((origin, radius)))
}

/// `limit?: integer` 1..=[`LIMIT_MAX`] -> `Some(limit)`; absent = the bridge's own default.
fn limit_arg(args: &Value) -> Result<Option<u64>, ToolError> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(limit) if (1..=LIMIT_MAX).contains(&limit) => Ok(Some(limit)),
            _ => Err(ToolError::invalid_arguments(format!(
                "limit must be an integer in 1..={LIMIT_MAX}, got {n}"
            ))),
        },
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "limit must be an integer in 1..={LIMIT_MAX}, got {other}"
        ))),
    }
}

// -------------------------------------------------------------------------------------------
// Error mapping
// -------------------------------------------------------------------------------------------

/// An `entity.get` failure in the file route's vocabulary (brief WP-T2b + FIX-R item 10): `-32602
/// {reason:"not an entity"}` -> `NotAnEntity`, `-32022` -> `EntityNotFound`, both with
/// `data.nodeID` and retry-safe (nothing was executed). Everything else keeps the wire mapping.
/// Shared by every entity tool that addresses a node (`entities_live::set_entity_keyvalues`,
/// `lifecycle::delete_nodes`) so one bridge condition has one tool-level name.
pub(crate) fn read_error(error: BridgeError, node_id: i64) -> ToolError {
    let reason = error
        .data
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match error.code {
        Some(-32602) if reason.contains("not an entity") => ToolError::new(
            "NotAnEntity",
            "NotAnEntity",
            format!(
                "nodeID {node_id} is not an entity (world, mesh or group): the live read only \
                 covers entities"
            ),
        )
        .with_code(-32602)
        .with_data(json!({"nodeID": node_id, "reason": reason}))
        .retry_safe(true),
        Some(-32022) => ToolError::new(
            "EntityNotFound",
            "EntityNotFound",
            format!("nodeID {node_id} is not in the live document"),
        )
        .with_code(-32022)
        .with_data(json!({"nodeID": node_id, "reason": reason}))
        .retry_safe(true),
        _ => ToolError::from(error),
    }
}

/// The bridge answered something that is not a result object: be honest, never invent one.
fn malformed(method: &str, result: &Value) -> ToolError {
    ToolError::internal(format!(
        "{method} returned a non-object result: {result} (crates/cs2-bridge/PROTOCOL.md §9.1, §9.2)"
    ))
}

// -------------------------------------------------------------------------------------------
// get_entity_live
// -------------------------------------------------------------------------------------------

/// `get_entity_live {nodeID}`: `entity.get` passthrough (classname, targetname, origin, angles,
/// keyvalues as text, `unsupported`, `connections` — the live vector since m2.18, an empty array
/// for a node without any — `source:"live"`) plus the file-route `note` for I/O connections.
pub async fn get_entity_live(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let node_id = node_id_arg(&args)?;
    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(
            "entity.get",
            json!({"nodeID": node_id}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => return Err(read_error(error, node_id)),
    };
    if !reply.result.is_object() {
        return Err(malformed("entity.get", &reply.result));
    }
    let mut out = reply.result;
    out["note"] = json!(CONNECTIONS_NOTE);
    Ok(out.into())
}

// -------------------------------------------------------------------------------------------
// find_entities_live
// -------------------------------------------------------------------------------------------

/// `find_entities_live {class?, targetname?, within?, limit?}`: `entity.find` passthrough
/// (`entities` rows, `count`, `visited`, `truncated`, `source:"live"`). Filters are exact and
/// case-insensitive on the bridge; no filter lists everything up to `limit`; an empty `class` or
/// `targetname` is refused (the bridge would read it as no filter at all — brief item 13).
pub async fn find_entities_live(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let class = filter_str(&args, "class")?;
    let targetname = filter_str(&args, "targetname")?;
    let within = within_arg(&args)?;
    let limit = limit_arg(&args)?;

    let mut params = Map::new();
    if let Some(class) = class {
        params.insert("class".into(), json!(class));
    }
    if let Some(targetname) = targetname {
        params.insert("targetname".into(), json!(targetname));
    }
    if let Some((origin, radius)) = within {
        params.insert("within".into(), json!({"origin": origin, "radius": radius}));
    }
    if let Some(limit) = limit {
        params.insert("limit".into(), json!(limit));
    }

    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(
            "entity.find",
            Value::Object(params),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => return Err(ToolError::from(error)),
    };
    if !reply.result.is_object() {
        return Err(malformed("entity.find", &reply.result));
    }
    Ok(reply.result.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_arg_shape() {
        assert_eq!(node_id_arg(&json!({"nodeID": 0})).unwrap(), 0);
        assert_eq!(node_id_arg(&json!({"nodeID": 42})).unwrap(), 42);
        assert_eq!(node_id_arg(&json!({"nodeID": MAX_NODE_ID})).unwrap(), MAX_NODE_ID);
        for bad in [
            json!({}),
            json!({"nodeID": null}),
            json!({"nodeID": -1}),
            json!({"nodeID": 1.5}),
            json!({"nodeID": "30"}),
            json!({"nodeID": true}),
            // above the bridge's `int`: it would truncate to a different node
            json!({"nodeID": 4_294_967_298i64}),
            json!({"nodeID": MAX_NODE_ID + 1}),
        ] {
            assert_eq!(node_id_arg(&bad).unwrap_err().name, "InvalidParams", "{bad}");
        }
    }

    #[test]
    fn limit_arg_bounds() {
        assert_eq!(limit_arg(&json!({})).unwrap(), None);
        assert_eq!(limit_arg(&json!({"limit": 1})).unwrap(), Some(1));
        assert_eq!(limit_arg(&json!({"limit": 2000})).unwrap(), Some(2000));
        for bad in [json!({"limit": 0}), json!({"limit": 2001}), json!({"limit": "500"})] {
            assert_eq!(limit_arg(&bad).unwrap_err().name, "InvalidParams", "{bad}");
        }
    }

    #[test]
    fn within_arg_shape() {
        assert_eq!(within_arg(&json!({})).unwrap(), None);
        assert_eq!(
            within_arg(&json!({"within": {"origin": [1.0, 2.0, 3.0], "radius": 5.0}})).unwrap(),
            Some(([1.0, 2.0, 3.0], 5.0))
        );
        // a sub-1e-4 component would serialize as `5.5e-17` (the bridge's scan stops at `e`): 0
        assert_eq!(
            within_arg(&json!({"within": {"origin": [5.5e-17, 2.0, 3.0], "radius": 5.0}})).unwrap(),
            Some(([0.0, 2.0, 3.0], 5.0))
        );
        for bad in [
            json!({"within": {"origin": [1.0, 2.0], "radius": 5.0}}),
            json!({"within": {"origin": [1.0, 2.0, 3.0], "radius": 0.0}}),
            json!({"within": {"origin": [1.0, 2.0, 3.0]}}),
            json!({"within": {"origin": [1.0, 2.0, "x"], "radius": 5.0}}),
            json!({"within": {"origin": [1.0, 2.0, 3.0], "radius": "5"}}),
            // above the exact range: `1e20` is written in exponent notation
            json!({"within": {"origin": [1e20, 2.0, 3.0], "radius": 5.0}}),
            json!({"within": {"origin": [1.0, 2.0, 3.0], "radius": 1e20}}),
        ] {
            assert_eq!(within_arg(&bad).unwrap_err().name, "InvalidParams", "{bad}");
        }
    }

    #[test]
    fn empty_filters_are_refused() {
        assert_eq!(filter_str(&json!({}), "class").unwrap(), None);
        assert_eq!(filter_str(&json!({"class": "x"}), "class").unwrap(), Some("x"));
        for bad in [json!({"class": ""}), json!({"targetname": ""})] {
            let err = filter_str(&bad, if bad.get("class").is_some() { "class" } else { "targetname" })
                .unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}");
        }
    }
}
