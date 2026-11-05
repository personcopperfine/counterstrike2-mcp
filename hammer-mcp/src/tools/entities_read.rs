//! `get_entity` / `find_entities` over BOTH routes (WP2.3): `source: "live"|"saved"`.
//!
//! * `live` = the Hammer document through the bridge (`entity.get` / `entity.find`, see
//!   [`super::entities_query`]); `saved` = the `.vmap` on disk ([`super::entities_file`], which
//!   reports `staleRead`).
//! * Omitted `source`: **live** when no `path` was given, a bridge document is open and the bridge
//!   advertises the methods, else **saved**. A call that only the saved route can answer (`path`,
//!   `withKeyvalues`) also falls back to saved. `nameContains` does NOT switch the route (the
//!   bridge's `entity.find` has no substring filter): on the live default it is refused and points
//!   at `get_scene_info {query}`. The result says which route ran (`sourceUsed`), for a defaulted
//!   choice why (`sourceWhy`, naming the open document), and a live result carries `document`.
//! * An argument that does not apply to an explicitly chosen source is refused, naming the
//!   argument and the source that takes it. `targetname` on `get_entity` is matched case-insensitively on BOTH
//!   routes (Hammer's own targetname semantics; live resolves it through `entity.find`) and two
//!   or more matches are `AmbiguousTargetname` with the candidates (pass `nodeID`).
//! * Each route keeps its own output fields (`entity` / `entities` + `source` object on saved, the
//!   bridge reply on live), so a caller of the old tools sees the same shapes. The old names
//!   `get_entity_live` / `find_entities_live` are deprecated aliases with `source:"live"`.
use crate::state::HammerState;
use crate::tools::live_common::read_active_document_locked;
use cs2_mcp_common::tools::{
    require_caps_within, CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec,
    CAPS_LOCK_WAIT,
};
use serde_json::{json, Value};

const LIMIT_MAX: u64 = 2000;
const MAX_NODE_ID: i64 = 2_147_483_647;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Source {
    Live,
    Saved,
}

impl Source {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Source::Live => "live",
            Source::Saved => "saved",
        }
    }
}

pub(crate) fn source_schema() -> Value {
    json!({
        "type": "string", "enum": ["live", "saved"],
        "description": "default live if a document is open"
    })
}

pub(crate) fn path_schema() -> Value {
    json!({
        "type": "string",
        "description": "saved only; default = active doc"
    })
}

pub fn register_get(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "get_entity",
            "One entity (keyvalues, I/O connections) by nodeID or targetname. source live = Hammer document (unsaved edits), saved = .vmap on disk.",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "source": source_schema(),
                "path": path_schema(),
                "nodeID": {
                    "type": "integer", "minimum": 0, "maximum": MAX_NODE_ID,
                    "description": "wins over targetname"
                },
                "targetname": {
                    "type": "string",
                    "description": "case-insensitive; 2+ matches = AmbiguousTargetname"
                }
            },
            "additionalProperties": false
        })),
        get_entity,
    );
}

pub fn register_find(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "find_entities",
            "Find entities by classname/targetname/sphere. source live = Hammer document, saved = .vmap on disk. Not for: the running game (use entity_find).",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "source": source_schema(),
                "path": path_schema(),
                "class": {
                    "type": "string", "minLength": 1,
                    "description": "case-insensitive"
                },
                "targetname": {
                    "type": "string", "minLength": 1,
                    "description": "case-insensitive"
                },
                "within": {
                    "type": "object",
                    "description": "sphere on the node origin",
                    "properties": {
                        "origin": {
                            "type": "array", "items": {"type": "number"},
                            "minItems": 3, "maxItems": 3
                        },
                        "radius": {"type": "number", "exclusiveMinimum": 0}
                    },
                    "required": ["origin", "radius"],
                    "additionalProperties": false
                },
                "nameContains": {
                    "type": "string", "minLength": 1,
                    "description": "saved only (live: get_scene_info query)"
                },
                "withKeyvalues": {
                    "type": "boolean",
                    "description": "saved only: include each entity's keyvalues"
                },
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": LIMIT_MAX,
                    "description": "max returned"
                }
            },
            "additionalProperties": false
        })),
        find_entities,
    );
}

/// The `source` argument: `None` = not given.
pub(crate) fn source_arg(args: &Value) -> Result<Option<Source>, ToolError> {
    match args.get("source") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s == "live" => Ok(Some(Source::Live)),
        Some(Value::String(s)) if s == "saved" => Ok(Some(Source::Saved)),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "source must be \"live\" or \"saved\", got {other}"
        ))),
    }
}

fn present(args: &Value, key: &str) -> bool {
    args.get(key).is_some_and(|v| !v.is_null())
}

/// Refuse the arguments that only `other` takes when the caller chose `chosen` explicitly.
pub(crate) fn refuse_foreign(
    args: &Value,
    chosen: Source,
    keys: &[&str],
    takes: Source,
) -> Result<(), ToolError> {
    for key in keys {
        if present(args, key) {
            return Err(ToolError::invalid_arguments(format!(
                "{key} applies to source \"{}\" only, but source is \"{}\": drop {key} or set \
                 source:\"{}\"",
                takes.name(),
                chosen.name(),
                takes.name()
            )));
        }
    }
    Ok(())
}

/// The route a call runs on, why it was chosen (a defaulted choice only) and, for a defaulted
/// live choice, the open Hammer document's path.
pub(crate) struct Picked {
    pub source: Source,
    pub why: Option<String>,
    pub document: Option<String>,
}

/// Pick the route. `saved_only` are the arguments only the saved route understands; `methods` the
/// bridge methods the live route needs.
pub(crate) async fn pick_source(
    state: &HammerState,
    ctx: &CallCtx,
    args: &Value,
    saved_only: &[&str],
    methods: &[&str],
) -> Result<Picked, ToolError> {
    let saved = |why: &str| Picked {
        source: Source::Saved,
        why: Some(why.to_string()),
        document: None,
    };
    if let Some(explicit) = source_arg(args)? {
        return Ok(Picked { source: explicit, why: None, document: None });
    }
    if saved_only.iter().any(|k| present(args, k)) {
        return Ok(saved("default: an argument only the saved route takes was given"));
    }
    let deadline = ctx.budget.deadline_ms()?;
    let mut bridge = state.bridge.lock().await;
    let doc = match read_active_document_locked(state, &mut bridge, Some(deadline)).await {
        Ok(Some(doc)) => doc,
        Ok(None) => return Ok(saved("default: no bridge document is open")),
        Err(error) => {
            return Ok(saved(&format!(
                "default: the bridge did not answer doc.active ({}), so no live document was assumed",
                error.message
            )))
        }
    };
    if !bridge.missing(methods).is_empty() {
        return Ok(saved("default: the bridge does not advertise the live read"));
    }
    let path = doc.get("path").and_then(Value::as_str).map(str::to_string);
    let why = match &path {
        Some(path) => format!("default: bridge document {path} is open"),
        None => "default: a bridge document is open".to_string(),
    };
    Ok(Picked { source: Source::Live, why: Some(why), document: path })
}

/// Capability gate for the live route (the registry gate is off: the saved route needs no bridge).
pub(crate) async fn need_live(
    state: &HammerState,
    ctx: &CallCtx,
    methods: &[&str],
) -> Result<(), ToolError> {
    require_caps_within(&state.bridge, methods, CAPS_LOCK_WAIT.min(ctx.budget.remaining())).await
}

/// Add `sourceUsed` (and `sourceWhy` for a defaulted choice) to an object result.
pub(crate) fn stamp(mut out: ToolOutput, picked: &Picked) -> ToolOutput {
    if let Some(m) = out.json.as_object_mut() {
        m.insert("sourceUsed".into(), json!(picked.source.name()));
        if let Some(why) = &picked.why {
            m.insert("sourceWhy".into(), json!(why));
        }
        if let Some(document) = &picked.document {
            m.insert("document".into(), json!(document));
        }
    }
    out
}

pub async fn get_entity(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    // a live lookup by name also needs entity.find: check it before choosing the route
    let by_name = !present(&args, "nodeID") && present(&args, "targetname");
    let methods: &[&str] = if by_name { &["entity.get", "entity.find"] } else { &["entity.get"] };
    let picked = pick_source(&state, &ctx, &args, &["path"], methods).await?;
    let out = match picked.source {
        Source::Saved => super::entities_file::get_entity(state, ctx, args).await?,
        Source::Live => {
            refuse_foreign(&args, Source::Live, &["path"], Source::Saved)?;
            need_live(&state, &ctx, methods).await?;
            let node_id = match super::entities_file::optional_i64(&args, "nodeID")? {
                Some(id) => id,
                None => match args.get("targetname").and_then(Value::as_str) {
                    Some(name) if !name.is_empty() => {
                        super::entities_query::resolve_live_targetname(&state, &ctx, name).await?
                    }
                    _ => {
                        return Err(ToolError::invalid_arguments(
                            "one selector is required: nodeID or targetname",
                        ))
                    }
                },
            };
            super::entities_query::get_entity_live(state, ctx, json!({"nodeID": node_id})).await?
        }
    };
    Ok(stamp(out, &picked))
}

pub async fn find_entities(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    // `nameContains` does not pick the route: entity.find has no substring filter, so on the
    // default (live) route it is refused with a pointer instead of silently reading the file
    let picked = pick_source(&state, &ctx, &args, &["path", "withKeyvalues"], &["entity.find"]).await?;
    let out = match picked.source {
        Source::Saved => super::entities_file::find_entities(state, ctx, args).await?,
        Source::Live => {
            refuse_foreign(&args, Source::Live, &["path", "withKeyvalues"], Source::Saved)?;
            if present(&args, "nameContains") {
                return Err(ToolError::invalid_arguments(
                    "nameContains applies to source \"saved\" only, but the live document is the \
                     source: use get_scene_info {query} (substring of classname/targetname, live or \
                     saved) or set source:\"saved\"",
                ));
            }
            need_live(&state, &ctx, &["entity.find"]).await?;
            super::entities_query::find_entities_live(state, ctx, args).await?
        }
    };
    Ok(stamp(out, &picked))
}
