//! `session_status` (never raises) and `console_exec` (guarded, optionally confirmed).
use crate::console::resolve::{choose, describe, resolve_within, session_arg, Purpose};
use crate::console::{ConsoleError, ConsoleLog, Cursor, WaitError, WaitFrom};
use crate::state::{ClientState, Flavour};
use crate::tools::client::TOOL_GROUPS;
use crate::tools::console::console_error;
use cs2_mcp_common::bridge::{BridgeClient, RequestOpts};
use cs2_mcp_common::capture::vklayer::layer_status;
use cs2_mcp_common::config::RuntimeKind;
use cs2_mcp_common::runtime::docker::SessionEntry;
use cs2_mcp_common::tools::{CallCtx, ToolError, ToolOutput};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::time::Duration;

/// What podman told us about the `cs2mcp-*` containers.
enum Podman {
    /// the local runtime: no containers are managed
    Local,
    /// a Windows host: the game runs natively, started by `cs2mcp-native up`; no podman here
    Native,
    Up(Vec<SessionEntry>),
    /// podman could not be used; the message says why
    Down(String),
}

fn session_json(e: &SessionEntry) -> Value {
    json!({
        "container": e.name,
        "kind": e.kind,
        "state": e.state,
        "owned": e.owned,
        "artifactsDir": e.artifacts_dir.as_deref().map(display),
        "consoleLog": e.console_log.as_deref().map(display),
        "map": e.map,
        "addon": e.addon,
        "note": e.note,
    })
}

fn display(p: &Path) -> String {
    p.display().to_string()
}

/// Why the podman-backed groups are off on a Windows host.
const NATIVE_NO_PODMAN: &str = "Windows host: sessions are started and stopped with `cs2mcp-native up` / `down` (crates/cs2-native); launch_session, run_testscript and server_up need the Linux podman runtime";

pub async fn session_status(
    state: ClientState,
    _ctx: CallCtx,
    _args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut out = Map::new();
    out.insert("server".into(), json!({"name": crate::NAME, "version": crate::VERSION, "startedAt": state.started_at, "expectedBuild": state.expect_build}));
    // one podman listing feeds the sessions array, the session group and the console.log group
    let podman = if cfg!(windows) {
        Podman::Native
    } else if state.runtime_kind == RuntimeKind::Local {
        Podman::Local
    } else {
        match state.runtime.list_sessions().await {
            Ok(list) => Podman::Up(list),
            Err(e) => Podman::Down(format!("{e:#}")),
        }
    };
    let sessions: &[SessionEntry] = match &podman {
        Podman::Up(list) => list,
        _ => &[],
    };
    let layer = layer_status(&state.vk_layer);
    // what the console tools would pick with no `session` argument (the same rules, same list)
    let pick = choose(
        state.console_log.as_deref(),
        &state.default_console_log,
        None,
        sessions,
    );
    let console_exists = pick.as_ref().is_ok_and(|r| r.path.exists());
    let console_pick = match &pick {
        Ok(r) => json!({
            "consoleLog": display(&r.path),
            "consoleLogSource": r.source,
            "consoleLogSession": r.session,
            "consoleLogExists": console_exists,
        }),
        Err(e) => json!({
            "consoleLog": null,
            "consoleLogSource": "ambiguous",
            "consoleLogNote": e.message,
            "consoleLogExists": false,
        }),
    };
    let mut runtime = json!({
        "backend": if matches!(podman, Podman::Native) { "native" } else { "docker" },
        "backendState": if matches!(podman, Podman::Native) {
            "attach-only: the session is cs2mcp-native's (`cs2mcp-native up` / `down`, crates/cs2-native)"
        } else {
            "launch_session/stop_session via podman (workflows/01-docker-runtime.md); attach-only otherwise"
        },
        "podman": state.runtime.podman_display(),
        "maxSessions": state.runtime.max_sessions,
        "configuredFlavour": state.flavour,
        // launches mount the capture layer only when both files are present here
        "vkLayer": layer.to_json(),
        "captureDir": state.capture_dir.as_ref().map(|d| d.display().to_string()),
        "vkLayerNote": "build with client-mcp/docker/build-vk-layer.sh; launch_session capture: false opts a session out",
    });
    if let (Value::Object(r), Value::Object(p)) = (&mut runtime, console_pick) {
        r.extend(p);
    }
    out.insert("runtime".into(), runtime);
    let mut bridge = state.bridge.lock().await;
    let section = cs2_mcp_common::status::collect(&mut bridge, &state.launch_command).await;
    let connected = section.connected;
    out.extend(section.fields);
    let flavour = match (state.flavour, connected) {
        (Flavour::Auto, true) if cfg!(windows) => json!(Flavour::NativeWindows),
        (Flavour::Auto, true) => json!("wine-tools"),
        (Flavour::Auto, false) => json!("unknown"),
        (f, _) => json!(f),
    };
    out["runtime"]["flavour"] = flavour;
    let podman_down = match &podman {
        Podman::Down(msg) => format!(
            "podman not usable ({msg}); launch_session needs it (check --podman / CS2_MCP_PODMAN)"
        ),
        _ => String::new(),
    };
    let mut groups = Map::new();
    for (group, tools, methods) in TOOL_GROUPS {
        let (available, reason) = if methods.is_empty() {
            match *group {
                "console_log" => {
                    let session_log = sessions
                        .iter()
                        .any(|e| e.console_log.as_deref().is_some_and(Path::exists));
                    (
                        console_exists || session_log,
                        if console_exists {
                            "console.log present"
                        } else if session_log {
                            "a session's console.log is present (console_tail/console_wait pick it)"
                        } else {
                            "console.log not found (no session, or set --console-log)"
                        },
                    )
                }
                "multiplayer" => match &podman {
                    Podman::Native => (false, NATIVE_NO_PODMAN),
                    Podman::Local => (
                        false,
                        "local runtime not implemented (workflows/10-local-install.md)",
                    ),
                    Podman::Up(_) => (
                        true,
                        "podman reachable: server_up and server_console_exec usable; client_connect needs a wine-tools session (bridge) or a launch with connect",
                    ),
                    Podman::Down(_) => (false, podman_down.as_str()),
                },
                "session" => match &podman {
                    Podman::Native => (false, NATIVE_NO_PODMAN),
                    Podman::Local => (
                        false,
                        "local runtime not implemented (workflows/10-local-install.md)",
                    ),
                    Podman::Up(_) => (
                        true,
                        "podman reachable: launch_session, stop_session and run_testscript usable",
                    ),
                    Podman::Down(_) => (false, podman_down.as_str()),
                },
                "game_test" => (
                    true,
                    "runs in-process: each step is gated by the bridge methods of its own tool (a missing method fails only that step)",
                ),
                "game_test_results" => (
                    true,
                    "reads this process's stored game_test results (no bridge)",
                ),
                "capture" if state.capture_dir.is_some() => (
                    true,
                    "native session (--capture-dir): screenshot/observe read the frames the layer cs2mcp-native up loads into the game writes there",
                ),
                "capture" => (
                    layer.valid,
                    if layer.valid {
                        "capture layer present: screenshot/observe read sessions launched with capture"
                    } else {
                        "capture layer missing (CS2_MCP_VK_LAYER_DIR lacks libcs2_vk_capture.so or its manifest; build-vk-layer.sh)"
                    },
                ),
                _ => (
                    false,
                    "no backend wired for this group yet",
                ),
            }
        } else if !connected {
            (
                false,
                "bridge not reachable (WineTools session with cs2_bridge loaded required)",
            )
        } else {
            let missing = bridge.missing(methods);
            if missing.is_empty() {
                (true, "bridge advertises the required methods")
            } else {
                groups.insert(group.to_string(), json!({"available": false, "tools": tools, "reason": format!("bridge does not advertise {}", missing.join(", ")), "missing": missing}));
                continue;
            }
        };
        groups.insert(
            group.to_string(),
            json!({"available": available, "tools": tools, "reason": reason}),
        );
    }
    drop(bridge);
    out.insert("toolGroups".into(), Value::Object(groups));
    let session_list: Vec<Value> = sessions.iter().map(session_json).collect();
    out.insert("sessions".into(), Value::Array(session_list));
    if let Podman::Down(msg) = &podman {
        out.insert("sessionsNote".into(), json!(msg));
    }
    out.insert(
        "lockstep".into(),
        json!("docker runtime: at most --max-sessions cs2* containers and one session per kind per shared mount; one bridge client at a time"),
    );
    Ok(Value::Object(out).into())
}

/// The console refusal guard: a forbidden command is `ForbiddenCommand` (retry-safe, nothing sent).
pub(crate) fn check_console(command: &str) -> Result<(), ToolError> {
    match cs2_mcp_common::console::refusal(command) {
        Some(reason) => Err(ToolError::new(
            "ForbiddenCommand",
            "ForbiddenCommand",
            format!("refusing {command:?}: {reason}"),
        )
        .retry_safe(true)
        .with_data(json!({"command": command}))),
        // the engine silently drops any single command over 510 bytes (or a name over 128)
        None => match cs2_mcp_common::console::engine_limit_violation(command) {
            Some(why) => Err(ToolError::invalid_arguments(format!(
                "{why}. Nothing was sent: shorten or split that command"
            ))),
            None => Ok(()),
        },
    }
}

/// Queue one console command over the bridge (`console.exec`), refused first by `check_console`,
/// under the call's budget. The reply has `route` and `command`; callers add their own fields.
/// The caller holds the bridge lock, so an in-process wait (setpos) can follow under the same lock.
pub(crate) async fn exec_console(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    command: &str,
) -> Result<Value, ToolError> {
    check_console(command)?;
    let deadline = ctx.budget.deadline_ms()?;
    let resp = bridge
        .request(
            "console.exec",
            json!({"command": command}),
            RequestOpts::deadline(deadline),
        )
        .await?;
    let mut result = resp.result;
    result["route"] = json!("bridge");
    result["command"] = json!(command);
    Ok(result)
}

/// Default and cap for `console_exec {confirm: true}` waiting for the marker line.
const CONFIRM_DEFAULT_MS: u64 = 5000;
/// Kept back from the call budget so the reply still goes out in time (as in console_wait).
const CONFIRM_MARGIN: Duration = Duration::from_millis(500);
const CONFIRM_TAIL_LINES: usize = 10;

/// A marker command name no engine command has, unique per call (a stale marker line never matches).
fn confirm_marker() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "cs2mcp_mark_{:x}{:x}",
        nanos,
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Queue `commands` with `exec_console`, then a marker command, and wait for console.log to show
/// `Unknown command: <marker>`. The engine's command buffer is FIFO and the warning is printed when
/// the marker runs, so every earlier command has run by then. Bridge-queued command output (echo,
/// status) never reaches console.log; the warning does. `log` and `cursor` must be taken before this
/// call, so the marker line cannot predate it. Nothing is sent when the budget is already spent.
/// A timeout, or a log problem after sending, is not retry-safe: the commands were sent.
pub(crate) async fn exec_console_confirmed(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    log: &Path,
    cursor: &Cursor,
    commands: &[&str],
    timeout: Duration,
) -> Result<Value, ToolError> {
    for command in commands {
        check_console(command)?;
    }
    if ctx.budget.remaining() < CONFIRM_MARGIN {
        return Err(
            ToolError::budget_exceeded(&ctx.budget).with_data(json!({"commandsSent": false}))
        );
    }
    let marker = confirm_marker();
    let mut replies = Vec::with_capacity(commands.len() + 1);
    for command in commands
        .iter()
        .copied()
        .chain(std::iter::once(marker.as_str()))
    {
        match exec_console(bridge, ctx, command).await {
            Ok(reply) => replies.push(reply),
            // nothing went out yet when the first command fails; a later failure leaves the earlier ones queued
            // unless the request timed out: then the bridge may have queued it (unknown, null)
            Err(e) if replies.is_empty() => {
                let sent = if e.outcome_unknown {
                    Value::Null
                } else {
                    json!(false)
                };
                return Err(e.with_data(json!({"commandsSent": sent})));
            }
            Err(e) => {
                return Err(e.retry_safe(false).with_data(json!({"commandsSent": true})));
            }
        }
    }
    let cap = ctx.budget.remaining().saturating_sub(CONFIRM_MARGIN);
    let wait = timeout.min(cap);
    let clamped = timeout > cap;
    let pattern = Regex::new(&format!(r"Unknown command: {}\s*$", regex::escape(&marker)))
        .map_err(|e| ToolError::internal(format!("marker pattern: {e}")))?;
    match ConsoleLog::new(log)
        .wait_for(&pattern, wait, WaitFrom::At(cursor.clone()))
        .await
    {
        Ok(m) => {
            // lines between the cursor and the marker that the engine refused: a mistyped command
            // shows up here even though the marker itself was processed
            let warnings = match ConsoleLog::new(log).read_new_lines(Some(cursor)).await {
                Ok(Some((lines, _))) => lines
                    .iter()
                    .take_while(|l| !l.contains(&marker))
                    .filter(|l| l.contains("Unknown command:"))
                    .cloned()
                    .collect(),
                _ => Vec::new(),
            };
            Ok(json!({
                "commands": commands,
                "marker": marker,
                "confirmed": true,
                "confirmMs": m.waited.as_millis() as u64,
                "clampedToBudget": clamped,
                "warnings": warnings,
                "replies": replies,
                "route": "bridge",
            }))
        }
        // the commands were sent already: report the log problem, do not suggest a retry
        Err(WaitError::Missing) => Err(console_error(ConsoleError::Missing, log)
            .retry_safe(false)
            .with_data(json!({"commandsSent": true}))),
        Err(WaitError::Io(e)) => Err(console_error(ConsoleError::Io(e), log)
            .retry_safe(false)
            .with_data(json!({"commandsSent": true}))),
        Err(WaitError::Timeout {
            waited,
            file_exists,
        }) => {
            let last_lines = match ConsoleLog::new(log).tail(CONFIRM_TAIL_LINES, None).await {
                Ok(t) => t.lines,
                Err(_) => Vec::new(),
            };
            Err(ToolError::new(
                "WaitTimeout",
                "WaitTimeout",
                format!(
                    "no 'Unknown command: {marker}' line within {} ms: the commands were sent but are not confirmed",
                    waited.as_millis()
                ),
            )
            .retry_safe(false)
            .with_data(json!({
                "commands": commands,
                "marker": marker,
                "path": display(log),
                "fileExists": file_exists,
                "waitedMs": waited.as_millis() as u64,
                "timeoutMs": wait.as_millis() as u64,
                "clampedToBudget": clamped,
                "commandsSent": true,
                "lastLines": last_lines,
            })))
        }
    }
}

/// `confirm` (default false) and `confirmTimeoutMs` (>= 1, only with confirm) of console_exec:
/// `None` when no confirmation was asked for. `confirmTimeoutMs` is validated only when present.
fn confirm_args(args: &Value) -> Result<Option<Duration>, ToolError> {
    let confirm = match args.get("confirm") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| ToolError::invalid_arguments("confirm must be a boolean"))?,
    };
    let timeout = match args.get("confirmTimeoutMs") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().filter(|n| *n >= 1).ok_or_else(|| {
            ToolError::invalid_arguments("confirmTimeoutMs must be an integer >= 1")
        })?),
    };
    match (confirm, timeout) {
        (false, Some(_)) => Err(ToolError::invalid_arguments(
            "confirmTimeoutMs needs confirm: true",
        )),
        (false, None) => Ok(None),
        (true, ms) => Ok(Some(Duration::from_millis(
            ms.unwrap_or(CONFIRM_DEFAULT_MS),
        ))),
    }
}

pub async fn console_exec(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| ToolError::invalid_arguments("command must be a non-empty string"))?;
    // refused before the session is resolved, so a forbidden command reports as such
    check_console(command)?;
    let confirm = confirm_args(&args)?;
    // Cursor taken before queueing, so `console_wait {since}` cannot miss the command's output.
    // An ambiguous session pick (several running, no `session`) does not block the command: the
    // cursor is just left out. A named session that does not exist is an error.
    let session = session_arg(&args)?;
    let resolved = resolve_within(&state, &ctx, session.as_deref(), Purpose::Bridge).await;
    if let Some(timeout) = confirm {
        // confirmation needs the log before anything is queued: no log, no command
        let r = resolved?;
        let cursor = match ConsoleLog::new(&r.path).cursor() {
            Ok(Some(c)) => c,
            Ok(None) => return Err(console_error(ConsoleError::Missing, &r.path).retry_safe(true)),
            Err(e) => return Err(console_error(ConsoleError::Io(e), &r.path)),
        };
        let mut bridge = state.bridge.lock().await;
        let mut result =
            exec_console_confirmed(&mut bridge, &ctx, &r.path, &cursor, &[command], timeout)
                .await?;
        result["command"] = json!(command);
        result["consoleCursor"] = json!(cursor);
        if let Value::Object(fields) = describe(json!({"consoleLog": display(&r.path)}), &r) {
            for (k, v) in fields {
                result[k.as_str()] = v;
            }
        }
        return Ok(result.into());
    }
    let (cursor, source) = match resolved {
        Ok(r) => (
            ConsoleLog::new(&r.path).cursor().ok().flatten(),
            describe(json!({"consoleLog": display(&r.path)}), &r),
        ),
        Err(e) if session.is_none() => (None, json!({"sourceNote": e.message})),
        Err(e) => return Err(e),
    };
    let mut bridge = state.bridge.lock().await;
    let mut result = exec_console(&mut bridge, &ctx, command).await?;
    result["consoleCursor"] = json!(cursor);
    if let Value::Object(fields) = source {
        for (k, v) in fields {
            result[k.as_str()] = v;
        }
    }
    result["note"] = json!(
        "queued to the engine command buffer; asynchronous - confirm with console_wait {since: consoleCursor}"
    );
    Ok(result.into())
}
