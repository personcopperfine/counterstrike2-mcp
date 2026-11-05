//! Entity class schema from the game's FGD files (`crate::fgd`): `list_entity_classes` and
//! `get_entity_class` (keyvalues with types/defaults/choices/flags, inputs, outputs, bases).
//! Plan: target/entities-plan.md WP-C.
//!
//! Pure file route: reads `<project_root>/workshop/run/app/game` through `fgd::load_for(&state)`
//! (mtime-cached, state.fgd_root), declares no bridge methods (the registry never gates these
//! tools). Error mapping: [`FgdError::Unavailable`] -> `FgdUnavailable` naming the path (never an
//! empty class list), `Parse`/`IncludeCycle` -> `FgdParseError`, `Io` -> `FgdUnavailable` (the
//! FGD tree is not readable), an unknown class -> `UnknownClass {data.class, data.nearest}` with
//! the database's nearest-name list, a bad `kind`/`limit`/argument -> `InvalidArguments`.
use crate::fgd::{self, ClassKind, Database, FgdError};
use crate::state::HammerState;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};

const PLAN: &str = "target/entities-plan.md WP-C";

/// `list_entity_classes` limit bound and default (brief WP-C).
const LIMIT_MAX: u64 = 1000;
const LIMIT_DEFAULT: u64 = 200;
/// Class descriptions are truncated to this many characters in the list (brief WP-C).
const DESC_MAX: usize = 200;

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "list_entity_classes",
            "Entity classes from the game's FGD files (filter by substring/kind/tag). logic_auto, logic_eventlistener, func_movelinear carry a playNote on round-start timing.",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "description": "substring, case-insensitive"
                },
                "kind": {
                    "type": "string",
                    "enum": ["point", "solid", "base", "all"],
                    "description": "default: point + solid"
                },
                "tag": {
                    "type": "string",
                    "description": "FGD tags() helper argument, e.g. Sound, PropStatic, Logic"
                },
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": LIMIT_MAX,
                    "description": "max classes returned"
                }
            },
            "additionalProperties": false
        })),
        list_entity_classes,
    );
    r.register(
        ToolSpec::new(
            "get_entity_class",
            "One entity class from the FGD: resolved keyvalues (type, default, choices, flags), inputs, outputs, bases. playNote (round-start timing: what fires at load vs when play begins) present for logic_auto, logic_eventlistener, func_movelinear.",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "class": {
                    "type": "string",
                    "description": "case-insensitive"
                },
                "includeBases": {
                    "type": "boolean",
                    "description": "false = only what the class itself declares"
                }
            },
            "required": ["class"],
            "additionalProperties": false
        })),
        get_entity_class,
    );
}

// -------------------------------------------------------------------------------------------
// Shared plumbing: FGD load + error mapping, kind parsing
// -------------------------------------------------------------------------------------------

/// Load the FGD database for `state.fgd_root` and map [`FgdError`] onto the tool taxonomy.
async fn load_db(state: &HammerState) -> Result<std::sync::Arc<Database>, ToolError> {
    fgd::load_for(state).map_err(fgd_error)
}

/// The FGD file set could not be turned into a database: never an empty success list.
fn fgd_error(error: FgdError) -> ToolError {
    let unavailable = matches!(&error, FgdError::Unavailable { .. });
    match error {
        FgdError::Unavailable { path } | FgdError::Io { path, .. } => {
            let reason = if unavailable {
                "csgo/csgo.fgd not found"
            } else {
                "not readable"
            };
            ToolError::new(
                "FgdUnavailable",
                "FgdUnavailable",
                format!(
                    "the game's FGD tree is not available at {} ({reason}); source.root names the \
                     expected directory",
                    path.display()
                ),
            )
            .with_data(json!({"path": path.display().to_string()}))
            .retry_safe(true)
        }
        FgdError::Parse { file, line, reason } => ToolError::new(
            "FgdParseError",
            "FgdParseError",
            format!("{}:{line}: {reason}", file.display()),
        )
        .with_data(json!({"file": file.display().to_string(), "line": line}))
        .retry_safe(true),
        FgdError::IncludeCycle { chain } => ToolError::new(
            "FgdParseError",
            "FgdParseError",
            format!("@include cycle in the FGD tree: {chain}"),
        )
        .with_data(json!({"chain": chain}))
        .retry_safe(true),
        FgdError::NotImplemented => {
            ToolError::internal(format!("{PLAN}: the FGD database reported NotImplemented"))
        }
    }
}

/// `kind?: "point"|"solid"|"base"|"all"` -> the [`ClassKind`]s to keep; `None` = default
/// (point + solid). An empty vector means "all kinds" ([`Database::classes`]).
fn kinds_arg(args: &Value) -> Result<Vec<ClassKind>, ToolError> {
    let invalid = |value: &Value| {
        ToolError::invalid_arguments(format!(
            "kind must be one of \"point\", \"solid\", \"base\", \"all\", got {value}"
        ))
    };
    match args.get("kind") {
        None | Some(Value::Null) => Ok(vec![ClassKind::Point, ClassKind::Solid]),
        Some(Value::String(kind)) => match kind.as_str() {
            "point" => Ok(vec![ClassKind::Point]),
            "solid" => Ok(vec![ClassKind::Solid]),
            "base" => Ok(vec![ClassKind::Base]),
            "all" => Ok(Vec::new()),
            other => Err(invalid(&json!(other))),
        },
        Some(other) => Err(invalid(other)),
    }
}

/// `tag?: string` — the `tags( … )` helper argument to keep (case-insensitive). Absent/null means
/// no tag filter; an empty/blank tag is `InvalidArguments` (it names nothing and would match an
/// empty `tags()` argument).
fn tag_arg(args: &Value) -> Result<Option<&str>, ToolError> {
    match optional_str(args, "tag")? {
        Some(tag) if tag.trim().is_empty() => Err(ToolError::invalid_arguments(
            "tag must be a non-empty string (e.g. \"Sound\"): a blank tag names nothing",
        )),
        other => Ok(other),
    }
}

/// `limit?: integer` — [`LIMIT_DEFAULT`] when absent, 1..=[`LIMIT_MAX`] when given.
fn limit_arg(args: &Value) -> Result<usize, ToolError> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(LIMIT_DEFAULT as usize),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(limit) if (1..=LIMIT_MAX).contains(&limit) => Ok(limit as usize),
            _ => Err(ToolError::invalid_arguments(format!(
                "limit must be an integer in 1..={LIMIT_MAX}, got {n}"
            ))),
        },
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "limit must be an integer in 1..={LIMIT_MAX}, got {other}"
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

/// Optional boolean argument: `None` when absent or null, an error for any other type.
fn optional_bool(args: &Value, name: &str, default: bool) -> Result<bool, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "{name} must be a boolean, got {other}"
        ))),
    }
}

/// Round-start behaviour an FGD does not say, seen live (acceptance run 3, 2026-10-26; it cost two
/// compile cycles to find out). Attached as `playNote` to `get_entity_class` / `list_entity_classes`.
pub fn play_note(class: &str) -> Option<&'static str> {
    match class.to_ascii_lowercase().as_str() {
        "logic_auto" => Some("Seen live: OnMapSpawn fires when the map loads, during the freeze period, not at round start (a mover it opens has finished its run before the first step). OnMultiNewRound with Open(delay) did not move a func_movelinear in a loaded round or after mp_restartgame. For 'when play begins' use a logic_eventlistener on round_freeze_end."),
        "logic_eventlistener" => Some("Seen live: with EventName round_freeze_end it fires when the freeze period ends (= play begins) and its OnEventFired output moved a func_movelinear. Use it, not logic_auto, to start things at round start."),
        "func_movelinear" => Some("Seen live: opening it from logic_auto OnMapSpawn runs it during the freeze period; OnMultiNewRound ... Open(delay) did not move it. Start it from a logic_eventlistener on round_freeze_end (OnEventFired -> Open)."),
        _ => None,
    }
}

fn kind_str(kind: ClassKind) -> &'static str {
    match kind {
        ClassKind::Point => "point",
        ClassKind::Solid => "solid",
        ClassKind::Base => "base",
        ClassKind::Other => "other",
    }
}

fn description_json(description: Option<&str>, truncate: bool) -> Value {
    match description {
        None => Value::Null,
        Some(text) => {
            let text = if truncate && text.chars().count() > DESC_MAX {
                text.chars().take(DESC_MAX).collect::<String>()
            } else {
                text.to_string()
            };
            json!(text)
        }
    }
}

/// The `source` object both tools report: the FGD root plus the files read and non-fatal
/// parse warnings (`root`, `files`, `warnings`).
fn source_json(state: &HammerState, db: &Database) -> Value {
    json!({
        "root": state.fgd_root.display().to_string(),
        "files": db.files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(),
        "warnings": db.warnings,
    })
}

// -------------------------------------------------------------------------------------------
// list_entity_classes
// -------------------------------------------------------------------------------------------

/// Classes of the FGD database, name-sorted: `filter` is a case-insensitive substring of the
/// name, `kind` picks point/solid/base/all (default point+solid), `tag` keeps only classes whose
/// resolved `tags( … )` helper args carry that tag (`Database::has_tag`, so a subclass inherits a
/// base's tag), `limit` caps the list. The result carries `total` (before the limit), `truncated`
/// and `source:{root, files, warnings}`. A tag nothing carries is an empty list with `total: 0`,
/// never an error (the caller asked which classes have it; the answer is none).
pub async fn list_entity_classes(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let filter = optional_str(&args, "filter")?;
    let kinds = kinds_arg(&args)?;
    let tag = tag_arg(&args)?;
    let limit = limit_arg(&args)?;
    let db = load_db(&state).await?;

    // Full (sorted, filtered) list first: `total` is the count before the limit.
    let all: Vec<&fgd::Class> = db
        .classes(filter, &kinds, usize::MAX)
        .into_iter()
        .filter(|class| {
            tag.map(|tag| db.has_tag(&class.name, tag))
                .unwrap_or(true)
        })
        .collect();
    let total = all.len();
    let classes: Vec<Value> = all
        .into_iter()
        .take(limit)
        .map(|class| {
            let mut entry = json!({
                "name": class.name,
                "kind": kind_str(class.kind),
                "description": description_json(class.description.as_deref(), true),
                "bases": class.bases,
                "keyCount": class.keys.len(),
                "inputCount": class.inputs.len(),
                "outputCount": class.outputs.len(),
            });
            if let Some(note) = play_note(&class.name) {
                entry["playNote"] = json!(note);
            }
            entry
        })
        .collect();

    let out = json!({
        "classes": classes,
        "count": total.min(limit),
        "total": total,
        "truncated": total > limit,
        "source": source_json(&state, &db),
    });
    Ok(out.into())
}

// -------------------------------------------------------------------------------------------
// get_entity_class
// -------------------------------------------------------------------------------------------

/// One FGD class: direct bases (`bases`), the full chain (`resolvedBases`), resolved keyvalues
/// (base keys first, a child's same-named key overrides in place) with choices/flags and the
/// declaring class, inputs, outputs, header helpers and the `file:line` of the declaration.
/// `includeBases:false` keeps only items `declaredIn == class` (declared by the class itself).
pub async fn get_entity_class(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let class = match args.get("class") {
        Some(Value::String(text)) => text.clone(),
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "class must be a string, got {other}"
            )))
        }
        None => {
            return Err(ToolError::invalid_arguments(
                "class is required: the entity class name (e.g. info_target)",
            ))
        }
    };
    let include_bases = optional_bool(&args, "includeBases", true)?;
    let db = load_db(&state).await?;

    let def = db.class(&class).ok_or_else(|| unknown_class(&db, &class))?;
    let own = |declared_in: &str| include_bases || declared_in.eq_ignore_ascii_case(&def.name);

    let keyvalues: Vec<Value> = def
        .keys
        .iter()
        .filter(|k| own(&k.declared_in))
        .map(|k| {
            let mut entry = json!({
                "name": k.name,
                "type": k.type_,
                "displayName": k.display_name,
                "default": k.default,
                "description": k.description,
                "declaredIn": k.declared_in,
            });
            let obj = entry.as_object_mut().expect("json object");
            if !k.choices.is_empty() {
                obj.insert(
                    "choices".into(),
                    json!(k
                        .choices
                        .iter()
                        .map(|c| json!({"value": c.value, "label": c.label}))
                        .collect::<Vec<_>>()),
                );
            }
            if !k.flags.is_empty() {
                obj.insert(
                    "flags".into(),
                    json!(k
                        .flags
                        .iter()
                        .map(|f| json!({"bit": f.bit, "label": f.label, "defaultOn": f.default_on}))
                        .collect::<Vec<_>>()),
                );
            }
            entry
        })
        .collect();
    let inputs: Vec<Value> = def
        .inputs
        .iter()
        .filter(|io| own(&io.declared_in))
        .map(|io| json!({"name": io.name, "type": io.type_, "description": io.description, "declaredIn": io.declared_in}))
        .collect();
    let outputs: Vec<Value> = def
        .outputs
        .iter()
        .filter(|io| own(&io.declared_in))
        .map(|io| json!({"name": io.name, "type": io.type_, "description": io.description, "declaredIn": io.declared_in}))
        .collect();

    let mut helpers = Map::new();
    for (name, args) in &def.helpers {
        helpers.insert(name.clone(), json!(args));
    }

    let mut out = json!({
        "name": def.name,
        "kind": kind_str(def.kind),
        "description": def.description,
        "bases": def.bases,
        "resolvedBases": def.resolved_bases,
        "keyvalues": keyvalues,
        "inputs": inputs,
        "outputs": outputs,
        "helpers": helpers,
        "source": def.source,
    });
    if let Some(note) = play_note(&def.name) {
        out["playNote"] = json!(note);
    }
    Ok(out.into())
}

/// `UnknownClass {data.class, data.nearest}`: the database's nearest-name list (edit distance ≤ 3,
/// up to 5 names) via the same logic `validate` uses for unknown classes.
fn unknown_class(db: &Database, class: &str) -> ToolError {
    let nearest = db
        .validate(class, &Map::new())
        .nearest;
    ToolError::new(
        "UnknownClass",
        "UnknownClass",
        format!("no entity class named '{class}' in the FGD database"),
    )
    .with_data(json!({"class": class, "nearest": nearest}))
    .did_you_mean(nearest)
    .retry_safe(true)
}
