//! Flags/env shared by both servers (the POC's flags where they existed). Servers embed this with
//! `#[command(flatten)]` and add their own.
use clap::{Args, ValueEnum};
use std::path::PathBuf;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum RuntimeKind {
    /// podman/docker containers (phase 1; the only backend that exists today).
    Docker,
    /// a game install on the host filesystem (phase 2; not implemented).
    Local,
}

#[derive(Args, Debug, Clone)]
pub struct CommonConfig {
    /// Runtime backend. Docker first, local later (see workflows/01-docker-runtime.md, 10-local-install.md).
    #[arg(long, value_enum, default_value_t = RuntimeKind::Docker, env = "CS2_MCP_RUNTIME")]
    pub runtime: RuntimeKind,

    /// Bridge discovery file (session.json). Default: derived from --pfx.
    #[arg(long, env = "CS2_MCP_DISCOVERY")]
    pub discovery: Option<PathBuf>,

    /// Wine prefix of the tools runtime (host path). Default: <project-root>/workshop/run/app/compatdata/730/pfx.
    #[arg(long, env = "CS2_MCP_PFX")]
    pub pfx: Option<PathBuf>,

    /// Project root: holds the runtime state (`workshop/run/app`, `.podman/xdg`, `artifacts/`).
    /// Default: this checkout. An older layout (a parent directory holding this repo as `cs2-mcp/`)
    /// works too (`mcp_repo_dir`).
    #[arg(long, env = "CS2_MCP_PROJECT_ROOT", default_value = crate::DEFAULT_PROJECT_ROOT)]
    pub project_root: PathBuf,

    /// Host path of the game tree root (the directory holding `game/` and `content/`). Default:
    /// `<project-root>/workshop/run/app` (the container's tree). A native install passes its
    /// install root (`.../Counter-Strike Global Offensive`; `cs2mcp-native mcp-config` does).
    #[arg(long, env = "CS2_MCP_APP_ROOT")]
    pub app_root: Option<PathBuf>,

    /// Signature database directory (`<module>.json`). Default: this repo's `sigs/signatures`.
    #[arg(long, env = "CS2_MCP_SIGS")]
    pub sigs: Option<PathBuf>,

    /// Per-request bridge deadline (ms). Never 0.
    #[arg(long, default_value_t = 30_000)]
    pub deadline_ms: u64,

    /// Per-tool-call budget (ms); each bridge request gets the remainder, capped by --deadline-ms.
    #[arg(long, default_value_t = 45_000)]
    pub tool_budget_ms: u64,

    /// Modal gate: refuse queued methods when mainThreadAgeMs exceeds this (ms).
    #[arg(long, default_value_t = 10_000)]
    pub blocked_age_ms: u64,

    /// Disable the pre-dispatch liveness (modal) gate.
    #[arg(long, default_value_t = false)]
    pub no_blocked_gate: bool,

    /// Windows->host path map, e.g. 'z:\app=/host/tree;c:=/pfx/drive_c'.
    #[arg(long, env = "CS2_MCP_PATH_MAP")]
    pub path_map: Option<String>,

    /// Expected CS2 ClientVersion (e.g. 2000930). Default: none (the sig DB's newest build is reported, not enforced).
    #[arg(long, env = "CS2_MCP_EXPECT_BUILD")]
    pub expect_build: Option<String>,

    /// Opt in to launching a session when the bridge is unreachable (attach-only otherwise).
    #[arg(long, default_value_t = false)]
    pub launch: bool,

    /// Maximum number of running `cs2*` containers launch_session allows (GPU memory: ~3 CS2
    /// instances fit at 1080p on this host).
    #[arg(long, default_value_t = 2, env = "CS2_MCP_MAX_SESSIONS", value_parser = parse_max_sessions)]
    pub max_sessions: usize,

    /// podman binary used by the docker runtime (tests point this at a fake).
    #[arg(long, default_value = "podman", env = "CS2_MCP_PODMAN")]
    pub podman: PathBuf,

    /// Container image the docker runtime runs (the game client userland: steamrt4, Xvfb,
    /// lavapipe, gbe; the Wine tools run in it too, from `<project-root>/workshop/run/app`).
    /// Default: the published `ghcr.io/personcopperfine/cs2-docker:latest`; a custom build from a
    /// clone of <https://github.com/personcopperfine/cs2-docker> (`podman build -t <name> <clone>/client`)
    /// is passed here.
    #[arg(long, env = "CS2_MCP_IMAGE", default_value = crate::runtime::docker::DEFAULT_IMAGE)]
    pub image: String,

    /// `bridge/install-rs.sh` run as `ensure` before every launch. Default: derived from the
    /// client's `docker/linux-client.sh` checkout (`<repo>/bridge/install-rs.sh`). Tests point
    /// this at an inert stub.
    #[arg(long, env = "CS2_MCP_INSTALL_SCRIPT")]
    pub install_script: Option<PathBuf>,

    /// X cookie file for wine-tools sessions (default: newest /run/user/<uid>/xauth_*, as run_vtest.sh).
    #[arg(long, env = "XAUTHORITY")]
    pub xauthority: Option<PathBuf>,

    /// Host X display for wine-tools sessions.
    #[arg(long, default_value = ":0", env = "DISPLAY")]
    pub host_display: String,

    /// Directory with `libcs2_vk_capture.so` and `VkLayer_cs2mcp_capture.json` (built by
    /// `client-mcp/docker/build-vk-layer.sh`). When both files are present, sessions launch with the
    /// capture layer mounted and enabled (see `capture: false` on launch_session to opt out).
    #[arg(long, env = "CS2_MCP_VK_LAYER_DIR", default_value = DEFAULT_VK_LAYER_DIR)]
    pub vk_layer_dir: PathBuf,

    /// Directory with `libs2sdk_probe.so` (built by `crates/s2sdk/docker/build-probe.sh`). When set, Linux
    /// sessions (linux-client, linux-dedicated) mount it read-only at /cs2mcp/s2sdk and preload the
    /// probe (`CS2MCP_PRELOAD`, prepended to LD_PRELOAD by lib-preamble.sh; the probe writes its
    /// report to S2SDK_PROBE_OUT=/work/s2sdk-probe.json). Unset: no probe, byte-identical argv.
    #[arg(long, env = "CS2_MCP_S2SDK_PROBE_DIR")]
    pub s2sdk_probe_dir: Option<PathBuf>,

    /// Fallback addon for the file-route tools (under `<project>/content/csgo_addons/<addon>`):
    /// used only while no `set_addon` choice is pinned and the active Hammer document is not in
    /// an addon. The ModelDoc container is started with `-addon <addon>` (bridge/runtime.sh).
    #[arg(long, env = "CS2_MCP_ADDON", default_value = "mcp_test")]
    pub addon: String,

    /// Tools container to exec in, overriding label discovery (default: the running container
    /// labelled `cs2mcp.tool=modeldoc` is found by `container_by_label` at call time).
    #[arg(long, env = "CS2_MCP_TOOLS_CONTAINER")]
    pub tools_container: Option<String>,

    /// Test knob: the runtime's short podman timeout in ms (default 10 s). Tests with a forked
    /// fake podman raise it so host load cannot turn a slow exec into a delivery warning.
    #[arg(long, hide = true, env = "CS2_MCP_PODMAN_SHORT_TIMEOUT_MS")]
    pub podman_short_timeout_ms: Option<u64>,
}

/// `<repo>/sigs/signatures`: the signature database (`sigs/README.md`).
/// The launch command on a Windows host: `cs2mcp-native` (crates/cs2-native) prepares the user's
/// install, starts the game with the bridge and loads it; a `--tools` session serves both servers
/// (client sessions need `--map <map>` before the bridge listens).
pub const NATIVE_LAUNCH_COMMAND: &str = "cs2mcp-native up --steam gbe --tools";

pub const DEFAULT_SIGS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../sigs/signatures");

/// `<repo>/target/vklayer`: where `build-vk-layer.sh` puts the layer by default.
pub const DEFAULT_VK_LAYER_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/vklayer");

fn parse_max_sessions(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(format!("must be an integer >= 1, got {s:?}")),
    }
}

impl CommonConfig {
    pub fn sigs_dir(&self) -> PathBuf {
        self.sigs.clone().unwrap_or_else(|| {
            let built = PathBuf::from(DEFAULT_SIGS_DIR);
            if built.is_dir() {
                return built;
            }
            // a copied binary (a native Windows install): `<exe dir>/sigs`, the database copied
            // next to the server (crates/cs2-native/scripts/package.sh lays it out)
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|d| d.join("sigs")))
                .filter(|d| d.is_dir())
                .unwrap_or(built)
        })
    }
    /// The game tree root: `--app-root`, else `<project-root>/workshop/run/app`.
    pub fn app_dir(&self) -> PathBuf {
        self.app_root
            .clone()
            .unwrap_or_else(|| self.project_root.join("workshop/run/app"))
    }
    pub fn pfx_dir(&self) -> PathBuf {
        self.pfx.clone().unwrap_or_else(|| {
            self.project_root
                .join("workshop/run/app/compatdata/730/pfx")
        })
    }
    pub fn discovery_path(&self) -> PathBuf {
        self.discovery.clone().unwrap_or_else(|| {
            // native Windows: the bridge's own `%LOCALAPPDATA%` file, no prefix in between
            #[cfg(windows)]
            if let Some(path) = crate::bridge::discovery::windows_default_path() {
                return path;
            }
            crate::bridge::discovery::default_path(&self.pfx_dir())
        })
    }
    /// Host command that brings a tools session up (the POC's `--launch-script`): `bridge/runtime.sh up`;
    /// on a Windows host, where there is no container, the native launcher's
    /// [`NATIVE_LAUNCH_COMMAND`] (crates/cs2-native FIXME 18).
    pub fn launch_command(&self) -> String {
        if cfg!(windows) {
            return NATIVE_LAUNCH_COMMAND.into();
        }
        format!(
            "{}/bridge/runtime.sh up",
            crate::mcp_repo_dir(&self.project_root).display()
        )
    }
    pub fn bridge_options(
        &self,
        client_name: &'static str,
        client_version: &'static str,
    ) -> crate::bridge::client::ClientOptions {
        crate::bridge::client::ClientOptions {
            discovery_path: self.discovery_path(),
            deadline_ms: self.deadline_ms,
            blocked_age_ms: self.blocked_age_ms,
            expected_build: self.expect_build.clone(),
            block_when_stale: !self.no_blocked_gate,
            client_name,
            client_version,
            ..Default::default()
        }
    }
}
