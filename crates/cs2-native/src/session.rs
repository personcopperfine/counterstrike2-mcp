//! `prepare` / `up` / `supervise` / `down` / `uninstall` / `status` / `cleanup`: the session
//! lifecycle over the pure modules (README "Launch, processes and readiness"). The container
//! session (`run`) is in `run.rs`. Progress goes to the context's [`Events`] sink; what the
//! launcher found that needs the user's eye is returned as findings (`findings.rs`).

use crate::args::{self, GameArgs};
use crate::events::Events;
use crate::findings::{self, Code, Finding, Policy, Severity, Stopped};
use crate::install::{Install, Os};
use crate::manifest::{self, Header, RestoreOpts};
use crate::plan::{self, Artifacts, PlanInput, SteamMode};
use crate::probe;
use crate::selfexe::NativeCmd;
use crate::state::{now_f64, Dirs, Phase, Session, Strategy, SESSION_FORMAT};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Everything a command needs about the install.
pub struct Ctx {
    pub install: Install,
    pub dirs: Dirs,
    pub strategy: Strategy,
    pub artifacts: Artifacts,
    pub discovery: PathBuf,
    /// where every progress line goes
    pub events: Events,
    /// the user's answers to decisions, given in advance (`--accept-conflicts`, ...)
    pub policy: Policy,
    /// the package dir whose `SHA256SUMS` covers the payload (`None`: a development build)
    pub package_dir: Option<PathBuf>,
    /// artefact dirs given explicitly (`--bridge-dir` ...): not covered by the package's sums
    pub overridden: Vec<String>,
    /// other CS2 installs the Steam libraries name (this one is the first)
    pub other_installs: Vec<PathBuf>,
    /// gbe's `local_save_path` (default `<state>/gbe-saves`)
    pub gbe_save_dir: Option<PathBuf>,
}

/// What [`Ctx::open`] takes (the CLI's global options; the GUI's settings).
#[derive(Debug, Clone, Default)]
pub struct CtxOpts {
    pub game_root: Option<PathBuf>,
    pub library: Vec<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub strategy: Option<Strategy>,
    pub bridge_dir: Option<PathBuf>,
    pub metamod_dir: Option<PathBuf>,
    pub gbe_dir: Option<PathBuf>,
    pub vk_layer_dir: Option<PathBuf>,
    pub discovery: Option<PathBuf>,
    pub gbe_save_dir: Option<PathBuf>,
    pub policy: Policy,
}

impl Ctx {
    pub fn open(o: &CtxOpts, events: Events) -> anyhow::Result<Ctx> {
        let (install, other_installs) = crate::install::discover_with_others(o.game_root.as_deref(), &o.library)?;
        let root = crate::state::state_root(o.state_dir.as_deref())?;
        let dirs = Dirs::new(&root, &install.root);
        let strategy = o.strategy.unwrap_or(Strategy::Auto).resolve(Os::host())?;
        let fetched = crate::gbe::fetched_dir(&root);
        let artifacts = Artifacts::resolve(o.bridge_dir.clone(), o.metamod_dir.clone(), o.gbe_dir.clone(), Some(fetched), o.vk_layer_dir.clone());
        let discovery = o
            .discovery
            .clone()
            .or_else(probe::default_discovery)
            .context("no discovery path (set --discovery)")?;
        let overridden = [("--bridge-dir", &o.bridge_dir), ("--metamod-dir", &o.metamod_dir), ("--gbe-dir", &o.gbe_dir), ("--vk-layer-dir", &o.vk_layer_dir)]
            .iter()
            .filter(|(_, v)| v.is_some())
            .map(|(k, _)| k.to_string())
            .collect();
        Ok(Ctx {
            install,
            dirs,
            strategy,
            artifacts,
            discovery,
            events,
            policy: o.policy,
            package_dir: crate::selfexe::package_dir(),
            overridden,
            other_installs,
            gbe_save_dir: o.gbe_save_dir.clone(),
        })
    }

    pub fn gbe_saves(&self) -> PathBuf {
        self.gbe_save_dir.clone().unwrap_or_else(|| self.dirs.gbe_saves())
    }

    fn restore_opts(&self) -> RestoreOpts {
        RestoreOpts { forget_missing_backups: self.policy.forget_missing_backups }
    }

    /// What is true of this install before any command runs: more than one install found, the
    /// payload's integrity, the game build.
    pub fn preflight(&self) -> anyhow::Result<Vec<Finding>> {
        let mut v = Vec::new();
        if !self.other_installs.is_empty() {
            v.push(
                Finding::new(
                    Code::MultipleInstalls,
                    Severity::Info,
                    format!("{} CS2 installs found; using {}", self.other_installs.len() + 1, self.install.root.display()),
                )
                .paths(self.other_installs.iter().map(|p| p.display().to_string()))
                .advice("pass --game-root <install> (GUI: Settings) for another one"),
            );
        }
        match &self.package_dir {
            Some(pkg) => {
                if let Some(f) = crate::payload::verify(pkg)?.finding() {
                    v.push(f);
                }
                if !self.overridden.is_empty() {
                    v.push(Finding::new(
                        Code::PayloadUnverified,
                        Severity::Info,
                        format!("{} given: those artefacts are not covered by the package's SHA256SUMS", self.overridden.join(", ")),
                    ));
                }
            }
            None => v.push(Finding::new(
                Code::PayloadUnverified,
                Severity::Info,
                "a development build (no package dir): the artefacts are not verified",
            )),
        }
        v.extend(crate::install::build_finding(self.install.build.as_deref()));
        Ok(v)
    }

    /// Leftovers of earlier installs in the tree (`leftovers.rs`), as findings.
    pub fn leftovers(&self) -> (crate::leftovers::Seen, Vec<Finding>) {
        let os = Os::host();
        let k = crate::leftovers::known(os, &self.artifacts, &self.gbe_saves());
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let seen = crate::leftovers::look(&self.install.root, os, &k, home.as_deref());
        let f = crate::leftovers::findings(&seen, os);
        (seen, f)
    }
}

pub fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        crate::win::pid_alive(pid)
    }
    #[cfg(not(windows))]
    {
        // SAFETY: signal 0 only checks existence/permission.
        let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
        alive && !is_zombie(pid)
    }
}

#[cfg(not(windows))]
fn is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(')').map(|(_, r)| r.trim_start().starts_with('Z')))
        .unwrap_or(false)
}

/// Refuse while a game runs from the tree (in-place: its files are in use; ours: still running).
fn guard_not_running(ctx: &Ctx) -> anyhow::Result<()> {
    if let Some(s) = Session::load(&ctx.dirs.session_file())? {
        if s.alive() {
            bail!(
                "a cs2mcp-native session is still running for this install (supervisor {:?}, game {:?}); `cs2mcp-native down` first",
                s.supervisor_pid,
                s.game_pids
            );
        }
    }
    #[cfg(windows)]
    {
        let running = crate::win::processes_under(&ctx.install.root, &["cs2.exe", "cs2mcp_launch.exe"], &[]);
        if !running.is_empty() {
            bail!(
                "CS2 is running from this install ({}); close it first",
                running
                    .iter()
                    .map(|(p, x)| format!("pid {p} {}", x.as_deref().map(|x| x.display().to_string()).unwrap_or_default()))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    Ok(())
}

/// How long `prepare`/`up`/`uninstall` wait for another command's state lock (`Dirs::lock`).
pub const LOCK_WAIT: Duration = Duration::from_secs(120);

/// Put the tree (or the overlay's upper dir) into the prepared state.
pub fn prepare(ctx: &Ctx, steam: SteamMode, tools: bool) -> anyhow::Result<Value> {
    let _lock = ctx.dirs.lock(LOCK_WAIT, "prepare", &ctx.events)?;
    prepare_locked(ctx, steam, tools)
}

fn report_all(ctx: &Ctx, f: &[Finding]) {
    for x in f {
        x.report(&ctx.events);
    }
}

/// An earlier manifest found at prepare: restore it (it covers everything on disk, also after a
/// crash mid-apply), and say what was recovered.
fn recover(ctx: &Ctx, findings: &mut Vec<Finding>) -> anyhow::Result<Option<Value>> {
    let store = ctx.dirs.store();
    let Some(m) = store.load()? else { return Ok(None) };
    let why = match m.state {
        manifest::State::Applying => "a prepare was interrupted mid-way (a crash or a kill)",
        manifest::State::Applied => "the last session did not restore the tree (a crash, a reboot, or --keep-installed)",
        manifest::State::Restoring => "a restore was interrupted mid-way",
    };
    ctx.events.info(format!("an earlier prepare is still recorded ({why}); restoring it first"));
    let Some(r) = manifest::restore(&ctx.install.root, &store, &ctx.dirs.logs(), ctx.restore_opts())? else { return Ok(None) };
    report_restore(&r, &ctx.events);
    let n = r.restored.len() + r.removed.len();
    findings.push(
        Finding::new(Code::Recovered, Severity::Warn, format!("recovered {n} entries of an earlier prepare ({why})"))
            .paths(r.restored.iter().map(|p| format!("restored: {p}")).chain(r.removed.iter().map(|p| format!("removed: {p}")))),
    );
    // the next prepare backs up the tree as it is now, so Steam's newer bytes become `before`:
    // a warning here, not a decision
    for mut f in r.findings() {
        if f.code == Code::Conflicts {
            f.severity = Severity::Warn;
        }
        findings.push(f);
    }
    if !r.verify_failures.is_empty() {
        return Err(Stopped { message: "the earlier prepare could not be restored cleanly; fix the files listed, then `cs2mcp-native uninstall`".into(), findings: findings.clone() }.into());
    }
    Ok(Some(json!({"entries": m.entries.len(), "previousState": m.state, "restore": r})))
}

fn prepare_locked(ctx: &Ctx, steam: SteamMode, tools: bool) -> anyhow::Result<Value> {
    guard_not_running(ctx)?;
    if tools && !ctx.install.has_tools {
        bail!(
            "--tools: this install has no Workshop Tools (game/bin/win64/tools/hammer.dll + game/bin/sdkenginetools.txt); install the \"Counter-Strike 2 Workshop Tools\" DLC in Steam"
        );
    }
    let mut found = ctx.preflight()?;
    // a tampered payload stops before the tree is looked at
    if found.iter().any(|f| f.severity == Severity::Blocker) {
        report_all(ctx, &found);
        return Err(Stopped { message: "the launcher's payload failed its integrity check".into(), findings: found }.into());
    }
    let os = Os::host();
    let saves = ctx.gbe_saves();
    std::fs::create_dir_all(&saves)?;
    let input = PlanInput { os, steam, tools, root: &ctx.install.root, gbe_save_dir: &saves };
    let recovered = if ctx.strategy == Strategy::InPlace { recover(ctx, &mut found)? } else { None };
    if recovered.is_none() {
        // no manifest: anything of ours in the tree now is a leftover no record covers
        found.extend(ctx.leftovers().1);
    }
    report_all(ctx, &found);
    findings::gate(&found, ctx.policy, "prepare stopped before changing anything: the install needs your decision (see the findings)")?;
    match ctx.strategy {
        Strategy::InPlace => {
            let store = ctx.dirs.store();
            // plan only after the restore: `before` must be the user's current bytes
            let plan = plan::build(&input, &ctx.artifacts)?;
            let header = Header { build: ctx.install.build.clone(), steam, tools };
            let m = manifest::apply(&plan, &ctx.install.root, &store, &header)?;
            let changed = m.entries.iter().filter(|e| e.role == manifest::Role::Ours).count();
            ctx.events.info(format!("prepared in place: {changed} paths recorded in {}", store.manifest_path().display()));
            Ok(json!({"strategy": "in-place", "manifest": store.manifest_path(), "entries": m.entries.len(),
                      "recovered": recovered, "findings": found}))
        }
        Strategy::Overlay => {
            #[cfg(target_os = "linux")]
            {
                let plan = plan::build(&input, &ctx.artifacts)?;
                let staged = crate::overlay::stage(&plan, &ctx.install.root, &ctx.dirs.upper(), &ctx.dirs.work())?;
                crate::overlay::probe(&ctx.dirs.dir.join("overlay-probe")).context(
                    "this system refuses an unprivileged user+mount namespace with overlayfs; use --strategy in-place",
                )?;
                ctx.events.info(format!("staged {} files into {} (the install is not touched)", staged.len(), ctx.dirs.upper().display()));
                Ok(json!({"strategy": "overlay", "upper": ctx.dirs.upper(), "files": staged.len(), "findings": found}))
            }
            #[cfg(not(target_os = "linux"))]
            {
                bail!("--strategy overlay is Linux-only")
            }
        }
        Strategy::Auto => unreachable!("resolved by the caller"),
    }
}

pub struct UpOpts {
    pub steam: SteamMode,
    pub tools: bool,
    pub game: GameArgs,
    pub timeout: Duration,
    pub keep_installed: bool,
    pub wait: bool,
    /// load the capture layer into the game when the payload has it (`--no-capture`: false)
    pub capture: bool,
    /// how to start `cs2mcp-native supervise` (the CLI: itself; the GUI: the sibling binary or
    /// `$APPIMAGE cs2mcp-native`, `selfexe::native_command`)
    pub launcher: NativeCmd,
}

/// The capture layer for a session: staged into the state dir (so the session never names a
/// directory that can go away), or `None` with the reason said.
fn session_capture(ctx: &Ctx, want: bool) -> Option<crate::capture::Capture> {
    if !want {
        return None;
    }
    let os = Os::host();
    match crate::capture::layer_dir(&ctx.artifacts.vklayer, os) {
        Some(src) => match crate::capture::stage_layer(&src, &ctx.dirs.dir, os) {
            Ok(layer_dir) => Some(crate::capture::Capture { layer_dir, dir: ctx.dirs.capture() }),
            Err(e) => {
                ctx.events.warn(format!("staging the capture layer from {}: {e}; screenshot/observe will have no frames", src.display()));
                None
            }
        },
        None => {
            ctx.events.info(format!("{}: screenshot/observe will have no frames", crate::capture::missing(&ctx.artifacts.vklayer, os)));
            None
        }
    }
}

/// A fresh session record for `ctx` (no pids yet, restored on exit, no capture).
fn new_session(ctx: &Ctx, steam: SteamMode, tools: bool, map: Option<String>, game_args: Vec<String>, kind: &str) -> Session {
    Session {
        format: SESSION_FORMAT.into(),
        install_root: ctx.install.root.clone(),
        os: Os::host(),
        strategy: ctx.strategy,
        steam,
        tools,
        map,
        args: game_args,
        started_at: now_f64(),
        supervisor_pid: None,
        game_pids: Vec::new(),
        phase: Phase::Starting,
        exit_code: None,
        keep_installed: false,
        discovery: Some(ctx.discovery.clone()),
        console_log: ctx.dirs.console_log(&ctx.install.root, ctx.strategy),
        restore: None,
        error: None,
        in_parent_job: false,
        capture: None,
        supervisor_id: None,
        game_ids: Vec::new(),
        kind: Some(kind.to_string()),
    }
}

/// `run`'s prepare (it holds the state lock itself, for the whole session).
#[cfg(unix)]
pub(crate) fn prepare_for_run(ctx: &Ctx, steam: SteamMode, tools: bool) -> anyhow::Result<Value> {
    prepare_locked(ctx, steam, tools)
}

/// `run`'s session record (no `--keep-installed`: the volume is always restored).
#[cfg(unix)]
pub(crate) fn new_run_session(ctx: &Ctx, steam: SteamMode, tools: bool, map: Option<String>, game_args: Vec<String>, capture: bool) -> Session {
    let mut s = new_session(ctx, steam, tools, map, game_args, "run");
    s.capture = session_capture(ctx, capture);
    s
}

/// `run`'s restore.
#[cfg(unix)]
pub(crate) fn finish_run(s: &mut Session, ctx: &Ctx, session_file: &Path) -> anyhow::Result<()> {
    finish(s, &ctx.dirs, session_file, ctx.restore_opts(), &ctx.events)
}

/// prepare + start the supervisor + the readiness ladder.
pub fn up(ctx: &Ctx, o: &UpOpts) -> anyhow::Result<Value> {
    let ev = &ctx.events;
    if o.steam == SteamMode::Client {
        // §4.1: the real account. -insecure is in every argv (args::build) and the launcher
        // checks its own tail; this is the visible half of that rule.
        ev.info("--steam client: the game runs with -insecure (VAC off; no official/secure servers) because it carries injected code");
    }
    #[cfg(windows)]
    if o.steam == SteamMode::Client && crate::win::processes_named("steam.exe").is_empty() {
        // with Steam down a direct cs2.exe stops on a modal "Failed to connect with local Steam
        // Client process" dialog (windows-vm-steam.md); refuse before touching anything
        bail!("--steam client: the Steam client is not running; start Steam and log in first (or use --steam gbe)");
    }
    let game_args = args::build(Os::host(), &GameArgs { tools: o.tools, ..o.game.clone() })?;
    // held until the supervisor has recorded its pid and the launcher's: a `down` before that
    // would see a session with nothing to kill and restore under a starting game (FIXME 6)
    let lock = ctx.dirs.lock(LOCK_WAIT, "up", ev)?;
    let prepared = prepare_locked(ctx, o.steam, o.tools)?;
    let capture = session_capture(ctx, o.capture);
    let mut s = new_session(ctx, o.steam, o.tools, o.game.map.clone(), game_args, "up");
    s.keep_installed = o.keep_installed;
    s.capture = capture;
    let sf = ctx.dirs.session_file();
    s.save(&sf)?;
    let pid = match spawn_supervisor(ctx, &o.launcher, &sf) {
        Ok(p) => p,
        Err(e) => {
            // nothing runs: undo the prepare right away
            if ctx.strategy == Strategy::InPlace {
                if let Some(r) = manifest::restore(&ctx.install.root, &ctx.dirs.store(), &ctx.dirs.logs(), ctx.restore_opts())? {
                    report_restore(&r, ev);
                }
            }
            return Err(e.context("starting the supervisor"));
        }
    };
    // the supervisor records its own pid (a save here could undo one of its own)
    ev.info(format!("supervisor pid {pid}; game args: {}", s.args.join(" ")));
    wait_supervisor_recorded(&sf, pid, Duration::from_secs(30), ev);
    drop(lock);
    let mut out = json!({"prepared": prepared, "session": sf, "supervisorPid": pid});
    if o.wait {
        out["ready"] = wait_ready(ctx, o.timeout, &AtomicBool::new(false))?;
    }
    Ok(out)
}

/// Until the session file carries the supervisor's pid **and** has left `starting` (the launcher
/// pid recorded, or a failure), or the supervisor is gone (a failed start: `down`/`uninstall`
/// restore from the manifest), or `timeout`. Only the supervisor's pid is not enough: a `down`
/// in between finds nothing to kill and waits out its whole timeout on the supervisor (seen
/// live on the VM).
fn wait_supervisor_recorded(session_file: &Path, pid: u32, timeout: Duration, ev: &Events) {
    let until = Instant::now() + timeout;
    loop {
        if let Ok(Some(s)) = Session::load(session_file) {
            if s.supervisor_pid == Some(pid) && s.phase != Phase::Starting {
                return;
            }
        }
        if !pid_alive(pid) {
            ev.warn(format!("the supervisor (pid {pid}) exited before it recorded itself; see logs/supervisor.log"));
            return;
        }
        if Instant::now() >= until {
            ev.warn(format!("the supervisor (pid {pid}) has not recorded itself after {} s", timeout.as_secs()));
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn spawn_supervisor(ctx: &Ctx, launcher: &NativeCmd, session_file: &Path) -> anyhow::Result<u32> {
    std::fs::create_dir_all(ctx.dirs.logs())?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(ctx.dirs.logs().join("supervisor.log"))?;
    let mut cmd = launcher.command();
    cmd.arg("supervise").arg(session_file);
    cmd.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        crate::win::no_inherit_std_handles();
        let base = crate::win::DETACHED_PROCESS | crate::win::CREATE_NEW_PROCESS_GROUP;
        cmd.creation_flags(base | crate::win::CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            Ok(c) => Ok(c.id()),
            Err(e) => {
                // a parent job that forbids breakaway: start inside it
                ctx.events.warn(format!("breakaway from the parent job refused ({e}); the supervisor stays in it"));
                cmd.creation_flags(base);
                Ok(cmd.spawn()?.id())
            }
        }
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe; it detaches from our terminal and process group.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        Ok(cmd.spawn()?.id())
    }
}

/// The game's command for session `s`: the Windows launcher (`cs2mcp_launch.exe`) or Linux
/// `cs2.sh` with the mmload `LD_PRELOAD`, the overlay namespace when the session uses it, the
/// capture layer's variables, and nothing of an AppImage's environment. `log`: where its output
/// goes (`None`: inherited, for a container's `podman logs`).
pub(crate) fn game_command(s: &Session, dirs: &Dirs, log: Option<std::fs::File>, ev: &Events) -> anyhow::Result<std::process::Command> {
    let root = &s.install_root;
    let game = root.join("game");
    let mut cmd;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd = std::process::Command::new(game.join("bin").join("win64").join("cs2mcp_launch.exe"));
        cmd.args(&s.args).current_dir(root).creation_flags(crate::win::CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        cmd = std::process::Command::new(game.join("cs2.sh"));
        cmd.args(&s.args);
        // mm-launch README: the interposer is named relative to the game dir (the absolute path
        // has spaces, which ld.so splits on), so the cwd must be the game dir
        let pre = "csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so";
        let ld = match std::env::var("LD_PRELOAD") {
            Ok(v) if !v.is_empty() => format!("{pre} {v}"),
            _ => pre.to_string(),
        };
        cmd.env("LD_PRELOAD", ld).process_group(0);
        match s.strategy {
            #[cfg(target_os = "linux")]
            Strategy::Overlay => crate::overlay::configure(&mut cmd, root, &dirs.upper(), &dirs.work(), &game)?,
            _ => {
                cmd.current_dir(&game);
            }
        }
    }
    // the bridge's default Windows log is the container's Z:\app\...; a log the caller named
    // (linux-client.sh: the host-visible /work) wins
    if std::env::var_os("CS2_BRIDGE_LOG").is_none_or(|v| v.is_empty()) {
        cmd.env("CS2_BRIDGE_LOG", dirs.bridge_log());
    }
    crate::selfexe::scrub_appimage_env(&mut cmd);
    // the capture layer, for the game only: an explicit layer named in its environment (the
    // launcher and cs2.exe inherit it; Linux: cs2.sh and the game inside the namespace)
    if let Some(c) = &s.capture {
        match crate::capture::ensure_dir(&c.dir) {
            Ok(()) => {
                for (k, v) in crate::capture::game_env(c, s.os, std::env::var_os("VK_ADD_LAYER_PATH"), std::env::var_os("VK_INSTANCE_LAYERS")) {
                    cmd.env(k, v);
                }
                ev.info(format!("capture layer {} -> {}", c.layer_dir.display(), c.dir.display()));
            }
            Err(e) => ev.warn(format!("capture dir {}: {e}; the game runs without the capture layer", c.dir.display())),
        }
    }
    cmd.stdin(std::process::Stdio::null());
    if let Some(log) = log {
        cmd.stdout(log.try_clone()?).stderr(log);
    }
    Ok(cmd)
}

/// The supervisor: start the game, wait for it, restore the tree (in-place) when it exits.
pub fn supervise(session_file: &Path, ev: &Events) -> anyhow::Result<()> {
    let mut s = Session::load(session_file)?.context("no session file")?;
    let dirs = Dirs { dir: session_file.parent().context("session file has no dir")?.to_path_buf() };
    std::fs::create_dir_all(dirs.logs())?;
    // ours to record: `up` only learns the pid from spawn and may save after us
    let me = std::process::id();
    s.supervisor_pid = Some(me);
    s.supervisor_id = Some(crate::procid::of(me));
    // already in a job before joining ours = `up`'s job refused the breakaway (FIXME 10): if
    // whoever owns that job closes it, this process and the game die before any restore
    #[cfg(windows)]
    {
        s.in_parent_job = crate::win::in_job();
        if s.in_parent_job {
            ev.warn("the supervisor runs inside its parent's job (breakaway refused): if that job is closed, the game dies with it and the tree stays prepared until `cs2mcp-native down` or `uninstall`");
        }
    }
    s.save(session_file)?;
    let root = s.install_root.clone();

    #[cfg(windows)]
    let job = match crate::win::Job::create_and_join(None) {
        Ok(j) => Some(j),
        Err(e) => {
            ev.warn(format!("job object refused ({e}); the game may outlive a killed supervisor"));
            None
        }
    };

    // a fresh log per session: the readiness ladder greps it (an old Metamod line would pass)
    let log = std::fs::File::create(dirs.launch_log())?;
    let mut cmd = game_command(&s, &dirs, Some(log), ev)?;
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            s.phase = Phase::Failed;
            s.error = Some(format!("spawn: {e}"));
            s.save(session_file)?;
            finish(&mut s, &dirs, session_file, RestoreOpts::default(), ev)?;
            return Err(e.into());
        }
    };
    s.game_pids = vec![child.id()];
    s.game_ids = vec![crate::procid::of(child.id())];
    s.phase = Phase::Running;
    s.save(session_file)?;
    ev.info(format!("launcher pid {}", child.id()));

    let status = child.wait()?;
    s = Session::load(session_file)?.unwrap_or(s);
    s.exit_code = status.code();
    s.phase = Phase::Exited;
    s.save(session_file)?;
    ev.info(format!("game exited: {status}"));
    // a killed launcher's cs2.exe dies with its job, but asynchronously: its files stay open
    // until it is gone, so wait for it before restoring
    if !wait_game_gone(&root, &s.game_pids, Duration::from_secs(60)) {
        ev.warn(format!("game processes still running under {} after 60s: {:?}", root.display(), game_processes(&root, &s.game_pids)));
    }
    // never close the job here: this process is in it, so KILL_ON_JOB_CLOSE would end the
    // supervisor before the restore. The handle closes when the process exits (and kills any
    // straggler then).
    #[cfg(windows)]
    std::mem::forget(job);
    finish(&mut s, &dirs, session_file, RestoreOpts::default(), ev)
}

/// After the game: restore (in place, unless `--keep-installed`), record the result.
pub(crate) fn finish(s: &mut Session, dirs: &Dirs, session_file: &Path, opts: RestoreOpts, ev: &Events) -> anyhow::Result<()> {
    if s.strategy == Strategy::InPlace && !s.keep_installed {
        match manifest::restore(&s.install_root, &dirs.store(), &dirs.logs(), opts) {
            Ok(Some(r)) => {
                report_restore(&r, ev);
                s.restore = Some(serde_json::to_value(&r)?);
            }
            Ok(None) => {}
            Err(e) => {
                ev.warn(format!("restore failed: {e:#}"));
                s.error = Some(format!("restore: {e:#}"));
                if let Some(st) = e.downcast_ref::<Stopped>() {
                    for f in &st.findings {
                        f.report(ev);
                    }
                }
            }
        }
    }
    if s.phase != Phase::Failed {
        s.phase = Phase::Done;
    }
    s.save(session_file)
}

pub fn report_restore(r: &manifest::Report, ev: &Events) {
    ev.info(format!(
        "restore: {} restored, {} removed, {} unchanged, {} conflicts, {} game-written files saved",
        r.restored.len(),
        r.removed.len(),
        r.unchanged.len(),
        r.conflicts.len(),
        r.saved_runtime.len()
    ));
    for c in &r.conflicts {
        ev.warn(format!("  left alone: {c}"));
    }
    for l in &r.lost {
        ev.warn(format!("  lost: {l}"));
    }
    for f in &r.verify_failures {
        ev.warn(format!("  VERIFY FAILED: {f}"));
    }
    if r.byte_for_byte() {
        ev.info(format!("every file we touched is byte-for-byte the user's again ({} entries checked)", r.checked));
    } else if !r.conflicts.is_empty() || !r.lost.is_empty() {
        ev.warn(format!("not byte-for-byte: {}", crate::findings::VERIFY_ADVICE));
    }
    if !r.game_written.is_empty() {
        ev.info(format!("the game itself wrote {} paths in the install during the session (not undone):", r.game_written.len()));
        for g in &r.game_written {
            ev.info(format!("  {g}"));
        }
    }
}

fn tail(path: &Path, n: usize) -> String {
    let Ok(f) = std::fs::File::open(path) else { return String::new() };
    let lines: Vec<String> = std::io::BufReader::new(f).lines().map_while(Result::ok).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn file_contains(path: &Path, needle: &str) -> bool {
    std::fs::read(path).map(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes())).unwrap_or(false)
}

fn mtime_secs(p: &Path) -> Option<f64> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    m.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs_f64())
}

/// The readiness ladder, polled every 500 ms until `timeout` or until `cancel` is set (the GUI
/// runs this on a worker thread; its Stop/Cancel button sets the flag).
pub fn wait_ready(ctx: &Ctx, timeout: Duration, cancel: &AtomicBool) -> anyhow::Result<Value> {
    let ev = &ctx.events;
    let sf = ctx.dirs.session_file();
    let t0 = Instant::now();
    let deadline = t0 + timeout;
    let mut reached: Vec<(String, f64)> = Vec::new();
    let mark = |name: &str, reached: &mut Vec<(String, f64)>| {
        if !reached.iter().any(|(n, _)| n == name) {
            let secs = t0.elapsed().as_secs_f64();
            ev.stage(name, secs);
            reached.push((name.to_string(), secs));
        }
    };
    let mut last_post: Option<Instant> = None;
    let mut hello: Option<probe::Hello> = None;
    let mut last_hello_err = String::new();
    let os = Os::host();
    let marker_log = ctx.dirs.launch_log();
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled; reached {:?}", reached.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>());
        }
        let s = Session::load(&sf)?.context("the session file is gone: the session was stopped (cs2mcp-native down) or finished")?;
        if matches!(s.phase, Phase::Exited | Phase::Done | Phase::Failed) {
            bail!(
                "the game is not running (phase {:?}, exit {:?}{}); launch.log:\n{}",
                s.phase,
                s.exit_code,
                s.error.as_ref().map(|e| format!(", {e}")).unwrap_or_default(),
                tail(&marker_log, 25)
            );
        }
        if s.phase == Phase::Running {
            mark("ProcessUp", &mut reached);
        }
        // the supervisor truncates launch.log before it starts the launcher: until it runs, the
        // file may still be the last session's
        let fresh = s.phase == Phase::Running;
        if fresh && (file_contains(&marker_log, "[cs2mcp-launch] FAIL") || file_contains(&marker_log, "[cs2mcp-mmload] FAIL")) {
            bail!("the launcher failed; launch.log:\n{}", tail(&marker_log, 25));
        }
        if fresh && file_contains(&marker_log, "[cs2mcp-mmload] redirect:") {
            mark("MetamodLoaded", &mut reached);
        }
        if mtime_secs(&s.console_log).is_some_and(|m| m + 2.0 >= s.started_at) {
            mark("ConsoleLogExists", &mut reached);
        }
        let want_bridge = s.tools || s.map.is_some();
        if s.tools && os == Os::Windows && crate::vtm::channel_up() {
            mark("ToolChannel", &mut reached);
            let fresh = mtime_secs(&ctx.discovery).is_some_and(|m| m + 2.0 >= s.started_at);
            if !fresh && last_post.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
                match crate::vtm::post(&crate::vtm::bridge_request()) {
                    Ok(seq) => ev.info(format!("posted tool_request cs2_bridge (seq {seq})")),
                    Err(e) => ev.info(format!("tool_request not posted yet: {e}")),
                }
                last_post = Some(Instant::now());
            }
        }
        if want_bridge && mtime_secs(&ctx.discovery).is_some_and(|m| m + 2.0 >= s.started_at) {
            mark("DiscoveryFresh", &mut reached);
            match probe::read_discovery(&ctx.discovery).and_then(|d| probe::hello(&d, Duration::from_secs(5))) {
                Ok(h) => {
                    mark("BridgeHello", &mut reached);
                    hello = Some(h);
                }
                Err(e) => last_hello_err = format!("{e:#}"),
            }
        }
        let done = if want_bridge { hello.is_some() } else { reached.iter().any(|(n, _)| n == "ConsoleLogExists") };
        if done {
            break;
        }
        if Instant::now() >= deadline {
            bail!(
                "not ready after {}s; reached {:?}{}; launch.log:\n{}",
                timeout.as_secs(),
                reached.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
                if last_hello_err.is_empty() { String::new() } else { format!("; last hello error: {last_hello_err}") },
                tail(&marker_log, 15)
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let s = Session::load(&sf)?.context("the session file is gone: the session was stopped (cs2mcp-native down) or finished")?;
    let mut v = json!({
        "stages": reached.iter().map(|(n, t)| json!({"stage": n, "seconds": (t * 10.0).round() / 10.0})).collect::<Vec<_>>(),
        "consoleLog": s.console_log,
        "discovery": ctx.discovery,
    });
    if let Some(h) = hello {
        v["hello"] = serde_json::to_value(h)?;
    } else if !s.tools && s.map.is_none() {
        v["note"] = json!("client session without --map: the bridge listener starts once a map runs (GameFrame), so BridgeHello was not waited for");
    }
    Ok(v)
}

/// The game's processes running from `root` (Windows: `cs2.exe` and the launcher, by image path,
/// so a cs2.exe started before the launcher recorded anything is found too; and a child of one of
/// `launchers`, the session's recorded launcher pids, whose image is no longer readable).
fn game_processes(root: &Path, launchers: &[u32]) -> Vec<u32> {
    #[cfg(windows)]
    {
        crate::win::processes_under(root, &["cs2mcp_launch.exe", "cs2.exe"], launchers).into_iter().map(|(p, _)| p).collect()
    }
    #[cfg(not(windows))]
    {
        let _ = (root, launchers);
        Vec::new()
    }
}

/// Poll until no [`game_processes`] are left, up to `timeout`.
fn wait_game_gone(root: &Path, launchers: &[u32], timeout: Duration) -> bool {
    let until = Instant::now() + timeout;
    loop {
        if game_processes(root, launchers).is_empty() {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn kill_game(s: &Session, ev: &Events) {
    #[cfg(windows)]
    {
        let mut pids: Vec<u32> = s.game_pids.iter().enumerate().filter(|(i, &p)| crate::procid::alive_as(p, s.game_ids.get(*i))).map(|(_, &p)| p).collect();
        pids.extend(game_processes(&s.install_root, &s.game_pids));
        pids.dedup();
        for &p in &pids {
            match crate::win::terminate(p) {
                Ok(true) => ev.info(format!("terminated pid {p}")),
                Ok(false) => {}
                Err(e) => ev.warn(format!("terminate pid {p}: {e}")),
            }
        }
        // a process handle is signalled once the process has fully terminated
        for &p in &pids {
            crate::win::wait_pid(p, 30_000);
        }
        if !wait_game_gone(&s.install_root, &s.game_pids, Duration::from_secs(30)) {
            ev.warn(format!("game processes still running after 30s: {:?}", game_processes(&s.install_root, &s.game_pids)));
        }
    }
    #[cfg(not(windows))]
    if let Some(&p) = s.game_pids.first() {
        // a pid from another boot or another PID namespace names somebody else's group
        if !crate::procid::alive_as(p, s.game_ids.first()) {
            ev.info(format!("pid {p} is not the session's launcher any more (it exited; the number may be reused): nothing to stop"));
            return;
        }
        crate::run::stop_group(p, Duration::from_secs(15), false, ev);
    }
}

fn wait_gone(pid: u32, timeout: Duration) -> bool {
    #[cfg(windows)]
    {
        crate::win::wait_pid(pid, timeout.as_millis() as u32)
    }
    #[cfg(not(windows))]
    {
        let until = Instant::now() + timeout;
        while Instant::now() < until {
            if !pid_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        !pid_alive(pid)
    }
}

/// Stop the game and make sure the tree is restored.
pub fn down(ctx: &Ctx, timeout: Duration) -> anyhow::Result<Value> {
    let ev = &ctx.events;
    // an `up` still starting holds it until its supervisor has recorded itself (FIXME 6)
    let _lock = ctx.dirs.lock(timeout.max(LOCK_WAIT), "down", ev)?;
    let sf = ctx.dirs.session_file();
    let s = Session::load(&sf)?;
    let mut out = json!({});
    if let Some(s) = &s {
        if s.alive() {
            kill_game(s, ev);
            if let Some(sp) = s.supervisor_pid {
                if crate::procid::alive_as(sp, s.supervisor_id.as_ref()) && !wait_gone(sp, timeout) {
                    ev.warn(format!("supervisor {sp} did not finish within {}s; restoring here", timeout.as_secs()));
                    #[cfg(windows)]
                    let _ = crate::win::terminate(sp);
                }
            }
        }
        let after = Session::load(&sf)?.unwrap_or_else(|| s.clone());
        out["session"] = json!({"phase": after.phase, "exitCode": after.exit_code, "error": after.error});
        if let Some(r) = &after.restore {
            out["restore"] = r.clone();
        }
    }
    // whatever the supervisor did, nothing may stay recorded; never restore under a live game
    let launchers = s.as_ref().map(|s| s.game_pids.clone()).unwrap_or_default();
    let left = game_processes(&ctx.install.root, &launchers);
    if !left.is_empty() && !wait_game_gone(&ctx.install.root, &launchers, Duration::from_secs(30)) {
        bail!("game processes {left:?} still run from {}; stop them, then run `cs2mcp-native uninstall`", ctx.install.root.display());
    }
    if let Some(r) = manifest::restore(&ctx.install.root, &ctx.dirs.store(), &ctx.dirs.logs(), ctx.restore_opts())? {
        report_restore(&r, ev);
        out["restore"] = serde_json::to_value(&r)?;
    }
    let clean = ctx.dirs.store().load()?.is_none();
    out["treeClean"] = json!(clean);
    let mut found = Vec::new();
    if let Some(r) = out.get("restore").cloned() {
        let empty = |k: &str| r[k].as_array().is_none_or(Vec::is_empty);
        out["byteForByte"] = json!(empty("verifyFailures") && empty("conflicts") && empty("lost"));
        if let Ok(rep) = serde_json::from_value::<ReportView>(r) {
            found = rep.findings();
        }
    }
    out["findings"] = json!(found);
    if s.is_some() {
        let ts = manifest::now_secs();
        std::fs::create_dir_all(ctx.dirs.logs())?;
        let _ = std::fs::rename(&sf, ctx.dirs.logs().join(format!("session-{ts}.json")));
    }
    if ctx.strategy == Strategy::Overlay {
        out["overlayUpper"] = json!(ctx.dirs.upper());
        ev.info(format!("overlay: the install was never written; this session's files stay in {} until the next up", ctx.dirs.upper().display()));
    }
    if s.is_none() && !out.as_object().is_some_and(|o| o.contains_key("restore")) {
        ev.info("nothing was running and nothing was recorded");
    }
    if !clean {
        bail!("the tree is not clean: {} still records changes (see the lines above)", ctx.dirs.store().manifest_path().display());
    }
    Ok(out)
}

/// The fields of a stored restore report that its findings need (the supervisor stored it as JSON
/// in the session file).
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct ReportView {
    conflicts: Vec<String>,
    lost: Vec<String>,
    verify_failures: Vec<String>,
}

impl ReportView {
    fn findings(self) -> Vec<Finding> {
        manifest::Report { conflicts: self.conflicts, lost: self.lost, verify_failures: self.verify_failures, ..Default::default() }.findings()
    }
}

/// The findings of a restore report stored as JSON (`Session::restore`).
#[cfg(unix)]
pub(crate) fn restore_findings(r: Option<&Value>) -> Vec<Finding> {
    r.and_then(|r| serde_json::from_value::<ReportView>(r.clone()).ok()).map(ReportView::findings).unwrap_or_default()
}

/// Restore without a session (after a crash or reboot). Refuses while a session runs.
pub fn uninstall(ctx: &Ctx) -> anyhow::Result<Value> {
    let ev = &ctx.events;
    let _lock = ctx.dirs.lock(LOCK_WAIT, "uninstall", ev)?;
    let s = Session::load(&ctx.dirs.session_file())?;
    if let Some(s) = &s {
        if s.alive() {
            bail!("a session is running; use `cs2mcp-native down`");
        }
    }
    let launchers = s.map(|s| s.game_pids).unwrap_or_default();
    let left = game_processes(&ctx.install.root, &launchers);
    if !left.is_empty() {
        bail!("game processes {left:?} run from {}; quit the game first (or `cs2mcp-native down`)", ctx.install.root.display());
    }
    let r = manifest::restore(&ctx.install.root, &ctx.dirs.store(), &ctx.dirs.logs(), ctx.restore_opts())?;
    let mut found = Vec::new();
    if let Some(r) = &r {
        report_restore(r, ev);
        found = r.findings();
    } else {
        ev.info("no manifest: nothing to restore");
        // nothing recorded, but the tree may still hold our files (a deleted state dir)
        let (_, f) = ctx.leftovers();
        if !f.is_empty() {
            report_all(ctx, &f);
            found = f;
        }
    }
    if ctx.strategy == Strategy::Overlay && ctx.dirs.dir.join("overlay").exists() {
        #[cfg(target_os = "linux")]
        crate::overlay::clear(&ctx.dirs.dir.join("overlay"))?;
        ev.info("removed the overlay's upper/work dirs");
    }
    Ok(json!({"restore": r, "treeClean": ctx.dirs.store().load()?.is_none(), "findings": found}))
}

/// Move the leftovers [`Ctx::leftovers`] finds out of the install (`yes`), or say what would be
/// done (`!yes`). Refuses while a manifest is recorded (that is `uninstall`'s) or a session runs.
pub fn cleanup(ctx: &Ctx, yes: bool) -> anyhow::Result<Value> {
    let ev = &ctx.events;
    let _lock = ctx.dirs.lock(LOCK_WAIT, "cleanup", ev)?;
    guard_not_running(ctx)?;
    if ctx.dirs.store().load()?.is_some() {
        bail!("{} records a prepared tree: `cs2mcp-native uninstall` restores it (cleanup is for files no manifest covers)", ctx.dirs.store().manifest_path().display());
    }
    let (seen, found) = ctx.leftovers();
    let actions = crate::leftovers::plan_cleanup(&seen, Os::host());
    let steps: Vec<String> = actions.iter().map(|a| a.describe()).collect();
    if !yes || actions.is_empty() {
        for s in &steps {
            ev.info(format!("would {s}"));
        }
        if actions.is_empty() {
            ev.info("nothing to clean up");
        } else {
            ev.info("run again with --yes to do it (everything taken out goes into the state dir, nothing is deleted)");
        }
        return Ok(json!({"wouldDo": steps, "findings": found, "done": false}));
    }
    let q = ctx.dirs.dir.join(format!("cleanup-{}", manifest::now_secs()));
    let r = crate::leftovers::cleanup(&ctx.install.root, &actions, &q, ev)?;
    let (_, after) = ctx.leftovers();
    for f in &after {
        f.report(ev);
    }
    Ok(json!({"done": true, "result": r, "findings": after}))
}

/// Read-only view.
pub fn status(ctx: &Ctx, hello: bool) -> anyhow::Result<Value> {
    let s = Session::load(&ctx.dirs.session_file())?;
    let m = ctx.dirs.store().load()?;
    let mut drift = Vec::new();
    if let Some(m) = &m {
        for e in &m.entries {
            if let (manifest::Kind::File, Some(a)) = (e.kind, &e.after) {
                let cur = manifest::hash_file(&manifest::under(&ctx.install.root, &e.path))?;
                if cur.as_ref().map(|c| &c.sha256) != Some(&a.sha256) {
                    drift.push(e.path.clone());
                }
            }
        }
    }
    let mut found = Vec::new();
    if !drift.is_empty() {
        found.push(
            Finding::new(Code::Conflicts, Severity::Warn, format!("{} file(s) we wrote were changed since (a Steam update or Verify during the session?)", drift.len()))
                .paths(drift.iter().cloned())
                .advice("they are left as they are at restore; afterwards run Verify integrity of game files in Steam"),
        );
    }
    if m.is_none() {
        found.extend(ctx.leftovers().1);
    }
    found.extend(crate::install::build_finding(ctx.install.build.as_deref()));
    let mut v = json!({
        "install": {"root": ctx.install.root, "build": ctx.install.build, "tools": ctx.install.has_tools,
                    "library": ctx.install.library, "targetBuild": crate::install::target_build(),
                    "others": ctx.other_installs},
        "strategy": ctx.strategy,
        "stateDir": ctx.dirs.dir,
        "lockHeldBy": ctx.dirs.lock_holder(),
        "session": s.as_ref().map(|s| json!({
            "phase": s.phase, "steam": s.steam, "tools": s.tools, "args": s.args, "kind": s.kind,
            "supervisorPid": s.supervisor_pid, "gamePids": s.game_pids, "inParentJob": s.in_parent_job,
            "alive": s.alive(), "exitCode": s.exit_code, "error": s.error,
            "consoleLog": s.console_log, "capture": s.capture})),
        "manifest": m.as_ref().map(|m| json!({"state": m.state, "entries": m.entries.len(), "steam": m.steam,
                    "tools": m.tools, "drift": drift})),
        "discovery": ctx.discovery,
        "findings": found,
    });
    if hello {
        v["hello"] = match probe::read_discovery(&ctx.discovery).and_then(|d| probe::hello(&d, Duration::from_secs(5))) {
            Ok(h) => serde_json::to_value(h)?,
            Err(e) => json!({"error": format!("{e:#}")}),
        };
    }
    Ok(v)
}

/// The MCP client config for this install (`mcpconfig::render` with the context's paths).
pub fn mcp_config(ctx: &Ctx, which: crate::mcpconfig::Which, servers: &crate::mcpconfig::Servers) -> Value {
    let console = ctx.dirs.console_log(&ctx.install.root, ctx.strategy);
    let project = ctx.dirs.dir.join("mcp");
    let capture = ctx.dirs.capture();
    crate::mcpconfig::render(&crate::mcpconfig::Input {
        os: Os::host(),
        servers,
        discovery: &ctx.discovery,
        console_log: &console,
        app_root: (ctx.strategy != Strategy::Overlay).then_some(ctx.install.root.as_path()),
        project_root: (Os::host() == Os::Windows).then_some(project.as_path()),
        build: ctx.install.build.as_deref(),
        capture_dir: crate::capture::layer_dir(&ctx.artifacts.vklayer, Os::host()).map(|_| capture.as_path()),
        which,
    })
}

/// Print a JSON value to stdout (the machine-readable result of every command).
pub fn emit(v: &Value) -> anyhow::Result<()> {
    let mut o = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut o, v)?;
    writeln!(o)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{put, scratch};

    /// A Windows-shaped install + payload in a scratch dir, a [`Ctx`] over them (in place).
    pub(crate) fn fixture(name: &str) -> Ctx {
        let base = scratch(name);
        for f in [
            "art/bridge-rs/win64/cs2_bridge.dll",
            "art/bridge-rs/win64/cs2mcp_launch.exe",
            "art/bridge-rs/win64/cs2mcp_mmload.dll",
            "art/bridge-rs/linuxsteamrt64/libcs2_bridge.so",
            "art/bridge-rs/linuxsteamrt64/libcs2mcp_mmload.so",
            "art/metamod/win64/addons/metamod/bin/win64/server.dll",
            "art/metamod/win64/addons/metamod/bin/win64/metamod.2.cs2.dll",
            "art/metamod/win64/addons/metamod/metaplugins.ini",
            "art/metamod/linux/addons/metamod/bin/linuxsteamrt64/libserver.so",
            "art/gbe/win64/steam_api64.dll",
            "art/gbe/linux64/libsteam_api.so",
            "art/gbe/linux64/steamclient.so",
        ] {
            put(&base.join(f), f.as_bytes());
        }
        let root = base.join("install");
        put(&root.join("game/csgo/steam.inf"), format!("ClientVersion={}\n", crate::install::target_build()).as_bytes());
        put(&root.join("game/bin/win64/steam_api64.dll"), b"..SteamClient017\0SteamUser023\0");
        put(&root.join("game/bin/linuxsteamrt64/libsteam_api.so"), b"SteamUser023\0");
        put(&root.join("game/csgo/console.log"), b"old log");
        let o = CtxOpts {
            game_root: Some(root),
            state_dir: Some(base.join("state")),
            strategy: Some(Strategy::InPlace),
            bridge_dir: Some(base.join("art/bridge-rs")),
            metamod_dir: Some(base.join("art/metamod")),
            gbe_dir: Some(base.join("art/gbe")),
            vk_layer_dir: Some(base.join("art/vklayer")),
            discovery: Some(base.join("discovery.json")),
            ..Default::default()
        };
        Ctx::open(&o, Events::null()).unwrap()
    }

    fn snapshot(root: &Path) -> crate::tree::Listing {
        let mut l = crate::tree::scan(root).unwrap();
        for (k, v) in l.iter_mut() {
            if let Some((size, _)) = v {
                // content, not mtime: a put_back restores the mtime separately
                *v = Some((*size, crate::gbe::sha256_file(&root.join(k)).ok().map(|h| u64::from_str_radix(&h[..12], 16).unwrap())));
            }
        }
        l
    }

    /// Crash mid-apply: the next prepare restores the half-written tree first, says so, then
    /// prepares from the user's bytes.
    #[test]
    fn a_crash_mid_apply_is_recovered_by_the_next_prepare() {
        let ctx = fixture("session-crash");
        let root = ctx.install.root.clone();
        let before = snapshot(&root);
        // a prepare that died after two of its writes: the write-ahead manifest says Applying
        let plan = plan::Plan {
            ops: vec![
                plan::Op { rel: "game/csgo/addons/metamod/cs2mcp.vdf".into(), source: plan::Source::Text("x".into()) },
                plan::Op { rel: "game/bin/win64/steam_api64.dll".into(), source: plan::Source::Text("gbe".into()) },
                plan::Op { rel: "game/bin/sdkenginetools.txt".into(), source: plan::Source::EditSdkEngineTools },
            ],
            runtime: vec![],
        };
        let hdr = Header { build: None, steam: SteamMode::Gbe, tools: false };
        assert!(manifest::apply(&plan, &root, &ctx.dirs.store(), &hdr).is_err(), "no sdkenginetools.txt: the third op fails");
        assert_eq!(ctx.dirs.store().load().unwrap().unwrap().state, manifest::State::Applying);
        let v = prepare(&ctx, SteamMode::Client, false).unwrap();
        let f: Vec<Finding> = serde_json::from_value(v["findings"].clone()).unwrap();
        let rec = f.iter().find(|f| f.code == Code::Recovered).expect("a recovered finding");
        // the vdf and the two dirs above it removed, the DLL put back
        assert!(rec.title.starts_with("recovered 4 entries"), "{}", rec.title);
        assert!(rec.title.contains("interrupted mid-way"));
        assert_eq!(v["recovered"]["previousState"], "applying");
        // the fresh prepare took the user's DLL as `before`, not the half-applied gbe bytes
        let m = ctx.dirs.store().load().unwrap().unwrap();
        assert!(m.entries.iter().all(|e| e.path != "game/bin/win64/steam_api64.dll"), "client mode does not touch it");
        uninstall(&ctx).unwrap();
        assert_eq!(snapshot(&root), before);
    }

    /// The state dir was deleted while the tree was prepared: the next prepare stops on the
    /// orphans (a decision), `--accept-conflicts` goes on, `cleanup --yes` removes them.
    #[test]
    fn a_deleted_state_dir_leaves_orphans_that_stop_prepare_until_decided() {
        let mut ctx = fixture("session-orphans");
        let root = ctx.install.root.clone();
        let before = snapshot(&root);
        prepare(&ctx, SteamMode::Client, false).unwrap();
        std::fs::remove_dir_all(&ctx.dirs.dir).unwrap();
        let e = prepare(&ctx, SteamMode::Client, false).unwrap_err();
        let st = e.downcast_ref::<Stopped>().expect("stopped on findings");
        assert!(st.findings.iter().any(|f| f.code == Code::Orphans && f.severity == Severity::Decision), "{:?}", st.findings);
        let st_json = status(&ctx, false).unwrap();
        assert!(st_json["findings"].as_array().unwrap().iter().any(|f| f["code"] == "orphans"));
        // dry run first, then the real thing
        let dry = cleanup(&ctx, false).unwrap();
        assert_eq!(dry["done"], false);
        assert!(!dry["wouldDo"].as_array().unwrap().is_empty());
        let done = cleanup(&ctx, true).unwrap();
        assert_eq!(done["done"], true);
        assert_eq!(snapshot(&root), before, "back to the user's tree");
        // and the decision path: accepted orphans are kept as the tree's own
        prepare(&ctx, SteamMode::Client, false).unwrap();
        std::fs::remove_dir_all(&ctx.dirs.dir).unwrap();
        ctx.policy.accept_conflicts = true;
        let v = prepare(&ctx, SteamMode::Client, false).unwrap();
        assert!(v["findings"].as_array().unwrap().iter().any(|f| f["code"] == "orphans"));
        uninstall(&ctx).unwrap();
    }

    /// Lost backups stop `uninstall` with the paths; `--forget-missing-backups` goes on.
    #[test]
    fn missing_backups_stop_uninstall_until_forgotten() {
        let mut ctx = fixture("session-backups");
        prepare(&ctx, SteamMode::Gbe, false).unwrap();
        std::fs::remove_dir_all(ctx.dirs.dir.join("backup")).unwrap();
        let e = uninstall(&ctx).unwrap_err();
        let st = e.downcast_ref::<Stopped>().unwrap();
        assert_eq!(st.findings[0].code, Code::MissingBackups);
        assert!(st.findings[0].paths.iter().any(|p| p.starts_with(plan::steam_api_rel(Os::host()))), "{:?}", st.findings[0].paths);
        ctx.policy.forget_missing_backups = true;
        let v = uninstall(&ctx).unwrap();
        assert_eq!(v["treeClean"], true);
        assert!(v["findings"].as_array().unwrap().iter().any(|f| f["code"] == "lost-backups"));
    }

    /// Another build than the shipped pins: a warning at prepare, and prepare goes on.
    #[test]
    fn another_game_build_warns_and_continues() {
        let ctx = fixture("session-build");
        put(&ctx.install.root.join("game/csgo/steam.inf"), b"ClientVersion=1\n");
        let ctx = Ctx { install: crate::install::Install::at(&ctx.install.root, None).unwrap(), ..ctx };
        let v = prepare(&ctx, SteamMode::Client, false).unwrap();
        let f: Vec<Finding> = serde_json::from_value(v["findings"].clone()).unwrap();
        assert!(f.iter().any(|f| f.code == Code::BuildMismatch && f.severity == Severity::Warn));
        uninstall(&ctx).unwrap();
    }

    /// A lock held by another command: the waiting one says which.
    #[test]
    fn a_held_lock_names_its_holder() {
        let ctx = fixture("session-lock");
        let _held = ctx.dirs.lock(Duration::from_secs(0), "up", &Events::null()).unwrap();
        // the lock is per open file description: a second open in this process conflicts too
        let e = ctx.dirs.lock(Duration::from_millis(200), "prepare", &ctx.events).unwrap_err().to_string();
        assert!(e.contains("cs2mcp-native up"), "{e}");
        assert_eq!(status(&ctx, false).unwrap()["lockHeldBy"]["command"], "up");
    }
}
