//! `job.start` / `job.status` / `job.cancel`: bridge jobs, first kind `resourcecompiler`
//! (bridge m3.12).
//!
//! A job runs `<gamedir>\bin\win64\resourcecompiler.exe` (the tools' asset compiler) as a child
//! of the tools process, so the calling model starts a compile, polls it and cancels it through
//! one API and never sees Wine, a container, or the lingering `vrad3.exe` tree.
//!
//! # Lane-answered
//!
//! All three rows are `needs == 0` and answered **on the IPC lane** ([`answer`], called from
//! `crate::wiring::BridgeState::route_with_params`), never queued to the main thread: spawning
//! and polling a process needs no tick, so they answer while Hammer sits in a modal or a map
//! load freezes the frame loop (the `metamod.plugins` precedent). `job.start` is a
//! [`crate::registry::RUST_METHODS`] row; `job.status` / `job.cancel` are
//! [`crate::registry::RUST_OVERRIDES`] of the C's blocked rows.
//!
//! # Wire
//!
//! * `job.start {kind:"resourcecompiler", input, game?, flags?, timeoutMs?, clientRef?}` ->
//!   `{jobId, state:"running", pid, log:{windows, relative}, command:[argv...], startedAtMs}`.
//!   `input` is a content-relative `csgo_addons/<addon>/...` path (see [`validate_input`]);
//!   `game:true` adds `-game <gamedir>\csgo`; `flags` is a token list from a fixed allowlist
//!   ([`parse_flags`]); `timeoutMs` is 1000..=3 600 000 (default 1 500 000). `-i <abs input>`
//!   is always last. At most one job runs at a time, 16 finished jobs are kept.
//! * Job ids are `j<N>-<nonce>`: `N` counts starts in this load, the nonce is the table's
//!   creation time (epoch ms, base 36), so an id never repeats across bridge loads.
//! * `job.status {jobId}` -> `{jobId, kind, state, input, clientRef, pid, exitCode?, elapsedMs,
//!   summary?, failLines, fatal, logTail, log, startedAtMs}`; `{}` -> `{jobs:[{jobId, kind,
//!   state, input, clientRef, elapsedMs, startedAtMs, log}]}`. `clientRef` is the caller's
//!   optional `job.start` token (`null` without one): a caller whose start reply was lost finds
//!   exactly its job by it.
//! * `job.cancel {jobId}` -> `{cancelled:true, state:"cancelled"}`; a finished job ->
//!   `{cancelled:false, state}`; an unknown id -> `-32602`.
//!
//! # Shape of the code
//!
//! The platform half is the [`Host`] / [`Process`] pair (Windows: `CreateProcessW` suspended,
//! assigned to a Job Object with `KILL_ON_JOB_CLOSE`, resumed; elsewhere: nothing to spawn).
//! Everything else - the parser, the validator, the [`LogScanner`], the [`JobTable`] and its
//! watcher threads, the reply builders - is plain Rust over those two traits and is host-tested
//! with a fake process (`tests/job.rs`).
//!
//! A watcher thread owns each running job: it reads the log incrementally (lossy, a non-UTF-8
//! byte never fails a read), ends the tree on the compiler's ` OK: N compiled, F failed ...`
//! line (state from the counts), on process exit without that line (`failed`, with the exit
//! code), or at the deadline (`timeout`). `job.cancel` and [`JobTable::shutdown`] terminate the
//! job object directly and set `cancelled`.

use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// The only job kind.
pub const KIND: &str = "resourcecompiler";
/// `job.start`.
pub const START: &str = "job.start";
/// `job.status`.
pub const STATUS: &str = "job.status";
/// `job.cancel`.
pub const CANCEL: &str = "job.cancel";
/// The `blockedOn` of every platform without a Windows resourcecompiler.
pub const WINDOWS_ONLY: &str = "job: resourcecompiler is Windows-only";
/// Default deadline of a job (`timeoutMs` omitted): 1500 s.
pub const DEFAULT_TIMEOUT_MS: u64 = 1_500_000;
/// Longest accepted `timeoutMs`: 3600 s.
pub const MAX_TIMEOUT_MS: u64 = 3_600_000;
/// Shortest accepted `timeoutMs`.
pub const MIN_TIMEOUT_MS: u64 = 1_000;
/// Finished jobs kept in the table; the oldest finished one is evicted past this.
pub const MAX_FINISHED: usize = 16;
/// How long after a `FATAL ERROR` line (with no OK line) a job is ended `failed`.
pub const FATAL_GRACE: Duration = Duration::from_secs(10);
/// The log poll interval of the production table.
pub const POLL: Duration = Duration::from_millis(100);
/// `fail_lines` kept per job.
const MAX_FAIL_LINES: usize = 100;
/// Lines kept in the log tail.
const TAIL_LINES: usize = 40;
/// A kept log line is cut to this many characters.
const MAX_LINE_CHARS: usize = 400;
/// The input path's total length limit.
const MAX_INPUT_LEN: usize = 260;

const INVALID_PARAMS: (i64, &str) = (-32602, "InvalidParams");
const PREREQUISITE: (i64, &str) = (-32017, "PrerequisiteFailed");
const INTERNAL: (i64, &str) = (-32603, "InternalError");

/// A lane reply: the bare result object, or `(code, message, data_json)`.
pub type Reply = Result<String, (i64, &'static str, String)>;

fn invalid(reason: &str) -> (i64, &'static str, String) {
    (INVALID_PARAMS.0, INVALID_PARAMS.1, json!({ "reason": reason }).to_string())
}

fn prerequisite(blocked_on: &str) -> (i64, &'static str, String) {
    (PREREQUISITE.0, PREREQUISITE.1, json!({ "blockedOn": blocked_on }).to_string())
}

fn internal(reason: &str) -> (i64, &'static str, String) {
    (INTERNAL.0, INTERNAL.1, json!({ "reason": reason }).to_string())
}

/// Whether this module serves `name` on the control plane.
#[must_use]
pub fn serves(name: &str) -> bool {
    matches!(name, START | STATUS | CANCEL)
}

/// The lane's answer for one of the three rows over the production table.
pub fn answer(method: &str, params: Option<&str>) -> Reply {
    table().handle(method, params)
}

// ---------------------------------------------------------------------------------------------
// The platform seam
// ---------------------------------------------------------------------------------------------

/// Where things live, derived from the running process image. Paths are strings in the
/// platform's own separator [`Layout::sep`] (backslash on Windows).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    /// The path separator used by every path below.
    pub sep: char,
    /// `<gamedir>\bin\win64\resourcecompiler.exe`.
    pub exe: String,
    /// `<gamedir>\bin\win64` - the compiler's working directory.
    pub cwd: String,
    /// `<gamedir>` (the `game` directory; `-game <gamedir>\csgo`).
    pub game_dir: String,
    /// `<gamedir>\..\content`, spelled without the `..` (`Z:\app\content` live).
    pub content_root: String,
    /// The per-job log directory (`<gamedir>\..\cs2mcp_jobs`, `Z:\app\cs2mcp_jobs` live).
    pub log_dir: String,
    /// The log directory relative to the app directory (the host maps it): `cs2mcp_jobs`.
    pub log_dir_rel: String,
}

impl Layout {
    /// The layout around a host executable path (`...\game\bin\win64\cs2.exe`). Both `\` and
    /// `/` split the path; the result uses `sep`. Refuses a path that is not under
    /// `bin\win64` of a game directory with a parent (the app directory).
    pub fn from_host_exe(host_exe: &str, sep: char) -> Result<Layout, String> {
        let parts: Vec<&str> = host_exe.split(['\\', '/']).collect();
        let bad = || format!("job: cannot locate the game directory from {host_exe:?}");
        if parts.len() < 6 {
            return Err(bad());
        }
        let dir = &parts[..parts.len() - 1];
        let n = dir.len();
        if !dir[n - 1].eq_ignore_ascii_case("win64") || !dir[n - 2].eq_ignore_ascii_case("bin") {
            return Err(bad());
        }
        let join = |parts: &[&str]| parts.join(&sep.to_string());
        let cwd = join(dir);
        let game_dir = join(&dir[..n - 2]);
        let app = join(&dir[..n - 3]);
        if game_dir.is_empty() || app.is_empty() {
            return Err(bad());
        }
        Ok(Layout {
            sep,
            exe: format!("{cwd}{sep}resourcecompiler.exe"),
            cwd,
            content_root: format!("{app}{sep}content"),
            log_dir: format!("{app}{sep}cs2mcp_jobs"),
            log_dir_rel: "cs2mcp_jobs".to_string(),
            game_dir,
        })
    }

    /// The job logs on a native install: `<localappdata>\cs2mcpbridge\jobs` when there is no
    /// container `Z:\app` (the renders' rule, `ops::render::resolve_renders_dir`). The default
    /// `<app>\cs2mcp_jobs` is the container's app dir, but natively `<app>` is the user's Steam
    /// install, which nothing of ours may write into (crates/cs2-native README). There is no
    /// app-relative form then (`log_dir_rel` empty, the reply's `log.relative` too): a server
    /// on the same host reads `log.windows` as it is.
    #[must_use]
    pub fn with_native_log_dir(mut self, z_app_exists: bool, localappdata: Option<&str>) -> Layout {
        if z_app_exists {
            return self;
        }
        if let Some(local) = localappdata.filter(|v| !v.is_empty()) {
            let sep = self.sep;
            self.log_dir = format!("{}{sep}cs2mcpbridge{sep}jobs", local.trim_end_matches(['\\', '/']));
            self.log_dir_rel = String::new();
        }
        self
    }

    fn join_rel(&self, root: &str, rel: &str) -> String {
        let sep = self.sep.to_string();
        format!("{root}{sep}{}", rel.replace('/', &sep))
    }
}

/// What a [`Host`] is asked to start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnSpec {
    /// The executable (also `argv[0]`).
    pub exe: String,
    /// Working directory.
    pub cwd: String,
    /// The whole argument vector, `argv[0]` included.
    pub argv: Vec<String>,
    /// Where stdout and stderr go (the host creates the file and its directory).
    pub log_path: String,
}

/// A started process tree. Dropping it must not leave the tree running.
pub trait Process: Send {
    /// The OS process id.
    fn pid(&self) -> u32;
    /// The exit code once the main process has exited, `None` while it runs.
    fn exit_code(&mut self) -> Option<i64>;
    /// End the whole tree. Idempotent.
    fn terminate(&mut self);
}

/// Why a [`Host::spawn`] failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnError {
    /// This host cannot guarantee the containment (no orphan may be left): answered
    /// `-32017 PrerequisiteFailed` with this text as `blockedOn`.
    Prerequisite(String),
    /// Anything else: answered `-32603 InternalError`.
    Failed(String),
}

/// The platform: layout discovery, a file check and the spawner.
pub trait Host: Send + Sync + 'static {
    /// The layout, or the `blockedOn` text when jobs cannot run here.
    fn layout(&self) -> Result<Layout, String>;
    /// Whether `path` is an existing regular file.
    fn is_file(&self, path: &str) -> bool;
    /// Start the process.
    fn spawn(&self, spec: &SpawnSpec) -> Result<Box<dyn Process>, SpawnError>;
}

/// The production host.
pub struct LiveHost;

impl Host for LiveHost {
    fn layout(&self) -> Result<Layout, String> {
        #[cfg(windows)]
        {
            let exe = s2sdk::module::host_executable()
                .ok_or_else(|| "job: the host executable path is unavailable".to_string())?;
            let local = std::env::var("LOCALAPPDATA").ok();
            Layout::from_host_exe(&exe, '\\')
                .map(|l| l.with_native_log_dir(std::path::Path::new("Z:\\app").is_dir(), local.as_deref()))
        }
        #[cfg(not(windows))]
        {
            Err(WINDOWS_ONLY.to_string())
        }
    }

    fn is_file(&self, path: &str) -> bool {
        std::fs::metadata(path).is_ok_and(|m| m.is_file())
    }

    fn spawn(&self, spec: &SpawnSpec) -> Result<Box<dyn Process>, SpawnError> {
        #[cfg(windows)]
        {
            win::spawn(spec)
        }
        #[cfg(not(windows))]
        {
            let _ = spec;
            Err(SpawnError::Prerequisite(WINDOWS_ONLY.to_string()))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------------------------

/// A parsed, shape-checked `job.start` request (the input is not yet checked against the
/// file system).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartRequest {
    /// Content-relative input path, `/`-separated.
    pub input: String,
    /// Add `-game <gamedir>\csgo`.
    pub game: bool,
    /// The allowlisted flag tokens, in the caller's order.
    pub flags: Vec<String>,
    /// The deadline.
    pub timeout: Duration,
    /// The caller's own correlation token (`clientRef`, optional), echoed by `job.status` so a
    /// caller whose `job.start` reply was lost can find exactly its job. 1..=64 bytes of
    /// `[A-Za-z0-9_.-]`; the bridge does not interpret it or check it for uniqueness.
    pub client_ref: Option<String>,
}

/// The longest `clientRef`.
pub const MAX_CLIENT_REF_LEN: usize = 64;

/// Parse the `job.start` params. `Err` is the `-32602` reason.
pub fn parse_start(params: Option<&str>) -> Result<StartRequest, String> {
    let value: Value = match params {
        None => json!({}),
        Some(text) => serde_json::from_str(text).map_err(|_| "params is not valid JSON".to_string())?,
    };
    let obj = value.as_object().ok_or("params must be an object")?;
    match obj.get("kind").and_then(Value::as_str) {
        Some(KIND) => {}
        Some(_) => return Err(format!("kind must be \"{KIND}\"")),
        None => return Err(format!("kind is required (\"{KIND}\")")),
    }
    let input = obj
        .get("input")
        .and_then(Value::as_str)
        .ok_or("input is required (a content-relative csgo_addons/<addon>/... path)")?
        .to_string();
    let game = match obj.get("game") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("game must be a boolean".to_string()),
    };
    let flags = match obj.get("flags") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            let mut tokens = Vec::with_capacity(items.len());
            for item in items {
                tokens.push(item.as_str().ok_or("flags must be an array of strings")?.to_string());
            }
            parse_flags(&tokens)?
        }
        Some(_) => return Err("flags must be an array of strings".to_string()),
    };
    let timeout_ms = match obj.get("timeoutMs") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_MS,
        Some(v) => v.as_u64().ok_or("timeoutMs must be a non-negative integer")?,
    };
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(format!("timeoutMs must be {MIN_TIMEOUT_MS}..={MAX_TIMEOUT_MS}"));
    }
    let client_ref = match obj.get("clientRef") {
        None | Some(Value::Null) => None,
        Some(Value::String(s))
            if !s.is_empty()
                && s.len() <= MAX_CLIENT_REF_LEN
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')) =>
        {
            Some(s.clone())
        }
        Some(_) => return Err(format!("clientRef must be 1..={MAX_CLIENT_REF_LEN} bytes of [A-Za-z0-9_.-]")),
    };
    Ok(StartRequest { input, game, flags, timeout: Duration::from_millis(timeout_ms), client_ref })
}

/// Flags that take no value. The five map stages (`-world -phys -vis -bakelighting -nav`) are
/// what hammer-mcp's `compile_map` sends for `novis` and `stages` (m3.16: the first four were
/// missing); hammer-mcp has a test that every flag it emits is in this list.
pub const PLAIN_FLAGS: &[&str] = &[
    "-world",
    "-phys",
    "-vis",
    "-bakelighting",
    "-nav",
    "-fshallow",
    "-quiet",
    "-unbufferedio",
    "-noassert",
    "-nop4",
    "-f",
];
/// Flags that take one decimal value, with their inclusive range.
pub const NUMERIC_FLAGS: &[(&str, u32, u32)] = &[("-maxtextureres", 1, 16384), ("-threads", 1, 64)];

/// A caller-supplied token echoed in an error: at most 64 characters.
fn short(token: &str) -> String {
    let mut out: String = token.chars().take(64).collect();
    if token.chars().count() > 64 {
        out.push('~');
    }
    out
}

/// Check a flag token list against the allowlist. Value flags are two tokens (`"-threads"`,
/// `"8"`); a repeated flag, an unknown flag and an out-of-range or non-decimal value are
/// refused. Returns the tokens unchanged.
pub fn parse_flags(tokens: &[String]) -> Result<Vec<String>, String> {
    let mut seen: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let flag = tokens[i].as_str();
        let shown = short(flag);
        if seen.contains(&flag) {
            return Err(format!("flag {shown} is repeated"));
        }
        if PLAIN_FLAGS.contains(&flag) {
            i += 1;
        } else if let Some(&(_, lo, hi)) = NUMERIC_FLAGS.iter().find(|(name, ..)| *name == flag) {
            let value = tokens.get(i + 1).ok_or_else(|| format!("flag {shown} needs a value"))?;
            let ok = !value.is_empty()
                && value.len() <= 6
                && value.bytes().all(|b| b.is_ascii_digit())
                && value.parse::<u32>().is_ok_and(|n| (lo..=hi).contains(&n));
            if !ok {
                return Err(format!("flag {shown} needs an integer {lo}..={hi}"));
            }
            i += 2;
        } else {
            return Err(format!("flag {shown:?} is not allowed"));
        }
        seen.push(flag);
    }
    Ok(tokens.to_vec())
}

/// Extensions the compiler is allowed to be pointed at.
const INPUT_EXTENSIONS: &[&str] = &[
    "vmap", "vmdl", "vpcf", "vsndevts", "vmat", "vtex", "vsnd", "vsmart", "vanim", "vmks", "js",
];

/// Check a content-relative input path: `csgo_addons/<addon>/<...>/<file>.<ext>`, `/`
/// separated, segments of `[A-Za-z0-9_.-]` (never `.` or `..`), an allowlisted extension, at
/// most [`MAX_INPUT_LEN`] bytes. No backslash, drive, UNC or absolute spelling gets through.
pub fn validate_input(input: &str) -> Result<(), String> {
    if input.is_empty() || input.len() > MAX_INPUT_LEN {
        return Err(format!("input must be 1..={MAX_INPUT_LEN} bytes"));
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
            return Err(format!("input segment {:?} is not allowed (letters, digits, _ . - only; no .. or empty)", short(segment)));
        }
    }
    let file = segments[segments.len() - 1];
    let ext = file.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase());
    if !ext.is_some_and(|ext| INPUT_EXTENSIONS.contains(&ext.as_str())) {
        return Err(format!("input extension must be one of: {}", INPUT_EXTENSIONS.join(" ")));
    }
    Ok(())
}

/// The compiler's argv (`argv[0]` included): flags as given, `-game <gamedir>\csgo` when
/// `game`, and `-i <absolute input>` last.
#[must_use]
pub fn build_argv(layout: &Layout, request: &StartRequest) -> Vec<String> {
    let mut argv = vec![layout.exe.clone()];
    argv.extend(request.flags.iter().cloned());
    if request.game {
        argv.push("-game".to_string());
        argv.push(format!("{}{}csgo", layout.game_dir, layout.sep));
    }
    argv.push("-i".to_string());
    argv.push(layout.join_rel(&layout.content_root, &request.input));
    argv
}

// ---------------------------------------------------------------------------------------------
// The log scanner
// ---------------------------------------------------------------------------------------------

/// The compiler's closing counts: ` OK: 19 compiled, 0 failed, 1 skipped, 2m:19s`.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    /// Compiled inputs.
    pub compiled: u64,
    /// Failed inputs.
    pub failed: u64,
    /// Skipped inputs (0 when the line carries none).
    pub skipped: u64,
    /// The elapsed time the compiler printed, when it parsed.
    pub seconds: Option<f64>,
}

/// Parse one log line as the OK line (leading blanks tolerated).
#[must_use]
pub fn parse_ok_line(line: &str) -> Option<Summary> {
    let rest = line.trim_start_matches([' ', '\t']).strip_prefix("OK: ")?;
    let mut parts = rest.trim_end().split(", ");
    let compiled = parts.next()?.strip_suffix(" compiled")?.parse().ok()?;
    let failed = parts.next()?.strip_suffix(" failed")?.parse().ok()?;
    let mut skipped = 0;
    let mut seconds = None;
    for part in parts {
        if let Some(n) = part.strip_suffix(" skipped") {
            skipped = n.parse().ok()?;
        } else {
            seconds = parse_duration(part);
        }
    }
    Some(Summary { compiled, failed, skipped, seconds })
}

/// `2m:19s`, `19s`, `1h:2m:3.5s` -> seconds.
fn parse_duration(text: &str) -> Option<f64> {
    let mut total = 0.0;
    for token in text.split(':') {
        let token = token.trim();
        let (n, unit) = token.split_at(token.len().checked_sub(1)?);
        let n: f64 = n.parse().ok()?;
        total += n * match unit {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            _ => return None,
        };
    }
    Some(total)
}

/// Incremental reader of the compiler's output: bytes in (any split, any encoding), complete
/// lines out. Non-UTF-8 bytes become U+FFFD and never fail a read.
#[derive(Debug, Default)]
pub struct LogScanner {
    partial: Vec<u8>,
    previous: String,
    /// `[FAIL] ...` lines (a bare `[FAIL]` carries the previous non-empty line).
    pub fail_lines: Vec<String>,
    /// The last [`TAIL_LINES`] lines.
    pub tail: std::collections::VecDeque<String>,
    /// The last OK line seen.
    pub ok: Option<Summary>,
    /// A `FATAL ERROR` line was seen (the line is also in [`LogScanner::fail_lines`]).
    pub fatal: bool,
}

impl LogScanner {
    /// Feed the next chunk of the log.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        while let Some(at) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=at).collect();
            self.line(&line[..line.len() - 1]);
        }
        // A runaway line without a newline must not grow without bound.
        if self.partial.len() > 64 * 1024 {
            let line = std::mem::take(&mut self.partial);
            self.line(&line);
        }
    }

    /// The log ended: the unterminated last line counts as a line.
    pub fn finish(&mut self) {
        if !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            self.line(&line);
        }
    }

    fn line(&mut self, raw: &[u8]) {
        let text = String::from_utf8_lossy(raw);
        let text = text.trim_end_matches('\r');
        let trimmed = text.trim();
        if let Some(summary) = parse_ok_line(text) {
            self.ok = Some(summary);
        }
        if text.contains("FATAL ERROR") {
            self.fatal = true;
            if self.fail_lines.len() < MAX_FAIL_LINES {
                self.fail_lines.push(cut(trimmed));
            }
        }
        if let Some(rest) = trimmed.strip_prefix("[FAIL]") {
            let entry = if rest.trim().is_empty() && !self.previous.is_empty() {
                format!("[FAIL] {}", self.previous)
            } else {
                trimmed.to_string()
            };
            if self.fail_lines.len() < MAX_FAIL_LINES {
                self.fail_lines.push(cut(&entry));
            }
        } else if !trimmed.is_empty() {
            self.previous = trimmed.to_string();
        }
        if self.tail.len() == TAIL_LINES {
            self.tail.pop_front();
        }
        self.tail.push_back(cut(text));
    }
}

fn cut(text: &str) -> String {
    text.chars().take(MAX_LINE_CHARS).collect()
}

// ---------------------------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------------------------

/// A job's lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    /// The process is running.
    Running,
    /// The OK line with `failed == 0`.
    Ok,
    /// The OK line with `failed > 0`, or the process exited without one.
    Failed,
    /// `job.cancel` or bridge shutdown.
    Cancelled,
    /// The deadline passed.
    Timeout,
}

impl State {
    /// The wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Ok => "ok",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
            State::Timeout => "timeout",
        }
    }
}

type SharedProcess = Arc<Mutex<Box<dyn Process>>>;

struct Job {
    id: String,
    state: State,
    started: Instant,
    started_unix_ms: u64,
    ended: Option<Instant>,
    exit_code: Option<i64>,
    summary: Option<Summary>,
    fail_lines: Vec<String>,
    fatal: bool,
    tail: Vec<String>,
    log_windows: String,
    log_relative: String,
    /// The request's content-relative `input`, echoed by `job.status` so a caller whose
    /// `job.start` reply was lost can find its job in the list.
    input: String,
    /// The request's `clientRef`, echoed by `job.status` (`null` when none was given).
    client_ref: Option<String>,
    command: Vec<String>,
    pid: u32,
    process: Option<SharedProcess>,
}

impl Job {
    fn log_json(&self) -> Value {
        // a native install's log has no app-relative form (`Layout::with_native_log_dir`)
        let relative = (!self.log_relative.is_empty()).then_some(&self.log_relative);
        json!({ "windows": self.log_windows, "relative": relative })
    }

    fn elapsed_ms(&self) -> u64 {
        let end = self.ended.unwrap_or_else(Instant::now);
        u64::try_from(end.saturating_duration_since(self.started).as_millis()).unwrap_or(u64::MAX)
    }

    fn terminate(&self) {
        if let Some(process) = &self.process {
            lock(process).terminate();
        }
    }

    /// The single running -> finished transition. `false` when it already happened (a cancel
    /// that won the race stays cancelled).
    fn finish(&mut self, state: State) -> bool {
        if self.state != State::Running {
            return false;
        }
        self.terminate();
        self.state = state;
        self.ended = Some(Instant::now());
        self.process = None;
        true
    }
}

#[derive(Default)]
struct Inner {
    jobs: Vec<Job>,
    next: u64,
    closed: bool,
    /// A `job.start` is between its reservation and its insertion (it spawns outside the lock).
    starting: bool,
    watchers: Vec<JoinHandle<()>>,
}

struct Shared {
    inner: Mutex<Inner>,
    wake: Condvar,
}

fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The job table: at most one running job, [`MAX_FINISHED`] finished ones, a watcher thread
/// per running job.
pub struct JobTable {
    shared: Arc<Shared>,
    host: Arc<dyn Host>,
    poll: Duration,
    fatal_grace: Duration,
    nonce: String,
}

/// The production table.
pub fn table() -> &'static JobTable {
    static TABLE: LazyLock<JobTable> = LazyLock::new(|| JobTable::new(Arc::new(LiveHost), POLL));
    &TABLE
}

/// `(id, kind, state)` of every job, oldest first - `control.status`'s `jobs` member.
#[must_use]
pub fn summaries() -> Vec<(String, &'static str, &'static str)> {
    table().summaries()
}

/// Terminate running jobs and join the watchers within `deadline`; `true` when every watcher
/// is joined. Called by the runtime's stop path.
pub fn shutdown(deadline: Duration) -> bool {
    table().shutdown(deadline)
}

/// Re-open the table after a [`shutdown`] (the runtime's `init`).
pub fn reopen() {
    table().reopen();
}

impl JobTable {
    /// A table over `host`, polling each running job's log every `poll`.
    #[must_use]
    pub fn new(host: Arc<dyn Host>, poll: Duration) -> JobTable {
        JobTable {
            shared: Arc::new(Shared { inner: Mutex::new(Inner::default()), wake: Condvar::new() }),
            host,
            poll,
            fatal_grace: FATAL_GRACE,
            nonce: base36(unix_ms()),
        }
    }

    /// The same table with another FATAL grace (tests).
    #[must_use]
    pub fn with_fatal_grace(mut self, grace: Duration) -> JobTable {
        self.fatal_grace = grace;
        self
    }

    /// The same table with a fixed load nonce (tests).
    #[must_use]
    pub fn with_nonce(mut self, nonce: &str) -> JobTable {
        self.nonce = nonce.to_string();
        self
    }

    /// The load nonce every job id of this table carries: `j<N>-<nonce>`.
    #[must_use]
    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    /// Route one of the three rows.
    pub fn handle(&self, method: &str, params: Option<&str>) -> Reply {
        match method {
            START => self.start(params),
            STATUS => self.status(params),
            CANCEL => self.cancel(params),
            _ => Err((-32601, "MethodNotFound", "{}".to_string())),
        }
    }

    /// `job.start`.
    pub fn start(&self, params: Option<&str>) -> Reply {
        let layout = self.host.layout().map_err(|blocked| prerequisite(&blocked))?;
        let request = parse_start(params).map_err(|reason| invalid(&reason))?;
        self.start_request(&layout, &request)
    }

    /// `job.start` over an already parsed request (tests use this to pick a short deadline).
    pub fn start_request(&self, layout: &Layout, request: &StartRequest) -> Reply {
        validate_input(&request.input).map_err(|reason| invalid(&reason))?;
        let argv = build_argv(layout, request);
        let input_abs = argv.last().cloned().unwrap_or_default();
        if !self.host.is_file(&input_abs) {
            return Err(invalid("input does not exist under the content root"));
        }
        // Reserve the one slot under the lock; the file system and the process are touched
        // outside it (a slow disk or CreateProcessW must not block `control.status`).
        let (id, finished) = {
            let mut inner = lock(&self.shared.inner);
            if inner.closed {
                return Err(prerequisite("job: bridge is shutting down"));
            }
            if let Some(running) = inner.jobs.iter().find(|j| j.state == State::Running) {
                return Err(prerequisite(&format!("job: {} running", running.id)));
            }
            if inner.starting {
                return Err(prerequisite("job: another job is starting"));
            }
            inner.starting = true;
            inner.next += 1;
            let (done, live): (Vec<_>, Vec<_>) =
                std::mem::take(&mut inner.watchers).into_iter().partition(JoinHandle::is_finished);
            inner.watchers = live;
            (format!("j{}-{}", inner.next, self.nonce), done)
        };
        for handle in finished {
            let _ = handle.join();
        }
        let mut reservation = Reservation { shared: &self.shared, armed: true };
        let reply = self.launch(layout, request, argv, id, &mut reservation);
        drop(reservation);
        reply
    }

    fn launch(
        &self,
        layout: &Layout,
        request: &StartRequest,
        argv: Vec<String>,
        id: String,
        reservation: &mut Reservation<'_>,
    ) -> Reply {
        if !self.host.is_file(&layout.exe) {
            return Err(prerequisite(&format!("job: resourcecompiler.exe not found at {}", layout.exe)));
        }
        let started_unix_ms = unix_ms();
        let file = format!("{id}.log");
        let log_windows = format!("{}{}{file}", layout.log_dir, layout.sep);
        let log_relative = if layout.log_dir_rel.is_empty() {
            String::new()
        } else {
            format!("{}/{file}", layout.log_dir_rel)
        };
        let spec = SpawnSpec {
            exe: layout.exe.clone(),
            cwd: layout.cwd.clone(),
            argv: argv.clone(),
            log_path: log_windows.clone(),
        };
        let process = self.host.spawn(&spec).map_err(|error| match error {
            SpawnError::Prerequisite(blocked) => prerequisite(&blocked),
            SpawnError::Failed(reason) => internal(&format!("spawn failed: {reason}")),
        })?;
        let pid = process.pid();
        let process: SharedProcess = Arc::new(Mutex::new(process));
        let started = Instant::now();
        let job = Job {
            id: id.clone(),
            state: State::Running,
            started,
            started_unix_ms,
            ended: None,
            exit_code: None,
            summary: None,
            fail_lines: Vec::new(),
            fatal: false,
            tail: Vec::new(),
            log_windows,
            log_relative,
            input: request.input.clone(),
            client_ref: request.client_ref.clone(),
            command: argv,
            pid,
            process: Some(Arc::clone(&process)),
        };
        let reply = json!({
            "jobId": job.id,
            "state": "running",
            "pid": pid,
            "log": job.log_json(),
            "command": job.command,
            "startedAtMs": started_unix_ms,
        })
        .to_string();
        let watcher = Watcher {
            shared: Arc::clone(&self.shared),
            id: id.clone(),
            process: Arc::clone(&process),
            log_path: spec.log_path,
            deadline: started + request.timeout,
            poll: self.poll,
            fatal_grace: self.fatal_grace,
        };
        let evicted = {
            let mut inner = lock(&self.shared.inner);
            inner.starting = false;
            reservation.armed = false;
            if inner.closed {
                // A shutdown ran while this job was being spawned: it did not see it, so end it
                // here (the tree dies with the process handle's job object).
                lock(&process).terminate();
                return Err(prerequisite("job: bridge is shutting down"));
            }
            inner.jobs.push(job);
            evict(&mut inner)
        };
        remove_logs(&evicted);
        match std::thread::Builder::new().name("cs2mcp-job".to_string()).spawn(move || watcher.run()) {
            Ok(handle) => lock(&self.shared.inner).watchers.push(handle),
            Err(error) => {
                let mut inner = lock(&self.shared.inner);
                if let Some(job) = inner.jobs.iter_mut().find(|j| j.id == id) {
                    job.finish(State::Failed);
                }
                return Err(internal(&format!("watcher thread failed: {error}")));
            }
        }
        Ok(reply)
    }

    /// `job.status`.
    pub fn status(&self, params: Option<&str>) -> Reply {
        let id = job_id_param(params, false)?;
        let inner = lock(&self.shared.inner);
        match id {
            None => {
                let jobs: Vec<Value> = inner
                    .jobs
                    .iter()
                    .map(|j| {
                        json!({
                            "jobId": j.id, "kind": KIND, "state": j.state.as_str(), "input": j.input, "clientRef": j.client_ref,
                            "elapsedMs": j.elapsed_ms(), "startedAtMs": j.started_unix_ms,
                            "log": j.log_json(),
                        })
                    })
                    .collect();
                Ok(json!({ "jobs": jobs }).to_string())
            }
            Some(id) => {
                let job = inner.jobs.iter().find(|j| j.id == id).ok_or_else(|| invalid("unknown jobId"))?;
                Ok(status_json(job).to_string())
            }
        }
    }

    /// `job.cancel`.
    pub fn cancel(&self, params: Option<&str>) -> Reply {
        let id = job_id_param(params, true)?.unwrap_or_default();
        let mut inner = lock(&self.shared.inner);
        let job = inner.jobs.iter_mut().find(|j| j.id == id).ok_or_else(|| invalid("unknown jobId"))?;
        let cancelled = job.finish(State::Cancelled);
        let state = job.state.as_str();
        let evicted = evict(&mut inner);
        drop(inner);
        remove_logs(&evicted);
        if cancelled {
            self.shared.wake.notify_all();
        }
        Ok(json!({ "cancelled": cancelled, "state": state }).to_string())
    }

    /// `(id, kind, state)` of every job, oldest first.
    #[must_use]
    pub fn summaries(&self) -> Vec<(String, &'static str, &'static str)> {
        lock(&self.shared.inner).jobs.iter().map(|j| (j.id.clone(), KIND, j.state.as_str())).collect()
    }

    /// Terminate running jobs (state `cancelled`), refuse new ones, and join the watchers
    /// within `deadline`. `true` when every watcher is joined; unjoined handles stay for a
    /// retry.
    pub fn shutdown(&self, deadline: Duration) -> bool {
        let handles = {
            let mut inner = lock(&self.shared.inner);
            inner.closed = true;
            for job in &mut inner.jobs {
                job.finish(State::Cancelled);
            }
            std::mem::take(&mut inner.watchers)
        };
        self.shared.wake.notify_all();
        let end = Instant::now() + deadline;
        let mut left = Vec::new();
        for handle in handles {
            while !handle.is_finished() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(2));
            }
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                left.push(handle);
            }
        }
        let all = left.is_empty();
        lock(&self.shared.inner).watchers.extend(left);
        all
    }

    /// Accept starts again after a [`JobTable::shutdown`].
    pub fn reopen(&self) {
        lock(&self.shared.inner).closed = false;
    }
}

fn job_id_param(params: Option<&str>, required: bool) -> Result<Option<String>, (i64, &'static str, String)> {
    let value: Value = match params {
        None => json!({}),
        Some(text) => serde_json::from_str(text).map_err(|_| invalid("params is not valid JSON"))?,
    };
    let obj = value.as_object().ok_or_else(|| invalid("params must be an object"))?;
    match obj.get("jobId") {
        Some(Value::String(id)) => Ok(Some(id.clone())),
        None | Some(Value::Null) if !required => Ok(None),
        _ => Err(invalid("jobId is required (a string)")),
    }
}

fn status_json(job: &Job) -> Value {
    let mut out = json!({
        "jobId": job.id,
        "kind": KIND,
        "state": job.state.as_str(),
        "input": job.input,
        "clientRef": job.client_ref,
        "pid": job.pid,
        "elapsedMs": job.elapsed_ms(),
        "failLines": job.fail_lines,
        "fatal": job.fatal,
        "logTail": job.tail.join("\n"),
        "log": job.log_json(),
        "startedAtMs": job.started_unix_ms,
    });
    let map = out.as_object_mut().expect("object literal");
    if let Some(code) = job.exit_code {
        map.insert("exitCode".to_string(), json!(code));
    }
    if let Some(s) = &job.summary {
        map.insert(
            "summary".to_string(),
            json!({ "compiled": s.compiled, "failed": s.failed, "skipped": s.skipped, "seconds": s.seconds }),
        );
    }
    out
}

/// The "starting" slot of a `job.start`: released on drop unless the job was inserted.
struct Reservation<'a> {
    shared: &'a Shared,
    armed: bool,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.armed {
            lock(&self.shared.inner).starting = false;
        }
    }
}

/// Keep at most [`MAX_FINISHED`] finished jobs: drop the oldest finished ones. Returns the
/// evicted jobs' log paths for [`remove_logs`] (run it after the lock is released).
fn evict(inner: &mut Inner) -> Vec<String> {
    let mut logs = Vec::new();
    while inner.jobs.iter().filter(|j| j.state != State::Running).count() > MAX_FINISHED {
        match inner.jobs.iter().position(|j| j.state != State::Running) {
            Some(at) => logs.push(inner.jobs.remove(at).log_windows),
            None => break,
        }
    }
    logs
}

/// Best-effort deletion of evicted jobs' log files.
fn remove_logs(paths: &[String]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

// ---------------------------------------------------------------------------------------------
// The watcher
// ---------------------------------------------------------------------------------------------

struct Watcher {
    shared: Arc<Shared>,
    id: String,
    process: SharedProcess,
    log_path: String,
    deadline: Instant,
    poll: Duration,
    fatal_grace: Duration,
}

impl Watcher {
    fn run(self) {
        let mut scanner = LogScanner::default();
        let mut offset = 0u64;
        let mut fatal_since: Option<Instant> = None;
        loop {
            // Exit first, log second: when the process is gone the reads below are the whole
            // log. Read to EOF (a read is capped, so one read can stop short of the OK line).
            let exit = lock(&self.process).exit_code();
            loop {
                let bytes = read_from(&self.log_path, &mut offset);
                if bytes.is_empty() {
                    break;
                }
                scanner.feed(&bytes);
            }
            if exit.is_some() {
                scanner.finish();
            }
            if scanner.fatal && fatal_since.is_none() {
                fatal_since = Some(Instant::now());
            }
            let mut inner = lock(&self.shared.inner);
            let Some(job) = inner.jobs.iter_mut().find(|j| j.id == self.id) else { return };
            if job.state != State::Running {
                return; // cancelled, or the shutdown took it
            }
            job.fail_lines.clone_from(&scanner.fail_lines);
            job.fatal = scanner.fatal;
            job.tail = scanner.tail.iter().cloned().collect();
            let outcome = if let Some(summary) = scanner.ok.clone() {
                let state = if summary.failed > 0 { State::Failed } else { State::Ok };
                job.exit_code = exit;
                job.summary = Some(summary);
                Some(state)
            } else if let Some(code) = exit {
                job.exit_code = Some(code);
                Some(State::Failed)
            } else if fatal_since.is_some_and(|at| at.elapsed() >= self.fatal_grace) {
                // The compiler printed FATAL ERROR and (under Wine) lingers without an OK line.
                Some(State::Failed)
            } else if Instant::now() >= self.deadline {
                Some(State::Timeout)
            } else {
                None
            };
            if let Some(state) = outcome {
                job.finish(state);
                let evicted = evict(&mut inner);
                drop(inner);
                remove_logs(&evicted);
                return;
            }
            let _ = self.shared.wake.wait_timeout(inner, self.poll);
        }
    }
}

/// The bytes appended to `path` since `offset` (advanced). A missing file or a read error is an
/// empty read: the compiler may not have created the log yet.
fn read_from(path: &str, offset: &mut u64) -> Vec<u8> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else { return Vec::new() };
    if file.seek(SeekFrom::Start(*offset)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.take(8 * 1024 * 1024).read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    *offset += bytes.len() as u64;
    bytes
}

// ---------------------------------------------------------------------------------------------
// Windows: CreateProcessW into a Job Object
// ---------------------------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;

    use windows_sys::Win32::Foundation::{CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess, GetExitCodeProcess,
        InitializeProcThreadAttributeList, ResumeThread, TerminateProcess,
        UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
        EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    };

    use super::{Process, SpawnError, SpawnSpec};

    /// `FILE_SHARE_READ | FILE_SHARE_WRITE`: the bridge reads the log while the child writes it.
    const SHARE_READ_WRITE: u32 = 1 | 2;

    /// A started compiler and the Job Object holding its tree. Dropping closes both handles; the
    /// job's `KILL_ON_JOB_CLOSE` ends whatever is left.
    struct WinProcess {
        job: HANDLE,
        process: HANDLE,
        pid: u32,
    }

    // SAFETY: the handles are plain kernel object handles, usable from any thread; the struct
    // is only ever driven through `&mut self` behind a mutex.
    unsafe impl Send for WinProcess {}

    impl Process for WinProcess {
        fn pid(&self) -> u32 {
            self.pid
        }

        fn exit_code(&mut self) -> Option<i64> {
            // SAFETY: `process` is a live process handle owned by `self`.
            unsafe {
                if WaitForSingleObject(self.process, 0) != WAIT_OBJECT_0 {
                    return None;
                }
                let mut code = 0u32;
                if GetExitCodeProcess(self.process, &mut code) == 0 {
                    return None;
                }
                Some(i64::from(code))
            }
        }

        fn terminate(&mut self) {
            // SAFETY: both handles are live and owned by `self`.
            unsafe {
                TerminateJobObject(self.job, 1);
            }
        }
    }

    impl Drop for WinProcess {
        fn drop(&mut self) {
            // SAFETY: closing the handles this struct owns, once.
            unsafe {
                CloseHandle(self.process);
                CloseHandle(self.job);
            }
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect()
    }

    /// The `CommandLineToArgvW`-compatible quoting of one argument.
    pub(super) fn quote_arg(arg: &str) -> String {
        if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
            return arg.to_string();
        }
        let mut out = String::from("\"");
        let mut backslashes = 0usize;
        for c in arg.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    out.push_str(&"\\".repeat(backslashes * 2 + 1));
                    out.push('"');
                    backslashes = 0;
                }
                _ => {
                    out.push_str(&"\\".repeat(backslashes));
                    out.push(c);
                    backslashes = 0;
                }
            }
        }
        out.push_str(&"\\".repeat(backslashes * 2));
        out.push('"');
        out
    }

    /// An inheritable duplicate of `handle` (the parent's own handle keeps its inherit bit
    /// off). Closed on drop.
    struct Dup(HANDLE);

    impl Dup {
        fn of(handle: HANDLE) -> Result<Dup, String> {
            let mut out: HANDLE = std::ptr::null_mut();
            // SAFETY: `handle` is a live handle of this process; `out` is writable.
            let ok = unsafe {
                let me = GetCurrentProcess();
                DuplicateHandle(me, handle, me, &mut out, 0, 1, DUPLICATE_SAME_ACCESS)
            };
            if ok == 0 {
                return Err(format!("DuplicateHandle failed: {}", std::io::Error::last_os_error()));
            }
            Ok(Dup(out))
        }
    }

    impl Drop for Dup {
        fn drop(&mut self) {
            // SAFETY: the duplicate this struct owns, closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// A process-thread attribute list holding one `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
    struct AttributeList {
        // u64 words: the list wants pointer alignment.
        buf: Vec<u64>,
    }

    impl AttributeList {
        /// `handles` must outlive the list (the attribute stores the pointer).
        fn with_handle_list(handles: &[HANDLE]) -> Result<AttributeList, String> {
            let mut size = 0usize;
            // SAFETY: the documented size query: a null list and a writable size.
            unsafe {
                InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
            }
            if size == 0 {
                return Err(format!(
                    "InitializeProcThreadAttributeList size query failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut list = AttributeList { buf: vec![0u64; size.div_ceil(8)] };
            // SAFETY: `buf` is at least `size` bytes and 8-aligned.
            if unsafe { InitializeProcThreadAttributeList(list.ptr(), 1, 0, &mut size) } == 0 {
                list.buf.clear(); // not initialised: Drop must not delete it
                return Err(format!(
                    "InitializeProcThreadAttributeList failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            // SAFETY: the list is initialised; `handles` outlives it (caller contract).
            let ok = unsafe {
                UpdateProcThreadAttribute(
                    list.ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr().cast(),
                    std::mem::size_of_val(handles),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            if ok == 0 {
                return Err(format!("UpdateProcThreadAttribute failed: {}", std::io::Error::last_os_error()));
            }
            Ok(list)
        }

        fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.buf.as_mut_ptr().cast()
        }
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            if !self.buf.is_empty() {
                // SAFETY: an initialised list, deleted once.
                unsafe { DeleteProcThreadAttributeList(self.buf.as_mut_ptr().cast()) };
            }
        }
    }

    pub(super) fn spawn(spec: &SpawnSpec) -> Result<Box<dyn super::Process>, SpawnError> {
        let failed = |text: String| SpawnError::Failed(text);
        if let Some(dir) = std::path::Path::new(&spec.log_path).parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| failed(format!("cannot create {}: {e}", dir.display())))?;
        }
        let log = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(SHARE_READ_WRITE)
            .open(&spec.log_path)
            .map_err(|e| failed(format!("cannot create {}: {e}", spec.log_path)))?;
        let nul = std::fs::OpenOptions::new()
            .read(true)
            .open("NUL")
            .map_err(|e| failed(format!("cannot open NUL: {e}")))?;
        // The child inherits exactly these two duplicates (the handle list below); the parent's
        // own handles stay non-inheritable and nothing else of cs2.exe leaks into the tree.
        let log_dup = Dup::of(log.as_raw_handle() as HANDLE).map_err(failed)?;
        let nul_dup = Dup::of(nul.as_raw_handle() as HANDLE).map_err(failed)?;
        let inherited = [log_dup.0, nul_dup.0];
        let mut attributes = AttributeList::with_handle_list(&inherited).map_err(failed)?;

        // SAFETY: a zeroed STARTUPINFOEXW / PROCESS_INFORMATION is the documented initial
        // state; `cb` is the extended size, the attribute list and the duplicates outlive
        // CreateProcessW.
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = nul_dup.0;
        startup.StartupInfo.hStdOutput = log_dup.0;
        startup.StartupInfo.hStdError = log_dup.0;
        startup.lpAttributeList = attributes.ptr();
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        let command_line = spec.argv.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ");
        let mut command_line = wide(&command_line);
        let exe = wide(&spec.exe);
        let cwd = wide(&spec.cwd);

        // SAFETY: every pointer is a NUL-terminated buffer that outlives the call (the command
        // line is mutable, as the API requires); the job object is created before the process
        // so that a failure leaves nothing running.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(failed(format!("CreateJobObjectW failed: {}", std::io::Error::last_os_error())));
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                let error = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(failed(format!("SetInformationJobObject failed: {error}")));
            }
            if CreateProcessW(
                exe.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1, // bInheritHandles: restricted to the handle list above
                CREATE_SUSPENDED | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
                std::ptr::null(),
                cwd.as_ptr(),
                std::ptr::from_ref(&startup.StartupInfo),
                &mut info,
            ) == 0
            {
                let error = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(failed(format!("CreateProcessW failed: {error}")));
            }
            if AssignProcessToJobObject(job, info.hProcess) == 0 {
                // Fail closed: without the job object there is no tree-kill guarantee.
                let error = std::io::Error::last_os_error();
                TerminateProcess(info.hProcess, 1);
                CloseHandle(info.hThread);
                CloseHandle(info.hProcess);
                CloseHandle(job);
                return Err(SpawnError::Prerequisite(format!("job: AssignProcessToJobObject failed: {error}")));
            }
            if ResumeThread(info.hThread) == u32::MAX {
                let error = std::io::Error::last_os_error();
                TerminateJobObject(job, 1);
                CloseHandle(info.hThread);
                CloseHandle(info.hProcess);
                CloseHandle(job);
                return Err(failed(format!("ResumeThread failed: {error}")));
            }
            CloseHandle(info.hThread);
            Ok(Box::new(WinProcess { job, process: info.hProcess, pid: info.dwProcessId }))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::quote_arg;

        #[test]
        fn quoting_follows_the_argv_rules() {
            assert_eq!(quote_arg("-nav"), "-nav");
            assert_eq!(quote_arg("Z:\\app\\a b\\x"), "\"Z:\\app\\a b\\x\"");
            assert_eq!(quote_arg("a b\\"), "\"a b\\\\\"");
            assert_eq!(quote_arg("q\"x"), "\"q\\\"x\"");
            assert_eq!(quote_arg(""), "\"\"");
        }
    }
}
