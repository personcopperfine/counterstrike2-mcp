//! `entity_find`: entities of the RUNNING game (bridge `game.entities`) by designer name and/or
//! targetname. Shared by client-mcp (the tool) and hammer-mcp (the tool plus `play_test`'s
//! `wait_entity` / `expect_entity` steps). Not the Hammer document: that is `find_entities {source:"live"}`.
use crate::bridge::{BridgeClient, RequestOpts};
use crate::tools::{CallCtx, ToolError, ToolOutput};
use serde_json::{json, Map, Value};

pub const SOURCE_NOTE: &str = "bridge game.entities (identities by designer name and/or targetname; truncated is true when more matched than max)";
pub const MAX_MAX: u64 = 256;

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "class": {"type": "string", "pattern": "^[A-Za-z0-9_]{1,64}$"},
            "name": {"type": "string", "minLength": 1, "maxLength": 127},
            "max": {"type": "integer", "minimum": 1, "maximum": MAX_MAX, "default": 64}
        }
    })
}

/// `game.entities` filter: a designer name (`class`) and/or a targetname (`name`), at least one of
/// them, so a lookup cannot list the whole entity table. `max` 1..256 (bridge default 64).
pub fn params(args: &Value) -> Result<Value, ToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| ToolError::invalid_arguments("arguments must be an object"))?;
    let mut params = Map::new();
    if let Some(c) = obj.get("class") {
        let c = c
            .as_str()
            .filter(|c| {
                c.len() <= 64 && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            })
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                ToolError::invalid_arguments(
                    "class must be a designer name (letters, digits and _, up to 64, e.g. func_button)",
                )
            })?;
        params.insert("class".into(), json!(c));
    }
    if let Some(n) = obj.get("name") {
        let n = n
            .as_str()
            .filter(|n| !n.is_empty() && n.len() <= 127 && !n.chars().any(char::is_control))
            .ok_or_else(|| {
                ToolError::invalid_arguments(
                    "name must be a targetname (1..127 bytes; the bridge buffer is 128 with NUL)",
                )
            })?;
        params.insert("name".into(), json!(n));
    }
    if params.is_empty() {
        return Err(ToolError::invalid_arguments(
            "class and/or name is required (entity_find does not list the whole entity table)",
        ));
    }
    if let Some(m) = obj.get("max") {
        let m = m
            .as_u64()
            .filter(|m| (1..=MAX_MAX).contains(m))
            .ok_or_else(|| ToolError::invalid_arguments("max must be an integer 1..256"))?;
        params.insert("max".into(), json!(m));
    }
    Ok(Value::Object(params))
}

/// One raw `game.entities` request with already-validated params.
pub async fn request(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    params: Value,
) -> Result<Value, ToolError> {
    Ok(bridge
        .request("game.entities", params, RequestOpts::deadline(ctx.budget.deadline_ms()?))
        .await
        .map_err(ToolError::from)?
        .result)
}

/// The `entity_find` tool body.
pub async fn find(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    args: &Value,
) -> Result<ToolOutput, ToolError> {
    let mut v = request(bridge, ctx, params(args)?).await?;
    v["source"] = json!(SOURCE_NOTE);
    Ok(v.into())
}

/// The entity a reply selects: the `index`-th of `entities` (first match by default).
pub fn pick(reply: &Value, index: usize) -> Option<&Value> {
    reply.get("entities")?.as_array()?.get(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_need_a_class_or_name_and_validate() {
        assert_eq!(
            params(&json!({"class": "func_movelinear", "max": 3})).unwrap(),
            json!({"class": "func_movelinear", "max": 3})
        );
        assert!(params(&json!({})).is_err());
        assert!(params(&json!({"class": "a b"})).is_err());
        assert!(params(&json!({"name": ""})).is_err());
        assert!(params(&json!({"class": "x", "max": 257})).is_err());
    }

    #[test]
    fn pick_takes_the_indexed_entity() {
        let reply = json!({"entities": [{"entindex": 5}, {"entindex": 9}]});
        assert_eq!(pick(&reply, 0).unwrap()["entindex"], 5);
        assert_eq!(pick(&reply, 1).unwrap()["entindex"], 9);
        assert!(pick(&reply, 2).is_none());
        assert!(pick(&json!({}), 0).is_none());
    }
}
