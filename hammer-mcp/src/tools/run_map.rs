//! `run_map` (WP-R): load the **COMPILED** map in the engine of the running tools session.
//!
//! Route (the proven recipe, `~/.tmp/defuse-poc/HANDOFF.md` "Playtest", `playtest/pt_lib.py`
//! `vcmd`, `~/.tmp/defuse-poc/compile_map.sh`):
//!
//! * one console command per VToolMessage `command` payload — `vtmsend.exe` reads the kv3 file
//!   from the app dir and hands the line to the engine's command channel
//!   ([`cs2_mcp_common::runtime::docker::vtm_command_payload`], [`vtmsend_program_argv`]). The
//!   bridge's own `console.exec` is **not** the route: it answers `-32011` in a tools session
//!   that was not launched with `-testscript` (the runtime brings the tools up without one);
//! * the load verb is `map <name> nomapvalidation=1`. A plain `map <name>` fails the engine's
//!   map-name validator ("invalid map name"), and the name is a raw console argument, so this
//!   tool validates it strictly ([`NAME_CHARS`]) before it ever reaches a payload.
//!   **Never `maps`** — that console command crashes the engine (2026-10-07 evidence, repeated in
//!   the POC handoff); nothing in this module builds it.
//! * the optional `gameType`/`gameMode` are sent **first**, each as its **own** post
//!   (`game_type N`, then `game_mode M`; defuse used `0`/`1`), because a console command issued
//!   while a level is loading crashed the server once (`playtest/load_map.py` order) and whether
//!   the `command` channel splits a line on `;` is unverified (review-1 L4) — the proven recipe
//!   posts the two lines separately;
//! * **a map the engine already has mounted is unmounted first** (review-1 H1). The engine keeps
//!   serving the copy it loaded when the `.vpk` is recompiled under it (HANDOFF "Playtest" item
//!   1), and the proven cure is the playtest-2 `step4_reload.py` loop: `disconnect` → wait for
//!   the engine's own idle-level line (`[Server] SV: Spawn Server: <empty>`, 120 s in that
//!   script) → then post the map command. Whether a map is up comes from `game.local` (m2.9,
//!   `{inGame:false, map:"<empty>"}` at the idle level — playtest 3 preflight) when the bridge
//!   exposes it, and from this server's own [`LastLoad`] memory when it does not. The remount is
//!   reported (`remount`) and a `disconnect` whose idle-level line is never seen means the map
//!   command is **not** sent: a console command while a level loads is the documented crash
//!   trigger, so that ends as `state:"failed"` with nothing else dispatched;
//! * **one load at a time** (review-1 M6). A `map` posted into a loading engine is the same crash
//!   trigger, and `podman exec` timing out (or a `crun` teardown) kills only the *client*:
//!   `vtmsend.exe` may still have delivered. So a post is never repeated once it may have
//!   reached the engine (only the `crun` spawn failure, which cannot have, is retried once), a
//!   post whose exec was cut off is reported `delivered:false` and turns a timeout into
//!   `state:"unknown"`, and a call that starts while this server's previous load has no verdict
//!   waits for it (`game/csgo/console.log`, then `game.local`) and refuses `EngineBusy` when it
//!   is still unresolved. The in-flight record is written only once the `map` post (or a
//!   `disconnect` that can be followed by nothing) has actually gone out — a post that FAILED
//!   before delivery leaves no record and no excuse (review-2 N3);
//! * every post is serialized by the server-wide `HammerState::vtm_gate` (review-1 M7): the
//!   payload path is a single fixed file that `vtmsend.exe` reads, so two write-then-exec pairs
//!   must not interleave — and the mount decision, the previous-record settle and the idle-line
//!   matching all RE-READ their inputs under that gate (a fresh `game.local`, a fresh offset
//!   for the remount, review-2 N2);
//! * the load is watched, not awaited: the tool takes its console.log offset (FRESH, under the
//!   gate, right before its `disconnect` — review-2 N2) and polls for
//!   `Loading map "<name>"` and then
//!   `OnSwitchLoopModeFinished( game : success )` (or a failure line) until the deadline. The
//!   live playtest saw the success line 9 s after the command; the default deadline is 60 s. The
//!   success (and every failure but the engine's own map-name validator) is only accepted **after
//!   that line**: a leftover success line from an earlier load says nothing about this one
//!   (review-1 M5);
//! * a load that fails, or one this call did not see finish, is a **result** (`state: "failed"` /
//!   `"timeout"` / `"unknown"`, `ok:false`) carrying what the console showed — never an error: the
//!   console command was already delivered, and a retry re-runs the load.
//!
//! What it does **not** do: it does not touch the Hammer document (no `expectDoc`, no write gate —
//! this tool changes no document state, so holding the gate would only block real edits for the
//! length of a load), it does not save anything, and it does not compile: the `.vpk` must already
//! exist (`compile_map`), which is why a missing one is refused before anything is sent
//! (`VpkNotFound`) and why a `.vpk` older than its `.vmap` is reported as `staleCompile`. Nothing
//! here invalidates the mesh cache either: a load moves neither the document's undo state nor its
//! geometry (the playtest read the same `epoch`/`undoDepth` before and after), so the entries this
//! server wrote are still the document's.
//!
//! What happens to the Hammer document while the map runs is only half verified: the defuse
//! playtest loaded `defuse_poc` in the `cs2tools` session *with* that document open and read the
//! same `doc.active` (`epoch:1`, `undoDepth:48`, `dirty:false`) before and after, so the load did
//! not touch it. What is **not** verified is what a bridge request sees *during* the load: the
//! engine load freezes the frame loop, so reads are expected to stall until it finishes. The tool
//! therefore calls `note_external_job` while it watches, which makes the blocked gate report
//! `EngineBusy` (an engine load, not a modal dialog) for the whole window — the bridge lists no
//! `running` job for it, which is exactly what the flag handles (review-1 L1) — and releases the
//! grace as soon as the load settles. The orchestrator verifies that live.
use crate::state::HammerState;
use cs2_mcp_common::bridge::{Kind, RequestOpts};
use cs2_mcp_common::runtime::docker::{
    vtm_command_payload, vtmsend_program_argv, write_payload_atomic, RuntimeError,
};
use cs2_mcp_common::runtime::readiness::CHANNEL_UP_MARKER;
use cs2_mcp_common::tools::{
    Budget, CallCtx, Mutability::Mutating, Registry, ToolError, ToolOutput, ToolSpec,
};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::compile::{mtime_of, CompileTarget};

/// Default and maximum wall clock for one engine load (`timeoutMs`).
pub const RUN_TIMEOUT_MS: u64 = 60_000;
pub const RUN_TIMEOUT_MAX_MS: u64 = 600_000;
/// Bounded console.log poll (the runtime's own podman cadence). Never a blind sleep: the deadline
/// is the tool budget.
pub const CONSOLE_POLL: Duration = Duration::from_millis(250);
/// The longest `note_job` grace one load may claim (the POC's `job_grace_s` default is 120).
const JOB_GRACE_MAX: Duration = Duration::from_secs(120);
/// How long the `disconnect` of a remount may take to reach the engine's idle level. The playtest
/// script waited 120 s (`step4_reload.py` `wait_console("Spawn Server: <empty>", 120)`); the
/// engine was there in ~3 s, so this is the recipe's bound, clamped to the tool budget.
pub const REMOUNT_TIMEOUT_MS: u64 = 120_000;
/// How long a call waits for *this server's* previous, unresolved load to settle before it refuses
/// `EngineBusy` (the load may be finishing right now: the live playtest saw 9-14 s).
pub const SETTLE_TIMEOUT_MS: u64 = 20_000;
/// How long an unresolved load record keeps counts as "possibly in flight" after the call that
/// started it returned. Past this the record is stale (the engine would have finished or hung) and
/// a new call proceeds with a note instead of refusing `EngineBusy` forever.
pub const SETTLE_STALE_MS: u64 = 60_000;
/// Console lines kept in the result's tail.
const CONSOLE_TAIL_LINES: usize = 40;
/// A map name is a raw console argument: this is the whole character set it may use. The engine's
/// own names are `[A-Za-z0-9_-]`; `/` is the sub-directory separator of an addon map
/// (`maps/sub/x.vmap` -> `sub/x`). A space, `"`, `;` or any control byte would let the caller
/// write extra console commands, and cannot be escaped through this route.
const NAME_CHARS: &str = "A-Za-z0-9_/-";
/// Map-name length bound (the addon path gate allows 512 for the whole `maps/…vmap`).
const NAME_MAX: usize = 128;
/// How much of `vtmsend.exe`'s output is kept per post (diagnostics).
const VTM_OUTPUT_MAX: usize = 400;
/// The engine's own unmount verb.
const DISCONNECT_COMMAND: &str = "disconnect";

/// `unload: true` (the caller's argument, validated): `run_map {unload:true}` unloads the map the
/// engine has mounted and stops there — it is the primitive the compile_map `map_in_game` refusal
/// points at, and the way to make the engine pick up a recompiled `.vpk` without loading another.
fn unload_requested(args: &Value) -> Result<bool, ToolError> {
    match args.get("unload") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "unload must be a boolean, got {other}"
        ))),
    }
}

/// The VPK header of a compiled map as seen live (`34 12 AA 55`, version 2, tree size): a file
/// that does not start with it, or whose tree runs past the file, would FATAL the engine (LIVE-P1,
/// a 4 KiB random file) and end the tools session. `None` = looks fine, or could not be read (then
/// the engine decides, as before). Only the 12-byte header is checked; deeper corruption is not.
fn vpk_header_problem(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut head = [0u8; 12];
    if len < 12 {
        return Some(format!("only {len} bytes, shorter than a VPK header"));
    }
    file.read_exact(&mut head).ok()?;
    let word = |at: usize| u32::from_le_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
    if word(0) != 0x55AA_1234 {
        return Some("it does not start with the VPK signature 0x55AA1234".to_string());
    }
    let version = word(4);
    if version != 2 {
        return Some(format!("VPK version {version}, the compiler writes 2"));
    }
    let tree = u64::from(word(8));
    if 28 + tree > len {
        return Some(format!("its directory tree ({tree} bytes) runs past the end of the {len}-byte file"));
    }
    None
}

/// LIVE-P1 minor: loading a map whose `compile_map` job is still running loads a half-written (or
/// the old) vpk. Refused with the job's id and a `job_status` next step, unless the caller passes
/// `ignoreRunningCompile:true`.
fn refuse_while_compiling(state: &HammerState, content_path: &str, args: &Value) -> Result<(), ToolError> {
    match args.get("ignoreRunningCompile") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(true)) => return Ok(()),
        Some(Value::Bool(false)) => {}
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "ignoreRunningCompile must be a boolean, got {other}"
            )))
        }
    }
    let Some(job_id) = state.compile_jobs.running_for_path(&state.addon(), content_path) else {
        return Ok(());
    };
    Err(ToolError::new(
        "CompileRunning",
        "EngineBusy",
        format!(
            "compile job {job_id} for {content_path} is still running: the .vpk is being \
             rewritten, so loading it now would run the old (or a half-written) map. Wait for the \
             job (job_status {{jobId:\"{job_id}\"}}) and run_map again, or pass \
             ignoreRunningCompile:true to load the current .vpk anyway"
        ),
    )
    .with_data(json!({"reason": "compile_running", "jobId": job_id, "path": content_path}))
    .use_instead("job_status", "wait for the compile to finish", Some(json!({"jobId": job_id})))
    .retry_safe(true))
}

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "run_map",
            "Load the COMPILED map in the tools session's engine and wait for its load signal. Not for: compiling (compile_map first; a .vpk older than the .vmap is staleCompile). \
             An already-mounted map is unmounted first. path (maps/<x>.vmap) or the active document; unsaved edits are not included. gameType/gameMode are posted first (defuse: 0/1). \
             unload:true only unmounts. state ok|failed|timeout|unknown|unconfirmed (a timeout is not an error). One load at a time (EngineBusy). \
             The player starts on team 0 and dead: pass team \"ct\"|\"t\" to join and wait until alive (team.skipped: load unconfirmed, no join).",
            Mutating,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "maps/<x>.vmap; default the active document. Not with unload"
                },
                "unload": {
                    "type": "boolean",
                    "description": "unmount the current map instead of loading; no-op outside a map"
                },
                "ignoreRunningCompile": {
                    "type": "boolean",
                    "description": "load even though a compile_map job for this map is running"
                },
                "gameType": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "posted as game_type N (0..=255)"
                },
                "gameMode": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "posted as game_mode M (0..=255; 1 = competitive)"
                },
                "team": super::team::team_schema(),
                "restartIfDead": super::team::restart_if_dead_schema(),
                "timeoutMs": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "load-signal watch, default 60000, max 600000; clamped to the tool budget"
                }
            },
            "additionalProperties": false
        })),
        run_map,
    );
}

// -------------------------------------------------------------------------------------------
// Arguments
// -------------------------------------------------------------------------------------------

/// `timeoutMs` as the caller asked it: `None` when absent/null (the default applies). A negative or
/// fractional number is refused, never rounded up to the maximum: the caller asked for something
/// this tool cannot honour. A larger value is at most [`RUN_TIMEOUT_MAX_MS`].
fn requested_timeout(args: &Value) -> Result<Option<u64>, ToolError> {
    match args.get("timeoutMs") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            let ms = n.as_u64().filter(|ms| *ms > 0).ok_or_else(|| {
                ToolError::invalid_arguments(format!("timeoutMs must be an integer > 0, got {n}"))
            })?;
            Ok(Some(ms.min(RUN_TIMEOUT_MAX_MS)))
        }
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "timeoutMs must be an integer, got {other}"
        ))),
    }
}

/// Did the caller pass an explicit `timeoutMs`? (The default is chosen to FIT the tool budget, so a
/// cut default is silent; only a cut *request* is flagged in the result — see [`run_timeout`].)
pub fn timeout_is_explicit(args: &Value) -> bool {
    args.get("timeoutMs").is_some_and(Value::is_number)
}

/// `timeoutMs`: at most [`RUN_TIMEOUT_MAX_MS`], never beyond the remaining tool budget (a zero
/// remainder is budget exhaustion, not a "forever" wait). The second half of the answer says
/// whether the budget — not the caller's number — set the deadline: an EXPLICIT request that got
/// cut is reported (`budgetClamped` + a note), while the DEFAULT already fits the budget silently
/// (it is `min(RUN_TIMEOUT_MS, what is left)`, and the effective value is what the result's
/// `timeoutMs` reports) — a default call is not a caller mistake and must not carry a note.
pub fn run_timeout(args: &Value, budget: &Budget) -> Result<(Duration, bool), ToolError> {
    let explicit = requested_timeout(args)?;
    let requested = Duration::from_millis(explicit.unwrap_or(RUN_TIMEOUT_MS));
    let effective = requested.min(budget.remaining());
    if effective.is_zero() {
        return Err(ToolError::budget_exceeded(budget));
    }
    Ok((effective, explicit.is_some() && requested > effective))
}

/// `gameType`/`gameMode` -> the mode lines sent **before** the map command, in a fixed order, one
/// post each (`["game_type 0", "game_mode 1"]`) like the proven recipe (`step4_reload.py` posts
/// `game_type 0` and `game_mode 1` separately); whether the `command` channel splits a line on `;`
/// is unverified, and if it does not, the cvars would silently not be set (review-1 L4). Both
/// values are console arguments like the map name, so both are validated here.
fn mode_command(args: &Value) -> Result<Vec<String>, ToolError> {
    let mut parts: Vec<String> = Vec::new();
    for (key, command) in [("gameType", "game_type"), ("gameMode", "game_mode")] {
        match args.get(key) {
            None | Some(Value::Null) => {}
            Some(Value::Number(n)) => {
                let value = n.as_u64().filter(|value| *value <= 255).ok_or_else(|| {
                    ToolError::invalid_arguments(format!(
                        "{key} must be an integer 0..=255, got {n}"
                    ))
                })?;
                parts.push(format!("{command} {value}"));
            }
            Some(other) => {
                return Err(ToolError::invalid_arguments(format!(
                    "{key} must be an integer 0..=255, got {other}"
                )))
            }
        }
    }
    Ok(parts)
}

/// The engine name of an addon map: `maps/<rest>.vmap` -> `<rest>`, refused unless it is a
/// canonical `maps/…vmap` path and every character is in [`NAME_CHARS`] (a console line is not a
/// place for quoting games).
pub fn map_name_of(content_path: &str) -> Result<String, ToolError> {
    let bad = |why: &str| {
        ToolError::invalid_arguments(format!(
            "{content_path:?} cannot be a console map name: {why} (allowed: {NAME_CHARS})"
        ))
        .with_data(json!({"path": content_path, "allowed": NAME_CHARS}))
    };
    let Some(rest) = content_path
        .strip_prefix("maps/")
        .and_then(|rest| rest.strip_suffix(".vmap"))
    else {
        return Err(bad("it must be a maps/<x>.vmap path"));
    };
    if rest.is_empty() || rest.len() > NAME_MAX {
        return Err(bad("1..128 chars"));
    }
    if rest.contains("..") {
        return Err(bad("no '..'"));
    }
    if !rest
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/'))
    {
        return Err(bad("every character must be a letter, digit, '_', '-' or the '/' of a \
                         sub-directory"));
    }
    if rest.split('/').any(str::is_empty) {
        return Err(bad("no empty path segment"));
    }
    Ok(rest.to_string())
}

// -------------------------------------------------------------------------------------------
// Targets
// -------------------------------------------------------------------------------------------

/// The resolved target of a `run_map` call: the addon-relative `.vmap`, the engine name, and the
/// compiled `.vpk` that will actually be loaded.
#[derive(Debug)]
struct Target {
    content_path: String,
    name: String,
    vmap: PathBuf,
    /// The vpk's host path (the compiler writes it under `<app>/game/`), the form that was found,
    /// and its addon-relative spelling for the result.
    vpk: PathBuf,
    vpk_relative: String,
    vpk_form: &'static str,
    /// The two candidates, in the order they were tried (the error names both).
    vpk_candidates: Vec<PathBuf>,
    vpk_found: bool,
    vmap_found: bool,
    /// The `.vpk` is older than the `.vmap` it was compiled from: the engine would run the old map.
    stale_compile: bool,
    /// Why the flattened `maps/<stem>.vpk` candidate was **rejected** for a sub-directory map
    /// (review-1 M8): `"absent"`, `"source-missing"` (the `.vmap` is not on the host, so the
    /// candidate cannot be attributed to it) or `"older"` (it predates the `.vmap`). `None` when
    /// there was nothing to reject (the content-relative vpk exists, or this is a top-level map).
    flattened_rejected: Option<&'static str>,
}

impl Target {
    fn vmap_mtime(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.vmap).ok()?.modified().ok()
    }

    fn vpk_mtime(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.vpk).ok()?.modified().ok()
    }
}

/// The active document's map as an addon-content-relative `maps/…vmap`. The default target of
/// `run_map`, the same way `open_map`'s `.vmap` is the document's file.
fn active_content_path(state: &HammerState, doc: &Value) -> Result<String, ToolError> {
    let path = doc.get("path").and_then(Value::as_str).unwrap_or("");
    if path.is_empty() {
        return Err(ToolError::invalid_arguments(
            "the active document has no file path (an untitled map), so it names no compiled map \
             to run: pass path (maps/<x>.vmap), or save the document first with save_map",
        )
        .with_data(json!({"path": Value::Null, "reason": "untitled"})));
    }
    let host = state.path_map.windows_to_host(path).ok_or_else(|| {
        ToolError::new(
            "PathNotMapped",
            "PathNotMapped",
            format!(
                "cannot map the active document's Windows path {path:?} to a host path, so its \
                 compiled map cannot be found: set CS2_MCP_PATH_MAP (e.g. 'z:\\app=<host path>') \
                 or pass path explicitly. Nothing was sent."
            ),
        )
        .with_code(-32602)
        .with_data(json!({"windowsPath": path}))
        .retry_safe(true)
    })?;
    let relative = state.content().addon_rel(&host).ok_or_else(|| {
        let hint = state.content().other_addon_hint(&host, state.addon_selection().1);
        ToolError::new(
            "InvalidPath",
            "InvalidPath",
            format!(
                "the active document's host path {} is not under the active addon's content root \
                 ({}), so run_map cannot address its compiled map: pass path explicitly{}. \
                 Nothing was sent.",
                host.display(),
                state.content().addon_root().display(),
                hint.map(|h| format!(" ({h})")).unwrap_or_default()
            ),
        )
        .with_code(-32602)
        .with_data(json!({"hostPath": host.display().to_string()}))
        .retry_safe(true)
    })?;
    if !relative.to_ascii_lowercase().ends_with(".vmap") {
        return Err(ToolError::invalid_arguments(format!(
            "the active document is not a .vmap ({relative}): pass path of a map to run"
        ))
        .with_code(-32602));
    }
    // the engine name comes out of this path (`map_name_of`): an active document outside the
    // addon's maps/ directory names no map this route can load
    map_name_of(&relative)?;
    Ok(relative)
}

/// Resolve the compiled `.vpk` of `content_path` under
/// `<app>/game/csgo_addons/<addon>/` through the ONE shared candidate helper
/// ([`super::compile_map::vpk_candidates`], review-1 M8/d): `compile_map` derives the output
/// locations of a job with it, so the two tools cannot disagree about where a compiled map lives.
///
/// Two spellings exist, and which one is accepted decides the engine name:
///
/// * the **content-relative** one (`maps/sub/x.vmap` -> `maps/sub/x.vpk`), the shape the
///   compiler's own content->game mapping produces, loaded as `sub/x` — always preferred;
/// * the **flattened** one (`maps/x.vpk`, `compile_map.sh`'s `OUT=.../maps/${MAPNAME##*/}.vpk`),
///   which that script uses for a sub-directory map and which the engine can only address as `x`.
///   It is **not** evidence that this map was compiled (review-1 M8): an unrelated top-level
///   `maps/x.vmap` may be the map that vpk belongs to, and running it would silently load the
///   wrong map. So for a sub-directory map it is accepted only when the content-relative one does
///   not exist *and* it is **newer** than the `.vmap` it is being attributed to *and* no
///   top-level `maps/<stem>.vmap` SOURCE exists (the completion of the same review's rule, its
///   M8 "partly" item: a fresh top-level compile of `maps/x.vmap` is exactly the unrelated vpk
///   a newer-than-vmap check alone would still accept). Otherwise the
///   call is `VpkNotFound` and `target.flattened_rejected` says why. `vpk.form` reports which
///   spelling was used (`content-relative` / `flattened`).
///
/// A caller that never compiles a sub-directory map sees the two as identical (the helper
/// deduplicates them into the one file). Both names are validated, so the reporter can always name
/// the file it looked for.
fn resolve_vpk(state: &HammerState, content_path: &str) -> Result<Target, ToolError> {
    // `name` is the map's path *inside* maps/ (`sub/x`), which is also the engine name in the
    // content-relative form
    let name = map_name_of(content_path)?;
    let candidates = super::compile_map::vpk_candidates(&state.content().app_root, &state.addon(), content_path)
        .filter(|candidates| !candidates.is_empty())
        .ok_or_else(|| {
            ToolError::invalid_arguments(format!(
                "{content_path:?} names no compiled map of the active addon ({}): it must be a \
                 maps/<x>.vmap",
                state.addon()
            ))
        })?;
    let content = &candidates[0];
    let flattened = candidates.get(1);
    let vmap = state.content().addon_root().join(content_path);
    let vmap_found = vmap.is_file();
    let vmap_mtime = std::fs::metadata(&vmap).ok().and_then(|m| m.modified().ok());
    // only a NEWER flattened vpk can be attributed to this `.vmap` (M8)
    let flat_attributable = match (
        flattened.and_then(|candidate| mtime_of(&candidate.path)),
        vmap_mtime,
    ) {
        (Some(flat), Some(vmap)) => flat > vmap,
        _ => false,
    };
    // ... and only when no top-level `maps/<stem>.vmap` source exists (the flattened vpk could
    // be THAT map's compile; M8's completed rule). `name` is `sub/x`: its stem is `x`.
    let stem = name.rsplit_once('/').map(|(_, stem)| stem).unwrap_or(&name);
    let top_level_source = flattened.is_some_and(|_| {
        state
            .content()
            .addon_root()
            .join(format!("maps/{stem}.vmap"))
            .is_file()
    });
    let (chosen, flattened_rejected) = match flattened {
        // a top-level map: the helper deduplicated the two spellings into one file
        None => (content, None),
        Some(_) if content.path.is_file() => (content, None),
        Some(flat) if flat.path.is_file() && flat_attributable && !top_level_source => (flat, None),
        Some(flat) => {
            let why = if top_level_source {
                "top-level-source"
            } else if !flat.path.is_file() {
                "absent"
            } else if !vmap_found {
                "source-missing"
            } else {
                "older"
            };
            (content, Some(why))
        }
    };
    let stale_compile = match (
        std::fs::metadata(&chosen.path).ok().and_then(|m| m.modified().ok()),
        vmap_mtime,
    ) {
        (Some(vpk), Some(vmap)) => vpk < vmap,
        _ => false,
    };
    Ok(Target {
        content_path: content_path.to_string(),
        name: if chosen.form == "flattened" {
            chosen.name.clone()
        } else {
            name
        },
        vmap,
        vpk_found: chosen.path.is_file(),
        vpk: chosen.path.clone(),
        vpk_relative: chosen.relative.clone(),
        vpk_form: chosen.form,
        vpk_candidates: candidates.iter().map(|c| c.path.clone()).collect(),
        vmap_found,
        stale_compile,
        flattened_rejected,
    })
}

/// What `report_ready` needs to know about a map's compiled `.vpk`: the SAME lookup (`resolve_vpk`),
/// header check (`vpk_header_problem`) and staleness rule `run_map` applies, so the ship check and the
/// load cannot disagree.
#[derive(Debug)]
pub(crate) struct VpkStatus {
    pub(crate) path: PathBuf,
    pub(crate) relative: String,
    pub(crate) found: bool,
    pub(crate) stale: bool,
    pub(crate) header_problem: Option<String>,
    pub(crate) vpk_mtime: Option<SystemTime>,
    pub(crate) vmap_mtime: Option<SystemTime>,
    pub(crate) bytes: Option<u64>,
}

/// [`VpkStatus`] of `content_path` (`maps/<x>.vmap`, active addon).
pub(crate) fn vpk_status(state: &HammerState, content_path: &str) -> Result<VpkStatus, ToolError> {
    let target = resolve_vpk(state, content_path)?;
    let header_problem = if target.vpk_found { vpk_header_problem(&target.vpk) } else { None };
    Ok(VpkStatus {
        vpk_mtime: target.vpk_mtime(),
        vmap_mtime: target.vmap_mtime(),
        bytes: std::fs::metadata(&target.vpk).ok().map(|m| m.len()),
        path: target.vpk,
        relative: target.vpk_relative,
        found: target.vpk_found,
        stale: target.stale_compile,
        header_problem,
    })
}

// -------------------------------------------------------------------------------------------
// The console channel
// -------------------------------------------------------------------------------------------

/// `VpkNotFound`: the map was never compiled. Nothing was sent, so a retry after a compile is
/// exactly the right move. A rejected flattened candidate is named with its reason (review-1 M8).
fn vpk_not_found(target: &Target) -> ToolError {
    let rejected = match target.flattened_rejected {
        Some("older") => " The flattened candidate maps/<stem>.vpk exists but is OLDER than the \
                          .vmap, so it cannot be this map's compile (it is probably another, \
                          top-level map's): it was not accepted.",
        Some("source-missing") => " The flattened candidate maps/<stem>.vpk exists but the .vmap \
                                   is not on the host at all, so it cannot be attributed to this \
                                   map: it was not accepted.",
        Some("top-level-source") => " The flattened candidate maps/<stem>.vpk is newer than the \
                                     .vmap, but a top-level maps/<stem>.vmap ALSO exists: that \
                                     vpk could be THAT map's compile (the same stem), so it was \
                                     not accepted. Compile the sub-directory map (mode full) and \
                                     use its content-relative output.",
        _ => "",
    };
    let missing = if target.vmap_found {
        ""
    } else {
        " The source .vmap does not exist on the host either: new_map creates it from a template."
    };
    let variant = if target.vmap_found { None } else { vmap_case_variant(target) };
    let error = ToolError::new(
        "VpkNotFound",
        "VpkNotFound",
        format!(
            "the compiled map {} does not exist: the engine loads the .vpk, not the .vmap, so \
             compile it first with compile_map (path {:?}).{rejected}{missing} Nothing was sent.",
            target.vpk.display(),
            target.content_path
        ),
    )
    .with_data(json!({
        "path": target.content_path,
        "vpkHostPath": target.vpk.display().to_string(),
        "vpkCandidates": target.vpk_candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "flattenedRejected": target.flattened_rejected,
    }))
    .retry_safe(true);
    if let Some(real) = variant {
        // the .vmap exists under another spelling (this host is case-sensitive, Windows is not)
        return error.did_you_mean([real]);
    }
    error.use_instead(
        if target.vmap_found { "compile_map" } else { "new_map" },
        if target.vmap_found {
            "compile the map to a .vpk first"
        } else {
            "the .vmap does not exist in any spelling: create it from a template first"
        },
        Some(json!({"path": target.content_path})),
    )
    .retry_safe(true)
}

/// The real spelling of the target's `.vmap` when only the case differs from `content_path`.
fn vmap_case_variant(target: &Target) -> Option<String> {
    let mut root = target.vmap.clone();
    for _ in target.content_path.split('/') {
        root.pop();
    }
    super::new_map::case_variant(&root, &target.content_path)
}

/// `RuntimeUnavailable`: no running tools container to post the console command in.
fn runtime_unavailable(detail: impl std::fmt::Display) -> ToolError {
    ToolError::new(
        "RuntimeUnavailable",
        "RuntimeUnavailable",
        format!(
            "no tools container to run a map in: {detail} (no running container with label {}; \
             start one with {}; or name one with --tools-container)",
            CompileTarget::ANY_TOOLS.label,
            CompileTarget::ANY_TOOLS.hint
        ),
    )
    .with_data(json!({
        "label": CompileTarget::ANY_TOOLS.label,
        "hint": CompileTarget::ANY_TOOLS.hint,
    }))
    .retry_safe(true)
}

/// What a Windows host's posts report as their `container`: there is none, the post goes into
/// this logon session's VToolMessage channel directly (crates/cs2-vtm, [`post_payload`]).
pub(crate) const NATIVE_SESSION: &str = "native";

/// The running tools session, discovered exactly like the file-route compile tools do it: the
/// `--tools-container` override, else the one running container carrying the label. A Windows
/// host has no containers: its session is [`NATIVE_SESSION`] (the game `cs2mcp-native up`
/// started on this desktop).
pub(crate) async fn tools_container(state: &HammerState) -> Result<String, ToolError> {
    if cfg!(windows) {
        return Ok(NATIVE_SESSION.to_string());
    }
    match &state.tools_container {
        Some(name) => Ok(name.clone()),
        None => match state
            .runtime
            .container_by_label(CompileTarget::ANY_TOOLS.label)
            .await
        {
            Ok(Some(container)) => Ok(container),
            Ok(None) => Err(runtime_unavailable(
                "no running container carries the label",
            )),
            Err(error) => Err(runtime_unavailable(error)),
        },
    }
}

/// The engine console the tools session writes (`<app>/game/csgo/console.log`; pt_lib.py
/// `CONSOLE`, `SessionPaths::console_log`). A Windows host has no container app dir: the
/// session is `cs2mcp-native`'s, whose `-condebug` writes into the install `--app-root` names
/// (the container layout under `--project-root` was watched there before, seen live).
pub(crate) fn console_log_path(state: &HammerState) -> PathBuf {
    if cfg!(windows) {
        return state.content().app_root.join("game/csgo/console.log");
    }
    state
        .runtime
        .wine_paths()
        .console_log
        .unwrap_or_else(|| state.content().app_root.join("game/csgo/console.log"))
}

/// One post: the command, whether `vtmsend` confirmed the VToolMessage channel, whether the exec
/// came back at all (`delivered`: only then has `vtmsend.exe` certainly run, review-1 M6), whether
/// the post had to be repeated (the transient `crun` spawn failure), and its output.
fn post_report(command: &str, output: &str, retried: bool, delivered: bool) -> Value {
    let confirmed = output.contains(CHANNEL_UP_MARKER);
    json!({
        "command": command,
        "confirmed": confirmed,
        "delivered": delivered,
        "retried": retried,
        "output": truncate(output, VTM_OUTPUT_MAX),
    })
}

/// podman's wording for a container it does not know (`compile.rs`'s rule).
fn no_such_container(stderr: &str) -> bool {
    stderr.contains("no such container") || stderr.contains("no container with name or ID")
}

/// The `crun: … status: No such file or directory` teardown race (`compile_map.sh` "transient …
/// retry once"): podman's own wording, so a post that hit it is repeated instead of reported.
fn transient_crun(stderr: &str) -> bool {
    stderr.contains("crun") && stderr.contains("status: No such file or directory")
}

/// Post one console command through the VToolMessage channel: write the kv3 payload where
/// `vtmsend.exe` reads it and run vtmsend in the tools container. The caller holds
/// `HammerState::vtm_gate` across the whole post sequence (review-1 M7: the payload path is one
/// fixed file).
///
/// **A post is never repeated once it may have been delivered** (review-1 M6): a `podman exec`
/// timeout kills only the podman *client*, `vtmsend.exe` in the container can still deliver, and a
/// second `map <name>` posted into a loading engine is the documented crash trigger. The only
/// repeat is the transient `crun` **spawn** failure, which cannot have reached the container. A
/// timed-out post comes back as a report with `delivered:false` (never as a tool error).
pub(crate) async fn post_command(
    state: &HammerState,
    container: &str,
    command: &str,
) -> Result<Value, ToolError> {
    post_payload(state, container, &vtm_command_payload(command), command).await
}

/// [`post_command`] for any VToolMessage payload (`open_map` posts `open_asset` through it);
/// `label` is what the report's `command` field shows. Same gate and no-repeat rules.
pub(crate) async fn post_payload(
    state: &HammerState,
    container: &str,
    payload: &str,
    command: &str,
) -> Result<Value, ToolError> {
    if cfg!(windows) && container == NATIVE_SESSION {
        return post_native(payload, command).await;
    }
    write_payload_atomic(&state.content().app_root, payload).map_err(|error| {
        ToolError::new(
            "RuntimeUnavailable",
            "RuntimeUnavailable",
            format!(
                "cannot write the VToolMessage payload into {}: {error} (the app dir must be \
                 mounted/writable; --project-root points at it)",
                state.content().app_root.display()
            ),
        )
        .retry_safe(true)
    })?;
    let argv = vtmsend_program_argv();
    let mut retried = false;
    loop {
        let run = state
            .runtime
            .exec_in(container, &argv, state.runtime.short_timeout())
            .await;
        match run {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !retried && !output.status.success() && transient_crun(&stderr) {
                    retried = true;
                    continue;
                }
                if !output.status.success() && no_such_container(&stderr) {
                    return Err(runtime_unavailable(stderr.trim()));
                }
                let text = super::compile::combine(&stdout, &stderr);
                if !output.status.success() && text.contains("OpenMutex failed") {
                    // the VToolMessage mutex does not exist: this is not a -tools session (or the
                    // engine has not created the channel yet), so nothing reached the engine
                    return Err(ToolError::new(
                        "RuntimeUnavailable",
                        "RuntimeUnavailable",
                        format!(
                            "the VToolMessage channel is not up in {container}: vtmsend answered \
                             {text:?} (the engine creates the channel at toolframework2 Init; \
                             check the session with session_status). Nothing was sent.",
                        ),
                    )
                    .with_data(json!({"container": container, "output": truncate(&text, VTM_OUTPUT_MAX)}))
                    .retry_safe(true));
                }
                return Ok(post_report(command, &text, retried, true));
            }
            Err(error) => match error.downcast_ref::<RuntimeError>() {
                Some(RuntimeError::PodmanUnavailable(_)) | Some(RuntimeError::InvalidExecName(_)) => {
                    return Err(runtime_unavailable(error))
                }
                _ => {
                    // the exec itself was cut off (a loaded host, the crun teardown race, or a
                    // podman timeout): `vtmsend.exe` may or may not have delivered the command.
                    // Only the spawn failure is known not to have (the retry above), so anything
                    // else is reported as `delivered:false` and NOT repeated — the console watch
                    // below decides the outcome, and an undecided one ends `state:"unknown"`
                    // instead of telling the caller to re-run blindly (review-1 M6).
                    let text = error.to_string();
                    if !retried && transient_crun(&text) {
                        retried = true;
                        continue;
                    }
                    return Ok(post_report(command, &text, retried, false));
                }
            },
        }
    }
}

/// [`post_payload`] on a Windows host: the record goes straight into the channel's shared memory
/// (crates/cs2-vtm, the `vtmsend.exe` port). Every error means nothing was written, so it is a
/// retry-safe `RuntimeUnavailable`, never a `delivered:false` report. A success is reported in
/// vtmsend's words (`creator version 1`), so `confirmed` reads the same as in a container.
async fn post_native(payload: &str, command: &str) -> Result<Value, ToolError> {
    let text = payload.to_owned();
    let posted = tokio::task::spawn_blocking(move || cs2_vtm::post(&text))
        .await
        .map_err(|error| ToolError::internal(format!("the VToolMessage post task failed: {error}")))?;
    match posted {
        Ok(seq) => Ok(post_report(
            command,
            &format!("cs2-vtm: {CHANNEL_UP_MARKER} 1, record {seq} posted natively"),
            false,
            true,
        )),
        Err(error) => {
            let why = match &error {
                cs2_vtm::Error::ChannelDown(_) => {
                    "the engine creates the channel at toolframework2 Init, and only processes in \
                     the game's logon session can open it: start hammer-mcp from the desktop the \
                     game runs on (not over ssh); check the session with session_status"
                }
                cs2_vtm::Error::Refused(_) => "the channel exists but did not take the record",
            };
            Err(ToolError::new(
                "RuntimeUnavailable",
                "RuntimeUnavailable",
                format!("the VToolMessage channel is not usable on this host: {error} ({why}). Nothing was sent."),
            )
            .with_data(json!({"container": NATIVE_SESSION, "output": error.to_string()}))
            .retry_safe(true))
        }
    }
}

pub(crate) fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(max).collect();
    format!("{head}…")
}

/// The console tail a `run_map` call watches: the file's length at send time and every byte
/// appended since (pt_lib.py `console_size` / `console_since`).
///
/// A **recreated** file is detected by identity, not only by a shrinking length (review-1 L5): a
/// new session (or a `-condebug` restart) can delete console.log and write a *longer* one between
/// two polls, and the old offset would then cut off the head. Both the inode and the length are
/// compared; either mismatch restarts the read at 0 so the caller sees the whole new file.
pub(crate) struct ConsoleWatch {
    path: PathBuf,
    offset: u64,
    seen: String,
    /// Bytes read but not yet decoded: the start of a multibyte character the next poll completes.
    pending: Vec<u8>,
    exists: bool,
    /// The file identity of the last read (`None` while absent). See [`ConsoleWatch::identity`].
    identity: Option<u64>,
    /// The file shrank or was replaced since the watch opened (the read restarted at 0).
    rotated: bool,
}

/// A file's identity: its inode where the host exposes one (a recreated file gets a new one), and
/// 0 elsewhere — the length/shrink check still applies there.
fn file_identity(metadata: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

impl ConsoleWatch {
    pub(crate) fn open(path: PathBuf) -> ConsoleWatch {
        Self::open_at(path, None)
    }

    /// Watch `path` from `offset` (the send-time length per pt_lib.py) instead of from its end.
    /// `None` = the whole file is already written and must not be reported (the send-time offset);
    /// `Some(n)` = a previous call's window, which is what the settle check re-reads (M6).
    pub(crate) fn open_at(path: PathBuf, offset: Option<u64>) -> ConsoleWatch {
        let metadata = std::fs::metadata(&path).ok();
        ConsoleWatch {
            exists: metadata.is_some(),
            identity: metadata.as_ref().map(file_identity),
            path,
            offset: offset.unwrap_or_else(|| metadata.as_ref().map(|m| m.len()).unwrap_or(0)),
            seen: String::new(),
            pending: Vec::new(),
            rotated: false,
        }
    }

    /// What appeared since the last read. A file that shrank **or was replaced** (a new session
    /// recreated console.log) restarts at 0, so the caller sees the whole new file rather than
    /// nothing.
    pub(crate) fn poll(&mut self) {
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            self.exists = false;
            self.identity = None;
            return;
        };
        let identity = file_identity(&metadata);
        if self.identity.is_some_and(|old| old != identity) {
            self.offset = 0;
            self.pending.clear();
            self.rotated = true;
        }
        self.identity = Some(identity);
        self.exists = true;
        if metadata.len() < self.offset {
            self.offset = 0;
            self.pending.clear();
            self.rotated = true;
        }
        if metadata.len() == self.offset {
            return;
        }
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return;
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut bytes = Vec::new();
        if (&mut file).take(metadata.len() - self.offset).read_to_end(&mut bytes).is_err() {
            return;
        }
        self.offset += bytes.len() as u64;
        self.pending.extend_from_slice(&bytes);
        self.decode_pending();
    }

    /// Move the decodable part of `pending` into `seen`. A multibyte character cut by the end of
    /// the read stays pending until the next poll completes it; invalid bytes become U+FFFD.
    fn decode_pending(&mut self) {
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    self.seen.push_str(text);
                    self.pending.clear();
                    return;
                }
                Err(error) => {
                    let good = error.valid_up_to();
                    self.seen
                        .push_str(std::str::from_utf8(&self.pending[..good]).unwrap_or(""));
                    match error.error_len() {
                        // the end of the read cut a character: keep its bytes for the next poll
                        None => {
                            self.pending.drain(..good);
                            return;
                        }
                        Some(bad) => {
                            self.seen.push('\u{FFFD}');
                            self.pending.drain(..good + bad);
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn seen(&self) -> &str {
        &self.seen
    }

    /// Did the file shrink or get replaced while it was watched?
    pub(crate) fn rotated(&self) -> bool {
        self.rotated
    }

    /// The complete lines seen so far (CR/LF stripped, blanks dropped) and whether an unterminated
    /// last line was left out: a partial line is never matched or returned, the next poll
    /// completes it.
    pub(crate) fn complete_lines(&self) -> (Vec<String>, bool) {
        let (whole, partial) = match self.seen.rfind('\n') {
            Some(at) => (&self.seen[..=at], !self.seen[at + 1..].trim().is_empty()),
            None => ("", !self.seen.trim().is_empty()),
        };
        let lines = whole
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect();
        (lines, partial)
    }

    /// Does the file exist (as of the last poll)?
    pub(crate) fn exists(&self) -> bool {
        self.exists
    }

    pub(crate) fn tail(&self, lines: usize) -> Vec<String> {
        let all: Vec<&str> = self
            .seen
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.trim().is_empty())
            .collect();
        all[all.len().saturating_sub(lines)..]
            .iter()
            .map(|line| line.to_string())
            .collect()
    }
}

/// `OnSwitchLoopModeFinished( game : success )` — the engine's own load signal (live
/// `[SignonState] CL:  CNetworkGameClient::OnSwitchLoopModeFinished( game : success )`).
pub(crate) fn success_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"OnSwitchLoopModeFinished\(\s*game\s*:\s*success\s*\)").expect("success regex")
    })
}

/// The failure lines this tool recognises. `invalid map name` is the engine's map-name validator
/// (proven with a plain `map <name>`); the rest are the load-failure wordings, and the
/// loop-mode-switch line for any non-success outcome. Live verification of the exact failure
/// shapes is the orchestrator's — a load that fails in a way not listed here ends as a `timeout`
/// with the console tail, which is why the tail is always returned.
///
/// All of these are anchored to this call's own `Loading map "<name>"` line (review-1 M5) — the
/// one exception is [`name_rejected_regex`], which is what a rejected `map <name>` prints
/// *instead of* a load.
pub(crate) fn failure_regexes() -> &'static [Regex] {
    static RE: OnceLock<Vec<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        [
            r"OnSwitchLoopModeFinished\(\s*game\s*:\s*(?:fail|error|unknown)",
            r"(?i)invalid map name",
            r"(?i)failed to load (?:map|level)",
            r"(?i)unable to load (?:map|level)",
            r"(?i)couldn't find map",
            r"(?i)map load failed",
        ]
        .iter()
        .map(|pattern| Regex::new(pattern).expect("failure regex"))
        .collect()
    })
}

/// The engine's map-name validator, the one failure shape that is recognised **without** the
/// `Loading map` anchor: a plain `map <name>` is rejected before any load starts (the recipe's
/// "plain `map <name>` fails 'invalid map name'"), so this line is its own evidence.
fn name_rejected_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)invalid map name").expect("name-rejected regex"))
}

/// `Loading map "<name>"` for exactly this map (the caller's name is escaped: it is a console
/// argument, so it can carry regex metacharacters even though [`NAME_CHARS`] is narrow).
fn loading_regex(name: &str) -> Regex {
    Regex::new(&format!(r#"Loading map "{}""#, regex::escape(name))).expect("loading regex")
}

/// The engine's own idle level, printed when `disconnect` unloads the map the engine had mounted
/// (`HANDOFF.md` step 6 and playtest 2's `step4_reload.py`, which waits for exactly this line
/// before it posts the next `map`).
pub(crate) fn idle_level_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"Spawn Server: <empty>").expect("idle-level regex"))
}

/// What one console window says about the load of `name` — the single reader the call's own watch
/// and the previous-load settle check share (review-1 M5).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct LoadLines {
    /// `(end of the match, the line)` of this window's `Loading map "<name>"` line.
    loading: Option<(usize, String)>,
    /// The terminal line, if the engine printed one: `Some((true, line))` success, `Some((false,
    /// line))` failure.
    verdict: Option<(bool, String)>,
}

impl LoadLines {
    /// The loading line as the result reports it.
    fn loading_line(&self) -> Option<String> {
        self.loading.as_ref().map(|(_, line)| line.clone())
    }
}

/// Read the engine's lines for `name` out of one console window.
///
/// **The success line only counts after this window's `Loading map "<name>"` line** (review-1 M5):
/// the window is one call's own (it starts at that call's send-time offset), but a previous load
/// that was still in flight can print its `OnSwitchLoopModeFinished( game : success )` inside it —
/// accepting that would report `ok` for a map that was never loaded (or for a `map` post that went
/// in during the previous load). Every failure wording is anchored the same way; the only
/// exception is the map-name validator, which a rejected `map <name>` prints *instead of* a load.
fn read_load_lines(seen: &str, name: &str) -> LoadLines {
    let loading = loading_regex(name)
        .find(seen)
        .map(|m| (m.end(), m.as_str().to_string()));
    if let Some((end, _)) = &loading {
        let after = &seen[*end..];
        if let Some(found) = success_regex().find(after) {
            return LoadLines {
                loading,
                verdict: Some((true, found.as_str().to_string())),
            };
        }
        if let Some(found) = failure_regexes().iter().find_map(|re| re.find(after)) {
            return LoadLines {
                loading,
                verdict: Some((false, found.as_str().to_string())),
            };
        }
    } else if let Some(found) = name_rejected_regex().find(seen) {
        return LoadLines {
            loading: None,
            verdict: Some((false, found.as_str().to_string())),
        };
    }
    LoadLines { loading, verdict: None }
}

/// The verdict `read_load_lines` found, as the result's `state`.
fn state_of(verdict: Option<(bool, String)>) -> &'static str {
    match verdict {
        Some((true, _)) => "ok",
        Some((false, _)) => "failed",
        None => "timeout",
    }
}

fn mtime_ms(time: Option<SystemTime>) -> Value {
    match time.and_then(|t| t.duration_since(UNIX_EPOCH).ok()) {
        Some(d) => json!(d.as_millis() as u64),
        None => Value::Null,
    }
}

// -------------------------------------------------------------------------------------------
// The engine's mounted map (H1) and the previous load (M6)
// -------------------------------------------------------------------------------------------

/// What this server's last `run_map` did, kept in [`HammerState::last_load`]. Two jobs:
///
/// * **the mount memory** (review-1 H1): when the bridge does not expose `game.local` (an older
///   build, or no client scope), a load that ended `ok` is this server's only evidence that the
///   engine has a map mounted and a recompile must be `disconnect`ed before the next `map`;
/// * **the in-flight load** (review-1 M6): a load that never showed a verdict may still be
///   loading, so the next call settles it (or refuses `EngineBusy`) before it posts.
#[derive(Debug, Clone)]
pub struct LastLoad {
    /// The engine name that was loaded.
    pub name: String,
    /// The `.vpk` mtime at send time (the copy the engine mounted).
    pub vpk_mtime_ms: Option<u64>,
    /// The console.log offset this call's window started at (the settle check re-reads from here).
    pub console_offset: u64,
    /// A terminal console line was seen for this load (`ok`/`failed`/`unconfirmed`).
    pub resolved: bool,
    /// The state the call returned.
    pub state: String,
    /// The settle TARGET is the engine's idle level, not a `Loading map "<name>"` line (N3):
    /// the record never posted a map (a failed remount, or a post that failed after its
    /// `disconnect` went out). `settle_previous` watches the idle line for such a record, and
    /// `game.local` saying inGame=false settles it directly.
    pub idle: bool,
    /// The end of the in-flight window (`start + timeoutMs + SETTLE_STALE_MS`): past this an
    /// unresolved record is stale (the engine would have finished one way or another) and a new
    /// call proceeds with a note instead of refusing `EngineBusy` for ever.
    pub in_flight_until: Instant,
}

impl LastLoad {
    fn to_json(&self) -> Value {
        json!({
            "map": self.name,
            "state": self.state,
            "resolved": self.resolved,
            "idle": self.idle,
            "vpkMtimeMs": self.vpk_mtime_ms,
            "consoleOffset": self.console_offset,
        })
    }
}

/// `EngineBusy`: this server's previous load never showed a verdict and the settle window did not
/// produce one either — a console command posted now can land while a level loads, which crashed
/// the engine once (review-1 M6). Retry-safe: nothing was sent.
///
/// For an `idle` record (N3) the wording is the disconnect's idle level, never a map verdict:
/// that record never posted a `map`, so a `Loading map` line will never settle it.
pub(crate) fn engine_busy_previous(previous: &LastLoad, waited_ms: u64) -> ToolError {
    let message = if previous.idle {
        format!(
            "this server's previous run_map posted a `disconnect` whose engine idle level \
             (`Spawn Server: <empty>`) has not appeared yet ({:?}; no `map` was posted after it), \
             so the engine may still be loading that level. A console command \
             posted during a load is the documented crash trigger, so nothing was sent. Wait for \
             the idle line (game/csgo/console.log) or check game.local, then retry.",
            previous.name
        )
    } else {
        format!(
            "this server's previous run_map ({:?}) has not shown a load verdict: the last console \
             lines were watched for {waited_ms} ms and none of `Loading map {:?}` + a success or \
             failure line appeared, so the engine may still be loading it. A console command \
             posted during a load is the documented crash trigger, so nothing was sent. Wait for \
             the load (game/csgo/console.log) or check game.local, then retry.",
            previous.name, previous.name
        )
    };
    ToolError::new("EngineBusy", "EngineBusy", message)
        .with_data(json!({"previousLoad": previous.to_json(), "waitedMs": waited_ms}))
        .retry_safe(true)
}

/// Settle a previous unresolved load before this call posts anything (review-1 M6): re-read the
/// previous call's console window for its verdict. `Ok(Some(verdict))` = it settled (`true`
/// success, `false` failure/idle), `Ok(None)` = the record was stale (past
/// [`LastLoad::in_flight_until`]), `Err(EngineBusy)` = still unresolved.
///
/// For an `idle` record (N3: the previous call never posted a `map`) the warned-for line is the
/// engine's idle level instead of a `Loading map "<name>"` line that will never come.
async fn settle_previous(
    console_path: &Path,
    previous: &LastLoad,
    budget: Duration,
) -> Result<Option<bool>, ToolError> {
    // a record whose in-flight window is already over is stale from the start: no settled
    // answer can change it (the load is not being watched by anything), so no bounded wait
    if Instant::now() >= previous.in_flight_until {
        return Ok(None);
    }
    let mut watch = ConsoleWatch::open_at(console_path.to_path_buf(), Some(previous.console_offset));
    let deadline = Instant::now() + budget;
    loop {
        watch.poll();
        if previous.idle {
            if idle_level_regex().is_match(watch.seen()) {
                return Ok(Some(false));
            }
        } else if let Some((ok, _)) = read_load_lines(watch.seen(), &previous.name).verdict {
            return Ok(Some(ok));
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(CONSOLE_POLL).await;
    }
    if Instant::now() >= previous.in_flight_until {
        // the window is over: the load is not being watched by anything any more
        return Ok(None);
    }
    Err(engine_busy_previous(previous, budget.as_millis() as u64))
}

/// M6, shared by `run_map` and `run_map {unload: true}`: settle THIS server's own unresolved load
/// before anything is posted — a console command issued while a level loads is the documented crash
/// trigger. `game.local` saying a map is up is proof the previous load finished; an `idle` record
/// (N3: no map was ever posted for it) is also settled by `game.local` saying inGame=false.
/// Otherwise the console window is re-read (up to [`SETTLE_TIMEOUT_MS`], or what is left of the
/// tool budget), and a record whose in-flight window is over ([`SETTLE_STALE_MS`]) is stale rather
/// than busy. The notes and the `settledPreviousLoad` value are the caller's; a still-unresolved
/// record is `EngineBusy` (nothing has been sent). A record that DID settle is marked resolved in
/// the store, so no later call repeats the settle on it.
pub(crate) async fn settle_unresolved_previous(
    state: &HammerState,
    console_path: &Path,
    local: Option<&GameLocal>,
    ctx: &CallCtx,
    notes: &mut Vec<String>,
) -> Result<Option<Value>, ToolError> {
    // the *freshest* record: a call that was waiting on the gate must see the load a concurrent
    // call recorded before its posts (M6)
    let previous = state
        .last_load
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let Some(previous) = previous.filter(|p| !p.resolved) else {
        return Ok(None);
    };
    let budget = Duration::from_millis(SETTLE_TIMEOUT_MS).min(ctx.budget.remaining());
    let mounted = local.and_then(GameLocal::mounted);
    let settled = match (mounted, previous.idle) {
        (Some(true), _) => Some(true),
        // an idle record never posted a map: game.local saying nothing is up IS the settle (N3)
        (Some(false), true) => Some(false),
        _ => settle_previous(console_path, &previous, budget).await?,
    };
    match settled {
        Some(true) => notes.push(format!(
            "the previous load of {:?} (this server, state {:?}) had not shown a verdict; \
             `game.local` says a map is up, so it finished and the remount below handles it",
            previous.name, previous.state
        )),
        Some(false) if previous.idle => notes.push(format!(
            "the previous record of {:?} was a remount that never posted a map (idle-level \
             settle target); the engine has now shown its idle level (or game.local says it is \
             not in a map), so nothing is in flight",
            previous.name
        )),
        Some(false) => notes.push(format!(
            "the previous load of {:?} (state {:?}) had not shown a verdict when its call \
             returned; its failure line is in the console now, so nothing was in flight",
            previous.name, previous.state
        )),
        None => notes.push(format!(
            "the previous load of {:?} never showed a verdict; its in-flight window is over \
             ({} s), so this call proceeds — if the engine is in fact still loading it, \
             re-check game/csgo/console.log",
            previous.name,
            SETTLE_STALE_MS / 1000
        )),
    }
    if let Some(verdict) = settled {
        let state_name = if verdict {
            "ok"
        } else if previous.idle {
            "idle"
        } else {
            "failed"
        };
        // the store's own record answers for its settle from now on: no later call repeats it
        if let Some(record) = state
            .last_load
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
        {
            if record.name == previous.name && record.console_offset == previous.console_offset {
                record.resolved = true;
                record.state = state_name.to_string();
            }
        }
    }
    Ok(Some(json!({
        "map": previous.name, "state": previous.state, "settled": settled, "idle": previous.idle,
    })))
}

/// What the engine's `game.local` says (m2.9, client scope): the direct engine-side answer to "is
/// a map up, and which one". Absent on an older bridge or a session without the client scope.
/// `pub(crate)`: `compile_map`'s mounted-map check reads it through the same helper (N-review
/// M9: it must `ensure_connected` before `has`, exactly as run_map learned to).
#[derive(Debug, Clone, Default)]
pub(crate) struct GameLocal {
    in_game: Option<bool>,
    pub(crate) map: Option<String>,
    error: Option<Value>,
}

impl GameLocal {
    pub(crate) fn to_json(&self) -> Value {
        if let Some(error) = &self.error {
            return json!({"inGame": Value::Null, "map": Value::Null, "error": error});
        }
        json!({
            "inGame": self.in_game.map(Value::Bool).unwrap_or(Value::Null),
            "map": self.map.as_ref().map(|m| Value::String(m.clone())).unwrap_or(Value::Null),
        })
    }

    /// Is a map up? `None` when the bridge could not say (not advertised, or the read failed).
    pub(crate) fn mounted(&self) -> Option<bool> {
        match &self.error {
            Some(_) => None,
            None => self.in_game,
        }
    }

    /// The read error, when there was one (`compile_map`'s mounted-note wording).
    pub(crate) fn error(&self) -> Option<&Value> {
        self.error.as_ref()
    }
}

/// Read `game.local` when the bridge exposes it. Never an error: a tools session without a client
/// scope (or a bridge that is `Busy` while the engine parks after a `disconnect`) leaves the
/// caller to fall back on its own memory and to say so.
///
/// The client connects LAZILY, so `has` before the first connect sees an empty capability map —
/// which is not "the bridge does not expose the method" (an unload can be the first bridge call
/// this server ever makes). The connect comes first; its failure is reported like a request
/// failure (`inGame`/`map` unknown plus the error), never as a missing method.
///
/// The live answer `-32024 NoLocalPlayer` means the engine is not in a map at all (what it
/// answers right after a `disconnect`, seen live 2026-10-15): that IS `inGame:false`, not an
/// unknown — it resolves to `Some(false)` with no error so callers can act on it.
pub(crate) async fn read_game_local(state: &HammerState, deadline_ms: u64) -> Option<GameLocal> {
    let mut bridge = state.bridge.lock().await;
    if let Err(error) = bridge.ensure_connected().await {
        return Some(GameLocal {
            error: Some(ToolError::from(error).to_json()),
            ..Default::default()
        });
    }
    if !bridge.has("game.local") {
        return None;
    }
    Some(match bridge
        .request("game.local", json!({}), RequestOpts::deadline(deadline_ms))
        .await
    {
        Ok(reply) => GameLocal {
            in_game: reply.result.get("inGame").and_then(Value::as_bool),
            map: reply
                .result
                .get("map")
                .and_then(Value::as_str)
                .map(str::to_string),
            error: None,
        },
        Err(error) => {
            if error.kind == Kind::NoLocalPlayer || error.code == Some(-32024) {
                return Some(GameLocal {
                    in_game: Some(false),
                    map: None,
                    error: None,
                });
            }
            GameLocal {
                error: Some(error.to_json()),
                ..Default::default()
            }
        }
    })
}

/// The remount decision and what was done about it (review-1 H1).
#[derive(Debug, Clone, Default)]
struct Remount {
    /// The engine had (or may have had) a map mounted, so `disconnect` was posted first.
    needed: bool,
    /// Where the answer came from: `"game.local"`, `"previous-load"` (this server's own last
    /// successful load) or `"unknown"` (neither).
    source: &'static str,
    in_game: Option<bool>,
    map: Option<String>,
    disconnected: bool,
    /// The idle-level line the `disconnect` produced (`Spawn Server: <empty>`).
    signal: Option<String>,
    waited_ms: u64,
    /// The `disconnect` was posted but its signal was never seen: nothing else was sent.
    failed: bool,
    /// Neither console.log nor `game.local` could be watched (no `-condebug`, no client scope):
    /// the `disconnect` was posted without waiting, and the result says so.
    unwatchable: bool,
}

impl Remount {
    fn to_json(&self) -> Value {
        json!({
            "needed": self.needed,
            "source": self.source,
            "inGame": self.in_game.map(Value::Bool).unwrap_or(Value::Null),
            "map": self.map.clone().map(Value::String).unwrap_or(Value::Null),
            "disconnected": self.disconnected,
            "signal": self.signal.clone().map(Value::String).unwrap_or(Value::Null),
            "waitedMs": self.waited_ms,
            "failed": self.failed,
            "unwatchable": self.unwatchable,
        })
    }
}

// -------------------------------------------------------------------------------------------
// The tool
// -------------------------------------------------------------------------------------------

pub async fn run_map(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    if unload_requested(&args)? {
        return unload_map(state, ctx, args).await;
    }
    let explicit = match args.get("path") {
        None | Some(Value::Null) => None,
        // the shape gate `open_map` uses: canonical `maps/…vmap`, no quote/control/':'/'\\',
        // no '.'/'..'/'//' segment, 1..512 chars — this path becomes a console argument
        Some(_) => Some(super::lifecycle::map_content_path(&args)?),
    };
    let mode_lines = mode_command(&args)?;
    let team = super::team::team_arg(&args)?;
    let restart_if_dead = super::team::restart_if_dead_arg(&args)?;
    if args.get("restartIfDead").is_some_and(|v| !v.is_null()) && team.is_none() {
        return Err(ToolError::invalid_arguments("restartIfDead only matters with team \"ct\"|\"t\""));
    }
    let explicit_timeout = timeout_is_explicit(&args);
    let (timeout, budget_clamped) = run_timeout(&args, &ctx.budget)?;
    let deadline = ctx.budget.deadline_ms()?;

    // The active document, through the shared read (it runs the mesh cache's sticky comparison
    // like every other `doc.active` read). A run needs a document only for the *default* target
    // and for the dirty note, so an explicit path survives an unreadable document.
    let mut document_error: Option<Value> = None;
    let doc = match super::live_common::read_active_document(&state, Some(deadline)).await {
        Ok(doc) => doc,
        Err(error) => {
            if explicit.is_none() {
                return Err(ToolError::from(error));
            }
            document_error = Some(error.to_json());
            None
        }
    };

    let content_path = match &explicit {
        Some(path) => path.clone(),
        None => match doc.as_ref() {
            Some(doc) => active_content_path(&state, doc)?,
            None => {
                return Err(ToolError::new(
                    "NoDocument",
                    "NoDocument",
                    "no active Hammer document to take the map from; pass path \
                     (maps/<x>.vmap) or open a map first",
                )
                .with_code(-32003)
                .retry_safe(true))
            }
        },
    };
    refuse_while_compiling(&state, &content_path, &args)?;
    let target = resolve_vpk(&state, &content_path)?;
    if !target.vpk_found {
        return Err(vpk_not_found(&target));
    }
    if let Some(problem) = vpk_header_problem(&target.vpk) {
        return Err(ToolError::new(
            "InvalidVpk",
            "InvalidParams",
            format!(
                "{} is not a loadable VPK ({problem}): the engine FATALs on a corrupt vpk and the \
                 tools session ends, so nothing was sent. Compile the map again (compile_map)",
                target.vpk.display()
            ),
        )
        .with_code(-32602)
        .with_data(json!({"reason": "invalid_vpk", "vpk": target.vpk.display().to_string()}))
        .use_instead("compile_map", "rebuild the .vpk", Some(json!({"path": content_path})))
        .retry_safe(true));
    }

    // A dirty document is NOT a refusal: a load does not touch the document, and Hammer can hold
    // an unsaved document while the engine runs a map (the defuse playtest did exactly that).
    // It is worth saying, because the engine loads the COMPILED .vpk: unsaved edits are not in it.
    let mut notes: Vec<String> = Vec::new();
    match doc.as_ref().and_then(super::lifecycle::dirty_state) {
        Some(true) => notes.push(
            "the document has unsaved changes: the engine loads the COMPILED .vpk, so those edits \
             are NOT in the map that runs (save_map + compile_map first for them to show up)"
                .into(),
        ),
        None if doc.is_some() => notes.push(
            "the document's dirty state is unknown (dirty=null), so this tool cannot say whether \
             the .vpk matches it: it loads the .vpk either way"
                .into(),
        ),
        _ => {}
    }
    if let Some(warning) = state.unsaved_warning() {
        notes.push(format!(
            "{warning}: the engine loads the COMPILED .vpk, which does not contain them"
        ));
    }
    if !target.vmap_found {
        notes.push(format!(
            "the source map {} does not exist on the host, so the .vpk could not be checked \
             against it: the .vpk is loaded as it is",
            target.vmap.display()
        ));
    }
    if target.stale_compile {
        notes.push(format!(
            "staleCompile: {} is OLDER than {} — compiling again (compile_map) is what makes the \
             document's edits run",
            target.vpk.display(),
            target.vmap.display()
        ));
    }
    if target.vpk_form == "flattened" {
        // M8: the fallback was accepted — say exactly why, because a flattened vpk could belong to
        // an unrelated top-level map of the same stem
        notes.push(format!(
            "vpkForm: the content-relative {} does not exist, so the flattened {} was used: it is \
             newer than {} and no content-relative compile of this map exists (the engine is \
             addressed as {:?})",
            target.vpk_candidates[0].display(),
            target.vpk.display(),
            target.vmap.display(),
            target.name
        ));
    }

    // The tools session the console command goes into.
    let container = tools_container(&state).await?;

    // The console offset at send time: everything after it belongs to this call. NOTE (review-2
    // N2): this PRE-gate offset and the mount decision that used to be made from a pre-gate
    // `game.local` read are both potentially STALE — another call of this server may be holding
    // the gate and posting a whole load in the meantime. Everything decision-relevant is
    // therefore re-read INSIDE the gate below (a fresh `game.local`, a fresh offset right
    // before the disconnect); these pre-gate values stay only as the no-remount path's window.
    let console_path = console_log_path(&state);
    let mut watch = ConsoleWatch::open(console_path.clone());
    let mut send_offset = watch.offset;

    let settled_previous: Option<Value>;
    // assigned inside the gate below (deferred init: the value is only known once the load record
    // and the mount decision have been read under it)
    let mut remount: Remount;
    let mut remount_failed = false;
    // the fresh, in-gate answer of `game.local` — the pre-gate read is GONE (N2) and this is
    // what the result's `gameLocalBefore` reports ("before the map command", not "before the
    // gate wait")
    let local: Option<GameLocal>;

    let start = Instant::now();    let mut posts: Vec<Value> = Vec::new();
    let map_command = format!("map {} nomapvalidation=1", target.name);

    // Every write-payload + vtmsend pair, the "is a load in flight?" decision that guards them,
    // the mount decision and this call's load record are serialized by the server-wide gate
    // (review-1 M7 + M6): the payload path is one fixed file, and two calls must not interleave
    // their posts. The gate IS held across the previous-load settle and a remount's idle wait —
    // so a second call waits here rather than being refused at once (review-2 N8; inside the
    // gate this call takes no other lock but the bridge's, which nothing takes before the gate,
    // so this is a bounded wait, and the N2 re-reads below are what makes it correct).
    {
        let _gate = state.vtm_gate.clone().lock_owned().await;
        // the *freshest* record: a call that was waiting on the gate must see the load a
        // concurrent call recorded before its posts (M6)
        let previous = state
            .last_load
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        // N2: a fresh `game.local` read under the gate. The engine's answer before ALL of this
        // call's posts is what the mount decision and the settle below both use.
        local = read_game_local(&state, deadline).await;
        if local.is_none() {
            notes.push(
                "the bridge does not expose game.local, so whether the engine has a map mounted is \
                 this server's own memory (it can miss a map loaded before this server started): a \
                 recompile of a map the engine already mounted may need a `disconnect` — check \
                 game/csgo/console.log"
                    .into(),
            );
        }

        // M6: a load this server started and never saw settle may still be running. `game.local`
        // saying a map is up is proof it finished; otherwise the console window is re-read.
        settled_previous = settle_unresolved_previous(
            &state,
            &console_path,
            local.as_ref(),
            &ctx,
            &mut notes,
        )
        .await?;

        // H1: an engine that already has a map mounted keeps serving the copy it loaded, so a
        // recompile would be invisible. `game.local` is the live answer when the bridge exposes
        // it; this server's own last load is the fallback (`previous resolved by a success line`
        // = the engine is in that map unless something else disconnected it).
        let (mounted, source) = match local.as_ref().and_then(GameLocal::mounted) {
            Some(flag) => (flag, "game.local"),
            None => (
                previous
                    .as_ref()
                    .is_some_and(|p| p.resolved && p.state == "ok"),
                if previous.is_some() {
                    "previous-load"
                } else {
                    "unknown"
                },
            ),
        };
        remount = Remount {
            needed: mounted,
            source,
            in_game: local.as_ref().and_then(|l| l.in_game),
            map: local.as_ref().and_then(|l| l.map.clone()),
            ..Default::default()
        };
        if mounted && source == "previous-load" {
            notes.push(format!(
                "game.local is not available and this server loaded {:?} earlier, so the engine \
                 may still have it mounted: disconnecting before the map command (an engine that \
                 keeps serving its mounted copy would not pick up a recompile)",
                previous.as_ref().map(|p| p.name.as_str()).unwrap_or("")
            ));
        }

        // `note_external_job`: while the engine loads, the frame loop is frozen, so a bridge
        // request from another tool would look like a modal dialog. This is NOT a bridge job
        // (`control.status` lists none for it), so the flag keeps the gate saying `EngineBusy`
        // for the whole window (review-1 L1); it is released below as soon as the load settles.
        state
            .bridge
            .lock()
            .await
            .note_external_job(timeout.min(JOB_GRACE_MAX));

        // whether the `disconnect` post went out (or at least may have): decides what the early
        // exits below do with the store and the grace (N3)
        let mut disconnect_posted = false;
        if mounted {
            // The proven remount (HANDOFF "Playtest" item 1; playtest 2 `step4_reload.py`):
            // `disconnect`, wait for the engine's idle level, then post the map command.
            remount.disconnected = true;
            let disconnect_started = Instant::now();
            // N2: this idle-line match is only ever `Spawn Server: <empty>` AFTER a fresh
            // offset taken right here. The pre-gate offset could already sit past ANOTHER
            // call's `disconnect`, whose idle line is not this one's — matching it would post
            // `map` while the engine still handles that older disconnect (the crash trigger
            // this wait exists to avoid), and the load record would start its window at a
            // line that is not ours.
            watch = ConsoleWatch::open(console_path.clone());
            send_offset = watch.offset;
            match post_command(&state, &container, DISCONNECT_COMMAND).await {
                Ok(report) => {
                    disconnect_posted = true;
                    posts.push(report);
                }
                Err(error) => {
                    // The disconnect was NOT delivered (`OpenMutex failed`, "nothing was
                    // sent", no such container...): the engine still has whatever it had
                    // before, so this call owns nothing — drop the external grace it claimed
                    // and leave the store to the settle verdict above (write-back), never
                    // claiming an in-flight load that was never started (N3).
                    state.bridge.lock().await.clear_external_job();
                    return Err(error);
                }
            }
            if watch.exists {
                let idle_deadline =
                    disconnect_started + disconnect_wait(&ctx, explicit_timeout, &mut notes);
                let mut idle_signal: Option<String> = None;
                loop {
                    watch.poll();
                    if let Some(found) = idle_level_regex().find(watch.seen()) {
                        idle_signal = Some(found.as_str().to_string());
                        break;
                    }
                    if Instant::now() >= idle_deadline {
                        break;
                    }
                    tokio::time::sleep(CONSOLE_POLL).await;
                }
                remount.waited_ms = disconnect_started.elapsed().as_millis() as u64;
                match idle_signal {
                    Some(signal) => remount.signal = Some(signal),
                    None => {
                        // The engine reloads the idle level after a disconnect; posting `map`
                        // while that level loads is the documented crash trigger, so nothing
                        // else is sent (review-1 H1).
                        remount.failed = true;
                        remount_failed = true;
                        notes.push(format!(
                            "the `disconnect` did not reach the engine's idle level within {} ms \
                             (expected `Spawn Server: <empty>` in {}): the map command was NOT \
                             sent, because a console command issued while a level loads is the \
                             documented crash trigger. Check the console tail and retry.",
                            remount.waited_ms,
                            console_path.display()
                        ));
                    }
                }
            } else {
                // No console.log: there is no signal to wait for. Post the map anyway and say the
                // wait could not be watched (the alternative is a tool that never runs a map in a
                // session started without -condebug).
                remount.unwatchable = true;
                remount.waited_ms = disconnect_started.elapsed().as_millis() as u64;
                notes.push(format!(
                    "{} was never seen, so the `disconnect` could not be waited on (the session \
                     must be started with -condebug for this tool to watch the engine): the map \
                     command is sent immediately, and if the engine's idle level is still loading \
                     it may be dropped (the documented crash trigger)",
                    console_path.display()
                ));
            }
        }

        // The posts. The failing-post handling is the N3 fix: a `post_command` Err is the
        // definite "nothing was sent" case, and a load record written for a load that never
        // started would refuse `run_map` AND `run_map {unload:true}` until the in-flight window
        // (timeout + 60 s) ran out.
        if !remount_failed {
            let mut post_error: Option<ToolError> = None;
            for line in &mode_lines {
                match post_command(&state, &container, line).await {
                    Ok(report) => posts.push(report),
                    Err(error) => {
                        post_error = Some(error);
                        break;
                    }
                }
            }
            if post_error.is_none() {
                match post_command(&state, &container, &map_command).await {
                    Ok(report) => posts.push(report),
                    Err(error) => post_error = Some(error),
                }
            }
            if let Some(error) = post_error {
                if disconnect_posted {
                    // The `disconnect` DID go out, so the engine may now be loading its idle
                    // level and nothing was posted on top: this call's honest record is that
                    // idle level, NOT an in-flight `map` of this call (N3's failed-remount/
                    // never-posted rule). Unresolved with a short window, so the next call
                    // settles it at `game.local` inGame=false or the idle line.
                    *state
                        .last_load
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(LastLoad {
                        name: target.name.clone(),
                        vpk_mtime_ms: target
                            .vpk_mtime()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as u64),
                        console_offset: send_offset,
                        resolved: false,
                        state: "failed".into(),
                        idle: true,
                        in_flight_until: Instant::now()
                            + Duration::from_millis(SETTLE_STALE_MS),
                    });
                    // the grace stays claimed: an engine-level load may truly be under way
                } else {
                    // Nothing of this call reached the engine: there is no in-flight load to
                    // excuse, so the record is CLEARED (never an unresolved `map` record for a
                    // load that was never posted) and the external grace is released.
                    *state
                        .last_load
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                    state.bridge.lock().await.clear_external_job();
                }
                return Err(error);
            }
            // This server's own load record — written only NOW that the `map` post has gone out
            // (delivered or unknown; N3): the gate is still held, so the next call of this
            // server sees it either way (a concurrent call blocks on the gate first, M6).
            *state
                .last_load
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(LastLoad {
                name: target.name.clone(),
                vpk_mtime_ms: target
                    .vpk_mtime()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64),
                console_offset: send_offset,
                resolved: false,
                state: "unknown".into(),
                idle: false,
                in_flight_until: start + timeout + Duration::from_millis(SETTLE_STALE_MS),
            });
        } else {
            // remount_failed: the `disconnect` went out and its idle level never showed — the
            // engine may be loading `<empty>` with NO map command after it (a load of its
            // own): record exactly that (the idle level is the settle TARGET, N3), with a
            // window that starts at the call's own `map` deadline.
            *state
                .last_load
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(LastLoad {
                name: target.name.clone(),
                vpk_mtime_ms: target
                    .vpk_mtime()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64),
                console_offset: send_offset,
                resolved: false,
                state: "failed".into(),
                idle: true,
                in_flight_until: start + timeout + Duration::from_millis(SETTLE_STALE_MS),
            });
        }
    }

    // `vtmsend` prints the channel marker once toolframework2 created the VToolMessage mutex; a
    // post without it is not refused (the engine's console line is the signal), but it is worth
    // saying when the answer below is a timeout.
    let mut post_unknown = false;
    {
        let unconfirmed: Vec<&Value> = posts
            .iter()
            .filter(|post| post["confirmed"] != json!(true))
            .collect();
        if !unconfirmed.is_empty() {
            notes.push(format!(
                "{} of {} VToolMessage post(s) did not report the channel marker ({:?}): the \
                 engine's console line is still the verdict — read vtmsend[] if this ends as a \
                 timeout",
                unconfirmed.len(),
                posts.len(),
                unconfirmed[0]["output"]
            ));
        }
        let undelivered: Vec<&Value> = posts
            .iter()
            .filter(|post| post["delivered"] == json!(false))
            .collect();
        if !undelivered.is_empty() {
            // M6: the exec was cut off, so `vtmsend.exe` may or may not have delivered. Never
            // repeat such a post (a second `map` into a loading engine is the crash trigger); the
            // verdict below decides, and an undecided one is `state:"unknown"`.
            post_unknown = true;
            notes.push(format!(
                "{} of {} VToolMessage post(s) did not come back ({:?}): the command may or may \
                 not have reached the engine, so it is NOT repeated automatically. The engine's \
                 console line is the verdict; if there is none, check game/csgo/console.log (or \
                 game.local) before re-running",
                undelivered.len(),
                posts.len(),
                undelivered[0]["output"]
            ));
        }
    }
    if !watch.exists {
        notes.push(format!(
            "{} does not exist: the session was not started with -condebug, so this tool cannot \
             see the engine's load signal. The command was posted anyway — check game.local (when \
             the bridge exposes it) or the engine window for the result",
            console_path.display()
        ));
    }

    // The budget is measured from the call's start (the document read, the container lookup and
    // the posts are part of it): clamp the wait to what is actually left, so this call cannot run
    // past the budget the caller gave it. Only an EXPLICIT request is flagged when the clamp bites
    // (the default is chosen to fit the budget, so a cut default is not the caller's problem).
    let remaining = ctx.budget.remaining();
    let (timeout, clamped_again) = if remaining < timeout {
        (remaining, true)
    } else {
        (timeout, false)
    };
    let budget_clamped = budget_clamped || (clamped_again && explicit_timeout);
    let watch_deadline = start + timeout;

    let mut loading_line: Option<String> = None;
    let mut success_line: Option<String> = None;
    let mut fail_line: Option<String> = None;
    let mut state_name = if remount_failed { "failed" } else { "timeout" };
    if !remount_failed {
        loop {
            watch.poll();
            let lines = read_load_lines(watch.seen(), &target.name);
            if loading_line.is_none() {
                loading_line = lines.loading_line();
            }
            if let Some((ok, line)) = lines.verdict {
                state_name = state_of(Some((ok, line.clone())));
                if ok {
                    success_line = Some(line);
                } else {
                    fail_line = Some(line);
                }
                break;
            }
            if Instant::now() >= watch_deadline {
                break;
            }
            tokio::time::sleep(CONSOLE_POLL).await;
        }
        // one last look: the engine may have written the signal while the last poll slept
        if state_name == "timeout" {
            watch.poll();
            let lines = read_load_lines(watch.seen(), &target.name);
            if loading_line.is_none() {
                loading_line = lines.loading_line();
            }
            if let Some((ok, line)) = lines.verdict {
                state_name = state_of(Some((ok, line.clone())));
                if ok {
                    success_line = Some(line);
                } else {
                    fail_line = Some(line);
                }
            } else if post_unknown {
                // M6: the post may never have reached the engine, so "no signal" is not a
                // timeout — it is an unknown outcome, and the note must not send the caller into
                // a blind retry.
                state_name = "unknown";
            }
        }
    }

    // `game.local`, with what is LEFT of the tool budget (review-1 L6: the watch has already
    // spent most of it): the direct engine-side answer to "is the map up, and which one".
    // Optional by design — a tools session without a client scope must not turn a load into an
    // error.
    let game_local = match ctx.budget.deadline_ms() {
        Ok(left) => read_game_local(&state, left).await,
        Err(_) => None,
    };
    if game_local.is_none() && local.is_some() {
        notes.push(
            "the final game.local read did not answer (a bridge that is Busy while the engine \
             parks after a disconnect, or a tool budget that ran out): the engine's earlier answer \
             is in gameLocalBefore, and the state above comes from the console"
                .into(),
        );
    }
    // M5: the console's success is cross-checked against the engine's own answer when it has one.
    // The console evidence is left as it was seen (`console.successLine` still says what the
    // engine printed) — the state and the note carry the mismatch.
    if state_name == "ok" {
        if let Some(true) = game_local.as_ref().and_then(GameLocal::mounted) {
            let reported = game_local.as_ref().and_then(|l| l.map.clone());
            if reported.as_deref().is_some_and(|map| map != target.name) {
                state_name = "unconfirmed";
                notes.push(format!(
                    "the console printed a load success for {:?} but game.local reports {:?}: the \
                     success line may belong to another load (a previous call's load that was \
                     still finishing, for example), so this is not reported as ok",
                    target.name,
                    reported.unwrap_or_default()
                ));
            }
        }
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;
    let ok = state_name == "ok";

    if state_name == "timeout" {
        notes.push(format!(
            "no load signal within {elapsed_ms} ms: the engine may still be loading — re-run \
             run_map to send the command again (this call's own load is now this server's \
             unresolved load, so the next call settles it first), or check \
             game/csgo/console.log — the tail below is what this call saw{}",
            if budget_clamped { ", cut short by the tool budget" } else { "" }
        ));
    }
    if state_name == "unknown" {
        notes.push(
            "a VToolMessage post did not come back, so whether the engine got the command is \
             unknown: do NOT re-run blindly — check game.local (when exposed) or \
             game/csgo/console.log, and remember a second `map` posted into a loading engine is \
             the documented crash trigger"
                .into(),
        );
    }
    if state_name == "failed" && !remount.failed {
        notes.push(
            "the engine reported a load failure: read failLine and the console tail (a map name \
             the engine cannot resolve is the usual cause; the compiled .vpk must be under the \
             addon's game maps directory)"
                .into(),
        );
    }

    // The load's own record: a terminal line means the engine settled it; a timeout, an unknown
    // post or a remount that never reached the idle level keeps the record unresolved, so the next
    // call settles it (or refuses `EngineBusy`) instead of posting into a level that may still be
    // loading. A failed REMOUNT is an idle-level record (N3): no `map` went out after the
    // `disconnect`, so the settle target is the idle line (or `game.local` inGame=false), never a
    // `Loading map "<name>"` line that will never come.
    let resolved = !remount_failed && matches!(state_name, "ok" | "failed" | "unconfirmed");
    let vpk_mtime_ms = target
        .vpk_mtime()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    *state
        .last_load
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(LastLoad {
        name: target.name.clone(),
        vpk_mtime_ms,
        console_offset: send_offset,
        resolved,
        state: state_name.to_string(),
        idle: remount_failed,
        in_flight_until: start + timeout + Duration::from_millis(SETTLE_STALE_MS),
    });
    if resolved {
        // the engine is not loading this any more: stop excusing a quiet main thread (L1)
        state.bridge.lock().await.clear_external_job();
    } else {
        // the load is unresolved: it may still be in flight for the rest of the record's window,
        // so the blocked gate keeps saying `EngineBusy` for exactly that long (L1) — a concurrent
        // tool must not be told "a modal dialog is up" while the engine is loading.
        state
            .bridge
            .lock()
            .await
            .note_external_job(Duration::from_millis(SETTLE_STALE_MS).min(JOB_GRACE_MAX));
    }

    let mut out = Map::new();
    out.insert("ok".into(), json!(ok));
    out.insert("state".into(), json!(state_name));
    out.insert("map".into(), json!(target.name));
    out.insert("contentPath".into(), json!(target.content_path));
    out.insert("command".into(), json!(format!("map {} nomapvalidation=1", target.name)));
    out.insert("modeCommands".into(), json!(mode_lines));
    out.insert("container".into(), json!(container));
    out.insert("elapsedMs".into(), json!(elapsed_ms));
    out.insert("timeoutMs".into(), json!(timeout.as_millis() as u64));
    out.insert("budgetClamped".into(), json!(budget_clamped));
    out.insert(
        "console".into(),
        json!({
            "path": console_path.display().to_string(),
            "exists": watch.exists,
            "loadingLine": loading_line,
            "successLine": success_line,
            "failLine": fail_line,
            "tail": watch.tail(CONSOLE_TAIL_LINES),
        }),
    );
    out.insert(
        "vpk".into(),
        json!({
            "hostPath": target.vpk.display().to_string(),
            "relative": format!("csgo_addons/{}/{}", state.addon(), target.vpk_relative),
            "found": target.vpk_found,
            "form": target.vpk_form,
            "bytes": std::fs::metadata(&target.vpk).map(|m| m.len()).ok(),
            "mtimeMs": mtime_ms(target.vpk_mtime()),
        }),
    );
    out.insert(
        "source".into(),
        json!({
            "hostPath": target.vmap.display().to_string(),
            "found": target.vmap_found,
            "mtimeMs": mtime_ms(target.vmap_mtime()),
            "staleCompile": target.stale_compile,
        }),
    );
    out.insert("remount".into(), remount.to_json());
    if let Some(settled) = settled_previous {
        out.insert("settledPreviousLoad".into(), settled);
    }
    out.insert("vtmsend".into(), Value::Array(posts));
    out.insert(
        "document".into(),
        super::session::doc_view(&state, doc.as_ref()),
    );
    if let Some(error) = document_error {
        out.insert("documentError".into(), error);
    }
    if let Some(local) = local {
        out.insert("gameLocalBefore".into(), local.to_json());
    }
    if let Some(local) = game_local {
        out.insert("gameLocal".into(), local.to_json());
    }
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    // the local player starts on team 0 and dead: with `team` the call joins it and waits for the
    // spawn (the load is already confirmed; the join reports itself and never fails the call)
    if let (true, Some(team)) = (ok, team) {
        out.insert("team".into(), super::team::join(&state, &ctx, team, restart_if_dead).await);
    } else if let Some(team) = team {
        out.insert(
            "team".into(),
            json!({
                "requested": team.name(), "joined": false, "alive": false,
                "skipped": "load not confirmed",
                "note": "the join runs only after a confirmed load; once the map is up, console_exec \"jointeam 3 1\" (CT) / \"jointeam 2 1\" (T) or run_map again",
            }),
        );
    } else if ok {
        notes.push(
            "the local player starts on team 0 and dead: pass team \"ct\"|\"t\" to run_map, or \
             console_exec \"jointeam 3 1\" (CT) / \"jointeam 2 1\" (T); game_state's hint says it too"
                .into(),
        );
    }
    out.insert("notes".into(), json!(notes));
    let mut output: ToolOutput = Value::Object(out).into();
    if ok {
        output = output
            .with_next("game_state", "confirm the engine has the map up and read the local player")
            .with_next(
                "render_world",
                "render the loaded map from a camera you give (origin+angles, or lookAt to orbit)",
            );
    }
    Ok(output)
}

/// `run_map {unload: true}`: unload the map the engine has mounted, and stop there. It is the
/// primitive the `compile_map` map-in-game refusal points at (and the way a recompiled `.vpk` is
/// picked up without loading a different map): ONE console command (`disconnect`, through the same
/// [`state.vtm_gate`] and the same VToolMessage route the load uses), then the engine's own
/// idle-level line (`Spawn Server: <empty>`) is waited on.
///
/// `state` (`ok|failed|timeout`, never an error once something was posted — the caller asked for
/// exactly this console command):
///
/// * `ok` — the idle level was seen: the engine has nothing mounted, and this server's own load
///   memory (`last_load`) is cleared so the next `run_map` does not disconnect for a map that is
///   not up;
/// * `failed` — the post did not come back (`delivered:false`): whether the engine got the
///   `disconnect` is unknown, and it is never repeated (M6);
/// * `timeout` — posted, no idle-level line within the wait (including a session whose console.log
///   cannot be watched, i.e. started without `-condebug`);
/// * a **no-op** (`state:"ok"`, `noop:true`, `disconnected:false`) with a note when `game.local`
///   says the engine is not in a map: there is nothing to unload and nothing is sent. The
///   decision is made from a FRESH `game.local` read under the gate (review-2 N2), and it runs
///   the previous-record settle FIRST: an unresolved record is never dropped silently (its
///   window may still be live and `game.local` can answer inGame=false between load stages, N9);
///   a no-op that settled nothing keeps the record.
///
/// Everything decision-relevant (the fresh `game.local`, the settle, the no-op and the fresh
/// console offset before the `disconnect`) happens under the same `vtm_gate` the post needs,
/// so a concurrent call's answers can no longer be stale by the time they are used (N2).
async fn unload_map(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    // this route sends ONLY `disconnect`: a path or a mode line would silently mean something else
    for key in ["path", "gameType", "gameMode", "team", "restartIfDead"] {
        if args.get(key).is_some_and(|value| !value.is_null()) {
            return Err(ToolError::invalid_arguments(format!(
                "unload:true unloads the map the engine has mounted and sends only `disconnect`: \
                 {key} is not allowed with it"
            )));
        }
    }
    let explicit_timeout = timeout_is_explicit(&args);
    let (timeout, budget_clamped) = run_timeout(&args, &ctx.budget)?;
    let deadline = ctx.budget.deadline_ms()?;
    let container = tools_container(&state).await?;
    let console_path = console_log_path(&state);
    let mut watch = ConsoleWatch::open(console_path.clone());
    // the gate-fresh `game.local` answer (N2: the pre-gate read is gone — another call of this
    // server may be holding the gate and its answer would be stale by the time it is used)
    let mut local: Option<GameLocal>;
    let mut notes: Vec<String> = Vec::new();

    let start = Instant::now();
    let mut posts: Vec<Value> = Vec::new();
    let settled_previous: Option<Value>;
    let mut signal: Option<String> = None;
    let mut waited_ms = 0u64;
    let mut disconnected = false;
    let noop: bool;
    let state_name: &str;
    // N2 + N9: EVERYTHING decision-relevant (the fresh `game.local`, the previous-record settle,
    // the no-op decision and the fresh console offset before the `disconnect`) happens under the
    // same vtm_gate the post itself needs — the pre-gate shapes of these answers were stale the
    // moment a concurrent call got in between them (review-2 N2). A second caller waits here
    // rather than being refused at once (N8: a bounded wait, and the re-reads below are what
    // make it correct).
    {
        let _gate = state.vtm_gate.clone().lock_owned().await;
        local = read_game_local(&state, deadline).await;
        if local.is_none() {
            notes.push(
                "the bridge does not expose game.local, so whether the engine has a map mounted is \
                 this server's own memory: the `disconnect` is posted regardless (it is a no-op at \
                 the idle level) and its idle-level line is the answer"
                    .into(),
            );
        }
        // N9: the no-op must not silently drop an unresolved record — while a load of this
        // server is in its non-frozen phases, `game.local` can briefly answer inGame=false. The
        // settle runs first (it may return `EngineBusy` for a still-live load), and the record
        // is cleared below only when it is resolved.
        settled_previous =
            settle_unresolved_previous(&state, &console_path, local.as_ref(), &ctx, &mut notes)
                .await?;
        // game.local saying the engine is NOT in a map is the no-op: nothing is sent, and this
        // server's own memory must not keep claiming a mount either (H1's fallback would otherwise
        // disconnect for a map that is not up)
        noop = local.as_ref().and_then(GameLocal::mounted) == Some(false);
        if noop {
            let still_unresolved = state
                .last_load
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .is_some_and(|record| !record.resolved);
            if !still_unresolved {
                // the record settled above (or was resolved from the start): the mount memory
                // can go (N9 keeps it when it did NOT settle)
                *state
                    .last_load
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            }
            state_name = "ok";
            notes.push(format!(
                "game.local says the engine is not in a map (inGame=false): there is nothing to \
                 unload and nothing was sent{}",
                if still_unresolved {
                    " (this server's own load memory did NOT settle, so it was kept: its window \
                     is still being watched by nothing, but the next call settles it first)"
                } else if !settled_previous.is_none() {
                    " (this server's own load memory settled above and was cleared too)"
                } else {
                    ""
                }
            ));
        } else {
            state
                .bridge
                .lock()
                .await
                .note_external_job(timeout.min(JOB_GRACE_MAX));
            disconnected = true;
            let posted_at = Instant::now();
            // N2: this idle-line match counts only `Spawn Server: <empty>` AFTER a fresh offset
            // taken right here — the pre-gate offset could sit past ANOTHER call's disconnect,
            // whose idle line is not this one's (its `disconnect` may still be in the engine).
            watch = ConsoleWatch::open(console_path.clone());
            match post_command(&state, &container, DISCONNECT_COMMAND).await {
                Ok(report) => posts.push(report),
                Err(error) => {
                    // a definite non-delivery (`OpenMutex failed`, no container...): nothing
                    // changed in the engine, so this call owns nothing — release the claimed
                    // external grace and report; the store is untouched
                    state.bridge.lock().await.clear_external_job();
                    return Err(error);
                }
            }
            if watch.exists {
                let idle_deadline = posted_at
                    + disconnect_wait(&ctx, explicit_timeout, &mut notes).min(timeout);
                loop {
                    watch.poll();
                    if let Some(found) = idle_level_regex().find(watch.seen()) {
                        signal = Some(found.as_str().to_string());
                        break;
                    }
                    if Instant::now() >= idle_deadline {
                        break;
                    }
                    tokio::time::sleep(CONSOLE_POLL).await;
                }
            } else {
                notes.push(format!(
                    "{} does not exist: the session was not started with -condebug, so the \
                     `disconnect` could not be watched — the post is the whole answer",
                    console_path.display()
                ));
            }
            waited_ms = posted_at.elapsed().as_millis() as u64;
            let delivered = posts.iter().all(|post| post["delivered"] != json!(false));
            if signal.is_some() {
                state_name = "ok";
            } else if !delivered {
                // M6: the exec was cut off, so whether the engine was told is unknown. Never repeated.
                state_name = "failed";
                notes.push(
                    "the `disconnect` post did not come back (`delivered:false`): whether the engine \
                     got it is unknown, and it is NOT repeated automatically — check \
                     game/csgo/console.log (or game.local, when the bridge exposes it) before \
                     retrying"
                        .into(),
                );
            } else {
                state_name = "timeout";
                notes.push(format!(
                    "no idle-level line within {waited_ms} ms: the map may still be mounted (the \
                     engine is expected to print `Spawn Server: <empty>` when `disconnect` unloads \
                     it; an engine that was ALREADY at the idle level may simply not print it again — \
                     unverified live) — check game/csgo/console.log, then run_map again or reconnect \
                     the session{}",
                    if budget_clamped {
                        ", cut short by the tool budget"
                    } else {
                        ""
                    }
                ));
            }
        }
    }

    if state_name == "ok" && !noop {
        // the engine is at the idle level: this server holds no map any more
        *state
            .last_load
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    // `game.local`, with what is LEFT of the budget (`run_map`'s L6 rule): the engine's own
    // answer to "is anything still up".
    let game_local = match ctx.budget.deadline_ms() {
        Ok(left) => read_game_local(&state, left).await,
        Err(_) => None,
    };
    if game_local.is_none() && local.is_some() {
        notes.push(
            "the final game.local read did not answer (a bridge that is Busy while the engine \
             parks after a disconnect, or a tool budget that ran out): the engine's earlier answer \
             is in gameLocalBefore"
                .into(),
        );
    }
    if !noop {
        // the engine parks at the idle level (a level load of its own): keep the window excused
        // while it settles, exactly as an unresolved load does (L1). The no-op claimed nothing.
        if state_name == "ok" {
            state.bridge.lock().await.clear_external_job();
        } else {
            state
                .bridge
                .lock()
                .await
                .note_external_job(Duration::from_millis(SETTLE_STALE_MS).min(JOB_GRACE_MAX));
        }
    }
    // a later call reads game.local first; this server's memory still has to be honest for the
    // no-game.local case, so an unload that could not be confirmed clears nothing
    if state_name != "ok" {
        let remembered = state
            .last_load
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        notes.push(format!(
            "the unload was not confirmed, so this server's own load memory was kept{}: a later \
             run_map still disconnects before its map command",
            match remembered {
                Some(load) => format!(" ({:?}, state {:?})", load.name, load.state),
                None => " (it held no load either)".to_string(),
            }
        ));
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;
    let mut out = Map::new();
    out.insert("ok".into(), json!(state_name == "ok"));
    out.insert("action".into(), json!("unload"));
    out.insert("state".into(), json!(state_name));
    out.insert("noop".into(), json!(noop));
    out.insert("disconnected".into(), json!(disconnected));
    out.insert("container".into(), json!(container));
    out.insert("command".into(), json!(DISCONNECT_COMMAND));
    out.insert("elapsedMs".into(), json!(elapsed_ms));
    out.insert("timeoutMs".into(), json!(timeout.as_millis() as u64));
    out.insert("budgetClamped".into(), json!(budget_clamped));
    out.insert(
        "mapBefore".into(),
        local
            .as_ref()
            .and_then(|local| local.map.clone())
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    out.insert(
        "signal".into(),
        signal.clone().map(Value::String).unwrap_or(Value::Null),
    );
    out.insert("waitedMs".into(), json!(waited_ms));
    out.insert(
        "console".into(),
        json!({
            "path": console_path.display().to_string(),
            "exists": watch.exists,
            "tail": watch.tail(CONSOLE_TAIL_LINES),
        }),
    );
    if let Some(settled) = settled_previous {
        out.insert("settledPreviousLoad".into(), settled);
    }
    out.insert("vtmsend".into(), Value::Array(posts));
    if let Some(local) = local.take() {
        out.insert("gameLocalBefore".into(), local.to_json());
    }
    if let Some(local) = game_local {
        out.insert("gameLocal".into(), local.to_json());
    }
    out.insert("notes".into(), json!(notes));
    Ok(Value::Object(out).into())
}

/// How long the `disconnect` of a remount may take: the recipe's [`REMOUNT_TIMEOUT_MS`], clamped
/// to the tool budget left. The recipe's 120 s is longer than a default call's budget, so the cut
/// is only worth a note for a caller who **asked** for the wait (`explicit`, a `timeoutMs` of its
/// own): a default call is told the effective wait in `remount.waitedMs` anyway and must not carry
/// a note about the tool's own default.
fn disconnect_wait(ctx: &CallCtx, explicit: bool, notes: &mut Vec<String>) -> Duration {
    let requested = Duration::from_millis(REMOUNT_TIMEOUT_MS);
    let remaining = ctx.budget.remaining();
    if explicit && remaining < requested {
        notes.push(format!(
            "the disconnect wait was cut to {} ms by the tool budget (the recipe waits {} s)",
            remaining.as_millis(),
            REMOUNT_TIMEOUT_MS / 1000
        ));
    }
    remaining.min(requested)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cs2_mcp_common::tools::Budget;

    fn budget(ms: u64) -> Budget {
        Budget::new(ms, 30_000)
    }

    /// `timeoutMs`: the default, the maximum, the clamp to the tool budget (a zero remainder is
    /// the budget error, not a "forever" wait), and the caller errors. The DEFAULT is chosen to fit
    /// the budget silently; only an explicit request the budget cut is flagged (`budgetClamped`).
    #[test]
    fn timeout_defaults_maxes_and_clamps_to_the_budget() {
        // the default 60 s fits a 90 s budget: not clamped, and the default is reported as is
        assert_eq!(
            run_timeout(&json!({}), &budget(90_000)).unwrap(),
            (Duration::from_millis(RUN_TIMEOUT_MS), false)
        );
        assert_eq!(
            run_timeout(&json!({"timeoutMs": 5_000}), &budget(90_000)).unwrap(),
            (Duration::from_secs(5), false)
        );
        assert_eq!(
            run_timeout(&json!({"timeoutMs": 5_000_000}), &budget(RUN_TIMEOUT_MAX_MS * 2)).unwrap(),
            (Duration::from_millis(RUN_TIMEOUT_MAX_MS), false),
            "the maximum, not the caller's larger number"
        );
        assert_eq!(
            run_timeout(&json!({"timeoutMs": null}), &budget(90_000)).unwrap(),
            (Duration::from_millis(RUN_TIMEOUT_MS), false)
        );
        // a budget smaller than the DEFAULT: the default fits it and says NOTHING about a clamp
        // (the effective value is what `timeoutMs` in the result reports)
        let (defaulted, silent) = run_timeout(&json!({}), &budget(2_000)).unwrap();
        assert!(!silent, "the default is not the caller's request");
        assert!(
            defaulted <= Duration::from_millis(2_000) && defaulted > Duration::from_millis(1_000),
            "{defaulted:?}"
        );
        // an EXPLICIT wait the budget cannot honour is cut and flagged (`budgetClamped` + note)
        let (cut, clamped) = run_timeout(&json!({"timeoutMs": 30_000}), &budget(2_000)).unwrap();
        assert!(clamped, "an explicit request that got cut is said so");
        assert!(cut <= Duration::from_millis(2_000), "{cut:?}");
        // ... while an explicit wait the budget can honour is not
        assert_eq!(
            run_timeout(&json!({"timeoutMs": 900}), &budget(2_000)).unwrap(),
            (Duration::from_millis(900), false)
        );
        assert!(timeout_is_explicit(&json!({"timeoutMs": 1})));
        assert!(!timeout_is_explicit(&json!({})));
        assert!(!timeout_is_explicit(&json!({"timeoutMs": null})));
        for bad in [json!({"timeoutMs": 0}), json!({"timeoutMs": "5s"}), json!({"timeoutMs": -1})] {
            assert!(run_timeout(&bad, &budget(90_000)).is_err(), "{bad}");
        }
        let exhausted = run_timeout(&json!({}), &budget(0)).unwrap_err();
        assert_eq!(exhausted.name, "ToolBudgetExceeded");
    }

    /// **L2/C**: the tool's own default must not carry a note about the tool budget, while an
    /// explicit request that the budget cut must (`budgetClamped` and the note are the caller's
    /// signal that the watch was shortened — a default call does not need it). The call path's flag
    /// is the first clamp's (only true for an explicit request, `run_timeout`) OR the mid-call
    /// re-clamp's, which counts only for an explicit request too.
    #[test]
    fn only_an_explicit_timeout_is_reported_when_the_budget_cuts_it() {
        let flag = |args: &Value, mid_call_clamp: bool| {
            let (_, first) = run_timeout(args, &budget(2_000)).unwrap();
            first || (mid_call_clamp && timeout_is_explicit(args))
        };
        assert!(
            !flag(&json!({}), true),
            "the default is cut to fit the budget: silent"
        );
        assert!(
            flag(&json!({"timeoutMs": 600_000}), true),
            "an explicit wait the budget cannot honour is flagged"
        );
        assert!(!flag(&json!({}), false));
        assert!(
            flag(&json!({"timeoutMs": 3_000}), false),
            "the explicit wait is already cut by the first clamp (the budget is 2 s)"
        );
    }

    /// `gameType`/`gameMode` -> the mode lines, in a fixed order, **one post each** before the map
    /// command (the proven recipe posts `game_type 0` and `game_mode 1` separately; review-1 L4:
    /// whether the `command` channel splits on `;` is unverified).
    #[test]
    fn mode_commands_are_validated_and_ordered() {
        assert_eq!(mode_command(&json!({})).unwrap(), Vec::<String>::new());
        assert_eq!(
            mode_command(&json!({"gameMode": 1})).unwrap(),
            vec!["game_mode 1".to_string()]
        );
        assert_eq!(
            mode_command(&json!({"gameMode": 1, "gameType": 0})).unwrap(),
            vec!["game_type 0".to_string(), "game_mode 1".to_string()]
        );
        for bad in [
            json!({"gameType": -1}),
            json!({"gameType": 256}),
            json!({"gameType": 1.5}),
            json!({"gameMode": "1"}),
            json!({"gameType": true}),
        ] {
            assert!(mode_command(&bad).is_err(), "{bad}");
        }
    }

    /// The map name is the console argument: `[A-Za-z0-9_/-]`, no empty segment, no `..` — a
    /// space, a `"` or a `;` can never reach a payload.
    #[test]
    fn the_map_name_is_strictly_validated() {
        assert_eq!(map_name_of("maps/probe.vmap").unwrap(), "probe");
        assert_eq!(map_name_of("maps/sub/probe.vmap").unwrap(), "sub/probe");
        for bad in [
            "maps/x y.vmap",
            "maps/x;quit.vmap",
            "maps/x\"y.vmap",
            "maps/../x.vmap",
            "maps//x.vmap",
            "maps/x\ny.vmap",
            "maps/ünïcode.vmap",
            "maps/.vmap",
        ] {
            let error = map_name_of(bad).unwrap_err();
            assert_eq!(error.type_, "InvalidArguments", "{bad}");
            assert_eq!(error.name, "InvalidParams", "{bad}");
            assert!(error.message.contains("allowed"), "{bad}: {error:?}");
        }
        // the length bound is on the *name*, not the path
        assert!(map_name_of(&format!("maps/{}.vmap", "a".repeat(NAME_MAX))).is_ok());
        assert!(map_name_of(&format!("maps/{}.vmap", "a".repeat(NAME_MAX + 1))).is_err());
    }

    /// Real-shaped console lines (CRLF, the timestamp/channel prefix) are matched. The success
    /// line is the one live evidence has (`HANDOFF.md`, four playtests).
    #[test]
    fn the_engine_lines_are_recognised() {
        let log = "10/13 20:24:00 Loading map \"defuse_poc\"\r\n\
                   10/13 20:24:08 [Server] SV:  Spawn Server: defuse_poc\r\n\
                   10/13 20:24:09 [SignonState] CL:  CNetworkGameClient::OnSwitchLoopModeFinished( game : success )\r\n";
        assert!(loading_regex("defuse_poc").is_match(log));
        assert!(!loading_regex("other").is_match(log), "the name is part of the signal");
        let found = success_regex().find(log).expect("success line");
        assert!(found.as_str().starts_with("OnSwitchLoopModeFinished( game : success"));
        let lines = read_load_lines(log, "defuse_poc");
        assert!(lines.loading_line().unwrap().contains("defuse_poc"));
        assert_eq!(
            lines.verdict.map(|(ok, _)| ok),
            Some(true),
            "the anchored reader sees the success"
        );
        for line in [
            "11/02 10:00:00 Failed to load map\r\n",
            "11/02 10:00:00 map: invalid map name\r\n",
            "11/02 10:00:00 Unable to load level\r\n",
            "11/02 10:00:00 Couldn't find map\r\n",
            "11/02 10:00:00 [SignonState] CL: OnSwitchLoopModeFinished( game : fail )\r\n",
        ] {
            assert!(
                failure_regexes().iter().any(|re| re.is_match(line)),
                "not recognised as a failure: {line:?}"
            );
            assert!(
                !success_regex().is_match(line),
                "a failure line must not read as success: {line:?}"
            );
        }
        // the idle level a `disconnect` parks the engine on (playtest 2 `step4_reload.py`)
        assert!(idle_level_regex()
            .is_match("10/13 21:04:08 [Server] SV:  Spawn Server: <empty>\r\n"));
        assert!(!idle_level_regex().is_match("10/13 21:04:08 [Server] SV:  Spawn Server: probe\r\n"));
        assert_eq!(state_of(None), "timeout");
        assert_eq!(state_of(Some((true, "x".into()))), "ok");
        assert_eq!(state_of(Some((false, "x".into()))), "failed");
    }

    /// **M5**: a success (or a load failure) is only evidence for *this* map when it follows this
    /// window's `Loading map "<name>"` line. The only exception is the engine's own name validator,
    /// which a rejected `map <name>` prints instead of a load.
    #[test]
    fn the_verdict_is_anchored_to_this_maps_loading_line() {
        // a previous load's success, with our own `map` post having gone in during it: the
        // `Loading map` line names the previous map, so the success is not ours
        let previous = "10/13 20:24:00 Loading map \"old\"\r\n\
                        10/13 20:24:09 OnSwitchLoopModeFinished( game : success )\r\n";
        let lines = read_load_lines(previous, "probe");
        assert_eq!(lines.loading, None, "not this map's load");
        assert_eq!(lines.verdict, None, "a leftover success must not answer for this one");
        assert_eq!(state_of(lines.verdict), "timeout");

        // our own loading line, then the previous load's success: still not ours (the success
        // precedes the loading line that belongs to us)
        let mixed = "10/13 20:24:00 Loading map \"probe\"\r\n\
                     10/13 20:24:09 OnSwitchLoopModeFinished( game : success )\r\n";
        assert_eq!(
            read_load_lines(mixed, "probe").verdict.map(|(ok, _)| ok),
            Some(true),
            "the success follows our own loading line"
        );
        let after = "10/13 20:24:09 OnSwitchLoopModeFinished( game : success )\r\n\
                     10/13 20:24:10 Loading map \"probe\"\r\n";
        assert_eq!(
            read_load_lines(after, "probe").verdict,
            None,
            "a success *before* our loading line is not our verdict"
        );
        // ... it becomes our verdict only once something follows the loading line
        let after_fail = format!("{after}10/13 20:24:11 Failed to load map\r\n");
        let lines = read_load_lines(&after_fail, "probe");
        assert_eq!(lines.verdict.map(|(ok, _)| ok), Some(false));
        assert!(lines.loading.is_some());

        // the engine's map-name validator: printed instead of a `Loading map` line
        let rejected = "11/02 10:00:00 map: invalid map name\r\n";
        let lines = read_load_lines(rejected, "probe");
        assert_eq!(lines.loading, None);
        assert_eq!(lines.verdict.map(|(ok, _)| ok), Some(false));

        // an unrelated failure wording, without our anchor, is not accepted as our failure
        let unrelated = "11/02 10:00:00 Failed to load map\r\n";
        assert_eq!(
            read_load_lines(unrelated, "probe").verdict,
            None,
            "a generic failure without our loading line may be another call's output"
        );
    }

    /// The watch reads only what was appended after `open`, tolerates a recreated (shorter) file
    /// and keeps its own tail.
    #[test]
    fn the_console_watch_reads_from_its_offset() {
        // a lib unit test has no CARGO_TARGET_TMPDIR; the crate's own `target/` is gitignored
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp")
            .join(format!("run-map-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("console.log");
        std::fs::write(&path, "old line\r\n").unwrap();
        let mut watch = ConsoleWatch::open(path.clone());
        assert!(watch.exists);
        watch.poll();
        assert_eq!(watch.seen(), "", "nothing appended yet");

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut file, b"10/13 20:24:00 Loading map \"probe\"\r\n").unwrap();
        watch.poll();
        assert!(watch.seen().contains("Loading map \"probe\""));
        assert!(!watch.seen().contains("old line"), "the offset was respected");
        watch.poll();
        assert_eq!(watch.tail(5).len(), 1, "the tail is not duplicated");

        // a recreated file (a new session) restarts at 0
        std::fs::write(&path, "new session\r\n").unwrap();
        watch.poll();
        assert!(watch.seen().contains("new session"));

        // a file that never existed reads as absent
        let missing = ConsoleWatch::open(dir.join("nope/console.log"));
        assert!(!missing.exists);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **L5**: a recreated console.log can be *longer* than the old offset between two polls; the
    /// inode check catches it where the shrink check cannot.
    #[test]
    fn the_console_watch_restarts_when_the_file_is_replaced() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp")
            .join(format!("run-map-watch-replaced-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("console.log");
        std::fs::write(&path, "old\r\n").unwrap();
        let mut watch = ConsoleWatch::open(path.clone());
        watch.poll();
        assert_eq!(watch.seen(), "");
        // replaced by a LONGER file (a new session, or a `-condebug` restart). Written next to the
        // old one and renamed over it: the two files exist at the same time, so the new path's
        // inode cannot be the old one (no reliance on what the filesystem does with a freed inode)
        let staging = dir.join("console.new");
        std::fs::write(&staging, "10/14 01:20:00 Loading map \"probe\"\r\n").unwrap();
        assert!(
            std::fs::metadata(&staging).unwrap().len() > 5,
            "the replacement is longer than the old offset"
        );
        std::fs::rename(&staging, &path).unwrap();
        watch.poll();
        assert!(
            watch.seen().starts_with("10/14 01:20:00"),
            "the head of the replaced file is not lost: {:?}",
            watch.seen()
        );
        assert!(watch.seen().contains("Loading map \"probe\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A multibyte character cut by the end of one read is decoded whole by the next poll, and an
    /// unterminated last line is not a complete line yet.
    #[test]
    fn the_console_watch_decodes_utf8_across_polls_and_holds_back_a_partial_line() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp")
            .join(format!("run-map-watch-utf8-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("console.log");
        std::fs::write(&path, "").unwrap();
        let mut watch = ConsoleWatch::open(path.clone());
        let bytes = "caf\u{e9} \u{1F600} ok\r\npart".as_bytes().to_vec();
        // cut inside the two-byte e-acute
        let cut = bytes.iter().position(|b| *b == 0xC3).unwrap() + 1;
        std::fs::write(&path, &bytes[..cut]).unwrap();
        watch.poll();
        assert_eq!(watch.seen(), "caf", "the cut character waits for its second byte");
        std::fs::write(&path, &bytes).unwrap();
        watch.poll();
        assert_eq!(watch.seen(), "caf\u{e9} \u{1F600} ok\r\npart");
        let (lines, partial) = watch.complete_lines();
        assert_eq!(lines, vec!["caf\u{e9} \u{1F600} ok"]);
        assert!(partial, "`part` has no newline yet");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **M6**: `open_at` re-reads a previous call's window from its recorded offset, which is what
    /// the settle check does.
    #[test]
    fn the_settle_check_re_reads_the_previous_calls_window() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp")
            .join(format!("run-map-watch-at-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("console.log");
        let before = "old session\r\n";
        std::fs::write(&path, before).unwrap();
        let offset = before.len() as u64;
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(
            &mut file,
            b"10/13 20:24:00 Loading map \"probe\"\r\n10/13 20:24:09 OnSwitchLoopModeFinished( game : success )\r\n",
        )
        .unwrap();
        let mut watch = ConsoleWatch::open_at(path.clone(), Some(offset));
        watch.poll();
        let lines = read_load_lines(watch.seen(), "probe");
        assert_eq!(lines.verdict.map(|(ok, _)| ok), Some(true));
        assert!(!watch.seen().contains("old session"), "the offset was respected");
        // the plain `open` (send-time offset) sees nothing of what is already there
        let mut fresh = ConsoleWatch::open(path.clone());
        fresh.poll();
        assert_eq!(fresh.seen(), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (b) `unload` is a boolean argument, and the pattern is the only thing that selects the
    /// unload route (`false`/absent is the ordinary load).
    #[test]
    fn unload_is_a_boolean_argument() {
        assert!(!unload_requested(&json!({})).unwrap());
        assert!(!unload_requested(&json!({"unload": null})).unwrap());
        assert!(!unload_requested(&json!({"unload": false})).unwrap());
        assert!(unload_requested(&json!({"unload": true})).unwrap());
        for bad in [json!({"unload": "true"}), json!({"unload": 1})] {
            let error = unload_requested(&bad).unwrap_err();
            assert_eq!(error.type_, "InvalidArguments", "{bad}");
            assert!(error.message.contains("must be a boolean"), "{bad}: {error:?}");
        }
    }

    /// (c) the `disconnect` wait's budget note is for a caller who asked for a longer wait, not for
    /// the tool's own default (which is chosen to fit the budget).
    #[test]
    fn the_disconnect_wait_note_only_fires_for_an_explicit_request() {
        let ctx = CallCtx {
            budget: budget(2_000),
        };
        let mut notes: Vec<String> = Vec::new();
        let wait = disconnect_wait(&ctx, false, &mut notes);
        assert!(!wait.is_zero() && wait <= Duration::from_millis(2_000), "{wait:?}");
        assert!(notes.is_empty(), "a default call gets no note: {notes:?}");
        let mut notes: Vec<String> = Vec::new();
        let _ = disconnect_wait(&ctx, true, &mut notes);
        assert!(
            notes.iter().any(|note| note.contains("cut to")),
            "an explicit request is told: {notes:?}"
        );
    }

    /// A `HammerState` on paths that do not exist: the vpk lookup is file-local, so `resolve_vpk`
    /// can be exercised for real (nothing is executed).
    fn state() -> HammerState {
        use clap::Parser;
        use cs2_mcp_common::config::CommonConfig;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: CommonConfig,
        }
        let argv: Vec<&str> = vec![
            "t",
            "--project-root",
            "/nonexistent/cs2mcp-run-map-test",
            "--podman",
            "/nonexistent/cs2mcp-run-map-test-podman",
            "--vk-layer-dir",
            "/nonexistent/cs2mcp-run-map-test-vklayer",
            "--xauthority",
            "/nonexistent/cs2mcp-run-map-test-xauth",
        ];
        HammerState::from_config(&W::parse_from(argv).c).expect("state")
    }

    #[test]
    fn vpk_header_check_catches_junk_and_accepts_a_real_header() {
        let dir = std::env::temp_dir().join(format!("cs2mcp-vpkhdr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        };
        let mut good = vec![0x34, 0x12, 0xAA, 0x55, 2, 0, 0, 0, 4, 0, 0, 0];
        good.extend([0u8; 40]);
        assert_eq!(vpk_header_problem(&write("good.vpk", &good)), None);
        let junk: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        assert!(vpk_header_problem(&write("junk.vpk", &junk)).unwrap().contains("signature"));
        assert!(vpk_header_problem(&write("tiny.vpk", b"x")).unwrap().contains("shorter"));
        let mut huge = good.clone();
        huge[8..12].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert!(vpk_header_problem(&write("huge.vpk", &huge)).unwrap().contains("past the end"));
        let mut v1 = good;
        v1[4] = 1;
        assert!(vpk_header_problem(&write("v1.vpk", &v1)).unwrap().contains("version 1"));
        assert_eq!(vpk_header_problem(&dir.join("absent.vpk")), None, "unreadable: the engine decides");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A running compile job for the map refuses the load (data: jobId, useInstead job_status)
    /// unless `ignoreRunningCompile:true`; a finished job, another map, or another addon do not.
    #[test]
    fn a_running_compile_of_the_map_refuses_run_map_unless_overridden() {
        use crate::tools::compile_map::{CompileJob, CompileMode, JobState};
        let state = state();
        let job = |id: &str, job_state: JobState, path: &str| CompileJob {
            id: id.to_string(),
            tag: crate::tools::compile_map::tag_value(id),
            state: job_state,
            mode: CompileMode::Full,
            path: path.to_string(),
            addon: state.addon(),
            container: "c".into(),
            log: "/log".into(),
            vpk: Vec::new(),
            vmap_before: None,
            started: std::time::Instant::now(),
            deadline: Duration::from_secs(1500),
            attempts: 1,
            summary: None,
            ok_line: None,
            reason: None,
            cleanup: None,
        };
        let none = json!({});
        refuse_while_compiling(&state, "maps/p1.vmap", &none).expect("nothing running");
        state.compile_jobs.insert(job("c7", JobState::Running, "maps/p1.vmap"));
        let err = refuse_while_compiling(&state, "maps/p1.vmap", &none).unwrap_err();
        assert_eq!(err.type_, "CompileRunning");
        assert_eq!(err.data["jobId"], json!("c7"));
        assert_eq!(err.data["useInstead"]["tool"], json!("job_status"));
        assert_eq!(err.data["useInstead"]["args"]["jobId"], json!("c7"));
        assert!(err.retry_safe);
        refuse_while_compiling(&state, "maps/other.vmap", &none).expect("another map");
        refuse_while_compiling(&state, "maps/p1.vmap", &json!({"ignoreRunningCompile": true}))
            .expect("override");
        refuse_while_compiling(&state, "maps/p1.vmap", &json!({"ignoreRunningCompile": false}))
            .unwrap_err();
        refuse_while_compiling(&state, "maps/p1.vmap", &json!({"ignoreRunningCompile": "yes"}))
            .unwrap_err();
        state.compile_jobs.update("c7", |j| j.state = JobState::Ok);
        refuse_while_compiling(&state, "maps/p1.vmap", &none).expect("finished");
    }

    /// (d) `run_map` resolves the compiled vpk through the SAME candidate helper `compile_map` uses
    /// (one lookup, two tools), and keeps its own stricter flattened rule on top: a sub-directory
    /// map's own content-relative spelling is what the target names unless only the flattened one
    /// is there and newer.
    #[test]
    fn the_vpk_lookup_is_the_shared_candidate_helper() {
        let state = state();
        let content_path = "maps/sub/arena.vmap";
        let candidates =
            crate::tools::compile_map::vpk_candidates(&state.content().app_root, &state.addon(), content_path)
                .expect("the shared helper");
        assert_eq!(candidates.len(), 2, "both spellings for a sub-directory map");
        let target = resolve_vpk(&state, content_path).expect("resolved");
        // the paths the two tools look at are literally the same list
        let paths: Vec<String> = candidates.iter().map(|c| c.path.display().to_string()).collect();
        let target_paths: Vec<String> = target
            .vpk_candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(target_paths, paths);
        // neither exists: the content-relative spelling is what the caller is told to compile
        assert_eq!(target.vpk, candidates[0].path);
        assert_eq!(target.vpk_relative, candidates[0].relative);
        assert_eq!(target.vpk_form, "content-relative");
        assert_eq!(target.name, "sub/arena", "the engine name keeps the sub-directory");
        assert_eq!(target.flattened_rejected, Some("absent"));
        assert!(!target.vpk_found);
        assert!(!target.vmap_found);
        assert!(!target.stale_compile, "no vpk at all is not a stale one");
        // a top-level map: the helper deduplicates the two spellings into one file
        let top = resolve_vpk(&state, "maps/probe.vmap").expect("resolved");
        assert_eq!(top.vpk_candidates.len(), 1);
        assert_eq!(top.name, "probe");
        assert_eq!(top.flattened_rejected, None);
        // a path the helper refuses (not a maps/<x>.vmap) never reaches it
        assert!(resolve_vpk(&state, "maps/probe.vpcf").is_err());
    }
}
