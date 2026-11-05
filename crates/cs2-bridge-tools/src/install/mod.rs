//! `cargo bridge install [--platform wine|linux] [--dry-run] <install|uninstall|status|ensure>`:
//! lay the bridge, Metamod:Source 2.0 and the launcher down in a game tree, check it, repair it,
//! take it out again. Gameinfo-free on both platforms: `gameinfo.gi` is never edited. Metamod's
//! loader finds the real game DLL itself by parsing the unmodified gameinfo
//! (`loader/gamedll.cpp` `mm_DetectGameInformation`), so no search path is added.
//!
//! # The trees
//!
//! **wine** (default): the shared Wine tools install, `<project root>/workshop/run/app`.
//! * `target/bridge-rs/win64/cs2_bridge.dll` -> `game/bin/win64/tools/cs2_bridge.dll`, refused
//!   unless `cargo bridge check-exports` finds exactly the `CreateInterface` export.
//! * the `m_Name = "cs2_bridge"` entry in `game/bin/sdkenginetools.txt`, inserted before the
//!   hammer entry: the one game-file edit.
//! * the launcher, not export-gated (an .exe and an IAT-hook DLL, neither has the tool-DLL
//!   contract): `target/bridge-rs/win64/{cs2mcp_launch.exe, cs2mcp_mmload.dll}` ->
//!   `game/bin/win64/`. The launcher starts cs2.exe suspended and injects the loader.
//! * Metamod in the standard layout, from `<cs2-mcp>/target/metamod/win64/addons/metamod`:
//!   `game/csgo/addons/metamod/{bin/win64/server.dll, bin/win64/metamod.2.cs2.dll,
//!   metaplugins.ini, cs2mcp.vdf}`. The vdf names the tools copy of the DLL (one image, see
//!   [`text::WINE_VDF`]).
//!
//! **linux**: the `cs2-home` volume's `.../Counter-Strike Global Offensive/game`, every file
//! operation inside `podman unshare` (the volume's files belong to a subuid): this binary runs
//! itself there with a hidden `tree-op` command ([`tree`]); paths travel as JSON arguments.
//! * `target/bridge-rs/linuxsteamrt64/{libcs2_bridge.so (export-gated), libcs2mcp_mmload.so}`
//!   -> `csgo/addons/cs2mcp/bin/linuxsteamrt64/`
//! * the whole Linux Metamod build (`<cs2-mcp>/target/metamod/linux/addons/metamod`) ->
//!   `csgo/addons/metamod/`, plus `csgo/addons/metamod/cs2mcp.vdf` naming the addons copy.
//! * Linux client launches need `-insecure` (Metamod refuses without `-dedicated`/`-insecure`);
//!   that belongs to the launch lane, not here.
//!
//! # Contracts
//!
//! * Every file that exists before install is backed up first (see [`records`] for the format,
//!   which stays compatible with the shell tool's), and every tree write is atomic (a same-dir
//!   temp, then a rename). `metaplugins.ini` is Metamod's config: created only when absent, never
//!   overwritten, deleted by uninstall only while byte-identical to what we created. A foreign
//!   file at one of OUR dest paths is overwritten (accepted limitation); uninstall gives the
//!   owner's bytes back.
//! * uninstall restores what install overwrote (the chain rule, [`records::Records::chain_src_for`]),
//!   deletes exactly what it created (never a pre-first-install file), reverts the manifest entry
//!   in the CURRENT file last, then retires the backups. A file changed after our last write is
//!   left alone with a message.
//! * ensure: a read-only check first (no write access, no guard needed), one line per artefact
//!   (ok / missing / stale / fatal). All ok: exit 0, nothing touched. Otherwise refuse with exit 4
//!   while a running container mounts the tree, else install (which backs up first).
//!   "Current": a copied artefact is byte-equal to its build when the build exists (no build + an
//!   installed copy = current; no build + nothing installed = exit 1 "BUILD IT FIRST"), the
//!   manifest is what the edit would produce from it AND its anchor is still there ("game file
//!   damaged" otherwise), the vdf and every Metamod file match.
//! * install/uninstall/ensure-repair refuse while a running container mounts the tree
//!   ([`guard`]). One writer at a time: `<project root>/.podman/install-rs-<platform>.lock`
//!   (exclusive; status and --dry-run take it shared), waited for 90 s (below the launch paths'
//!   120 s), then exit 4. The file name is the shell tool's, so the two exclude each other.
//!
//! Exit codes: 0 ok/repaired, 1 failure, 2 usage, 3 `--dry-run ensure` found something to repair,
//! 4 refused (busy tree or held lock), 99 a test fault point fired.
//!
//! # Environment (names kept from the shell tool)
//!
//! `CS2_GAME` (wine game dir), `CS2_LINUX_GAME` (linux game dir; when set the volume is not
//! inspected), `CS2_MCP_DIR` (backups + default Metamod builds; default the main checkout),
//! `CS2_MCP_PROJECT_ROOT` / deprecated `CS2DOCKER_ROOT` (its `workshop/run/app` is the REAL Wine
//! tree, its `.podman/` the lock and podman's default XDG dir), `CS2_BRIDGE_DLL`,
//! `CS2_BRIDGE_LINUX_SO`, `CS2_MM_EXE`, `CS2_MM_DLL`, `CS2_MMLOAD_SO` (the artefacts; default
//! this checkout's `target/bridge-rs/...`), `CS2_MM` / `CS2_MM_LINUX` (the Metamod builds),
//! `PODMAN`, `PODMAN_XDG`, `GUARD_FAKE=1` (run the guard on a test tree). Test-only:
//! `INSTALL_RS_LOCK_WAIT` (seconds), `INSTALL_RS_REAL_APP` (stands in for the real Wine app
//! dir), `INSTALL_RS_FAULT=<point>` (exit 99 at `pre-mv:<rel>` - a tree file's temp written, the
//! rename not done -, `mid-backup`, `pre-mv:uninstall:<rel>`, `pre-mv:uninstall-restore:<rel>`).
//!
//! Dropped with the shell tool: the gameinfo-era migration (detecting and removing the old
//! `game/metamod`, `content/metamod`, loader vdfs and gameinfo lines). A tree whose backups still
//! record un-migrated gameinfo edits is refused ([`records::gameinfo_era_refusal`]).

mod fsx;
mod guard;
mod linux;
mod records;
mod text;
mod tree;
mod wine;

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fsx::{die, die4, say, Stop, R};

/// The Linux game lives in this podman volume.
pub(crate) const LINUX_VOLUME: &str = "cs2-home";

/// The command line of `cargo bridge install`.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// Which tree: the shared Wine tools install, or the cs2-home volume's Linux game
    #[arg(long, value_enum, default_value_t = Platform::Wine)]
    pub platform: Platform,
    /// Report what would change and touch nothing (`ensure`: exit 3 when something is stale)
    #[arg(long)]
    pub dry_run: bool,
    /// What to do
    #[arg(value_enum)]
    pub command: Cmd,
    /// The tree op's JSON request (`tree-op` only; internal)
    #[arg(long, hide = true)]
    pub request: Option<String>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Wine,
    Linux,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    /// Back up, then lay the bridge + Metamod + launcher down (idempotent)
    Install,
    /// Restore what install overwrote, remove what it created, revert the manifest entry
    Uninstall,
    /// What is installed (sha256, which build it matches)
    Status,
    /// Check every artefact read-only; repair (install) only when something is missing/stale
    Ensure,
    /// Internal: one file operation inside `podman unshare` (the Linux tree)
    #[value(hide = true)]
    TreeOp,
}

/// Runs the command; `Ok` is the exit code.
pub fn run(args: Args) -> anyhow::Result<u8> {
    fsx::install_signal_cleanup();
    let result = match args.command {
        Cmd::TreeOp => tree::serve(args.request.as_deref()),
        _ => run_command(&args),
    };
    fsx::cleanup();
    Ok(match result {
        Ok(code) => code,
        Err(Stop(code)) => code,
    })
}

/// Everything a command reads from the environment, resolved once.
pub(crate) struct Ctx {
    pub platform: Platform,
    pub dry: bool,
    /// the checkout this binary was built from (default build artefacts)
    pub checkout: PathBuf,
    /// `CS2_MCP_DIR`: backups + the shared Metamod builds
    pub mcp_dir: PathBuf,
    pub project: PathBuf,
    pub real_game: PathBuf,
    pub podman: OsString,
    pub xdg: PathBuf,
    pub backup_root: PathBuf,
    /// `GUARD_FAKE=1`
    pub guard_fake: bool,
    /// `GUARD_FAKE` set at all (only changes a message)
    pub guard_fake_set: bool,
    pub fault: Option<String>,
    // wine
    pub game: PathBuf,
    pub app: PathBuf,
    pub man: PathBuf,
    pub rs_lib: PathBuf,
    pub launch_exe: PathBuf,
    pub load_dll: PathBuf,
    pub mm_build: PathBuf,
    // linux
    pub linux_game_override: Option<PathBuf>,
    pub so_lib: PathBuf,
    pub mmload_so: PathBuf,
    pub mm_build_linux: PathBuf,
    pub bk_prefix: &'static str,
}

/// A non-empty env var (the shell's `${VAR:-default}`).
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

impl Ctx {
    fn from_env(platform: Platform, dry: bool) -> Ctx {
        let checkout = crate::root::checkout();
        let mcp_dir =
            env_path("CS2_MCP_DIR").unwrap_or_else(|| crate::root::main_checkout(&checkout));
        let project = crate::root::project_root();
        let real_app =
            env_path("INSTALL_RS_REAL_APP").unwrap_or_else(|| project.join("workshop/run/app"));
        let real_game = real_app.join("game");
        let game = env_path("CS2_GAME").unwrap_or_else(|| real_game.clone());
        let app = fsx::dirname(&game);
        let bridge_rs = checkout.join("target/bridge-rs");
        let guard = std::env::var("GUARD_FAKE").unwrap_or_default();
        Ctx {
            platform,
            dry,
            mcp_dir: mcp_dir.clone(),
            podman: std::env::var_os("PODMAN")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "podman".into()),
            xdg: env_path("PODMAN_XDG").unwrap_or_else(|| crate::root::podman_xdg(&project)),
            backup_root: mcp_dir.join("target/bridge"),
            guard_fake: guard == "1",
            guard_fake_set: !guard.is_empty(),
            fault: std::env::var("INSTALL_RS_FAULT")
                .ok()
                .filter(|v| !v.is_empty()),
            man: game.join("bin/sdkenginetools.txt"),
            game,
            app,
            real_game,
            project,
            rs_lib: env_path("CS2_BRIDGE_DLL")
                .unwrap_or_else(|| bridge_rs.join("win64/cs2_bridge.dll")),
            launch_exe: env_path("CS2_MM_EXE")
                .unwrap_or_else(|| bridge_rs.join("win64/cs2mcp_launch.exe")),
            load_dll: env_path("CS2_MM_DLL")
                .unwrap_or_else(|| bridge_rs.join("win64/cs2mcp_mmload.dll")),
            mm_build: env_path("CS2_MM")
                .unwrap_or_else(|| mcp_dir.join("target/metamod/win64/addons")),
            linux_game_override: env_path("CS2_LINUX_GAME"),
            so_lib: env_path("CS2_BRIDGE_LINUX_SO")
                .unwrap_or_else(|| bridge_rs.join("linuxsteamrt64/libcs2_bridge.so")),
            mmload_so: env_path("CS2_MMLOAD_SO")
                .unwrap_or_else(|| bridge_rs.join("linuxsteamrt64/libcs2mcp_mmload.so")),
            mm_build_linux: env_path("CS2_MM_LINUX")
                .unwrap_or_else(|| mcp_dir.join("target/metamod/linux/addons")),
            bk_prefix: match platform {
                Platform::Wine => "backup-rs",
                Platform::Linux => "backup-linux",
            },
            checkout,
        }
    }

    fn fault_cfg(&self) -> Option<&str> {
        self.fault.as_deref()
    }

    /// A new backup dir name (nanosecond stamp: two cycles in one second must not collide).
    fn new_backup_dir(&self) -> PathBuf {
        self.backup_root
            .join(format!("{}-{}", self.bk_prefix, fsx::utc_stamp()))
    }

    /// The bridge build this run installs, and where the backups and Metamod builds come from:
    /// the artefacts default to THIS checkout's build, the rest to `CS2_MCP_DIR`.
    fn say_build_checkout(&self, lib: &Path) {
        say(format_args!(
            "bridge build: {} (this checkout: {}; backups + metamod builds: {})",
            lib.display(),
            self.checkout.display(),
            self.mcp_dir.display()
        ));
    }

    /// The export gate on a bridge artefact: `cargo bridge check-exports <file>` (this binary).
    fn export_gate(&self, file: &Path, what: &str) -> R<()> {
        say(format_args!("export gate on {}:", file.display()));
        let exe = std::env::current_exe().map_err(|e| {
            die(format_args!(
                "cannot locate this binary for the export gate: {e}"
            ))
        })?;
        let status = std::process::Command::new(exe)
            .arg("check-exports")
            .arg(file)
            .stdin(std::process::Stdio::null())
            .status()
            .map_err(|e| {
                die(format_args!(
                    "cannot run the export gate on {}: {e}",
                    file.display()
                ))
            })?;
        if !status.success() {
            return Err(die(format_args!(
                "check-exports refused {} (the engine calls a tool {what}'s exports; refusing)",
                file.display()
            )));
        }
        Ok(())
    }
}

/// The lock wait in seconds (`INSTALL_RS_LOCK_WAIT`, default 90); a non-number is a usage error.
fn lock_wait() -> R<u64> {
    match std::env::var("INSTALL_RS_LOCK_WAIT") {
        Err(_) => Ok(90),
        Ok(v) if v.is_empty() => Ok(90),
        Ok(v) => v
            .parse::<u64>()
            .ok()
            .filter(|_| v.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| {
                say(format_args!(
                    "FAIL: INSTALL_RS_LOCK_WAIT must be an integer number of seconds (got '{v}')"
                ));
                Stop(2)
            }),
    }
}

/// Take the single-writer lock (shared for status and --dry-run), polling until the deadline.
fn take_lock(ctx: &Ctx, cmd: Cmd, wait: u64) -> R<File> {
    let root = ctx.project.join(".podman");
    let file = root.join(format!(
        "install-rs-{}.lock",
        match ctx.platform {
            Platform::Wine => "wine",
            Platform::Linux => "linux",
        }
    ));
    std::fs::create_dir_all(&root).map_err(|e| {
        die(format_args!(
            "cannot create {} (needed for the install lock): {e}",
            root.display()
        ))
    })?;
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .map_err(|e| {
            die(format_args!(
                "cannot open the install lock {}: {e}",
                file.display()
            ))
        })?;
    let shared = cmd == Cmd::Status || ctx.dry;
    let deadline = Instant::now() + Duration::from_secs(wait);
    loop {
        let got = if shared {
            f.try_lock_shared()
        } else {
            f.try_lock()
        };
        match got {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                let why = match (cmd, ctx.dry) {
                    (Cmd::Status, _) => "; status needs it shared",
                    (_, true) => "; dry-run needs it shared",
                    _ => "",
                };
                return Err(die4(format_args!(
                    "another `cargo bridge install` holds {} (waited {wait}s{why})",
                    file.display()
                )));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(die(format_args!("cannot lock {}: {e}", file.display())));
            }
        }
    }
    say(format_args!("lock: {} ({wait}s wait)", file.display()));
    Ok(f)
}

fn run_command(args: &Args) -> R<u8> {
    if args.request.is_some() {
        say("FAIL: --request belongs to the internal tree-op command");
        return Err(Stop(2));
    }
    let ctx = Ctx::from_env(args.platform, args.dry_run);
    let wait = lock_wait()?;
    let _lock = take_lock(&ctx, args.command, wait)?;
    match (args.platform, args.command) {
        (Platform::Wine, Cmd::Install) => wine::install(&ctx),
        (Platform::Wine, Cmd::Uninstall) => wine::uninstall(&ctx),
        (Platform::Wine, Cmd::Status) => wine::status(&ctx),
        (Platform::Wine, Cmd::Ensure) => wine::ensure(&ctx),
        (Platform::Linux, Cmd::Install) => linux::install(&ctx),
        (Platform::Linux, Cmd::Uninstall) => linux::uninstall(&ctx),
        (Platform::Linux, Cmd::Status) => linux::status(&ctx),
        (Platform::Linux, Cmd::Ensure) => linux::ensure(&ctx),
        (_, Cmd::TreeOp) => unreachable!("dispatched in run"),
    }
}

/// The ensure verdict shared by both platforms: a fatal first (a fatal with no other issue is
/// not "all current"), then all-current (exit 0, nothing touched), --dry-run (exit 3), the busy
/// refusal (exit 4), else `repair` (install, which backs up first).
fn ensure_repair(
    ctx: &Ctx,
    issues: bool,
    fatal: Option<String>,
    repair: impl FnOnce() -> R<u8>,
) -> R<u8> {
    if let Some(msg) = fatal {
        say(format_args!("ensure: cannot repair - {msg}"));
        return Err(Stop(1));
    }
    if !issues {
        say("ensure: all artefacts current - nothing to do (exit 0, nothing touched)");
        return Ok(0);
    }
    if ctx.dry {
        say("ensure: dry-run - the artefacts above are NOT repaired (--dry-run touches nothing)");
        return Err(Stop(3));
    }
    if guard::guard_needed(ctx) {
        let busy = guard::busy_probe(ctx).map_err(|rc| {
            die(format_args!(
                "podman failed while probing running containers (rc {rc}) - cannot check whether the tree is safe to repair (never guessing that nothing is running)"
            ))
        })?;
        if !busy.is_empty() {
            say("these running containers mount the tree:");
            for n in &busy {
                say(format_args!("  - {n}"));
            }
            say("and the artefacts listed above are missing/stale - refusing to repair while they mount the tree");
            return Err(Stop(4));
        }
    }
    say("ensure: installing to repair (backup first, as install always does)");
    repair()
}

/// One ensure line: `  <state>  <what>`.
fn ensure_report(state: &str, what: impl std::fmt::Display) {
    say(format_args!("  {state}  {what}"));
}

/// Retire every backup of this tree after an uninstall: a later install/uninstall cycle must
/// restore ITS pre-install image, not this one (a game update may have changed the files).
fn retire_backups(ctx: &Ctx, bks: &[PathBuf]) {
    let retired = ctx.backup_root.join("retired");
    let _ = std::fs::create_dir_all(&retired);
    for b in bks {
        let name = b
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if std::fs::rename(b, retired.join(&name)).is_ok() {
            say(format_args!("  retired backup: {name}"));
        } else {
            say(format_args!(
                "  WARN: could not retire backup {name} (rename failed) - it stays in place; uninstall of the NEXT cycle restores from the oldest backup that matches this tree"
            ));
        }
    }
}

/// `matches build <a>[ and <b>...]` / `matches no build here`: which build dir's bytes an
/// installed lib is (status), so a tree installed from another checkout is visible.
fn lib_build_match(installed_sha: &str, candidates: &[PathBuf]) -> String {
    let mut hits: Vec<&PathBuf> = Vec::new();
    for c in candidates {
        if hits.contains(&c) || !c.is_file() {
            continue;
        }
        if fsx::sha_file(c).ok().as_deref() == Some(installed_sha) {
            hits.push(c);
        }
    }
    if hits.is_empty() {
        return "matches no build here".into();
    }
    let names: Vec<String> = hits.iter().map(|h| h.display().to_string()).collect();
    format!("matches build {}", names.join(" and "))
}
