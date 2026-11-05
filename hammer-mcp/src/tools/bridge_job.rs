//! The bridge's job model (`job.start` / `job.status` / `job.cancel`, first kind
//! `resourcecompiler`) as one helper: start, poll, wait, cancel, capability probe, error mapping
//! and the bridge-log -> host-path mapping. The calling model never sees how the compiler runs
//! (Wine inside Hammer's container, the tagged-kill teardown, the lingering process); it sees a
//! job id and a state.
//!
//! Wire contract (`target/orch/unblock-rows/jobs-contract.md`):
//!
//! * `job.start {kind:"resourcecompiler", input:"csgo_addons/<addon>/<path>", game?, flags?:[tokens],
//!   timeoutMs?, clientRef?}` (`clientRef`: the caller's token, echoed by `job.status`) -> `{jobId, state:"running", pid, log:{windows, relative}, command, startedAtMs}`
//!   (a job id is OPAQUE: `j<N>-<nonce>` today, always handed back verbatim);
//! * `job.status {jobId}` -> `{jobId, kind, state: running|ok|failed|cancelled|timeout, input,
//!   elapsedMs, failLines, fatal, logTail, log, summary?:{compiled,failed,skipped,seconds}, exitCode?}`;
//!   `job.status {}` -> `{jobs:[...]}`; `job.cancel {jobId}` -> `{cancelled, state}`;
//! * one running job per bridge: `-32017 {blockedOn:"job: <id> running"}` (or `"job: another job is
//!   starting"` for a concurrent start's reservation window); Windows-only / compiler
//!   missing are `-32017` with another `blockedOn`; bad parameters and unknown ids are `-32602`;
//! * all three answer on the bridge's IPC lane (they work while Hammer's main thread sits in a
//!   modal), so the requests are **ungated**.
//!
//! The bridge client connects lazily, so [`probe`] connects first and only then asks `has`
//! (the same order `run_map`'s `read_game_local` uses).
use crate::state::HammerState;
use cs2_mcp_common::bridge::{BridgeError, Kind, RequestOpts};
use cs2_mcp_common::tools::ToolError;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const JOB_START: &str = "job.start";
pub const JOB_STATUS: &str = "job.status";
pub const JOB_CANCEL: &str = "job.cancel";

/// Per-request deadline for the `job.*` round trips (they answer on the IPC lane, so this is
/// generous, not a compile budget).
pub const REQUEST_DEADLINE_MS: u64 = 5_000;
/// `wait`'s poll interval: starts at the first, grows to the second (never a blind sleep: every
/// iteration is a `job.status`, and the loop is bounded by the caller's deadline).
pub const POLL_FIRST: Duration = Duration::from_millis(250);
pub const POLL_MAX: Duration = Duration::from_millis(500);
/// The bridge's `timeoutMs` range.
pub const BRIDGE_TIMEOUT_MIN_MS: u64 = 1_000;
pub const BRIDGE_TIMEOUT_MAX_MS: u64 = 3_600_000;

/// What the capability probe found.
#[derive(Debug, Clone)]
pub enum Probe {
    /// The bridge advertises `job.start`.
    Available,
    /// Connected, but the bridge does not advertise `job.start` (an older build).
    Missing,
    /// No bridge to ask (not running, refused, ...): the text is the connection error.
    NoBridge(String),
}

impl Probe {
    pub fn available(&self) -> bool {
        matches!(self, Probe::Available)
    }
}

/// Connect (lazily, like `read_game_local`) and ask whether the bridge advertises `job.start`.
pub async fn probe(state: &HammerState) -> Probe {
    let mut bridge = state.bridge.lock().await;
    if let Err(error) = bridge.ensure_connected().await {
        return Probe::NoBridge(error.message);
    }
    if bridge.has(JOB_START) {
        Probe::Available
    } else {
        Probe::Missing
    }
}

/// The error for a tool that has no other route (`compile_asset`, `job_status` on a bridge id, ...)
/// when the bridge cannot run jobs: `RuntimeUnavailable`, naming `job.start`.
pub fn unavailable(probe: &Probe, what: &str) -> ToolError {
    let (detail, reason) = match probe {
        Probe::Available => ("the bridge advertises job.start".to_string(), "available"),
        Probe::Missing => (
            "the connected bridge does not advertise job.start (an older build: rebuild/install the \
             bridge)"
                .to_string(),
            "job_start_not_advertised",
        ),
        Probe::NoBridge(error) => (
            format!("no bridge connection to ask for job.start ({error})"),
            "no_bridge",
        ),
    };
    ToolError::new(
        "RuntimeUnavailable",
        "RuntimeUnavailable",
        format!("{what} needs the bridge's job route (job.start): {detail}"),
    )
    .with_data(json!({"missing": [JOB_START], "reason": reason}))
    .retry_safe(true)
}

/// Map a `job.*` bridge failure to this server's taxonomy:
/// `-32017 "job: <id> running"` -> `EngineBusy` naming the running job (`data.jobId`),
/// any other `-32017` (Windows-only, compiler missing, shutting down, ...) -> `RuntimeUnavailable`
/// with the bridge's `blockedOn`, `-32602` -> `InvalidArguments` (the bridge's `reason`),
/// everything else keeps the bridge taxonomy.
pub fn map_error(error: BridgeError) -> ToolError {
    let blocked_on = error
        .data
        .get("blockedOn")
        .and_then(Value::as_str)
        .map(str::to_string);
    if error.kind == Kind::PrerequisiteFailed {
        let blocked = blocked_on.unwrap_or_else(|| error.message.clone());
        if blocked == "job: another job is starting" {
            return ToolError::new(
                "EngineBusy",
                "EngineBusy",
                "the bridge is starting another job right now (one compile at a time): retry in a \
                 moment, or poll job_status to see the job that is starting",
            )
            .with_data(json!({"reason": "bridge_job_starting", "blockedOn": blocked}))
            .retry_safe(true);
        }
        if let Some(id) = blocked
            .strip_prefix("job: ")
            .and_then(|rest| rest.strip_suffix(" running"))
        {
            return ToolError::new(
                "EngineBusy",
                "EngineBusy",
                format!(
                    "the bridge is already running job {id}: one compile at a time — poll it with \
                     job_status {{jobId: {id:?}}} or end it with job_cancel {{jobId: {id:?}}}"
                ),
            )
            .with_data(json!({"reason": "bridge_job_running", "jobId": id, "blockedOn": blocked}))
            .retry_safe(true);
        }
        return ToolError::new(
            "RuntimeUnavailable",
            "RuntimeUnavailable",
            format!("the bridge cannot run this job: {blocked}"),
        )
        .with_data(json!({"blockedOn": blocked, "reason": "bridge_job_blocked"}))
        .retry_safe(true);
    }
    if error.code == Some(-32602) {
        let reason = error
            .data
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or(&error.message)
            .to_string();
        return ToolError::invalid_arguments(format!("the bridge refused the job request: {reason}"))
            .with_data(json!({"reason": reason}));
    }
    ToolError::from(error)
}

/// Is `error` (from [`start`]) "this bridge cannot run compile jobs HERE" — Windows-only bridge,
/// the compiler executable missing, the job object refused? The callers that have a podman route
/// (`compile_map`, `model_compile`, `particle_compile`) fall back to it with a note; only
/// `compile_asset`, which has no other route, refuses with the error.
pub fn route_unavailable(error: &ToolError) -> bool {
    error.name == "RuntimeUnavailable"
        && error
            .data
            .get("blockedOn")
            .and_then(Value::as_str)
            .is_some_and(|blocked| {
                blocked.starts_with("job: AssignProcessToJobObject")
                    || blocked.starts_with("job: resourcecompiler.exe not found")
                    || blocked.contains("Windows-only")
            })
}

/// `fatal`: the bridge saw a `FATAL ERROR` line (it ends such a job `failed` itself).
pub fn fatal(status: &Value) -> bool {
    status.get("fatal").and_then(Value::as_bool) == Some(true)
}

/// One `job.*` round trip (ungated: the bridge answers these while Hammer is in a modal).
async fn request_raw(
    state: &HammerState,
    method: &str,
    params: Value,
    deadline_ms: u64,
) -> Result<Value, BridgeError> {
    let mut bridge = state.bridge.lock().await;
    bridge
        .request(method, params, RequestOpts::ungated(deadline_ms.max(1)))
        .await
        .map(|reply| reply.result)
}

async fn request(
    state: &HammerState,
    method: &str,
    params: Value,
    deadline_ms: u64,
) -> Result<Value, ToolError> {
    request_raw(state, method, params, deadline_ms).await.map_err(map_error)
}

/// The `timeoutMs` the bridge is given for a compile that runs for `wanted`.
pub fn bridge_timeout_ms(wanted: Duration) -> u64 {
    (wanted.as_millis() as u64).clamp(BRIDGE_TIMEOUT_MIN_MS, BRIDGE_TIMEOUT_MAX_MS)
}

/// Why a `job.start` did not hand back a job.
#[derive(Debug)]
pub enum StartFailure {
    /// The bridge answered with a refusal (`-32017`/`-32602`/`-32603`): nothing was spawned.
    Refused(ToolError),
    /// No answer (timeout, disconnect, queue timeout): the job MAY have been spawned. The caller
    /// looks for it with [`adopt`].
    Lost(ToolError),
}

/// Unix epoch milliseconds (the clock the bridge's `startedAtMs` uses).
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A fresh `clientRef` for one `job.start` (`hm<pid base36>-<counter base36>`: unique within this
/// server and across the servers sharing a bridge; 1..=64 bytes of `[A-Za-z0-9_.-]`).
pub fn new_client_ref() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    format!("hm{}-{}", to_base36(std::process::id() as u64), to_base36(n))
}

fn to_base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// `job.start {kind:"resourcecompiler", ..., clientRef}` with the refusal / lost-reply
/// distinction. `input` is `csgo_addons/<addon>/<path>`; `client_ref` is this start's own token
/// ([`new_client_ref`]), the only thing a lost reply is matched by ([`adopt`]). An error raised
/// before the request was written (no bridge, connect/handshake failure) is `Refused`: nothing
/// can have been spawned.
pub async fn start_raw(
    state: &HammerState,
    input: &str,
    game: bool,
    flags: &[String],
    timeout_ms: Option<u64>,
    client_ref: &str,
) -> Result<Value, StartFailure> {
    let mut params = json!({
        "kind": "resourcecompiler",
        "input": input,
        "game": game,
        "flags": flags,
        "clientRef": client_ref,
    });
    if let Some(ms) = timeout_ms {
        params["timeoutMs"] = json!(ms);
    }
    let mut bridge = state.bridge.lock().await;
    if let Err(error) = bridge.ensure_connected().await {
        return Err(StartFailure::Refused(map_error(error)));
    }
    let answer = bridge
        .request(JOB_START, params, RequestOpts::ungated(REQUEST_DEADLINE_MS))
        .await
        .map(|reply| reply.result);
    drop(bridge);
    match answer {
        Ok(reply) => Ok(reply),
        Err(error) if matches!(error.code, Some(-32017) | Some(-32602) | Some(-32603)) => {
            Err(StartFailure::Refused(map_error(error)))
        }
        Err(error) => Err(StartFailure::Lost(map_error(error))),
    }
}

/// After a lost `job.start` reply: the job the bridge spawned anyway — the entry of
/// `job.status {}` whose `clientRef` equals OURS (any state: it may have finished already). No
/// input or time heuristics: another caller's job for the same input is never adopted.
/// `Ok(None)` = no such job (yet).
pub async fn adopt(state: &HammerState, client_ref: &str) -> Result<Option<Value>, ToolError> {
    let listed = list(state).await?;
    Ok(listed
        .get("jobs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|job| job.get("clientRef").and_then(Value::as_str) == Some(client_ref))
        .cloned())
}

/// The error for a start that arrives while this server is shutting down.
pub fn shutting_down_error() -> ToolError {
    ToolError::new(
        "EngineBusy",
        "EngineBusy",
        "this server is shutting down (the job cleanup is running): no compile is started",
    )
    .with_data(json!({"reason": "shutting_down"}))
    .retry_safe(true)
}

/// `job.start` for the callers that own no store entry (`compile_asset`, the file-route compiles):
/// registered against the shutdown BEFORE the request (a start in flight when the shutdown begins
/// is waited for, and cancelled when its reply arrives), a lost reply is adopted from
/// `job.status {}`, and the new id is remembered so the shutdown cancels it.
pub async fn start_with_timeout(
    state: &HammerState,
    input: &str,
    game: bool,
    flags: &[String],
    timeout_ms: Option<u64>,
) -> Result<Value, ToolError> {
    let Some(_guard) = state.compile_jobs.begin_start() else {
        return Err(shutting_down_error());
    };
    // the placeholder is registered BEFORE the request: a reply that is lost, or a future that is
    // dropped, leaves a job nobody holds an id for — the next `job.status {}` (a poll, a list, the
    // shutdown) resolves the clientRef to it and the shutdown cancels it
    let client_ref = new_client_ref();
    state.compile_jobs.note_ref(&client_ref);
    let reply = match start_raw(state, input, game, flags, timeout_ms, &client_ref).await {
        Ok(reply) => reply,
        Err(StartFailure::Refused(mut error)) => {
            state.compile_jobs.drop_ref(&client_ref);
            if is_stage_flag_refusal(&error.message) {
                error.message.push_str(
                    " (the installed bridge is probably older than m3.16, which added the map \
                     stage flags -world/-phys/-vis/-bakelighting: reinstall the bridge DLL, \
                     `cargo bridge install`)",
                );
            }
            if input.ends_with(".js") && error.message.contains("input extension must be one of") {
                error.message.push_str(
                    " (the installed bridge is older than m3.17, which added .js inputs for cs_script: \
                     reinstall the bridge DLL, `cargo bridge install`)",
                );
            }
            return Err(error);
        }
        Err(StartFailure::Lost(error)) => match adopt(state, &client_ref).await {
            Ok(Some(found)) => found,
            _ => return Err(error), // the placeholder stays
        },
    };
    state.compile_jobs.drop_ref(&client_ref);
    if let Some(id) = reply.get("jobId").and_then(Value::as_str) {
        state.compile_jobs.note_asset(id);
        if state.compile_jobs.is_shutting_down() {
            let _ = cancel(state, id).await;
            return Err(shutting_down_error());
        }
    }
    Ok(reply)
}

/// A `job.start` refusal of one of the map-stage flags m3.16 added to the bridge allowlist.
fn is_stage_flag_refusal(message: &str) -> bool {
    message.contains("is not allowed")
        && ["-world", "-phys", "-vis", "-bakelighting"]
            .iter()
            .any(|flag| message.contains(&format!("\"{flag}\"")))
}

/// [`start_with_timeout`] with an explicit deadline.
pub async fn start(
    state: &HammerState,
    input: &str,
    game: bool,
    flags: &[String],
    timeout_ms: u64,
) -> Result<Value, ToolError> {
    start_with_timeout(state, input, game, flags, Some(timeout_ms)).await
}

pub async fn status(state: &HammerState, job_id: &str) -> Result<Value, ToolError> {
    status_within(state, job_id, REQUEST_DEADLINE_MS).await
}

/// [`status`] with the request deadline cut to `deadline_ms` (a waiting poll bounds its last
/// request by what is left of the tool budget); never longer than [`REQUEST_DEADLINE_MS`].
pub async fn status_within(
    state: &HammerState,
    job_id: &str,
    deadline_ms: u64,
) -> Result<Value, ToolError> {
    let deadline = deadline_ms.clamp(1, REQUEST_DEADLINE_MS);
    request(state, JOB_STATUS, json!({"jobId": job_id}), deadline).await
}

/// `job.status {}`. Every answer also resolves the pending `clientRef` placeholders
/// ([`crate::tools::compile_map::CompileJobs::note_ref`]) of starts whose reply was lost: a listed
/// job carrying one of them becomes a known asset id (the shutdown cancels it).
pub async fn list(state: &HammerState) -> Result<Value, ToolError> {
    let listed = request(state, JOB_STATUS, json!({}), REQUEST_DEADLINE_MS).await?;
    let pending = state.compile_jobs.pending_refs();
    if !pending.is_empty() {
        for job in listed.get("jobs").and_then(Value::as_array).into_iter().flatten() {
            let Some(client_ref) = job.get("clientRef").and_then(Value::as_str) else { continue };
            if !pending.iter().any(|p| p == client_ref) {
                continue;
            }
            if let Some(id) = job.get("jobId").and_then(Value::as_str) {
                if job.get("state").and_then(Value::as_str) == Some("running") {
                    state.compile_jobs.note_asset(id);
                }
            }
            state.compile_jobs.drop_ref(client_ref);
        }
    }
    Ok(listed)
}

pub async fn cancel(state: &HammerState, job_id: &str) -> Result<Value, ToolError> {
    request(state, JOB_CANCEL, json!({"jobId": job_id}), REQUEST_DEADLINE_MS).await
}

/// A terminal state (anything but `running`).
pub fn is_terminal(state: &str) -> bool {
    state != "running"
}

/// `status["state"]`.
pub fn state_of(status: &Value) -> &str {
    status.get("state").and_then(Value::as_str).unwrap_or("running")
}

/// The bridge's log (`log.relative` is relative to the app dir) as a host path
/// (`<app root>/<relative>`); `None` when the bridge sent no usable relative path.
pub fn host_log(state: &HammerState, log: Option<&Value>) -> Option<PathBuf> {
    let log = log?;
    // a native install (Windows host): the bridge keeps its job logs out of the install
    // (`%LOCALAPPDATA%\cs2mcpbridge\jobs`, `relative: null`) and its Windows path is this host's
    if cfg!(windows) && log.get("relative").is_none_or(Value::is_null) {
        return log.get("windows")?.as_str().map(PathBuf::from);
    }
    let relative = log.get("relative")?.as_str()?;
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.contains('\\')
        || relative.split('/').any(|s| s.is_empty() || s == "." || s == "..")
    {
        return None;
    }
    Some(state.content().app_root.join(relative))
}

/// A bridge status/start reply with the host log path added (`log.host`) and `route:"bridge"`.
pub fn with_host_log(state: &HammerState, mut value: Value) -> Value {
    let host = host_log(state, value.get("log")).map(|p| p.display().to_string());
    if let Some(object) = value.as_object_mut() {
        if let Some(log) = object.get_mut("log").and_then(Value::as_object_mut) {
            log.insert("host".into(), json!(host));
        }
        object.insert("route".into(), json!("bridge"));
    }
    value
}

/// What `wait` ended with.
#[derive(Debug, Clone)]
pub struct Waited {
    /// The last `job.status` reply (terminal unless the wait itself gave up).
    pub status: Value,
    /// The deadline passed first: the job was cancelled (best effort) and `status` re-read.
    pub timed_out: bool,
    /// The cancel after the deadline failed (the job may still be running).
    pub cancel_error: Option<String>,
}

/// Ends a job when the future that owns it is dropped (a client-cancelled tool call): spawns a
/// best-effort `job.cancel`. Disarmed once the job is known terminal or was cancelled explicitly.
struct CancelOnDrop {
    state: HammerState,
    id: String,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let state = self.state.clone();
            let id = self.id.clone();
            runtime.spawn(async move {
                let _ = cancel(&state, &id).await;
            });
        }
    }
}

/// Poll `job.status` until the job is terminal or `timeout` passes; on the deadline the job is
/// cancelled and its final status re-read. Every iteration is one bounded round trip followed by
/// the poll interval clipped to the remaining time — no fixed sleeps. A status error ends the wait
/// but never leaves the job behind: one cancel is tried first; and a dropped future cancels too.
pub async fn wait(state: &HammerState, job_id: &str, timeout: Duration) -> Result<Waited, ToolError> {
    let mut guard = CancelOnDrop { state: state.clone(), id: job_id.to_string(), armed: true };
    let deadline = Instant::now() + timeout;
    let mut interval = POLL_FIRST;
    loop {
        let status = match self::status(state, job_id).await {
            Ok(status) => status,
            Err(error) => {
                guard.armed = false;
                let _ = cancel(state, job_id).await;
                return Err(error);
            }
        };
        if is_terminal(state_of(&status)) {
            guard.armed = false;
            state.compile_jobs.forget_asset(job_id);
            return Ok(Waited { status, timed_out: false, cancel_error: None });
        }
        let now = Instant::now();
        if now >= deadline {
            // the compile outlived its budget: end it, then report what the bridge says now
            guard.armed = false;
            let cancel_error = cancel(state, job_id).await.err().map(|e| e.message);
            let status = self::status(state, job_id).await.unwrap_or(status);
            if is_terminal(state_of(&status)) {
                state.compile_jobs.forget_asset(job_id);
            }
            return Ok(Waited { status, timed_out: true, cancel_error });
        }
        tokio::time::sleep(interval.min(deadline - now)).await;
        interval = (interval + POLL_FIRST / 2).min(POLL_MAX);
    }
}

/// `summary` of a status as (compiled, failed, skipped).
pub fn summary_counts(status: &Value) -> (Option<i64>, Option<i64>, Option<i64>) {
    let summary = status.get("summary");
    let number = |key: &str| summary.and_then(|s| s.get(key)).and_then(Value::as_i64);
    (number("compiled"), number("failed"), number("skipped"))
}

/// `failLines` of a status as strings.
pub fn fail_lines(status: &Value) -> Vec<String> {
    status
        .get("failLines")
        .and_then(Value::as_array)
        .map(|lines| lines.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(code: i64, data: Value) -> BridgeError {
        let mut error = BridgeError::new(Kind::from_code(code).expect("kind"), "wire message");
        error.code = Some(code);
        if let Value::Object(map) = data {
            error.data = map;
        }
        error
    }

    #[test]
    fn a_running_job_is_engine_busy_naming_it() {
        let error = map_error(wire(-32017, json!({"blockedOn": "job: j7 running"})));
        assert_eq!(error.name, "EngineBusy");
        assert_eq!(error.data["jobId"], json!("j7"));
        assert_eq!(error.data["reason"], json!("bridge_job_running"));
        assert!(error.retry_safe);
    }

    #[test]
    fn other_prerequisites_are_runtime_unavailable_with_the_blocked_on() {
        for blocked in [
            "job: resourcecompiler is Windows-only",
            "job: resourcecompiler.exe not found at Z:\\x",
            "job: bridge is shutting down",
        ] {
            let error = map_error(wire(-32017, json!({"blockedOn": blocked})));
            assert_eq!(error.name, "RuntimeUnavailable", "{blocked}");
            assert_eq!(error.data["blockedOn"], json!(blocked));
        }
    }

    #[test]
    fn invalid_params_keep_the_bridge_reason() {
        let error = map_error(wire(-32602, json!({"reason": "unknown jobId"})));
        assert_eq!(error.name, "InvalidParams");
        assert!(error.message.contains("unknown jobId"), "{error:?}");
        assert_eq!(error.data["reason"], json!("unknown jobId"));
    }

    #[test]
    fn the_unavailable_routes_are_recognised() {
        for blocked in [
            "job: resourcecompiler is Windows-only",
            "job: AssignProcessToJobObject failed: 5",
            "job: resourcecompiler.exe not found at Z:\\x",
        ] {
            assert!(route_unavailable(&map_error(wire(-32017, json!({"blockedOn": blocked})))), "{blocked}");
        }
        for blocked in ["job: j7 running", "job: bridge is shutting down"] {
            assert!(!route_unavailable(&map_error(wire(-32017, json!({"blockedOn": blocked})))), "{blocked}");
        }
    }

    #[test]
    fn the_bridge_timeout_is_clamped_to_the_wire_range() {
        assert_eq!(bridge_timeout_ms(Duration::from_millis(5)), 1_000);
        assert_eq!(bridge_timeout_ms(Duration::from_secs(120)), 120_000);
        assert_eq!(bridge_timeout_ms(Duration::from_secs(99_999)), 3_600_000);
    }

    #[test]
    fn only_a_stage_flag_refusal_gets_the_old_bridge_hint() {
        assert!(is_stage_flag_refusal("flag \"-world\" is not allowed"));
        assert!(is_stage_flag_refusal("job.start: flag \"-bakelighting\" is not allowed"));
        assert!(!is_stage_flag_refusal("flag \"-evil\" is not allowed"));
        assert!(!is_stage_flag_refusal("flag \"-world\" needs an integer 1..=64"));
    }
}
