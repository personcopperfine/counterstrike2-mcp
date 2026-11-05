//! `run_testscript`, `test_status`, `test_wait` (workflows/04 step 2). Staging and judging live in
//! `testscript/`; the session comes from the docker runtime (`preflight`, then `launch_within`: the
//! launch_session path with wine-tools and `-testscript /app/vtest/<name>.txt`). Nothing here shells
//! out. The run's background task (`testscript::run::spawn_drive`) outlives the call.
use crate::state::ClientState;
use crate::testscript::judge;
use crate::testscript::run::{spawn_drive, Run, RunState, RunTable, Session, Timing};
use crate::testscript::{stage, unstage, Suite, LIBRARY};
use crate::tools::runtime_tools::{lock_budget, runtime_error};
use crate::video::judge::MovieExpect;
use crate::video::MovieFormat;
use cs2_mcp_common::runtime::{LaunchSpec, SessionKind};
use cs2_mcp_common::tools::{CallCtx, ToolError, ToolOutput};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DEFAULT_TIMEOUT_MS: u64 = 300_000;
pub const MAX_TIMEOUT_MS: u64 = 900_000;
const WAIT_DEFAULT_MS: u64 = 60_000;
/// Kept from the call budget for the reply (console_wait uses the same margin).
const BUDGET_MARGIN: Duration = Duration::from_millis(500);
/// A run needs time to stage and launch; with less call budget left it is refused up front.
const RUN_MIN_BUDGET: Duration = Duration::from_secs(5);
const WAIT_POLL: Duration = Duration::from_millis(100);

pub fn run_schema() -> Value {
    let library: Vec<&str> = LIBRARY.iter().map(|(n, _)| *n).collect();
    json!({
        "type": "object",
        "properties": {
            "suite": {"type": "string", "enum": library},
            "inline": {"type": "string", "description": "script text; ';' splits lines, `mark <name>` is a checkpoint barrier"},
            "judge": {"type": "string", "enum": judge::preset_names()},
            "demo": {"type": "boolean", "default": false},
            "timeoutMs": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS, "default": DEFAULT_TIMEOUT_MS}
        }
    })
}

pub fn status_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"runId": {"type": "string"}},
        "required": ["runId"]
    })
}

pub fn wait_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "runId": {"type": "string"},
            "timeoutMs": {"type": "integer", "minimum": 0, "default": WAIT_DEFAULT_MS}
        },
        "required": ["runId"]
    })
}

fn invalid(msg: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(msg)
}

fn opt_str(args: &Value, key: &str) -> Result<Option<String>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(invalid(format!("{key} must be a string"))),
    }
}

fn opt_bool(args: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(invalid(format!("{key} must be a boolean"))),
    }
}

fn inline_name() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("inline-{ms}")
}

/// A `KindBusy` from the runtime names the run that owns the busy container, when one of this
/// server's runs owns it (`data.runId`). Other refusals pass through unchanged.
fn busy_with_run(mut err: ToolError, runs: &Mutex<RunTable>) -> ToolError {
    if err.type_ != "KindBusy" {
        return err;
    }
    let container = err
        .data
        .get("container")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(container) = container {
        let owner = runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .for_container(&container);
        if let Some(run) = owner {
            err.data.insert("runId".into(), json!(run.id));
        }
    }
    err
}

pub async fn run_testscript(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let suite_arg = opt_str(&args, "suite")?;
    let inline_arg = opt_str(&args, "inline")?;
    let judge_name = opt_str(&args, "judge")?;
    if let Some(j) = &judge_name {
        if !judge::known_preset(j) {
            return Err(invalid(format!(
                "unknown judge preset {j:?} (known: {})",
                judge::preset_names().join(", ")
            )));
        }
    }
    let demo = opt_bool(&args, "demo")?.unwrap_or(false);
    let timeout_ms = match args.get("timeoutMs") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_MS,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_TIMEOUT_MS).contains(n))
            .ok_or_else(|| invalid("timeoutMs must be an integer in 1..=900000"))?,
    };
    let inline = inline_arg.is_some();
    let suite = match (suite_arg, inline_arg) {
        (Some(name), None) => Suite::library(&name).ok_or_else(|| {
            let known: Vec<&str> = LIBRARY.iter().map(|(n, _)| *n).collect();
            invalid(format!(
                "unknown suite {name:?} (library: {})",
                known.join(", ")
            ))
        })?,
        (None, Some(body)) => Suite::inline(&inline_name(), &body),
        _ => {
            return Err(invalid(
                "give exactly one of suite (library name) or inline (script text)",
            ))
        }
    };
    let req = StartRequest {
        suite,
        inline,
        judge: judge_name,
        demo,
        timeout_ms,
        movie: None,
    };
    start_run(state, ctx, req).await.map(Into::into)
}

/// A recorder job for `demo_to_video`: the run's output is checked and collected as a movie.
#[derive(Debug, Clone)]
pub struct MovieRequest {
    pub name: String,
    pub format: MovieFormat,
    pub framerate: u32,
    pub ffprobe: Option<PathBuf>,
    pub ffmpeg: Option<PathBuf>,
    pub count_frames: bool,
}

/// What a run needs: the staged suite plus the judge, demo and timeout options.
#[derive(Debug, Clone)]
pub struct StartRequest {
    pub suite: Suite,
    /// an inline suite is staged under a generated name: unstage it when the run is refused
    pub inline: bool,
    pub judge: Option<String>,
    pub demo: bool,
    pub timeout_ms: u64,
    pub movie: Option<MovieRequest>,
}

/// Stage the suite, launch the wine-tools session with it and start the background run. Refuses
/// (cap, one session per app dir) before staging, so a refused call leaves nothing behind. Returns
/// the run's status document (`runId`, `container`, `stagedPath`, ...). Shared by `run_testscript`
/// and `demo_to_video`.
pub async fn start_run(
    state: ClientState,
    ctx: CallCtx,
    req: StartRequest,
) -> Result<Value, ToolError> {
    let StartRequest {
        suite,
        inline,
        judge: judge_name,
        demo,
        timeout_ms,
        movie,
    } = req;
    // a bad script is refused before anything is written or launched
    suite.render().map_err(|e| invalid(format!("{e:#}")))?;
    if ctx.budget.remaining() < RUN_MIN_BUDGET {
        return Err(ToolError::budget_exceeded(&ctx.budget));
    }

    let app_root = state.runtime.wine_app_dir();
    if !app_root.is_dir() {
        return Err(ToolError::new(
            "AppNotFound",
            "AppNotFound",
            "the wine-tools app tree is missing",
        )
        .retry_safe(true)
        .with_data(json!({"appRoot": app_root.display().to_string()})));
    }
    let spec = LaunchSpec {
        kind: SessionKind::WineTools { addon: None },
        testscript: Some(suite.name.clone()),
        map: None,
        args: Vec::new(),
        headless: false,
        gpus: true,
        container_name: None,
        // testscript runs capture too: a judge can read the frame it stopped on
        capture: true,
        bridge: true,
    };
    // refuse (cap, one wine session per app dir) before staging: a refused call writes nothing
    state
        .runtime
        .preflight(&spec)
        .await
        .map_err(|e| busy_with_run(runtime_error(e), &state.runs))?;

    // evidence older than this instant is not this run's (the judge copies only newer files)
    let since = SystemTime::now();
    let movie_expect = movie.map(|m| MovieExpect {
        name: m.name,
        format: m.format,
        framerate: m.framerate,
        game_dir: app_root.join("game/csgo"),
        since,
        ffprobe: m.ffprobe,
        ffmpeg: m.ffmpeg,
        count_frames: m.count_frames,
        probe_timeout: None,
    });
    let staged = stage(&app_root, &suite)
        .map_err(|e| ToolError::internal(format!("staging the suite failed: {e:#}")))?;
    let refused = |err: ToolError| {
        // a generated or movie suite name is ours: remove it. A library suite is kept.
        if inline || movie_expect.is_some() {
            unstage(&staged);
        }
        busy_with_run(err, &state.runs)
    };
    let launched = match state.runtime.launch_within(&spec, lock_budget(&ctx)).await {
        Ok(l) => l,
        Err(e) => return Err(refused(runtime_error(e))),
    };
    if launched.reused {
        // identical session still running (preflight should have caught it); nothing new started
        let err = ToolError::new(
            "KindBusy",
            "KindBusy",
            format!("{} is already running this suite", launched.container),
        )
        .retry_safe(true)
        .with_data(json!({
            "kind": "wine-tools",
            "container": launched.container,
            "shared": "app dir",
        }));
        return Err(refused(err));
    }
    let console_log = launched
        .paths
        .console_log
        .clone()
        .ok_or_else(|| ToolError::internal("the wine-tools session has no console.log path"))?;

    let run = {
        let mut table = state.runs.lock().unwrap_or_else(|e| e.into_inner());
        let mut run = Run::new(
            table.next_id(),
            launched.container.clone(),
            suite.name.clone(),
            judge_name.clone(),
            demo,
            staged.clone(),
            launched.artifacts_dir.clone(),
        );
        run.movie = movie_expect;
        let run = Arc::new(run);
        table.insert(run.clone());
        run
    };
    let session = Session {
        runtime: state.runtime.clone(),
        container: launched.container.clone(),
        console_log,
        since,
    };
    spawn_drive(
        run.clone(),
        session,
        Timing::new(Duration::from_millis(timeout_ms)),
    );
    Ok(json!({
        "runId": run.id,
        "container": run.container,
        "stagedPath": staged.display().to_string(),
        "artifactsDir": run.artifacts_dir.display().to_string(),
        "suite": run.suite,
        "judge": run.judge,
        "demo": demo,
        "timeoutMs": timeout_ms,
        "state": "running",
    }))
}

fn find_run(state: &ClientState, args: &Value) -> Result<Arc<Run>, ToolError> {
    let id = args
        .get("runId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("runId must be a non-empty string"))?;
    state
        .runs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .ok_or_else(|| {
            ToolError::new(
                "UnknownRun",
                "UnknownRun",
                format!("no run {id} in this server process"),
            )
            .retry_safe(true)
            .with_data(json!({"runId": id}))
        })
}

/// The status document shared by `test_status` and `test_wait`.
fn status_value(run: &Run) -> Value {
    let o = run.snapshot();
    let elapsed_ms = o.finished_ms.unwrap_or_else(|| run.elapsed_ms());
    let mut v = json!({
        "runId": run.id,
        "state": o.state.as_str(),
        "container": run.container,
        "suite": run.suite,
        "judge": run.judge,
        "demo": run.demo,
        "stagedPath": run.staged_path.display().to_string(),
        "artifactsDir": run.artifacts_dir.display().to_string(),
        "elapsedMs": elapsed_ms,
        "exitCode": o.exit_code,
        "timeline": o.timeline,
        "timelineTruncated": o.timeline_truncated,
    });
    // checks that were not evaluated (a tool absent on the host): never passed, never failed
    let skipped: Vec<&str> = o
        .checks
        .iter()
        .flatten()
        .filter(|c| c.skipped)
        .map(|c| c.name.as_str())
        .collect();
    v["skipped"] = json!(skipped);
    if let Some(checks) = &o.checks {
        v["checks"] = json!(checks);
    }
    if o.state != RunState::Running {
        v["artifacts"] = json!(o.artifacts);
    }
    if let Some(e) = &o.error {
        v["error"] = json!(e);
    }
    v
}

pub async fn test_status(
    state: ClientState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let run = find_run(&state, &args)?;
    Ok(status_value(&run).into())
}

pub async fn test_wait(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let run = find_run(&state, &args)?;
    let requested = match args.get("timeoutMs") {
        None | Some(Value::Null) => WAIT_DEFAULT_MS,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| invalid("timeoutMs must be a non-negative integer"))?,
    };
    let started = Instant::now();
    let mut clamped = false;
    let wait = if run.state() != RunState::Running {
        Duration::ZERO
    } else {
        // never wait past the call budget (console_wait's rule): too little left is the budget error
        let remaining = ctx.budget.remaining();
        if remaining < BUDGET_MARGIN {
            return Err(ToolError::budget_exceeded(&ctx.budget));
        }
        let cap = remaining - BUDGET_MARGIN;
        let asked = Duration::from_millis(requested);
        clamped = asked > cap;
        asked.min(cap)
    };
    while run.state() == RunState::Running && started.elapsed() < wait {
        tokio::time::sleep(WAIT_POLL.min(wait.saturating_sub(started.elapsed()))).await;
    }
    let pending = run.state() == RunState::Running;
    let mut v = status_value(&run);
    v["waitedMs"] = json!(started.elapsed().as_millis() as u64);
    v["timeoutMs"] = json!(requested);
    v["clampedToBudget"] = json!(clamped);
    v["pending"] = json!(pending);
    if pending {
        v["next"] = json!("test_wait {runId}");
    }
    Ok(v.into())
}
