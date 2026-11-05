//! `drain_sounds` (plan §1 "drain_sounds" + §5 item 4 "Position"): read and consume the bridge's
//! sound-event ring and decode the packed sound params into a world `position`.
//!
//! **The ring is the only history.** The bridge wraps the sound-op system on first use (m2.9,
//! `sound.status` installs it too) and records every start that goes through `ISoundOpSystem`
//! slots 11/12/13 into a 256-entry ring. `sound.drain` hands over what is there and *destroys* it
//! (the events that do not fit the bridge's 96 KB reply are requeued, `truncated:true`, the rest
//! overflow into `dropped`), so the tool cannot be idempotent and says so in its notes: call it
//! promptly after the moment you are observing, and again while `truncated` is true. A tool call
//! holds no write gate and reads no document — the ring is engine state, not Hammer state.
//!
//! **What the bridge reports** (`mcp_bridge_game.inc`, verified live in the Wine tools
//! runtime, R3 of `target/orch/re-snd.md`): per event `seq`, `hash`, `name` (resolved through
//! `ISoundEventManager` when the game has it), `entindex`, `guid`, `via` (the SOS slot: 11 for
//! `StartSoundEventWithParams`, 12/13/81 the other routes), `flags`, `ageMs` (measured against
//! the drain), plus — with `resolve:true` and a game scope — `origin`/`entityName`, and for slot
//! 81 `aux`/`inst150`. A `snd_event_point` started by `startOnSpawn` arrives as
//! `{via:11, entindex:0, entityName:"worldent", origin:[0,0,0]}`: the sound entities are
//! server-only, so the client-side wrap cannot name them and the *entity* origin is not the
//! event's position. `env_soundscape`/`snd_soundscape` with `enablesoundevent 1` shows the parent
//! event on round start and on each re-entry into the radius; its `soundevent_01` children start
//! inside the operator stack and are never seen.
//!
//! **Where the position comes from** (R3 "Position", [V live + V bin]). Slot 11's argument is a
//! `SosStartParams_t` whose `+0x08` is the server's packed-params buffer and `+0x1C` its length.
//! Bridge m2.20 (WP-B) copies at most 128 of those bytes into the ring entry as
//! `"params":{"len":N,"hex":"…","truncated":bool}` (`null` when the pointer is unreadable, absent
//! for slots 12/13/81), and this tool walks them: entries are
//! `u32 hash LE | u8 type | u16 size LE | data[size]`, and a `snd_event_*` carries
//! `4dce7c5a 0a 0c00 <f32 x><f32 y><f32 z>` — hash `0x5A7CCE4D` (`position`, the
//! `CC_snd_sos_start_soundevent_at_pos` param), type `0x0a`, size 12, the entity's Hammer origin
//! exactly. `params.decoded` is true **only** when the walk ends exactly at `len` and the buffer
//! was not truncated; anything else (a truncated copy, a walk that overruns, a `len` the bytes do
//! not match) keeps the entries that could be read whole but reports `decoded:false`, and
//! `position` stays `null` — a half-read buffer never invents a location.
//!
//! **"Old bridge" is said, not guessed** (review-rust M5): an m2.20 bridge always writes the
//! `params` key on a via-11 event — `null` when the pointer was unreadable — so three reply shapes
//! carry three meanings. On a via-11 event with the key **absent** (a bridge older than m2.20:
//! there never was a params copy) the event keeps `params:null`/`position:null` and the result
//! carries ONE drain note naming the build string the reply's envelope carries
//! ("bridge < m2.20 (build X): no packed params, positions unavailable"). `params:null` PRESENT on
//! a via-11 event (the m2.20 unreadable-pointer case) is marked per event as
//! `params:{"unreadable":true}` instead, with no bridge-age note — the params path existed, the
//! buffer did not. The other slots (12/13/81) never carry the key and are never noted.
//!
//! **Filtering is local and after the drain** (plan §1): `filter.namePrefix` / `filter.names`
//! (exact, ≤32) match the event name, and `filter.within` (a sphere: `center` + positive
//! `radius`) matches the event's decoded `position` first, else a non-null non-zero `origin`. An
//! event with neither can never match a `within` filter; it is *still consumed* and counted in
//! `unmatched`, and the result carries a note saying how many were like that. `matched` +
//! `unmatched` is the number of events this drain returned.
//!
//! **`installOnly:true`** sends `sound.status` instead and returns its object: that installs the
//! wrap without draining, which is the "arm the capture before `run_map`" step (the live R3 pass
//! caught a map load's `startOnSpawn` events that way). It sends nothing else — not even a
//! `sound.drain`. `filter` is refused with it (`InvalidArguments`, nothing sent): there is
//! nothing to filter, and silently ignoring it would hide a caller's mistake; `max`/`resolve` are
//! ignored with a note for the same reason. The *capability* gate is the registry's and it runs
//! before the handler, so `installOnly` needs the tool's declared `sound.drain` to be advertised
//! as well: the bridge answers `sound.status` and `sound.drain` from one `op_sound` handler
//! (`mcp_bridge_game.inc`), so the pair is never split in practice.
use crate::state::HammerState;
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{
    CallCtx, Mutability::Mutating, Registry, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Map, Value};

/// The tool this module registers (the last entry of `sounds::SOUND_TOOL_ORDER`).
pub const SOUND_RUNTIME_TOOL_ORDER: [&str; 1] = ["drain_sounds"];

/// `max`'s upper bound: the bridge's own ring size (`SND_RING`); a bigger value is refused rather
/// than silently clamped, because a clamp would make the result's `truncated`/`pending` lie.
pub const MAX_MAX: u64 = 256;
/// `filter.names`' cap (plan §1: "≤ 32").
pub const MAX_NAMES: usize = 32;

/// The field hash of the packed `position` param (`soundsystem`
/// `CC_snd_sos_start_soundevent_at_pos`, R3 [V bin]); the result spells it `0x5a7cce4d`.
const POSITION_HASH: u32 = 0x5A7C_CE4D;
/// One packed-params entry header: `u32 hash LE | u8 type | u16 size LE`, then `size` bytes.
const ENTRY_HEADER: usize = 7;
/// The type byte of a 3-float vector in the packed params (R3: `type 0x0a`, size 12).
const TYPE_VEC3: u8 = 10;
/// The `position` vector's byte size.
const VEC3_SIZE: usize = 12;

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "drain_sounds",
            "Read AND consume the sound events started since the last drain (name, entity, ageMs, \
             decoded params and world position). The ring holds 256 entries: drain promptly and \
             repeat while truncated is true. The local `filter` runs AFTER the drain, so \
             filtered-out events are still consumed (counted in unmatched). `installOnly:true` arms \
             the capture without draining: do it before run_map to catch startOnSpawn events.",
            Mutating,
            &["sound.drain"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "max": {
                    "type": "integer", "minimum": 1, "maximum": 256, "default": 256
                },
                "resolve": {
                    "type": "boolean",
                    "description": "default true; false skips the entity lookup (no origin)"
                },
                "filter": {
                    "type": "object",
                    "properties": {
                        "namePrefix": {
                            "type": "string"
                        },
                        "names": {
                            "type": "array", "items": {"type": "string"},
                            "minItems": 1, "maxItems": 32,
                            "description": "exact, case-sensitive"
                        },
                        "within": {
                            "type": "object",
                            "description": "sphere, matched on the decoded position, else origin",
                            "properties": {
                                "center": {
                                    "type": "array", "items": {"type": "number"},
                                    "minItems": 3, "maxItems": 3
                                },
                                "radius": {
                                    "type": "number", "exclusiveMinimum": 0
                                }
                            },
                            "required": ["center", "radius"],
                            "additionalProperties": false
                        }
                    },
                    "additionalProperties": false
                },
                "installOnly": {
                    "type": "boolean",
                    "description": "arm without draining; refuses filter"
                }
            },
            "additionalProperties": false
        })),
        drain_sounds,
    );
}

// ---------------------------------------------------------------- arguments

/// The validated `drain_sounds` arguments.
#[derive(Debug, Clone, PartialEq)]
struct DrainArgs {
    max: u64,
    resolve: bool,
    install_only: bool,
    /// `max` was given explicitly (only used for the `installOnly` note).
    max_given: bool,
    /// `resolve` was given explicitly (only used for the `installOnly` note).
    resolve_given: bool,
    filter: Option<EventFilter>,
}

/// The local, post-drain filter.
#[derive(Debug, Clone, PartialEq)]
struct EventFilter {
    name_prefix: Option<String>,
    names: Option<Vec<String>>,
    within: Option<Sphere>,
}

/// A `within` sphere.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Sphere {
    center: [f64; 3],
    radius: f64,
}

/// What the filter says about one event; [`Match::NoPoint`] is the "it passed the name filters
/// but carries neither a decoded position nor a non-null non-zero origin, so a `within` filter
/// can never match it" case the result carries a note for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match {
    Yes,
    No,
    NoPoint,
}

fn parse_args(args: &Value) -> Result<DrainArgs, ToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| ToolError::invalid_arguments("arguments must be an object"))?;
    if let Some(bad) = obj
        .keys()
        .find(|k| !["max", "resolve", "filter", "installOnly"].contains(&k.as_str()))
    {
        return Err(ToolError::invalid_arguments(format!(
            "unknown argument {bad:?} (max, resolve, filter, installOnly)"
        )));
    }
    let max_given = matches!(obj.get("max"), Some(v) if !v.is_null());
    let max = match obj.get("max") {
        None | Some(Value::Null) => MAX_MAX,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_MAX).contains(n))
            .ok_or_else(|| {
                ToolError::invalid_arguments(
                    "max: an integer in 1..=256 is required (0 = the \
                                              whole ring is the same as 256; a bigger value \
                                              cannot be satisfied)",
                )
            })?,
    };
    let resolve_given = matches!(obj.get("resolve"), Some(v) if !v.is_null());
    let resolve = match obj.get("resolve") {
        None | Some(Value::Null) => true,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| ToolError::invalid_arguments("resolve: boolean expected"))?,
    };
    let install_only = match obj.get("installOnly") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| ToolError::invalid_arguments("installOnly: boolean expected"))?,
    };
    let filter = match obj.get("filter") {
        None | Some(Value::Null) => None,
        Some(v) => {
            if install_only {
                return Err(ToolError::invalid_arguments(
                    "filter and installOnly are mutually exclusive: installOnly sends \
                     sound.status and drains nothing, so there is nothing to filter",
                ));
            }
            Some(parse_filter(v)?)
        }
    };
    Ok(DrainArgs {
        max,
        resolve,
        install_only,
        max_given,
        resolve_given,
        filter,
    })
}

fn parse_filter(value: &Value) -> Result<EventFilter, ToolError> {
    let obj = value
        .as_object()
        .ok_or_else(|| ToolError::invalid_arguments("filter: an object is required"))?;
    if let Some(bad) = obj
        .keys()
        .find(|k| !["namePrefix", "names", "within"].contains(&k.as_str()))
    {
        return Err(ToolError::invalid_arguments(format!(
            "filter: unknown argument {bad:?} (namePrefix, names, within)"
        )));
    }
    let name_prefix = match obj.get("namePrefix") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| {
                    ToolError::invalid_arguments("filter.namePrefix: a string is required")
                })?
                .to_string(),
        ),
    };
    let names = match obj.get("names") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let array = v.as_array().ok_or_else(|| {
                ToolError::invalid_arguments("filter.names: an array of strings is required")
            })?;
            if array.is_empty() || array.len() > MAX_NAMES {
                return Err(ToolError::invalid_arguments(format!(
                    "filter.names: 1..={MAX_NAMES} names are required, got {}",
                    array.len()
                )));
            }
            let mut out = Vec::with_capacity(array.len());
            for (index, entry) in array.iter().enumerate() {
                out.push(
                    entry
                        .as_str()
                        .ok_or_else(|| {
                            ToolError::invalid_arguments(format!(
                                "filter.names[{index}]: a string is required (names are matched \
                                 exactly, case-sensitive)"
                            ))
                        })?
                        .to_string(),
                );
            }
            Some(out)
        }
    };
    let within = match obj.get("within") {
        None | Some(Value::Null) => None,
        Some(v) => Some(parse_sphere(v)?),
    };
    Ok(EventFilter {
        name_prefix,
        names,
        within,
    })
}

fn parse_sphere(value: &Value) -> Result<Sphere, ToolError> {
    let obj = value
        .as_object()
        .ok_or_else(|| ToolError::invalid_arguments("filter.within: an object is required"))?;
    if let Some(bad) = obj
        .keys()
        .find(|k| !["center", "radius"].contains(&k.as_str()))
    {
        return Err(ToolError::invalid_arguments(format!(
            "filter.within: unknown argument {bad:?} (center, radius)"
        )));
    }
    let center = match obj.get("center") {
        None => {
            return Err(ToolError::invalid_arguments(
                "filter.within.center: [x, y, z] is required",
            ))
        }
        Some(v) => {
            let array = v.as_array().ok_or_else(|| {
                ToolError::invalid_arguments("filter.within.center: [x, y, z] is required")
            })?;
            let mut out = [0f64; 3];
            if array.len() != 3 {
                return Err(ToolError::invalid_arguments(format!(
                    "filter.within.center: exactly 3 numbers are required, got {}",
                    array.len()
                )));
            }
            for (axis, entry) in array.iter().enumerate() {
                out[axis] = entry.as_f64().filter(|n| n.is_finite()).ok_or_else(|| {
                    ToolError::invalid_arguments(format!(
                        "filter.within.center[{axis}]: a finite number is required"
                    ))
                })?;
            }
            out
        }
    };
    let radius = match obj.get("radius") {
        None => {
            return Err(ToolError::invalid_arguments(
                "filter.within.radius: required (> 0)",
            ))
        }
        Some(v) => v
            .as_f64()
            .filter(|r| r.is_finite() && *r > 0.0)
            .ok_or_else(|| {
                ToolError::invalid_arguments(
                    "filter.within.radius: a finite number > 0 is required",
                )
            })?,
    };
    Ok(Sphere { center, radius })
}

impl EventFilter {
    /// The name half of the filter (everything before `within`).
    fn name_passes(&self, name: Option<&str>) -> bool {
        if let Some(prefix) = &self.name_prefix {
            if !name.is_some_and(|n| n.starts_with(prefix.as_str())) {
                return false;
            }
        }
        if let Some(names) = &self.names {
            if !name.is_some_and(|n| names.iter().any(|wanted| wanted == n)) {
                return false;
            }
        }
        true
    }

    fn within_passes(
        &self,
        sphere: &Sphere,
        position: Option<[f64; 3]>,
        origin: Option<[f64; 3]>,
    ) -> bool {
        // the position first (R3/§5), else a non-null non-zero origin
        let point = position.or_else(|| origin.filter(|o| o != &[0.0, 0.0, 0.0]));
        let Some(point) = point else { return false };
        let dx = point[0] - sphere.center[0];
        let dy = point[1] - sphere.center[1];
        let dz = point[2] - sphere.center[2];
        dx * dx + dy * dy + dz * dz <= sphere.radius * sphere.radius
    }

    /// The verdict for one event, with `has_point` = it carries a decoded position or a non-null
    /// non-zero origin (so `NoPoint` only ever means "the sphere could not be applied").
    fn verdict(
        &self,
        name: Option<&str>,
        position: Option<[f64; 3]>,
        origin: Option<[f64; 3]>,
    ) -> Match {
        if !self.name_passes(name) {
            return Match::No;
        }
        let Some(sphere) = &self.within else {
            return Match::Yes;
        };
        if self.within_passes(sphere, position, origin) {
            return Match::Yes;
        }
        let has_point = position.is_some() || origin.is_some_and(|o| o != [0.0, 0.0, 0.0]);
        if has_point {
            Match::No
        } else {
            Match::NoPoint
        }
    }
}

// ---------------------------------------------------------------- the tool

/// `drain_sounds {max?, resolve?, filter?, installOnly?}`.
pub async fn drain_sounds(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let parsed = parse_args(&args)?;
    if parsed.install_only {
        return install_only(&state, ctx, &parsed).await;
    }
    // params first: everything is validated before anything is sent
    let mut params = Map::new();
    params.insert("max".into(), json!(parsed.max));
    params.insert("resolve".into(), json!(parsed.resolve));
    let resp = {
        // lock first: the deadline is computed from the budget the moment the request goes out
        let mut bridge = state.bridge.lock().await;
        let deadline = ctx.budget.deadline_ms()?;
        bridge
            .request(
                "sound.drain",
                Value::Object(params),
                RequestOpts::deadline(deadline),
            )
            .await?
    };
    // the reply's envelope names the build that answered ("bridge":{"build":…}, every bridge
    // reply carries it): quoted in the old-bridge note (review M5)
    let bridge_build = resp.build().unwrap_or("unknown").to_string();
    let result = match resp.result {
        Value::Object(m) => m,
        other => {
            return Err(ToolError::internal(format!(
                "sound.drain returned a non-object result: {other}"
            )))
        }
    };
    let events = result
        .get("events")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ToolError::internal(format!(
                "sound.drain result has no `events` array (expected \
                 {{events:[...], truncated, dropped, total, pending}}, got {}) — a reply without \
                 events is not an empty drain",
                shape_of(&result)
            ))
        })?;

    let mut matched = 0usize;
    let mut unmatched = 0usize;
    let mut no_point = 0usize;
    let mut old_bridge_events = 0usize;
    let mut out_events: Vec<Value> = Vec::with_capacity(events.len());
    for (index, raw) in events.iter().enumerate() {
        let event = raw.as_object().ok_or_else(|| {
            ToolError::internal(format!(
                "sound.drain event #{index} is not an object: {raw}"
            ))
        })?;
        // every bridge field passes through untouched; `params` is replaced by its decoded form
        let mut event = event.clone();
        let via11 = event.get("via").and_then(Value::as_i64) == Some(11);
        // Three meanings (review M5): the key ABSENT on a via-11 event = the bridge has no packed
        // params at all (older than m2.20, counted once below); present-but-`null` = the m2.20
        // unreadable-pointer case, marked per event so it never reads as "old bridge"; anything
        // else is decoded. An absent key on the other slots is the ordinary shape, never counted.
        let params = match event.remove("params") {
            Some(Value::Null) => DecodedParams {
                json: json!({"unreadable": true}),
                position: None,
            },
            Some(raw) => decode_params(&raw),
            None => {
                if via11 {
                    old_bridge_events += 1;
                }
                DecodedParams::default()
            },
        };
        let position = params.position;
        event.insert(
            "position".into(),
            position.map(|p| json!(p)).unwrap_or(Value::Null),
        );
        event.insert("params".into(), params.json);
        let origin = event.get("origin").and_then(vec3_of);
        let name = event
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        let verdict = match &parsed.filter {
            None => Match::Yes,
            Some(filter) => filter.verdict(name.as_deref(), position, origin),
        };
        match verdict {
            Match::Yes => matched += 1,
            Match::No => unmatched += 1,
            Match::NoPoint => {
                unmatched += 1;
                no_point += 1;
            }
        }
        if verdict == Match::Yes {
            out_events.push(Value::Object(event));
        }
    }

    let truncated = result
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let dropped = result.get("dropped").and_then(Value::as_i64).unwrap_or(0);
    let total = result.get("total").and_then(Value::as_i64).unwrap_or(0);
    let pending = result.get("pending").and_then(Value::as_i64).unwrap_or(0);

    let mut notes = vec![
        "ageMs is measured at this drain, not at the event's start".to_string(),
        "the ring holds 256 events: drain promptly after the expected moment, or the oldest are \
         dropped"
            .to_string(),
        "call drain_sounds again while truncated is true: the events that did not fit the reply \
         stay queued"
            .to_string(),
    ];
    if truncated {
        notes.push(
            "truncated: the bridge's reply cap cut this batch short; the remaining events were \
             requeued at the front of the ring — call again to get them (do not re-run the map)"
                .to_string(),
        );
    }
    if dropped != 0 {
        notes.push(format!(
            "dropped: {dropped} event(s) overflowed the 256-entry ring since the last drain (a \
             drain that is too late loses the oldest)"
        ));
    }
    if !parsed.resolve {
        notes.push(
            "resolve:false — the bridge reported no origins and entity names; a within filter can \
             then only match an event's decoded position"
                .to_string(),
        );
    }
    if no_point != 0 {
        notes.push(format!(
            "within: {no_point} event(s) had no decoded position and no non-null non-zero origin, \
             so they cannot match a within filter (they are consumed and counted in unmatched)"
        ));
    }
    if old_bridge_events != 0 {
        notes.push(format!(
            "bridge < m2.20 (build {bridge_build}): no packed params, positions unavailable \
             ({old_bridge_events} via-11 event(s) came back without a params key)"
        ));
    }

    Ok(json!({
        "events": out_events,
        "matched": matched,
        "unmatched": unmatched,
        "total": total,
        "dropped": dropped,
        "truncated": truncated,
        "pending": pending,
        "notes": notes,
    })
    .into())
}

/// The `installOnly` half: `sound.status` (which installs the wrap) with no drain and no other
/// request. Its object passes through; `installOnly:true` and `notes` are added.
async fn install_only(
    state: &HammerState,
    ctx: CallCtx,
    parsed: &DrainArgs,
) -> Result<ToolOutput, ToolError> {
    let resp = {
        let mut bridge = state.bridge.lock().await;
        let deadline = ctx.budget.deadline_ms()?;
        bridge
            .request("sound.status", json!({}), RequestOpts::deadline(deadline))
            .await?
    };
    let mut out = match resp.result {
        Value::Object(m) => m,
        other => {
            return Err(ToolError::internal(format!(
                "sound.status returned a non-object result: {other}"
            )))
        }
    };
    let installed = out
        .get("installed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut notes = vec![
        "sound.status installed the sound-op wrap without draining: events started from now on \
         are captured and sound.drain hands them over (the ring was not touched)"
            .to_string(),
        "this is the arm-before-run_map step: the load's startOnSpawn events are in the ring \
         afterwards"
            .to_string(),
    ];
    if !installed {
        notes.push(
            "the bridge reports installed:false — the wrap is NOT armed (the soundsystem pins, \
             -32017 PrerequisiteFailed, are the usual cause)"
                .to_string(),
        );
    }
    if parsed.max_given || parsed.resolve_given {
        notes.push(
            "installOnly: max/resolve are ignored — nothing is drained (they are not an error, \
             just meaningless here)"
                .to_string(),
        );
    }
    out.insert("installOnly".into(), json!(true));
    out.insert("notes".into(), json!(notes));
    Ok(Value::Object(out).into())
}

// ---------------------------------------------------------------- packed params

/// One event's `params`, decoded: the JSON the result carries plus the position the `within`
/// filter uses (which is only trustworthy when the JSON's `decoded` is true).
#[derive(Debug, Clone, Default)]
struct DecodedParams {
    json: Value,
    position: Option<[f64; 3]>,
}

/// Decode the bridge's `params` object (`{len, hex, truncated}`) into
/// `{len, bytes, truncated, decoded, fields:[{hash, type, size, name?, value?}]}`.
///
/// The walk is `u32 hash LE | u8 type | u16 size LE | data[size]` (R3, verified against the live
/// buffer: 19 bytes = `4dce7c5a 0a 0c00` + 12 float bytes). `decoded` is true only when the walk
/// consumed the whole buffer **and** that buffer is `len` bytes and not a truncated copy; the
/// entries read whole before a cut are still reported (they say what the buffer held), but the
/// position is dropped: a partial buffer never invents a location.
fn decode_params(raw: &Value) -> DecodedParams {
    let Some(obj) = raw.as_object() else {
        return DecodedParams {
            json: Value::Null,
            ..Default::default()
        };
    };
    let len = obj.get("len").and_then(Value::as_u64).unwrap_or(0);
    let truncated = obj
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let bytes = parse_hex(obj.get("hex").and_then(Value::as_str).unwrap_or("")).unwrap_or_default();
    let walked = walk(&bytes);
    let decoded = walked.complete && !truncated && bytes.len() as u64 == len;
    DecodedParams {
        json: json!({
            "len": len,
            "bytes": bytes.len(),
            "truncated": truncated,
            "decoded": decoded,
            "fields": walked.fields,
        }),
        position: walked.position.filter(|_| decoded),
    }
}

/// What a walk of the packed buffer found.
struct Walked {
    fields: Vec<Value>,
    /// The first `position`-hashed vec3 field, whatever the walk's completeness.
    position: Option<[f64; 3]>,
    /// True when the buffer ended exactly on an entry boundary (no cut entry, no header tail).
    complete: bool,
}

fn walk(bytes: &[u8]) -> Walked {
    let mut fields = Vec::new();
    let mut position = None;
    let mut offset = 0usize;
    loop {
        if offset == bytes.len() {
            return Walked {
                fields,
                position,
                complete: true,
            };
        }
        // an entry header or its data does not fit: the buffer is cut, keep what was read whole
        if bytes.len() - offset < ENTRY_HEADER {
            return Walked {
                fields,
                position,
                complete: false,
            };
        }
        let hash = u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]);
        let type_ = bytes[offset + 4];
        let size = u16::from_le_bytes([bytes[offset + 5], bytes[offset + 6]]) as usize;
        let data_start = offset + ENTRY_HEADER;
        if bytes.len() - data_start < size {
            return Walked {
                fields,
                position,
                complete: false,
            };
        }
        let data = &bytes[data_start..data_start + size];
        if hash == POSITION_HASH && type_ == TYPE_VEC3 {
            if let Some(vector) = vec3_of_bytes(data) {
                position.get_or_insert(vector);
            }
        }
        fields.push(field_json(hash, type_, size, data));
        offset = data_start + size;
    }
}

/// One field as the result spells it: the hash as `0x…`, the raw type byte and size, the semantic
/// `name` when the hash is known, and a `value` — the float vector when the entry is a
/// type-10/size-12 vec3, the raw bytes as `0x…` for every other kind (a non-finite float falls
/// back to the hex spelling too).
fn field_json(hash: u32, type_: u8, size: usize, data: &[u8]) -> Value {
    let mut field = Map::new();
    field.insert("hash".into(), json!(format!("0x{hash:08x}")));
    field.insert("type".into(), json!(type_));
    field.insert("size".into(), json!(size));
    if hash == POSITION_HASH {
        field.insert("name".into(), json!("position"));
    }
    let value = match (type_, size) {
        (TYPE_VEC3, VEC3_SIZE) => match vec3_of_bytes(data) {
            Some(vector) => json!(vector),
            None => json!(hex_text(data)),
        },
        _ => json!(hex_text(data)),
    };
    field.insert("value".into(), value);
    Value::Object(field)
}

/// A 3-float vector from exactly 12 bytes; `None` when the data is another size or any component
/// is not finite (JSON has no NaN/∞, and a bogus float is not a position).
fn vec3_of_bytes(data: &[u8]) -> Option<[f64; 3]> {
    if data.len() != VEC3_SIZE {
        return None;
    }
    let mut out = [0f64; 3];
    for (axis, slot) in out.iter_mut().enumerate() {
        let chunk = &data[axis * 4..axis * 4 + 4];
        let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64;
        if !value.is_finite() {
            return None;
        }
        *slot = value;
    }
    Some(out)
}

/// A `[x, y, z]` JSON array of finite numbers (the bridge's `origin`, which may be `null`).
fn vec3_of(value: &Value) -> Option<[f64; 3]> {
    let array = value.as_array()?;
    if array.len() != 3 {
        return None;
    }
    let mut out = [0f64; 3];
    for (axis, entry) in array.iter().enumerate() {
        out[axis] = entry.as_f64().filter(|n| n.is_finite())?;
    }
    Some(out)
}

/// Hex text (an optional `0x`, either case) to bytes; `None` for an odd length or a non-hex byte.
fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let text = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        out.push(hex_digit(pair[0])? << 4 | hex_digit(pair[1])?);
    }
    Some(out)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Bytes to the `0x…` lowercase hex the result spells raw field data with.
fn hex_text(data: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(2 + data.len() * 2);
    out.push_str("0x");
    for byte in data {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The key list of a reply's object, for an error message about a shape we did not get.
fn shape_of(object: &Map<String, Value>) -> String {
    let keys: Vec<&str> = object.keys().map(String::as_str).collect();
    if keys.is_empty() {
        "an object with no keys".to_string()
    } else {
        format!("keys {}", keys.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live captured buffer (R3): hash `0x5A7CCE4D`, type 0x0a, size 12, (944, 160, 64).
    const CAPTURED: &str = "4dce7c5a0a0c0000006c440000204300008042";

    fn decode(len: u64, hex: &str, truncated: bool) -> DecodedParams {
        decode_params(&json!({"len": len, "hex": hex, "truncated": truncated}))
    }

    /// The `decoded` flag the result carries (the struct keeps only the position beside it).
    fn is_decoded(decoded: &DecodedParams) -> bool {
        decoded.json["decoded"] == json!(true)
    }

    #[test]
    fn the_captured_position_buffer_decodes() {
        let decoded = decode(19, CAPTURED, false);
        assert!(is_decoded(&decoded));
        assert_eq!(decoded.position, Some([944.0, 160.0, 64.0]));
        assert_eq!(
            decoded.json["fields"],
            json!([{
                "hash": "0x5a7cce4d", "type": 10, "size": 12,
                "name": "position", "value": [944.0, 160.0, 64.0],
            }])
        );
        assert_eq!(decoded.json["len"], json!(19));
        assert_eq!(decoded.json["bytes"], json!(19));
        assert_eq!(decoded.json["truncated"], json!(false));
    }

    #[test]
    fn a_multi_entry_buffer_walks_every_entry_and_names_only_position() {
        // position + an unknown 2-byte field (type 0x13; the hash is little-endian on the wire,
        // so the bytes `ddccbbaa` are the number 0xaabbccdd)
        let hex = format!("{CAPTURED}ddccbbaa130200ff00");
        let decoded = decode(28, &hex, false);
        assert!(is_decoded(&decoded), "{:?}", decoded.json);
        assert_eq!(decoded.position, Some([944.0, 160.0, 64.0]));
        let fields = decoded.json["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[1]["hash"], json!("0xaabbccdd"));
        assert_eq!(fields[1]["type"], json!(19));
        assert_eq!(fields[1]["size"], json!(2));
        assert_eq!(fields[1]["value"], json!("0xff00"));
        assert!(
            fields[1].get("name").is_none(),
            "only the position is named"
        );
    }

    #[test]
    fn a_truncated_copy_is_never_decoded_even_when_the_walk_fits() {
        let decoded = decode(200, CAPTURED, true);
        assert!(!is_decoded(&decoded));
        assert_eq!(decoded.position, None);
        assert_eq!(decoded.json["truncated"], json!(true));
        assert_eq!(
            decoded.json["fields"].as_array().unwrap().len(),
            1,
            "what was copied whole is still reported"
        );
    }

    #[test]
    fn a_walk_overrun_keeps_the_whole_entries_and_drops_the_position() {
        // the header claims 32 bytes of data but only 12 follow
        let decoded = decode(19, "4dce7c5a0a200000006c440000204300008042", false);
        assert!(!is_decoded(&decoded));
        assert_eq!(decoded.position, None);
        assert!(decoded.json["fields"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_len_mismatch_is_not_decoded() {
        // the bridge says 25, the bytes are 19 (and the walk completes at 19)
        let decoded = decode(25, CAPTURED, false);
        assert!(!is_decoded(&decoded));
        assert_eq!(decoded.position, None);
        assert_eq!(decoded.json["bytes"], json!(19));
    }

    #[test]
    fn an_undecodable_hex_is_reported_as_no_bytes() {
        for hex in ["4dce7c5a0", "4dce7c5azz0c0000006c44", ""] {
            let decoded = decode(19, hex, false);
            assert!(!is_decoded(&decoded), "{hex}");
            assert_eq!(decoded.position, None);
            assert_eq!(decoded.json["bytes"], json!(0));
        }
    }

    #[test]
    fn a_null_params_is_null() {
        let decoded = decode_params(&Value::Null);
        assert_eq!(decoded.json, Value::Null);
        assert!(!is_decoded(&decoded));
        assert_eq!(decoded.position, None);
    }

    #[test]
    fn a_non_finite_float_falls_back_to_hex_instead_of_inventing_a_position() {
        // +inf in the z lane (0x7f800000): the vector is not finite, so no position is invented
        let hex = "4dce7c5a0a0c0000006c44000020430000807f";
        let decoded = decode(19, hex, false);
        assert!(is_decoded(&decoded), "the walk itself is exact");
        assert_eq!(decoded.position, None);
        assert_eq!(
            decoded.json["fields"][0]["value"],
            json!("0x00006c44000020430000807f")
        );
    }

    #[test]
    fn the_sphere_filters_on_position_first_then_a_non_zero_origin() {
        let sphere = Sphere {
            center: [0.0, 0.0, 0.0],
            radius: 100.0,
        };
        let filter = EventFilter {
            name_prefix: None,
            names: None,
            within: Some(sphere),
        };
        // position decides even when the origin is outside
        assert_eq!(
            filter.verdict(Some("a"), Some([10.0, 0.0, 0.0]), Some([900.0, 0.0, 0.0])),
            Match::Yes
        );
        assert_eq!(
            filter.verdict(Some("a"), Some([900.0, 0.0, 0.0]), Some([10.0, 0.0, 0.0])),
            Match::No
        );
        // no position: a non-zero origin is used
        assert_eq!(
            filter.verdict(Some("a"), None, Some([10.0, 0.0, 0.0])),
            Match::Yes
        );
        assert_eq!(
            filter.verdict(Some("a"), None, Some([900.0, 0.0, 0.0])),
            Match::No
        );
        // neither (no origin, or the worldent's [0,0,0]) can never match
        assert_eq!(filter.verdict(Some("a"), None, None), Match::NoPoint);
        assert_eq!(
            filter.verdict(Some("a"), None, Some([0.0, 0.0, 0.0])),
            Match::NoPoint
        );
    }

    #[test]
    fn the_name_filters_decide_before_the_unnamed_within_case() {
        let filter = EventFilter {
            name_prefix: Some("mcp.".into()),
            names: None,
            within: Some(Sphere {
                center: [0.0; 3],
                radius: 10.0,
            }),
        };
        // a name that fails the prefix is a plain miss, not a "cannot be located" one
        assert_eq!(filter.verdict(Some("other.a"), None, None), Match::No);
        assert_eq!(filter.verdict(None, None, None), Match::No);
    }

    #[test]
    fn parse_args_defaults_and_bounds() {
        let parsed = parse_args(&json!({})).unwrap();
        assert_eq!(parsed.max, MAX_MAX);
        assert!(parsed.resolve && !parsed.install_only && parsed.filter.is_none());
        assert!(!parsed.max_given && !parsed.resolve_given);
        assert!(parse_args(&json!({"max": 1, "resolve": false})).is_ok());
        assert!(parse_args(&json!({"max": 0})).is_err());
        assert!(parse_args(&json!({"max": 257})).is_err());
        assert!(parse_args(&json!({"max": 1.5})).is_err());
        assert!(parse_args(&json!({"max": "8"})).is_err());
        assert!(parse_args(&json!({"nope": 1})).is_err());
        assert!(parse_args(&json!({"filter": {"nope": 1}})).is_err());
        assert!(parse_args(&json!({"filter": {"names": vec!["a"; 33]}})).is_err());
        assert!(
            parse_args(&json!({"filter": {"within": {"center": [0, 0, 0], "radius": 0}}})).is_err()
        );
        assert!(parse_args(&json!({
            "filter": {"within": {"center": [0, 0, 0], "radius": 1}},
            "installOnly": true
        }))
        .is_err());
    }
}
