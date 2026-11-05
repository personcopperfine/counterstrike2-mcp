//! `console_tail` and `console_wait` over the host `console.log` (workflows/03). Both are
//! ReadOnly and dispatch no bridge methods: they work without a bridge.
use crate::console::resolve::{describe, resolve_within, session_arg, Purpose, ResolvedLog};
use crate::console::{ConsoleError, ConsoleLog, Cursor, WaitError, WaitFrom};
use crate::state::ClientState;
use cs2_mcp_common::tools::{CallCtx, ToolError, ToolOutput};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::time::Duration;

const TAIL_DEFAULT: u64 = 50;
const TAIL_MAX: u64 = 2000;
const WAIT_DEFAULT_MS: u64 = 10_000;
/// Keep this much of the call budget for the response after waiting.
const BUDGET_MARGIN: Duration = Duration::from_millis(500);
const WAIT_TIMEOUT_LAST_LINES: usize = 10;

pub async fn console_tail(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let lines = match args.get("lines") {
        None | Some(Value::Null) => TAIL_DEFAULT,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=TAIL_MAX).contains(n))
            .ok_or_else(|| ToolError::invalid_arguments("lines must be an integer 1..=2000"))?,
    };
    let grep = optional_regex(&args, "grep")?;
    let resolved = resolve_within(
        &state,
        &ctx,
        session_arg(&args)?.as_deref(),
        Purpose::Reader,
    )
    .await?;
    let log = ConsoleLog::new(&resolved.path);
    match log.tail(lines as usize, grep).await {
        Ok(tail) => Ok(describe(
            json!({
                "path": display(&resolved.path),
                "lines": tail.lines,
                "linesScanned": tail.lines_scanned,
                "bytesScanned": tail.bytes_scanned,
                "reachedStartOfFile": tail.reached_start_of_file,
                "cursor": tail.cursor,
            }),
            &resolved,
        )
        .into()),
        Err(e) => Err(console_error(e, &resolved.path)),
    }
}

pub async fn console_wait(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let pattern = args
        .get("regex")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| ToolError::invalid_arguments("regex must be a non-empty string"))?;
    let re = compile(pattern, "regex")?;
    let session = session_arg(&args)?;
    let requested_ms = match args.get("timeoutMs") {
        None | Some(Value::Null) => WAIT_DEFAULT_MS,
        Some(v) => v.as_u64().ok_or_else(|| {
            ToolError::invalid_arguments("timeoutMs must be a non-negative integer")
        })?,
    };
    let from_start = match args.get("fromStart") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| ToolError::invalid_arguments("fromStart must be a boolean"))?,
    };
    let since = parse_since(&args)?;
    if from_start && since.is_some() {
        return Err(ToolError::invalid_arguments(
            "fromStart and since are mutually exclusive",
        ));
    }
    if requested_ms == 0 && !from_start && since.is_none() {
        return Err(ToolError::invalid_arguments(
            "timeoutMs 0 is only allowed with fromStart or since (it would only look at what is already there)",
        ));
    }
    // Never wait past the call budget: the client would give up first. Too little budget left
    // to wait at all is the budget error, not a timeout.
    let remaining = ctx.budget.remaining();
    if remaining < BUDGET_MARGIN {
        return Err(ToolError::budget_exceeded(&ctx.budget));
    }
    let cap = remaining - BUDGET_MARGIN;
    let mut resolved = resolve_within(&state, &ctx, session.as_deref(), Purpose::Reader).await?;
    // A cursor names the file it was taken from: that is explicit evidence of what the caller
    // means, so it wins over the resolved file (source "cursor").
    if let Some(path) = since.as_ref().and_then(|c| c.path.as_deref()) {
        if Path::new(path) != resolved.path {
            resolved = ResolvedLog {
                path: path.into(),
                source: "cursor",
                session: None,
                note: None,
            };
        }
    }
    // Same file, but the inode changed (rotation) or the file is shorter than the cursor: the
    // wait falls back to offset 0 and says so.
    let cursor_reset = match (
        &since,
        ConsoleLog::new(&resolved.path).cursor().ok().flatten(),
    ) {
        (Some(c), Some(now)) => c.inode != now.inode || c.offset > now.offset,
        _ => false,
    };
    let requested = Duration::from_millis(requested_ms);
    let timeout = requested.min(cap);
    let from = match (from_start, since) {
        (true, _) => WaitFrom::Beginning,
        (false, Some(c)) => WaitFrom::At(c),
        (false, None) => WaitFrom::End,
    };
    let log = ConsoleLog::new(&resolved.path);
    match log.wait_for(&re, timeout, from).await {
        Ok(m) => Ok(describe(
            json!({
                "matched": true,
                "line": m.line,
                "captures": captures(&re, &m.line),
                "waitedMs": millis(m.waited),
                "path": display(&resolved.path),
                "cursorReset": cursor_reset,
            }),
            &resolved,
        )
        .into()),
        Err(WaitError::Missing) => Err(console_error(ConsoleError::Missing, &resolved.path)),
        Err(WaitError::Timeout {
            waited,
            file_exists,
        }) => {
            let last_lines = match log.tail(WAIT_TIMEOUT_LAST_LINES, None).await {
                Ok(t) => t.lines,
                Err(_) => Vec::new(),
            };
            let data = describe(
                json!({
                    "regex": pattern,
                    "path": display(&resolved.path),
                    "fileExists": file_exists,
                    "waitedMs": millis(waited),
                    "timeoutMs": millis(timeout),
                    "requestedTimeoutMs": millis(requested),
                    "clampedToBudget": requested > cap,
                    "lastLines": last_lines,
                    "cursorReset": cursor_reset,
                }),
                &resolved,
            );
            Err(ToolError::new(
                "WaitTimeout",
                "WaitTimeout",
                format!(
                    "no complete console.log line matched {pattern:?} within {} ms",
                    millis(waited)
                ),
            )
            .retry_safe(true)
            .with_data(data))
        }
        Err(WaitError::Io(e)) => Err(io_error(e)),
    }
}

/// `since: {inode, offset}` from an earlier `console_tail` (or `ConsoleLog::cursor`).
fn parse_since(args: &Value) -> Result<Option<Cursor>, ToolError> {
    match args.get("since") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => serde_json::from_value::<Cursor>(v.clone())
            .map(Some)
            .map_err(|e| {
                ToolError::invalid_arguments(format!(
                    "since must be {{\"inode\": u64, \"offset\": u64}}: {e}"
                ))
            }),
    }
}

/// Named groups by name; unnamed groups by their index (group 0 is the whole match).
fn captures(re: &Regex, line: &str) -> Value {
    let mut out = Map::new();
    if let Some(caps) = re.captures(line) {
        for (i, name) in re.capture_names().enumerate().skip(1) {
            let value = caps.get(i).map_or(Value::Null, |m| json!(m.as_str()));
            let key = name.map_or_else(|| i.to_string(), str::to_string);
            out.insert(key, value);
        }
    }
    Value::Object(out)
}

fn optional_regex(args: &Value, field: &str) -> Result<Option<Regex>, ToolError> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => compile(s, field).map(Some),
        Some(_) => Err(ToolError::invalid_arguments(format!(
            "{field} must be a string"
        ))),
    }
}

fn compile(pattern: &str, field: &str) -> Result<Regex, ToolError> {
    Regex::new(pattern)
        .map_err(|e| ToolError::invalid_arguments(format!("{field} is not a valid regex: {e}")))
}

pub(crate) fn console_error(e: ConsoleError, path: &Path) -> ToolError {
    match e {
        ConsoleError::Missing => ToolError::new(
            "ConsoleLogMissing",
            "ConsoleLogMissing",
            format!("console.log not found at {}", display(path)),
        )
        .retry_safe(true)
        .with_data(json!({
            "path": display(path),
            "hint": "pass --console-log <path> (CS2_MCP_CONSOLE_LOG), or start a session that writes console.log (-condebug)",
        })),
        ConsoleError::Io(e) => io_error(e),
    }
}

fn io_error(e: std::io::Error) -> ToolError {
    ToolError::new(
        "ConsoleLogIo",
        "ConsoleLogIo",
        format!("console.log I/O failed: {e}"),
    )
    .retry_safe(true)
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

fn millis(d: Duration) -> u64 {
    d.as_millis() as u64
}
