//! The launcher's actions: each one is a `cs2mcp-native` CLI run in a child process. The window
//! does no install work itself; it builds a command line ([`argv`]), reads the child's progress
//! (`--events jsonl` on stderr, one [`Event`] per line) and its result (the JSON on stdout), and
//! maps the exit code ([`outcome`]): 0 done, 2 a decision is needed (the JSON names the
//! findings), anything else an error. `--no-dialog` is always passed: the window shows its own.
//!
//! Reader threads feed two channels the UI polls with `try_recv` on every frame.

use cs2_native::events::Event;
use cs2_native::findings::Finding;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `status --preflight`
    Status,
    /// `prepare`
    Prepare,
    /// `up --no-wait`: prepare + launch; the window then runs [`Action::Wait`]
    Up,
    /// `wait`: the readiness ladder (ended when Stop is pressed)
    Wait,
    /// `down`: stop the game, restore, verify
    Stop,
    /// `uninstall`: restore without a session
    Uninstall,
    /// `cleanup`: list what a cleanup would move
    CleanupPreview,
    /// `cleanup --yes`
    Cleanup,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Status => "Status",
            Action::Prepare => "Prepare",
            Action::Up => "Prepare + Launch",
            Action::Wait => "Waiting for the bridge",
            Action::Stop => "Stop and restore",
            Action::Uninstall => "Repair (restore the install)",
            Action::CleanupPreview => "Clean up (preview)",
            Action::Cleanup => "Clean up",
        }
    }

    /// Does it change the install (so the status is read again afterwards)?
    pub fn changes(self) -> bool {
        !matches!(self, Action::Status | Action::CleanupPreview)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Steam {
    Gbe,
    Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Auto,
    Overlay,
    InPlace,
}

impl Strategy {
    pub fn cli(self) -> &'static str {
        match self {
            Strategy::Auto => "auto",
            Strategy::Overlay => "overlay",
            Strategy::InPlace => "in-place",
        }
    }
}

/// What the user set in the window.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub game_root: String,
    pub library: String,
    pub state_dir: String,
    pub strategy: Strategy,
    pub steam: Steam,
    pub tools: bool,
    pub map: String,
    pub addon: String,
    /// extra game arguments, whitespace-separated
    pub extra: String,
    pub capture: bool,
    pub timeout_secs: u64,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            game_root: String::new(),
            library: String::new(),
            state_dir: String::new(),
            strategy: Strategy::Auto,
            steam: Steam::Gbe,
            tools: false,
            map: String::new(),
            addon: String::new(),
            extra: String::new(),
            capture: true,
            timeout_secs: 900,
        }
    }
}

/// The user's answers to findings, as the CLI's flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Answers {
    /// `--accept-conflicts` ("Continue anyway")
    pub accept_conflicts: bool,
    /// `--forget-missing-backups` ("Mark as lost")
    pub forget_missing_backups: bool,
}

fn push_opt(v: &mut Vec<String>, flag: &str, value: &str) {
    let t = value.trim();
    if !t.is_empty() {
        v.push(flag.into());
        v.push(t.into());
    }
}

/// The `cs2mcp-native` arguments for `action`.
pub fn argv(action: Action, s: &Settings, a: Answers) -> Vec<String> {
    let mut v: Vec<String> = vec!["--events".into(), "jsonl".into(), "--no-dialog".into()];
    push_opt(&mut v, "--game-root", &s.game_root);
    push_opt(&mut v, "--library", &s.library);
    push_opt(&mut v, "--state-dir", &s.state_dir);
    v.push("--strategy".into());
    v.push(s.strategy.cli().into());
    if a.accept_conflicts {
        v.push("--accept-conflicts".into());
    }
    if a.forget_missing_backups {
        v.push("--forget-missing-backups".into());
    }
    let mode = |v: &mut Vec<String>| {
        v.push("--steam".into());
        v.push(match s.steam {
            Steam::Gbe => "gbe".into(),
            Steam::Client => "client".into(),
        });
        if s.tools {
            v.push("--tools".into());
        }
    };
    match action {
        Action::Status => v.extend(["status".into(), "--preflight".into()]),
        Action::Prepare => {
            v.push("prepare".into());
            mode(&mut v);
        }
        Action::Up => {
            v.push("up".into());
            mode(&mut v);
            push_opt(&mut v, "--map", &s.map);
            push_opt(&mut v, "--addon", &s.addon);
            v.push("--no-wait".into());
            if !s.capture {
                v.push("--no-capture".into());
            }
            let extra: Vec<String> = s.extra.split_whitespace().map(String::from).collect();
            if !extra.is_empty() {
                v.push("--".into());
                v.extend(extra);
            }
        }
        Action::Wait => v.extend(["wait".into(), "--timeout".into(), s.timeout_secs.to_string()]),
        Action::Stop => v.push("down".into()),
        Action::Uninstall => v.push("uninstall".into()),
        Action::CleanupPreview => v.push("cleanup".into()),
        Action::Cleanup => v.extend(["cleanup".into(), "--yes".into()]),
    }
    v
}

/// The `cs2mcp-native` to run: the package's (next to this launcher, `/opt/cs2mcp` for the .deb,
/// `$APPDIR/opt/cs2mcp` in the AppImage, where it inherits the AppRun environment and so names
/// `$APPIMAGE` for the supervisor that outlives this window), else a development build's sibling.
pub fn cli_path() -> anyhow::Result<PathBuf> {
    let name = cs2_native::selfexe::exe_name("cs2mcp-native");
    let mut tried = Vec::new();
    let pkg = cs2_native::selfexe::package_dir();
    let sib = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.to_path_buf()));
    for d in pkg.into_iter().chain(sib) {
        let p = d.join(&name);
        if p.is_file() {
            return Ok(p);
        }
        tried.push(p.display().to_string());
    }
    anyhow::bail!("{name} is not installed next to the launcher (looked for {}); the launcher runs it for every action", tried.join(", "))
}

pub enum Outcome {
    Done(Value),
    /// a decision is needed: the CLI stopped with exit 2 and these findings
    Stopped { message: String, findings: Vec<Finding> },
    Failed(String),
}

/// How a CLI run ended: its exit code (`None`: killed), its stdout JSON and the last stderr lines.
pub fn outcome(code: Option<i32>, stdout: &str, stderr_tail: &[String]) -> Outcome {
    let v: Option<Value> = serde_json::from_str(stdout.trim()).ok();
    let error = v.as_ref().and_then(|v| v["error"].as_str().map(String::from));
    match (code, v) {
        (Some(0), Some(v)) => Outcome::Done(v),
        // stopped before it changed anything, on a decision or a blocker
        (Some(2), Some(v)) if error.is_some() => {
            Outcome::Stopped { message: error.unwrap_or_default(), findings: serde_json::from_value(v["findings"].clone()).unwrap_or_default() }
        }
        // a restore that finished with a decision open: done, the findings are in the result
        (Some(2), Some(v)) => Outcome::Done(v),
        (None, _) => Outcome::Failed("ended (the command was stopped)".into()),
        (Some(c), _) => Outcome::Failed(error.unwrap_or_else(|| {
            let tail = stderr_tail.join("\n");
            format!("cs2mcp-native exited with {c}{}", if tail.is_empty() { String::new() } else { format!(":\n{tail}") })
        })),
    }
}

/// One stderr line: a JSON event, or plain text (a panic message, another program's output).
pub fn parse_event(line: &str) -> Event {
    serde_json::from_str(line).unwrap_or_else(|_| Event::Info { text: line.to_string() })
}

pub struct Job {
    pub action: Action,
    pub answers: Answers,
    pub events: Receiver<Event>,
    pub done: Receiver<Outcome>,
    child: Arc<Mutex<Option<Child>>>,
    pub started: Instant,
}

impl Job {
    /// End the child. Only the readiness `wait` is ended this way: it changes nothing.
    pub fn cancel(&self) {
        if let Ok(mut c) = self.child.lock() {
            if let Some(c) = c.as_mut() {
                let _ = c.kill();
            }
        }
    }
}

/// Start `action`: spawn the CLI, read its output on two threads, report the outcome.
pub fn start(action: Action, settings: &Settings, answers: Answers, repaint: impl Fn() + Send + Clone + 'static) -> Job {
    let (etx, events) = mpsc::channel();
    let (dtx, done) = mpsc::channel();
    let slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let job = Job { action, answers, events, done, child: slot.clone(), started: Instant::now() };
    let args = argv(action, settings, answers);
    let spawned = cli_path().and_then(|p| {
        let mut cmd = Command::new(&p);
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let _ = etx.send(Event::Info { text: format!("$ {} {}", p.display(), args.join(" ")) });
        cmd.spawn().map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))
    });
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let _ = dtx.send(Outcome::Failed(format!("{e:#}")));
            repaint();
            return job;
        }
    };
    let (out, err) = (child.stdout.take(), child.stderr.take());
    if let Ok(mut s) = slot.lock() {
        *s = Some(child);
    }
    let r = repaint.clone();
    let tail = Arc::new(Mutex::new(Vec::<String>::new()));
    let tail_w = tail.clone();
    let stderr_reader = std::thread::spawn(move || {
        let Some(err) = err else { return };
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            if let Ok(mut t) = tail_w.lock() {
                t.push(line.clone());
                let n = t.len();
                if n > 20 {
                    t.drain(..n - 20);
                }
            }
            let _ = etx.send(parse_event(&line));
            r();
        }
    });
    std::thread::spawn(move || {
        let mut stdout = String::new();
        if let Some(mut o) = out {
            let _ = o.read_to_string(&mut stdout);
        }
        let _ = stderr_reader.join();
        // both pipes are closed: the child has exited (or is exiting). Poll its status without
        // holding the lock across a blocking wait, so cancel() can still take it.
        let code = loop {
            let st = slot.lock().ok().and_then(|mut c| c.as_mut().map(|c| c.try_wait()));
            match st {
                Some(Ok(Some(st))) => break st.code(),
                Some(Ok(None)) => std::thread::sleep(std::time::Duration::from_millis(20)),
                _ => break None,
            }
        };
        let tail: Vec<String> = tail.lock().map(|t| t.iter().map(|l| parse_event(l).line()).collect()).unwrap_or_default();
        let _ = dtx.send(outcome(code, &stdout, &tail));
        repaint();
    });
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    use cs2_native::findings::{Code, Severity};

    #[test]
    fn the_command_lines_are_the_cli_s() {
        let s = Settings { game_root: " /g ".into(), map: "de_dust2".into(), extra: "-novid  -w 1280".into(), capture: false, ..Settings::default() };
        assert_eq!(
            argv(Action::Up, &s, Answers::default()),
            ["--events", "jsonl", "--no-dialog", "--game-root", "/g", "--strategy", "auto", "up", "--steam", "gbe", "--map", "de_dust2", "--no-wait", "--no-capture", "--", "-novid", "-w", "1280"]
        );
        let c = Settings { steam: Steam::Client, tools: true, strategy: Strategy::InPlace, ..Settings::default() };
        let a = Answers { accept_conflicts: true, forget_missing_backups: true };
        assert_eq!(
            argv(Action::Prepare, &c, a),
            ["--events", "jsonl", "--no-dialog", "--strategy", "in-place", "--accept-conflicts", "--forget-missing-backups", "prepare", "--steam", "client", "--tools"]
        );
        let tail = |a: Action| argv(a, &c, Answers::default())[5..].to_vec();
        assert_eq!(tail(Action::Status), ["status", "--preflight"]);
        assert_eq!(tail(Action::Stop), ["down"]);
        assert_eq!(tail(Action::Uninstall), ["uninstall"]);
        assert_eq!(tail(Action::CleanupPreview), ["cleanup"]);
        assert_eq!(tail(Action::Cleanup), ["cleanup", "--yes"]);
        assert_eq!(tail(Action::Wait), ["wait", "--timeout", "900"]);
    }

    #[test]
    fn exit_codes_map_to_outcomes() {
        assert!(matches!(outcome(Some(0), r#"{"ok":1}"#, &[]), Outcome::Done(v) if v["ok"] == 1));
        let f = serde_json::to_string(&Finding::new(Code::Legacy, Severity::Decision, "old layout")).unwrap();
        match outcome(Some(2), &format!(r#"{{"error":"stopped","findings":[{f}]}}"#), &[]) {
            Outcome::Stopped { message, findings } => {
                assert_eq!(message, "stopped");
                assert_eq!(findings[0].code, Code::Legacy);
            }
            _ => panic!("not stopped"),
        }
        // a restore with conflicts: exit 2 without "error" is a finished result
        assert!(matches!(outcome(Some(2), r#"{"restore":{},"findings":[]}"#, &[]), Outcome::Done(_)));
        assert!(matches!(outcome(Some(1), r#"{"error":"no install","findings":[]}"#, &[]), Outcome::Failed(e) if e == "no install"));
        assert!(matches!(outcome(Some(101), "", &["thread panicked".into()]), Outcome::Failed(e) if e.contains("exited with 101") && e.contains("panicked")));
        assert!(matches!(outcome(None, "", &[]), Outcome::Failed(_)));
    }

    #[test]
    fn stderr_lines_become_events() {
        assert_eq!(parse_event(r#"{"kind":"stage","stage":"BridgeHello","seconds":3.5}"#), Event::Stage { stage: "BridgeHello".into(), seconds: 3.5 });
        assert_eq!(parse_event(r#"{"kind":"warn","text":"w"}"#), Event::Warn { text: "w".into() });
        assert_eq!(parse_event("thread 'main' panicked"), Event::Info { text: "thread 'main' panicked".into() });
    }
}
