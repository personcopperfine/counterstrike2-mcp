//! `setpos`: move the local player with `setpos_exact` (and `setang_exact`) over console.exec, then
//! verify in-process: `run_wait` with the `at` predicate of game_wait, under the bridge lock this
//! tool holds. Mutating (sends console commands). Refuses before sending, with PrerequisiteFailed,
//! when there is no live local player or an input.set angle override is active. The input hook's
//! buttons are left alone (no input.clear). Once the command is sent, every error is not retry-safe.
//!
//! `input_sequence`: a list of input.set steps run back to back under one bridge lock. Each step is
//! held for `holdTicks` game ticks, measured on game.local `tick` samples (`run_wait` with the `tick`
//! predicate), never wall time. `input.clear` is always sent once the first step may have started.
//!
//! `fire_weapon`: taps attack once per shot; each shot waits for clip1 to drop (the `ammo` predicate
//! with the caller's clip1 + entindex baseline, so a shot that lands before the first sample counts).
//! Sound events from sound.drain are reported as evidence. `look_at` aims the view with an
//! input.set angle override (`aim_angles` from the eye), `use_entity` aims, taps use, and reports
//! evidence. `ent_fire` sends `ent_fire <target> <input> [value] [delay]` through console.exec with
//! the confirmation marker. Every input these tools set is undone on any error after the first send.
use crate::console::resolve::{resolve_within, Purpose};
use crate::console::{ConsoleError, ConsoleLog};
use crate::input::InputSet;
use crate::state::ClientState;
use crate::tools::bridge_tools::{wire_angles, PITCH_LIMIT};
use crate::tools::console::console_error;
use crate::tools::game_state::is_weapon_name;
use crate::tools::game_wait::{
    distance, parse_request, player_of, run_wait, run_wait_settle, sample_of, to_target, vec3,
    view, AmmoFrom, AtSpec, FloorSpec, Predicate, RoundBase, Sample, WaitEnd, WaitRequest,
    SAMPLE_SLACK_MS,
};
use crate::tools::session::{exec_console, exec_console_confirmed};
use cs2_mcp_common::bridge::{BridgeClient, RequestOpts};
use cs2_mcp_common::tools::{CallCtx, Mutability, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};
use std::time::{Duration, Instant};

const KEYS: [&str; 9] = [
    "origin",
    "angles",
    "exact",
    "floor",
    "minZ",
    "tolerance",
    "zTolerance",
    "angleTolerance",
    "timeoutMs",
];
/// Coordinates beyond this are outside the map (|v| <= 32768).
const COORD_LIMIT: f64 = 32768.0;
/// The wait's own margin: game_wait keeps this much of the budget for the reply.
const WAIT_MARGIN: Duration = Duration::from_millis(500);
/// Budget needed for the wait's first sample (its bridge deadline) on top of the margin.
const ONE_SAMPLE: Duration = Duration::from_millis(SAMPLE_SLACK_MS);
/// Speed (units/s) above which a player counts as moving before a setpos.
const STOP_SPEED: f64 = 1.0;
/// Longest wait for a moving player to stop before the setpos command is sent.
const STOP_WAIT_MAX: Duration = Duration::from_millis(1000);
/// Re-sends of the setpos command when the player comes to rest off the target.
const MAX_RESENDS: u64 = 2;
/// Smallest tolerance the wait uses. The command is printed to 3 decimals, so the target is exact
/// to 0.001; a zero tolerance would only ever match by luck of the float printing.
const MIN_TOLERANCE: f64 = 0.01;

/// Validated `setpos` arguments: the console command and the wait that verifies it. The origin and
/// angles are the rounded, wire-safe values the command carries; the wait targets the same ones.
pub(crate) struct SetposRequest {
    pub(crate) origin: [f64; 3],
    pub(crate) angles: Option<[f64; 3]>,
    pub(crate) exact: bool,
    /// verify the landing (`Predicate::Floor`) instead of standing at origin.z
    pub(crate) floor: bool,
    pub(crate) command: String,
    pub(crate) wait: WaitRequest,
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(message)
}

/// Round to the 3 decimals the console command is printed with (and fold -0.0 to 0.0).
fn round3(x: f64) -> f64 {
    let r = (x * 1000.0).round() / 1000.0;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

/// Format one coordinate triple for the console: `x y z`, three decimals.
fn triple(v: [f64; 3]) -> String {
    format!("{:.3} {:.3} {:.3}", v[0], v[1], v[2])
}

/// Validate `setpos` arguments. Nothing is sent.
pub(crate) fn parse_setpos(args: &Value) -> Result<SetposRequest, ToolError> {
    let obj = match args {
        Value::Object(m) => m,
        _ => return Err(invalid("arguments must be an object")),
    };
    if let Some(k) = obj.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(invalid(format!(
            "unknown key {k:?} in setpos (allowed: {})",
            KEYS.join(", ")
        )));
    }
    let raw = obj
        .get("origin")
        .and_then(vec3)
        .ok_or_else(|| invalid("origin must be [x, y, z] (three finite numbers)"))?;
    if raw.iter().any(|c| c.abs() > COORD_LIMIT) {
        return Err(invalid(format!(
            "origin coordinates must satisfy |v| <= {COORD_LIMIT}"
        )));
    }
    let origin = raw.map(round3);
    let angles = match obj.get("angles") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let a = vec3(v)
                .ok_or_else(|| invalid("angles must be [pitch, yaw, roll] (finite numbers)"))?;
            Some(wire_angles(a, "setpos")?)
        }
    };
    let exact = match obj.get("exact") {
        None | Some(Value::Null) => true,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| invalid("exact must be true or false"))?,
    };
    let floor = match obj.get("floor") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| invalid("floor must be true or false"))?,
    };
    let min_z = match obj.get("minZ") {
        None | Some(Value::Null) => None,
        Some(v) => {
            if !floor {
                return Err(invalid("minZ needs floor: true"));
            }
            let m = v
                .as_f64()
                .filter(|m| m.is_finite() && m.abs() <= COORD_LIMIT)
                .ok_or_else(|| invalid(format!("minZ must be a number, |v| <= {COORD_LIMIT}")))?;
            if m > origin[2] {
                return Err(invalid("minZ must not be above origin z"));
            }
            Some(round3(m))
        }
    };
    // the wait is game_wait's `at` predicate: same keys, same validation, nothing sent
    let mut at = Map::new();
    at.insert("until".into(), json!("at"));
    at.insert("origin".into(), json!(origin));
    if let Some(a) = angles {
        at.insert("angles".into(), json!(a));
    }
    for key in ["tolerance", "zTolerance", "angleTolerance", "timeoutMs"] {
        if let Some(v) = obj.get(key) {
            at.insert(key.into(), v.clone());
        }
    }
    let mut wait = parse_request(&Value::Object(at))?;
    if let Predicate::At(t) = &mut wait.predicate {
        t.tolerance = t.tolerance.max(MIN_TOLERANCE);
        t.z_tolerance = t.z_tolerance.max(MIN_TOLERANCE);
        t.angle_tolerance = t.angle_tolerance.max(MIN_TOLERANCE);
    }
    if floor {
        let at = at_of(&wait)?.clone();
        wait.predicate = Predicate::Floor(FloorSpec { at, min_z });
    }
    let (set, ang) = if exact {
        ("setpos_exact", "setang_exact")
    } else {
        ("setpos", "setang")
    };
    let mut command = format!("{set} {}", triple(origin));
    if let Some(a) = angles {
        command.push_str(&format!("; {ang} {}", triple(a)));
    }
    Ok(SetposRequest {
        origin,
        angles,
        exact,
        floor,
        command,
        wait,
    })
}

/// The target of a setpos wait: the `at` spec, or the floor predicate's.
fn at_of(req: &WaitRequest) -> Result<&AtSpec, ToolError> {
    match &req.predicate {
        Predicate::At(t) => Ok(t),
        Predicate::Floor(f) => Ok(&f.at),
        _ => Err(ToolError::internal("setpos wait is not an at predicate")),
    }
}

/// Once the command is sent, an error is never retry-safe: the caller must look before acting again.
fn after_send(e: ToolError, command: &str) -> ToolError {
    let mut e = e.retry_safe(false);
    e.data.insert("commandsSent".into(), json!(true));
    e.data.insert("command".into(), json!(command));
    e
}

/// A hint when the origin was reached on a timeout but the view was not: an override or a clamp.
fn angle_hint(t: &AtSpec, to: &Value) -> Option<&'static str> {
    t.angles?;
    let at_origin = to["xy"].as_f64().is_some_and(|d| d <= t.tolerance)
        && to["z"].as_f64().is_some_and(|d| d <= t.z_tolerance);
    let view_ok = match (to["angle"]["pitch"].as_f64(), to["angle"]["yaw"].as_f64()) {
        (Some(p), Some(y)) => p.abs() <= t.angle_tolerance && y.abs() <= t.angle_tolerance,
        _ => false,
    };
    (at_origin && !view_ok).then_some(
        "origin reached but the view did not settle: an input angle override or a view clamp may be in effect; input_clear, then retry",
    )
}

pub async fn setpos(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let req = parse_setpos(&args)?;
    let mut bridge = state.bridge.lock().await;
    setpos_checked(&mut bridge, &ctx, &req)
        .await
        .map(Into::into)
}

/// The setpos path under a bridge lock the caller holds (game_snapshot's restore reuses it): refuse
/// before any console command, send the command, then wait in-process for `at`. The reply is the
/// setpos reply (`after`, `toTarget`, ...). Errors once the command is sent are not retry-safe.
pub(crate) async fn setpos_checked(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    req: &SetposRequest,
) -> Result<Value, ToolError> {
    let target = at_of(&req.wait)?.clone();
    // refuse before any console command: not in game or dead is PrerequisiteFailed
    let pre = bridge
        .request(
            "game.local",
            json!({}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await?
        .result;
    player_of(&pre, false)?;
    let pre_sample = sample_of(&pre, None, false)?;
    let before = view(&pre_sample);
    let pre_origin = pre_sample.origin;
    let pre_speed = pre_sample.velocity.map(|v| distance(v, [0.0; 3]));
    // an input.set angle override wins over setang every tick: refuse rather than wait for nothing
    if req.angles.is_some() && bridge.has("input.status") {
        let status = bridge
            .request(
                "input.status",
                json!({}),
                RequestOpts::deadline(ctx.budget.deadline_ms()?),
            )
            .await?
            .result;
        if status.get("angles").is_some_and(|a| !a.is_null()) {
            return Err(ToolError::new(
                "PrerequisiteFailed",
                "PrerequisiteFailed",
                "input angle override active; input_clear first (nothing was sent)",
            )
            .retry_safe(true)
            .with_data(json!({"inputAngles": status["angles"], "commandsSent": false})));
        }
    }
    // the command and a wait that can use one sample must both fit: fail retry-safe before sending
    if ctx.budget.remaining() < WAIT_MARGIN + ONE_SAMPLE {
        return Err(ToolError::budget_exceeded(&ctx.budget)
            .retry_safe(true)
            .with_data(json!({"commandsSent": false})));
    }
    // setpos keeps the velocity: a player still moving after the teleport slides on past the
    // target. A moving player is first waited (bounded) to a stop, so the teleport lands at rest.
    let mut waited_for_stop = false;
    if pre_speed.is_some_and(|v| v > STOP_SPEED) {
        let stop_ms = STOP_WAIT_MAX
            .min(
                ctx.budget
                    .remaining()
                    .saturating_sub(WAIT_MARGIN + ONE_SAMPLE),
            )
            .as_millis() as u64;
        if stop_ms > 0 {
            let stop = parse_request(&json!({"until": "stopped", "timeoutMs": stop_ms}))?;
            let out = run_wait(bridge, ctx, &stop).await?;
            waited_for_stop = matches!(out.end, WaitEnd::Matched { .. });
        }
        if ctx.budget.remaining() < WAIT_MARGIN + ONE_SAMPLE {
            return Err(ToolError::budget_exceeded(&ctx.budget)
                .retry_safe(true)
                .with_data(json!({"commandsSent": false})));
        }
    }
    exec_console(bridge, ctx, &req.command).await?;
    // The wait runs under the one timeout. A player that comes to rest off the target gets the
    // command again (at most MAX_RESENDS times), since the teleport did not hold.
    let started = Instant::now();
    let total = Duration::from_millis(req.wait.timeout_ms);
    let mut resends: u64 = 0;
    let mut samples: u64 = 0;
    let out = loop {
        let mut attempt_req = req.wait.clone();
        attempt_req.timeout_ms = total.saturating_sub(started.elapsed()).as_millis().max(1) as u64;
        let rest_from = if resends < MAX_RESENDS {
            pre_origin
        } else {
            None
        };
        let attempt = match run_wait_settle(bridge, ctx, &attempt_req, rest_from).await {
            Ok(out) => out,
            Err(e) => return Err(after_send(e, &req.command)),
        };
        samples += attempt.samples;
        let resend_blocked =
            started.elapsed() >= total || ctx.budget.remaining() < WAIT_MARGIN + ONE_SAMPLE;
        if !attempt.rested || resend_blocked {
            break attempt;
        }
        if let Err(e) = exec_console(bridge, ctx, &req.command).await {
            return Err(after_send(e, &req.command));
        }
        resends += 1;
    };
    let mut out = out;
    out.samples = samples;
    out.waited = started.elapsed();
    let angles = match req.angles {
        Some(a) => json!(a),
        None => Value::Null,
    };
    match out.end {
        WaitEnd::Matched { sample, .. } => {
            let mut reply = json!({
                "command": req.command,
                "origin": req.origin,
                "angles": angles,
                "exact": req.exact,
                "before": before,
                "after": view(&sample),
                "toTarget": to_target(&target, &sample),
                "waitedMs": out.waited.as_millis() as u64,
                "samples": out.samples,
                "clampedToBudget": out.clamped,
                "resends": resends,
                "waitedForStop": waited_for_stop,
            });
            if req.floor {
                // matched by the floor predicate, which always has the origin
                let landed = sample.origin.map_or(Value::Null, |o| json!(o[2]));
                reply["floor"] = json!(true);
                reply["landedZ"] = landed.clone();
                reply["dropped"] = match landed.as_f64() {
                    Some(z) => json!(req.origin[2] - z),
                    None => Value::Null,
                };
                reply["minZ"] = match &req.wait.predicate {
                    Predicate::Floor(f) => json!(f.min_z),
                    _ => Value::Null,
                };
            }
            Ok(reply)
        }
        WaitEnd::Expired { last, bridge_error } => {
            let last_json = last.as_ref().map(|s| {
                let mut v = view(s);
                v["toTarget"] = to_target(&target, s);
                v
            });
            let hint = last_json
                .as_ref()
                .and_then(|v| angle_hint(&target, &v["toTarget"]));
            let mut data = json!({
                "command": req.command,
                "origin": req.origin,
                "angles": angles,
                "until": "at",
                "waitedMs": out.waited.as_millis() as u64,
                "timeoutMs": out.timeout.as_millis() as u64,
                "requestedTimeoutMs": req.wait.timeout_ms,
                "clampedToBudget": out.clamped,
                "samples": out.samples,
                "resends": resends,
                "waitedForStop": waited_for_stop,
                "before": before,
                "last": last_json,
                // sent already: the position may still be changing, so a retry is not safe
                "commandsSent": true,
            });
            if let Some(h) = hint {
                data["hint"] = json!(h);
            }
            let message = match &bridge_error {
                Some(e) => {
                    data["reason"] = json!("bridgeQueueTimeout");
                    data["bridgeError"] =
                        json!({"name": e.name, "code": e.code, "message": e.message});
                    format!(
                        "bridge main thread did not serve game.local; setpos ended early after {} samples ({} ms)",
                        out.samples,
                        out.waited.as_millis()
                    )
                }
                None => {
                    data["reason"] = json!("timeout");
                    format!(
                        "player did not reach the setpos target within {} ms ({} samples); {} was sent",
                        out.waited.as_millis(),
                        out.samples,
                        req.command
                    )
                }
            };
            let message = match hint {
                Some(h) => format!("{message}. {h}"),
                None => message,
            };
            Err(ToolError::new("WaitTimeout", "WaitTimeout", message)
                .retry_safe(false)
                .with_data(data))
        }
    }
}

// ---- input_sequence: the engine lives in cs2-mcp-common (hammer-mcp registers the same tool) ----

pub use cs2_mcp_common::play::sequence::{input_sequence_schema, release, CLEAR_FLOOR_MS};

pub async fn input_sequence(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut bridge = state.bridge.lock().await;
    cs2_mcp_common::play::sequence::input_sequence(&mut bridge, &ctx, &args).await
}

pub fn setpos_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "origin": {"type": "array", "items": {"type": "number", "minimum": -COORD_LIMIT, "maximum": COORD_LIMIT}, "minItems": 3, "maxItems": 3},
            "angles": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3, "description": "[pitch, yaw, roll]: pitch +-89, roll +-180; roll not compared"},
            "exact": {"type": "boolean", "default": true},
            "floor": {"type": "boolean", "default": false, "description": "z is only an upper bound: the player may fall"},
            "minZ": {"type": "number", "minimum": -32768, "maximum": 32768, "description": "with floor: a lower landing is FellOutOfMap"},
            "tolerance": {"type": "number", "minimum": 0},
            "zTolerance": {"type": "number", "minimum": 0},
            "angleTolerance": {"type": "number", "minimum": 0},
            "timeoutMs": {"type": "integer", "minimum": 1}
        },
        "required": ["origin"]
    })
}

// ---- fire_weapon, look_at, use_entity, ent_fire ------------------------------------------------

const SHOTS_MAX: u64 = 30;
const FRAMES_DEFAULT: u64 = 2;
const FRAMES_MAX: u64 = 64;
const FIRE_TIMEOUT_DEFAULT_MS: u64 = 3000;
const TIMEOUT_MAX_MS: u64 = 60_000;
const AIM_TIMEOUT_DEFAULT_MS: u64 = 2000;
/// Pace of the ammo, view and tick samples: a shot or a turn lands within a few game ticks.
const AIM_POLL_MS: u64 = 20;
/// The view must be within this many degrees (pitch and yaw) of the aim.
const AIM_TOLERANCE_DEG: f64 = 1.0;
/// Ring size the bridge keeps; sound.drain asks for all of it.
const RING_MAX: u64 = 256;
/// Entity indexes the bridge walks: 1..=ENTINDEX_MAX, i.e. every slot MAX_EDICTS (public/const.h
/// `MAX_EDICTS` = 1<<MAX_EDICT_BITS, from s2sdk-sys) can hold minus the null slot 0.
const ENTINDEX_MAX: u64 = s2sdk_sys::generated::MAX_EDICTS as u64 - 1;
/// Frames the use key is pressed.
const USE_FRAMES: u64 = 4;
/// Game ticks after the use tap in which the button's effect is looked for (0.5 s at 64 ticks).
const USE_WINDOW_TICKS: u64 = 32;
const USE_TIMEOUT_DEFAULT_MS: u64 = 2000;
const MAX_DISTANCE_DEFAULT: f64 = 64.0;
const MAX_DISTANCE_LIMIT: f64 = 4096.0;
/// Ticks after the aim is cleared in which the view is read back (the snap-back check).
const CLEAR_READ_TICKS: u64 = 4;
const CLEAR_READ_TIMEOUT_MS: u64 = 500;
/// The `at` tolerance that makes the origin irrelevant: the view alone is awaited.
const ANY_ORIGIN: f64 = 1.0e9;
const DELAY_MAX: f64 = 60.0;
const ENT_TARGET_MAX: usize = 64;
const ENT_INPUT_MAX: usize = 64;
const ENT_VALUE_MAX: usize = 128;
/// How long ent_fire waits for the confirmation marker.
const ENT_CONFIRM_MS: u64 = 5000;

/// A look or use target: a point, or an entity by index (resolved through game.entities).
#[derive(Debug, Clone, PartialEq)]
enum Target {
    Point([f64; 3]),
    Index(i64),
}

#[derive(Debug, Clone, PartialEq)]
struct FireRequest {
    shots: u64,
    frames: u64,
    weapon: Option<String>,
    timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct LookRequest {
    target: Target,
    eye: bool,
    hold: bool,
    timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct UseRequest {
    target: Target,
    max_distance: f64,
    timeout_ms: u64,
}

/// An entity as game.entities reports it.
#[derive(Debug, Clone)]
struct Entity {
    entindex: i64,
    class: Value,
    name: Value,
    origin: [f64; 3],
    angles: Option<[f64; 3]>,
}

impl Entity {
    fn json(&self) -> Value {
        json!({
            "entindex": self.entindex,
            "class": self.class,
            "name": self.name,
            "origin": self.origin,
            "angles": self.angles,
        })
    }
}

/// `[p, y, r]` looking from `eye` at `target`: pitch is positive looking down and clamped to
/// +-PITCH_LIMIT; yaw 0 is +x and 90 is +y (atan2, so in [-180, 180]); roll 0.
pub fn aim_angles(eye: [f64; 3], target: [f64; 3]) -> [f64; 3] {
    let d = [target[0] - eye[0], target[1] - eye[1], target[2] - eye[2]];
    let horizontal = d[0].hypot(d[1]);
    let yaw = d[1].atan2(d[0]).to_degrees();
    let pitch = (-d[2].atan2(horizontal).to_degrees()).clamp(-PITCH_LIMIT, PITCH_LIMIT);
    [pitch, yaw, 0.0]
}

/// The arguments object, with only `allowed` keys.
fn keys_of<'a>(
    args: &'a Value,
    allowed: &[&str],
    what: &str,
) -> Result<&'a Map<String, Value>, ToolError> {
    let obj = match args {
        Value::Object(m) => m,
        _ => return Err(invalid("arguments must be an object")),
    };
    if let Some(k) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(invalid(format!(
            "unknown key {k:?} in {what} (allowed: {})",
            allowed.join(", ")
        )));
    }
    Ok(obj)
}

/// Optional integer in `min..=max` (null counts as absent).
fn bounded(
    obj: &Map<String, Value>,
    key: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, ToolError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .filter(|n| (min..=max).contains(n))
            .ok_or_else(|| invalid(format!("{key} must be an integer {min}..={max}"))),
    }
}

/// A target: `[x, y, z]` (finite, within the map) or an entindex in 1..=ENTINDEX_MAX.
fn parse_target(v: Option<&Value>) -> Result<Target, ToolError> {
    let bad = || {
        invalid(format!(
            "target must be [x, y, z] (finite numbers, |v| <= {COORD_LIMIT}) or an entindex 1..={ENTINDEX_MAX}"
        ))
    };
    match v {
        Some(Value::Array(_)) => {
            let p = vec3(v.unwrap_or(&Value::Null)).ok_or_else(bad)?;
            if p.iter().any(|c| c.abs() > COORD_LIMIT) {
                return Err(bad());
            }
            Ok(Target::Point(p))
        }
        Some(n) => n
            .as_u64()
            .filter(|i| (1..=ENTINDEX_MAX).contains(i))
            .map(|i| Target::Index(i as i64))
            .ok_or_else(bad),
        None => Err(bad()),
    }
}

/// Validate `fire_weapon` arguments. Nothing is sent.
fn parse_fire(args: &Value) -> Result<FireRequest, ToolError> {
    let obj = keys_of(
        args,
        &["shots", "frames", "weapon", "timeoutMs"],
        "fire_weapon",
    )?;
    let weapon = match obj.get("weapon") {
        None | Some(Value::Null) => None,
        Some(Value::String(w)) if is_weapon_name(w) => Some(w.clone()),
        Some(_) => {
            return Err(invalid(
                "weapon must look like weapon_hkp2000 (lower-case, weapon_ prefix)",
            ))
        }
    };
    Ok(FireRequest {
        shots: bounded(obj, "shots", 1, 1, SHOTS_MAX)?,
        frames: bounded(obj, "frames", FRAMES_DEFAULT, 1, FRAMES_MAX)?,
        weapon,
        timeout_ms: bounded(obj, "timeoutMs", FIRE_TIMEOUT_DEFAULT_MS, 1, TIMEOUT_MAX_MS)?,
    })
}

/// Validate `look_at` arguments. Nothing is sent.
fn parse_look(args: &Value) -> Result<LookRequest, ToolError> {
    let obj = keys_of(args, &["target", "eye", "hold", "timeoutMs"], "look_at")?;
    let flag = |key: &str, default: bool| match obj.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| invalid(format!("{key} must be true or false"))),
    };
    Ok(LookRequest {
        target: parse_target(obj.get("target"))?,
        eye: flag("eye", true)?,
        hold: flag("hold", false)?,
        timeout_ms: bounded(obj, "timeoutMs", AIM_TIMEOUT_DEFAULT_MS, 1, TIMEOUT_MAX_MS)?,
    })
}

/// Validate `use_entity` arguments. Nothing is sent.
fn parse_use(args: &Value) -> Result<UseRequest, ToolError> {
    let obj = keys_of(args, &["target", "maxDistance", "timeoutMs"], "use_entity")?;
    let max_distance = match obj.get("maxDistance") {
        None | Some(Value::Null) => MAX_DISTANCE_DEFAULT,
        Some(v) => v
            .as_f64()
            .filter(|d| d.is_finite() && *d > 0.0 && *d <= MAX_DISTANCE_LIMIT)
            .ok_or_else(|| {
                invalid(format!(
                    "maxDistance must be a number > 0 and <= {MAX_DISTANCE_LIMIT}"
                ))
            })?,
    };
    Ok(UseRequest {
        target: parse_target(obj.get("target"))?,
        max_distance,
        timeout_ms: bounded(obj, "timeoutMs", USE_TIMEOUT_DEFAULT_MS, 1, TIMEOUT_MAX_MS)?,
    })
}

/// A refusal before anything was sent: PrerequisiteFailed, retry-safe.
fn prerequisite(message: impl Into<String>) -> ToolError {
    ToolError::new("PrerequisiteFailed", "PrerequisiteFailed", message)
        .retry_safe(true)
        .with_data(json!({"inputSent": false}))
}

/// game.local, refused when there is no live local player (nothing else is sent before it).
async fn local_now(bridge: &mut BridgeClient, ctx: &CallCtx) -> Result<Value, ToolError> {
    let v = bridge
        .request(
            "game.local",
            json!({}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await?
        .result;
    player_of(&v, false)?;
    Ok(v)
}

/// The eye (or origin) of the local player in `local`, for aiming.
fn start_point(local: &Value, eye: bool) -> Result<[f64; 3], ToolError> {
    let player = player_of(local, false)?;
    let key = if eye { "eye" } else { "origin" };
    player.get(key).and_then(vec3).ok_or_else(|| {
        ToolError::new(
            "SchemaUnavailable",
            "SchemaUnavailable",
            format!("game.local has no player.{key} (a failed schema read): cannot aim"),
        )
        .retry_safe(true)
    })
}

/// game.entities by index: the bridge reads only that slot (its `entindex` filter), so any index
/// 1..=ENTINDEX_MAX (every MAX_EDICTS slot but the null one) is reachable. An empty slot is
/// EntityNotFound (nothing sent).
async fn find_entity(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    entindex: i64,
) -> Result<Entity, ToolError> {
    let reply = bridge
        .request(
            "game.entities",
            json!({"entindex": entindex}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await?
        .result;
    let hit = reply
        .get("entities")
        .and_then(Value::as_array)
        .and_then(|l| {
            l.iter()
                .find(|e| e.get("entindex").and_then(Value::as_i64) == Some(entindex))
        });
    match hit {
        Some(e) => Ok(Entity {
            entindex,
            class: e.get("class").cloned().unwrap_or(Value::Null),
            name: e.get("name").cloned().unwrap_or(Value::Null),
            origin: e.get("origin").and_then(vec3).ok_or_else(|| {
                ToolError::new(
                    "SchemaUnavailable",
                    "SchemaUnavailable",
                    format!("game.entities has no origin for entindex {entindex}"),
                )
                .retry_safe(true)
            })?,
            angles: e.get("angles").and_then(vec3),
        }),
        None => {
            let truncated = reply
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let message = format!("no entity in slot {entindex} (empty or not an entity)");
            Err(ToolError::new("EntityNotFound", "EntityNotFound", message)
                .retry_safe(true)
                .with_data(
                    json!({"inputSent": false, "entindex": entindex, "truncated": truncated}),
                ))
        }
    }
}

/// The aim point of a target: its position, and the entity when it was given by index.
async fn resolve_target(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    target: &Target,
) -> Result<([f64; 3], Option<Entity>), ToolError> {
    match target {
        Target::Point(p) => Ok((*p, None)),
        Target::Index(i) => {
            let e = find_entity(bridge, ctx, *i).await?;
            Ok((e.origin, Some(e)))
        }
    }
}

/// Refuse to send when the call budget cannot cover a send and one sample of its wait.
fn send_budget(ctx: &CallCtx) -> Result<(), ToolError> {
    if ctx.budget.remaining() < WAIT_MARGIN + ONE_SAMPLE {
        return Err(ToolError::budget_exceeded(&ctx.budget)
            .retry_safe(true)
            .with_data(json!({"inputSent": false})));
    }
    Ok(())
}

/// input.set with the call's deadline.
async fn send_input(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    set: &InputSet,
) -> Result<Value, ToolError> {
    let params = serde_json::to_value(set)
        .map_err(|e| ToolError::internal(format!("cannot encode input.set params: {e}")))?;
    let deadline = ctx.budget.deadline_ms()?;
    Ok(bridge
        .request("input.set", params, RequestOpts::deadline(deadline))
        .await?
        .result)
}

/// The undo after a failure: each input.set in `undo` (a release, or an angles null), in order and sent
/// even when the budget is spent (a held button outlives the call). `inputCleared` is true when all were
/// sent, "failed" when one was not.
async fn undo_input(bridge: &mut BridgeClient, ctx: &CallCtx, undo: &[InputSet]) -> Value {
    let mut all = true;
    for set in undo {
        let Ok(params) = serde_json::to_value(set) else {
            all = false;
            continue;
        };
        let ms = (ctx.budget.remaining().as_millis() as u64).max(CLEAR_FLOOR_MS);
        if bridge
            .request("input.set", params, RequestOpts::ungated(ms))
            .await
            .is_err()
        {
            all = false;
        }
    }
    if all {
        json!(true)
    } else {
        json!("failed")
    }
}

/// The tail of a tool that may have sent input: an error after the first send is not retry-safe,
/// and the input is undone (`undo`, in order) with what was sent and the undo's result attached.
async fn finish(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    sent: bool,
    res: Result<Value, ToolError>,
    undo: &[InputSet],
) -> Result<ToolOutput, ToolError> {
    match res {
        Ok(v) => Ok(v.into()),
        Err(e) if !sent => Err(e),
        Err(e) => {
            let cleared = undo_input(bridge, ctx, undo).await;
            Err(e
                .retry_safe(false)
                .with_data(json!({"inputSent": true, "inputCleared": cleared})))
        }
    }
}

/// Wait until the view is within AIM_TOLERANCE_DEG of `angles` (pitch and yaw). The origin is not
/// checked. A timeout is WaitTimeout (not retry-safe) with the view it last read.
async fn settle_view(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    angles: [f64; 3],
    timeout_ms: u64,
) -> Result<Sample, ToolError> {
    let req = WaitRequest {
        predicate: Predicate::At(AtSpec {
            origin: [0.0; 3],
            tolerance: ANY_ORIGIN,
            z_tolerance: ANY_ORIGIN,
            angles: Some(angles),
            angle_tolerance: AIM_TOLERANCE_DEG,
            require_stopped: false,
        }),
        from: None,
        round: RoundBase::default(),
        ammo_from: None,
        timeout_ms,
        poll_ms: AIM_POLL_MS,
    };
    let out = run_wait(bridge, ctx, &req).await?;
    match out.end {
        WaitEnd::Matched { sample, .. } => Ok(sample),
        WaitEnd::Expired { last, bridge_error } => {
            let achieved = last.as_ref().and_then(|s| s.view_angles);
            Err(ToolError::new(
                "WaitTimeout",
                "WaitTimeout",
                format!(
                    "the view did not settle on pitch {:.2} yaw {:.2} within {} ms; the aim override is undone",
                    angles[0],
                    angles[1],
                    out.waited.as_millis()
                ),
            )
            .retry_safe(false)
            .with_data(json!({
                "reason": if bridge_error.is_some() { "bridgeQueueTimeout" } else { "timeout" },
                "angles": angles,
                "achieved": achieved,
                "samples": out.samples,
                "waitedMs": out.waited.as_millis() as u64,
            })))
        }
    }
}

/// Wait for `ticks` game ticks after the first game.local sample. A timeout is WaitTimeout.
async fn wait_ticks(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    ticks: u64,
    timeout_ms: u64,
) -> Result<Sample, ToolError> {
    let req = WaitRequest {
        predicate: Predicate::Tick { ticks },
        from: None,
        round: RoundBase::default(),
        ammo_from: None,
        timeout_ms,
        poll_ms: AIM_POLL_MS,
    };
    let out = run_wait(bridge, ctx, &req).await?;
    match out.end {
        WaitEnd::Matched { sample, .. } => Ok(sample),
        WaitEnd::Expired { .. } => Err(ToolError::new(
            "WaitTimeout",
            "WaitTimeout",
            format!(
                "game did not advance {ticks} ticks within {} ms",
                out.waited.as_millis()
            ),
        )
        .retry_safe(false)
        .with_data(json!({"ticks": ticks, "samples": out.samples}))),
    }
}

/// sound.drain: the ring since the last drain (it empties the ring; the first call installs the hook).
async fn drain_sounds(bridge: &mut BridgeClient, ctx: &CallCtx) -> Result<Value, ToolError> {
    Ok(bridge
        .request(
            "sound.drain",
            json!({"max": RING_MAX}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await?
        .result)
}

/// The events of a sound.drain reply.
fn sound_events(reply: &Value) -> &[Value] {
    reply
        .get("events")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// A weapon's shot sound: `Weapon_<name>.Single|Fire|Auto`.
fn is_shot_sound(name: &str) -> bool {
    name.starts_with("Weapon_")
        && [".Single", ".Fire", ".Auto"]
            .iter()
            .any(|s| name.ends_with(s))
}

/// A bullet impact sound (`Default.BulletImpact`, `Concrete.BulletImpact`, ...).
fn is_impact_sound(name: &str) -> bool {
    name.contains(".BulletImpact")
}

/// Game units within which a Button or Door sound counts as the use of the target. An event without an
/// origin never counts by name.
const USE_SOUND_RADIUS: f64 = 128.0;

/// A sound that may be the use of `target`: one from the target's own entindex always counts; a Button
/// or Door sound counts within USE_SOUND_RADIUS of the target (`origin_dist`, from the event's origin).
fn is_use_sound(name: &str, entindex: i64, target: Option<i64>, origin_dist: Option<f64>) -> bool {
    target == Some(entindex)
        || ((name.contains("Button") || name.contains("Door"))
            && origin_dist.is_some_and(|d| d <= USE_SOUND_RADIUS))
}

/// The largest pitch or yaw difference between two views, yaw wrapped, in degrees.
fn view_moved(a: [f64; 3], b: [f64; 3]) -> f64 {
    let wrap = |d: f64| (d + 180.0).rem_euclid(360.0) - 180.0;
    wrap(a[0] - b[0]).abs().max(wrap(a[1] - b[1]).abs())
}

fn sound_entry(ev: &Value) -> Value {
    json!({
        "seq": ev.get("seq").cloned().unwrap_or(Value::Null),
        "name": ev.get("name").cloned().unwrap_or(Value::Null),
        "entindex": ev.get("entindex").cloned().unwrap_or(Value::Null),
    })
}

/// The weapon the shot is read from, as game.local shows it, checked before anything is sent.
struct Gun {
    name: String,
    entindex: i64,
    clip1: i64,
    pawn: Option<i64>,
    tick: u64,
}

fn gun_of(local: &Value, req: &FireRequest) -> Result<Gun, ToolError> {
    let s = sample_of(local, None, false)?;
    if !s.weapon.is_object() {
        return Err(prerequisite(
            "no active weapon: select one first (nothing was sent)",
        ));
    }
    let name = s
        .weapon
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if let Some(want) = &req.weapon {
        if &name != want {
            return Err(prerequisite(format!(
                "the active weapon is {name:?}, not {want:?}: select it first (nothing was sent)"
            )));
        }
    }
    let clip1 = s
        .weapon
        .get("clip1")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            prerequisite("the active weapon has no clip1 (a failed schema read); nothing was sent")
        })?;
    if clip1 < 0 {
        return Err(prerequisite(
            "the active weapon has no clip (clip1 -1: a knife or a grenade); nothing was sent",
        ));
    }
    if (clip1 as u64) < req.shots {
        return Err(prerequisite(format!(
            "clip1 {clip1} cannot fire {} shots: reload first (nothing was sent)",
            req.shots
        )));
    }
    let entindex = s
        .weapon
        .get("entindex")
        .and_then(Value::as_i64)
        .filter(|e| *e >= 0)
        .ok_or_else(|| {
            prerequisite(
                "the active weapon has no entindex (a failed schema read); nothing was sent",
            )
        })?;
    Ok(Gun {
        name,
        entindex,
        clip1,
        pawn: local
            .get("player")
            .and_then(|p| p.get("pawnIndex"))
            .and_then(Value::as_i64)
            .filter(|p| *p >= 0),
        tick: s.tick,
    })
}

/// Per shot: a release, then a tap. A tap that has not dropped clip1 within FIRE_RETAP_MS is released and
/// tapped again, until the shot's deadline: a semi-auto ignores a tap that arrives during its cycle, and
/// a tap during an active press never raises attack again.
const FIRE_RETAP_MS: u64 = 200;
/// Longest one shot may take (the call's remaining timeout caps it too).
const FIRE_SHOT_MS: u64 = 1000;
/// Game ticks the sound poll may run after the last shot (shot sounds can trail the clip drop).
const SOUND_POLL_TICKS: u64 = 16;
/// Longest one sound poll step waits for its ticks.
const SOUND_POLL_WAIT_MS: u64 = 500;

fn release_attack() -> InputSet {
    InputSet {
        release: vec!["attack".into()],
        ..Default::default()
    }
}

fn angle_undo() -> InputSet {
    InputSet {
        angles: Some(None),
        ..Default::default()
    }
}

/// What the sound poll saw: the shot and impact events, the ring's dropped count, and a note when the
/// poll ended short of the shots.
struct SoundPoll {
    shot: Vec<Value>,
    impact: Vec<Value>,
    dropped: u64,
    note: Value,
}

/// Drain the sound ring until `shots` shot sounds from the shooter's pawn are seen, or SOUND_POLL_TICKS
/// game ticks pass since `from_tick`. Only a failed drain is an error; a short poll is a note.
async fn poll_shot_sounds(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    pawn: Option<i64>,
    shots: u64,
    from_tick: u64,
) -> Result<SoundPoll, ToolError> {
    let mut poll = SoundPoll {
        shot: Vec::new(),
        impact: Vec::new(),
        dropped: 0,
        note: Value::Null,
    };
    let mut tick = from_tick;
    loop {
        let reply = drain_sounds(bridge, ctx).await?;
        poll.dropped += reply.get("dropped").and_then(Value::as_u64).unwrap_or(0);
        for ev in sound_events(&reply) {
            let name = ev.get("name").and_then(Value::as_str).unwrap_or_default();
            let ent = ev.get("entindex").and_then(Value::as_i64);
            if is_shot_sound(name) && pawn.is_none_or(|p| ent == Some(p)) {
                poll.shot.push(sound_entry(ev));
            } else if is_impact_sound(name) {
                poll.impact.push(sound_entry(ev));
            }
        }
        if poll.shot.len() as u64 >= shots {
            break;
        }
        if tick >= from_tick + SOUND_POLL_TICKS {
            poll.note = json!(format!(
                "{} of {shots} shot sounds seen within {SOUND_POLL_TICKS} ticks",
                poll.shot.len()
            ));
            break;
        }
        match wait_ticks(bridge, ctx, 2, SOUND_POLL_WAIT_MS).await {
            Ok(s) => tick = s.tick,
            Err(e) => {
                poll.note = json!(format!("sound poll stopped: {}", e.message));
                break;
            }
        }
    }
    Ok(poll)
}

fn fire_timeout(message: String, fired: u64, clip: i64, taps: u64, reason: &str) -> ToolError {
    ToolError::new("WaitTimeout", "WaitTimeout", message)
        .retry_safe(false)
        .with_data(json!({
            "reason": reason,
            "shotsFired": fired,
            "clip1": clip,
            "taps": taps,
        }))
}

/// `fire_weapon`. Each shot is a release and a tap, re-tapped every FIRE_RETAP_MS until clip1 drops by
/// one from the caller's baseline (the clip and entindex read before the first tap). Then attack is
/// released, and the shot sounds are polled.
async fn fire_run(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    req: &FireRequest,
    sent: &mut bool,
) -> Result<Value, ToolError> {
    let first = local_now(bridge, ctx).await?;
    gun_of(&first, req)?;
    let sounds_on = bridge.has("sound.drain");
    if sounds_on {
        // the baseline: everything before this shot is discarded (and the hook is installed)
        drain_sounds(bridge, ctx).await?;
    }
    let local = local_now(bridge, ctx).await?;
    let gun = gun_of(&local, req)?;
    send_budget(ctx)?;
    let total_end = Instant::now() + Duration::from_millis(req.timeout_ms);
    let mut clip = gun.clip1;
    let mut tick = gun.tick;
    let mut waited = Duration::ZERO;
    let mut fired: u64 = 0;
    let mut taps: u64 = 0;
    for _ in 0..req.shots {
        let shot_left = total_end.saturating_duration_since(Instant::now());
        let shot_end = Instant::now() + Duration::from_millis(FIRE_SHOT_MS).min(shot_left);
        loop {
            let left = shot_end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(fire_timeout(
                    format!(
                        "shot {} did not drop clip1 below {clip} within its deadline ({taps} taps)",
                        fired + 1
                    ),
                    fired,
                    clip,
                    taps,
                    "timeout",
                ));
            }
            // a fresh press: release what may still be held, then tap
            *sent = true;
            send_input(bridge, ctx, &release_attack()).await?;
            let tap = InputSet {
                tap: vec!["attack".into()],
                frames: Some(req.frames as u32),
                ..Default::default()
            };
            send_input(bridge, ctx, &tap).await?;
            taps += 1;
            let attempt = left.min(Duration::from_millis(FIRE_RETAP_MS));
            let wait = WaitRequest {
                predicate: Predicate::Ammo {
                    clip1_delta: -1,
                    weapon: None,
                },
                from: None,
                round: RoundBase::default(),
                ammo_from: Some(AmmoFrom {
                    clip1: clip,
                    entindex: gun.entindex,
                }),
                timeout_ms: attempt.as_millis().max(1) as u64,
                poll_ms: AIM_POLL_MS,
            };
            let out = run_wait(bridge, ctx, &wait).await?;
            waited += out.waited;
            match out.end {
                WaitEnd::Matched { sample, .. } => {
                    clip = sample
                        .weapon
                        .get("clip1")
                        .and_then(Value::as_i64)
                        .unwrap_or(clip - 1);
                    tick = sample.tick;
                    fired += 1;
                    break;
                }
                WaitEnd::Expired {
                    bridge_error: Some(e),
                    ..
                } => {
                    return Err(fire_timeout(
                        format!(
                            "the bridge dropped a game.local sample during shot {}: {}",
                            fired + 1,
                            e.message
                        ),
                        fired,
                        clip,
                        taps,
                        "bridgeQueueTimeout",
                    ));
                }
                // not landed within this attempt: the loop releases and taps again, or times out
                WaitEnd::Expired {
                    bridge_error: None, ..
                } => {}
            }
        }
    }
    // the taps release themselves, but attack must be up when the call returns
    send_input(bridge, ctx, &release_attack()).await?;
    let (sounds, note, sounds_ok) = if sounds_on {
        match poll_shot_sounds(bridge, ctx, gun.pawn, req.shots, tick).await {
            Ok(poll) => {
                let ok = poll.shot.len() as u64 >= req.shots;
                (
                    json!({"shot": poll.shot, "impact": poll.impact, "dropped": poll.dropped}),
                    poll.note,
                    ok,
                )
            }
            Err(e) => (
                Value::Null,
                json!(format!("sound.drain failed: {}", e.message)),
                false,
            ),
        }
    } else {
        (
            Value::Null,
            json!("sound.drain is not advertised by the bridge: sounds were not checked"),
            true,
        )
    };
    let delta = clip - gun.clip1;
    Ok(json!({
        "weapon": gun.name,
        "entindex": gun.entindex,
        "shots": req.shots,
        "frames": req.frames,
        "shotsFired": fired,
        "taps": taps,
        "released": true,
        "clip1Before": gun.clip1,
        "clip1After": clip,
        "clip1Delta": delta,
        "sounds": sounds,
        "note": note,
        "ticks": {"start": gun.tick, "end": tick},
        "waitedMs": waited.as_millis() as u64,
        "ok": delta == -(req.shots as i64) && sounds_ok,
    }))
}

pub async fn fire_weapon(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let req = parse_fire(&args)?;
    let mut bridge = state.bridge.lock().await;
    let mut sent = false;
    let res = fire_run(&mut bridge, &ctx, &req, &mut sent).await;
    finish(&mut bridge, &ctx, sent, res, &[release_attack()]).await
}

/// `look_at`: the aim as an angle override; held when `hold`, else cleared once the view settled, and
/// the view read back a few ticks later (`snappedBack` when it moved more than the aim tolerance).
async fn look_run(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    req: &LookRequest,
    sent: &mut bool,
) -> Result<Value, ToolError> {
    let local = local_now(bridge, ctx).await?;
    let start = start_point(&local, req.eye)?;
    let (point, entity) = resolve_target(bridge, ctx, &req.target).await?;
    let angles = wire_angles(aim_angles(start, point), "look_at")?;
    send_budget(ctx)?;
    *sent = true;
    let set = InputSet {
        angles: Some(Some(angles)),
        ..Default::default()
    };
    send_input(bridge, ctx, &set).await?;
    let settled = settle_view(bridge, ctx, angles, req.timeout_ms).await?;
    let mut cleared = false;
    let mut after: Option<[f64; 3]> = None;
    if !req.hold {
        send_input(bridge, ctx, &angle_undo()).await?;
        cleared = true;
        // the snap-back check: the view a few ticks after the override is gone
        after = match wait_ticks(bridge, ctx, CLEAR_READ_TICKS, CLEAR_READ_TIMEOUT_MS).await {
            Ok(s) => s.view_angles,
            Err(_) => None,
        };
    }
    let moved = match (after, settled.view_angles) {
        (Some(a), Some(b)) => Some(view_moved(a, b)),
        _ => None,
    };
    let snapped = moved.map(|m| m > AIM_TOLERANCE_DEG);
    let note = match moved {
        Some(m) if m > AIM_TOLERANCE_DEG => json!(format!(
            "the view moved {m:.2} degrees after the override was cleared (snapped back)"
        )),
        _ => Value::Null,
    };
    Ok(json!({
        "target": {"point": point, "entity": entity.as_ref().map(Entity::json)},
        "from": start,
        "distance": distance(start, point),
        "angles": angles,
        "achieved": settled.view_angles,
        "tick": settled.tick,
        "held": req.hold,
        "inputCleared": cleared,
        "viewAfterClear": after,
        "snappedBack": snapped,
        "note": note,
    }))
}

pub async fn look_at(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let req = parse_look(&args)?;
    let mut bridge = state.bridge.lock().await;
    let mut sent = false;
    let res = look_run(&mut bridge, &ctx, &req, &mut sent).await;
    finish(&mut bridge, &ctx, sent, res, &[angle_undo()]).await
}

/// `use_entity`: aim at the target (held), press use for USE_FRAMES frames, look for the evidence in the
/// following USE_WINDOW_TICKS, then clear the aim and read the sounds and the entity. Success is only
/// claimed with evidence. A target gone after the use is evidence (removed).
async fn use_run(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    req: &UseRequest,
    sent: &mut bool,
) -> Result<Value, ToolError> {
    let local = local_now(bridge, ctx).await?;
    let eye = start_point(&local, true)?;
    let (point, before) = resolve_target(bridge, ctx, &req.target).await?;
    let dist = distance(eye, point);
    if dist > req.max_distance {
        return Err(prerequisite(format!(
            "the target is {dist:.1} units from the eye, beyond maxDistance {}; nothing was sent",
            req.max_distance
        ))
        .with_data(json!({"distance": dist, "maxDistance": req.max_distance, "target": point})));
    }
    let angles = wire_angles(aim_angles(eye, point), "use_entity")?;
    send_budget(ctx)?;
    let sounds_on = bridge.has("sound.drain");
    if sounds_on {
        // the window starts here: earlier sounds are discarded
        drain_sounds(bridge, ctx).await?;
    }
    *sent = true;
    let set = InputSet {
        angles: Some(Some(angles)),
        ..Default::default()
    };
    send_input(bridge, ctx, &set).await?;
    settle_view(bridge, ctx, angles, req.timeout_ms).await?;
    let tap = InputSet {
        tap: vec!["use".into()],
        frames: Some(USE_FRAMES as u32),
        ..Default::default()
    };
    send_input(bridge, ctx, &tap).await?;
    let window = wait_ticks(bridge, ctx, USE_WINDOW_TICKS, req.timeout_ms).await?;
    send_input(bridge, ctx, &angle_undo()).await?;
    let (events, note) = if sounds_on {
        match drain_sounds(bridge, ctx).await {
            Ok(reply) => (Some(sound_events(&reply).to_vec()), Value::Null),
            Err(e) => (None, json!(format!("sound.drain failed: {}", e.message))),
        }
    } else {
        (
            None,
            json!("sound.drain is not advertised by the bridge: sounds were not checked"),
        )
    };
    let target_index = match req.target {
        Target::Index(i) => Some(i),
        Target::Point(_) => None,
    };
    let mut evidence = Vec::new();
    let mut names: Option<Vec<Value>> = None;
    if let Some(events) = &events {
        let mut all = Vec::new();
        for ev in events {
            let name = ev.get("name").and_then(Value::as_str).unwrap_or_default();
            let ent = ev.get("entindex").and_then(Value::as_i64).unwrap_or(-1);
            let origin_dist = ev.get("origin").and_then(vec3).map(|o| distance(o, point));
            all.push(json!(name));
            if is_use_sound(name, ent, target_index, origin_dist) {
                let mut entry = sound_entry(ev);
                entry["kind"] = json!("sound");
                entry["distance"] = json!(origin_dist);
                evidence.push(entry);
            }
        }
        names = Some(all);
    }
    let (after, removed) = match &before {
        None => (None, false),
        Some(e) => match find_entity(bridge, ctx, e.entindex).await {
            Ok(a) => (Some(a), false),
            // the slot is empty now: the target was removed by the use
            Err(err) if err.name == "EntityNotFound" => (None, true),
            Err(err) => return Err(err),
        },
    };
    let changed = removed
        || match (&before, &after) {
            (Some(b), Some(a)) => {
                distance(b.origin, a.origin) > 1e-3
                    || match (b.angles, a.angles) {
                        (Some(x), Some(y)) => distance(x, y) > 1e-3,
                        _ => false,
                    }
            }
            _ => false,
        };
    if removed {
        if let Some(b) = &before {
            evidence.push(json!({"kind": "entity", "entindex": b.entindex, "removed": true}));
        }
    } else if changed {
        if let (Some(b), Some(a)) = (&before, &after) {
            evidence.push(json!({
                "kind": "entity",
                "entindex": a.entindex,
                "origin": {"before": b.origin, "after": a.origin},
                "angles": {"before": b.angles, "after": a.angles},
            }));
        }
    }
    let ok = !evidence.is_empty();
    Ok(json!({
        "target": {"point": point, "entity": before.as_ref().map(Entity::json)},
        "distance": dist,
        "maxDistance": req.max_distance,
        "angles": angles,
        "achieved": window.view_angles,
        "evidence": evidence,
        "changed": changed,
        "removed": removed,
        "ok": ok,
        "entityAfter": after.as_ref().map(Entity::json),
        "soundsInWindow": names,
        "note": note,
        "ticks": {"start": local.get("tick").cloned().unwrap_or(Value::Null), "end": window.tick},
    }))
}

pub async fn use_entity(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let req = parse_use(&args)?;
    let mut bridge = state.bridge.lock().await;
    let mut sent = false;
    let res = use_run(&mut bridge, &ctx, &req, &mut sent).await;
    // a use that is still held is released first, then the aim is cleared
    let release_use = InputSet {
        release: vec!["use".into()],
        ..Default::default()
    };
    finish(&mut bridge, &ctx, sent, res, &[release_use, angle_undo()]).await
}

/// Validate a `ent_fire` value: a short token, spaces allowed (the command quotes it).
fn token(
    obj: &Map<String, Value>,
    key: &str,
    max: usize,
    allowed: impl Fn(char) -> bool,
    required: bool,
) -> Result<Option<String>, ToolError> {
    let s = match obj.get(key) {
        None | Some(Value::Null) => {
            return if required {
                Err(invalid(format!("{key} is required")))
            } else {
                Ok(None)
            }
        }
        Some(Value::String(s)) => s,
        Some(_) => return Err(invalid(format!("{key} must be a string"))),
    };
    if s.is_empty() && !required {
        return Ok(None);
    }
    if s.is_empty() || s.len() > max || !s.chars().all(&allowed) {
        return Err(invalid(format!(
            "{key} must be 1..={max} characters from the allowed set (no ; or quotes or newlines)"
        )));
    }
    Ok(Some(s.clone()))
}

/// Inputs that destroy: refused on a wildcard or classname target unless allowDestructive.
const DESTRUCTIVE_INPUTS: [&str; 2] = ["Kill", "KillHierarchy"];
/// Target prefixes that name a classname (a name the map gave an entity is not one of these, by
/// convention). A heuristic: a targetname with one of these prefixes needs allowDestructive too.
const CLASS_PREFIXES: [&str; 22] = [
    "func_", "prop_", "trigger_", "point_", "env_", "info_", "logic_", "game_", "cs_", "weapon_",
    "light_", "math_", "filter_", "physics_", "ambient_", "beam_", "sprite_", "path_", "item_",
    "hostage_", "bot_", "player",
];

fn is_classname(target: &str) -> bool {
    CLASS_PREFIXES.iter().any(|p| target.starts_with(p))
}

/// Build the `ent_fire` console command. Nothing is sent: the arguments are checked here. `*` is refused
/// outright; Kill and KillHierarchy on a wildcard or classname target need `allowDestructive: true`.
/// Not a security boundary: an entity such as point_servercommand or a logic relay can still run other
/// inputs or commands, and ent_fire does not look inside those chains.
pub fn ent_fire_command(args: &Value) -> Result<String, ToolError> {
    let obj = keys_of(
        args,
        &["target", "input", "value", "delay", "allowDestructive"],
        "ent_fire",
    )?;
    let target = token(
        obj,
        "target",
        ENT_TARGET_MAX,
        |c| c.is_ascii_alphanumeric() || "_!*.-".contains(c),
        true,
    )?
    .unwrap_or_default();
    let input = token(
        obj,
        "input",
        ENT_INPUT_MAX,
        |c| c.is_ascii_alphanumeric() || c == '_',
        true,
    )?
    .unwrap_or_default();
    let value = token(
        obj,
        "value",
        ENT_VALUE_MAX,
        |c| c.is_ascii_alphanumeric() || "_.- ".contains(c),
        false,
    )?;
    let delay = match obj.get("delay") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64()
                .filter(|d| d.is_finite() && (0.0..=DELAY_MAX).contains(d))
                .ok_or_else(|| invalid(format!("delay must be a number 0..={DELAY_MAX}")))?,
        ),
    };
    let allow = match obj.get("allowDestructive") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| invalid("allowDestructive must be true or false"))?,
    };
    if target == "*" {
        return Err(invalid(
            "target * is refused: it reaches every entity; name the target or a classname prefix",
        ));
    }
    let destructive = DESTRUCTIVE_INPUTS
        .iter()
        .any(|d| d.eq_ignore_ascii_case(&input));
    if destructive && !allow && (target.contains('*') || is_classname(&target)) {
        return Err(invalid(format!(
            "{input} on {target:?} (a wildcard or classname target) can destroy many entities: pass allowDestructive true to send it"
        )));
    }
    let mut command = format!("ent_fire {target} {input}");
    match (&value, delay) {
        (Some(v), _) if v.contains(' ') => command.push_str(&format!(" \"{v}\"")),
        (Some(v), _) => command.push_str(&format!(" {v}")),
        // a delay needs a value slot: an empty quoted string
        (None, Some(_)) => command.push_str(" \"\""),
        (None, None) => {}
    }
    if let Some(d) = delay {
        command.push_str(&format!(" {d:.3}"));
    }
    Ok(command)
}

pub async fn ent_fire(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let command = ent_fire_command(&args)?;
    let mut bridge = state.bridge.lock().await;
    if !bridge.has("console.exec") {
        return Err(ToolError::not_implemented(
            "CapabilityMissing",
            "the bridge does not advertise console.exec (no command was sent)",
            json!({"missing": ["console.exec"]}),
        ));
    }
    let log = resolve_within(&state, &ctx, None, Purpose::Bridge).await?;
    let cursor = match ConsoleLog::new(&log.path).cursor() {
        Ok(Some(c)) => c,
        Ok(None) => return Err(console_error(ConsoleError::Missing, &log.path).retry_safe(true)),
        Err(e) => return Err(console_error(ConsoleError::Io(e), &log.path)),
    };
    let mut out = exec_console_confirmed(
        &mut bridge,
        &ctx,
        log.path.as_path(),
        &cursor,
        &[command.as_str()],
        Duration::from_millis(ENT_CONFIRM_MS),
    )
    .await
    .map_err(|e| {
        if e.data.get("commandsSent") == Some(&Value::Bool(false)) {
            e.retry_safe(true)
        } else {
            e
        }
    })?;
    out["effect"] = json!("not verified: confirmed means the engine ran the command in order, not that the entity reacted; check the effect with game_state, entity_find, sounds_drain or console_wait");
    Ok(out.into())
}

pub fn fire_weapon_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "shots": {"type": "integer", "minimum": 1, "maximum": SHOTS_MAX, "default": 1},
            "frames": {"type": "integer", "minimum": 1, "maximum": FRAMES_MAX, "default": FRAMES_DEFAULT},
            "weapon": {"type": "string"},
            "timeoutMs": {"type": "integer", "minimum": 1, "maximum": TIMEOUT_MAX_MS, "default": FIRE_TIMEOUT_DEFAULT_MS}
        },
        "additionalProperties": false
    })
}

fn target_schema() -> Value {
    json!({
        "description": format!("[x, y, z] point, or an entindex (1..{ENTINDEX_MAX})"),
        "oneOf": [
            {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
            {"type": "integer", "minimum": 1, "maximum": ENTINDEX_MAX}
        ]
    })
}

pub fn look_at_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "target": target_schema(),
            "eye": {"type": "boolean", "default": true},
            "hold": {"type": "boolean", "default": false},
            "timeoutMs": {"type": "integer", "minimum": 1, "maximum": TIMEOUT_MAX_MS, "default": AIM_TIMEOUT_DEFAULT_MS}
        },
        "required": ["target"],
        "additionalProperties": false
    })
}

pub fn use_entity_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "target": target_schema(),
            "maxDistance": {"type": "number", "exclusiveMinimum": 0, "maximum": MAX_DISTANCE_LIMIT, "default": MAX_DISTANCE_DEFAULT},
            "timeoutMs": {"type": "integer", "minimum": 1, "maximum": TIMEOUT_MAX_MS, "default": USE_TIMEOUT_DEFAULT_MS}
        },
        "required": ["target"],
        "additionalProperties": false
    })
}

pub fn ent_fire_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "target": {"type": "string", "maxLength": ENT_TARGET_MAX, "pattern": "^[A-Za-z0-9_!*.\\-]{1,64}$"},
            "input": {"type": "string", "maxLength": ENT_INPUT_MAX, "pattern": "^[A-Za-z0-9_]{1,64}$"},
            "value": {"type": "string", "maxLength": ENT_VALUE_MAX, "pattern": "^[A-Za-z0-9_.\\- ]{0,128}$"},
            "delay": {"type": "number", "minimum": 0, "maximum": DELAY_MAX},
            "allowDestructive": {"type": "boolean", "default": false, "description": "for Kill/KillHierarchy on a wildcard or classname"}
        },
        "required": ["target", "input"],
        "additionalProperties": false
    })
}

pub fn register(r: &mut Registry<ClientState>) {
    r.register(
        ToolSpec::new(
            "fire_weapon",
            "Shoot: per shot, release then tap attack until clip1 drops by one, then check the shot sounds. Refused before sending (PrerequisiteFailed): wrong weapon, no clip (knife), clip1 below shots. A timeout is not retry-safe (shotsFired says how far it got).",
            Mutability::Mutating,
            &["input.set", "game.local"],
        )
        .with_schema(fire_weapon_schema()),
        fire_weapon,
    );
    r.register(
        ToolSpec::new(
            "look_at",
            "Aim the view at a point or an entity (entindex) and wait until it is within 1 degree. hold false (default) then clears the override and reads the view back (snappedBack: true if it moved over 1 degree).",
            Mutability::Mutating,
            &["input.set", "game.local", "game.entities"],
        )
        .with_schema(look_at_schema()),
        look_at,
    );
    r.register(
        ToolSpec::new(
            "use_entity",
            "Use an entity: aim, press use, wait 32 ticks, re-read it. Refused (PrerequisiteFailed) when farther than maxDistance (64) from the eye: setpos near it first. ok is true only with evidence: use sounds, an origin/angles change, or the slot emptying.",
            Mutability::Mutating,
            &["input.set", "game.local", "game.entities"],
        )
        .with_schema(use_entity_schema()),
        use_entity,
    );
    r.register(
        ToolSpec::new(
            "ent_fire",
            "Send `ent_fire <target> <input> [value] [delay]` through console.exec and confirm the engine ran it; the effect is NOT verified (read it back). target * is refused; Kill/KillHierarchy on a wildcard or classname target needs allowDestructive.",
            Mutability::Mutating,
            &["console.exec"],
        )
        .with_schema(ent_fire_schema()),
        ent_fire,
    );
    r.register(
        ToolSpec::new(
            "setpos",
            "Teleport the local player (setpos_exact/setang_exact), then wait until it is at origin and stopped. floor true: origin.z is only an upper bound (FellOutOfMap past 512 u or below minZ). Refused while an input.set angle override is active (input_clear first). Once sent, errors are not retry-safe.",
            Mutability::Mutating,
            &["console.exec", "game.local"],
        )
        .with_schema(setpos_schema()),
        setpos,
    );
    r.register(
        ToolSpec::new(
            "input_sequence",
            "Run up to 32 input.set steps in order. Each step takes the input_set fields and holds holdTicks game ticks (default 1, max 640). Presses persist across steps until released; tap frames are client frames. Not for: a single press (input_set). input.clear is sent at the end and on error. ToolBudgetExceeded (retry-safe) when the steps cannot fit the call budget. An error after the first input.set is not retry-safe.",
            Mutability::Mutating,
            &["input.set", "input.clear", "game.local"],
        )
        .with_schema(input_sequence_schema()),
        input_sequence,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::bridge_tools::wire_angles;

    fn close(a: [f64; 3], b: [f64; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
    }

    #[test]
    fn aim_straight_ahead_is_zero() {
        assert!(close(
            aim_angles([0.0, 0.0, 64.0], [100.0, 0.0, 64.0]),
            [0.0, 0.0, 0.0]
        ));
    }

    #[test]
    fn aim_pitch_is_positive_looking_down() {
        // 45 degrees up is -45 (Source pitch), 45 down is +45
        assert!(close(
            aim_angles([0.0, 0.0, 0.0], [100.0, 0.0, 100.0]),
            [-45.0, 0.0, 0.0]
        ));
        assert!(close(
            aim_angles([0.0, 0.0, 0.0], [100.0, 0.0, -100.0]),
            [45.0, 0.0, 0.0]
        ));
    }

    #[test]
    fn aim_straight_up_and_down_clamp_to_89() {
        assert!(close(
            aim_angles([0.0, 0.0, 0.0], [0.0, 0.0, 500.0]),
            [-PITCH_LIMIT, 0.0, 0.0]
        ));
        assert!(close(
            aim_angles([0.0, 0.0, 0.0], [0.0, 0.0, -500.0]),
            [PITCH_LIMIT, 0.0, 0.0]
        ));
    }

    #[test]
    fn aim_yaw_quadrants() {
        // yaw 0 is +x, 90 is +y
        assert!(close(
            aim_angles([0.0; 3], [0.0, 10.0, 0.0]),
            [0.0, 90.0, 0.0]
        ));
        assert!(close(
            aim_angles([0.0; 3], [0.0, -10.0, 0.0]),
            [0.0, -90.0, 0.0]
        ));
        assert!(close(
            aim_angles([0.0; 3], [10.0, 10.0, 0.0]),
            [0.0, 45.0, 0.0]
        ));
        let back = aim_angles([0.0; 3], [-10.0, 0.0, 0.0]);
        assert!((back[1].abs() - 180.0).abs() < 1e-9, "{back:?}");
    }

    #[test]
    fn aim_wraps_into_the_wire_range() {
        // the back-facing yaw (+-180) leaves the wire as -180, never +180
        let back = wire_angles(aim_angles([0.0; 3], [-10.0, 0.0, 0.0]), "t").unwrap();
        assert!((-180.0..180.0).contains(&back[1]), "{back:?}");
        let wrapped = wire_angles([0.0, 190.0, 0.0], "t").unwrap();
        assert!((wrapped[1] + 170.0).abs() < 1e-9);
    }

    #[test]
    fn aim_from_the_eye_not_the_origin() {
        // level with the eye (z 64): level aim from the eye, but a downward aim from the origin
        let eye = [0.0, 0.0, 64.0];
        assert!(close(aim_angles(eye, [100.0, 0.0, 64.0]), [0.0, 0.0, 0.0]));
        assert!(aim_angles([0.0; 3], [100.0, 0.0, 64.0])[0] < 0.0);
    }

    #[test]
    fn target_parsing() {
        assert_eq!(
            parse_target(Some(&json!([1, 2, 3]))).unwrap(),
            Target::Point([1.0, 2.0, 3.0])
        );
        assert_eq!(parse_target(Some(&json!(364))).unwrap(), Target::Index(364));
        for bad in [
            json!(0),
            // One past the last walkable slot: MAX_EDICTS = ENTINDEX_MAX + 1.
            json!(s2sdk_sys::generated::MAX_EDICTS),
            json!(-1),
            json!([1, 2]),
            json!("x"),
            json!([1, 2, 99999]),
        ] {
            assert!(parse_target(Some(&bad)).is_err(), "{bad}");
        }
        assert!(parse_target(None).is_err());
    }

    #[test]
    fn sound_name_rules() {
        assert!(is_shot_sound("Weapon_hkp2000.Single"));
        assert!(!is_shot_sound("Weapon_hkp2000.Reload"));
        assert!(!is_shot_sound("Default.BulletImpact"));
        assert!(is_impact_sound("Default.BulletImpact"));
        // a button or door sound counts only near the target and only with an origin
        assert!(is_use_sound("Buttons.snd9", 0, None, Some(10.0)));
        assert!(is_use_sound("Doors.Move", 0, None, Some(USE_SOUND_RADIUS)));
        assert!(!is_use_sound("Buttons.snd9", 0, None, Some(500.0)));
        assert!(!is_use_sound("Buttons.snd9", 0, None, None));
        // the target's own entindex always counts, whatever the name or distance
        assert!(is_use_sound("Player.Footstep", 370, Some(370), None));
        assert!(!is_use_sound("Player.Footstep", 5, Some(370), Some(1.0)));
    }

    #[test]
    fn view_moved_wraps_yaw() {
        assert!((view_moved([0.0, 179.0, 0.0], [0.0, -179.0, 0.0]) - 2.0).abs() < 1e-9);
        assert!((view_moved([10.0, 0.0, 0.0], [12.5, 0.0, 0.0]) - 2.5).abs() < 1e-9);
    }

    #[test]
    fn ent_fire_refuses_wildcards_and_destructive_classnames() {
        let refused = |v: Value| ent_fire_command(&v).unwrap_err();
        // * is refused outright, whatever the input
        assert_eq!(
            refused(json!({"target": "*", "input": "Open"})).name,
            "InvalidParams"
        );
        assert_eq!(
            refused(json!({"target": "*", "input": "Kill", "allowDestructive": true})).name,
            "InvalidParams"
        );
        // Kill on a wildcard or classname needs allowDestructive
        assert!(ent_fire_command(&json!({"target": "prop*", "input": "Kill"})).is_err());
        assert!(
            ent_fire_command(&json!({"target": "prop_dynamic", "input": "KillHierarchy"})).is_err()
        );
        assert_eq!(
            ent_fire_command(
                &json!({"target": "prop_dynamic", "input": "Kill", "allowDestructive": true})
            )
            .unwrap(),
            "ent_fire prop_dynamic Kill"
        );
        // a named target is not a class: Kill is allowed; so is a non-destructive input on a wildcard
        assert_eq!(
            ent_fire_command(&json!({"target": "door_1", "input": "Kill"})).unwrap(),
            "ent_fire door_1 Kill"
        );
        assert_eq!(
            ent_fire_command(&json!({"target": "prop*", "input": "Open"})).unwrap(),
            "ent_fire prop* Open"
        );
        assert!(ent_fire_command(
            &json!({"target": "t", "input": "Kill", "allowDestructive": "yes"})
        )
        .is_err());
    }

    #[test]
    fn ent_fire_builds_the_command_and_rejects_unsafe_text() {
        assert_eq!(
            ent_fire_command(&json!({"target": "!player", "input": "sethealth", "value": "50"}))
                .unwrap(),
            "ent_fire !player sethealth 50"
        );
        assert_eq!(
            ent_fire_command(&json!({"target": "door_1", "input": "Open"})).unwrap(),
            "ent_fire door_1 Open"
        );
        assert_eq!(
            ent_fire_command(
                &json!({"target": "t", "input": "Say", "value": "hi there", "delay": 2})
            )
            .unwrap(),
            "ent_fire t Say \"hi there\" 2.000"
        );
        assert_eq!(
            ent_fire_command(&json!({"target": "t", "input": "Press", "delay": 0.5})).unwrap(),
            "ent_fire t Press \"\" 0.500"
        );
        for bad in [
            json!({"target": "t", "input": "Kill", "value": "1; quit"}),
            json!({"target": "t", "input": "Kill", "value": "a\"b"}),
            json!({"target": "t", "input": "Kill", "value": "a\nb"}),
            json!({"target": "t;x", "input": "Kill"}),
            json!({"target": "t", "input": "Kill!"}),
            json!({"target": "", "input": "Kill"}),
            json!({"target": "t", "input": "Kill", "delay": 61}),
            json!({"target": "t", "input": "Kill", "delay": -1}),
            json!({"target": "t", "input": "Kill", "extra": 1}),
            json!({"target": "t"}),
        ] {
            let err = ent_fire_command(&bad).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}");
        }
    }
}
