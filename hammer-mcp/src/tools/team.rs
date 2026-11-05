//! The local player's team in the tools session. After `run_map` the player is on team 0 and dead
//! (run2: the agent needed `console_exec "jointeam 3 1"` and a restart to play). This module holds
//! the three pieces that fix that:
//!
//! * [`team_arg`]: `run_map`'s optional `team: "ct"|"t"`;
//! * [`join`]: after the load is confirmed, post `jointeam <2|3> 1` through `console_exec`'s own
//!   route (so the `vtm_gate` rules and the bridge/VToolMessage choice are the ones `console_exec`
//!   follows) and wait, polling `game.local` against a deadline, until the player is alive on that
//!   team. A player who joined but is waiting for the next round (dead on the team) gets one
//!   `mp_restartgame 1`, reported as `restartedGame`;
//! * [`hint`]: `game_state`'s `hint` for a player on team 0 or dead, with the exact fix.
use super::console_live;
use crate::state::HammerState;
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{CallCtx, ToolError, ToolOutput};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// How often `game.local` is re-read while waiting for the spawn.
const POLL: Duration = Duration::from_millis(150);
/// The first wait (the join alone); a player still dead then gets the restart.
const JOIN_WAIT: Duration = Duration::from_millis(2_500);
/// The longest the whole join may take.
const TOTAL_WAIT: Duration = Duration::from_millis(15_000);
/// Kept back from the call budget for the reply.
const MARGIN: Duration = Duration::from_millis(1_000);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Team {
    Ct,
    T,
}

impl Team {
    /// The engine's team number (`jointeam 2` = T, `jointeam 3` = CT).
    pub fn number(self) -> u64 {
        match self {
            Team::Ct => 3,
            Team::T => 2,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Team::Ct => "ct",
            Team::T => "t",
        }
    }
    /// The console line (the trailing `1` lets the game pick the slot).
    pub fn join_command(self) -> String {
        format!("jointeam {} 1", self.number())
    }
}

pub fn team_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["ct", "t"],
        "description": "join this team and wait until alive (without it the player is on team 0, dead)"
    })
}

/// `restartIfDead` (default true): may the join send `mp_restartgame 1` when the player is on the
/// team but dead? That restarts the WHOLE round and match state, harmless right after a fresh load
/// and unwanted anywhere else.
pub fn restart_if_dead_arg(args: &Value) -> Result<bool, ToolError> {
    match args.get("restartIfDead") {
        None | Some(Value::Null) => Ok(true),
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => Err(ToolError::invalid_arguments(format!("restartIfDead must be a boolean, got {other}"))),
    }
}

pub fn restart_if_dead_schema() -> Value {
    json!({
        "type": "boolean",
        "default": true,
        "description": "with team: send mp_restartgame 1 if joined but dead (restarts the match score); false never restarts"
    })
}

/// `team` from the arguments (absent/null = none).
pub fn team_arg(args: &Value) -> Result<Option<Team>, ToolError> {
    match args.get("team") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => match s.to_ascii_lowercase().as_str() {
            "ct" => Ok(Some(Team::Ct)),
            "t" => Ok(Some(Team::T)),
            _ => Err(ToolError::invalid_arguments(format!("team must be \"ct\" or \"t\", got {s:?}"))),
        },
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "team must be \"ct\" or \"t\", got {other}"
        ))),
    }
}

/// What a `game.local` reply says about the player: `(team, alive)`; `None` when no map is up.
fn player_of(local: &Value) -> Option<(u64, bool)> {
    if local.get("inGame").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let player = local.get("player")?;
    Some((
        player.get("team").and_then(Value::as_u64).unwrap_or(0),
        player.get("alive").and_then(Value::as_bool).unwrap_or(false),
    ))
}

/// `game_state`'s `hint` when the player cannot play: on team 0 or dead. `{problem, fix:{tool,
/// args}, why}`; `None` for a live player on a team (or no map).
pub fn hint(local: &Value) -> Option<Value> {
    let (team, alive) = player_of(local)?;
    if team <= 1 {
        let what = if team == 0 { "team 0 (no team)" } else { "team 1 (spectator)" };
        return Some(json!({
            "problem": format!("the local player is on {what} and cannot play"),
            "fix": {"tool": "console_exec", "args": {"command": "jointeam 3 1"}},
            "why": "joins CT (jointeam 2 1 = T); then game_state again, and if the player is still \
                    dead console_exec \"mp_restartgame 1\" respawns it. run_map {team:\"ct\"|\"t\"} \
                    does all of this right after the load",
        }));
    }
    if !alive {
        return Some(json!({
            "problem": "the local player is dead (it respawns at the next round start)",
            "fix": {"tool": "console_exec", "args": {"command": "mp_restartgame 1"}},
            "why": "starts a new round now; wait about 1 s, then game_state again (the freeze \
                    period locks movement: play_test input steps wait it out)",
        }));
    }
    None
}

async fn read_local(state: &HammerState, ctx: &CallCtx) -> Option<Value> {
    let deadline = ctx.budget.deadline_ms().ok()?;
    let mut bridge = state.bridge.lock().await;
    bridge.request("game.local", json!({}), RequestOpts::deadline(deadline)).await.ok().map(|r| r.result)
}

/// Poll `game.local` until the player is alive on `team` or `limit` passes. The last
/// `(team, alive)` seen is returned with whether it is the wanted state.
async fn wait_alive(state: &HammerState, ctx: &CallCtx, team: Team, limit: Duration) -> (Option<(u64, bool)>, bool) {
    let started = Instant::now();
    let mut seen = None;
    loop {
        if let Some(local) = read_local(state, ctx).await {
            seen = player_of(&local);
            if seen == Some((team.number(), true)) {
                return (seen, true);
            }
        }
        if started.elapsed() >= limit {
            return (seen, false);
        }
        tokio::time::sleep(POLL.min(limit.saturating_sub(started.elapsed()))).await;
    }
}

/// One console line through `console_exec` (its guard, gate and route), without its output.
async fn exec(state: &HammerState, ctx: &CallCtx, line: &str) -> Result<Value, ToolError> {
    let slice = CallCtx { budget: ctx.budget.slice(Duration::from_millis(4_000)) };
    let out: ToolOutput =
        console_live::console_exec(state.clone(), slice, json!({"command": line, "waitMs": 300})).await?;
    Ok(json!({"command": line, "route": out.json.get("route").cloned().unwrap_or(Value::Null)}))
}

/// Put the local player on `team` and alive. Never an error: the report says how far it got
/// (`joined`, `alive`, `restartedGame`, `commands`, `waitedMs`, `note`), so a load that succeeded
/// stays a success. `restart_if_dead` false never sends `mp_restartgame 1`.
pub async fn join(state: &HammerState, ctx: &CallCtx, team: Team, restart_if_dead: bool) -> Value {
    let started = Instant::now();
    let left = ctx.budget.remaining().saturating_sub(MARGIN);
    let limit = left.min(TOTAL_WAIT);
    let mut report = json!({
        "requested": team.name(), "joined": false, "alive": false, "restartedGame": false,
        "commands": [],
    });
    let fail = |mut report: Value, note: String| {
        report["waitedMs"] = json!(started.elapsed().as_millis() as u64);
        report["note"] = json!(note);
        report
    };
    if limit < Duration::from_millis(1_500) {
        return fail(report, "the tool budget was spent by the load; join with console_exec \"jointeam N 1\" (3 = CT, 2 = T)".into());
    }
    let push = |report: &mut Value, step: Value| report["commands"].as_array_mut().unwrap().push(step);
    match exec(state, ctx, &team.join_command()).await {
        Ok(step) => push(&mut report, step),
        Err(e) => return fail(report, format!("jointeam was not sent: {} ({})", e.message, e.name)),
    }
    let first = JOIN_WAIT.min(limit.saturating_sub(started.elapsed()));
    let (mut seen, mut ok) = wait_alive(state, ctx, team, first).await;
    if !ok && restart_if_dead && matches!(seen, Some((t, false)) if t == team.number()) {
        // on the team but waiting for the next round: start one
        match exec(state, ctx, "mp_restartgame 1").await {
            Ok(step) => {
                push(&mut report, step);
                report["restartedGame"] = json!(true);
                let rest = limit.saturating_sub(started.elapsed());
                (seen, ok) = wait_alive(state, ctx, team, rest).await;
            }
            Err(e) => report["restartError"] = json!(e.message),
        }
    }
    report["joined"] = json!(matches!(seen, Some((t, _)) if t == team.number()));
    report["alive"] = json!(ok);
    report["seen"] = json!(seen.map(|(t, a)| json!({"team": t, "alive": a})));
    report["restartAllowed"] = json!(restart_if_dead);
    if ok {
        report["waitedMs"] = json!(started.elapsed().as_millis() as u64);
        report["note"] = json!("the player is alive on the team; the freeze period may still lock movement for a few seconds (play_test input steps wait it out)");
        report
    } else {
        let note = if !restart_if_dead && matches!(seen, Some((t, false)) if t == team.number()) {
            "the player joined but is dead and restartIfDead is false: console_exec \"mp_restartgame 1\" respawns it (it restarts the round)"
        } else {
            "the player is not alive on that team yet: game_state shows where it stands (its hint names the fix)"
        };
        fail(report, note.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(team: u64, alive: bool) -> Value {
        json!({"inGame": true, "player": {"team": team, "alive": alive}})
    }

    #[test]
    fn team_args() {
        assert_eq!(team_arg(&json!({})).unwrap(), None);
        assert_eq!(team_arg(&json!({"team": "ct"})).unwrap(), Some(Team::Ct));
        assert_eq!(team_arg(&json!({"team": "T"})).unwrap(), Some(Team::T));
        assert!(team_arg(&json!({"team": "spec"})).is_err());
        assert!(team_arg(&json!({"team": 3})).is_err());
        assert_eq!(Team::Ct.join_command(), "jointeam 3 1");
        assert_eq!(Team::T.join_command(), "jointeam 2 1");
    }

    #[test]
    fn hints_name_the_exact_fix() {
        let h = hint(&local(0, false)).unwrap();
        assert_eq!(h["fix"]["args"]["command"], json!("jointeam 3 1"));
        let h = hint(&local(3, false)).unwrap();
        assert_eq!(h["fix"]["args"]["command"], json!("mp_restartgame 1"));
        let spec = hint(&local(1, false)).unwrap();
        assert_eq!(spec["fix"]["args"]["command"], json!("jointeam 3 1"), "a spectator joins a team, a restart does not spawn it");
        assert!(spec["problem"].as_str().unwrap().contains("spectator"));
        assert!(hint(&local(3, true)).is_none());
        assert!(restart_if_dead_arg(&json!({})).unwrap());
        assert!(!restart_if_dead_arg(&json!({"restartIfDead": false})).unwrap());
        assert!(restart_if_dead_arg(&json!({"restartIfDead": "no"})).is_err());
        assert!(hint(&json!({"inGame": false})).is_none());
    }
}
