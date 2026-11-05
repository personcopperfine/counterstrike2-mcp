//! Runtime/launcher abstraction. Phase 1 = `docker` (podman, the published client image
//! `ghcr.io/personcopperfine/cs2-docker` + our own image under ./docker). Phase 2 = `local` (a game install on the host).
//!
//! The MCP tools never shell out to podman themselves; they ask the `Runtime`.

pub mod docker;
pub mod local;
pub mod readiness;

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum SessionKind {
    /// Windows workshop tools under Wine (`cs2.exe -tools`), bridge loadable.
    WineTools { addon: Option<String> },
    /// Windows game client under Wine without `-tools`: Metamod loads with the cs2-mcp bridge as a
    /// plain plugin; bridge RPC once a map runs (the GameFrame tick starts the listener; none at the menu); runs with `-insecure` (VAC off,
    /// no official servers; local servers unaffected).
    WineClient,
    /// Native Linux client (`cs2.sh -steam`), headless under Xvfb + lavapipe; no bridge.
    LinuxClient,
    /// Native Linux dedicated server (`cs2 -dedicated`): unverified in this repo.
    LinuxDedicated,
}

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub kind: SessionKind,
    /// wine-tools only: suite name; staged at `<app>/vtest/<name>.txt` and passed as `-testscript`
    pub testscript: Option<String>,
    /// the map the harness loads (`launch_session {map}`, wine-tools). Part of the identical-launch
    /// key and of the owned session's `SessionEntry`; the harness suite itself carries it too
    pub map: Option<String>,
    /// extra game command-line args (appended after the fixed flags)
    pub args: Vec<String>,
    /// request a display that is not the host's X11 (see workflows on headless)
    pub headless: bool,
    /// pass `--gpus all` (NVIDIA; WineTools always wants it, see run_vtest.sh)
    pub gpus: bool,
    /// explicit container name; must start with `cs2mcp-`. Default: generated.
    pub container_name: Option<String>,
    /// mount and enable the Vulkan capture layer (when the runtime has a valid layer dir);
    /// `false` is the opt-out (`capture: false` on launch_session)
    pub capture: bool,
    /// wine-tools: wait for the bridge (load it with a tool_request, then `control.hello`). `false`
    /// stops the readiness ladder at the channel (BridgeHello reported unchecked)
    pub bridge: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeStatus {
    pub running: bool,
    pub container: Option<String>,
    pub pid_hint: Option<u32>,
    pub display: Option<String>,
    pub notes: Vec<String>,
}

/// Host-side view of a running session: where files land.
#[derive(Debug, Clone)]
pub struct SessionPaths {
    /// host path of the game tree root (`.../workshop/run/app` or the Linux install)
    pub app_root: PathBuf,
    /// host path of the bridge discovery file, if the session can have a bridge
    pub discovery: Option<PathBuf>,
    /// host path of csgo/console.log
    pub console_log: Option<PathBuf>,
    /// Windows prefix (Wine) when applicable
    pub pfx: Option<PathBuf>,
}

#[allow(async_fn_in_trait)]
pub trait Runtime: Send + Sync {
    fn name(&self) -> &'static str;
    async fn status(&self) -> anyhow::Result<RuntimeStatus>;
    async fn up(&self, spec: &LaunchSpec) -> anyhow::Result<SessionPaths>;
    async fn down(&self) -> anyhow::Result<()>;
    /// run a helper inside the session environment (e.g. `vtmsend.exe` to post a tool_request)
    async fn exec(&self, argv: &[String]) -> anyhow::Result<std::process::Output>;
    fn paths(&self) -> SessionPaths;
    /// tail of the engine console / container log (stderr diagnostics for the model)
    async fn logs(&self, lines: usize) -> anyhow::Result<String>;
}
