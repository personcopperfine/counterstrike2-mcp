//! `screenshot` and `observe` (workflows 08 and 09). Frames come from the game's own Vulkan
//! swapchain through the capture layer (`cs2-vk-capture`, protocol client in
//! `cs2_mcp_common::capture::vklayer`). X server grabs are never used.
//!
//! A session is captured through its `<artifacts>/capture` directory, which exists only when the
//! session was launched with the layer. Session choice: a named `cs2mcp-*` session, else the single
//! running session (preferring the one that has a capture dir), else an error that names the
//! candidates.
//!
//! Budgets (per call, inside the tool budget minus [`BUDGET_MARGIN`]): the frame waits at most
//! [`FRAME_CAP`], OCR at most [`OCR_CAP`], the bridge tick at most [`TICK_CAP`]; the console part
//! gets the rest and runs before the tick.
use crate::console::resolve::{describe, resolve_within, session_arg, Purpose, ResolvedLog};
use crate::console::{ConsoleError, ConsoleLog, Cursor};
use crate::state::ClientState;
use crate::tools::console::console_error;
use crate::tools::runtime_tools::runtime_error;
use crate::tools::vision::{finish_capture, process_capture_mode, Captured, InlineMode};
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::capture::frame::ImageStats;
use cs2_mcp_common::capture::ocr::{self, OcrError, OCR_TIMEOUT};
use cs2_mcp_common::capture::vklayer::{self, FrameError, FrameErrorKind};
use cs2_mcp_common::config::RuntimeKind;
use cs2_mcp_common::runtime::docker::{valid_name, SessionEntry};
use cs2_mcp_common::tools::{
    CallCtx, Image, Mutability::ReadOnly, ToolError, ToolOutput, ToolSpec,
};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_TIMEOUT_MS: u64 = 5000;
pub const MAX_TIMEOUT_MS: u64 = 60_000;
/// Longest a frame may wait for the layer (observe uses it as its frame budget).
pub const FRAME_CAP: Duration = Duration::from_secs(5);
/// Longest OCR may run for one frame.
pub const OCR_CAP: Duration = Duration::from_secs(10);
/// Longest the bridge `game.tick` part may take, connect included.
pub const TICK_CAP: Duration = Duration::from_secs(3);
/// Frames kept in a session's capture dir; older ones are removed after each capture.
pub const KEEP_CAPTURES: usize = 50;
/// Kept back from the call budget so the reply goes out in time.
const BUDGET_MARGIN: Duration = Duration::from_millis(1000);
const OCR_PSM: u8 = 6;
const OBSERVE_CONSOLE_DEFAULT: u64 = 20;
const OBSERVE_CONSOLE_MAX: u64 = 500;
/// Inline width of observe's `image: "small"` (the full-size file is always the `path`).
pub const SMALL_INLINE_WIDTH: u32 = 640;
/// Upper-case phrases of the modal dialogs seen on first boot (after upper-casing and mapping the
/// typographic apostrophe). Either one is enough.
const DIALOG_PHRASES: [&str; 2] = ["SETTINGS RECOMMENDATION", "DON'T SHOW AGAIN"];
/// Cues that make a dialog when a whole-word "OK" is on the frame (not as a standalone line).
const DIALOG_CUES: [&str; 4] = ["HZ", "RECOMMEND", "CANCEL", "APPLY"];
/// A frame with at most this many distinct colours is treated as a loading or blank screen.
pub const LOADING_MAX_COLORS: usize = 32;
/// The checkpoint line the test-script runner writes (`testscript_debug 1`).
const CHECKPOINT_RE: &str = r"TESTSCRIPT: CheckPoint -> '([^']+)'";
/// The one session a `--capture-dir` server captures (hammer-mcp's `run_map` names its native
/// posts the same way).
pub(crate) const NATIVE_SESSION: &str = "native";
/// `--capture-dir` sessions (`cs2mcp-native up` loads the layer into the game; README "Packaging")
const HINT_NATIVE: &str = "the game must run with the capture layer (cs2mcp-native up loads it when its payload has vklayer/<platform>; cs2mcp-native status shows capture) and be presenting frames; a Windows game must run with -vulkan";
const HINT_LAYER: &str = "check session_status runtime.vkLayer (must be valid) and that the session was launched with capture (launch_session capture: true, the default). The layer runs as the container's root and refuses a capture dir its uid does not own: rootless podman maps that root to this user, so rootful podman or --userns=keep-id makes the layer ignore requests (see client-mcp/docker/README.md, ownership). The game must be presenting frames.";

static SEQ: AtomicU64 = AtomicU64::new(0);

pub fn screenshot_spec() -> ToolSpec {
    ToolSpec::new(
        "screenshot",
        "Capture the game view: the next frame the session presents, from its Vulkan swapchain via the capture layer. Returns the host PNG path (full size) and an inline image. Needs a session launched with capture (the default). Not for: a render without the player (render_world).",
        ReadOnly,
        &[],
    )
    .with_schema(json!({
        "type": "object",
        "properties": {
            "session": {"type": "string"},
            "timeoutMs": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS, "default": DEFAULT_TIMEOUT_MS},
            "ocr": {"type": "boolean", "default": false},
            "maxWidth": {"type": "integer", "minimum": 16, "maximum": 8192}
        },
        "additionalProperties": false
    }))
}

pub fn observe_spec() -> ToolSpec {
    ToolSpec::new(
        "observe",
        "One playtest observation: screenshot, console.log tail or lines since a cursor, checkpoint events, game.tick, a screen guess (dialog|loading|unknown). Each part fails independently into errors[].",
        ReadOnly,
        &[],
    )
    .with_schema(json!({
        "type": "object",
        "properties": {
            "session": {"type": "string"},
            "capture": {"type": "boolean", "default": true},
            "ocr": {"type": "boolean", "default": false},
            "image": {"type": "string", "enum": ["inline", "small", "none"], "default": "small", "description": "inline | small (<= 640 px) | none; the file is full size"},
            "consoleLines": {"type": "integer", "minimum": 0, "maximum": OBSERVE_CONSOLE_MAX, "default": OBSERVE_CONSOLE_DEFAULT},
            "since": {"type": "object", "properties": {"inode": {"type": "integer"}, "offset": {"type": "integer"}, "path": {"type": "string"}}, "required": ["inode", "offset"]}
        },
        "additionalProperties": false
    }))
}

// ---------------------------------------------------------------------------------------------
// arguments
// ---------------------------------------------------------------------------------------------

fn invalid(msg: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(msg)
}

fn object_args<'a>(args: &'a Value, allowed: &[&str]) -> Result<&'a Map<String, Value>, ToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| invalid("arguments must be an object"))?;
    if let Some(bad) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(invalid(format!("unknown argument {bad:?}")));
    }
    Ok(obj)
}

/// `session` as the console tools take it, validated up front: a bad name is an argument error
/// before any capture or console part runs. With `--capture-dir` the only session is `native`
/// (passed or not, it resolves to `None`: the console part then reads `--console-log`).
fn checked_session(state: &ClientState, args: &Value) -> Result<Option<String>, ToolError> {
    let session = session_arg(args)?;
    if state.capture_dir.is_some() {
        native_session(session.as_deref())?;
        return Ok(None);
    }
    if let Some(name) = &session {
        if !valid_name(name) {
            return Err(invalid(format!(
                "session must be a cs2mcp-* container name, got {name:?}"
            )));
        }
    }
    Ok(session)
}

fn opt_bool(obj: &Map<String, Value>, key: &str, default: bool) -> Result<bool, ToolError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(invalid(format!("{key} must be a boolean"))),
    }
}

fn opt_u64(
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
            .ok_or_else(|| invalid(format!("{key} must be an integer in {min}..={max}"))),
    }
}

// ---------------------------------------------------------------------------------------------
// session and frame
// ---------------------------------------------------------------------------------------------

fn capture_unavailable(message: impl Into<String>, data: Value) -> ToolError {
    ToolError::new("CaptureUnavailable", "CaptureUnavailable", message)
        .retry_safe(true)
        .with_data(data)
}

/// Time left for a part, after the call-budget margin. Zero means no budget.
fn room(ctx: &CallCtx) -> Duration {
    ctx.budget.remaining().saturating_sub(BUDGET_MARGIN)
}

/// The session a capture comes from. Named (any cs2mcp-* container, running or not) > the single
/// running session with an artifacts dir > among several, the single one with a capture dir >
/// error. Bounded by the call budget.
async fn capture_session(
    state: &ClientState,
    ctx: &CallCtx,
    session: Option<&str>,
) -> Result<SessionEntry, ToolError> {
    if state.runtime_kind != RuntimeKind::Docker {
        return Err(capture_unavailable(
            "the local runtime has no launched sessions to capture",
            json!({"hint": "launch_session needs the docker runtime (workflows/10-local-install.md)"}),
        ));
    }
    let budget_room = room(ctx);
    if budget_room.is_zero() {
        return Err(ToolError::budget_exceeded(&ctx.budget));
    }
    let found = tokio::time::timeout(budget_room, async {
        match session {
            Some(name) => state
                .runtime
                .session_entry(name)
                .await
                .map_err(runtime_error)?
                .ok_or_else(|| {
                    invalid(format!(
                        "no cs2mcp session named {name:?} (session_status lists them)"
                    ))
                }),
            None => {
                let list = state.runtime.list_sessions().await.map_err(runtime_error)?;
                let running: Vec<SessionEntry> = list
                    .into_iter()
                    .filter(|e| e.running() && e.artifacts_dir.is_some())
                    .collect();
                pick_running(running)
            }
        }
    })
    .await;
    match found {
        Ok(result) => result,
        Err(_) => Err(ToolError::budget_exceeded(&ctx.budget)),
    }
}

/// Choose among running sessions without a name: one running session is that one (even without a
/// capture dir: `capture_dir_of` then says why); several need exactly one with a capture dir.
fn pick_running(mut running: Vec<SessionEntry>) -> Result<SessionEntry, ToolError> {
    match running.len() {
        0 => Err(capture_unavailable(
            "no running session to capture",
            json!({"hint": "launch_session first, or pass session: <cs2mcp-* container>"}),
        )),
        1 => Ok(running.remove(0)),
        n => {
            let with_capture: Vec<usize> = running
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    e.artifacts_dir
                        .as_ref()
                        .is_some_and(|a| a.join("capture").is_dir())
                })
                .map(|(i, _)| i)
                .collect();
            if let [only] = with_capture.as_slice() {
                return Ok(running.swap_remove(*only));
            }
            let names: Vec<&str> = running.iter().map(|e| e.name.as_str()).collect();
            Err(invalid(format!(
                "{n} sessions are running ({}): pass session: <container> to pick the capture",
                names.join(", ")
            ))
            .with_data(json!({"sessions": names})))
        }
    }
}

/// The capture dir of a session: `<artifacts>/capture`, which exists only for a session launched
/// with the layer. A stopped session has no live layer, so it is refused too.
fn capture_dir_of(entry: &SessionEntry) -> Result<PathBuf, ToolError> {
    if !entry.running() {
        return Err(capture_unavailable(
            format!(
                "session {} is not running (state {})",
                entry.name, entry.state
            ),
            json!({"session": entry.name, "state": entry.state, "hint": HINT_LAYER}),
        ));
    }
    let artifacts = entry.artifacts_dir.as_ref().ok_or_else(|| {
        capture_unavailable(
            format!("session {} has no host artifacts dir", entry.name),
            json!({"session": entry.name, "note": entry.note, "hint": HINT_LAYER}),
        )
    })?;
    let dir = artifacts.join("capture");
    if !dir.is_dir() {
        return Err(capture_unavailable(
            format!(
                "session {} was launched without capture (capture: false, or no valid CS2_MCP_VK_LAYER_DIR at launch)",
                entry.name
            ),
            json!({"session": entry.name, "captureDir": dir.display().to_string(), "hint": HINT_LAYER}),
        ));
    }
    Ok(dir)
}

fn request_id() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("shot-{ms}-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

/// With `--capture-dir` (a `cs2mcp-native` session) there is one session, `native`.
fn native_session(session: Option<&str>) -> Result<String, ToolError> {
    match session {
        None | Some(NATIVE_SESSION) => Ok(NATIVE_SESSION.to_string()),
        Some(other) => Err(invalid(format!(
            "this server captures the native session only (--capture-dir); there is no session {other:?}"
        ))),
    }
}

fn frame_error(
    e: FrameError,
    session: &str,
    hint: &str,
    dir: &Path,
    id: &str,
    timeout: Duration,
) -> ToolError {
    let base = json!({
        "session": session,
        "requestId": id,
        "captureDir": dir.display().to_string(),
    });
    match e.kind {
        FrameErrorKind::Timeout => {
            let mut data = base;
            data["timeoutMs"] = json!(timeout.as_millis() as u64);
            data["hint"] = json!(hint);
            capture_unavailable(format!("no frame from the capture layer: {e}"), data)
        }
        FrameErrorKind::LayerFailed => {
            let mut data = base;
            data["layerError"] = json!(e.message);
            ToolError::new("CaptureFailed", "CaptureFailed", e.message)
                .retry_safe(true)
                .with_data(data)
        }
        FrameErrorKind::Setup => {
            ToolError::new("CaptureFailed", "CaptureFailed", e.message).with_data(base)
        }
    }
}

fn capture_failed(message: impl Into<String>) -> ToolError {
    ToolError::new("CaptureFailed", "CaptureFailed", message).retry_safe(true)
}

/// A captured frame, as the tool reports it and as observe needs it.
pub struct Frame {
    pub output: ToolOutput,
    pub stats: ImageStats,
    pub ocr_text: Option<String>,
    pub session: String,
}

/// Request one frame from the session's layer, wait for it (at most `timeout_ms`, and at most the
/// call budget minus the margin), process it, optionally OCR it, and prune old captures. The one
/// path both `screenshot` and `observe` use.
pub async fn capture_frame(
    state: &ClientState,
    ctx: &CallCtx,
    session: Option<&str>,
    timeout_ms: u64,
    want_ocr: bool,
    image: InlineMode,
) -> Result<Frame, ToolError> {
    let (name, dir, hint) = match &state.capture_dir {
        Some(d) => (native_session(session)?, d.clone(), HINT_NATIVE),
        None => {
            let entry = capture_session(state, ctx, session).await?;
            let dir = capture_dir_of(&entry)?;
            (entry.name, dir, HINT_LAYER)
        }
    };
    let budget_room = room(ctx);
    if budget_room.is_zero() {
        return Err(ToolError::budget_exceeded(&ctx.budget));
    }
    let timeout = Duration::from_millis(timeout_ms).min(budget_room);
    let id = request_id();
    let png = vklayer::request_frame(&dir, &id, timeout)
        .await
        .map_err(|e| frame_error(e, &name, hint, &dir, &id, timeout))?;

    let layer = read_layer_result(&dir, &id)?;
    let owned = png.clone();
    let captured: Captured =
        tokio::task::spawn_blocking(move || process_capture_mode(&owned, false, image))
            .await
            .map_err(|e| ToolError::internal(format!("capture task failed: {e}")))??;
    let stats = captured.stats;

    let mut ocr_text = None;
    let mut ocr_meta = None;
    if want_ocr {
        match ocr_frame(&png, room(ctx)).await {
            Ok(result) => {
                ocr_text = Some(result.text.clone());
                ocr_meta =
                    Some(json!({"text": result.text, "psm": result.psm, "engine": result.engine}));
            }
            // OCR failing does not fail the capture: the frame is still returned, with the reason
            Err((kind, message)) => {
                ocr_meta = Some(json!({"error": {"type": kind, "message": message}}))
            }
        }
    }

    if let Err(e) = vklayer::cleanup(&dir, KEEP_CAPTURES).await {
        tracing::warn!("capture cleanup in {} failed: {e:#}", dir.display());
    }

    let mut meta = Map::new();
    meta.insert("session".into(), json!(name));
    meta.insert("requestId".into(), json!(id));
    meta.insert("captureDir".into(), json!(dir.display().to_string()));
    for key in ["pid", "exe", "presentIndex"] {
        if let Some(v) = layer.get(key) {
            meta.insert(key.into(), v.clone());
        }
    }
    meta.insert("layer".into(), layer);
    let mut output = finish_capture(state, &png, captured, meta);
    if let Some(m) = ocr_meta {
        output.json["ocr"] = m;
    }
    Ok(Frame {
        output,
        stats,
        ocr_text,
        session: name,
    })
}

fn read_layer_result(dir: &Path, id: &str) -> Result<Value, ToolError> {
    let path = dir.join(format!("{id}.json"));
    let bytes = std::fs::read(&path)
        .map_err(|e| capture_failed(format!("read {}: {e}", path.display())))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| capture_failed(format!("parse {}: {e}", path.display())))
}

/// OCR of a frame, capped at [`OCR_CAP`] and at `room`. Failure as `(error type, message)`.
async fn ocr_frame(png: &Path, room: Duration) -> Result<ocr::OcrResult, (String, String)> {
    if room.is_zero() {
        return Err((
            "ToolBudgetExceeded".into(),
            "no call budget left for OCR".into(),
        ));
    }
    ocr::ocr_png(png, OCR_PSM, OCR_CAP.min(room).min(OCR_TIMEOUT))
        .await
        .map_err(|e| match e {
            OcrError::Invalid(m) => ("InvalidParams".into(), m),
            OcrError::Unreadable(m) => ("CaptureFailed".into(), m),
            OcrError::Engine(m) => ("OcrFailed".into(), m),
        })
}

// ---------------------------------------------------------------------------------------------
// screenshot
// ---------------------------------------------------------------------------------------------

pub async fn screenshot(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let obj = object_args(&args, &["session", "timeoutMs", "ocr", "maxWidth"])?;
    let session = checked_session(&state, &args)?;
    let timeout_ms = opt_u64(obj, "timeoutMs", DEFAULT_TIMEOUT_MS, 1, MAX_TIMEOUT_MS)?;
    let want_ocr = opt_bool(obj, "ocr", false)?;
    let image = match obj.get("maxWidth") {
        None | Some(Value::Null) => InlineMode::Full,
        Some(_) => InlineMode::MaxWidth(opt_u64(obj, "maxWidth", 0, 16, 8192)? as u32),
    };
    let frame = capture_frame(
        &state,
        &ctx,
        session.as_deref(),
        timeout_ms,
        want_ocr,
        image,
    )
    .await?;
    Ok(frame.output)
}

// ---------------------------------------------------------------------------------------------
// observe
// ---------------------------------------------------------------------------------------------

/// The screen guess. `dialog` when the OCR text has a known modal string; `loading` when the frame
/// has few distinct colours (a blank or loading screen); otherwise `unknown`.
pub fn classify_screen(ocr_text: Option<&str>, unique_colors: Option<usize>) -> &'static str {
    if ocr_text.is_some_and(is_dialog_text) {
        return "dialog";
    }
    match unique_colors {
        Some(n) if n <= LOADING_MAX_COLORS => "loading",
        _ => "unknown",
    }
}

/// A modal dialog: one of [`DIALOG_PHRASES`], or an "OK" button line (a standalone `OK` line, or a
/// whole-word `OK` together with a [`DIALOG_CUES`] word).
pub fn is_dialog_text(text: &str) -> bool {
    let upper = text.to_uppercase().replace('\u{2019}', "'");
    if DIALOG_PHRASES.iter().any(|p| upper.contains(p)) {
        return true;
    }
    if text.lines().any(|l| l.trim() == "OK") {
        return true;
    }
    let has_ok_word = text
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w == "OK");
    has_ok_word && DIALOG_CUES.iter().any(|c| upper.contains(c))
}

/// One part's failure, as an entry of `errors[]`.
fn part_error(part: &str, e: &ToolError) -> Value {
    json!({
        "part": part,
        "type": e.type_,
        "message": e.message,
        "retrySafe": e.retry_safe,
        "data": e.data,
    })
}

struct ConsoleSample {
    json: Value,
    events: Vec<Value>,
    session: Option<String>,
}

/// The console part of observe: the last `n` lines, or the lines since `since`, plus checkpoint
/// events and the cursor for the next call.
async fn console_part(
    state: &ClientState,
    ctx: &CallCtx,
    session: Option<&str>,
    since: Option<Cursor>,
    n: usize,
) -> Result<ConsoleSample, ToolError> {
    let mut resolved = resolve_within(state, ctx, session, Purpose::Reader).await?;
    // a cursor names the file it came from: that wins over the resolved file (as console_wait does)
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
    let log = ConsoleLog::new(&resolved.path);
    let path = resolved.path.clone();
    let (lines, cursor, cursor_reset, new_lines) = match since {
        None => {
            let tail = log
                .tail(n, None)
                .await
                .map_err(|e| console_error(e, &path))?;
            (tail.lines, tail.cursor, false, None)
        }
        Some(c) => {
            let reset = match log.cursor().ok().flatten() {
                Some(now) => c.inode != now.inode || c.offset > now.offset,
                None => false,
            };
            let (all, next) = log
                .read_new_lines(Some(&c))
                .await
                .map_err(|e| console_error(ConsoleError::Io(e), &path))?
                .ok_or_else(|| console_error(ConsoleError::Missing, &path))?;
            let total = all.len();
            let keep = total.saturating_sub(n);
            (all[keep..].to_vec(), next, reset, Some(total))
        }
    };
    let re = Regex::new(CHECKPOINT_RE).expect("static checkpoint regex");
    let events: Vec<Value> = lines
        .iter()
        .filter_map(|line| {
            re.captures(line)
                .map(|c| json!({"type": "checkpoint", "name": &c[1], "line": line}))
        })
        .collect();
    let mut body = json!({
        "path": path.display().to_string(),
        "lines": lines,
        "cursor": cursor,
        "cursorReset": cursor_reset,
    });
    if let Some(total) = new_lines {
        body["newLines"] = json!(total);
    }
    Ok(ConsoleSample {
        json: describe(body, &resolved),
        events,
        session: resolved.session.clone(),
    })
}

/// Why the tick part did not run, or that it failed.
enum TickStop {
    Skipped(String),
    Failed(ToolError),
}

/// The bridge `game.tick` for `target`, if and only if the bridge is that session's: the same gate
/// `stop_session` uses (a running wine-tools session whose /app mount holds the discovery file).
/// Connects on demand; an unreachable bridge is a skip.
async fn bridge_tick(
    state: &ClientState,
    target: &str,
    timeout: Duration,
) -> Result<Value, TickStop> {
    let mut bridge = state.bridge.lock().await;
    crate::tools::runtime_tools::bridge_session_gate(state, target, &bridge)
        .await
        .map_err(TickStop::Skipped)?;
    bridge
        .ensure_connected()
        .await
        .map_err(|e| TickStop::Skipped(format!("bridge not reachable ({e})")))?;
    if !bridge.has("game.tick") {
        return Err(TickStop::Skipped(
            "bridge does not advertise game.tick".into(),
        ));
    }
    let deadline_ms = (timeout.as_millis() as u64).max(1);
    let resp = bridge
        .request("game.tick", json!({}), RequestOpts::deadline(deadline_ms))
        .await
        .map_err(|e| TickStop::Failed(e.into()))?;
    Ok(tag_tick(resp.result, target))
}

/// Label a tick result with where it came from.
fn tag_tick(result: Value, session: &str) -> Value {
    let mut v = match result {
        Value::Object(_) => result,
        other => json!({"result": other}),
    };
    v["source"] = json!("bridge");
    v["session"] = json!(session);
    v
}

pub async fn observe(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let obj = object_args(
        &args,
        &[
            "session",
            "capture",
            "ocr",
            "image",
            "consoleLines",
            "since",
        ],
    )?;
    let session = checked_session(&state, &args)?;
    let want_capture = opt_bool(obj, "capture", true)?;
    let want_ocr = opt_bool(obj, "ocr", false)?;
    let image = match obj.get("image") {
        None | Some(Value::Null) => InlineMode::MaxWidth(SMALL_INLINE_WIDTH),
        Some(Value::String(s)) => match s.as_str() {
            "inline" => InlineMode::Full,
            "small" => InlineMode::MaxWidth(SMALL_INLINE_WIDTH),
            "none" => InlineMode::None,
            _ => return Err(invalid("image must be \"inline\", \"small\" or \"none\"")),
        },
        Some(_) => return Err(invalid("image must be \"inline\", \"small\" or \"none\"")),
    };
    let console_lines = opt_u64(
        obj,
        "consoleLines",
        OBSERVE_CONSOLE_DEFAULT,
        0,
        OBSERVE_CONSOLE_MAX,
    )? as usize;
    let since = match obj.get("since") {
        None | Some(Value::Null) => None,
        Some(v) => Some(serde_json::from_value::<Cursor>(v.clone()).map_err(|e| {
            invalid(format!(
                "since must be {{\"inode\": u64, \"offset\": u64}}: {e}"
            ))
        })?),
    };

    // Every part is independent: a failure is recorded in errors[] and the others still run.
    let mut errors: Vec<Value> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut images: Vec<Image> = Vec::new();
    // `--capture-dir`: the frame and the tick are the native session's; its console is
    // `--console-log` (the console part takes no session then)
    let native = state.capture_dir.is_some();
    let mut session = if native { Some(NATIVE_SESSION.to_string()) } else { session };
    let mut stats: Option<ImageStats> = None;
    let mut ocr_text: Option<String> = None;

    // 1. the frame (<= FRAME_CAP) and its OCR (<= OCR_CAP)
    let capture = if want_capture {
        match capture_frame(
            &state,
            &ctx,
            session.as_deref(),
            FRAME_CAP.as_millis() as u64,
            want_ocr,
            image,
        )
        .await
        {
            Ok(frame) => {
                session = Some(frame.session.clone());
                stats = Some(frame.stats);
                ocr_text = frame.ocr_text.clone();
                images = frame.output.images;
                frame.output.json
            }
            Err(e) => {
                errors.push(part_error("capture", &e));
                Value::Null
            }
        }
    } else {
        notes.push("capture: false: no screenshot taken".into());
        Value::Null
    };
    if want_capture && !want_ocr {
        notes.push("ocr: false: dialog detection needs ocr: true".into());
    }

    // 2. the console part runs before the tick, so a slow bridge never starves it
    let (console, events) =
        match console_part(&state, &ctx, session.as_deref().filter(|_| !native), since, console_lines).await {
            Ok(sample) => {
                if session.is_none() {
                    session = sample.session.clone();
                }
                (sample.json, sample.events)
            }
            Err(e) => {
                errors.push(part_error("console", &e));
                (Value::Null, Vec::new())
            }
        };

    // 3. game.tick (<= TICK_CAP), only for the session the bridge belongs to
    let tick = match session.clone() {
        None => {
            notes.push(
                "game.tick skipped: no session to tick (pass session: <wine-tools container>)"
                    .into(),
            );
            Value::Null
        }
        Some(target) => {
            let tick_room = TICK_CAP.min(room(&ctx));
            if tick_room.is_zero() {
                errors.push(part_error("tick", &ToolError::budget_exceeded(&ctx.budget)));
                Value::Null
            } else {
                match tokio::time::timeout(tick_room, bridge_tick(&state, &target, tick_room)).await
                {
                    Ok(Ok(v)) => v,
                    Ok(Err(TickStop::Skipped(reason))) => {
                        notes.push(format!("game.tick skipped: {reason}"));
                        Value::Null
                    }
                    Ok(Err(TickStop::Failed(e))) => {
                        errors.push(part_error("tick", &e));
                        Value::Null
                    }
                    Err(_) => {
                        errors.push(part_error(
                            "tick",
                            &ToolError::new(
                                "TickTimeout",
                                "TickTimeout",
                                format!(
                                    "game.tick did not answer within {} ms",
                                    tick_room.as_millis()
                                ),
                            )
                            .retry_safe(true),
                        ));
                        Value::Null
                    }
                }
            }
        }
    };

    // 4. the screen guess from what the frame gave us
    let screen = classify_screen(ocr_text.as_deref(), stats.map(|s| s.unique_colors));

    let out = json!({
        "session": session,
        "screen": screen,
        "screenBasis": {
            "uniqueColors": stats.map(|s| s.unique_colors),
            "ocrText": ocr_text.is_some(),
        },
        "capture": capture,
        "tick": tick,
        "console": console,
        "events": events,
        "errors": errors,
        "notes": notes,
    });
    Ok(ToolOutput {
        json: out,
        images,
        ..Default::default()
    })
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn dialog_phrases_and_ok_are_recognised() {
        assert!(is_dialog_text(
            "Settings Recommendation\nYour GPU reports 0 Hz"
        ));
        assert!(is_dialog_text("DON’T SHOW AGAIN   OK"));
        // a standalone OK line is a button
        assert!(is_dialog_text("Some text\n  OK  \n"));
        // a whole-word OK needs a dialog cue
        assert!(is_dialog_text("Cancel  OK"));
        assert!(!is_dialog_text("Round 3  OK")); // no cue, not a standalone line
        assert!(!is_dialog_text("okay then, this is ok-ish")); // lower-case words are not buttons
        assert!(!is_dialog_text("Round 3  Score 2:1"));
    }

    #[test]
    fn screen_is_dialog_then_loading_then_unknown() {
        assert_eq!(
            classify_screen(Some("Settings Recommendation"), Some(9000)),
            "dialog"
        );
        assert_eq!(classify_screen(None, Some(1)), "loading");
        assert_eq!(
            classify_screen(Some("Round 3"), Some(LOADING_MAX_COLORS)),
            "loading"
        );
        assert_eq!(classify_screen(Some("Round 3"), Some(9000)), "unknown");
        assert_eq!(classify_screen(None, None), "unknown");
    }

    #[test]
    fn checkpoint_regex_takes_the_name() {
        let re = Regex::new(CHECKPOINT_RE).unwrap();
        let line = "TESTSCRIPT: CheckPoint -> 'gt_pass_move_jump'";
        assert_eq!(&re.captures(line).unwrap()[1], "gt_pass_move_jump");
        assert!(re.captures("TESTSCRIPT: CheckPoint -> ").is_none());
    }

    #[test]
    fn request_ids_are_layer_safe() {
        let id = request_id();
        assert!(vklayer::valid_request_id(&id), "{id}");
        assert_ne!(id, request_id());
    }

    #[test]
    fn tick_is_labelled_with_its_source() {
        let v = tag_tick(json!({"tick": 7}), "cs2mcp-wine-tools-1");
        assert_eq!(v["source"], json!("bridge"));
        assert_eq!(v["session"], json!("cs2mcp-wine-tools-1"));
        assert_eq!(v["tick"], json!(7));
        assert_eq!(tag_tick(json!(3), "s")["result"], json!(3));
    }

    #[test]
    fn running_sessions_prefer_the_one_with_a_capture_dir() {
        // scratch under the crate's target dir, not /tmp
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/test-scratch")
            .join(format!("cs2mcp-pick-{}", std::process::id()));
        let with = root.join("with");
        let without = root.join("without");
        std::fs::create_dir_all(with.join("capture")).unwrap();
        std::fs::create_dir_all(&without).unwrap();
        let entry = |name: &str, dir: &Path| SessionEntry {
            name: name.into(),
            kind: "linux-client".into(),
            state: "running".into(),
            owned: true,
            artifacts_dir: Some(dir.to_path_buf()),
            console_log: None,
            app_dir: None,
            note: None,
            map: None,
            addon: None,
        };
        let picked = pick_running(vec![
            entry("cs2mcp-linux-client-a", &without),
            entry("cs2mcp-linux-client-b", &with),
        ])
        .unwrap();
        assert_eq!(picked.name, "cs2mcp-linux-client-b");
        let err = pick_running(vec![
            entry("cs2mcp-linux-client-a", &without),
            entry("cs2mcp-linux-client-c", &without),
        ])
        .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments");
        let _ = std::fs::remove_dir_all(&root);
    }
}
