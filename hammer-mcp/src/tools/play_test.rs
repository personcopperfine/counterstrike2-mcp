//! `play_test`: a scripted play-test of the running map in ONE call. The steps are the things an
//! agent used to drive from a throwaway python script (`drive.py`, `endtest.py`): hold keys and
//! steer, wait for a log line or a state, check the result. Everything goes through the same
//! shared code as `game_state` / `input_sequence` (`cs2_mcp_common::play`); the console.log tail is
//! `run_map`'s [`ConsoleWatch`].
//!
//! Steps (`kind`): `input` (as input_sequence, but the keys STAY held for the next step),
//! `wait_ms`, `wait_state` (poll game_state until `{path, eq|gt|...}` holds), `wait_log` (regex
//! over new console.log lines), `expect_state` (an immediate check), `wait_entity` / `expect_entity` (the same over an entity of the
//! running game found by class and/or name, via `entity_find`'s `game.entities`). The run stops at the first
//! step that fails or when the next one would overrun the call budget; input is cleared however
//! it ends (unless `keepInput` and it passed). Every step is validated before anything is sent.
use super::game_live::{no_map, no_map_error};
use super::run_map::{console_log_path, ConsoleWatch};
use crate::state::HammerState;
use cs2_mcp_common::bridge::{BridgeClient, RequestOpts, SharedBridge};
use cs2_mcp_common::play::{entities, predicate::Predicate, sequence};
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::time::{Duration, Instant};

pub const MAX_STEPS: usize = 64;
const MAX_WAIT_MS: u64 = 120_000;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const POLL_DEFAULT_MS: u64 = 100;
const POLL_MIN_MS: u64 = 20;
const POLL_MAX_MS: u64 = 1_000;
const LOG_POLL_MS: u64 = 50;
/// Kept back from the call budget for the closing input.clear and the reply.
const END_MARGIN: Duration = Duration::from_millis(1_500);
const MAX_PATTERN: usize = 512;
const MAX_LABEL: usize = 64;
const MAX_ENTITY_INDEX: u64 = 255;
/// The longest a key-pressing `input` step waits for the freeze period / team intro to end before
/// it sends, unless the step sets `lockWaitMs` (competitive: `mp_freezetime` 15 plus a team intro).
/// When the step does not set it and a lock is seen, the default grows to cover `mp_freezetime`
/// (read with `cvar.get`) plus [`FREEZE_SLACK_MS`], capped at [`MOVEMENT_LOCK_WAIT_MAX_MS`].
const MOVEMENT_LOCK_WAIT_MS: u64 = 20_000;
const MOVEMENT_LOCK_WAIT_MAX_MS: u64 = 60_000;
/// Slack added to `mp_freezetime` when the default wait is sized from it (team intro, tick latency).
const FREEZE_SLACK_MS: u64 = 5_000;
const LOCK_POLL_MS: u64 = 100;
/// Movement keys: a step that holds one of them and moves nothing gets the "did not move" note.
const MOVE_KEYS: [&str; 6] = ["forward", "back", "left", "right", "moveleft", "moveright"];

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "play_test",
            "Run steps against the local player in the running map; stops at the first failure. Step kinds: \
             input {steps:[as input_sequence]} (keys stay held); wait_ms {ms}; wait_state / expect_state {path, op} on game_state (wait polls, timeoutMs); \
             wait_log {pattern (regex), timeoutMs?, since?: step|start}; wait_entity / expect_entity {class?, name?, index?, path, op} on RUNNING-game entities. \
             op: eq|ne|gt|ge|lt|le|near(+tolerance)|contains|exists, value under that key: {kind:'wait_entity',class:'func_movelinear',path:'origin[0]',gt:1500,timeoutMs:20000}. \
             path: player.alive / player.origin[0]; entity: origin[0], health. \
             Returns passed and endedBy complete|failed|budget (budget = the 45 s call budget ran out: not a pass). \
             Keys wait out freeze (lockWaitMs); still locked then: step fails with movementLocked:true (raise lockWaitMs or lower mp_freezetime). \
             wait_log since:start for a line that can appear during an earlier step. Needs a map (run_map).",
            Mutating,
            &["game.local", "input.set", "input.clear"],
        )
        .with_schema(play_test_schema()),
        play_test,
    );
}

fn play_test_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "steps": {
                "type": "array", "minItems": 1, "maxItems": MAX_STEPS,
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "enum": ["input", "wait_ms", "wait_state", "wait_log", "expect_state", "wait_entity", "expect_entity"]},
                        "class": {"type": "string", "pattern": "^[A-Za-z0-9_]{1,64}$", "description": "wait_entity/expect_entity: designer class"},
                        "name": {"type": "string", "minLength": 1, "maxLength": 127, "description": "wait_entity/expect_entity: targetname"},
                        "index": {"type": "integer", "minimum": 0, "maximum": MAX_ENTITY_INDEX, "description": "wait_entity/expect_entity: nth match (default 0)"},
                        "label": {"type": "string", "maxLength": MAX_LABEL},
                        "steps": {"type": "array", "description": "kind input: input_sequence steps"},
                        "ms": {"type": "integer", "minimum": 1, "maximum": MAX_WAIT_MS, "description": "kind wait_ms"},
                        "path": {"type": "string", "description": "game_state field (state kinds) or entity field (entity kinds), e.g. origin[0]"},
                        "eq": {}, "ne": {},
                        "gt": {"type": "number"}, "ge": {"type": "number"}, "lt": {"type": "number"}, "le": {"type": "number"},
                        "near": {"type": "number"}, "tolerance": {"type": "number", "minimum": 0},
                        "contains": {"type": "string"}, "exists": {"type": "boolean"},
                        "pattern": {"type": "string", "maxLength": MAX_PATTERN, "description": "kind wait_log: regex"},
                        "since": {"type": "string", "enum": ["step", "start"], "description": "kind wait_log: default step"},
                        "timeoutMs": {"type": "integer", "minimum": 1, "maximum": MAX_WAIT_MS, "description": "wait kinds, default 5000; cut to the call budget"},
                        "lockWaitMs": {"type": "integer", "minimum": 0, "maximum": MOVEMENT_LOCK_WAIT_MAX_MS, "description": "input: max wait for freeze/intro to end (default 20000 or freezetime+5 s); 0 = none"},
                        "pollMs": {"type": "integer", "minimum": POLL_MIN_MS, "maximum": POLL_MAX_MS, "description": "default 100"}
                    },
                    "required": ["kind"]
                }
            },
            "keepInput": {"type": "boolean", "description": "keep keys/view override after a pass"}
        },
        "required": ["steps"],
        "additionalProperties": false
    })
}

enum Kind {
    Input { args: Value, lock_wait_ms: Option<u64> },
    WaitMs(u64),
    WaitState { pred: Predicate, timeout_ms: u64, poll_ms: u64 },
    WaitLog { re: Regex, pattern: String, timeout_ms: u64, since_start: bool },
    Expect(Predicate),
    WaitEntity { sel: EntitySel, pred: Predicate, timeout_ms: u64, poll_ms: u64 },
    ExpectEntity { sel: EntitySel, pred: Predicate },
}

/// Which game entity an entity step looks at: `entity_find` params (class and/or name) and the
/// index of the match.
struct EntitySel {
    params: Value,
    index: usize,
}

struct Step {
    label: Option<String>,
    name: &'static str,
    kind: Kind,
}

fn invalid(m: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(m)
}

fn uint(obj: &Map<String, Value>, key: &str, default: u64, min: u64, max: u64, at: &str) -> Result<u64, ToolError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .filter(|n| (min..=max).contains(n))
            .ok_or_else(|| invalid(format!("{at}: {key} must be an integer in {min}..={max}"))),
    }
}

fn entity_sel(obj: &Map<String, Value>, at: &str) -> Result<EntitySel, ToolError> {
    let index = uint(obj, "index", 0, 0, MAX_ENTITY_INDEX, at)? as usize;
    let mut find = Map::new();
    for key in ["class", "name"] {
        if let Some(v) = obj.get(key) {
            find.insert(key.to_string(), v.clone());
        }
    }
    // the bridge returns at most `max` matches: ask for exactly as many as the index needs
    find.insert("max".into(), json!(index + 1));
    let params = entities::params(&Value::Object(find)).map_err(|e| invalid(format!("{at}: {}", e.message)))?;
    Ok(EntitySel { params, index })
}

fn parse_step(i: usize, v: &Value) -> Result<Step, ToolError> {
    let at = format!("steps[{i}]");
    let obj = v.as_object().ok_or_else(|| invalid(format!("{at} must be an object with a kind")))?;
    let name: &'static str = match obj.get("kind").and_then(Value::as_str) {
        Some("input") => "input",
        Some("wait_ms") => "wait_ms",
        Some("wait_state") => "wait_state",
        Some("wait_log") => "wait_log",
        Some("expect_state") => "expect_state",
        Some("wait_entity") => "wait_entity",
        Some("expect_entity") => "expect_entity",
        other => {
            return Err(invalid(format!(
                "{at}: kind must be input, wait_ms, wait_state, wait_log, expect_state, wait_entity or expect_entity (got {other:?})"
            )))
        }
    };
    let label = match obj.get("label") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.len() <= MAX_LABEL => Some(s.clone()),
        _ => return Err(invalid(format!("{at}: label must be a string of at most {MAX_LABEL} bytes"))),
    };
    let allow = |keys: &[&str]| -> Result<(), ToolError> {
        match obj.keys().find(|k| *k != "kind" && *k != "label" && !keys.contains(&k.as_str())) {
            None => Ok(()),
            Some(k) => Err(invalid(format!("{at}: unknown key {k:?} for kind {name} (allowed: {})", keys.join(", ")))),
        }
    };
    let predicate = |skip: &[&str]| -> Result<Predicate, ToolError> {
        let mut p = obj.clone();
        for k in skip.iter().copied().chain(["kind", "label"]) {
            p.remove(k);
        }
        Predicate::parse(&Value::Object(p)).map_err(|e| invalid(format!("{at}: {}", e.message)))
    };
    let kind = match name {
        "input" => {
            allow(&["steps", "lockWaitMs"])?;
            let lock_wait_ms = match obj.get("lockWaitMs") {
                None | Some(Value::Null) => None,
                Some(_) => Some(uint(obj, "lockWaitMs", MOVEMENT_LOCK_WAIT_MS, 0, MOVEMENT_LOCK_WAIT_MAX_MS, &at)?),
            };
            let args = json!({"steps": obj.get("steps").cloned().unwrap_or(Value::Null)});
            sequence::validate(&args).map_err(|e| invalid(format!("{at}: {}", e.message)))?;
            Kind::Input { args, lock_wait_ms }
        }
        "wait_ms" => {
            allow(&["ms"])?;
            if obj.get("ms").is_none() {
                return Err(invalid(format!("{at}: wait_ms needs ms (1..={MAX_WAIT_MS})")));
            }
            Kind::WaitMs(uint(obj, "ms", 0, 1, MAX_WAIT_MS, &at)?)
        }
        "wait_state" => Kind::WaitState {
            pred: predicate(&["timeoutMs", "pollMs"])?,
            timeout_ms: uint(obj, "timeoutMs", DEFAULT_TIMEOUT_MS, 1, MAX_WAIT_MS, &at)?,
            poll_ms: uint(obj, "pollMs", POLL_DEFAULT_MS, POLL_MIN_MS, POLL_MAX_MS, &at)?,
        },
        "expect_state" => Kind::Expect(predicate(&[])?),
        "wait_entity" => Kind::WaitEntity {
            sel: entity_sel(obj, &at)?,
            pred: predicate(&["timeoutMs", "pollMs", "class", "name", "index"])?,
            timeout_ms: uint(obj, "timeoutMs", DEFAULT_TIMEOUT_MS, 1, MAX_WAIT_MS, &at)?,
            poll_ms: uint(obj, "pollMs", POLL_DEFAULT_MS, POLL_MIN_MS, POLL_MAX_MS, &at)?,
        },
        "expect_entity" => Kind::ExpectEntity {
            sel: entity_sel(obj, &at)?,
            pred: predicate(&["class", "name", "index"])?,
        },
        _ => {
            allow(&["pattern", "timeoutMs", "since"])?;
            let pattern = obj
                .get("pattern")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty() && p.len() <= MAX_PATTERN)
                .ok_or_else(|| invalid(format!("{at}: pattern must be a regex of 1..={MAX_PATTERN} bytes")))?;
            let re = Regex::new(pattern).map_err(|e| invalid(format!("{at}: pattern is not a valid regex: {e}")))?;
            let since_start = match obj.get("since").and_then(Value::as_str) {
                None | Some("step") => false,
                Some("start") => true,
                Some(o) => return Err(invalid(format!("{at}: since must be step or start (got {o:?})"))),
            };
            Kind::WaitLog {
                re,
                pattern: pattern.to_string(),
                timeout_ms: uint(obj, "timeoutMs", DEFAULT_TIMEOUT_MS, 1, MAX_WAIT_MS, &at)?,
                since_start,
            }
        }
    };
    Ok(Step { label, name, kind })
}

fn parse(args: &Value) -> Result<(Vec<Step>, bool), ToolError> {
    let obj = args.as_object().ok_or_else(|| invalid("arguments must be an object"))?;
    if let Some(k) = obj.keys().find(|k| *k != "steps" && *k != "keepInput") {
        return Err(invalid(format!("unknown key {k:?} in play_test (allowed: steps, keepInput)")));
    }
    let items = obj
        .get("steps")
        .and_then(Value::as_array)
        .filter(|a| (1..=MAX_STEPS).contains(&a.len()))
        .ok_or_else(|| invalid(format!("steps must be an array of 1..={MAX_STEPS} steps")))?;
    let keep = match obj.get("keepInput") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        _ => return Err(invalid("keepInput must be true or false")),
    };
    let steps = items.iter().enumerate().map(|(i, s)| parse_step(i, s)).collect::<Result<Vec<_>, _>>()?;
    Ok((steps, keep))
}

/// How one step ended.
enum Outcome {
    /// the step held
    Ok(Value),
    /// the step ran and its check did not hold (the run ends: `failed`)
    Failed(Value),
    /// the call budget cannot cover the step (the run ends: `budget`); the value says why
    Budget(Value),
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

async fn read_local(bridge: &mut BridgeClient, ctx: &CallCtx) -> Result<Value, ToolError> {
    let v = bridge
        .request("game.local", json!({}), RequestOpts::deadline(ctx.budget.deadline_ms()?))
        .await
        .map_err(ToolError::from)
        .map_err(no_map)?
        .result;
    if v.get("inGame").and_then(Value::as_bool) == Some(false) {
        return Err(no_map_error());
    }
    Ok(v)
}

/// The selected game entity right now (`None`: nothing matches yet). `game.entities` is the same
/// bridge method as `entity_find`.
async fn read_entity(bridge: &mut BridgeClient, ctx: &CallCtx, sel: &EntitySel) -> Result<Option<Value>, ToolError> {
    let reply = entities::request(bridge, ctx, sel.params.clone()).await.map_err(no_map)?;
    if reply.get("inGame").and_then(Value::as_bool) == Some(false) {
        return Err(no_map_error());
    }
    Ok(entities::pick(&reply, sel.index).cloned())
}

/// What a passing/failing entity check reports: the predicate, the value it saw and which entity.
fn entity_report(pred: &Predicate, entity: &Value) -> (bool, Value) {
    let verdict = pred.check(entity);
    let mut v = pred.describe();
    v["got"] = verdict.got;
    v["entity"] = json!({
        "entindex": entity.get("entindex").cloned().unwrap_or(Value::Null),
        "class": entity.get("class").cloned().unwrap_or(Value::Null),
        "name": entity.get("name").cloned().unwrap_or(Value::Null),
    });
    (verdict.ok, v)
}

/// Why the game ignores movement right now, from a `game.rules` reply: the freeze period, the team
/// intro and the restart window (`mp_restartgame`) all lock the player in place.
fn movement_lock(rules: &Value) -> Option<&'static str> {
    [("freeze", "freeze period"), ("teamIntro", "team intro"), ("gameRestart", "restart window")]
        .into_iter()
        .find(|(key, _)| rules.get(*key).and_then(Value::as_bool) == Some(true))
        .map(|(_, why)| why)
}

async fn read_rules(bridge: &mut BridgeClient, ctx: &CallCtx) -> Option<Value> {
    let deadline = ctx.budget.deadline_ms().ok()?;
    bridge.request("game.rules", json!({}), RequestOpts::deadline(deadline)).await.ok().map(|r| r.result)
}

/// Before an `input` step: while `game.rules` says movement is locked, wait for it to end (bounded
/// by `cap_ms` and the budget left). `None` when the bridge has no `game.rules`
/// or nothing was locked; else `{phase, waitedMs, stillLocked}`.
async fn wait_for_movement(bridge: &mut BridgeClient, ctx: &CallCtx, left: Duration, explicit_ms: Option<u64>) -> Option<Value> {
    if !bridge.has("game.rules") {
        return None;
    }
    let budget_cap = left.saturating_sub(Duration::from_millis(2_000));
    // the effective wait: the step's lockWaitMs, else the default, sized up from mp_freezetime once
    // a lock is actually seen (game.rules has no remaining time of its own)
    let (mut limit_ms, mut source) = match explicit_ms {
        Some(n) => (n, "lockWaitMs"),
        None => (MOVEMENT_LOCK_WAIT_MS, "default"),
    };
    let started = Instant::now();
    let mut phase = None;
    let mut sized = explicit_ms.is_some();
    loop {
        let locked = movement_lock(&read_rules(bridge, ctx).await?);
        let Some(why) = locked else { break };
        phase.get_or_insert(why);
        if !sized {
            sized = true;
            if let Some(freeze) = freezetime_ms(bridge, ctx).await {
                let want = (freeze + FREEZE_SLACK_MS).min(MOVEMENT_LOCK_WAIT_MAX_MS);
                if want > limit_ms {
                    (limit_ms, source) = (want, "mp_freezetime");
                }
            }
        }
        let cap = Duration::from_millis(limit_ms).min(budget_cap);
        if started.elapsed() >= cap {
            return Some(json!({"phase": phase, "waitedMs": ms(started.elapsed()), "stillLocked": true, "lockWaitMs": limit_ms, "lockWaitSource": source}));
        }
        tokio::time::sleep(Duration::from_millis(LOCK_POLL_MS).min(cap.saturating_sub(started.elapsed()))).await;
    }
    phase.map(|why| json!({"phase": why, "waitedMs": ms(started.elapsed()), "stillLocked": false, "lockWaitMs": limit_ms, "lockWaitSource": source}))
}

/// `mp_freezetime` in ms from `cvar.get`; `None` when the bridge has no `cvar.get`, the cvar did not
/// read or its value is not a number.
async fn freezetime_ms(bridge: &mut BridgeClient, ctx: &CallCtx) -> Option<u64> {
    if !bridge.has("cvar.get") {
        return None;
    }
    let deadline = ctx.budget.deadline_ms().ok()?;
    let reply = bridge.request("cvar.get", json!({"names": ["mp_freezetime"]}), RequestOpts::deadline(deadline)).await.ok()?.result;
    let entry = reply.get("cvars")?.as_array()?.first()?;
    if entry.get("found").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let secs = match entry.get("value")? {
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        v => v.as_f64()?,
    };
    (secs.is_finite() && secs >= 0.0).then_some((secs * 1000.0) as u64)
}

/// After an `input` step: `movementLock` (what the wait saw) and, when the step held a move key for
/// a while and the player did not move, a `note` that names the likely reason from a fresh
/// `game.rules` / `game.local` read.
async fn annotate_input(bridge: &mut BridgeClient, ctx: &CallCtx, args: &Value, lock: Option<Value>, out: &mut Value) {
    if let Some(lock) = lock {
        let note = if lock["stillLocked"] == json!(true) {
            format!(
                "movement stayed locked ({}) for the whole {} ms wait",
                lock["phase"].as_str().unwrap_or("freeze"),
                lock["waitedMs"]
            )
        } else {
            format!(
                "waited {} ms for the {} to end before sending input (keys pressed during it stay dead even after it ends)",
                lock["waitedMs"],
                lock["phase"].as_str().unwrap_or("freeze period")
            )
        };
        out["movementLock"] = lock;
        out["note"] = json!(note);
    }
    let holds_move = args
        .get("steps")
        .and_then(Value::as_array)
        .is_some_and(|steps| {
            steps.iter().any(|s| {
                s.get("press")
                    .and_then(Value::as_array)
                    .is_some_and(|keys| keys.iter().any(|k| k.as_str().is_some_and(|k| MOVE_KEYS.contains(&k))))
            })
        });
    let ticks = out.pointer("/totals/ticks").and_then(Value::as_u64).unwrap_or(0);
    let distance = out.pointer("/totals/distance").and_then(Value::as_f64).unwrap_or(f64::MAX);
    if !holds_move || ticks < 8 || distance >= 1.0 || out.get("movementLock").is_some() {
        return;
    }
    let mut why = vec![format!("a move key was held for {ticks} ticks and the player moved {distance:.1} units")];
    if let Some(rules) = read_rules(bridge, ctx).await {
        if let Some(phase) = movement_lock(&rules) {
            why.push(format!("game.rules says the round is in its {phase}: the game ignores movement until it ends"));
        }
    }
    if let Ok(local) = read_local(bridge, ctx).await {
        if local.pointer("/player/alive").and_then(Value::as_bool) == Some(false) {
            why.push("the player is dead (a dead player does not move; run_map {team} or game_state's hint says how to spawn)".into());
        }
        if local.pointer("/player/team").and_then(Value::as_u64) == Some(0) {
            why.push("the player is on team 0 (no team)".into());
        }
    }
    why.push("otherwise the way may be blocked by geometry (check player.origin against the map)".into());
    out["note"] = json!(why.join("; "));
}

/// Does an input step press (or tap) any key? Release-only and angles-only steps do not.
fn presses_keys(args: &Value) -> bool {
    args.get("steps").and_then(Value::as_array).is_some_and(|steps| {
        steps.iter().any(|s| {
            ["press", "tap"]
                .iter()
                .any(|k| s.get(*k).and_then(Value::as_array).is_some_and(|keys| !keys.is_empty()))
        })
    })
}

fn failed(message: impl Into<String>, extra: Value) -> Outcome {
    let mut v = json!({"message": message.into()});
    if let (Some(m), Value::Object(e)) = (v.as_object_mut(), extra) {
        m.extend(e);
    }
    Outcome::Failed(v)
}

fn out_of_budget(why: &str, left: Duration) -> Outcome {
    Outcome::Budget(json!({"message": why, "budgetLeftMs": ms(left)}))
}

async fn run_step(
    step: &Step,
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    log: &mut ConsoleWatch,
    sent_input: &mut bool,
    lock_waited: &mut bool,
) -> Result<Outcome, ToolError> {
    let left = ctx.budget.remaining().saturating_sub(END_MARGIN);
    if left.is_zero() {
        return Ok(out_of_budget("no budget left for this step", left));
    }
    match &step.kind {
        Kind::Input { args, lock_wait_ms } => {
            // only a step that PRESSES a key waits (a release/angles-only step is harmless in a
            // lock), and only once per run: a lock flag that sticks must not burn the budget on
            // every input step
            let lock = if *lock_wait_ms != Some(0) && !*lock_waited && presses_keys(args) {
                let lock = wait_for_movement(bridge, ctx, left, *lock_wait_ms).await;
                *lock_waited = lock.is_some();
                lock
            } else {
                None
            };
            if let Some(lock) = lock.as_ref().filter(|l| l["stillLocked"] == json!(true)) {
                // keys pressed inside the lock stay dead after it ends (run 2, call 847), so
                // sending now would only hide the cause behind a later failed wait_state
                return Ok(failed(
                    format!(
                        "movement stayed locked ({}) for the whole {} ms wait: nothing was sent, because keys pressed during the lock stay dead even after it ends. Raise this step's lockWaitMs, or shorten the lock (console_exec \"mp_freezetime 0; mp_restartgame 1\"; run_map's team), then repeat the run",
                        lock["phase"].as_str().unwrap_or("freeze period"),
                        lock["waitedMs"]
                    ),
                    json!({"movementLocked": true, "movementLock": lock, "inputSent": false}),
                ));
            }
            match sequence::run(bridge, ctx, args, false).await {
                Ok(out) => {
                    *sent_input = true;
                    let mut json = out.json;
                    annotate_input(bridge, ctx, args, lock, &mut json).await;
                    Ok(Outcome::Ok(json))
                }
                Err(e) if e.name == "ToolBudgetExceeded" && e.data.get("commandsSent") == Some(&json!(false)) => {
                    Ok(Outcome::Budget(e.to_json()))
                }
                Err(e) => {
                    // a refusal before the first input.set reports inputSent:false; anything else may
                    // have left keys held
                    if e.data.get("inputSent") != Some(&json!(false)) {
                        *sent_input = true;
                    }
                    let mut v = no_map(e).to_json();
                    // run() was told not to clear, so its inputCleared:false would contradict the
                    // run's own closing clear
                    if let Some(d) = v.get_mut("data").and_then(Value::as_object_mut) {
                        d.remove("inputCleared");
                    }
                    Ok(Outcome::Failed(v))
                }
            }
        }
        Kind::WaitMs(wait) => {
            let wait = Duration::from_millis(*wait);
            if wait > left {
                return Ok(out_of_budget("wait_ms is longer than the budget left", left));
            }
            tokio::time::sleep(wait).await;
            Ok(Outcome::Ok(json!({"waitedMs": ms(wait)})))
        }
        Kind::Expect(pred) => {
            let state = read_local(bridge, ctx).await?;
            let verdict = pred.check(&state);
            let mut v = pred.describe();
            v["got"] = verdict.got;
            Ok(if verdict.ok { Outcome::Ok(v) } else { failed("expectation does not hold", v) })
        }
        Kind::ExpectEntity { sel, pred } => {
            let Some(entity) = read_entity(bridge, ctx, sel).await? else {
                return Ok(failed("no entity of the running game matches", json!({"find": sel.params, "index": sel.index})));
            };
            let (ok, v) = entity_report(pred, &entity);
            Ok(if ok { Outcome::Ok(v) } else { failed("expectation does not hold", v) })
        }
        Kind::WaitEntity { sel, pred, timeout_ms, poll_ms } => {
            let want = Duration::from_millis(*timeout_ms);
            let clamped = want > left;
            let limit = want.min(left);
            let started = Instant::now();
            let mut polls = 0u64;
            loop {
                let entity = read_entity(bridge, ctx, sel).await?;
                polls += 1;
                let mut v = match &entity {
                    Some(e) => {
                        let (ok, v) = entity_report(pred, e);
                        if ok {
                            let mut v = v;
                            v["waitedMs"] = json!(ms(started.elapsed()));
                            v["polls"] = json!(polls);
                            return Ok(Outcome::Ok(v));
                        }
                        v
                    }
                    None => {
                        let mut v = pred.describe();
                        v["found"] = json!(false);
                        v["find"] = sel.params.clone();
                        v
                    }
                };
                if started.elapsed() >= limit {
                    v["waitedMs"] = json!(ms(started.elapsed()));
                    v["polls"] = json!(polls);
                    if let Some(e) = &entity {
                        v["origin"] = e.get("origin").cloned().unwrap_or(Value::Null);
                    }
                    let what = if entity.is_some() { "not reached" } else { "no entity matched, so never reached" };
                    return Ok(if clamped {
                        v["message"] = json!(format!("{} {what} before the call budget ran out", pred.path));
                        Outcome::Budget(v)
                    } else {
                        failed(format!("entity {} {what} within {timeout_ms} ms", pred.path), v)
                    });
                }
                let pause = Duration::from_millis(*poll_ms).min(limit.saturating_sub(started.elapsed()));
                tokio::time::sleep(pause).await;
            }
        }
        Kind::WaitState { pred, timeout_ms, poll_ms } => {
            let want = Duration::from_millis(*timeout_ms);
            let clamped = want > left;
            let limit = want.min(left);
            let started = Instant::now();
            let mut polls = 0u64;
            loop {
                let state = read_local(bridge, ctx).await?;
                polls += 1;
                let verdict = pred.check(&state);
                if verdict.ok {
                    let mut v = pred.describe();
                    v["got"] = verdict.got;
                    v["waitedMs"] = json!(ms(started.elapsed()));
                    v["polls"] = json!(polls);
                    return Ok(Outcome::Ok(v));
                }
                if started.elapsed() >= limit {
                    let mut v = pred.describe();
                    v["got"] = verdict.got;
                    v["waitedMs"] = json!(ms(started.elapsed()));
                    v["polls"] = json!(polls);
                    v["origin"] = state.pointer("/player/origin").cloned().unwrap_or(Value::Null);
                    return Ok(if clamped {
                        v["message"] = json!(format!("{} not reached before the call budget ran out", pred.path));
                        Outcome::Budget(v)
                    } else {
                        failed(format!("{} not reached within {timeout_ms} ms", pred.path), v)
                    });
                }
                let pause = Duration::from_millis(*poll_ms).min(limit.saturating_sub(started.elapsed()));
                tokio::time::sleep(pause).await;
            }
        }
        Kind::WaitLog { re, pattern, timeout_ms, since_start } => {
            let want = Duration::from_millis(*timeout_ms);
            let clamped = want > left;
            let limit = want.min(left);
            // everything up to now is "before this step"; a since-start wait looks from the run's start
            log.poll();
            let mark = if *since_start { 0 } else { log.seen().len() };
            let started = Instant::now();
            loop {
                log.poll();
                let hay = &log.seen()[mark..];
                if let Some(m) = re.find(hay) {
                    let from = hay[..m.start()].rfind('\n').map_or(0, |i| i + 1);
                    let to = hay[m.end()..].find('\n').map_or(hay.len(), |i| m.end() + i);
                    return Ok(Outcome::Ok(json!({
                        "pattern": pattern,
                        "line": hay[from..to].trim_end(),
                        "waitedMs": ms(started.elapsed()),
                    })));
                }
                if started.elapsed() >= limit {
                    let tail: Vec<String> = log.tail(5);
                    let v = json!({
                        "pattern": pattern, "waitedMs": ms(started.elapsed()),
                        "consoleLogFound": log.exists(), "tail": tail,
                    });
                    return Ok(if clamped {
                        let mut v = v;
                        v["message"] = json!("log line not seen before the call budget ran out");
                        Outcome::Budget(v)
                    } else {
                        failed(format!("no console.log line matched {pattern:?} within {timeout_ms} ms"), v)
                    });
                }
                let pause = Duration::from_millis(LOG_POLL_MS).min(limit.saturating_sub(started.elapsed()));
                tokio::time::sleep(pause).await;
            }
        }
    }
}

/// Sends `input.clear` from a spawned task if a run unwinds (a panic in a step) with input sent: the
/// request task dies with no reply and the keys would stay held. Disarmed after the normal clear.
struct ClearOnUnwind {
    bridge: SharedBridge,
    armed: bool,
}

impl Drop for ClearOnUnwind {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let bridge = self.bridge.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = bridge
                    .lock()
                    .await
                    .request("input.clear", json!({}), RequestOpts::ungated(sequence::CLEAR_FLOOR_MS))
                    .await;
            });
        }
    }
}

pub async fn play_test(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let (steps, keep_input) = parse(&args)?;
    let started = Instant::now();
    let mut bridge = state.bridge.lock().await;
    // refuse before any input: no map in the engine
    read_local(&mut bridge, &ctx).await?;
    // an entity step on a bridge without game.entities would otherwise fail only after the input
    // steps before it were sent
    if !bridge.has("game.entities")
        && steps.iter().any(|s| matches!(s.kind, Kind::WaitEntity { .. } | Kind::ExpectEntity { .. }))
    {
        return Err(ToolError::not_implemented(
            "CapabilityMissing",
            "the bridge does not advertise game.entities (nothing was sent): wait_entity / expect_entity need it",
            json!({"missing": ["game.entities"], "blockedOn": "docs/data/bridge-protocol.md §7 method table"}),
        ));
    }
    let mut log = ConsoleWatch::open(console_log_path(&state));
    let mut sent_input = false;
    let mut lock_waited = false;
    let mut guard = ClearOnUnwind { bridge: state.bridge.clone(), armed: false };
    let mut rows: Vec<Value> = Vec::new();
    let mut ended_by = "complete";
    let mut failed_step: Option<usize> = None;
    let mut error: Option<ToolError> = None;
    for (i, step) in steps.iter().enumerate() {
        let t0 = Instant::now();
        let outcome = run_step(step, &mut bridge, &ctx, &mut log, &mut sent_input, &mut lock_waited).await;
        guard.armed = sent_input;
        let mut row = json!({"step": i, "kind": step.name, "label": step.label, "ms": ms(t0.elapsed())});
        match outcome {
            Ok(Outcome::Ok(v)) => {
                row["ok"] = json!(true);
                row["result"] = v;
                rows.push(row);
            }
            Ok(Outcome::Failed(v)) => {
                row["ok"] = json!(false);
                row["error"] = v;
                rows.push(row);
                ended_by = "failed";
                failed_step = Some(i);
                break;
            }
            Ok(Outcome::Budget(v)) => {
                row["ok"] = json!(false);
                row["error"] = v;
                rows.push(row);
                ended_by = "budget";
                failed_step = Some(i);
                break;
            }
            Err(e) => {
                row["ok"] = json!(false);
                row["error"] = e.to_json();
                rows.push(row);
                ended_by = "failed";
                failed_step = Some(i);
                error = Some(e);
                break;
            }
        }
    }
    let passed = ended_by == "complete";
    // clear on every end but a passing keepInput run; the clear may overrun the budget a little
    let cleared = if sent_input && !(passed && keep_input) {
        let ms = (ctx.budget.remaining().as_millis() as u64).max(sequence::CLEAR_FLOOR_MS);
        match bridge.request("input.clear", json!({}), RequestOpts::ungated(ms)).await {
            Ok(_) => json!(true),
            Err(_) => json!("failed"),
        }
    } else {
        json!(false)
    };
    guard.armed = false;
    drop(bridge);
    if let Some(e) = error {
        // a bridge/protocol error mid-run: the row says which step; the tool fails like any other
        return Err(e.with_data(json!({"steps": rows, "failedStep": failed_step, "inputCleared": cleared})));
    }
    let mut out = json!({
        "passed": passed,
        "endedBy": ended_by,
        "stepsRun": rows.len(),
        "stepsTotal": steps.len(),
        "steps": rows,
        "inputCleared": cleared,
        "durationMs": ms(started.elapsed()),
    });
    if let Some(i) = failed_step {
        out["failedStep"] = json!(i);
    }
    Ok(out.into())
}
