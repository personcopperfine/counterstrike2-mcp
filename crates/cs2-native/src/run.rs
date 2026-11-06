//! `cs2mcp-native run`: a foreground session for containers. One command prepares the game on
//! the volume where it lives (in place, with the manifest; overlays need a user namespace that
//! container runtimes usually refuse), runs it in its own process group, and restores and
//! verifies the volume before it exits, with the game's exit code. SIGTERM / SIGINT / SIGHUP stop
//! the game the way `down` does (TERM to the group, KILL after the grace), then restore.
//!
//! The state dir belongs on the same volume (`--state-dir <volume>/.cs2mcp-native`): a container
//! that was killed before its restore leaves the manifest there, and the next `run` restores it
//! first (`prepare` does that for any recorded manifest). The state lock is held for the whole
//! session, so a second container on the same volume waits and says which one holds it; pids in
//! an old `session.json` from another PID namespace are recognised as stale (`procid`).
//!
//! It may be PID 1: it becomes a child subreaper and reaps every child it inherits.

use crate::args::GameArgs;
use crate::plan::SteamMode;
use crate::session::Ctx;
use std::time::Duration;

pub struct RunOpts {
    pub steam: SteamMode,
    pub tools: bool,
    pub game: GameArgs,
    pub capture: bool,
    /// move leftovers of the old container layout (`cargo bridge install`, setup_gbe.sh) out first
    pub cleanup_legacy: bool,
}

/// How long the game gets between SIGTERM and SIGKILL (`podman stop -t` must leave room for this
/// plus the restore).
pub const STOP_GRACE: Duration = Duration::from_secs(15);

#[cfg(unix)]
mod imp {
    use super::*;
    use crate::args;
    use crate::events::Events;
    use crate::install::Os;
    use crate::state::{Phase, Strategy};
    use anyhow::bail;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::time::Instant;

    static SIGNALLED: AtomicI32 = AtomicI32::new(0);

    extern "C" fn on_signal(sig: libc::c_int) {
        SIGNALLED.store(sig, Ordering::SeqCst);
    }

    /// Note SIGTERM/SIGINT/SIGHUP instead of dying of them (the restore must run).
    fn install_handlers() {
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            // SAFETY: a plain handler that only stores to an atomic (async-signal-safe).
            unsafe {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = on_signal as *const () as usize;
                libc::sigemptyset(&mut sa.sa_mask);
                sa.sa_flags = libc::SA_RESTART;
                libc::sigaction(sig, &sa, std::ptr::null_mut());
            }
        }
    }

    pub fn signalled() -> Option<i32> {
        match SIGNALLED.load(Ordering::SeqCst) {
            0 => None,
            s => Some(s),
        }
    }

    /// Reap every exited child (`run` owns all of its children: it may be PID 1); return the wait
    /// status of `pid` if it was among them.
    fn reap(pid: i32) -> Option<i32> {
        let mut got = None;
        loop {
            let mut st = 0;
            // SAFETY: plain non-blocking wait for any child.
            let r = unsafe { libc::waitpid(-1, &mut st, libc::WNOHANG) };
            if r <= 0 {
                return got;
            }
            if r == pid {
                got = Some(st);
            }
        }
    }

    /// Reap `pid` alone if it is our exited child (a caller with other children of its own, the
    /// GUI or a test, must not take theirs); its wait status if it was reaped.
    fn reap_one(pid: i32) -> Option<i32> {
        let mut st = 0;
        // SAFETY: plain non-blocking wait for one pid; ECHILD (not ours) is ignored.
        (unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) } == pid).then_some(st)
    }

    fn group_alive(pgid: i32) -> bool {
        // SAFETY: signal 0 to a process group only checks existence.
        unsafe { libc::kill(-pgid, 0) == 0 }
    }

    /// TERM the process group `pgid`, KILL it after `grace`; returns when the group is gone or
    /// after a further 15 s. `reap_any`: reap every child meanwhile (`run`, which owns all of
    /// them); else only the group leader, if it is ours. Returns the leader's wait status when it
    /// was reaped here (the caller's own wait for it would never see it again).
    pub fn stop_group(pgid: u32, grace: Duration, reap_any: bool, ev: &Events) -> Option<i32> {
        let g = pgid as i32;
        let mut leader = None;
        let mut reap_now = || {
            let st = if reap_any { reap(g) } else { reap_one(g) };
            leader = leader.or(st);
        };
        // SAFETY: a signal to the recorded process group.
        unsafe { libc::kill(-g, libc::SIGTERM) };
        let until = Instant::now() + grace;
        while Instant::now() < until && group_alive(g) {
            reap_now();
            std::thread::sleep(Duration::from_millis(100));
        }
        if group_alive(g) {
            // SAFETY: as above.
            unsafe { libc::kill(-g, libc::SIGKILL) };
            ev.info(format!("process group {g} killed"));
            let until = Instant::now() + Duration::from_secs(15);
            while Instant::now() < until && group_alive(g) {
                reap_now();
                std::thread::sleep(Duration::from_millis(100));
            }
        } else {
            ev.info(format!("process group {g} ended"));
        }
        // the leader may have exited after the last pass (a zombie keeps the group alive)
        reap_now();
        leader
    }

    /// The exit code of a wait status: the code, or 128 + the signal.
    fn code_of(st: i32) -> i32 {
        if libc::WIFEXITED(st) {
            libc::WEXITSTATUS(st)
        } else if libc::WIFSIGNALED(st) {
            128 + libc::WTERMSIG(st)
        } else {
            1
        }
    }

    pub fn run(ctx: &Ctx, o: &RunOpts) -> anyhow::Result<(Value, i32)> {
        let ev = &ctx.events;
        if ctx.strategy != Strategy::InPlace {
            bail!("run prepares the volume in place (--strategy in-place); overlays need a user namespace that containers refuse");
        }
        install_handlers();
        // PID 1 or not: orphans of the game come to us and are reaped below
        // SAFETY: plain prctl on this process.
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
        let game_args = args::build(Os::host(), &GameArgs { tools: o.tools, ..o.game.clone() })?;
        // the whole session: another container on this volume waits here and is told who holds it
        let _lock = ctx.dirs.lock(crate::session::LOCK_WAIT, "run", ev)?;
        let mut cleaned = Value::Null;
        if o.cleanup_legacy && ctx.dirs.store().load()?.is_none() {
            let (seen, _) = ctx.leftovers();
            let actions = crate::leftovers::plan_cleanup(&seen, Os::host());
            if !actions.is_empty() {
                let q = ctx.dirs.dir.join(format!("cleanup-{}", crate::manifest::now_secs()));
                cleaned = crate::leftovers::cleanup(&ctx.install.root, &actions, &q, ev)?;
            }
        }
        let prepared = crate::session::prepare_for_run(ctx, o.steam, o.tools)?;
        let sf = ctx.dirs.session_file();
        let me = std::process::id();
        let mut s = crate::session::new_run_session(ctx, o.steam, o.tools, o.game.map.clone(), game_args, o.capture);
        s.supervisor_pid = Some(me);
        s.supervisor_id = Some(crate::procid::of(me));
        s.save(&sf)?;
        let mut stopped_by: Option<i32> = signalled();
        let mut code;
        if stopped_by.is_none() {
            let mut cmd = crate::session::game_command(&s, &ctx.dirs, None, ev)?;
            // stdout carries our JSON result: the game's output goes to the container log (stderr)
            {
                use std::os::fd::AsFd;
                cmd.stdout(std::io::stderr().as_fd().try_clone_to_owned()?);
            }
            match cmd.spawn() {
                Ok(child) => {
                    let pid = child.id();
                    // reaped below with waitpid(-1): the Child handle is never waited on
                    drop(child);
                    s.game_pids = vec![pid];
                    s.game_ids = vec![crate::procid::of(pid)];
                    s.phase = Phase::Running;
                    s.save(&sf)?;
                    ev.info(format!("game pid {pid} (process group); args: {}", s.args.join(" ")));
                    let st = loop {
                        if let Some(st) = reap(pid as i32) {
                            break st;
                        }
                        if stopped_by.is_none() {
                            if let Some(sig) = signalled() {
                                stopped_by = Some(sig);
                                ev.info(format!("signal {sig}: stopping the game"));
                                if let Some(st) = stop_group(pid, STOP_GRACE, true, ev) {
                                    break st;
                                }
                                continue;
                            }
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    };
                    code = code_of(st);
                    ev.info(format!("game exited with {code}"));
                    // its children (cs2.sh runs the game binary) may still hold files open
                    if group_alive(pid as i32) {
                        stop_group(pid, Duration::from_secs(5), true, ev);
                    }
                    s.exit_code = Some(code);
                    s.phase = Phase::Exited;
                    s.save(&sf)?;
                }
                Err(e) => {
                    s.phase = Phase::Failed;
                    s.error = Some(format!("spawn: {e}"));
                    code = 127;
                    ev.warn(format!("starting {}: {e}", ctx.install.root.join("game/cs2.sh").display()));
                }
            }
        } else {
            ev.info("stopped before the game started");
            code = 128 + stopped_by.unwrap_or(0);
        }
        crate::session::finish_run(&mut s, ctx, &sf)?;
        let restore = s.restore.clone();
        let found = crate::session::restore_findings(restore.as_ref());
        let clean = ctx.dirs.store().load()?.is_none();
        // the volume must be the user's again: a session that could not restore says so in its code
        if !clean || s.error.as_deref().is_some_and(|e| e.starts_with("restore")) {
            ev.warn("the volume was NOT restored: the next `run` (or `cs2mcp-native uninstall` with the same --state-dir) restores it");
            code = 3;
        }
        let ts = crate::manifest::now_secs();
        std::fs::create_dir_all(ctx.dirs.logs())?;
        let _ = std::fs::rename(&sf, ctx.dirs.logs().join(format!("session-{ts}.json")));
        let v = json!({
            "prepared": prepared, "cleanup": cleaned, "exitCode": code, "signal": stopped_by,
            "restore": restore, "treeClean": clean, "findings": found,
        });
        Ok((v, code))
    }
}

#[cfg(unix)]
pub use imp::{run, stop_group};

/// Windows has no container mode.
#[cfg(not(unix))]
pub fn run(_ctx: &Ctx, _o: &RunOpts) -> anyhow::Result<(serde_json::Value, i32)> {
    anyhow::bail!("`run` is the container mode (Linux); on Windows use `cs2mcp-native up`")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::events::Events;

    /// TERM reaches a whole process group, and a group that ignores it is killed after the grace.
    #[test]
    fn stop_group_terms_then_kills() {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let ev = Events::null();
        let mut c = std::process::Command::new("sh").args(["-c", "sleep 30 & wait"]).process_group(0).spawn().unwrap();
        let t = std::time::Instant::now();
        stop_group(c.id(), Duration::from_secs(5), false, &ev);
        assert!(t.elapsed() < Duration::from_secs(4), "TERM was enough");
        let _ = c.wait();
        let mut c = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; echo ready; sleep 30 & wait; sleep 30"])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(c.stdout.take().unwrap()).read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "ready");
        let t = std::time::Instant::now();
        stop_group(c.id(), Duration::from_millis(500), false, &ev);
        assert!(t.elapsed() >= Duration::from_millis(500));
        // SAFETY: signal 0 to a process group only checks existence.
        assert!(unsafe { libc::kill(-(c.id() as i32), 0) } != 0, "gone");
        let _ = c.wait();
    }
}
