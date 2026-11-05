//! `cargo bridge build`: build the Rust bridge and the Metamod loader for both targets, stage
//! the artefacts and gate their exports.
//!
//! ```text
//! target/bridge-rs/win64/cs2_bridge.dll               <- cs2.exe -tools (Wine) loads this
//! target/bridge-rs/win64/cs2mcp_launch.exe            <- the tools runtime starts this instead of
//! target/bridge-rs/win64/cs2mcp_mmload.dll               cs2.exe; it injects the loader DLL
//! target/bridge-rs/linuxsteamrt64/libcs2_bridge.so
//! target/bridge-rs/linuxsteamrt64/libcs2mcp_mmload.so <- LD_PRELOADed on the cs2.sh line
//! ```
//!
//! The paths are a contract: `cargo bridge install` and `crates/cs2-native` read them.
//!
//! Order, and why:
//!
//! 1. **The pins gate** (`cargo bridge pins --check`, run as this binary's own subcommand): the
//!    crate embeds `crates/cs2-bridge/src/generated/pins.rs`, so a stale or hand-edited one fails
//!    the build here, before any cargo work.
//! 2. **Four release builds**, `cs2-bridge` and `mm-launch` for `x86_64-pc-windows-msvc` and
//!    `x86_64-unknown-linux-gnu`, each in the memory-bounded form the repo rules require
//!    (AGENTS.md: no `--workspace`, a `CARGO_TARGET_DIR` inside the checkout, a virtual-memory
//!    cap and a deadline; see [`release_build`]). The Linux build of `mm-launch` produces the
//!    interposer `.so` plus the launcher's "windows only" stub bin, which is not staged: the
//!    launcher is a Windows-only binary.
//! 3. **Stage** the five files.
//! 4. **Gate** them natively ([`crate::exports`]): the bridge exports exactly `CreateInterface`,
//!    the loader exactly `DllMain` / `dlopen`, and neither DLL imports a CRT. Every gate runs,
//!    and any refusal fails the build.
//!
//! The Windows half needs the xwin headers/libs and `lld-link`; both are configured in
//! `.cargo/config.toml`, which is why the builds run from the checkout with the environment's
//! rustflags cleared.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::exports::{self, Gate};
use crate::root;

/// The command line of `cargo bridge build`.
#[derive(clap::Args, Debug)]
pub struct Args {}

/// The two targets every artefact is built for.
pub const TARGETS: [&str; 2] = ["x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu"];

/// The address-space cap of one cargo build, in KiB (`ulimit -v 6000000`): rustc's peak for the
/// bridge stays well under it, and a runaway link fails instead of pushing the shared host into
/// swap.
pub const ADDRESS_SPACE_KIB: u64 = 6_000_000;

/// The deadline of one cargo build (`timeout 900`). A cold release build of the bridge takes a
/// few minutes at `-j2`; past this something is stuck.
pub const DEADLINE: Duration = Duration::from_secs(900);

/// How long a timed-out or interrupted build gets to exit after the signal before it is killed.
const GRACE: Duration = Duration::from_secs(30);

/// The cargo jobs of one build (`-j2`: the host is shared).
const JOBS: &str = "2";

/// One staged artefact: what cargo writes (relative to `<CARGO_TARGET_DIR>/<target>/release`)
/// and where it is staged (relative to `target/bridge-rs`).
struct Artefact {
    target: &'static str,
    built: &'static str,
    staged: &'static str,
    gate: Option<Gate>,
}

const ARTEFACTS: [Artefact; 5] = [
    Artefact { target: TARGETS[0], built: "cs2_bridge.dll", staged: "win64/cs2_bridge.dll", gate: Some(Gate::Bridge) },
    Artefact { target: TARGETS[0], built: "cs2mcp_launch.exe", staged: "win64/cs2mcp_launch.exe", gate: None },
    Artefact { target: TARGETS[0], built: "cs2mcp_mmload.dll", staged: "win64/cs2mcp_mmload.dll", gate: Some(Gate::Loader) },
    Artefact { target: TARGETS[1], built: "libcs2_bridge.so", staged: "linuxsteamrt64/libcs2_bridge.so", gate: Some(Gate::Bridge) },
    Artefact {
        target: TARGETS[1],
        built: "libcs2mcp_mmload.so",
        staged: "linuxsteamrt64/libcs2mcp_mmload.so",
        gate: Some(Gate::Loader),
    },
];

/// Runs the command; `Ok` is the exit code.
pub fn run(_args: Args) -> Result<u8> {
    let checkout = root::checkout();

    println!("build: cargo bridge pins --check");
    let exe = std::env::current_exe().context("locating this binary for `pins --check`")?;
    let status = Command::new(exe)
        .args(["pins", "--check"])
        .current_dir(&checkout)
        .status()
        .context("running `cargo bridge pins --check`")?;
    if !status.success() {
        eprintln!("build: pins --check failed ({status}): regenerate the pins (`cargo bridge pins`) or fix the signatures first");
        return Ok(exit_code(status));
    }

    for target in TARGETS {
        for package in ["cs2-bridge", "mm-launch"] {
            release_build(&checkout, "build", package, target)?;
        }
    }

    let release = |target: &str| cargo_target_dir(&checkout).join(target).join("release");
    for a in &ARTEFACTS {
        let built = release(a.target).join(a.built);
        if !built.is_file() {
            bail!("{} was not built", built.display());
        }
    }
    let out = checkout.join("target/bridge-rs");
    let staged: Vec<PathBuf> = ARTEFACTS
        .iter()
        .map(|a| stage(&release(a.target).join(a.built), &out.join(a.staged)))
        .collect::<Result<_>>()?;
    println!(
        "build: staged {0}/win64/{{cs2_bridge.dll,cs2mcp_launch.exe,cs2mcp_mmload.dll}} and \
         {0}/linuxsteamrt64/{{libcs2_bridge.so,libcs2mcp_mmload.so}}",
        out.display()
    );

    let mut refused = 0;
    for (a, path) in ARTEFACTS.iter().zip(&staged) {
        if let Some(gate) = a.gate {
            if exports::gate_file(path, gate) != 0 {
                refused += 1;
            }
        }
    }
    if refused > 0 {
        eprintln!("build: {refused} artefact(s) failed the export gate");
        return Ok(1);
    }
    Ok(0)
}

/// `<checkout>/target/cargo`: the release builds' own target dir, inside the checkout (the
/// rules) and apart from the workspace's default `target/` so a debug `cargo test` does not
/// invalidate them.
pub fn cargo_target_dir(checkout: &Path) -> PathBuf {
    checkout.join("target/cargo")
}

/// The `cargo build --release -p <package> --target <target> -j2` command, before its limits.
///
/// * The cwd is the checkout: `.cargo/config.toml`'s target flags (linker, xwin lib paths,
///   `+crt-static`) are picked up relative to the current directory.
/// * `RUSTFLAGS` / `CARGO_ENCODED_RUSTFLAGS` are removed: either would *replace* those config
///   flags, and the build would then fail for a missing linker or link a DLL that depends on
///   `vcruntime140.dll`. The config is the only source of those flags.
/// * The cargo is the one running this tool (`$CARGO`, set by `cargo run`), else `cargo` on
///   `PATH`.
pub fn cargo_command(checkout: &Path, package: &str, target: &str) -> Command {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut cmd = Command::new(cargo);
    cmd.args(["build", "--release", "-p", package, "--target", target, "-j", JOBS])
        .current_dir(checkout)
        .env("CARGO_TARGET_DIR", cargo_target_dir(checkout))
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        // The build runs in its own process group (see `run_bounded`), where reading the
        // terminal would stop it; cargo never needs stdin.
        .stdin(Stdio::null());
    cmd
}

/// One bounded release build of `package` for `target` ([`cargo_command`]), logged as
/// `<tag>: cargo build ...`; `Err` unless it exits 0 within [`DEADLINE`].
pub fn release_build(checkout: &Path, tag: &str, package: &str, target: &str) -> Result<()> {
    println!("{tag}: cargo build --release -p {package} --target {target}");
    let mut cmd = cargo_command(checkout, package, target);
    let status = run_bounded(&mut cmd, DEADLINE).with_context(|| format!("cargo build -p {package} --target {target}"))?;
    if !status.success() {
        bail!("cargo build --release -p {package} --target {target} failed ({status})");
    }
    Ok(())
}

/// The signal that interrupted a bounded run (0: none). Set by [`forward_signal`].
static INTERRUPTED: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward_signal(signal: libc::c_int) {
    INTERRUPTED.store(signal, Ordering::SeqCst);
}

/// Runs `cmd` the way `( ulimit -v 6000000; timeout 900 cmd )` did.
///
/// * The address space is capped at [`ADDRESS_SPACE_KIB`] (soft and hard, as `ulimit -v` sets
///   both), inherited by every rustc the build starts.
/// * The command runs in its own process group so the deadline can stop the whole build (cargo
///   and its rustc/linker children), as `timeout` does: past `deadline` the group gets
///   `SIGTERM`, then `SIGKILL` after a grace period, and the run is an error.
/// * Because the group is not the terminal's foreground group any more, a `SIGINT`/`SIGTERM`/
///   `SIGHUP` this process receives meanwhile is forwarded to it (again as `timeout` does), so
///   Ctrl-C still stops the build instead of orphaning it.
pub fn run_bounded(cmd: &mut Command, deadline: Duration) -> Result<ExitStatus> {
    let cap = ADDRESS_SPACE_KIB * 1024;
    // SAFETY: the closure runs in the forked child before exec and only calls setrlimit, which
    // is async-signal-safe; it touches no memory shared with the parent.
    unsafe {
        cmd.pre_exec(move || {
            let limit = libc::rlimit { rlim_cur: cap, rlim_max: cap };
            if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.process_group(0);
    INTERRUPTED.store(0, Ordering::SeqCst);
    let signals = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];
    // SAFETY: the handler only stores into an atomic, which is async-signal-safe.
    let previous = signals.map(|s| unsafe { libc::signal(s, forward_signal as extern "C" fn(libc::c_int) as libc::sighandler_t) });
    let result = wait_bounded(cmd, deadline);
    for (signal, handler) in signals.into_iter().zip(previous) {
        // SAFETY: puts back the disposition `signal` had before this run.
        unsafe { libc::signal(signal, handler) };
    }
    result
}

/// Why a bounded run was stopped.
#[derive(Clone, Copy)]
enum Stop {
    Deadline,
    Signal(libc::c_int),
}

/// The wait half of [`run_bounded`]: spawn, poll, stop the group when needed.
fn wait_bounded(cmd: &mut Command, deadline: Duration) -> Result<ExitStatus> {
    let mut child = cmd.spawn().context("spawning")?;
    let group = child.id() as libc::pid_t;
    let started = Instant::now();
    let mut stopped: Option<(Stop, Instant)> = None;
    let mut killed = false;
    loop {
        if let Some(status) = child.try_wait()? {
            return match stopped {
                None => Ok(status),
                Some((Stop::Deadline, _)) => bail!("did not finish within {} s (stopped: {status})", deadline.as_secs()),
                Some((Stop::Signal(signal), _)) => bail!("interrupted by signal {signal} ({status})"),
            };
        }
        match stopped {
            None => {
                let interrupted = INTERRUPTED.load(Ordering::SeqCst);
                if interrupted != 0 {
                    signal_group(group, interrupted);
                    stopped = Some((Stop::Signal(interrupted), Instant::now()));
                } else if started.elapsed() >= deadline {
                    eprintln!("deadline of {} s reached, stopping the build", deadline.as_secs());
                    signal_group(group, libc::SIGTERM);
                    stopped = Some((Stop::Deadline, Instant::now()));
                }
            }
            Some((_, at)) if !killed && at.elapsed() >= GRACE => {
                signal_group(group, libc::SIGKILL);
                killed = true;
            }
            Some(_) => {}
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Sends `signal` to the process group `group`.
fn signal_group(group: libc::pid_t, signal: libc::c_int) {
    // SAFETY: kill(2) with a negative pid signals that process group; no memory is involved.
    unsafe { libc::kill(-group, signal) };
}

/// Copies `from` to `to` (creating `to`'s directory) and returns `to`.
pub fn stage(from: &Path, to: &Path) -> Result<PathBuf> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::copy(from, to).with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    Ok(to.to_path_buf())
}

/// A child's exit status as this process's exit code (`1` for a signal or an out-of-range
/// code).
pub fn exit_code(status: ExitStatus) -> u8 {
    status.code().and_then(|c| u8::try_from(c).ok()).filter(|c| *c != 0).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cargo_command_is_the_memory_bounded_form() {
        let checkout = Path::new("/checkout");
        let cmd = cargo_command(checkout, "cs2-bridge", TARGETS[0]);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args, ["build", "--release", "-p", "cs2-bridge", "--target", "x86_64-pc-windows-msvc", "-j", "2"]);
        assert_eq!(cmd.get_current_dir(), Some(checkout));
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&("CARGO_TARGET_DIR".as_ref(), Some("/checkout/target/cargo".as_ref()))));
        assert!(envs.contains(&("RUSTFLAGS".as_ref(), None)));
        assert!(envs.contains(&("CARGO_ENCODED_RUSTFLAGS".as_ref(), None)));
    }

    #[test]
    fn a_bounded_run_carries_the_address_space_cap() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "test \"$(ulimit -v)\" = 6000000 && test \"$(ulimit -H -v)\" = 6000000"]);
        let status = run_bounded(&mut cmd, Duration::from_secs(60)).unwrap();
        assert!(status.success(), "{status}");
    }

    #[test]
    fn a_bounded_run_is_stopped_at_its_deadline() {
        let mut cmd = Command::new("sh");
        // The child and a grandchild: the whole group must go.
        cmd.args(["-c", "sleep 600 & wait"]);
        let started = Instant::now();
        let err = run_bounded(&mut cmd, Duration::from_millis(300)).unwrap_err();
        assert!(err.to_string().contains("did not finish within"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn every_staged_artefact_is_gated_except_the_launcher() {
        let ungated: Vec<_> = ARTEFACTS.iter().filter(|a| a.gate.is_none()).map(|a| a.staged).collect();
        assert_eq!(ungated, ["win64/cs2mcp_launch.exe"]);
    }
}
