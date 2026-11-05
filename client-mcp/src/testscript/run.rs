//! Runs behind `run_testscript` / `test_status` / `test_wait`.
//!
//! A [`Run`] is one staged suite in one wine-tools session. [`spawn_drive`] starts the background
//! task: [`drive`] follows console.log (through [`ConsoleLog::read_new_lines`]), records checkpoint,
//! quit and fatal lines as a timeline, and watches the container. When the container exits (or the
//! run times out) it copies the evidence into the artifacts dir, prunes the stale `vtest_*` files
//! from the game dir, and judges. The state lives in [`RunTable`], which `ClientState` owns.
//!
//! Lifetime: runs live in this server process. When the server exits, [`stop_running`] removes the
//! containers of runs still in progress (best effort); finished runs and their artifacts stay on disk.
use crate::console::{ConsoleLog, Cursor};
use crate::testscript::judge::{self, Check, Evidence, Verdict};
use crate::testscript::HANG_GUARD_EXIT;
use crate::video::judge::{self as movie_judge, MovieExpect};
use cs2_mcp_common::runtime::docker::{ContainerState, DockerRuntime, StopOutcome};
use regex::Regex;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

/// Once the engine logs its quit request, the process should be gone within this long. The suite's
/// own hang guard (`Test_ExitProcess 3`) fires after `Test_Wait 30`, inside this grace.
pub const QUIT_GRACE: Duration = Duration::from_secs(45);
pub const POLL: Duration = Duration::from_millis(250);
/// Finished runs kept in the table (the oldest are dropped on insert).
pub const KEEP_FINISHED: usize = 64;
/// Timeline events kept per run; later events set `timelineTruncated`.
pub const TIMELINE_CAP: usize = 2000;

/// Evidence extensions: PNGs from `csgo/screenshots/`, media from `csgo/`; `vtest_*` names only.
const SCREENSHOT_EXT: &[&str] = &["png"];
const MEDIA_EXT: &[&str] = &["dem", "webm", "mp4", "wav"];

static CHECKPOINT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"TESTSCRIPT: CheckPoint -> '([^']+)'").expect("regex"));
/// `[HostStateManager] CHostStateMgr::QueueNewRequest( Quitting, N )` (seen in every clean exit).
static QUIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"HostStateManager\] CHostStateMgr::QueueNewRequest\( Quitting").expect("regex")
});
static FATAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Plat_FatalError|FATAL|Error starting testscript").expect("regex")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Running,
    Passed,
    Failed,
    Error,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::Passed => "passed",
            RunState::Failed => "failed",
            RunState::Error => "error",
        }
    }
}

/// One line of the run's timeline (`at_ms` from the run start).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub at_ms: u64,
    /// `checkpoint`, `quit` or `fatal`
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
}

/// Classify one console.log line. Checkpoints carry their name; quit and fatal lines their text.
pub fn event_for(line: &str, at_ms: u64) -> Option<Event> {
    if let Some(c) = CHECKPOINT.captures(line) {
        return Some(Event {
            at_ms,
            kind: "checkpoint",
            name: Some(c[1].to_string()),
            line: None,
        });
    }
    let kind = if QUIT.is_match(line) {
        "quit"
    } else if FATAL.is_match(line) {
        "fatal"
    } else {
        return None;
    };
    Some(Event {
        at_ms,
        kind,
        name: None,
        line: Some(line.to_string()),
    })
}

/// Everything a status call reports. Checks and artifacts exist once the run has been judged.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub state: RunState,
    pub timeline: Vec<Event>,
    pub timeline_truncated: bool,
    pub exit_code: Option<i64>,
    pub checks: Option<Vec<Check>>,
    pub artifacts: Vec<String>,
    pub error: Option<String>,
    pub finished_ms: Option<u64>,
}

/// One `run_testscript` call.
pub struct Run {
    pub id: String,
    pub container: String,
    pub suite: String,
    pub judge: Option<String>,
    pub demo: bool,
    pub staged_path: PathBuf,
    pub artifacts_dir: PathBuf,
    /// set by `demo_to_video`: the recorder's output is checked and collected too
    pub movie: Option<MovieExpect>,
    started: Instant,
    outcome: Mutex<Outcome>,
}

impl Run {
    pub fn new(
        id: String,
        container: String,
        suite: String,
        judge: Option<String>,
        demo: bool,
        staged_path: PathBuf,
        artifacts_dir: PathBuf,
    ) -> Self {
        Run {
            id,
            container,
            suite,
            judge,
            demo,
            staged_path,
            artifacts_dir,
            movie: None,
            started: Instant::now(),
            outcome: Mutex::new(Outcome {
                state: RunState::Running,
                timeline: Vec::new(),
                timeline_truncated: false,
                exit_code: None,
                checks: None,
                artifacts: Vec::new(),
                error: None,
                finished_ms: None,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Outcome> {
        self.outcome.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> Outcome {
        self.lock().clone()
    }

    pub fn state(&self) -> RunState {
        self.lock().state
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn push_event(&self, event: Event) {
        let mut o = self.lock();
        if o.timeline.len() >= TIMELINE_CAP {
            o.timeline_truncated = true;
        } else {
            o.timeline.push(event);
        }
    }

    fn finish(
        &self,
        state: RunState,
        exit_code: Option<i64>,
        checks: Option<Vec<Check>>,
        artifacts: Vec<String>,
        error: Option<String>,
    ) {
        let finished_ms = self.elapsed_ms();
        let mut o = self.lock();
        o.state = state;
        o.exit_code = exit_code;
        o.checks = checks;
        o.artifacts = artifacts;
        o.error = error;
        o.finished_ms = Some(finished_ms);
    }
}

/// Runs of this server process, by id. `ClientState` holds it behind a std mutex (never held
/// across an await). Keeps every running run and the newest [`KEEP_FINISHED`] finished ones.
#[derive(Default)]
pub struct RunTable {
    seq: u64,
    order: VecDeque<String>,
    runs: HashMap<String, Arc<Run>>,
}

impl RunTable {
    pub fn next_id(&mut self) -> String {
        self.seq += 1;
        format!("run-{}", self.seq)
    }

    pub fn insert(&mut self, run: Arc<Run>) {
        // make room first: the new run finishes later, so the total stays at KEEP_FINISHED
        self.prune(KEEP_FINISHED - 1);
        self.order.push_back(run.id.clone());
        self.runs.insert(run.id.clone(), run);
    }

    /// Drop the oldest finished runs beyond `keep`. Running runs are never dropped.
    fn prune(&mut self, keep: usize) {
        let finished: Vec<String> = self
            .order
            .iter()
            .filter(|id| {
                self.runs
                    .get(*id)
                    .is_some_and(|r| r.state() != RunState::Running)
            })
            .cloned()
            .collect();
        let excess = finished.len().saturating_sub(keep);
        for id in finished.into_iter().take(excess) {
            self.runs.remove(&id);
            self.order.retain(|o| *o != id);
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Run>> {
        self.runs.get(id).cloned()
    }

    /// The run (running or not) that owns this container, if any.
    pub fn for_container(&self, container: &str) -> Option<Arc<Run>> {
        self.runs
            .values()
            .find(|r| r.container == container)
            .cloned()
    }

    /// Containers of runs still in progress.
    pub fn running_containers(&self) -> Vec<String> {
        self.runs
            .values()
            .filter(|r| r.state() == RunState::Running)
            .map(|r| r.container.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.runs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }
}

/// The session a run follows. `since` separates this run's evidence from older files.
#[derive(Clone)]
pub struct Session {
    pub runtime: Arc<DockerRuntime>,
    pub container: String,
    /// `<app>/game/csgo/console.log`; the screenshots and demos sit beside it
    pub console_log: PathBuf,
    pub since: SystemTime,
}

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub poll: Duration,
    pub timeout: Duration,
    pub quit_grace: Duration,
}

impl Timing {
    pub fn new(timeout: Duration) -> Self {
        Timing {
            poll: POLL,
            timeout,
            quit_grace: QUIT_GRACE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// the container exited with this code
    Exited(i64),
    /// the container vanished before it exited
    Gone,
    /// `quit` was logged but the process stayed up past the grace period
    QuitStalled,
    /// the run's timeout passed
    Timeout,
}

/// Read every complete line appended since the cursor, record the events, and advance the cursor.
/// Reads until nothing new is left (one read is capped), so after an exit the tail is recorded too.
async fn pump(
    log: &ConsoleLog,
    cursor: &mut Option<Cursor>,
    quit_at: &mut Option<Instant>,
    run: &Run,
) {
    // a missing file or a transient I/O error ends the drain; the next poll retries
    while let Ok(Some((lines, next))) = log.read_new_lines(cursor.as_ref()).await {
        *cursor = Some(next);
        if lines.is_empty() {
            break;
        }
        for line in &lines {
            let Some(event) = event_for(line, run.elapsed_ms()) else {
                continue;
            };
            if event.kind == "quit" && quit_at.is_none() {
                *quit_at = Some(Instant::now());
            }
            run.push_event(event);
        }
    }
}

/// Start the background task for a run. A supervisor awaits it: if it panics or is cancelled the
/// run is finished as `error` instead of staying `running` forever.
pub fn spawn_drive(run: Arc<Run>, session: Session, timing: Timing) {
    let watched = run.clone();
    let task = tokio::spawn(drive(run, session, timing));
    tokio::spawn(async move {
        if let Err(e) = task.await {
            tracing::error!(run = %watched.id, error = %e, "run task failed");
            if watched.state() == RunState::Running {
                watched.finish(
                    RunState::Error,
                    None,
                    None,
                    Vec::new(),
                    Some(format!("background task failed: {e}")),
                );
            }
        }
    });
}

/// Follow the run until it ends, then collect and judge. Failures become the run's `error`.
pub async fn drive(run: Arc<Run>, session: Session, timing: Timing) {
    let log = ConsoleLog::new(&session.console_log);
    let mut cursor: Option<Cursor> = None;
    let mut quit_at: Option<Instant> = None;
    let end = loop {
        // State first: lines written before an exit are read by this same iteration.
        let ended = match session.runtime.container_state(&session.container).await {
            Ok(ContainerState::Exited { code }) => Some(End::Exited(code)),
            Ok(ContainerState::Gone) => Some(End::Gone),
            // running, another state, or a transient podman error: keep polling
            _ => None,
        };
        pump(&log, &mut cursor, &mut quit_at, &run).await;
        if let Some(end) = ended {
            break end;
        }
        if quit_at.is_some_and(|t| t.elapsed() >= timing.quit_grace) {
            break End::QuitStalled;
        }
        if run.elapsed() >= timing.timeout {
            break End::Timeout;
        }
        tokio::time::sleep(timing.poll).await;
    };

    let stop = match end {
        End::Timeout | End::QuitStalled => Some(stop_note(&session).await),
        End::Exited(_) | End::Gone => None,
    };

    let csgo = session
        .console_log
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let (out, console, since) = (
        run.artifacts_dir.clone(),
        session.console_log.clone(),
        session.since,
    );
    let movie = run.movie.clone();
    let collected = tokio::task::spawn_blocking(move || {
        let mut names = collect(&csgo, &console, &out, since)?;
        if let Some(m) = movie {
            if let Some(video) = movie_judge::collect_video(&m, &out)? {
                names.push(video);
                names.sort();
            }
        }
        Ok::<_, io::Error>(names)
    })
    .await;
    let mut artifacts = match collected {
        Ok(Ok(names)) => names,
        Ok(Err(e)) => {
            let error = format!("artifact collection failed: {e}");
            run.finish(
                RunState::Error,
                None,
                None,
                Vec::new(),
                Some(with_stop(error, stop)),
            );
            return;
        }
        Err(e) => {
            let error = format!("artifact collection panicked: {e}");
            run.finish(
                RunState::Error,
                None,
                None,
                Vec::new(),
                Some(with_stop(error, stop)),
            );
            return;
        }
    };

    let (state, exit_code, checks, error) = match end {
        End::Exited(code) => {
            let (dir, judge, demo, list, movie) = (
                run.artifacts_dir.clone(),
                run.judge.clone(),
                run.demo,
                artifacts.clone(),
                run.movie.clone(),
            );
            match tokio::task::spawn_blocking(move || {
                judge_exit(&dir, judge.as_deref(), demo, code, &list, movie.as_ref())
            })
            .await
            {
                Ok((state, checks, error, extra)) => {
                    // the judge may add artifacts (the movie's contact sheet)
                    artifacts.extend(extra);
                    artifacts.sort();
                    (state, Some(code), checks, error)
                }
                Err(e) => (
                    RunState::Error,
                    Some(code),
                    None,
                    Some(format!("judging panicked: {e}")),
                ),
            }
        }
        End::Gone => (
            RunState::Error,
            None,
            None,
            Some("container disappeared before the game exited".into()),
        ),
        End::QuitStalled => (
            RunState::Error,
            None,
            None,
            Some(format!(
                "quit was logged but the process did not exit within {} s",
                timing.quit_grace.as_secs()
            )),
        ),
        End::Timeout => (
            RunState::Error,
            None,
            None,
            Some(format!("timed out after {} ms", timing.timeout.as_millis())),
        ),
    };
    let error = match (error, stop) {
        (Some(e), stop) => Some(with_stop(e, stop)),
        (None, stop) => stop,
    };
    run.finish(state, exit_code, checks, artifacts, error);
}

fn with_stop(error: String, stop: Option<String>) -> String {
    match stop {
        Some(s) => format!("{error}; {s}"),
        None => error,
    }
}

/// Stop the run's container and say what happened. A failed stop is reported as failed.
async fn stop_note(session: &Session) -> String {
    match session.runtime.down_named(&session.container).await {
        Ok(true) => format!("container {} stopped", session.container),
        Ok(false) => format!("container {} was already gone", session.container),
        Err(e) => format!("stopping container {} failed: {e:#}", session.container),
    }
}

/// The exit check, then the judge's checks. Passed only when all of them pass. Blocking file and
/// PNG work: call it from `spawn_blocking`.
fn judge_exit(
    dir: &Path,
    preset: Option<&str>,
    demo: bool,
    code: i64,
    artifacts: &[String],
    movie: Option<&MovieExpect>,
) -> (RunState, Option<Vec<Check>>, Option<String>, Vec<String>) {
    let note = if code == HANG_GUARD_EXIT {
        " (3 = Test_ExitProcess hang guard: quit did not finish)"
    } else {
        ""
    };
    let mut checks = vec![Check::new(
        "game exit code 0 (suite reached quit)",
        code == 0,
        format!("exit code {code}{note}"),
    )];
    let ev = Evidence::load(dir);
    match judge::checks_for(preset, demo, &ev) {
        Ok(more) => checks.extend(more),
        Err(e) => return (RunState::Error, None, Some(e), Vec::new()),
    }
    let mut extra = Vec::new();
    if let Some(m) = movie {
        let judgment = movie_judge::movie_checks(&ev.console, m, dir);
        checks.extend(judgment.checks);
        extra = judgment.artifacts;
    }
    let verdict = Verdict::new(checks, artifacts.to_vec());
    let state = if verdict.passed {
        RunState::Passed
    } else {
        RunState::Failed
    };
    (state, Some(verdict.checks), None, extra)
}

/// A `vtest_*` evidence file in the game dir: its path, name and metadata.
struct EvidenceFile {
    path: PathBuf,
    name: String,
    modified: Option<SystemTime>,
}

fn evidence_files(csgo: &Path) -> io::Result<Vec<EvidenceFile>> {
    let mut found = Vec::new();
    let sources = [
        (csgo.join("screenshots"), SCREENSHOT_EXT),
        (csgo.to_path_buf(), MEDIA_EXT),
    ];
    for (dir, exts) in sources {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let ext = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if !name.starts_with("vtest_") || !exts.contains(&ext) {
                continue;
            }
            let meta = entry.metadata()?;
            if meta.is_file() {
                found.push(EvidenceFile {
                    path: entry.path(),
                    name,
                    modified: meta.modified().ok(),
                });
            }
        }
    }
    Ok(found)
}

/// Copy this run's evidence into `out`: `console.log`, and the `vtest_*` PNGs and media modified at
/// or after `since`. Then prune the older `vtest_*` files from the game dir (the POC's cleanup: the
/// next run sees only its own evidence; the copies in earlier artifacts dirs are kept). Returns the
/// artifact names, sorted.
pub fn collect(
    csgo: &Path,
    console: &Path,
    out: &Path,
    since: SystemTime,
) -> io::Result<Vec<String>> {
    std::fs::create_dir_all(out)?;
    let mut names = Vec::new();
    match std::fs::copy(console, out.join("console.log")) {
        Ok(_) => names.push("console.log".to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    for file in evidence_files(csgo)? {
        let fresh = file.modified.is_none_or(|m| m >= since);
        if fresh {
            std::fs::copy(&file.path, out.join(&file.name))?;
            names.push(file.name);
        } else {
            // stale: from an earlier run. Best effort; it is not part of this run's evidence.
            let _ = std::fs::remove_file(&file.path);
        }
    }
    names.sort();
    Ok(names)
}

/// How long shutdown waits per container before it gives up on the stop (podman keeps stopping it).
const SHUTDOWN_STOP: Duration = Duration::from_secs(5);

/// Best-effort: stop the containers of runs still in progress. Called when the server exits;
/// without it their game would keep running with nobody following it. All containers are stopped
/// concurrently, each for at most [`SHUTDOWN_STOP`]; a stop still pending then is logged, not
/// failed. Returns the names that are gone.
pub async fn stop_running(runs: &Mutex<RunTable>, runtime: &Arc<DockerRuntime>) -> Vec<String> {
    let containers = runs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .running_containers();
    let mut set = tokio::task::JoinSet::new();
    for container in containers {
        let runtime = runtime.clone();
        set.spawn(async move {
            let outcome = runtime.stop_named(&container, SHUTDOWN_STOP).await;
            (container, outcome)
        });
    }
    let mut stopped = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((container, Ok(StopOutcome::Removed | StopOutcome::AlreadyGone))) => {
                stopped.push(container)
            }
            Ok((container, Ok(StopOutcome::Pending { state, .. }))) => {
                tracing::warn!(%container, %state, "run container still stopping at shutdown")
            }
            Ok((container, Err(e))) => {
                tracing::warn!(%container, error = %e, "could not stop a run container at shutdown")
            }
            Err(e) => tracing::warn!(error = %e, "a shutdown stop task failed"),
        }
    }
    stopped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str) -> Arc<Run> {
        Arc::new(Run::new(
            id.into(),
            format!("cs2mcp-wine-tools-{id}"),
            "move_jump".into(),
            None,
            false,
            PathBuf::from("/nope"),
            PathBuf::from("/nope"),
        ))
    }

    #[test]
    fn classifies_checkpoints_quit_and_fatal_lines() {
        let cp = event_for("10/10 08:20:05 TESTSCRIPT: CheckPoint -> 'VTEST_BEGIN'.", 5).unwrap();
        assert_eq!(
            (cp.kind, cp.name.as_deref(), cp.at_ms),
            ("checkpoint", Some("VTEST_BEGIN"), 5)
        );
        let q = event_for(
            "10/10 08:20:38 [HostStateManager] CHostStateMgr::QueueNewRequest( Quitting, 3 )",
            9,
        )
        .unwrap();
        assert_eq!(q.kind, "quit");
        let f = event_for("x Plat_FatalError: boom", 1).unwrap();
        assert_eq!(f.kind, "fatal");
        assert!(event_for("waiting for checkpoint 'Wrote_Screenshot'.", 1).is_none());
        assert!(event_for("[IME] IME Error:Opening Registry Key failed (2)", 1).is_none());
    }

    #[test]
    fn the_timeline_is_capped_and_says_so() {
        let r = run("cap");
        for i in 0..(TIMELINE_CAP + 100) {
            r.push_event(Event {
                at_ms: i as u64,
                kind: "fatal",
                name: None,
                line: Some("x".into()),
            });
        }
        let o = r.snapshot();
        assert_eq!(o.timeline.len(), TIMELINE_CAP);
        assert!(o.timeline_truncated);
    }

    #[test]
    fn the_table_keeps_the_newest_finished_runs_and_never_running_ones() {
        let mut table = RunTable::default();
        let first_running = run("keep-me");
        table.insert(first_running.clone());
        for _ in 0..(KEEP_FINISHED + 20) {
            let id = table.next_id();
            let r = run(&id);
            table.insert(r.clone());
            r.finish(RunState::Passed, Some(0), None, vec![], None);
        }
        assert!(table.get("run-1").is_none(), "oldest finished dropped");
        assert!(table.get(&format!("run-{}", KEEP_FINISHED + 20)).is_some());
        assert!(table.get("keep-me").is_some(), "running runs are kept");
        assert!(table.len() <= KEEP_FINISHED + 1, "len {}", table.len());
    }

    #[test]
    fn collect_copies_fresh_evidence_and_prunes_stale_files() {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!("ts-collect-{}-{nanos}", std::process::id()));
        let csgo = base.join("csgo");
        std::fs::create_dir_all(csgo.join("screenshots")).unwrap();
        std::fs::write(csgo.join("console.log"), "log\n").unwrap();
        std::fs::write(csgo.join("screenshots/vtest_spawn_0000.png"), b"png").unwrap();
        std::fs::write(csgo.join("screenshots/other.png"), b"nope").unwrap();
        std::fs::write(csgo.join("vtest_move_jump.dem"), b"dem").unwrap();
        std::fs::write(csgo.join("vtest_src.txt"), b"nope").unwrap();
        let since = SystemTime::now() - Duration::from_secs(60);
        let out = base.join("artifacts");
        let names = collect(&csgo, &csgo.join("console.log"), &out, since).unwrap();
        assert_eq!(
            names,
            vec!["console.log", "vtest_move_jump.dem", "vtest_spawn_0000.png"]
        );
        assert!(out.join("vtest_spawn_0000.png").is_file());
        assert!(
            csgo.join("vtest_src.txt").is_file(),
            "non-evidence is left alone"
        );

        // every file is older than a later `since`: nothing is collected, the stale files are pruned
        let later = collect(
            &csgo,
            &csgo.join("console.log"),
            &base.join("late"),
            SystemTime::now() + Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(later, vec!["console.log"]);
        assert!(!csgo.join("vtest_move_jump.dem").exists());
        assert!(!csgo.join("screenshots/vtest_spawn_0000.png").exists());
        assert!(csgo.join("screenshots/other.png").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
