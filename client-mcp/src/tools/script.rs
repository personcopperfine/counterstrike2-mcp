//! `script_run` (workflows/06-cs-script-vscript.md).
//!
//! `kind: "cs_script"` with an `addon` runs the scripts of an existing compiled addon map: it
//! launches (or reuses) the session with `launch_session {kind:"wine-tools", addon, map}`, confirms
//! the map with `game.local` when the launch did not, fires the optional `entry` (an entity input or
//! a console command) with `exec_console_confirmed`, waits for the `expect` regexes in order on the
//! console log (`ConsoleLog::wait_for_by`, bounded by `timeoutMs` and the call budget), and judges
//! the segment with `script::judge`. A run whose expects do not match is `passed: false`, not an
//! error. A wait the call budget cut short is `pending` (`passed: null`), not a failure.
//!
//! Everything else is refused with `NotImplementedYet` before any runtime call (convention 13):
//! `vscript` (CNopVM), and inline `source`/`path` (compiling JS needs resourcecompiler, not wired).
use crate::console::{ConsoleError, ConsoleLog, Cursor, WaitError, WaitFrom};
use crate::script::{self, RunArgs, ScriptKind};
use crate::state::ClientState;
use crate::tools::console::console_error;
use crate::tools::runtime_tools::{bridge_session_gate, launch_session};
use crate::tools::session::exec_console_confirmed;
use cs2_mcp_common::bridge::{Kind, RequestOpts};
use cs2_mcp_common::tools::{CallCtx, ToolError, ToolOutput};
use serde_json::{json, Value};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Kept back from the call budget so the reply still goes out in time (as in console_wait).
const BUDGET_MARGIN: Duration = Duration::from_millis(500);
/// Deadline of the `game.local` request that confirms the map when the launch did not.
const MAP_REQUEST_MS: u64 = 2000;

pub fn script_run_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": {"type": "string", "enum": ["cs_script", "vscript"]},
            "addon": {"type": "string", "description": "required for cs_script"},
            "map": {"type": "string", "description": "default: the addon name"},
            "entry": {
                "type": "object",
                "description": "fired after load: {target, input} (RunScriptInput) or {command}",
                "properties": {
                    "target": {"type": "string"},
                    "input": {"type": "string"},
                    "command": {"type": "string"}
                }
            },
            "anyLine": {"type": "boolean", "default": false},
            "source": {"type": "string"},
            "path": {"type": "string"},
            "timeoutMs": {"type": "integer", "minimum": 1, "maximum": 900000},
            "expect": {"type": "array", "minItems": 1, "maxItems": script::MAX_EXPECTS, "items": {"type": "string"}, "description": "regexes in order over whole lines (`MM/DD HH:MM:SS [cs_script] text`)"}
        },
        "required": ["kind", "expect"]
    })
}

/// `script_run`: validate, then run a cs_script addon, or refuse. The validation errors are
/// `InvalidArguments`; the refusals are `NotImplementedYet` (nothing dispatched).
pub async fn script_run(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let run = RunArgs::parse(&args).map_err(ToolError::invalid_arguments)?;
    match (run.kind, &run.addon) {
        (ScriptKind::CsScript, Some(_)) if run.source.is_none() && run.path.is_none() => {
            run_addon_scripts(&state, &ctx, &run).await
        }
        _ => Err(not_implemented(&run)),
    }
}

/// Launch (or reuse) the addon's session, check its map, fire the entry, and judge the console
/// output the expects match. The session is left running.
async fn run_addon_scripts(
    state: &ClientState,
    ctx: &CallCtx,
    run: &RunArgs,
) -> Result<ToolOutput, ToolError> {
    let addon = run.addon.clone().unwrap_or_default();
    let map = run.map_name().unwrap_or_default();
    let timeout_ms = run.timeout_ms.unwrap_or(script::DEFAULT_RUN_TIMEOUT_MS);
    let launched_at = SystemTime::now();
    // the timeout also bounds the readiness wait; an identical running session is reused
    let launch = launch_session(
        state.clone(),
        ctx.clone(),
        json!({"kind": "wine-tools", "addon": addon, "map": map, "timeoutMs": timeout_ms}),
    )
    .await?
    .json;
    let container = launch["container"].as_str().unwrap_or_default().to_string();
    if launch["pending"].as_bool() == Some(true) {
        // not ready inside the budget: nothing was fired, so the same call is safe to repeat
        return Ok(json!({
            "kind": "cs_script",
            "addon": addon,
            "map": map,
            "session": container,
            "pending": true,
            "passed": null,
            "reason": "the session was not ready within timeoutMs",
            "sessionLeftRunning": true,
            "next": "script_run again with the same arguments (nothing was fired; the session is reused)",
        })
        .into());
    }
    let console = launch["paths"]["consoleLog"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| {
            ToolError::internal("the session reports no console.log path")
                .with_data(json!({"session": container}))
        })?;
    let reused = launch["reused"].as_bool().unwrap_or(false);
    let log = ConsoleLog::new(&console);
    if ctx.budget.remaining() < BUDGET_MARGIN {
        return Err(ToolError::budget_exceeded(&ctx.budget)
            .retry_safe(true)
            .with_data(json!({"session": container, "commandsSent": false})));
    }
    let end = match log.cursor() {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Err(console_error(ConsoleError::Missing, &console)
                .retry_safe(true)
                .with_data(json!({"session": container})))
        }
        Err(e) => return Err(console_error(ConsoleError::Io(e), &console)),
    };
    // console.log is rotated at a fresh launch, so a log last written before this launch began
    // means the rotation did not happen and its output is not this session's. One second of slack
    // covers the filesystem's coarse timestamps.
    if !reused {
        if let Some(modified) = std::fs::metadata(&console)
            .and_then(|m| m.modified())
            .ok()
            .filter(|m| *m + Duration::from_secs(1) < launched_at)
        {
            let ms = |t: SystemTime| {
                t.duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64)
            };
            return Err(ToolError::new(
                "StaleConsoleLog",
                "StaleConsoleLog",
                format!(
                    "{} was last written before this launch started, so it is not this session's output (the rotation at launch did not happen); nothing was fired",
                    console.display()
                ),
            )
            .retry_safe(false)
            .with_data(json!({
                "session": container,
                "consoleLog": console.display().to_string(),
                "modifiedMs": ms(modified),
                "launchedMs": ms(launched_at),
                "commandsSent": false,
                "next": format!("stop_session {container}"),
            })));
        }
    }
    let mut map_verified = launch["mapVerified"].as_bool() == Some(true);
    let mut map_note = launch["mapNote"].clone();
    if !map_verified {
        match confirm_map(state, &container, &map).await? {
            None => map_verified = true,
            Some(note) => map_note = json!(note),
        }
    }
    // the segment: after the entry's cursor when there is an entry; otherwise from offset 0 of this
    // log, which is the session's load (rotated at launch; it includes earlier entries' output)
    let from = match run.entry {
        Some(_) => end.clone(),
        None => Cursor {
            offset: 0,
            ..end.clone()
        },
    };
    let segment = if run.entry.is_some() {
        "entry"
    } else {
        "session-start"
    };
    let run_started = Instant::now();
    let mut fired = false;
    let mut entry_report = Value::Null;
    let mut refused: Option<String> = None;
    if let Some(entry) = &run.entry {
        let command = entry.command();
        let mut bridge = state.bridge.lock().await;
        if !bridge.has("console.exec") {
            return Err(ToolError::not_implemented(
                "CapabilityMissing",
                "the bridge does not advertise console.exec (no command was sent)",
                json!({"missing": ["console.exec"], "session": container}),
            ));
        }
        if let Err(why) = bridge_session_gate(state, &container, &bridge).await {
            return Err(ToolError::new(
                "SessionNotBridged",
                "SessionNotBridged",
                format!("the entry cannot be fired: {why} (nothing was sent)"),
            )
            .retry_safe(true)
            .with_data(json!({"session": container, "commandsSent": false})));
        }
        let sent = exec_console_confirmed(
            &mut bridge,
            ctx,
            &console,
            &end,
            &[command.as_str()],
            Duration::from_millis(timeout_ms),
        )
        .await?;
        fired = true;
        let warnings: Vec<Value> = sent["warnings"].as_array().cloned().unwrap_or_default();
        // the engine's `Unknown command: <word>` for the entry: nothing will run, so fail now
        let word = command.split_whitespace().next().unwrap_or_default();
        let unknown = format!("Unknown command: {word}");
        refused = warnings
            .iter()
            .filter_map(Value::as_str)
            .find(|w| w.contains(&unknown))
            .map(|w| format!("the engine did not know the entry command: {w}"));
        entry_report = json!({
            "command": command,
            "confirmed": sent["confirmed"],
            "confirmMs": sent["confirmMs"],
            "clampedToBudget": sent["clampedToBudget"],
            "warnings": warnings,
        });
    }
    let mut timed_out = false;
    let mut clamped = false;
    if refused.is_none() {
        // the expects get what is left of timeoutMs, and never more than the call budget allows
        let room = ctx.budget.remaining().saturating_sub(BUDGET_MARGIN);
        let want = Duration::from_millis(timeout_ms).saturating_sub(run_started.elapsed());
        clamped = want > room;
        let matcher = script::ordered_matcher(&run.expects, run.any_line);
        timed_out = match log
            .wait_for_by(&matcher, want.min(room), WaitFrom::At(from.clone()))
            .await
        {
            Ok(_) => false,
            Err(WaitError::Timeout { .. }) => true,
            Err(WaitError::Missing) => {
                return Err(io_failure(
                    ConsoleError::Missing,
                    &console,
                    &container,
                    fired,
                ))
            }
            Err(WaitError::Io(e)) => {
                return Err(io_failure(ConsoleError::Io(e), &console, &container, fired))
            }
        };
    }
    let lines = read_segment(&log, &from)
        .await
        .map_err(|e| io_failure(ConsoleError::Io(e), &console, &container, fired))?;
    let texts: Vec<&str> = lines.iter().map(String::as_str).collect();
    let run_text: Vec<&str> = texts
        .iter()
        .copied()
        .filter(|l| script::is_run_line(l, run.any_line))
        .collect();
    let verdict = script::judge(&run_text.join("\n"), &run.expects, &run.expect);
    let (scripted, scripted_dropped) = script::cap_script_lines(&script::script_lines(&texts));
    let matched: Vec<Value> = verdict
        .matched
        .iter()
        .zip(&verdict.matched_expects)
        .map(|(line, &k)| json!({"regex": run.expect[k], "line": line}))
        .collect();
    // a wait the budget cut short has not judged the expects: pending, not failed
    let pending = refused.is_none()
        && timed_out
        && clamped
        && !verdict.unmatched.is_empty()
        && verdict.errors.is_empty();
    let (passed, reason): (Value, Option<String>) = if let Some(r) = refused.clone() {
        (json!(false), Some(r))
    } else if pending {
        (
            Value::Null,
            Some("the call budget ended the wait before timeoutMs".into()),
        )
    } else if !verdict.errors.is_empty() {
        (json!(false), Some("a script error line appeared".into()))
    } else if timed_out {
        (
            json!(false),
            Some("timeoutMs expired before every expect matched".into()),
        )
    } else if !verdict.unmatched.is_empty() {
        (
            json!(false),
            Some("an expect did not match in order".into()),
        )
    } else {
        (json!(true), None)
    };
    let mut out = json!({
        "kind": "cs_script",
        "addon": addon,
        "map": map,
        "session": container,
        "reused": reused,
        "mapVerified": map_verified,
        "mapNote": map_note,
        "sessionLeftRunning": true,
        "consoleLog": console.display().to_string(),
        "segmentFrom": segment,
        "entry": entry_report,
        "matched": matched,
        "unmatched": verdict.unmatched,
        "scriptLines": scripted,
        "scriptLinesTruncated": scripted_dropped,
        "errors": verdict.errors,
        "passed": passed,
        "reason": reason,
        "pending": pending,
        "timedOut": timed_out,
        "waitedMs": run_started.elapsed().as_millis() as u64,
        "clampedToBudget": clamped,
        "logExcerpt": verdict.log_excerpt,
    });
    if pending {
        out["consoleCursor"] = json!(end);
        out["next"] = if fired {
            json!(format!(
                "console_wait {{since: consoleCursor, regex: {:?}}} (the entry already fired: do not repeat it)",
                verdict.unmatched.first().cloned().unwrap_or_default()
            ))
        } else {
            json!("script_run again with the same arguments (nothing was fired; the session is reused)")
        };
    }
    Ok(out.into())
}

/// Whether `game.local` says the requested map is loaded and the player is in game. `Ok(None)`:
/// confirmed. `Ok(Some(note))`: the bridge could not be asked (the run goes on, unverified). A
/// different map, or no player in game, is `MapLoadFailed`: the run did not start on that map.
async fn confirm_map(
    state: &ClientState,
    container: &str,
    map: &str,
) -> Result<Option<String>, ToolError> {
    let mut bridge = state.bridge.lock().await;
    if !bridge.has("game.local") {
        return Ok(Some("the bridge does not offer game.local".into()));
    }
    if let Err(why) = bridge_session_gate(state, container, &bridge).await {
        return Ok(Some(format!("game.local not checked: {why}")));
    }
    match bridge
        .request(
            "game.local",
            json!({}),
            RequestOpts::deadline(MAP_REQUEST_MS),
        )
        .await
    {
        Ok(r) => {
            let in_game = r.result.get("inGame").and_then(Value::as_bool) == Some(true);
            let seen = r
                .result
                .get("map")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if in_game && seen.eq_ignore_ascii_case(map) {
                Ok(None)
            } else {
                Err(map_mismatch(
                    container,
                    map,
                    json!({"inGame": in_game, "map": seen}),
                ))
            }
        }
        Err(e) if matches!(e.kind, Kind::NoLocalPlayer) => Err(map_mismatch(
            container,
            map,
            json!({"error": e.kind.name()}),
        )),
        Err(e) => Ok(Some(format!("game.local did not answer: {}", e.message))),
    }
}

fn map_mismatch(container: &str, map: &str, seen: Value) -> ToolError {
    ToolError::new(
        "MapLoadFailed",
        "MapLoadFailed",
        format!(
            "game.local does not show map {map:?} in game; the session {container} is left running (stop_session {container})"
        ),
    )
    .retry_safe(false)
    .with_data(json!({
        "session": container,
        "map": map,
        "seen": seen,
        "commandsSent": false,
        "next": format!("stop_session {container}"),
    }))
}

/// A console.log read or wait failed. Retry-safe only while nothing was fired.
fn io_failure(e: ConsoleError, console: &Path, container: &str, fired: bool) -> ToolError {
    console_error(e, console)
        .retry_safe(!fired)
        .with_data(json!({
            "session": container,
            "consoleLog": console.display().to_string(),
            "commandsSent": fired,
        }))
}

/// Every complete line from `from` to the end of the log now, read in bounded steps.
async fn read_segment(log: &ConsoleLog, from: &Cursor) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut at = from.clone();
    while let Some((mut got, next)) = log.read_new_lines(Some(&at)).await? {
        let moved = next.inode != at.inode || next.offset != at.offset;
        lines.append(&mut got);
        if !moved {
            break;
        }
        at = next;
    }
    Ok(lines)
}

fn not_implemented(run: &RunArgs) -> ToolError {
    let b = script::blocker(run.kind);
    let kind = run.kind.as_str();
    let message = format!(
        "script_run {kind} is blocked: {}. Nothing was staged, launched or sent.",
        b.missing
    );
    ToolError::not_implemented(
        "NotImplementedYet",
        message,
        json!({
            "kind": kind,
            "blockedOn": script::BLOCKED_ON,
            "missing": b.missing,
            "evidence": b.evidence,
            "unblock": b.unblock,
            "staged": false,
            "launched": false,
            "sent": false,
            "requested": {
                "addon": run.addon,
                "map": run.map,
                "entry": run.entry.as_ref().map(script::EntryFire::command),
                "anyLine": run.any_line,
                "timeoutMs": run.timeout_ms,
                "expect": run.expect,
                "hasSource": run.source.is_some(),
                "path": run.path,
            },
        }),
    )
}
