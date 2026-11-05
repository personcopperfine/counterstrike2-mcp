//! `cs2mcp-native`: see the crate README. Results go to stdout as JSON, progress to stderr.

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use cs2_native::args::GameArgs;
use cs2_native::install::{self, Os};
use cs2_native::mcpconfig::{self, Which};
use cs2_native::plan::{Artifacts, SteamMode};
use cs2_native::session::{self, emit, say, Ctx, UpOpts};
use cs2_native::state::{self, Dirs, Strategy};
use serde_json::{json, Value};
use std::path::PathBuf;
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
    /// How files reach the game: overlay (Linux default), in-place (Windows default).
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
    /// Print the MCP client config (`mcpServers`) for this install.
    McpConfig {
        #[arg(long, value_enum, default_value_t = Which::Both)]
        server: Which,
        /// Directory with hammer-mcp / client-mcp. Default: next to this exe, else <repo>/target/release.
        #[arg(long)]
        server_dir: Option<PathBuf>,
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

fn ctx(g: &Global) -> anyhow::Result<Ctx> {
    let install = install::discover(g.game_root.as_deref(), &g.library)?;
    let root = state::state_root(g.state_dir.as_deref())?;
    let dirs = Dirs::new(&root, &install.root);
    let strategy = g.strategy.resolve(Os::host())?;
    let fetched = cs2_native::gbe::fetched_dir(&root);
    let artifacts = Artifacts::resolve(
        g.bridge_dir.clone(),
        g.metamod_dir.clone(),
        g.gbe_dir.clone(),
        Some(fetched),
        g.vk_layer_dir.clone(),
    );
    let discovery = g
        .discovery
        .clone()
        .or_else(cs2_native::probe::default_discovery)
        .context("no discovery path (set --discovery)")?;
    Ok(Ctx { install, dirs, strategy, artifacts, discovery })
}

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => {}
        Err(e) => {
            say(format!("error: {e:#}"));
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.cmd {
        Cmd::Supervise { session_file } => return session::supervise(&session_file),
        Cmd::Open { asset } => {
            let seq = cs2_native::vtm::post(&cs2_native::vtm::open_asset(&asset)).map_err(anyhow::Error::msg)?;
            return emit(&json!({"posted": seq, "asset": asset}));
        }
        Cmd::FetchGbe { out, archive } => return fetch_gbe(&cli.g, out, archive),
        _ => {}
    }
    let c = ctx(&cli.g)?;
    let v = match cli.cmd {
        Cmd::Discover => json!({
            "root": c.install.root, "library": c.install.library, "build": c.install.build,
            "tools": c.install.has_tools, "gameBinary": c.install.has_game_binary,
            "os": Os::host(), "strategy": c.strategy, "stateDir": c.dirs.dir,
            "artifacts": {"bridge": c.artifacts.bridge, "metamod": c.artifacts.metamod, "gbe": c.artifacts.gbe,
                          "vklayer": c.artifacts.vklayer,
                          "captureLayer": cs2_native::capture::layer_dir(&c.artifacts.vklayer, Os::host())},
            "discovery": c.discovery,
        }),
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
            };
            let mut v = session::up(&c, &o)?;
            v["mcpConfig"] = mcp_config(&c, Which::Both, None);
            v
        }
        Cmd::Wait { timeout } => session::wait_ready(&c, Duration::from_secs(timeout))?,
        Cmd::Status { hello } => session::status(&c, hello)?,
        Cmd::Down { timeout } => session::down(&c, Duration::from_secs(timeout))?,
        Cmd::Uninstall => session::uninstall(&c)?,
        Cmd::McpConfig { server, server_dir } => {
            let dir = server_dir_or_default(server_dir);
            let v = mcp_config(&c, server, Some(dir.clone()));
            for n in mcpconfig::notes(Os::host(), &dir) {
                say(format!("note: {n}"));
            }
            v
        }
        Cmd::Supervise { .. } | Cmd::Open { .. } | Cmd::FetchGbe { .. } => {
            unreachable!()
        }
    };
    emit(&v)
}

fn fetch_gbe(g: &Global, out: Option<PathBuf>, archive: Option<PathBuf>) -> anyhow::Result<()> {
    use cs2_native::gbe;
    if Os::host() != Os::Linux {
        anyhow::bail!("fetch-gbe fetches gbe's Linux libraries; on Windows the package carries payload/gbe/win64 (scripts/stage-gbe.sh)");
    }
    let root = state::state_root(g.state_dir.as_deref())?;
    let dest = out.unwrap_or_else(|| gbe::fetched_dir(&root));
    let cache = gbe::cache_dir(&root);
    let o = gbe::FetchOpts { dest: &dest, cache: &cache, archive: archive.as_deref() };
    let v = gbe::fetch_linux(&o, say)?;
    if g.gbe_dir.is_some() {
        say("note: --gbe-dir / CS2MCP_GBE_DIR is set and wins over the fetched dir");
    }
    emit(&v)
}

fn server_dir_or_default(d: Option<PathBuf>) -> PathBuf {
    d.unwrap_or_else(mcpconfig::default_server_dir)
}

fn mcp_config(c: &Ctx, which: Which, server_dir: Option<PathBuf>) -> Value {
    let dir = server_dir_or_default(server_dir);
    let console = c.dirs.console_log(&c.install.root, c.strategy);
    let project = c.dirs.dir.join("mcp");
    let capture = c.dirs.capture();
    let capture = capture.as_path();
    mcpconfig::render(&mcpconfig::Input {
        os: Os::host(),
        server_dir: &dir,
        discovery: &c.discovery,
        console_log: &console,
        app_root: (c.strategy != Strategy::Overlay).then_some(c.install.root.as_path()),
        project_root: (Os::host() == Os::Windows).then_some(project.as_path()),
        build: c.install.build.as_deref(),
        capture_dir: cs2_native::capture::layer_dir(&c.artifacts.vklayer, Os::host()).map(|_| capture),
        which,
    })
}
