//! The `resourcecompiler` route shared by the file-route compile tools (`model_compile` for
//! `.vmdl`, `particle_compile` for `.vpcf`).
//!
//! One implementation, parameterised by [`CompileTarget`] (which running tools container to exec
//! in) and by the caller's budget/timeout; the caller owns the source-path rules
//! ([`crate::vmdl::ContentPaths`]) and the "what does `ok` mean" verdict.
//!
//! Facts encoded here (hammer-mcp/research/modeldoc-re.md, target/orch/facts-round2.md; the
//! particle route runs the same compiler in the same container layout):
//! * `podman exec <container> bash -c 'cd /app && ./entrypoint.sh resourcecompiler.exe -nop4
//!   -game /app/game/csgo -i <container path>'` — `-game /app/game/csgo`, never
//!   `<game>/csgo_addons/<addon>` (FATAL gameinfo.gi);
//! * the compiler's exit status says nothing; the verdict comes from its `OK: <n> compiled, <m>
//!   failed, <k> skipped` summary plus the compiled `.vmdl_c`/`.vpcf_c` mtime, so `ok:false` is a
//!   **result**, never an error (the caller reads `failLines`/`fatal`);
//! * `resourcecompiler` skips an input whose output is newer: "0 compiled, 1 skipped" with an
//!   output at least as new as the source is up to date, not a stale build;
//! * the tools container is started by the orchestrator (`cargo bridge runtime up`), never by this
//!   server: no container -> `RuntimeUnavailable` with the label and the launch hint;
//! * the container path is embedded in a `bash -c` command line, so characters outside
//!   `safe_container_path` are refused instead of quoted through.
//!
//! Two runners behind the one entry point [`compile_resourcecompiler`] (the result's `compiler`
//! says which ran): when the bridge advertises `job.start` the compile is a **bridge job**
//! (`tools/bridge_job.rs`: `game:true`, polled until terminal, cancelled on the tool's timeout;
//! the result carries its `jobId` and the host-side `log`), otherwise the podman exec above runs
//! unchanged. The output-updated verdict is the same host-side mtime check on both.

use crate::state::HammerState;
use cs2_mcp_common::runtime::docker::{sh_quote, RuntimeError};
use cs2_mcp_common::tools::{Budget, ToolError, ToolOutput};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

/// Default and maximum wall clock for one compile (`timeoutMs`), also used by
/// `model_compile`'s editor route poll.
pub const COMPILE_TIMEOUT_MS: u64 = 120_000;
pub const COMPILE_TIMEOUT_MAX_MS: u64 = 600_000;
/// Compile-output mtime poll: bounded interval, deadline from the tool budget (no fixed sleeps).
pub const COMPILE_POLL: Duration = Duration::from_millis(100);
/// Compiler output kept in `stdoutTail` (last N lines of stdout + stderr).
const STDOUT_TAIL_LINES: usize = 40;

/// Which running container a file-route compile execs in: the podman `label=` filter plus the
/// launch hint reported when nothing carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompileTarget {
    /// The `podman ps --filter label=<label>` value. `cs2mcp.tool=modeldoc` pins the ModelDoc
    /// container; the bare key `cs2mcp.tool` accepts any tools container (pet runs in the default
    /// `cs2mcp.tool=hammer` session).
    pub label: &'static str,
    /// The command that starts such a container (goes into `RuntimeUnavailable`'s message/data).
    pub hint: &'static str,
}

impl CompileTarget {
    /// The ModelDoc instance (`TOOL=modeldoc cargo bridge runtime up`).
    pub const MODELDOC: CompileTarget = CompileTarget {
        label: "cs2mcp.tool=modeldoc",
        hint: "TOOL=modeldoc cargo bridge runtime up",
    };
    /// Any tools container: the bare label key, so the particle route also finds a container
    /// started with the default `TOOL=hammer`. Two running tools containers both carrying
    /// `cs2mcp.tool` make the lookup ambiguous -> `RuntimeUnavailable` (name one with
    /// `--tools-container`).
    pub const ANY_TOOLS: CompileTarget = CompileTarget {
        label: "cs2mcp.tool",
        hint: "cargo bridge runtime up",
    };
}

/// `resourcecompiler -i` container path characters. The path comes from a validated addon host
/// path but is embedded in a `bash -c` command line, so anything outside this set is refused
/// instead of quoted through.
pub fn safe_container_path(path: &str) -> bool {
    path.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-'))
}

/// `timeoutMs?`: default [`COMPILE_TIMEOUT_MS`], at most [`COMPILE_TIMEOUT_MAX_MS`], never beyond
/// the remaining tool budget (a zero remainder is budget exhaustion, not a "forever" compile).
pub fn compile_timeout(args: &Value, budget: &Budget) -> Result<Duration, ToolError> {
    let requested = match args.get("timeoutMs") {
        None | Some(Value::Null) => COMPILE_TIMEOUT_MS,
        Some(Value::Number(n)) => {
            const OVER: u64 = COMPILE_TIMEOUT_MAX_MS + 1;
            let ms = n.as_u64().unwrap_or(OVER);
            if ms == 0 {
                return Err(ToolError::invalid_arguments("timeoutMs must be > 0"));
            }
            ms.min(COMPILE_TIMEOUT_MAX_MS)
        }
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "timeoutMs must be an integer, got {other}"
            )))
        }
    };
    let timeout = Duration::from_millis(requested).min(budget.remaining());
    if timeout.is_zero() {
        return Err(ToolError::budget_exceeded(budget));
    }
    Ok(timeout)
}

/// `RuntimeUnavailable`: no running tools container to compile in (or an ambiguous label match).
/// `pub(crate)`: `tools/compile_map.rs` starts its background job against the same container set.
pub(crate) fn runtime_unavailable(target: CompileTarget, detail: impl std::fmt::Display) -> ToolError {
    ToolError::new(
        "RuntimeUnavailable",
        "RuntimeUnavailable",
        format!(
            "no tools container to compile in: {detail} (no running container with label {}; \
             start one with {}; or name one with --tools-container)",
            target.label, target.hint
        ),
    )
    .with_data(json!({"label": target.label, "hint": target.hint}))
    .retry_safe(true)
}

/// A Windows host has no podman route: its compiles are bridge jobs (`job.start`, which the
/// bridge in a `cs2mcp-native up` session answers). Called where the podman route would start,
/// so the caller gets the reason the bridge route was passed over and the native way in, never
/// a container hint (crates/cs2-native FIXME 17).
pub(crate) fn podman_route_here(fallback_note: Option<&str>) -> Result<(), ToolError> {
    if !cfg!(windows) {
        return Ok(());
    }
    let why = fallback_note.unwrap_or("the bridge is not connected or does not advertise job.start");
    Err(ToolError::new(
        "RuntimeUnavailable",
        "RuntimeUnavailable",
        format!(
            "compiles on a Windows host run as bridge jobs, and that route is not available: {why}. \
             Start the session with `cs2mcp-native up --steam gbe --tools` (session_status shows \
             the bridge and its capabilities); there is no container route on Windows"
        ),
    )
    .with_data(json!({"hint": crate::tools::compile::NATIVE_COMPILE_HINT}))
    .retry_safe(true))
}

/// The launch hint of [`podman_route_here`].
pub(crate) const NATIVE_COMPILE_HINT: &str = "cs2mcp-native up --steam gbe --tools";

/// podman's wording for a container it does not know (`podman exec` on a bad name fails with a
/// nonzero exit and no compiler output at all).
fn no_such_container(stderr: &str) -> bool {
    stderr.contains("no such container") || stderr.contains("no container with name or ID")
}

/// Route `resourcecompiler`: exec the compiler in the discovered tools container, then judge the
/// output. A failing run (`[FAIL]` lines, non-zero counts, a FATAL, no output) is a RESULT with
/// `ok:false`.
pub async fn compile_resourcecompiler(
    state: HammerState,
    path: &str,
    host: PathBuf,
    target: CompileTarget,
    timeout: Duration,
    force: Option<bool>,
) -> Result<ToolOutput, ToolError> {
    let fallback_note = match compile_via_bridge(&state, path, &host, timeout, force).await? {
        BridgeAttempt::Done(done) => return Ok(done),
        BridgeAttempt::Fallback(note) => note,
    };
    podman_route_here(fallback_note.as_deref())?;
    // The tools container: the explicit override, else discover the running instance.
    let container = match &state.tools_container {
        Some(name) => name.clone(),
        None => match state.runtime.container_by_label(target.label).await {
            Ok(Some(container)) => container,
            Ok(None) => {
                return Err(runtime_unavailable(
                    target,
                    "no running container carries the label",
                ));
            }
            Err(error) => {
                return Err(runtime_unavailable(target, error));
            }
        },
    };
    let Some(container_path) = state.content().container_path(&host) else {
        return Err(ToolError::invalid_arguments(format!(
            "{} is not under the content root",
            host.display()
        )));
    };
    if !safe_container_path(&container_path) {
        return Err(ToolError::invalid_arguments(format!(
            "container path {container_path:?} contains characters that could become shell code"
        )));
    }
    let Some(output_path) = state.content().compiled_output(&host) else {
        return Err(ToolError::invalid_arguments(format!(
            "{} has no compiled-output path",
            host.display()
        )));
    };
    let before = mtime_of(&output_path);
    let argv = vec![
        "bash".to_string(),
        "-c".to_string(),
        format!(
            "cd /app && ./entrypoint.sh resourcecompiler.exe -nop4{} -game /app/game/csgo -i {}",
            if force == Some(true) { " -f" } else { "" },
            sh_quote(&container_path)
        ),
    ];
    let started = Instant::now();
    let run = state.runtime.exec_in(&container, &argv, timeout).await;
    let text = match run {
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !output.status.success() && no_such_container(&stderr) {
                return Err(runtime_unavailable(target, stderr.trim()));
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            combine(&stdout, &stderr)
        }
        Err(error) => return Err(exec_failure(target, error)),
    };
    let after = mtime_of(&output_path);
    let updated = mtime_advanced(after, before);
    let summary = parse_compile_output(&text);
    // resourcecompiler skips an input whose output is newer: "0 compiled, 1 skipped" with an
    // output at least as new as the source is a success, not a stale build
    let up_to_date = !updated
        && summary.compiled == Some(0)
        && summary.skipped.is_some_and(|n| n > 0)
        && matches!((after, mtime_of(&host)), (Some(out), Some(src)) if out >= src);
    let ok = summary.compiled.is_some()
        && summary.failed == Some(0)
        && !summary.fatal
        && (updated || up_to_date);
    let skip = skip_verdict(&summary, updated, after, mtime_of(&host), force);
    let reason = failure_reason(ok, &summary, updated, None, skip.as_ref());
    let mut result = json!({
        "route": "resourcecompiler",
        "compiler": "podman",
        "skippedByCompiler": skip.is_some(),
        "reason": reason,
        "jobId": Value::Null,
        "path": path,
        "ok": ok,
        "compiled": summary.compiled,
        "failed": summary.failed,
        "skipped": summary.skipped,
        "failLines": summary.fail_lines,
        "fatal": summary.fatal,
        "outputPath": output_path.display().to_string(),
        "outputUpdated": updated,
        "upToDate": up_to_date,
        "durationMs": started.elapsed().as_millis() as u64,
        "stdoutTail": tail(&text, STDOUT_TAIL_LINES),
        "container": container,
    });
    if let Some(note) = fallback_note {
        result["notes"] = json!([note]);
    }
    Ok(result.into())
}

/// One input of a multi-file compile ([`compile_resourcecompiler_set`]).
#[derive(Debug, Clone)]
pub struct CompileFile {
    /// The addon-relative spelling reported as the verdict's `path` (`particles/x.vpcf`).
    pub rel: String,
    pub host: PathBuf,
    /// The file that references this one (a child pulled in by its parent), if any.
    pub referenced_by: Option<String>,
}

/// Several inputs through the compiler in ONE run (podman route: several `-i`, a single exec and
/// a single OK summary) with a per-file verdict list. The bridge route's `job.start` takes one
/// `input`, so there each file is its own bridge job, in the given order (children first).
/// `missing` are verdicts the caller already settled as failures (a referenced file that does
/// not exist): they are reported in `files` and make the overall `ok` false; nothing is sent for
/// them. A file's verdict is `ok` when its output was refreshed, or is at least as new as the
/// source with nothing in the run's `[FAIL]` lines naming it.
pub async fn compile_resourcecompiler_set(
    state: HammerState,
    files: Vec<CompileFile>,
    missing: Vec<Value>,
    target: CompileTarget,
    timeout: Duration,
    force: Option<bool>,
) -> Result<ToolOutput, ToolError> {
    let started = Instant::now();
    let mut verdicts: Vec<Value> = Vec::new();
    let mut notes: Vec<Value> = Vec::new();
    let mut result = json!({"route": "resourcecompiler"});
    let mut run_ok = true;
    if !files.is_empty() {
        let mut bridge_done = false;
        if super::bridge_job::probe(&state).await.available() {
            // the first file decides whether the bridge route is usable here at all
            let mut ran: Vec<Value> = Vec::new();
            let mut fell_back = false;
            for (n, file) in files.iter().enumerate() {
                let left = timeout.saturating_sub(started.elapsed());
                let attempt = match compile_via_bridge(&state, &file.rel, &file.host, left, force).await {
                    Ok(attempt) => attempt,
                    // the first file's error is the whole call's error; a later one ends the run
                    // but keeps the verdicts already earned
                    Err(error) if n > 0 => {
                        ran.push(json!({
                            "path": file.rel, "ok": false, "skippedByCompiler": false, "failLines": [],
                            "reason": error.message,
                            "referencedBy": file.referenced_by,
                        }));
                        notes.push(json!(format!(
                            "the run stopped at {} ({}): the files after it were not compiled",
                            file.rel, error.message
                        )));
                        for rest in &files[n + 1..] {
                            ran.push(json!({
                                "path": rest.rel, "ok": false, "skippedByCompiler": false, "failLines": [],
                                "reason": "not attempted",
                                "referencedBy": rest.referenced_by,
                            }));
                        }
                        break;
                    }
                    Err(error) => return Err(error),
                };
                match attempt {
                    BridgeAttempt::Done(done) => {
                        let j = done.json;
                        ran.push(json!({
                            "path": file.rel,
                            "ok": j["ok"],
                            "skippedByCompiler": j["skippedByCompiler"],
                            "reason": j["reason"],
                            "upToDate": j["upToDate"],
                            "failLines": j["failLines"],
                            "jobId": j["jobId"],
                            "referencedBy": file.referenced_by,
                        }));
                    }
                    BridgeAttempt::Fallback(note) => {
                        if n == 0 {
                            fell_back = true;
                            if let Some(note) = note {
                                notes.push(json!(note));
                            }
                            break;
                        }
                        ran.push(json!({
                            "path": file.rel, "ok": false, "skippedByCompiler": false, "failLines": [],
                            "reason": "the bridge route became unavailable mid-run",
                            "referencedBy": file.referenced_by,
                        }));
                    }
                }
            }
            if !fell_back {
                run_ok = ran.iter().all(|v| v["ok"] == json!(true));
                verdicts = ran;
                result["compiler"] = json!("bridge");
                bridge_done = true;
            }
        }
        if !bridge_done {
            podman_route_here(None)?;
            let container = match &state.tools_container {
                Some(name) => name.clone(),
                None => match state.runtime.container_by_label(target.label).await {
                    Ok(Some(container)) => container,
                    Ok(None) => {
                        return Err(runtime_unavailable(target, "no running container carries the label"))
                    }
                    Err(error) => return Err(runtime_unavailable(target, error)),
                },
            };
            let mut inputs = Vec::new();
            let mut befores = Vec::new();
            for file in &files {
                let Some(container_path) = state.content().container_path(&file.host) else {
                    return Err(ToolError::invalid_arguments(format!(
                        "{} is not under the content root",
                        file.host.display()
                    )));
                };
                if !safe_container_path(&container_path) {
                    return Err(ToolError::invalid_arguments(format!(
                        "container path {container_path:?} contains characters that could become shell code"
                    )));
                }
                let Some(output) = state.content().compiled_output(&file.host) else {
                    return Err(ToolError::invalid_arguments(format!(
                        "{} has no compiled-output path",
                        file.host.display()
                    )));
                };
                befores.push((output.clone(), mtime_of(&output)));
                inputs.push(format!("-i {}", sh_quote(&container_path)));
            }
            let argv = vec![
                "bash".to_string(),
                "-c".to_string(),
                format!(
                    "cd /app && ./entrypoint.sh resourcecompiler.exe -nop4{} -game /app/game/csgo {}",
                    if force == Some(true) { " -f" } else { "" },
                    inputs.join(" ")
                ),
            ];
            let text = match state.runtime.exec_in(&container, &argv, timeout).await {
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    if !output.status.success() && no_such_container(&stderr) {
                        return Err(runtime_unavailable(target, stderr.trim()));
                    }
                    combine(&String::from_utf8_lossy(&output.stdout), &stderr)
                }
                Err(error) => return Err(exec_failure(target, error)),
            };
            let summary = parse_compile_output(&text);
            // an up-to-date-by-mtime output is not proof the compiler handled the file: a clean
            // run must account for every input (compiled + skipped >= inputs)
            // (dependencies are counted too: one input can print "29 compiled", so AT LEAST the
            // inputs)
            let accounted = summary.compiled.unwrap_or(0) + summary.skipped.unwrap_or(0)
                >= files.len() as i64;
            let parsed_clean =
                summary.compiled.is_some() && summary.failed == Some(0) && !summary.fatal;
            let run_clean = parsed_clean && accounted;
            if parsed_clean && !accounted {
                result["reason"] = json!(format!(
                    "compiler did not account for every input: {} inputs sent, only {} compiled + {} skipped reported",
                    files.len(),
                    summary.compiled.unwrap_or(0),
                    summary.skipped.unwrap_or(0)
                ));
            }
            for (file, (output, before)) in files.iter().zip(&befores) {
                let after = mtime_of(output);
                let updated = mtime_advanced(after, *before);
                let up_to_date = !updated
                    && matches!((after, mtime_of(&file.host)), (Some(out), Some(src)) if out >= src);
                let named: Vec<&String> = summary
                    .fail_lines
                    .iter()
                    .filter(|line| line.contains(file.rel.as_str()))
                    .collect();
                // a refreshed output proves itself; an up-to-date one only counts when the run accounted
                // for every input
                let ok = named.is_empty() && (updated || (up_to_date && run_clean));
                let skip = skip_verdict(&summary, updated, after, mtime_of(&file.host), force);
                let reason = if ok && skip.is_none() {
                    Value::Null
                } else {
                    json!(skip.as_ref().map(|s| s.reason.clone()).or_else(|| {
                        (!ok && named.is_empty()).then(|| {
                            "the compiler did not refresh this output and named no failure for it; see the run's stdoutTail".to_string()
                        })
                    }))
                };
                verdicts.push(json!({
                    "path": file.rel,
                    "ok": ok,
                    "skippedByCompiler": skip.is_some(),
                    "reason": reason,
                    "upToDate": up_to_date,
                    "outputUpdated": updated,
                    "failLines": named,
                    "referencedBy": file.referenced_by,
                }));
            }
            run_ok = run_clean;
            result["compiler"] = json!("podman");
            result["container"] = json!(container);
            result["compiled"] = json!(summary.compiled);
            result["failed"] = json!(summary.failed);
            result["skipped"] = json!(summary.skipped);
            result["fatal"] = json!(summary.fatal);
            result["failLines"] = json!(summary.fail_lines);
            result["stdoutTail"] = json!(tail(&text, STDOUT_TAIL_LINES));
        }
    }
    verdicts.extend(missing);
    let ok = run_ok && !verdicts.is_empty() && verdicts.iter().all(|v| v["ok"] == json!(true));
    result["ok"] = json!(ok);
    result["files"] = json!(verdicts);
    result["durationMs"] = json!(started.elapsed().as_millis() as u64);
    if !notes.is_empty() {
        result["notes"] = json!(notes);
    }
    Ok(result.into())
}

/// What the bridge runner decided.
enum BridgeAttempt {
    /// The compile ran as a bridge job; this is the result.
    Done(ToolOutput),
    /// The podman route runs; the note (when set) says why the bridge route was passed over after
    /// it was tried.
    Fallback(Option<String>),
}

/// Is `input` (`csgo_addons/<addon>/...`) a path the bridge's `job.start` accepts (`'/'`-separated
/// segments of `[A-Za-z0-9_.-]`, no `.`/`..`, at most 260 bytes)? Anything else keeps the podman
/// route, which has its own path rules.
fn bridge_input_ok(input: &str) -> bool {
    input.starts_with("csgo_addons/")
        && input.len() <= 260
        && input.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        })
}

/// The bridge-job runner of [`compile_resourcecompiler`]: `Ok(None)` when the bridge does not
/// advertise `job.start` (or the path is not one the bridge takes), so the podman route runs.
async fn compile_via_bridge(
    state: &HammerState,
    path: &str,
    host: &Path,
    timeout: Duration,
    force: Option<bool>,
) -> Result<BridgeAttempt, ToolError> {
    use super::bridge_job as job;
    if !job::probe(state).await.available() {
        return Ok(BridgeAttempt::Fallback(None));
    }
    let Some(input) = state.content().content_rel(host).filter(|i| bridge_input_ok(i)) else {
        return Ok(BridgeAttempt::Fallback(None));
    };
    let Some(output_path) = state.content().compiled_output(host) else {
        return Err(ToolError::invalid_arguments(format!(
            "{} has no compiled-output path",
            host.display()
        )));
    };
    let before = mtime_of(&output_path);
    let started = Instant::now();
    // the bridge gets a backstop deadline a little beyond ours, so OUR deadline (and its cancel)
    // is the one that normally fires
    let reply = match job::start(
        state,
        &input,
        true,
        &compiler_flags(force == Some(true)),
        job::bridge_timeout_ms(timeout + Duration::from_secs(5)),
    )
    .await
    {
        Ok(reply) => reply,
        Err(error) if job::route_unavailable(&error) => {
            return Ok(BridgeAttempt::Fallback(Some(format!(
                "the bridge route was unavailable ({}): this compile ran through the podman route \
                 instead",
                error.message
            ))));
        }
        Err(error) => return Err(error),
    };
    let Some(job_id) = reply.get("jobId").and_then(Value::as_str).map(str::to_string) else {
        return Err(ToolError::internal(format!(
            "the bridge's job.start reply carries no jobId: {reply}"
        )));
    };
    // (`job::start` already recorded the id for the shutdown's cancel; `wait` forgets it again
    // once the job is terminal and cancels it when this future is dropped)
    let waited = job::wait(state, &job_id, timeout).await?;
    let status = waited.status;
    let bridge_state = job::state_of(&status).to_string();
    let after = mtime_of(&output_path);
    let updated = mtime_advanced(after, before);
    let log_tail = status.get("logTail").and_then(Value::as_str).unwrap_or_default();
    let mut summary = parse_compile_output(log_tail);
    let (compiled, failed, skipped) = job::summary_counts(&status);
    if compiled.is_some() {
        summary.compiled = compiled;
        summary.failed = failed;
        summary.skipped = skipped;
    }
    let lines = job::fail_lines(&status);
    if !lines.is_empty() {
        summary.fail_lines = lines;
    }
    summary.fatal = summary.fatal || job::fatal(&status);
    let up_to_date = !updated
        && summary.compiled == Some(0)
        && summary.skipped.is_some_and(|n| n > 0)
        && matches!((after, mtime_of(host)), (Some(out), Some(src)) if out >= src);
    let ok = bridge_state == "ok"
        && summary.compiled.is_some()
        && summary.failed == Some(0)
        && !summary.fatal
        && (updated || up_to_date);
    let skip = skip_verdict(&summary, updated, after, mtime_of(host), force);
    let reason = failure_reason(ok, &summary, updated, Some(&bridge_state), skip.as_ref());
    Ok(BridgeAttempt::Done(
        json!({
            "route": "resourcecompiler",
            "compiler": "bridge",
            "skippedByCompiler": skip.is_some(),
            "reason": reason,
            "jobId": job_id,
            "state": bridge_state,
            "timedOut": waited.timed_out,
            "cancelError": waited.cancel_error,
            "path": path,
            "ok": ok,
            "compiled": summary.compiled,
            "failed": summary.failed,
            "skipped": summary.skipped,
            "failLines": summary.fail_lines,
            "fatal": summary.fatal,
            "outputPath": output_path.display().to_string(),
            "outputUpdated": updated,
            "upToDate": up_to_date,
            "durationMs": started.elapsed().as_millis() as u64,
            "stdoutTail": tail(log_tail, STDOUT_TAIL_LINES),
            "container": Value::Null,
            "log": job::host_log(state, status.get("log")).map(|p| p.display().to_string()),
        })
        .into(),
    ))
}

/// The `job.start` flags of a file compile: `-nop4`, plus `-f` when the caller forces it.
fn compiler_flags(force: bool) -> Vec<String> {
    let mut flags = vec!["-nop4".to_string()];
    if force {
        flags.push("-f".to_string());
    }
    flags
}

/// A compiler "skipped" verdict for one input: the run compiled nothing, skipped something and
/// failed nothing, and did not refresh the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipVerdict {
    /// The output is at least as new as the source (so the skip is a clean "up to date").
    pub fresh: bool,
    pub reason: String,
}

/// Explain a compiler skip. `force` is `None` when the tool has no force option.
pub fn skip_verdict(
    summary: &CompileSummary,
    updated: bool,
    output_mtime: Option<SystemTime>,
    source_mtime: Option<SystemTime>,
    force: Option<bool>,
) -> Option<SkipVerdict> {
    if updated
        || summary.compiled != Some(0)
        || !summary.skipped.is_some_and(|n| n > 0)
        || summary.failed != Some(0)
        || summary.fatal
    {
        return None;
    }
    let retry = match force {
        Some(false) => "retry with force:true (compiles with -f)",
        Some(true) => "force was already set and the compiler still skipped it; edit the source (touch it) and compile again",
        None => "touch the source and compile again",
    };
    Some(match (output_mtime, source_mtime) {
        (Some(out), Some(src)) if out >= src => SkipVerdict {
            fresh: true,
            reason: "the compiler skipped this input: its compiled output is already up to date".to_string(),
        },
        (Some(_), Some(_)) => SkipVerdict {
            fresh: false,
            reason: format!(
                "the compiler skipped this input (it treats its output as current), but the compiled output is older than the source; {retry}"
            ),
        },
        (None, _) => SkipVerdict {
            fresh: false,
            reason: format!("the compiler skipped this input but no compiled output exists; {retry}"),
        },
        (Some(_), None) => SkipVerdict {
            fresh: false,
            reason: format!("the compiler skipped this input and the source's mtime could not be read; {retry}"),
        },
    })
}

/// Why a single-file result is not `ok` (or a note for a clean skip). Never `None` for
/// `ok:false` with no `failLines`.
fn failure_reason(
    ok: bool,
    summary: &CompileSummary,
    updated: bool,
    bridge_state: Option<&str>,
    skip: Option<&SkipVerdict>,
) -> Value {
    if let Some(skip) = skip {
        return json!(skip.reason);
    }
    if ok {
        return Value::Null;
    }
    if !summary.fail_lines.is_empty() {
        return Value::Null; // failLines carry the reason
    }
    let text = if summary.fatal {
        "the compiler reported a FATAL; see stdoutTail".to_string()
    } else if bridge_state.is_some_and(|s| s != "ok") {
        format!("the compiler job ended {}; see stdoutTail / log", bridge_state.unwrap_or_default())
    } else if summary.compiled.is_none() {
        "no `OK: n compiled, m failed, k skipped` line in the compiler output (it did not finish); see stdoutTail".to_string()
    } else if !updated {
        "the compiler reported no failure but the compiled output was not refreshed; see stdoutTail".to_string()
    } else {
        "the compiler reported failures; see stdoutTail".to_string()
    };
    json!(text)
}

/// `podman exec` errors: a podman that cannot run (or a malformed container name) is
/// `RuntimeUnavailable`; anything later (e.g. the exec was cut off by a timeout) is a
/// `CompileFailed` whose outcome is unknown — the compiler may or may not have run.
fn exec_failure(target: CompileTarget, error: anyhow::Error) -> ToolError {
    match error.downcast_ref::<RuntimeError>() {
        Some(RuntimeError::PodmanUnavailable(_)) | Some(RuntimeError::InvalidExecName(_)) => {
            runtime_unavailable(target, error)
        }
        _ => {
            let mut error = ToolError::new(
                "CompileFailed",
                "CompileFailed",
                format!("the compiler could not be started: {error}"),
            )
            .retry_safe(false);
            error.outcome_unknown = true;
            error
        }
    }
}

/// mtime advanced: a fresh output counts; a stale one (or none) does not.
pub fn mtime_advanced(after: Option<SystemTime>, before: Option<SystemTime>) -> bool {
    match (after, before) {
        (Some(after), Some(before)) => after > before,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

pub fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Poll `path`'s mtime until it moves past `before` (a fresh output counts) or the deadline.
pub async fn poll_output_updated(path: &Path, before: Option<SystemTime>, deadline: Instant) -> bool {
    loop {
        if mtime_advanced(mtime_of(path), before) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(COMPILE_POLL).await;
    }
}

/// stdout, then a non-empty stderr appended (the parser and the tail see both).
pub fn combine(stdout: &str, stderr: &str) -> String {
    let mut text = stdout.to_string();
    if !stderr.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(stderr);
    }
    text
}

/// Compact stdout+stderr to the last `lines` lines.
pub fn tail(text: &str, lines: usize) -> String {
    let collected: Vec<&str> = text.lines().collect();
    let start = collected.len().saturating_sub(lines);
    collected[start..].join("\n")
}

/// One compiler run parsed from combined stdout+stderr.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CompileSummary {
    /// `Some` when the `OK: <n> compiled, <m> failed, <k> skipped` line appeared.
    pub compiled: Option<i64>,
    pub failed: Option<i64>,
    pub skipped: Option<i64>,
    /// `[FAIL]` lines (a bare one gets the preceding error line): dependency failures that do
    /// NOT count in `failed`.
    pub fail_lines: Vec<String>,
    pub fatal: bool,
}

static OK_LINE: OnceLock<regex::Regex> = OnceLock::new();

/// Parse a compiler run's output. Pure; unit-tested in `tests/model_file.rs`.
pub fn parse_compile_output(text: &str) -> CompileSummary {
    let regex = OK_LINE.get_or_init(|| {
        regex::Regex::new(r"OK: (\d+) compiled, (\d+) failed, (\d+) skipped")
            .expect("ok-line regex")
    });
    let mut summary = CompileSummary::default();
    if let Some(capture) = regex.captures(text) {
        let number = |i: usize| capture[i].parse::<i64>().ok();
        summary.compiled = number(1);
        summary.failed = number(2);
        summary.skipped = number(3);
    }
    // resourcecompiler right-aligns a bare `[FAIL]` on its own line after the error it belongs
    // to: report `[FAIL] <previous non-empty line>` so the caller sees what failed
    let mut previous = "";
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("[FAIL]") {
            let rest = rest.trim();
            summary.fail_lines.push(match (rest.is_empty(), previous.is_empty()) {
                (true, false) => format!("[FAIL] {previous}"),
                _ => trimmed.to_string(),
            });
        } else if !trimmed.is_empty() {
            previous = trimmed;
        }
    }
    summary.fatal = text.contains("FATAL ERROR");
    summary
}
