//! `list_props` (WP-P): prop entities of a map, from the SAVED `.vmap` (file route) or the LIVE
//! document (bridge m2.15 `entity.find` + `entity.get`).
//!
//! A prop is a `CMapEntity` whose FGD class carries a prop tag (`base.fgd`:
//! `tags( PropStatic )` on `prop_static`, `tags( PropDynamic )` on `prop_dynamic_base` — its
//! children inherit it through `base()` — `tags( PropPhysics )` on `BasePropPhysics`); the tag
//! lookup is [`crate::fgd::Database::prop_tag`] (the class first, then its resolved base chain).
//! Kind: `static` / `dynamic` / `physics`; `kind:"any"` (the default) lists all of them.
//!
//! `includeOtherModelEntities:true` adds two more groups:
//! - FGD classes that are not prop-tagged but declare a `model` keyvalue of type `studio`
//!   (Studiomodel's `model(studio)`; e.g. `texture_based_animatable`) — kind `other`;
//! - classes the FGD does not carry at all, whose entities carry a non-empty `model` keyvalue —
//!   kind `unknown` with `problems:["unknown class"]` (a stale/typoed class name must not
//!   vanish silently). An unknown class without a `model` keyvalue is not a model entity and
//!   stays out.
//!
//! Exactly one route runs per call: `source` defaults to `file` when `path` is given, `live`
//! when it is not; `source:"live"` with a `path` is refused (contradictory: the live read
//! addresses the active document, not a file). The specs declare no bridge methods — like the
//! file tools, a file read never needs the bridge; a bridge without the m2.15 reads answers
//! `-32011 NotImplemented` for the `entity.find` request itself.
//!
//! File route: the same `path` argument rules, `staleRead` envelope and error codes as
//! `list_entities` ([`crate::tools::entities_file`]); the file's entity keyvalues are DMX
//! strings already parsed, so everything is computed locally. The FGD database is required for
//! the classification on BOTH routes — a game root without `csgo/csgo.fgd` is `FgdUnavailable`,
//! never an empty success (brief: no silent fallbacks).
//!
//! Live route: ONE `entity.find` scan per call — `class`/`targetname` (the bridge's own exact
//! case-insensitive filters) are forwarded; the box `within` stays local (the bridge's `within`
//! is a sphere). The scan is bounded by the bridge's own fixed 2000-row `limit` and this tool's
//! [`SCAN_CAP`] examine window; candidates past either bound are NOT in the result and the
//! `notes` say so (no silent cut). Every included candidate is read with one `entity.get`
//! (its `keyValues` and per-node `angles`/`scales`); a failed read keeps the candidate as a prop
//! with a `problems:["live read failed: …"]` entry instead of inventing fields, and a
//! tool-budget exhaustion during the pass drops the unexamined remainder with a note. The m2.18
//! `entity.get` reports the node `scales` (the C++ field, `re-props.md` Q2), so live `scales` is
//! the real value; a bridge that does not report the field (older than m2.18) leaves `scales`
//! `null` **and** adds a note — never an invented `[1,1,1]` (WP-P; review W2 3c).
//!
//! Result: `{source, path?, file?, staleRead?, staleReason?, unsavedBridgeEdits? (file only),
//! total, returned, offset, truncated, summary:{byKind:{static,dynamic,physics,other,unknown},
//! byModel:[{model,count}] (top [`MODEL_TOP`] by count, then name)},
//! props:[{nodeID, classname, kind, model, skin, solid, targetname, origin, angles, scales,
//! problems, keyvalues? (includeKeyValues:true only)}], notes:[]}`. `total`/`summary` cover
//! every surviving prop; props keep the scan order (file element order / the bridge's walk
//! order) — stable. A prop whose `model` keyvalue is missing or empty gets
//! `problems:["no model"]` (Hammer shows an error box for those). Tests: `tests/props_query.rs`.
use crate::state::HammerState;
use crate::tools::entities_file::{resolve_document, source_json, stale_read};
use crate::vmap::Entity;
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// `limit` bound and default (WP-P).
const LIMIT_MAX: u64 = 2000;
const LIMIT_DEFAULT: u64 = 200;
/// The live scan reads at most this many candidates with one `entity.get` each (one bridge
/// request per prop: the pass stays bounded, and `notes` names the cut when more candidates
/// existed). The candidates themselves are additionally bounded by the bridge's own
/// `entity.find` page (2000 rows).
const SCAN_CAP: usize = 512;
/// `summary.byModel` holds at most this many entries (top by count, then name).
const MODEL_TOP: usize = 50;
/// The kinds a `kind` argument may ask for (`any` = no filter; `other`/`unknown` only match
/// when `includeOtherModelEntities` is true — the classification ignores those classes without
/// it).
const KIND_FILTERS: [&str; 6] = ["any", "static", "dynamic", "physics", "other", "unknown"];
/// Every prop kind reported in `summary.byKind`.
const KINDS: [&str; 5] = ["static", "dynamic", "physics", "other", "unknown"];

// The FGD load + taxonomy mapping is `live_common::fgd_or_error` (shared with
// `list_triggers`, review C6); re-exported under this module's name so the unit
// tests and callers keep the same path.
pub(crate) use super::live_common::fgd_or_error;

// -------------------------------------------------------------------------------------------
// Registration
// -------------------------------------------------------------------------------------------

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "list_props",
            "Prop entities of a map, from the saved .vmap (source file) or the live document (source live). Kind comes from FGD tags(Prop*).",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "enum": ["file", "live"],
                    "description": "default: file if `path` given, else live"
                },
                "path": {
                    "type": "string",
                    "description": "file source only; addon-relative or host path"
                },
                "kind": {
                    "type": "string",
                    "enum": KIND_FILTERS,
                    "description": "other/unknown need includeOtherModelEntities"
                },
                "class": {
                    "type": "string",
                    "minLength": 1,
                    "description": "case-insensitive"
                },
                "targetname": {
                    "type": "string",
                    "minLength": 1,
                    "description": "case-insensitive"
                },
                "model": {
                    "type": "string",
                    "minLength": 1,
                    "description": "case-insensitive substring of model"
                },
                "within": {
                    "type": "object",
                    "description": "axis-aligned box on the node origin; max >= min on every axis",
                    "properties": {
                        "min": {
                            "type": "array", "items": {"type": "number"},
                            "minItems": 3, "maxItems": 3
                        },
                        "max": {
                            "type": "array", "items": {"type": "number"},
                            "minItems": 3, "maxItems": 3
                        }
                    },
                    "required": ["min", "max"],
                    "additionalProperties": false
                },
                "includeOtherModelEntities": {
                    "type": "boolean",
                    "description": "include non-prop classes with a model keyvalue (kind other/unknown)"
                },
                "includeKeyValues": {
                    "type": "boolean",
                    "description": "include keyvalues"
                },
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": LIMIT_MAX,
                    "description": "max props returned"
                },
                "offset": {
                    "type": "integer", "minimum": 0,
                    "description": "skip this many matches"
                }
            },
            "additionalProperties": false
        })),
        list_props,
    );
}

// -------------------------------------------------------------------------------------------
// Arguments (every one validated locally first — a bad call never reaches a route)
// -------------------------------------------------------------------------------------------

/// Accepted argument names. The schema says `additionalProperties:false` for the client, but the
/// handler is the strict validator (the registry does not enforce the schema).
const KNOWN_ARGS: [&str; 11] = [
    "source",
    "path",
    "kind",
    "class",
    "targetname",
    "model",
    "within",
    "includeOtherModelEntities",
    "includeKeyValues",
    "limit",
    "offset",
];

/// `limit?` in 1..=2000, default [`LIMIT_DEFAULT`].
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

/// `offset?` >= 0 (default 0).
fn offset_arg(args: &Value) -> Result<u64, ToolError> {
    match args.get("offset") {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
            ToolError::invalid_arguments(format!("offset must be a non-negative integer, got {n}"))
        }),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "offset must be a non-negative integer, got {other}"
        ))),
    }
}

/// A boolean flag argument (default false).
fn bool_arg(args: &Value, name: &str) -> Result<bool, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "{name} must be a boolean, got {other}"
        ))),
    }
}

/// An optional string filter; a present-but-empty string is refused (an empty class/targetname/
/// model filter would match everything while pretending to filter — the entities-file rule).
fn filter_str<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.is_empty() => Ok(Some(text.as_str())),
        Some(Value::String(_)) => Err(ToolError::invalid_arguments(format!(
            "{name} is empty: an empty filter matches everything; omit {name} instead"
        ))),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "{name} must be a string, got {other}"
        ))),
    }
}

/// `kind?`: `None` (= any) or one of [`KIND_FILTERS`]; `any` is stored as `None`.
fn kind_arg(args: &Value) -> Result<Option<String>, ToolError> {
    match args.get("kind") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            if !KIND_FILTERS.contains(&text.as_str()) {
                return Err(ToolError::invalid_arguments(format!(
                    "kind must be one of {}, got {text:?}",
                    KIND_FILTERS.join(", ")
                )));
            }
            Ok((text != "any").then(|| text.to_string()))
        }
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "kind must be one of {}, got {other}",
            KIND_FILTERS.join(", ")
        ))),
    }
}

/// `source?`: `None` when absent, `Some("file"|"live")` when given, else `InvalidArguments`.
fn source_arg(args: &Value) -> Result<Option<&'static str>, ToolError> {
    match args.get("source") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text == "file" || text == "live" => {
            Ok(Some(if text == "live" { "live" } else { "file" }))
        }
        Some(Value::String(other)) => Err(ToolError::invalid_arguments(format!(
            "source must be \"file\" or \"live\", got {other:?}"
        ))),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "source must be \"file\" or \"live\", got {other}"
        ))),
    }
}

/// The route `list_props` reads one document through. A two-variant enum instead of a
/// `&'static str`: the dispatch cannot silently grow a third spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// The SAVED `.vmap` file (`path` names it or the bridge's active document).
    File,
    /// The LIVE document through the bridge.
    Live,
}

/// `path?`: an optional string (the file-route argument). A present non-string is refused in
/// the argument phase — treating it as absent would silently reroute the call (`path:123` to
/// `live`, `source:"file"` + `path:123` to the active document) and would bypass the
/// `source:"live"`+`path` contradiction check.
fn path_arg(args: &Value) -> Result<Option<&str>, ToolError> {
    match args.get("path") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "path must be a string, got {other}"
        ))),
    }
}

/// The effective route: an explicit `source` wins (and `source:"live"` with a `path` is a
/// contradiction); the default is `file` when `path` is given, `live` when it is not. `path`
/// comes pre-validated as an optional string ([`path_arg`]).
fn route_of(args: &Value, path: Option<&str>) -> Result<Route, ToolError> {
    match source_arg(args)? {
        Some("live") if path.is_some() => Err(ToolError::invalid_arguments(
            "path is a file-route argument: omit it to read the live document, or ask for \
                 source \"file\"",
        )),
        Some(route) => Ok(if route == "live" {
            Route::Live
        } else {
            Route::File
        }),
        None => Ok(if path.is_some() {
            Route::File
        } else {
            Route::Live
        }),
    }
}

/// A parsed box: `(min corner, max corner)`.
type Box3 = ([f64; 3], [f64; 3]);

/// `within?: {min:[x,y,z], max:[x,y,z]}` — an axis-aligned box on the node origin; `max >= min`
/// on every axis. Applied locally for both routes (the bridge's `within` is a sphere, so the
/// live route never forwards it and needs no coordinate-form checks: the numbers are only
/// compared, never serialized).
fn within_box(args: &Value) -> Result<Option<Box3>, ToolError> {
    let Some(within) = args.get("within").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let invalid = |msg: String| ToolError::invalid_arguments(format!("within: {msg}"));
    if !within.is_object() {
        return Err(invalid(format!(
            "an object {{min, max}} is required, got {within}"
        )));
    }
    for name in within.as_object().expect("checked object").keys() {
        if name != "min" && name != "max" {
            return Err(invalid(format!(
                "unknown argument {name:?} (within takes only min and max)"
            )));
        }
    }
    let mut corners: [Option<[f64; 3]>; 2] = [None, None];
    for (slot, name) in [(0usize, "min"), (1usize, "max")] {
        let values = within
            .get(name)
            .and_then(Value::as_array)
            .filter(|values| values.len() == 3)
            .ok_or_else(|| invalid(format!("{name} must be [x, y, z]")))?;
        let mut point = [0.0f64; 3];
        for (axis, value) in values.iter().enumerate() {
            point[axis] = value.as_f64().filter(|v| v.is_finite()).ok_or_else(|| {
                invalid(format!(
                    "{name}[{axis}] must be a finite number, got {value}"
                ))
            })?;
        }
        corners[slot] = Some(point);
    }
    let (min, max) = (
        corners[0].expect("min parsed above"),
        corners[1].expect("max parsed above"),
    );
    for axis in 0..3 {
        if max[axis] < min[axis] {
            return Err(invalid(format!(
                "max[{axis}] ({}) is below min[{axis}] ({}): max must be >= min on every axis",
                max[axis], min[axis]
            )));
        }
    }
    Ok(Some((min, max)))
}

/// An unknown argument: `InvalidArguments` before any work happens.
fn check_known_args(args: &Value) -> Result<(), ToolError> {
    let Some(map) = args.as_object() else {
        return Err(ToolError::invalid_arguments("arguments must be an object"));
    };
    for name in map.keys() {
        if !KNOWN_ARGS.contains(&name.as_str()) {
            return Err(ToolError::invalid_arguments(format!(
                "unknown argument {name:?} (accepted: {})",
                KNOWN_ARGS.join(", ")
            )));
        }
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------
// Candidate gathering and classification
// -------------------------------------------------------------------------------------------

/// Why an entity is (or is not) part of the prop list.
#[derive(Debug, Clone)]
enum Inclusion {
    /// A prop-tagged FGD class — always a prop.
    Prop(&'static str),
    /// A class that declares `model(studio)` but carries no prop tag (includeOther only).
    Other,
    /// A class the FGD does not carry at all. Confirmed only when the entity's keyvalues carry a
    /// non-empty `model` value (includeOther only).
    Unknown,
}

/// Classify one entity's classname against the FGD database under `includeOther`.
fn classify(db: &crate::fgd::Database, classname: &str, include_other: bool) -> Option<Inclusion> {
    if let Some(tag) = db.prop_tag(classname) {
        return Some(Inclusion::Prop(match tag {
            crate::fgd::PropTag::Static => "static",
            crate::fgd::PropTag::Dynamic => "dynamic",
            crate::fgd::PropTag::Physics => "physics",
        }));
    }
    if !include_other {
        return None;
    }
    if db.class(classname).is_none() {
        return Some(Inclusion::Unknown);
    }
    db.declares_model_studio(classname)
        .then_some(Inclusion::Other)
}

/// One scanned entity plus what the examine pass knows about it.
struct Candidate {
    entity: Entity,
    inclusion: Inclusion,
    /// File route: the entity's keyvalues (DMX strings); live route: empty until examined.
    file_keyvalues: Vec<(String, Value)>,
    /// Live route: the `entity.get` reply's keyValues, once read.
    live_keyvalues: Option<Map<String, Value>>,
    /// Live route: the `entity.get` reply's angles (null on the wire when unreadable).
    live_angles: Option<Value>,
    /// Live route: the `entity.get` reply's `scales` — the node field, reported since m2.18. An
    /// older bridge omits the member entirely (`None`), which is what the "no scales" note is for
    /// (review W2 3c); a reply that carries `null` is a failed read of the field, also `None`.
    live_scales: Option<Value>,
    /// Live route: failure of the per-prop read (a `problems` entry, never a silent skip).
    fetch_error: Option<String>,
}

impl Candidate {
    /// `(model, skin, solid)`: the first case-insensitive match per name over the keyvalues.
    /// An empty string does not count (an empty model IS the "no model" problem).
    fn named_keyvalues(&self) -> [Option<Value>; 3] {
        let names = ["model", "skin", "solid"];
        let mut out: [Option<Value>; 3] = [None, None, None];
        let live: Vec<(String, Value)> = match &self.live_keyvalues {
            Some(map) => map
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            None => Vec::new(),
        };
        for (name, value) in self.file_keyvalues.iter().chain(live.iter()) {
            for (i, want) in names.iter().enumerate() {
                if out[i].is_none()
                    && name.eq_ignore_ascii_case(want)
                    && !value.as_str().is_some_and(str::is_empty)
                {
                    out[i] = Some(value.clone());
                }
            }
        }
        out
    }

    /// The entity's keyvalues as a JSON object (live: the reply's keyValues; file: the artifact).
    fn keyvalues_object(&self) -> Value {
        self.live_keyvalues
            .as_ref()
            .map(|map| Value::Object(map.clone()))
            .unwrap_or_else(|| {
                let mut object = Map::new();
                for (name, value) in &self.file_keyvalues {
                    object.insert(name.clone(), value.clone());
                }
                Value::Object(object)
            })
    }
}

/// The file-route candidates: every entity of the SAVED `.vmap` whose element class is
/// `CMapEntity`, in element order, each classified against the FGD database. Resolution and the
/// error codes are [`resolve_document`]'s (the same ones `list_entities` reports).
async fn candidates_file(
    state: &HammerState,
    ctx: &CallCtx,
    db: &Arc<crate::fgd::Database>,
    include_other: bool,
    args: &Value,
) -> Result<(Vec<Candidate>, Map<String, Value>), ToolError> {
    let (host, document, is_active, dirty) = resolve_document(state, ctx, args).await?;
    let candidates: Vec<Candidate> = document
        .entities()
        .iter()
        .filter(|entity| entity.node_class == "CMapEntity")
        .filter_map(|entity| {
            let inclusion = classify(db.as_ref(), &entity.classname, include_other)?;
            Some(Candidate {
                entity: entity.clone(),
                inclusion,
                file_keyvalues: entity.keyvalues.clone(),
                live_keyvalues: None,
                live_angles: None,
                live_scales: None,
                fetch_error: None,
            })
        })
        .collect();
    let (stale, reason) = stale_read(state, is_active, dirty);
    let mut envelope = Map::new();
    envelope.insert("source".into(), json!("file"));
    let input = args.get("path").and_then(Value::as_str);
    envelope.insert(
        "path".into(),
        input
            .map(Value::from)
            .unwrap_or_else(|| json!(host.display().to_string())),
    );
    envelope.insert("file".into(), source_json(&host, &document));
    envelope.insert("staleRead".into(), json!(stale));
    envelope.insert(
        "staleReason".into(),
        reason.map(Value::from).unwrap_or(Value::Null),
    );
    envelope.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    Ok((candidates, envelope))
}

/// The caller's live-scan filters: `class`/`targetname` are forwarded to the bridge's own
/// (exact, case-insensitive) filters; `kind` and `within` are applied ON the `entity.find`
/// rows (both are decidable there — the classname carries the kind, the origin rides the
/// row), before the [`SCAN_CAP`] window fills and before `inclusive_seen` counts them. The
/// `model` filter (needs the per-prop `entity.get` keyvalues) stays in the post-pass.
struct ScanFilters<'a> {
    class: Option<&'a str>,
    targetname: Option<&'a str>,
    kind: Option<&'a str>,
    within: Option<&'a Box3>,
}

/// The live-route candidates (bridge m2.15 `entity.find`): one scan, `class`/`targetname`
/// forwarded, at most 2000 rows (the bridge's own cap). Returns the classified candidates only
/// (non-prop entities are dropped), but never more than [`SCAN_CAP`] of them: `notes` says when
/// candidates were cut. Candidates the filters dropped never consume a window slot (nor a
/// bridge `entity.get`).
async fn candidates_live(
    state: &HammerState,
    ctx: &CallCtx,
    db: &Arc<crate::fgd::Database>,
    filters: &ScanFilters<'_>,
    include_other: bool,
) -> Result<(Vec<Candidate>, bool, usize), ToolError> {
    let ScanFilters {
        class,
        targetname,
        kind: kind_filter,
        within,
    } = *filters;
    let mut params = Map::new();
    if let Some(class) = class {
        params.insert("class".into(), json!(class));
    }
    if let Some(targetname) = targetname {
        params.insert("targetname".into(), json!(targetname));
    }
    // Scan at the bridge's own full page width (its default is 500); the classification here
    // decides what is a prop, so nothing narrower is right.
    params.insert("limit".into(), json!(2000));
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
    let Some(rows) = reply.result.get("entities").and_then(Value::as_array) else {
        return Err(ToolError::internal(format!(
            "entity.find returned a malformed result: {} (bridge/protocol-diff-m2.15.md §7)",
            reply.result
        )));
    };
    let bridge_truncated = reply
        .result
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut inclusive_seen = 0usize;
    for row in rows {
        let entity = Entity {
            guid: String::new(),
            node_class: "CMapEntity".into(),
            node_id: row.get("nodeID").and_then(Value::as_i64),
            reference_id: None,
            classname: row
                .get("classname")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            targetname: row
                .get("targetname")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_string),
            origin: wire_vec3(row.get("origin")),
            angles: None,
            scales: None,
            keyvalues: Vec::new(),
            connections: Vec::new(),
        };
        // Only candidates that classify as props are included, and only ones the caller's
        // row-decidable filters keep; they fill at most SCAN_CAP slots (the post-pass filter
        // re-checks everything for identical semantics on both routes).
        let Some(inclusion) = classify(db.as_ref(), &entity.classname, include_other) else {
            continue;
        };
        let kind = match &inclusion {
            Inclusion::Prop(kind) => *kind,
            Inclusion::Other => "other",
            Inclusion::Unknown => "unknown",
        };
        if let Some(want) = kind_filter {
            if kind != want {
                continue;
            }
        }
        if let Some((min, max)) = within {
            match entity.origin {
                // A row without a usable origin could never sit inside the box (the post-pass
                // would drop it): the window is not spent on it.
                Some(origin) => {
                    if !(0..3).all(|axis| origin[axis] >= min[axis] && origin[axis] <= max[axis]) {
                        continue;
                    }
                }
                None => continue,
            }
        }
        inclusive_seen += 1;
        if candidates.len() >= SCAN_CAP {
            continue;
        }
        candidates.push(Candidate {
            entity,
            inclusion,
            file_keyvalues: Vec::new(),
            live_keyvalues: None,
            live_angles: None,
            live_scales: None,
            fetch_error: None,
        });
    }
    Ok((candidates, bridge_truncated, inclusive_seen))
}

/// The live examine pass: one `entity.get` per candidate for its keyvalues, angles and scales. A
/// failed read is kept as a `fetch_error` problems entry; a tool-budget exhaustion DROPS the
/// unexamined remainder with a note (their unknown detail must not read as "no model").
///
/// Returns how many examined candidates got **no** usable `scales` field, which the caller turns
/// into one note: m2.18's `entity.get` reports the node scales (review W2 3c), and a reply that
/// lacks a usable value is an older bridge (or an unreadable field) — `scales` stays `null` there
/// instead of an invented `[1,1,1]`.
async fn examine_live(
    state: &HammerState,
    ctx: &CallCtx,
    candidates: &mut Vec<Candidate>,
    notes: &mut Vec<String>,
) -> usize {
    let mut missing_scales = 0usize;
    for index in 0..candidates.len() {
        let deadline = match ctx.budget.deadline_ms() {
            Ok(deadline) => deadline,
            Err(_) => {
                notes.push(format!(
                    "the tool budget ran out after {index} live reads: candidates past that are \
                     not in this result"
                ));
                candidates.truncate(index);
                return missing_scales;
            }
        };
        let node_id = candidates[index].entity.node_id;
        let Some(node_id) = node_id.filter(|id| (0..=i64::from(i32::MAX)).contains(id)) else {
            candidates[index].fetch_error = Some("no usable node id in the live scan row".into());
            continue;
        };
        let mut bridge = state.bridge.lock().await;
        let reply = bridge
            .request(
                "entity.get",
                json!({ "nodeID": node_id }),
                RequestOpts::deadline(deadline),
            )
            .await;
        drop(bridge);
        match reply {
            Ok(reply) => {
                let result = &reply.result;
                let field = result.get("scales");
                // a wire `null` (or a malformed array) is the bridge's "the field did not read":
                // not a value, so `scales` stays null and the note counts it; a reply with no
                // member at all is an older bridge, counted the same way
                if field.and_then(Value::as_array).map(Vec::len) != Some(3) {
                    missing_scales += 1;
                }
                candidates[index].live_scales = field.cloned().filter(|value| {
                    value
                        .as_array()
                        .is_some_and(|axes| axes.len() == 3 && axes.iter().all(Value::is_number))
                });
                candidates[index].live_keyvalues =
                    result.get("keyValues").and_then(Value::as_object).cloned();
                candidates[index].live_angles = result.get("angles").cloned();
            }
            Err(error) => {
                // a failed read is both the keyvalue problem and a missing scales source: count it
                // so the scales note names it too (never a silent null)
                missing_scales += 1;
                candidates[index].fetch_error = Some(error.to_string());
            }
        }
    }
    missing_scales
}

/// A three-number wire array (`[x, y, z]`), else `None` — honest, never an invented vector.
fn wire_vec3(value: Option<&Value>) -> Option<[f64; 3]> {
    let values = value?.as_array()?;
    if values.len() != 3 {
        return None;
    }
    Some([
        values[0].as_f64()?,
        values[1].as_f64()?,
        values[2].as_f64()?,
    ])
}

// -------------------------------------------------------------------------------------------
// Prop records
// -------------------------------------------------------------------------------------------

/// One finished prop record; JSON is rendered at the page boundary.
struct Prop {
    candidate: Candidate,
    kind: &'static str,
    model: Option<Value>,
    skin: Option<Value>,
    solid: Option<Value>,
    angles: Option<Value>,
    scales: Option<Value>,
    origin: Option<[f64; 3]>,
    problems: Vec<String>,
}

impl Prop {
    fn new(mut candidate: Candidate) -> Prop {
        let [model, skin, solid] = candidate.named_keyvalues();
        let kind = match candidate.inclusion {
            Inclusion::Prop(kind) => kind,
            Inclusion::Other => "other",
            Inclusion::Unknown => "unknown",
        };
        let mut problems: Vec<String> = Vec::new();
        if matches!(candidate.inclusion, Inclusion::Unknown) {
            problems.push("unknown class".to_string());
        }
        if model.is_none() {
            problems.push("no model".to_string());
        }
        if candidate.entity.node_id.is_none() {
            problems.push("no node id".to_string());
        }
        if let Some(error) = &candidate.fetch_error {
            problems.push(format!("live read failed: {error}"));
        }
        // The live route carries the bridge's angles and scales (null when the bridge does not
        // report them: review W2 3c); the file route the node's own attributes.
        let angles = match candidate.live_angles.take() {
            Some(angles) => Some(angles),
            None => candidate
                .entity
                .angles
                .map(|angles| json!([angles[0], angles[1], angles[2]])),
        };
        let scales = match candidate.live_scales.take() {
            Some(scales) => Some(scales),
            None => candidate
                .entity
                .scales
                .map(|scales| json!([scales[0], scales[1], scales[2]])),
        };
        Prop {
            kind,
            model,
            skin,
            solid,
            origin: candidate.entity.origin,
            angles,
            scales,
            problems,
            candidate,
        }
    }

    fn to_json(&self, include_keyvalues: bool) -> Value {
        let mut map = Map::new();
        map.insert(
            "nodeID".into(),
            self.candidate
                .entity
                .node_id
                .map(Value::from)
                .unwrap_or(Value::Null),
        );
        map.insert("classname".into(), json!(self.candidate.entity.classname));
        map.insert("kind".into(), json!(self.kind));
        map.insert("model".into(), self.model.clone().unwrap_or(Value::Null));
        map.insert("skin".into(), self.skin.clone().unwrap_or(Value::Null));
        map.insert("solid".into(), self.solid.clone().unwrap_or(Value::Null));
        map.insert(
            "targetname".into(),
            self.candidate
                .targetname_json_name()
                .map(Value::from)
                .unwrap_or(Value::Null),
        );
        map.insert(
            "origin".into(),
            self.origin
                .map(|origin| json!([origin[0], origin[1], origin[2]]))
                .unwrap_or(Value::Null),
        );
        map.insert("angles".into(), self.angles.clone().unwrap_or(Value::Null));
        map.insert("scales".into(), self.scales.clone().unwrap_or(Value::Null));
        map.insert(
            "problems".into(),
            Value::Array(self.problems.iter().map(|problem| json!(problem)).collect()),
        );
        if include_keyvalues {
            map.insert("keyvalues".into(), self.candidate.keyvalues_object());
        }
        Value::Object(map)
    }
}

// -------------------------------------------------------------------------------------------
// list_props
// -------------------------------------------------------------------------------------------

/// The tool's entry point (see the module header for the whole result shape).
pub async fn list_props(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    check_known_args(&args)?;
    let limit = limit_arg(&args)?;
    let offset = offset_arg(&args)?;
    let kind_filter = kind_arg(&args)?;
    let class = filter_str(&args, "class")?;
    let targetname = filter_str(&args, "targetname")?;
    let model_filter = filter_str(&args, "model")?;
    let within = within_box(&args)?;
    let include_other = bool_arg(&args, "includeOtherModelEntities")?;
    let include_keyvalues = bool_arg(&args, "includeKeyValues")?;
    let path = path_arg(&args)?;
    let route = route_of(&args, path)?;

    // The classification IS the prop definition: a game root without csgo/csgo.fgd cannot
    // silently mean "no props".
    let db = fgd_or_error(&state)?;

    let mut notes: Vec<String> = Vec::new();
    let (envelope, mut candidates) = match route {
        Route::File => {
            let (candidates, envelope) =
                candidates_file(&state, &ctx, &db, include_other, &args).await?;
            (envelope, candidates)
        }
        Route::Live => {
            let (candidates, bridge_truncated, inclusive_seen) = candidates_live(
                &state,
                &ctx,
                &db,
                &ScanFilters {
                    class,
                    targetname,
                    kind: kind_filter.as_deref(),
                    within: within.as_ref(),
                },
                include_other,
            )
            .await?;
            if bridge_truncated {
                notes.push(
                    "the live scan reads the bridge's first 2000 entities (entity.find has no \
                     offset): entities past that page are not in this result"
                        .to_string(),
                );
            }
            if inclusive_seen > SCAN_CAP {
                notes.push(format!(
                    "the live scan examined only the first {SCAN_CAP} of {inclusive_seen} \
                     candidate props (one live read each): the rest are not in this result"
                ));
            }
            let mut envelope = Map::new();
            envelope.insert("source".into(), json!("live"));
            (envelope, candidates)
        }
    };

    // Live: read per-prop keyvalues (model/skin/solid and angles/scales) BEFORE the filters: the
    // model filter and the "no model" problem need them.
    if route == Route::Live {
        let missing_scales = examine_live(&state, &ctx, &mut candidates, &mut notes).await;
        if missing_scales > 0 {
            // m2.18's entity.get reports the node scales (re-props.md Q2); a reply without a usable
            // value is an OLDER bridge (or a field that did not read): those props keep `null`
            // scales, never an invented [1,1,1] (WP-P; review W2 3c)
            notes.push(format!(
                "{missing_scales} live read(s) carried no usable `scales`: the bridge is older \
                 than m2.18 (or the node field did not read), so `scales` is null for those props"
            ));
        }
    }

    // Unknown classes are confirmed only by their own keyvalues: a stale/typoed class name that
    // turns out not to be a model entity is not a prop.
    candidates.retain(|candidate| match &candidate.inclusion {
        Inclusion::Unknown => candidate.named_keyvalues()[0].is_some(),
        _ => true,
    });

    let mut props: Vec<Prop> = candidates.into_iter().map(Prop::new).collect();

    // Record-level filters (the bridge already did class/targetname live, but equal semantics
    // here keep both routes identical).
    props.retain(|prop| {
        kind_filter
            .as_deref()
            .map(|kind| prop.kind == kind)
            .unwrap_or(true)
            && class
                .map(|class| prop.candidate.entity.classname.eq_ignore_ascii_case(class))
                .unwrap_or(true)
            && targetname
                .map(|name| {
                    prop.candidate
                        .targetname_json_name()
                        .is_some_and(|value| value.eq_ignore_ascii_case(name))
                })
                .unwrap_or(true)
            && model_filter
                .map(|needle| {
                    prop.model
                        .as_ref()
                        .and_then(Value::as_str)
                        .is_some_and(|model| model.to_lowercase().contains(&needle.to_lowercase()))
                })
                .unwrap_or(true)
            && within
                .map(|(min, max)| {
                    prop.origin.is_some_and(|origin| {
                        (0..3).all(|axis| origin[axis] >= min[axis] && origin[axis] <= max[axis])
                    })
                })
                .unwrap_or(true)
    });

    // Totals + summary cover every surviving prop, the page only the slice.
    let total = props.len() as u64;
    let mut by_kind: Map<String, Value> = KINDS
        .iter()
        .map(|kind| (kind.to_string(), json!(0u64)))
        .collect();
    // byModel groups case-insensitively (model paths are case-insensitive text on this tree —
    // the path map's rule) and reports the first-seen spelling (scan order); top by count, then
    // name.
    let mut model_counts: std::collections::BTreeMap<String, (String, u64)> = Default::default();
    for prop in &props {
        if let Some(kind) = by_kind.get_mut(prop.kind) {
            *kind = json!(kind.as_u64().unwrap_or(0) + 1);
        }
        if let Some(model) = prop.model.as_ref().and_then(Value::as_str) {
            model_counts
                .entry(model.to_ascii_lowercase())
                .or_insert_with(|| (model.to_string(), 0u64))
                .1 += 1;
        }
    }
    let mut pairs: Vec<(String, u64)> = model_counts.into_values().collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    pairs.truncate(MODEL_TOP);
    let by_model_json: Vec<Value> = pairs
        .into_iter()
        .map(|(model, count)| json!({ "model": model, "count": count }))
        .collect();

    let skip = (offset.min(total)) as usize;
    let page: Vec<Prop> = props.into_iter().skip(skip).take(limit).collect();
    let returned = page.len() as u64;
    let truncated = offset + returned < total;

    let mut out = envelope;
    out.insert("total".into(), json!(total));
    out.insert("returned".into(), json!(returned));
    out.insert("offset".into(), json!(offset));
    out.insert("truncated".into(), json!(truncated));
    out.insert(
        "summary".into(),
        json!({ "byKind": Value::Object(by_kind), "byModel": by_model_json }),
    );
    out.insert(
        "props".into(),
        Value::Array(
            page.iter()
                .map(|prop| prop.to_json(include_keyvalues))
                .collect(),
        ),
    );
    out.insert("notes".into(), json!(notes));
    Ok(Value::Object(out).into())
}

impl Candidate {
    /// The targetname as it lands in a prop record (`null` when the entity is unnamed, and the
    /// string case known on both routes).
    fn targetname_json_name(&self) -> Option<&str> {
        self.entity
            .targetname
            .as_deref()
            .filter(|name| !name.is_empty())
            .or_else(|| {
                self.file_keyvalues
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("targetname"))
                    .and_then(|(_, value)| value.as_str())
            })
            .filter(|name| !name.is_empty())
    }
}
