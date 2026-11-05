//! `round_start`: put the round in a known state for a test, idempotently. Mutating.
//!
//! `ensure` (default) decides from one game.rules read and one game.local read:
//! - the round is live: nothing is sent, unless cvars or shortcuts were given (then they are sent
//!   and nothing waits: the round stays live);
//! - the player cannot play the round (dead, not on a team, or a round winner is set): this falls
//!   back to the restart path, with `fallback` and `fallbackReason` in the result;
//! - warmup: the settings and `mp_warmup_end`, then a wait for roundRestarted (the warmup end starts
//!   the round, so the start time moves);
//! - otherwise (freeze, team intro, restart window): the settings, then a wait for roundLive.
//!
//! `restart` queues the settings and `mp_restartgame 1` and waits for roundRestarted against the
//! game.rules read before anything was sent, so a restart that finished before the first wait
//! sample still counts.
//!
//! Every mode sends `input.clear` first (a button held across a restart wedges prediction) and checks
//! the arguments before any bridge request. The commands go through console.exec with the
//! confirmation marker of `console_exec {confirm}`: `warnings` lists the engine's refusals. `timeoutMs`
//! is one deadline for the confirmation and the round wait together. Once a command is queued, every
//! error is not retry-safe (commandsSent).
use crate::console::resolve::{resolve_within, Purpose};
use crate::console::{ConsoleError, ConsoleLog};
use crate::state::ClientState;
use crate::tools::console::console_error;
use crate::tools::game_wait::{
    round_live, run_wait, sample_of, Predicate, RoundBase, Sample, WaitEnd, WaitRequest,
};
use crate::tools::session::{check_console, exec_console_confirmed};
use cs2_mcp_common::bridge::{BridgeClient, RequestOpts};
use cs2_mcp_common::tools::{
    require_caps, CallCtx, Mutability, Registry, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Map, Value};
use std::time::{Duration, Instant};

const KEYS: [&str; 6] = [
    "mode",
    "cvars",
    "respawn",
    "freezetime",
    "roundtime",
    "timeoutMs",
];
const TIMEOUT_DEFAULT_MS: u64 = 8000;
/// Pace of the wait's game.rules + game.local samples (the game_wait default).
const POLL_MS: u64 = 100;
/// Budget needed to queue the commands and still have a wait sample (the setpos floor).
const SEND_FLOOR: Duration = Duration::from_millis(750);
/// Most of the timeout the command confirmation may take; the round wait gets the rest.
const CONFIRM_CAP: Duration = Duration::from_millis(2000);
const MAX_CVARS: usize = 16;
const CVAR_NAME_MAX: usize = 64;
const VALUE_MAX: usize = 64;
const FREEZETIME_MAX: u64 = 60;
const ROUNDTIME_MAX_MIN: u64 = 60;
/// Cvars that change the map, run other commands, end the round or the game: refused as cvars.
const REFUSED_CVARS: [&str; 12] = [
    "map",
    "changelevel",
    "changelevel2",
    "exec",
    "alias",
    "bind",
    "unbind",
    "unbindall",
    "mp_restartgame",
    "quit",
    "disconnect",
    "exit",
];
const REFUSED_PREFIXES: [&str; 4] = ["mp_warmup_", "host_", "map_", "changelevel"];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Ensure,
    Restart,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Ensure => "ensure",
            Mode::Restart => "restart",
        }
    }
}

/// Validated arguments. `settings` are the cvar and shortcut commands, in order.
#[derive(Debug, PartialEq)]
struct RoundRequest {
    mode: Mode,
    settings: Vec<String>,
    timeout_ms: u64,
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(message)
}

/// Optional integer in `min..=max` (null counts as absent).
fn uint_in(
    obj: &Map<String, Value>,
    key: &str,
    min: u64,
    max: u64,
) -> Result<Option<u64>, ToolError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|n| (min..=max).contains(n))
            .map(Some)
            .ok_or_else(|| invalid(format!("{key} must be an integer {min}..={max}"))),
    }
}

/// A cvar name: lowercase letters, digits and underscores (no spaces, no command separators).
fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= CVAR_NAME_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A cvar that round_start must not set (map changes, command aliases, round and game control).
fn refused_cvar(name: &str) -> bool {
    REFUSED_CVARS.contains(&name) || REFUSED_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// A simple value token: letters, digits, `_`, `.` and `-`. Numbers are rendered first.
fn token_ok(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= VALUE_MAX
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// The console text of one cvar value: a number (integers as written, floats as Rust prints them)
/// or a simple token. Anything else, booleans and nulls included, is refused.
fn cvar_value(name: &str, v: &Value) -> Result<String, ToolError> {
    let text = match v {
        Value::Number(n) => n.as_i64().map_or_else(
            || n.as_f64().map_or_else(String::new, |f| f.to_string()),
            |i| i.to_string(),
        ),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    if token_ok(&text) {
        Ok(text)
    } else {
        Err(invalid(format!(
            "cvars.{name} must be a number or a simple token ([A-Za-z0-9_.-], at most {VALUE_MAX} bytes)"
        )))
    }
}

/// Validate `round_start` arguments. Nothing is sent.
fn parse_round(args: &Value) -> Result<RoundRequest, ToolError> {
    let empty = Map::new();
    let obj = match args {
        Value::Object(m) => m,
        Value::Null => &empty,
        _ => return Err(invalid("arguments must be an object")),
    };
    if let Some(k) = obj.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(invalid(format!(
            "unknown key {k:?} in round_start (allowed: {})",
            KEYS.join(", ")
        )));
    }
    let mode = match obj.get("mode") {
        None | Some(Value::Null) => Mode::Ensure,
        Some(Value::String(s)) if s == "ensure" => Mode::Ensure,
        Some(Value::String(s)) if s == "restart" => Mode::Restart,
        Some(_) => return Err(invalid("mode must be \"ensure\" or \"restart\"")),
    };
    let mut settings = Vec::new();
    if let Some(f) = uint_in(obj, "freezetime", 0, FREEZETIME_MAX)? {
        settings.push(format!("mp_freezetime {f}"));
    }
    if let Some(r) = uint_in(obj, "roundtime", 1, ROUNDTIME_MAX_MIN)? {
        settings.push(format!("mp_roundtime {r}"));
    }
    match obj.get("respawn") {
        None | Some(Value::Null) => {}
        Some(v) => {
            let on = v
                .as_bool()
                .ok_or_else(|| invalid("respawn must be true or false"))?;
            let flag = u8::from(on);
            settings.push(format!("mp_respawn_on_death_ct {flag}"));
            settings.push(format!("mp_respawn_on_death_t {flag}"));
        }
    }
    match obj.get("cvars") {
        None | Some(Value::Null) => {}
        Some(v) => {
            let cvars = v
                .as_object()
                .ok_or_else(|| invalid("cvars must be an object of cvar name to value"))?;
            if cvars.len() > MAX_CVARS {
                return Err(invalid(format!("cvars holds at most {MAX_CVARS} entries")));
            }
            for (name, value) in cvars {
                if !name_ok(name) {
                    return Err(invalid(format!(
                        "cvar name {name:?} must be [a-z0-9_], at most {CVAR_NAME_MAX} bytes"
                    )));
                }
                if refused_cvar(name) {
                    return Err(invalid(format!(
                        "cvar {name:?} cannot be set through round_start (it changes the map, runs commands, or ends the game); use the dedicated tools"
                    )));
                }
                let command = format!("{name} {}", cvar_value(name, value)?);
                // the console guard: same refusals as console_exec, reported as bad arguments
                check_console(&command)
                    .map_err(|e| invalid(format!("cvar {name:?} refused: {}", e.message)))?;
                settings.push(command);
            }
        }
    }
    let timeout_ms = uint_in(obj, "timeoutMs", 1, u64::MAX)?.unwrap_or(TIMEOUT_DEFAULT_MS);
    Ok(RoundRequest {
        mode,
        settings,
        timeout_ms,
    })
}

async fn read(bridge: &mut BridgeClient, ctx: &CallCtx, method: &str) -> Result<Value, ToolError> {
    let ms = ctx.budget.deadline_ms()?;
    Ok(bridge
        .request(method, json!({}), RequestOpts::deadline(ms))
        .await?
        .result)
}

/// Refuse before anything is sent when the local player is not in game (a dead player is fine).
fn in_game(local: &Value) -> Result<(), ToolError> {
    let in_game = local
        .get("inGame")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let has_player = local.get("player").is_some_and(Value::is_object);
    if in_game && has_player {
        return Ok(());
    }
    Err(ToolError::new(
        "PrerequisiteFailed",
        "PrerequisiteFailed",
        format!(
            "round_start needs the local player in game (inGame {in_game}): start or join a map first; nothing was sent"
        ),
    )
    .retry_safe(true)
    .with_data(json!({
        "inGame": in_game,
        "map": local.get("map").cloned().unwrap_or(Value::Null),
        "commandsSent": false,
    })))
}

/// The player as the caller sees it after the round: alive, origin, health and the weapons.
/// `weapons` comes from game.weapons when the bridge has it. Reads only; a failed read is noted, not
/// fatal, because the round itself already happened.
async fn player_view(bridge: &mut BridgeClient, ctx: &CallCtx, local: &Value) -> Value {
    let p = local.get("player").cloned().unwrap_or(Value::Null);
    let weapons = if bridge.has("game.weapons") {
        match read(bridge, ctx, "game.weapons").await {
            Ok(w) => json!({
                "count": w.get("count").cloned().unwrap_or(Value::Null),
                "active": w["active"]["weapon"]["name"].clone(),
                "names": w["weapons"]
                    .as_array()
                    .map(|a| a.iter().map(|x| x["name"].clone()).collect::<Vec<_>>()),
            }),
            Err(e) => json!({"note": e.message}),
        }
    } else {
        json!({"note": "bridge does not advertise game.weapons"})
    };
    json!({
        "alive": p.get("alive").cloned().unwrap_or(Value::Null),
        "origin": p.get("origin").cloned().unwrap_or(Value::Null),
        "health": p.get("health").cloned().unwrap_or(Value::Null),
        "weapons": weapons,
    })
}

/// `player_view` of a fresh game.local read, and the rules as read now (both non-fatal).
async fn after_state(bridge: &mut BridgeClient, ctx: &CallCtx) -> (Value, Value, Option<i64>) {
    let rules = match read(bridge, ctx, "game.rules").await {
        Ok(r) => r,
        Err(e) => json!({"note": e.message}),
    };
    match read(bridge, ctx, "game.local").await {
        Ok(local) => {
            let tick = local.get("tick").and_then(Value::as_i64);
            (player_view(bridge, ctx, &local).await, rules, tick)
        }
        Err(e) => (json!({"note": e.message}), rules, None),
    }
}

/// Why the player cannot play the round when a restart is the only fix, if they cannot.
fn blocked_reason(pre: &Sample, team: Option<i64>, rules: &Value) -> Option<&'static str> {
    if !pre.alive {
        return Some("player dead");
    }
    if team.is_some_and(|t| t <= 1) {
        return Some("player is not on a team (spectator)");
    }
    if rules
        .get("roundWinStatus")
        .and_then(Value::as_i64)
        .is_some_and(|w| w != 0)
    {
        return Some("round has a winner (roundWinStatus set)");
    }
    None
}

/// What round_start will do, decided from the pre-read.
struct Plan {
    /// the commands to queue, in order
    commands: Vec<String>,
    /// the round wait, or None when nothing needs waiting
    wait: Option<(Predicate, RoundBase)>,
    /// set when ensure falls back to the restart path, with the reason
    fallback: Option<&'static str>,
}

/// The round baseline from a game.rules reading: roundStartTime and roundsPlayed (either may be absent).
fn baseline(rules: &Value) -> RoundBase {
    RoundBase {
        start_time: rules.get("roundStartTime").and_then(Value::as_f64),
        rounds_played: rules.get("roundsPlayed").and_then(Value::as_i64),
    }
}

fn restart_plan(
    settings: &[String],
    rules: &Value,
    fallback: Option<&'static str>,
) -> Result<Plan, ToolError> {
    let round = baseline(rules);
    if round.start_time.is_none() && round.rounds_played.is_none() {
        return Err(ToolError::new(
            "PrerequisiteFailed",
            "PrerequisiteFailed",
            "game.rules has neither roundStartTime nor roundsPlayed to measure a restart against; no command was sent",
        )
        .retry_safe(true));
    }
    let mut commands = settings.to_vec();
    commands.push("mp_restartgame 1".to_string());
    Ok(Plan {
        commands,
        wait: Some((Predicate::RoundRestarted, round)),
        fallback,
    })
}

/// The decision for `mode`, from the pre-read rules, the pre-read sample and the raw team.
fn plan(
    req: &RoundRequest,
    rules: &Value,
    pre: &Sample,
    team: Option<i64>,
) -> Result<Plan, ToolError> {
    match req.mode {
        Mode::Restart => restart_plan(&req.settings, rules, None),
        Mode::Ensure => {
            // a spectator is not playing the round, whatever the rules say
            let spectator = team.is_some_and(|t| t <= 1);
            if !spectator && round_live(pre, "roundLive")? {
                return Ok(Plan {
                    commands: req.settings.clone(),
                    wait: None,
                    fallback: None,
                });
            }
            if let Some(reason) = blocked_reason(pre, team, rules) {
                return restart_plan(&req.settings, rules, Some(reason));
            }
            if rules.get("warmup").and_then(Value::as_bool) == Some(true) {
                let mut commands = req.settings.clone();
                commands.push("mp_warmup_end".to_string());
                return Ok(Plan {
                    commands,
                    wait: Some((Predicate::RoundRestarted, baseline(rules))),
                    fallback: None,
                });
            }
            Ok(Plan {
                commands: req.settings.clone(),
                wait: Some((Predicate::RoundLive, RoundBase::default())),
                fallback: None,
            })
        }
    }
}

/// An error once something was queued: the round may be moving, so a retry is not safe.
fn after_send(e: ToolError, commands: &[String]) -> ToolError {
    e.retry_safe(false).with_data(json!({
        "commands": commands,
        "commandsSent": true,
    }))
}

/// One deadline for the whole call: `timeoutMs` from the start, never below 1 ms.
#[derive(Clone, Copy)]
struct Timing {
    started: Instant,
    deadline: Duration,
}

impl Timing {
    fn left(&self) -> Duration {
        self.deadline
            .saturating_sub(self.started.elapsed())
            .max(Duration::from_millis(1))
    }
}

pub async fn round_start(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let req = parse_round(&args)?;
    require_caps(&state.bridge, &["game.rules", "game.local"]).await?;
    let timing = Timing {
        started: Instant::now(),
        deadline: Duration::from_millis(req.timeout_ms),
    };
    let mut bridge = state.bridge.lock().await;
    let pre_local = read(&mut bridge, &ctx, "game.local").await?;
    in_game(&pre_local)?;
    let cleared = if bridge.has("input.clear") {
        let ms = ctx.budget.deadline_ms()?;
        bridge
            .request("input.clear", json!({}), RequestOpts::deadline(ms))
            .await?;
        true
    } else {
        false
    };
    match body(&state, &ctx, &mut bridge, &req, timing, &pre_local).await {
        Ok(v) => Ok(v.into()),
        Err(mut e) => {
            // the input release already went out: say so, and say whether a command did
            e.data.insert("inputCleared".into(), json!(cleared));
            if !e.data.contains_key("commandsSent") {
                e.data.insert("commandsSent".into(), json!(false));
            }
            Err(e)
        }
    }
}

async fn body(
    state: &ClientState,
    ctx: &CallCtx,
    bridge: &mut BridgeClient,
    req: &RoundRequest,
    timing: Timing,
    pre_local: &Value,
) -> Result<Value, ToolError> {
    let pre_rules = read(bridge, ctx, "game.rules").await?;
    let pre = sample_of(pre_local, Some(pre_rules.clone()), true)?;
    let team = pre_local
        .get("player")
        .and_then(|p| p.get("team"))
        .and_then(Value::as_i64);
    let plan = plan(req, &pre_rules, &pre, team)?;
    if plan.commands.is_empty() && plan.wait.is_none() {
        // already live, nothing asked for: nothing sent, nothing waited for
        let player = player_view(bridge, ctx, pre_local).await;
        return Ok(json!({
            "mode": req.mode.name(),
            "changed": false,
            "fallback": null,
            "fallbackReason": null,
            "commands": [],
            "warnings": [],
            "before": {"rules": pre_rules},
            "after": {"rules": pre_rules, "player": player},
            "waitedMs": 0,
            "ticks": 0,
            "samples": 0,
            "clampedToBudget": false,
        }));
    }
    let mut warnings = Value::Array(Vec::new());
    let mut clamped = false;
    let commands = plan.commands.clone();
    if !commands.is_empty() {
        if !bridge.has("console.exec") {
            return Err(ToolError::not_implemented(
                "CapabilityMissing",
                "the bridge does not advertise console.exec (no command was sent)",
                json!({"missing": ["console.exec"]}),
            ));
        }
        // the log gives the confirmation cursor: no log, no command
        let log = resolve_within(state, ctx, None, Purpose::Bridge).await?;
        let cursor = match ConsoleLog::new(&log.path).cursor() {
            Ok(Some(c)) => c,
            Ok(None) => {
                return Err(console_error(ConsoleError::Missing, &log.path).retry_safe(true))
            }
            Err(e) => return Err(console_error(ConsoleError::Io(e), &log.path)),
        };
        if ctx.budget.remaining() < SEND_FLOOR {
            return Err(ToolError::budget_exceeded(&ctx.budget)
                .retry_safe(true)
                .with_data(json!({"commandsSent": false})));
        }
        let refs: Vec<&str> = commands.iter().map(String::as_str).collect();
        let confirm = timing.left().min(CONFIRM_CAP);
        let sent = exec_console_confirmed(bridge, ctx, log.path.as_path(), &cursor, &refs, confirm)
            .await
            .map_err(|e| {
                if e.data.get("commandsSent") == Some(&Value::Bool(false)) {
                    // the first command never went out: the caller may retry
                    e.retry_safe(true)
                } else {
                    let commands = commands.clone();
                    e.with_data(json!({"commands": commands}))
                }
            })?;
        warnings = sent["warnings"].clone();
        clamped |= sent["clampedToBudget"].as_bool().unwrap_or(false);
    }
    let Some((predicate, round)) = plan.wait.clone() else {
        // settings only, on a live round: read the result back, no wait
        let (player, rules, tick) = after_state(bridge, ctx).await;
        return Ok(json!({
            "mode": req.mode.name(),
            "changed": true,
            "fallback": null,
            "fallbackReason": null,
            "commands": commands,
            "warnings": warnings,
            "before": {"rules": pre_rules},
            "after": {"rules": rules, "player": player},
            "waitedMs": timing.started.elapsed().as_millis() as u64,
            "ticks": tick.map_or(0, |t| (t - pre.tick as i64).max(0) as u64),
            "samples": 0,
            "clampedToBudget": clamped,
        }));
    };
    let wait = WaitRequest {
        predicate,
        from: None,
        round,
        ammo_from: None,
        timeout_ms: timing.left().as_millis() as u64,
        poll_ms: POLL_MS,
    };
    let out = match run_wait(bridge, ctx, &wait).await {
        Ok(out) => out,
        Err(e) if !commands.is_empty() => return Err(after_send(e, &commands)),
        Err(e) => return Err(e),
    };
    clamped |= out.clamped;
    let fallback = plan.fallback;
    match out.end {
        WaitEnd::Matched { sample, .. } => {
            let player = match read(bridge, ctx, "game.local").await {
                Ok(local) => player_view(bridge, ctx, &local).await,
                Err(e) => json!({"note": e.message}),
            };
            Ok(json!({
                "mode": req.mode.name(),
                "changed": true,
                "fallback": fallback.map(|_| "restart"),
                "fallbackReason": fallback,
                "commands": commands,
                "warnings": warnings,
                "before": {"rules": pre_rules},
                "after": {"rules": sample.rules.clone().unwrap_or(Value::Null), "player": player},
                "waitedMs": timing.started.elapsed().as_millis() as u64,
                "ticks": sample.tick.saturating_sub(pre.tick),
                "samples": out.samples,
                "clampedToBudget": clamped,
            }))
        }
        WaitEnd::Expired { last, bridge_error } => {
            let sent_any = !commands.is_empty();
            let goal = match wait.predicate {
                Predicate::RoundRestarted => "restart",
                _ => "go live",
            };
            let reason = match &bridge_error {
                None => "timeout",
                Some(_) => "bridgeQueueTimeout",
            };
            let waited = out.waited.as_millis() as u64;
            let message = if sent_any {
                format!(
                    "the round did not {goal} within {waited} ms ({} samples); {} was sent",
                    out.samples,
                    commands.join("; ")
                )
            } else {
                format!(
                    "the round did not {goal} within {waited} ms ({} samples)",
                    out.samples
                )
            };
            let mut data = json!({
                "mode": req.mode.name(),
                "fallback": fallback.map(|_| "restart"),
                "fallbackReason": fallback,
                "commands": commands,
                "warnings": warnings,
                "before": {"rules": pre_rules},
                "last": last.as_ref().and_then(|s| s.rules.clone()),
                "waitedMs": waited,
                "timeoutMs": out.timeout.as_millis() as u64,
                "requestedTimeoutMs": timing.deadline.as_millis() as u64,
                "clampedToBudget": clamped,
                "samples": out.samples,
                "reason": reason,
                "commandsSent": sent_any,
            });
            if let Some(e) = &bridge_error {
                data["bridgeError"] = json!({"name": e.name, "code": e.code, "message": e.message});
            }
            Err(ToolError::new("WaitTimeout", "WaitTimeout", message)
                .retry_safe(!sent_any)
                .with_data(data))
        }
    }
}

pub fn round_start_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "mode": {"type": "string", "enum": ["ensure", "restart"], "default": "ensure"},
            "cvars": {"type": "object", "maxProperties": MAX_CVARS, "additionalProperties": {"type": ["number", "string"]}, "description": "extra cvar -> value, sent before the warmup end or restart"},
            "respawn": {"type": "boolean", "description": "mp_respawn_on_death_ct/_t 1|0"},
            "freezetime": {"type": "integer", "minimum": 0, "maximum": FREEZETIME_MAX, "description": "seconds (mp_freezetime)"},
            "roundtime": {"type": "integer", "minimum": 1, "maximum": ROUNDTIME_MAX_MIN, "description": "minutes (mp_roundtime)"},
            "timeoutMs": {"type": "integer", "minimum": 1, "default": TIMEOUT_DEFAULT_MS}
        },
        "additionalProperties": false
    })
}

pub fn register(r: &mut Registry<ClientState>) {
    r.register(
        ToolSpec::new(
            "round_start",
            "Put the round in a known state (idempotent). mode ensure (default): no-op on a live round; a dead player, no team or a round winner restarts; in warmup it ends the warmup; else waits for roundLive. mode restart: mp_restartgame 1. Refused cvars (map changes, exec/alias/bind, mp_restartgame, mp_warmup_*, host_*, quit) are InvalidParams. Once queued, errors are not retry-safe.",
            Mutability::Mutating,
            &["console.exec", "game.rules", "game.local"],
        )
        .with_schema(round_start_schema()),
        round_start,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_shortcuts_build_the_settings_in_order() {
        let r = parse_round(&json!({})).unwrap();
        assert_eq!(r.mode, Mode::Ensure);
        assert!(r.settings.is_empty());
        assert_eq!(r.timeout_ms, TIMEOUT_DEFAULT_MS);

        let r = parse_round(&json!({
            "mode": "restart",
            "freezetime": 0,
            "roundtime": 5,
            "respawn": true,
            "cvars": {"bot_quota": 2, "sv_cheats": "1", "mp_maxmoney": 16000.5}
        }))
        .unwrap();
        assert_eq!(r.mode, Mode::Restart);
        assert_eq!(
            r.settings,
            vec![
                "mp_freezetime 0",
                "mp_roundtime 5",
                "mp_respawn_on_death_ct 1",
                "mp_respawn_on_death_t 1",
                // cvars after the shortcuts, in name order (the JSON object's order)
                "bot_quota 2",
                "mp_maxmoney 16000.5",
                "sv_cheats 1",
            ]
        );
    }

    #[test]
    fn invalid_arguments_are_refused_before_anything_is_sent() {
        for bad in [
            json!([]),
            json!({"mode": "reset"}),
            json!({"mode": 1}),
            json!({"unknown": true}),
            json!({"timeoutMs": 0}),
            json!({"freezetime": 61}),
            json!({"roundtime": 0}),
            json!({"roundtime": 61}),
            json!({"respawn": "yes"}),
            json!({"cvars": []}),
            json!({"cvars": {"Bot_Quota": 1}}),
            json!({"cvars": {"bot quota": 1}}),
            json!({"cvars": {"bot_quota; quit": 1}}),
            json!({"cvars": {"bot_quota": "1; quit"}}),
            json!({"cvars": {"bot_quota": true}}),
            json!({"cvars": {"bot_quota": null}}),
            json!({"cvars": {"bot_quota": "x".repeat(65)}}),
        ] {
            let err = parse_round(&bad).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{bad}");
        }
    }

    #[test]
    fn cvars_that_change_the_map_or_run_commands_are_refused() {
        for name in [
            "map",
            "changelevel",
            "map_workshop",
            "exec",
            "alias",
            "bind",
            "unbind",
            "mp_restartgame",
            "mp_warmup_end",
            "mp_warmup_pausetime",
            "quit",
            "disconnect",
            "host_timescale",
        ] {
            let err = parse_round(&json!({"cvars": {name: 1}})).unwrap_err();
            assert_eq!(err.name, "InvalidParams", "{name}");
        }
        // a name the console guard refuses is bad arguments too
        assert_eq!(
            parse_round(&json!({"cvars": {"exit": 1}}))
                .unwrap_err()
                .name,
            "InvalidParams"
        );
        // ordinary cvars are fine
        assert!(parse_round(&json!({"cvars": {"bot_quota": 0, "mp_maxrounds": 3}})).is_ok());
    }

    #[test]
    fn cvar_values_render_numbers_and_tokens() {
        assert_eq!(cvar_value("x", &json!(3)).unwrap(), "3");
        assert_eq!(cvar_value("x", &json!(-1)).unwrap(), "-1");
        assert_eq!(cvar_value("x", &json!(0.25)).unwrap(), "0.25");
        assert_eq!(cvar_value("x", &json!("de_dust2")).unwrap(), "de_dust2");
        assert!(cvar_value("x", &json!("a b")).is_err());
    }

    #[test]
    fn timing_never_goes_below_one_millisecond() {
        let t = Timing {
            started: Instant::now() - Duration::from_secs(10),
            deadline: Duration::from_millis(100),
        };
        assert_eq!(t.left(), Duration::from_millis(1));
    }
}
