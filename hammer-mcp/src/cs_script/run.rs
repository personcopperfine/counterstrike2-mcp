//! Host child processes for the cs_script tool: npm, node, and the project's `tsc` / `rollup`.
//! Every run has a deadline and captures stdout/stderr (nothing is inherited, nothing sleeps: the
//! wait is on the process). A long run (`npm install`) is a *job*: it keeps running when the tool
//! call's budget ends, and the next identical call joins it instead of starting a second one.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

/// The executables the tool starts. `CS_SCRIPT_NPM` / `CS_SCRIPT_NODE` override the PATH lookup
/// (a test points them at a fake).
#[derive(Debug, Clone)]
pub struct Programs {
    pub npm: PathBuf,
    pub node: PathBuf,
}

impl Programs {
    pub fn from_env() -> Programs {
        let pick = |var: &str, default: &str| {
            std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(default))
        };
        Programs { npm: pick("CS_SCRIPT_NPM", "npm"), node: pick("CS_SCRIPT_NODE", "node") }
    }
}

/// One finished (or failed-to-start, or killed) run.
#[derive(Debug, Clone, Default)]
pub struct RunResult {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// the program could not be started (not installed, bad cwd)
    pub spawn_error: Option<String>,
    pub elapsed_ms: u64,
}

impl RunResult {
    pub fn ok(&self) -> bool {
        self.code == Some(0) && !self.timed_out && self.spawn_error.is_none()
    }

    /// stdout then stderr.
    pub fn combined(&self) -> String {
        format!("{}{}{}", self.stdout, if self.stdout.ends_with('\n') || self.stdout.is_empty() { "" } else { "\n" }, self.stderr)
    }
}

/// Bytes kept per stream (the tails are what matters).
const KEEP: usize = 256 * 1024;

async fn drain<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> String {
    let mut kept: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                kept.extend_from_slice(&buf[..n]);
                if kept.len() > KEEP * 2 {
                    kept.drain(..kept.len() - KEEP);
                }
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// Kills a whole process group (the child and anything it spawned) on drop unless disarmed.
struct GroupKill(Option<u32>);

impl GroupKill {
    fn kill_now(&mut self) {
        if let Some(pid) = self.0.take() {
            // `kill -KILL -- -<pgid>`: the child is its own group leader (process_group(0))
            let _ = std::process::Command::new("kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        self.kill_now();
    }
}

/// How long the output drains may take after the process ended or was killed: a grandchild that
/// escaped the group and holds the pipe open must not hang the call.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Run `program args` in `cwd` with `env` added, killing its whole process group after `timeout`
/// (and when the future is dropped).
pub async fn run(program: &Path, args: &[&str], cwd: &Path, env: &[(&str, &str)], timeout: Duration) -> RunResult {
    let started = Instant::now();
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .envs(env.iter().copied())
        .env("NO_COLOR", "1")
        .env("NPM_CONFIG_UPDATE_NOTIFIER", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return RunResult {
                spawn_error: Some(format!("cannot start {}: {e}", program.display())),
                elapsed_ms: started.elapsed().as_millis() as u64,
                ..Default::default()
            }
        }
    };
    #[cfg(unix)]
    let mut group = GroupKill(child.id());
    #[cfg(not(unix))]
    let mut group = GroupKill(None);
    let mut out = tokio::spawn(drain(child.stdout.take().expect("piped")));
    let mut err = tokio::spawn(drain(child.stderr.take().expect("piped")));
    let (code, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(_)) => (None, false),
        Err(_) => {
            group.kill_now();
            let _ = child.kill().await;
            (None, true)
        }
    };
    // the child is gone; strays of its group go too, then the pipes close (bounded in any case)
    group.kill_now();
    let stdout = match tokio::time::timeout(DRAIN_GRACE, &mut out).await {
        Ok(joined) => joined.unwrap_or_default(),
        Err(_) => {
            out.abort();
            String::new()
        }
    };
    let stderr = match tokio::time::timeout(DRAIN_GRACE, &mut err).await {
        Ok(joined) => joined.unwrap_or_default(),
        Err(_) => {
            err.abort();
            String::new()
        }
    };
    RunResult { code, stdout, stderr, timed_out, spawn_error: None, elapsed_ms: started.elapsed().as_millis() as u64 }
}

struct Entry {
    rx: watch::Receiver<Option<Arc<RunResult>>>,
    /// set by the job's task when the process ended, whether or not anyone is waiting
    done_at: Arc<Mutex<Option<Instant>>>,
}

/// A finished job nobody collected is offered once, for this long.
const KEEP_FINISHED: Duration = Duration::from_secs(600);

fn jobs() -> &'static Mutex<HashMap<String, Entry>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Is the job under `key` still running? A job that ended is not, even if nobody collected it.
pub fn job_running(key: &str) -> bool {
    jobs()
        .lock()
        .map(|j| j.get(key).is_some_and(|e| e.done_at.lock().map(|d| d.is_none()).unwrap_or(false)))
        .unwrap_or(false)
}

/// Is there a job under `key` to join: running, or finished within [`KEEP_FINISHED`] and not yet
/// collected (the next [`run_job`] with the key returns its result once and forgets it)?
pub fn job_pending(key: &str) -> bool {
    let mut table = jobs().lock().expect("job table");
    let Some(entry) = table.get(key) else { return false };
    let stale = entry.done_at.lock().ok().and_then(|d| *d).is_some_and(|at| at.elapsed() > KEEP_FINISHED);
    if stale {
        table.remove(key);
        return false;
    }
    true
}

/// Start `program args` as the job `key` (or join the one already under it) and wait up to `wait`
/// for it. `None` = still running: call again with the same key to keep waiting. A finished job is
/// collected (removed) by the call that sees it. `job_timeout` bounds the process itself.
pub async fn run_job(
    key: &str,
    program: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    job_timeout: Duration,
    wait: Duration,
) -> Option<Arc<RunResult>> {
    let mut slot = {
        let mut table = jobs().lock().expect("job table");
        let stale = table
            .get(key)
            .and_then(|e| e.done_at.lock().ok().and_then(|d| *d))
            .is_some_and(|at| at.elapsed() > KEEP_FINISHED);
        if stale {
            table.remove(key);
        }
        match table.get(key) {
            Some(entry) => entry.rx.clone(),
            None => {
                let (tx, rx) = watch::channel(None);
                let done_at = Arc::new(Mutex::new(None));
                table.insert(key.to_string(), Entry { rx: rx.clone(), done_at: done_at.clone() });
                tokio::spawn(async move {
                    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                    let result = run(&program, &arg_refs, &cwd, &[], job_timeout).await;
                    if let Ok(mut d) = done_at.lock() {
                        *d = Some(Instant::now());
                    }
                    let _ = tx.send(Some(Arc::new(result)));
                });
                rx
            }
        }
    };
    let finished = tokio::time::timeout(wait, async {
        loop {
            if let Some(result) = slot.borrow_and_update().clone() {
                return Some(result);
            }
            if slot.changed().await.is_err() {
                return slot.borrow().clone();
            }
        }
    })
    .await
    .ok()
    .flatten();
    if finished.is_some() {
        jobs().lock().expect("job table").remove(key);
    }
    finished
}
