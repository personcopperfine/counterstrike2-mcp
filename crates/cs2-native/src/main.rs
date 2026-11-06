//! `cs2mcp-native`: see the crate README. Results go to stdout as JSON, progress to stderr.
//!
//! Exit codes: 0 done; 1 an error; 2 stopped on (or finished with) findings that need the user's
//! decision (`findings.rs`; the JSON lists them); `run`: the game's exit code, 3 when the volume
//! could not be restored.

use clap::{Args, Parser, Subcommand};
use cs2_native::args::GameArgs;
use cs2_native::events::Events;
use cs2_native::findings::{self, Finding, Policy, Stopped};
use cs2_native::install::Os;
use cs2_native::mcpconfig::{self, Servers, Transport, Which};
use cs2_native::plan::SteamMode;
use cs2_native::selfexe::NativeCmd;
use cs2_native::session::{self, emit, Ctx, CtxOpts, UpOpts};
use cs2_native::state::{self, Strategy};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "cs2mcp-native", version, about = "Prepare and launch a user's own Steam CS2 install for cs2-mcp (no containers)")]
struct Cli {
    #[command(flatten)]
    g: Global,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Debug)]
struct Global {
    /// The install root (`.../Counter-Strike Global Offensive`, or its `game` dir). Default: Steam's libraries.
    #[arg(long, global = true, env = "CS2MCP_GAME_ROOT")]
    game_root: Option<PathBuf>,
    /// An extra Steam library to search (repeatable), e.g. `C:\SteamLibrary`.
    #[arg(long, global = true)]
    library: Vec<PathBuf>,
    /// State dir. Default: %LOCALAPPDATA%\cs2mcp-native / $XDG_STATE_HOME/cs2mcp-native.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// How files reach the game: overlay (Linux default), in-place (Windows default; `run` always).
    #[arg(long, global = true, value_enum, default_value_t = Strategy::Auto)]
    strategy: Strategy,
    /// `bridge/build-rs.sh` output (win64/, linuxsteamrt64/).
    #[arg(long, global = true, env = "CS2MCP_BRIDGE_DIR")]
    bridge_dir: Option<PathBuf>,
    /// Metamod builds (win64/addons/metamod, linux/addons/metamod).
    #[arg(long, global = true, env = "CS2MCP_METAMOD_DIR")]
    metamod_dir: Option<PathBuf>,
    /// gbe_fork binaries (win64/steam_api64.dll, linux64/{libsteam_api.so,steamclient.so}).
    #[arg(long, global = true, env = "CS2MCP_GBE_DIR")]
    gbe_dir: Option<PathBuf>,
    /// The capture layer (win64/, linux64/: `VkLayer_cs2mcp_capture.json` + the library).
    #[arg(long, global = true, env = "CS2MCP_VK_LAYER_DIR")]
    vk_layer_dir: Option<PathBuf>,
    /// The bridge's discovery file. Default: %LOCALAPPDATA%\cs2mcpbridge\session.json / $XDG_RUNTIME_DIR/cs2mcpbridge/session.json.
    #[arg(long, global = true, env = "CS2_MCP_DISCOVERY")]
    discovery: Option<PathBuf>,
    /// gbe's save dir (`local_save_path`). Default: <state>/gbe-saves.
    #[arg(long, global = true, env = "CS2MCP_GBE_SAVE_DIR")]
    gbe_save_dir: Option<PathBuf>,
    /// Go on past findings that need a decision: keep leftovers of an older install as the tree's
    /// own, accept a restore that left Steam's newer files in place (exit 0 instead of 2).
    #[arg(long, global = true)]
    accept_conflicts: bool,
    /// The manifest's backups are gone: restore everything else and record those files as lost
    /// (our bytes are removed from them; Verify integrity in Steam brings Valve's back).
    #[arg(long, global = true)]
    forget_missing_backups: bool,
}

#[derive(Args, Debug, Clone)]
struct Mode {
    /// gbe = the Steam emulator (offline, no account); client = the user's running Steam client.
    #[arg(long, value_enum)]
    steam: SteamMode,
    /// Workshop Tools session (`-tools`, the bridge's sdkenginetools.txt entry). Windows only.
    #[arg(long)]
    tools: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Find the install and print what the launcher knows about it.
    Discover,
    /// Lay the files down (or stage them into the overlay) without launching.
    Prepare {
        #[command(flatten)]
        mode: Mode,
    },
    /// prepare + launch + wait until the bridge answers.
    Up {
        #[command(flatten)]
        mode: Mode,
        /// `+map <map>` (client sessions start the bridge listener only once a map runs).
        #[arg(long)]
        map: Option<String>,
        /// `-addon <addon>` (tools).
        #[arg(long)]
        addon: Option<String>,
        /// Readiness deadline in seconds (a first boot compiles shaders).
        #[arg(long, default_value_t = 900)]
        timeout: u64,
        /// Leave the tree prepared when the game exits (in-place). Default: restore on exit.
        #[arg(long)]
        keep_installed: bool,
        /// Return after the launch, without waiting for readiness.
        #[arg(long)]
        no_wait: bool,
        /// Do not load the capture layer into the game (screenshot/observe then have no frames).
        #[arg(long)]
        no_capture: bool,
        /// Extra game arguments (after `--`). `-insecure` is always added; `-secure` is refused.
        #[arg(last = true)]
        extra: Vec<String>,
    },
    /// Container mode (Linux): prepare in place, run the game in the foreground, restore and
    /// verify the volume on exit or on SIGTERM/SIGINT; exits with the game's code.
    Run {
        #[command(flatten)]
        mode: Mode,
        #[arg(long)]
        map: Option<String>,
        #[arg(long)]
        addon: Option<String>,
        /// Do not load the capture layer (a container runtime that mounts its own sets it up).
        #[arg(long)]
        no_capture: bool,
        /// First move the old container layout's leftovers out of the volume (install-rs.sh's
        /// files, setup_gbe.sh's emulator in place of Valve's library: put back from its backup).
        #[arg(long)]
        cleanup_legacy: bool,
        /// Extra game arguments (after `--`). `-insecure` is always added; `-secure` is refused.
        #[arg(last = true)]
        extra: Vec<String>,
    },
    /// Wait for the running session's readiness ladder.
    Wait {
        #[arg(long, default_value_t = 900)]
        timeout: u64,
    },
    /// Session, manifest and drift (read-only).
    Status {
        /// Also try a control.hello on the discovery file.
        #[arg(long)]
        hello: bool,
    },
    /// Stop the game, restore the tree, verify it.
    Down {
        /// Seconds to wait for the supervisor's restore.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Restore the tree without a session (after a crash or a reboot).
    Uninstall,
    /// Move leftovers no manifest records (our files after a deleted state dir, an older
    /// install-rs.sh layout) out of the install, into the state dir. Without --yes: only list.
    Cleanup {
        #[arg(long)]
        yes: bool,
    },
    /// Print the MCP client config (`mcpServers`) for this install.
    McpConfig {
        #[arg(long, value_enum, default_value_t = Which::Both)]
        server: Which,
        /// Directory with hammer-mcp / client-mcp. Default: the package dir, else <repo>/target/release.
        #[arg(long)]
        server_dir: Option<PathBuf>,
        /// stdio (the MCP client starts the servers); http is not available yet.
        #[arg(long, value_enum, default_value_t = Transport::Stdio)]
        transport: Transport,
    },
    /// (internal) the session supervisor.
    #[command(hide = true)]
    Supervise { session_file: PathBuf },
    /// Post an `open_asset` into the running `-tools` session (runtime.sh `open`): a map opens in
    /// Hammer, a `.vmdl` loads ModelDoc. Run it on the game's desktop (the channel is per logon session).
    Open {
        /// Content-relative asset path (`maps/x.vmap`, `models/x.vmdl`).
        asset: String,
    },
    /// Download gbe_fork's Linux release (pinned tag + SHA-256, checked) and install its
    /// libsteam_api.so + steamclient.so into <state dir>/gbe/linux64 for `--steam gbe`, when the
    /// payload has none. Needs `curl` and `tar` (bzip2). Downloads only when run.
    FetchGbe {
        /// Directory to fill (`linux64/`, `GBE_REF.txt`). Default: <state dir>/gbe, which `--steam gbe` uses when the payload has no gbe.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Use this copy of the release archive instead of downloading it (still hash-checked).
        #[arg(long)]
        archive: Option<PathBuf>,
    },
}

fn opts(g: &Global, strategy: Option<Strategy>) -> CtxOpts {
    CtxOpts {
        game_root: g.game_root.clone(),
        library: g.library.clone(),
        state_dir: g.state_dir.clone(),
        strategy: strategy.or(Some(g.strategy)),
        bridge_dir: g.bridge_dir.clone(),
        metamod_dir: g.metamod_dir.clone(),
        gbe_dir: g.gbe_dir.clone(),
        vk_layer_dir: g.vk_layer_dir.clone(),
        discovery: g.discovery.clone(),
        gbe_save_dir: g.gbe_save_dir.clone(),
        policy: Policy { accept_conflicts: g.accept_conflicts, forget_missing_backups: g.forget_missing_backups },
    }
}

fn main() {
    let cli = Cli::parse();
    let ev = Events::stderr();
    match run(cli, &ev) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            if let Some(st) = e.downcast_ref::<Stopped>() {
                // the findings are the answer: the JSON a script or the GUI reads, exit 2
                let _ = emit(&json!({"error": st.message, "findings": st.findings}));
                ev.warn(format!("stopped: {} (exit 2; see the findings)", st.message));
                std::process::exit(2);
            }
            ev.warn(format!("error: {e:#}"));
            std::process::exit(1);
        }
    }
}

/// 2 when the result still holds a decision the policy did not accept (a restore with conflicts).
fn finished(v: &Value, policy: Policy) -> i32 {
    let f: Vec<Finding> = serde_json::from_value(v["findings"].clone()).unwrap_or_default();
    findings::exit_code(&f, policy)
}

fn run(cli: Cli, ev: &Events) -> anyhow::Result<i32> {
    match cli.cmd {
        Cmd::Supervise { session_file } => return session::supervise(&session_file, ev).map(|()| 0),
        Cmd::Open { asset } => {
            let seq = cs2_native::vtm::post(&cs2_native::vtm::open_asset(&asset)).map_err(anyhow::Error::msg)?;
            emit(&json!({"posted": seq, "asset": asset}))?;
            return Ok(0);
        }
        Cmd::FetchGbe { out, archive } => return fetch_gbe(&cli.g, out, archive, ev).map(|()| 0),
        _ => {}
    }
    // the container mode is always in place (an explicit overlay is refused by `run`)
    let strategy = matches!(cli.cmd, Cmd::Run { .. }).then(|| if cli.g.strategy == Strategy::Auto { Strategy::InPlace } else { cli.g.strategy });
    let o = opts(&cli.g, strategy);
    let c = Ctx::open(&o, ev.clone())?;
    let mut code = 0;
    let v = match cli.cmd {
        Cmd::Discover => {
            let pre = c.preflight()?;
            for f in &pre {
                f.report(ev);
            }
            json!({
                "root": c.install.root, "library": c.install.library, "build": c.install.build,
                "targetBuild": cs2_native::install::target_build(), "otherInstalls": c.other_installs,
                "tools": c.install.has_tools, "gameBinary": c.install.has_game_binary,
                "os": Os::host(), "strategy": c.strategy, "stateDir": c.dirs.dir, "packageDir": c.package_dir,
                "artifacts": {"bridge": c.artifacts.bridge, "metamod": c.artifacts.metamod, "gbe": c.artifacts.gbe,
                              "vklayer": c.artifacts.vklayer,
                              "captureLayer": cs2_native::capture::layer_dir(&c.artifacts.vklayer, Os::host())},
                "discovery": c.discovery,
                "findings": pre,
            })
        }
        Cmd::Prepare { mode } => session::prepare(&c, mode.steam, mode.tools)?,
        Cmd::Up { mode, map, addon, timeout, keep_installed, no_wait, no_capture, extra } => {
            let o = UpOpts {
                steam: mode.steam,
                tools: mode.tools,
                game: GameArgs { tools: mode.tools, map, addon, extra },
                timeout: Duration::from_secs(timeout),
                keep_installed,
                wait: !no_wait,
                capture: !no_capture,
                launcher: NativeCmd::current()?,
            };
            let mut v = session::up(&c, &o)?;
            v["mcpConfig"] = session::mcp_config(&c, Which::Both, &mcpconfig::default_servers());
            v
        }
        Cmd::Run { mode, map, addon, no_capture, cleanup_legacy, extra } => {
            let o = cs2_native::run::RunOpts {
                steam: mode.steam,
                tools: mode.tools,
                game: GameArgs { tools: mode.tools, map, addon, extra },
                capture: !no_capture,
                cleanup_legacy,
            };
            let (v, game_code) = cs2_native::run::run(&c, &o)?;
            emit(&v)?;
            return Ok(game_code);
        }
        Cmd::Wait { timeout } => session::wait_ready(&c, Duration::from_secs(timeout), &AtomicBool::new(false))?,
        Cmd::Status { hello } => session::status(&c, hello)?,
        Cmd::Down { timeout } => {
            let v = session::down(&c, Duration::from_secs(timeout))?;
            code = finished(&v, o.policy);
            v
        }
        Cmd::Uninstall => {
            let v = session::uninstall(&c)?;
            code = finished(&v, o.policy);
            v
        }
        Cmd::Cleanup { yes } => session::cleanup(&c, yes)?,
        Cmd::McpConfig { server, server_dir, transport } => {
            if !transport.available() {
                anyhow::bail!("--transport http is not available yet: the servers speak stdio only (the MCP client starts them)");
            }
            let servers = server_dir.map(Servers::Dir).unwrap_or_else(mcpconfig::default_servers);
            let v = session::mcp_config(&c, server, &servers);
            for n in mcpconfig::notes(Os::host(), &servers) {
                ev.info(format!("note: {n}"));
            }
            v
        }
        Cmd::Supervise { .. } | Cmd::Open { .. } | Cmd::FetchGbe { .. } => {
            unreachable!()
        }
    };
    emit(&v)?;
    Ok(code)
}

fn fetch_gbe(g: &Global, out: Option<PathBuf>, archive: Option<PathBuf>, ev: &Events) -> anyhow::Result<()> {
    use cs2_native::gbe;
    if Os::host() != Os::Linux {
        anyhow::bail!("fetch-gbe fetches gbe's Linux libraries; on Windows the package carries payload/gbe/win64 (scripts/stage-gbe.sh)");
    }
    let root = state::state_root(g.state_dir.as_deref())?;
    let dest = out.unwrap_or_else(|| gbe::fetched_dir(&root));
    let cache = gbe::cache_dir(&root);
    let o = gbe::FetchOpts { dest: &dest, cache: &cache, archive: archive.as_deref() };
    let v = gbe::fetch_linux(&o, |s| ev.info(s))?;
    if g.gbe_dir.is_some() {
        ev.info("note: --gbe-dir / CS2MCP_GBE_DIR is set and wins over the fetched dir");
    }
    emit(&v)
}
