//! The mock's `job.*` (the bridge's job model, first kind `resourcecompiler`): `job.start`,
//! `job.status`, `job.cancel` with **scriptable outcomes**, advertised by [`crate::mock::MockBuilder::jobs`].
//!
//! The wire shapes follow the bridge contract (`target/orch/unblock-rows/jobs-contract.md`):
//! one running job per bridge (`-32017 PrerequisiteFailed {blockedOn: "job: <id> running"}`), the
//! log under `cs2mcp_jobs/<id>-<startedAtMs>.log` relative to the app dir, `-32602` for bad
//! parameters and unknown ids. Outcomes are scripted per `job.start` ([`MockJobOutcome`]); a job
//! reports `running` for its first `polls` `job.status` answers and then turns terminal (or never,
//! for [`MockJobOutcome::Hang`], which only `job.cancel` or the bridge-side timeout ends).
use serde_json::{json, Value};
use std::collections::VecDeque;

/// How a started job ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockJobOutcome {
    /// `OK: 3 compiled, 0 failed, 1 skipped` -> state `ok`.
    Ok,
    /// `OK: 0 compiled, 0 failed, 1 skipped` -> state `ok`, nothing written (the compiler's skip).
    Skipped,
    /// `OK: 1 compiled, 2 failed` + a `[FAIL]` line -> state `failed`.
    Failed,
    /// The bridge-side deadline ran out -> state `timeout`.
    Timeout,
    /// A `FATAL ERROR` line: state `failed` with `fatal: true` (no OK line, no summary).
    Fatal,
    /// Never ends on its own: `running` until `job.cancel` (state `cancelled`).
    Hang,
}

impl MockJobOutcome {
    fn state(self) -> &'static str {
        match self {
            MockJobOutcome::Ok | MockJobOutcome::Skipped => "ok",
            MockJobOutcome::Failed | MockJobOutcome::Fatal => "failed",
            MockJobOutcome::Timeout => "timeout",
            MockJobOutcome::Hang => "running",
        }
    }
}

/// One scripted job: the outcome and how many `job.status` answers say `running` first.
#[derive(Debug, Clone, Copy)]
pub struct MockJobScript {
    pub outcome: MockJobOutcome,
    pub polls: u32,
}

#[derive(Debug, Clone)]
pub struct MockJob {
    pub id: String,
    pub input: String,
    /// The caller's `clientRef` (echoed by `job.status`, `null` without one).
    pub client_ref: Option<String>,
    pub game: bool,
    pub flags: Vec<String>,
    pub state: String,
    pub script: MockJobScript,
    pub polls_seen: u32,
    /// Unix epoch milliseconds (the bridge's `startedAtMs`).
    pub started_ms: u64,
}

/// The mock's job table.
#[derive(Debug, Default)]
pub struct MockJobs {
    /// Scripts consumed by successive `job.start`s; once empty, `default_script` applies.
    pub scripts: VecDeque<MockJobScript>,
    pub default_script: Option<MockJobScript>,
    /// `Some(blockedOn)`: every `job.start` answers `-32017 {blockedOn}` (Windows-only, exe missing).
    pub unavailable: Option<String>,
    pub seq: u64,
    pub jobs: Vec<MockJob>,
    /// Host files the mock "compiler" writes when a job turns `ok` (the compiled output the
    /// host-side verdict looks at).
    pub writes: Vec<std::path::PathBuf>,
    /// While set, `job.start` answers `-32017 "job: another job is starting"` (the bridge's
    /// reservation window of a concurrent start).
    pub starting: bool,
    /// The content root the bridge checks `input` against ("input does not exist under the
    /// content root"); `None` skips the existence check.
    pub content_root: Option<std::path::PathBuf>,
}

const EXTS: [&str; 11] = [
    "vmap", "vmdl", "vpcf", "vsndevts", "vmat", "vtex", "vsnd", "vsmart", "vanim", "vmks", "js",
];
const PLAIN_FLAGS: [&str; 7] = [
    "-nav", "-fshallow", "-quiet", "-unbufferedio", "-noassert", "-nop4", "-f",
];
const NUMERIC_FLAGS: [(&str, u32, u32); 2] = [("-maxtextureres", 1, 16384), ("-threads", 1, 64)];

type Reply = (Option<Value>, Option<Value>, bool);

fn invalid(reason: &str) -> Reply {
    (
        None,
        Some(json!({"code": -32602, "message": "InvalidParams", "data": {"reason": reason}})),
        false,
    )
}

fn prerequisite(blocked_on: &str) -> Reply {
    (
        None,
        Some(json!({"code": -32017, "message": "PrerequisiteFailed", "data": {"blockedOn": blocked_on}})),
        false,
    )
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn log_of(job: &MockJob) -> Value {
    let file = format!("{}.log", job.id);
    json!({
        "windows": format!("Z:\\app\\cs2mcp_jobs\\{file}"),
        "relative": format!("cs2mcp_jobs/{file}"),
    })
}

fn status_of(job: &MockJob) -> Value {
    let mut out = json!({
        "jobId": job.id,
        "kind": "resourcecompiler",
        "state": job.state,
        "input": job.input,
        "clientRef": job.client_ref,
        "pid": 4242,
        "elapsedMs": unix_ms().saturating_sub(job.started_ms),
        "failLines": [],
        "fatal": false,
        "logTail": "",
        "log": log_of(job),
        "startedAtMs": job.started_ms,
    });
    match (job.state.as_str(), job.script.outcome) {
        ("ok", MockJobOutcome::Skipped) => {
            out["logTail"] = json!("OK: 0 compiled, 0 failed, 1 skipped, 0m:00s");
            out["summary"] = json!({"compiled": 0, "failed": 0, "skipped": 1, "seconds": 0});
            out["exitCode"] = json!(0);
        }
        ("ok", _) => {
            out["logTail"] = json!("Compiling\n OK: 3 compiled, 0 failed, 1 skipped, 0m:05s");
            out["summary"] = json!({"compiled": 3, "failed": 0, "skipped": 1, "seconds": 5});
            out["exitCode"] = json!(0);
        }
        ("failed", MockJobOutcome::Fatal) => {
            out["fatal"] = json!(true);
            out["logTail"] = json!("FATAL ERROR: something fatal");
            out["exitCode"] = json!(1);
        }
        ("failed", _) => {
            out["failLines"] = json!(["[FAIL] dependency failed: maps/missing.vmap"]);
            out["logTail"] = json!("[FAIL] dependency failed: maps/missing.vmap\n OK: 1 compiled, 2 failed, 0 skipped, 0m:00s");
            out["summary"] = json!({"compiled": 1, "failed": 2, "skipped": 0, "seconds": 0});
        }
        ("running", _) | ("cancelled", _) | ("timeout", _) => {
            out["logTail"] = json!("Compiling maps/probe.vmap");
        }
        _ => {}
    }
    out
}

impl MockJobs {
    /// `control.status`'s `jobs` entries: `{id, kind, state}` per job, oldest first.
    pub fn summaries(&self) -> Vec<Value> {
        self.jobs
            .iter()
            .map(|j| json!({"id": j.id, "kind": "resourcecompiler", "state": j.state}))
            .collect()
    }
}

/// The flag allowlist exactly as the bridge's `parse_flags` applies it.
fn parse_flags(list: &[Value]) -> Result<Vec<String>, String> {
    let mut tokens: Vec<String> = Vec::new();
    for item in list {
        tokens.push(item.as_str().ok_or("flags must be an array of strings")?.to_string());
    }
    let mut seen: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let flag = tokens[i].as_str();
        if seen.contains(&flag) {
            return Err(format!("flag {flag} is repeated"));
        }
        if PLAIN_FLAGS.contains(&flag) {
            i += 1;
        } else if let Some(&(_, lo, hi)) = NUMERIC_FLAGS.iter().find(|(name, ..)| *name == flag) {
            let value = tokens.get(i + 1).ok_or_else(|| format!("flag {flag} needs a value"))?;
            let ok = !value.is_empty()
                && value.len() <= 6
                && value.bytes().all(|b| b.is_ascii_digit())
                && value.parse::<u32>().is_ok_and(|n| (lo..=hi).contains(&n));
            if !ok {
                return Err(format!("flag {flag} needs an integer {lo}..={hi}"));
            }
            i += 2;
        } else {
            return Err(format!("flag {flag:?} is not allowed"));
        }
        seen.push(flag);
    }
    Ok(tokens)
}

/// The bridge's `validate_input`.
fn validate_input(input: &str) -> Result<(), String> {
    if input.is_empty() || input.len() > 260 {
        return Err("input must be 1..=260 bytes".to_string());
    }
    let segments: Vec<&str> = input.split('/').collect();
    if segments.len() < 3 || segments[0] != "csgo_addons" {
        return Err("input must be csgo_addons/<addon>/<path>".to_string());
    }
    for segment in &segments {
        let clean = !segment.is_empty()
            && *segment != "."
            && *segment != ".."
            && segment.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
        if !clean {
            return Err(format!("input segment {segment:?} is not allowed (letters, digits, _ . - only; no .. or empty)"));
        }
    }
    let ext = segments[segments.len() - 1]
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    if !ext.is_some_and(|ext| EXTS.contains(&ext.as_str())) {
        return Err(format!("input extension must be one of: {}", EXTS.join(" ")));
    }
    Ok(())
}

/// `jobId` param: `None` when absent/null and not required; anything but a string is the
/// bridge's "jobId is required (a string)".
fn job_id_param(params: &Value, required: bool) -> Result<Option<String>, Reply> {
    match params.get("jobId") {
        Some(Value::String(id)) => Ok(Some(id.clone())),
        None | Some(Value::Null) if !required => Ok(None),
        _ => Err(invalid("jobId is required (a string)")),
    }
}

/// The three `job.*` built-ins (the bridge's order of refusals: platform, params, running,
/// starting).
pub fn builtin(jobs: &mut MockJobs, method: &str, params: &Value) -> Option<Reply> {
    match method {
        "job.start" => {
            if let Some(blocked) = &jobs.unavailable {
                return Some(prerequisite(blocked));
            }
            Some(start(jobs, params))
        }
        "job.status" => Some(match job_id_param(params, false) {
            Err(reply) => reply,
            Ok(None) => {
                let list: Vec<Value> = jobs
                    .jobs
                    .iter()
                    .map(|j| {
                        json!({
                            "jobId": j.id, "kind": "resourcecompiler", "state": j.state,
                            "input": j.input, "clientRef": j.client_ref,
                            "elapsedMs": unix_ms().saturating_sub(j.started_ms),
                            "startedAtMs": j.started_ms, "log": log_of(j),
                        })
                    })
                    .collect();
                (Some(json!({"jobs": list})), None, false)
            }
            Ok(Some(id)) => {
                let writes = jobs.writes.clone();
                let Some(job) = jobs.jobs.iter_mut().find(|j| j.id == id) else {
                    return Some(invalid("unknown jobId"));
                };
                if job.state == "running" {
                    job.polls_seen += 1;
                    if job.script.outcome != MockJobOutcome::Hang && job.polls_seen > job.script.polls {
                        job.state = job.script.outcome.state().to_string();
                        if job.state == "ok" {
                            for path in &writes {
                                if let Some(parent) = path.parent() {
                                    let _ = std::fs::create_dir_all(parent);
                                }
                                let _ = std::fs::write(path, b"compiled");
                            }
                        }
                    }
                }
                (Some(status_of(job)), None, false)
            }
        }),
        "job.cancel" => Some(match job_id_param(params, true) {
            Err(reply) => reply,
            Ok(id) => {
                let id = id.unwrap_or_default();
                let Some(job) = jobs.jobs.iter_mut().find(|j| j.id == id) else {
                    return Some(invalid("unknown jobId"));
                };
                if job.state == "running" {
                    job.state = "cancelled".into();
                    (Some(json!({"cancelled": true, "state": "cancelled"})), None, false)
                } else {
                    (Some(json!({"cancelled": false, "state": job.state})), None, false)
                }
            }
        }),
        _ => None,
    }
}

fn start(jobs: &mut MockJobs, params: &Value) -> Reply {
    let Some(obj) = params.as_object() else {
        return invalid("params must be an object");
    };
    match obj.get("kind").and_then(Value::as_str) {
        Some("resourcecompiler") => {}
        Some(_) => return invalid("kind must be \"resourcecompiler\""),
        None => return invalid("kind is required (\"resourcecompiler\")"),
    }
    let Some(input) = obj.get("input").and_then(Value::as_str) else {
        return invalid("input is required (a content-relative csgo_addons/<addon>/... path)");
    };
    let game = match obj.get("game") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return invalid("game must be a boolean"),
    };
    let flags = match obj.get("flags") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(list)) => match parse_flags(list) {
            Ok(flags) => flags,
            Err(reason) => return invalid(&reason),
        },
        Some(_) => return invalid("flags must be an array of strings"),
    };
    match obj.get("timeoutMs") {
        None | Some(Value::Null) => {}
        Some(v) => match v.as_u64() {
            None => return invalid("timeoutMs must be a non-negative integer"),
            Some(ms) if !(1_000..=3_600_000).contains(&ms) => {
                return invalid("timeoutMs must be 1000..=3600000")
            }
            Some(_) => {}
        },
    }
    let client_ref = match obj.get("clientRef") {
        None | Some(Value::Null) => None,
        Some(Value::String(s))
            if !s.is_empty()
                && s.len() <= 64
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')) =>
        {
            Some(s.clone())
        }
        Some(_) => return invalid("clientRef must be 1..=64 bytes of [A-Za-z0-9_.-]"),
    };
    if let Err(reason) = validate_input(input) {
        return invalid(&reason);
    }
    if let Some(root) = &jobs.content_root {
        if !root.join(input).is_file() {
            return invalid("input does not exist under the content root");
        }
    }
    if let Some(running) = jobs.jobs.iter().find(|j| j.state == "running") {
        return prerequisite(&format!("job: {} running", running.id));
    }
    if jobs.starting {
        return prerequisite("job: another job is starting");
    }
    let script = jobs
        .scripts
        .pop_front()
        .or(jobs.default_script)
        .unwrap_or(MockJobScript { outcome: MockJobOutcome::Ok, polls: 1 });
    jobs.seq += 1;
    let job = MockJob {
        // ids are unique across bridge loads: `j<N>-<nonce>` (opaque to the caller)
        id: format!("j{}-mock{}", jobs.seq, jobs.seq * 7 + 3),
        input: input.to_string(),
        client_ref,
        game,
        flags: flags.clone(),
        state: "running".into(),
        script,
        polls_seen: 0,
        started_ms: unix_ms(),
    };
    let mut command = vec![json!("Z:\\app\\game\\bin\\win64\\resourcecompiler.exe")];
    command.extend(flags.iter().map(|f| json!(f)));
    if game {
        command.push(json!("-game"));
        command.push(json!("Z:\\app\\game\\csgo"));
    }
    command.push(json!("-i"));
    command.push(json!(format!("Z:\\app\\content\\{}", input.replace('/', "\\"))));
    let reply = json!({
        "jobId": job.id,
        "state": "running",
        "pid": 4242,
        "log": log_of(&job),
        "command": command,
        "startedAtMs": job.started_ms,
    });
    jobs.jobs.push(job);
    (Some(reply), None, false)
}

#[cfg(test)]
mod client_ref_tests {
    use super::*;

    fn start_with(jobs: &mut MockJobs, client_ref: Value) -> Reply {
        start(
            jobs,
            &json!({"kind": "resourcecompiler", "input": "csgo_addons/a/maps/x.vmap", "clientRef": client_ref}),
        )
    }

    #[test]
    fn the_client_ref_is_validated_like_the_bridge_and_echoed() {
        let mut jobs = MockJobs::default();
        for bad in [json!(""), json!("a".repeat(65)), json!("has space"), json!(7), json!("a/b")] {
            let reply = start_with(&mut jobs, bad.clone());
            assert!(reply.0.is_none(), "{bad}");
        }
        assert!(jobs.jobs.is_empty());
        let reply = start_with(&mut jobs, json!("hm1-a.b_c"));
        assert!(reply.0.is_some());
        assert_eq!(status_of(&jobs.jobs[0])["clientRef"], json!("hm1-a.b_c"));
        let none = start(
            &mut MockJobs::default(),
            &json!({"kind": "resourcecompiler", "input": "csgo_addons/a/maps/x.vmap"}),
        );
        assert!(none.0.is_some());
    }
}
