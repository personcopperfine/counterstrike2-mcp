//! `compile_map`: compile a Hammer map headless — the proven direct `resourcecompiler` route
//! (`~/.tmp/defuse-poc/compile_map.sh`, proven live 2026-10-13; NEVER Hammer's map.compile/F9
//! dialog, whose data-dependent modals block the tools main thread).
//!
//! ## Route (every line from the proven recipe, flags included)
//!
//! `podman exec <tools container> bash -c 'cd /app && ./entrypoint.sh resourcecompiler.exe
//! -threads 8 -fshallow -maxtextureres 256 -nop4 -quiet -unbufferedio -noassert [-nav]
//! -i /app/content/csgo_addons/<addon>/<path>'`, output `.vpk` at
//! `game/csgo_addons/<addon>/<path minus .vmap>.vpk` (the compiler maps content → game itself, no
//! copy step; unlike the model/particle file routes there is **no `-game`** — the recipe proven
//! live leaves it out and the addon paths resolve from the tree layout).
//!
//! ## Quirks the recipe encodes (kept)
//!
//! * The Wine process LINGERS after printing ` OK: N compiled, 0 failed, M skipped, Tm:Ts` (the
//!   line is CRLF with a leading space, `target/compile-defuse_poc/rc.log`), so a plain exec never
//!   returns: the job runs `podman exec` **in the background** (stdout/stderr appended to the log
//!   file, `DockerRuntime::exec_in_background`), and a 500 ms poll watches the log for the
//!   `^[[:space:]]*OK: [0-9]+ compiled` line. When it appears the exec is SIGKILLed (the recipe's
//!   observed exit-137 teardown — the exit status is worthless), the container-side cleanup kills
//!   THIS job's process tree (see below), and the vpk presence + the parsed summary decide the
//!   verdict. A process that exits *without* the line is a failure — but the log is read once
//!   more at the exit, so a clean exit that raced this poll's read by microseconds still shows
//!   the line instead of a false `failed`.
//!   on the timeout path is still not seen live). Instead the compile's
//!   `bash -c` wrapper exports `CS2MCP_COMPILE_JOB=<tag>` before itself (Wine children inherit
//!   the environment), and the cleanup kills only the `/proc` entries whose environment carries
//!   one of THIS job's tags (SIGTERM, one second of grace, then SIGKILL; the recipe's teardown
//!   wording). If nothing matches, NOTHING is killed and the job's `cleanup` report says so.
//! * Transient `crun: error opening file /run/user/…/status: No such file or directory` exec
//!   failures are retried exactly once (HANDOFF.md quirk).
//! * Deadline: 1500 s default (fresh compiles took 2-7 min, vrad3 lighting being the slow part);
//!   on a timeout the exec is killed the same way and the job ends `timeout`.
//! * Log: `<repo>/target/compile-<name-dashed>/rc-<jobId>.log` — the recipe's
//!   directory with ONE LOG PER JOB, so a new start never deletes or overwrites a file an older
//!   job's poll view points at (`maps/a-b.vmap` and `maps/a/b.vmap` share the dash-collapsed
//!   directory name).
//!
//! ## Where the output lands (the vpk)
//!
//! The authoritative vpk location for a sub-directory map is unverified live. [`vpk_candidates`]
//! derives BOTH spellings — the compiler's own content→game shape (`maps/sub/x.vmap` →
//! `maps/sub/x.vpk`) and the recipe script's flattened `maps/${MAPNAME##*/}.vpk`
//! (`compile_map.sh`'s `OUT`). The job records every candidate's mtime at start; the end verdict
//! and every poll report the candidate whose mtime showed the refresh (`vpkPath` + `vpkForm`),
//! and when neither advanced, both (`vpkCandidates`). `run_map` resolves the SAME candidates
//! through the same helper, so the two tools cannot disagree.
//!
//! ## Shape
//!
//! A map compile takes minutes, so the start returns a **background job** at once
//! (`{jobId, state:"running", log}`) and `compile_map {jobId}` polls it (state
//! `running|ok|failed|timeout`, elapsed, the OK summary, vpk path + mtime-advanced, log tail,
//! failLines, the source-vmap-moved flag, the cleanup report). Jobs live in
//! [`HammerState::compile_jobs`] (bounded at 8); one compile at a time per server — the
//! check-and-insert is ONE store operation under the store's own mutex ([`CompileJobs::start`]),
//! so the server-wide write gate stays free for counted writes and the compile never holds it.
//! The spawned run task is SUPERVISED: a panic in it (a poisoned lock, a runaway invariant)
//! closes the job `failed` instead of leaving it `Running` forever, and a job that outlives its
//! deadline + a reap grace is reaped the same way when anything asks the store.
//!
//! The target map can also be IN the engine (`run_map` + a recompile on top of the copy it has
//! mounted): when the engine's `game.local` (or, without it, this server's own load memory)
//! says this map is in game, the compile is refused with an `run_map {unload:true}` hint (the
//! compiler would rewrite the vpk under the running engine). A differently-mounted engine is
//! not a refusal, and the bare-stem mounted name is not a refusal when a top-level source map
//! could own it (see `mounted_refusal`).
//!
//! A compile can also outlive the server that started it: `main` runs [`shutdown_cleanup`] on
//! stdin EOF, SIGTERM and SIGINT, which ends the tagged tree of EVERY job left without a cleanup
//! report (`Running` or not; bounded at [`SHUTDOWN_KILL_BUDGET`], batched into one kill script
//! per container) — without it the container-side compiler keeps running with nobody watching
//! it, and a start that finds a tagged compile tree in the container which this server's
//! store does not know (the case a server kill still leaves behind) reports it and REFUSES
//! rather than stacking a second compiler on top: the foreign tag is named, and nothing is
//! auto-killed, because the job behind it is unknowable from here. A tag whose value this
//! server DOES know is ended with the same tagged kill.
//!
//! The tool compiles the SAVED file, so when the target map is the active document it is refused
//! (`-32016 UnsavedChanges`, the file-route wording) while the document is dirty, dirty-unknown,
//! or this server has unsaved edits counted for it — `save_map` first. `mode:"nav"` runs the
//! `-nav` extra-flags pass that builds ONLY nav + the map resource from the existing vpk, so it
//! is refused when that vpk does not exist yet. Mutability is `Writer` (it writes the vpk), like
//! `model_compile`; it declares no bridge methods (the bridge is consulted only to pick the
//! default map and to compare the target with the active document).
use crate::state::HammerState;
use crate::vmdl::ContentPaths;
use crate::tools::compile::{
    combine, mtime_advanced, mtime_of, parse_compile_output, tail, CompileSummary, CompileTarget,
};
use crate::tools::live_common::{read_active_document, require_document};
use cs2_mcp_common::runtime::docker::ExecHandle;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// Deadline default and cap (`timeoutMs`): fresh compiles ran 2-7 min (vrad3 lighting the slow
/// part), so the recipe's 1500 s default stands; the cap is a generous hour for a huge map.
pub const MAP_COMPILE_TIMEOUT_MS: u64 = 1_500_000;
pub const MAP_COMPILE_MAX_MS: u64 = 3_600_000;
/// Log poll interval (bounded; every poll is one file read + one process-exit probe).
pub const MAP_COMPILE_POLL: Duration = Duration::from_millis(500);
/// Job history kept in [`HammerState::compile_jobs`] (older finished entries evicted first).
pub const MAX_JOBS: usize = 8;
/// Log lines kept in the poll result's `logTail`.
const LOG_TAIL_LINES: usize = 40;
/// A `Running` job this much past its deadline is DEAD (M4): its watcher never closed it (a lost
/// or panicked task). The store reaps these into `failed` before anyone reads running jobs — a
/// dead job can no longer refuse every later start until the server restarts.
pub const DEAD_GRACE: Duration = Duration::from_secs(120);
/// The tagged-tree kill's own exec budget (the script SIGTERMs, waits 1 s, then SIGKILLs).
const LINGER_KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// The shutdown cleanup's TOTAL budget: the process is going away, so a hung podman must not hold
/// the exit (`main` runs this after the transport closed; the kill script itself takes ~1 s).
pub const SHUTDOWN_KILL_BUDGET: Duration = Duration::from_secs(5);
/// How long the shutdown waits for `job.start` requests in flight, BEFORE the cancels (which then
/// get their own [`SHUTDOWN_KILL_BUDGET`]: a slow start reply must not eat the cancels' time).
pub const SHUTDOWN_START_WAIT: Duration = Duration::from_secs(3);
/// The orphan scan's own exec budget (one `/proc` walk in the container).
const TAGGED_SCAN_TIMEOUT: Duration = Duration::from_secs(5);

// -------------------------------------------------------------------------------------------
// Mode, jobs
// -------------------------------------------------------------------------------------------

/// The map compiler stages that have a proven command-line flag in this repo, in the compiler's
/// own order (the recipe's `-world -phys -vis -bakelighting -nav`; `source2-mcp-research.md`
/// quotes Hammer's own line with the same five). The step table inside resourcecompiler has more
/// rows (`lod gridnav sareverb sapaths sacustomdata`, `idabins/docs/modules/resourcecompiler.md`)
/// but no flag spelling for them is shown anywhere, so they are NOT accepted here.
pub const STAGES: [&str; 5] = ["world", "phys", "vis", "bakelighting", "nav"];

/// A subset of [`STAGES`] as a bit mask (bit i = `STAGES[i]`); always normalised to compiler order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageSet(u8);

impl StageSet {
    pub const WORLD: u8 = 1;
    pub const fn from_bits(bits: u8) -> StageSet {
        StageSet(bits)
    }
    pub fn contains(self, stage: &str) -> bool {
        STAGES.iter().position(|s| *s == stage).is_some_and(|i| self.0 & (1 << i) != 0)
    }
    /// The stage names in compiler order.
    pub fn names(self) -> Vec<&'static str> {
        STAGES
            .iter()
            .enumerate()
            .filter(|(i, _)| self.0 & (1 << i) != 0)
            .map(|(_, s)| *s)
            .collect()
    }
    fn flags(self) -> Vec<String> {
        self.names().iter().map(|s| format!("-{s}")).collect()
    }
    /// Parse `stages: [..]`: any subset, any order, duplicates folded; unknown names are
    /// InvalidParams with `didYouMean`.
    fn parse(value: &Value) -> Result<StageSet, ToolError> {
        let list = value
            .as_array()
            .filter(|l| !l.is_empty())
            .ok_or_else(|| ToolError::invalid_arguments("stages must be a non-empty array of stage names"))?;
        let mut bits = 0u8;
        for item in list {
            let name = item.as_str().ok_or_else(|| {
                ToolError::invalid_arguments(format!("stages entries must be strings, got {item}"))
            })?;
            let lowered = name.to_ascii_lowercase();
            match STAGES.iter().position(|s| *s == lowered) {
                Some(i) => bits |= 1 << i,
                None => {
                    let suggestion = did_you_mean(name);
                    return Err(ToolError::invalid_arguments(format!(
                        "unknown stage {name:?}; the stages are {STAGES:?}{}",
                        suggestion.map(|s| format!(" (did you mean {s:?}?)")).unwrap_or_default()
                    ))
                    .with_data(json!({"didYouMean": suggestion, "stages": STAGES, "got": name})));
                }
            }
        }
        Ok(StageSet(bits))
    }
}

/// The closest stage name by edit distance (<= 2, or a prefix/substring relation), if any.
fn did_you_mean(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    STAGES
        .iter()
        .map(|s| (edit_distance(&lower, s), *s))
        .filter(|(d, s)| *d <= 2 || s.contains(lower.as_str()) || lower.contains(*s))
        .min_by_key(|(d, _)| *d)
        .map(|(_, s)| s)
}

pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i];
        for j in 1..=b.len() {
            let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            cur.push(sub.min(prev[j] + 1).min(cur[j - 1] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// `mode` presets (named shortcuts for stage lists) or an explicit `stages` list: `"full"` (no
/// stage flags: the compiler's default full compile), `"nav"` (`-nav` only; the world comes from
/// the existing vpk), `"novis"` (`-world -phys -bakelighting -nav`), or `Stages` (the caller's
/// own subset). Any list WITHOUT `world` builds on an existing vpk, so a nonexistent one is
/// refused before anything is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileMode {
    Full,
    Nav,
    /// An open map's VIS is most of a full compile (408 of 576 s, live 2026-10-21) and buys
    /// nothing while iterating; this mode took 171 s on the same map. Release builds use `full`.
    NoVis,
    Stages(StageSet),
}

impl CompileMode {
    /// `mode` / `stages` from the arguments (mutually exclusive).
    fn arg(args: &Value) -> Result<CompileMode, ToolError> {
        let stages = args.get("stages").filter(|v| !v.is_null());
        let mode = args.get("mode").filter(|v| !v.is_null());
        if let (Some(_), Some(_)) = (stages, mode) {
            return Err(ToolError::invalid_arguments(
                "mode and stages are mutually exclusive: mode is a named preset of the stage list",
            ));
        }
        if let Some(stages) = stages {
            return Ok(CompileMode::Stages(StageSet::parse(stages)?));
        }
        match mode {
            None => Ok(CompileMode::Full),
            Some(other) => match other.as_str() {
                Some("full") => Ok(CompileMode::Full),
                Some("nav") => Ok(CompileMode::Nav),
                Some("novis") => Ok(CompileMode::NoVis),
                _ => Err(ToolError::invalid_arguments(format!(
                    "mode must be \"full\", \"nav\" or \"novis\", got {other}"
                ))),
            },
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CompileMode::Full => "full",
            CompileMode::Nav => "nav",
            CompileMode::NoVis => "novis",
            CompileMode::Stages(_) => "stages",
        }
    }

    /// The explicit stage set, `None` for `full` (no flags: the compiler's default).
    pub fn stage_set(self) -> Option<StageSet> {
        match self {
            CompileMode::Full => None,
            CompileMode::Nav => Some(StageSet(1 << 4)),
            CompileMode::NoVis => Some(StageSet(0b1_1011)),
            CompileMode::Stages(set) => Some(set),
        }
    }

    /// The resolved stage list reported in the result (compiler order). `full` passes no stage
    /// flags, so the compiler runs its default set: all of [`STAGES`] are reported for it.
    pub fn stage_names(self) -> Vec<&'static str> {
        self.stage_set().map_or_else(|| STAGES.to_vec(), StageSet::names)
    }

    /// Where the resolved stage list comes from: `explicit` (the caller's `stages`), `preset`
    /// (nav/novis), or `compiler default` (full: no stage flag is sent at all).
    pub fn stages_from(self) -> &'static str {
        match self {
            CompileMode::Full => "compiler default",
            CompileMode::Stages(_) => "explicit",
            _ => "preset",
        }
    }

    /// Does this compile build on an existing vpk (no `world` stage)?
    pub fn needs_existing_vpk(self) -> bool {
        self.stage_set().is_some_and(|set| !set.contains("world"))
    }

    /// The stage tokens this mode adds after the recipe's base flags.
    fn stage_flags(self) -> Vec<String> {
        self.stage_set().map(StageSet::flags).unwrap_or_default()
    }

    /// The recipe's `EXTRA_FLAGS` for this mode.
    fn flags(self) -> String {
        self.stage_flags().iter().map(|f| format!(" {f}")).collect()
    }
}

/// One compiled-output candidate of a job (M8): the authoritative vpk location for a
/// sub-directory map is unverified live, so a job carries BOTH derived spellings and the
/// refreshed one decides. `before` is `Some` once the job recorded the mtime its refresh is
/// judged from (the helper leaves it `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VpkCandidate {
    pub name: String,
    pub relative: String,
    pub form: &'static str,
    pub path: PathBuf,
    pub before: Option<SystemTime>,
}

/// One map-compile run: created by the start, advanced to a final state by its background task
/// (`run_compile`), read by every `compile_map {jobId}` poll.
#[derive(Debug, Clone)]
pub struct CompileJob {
    pub id: String,
    /// The unique-per-process tag value this job exports into the container's `/proc/*/environ`
    /// ([`tag_value`]; N1). The short `id` is only the poll's handle: two hammer-mcp processes
    /// can run `c1` at the same time, and the orphan check classifies a live tag as this
    /// server's own ONLY on an exact match with one of [`CompileJobs::tags`].
    pub tag: String,
    pub state: JobState,
    pub mode: CompileMode,
    /// The addon-content-relative map path (`maps/…vmap`) as the tool accepted it.
    pub path: String,
    /// The addon the job was started for (a later `set_addon` / document switch must not
    /// re-point this job's paths).
    pub addon: String,
    pub container: String,
    pub log: PathBuf,
    /// The compiled output candidates (M8), in start order; each carries the mtime its refresh
    /// is judged from.
    pub vpk: Vec<VpkCandidate>,
    /// The source .vmap's mtime at start (L2): the poll flags a job whose map moved under it
    /// (e.g. a `save_map` while the compile runs — the compiler may reread it for vrad3).
    pub vmap_before: Option<SystemTime>,
    pub started: Instant,
    pub deadline: Duration,
    /// 1 (or 2 after the retried transient `crun` exec).
    pub attempts: u32,
    /// Parsed from the log once the run ended.
    pub summary: Option<CompileSummary>,
    /// The verbatim `OK:` line, when the log shows one.
    pub ok_line: Option<String>,
    /// Why the run is `failed`/`timeout` (or the every-count-was-a-skip note on `ok`).
    pub reason: Option<String>,
    /// What the tagged-tree cleanup (M3) matched and killed — `null`-ish details when the
    /// cleanup could not run; the poll result carries it under `cleanup`.
    pub cleanup: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Ok,
    Failed,
    Timeout,
    /// Ended by `compile_map {jobId, cancel:true}` / `job_cancel` (either route).
    Cancelled,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Ok => "ok",
            JobState::Failed => "failed",
            JobState::Timeout => "timeout",
            JobState::Cancelled => "cancelled",
        }
    }
}

/// The job store (in [`HammerState::compile_jobs`]): bounded, insertion-ordered, one running at a
/// time. A `std` Mutex is enough: no entry is ever held across an await (the background task
/// updates it field-wise through `update`), and the one-compile-at-a-time check-and-insert is a
/// single operation on this lock (`start`) — the server-wide write gate is NOT part of it.
#[derive(Debug, Default)]
pub struct CompileJobs {
    inner: Mutex<JobsInner>,
    /// Set by [`shutdown_cleanup`] BEFORE the jobs are read (review-2 N7): a start that races the
    /// shutdown (between the running list and the kills) would otherwise insert+spawn its job
    /// after the store was read and be orphaned by the exit.
    shutting_down: std::sync::atomic::AtomicBool,
    /// Bridge `job.start` requests in flight ([`StartGuard`]): the shutdown waits for them (their
    /// reply may be a spawned job nobody knows yet) before it reads the jobs to cancel.
    starts_in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// A bridge `job.start` in flight; drop it once the new job's id is recorded.
#[derive(Debug)]
pub struct StartGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for StartGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Why a start was refused: the running job's id (poll it), or the server going away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartRefusal {
    Running(String),
    ShuttingDown,
}

#[derive(Debug, Default)]
struct JobsInner {
    jobs: HashMap<String, CompileJob>,
    /// Insertion order (eviction and the "known jobs" listing read oldest first).
    order: Vec<String>,
    next: u64,
    /// The jobs that run on the bridge (`job.start`) instead of in the podman container: the
    /// `c<N>` id -> its bridge side. Such a job has NO run task and NO tag: the bridge owns the
    /// process, the poll folds `job.status` into the entry.
    bridge: HashMap<String, BridgeRef>,
    /// `c<N>` ids of podman-route jobs a cancel was requested for; their run task ends the exec
    /// and the tagged tree at its next poll and closes the job `cancelled`.
    cancel: std::collections::HashSet<String>,
    /// Bridge job ids (`j<N>`) started by `compile_asset`: not compile_map jobs, but the same rule
    /// holds (a job does not outlive this server), so the shutdown cancels them too.
    assets: Vec<String>,
    /// `clientRef`s of `job.start`s that carry no entry (compile_asset, file-route compiles)
    /// whose job id is not known yet (sent, reply lost or the future dropped): the shutdown and
    /// every `job.status {}` resolve them to the job.
    pending_refs: Vec<String>,
}

/// The bridge side of a `compile_map` job that runs as a bridge job.
#[derive(Debug, Clone, Default)]
pub struct BridgeRef {
    /// `j<N>`; empty until `job.start` answered.
    pub bridge_id: String,
    /// The bridge log as a host path.
    pub log_host: Option<PathBuf>,
    /// The `command` the bridge reported at start.
    pub command: Option<Value>,
    /// The last `job.status` reply (the log tail and elapsed time of the poll view).
    pub last_status: Option<Value>,
    /// The `clientRef` this entry's `job.start` carried: a lost reply is resolved by it and by
    /// nothing else ([`super::bridge_job::adopt`]).
    pub client_ref: String,
    /// No id yet: the start's reply is in flight or was lost, so the job is looked up by
    /// `client_ref`.
    pub adopting: bool,
    /// The reaper closed this entry "outcome unknown" (the bridge could not be asked): a later
    /// terminal `job.status` still overwrites that, the compile may well have finished.
    pub reaped: bool,
    /// A cancel arrived before the id was known; applied the moment the id is.
    pub pending_cancel: bool,
}

impl CompileJobs {
    pub fn new() -> Self {
        CompileJobs {
            inner: Mutex::default(),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            starts_in_flight: std::sync::Arc::default(),
        }
    }

    /// Register a bridge `job.start` about to be sent; `None` while the server is shutting down.
    /// The count goes up BEFORE the flag is read, so a shutdown that sets the flag and then waits
    /// for the count cannot miss a start that got past this check.
    pub fn begin_start(&self) -> Option<StartGuard> {
        self.starts_in_flight.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let guard = StartGuard(self.starts_in_flight.clone());
        if self.is_shutting_down() {
            return None; // the guard drops here
        }
        Some(guard)
    }

    pub fn starts_in_flight(&self) -> usize {
        self.starts_in_flight.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Close the entries the reaper considers lost (what every store read does implicitly).
    pub fn reap(&self) {
        reap_dead(&mut self.lock());
    }

    /// Is a `job.start` request in flight for this entry? (The id-less entry of a request that
    /// has not answered yet must not be given up on.) Any guard alive means "maybe".
    pub fn start_in_flight_for(&self, _id: &str) -> bool {
        self.starts_in_flight() > 0
    }

    /// A bridge job id known to be finished: the shutdown has nothing to cancel for it.
    pub fn forget_asset(&self, bridge_id: &str) {
        self.lock().assets.retain(|known| known != bridge_id);
    }

    /// Block new starts from now on (the shutdown cleanup sets this first, N7). A start that has
    /// inserted its job before this flag flips is still seen by the shutdown's job read — the
    /// flag is checked INSIDE `start` under the store lock, so there is no insert-after-read.
    pub fn begin_shutdown(&self) {
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, JobsInner> {
        self.inner.lock().expect("compile-jobs lock")
    }

    /// The id of the running job, if any. Dead `Running` jobs (M4: running past
    /// deadline + [`DEAD_GRACE`] — a lost watcher) are reaped `failed` first, so a dead job
    /// cannot block starts until the server restarts.
    pub fn running(&self) -> Option<String> {
        let mut inner = self.lock();
        reap_dead(&mut inner);
        inner
            .jobs
            .values()
            .find(|job| job.state == JobState::Running)
            .map(|job| job.id.clone())
    }

    /// The id of the `Running` job compiling `content_path`, if any (review L2): a `save_map` or
    /// `reload_map` of that map while its compile runs changes the file the compiler may reread
    /// for its later passes, so the result carries a note — never a refusal. The comparison is
    /// case-insensitive (both spellings come from the same tool gate, and Windows paths compare
    /// that way).
    pub fn running_for_path(&self, addon: &str, content_path: &str) -> Option<String> {
        let mut inner = self.lock();
        reap_dead(&mut inner);
        inner
            .jobs
            .values()
            .find(|job| {
                job.state == JobState::Running
                    && job.addon.eq_ignore_ascii_case(addon)
                    && job.path.eq_ignore_ascii_case(content_path)
            })
            .map(|job| job.id.clone())
    }

    /// The atomic one-compile-at-a-time start (M4's backstop + L7): a shutting-down flag (N7) and
    /// dead Running jobs are answered first, a live one REFUSES with its id, and a fresh one is
    /// built (its id and per-process tag assigned) and inserted UNDER THE SAME LOCK — two
    /// concurrent starts (rmcp runs one task per request) cannot both pass the check. The `build`
    /// closure runs file-mtime reads only.
    pub fn start(&self, build: impl FnOnce(&str) -> CompileJob) -> Result<String, StartRefusal> {
        self.start_with(build, None)
    }

    /// [`CompileJobs::start`] for a job that runs on the bridge (`bridge` set: the entry is born a
    /// bridge-route job, so no shutdown/reap path ever mistakes it for a podman one).
    pub fn start_with(
        &self,
        build: impl FnOnce(&str) -> CompileJob,
        bridge: Option<BridgeRef>,
    ) -> Result<String, StartRefusal> {
        if self.is_shutting_down() {
            return Err(StartRefusal::ShuttingDown);
        }
        let mut inner = self.lock();
        reap_dead(&mut inner);
        if self.is_shutting_down() {
            // the shutdown may have flipped while this start waited on the store lock (N7)
            return Err(StartRefusal::ShuttingDown);
        }
        if let Some(id) = inner
            .jobs
            .values()
            .find(|job| job.state == JobState::Running)
            .map(|job| job.id.clone())
        {
            return Err(StartRefusal::Running(id));
        }
        inner.next += 1;
        let id = format!("c{}", inner.next);
        let job = build(&id);
        if let Some(bridge) = bridge {
            inner.bridge.insert(id.clone(), bridge);
        }
        accept(&mut inner, job);
        Ok(id)
    }

    /// Drop a job whose start never reached the compiler (the bridge refused `job.start`).
    pub fn remove(&self, id: &str) {
        let mut inner = self.lock();
        inner.order.retain(|known| known != id);
        inner.jobs.remove(id);
        inner.bridge.remove(id);
        inner.cancel.remove(id);
    }

    /// The bridge side of a job, when it runs on the bridge.
    pub fn bridge_ref(&self, id: &str) -> Option<BridgeRef> {
        self.lock().bridge.get(id).cloned()
    }

    pub fn update_bridge(&self, id: &str, f: impl FnOnce(&mut BridgeRef)) {
        if let Some(bridge) = self.lock().bridge.get_mut(id) {
            f(bridge);
        }
    }

    /// The `c<N>` id of the compile_map job that runs as bridge job `bridge_id`.
    pub fn id_for_bridge(&self, bridge_id: &str) -> Option<String> {
        let inner = self.lock();
        inner
            .bridge
            .iter()
            .find(|(_, b)| b.bridge_id == bridge_id)
            .map(|(id, _)| id.clone())
    }

    /// `(c-id, bridge job id)` of every bridge-route job still `Running` in the store.
    pub fn running_bridge_jobs(&self) -> Vec<(String, String)> {
        let inner = self.lock();
        inner
            .order
            .iter()
            .filter(|id| inner.jobs.get(*id).is_some_and(|job| job.state == JobState::Running))
            .filter_map(|id| {
                inner
                    .bridge
                    .get(id)
                    .filter(|b| !b.bridge_id.is_empty())
                    .map(|b| (id.clone(), b.bridge_id.clone()))
            })
            .collect()
    }

    /// `(c-id, bridge job id or "")` of every bridge-route job still `Running`, including those
    /// whose id is not known yet (lost `job.start` reply).
    pub fn running_bridge_entries(&self) -> Vec<(String, String)> {
        let inner = self.lock();
        inner
            .order
            .iter()
            .filter(|id| inner.jobs.get(*id).is_some_and(|job| job.state == JobState::Running))
            .filter_map(|id| inner.bridge.get(id).map(|b| (id.clone(), b.bridge_id.clone())))
            .collect()
    }

    /// Remember a bridge job started by `compile_asset` (the shutdown cancels it).
    pub fn note_asset(&self, bridge_id: &str) {
        let mut inner = self.lock();
        if !inner.assets.iter().any(|known| known == bridge_id) {
            inner.assets.push(bridge_id.to_string());
            // bounded: the bridge keeps 16 finished jobs, so older ids are long gone anyway
            if inner.assets.len() > 64 {
                inner.assets.remove(0);
            }
        }
    }

    pub fn assets(&self) -> Vec<String> {
        self.lock().assets.clone()
    }

    /// Register a `clientRef` placeholder BEFORE its `job.start` is sent.
    pub fn note_ref(&self, client_ref: &str) {
        let mut inner = self.lock();
        inner.pending_refs.push(client_ref.to_string());
        if inner.pending_refs.len() > 64 {
            inner.pending_refs.remove(0);
        }
    }

    /// The placeholder is resolved (the id is known) or the start was refused (nothing spawned).
    pub fn drop_ref(&self, client_ref: &str) {
        self.lock().pending_refs.retain(|known| known != client_ref);
    }

    pub fn pending_refs(&self) -> Vec<String> {
        self.lock().pending_refs.clone()
    }

    /// ONE locked step for a cancel: the bridge id of the entry when it is known, else (the
    /// start window) mark the cancel pending — the start's reply applies it — and answer `None`.
    /// Splitting the read and the mark would let the id arrive in between and lose the cancel.
    pub fn bridge_id_or_pend_cancel(&self, id: &str) -> Option<String> {
        let mut inner = self.lock();
        let bridge = inner.bridge.get_mut(id)?;
        if bridge.bridge_id.is_empty() {
            bridge.pending_cancel = true;
            None
        } else {
            Some(bridge.bridge_id.clone())
        }
    }

    /// Close a `Running` bridge-route entry the bridge could not be asked about for deadline +
    /// [`DEAD_GRACE`] "outcome unknown". Called ONLY from the sync paths that just failed to reach
    /// the bridge: a store read alone must never reap, the compile may have finished unpolled.
    pub fn reap_bridge(&self, id: &str) {
        let mut inner = self.lock();
        let JobsInner { jobs, bridge, .. } = &mut *inner;
        let Some(entry) = bridge.get_mut(id) else { return };
        let Some(job) = jobs.get_mut(id) else { return };
        if job.state != JobState::Running || job.started.elapsed() <= job.deadline + DEAD_GRACE {
            return;
        }
        job.state = JobState::Failed;
        entry.reaped = true;
        job.reason = Some(format!(
            "outcome unknown (bridge unreachable): the bridge could not be asked for this job's final \
             state within the {:.0} s deadline plus the {:.0} s grace — the compile may still have \
             run (the bridge ends its compiler with its own deadline); poll again, check the log and \
             the output",
            job.deadline.as_secs_f64(),
            DEAD_GRACE.as_secs_f64(),
        ));
    }

    /// Ask a podman-route job's run task to end the exec and the tagged tree.
    pub fn request_cancel(&self, id: &str) {
        self.lock().cancel.insert(id.to_string());
    }

    pub fn cancel_requested(&self, id: &str) -> bool {
        self.lock().cancel.contains(id)
    }

    /// Insert a new job. Beyond [`MAX_JOBS`] the oldest **finished** jobs are evicted (at most
    /// one job ever runs, so there is always at least one finished entry to drop).
    pub fn insert(&self, job: CompileJob) {
        let mut inner = self.lock();
        accept(&mut inner, job);
    }

    /// A copy of one job (polls read it).
    pub fn get(&self, id: &str) -> Option<CompileJob> {
        self.lock().jobs.get(id).cloned()
    }

    /// The known job ids, oldest first.
    pub fn ids(&self) -> Vec<String> {
        self.lock().order.clone()
    }

    /// Every job's tag value, oldest first ([`classify_tagged_jobs`]'s "known" list; exact
    /// value comparison, N1).
    pub fn tags(&self) -> Vec<String> {
        let inner = self.lock();
        inner
            .order
            .iter()
            .filter_map(|id| inner.jobs.get(id))
            .map(|job| job.tag.clone())
            .collect()
    }

    /// Every job (any state, oldest first) with no cleanup report yet — exactly the entries the
    /// shutdown cleanup must kill for (review-2 N5): a `Running` job, but also one the
    /// probe-error / exit-fold / supervisor / dead-reaper paths closed `failed` whose tagged
    /// children may still be alive in the container. In-process the next start would end them as
    /// "own"; after the exit nobody can, so they become foreign orphans.
    pub fn jobs_needing_cleanup(&self) -> Vec<CompileJob> {
        let mut inner = self.lock();
        reap_dead(&mut inner);
        inner
            .order
            .iter()
            .filter_map(|id| inner.jobs.get(id))
            // a bridge-route job has no container tree: the bridge owns the process
            .filter(|job| job.cleanup.is_none() && !inner.bridge.contains_key(&job.id))
            .cloned()
            .collect()
    }

    /// In-place update from the background task.
    pub fn update(&self, id: &str, f: impl FnOnce(&mut CompileJob)) {
        if let Some(job) = self.lock().jobs.get_mut(id) {
            f(job);
        }
    }
}

/// Push-insert + bound (shared by [`CompileJobs::start`] and [`CompileJobs::insert`]).
fn accept(inner: &mut JobsInner, job: CompileJob) {
    let id = job.id.clone();
    inner.order.push(id.clone());
    inner.jobs.insert(id, job);
    while inner.order.len() > MAX_JOBS {
        let victim = inner
            .order
            .iter()
            .find(|id| {
                inner
                    .jobs
                    .get(*id)
                    .is_some_and(|job| job.state != JobState::Running)
            })
            .cloned();
        let Some(victim) = victim else { break };
        inner.order.retain(|id| *id != victim);
        inner.jobs.remove(&victim);
        inner.bridge.remove(&victim);
        inner.cancel.remove(&victim);
    }
}

/// The M4 reaper: `Running` jobs past deadline + [`DEAD_GRACE`] become `Failed` with the reason
/// the poll shows (the watcher never closed them; tagged compiler processes may still run).
fn reap_dead(inner: &mut JobsInner) {
    let mut dead: Vec<String> = Vec::new();
    for (id, job) in inner.jobs.iter_mut() {
        // (a bridge-route entry is NOT reaped here: only the sync paths that failed to reach the
        // bridge close it, `CompileJobs::reap_bridge`)
        if job.state == JobState::Running
            && !inner.bridge.contains_key(id)
            && job.started.elapsed() > job.deadline + DEAD_GRACE
        {
            dead.push(id.clone());
        }
    }
    for id in dead {
        let (deadline_secs, tag) = inner
            .jobs
            .get(&id)
            .map(|job| (job.deadline.as_secs_f64(), job.tag.clone()))
            .unwrap_or_default();
        if let Some(job) = inner.jobs.get_mut(&id) {
            job.state = JobState::Failed;
            job.reason = Some(format!(
                "the compile watcher did not close this job within its deadline ({deadline_secs:.0} s) \
                 plus the {:.0} s reap grace — the run task was lost, so this is a reap, not a \
                 compiler verdict; tagged compiler processes ({}) may still \
                 be running in the container (the log shows what they printed)",
                DEAD_GRACE.as_secs_f64(),
                job_tag(&tag),
            ));
        }
    }
}

// -------------------------------------------------------------------------------------------
// Registration + the tool
// -------------------------------------------------------------------------------------------

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "compile_map",
            "Compile a Hammer map headless; starts a background job ({jobId, state}). Wait with waitMs (here, or compile_map {jobId, waitMs:30000}) until done:true; success only if state is ok (running|ok|failed|timeout|cancelled; failLines/logTail say why not). {jobId, cancel:true} ends it. \
             path: maps/<x>.vmap, default the active document. mode (exclusive with stages): \"full\" (default), \"novis\" (all but VIS, fast), \"nav\" (-nav only). \
             stages: subset of world, phys, vis, bakelighting, nav; without world it builds on the existing .vpk. \
             UnsavedChanges if the document is unsaved (save_map first); EngineBusy if the engine is in this map (run_map {unload:true}) or another compile runs.",
            Writer,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "maps/<x>.vmap; default the active document"
                },
                "mode": {
                    "type": "string",
                    "enum": ["full", "nav", "novis"],
                    "description": "full: all stages; nav: -nav only (needs .vpk); novis: all but VIS"
                },
                "stages": {
                    "type": "array",
                    "items": {"type": "string", "enum": STAGES},
                    "minItems": 1,
                    "description": "stage subset; without world needs the existing .vpk. Exclusive with mode"
                },
                "jobId": {
                    "type": "string",
                    "description": "poll this job instead of starting one (no path/mode/stages)"
                },
                "waitMs": super::jobs::wait_ms_schema(),
                "cancel": {
                    "type": "boolean",
                    "description": "with jobId: end the job instead of polling"
                },
                "timeoutMs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAP_COMPILE_MAX_MS,
                    "description": "compile deadline, default 1500000"
                }
            },
            "additionalProperties": false
        })),
        compile_map,
    );
}

/// `compile_map`: `jobId` present -> poll that job; otherwise resolve the target, check the
/// container + the saved file (+ the vpk for `nav`), then start the background job.
pub async fn compile_map(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    if args.get("jobId").is_some() {
        if args.get("path").is_some() || args.get("mode").is_some() || args.get("stages").is_some() {
            return Err(ToolError::invalid_arguments(
                "jobId polls an existing job: pass no path/mode/stages with it",
            ));
        }
        let id = args
            .get("jobId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ToolError::invalid_arguments("jobId must be a non-empty string"))?;
        let deadline = ctx.budget.deadline_ms()?;
        return match args.get("cancel") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => {
                let wait_ms = super::jobs::wait_ms_arg(&args)?;
                super::jobs::wait_for_job(
                    &ctx.budget,
                    wait_ms,
                    super::jobs::WAIT_POLL_INTERVAL,
                    "compile_map",
                    id,
                    |left_ms| poll_job(&state, id, left_ms),
                )
                .await
            }
            Some(Value::Bool(true)) => cancel_job(&state, id, deadline).await,
            Some(other) => Err(ToolError::invalid_arguments(format!(
                "cancel must be a boolean, got {other}"
            ))),
        };
    }
    if args.get("cancel").is_some() {
        return Err(ToolError::invalid_arguments(
            "cancel ends an existing job: pass the jobId with it",
        ));
    }
    // `waitMs` on a start: start the job, then wait for it like `{jobId, waitMs}` does
    let wait = match args.get("waitMs") {
        None | Some(Value::Null) => None,
        Some(_) => Some(super::jobs::wait_ms_arg(&args)?),
    };
    let started = start_compile(state.clone(), ctx.clone(), args).await?;
    let Some(wait_ms) = wait else {
        return Ok(started);
    };
    let Some(id) = started.json.get("jobId").and_then(Value::as_str).map(str::to_string) else {
        return Ok(started);
    };
    let carried: Vec<(&str, Value)> = ["notes", "orphanKills"]
        .into_iter()
        .filter_map(|k| started.json.get(k).map(|v| (k, v.clone())))
        .collect();
    let mut out = super::jobs::wait_for_job(
        &ctx.budget,
        wait_ms,
        super::jobs::WAIT_POLL_INTERVAL,
        "compile_map",
        &id,
        |left_ms| poll_job(&state, &id, left_ms),
    )
    .await?;
    if let Some(map) = out.json.as_object_mut() {
        for (k, v) in carried {
            map.entry(k).or_insert(v);
        }
    }
    Ok(out)
}

/// The start half of `compile_map`: resolve the target, check the container + the saved file,
/// then start the background job.
async fn start_compile(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let mode = CompileMode::arg(&args)?;
    let deadline = compile_deadline(&args)?;
    // The one-compile-at-a-time check-and-insert is ONE operation on the job store's own mutex
    // below (`start`), so this call does NOT take the server-wide write gate (L7): the gate
    // exists to serialise one tool call's bridge write sequence — the compile only reads the
    // active document to pick/gate its target, and its real work runs in a background task.
    let mut target = resolve_target(&state, ctx.budget.deadline_ms()?, &args).await?;

    // The map file itself must exist on the host: the compiler compiles a SAVED .vmap, and a
    // missing one can only be a wrong spelling (the container would see the same /app tree).
    let content = ContentPaths::new(state.app_root(), target.addon.clone());
    let host = content.addon_root().join(&target.content_path);
    if !host.is_file() {
        let variant = super::new_map::case_variant(&content.addon_root(), &target.content_path);
        let error = ToolError::new(
            "AssetNotFound",
            "AssetNotFound",
            format!(
                "{} is not readable under the mapped host addon root ({}): compile_map compiles \
                 the SAVED file (if the map does not exist yet, new_map creates it from a \
                 template; otherwise check the spelling: Windows compares case-insensitively, \
                 this host does not)",
                target.content_path,
                content.addon_root().display()
            ),
        )
        .with_code(-32014)
        .with_data(json!({"path": target.content_path, "reason": "missing_host_file"}))
        .retry_safe(true);
        return Err(match variant {
            // the file exists under another spelling: this host is case-sensitive, Windows is not
            Some(real) => error.did_you_mean([real]),
            None => error.use_instead(
                "new_map",
                "the map file does not exist in any spelling: create it from a template first",
                Some(json!({"path": target.content_path})),
            ),
        });
    }

    // M8: the authoritative output location for a sub-directory map is unverified live, so the
    // job carries BOTH spellings — the compiler's content→game shape and the recipe script's
    // flattened one ([`vpk_candidates`; `run_map` resolves the same list through it]).
    let candidates = vpk_candidates(&content.app_root, &target.addon, &target.content_path)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| {
            ToolError::invalid_arguments(format!(
                "the content root cannot place a compiled .vpk for {}",
                host.display()
            ))
        })?;
    if mode.needs_existing_vpk() && !candidates.iter().any(|c| c.path.is_file()) {
        let both: Vec<String> = candidates
            .iter()
            .map(|c| c.path.display().to_string())
            .collect();
        return Err(ToolError::new(
            "AssetNotFound",
            "AssetNotFound",
            format!(
                "stages {:?} (mode {}) have no `world` stage, so they build on the existing \
                 compiled vpk, and none of the candidate output locations exist yet ({both:?}): \
                 compile the map with mode full (or stages including world) first (or pass the \
                 path of the map whose vpk already exists)",
                mode.stage_names(),
                mode.as_str()
            ),
        )
        .with_code(-32014)
        .with_data(json!({"reason": "no_compiled_vpk", "mode": mode.as_str(), "stages": mode.stage_names(), "candidates": both}))
        .retry_safe(true));
    }

    // The route: a bridge job when the bridge advertises `job.start` (the connection is made
    // lazily, so probing connects first), else the proven podman exec.
    let use_bridge = super::bridge_job::probe(&state).await.available();

    // The tools container (podman route): the explicit override, else discover the running
    // instance (same rule as the model/particle file-route compiles).
    let mut container = if use_bridge {
        String::new()
    } else {
        tools_container(&state).await?
    };

    // M9: a compile while the engine has THIS map loaded would rewrite the .vpk under the
    // mounted reader; refused when the bridge's game.local says so (an `run_map {unload:true}`
    // hint), a note when the method is exposed but the read failed, and no question — no note —
    // when the bridge does not expose the method at all.
    mounted_map_check(&state, ctx.budget.deadline_ms()?, &mut target).await?;

    if use_bridge {
        match start_bridge_compile(&state, &target, mode, deadline, &candidates).await {
            Ok(out) => return Ok(out),
            // a bridge that cannot run compile jobs HERE (Windows-only, compiler missing, job
            // object refused): the podman route runs instead, and the result says so
            Err(error) if super::bridge_job::route_unavailable(&error) => {
                target.notes.push(json!(format!(
                    "the bridge route was unavailable ({}): this compile ran through the podman \
                     route instead",
                    error.message
                )));
                container = tools_container(&state).await?;
            }
            Err(error) => return Err(error),
        }
    }

    // One compile at a time, part 1 (cheap, no podman): a job of THIS server that is still running
    // is refused with its id — and, just as important, is never seen by the orphan scan below as a
    // killable tree.
    if let Some(running) = state.compile_jobs.running() {
        return Err(engine_busy_running(&running));
    }

    // Part 2 (one bounded exec): are tagged compile trees alive in the container that no job of
    // this server owns? The usual case is a server that exited mid-compile: its `CS2MCP_COMPILE_JOB`
    // tree kept running (that is what the shutdown cleanup now prevents) and a second compiler on
    // top of it would race it on the same output. A tag this server DOES know is its own (a
    // finished or reaped job whose tree outlived its verdict) and is ended here — the one case the
    // id being ours makes safe. A foreign tag is reported, never auto-killed, and refuses the
    // start.
    let mut own_orphans: Vec<Value> = Vec::new();
    match scan_tagged_jobs(&state, &container).await {
        Ok(alive) => {
            // a start that raced this one may have inserted a job while the scan ran: its live
            // tree must never be mistaken for an orphan (and the atomic check below would refuse
            // this start anyway), so nothing is killed
            if let Some(running) = state.compile_jobs.running() {
                return Err(engine_busy_running(&running));
            }
            // N1: the comparison is on the full `/proc` tag values, not on the short job ids —
            // two hammer-mcp processes can both run `c1`, and only an exact tag is this
            // server's own.
            let tagged = classify_tagged_jobs(&alive, &state.compile_jobs.tags());
            if !tagged.own.is_empty() {
                for tag in &tagged.own {
                    own_orphans.push(end_the_linger(&state, &container, std::slice::from_ref(tag)).await);
                }
                target.notes.push(json!(format!(
                    "a tagged compiler tree of this server's own job tag(s) {} was still running in \
                     {container}: it was ended with the tagged kill before this start",
                    tagged
                        .own
                        .iter()
                        .map(|tag| job_tag(tag))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            if !tagged.foreign.is_empty() {
                return Err(foreign_compile_job_error(&tagged.foreign));
            }
        }
        Err(error) => target.notes.push(json!(format!(
            "the check for a compile job of another server still running in {container} could not \
             run ({error}): if a compiler tree from an exited server is alive in there, this start \
             races it on the same output"
        ))),
    }

    // The store's atomic start assigns the id AND the per-process tag under the same lock that
    // checks one-at-a-time. The job's log is ONE FILE PER JOB (L3) and can be named inside the
    // closure; the candidates' before-mtimes are read at insert time, as late as anything reads
    // them.
    let map_path = target.content_path.clone();
    let map_host = host.clone();
    let project_root = state.project_root.clone();
    let container_name = container.clone();
    let id = state
        .compile_jobs
        .start(|id| CompileJob {
            id: id.to_string(),
            tag: tag_value(id),
            state: JobState::Running,
            mode,
            path: map_path.clone(),
            addon: target.addon.clone(),
            container: container_name.clone(),
            log: log_path(&project_root, &map_path, id),
            vpk: candidates
                .iter()
                .map(|c| VpkCandidate {
                    before: mtime_of(&c.path),
                    ..c.clone()
                })
                .collect(),
            vmap_before: mtime_of(&map_host),
            started: Instant::now(),
            deadline,
            attempts: 0,
            summary: None,
            ok_line: None,
            reason: None,
            cleanup: None,
        })
        .map_err(start_refusal_error)?;
    // the tag the start gave the job (argv + every cleanup exec carry exactly it, N1)
    let tag = state
        .compile_jobs
        .get(&id)
        .map(|job| job.tag.clone())
        .expect("the fresh job");

    let log = log_path(&state.project_root, &target.content_path, &id);
    // fresh job, fresh log (the recipe's `rm -f "$LOG"`): the per-job name makes the path unique
    // to THIS job, and removal covers the one case where a name repeats — after a server
    // restart, where a retried attempt must append to a file this job owns
    let _ = std::fs::remove_file(&log);

    // the wrapped command exports the job's cleanup tag (M3: the kill targets exactly this tree)
    let argv = compile_argv(&target.addon, &target.content_path, mode, &tag);
    let run_state = state.clone();
    let run_id = id.clone();
    let run_container = container.clone();
    let run_log = log.clone();
    // M4: the spawned task is supervised — a panic in the run task closes the store entry
    // `failed` instead of leaving it `Running` forever (every later start would be refused).
    let task = tokio::spawn(run_compile(
        run_state, run_id, argv, run_container, run_log, deadline,
    ));
    tokio::spawn(supervise_compile(state.clone(), id.clone(), task));

    let chosen = candidates.first().expect("checked non-empty");
    let mut out = Map::new();
    out.insert("jobId".into(), json!(id));
    out.insert("state".into(), json!("running"));
    out.insert("mode".into(), json!(mode.as_str()));
    out.insert("stages".into(), json!(mode.stage_names()));
    out.insert("stageFlags".into(), json!(mode.stage_flags()));
    out.insert("stagesFrom".into(), json!(mode.stages_from()));
    out.insert("path".into(), json!(target.content_path));
    out.insert("vpkPath".into(), json!(chosen.path.display().to_string()));
    out.insert("vpkForm".into(), json!(chosen.form));
    out.insert(
        "vpkCandidates".into(),
        json!(candidates
            .iter()
            .map(|c| json!({"form": c.form, "path": c.path.display().to_string()}))
            .collect::<Vec<_>>()),
    );
    out.insert("log".into(), json!(log.display().to_string()));
    out.insert("deadlineMs".into(), json!(deadline.as_millis() as u64));
    out.insert("container".into(), json!(container));
    out.insert("route".into(), json!("podman"));
    if !own_orphans.is_empty() {
        out.insert("orphanKills".into(), Value::Array(own_orphans));
    }
    out.insert("addon".into(), json!(target.addon));
    out.insert("addonSource".into(), json!(state.addon_selection().1.as_str()));
    if !target.notes.is_empty() {
        out.insert("notes".into(), Value::Array(target.notes));
    }
    Ok(poll_next(Value::Object(out).into(), &id))
}

/// The start refusals as tool errors (the store's one-at-a-time / shutting-down answers).
fn start_refusal_error(refusal: StartRefusal) -> ToolError {
    match refusal {
        StartRefusal::Running(running) => engine_busy_running(&running),
        StartRefusal::ShuttingDown => ToolError::new(
            "EngineBusy",
            "EngineBusy",
            "this server is shutting down (the compile-job cleanup is running): no compile is \
             started",
        )
        .with_data(json!({"reason": "shutting_down"})),
    }
}

/// The recipe's flags for a map compile as the bridge's `flags` tokens.
fn bridge_flags(mode: CompileMode) -> Vec<String> {
    let mut flags: Vec<String> = [
        "-threads", "8", "-fshallow", "-maxtextureres", "256", "-nop4", "-quiet", "-unbufferedio",
        "-noassert",
    ]
    .iter()
    .map(|flag| flag.to_string())
    .collect();
    flags.extend(mode.stage_flags());
    flags
}

/// The bridge route of the start: the same target resolution and gates ran already; the compile is
/// `job.start {kind:"resourcecompiler", input:"csgo_addons/<addon>/<path>", game:false, flags}`.
/// The store entry is created first (atomic one-at-a-time, the id space stays `c<N>`) and is
/// dropped again only when the bridge DEFINITIVELY refuses the start. A start whose reply never
/// arrived (timeout, disconnect) may have spawned the job: the entry stays, id-less, and the job is
/// adopted from `job.status {}` (now, and on every later poll/cancel). The start registers against
/// the shutdown (`StartGuard`), so a shutdown that begins mid-request waits for the reply and the
/// job it carries is cancelled, never orphaned.
async fn start_bridge_compile(
    state: &HammerState,
    target: &MapTarget,
    mode: CompileMode,
    deadline: Duration,
    candidates: &[VpkCandidate],
) -> Result<ToolOutput, ToolError> {
    use super::bridge_job as bj;
    // a bridge job that ended since the last poll must not block this start. (BEFORE this start
    // registers: its own guard would make every id-less entry look like "a start is in flight"
    // and an old one could never be given up.)
    sync_running_bridge_jobs(state).await;
    let Some(guard) = state.compile_jobs.begin_start() else {
        return Err(bj::shutting_down_error());
    };
    if let Some(running) = state.compile_jobs.running() {
        return Err(engine_busy_running(&running));
    }
    let map_path = target.content_path.clone();
    let map_host = ContentPaths::new(state.app_root(), target.addon.clone())
        .addon_root()
        .join(&map_path);
    let input = format!("csgo_addons/{}/{}", target.addon, map_path);
    let client_ref = bj::new_client_ref();
    let id = state
        .compile_jobs
        .start_with(
            |id| CompileJob {
                id: id.to_string(),
                tag: String::new(),
                state: JobState::Running,
                mode,
                path: map_path.clone(),
                addon: target.addon.clone(),
                container: String::new(),
                log: PathBuf::new(),
                vpk: candidates
                    .iter()
                    .map(|c| VpkCandidate {
                        before: mtime_of(&c.path),
                        ..c.clone()
                    })
                    .collect(),
                vmap_before: mtime_of(&map_host),
                started: Instant::now(),
                deadline,
                attempts: 1,
                summary: None,
                ok_line: None,
                reason: None,
                cleanup: None,
            },
            Some(BridgeRef {
                client_ref: client_ref.clone(),
                adopting: true,
                ..BridgeRef::default()
            }),
        )
        .map_err(start_refusal_error)?;
    let mut note: Option<String> = None;
    let reply = match bj::start_raw(
        state,
        &input,
        false,
        &bridge_flags(mode),
        Some(bj::bridge_timeout_ms(deadline)),
        &client_ref,
    )
    .await
    {
        Ok(reply) => Some(reply),
        Err(bj::StartFailure::Refused(error)) => {
            state.compile_jobs.remove(&id);
            return Err(error);
        }
        Err(bj::StartFailure::Lost(error)) => {
            // the reply never came: the job may exist. Look for it now; failing that the entry
            // stays id-less and later polls keep looking.
            match bj::adopt(state, &client_ref).await {
                Ok(Some(found)) => Some(found),
                _ => {
                    note = Some(format!(
                        "the bridge's answer to job.start was lost ({}): the job may be running; it \
                         is looked up on every poll (poll compile_map {{jobId: {id:?}}})",
                        error.message
                    ));
                    None
                }
            }
        }
    };
    if let Some(reply) = &reply {
        if !record_bridge_start(state, &id, reply) {
            state.compile_jobs.remove(&id);
            return Err(ToolError::internal(format!(
                "the bridge's job.start reply carries no jobId: {reply}"
            )));
        }
    }
    drop(guard);
    // a cancel (or the shutdown) that came in during the start window lands now
    settle_start_window(state, &id).await;
    let bridge = state.compile_jobs.bridge_ref(&id).unwrap_or_default();
    let chosen = candidates.first().expect("checked non-empty");
    let mut out = Map::new();
    out.insert("jobId".into(), json!(id));
    out.insert(
        "state".into(),
        json!(state.compile_jobs.get(&id).map_or("running", |j| j.state.as_str())),
    );
    out.insert("route".into(), json!("bridge"));
    out.insert(
        "bridgeJobId".into(),
        if bridge.bridge_id.is_empty() { Value::Null } else { json!(bridge.bridge_id) },
    );
    out.insert("mode".into(), json!(mode.as_str()));
    out.insert("stages".into(), json!(mode.stage_names()));
    out.insert("stageFlags".into(), json!(mode.stage_flags()));
    out.insert("stagesFrom".into(), json!(mode.stages_from()));
    out.insert("path".into(), json!(map_path));
    out.insert("vpkPath".into(), json!(chosen.path.display().to_string()));
    out.insert("vpkForm".into(), json!(chosen.form));
    out.insert(
        "vpkCandidates".into(),
        json!(candidates
            .iter()
            .map(|c| json!({"form": c.form, "path": c.path.display().to_string()}))
            .collect::<Vec<_>>()),
    );
    out.insert(
        "log".into(),
        json!(bridge.log_host.as_ref().map(|p| p.display().to_string())),
    );
    out.insert("command".into(), bridge.command.clone().unwrap_or(Value::Null));
    out.insert("deadlineMs".into(), json!(deadline.as_millis() as u64));
    out.insert("addon".into(), json!(target.addon));
    out.insert("addonSource".into(), json!(state.addon_selection().1.as_str()));
    let mut notes = target.notes.clone();
    notes.extend(note.map(Value::from));
    if !notes.is_empty() {
        out.insert("notes".into(), Value::Array(notes));
    }
    Ok(poll_next(Value::Object(out).into(), &id))
}

/// The step after a compile start: poll the job.
fn poll_next(output: ToolOutput, id: &str) -> ToolOutput {
    output.with_next_args(
        "compile_map",
        "pass waitMs to wait for the job instead of polling (compile_map {jobId, cancel:true} ends it)",
        json!({"jobId": id, "waitMs": 30_000}),
    )
}

/// Record the bridge's start reply (or an adopted `job.status {}` entry) on the entry. `false`
/// when it carries no job id.
fn record_bridge_start(state: &HammerState, id: &str, reply: &Value) -> bool {
    let Some(bridge_id) = reply.get("jobId").and_then(Value::as_str).map(str::to_string) else {
        return false;
    };
    let log_host = super::bridge_job::host_log(state, reply.get("log"));
    state.compile_jobs.update_bridge(id, |bridge| {
        bridge.bridge_id = bridge_id.clone();
        bridge.log_host = log_host.clone();
        bridge.command = reply.get("command").cloned();
        bridge.adopting = false;
    });
    if let Some(log) = &log_host {
        state.compile_jobs.update(id, |job| job.log = log.clone());
    }
    true
}

/// After the bridge id of an entry became known: apply a cancel that was requested while it was
/// not, and cancel it when the server is shutting down.
async fn settle_start_window(state: &HammerState, id: &str) {
    let Some(bridge) = state.compile_jobs.bridge_ref(id).filter(|b| !b.bridge_id.is_empty()) else {
        return;
    };
    let shutting = state.compile_jobs.is_shutting_down();
    if !(bridge.pending_cancel || shutting) {
        return;
    }
    let _ = super::bridge_job::cancel(state, &bridge.bridge_id).await;
    if shutting {
        state.compile_jobs.forget_asset(&bridge.bridge_id);
    }
    // fold the answer in directly (no recursion through the sync path that may have called us)
    if let Ok(status) = super::bridge_job::status(state, &bridge.bridge_id).await {
        apply_bridge_status(state, id, &status);
    }
}

/// The tools container of the podman route: the explicit override, else the running instance.
async fn tools_container(state: &HammerState) -> Result<String, ToolError> {
    super::compile::podman_route_here(None)?;
    match &state.tools_container {
        Some(name) => Ok(name.clone()),
        None => match state.runtime.container_by_label(CompileTarget::ANY_TOOLS.label).await {
            Ok(Some(container)) => Ok(container),
            Ok(None) => Err(super::compile::runtime_unavailable(
                CompileTarget::ANY_TOOLS,
                "no running container carries the label",
            )),
            Err(error) => Err(super::compile::runtime_unavailable(CompileTarget::ANY_TOOLS, error)),
        },
    }
}

/// Fold one `job.status` reply into the store entry of the compile_map job `id` (bridge route):
/// the live view always lands in the entry; a terminal state closes the job exactly once, with the
/// same host-side verdict the podman route reaches (the vpk candidates against their start mtimes).
fn apply_bridge_status(state: &HammerState, id: &str, status: &Value) {
    use super::bridge_job::{fail_lines, state_of, summary_counts};
    state
        .compile_jobs
        .update_bridge(id, |bridge| bridge.last_status = Some(status.clone()));
    let bridge_state = state_of(status);
    if bridge_state == "running" {
        return;
    }
    let Some(job) = state.compile_jobs.get(id) else { return };
    // a reaped "outcome unknown" entry is overwritten by the bridge's real terminal answer
    let reaped = state.compile_jobs.bridge_ref(id).is_some_and(|b| b.reaped);
    if job.state != JobState::Running && !reaped {
        return;
    }
    if reaped {
        state.compile_jobs.update_bridge(id, |bridge| bridge.reaped = false);
    }
    let mut log_tail = status.get("logTail").and_then(Value::as_str).unwrap_or_default().to_string();
    // The terminal status's tail can lack a parsable summary although the compiler printed it
    // (live c2, 2026-10-21: "OK: 29 compiled, 0 failed, 1 skipped, 9m:35s" in the log, the job
    // closed `failed`, "summary could not be parsed"): the job's own log file on the host has the
    // whole run, so an `ok` status without a summary in its tail is judged from that file. The
    // raw `job.status` reply has only `log.relative` (`log.host` is added to the replies shown to
    // the caller), so the path is resolved here too (live 2026-10-29: "OK: 17 compiled, 0 failed"
    // in the log, the job closed `failed` again).
    if bridge_state == "ok" && parse_compile_output(&log_tail).compiled.is_none() {
        let host = status
            .pointer("/log/host")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| super::bridge_job::host_log(state, status.get("log")));
        if let Some(text) = host.and_then(|host| std::fs::read(host).ok()) {
            log_tail = String::from_utf8_lossy(&text).into_owned();
        }
    }
    let log_tail = log_tail.as_str();
    let mut summary = parse_compile_output(log_tail);
    let (compiled, failed, skipped) = summary_counts(status);
    if compiled.is_some() {
        summary.compiled = compiled;
        summary.failed = failed;
        summary.skipped = skipped;
    }
    let lines = fail_lines(status);
    if !lines.is_empty() {
        summary.fail_lines = lines;
    }
    summary.fatal = summary.fatal || super::bridge_job::fatal(status);
    let ok_line = find_ok_line(log_tail);
    let exit = status.get("exitCode").and_then(Value::as_i64);
    let (job_state, reason) = match bridge_state {
        "ok" => ok_verdict(&job.vpk, &summary),
        "failed" => (
            JobState::Failed,
            Some(match (summary.failed, exit) {
                (Some(n), _) if n > 0 => format!(
                    "the compiler reported {n} failure(s) (see failed/failLines): fix and re-run"
                ),
                (_, Some(code)) => format!(
                    "the compiler exited ({code}) without an OK line — the compile failed (the log \
                     tail shows why)"
                ),
                _ => "the compile failed (see failLines and the log tail)".to_string(),
            }),
        ),
        "timeout" => (
            JobState::Timeout,
            Some(format!(
                "the bridge ended the compile at its {:.0} s deadline; the compiler may have been \
                 working — check the log and re-run",
                job.deadline.as_secs_f64()
            )),
        ),
        "cancelled" => (
            JobState::Cancelled,
            Some("the compile was cancelled (job.cancel); the bridge ended its process tree".to_string()),
        ),
        other => (
            JobState::Failed,
            Some(format!("the bridge reported the unknown job state {other:?}")),
        ),
    };
    let summary = (summary.compiled.is_some() || summary.fatal || !summary.fail_lines.is_empty())
        .then_some(summary);
    finish(
        state,
        id,
        job_state,
        summary,
        ok_line,
        reason,
        Some(json!({
            "route": "bridge",
            "note": "the bridge owns the process tree: it ended it with the job (no container cleanup)",
            "exitCode": exit,
        })),
    );
}

/// How long an id-less entry (lost `job.start` reply) is looked for before it is closed `failed`
/// ("no job found"): the bridge answers `job.status {}` at once, so a job that was going to exist
/// does within this.
const ADOPT_GIVE_UP: Duration = Duration::from_secs(30);

/// Look for the job of an id-less bridge entry in `job.status {}` and adopt it. `Ok(true)` once the
/// entry has its id. A list that answers with no match for [`ADOPT_GIVE_UP`] closes the entry
/// `failed` (the start did not spawn anything).
async fn adopt_entry(state: &HammerState, id: &str) -> Result<bool, ToolError> {
    let Some(bridge) = state.compile_jobs.bridge_ref(id) else {
        return Ok(false);
    };
    if !bridge.bridge_id.is_empty() {
        return Ok(true);
    }
    let waited = state.compile_jobs.get(id).map(|j| j.started.elapsed());
    let given_up = waited.is_some_and(|w| w > ADOPT_GIVE_UP) && !state.compile_jobs.start_in_flight_for(id);
    match super::bridge_job::adopt(state, &bridge.client_ref).await {
        Ok(Some(found)) => {
            if record_bridge_start(state, id, &found) {
                settle_start_window(state, id).await;
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Ok(None) => {
            if given_up {
                finish(
                    state,
                    id,
                    JobState::Failed,
                    None,
                    None,
                    Some(
                        "the bridge's answer to job.start was lost and no job carrying this \
                         start's clientRef exists on the bridge: nothing was started — re-run the \
                         compile"
                            .to_string(),
                    ),
                    Some(json!({"route": "bridge", "note": "no job on the bridge"})),
                );
            }
            Ok(false)
        }
        Err(error) => {
            // the bridge could not be asked. Long enough: close the entry — "outcome unknown", the
            // bridge's job object ends the compiler when the bridge dies anyway — so an id-less
            // entry cannot block every compile start (the podman fallback included)
            if given_up {
                give_up_unreachable(state, id);
            }
            Err(error)
        }
    }
}

/// Close an id-less entry whose job.start reply was lost and whose bridge stayed unreachable.
fn give_up_unreachable(state: &HammerState, id: &str) {
    finish(
        state,
        id,
        JobState::Failed,
        None,
        None,
        Some(
            "outcome unknown (bridge unreachable): the answer to job.start was lost and the bridge \
             could not be asked since — the job may or may not have started (the bridge's job \
             object ends the compiler if the bridge died); a later poll that reaches the bridge \
             corrects this"
                .to_string(),
        ),
        Some(json!({"route": "bridge", "note": "bridge unreachable"})),
    );
    state.compile_jobs.update_bridge(id, |bridge| bridge.reaped = true);
}

/// One `job.status` for a bridge-route entry, folded in (an id-less entry is adopted first). An
/// id the bridge no longer knows (a restarted bridge) closes the entry `failed` instead of leaving
/// it running for ever; an unreachable bridge leaves it as it was (the store's reaper closes it
/// "outcome unknown" past its deadline plus grace) and the error is returned.
async fn sync_bridge_job(state: &HammerState, id: &str, bridge_id: &str) -> Result<(), ToolError> {
    sync_bridge_job_within(state, id, bridge_id, super::bridge_job::REQUEST_DEADLINE_MS).await
}

/// [`sync_bridge_job`] with the status request's deadline cut to `deadline_ms`.
async fn sync_bridge_job_within(
    state: &HammerState,
    id: &str,
    bridge_id: &str,
    deadline_ms: u64,
) -> Result<(), ToolError> {
    let bridge_id = if bridge_id.is_empty() {
        match adopt_entry(state, id).await {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(error) => {
                state.compile_jobs.reap_bridge(id);
                return Err(error);
            }
        }
        match state.compile_jobs.bridge_ref(id) {
            Some(bridge) if !bridge.bridge_id.is_empty() => bridge.bridge_id,
            _ => return Ok(()),
        }
    } else {
        bridge_id.to_string()
    };
    match super::bridge_job::status_within(state, &bridge_id, deadline_ms).await {
        Ok(status) => {
            apply_bridge_status(state, id, &status);
            Ok(())
        }
        Err(error)
            if error.name == "InvalidParams"
                && error.data.get("reason").and_then(Value::as_str) == Some("unknown jobId") =>
        {
            finish(
                state,
                id,
                JobState::Failed,
                None,
                None,
                Some(format!(
                    "the bridge no longer knows job {bridge_id} (it was restarted, or evicted the \
                     job): the outcome is unknown — re-run the compile"
                )),
                Some(json!({"route": "bridge", "note": "unknown to the bridge"})),
            );
            Ok(())
        }
        Err(error) => {
            state.compile_jobs.reap_bridge(id);
            Err(error)
        }
    }
}

/// Sync every `Running` bridge-route entry (best effort: errors leave the entry as it was).
async fn sync_running_bridge_jobs(state: &HammerState) {
    for (id, bridge_id) in state.compile_jobs.running_bridge_entries() {
        let _ = sync_bridge_job(state, &id, &bridge_id).await;
    }
}

fn unknown_job(state: &HammerState, id: &str) -> ToolError {
    let known = state.compile_jobs.ids();
    ToolError::invalid_arguments(format!(
        "unknown jobId {id:?}: this server knows {known:?}"
    ))
}

/// How long `cancel` waits for a podman-route job's run task to end the exec and the tagged tree
/// (further bounded by the tool call's own deadline).
const CANCEL_WAIT: Duration = Duration::from_secs(20);

/// The poll: one job's current view. A bridge-route job is asked first (`job.status`), so the view
/// is the live one.
pub(crate) async fn poll_job(
    state: &HammerState,
    id: &str,
    deadline_ms: u64,
) -> Result<ToolOutput, ToolError> {
    let Some(job) = state.compile_jobs.get(id) else {
        return Err(unknown_job(state, id));
    };
    if let Some(bridge) = state.compile_jobs.bridge_ref(id) {
        // a reaped "outcome unknown" entry is asked again: the bridge's terminal answer wins
        if job.state == JobState::Running || bridge.reaped {
            sync_bridge_job_within(state, id, &bridge.bridge_id, deadline_ms).await?;
        }
    }
    let view = render_job(state, id)?;
    let ok = view.get("state").and_then(Value::as_str) == Some(JobState::Ok.as_str());
    let source_changed = view.get("sourceChanged").and_then(Value::as_bool) == Some(true);
    let output: ToolOutput = Value::Object(view).into();
    // `run_map`/`compile_map {path}` resolve the path in the CURRENT addon; the job may belong to
    // another one (the document or set_addon moved since it started): select it first and
    // suggest nothing that would resolve the path in the wrong addon.
    if ok && !job.addon.eq_ignore_ascii_case(&state.addon()) {
        return Ok(output.with_next_args(
            "set_addon",
            &format!(
                "this job ran in addon {:?}, not the current one; select it, then {} {{path: {:?}}}",
                job.addon,
                if source_changed { "compile_map" } else { "run_map" },
                job.path
            ),
            json!({"addon": job.addon}),
        ));
    }
    Ok(match (ok, source_changed) {
        (true, false) => output.with_next_args(
            "run_map",
            "load the compiled map in the tools game to play-test it",
            json!({"path": job.path}),
        ),
        // the .vmap was saved again while this compile ran: the vpk does not have those edits
        (true, true) => output.with_next_args(
            "compile_map",
            "the source map changed while this compile ran; compile again before running it",
            json!({"path": job.path}),
        ),
        _ => output,
    })
}

/// Fold a bridge `job.status` reply the caller already has (`job_status` on the bridge id of a
/// compile_map job) into that job's entry, like a compile_map poll would.
pub(crate) fn fold_bridge_status(state: &HammerState, id: &str, status: &Value) {
    apply_bridge_status(state, id, status);
}

/// `compile_map {jobId, cancel:true}` / `job_cancel {jobId: c<N>}`: end the job on its route and
/// answer its final state with `cancelled` (`false` for a job that was already finished, or that
/// finished before the cancel landed). A cancel that could not be delivered yet (the bridge id of
/// the job is still unknown, or the podman job has not closed within the wait) answers
/// `cancelRequested:true` with the state `running`: it is applied as soon as it can be.
pub(crate) async fn cancel_job(
    state: &HammerState,
    id: &str,
    deadline_ms: u64,
) -> Result<ToolOutput, ToolError> {
    let Some(job) = state.compile_jobs.get(id) else {
        return Err(unknown_job(state, id));
    };
    let mut cancelled = false;
    let mut requested = false;
    if job.state == JobState::Running {
        // the route is a property of the entry, not of whether its bridge id is known yet
        if let Some(bridge) = state.compile_jobs.bridge_ref(id) {
            // the compile may have ended since the last poll; an id-less entry is looked up (a
            // bridge that does not answer just leaves it id-less: the cancel is then remembered)
            if bridge.bridge_id.is_empty() {
                let _ = sync_bridge_job(state, id, "").await;
            } else {
                sync_bridge_job(state, id, &bridge.bridge_id).await?;
            }
            if state.compile_jobs.get(id).is_some_and(|j| j.state == JobState::Running) {
                // one locked step: the known id, or (start window) the cancel marked pending
                match state.compile_jobs.bridge_id_or_pend_cancel(id) {
                    None => requested = true, // remembered, applied when the id arrives
                    Some(bridge_id) => {
                    let reply = super::bridge_job::cancel(state, &bridge_id).await?;
                    cancelled = reply.get("cancelled").and_then(Value::as_bool).unwrap_or(false);
                    // the cancel is done; a status that cannot be read now must not hide that
                    let _ = sync_bridge_job(state, id, &bridge_id).await;
                    }
                }
            }
        } else {
            // podman route: the run task ends the exec and runs the tagged kill at its next poll
            state.compile_jobs.request_cancel(id);
            let wait = CANCEL_WAIT.min(Duration::from_millis(deadline_ms));
            let until = Instant::now() + wait;
            while state.compile_jobs.get(id).is_some_and(|j| j.state == JobState::Running)
                && Instant::now() < until
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            cancelled = state
                .compile_jobs
                .get(id)
                .is_some_and(|j| j.state == JobState::Cancelled);
            requested = state
                .compile_jobs
                .get(id)
                .is_some_and(|j| j.state == JobState::Running);
        }
    }
    let mut view = render_job(state, id)?;
    view.insert("cancelled".into(), json!(cancelled));
    if requested {
        view.insert("cancelRequested".into(), json!(true));
    }
    Ok(Value::Object(view).into())
}

/// The compile_map jobs as the `job_status {}` list.
pub(crate) fn list_jobs(state: &HammerState) -> Vec<Value> {
    state
        .compile_jobs
        .ids()
        .iter()
        .filter_map(|id| state.compile_jobs.get(id))
        .map(|job| {
            let bridge = state.compile_jobs.bridge_ref(&job.id);
            json!({
                "jobId": job.id,
                "kind": "compile_map",
                "state": job.state.as_str(),
                "path": job.path,
                "route": if bridge.is_some() { "bridge" } else { "podman" },
                "bridgeJobId": bridge.map(|b| b.bridge_id),
                "elapsedMs": job.started.elapsed().as_millis() as u64,
                "log": job.log.display().to_string(),
            })
        })
        .collect()
}

/// One job's view (the shape `compile_map {jobId}` answers on either route).
fn render_job(state: &HammerState, id: &str) -> Result<Map<String, Value>, ToolError> {
    let Some(job) = state.compile_jobs.get(id) else {
        return Err(unknown_job(state, id));
    };
    let bridge = state.compile_jobs.bridge_ref(id);
    let status = bridge.as_ref().and_then(|b| b.last_status.as_ref());
    let elapsed_ms = status
        .and_then(|s| s.get("elapsedMs").and_then(Value::as_u64))
        .unwrap_or_else(|| job.started.elapsed().as_millis() as u64);
    let text = match status.and_then(|s| s.get("logTail").and_then(Value::as_str)) {
        Some(tail) => tail.to_string(),
        None => log_text(&job.log),
    };
    let summary = job.summary.clone();
    let mut out = Map::new();
    out.insert("jobId".into(), json!(job.id));
    out.insert("state".into(), json!(job.state.as_str()));
    out.insert("route".into(), json!(if bridge.is_some() { "bridge" } else { "podman" }));
    if let Some(bridge) = &bridge {
        out.insert("bridgeJobId".into(), json!(bridge.bridge_id));
    }
    out.insert("mode".into(), json!(job.mode.as_str()));
    out.insert("stages".into(), json!(job.mode.stage_names()));
    out.insert("stageFlags".into(), json!(job.mode.stage_flags()));
    out.insert("stagesFrom".into(), json!(job.mode.stages_from()));
    out.insert("path".into(), json!(job.path));
    out.insert("elapsedMs".into(), json!(elapsed_ms));
    out.insert("attempts".into(), json!(job.attempts));
    out.insert(
        "container".into(),
        if bridge.is_some() { Value::Null } else { json!(job.container) },
    );
    out.insert("log".into(), json!(job.log.display().to_string()));
    // M8: the chosen output form + every candidate's state
    merge(&mut out, &vpk_section(&job.vpk));
    // L2: did the source .vmap move under this job while it ran (a save_map during the compile
    // would make the later-started vrad3 reread it)? The flag is the honesty check, not a fault.
    let vmap = ContentPaths::new(state.app_root(), job.addon.clone()).addon_root().join(&job.path);
    let source_changed = match (mtime_of(&vmap), job.vmap_before) {
        (Some(now), Some(before)) => now > before,
        _ => false,
    };
    out.insert("sourceChanged".into(), json!(source_changed));
    match summary {
        Some(summary) => {
            out.insert("compiled".into(), json!(summary.compiled));
            out.insert("failed".into(), json!(summary.failed));
            out.insert("skipped".into(), json!(summary.skipped));
            out.insert("fatal".into(), json!(summary.fatal));
            out.insert(
                "failLines".into(),
                json!(summary.fail_lines),
            );
        }
        None => {
            for key in ["compiled", "failed", "skipped", "fatal", "failLines"] {
                out.insert(key.into(), Value::Null);
            }
        }
    }
    out.insert(
        "okLine".into(),
        json!(job.ok_line.or_else(|| find_ok_line(&text))),
    );
    out.insert("logTail".into(), json!(tail(&text, LOG_TAIL_LINES)));
    if let Some(cleanup) = &job.cleanup {
        out.insert("cleanup".into(), cleanup.clone());
    }
    if let Some(reason) = &job.reason {
        out.insert("reason".into(), json!(reason));
    }
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// Target resolution
// -------------------------------------------------------------------------------------------

/// The target the start identified, with the notes it could not decide on the way.
struct MapTarget {
    /// the addon this target was resolved in (one snapshot per call)
    addon: String,
    /// addon-content-relative, `maps/…vmap`.
    content_path: String,
    notes: Vec<Value>,
}

/// The dirty gate (`compile_refusal`): the target IS the active document means the compiler reads
/// the SAVED file of a possibly-dirty editor state — refuse like the mesh file route does
/// (`-32016`, the file-route wording); unsaved edits this server counted (the counter > 0) mean
/// exactly the same uncertainty with `dirty:false`. The pure decision, unit-tested.
pub(crate) fn compile_refusal(
    doc: Option<&Value>,
    content_path: &str,
    is_active: bool,
    unsaved: u64,
) -> Option<ToolError> {
    if !is_active {
        return None;
    }
    let dirty = doc.and_then(|d| d.get("dirty").and_then(Value::as_bool));
    let blocked = match dirty {
        Some(false) => unsaved > 0,
        _ => true,
    };
    if !blocked {
        return None;
    }
    let message = match dirty {
        Some(true) => "the map to compile is the active document and it is dirty (the compiler \
                       reads the SAVED file, so the output would not include the unsaved \
                       in-editor edits): save_map first, then compile",
        Some(false) => "the map to compile is the active document and this server has counted \
                        unsaved bridge edits for it (the compiler reads the SAVED file): \
                        save_map first, then compile",
        None => "the active document's dirty state is not reported; refusing to compile it (the \
                 compiler reads the SAVED file, so the output could be stale): save_map first, \
                 then compile",
    };
    let message = if unsaved > 0 {
        format!(
            "{message} ({unsaved} bridge edits counted on this server since the last save)"
        )
    } else {
        message.to_string()
    };
    Some(
        ToolError::new("UnsavedChanges", "UnsavedChanges", message)
            .with_code(-32016)
            .retry_safe(true)
            .with_data(json!({
                "reason": "compile reads the saved file",
                "path": content_path,
                "unsavedBridgeEdits": unsaved,
            })),
    )
}

/// The mounted-map decision (pure, unit-tested; M9 + N-review): a refusal when the engine holds
/// one of the target's engine names in game. `game_local` is the `game.local` reply (or a reply
/// shape` built from this server's own load memory, when the bridge has no `game.local`), and
/// `source` names where the answer came from in the message. The engine's name `sub/x` always
/// matches exactly; the bare stem `x` matches only when it is unambiguous — a top-level
/// `maps/x.vmap` source next to the target means the engine's `x` could be THAT map's compile,
/// and refusing the sub-directory map's compile would be a false positive.
#[derive(Debug)]
pub(crate) struct EngineNames {
    /// The content-relative spelling(s): ["sub/x"] or ["probe"].
    pub full: Vec<String>,
    /// The bare stem of a sub-directory map (`x`), `None` for a top-level one. It is the
    /// flattened candidate's engine name and — with no top-level source — unambiguous.
    pub stem: Option<String>,
}

impl EngineNames {
    /// Does `map` (the engine's reported map name, case-insensitive) name this target?
    fn matches(&self, map: &str) -> bool {
        self.full.iter().any(|name| name.eq_ignore_ascii_case(map))
            || self
                .stem
                .as_deref()
                .is_some_and(|stem| stem.eq_ignore_ascii_case(map))
    }
}

pub(crate) fn engine_names(content_path: &str) -> EngineNames {
    let rest = content_path
        .strip_prefix("maps/")
        .unwrap_or(content_path)
        .strip_suffix(".vmap")
        .unwrap_or(content_path);
    match rest.rsplit_once('/') {
        Some((_, stem)) => EngineNames {
            full: vec![rest.to_string()],
            stem: Some(stem.to_string()),
        },
        None => EngineNames {
            full: vec![rest.to_string()],
            stem: None,
        },
    }
}

/// Is the bare-stem engine name ambiguous for `content_path`, i.e. does a top-level
/// `maps/<stem>.vmap` exist that the flattened `maps/<stem>.vpk` could belong to (M8's rule on
/// the mounted half, review-2 M9)?
fn bare_stem_is_ambiguous(state: &HammerState, names: &EngineNames) -> bool {
    names
        .stem
        .as_ref()
        .is_some_and(|stem| state.content().addon_root().join("maps").join(format!("{stem}.vmap")).is_file())
}

pub(crate) fn mounted_refusal(
    game_local: &Value,
    names: &EngineNames,
    content_path: &str,
    ambiguous_stem: bool,
    source: &str,
) -> Option<ToolError> {
    if game_local.get("inGame").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let map = game_local.get("map").and_then(Value::as_str)?;
    if !names.matches(map) {
        return None;
    }
    if ambiguous_stem && names.stem.as_deref().is_some_and(|stem| stem.eq_ignore_ascii_case(map))
    {
        // the engine is in a map with the target's STEM, but a top-level maps/<stem>.vmap
        // exists: that flattened vpk is probably not this map's compile — the compile proceeds
        // (the caller adds a note)
        return None;
    }
    Some(
        ToolError::new(
            "EngineBusy",
            "EngineBusy",
            format!(
                "the engine has {map:?} in game ({source}), and {content_path} \
                 compiles to a .vpk the engine holds mounted: rewriting it under the running \
                 engine can fail the write or swap the map out underneath the reader — unload \
                 the engine from the map first with run_map {{unload: true}} (it posts the \
                 engine's own `disconnect` and waits for the idle level), or stop the tools \
                 session's game, then compile"
            ),
        )
        .with_data(json!({
            "reason": "map_in_game",
            "path": content_path,
            "gameLocal": game_local.clone(),
        }))
        .use_instead("run_map", "unload the engine from the map, then compile", Some(json!({"unload": true})))
        .retry_safe(true),
    )
}

/// The mounted-map check (M9 + ammendments): `game.local` (with ensure_connected + the live
/// NoLocalPlayer answer) is the engine's answer; the bridge's read failure is a NOTE (not a
/// refusal); when the bridge does not expose `game.local`, this server's own load memory (H1)
/// is the fallback evidence; the ambiguous bare-stem case never refuses. Mutates `target`'s
/// notes.
async fn mounted_map_check(
    state: &HammerState,
    deadline: u64,
    target: &mut MapTarget,
) -> Result<(), ToolError> {
    let local = super::run_map::read_game_local(state, deadline).await;
    let names = engine_names(&target.content_path);
    let ambiguous = bare_stem_is_ambiguous(state, &names);
    let memory = state.last_load.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    let engine_says_up = local.as_ref().and_then(super::run_map::GameLocal::mounted);
    // (only the `ok`-load memory can speak for the engine: an unresolved/failed/idle record
    // cannot)
    let mounted_by_memory = memory
        .as_ref()
        .filter(|last| last.resolved && !last.idle && last.state == "ok")
        // the store's name is the ENGINE name run_map told the console (`sub/x`, or the bare
        // stem it loaded the flattened vpk under)
        .filter(|last| names.matches(&last.name) && !(ambiguous && names.stem.as_deref().is_some_and(|stem| stem.eq_ignore_ascii_case(&last.name))));
    match local.as_ref().map(|l| (engine_says_up, l)) {
        Some((Some(false), local)) => {
            // the engine says it is NOT in a map: not a refusal, nothing to note — and the
            // NoLocalPlayer answer ("not in a map at all", the live disconnect state) arrives
            // already resolved as inGame:false (the live TODO)
            let _ = local;
        }
        Some((Some(true), local)) => {
            if ambiguity_note_if_bare_stem(local, &names, ambiguous, &mut target.notes).is_none() {
                if let Some(err) = mounted_refusal(
                    &local.to_json(),
                    &names,
                    &target.content_path,
                    ambiguous,
                    "game.local: inGame=true",
                ) {
                    return Err(err);
                }
            }
        }
        // the bridge exposes nothing usable (not advertised, or the read failed): the note, plus
        // this server's own load memory as the fallback evidence (H1's rule on the compile side)
        other => {
            if let Some((None, local)) = other {
                target.notes.push(json!(format!(
                    "game.local could not be read ({}) — the up-to-date engine answer is missing",
                    local
                        .error()
                        .map(json_string)
                        .unwrap_or_else(|| "no reply".into())
                )));
            }
            if let Some(last) = mounted_by_memory {
                if let Some(err) = mounted_refusal(
                    &json!({"inGame": true, "map": last.name, "source": "previous-load"}),
                    &names,
                    &target.content_path,
                    ambiguous,
                    "this server's last run_map load (no game.local exposed)",
                ) {
                    return Err(err);
                }
            }
        }
    }
    Ok(())
}

/// A note when the engine is in a map with the target's bare stem AND that stem is ambiguous
/// (a top-level source exists): the compile proceeds, but the caller should know.
fn ambiguity_note_if_bare_stem(
    local: &super::run_map::GameLocal,
    names: &EngineNames,
    ambiguous: bool,
    notes: &mut Vec<Value>,
) -> Option<()> {
    let map = local.map.as_deref()?;
    if !(ambiguous && names.stem.as_deref().is_some_and(|stem| stem.eq_ignore_ascii_case(map))) {
        return None;
    }
    let stem = names.stem.as_deref().expect("checked");
    notes.push(json!(format!(
        "the engine has {map:?} in game; a top-level maps/{stem}.vmap also exists, so the mounted \
         .vpk could belong to that map instead of this sub-directory map's compile: the compile \
         proceeds — run compile's target with run_map {{unload: true}} first if game.local's map \
         IS this map's flattened vpk"
    )));
    Some(())
}

fn json_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Which addon-relative `maps/…vmap` the start targets: an explicit accepted `path`, else the
/// active document's map (the default). A target that is the active document is gated
/// (`compile_refusal`); with an explicit path and no bridge, the gate is skipped with a note (the
/// compile itself needs no bridge).
async fn resolve_target(
    state: &HammerState,
    deadline: u64,
    args: &Value,
) -> Result<MapTarget, ToolError> {
    // The addon is snapshotted AFTER each branch's own `doc.active` read: that read refreshes the
    // document-derived addon, and the target must be resolved against the refreshed value.
    if let Some(raw) = args.get("path") {
        let Some(path) = raw.as_str() else {
            return Err(ToolError::invalid_arguments("path must be a string"));
        };
        let content_path = super::lifecycle::map_content_path(&json!({"path": path}))?;
        // The dirty gate needs the active document; without a bridge the rest of the route
        // still works, so this proceeds with a note instead of refusing.
        let doc = read_active_document(state, Some(deadline)).await.ok().flatten();
        let content = state.content();
        let active_map = doc
            .as_ref()
            .and_then(|d| d.get("path").and_then(Value::as_str))
            .and_then(|w| state.path_map.windows_to_host(w))
            .and_then(|host| content.addon_rel(&host));
        let is_active = active_map
            .as_deref()
            .map(|s| s.to_ascii_lowercase())
            == Some(content_path.to_ascii_lowercase());
        let mut notes = Vec::new();
        if doc.is_none() {
            notes.push(json!(
                "doc.active could not be read (no bridge?), so the unsaved-changes gate for the \
                 active document was skipped: if this server made the last edits, save_map \
                 before trusting the output"
            ));
        }
        return match compile_refusal(
            doc.as_ref(),
            &content_path,
            is_active,
            state.unsaved_edits(),
        ) {
            Some(err) => Err(err),
            None => Ok(MapTarget { addon: content.addon.clone(), content_path, notes }),
        };
    }

    // No path: the active document's map — this needs the bridge.
    let doc = require_document(state, deadline).await?;
    let content = state.content();
    let unsaved = state.unsaved_edits();
    if super::lifecycle::is_untitled(&doc) {
        return Err(ToolError::new(
            "UntitledDocument",
            "UntitledDocument",
            format!(
                "the active document is untitled (no path on disk): there is no saved map to \
                 compile — save_map first{}",
                if unsaved > 0 {
                    format!(" ({unsaved} unsaved bridge edits counted on this server)")
                } else {
                    String::new()
                }
            ),
        )
        .with_code(-32015)
        .with_data(json!({
            "path": doc.get("path").cloned().unwrap_or(Value::Null),
            "unsavedBridgeEdits": unsaved,
        }))
        .retry_safe(true));
    }
    let windows = doc
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let Some(host) = state.path_map.windows_to_host(&windows) else {
        return Err(ToolError::new(
            "PathNotMapped",
            "PathNotMapped",
            format!("the active document path {windows:?} has no host mapping (check --path-map)"),
        )
        .with_code(-32602)
        .retry_safe(true));
    };
    let Some(content_path) = content.addon_rel(&host) else {
        let hint = content.other_addon_hint(&host, state.addon_selection().1);
        return Err(ToolError::invalid_arguments(format!(
            "the active document is not under the active addon content root \
             ({}); only maps inside it can be compiled{}",
            content.addon_root().display(),
            hint.map(|h| format!(": {h}")).unwrap_or_default()
        )));
    };
    let lower = content_path.to_ascii_lowercase();
    if !lower.starts_with("maps/") || !lower.ends_with(".vmap") {
        return Err(ToolError::invalid_arguments(format!(
            "the active document is {content_path:?}, not a maps/<name>.vmap of the active addon"
        )));
    }
    match compile_refusal(Some(&doc), &content_path, true, unsaved) {
        Some(err) => Err(err),
        None => Ok(MapTarget {
            addon: content.addon.clone(),
            content_path,
            notes: vec![],
        }),
    }
}

/// `timeoutMs?`: default [`MAP_COMPILE_TIMEOUT_MS`], cap [`MAP_COMPILE_MAX_MS`]. NOT clamped to
/// the tool budget: the deadline governs the **background job**, not this call.
pub fn compile_deadline(args: &Value) -> Result<Duration, ToolError> {
    match args.get("timeoutMs") {
        None | Some(Value::Null) => Ok(Duration::from_millis(MAP_COMPILE_TIMEOUT_MS)),
        Some(Value::Number(n)) => {
            let Some(ms) = n.as_u64() else {
                return Err(ToolError::invalid_arguments(
                    "timeoutMs must be a positive integer",
                ));
            };
            if ms == 0 {
                return Err(ToolError::invalid_arguments("timeoutMs must be > 0"));
            }
            Ok(Duration::from_millis(ms.min(MAP_COMPILE_MAX_MS)))
        }
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "timeoutMs must be an integer, got {other}"
        ))),
    }
}

// -------------------------------------------------------------------------------------------
// Paths + argv
// -------------------------------------------------------------------------------------------

/// The compiled `.vpk` candidates of an addon-content-relative `maps/<x>.vmap` under
/// `<app>/game/csgo_addons/<addon>/` (M8). The authoritative location for a sub-directory map is
/// unverified live, so BOTH derived spellings ride along and the refreshed one decides:
///
/// * `content-relative` — the compiler's own content→game mapping:
///   `maps/sub/x.vmap` -> `maps/sub/x.vpk` (engine `map sub/x`);
/// * `flattened` — the recipe script's output rule: `maps/sub/x.vmap` -> `maps/x.vpk`
///   (engine `map x`; `compile_map.sh`: `OUT=…/game/csgo_addons/<addon>/maps/${MAPNAME##*/}.vpk`).
///
/// Identical for a top-level map (deduplicated to one candidate; **live**: a top-level map compiles
/// to `game/csgo_addons/<addon>/maps/<name>.vpk`, so the two spellings ARE the one file). `pub(crate)`:
/// `run_map` resolves the SAME candidates through it — one helper, two tools (M8).
///
/// Build the candidate list; `None` for a path this tool's gates should never have accepted.
pub(crate) fn vpk_candidates(
    app_root: &Path,
    addon: &str,
    content_path: &str,
) -> Option<Vec<VpkCandidate>> {
    let rest = content_path.strip_prefix("maps/")?;
    let rest = rest.strip_suffix(".vmap")?;
    if rest.is_empty() || rest.contains("..") {
        return None;
    }
    let mut out: Vec<VpkCandidate> = Vec::new();
    let push = |name: &str, form: &'static str, out: &mut Vec<VpkCandidate>| {
        let relative = format!("maps/{name}.vpk");
        if out.iter().any(|c| c.relative == relative) {
            return; // a top-level map: the two spellings are the same file
        }
        out.push(VpkCandidate {
            name: name.to_string(),
            relative,
            form,
            path: app_root
                .join("game/csgo_addons")
                .join(addon)
                .join("maps")
                .join(format!("{name}.vpk")),
            before: None,
        });
    };
    push(rest, "content-relative", &mut out);
    if let Some((_, stem)) = rest.rsplit_once('/') {
        push(stem, "flattened", &mut out);
    }
    (!out.is_empty()).then_some(out)
}

/// The output verdict (pure, M8): which candidate a verdict/poll reports — the one whose mtime
/// ADVANCED (that is the file the compiler refreshed); when none advanced, the first EXISTING
/// one; with neither, the first candidate (so the answer still names where the output should
/// be). Returns `(index, chosen_exists, chosen_advanced)`; `None` with no candidates at all.
fn vpk_choice(forms: &[VpkCandidate]) -> Option<(usize, bool, bool)> {
    let index = forms
        .iter()
        .position(|f| mtime_advanced(mtime_of(&f.path), f.before))
        .or_else(|| forms.iter().position(|f| f.path.is_file()))
        .or_else(|| (!forms.is_empty()).then_some(0))?;
    let chosen = &forms[index];
    Some((
        index,
        chosen.path.is_file(),
        mtime_advanced(mtime_of(&chosen.path), chosen.before),
    ))
}

/// The poll's `vpk…` fields from the job's candidates (pure, on top of [`vpk_choice`]), plus
/// every candidate's own state so "neither advanced, both reported" is visible.
fn vpk_section(forms: &[VpkCandidate]) -> Value {
    let candidates: Vec<Value> = forms
        .iter()
        .map(|f| {
            json!({
                "form": f.form,
                "path": f.path.display().to_string(),
                "engineName": f.name,
                "exists": f.path.is_file(),
                "mtimeAdvanced": mtime_advanced(mtime_of(&f.path), f.before),
            })
        })
        .collect();
    match vpk_choice(forms) {
        Some((index, exists, advanced)) => json!({
            "vpkPath": forms[index].path.display().to_string(),
            "vpkForm": forms[index].form,
            "vpkExists": exists,
            "vpkUpdated": advanced,
            "vpkCandidates": candidates,
        }),
        None => json!({
            "vpkPath": Value::Null,
            "vpkForm": Value::Null,
            "vpkExists": false,
            "vpkUpdated": false,
            "vpkCandidates": candidates,
        }),
    }
}

/// Merge a `json!`-object section into a `Map` reply (a field-set a poll and a start share).
fn merge(out: &mut Map<String, Value>, section: &Value) {
    if let Some(object) = section.as_object() {
        for (key, value) in object {
            out.insert(key.clone(), value.clone());
        }
    }
}

/// `<repo>/target/compile-<name-dashed>/rc-<jobId>.log` (`<repo>`: [`cs2_mcp_common::mcp_repo_dir`]
/// of the project root) — the directory the proven script kept its log at (`compile_map.sh`:
/// `$PROJECT/cs2-mcp/target/compile-${MAPNAME////-}`, the older parent layout),
/// with ONE LOG PER JOB (L3): `maps/a-b.vmap` and `maps/a/b.vmap` share `compile-a-b`, so a new
/// start must never overwrite or delete what an older job's poll view points at.
fn log_path(project_root: &Path, content_path: &str, id: &str) -> PathBuf {
    let name = content_path
        .trim()
        .trim_start_matches("maps/")
        .trim_end_matches(".vmap")
        .replace('/', "-");
    let name = if name.is_empty() { "map".to_string() } else { name };
    cs2_mcp_common::mcp_repo_dir(project_root)
        .join(format!("target/compile-{name}"))
        .join(format!("rc-{id}.log"))
}

/// The `bash -c` command line: the recipe verbatim (`compile_map.sh`) with the mode's
/// `EXTRA_FLAGS` in place — plus the `export CS2MCP_COMPILE_JOB=<tag>` that tags this run's
/// process tree (M3 + N1: the tag VALUE is unique per process ([`tag_value`]), for two reasons
/// the plain job id was not: two hammer-mcp processes can share the tools container, each
/// running its own `c1`, and a restart can reuse a poll's id for a different job). Wine children
/// inherit the environment, so the cleanup can kill exactly this job's
/// `resourcecompiler.exe`…`vrad3.exe` tree and nothing else in the container. The container path
/// is built over an already shape-enforced addon-relative path (`map_content_path`, which is
/// what `safe_container_path` also accepts) and the addon is the state's own (`--addon`), so the
/// string carries nothing injectable.
fn compile_argv(addon: &str, content_path: &str, mode: CompileMode, tag: &str) -> Vec<String> {
    let container_path = format!("/app/content/csgo_addons/{addon}/{content_path}");
    debug_assert!(super::compile::safe_container_path(&container_path));
    vec![
        "bash".to_string(),
        "-c".to_string(),
        format!(
            "export CS2MCP_COMPILE_JOB={tag}; cd /app && ./entrypoint.sh resourcecompiler.exe \
             -threads 8 -fshallow -maxtextureres 256 -nop4 -quiet -unbufferedio -noassert{} \
             -i {container_path}",
            mode.flags()
        ),
    ]
}

/// The unique-per-process tag VALUE one compile job exports into `/proc/*/environ` (review-2
/// N1): `<jobId>-<pid>-<hex nonce>`. The short `jobId` stays the tool's poll handle, but it is
/// NOT unique across hammer-mcp processes (two servers can both have a job `c1`) and a restart
/// can re-emit the same id — the orphan check must only ever classify a LIVE tag as this
/// server's own on an exact match with one of this store's tags, which is only safe when no
/// other process could mint the same tag. Character set `[a-z0-9-]`: the tag rides inside
/// single quotes in the kill script and as a shell word, so even `-` is the only punctuation
/// present.
pub fn tag_value(id: &str) -> String {
    static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = NONCE
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ^ SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
    format!("{id}-{}-{nonce:x}", std::process::id())
}

/// The tag as it appears in `/proc/*/environ` (the kill/scan scripts match exactly this, and
/// every report/error names it).
pub(crate) fn job_tag(tag_value: &str) -> String {
    format!("CS2MCP_COMPILE_JOB={tag_value}")
}

/// The tagged-tree kill (pure, M3 + N6): the container-side record of the recipe's cleanup. A
/// bare `pkill -f resourcecompiler.exe` would sweep every matching process in the container — a
/// concurrent `model_compile`/`particle_compile` included — and would MISS this job's `vrad3.exe`
/// children (whose command line does not name resourcecompiler). This script kills only the
/// `/proc` entries whose environment carries one of the given `tag_values` EXACTLY (Wine
/// children inherit it): SIGTERM first (the recipe's `pkill` wording), one second of grace, then
/// SIGKILL. Its last line is parsed by [`end_the_linger`]:
/// `tagged-kill: matched=<n> killed=<n> remaining=<r>`.
///
/// N6 hardening and the live-lesson fixes:
///
/// * the tag *scan* inside the script is ANCHORED
///   (`grep -E -m1 '^CS2MCP_COMPILE_JOB='`): an unanchored `grep -F` read a bystanders'
///   `OTHER_CS2MCP_COMPILE_JOB=…` as a malformed foreign tag;
/// * `wineserver` and `services.exe` are EXCLUDED by `/proc/<pid>/comm`: if no Wine app runs in
///   the container when a compile starts, the compile's wine bootstrap itself carries the tag —
///   and a Hammer started in that same container during the compile would attach to the tagged
///   `wineserver` and die at cleanup (the processes this script must reach are the compiler's
///   own executables and the tagged `bash` wrapper);
/// * the `remaining` count ignores ZOMBIES (state `Z`) and RE-polls it for up to 2 s after the
///   SIGKILL; a pid whose /proc entry is gone counts as dead (live: the old count treated a reaped
///   pid as alive and reported `remaining=1`) — the live run showed a kill reporting `remaining=1` where a scan moments later
///   found nothing, so the count is only read once the still-alive set has stabilised.
pub(crate) fn tagged_kill_script(tag_values: &[String]) -> String {
    let tags = tag_values.join(" ");
    debug_assert!(!tags.is_empty() && !tags.split_whitespace().any(|tag| {
        tag.is_empty() || !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    }), "tag values must be shell-safe words: {tags:?}");
    format!(
        "self=$$; declare -A PD=(); pids=''; matched=0\n\
         \x20for d in /proc/[0-9]*; do\n\
         \x20\x20p=\"${{d#/proc/}}\"\n\
         \x20\x20[ \"$p\" = \"$self\" ] && continue\n\
         \x20\x20case \"$(cat \"$d/comm\" 2>/dev/null)\" in wineserver|services.exe) continue ;; esac\n\
         \x20\x20v=$(tr '\\0' '\\n' <\"$d/environ\" 2>/dev/null | grep -E -m1 '^CS2MCP_COMPILE_JOB=')\n\
         \x20\x20[ -n \"$v\" ] || continue\n\
         \x20\x20t=\"${{v#CS2MCP_COMPILE_JOB=}}\"\n\
         \x20\x20for want in {tags}; do\n\
         \x20\x20\x20[ \"$t\" = \"$want\" ] && PD[$want]=\"${{PD[$want]}} $p\" && break\n\
         \x20\x20done\n\
         \x20done\n\
         \x20for want in {tags}; do\n\
         \x20\x20n=$(echo ${{PD[$want]}} | wc -w)\n\
         \x20\x20[ \"$n\" -gt 0 ] || continue\n\
         \x20\x20pids=\"$pids ${{PD[$want]}}\"\n\
         \x20\x20matched=$((matched+n))\n\
         \x20done\n\
         if [ \"$matched\" = 0 ]; then\n\
         \x20 echo 'tagged-kill: matched=0 killed=0 remaining=0'\n\
         \x20 exit 0\n\
         fi\n\
         kill -TERM $pids 2>/dev/null\n\
         sleep 1\n\
         kill -KILL $pids 2>/dev/null\n\
         zombie_free() {{\n\
         \x20 n=0\n\
         \x20 for p in $1; do\n\
         \x20\x20 s=$(sed -n 's/^State:[[:space:]]*\\(.\\).*/\\1/p' \"/proc/$p/status\" 2>/dev/null)\n\
         \x20\x20 [ -n \"$s\" ] && [ \"$s\" != Z ] && n=$((n+1))\n\
         \x20 done\n\
         \x20 echo $n\n\
         }}\n\
         remaining=$(zombie_free \"$pids\"); try=0\n\
         while [ \"$remaining\" -gt 0 ] && [ \"$try\" -lt 20 ]; do\n\
         \x20 sleep 0.1; remaining=$(zombie_free \"$pids\"); try=$((try+1))\n\
         done\n\
         echo \"tagged-kill: matched=$matched killed=$matched remaining=$remaining\"\n"
    )
}

/// The cleanup exec's argv for one (or several) job tag values.
pub(crate) fn tagged_kill_argv(tag_values: &[String]) -> Vec<String> {
    vec![
        "bash".to_string(),
        "-c".to_string(),
        tagged_kill_script(tag_values),
    ]
}

/// The orphan scan (one bounded exec at every start): which `CS2MCP_COMPILE_JOB=<tag>` process
/// trees are alive in the container right now. It is [`tagged_kill_script`]'s reader without the
/// kill: the same `/proc/*/environ` walk, the same ANCHORED exact-token match (N6: an
/// unanchored `grep -F` read a bystander's `OTHER_CS2MCP_COMPILE_JOB=…` as a foreign tag), the
/// same `wineserver`/`services.exe` exclusion by comm, and its last line is
/// `tagged-scan: jobs=<tag> <tag> …` (parsed by [`parse_tagged_scan`]).
///
/// The **permission** fact is load-bearing: the podman-exec user can read the compiler's
/// `/proc/<pid>/environ` (Wine children of this container) but gets `Permission denied` on the
/// game's processes (a foreign uid). The read is redirected to `/dev/null`, so those processes are
/// silent and are **not counted** — a tag this scan cannot read is not evidence of a foreign job.
pub(crate) fn tagged_scan_script() -> String {
    "self=$$; pat='CS2MCP_COMPILE_JOB='; seen=''\n\
     for d in /proc/[0-9]*; do\n\
     \x20 p=\"${d#/proc/}\"\n\
     \x20 [ \"$p\" = \"$self\" ] && continue\n\
     \x20 case \"$(cat \"$d/comm\" 2>/dev/null)\" in wineserver|services.exe) continue ;; esac\n\
     \x20 v=$(tr '\\0' '\\n' <\"$d/environ\" 2>/dev/null | grep -E -m1 '^CS2MCP_COMPILE_JOB=')\n\
     \x20 [ -n \"$v\" ] && seen=\"$seen ${v#\"$pat\"}\"\n\
     done\n\
     echo \"tagged-scan: jobs=$seen\"\n"
        .to_string()
}

/// The scan's report line -> the distinct tags it saw (`None` when the line is absent: the script
/// did not run, or its output is not the report). A tag is a per-process tag value
/// ([`tag_value`], e.g. `c1-32251-abcd`); the order is the report's, deduplicated.
pub fn parse_tagged_scan(text: &str) -> Option<Vec<String>> {
    static SCAN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let regex = SCAN.get_or_init(|| {
        regex::Regex::new(r"(?m)^[ \t]*tagged-scan: jobs=(.*)$").expect("tagged-scan report regex")
    });
    let captured = regex.captures(text)?;
    let mut tags: Vec<String> = Vec::new();
    for tag in captured[1].split_whitespace() {
        if !tags.iter().any(|seen| seen == tag) {
            tags.push(tag.to_string());
        }
    }
    Some(tags)
}

/// What the start's orphan scan found: the tags of the compile trees alive in the container, split
/// by whether this server knows the job.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TaggedJobs {
    /// Tags of jobs THIS server knows (a finished job whose tagged tree outlived its verdict, a
    /// reaped one): killing them is allowed — the id is ours.
    pub own: Vec<String>,
    /// Tags of jobs this server does **not** know (the usual case: a server that exited mid-compile
    /// left an orphan, and a fresh server's store is empty). Reported, never auto-killed: the job
    /// behind the tag is unknowable from here.
    pub foreign: Vec<String>,
}

/// Split the scan's tags into this server's own and foreign ones (pure, unit-tested; N1): the
/// comparison is exact tag-VALUE equality against this store's tags ([`CompileJobs::tags`]) —
/// never against the short job ids, which are not unique across processes.
pub(crate) fn classify_tagged_jobs(alive: &[String], known_tags: &[String]) -> TaggedJobs {
    let mut out = TaggedJobs::default();
    for tag in alive {
        if known_tags.iter().any(|known| known == tag) {
            out.own.push(tag.clone());
        } else {
            out.foreign.push(tag.clone());
        }
    }
    out
}

/// `EngineBusy`: this server is already compiling a map (`CompileJobs::start` is the atomic
/// check-and-insert; the pre-checks and the post-scan re-check share this wording so every refusal
/// names the same running job and the same way to poll it).
pub(crate) fn engine_busy_running(running: &str) -> ToolError {
    ToolError::new(
        "EngineBusy",
        "EngineBusy",
        format!(
            "a map compile is already running (jobId {running:?}): poll it with \
             compile_map {{jobId: {running:?}}} — one compile at a time"
        ),
    )
    .with_data(json!({"jobId": running}))
    .retry_safe(true)
}

/// `EngineBusy`: a compile job of ANOTHER server is still running in this container. A second
/// compiler started on top of it would race it on the same output, so the start is refused — with
/// the tag and a way to clear it (this server does not kill a tree whose job it cannot know).
pub(crate) fn foreign_compile_job_error(foreign: &[String]) -> ToolError {
    let tags: Vec<String> = foreign.iter().map(|tag| job_tag(tag)).collect();
    let first = foreign.first().cloned().unwrap_or_default();
    ToolError::new(
        "EngineBusy",
        "EngineBusy",
        format!(
            "a map compile from another server is still running in this container (tag {}): a \
             second compiler on top of it would race it on the same .vpk, so nothing was \
             started. Nothing can be attributed to the job behind the tag, so this server does \
             not kill it: wait for it to finish, or stop it in the container (podman exec \
             <container> bash -c 'for d in /proc/[0-9]*; do tr \"\\0\" \"\\n\" <\"$d/environ\" \
             2>/dev/null | grep -Fqx -- {} && kill -TERM \"${{d#/proc/}}\"; done') and start \
             again",
            tags.join(", "),
            job_tag(&first)
        ),
    )
    .with_data(json!({
        "reason": "foreign_compile_job",
        "foreignJobs": foreign,
        "tags": tags,
        "note": "tags found in the container's /proc/*/environ that no job of this server owns: \
                 reported, not killed (the job behind them is unknowable)",
        "hint": "wait for the other compile to finish, or stop its tree in the container",
    }))
    .retry_safe(true)
}

/// The start's orphan check: one bounded `exec_in` that reads the tags alive in the container.
/// `Err(text)` is the scan's own diagnostic (podman error, no report line): the check is
/// best-effort and a start without it is the pre-check behaviour, so the caller turns it into a
/// note, never a refusal.
async fn scan_tagged_jobs(state: &HammerState, container: &str) -> Result<Vec<String>, String> {
    let argv = vec![
        "bash".to_string(),
        "-c".to_string(),
        tagged_scan_script(),
    ];
    match state
        .runtime
        .exec_in(container, &argv, TAGGED_SCAN_TIMEOUT)
        .await
    {
        Ok(output) => {
            let text = combine(
                &String::from_utf8_lossy(&output.stdout),
                &String::from_utf8_lossy(&output.stderr),
            );
            parse_tagged_scan(&text).ok_or_else(|| {
                format!(
                    "the tagged-process scan printed no report ({:?})",
                    tail(&text, 5)
                )
            })
        }
        Err(error) => Err(format!("{error:#}")),
    }
}

/// The shutdown cleanup: end the container-side process tree of every compile job this server
/// leaves WITHOUT a cleanup report — a `Running` one, but also a job another path closed
/// `failed` (the probe-error, exit-fold, supervisor or dead-reaper path) whose tagged children
/// may still be alive (review-2 N5): in-process the next start ends such a tree as "own", which
/// is no longer possible after the process is gone. Without the cleanup the container-side
/// compiler keeps running with nobody watching it, its `CS2MCP_COMPILE_JOB=<tag>` has no owner,
/// and the next server cannot tell it from a live compile (it refuses the start and names the
/// tag). The same tagged kill the OK/timeout teardown uses — batched into ONE exec per container
/// when several jobs need it (the kill script accepts several exact tags, so the agents-share-
/// the-instance case does not burn the [`SHUTDOWN_KILL_BUDGET`] with one sleep per job). The
/// whole cleanup is capped, so a hung podman cannot hold the exit. Returns the report `main`
/// logs.
pub async fn shutdown_cleanup(state: &HammerState) -> Value {
    // N7: block new starts FIRST, before the store is read: a start between the read and the
    // execs would otherwise insert + spawn its job and be orphaned by the exit. `start` checks
    // the flag under the store lock, so an insert cannot land after this read.
    state.compile_jobs.begin_shutdown();
    // bridge-route jobs: the bridge owns the process, so the cleanup is a best-effort, bounded
    // `job.cancel` (the job must not outlive this server either); the orphan /proc scan and the
    // tagged kill are the podman route's only. The two halves run CONCURRENTLY, each inside its
    // own [`SHUTDOWN_KILL_BUDGET`], so the exit is held for one budget, not two.
    let (bridge_report, mut report) = tokio::join!(
        cancel_bridge_jobs_at_shutdown(state),
        podman_shutdown(state)
    );
    if let Some(bridge) = bridge_report {
        report["bridge"] = bridge;
    }
    report
}

/// The podman half of [`shutdown_cleanup`]: the tagged kill for every job left without a cleanup
/// report.
async fn podman_shutdown(state: &HammerState) -> Value {
    let jobs = state.compile_jobs.jobs_needing_cleanup();
    if jobs.is_empty() {
        return json!({
            "jobs": [],
            "note": "no compile job was left without a cleanup report: nothing to kill",
        });
    }
    let deadline = Instant::now() + SHUTDOWN_KILL_BUDGET;
    // batch the jobs per container (a container switch between jobs is possible): one kill exec
    // for all of one container's tags
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&CompileJob>> = HashMap::new();
    for job in &jobs {
        if !groups.contains_key(&job.container) {
            order.push(job.container.clone());
        }
        groups.entry(job.container.clone()).or_default().push(job);
    }
    let mut killed: Vec<Value> = Vec::new();
    for container in &order {
        let group = &groups[container];
        let left = deadline.saturating_duration_since(Instant::now());
        let report = if left.is_zero() {
            Err("the shutdown budget ran out before this group's kill could run: its tags still \
                 name a compiler tree in the container".to_string())
        } else {
            let tags: Vec<String> = group.iter().map(|job| job.tag.clone()).collect();
            match tokio::time::timeout(left, end_the_linger(state, container, &tags)).await {
                Ok(report) => Ok(report),
                Err(_) => Err(format!(
                    "the tagged kill did not finish within the {} s shutdown budget: the tags \
                     still name a compiler tree in the container",
                    SHUTDOWN_KILL_BUDGET.as_secs_f64()
                )),
            }
        };
        for job in group {
            match &report {
                Ok(value) => {
                    let mut entry = value.clone();
                    if group.len() > 1 {
                        // the batched report is one aggregate for all the group's tags: mark
                        // each job's share so the reader cannot mistake it for a per-job count
                        entry["tag"] = json!(job_tag(&job.tag));
                        entry["batched"] = json!(true);
                        entry["note"] = json!(format!(
                            "batched with {} other job tag(s) of the same container in one kill \
                             script (the shutdown budget is too small for one exec each): the \
                             matched/killed/remaining counts cover the whole batch",
                            group.len() - 1
                        ));
                    }
                    killed.push(entry);
                }
                Err(note) => killed.push(json!({
                    "tag": job_tag(&job.tag),
                    "matched": Value::Null,
                    "note": note,
                })),
            }
        }
    }
    // Close the `Running` store entries: nothing reads them again in this process, but a job
    // left `Running` would be the honest state only for a server that is still there to watch
    // it. (Jobs that were already closed stay as they are.)
    for job in &jobs {
        if job.state == JobState::Running {
            finish(
                state,
                &job.id,
                JobState::Failed,
                None,
                None,
                Some(format!(
                    "the server shut down while this job was running: its tagged compiler tree \
                     ({}) was ended with the tagged kill (see the shutdown cleanup report)",
                    job_tag(&job.tag)
                )),
                None,
            );
        }
    }
    json!({
        "jobs": jobs.iter().map(|job| job.id.clone()).collect::<Vec<_>>(),
        "killed": killed,
        "budgetMs": SHUTDOWN_KILL_BUDGET.as_millis() as u64,
    })
}

/// The shutdown half of the bridge route, inside one [`SHUTDOWN_KILL_BUDGET`]:
///
/// 1. wait for the `job.start` requests in flight (they hold a [`StartGuard`]): their reply may be a
///    spawned job nobody has recorded yet — each such start also cancels its own job when its
///    reply lands after the flag, so the wait only has to keep the process alive until it does;
/// 2. `job.cancel` for every bridge job that may still run: the `Running` compile_map jobs (an
///    id-less one is looked up in `job.status {}` first) and the `compile_asset`/file-route ids this
///    server started and does not know to be finished.
///
/// Every answer is kept as it arrives; what the budget cut off is listed as unconfirmed. `None`
/// when there is nothing to cancel (the bridge is not even connected to then).
async fn cancel_bridge_jobs_at_shutdown(state: &HammerState) -> Option<Value> {
    let wait_until = tokio::time::Instant::now() + SHUTDOWN_START_WAIT;
    while state.compile_jobs.starts_in_flight() > 0 && tokio::time::Instant::now() < wait_until {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let starts_still_in_flight = state.compile_jobs.starts_in_flight();
    // the cancels' own budget starts now
    let deadline = tokio::time::Instant::now() + SHUTDOWN_KILL_BUDGET;
    let mut unconfirmed: Vec<Value> = Vec::new();
    // starts whose reply was lost (or whose future was dropped) left a clientRef placeholder: one
    // `job.status {}` resolves them to the jobs they spawned (the list does the bookkeeping)
    let pending = state.compile_jobs.pending_refs();
    // (twice: a future dropped mid-request leaves its late reply on the connection, and the next
    // request reads it as an unexpected id — the second one is clean)
    let mut resolved = false;
    if !pending.is_empty() {
        for _ in 0..2 {
            if matches!(
                tokio::time::timeout_at(deadline, super::bridge_job::list(state)).await,
                Ok(Ok(_))
            ) {
                resolved = true;
                break;
            }
        }
    }
    if !pending.is_empty() && !resolved {
        for client_ref in pending {
            unconfirmed.push(json!({
                "clientRef": client_ref,
                "note": "a job.start reply was lost and the bridge could not be asked for the job \
                         it may have spawned (the bridge ends it with its own deadline)",
            }));
        }
    }
    let entries = state.compile_jobs.running_bridge_entries();
    let assets = state.compile_jobs.assets();
    if entries.is_empty() && assets.is_empty() && unconfirmed.is_empty() {
        return None;
    }
    let mut cancelled: Vec<Value> = Vec::new();
    let mut targets: Vec<(Option<String>, String)> = Vec::new();
    for (id, bridge_id) in entries {
        if !bridge_id.is_empty() {
            targets.push((Some(id), bridge_id));
            continue;
        }
        // lost start reply: the job may exist on the bridge under no id we know
        match tokio::time::timeout_at(deadline, adopt_entry(state, &id)).await {
            Ok(Ok(true)) => {
                if let Some(bridge) = state.compile_jobs.bridge_ref(&id) {
                    targets.push((Some(id), bridge.bridge_id));
                }
            }
            _ => unconfirmed.push(json!({"jobId": id, "note": "no bridge id: the job.start reply was lost and no matching job was found"})),
        }
    }
    for asset in assets {
        if !targets.iter().any(|(_, bridge_id)| *bridge_id == asset) {
            targets.push((None, asset));
        }
    }
    for (id, bridge_id) in &targets {
        let answer = tokio::time::timeout_at(
            deadline,
            super::bridge_job::cancel(state, bridge_id),
        )
        .await;
        match answer {
            Ok(Ok(reply)) => {
                if let Some(id) = id {
                    if reply.get("cancelled").and_then(Value::as_bool) == Some(true) {
                        finish(
                            state,
                            id,
                            JobState::Cancelled,
                            None,
                            None,
                            Some(
                                "the server shut down while this job was running: it was \
                                 cancelled on the bridge (job.cancel)"
                                    .to_string(),
                            ),
                            Some(json!({"route": "bridge", "note": "cancelled at shutdown"})),
                        );
                    }
                }
                cancelled.push(json!({
                    "bridgeJobId": bridge_id,
                    "jobId": id,
                    "cancelled": reply.get("cancelled"),
                    "state": reply.get("state"),
                }));
            }
            Ok(Err(error)) => cancelled.push(json!({
                "bridgeJobId": bridge_id,
                "jobId": id,
                "error": error.message,
            })),
            Err(_) => unconfirmed.push(json!({
                "bridgeJobId": bridge_id,
                "jobId": id,
                "note": "the shutdown budget ran out before the bridge answered job.cancel: the job \
                         may still be running (the bridge ends it with its own deadline)",
            })),
        }
    }
    let mut report = json!({
        "cancelled": cancelled,
        "budgetMs": SHUTDOWN_KILL_BUDGET.as_millis() as u64,
        "startWaitMs": SHUTDOWN_START_WAIT.as_millis() as u64,
    });
    if starts_still_in_flight > 0 {
        report["startsStillInFlight"] = json!(starts_still_in_flight);
    }
    if !unconfirmed.is_empty() {
        report["unconfirmed"] = Value::Array(unconfirmed);
    }
    Some(report)
}

// -------------------------------------------------------------------------------------------
// Log watching
// -------------------------------------------------------------------------------------------

static OK_WATCH: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

/// The completion line, verbatim: `^[[:space:]]*OK: [0-9]+ compiled, [0-9]+ failed` (CRLF and the
/// recipe's leading-space tolerance; the real line is ` OK: 19 compiled, 0 failed, 1 skipped,
/// 2m:19s`). `None` until the compiler reaches its summary.
pub fn find_ok_line(text: &str) -> Option<String> {
    let regex = OK_WATCH.get_or_init(|| {
        regex::Regex::new(r"(?m)^[ \t]*OK: [0-9]+ compiled, [0-9]+ failed")
            .expect("ok-line watch regex")
    });
    // the LAST matching line: a retried attempt keeps the failed attempt's output in the file
    let mut last = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if regex.is_match(line) {
            last = Some(line.trim().to_string());
        }
    }
    last
}

/// Transient exec failure (HANDOFF.md quirk):
/// `crun: error opening file /run/user/…/crun/<id>/status: No such file or directory` — podman's
/// runtime dir did not exist for that one call; the recipe's answer is one retry.
fn crun_transient(text: &str) -> bool {
    text.contains("crun") && text.contains("status: No such file or directory")
}

/// Read the whole log (it may not exist yet: podman has not written a byte). LOSSY (M1): Wine
/// or the compiler can emit one byte that is not UTF-8 (a CP1252 path or message, a cut
/// multibyte sequence) and `read_to_string`'s error would be treated as an EMPTY log — the OK
/// line would never be seen and a successful compile would sit as a false `timeout` until the
/// deadline. `from_utf8_lossy` reads every byte.
fn log_text(log: &Path) -> String {
    match std::fs::read(log) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    }
}

/// The bytes appended to the log since `*offset` (M1: the 500 ms poll reads only what is new —
/// no full re-read and re-parse of the file every time), lossy like [`log_text`]. A file that
/// shrank (a fresh attempt recreated it) restarts at 0, and a partial final line is left for
/// the next poll (the reader stops at the last newline: [`find_ok_line`] whole lines only).
fn appended_log_text(log: &Path, offset: &mut u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let len = match std::fs::metadata(log).map(|m| m.len()) {
        Ok(len) => len,
        Err(_) => return String::new(),
    };
    if len < *offset {
        *offset = 0;
    }
    if len <= *offset {
        return String::new();
    }
    let start = *offset;
    let Ok(mut file) = std::fs::File::open(log) else {
        return String::new();
    };
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let Some(new_line) = bytes.iter().rposition(|&b| b == b'\n') else {
        return String::new();
    };
    *offset = start + new_line as u64 + 1;
    String::from_utf8_lossy(&bytes[..=new_line]).into_owned()
}

// -------------------------------------------------------------------------------------------
// The background run
// -------------------------------------------------------------------------------------------

/// How the exec's own exit folds into the job (pure, M2 pinned): the log is read ONCE MORE when
/// the exec has exited — a clean exit that raced the poll's read by microseconds still shows the
/// line and takes the OK path, instead of a false `failed` against a vpk that is fine.
#[derive(Debug)]
enum ExitFold {
    Ok,
    RetryCrun,
    Failed {
        summary: Option<CompileSummary>,
        reason: String,
    },
}

fn fold_exit(text: &str, attempt: u32, status: &str) -> ExitFold {
    if find_ok_line(text).is_some() {
        return ExitFold::Ok;
    }
    if attempt == 1 && crun_transient(text) {
        return ExitFold::RetryCrun;
    }
    ExitFold::Failed {
        summary: parse_summary(text),
        reason: format!(
            "the compiler exec exited ({status}) without an OK line — the compile failed; \
             {attempt} attempt(s) in this job"
        ),
    }
}

/// The compile task: run the exec in the background (output → the log file), poll for the OK
/// line / process death / deadline, then finalise the job. One bounded retry on the transient
/// `crun` exec failure; nothing here waits on the watch that cannot report itself.
async fn run_compile(
    state: HammerState,
    id: String,
    argv: Vec<String>,
    container: String,
    log: PathBuf,
    deadline: Duration,
) {
    // the tag this run exported (argv carries `CS2MCP_COMPILE_JOB=<tag>`); every cleanup exec and
    // reason that names the tree uses it, not the poll's short id (N1)
    let tag = state
        .compile_jobs
        .get(&id)
        .map(|job| job.tag)
        .unwrap_or_else(|| tag_value(&id));
    let deadline_at = Instant::now() + deadline;
    'attempts: for attempt in 1..=2u32 {
        state.compile_jobs.update(&id, |job| job.attempts = attempt);
        let mut handle = match state
            .runtime
            .exec_in_background(&container, &argv, &log)
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                // A missing podman binary cannot recover; the one bounded second chance is the
                // transient `crun` spawn failure. Both leave the job's answer in its state.
                if attempt == 1 && format!("{error:#}").contains("crun") {
                    continue 'attempts;
                }
                finish(
                    &state,
                    &id,
                    JobState::Failed,
                    None,
                    None,
                    Some(format!("the compiler exec could not be started: {error:#}")),
                    None,
                );
                return;
            }
        };
        // M1: each poll reads only the bytes appended since the last one.
        let mut offset: u64 = 0;
        loop {
            tokio::time::sleep(MAP_COMPILE_POLL).await;
            let appended = appended_log_text(&log, &mut offset);
            if find_ok_line(&appended).is_some() {
                finish_ok(&state, &id, &tag, &container, &mut handle, &log).await;
                return;
            }
            if state.compile_jobs.cancel_requested(&id) {
                // `compile_map {cancel:true}` / `job_cancel`: end the exec, run the tagged kill
                // (the same teardown as the deadline), close the job `cancelled`
                handle.kill();
                let _ = handle.withdrawn().await;
                let cleanup = Some(end_the_linger(&state, &container, std::slice::from_ref(&tag)).await);
                finish(
                    &state,
                    &id,
                    JobState::Cancelled,
                    parse_summary(&log_text(&log)),
                    None,
                    Some(
                        "cancelled on request: the exec was ended and this job's tagged compiler \
                         tree was killed (see cleanup); the output may be partial or stale"
                            .to_string(),
                    ),
                    cleanup,
                );
                return;
            }
            match handle.finished() {
                Err(error) => {
                    // M4: a failed probe is an exec death, never "still running" — the job
                    // closes here instead of spinning to the deadline against a dead handle.
                    let text = log_text(&log);
                    finish(
                        &state,
                        &id,
                        JobState::Failed,
                        parse_summary(&text),
                        None,
                        Some(format!(
                            "the exec could not be probed ({error}): the podman exec process \
                             died while this task watched it — the log still shows whatever the \
                             compiler printed ({attempt} attempt(s) in this job)"
                        )),
                        None,
                    );
                    return;
                }
                Ok(Some(status)) => {
                    // M2: the exec ended between the poll's log read and this check — the log is
                    // read ONCE more before it is called a failure.
                    let text = log_text(&log);
                    match fold_exit(&text, attempt, &status.to_string()) {
                        ExitFold::Ok => {
                            finish_ok(&state, &id, &tag, &container, &mut handle, &log).await;
                            return;
                        }
                        ExitFold::RetryCrun => continue 'attempts,
                        ExitFold::Failed { summary, reason } => {
                            // N5: the compiler exited without an OK line, so the tagged kill
                            // never ran for it — its tree may still be alive (a vrad3 child that
                            // keeps going, most commonly). Run it here, so the in-process store
                            // already carries a cleanup report and the shutdown has one less
                            // tree to guess at.
                            let cleanup = Some(end_the_linger(&state, &container, std::slice::from_ref(&tag)).await);
                            finish(
                                &state,
                                &id,
                                JobState::Failed,
                                summary,
                                None,
                                Some(reason),
                                cleanup,
                            );
                            return;
                        }
                    }
                }
                Ok(None) if Instant::now() >= deadline_at => {
                    handle.kill();
                    let _ = handle.withdrawn().await;
                    let cleanup = Some(end_the_linger(&state, &container, std::slice::from_ref(&tag)).await);
                    finish(
                        &state,
                        &id,
                        JobState::Timeout,
                        parse_summary(&log_text(&log)),
                        None,
                        Some(format!(
                            "no OK line within {:.0} s: the exec was ended (the recipe's deadline \
                             kill); the compiler may still have been working — check the log and \
                             re-run",
                            deadline.as_secs_f64()
                        )),
                        cleanup,
                    );
                    return;
                }
                Ok(None) => {}
            }
        }
    }
}

/// The M4 supervisor: await the run task; on a panic (a poisoned store lock, a runaway
/// invariant) close the job `failed` with the panic text — a job left `Running` would refuse
/// every later start until the server restarts, and nothing would watch the container. A
/// *cancelled* task (the runtime itself shutting down) is not closed: the store's dead-job
/// reaper answers for it once anything polls.
async fn supervise_compile(state: HammerState, id: String, task: tokio::task::JoinHandle<()>) {
    if let Err(error) = task.await {
        if error.is_cancelled() {
            return;
        }
        // only run when the job is (still) running: the panicking task may have finished it first
        if let Some(job) = state.compile_jobs.get(&id) {
            if job.state != JobState::Running {
                return;
            }
            finish(
                &state,
                &id,
                JobState::Failed,
                None,
                None,
                Some(format!(
                    "the compile watcher panicked: {error} — the job was left running and is \
                     closed here; tagged compiler processes ({}) may still \
                     run in the container (the log shows what they printed)",
                    job_tag(&job.tag)
                )),
                None,
            );
        }
    }
}

/// Is there anything summary-like to report? (An OK summary counts, and so do `[FAIL]` lines and
/// a `FATAL` — attributable results of a run that did not finish cleanly.)
fn parse_summary(text: &str) -> Option<CompileSummary> {
    let summary = parse_compile_output(text);
    (summary.compiled.is_some() || summary.fatal || !summary.fail_lines.is_empty())
        .then_some(summary)
}

/// The OK case: the Wine process lingers after printing the line, so end the exec first (the
/// recipe's `kill "$RC_PID"` teardown — the observed exit 137), clean THIS job's container-side
/// tree with the tagged kill (M3), then decide from the output candidates against their
/// before-mtimes (M8).
async fn finish_ok(
    state: &HammerState,
    id: &str,
    tag: &str,
    container: &str,
    handle: &mut ExecHandle,
    log: &Path,
) {
    handle.kill();
    let _ = handle.withdrawn().await;
    let cleanup = Some(end_the_linger(state, container, &[tag.to_string()]).await);
    // output written after the OK line (the `VPK: Wrote file …` block) is part of the verdict
    let text = log_text(log);
    let summary = parse_compile_output(&text);
    let ok_line = find_ok_line(&text);
    let candidates = state
        .compile_jobs
        .get(id)
        .map(|job| job.vpk)
        .unwrap_or_default();
    let (job_state, reason) = ok_verdict(&candidates, &summary);
    finish(state, id, job_state, Some(summary), ok_line, reason, cleanup);
}

/// The host-side verdict after the compiler printed its `OK:` line (both routes): the output
/// candidates against their start mtimes, and the summary's failure count.
fn ok_verdict(candidates: &[VpkCandidate], summary: &CompileSummary) -> (JobState, Option<String>) {
    let chosen = vpk_choice(candidates);
    if !chosen.is_some_and(|(_, exists, _)| exists) {
        let both: Vec<String> = candidates
            .iter()
            .map(|c| c.path.display().to_string())
            .collect();
        return (
            JobState::Failed,
            Some(format!(
                "the OK line was seen but the output .vpk is missing at BOTH candidate \
                 locations ({both:?}; the recipe exits nonzero here: 'OK reported but output \
                 VPK missing')"
            )),
        );
    }
    if summary.failed.is_some_and(|n| n > 0) {
        return (
            JobState::Failed,
            Some(
                "the compiler reported failures (see failed/failLines) and wrote the vpk anyway: \
                 treat the output as partial; read the failure lines, fix and re-run"
                    .to_string(),
            ),
        );
    }
    if summary.compiled.is_none() {
        return (
            JobState::Failed,
            Some(
                "an OK-shaped line was seen but its `compiled/failed/skipped` summary could not \
                 be parsed"
                    .to_string(),
            ),
        );
    }
    let (index, _exists, advanced) = chosen.expect("checked exists");
    let path = candidates[index].path.display().to_string();
    let reason = (!advanced).then(|| {
        format!(
            "the summary counts this run OK but the vpk's mtime did not advance past the \
             start ({path}; every count may have been an up-to-date skip)"
        )
    });
    (JobState::Ok, reason)
}

/// The container-side cleanup of the tagged process tree(s) (M3, and N5's shutdown batching):
/// the TAGGED kill of exactly the given tags' trees — NEVER a bare `pkill -f
/// resourcecompiler.exe`, which would also hit a concurrent `model_compile`/`particle_compile`
/// and would never reach this job's `vrad3.exe` children. One exec covers ONE OR SEVERAL tags
/// (the shutdown cleanup batches a container's jobs into one script, since its budget cannot
/// afford one 1 s grace sleep per job). Fire-and-forget about verdicts, but the REPORT is
/// returned and stored on the job: when nothing in the container carries the tag, nothing is
/// killed and the result says so (`end_the_linger`'s report is the job's `cleanup`).
async fn end_the_linger(state: &HammerState, container: &str, tags: &[String]) -> Value {
    debug_assert!(!tags.is_empty(), "at least one tag to kill");
    let tag_report = if tags.len() == 1 {
        json!(job_tag(&tags[0]))
    } else {
        json!(tags.iter().map(|tag| job_tag(tag)).collect::<Vec<_>>())
    };
    let run = state
        .runtime
        .exec_in(container, &tagged_kill_argv(tags), LINGER_KILL_TIMEOUT)
        .await;
    match run {
        Ok(output) => {
            let text = combine(
                &String::from_utf8_lossy(&output.stdout),
                &String::from_utf8_lossy(&output.stderr),
            );
            static TAGGED_KILL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            let regex = TAGGED_KILL.get_or_init(|| {
                regex::Regex::new(r"tagged-kill: matched=(\d+) killed=(\d+) remaining=(\d+)")
                    .expect("tagged-kill report regex")
            });
            let parsed = regex.captures(&text).and_then(|capture| {
                let number = |index: usize| capture[index].parse::<u64>().ok();
                Some((number(1)?, number(2)?, number(3)?))
            });
            match parsed {
                Some((matched, killed, remaining)) => json!({
                    "tag": tag_report,
                    "matched": matched,
                    "killed": killed,
                    "remaining": remaining,
                    "note": if matched == 0 {
                        format!(
                            "no process in the container carries {}'s tag: NOTHING was killed",
                            if tags.len() == 1 { "this job" } else { "these jobs" }
                        )
                    } else {
                        "the tagged process tree was SIGTERMed, then SIGKILLed after 1 s".to_string()
                    },
                }),
                None => json!({
                    "tag": tag_report,
                    "matched": Value::Null,
                    "note": "the tagged kill's report could not be read (unexpected output \
                             below); NOTHING was confirmed killed",
                    "output": tail(&text, 20),
                }),
            }
        }
        Err(error) => json!({
            "tag": tag_report,
            "matched": Value::Null,
            "note": format!(
                "the tagged kill exec did not run ({error:#}); NOTHING was killed here — if \
                 this job's compiler processes are still alive in the container they carry the \
                 tag in /proc/*/environ"
            ),
        }),
    }
}

/// Write the final state of a job (the store entry is the only thing the poll reads).
fn finish(
    state: &HammerState,
    id: &str,
    job_state: JobState,
    summary: Option<CompileSummary>,
    ok_line: Option<String>,
    reason: Option<String>,
    cleanup: Option<Value>,
) {
    state.compile_jobs.update(id, |job| {
        job.state = job_state;
        if summary.is_some() {
            job.summary = summary.clone();
        }
        job.ok_line = ok_line;
        job.reason = reason;
        job.cleanup = cleanup;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use cs2_mcp_common::config::CommonConfig;

    /// A `HammerState` with defaults that point nowhere (the pure path helpers need no bridge,
    /// and `project_root` decides where `log_path` + `vpk_path` place their files).
    fn state() -> HammerState {
        state_at("/nonexistent/cs2mcp-compile-map-test")
    }

    fn state_at(project_root: &str) -> HammerState {
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: CommonConfig,
        }
        let argv: Vec<&str> = vec![
            "t",
            "--project-root",
            project_root,
            "--podman",
            "/nonexistent/cs2mcp-compile-map-test-podman",
            "--vk-layer-dir",
            "/nonexistent/cs2mcp-compile-map-test-vklayer",
            "--xauthority",
            "/nonexistent/cs2mcp-compile-map-test-xauth",
            "--discovery",
            "/nonexistent/cs2mcp-compile-map-test-discovery.json",
        ];
        HammerState::from_config(&W::parse_from(argv).c).expect("state")
    }

    #[test]
    fn ok_line_finds_the_cfled_leading_space_line() {
        let real = concat!(
            "-----------------------------------------------------------------\r\n",
            " OK: 19 compiled, 0 failed, 1 skipped, 2m:19s\r\n",
            "-----------------------------------------------------------------\r\n",
            "0488:fixme:kernelbase:AppPolicyGetProcessTerminationMethod …\r\n",
            "0488:fixme:kernelbase:AppPolicyGetProcessTerminationMethod …\n",
            "efixme wine noise without the CRLF (the kill cut the line)",
        );
        assert_eq!(
            find_ok_line(real).as_deref(),
            Some("OK: 19 compiled, 0 failed, 1 skipped, 2m:19s")
        );
        // the recipe's leading-space tolerance: a column-0 OK line is found too
        assert!(find_ok_line("OK: 2 compiled, 0 failed, 1 skipped, 0m:49s\r\n").is_some());
        // and nothing is found before the compiler reaches its summary
        assert_eq!(find_ok_line("Compiling maps/probe.vmap\r\n[FAIL] nope\r\n"), None);
        // the last one wins (a retried attempt keeps the first attempt's output)
        let two = " OK: 1 compiled, 0 failed, 0 skipped, 0m:01s\r\n OK: 3 compiled, 0 failed, 1 skipped\r\n";
        assert!(find_ok_line(two).unwrap().starts_with("OK: 3"));
    }

    #[test]
    fn parse_summary_likes_failure_lines_and_fatal() {
        let text = "[FAIL] dependency failed\nFATAL ERROR: bad gameinfo\n";
        let summary = parse_summary(text).expect("fail lines stand alone");
        assert_eq!(summary.compiled, None);
        assert!(summary.fatal);
        // a clean (partial) run output with counts
        let with_ok = " OK: 0 compiled, 0 failed, 1 skipped\r\n";
        let summary = parse_summary(with_ok).expect("the ok counts parse");
        assert_eq!(summary.compiled, Some(0));
    }

    #[test]
    fn the_deadline_defaults_and_caps() {
        assert_eq!(compile_deadline(&json!({})).unwrap(), Duration::from_millis(MAP_COMPILE_TIMEOUT_MS));
        assert_eq!(
            compile_deadline(&json!({"timeoutMs": 250_000})).unwrap(),
            Duration::from_millis(250_000)
        );
        // the cap is the maximum, budget-independent (the job runs in the background)
        assert_eq!(
            compile_deadline(&json!({"timeoutMs": MAP_COMPILE_MAX_MS * 99})).unwrap(),
            Duration::from_millis(MAP_COMPILE_MAX_MS)
        );
        for bad in [
            json!({"timeoutMs": 0}),
            json!({"timeoutMs": "1500"}),
            json!({"timeoutMs": 1.5}),
            json!({"timeoutMs": -4}),
        ] {
            assert!(compile_deadline(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_dirty_gate_refuses_only_the_active_document_case() {
        let dirty = json!({"dirty": true, "path": "z:\\app\\x.vmap"});
        let dirty = &dirty;
        let clean = json!({"dirty": false});
        let clean = &clean;
        let unknown = json!({});
        let unknown = &unknown;
        // active + dirty / dirty-unsaved-unknown -> UnsavedChanges
        for (doc, unsaved, name) in [
            (Some(dirty), 0u64, "some"),
            (Some(unknown), 0u64, "many"),
            (Some(clean), 3u64, "counter"),
        ] {
            let err = compile_refusal(doc, "maps/probe.vmap", true, unsaved).expect("refused");
            assert_eq!(err.name, "UnsavedChanges", "{name}");
            assert_eq!(err.code, Some(-32016));
            assert!(err.retry_safe, "{name}");
            assert!(err.message.contains("save_map"), "{name}: {err:?}");
            assert_eq!(
                err.data["unsavedBridgeEdits"],
                json!(unsaved),
                "{name}"
            );
        }
        // a clean document with no unsaved edits compiles
        assert!(compile_refusal(Some(clean), "maps/probe.vmap", true, 0).is_none());
        // a document that is NOT the target is never refused
        assert!(compile_refusal(Some(dirty), "maps/probe.vmap", false, 99).is_none());
        assert!(compile_refusal(None, "maps/probe.vmap", false, 1).is_none());
    }

    #[test]
    fn the_log_and_vpk_paths_follow_the_recipe() {
        let state = state();
        // compile_map.sh's LOGDIR with ONE LOG PER JOB (L3)
        assert_eq!(
            log_path(state.project_root.as_path(), "maps/probe.vmap", "c7"),
            state.project_root.join("target/compile-probe/rc-c7.log")
        );
        assert_eq!(
            log_path(state.project_root.as_path(), "maps/a/b/c.vmap", "c2"),
            state.project_root.join("target/compile-a-b-c/rc-c2.log")
        );
        // M8: BOTH output spellings, content-relative first, deduplicated for a top-level map
        let app_root = state.content().app_root.clone();
        let top = vpk_candidates(&app_root, "mcp_test", "maps/probe.vmap").expect("top-level");
        assert_eq!(top.len(), 1, "one deduped candidate for a top-level map");
        assert_eq!(top[0].form, "content-relative");
        assert_eq!(top[0].name, "probe");
        assert_eq!(
            top[0].path,
            state
                .content()
                .app_root
                .join("game/csgo_addons/mcp_test/maps/probe.vpk")
        );
        assert!(top[0].before.is_none(), "the helper does not read mtimes");
        let subdir = vpk_candidates(&app_root, "mcp_test", "maps/sub/arena.vmap").expect("subdir");
        assert_eq!(subdir.len(), 2, "both spellings for a sub-directory map");
        assert_eq!(subdir[0].form, "content-relative");
        assert_eq!(subdir[0].name, "sub/arena");
        assert_eq!(subdir[0].relative, "maps/sub/arena.vpk");
        assert_eq!(subdir[1].form, "flattened");
        assert_eq!(subdir[1].name, "arena");
        assert_eq!(subdir[1].relative, "maps/arena.vpk");
        assert_eq!(
            subdir[0].path,
            subdir[1].path.parent().unwrap().join("sub/arena.vpk"),
            "the two paths differ"
        );
        for bad in ["probe.vmap", "maps/.vmap", "maps/../x.vmap"] {
            assert!(vpk_candidates(&app_root, "mcp_test", bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_argv_is_the_recipe_verbatim_and_carries_the_job_tag() {
        let state = state();
        // M3: the tag export prefixes the recipe verbatim (`compile_map.sh`'s command outside)
        let full = compile_argv(&state.addon(), "maps/probe.vmap", CompileMode::Full, "c3");
        let line = full.last().expect("the bash -c command");
        assert!(
            line.starts_with(
                "export CS2MCP_COMPILE_JOB=c3; cd /app && ./entrypoint.sh resourcecompiler.exe"
            ),
            "{line}"
        );
        for flag in [
            "-threads 8",
            "-fshallow",
            "-maxtextureres 256",
            "-nop4",
            "-quiet",
            "-unbufferedio",
            "-noassert",
        ] {
            assert!(line.contains(flag), "{flag} in {line}");
        }
        assert!(line.ends_with("-i /app/content/csgo_addons/mcp_test/maps/probe.vmap"));
        assert!(!line.contains(" -game "), "the recipe leaves -game out");
        // nav adds EXTRA_FLAGS -nav in the same position
        let nav = compile_argv(&state.addon(), "maps/probe.vmap", CompileMode::Nav, "c3")
            .pop()
            .unwrap();
        assert!(nav.contains(" -noassert -nav -i "), "ordering: {nav}");
        // novis: the explicit stage list without -vis, on both routes
        let novis = compile_argv(&state.addon(), "maps/probe.vmap", CompileMode::NoVis, "c3")
            .pop()
            .unwrap();
        assert!(novis.contains(" -noassert -world -phys -bakelighting -nav -i "), "ordering: {novis}");
        assert!(!novis.contains("-vis"));
        let bridge = bridge_flags(CompileMode::NoVis);
        assert_eq!(bridge[bridge.len() - 4..], ["-world", "-phys", "-bakelighting", "-nav"]);
        assert!(CompileMode::arg(&json!({"mode": "novis"})).unwrap() == CompileMode::NoVis);
    }

    /// LIVE-P1 bug 1: the bridge refused `-world` etc. Every flag any mode / stage subset makes
    /// hammer-mcp send to `job.start` must pass the bridge's own flag parser.
    #[test]
    fn bridge_flags_are_all_in_the_job_allowlist() {
        let mut modes = vec![CompileMode::Full, CompileMode::Nav, CompileMode::NoVis];
        for bits in 1u8..(1 << STAGES.len()) {
            modes.push(CompileMode::Stages(StageSet::from_bits(bits)));
        }
        for mode in modes {
            let flags = bridge_flags(mode);
            if let Err(e) = cs2_bridge::ops::job::parse_flags(&flags) {
                panic!("{mode:?} sends {flags:?}: the bridge refuses it: {e}");
            }
        }
        for stage in STAGES {
            let flag = format!("-{stage}");
            assert!(cs2_bridge::ops::job::PLAIN_FLAGS.contains(&flag.as_str()), "{flag}");
        }
    }

    #[test]
    fn stages_resolve_to_compiler_order_and_validate() {
        let state = state();
        let mode = CompileMode::arg(&json!({"stages": ["nav", "world"]})).unwrap();
        assert_eq!(mode.stage_names(), ["world", "nav"]);
        let line = compile_argv(&state.addon(), "maps/probe.vmap", mode, "c3").pop().unwrap();
        assert!(line.contains(" -noassert -world -nav -i "), "{line}");
        let bridge = bridge_flags(mode);
        assert_eq!(bridge[bridge.len() - 2..], ["-world", "-nav"]);
        assert!(!mode.needs_existing_vpk());
        // duplicates fold, presets resolve to their lists, full reports the whole set
        let dup = CompileMode::arg(&json!({"stages": ["vis", "vis", "phys"]})).unwrap();
        assert_eq!(dup.stage_names(), ["phys", "vis"]);
        assert!(dup.needs_existing_vpk());
        assert_eq!(CompileMode::NoVis.stage_names(), ["world", "phys", "bakelighting", "nav"]);
        assert_eq!(CompileMode::Nav.stage_names(), ["nav"]);
        assert_eq!(CompileMode::Full.stage_names(), STAGES);
        assert!(CompileMode::Full.stage_flags().is_empty());
        assert_eq!(CompileMode::Full.stages_from(), "compiler default");
        assert_eq!(CompileMode::NoVis.stages_from(), "preset");
        let mixed = CompileMode::arg(&json!({"stages": ["NAV", "World"]})).unwrap();
        assert_eq!(mixed.stage_names(), ["world", "nav"]);
        assert_eq!(mixed.stages_from(), "explicit");
        assert!(CompileMode::Nav.needs_existing_vpk() && !CompileMode::NoVis.needs_existing_vpk());
        // a typo is InvalidParams with didYouMean
        let err = CompileMode::arg(&json!({"stages": ["vsi"]})).unwrap_err();
        assert_eq!(err.data["didYouMean"], json!("vis"), "{err:?}");
        let err = CompileMode::arg(&json!({"stages": ["lightmap"]})).unwrap_err();
        assert!(err.data["didYouMean"].is_null());
        // exclusive with mode; empty and non-array refused
        assert!(CompileMode::arg(&json!({"mode": "nav", "stages": ["nav"]})).is_err());
        assert!(CompileMode::arg(&json!({"stages": []})).is_err());
        assert!(CompileMode::arg(&json!({"stages": "nav"})).is_err());
    }

    #[test]
    fn the_vpk_choice_reports_the_refreshed_candidate_or_both() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp")
            .join(format!("compile-map-choice-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("maps/sub")).unwrap();
        let flat = dir.join("maps/arena.vpk");
        let nested = dir.join("maps/sub/arena.vpk");
        let fresh = |path: &Path| {
            std::thread::sleep(Duration::from_millis(30));
            std::fs::write(path, b"x").unwrap();
            mtime_of(path)
        };
        // neither exists: the first candidate is still reported, nothing advanced
        let forms = vec![
            mk("content-relative", &nested, None),
            mk("flattened", &flat, None),
        ];
        let section = vpk_section(&forms);
        assert_eq!(
            section["vpkPath"],
            json!(nested.display().to_string()),
            "the first candidate stands in when neither exists"
        );
        assert_eq!(section["vpkExists"], json!(false));
        assert_eq!(section["vpkUpdated"], json!(false));
        // the flattened one exists, mtime unchanged: chosen (the first existing), NOT advanced
        let unchanged = fresh(&flat);
        let forms = vec![
            mk("content-relative", &nested, None),
            mk("flattened", &flat, unchanged),
        ];
        let section = vpk_section(&forms);
        assert_eq!(section["vpkForm"], json!("flattened"), "the existing one");
        assert_eq!(section["vpkUpdated"], json!(false), "mtime unchanged");
        // ... and when IT has advanced, it is chosen over an existing-but-stale sibling
        let was = unchanged;
        let refreshed = fresh(&flat);
        assert!(refreshed > was, "the refresh bumped the mtime");
        let forms = vec![
            mk("flattened", &flat, was),
            mk("content-relative", &nested, None),
        ];
        let section = vpk_section(&forms);
        assert_eq!(
            section["vpkPath"],
            json!(flat.display().to_string()),
            "the advanced mtime decides"
        );
        assert_eq!(section["vpkForm"], json!("flattened"));
        assert_eq!(section["vpkUpdated"], json!(true));
        assert_eq!(
            section["vpkCandidates"].as_array().unwrap().len(),
            2,
            "both candidates stay visible"
        );
        // the content-relative one advanced: it wins even though the flattened one also exists
        let nested_before = fresh(&nested);
        let _ = fresh(&nested);
        let forms = vec![
            mk("content-relative", &nested, nested_before),
            mk("flattened", &flat, refreshed),
        ];
        assert_eq!(
            vpk_section(&forms)["vpkForm"],
            json!("content-relative"),
            "the refreshed form wins"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn mk(form: &'static str, path: &Path, before: Option<SystemTime>) -> VpkCandidate {
        VpkCandidate {
            name: path.file_stem().unwrap().to_string_lossy().into_owned(),
            relative: format!("maps/{}", path.file_name().unwrap().to_string_lossy()),
            form,
            path: path.to_path_buf(),
            before,
        }
    }

    #[test]
    fn two_concurrent_starts_cannot_both_insert_and_dead_jobs_are_reaped() {
        // L7: `start` is the atomic check-and-insert; M4: a Running job past deadline+grace
        // cannot hold the server hostage.
        let jobs = CompileJobs::new();
        let job = |id: &str, state: JobState, started: Instant| CompileJob {
            id: id.to_string(),
            tag: tag_value(id),
            state,
            mode: CompileMode::Full,
            path: "maps/probe.vmap".into(),
            addon: "mcp_test".to_string(),
            container: "cs2tools".into(),
            log: "/log".into(),
            vpk: vec![mock_candidate("content-relative", "/vpk")],
            vmap_before: None,
            started,
            deadline: MAP_COMPILE_POLL,
            attempts: 1,
            summary: None,
            ok_line: None,
            reason: None,
            cleanup: None,
        };
        let stale_started = Instant::now() - (MAP_COMPILE_POLL + DEAD_GRACE + Duration::from_secs(30));
        jobs.insert(job("stale", JobState::Running, stale_started));
        // the reaper runs before anything reads the running list: a fresh start succeeds
        let fresh_start = jobs.start(|id| job(id, JobState::Running, Instant::now())).expect("the stale job is reaped");
        assert!(
            fresh_start != "stale",
            "the id comes from the store's counter"
        );
        let reaped = jobs.get("stale").expect("kept in history");
        assert_eq!(reaped.state, JobState::Failed);
        assert!(
            reaped.reason.as_deref().unwrap().contains("reap"),
            "the poll answers honestly: {reaped:?}"
        );
        // the LIVE running job refuses the second start, naming its id (same lock, both halves)
        let err = jobs
            .start(|id| job(id, JobState::Running, Instant::now()))
            .expect_err("refused");
        assert_eq!(err, StartRefusal::Running(fresh_start.clone()));
        // once the job finishes, start works again
        jobs.update(&fresh_start, |job| job.state = JobState::Ok);
        assert!(jobs.start(|id| job(id, JobState::Running, Instant::now())).is_ok());
        // N7: once the shutdown cleanup began, NO new start is accepted (it would be orphaned
        // by the exit its cleanup precedes)
        jobs.begin_shutdown();
        assert_eq!(
            jobs.start(|id| job(id, JobState::Running, Instant::now()))
                .unwrap_err(),
            StartRefusal::ShuttingDown
        );
    }

    #[test]
    fn a_bridge_route_job_the_bridge_never_confirmed_is_closed_outcome_unknown() {
        // a lost bridge: the entry never reached a confirmed state; past deadline + grace the
        // reaper closes it failed, honestly worded (it is NOT a compiler verdict)
        let jobs = CompileJobs::new();
        let started = Instant::now() - (Duration::from_secs(1) + DEAD_GRACE + Duration::from_secs(30));
        let id = jobs
            .start_with(
                |id| CompileJob {
                    id: id.to_string(),
                    tag: String::new(),
                    state: JobState::Running,
                    mode: CompileMode::Full,
                    path: "maps/probe.vmap".into(),
                    addon: "mcp_test".to_string(),
                    container: String::new(),
                    log: PathBuf::new(),
                    vpk: vec![],
                    vmap_before: None,
                    started,
                    deadline: Duration::from_secs(1),
                    attempts: 1,
                    summary: None,
                    ok_line: None,
                    reason: None,
                    cleanup: None,
                },
                Some(BridgeRef { bridge_id: "j1-x".into(), ..BridgeRef::default() }),
            )
            .expect("start");
        // a store read alone never reaps a bridge entry (the compile may have finished unpolled)
        jobs.reap();
        assert!(jobs.running().is_some(), "still Running after plain store reads");
        assert_eq!(jobs.get(&id).unwrap().state, JobState::Running);
        // only the sync path that failed to reach the bridge closes it
        jobs.reap_bridge(&id);
        let job = jobs.get(&id).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert!(job.reason.as_deref().unwrap().contains("outcome unknown"), "{job:?}");
        assert!(job.reason.as_deref().unwrap().contains("bridge unreachable"));
        assert!(jobs.bridge_ref(&id).unwrap().reaped);
    }

    /// An id-less (lost `job.start` reply) bridge entry of a compile started `age` ago.
    fn idless_entry(state: &HammerState, age: Duration) -> String {
        state
            .compile_jobs
            .start_with(
                |id| CompileJob {
                    id: id.to_string(),
                    tag: String::new(),
                    state: JobState::Running,
                    mode: CompileMode::Full,
                    path: "maps/probe.vmap".into(),
                    addon: "mcp_test".to_string(),
                    container: String::new(),
                    log: PathBuf::new(),
                    vpk: vec![],
                    vmap_before: None,
                    started: Instant::now() - age,
                    deadline: Duration::from_secs(1500),
                    attempts: 1,
                    summary: None,
                    ok_line: None,
                    reason: None,
                    cleanup: None,
                },
                Some(BridgeRef { client_ref: "hm1-1".into(), adopting: true, ..BridgeRef::default() }),
            )
            .expect("start")
    }

    #[tokio::test]
    async fn an_idless_entry_on_an_unreachable_bridge_is_given_up_after_the_wait() {
        // N2: a lost start reply plus a bridge that stays unreachable must not block every
        // compile start for the deadline + grace: past ADOPT_GIVE_UP it is closed "outcome unknown"
        let state = state();
        let young = idless_entry(&state, Duration::from_secs(5));
        assert!(adopt_entry(&state, &young).await.is_err(), "no bridge to ask");
        assert_eq!(state.compile_jobs.get(&young).unwrap().state, JobState::Running, "still waiting");
        state.compile_jobs.remove(&young);
        let old = idless_entry(&state, ADOPT_GIVE_UP + Duration::from_secs(5));
        assert!(adopt_entry(&state, &old).await.is_err());
        let job = state.compile_jobs.get(&old).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert!(job.reason.as_deref().unwrap().contains("outcome unknown (bridge unreachable)"), "{job:?}");
        assert!(state.compile_jobs.running().is_none(), "the start is unblocked");
    }

    #[tokio::test]
    async fn a_terminal_bridge_status_overwrites_a_reaped_outcome_unknown_entry() {
        // N3: the reaper's verdict is a guess; the bridge's real answer replaces it
        let state = state();
        let id = idless_entry(&state, ADOPT_GIVE_UP + Duration::from_secs(5));
        let _ = adopt_entry(&state, &id).await;
        assert!(state.compile_jobs.bridge_ref(&id).unwrap().reaped);
        apply_bridge_status(&state, &id, &json!({"state": "cancelled", "logTail": ""}));
        assert_eq!(state.compile_jobs.get(&id).unwrap().state, JobState::Cancelled);
        assert!(!state.compile_jobs.bridge_ref(&id).unwrap().reaped);
        // a finished entry is not rewritten by a later status
        apply_bridge_status(&state, &id, &json!({"state": "failed", "logTail": ""}));
        assert_eq!(state.compile_jobs.get(&id).unwrap().state, JobState::Cancelled);
    }

    #[test]
    fn an_ok_status_without_a_summary_is_judged_from_the_relative_log() {
        // live 2026-10-29: the raw job.status reply has only `log.relative`; its tail lacked the
        // OK line, so the job closed "summary could not be parsed" although the log had it
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp")
            .join(format!("compile-map-rel-log-{}", std::process::id()));
        let logs = root.join("workshop/run/app/cs2mcp_jobs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("j6-x.log"), " OK: 17 compiled, 0 failed, 1 skipped, 0m:48s\r\n").unwrap();
        let state = state_at(root.to_str().unwrap());
        let id = idless_entry(&state, Duration::from_secs(1));
        let status = json!({
            "state": "ok",
            "logTail": "... Building 'nav'",
            "log": {"windows": "Z:\\app\\cs2mcp_jobs\\j6-x.log", "relative": "cs2mcp_jobs/j6-x.log"},
        });
        apply_bridge_status(&state, &id, &status);
        let job = state.compile_jobs.get(&id).unwrap();
        assert_eq!(job.summary.as_ref().and_then(|s| s.compiled), Some(17), "{job:?}");
        assert!(!job.reason.as_deref().unwrap_or("").contains("could not be parsed"), "{job:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cancel_is_pended_or_answered_with_the_id_in_one_step() {
        // #7
        let jobs = CompileJobs::new();
        let state = state();
        let id = idless_entry(&state, Duration::from_secs(1));
        assert_eq!(state.compile_jobs.bridge_id_or_pend_cancel(&id), None);
        assert!(state.compile_jobs.bridge_ref(&id).unwrap().pending_cancel);
        state.compile_jobs.update_bridge(&id, |b| b.bridge_id = "j1-x".into());
        assert_eq!(state.compile_jobs.bridge_id_or_pend_cancel(&id).as_deref(), Some("j1-x"));
        assert_eq!(jobs.bridge_id_or_pend_cancel("c9"), None, "no such entry");
    }

    fn mock_candidate(form: &'static str, path: &str) -> VpkCandidate {
        VpkCandidate {
            name: path.to_string(),
            relative: path.to_string(),
            form,
            path: path.into(),
            before: None,
        }
    }

    #[test]
    fn an_exit_between_the_log_read_and_the_check_still_reports_ok() {
        // M2: the exit fold re-reads the log; only a log that really lacks the line fails.
        let ok_text = "compiling\r\n OK: 3 compiled, 0 failed, 1 skipped, 0m:05s\r\n";
        assert!(matches!(fold_exit(ok_text, 1, "exit status: 0"), ExitFold::Ok));
        assert!(matches!(fold_exit(ok_text, 2, "exit status: 0"), ExitFold::Ok));
        // the transient crun exec failure retries exactly once
        let crun_text = "crun: error opening file /run/user/1000/.../status: No such file or directory";
        match fold_exit(crun_text, 1, "exit status: 1") {
            ExitFold::RetryCrun => {}
            other => panic!("attempt 1 must retry: {other:?}"),
        }
        match fold_exit(crun_text, 2, "exit status: 1") {
            ExitFold::Failed { reason, .. } => {
                assert!(reason.contains("without an OK line"), "{reason}");
                assert!(reason.contains("2 attempt(s)"), "{reason}");
            }
            other => panic!("attempt 2 is final: {other:?}"),
        }
        // a death without an OK line: failed with whatever summary the log shows
        let death = "[FAIL] dependency failed\nFATAL ERROR: bad\n";
        match fold_exit(death, 1, "exit status: 3") {
            ExitFold::Failed { summary, reason } => {
                let summary = summary.expect("the fail lines stand alone");
                assert!(summary.fatal);
                assert!(reason.contains("exit status: 3"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_log_reads_are_lossy_and_incremental() {
        // M1: one non-UTF-8 byte must not blank the log (read_to_string would return Err and
        // the OK line would never be found: a 25-minute false timeout on a succeeded compile).
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp")
            .join(format!("compile-map-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("rc.log");
        std::fs::write(
            &log,
            ["first line\r\n", " OK: 1 compiled, 0 failed, 0 skipped\r\n"]
                .join(""),
        )
        .unwrap();
        // the invalid byte appends through the kernel (not a Rust string literal)
        {
            let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
            std::io::Write::write_all(&mut file, &[0xFF, b'\n'])
                .expect("a raw non-UTF-8 byte");
        }
        let text = log_text(&log);
        assert!(find_ok_line(&text).is_some(), "the OK line survives: {text:?}");
        assert!(text.contains(char::REPLACEMENT_CHARACTER), "lossy: {text:?}");

        // the incremental poll reads only the appended-and-newline-terminated bytes
        let mut offset: u64 = 0;
        let first = appended_log_text(&log, &mut offset);
        assert!(first.contains("first line"), "the whole preamble: {first:?}");
        assert!(offset > 0, "the offset advanced");
        assert!(find_ok_line(&first).is_some(), "the append carried the line");
        assert_eq!(appended_log_text(&log, &mut offset), "", "nothing new after");
        // a partial final line is left for the next poll
        {
            let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
            std::io::Write::write_all(&mut file, b"part").unwrap();
        }
        assert_eq!(appended_log_text(&log, &mut offset), "", "no newline, no read");
        {
            let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
            std::io::Write::write_all(&mut file, b"ial  OK: 2 compiled\r\n").unwrap();
        }
        let more = appended_log_text(&log, &mut offset);
        assert_eq!(more, "partial  OK: 2 compiled\r\n", "the line completed: {more:?}");
        // a recreated (smaller) file restarts at 0
        std::fs::write(&log, b"recreated\r\n").unwrap();
        let again = appended_log_text(&log, &mut offset);
        assert_eq!(again, "recreated\r\n", "{again:?}");
        // a missing file reads empty without touching the offset
        assert_eq!(appended_log_text(&dir.join("nope.log"), &mut offset), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_mounted_map_refusal_matches_a_loaded_map_only() {
        // M9: in game + the target's engine name -> the refusal. The full spelling (`sub/arena`,
        // case-insensitive) always matches; the bare stem (`arena`) matches only when it is NOT
        // ambiguous — a top-level maps/arena.vmap next to it means the engine's map could be
        // THAT one (review-2 M9).
        let target = "maps/sub/arena.vmap";
        let names = engine_names(target);
        for game_map in ["sub/arena", "Sub/Arena"] {
            let reply = json!({"inGame": true, "map": game_map});
            let err = mounted_refusal(&reply, &names, target, false, "game.local: inGame=true")
                .unwrap_or_else(|| panic!("{game_map:?} should count as this map"));
            assert_eq!(err.name, "EngineBusy");
            assert!(err.message.contains("disconnect"), "{err:?}");
            assert!(err.retry_safe);
            assert_eq!(err.data["reason"], json!("map_in_game"));
            assert!(
                err.message.contains("game.local"),
                "the source is named: {err:?}"
            );
        }
        // the unambiguous bare stem: the flattened candidate can only be this map's
        let reply = json!({"inGame": true, "map": "arena"});
        let err = mounted_refusal(&reply, &names, target, false, "game.local: inGame=true")
            .expect("an unambiguous stem refuses");
        assert_eq!(err.name, "EngineBusy", "{err:?}");
        // ... and the AMBIGUOUS one does not (a top-level source exists: the reviewer's note case)
        assert!(mounted_refusal(&reply, &names, target, true, "game.local: inGame=true").is_none());
        // a different map (or nothing) in game is not a refusal
        for reply in [
            json!({"inGame": true, "map": "probe"}),
            json!({"inGame": true, "map": "other/sub/arena"}),
            json!({"inGame": false, "map": "arena"}),
            json!({"map": "arena"}),
            json!({}),
        ] {
            assert!(
                mounted_refusal(&reply, &names, target, false, "game.local: inGame=true")
                    .is_none(),
                "{reply}"
            );
        }
        // a top-level map has no stem, so its one name always matches exactly
        let top = engine_names("maps/probe.vmap");
        assert!(top.stem.is_none(), "{top:?}");
        assert!(
            mounted_refusal(
                &json!({"inGame": true, "map": "probe"}),
                &top,
                "maps/probe.vmap",
                false,
                "game.local: inGame=true"
            )
            .is_some()
        );
        // and a refusal built from this server's own load memory names THAT as its source
        let err = mounted_refusal(
            &json!({"inGame": true, "map": "sub/arena", "source": "previous-load"}),
            &names,
            target,
            false,
            "this server's last run_map load (no game.local exposed)",
        )
        .expect("the memory fallback refuses the same way");
        assert!(
            err.message.contains("last run_map load"),
            "the fallback says where its answer came from: {err:?}"
        );
        // the two spellings the mounted check compares against
        let sub = engine_names("maps/sub/arena.vmap");
        assert_eq!(sub.full, vec!["sub/arena"]);
        assert_eq!(sub.stem.as_deref(), Some("arena"));
    }

    #[test]
    fn the_tagged_kill_script_targets_one_job_and_reports() {
        // M3 (+N6): never a bare `pkill -f resourcecompiler.exe` — the script matches the tag as
        // an exact token, reads the environ ANCHORED (`^CS2MCP_COMPILE_JOB=`), excludes itself,
        // excludes the Wine bootstrap processes by comm, polls the non-zombie count after the
        // SIGKILL, and prints a parseable report.
        let script = tagged_kill_script(&["c12".to_string()]);
        assert!(!script.contains("pkill"), "{script}");
        assert!(
            script.contains("grep -E -m1 '^CS2MCP_COMPILE_JOB='"),
            "an anchored read, not a substring search: {script}"
        );
        assert!(
            script.contains(r#"[ "$t" = "$want" ]"#),
            "the tag matches as an exact token: {script}"
        );
        assert!(script.contains("wineserver"), "the Wine bootstrap exclusion: {script}");
        assert!(script.contains("services.exe"), "{script}");
        assert!(script.contains("$$"), "it excludes itself");
        assert!(script.contains("kill -TERM"), "TERM first");
        assert!(script.contains("kill -KILL"), "KILL after grace");
        assert!(
            script.contains("^State:") && script.contains("[ -n \"$s\" ] && [ \"$s\" != Z ]"),
            "zombies AND exited pids (no /proc status) are excluded from the count (live: an exited \
             pid was counted as remaining=1): {script}"
        );
        assert!(script.contains("sleep 0.1"), "the bounded re-poll: {script}");
        assert!(script.contains("tagged-kill: matched=$matched killed=$matched remaining=$remaining"));
        assert!(tagged_kill_argv(&["c12".to_string()])[2].contains("for want in c12"));
        // several tags ride in the ONE script (the shutdown batching, N5/N6)
        let multi = tagged_kill_script(&["c1-1-a".to_string(), "c2-2-b".to_string()]);
        assert!(multi.contains("for want in c1-1-a c2-2-b"), "{multi}");
    }

    #[test]
    fn the_tagged_kill_script_kills_only_the_tagged_tree() {
        // The script RUNS on the host with unique tags: two processes carry this job's tag, one
        // carries a different job's tag (the concurrent-compile case), and the untagged shell
        // survives. The tagged ones are SIGTERMed/SIGKILLed; the report is parsed like
        // end_the_linger. The killed sleepers stay ZOMBIES of this test process (nobody reaps
        // them here) — which is the live lesson (review-2 N6 + the STATUS "remaining":1): the
        // zombie state must be IGNORED, so the reported `remaining` is 0.
        let tag_value = format!("cfix{}a", std::process::id());
        let other_value = format!("cfix{}b", std::process::id());
        let spawn = |value: Option<&str>| {
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-c").arg("exec sleep 30");
            if let Some(value) = value {
                cmd.env("CS2MCP_COMPILE_JOB", value);
            }
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("tagged sleeper")
        };
        let mut sleepers = [
            spawn(Some(&tag_value)),
            spawn(Some(&tag_value)),
            spawn(Some(&other_value)),
        ];
        std::thread::sleep(Duration::from_millis(150)); // the pids are listenable
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(tagged_kill_script(std::slice::from_ref(&tag_value)))
            .output()
            .expect("the kill script ran");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains("tagged-kill: matched=2 killed=2 remaining=0"),
            "both tagged sleepers were matched and killed, and the (unreaped) zombies they \
             became did not count as remaining: {text:?}"
        );
        std::thread::sleep(Duration::from_millis(150));
        // the children are genuinely dead (their /proc entries only linger as zombies now)
        for (index, note) in [(0usize, "tagged sleeper 1"), (1, "tagged sleeper 2")] {
            let status = sleepers[index]
                .try_wait()
                .expect("try_wait")
                .unwrap_or_else(|| panic!("{note} is still running"));
            assert!(status.code().is_none() || status.code() != Some(0), "{note}");
        }
        assert!(
            sleepers[2].try_wait().expect("try_wait").is_none(),
            "the OTHER job's process survived the kill"
        );
        // cleanup the survivor
        let _ = sleepers[2].kill();
        let _ = sleepers[2].wait();
        for sleeper in sleepers.iter_mut() {
            let _ = sleeper.wait();
        }
        // and the matched-nothing run is a clean report, not an error
        let empty = tagged_kill_script(&["cnothing".to_string()]);
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&empty)
            .output()
            .expect("the empty run");
        assert!(String::from_utf8_lossy(&output.stdout)
            .contains("tagged-kill: matched=0 killed=0 remaining=0"));
    }

    #[test]
    fn jobs_eject_the_oldest_finished_only() {
        let jobs = CompileJobs::new();
        let job = |id: &str, state: JobState| CompileJob {
            id: id.to_string(),
            tag: tag_value(id),
            state,
            mode: CompileMode::Full,
            path: "maps/probe.vmap".into(),
            addon: "mcp_test".to_string(),
            container: "cs2tools".into(),
            log: "/log".into(),
            vpk: vec![mock_candidate("content-relative", "/vpk")],
            vmap_before: None,
            started: Instant::now(),
            deadline: MAP_COMPILE_POLL,
            attempts: 1,
            summary: None,
            ok_line: None,
            reason: None,
            cleanup: None,
        };
        // a full history of finished jobs...
        for n in 0..MAX_JOBS {
            jobs.insert(job(&format!("c{n}"), JobState::Ok));
        }
        assert_eq!(jobs.ids().len(), MAX_JOBS, "the store is bounded");
        assert!(jobs.get("c0").is_some());
        // ... a running insert ejects the OLDEST FINISHED, never the running one
        jobs.insert(job("run", JobState::Running));
        assert_eq!(jobs.ids().len(), MAX_JOBS, "still bounded");
        assert_eq!(jobs.running().as_deref(), Some("run"), "the running job is found");
        assert!(jobs.get("c0").is_none(), "the oldest finished is gone");
        assert!(jobs.get("c1").is_some(), "the others are kept");
        // a further insert ejects the next-oldest finished
        jobs.insert(job("c9", JobState::Ok));
        assert!(jobs.get("c1").is_none());
        assert!(jobs.get(&format!("c{}", MAX_JOBS - 2)).is_some(), "the newer were kept");
        assert_eq!(jobs.running().as_deref(), Some("run"));
    }

    /// (a) the orphan scan: the script reads each `/proc/*/environ` with the read's stderr
    /// redirected (the podman-exec user gets `Permission denied` on the game's processes: they must
    /// stay silent and NOT be counted), matches the tag as an exact token, and prints one parseable
    /// report line.
    #[test]
    fn the_tagged_scan_script_is_silent_about_unreadable_processes_and_reports() {
        let script = tagged_scan_script();
        assert!(
            script.contains("<\"$d/environ\" 2>/dev/null"),
            "an unreadable environ must be silent, not an error: {script}"
        );
        assert!(!script.contains("pkill"), "{script}");
        assert!(!script.contains("kill -TERM"), "the scan kills nothing: {script}");
        assert!(
            script.contains("grep -E -m1 '^CS2MCP_COMPILE_JOB='"),
            "an anchored read, not a substring search (an `OTHER_CS2MCP_COMPILE_JOB=…` \
             environ entry must not read as a foreign tag): {script}"
        );
        assert!(
            script.contains("wineserver") && script.contains("services.exe"),
            "the Wine bootstrap exclusions (a tagged wineserver would look like an eternal \
             orphan): {script}"
        );
        assert!(script.contains("self=$$"), "it excludes itself: {script}");
        assert!(script.contains("tagged-scan: jobs=$seen"), "the report line: {script}");
        // the script really runs: a tagged sleeper (NOT this bash, which excludes itself) is found
        // through its /proc environ on the host
        let value = format!("scan{}", std::process::id());
        let mut sleeper = std::process::Command::new("sh")
            .arg("-c")
            .arg("exec sleep 10")
            .env("CS2MCP_COMPILE_JOB", &value)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("tagged sleeper");
        std::thread::sleep(Duration::from_millis(150));
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("the scan script ran");
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let tags = parse_tagged_scan(&text).expect("the report line");
        assert!(
            tags.contains(&value),
            "the scan reads the tag out of a /proc environ: {text:?}"
        );
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        // ... and it is not the kill script
        assert!(!tagged_kill_script(std::slice::from_ref(&value)).contains("tagged-scan"));
    }

    /// The scan's report line -> tags: absent (unparsable) is `None`, duplicates are dropped, and
    /// an empty report is `Some([])` (nothing carries a tag) — which is not the same as "the scan
    /// could not run".
    #[test]
    fn the_scan_report_parses_into_tags() {
        assert_eq!(parse_tagged_scan("tagged-scan: jobs=\n"), Some(vec![]));
        assert_eq!(
            parse_tagged_scan("noise\ntagged-scan: jobs=c1 c7 c1\nmore noise\n"),
            Some(vec!["c1".to_string(), "c7".to_string()])
        );
        assert_eq!(parse_tagged_scan("bash: /proc: No such file\n"), None);
        assert_eq!(parse_tagged_scan(""), None);
    }

    /// The classification the start acts on: an exact TAG VALUE this server's store knows is its
    /// own (killable), anything else is foreign (reported, refused, never killed). The N1 shape:
    /// a tag `c1-<other pid>-<nonce>` is NOT this server's `c1`, even though the short job id
    /// matches.
    #[test]
    fn the_tagged_scan_splits_own_jobs_from_foreign_ones() {
        let own_tag = "c1-32251-abc".to_string();
        let other_own = "c9-42-def".to_string();
        let known = vec![own_tag.clone(), other_own.clone()];
        // a foreign server's own job ALSO numbering its compile `c1`: a different pid+nonce, so
        // a different tag — never classified as ours
        let alien = format!("c1-{}-deadbeef", std::process::id() + 1);
        let alive = vec![own_tag.clone(), alien.clone(), other_own.clone()];
        let tagged = classify_tagged_jobs(&alive, &known);
        assert_eq!(tagged.own, vec![own_tag.clone(), other_own.clone()]);
        assert_eq!(tagged.foreign, vec![alien.clone()]);
        // nothing alive, or nothing known: both halves empty / all foreign
        assert_eq!(
            classify_tagged_jobs(&[], &known),
            TaggedJobs::default()
        );
        assert_eq!(
            classify_tagged_jobs(std::slice::from_ref(&alien), &[]).foreign,
            vec![alien.clone()]
        );
        // the refusal names the tag and the way out, in the report shape the caller reads
        let err = foreign_compile_job_error(std::slice::from_ref(&alien));
        assert_eq!(err.name, "EngineBusy");
        assert!(err.retry_safe);
        assert_eq!(err.data["reason"], json!("foreign_compile_job"));
        assert_eq!(
            err.data["tags"],
            json!([format!("CS2MCP_COMPILE_JOB={alien}")])
        );
        assert!(
            err.message.contains(&format!("CS2MCP_COMPILE_JOB={alien}")),
            "{err:?}"
        );
        assert!(
            err.message.contains("does not kill it"),
            "the no-auto-kill rule is stated: {err:?}"
        );
    }

    /// (a)/(e): the store's readers the new paths use — `jobs_needing_cleanup` (the shutdown
    /// cleanup, review-2 N5: ANY job left without a cleanup report, `Running` or not) and
    /// `running_for_path` (the save/reload note) — with the L2 comparison case-insensitive (both
    /// spellings come from the same tool gate and Windows paths compare that way).
    #[test]
    fn the_store_answers_needing_cleanup_and_running_for_a_map() {
        let jobs = CompileJobs::new();
        let job = |id: &str, state: JobState, path: &str| CompileJob {
            id: id.to_string(),
            tag: tag_value(id),
            state,
            mode: CompileMode::Full,
            path: path.to_string(),
            addon: "mcp_test".to_string(),
            container: "cs2tools".into(),
            log: "/log".into(),
            vpk: vec![mock_candidate("content-relative", "/vpk")],
            vmap_before: None,
            started: Instant::now(),
            deadline: Duration::from_secs(1500),
            attempts: 1,
            summary: None,
            ok_line: None,
            reason: None,
            cleanup: None,
        };
        assert!(jobs.jobs_needing_cleanup().is_empty());
        assert!(jobs.running_for_path("mcp_test", "maps/probe.vmap").is_none());
        jobs.insert(job("c1", JobState::Running, "maps/Probe.vmap"));
        jobs.insert(job("c2", JobState::Ok, "maps/other.vmap"));
        let needing = jobs.jobs_needing_cleanup();
        assert_eq!(needing.len(), 2, "both are left without a report: {needing:?}");
        assert_eq!(needing[0].id, "c1");
        assert_eq!(needing[0].container, "cs2tools", "the cleanup needs the container");
        assert_eq!(
            needing[1].state,
            JobState::Ok,
            "a job closed without a cleanup needs the kill too (N5: its tree may still be alive)"
        );
        assert_eq!(
            jobs.running_for_path("mcp_test", "maps/probe.vmap").as_deref(),
            Some("c1"),
            "the comparison is case-insensitive (Windows paths)"
        );
        assert!(jobs.running_for_path("mcp_test", "maps/sub/probe.vmap").is_none());
        // a FINISHED job for the same map is not "running": no note, no kill
        jobs.update("c1", |job| job.state = JobState::Ok);
        assert!(jobs.running_for_path("mcp_test", "maps/probe.vmap").is_none());
        // ... and N5's other half: once this job carries a cleanup report (its OK/timeout
        // teardown ran) the shutdown has nothing left to guess at
        assert_eq!(jobs.jobs_needing_cleanup().len(), 2);
        jobs.update("c1", |job| job.cleanup = Some(json!({"matched": 1})));
        let left = jobs.jobs_needing_cleanup();
        assert_eq!(left.len(), 1, "{left:?}");
        assert_eq!(left[0].id, "c2");
    }

    /// (a) the shutdown cleanup's report shape with no job at all: no exec, an explicit note. (The
    /// job-path report is exercised against the fake podman in `tests/compile_map.rs`.)
    #[tokio::test]
    async fn the_shutdown_cleanup_with_no_running_job_does_nothing() {
        let state = state();
        let report = shutdown_cleanup(&state).await;
        assert_eq!(report["jobs"], json!([]));
        assert!(report["note"].as_str().unwrap().contains("nothing to kill"));
    }
}

