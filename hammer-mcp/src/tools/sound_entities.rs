//! `create_sound_entities` (WP-S4, plan §1): the sound-entity placement tool. One call creates
//! the `tags( Sound )` FGD classes (`snd_event_*`, `env_soundscape`/`env_soundscape_proxy`) with
//! the key spellings the engine expects (`soundName`/`soundevent`, `enablesoundevent`,
//! `radius`, `box_mins`/`box_maxs`, `position0..7`) and — on request — the timing helper
//! entities (`logic_auto` with `OnMapSpawn -> StartSound/StopSound`, `logic_timer` with
//! `RefireTime`/`InitialDelay` and `OnTimer -> StartSound`), so an agent does not have to get
//! any of that right by hand (the generic `create_entities` stays the fallback).
//!
//! Flow (one write gate, `create_trigger`'s shape): everything local first — the kind→class map
//! or the FGD sound-class check, the entity family ([`Family`], derived from the FGD's class +
//! resolved bases: `env_soundscape` in the chain → soundscape, `point_soundevent` → snd_event,
//! anything else tags(Sound) → "other" — with the real FGD's class list standing in when there is
//! no FGD), the sound-name verdict (`check_sound_name`), the connection and
//! helper pre-flights (`entity_io::connection_run` with the class standing in for the entity it
//! is about to create) — **nothing is sent on any failure**; then ONE `edit.batch` of
//! `entity.create` ops for the sound entities AND their helpers (one undo group, `MCP: batch`);
//! then, per entity that has `connections` or a `schedule`, one `entity.addConnections` on the
//! new nodeIDs (one undo group each, `ConnectionSource::Node` — the targets only exist after the
//! batch). A failure after the batch is `PartialFailure` naming every created nodeID.
//!
//! The sound-name verdict goes through the shared [`crate::tools::sound_check`] seam: the
//! verdict source is a `HammerState` field (`SoundCheckSource`, production `Index` = the real
//! `soundevents::check_sound_name`), the same state seam `create_entities` /
//! `set_entity_keyvalues` read — one seam, not one per tool.
use crate::soundevents::SoundNameVerdict;
use crate::state::HammerState;
use cs2_mcp_common::bridge::{RequestOpts};
use cs2_mcp_common::tools::{
    CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec, require_caps,
};
use serde_json::{Map, Value, json};

pub(crate) use super::entities_live::{
    FgdNotes, FgdSource, MAX_OP_BYTES, batch_failure, batch_maybe_executed, keyvalues_arg,
    result_items, short_batch_failure, unknown_class, vec3_arg,
};
pub(crate) use super::entity_io::{Connection, ConnectionSource, connection_run, connections_list};
pub(crate) use super::live_common::{
    bool_arg, doc_id_of, expect_doc, invalid, note_edit, reject_unknown_keys,
    require_document,
};
pub(crate) use super::trigger_args::box_corner;

/// `entities` carries at most 32 entries (plan §1; the batch's own budget is 64 ops, but the
/// sound entities come with helpers, so the caller's list is tighter).
const MAX_ENTITIES: usize = 32;
/// `positions` maps onto `position0..7` (the FGD declares eight).
const MAX_POSITIONS: usize = 8;
/// `connections` carries at most 16 entries per entity (plan §1).
const MAX_CONNECTIONS: usize = 16;
/// The FGD default box the alignedbox/orientedbox classes declare (`box_mins`/`box_maxs`
/// `"-64 -64 -64"`/`"64 64 64"`); sent explicitly when the caller omits `box`, with a note.
const DEFAULT_BOX_MINS: [f64; 3] = [-64.0; 3];
const DEFAULT_BOX_MAXS: [f64; 3] = [64.0; 3];
/// The undo labels the bridge itself opens (the echoes are the reply's own labels; these are the
/// fallbacks when a reply carries none).
const BATCH_UNDO_LABEL: &str = "MCP: batch";
const ADD_CONNECTIONS_LABEL: &str = "MCP: add connections";
const NO_SAVE_NOTE: &str =
    "no auto-save: run save_map to write the changes; file-backed reads stay stale until then";

/// The argument names one entity accepts (`additionalProperties:false` is the schema's contract;
/// the handler is the gate).
const ENTITY_ARGS: [&str; 17] = [
    "kind",
    "class",
    "soundEvent",
    "legacySoundscape",
    "origin",
    "angles",
    "targetname",
    "startOnSpawn",
    "toLocalPlayer",
    "stopOnNew",
    "sourceEntityName",
    "radius",
    "box",
    "positions",
    "connections",
    "keyValues",
    "schedule",
];
/// `schedule`'s members (closed too).
const SCHEDULE_ARGS: [&str; 4] = ["startDelay", "stopDelay", "repeatEvery", "repeatInitialDelay"];
/// The key spellings this tool owns: inside `keyValues` they are refused ("use the field"),
/// case-insensitively like `keyvalues_arg`'s own refusals.
const RESERVED_KEYS: [&str; 10] = [
    "soundname",
    "soundevent",
    "enablesoundevent",
    "box_mins",
    "box_maxs",
    "radius",
    "startonspawn",
    "tolocalplayer",
    "stoponnew",
    "sourceentityname",
];

/// The kind→class map (plan §1): the fixed classes these kinds name, in declaration order.
const KIND_CLASS: [(&str, &str); 6] = [
    ("point", "snd_event_point"),
    ("sphere", "snd_event_sphere"),
    ("alignedbox", "snd_event_alignedbox"),
    ("orientedbox", "snd_event_orientedbox"),
    ("soundscape", "env_soundscape"),
    ("soundscape_proxy", "env_soundscape_proxy"),
];

fn class_of_kind(kind: &str) -> Option<&'static str> {
    KIND_CLASS
        .iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, class)| *class)
}

/// The entity family of a sound class, derived from the FGD (review H2), NOT from name
/// prefixes: `Soundscape` when the class or its resolved base chain contains
/// `env_soundscape` (`env_soundscape`, `snd_soundscape`, `env_soundscape_triggerable`,
/// `snd_soundscape_triggerable`); `SndEvent` when the chain contains `point_soundevent`
/// (`snd_event_point/sphere/alignedbox/orientedbox`, `snd_event_cone`, ...). Everything else
/// that carries `tags( Sound )` is `Other` (`snd_event_box_helper`, `snd_event_param`, the
/// proxy pair, `ambient_generic`, ...). The family decides whether `soundEvent` is required,
/// the soundscape-only keys (`enablesoundevent`, `position0..7`, the legacy `.vsc` route),
/// the schedule gate and the radius/box rules.
///
/// The proxy classes are deliberately NOT the soundscape family: their bases are
/// Targetname/Parentname (no `env_soundscape`), they declare `MainSoundscapeName` and no
/// sound-typed key, so `soundEvent`/`legacySoundscape` mean nothing on them. Same for
/// `snd_event_box_helper` (a Targetname child with box keys and no `soundName`) and
/// `snd_event_param` (an opvar setter): no required `soundEvent`, no family keys.
///
/// Without an FGD (or for a class the FGD does not carry) the family comes from the real FGD's
/// class list instead of the base chain: `env_soundscape`, `snd_soundscape`,
/// `env_soundscape_triggerable` and `snd_soundscape_triggerable` are the soundscape family,
/// every other `snd_event_*` is the point_soundevent family — **except** `snd_event_box_helper`
/// and `snd_event_param`, which share the prefix but are `Other` (they declare no sound-typed
/// key, so a `soundEvent` on them has nowhere to go). Everything else is `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    SndEvent,
    Soundscape,
    Other,
}

fn family_of(db: Option<&crate::fgd::Database>, class: &str) -> Family {
    match db.and_then(|db| db.class(class)) {
        Some(def) => {
            // the class itself plus its resolved base chain (depth-first, base first)
            let chain = std::iter::once(def.name.as_str())
                .chain(def.resolved_bases.iter().map(String::as_str));
            for name in chain {
                match name.to_ascii_lowercase().as_str() {
                    "env_soundscape" => return Family::Soundscape,
                    "point_soundevent" => return Family::SndEvent,
                    _ => {}
                }
            }
            Family::Other
        }
        // No FGD (or the class is not in it): the real `core/base.fgd`'s families, spelled out
        // (the FGD-free tree cannot derive them); anything else cannot be derived and is `Other`.
        None => {
            let lower = class.to_ascii_lowercase();
            match lower.as_str() {
                // `env_soundscape` and the classes that inherit it in the real FGD — these write
                // `soundevent` + `enablesoundevent` (they carry the sound-typed key)
                "env_soundscape"
                | "snd_soundscape"
                | "env_soundscape_triggerable"
                | "snd_soundscape_triggerable" => Family::Soundscape,
                // point_soundevent's subclasses, minus the two `tags( Sound )` classes that
                // share the prefix and declare no sound-typed key at all
                "snd_event_box_helper" | "snd_event_param" => Family::Other,
                _ if lower.starts_with("snd_event_") => Family::SndEvent,
                _ => Family::Other,
            }
        }
    }
}

// -------------------------------------------------------------------------------------------
// The sound-name gate (the shared state seam)
// -------------------------------------------------------------------------------------------

// -------------------------------------------------------------------------------------------
// The sound-name verdict → per-entity outcome
// -------------------------------------------------------------------------------------------

/// What the sound check decided for one entity (the result's `soundEvent.verdict` spelling).
enum SoundCheck {
    Known,
    SourceOnly { file: String },
    /// Only the compiled addon file still holds the name; its source dropped it.
    Stale { file: String },
    FilePath,
    /// Not found, and `allowUnknownSoundEvent:true` let it through.
    Unchecked,
}

impl SoundCheck {
    fn as_str(&self) -> &'static str {
        match self {
            SoundCheck::Known => "known",
            SoundCheck::SourceOnly { .. } => "sourceOnly",
            SoundCheck::Stale { .. } => "stale",
            SoundCheck::FilePath => "filePath",
            SoundCheck::Unchecked => "unchecked",
        }
    }
}

/// Check one entity's `soundEvent` name; `Unknown` refuses the whole call (nothing sent) unless
/// `allow_unknown` — a misspelt event name is silent at runtime and the hardest sound bug to
/// find (the model-check argument). The index cannot be built (`VpkMissing` …) refuses too:
/// never silently unchecked.
async fn check_entity_sound(
    state: &HammerState,
    index: usize,
    name: &str,
    allow_unknown: bool,
    warnings: &mut Vec<String>,
    notes: &mut Vec<String>,
) -> Result<SoundCheck, ToolError> {
    let verdict = crate::tools::sound_check::verdict(state, name).await?;
    Ok(match verdict {
        SoundNameVerdict::Known { source, file, .. } => {
            notes.push(format!(
                "soundevent '{name}' is in the index ({}, {file})",
                source.as_str()
            ));
            SoundCheck::Known
        }
        SoundNameVerdict::SourceOnly { file } => {
            notes.push(format!(
                "entities[{index}].soundEvent '{name}' is only in the addon source ({file}), not \
                 compiled: compile_soundevents, then re-check"
            ));
            SoundCheck::SourceOnly { file }
        }
        // The addon's compiled file still carries the name but its `.vsndevts` source removed
        // or renamed it: warn, never refuse (it still plays until the next compile).
        SoundNameVerdict::Stale { file } => {
            warnings.push(format!(
                "entities[{index}].soundEvent '{name}' was removed or renamed in the addon source \
                 ({file}) but the compiled file still carries it: it is gone after the next \
                 compile_soundevents"
            ));
            notes.push(format!(
                "entities[{index}].soundEvent '{name}' is stale in {file}: recompile and use the \
                 current name"
            ));
            SoundCheck::Stale { file }
        }
        SoundNameVerdict::FilePath => {
            notes.push(format!(
                "entities[{index}].soundEvent '{name}' is a file path: it is written verbatim \
                 and never looked up in the index"
            ));
            SoundCheck::FilePath
        }
        SoundNameVerdict::Empty => {
            // unreachable in practice (an empty soundEvent is refused at parse time); treat it
            // like the unchecked pass-through it would be
            warnings.push(format!(
                "entities[{index}].soundEvent is empty: the event name was written as \"\""
            ));
            SoundCheck::Unchecked
        }
        SoundNameVerdict::Unknown { suggestions } => {
            if !allow_unknown {
                return Err(sound_event_not_found(index, name, suggestions));
            }
            warnings.push(format!(
                "entities[{index}].soundEvent '{name}' is not in the soundevent index (unknown; \
                 written unchecked: allowUnknownSoundEvent)"
            ));
            SoundCheck::Unchecked
        }
    })
}

/// `SoundEventNotFound` (plan §1's local error): the whole call is refused before anything is
/// sent, with the case-insensitive/prefix suggestions and where the index looked.
fn sound_event_not_found(index: usize, name: &str, suggestions: Vec<String>) -> ToolError {
    let whitespace = if name != name.trim() {
        format!(
            " The name has leading/trailing whitespace; the trimmed name is '{}'.",
            name.trim()
        )
    } else {
        String::new()
    };
    ToolError::new(
        "SoundEventNotFound",
        "SoundEventNotFound",
        format!(
            "entities[{index}].soundEvent '{name}' is not in the soundevent index (addon files, \
             game/csgo and game/core VPKs): a name the engine does not register is silent at \
             runtime, so nothing was sent. Use a known name, allowUnknownSoundEvent to write it \
             unchecked, or edit_soundevents to add it.{whitespace}"
        ),
    )
    .with_data(json!({
        "index": index,
        "name": name,
        "sourcesChecked": ["addon-game", "addon-source", "vpk:csgo", "vpk:core"],
    }))
    .did_you_mean(suggestions)
    .retry_safe(true)
}

// -------------------------------------------------------------------------------------------
// Argument parsing
// -------------------------------------------------------------------------------------------

/// A number as the keyvalue text Hammer stores (`256` stays `256`, `2.5` stays `2.5`).
fn num_text(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// `[x, y, z]` as the FGD `vector` text (`"-64 -64 -64"`).
fn vec_text(value: &[f64; 3]) -> String {
    value
        .iter()
        .map(|axis| num_text(*axis))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `radius` per class: the shape comes from the FGD's own `radius` key when the class
/// declares one (`snd_event_sphere`'s float, `env_soundscape`/`env_soundscape_proxy`'s
/// integer), else — only when the FGD does not know the class — from the family (`SndEvent`
/// float > 0, `Soundscape` integer ≥ 1 or `-1`, `Other` refused). A known class whose FGD
/// declares no radius key refuses the field (there is no key to write).
fn radius_arg(
    label: &str,
    value: &Value,
    class: &str,
    class_known: bool,
    family: Family,
    radius_key: Option<&crate::fgd::KeyDef>,
) -> Result<String, ToolError> {
    let number = value.as_f64().filter(|r| r.is_finite()).ok_or_else(|| {
        invalid(format!("{label}.radius must be a finite number, got {value}"))
    })?;
    let float_rules = |number: f64| -> Result<String, ToolError> {
        if number <= 0.0 {
            return Err(invalid(format!(
                "{label}.radius must be > 0 (the FGD radius key is a float), got {number}"
            )));
        }
        Ok(num_text(number))
    };
    let integer_rules = |number: f64| -> Result<String, ToolError> {
        if number.fract() != 0.0 {
            return Err(invalid(format!(
                "{label}.radius must be an integer (the FGD radius key is an integer key), got \
                 {number}"
            )));
        }
        if !(number >= 1.0 || number == -1.0) {
            return Err(invalid(format!(
                "{label}.radius must be >= 1 or -1 (the FGD off value), got {number}"
            )));
        }
        Ok(num_text(number))
    };
    if let Some(key) = radius_key {
        return if matches!(key.type_.as_str(), "integer" | "int" | "flags") {
            integer_rules(number)
        } else {
            float_rules(number)
        };
    }
    if class_known {
        return Err(invalid(format!(
            "{label}.radius is not valid for a class the FGD gives no radius key ({class}); \
             write it through keyValues only if a real key exists"
        )));
    }
    match family {
        Family::SndEvent => float_rules(number),
        Family::Soundscape => integer_rules(number),
        Family::Other => Err(invalid(format!(
            "{label}.radius is only valid for kind \"sphere\" and the soundscape kinds (the \
             other classes declare no radius key)"
        ))),
    }
}

/// `box {min, max}` per class: allowed only when the FGD knows the class AND declares both
/// `box_mins`/`box_maxs` (`snd_event_alignedbox`, `snd_event_orientedbox`,
/// `snd_event_box_helper`), or — when the FGD is unavailable / the class is not in it — for
/// the two fixed box kinds. Per-axis `min < max`, corners within the bridge's coordinate
/// bound (`trigger_args::box_corner`). `Ok(None)` = the field was absent; the caller then
/// sends the FGD default box explicitly (the two fixed box kinds) or nothing (`Other`).
#[allow(clippy::type_complexity)]
fn box_arg(
    label: &str,
    value: Option<&Value>,
    class: &str,
    class_known: bool,
    box_keys: bool,
) -> Result<Option<([f64; 3], [f64; 3])>, ToolError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let allowed = if class_known {
        box_keys
    } else {
        class == "snd_event_alignedbox" || class == "snd_event_orientedbox"
    };
    if !allowed {
        return Err(invalid(format!(
            "{label}.box is only valid for the classes whose FGD declares box_mins/box_maxs \
             (the alignedbox/orientedbox kinds, snd_event_box_helper); use origin (and radius \
             for a sphere)"
        )));
    }
    let object = value.as_object().ok_or_else(|| {
        invalid(format!("{label}.box must be an object {{min, max}} of [x, y, z] corners"))
    })?;
    let unknown = object
        .keys()
        .find(|key| !matches!(key.as_str(), "min" | "max"));
    if let Some(unknown) = unknown {
        return Err(invalid(format!(
            "{label}.box has the unknown member {unknown:?}: a box takes min and max"
        )));
    }
    let min = box_corner(&format!("{label}.box.min"), object.get("min").unwrap_or(&Value::Null))?;
    let max = box_corner(&format!("{label}.box.max"), object.get("max").unwrap_or(&Value::Null))?;
    for axis in 0..3 {
        if min[axis] >= max[axis] {
            return Err(invalid(format!(
                "{label}.box axis {axis}: min {} must be < max {} (box_mins/box_maxs are written \
                 verbatim; the box is entity-local — Hammer's own helper draws it around the \
                 origin)",
                min[axis], max[axis]
            )));
        }
    }
    Ok(Some((min, max)))
}

/// `positions` per family: the soundscape family only, 1..=8 non-empty targetname strings
/// written to `position0..7` (the env_soundscape keys the family inherits).
fn positions_arg(label: &str, value: &Value, class: &str, family: Family) -> Result<Vec<String>, ToolError> {
    if family != Family::Soundscape {
        return Err(invalid(format!(
            "{label}.positions is only valid for the soundscape family ('{class}' declares no \
             position0..7 keys in the FGD)"
        )));
    }
    let Some(list) = value.as_array() else {
        return Err(invalid(format!(
            "{label}.positions must be an array of 1..={MAX_POSITIONS} targetname strings"
        )));
    };
    if list.is_empty() || list.len() > MAX_POSITIONS {
        return Err(invalid(format!(
            "{label}.positions has {} entries, 1..={MAX_POSITIONS} fit (the FGD declares \
             position0..7)",
            list.len()
        )));
    }
    let mut out = Vec::with_capacity(list.len());
    for (index, entry) in list.iter().enumerate() {
        let Some(text) = entry.as_str().filter(|text| !text.trim().is_empty()) else {
            return Err(invalid(format!(
                "{label}.positions[{index}] must be a non-empty string (a targetname), got \
                 {entry}"
            )));
        };
        if text.len() > 255 || text.chars().any(char::is_control) {
            return Err(invalid(format!(
                "{label}.positions[{index}] must be at most 255 bytes without control characters"
            )));
        }
        out.push(text.to_string());
    }
    Ok(out)
}

/// The timing schedule of one entity: `startDelay`/`stopDelay` build ONE `logic_auto`
/// (`OnMapSpawn -> StartSound (delay startDelay)` / `-> StopSound (delay stopDelay)`),
/// `repeatEvery` ONE `logic_timer` (`RefireTime`, `InitialDelay`, `UseRandomTime "0"`,
/// `OnTimer -> StartSound`; the timer's FIRST fire lands at InitialDelay + RefireTime after
/// load — live: repeatEvery 5, repeatInitialDelay 1 → first StartSound 6 s after load, then
/// every 5 s). Refused for any class whose FGD carries no `StartSound` input (the soundscape
/// family: it plays while the player is inside its radius; `snd_event_box_helper` and
/// `snd_event_param` have no input to schedule either). `has_start_sound` is `None` when the
/// FGD cannot answer — the fallback then refuses only the known soundscape classes.
struct Schedule {
    start_delay: Option<f64>,
    stop_delay: Option<f64>,
    repeat_every: Option<f64>,
    repeat_initial_delay: Option<f64>,
}

fn schedule_arg(
    label: &str,
    value: Option<&Value>,
    class: &str,
    family: Family,
    has_start_sound: Option<bool>,
) -> Result<Option<Schedule>, ToolError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let allowed = has_start_sound
        .unwrap_or_else(|| family != Family::Soundscape);
    if !allowed {
        return Err(invalid(format!(
            "{label}.schedule needs a StartSound input to schedule, which '{class}' does not \
             declare in the FGD (soundscapes play while the player is inside their radius; \
             there is nothing to start)"
        )));
    }
    let object = value
        .as_object()
        .ok_or_else(|| invalid(format!("{label}.schedule must be an object")))?;
    if let Some(unknown) = object
        .keys()
        .find(|key| !SCHEDULE_ARGS.contains(&key.as_str()))
    {
        return Err(invalid(format!(
            "{label}.schedule has the unknown member {unknown:?}: a schedule takes startDelay, \
             stopDelay, repeatEvery and repeatInitialDelay"
        )));
    }
    let seconds = |key: &str| -> Result<Option<f64>, ToolError> {
        match object.get(key).filter(|value| !value.is_null()) {
            None => Ok(None),
            Some(value) => {
                let seconds = value.as_f64().filter(|s| s.is_finite()).ok_or_else(|| {
                    invalid(format!("{label}.schedule.{key} must be a finite number of seconds"))
                })?;
                Ok(Some(seconds))
            }
        }
    };
    let start_delay = seconds("startDelay")?;
    let stop_delay = seconds("stopDelay")?;
    let repeat_every = seconds("repeatEvery")?;
    let repeat_initial_delay = seconds("repeatInitialDelay")?;
    if let Some(start) = start_delay {
        if start < 0.0 {
            return Err(invalid(format!(
                "{label}.schedule.startDelay must be >= 0, got {start}"
            )));
        }
    }
    if let Some(stop) = stop_delay {
        let start = start_delay.unwrap_or(0.0);
        if stop <= start {
            return Err(invalid(format!(
                "{label}.schedule.stopDelay ({stop}) must be > startDelay ({start}): the sound \
                 would stop before it starts"
            )));
        }
    }
    if let Some(every) = repeat_every {
        if every <= 0.0 {
            return Err(invalid(format!(
                "{label}.schedule.repeatEvery must be > 0 (the logic_timer's RefireTime), got \
                 {every}"
            )));
        }
    } else if repeat_initial_delay.is_some() {
        return Err(invalid(format!(
            "{label}.schedule.repeatInitialDelay needs repeatEvery: without a repeat there is no \
             logic_timer to delay"
        )));
    }
    if start_delay.is_none() && stop_delay.is_none() && repeat_every.is_none() {
        return Err(invalid(format!(
            "{label}.schedule is empty: name startDelay, stopDelay and/or repeatEvery"
        )));
    }
    Ok(Some(Schedule {
        start_delay,
        stop_delay,
        repeat_every,
        repeat_initial_delay,
    }))
}

// -------------------------------------------------------------------------------------------
// The planned entities
// -------------------------------------------------------------------------------------------

/// One timing helper the batch creates with its parent.
struct Helper {
    /// `auto` | `timer` (the result's `helpers[].role`).
    role: &'static str,
    class: &'static str,
    targetname: String,
    keyvalues: Map<String, Value>,
    /// The connections `entity.addConnections` writes onto the helper after the batch.
    connections: Vec<Connection>,
}

/// One planned sound entity: everything the batch and the connection groups need.
struct Planned {
    index: usize,
    kind: Option<String>,
    class: String,
    targetname: Option<String>,
    origin: [f64; 3],
    angles: Option<[f64; 3]>,
    keyvalues: Map<String, Value>,
    sound_event: Option<String>,
    sound_check: Option<SoundCheck>,
    connections: Option<Vec<Connection>>,
    helpers: Vec<Helper>,
    warnings: Vec<String>,
    notes: Vec<String>,
}

/// The `entity.create` op for one planned entity or helper (`create_entities`' op shape).
fn create_op(
    class: &str,
    origin: [f64; 3],
    angles: Option<[f64; 3]>,
    keyvalues: &Map<String, Value>,
) -> Result<Value, ToolError> {
    let mut params = Map::new();
    params.insert("class".into(), json!(class));
    params.insert("origin".into(), json!(origin));
    if let Some(angles) = angles {
        params.insert("angles".into(), json!(angles));
    }
    if !keyvalues.is_empty() {
        params.insert("keyValues".into(), Value::Object(keyvalues.clone()));
    }
    let op = json!({"method": "entity.create", "params": Value::Object(params)});
    let bytes = op["params"].to_string().len();
    if bytes >= MAX_OP_BYTES {
        return Err(invalid(format!(
            "the entity serializes to {bytes} bytes of op params; the bridge cuts op params at \
             8192, keep it under {MAX_OP_BYTES} bytes"
        )));
    }
    Ok(op)
}

/// One entity of the request, fully planned: the class resolved (kind map or FGD sound class),
/// the keyvalues built with this tool's spellings, the sound verdict taken and the schedule
/// helpers built. **Nothing has been sent** when this returns — every failure here is a local
/// refusal.
async fn plan_entity(
    state: &HammerState,
    fgd: &FgdSource,
    entities: &[Value],
    index: usize,
    strict: bool,
    allow_unknown: bool,
) -> Result<Planned, ToolError> {
    let label = format!("entities[{index}]");
    let Some(entity) = entities.get(index).and_then(Value::as_object) else {
        return Err(invalid(format!("{label} must be an object")));
    };
    if let Some(unknown) = entity.keys().find(|key| !ENTITY_ARGS.contains(&key.as_str())) {
        return Err(invalid(format!(
            "{label} has the unknown member {unknown:?}: an entity takes kind or class, \
             soundEvent, legacySoundscape, origin, angles, targetname, startOnSpawn, \
             toLocalPlayer, stopOnNew, sourceEntityName, radius, box, positions, connections and \
             keyValues"
        )));
    }

    // exactly one of `kind` and `class`
    let kind = entity.get("kind").filter(|value| !value.is_null());
    let class_arg = entity.get("class").filter(|value| !value.is_null());
    let (class, kind_name) = match (kind, class_arg) {
        (Some(_), Some(_)) => {
            return Err(invalid(format!(
                "{label}: exactly one of kind and class, got both (the kind names a fixed sound \
                 class; class is for any other tags(Sound) class)"
            )));
        }
        (Some(kind), None) => {
            let kind = kind.as_str().ok_or_else(|| {
                invalid(format!("{label}.kind must be a string, got {kind}"))
            })?;
            let class = class_of_kind(kind).ok_or_else(|| {
                invalid(format!(
                    "{label}.kind '{kind}' is not one of {}",
                    KIND_CLASS
                        .iter()
                        .map(|(name, _)| format!("\"{name}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
            (class, Some(kind.to_string()))
        }
        (None, Some(class_arg)) => {
            let class = class_arg
                .as_str()
                .filter(|class| !class.trim().is_empty())
                .ok_or_else(|| {
                    invalid(format!("{label}.class must be a non-empty string, got {class_arg}"))
                })?;
            (class, None)
        }
        (None, None) => {
            return Err(invalid(format!(
                "{label}: exactly one of kind and class is required (the kind names one of {})",
                KIND_CLASS
                    .iter()
                    .map(|(name, _)| format!("\"{name}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    };

    let mut warnings: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // the FGD class checks (the kind map needs none; the explicit class must be a sound class)
    if let Some(db) = fgd.db() {
        if kind_name.is_none() {
            let Some(def) = db.class(class) else {
                let validation = db.validate(class, &Map::new());
                return Err(unknown_class(index, class, &validation.nearest));
            };
            if !db.is_sound_class(class) {
                return Err(ToolError::new(
                    "NotASoundClass",
                    "NotASoundClass",
                    format!(
                        "{label}: class '{class}' is a @{}Class in the FGD but carries no \
                         tags(Sound) (itself or through base()) — create_sound_entities places \
                         sound entities only; create_entities places the rest",
                        match def.kind {
                            crate::fgd::ClassKind::Solid => "Solid",
                            crate::fgd::ClassKind::Point => "Point",
                            crate::fgd::ClassKind::Base => "Base",
                            crate::fgd::ClassKind::Other => "other",
                        }
                    ),
                )
                .with_data(json!({"index": index, "class": class}))
                .retry_safe(true));
            }
        } else if db.class(class).is_none() {
            warnings.push(format!(
                "fgd: class '{class}' is not in the FGD database; its keyvalues were not \
                 validated"
            ));
        }
    }

    // the family + the FGD facts the key spellings hang off (review H2: derive the family
    // from the FGD, never from the class name)
    let db = fgd.db();
    let family = family_of(db, class);
    let def = db.and_then(|db| db.class(class));
    let class_known = def.is_some();
    let radius_key = def.and_then(|def| {
        def.keys.iter().find(|key| key.name.eq_ignore_ascii_case("radius"))
    });
    let box_keys = class_known
        && ["box_mins", "box_maxs"].iter().all(|name| {
            def.expect("class_known")
                .keys
                .iter()
                .any(|key| key.name.eq_ignore_ascii_case(name))
        });
    let has_start_sound =
        def.map(|def| def.inputs.iter().any(|io| io.name.eq_ignore_ascii_case("StartSound")));

    // the key spelling fields
    let origin = vec3_arg(&format!("{label}.origin"), entity.get("origin"))?;
    let angles = match entity.get("angles").filter(|value| !value.is_null()) {
        None => None,
        Some(angles) => Some(vec3_arg(&format!("{label}.angles"), Some(angles))?),
    };
    let targetname = match entity.get("targetname").filter(|value| !value.is_null()) {
        None => None,
        Some(Value::String(text)) if !text.trim().is_empty() => Some(text.clone()),
        Some(other) => {
            return Err(invalid(format!(
                "{label}.targetname must be a non-empty string, got {other}"
            )));
        }
    };
    let flag = |name: &str| -> Result<Option<&'static str>, ToolError> {
        match entity.get(name).filter(|value| !value.is_null()) {
            None => Ok(None),
            Some(Value::Bool(true)) => Ok(Some("1")),
            Some(Value::Bool(false)) => Ok(Some("0")),
            Some(other) => Err(invalid(format!(
                "{label}.{name} must be a boolean, got {other}"
            ))),
        }
    };
    let start_on_spawn = flag("startOnSpawn")?;
    let to_local_player = flag("toLocalPlayer")?;
    let stop_on_new = flag("stopOnNew")?;
    let source_entity_name = match entity.get("sourceEntityName").filter(|v| !v.is_null()) {
        None => None,
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        Some(other) => {
            return Err(invalid(format!(
                "{label}.sourceEntityName must be a non-empty string, got {other}"
            )));
        }
    };

    // the sound event: required for the four snd_event_* kinds and for the soundscape kinds
    // unless legacySoundscape names the legacy .vsc route
    let legacy = match entity.get("legacySoundscape").filter(|value| !value.is_null()) {
        None => None,
        Some(Value::String(text)) if !text.trim().is_empty() => Some(text.clone()),
        Some(other) => {
            return Err(invalid(format!(
                "{label}.legacySoundscape must be a non-empty string (the .vsc name), got {other}"
            )));
        }
    };
    if legacy.is_some() && family != Family::Soundscape {
        return Err(invalid(format!(
            "{label}.legacySoundscape is only valid for the soundscape family ('{class}' is no \
             env_soundscape descendant in the FGD: no legacy .vsc soundscape route)"
        )));
    }
    let sound_event = match entity.get("soundEvent").filter(|value| !value.is_null()) {
        None => None,
        Some(Value::String(text)) if !text.trim().is_empty() => Some(text.clone()),
        Some(other) => {
            return Err(invalid(format!(
                "{label}.soundEvent must be a non-empty string, got {other}"
            )));
        }
    };
    if sound_event.is_some() && legacy.is_some() {
        return Err(invalid(format!(
            "{label}: exactly one of soundEvent and legacySoundscape — the event name and the \
             legacy .vsc name are two routes to the same key"
        )));
    }
    if sound_event.is_none() && legacy.is_none() {
        match family {
            Family::SndEvent => {
                return Err(invalid(format!(
                    "{label}: soundEvent is required for '{class}' (the point_soundevent \
                     family plays nothing without one)"
                )));
            }
            Family::Soundscape => {
                return Err(invalid(format!(
                    "{label}: soundEvent is required for '{class}' (or legacySoundscape: the \
                     legacy .vsc route)"
                )));
            }
            // `Other` tags(Sound) classes (snd_event_box_helper, snd_event_param, ...): the
            // event name is optional — many of them carry no sound-typed key at all
            Family::Other => {}
        }
    }

    // the keyvalues start with the targetname; the sound key lands below (it needs the family
    // facts and runs the sound-name verdict)
    let mut keyvalues: Map<String, Value> = Map::new();
    if let Some(text) = targetname.as_deref() {
        keyvalues.insert("targetname".into(), json!(text));
    }

    // the sound event key: the class's sound-typed FGD key (`soundName` for the
    // point_soundevent family, `soundevent` for the soundscapes, whatever an `Other` class
    // declares — ambient_generic's `message`); the family default stands in when the FGD is
    // unavailable or declares none. An `Other` class with no sound-typed key at all refuses a
    // `soundEvent` (above): the name would be written nowhere.
    let mut sound_check: Option<SoundCheck> = None;
    if let Some(name) = &sound_event {
        let keys = db.map(|db| db.sound_keys(class)).unwrap_or_default();
        let key: Option<String> = if keys.len() == 1 {
            Some(keys[0].name.clone())
        } else if keys.len() > 1 {
            let default = if family == Family::SndEvent { "soundName" } else { "soundevent" };
            let key = keys
                .iter()
                .find(|key| key.name.eq_ignore_ascii_case(default))
                .map(|key| key.name.clone())
                .unwrap_or_else(|| keys[0].name.clone());
            notes.push(format!(
                "{label}: class '{class}' declares {} sound-typed keys ({}); '{key}' is used",
                keys.len(),
                keys.iter().map(|key| key.name.as_str()).collect::<Vec<_>>().join(", ")
            ));
            Some(key)
        } else {
            match family {
                // the family default: the real classes declare exactly one sound key each
                Family::SndEvent => Some("soundName".to_string()),
                Family::Soundscape => Some("soundevent".to_string()),
                Family::Other => {
                    // There is no key to write the name to: the entity would be created, the
                    // caller's `soundEvent` silently dropped and no verdict run, so the intent
                    // ("play this event here") would play nothing at runtime. Refuse the whole
                    // call instead — `keyValues` stays open for a class whose real sound key the
                    // FGD does not declare.
                    return Err(invalid(format!(
                        "{label}: class '{class}' declares no sound-typed key in the FGD; drop \
                         soundEvent, or pass the real key via keyValues (writing the name nowhere \
                         would create an entity that plays nothing)"
                    )));
                }
            }
        };
        if let Some(key) = key {
            // the sound-name verdict, BEFORE anything is sent (only when the name is written)
            sound_check = Some(
                check_entity_sound(
                    state,
                    index,
                    name,
                    allow_unknown,
                    &mut warnings,
                    &mut notes,
                )
                .await?,
            );
            keyvalues.insert(key, json!(name));
        }
    }
    if let Some(name) = &legacy {
        keyvalues.insert("soundscape".into(), json!(name));
        notes.push(format!(
            "{label}: legacySoundscape '{name}' is written unvalidated (the \
             scripts/soundscapes_*.vsc route is not indexed)"
        ));
    }
    if family == Family::Soundscape && (sound_event.is_some() || legacy.is_some()) {
        keyvalues.insert(
            "enablesoundevent".into(),
            json!(if legacy.is_some() { "0" } else { "1" }),
        );
    }
    if let Some(text) = start_on_spawn {
        keyvalues.insert("startOnSpawn".into(), json!(text));
    }
    if let Some(text) = to_local_player {
        keyvalues.insert("toLocalPlayer".into(), json!(text));
    }
    if let Some(text) = stop_on_new {
        keyvalues.insert("stopOnNew".into(), json!(text));
    }
    if let Some(text) = source_entity_name {
        keyvalues.insert("sourceEntityName".into(), json!(text));
    }
    if let Some(radius) = entity.get("radius").filter(|value| !value.is_null()) {
        let text = radius_arg(&label, radius, class, class_known, family, radius_key)?;
        keyvalues.insert("radius".into(), json!(text));
    }
    let box_corners = box_arg(&label, entity.get("box"), class, class_known, box_keys)?;
    if class == "snd_event_alignedbox" || class == "snd_event_orientedbox" {
        let (min, max) = box_corners.unwrap_or((DEFAULT_BOX_MINS, DEFAULT_BOX_MAXS));
        if box_corners.is_none() {
            notes.push(format!(
                "{label}: box omitted — the FGD default box {}..{} is sent explicitly",
                vec_text(&DEFAULT_BOX_MINS),
                vec_text(&DEFAULT_BOX_MAXS)
            ));
        }
        keyvalues.insert("box_mins".into(), json!(vec_text(&min)));
        keyvalues.insert("box_maxs".into(), json!(vec_text(&max)));
    } else if let Some((min, max)) = box_corners {
        // an `Other` class whose FGD declares the box keys (snd_event_box_helper): the
        // caller's corners verbatim, no FGD-default injection
        keyvalues.insert("box_mins".into(), json!(vec_text(&min)));
        keyvalues.insert("box_maxs".into(), json!(vec_text(&max)));
    }
    if let Some(positions) = entity.get("positions").filter(|value| !value.is_null()) {
        for (index, name) in
            positions_arg(&label, positions, class, family)?.into_iter().enumerate()
        {
            keyvalues.insert(format!("position{index}"), json!(name));
        }
    }
    // the caller's own extra keyvalues, minus this tool's spellings
    if let Some(raw) = entity.get("keyValues").filter(|value| !value.is_null()) {
        let extra = keyvalues_arg(&format!("{label}.keyValues"), raw)?;
        for key in extra.keys() {
            let lower = key.to_ascii_lowercase();
            if RESERVED_KEYS.contains(&lower.as_str())
                || lower.starts_with("position")
                    && lower["position".len()..].parse::<u8>().is_ok()
            {
                return Err(invalid(format!(
                    "{label}.keyValues.{key} cannot be set as a keyvalue: use the field (this \
                     tool owns the sound spellings so they cannot drift from the request)"
                )));
            }
        }
        keyvalues.extend(extra);
    }

    // the FGD keyvalue check (`create_entities`' policy: issues are warnings, `strict` refuses)
    let fgd_notes = match fgd.db() {
        Some(db) => FgdNotes::from_issues(&db.validate(class, &keyvalues).issues),
        None => FgdNotes::from_issues(&[]),
    };
    if strict && fgd_notes.has_errors() {
        return Err(invalid(format!(
            "{label} failed the FGD keyvalue check (strict): {}",
            fgd_notes.errors.join("; ")
        )));
    }
    warnings.extend(fgd_notes.messages.iter().cloned());

    // the schedule helpers, built now (their connections are pre-flighted against the helper
    // class, then written after the batch when the parent targetname exists)
    let schedule = schedule_arg(&label, entity.get("schedule"), class, family, has_start_sound)?;
    let mut helpers: Vec<Helper> = Vec::new();
    if let Some(schedule) = &schedule {
        let Some(parent) = targetname.as_deref() else {
            return Err(invalid(format!(
                "{label}: schedule needs targetname — the helpers address the sound entity by \
                 name (OnMapSpawn/OnTimer -> <targetname> would have no target)"
            )));
        };
        let mut auto_connections: Vec<Connection> = Vec::new();
        if let Some(start) = schedule.start_delay {
            auto_connections.push(helper_connection("OnMapSpawn", parent, "StartSound", Some(start)));
        }
        if let Some(stop) = schedule.stop_delay {
            auto_connections.push(helper_connection("OnMapSpawn", parent, "StopSound", Some(stop)));
        }
        if !auto_connections.is_empty() {
            helpers.push(Helper {
                role: "auto",
                class: "logic_auto",
                targetname: format!("{parent}_auto"),
                keyvalues: {
                    let mut kv = Map::new();
                    kv.insert("targetname".into(), json!(format!("{parent}_auto")));
                    kv
                },
                connections: auto_connections,
            });
        }
        // (auto_connections' entries are built below through `helper_connection`)
        if let Some(every) = schedule.repeat_every {
            let mut kv = Map::new();
            kv.insert("targetname".into(), json!(format!("{parent}_timer")));
            kv.insert("RefireTime".into(), json!(num_text(every)));
            if let Some(delay) = schedule.repeat_initial_delay {
                kv.insert("InitialDelay".into(), json!(num_text(delay)));
            }
            kv.insert("UseRandomTime".into(), json!("0"));
            helpers.push(Helper {
                role: "timer",
                class: "logic_timer",
                targetname: format!("{parent}_timer"),
                keyvalues: kv,
                connections: vec![helper_connection("OnTimer", parent, "StartSound", None)],
            });
        }
    }

    // the caller's own connections, parsed (1..=16)
    let connections = match entity.get("connections").filter(|value| !value.is_null()) {
        None => None,
        Some(value) => {
            let list = connections_list(value)?;
            if list.len() > MAX_CONNECTIONS {
                return Err(invalid(format!(
                    "{label}.connections has {} entries, at most {MAX_CONNECTIONS} fit per entity",
                    list.len()
                )));
            }
            Some(list)
        }
    };

    Ok(Planned {
        index,
        kind: kind_name,
        class: class.to_string(),
        targetname,
        origin,
        angles,
        keyvalues,
        sound_event,
        sound_check,
        connections,
        helpers,
        warnings,
        notes,
    })
}

/// One `output -> parent input` connection of a helper (the delay is the wire `delay`). The
/// `Connection` fields are private to `entity_io`; its parser is the only constructor, so the
/// wire object is rebuilt and parsed back — the same validation runs.
fn helper_connection(output: &str, parent: &str, input: &str, delay: Option<f64>) -> Connection {
    let mut object = json!({
        "output": output,
        "target": parent,
        "input": input,
    });
    if let Some(delay) = delay {
        object["delay"] = json!(delay);
    }
    connections_list(&json!([object]))
        .expect("helper connection is well-formed")
        .remove(0)
}

// -------------------------------------------------------------------------------------------
// The tool
// -------------------------------------------------------------------------------------------

/// `create_sound_entities {entities[1..=32], strict?, allowUnknownSoundEvent?, undoLabel?}`.
pub async fn create_sound_entities(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    reject_unknown_keys(
        &args,
        &["entities", "strict", "allowUnknownSoundEvent", "undoLabel"],
    )?;
    let Some(entities) = args.get("entities").and_then(Value::as_array) else {
        return Err(invalid(
            "entities: an array of 1..=32 sound-entity objects is required",
        ));
    };
    if entities.is_empty() {
        return Err(invalid("entities is empty, there is nothing to create"));
    }
    if entities.len() > MAX_ENTITIES {
        return Err(invalid(format!(
            "entities has {} entries, at most {MAX_ENTITIES} fit in one call (each may add \
             helper entities to the batch)",
            entities.len()
        )));
    }
    let strict = bool_arg(&args, "strict")?.unwrap_or(false);
    let allow_unknown = bool_arg(&args, "allowUnknownSoundEvent")?.unwrap_or(false);
    let undo_label = match args.get("undoLabel").filter(|value| !value.is_null()) {
        None => None,
        Some(Value::String(text)) if !text.trim().is_empty() => Some(text.clone()),
        Some(other) => {
            return Err(invalid(format!(
                "undoLabel must be a non-empty string, got {other}"
            )));
        }
    };

    let fgd = FgdSource::load(&state);
    let mut warnings: Vec<String> = fgd.warning().into_iter().collect();
    let mut notes: Vec<String> = vec![NO_SAVE_NOTE.to_string()];

    // everything local first: plan every entity (the sound verdict included) — nothing is sent
    // while any of this can still refuse
    let mut planned: Vec<Planned> = Vec::with_capacity(entities.len());
    for index in 0..entities.len() {
        planned.push(
            plan_entity(&state, &fgd, entities, index, strict, allow_unknown).await?,
        );
    }

    // the bridge methods THIS call dispatches, checked before the first send (the registry can
    // only declare what every call needs: a plain create works without the connection methods)
    let needs_connections = planned
        .iter()
        .any(|entity| entity.connections.is_some() || !entity.helpers.is_empty());
    if needs_connections {
        require_caps(
            &state.bridge,
            &["entity.find", "entity.get", "entity.addConnections"],
        )
        .await?;
    }

    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(&state, deadline).await?;
    let doc_id = doc_id_of(&doc);

    // the helper targetnames must not already be in the live document (a second `logic_auto` of
    // the same name would make the OnMapSpawn routing ambiguous — refused, nothing sent)
    let mut helper_names: Vec<(String, usize)> = Vec::new();
    for entity in &planned {
        for helper in &entity.helpers {
            helper_names.push((helper.targetname.clone(), entity.index));
        }
    }
    helper_names.sort_by(|a, b| a.0.cmp(&b.0));
    for pair in helper_names.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(invalid(format!(
                "the schedule of entities[{}] and entities[{}] both build helper '{}`: rename \
                 one of the entities' targetnames",
                pair[0].1, pair[1].1, pair[0].0
            )));
        }
    }
    for (name, index) in &helper_names {
        let rows = find_by_targetname(&state, deadline, name).await?;
        if !rows.is_empty() {
            let listed = rows
                .iter()
                .map(|(node_id, class)| format!("nodeID {node_id} ({class})"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(invalid(format!(
                "entities[{index}]: the helper targetname '{name}' is already in the live \
                 document ({listed}) — rename the entity's targetname or delete the old helper; \
                 nothing was sent"
            )));
        }
    }

    // the connection pre-flights, BEFORE the entities exist: the class stands in for the source
    // (an output the class does not declare — or a prop_static target — refuses the call with
    // nothing sent, `create_trigger`'s rule); the schedule helpers pre-flight against their own
    // class, their parent target does not exist yet and is allowed with a note
    for entity in &planned {
        if let Some(list) = &entity.connections {
            let run = connection_run(
                &state,
                deadline,
                ConnectionSource::Class(&entity.class),
                list,
                false,
            )
            .await?;
            merge_run_texts(&run.warnings, &run.notes, &mut warnings, &mut notes);
        }
        for helper in &entity.helpers {
            let run = connection_run(
                &state,
                deadline,
                ConnectionSource::Class(helper.class),
                &helper.connections,
                false,
            )
            .await?;
            merge_run_texts(&run.warnings, &run.notes, &mut warnings, &mut notes);
        }
    }

    // ---- ONE edit.batch: the sound entities AND their helpers (one undo group) ----
    let mut ops: Vec<Value> = Vec::new();
    /// The op index each planned entity / helper lands at (op order == creation order).
    struct OpSlot {
        entity: usize,
        helper: Option<usize>,
    }
    let mut slots: Vec<OpSlot> = Vec::new();
    for (entity_index, entity) in planned.iter().enumerate() {
        ops.push(create_op(
            &entity.class,
            entity.origin,
            entity.angles,
            &entity.keyvalues,
        )?);
        slots.push(OpSlot {
            entity: entity_index,
            helper: None,
        });
        for (helper_index, helper) in entity.helpers.iter().enumerate() {
            ops.push(create_op(
                helper.class,
                entity.origin,
                None,
                &helper.keyvalues,
            )?);
            slots.push(OpSlot {
                entity: entity_index,
                helper: Some(helper_index),
            });
        }
    }

    let mut batch_params = Map::new();
    if let Some(label) = &undo_label {
        batch_params.insert("undoLabel".into(), json!(label));
    }
    batch_params.insert("ops".into(), Value::Array(ops.clone()));

    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "edit.batch",
                Value::Object(batch_params),
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
            if batch_maybe_executed(&error) {
                note_edit(&state, doc_id.as_deref());
            }
            return Err(batch_failure(error, &state, &ops));
        }
    };
    note_edit(&state, doc_id.as_deref());
    if !reply.result.is_object() {
        return Err(ToolError::internal(format!(
            "edit.batch returned a non-object result: {} (crates/cs2-bridge/PROTOCOL.md §10)",
            reply.result
        )));
    }
    let items = result_items(&reply.result["results"], &ops);
    if items.len() != ops.len() {
        return Err(short_batch_failure(
            &state,
            items,
            ops.len(),
            reply.result.get("failedIndex").cloned().unwrap_or(Value::Null),
        ));
    }
    let mut labels: Vec<String> = vec![reply
        .result
        .get("undoLabel")
        .and_then(Value::as_str)
        .unwrap_or(BATCH_UNDO_LABEL)
        .to_string()];

    // the nodeIDs the batch created (an entity that needs connections but whose reply carries
    // none cannot be addressed — PartialFailure, everything up to here stands)
    let node_ids: Vec<Option<i64>> = items
        .iter()
        .map(|item| item.get("nodeID").and_then(Value::as_i64))
        .collect();
    let created: Vec<i64> = node_ids.iter().filter_map(|id| *id).collect();
    let mut progress = Progress {
        labels: labels.clone(),
        created,
    };
    for entity in planned.iter() {
        let needs_ids = entity.connections.is_some() || !entity.helpers.is_empty();
        let entity_slot = slots
            .iter()
            .position(|slot| slot.entity == entity.index && slot.helper.is_none())
            .unwrap_or(usize::MAX);
        if needs_ids && node_ids.get(entity_slot).copied().flatten().is_none() {
            return Err(after_batch_failure(
                &state,
                &progress,
                "entity.create reply",
                ToolError::internal(format!(
                    "entities[{}] was created but its reply carries no nodeID (a bridge without \
                     the m2.18 nodeID echo); the connections cannot be addressed",
                    entity.index
                )),
            ));
        }
    }

    // ---- per entity: the connections group(s), targets now resolvable ----
    let mut entity_out: Vec<Value> = Vec::new();
    for (entity_index, entity) in planned.iter().enumerate() {
        let entity_slot = slots
            .iter()
            .position(|slot| slot.entity == entity_index && slot.helper.is_none())
            .expect("every entity has its own op");
        let entity_id = node_ids[entity_slot].expect("checked above");

        // (a) the caller's connections onto the sound entity itself
        let mut connections_json = json!({"added": [], "unconfirmed": []});
        if let Some(list) = &entity.connections {
            let write = add_connections(
                &state,
                &mut progress,
                &mut labels,
                deadline,
                entity_id,
                list,
            )
            .await?;
            connections_json = write;
        }

        // (b) the schedule helpers' connections onto each helper node
        let mut helper_out: Vec<Value> = Vec::new();
        for (helper_index, helper) in entity.helpers.iter().enumerate() {
            let helper_slot = slots
                .iter()
                .position(|slot| {
                    slot.entity == entity_index && slot.helper == Some(helper_index)
                })
                .expect("every helper has its own op");
            let helper_id = node_ids[helper_slot].unwrap_or_else(|| {
                panic!("helper of a checked entity always has its nodeID")
            });
            add_connections(
                &state,
                &mut progress,
                &mut labels,
                deadline,
                helper_id,
                &helper.connections,
            )
            .await?;
            helper_out.push(json!({
                "role": helper.role,
                "class": helper.class,
                "nodeID": helper_id,
                "targetname": helper.targetname,
            }));
        }

        // (the item's echoed `applied`/`warnings` rows are folded into the result shape below)
        let sound_json = match (&entity.sound_event, &entity.sound_check) {
            (None, _) => Value::Null,
            (Some(name), None) => json!({"name": name, "verdict": "unchecked"}),
            (Some(name), Some(check)) => {
                let mut object = Map::new();
                object.insert("name".into(), json!(name));
                object.insert("verdict".into(), json!(check.as_str()));
                if let SoundCheck::SourceOnly { file } | SoundCheck::Stale { file } = check {
                    object.insert("file".into(), json!(file));
                }
                Value::Object(object)
            }
        };
        entity_out.push(json!({
            "index": entity.index,
            "kind": entity.kind,
            "class": entity.class,
            "nodeID": entity_id,
            "targetname": entity.targetname,
            "origin": entity.origin,
            "soundEvent": sound_json,
            "keyValuesSent": Value::Object(entity.keyvalues.clone()),
            "helpers": helper_out,
            "connections": connections_json,
            "warnings": entity.warnings,
            "notes": entity.notes,
        }));
    }

    let undo_groups = labels.len();
    notes.push(format!(
        "this call opened {undo_groups} undo group(s) ({}): `undo_last` {undo_groups} time(s) \
         reverts it (the batch is one group, each connections step its own)",
        labels
            .iter()
            .map(|label| format!("\"{label}\""))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if planned
        .iter()
        .any(|entity| entity.keyvalues.get("startOnSpawn").and_then(Value::as_str) != Some("1"))
    {
        notes.push(
            "startOnSpawn:false entities wait for a StartSound input (schedule startDelay, a \
             trigger, another entity's I/O)"
                .to_string(),
        );
    }

    let mut out = Map::new();
    out.insert("count".into(), json!(planned.len()));
    out.insert("requested".into(), json!(planned.len()));
    out.insert("entities".into(), Value::Array(entity_out));
    out.insert("undoGroups".into(), json!(undo_groups));
    out.insert("undoLabels".into(), json!(labels));
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("saved".into(), json!(false));
    out.insert("fgd".into(), fgd.json());
    if let Some(label) = &undo_label {
        out.insert("requestedUndoLabel".into(), json!(label));
    }
    out.insert("warnings".into(), json!(warnings));
    out.insert("notes".into(), json!(notes));
    Ok(Value::Object(out).into())
}

/// One `entity.addConnections` group after the batch (`ConnectionSource::Node`), as the
/// result's `connections` object; a failure is the call's `PartialFailure`.
async fn add_connections(
    state: &HammerState,
    progress: &mut Progress,
    labels: &mut Vec<String>,
    deadline: u64,
    node_id: i64,
    list: &[Connection],
) -> Result<Value, ToolError> {
    let run = match connection_run(
        state,
        deadline,
        ConnectionSource::Node(node_id),
        list,
        false,
    )
    .await
    {
        Ok(run) => run,
        Err(error) => {
            return Err(after_batch_failure(
                state,
                progress,
                "entity.addConnections",
                error,
            ));
        }
    };
    let write = run.write.expect("the Node form always performs the write");
    labels.push(
        write
            .result
            .get("undoLabel")
            .and_then(Value::as_str)
            .unwrap_or(ADD_CONNECTIONS_LABEL)
            .to_string(),
    );
    progress.labels = labels.clone();
    Ok(json!({
        "added": write.added,
        "unconfirmed": write.unconfirmed,
        "count": write.count,
        "outcomeUnknown": write.unconfirmed_note,
    }))
}

/// What one multi-group call has created and opened so far (the `PartialFailure` payload's
/// inputs; `create_trigger`'s `Progress` reduced to this tool's two facts).
struct Progress {
    labels: Vec<String>,
    created: Vec<i64>,
}

/// A failure **after** the batch: the entities exist, so it is `PartialFailure` (`-32012`,
/// not retry-safe) naming every created nodeID and the groups opened (`undo_last N`, or
/// `delete_nodes` for the created nodes).
fn after_batch_failure(
    state: &HammerState,
    progress: &Progress,
    step: &str,
    error: ToolError,
) -> ToolError {
    let groups = progress.labels.len();
    let created = progress.created.clone();
    let list = created
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut tool = ToolError::new(
        "PartialFailure",
        "PartialFailure",
        format!(
            "the sound entities were only partly built: {step} failed after {groups} undo \
             group(s) (this call created {created:?}): `undo_last` {groups} time(s) reverts this \
             call, or delete_nodes [{list}] for the created nodes — a retry of the whole call \
             would create the entities a second time. Bridge error: {}",
            error.message
        ),
    )
    .with_code(-32012)
    .with_data(json!({
        "step": step,
        "createdNodeIDs": created,
        "undoGroups": groups,
        "undoLabels": progress.labels,
        "bridgeError": {
            "name": error.name,
            "code": error.code,
            "message": error.message,
            "data": Value::Object(error.data.clone()),
        },
        "unsavedBridgeEdits": state.unsaved_edits(),
    }));
    tool.retry_safe = false;
    tool.outcome_unknown = error.outcome_unknown;
    tool
}

/// Merge a pre-flight run's warnings/notes into the call's lists (deduplicated — the write step
/// re-runs the same checks and its notes are the caller's own).
fn merge_run_texts(
    run_warnings: &[String],
    run_notes: &[String],
    warnings: &mut Vec<String>,
    notes: &mut Vec<String>,
) {
    for warning in run_warnings {
        if !warnings.contains(warning) {
            warnings.push(warning.clone());
        }
    }
    for note in run_notes {
        if !notes.contains(note) && note != NO_SAVE_NOTE {
            notes.push(note.clone());
        }
    }
}

/// One `entity.find` by targetname as `(nodeID, classname)` rows (the helper-clash check;
/// `entity_io::live_targets` is the connection pre-flight's own private copy).
async fn find_by_targetname(
    state: &HammerState,
    deadline: u64,
    targetname: &str,
) -> Result<Vec<(i64, String)>, ToolError> {
    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(
            "entity.find",
            json!({"targetname": targetname}),
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
    Ok(reply
        .result
        .get("entities")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some((
                        row.get("nodeID").and_then(Value::as_i64)?,
                        row.get("classname")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default())
}

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "create_sound_entities",
            "Create sound entities in the LIVE map: `kind` point|sphere|alignedbox|orientedbox|\
             soundscape|soundscape_proxy, or any FGD tags(Sound) `class`. The soundevent name is \
             checked against the index first (a misspelt event is silent at runtime). Engine key \
             spellings are written for you; `schedule` builds a logic_auto (startDelay/stopDelay) \
             and/or a logic_timer (repeatEvery; first fire at repeatInitialDelay + repeatEvery). \
             Needs targetname with schedule/connections; soundscapes refuse schedule. Never saves.",
            Mutating,
            &["edit.batch", "entity.create"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "entities": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_ENTITIES,
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": {
                                "type": "string",
                                "enum": KIND_CLASS.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
                                "description": "exactly one of kind and class"
                            },
                            "class": {
                                "type": "string",
                                "description": "FGD tags(Sound) class (snd_event_path, ambient_generic, ...)"
                            },
                            "soundEvent": {
                                "type": "string",
                                "description": "soundevent name; required for point/soundscape kinds"
                            },
                            "legacySoundscape": {
                                "type": "string",
                                "description": "legacy .vsc name (soundscape family), unvalidated"
                            },
                            "origin": {
                                "type": "array", "items": {"type": "number"},
                                "minItems": 3, "maxItems": 3,
                                "description": "[x, y, z], required"
                            },
                            "angles": {
                                "type": "array", "items": {"type": "number"},
                                "minItems": 3, "maxItems": 3,
                                "description": "[pitch, yaw, roll]"
                            },
                            "targetname": {
                                "type": "string"
                            },
                            "startOnSpawn": {"type": "boolean"},
                            "toLocalPlayer": {"type": "boolean"},
                            "stopOnNew": {"type": "boolean"},
                            "sourceEntityName": {"type": "string"},
                            "radius": {
                                "type": "number",
                                "description": "only if the class has a radius key; soundscapes: integer >= 1 or -1"
                            },
                            "box": {
                                "type": "object",
                                "description": "box kinds only; default -64..64",
                                "properties": {
                                    "min": {"type": "array", "items": {"type": "number"},
                                            "minItems": 3, "maxItems": 3},
                                    "max": {"type": "array", "items": {"type": "number"},
                                            "minItems": 3, "maxItems": 3}
                                },
                                "required": ["min", "max"],
                                "additionalProperties": false
                            },
                            "positions": {
                                "type": "array",
                                "minItems": 1, "maxItems": MAX_POSITIONS,
                                "items": {"type": "string"},
                                "description": "soundscape family: targetnames for position0..7"
                            },
                            "connections": {
                                "type": "array",
                                "minItems": 1, "maxItems": MAX_CONNECTIONS,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "output": {"type": "string"},
                                        "target": {"type": "string"},
                                        "input": {"type": "string"},
                                        "param": {"type": "string"},
                                        "delay": {"type": "number"},
                                        "timesToFire": {"type": "integer", "minimum": -1}
                                    },
                                    "required": ["output", "target", "input"],
                                    "additionalProperties": false
                                },
                                "description": "I/O on this entity, checked against its FGD outputs"
                            },
                            "keyValues": {
                                "type": "object",
                                "description": "other FGD keyvalues; fields above are refused here"
                            },
                            "schedule": {
                                "type": "object",
                                "description": "timing helpers; needs targetname",
                                "properties": {
                                    "startDelay": {"type": "number", "minimum": 0},
                                    "stopDelay": {"type": "number"},
                                    "repeatEvery": {"type": "number"},
                                    "repeatInitialDelay": {"type": "number"}
                                },
                                "additionalProperties": false
                            }
                        },
                        "additionalProperties": false
                    }
                },
                "strict": {
                    "type": "boolean", "default": false,
                    "description": "refuse on FGD keyvalue errors"
                },
                "allowUnknownSoundEvent": {
                    "type": "boolean", "default": false,
                    "description": "warn instead of refusing unknown soundevents"
                },
                "undoLabel": {"type": "string"}
            },
            "required": ["entities"],
            "additionalProperties": false
        })),
        create_sound_entities,
    );
}
