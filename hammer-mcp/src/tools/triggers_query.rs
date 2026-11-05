//! `list_triggers` (WP-L): the brush entities of a map — the trigger volumes and the other
//! `@SolidClass` brushes — from the SAVED `.vmap` (file route) or the LIVE document (bridge
//! m2.15 `entity.find` + one `entity.get` per candidate).
//!
//! A trigger is a `CMapEntity` whose FGD class is a `@SolidClass` that resolves to a trigger:
//! its resolved base chain carries a base named `Trigger`/`TriggerOnce`, or a `tags( … )`
//! helper on the class or its chain carries the `Trigger` tag (plan-triggers.md §1) —
//! [`crate::fgd::Database::is_trigger_class`] owns that definition (WP-F's merged API; the
//! Solid-kind gate stays here, review B4: `is_trigger_class` is true for the `@BaseClass`
//! `Trigger`/`TriggerOnce` themselves and for a `tags(Trigger)` class of any kind). `kind`
//! (default `trigger`) selects:
//!
//! * `trigger` — only trigger-like Solid classes (`trigger_once`, `trigger_push`, …);
//! * `solid` — every `@SolidClass` entity (`func_buyzone`, `func_brush`, …) **plus** the
//!   entities whose class the FGD does not carry at all. An unknown class cannot prove it is a
//!   brush entity, so it is confirmed only by its own `CMapMesh` children (the plan's brush
//!   definition: the FGD kind *and* the mesh children) and listed with
//!   `problems:["unknown class"]` + a note — one without mesh children stays out (the
//!   `list_props` "unknown class" rule, mirrored).
//!
//! A Solid-class entity without mesh children is listed with `problems:["no mesh children"]`
//! and an empty `meshes` list; an entity whose `spawnflags` keyvalue does not parse as an
//! integer gets `problems:["spawnflags not an integer"]` (the record keeps the raw text, value
//! `null`) — never a crash.
//!
//! File route: the same `path` argument rules, `staleRead` envelope and error codes as
//! `list_entities` ([`crate::tools::entities_file`]); the volumes are computed locally from
//! [`crate::vmap::Document::children_of`] (new, WP-L) + [`crate::vmap::Document::mesh_by_id`] —
//! the union of the mesh children's WORLD-space bounds
//! ([`crate::vmap::mesh::MeshRecord::bounds`]). A mesh child the mesh view could not extract
//! (no position stream, a broken walk), one without a node id, or one whose `children`-chain is
//! not a proven container ([`crate::vmap::mesh::MeshRecord::parent_problem`]) is named in
//! `problems` and left out of the union — never a silent shrink of the volume.
//!
//! Live route: ONE `entity.find` scan per call (class/targetname forwarded; at most the
//! bridge's own 2000-row page), then one `entity.get` per candidate bounded by the same
//! [`SCAN_CAP`] examine window `list_props` uses. The volume needs the m2.19
//! `entity.get.children`, which no shipped bridge had before m2.19: when the reply carries
//! `children` the mesh nodeIDs are listed and the volume is computed from the live
//! `mesh.get` reads ([`crate::tools::mesh_read`], one per mesh child, when the bridge advertises
//! it) or the SAVED file (only while the document is
//! clean — `dirty:false` REPORTED; an unreported dirty state is not clean, review C3 — the
//! volume's `geometrySource` says which), else `volume` stays `null` **with a note** to
//! save_map and read the file route. When the reply carries no `children` at all (an m2.18
//! bridge), `meshes` and `volume` stay `null` and a note says so — never an invented
//! empty list. A `childrenTruncated:true` reply keeps the meshes it listed but refuses the
//! volume (a partial union would read as a shrunken box, review C4).
//!
//! `within` is computed locally on BOTH routes (the bridge's `within` is a sphere) AFTER the
//! per-candidate reads, because the volume decides. `withinMode` (default `"contains"`)
//! picks the rule: `"contains"` = the trigger's volume bounds must lie entirely inside the
//! box; `"intersects"` = the volume must overlap the box (a trigger straddling the edge
//! matches). Either way a candidate without a volume falls back to its ORIGIN (the entity
//! origin = the first mesh's pivot, which can lie outside a box the volume overlaps — the
//! result keeps to what the route could see). A candidate without a volume and without an
//! origin can never match, so the live window may be spent on rows the filter later drops
//! — the caps and their notes stay honest.
//!
//! The FGD is required on both routes (`FgdUnavailable`, never an empty success — the
//! classification IS the definition). Result: `{source, path?, file?, staleRead?, staleReason?,
//! unsavedBridgeEdits? (file only), total, returned, offset, truncated, summary:{byClass},
//! triggers:[{nodeID, classname, targetname, origin, angles, meshes, volume, spawnflags,
//! connections, keyvalues?, problems}], notes:[]}` — triggers keep the scan order (file
//! element order / the bridge's walk order); `total`/`summary.byClass` cover every surviving
//! trigger, the page only the slice. Tests: `tests/triggers_query.rs`.
use crate::state::HammerState;
use crate::tools::entities_file::{resolve_document, source_json, stale_read};
use crate::vmap::{Document, Entity};
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// `limit` bound and default (plan-triggers.md §1).
const LIMIT_MAX: u64 = 2000;
const LIMIT_DEFAULT: u64 = 200;
/// The live scan reads at most this many candidates with one `entity.get` each (the
/// `list_props` rule; `notes` names the cut when more candidates existed).
const SCAN_CAP: usize = 512;
/// The kinds a `kind` argument may ask for. There is no "any": the tool defaults to
/// `trigger` and `solid` is the audit view over every `@SolidClass`.
const KIND_FILTERS: [&str; 2] = ["trigger", "solid"];
/// The `withinMode` values (review C5: containment was implicit; the orchestrator's
/// decision adds the explicit overlap mode).
const WITHIN_MODES: [&str; 2] = ["contains", "intersects"];

/// The FGD database for `state.fgd_root`, mapped onto the tool taxonomy — shared with
/// `list_props` in [`super::live_common`] (review C6: the module-local copy is gone).
use super::live_common::fgd_or_error;

// -------------------------------------------------------------------------------------------
// Registration
// -------------------------------------------------------------------------------------------

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "list_triggers",
            "List brush entities (triggers, or every @SolidClass with kind \"solid\") from the SAVED \
             .vmap (source \"file\", reports staleRead) or the LIVE document (source \"live\"). \
             Volumes are the union of the mesh children's bounds. Filter by class, targetname or a \
             `within` box.",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "enum": ["file", "live"],
                    "description": "default: file when `path` is given, else live"
                },
                "path": {
                    "type": "string",
                    "description": "file route only: addon-relative (maps/x.vmap) or absolute .vmap"
                },
                "kind": {
                    "type": "string",
                    "enum": KIND_FILTERS,
                    "description": "trigger (default) = trigger classes; solid = every @SolidClass"
                },
                "class": {
                    "type": "string",
                    "minLength": 1,
                    "description": "exact classname, case-insensitive"
                },
                "targetname": {
                    "type": "string",
                    "minLength": 1,
                    "description": "exact targetname, case-insensitive"
                },
                "within": {
                    "type": "object",
                    "description": "axis-aligned box matched against the volume (origin if the volume is unknown)",
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
                "withinMode": {
                    "type": "string",
                    "enum": WITHIN_MODES,
                    "description": "contains (default) = volume entirely inside; intersects = overlaps"
                },
                "includeKeyValues": {
                    "type": "boolean",
                    "description": "default false"
                },
                "includeConnections": {
                    "type": "boolean",
                    "description": "default true"
                },
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": LIMIT_MAX,
                    "default": LIMIT_DEFAULT
                },
                "offset": {
                    "type": "integer", "minimum": 0, "default": 0
                }
            },
            "additionalProperties": false
        })),
        list_triggers,
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
    "within",
    "withinMode",
    "includeKeyValues",
    "includeConnections",
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

/// A boolean flag argument with a default (`includeConnections` defaults to true).
fn bool_arg(args: &Value, name: &str, default: bool) -> Result<bool, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "{name} must be a boolean, got {other}"
        ))),
    }
}

/// An optional string filter; a present-but-empty string is refused (an empty class/targetname
/// filter would match everything while pretending to filter — the entities-file rule).
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

/// `kind?`: `None` (= the default `trigger`) or one of [`KIND_FILTERS`].
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
            Ok(Some(text.to_string()))
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

/// The route `list_triggers` reads one document through. A two-variant enum instead of a
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

/// `within?: {min:[x,y,z], max:[x,y,z]}` — an axis-aligned box the trigger must lie in;
/// `max >= min` on every axis. Applied locally on both routes (the bridge's `within` is a
/// sphere, so the live route never forwards it): the numbers are only compared, never
/// serialized, so no wire coordinate-form checks apply.
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

/// `withinMode?`: `"contains"` (the default) or `"intersects"` (review C5); anything else is
/// `InvalidArguments` before any work happens.
fn within_mode_arg(args: &Value) -> Result<&'static str, ToolError> {
    match args.get("withinMode") {
        None | Some(Value::Null) => Ok("contains"),
        Some(Value::String(text)) if text == "contains" => Ok("contains"),
        Some(Value::String(text)) if text == "intersects" => Ok("intersects"),
        Some(Value::String(other)) => Err(ToolError::invalid_arguments(format!(
            "withinMode must be one of {}, got {other:?}",
            WITHIN_MODES.join(", ")
        ))),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "withinMode must be one of {}, got {other}",
            WITHIN_MODES.join(", ")
        ))),
    }
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
// Classification (WP-F's merged API)
// -------------------------------------------------------------------------------------------

/// Why an entity is (or is not) part of the trigger list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inclusion {
    /// A `@SolidClass` that resolves to a trigger — listed under kind trigger (and solid).
    Trigger,
    /// A `@SolidClass` that is not a trigger (`func_buyzone`, `func_brush`, …) — listed under
    /// kind solid.
    Solid,
    /// A class the FGD does not carry at all. Confirmed only by the entity's own `CMapMesh`
    /// children (kind solid only) — a stale/typoed classname must not vanish silently.
    Unknown,
}

/// Classify one entity's classname against the FGD database (see [`Inclusion`]).
///
/// The trigger definition is [`crate::fgd::Database::is_trigger_class`] (WP-F's merged API;
/// review C2's shim switch-over). The Solid-kind gate stays HERE on purpose (review B4):
/// `is_trigger_class` is true for the `@BaseClass` `Trigger`/`TriggerOnce` themselves and for
/// a `tags(Trigger)` class of any kind, so the caller keeps the kind check and the API cannot
/// turn a point class into a brush entity.
fn classify(db: &crate::fgd::Database, classname: &str) -> Option<Inclusion> {
    use crate::fgd::ClassKind::Solid;
    let Some(def) = db.class(classname) else {
        return Some(Inclusion::Unknown);
    };
    if def.kind != Solid {
        return None; // a Point/Base/other class is never a brush entity
    }
    if db.is_trigger_class(classname) {
        Some(Inclusion::Trigger)
    } else {
        Some(Inclusion::Solid)
    }
}

/// The spawnflag definition of `class` through the FGD database (WP-F's
/// [`crate::fgd::Database::spawnflags_def`]), kept filtered to a `spawnflags(flags)` key
/// until WP-V's `type:"flags"` check lands in the database itself (review B4/C2): a class
/// declaring `spawnflags(integer)` must not decode with an empty flag list and
/// `unknown_bits == value`.
fn spawnflags_def<'a>(db: &'a crate::fgd::Database, class: &str) -> Option<&'a crate::fgd::KeyDef> {
    db.spawnflags_def(class).filter(|key| key.type_ == "flags")
}

/// One decoded `spawnflags` view (`spawnflags:{value, raw, flags, unknownBits}` in the record).
struct SpawnflagsView {
    /// The parsed value; `None` when the entity carries no `spawnflags` keyvalue at all or the
    /// text does not parse (the `problems` entry names it, the record keeps `raw`).
    value: Option<u64>,
    /// The raw keyvalue text (`None` when the entity has no `spawnflags` keyvalue).
    raw: Option<String>,
    /// The declared flags with their on state (declaration order; empty when there is no
    /// definition or no value to decode).
    flags: Vec<(u64, String, bool)>,
    /// Value bits no declared flag covers (`value & !mask`); every bit of the value when
    /// there is no definition to decode against.
    unknown_bits: u64,
}

/// Decode one entity's `spawnflags` keyvalue against the class's definition, through WP-F's
/// [`crate::fgd::Database::decode_spawnflags`] (review C2's shim switch-over; the local
/// [`SpawnflagsView`] keeps `raw`, which the database's view lacks): the text parses as `u64`
/// after trimming, and an empty text is value 0 (how Hammer stores "all off", not a parse
/// failure); every other unparseable value, including a negative or fractional one, is a
/// problem entry, never a crash. `raw` may be a JSON string or number (the live reply's
/// keyValues are text, but a numeric-looking value survives as a number through some
/// transports); any other JSON type is not decoded at all (no problem entry — the record
/// keeps the raw text only for text/number values, as before the switch).
///
/// The decode runs only when the class has a *filtered* definition ([`spawnflags_def`]):
/// without one (or with a `spawnflags(integer)` declaration, review B4) the value still
/// parses locally — flags stay `[]` and every bit counts as unknown.
fn decode_spawnflags(
    db: &crate::fgd::Database,
    class: &str,
    raw: Option<&Value>,
) -> SpawnflagsView {
    let text = match raw {
        None | Some(Value::Null) => {
            return SpawnflagsView {
                value: None,
                raw: None,
                flags: Vec::new(),
                unknown_bits: 0,
            };
        }
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(_) => {
            return SpawnflagsView {
                value: None,
                raw: None,
                flags: Vec::new(),
                unknown_bits: 0,
            };
        }
    };
    if spawnflags_def(db, class).is_none() {
        let trimmed = text.trim();
        let value = if trimmed.is_empty() {
            Some(0u64)
        } else {
            trimmed.parse::<u64>().ok()
        };
        return SpawnflagsView {
            value,
            raw: Some(text),
            flags: Vec::new(),
            unknown_bits: value.unwrap_or(0),
        };
    }
    match db.decode_spawnflags(class, &text) {
        Ok(view) => SpawnflagsView {
            value: Some(view.value),
            raw: Some(text),
            flags: view
                .flags
                .into_iter()
                .map(|flag| (flag.bit, flag.label, flag.on))
                .collect(),
            unknown_bits: view.unknown_bits,
        },
        Err(crate::fgd::SpawnflagsError::NotInteger { .. }) => SpawnflagsView {
            value: None,
            raw: Some(text),
            flags: Vec::new(),
            unknown_bits: 0,
        },
        // unreachable through the filtered def above; kept for exhaustiveness
        Err(crate::fgd::SpawnflagsError::NoSpawnflags { .. }) => SpawnflagsView {
            value: None,
            raw: Some(text),
            flags: Vec::new(),
            unknown_bits: 0,
        },
    }
}

// -------------------------------------------------------------------------------------------
// Candidate gathering
// -------------------------------------------------------------------------------------------

/// One scanned entity plus what the examine pass knows about it.
struct Candidate {
    entity: Entity,
    inclusion: Inclusion,
    /// File route: the entity's keyvalues (DMX strings); live route: empty until examined.
    file_keyvalues: Vec<(String, Value)>,
    /// File route: the mesh children `(class, nodeID)` of the entity
    /// ([`Document::children_of`]).
    file_children: Option<Vec<(String, Option<i64>)>>,
    /// Live route: the `entity.get` reply's keyValues, once read.
    live_keyvalues: Option<Map<String, Value>>,
    /// Live route: the `entity.get` reply's angles (null on the wire when unreadable).
    live_angles: Option<Value>,
    /// Live route: the reply's `children` — `None` = the member is absent (no m2.19 bridge),
    /// `Some(None)` = `null` (the bridge could not read the array),
    /// `Some(Some(pairs))` = the `[{nodeID, class}]` rows.
    live_children: Option<Option<Vec<(i64, String)>>>,
    /// Live route: the reply's `childrenTruncated` (absent/false = the full list arrived;
    /// true = the bridge cut the list, so the volume must not be computed from the partial
    /// rows — review C4).
    live_children_truncated: bool,
    /// Live route: the reply's I/O `connections` array, as the bridge reports it.
    live_connections: Option<Value>,
    /// Live route: failure of the per-trigger read (a `problems` entry, never a silent skip).
    fetch_error: Option<String>,
}

impl Candidate {
    /// The spawnflags keyvalue of the entity, whichever route supplied it (file keyvalues in
    /// file order, then the live reply's keyValues).
    fn spawnflags_value(&self) -> Option<&Value> {
        if let Some((_, value)) = self
            .file_keyvalues
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("spawnflags"))
        {
            return Some(value);
        }
        self.live_keyvalues
            .as_ref()?
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("spawnflags"))
            .map(|(_, value)| value)
    }

    /// The entity's keyvalues as a JSON object (live: the reply's keyValues; file: the
    /// DMX attributes).
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

/// The targetname as it lands in a trigger record (`null` when the entity is unnamed).
fn targetname_json_name(entity: &Entity) -> Option<&str> {
    entity.targetname.as_deref().filter(|name| !name.is_empty())
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

/// The file-route candidates: every entity of the SAVED `.vmap` whose element class is
/// `CMapEntity`, in element order, each classified against the FGD database. Resolution and the
/// error codes are [`resolve_document`]'s (the same ones `list_entities` reports). Returns the
/// parsed document too (the record build reads the mesh records from it).
async fn candidates_file(
    state: &HammerState,
    ctx: &CallCtx,
    db: &Arc<crate::fgd::Database>,
    kind: &str,
    args: &Value,
) -> Result<(Vec<Candidate>, Map<String, Value>, Arc<Document>), ToolError> {
    let (host, document, is_active, dirty) = resolve_document(state, ctx, args).await?;
    let candidates: Vec<Candidate> = document
        .entities()
        .iter()
        .filter(|entity| entity.node_class == "CMapEntity")
        .filter_map(|entity| {
            let inclusion = classify(db.as_ref(), &entity.classname)?;
            // kind solid keeps the unknown-class candidates in play: the record build confirms
            // them by the entity's own mesh children (kind trigger drops them here already).
            if kind == "trigger" && inclusion != Inclusion::Trigger {
                return None;
            }
            Some(Candidate {
                entity: entity.clone(),
                inclusion,
                file_keyvalues: entity.keyvalues.clone(),
                file_children: entity
                    .node_id
                    .and_then(|id| document.children_of(id))
                    .map(<[(String, Option<i64>)]>::to_vec),
                live_keyvalues: None,
                live_angles: None,
                live_children: None,
                live_children_truncated: false,
                live_connections: None,
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
    Ok((candidates, envelope, document))
}

/// The caller's live-scan filters: `class`/`targetname` are forwarded to the bridge's own
/// (exact, case-insensitive) filters; `kind` is decidable from the row's classname (the FGD
/// classification) and thins the scan before the [`SCAN_CAP`] window fills. `within` is NOT
/// row-decidable (the volume comes from the per-candidate read), so it stays post-pass.
struct ScanFilters<'a> {
    class: Option<&'a str>,
    targetname: Option<&'a str>,
    kind: &'a str,
}

/// The live-route candidates (bridge m2.15 `entity.find`): one scan, `class`/`targetname`
/// forwarded, at most 2000 rows (the bridge's own cap). Returns the classified candidates only,
/// but never more than [`SCAN_CAP`] of them: `notes` says when candidates were cut.
async fn candidates_live(
    state: &HammerState,
    ctx: &CallCtx,
    db: &Arc<crate::fgd::Database>,
    filters: &ScanFilters<'_>,
) -> Result<(Vec<Candidate>, bool, usize), ToolError> {
    let ScanFilters {
        class,
        targetname,
        kind,
    } = *filters;
    let mut params = Map::new();
    if let Some(class) = class {
        params.insert("class".into(), json!(class));
    }
    if let Some(targetname) = targetname {
        params.insert("targetname".into(), json!(targetname));
    }
    // Scan at the bridge's own full page width (its default is 500); the classification here
    // decides what is a trigger, so nothing narrower is right.
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
        // Only candidates the caller's kind keeps are included; the unknown-class confirmation
        // needs the per-candidate read, so unknown rows fill the window under kind solid and
        // are confirmed (or dropped) at the record build.
        let Some(inclusion) = classify(db.as_ref(), &entity.classname) else {
            continue;
        };
        if kind == "trigger" && inclusion != Inclusion::Trigger {
            continue;
        }
        inclusive_seen += 1;
        if candidates.len() >= SCAN_CAP {
            continue;
        }
        candidates.push(Candidate {
            entity,
            inclusion,
            file_keyvalues: Vec::new(),
            file_children: None,
            live_keyvalues: None,
            live_angles: None,
            live_children: None,
            live_children_truncated: false,
            live_connections: None,
            fetch_error: None,
        });
    }
    Ok((candidates, bridge_truncated, inclusive_seen))
}

/// The live examine pass: one `entity.get` per candidate for its keyvalues, angles, connections
/// and (m2.19) `children`. A failed read is kept as a `fetch_error` problems entry; a
/// tool-budget exhaustion DROPS the unexamined remainder with a note.
async fn examine_live(
    state: &HammerState,
    ctx: &CallCtx,
    candidates: &mut Vec<Candidate>,
    notes: &mut Vec<String>,
) {
    for index in 0..candidates.len() {
        let deadline = match ctx.budget.deadline_ms() {
            Ok(deadline) => deadline,
            Err(_) => {
                notes.push(format!(
                    "the tool budget ran out after {index} live reads: candidates past that are \
                     not in this result"
                ));
                candidates.truncate(index);
                return;
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
                candidates[index].live_keyvalues =
                    result.get("keyValues").and_then(Value::as_object).cloned();
                candidates[index].live_angles = result.get("angles").cloned();
                candidates[index].live_connections = result.get("connections").cloned();
                // absent (an m2.18 bridge) and explicit `false` both mean "the full list"
                candidates[index].live_children_truncated = result
                    .get("childrenTruncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                candidates[index].live_children = result.get("children").map(|children| {
                    children
                        .as_array()
                        .map(|rows| {
                            rows.iter()
                                .filter_map(|row| {
                                    Some((
                                        row.get("nodeID").and_then(Value::as_i64)?,
                                        row.get("class")
                                            .and_then(Value::as_str)
                                            .unwrap_or_default()
                                            .to_string(),
                                    ))
                                })
                                .collect::<Vec<_>>()
                        })
                        // a wire `null` is the bridge's "the array did not read"
                        .filter(|_| !children.is_null())
                });
            }
            Err(error) => {
                candidates[index].fetch_error = Some(error.to_string());
            }
        }
    }
}

// -------------------------------------------------------------------------------------------
// Volumes
// -------------------------------------------------------------------------------------------

/// A computed volume: the union of the mesh children's world bounds.
struct Volume {
    min: [f64; 3],
    max: [f64; 3],
}

impl Volume {
    fn from_union(boundss: &[([f64; 3], [f64; 3])]) -> Option<Volume> {
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        for (lo, hi) in boundss {
            for axis in 0..3 {
                min[axis] = min[axis].min(lo[axis]);
                max[axis] = max[axis].max(hi[axis]);
            }
        }
        (!boundss.is_empty()).then_some(Volume { min, max })
    }

    fn to_json(&self, geometry_source: Option<&str>) -> Value {
        // the mesh summary's shared rounding (`vmap::mesh::round3`, review C6)
        use crate::vmap::mesh::round3;
        json!({
            "min": self.min.iter().map(|c| round3(*c)).collect::<Vec<_>>(),
            "max": self.max.iter().map(|c| round3(*c)).collect::<Vec<_>>(),
            "size": (0..3)
                .map(|axis| round3(self.max[axis] - self.min[axis]))
                .collect::<Vec<_>>(),
            "center": (0..3)
                .map(|axis| round3((self.max[axis] + self.min[axis]) / 2.0))
                .collect::<Vec<_>>(),
            "geometrySource": geometry_source.map(Value::from).unwrap_or(Value::Null),
        })
    }
}

/// The file-route volume for one candidate: the union of its mesh children's bounds from the
/// document's mesh records, with a `problems` entry for every child this crate cannot place
/// (a skipped mesh record, a missing node id, an unproven container).
fn file_volume(
    document: &Document,
    children: Option<&[(String, Option<i64>)]>,
    problems: &mut Vec<String>,
) -> (Option<Vec<i64>>, Option<Volume>) {
    let Some(children) = children else {
        return (None, None);
    };
    let mesh_ids: Vec<i64> = children
        .iter()
        .filter(|(class, _)| class == "CMapMesh")
        .filter_map(|(_, id)| *id)
        .collect();
    if children
        .iter()
        .any(|(class, id)| class == "CMapMesh" && id.is_none())
    {
        problems.push("a mesh child has no node id".to_string());
    }
    if mesh_ids.is_empty() {
        return (Some(Vec::new()), None);
    }
    let mut boundss = Vec::new();
    for id in &mesh_ids {
        let Some(record) = document.mesh_by_id(*id) else {
            problems.push(format!(
                "mesh {id} is in the entity's children but the file's readable mesh records do \
                 not carry it"
            ));
            continue;
        };
        if let Some(problem) = record.parent_problem() {
            problems.push(format!("mesh {id}: {problem:?}"));
            continue;
        }
        boundss.push(record.bounds());
    }
    (Some(mesh_ids), Volume::from_union(&boundss))
}

/// The most `mesh.get` reads one `list_triggers`/`list_brush_entities` live scan makes.
const MAX_LIVE_MESH_READS: usize = 64;

/// The live route's geometry sources for the whole call: the bounds of every mesh child read live
/// through `mesh.get` (when the bridge advertises it), and the SAVED file parsed for the volume
/// fallback.
type LiveGeometry = (HashMap<i64, ([f64; 3], [f64; 3])>, Option<Arc<Document>>);

/// The live-route volume for one candidate's mesh children: the live `mesh.get` bounds first,
/// then the SAVED file. `None` = no source answered for every mesh; the caller notes it.
fn live_volume(
    mesh_ids: &[i64],
    live: &HashMap<i64, ([f64; 3], [f64; 3])>,
    saved: Option<&Document>,
) -> (Option<Volume>, Option<&'static str>) {
    let mut boundss = Vec::new();
    let mut missing = 0usize;
    let mut live_hits = 0usize;
    for id in mesh_ids {
        if let Some(bounds) = live.get(id) {
            boundss.push(*bounds);
            live_hits += 1;
            continue;
        }
        let Some(record) = saved.and_then(|saved| saved.mesh_by_id(*id)) else {
            missing += 1;
            continue;
        };
        if record.parent_problem().is_some() {
            missing += 1;
            continue;
        }
        boundss.push(record.bounds());
    }
    if missing > 0 {
        // a partial union would read as a shrunken volume: no volume instead
        return (None, None);
    }
    let source = if boundss.is_empty() {
        None
    } else if live_hits == boundss.len() {
        Some("live")
    } else if live_hits == 0 {
        Some("saved-file")
    } else {
        Some("mixed")
    };
    (Volume::from_union(&boundss), source)
}

/// The live route's geometry sources for the whole call: a `doc.active` read, one `mesh.get` per
/// mesh child of the candidates (skipped for a bridge without it), and, while the document is
/// clean, its SAVED file parsed for the volume fallback. `None` = no source is available (the
/// caller notes why).
async fn live_geometry_state(
    state: &HammerState,
    ctx: &CallCtx,
    candidates: &[Candidate],
    notes: &mut Vec<String>,
) -> Option<LiveGeometry> {
    let deadline = ctx.budget.deadline_ms().ok()?;
    let doc = match crate::tools::live_common::read_active_document(state, Some(deadline)).await {
        Ok(doc) => doc?,
        Err(error) => {
            notes.push(format!(
                "the active document could not be read ({error}): live `volume` stays null"
            ));
            return None;
        }
    };
    // the live bounds of every mesh child (each a read of the document as it is now)
    let mut live: HashMap<i64, ([f64; 3], [f64; 3])> = HashMap::new();
    let mut ids: Vec<i64> = candidates
        .iter()
        .filter_map(|candidate| candidate.live_children.as_ref()?.as_ref())
        .flatten()
        .filter(|(_, class)| class.eq_ignore_ascii_case("mesh"))
        .map(|(id, _)| *id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let total_ids = ids.len();
    let mut failed = 0usize;
    let mut stopped: Option<String> = None;
    for (read, id) in ids.into_iter().enumerate() {
        // one `mesh.get` per mesh child is a whole mesh each: cap them (a volume's meshes are
        // few; a scan with thousands is not worth thousands of full-geometry reads)
        if read >= MAX_LIVE_MESH_READS {
            stopped = Some(format!(
                "only the first {MAX_LIVE_MESH_READS} of {total_ids} mesh children were read \
                 live (the cap); the others take the saved file's volume when it is available"
            ));
            break;
        }
        match crate::tools::mesh_read::read_live_mesh(state, deadline, &doc, id).await {
            // a mesh under a container that is not proven world-space has no provable bounds
            Ok(Some(record)) if record.parent_problem().is_some() => failed += 1,
            Ok(Some(record)) => {
                live.insert(id, record.bounds());
            }
            // a bridge without `mesh.get`: the saved file is the only source
            Ok(None) => break,
            // this mesh has no usable live read (it vanished, it is too large for one reply):
            // the saved file may still answer for it; the volume is withheld when nothing does
            Err(error)
                if matches!(error.name.as_str(), "MeshNotFound" | "NotAMesh")
                    || crate::tools::mesh_read::is_too_large(&error) =>
            {
                failed += 1;
            }
            // anything else (the document moved, the bridge is gone) would repeat for every
            // remaining mesh: stop and say so
            Err(error) => {
                stopped = Some(format!(
                    "the live mesh reads stopped at nodeID {id} ({}): the remaining meshes \
                     take the saved file's volume when it is available",
                    error.name
                ));
                break;
            }
        }
    }
    if failed > 0 {
        notes.push(format!(
            "{failed} mesh(es) could not be read live (not in the document, too large for one \
             reply, or under a container not proven world-space): their volume comes from the \
             saved file or stays null"
        ));
    }
    if let Some(note) = stopped {
        notes.push(note);
    }
    let dirty = super::lifecycle::dirty_state(&doc);
    // Review C3: an UNREPORTED dirty state is not a clean one (the same rule
    // `mesh_live::current_mesh` and `trigger_volume` apply) — a bridge that does not say
    // whether the document changed since its save must not serve the saved file's geometry
    // as the live volume.
    let saved = if dirty != Some(false) || state.unsaved_edits() > 0 {
        if !live.is_empty() {
            // the live reads answered: the stale file is not needed and nothing is worth saying
            None
        } else if dirty.is_none() {
            notes.push(
                "the active document does not report its `dirty` state: the saved file's mesh \
                 geometry cannot be trusted to be current, so `volume` stays null (save_map \
                 and re-read)"
                    .to_string(),
            );
            None
        } else {
            notes.push(
                "the live document has unsaved edits: the saved file's mesh geometry is stale, \
                 so `volume` stays null (save_map and re-read)"
                    .to_string(),
            );
            None
        }
    } else {
        let host = doc
            .get("path")
            .and_then(Value::as_str)
            .and_then(|windows| state.path_map.windows_to_host(windows));
        let Some(host) = host else {
            notes.push(
                "the active document has no mapped host path: the saved-file volume source is \
                 unavailable (save_map and read the file route)"
                    .to_string(),
            );
            return Some((live, None));
        };
        match crate::vmap::load(&host) {
            Ok(document) => Some(document),
            Err(error) => {
                notes.push(format!(
                    "the active document's saved file could not be read ({error}): the \
                     file-route volume source is unavailable"
                ));
                None
            }
        }
    };
    Some((live, saved))
}

// -------------------------------------------------------------------------------------------
// Trigger records
// -------------------------------------------------------------------------------------------

/// Where a record's volume comes from: the file route's own document, or the live route's
/// shared geometry state (live `mesh.get` bounds + the saved file).
enum VolumeSource<'a> {
    File(&'a Document),
    Live(Option<&'a LiveGeometry>),
}

/// The `(meshes, volume, geometrySource)` triple of one record. The mesh children decide it:
/// the file route knows them from the document ([`file_volume`]), the live route from the
/// reply's `children` ([`live_volume`] over the shared geometry state). Every "cannot say"
/// answer is an explicit `None` (never an invented empty list) and lands a problem or a note.
fn record_meshes_and_volume(
    candidate: &Candidate,
    source: &VolumeSource<'_>,
    problems: &mut Vec<String>,
    notes: &mut Vec<String>,
) -> (Option<Vec<i64>>, Option<Volume>, Option<&'static str>) {
    // File route: the document's children_of is the truth.
    if let Some(children) = &candidate.file_children {
        return match source {
            VolumeSource::File(document) => {
                let (meshes, volume) = file_volume(document, Some(children), problems);
                (meshes, volume, None)
            }
            // cannot happen (a file candidate always carries its children)
            VolumeSource::Live(_) => (None, None, None),
        };
    }
    // Live route: the reply's `children` — absent (older bridge), null (unreadable), rows.
    let Some(live) = &candidate.live_children else {
        let note = "no `children` in this bridge's entity.get replies (older than m2.19): \
                    `meshes` and `volume` are null — save_map and read the file route";
        if !notes.iter().any(|seen| seen == note) {
            notes.push(note.to_string());
        }
        return (None, None, None);
    };
    let Some(children) = live else {
        problems.push("the entity's children array could not be read (null)".into());
        return (None, None, None);
    };
    let ids: Vec<i64> = children
        .iter()
        .filter(|(_, class)| class.eq_ignore_ascii_case("mesh"))
        .map(|(id, _)| *id)
        .collect();
    if ids.len() != children.len() {
        problems.push(format!(
            "{} child(ren) are not meshes",
            children.len() - ids.len()
        ));
    }
    // A truncated list is a partial union: computing the volume from the rows that arrived
    // would read as a shrunken box of the real one (review C4). The meshes stay listed.
    if candidate.live_children_truncated {
        problems.push("children list truncated by the bridge".into());
        return (Some(ids), None, None);
    }
    if ids.is_empty() {
        return (Some(ids), None, None);
    }
    let Some((live_bounds, saved)) = (match source {
        VolumeSource::Live(Some(geometry)) => Some(geometry),
        // cannot happen (a live candidate never carries file children) / no geometry state
        _ => None,
    }) else {
        notes.push(
            "no geometry source for the live `volume` (no clean document read): save_map and \
             read the file route"
                .to_string(),
        );
        return (Some(ids), None, None);
    };
    let (volume, source_name) = live_volume(&ids, live_bounds, saved.as_deref());
    if volume.is_none() {
        notes.push(format!(
            "no volume for node {}: neither a live mesh.get read nor the saved file names geometry \
             for mesh(es) {:?} — save_map and read the file route",
            candidate
                .entity
                .node_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            ids
        ));
    }
    (Some(ids), volume, source_name)
}

/// One finished trigger record; JSON is rendered at the page boundary.
struct Trigger {
    candidate: Candidate,
    /// `None` = the route cannot say (live, no `children` in the reply); `Some` = the mesh
    /// children's node ids (possibly empty — then "no mesh children").
    meshes: Option<Vec<i64>>,
    volume: Option<Volume>,
    /// The live route's volume source (`live`/`saved-file`/`mixed`), when the volume
    /// is one.
    geometry_source: Option<&'static str>,
    spawnflags: SpawnflagsView,
    connections: Option<Value>,
    problems: Vec<String>,
}

impl Trigger {
    /// Build one record (meshes, volume, spawnflags, connections, problems). `route` decides
    /// where the I/O `connections` come from (review C1): the file route reads the entity's
    /// own `connectionsData` (the saved `.vmap` is the truth; the same records `get_entity`
    /// reports), the live route the reply's `connections` array.
    fn build(
        candidate: Candidate,
        db: &crate::fgd::Database,
        route: Route,
        source: &VolumeSource<'_>,
        notes: &mut Vec<String>,
    ) -> Trigger {
        let mut problems: Vec<String> = Vec::new();
        if candidate.inclusion == Inclusion::Unknown {
            problems.push("unknown class".to_string());
        }
        let (meshes, volume, geometry_source) =
            record_meshes_and_volume(&candidate, source, &mut problems, notes);
        if matches!(candidate.inclusion, Inclusion::Solid | Inclusion::Trigger)
            && meshes.as_ref().is_some_and(Vec::is_empty)
        {
            problems.push("no mesh children".to_string());
        }

        // spawnflags: the entity's own keyvalue against the class's definition; an unparseable
        // text is a problem entry, never a crash.
        let spawnflags = decode_spawnflags(
            db,
            &candidate.entity.classname,
            candidate.spawnflags_value(),
        );
        if spawnflags.value.is_none() && spawnflags.raw.is_some() {
            problems.push("spawnflags not an integer".to_string());
        }

        let connections = if let Some(error) = &candidate.fetch_error {
            problems.push(format!("live read failed: {error}"));
            None
        } else if route == Route::Live {
            candidate.live_connections.clone()
        } else {
            Some(Value::Array(
                candidate
                    .entity
                    .connections
                    .iter()
                    .map(|connection| Value::Object(connection.clone()))
                    .collect(),
            ))
        };
        Trigger {
            candidate,
            meshes,
            volume,
            geometry_source,
            spawnflags,
            connections,
            problems,
        }
    }

    fn to_json(&self, include_keyvalues: bool, include_connections: bool) -> Value {
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
        map.insert(
            "targetname".into(),
            targetname_json_name(&self.candidate.entity)
                .map(Value::from)
                .unwrap_or(Value::Null),
        );
        map.insert(
            "origin".into(),
            self.candidate
                .entity
                .origin
                .map(|origin| json!([origin[0], origin[1], origin[2]]))
                .unwrap_or(Value::Null),
        );
        map.insert(
            "angles".into(),
            match &self.candidate.live_angles {
                Some(angles) => angles.clone(),
                None => self
                    .candidate
                    .entity
                    .angles
                    .map(|angles| json!([angles[0], angles[1], angles[2]]))
                    .unwrap_or(Value::Null),
            },
        );
        map.insert(
            "meshes".into(),
            self.meshes
                .as_ref()
                .map(|meshes| json!(meshes))
                .unwrap_or(Value::Null),
        );
        map.insert(
            "volume".into(),
            self.volume
                .as_ref()
                .map(|volume| volume.to_json(self.geometry_source))
                .unwrap_or(Value::Null),
        );
        map.insert(
            "spawnflags".into(),
            json!({
                "value": self.spawnflags.value,
                "raw": self.spawnflags.raw,
                "flags": self.spawnflags.flags.iter().map(|(bit, label, on)| json!({
                    "bit": bit, "label": label, "on": on,
                })).collect::<Vec<_>>(),
                "unknownBits": self.spawnflags.unknown_bits,
            }),
        );
        if include_connections {
            map.insert(
                "connections".into(),
                self.connections.clone().unwrap_or(json!([])),
            );
        }
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
// list_triggers
// -------------------------------------------------------------------------------------------

/// The tool's entry point (see the module header for the whole result shape).
pub async fn list_triggers(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    check_known_args(&args)?;
    let limit = limit_arg(&args)?;
    let offset = offset_arg(&args)?;
    let kind = kind_arg(&args)?.unwrap_or_else(|| "trigger".to_string());
    let class = filter_str(&args, "class")?;
    let targetname = filter_str(&args, "targetname")?;
    let within = within_box(&args)?;
    let within_mode = within_mode_arg(&args)?;
    let include_keyvalues = bool_arg(&args, "includeKeyValues", false)?;
    let include_connections = bool_arg(&args, "includeConnections", true)?;
    let path = path_arg(&args)?;
    let route = route_of(&args, path)?;

    // The classification IS the brush definition: a game root without csgo/csgo.fgd cannot
    // silently mean "no triggers".
    let db = fgd_or_error(&state)?;

    let mut notes: Vec<String> = Vec::new();
    let (envelope, mut candidates, document) = match route {
        Route::File => {
            let (candidates, envelope, document) =
                candidates_file(&state, &ctx, &db, &kind, &args).await?;
            (envelope, candidates, Some(document))
        }
        Route::Live => {
            let (candidates, bridge_truncated, inclusive_seen) = candidates_live(
                &state,
                &ctx,
                &db,
                &ScanFilters {
                    class,
                    targetname,
                    kind: &kind,
                },
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
                     candidate triggers (one live read each): the rest are not in this result"
                ));
            }
            let mut envelope = Map::new();
            envelope.insert("source".into(), json!("live"));
            (envelope, candidates, None)
        }
    };

    // Live: read per-trigger detail (keyvalues, angles, connections, children) before the
    // record build; the geometry sources come from one shared doc read.
    let mut geometry: Option<LiveGeometry> = None;
    if route == Route::Live {
        examine_live(&state, &ctx, &mut candidates, &mut notes).await;
        geometry = live_geometry_state(&state, &ctx, &candidates, &mut notes).await;
    }

    // The kind's post-pass (the unknown-class confirmation plus the class/targetname filters
    // with identical semantics on both routes — the live scan already forwarded the two to the
    // bridge's own exact filters): a stale/typoed classname must not vanish silently, but
    // neither does it become a brush entity without its mesh children.
    let mut unknown_dropped = 0usize;
    candidates.retain(|candidate| {
        if class
            .map(|class| !candidate.entity.classname.eq_ignore_ascii_case(class))
            .unwrap_or(false)
        {
            return false;
        }
        if targetname
            .map(|name| {
                !targetname_json_name(&candidate.entity)
                    .is_some_and(|value| value.eq_ignore_ascii_case(name))
            })
            .unwrap_or(false)
        {
            return false;
        }
        if candidate.inclusion != Inclusion::Unknown {
            return true;
        }
        let has_meshes = match &candidate.file_children {
            Some(children) => children.iter().any(|(class, _)| class == "CMapMesh"),
            None => candidate
                .live_children
                .as_ref()
                .and_then(|children| children.as_ref())
                .is_some_and(|children| {
                    children
                        .iter()
                        .any(|(_, class)| class.eq_ignore_ascii_case("mesh"))
                }),
        };
        if has_meshes {
            true
        } else {
            unknown_dropped += 1;
            false
        }
    });
    if unknown_dropped > 0 {
        notes.push(format!(
            "{unknown_dropped} entity/entities of classes the FGD does not carry stayed out \
             (kind \"solid\" lists an unknown class only when its entity has CMapMesh children)"
        ));
    }

    // The records, in scan order. The file route reads the volumes from its own document; the
    // live route from the shared geometry state.
    let source = match route {
        Route::File => VolumeSource::File(
            document
                .as_deref()
                .expect("the file route always carries its parsed document"),
        ),
        Route::Live => VolumeSource::Live(geometry.as_ref()),
    };
    let mut triggers: Vec<Trigger> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        triggers.push(Trigger::build(
            candidate,
            db.as_ref(),
            route,
            &source,
            &mut notes,
        ));
    }

    // Record-level `within` (review C5): `withinMode` picks the rule — "contains" (default)
    // requires the volume bounds to lie ENTIRELY inside the box, "intersects" accepts an
    // overlap (a trigger straddling the box edge matches). A candidate without a volume is
    // tested on its ORIGIN in both modes (the entity origin is the first mesh's pivot, which
    // can lie outside a box the volume overlaps) — say so once when it applied.
    if let Some((min, max)) = within {
        let intersects = within_mode == "intersects";
        let mut origin_only = 0usize;
        triggers.retain(|trigger| {
            let inside = |point: &[f64; 3]| {
                (0..3).all(|axis| point[axis] >= min[axis] && point[axis] <= max[axis])
            };
            match &trigger.volume {
                Some(volume) => {
                    if intersects {
                        (0..3).all(|axis| {
                            volume.min[axis] <= max[axis] && volume.max[axis] >= min[axis]
                        })
                    } else {
                        inside(&volume.min) && inside(&volume.max)
                    }
                }
                None => {
                    let matched = trigger
                        .candidate
                        .entity
                        .origin
                        .is_some_and(|origin| inside(&origin));
                    if matched {
                        origin_only += 1;
                    }
                    matched
                }
            }
        });
        if origin_only > 0 {
            notes.push(format!(
                "{origin_only} trigger(s) matched the `within` box on their ORIGIN only (no \
                 volume on this route: the entity origin is the first mesh's pivot and can lie \
                 outside a box the volume overlaps)"
            ));
        }
    }

    // Totals + summary cover every surviving trigger, the page only the slice.
    let total = triggers.len() as u64;
    let mut by_class: BTreeMap<String, (String, u64)> = Default::default();
    for trigger in &triggers {
        let name = &trigger.candidate.entity.classname;
        by_class
            .entry(name.to_ascii_lowercase())
            .or_insert_with(|| (name.to_string(), 0u64))
            .1 += 1;
    }
    let summary: Map<String, Value> = by_class
        .into_values()
        .map(|(name, count)| (name, json!(count)))
        .collect();

    let skip = (offset.min(total)) as usize;
    let page: Vec<&Trigger> = triggers.iter().skip(skip).take(limit).collect();
    let returned = page.len() as u64;
    let truncated = offset + returned < total;

    let mut out = envelope;
    out.insert("total".into(), json!(total));
    out.insert("returned".into(), json!(returned));
    out.insert("offset".into(), json!(offset));
    out.insert("truncated".into(), json!(truncated));
    out.insert(
        "summary".into(),
        json!({ "byClass": Value::Object(summary) }),
    );
    out.insert(
        "triggers".into(),
        Value::Array(
            page.iter()
                .map(|trigger| trigger.to_json(include_keyvalues, include_connections))
                .collect(),
        ),
    );
    out.insert("notes".into(), json!(notes));
    Ok(Value::Object(out).into())
}
