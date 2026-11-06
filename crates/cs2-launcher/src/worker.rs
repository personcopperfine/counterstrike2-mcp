//! The launcher's actions, each on a worker thread: the library calls block (a prepare hashes
//! the tree, `wait_ready` polls for minutes), the UI must not. Progress comes back through an
//! [`Events::channel`], the result through a second channel; the UI polls both with `try_recv`
//! on every frame and asks for the next frame with `request_repaint_after` while a job runs.

use cs2_native::args::GameArgs;
use cs2_native::events::{Event, Events};
use cs2_native::findings::{Policy, Stopped};
use cs2_native::mcpconfig::{self, Which};
use cs2_native::plan::SteamMode;
use cs2_native::session::{self, Ctx, CtxOpts, UpOpts};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Status,
    Prepare,
    /// prepare + launch + the readiness ladder
    Up,
    /// stop the game, restore, verify
    Stop,
    /// restore without a session
    Uninstall,
    /// list what a cleanup would move
    CleanupPreview,
    /// move the leftovers out
    Cleanup,
    /// the `mcpServers` block
    McpConfig,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Status => "Status",
            Action::Prepare => "Prepare",
            Action::Up => "Prepare + Launch",
            Action::Stop => "Stop",
            Action::Uninstall => "Repair (restore the install)",
            Action::CleanupPreview => "Clean up (preview)",
            Action::Cleanup => "Clean up",
            Action::McpConfig => "MCP config",
        }
    }
}

/// What the user set in the window.
#[derive(Debug, Clone)]
pub struct Settings {
    pub ctx: CtxOpts,
    pub steam: SteamMode,
    pub tools: bool,
    pub map: String,
    pub addon: String,
    /// extra game arguments, whitespace-separated
    pub extra: String,
    pub capture: bool,
    pub keep_installed: bool,
    pub timeout: Duration,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            ctx: CtxOpts::default(),
            steam: SteamMode::Gbe,
            tools: false,
            map: String::new(),
            addon: String::new(),
            extra: String::new(),
            capture: true,
            keep_installed: false,
            timeout: Duration::from_secs(900),
        }
    }
}

fn some(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

pub enum Outcome {
    Done(Value),
    /// a decision is needed (`findings::Stopped`): the dialog offers the ways on
    Stopped(Stopped),
    Failed(String),
}

pub struct Job {
    pub action: Action,
    pub policy: Policy,
    pub events: Receiver<Event>,
    pub done: Receiver<Outcome>,
    pub cancel: Arc<AtomicBool>,
    pub started: Instant,
}

impl Job {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Start `action` with `settings` and the user's answers `policy`.
pub fn start(action: Action, settings: &Settings, policy: Policy, repaint: impl Fn() + Send + 'static) -> Job {
    let (ev, events) = Events::channel();
    let (tx, done) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let s = settings.clone();
    let c = cancel.clone();
    std::thread::spawn(move || {
        let out = match run(action, &s, policy, ev.clone(), &c) {
            Ok(v) => Outcome::Done(v),
            Err(e) => match e.downcast::<Stopped>() {
                Ok(st) => Outcome::Stopped(st),
                Err(e) => {
                    ev.warn(format!("error: {e:#}"));
                    Outcome::Failed(format!("{e:#}"))
                }
            },
        };
        let _ = tx.send(out);
        repaint();
    });
    Job { action, policy, events, done, cancel, started: Instant::now() }
}

fn run(action: Action, s: &Settings, policy: Policy, ev: Events, cancel: &AtomicBool) -> anyhow::Result<Value> {
    let o = CtxOpts { policy, ..s.ctx.clone() };
    let ctx = Ctx::open(&o, ev.clone())?;
    match action {
        Action::Status => {
            let mut v = session::status(&ctx, false)?;
            let pre = ctx.preflight()?;
            if let Some(f) = v["findings"].as_array_mut() {
                f.extend(pre.into_iter().filter_map(|x| serde_json::to_value(x).ok()));
            }
            Ok(v)
        }
        Action::Prepare => session::prepare(&ctx, s.steam, s.tools),
        Action::Up => {
            let game = GameArgs { tools: s.tools, map: some(&s.map), addon: some(&s.addon), extra: s.extra.split_whitespace().map(String::from).collect() };
            let up = UpOpts {
                steam: s.steam,
                tools: s.tools,
                game,
                timeout: s.timeout,
                keep_installed: s.keep_installed,
                // the ladder runs below, where the Stop button can cancel it
                wait: false,
                capture: s.capture,
                launcher: cs2_native::selfexe::native_command()?,
            };
            let mut v = session::up(&ctx, &up)?;
            v["ready"] = session::wait_ready(&ctx, s.timeout, cancel)?;
            Ok(v)
        }
        Action::Stop => session::down(&ctx, Duration::from_secs(120)),
        Action::Uninstall => session::uninstall(&ctx),
        Action::CleanupPreview => session::cleanup(&ctx, false),
        Action::Cleanup => session::cleanup(&ctx, true),
        Action::McpConfig => {
            let servers = mcpconfig::default_servers();
            let notes = mcpconfig::notes(cs2_native::install::Os::host(), &servers);
            Ok(json!({"config": session::mcp_config(&ctx, Which::Both, &servers), "notes": notes}))
        }
    }
}
