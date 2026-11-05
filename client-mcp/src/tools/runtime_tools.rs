//! `launch_session` / `wait_session` / `stop_session`: the docker runtime behind the MCP tools
//! (workflows/01-docker-runtime.md). The runtime owns the session table and does the podman work;
//! this module validates arguments, clamps waits to the call budget, and maps runtime errors to
//! `ToolError`. Errors after a container exists carry `{container, artifactsDir}` in `data`.
use crate::state::ClientState;
use crate::testscript::{harness_suite, stage, valid_addon, valid_map_name, HarnessOpts, Suite};
use cs2_mcp_common::bridge::{BridgeClient, Kind, RequestOpts};
use cs2_mcp_common::config::RuntimeKind;
use cs2_mcp_common::runtime::docker::{
    kind_slug, valid_name, valid_suite_name, ContainerState, RuntimeError, StopOutcome, NAME_PREFIX,
};
use cs2_mcp_common::runtime::readiness::{self, Readiness, ReadyStage};
use cs2_mcp_common::runtime::{LaunchSpec, SessionKind, SessionPaths};
use cs2_mcp_common::tools::{CallCtx, ToolError, ToolOutput};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const DEFAULT_TIMEOUT_MS: u64 = 180_000;
pub const MAX_TIMEOUT_MS: u64 = 900_000;
/// Left unused at the end of every call so the reply goes out inside the tool budget.
const BUDGET_RESERVE: Duration = Duration::from_secs(2);

fn stage_names() -> Vec<&'static str> {
    ReadyStage::ALL.iter().map(|s| s.as_str()).collect()
}

pub fn launch_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": {"type": "string", "enum": ["linux-client", "wine-tools", "wine-client"]},
            "testscript": {"type": "string", "description": "wine-tools: existing suite <app>/vtest/<name>.txt; not with map"},
            "map": {"type": "string", "description": "wine-tools: e.g. de_dust2, editor/zoo/smartprop_zoo; MapLoadFailed if not in game on it in ~10 s"},
            "addon": {"type": "string", "description": "wine-tools only"},
            "args": {"type": "array", "items": {"type": "string"}},
            "headless": {"type": "boolean", "description": "linux-client: true; wine kinds: false"},
            "gpus": {"type": "boolean"},
            "waitFor": {"type": "string", "enum": stage_names(), "description": "default: MenuReached; BridgeHello for wine-tools; Parked with testscript"},
            "bridge": {"type": "boolean", "default": true, "description": "wine-tools: false stops the wait at MenuReached"},
            "connect": {"type": "string", "description": "host:port. linux-client: not working; wine-tools: use client_connect"},
            "timeoutMs": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS},
            "capture": {"type": "boolean", "default": true}
        },
        "required": ["kind"]
    })
}

pub fn wait_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "container": {"type": "string"},
            "waitFor": {"type": "string", "enum": stage_names()},
            "timeoutMs": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS}
        }
    })
}

pub fn stop_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "container": {"type": "string"}
        }
    })
}

pub(crate) fn invalid(msg: &str) -> ToolError {
    ToolError::invalid_arguments(msg)
}

fn opt_bool(args: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(invalid(&format!("{key} must be a boolean"))),
    }
}

pub(crate) fn opt_timeout(args: &Value) -> Result<Option<u64>, ToolError> {
    match args.get("timeoutMs") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_TIMEOUT_MS).contains(n))
            .map(Some)
            .ok_or_else(|| invalid("timeoutMs must be an integer in 1..=900000")),
    }
}

pub(crate) fn opt_wait_for(args: &Value) -> Result<Option<ReadyStage>, ToolError> {
    match args.get("waitFor") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => ReadyStage::parse(s)
            .map(Some)
            .ok_or_else(|| invalid("waitFor must be a readiness stage name")),
        Some(_) => Err(invalid("waitFor must be a readiness stage name")),
    }
}

/// Wait for the cross-process launch lock: what is left of the call budget minus the reserve.
pub fn lock_budget(ctx: &CallCtx) -> Duration {
    ctx.budget.remaining().saturating_sub(BUDGET_RESERVE)
}

/// The wait a caller gets: the requested upper bound, capped to the call budget minus a reserve.
pub(crate) fn wait_within_budget(ctx: &CallCtx, requested_ms: u64) -> Duration {
    let room = ctx.budget.remaining().saturating_sub(BUDGET_RESERVE);
    Duration::from_millis(requested_ms).min(room)
}

pub(crate) fn paths_json(p: &SessionPaths) -> Value {
    json!({
        "appRoot": p.app_root.display().to_string(),
        "discovery": p.discovery.as_ref().map(|d| d.display().to_string()),
        "consoleLog": p.console_log.as_ref().map(|d| d.display().to_string()),
        "pfx": p.pfx.as_ref().map(|d| d.display().to_string()),
    })
}

/// Report a readiness wait. `pending` means the budget or timeout ended the wait; the session is
/// still up and `wait_session` continues it.
pub(crate) fn wait_report(readiness: &Readiness) -> (bool, Option<&'static str>) {
    let pending = !readiness.target_reached && readiness.exited.is_none();
    (
        pending,
        pending.then_some("wait_session {container, waitFor}"),
    )
}

/// The validated argument plumbing of `launch_session`: the kind with the addon folded in, the
/// wine-tools-only refusals applied, `connect` folded into the engine args, and the computed
/// defaults. Pure, so the launch-kind plumbing is testable without a runtime or podman.
#[derive(Debug)]
struct ParsedLaunch {
    kind: SessionKind,
    testscript: Option<String>,
    map: Option<String>,
    addon: Option<String>,
    args: Vec<String>,
    headless: bool,
    gpus: bool,
    capture: bool,
    bridge: bool,
}

fn parse_launch_args(args: &Value) -> Result<ParsedLaunch, ToolError> {
    let kind = match args.get("kind").and_then(Value::as_str) {
        Some("linux-client") => SessionKind::LinuxClient,
        Some("wine-client") => SessionKind::WineClient,
        Some("wine-tools") => SessionKind::WineTools { addon: None },
        _ => {
            return Err(invalid(
                "kind must be \"linux-client\", \"wine-tools\" or \"wine-client\"",
            ))
        }
    };
    let wine_tools = matches!(kind, SessionKind::WineTools { .. });
    let wine = wine_tools || matches!(kind, SessionKind::WineClient);
    let testscript = match args.get("testscript") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if valid_suite_name(s) => Some(s.clone()),
        Some(_) => return Err(invalid("testscript must be a suite name: [A-Za-z0-9_-]+")),
    };
    if testscript.is_some() && !wine_tools {
        return Err(invalid(
            "testscript is wine-tools only (the Linux suite path is an open question, workflows/01)",
        ));
    }
    let map = match args.get("map") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if valid_map_name(s) => Some(s.clone()),
        Some(_) => {
            return Err(invalid(
                "map must be a map name: [A-Za-z0-9_-] segments joined by /, at most 128 bytes",
            ))
        }
    };
    if map.is_some() && !wine_tools {
        return Err(invalid("map is wine-tools only"));
    }
    // map generates the per-map harness: no other testscript, or the POC name "harness"
    if map.is_some() && testscript.as_deref().is_some_and(|t| t != "harness") {
        return Err(invalid(
            "map generates its own harness suite: testscript must be absent or \"harness\"",
        ));
    }
    let addon = match args.get("addon") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if valid_addon(s) => Some(s.clone()),
        Some(_) => return Err(invalid("addon must be [A-Za-z0-9_]{1,64}")),
    };
    if addon.is_some() && !wine_tools {
        return Err(invalid("addon is wine-tools only"));
    }
    let mut extra = Vec::new();
    match args.get("args") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for item in items {
                extra.push(
                    item.as_str()
                        .ok_or_else(|| invalid("args must be an array of strings"))?
                        .to_string(),
                );
            }
        }
        Some(_) => return Err(invalid("args must be an array of strings")),
    }
    if matches!(kind, SessionKind::WineClient) {
        reject_wine_client_reserved(&extra)?;
    }
    if let Some(addr) = super::multiplayer::opt_connect(args)? {
        if wine_tools {
            return Err(invalid(
                "connect is linux-client only; wine tools sessions use client_connect",
            ));
        }
        // a wine client takes +connect as a plain engine command line (like the retail client);
        // there is no bridge console.exec on it yet to do this after the launch
        extra.push("+connect".into());
        extra.push(addr);
    }
    if !wine_tools && args.get("bridge").is_some_and(|v| !v.is_null()) {
        return Err(invalid("bridge is wine-tools only"));
    }
    let headless = opt_bool(args, "headless")?.unwrap_or(!wine);
    let gpus = opt_bool(args, "gpus")?.unwrap_or(wine);
    let capture = opt_bool(args, "capture")?.unwrap_or(true);
    // `bridge` means "wait for the bridge's RPC" and only wine-tools has that: a wine client runs
    // the bridge as a plain Metamod plugin with no listener (the GameFrame tick is open), so its
    // spec carries bridge: false even though Metamod loads the DLL
    let bridge = wine_tools && opt_bool(args, "bridge")?.unwrap_or(true);
    let kind = if wine_tools {
        SessionKind::WineTools {
            addon: addon.clone(),
        }
    } else {
        kind
    };
    Ok(ParsedLaunch {
        kind,
        testscript,
        map,
        addon,
        args: extra,
        headless,
        gpus,
        capture,
        bridge,
    })
}

/// Bare game flag names that would silently turn a wine-client session into a different session
/// kind: `-tools` is the wine-tools launch itself, `-testscript` and `-addon` are the wine-tools
/// parameters. The engine's command line takes them in any case and with a leading `-` or `+`
/// (it strips the sign before comparing, so a bare word counts too). Case-insensitive like
/// `multiplayer::reject_reserved_args` for server_up.
const WINE_CLIENT_RESERVED: &[&str] = &["tools", "testscript", "addon"];

fn reject_wine_client_reserved(extra: &[String]) -> Result<(), ToolError> {
    for a in extra {
        let bare = a.trim_start_matches(['-', '+']);
        let Some(reserved) = WINE_CLIENT_RESERVED
            .iter()
            .find(|r| bare.eq_ignore_ascii_case(r))
        else {
            continue;
        };
        let instead = match *reserved {
            "tools" => "use kind \"wine-tools\" instead".to_string(),
            other => format!("use the {other} parameter instead (wine-tools only)"),
        };
        return Err(invalid(&format!(
            "{a} is not accepted for kind \"wine-client\": {instead}"
        )));
    }
    Ok(())
}

pub async fn launch_session(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    if state.runtime_kind != RuntimeKind::Docker {
        return Err(ToolError::not_implemented(
            "NotImplementedYet",
            "the local runtime is not implemented yet",
            json!({"blockedOn": "workflows/10-local-install.md"}),
        ));
    }
    let p = parse_launch_args(&args)?;
    let ParsedLaunch {
        kind,
        testscript,
        map,
        addon,
        args: extra,
        headless,
        gpus,
        capture,
        bridge,
    } = p;
    // map_workshop only for the addon's own map; any other map loads with plain `map`
    let workshop = match (&addon, &map) {
        (Some(a), Some(m)) if is_addon_map(&state.runtime.wine_app_dir(), a, m) => Some(a.clone()),
        _ => None,
    };
    let harness = map.as_ref().map(|m| {
        harness_suite(&HarnessOpts {
            map: m.clone(),
            tv: true,
            addon: workshop.clone(),
        })
    });
    let testscript = harness.as_ref().map(|s| s.name.clone()).or(testscript);
    let spec = LaunchSpec {
        kind,
        testscript,
        args: extra,
        headless,
        gpus,
        container_name: None,
        capture,
        bridge,
        map: map.clone(),
    };
    let wait_for = opt_wait_for(&args)?.unwrap_or_else(|| readiness::default_wait(&spec));
    let requested_ms = opt_timeout(&args)?.unwrap_or(DEFAULT_TIMEOUT_MS);
    let kind_name = kind_slug(&spec.kind);
    // the runtime's own refusals (headless, display, cap, KindBusy) come before anything is written;
    // an identical running session passes them, since the launch reuses it. Staging precedes the
    // launch, which refuses an unstaged suite. A launch refused after this point (a race) leaves the
    // harness staged, as a library suite does: the file is the same for every launch of this map.
    let staged = match &harness {
        Some(suite) => {
            state
                .runtime
                .preflight_reusable(&spec)
                .await
                .map_err(runtime_error)?;
            Some(stage_harness(&state, suite)?)
        }
        None => None,
    };

    let launched = state
        .runtime
        .launch_within(&spec, lock_budget(&ctx))
        .await
        .map_err(runtime_error)?;
    let container = launched.container.clone();
    let artifacts = launched.artifacts_dir.display().to_string();
    let timeout = wait_within_budget(&ctx, requested_ms);
    let readiness = match state
        .runtime
        .wait_ready(&container, wait_for, timeout)
        .await
    {
        Ok(r) => r,
        Err(e) => Readiness::failed(wait_for, format!("{e:#}")),
    };
    // the map check runs after the wait whatever it found: the refusal is already in console.log,
    // and the game check needs the harness to have parked
    let map_report = match &map {
        Some(m) => Some(
            check_map(
                &state,
                &ctx,
                &container,
                MapCheck {
                    map: m,
                    addon: addon.as_deref(),
                    bridge: spec.bridge,
                    reused: launched.reused,
                    console_log: launched.paths.console_log.as_deref(),
                    parked: readiness.reached.contains(&ReadyStage::Parked),
                    artifacts: &artifacts,
                    readiness: json!(&readiness),
                    argv: json!(&launched.argv),
                },
            )
            .await?,
        ),
        None => None,
    };
    let (pending, next) = wait_report(&readiness);
    let mut out = json!({
        "container": container,
        "kind": kind_name,
        "reused": launched.reused,
        "argv": launched.argv,
        "paths": paths_json(&launched.paths),
        "artifactsDir": artifacts,
        // the capture dir exists only when the layer is mounted (capture: true and a valid layer dir)
        "captureDir": launched.capture_dir.as_deref().map(|d| d.display().to_string()),
        "readiness": readiness,
        "pending": pending,
    });
    if let Some(addon) = &addon {
        out["addon"] = json!(addon);
    }
    if let (Some(map), Some(path), Some(report)) = (&map, &staged, &map_report) {
        out["map"] = json!(map);
        out["harness"] = json!(harness.as_ref().map(|s| s.name.clone()));
        out["mapCommand"] = json!(if workshop.is_some() {
            "map_workshop"
        } else {
            "map"
        });
        out["stagedPath"] = json!(path.display().to_string());
        out["mapVerified"] = json!(report.verified);
        if let Some(note) = &report.note {
            out["mapNote"] = json!(note);
        }
        if let Some(seen) = &report.seen {
            out["mapSeen"] = seen.clone();
        }
    }
    if let Some(n) = next {
        out["next"] = json!(n);
    }
    Ok(out.into())
}

/// Stage the per-map harness under the wine app tree, as `run_testscript` stages a library suite.
/// A missing app tree is refused first: staging would create one.
fn stage_harness(state: &ClientState, suite: &Suite) -> Result<PathBuf, ToolError> {
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
    stage(&app_root, suite)
        .map_err(|e| ToolError::internal(format!("staging the harness failed: {e:#}")))
}

/// Longest window the map check polls `game.local` in (the call budget caps it further).
const MAP_WINDOW: Duration = Duration::from_secs(10);
/// Deadline of one `game.local` request made by the map check.
const MAP_REQUEST_DEADLINE: Duration = Duration::from_secs(2);
/// Pause between two `game.local` requests of the map check.
const MAP_PACE: Duration = Duration::from_millis(250);

/// The map is the addon's own (`<app>/game/csgo_addons/<addon>/maps/<map>.vpk`). Only then does the
/// harness load it with `map_workshop <addon> <map>`; any other map loads with plain `map`.
fn is_addon_map(app_root: &Path, addon: &str, map: &str) -> bool {
    app_root
        .join("game/csgo_addons")
        .join(addon)
        .join("maps")
        .join(format!("{map}.vpk"))
        .is_file()
}

/// What the map check is given: the request, and what the launch established.
struct MapCheck<'a> {
    map: &'a str,
    addon: Option<&'a str>,
    /// the session has a bridge (`bridge: true`); without one there is no game check
    bridge: bool,
    /// the launch reused a running session: its console was not read by this launch
    reused: bool,
    console_log: Option<&'a Path>,
    /// the wait reached `Parked`: the harness ran past its map-load wait
    parked: bool,
    artifacts: &'a str,
    readiness: Value,
    argv: Value,
}

/// The outcome of a map check that did not fail the launch.
struct MapReport {
    verified: bool,
    note: Option<String>,
    /// what `game.local` last said, when it was asked
    seen: Option<Value>,
}

impl MapReport {
    fn unverified(note: impl Into<String>) -> Self {
        MapReport {
            verified: false,
            note: Some(note.into()),
            seen: None,
        }
    }
}

/// Console lines that say the engine refused `map`: `map cs_script_demo:  invalid map name` (live,
/// with -addon). The map is matched as `" <map>:"`, so `de_dust2` does not match `de_dust2_old`,
/// and a `/` in the name is literal (`editor/zoo/smartprop_zoo`).
fn invalid_map_lines(console: &str, map: &str) -> Vec<String> {
    let name = format!(" {map}:");
    console
        .lines()
        .filter(|l| l.contains("invalid map name") && l.contains(&name))
        .take(5)
        .map(|l| l.trim().to_string())
        .collect()
}

/// The part of console.log before the harness's first map-load mark. A refusal after it comes from
/// a later command (a manual `map` in a session someone keeps using), not from this launch's load.
fn before_map_marks(console: &str) -> &str {
    const MARKS: [&str; 2] = [
        "CheckPoint -> 'VTEST_MAP_LOADED'",
        "CheckPoint -> 'VTEST_PARKED'",
    ];
    // cut at the start of the line that holds the mark (`TESTSCRIPT: CheckPoint -> ...`)
    let cut = match MARKS.iter().filter_map(|m| console.find(m)).min() {
        Some(at) => console[..at].rfind('\n').map_or(0, |n| n + 1),
        None => console.len(),
    };
    &console[..cut]
}

/// One `game.local` answer, as the map check reads it.
#[derive(Debug, Clone, PartialEq)]
enum LocalSample {
    Local {
        in_game: bool,
        map: Option<String>,
    },
    /// the bridge answered with an error (NoLocalPlayer, ...): by its name
    Refused(String),
    /// never answered: busy, transport, or the method is not there
    Silent(String),
}

impl LocalSample {
    fn answered(&self) -> bool {
        !matches!(self, LocalSample::Silent(_))
    }

    /// In game, on exactly this map (case-insensitive: the engine's casing is not ours).
    fn is_on(&self, map: &str) -> bool {
        match self {
            LocalSample::Local {
                in_game: true,
                map: Some(seen),
            } => seen.eq_ignore_ascii_case(map),
            _ => false,
        }
    }

    fn seen(&self) -> Value {
        match self {
            LocalSample::Local { in_game, map } => json!({"inGame": in_game, "map": map}),
            LocalSample::Refused(name) => json!({"error": name}),
            LocalSample::Silent(why) => json!({"error": why}),
        }
    }

    fn describe(&self) -> String {
        match self {
            LocalSample::Local { in_game, map } => {
                format!("inGame {in_game} on map {:?}", map.as_deref().unwrap_or(""))
            }
            LocalSample::Refused(name) => format!("error {name}"),
            LocalSample::Silent(why) => format!("no answer ({why})"),
        }
    }
}

/// How the poll ended.
#[derive(Debug, PartialEq)]
enum Verdict {
    /// the game reported the map, in game
    Verified(LocalSample),
    /// answered, but never on the map: the last answer
    Failed(LocalSample),
    /// never answered within the window: the last reason
    Unanswered(String),
}

/// The verdict over every sample taken: any sample on the map verifies (a NoLocalPlayer before the
/// player spawned does not count against it); otherwise the last answer decides.
fn poll_verdict(history: &[LocalSample], map: &str) -> Verdict {
    if let Some(hit) = history.iter().find(|s| s.is_on(map)) {
        return Verdict::Verified(hit.clone());
    }
    if let Some(last) = history.iter().rev().find(|s| s.answered()) {
        return Verdict::Failed(last.clone());
    }
    let why = match history.last() {
        Some(LocalSample::Silent(why)) => why.clone(),
        _ => "the window had no time for a request".to_string(),
    };
    Verdict::Unanswered(why)
}

/// One `game.local` request under the bridge lock, classified. Busy and transport failures and a
/// method the bridge does not have are silence; any other error is an answer.
async fn sample_local(state: &ClientState, deadline_ms: u64) -> LocalSample {
    let mut bridge = state.bridge.lock().await;
    match bridge
        .request("game.local", json!({}), RequestOpts::deadline(deadline_ms))
        .await
    {
        Ok(r) => LocalSample::Local {
            in_game: r.result.get("inGame").and_then(Value::as_bool) == Some(true),
            map: r
                .result
                .get("map")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        // only NoLocalPlayer says something about the map; timeouts (OutcomeUnknown), a schema
        // not resolved yet right after the load, busy or transport errors are silence
        Err(e) => match e.kind {
            Kind::NoLocalPlayer => LocalSample::Refused(e.kind.name().to_string()),
            _ => LocalSample::Silent(format!("{}: {}", e.kind.name(), e.message)),
        },
    }
}

/// Poll `game.local` until it reports the map in game or the window ends. Requests are paced by
/// [`MAP_PACE`], each has a deadline of at most [`MAP_REQUEST_DEADLINE`], and the window is
/// [`MAP_WINDOW`] or what the call budget has left, whichever is less.
async fn poll_game_local(state: &ClientState, ctx: &CallCtx, map: &str) -> Verdict {
    let window = MAP_WINDOW.min(ctx.budget.remaining().saturating_sub(BUDGET_RESERVE));
    let started = Instant::now();
    let mut history = Vec::new();
    loop {
        let left = window.saturating_sub(started.elapsed());
        if left.is_zero() {
            break;
        }
        let deadline_ms = MAP_REQUEST_DEADLINE.min(left).as_millis().max(1) as u64;
        let sample = sample_local(state, deadline_ms).await;
        let done = sample.is_on(map);
        history.push(sample);
        if done {
            break;
        }
        let left = window.saturating_sub(started.elapsed());
        if left.is_zero() {
            break;
        }
        tokio::time::sleep(MAP_PACE.min(left)).await;
    }
    poll_verdict(&history, map)
}

/// `MapLoadFailed` (not retry-safe: identical arguments reuse this session and fail the same way).
/// The session stays up, so the error names it, with its readiness and argv.
fn map_load_failed(check: &MapCheck, container: &str, message: String, extra: Value) -> ToolError {
    let mut data = json!({
        "container": container,
        "artifactsDir": check.artifacts,
        "map": check.map,
        "addon": check.addon,
        "next": "stop_session {container}",
        "readiness": check.readiness,
        "argv": check.argv,
    });
    if let (Some(d), Value::Object(e)) = (data.as_object_mut(), extra) {
        d.extend(e);
    }
    ToolError::new(
        "MapLoadFailed",
        "MapLoadFailed",
        format!("{message}; the session {container} is left running (stop_session {container})"),
    )
    .retry_safe(false)
    .with_data(data)
}

/// Did the engine load the requested map? In order:
/// 1. a reused session is not re-verified (a previous launch may have failed the check; the note says so).
/// 2. the console before the harness's first map-load mark has the engine's refusal for this map:
///    fail, whatever the harness did after it (live: the harness still parks after `map <invalid>`).
/// 3. the harness parked and the bridge is up: poll `game.local` until it is in game on the
///    requested map. A window that ends on a wrong answer fails; one that never got an answer
///    leaves the check unverified, with the reason.
async fn check_map(
    state: &ClientState,
    ctx: &CallCtx,
    container: &str,
    check: MapCheck<'_>,
) -> Result<MapReport, ToolError> {
    if check.reused {
        return Ok(MapReport::unverified(
            "reused session; not re-verified (a previous launch of this session may have failed its map check)",
        ));
    }
    if let Some(path) = check.console_log {
        if let Ok(bytes) = tokio::fs::read(path).await {
            // console.log is engine output: a stray byte must not hide a refusal line
            let text = String::from_utf8_lossy(&bytes);
            let lines = invalid_map_lines(before_map_marks(&text), check.map);
            if !lines.is_empty() {
                let message = format!(
                    "the engine refused map {:?}: {}",
                    check.map,
                    lines.join(" | ")
                );
                return Err(map_load_failed(
                    &check,
                    container,
                    message,
                    json!({"evidence": lines}),
                ));
            }
        }
    }
    if !check.parked {
        return Ok(MapReport::unverified(
            "the wait did not reach Parked, so the map load is not confirmed (game.local not asked)",
        ));
    }
    if !check.bridge {
        return Ok(MapReport::unverified(
            "bridge false: no game.local check; the harness parked, the map is not confirmed",
        ));
    }
    {
        let bridge = state.bridge.lock().await;
        if let Err(why) = bridge_session_gate(state, container, &bridge).await {
            return Ok(MapReport::unverified(format!(
                "game.local not checked: {why}"
            )));
        }
    }
    match poll_game_local(state, ctx, check.map).await {
        Verdict::Verified(seen) => Ok(MapReport {
            verified: true,
            note: None,
            seen: Some(seen.seen()),
        }),
        Verdict::Failed(last) => Err(map_load_failed(
            &check,
            container,
            format!(
                "game.local did not show map {:?} in game: last answer {}",
                check.map,
                last.describe()
            ),
            json!({"seen": last.seen()}),
        )),
        Verdict::Unanswered(why) => Ok(MapReport::unverified(format!(
            "game.local never answered within the window, map not confirmed: {why}"
        ))),
    }
}

pub async fn wait_session(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let owned = match args.get("container") {
        None | Some(Value::Null) => state.runtime.last_owned().ok_or_else(no_session)?,
        Some(Value::String(name)) => {
            if !valid_name(name) {
                return Err(invalid("container must match ^cs2mcp-[a-z0-9][a-z0-9-]*$"));
            }
            state.runtime.owned_named(name).ok_or_else(|| {
                runtime_error(
                    RuntimeError::NoSession(format!("{name} was not launched by this server"))
                        .into(),
                )
            })?
        }
        Some(_) => return Err(invalid("container must be a string")),
    };
    let wait_for = opt_wait_for(&args)?.unwrap_or_else(|| readiness::default_wait(&owned.spec));
    let requested_ms = opt_timeout(&args)?.unwrap_or(DEFAULT_TIMEOUT_MS);
    let timeout = wait_within_budget(&ctx, requested_ms);
    let readiness = match state
        .runtime
        .wait_ready(&owned.name, wait_for, timeout)
        .await
    {
        Ok(r) => r,
        Err(e) => Readiness::failed(wait_for, format!("{e:#}")),
    };
    let (pending, next) = wait_report(&readiness);
    let mut out = json!({
        "container": owned.name,
        "readiness": readiness,
        "pending": pending,
    });
    if let Some(n) = next {
        out["next"] = json!(n);
    }
    Ok(out.into())
}

/// Longest one best-effort input-hook request may take inside `stop_session`.
const INPUT_CLEAR_DEADLINE_MS: u64 = 2_000;

/// Outcome of the best-effort input-hook teardown before a session is removed.
#[derive(Default)]
struct InputTeardown {
    cleared: bool,
    uninstalled: bool,
    /// every reason something was skipped or failed, in order
    notes: Vec<String>,
}

impl InputTeardown {
    fn skipped(note: &str) -> Self {
        InputTeardown {
            notes: vec![note.to_string()],
            ..Default::default()
        }
    }

    fn note(&self) -> Option<String> {
        (!self.notes.is_empty()).then(|| self.notes.join("; "))
    }
}

const NOT_BRIDGE_SESSION: &str = "target is not the bridge's session; input hook untouched";

/// Is `target` the session the bridge talks to? Only a running wine-tools container whose /app
/// mount holds the bridge's discovery file. Anything else sends nothing, and the reason is returned.
pub(crate) async fn bridge_session_gate(
    state: &ClientState,
    target: &str,
    bridge: &BridgeClient,
) -> Result<(), String> {
    // a `cs2mcp-native` server (`--capture-dir`): its one session owns the bridge it attaches to
    if state.capture_dir.is_some() && target == crate::tools::capture::NATIVE_SESSION {
        return Ok(());
    }
    let entry = match state.runtime.session_entry(target).await {
        Ok(Some(e)) => e,
        Ok(None) => return Err(NOT_BRIDGE_SESSION.into()),
        Err(e) => {
            return Err(format!(
                "could not inspect {target} ({e:#}); input hook untouched"
            ))
        }
    };
    if entry.kind != "wine-tools" {
        return Err(NOT_BRIDGE_SESSION.into());
    }
    let running = matches!(
        state.runtime.container_state(target).await,
        Ok(ContainerState::Running)
    );
    if !running {
        return Err("target is not a running wine-tools session; input hook untouched".into());
    }
    match &entry.app_dir {
        Some(app) if bridge.discovery_path().starts_with(app) => Ok(()),
        _ => Err(NOT_BRIDGE_SESSION.into()),
    }
}

/// `input.clear`, then `input.uninstall` when advertised. Only over a connected bridge (no connect
/// attempt here); every failure is recorded in `notes` and never stops the stop. Caller holds the
/// bridge lock, taken before this computes its deadlines.
async fn teardown_input(bridge: &mut BridgeClient, ctx: &CallCtx) -> InputTeardown {
    if !bridge.connected() {
        return InputTeardown::skipped("bridge not connected; input hook not cleared");
    }
    if !bridge.has("input.clear") {
        return InputTeardown::skipped("bridge does not advertise input.clear");
    }
    let mut out = InputTeardown::default();
    let step = |ctx: &CallCtx| {
        ctx.budget
            .deadline_ms()
            .map(|ms| ms.min(INPUT_CLEAR_DEADLINE_MS))
    };
    let cleared = match step(ctx) {
        Ok(ms) => bridge
            .request("input.clear", json!({}), RequestOpts::deadline(ms))
            .await
            .map_err(|e| e.message),
        Err(e) => Err(e.message),
    };
    match cleared {
        Ok(_) => out.cleared = true,
        Err(why) => out.notes.push(format!("input.clear failed: {why}")),
    }
    if bridge.has("input.uninstall") {
        match step(ctx) {
            Ok(ms) => {
                match bridge
                    .request("input.uninstall", json!({}), RequestOpts::deadline(ms))
                    .await
                {
                    Ok(_) => out.uninstalled = true,
                    Err(e) => out
                        .notes
                        .push(format!("input.uninstall failed: {}", e.message)),
                }
            }
            Err(e) => out
                .notes
                .push(format!("input.uninstall failed: {}", e.message)),
        }
    }
    out
}

pub async fn stop_session(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let target = match args.get("container") {
        None | Some(Value::Null) => state
            .runtime
            .last_owned()
            .map(|o| o.name)
            .ok_or_else(no_session)?,
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(_) => return Err(invalid("container must be a non-empty string")),
    };
    if !target.starts_with(NAME_PREFIX) {
        return Err(runtime_error(RuntimeError::ForeignContainer(target).into()));
    }
    if !valid_name(&target) {
        return Err(runtime_error(RuntimeError::InvalidName(target).into()));
    }
    // Input hook first (AGENT.md rule 7). The lock is taken before the budget is read, so the
    // deadlines the teardown uses are computed when the bridge is actually ours to talk to.
    let input = {
        let mut bridge = state.bridge.lock().await;
        match bridge_session_gate(&state, &target, &bridge).await {
            Ok(()) => teardown_input(&mut bridge, &ctx).await,
            Err(note) => InputTeardown::skipped(&note),
        }
    };
    // Containers run without --rm, so logs stay until this removes them. The stop is never a kill:
    // podman stops the container and this waits for the exit within what the budget has left.
    let within = stop_within(&ctx, state.runtime.short_timeout());
    let report = state
        .runtime
        .stop_named_report(&target, within)
        .await
        .map_err(runtime_error)?;
    let mut out = match report.outcome {
        StopOutcome::Removed => json!({"stopped": target, "alreadyGone": false}),
        StopOutcome::AlreadyGone => json!({"stopped": target, "alreadyGone": true}),
        StopOutcome::Pending {
            state: st,
            stop_issued_ms,
        } => json!({
            "stopped": false,
            "pending": true,
            "container": target,
            "state": st,
            "stopIssuedMs": stop_issued_ms,
            "next": "stop_session {container}",
        }),
    };
    // ms since the stop began. exitedMs is when podman reported the exit: on a stalled container
    // store that is long after the game died (live: died at ~1.5 s, reported at 93.7 s), as are
    // the cleanup and removal after it
    out["timing"] = json!(report.timing);
    out["inputCleared"] = json!(input.cleared);
    out["inputUninstalled"] = json!(input.uninstalled);
    if let Some(note) = input.note() {
        out["inputNote"] = json!(note);
    }
    Ok(out.into())
}

/// What a stop may wait for: the call budget minus the reserve, and the removal's own podman timeout.
fn stop_within(ctx: &CallCtx, short_timeout: Duration) -> Duration {
    ctx.budget
        .remaining()
        .saturating_sub(BUDGET_RESERVE)
        .saturating_sub(short_timeout)
}

fn no_session() -> ToolError {
    runtime_error(RuntimeError::NoSession("this server has not launched a session".into()).into())
}

/// Map a runtime failure to the tool error taxonomy (`retrySafe` only where nothing was started).
pub fn runtime_error(e: anyhow::Error) -> ToolError {
    let message = format!("{e:#}");
    match e.downcast_ref::<RuntimeError>() {
        Some(RuntimeError::TooManySessions { running, max }) => {
            ToolError::new("TooManySessions", "TooManySessions", message)
                .retry_safe(true)
                .with_data(json!({
                    "runningContainers": running,
                    "max": max,
                    "hint": "each CS2 instance needs GPU memory; about 3 fit at 1080p on this host. Foreign cs2* containers (cs2vtest, cs2tools, ...) count too. stop_session one first, or raise --max-sessions / CS2_MCP_MAX_SESSIONS if the GPU has room",
                }))
        }
        Some(RuntimeError::KindBusy { kind, container, shared }) => {
            ToolError::new("KindBusy", "KindBusy", message)
                .retry_safe(true)
                .with_data(json!({"kind": kind, "container": container, "shared": shared}))
        }
        Some(RuntimeError::ForeignContainer(name)) => {
            ToolError::new("ForeignContainer", "ForeignContainer", message)
                .retry_safe(true)
                .with_data(json!({"container": name, "allowedPrefix": NAME_PREFIX}))
        }
        Some(RuntimeError::InvalidName(_)) => invalid(&message),
        Some(RuntimeError::InvalidExecName(_)) => invalid(&message),
        Some(RuntimeError::NoSession(_)) => {
            ToolError::new("NoSession", "NoSession", message).retry_safe(true)
        }
        Some(RuntimeError::Unsupported(_)) => {
            ToolError::new("NotSupported", "NotSupported", message).retry_safe(true)
        }
        Some(RuntimeError::MissingXauthority(_)) => {
            ToolError::new("DisplayUnavailable", "DisplayUnavailable", message).retry_safe(true)
        }
        Some(RuntimeError::SuiteNotStaged(path)) => {
            ToolError::new("SuiteNotStaged", "SuiteNotStaged", message)
                .retry_safe(true)
                .with_data(json!({"path": path}))
        }
        Some(RuntimeError::StopPending { .. }) => {
            ToolError::new("StopPending", "StopPending", message).retry_safe(true)
        }
        Some(RuntimeError::PodmanUnavailable(bin)) => {
            ToolError::new("PodmanUnavailable", "PodmanUnavailable", message)
                .retry_safe(true)
                .with_data(json!({"podman": bin}))
        }
        Some(RuntimeError::PodmanTimeout { .. }) => {
            // a `run` may have started a container before the timeout: outcome unknown
            let mut err = ToolError::new("PodmanTimeout", "PodmanTimeout", message);
            err.outcome_unknown = true;
            err
        }
        Some(RuntimeError::LaunchLockTimeout { .. }) => {
            ToolError::new("LaunchLockTimeout", "LaunchLockTimeout", message).retry_safe(true)
        }
        Some(RuntimeError::Podman { .. }) => ToolError::new("RuntimeError", "PodmanFailed", message),
        Some(RuntimeError::InstallBusy { stderr }) => {
            ToolError::new("InstallBusy", "InstallBusy", message)
                .retry_safe(true)
                .with_data(json!({"stderr": stderr}))
        }
        Some(RuntimeError::InstallFailed { code, stderr, .. }) => {
            ToolError::new("InstallFailed", "InstallFailed", message)
                .retry_safe(true)
                .with_data(json!({"exitCode": code, "stderr": stderr}))
        }
        Some(RuntimeError::InstallTimeout { secs, .. }) => {
            // the tree may be half-repaired: not retry-safe until someone looked
            ToolError::new("InstallTimeout", "InstallTimeout", message)
                .retry_safe(false)
                .with_data(json!({
                    "timeoutSecs": secs,
                    "hint": "run `bridge/install-rs.sh status` to check the tree first",
                }))
        }
        Some(RuntimeError::InstallScriptMissing { path }) => {
            // a configuration error (this client-mcp has no install script): retrying the same
            // launch fails the same way until the configuration is fixed
            ToolError::new("InstallScriptMissing", "InstallScriptMissing", message)
                .retry_safe(false)
                .with_data(json!({"path": path}))
        }
        None => ToolError::new("RuntimeError", "RuntimeError", message),
    }
}

#[cfg(test)]
mod map_check_tests {
    use super::*;

    fn local(in_game: bool, map: Option<&str>) -> LocalSample {
        LocalSample::Local {
            in_game,
            map: map.map(str::to_string),
        }
    }

    #[test]
    fn refusal_lines_name_the_map_exactly() {
        let console = "map cs_script_demo:  invalid map name\nunrelated\n";
        assert_eq!(
            invalid_map_lines(console, "cs_script_demo"),
            vec!["map cs_script_demo:  invalid map name".to_string()]
        );
        // changelevel and map_workshop lines match the same way
        let console =
            "changelevel de_dust2:  invalid map name\nmap_workshop a b:  invalid map name\n";
        assert_eq!(invalid_map_lines(console, "de_dust2").len(), 1);
    }

    #[test]
    fn slashes_are_literal_in_map_names() {
        let console = "map editor/zoo/smartprop_zoo:  invalid map name\n";
        assert_eq!(
            invalid_map_lines(console, "editor/zoo/smartprop_zoo").len(),
            1
        );
        // the same leaf under another path is a different map
        assert!(invalid_map_lines(console, "smartprop_zoo").is_empty());
        assert!(invalid_map_lines(console, "zoo/smartprop_zoo").is_empty());
    }

    #[test]
    fn near_misses_do_not_match() {
        // a longer name that starts with the requested one
        let console = "map de_dust2_old:  invalid map name\n";
        assert!(invalid_map_lines(console, "de_dust2").is_empty());
        // the same name without the refusal text (a load that worked)
        assert!(invalid_map_lines("map de_dust2:  loading\n", "de_dust2").is_empty());
        // the name inside another word: no space before it
        assert!(invalid_map_lines("xde_dust2:  invalid map name\n", "de_dust2").is_empty());
        // the name, but no colon after it
        assert!(invalid_map_lines("map de_dust2 invalid map name\n", "de_dust2").is_empty());
    }

    #[test]
    fn only_the_text_before_the_first_map_mark_is_scanned() {
        let console = "map a:  invalid map name\nTESTSCRIPT: CheckPoint -> 'VTEST_MAP_LOADED'\nmap b:  invalid map name\n";
        assert_eq!(before_map_marks(console), "map a:  invalid map name\n");
        let parked = "x\nTESTSCRIPT: CheckPoint -> 'VTEST_PARKED'\ny\n";
        assert_eq!(before_map_marks(parked), "x\n");
        assert_eq!(before_map_marks("no marks yet"), "no marks yet");
    }

    #[test]
    fn map_matching_ignores_case_but_not_in_game_state() {
        assert!(local(true, Some("CS_SCRIPT_DEMO")).is_on("cs_script_demo"));
        assert!(!local(false, Some("cs_script_demo")).is_on("cs_script_demo"));
        assert!(!local(true, None).is_on("cs_script_demo"));
        assert!(!local(true, Some("de_dust2")).is_on("cs_script_demo"));
        assert!(!LocalSample::Refused("NoLocalPlayer".into()).is_on("x"));
    }

    #[test]
    fn a_no_local_player_before_the_spawn_does_not_fail_a_later_success() {
        let history = [
            LocalSample::Refused("NoLocalPlayer".into()),
            LocalSample::Refused("NoLocalPlayer".into()),
            local(true, Some("cs_script_demo")),
        ];
        assert!(matches!(
            poll_verdict(&history, "cs_script_demo"),
            Verdict::Verified(_)
        ));
    }

    #[test]
    fn the_last_answer_decides_a_failure() {
        let persistent = [local(false, Some("")), local(false, Some(""))];
        assert_eq!(
            poll_verdict(&persistent, "cs_script_demo"),
            Verdict::Failed(local(false, Some("")))
        );
        let wrong_map = [
            LocalSample::Refused("NoLocalPlayer".into()),
            local(true, Some("de_dust2")),
            LocalSample::Silent("busy".into()),
        ];
        assert_eq!(
            poll_verdict(&wrong_map, "cs_script_demo"),
            Verdict::Failed(local(true, Some("de_dust2")))
        );
    }

    #[test]
    fn silence_for_the_whole_window_is_unverified_not_failed() {
        let silent = [
            LocalSample::Silent("busy".into()),
            LocalSample::Silent("connection lost".into()),
        ];
        assert_eq!(
            poll_verdict(&silent, "cs_script_demo"),
            Verdict::Unanswered("connection lost".into())
        );
    }

    #[test]
    fn a_stale_busy_answer_before_a_real_one_is_not_a_failure() {
        let history = [
            LocalSample::Silent("busy".into()),
            local(false, Some("")),
            LocalSample::Silent("busy".into()),
        ];
        // the last answered sample is the one that says what the game is doing
        assert_eq!(
            poll_verdict(&history, "cs_script_demo"),
            Verdict::Failed(local(false, Some("")))
        );
    }
}

#[cfg(test)]
mod launch_args_tests {
    use super::*;

    #[test]
    fn wine_client_kind_maps_with_wine_defaults_and_no_bridge() {
        let p = parse_launch_args(&json!({"kind": "wine-client"})).unwrap();
        assert!(matches!(p.kind, SessionKind::WineClient));
        // a wine session wants the host X and the GPUs, like wine-tools
        assert!(!p.headless);
        assert!(p.gpus);
        // `bridge` means "wait for the bridge's RPC"; a wine client has none
        assert!(!p.bridge);
    }

    #[test]
    fn wine_client_refuses_the_wine_tools_only_arguments() {
        for (args, field) in [
            (json!({"kind": "wine-client", "testscript": "sandbox"}), "testscript"),
            (json!({"kind": "wine-client", "map": "de_dust2"}), "map"),
            (json!({"kind": "wine-client", "addon": "poc"}), "addon"),
            (json!({"kind": "wine-client", "bridge": false}), "bridge"),
        ] {
            let err = parse_launch_args(&args).unwrap_err();
            assert!(err.to_string().contains(field), "{err} for {args}");
        }
    }

    #[test]
    fn wine_client_connect_becomes_a_plus_connect_engine_arg() {
        let p = parse_launch_args(&json!({"kind": "wine-client", "connect": "127.0.0.1:27015"}))
            .unwrap();
        assert_eq!(p.args, vec!["+connect", "127.0.0.1:27015"]);
        // wine-tools keeps the refusal: its console goes through client_connect
        let err = parse_launch_args(&json!({"kind": "wine-tools", "connect": "127.0.0.1:27015"}))
            .unwrap_err();
        assert!(err.to_string().contains("client_connect"), "{err}");
    }

    #[test]
    fn wine_client_refuses_the_session_kind_flags_whatever_their_case_or_sign() {
        for a in ["-tools", "+TOOLS", "Tools", "-testscript", "+TestScript", "ADDON", "addon"] {
            let err = parse_launch_args(&json!({"kind": "wine-client", "args": [a]}))
                .unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("wine-client"), "{a}: {msg}");
        }
    }

    #[test]
    fn the_wine_client_refusals_name_the_right_kind_or_parameter() {
        // -tools would make it a wine-tools session
        let err = parse_launch_args(&json!({"kind": "wine-client", "args": ["-tools"]}))
            .unwrap_err();
        assert!(err.to_string().contains("wine-tools"), "{err}");
        // -testscript / -addon are the wine-tools parameters of the same names
        for (a, param) in [("-testscript", "testscript"), ("+addon", "addon")] {
            let err = parse_launch_args(&json!({"kind": "wine-client", "args": [a, "x"]}))
                .unwrap_err();
            assert!(
                err.to_string().contains(&format!("{param} parameter")),
                "{a}: {err}"
            );
        }
        // a wine-tools session may pass -tools back (it is redundant, not a kind change), and a
        // linux-client has no such kind confusion: free args stay free there
        assert!(parse_launch_args(&json!({"kind": "wine-tools", "args": ["-tools"]})).is_ok());
        assert!(parse_launch_args(&json!({"kind": "linux-client", "args": ["-tools"]})).is_ok());
        // anything else stays accepted for wine-client
        assert!(parse_launch_args(&json!({"kind": "wine-client", "args": ["-toolsmode", "1", "-tickrate", "128"]})).is_ok());
    }
}
