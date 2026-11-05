//! Bridge-backed test and input tools: `game_tick`, `checkpoint`, `input_set`, `input_clear`.
//!
//! Every tool validates its arguments before anything is sent (a validation failure never reaches
//! the bridge). Bridge requests take the bridge lock first and then the per-call budget deadline
//! (AGENT.md convention 4). Wire shapes: `../idabins/docs/data/bridge-protocol.md` §7; C handlers
//! in `bridge/mcp_bridge.c` (`op_input`, `op_test`, `json_float3`, `in_each_name`).
use crate::state::ClientState;
use cs2_mcp_common::bridge::{BridgeClient, BridgeError, RequestOpts};
use cs2_mcp_common::tools::{CallCtx, ToolError, ToolOutput};
use serde_json::{json, Map, Value};

// the input.set argument rules live in cs2-mcp-common (hammer-mcp's input_sequence shares them)
pub use cs2_mcp_common::play::input::*;

/// The bridge copies checkpoint names into a 128-byte buffer and truncates longer ones silently
/// (`mcp_bridge.c` `op_test`, `json_string`), so longer names are rejected here.
pub const MAX_CHECKPOINT_NAME: usize = 127;

/// Movement buttons: a `press` of one of these is followed by `game_wait {until: moved}`.
const MOVE_BUTTONS: [&str; 4] = ["forward", "back", "left", "right"];

/// `game.tick`: returned as-is (`{tick, frames}`).
pub async fn game_tick(
    state: ClientState,
    ctx: CallCtx,
    _args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut bridge = state.bridge.lock().await;
    let deadline = ctx.budget.deadline_ms()?;
    let resp = bridge
        .request("game.tick", json!({}), RequestOpts::deadline(deadline))
        .await?;
    Ok(resp.result.into())
}

/// `cvar.get {names}` (1..64): the names are validated here, the reply is redacted by name (`rcon_password`, `*password*`, ...).
pub async fn cvar_get(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let names =
        cs2_mcp_common::console::cvar_names(&args).map_err(ToolError::invalid_arguments)?;
    let mut bridge = state.bridge.lock().await;
    let deadline = ctx.budget.deadline_ms()?;
    let resp = bridge
        .request("cvar.get", json!({"names": names}), RequestOpts::deadline(deadline))
        .await?;
    let mut result = resp.result;
    cs2_mcp_common::console::redact_cvar_reply(&mut result, &names);
    Ok(result.into())
}

/// Validate a checkpoint name: non-empty, at most 127 bytes, no whitespace, control chars or `;`.
pub fn checkpoint_name(args: &Value) -> Result<&str, ToolError> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::invalid_arguments("name must be a string"))?;
    if name.is_empty() {
        return Err(ToolError::invalid_arguments("name must be non-empty"));
    }
    if name.len() > MAX_CHECKPOINT_NAME {
        return Err(ToolError::invalid_arguments(format!(
            "name must be at most {MAX_CHECKPOINT_NAME} bytes (bridge buffer)"
        )));
    }
    if name
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == ';')
    {
        return Err(ToolError::invalid_arguments(
            "name must not contain whitespace, control characters or ';' (test-script syntax)",
        ));
    }
    Ok(name)
}

/// `test.checkpoint {name}` -> `{active}`, plus the echoed name and a note.
pub async fn checkpoint(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let name = checkpoint_name(&args)?;
    let mut bridge = state.bridge.lock().await;
    let deadline = ctx.budget.deadline_ms()?;
    let resp = bridge
        .request(
            "test.checkpoint",
            json!({"name": name}),
            RequestOpts::deadline(deadline),
        )
        .await?;
    let note = if resp.result.get("active").and_then(Value::as_bool) == Some(false) {
        "no test script is active (TestScriptMgr IsActive=false), so this checkpoint had no listener"
    } else {
        "checkpoints that arrive while the script is in Test_Wait are dropped; use Test_WaitForCheckPoint barriers"
    };
    Ok(with_fields(
        resp.result,
        vec![("name", json!(name)), ("note", json!(note))],
    )
    .into())
}


/// `input.set` (buttons + optional view override), with `turn` resolved via `input.status`.
///
/// Every error carries `data.inputCleared`: `true` when a best-effort `input.clear` succeeded,
/// `"failed"` when it was sent and failed, `false` when none was sent. A clear is sent only after
/// an outcome-unknown bridge error (the set may have partly applied); it is never sent after a
/// retry-safe error or `-32602`, where the bridge did not act.
pub async fn input_set(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    input_set_inner(state, ctx, args).await.map_err(|e| {
        if e.data.contains_key("inputCleared") {
            e
        } else {
            e.with_data(json!({"inputCleared": false}))
        }
    })
}

async fn input_set_inner(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let InputRequest { mut set, turn } = parse_input_set(&args)?;
    let mut bridge = state.bridge.lock().await;
    let mut resolved = None;
    if let Some(turn) = turn {
        if !bridge.has("input.status") {
            return Err(ToolError::not_implemented(
                "CapabilityMissing",
                "turn needs input.status to read the current override angles; the bridge does not advertise it (nothing was sent)",
                json!({"missing": ["input.status"]}),
            ));
        }
        let deadline = ctx.budget.deadline_ms()?;
        let status = bridge
            .request("input.status", json!({}), RequestOpts::deadline(deadline))
            .await?;
        let angles = apply_turn(override_angles(&status.result)?, turn)?;
        set.angles = Some(Some(angles));
        resolved = Some(angles);
    }
    let moves = set.press.iter().any(|b| MOVE_BUTTONS.contains(&b.as_str()));
    let params = serde_json::to_value(&set)
        .map_err(|e| ToolError::internal(format!("cannot encode input.set params: {e}")))?;
    let deadline = ctx.budget.deadline_ms()?;
    match bridge
        .request("input.set", params, RequestOpts::deadline(deadline))
        .await
    {
        Ok(resp) => {
            let mut fields = Vec::new();
            if let Some(angles) = resolved {
                fields.push(("resolvedAngles", json!(angles)));
            }
            if moves {
                fields.push(("next", json!("game_wait {\"until\":\"moved\"}")));
            }
            Ok(with_fields(resp.result, fields).into())
        }
        Err(e) => {
            let cleared = clear_after_failed_set(&mut bridge, &ctx, &e).await;
            Err(ToolError::from(e).with_data(json!({"inputCleared": cleared})))
        }
    }
}

/// The best-effort release after a failed `input.set`. Returns the `inputCleared` value.
async fn clear_after_failed_set(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    e: &BridgeError,
) -> Value {
    if !e.outcome_unknown() {
        return json!(false); // the bridge did not act (retry-safe or rejected params)
    }
    if !bridge.has("input.clear") {
        return json!(false);
    }
    let Ok(deadline) = ctx.budget.deadline_ms() else {
        return json!(false);
    };
    match bridge
        .request("input.clear", json!({}), RequestOpts::deadline(deadline))
        .await
    {
        Ok(_) => json!(true),
        Err(_) => json!("failed"),
    }
}

/// `input.clear {}`: release every virtual button and disable the angle override.
pub async fn input_clear(
    state: ClientState,
    ctx: CallCtx,
    _args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut bridge = state.bridge.lock().await;
    let deadline = ctx.budget.deadline_ms()?;
    let resp = bridge
        .request("input.clear", json!({}), RequestOpts::deadline(deadline))
        .await?;
    let mut result = resp.result;
    cs2_mcp_common::play::input::settle_cleared_reply(&mut result);
    Ok(result.into())
}

/// Add fields to a bridge result object (a non-object result is kept under `result`).
fn with_fields(result: Value, fields: Vec<(&str, Value)>) -> Value {
    let mut map = match result {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("result".into(), other);
            m
        }
    };
    for (k, v) in fields {
        map.insert(k.into(), v);
    }
    Value::Object(map)
}

pub fn checkpoint_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_CHECKPOINT_NAME,
                "pattern": "^[^\\s;]+$"
            }
        },
        "required": ["name"]
    })
}


pub fn input_clear_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn checkpoint_name_rules() {
        assert!(checkpoint_name(&json!({"name": "gt_begin_x"})).is_ok());
        for bad in [
            json!({}),
            json!({"name": ""}),
            json!({"name": "a b"}),
            json!({"name": "a;b"}),
            json!({"name": "a\nb"}),
            json!({"name": "x".repeat(128)}),
        ] {
            assert!(checkpoint_name(&bad).is_err(), "{bad}");
        }
    }

}
