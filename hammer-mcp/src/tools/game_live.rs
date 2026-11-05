//! The engine side of the tools session, for play-testing a map from Hammer without a second
//! bridge client (the bridge takes one: while hammer-mcp runs, client-mcp cannot attach).
//! The same tool names client-mcp has, over the same shared code (`cs2_mcp_common::play`):
//!
//! * `game_state`: the bridge's `game.local` (the local player as the CLIENT sees it: view,
//!   origin, velocity, active weapon), once or sampled `samples` times every `intervalMs`, which is
//!   how a jittering view shows up in numbers (e.g. a player carried by a scripted vehicle).
//!   `game_local` is its deprecated alias.
//! * `input_sequence`: input.set steps held for game ticks, input.clear at the end.
//! * `console_exec` / `cvar_get`: see [`super::console_live`].
//! * `input_set` / `input_clear`: the bridge's usercmd-level `input.set` / `input.clear` (virtual
//!   buttons held across frames, a view-angle override), the same methods client-mcp drives.
//! * `play_test` (`play_test.rs`): a scripted run of inputs, waits and expectations in one call.
use super::live_common::{invalid, reject_unknown_keys};
use crate::state::HammerState;
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::play;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Value};

/// The game tools' order, after [`super::selection_live::SELECTION_TOOL_ORDER`] (contract tests pin
/// it). `game_local` is the deprecated alias of `game_state`, registered last.
pub const GAME_TOOL_ORDER: [&str; 10] = [
    "game_state", "input_set", "input_clear", "console_exec", "cvar_get", "input_sequence",
    "play_test", "entity_find", "game_rules", "game_local",
];

/// What the three play tools say when no map is running in the engine: the engine answers
/// `-32024 NoLocalPlayer` (or `inGame: false`), which an agent reads as a broken bridge.
pub(crate) fn no_map(e: ToolError) -> ToolError {
    if e.name == "NoLocalPlayer" || e.code == Some(-32024) {
        return no_map_error();
    }
    e
}

pub(crate) fn no_map_error() -> ToolError {
    ToolError::new(
        "NoMapLoaded",
        "NoMapLoaded",
        "no map is running in the tools session's engine (the play tools read the local player)",
    )
    .retry_safe(true)
    .with_data(json!({"inGame": false}))
    .use_instead("run_map", "load the compiled map in the engine, then play-test it", None)
}

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "game_state",
            "The local player in the tools session's engine: inGame, map, player {team, alive, origin, velocity, eye angles, flags, active weapon}. \
             samples > 1 re-reads every intervalMs and adds originStepStats (smooth ride = steady steps; jitter = spikes). \
             A dead or team-0 player gets a `hint` with the fix. NoMapLoaded (useInstead run_map) when no map is up. Same as client-mcp's game_state.",
            ReadOnly,
            &["game.local"],
        )
        .with_schema(play::state::game_state_schema()),
        game_state,
    );
    r.register(
        ToolSpec::new(
            "input_set",
            "Press/release/tap virtual buttons for the local player and set the view-angle override (usercmd level). \
             press holds until released; tap holds `frames` input frames. Buttons: forward, back, left, right, moveleft, moveright, jump, duck, attack, attack2, use, reload, ... \
             angles [pitch, yaw, roll] is absolute and wins over console setang until null/input_clear. \
             release + tap of the same key in one call re-presses it.",
            Mutating,
            &["input.set"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "press": {"type": "array", "items": {"type": "string", "minLength": 1, "maxLength": 64}},
                "release": {"type": "array", "items": {"type": "string", "minLength": 1, "maxLength": 64}},
                "tap": {"type": "array", "items": {"type": "string", "minLength": 1, "maxLength": 64}},
                "frames": {"type": "integer", "minimum": 1, "maximum": 10000,
                           "description": "tap hold length; needs tap"},
                "angles": {
                    "description": "[pitch, yaw, roll]; null clears, absent keeps",
                    "oneOf": [
                        {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
                        {"type": "null"}
                    ]
                }
            },
            "additionalProperties": false
        })),
        input_set,
    );
    r.register(
        ToolSpec::new(
            "input_clear",
            "Release every virtual button and drop the view-angle override.",
            Mutating,
            &["input.clear"],
        )
        .with_schema(json!({"type": "object", "properties": {}, "additionalProperties": false})),
        input_clear,
    );
    super::console_live::register(r);
    r.register(
        ToolSpec::new(
            "input_sequence",
            "Run up to 32 input_set steps in order in one call. Each step takes the input_set fields plus turn, and holds holdTicks game ticks (default 1, max 640). \
             Presses persist across steps. Budget about 100 ms per step + ticks/64 s + 1.5 s, checked up front (ToolBudgetExceeded, maxTotalTicks says what fits). \
             Input is cleared at the end and after any error. Not for: holding keys across waits/checks (play_test). \
             Returns steps (startTick, endTick, origin, viewAngles), totals, start, final, inputCleared. NoMapLoaded (useInstead run_map) when no map is up.",
            Mutating,
            &["input.set", "input.clear", "game.local"],
        )
        .with_schema(play::sequence::input_sequence_schema()),
        input_sequence,
    );
    super::play_test::register(r);
    r.register(
        ToolSpec::new(
            "entity_find",
            "Find entities in the RUNNING game by class (designer name) and/or name (targetname): entindex, class, name, origin, angles, health, toggleState. \
             Not for: the Hammer document (find_entities). Use it to check where a mover or door is while the map runs. Needs class or name; truncated is true when more matched. \
             NoMapLoaded (useInstead run_map) when no map is running. In play_test use wait_entity / expect_entity.",
            ReadOnly,
            &["game.entities"],
        )
        .with_schema(play::entities::schema()),
        entity_find,
    );
    r.register(
        ToolSpec::new(
            "game_rules",
            "Round and warmup state of the RUNNING game: warmup, freeze, team intro, restart, round times, win status, bomb planted. \
             Use it to check the freeze period (play_test waits for it before pressing keys). A null field did not resolve (see schemaMissing). NoMapLoaded (useInstead run_map) when no map is running.",
            ReadOnly,
            &["game.rules"],
        )
        .with_schema(json!({"type": "object", "properties": {}, "additionalProperties": false})),
        game_rules,
    );
    r.alias("game_local", "game_state", json!({}));
}

pub async fn game_state(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    reject_unknown_keys(&args, &["samples", "intervalMs"])?;
    // the lock is taken per read inside, so an input_set can interleave with a sampling run
    let mut out = play::state::game_state(&state.bridge, &ctx, &args).await.map_err(no_map)?;
    // a player on team 0 or dead cannot play: say so with the exact fix (the last sample of a run)
    let last = out.json.get("samples").and_then(|s| s.as_array()).and_then(|s| s.last()).unwrap_or(&out.json);
    if let Some(hint) = super::team::hint(last) {
        if let Some(fix) = hint.get("fix").cloned() {
            let why = hint["why"].as_str().unwrap_or("").to_string();
            out = out.with_next_args(
                fix["tool"].as_str().unwrap_or("console_exec"),
                &why,
                fix["args"].clone(),
            );
        }
        out.json["hint"] = hint;
    }
    Ok(out)
}

pub async fn entity_find(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    reject_unknown_keys(&args, &["class", "name", "max"])?;
    let mut bridge = state.bridge.lock().await;
    let out = play::entities::find(&mut bridge, &ctx, &args).await.map_err(no_map)?;
    if out.json.get("inGame").and_then(Value::as_bool) == Some(false) {
        return Err(no_map_error());
    }
    Ok(out)
}

pub async fn game_rules(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    reject_unknown_keys(&args, &[])?;
    let mut bridge = state.bridge.lock().await;
    let out = play::state::rules(&mut bridge, &ctx).await.map_err(no_map)?;
    if out.get("inGame").and_then(Value::as_bool) == Some(false) {
        return Err(no_map_error());
    }
    Ok(out.into())
}

pub async fn input_sequence(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let mut bridge = state.bridge.lock().await;
    play::sequence::input_sequence(&mut bridge, &ctx, &args).await.map_err(no_map)
}

pub async fn input_set(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    reject_unknown_keys(&args, &["press", "release", "tap", "frames", "angles"])?;
    if args.get("frames").is_some() && args.get("tap").is_none() {
        return Err(invalid("frames applies to tap; give tap as well"));
    }
    if ["press", "release", "tap", "angles"].iter().all(|k| args.get(*k).is_none()) {
        return Err(invalid("nothing to do: give press, release, tap or angles"));
    }
    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request("input.set", args, RequestOpts::deadline(ctx.budget.deadline_ms()?))
        .await
        .map_err(ToolError::from)?;
    Ok(reply.result.into())
}

pub async fn input_clear(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    reject_unknown_keys(&args, &[])?;
    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request("input.clear", json!({}), RequestOpts::deadline(ctx.budget.deadline_ms()?))
        .await
        .map_err(ToolError::from)?;
    let mut result = reply.result;
    cs2_mcp_common::play::input::settle_cleared_reply(&mut result);
    Ok(result.into())
}

