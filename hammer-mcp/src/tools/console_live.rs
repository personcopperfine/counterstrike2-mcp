//! `console_exec` (a guarded console line with its output read back) and `cvar_get` (the bridge's
//! `cvar.get`), for the engine of the tools session.
//!
//! `console_exec`:
//!
//! * the refusal guard comes first ([`cs2_mcp_common::console::refusal`], the same one client-mcp
//!   applies, plus the map-changing commands, which are `run_map`'s): a refused line is
//!   `ConsoleCommandRefused` and **nothing is sent**;
//! * everything after the guard happens under the server-wide `vtm_gate` (the one `run_map`
//!   takes): the "is a load in flight?" decision (`settle_unresolved_previous`, which re-reads
//!   `last_load`, `game.local` and the console window) and the sends cannot interleave with a
//!   `run_map`'s posts;
//! * the route is the bridge's `console.exec` when it is advertised and the session allows it (a
//!   `-tools` or `-testscript` command line); a bridge that answers `NotImplemented` (the gate) or
//!   does not advertise the method falls back to the VToolMessage command channel (`run_map`'s
//!   `post_command`). The result says which one was used (`route`);
//! * a marker command (`cs2mcp_mark_<unique>`, the engine answers `Unknown command: <marker>`) is
//!   sent right after the line, the way client-mcp confirms its commands: the engine runs its
//!   command buffer in order, so the marker line ends the output;
//! * the output is what `game/csgo/console.log` gained **after** the call started: whole lines
//!   only (an unterminated last line is never matched or returned), UTF-8 decoded across polls
//!   (the file's length is noted before the send; a file that shrank or was replaced restarts at 0
//!   and says so in `console.rotated`);
//! * the wait is polled against a deadline (`waitMs`, never a fixed sleep) and ends (`endedBy`) at
//!   a `waitFor` match, at the marker, once output arrived and went quiet while no marker could
//!   be seen, at the `waitMs` bound, or at once when there is no console.log (`noFile`).
//!
//! The idle level (no map yet) runs console commands safely (`run_map` posts there too): they are
//! allowed with a note. A level that is still loading is `EngineBusy`.
use super::live_common::{invalid, reject_unknown_keys};
use super::run_map::{
    console_log_path, failure_regexes, idle_level_regex, post_command, read_game_local,
    settle_unresolved_previous, success_regex, tools_container, ConsoleWatch, GameLocal, SETTLE_STALE_MS,
};
use crate::state::HammerState;
use cs2_mcp_common::bridge::{Kind, RequestOpts};
use cs2_mcp_common::console::{engine_limit_violation, refusal_hinted, CVAR_NAMES_MAX, MAP_CHANGING_COMMANDS};
use cs2_mcp_common::tools::{
    CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec,
};
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub const WAIT_DEFAULT_MS: u64 = 1500;
pub const WAIT_MAX_MS: u64 = 10_000;
pub const MAX_LINES_DEFAULT: usize = 200;
pub const MAX_LINES_MAX: usize = 2000;
/// How often the console.log is re-read while waiting.
const POLL: Duration = Duration::from_millis(50);
/// No marker could be seen: the output counts as complete once it has been silent this long.
const QUIET: Duration = Duration::from_millis(500);
/// The bridge copies the whole line into a 1024-byte buffer, so no line over this is sent on any
/// route. This is the WHOLE-LINE cap; the engine's own limit is tighter and per `;`-separated
/// command (510 bytes, 128 for a command name: `cs2_mcp_common::console::engine_limit_violation`).
pub const BRIDGE_COMMAND_MAX: usize = 1023;
/// How much of console.log's end is read to find the engine's last level line.
const LEVEL_TAIL_BYTES: u64 = 256 * 1024;

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "console_exec",
            "Run one console line in the tools session's engine; returns the console.log lines it produced (the last maxLines). Works at the idle level and in a running map; EngineBusy while a level loads. \
             Not for: loading a map (run_map; map/changelevel/connect are refused with useInstead), reading cvars (cvar_get). Session-ending or crashing lines (quit, exit, disconnect, ...) are ConsoleCommandRefused; nothing is sent. \
             In a -tools session `echo` and cvar prints are not logged, so waitFor cannot see them. Limits: line 1023 bytes, `;`-chained command 510, command name 128. \
             The wait ends (`endedBy`) at a waitFor regex match, else the end-of-output marker, else waitMs.",
            Mutating,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "minLength": 1,
                            "description": "one line; `;` chains commands"},
                "waitMs": {"type": "integer", "minimum": 0, "maximum": WAIT_MAX_MS,
                           "description": "default 1500"},
                "waitFor": {"type": "string",
                            "description": "regex; stop at the first matching output line"},
                "maxLines": {"type": "integer", "minimum": 1, "maximum": MAX_LINES_MAX,
                             "description": "default 200; the LAST ones kept (truncated=true)"}
            },
            "required": ["command"],
            "additionalProperties": false
        })),
        console_exec,
    );
    r.register(
        ToolSpec::new(
            "cvar_get",
            "Read console variables: per name {found, value (text), type, default, flags}. devonly cvars read as not found; protected/secret ones (*password*, *token*) are redacted:true. Not for: setting (console_exec `<name> <value>`).",
            ReadOnly,
            &["cvar.get"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "names": {"type": "array", "minItems": 1, "maxItems": CVAR_NAMES_MAX,
                          "items": {"type": "string", "minLength": 1, "maxLength": 128},
                          "description": "cvar names"}
            },
            "required": ["names"],
            "additionalProperties": false
        })),
        cvar_get,
    );
}

/// The validated `console_exec` arguments.
#[derive(Debug)]
pub struct ConsoleArgs {
    pub command: String,
    pub wait_ms: u64,
    pub wait_for: Option<Regex>,
    pub max_lines: usize,
}

impl ConsoleArgs {
    pub fn parse(args: &Value) -> Result<ConsoleArgs, ToolError> {
        reject_unknown_keys(args, &["command", "waitMs", "waitFor", "maxLines"])?;
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("command (string) is required"))?
            .to_string();
        if command.chars().any(|c| c == '\0' || c == '\n' || c == '\r') {
            return Err(invalid(
                "command must be a single line (no newline or NUL); chain commands with `;`",
            ));
        }
        let wait_ms = match args.get("waitMs") {
            None | Some(Value::Null) => WAIT_DEFAULT_MS,
            Some(v) => v
                .as_u64()
                .filter(|n| *n <= WAIT_MAX_MS)
                .ok_or_else(|| invalid(format!("waitMs must be an integer in 0..={WAIT_MAX_MS}")))?,
        };
        let max_lines = match args.get("maxLines") {
            None | Some(Value::Null) => MAX_LINES_DEFAULT,
            Some(v) => v
                .as_u64()
                .filter(|n| (1..=MAX_LINES_MAX as u64).contains(n))
                .ok_or_else(|| invalid(format!("maxLines must be an integer in 1..={MAX_LINES_MAX}")))?
                as usize,
        };
        let wait_for = match args.get("waitFor") {
            None | Some(Value::Null) => None,
            Some(Value::String(pattern)) => Some(
                RegexBuilder::new(pattern)
                    .size_limit(1 << 20)
                    .build()
                    .map_err(|e| invalid(format!("waitFor is not a valid regex: {e}")))?,
            ),
            Some(other) => return Err(invalid(format!("waitFor must be a string, got {other}"))),
        };
        Ok(ConsoleArgs {
            command,
            wait_ms,
            wait_for,
            max_lines,
        })
    }
}

/// `ConsoleCommandRefused`: the guard said no; nothing was sent. A map-changing command (the
/// hammer-only list) points at `run_map`.
fn refused(command: &str, reason: &str, map_change: bool) -> ToolError {
    let error = ToolError::new(
        "ConsoleCommandRefused",
        "ConsoleCommandRefused",
        format!("not sent: {reason}"),
    )
    .with_data(json!({"command": command, "reason": reason}))
    .retry_safe(true);
    if map_change {
        error.use_instead(
            "run_map",
            "loading, switching and unloading a map is run_map's job (it settles the previous \
             load, validates the name and reads the verdict)",
            None,
        )
    } else {
        error
    }
}

/// What the engine's own console says about the level, from the last level line of the tail.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LevelState {
    /// `Loading map "<name>"` is the last level line: a level load is running.
    Loading(String),
    /// `Spawn Server: <empty>`: the idle level.
    Idle,
    /// The last level line is a load success.
    Mounted,
    /// No level line in the tail (or a failed load).
    Unknown,
}

fn loading_line_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"Loading map ""#).expect("loading regex"))
}

/// The newest level line in `text` decides.
pub(crate) fn level_state_of(text: &str) -> LevelState {
    for line in text.lines().rev() {
        if loading_line_regex().is_match(line) {
            return LevelState::Loading(line.trim_end().to_string());
        }
        if idle_level_regex().is_match(line) {
            return LevelState::Idle;
        }
        if success_regex().is_match(line) {
            return LevelState::Mounted;
        }
        if failure_regexes().iter().any(|re| re.is_match(line)) {
            return LevelState::Unknown;
        }
    }
    LevelState::Unknown
}

/// [`level_state_of`] over the end of console.log (a missing or unreadable file is `Unknown`).
fn level_state(path: &Path) -> LevelState {
    let read = || -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(LEVEL_TAIL_BYTES)))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    read().map_or(LevelState::Unknown, |text| level_state_of(&text))
}

/// How long console.log has not been written (`None`: no file or no readable mtime).
pub(crate) fn console_idle_ms(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.elapsed().map_or(0, |d| d.as_millis() as u64))
}

/// `EngineBusy`: the console's last level line says a level is loading. Nothing was sent.
fn level_loading(line: &str) -> ToolError {
    ToolError::new(
        "EngineBusy",
        "EngineBusy",
        format!(
            "the engine is loading a level (the last level line in game/csgo/console.log is \
             {line:?}); a console command posted during a load is the documented crash trigger, \
             so nothing was sent. Retry when the load has finished (game_state, run_map's verdict); a load \
             that never ends is taken as stale once console.log has been silent for {} s, or reset \
             it with run_map {{unload: true}}.",
            SETTLE_STALE_MS / 1000
        ),
    )
    .with_data(json!({"levelLine": line}))
    .retry_safe(true)
}

/// A marker command no engine command has, unique per call.
fn new_marker() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("cs2mcp_mark_{:x}{:x}", nanos, SEQ.fetch_add(1, Ordering::Relaxed))
}

/// The engine's answer to the marker: `Unknown command: <marker>`.
fn marker_regex(marker: &str) -> Regex {
    Regex::new(&format!(r"Unknown command: {}\s*$", regex::escape(marker))).expect("marker regex")
}

/// The first line `re` matches.
pub fn first_match<'a>(lines: &'a [String], re: &Regex) -> Option<&'a String> {
    lines.iter().find(|line| re.is_match(line))
}

/// The engine's two "dropped" lines: `WARNING: Command too long... ignoring!` (a command over 510
/// bytes) and `Console command too long.` (a command name over 128 bytes).
fn is_too_long_warning(line: &str) -> bool {
    line.contains("Command too long") || line.contains("Console command too long")
}

/// The engine logs every VToolMessage it handles as a block:
/// `[ToolFramework2] Handling VToolMessage:`, `{`, `\t_message = "command"`, `\t_data = "<command>"`,
/// `}`. On the vtm route that block repeats the command text, so a `waitFor` that occurs in the
/// command would match its own echo. `true` marks the lines of such blocks.
///
/// The mask is bounded to the known shape: header, `{`, at most `ECHO_BLOCK_BODY_MAX` tab-indented
/// `key = value` lines, `}`. At the first line that does not fit (or past the body bound) the
/// masking stops, so a malformed or never-closed block cannot hide real output. After a
/// `Command too long... ignoring!` warning the engine prints an untimestamped copy of the dropped
/// command; that line is masked too.
pub fn echo_block_mask(lines: &[String]) -> Vec<bool> {
    const ECHO_BLOCK_BODY_MAX: usize = 4;
    let mut mask = vec![false; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        // only `Command too long... ignoring!` is followed by a copy of the dropped command;
        // `Console command too long.` (case-sensitive: no match) is not (LIVE-P1b 06c)
        if i > 0 && lines[i - 1].contains("Command too long") {
            mask[i] = true;
        }
        if !lines[i].trim_end().ends_with("[ToolFramework2] Handling VToolMessage:") {
            i += 1;
            continue;
        }
        mask[i] = true;
        // a header not followed by `{` is a bare header line
        if lines.get(i + 1).map(|l| l.trim()) != Some("{") {
            i += 1;
            continue;
        }
        mask[i + 1] = true;
        let mut j = i + 2;
        let mut body = 0;
        while j < lines.len() {
            if lines[j].trim() == "}" {
                mask[j] = true;
                j += 1;
                break;
            }
            if body == ECHO_BLOCK_BODY_MAX || !lines[j].starts_with('\t') {
                break;
            }
            mask[j] = true;
            body += 1;
            j += 1;
        }
        i = j;
    }
    mask
}

/// What the wait saw.
#[derive(Debug)]
pub(crate) struct Readback {
    /// The output lines (whole lines, cut at the marker).
    pub lines: Vec<String>,
    pub matched: Option<String>,
    /// `match`, `marker`, `quiet`, `timeout` or `noFile`.
    pub ended_by: &'static str,
    pub waited_ms: u64,
    pub rotated: bool,
    pub exists: bool,
    /// An unterminated last line was left out.
    pub partial_tail: bool,
    /// The end-of-output marker line arrived (always `false` when none was sent).
    pub marker_seen: bool,
}

/// Poll `watch` until `wait_for` matches a line, the `marker` line arrived (only without a
/// `wait_for`: with one, the marker does not end the wait and `wait_for` is matched over every
/// complete line, also those the engine prints after the command returned), (when the marker could
/// not be sent or never shows) the output went quiet, or `wait` passed. `wait` of zero reads once.
/// A console.log that does not exist at the first poll ends the wait (`noFile`).
/// `quiet_with_marker`: the quiet fallback also applies when a marker was sent (the vtm route,
/// which may not echo it).
pub(crate) async fn read_back(
    watch: &mut ConsoleWatch,
    wait: Duration,
    wait_for: Option<&Regex>,
    marker: Option<&Regex>,
    quiet_with_marker: bool,
) -> Readback {
    let start = Instant::now();
    let mut last_len = 0usize;
    let mut last_change = start;
    let mut first = true;
    loop {
        watch.poll();
        let (all, partial_tail) = watch.complete_lines();
        let marker_at = marker.and_then(|re| all.iter().position(|line| re.is_match(line)));
        // without waitFor the output ends at the marker; with one, every line but the marker's own
        let after_marker: Vec<String>;
        let body: &[String] = match (marker_at, wait_for.is_some()) {
            (Some(at), true) => {
                after_marker = all.iter().enumerate().filter(|(i, _)| *i != at).map(|(_, l)| l.clone()).collect();
                &after_marker
            }
            (Some(at), false) => &all[..at],
            (None, _) => &all,
        };
        let len = watch.seen().len();
        if len != last_len {
            last_len = len;
            last_change = Instant::now();
        }
        let mut matched = None;
        let mut ended_by = None;
        // waitFor never matches the engine's echo of a posted command (see `echo_block_mask`),
        // nor the marker's own line
        let echo = echo_block_mask(&all);
        let found = wait_for.and_then(|re| {
            all.iter()
                .enumerate()
                .find(|(i, line)| !echo[*i] && Some(*i) != marker_at && re.is_match(line))
                .map(|(_, line)| line)
        });
        if let Some(line) = found {
            matched = Some(line.clone());
            ended_by = Some("match");
        } else if marker_at.is_some() && wait_for.is_none() {
            ended_by = Some("marker");
        } else if first && !watch.exists() {
            ended_by = Some("noFile");
        } else if !body.is_empty() && last_change.elapsed() >= QUIET
            && (marker.is_none() || quiet_with_marker)
        {
            ended_by = Some("quiet");
        } else if start.elapsed() >= wait {
            ended_by = Some("timeout");
        }
        first = false;
        if let Some(ended_by) = ended_by {
            return Readback {
                lines: body.to_vec(),
                matched,
                ended_by,
                waited_ms: start.elapsed().as_millis() as u64,
                rotated: watch.rotated(),
                exists: watch.exists(),
                partial_tail,
                marker_seen: marker_at.is_some(),
            };
        }
        let remaining = wait.saturating_sub(start.elapsed());
        tokio::time::sleep(POLL.min(remaining.max(Duration::from_millis(1)))).await;
    }
}

/// The `console_exec` result shape.
pub(crate) fn result_json(route: &str, command: &str, back: &Readback, args: &ConsoleArgs, extra: Value) -> Value {
    let total = back.lines.len();
    let truncated = total > args.max_lines;
    let lines: Vec<&String> = back.lines.iter().skip(total - total.min(args.max_lines)).collect();
    let mut out = json!({
        "route": route,
        "command": command,
        "lines": lines,
        "lineCount": total,
        "truncated": truncated,
        "endedBy": back.ended_by,
        "markerSeen": back.marker_seen,
        "waitedMs": back.waited_ms,
        "console": {"exists": back.exists, "rotated": back.rotated, "partialTail": back.partial_tail},
    });
    if args.wait_for.is_some() {
        out["matched"] = json!(back.matched.is_some());
        out["matchedLine"] = back.matched.clone().map(Value::String).unwrap_or(Value::Null);
    }
    if let (Value::Object(target), Value::Object(more)) = (&mut out, extra) {
        target.extend(more);
    }
    out
}

/// Send one line through the bridge's `console.exec`. `Ok(false)` = the bridge was not used (not
/// reachable, not advertised, or its session gate refused; the reason is in `extra`/`notes`).
/// The caller holds `vtm_gate`.
async fn send_via_bridge(
    state: &HammerState,
    ctx: &CallCtx,
    line: &str,
    extra: &mut serde_json::Map<String, Value>,
    notes: &mut Vec<String>,
) -> Result<bool, ToolError> {
    let mut bridge = state.bridge.lock().await;
    if let Err(error) = bridge.ensure_connected().await {
        notes.push(format!(
            "the bridge could not be reached ({error}), so the line went over the VToolMessage channel"
        ));
        return Ok(false);
    }
    if !bridge.has("console.exec") {
        return Ok(false);
    }
    match bridge
        .request(
            "console.exec",
            json!({"command": line}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await
    {
        Ok(_) => Ok(true),
        Err(error) if error.kind == Kind::NotImplemented => {
            extra.insert(
                "bridgeRefused".into(),
                json!(format!("console.exec: {}", error.message)),
            );
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

/// Send one line over the VToolMessage channel (the caller holds `vtm_gate`).
async fn send_via_vtm(
    state: &HammerState,
    line: &str,
    extra: &mut serde_json::Map<String, Value>,
    key: &str,
) -> Result<(), ToolError> {
    let container = tools_container(state).await?;
    let post = post_command(state, &container, line).await?;
    if post["delivered"] == json!(false) {
        extra.insert(
            "deliveryWarning".into(),
            json!("the vtmsend exec was cut off: the line may or may not have reached the engine; it was not repeated"),
        );
    }
    extra.insert(key.into(), post);
    Ok(())
}

pub async fn console_exec(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let parsed = ConsoleArgs::parse(&args)?;
    // the engine's per-command limits first (they are the tighter ones and the usual cause), then
    // the bridge's whole-line buffer
    if let Some(why) = engine_limit_violation(&parsed.command) {
        return Err(invalid(format!("{why}. Nothing was sent: shorten or split that command")));
    }
    if parsed.command.len() > BRIDGE_COMMAND_MAX {
        return Err(invalid(format!(
            "the line is {} bytes; the bridge's line buffer takes at most {BRIDGE_COMMAND_MAX} \
             (a separate limit from the engine's 510 bytes per `;`-separated command, which every \
             command in it is within). Nothing was sent: split it into several console_exec calls",
            parsed.command.len()
        )));
    }
    if let Some(reason) = refusal_hinted(&parsed.command, &[], "") {
        return Err(refused(&parsed.command, &reason, false));
    }
    if let Some(reason) = refusal_hinted(&parsed.command, MAP_CHANGING_COMMANDS, "") {
        return Err(refused(&parsed.command, &reason, true));
    }

    // Everything from the load-in-flight decision to the last send is one critical section with
    // run_map's posts: its `last_load` record and its `game.local` read are re-read under the gate.
    let _gate = state.vtm_gate.clone().lock_owned().await;
    let console_path = console_log_path(&state);
    let mut notes: Vec<String> = Vec::new();
    let local: Option<GameLocal> = read_game_local(&state, ctx.budget.deadline_ms()?.min(2000)).await;
    // an unresolved load of this server: a finished one settles here (game.local, or its verdict
    // in the console window); a still-running one is EngineBusy and nothing is sent
    let settled_previous = settle_unresolved_previous(&state, &console_path, local.as_ref(), &ctx, &mut notes).await?;
    let mounted = local.as_ref().and_then(GameLocal::mounted);
    let mut idle = mounted == Some(false);
    if mounted != Some(true) {
        match level_state(&console_path) {
            LevelState::Loading(line) => match console_idle_ms(&console_path) {
                // a `Loading map` line nothing followed for the whole stale window: a failure
                // line outside failure_regexes, an aborted load, a load Hammer's own UI started
                Some(idle_ms) if idle_ms >= SETTLE_STALE_MS => notes.push(format!(
                    "the last level line in console.log is {line:?} and the console has been \
                     silent for {} s, so the load is taken as stale and the line runs; if the \
                     engine is in fact still loading, re-check game/csgo/console.log",
                    idle_ms / 1000
                )),
                _ => return Err(level_loading(&line)),
            },
            LevelState::Idle => idle = true,
            LevelState::Mounted | LevelState::Unknown => {}
        }
    }
    if idle {
        notes.push(
            "the engine is at its idle level (no map loaded): the line runs, but map-dependent \
             commands (sv_*, entity and player commands) have nothing to act on; game_type, \
             game_mode, sv_cheats and mp_* set here apply to the next run_map"
                .into(),
        );
    }

    let mut watch = ConsoleWatch::open(console_path);
    let marker = new_marker();
    let mut extra = serde_json::Map::new();
    let bridge_used = send_via_bridge(&state, &ctx, &parsed.command, &mut extra, &mut notes).await?;
    let route = if bridge_used {
        "bridge"
    } else {
        send_via_vtm(&state, &parsed.command, &mut extra, "vtmsend").await?;
        "vtm"
    };
    // the end-of-output marker goes the same way; a failure here only costs the quiet fallback
    // (not after a cut-off exec: a second post to a stuck channel only doubles the wait)
    let marker_sent = if extra.contains_key("deliveryWarning") {
        false
    } else {
        let sent = if bridge_used {
            send_via_bridge(&state, &ctx, &marker, &mut extra, &mut notes).await
        } else {
            send_via_vtm(&state, &marker, &mut extra, "vtmsendMarker").await.map(|()| true)
        };
        match sent {
            Ok(true) => true,
            Ok(false) => false,
            Err(error) => {
                notes.push(format!(
                    "the end-of-output marker could not be sent ({}); the wait ends when the output goes quiet",
                    error.message
                ));
                false
            }
        }
    };

    if route == "bridge" {
        let first = parsed.command.split_whitespace().next().unwrap_or("");
        if first.eq_ignore_ascii_case("echo") || (!first.contains(';') && parsed.command.trim() == first && first.contains('_')) {
            notes.push(
                "on the bridge route in a -tools session, echo and cvar prints do not reach \
                 console.log, so no output or waitFor match will show: if this is a cvar read, \
                 use cvar_get; only status-class output and errors are logged"
                    .into(),
            );
        }
    }

    let wait = Duration::from_millis(parsed.wait_ms)
        .min(ctx.budget.remaining().saturating_sub(Duration::from_millis(200)));
    let marker_re = marker_sent.then(|| marker_regex(&marker));
    let quiet_with_marker = route == "vtm" && parsed.wait_for.is_none();
    let back = read_back(&mut watch, wait, parsed.wait_for.as_ref(), marker_re.as_ref(), quiet_with_marker).await;
    // the engine dropped a line (belt and braces: lines over the limit are refused up front)
    if back.lines.iter().any(|l| is_too_long_warning(l)) {
        let warning = "the engine logged \"Command too long\" (a command over 510 bytes) or \"Console command too long.\" (a command name over 128 bytes): a command was dropped, it did not run";
        // keep an earlier delivery warning (a vtm cut-off) and append this one
        let joined = match extra.get("deliveryWarning").and_then(Value::as_str) {
            Some(earlier) => format!("{earlier}; {warning}"),
            None => warning.to_string(),
        };
        extra.insert("deliveryWarning".into(), json!(joined));
    }
    if !back.exists {
        extra.insert(
            "consoleNote".into(),
            json!("game/csgo/console.log does not exist (the session needs -condebug): no output to read back"),
        );
    }
    let unverified_echo = marker_sent && route == "vtm" && back.ended_by == "timeout" && back.lines.is_empty();
    if unverified_echo {
        notes.push(
            "nothing came back and the marker echo was not seen: the vtm route may print neither \
             the command's output nor the marker to console.log (the echo is unverified there), so \
             this does not show whether the command ran"
                .into(),
        );
    } else if back.lines.is_empty() && back.exists {
        if route == "vtm" {
            notes.push(
                "the vtm route returned no lines: commands posted over the VToolMessage channel \
                 print nothing to console.log for cvar queries (use cvar_get) and some others"
                    .into(),
            );
        } else if back.ended_by == "marker" {
            notes.push(
                "the engine ran the command (the marker followed it) but console.log gained no \
                 output lines from it; some commands print nowhere console.log sees. For a cvar \
                 value use cvar_get"
                    .into(),
            );
        }
    }
    if marker_sent && back.ended_by == "timeout" && !back.marker_seen && !unverified_echo {
        notes.push(
            "the end-of-output marker was not seen within waitMs: the output may be incomplete \
             (a longer waitMs, or waitFor)"
                .into(),
        );
    }
    extra.insert("markerSent".into(), json!(marker_sent));
    if let Some(previous) = settled_previous {
        extra.insert("settledPreviousLoad".into(), previous);
    }
    if !notes.is_empty() {
        extra.insert("notes".into(), json!(notes));
    }
    Ok(result_json(route, &parsed.command, &back, &parsed, Value::Object(extra)).into())
}

pub async fn cvar_get(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let names = cs2_mcp_common::console::cvar_names(&args).map_err(invalid)?;
    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(
            "cvar.get",
            json!({"names": names}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await
        .map_err(ToolError::from)?;
    let mut result = reply.result;
    cs2_mcp_common::console::redact_cvar_reply(&mut result, &names);
    Ok(result.into())
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn arguments_are_validated() {
        let ok = ConsoleArgs::parse(&json!({"command": "sv_cheats 1; echo ok"})).unwrap();
        assert_eq!((ok.wait_ms, ok.max_lines), (WAIT_DEFAULT_MS, MAX_LINES_DEFAULT));
        for bad in [
            json!({}),
            json!({"command": 3}),
            json!({"command": "a\nb"}),
            json!({"command": "a", "waitMs": 10_001}),
            json!({"command": "a", "waitMs": -1}),
            json!({"command": "a", "maxLines": 0}),
            json!({"command": "a", "waitFor": "("}),
            json!({"command": "a", "waitFor": 3}),
            json!({"command": "a", "extra": 1}),
        ] {
            assert!(ConsoleArgs::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_newest_level_line_decides() {
        assert_eq!(level_state_of("hello\n"), LevelState::Unknown);
        assert_eq!(
            level_state_of("Spawn Server: <empty>\nLoading map \"probe\"\n"),
            LevelState::Loading("Loading map \"probe\"".into())
        );
        assert_eq!(
            level_state_of("Loading map \"probe\"\nOnSwitchLoopModeFinished( game : success )\nx\n"),
            LevelState::Mounted
        );
        assert_eq!(
            level_state_of("Loading map \"probe\"\nSpawn Server: <empty>\nnoise\n"),
            LevelState::Idle
        );
        assert_eq!(
            level_state_of("Loading map \"probe\"\nOnSwitchLoopModeFinished( game : fail )\n"),
            LevelState::Unknown
        );
    }
}
