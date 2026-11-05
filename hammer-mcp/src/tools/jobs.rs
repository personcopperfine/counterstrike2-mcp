//! The generic job tools over the bridge's job model (`tools/bridge_job.rs`): `compile_asset`
//! starts a resourcecompiler job for any allowlisted asset and returns at once, `job_status` polls
//! (or lists) jobs and `job_cancel` ends one. The calling model never has to know how the compiler
//! runs: a job id goes in, a state comes out.
//!
//! Two id spaces, one answer shape per space:
//!
//! * a bridge job id (`j<N>-<nonce>` today; **opaque, always pass it back verbatim**) — a **bridge job** (what `compile_asset` returns; `compile_map`'s bridge route runs on
//!   one too and reports it as `bridgeJobId`): `job_status` answers the bridge's status with the
//!   host path of its log added (`log.host`);
//! * `c<N>` — a **`compile_map` job** (either route): `job_status`/`job_cancel` answer exactly what
//!   `compile_map {jobId}` / `compile_map {jobId, cancel:true}` answer (with the map-specific vpk
//!   verdict).
//!
//! `compile_asset` has no podman fallback: it needs the bridge's `job.start` (`RuntimeUnavailable`
//! naming it otherwise). Like every compile it dies with the server: the shutdown cancels the
//! bridge jobs this server started (`compile_map::shutdown_cleanup`).
use super::bridge_job as job;
use super::compile_map;
use crate::state::HammerState;
use crate::vmdl::VmdlError;
use cs2_mcp_common::tools::{
    Budget, CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Map, Value};
use std::future::Future;
use std::time::{Duration, Instant};

/// The longest `waitMs` a job poll accepts (the tool budget cuts it shorter in practice).
pub const WAIT_MAX_MS: u64 = 600_000;
/// The `waitMs` of a job poll that names none: a bare `{jobId}` waits (an agent that may not sleep
/// once polled 2019 times at 3 ms each); `waitMs:0` is the explicit "answer at once".
pub const WAIT_DEFAULT_MS: u64 = 30_000;
/// How often a waiting poll re-asks for the job's state. The job table has no completion signal
/// to wait on (a bridge job ends inside the bridge, and only `job.status` tells), so the wait is a
/// poll loop INSIDE the server with a hard deadline: the calling model makes one call, not 2000.
pub const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Left of the tool budget after a wait, so the reply can still be built and sent.
const WAIT_MARGIN: Duration = Duration::from_millis(1_500);

/// The `waitMs` property both job polls share (schema fragment).
pub fn wait_ms_schema() -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "maximum": WAIT_MAX_MS,
        "description": "wait for the job up to this long (default 30000, 0 = answer now); reply has done; repeat while done is false"
    })
}

/// `waitMs` from the arguments (absent/null = [`WAIT_DEFAULT_MS`]).
pub fn wait_ms_arg(args: &Value) -> Result<u64, ToolError> {
    match args.get("waitMs") {
        None | Some(Value::Null) => Ok(WAIT_DEFAULT_MS),
        Some(value) => match value.as_u64() {
            Some(ms) if ms <= WAIT_MAX_MS => Ok(ms),
            _ => Err(ToolError::invalid_arguments(format!(
                "waitMs must be an integer in 0..={WAIT_MAX_MS}, got {value}"
            ))),
        },
    }
}

/// Answer a job poll, waiting up to `wait_ms` for the job to end. `poll` is one fresh view of the
/// job (`state` != "running" means ended). Adds `done`, `waitedMs` (and `budgetClamped` when the
/// tool budget, not the caller, cut the wait); a job still running gets a `next` step that calls
/// `tool` again with waitMs. Errors from `poll` end the wait.
pub async fn wait_for_job<F, Fut>(
    budget: &Budget,
    wait_ms: u64,
    interval: Duration,
    tool: &str,
    job_id: &str,
    mut poll: F,
) -> Result<ToolOutput, ToolError>
where
    F: FnMut(u64) -> Fut,
    Fut: Future<Output = Result<ToolOutput, ToolError>>,
{
    let started = Instant::now();
    let wanted = Duration::from_millis(wait_ms);
    let allowed = wanted.min(budget.remaining().saturating_sub(WAIT_MARGIN));
    let clamped = allowed < wanted;
    // each poll's request deadline is what the tool budget leaves (minus the reply margin), so a
    // bridge that stalls on the last poll cannot push the call past its budget
    let left_ms = |budget: &Budget| budget.remaining().saturating_sub(WAIT_MARGIN).as_millis() as u64;
    let mut out = poll(left_ms(budget).max(500)).await?;
    while !is_done(&out.json) && started.elapsed() < allowed {
        tokio::time::sleep(interval.min(allowed.saturating_sub(started.elapsed()))).await;
        out = poll(left_ms(budget).max(500)).await?;
    }
    let done = is_done(&out.json);
    if let Some(map) = out.json.as_object_mut() {
        map.insert("done".into(), json!(done));
        map.insert("waitedMs".into(), json!(started.elapsed().as_millis() as u64));
        if clamped {
            map.insert("budgetClamped".into(), json!(true));
        }
    }
    if !done {
        out = out.with_next_args(
            tool,
            "the job is still running: call again with waitMs to wait for it instead of polling \
             (job_cancel ends it)",
            json!({"jobId": job_id, "waitMs": 30_000}),
        );
    }
    Ok(out)
}

fn is_done(view: &Value) -> bool {
    view.get("state").and_then(Value::as_str) != Some("running")
}

/// The extensions the bridge's `job.start` allowlists.
pub const ASSET_EXTENSIONS: [&str; 11] = [
    "vmap", "vmdl", "vpcf", "vsndevts", "vmat", "vtex", "vsnd", "vsmart", "vanim", "vmks", "js",
];

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "compile_asset",
            "Start the resource compiler on ONE asset of the active addon (vmap vmdl vpcf vsndevts vmat vtex vsnd vsmart vanim vmks js); returns {jobId, state:\"running\"} at once. \
             Poll with job_status {jobId, waitMs} until done:true; success only if state is ok (failLines/logTail say why not). Not for: maps (compile_map adds the dirty/in-game gates and the vpk verdict). \
             path: addon-relative, content-relative (csgo_addons/<addon>/...) or a host/Windows path inside the addon. One compile at a time (EngineBusy names the running job).",
            Writer,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "addon-relative, content-relative or absolute path inside the addon"
                },
                "game": {
                    "type": "boolean",
                    "description": "pass -game <gamedir>/csgo; default false for .vmap, else true"
                },
                "flags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "allowlist: -nav -fshallow -quiet -unbufferedio -noassert -nop4 -f; -maxtextureres N / -threads N as two tokens"
                },
                "timeoutMs": {
                    "type": "integer",
                    "minimum": job::BRIDGE_TIMEOUT_MIN_MS,
                    "maximum": job::BRIDGE_TIMEOUT_MAX_MS,
                    "description": "job deadline (state timeout); default 1500000"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })),
        compile_asset,
    );
    r.register(
        ToolSpec::new(
            "job_status",
            "Status of a compile job (compile_asset or compile_map): state running|ok|failed|cancelled|timeout, elapsedMs, summary, failLines, logTail. \
             jobId c<N> is a compile_map job (same answer as compile_map {jobId}); any other id is a bridge job id, pass it verbatim. No jobId lists both. waitMs waits for the end instead of polling; repeat while done is false; ok only if state is ok.",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "jobId": {
                    "type": "string",
                    "description": "bridge job id (verbatim) or c<N>; omit to list"
                },
                "waitMs": wait_ms_schema()
            },
            "additionalProperties": false
        })),
        job_status,
    );
    r.register(
        ToolSpec::new(
            "job_cancel",
            "End a compile job (bridge job id or c<N>) and its whole process tree. Returns the final state; cancelled:false if it had already finished.",
            Writer,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "jobId": {"type": "string", "description": "bridge job id or c<N>"}
            },
            "required": ["jobId"],
            "additionalProperties": false
        })),
        job_cancel,
    );
}

/// The job tools' order after [`super::sounds::SOUND_TOOL_ORDER`] (contract tests pin it).
pub const JOB_TOOL_ORDER: [&str; 3] = ["compile_asset", "job_status", "job_cancel"];

fn path_error(error: VmdlError) -> ToolError {
    match error {
        VmdlError::Path(message) if message.contains("no host mapping") => {
            ToolError::new("PathNotMapped", "PathNotMapped", message)
                .with_code(-32602)
                .retry_safe(true)
        }
        VmdlError::Path(message) if message.contains("cannot be read") => {
            ToolError::new("AssetNotFound", "AssetNotFound", message)
                .with_code(-32014)
                .retry_safe(true)
        }
        VmdlError::Path(message) => ToolError::new("InvalidPath", "InvalidPath", message)
            .with_code(-32602)
            .retry_safe(true),
        other => ToolError::internal(other.to_string()),
    }
}

/// A Windows or absolute host path (everything else is addon-/content-relative).
fn absolute_ish(input: &str) -> bool {
    let bytes = input.as_bytes();
    input.starts_with('/')
        || input.starts_with("\\\\")
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// The asset to compile: `(extension, addon-relative path, bridge input, host path)`. The file
/// must exist inside `content/csgo_addons/<addon>/` (the addon content tree: nothing outside it is
/// ever handed to the compiler).
fn resolve_asset(
    state: &HammerState,
    raw: &str,
) -> Result<(String, String, String, std::path::PathBuf), ToolError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ToolError::invalid_arguments("path must be a non-empty string"));
    }
    let file = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let ext = file
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    if !ASSET_EXTENSIONS.contains(&ext.as_str()) {
        return Err(ToolError::invalid_arguments(format!(
            "{file:?} is not a compilable asset: the extension must be one of {ASSET_EXTENSIONS:?}"
        ))
        .with_data(json!({"allowed": ASSET_EXTENSIONS})));
    }
    let normalised = raw.replace('\\', "/");
    let candidate = if absolute_ish(raw) || normalised.starts_with("csgo_addons/") {
        raw.to_string()
    } else {
        format!("csgo_addons/{}/{}", state.addon(), normalised.trim_start_matches("./"))
    };
    let host = state
        .content()
        .resolve_read_ext(&candidate, &state.path_map, &ext)
        .map_err(path_error)?;
    if !state.content().in_addon(&host) {
        return Err(ToolError::new(
            "WriteOutsideAddon",
            "WriteOutsideAddon",
            format!(
                "{} is outside content/csgo_addons/{}: only assets of the active addon can be \
                 compiled",
                host.display(),
                state.addon()
            ),
        )
        .with_code(-32602)
        .retry_safe(true));
    }
    let Some(addon_rel) = state.content().addon_rel(&host) else {
        return Err(ToolError::invalid_arguments(format!(
            "{} is not inside the active addon content tree",
            host.display()
        )));
    };
    let input = format!("csgo_addons/{}/{}", state.addon(), addon_rel);
    Ok((ext, addon_rel, input, host))
}

/// `compile_asset {path, game?, flags?, timeoutMs?}`.
pub async fn compile_asset(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::invalid_arguments("path (string) is required"))?;
    let (ext, addon_rel, input, _host) = resolve_asset(&state, path)?;
    let game = match args.get("game") {
        None | Some(Value::Null) => ext != "vmap",
        Some(Value::Bool(flag)) => *flag,
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "game must be a boolean, got {other}"
            )))
        }
    };
    let flags: Vec<String> = match args.get("flags") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_string).ok_or_else(|| {
                    ToolError::invalid_arguments("flags must be an array of strings")
                })
            })
            .collect::<Result<_, _>>()?,
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "flags must be an array of strings, got {other}"
            )))
        }
    };
    let timeout_ms = match args.get("timeoutMs") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64() {
            Some(ms) if (job::BRIDGE_TIMEOUT_MIN_MS..=job::BRIDGE_TIMEOUT_MAX_MS).contains(&ms) => {
                Some(ms)
            }
            _ => {
                return Err(ToolError::invalid_arguments(format!(
                    "timeoutMs must be an integer in {}..={}, got {value}",
                    job::BRIDGE_TIMEOUT_MIN_MS,
                    job::BRIDGE_TIMEOUT_MAX_MS
                )))
            }
        },
    };
    let probe = job::probe(&state).await;
    if !probe.available() {
        return Err(job::unavailable(&probe, "compile_asset"));
    }
    let reply = job::start_with_timeout(&state, &input, game, &flags, timeout_ms).await?;
    let Some(job_id) = reply.get("jobId").and_then(Value::as_str).map(str::to_string) else {
        return Err(ToolError::internal(format!(
            "the bridge's job.start reply carries no jobId: {reply}"
        )));
    };
    let log = job::host_log(&state, reply.get("log")).map(|p| p.display().to_string());
    let started: ToolOutput = json!({
        "jobId": job_id,
        "state": "running",
        "route": "bridge",
        "kind": "resourcecompiler",
        "path": addon_rel,
        "input": input,
        "game": game,
        "flags": flags,
        "log": log,
        "logRelative": reply.get("log").and_then(|l| l.get("relative")).cloned(),
        "command": reply.get("command").cloned().unwrap_or(Value::Null),
        "startedAtMs": reply.get("startedAtMs").cloned().unwrap_or(Value::Null),
    })
    .into();
    let started = started.with_next_args(
        "job_status",
        "pass waitMs to wait for the job instead of polling (job_cancel ends it)",
        json!({"jobId": job_id, "waitMs": 30_000}),
    );
    Ok(if ext == "vmap" {
        started.with_next("compile_map", "for a map, this gives the vpk verdict instead")
    } else {
        started
    })
}

/// A bridge `job.status` reply with the host log path and the compile_map job it belongs to. The
/// status is also folded into that job's store entry (like a compile_map poll would), and a
/// terminal state drops the id from the shutdown's cancel list.
fn bridge_view(state: &HammerState, status: Value) -> Value {
    let bridge_id = status.get("jobId").and_then(Value::as_str).map(str::to_string);
    if let Some(bridge_id) = &bridge_id {
        if job::is_terminal(job::state_of(&status)) {
            state.compile_jobs.forget_asset(bridge_id);
        }
        if let Some(id) = state.compile_jobs.id_for_bridge(bridge_id) {
            compile_map::fold_bridge_status(state, &id, &status);
        }
    }
    let mut view = job::with_host_log(state, status);
    if let Some(bridge_id) = bridge_id {
        if let Some(id) = state.compile_jobs.id_for_bridge(&bridge_id) {
            view["compileMapJobId"] = json!(id);
        }
    }
    view
}

#[derive(Clone, Copy)]
enum IdKind {
    CompileMap,
    Bridge,
}

fn id_kind(id: &str) -> Result<IdKind, ToolError> {
    // compile_map ids are `c<N>`; bridge ids are opaque (`j<N>-<nonce>`), recognised by the `j`
    let digits = |rest: &str| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    let opaque = |rest: &str| {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    match (id.strip_prefix('c'), id.strip_prefix('j')) {
        (Some(rest), _) if digits(rest) => Ok(IdKind::CompileMap),
        (_, Some(rest)) if opaque(rest) => Ok(IdKind::Bridge),
        _ => Err(ToolError::invalid_arguments(format!(
            "unknown jobId {id:?}: ids look like j<N> (bridge job, from compile_asset) or c<N> \
             (compile_map job)"
        ))),
    }
}

fn job_id_arg(args: &Value, required: bool) -> Result<Option<String>, ToolError> {
    match args.get("jobId") {
        None | Some(Value::Null) if !required => Ok(None),
        Some(Value::String(id)) if !id.is_empty() => Ok(Some(id.clone())),
        _ => Err(ToolError::invalid_arguments("jobId must be a non-empty string")),
    }
}

/// `job_status {jobId?}`.
pub async fn job_status(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let wait_ms = wait_ms_arg(&args)?;
    if let Some(id) = job_id_arg(&args, false)? {
        let kind = id_kind(&id)?;
        if matches!(kind, IdKind::Bridge) {
            let probe = job::probe(&state).await;
            if !probe.available() {
                return Err(job::unavailable(&probe, "job_status on a bridge job"));
            }
        }
        let (state_ref, id_ref) = (&state, &id);
        return wait_for_job(&ctx.budget, wait_ms, WAIT_POLL_INTERVAL, "job_status", &id, |left_ms| async move {
            match kind {
                IdKind::CompileMap => compile_map::poll_job(state_ref, id_ref, left_ms).await,
                IdKind::Bridge => {
                    let status = job::status_within(state_ref, id_ref, left_ms).await?;
                    Ok(bridge_view(state_ref, status).into())
                }
            }
        })
        .await;
    }
    let mut out = Map::new();
    let probe = job::probe(&state).await;
    if probe.available() {
        let listed = job::list(&state).await?;
        let jobs: Vec<Value> = listed
            .get("jobs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|entry| bridge_view(&state, entry))
            .collect();
        out.insert("bridge".into(), json!({"jobs": jobs}));
    } else {
        out.insert("bridge".into(), Value::Null);
        out.insert(
            "bridgeNote".into(),
            json!(job::unavailable(&probe, "the bridge job list").message),
        );
    }
    out.insert("compileMap".into(), json!(compile_map::list_jobs(&state)));
    Ok(Value::Object(out).into())
}

/// `job_cancel {jobId}`.
pub async fn job_cancel(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let deadline = ctx.budget.deadline_ms()?;
    let id = job_id_arg(&args, true)?.expect("required");
    match id_kind(&id)? {
        IdKind::CompileMap => compile_map::cancel_job(&state, &id, deadline).await,
        IdKind::Bridge => {
            // a bridge job that compile_map owns is cancelled THROUGH compile_map, so its store
            // entry follows the bridge
            if let Some(map_id) = state.compile_jobs.id_for_bridge(&id) {
                return compile_map::cancel_job(&state, &map_id, deadline).await;
            }
            let probe = job::probe(&state).await;
            if !probe.available() {
                return Err(job::unavailable(&probe, "job_cancel on a bridge job"));
            }
            let reply = job::cancel(&state, &id).await?;
            let status = job::status(&state, &id).await?;
            let mut view = bridge_view(&state, status);
            view["cancelled"] = reply.get("cancelled").cloned().unwrap_or(json!(false));
            Ok(view.into())
        }
    }
}

#[cfg(test)]
mod wait_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn view(state: &str) -> ToolOutput {
        json!({"jobId": "c1", "state": state}).into()
    }

    #[tokio::test]
    async fn a_wait_ends_when_the_job_completes_before_the_deadline() {
        let budget = Budget::new(60_000, 5_000);
        let polls = AtomicU32::new(0);
        let out = wait_for_job(&budget, 20_000, Duration::from_millis(5), "job_status", "c1", |_| async {
            let n = polls.fetch_add(1, Ordering::SeqCst);
            Ok(view(if n >= 3 { "ok" } else { "running" }))
        })
        .await
        .unwrap();
        assert_eq!(out.json["done"], json!(true));
        assert_eq!(out.json["state"], json!("ok"));
        assert!(out.json["waitedMs"].as_u64().unwrap() < 5_000, "{}", out.json);
        assert_eq!(polls.load(Ordering::SeqCst), 4);
        assert!(out.next.is_none(), "a finished job adds no wait step");
    }

    #[tokio::test]
    async fn a_wait_ends_at_the_deadline_with_a_next_step() {
        let budget = Budget::new(60_000, 5_000);
        let out = wait_for_job(&budget, 60, Duration::from_millis(10), "compile_map", "c1", |_| async {
            Ok(view("running"))
        })
        .await
        .unwrap();
        assert_eq!(out.json["done"], json!(false));
        let waited = out.json["waitedMs"].as_u64().unwrap();
        assert!((60..1_000).contains(&waited), "waited {waited}");
        assert!(out.json.get("budgetClamped").is_none());
        let next = &out.next.unwrap()[0];
        assert_eq!(next.tool, "compile_map");
        assert_eq!(next.args.as_ref().unwrap()["jobId"], json!("c1"));
        assert!(next.args.as_ref().unwrap()["waitMs"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn waitms_zero_answers_at_once_with_one_poll() {
        let budget = Budget::new(60_000, 5_000);
        let polls = AtomicU32::new(0);
        let out = wait_for_job(&budget, 0, Duration::from_millis(10), "job_status", "c1", |_| async {
            polls.fetch_add(1, Ordering::SeqCst);
            Ok(view("running"))
        })
        .await
        .unwrap();
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert_eq!(out.json["done"], json!(false));
        assert_eq!(out.json["waitedMs"], json!(0));
    }

    #[tokio::test]
    async fn the_wait_is_clamped_to_the_tool_budget() {
        // 1.6 s of budget minus the 1.5 s margin leaves ~100 ms, whatever was asked
        let budget = Budget::new(1_600, 1_000);
        let out = wait_for_job(&budget, 600_000, Duration::from_millis(10), "job_status", "c1", |_| async {
            Ok(view("running"))
        })
        .await
        .unwrap();
        assert_eq!(out.json["budgetClamped"], json!(true));
        assert!(out.json["waitedMs"].as_u64().unwrap() < 600, "{}", out.json);
        assert_eq!(out.json["done"], json!(false));
    }

    #[tokio::test]
    async fn each_poll_gets_a_request_deadline_bounded_by_the_budget_left() {
        // 3 s of budget minus the 1.5 s margin: no poll may be given more than ~1.5 s
        let budget = Budget::new(3_000, 1_000);
        let seen = std::sync::Mutex::new(Vec::new());
        wait_for_job(&budget, 200, Duration::from_millis(10), "job_status", "c1", |left_ms| {
            seen.lock().unwrap().push(left_ms);
            async { Ok(view("running")) }
        })
        .await
        .unwrap();
        let seen = seen.into_inner().unwrap();
        assert!(seen.len() >= 2 && seen.iter().all(|ms| (500..=1_500).contains(ms)), "{seen:?}");
        assert!(seen.windows(2).all(|w| w[1] <= w[0]), "the deadline only shrinks: {seen:?}");
    }

    #[tokio::test]
    async fn a_poll_error_ends_the_wait() {
        let budget = Budget::new(60_000, 5_000);
        let result = wait_for_job(&budget, 10_000, Duration::from_millis(5), "job_status", "c1", |_| async {
            Err::<ToolOutput, _>(ToolError::internal("boom"))
        })
        .await;
        assert!(result.is_err());
    }

    #[test]
    fn waitms_is_validated() {
        assert_eq!(wait_ms_arg(&json!({})).unwrap(), WAIT_DEFAULT_MS);
        assert_eq!(wait_ms_arg(&json!({"waitMs": null})).unwrap(), 30_000);
        assert_eq!(wait_ms_arg(&json!({"waitMs": 0})).unwrap(), 0);
        assert_eq!(wait_ms_arg(&json!({"waitMs": 250})).unwrap(), 250);
        assert!(wait_ms_arg(&json!({"waitMs": -1})).is_err());
        assert!(wait_ms_arg(&json!({"waitMs": WAIT_MAX_MS + 1})).is_err());
        assert!(wait_ms_arg(&json!({"waitMs": "5"})).is_err());
    }
}
