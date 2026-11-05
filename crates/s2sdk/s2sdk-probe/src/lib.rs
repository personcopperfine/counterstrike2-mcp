//! `s2sdk-probe`: the `LD_PRELOAD` payload for the Linux CS2 sessions. `docker.rs` mounts the
//! payload dir read-only at `/cs2mcp/s2sdk` and `lib-preamble.sh` prepends
//! `/cs2mcp/s2sdk/libs2sdk_probe.so` to `LD_PRELOAD` (see [`s2sdk_probe_argv`] in cs2-mcp-common).
//!
//! * The ELF constructor runs in *every* process the session scripts start (bash, tee, cs2.sh's
//!   helpers, steam bits). It reads `/proc/self/exe` and returns immediately and silently unless
//!   the basename is the game binary ([`GAME_EXE`], verified against the image's `cs2.sh`:
//!   `GAMEEXE=bin/linuxsteamrt64/cs2`).
//! * In the game process it prints one early stderr line, then spawns a detached thread (never
//!   blocking the game) that runs the check sequence below.
//! * Every check is a read-only call through the `s2sdk` safe layer (`s2sdk/src/`): no
//!   hand-written offsets or vtable slots, every FFI result checked, and every wait is a poll
//!   against one deadline (`S2SDK_PROBE_DEADLINE_MS`, default 120000). Nothing here calls
//!   `AddListener`/`RemoveListener`: the engine's event manager is not thread-safe, and this is
//!   not the game thread.
//! * It reports exactly one RESULT line on stderr and one JSON object at
//!   `$S2SDK_PROBE_OUT` (docker.rs: `/work/s2sdk-probe.json`, i.e. the session artifacts dir,
//!   written atomically: tmp + rename):
//!
//! ```text
//! [s2sdk-probe] loaded pid=123 exe=/.../bin/linuxsteamrt64/cs2
//! [s2sdk-probe] RESULT ok stage=sdk
//! [s2sdk-probe] RESULT fail stage=timeout reason=schemasystem: libschemasystem.so not seen within 120000 ms; ...
//! ```
//!
//! # The check sequence (each step is one entry of the report's `checks` array)
//!
//! 1. `loaded` / `game_process` — the constructor ran in the game binary (informational).
//! 2. `schemasystem` — `libschemasystem.so` is mapped (`dlopen(RTLD_NOW | RTLD_NOLOAD)`
//!    first, a `/proc/self/maps` basename scan as a diagnostic fallback; the probe never loads
//!    a library itself).
//! 3. `schema_system` — `SchemaSystem::from_process()` (`SchemaSystem_001`) and
//!    `is_ready()` both true.
//! 4. `schema_scope` — the server module's type scope, tried under `SERVER_MODULE`, then
//!    `SERVER_MODULE_ALT`, then the Windows `"server.dll"` and the module's mapped path. The
//!    engine's lookup returns the *global* scope on a miss, so a candidate only counts when
//!    `IsGlobalScope()` is false; the report names which name resolved and the scope's own
//!    `name()`.
//! 5. `schema_class` — `CBaseEntity`: name / size / field count, every field name non-empty and
//!    every offset inside the class, the first few fields, `m_iHealth`'s offset, and the
//!    `CCSPlayerPawn` base-class chain reaching `CBaseEntity`.
//! 6. `cvar` — `Cvar::from_process_with_lib()` (which library served `VEngineCvar007`),
//!    `sv_cheats` (Bool, false on this server), `mp_roundtime` (> 0), and a missing cvar
//!    (`NotFound`).
//! 7. `events` — `GameEventManager::from_process()` plus `has_event` / `lookup_event_id`
//!    read-only calls. Non-mandatory: the Linux dedicated server serves
//!    `GAMEEVENTSMANAGER002` from none of its modules (live finding, WP6b), so a failure here
//!    is a reported finding, not a failed run (see `s2sdk/src/events.rs`, strategy B).
//!
//! `ok` on the RESULT line means every *mandatory* check passed ([`MANDATORY`]); `events` is
//! reported but never gates it. The stage is `sdk` when they all pass, the last completed stage
//! otherwise, and `timeout` when the deadline expired mid-sequence.
//!
//! The JSON layout is [`report_json`]. The live test driver is `tests/live.rs` (podman).

use std::collections::HashSet;
use std::ffi::{CString, OsStr};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use libc::{c_char, c_int, c_void};

use s2sdk::cvar::{Cvar, CvarValue};
use s2sdk::events::GameEventManager;
use s2sdk::module::versions as sdk_versions;
use s2sdk::schema::{SchemaSystem, TypeScope, SERVER_MODULE, SERVER_MODULE_ALT};

/// Basename of the game binary. Every process whose `/proc/self/exe` does not end in this is
/// left alone (the LD_PRELOAD reaches all of the session's shell processes too).
pub const GAME_EXE: &str = "cs2";
/// The library the probe waits for. Checked with `RTLD_NOLOAD` only: the game must load it.
const SCHEMA_LIB: &str = "libschemasystem.so";
/// Env var holding the JSON report path (`docker.rs`: `/work/s2sdk-probe.json`).
pub const OUT_ENV: &str = "S2SDK_PROBE_OUT";
/// Env var holding the poll deadline in milliseconds.
pub const DEADLINE_ENV: &str = "S2SDK_PROBE_DEADLINE_MS";
const DEFAULT_DEADLINE_MS: u64 = 120_000;
/// Pause between probes. Short on purpose: the game may exit at any moment, and no probe
/// outcome is worth blocking it for.
const POLL_PAUSE: Duration = Duration::from_millis(100);
/// Pause between two named checks (the game keeps running while the probe reads).
const STEP_PAUSE: Duration = Duration::from_millis(10);

/// The check names that gate the RESULT line, in report order. `events` is deliberately not
/// here: `GAMEEVENTSMANAGER002` availability is the known unknown (see `s2sdk/src/events.rs`).
pub const MANDATORY: [&str; 5] = [
    "schemasystem",
    "schema_system",
    "schema_scope",
    "schema_class",
    "cvar",
];

/// The checks that make up the `schemasystem` stage: the library, the interface and the first
/// two schema reads. All of them must pass for the stage to be reported as `schemasystem`.
const SCHEMA_STAGE_CHECKS: [&str; 4] = [
    "schemasystem",
    "schema_system",
    "schema_scope",
    "schema_class",
];

/// Every check the report can carry, in report order (the shape is stable on every path: a step
/// that is not reached is reported `ok:false`).
pub const ALL_CHECKS: [&str; 8] = [
    "loaded",
    "game_process",
    "schemasystem",
    "schema_system",
    "schema_scope",
    "schema_class",
    "cvar",
    "events",
];

/// The class whose declared fields the `schema_class` check reads.
pub const FIELD_CLASS: &str = "CBaseEntity";
/// The field the `schema_class` check requires and reports the offset of.
pub const HEALTH_FIELD: &str = "m_iHealth";
/// The base chain the `schema_class` check walks: `BASE_CHAIN_START` must reach
/// `BASE_CHAIN_TARGET`.
pub const BASE_CHAIN_START: &str = "CCSPlayerPawn";
/// The class the base-class walk looks for (see [`BASE_CHAIN_START`]).
pub const BASE_CHAIN_TARGET: &str = "CBaseEntity";
/// How many leading fields the report shows.
pub const FIRST_FIELDS: usize = 5;
/// How deep [`base_chain`] may walk before it gives up (a cycle or a pathological schema graph
/// must not spin).
pub const MAX_BASE_DEPTH: usize = 32;

// dlopen/dlclose/dlerror (libdl, in glibc since 2.34 — the image runs 2.41). The probe only
// *checks* for mapped libraries; it never loads one.
extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn dlerror() -> *const c_char;
}

use libc::{RTLD_NOLOAD, RTLD_NOW};

/// How far the probe got. `loaded`: the constructor ran but no SDK check ran; `schemasystem`:
/// the schema layer answered; `sdk`: every mandatory check passed; `timeout`: the deadline
/// passed before a mandatory check could be evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Loaded,
    Schemasystem,
    Sdk,
    Timeout,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Loaded => "loaded",
            Stage::Schemasystem => "schemasystem",
            Stage::Sdk => "sdk",
            Stage::Timeout => "timeout",
        }
    }
}

/// A JSON value for the report's per-check `data` object.
///
/// Written by hand like the rest of the report (serde-free: the payload stays small and its
/// glibc surface tiny). Every string is escaped through [`json_escape`].
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    /// An object as ordered key/value pairs (the report's shape stays diff-able).
    Object(Vec<(String, Json)>),
}

impl Json {
    /// A string value.
    pub fn str(s: impl Into<String>) -> Self {
        Json::Str(s.into())
    }

    /// An object from `(&str, Json)` pairs.
    pub fn obj(pairs: Vec<(&str, Json)>) -> Self {
        Json::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    /// The JSON text for this value.
    pub fn to_json(&self) -> String {
        match self {
            Json::Null => "null".to_string(),
            Json::Bool(b) => b.to_string(),
            Json::Int(i) => i.to_string(),
            Json::UInt(u) => u.to_string(),
            // `{:?}` keeps the ".0" (a JSON reader can tell 2.0 from 2); NaN/infinity have no
            // JSON literal, so they are reported as strings instead of invalid JSON.
            Json::Float(f) if f.is_finite() => format!("{f:?}"),
            Json::Float(f) => Json::Str(format!("{f}")).to_json(),
            Json::Str(s) => format!("\"{}\"", json_escape(s)),
            Json::Array(items) => {
                let mut s = String::from("[");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    s.push_str(&item.to_json());
                }
                s.push(']');
                s
            }
            Json::Object(pairs) => {
                let mut s = String::from("{");
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    s.push_str(&format!("\"{}\":{}", json_escape(k), v.to_json()));
                }
                s.push('}');
                s
            }
        }
    }
}

/// One named result in the report's `checks` array.
///
/// `mandatory` is what the RESULT line's `ok` is computed from ([`MANDATORY`]); `data` is the
/// check's evidence (ids, offsets, names — never a guess).
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub mandatory: bool,
    pub detail: String,
    pub data: Vec<(String, Json)>,
}

impl Check {
    fn new(name: &str, ok: bool, mandatory: bool, detail: String) -> Self {
        Check {
            name: name.to_string(),
            ok,
            mandatory,
            detail,
            data: Vec::new(),
        }
    }

    /// Attach the check's evidence object.
    fn with_data(mut self, data: Vec<(&str, Json)>) -> Self {
        self.data = data.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        self
    }

    /// The check's JSON object.
    fn to_json(&self) -> String {
        let data = Json::Object(self.data.clone()).to_json();
        format!(
            "{{\"name\":\"{}\",\"ok\":{},\"mandatory\":{},\"detail\":\"{}\",\"data\":{}}}",
            json_escape(&self.name),
            self.ok,
            self.mandatory,
            json_escape(&self.detail),
            data
        )
    }
}

/// The full report, as written to `$S2SDK_PROBE_OUT` (format fixed by [`report_json`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub pid: u64,
    pub exe: String,
    pub stage: Stage,
    pub ok: bool,
    pub checks: Vec<Check>,
    pub elapsed_ms: u64,
}

/// True when `exe` (a `/proc/self/exe` readlink) is the game binary: basename `cs2`, so the
/// dedicated server (`bin/linuxsteamrt64/cs2`) matches and `bash`, `tee` and the steam helpers
/// do not.
pub fn is_game_exe(exe: &str) -> bool {
    Path::new(exe).file_name() == Some(OsStr::new(GAME_EXE))
}

/// Is `name` in [`MANDATORY`]?
pub fn is_mandatory(name: &str) -> bool {
    MANDATORY.contains(&name)
}

/// The RESULT line: exactly one is printed per game process.
pub fn result_line(ok: bool, stage: Stage, reason: Option<&str>) -> String {
    match (ok, reason) {
        (true, _) => format!("[s2sdk-probe] RESULT ok stage={}", stage.as_str()),
        (false, Some(r)) => format!(
            "[s2sdk-probe] RESULT fail stage={} reason={r}",
            stage.as_str()
        ),
        (false, None) => format!(
            "[s2sdk-probe] RESULT fail stage={} reason=unspecified",
            stage.as_str()
        ),
    }
}

/// The report as one JSON object, written by hand (serde-free: the payload stays small and its
/// glibc surface tiny). Strings are escaped. Version 2 added `mandatory` and `data` per check
/// (WP6b); the top-level fields are unchanged from version 1.
pub fn report_json(r: &Report) -> String {
    let mut s = String::with_capacity(512);
    s.push_str("{\"probe\":\"s2sdk-probe\",\"version\":2");
    s.push_str(&format!(",\"pid\":{}", r.pid));
    s.push_str(&format!(",\"exe\":\"{}\"", json_escape(&r.exe)));
    s.push_str(&format!(
        ",\"stage\":\"{}\",\"ok\":{}",
        r.stage.as_str(),
        r.ok
    ));
    s.push_str(",\"checks\":[");
    for (i, c) in r.checks.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&c.to_json());
    }
    s.push(']');
    s.push_str(&format!(",\"elapsed_ms\":{}", r.elapsed_ms));
    s.push('}');
    s
}

/// Minimal JSON string escaping: backslash, quote and the C0 controls (as \\u00XX).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// check aggregation (pure: the RESULT line's ok/stage/reason)
// ---------------------------------------------------------------------------

/// Did every mandatory check pass? A mandatory check that is missing from the report counts as
/// a failure (the report's shape is only complete on the success path).
pub fn all_mandatory_ok(checks: &[Check]) -> bool {
    MANDATORY
        .iter()
        .all(|name| checks.iter().any(|c| c.name == *name && c.ok))
        && !checks.iter().any(|c| c.mandatory && !c.ok)
}

/// The stage the run reached: `timeout` when the deadline expired mid-sequence, `sdk` when
/// every mandatory check passed, else the last completed stage (`schemasystem` once the whole
/// schema layer answered, `loaded` before that).
pub fn final_stage(checks: &[Check], timed_out: bool) -> Stage {
    if timed_out {
        return Stage::Timeout;
    }
    if all_mandatory_ok(checks) {
        return Stage::Sdk;
    }
    if SCHEMA_STAGE_CHECKS
        .iter()
        .all(|name| checks.iter().any(|c| c.name == *name && c.ok))
    {
        Stage::Schemasystem
    } else {
        Stage::Loaded
    }
}

/// The RESULT line's `reason` for a failing run: the first failing mandatory check, in report
/// order (`None` when nothing mandatory failed).
pub fn failure_reason(checks: &[Check]) -> Option<String> {
    checks
        .iter()
        .find(|c| c.mandatory && !c.ok)
        .map(|c| format!("{}: {}", c.name, c.detail))
}

// ---------------------------------------------------------------------------
// the base-class walk (pure: no game, unit-tested with a fake hierarchy)
// ---------------------------------------------------------------------------

/// Depth-first search from `start` through its (transitive) base classes looking for `target`;
/// returns the chain of names from `start` to `target`, inclusive, when the target is reached.
/// `bases_of` yields the direct base names of a class (empty for an unknown one).
///
/// Cycle- and depth-safe: each class is visited once and the walk stops at `max_depth`, so a
/// malformed schema graph cannot spin. Generic over the lookup so the walk is unit-tested
/// without the game.
pub fn base_chain<B>(
    start: &str,
    target: &str,
    bases_of: &mut B,
    max_depth: usize,
) -> Option<Vec<String>>
where
    B: FnMut(&str) -> Vec<String>,
{
    fn walk<B>(
        path: &mut Vec<String>,
        target: &str,
        bases_of: &mut B,
        visited: &mut HashSet<String>,
        depth: usize,
        max_depth: usize,
    ) -> bool
    where
        B: FnMut(&str) -> Vec<String>,
    {
        if path.last().map(String::as_str) == Some(target) {
            return true;
        }
        if depth >= max_depth {
            return false;
        }
        let current = path.last().cloned().unwrap_or_default();
        for base in bases_of(&current) {
            if !visited.insert(base.clone()) {
                continue; // already on the path (or explored): a cycle cannot recurse
            }
            path.push(base);
            if walk(path, target, bases_of, visited, depth + 1, max_depth) {
                return true;
            }
            path.pop();
        }
        false
    }

    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(start.to_string());
    let mut path = vec![start.to_string()];
    if walk(&mut path, target, bases_of, &mut visited, 1, max_depth) {
        Some(path)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// library detection (the WP7a path, kept as the first check)
// ---------------------------------------------------------------------------

/// How the schema library was detected in this process, when it is mapped at all: the
/// detection method ("RTLD_NOLOAD" or "/proc/self/maps"), or `None`. Primary check is
/// `dlopen(RTLD_NOLOAD)` (never loads it itself; a null handle costs one `dlclose` of the
/// extra reference). Fallback is a `/proc/self/maps` basename scan, so a failing NOLOAD
/// lookup is reported (`timeout_diagnostics`, the check's detail) instead of hiding a loaded
/// library. (An early version passed a wrong RTLD_NOLOAD value, which glibc rejects as an
/// invalid mode; the constants now come from `libc`.)
fn schema_lib_seen() -> Option<&'static str> {
    if dlopen_noload(SCHEMA_LIB) {
        Some("RTLD_NOLOAD")
    } else if maps_contain(SCHEMA_LIB) {
        Some("/proc/self/maps")
    } else {
        None
    }
}

/// `dlopen(name, RTLD_NOW | RTLD_NOLOAD)`; non-null means already mapped.
fn dlopen_noload(name: &str) -> bool {
    let Ok(cname) = CString::new(name) else {
        return false;
    };
    unsafe {
        dlerror();
        let handle = dlopen(cname.as_ptr(), RTLD_NOW | RTLD_NOLOAD);
        if handle.is_null() {
            false
        } else {
            dlclose(handle);
            true
        }
    }
}

/// The `dlerror()` text of the last failed call (for the timeout diagnostics).
fn last_dlerror() -> Option<String> {
    unsafe {
        let p = dlerror();
        if p.is_null() {
            None
        } else {
            Some(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
        }
    }
}

/// True when some mapped file's path ends with `/<name>` (e.g. the engine's own module dirs).
fn maps_contain(name: &str) -> bool {
    let needle = format!("/{name}");
    match std::fs::read_to_string("/proc/self/maps") {
        Ok(maps) => maps.lines().any(|l| l.contains(&needle)),
        Err(_) => false,
    }
}

/// Why the NOLOAD lookup by SONAME may have failed while the library is mapped anyway:
/// the dlerror text, whether the maps agree, and how many of the engine's modules the
/// caller's link map lists (`dl_iterate_phdr` — 0 would mean the engine loads its modules
/// outside the base namespace).
fn timeout_diagnostics() -> String {
    let dlerr = last_dlerror().unwrap_or_else(|| "(dlerror empty)".into());
    let mapped = maps_contain(SCHEMA_LIB);
    format!(
        "dlopen(NOLOAD) error: {dlerr}; /proc/self/maps says mapped: {mapped}; engine modules in this process's link map (dl_iterate_phdr): {}",
        phdr_engine_modules()
    )
}

unsafe extern "C" fn phdr_cb(
    info: *mut libc::dl_phdr_info,
    _size: usize,
    data: *mut c_void,
) -> c_int {
    if !info.is_null() {
        let name = (*info).dlpi_name;
        if !name.is_null()
            && std::ffi::CStr::from_ptr(name)
                .to_string_lossy()
                .contains("linuxsteamrt64")
        {
            let count = data as *mut u32;
            *count += 1;
        }
    }
    0
}

/// How many mapped objects the caller's link map lists under the engine's module dir.
fn phdr_engine_modules() -> u32 {
    let mut count: u32 = 0;
    unsafe {
        libc::dl_iterate_phdr(Some(phdr_cb), &mut count as *mut u32 as *mut c_void);
    }
    count
}

// ---------------------------------------------------------------------------
// the poll loop (pure helpers: injected clock, predicate and pause)
// ---------------------------------------------------------------------------

/// Poll `f` until it yields `Some`, returning the value and how many attempts it took; `None`
/// when `now_ms()` reached `deadline_ms` first. `f` sees the 1-based attempt number. The clock
/// and the pause are injected so tests run the loop without sleeping.
pub fn poll_until<T, C, S>(
    now_ms: &mut C,
    f: &mut impl FnMut(u32) -> Option<T>,
    pause: &mut S,
    deadline_ms: u64,
) -> Option<(T, u32)>
where
    C: FnMut() -> u64,
    S: FnMut(),
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        if let Some(value) = f(attempt) {
            return Some((value, attempt));
        }
        if now_ms() >= deadline_ms {
            return None;
        }
        pause();
    }
}

/// The `libschemasystem.so` poll of WP7a, kept as the first check: [`Stage::Schemasystem`]
/// when the predicate said "seen", [`Stage::Timeout`] once `now_ms` passed `deadline_ms`
/// first.
pub fn poll_schema<C, P, S>(
    now_ms: &mut C,
    schema_loaded: &mut P,
    pause: &mut S,
    deadline_ms: u64,
) -> Stage
where
    C: FnMut() -> u64,
    P: FnMut() -> bool,
    S: FnMut(),
{
    let mut predicate = |_attempt: u32| schema_loaded().then_some(());
    match poll_until(now_ms, &mut predicate, pause, deadline_ms) {
        Some(_) => Stage::Schemasystem,
        None => Stage::Timeout,
    }
}

// ---------------------------------------------------------------------------
// the run: checks in order, one deadline
// ---------------------------------------------------------------------------

/// One probe run: the report's checks, the deadline they share, and the "the deadline expired
/// before this check could run" flag that turns the stage into `timeout`.
struct Run {
    exe: String,
    started: Instant,
    deadline_ms: u64,
    checks: Vec<Check>,
    timed_out: bool,
}

impl Run {
    fn new(exe: &str, started: Instant, deadline_ms: u64) -> Self {
        let mut run = Run {
            exe: exe.to_string(),
            started,
            deadline_ms,
            checks: Vec::new(),
            timed_out: false,
        };
        run.checks.push(Check::new(
            "loaded",
            true,
            false,
            "constructor ran in the game process".to_string(),
        ));
        run.checks.push(Check::new(
            "game_process",
            true,
            false,
            format!("/proc/self/exe is the game binary ({exe})"),
        ));
        run
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn push(&mut self, check: Check) {
        self.checks.push(check);
    }

    /// Poll `f` against this run's deadline; `None` means the deadline expired (and marks the
    /// run as timed out).
    fn poll<T>(&mut self, mut f: impl FnMut(u32) -> Option<T>) -> Option<(T, u32)> {
        let started = self.started;
        let deadline_ms = self.deadline_ms;
        let result = poll_until(
            &mut || started.elapsed().as_millis() as u64,
            &mut f,
            &mut || std::thread::sleep(POLL_PAUSE),
            deadline_ms,
        );
        if result.is_none() {
            self.timed_out = true;
        }
        result
    }

    /// Report every check that was not reached (in [`ALL_CHECKS`] order) as a failure, so the
    /// report's shape is the same on every path.
    fn skip_unreached(&mut self, reason: &str) {
        for name in ALL_CHECKS {
            if !self.checks.iter().any(|c| c.name == name) {
                self.checks.push(Check::new(
                    name,
                    false,
                    is_mandatory(name),
                    format!("not reached: {reason}"),
                ));
            }
        }
    }

    /// The report: ok/stage/reason derived from the checks (nothing hand-set).
    fn finish(self) -> Report {
        let ok = all_mandatory_ok(&self.checks);
        let stage = final_stage(&self.checks, self.timed_out);
        let reason = (!ok).then(|| failure_reason(&self.checks)).flatten();
        emit(&self.exe, self.started, stage, ok, reason, self.checks)
    }
}

/// The `schemasystem` check: the probe's own `RTLD_NOLOAD` scan for the library (the game must
/// load it; the probe never does).
fn check_schemasystem_library(run: &mut Run) -> bool {
    match run.poll(|_| schema_lib_seen()) {
        Some((via, attempts)) => {
            run.push(
                Check::new(
                    "schemasystem",
                    true,
                    true,
                    format!(
                        "{SCHEMA_LIB} seen ({via}) after {attempts} polls ({} ms)",
                        run.elapsed_ms()
                    ),
                )
                .with_data(vec![
                    ("library", Json::str(SCHEMA_LIB)),
                    ("detected_via", Json::str(via)),
                    ("attempts", Json::UInt(u64::from(attempts))),
                ]),
            );
            true
        }
        None => {
            let diag = timeout_diagnostics();
            run.push(
                Check::new(
                    "schemasystem",
                    false,
                    true,
                    format!(
                        "{SCHEMA_LIB} not seen within {} ms; {diag}",
                        run.deadline_ms
                    ),
                )
                .with_data(vec![
                    ("library", Json::str(SCHEMA_LIB)),
                    ("detected_via", Json::Null),
                    ("diagnostics", Json::str(diag)),
                ]),
            );
            false
        }
    }
}

/// The `schema_system` check: `SchemaSystem_001` exists and the schema system says it is ready.
fn check_schema_system(run: &mut Run) -> Option<SchemaSystem> {
    let mut last = String::new();
    let polled = run.poll(|_| match SchemaSystem::from_process() {
        Err(e) => {
            last = format!("CreateInterface: {e}");
            None
        }
        Ok(schema) => match schema.is_ready() {
            Ok(true) => Some(schema),
            Ok(false) => {
                last = "SchemaSystemIsReady() == false".to_string();
                None
            }
            Err(e) => {
                last = format!("SchemaSystemIsReady: {e}");
                None
            }
        },
    });
    match polled {
        Some((schema, attempts)) => {
            std::thread::sleep(STEP_PAUSE);
            run.push(
                Check::new(
                    "schema_system",
                    true,
                    true,
                    format!(
                        "SchemaSystem_001 created and ready after {attempts} polls ({} ms)",
                        run.elapsed_ms()
                    ),
                )
                .with_data(vec![
                    ("interface", Json::str("SchemaSystem_001")),
                    ("library", Json::str("libschemasystem.so")),
                    ("ready", Json::Bool(true)),
                    ("attempts", Json::UInt(u64::from(attempts))),
                ]),
            );
            Some(schema)
        }
        None => {
            run.push(
                Check::new(
                    "schema_system",
                    false,
                    true,
                    format!(
                        "SchemaSystem_001 did not become ready within {} ms (last: {last})",
                        run.deadline_ms
                    ),
                )
                .with_data(vec![
                    ("interface", Json::str("SchemaSystem_001")),
                    ("ready", Json::Bool(false)),
                    ("last_error", Json::str(last)),
                ]),
            );
            None
        }
    }
}

/// The `schema_scope` check: the server module's type scope.
///
/// The engine's `FindTypeScopeForModule` **never returns null on a miss**: it hands back the
/// *global* scope (named `!GlobalTypes`) instead, so a candidate name is only accepted when
/// `IsGlobalScope()` says the scope is not the global one (live fact, WP6b run 1: the brief's
/// `"libserver.so"` resolved to `!GlobalTypes`, and the `CBaseEntity` lookup that followed
/// failed). The candidate names are [`SERVER_MODULE`], [`SERVER_MODULE_ALT`],
/// [`SERVER_MODULE_BINDING`] and the module's own mapped path; every attempt is recorded in the
/// check's `data`.
fn check_schema_scope(run: &mut Run, schema: &SchemaSystem) -> Option<TypeScope> {
    // Rebuilt every poll round: the module's mapped path only becomes a candidate once
    // `libserver.so` is mapped (which is what the poll is waiting for anyway).
    let mut candidates: Vec<String> = server_scope_candidates();
    let mut attempts: Vec<Json> = Vec::new();
    let mut found: Option<(String, TypeScope)> = None;
    let polled = run.poll(|_| {
        candidates = server_scope_candidates();
        attempts.clear();
        for name in &candidates {
            match schema.type_scope(name) {
                Ok(scope) => match scope.is_global_scope() {
                    Ok(true) => attempts.push(Json::obj(vec![
                        ("module_name", Json::str(name.clone())),
                        (
                            "resolved",
                            Json::str("global scope (module not registered)"),
                        ),
                    ])),
                    Ok(false) => {
                        attempts.push(Json::obj(vec![
                            ("module_name", Json::str(name.clone())),
                            ("resolved", Json::str(scope_name_or_placeholder(&scope))),
                        ]));
                        found = Some((name.clone(), scope));
                        return Some(());
                    }
                    Err(e) => attempts.push(Json::obj(vec![
                        ("module_name", Json::str(name.clone())),
                        (
                            "resolved",
                            Json::str(format!("(IsGlobalScope failed: {e})")),
                        ),
                    ])),
                },
                Err(e) => attempts.push(Json::obj(vec![
                    ("module_name", Json::str(name.clone())),
                    ("resolved", Json::str(format!("(lookup failed: {e})"))),
                ])),
            }
        }
        None
    });
    let mut data = vec![
        (
            "module_names_tried",
            Json::Array(candidates.iter().map(Json::str).collect()),
        ),
        (
            "attempts",
            Json::UInt(u64::from(polled.map_or(0, |(_value, attempts)| attempts))),
        ),
        ("candidate_results", Json::Array(attempts.clone())),
    ];

    match polled {
        Some(_) => {
            std::thread::sleep(STEP_PAUSE);
            let (found_via, scope) = found?;
            let name = scope.name();
            data.push(("found_via", Json::str(found_via.clone())));
            match &name {
                Ok(n) => {
                    data.push(("scope_name", Json::str(n.clone())));
                    run.push(
                        Check::new(
                            "schema_scope",
                            true,
                            true,
                            format!("server type scope found via {found_via:?}; scope name {n:?}"),
                        )
                        .with_data(data),
                    );
                    Some(scope)
                }
                Err(e) => {
                    data.push(("scope_name", Json::Null));
                    run.push(
                        Check::new(
                            "schema_scope",
                            false,
                            true,
                            format!(
                                "server type scope found via {found_via:?} but GetScopeName failed: {e}"
                            ),
                        )
                        .with_data(data),
                    );
                    None
                }
            }
        }
        None => {
            data.push(("found_via", Json::Null));
            run.push(
                Check::new(
                    "schema_scope",
                    false,
                    true,
                    format!(
                        "no module-local server type scope within {} ms: every candidate name \
                         resolved to the global scope ({})",
                        run.deadline_ms,
                        candidates.join(", ")
                    ),
                )
                .with_data(data),
            );
            None
        }
    }
}

/// The module names [`check_schema_scope`] tries, in order: the brief's two names, the Windows
/// schema module name, and the module's own mapped path.
///
/// `"server.dll"` is not a guess: the Linux `libserver.so` carries that exact string (a
/// module-name accessor at file offset 0x20ba520 returns it — the same constant the Windows
/// module's schema lookups use), so a Linux build could key its scope under it. Live-verified
/// (WP6b): it does not; `"libserver.so"` is the Linux registration name. The extra names cost a
/// hash lookup each and are the only way to tell "registered under another name" from "not
/// registered yet", because a miss returns the global scope, not null.
const SERVER_MODULE_WINDOWS: &str = "server.dll";

fn server_scope_candidates() -> Vec<String> {
    let mut names = vec![
        SERVER_MODULE.to_string(),
        SERVER_MODULE_ALT.to_string(),
        SERVER_MODULE_WINDOWS.to_string(),
    ];
    if let Some(module) = s2sdk::module::Module::find_loaded(SERVER_MODULE) {
        let path = module.path().display().to_string();
        if !path.is_empty() && !names.contains(&path) {
            names.push(path);
        }
    }
    names
}

/// The scope's `name()`, or a placeholder when it cannot be read (used inside another check's
/// data).
fn scope_name_or_placeholder(scope: &TypeScope) -> String {
    scope
        .name()
        .unwrap_or_else(|e| format!("(GetScopeName failed: {e})"))
}

/// The `schema_class` check: `CBaseEntity`'s own reads, plus the `CCSPlayerPawn` base chain.
fn check_schema_class(run: &mut Run, scope: &TypeScope) -> bool {
    std::thread::sleep(STEP_PAUSE);
    let mut problems: Vec<String> = Vec::new();

    // The class may be registered a moment after the scope itself: poll for it.
    let mut last = String::new();
    let polled = run.poll(|_| match scope.find_class_threadsafe(FIELD_CLASS) {
        Ok(class) => Some(class),
        Err(e) => {
            last = e.to_string();
            None
        }
    });
    let Some((class, attempts)) = polled else {
        run.push(
            Check::new(
                "schema_class",
                false,
                true,
                format!(
                    "{FIELD_CLASS} not declared within {} ms (last: {last})",
                    run.deadline_ms
                ),
            )
            .with_data(vec![
                ("class", Json::str(FIELD_CLASS)),
                ("scope_name", scope_name_json(scope)),
                ("last_error", Json::str(last)),
            ]),
        );
        return false;
    };

    let class_name = match class.name() {
        Ok(n) => n,
        Err(e) => {
            problems.push(format!("name(): {e}"));
            String::new()
        }
    };
    let size = class.size();
    let declared = class.field_count();
    let scope_name = scope_name_json(scope);

    let mut seen: u32 = 0;
    let mut errors: Vec<String> = Vec::new();
    let mut first: Vec<Json> = Vec::new();
    let mut health: Option<i32> = None;
    for field in class.fields() {
        match field {
            Ok(f) => {
                seen += 1;
                if f.name.is_empty() {
                    errors.push(format!("field #{seen} has an empty name"));
                }
                if f.offset < 0 || f.offset >= size {
                    errors.push(format!(
                        "field {} offset {} outside the class (size {size})",
                        f.name, f.offset
                    ));
                }
                if f.name == HEALTH_FIELD {
                    health = Some(f.offset);
                }
                if first.len() < FIRST_FIELDS {
                    first.push(Json::obj(vec![
                        ("name", Json::str(f.name.clone())),
                        ("offset", Json::Int(i64::from(f.offset))),
                        ("type_name", Json::str(f.type_name.clone())),
                    ]));
                }
            }
            Err(e) => errors.push(format!("field #{seen} unreadable: {e}")),
        }
    }
    if u32::from(declared) != seen {
        problems.push(format!(
            "iterated {seen} fields, m_nFieldCount says {declared}"
        ));
    }
    problems.extend(errors.iter().cloned());
    if health.is_none() {
        problems.push(format!("{HEALTH_FIELD} not declared on {FIELD_CLASS}"));
    }

    // The base chain of CCSPlayerPawn must reach CBaseEntity.
    let mut bases_of = |name: &str| -> Vec<String> {
        match scope.find_class_threadsafe(name) {
            Ok(c) => c
                .base_classes()
                .into_iter()
                .filter_map(|(_, base)| base.name().ok())
                .collect(),
            Err(_) => Vec::new(),
        }
    };
    let chain = base_chain(
        BASE_CHAIN_START,
        BASE_CHAIN_TARGET,
        &mut bases_of,
        MAX_BASE_DEPTH,
    );
    if chain.is_none() {
        let direct = bases_of(BASE_CHAIN_START);
        problems.push(format!(
            "{BASE_CHAIN_START} has no base chain to {BASE_CHAIN_TARGET} (direct bases: {direct:?})"
        ));
    }

    let data = vec![
        ("class", Json::str(class_name.clone())),
        ("scope_name", scope_name),
        ("size", Json::Int(i64::from(size))),
        ("field_count", Json::UInt(u64::from(declared))),
        ("fields_iterated", Json::UInt(u64::from(seen))),
        ("fields_checked", Json::UInt(u64::from(seen))),
        ("attempts", Json::UInt(u64::from(attempts))),
        ("first_fields", Json::Array(first)),
        (
            HEALTH_FIELD,
            match health {
                Some(offset) => Json::obj(vec![
                    ("present", Json::Bool(true)),
                    ("offset", Json::Int(i64::from(offset))),
                ]),
                None => Json::obj(vec![("present", Json::Bool(false)), ("offset", Json::Null)]),
            },
        ),
        (
            "base_chain",
            match &chain {
                Some(chain) => Json::Array(chain.iter().map(Json::str).collect()),
                None => Json::Array(Vec::new()),
            },
        ),
        ("base_chain_start", Json::str(BASE_CHAIN_START)),
        ("base_chain_target", Json::str(BASE_CHAIN_TARGET)),
    ];

    let ok = problems.is_empty();
    let detail = if ok {
        format!(
            "{class_name}: size {size}, {declared} fields (all named, all inside the class), \
             {HEALTH_FIELD} at {:?}, base chain {} -> {BASE_CHAIN_TARGET}",
            health.unwrap_or(-1),
            BASE_CHAIN_START
        )
    } else {
        format!("{class_name}: {}", problems.join("; "))
    };
    run.push(Check::new("schema_class", ok, true, detail).with_data(data));
    ok
}

/// The scope's `name()`, or `null` when it cannot be read (used inside another check's data).
fn scope_name_json(scope: &TypeScope) -> Json {
    match scope.name() {
        Ok(n) => Json::str(n),
        Err(_) => Json::Null,
    }
}

/// The name of a read convar value's type tag (the `CvarValue` variant is the type the convar
/// declares, `EConVarType`-named).
fn cvar_value_name(value: &CvarValue) -> &'static str {
    match value {
        CvarValue::Bool(_) => "Bool",
        CvarValue::I16(_) => "Int16",
        CvarValue::U16(_) => "UInt16",
        CvarValue::I32(_) => "Int32",
        CvarValue::U32(_) => "UInt32",
        CvarValue::I64(_) => "Int64",
        CvarValue::U64(_) => "UInt64",
        CvarValue::F32(_) => "Float32",
        CvarValue::F64(_) => "Float64",
        CvarValue::StringUnavailableOffThread => "String",
        CvarValue::Color(_) => "Color",
        CvarValue::Vec2(_) => "Vector2",
        CvarValue::Vec3(_) => "Vector3",
        CvarValue::Vec4(_) => "Vector4",
        CvarValue::QAngle(_) => "QAngle",
        CvarValue::Unknown(_) => "Unknown",
    }
}

/// The value as JSON (colors and vectors as arrays).
fn cvar_value_json(value: &CvarValue) -> Json {
    match value {
        CvarValue::Bool(b) => Json::Bool(*b),
        CvarValue::I16(v) => Json::Int(i64::from(*v)),
        CvarValue::U16(v) => Json::UInt(u64::from(*v)),
        CvarValue::I32(v) => Json::Int(i64::from(*v)),
        CvarValue::U32(v) => Json::UInt(u64::from(*v)),
        CvarValue::I64(v) => Json::Int(*v),
        CvarValue::U64(v) => Json::UInt(*v),
        CvarValue::F32(v) => Json::Float(f64::from(*v)),
        CvarValue::F64(v) => Json::Float(*v),
        CvarValue::StringUnavailableOffThread => {
            Json::str("(string: game-thread-only read)".to_string())
        }
        CvarValue::Color(c) => Json::Array(c.iter().map(|b| Json::UInt(u64::from(*b))).collect()),
        CvarValue::Vec2(v) => Json::Array(v.iter().map(|f| Json::Float(f64::from(*f))).collect()),
        CvarValue::Vec3(v) => Json::Array(v.iter().map(|f| Json::Float(f64::from(*f))).collect()),
        CvarValue::Vec4(v) => Json::Array(v.iter().map(|f| Json::Float(f64::from(*f))).collect()),
        CvarValue::QAngle(v) => Json::Array(v.iter().map(|f| Json::Float(f64::from(*f))).collect()),
        CvarValue::Unknown(tag) => Json::str(format!("EConVarType({})", tag.0)),
    }
}

/// Is the value a number greater than zero (the `mp_roundtime` check)?
fn cvar_value_positive(value: &CvarValue) -> bool {
    match value {
        CvarValue::I16(v) => *v > 0,
        CvarValue::U16(v) => *v > 0,
        CvarValue::I32(v) => *v > 0,
        CvarValue::U32(v) => *v > 0,
        CvarValue::I64(v) => *v > 0,
        CvarValue::U64(v) => *v > 0,
        CvarValue::F32(v) => *v > 0.0,
        CvarValue::F64(v) => *v > 0.0,
        _ => false,
    }
}

/// The `cvar` check: the interface (and which library served it), `sv_cheats`, `mp_roundtime`
/// and a cvar that must not exist.
fn check_cvar(run: &mut Run) -> bool {
    std::thread::sleep(STEP_PAUSE);
    let (cvar, lib) = match Cvar::from_process_with_lib() {
        Ok(ok) => ok,
        Err(e) => {
            run.push(
                Check::new(
                    "cvar",
                    false,
                    true,
                    format!("VEngineCvar007 not available: {e}"),
                )
                .with_data(vec![
                    ("interface", Json::str("VEngineCvar007")),
                    ("serve_module", Json::Null),
                    ("error", Json::str(e.to_string())),
                ]),
            );
            return false;
        }
    };

    let mut data = vec![
        ("interface", Json::str("VEngineCvar007")),
        ("serve_module", Json::str(lib)),
    ];
    let mut problems: Vec<String> = Vec::new();

    // sv_cheats: a Bool convar, false on an unmodified dedicated server.
    let mut sv_cheats = Json::Null;
    match cvar.find("sv_cheats") {
        Ok(convar) => match convar.value() {
            Ok(value) => {
                let name = cvar_value_name(&value);
                sv_cheats = Json::obj(vec![
                    ("var_type", Json::str(name)),
                    ("value", cvar_value_json(&value)),
                ]);
                if name != "Bool" {
                    problems.push(format!("sv_cheats is {name}, expected Bool"));
                } else if value != CvarValue::Bool(false) {
                    problems.push(format!(
                        "sv_cheats is {} on this server, expected false",
                        cvar_value_json(&value).to_json()
                    ));
                }
            }
            Err(e) => problems.push(format!("sv_cheats value: {e}")),
        },
        Err(e) => problems.push(format!("sv_cheats: {e}")),
    }

    // mp_roundtime: a positive number.
    let mut mp_roundtime = Json::Null;
    match cvar.find("mp_roundtime") {
        Ok(convar) => match convar.value() {
            Ok(value) => {
                let name = cvar_value_name(&value);
                mp_roundtime = Json::obj(vec![
                    ("var_type", Json::str(name)),
                    ("value", cvar_value_json(&value)),
                ]);
                if !matches!(value, CvarValue::I32(_) | CvarValue::F32(_)) {
                    problems.push(format!("mp_roundtime is {name}, expected Int32/Float32"));
                }
                if !cvar_value_positive(&value) {
                    problems.push("mp_roundtime is not > 0".to_string());
                }
            }
            Err(e) => problems.push(format!("mp_roundtime value: {e}")),
        },
        Err(e) => problems.push(format!("mp_roundtime: {e}")),
    }

    // A cvar that must not exist: the lookup's NotFound path.
    let mut missing = Json::Null;
    match cvar.find("no_such_cvar_s2sdk") {
        Ok(_) => problems.push("no_such_cvar_s2sdk was found".to_string()),
        Err(e) => {
            let not_found = matches!(&e, s2sdk::Error::NotFound(_));
            missing = Json::str(e.to_string());
            if !not_found {
                problems.push(format!("no_such_cvar_s2sdk: unexpected error {e}"));
            }
        }
    }

    data.push(("sv_cheats", sv_cheats));
    data.push(("mp_roundtime", mp_roundtime));
    data.push(("missing_cvar", missing));

    let ok = problems.is_empty();
    let detail = if ok {
        format!(
            "VEngineCvar007 from {lib}: sv_cheats Bool false, mp_roundtime > 0, missing cvar NotFound"
        )
    } else {
        problems.join("; ")
    };
    run.push(Check::new("cvar", ok, true, detail).with_data(data));
    ok
}

/// The `events` check (non-mandatory): the manager, `round_start` and a missing event, all
/// read-only (`AddListener` is deliberately not called: not the game thread).
///
/// Live finding (WP6b): the version string is not served by any module of the Linux dedicated
/// server, so this check reports `ok:false` with the lookup error — the manager needs the
/// `ISource2Server`-relative lookup (`s2sdk/src/events.rs`, strategy B).
fn check_events(run: &mut Run) -> bool {
    std::thread::sleep(STEP_PAUSE);
    let version = sdk_versions::GAME_EVENTS_MANAGER_INTERFACE_VERSION;
    let manager = match GameEventManager::from_process() {
        Ok(manager) => manager,
        Err(e) => {
            run.push(
                Check::new(
                    "events",
                    false,
                    false,
                    format!(
                        "{version} unavailable ({e}); non-mandatory (see s2sdk/src/events.rs, strategy B)"
                    ),
                )
                .with_data(vec![
                    ("interface", Json::str(version)),
                    ("library", Json::str("libengine2.so")),
                    ("manager", Json::Bool(false)),
                    ("error", Json::str(e.to_string())),
                ]),
            );
            return false;
        }
    };

    let mut problems: Vec<String> = Vec::new();
    let mut has_round_start = Json::Null;
    match manager.has_event("round_start") {
        Ok(true) => has_round_start = Json::Bool(true),
        Ok(false) => {
            has_round_start = Json::Bool(false);
            problems.push("round_start has no event descriptor".to_string());
        }
        Err(e) => problems.push(format!("has_event(round_start): {e}")),
    }
    let mut round_start_id = Json::Null;
    match manager.lookup_event_id("round_start") {
        Ok(id) => {
            round_start_id = Json::Int(i64::from(id));
            if id < 0 {
                problems.push(format!("lookup_event_id(round_start) = {id}"));
            }
        }
        Err(e) => problems.push(format!("lookup_event_id(round_start): {e}")),
    }
    let mut has_missing = Json::Null;
    match manager.has_event("no_such_event_s2sdk") {
        Ok(false) => has_missing = Json::Bool(false),
        Ok(true) => {
            has_missing = Json::Bool(true);
            problems.push("no_such_event_s2sdk has an event descriptor".to_string());
        }
        Err(e) => problems.push(format!("has_event(no_such_event_s2sdk): {e}")),
    }

    let ok = problems.is_empty();
    let detail = if ok {
        format!(
            "{version} from libengine2.so: round_start id {}, unknown event not registered",
            round_start_id.to_json()
        )
    } else {
        problems.join("; ")
    };
    run.push(Check::new("events", ok, false, detail).with_data(vec![
        ("interface", Json::str(version)),
        ("library", Json::str("libengine2.so")),
        ("manager", Json::Bool(true)),
        ("has_round_start", has_round_start),
        ("round_start_id", round_start_id),
        ("has_missing_event", has_missing),
    ]));
    ok
}

/// The ELF constructor. Cheap, never panics across FFI, never blocks the game: the polling
/// happens on a detached thread (see [`start`]).
#[used]
#[link_section = ".init_array"]
static INIT: extern "C" fn() = init;

extern "C" fn init() {
    // a panic unwinding through the loader is UB for the host process: catch everything
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(start));
}

/// Constructor body, game processes only (everything else returned before this was called).
fn start() {
    let exe = match std::fs::read_link("/proc/self/exe") {
        Ok(p) => p,
        Err(_) => return, // cannot tell what process this is: stay silent
    };
    let exe = exe.to_string_lossy().into_owned();
    if !is_game_exe(&exe) {
        return; // bash / tee / steam bits: silent
    }
    let started = Instant::now();
    eprintln!(
        "[s2sdk-probe] loaded pid={} exe={}",
        std::process::id(),
        exe
    );
    let exe_for_thread = exe.clone();
    let spawn = std::thread::Builder::new()
        .name("s2sdk-probe".into())
        .spawn(move || {
            // the thread must never take the process down either
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                thread_main(&exe_for_thread, started);
            }));
        });
    if let Err(e) = spawn {
        let deadline_ms = deadline_ms();
        let mut run = Run::new(&exe, started, deadline_ms);
        run.push(Check::new(
            "schemasystem",
            false,
            true,
            format!("probe thread spawn failed: {e}"),
        ));
        run.skip_unreached("the probe thread could not start");
        run.finish();
    }
}

/// The poll deadline: `$S2SDK_PROBE_DEADLINE_MS`, default [`DEFAULT_DEADLINE_MS`].
fn deadline_ms() -> u64 {
    std::env::var(DEADLINE_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DEADLINE_MS)
}

/// Thread body: the check sequence, on one deadline, then the report.
fn thread_main(exe: &str, started: Instant) {
    let deadline_ms = deadline_ms();
    let mut run = Run::new(exe, started, deadline_ms);

    if !check_schemasystem_library(&mut run) {
        run.skip_unreached("the schema library was never seen");
        run.finish();
        return;
    }
    let Some(schema) = check_schema_system(&mut run) else {
        run.skip_unreached("the schema system never became ready");
        run.finish();
        return;
    };
    let Some(scope) = check_schema_scope(&mut run, &schema) else {
        run.skip_unreached("no server type scope was found");
        run.finish();
        return;
    };
    if !check_schema_class(&mut run, &scope) {
        run.skip_unreached(if run.timed_out {
            "the class check timed out"
        } else {
            "the class check failed"
        });
        run.finish();
        return;
    }
    if !check_cvar(&mut run) {
        run.skip_unreached("the cvar check failed");
        run.finish();
        return;
    }
    // Non-mandatory: reported either way.
    check_events(&mut run);
    run.finish();
}

/// Print the RESULT line and write the JSON report (when `$S2SDK_PROBE_OUT` is set). The
/// constructor already printed the `loaded` line.
fn emit(
    exe: &str,
    started: Instant,
    stage: Stage,
    ok: bool,
    reason: Option<String>,
    checks: Vec<Check>,
) -> Report {
    let report = Report {
        pid: u64::from(std::process::id()),
        exe: exe.to_string(),
        stage,
        ok,
        checks,
        elapsed_ms: started.elapsed().as_millis() as u64,
    };
    eprintln!("{}", result_line(ok, stage, reason.as_deref()));
    if let Some(out) = std::env::var_os(OUT_ENV) {
        let json = report_json(&report);
        if let Err(e) = write_report_atomic(Path::new(&out), &json) {
            eprintln!(
                "[s2sdk-probe] report write failed ({}): {e}",
                Path::new(&out).display()
            );
        }
    }
    report
}

/// Write `json` to `path` atomically: tmp file in the same directory (rename stays on one
/// filesystem), then rename over the target. A reader never sees a half-written report.
fn write_report_atomic(path: &Path, json: &str) -> std::io::Result<()> {
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch dir for fs-touching tests: `<workspace target>/s2sdk-probe-test-scratch` (the
    /// repo keeps scratch out of /tmp, which is a tmpfs).
    fn target_scratch() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target/s2sdk-probe-test-scratch")
    }

    /// A check with the given name/ok and no data.
    fn check(name: &str, ok: bool) -> Check {
        Check::new(name, ok, is_mandatory(name), format!("{name} detail"))
    }

    /// Every mandatory check passing, events failing: still `ok`/`sdk` (events is reported,
    /// not gating).
    fn passing_checks() -> Vec<Check> {
        let mut checks: Vec<Check> = ALL_CHECKS
            .iter()
            .map(|name| check(name, *name != "events"))
            .collect();
        // `loaded`/`game_process` are informational but ok.
        for c in checks.iter_mut() {
            if c.name == "loaded" || c.name == "game_process" {
                c.ok = true;
            }
        }
        checks
    }

    #[test]
    fn exe_filter_matches_only_the_game_binary() {
        assert!(is_game_exe(
            "/h/steamapps/common/C/game/bin/linuxsteamrt64/cs2"
        ));
        assert!(is_game_exe("cs2")); // bare name (readlink is always absolute, but be safe)
        assert!(!is_game_exe("/bin/bash"));
        assert!(!is_game_exe("/usr/bin/tee"));
        assert!(!is_game_exe("/h/game/cs2.sh"));
        assert!(!is_game_exe("/h/game/bin/linuxsteamrt64/cs2_old"));
        assert!(!is_game_exe(""));
    }

    #[test]
    fn mandatory_set_matches_the_brief() {
        assert_eq!(
            MANDATORY,
            [
                "schemasystem",
                "schema_system",
                "schema_scope",
                "schema_class",
                "cvar"
            ]
        );
        assert!(!is_mandatory("events"));
        assert!(!is_mandatory("loaded"));
        assert!(!is_mandatory("game_process"));
        assert!(is_mandatory("cvar"));
    }

    #[test]
    fn json_values_render() {
        assert_eq!(Json::Null.to_json(), "null");
        assert_eq!(Json::Bool(true).to_json(), "true");
        assert_eq!(Json::Int(-3).to_json(), "-3");
        assert_eq!(Json::UInt(7).to_json(), "7");
        assert_eq!(Json::Float(2.5).to_json(), "2.5");
        assert_eq!(Json::Float(2.0).to_json(), "2.0");
        assert_eq!(Json::str("a\"b").to_json(), "\"a\\\"b\"");
        assert_eq!(
            Json::Array(vec![Json::Int(1), Json::str("x")]).to_json(),
            "[1,\"x\"]"
        );
        assert_eq!(
            Json::obj(vec![("b", Json::Bool(false)), ("n", Json::Null)]).to_json(),
            "{\"b\":false,\"n\":null}"
        );
        assert_eq!(Json::Array(Vec::new()).to_json(), "[]");
        assert_eq!(Json::Object(Vec::new()).to_json(), "{}");
        // NaN has no JSON literal: it is reported as a string, never as broken JSON.
        assert_eq!(Json::Float(f64::NAN).to_json(), "\"NaN\"");
    }

    #[test]
    fn json_report_shape_with_data_objects() {
        let r = Report {
            pid: 1234,
            exe: "/h/game/bin/linuxsteamrt64/cs2".into(),
            stage: Stage::Sdk,
            ok: true,
            checks: vec![
                Check::new(
                    "loaded",
                    true,
                    false,
                    "constructor ran in the game process".into(),
                ),
                Check::new(
                    "schemasystem",
                    true,
                    true,
                    "libschemasystem.so seen (RTLD_NOLOAD)".into(),
                )
                .with_data(vec![
                    ("library", Json::str("libschemasystem.so")),
                    ("detected_via", Json::str("RTLD_NOLOAD")),
                    ("attempts", Json::UInt(3)),
                ]),
                Check::new("schema_class", true, true, "CBaseEntity: size 1234".into()).with_data(
                    vec![
                        (
                            "first_fields",
                            Json::Array(vec![Json::obj(vec![
                                ("name", Json::str("m_iHealth")),
                                ("offset", Json::Int(836)),
                                ("type_name", Json::str("int32")),
                            ])]),
                        ),
                        (
                            "m_iHealth",
                            Json::obj(vec![
                                ("present", Json::Bool(true)),
                                ("offset", Json::Int(836)),
                            ]),
                        ),
                    ],
                ),
            ],
            elapsed_ms: 4210,
        };
        assert_eq!(
            report_json(&r),
            concat!(
                "{\"probe\":\"s2sdk-probe\",\"version\":2,\"pid\":1234,",
                "\"exe\":\"/h/game/bin/linuxsteamrt64/cs2\",",
                "\"stage\":\"sdk\",\"ok\":true,\"checks\":[",
                "{\"name\":\"loaded\",\"ok\":true,\"mandatory\":false,",
                "\"detail\":\"constructor ran in the game process\",\"data\":{}},",
                "{\"name\":\"schemasystem\",\"ok\":true,\"mandatory\":true,",
                "\"detail\":\"libschemasystem.so seen (RTLD_NOLOAD)\",",
                "\"data\":{\"library\":\"libschemasystem.so\",\"detected_via\":\"RTLD_NOLOAD\",\"attempts\":3}},",
                "{\"name\":\"schema_class\",\"ok\":true,\"mandatory\":true,",
                "\"detail\":\"CBaseEntity: size 1234\",",
                "\"data\":{\"first_fields\":[{\"name\":\"m_iHealth\",\"offset\":836,\"type_name\":\"int32\"}],",
                "\"m_iHealth\":{\"present\":true,\"offset\":836}}}",
                "],\"elapsed_ms\":4210}"
            )
        );
    }

    #[test]
    fn json_report_escapes_strings() {
        let r = Report {
            pid: 1,
            exe: "/tmp/x\"q\\w\n\t".into(),
            stage: Stage::Timeout,
            ok: false,
            checks: vec![Check::new("n\"a", false, true, "\u{1}".to_string())
                .with_data(vec![("k\\1", Json::str("\"v\""))])],
            elapsed_ms: 0,
        };
        assert_eq!(
            report_json(&r),
            concat!(
                "{\"probe\":\"s2sdk-probe\",\"version\":2,\"pid\":1,",
                "\"exe\":\"/tmp/x\\\"q\\\\w\\n\\t\",",
                "\"stage\":\"timeout\",\"ok\":false,\"checks\":[",
                "{\"name\":\"n\\\"a\",\"ok\":false,\"mandatory\":true,",
                "\"detail\":\"\\u0001\",\"data\":{\"k\\\\1\":\"\\\"v\\\"\"}}",
                "],\"elapsed_ms\":0}"
            )
        );
    }

    #[test]
    fn result_lines() {
        assert_eq!(
            result_line(true, Stage::Sdk, None),
            "[s2sdk-probe] RESULT ok stage=sdk"
        );
        assert_eq!(
            result_line(true, Stage::Schemasystem, None),
            "[s2sdk-probe] RESULT ok stage=schemasystem"
        );
        assert_eq!(
            result_line(false, Stage::Timeout, Some("libschemasystem.so not seen within 120000 ms")),
            "[s2sdk-probe] RESULT fail stage=timeout reason=libschemasystem.so not seen within 120000 ms"
        );
        assert_eq!(
            result_line(false, Stage::Loaded, None),
            "[s2sdk-probe] RESULT fail stage=loaded reason=unspecified"
        );
    }

    #[test]
    fn aggregation_ok_and_stage() {
        let all = passing_checks();
        assert!(all_mandatory_ok(&all));
        assert_eq!(final_stage(&all, false), Stage::Sdk);
        assert_eq!(failure_reason(&all), None);

        // events failing does not change the verdict (non-mandatory, reported).
        let mut with_events_failed = all.clone();
        for c in with_events_failed.iter_mut() {
            if c.name == "events" {
                c.ok = false;
            }
        }
        assert!(all_mandatory_ok(&with_events_failed));
        assert_eq!(final_stage(&with_events_failed, false), Stage::Sdk);

        // cvar failing: the schema stage is still complete, so the stage is schemasystem.
        let mut cvar_failed = all.clone();
        for c in cvar_failed.iter_mut() {
            if c.name == "cvar" {
                c.ok = false;
            }
        }
        assert!(!all_mandatory_ok(&cvar_failed));
        assert_eq!(final_stage(&cvar_failed, false), Stage::Schemasystem);
        assert_eq!(
            failure_reason(&cvar_failed).as_deref(),
            Some("cvar: cvar detail")
        );

        // the class check failing before the schema stage completes: stage stays loaded.
        let mut class_failed = all.clone();
        for c in class_failed.iter_mut() {
            if c.name == "schema_class" {
                c.ok = false;
            }
        }
        assert_eq!(final_stage(&class_failed, false), Stage::Loaded);
        assert_eq!(
            failure_reason(&class_failed).as_deref(),
            Some("schema_class: schema_class detail")
        );

        // the deadline expiring is always the timeout stage, however far the run got.
        assert_eq!(final_stage(&all, true), Stage::Timeout);
    }

    #[test]
    fn aggregation_treats_a_missing_mandatory_check_as_failure() {
        let checks = vec![check("loaded", true), check("schemasystem", true)];
        assert!(!all_mandatory_ok(&checks));
        assert_eq!(final_stage(&checks, false), Stage::Loaded);
    }

    #[test]
    fn unreached_checks_keep_the_report_shape() {
        // A run that stopped at the library check: every later check is reported, in order.
        let mut run = Run::new("/h/game/bin/linuxsteamrt64/cs2", Instant::now(), 1000);
        run.push(check("schemasystem", false));
        run.skip_unreached("the schema library was never seen");
        let names: Vec<&str> = run.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ALL_CHECKS.to_vec());
        assert_eq!(
            run.checks[3].detail,
            "not reached: the schema library was never seen"
        );
        assert!(run.checks[0].ok, "loaded stays ok");
        assert!(
            !run.checks
                .iter()
                .find(|c| c.name == "events")
                .unwrap()
                .mandatory
        );
        assert!(
            run.checks
                .iter()
                .find(|c| c.name == "schema_system")
                .unwrap()
                .mandatory
        );
    }

    /// The poll loop with an injected clock and predicate: no real sleeping.
    #[test]
    fn poll_returns_schemasystem_when_the_library_appears() {
        let now = std::cell::Cell::new(0u64);
        let mut pauses = 0u32;
        let stage = poll_schema(
            &mut || {
                now.set(now.get() + 50);
                now.get()
            },
            // the library "appears" after 500 ms
            &mut || now.get() >= 500,
            &mut || pauses += 1,
            120_000,
        );
        assert_eq!(stage, Stage::Schemasystem);
        assert_eq!(pauses, 10); // checks at 0,50..,450 ms: 10 pauses, the 11th check sees it
    }

    #[test]
    fn poll_times_out_at_the_deadline() {
        let now = std::cell::Cell::new(0u64);
        let mut pauses = 0u32;
        let stage = poll_schema(
            &mut || {
                now.set(now.get() + 100);
                now.get()
            },
            &mut || false,
            &mut || pauses += 1,
            1_000,
        );
        assert_eq!(stage, Stage::Timeout);
        assert_eq!(pauses, 9); // 9 pauses (100..900 ms); the check at 1000 ms returns first
    }

    #[test]
    fn poll_deadline_zero_never_pauses() {
        let mut pauses = 0u32;
        let stage = poll_schema(&mut || 0, &mut || false, &mut || pauses += 1, 0);
        assert_eq!(stage, Stage::Timeout);
        assert_eq!(pauses, 0);
    }

    #[test]
    fn poll_until_counts_attempts_and_returns_the_value() {
        let now = std::cell::Cell::new(0u64);
        let mut pauses = 0u32;
        let mut f = |attempt: u32| (attempt >= 3).then_some(attempt * 10);
        let found = poll_until(
            &mut || {
                now.set(now.get() + 100);
                now.get()
            },
            &mut f,
            &mut || pauses += 1,
            10_000,
        );
        assert_eq!(found, Some((30, 3)));
        assert_eq!(pauses, 2);

        let mut never = |_attempt: u32| -> Option<u32> { None };
        let timed_out = poll_until(&mut || 0, &mut never, &mut || {}, 0);
        assert_eq!(timed_out, None);
    }

    /// The base-class walk, on a fake hierarchy with a diamond and a cycle: the walk is
    /// cycle-safe and reports the path it took.
    #[test]
    fn base_chain_walks_to_the_target() {
        let bases = |name: &str| -> Vec<String> {
            match name {
                "CCSPlayerPawn" => vec!["CCSPlayerPawnBase".to_string()],
                "CCSPlayerPawnBase" => vec!["CBasePlayerPawn".to_string()],
                "CBasePlayerPawn" => vec!["CBaseCombatCharacter".to_string()],
                "CBaseCombatCharacter" => vec!["CBaseEntity".to_string()],
                // two paths to the same base (a diamond) and a cycle back to the start
                "CBaseEntity" => vec!["CBaseEntity".to_string()],
                _ => Vec::new(),
            }
        };
        let mut bases = bases;
        assert_eq!(
            base_chain("CCSPlayerPawn", "CBaseEntity", &mut bases, MAX_BASE_DEPTH),
            Some(vec![
                "CCSPlayerPawn".to_string(),
                "CCSPlayerPawnBase".to_string(),
                "CBasePlayerPawn".to_string(),
                "CBaseCombatCharacter".to_string(),
                "CBaseEntity".to_string(),
            ])
        );
        // The start itself counts as reached (a class is its own ancestor here).
        assert_eq!(
            base_chain("CCSPlayerPawn", "CCSPlayerPawn", &mut bases, MAX_BASE_DEPTH),
            Some(vec!["CCSPlayerPawn".to_string()])
        );
        // An unreachable target and a missing class come back as None, never a spin.
        assert_eq!(
            base_chain("CCSPlayerPawn", "CNoSuchClass", &mut bases, MAX_BASE_DEPTH),
            None
        );
        let mut empty = |_: &str| Vec::new();
        assert_eq!(
            base_chain("CNoSuchClass", "CBaseEntity", &mut empty, MAX_BASE_DEPTH),
            None
        );
        // A self-cycle with a target elsewhere terminates at the depth bound.
        let mut cycle = |name: &str| {
            if name == "CBaseEntity" {
                Vec::new()
            } else {
                vec!["CA".to_string(), "CB".to_string()]
            }
        };
        assert_eq!(
            base_chain("CA", "CBaseEntity", &mut cycle, MAX_BASE_DEPTH),
            None
        );
    }

    #[test]
    fn cvar_value_helpers_read_the_variant() {
        assert_eq!(cvar_value_name(&CvarValue::Bool(false)), "Bool");
        assert_eq!(cvar_value_name(&CvarValue::F32(1.5)), "Float32");
        assert_eq!(cvar_value_name(&CvarValue::I32(2)), "Int32");
        assert_eq!(
            cvar_value_name(&CvarValue::Vec3([1.0, 2.0, 3.0])),
            "Vector3"
        );
        assert_eq!(cvar_value_json(&CvarValue::Bool(false)).to_json(), "false");
        assert_eq!(cvar_value_json(&CvarValue::F32(2.5)).to_json(), "2.5");
        assert_eq!(cvar_value_json(&CvarValue::I32(-1)).to_json(), "-1");
        assert_eq!(
            cvar_value_json(&CvarValue::StringUnavailableOffThread).to_json(),
            "\"(string: game-thread-only read)\""
        );
        assert_eq!(
            cvar_value_json(&CvarValue::Vec3([1.0, 2.0, 3.0])).to_json(),
            "[1.0,2.0,3.0]"
        );
        assert!(cvar_value_positive(&CvarValue::F32(2.5)));
        assert!(cvar_value_positive(&CvarValue::I32(2)));
        assert!(!cvar_value_positive(&CvarValue::F32(0.0)));
        assert!(!cvar_value_positive(&CvarValue::Bool(true)));
    }

    #[test]
    fn atomic_write_replaces_the_target() {
        // scratch under the workspace target dir, never /tmp (repo rule: tmpfs)
        let dir = target_scratch().join("atomic-write-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.json");
        write_report_atomic(&path, "{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}\n");
        write_report_atomic(&path, "{\"a\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":2}\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
