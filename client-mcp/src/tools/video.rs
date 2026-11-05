//! `demo_record_start`, `demo_record_stop` and `demo_to_video` (workflows/07).
//!
//! Recording is two console commands over the bridge (`console.exec`, WineTools only: the registry
//! gates the tools on that method, so without it the call is `NotImplemented` and nothing is sent).
//! Each tool reads the console.log cursor BEFORE it sends, then confirms the engine's own line from
//! that cursor (a predicate wait, workflows/AGENT.md convention 12). `demo_to_video` stages a
//! generated movie suite and runs it through the run_testscript machinery (`start_run`) under
//! wine-tools, the proven encoder path; its judge is `video::judge`.
use crate::console::resolve::{describe, resolve_within, session_arg, Purpose, ResolvedLog};
use crate::console::{ConsoleLog, Cursor, WaitError, WaitFrom, WaitMatch};
use crate::state::ClientState;
use crate::tools::testscript::{
    start_run, MovieRequest, StartRequest, DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS,
};
use crate::video::{valid_movie_name, MovieFormat, MovieSpec, DEFAULT_FRAMERATE, MAX_FRAMERATE};
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{CallCtx, ToolError, ToolOutput};
use regex::Regex;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long `demo_record_start` waits for the engine's "Recording" line (clamped to the budget).
const START_CONFIRM: Duration = Duration::from_secs(2);
/// How long `demo_record_stop` waits for the "Completed" line (clamped to the budget).
const STOP_CONFIRM: Duration = Duration::from_secs(5);
/// The size fallback (no console.log): poll interval, and the wait cap.
const STABLE_POLL: Duration = Duration::from_millis(250);
const STABLE_MAX: Duration = Duration::from_secs(3);
/// Kept from the call budget for the reply.
const BUDGET_MARGIN: Duration = Duration::from_millis(500);

/// `tv_delay 0; tv_record <name>`: the command `demo_record_start` sends (one console.exec).
pub fn record_start_command(name: &str) -> String {
    format!("tv_delay 0; tv_record {name}")
}

pub const RECORD_STOP_COMMAND: &str = "tv_stoprecord";
/// The line the engine logs when a SourceTV recording starts (gametest-20261010-135002 console.log:607).
pub const START_PATTERN: &str = r"\[HLTV Server\] Recording SourceTV replay from";

/// The line the engine logs when a recording is finished (gametest-20261010-135002 console.log:629).
pub fn stop_pattern(name: &str) -> String {
    format!(
        r#"\[HLTV Server\] Completed SourceTV demo "{}\.dem""#,
        regex::escape(name)
    )
}

/// The SourceTV preamble note, returned with every `demo_record_start` result.
pub const TV_ENABLE_NOTE: &str = "tv_enable 1 must be set BEFORE `map`: SourceTV is decided when a map loads, and this command does not reload the map. For a suite, use run_testscript {demo:true, inline:\"tv_enable 1; map <map>; ...\"} (the run then also requires a vtest_*.dem; name the recording vtest_<...>).";

fn invalid(msg: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(msg)
}

/// A required name: [`valid_movie_name`] (alphanumeric first, `[A-Za-z0-9_-]`, at most 64).
fn name_arg(args: &Value, key: &str) -> Result<String, ToolError> {
    let s = args
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{key} must be a string")))?;
    if !valid_movie_name(s) {
        return Err(invalid(format!(
            "{key} {s:?} must start with a letter or digit, then [A-Za-z0-9_-], at most 64 characters"
        )));
    }
    Ok(s.to_string())
}

fn opt_u32(args: &Value, key: &str) -> Result<Option<u32>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| invalid(format!("{key} must be a non-negative integer"))),
    }
}

fn opt_bool(args: &Value, key: &str) -> Result<bool, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(invalid(format!("{key} must be a boolean"))),
    }
}

/// The session's log, if one is resolvable. A named session that does not exist is an error; with
/// no session named, "no log" is `None` and the caller reports the result as unconfirmed.
async fn best_effort_log(
    state: &ClientState,
    ctx: &CallCtx,
    session: Option<&str>,
) -> Result<Option<ResolvedLog>, ToolError> {
    match resolve_within(state, ctx, session, Purpose::Bridge).await {
        Ok(r) => Ok(Some(r)),
        Err(e) if session.is_none() => {
            tracing::info!(note = %e.message, "no console.log for the recording tools");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// The `csgo/` folder: beside the session's console.log, else the default wine app tree.
fn csgo_for(state: &ClientState, resolved: Option<&ResolvedLog>) -> PathBuf {
    resolved
        .and_then(|r| r.path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| state.runtime.wine_app_dir().join("game/csgo"))
}

/// The budget left for a confirmation wait: `want`, clamped to the call budget minus the margin.
fn confirm_cap(ctx: &CallCtx, want: Duration) -> Duration {
    ctx.budget
        .remaining()
        .checked_sub(BUDGET_MARGIN)
        .unwrap_or(Duration::ZERO)
        .min(want)
}

fn cursor_json(c: &Option<Cursor>) -> Value {
    json!(c)
}

pub fn record_start_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "session": {"type": "string"}
        },
        "required": ["name"]
    })
}

pub fn record_stop_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "session": {"type": "string"}
        },
        "required": ["name"]
    })
}

pub fn to_video_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "demo": {"type": "string", "description": "stem or host path of a demo directly in csgo/"},
            "name": {"type": "string", "description": "output is csgo/<name>.webm"},
            "framerate": {"type": "integer", "minimum": 1, "maximum": MAX_FRAMERATE, "default": DEFAULT_FRAMERATE},
            "format": {"type": "string", "enum": ["webm", "png", "tga", "jpg"], "default": "webm"},
            "width": {"type": "integer", "minimum": 1, "description": "with height; default: render target"},
            "height": {"type": "integer", "minimum": 1},
            "bitrate": {"type": "integer", "minimum": 1, "maximum": 500},
            "countFrames": {"type": "boolean", "default": false},
            "timeoutMs": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS, "default": DEFAULT_TIMEOUT_MS}
        },
        "required": ["demo", "name"]
    })
}

/// `demo_record_start {name, session?}`: cursor, then `tv_delay 0; tv_record <name>` over
/// console.exec, then the engine's "Recording SourceTV replay from" line from that cursor.
pub async fn demo_record_start(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let name = name_arg(&args, "name")?;
    let session = session_arg(&args)?;
    let command = record_start_command(&name);
    let resolved = best_effort_log(&state, &ctx, session.as_deref()).await?;
    // the cursor is read BEFORE the command is sent, so the confirmation cannot miss its line
    let cursor = resolved
        .as_ref()
        .and_then(|r| ConsoleLog::new(&r.path).cursor().ok().flatten());
    let log_path = resolved
        .as_ref()
        .map(|r| r.path.clone())
        .filter(|p| p.is_file());
    let deadline = ctx.budget.deadline_ms()?;
    let queued = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "console.exec",
                json!({"command": command}),
                RequestOpts::deadline(deadline),
            )
            .await?
            .result
    };
    let csgo = csgo_for(&state, resolved.as_ref());
    let demo_path = csgo.join(format!("{name}.dem")).display().to_string();
    let mut result = json!({
        "route": "bridge",
        "name": name,
        "command": command,
        "queued": queued,
        "consoleCursor": cursor_json(&cursor),
        "demoPath": demo_path,
        "tvEnableBeforeMap": TV_ENABLE_NOTE,
    });
    let Some(path) = log_path else {
        result["recording"] = json!("unconfirmed");
        result["note"] = json!(
            "no console.log to confirm from (no session log resolvable): the command was queued, and the recording is not confirmed"
        );
        return Ok(result.into());
    };
    let re = Regex::new(START_PATTERN).expect("fixed pattern");
    let from = cursor.clone().map_or(WaitFrom::Beginning, WaitFrom::At);
    let cap = confirm_cap(&ctx, START_CONFIRM);
    let started = Instant::now();
    match ConsoleLog::new(&path).wait_for(&re, cap, from).await {
        Ok(WaitMatch { line, waited }) => {
            result["recording"] = json!(true);
            result["confirmedLine"] = json!(line);
            result["waitedMs"] = json!(waited.as_millis() as u64);
            if let Some(r) = &resolved {
                merge_describe(&mut result, r);
            }
            Ok(result.into())
        }
        Err(WaitError::Timeout { .. }) => Err(ToolError::new(
            "RecordingNotStarted",
            "RecordingNotStarted",
            format!(
                "tv_record {name} was queued, but no \"Recording SourceTV replay from\" line followed in {} ms. {TV_ENABLE_NOTE}",
                started.elapsed().as_millis()
            ),
        )
        .retry_safe(true)
        .with_data(json!({
            "name": name,
            "command": command,
            "consoleCursor": cursor_json(&cursor),
            "waitedMs": started.elapsed().as_millis() as u64,
            "tvEnableBeforeMap": TV_ENABLE_NOTE,
            "consoleLog": path.display().to_string(),
        }))),
        Err(e) => {
            result["recording"] = json!("unconfirmed");
            result["note"] = json!(format!("console.log could not be read: {}", wait_err_text(&e)));
            Ok(result.into())
        }
    }
}

fn wait_err_text(e: &WaitError) -> String {
    match e {
        WaitError::Timeout { .. } => "timed out".into(),
        WaitError::Missing => "missing".into(),
        WaitError::Io(e) => e.to_string(),
    }
}

fn merge_describe(result: &mut Value, r: &ResolvedLog) {
    if let Value::Object(fields) = describe(json!({}), r) {
        for (k, v) in fields {
            result[k.as_str()] = v;
        }
    }
}

/// `demo_record_stop {name, session?}`: cursor, `tv_stoprecord`, then the engine's
/// "Completed SourceTV demo" line from that cursor. Without a log, the .dem's size must settle.
/// The file must be modified at or after this call started (an older .dem is a previous run's).
pub async fn demo_record_stop(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let name = name_arg(&args, "name")?;
    let session = session_arg(&args)?;
    let resolved = best_effort_log(&state, &ctx, session.as_deref()).await?;
    let csgo = csgo_for(&state, resolved.as_ref());
    let demo = csgo.join(format!("{name}.dem"));
    let cursor = resolved
        .as_ref()
        .and_then(|r| ConsoleLog::new(&r.path).cursor().ok().flatten());
    let log_path = resolved
        .as_ref()
        .map(|r| r.path.clone())
        .filter(|p| p.is_file());
    let since = SystemTime::now();
    let deadline = ctx.budget.deadline_ms()?;
    let queued = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "console.exec",
                json!({"command": RECORD_STOP_COMMAND}),
                RequestOpts::deadline(deadline),
            )
            .await?
            .result
    };
    let started = Instant::now();
    let mut result = json!({
        "route": "bridge",
        "name": name,
        "command": RECORD_STOP_COMMAND,
        "queued": queued,
        "consoleCursor": cursor_json(&cursor),
        "demoPath": demo.display().to_string(),
    });
    let confirmed_by = match log_path {
        Some(path) => {
            let re = Regex::new(&stop_pattern(&name)).expect("escaped name");
            let from = cursor.clone().map_or(WaitFrom::Beginning, WaitFrom::At);
            let cap = confirm_cap(&ctx, STOP_CONFIRM);
            match ConsoleLog::new(&path).wait_for(&re, cap, from).await {
                Ok(WaitMatch { line, .. }) => {
                    result["confirmedLine"] = json!(line);
                    "log"
                }
                Err(e) => {
                    return Err(ToolError::new(
                        "DemoNotCompleted",
                        "DemoNotCompleted",
                        format!(
                            "tv_stoprecord was queued, but no \"Completed SourceTV demo\" line for {name}.dem followed in {} ms ({})",
                            started.elapsed().as_millis(),
                            wait_err_text(&e)
                        ),
                    )
                    .retry_safe(true)
                    .with_data(json!({
                        "name": name,
                        "demoPath": demo.display().to_string(),
                        "consoleCursor": cursor_json(&cursor),
                        "waitedMs": started.elapsed().as_millis() as u64,
                    })));
                }
            }
        }
        None => {
            let settle = wait_stable(&demo, confirm_cap(&ctx, STABLE_MAX)).await;
            result["stable"] = json!(settle.stable);
            "size"
        }
    };
    let (bytes, modified) = fresh_demo(&demo, since)?;
    result["confirmedBy"] = json!(confirmed_by);
    result["bytes"] = json!(bytes);
    result["modifiedMs"] = json!(modified);
    result["waitedMs"] = json!(started.elapsed().as_millis() as u64);
    if let Some(r) = &resolved {
        merge_describe(&mut result, r);
    }
    Ok(result.into())
}

/// The .dem must exist, be non-empty, and be modified at or after `since`. Returns (bytes, the
/// modification time in ms since the epoch).
fn fresh_demo(path: &Path, since: SystemTime) -> Result<(u64, u128), ToolError> {
    let meta = path.metadata().ok().filter(|m| m.is_file());
    let Some(meta) = meta else {
        return Err(ToolError::new(
            "DemoNotWritten",
            "DemoNotWritten",
            format!("{} does not exist", path.display()),
        )
        .retry_safe(true)
        .with_data(json!({"path": path.display().to_string()})));
    };
    let modified = meta.modified().unwrap_or(UNIX_EPOCH);
    let ms = |t: SystemTime| {
        t.duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    };
    if modified < since {
        return Err(ToolError::new(
            "DemoStale",
            "DemoStale",
            format!(
                "{} was last modified before this call started: it is an earlier recording",
                path.display()
            ),
        )
        .retry_safe(true)
        .with_data(json!({
            "path": path.display().to_string(),
            "modifiedMs": ms(modified),
            "callStartedMs": ms(since),
        })));
    }
    if meta.len() == 0 {
        return Err(ToolError::new(
            "DemoNotWritten",
            "DemoNotWritten",
            format!("{} is empty", path.display()),
        )
        .retry_safe(true)
        .with_data(json!({"path": path.display().to_string()})));
    }
    Ok((meta.len(), ms(modified)))
}

/// Poll the file until two equal non-zero sizes in a row, or the cap (the no-log fallback).
async fn wait_stable(path: &Path, cap: Duration) -> Settle {
    let started = Instant::now();
    let mut last: Option<u64> = None;
    loop {
        let size = std::fs::metadata(path)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len());
        let stable = matches!(size, Some(s) if s > 0 && last == Some(s));
        if stable || started.elapsed() >= cap {
            return Settle { stable };
        }
        last = size;
        tokio::time::sleep(STABLE_POLL.min(cap.saturating_sub(started.elapsed()))).await;
    }
}

struct Settle {
    stable: bool,
}

/// The demo argument: a stem, or a `.dem` path directly inside `csgo/`. Returns the stem.
/// `<csgo>/../csgo/x.dem` and a relative `x.dem` are refused: the parent must be exactly `csgo`.
fn demo_stem(arg: &str, csgo: &Path) -> Result<String, ToolError> {
    let p = Path::new(arg);
    let stem = if p.extension().is_some_and(|e| e == "dem") {
        if p.parent() != Some(csgo) {
            return Err(invalid(format!(
                "demo {arg:?} must be a .dem directly in {} (or a bare stem)",
                csgo.display()
            )));
        }
        p.file_stem().map(|s| s.to_string_lossy().into_owned())
    } else {
        Some(arg.to_string())
    };
    stem.filter(|s| valid_movie_name(s)).ok_or_else(|| {
        invalid(format!(
            "demo {arg:?} must be a stem: alphanumeric first, then [A-Za-z0-9_-], at most 64 characters"
        ))
    })
}

/// `demo_to_video {demo, name, framerate?, format?, width?, height?, bitrate?, countFrames?, timeoutMs?}`.
pub async fn demo_to_video(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let demo_arg = args
        .get("demo")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("demo must be a string"))?;
    let name = name_arg(&args, "name")?;
    let framerate = opt_u32(&args, "framerate")?.unwrap_or(DEFAULT_FRAMERATE);
    let format = match args.get("format") {
        None | Some(Value::Null) => MovieFormat::Webm,
        Some(Value::String(s)) => MovieFormat::parse(s).map_err(invalid)?,
        Some(_) => return Err(invalid("format must be a string")),
    };
    let count_frames = opt_bool(&args, "countFrames")?;
    let timeout_ms = match args.get("timeoutMs") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_MS,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_TIMEOUT_MS).contains(n))
            .ok_or_else(|| invalid("timeoutMs must be an integer in 1..=900000"))?,
    };
    let csgo = state.runtime.wine_app_dir().join("game/csgo");
    let demo = demo_stem(demo_arg, &csgo)?;
    let mut spec = MovieSpec::new(demo, name.clone(), format).framerate(framerate);
    spec.width = opt_u32(&args, "width")?;
    spec.height = opt_u32(&args, "height")?;
    if let Some(b) = opt_u32(&args, "bitrate")? {
        spec = spec.bitrate(b);
    }
    spec.validate().map_err(invalid)?;
    let dem = csgo.join(format!("{}.dem", spec.demo));
    if !dem.metadata().is_ok_and(|m| m.is_file() && m.len() > 0) {
        return Err(ToolError::new(
            "DemoNotFound",
            "DemoNotFound",
            format!("{} is missing or empty", dem.display()),
        )
        .retry_safe(true)
        .with_data(json!({"path": dem.display().to_string()})));
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let req = StartRequest {
        suite: spec.suite(stamp),
        inline: false,
        judge: None,
        demo: false,
        timeout_ms,
        movie: Some(MovieRequest {
            name: name.clone(),
            format,
            framerate,
            ffprobe: state.ffprobe.clone(),
            ffmpeg: state.ffmpeg.clone(),
            count_frames,
        }),
    };
    let mut out = start_run(state.clone(), ctx, req).await?;
    let output = match format {
        MovieFormat::Webm => csgo.join(format!("{name}.webm")).display().to_string(),
        _ => csgo
            .join(format!("{name}NNNNNNNN.{}", format.as_str()))
            .display()
            .to_string(),
    };
    out["movie"] = json!({
        "name": name,
        "format": format.as_str(),
        "framerate": framerate,
        "width": spec.width,
        "height": spec.height,
        "bitrate": spec.bitrate,
        "countFrames": count_frames,
        "playdemo": spec.playdemo_args(),
        "output": output,
    });
    out["ffprobe"] = json!(state.ffprobe.as_ref().map(|p| p.display().to_string()));
    out["ffmpeg"] = json!(state.ffmpeg.as_ref().map(|p| p.display().to_string()));
    out["timeoutMs"] = json!(timeout_ms);
    out["runsUnder"] = json!("wine-tools (every flavour: the engine encoder path)");
    out["next"] = json!("test_wait {runId}");
    Ok(out.into())
}
