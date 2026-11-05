//! Per-process state for the client tools + the client-specific config.
use crate::gametest::GameTestStore;
use crate::snapshot::SnapshotStore;
use crate::testscript::run::RunTable;
use clap::{Args, ValueEnum};
use cs2_mcp_common::bridge::{BridgeClient, SharedBridge};
use cs2_mcp_common::config::{CommonConfig, RuntimeKind};
use cs2_mcp_common::pathmap::PathMap;
use cs2_mcp_common::runtime::docker::{resolve_xauthority, DockerOptions, DockerRuntime};
use cs2_mcp_common::tools::ToolState;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Which kind of game session the tools talk to (AGENT.md "Two session flavours").
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Flavour {
    /// Windows `cs2.exe -tools` under Proton; bridge loadable (input.*, test.*, console.exec, game.tick, world.render)
    WineTools,
    /// Windows game client under Proton without -tools: Metamod loads with the cs2-mcp bridge as a
    /// plugin; bridge RPC once a map runs (the GameFrame tick starts the listener; none at the menu); runs with -insecure (VAC off, no
    /// official servers; local servers unaffected)
    WineClient,
    /// native Linux client, headless under Xvfb; no bridge
    LinuxClient,
    /// native Linux dedicated server (unverified)
    LinuxDedicated,
    /// Windows `cs2.exe` (with or without -tools) on this host, started by `cs2mcp-native up`
    /// (crates/cs2-native): the same bridge as WineTools/WineClient, found through
    /// `%LOCALAPPDATA%\cs2mcpbridge\session.json`; console.log via --console-log
    NativeWindows,
    /// decide from what is reachable (bridge hello -> WineTools; NativeWindows on a Windows host)
    Auto,
}

#[derive(Args, Debug, Clone)]
pub struct ClientConfig {
    #[command(flatten)]
    pub common: CommonConfig,

    /// Session flavour (default: auto = WineTools when the bridge answers, else unknown).
    #[arg(long, value_enum, default_value_t = Flavour::Auto, env = "CS2_MCP_FLAVOUR")]
    pub flavour: Flavour,

    /// Host path of the game's console.log (default: <app-root>/game/csgo/console.log).
    #[arg(long, env = "CS2_MCP_CONSOLE_LOG")]
    pub console_log: Option<PathBuf>,

    /// The capture layer's directory of a session this server did not launch (`cs2mcp-native up`
    /// loads the layer into the game with `CS2MCP_CAPTURE_DIR=<state>/capture`; `mcp-config`
    /// passes it). Set: screenshot/observe read only this dir, session `native`.
    #[arg(long)]
    pub capture_dir: Option<PathBuf>,

    /// ffprobe binary for the demo-to-video judge (default: `ffprobe` on PATH; absent = probe checks skipped).
    #[arg(long, env = "CS2_MCP_FFPROBE")]
    pub ffprobe: Option<PathBuf>,

    /// ffmpeg binary for the contact sheet (default: `ffmpeg` on PATH; absent = contact sheet skipped).
    #[arg(long, env = "CS2_MCP_FFMPEG")]
    pub ffmpeg: Option<PathBuf>,
    /// Directory with radar overview files `<map>.txt` (pos_x/pos_y/scale). Default:
    /// <project-root>/GameTracking-CS2/game/csgo/pak01_dir/resource/overviews.
    #[arg(long, env = "CS2_MCP_OVERVIEWS_DIR")]
    pub overviews_dir: Option<PathBuf>,
}

impl ClientConfig {
    /// Directory holding the radar overview files (`resource/overviews`), from config or the project root.
    pub fn overviews_dir_path(&self) -> PathBuf {
        self.overviews_dir.clone().unwrap_or_else(|| {
            self.common
                .project_root
                .join("GameTracking-CS2/game/csgo/pak01_dir/resource/overviews")
        })
    }

    /// The console.log a console tool reads when no session is picked (see `console::resolve`).
    pub fn default_console_log_path(&self) -> PathBuf {
        self.common.app_dir().join("game/csgo/console.log")
    }
}

#[derive(Clone)]
pub struct ClientState {
    pub bridge: SharedBridge,
    pub flavour: Flavour,
    /// `--console-log` as given; `None` when the flag was not passed (the default applies).
    pub console_log: Option<PathBuf>,
    /// `<project>/workshop/run/app/game/csgo/console.log`, used when no flag and no session matches.
    pub default_console_log: PathBuf,
    pub started_at: f64,
    pub launch_command: String,
    pub expect_build: Option<String>,
    /// Windows->host map for bridge-written files (`--path-map`, default `z:\app` -> workshop tree).
    pub path_map: Arc<PathMap>,
    /// Host path of the most recent capture (render/screenshot); `ocr {last: true}` reads it.
    pub last_capture: Arc<Mutex<Option<PathBuf>>>,
    pub runtime_kind: RuntimeKind,
    pub runtime: Arc<DockerRuntime>,
    /// `run_testscript` runs of this process (background tasks write them, `test_status` reads them)
    pub runs: Arc<Mutex<RunTable>>,
    /// ffprobe for the demo-to-video judge (`None`: the probe checks are skipped)
    pub ffprobe: Option<PathBuf>,
    /// ffmpeg for the contact sheet (`None`: the contact sheet is skipped)
    pub ffmpeg: Option<PathBuf>,
    /// configured capture-layer directory (`--vk-layer-dir`); validity is checked on use
    pub vk_layer: PathBuf,
    /// `--capture-dir`: the native session's capture dir (no podman session lookup)
    pub capture_dir: Option<PathBuf>,
    /// Radar overview files (`resource/overviews/<map>.txt`) for `radar` projection.
    pub overviews_dir: PathBuf,
    /// `<project>/artifacts`: `game_snapshot {export}` writes `snapshots/<name>.json` under it
    pub artifacts_dir: PathBuf,
    /// Named player snapshots of this process (`game_snapshot`, `game_restore {name}`), bounded.
    pub snapshots: Arc<Mutex<SnapshotStore>>,
    /// Results of `game_test` runs of this process (bounded; `game_test_results` reads them).
    pub gametests: Arc<Mutex<GameTestStore>>,
}

impl ClientState {
    pub fn from_config(cfg: &ClientConfig) -> anyhow::Result<Self> {
        let mut path_map = PathMap::for_host(&cfg.common.project_root, &cfg.common.app_dir(), &cfg.common.pfx_dir());
        if let Some(spec) = &cfg.common.path_map {
            path_map.extend(&PathMap::parse(spec)?);
        }
        Ok(ClientState {
            bridge: BridgeClient::shared(cfg.common.bridge_options(crate::NAME, crate::VERSION)),
            flavour: cfg.flavour,
            console_log: cfg.console_log.clone(),
            default_console_log: cfg.default_console_log_path(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
            launch_command: cfg.common.launch_command(),
            expect_build: cfg.common.expect_build.clone(),
            path_map: Arc::new(path_map),
            last_capture: Arc::new(Mutex::new(None)),
            runtime_kind: cfg.common.runtime,
            vk_layer: cfg.common.vk_layer_dir.clone(),
            capture_dir: cfg.capture_dir.clone(),
            overviews_dir: cfg.overviews_dir_path(),
            artifacts_dir: cfg.common.project_root.join("artifacts"),
            snapshots: Arc::new(Mutex::new(SnapshotStore::default())),
            gametests: Arc::new(Mutex::new(GameTestStore::default())),
            runtime: Arc::new(DockerRuntime::new(DockerOptions {
                host_display: cfg.common.host_display.clone(),
                xauthority: resolve_xauthority(cfg.common.xauthority.as_deref()),
                // checked per launch (the layer may be built after the server started)
                vk_layer_dir: Some(cfg.common.vk_layer_dir.clone()),
                s2sdk_probe_dir: cfg.common.s2sdk_probe_dir.clone(),
                install_script: cfg.common.install_script.clone(),
                image: cfg.common.image.clone(),
                // docker/linux-client.sh ships in this crate; the container gets it read-only
                linux_client_script: PathBuf::from(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/docker/linux-client.sh"
                )),
                ..DockerOptions::new(
                    cfg.common.project_root.clone(),
                    cfg.common.podman.clone(),
                    cfg.common.max_sessions,
                )
            })),
            runs: Arc::new(Mutex::new(RunTable::default())),
            ffprobe: resolve_tool(cfg.ffprobe.as_deref(), "ffprobe"),
            ffmpeg: resolve_tool(cfg.ffmpeg.as_deref(), "ffmpeg"),
        })
    }
}

/// An explicit path must be an executable file (else `None`); without one, the tool is looked up on
/// PATH. `None` means the tool is absent and the checks that need it are skipped.
fn resolve_tool(explicit: Option<&std::path::Path>, name: &str) -> Option<PathBuf> {
    match explicit {
        Some(p) => cs2_mcp_common::runtime::docker::binary_on_path(p),
        None => cs2_mcp_common::runtime::docker::binary_on_path(std::path::Path::new(name)),
    }
}

impl ToolState for ClientState {
    fn bridge(&self) -> &SharedBridge {
        &self.bridge
    }
}
