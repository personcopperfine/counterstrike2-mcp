//! Entity I/O connections (bridge m2.18): `add_entity_connections` (`entity.addConnections`) and
//! `remove_entity_connections` (`entity.removeConnections`). Plan: `target/WP.md` WP-R2, wire
//! contract `target/BRIDGE-M2.18-SPEC.md` §4, ground truth `target/re-props.md` Q3.
//!
//! The connections are the live `CMapEntityIONode` vector (80-byte entries: output, target, input,
//! override param, delay, timesToFire); `entity.get` reports them since m2.18 and `get_entity`
//! (file route) reports the SAVED `connectionsData`, so the same names come out of both routes
//! (`outputName`, `targetType`, `targetName`, `inputName`, `overrideParam`, `delay`, `timesToFire`).
//!
//! **`add_entity_connections {nodeID, connections, force?}`** validates every connection locally
//! (1..=64 entries; `output`/`target`/`input` non-empty <= 255 bytes without control characters;
//! `delay` finite in [0, 3600]; `timesToFire` an int >= -1 that is not 0; `param` text, may be
//! empty), reads the source entity (`entity.get`) for its live FGD class, resolves every `target`
//! in the live document (`entity.find` by targetname) and then checks the names against the FGD —
//! all of it before anything is sent:
//!
//! * the source class must declare the connection's `output` (FGD outputs, case-insensitive), else
//!   `UnknownOutput` listing the class's outputs; `force:true` skips this check;
//! * a `target` that names exactly one live entity whose class the FGD carries must accept the
//!   `input` (its own or an inherited one), else `UnknownInput` listing the valid inputs;
//!   `force:true` skips this check. Several entities sharing the targetname are checked as a group
//!   (the input has to be declared by at least one of them, with a note naming the ambiguity — the
//!   engine's own name lookup and the surviving recipients are not ours to decide);
//! * a `target` that names a `prop_static` (the FGD `tags(PropStatic)` tag) is **refused**
//!   outright — a compile-time prop is baked into the world and has no runtime inputs, so a
//!   connection to it can never fire. `force` does not skip this one;
//! * a `target` that names nothing, or starts with `!` (`!activator`, `!self`, `!player`, ...), is
//!   allowed with a note: it may be created later, and the `!` names are runtime keywords;
//! * an unavailable FGD is a warning, never a refusal (the `create_entities` policy): no name
//!   check runs then, and the result says so under `fgd.unavailable`.
//!
//! The add is **per-entry verified by the bridge**: `added[i]` is read back out of the vector, and
//! an entry the vector did not confirm comes back as `{"readback":false}` (carried positionally in
//! `landed[]`). Such an entry is *not* a success — it was handed to `AddConnection` and may be in
//! the document — so the tool reports it under `unconfirmed` (wire positions), sets
//! `outcomeUnknown:true` and says "read it back, do not retry" in `notes` (review W2 #3: a caller
//! that retried a maybe-executed entry duplicated the connection). `count` is the bridge's
//! confirmed-add count.
//!
//! WP-T: the add's body is `connection_run`, one **gate-free** core (`add_entity_connections` is
//! the gate + envelope over it) whose [`ConnectionSource`] says where the source class comes from:
//! `Node` is the live source entity and writes, `Class` is `create_trigger`'s pre-flight — the same
//! FGD output/input checks against the class it is about to create, with **nothing sent** (so an
//! unknown output refuses the trigger call before its volume exists). The gate stays in the
//! handlers: a core never locks [`HammerState::write_gate`] (one lock per tool call, never nested).
//!
//! **`remove_entity_connections {nodeID, indices? | match?}`** is deliberately thin: exactly one
//! of the two forms, then the bridge resolves it against the live vector *inside the write's own
//! dispatch* — out-of-range or duplicate indices and a `match` with no hit are refused there
//! (nothing opened). Indices are positional, so a Hammer edit between the caller's read and this
//! call can shift them: `expectDoc` catches a document switch, not an in-document edit (the
//! fingerprint does not move for edits) — `match` is the safer form, and the result notes it. The
//! tool reads the bridge's explicit **`remaining`** (the vector's size after the group) and never
//! the reply's `count` — that one is the number REMOVED (review W2 #1: reading it as the remainder
//! reported "3 left" for 5-3) — and reports both (`removedCount` = `count`).
//!
//! Both tools count the edit on success ([`note_edit`], like `set_entity_keyvalues`) and do not
//! echo the m2.17 `doc` reply tail. A failure that may have landed (`mutator_failure`) bumps the
//! counter too.
use crate::fgd;
use crate::state::HammerState;
use cs2_mcp_common::bridge::{BridgeError, Kind, RequestOpts};
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Review L7: the plumbing shared with `entities_live`/`lifecycle` (the document read with its
/// `expectDoc` fingerprint, the unsaved-edit counter, the maybe-executed taxonomy).
pub(crate) use super::live_common::{
    bool_arg, doc_id_of, expect_doc, invalid, mutator_failure, node_id_arg, note_edit,
    reject_unknown_keys, require_document, MAX_NODE_ID,
};

/// One request carries at most this many connections (spec §4).
const MAX_CONNECTIONS: usize = 64;
/// `remove_entity_connections`' `indices` carries at most this many entries — the bridge's own
/// `RM_INDEX_MAX` (the m2.18 `indices: [1..512 distinct ints]`, and what `entity.get` lists).
/// Review W2 #8: the schema used to reuse [`MAX_CONNECTIONS`] (64), a tool-side cap smaller than
/// the bridge's that the description did not name.
const MAX_INDICES: usize = 512;
/// `output`/`target`/`input`/`param` are at most this many bytes (spec §4).
const MAX_FIELD_BYTES: usize = 255;
/// The `targetType` every connection the bridge writes carries (the Hammer UI default and every
/// saved Valve connection; spec §4, `re-props.md` Q3).
const BRIDGE_TARGET_TYPE: i64 = 7;
/// `delay` is finite in `0..=`[`MAX_DELAY`] (spec §4).
const MAX_DELAY: f64 = 3600.0;
/// The undo label the m2.18 `entity.addConnections` arm opens (spec §4).
const ADD_UNDO_LABEL: &str = "MCP: add connections";
/// The remove arm's label: **inferred** — the spec names the add label only. The reply's own
/// `undoLabel` wins when it carries one.
const REMOVE_UNDO_LABEL: &str = "MCP: remove connections";
/// The no-save note every mutator carries.
const NO_SAVE_NOTE: &str = "no auto-save: run save_map to write the changes; file-backed reads \
                            (get_entity {source:saved}) stay stale until then";
/// The argument names each tool accepts (`additionalProperties:false` is the schema's contract;
/// the handler is the gate).
const ADD_ARGS: [&str; 3] = ["nodeID", "connections", "force"];
const REMOVE_ARGS: [&str; 3] = ["nodeID", "indices", "match"];

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "add_entity_connections",
            "Add I/O connections to a live entity by nodeID (one undo group, never saves). Output, target input and prop_static targets are checked against the FGD (force skips the FGD checks, never the prop_static refusal). An entry the bridge could not confirm comes back under `unconfirmed` with outcomeUnknown: read it back, do not retry. Example: trigger_multiple OnStartTouch -> door SetAnimationLooping.",
            Mutating,
            &["entity.addConnections", "entity.get", "entity.find"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "nodeID": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_NODE_ID,
                    "description": "source entity node id"
                },
                "connections": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_CONNECTIONS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "output": {
                                "type": "string",
                                "minLength": 1,
                                "description": "FGD output name, e.g. OnStartTouch"
                            },
                            "target": {
                                "type": "string",
                                "minLength": 1,
                                "description": "targetname; `!activator`/`!self`/`!player` or a name not yet in the document are allowed"
                            },
                            "input": {
                                "type": "string",
                                "minLength": 1,
                                "description": "FGD input name, e.g. SetAnimationLooping"
                            },
                            "param": {
                                "type": "string",
                                "description": "override parameter, e.g. animation name"
                            },
                            "delay": {
                                "type": "number",
                                "minimum": 0,
                                "maximum": MAX_DELAY,
                                "description": "seconds"
                            },
                            "timesToFire": {
                                "type": "integer",
                                "minimum": -1,
                                "description": "-1 (default) = every time; 0 refused"
                            }
                        },
                        "required": ["output", "target", "input"],
                        "additionalProperties": false
                    }
                },
                "force": {
                    "type": "boolean",
                    "default": false,
                    "description": "skip the FGD output/input checks"
                }
            },
            "required": ["nodeID", "connections"],
            "additionalProperties": false
        })),
        add_entity_connections,
    );
    r.register(
        ToolSpec::new(
            "remove_entity_connections",
            "Remove live entity I/O connections by nodeID, by `indices` or `match` (output/target/input; safer, indices shift with every edit). One undo group, never saves. `remaining` is what is left; the bridge `count` is the number removed.",
            Mutating,
            &["entity.removeConnections"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "nodeID": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_NODE_ID,
                    "description": "entity node id"
                },
                "indices": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_INDICES,
                    "items": {"type": "integer", "minimum": 0},
                    "description": "0-based, as get_entity lists them; max 512; out-of-range or duplicates refused"
                },
                "match": {
                    "type": "object",
                    "description": "all connections matching these fields; at least one key",
                    "properties": {
                        "output": {"type": "string", "minLength": 1},
                        "target": {"type": "string", "minLength": 1},
                        "input": {"type": "string", "minLength": 1}
                    },
                    "additionalProperties": false
                }
            },
            "required": ["nodeID"],
            "additionalProperties": false
        })),
        remove_entity_connections,
    );
}

// -------------------------------------------------------------------------------------------
// Arguments
// -------------------------------------------------------------------------------------------

/// One validated `connections[i]` entry: the wire members the caller gave (absent members are not
/// sent — the bridge's own defaults are `param ""`, `delay 0`, `timesToFire -1`, spec §4).
///
/// `pub(crate)` since WP-T: `create_trigger` pre-checks its connections through
/// [`connection_run`] with the class it is about to create, and reuses the very same parsed list
/// for the write that follows the tie.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Connection {
    output: String,
    target: String,
    input: String,
    param: Option<String>,
    delay: Option<f64>,
    times_to_fire: Option<i64>,
}

impl Connection {
    /// The exact wire object for this connection.
    fn json(&self) -> Value {
        let mut out = Map::new();
        out.insert("output".into(), json!(self.output));
        out.insert("target".into(), json!(self.target));
        out.insert("input".into(), json!(self.input));
        if let Some(param) = &self.param {
            out.insert("param".into(), json!(param));
        }
        if let Some(delay) = self.delay {
            out.insert("delay".into(), json!(delay));
        }
        if let Some(times) = self.times_to_fire {
            out.insert("timesToFire".into(), json!(times));
        }
        Value::Object(out)
    }

    /// The `added[i]` entry for a reply that carried none: the values this server sent with the
    /// bridge's defaults filled in, in the bridge's own `added` shape (`sb_add_conn_fields`: the
    /// `.vmap` reader's names, `targetType` 7 — review W2 #2, the fallback used to invent a third
    /// shape), and `index` unknown (`null`).
    fn assumed_json(&self, target_type: i64) -> Value {
        json!({
            "index": Value::Null,
            "outputName": self.output,
            "targetType": target_type,
            "targetName": self.target,
            "inputName": self.input,
            "overrideParam": self.param.clone().unwrap_or_default(),
            "delay": self.delay.unwrap_or(0.0),
            "timesToFire": self.times_to_fire.unwrap_or(-1),
        })
    }
}

/// A connection string: `output`/`target`/`input` are required and non-empty, `param` is optional
/// and may be empty; all of them are at most [`MAX_FIELD_BYTES`] bytes and carry no control
/// characters (spec §4). `Ok(None)` = the optional member was not given.
fn connection_string(
    label: &str,
    entry: &Map<String, Value>,
    key: &str,
    required: bool,
) -> Result<Option<String>, ToolError> {
    let Some(value) = entry.get(key).filter(|value| !value.is_null()) else {
        return if required {
            Err(invalid(format!(
                "{label}.{key} is required (a non-empty string)"
            )))
        } else {
            Ok(None)
        };
    };
    let Some(text) = value.as_str() else {
        return Err(invalid(format!(
            "{label}.{key} must be a string, got {value}"
        )));
    };
    if required && text.is_empty() {
        return Err(invalid(format!(
            "{label}.{key} must not be empty (the FGD name it refers to is required)"
        )));
    }
    if text.len() > MAX_FIELD_BYTES {
        return Err(invalid(format!(
            "{label}.{key} is {} bytes, at most {MAX_FIELD_BYTES} fit",
            text.len()
        )));
    }
    if text.chars().any(char::is_control) {
        return Err(invalid(format!(
            "{label}.{key} cannot contain control characters"
        )));
    }
    Ok(Some(text.to_string()))
}

/// `connections`: 1..=[`MAX_CONNECTIONS`] objects, each with exactly the wire members (an unknown
/// member is a typo that would silently drop an edit — `delayTime` for `delay` — so it is refused
/// rather than ignored).
fn connections_arg(args: &Value) -> Result<Vec<Connection>, ToolError> {
    connections_list(args.get("connections").unwrap_or(&Value::Null))
}

/// [`connections_arg`] on the array itself (WP-T: `create_trigger` validates its own
/// `connections` argument before its mesh exists, so it parses the list first and hands it to
/// [`connection_run`] twice).
pub(crate) fn connections_list(value: &Value) -> Result<Vec<Connection>, ToolError> {
    let Some(list) = value.as_array() else {
        return Err(invalid(
            "connections is required: an array of 1..=64 {output, target, input, param?, delay?, \
             timesToFire?} objects",
        ));
    };
    if list.is_empty() {
        return Err(invalid("connections is empty, there is nothing to add"));
    }
    if list.len() > MAX_CONNECTIONS {
        return Err(invalid(format!(
            "connections has {} entries, at most {MAX_CONNECTIONS} fit in one request",
            list.len()
        )));
    }
    let allowed = ["output", "target", "input", "param", "delay", "timesToFire"];
    let mut out = Vec::with_capacity(list.len());
    for (index, entry) in list.iter().enumerate() {
        let label = format!("connections[{index}]");
        let Some(entry) = entry.as_object() else {
            return Err(invalid(format!("{label} must be an object")));
        };
        if let Some(unknown) = entry.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(invalid(format!(
                "{label} has the unknown member {unknown:?}: a connection takes output, target, \
                 input, param, delay and timesToFire"
            )));
        }
        let output = connection_string(&label, entry, "output", true)?.expect("required");
        let target = connection_string(&label, entry, "target", true)?.expect("required");
        let input = connection_string(&label, entry, "input", true)?.expect("required");
        let param = connection_string(&label, entry, "param", false)?;
        let delay = match entry.get("delay").filter(|value| !value.is_null()) {
            None => None,
            Some(value) => {
                let delay = value
                    .as_f64()
                    .filter(|delay| delay.is_finite())
                    .ok_or_else(|| {
                        invalid(format!(
                            "{label}.delay must be a finite number in 0..={MAX_DELAY}, got {value}"
                        ))
                    })?;
                if !(0.0..=MAX_DELAY).contains(&delay) {
                    return Err(invalid(format!(
                        "{label}.delay is {delay}, outside 0..={MAX_DELAY} (the bridge's own gate)"
                    )));
                }
                Some(delay)
            }
        };
        let times_to_fire = match entry.get("timesToFire").filter(|value| !value.is_null()) {
            None => None,
            Some(value) => {
                let times = value.as_i64().filter(|times| *times != 0).ok_or_else(|| {
                    invalid(format!(
                        "{label}.timesToFire must be an int >= -1 and not 0, got {value}"
                    ))
                })?;
                if times < -1 {
                    return Err(invalid(format!(
                        "{label}.timesToFire is {times}, below -1 (-1 = fire every time)"
                    )));
                }
                Some(times)
            }
        };
        out.push(Connection {
            output,
            target,
            input,
            param,
            delay,
            times_to_fire,
        });
    }
    Ok(out)
}

/// `force?`: `true` skips the FGD output/input checks (never the prop_static refusal).
fn force_arg(args: &Value) -> Result<bool, ToolError> {
    Ok(bool_arg(args, "force")?.unwrap_or(false))
}

/// `indices?`: 1..=[`MAX_CONNECTIONS`] non-negative ints. Duplicates and out-of-range values are
/// **not** checked here: the bridge resolves them against the live vector in the write's own
/// dispatch, and its refusal is passed through (the vector can move between this server's reads).
fn indices_arg(value: &Value) -> Result<Vec<i64>, ToolError> {
    let Some(list) = value.as_array() else {
        return Err(invalid("indices must be an array of ints"));
    };
    if list.is_empty() {
        return Err(invalid("indices is empty, there is nothing to remove"));
    }
    if list.len() > MAX_INDICES {
        return Err(invalid(format!(
            "indices has {} entries, at most {MAX_INDICES} fit in one request (the bridge's own \
             cap)",
            list.len()
        )));
    }
    let mut out = Vec::with_capacity(list.len());
    for (position, value) in list.iter().enumerate() {
        let Some(index) = value.as_i64().filter(|index| *index >= 0) else {
            return Err(invalid(format!(
                "indices[{position}] must be a non-negative integer, got {value}"
            )));
        };
        out.push(index);
    }
    Ok(out)
}

/// `match?`: at least one of `output`/`target`/`input`, each a non-empty string of at most
/// [`MAX_FIELD_BYTES`] bytes without control characters. An unknown member is refused: a typo like
/// `targets` would otherwise silently broaden the match and remove connections the caller did not
/// name.
fn match_arg(value: &Value) -> Result<Map<String, Value>, ToolError> {
    let Some(matcher) = value.as_object() else {
        return Err(invalid(
            "match must be an object with at least one of output, target or input",
        ));
    };
    if matcher.is_empty() {
        return Err(invalid(
            "match is empty: at least one of output, target or input is required (an empty match \
             would remove everything)",
        ));
    }
    let mut out = Map::new();
    for (key, value) in matcher {
        if !matches!(key.as_str(), "output" | "target" | "input") {
            return Err(invalid(format!(
                "match has the unknown member {key:?}: it takes output, target and input"
            )));
        }
        let Some(text) = value.as_str() else {
            return Err(invalid(format!(
                "match.{key} must be a string, got {value}"
            )));
        };
        if text.is_empty() {
            return Err(invalid(format!(
                "match.{key} must not be empty (an empty field matches only empty names)"
            )));
        }
        if text.len() > MAX_FIELD_BYTES {
            return Err(invalid(format!(
                "match.{key} is {} bytes, at most {MAX_FIELD_BYTES} fit",
                text.len()
            )));
        }
        if text.chars().any(char::is_control) {
            return Err(invalid(format!(
                "match.{key} cannot contain control characters"
            )));
        }
        out.insert(key.clone(), json!(text));
    }
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// FGD helpers (the `create_entities` policy: an unavailable database is a warning, never a refusal)
// -------------------------------------------------------------------------------------------

/// The FGD class database for this state, or the warning that stands in for it (`entities_live`'s
/// `FgdSource`, mirrored here: this file does not reach into that module's private types).
enum FgdSource {
    Loaded(Arc<fgd::Database>),
    Unavailable(String),
}

impl FgdSource {
    fn load(state: &HammerState) -> FgdSource {
        match fgd::load_for(state) {
            Ok(db) => FgdSource::Loaded(db),
            Err(error) => FgdSource::Unavailable(format!("fgd unavailable: {error}")),
        }
    }

    fn db(&self) -> Option<&fgd::Database> {
        match self {
            FgdSource::Loaded(db) => Some(db),
            FgdSource::Unavailable(_) => None,
        }
    }

    fn warning(&self) -> Option<String> {
        match self {
            FgdSource::Loaded(_) => None,
            FgdSource::Unavailable(message) => Some(message.clone()),
        }
    }

    fn json(&self) -> Value {
        match self {
            FgdSource::Loaded(db) => json!({
                "files": db.files.len(),
                "warnings": db.warnings.iter().take(10).collect::<Vec<_>>(),
                "warningCount": db.warnings.len(),
            }),
            FgdSource::Unavailable(message) => json!({"files": 0, "unavailable": message}),
        }
    }
}

/// The class's declared input or output names, sorted case-insensitively (stable in every result
/// and error message), or `None` when the database does not carry the class.
fn io_names(db: &fgd::Database, class: &str, outputs: bool) -> Option<Vec<String>> {
    let def = db.class(class)?;
    let mut names: Vec<String> = if outputs {
        def.outputs.iter().map(|io| io.name.clone()).collect()
    } else {
        def.inputs.iter().map(|io| io.name.clone()).collect()
    };
    names.sort_by_key(|name| name.to_ascii_lowercase());
    Some(names)
}

/// True when the class declares the FGD name (case-insensitive).
fn declares(names: &[String], name: &str) -> bool {
    names
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(name))
}

/// The class is a compile-time prop (`tags( PropStatic )`, directly or through its base chain).
fn is_compile_time_prop(db: &fgd::Database, class: &str) -> bool {
    db.prop_tag(class) == Some(fgd::PropTag::Static)
}

/// An unknown FGD output: refused locally with the class's own list (`force:true` skips).
fn unknown_output(index: usize, class: &str, output: &str, valid: &[String]) -> ToolError {
    ToolError::new(
        "UnknownOutput",
        "UnknownOutput",
        format!(
            "connections[{index}].output '{output}' is not an output of the source class '{class}' \
             (FGD): its outputs are {}; nothing was sent (pass force:true to write it anyway)",
            if valid.is_empty() {
                "none".to_string()
            } else {
                valid.join(", ")
            }
        ),
    )
    .with_code(-32602)
    .with_data(json!({"index": index, "class": class, "output": output, "valid": valid}))
    .retry_safe(true)
}

/// An unknown FGD input on the resolved target: refused locally with the target's own list.
fn unknown_input(
    index: usize,
    target: &str,
    input: &str,
    classes: &[(String, Vec<String>)],
) -> ToolError {
    let listed: Vec<String> = classes
        .iter()
        .map(|(class, valid)| {
            format!(
                "{class}: {}",
                if valid.is_empty() {
                    "no inputs".to_string()
                } else {
                    valid.join(", ")
                }
            )
        })
        .collect();
    ToolError::new(
        "UnknownInput",
        "UnknownInput",
        format!(
            "connections[{index}].input '{input}' is not an input of any live entity named \
             '{target}' ({}); nothing was sent (pass force:true to write it anyway)",
            listed.join(" | ")
        ),
    )
    .with_code(-32602)
    .with_data(json!({
        "index": index,
        "target": target,
        "input": input,
        "classes": classes
            .iter()
            .map(|(class, valid)| json!({"class": class, "valid": valid}))
            .collect::<Vec<Value>>(),
    }))
    .retry_safe(true)
}

/// A connection targeting a compile-time `prop_static`: always refused (it is baked into the
/// world and exists at runtime as no input-carrying entity at all), `force` does not skip this.
fn static_prop_target(index: usize, target: &str, nodes: &[(i64, String)]) -> ToolError {
    let listed: Vec<String> = nodes
        .iter()
        .map(|(node_id, class)| format!("nodeID {node_id} ({class})"))
        .collect();
    ToolError::new(
        "StaticPropTarget",
        "StaticPropTarget",
        format!(
            "connections[{index}] targets '{target}', which the live document holds as a \
             compile-time prop ({}): prop_static is baked into the world by the resource compiler \
             and has no runtime inputs, so this connection can never fire (force does not skip \
             this check). Rename the runtime target or use prop_dynamic; nothing was sent",
            listed.join(", ")
        ),
    )
    .with_code(-32602)
    .with_data(json!({
        "index": index,
        "target": target,
        "staticNodes": nodes.iter().map(|(node_id, class)| json!({"nodeID": node_id, "class": class})).collect::<Vec<Value>>(),
    }))
    .retry_safe(true)
}

/// One live entity row of an `entity.find` reply.
#[derive(Debug, Clone)]
struct LiveTarget {
    node_id: i64,
    classname: String,
}

/// Resolve one `target` in the live document (`entity.find` by targetname, exact and
/// case-insensitive on the bridge). `None` = no live entity carries that name.
async fn live_targets(
    state: &HammerState,
    deadline: u64,
    target: &str,
) -> Result<Option<Vec<LiveTarget>>, ToolError> {
    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(
            "entity.find",
            json!({"targetname": target}),
            RequestOpts::deadline(deadline),
        )
        .await
        .map_err(ToolError::from)?;
    drop(bridge);
    if !reply.result.is_object() {
        return Err(ToolError::internal(format!(
            "entity.find returned a non-object result: {} (crates/cs2-bridge/PROTOCOL.md §9.2)",
            reply.result
        )));
    }
    let rows = reply
        .result
        .get("entities")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some(LiveTarget {
                        node_id: row.get("nodeID").and_then(Value::as_i64)?,
                        classname: row
                            .get("classname")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    })
                })
                .collect::<Vec<LiveTarget>>()
        })
        .unwrap_or_default();
    Ok(if rows.is_empty() { None } else { Some(rows) })
}

// -------------------------------------------------------------------------------------------
// The shared connection run (add_entity_connections + create_trigger's step (c), WP-T)
// -------------------------------------------------------------------------------------------

/// Where a connection run reads the SOURCE class from. `create_trigger` (WP-T) needs the very
/// same pre-checks `add_entity_connections` runs, but **before** its entity exists, so the class
/// stands in for the live node and nothing is sent.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ConnectionSource<'a> {
    /// The live source entity: `entity.get` gives its classname, and the run **writes** the
    /// `entity.addConnections` group (`add_entity_connections`, `create_trigger`'s step (c)).
    Node(i64),
    /// A class the caller is about to create: no source read, and the run **sends nothing at all**
    /// — it is the pre-flight that makes `create_trigger` refuse an unknown output before its mesh
    /// exists.
    Class(&'a str),
}

/// What one connection run found, and (for [`ConnectionSource::Node`]) what it wrote.
pub(crate) struct ConnectionRun {
    /// The source class the checks used: the live classname, or the class the caller passed.
    pub classname: Option<String>,
    /// One `{class, outputs}` or `{target, nodes}` entry per check that ran (the result's
    /// `checked`).
    pub checked: Vec<Value>,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
    pub fgd: Value,
    /// The write's numbers; `None` for [`ConnectionSource::Class`] (nothing was sent).
    pub write: Option<ConnectionWrite>,
}

/// The `entity.addConnections` write one [`ConnectionSource::Node`] run performed.
pub(crate) struct ConnectionWrite {
    /// The bridge's own reply members with `doc` removed (the m2.17 tail is not a result field).
    pub result: Map<String, Value>,
    /// `added[]`, or the request-shaped fallback when the bridge sent none.
    pub added: Vec<Value>,
    /// The bridge's confirmed-add count (`None` with the fallback entries).
    pub count: Option<i64>,
    /// The wire positions the vector did not confirm (review W2 #3: maybe-executed).
    pub unconfirmed: Vec<i64>,
    /// True when `unconfirmed` is non-empty (the caller sets `outcomeUnknown`).
    pub unconfirmed_note: bool,
    /// True when `added` is this server's assumption (`addedFromRequest`).
    pub from_request: bool,
}

/// **The gate-free core of `add_entity_connections`**, and (WP-T) of `create_trigger`'s connection
/// step: one undo group per run, everything checked first (see the module doc).
///
/// It takes the already-parsed connection list so the caller that runs it twice — create_trigger's
/// pre-flight against the class it is about to create, then the write against the node it made —
/// validates one list. It never locks [`HammerState::write_gate`]: its callers own the gate for
/// their whole bridge sequence (`live_common`'s rule: one lock per tool call, never nested).
pub(crate) async fn connection_run(
    state: &HammerState,
    deadline: u64,
    source: ConnectionSource<'_>,
    connections: &[Connection],
    force: bool,
) -> Result<ConnectionRun, ToolError> {
    // the write's document read (and with it the `expectDoc` fingerprint) — the Class form never
    // touches the document at all
    let (doc, doc_id, classname) = match source {
        ConnectionSource::Class(class) => (None, None, Some(class.to_string())),
        ConnectionSource::Node(node_id) => {
            let doc = require_document(state, deadline).await?;
            let doc_id = doc_id_of(&doc);
            // the SOURCE entity: its live FGD class drives the output check
            let classname = {
                let mut bridge = state.bridge.lock().await;
                match bridge
                    .request(
                        "entity.get",
                        json!({"nodeID": node_id}),
                        RequestOpts::deadline(deadline),
                    )
                    .await
                {
                    Ok(reply) => reply
                        .result
                        .get("classname")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    Err(error) => {
                        return Err(crate::tools::entities_query::read_error(error, node_id));
                    }
                }
            };
            (Some(doc), doc_id, classname)
        }
    };

    let fgd = FgdSource::load(state);
    let mut warnings: Vec<String> = fgd.warning().into_iter().collect();
    let mut notes: Vec<String> = vec![NO_SAVE_NOTE.to_string()];
    let mut checked: Vec<Value> = Vec::new();

    if let (Some(db), Some(classname)) = (fgd.db(), classname.as_deref()) {
        // 1. the source class must declare every output (FGD, case-insensitive)
        match io_names(db, classname, true) {
            None => warnings.push(format!(
                "fgd: the source class '{classname}' is not in the FGD database; the outputs were \
                 not validated"
            )),
            Some(valid) => {
                if !force {
                    for (index, connection) in connections.iter().enumerate() {
                        if !declares(&valid, &connection.output) {
                            return Err(unknown_output(
                                index,
                                classname,
                                &connection.output,
                                &valid,
                            ));
                        }
                    }
                }
                checked.push(json!({"class": classname, "outputs": valid}));
            }
        }

        // 2. every target the live document names: prop_static refusal + input check
        let mut resolved: Vec<(String, Option<Vec<LiveTarget>>)> = Vec::new();
        for connection in connections {
            if resolved.iter().any(|(name, _)| name == &connection.target) {
                continue;
            }
            // `!`-prefixed names are the engine's runtime keywords (!self, !activator, !player,
            // ...): they never name a document entity, so no lookup is made for them
            let targets = if connection.target.starts_with('!') {
                None
            } else {
                live_targets(state, deadline, &connection.target).await?
            };
            resolved.push((connection.target.clone(), targets));
        }
        for (index, connection) in connections.iter().enumerate() {
            let Some(Some(targets)) = resolved
                .iter()
                .find(|(name, _)| name == &connection.target)
                .map(|(_, targets)| targets)
            else {
                if connection.target.starts_with('!') {
                    notes.push(format!(
                        "connections[{index}].target '{}' is a runtime keyword (`!` name): no live \
                         entity is looked up for it",
                        connection.target
                    ));
                } else {
                    notes.push(format!(
                        "connections[{index}].target '{}' is not in the live document: it may be \
                         created later, and the input was not validated",
                        connection.target
                    ));
                }
                continue;
            };
            let static_nodes: Vec<(i64, String)> = targets
                .iter()
                .filter(|target| is_compile_time_prop(db, &target.classname))
                .map(|target| (target.node_id, target.classname.clone()))
                .collect();
            if !static_nodes.is_empty() {
                return Err(static_prop_target(index, &connection.target, &static_nodes));
            }
            let classes: Vec<(String, Vec<String>)> = targets
                .iter()
                .filter_map(|target| {
                    io_names(db, &target.classname, false)
                        .map(|valid| (target.classname.clone(), valid))
                })
                .collect();
            if classes.is_empty() {
                warnings.push(format!(
                    "connections[{index}].target '{}' names {} live entit(y|ies) whose class is \
                     not in the FGD database; the input was not validated",
                    connection.target,
                    targets.len()
                ));
                continue;
            }
            if !force
                && !classes
                    .iter()
                    .any(|(_, valid)| declares(valid, &connection.input))
            {
                return Err(unknown_input(
                    index,
                    &connection.target,
                    &connection.input,
                    &classes,
                ));
            }
            if targets.len() > 1 {
                notes.push(format!(
                    "connections[{index}].target '{}' names {} live entities ({}): which of them \
                     receives the input is the engine's own name lookup, so the FGD check accepts \
                     an input declared by any of their classes",
                    connection.target,
                    targets.len(),
                    targets
                        .iter()
                        .map(|target| format!("nodeID {}", target.node_id))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            checked.push(json!({
                "target": connection.target,
                "nodes": targets.iter().map(|t| json!({"nodeID": t.node_id, "class": t.classname})).collect::<Vec<Value>>(),
            }));
        }
    } else if fgd.warning().is_none() {
        warnings.push(
            "the source entity's classname could not be read; the FGD check was skipped"
                .to_string(),
        );
    }
    if force {
        notes.push(
            "force:true: the FGD output/input checks were skipped (the prop_static refusal is \
             never skipped)"
                .to_string(),
        );
    }
    let fgd_json = fgd.json();

    let ConnectionSource::Node(node_id) = source else {
        // the pre-flight: the checks above ran, nothing was written
        return Ok(ConnectionRun {
            classname,
            checked,
            warnings,
            notes,
            fgd: fgd_json,
            write: None,
        });
    };
    let doc = doc.expect("the Node form read the document");

    let wire: Vec<Value> = connections.iter().map(Connection::json).collect();
    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "entity.addConnections",
                json!({"nodeID": node_id, "connections": wire}),
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
        Err(error) => return Err(mutator_failure(error, state, doc_id.as_deref())),
    };
    // m2.17's `doc` tail is not a result field — `set_entity_keyvalues` strips it the same way
    note_edit(state, doc_id.as_deref());

    if !reply.result.is_object() {
        return Err(ToolError::internal(format!(
            "entity.addConnections returned a non-object result: {} \
             (crates/cs2-bridge/PROTOCOL.md §9.6)",
            reply.result
        )));
    }
    let mut out = match reply.result {
        Value::Object(map) => map,
        _ => unreachable!("checked above"),
    };
    out.remove("doc");
    let added_missing = !matches!(out.get("added"), Some(Value::Array(_)));
    let added = match out.get("added") {
        Some(Value::Array(added)) => added.clone(),
        _ => {
            notes.push(
                "the bridge's reply carried no `added` array: the entries below are the \
                 connections this server sent with the bridge's defaults, and their `index` is \
                 unknown — read them back with get_entity"
                    .to_string(),
            );
            connections
                .iter()
                .map(|connection| connection.assumed_json(BRIDGE_TARGET_TYPE))
                .collect()
        }
    };
    // the bridge hands each entry to AddConnection and then cross-checks the vector; an entry it
    // could not confirm is `{"readback":false}`/`landed:false` and is NOT a success: report it as
    // unconfirmed (the connection may have landed) instead of letting it pass as one (review W2
    // #3 — a caller that retried such an entry duplicated it).
    let unconfirmed = unconfirmed_entries(&added, out.get("landed"));
    let mut unconfirmed_note = false;
    if !unconfirmed.0.is_empty() || unconfirmed.1 > 0 {
        unconfirmed_note = true;
        let list = unconfirmed
            .0
            .iter()
            .map(|index| index.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let unplaced = if unconfirmed.1 > 0 {
            format!(
                "{}{} further unconfirmed entr(y|ies) could not be placed on the request's order \
                 (the reply carried no `landed` array)",
                if list.is_empty() { "" } else { "; " },
                unconfirmed.1
            )
        } else {
            String::new()
        };
        notes.push(format!(
            "connection(s) {list}{unplaced} were handed to the bridge but the vector did not \
             confirm them: maybe-executed — read them back with get_entity and do not retry \
             them (a retry could duplicate the connection)"
        ));
    }
    // `count` is the bridge's confirmed-add count; a reply that carried no `added` never carried
    // a trustworthy count either (the fallback entries are this server's assumption), so it stays
    // null there with the note above saying why
    let count = if added_missing {
        None
    } else {
        out.get("count")
            .and_then(Value::as_i64)
            .or(Some(added.len() as i64))
    };
    Ok(ConnectionRun {
        classname,
        checked,
        warnings,
        notes,
        fgd: fgd_json,
        write: Some(ConnectionWrite {
            result: out,
            added,
            count,
            unconfirmed: unconfirmed.0,
            unconfirmed_note,
            from_request: added_missing,
        }),
    })
}

// -------------------------------------------------------------------------------------------
// add_entity_connections
// -------------------------------------------------------------------------------------------

/// `add_entity_connections {nodeID, connections, force?}`: everything is checked before anything
/// is sent (see the module doc), then one `entity.addConnections` with the `expectDoc` read off
/// this call's own `doc.active` — all of it in [`connection_run`], this function's shared core
/// with `create_trigger`.
pub async fn add_entity_connections(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(&args, &ADD_ARGS)?;
    let node_id = node_id_arg(&args)?;
    let connections = connections_arg(&args)?;
    let force = force_arg(&args)?;

    let deadline = ctx.budget.deadline_ms()?;
    let run = connection_run(
        &state,
        deadline,
        ConnectionSource::Node(node_id),
        &connections,
        force,
    )
    .await?;
    let write = run
        .write
        .expect("the Node form always performs the write");

    let mut out = write.result;
    out.insert("nodeID".into(), json!(node_id));
    out.insert("count".into(), json!(write.count));
    out.insert("added".into(), Value::Array(write.added));
    out.insert("addedCount".into(), json!(write.count));
    out.insert("unconfirmed".into(), json!(write.unconfirmed));
    if write.unconfirmed_note {
        out.insert("outcomeUnknown".into(), json!(true));
    }
    if write.from_request {
        // the fallback shape is this server's, not the bridge's: say so instead of letting it read
        // like a bridge reply (the note above says what happened)
        out.insert("addedFromRequest".into(), json!(true));
    }
    out.entry("undoLabel").or_insert(json!(ADD_UNDO_LABEL));
    if let Some(classname) = run.classname {
        out.insert("classname".into(), json!(classname));
    }
    out.insert("checked".into(), Value::Array(run.checked));
    out.insert("fgd".into(), run.fgd);
    out.insert("forced".into(), json!(force));
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("saved".into(), json!(false));
    out.insert("warnings".into(), json!(run.warnings));
    out.insert("notes".into(), json!(run.notes));
    Ok(Value::Object(out).into())
}

// -------------------------------------------------------------------------------------------
// remove_entity_connections
// -------------------------------------------------------------------------------------------

/// The bridge's zero-hit `match` (`-32026 ConnectionNotFound`, m2.18; verified live) as the tool reports it: the entity exists, the
/// connection the match named does not — never `EntityNotFound` (the node *is* there), nothing was
/// opened, so it is retry-safe.
fn connection_not_found(error: &BridgeError, node_id: i64, selector: &Value) -> Option<ToolError> {
    if error.kind != Kind::ConnectionNotFound {
        return None;
    }
    let reason = error
        .data
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("no connection matches");
    Some(
        ToolError::new(
            "ConnectionNotFound",
            "ConnectionNotFound",
            format!(
                "nodeID {node_id} has no connection matching {selector}: nothing was removed \
                 ({reason})"
            ),
        )
        .with_code(-32026)
        .with_data(json!({"nodeID": node_id, "match": selector, "reason": reason}))
        .retry_safe(true),
    )
}

/// The `added` entries the bridge did not confirm, as the **wire index** of the connection the
/// caller sent (review W2 #3), plus how many unconfirmed entries could not be placed on that
/// wire index.
///
/// Two reply shapes say "not confirmed":
///
/// * `landed[]` — the m2.18 bridge's positional array, one `{index, landed}` per request entry,
///   so `landed[i]` belongs to `connections[i]` of this call. `landed == false` is exact.
/// * the per-entry `{"readback":false}` of the same bridge. `added` and `landed` are built in the
///   same loop, so their positions agree **when `landed` was sent**; a reply without `landed[]`
///   (an older bridge, or a future one that drops the member) can still carry `readback:false`,
///   and its position may or may not be the wire index — those are counted, not placed.
fn unconfirmed_entries(added: &[Value], landed: Option<&Value>) -> (Vec<i64>, usize) {
    let mut out: Vec<i64> = Vec::new();
    let mut unplaced = 0usize;
    let landed_entries = landed.and_then(Value::as_array);
    if let Some(entries) = landed_entries {
        for (position, entry) in entries.iter().enumerate() {
            if entry.get("landed").and_then(Value::as_bool) == Some(false) {
                out.push(position as i64);
            }
        }
    }
    for (position, entry) in added.iter().enumerate() {
        if entry.get("readback").and_then(Value::as_bool) != Some(false) {
            continue;
        }
        if landed_entries.is_some() && out.contains(&(position as i64)) {
            continue; // the positional array already named it (the normal case)
        }
        if landed_entries.is_none() {
            unplaced += 1; // position is not proof of the wire index without the array
        } else {
            out.push(position as i64);
        }
    }
    out.sort_unstable();
    (out, unplaced)
}

/// `remove_entity_connections {nodeID, indices | match}`: thin by design — the bridge resolves the
/// selector against the live vector inside the write's own dispatch and refuses duplicates,
/// out-of-range indices and a zero-hit `match` before opening anything; those refusals pass
/// through (the selector decides how the error reads).
pub async fn remove_entity_connections(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(&args, &REMOVE_ARGS)?;
    let node_id = node_id_arg(&args)?;
    let indices = args.get("indices").filter(|value| !value.is_null());
    let matcher = args.get("match").filter(|value| !value.is_null());
    if indices.is_some() == matcher.is_some() {
        return Err(invalid(
            "exactly one of indices or match is required: indices are positional, match selects \
             by output/target/input",
        ));
    }
    let selector = match (indices, matcher) {
        (Some(indices), _) => {
            let list = indices_arg(indices)?;
            json!({"indices": list})
        }
        (_, Some(matcher)) => {
            let matcher = match_arg(matcher)?;
            json!({"match": matcher})
        }
        _ => unreachable!("exactly one form is present"),
    };

    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(&state, deadline).await?;
    let doc_id = doc_id_of(&doc);

    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "entity.removeConnections",
                {
                    let mut params = match selector.clone() {
                        Value::Object(map) => map,
                        _ => Map::new(),
                    };
                    params.insert("nodeID".into(), json!(node_id));
                    Value::Object(params)
                },
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
        Err(error) => {
            if let Some(tool) = connection_not_found(&error, node_id, &selector) {
                return Err(tool);
            }
            return Err(mutator_failure(error, &state, doc_id.as_deref()));
        }
    };
    note_edit(&state, doc_id.as_deref());

    if !reply.result.is_object() {
        return Err(ToolError::internal(format!(
            "entity.removeConnections returned a non-object result: {} \
             (crates/cs2-bridge/PROTOCOL.md §9.7)",
            reply.result
        )));
    }
    let mut out = match reply.result {
        Value::Object(map) => map,
        _ => unreachable!("checked above"),
    };
    out.remove("doc");
    let mut notes: Vec<String> = vec![
        NO_SAVE_NOTE.to_string(),
        "indices are positional: the bridge resolved them against the live vector inside this \
         write's own dispatch, and expectDoc catches a document switch, not a Hammer edit that \
         moved the vector — prefer `match` when the document may have changed"
            .to_string(),
    ];
    let removed = match out.get("removed") {
        Some(Value::Array(removed)) => {
            // the bridge removes in descending order; report the set ascending (stable output)
            let mut sorted = removed.clone();
            sorted.sort_by_key(|value| value.as_i64().unwrap_or(i64::MAX));
            sorted
        }
        _ => {
            notes.push(
                "the bridge's reply carried no `removed` array: which connections were removed is \
                 unknown — read the entity back with get_entity"
                    .to_string(),
            );
            Vec::new()
        }
    };
    // the bridge's explicit `remaining` (the vector's size after the group, m2.18 +
    // review W2 #1). The reply's `count` is the number REMOVED, never the remainder — reading it
    // as one under-reports by the removed amount — and is reported as `removedCount` below.
    let remaining = out.get("remaining").and_then(Value::as_i64);
    if remaining.is_none() {
        notes.push(
            "the bridge's reply carried no `remaining`: how many connections the entity holds now \
             is unknown (the reply's `count` is the number removed, not what is left) — read the \
             entity back with get_entity"
                .to_string(),
        );
    }
    out.insert("nodeID".into(), json!(node_id));
    // `removedCount` was always len(removed) (a count derived from the list); the bridge's `count`
    // says the same thing (it IS the removed count, review W2 #1) — prefer the bridge's number and
    // fall back to the list's length, so the field can never disagree with the wire
    let removed_count = out
        .get("count")
        .and_then(Value::as_i64)
        .unwrap_or(removed.len() as i64);
    out.insert("removedCount".into(), json!(removed_count));
    out.insert("removed".into(), Value::Array(removed));
    out.insert("remaining".into(), json!(remaining));
    out.entry("undoLabel").or_insert(json!(REMOVE_UNDO_LABEL));
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("saved".into(), json!(false));
    out.insert("notes".into(), json!(notes));
    Ok(Value::Object(out).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_arg_validates_every_member() {
        let good = json!({"connections": [
            {"output": "OnStartTouch", "target": "door", "input": "SetAnimation"},
            {"output": "OnUser1", "target": "!self", "input": "Kill", "param": "", "delay": 0.5,
             "timesToFire": 3}
        ]});
        let parsed = connections_arg(&good).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].param, None);
        assert_eq!(parsed[0].delay, None);
        assert_eq!(parsed[0].times_to_fire, None);
        assert_eq!(parsed[1].param.as_deref(), Some(""));
        assert_eq!(parsed[1].delay, Some(0.5));
        assert_eq!(parsed[1].times_to_fire, Some(3));
        // only the members the caller gave go on the wire
        assert_eq!(
            parsed[0].json(),
            json!({"output": "OnStartTouch", "target": "door", "input": "SetAnimation"})
        );
        assert_eq!(
            parsed[1].json(),
            json!({"output": "OnUser1", "target": "!self", "input": "Kill", "param": "",
                   "delay": 0.5, "timesToFire": 3})
        );
        // the fallback entry fills the bridge's defaults, uses the bridge's own field names, and
        // leaves the index unknown
        let assumed = parsed[0].assumed_json(BRIDGE_TARGET_TYPE);
        assert_eq!(assumed["delay"], json!(0.0));
        assert_eq!(assumed["timesToFire"], json!(-1));
        assert_eq!(assumed["index"], Value::Null);
        assert_eq!(
            assumed,
            json!({"index": Value::Null, "outputName": "OnStartTouch", "targetType": 7,
                   "targetName": "door", "inputName": "SetAnimation", "overrideParam": "",
                   "delay": 0.0, "timesToFire": -1}),
            "the fallback is the bridge's added[] shape, not a third one (review W2 #2)"
        );

        for bad in [
            json!({"connections": []}),
            json!({"connections": "x"}),
            json!({}),
            json!({"connections": [1]}),
            json!({"connections": [{"output": "OnUser1", "target": "t"}]}),
            json!({"connections": [{"output": "", "target": "t", "input": "i"}]}),
            json!({"connections": [{"output": "o", "target": "t", "input": ""}]}),
            json!({"connections": [{"output": "o", "target": null, "input": "i"}]}),
            json!({"connections": [{"output": "o", "target": "t", "input": "i", "delay": -1}]}),
            json!({"connections": [{"output": "o", "target": "t", "input": "i", "delay": 3601}]}),
            json!({"connections": [{"output": "o", "target": "t", "input": "i", "delay": "1"}]}),
            json!({"connections": [{"output": "o", "target": "t", "input": "i", "timesToFire": 0}]}),
            json!({"connections": [{"output": "o", "target": "t", "input": "i", "timesToFire": -2}]}),
            json!({"connections": [{"output": "o", "target": "t", "input": "i", "timesToFire": 1.5}]}),
            // an unknown member would silently drop an edit (a `delayTime` typo)
            json!({"connections": [{"output": "o", "target": "t", "input": "i", "delayTime": 1}]}),
            json!({"connections": [{"output": "o", "target": "t", "input": "i", "param": 3}]}),
            json!({"connections": [{"output": "o", "target": "t", "input": "i", "param": "a\u{0}b"}]}),
        ] {
            let err = connections_arg(&bad).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}");
        }

        // the size limits: 255 bytes pass, 256 do not; 64 entries pass, 65 do not
        let long = "x".repeat(MAX_FIELD_BYTES);
        assert!(connections_arg(
            &json!({"connections": [{"output": "o", "target": "t", "input": long}]})
        )
        .is_ok());
        let too_long = "x".repeat(MAX_FIELD_BYTES + 1);
        assert!(connections_arg(
            &json!({"connections": [{"output": "o", "target": "t", "input": too_long}]})
        )
        .is_err());
        let many: Vec<Value> = (0..=MAX_CONNECTIONS)
            .map(|i| json!({"output": format!("o{i}"), "target": "t", "input": "i"}))
            .collect();
        assert!(connections_arg(&json!({"connections": many})).is_err());
    }

    #[test]
    fn indices_and_match_args() {
        assert_eq!(indices_arg(&json!([0, 2])).unwrap(), vec![0, 2]);
        // review W2 #8: the cap is the bridge's 512 (it used to be the 64 of `connections`)
        assert!(indices_arg(&json!((0..MAX_INDICES as i64).collect::<Vec<i64>>())).is_ok());
        assert!(
            indices_arg(&json!((0..MAX_CONNECTIONS as i64).collect::<Vec<i64>>())).is_ok(),
            "65 indices are inside the bridge's cap: they go to the bridge, which refuses them \
             only when the entity has fewer connections"
        );
        for bad in [
            json!([]),
            json!("0"),
            json!([-1]),
            json!([1.5]),
            json!([null]),
            json!(["0"]),
            json!((0..=MAX_INDICES as i64).collect::<Vec<i64>>()),
        ] {
            assert!(indices_arg(&bad).is_err(), "{bad}");
        }

        assert_eq!(
            match_arg(&json!({"output": "OnUser1"})).unwrap(),
            Map::<String, Value>::from_iter([("output".to_string(), json!("OnUser1"))])
        );
        assert_eq!(
            match_arg(&json!({"output": "o", "target": "t", "input": "i"}))
                .unwrap()
                .len(),
            3
        );
        for bad in [
            json!({}),
            json!("x"),
            json!({"targets": "door"}),
            json!({"output": ""}),
            json!({"output": 1}),
            json!({"output": "a\u{0}b"}),
        ] {
            let err = match_arg(&bad).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}");
        }
        let too_long = "x".repeat(MAX_FIELD_BYTES + 1);
        assert!(match_arg(&json!({"input": too_long})).is_err());
    }

    #[test]
    fn io_names_and_declarations_are_case_insensitive() {
        let names = vec!["Enable".to_string(), "Disable".to_string()];
        assert!(declares(&names, "enable"));
        assert!(declares(&names, "DISABLE"));
        assert!(!declares(&names, "Kill"));
        assert!(!declares(&[], "Enable"));
    }

    #[test]
    fn the_error_payloads_name_the_lists() {
        let valid = vec!["BuyzoneEntered".to_string()];
        let err = unknown_output(1, "func_buyzone", "Nope", &valid);
        assert_eq!(err.name, "UnknownOutput");
        assert_eq!(err.data["valid"], json!(["BuyzoneEntered"]));
        assert_eq!(err.data["index"], json!(1));
        assert!(err.retry_safe);
        assert!(err.message.contains("BuyzoneEntered"), "{}", err.message);

        let classes = vec![
            ("func_buyzone".to_string(), vec!["Enable".to_string()]),
            ("info_target".to_string(), Vec::new()),
        ];
        let err = unknown_input(0, "door", "Nope", &classes);
        assert_eq!(err.name, "UnknownInput");
        assert_eq!(err.data["classes"][0]["class"], json!("func_buyzone"));
        assert!(
            err.message.contains("func_buyzone: Enable"),
            "{}",
            err.message
        );
        assert!(
            err.message.contains("info_target: no inputs"),
            "{}",
            err.message
        );

        let err = static_prop_target(2, "wall", &[(9, "prop_static".to_string())]);
        assert_eq!(err.name, "StaticPropTarget");
        assert_eq!(err.data["staticNodes"][0]["nodeID"], json!(9));
        assert!(
            err.message.contains("force does not skip"),
            "{}",
            err.message
        );
    }

    #[test]
    fn a_zero_hit_match_maps_to_connection_not_found() {
        let error = BridgeError::new(Kind::ConnectionNotFound, "ConnectionNotFound")
            .with_code(-32026)
            .with_data(json!({"reason": "no connection matches the given fields"}));
        let tool = connection_not_found(&error, 30, &json!({"match": {"output": "OnUser1"}}))
            .expect("mapped");
        assert_eq!(tool.name, "ConnectionNotFound");
        assert_eq!(tool.code, Some(-32026));
        assert!(tool.retry_safe);
        assert_eq!(tool.data["nodeID"], json!(30));
        // a missing NODE stays the wire's NodeNotFound, never ConnectionNotFound
        let node = BridgeError::new(Kind::NodeNotFound, "NodeNotFound").with_code(-32022);
        assert!(connection_not_found(&node, 30, &json!({"indices": [0]})).is_none());
        // every other failure keeps the wire taxonomy
        let other = BridgeError::new(Kind::ProtocolError, "x").with_code(-32602);
        assert!(connection_not_found(&other, 30, &json!({"indices": [0]})).is_none());
    }
}
