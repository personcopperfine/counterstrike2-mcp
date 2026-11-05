//! The `game_wait` engine (client-mcp keeps the tool wrapper): wait until a predicate over the local player holds (tick advanced, moved, stopped,
//! weapon selected, at a target origin and view), polling `game.local` server-side. One bridge lock
//! for the whole wait; one `game.local` per sample; between real samples the wait sleeps only
//! `pollMs - request_elapsed`, never past the call budget. ReadOnly: nothing but `game.local` is sent.
//! `run_wait` is the sampling loop without the tool wrapper, so other tools (setpos) can wait in-process
//! with their own predicate and budget, under a bridge lock they already hold.
use crate::bridge::{BridgeClient, RequestOpts};
use crate::tools::{CallCtx, ToolError};
use serde_json::{json, Map, Value};
use std::time::{Duration, Instant};

const TIMEOUT_DEFAULT_MS: u64 = 5000;
const POLL_DEFAULT_MS: u64 = 100;
pub const POLL_MIN_MS: u64 = 50;
pub const POLL_MAX_MS: u64 = 1000;
pub const TICKS_MAX: u64 = 1_000_000;
/// Headroom past the wait's remainder for a sample's bridge deadline (about 15 frames).
pub const SAMPLE_SLACK_MS: u64 = 250;
/// Keep this much of the call budget for the response after waiting (same as console_wait).
const BUDGET_MARGIN: Duration = Duration::from_millis(500);
/// Speed (units/s) below which the player counts as standing still.
const STOPPED_SPEED: f64 = 1.0;
/// Distance a rest must be from the pre-command origin to count as the teleport having landed.
const REST_MOVED_MIN: f64 = 0.01;
/// `game.local` player.flags bit FL_ONGROUND: the player stands on something.
pub const FL_ONGROUND: i64 = 1;
/// setpos `floor`: a landing this far below the requested z is a fall out of the map.
pub const FLOOR_MAX_DROP: f64 = 512.0;
const MOVED_MIN_DISTANCE: f64 = 1.0;
const AT_TOLERANCE_DEFAULT: f64 = 2.0;
const AT_Z_TOLERANCE_DEFAULT: f64 = 4.0;
const ANGLE_TOLERANCE_DEFAULT: f64 = 1.0;
pub const UNTIL: [&str; 8] = [
    "tick",
    "moved",
    "stopped",
    "weapon",
    "at",
    "roundLive",
    "roundRestarted",
    "ammo",
];
const KEYS: [&str; 19] = [
    "until",
    "ticks",
    "minDistance",
    "weapon",
    "defIndex",
    "from",
    "origin",
    "tolerance",
    "zTolerance",
    "angles",
    "angleTolerance",
    "requireStopped",
    "clip1Delta",
    "clip1",
    "entindex",
    "roundStartTime",
    "roundsPlayed",
    "timeoutMs",
    "pollMs",
];
/// The keys that belong to one predicate (every key in `KEYS` but `until`, `timeoutMs`, `pollMs`).
const PREDICATE_KEYS: [&str; 16] = [
    "ticks",
    "minDistance",
    "weapon",
    "defIndex",
    "from",
    "origin",
    "tolerance",
    "zTolerance",
    "angles",
    "angleTolerance",
    "requireStopped",
    "clip1Delta",
    "clip1",
    "entindex",
    "roundStartTime",
    "roundsPlayed",
];
const CLIP1_DELTA_MAX: i64 = 30;
const AT_KEYS: [&str; 6] = [
    "origin",
    "tolerance",
    "zTolerance",
    "angles",
    "angleTolerance",
    "requireStopped",
];

/// The `until` predicate with its own parameters.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// `tick >= baseline.tick + ticks`
    Tick { ticks: u64 },
    /// 3D distance from the baseline origin >= `min_distance`, and the tick advanced
    Moved { min_distance: f64 },
    /// speed < 1 on two consecutive samples with the tick advancing between them
    Stopped,
    /// `activeWeapon.name == weapon` or `activeWeapon.defIndex == defIndex`
    Weapon { by: WeaponBy },
    /// origin at the target (see `AtSpec`); no baseline involved
    At(AtSpec),
    /// no warmup, freeze, team intro or restart, no round winner, local player alive (`game.rules` + `game.local`)
    RoundLive,
    /// the round restarted since the baseline (roundStartTime grew or roundsPlayed changed), and it is live
    RoundRestarted,
    /// `activeWeapon.clip1` moved from the baseline by `clip1_delta` (negative: at most that; positive: at
    /// least), with the same weapon as the baseline (entindex, defIndex) and, when given, its name. A weapon
    /// switch keeps the wait going; no active weapon now is not a match.
    Ammo {
        clip1_delta: i64,
        weapon: Option<String>,
    },
    /// setpos `floor` only (`parse_request` never produces it): see `floor_matches`
    Floor(FloorSpec),
}

/// The setpos `floor` verification: the `at` target (xy and z tolerances; the z tolerance is an upper
/// bound only, since the player may fall) and an optional lowest z.
#[derive(Debug, Clone, PartialEq)]
pub struct FloorSpec {
    pub at: AtSpec,
    /// the player counts as fallen out of the map below this z
    pub min_z: Option<f64>,
}

/// The `at` predicate: where the player stands and looks, with its tolerances.
#[derive(Debug, Clone, PartialEq)]
pub struct AtSpec {
    pub origin: [f64; 3],
    /// horizontal (xy) distance from `origin`
    pub tolerance: f64,
    /// vertical (z) distance from `origin`
    pub z_tolerance: f64,
    /// `[pitch, yaw, roll]` target for the view; pitch and yaw are compared, roll is not
    pub angles: Option<[f64; 3]>,
    /// largest pitch or yaw difference (wrapped to [-180, 180)) that still counts as aimed
    pub angle_tolerance: f64,
    /// speed < 1 on this sample and the previous one
    pub require_stopped: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WeaponBy {
    Name(String),
    DefIndex(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Baseline {
    pub tick: u64,
    /// `None` when the first sample had no origin (a failed schema read); `moved` then cannot run
    pub origin: Option<[f64; 3]>,
    /// `activeWeapon.clip1` of the first sample (`ammo`), or the caller's `clip1`; `None` without one
    pub clip1: Option<i64>,
    /// `activeWeapon` of the first sample (`ammo` compares entindex and defIndex), or `{entindex}` from the caller
    pub weapon: Value,
    /// round state the restart check compares against (`roundRestarted`)
    pub round: RoundBase,
}

/// The `game.rules` values `roundRestarted` compares: caller-given, else the first sample's.
/// The caller's ammo baseline: the clip count and the weapon's entindex before the shot.
#[derive(Debug, Clone, PartialEq)]
pub struct AmmoFrom {
    pub clip1: i64,
    pub entindex: i64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoundBase {
    pub start_time: Option<f64>,
    pub rounds_played: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WaitRequest {
    pub predicate: Predicate,
    /// `from` for tick/moved; default is the first sample
    pub from: Option<Baseline>,
    /// `roundStartTime` / `roundsPlayed` for roundRestarted; unset parts come from the first sample
    pub round: RoundBase,
    /// `clip1` + `entindex` for ammo: the baseline when a shot may land before the first sample
    pub ammo_from: Option<AmmoFrom>,
    pub timeout_ms: u64,
    pub poll_ms: u64,
}

impl Predicate {
    pub fn name(&self) -> &'static str {
        match self {
            Predicate::Tick { .. } => "tick",
            Predicate::Moved { .. } => "moved",
            Predicate::Stopped => "stopped",
            Predicate::Weapon { .. } => "weapon",
            Predicate::At(_) => "at",
            Predicate::RoundLive => "roundLive",
            Predicate::RoundRestarted => "roundRestarted",
            Predicate::Ammo { .. } => "ammo",
            Predicate::Floor(_) => "floor",
        }
    }

    /// Needs `game.rules` as well as `game.local` on every sample.
    pub fn reads_rules(&self) -> bool {
        matches!(self, Predicate::RoundLive | Predicate::RoundRestarted)
    }

    /// Keeps waiting while the local player is dead (a round wait ends after a respawn, not at a death).
    pub fn allows_dead(&self) -> bool {
        self.reads_rules()
    }
}

/// The `game.local` and `game.rules` fields the predicates read.
#[derive(Debug, Clone)]
pub struct Sample {
    pub tick: u64,
    /// null from the bridge when the schema read failed; only the predicates that read it need it
    pub origin: Option<[f64; 3]>,
    pub velocity: Option<[f64; 3]>,
    /// `player.flags` (bit 0 is FL_ONGROUND); null from the bridge when the schema read failed
    pub flags: Option<i64>,
    /// top-level `viewAngles` (the pawn's eye angles `[pitch, yaw, roll]`)
    pub view_angles: Option<[f64; 3]>,
    pub weapon: Value,
    /// `player.alive`; false only when the predicate allows a dead player (`allows_dead`)
    pub alive: bool,
    /// the `game.rules` reply, when the predicate reads the round
    pub rules: Option<Value>,
}

/// How a `run_wait` ended.
pub enum WaitEnd {
    /// the predicate held on `sample`; `base` is the baseline it was measured from
    Matched { base: Baseline, sample: Sample },
    /// the timeout ran out. `last` is the last sample read. `bridge_error` is set when a later
    /// sample was dropped by the bridge queue, which ends the wait early as a timeout.
    Expired {
        last: Option<Sample>,
        bridge_error: Option<ToolError>,
    },
}

/// What `run_wait` measured, whichever way it ended.
pub struct WaitOutcome {
    pub end: WaitEnd,
    pub samples: u64,
    pub waited: Duration,
    /// the requested timeout, clamped to the call budget
    pub timeout: Duration,
    /// the requested timeout was longer than the budget allowed
    pub clamped: bool,
    /// the first sample (or `from`), as the wait saw the player when it started
    pub before: Value,
    /// `run_wait_settle` only: ended early because the player came to rest off the target. `end` is
    /// then `Expired` with that sample as `last`, and no bridge error.
    pub rested: bool,
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(message)
}

/// Optional unsigned integer field in `min..=max` (null counts as absent).
fn uint(
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

/// Optional finite number >= 0 (null counts as absent).
fn nonneg(obj: &Map<String, Value>, key: &str, default: f64) -> Result<f64, ToolError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_f64()
            .filter(|d| d.is_finite() && *d >= 0.0)
            .ok_or_else(|| invalid(format!("{key} must be a number >= 0"))),
    }
}

/// Three finite numbers.
pub fn vec3(v: &Value) -> Option<[f64; 3]> {
    let a = v.as_array()?;
    if a.len() != 3 {
        return None;
    }
    let mut out = [0.0; 3];
    for (o, x) in out.iter_mut().zip(a) {
        *o = x.as_f64().filter(|f| f.is_finite())?;
    }
    Some(out)
}

fn baseline(v: &Value) -> Result<Baseline, ToolError> {
    let obj = v
        .as_object()
        .ok_or_else(|| invalid("from must be {\"tick\": u64, \"origin\": [x, y, z]}"))?;
    let tick = obj
        .get("tick")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("from.tick must be a non-negative integer"))?;
    let origin = obj
        .get("origin")
        .and_then(vec3)
        .ok_or_else(|| invalid("from.origin must be three finite numbers"))?;
    Ok(Baseline {
        tick,
        origin: Some(origin),
        clip1: None,
        weapon: Value::Null,
        round: RoundBase::default(),
    })
}

/// until ammo: `clip1Delta` (default -1; non-zero, within +-CLIP1_DELTA_MAX), optional `weapon` name, and
/// the caller's baseline `clip1` + `entindex` (both or neither).
fn ammo_params(
    obj: &Map<String, Value>,
) -> Result<(i64, Option<String>, Option<AmmoFrom>), ToolError> {
    let delta = match obj.get("clip1Delta") {
        None | Some(Value::Null) => -1,
        Some(v) => v
            .as_i64()
            .filter(|d| *d != 0 && d.abs() <= CLIP1_DELTA_MAX)
            .ok_or_else(|| invalid("clip1Delta must be a non-zero integer -30..30"))?,
    };
    let weapon = match obj.get("weapon") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let w = v
                .as_str()
                .filter(|w| is_weapon_name(w))
                .ok_or_else(|| {
                    invalid("weapon must look like weapon_ak47 (lower-case, weapon_ prefix)")
                })?;
            Some(w.to_string())
        }
    };
    let clip1 = obj.get("clip1").filter(|v| !v.is_null());
    let entindex = obj.get("entindex").filter(|v| !v.is_null());
    let from =
        match (clip1, entindex) {
            (None, None) => None,
            (Some(c), Some(e)) => Some(AmmoFrom {
                clip1: c
                    .as_u64()
                    .filter(|c| *c <= 1000)
                    .ok_or_else(|| invalid("clip1 must be an integer 0..1000"))?
                    as i64,
                entindex: e
                    .as_u64()
                    // The ehandle identity index bound: CEntityHandle::m_EntityIndex is
                    // ENTITY_INDEX_BITS (15) bits wide, so a valid entindex is
                    // 0..=ENTITY_INDEX_MASK, i.e. < 1<<ENTITY_INDEX_BITS (0x8000). This is the
                    // identity bound, not NUM_ENT_ENTRIES (1<<16, the ehandle entry space) and
                    // not MAX_EDICTS (16384) - see sdk-migration-design.md risk R2.
                    .filter(|e| *e < (1u64 << s2sdk_sys::generated::ENTITY_INDEX_BITS))
                    .ok_or_else(|| {
                        invalid(format!(
                            "entindex must be an integer 0..{}",
                            s2sdk_sys::generated::ENTITY_INDEX_MASK
                        ))
                    })? as i64,
            }),
            _ => return Err(invalid(
                "clip1 and entindex come together: the clip count and the weapon it was read from",
            )),
        };
    Ok((delta, weapon, from))
}

/// The roundRestarted baseline keys (`roundStartTime` seconds, `roundsPlayed` count); both optional.
fn round_base(obj: &Map<String, Value>) -> Result<RoundBase, ToolError> {
    let start_time = match obj.get("roundStartTime") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64()
                .filter(|t| t.is_finite() && *t >= 0.0)
                .ok_or_else(|| invalid("roundStartTime must be a non-negative number"))?,
        ),
    };
    let rounds_played = match obj.get("roundsPlayed") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|n| *n <= i64::MAX as u64)
                .ok_or_else(|| invalid("roundsPlayed must be a non-negative integer"))?
                as i64,
        ),
    };
    Ok(RoundBase {
        start_time,
        rounds_played,
    })
}

/// Validate the `until: "at"` keys into a spec. Nothing is sent.
fn at_spec(obj: &Map<String, Value>) -> Result<AtSpec, ToolError> {
    let origin = obj
        .get("origin")
        .and_then(vec3)
        .ok_or_else(|| invalid("until at needs origin: [x, y, z] (three finite numbers)"))?;
    let angles = match obj.get("angles") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            vec3(v).ok_or_else(|| invalid("angles must be [pitch, yaw, roll] (finite numbers)"))?,
        ),
    };
    if angles.is_none() && obj.get("angleTolerance").is_some_and(|v| !v.is_null()) {
        return Err(invalid("angleTolerance needs angles"));
    }
    let require_stopped = match obj.get("requireStopped") {
        None | Some(Value::Null) => true,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| invalid("requireStopped must be true or false"))?,
    };
    Ok(AtSpec {
        origin,
        tolerance: nonneg(obj, "tolerance", AT_TOLERANCE_DEFAULT)?,
        z_tolerance: nonneg(obj, "zTolerance", AT_Z_TOLERANCE_DEFAULT)?,
        angles,
        angle_tolerance: nonneg(obj, "angleTolerance", ANGLE_TOLERANCE_DEFAULT)?,
        require_stopped,
    })
}

/// Validate `game_wait` arguments into a request. Nothing is sent.
pub fn parse_request(args: &Value) -> Result<WaitRequest, ToolError> {
    let obj = match args {
        Value::Object(m) => m,
        Value::Null => {
            return Err(invalid(
                "until is required: tick|moved|stopped|weapon|at|roundLive|roundRestarted|ammo",
            ))
        }
        _ => return Err(invalid("arguments must be an object")),
    };
    if let Some(k) = obj.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(invalid(format!(
            "unknown key {k:?} in game_wait (allowed: {})",
            KEYS.join(", ")
        )));
    }
    let until = obj
        .get("until")
        .and_then(Value::as_str)
        .filter(|u| UNTIL.contains(u))
        .ok_or_else(|| {
            invalid(
                "until must be one of tick|moved|stopped|weapon|at|roundLive|roundRestarted|ammo",
            )
        })?;
    let applies: &[&str] = match until {
        "tick" => &["ticks", "from"],
        "moved" => &["minDistance", "from"],
        "weapon" => &["weapon", "defIndex"],
        "at" => &AT_KEYS,
        "roundRestarted" => &["roundStartTime", "roundsPlayed"],
        "ammo" => &["clip1Delta", "weapon", "clip1", "entindex"],
        _ => &[],
    };
    for key in PREDICATE_KEYS {
        if obj.get(key).is_some_and(|v| !v.is_null()) && !applies.contains(&key) {
            return Err(invalid(format!("{key} does not apply to until {until}")));
        }
    }
    let predicate = match until {
        "tick" => Predicate::Tick {
            ticks: uint(obj, "ticks", 1, 1, TICKS_MAX)?,
        },
        "moved" => Predicate::Moved {
            min_distance: nonneg(obj, "minDistance", MOVED_MIN_DISTANCE)?,
        },
        "weapon" => {
            let by = match (obj.get("weapon"), obj.get("defIndex")) {
                (Some(w), None) if !w.is_null() => {
                    let w = w
                        .as_str()
                        .ok_or_else(|| invalid("weapon must be a string"))?;
                    if !is_weapon_name(w) {
                        return Err(invalid(
                            "weapon must look like weapon_ak47 (lower-case, weapon_ prefix)",
                        ));
                    }
                    WeaponBy::Name(w.to_string())
                }
                (None, Some(d)) if !d.is_null() => WeaponBy::DefIndex(
                    d.as_u64()
                        .filter(|d| *d <= u64::from(u16::MAX))
                        .ok_or_else(|| invalid("defIndex must be an integer 0..65535"))?,
                ),
                _ => {
                    return Err(invalid(
                        "until weapon needs exactly one of weapon or defIndex",
                    ))
                }
            };
            Predicate::Weapon { by }
        }
        "at" => Predicate::At(at_spec(obj)?),
        "roundLive" => Predicate::RoundLive,
        "roundRestarted" => Predicate::RoundRestarted,
        "ammo" => {
            let (clip1_delta, weapon, _) = ammo_params(obj)?;
            Predicate::Ammo {
                clip1_delta,
                weapon,
            }
        }
        _ => Predicate::Stopped,
    };
    let from = match obj.get("from") {
        None | Some(Value::Null) => None,
        Some(v) => Some(baseline(v)?),
    };
    Ok(WaitRequest {
        predicate,
        from,
        round: round_base(obj)?,
        ammo_from: ammo_params(obj)?.2,
        timeout_ms: uint(obj, "timeoutMs", TIMEOUT_DEFAULT_MS, 1, u64::MAX)?,
        poll_ms: uint(obj, "pollMs", POLL_DEFAULT_MS, POLL_MIN_MS, POLL_MAX_MS)?,
    })
}

/// Fail fast (not a timeout) when there is no live local player to wait on; `dead_ok` waits on a dead one.
pub fn player_of(v: &Value, dead_ok: bool) -> Result<&Value, ToolError> {
    let in_game = v.get("inGame").and_then(Value::as_bool).unwrap_or(false);
    let player = v.get("player").filter(|p| p.is_object());
    let alive = player
        .and_then(|p| p.get("alive"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match player {
        Some(p) if in_game && (alive || dead_ok) => Ok(p),
        _ => Err(ToolError::new(
            "PrerequisiteFailed",
            "PrerequisiteFailed",
            format!("game_wait needs a live local player (inGame {in_game}, alive {alive})"),
        )
        .retry_safe(true)
        .with_data(json!({
            "inGame": in_game,
            "alive": alive,
            "map": v.get("map").cloned().unwrap_or(Value::Null),
            "hint": "start or join a map and spawn first; nothing was waited for",
        }))),
    }
}

pub fn sample_of(
    v: &Value,
    rules: Option<Value>,
    dead_ok: bool,
) -> Result<Sample, ToolError> {
    let player = player_of(v, dead_ok)?;
    Ok(Sample {
        tick: v
            .get("tick")
            .and_then(Value::as_u64)
            .ok_or_else(|| ToolError::internal("game.local has no tick"))?,
        origin: player.get("origin").and_then(vec3),
        velocity: player.get("velocity").and_then(vec3),
        flags: player.get("flags").and_then(Value::as_i64),
        view_angles: v.get("viewAngles").and_then(vec3),
        weapon: player.get("activeWeapon").cloned().unwrap_or(Value::Null),
        alive: player
            .get("alive")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        rules,
    })
}

/// Designer weapon name (`weapon_ak47`): lower-case letters, digits and `_`, `weapon_` prefix.
pub fn is_weapon_name(w: &str) -> bool {
    w.len() > 7
        && w.len() < 64
        && w.starts_with("weapon_")
        && w.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub fn clip1_of(weapon: &Value) -> Option<i64> {
    weapon.get("clip1").and_then(Value::as_i64)
}

/// A field a predicate needs but `game.local` sent as null (a failed schema read): retry-safe,
/// and only for the predicate that reads it. `path` is where the field sits in `game.local`.
fn need<T>(v: Option<T>, path: &str, until: &str) -> Result<T, ToolError> {
    v.ok_or_else(|| {
        ToolError::new(
            "SchemaUnavailable",
            "SchemaUnavailable",
            format!("game.local sent no {path}; until {until} needs it"),
        )
        .retry_safe(true)
        .with_data(json!({"field": path, "until": until}))
    })
}

pub fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).powi(2))
        .sum::<f64>()
        .sqrt()
}

/// Horizontal (xy) and vertical (z) distance between two origins.
fn split_distance(a: [f64; 3], b: [f64; 3]) -> (f64, f64) {
    (
        distance([a[0], a[1], 0.0], [b[0], b[1], 0.0]),
        (a[2] - b[2]).abs(),
    )
}

fn speed(v: [f64; 3]) -> f64 {
    distance(v, [0.0; 3])
}

/// Signed difference `a - b` of two angles in degrees, wrapped into [-180, 180).
fn angle_delta(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(360.0);
    if d >= 180.0 {
        d - 360.0
    } else {
        d
    }
}

fn weapon_matches(by: &WeaponBy, w: &Value) -> bool {
    match by {
        WeaponBy::Name(n) => w.get("name").and_then(Value::as_str) == Some(n.as_str()),
        WeaponBy::DefIndex(d) => w.get("defIndex").and_then(Value::as_u64) == Some(*d),
    }
}

/// Two samples in a row at speed < 1, the tick advancing (the rest `run_wait_settle` looks for).
fn at_rest(prev: Option<&Sample>, s: &Sample) -> bool {
    prev.is_some_and(|q| {
        s.tick > q.tick
            && s.velocity.is_some_and(|v| speed(v) < STOPPED_SPEED)
            && q.velocity.is_some_and(|v| speed(v) < STOPPED_SPEED)
    })
}

/// Is the origin outside the target a settle wait is for: the `at` window, or the floor's xy tolerance.
fn off_target(p: &Predicate, s: &Sample) -> bool {
    let Some(o) = s.origin else { return false };
    match p {
        Predicate::At(t) => !in_window(t, o),
        Predicate::Floor(f) => split_distance(o, f.at.origin).0 > f.at.tolerance,
        _ => false,
    }
}

/// Is origin `o` inside the `at` xy and z tolerances?
fn in_window(t: &AtSpec, o: [f64; 3]) -> bool {
    let (xy, dz) = split_distance(o, t.origin);
    xy <= t.tolerance && dz <= t.z_tolerance
}

/// The `at` predicate on sample `s`: origin within both tolerances, view within the angle tolerance
/// (pitch and yaw, wrapped). With `requireStopped`, also speed < 1 on `s`, and the sample before it
/// is inside the window too, still, and at an earlier tick (a frozen tick proves nothing).
fn at_matches(t: &AtSpec, s: &Sample, prev: Option<&Sample>) -> Result<bool, ToolError> {
    let now = need(s.origin, "player.origin", "at")?;
    if !in_window(t, now) {
        return Ok(false);
    }
    if let Some(a) = t.angles {
        let eye = need(s.view_angles, "viewAngles", "at")?;
        if angle_delta(eye[0], a[0]).abs() > t.angle_tolerance
            || angle_delta(eye[1], a[1]).abs() > t.angle_tolerance
        {
            return Ok(false);
        }
    }
    if !t.require_stopped {
        return Ok(true);
    }
    let velocity = need(s.velocity, "player.velocity", "at")?;
    Ok(speed(velocity) < STOPPED_SPEED
        && prev.is_some_and(|q| {
            s.tick > q.tick
                && q.origin.is_some_and(|o| in_window(t, o))
                && q.velocity.is_some_and(|v| speed(v) < STOPPED_SPEED)
        }))
}

/// Where a sample stands against the `at` target: xy and z distance of the origin, and the signed
/// pitch and yaw differences of the view (`eye - target`, wrapped) when `angles` is set. Null where
/// the sample lacks the field.
pub fn to_target(t: &AtSpec, s: &Sample) -> Value {
    let (xy, z) = match s.origin {
        Some(o) => {
            let (xy, z) = split_distance(o, t.origin);
            (json!(xy), json!(z))
        }
        None => (Value::Null, Value::Null),
    };
    let mut out = json!({"xy": xy, "z": z});
    if let Some(a) = t.angles {
        out["angle"] = match s.view_angles {
            Some(eye) => json!({
                "pitch": angle_delta(eye[0], a[0]),
                "yaw": angle_delta(eye[1], a[1]),
            }),
            None => Value::Null,
        };
    }
    out
}

/// A boolean `game.rules` field; null (a failed schema read) is retry-safe SchemaUnavailable.
fn rules_flag(rules: &Value, key: &str, until: &str) -> Result<bool, ToolError> {
    need(
        rules.get(key).and_then(Value::as_bool),
        &format!("game.rules.{key}"),
        until,
    )
}

/// Round live: no warmup, freeze, team intro or restart, no round winner, and the local player alive.
pub fn round_live(s: &Sample, until: &str) -> Result<bool, ToolError> {
    let rules = need(s.rules.as_ref(), "game.rules", until)?;
    let idle = !rules_flag(rules, "warmup", until)?
        && !rules_flag(rules, "freeze", until)?
        && !rules_flag(rules, "teamIntro", until)?
        && !rules_flag(rules, "gameRestart", until)?;
    let winner = need(
        rules.get("roundWinStatus").and_then(Value::as_i64),
        "game.rules.roundWinStatus",
        until,
    )?;
    Ok(idle && winner == 0 && s.alive)
}

/// The round restarted since the baseline: roundStartTime grew, or roundsPlayed changed.
/// A restart since the baseline. Any of these counts:
/// roundStartTime grew (when both the baseline and this sample have it); roundsPlayed changed (same
/// rule); or gameRestart was set on the previous sample (the restart window was seen and this sample
/// is past it: the fallback when the schema gave no start time). A null in this sample skips its
/// comparison instead of failing the wait.
fn round_restarted(
    base: &Baseline,
    s: &Sample,
    prev: Option<&Sample>,
    until: &str,
) -> Result<bool, ToolError> {
    let rules = need(s.rules.as_ref(), "game.rules", until)?;
    let mut restarted = false;
    if let (Some(t0), Some(t)) = (
        base.round.start_time,
        rules.get("roundStartTime").and_then(Value::as_f64),
    ) {
        restarted |= t > t0;
    }
    if let (Some(n0), Some(n)) = (
        base.round.rounds_played,
        rules.get("roundsPlayed").and_then(Value::as_i64),
    ) {
        restarted |= n != n0;
    }
    // the restart window: gameRestart was set on the previous sample, so this sample is past it
    let window = prev
        .and_then(|q| q.rules.as_ref())
        .and_then(|r| r.get("gameRestart"))
        .and_then(Value::as_bool);
    restarted |= window == Some(true);
    Ok(restarted)
}

/// Same weapon as the baseline: the same entindex (when both are known) and defIndex (when both are
/// known); without a known entindex, the same name.
fn same_weapon(base: &Value, now: &Value) -> bool {
    let num = |w: &Value, k: &str| w.get(k).and_then(Value::as_i64);
    let def_ok = match (num(base, "defIndex"), num(now, "defIndex")) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    };
    match (num(base, "entindex"), num(now, "entindex")) {
        (Some(a), Some(b)) if a >= 0 && b >= 0 => a == b && def_ok,
        _ => {
            let name = |w: &Value| w.get("name").and_then(Value::as_str).map(str::to_string);
            name(base).is_some() && name(base) == name(now) && def_ok
        }
    }
}

/// Does sample `s` satisfy the predicate? `base` is `from` or the first sample; `prev` the sample before `s`.
fn matched(
    p: &Predicate,
    base: &Baseline,
    s: &Sample,
    prev: Option<&Sample>,
) -> Result<bool, ToolError> {
    Ok(match p {
        Predicate::RoundLive => round_live(s, p.name())?,
        Predicate::RoundRestarted => {
            round_restarted(base, s, prev, p.name())? && round_live(s, p.name())?
        }
        Predicate::Ammo {
            clip1_delta,
            weapon,
        } => {
            if base.weapon.is_null() && base.clip1.is_none() {
                return Err(ToolError::new(
                    "PrerequisiteFailed",
                    "PrerequisiteFailed",
                    "no active weapon in the baseline: select a weapon first, or pass clip1 and entindex",
                )
                .retry_safe(true));
            }
            // a switch, a drop or no weapon now keeps the wait going: only the baseline weapon counts
            if s.weapon.is_null() || !same_weapon(&base.weapon, &s.weapon) {
                return Ok(false);
            }
            if let Some(w) = weapon {
                if s.weapon.get("name").and_then(Value::as_str) != Some(w.as_str()) {
                    return Ok(false);
                }
            }
            let now = need(clip1_of(&s.weapon), "player.activeWeapon.clip1", p.name())?;
            let from = need(base.clip1, "player.activeWeapon.clip1", p.name())?;
            let moved = now - from;
            if *clip1_delta < 0 {
                moved <= *clip1_delta
            } else {
                moved >= *clip1_delta
            }
        }
        Predicate::Tick { ticks } => s.tick >= base.tick.saturating_add(*ticks),
        Predicate::Moved { min_distance } => {
            let now = need(s.origin, "player.origin", p.name())?;
            let from = need(base.origin, "player.origin", p.name())?;
            s.tick > base.tick && distance(from, now) >= *min_distance
        }
        Predicate::Stopped => {
            let now = need(s.velocity, "player.velocity", p.name())?;
            speed(now) < STOPPED_SPEED
                && prev.is_some_and(|q| {
                    q.velocity.is_some_and(|v| speed(v) < STOPPED_SPEED) && s.tick > q.tick
                })
        }
        Predicate::Weapon { by } => weapon_matches(by, &s.weapon),
        Predicate::At(t) => at_matches(t, s, prev)?,
        Predicate::Floor(f) => floor_matches(f, s, prev)?,
    })
}

/// The setpos `floor` predicate on sample `s`: origin within the xy tolerance, on the ground
/// (FL_ONGROUND), stopped (speed < 1 on this sample and the one before, at an earlier tick, both at
/// the target xy), not above the target z plus zTolerance, and the view within the angle tolerance
/// when `angles` is set. A sample at the target xy that fell more than FLOOR_MAX_DROP below the
/// target z, or below `minZ`, is an error (FellOutOfMap) and ends the wait. Those two checks need the
/// xy match, so a sample read before the teleport took effect cannot trip them.
fn floor_matches(f: &FloorSpec, s: &Sample, prev: Option<&Sample>) -> Result<bool, ToolError> {
    let t = &f.at;
    let now = need(s.origin, "player.origin", "floor")?;
    let xy_of = |o: [f64; 3]| split_distance(o, t.origin).0;
    if xy_of(now) > t.tolerance {
        return Ok(false);
    }
    let req_z = t.origin[2];
    let fell = req_z - now[2];
    if fell > FLOOR_MAX_DROP || f.min_z.is_some_and(|m| now[2] < m) {
        let reason = if fell > FLOOR_MAX_DROP {
            format!("fell {fell:.3} u below the requested z {req_z:.3} (limit {FLOOR_MAX_DROP} u)")
        } else {
            format!(
                "landed at z {:.3}, below minZ {:.3}",
                now[2],
                f.min_z.unwrap_or_default()
            )
        };
        return Err(ToolError::new(
            "FellOutOfMap",
            "FellOutOfMap",
            format!("player fell out of the map: {reason}"),
        )
        .retry_safe(false)
        .with_data(json!({
            "requestedZ": req_z,
            "z": now[2],
            "dropped": fell,
            "maxDrop": FLOOR_MAX_DROP,
            "minZ": f.min_z,
        })));
    }
    if let Some(a) = t.angles {
        let eye = need(s.view_angles, "viewAngles", "floor")?;
        if angle_delta(eye[0], a[0]).abs() > t.angle_tolerance
            || angle_delta(eye[1], a[1]).abs() > t.angle_tolerance
        {
            return Ok(false);
        }
    }
    if now[2] > req_z + t.z_tolerance {
        return Ok(false);
    }
    let flags = need(s.flags, "player.flags", "floor")?;
    if flags & FL_ONGROUND == 0 {
        return Ok(false);
    }
    let velocity = need(s.velocity, "player.velocity", "floor")?;
    Ok(speed(velocity) < STOPPED_SPEED
        && prev.is_some_and(|q| {
            s.tick > q.tick
                && q.origin.is_some_and(|o| xy_of(o) <= t.tolerance)
                && q.velocity.is_some_and(|v| speed(v) < STOPPED_SPEED)
        }))
}

pub fn view(s: &Sample) -> Value {
    json!({"tick": s.tick, "origin": s.origin, "viewAngles": s.view_angles, "activeWeapon": s.weapon})
}

pub fn millis(d: Duration) -> u64 {
    d.as_millis() as u64
}

/// One wait sample: `game.local`, then `game.rules` when the predicate reads the round. The first request
/// takes the liveness gate when `gated`; later ones are ungated (this loop already holds the bridge). The
/// rules request gets what is left of the sample's deadline `ms`, counted from `t0`.
async fn fetch_sample(
    bridge: &mut BridgeClient,
    ms: u64,
    gated: bool,
    rules: bool,
    t0: Instant,
) -> Result<(Value, Option<Value>), ToolError> {
    let local = if gated {
        bridge
            .request("game.local", json!({}), RequestOpts::deadline(ms))
            .await
    } else {
        bridge
            .request("game.local", json!({}), RequestOpts::ungated(ms))
            .await
    }
    .map(|r| r.result)
    .map_err(ToolError::from)?;
    if !rules {
        return Ok((local, None));
    }
    let left = ms.saturating_sub(millis(t0.elapsed())).max(1);
    let round = bridge
        .request("game.rules", json!({}), RequestOpts::ungated(left))
        .await
        .map(|r| r.result)
        .map_err(ToolError::from)?;
    Ok((local, Some(round)))
}

/// The baseline the first sample (or the caller's ammo baseline) sets.
fn first_baseline(req: &WaitRequest, s: &Sample) -> Baseline {
    let (clip1, weapon) = match &req.ammo_from {
        Some(f) => (Some(f.clip1), json!({"entindex": f.entindex})),
        None => (clip1_of(&s.weapon), s.weapon.clone()),
    };
    Baseline {
        tick: s.tick,
        origin: s.origin,
        clip1,
        weapon,
        round: first_round(&req.round, s.rules.as_ref()),
    }
}

/// The roundRestarted baseline of the first sample: the caller's values win, the rest is read from it.
fn first_round(given: &RoundBase, rules: Option<&Value>) -> RoundBase {
    RoundBase {
        start_time: given.start_time.or_else(|| {
            rules
                .and_then(|r| r.get("roundStartTime"))
                .and_then(Value::as_f64)
        }),
        rounds_played: given.rounds_played.or_else(|| {
            rules
                .and_then(|r| r.get("roundsPlayed"))
                .and_then(Value::as_i64)
        }),
    }
}

/// Poll `game.local` until `req.predicate` holds or the wait times out. The caller holds the bridge
/// lock. The timeout is clamped to the call budget minus `BUDGET_MARGIN`; too little budget left is
/// `ToolBudgetExceeded` before anything is sent. Errors other than the bridge queue dropping a later
/// sample are returned as they are.
pub async fn run_wait(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    req: &WaitRequest,
) -> Result<WaitOutcome, ToolError> {
    run_wait_settle(bridge, ctx, req, None).await
}

/// `run_wait`, and with `rest_from` set (setpos), also ends when the player comes to rest (two
/// stopped samples, tick advancing) off the target: outside the `at` window, or outside the floor's
/// xy tolerance. A rest at `rest_from` (the origin before the command) does not count: the command
/// has not moved the player yet.
pub async fn run_wait_settle(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    req: &WaitRequest,
    rest_from: Option<[f64; 3]>,
) -> Result<WaitOutcome, ToolError> {
    // Never wait past the call budget: the client would give up first. Too little budget left
    // to wait at all is the budget error, not a timeout.
    let remaining = ctx.budget.remaining();
    if remaining < BUDGET_MARGIN {
        return Err(ToolError::budget_exceeded(&ctx.budget));
    }
    let cap = remaining - BUDGET_MARGIN;
    let requested = Duration::from_millis(req.timeout_ms);
    let timeout = requested.min(cap);
    let poll = Duration::from_millis(req.poll_ms);
    let started = Instant::now();
    let mut base = req.from.clone();
    let mut before = Value::Null;
    let mut prev: Option<Sample> = None;
    let mut last: Option<Sample> = None;
    let mut samples: u64 = 0;
    // Set when a later sample was dropped by the bridge queue: the wait ends early, as a timeout.
    let mut bridge_error: Option<ToolError> = None;
    loop {
        if samples > 0 && started.elapsed() >= timeout {
            break;
        }
        let t0 = Instant::now();
        // A sample gets the budget deadline, but no more than the wait's remainder plus
        // SAMPLE_SLACK_MS: a main-thread stall then ends at the wait's timeout, not the budget's.
        // The slack is about 15 frames, so it never expires a healthy sample; left <= remaining - 500
        // keeps it inside the budget.
        let left = millis(timeout.saturating_sub(started.elapsed()));
        let deadline = ctx
            .budget
            .deadline_ms()
            .map(|budget| budget.min(left + SAMPLE_SLACK_MS));
        let sent = match deadline {
            Ok(ms) => fetch_sample(bridge, ms, samples == 0, req.predicate.reads_rules(), t0).await,
            Err(e) => Err(e),
        };
        let (v, rules) = match sent {
            Ok(pair) => pair,
            // A later sample that expired in the bridge queue, or ran into the budget after the
            // wait had run out, is this wait's timeout (with `last`), not a separate error.
            Err(e) if samples > 0 && e.name == "QueueTimeout" => {
                bridge_error = Some(e);
                break;
            }
            Err(e)
                if samples > 0
                    && e.name == "ToolBudgetExceeded"
                    && started.elapsed() >= timeout =>
            {
                break
            }
            Err(e) => return Err(e),
        };
        let s = sample_of(&v, rules, req.predicate.allows_dead())?;
        samples += 1;
        let b = base.get_or_insert_with(|| first_baseline(req, &s)).clone();
        if samples == 1 {
            before = match &req.from {
                Some(f) => {
                    json!({"tick": f.tick, "origin": f.origin, "viewAngles": null, "activeWeapon": null})
                }
                None => view(&s),
            };
        }
        if matched(&req.predicate, &b, &s, prev.as_ref())? {
            return Ok(WaitOutcome {
                end: WaitEnd::Matched { base: b, sample: s },
                samples,
                waited: started.elapsed(),
                timeout,
                clamped: requested > cap,
                before,
                rested: false,
            });
        }
        if let Some(from) = rest_from {
            let moved = s.origin.is_some_and(|o| distance(o, from) > REST_MOVED_MIN);
            if moved && at_rest(prev.as_ref(), &s) && off_target(&req.predicate, &s) {
                return Ok(WaitOutcome {
                    end: WaitEnd::Expired {
                        last: Some(s),
                        bridge_error: None,
                    },
                    samples,
                    waited: started.elapsed(),
                    timeout,
                    clamped: requested > cap,
                    before,
                    rested: true,
                });
            }
        }
        // Pace real samples: sleep only what the request did not already use of pollMs.
        let pause = poll
            .saturating_sub(t0.elapsed())
            .min(timeout.saturating_sub(started.elapsed()));
        last = Some(s.clone());
        prev = Some(s);
        if !pause.is_zero() {
            tokio::time::sleep(pause).await;
        }
    }
    Ok(WaitOutcome {
        end: WaitEnd::Expired { last, bridge_error },
        samples,
        waited: started.elapsed(),
        timeout,
        clamped: requested > cap,
        before,
        rested: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(origin: [f64; 3], velocity: [f64; 3], view_angles: [f64; 3]) -> Sample {
        Sample {
            tick: 1,
            origin: Some(origin),
            velocity: Some(velocity),
            flags: None,
            view_angles: Some(view_angles),
            weapon: Value::Null,
            alive: true,
            rules: None,
        }
    }

    /// A sample with a weapon (clip1) and `game.rules` values for the round predicates.
    fn round_sample(tick: u64, weapon_clip: Option<i64>, rules: Value, alive: bool) -> Sample {
        Sample {
            tick,
            origin: Some([0.0; 3]),
            velocity: Some([0.0; 3]),
            flags: None,
            view_angles: Some([0.0; 3]),
            weapon: match weapon_clip {
                Some(c) => json!({"name": "weapon_hkp2000", "clip1": c}),
                None => Value::Null,
            },
            alive,
            rules: Some(rules),
        }
    }

    /// `game.rules` as the bridge sends it when the round is live.
    fn live_rules(start: f64, played: i64) -> Value {
        json!({"warmup": false, "freeze": false, "teamIntro": false, "gameRestart": false,
               "roundWinStatus": 0, "roundStartTime": start, "roundsPlayed": played, "schemaMissing": []})
    }

    fn at_spec_of(v: Value) -> AtSpec {
        match parse_request(&v).unwrap().predicate {
            Predicate::At(t) => t,
            other => panic!("not at: {other:?}"),
        }
    }

    #[test]
    fn angle_delta_wraps_across_plus_minus_180() {
        assert_eq!(angle_delta(179.0, -179.0), -2.0);
        assert_eq!(angle_delta(-179.0, 179.0), 2.0);
        assert_eq!(angle_delta(10.0, 350.0), 20.0);
        assert_eq!(angle_delta(350.0, 10.0), -20.0);
        assert_eq!(angle_delta(45.0, 45.0), 0.0);
        // 180 apart is the far edge of the range: wrapped into [-180, 180)
        assert_eq!(angle_delta(0.0, 180.0), -180.0);
    }

    #[test]
    fn split_distance_separates_xy_and_z() {
        let (xy, dz) = split_distance([3.0, 4.0, 10.0], [0.0, 0.0, 4.0]);
        assert_eq!((xy, dz), (5.0, 6.0));
        // z does not leak into the horizontal distance
        let (xy, dz) = split_distance([1.0, 2.0, 3.0], [1.0, 2.0, -3.0]);
        assert_eq!((xy, dz), (0.0, 6.0));
    }

    #[test]
    fn distance_and_speed() {
        assert_eq!(distance([1.0, 2.0, 3.0], [1.0, 2.0, 3.0]), 0.0);
        assert_eq!(distance([0.0; 3], [3.0, 4.0, 0.0]), 5.0);
        assert_eq!(speed([0.0, 3.0, 4.0]), 5.0);
    }

    #[test]
    fn at_defaults_follow_the_design() {
        let t = at_spec_of(json!({"until": "at", "origin": [1, 2, 3]}));
        assert_eq!(t.origin, [1.0, 2.0, 3.0]);
        assert_eq!(t.tolerance, 2.0);
        assert_eq!(t.z_tolerance, 4.0);
        assert_eq!(t.angles, None);
        assert_eq!(t.angle_tolerance, 1.0);
        assert!(t.require_stopped);
    }

    #[test]
    fn at_matches_applies_xy_z_and_angle_tolerances() {
        let t = AtSpec {
            origin: [0.0, 0.0, 0.0],
            tolerance: 2.0,
            z_tolerance: 4.0,
            angles: Some([0.0, 179.0, 0.0]),
            angle_tolerance: 3.0,
            require_stopped: false,
        };
        let ok = sample([1.0, 1.0, 3.0], [0.0; 3], [0.0, -179.0, 0.0]);
        assert!(at_matches(&t, &ok, None).unwrap());
        // xy 2.5 > 2
        let far_xy = sample([2.5, 0.0, 0.0], [0.0; 3], [0.0, 179.0, 0.0]);
        assert!(!at_matches(&t, &far_xy, None).unwrap());
        // dz 5 > 4
        let far_z = sample([0.0, 0.0, 5.0], [0.0; 3], [0.0, 179.0, 0.0]);
        assert!(!at_matches(&t, &far_z, None).unwrap());
        // yaw 90 off
        let off_yaw = sample([0.0; 3], [0.0; 3], [0.0, 89.0, 0.0]);
        assert!(!at_matches(&t, &off_yaw, None).unwrap());
    }

    #[test]
    fn at_require_stopped_needs_this_and_previous_sample_still() {
        let t = AtSpec {
            origin: [0.0; 3],
            tolerance: 2.0,
            z_tolerance: 4.0,
            angles: None,
            angle_tolerance: 1.0,
            require_stopped: true,
        };
        let mut still = sample([0.0; 3], [0.0; 3], [0.0; 3]);
        still.tick = 2;
        let mut moving = sample([0.0; 3], [250.0, 0.0, 0.0], [0.0; 3]);
        moving.tick = 1;
        let mut earlier = still.clone();
        earlier.tick = 1;
        assert!(!at_matches(&t, &still, None).unwrap());
        assert!(!at_matches(&t, &still, Some(&moving)).unwrap());
        assert!(at_matches(&t, &still, Some(&earlier)).unwrap());
        // the previous sample off target does not count, even when still
        let mut off = earlier.clone();
        off.origin = Some([9.0, 0.0, 0.0]);
        assert!(!at_matches(&t, &still, Some(&off)).unwrap());
        // a frozen tick proves nothing: same tick as the previous sample
        let mut frozen = still.clone();
        frozen.tick = 2;
        assert!(!at_matches(&t, &still, Some(&frozen)).unwrap());
    }

    #[test]
    fn at_parse_rejects_bad_keys_before_any_request() {
        for bad in [
            json!({"until": "at"}),
            json!({"until": "at", "origin": [0, 0]}),
            json!({"until": "at", "origin": [0, 0, null]}),
            json!({"until": "at", "origin": [0, 0, 0], "tolerance": -1}),
            json!({"until": "at", "origin": [0, 0, 0], "zTolerance": "x"}),
            json!({"until": "at", "origin": [0, 0, 0], "angles": [0, 0]}),
            json!({"until": "at", "origin": [0, 0, 0], "angleTolerance": 1}),
            json!({"until": "at", "origin": [0, 0, 0], "angles": [0, 0, 0], "angleTolerance": -1}),
            json!({"until": "at", "origin": [0, 0, 0], "requireStopped": "yes"}),
            json!({"until": "at", "origin": [0, 0, 0], "from": {"tick": 1, "origin": [0, 0, 0]}}),
            json!({"until": "tick", "origin": [0, 0, 0]}),
        ] {
            let err = parse_request(&bad).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}");
        }
    }

    #[test]
    fn round_and_ammo_parse_and_reject_bad_keys() {
        let ok = parse_request(&json!({"until": "roundLive"})).unwrap();
        assert_eq!(ok.predicate, Predicate::RoundLive);
        let ok = parse_request(
            &json!({"until": "roundRestarted", "roundStartTime": 12.5, "roundsPlayed": 3}),
        )
        .unwrap();
        assert_eq!(ok.predicate, Predicate::RoundRestarted);
        assert_eq!(
            ok.round,
            RoundBase {
                start_time: Some(12.5),
                rounds_played: Some(3)
            }
        );
        let ok = parse_request(&json!({"until": "ammo", "clip1Delta": -1})).unwrap();
        assert_eq!(
            ok.predicate,
            Predicate::Ammo {
                clip1_delta: -1,
                weapon: None
            }
        );
        for bad in [
            json!({"until": "ammo", "clip1": 13}),
            json!({"until": "ammo", "clip1Delta": 0}),
            json!({"until": "ammo", "clip1Delta": 31}),
            json!({"until": "ammo", "clip1Delta": "1"}),
            json!({"until": "ammo", "clip1Delta": 1, "from": {"tick": 1, "origin": [0, 0, 0]}}),
            json!({"until": "roundLive", "clip1Delta": 1}),
            json!({"until": "roundLive", "roundsPlayed": 1}),
            json!({"until": "ammo", "clip1Delta": 1, "roundStartTime": 1}),
            json!({"until": "roundRestarted", "roundStartTime": -1}),
            json!({"until": "roundRestarted", "roundsPlayed": -1}),
            json!({"until": "roundRestarted", "roundsPlayed": 1.5}),
        ] {
            let err = parse_request(&bad).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}");
        }
    }

    #[test]
    fn round_predicates_read_rules_and_wait_through_death() {
        assert!(Predicate::RoundLive.reads_rules() && Predicate::RoundLive.allows_dead());
        assert!(Predicate::RoundRestarted.reads_rules());
        assert!(!Predicate::Ammo {
            clip1_delta: 1,
            weapon: None
        }
        .reads_rules());
        assert!(!Predicate::Ammo {
            clip1_delta: 1,
            weapon: None
        }
        .allows_dead());
        assert!(!Predicate::Stopped.allows_dead());
    }

    #[test]
    fn round_live_needs_idle_rules_and_a_living_player() {
        let live = round_sample(5, Some(13), live_rules(12.5, 0), true);
        assert!(matched(
            &Predicate::RoundLive,
            &Baseline {
                tick: 1,
                origin: None,
                clip1: None,
                weapon: Value::Null,
                round: RoundBase::default()
            },
            &live,
            None
        )
        .unwrap());
        let base = Baseline {
            tick: 1,
            origin: None,
            clip1: None,
            weapon: Value::Null,
            round: RoundBase::default(),
        };
        for (rules, alive) in [
            (
                json!({"warmup": true, "freeze": false, "teamIntro": false, "gameRestart": false, "roundWinStatus": 0}),
                true,
            ),
            (
                json!({"warmup": false, "freeze": true, "teamIntro": false, "gameRestart": false, "roundWinStatus": 0}),
                true,
            ),
            (
                json!({"warmup": false, "freeze": false, "teamIntro": true, "gameRestart": false, "roundWinStatus": 0}),
                true,
            ),
            (
                json!({"warmup": false, "freeze": false, "teamIntro": false, "gameRestart": true, "roundWinStatus": 0}),
                true,
            ),
            (
                json!({"warmup": false, "freeze": false, "teamIntro": false, "gameRestart": false, "roundWinStatus": 2}),
                true,
            ),
            (
                json!({"warmup": false, "freeze": false, "teamIntro": false, "gameRestart": false, "roundWinStatus": 0}),
                false,
            ),
        ] {
            let s = round_sample(5, Some(13), rules, alive);
            assert!(
                !matched(&Predicate::RoundLive, &base, &s, None).unwrap(),
                "{s:?}"
            );
        }
        // a null flag is a failed schema read: retry-safe SchemaUnavailable, not a silent false
        let null_flag = round_sample(
            5,
            Some(13),
            json!({"warmup": null, "freeze": false, "teamIntro": false, "gameRestart": false, "roundWinStatus": 0}),
            true,
        );
        let err = matched(&Predicate::RoundLive, &base, &null_flag, None).unwrap_err();
        assert_eq!(err.name, "SchemaUnavailable");
        let no_rules = Sample {
            rules: None,
            ..round_sample(5, Some(13), live_rules(1.0, 0), true)
        };
        assert_eq!(
            matched(&Predicate::RoundLive, &base, &no_rules, None)
                .unwrap_err()
                .name,
            "SchemaUnavailable"
        );
    }

    #[test]
    fn round_restarted_needs_a_newer_start_or_count_and_a_live_round() {
        let base = Baseline {
            tick: 1,
            origin: None,
            clip1: None,
            weapon: Value::Null,
            round: RoundBase {
                start_time: Some(12.5),
                rounds_played: Some(0),
            },
        };
        let same = round_sample(5, Some(13), live_rules(12.5, 0), true);
        assert!(!matched(&Predicate::RoundRestarted, &base, &same, None).unwrap());
        let restarted = round_sample(9, Some(13), live_rules(14.0, 0), true);
        assert!(matched(&Predicate::RoundRestarted, &base, &restarted, None).unwrap());
        let by_count = round_sample(9, Some(13), live_rules(12.5, 1), true);
        assert!(matched(&Predicate::RoundRestarted, &base, &by_count, None).unwrap());
        // restarted but not yet live (still in the restart window): keep waiting
        let not_live = round_sample(
            9,
            Some(13),
            json!({"warmup": false, "freeze": false, "teamIntro": false, "gameRestart": true, "roundWinStatus": 0, "roundStartTime": 14.0, "roundsPlayed": 0}),
            true,
        );
        assert!(!matched(&Predicate::RoundRestarted, &base, &not_live, None).unwrap());
        // no baseline values at all (a null roundStartTime in the first sample): the restart-window flag
        // going false -> true between samples is the signal; without a previous sample it cannot fire
        let no_base = Baseline {
            round: RoundBase::default(),
            ..base.clone()
        };
        assert!(!matched(&Predicate::RoundRestarted, &no_base, &restarted, None).unwrap());
        let window = Sample {
            rules: Some(
                json!({"warmup": false, "freeze": false, "teamIntro": false, "gameRestart": true, "roundWinStatus": 0}),
            ),
            ..round_sample(9, Some(13), live_rules(14.0, 0), true)
        };
        let ended = round_sample(10, Some(13), live_rules(14.0, 0), true);
        assert!(matched(&Predicate::RoundRestarted, &no_base, &ended, Some(&window)).unwrap());
        assert!(!matched(&Predicate::RoundRestarted, &no_base, &ended, Some(&ended)).unwrap());
    }

    #[test]
    fn first_round_prefers_the_callers_baseline() {
        let rules = json!({"roundStartTime": 3.0, "roundsPlayed": 2});
        assert_eq!(
            first_round(&RoundBase::default(), Some(&rules)),
            RoundBase {
                start_time: Some(3.0),
                rounds_played: Some(2)
            }
        );
        let given = RoundBase {
            start_time: Some(9.0),
            rounds_played: None,
        };
        assert_eq!(
            first_round(&given, Some(&rules)),
            RoundBase {
                start_time: Some(9.0),
                rounds_played: Some(2)
            }
        );
        assert_eq!(
            first_round(&RoundBase::default(), None),
            RoundBase::default()
        );
    }

    #[test]
    fn ammo_moves_by_the_requested_delta_from_the_baseline() {
        let base = Baseline {
            tick: 1,
            origin: None,
            clip1: Some(13),
            weapon: json!({"name": "weapon_hkp2000"}),
            round: RoundBase::default(),
        };
        let shot = round_sample(4, Some(12), live_rules(1.0, 0), true);
        assert!(matched(
            &Predicate::Ammo {
                clip1_delta: -1,
                weapon: None
            },
            &base,
            &shot,
            None
        )
        .unwrap());
        let same = round_sample(4, Some(13), live_rules(1.0, 0), true);
        assert!(!matched(
            &Predicate::Ammo {
                clip1_delta: -1,
                weapon: None
            },
            &base,
            &same,
            None
        )
        .unwrap());
        let reloaded = round_sample(4, Some(20), live_rules(1.0, 0), true);
        assert!(matched(
            &Predicate::Ammo {
                clip1_delta: 1,
                weapon: None
            },
            &base,
            &reloaded,
            None
        )
        .unwrap());
        assert!(!matched(
            &Predicate::Ammo {
                clip1_delta: 1,
                weapon: None
            },
            &base,
            &same,
            None
        )
        .unwrap());
        // no weapon now: keep waiting (a drop or a switch in progress is not a shot)
        let no_weapon = round_sample(4, None, live_rules(1.0, 0), true);
        assert!(!matched(
            &Predicate::Ammo {
                clip1_delta: -1,
                weapon: None
            },
            &base,
            &no_weapon,
            None
        )
        .unwrap());
        // a null clip1 on the same weapon is a failed schema read
        let mut null_clip = round_sample(4, Some(12), live_rules(1.0, 0), true);
        null_clip.weapon = json!({"name": "weapon_hkp2000"});
        assert_eq!(
            matched(
                &Predicate::Ammo {
                    clip1_delta: -1,
                    weapon: None
                },
                &base,
                &null_clip,
                None
            )
            .unwrap_err()
            .name,
            "SchemaUnavailable"
        );
    }

    /// A sample whose active weapon is `name` with entindex `ent` and `clip` rounds.
    fn weapon_sample(tick: u64, name: &str, ent: i64, clip: i64) -> Sample {
        let mut s = round_sample(tick, Some(clip), live_rules(1.0, 0), true);
        s.weapon = json!({"name": name, "entindex": ent, "defIndex": 1, "clip1": clip});
        s
    }

    #[test]
    fn ammo_waits_out_a_weapon_switch_and_counts_only_the_baseline_weapon() {
        let base = Baseline {
            tick: 1,
            origin: None,
            clip1: Some(13),
            weapon: json!({"name": "weapon_hkp2000", "entindex": 300, "defIndex": 1}),
            round: RoundBase::default(),
        };
        let ammo = Predicate::Ammo {
            clip1_delta: -1,
            weapon: None,
        };
        // switched to a glock that lost a round: not our shot
        let other = weapon_sample(2, "weapon_glock", 301, 10);
        assert!(!matched(&ammo, &base, &other, None).unwrap());
        // back on the pistol, down one
        let shot = weapon_sample(3, "weapon_hkp2000", 300, 12);
        assert!(matched(&ammo, &base, &shot, None).unwrap());
        // the weapon filter names a weapon: a pistol shot does not count for weapon_glock
        let glock_only = Predicate::Ammo {
            clip1_delta: -1,
            weapon: Some("weapon_glock".into()),
        };
        assert!(!matched(&glock_only, &base, &shot, None).unwrap());
        // a new entindex with the same name is a different weapon (dropped and picked up again)
        let picked_up = weapon_sample(4, "weapon_hkp2000", 412, 12);
        assert!(!matched(&ammo, &base, &picked_up, None).unwrap());
    }

    #[test]
    fn ammo_baseline_without_a_weapon_is_prerequisite_failed() {
        let base = Baseline {
            tick: 1,
            origin: None,
            clip1: None,
            weapon: Value::Null,
            round: RoundBase::default(),
        };
        let s = weapon_sample(2, "weapon_hkp2000", 300, 12);
        let err = matched(
            &Predicate::Ammo {
                clip1_delta: -1,
                weapon: None,
            },
            &base,
            &s,
            None,
        )
        .unwrap_err();
        assert_eq!(err.name, "PrerequisiteFailed");
        assert!(err.retry_safe);
    }

    #[test]
    fn ammo_caller_baseline_matches_a_shot_that_landed_before_the_first_sample() {
        // the caller read 13 on entindex 300 before firing; the first sample already shows 12
        let base = first_baseline(
            &WaitRequest {
                predicate: Predicate::Ammo {
                    clip1_delta: -1,
                    weapon: None,
                },
                from: None,
                round: RoundBase::default(),
                ammo_from: Some(AmmoFrom {
                    clip1: 13,
                    entindex: 300,
                }),
                timeout_ms: 1,
                poll_ms: 100,
            },
            &weapon_sample(1, "weapon_hkp2000", 300, 12),
        );
        assert_eq!(base.clip1, Some(13));
        assert!(matched(
            &Predicate::Ammo {
                clip1_delta: -1,
                weapon: None
            },
            &base,
            &weapon_sample(1, "weapon_hkp2000", 300, 12),
            None
        )
        .unwrap());
    }

    #[test]
    fn ammo_params_default_to_one_shot_and_pair_clip1_with_entindex() {
        let (d, w, f) = ammo_params(json!({"until": "ammo"}).as_object().unwrap()).unwrap();
        assert_eq!((d, w, f), (-1, None, None));
        let (_, _, f) =
            ammo_params(json!({"clip1": 13, "entindex": 300}).as_object().unwrap()).unwrap();
        assert_eq!(
            f,
            Some(AmmoFrom {
                clip1: 13,
                entindex: 300
            })
        );
        for bad in [
            json!({"clip1": 13}),
            json!({"entindex": 300}),
            json!({"clip1": 2000, "entindex": 300}),
            json!({"weapon": "ak47"}),
        ] {
            assert!(ammo_params(bad.as_object().unwrap()).is_err(), "{bad}");
        }
    }

    #[test]
    fn dead_player_is_a_sample_only_for_round_predicates() {
        let v = json!({"inGame": true, "tick": 3, "map": "m", "viewAngles": [0, 0, 0],
            "player": {"alive": false, "origin": [0, 0, 0], "velocity": [0, 0, 0], "activeWeapon": null}});
        assert!(!sample_of(&v, None, true).unwrap().alive);
        assert_eq!(
            sample_of(&v, None, false).unwrap_err().name,
            "PrerequisiteFailed"
        );
    }
}
