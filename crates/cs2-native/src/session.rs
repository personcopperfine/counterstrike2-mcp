//! `prepare` / `up` / `supervise` / `down` / `uninstall` / `status`: the session lifecycle over the
//! pure modules (README "Launch, processes and readiness").

use crate::args::{self, GameArgs};
use crate::install::{Install, Os};
use crate::manifest::{self, Header};
use crate::plan::{self, Artifacts, PlanInput, SteamMode};
use crate::probe;
use crate::state::{now_f64, Dirs, Phase, Session, Strategy, SESSION_FORMAT};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Everything a command needs about the install.
pub struct Ctx {
    pub install: Install,
    pub dirs: Dirs,
    pub strategy: Strategy,
    pub artifacts: Artifacts,
    pub discovery: PathBuf,
}

pub fn say(msg: impl AsRef<str>) {
    eprintln!("[cs2mcp-native] {}", msg.as_ref());
}

/// Is our previous session still running?
fn session_alive(s: &Session) -> bool {
    s.supervisor_pid.is_some_and(pid_alive) || s.game_pids.iter().any(|&p| pid_alive(p))
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
        if session_alive(&s) {
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
const LOCK_WAIT: Duration = Duration::from_secs(120);

/// Put the tree (or the overlay's upper dir) into the prepared state.
pub fn prepare(ctx: &Ctx, steam: SteamMode, tools: bool) -> anyhow::Result<Value> {
    let _lock = ctx.dirs.lock(LOCK_WAIT)?;
    prepare_locked(ctx, steam, tools)
}

fn prepare_locked(ctx: &Ctx, steam: SteamMode, tools: bool) -> anyhow::Result<Value> {
    guard_not_running(ctx)?;
    if tools && !ctx.install.has_tools {
        bail!(
            "--tools: this install has no Workshop Tools (game/bin/win64/tools/hammer.dll + game/bin/sdkenginetools.txt); install the \"Counter-Strike 2 Workshop Tools\" DLC in Steam"
        );
    }
    let os = Os::host();
    let saves = ctx.dirs.gbe_saves();
    std::fs::create_dir_all(&saves)?;
    let input = PlanInput { os, steam, tools, root: &ctx.install.root, gbe_save_dir: &saves };
    match ctx.strategy {
        Strategy::InPlace => {
            let store = ctx.dirs.store();
            if store.load()?.is_some() {
                say("an earlier prepare is still recorded; restoring it first");
                if let Some(r) = manifest::restore(&ctx.install.root, &store, &ctx.dirs.logs())? {
                    report_restore(&r);
                    if !r.verify_failures.is_empty() {
                        bail!("the earlier prepare could not be restored cleanly; fix the files above, then `cs2mcp-native uninstall`");
                    }
                }
            }
            // plan only after the restore: `before` must be the user's current bytes
            let plan = plan::build(&input, &ctx.artifacts)?;
            let header = Header { build: ctx.install.build.clone(), steam, tools };
            let m = manifest::apply(&plan, &ctx.install.root, &store, &header)?;
            let changed = m.entries.iter().filter(|e| e.role == manifest::Role::Ours).count();
            say(format!("prepared in place: {changed} paths recorded in {}", store.manifest_path().display()));
            Ok(json!({"strategy": "in-place", "manifest": store.manifest_path(), "entries": m.entries.len()}))
        }
        Strategy::Overlay => {
            #[cfg(target_os = "linux")]
            {
                let plan = plan::build(&input, &ctx.artifacts)?;
                let staged = crate::overlay::stage(&plan, &ctx.install.root, &ctx.dirs.upper(), &ctx.dirs.work())?;
                crate::overlay::probe(&ctx.dirs.dir.join("overlay-probe")).context(
                    "this system refuses an unprivileged user+mount namespace with overlayfs; use --strategy in-place",
                )?;
                say(format!("staged {} files into {} (the install is not touched)", staged.len(), ctx.dirs.upper().display()));
                Ok(json!({"strategy": "overlay", "upper": ctx.dirs.upper(), "files": staged.len()}))
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
}

/// prepare + start the supervisor + the readiness ladder.
pub fn up(ctx: &Ctx, o: &UpOpts) -> anyhow::Result<Value> {
    if o.steam == SteamMode::Client {
        // §4.1: the real account. -insecure is in every argv (args::build) and the launcher
        // checks its own tail; this is the visible half of that rule.
        say("--steam client: the game runs with -insecure (VAC off; no official/secure servers) because it carries injected code");
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
    let lock = ctx.dirs.lock(LOCK_WAIT)?;
    let prepared = prepare_locked(ctx, o.steam, o.tools)?;
    let console_log = ctx.dirs.console_log(&ctx.install.root, ctx.strategy);
    let capture = if o.capture {
        match crate::capture::layer_dir(&ctx.artifacts.vklayer, Os::host()) {
            Some(layer_dir) => Some(crate::capture::Capture { layer_dir, dir: ctx.dirs.capture() }),
            None => {
                say(format!("{}: screenshot/observe will have no frames", crate::capture::missing(&ctx.artifacts.vklayer, Os::host())));
                None
            }
        }
    } else {
        None
    };
    let s = Session {
        format: SESSION_FORMAT.into(),
        install_root: ctx.install.root.clone(),
        os: Os::host(),
        strategy: ctx.strategy,
        steam: o.steam,
        tools: o.tools,
        map: o.game.map.clone(),
        args: game_args,
        started_at: now_f64(),
        supervisor_pid: None,
        game_pids: Vec::new(),
        phase: Phase::Starting,
        exit_code: None,
        keep_installed: o.keep_installed,
        discovery: Some(ctx.discovery.clone()),
        console_log,
        restore: None,
        error: None,
        in_parent_job: false,
        capture,
    };
    let sf = ctx.dirs.session_file();
    s.save(&sf)?;
    let pid = match spawn_supervisor(ctx, &sf) {
        Ok(p) => p,
        Err(e) => {
            // nothing runs: undo the prepare right away
            if ctx.strategy == Strategy::InPlace {
                if let Some(r) = manifest::restore(&ctx.install.root, &ctx.dirs.store(), &ctx.dirs.logs())? {
                    report_restore(&r);
                }
            }
            return Err(e.context("starting the supervisor"));
        }
    };
    // the supervisor records its own pid (a save here could undo one of its own)
    say(format!("supervisor pid {pid}; game args: {}", s.args.join(" ")));
    wait_supervisor_recorded(&sf, pid, Duration::from_secs(30));
    drop(lock);
    let mut out = json!({"prepared": prepared, "session": sf, "supervisorPid": pid});
    if o.wait {
        out["ready"] = wait_ready(ctx, o.timeout)?;
    }
    Ok(out)
}

/// Until the session file carries the supervisor's pid **and** has left `starting` (the launcher
/// pid recorded, or a failure), or the supervisor is gone (a failed start: `down`/`uninstall`
/// restore from the manifest), or `timeout`. Only the supervisor's pid is not enough: a `down`
/// in between finds nothing to kill and waits out its whole timeout on the supervisor (seen
/// live on the VM).
fn wait_supervisor_recorded(session_file: &Path, pid: u32, timeout: Duration) {
    let until = Instant::now() + timeout;
    loop {
        if let Ok(Some(s)) = Session::load(session_file) {
            if s.supervisor_pid == Some(pid) && s.phase != Phase::Starting {
                return;
            }
        }
        if !pid_alive(pid) {
            say(format!("the supervisor (pid {pid}) exited before it recorded itself; see logs/supervisor.log"));
            return;
        }
        if Instant::now() >= until {
            say(format!("the supervisor (pid {pid}) has not recorded itself after {} s", timeout.as_secs()));
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn spawn_supervisor(ctx: &Ctx, session_file: &Path) -> anyhow::Result<u32> {
    let exe = std::env::current_exe()?;
    std::fs::create_dir_all(ctx.dirs.logs())?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(ctx.dirs.logs().join("supervisor.log"))?;
    let mut cmd = std::process::Command::new(exe);
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
                say(format!("breakaway from the parent job refused ({e}); the supervisor stays in it"));
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

/// The supervisor: start the game, wait for it, restore the tree (in-place) when it exits.
pub fn supervise(session_file: &Path) -> anyhow::Result<()> {
    let mut s = Session::load(session_file)?.context("no session file")?;
    let dirs = Dirs { dir: session_file.parent().context("session file has no dir")?.to_path_buf() };
    std::fs::create_dir_all(dirs.logs())?;
    // ours to record: `up` only learns the pid from spawn and may save after us
    s.supervisor_pid = Some(std::process::id());
    // already in a job before joining ours = `up`'s job refused the breakaway (FIXME 10): if
    // whoever owns that job closes it, this process and the game die before any restore
    #[cfg(windows)]
    {
        s.in_parent_job = crate::win::in_job();
        if s.in_parent_job {
            say("the supervisor runs inside its parent's job (breakaway refused): if that job is closed, the game dies with it and the tree stays prepared until `cs2mcp-native down` or `uninstall`");
        }
    }
    s.save(session_file)?;
    let root = s.install_root.clone();
    let game = root.join("game");
    // the bridge's default Windows log is the container's Z:\app\...
    std::env::set_var("CS2_BRIDGE_LOG", dirs.bridge_log());

    #[cfg(windows)]
    let job = match crate::win::Job::create_and_join(None) {
        Ok(j) => Some(j),
        Err(e) => {
            say(format!("job object refused ({e}); the game may outlive a killed supervisor"));
            None
        }
    };

    // a fresh log per session: the readiness ladder greps it (an old Metamod line would pass)
    let log = std::fs::File::create(dirs.launch_log())?;
    let mut cmd;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd = std::process::Command::new(game.join("bin").join("win64").join("cs2mcp_launch.exe"));
        cmd.args(&s.args).current_dir(&root).creation_flags(crate::win::CREATE_NO_WINDOW);
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
            Strategy::Overlay => crate::overlay::configure(&mut cmd, &root, &dirs.upper(), &dirs.work(), &game)?,
            _ => {
                cmd.current_dir(&game);
            }
        }
    }
    // the capture layer, for the game only: an explicit layer named in its environment (the
    // launcher and cs2.exe inherit it; Linux: cs2.sh and the game inside the namespace)
    if let Some(c) = &s.capture {
        match crate::capture::ensure_dir(&c.dir) {
            Ok(()) => {
                for (k, v) in crate::capture::game_env(
                    c,
                    s.os,
                    std::env::var_os("VK_ADD_LAYER_PATH"),
                    std::env::var_os("VK_INSTANCE_LAYERS"),
                ) {
                    cmd.env(k, v);
                }
                say(format!("capture layer {} -> {}", c.layer_dir.display(), c.dir.display()));
            }
            Err(e) => say(format!("capture dir {}: {e}; the game runs without the capture layer", c.dir.display())),
        }
    }
    cmd.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            s.phase = Phase::Failed;
            s.error = Some(format!("spawn: {e}"));
            s.save(session_file)?;
            finish(&mut s, &dirs, session_file)?;
            return Err(e.into());
        }
    };
    s.game_pids = vec![child.id()];
    s.phase = Phase::Running;
    s.save(session_file)?;
    say(format!("launcher pid {}", child.id()));

    let status = child.wait()?;
    s = Session::load(session_file)?.unwrap_or(s);
    s.exit_code = status.code();
    s.phase = Phase::Exited;
    s.save(session_file)?;
    say(format!("game exited: {status}"));
    // a killed launcher's cs2.exe dies with its job, but asynchronously: its files stay open
    // until it is gone, so wait for it before restoring
    if !wait_game_gone(&root, &s.game_pids, Duration::from_secs(60)) {
        say(format!("game processes still running under {} after 60s: {:?}", root.display(), game_processes(&root, &s.game_pids)));
    }
    // never close the job here: this process is in it, so KILL_ON_JOB_CLOSE would end the
    // supervisor before the restore. The handle closes when the process exits (and kills any
    // straggler then).
    #[cfg(windows)]
    std::mem::forget(job);
    finish(&mut s, &dirs, session_file)
}

fn finish(s: &mut Session, dirs: &Dirs, session_file: &Path) -> anyhow::Result<()> {
    if s.strategy == Strategy::InPlace && !s.keep_installed {
        match manifest::restore(&s.install_root, &dirs.store(), &dirs.logs()) {
            Ok(Some(r)) => {
                report_restore(&r);
                s.restore = Some(serde_json::to_value(&r)?);
            }
            Ok(None) => {}
            Err(e) => {
                say(format!("restore failed: {e:#}"));
                s.error = Some(format!("restore: {e:#}"));
            }
        }
    }
    if s.phase != Phase::Failed {
        s.phase = Phase::Done;
    }
    s.save(session_file)
}

pub fn report_restore(r: &manifest::Report) {
    say(format!(
        "restore: {} restored, {} removed, {} unchanged, {} conflicts, {} game-written files saved",
        r.restored.len(),
        r.removed.len(),
        r.unchanged.len(),
        r.conflicts.len(),
        r.saved_runtime.len()
    ));
    for c in &r.conflicts {
        say(format!("  left alone: {c}"));
    }
    for f in &r.verify_failures {
        say(format!("  VERIFY FAILED: {f}"));
    }
    if r.byte_for_byte() {
        say(format!("every file we touched is byte-for-byte the user's again ({} entries checked)", r.checked));
    }
    if !r.game_written.is_empty() {
        say(format!("the game itself wrote {} paths in the install during the session (not undone):", r.game_written.len()));
        for g in &r.game_written {
            say(format!("  {g}"));
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

/// The readiness ladder, polled every 500 ms until `timeout`.
pub fn wait_ready(ctx: &Ctx, timeout: Duration) -> anyhow::Result<Value> {
    let sf = ctx.dirs.session_file();
    let t0 = Instant::now();
    let deadline = t0 + timeout;
    let mut reached: Vec<(String, f64)> = Vec::new();
    let mark = |name: &str, reached: &mut Vec<(String, f64)>| {
        if !reached.iter().any(|(n, _)| n == name) {
            let secs = t0.elapsed().as_secs_f64();
            say(format!("ready: {name} after {secs:.1}s"));
            reached.push((name.to_string(), secs));
        }
    };
    let mut last_post: Option<Instant> = None;
    let mut hello: Option<probe::Hello> = None;
    let mut last_hello_err = String::new();
    let os = Os::host();
    let marker_log = ctx.dirs.launch_log();
    loop {
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
                    Ok(seq) => say(format!("posted tool_request cs2_bridge (seq {seq})")),
                    Err(e) => say(format!("tool_request not posted yet: {e}")),
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

fn kill_game(s: &Session) {
    #[cfg(windows)]
    {
        let mut pids = s.game_pids.clone();
        pids.extend(game_processes(&s.install_root, &s.game_pids));
        pids.dedup();
        for &p in &pids {
            match crate::win::terminate(p) {
                Ok(true) => say(format!("terminated pid {p}")),
                Ok(false) => {}
                Err(e) => say(format!("terminate pid {p}: {e}")),
            }
        }
        // a process handle is signalled once the process has fully terminated
        for &p in &pids {
            crate::win::wait_pid(p, 30_000);
        }
        if !wait_game_gone(&s.install_root, &s.game_pids, Duration::from_secs(30)) {
            say(format!("game processes still running after 30s: {:?}", game_processes(&s.install_root, &s.game_pids)));
        }
    }
    #[cfg(not(windows))]
    if let Some(&p) = s.game_pids.first() {
        // the launcher is the leader of its own process group (process_group(0))
        // SAFETY: plain signal to a recorded process group.
        unsafe { libc::kill(-(p as i32), libc::SIGTERM) };
        let until = Instant::now() + Duration::from_secs(15);
        while Instant::now() < until && s.game_pids.iter().any(|&q| pid_alive(q)) {
            std::thread::sleep(Duration::from_millis(200));
        }
        if s.game_pids.iter().any(|&q| pid_alive(q)) {
            unsafe { libc::kill(-(p as i32), libc::SIGKILL) };
            say(format!("process group {p} killed"));
        } else {
            say(format!("process group {p} ended"));
        }
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
    // an `up` still starting holds it until its supervisor has recorded itself (FIXME 6)
    let _lock = ctx.dirs.lock(timeout.max(LOCK_WAIT))?;
    let sf = ctx.dirs.session_file();
    let s = Session::load(&sf)?;
    let mut out = json!({});
    if let Some(s) = &s {
        if session_alive(s) {
            kill_game(s);
            if let Some(sp) = s.supervisor_pid {
                if !wait_gone(sp, timeout) {
                    say(format!("supervisor {sp} did not finish within {}s; restoring here", timeout.as_secs()));
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
    if let Some(r) = manifest::restore(&ctx.install.root, &ctx.dirs.store(), &ctx.dirs.logs())? {
        report_restore(&r);
        out["restore"] = serde_json::to_value(&r)?;
    }
    let clean = ctx.dirs.store().load()?.is_none();
    out["treeClean"] = json!(clean);
    if let Some(r) = out.get("restore") {
        let bfb = r["verifyFailures"].as_array().is_some_and(Vec::is_empty) && r["conflicts"].as_array().is_some_and(Vec::is_empty);
        out["byteForByte"] = json!(bfb);
    }
    if s.is_some() {
        let ts = manifest::now_secs();
        std::fs::create_dir_all(ctx.dirs.logs())?;
        let _ = std::fs::rename(&sf, ctx.dirs.logs().join(format!("session-{ts}.json")));
    }
    if ctx.strategy == Strategy::Overlay {
        out["overlayUpper"] = json!(ctx.dirs.upper());
        say(format!("overlay: the install was never written; this session's files stay in {} until the next up", ctx.dirs.upper().display()));
    }
    if s.is_none() && !out.as_object().is_some_and(|o| o.contains_key("restore")) {
        say("nothing was running and nothing was recorded");
    }
    if !clean {
        bail!("the tree is not clean: {} still records changes (see the lines above)", ctx.dirs.store().manifest_path().display());
    }
    Ok(out)
}

/// Restore without a session (after a crash or reboot). Refuses while a session runs.
pub fn uninstall(ctx: &Ctx) -> anyhow::Result<Value> {
    let _lock = ctx.dirs.lock(LOCK_WAIT)?;
    let s = Session::load(&ctx.dirs.session_file())?;
    if let Some(s) = &s {
        if session_alive(s) {
            bail!("a session is running; use `cs2mcp-native down`");
        }
    }
    let launchers = s.map(|s| s.game_pids).unwrap_or_default();
    let left = game_processes(&ctx.install.root, &launchers);
    if !left.is_empty() {
        bail!("game processes {left:?} run from {}; quit the game first (or `cs2mcp-native down`)", ctx.install.root.display());
    }
    let r = manifest::restore(&ctx.install.root, &ctx.dirs.store(), &ctx.dirs.logs())?;
    if let Some(r) = &r {
        report_restore(r);
    } else {
        say("no manifest: nothing to restore");
    }
    if ctx.strategy == Strategy::Overlay && ctx.dirs.dir.join("overlay").exists() {
        #[cfg(target_os = "linux")]
        crate::overlay::clear(&ctx.dirs.dir.join("overlay"))?;
        say("removed the overlay's upper/work dirs");
    }
    Ok(json!({"restore": r, "treeClean": ctx.dirs.store().load()?.is_none()}))
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
    let mut v = json!({
        "install": {"root": ctx.install.root, "build": ctx.install.build, "tools": ctx.install.has_tools,
                    "library": ctx.install.library},
        "strategy": ctx.strategy,
        "stateDir": ctx.dirs.dir,
        "session": s.as_ref().map(|s| json!({
            "phase": s.phase, "steam": s.steam, "tools": s.tools, "args": s.args,
            "supervisorPid": s.supervisor_pid, "gamePids": s.game_pids, "inParentJob": s.in_parent_job,
            "alive": session_alive(s), "exitCode": s.exit_code, "error": s.error,
            "consoleLog": s.console_log, "capture": s.capture})),
        "manifest": m.as_ref().map(|m| json!({"state": m.state, "entries": m.entries.len(), "steam": m.steam,
                    "tools": m.tools, "drift": drift})),
        "discovery": ctx.discovery,
    });
    if hello {
        v["hello"] = match probe::read_discovery(&ctx.discovery).and_then(|d| probe::hello(&d, Duration::from_secs(5))) {
            Ok(h) => serde_json::to_value(h)?,
            Err(e) => json!({"error": format!("{e:#}")}),
        };
    }
    Ok(v)
}

/// Print a JSON value to stdout (the machine-readable result of every command).
pub fn emit(v: &Value) -> anyhow::Result<()> {
    let mut o = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut o, v)?;
    writeln!(o)?;
    Ok(())
}
