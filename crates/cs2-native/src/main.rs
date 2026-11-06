//! `cs2mcp-native`: see the crate README. Results go to stdout as JSON, progress to stderr
//! (`--events jsonl`: one JSON [`Event`](cs2_native::events::Event) per line, for the GUI).
//! An error is a JSON result too: `{"error": "...", "findings": [...]}` on stdout.
//!
//! Exit codes: 0 done; 1 an error; 2 stopped on (or finished with) findings that need the user's
//! decision (`findings.rs`; the JSON lists them); `run`: the game's exit code, 3 when the volume
//! could not be restored.

use clap::{Args, Parser, Subcommand};
use cs2_native::args::GameArgs;
use cs2_native::dialog;
use cs2_native::events::Events;
use cs2_native::findings::{self, Finding, Policy, Stopped};
use cs2_native::install::Os;
use cs2_native::mcpconfig::{self, Servers, Transport, Which};
use cs2_native::plan::SteamMode;
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
    /// Progress on stderr: `text` (`[cs2mcp-native] ...` lines) or `jsonl` (one JSON event per
    /// line: `{"kind":"info"|"warn","text":...}`, `{"kind":"stage","stage":...,"seconds":...}`).
    #[arg(long, global = true, value_enum, default_value_t = EventsFormat::Text, env = "CS2MCP_EVENTS")]
    events: EventsFormat,
    /// Never show a native error dialog (also `CS2MCP_NO_DIALOG=1`). Without it, a failed
    /// prepare/up/wait/down/uninstall/open shows one on a desktop (zenity/kdialog, MessageBoxW);
    /// never in a container, over ssh (Windows), or for `run`.
    #[arg(long, global = true)]
    no_dialog: bool,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum EventsFormat {
    Text,
    Jsonl,
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
        /// Also the preflight findings `discover` reports (payload, leftovers, the game build).
        #[arg(long)]
        preflight: bool,
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
    /// (internal) the Windows error dialog, run as a detached child (dialog.rs).
    #[command(hide = true)]
    ShowDialog {
        #[arg(long)]
        title: String,
        #[arg(long)]
        text: String,
    },
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
    let ev = match cli.g.events {
        EventsFormat::Text => Events::stderr(),
        EventsFormat::Jsonl => Events::jsonl(),
    };
    // the commands whose failure leaves the install not as the user asked: a desktop user gets a
    // dialog (dialog.rs decides whether one can be shown). Never `run` (a container) or `supervise`.
    let dialog_cmd = matches!(cli.cmd, Cmd::Prepare { .. } | Cmd::Up { .. } | Cmd::Wait { .. } | Cmd::Down { .. } | Cmd::Uninstall | Cmd::Open { .. });
    let no_dialog = cli.g.no_dialog;
    let (code, error, findings) = match run(cli, &ev) {
        Ok(Done { code, unresolved: None }) => std::process::exit(code),
        // a restore that finished with an open decision (conflicts, a tree that did not verify):
        // the JSON result is already out, the dialog repeats it
        Ok(Done { code, unresolved: Some(f) }) => {
            if dialog_cmd {
                if let Some(tool) = dialog::choose(&dialog::Facts::gather(no_dialog)) {
                    dialog::show(tool, dialog::TITLE, &dialog::message("The install was restored, but needs your attention", &f));
                }
            }
            std::process::exit(code)
        }
        Err(e) => match e.downcast::<Stopped>() {
            // the findings are the answer: the JSON a script or the GUI reads, exit 2
            Ok(st) => {
                ev.warn(format!("stopped: {} (exit 2; see the findings)", st.message));
                (2, st.message, st.findings)
            }
            Err(e) => {
                ev.warn(format!("error: {e:#}"));
                (1, format!("{e:#}"), Vec::new())
            }
        },
    };
    let _ = emit(&json!({"error": error, "findings": findings}));
    if dialog_cmd {
        if let Some(tool) = dialog::choose(&dialog::Facts::gather(no_dialog)) {
            dialog::show(tool, dialog::TITLE, &dialog::message(&error, &findings));
        }
    }
    std::process::exit(code);
}

/// How a command ended without an error: its exit code and, for a restore that left a decision
/// open, the findings behind it.
struct Done {
    code: i32,
    unresolved: Option<Vec<Finding>>,
}

impl From<i32> for Done {
    fn from(code: i32) -> Done {
        Done { code, unresolved: None }
    }
}

/// 2 (with the findings) when the result still holds a decision the policy did not accept (a
/// restore with conflicts).
/// Append `more` to `v["findings"]`, leaving out the ones already there: `status` and the
/// preflight both report the game build (seen live: the warning shown twice).
fn add_findings(v: &mut Value, more: &[Finding]) {
    if !v["findings"].is_array() {
        v["findings"] = Value::Array(Vec::new());
    }
    let Some(f) = v["findings"].as_array_mut() else { return };
    for x in more.iter().filter_map(|x| serde_json::to_value(x).ok()) {
        if !f.contains(&x) {
            f.push(x);
        }
    }
}

fn finished(v: &Value, policy: Policy) -> Done {
    let f: Vec<Finding> = serde_json::from_value(v["findings"].clone()).unwrap_or_default();
    let code = findings::exit_code(&f, policy);
    Done { code, unresolved: (code != 0).then_some(f) }
}

fn run(cli: Cli, ev: &Events) -> anyhow::Result<Done> {
    match cli.cmd {
        Cmd::Supervise { session_file } => return session::supervise(&session_file, ev).map(|()| 0.into()),
        Cmd::ShowDialog { title, text } => {
            #[cfg(windows)]
            cs2_native::win::message_box(&title, &text);
            #[cfg(not(windows))]
            eprintln!("{title}: {text}");
            return Ok(0.into());
        }
        Cmd::Open { asset } => {
            let seq = cs2_native::vtm::post(&cs2_native::vtm::open_asset(&asset)).map_err(anyhow::Error::msg)?;
            emit(&json!({"posted": seq, "asset": asset}))?;
            return Ok(0.into());
        }
        Cmd::FetchGbe { out, archive } => return fetch_gbe(&cli.g, out, archive, ev).map(|()| 0.into()),
        _ => {}
    }
    // the container mode is always in place (an explicit overlay is refused by `run`)
    let strategy = matches!(cli.cmd, Cmd::Run { .. }).then(|| if cli.g.strategy == Strategy::Auto { Strategy::InPlace } else { cli.g.strategy });
    let o = opts(&cli.g, strategy);
    let c = Ctx::open(&o, ev.clone())?;
    let mut code = Done::from(0);
    let v = match cli.cmd {
        Cmd::Discover => {
            let mut pre = c.preflight()?;
            // no manifest: our files in the tree are leftovers no record covers (like status)
            if c.install.root.is_dir() && c.dirs.store().load()?.is_none() {
                pre.extend(c.leftovers().1);
            }
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
                // `$APPIMAGE cs2mcp-native` inside an AppImage (the supervisor outlives this mount), else this exe
                launcher: cs2_native::selfexe::native_command()?,
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
            return Ok(game_code.into());
        }
        Cmd::Wait { timeout } => session::wait_ready(&c, Duration::from_secs(timeout), &AtomicBool::new(false))?,
        Cmd::Status { hello, preflight } => {
            let mut v = session::status(&c, hello)?;
            if preflight {
                add_findings(&mut v, &c.preflight()?);
            }
            v
        }
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
        Cmd::Supervise { .. } | Cmd::ShowDialog { .. } | Cmd::Open { .. } | Cmd::FetchGbe { .. } => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_preflight_reports_a_finding_once() {
        let build = cs2_native::install::build_finding(Some("1")).unwrap();
        let info = Finding::new(findings::Code::PayloadUnverified, findings::Severity::Info, "dev build");
        let mut v = json!({"findings": [build]});
        add_findings(&mut v, &[info, build.clone()]);
        assert_eq!(v["findings"].as_array().unwrap().len(), 2);
        let mut v = json!({});
        add_findings(&mut v, &[build]);
        assert_eq!(v["findings"].as_array().unwrap().len(), 1);
    }

    /// The command line dialog.rs builds for the Windows box parses, whatever the message starts with.
    #[test]
    fn show_dialog_takes_a_message_that_starts_like_an_option() {
        let m = "--steam client: the Steam client is not running";
        let a = dialog::argv(dialog::Tool::MessageBox, dialog::TITLE, m);
        let cli = Cli::try_parse_from(std::iter::once("cs2mcp-native".to_string()).chain(a)).unwrap();
        match cli.cmd {
            Cmd::ShowDialog { title, text } => assert_eq!((title.as_str(), text.as_str()), (dialog::TITLE, m)),
            _ => panic!("not show-dialog"),
        }
    }
}
