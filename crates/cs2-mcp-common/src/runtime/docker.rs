//! podman backend (phase 1).
//!
//! Facts it encodes (from bridge/runtime.sh, ../idabins/worknotes/vtest/run_vtest.sh,
//! the `cs2-docker` submodule's client/entrypoint.sh, an earlier harness/run-cs2.sh not in this repo):
//! * userland image [`DEFAULT_IMAGE`] (steamrt4 + lavapipe fork + Xvfb + gbe);
//! * Wine sessions mount `<project>/workshop/run/app` at `/app` and `.../run/cache` at `/cache`,
//!   run `./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console -condebug -insecure [args] +engine_no_focus_sleep 0 +r_always_render_all_windows 1`
//!   ([`FOCUS_ARGS`]: full speed while another window has focus), `--network=host`, `--gpus all`,
//!   and need the HOST X display (`-v /tmp/.X11-unix`, XAUTHORITY); headless Wine is refused
//!   (Xvfb dies: BAD_EDID -> DXVK divide-by-zero, hammer-mcp 08). Container PID 1 is the launcher
//!   (`crates/mm-launch`), not the game: it starts `cs2.exe` suspended, injects the Metamod loader
//!   (`cs2mcp_mmload.dll`, loader path in `CS2MCP_MM_LOADER`), then waits and exits with the
//!   game's code - the gameinfo-free Metamod load (bridge/metamod/README.md, "Launching");
//! * Linux sessions run `client-mcp/docker/linux-client.sh` (mounted read-only at /cs2mcp/), a port of
//!   harness/run-cs2.sh with an in-container Xvfb :99 and console.log mirrored into the artifacts dir.
//!   The game lives in the `cs2-home` volume (`/home/steam/steamapps/...`, run-cs2.sh:9); both Linux
//!   scripts prepend `csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so` to LD_PRELOAD on
//!   the cs2.sh command only (the gameinfo-free Metamod load) and refuse to launch without it;
//! * LinuxDedicated runs `client-mcp/docker/linux-dedicated.sh` (no X), `--network=host`, the same
//!   `cs2-home` volume and the shared preamble `lib-preamble.sh` (mounted next to it). The preamble
//!   runs under a flock in the volume, so a client and a dedicated server can run together;
//! * podman needs `XDG_RUNTIME_DIR=<project>/.podman/xdg` on every call.
//!
//! Concurrency policy (AGENT.md rule 8):
//! * at most `--max-sessions` running `cs2*` containers in total (GPU memory; counts containers this
//!   server did not launch too);
//! * at most one session per kind per shared mount: one Wine session per app dir (WineTools and
//!   WineClient both mount it, so at most one of the two), one
//!   LinuxClient session and one LinuxDedicated session per `cs2-home` volume (`KindBusy`); a client
//!   and a dedicated server may share the volume (see `may_conflict`). Checked under the in-process gate and a
//!   cross-process file lock (`<project>/.podman/cs2mcp-launch.lock`) across `ps` -> `run`.
//!
//! Containers run WITHOUT `--rm`, so their logs stay available after the game exits. `stop_named`
//! stops them (`podman stop -t 0`, detached, never killed by us), waits for the exit with
//! `podman wait`, then removes them (`podman rm`, no `-f`). podman is never killed mid-stop.
//!
//! Wine readiness mirrors bridge/runtime.sh: `wait_channel` (vtmsend posts a probe command and
//! prints `creator version` once the VToolMessage mutex exists) and `load_rs_bridge` (a
//! tool_request for `cs2_bridge`, re-posted every 5 s until the discovery file is fresh; then
//! `control.hello`).

use super::readiness::{
    self, Check, ExitInfo, Readiness, ReadyStage, UncheckedStage, CHANNEL_UP_MARKER,
    SERVER_LOG_NAME,
};
use super::*;
use crate::bridge::{BridgeClient, ClientOptions, Kind};
use crate::capture::vklayer::{
    layer_status, CONTAINER_CAPTURE_DIR, CONTAINER_LAYER_DIR, LAYER_NAME,
};
use serde_json::Value;
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::io::SeekFrom;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Mutex as AsyncMutex;

/// Every container this runtime launches starts with this (and only these are stopped).
pub const NAME_PREFIX: &str = "cs2mcp-";
/// Running containers whose name starts with this count toward `--max-sessions`.
pub const CAP_PREFIX: &str = "cs2";
/// The published client image (https://github.com/personcopperfine/cs2-docker, `client/`);
/// `--image` / `CS2_MCP_IMAGE` overrides it (e.g. a local build of the `cs2-docker` submodule).
pub const DEFAULT_IMAGE: &str = "ghcr.io/personcopperfine/cs2-docker:latest";
/// The Linux game tree inside the `cs2-home` volume (harness/run-cs2.sh:9; tests/cs2/t30_cs2_boot.sh:31).
pub const DEFAULT_LINUX_GAME_DIR: &str =
    "/home/steam/steamapps/common/Counter-Strike Global Offensive/game";
pub const LINUX_HOME_VOLUME: &str = "cs2-home";
pub const LINUX_HOME_DEST: &str = "/home/steam";
pub const LINUX_SCRIPT_DEST: &str = "/cs2mcp/linux-client.sh";
pub const LINUX_DEDICATED_SCRIPT_DEST: &str = "/cs2mcp/linux-dedicated.sh";
/// Shared preamble, sourced by both Linux scripts; mounted next to them (see `sibling_script`).
pub const LINUX_LIB_DEST: &str = "/cs2mcp/lib-preamble.sh";
/// Script file names next to `linux-client.sh` on the host (the directory `client-mcp/docker/`).
pub const LINUX_DEDICATED_SCRIPT_NAME: &str = "linux-dedicated.sh";
pub const LINUX_LIB_NAME: &str = "lib-preamble.sh";
/// Linux sessions with an s2sdk probe dir mount it read-only here (see `s2sdk_probe_argv`).
pub const CONTAINER_S2SDK_DIR: &str = "/cs2mcp/s2sdk";
/// The probe payload inside the mounted dir (`crates/s2sdk/docker/build-probe.sh` copies it there).
pub const PROBE_SO_NAME: &str = "libs2sdk_probe.so";
/// The probe's JSON report, written into the artifacts dir mount (Linux sessions always have /work).
pub const CONTAINER_PROBE_OUT: &str = "/work/s2sdk-probe.json";
/// Default game port of a dedicated server (game traffic and RCON, both TCP/UDP on the same port).
pub const DEDICATED_DEFAULT_PORT: u16 = 27015;
pub const WINE_APP_DEST: &str = "/app";
pub const LINUX_WORK_DEST: &str = "/work";
pub const DEFAULT_SHORT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(60);
/// Upper bound for `bridge/install-rs.sh ensure` at launch (a first install copies the bridge,
/// Metamod and the SDK entry; a read-only "current?" check returns in a second).
pub const DEFAULT_INSTALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Grace a timed-out `install-rs.sh ensure` process group gets between SIGTERM and SIGKILL.
const INSTALL_KILL_GRACE: Duration = Duration::from_secs(5);
/// How many stderr lines of a failed ensure are quoted back to the model.
pub const INSTALL_STDERR_LINES: usize = 20;
const LOCK_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `list_sessions` answers from its cache (podman is not asked again inside this window).
const SESSION_CACHE: Duration = Duration::from_millis(1500);
const LIVENESS_EVERY: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const EXIT_LOG_LINES: usize = 20;
/// Bytes re-read before the last scan offset, so a marker split across two reads is found.
const SCAN_OVERLAP: u64 = 256;
/// Wine: vtmsend probe pacing (one real probe per second; between probes the wait only polls).
const CHANNEL_PROBE_EVERY: Duration = Duration::from_secs(1);
/// Wine: tool_request re-post pacing while the discovery file is missing.
const BRIDGE_REPOST_EVERY: Duration = Duration::from_secs(5);
/// Wine: `control.hello` pacing once a fresh discovery file exists.
const HELLO_EVERY: Duration = Duration::from_millis(500);
const HELLO_CONNECT: Duration = Duration::from_millis(2000);
const HELLO_DEADLINE: Duration = Duration::from_millis(5000);
/// A discovery file this much older than the launch still counts as fresh (mtime granularity).
const FRESH_SLACK: Duration = Duration::from_secs(2);
/// Wine: the payload file vtmsend reads (`<app>/vtm_payload.kv3`), written atomically.
pub const VTM_PAYLOAD_NAME: &str = "vtm_payload.kv3";
/// Wine: the bridge's log in the app dir (diagnostics only: its tail is quoted on a bridge timeout).
pub const BRIDGE_LOG_NAME: &str = "mcp_bridge_rs.log";
/// A `stop` older than this is issued again when the container still reports `stopping`.
const STOP_REISSUE_AFTER: Duration = Duration::from_secs(20);
/// Inspect pacing of the stop wait when `podman wait` cannot be used.
const STOP_POLL: Duration = Duration::from_millis(500);
/// The removal after an exit is confirmed for at least this long (capped at the short podman
/// timeout): see `remove_confirmed`.
const RM_CONFIRM: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("{} CS2 containers already running (max {max}); foreign cs2* containers count too: {}", running.len(), running.join(", "))]
    TooManySessions { running: Vec<String>, max: usize },
    #[error("a {kind} session is already running and shares the {shared}: {container} (one per kind per shared mount)")]
    KindBusy {
        kind: &'static str,
        container: String,
        shared: &'static str,
    },
    #[error("refusing to touch container {0:?}: only {NAME_PREFIX}* containers are managed")]
    ForeignContainer(String),
    #[error("invalid container name {0:?}: must be a podman name ([A-Za-z0-9._-], no leading '-' or '.')")]
    InvalidExecName(String),
    #[error("invalid container name {0:?}: must match ^cs2mcp-[a-z0-9][a-z0-9-]*$")]
    InvalidName(String),
    #[error("no session to act on: {0}")]
    NoSession(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("XAUTHORITY file not found ({0}); pass --xauthority or set XAUTHORITY")]
    MissingXauthority(String),
    #[error("suite not staged: {0} does not exist (staging is not implemented; put the suite there first)")]
    SuiteNotStaged(String),
    #[error("cannot run {0}: not found (set --podman / CS2_MCP_PODMAN)")]
    PodmanUnavailable(String),
    #[error("podman {args} did not finish within {secs} s")]
    PodmanTimeout { args: String, secs: u64 },
    #[error("container {container} is still stopping after {secs} s (podman has not reported it exited); stop_session again")]
    StopPending { container: String, secs: u64 },
    #[error("waited {secs} s for the launch lock {path} (another server is launching)")]
    LaunchLockTimeout { path: String, secs: u64 },
    #[error("podman {args} failed ({status}): {stderr}")]
    Podman {
        args: String,
        status: String,
        stderr: String,
    },
    /// `install-rs.sh ensure` refused because a running container mounts the tree it would repair
    /// (exit 4). `stderr` names the containers and what is missing/stale.
    #[error("the game tree needs (re)installing but containers mount it; stop them or run `bridge/install-rs.sh ensure` by hand: {stderr}")]
    InstallBusy { stderr: String },
    /// Any other non-zero ensure exit: carries the exit code and the script's stderr tail.
    #[error("install script {path} {args} failed (exit {code}): {stderr}")]
    InstallFailed {
        path: String,
        args: String,
        code: i32,
        stderr: String,
    },
    #[error(
        "install script {path} {args} did not finish within {secs} s (the launch was not started)"
    )]
    InstallTimeout {
        path: String,
        args: String,
        secs: u64,
    },
    #[error("install script not found: {path} (configuration error: set DockerOptions.install_script, or point the client at a checkout that has client-mcp/docker/linux-client.sh)")]
    InstallScriptMissing { path: String },
}

/// Options for [`DockerRuntime::new`].
#[derive(Debug, Clone)]
pub struct DockerOptions {
    pub project_root: PathBuf,
    pub podman: PathBuf,
    pub image: String,
    pub max_sessions: usize,
    /// Linux game dir inside the `cs2-home` volume, passed to the script as `CS2_GAME_DIR`.
    pub linux_game_dir: String,
    /// host path of `client-mcp/docker/linux-client.sh` (mounted read-only into the container).
    /// `linux-dedicated.sh` and `lib-preamble.sh` are looked up next to it.
    pub linux_client_script: PathBuf,
    /// host X display for wine-tools (`DISPLAY`)
    pub host_display: String,
    /// X cookie file for wine-tools; `None` = refused with `MissingXauthority`
    pub xauthority: Option<PathBuf>,
    pub short_timeout: Duration,
    pub run_timeout: Duration,
    /// Configured capture-layer directory. Validity (both files present) is checked at each launch;
    /// an invalid one launches without the layer. `None`: no layer at all.
    pub vk_layer_dir: Option<PathBuf>,
    /// Configured s2sdk probe directory. When `Some`, Linux sessions mount it read-only and
    /// preload the probe (see [`s2sdk_probe_argv`]); `None`: no probe, byte-identical argv.
    pub s2sdk_probe_dir: Option<PathBuf>,
    /// `bridge/install-rs.sh`, run as `ensure` before every launch. `None`: derived from
    /// `linux_client_script` (repo root + `bridge/install-rs.sh`, see [`install_script_for`]).
    pub install_script: Option<PathBuf>,
    /// Upper bound for that ensure call (see [`DEFAULT_INSTALL_TIMEOUT`]).
    pub install_timeout: Duration,
}

impl DockerOptions {
    /// Defaults for everything except the project root and the podman binary.
    pub fn new(project_root: PathBuf, podman: PathBuf, max_sessions: usize) -> Self {
        DockerOptions {
            project_root,
            podman,
            image: DEFAULT_IMAGE.into(),
            max_sessions,
            linux_game_dir: DEFAULT_LINUX_GAME_DIR.into(),
            // set by the caller: client-mcp passes its own docker/linux-client.sh
            linux_client_script: PathBuf::new(),
            host_display: ":0".into(),
            xauthority: None,
            short_timeout: DEFAULT_SHORT_TIMEOUT,
            run_timeout: DEFAULT_RUN_TIMEOUT,
            vk_layer_dir: None,
            s2sdk_probe_dir: None,
            install_script: None,
            install_timeout: DEFAULT_INSTALL_TIMEOUT,
        }
    }
}

/// A container this runtime launched and has not stopped (the single source of truth for owned
/// sessions; the tools read it through the runtime).
#[derive(Debug, Clone)]
pub struct Owned {
    pub name: String,
    pub spec: LaunchSpec,
    pub paths: SessionPaths,
    pub artifacts_dir: PathBuf,
    /// `<artifacts>/capture` when the session was launched with the capture layer
    pub capture_dir: Option<PathBuf>,
    /// the podman invocation, binary first
    pub argv: Vec<String>,
    /// when podman was asked to run the container (the reference for a fresh discovery file)
    pub launched_at: SystemTime,
    /// Wine: the VToolMessage channel answered a probe (set once; persists across waits)
    pub channel_up: bool,
    /// Wine: when the bridge's tool_request was last posted
    pub bridge_requested_at: Option<Instant>,
    /// Wine: size of `mcp_bridge_rs.log` at launch; a bridge timeout quotes what was appended since
    pub bridge_log_offset: u64,
    /// Wine: the bridge answered `control.hello` (or refused it as busy: another client holds its
    /// one authenticated slot, so it is up). Set once; later waits do not connect again.
    pub bridge_up: bool,
}

/// What `launch` produced.
#[derive(Debug, Clone)]
pub struct Launched {
    pub container: String,
    pub argv: Vec<String>,
    pub paths: SessionPaths,
    pub artifacts_dir: PathBuf,
    /// `<artifacts>/capture` when the session was launched with the capture layer (else `None`)
    pub capture_dir: Option<PathBuf>,
    /// an identical session was already running; nothing new was started
    pub reused: bool,
}

/// `podman inspect` view of one container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerState {
    Running,
    /// podman is stopping it (`stop` issued, the process has not exited yet)
    Stopping,
    Exited {
        code: i64,
    },
    Other(String),
    Gone,
}

/// What [`DockerRuntime::stop_named`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// this call removed the container
    Removed,
    /// nothing to remove: not listed, or removed meanwhile
    AlreadyGone,
    /// podman has not reported the container exited within the wait; call again (idempotent)
    Pending {
        state: String,
        stop_issued_ms: Option<u64>,
    },
}

/// Where one [`DockerRuntime::stop_named_report`] call spent its time, in ms since the call began.
/// A step this call did not reach (or did not take) is `None`. podman's own bookkeeping can take
/// far longer than the game's teardown: on a host whose storage stalls, `stop`, the exit cleanup
/// and `rm` each wait for the container store to sync, so exited -> removed is podman, not the game.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopTiming {
    /// the state the first `inspect` reported; `unknown` when it timed out (the stop is issued anyway)
    pub initial_state: String,
    /// this call issued `podman stop` (not issued: already exited, or a recent stop is still running)
    pub stop_issued_ms: Option<u64>,
    /// podman reported the container exited (`podman wait`, or the first `inspect` already said so).
    /// podman records the exit (and cleans up) before it reports it, so on a stalled store this is
    /// later than the game's death: live, the processes were gone ~1.5 s after the stop and podman
    /// reported the exit at 93.7 s
    pub exited_ms: Option<u64>,
    /// `podman rm` was spawned
    pub rm_issued_ms: Option<u64>,
    /// the container was confirmed gone after this call's `rm`
    pub removed_ms: Option<u64>,
    /// the whole call
    pub total_ms: u64,
}

/// [`StopOutcome`] with the [`StopTiming`] of the call that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopReport {
    pub outcome: StopOutcome,
    pub timing: StopTiming,
}

/// Marks the steps of one stop against its start.
struct StopClock {
    started: Instant,
    timing: StopTiming,
}

impl StopClock {
    fn new() -> Self {
        StopClock {
            started: Instant::now(),
            timing: StopTiming::default(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

/// The outcome of waiting for a container to exit.
enum Exit {
    Exited,
    Gone,
    Pending,
}

/// The handle [`DockerRuntime::exec_in_background`] returns: a running `podman exec` whose
/// stdout/stderr stream into a log file. `finished` polls (never blocks), `withdrawn` waits,
/// `kill` ends the exec the way the proven compile recipe does (`kill <podman-exec pid>`: the
/// SIGKILLed teardown `compile_map.sh` observed as exit 137).
#[derive(Debug)]
pub struct ExecHandle {
    child: tokio::process::Child,
    log: PathBuf,
}

impl ExecHandle {
    /// The log file output is appended to.
    pub fn log(&self) -> &Path {
        &self.log
    }

    /// `None` while podman is streaming, `Some` once podman itself exited (NOT when the process
    /// inside the container is done — the reason this handle exists). `Err` when the child state
    /// could not be read (`try_wait` only fails on a broken child channel, which is a process
    /// death): the caller must treat this as an exec failure, never as "still running" — an
    /// `Err` answered until the deadline would otherwise leave a dead exec watching a log only
    /// luck could rescue (the exec-kill/task-loss case stays the caller's to close out).
    pub fn finished(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// SIGKILL the exec (the container keeps running; a compile that printed its `OK:` line but
    /// lingers is stopped this way, like `compile_map.sh` does). No-op when it already exited.
    pub fn kill(&mut self) {
        let _ = self.child.start_kill();
    }

    /// Wait for the exec process to end (already killed, or died on its own).
    pub async fn withdrawn(&mut self) {
        let _ = self.child.wait().await;
    }
}

pub struct DockerRuntime {
    pub project_root: PathBuf,
    /// podman binary (default `podman`; tests point it at a fake script)
    pub podman: PathBuf,
    pub image: String,
    pub max_sessions: usize,
    pub linux_game_dir: String,
    pub linux_client_script: PathBuf,
    pub host_display: String,
    pub xauthority: Option<PathBuf>,
    /// valid capture layer directory (see [`DockerOptions::vk_layer_dir`])
    pub vk_layer_dir: Option<PathBuf>,
    /// s2sdk probe directory (see [`DockerOptions::s2sdk_probe_dir`])
    pub s2sdk_probe_dir: Option<PathBuf>,
    /// `bridge/install-rs.sh` (see [`DockerOptions::install_script`])
    pub install_script: Option<PathBuf>,
    install_timeout: Duration,
    short_timeout: Duration,
    run_timeout: Duration,
    owned: Mutex<Vec<Owned>>,
    /// last `list_sessions` answer and when it was taken
    session_cache: Mutex<Option<(Instant, Vec<SessionEntry>)>>,
    /// serialises the whole launch path inside this process
    launch_gate: AsyncMutex<()>,
    /// when `stop` was last issued per container (decides whether a `stopping` one is re-stopped)
    stop_issued: Mutex<HashMap<String, Instant>>,
}

// ---------------------------------------------------------------------------------------------
// Pure argv builders (golden-tested). They return podman arguments WITHOUT the binary.
// ---------------------------------------------------------------------------------------------

/// Inputs of [`run_argv`] that are host paths or environment.
#[derive(Debug, Clone)]
pub struct RunPlan<'a> {
    pub name: &'a str,
    pub image: &'a str,
    /// LinuxClient: the session artifacts dir (mounted at /work; console.log is mirrored there)
    pub out_dir: &'a Path,
    /// Linux kinds: host path of the kind's script (linux-client.sh or linux-dedicated.sh), mounted
    /// read-only at its /cs2mcp/ path; `lib-preamble.sh` is mounted from the same directory
    pub linux_script: &'a Path,
    /// LinuxClient: game dir inside the home volume (`CS2_GAME_DIR`)
    pub linux_game_dir: &'a str,
    /// WineTools: `<project>/workshop/run/app` (mounted at /app)
    pub app_dir: &'a Path,
    /// WineTools: `<project>/workshop/run/cache` (mounted at /cache)
    pub cache_dir: &'a Path,
    /// WineTools: host X cookie file, mounted read-only at /root/.Xauthority
    pub xauthority: Option<&'a Path>,
    /// WineTools: DISPLAY passed into the container (host display)
    pub display: &'a str,
    /// Host directory of a valid capture layer (`libcs2_vk_capture.so` + manifest). `Some` mounts
    /// it read-only and enables the layer with `CS2MCP_CAPTURE_DIR=/work/capture`; the capture dir
    /// itself is `<out_dir>/capture`. `None`: no layer (the opt-out, or no valid layer dir).
    pub capture_layer: Option<&'a Path>,
    /// Host directory with `libs2sdk_probe.so`. `Some` mounts it read-only and preloads the probe
    /// (Linux sessions only); `None`: no probe. Consumed by [`linux_argv`] only.
    pub s2sdk_probe: Option<&'a Path>,
}

/// The capture-layer arguments: layer mount and the explicit-layer environment. Pure.
pub fn capture_argv(layer_dir: &Path) -> Vec<String> {
    let mut a = Vec::new();
    push(
        &mut a,
        &[
            "-v",
            &format!("{}:{CONTAINER_LAYER_DIR}:ro", layer_dir.display()),
            "-e",
            &format!("VK_ADD_LAYER_PATH={CONTAINER_LAYER_DIR}"),
            "-e",
            &format!("VK_INSTANCE_LAYERS={LAYER_NAME}"),
            "-e",
            &format!("CS2MCP_CAPTURE_DIR={CONTAINER_CAPTURE_DIR}"),
        ],
    );
    a
}

/// The s2sdk-probe arguments: probe dir mount and the preload/report environment. Pure. Consumed
/// by [`linux_argv`] only: the probe targets the native Linux client and dedicated server, not
/// Wine (the Wine side keeps the C bridge).
pub fn s2sdk_probe_argv(probe_dir: &Path) -> Vec<String> {
    let mut a = Vec::new();
    push(
        &mut a,
        &[
            "-v",
            &format!("{}:{CONTAINER_S2SDK_DIR}:ro", probe_dir.display()),
            "-e",
            &format!("CS2MCP_PRELOAD={CONTAINER_S2SDK_DIR}/{PROBE_SO_NAME}"),
            "-e",
            &format!("S2SDK_PROBE_OUT={CONTAINER_PROBE_OUT}"),
        ],
    );
    a
}

/// `podman run` arguments for a session. Pure: no I/O. Containers are NOT started with `--rm`.
pub fn run_argv(spec: &LaunchSpec, plan: &RunPlan) -> Vec<String> {
    let mut a: Vec<String> = vec!["run".into(), "-d".into()];
    match &spec.kind {
        SessionKind::LinuxClient => {
            if spec.gpus {
                push(&mut a, &["--gpus", "all"]);
            }
            linux_argv(&mut a, plan, LINUX_SCRIPT_DEST, true);
            a.extend(spec.args.iter().cloned());
        }
        SessionKind::LinuxDedicated => {
            // no GPU, no shader cache: the server needs neither
            linux_argv(&mut a, plan, LINUX_DEDICATED_SCRIPT_DEST, false);
            a.extend(spec.args.iter().cloned());
        }
        SessionKind::WineTools { addon } => {
            // run_vtest.sh:65 order: -d --gpus all --name --network=host
            if spec.gpus {
                push(&mut a, &["--gpus", "all"]);
            }
            wine_container_argv(&mut a, plan);
            a.push(wine_command(addon.as_deref(), spec));
        }
        SessionKind::WineClient => {
            if spec.gpus {
                push(&mut a, &["--gpus", "all"]);
            }
            wine_container_argv(&mut a, plan);
            a.push(wine_client_command(spec));
        }
    }
    a
}

/// The shared Wine container shape up to the shell command (both Wine kinds mount the same app
/// tree and diff only in the game command line): `--name --network=host`, the app/cache/X mounts,
/// the optional capture layer, the display env, then `--entrypoint /bin/bash IMAGE -c`. Pure.
fn wine_container_argv(a: &mut Vec<String>, plan: &RunPlan) {
    push(a, &["--name", plan.name, "--network=host"]);
    push(
        a,
        &[
            "-v",
            &format!("{}:{WINE_APP_DEST}", plan.app_dir.display()),
            "-v",
            &format!("{}:/cache", plan.cache_dir.display()),
            "-v",
            "/tmp/.X11-unix:/tmp/.X11-unix",
        ],
    );
    if let Some(xa) = plan.xauthority {
        push(
            a,
            &["-v", &format!("{}:/root/.Xauthority:ro", xa.display())],
        );
    }
    // the Wine path is UNVERIFIED live (crates/cs2-vk-capture/README.md, "Proton and
    // pressure-vessel"): the layer must be loaded by the Linux process hosting Wine, which
    // inherits these container variables through entrypoint.sh. Wine sessions have no
    // /work mount otherwise, so the artifacts dir is mounted only when capturing.
    if let Some(layer) = plan.capture_layer {
        push(
            a,
            &["-v", &format!("{}:{LINUX_WORK_DEST}", plan.out_dir.display())],
        );
        a.extend(capture_argv(layer));
    }
    push(a, &["-e", &format!("DISPLAY={}", plan.display)]);
    if plan.xauthority.is_some() {
        push(a, &["-e", "XAUTHORITY=/root/.Xauthority"]);
    }
    push(a, &["--entrypoint", "/bin/bash", plan.image, "-c"]);
}

/// The shared Linux container shape (`run -d --name --network=host -v ... --entrypoint bash IMAGE
/// <script>`). `shadercache` is mounted for the client only: a server never draws.
fn linux_argv(a: &mut Vec<String>, plan: &RunPlan, script_dest: &str, shadercache: bool) {
    push(a, &["--name", plan.name, "--network=host"]);
    let mut mounts = vec![
        "-v".to_string(),
        format!("{LINUX_HOME_VOLUME}:{LINUX_HOME_DEST}"),
    ];
    if shadercache {
        mounts.extend(["-v".to_string(), "cs2-shadercache:/cache".to_string()]);
    }
    mounts.extend([
        "-v".to_string(),
        format!("{}:{LINUX_WORK_DEST}", plan.out_dir.display()),
        "-v".to_string(),
        format!("{}:{script_dest}:ro", plan.linux_script.display()),
        "-v".to_string(),
        format!(
            "{}:{LINUX_LIB_DEST}:ro",
            sibling_script(plan.linux_script, LINUX_LIB_NAME).display()
        ),
    ]);
    // the artifacts dir is already /work above, so only the layer is added here
    if let Some(layer) = plan.capture_layer {
        mounts.extend(capture_argv(layer));
    }
    // the s2sdk probe mount + preload/report env (Linux sessions only)
    if let Some(probe) = plan.s2sdk_probe {
        mounts.extend(s2sdk_probe_argv(probe));
    }
    mounts.extend([
        "-e".to_string(),
        format!("CS2_GAME_DIR={}", plan.linux_game_dir),
        "-u".to_string(),
        "0".to_string(),
        "--entrypoint".to_string(),
        "bash".to_string(),
        plan.image.to_string(),
        script_dest.to_string(),
    ]);
    a.extend(mounts);
}

/// `name` in the same directory as `script` (the scripts and `lib-preamble.sh` ship together).
pub fn sibling_script(script: &Path, name: &str) -> PathBuf {
    script.with_file_name(name)
}

/// `bridge/install-rs.sh` derived from the Linux script's checkout:
/// `<repo>/client-mcp/docker/linux-client.sh` -> `<repo>/bridge/install-rs.sh`. `None` when the
/// path has fewer ancestors (an unset/relative script), which `launch` reports by name.
pub fn install_script_for(linux_script: &Path) -> Option<PathBuf> {
    let repo = linux_script.parent()?.parent()?.parent()?;
    Some(repo.join("bridge/install-rs.sh"))
}

/// The `install-rs.sh` arguments that make a kind's tree current. Every kind this runtime launches
/// has one: Wine (tools and client) and the Linux kinds (client and dedicated share `cs2-home`)
/// each get their platform's ensure. A launch must not start on a tree that was not prepared. Pure.
pub fn ensure_argv(kind: &SessionKind) -> Vec<String> {
    let platform = match kind {
        SessionKind::WineTools { .. } | SessionKind::WineClient => "wine",
        SessionKind::LinuxClient | SessionKind::LinuxDedicated => "linux",
    };
    vec!["--platform".into(), platform.into(), "ensure".into()]
}

/// Join the concurrent stderr reader of an ensure child, bounded: a straggler that still holds the
/// pipe open (or a reader task that was cancelled) must not wedge the launch. Returns the text and
/// whether the task actually finished (`false`: a member still holds the pipe; the caller kills
/// the group and aborts the task).
async fn collect_stderr(task: &mut tokio::task::JoinHandle<Vec<u8>>) -> (String, bool) {
    match tokio::time::timeout(INSTALL_KILL_GRACE, task).await {
        Ok(Ok(buf)) => (String::from_utf8_lossy(&buf).into_owned(), true),
        Ok(Err(_)) => (String::new(), true),
        Err(_) => (String::new(), false),
    }
}

/// A `bash install-rs.sh` child in its own process group (see [`DockerRuntime::ensure_install`]).
///
/// The child's `kill_on_drop` only signals the script itself. An ensure that repairs a tree spawns
/// writers of its own (`podman`/`rsync`/`cp`), so a cancelled launch would otherwise leave them
/// running after the lock is gone. `Drop` therefore SIGKILLs the whole group when the ensure future
/// is dropped mid-flight; a cancelled launch may leave a partial install behind, which the next
/// ensure's hash check repairs.
#[cfg(unix)]
struct InstallGroup {
    /// The child's pid, which is also its process group id (`process_group(0)`).
    pgid: Option<libc::pid_t>,
}

/// What [`InstallGroup::wait_unreaped`][wait] found out about the child.
///
/// [wait]: InstallGroup::wait_unreaped
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnreapedWait {
    /// The child exited and is now an unreaped zombie (`WNOWAIT`): its pid — and so its process
    /// group id — cannot have been reused yet; the group may still be signalled.
    ExitedUnreaped,
    /// Not our child anymore (`waitid` `ECHILD`): it was already reaped elsewhere, its pid may have
    /// been reused by another process, and the group must NOT be signalled.
    GoneReaped,
    /// Still running (or the state is unknown) at the deadline.
    Timeout,
}

#[cfg(unix)]
impl InstallGroup {
    fn new(pid: Option<u32>) -> Self {
        Self {
            pgid: pid.map(|p| p as libc::pid_t),
        }
    }

    fn signal(&self, sig: libc::c_int) {
        if let Some(pgid) = self.pgid {
            // ESRCH (the group is already gone) is the state we wanted; nothing to report
            unsafe {
                libc::killpg(pgid, sig);
            }
        }
    }

    /// Wait without reaping until the child has exited or the deadline passes.
    /// [`UnreapedWait::ExitedUnreaped`]: the child is an unreaped zombie now (its pid — and so its
    /// process group id — cannot be reused yet). [`UnreapedWait::GoneReaped`]: the child was
    /// already reaped elsewhere; its pid may be reused, so the caller must not signal the group.
    /// Runs on a blocking thread: `waitid` has no async form, and the poll is bounded.
    fn wait_unreaped(pid: libc::pid_t, deadline: std::time::Instant) -> UnreapedWait {
        loop {
            // With WNOHANG, waitid returns 0 and leaves si_pid == 0 when no child has changed
            // state (the struct must be zeroed first so that is detectable); WNOWAIT leaves the
            // exited child a zombie.
            let mut si: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut si,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            if rc == 0 && unsafe { si.si_pid() } != 0 {
                return UnreapedWait::ExitedUnreaped;
            }
            let errno = std::io::Error::last_os_error().raw_os_error();
            if rc < 0 {
                if errno == Some(libc::EINTR) {
                    continue;
                }
                // ECHILD is the only errno whose man-page meaning is "no child to wait for": the
                // child was already reaped elsewhere (its pid may be reused). Anything else is not
                // that (EINVAL, ...): keep polling to the deadline.
                if errno == Some(libc::ECHILD) {
                    return UnreapedWait::GoneReaped;
                }
            }
            if std::time::Instant::now() >= deadline {
                return UnreapedWait::Timeout;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// SIGTERM the group, wait up to `grace` for the leader to exit WITHOUT reaping it (an unreaped
    /// zombie keeps its pid, so the group id below cannot have been reused), then SIGKILL the whole
    /// group: a member that handles or ignores SIGTERM would survive its leader and keep writing
    /// after the launch lock is released. Finally reap the leader.
    ///
    /// The one exception is [`UnreapedWait::GoneReaped`]: the leader was already reaped elsewhere,
    /// so its pid — and with it the group id — may have been reused by an unrelated process. The
    /// group is then NOT signalled (nothing is left to clean up; a member that outlived the leader
    /// this way is the price of never killpg-ing a reused pid).
    async fn terminate(&mut self, child: &mut tokio::process::Child, grace: Duration) {
        self.signal(libc::SIGTERM);
        let outcome = if let Some(pid) = child.id() {
            let pid = pid as libc::pid_t;
            tokio::task::spawn_blocking(move || {
                Self::wait_unreaped(pid, std::time::Instant::now() + grace)
            })
            .await
            .unwrap_or(UnreapedWait::Timeout)
        } else {
            // no pid left on the handle: the child was already reaped (by tokio)
            UnreapedWait::GoneReaped
        };
        if outcome != UnreapedWait::GoneReaped {
            self.signal(libc::SIGKILL);
        }
        let _ = child.wait().await;
        self.disarm();
    }

    /// The child has been reaped: nothing is left to clean up.
    fn disarm(&mut self) {
        self.pgid = None;
    }
}

#[cfg(unix)]
impl Drop for InstallGroup {
    fn drop(&mut self) {
        // only while armed: a reaped child's pid may already belong to another group
        if self.pgid.is_some() {
            self.signal(libc::SIGKILL);
        }
    }
}

#[cfg(unix)]
impl InstallGroup {
    /// SIGKILL the whole group (a member still holds the stderr pipe).
    fn kill_group(&self) {
        self.signal(libc::SIGKILL);
    }
}

/// Non-Unix stand-in: no process groups, so only the script itself is ended (`kill_on_drop`
/// covers a dropped launch). The podman runtime is a Linux backend; this keeps the servers
/// building for Windows, where the session is the native launcher's (`SessionKind::NativeWindows`).
#[cfg(not(unix))]
struct InstallGroup;

#[cfg(not(unix))]
impl InstallGroup {
    fn new(_pid: Option<u32>) -> Self {
        Self
    }
    async fn terminate(&mut self, child: &mut tokio::process::Child, _grace: Duration) {
        let _ = child.kill().await;
    }
    fn kill_group(&self) {}
    fn disarm(&mut self) {}
}

/// The last `lines` lines of a command's stderr (`install-rs.sh` names what is missing/stale or
/// which containers hold the tree). Pure.
pub fn stderr_tail(text: &str, lines: usize) -> String {
    let trimmed = text.trim();
    let all: Vec<&str> = trimmed.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// Game arguments of a dedicated server (workflows/05 step 1), in this order:
/// `-dedicated -insecure -console -usercon +sv_lan 1 +game_type 0 +game_mode 0 +map <map>`,
/// `+rcon_password <pw>` when given, then the caller's extra args (appended last, so they win).
pub fn server_game_args(map: &str, rcon_password: Option<&str>, extra: &[String]) -> Vec<String> {
    let mut a = s(&[
        "-dedicated",
        "-insecure",
        "-console",
        "-usercon",
        "+sv_lan",
        "1",
        "+game_type",
        "0",
        "+game_mode",
        "0",
        "+map",
    ]);
    a.push(map.to_string());
    if let Some(pw) = rcon_password {
        a.push("+rcon_password".into());
        a.push(pw.into());
    }
    a.extend(extra.iter().cloned());
    a
}

/// Engine flags that keep a tools session running at full speed while another window has focus
/// (the session shows on the user's desktop, and the user keeps working there).
///
/// * `+r_always_render_all_windows 1`: in `-tools` mode `CRenderService::ShouldRenderFrame`
///   (engine2.dll!0x1e8130 / libengine2.so!0x43f320, build 2000930) renders the game views only when
///   the game window has mouse focus (`IInputSystem` slot 68) or `CRenderService::ShouldRender`
///   (engine2.dll!0x1e82d0) is true, and that convar is its first test. Without it, a session whose
///   game window is not focused presents nothing (0 presents/s measured live; 118/s with it), so
///   frame captures and anything waiting on a rendered frame time out, and
///   `CToolService::ThrottleRenderingToYieldCPU` (engine2.dll!0x1fb9b0) also waits 10 ms per frame
///   once the app is inactive.
/// * `+engine_no_focus_sleep 0`: the per-frame sleep of `CEngineServiceMgr::_MainLoop` while the app
///   is inactive (default 20 ms). It never stopped rendering; a minimized window still sleeps
///   `max(100, value)` ms.
pub const FOCUS_ARGS: [&str; 4] = [
    "+engine_no_focus_sleep",
    "0",
    "+r_always_render_all_windows",
    "1",
];

/// Shell line run inside the Wine tools container (`bash -c`). Matches run_vtest.sh:62 and
/// runtime.sh:120; the testscript flag and user args go between `-insecure` and the engine flag.
/// The program is the launcher (`cs2mcp_launch.exe`), NOT `cs2.exe`: it starts the game suspended,
/// injects the Metamod loader and waits (see [`stop_argv`]); the arguments after it are cs2's.
pub fn wine_command(addon: Option<&str>, spec: &LaunchSpec) -> String {
    let mut parts: Vec<String> = [
        "./entrypoint.sh",
        "cs2mcp_launch.exe",
        "-tools",
        "-vulkan",
        "-console",
        "-condebug",
        "-insecure",
    ]
    .map(String::from)
    .to_vec();
    if let Some(addon) = addon {
        parts.push("-addon".into());
        parts.push(sh_quote(addon));
    }
    if let Some(ts) = &spec.testscript {
        parts.push("-testscript".into());
        parts.push(format!("/app/vtest/{ts}.txt"));
    }
    parts.extend(spec.args.iter().map(|a| sh_quote(a)));
    parts.extend(FOCUS_ARGS.map(String::from));
    format!("cd /app; exec {}", parts.join(" "))
}

/// Shell line run inside the Wine **client** container (`bash -c`): the retail-like launch, i.e.
/// `wine_command` without `-tools`, without `-addon` and without `-testscript` (testscript runs
/// stay wine-tools only; `validate` refuses them, and the client kind carries no addon). Keeps
/// `-insecure` (Metamod requires `-dedicated` or `-insecure`; VAC off, no official servers),
/// `-vulkan -console -condebug` (console.log is what the readiness ladder scans) and
/// [`FOCUS_ARGS`]. The program is the same launcher as the tools line
/// ([`wine_command`]): `cs2mcp_launch.exe` starts `cs2.exe` and injects the Metamod loader. Pure.
pub fn wine_client_command(spec: &LaunchSpec) -> String {
    let mut parts: Vec<String> = [
        "./entrypoint.sh",
        "cs2mcp_launch.exe",
        "-vulkan",
        "-console",
        "-condebug",
        "-insecure",
    ]
    .map(String::from)
    .to_vec();
    parts.extend(spec.args.iter().map(|a| sh_quote(a)));
    parts.extend(FOCUS_ARGS.map(String::from));
    format!("cd /app; exec {}", parts.join(" "))
}

/// `podman ps` listing running containers (the cap check and readiness both use it).
pub fn ps_argv() -> Vec<String> {
    s(&["ps", "--filter", "name=cs2", "--format", "json"])
}

/// `podman rm <name>` (no `-f`: podman refuses a running container, so a removal never kills one).
pub fn rm_argv(name: &str) -> Vec<String> {
    vec!["rm".into(), name.into()]
}

/// `podman stop -t 0 <name>`: PID 1 is the launcher (`cs2mcp_launch.exe`), which waits for
/// `cs2.exe`: a zero timeout SIGKILLs PID 1 and the container's PID namespace takes `cs2.exe`
/// with it. There is no graceful teardown path to wait for.
pub fn stop_argv(name: &str) -> Vec<String> {
    vec!["stop".into(), "-t".into(), "0".into(), name.into()]
}

/// `podman wait --condition=exited <name>`: read-only, prints the exit code. Safe to time out.
pub fn wait_argv(name: &str) -> Vec<String> {
    vec!["wait".into(), "--condition=exited".into(), name.into()]
}

/// The argv `vtmsend.exe` runs with inside the session (runtime.sh `vtmsend`). Published so a
/// caller can post a payload through [`DockerRuntime::exec_in`] on a container this runtime did
/// **not** launch (the tools session belongs to the orchestrator), the way the file-route compile
/// tools exec `resourcecompiler.exe` there.
pub fn vtmsend_program_argv() -> Vec<String> {
    s(&["bash", "-c", "cd /app && ./entrypoint.sh vtmsend.exe 2>&1"])
}

/// `vtmsend.exe` inside the session, as runtime.sh `vtmsend` runs it (behind `podman exec -i`).
pub fn vtmsend_argv(name: &str) -> Vec<String> {
    exec_argv(name, &vtmsend_program_argv())
}

/// A vtmsend payload: the kv3 header and the body in braces (runtime.sh `put_payload`).
pub fn vtm_payload(body: &str) -> String {
    format!(
        "<!-- kv3 encoding:text:version{{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}} format:generic:version{{7412167c-06e9-4698-aff2-e63eb59037e7}} -->\n{{\n{body}\n}}\n"
    )
}

/// A VToolMessage `command` payload: one console command the engine runs
/// (`idabins/worknotes/vtmsend/`; `runtime.sh` `put_payload` + `vtmsend`, the `vcmd.sh` recipe).
/// This is the route a tool takes to put a line into the engine console while the bridge's
/// `console.exec` is gated on `-testscript` or `-tools` (it answers `-32011` in any other session).
///
/// `command` is escaped for the kv3 string (`\` and `"`), the same two characters the recipe
/// escapes; a command that needs anything else in the payload should be refused by the caller
/// before it gets here (a console line is not a place for quoting games).
pub fn vtm_command_payload(command: &str) -> String {
    let escaped = command.replace('\\', "\\\\").replace('"', "\\\"");
    vtm_payload(&format!("\t_message = \"command\"\n\t_data = \"{escaped}\""))
}

/// The channel probe: a `command` that echoes a marker (runtime.sh `wait_channel`).
pub fn channel_probe_payload() -> String {
    vtm_command_payload("echo mcp_runtime_channel_probe")
}

/// The bridge load: a `tool_request` for `cs2_bridge` (runtime.sh `load_rs_bridge`).
pub fn bridge_request_payload() -> String {
    vtm_payload("\t_message = \"tool_request\"\n\ttool_name = \"cs2_bridge\"\n\t_data = -1")
}

/// Write the payload into the app dir through a temp file and a rename, so vtmsend never reads a
/// half-written file.
///
/// The payload path is **fixed** (`vtmsend.exe` opens `Z:\app\vtm_payload.kv3`; the program takes
/// no path argument, `idabins/worknotes/vtmsend/vtmsend.c`), so two posters that each
/// write-then-exec cannot be told apart by the engine: a caller that posts through
/// [`vtmsend_program_argv`] must serialize the write + exec pair against every other poster
/// (`hammer-mcp`'s `run_map` holds `HammerState::vtm_gate` across its whole post sequence,
/// review-1 M7). This function only makes one *write* atomic; it cannot order two writers.
pub fn write_payload_atomic(app_dir: &Path, payload: &str) -> std::io::Result<()> {
    // unique per writer: two runtimes or two waits must never share a temp file
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = app_dir.join(format!(
        "{VTM_PAYLOAD_NAME}.{}.{seq}.tmp",
        std::process::id()
    ));
    std::fs::write(&tmp, payload)?;
    std::fs::rename(&tmp, app_dir.join(VTM_PAYLOAD_NAME))
}

/// Is the discovery file present and written at or after the launch (within [`FRESH_SLACK`])? A
/// file from an earlier bridge is removed at launch, so this is a second guard, not the signal.
pub fn discovery_fresh(path: &Path, launched_at: SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t + FRESH_SLACK >= launched_at)
        .unwrap_or(false)
}

/// `podman exec -i <name> <argv...>` (run_vtest/runtime.sh use `exec -i` for helpers).
pub fn exec_argv(name: &str, argv: &[String]) -> Vec<String> {
    let mut a = vec!["exec".into(), "-i".into(), name.into()];
    a.extend(argv.iter().cloned());
    a
}

/// `podman ps --filter label=<label> --format json`: the running containers carrying `label`
/// (the ModelDoc instance is found by `cs2mcp.tool=modeldoc`).
pub fn ps_label_argv(label: &str) -> Vec<String> {
    s(&[
        "ps",
        "--filter",
        &format!("label={label}"),
        "--format",
        "json",
    ])
}

/// [`exec_argv`] for a container this runtime did not launch (`exec_in`): identical shape.
pub fn exec_in_argv(name: &str, argv: &[String]) -> Vec<String> {
    exec_argv(name, argv)
}

/// A podman-plausible container name for `exec_in`: non-empty, no whitespace or control
/// characters, no leading separator. NOT [`valid_name`]: that one is for `cs2mcp-*` session
/// containers this runtime manages, while `exec_in` targets foreign containers too (the ModelDoc
/// instance is named `cs2tools`, bridge/runtime.sh `CONTAINER`).
pub fn valid_container_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with(['-', '.'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

pub fn logs_argv(name: &str, lines: usize) -> Vec<String> {
    vec![
        "logs".into(),
        "--tail".into(),
        lines.to_string(),
        name.into(),
    ]
}

/// `podman inspect` for the state word and exit code (both are needed for exit reporting).
pub fn inspect_state_argv(name: &str) -> Vec<String> {
    vec![
        "inspect".into(),
        "-f".into(),
        "{{.State.Status}} {{.State.ExitCode}}".into(),
        name.into(),
    ]
}

/// `podman inspect` for the addon a tools container was started with: the `cs2mcp.addon` label
/// (`bridge/runtime.sh up`), then the launch command (`-addon <name>`), separated by `|`.
pub fn inspect_addon_argv(name: &str) -> Vec<String> {
    vec![
        "inspect".into(),
        "-f".into(),
        "{{index .Config.Labels \"cs2mcp.addon\"}}|{{json .Config.Cmd}}".into(),
        name.into(),
    ]
}

/// The addon out of [`inspect_addon_argv`]'s output: the label when set, else the word after
/// `-addon` in the launch command; `None` when the container was started without one.
pub fn parse_container_addon(text: &str) -> Option<String> {
    let valid = |a: &str| {
        !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    };
    let (label, cmd) = text.trim().split_once('|').unwrap_or(("", text.trim()));
    let label = label.trim();
    if valid(label) && label != "<no value>" {
        return Some(label.to_string());
    }
    let at = cmd.find("-addon")?;
    let word: String = cmd[at + "-addon".len()..]
        .trim_start_matches([' ', '\t'])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .collect();
    valid(&word).then_some(word)
}

/// `podman inspect` for the mounts (`Type`, `Name`, `Source`, `Destination`).
pub fn inspect_mounts_argv(name: &str) -> Vec<String> {
    vec![
        "inspect".into(),
        "-f".into(),
        "{{json .Mounts}}".into(),
        name.into(),
    ]
}

/// POSIX single-quote an argument unless it is made only of safe characters.
pub fn sh_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=+,@%".contains(c));
    if safe {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// `^cs2mcp-[a-z0-9][a-z0-9-]*$`, checked by hand (no regex dependency here).
pub fn valid_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(NAME_PREFIX) else {
        return false;
    };
    let mut chars = rest.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Suite names are `[A-Za-z0-9_-]+` (they become a file name under `<app>/vtest/`).
pub fn valid_suite_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Container name for a kind: `cs2mcp-<kind>-<8 hex>`.
pub fn generated_name(kind: &SessionKind) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    h.write_u64(SEQ.fetch_add(1, Ordering::Relaxed));
    format!("{NAME_PREFIX}{}-{:08x}", kind_slug(kind), h.finish() as u32)
}

pub fn kind_slug(kind: &SessionKind) -> &'static str {
    match kind {
        SessionKind::WineTools { .. } => "wine-tools",
        SessionKind::WineClient => "wine-client",
        SessionKind::LinuxClient => "linux-client",
        SessionKind::LinuxDedicated => "linux-dedicated",
    }
}

/// Identity of a request: two launches with the same key are the same session (idempotent launch).
pub fn spec_key(spec: &LaunchSpec) -> String {
    format!(
        "{:?}|gpus={}|headless={}|testscript={:?}|map={:?}|args={:?}|capture={}",
        spec.kind, spec.gpus, spec.headless, spec.testscript, spec.map, spec.args, spec.capture
    )
}

/// `podman ps -a --format json` (every state, not only running).
pub fn ps_all_argv() -> Vec<String> {
    s(&["ps", "-a", "--format", "json"])
}

/// Parse `podman ps --format json` into `(name, state)` pairs: names as an array (podman) or a
/// string (docker); a missing `State` counts as `running`.
pub fn parse_ps_entries(json: &str) -> anyhow::Result<Vec<(String, String)>> {
    let json = json.trim();
    if json.is_empty() {
        return Ok(Vec::new());
    }
    let v: Value = serde_json::from_str(json)?;
    let mut out = Vec::new();
    for e in v.as_array().map(Vec::as_slice).unwrap_or_default() {
        let state = e
            .get("State")
            .and_then(Value::as_str)
            .unwrap_or("running")
            .to_string();
        let names: Vec<String> = match e.get("Names") {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect(),
            Some(Value::String(s)) => vec![s.clone()],
            _ => Vec::new(),
        };
        out.extend(names.into_iter().map(|n| (n, state.clone())));
    }
    Ok(out)
}

/// Names of the running containers in `podman ps --format json` output.
pub fn parse_ps(json: &str) -> anyhow::Result<Vec<String>> {
    Ok(parse_ps_entries(json)?
        .into_iter()
        .filter(|(_, state)| state == "running")
        .map(|(name, _)| name)
        .collect())
}

/// Which kind a `cs2mcp-<kind>-<hex>` name carries (`unknown` for anything else).
pub fn kind_of_name(name: &str) -> &'static str {
    let rest = name.strip_prefix(NAME_PREFIX).unwrap_or(name);
    for slug in [
        "wine-tools",
        "linux-client",
        "wine-client",
        "linux-dedicated",
    ] {
        if rest.strip_prefix(slug).is_some_and(|r| r.starts_with('-')) {
            return slug;
        }
    }
    "unknown"
}

/// One `cs2mcp-*` container: launched by this runtime (`owned`) or discovered through podman.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub name: String,
    /// `wine-tools`, `linux-client`, ... or `unknown` for a name this server did not generate
    pub kind: String,
    /// podman's state word: `running`, `exited`, `created`, ...
    pub state: String,
    pub owned: bool,
    /// host dir mounted at /work (Linux sessions; Wine sessions only when they capture); `None`
    /// for volumes and unknown mounts
    pub artifacts_dir: Option<PathBuf>,
    /// host path of the session's console.log (`<artifacts>/console.log`, or
    /// `<app>/game/csgo/console.log` for Wine sessions)
    pub console_log: Option<PathBuf>,
    /// host game tree of a Wine session (the bridge discovery file lives under it)
    pub app_dir: Option<PathBuf>,
    /// why this entry has no host paths (inspect failed, a volume mount, ambiguous mounts)
    pub note: Option<String>,
    /// the map the harness loads (owned sessions launched with `map`; `None` otherwise)
    pub map: Option<String>,
    /// the addon passed with `-addon` (owned sessions only)
    pub addon: Option<String>,
}

impl SessionEntry {
    pub fn running(&self) -> bool {
        self.state == "running"
    }

    /// A container listed by `ps -a` whose mounts were not read (stopped, or not inspected).
    fn bare(name: String, state: String, owned: bool) -> Self {
        SessionEntry {
            kind: kind_of_name(&name).to_string(),
            name,
            state,
            owned,
            artifacts_dir: None,
            console_log: None,
            app_dir: None,
            note: None,
            map: None,
            addon: None,
        }
    }

    /// Owned by this runtime: the paths it chose at launch.
    fn from_owned(o: &Owned, state: String) -> Self {
        let wine = matches!(
            o.spec.kind,
            SessionKind::WineTools { .. } | SessionKind::WineClient
        );
        SessionEntry {
            name: o.name.clone(),
            kind: kind_slug(&o.spec.kind).to_string(),
            state,
            owned: true,
            artifacts_dir: Some(o.artifacts_dir.clone()),
            console_log: o.paths.console_log.clone(),
            app_dir: wine.then(|| o.paths.app_root.clone()),
            note: None,
            map: o.spec.map.clone(),
            addon: match &o.spec.kind {
                SessionKind::WineTools { addon } => addon.clone(),
                _ => None,
            },
        }
    }

    /// Discovered: the host paths come from the bind mounts. A volume mount has no host path, so
    /// it yields no console.log. When both `/app` and `/work` are bound, the name's kind decides.
    fn from_mounts(name: String, state: String, mounts: &[Mount]) -> Self {
        enum Side {
            Wine(PathBuf),
            Linux(PathBuf),
        }
        let mut e = Self::bare(name, state, false);
        let bind = |dest: &str| {
            mounts
                .iter()
                .find(|m| m.destination == dest && m.kind == "bind")
                .map(|m| PathBuf::from(&m.source))
        };
        let volume = |dest: &str| {
            mounts
                .iter()
                .any(|m| m.destination == dest && m.kind != "bind")
        };
        let side = match (bind(WINE_APP_DEST), bind(LINUX_WORK_DEST)) {
            (Some(app), Some(work)) => match e.kind.as_str() {
                "wine-tools" | "wine-client" => Some(Side::Wine(app)),
                "linux-client" => Some(Side::Linux(work)),
                _ => {
                    e.note = Some(
                        "both /app and /work are bind-mounted and the name does not say which kind"
                            .into(),
                    );
                    None
                }
            },
            (Some(app), None) => Some(Side::Wine(app)),
            (None, Some(work)) => Some(Side::Linux(work)),
            (None, None) => {
                if volume(WINE_APP_DEST) || volume(LINUX_WORK_DEST) {
                    e.note = Some(
                        "the session mount is a volume, not a host path: no console.log path"
                            .into(),
                    );
                }
                None
            }
        };
        match side {
            Some(Side::Wine(app)) => {
                e.console_log = Some(app.join("game/csgo/console.log"));
                e.app_dir = Some(app);
                // a Wine session has /work only when it captures: that is where its frames go
                e.artifacts_dir = bind(LINUX_WORK_DEST);
            }
            Some(Side::Linux(work)) => {
                e.console_log = Some(work.join("console.log"));
                e.artifacts_dir = Some(work);
            }
            None => {}
        }
        e
    }
}

/// One container mount as podman reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub kind: String,
    pub name: String,
    pub source: String,
    pub destination: String,
}

/// Parse `{{json .Mounts}}`; `null` or an empty list is no mounts.
pub fn parse_mounts(json: &str) -> anyhow::Result<Vec<Mount>> {
    let v: Value = serde_json::from_str(json.trim())?;
    let mut out = Vec::new();
    for m in v.as_array().map(Vec::as_slice).unwrap_or_default() {
        let get = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        out.push(Mount {
            kind: get("Type"),
            name: get("Name"),
            source: get("Source"),
            destination: get("Destination"),
        });
    }
    Ok(out)
}

/// Does a mount list hold the resource a session of this kind needs exclusively?
/// WineTools and WineClient: the app dir at /app. LinuxClient / LinuxDedicated: the `cs2-home`
/// volume (the kind filter in `may_conflict` decides which running containers count).
pub fn shared_mount_conflict(
    spec: &LaunchSpec,
    app_dir: &Path,
    mounts: &[Mount],
) -> Option<&'static str> {
    match spec.kind {
        SessionKind::WineTools { .. } | SessionKind::WineClient => mounts
            .iter()
            .any(|m| m.destination == WINE_APP_DEST && m.source == app_dir.display().to_string())
            .then_some("app dir"),
        SessionKind::LinuxClient | SessionKind::LinuxDedicated => mounts
            .iter()
            .any(|m| m.name == LINUX_HOME_VOLUME || m.destination == LINUX_HOME_DEST)
            .then_some("cs2-home volume"),
    }
}

/// Can a running container of kind `other` (from its name) hold the mount `spec` needs? WineTools
/// and everything unknown are checked against every kind. The Linux kinds share the `cs2-home`
/// volume: a client and a dedicated server may run together (the shared preamble is serialised by
/// a flock, see `client-mcp/docker/lib-preamble.sh`), so only the same kind conflicts.
pub fn may_conflict(spec: &LaunchSpec, other: &str) -> bool {
    match spec.kind {
        SessionKind::LinuxClient | SessionKind::LinuxDedicated => {
            other == kind_slug(&spec.kind) || other == "unknown"
        }
        SessionKind::WineTools { .. } | SessionKind::WineClient => true,
    }
}

/// Newest `xauth_*` file in `dir` (run_vtest.sh:44 and runtime.sh:51 use `ls -t | head -1`).
pub fn newest_xauth_in(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("xauth_"))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

/// The cookie file a wine-tools launch uses: the explicit one (even if missing, so the refusal
/// names it), else the newest `/run/user/<uid>/xauth_*`.
pub fn resolve_xauthority(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    #[cfg(unix)]
    {
        let uid = std::fs::metadata("/proc/self").ok()?.uid();
        newest_xauth_in(Path::new(&format!("/run/user/{uid}")))
    }
    #[cfg(not(unix))]
    None
}

/// Is `bin` runnable? A path with a `/` is checked directly, otherwise searched in `PATH`.
pub fn binary_on_path(bin: &Path) -> Option<PathBuf> {
    #[cfg(unix)]
    let is_exe = |p: &Path| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    // Windows: no exec bit; a file named `<bin>` or `<bin>.exe` is runnable
    #[cfg(not(unix))]
    let is_exe = |p: &Path| p.is_file() || p.with_extension("exe").is_file();
    if bin.components().count() > 1 {
        return is_exe(bin).then(|| bin.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| is_exe(p))
}

/// Names of `(name, state)` pairs from [`DockerRuntime::busy_cs2`].
fn busy_names(busy: &[(String, String)]) -> Vec<String> {
    busy.iter().map(|(n, _)| n.clone()).collect()
}

fn push(a: &mut Vec<String>, items: &[&str]) {
    a.extend(items.iter().map(|s| s.to_string()));
}

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// Create `dir` with mode 0700 (the capture dir: requests and results must not be readable by
/// other users; the server and the layer refuse a shared one).
fn make_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().mode(0o700).create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    // Windows: the per-user profile dir is private already
    #[cfg(not(unix))]
    std::fs::create_dir(dir)
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// podman's wording for a missing container (stderr of any verb; `inspect` says "no such object").
fn no_such_text(stderr: &str) -> bool {
    stderr.contains("no such container")
        || stderr.contains("no container with name or ID")
        || stderr.contains("no such object")
}

fn is_no_such(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<RuntimeError>() {
        Some(RuntimeError::Podman { stderr, .. }) => no_such_text(stderr),
        _ => false,
    }
}

/// A podman call that ran out of time (podman itself stalled, e.g. on the container store's disk).
fn is_podman_timeout(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<RuntimeError>(),
        Some(RuntimeError::PodmanTimeout { .. })
    )
}

/// The word `podman inspect` reports, for messages.
fn state_label(state: &ContainerState) -> String {
    match state {
        ContainerState::Running => "running".into(),
        ContainerState::Stopping => "stopping".into(),
        ContainerState::Exited { .. } => "exited".into(),
        ContainerState::Other(s) => s.clone(),
        ContainerState::Gone => "gone".into(),
    }
}

/// Per-call state of the Wine probes in `wait_ready`: pacing and the last failure, for the note.
#[derive(Default)]
struct Probes {
    channel_at: Option<Instant>,
    hello_at: Option<Instant>,
    last_error: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// The runtime
// ---------------------------------------------------------------------------------------------

/// Liveness cache for `wait_ready`: `ps` at most every [`LIVENESS_EVERY`].
#[derive(Default)]
struct Liveness {
    last: Option<Instant>,
    running: bool,
}

/// Incremental scan of console.log for one marker (reads only new bytes).
#[derive(Default)]
struct MarkerScan {
    offset: u64,
    found: bool,
}

impl DockerRuntime {
    pub fn new(opts: DockerOptions) -> Self {
        // logged once, at startup: the script every launch runs before `podman run`
        match opts
            .install_script
            .clone()
            .or_else(|| install_script_for(&opts.linux_client_script))
        {
            Some(script) => {
                tracing::info!(path = %script.display(), "launch ensure: bridge/install-rs.sh")
            }
            None => tracing::warn!(
                linux_script = %opts.linux_client_script.display(),
                "no bridge/install-rs.sh resolved (set install_script); launches that need ensure will be refused"
            ),
        }
        Self {
            project_root: opts.project_root,
            podman: opts.podman,
            image: opts.image,
            max_sessions: opts.max_sessions,
            linux_game_dir: opts.linux_game_dir,
            linux_client_script: opts.linux_client_script,
            host_display: opts.host_display,
            xauthority: opts.xauthority,
            vk_layer_dir: opts.vk_layer_dir,
            s2sdk_probe_dir: opts.s2sdk_probe_dir,
            install_script: opts.install_script,
            install_timeout: opts.install_timeout,
            short_timeout: opts.short_timeout,
            run_timeout: opts.run_timeout,
            owned: Mutex::new(Vec::new()),
            session_cache: Mutex::new(None),
            launch_gate: AsyncMutex::new(()),
            stop_issued: Mutex::new(HashMap::new()),
        }
    }

    pub fn podman_display(&self) -> String {
        self.podman.display().to_string()
    }

    /// Sessions this instance launched and has not stopped, oldest first.
    pub fn owned(&self) -> Vec<Owned> {
        self.owned.lock().expect("owned lock").clone()
    }

    /// The most recently launched session still owned (the `stop_session` default).
    pub fn last_owned(&self) -> Option<Owned> {
        self.owned.lock().expect("owned lock").last().cloned()
    }

    pub fn owned_named(&self, name: &str) -> Option<Owned> {
        self.owned().into_iter().find(|o| o.name == name)
    }

    /// Change one owned session in place (the Wine probes persist their progress this way).
    fn with_owned<R>(&self, name: &str, f: impl FnOnce(&mut Owned) -> R) -> Option<R> {
        let mut all = self.owned.lock().expect("owned lock");
        all.iter_mut().find(|o| o.name == name).map(f)
    }

    /// The runtime's short podman timeout (the stop tool budgets its wait around it).
    pub fn short_timeout(&self) -> Duration {
        self.short_timeout
    }

    /// Host layout of a Wine tools session (`workshop/run/app`, run_vtest.sh:21-23).
    pub fn wine_app_dir(&self) -> PathBuf {
        self.project_root.join("workshop/run/app")
    }

    /// The `install-rs.sh` to run: the configured one, else the `bridge/install-rs.sh` of the
    /// checkout `linux_client_script` belongs to ([`install_script_for`]). A missing script is a
    /// configuration error and fails the launch by path, never a skipped ensure.
    fn resolved_install_script(&self) -> anyhow::Result<PathBuf> {
        let script = match &self.install_script {
            Some(p) => p.clone(),
            None => install_script_for(&self.linux_client_script).ok_or_else(|| {
                RuntimeError::InstallScriptMissing {
                    path: format!(
                        "cannot derive bridge/install-rs.sh from {}",
                        self.linux_client_script.display()
                    ),
                }
            })?,
        };
        if !script.is_file() {
            return Err(RuntimeError::InstallScriptMissing {
                path: script.display().to_string(),
            }
            .into());
        }
        Ok(script)
    }

    /// Run `bridge/install-rs.sh --platform <wine|linux> ensure` for a kind before it launches.
    /// Read-only while the tree is current; only a missing/stale tree is repaired, and that part
    /// refuses (exit 4 -> [`RuntimeError::InstallBusy`]) while a running container mounts it.
    /// Bounded by `install_timeout`; every failure aborts the launch before `podman run`.
    ///
    /// The script runs in its own process group. A timeout SIGTERMs the group, waits
    /// [`INSTALL_KILL_GRACE`], then SIGKILLs it and reaps the leader, all while the caller still
    /// holds the launch lock; a dropped (cancelled) launch kills the group through [`InstallGroup`].
    async fn ensure_install(&self, kind: &SessionKind) -> anyhow::Result<()> {
        let args = ensure_argv(kind);
        let script = self.resolved_install_script()?;
        let xdg = self.project_root.join(".podman/xdg");
        std::fs::create_dir_all(&xdg)?;
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg(&script)
            .args(&args)
            .env("PODMAN_XDG", &xdg)
            .env("XDG_RUNTIME_DIR", &xdg)
            // the same project root as this runtime: install-rs.sh derives its REAL Wine tree
            // (the busy guard compares CS2_GAME with it) and its install lock from it
            .env("CS2_MCP_PROJECT_ROOT", &self.project_root)
            // install-rs.sh looks `podman` up on PATH; forward the binary this runtime was
            // configured with, so a non-default podman is the one its guards and installs use
            .env("PODMAN", &self.podman)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // backstop for cancellation only: the group kill below is the real cleanup
            .kill_on_drop(true);
        // Always name the Wine tree (canonicalized, so install-rs.sh can compare it with its own
        // real tree and its busy guard applies). The Linux tree is the cs2-home volume, which the
        // script addresses itself.
        if matches!(
            kind,
            SessionKind::WineTools { .. } | SessionKind::WineClient
        ) {
            let app = self.wine_app_dir();
            let canonical = std::fs::canonicalize(&app).map_err(|e| {
                anyhow::anyhow!(
                    "cannot resolve the Wine app dir {}: {e}; the launch needs an existing game tree (install it or point --project-root at the checkout that has it)",
                    app.display()
                )
            })?;
            cmd.env("CS2_GAME", canonical.join("game"));
        }
        // its own process group, so a timeout takes the writers it spawned with it
        #[cfg(unix)]
        cmd.as_std_mut().process_group(0);
        let shown = format!("{} {}", script.display(), args.join(" "));
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot run {shown}: {e}"))?;
        let mut group = InstallGroup::new(child.id());
        // read stderr concurrently: a chatty script must not block on a full pipe while we wait
        let pipe = child.stderr.take();
        let mut stderr_reader = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf).await;
            }
            buf
        });
        let status = match tokio::time::timeout(self.install_timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => return Err(anyhow::anyhow!("cannot run {shown}: {e}")),
            Err(_) => {
                // Timeout: end the whole group while the caller's launch lock is still held
                group.terminate(&mut child, INSTALL_KILL_GRACE).await;
                let (text, joined) = collect_stderr(&mut stderr_reader).await;
                if !joined {
                    stderr_reader.abort();
                }
                let tail = stderr_tail(&text, INSTALL_STDERR_LINES);
                if !tail.is_empty() {
                    tracing::warn!(stderr = %tail, "install-rs.sh ensure timed out; last output");
                }
                return Err(RuntimeError::InstallTimeout {
                    path: script.display().to_string(),
                    args: args.join(" "),
                    secs: self.install_timeout.as_secs(),
                }
                .into());
            }
        };
        let (text, joined) = collect_stderr(&mut stderr_reader).await;
        if !joined {
            // a member still holds the stderr pipe although the ensure succeeded: kill the group
            // (its pgid is still live while the member runs) and drop the reader task that would
            // otherwise hold the pipe past the launch, THEN disarm
            group.kill_group();
            stderr_reader.abort();
        }
        group.disarm();
        let stderr = stderr_tail(&text, INSTALL_STDERR_LINES);
        if status.success() {
            // one line per artefact: current vs repaired is visible in the log
            let shown_stderr = if stderr.is_empty() {
                "(no output)"
            } else {
                stderr.as_str()
            };
            tracing::info!(
                args = %args.join(" "),
                stderr = %shown_stderr,
                "install-rs.sh ensure"
            );
            return Ok(());
        }
        let code = status.code().unwrap_or(-1);
        if code == 4 {
            return Err(RuntimeError::InstallBusy { stderr }.into());
        }
        Err(RuntimeError::InstallFailed {
            path: script.display().to_string(),
            args: args.join(" "),
            code,
            stderr,
        }
        .into())
    }

    /// Run podman with `XDG_RUNTIME_DIR=<project>/.podman/xdg`, bounded by `timeout`. Maps a missing
    /// binary to `PodmanUnavailable`; retries the spawn briefly on ETXTBSY (a freshly written script).
    async fn podman_raw(
        &self,
        args: &[String],
        timeout: Duration,
    ) -> anyhow::Result<std::process::Output> {
        let xdg = self.project_root.join(".podman/xdg");
        std::fs::create_dir_all(&xdg)?;
        let mut attempt = 0;
        loop {
            let fut = tokio::process::Command::new(&self.podman)
                .args(args)
                .env("XDG_RUNTIME_DIR", &xdg)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output();
            match tokio::time::timeout(timeout, fut).await {
                Err(_) => {
                    return Err(RuntimeError::PodmanTimeout {
                        args: args.join(" "),
                        secs: timeout.as_secs(),
                    }
                    .into())
                }
                Ok(Ok(out)) => return Ok(out),
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(RuntimeError::PodmanUnavailable(self.podman_display()).into())
                }
                Ok(Err(e)) if e.raw_os_error() == Some(26) && attempt < 5 => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Ok(Err(e)) => {
                    return Err(anyhow::anyhow!("cannot run {}: {e}", self.podman_display()))
                }
            }
        }
    }

    /// A non-zero exit becomes [`RuntimeError::Podman`]; returns stdout.
    async fn podman_checked(&self, args: &[String], timeout: Duration) -> anyhow::Result<String> {
        let out = self.podman_raw(args, timeout).await?;
        if !out.status.success() {
            return Err(RuntimeError::Podman {
                args: args.join(" "),
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            }
            .into());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// stdout and stderr together (`podman logs` writes the container's stderr to stderr).
    async fn podman_text(&self, args: &[String], timeout: Duration) -> anyhow::Result<String> {
        let out = self.podman_raw(args, timeout).await?;
        if !out.status.success() {
            return Err(RuntimeError::Podman {
                args: args.join(" "),
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            }
            .into());
        }
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok(text)
    }

    /// Names of all running containers (any name).
    pub async fn running_names(&self) -> anyhow::Result<Vec<String>> {
        parse_ps(&self.podman_checked(&ps_argv(), self.short_timeout).await?)
    }

    /// Running containers that count toward the cap (`cs2*`).
    pub async fn running_cs2(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .running_names()
            .await?
            .into_iter()
            .filter(|n| n.starts_with(CAP_PREFIX))
            .collect())
    }

    /// `cs2*` containers that still hold GPU memory or a shared mount: running, or stopping (podman
    /// keeps them until they exit). The cap, the one-per-kind rule and the reuse check use this;
    /// `(name, state)`. A stopping container is not reused (only `running` ones are).
    pub async fn busy_cs2(&self) -> anyhow::Result<Vec<(String, String)>> {
        Ok(parse_ps_entries(
            &self
                .podman_checked(&ps_all_argv(), self.short_timeout)
                .await?,
        )?
        .into_iter()
        .filter(|(n, st)| {
            n.starts_with(CAP_PREFIX) && matches!(st.as_str(), "running" | "stopping" | "paused")
        })
        .collect())
    }

    /// Every `cs2mcp-*` container in any state, as `(name, state)`. `podman ps -a`.
    pub async fn session_containers(&self) -> anyhow::Result<Vec<(String, String)>> {
        Ok(parse_ps_entries(
            &self
                .podman_checked(&ps_all_argv(), self.short_timeout)
                .await?,
        )?
        .into_iter()
        .filter(|(n, _)| n.starts_with(NAME_PREFIX))
        .collect())
    }

    /// The RUNNING container carrying `label` (ModelDoc: `cs2mcp.tool=modeldoc`), or `None` when
    /// nothing carries it. Errors when more than one matches (names both). No ownership check:
    /// the tools container is started by the orchestrator, never by this runtime.
    pub async fn container_by_label(&self, label: &str) -> anyhow::Result<Option<String>> {
        let entries = parse_ps_entries(
            &self
                .podman_checked(&ps_label_argv(label), self.short_timeout)
                .await?,
        )?;
        let names: Vec<String> = entries
            .into_iter()
            .filter(|(_, state)| state == "running")
            .map(|(name, _)| name)
            .collect();
        match names.len() {
            0 => Ok(None),
            1 => Ok(Some(names.into_iter().next().expect("len 1"))),
            _ => Err(anyhow::anyhow!(
                "more than one running container carries the label {label:?}: {}",
                names.join(", ")
            )),
        }
    }

    /// The addon the tools container `name` was started with (`None`: started without `-addon`).
    pub async fn container_addon(&self, name: &str) -> anyhow::Result<Option<String>> {
        if !valid_container_name(name) {
            return Err(RuntimeError::InvalidExecName(name.to_string()).into());
        }
        let text = self
            .podman_checked(&inspect_addon_argv(name), self.short_timeout)
            .await?;
        Ok(parse_container_addon(&text))
    }

    /// `podman exec -i <name> <argv...>` on a container this runtime did NOT launch (no owned
    /// session check: the ModelDoc container belongs to the orchestrator). podman runs with
    /// `XDG_RUNTIME_DIR=<project>/.podman/xdg` like every call here; the exit status, stdout and
    /// stderr are the caller's to judge (`std::process::Output`). A missing podman binary maps
    /// to `PodmanUnavailable`.
    pub async fn exec_in(
        &self,
        name: &str,
        argv: &[String],
        timeout: Duration,
    ) -> anyhow::Result<std::process::Output> {
        if !valid_container_name(name) {
            return Err(RuntimeError::InvalidExecName(name.to_string()).into());
        }
        self.podman_raw(&exec_in_argv(name, argv), timeout).await
    }

    /// [`DockerRuntime::exec_in`] for a process that streams for minutes and may never exit on its
    /// own (a `resourcecompiler` map compile: the Wine process lingers after its `OK:` line, so its
    /// output must be watched **while it runs**). stdout and stderr are appended to `log`
    /// (created if needed), and the child handle lets the caller poll whether podman itself
    /// already exited and end it (`kill`) when it did not — the `.output()` of [`exec_in`] would
    /// only return at the caller's timeout, throwing away everything printed before it.
    ///
    /// Same shape as [`exec_in_argv`] (`exec -i`), same `XDG_RUNTIME_DIR`, same
    /// `valid_container_name` check; `stdin` is null. The child is `kill_on_drop`: a caller that
    /// forgets to kill still cannot leak an exec scan.
    pub async fn exec_in_background(
        &self,
        name: &str,
        argv: &[String],
        log: &Path,
    ) -> anyhow::Result<ExecHandle> {
        if !valid_container_name(name) {
            return Err(RuntimeError::InvalidExecName(name.to_string()).into());
        }
        let xdg = self.project_root.join(".podman/xdg");
        std::fs::create_dir_all(&xdg)?;
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // append: a retried attempt keeps what the failed one printed (the transient `crun` line
        // is classified from this same file)
        let stdout = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
        let stderr = stdout.try_clone()?;
        let child = tokio::process::Command::new(&self.podman)
            .args(exec_in_argv(name, argv))
            .env("XDG_RUNTIME_DIR", &xdg)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    RuntimeError::PodmanUnavailable(self.podman_display()).into()
                } else {
                    anyhow::Error::from(e).context(format!("cannot run {}", self.podman_display()))
                }
            })?;
        Ok(ExecHandle { child, log: log.to_path_buf() })
    }

    /// All `cs2mcp-*` containers (any state). Owned ones report the paths this runtime chose.
    /// Only RUNNING discovered containers are inspected (one `podman inspect` for all of them);
    /// a stopped one is listed bare. Answers from a cache of [`SESSION_CACHE`] (cleared by
    /// `launch` and `down_named`).
    pub async fn list_sessions(&self) -> anyhow::Result<Vec<SessionEntry>> {
        let cached = self
            .session_cache
            .lock()
            .expect("session cache lock")
            .as_ref()
            .filter(|(at, _)| at.elapsed() < SESSION_CACHE)
            .map(|(_, list)| list.clone());
        if let Some(list) = cached {
            return Ok(list);
        }
        let list = self.scan_sessions().await?;
        *self.session_cache.lock().expect("session cache lock") =
            Some((Instant::now(), list.clone()));
        Ok(list)
    }

    async fn scan_sessions(&self) -> anyhow::Result<Vec<SessionEntry>> {
        let owned = self.owned();
        let listed = self.session_containers().await?;
        let inspect: Vec<String> = listed
            .iter()
            .filter(|(n, s)| s == "running" && !owned.iter().any(|o| &o.name == n))
            .map(|(n, _)| n.clone())
            .collect();
        let mut mounts = self.inspect_mounts(&inspect).await?;
        Ok(listed
            .into_iter()
            .map(
                |(name, state)| match owned.iter().find(|o| o.name == name) {
                    Some(o) => SessionEntry::from_owned(o, state),
                    None => match mounts.remove(&name) {
                        Some(Ok(m)) => SessionEntry::from_mounts(name, state, &m),
                        Some(Err(why)) => {
                            let mut e = SessionEntry::bare(name, state, false);
                            e.note = Some(format!("inspect failed: {why}"));
                            e
                        }
                        None => SessionEntry::bare(name, state, false),
                    },
                },
            )
            .collect())
    }

    /// One session by name, owned or discovered, in any state; `None` when podman does not list it.
    /// Inspects that container only.
    pub async fn session_entry(&self, name: &str) -> anyhow::Result<Option<SessionEntry>> {
        let listed = self
            .session_containers()
            .await?
            .into_iter()
            .find(|(n, _)| n == name);
        if let Some(o) = self.owned_named(name) {
            let state = listed.map(|(_, s)| s).unwrap_or_else(|| "gone".into());
            return Ok(Some(SessionEntry::from_owned(&o, state)));
        }
        let Some((_, state)) = listed else {
            return Ok(None);
        };
        let mut mounts = self.inspect_mounts(&[name.to_string()]).await?;
        Ok(Some(match mounts.remove(name) {
            Some(Ok(m)) => SessionEntry::from_mounts(name.to_string(), state, &m),
            Some(Err(why)) => {
                let mut e = SessionEntry::bare(name.to_string(), state, false);
                e.note = Some(format!("inspect failed: {why}"));
                e
            }
            None => SessionEntry::bare(name.to_string(), state, false),
        }))
    }

    /// `podman inspect` mounts of several containers in one call. A container that cannot be
    /// read maps to `Err(reason)`; podman's exit status is not used (it fails when any name is
    /// missing, while the others are still printed).
    pub async fn inspect_mounts(
        &self,
        names: &[String],
    ) -> anyhow::Result<HashMap<String, Result<Vec<Mount>, String>>> {
        let mut out = HashMap::new();
        if names.is_empty() {
            return Ok(out);
        }
        let mut args = vec![
            "inspect".to_string(),
            "-f".into(),
            "{{.Name}}\t{{json .Mounts}}".into(),
        ];
        args.extend(names.iter().cloned());
        let raw = self.podman_raw(&args, self.short_timeout).await?;
        for line in String::from_utf8_lossy(&raw.stdout).lines() {
            if let Some((name, json)) = line.split_once('\t') {
                let parsed = parse_mounts(json).map_err(|e| format!("{e:#}"));
                out.insert(name.trim_start_matches('/').to_string(), parsed);
            }
        }
        let stderr = String::from_utf8_lossy(&raw.stderr).trim().to_string();
        for n in names {
            out.entry(n.clone()).or_insert_with(|| {
                Err(if stderr.is_empty() {
                    "no inspect output".to_string()
                } else {
                    stderr.clone()
                })
            });
        }
        Ok(out)
    }

    fn invalidate_sessions(&self) {
        *self.session_cache.lock().expect("session cache lock") = None;
    }

    /// `podman inspect` state of one container; `Gone` when it no longer exists.
    pub async fn container_state(&self, name: &str) -> anyhow::Result<ContainerState> {
        match self
            .podman_checked(&inspect_state_argv(name), self.short_timeout)
            .await
        {
            Ok(out) => {
                let text = out.trim();
                let mut it = text.split_whitespace();
                let status = it.next().unwrap_or("");
                // an unparsable or missing code is reported as -1, never as a clean exit
                let code = it.next().and_then(|c| c.parse::<i64>().ok()).unwrap_or(-1);
                Ok(match status {
                    "running" => ContainerState::Running,
                    "stopping" => ContainerState::Stopping,
                    "exited" | "stopped" => ContainerState::Exited { code },
                    other => ContainerState::Other(other.to_string()),
                })
            }
            Err(e) if is_no_such(&e) => Ok(ContainerState::Gone),
            Err(e) => Err(e),
        }
    }

    /// Cross-process launch lock: `<project>/.podman/cs2mcp-launch.lock`, held across ps -> run.
    async fn acquire_file_lock(&self, timeout: Duration) -> anyhow::Result<std::fs::File> {
        let dir = self.project_root.join(".podman");
        let path = dir.join("cs2mcp-launch.lock");
        tokio::task::spawn_blocking(move || -> anyhow::Result<std::fs::File> {
            std::fs::create_dir_all(&dir)?;
            let f = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&path)?;
            let started = Instant::now();
            loop {
                match f.try_lock() {
                    Ok(()) => return Ok(f),
                    Err(std::fs::TryLockError::WouldBlock) => {
                        if started.elapsed() >= timeout {
                            return Err(RuntimeError::LaunchLockTimeout {
                                path: path.display().to_string(),
                                secs: timeout.as_secs(),
                            }
                            .into());
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
                }
            }
        })
        .await?
    }

    /// Everything that can be decided without touching podman.
    fn validate(&self, spec: &LaunchSpec, check_staged: bool) -> anyhow::Result<()> {
        // the container sessions are Linux (podman + Wine/Proton); a Windows server attaches to the
        // game `cs2mcp-native up` started instead
        if cfg!(windows) {
            anyhow::bail!(
                "launch_session needs the Linux podman runtime; on Windows start the game with `cs2mcp-native up` and this server attaches to it (crates/cs2-native)"
            );
        }
        if let Some(n) = &spec.container_name {
            if !valid_name(n) {
                return Err(RuntimeError::InvalidName(n.clone()).into());
            }
        }
        if let Some(t) = &spec.testscript {
            if !valid_suite_name(t) {
                return Err(RuntimeError::Unsupported(format!(
                    "testscript {t:?} is not a suite name ([A-Za-z0-9_-]+)"
                ))
                .into());
            }
        }
        match &spec.kind {
            SessionKind::LinuxClient => {
                if !spec.headless {
                    return Err(RuntimeError::Unsupported(
                        "linux-client is headless only (in-container Xvfb); host-display Linux is not implemented".into(),
                    )
                    .into());
                }
                if spec.testscript.is_some() {
                    return Err(RuntimeError::Unsupported(
                        "testscript runs are wine-tools only; the Linux suite path is an open question (workflows/01)".into(),
                    )
                    .into());
                }
                self.check_linux_scripts(&self.linux_client_script)?;
            }
            SessionKind::LinuxDedicated => {
                if spec.testscript.is_some() {
                    return Err(RuntimeError::Unsupported(
                        "linux-dedicated has no testscript mode (use server_up for the server, launch_session for a client)".into(),
                    )
                    .into());
                }
                self.check_linux_scripts(&sibling_script(
                    &self.linux_client_script,
                    LINUX_DEDICATED_SCRIPT_NAME,
                ))?;
            }
            SessionKind::WineTools { .. } => {
                if spec.headless {
                    return Err(RuntimeError::Unsupported(
                        "headless wine-tools is not implemented (needs the hammer-mcp 08 display fix); use the host X display".into(),
                    )
                    .into());
                }
                if let (Some(t), true) = (&spec.testscript, check_staged) {
                    let staged = self.wine_app_dir().join("vtest").join(format!("{t}.txt"));
                    if !staged.is_file() {
                        return Err(
                            RuntimeError::SuiteNotStaged(staged.display().to_string()).into()
                        );
                    }
                }
                match &self.xauthority {
                    Some(p) if p.is_file() => {}
                    other => {
                        return Err(RuntimeError::MissingXauthority(
                            other
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "none found in /run/user".into()),
                        )
                        .into())
                    }
                }
            }
            SessionKind::WineClient => {
                // same shape as wine-tools minus the tools-only extras: the kind carries no addon
                // (`SessionKind::WineClient` is a unit variant, so a spec cannot carry one) and a
                // testscript has no meaning without `-tools`
                if spec.headless {
                    return Err(RuntimeError::Unsupported(
                        "headless wine-client is not implemented (needs the hammer-mcp 08 display fix); use the host X display".into(),
                    )
                    .into());
                }
                if spec.testscript.is_some() {
                    return Err(RuntimeError::Unsupported(
                        "testscript runs are wine-tools only; the wine client has no -tools".into(),
                    )
                    .into());
                }
                match &self.xauthority {
                    Some(p) if p.is_file() => {}
                    other => {
                        return Err(RuntimeError::MissingXauthority(
                            other
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "none found in /run/user".into()),
                        )
                        .into())
                    }
                }
            }
        }
        Ok(())
    }

    /// The script a Linux kind mounts and the preamble it sources must both exist on the host.
    fn check_linux_scripts(&self, script: &Path) -> anyhow::Result<()> {
        for file in [
            script.to_path_buf(),
            sibling_script(&self.linux_client_script, LINUX_LIB_NAME),
        ] {
            if !file.is_file() {
                return Err(RuntimeError::Unsupported(format!(
                    "linux script missing: {}",
                    file.display()
                ))
                .into());
            }
        }
        Ok(())
    }

    /// One session per kind per shared mount (see the module doc).
    async fn check_kind_busy(&self, spec: &LaunchSpec, running: &[String]) -> anyhow::Result<()> {
        let app = self.wine_app_dir();
        let mut mounts = self.inspect_mounts(running).await?;
        for name in running {
            if !may_conflict(spec, kind_of_name(name)) {
                continue;
            }
            // unreadable mounts: the container is gone or broken and holds nothing we can see
            let Some(Ok(list)) = mounts.remove(name) else {
                continue;
            };
            if let Some(shared) = shared_mount_conflict(spec, &app, &list) {
                return Err(RuntimeError::KindBusy {
                    kind: kind_slug(&spec.kind),
                    container: name.clone(),
                    shared,
                }
                .into());
            }
        }
        Ok(())
    }

    /// What `launch` decides before it writes anything: argument validation (a testscript need not
    /// be staged yet), the session cap and the one-per-kind rule. Takes no lock; `launch` re-checks
    /// under the lock. Use it to refuse before the caller stages files.
    pub async fn preflight(&self, spec: &LaunchSpec) -> anyhow::Result<()> {
        self.validate(spec, false)?;
        let running = busy_names(&self.busy_cs2().await?);
        if running.len() >= self.max_sessions {
            return Err(RuntimeError::TooManySessions {
                running,
                max: self.max_sessions,
            }
            .into());
        }
        self.check_kind_busy(spec, &running).await
    }

    /// [`DockerRuntime::preflight`] for a caller that stages files for the launch. An identical
    /// request whose session is running is reused by `launch`, so it passes here without the cap
    /// and KindBusy checks (those would refuse the reuse). Nothing is written.
    pub async fn preflight_reusable(&self, spec: &LaunchSpec) -> anyhow::Result<()> {
        self.validate(spec, false)?;
        let busy = self.busy_cs2().await?;
        if self.reusable(spec, &busy).is_some() {
            return Ok(());
        }
        let running = busy_names(&busy);
        if running.len() >= self.max_sessions {
            return Err(RuntimeError::TooManySessions {
                running,
                max: self.max_sessions,
            }
            .into());
        }
        self.check_kind_busy(spec, &running).await
    }

    /// The owned session an identical request reuses: same spec key, still running. A stopping
    /// one is not reused: its process is going away.
    fn reusable(&self, spec: &LaunchSpec, busy: &[(String, String)]) -> Option<Owned> {
        let key = spec_key(spec);
        self.owned().into_iter().rev().find(|o| {
            spec_key(&o.spec) == key && busy.iter().any(|(n, st)| n == &o.name && st == "running")
        })
    }

    /// Launch a session: validate, lock, reuse-or-cap-or-busy, prepare artifacts, `podman run -d`.
    pub async fn launch(&self, spec: &LaunchSpec) -> anyhow::Result<Launched> {
        self.launch_within(spec, LOCK_TIMEOUT).await
    }

    /// [`DockerRuntime::launch`] with the wait for the cross-process launch lock capped at
    /// `lock_timeout` (callers pass what is left of their call budget).
    pub async fn launch_within(
        &self,
        spec: &LaunchSpec,
        lock_timeout: Duration,
    ) -> anyhow::Result<Launched> {
        self.validate(spec, true)?;
        let _gate = self.launch_gate.lock().await;
        let _file_lock = self.acquire_file_lock(lock_timeout).await?;

        let busy = self.busy_cs2().await?;
        let running = busy_names(&busy);
        // idempotent: an identical request while its session runs returns that session
        if let Some(o) = self.reusable(spec, &busy) {
            return Ok(Launched {
                container: o.name,
                argv: o.argv,
                paths: o.paths,
                artifacts_dir: o.artifacts_dir,
                capture_dir: o.capture_dir,
                reused: true,
            });
        }
        if running.len() >= self.max_sessions {
            return Err(RuntimeError::TooManySessions {
                running,
                max: self.max_sessions,
            }
            .into());
        }
        self.check_kind_busy(spec, &running).await?;
        // Metamod/the bridge live in the shared game tree: make it current before `podman run`.
        // Only a real launch reaches this point (a reused session returned above).
        self.ensure_install(&spec.kind).await?;

        let name = match &spec.container_name {
            Some(n) => n.clone(),
            None => generated_name(&spec.kind),
        };
        let artifacts_dir = self
            .project_root
            .join("artifacts")
            .join(format!("{name}-{}", unix_secs()));
        std::fs::create_dir_all(&artifacts_dir)?;
        // the layer is mounted only when the spec does not opt out AND the layer dir is valid
        // checked here, not at startup: the layer dir may be built or removed while the server runs
        let layer = self
            .vk_layer_dir
            .as_deref()
            .filter(|d| spec.capture && layer_status(d).valid);
        let capture_dir = match layer {
            Some(_) => {
                let dir = artifacts_dir.join("capture");
                make_private_dir(&dir)?;
                Some(dir)
            }
            None => None,
        };

        let app_dir = self.wine_app_dir();
        let cache_dir = self.project_root.join("workshop/run/cache");
        let display = self.host_display.clone();
        let (paths, argv) = match &spec.kind {
            SessionKind::WineTools { .. } | SessionKind::WineClient => {
                let mut paths = self.wine_paths();
                // a bridge that was SIGKILLed never deleted its discovery file: it would look like
                // this launch's bridge. The KindBusy check above guarantees no other session uses
                // it. Both Wine kinds share the file, so both clean it.
                if let Some(discovery) = &paths.discovery {
                    let _ = std::fs::remove_file(discovery);
                }
                // the wine client runs the bridge as a plain Metamod plugin: nothing listens, so
                // there is no discovery file to report after the cleanup above
                if matches!(spec.kind, SessionKind::WineClient) {
                    paths.discovery = None;
                }
                // run_vtest.sh:52 moves the previous console.log aside, right before `run`. A log
                // that cannot be moved aside must not survive: it would be appended to by the new
                // client and a previous run's menu line would satisfy the readiness marker.
                let console = paths.console_log.clone().expect("wine console log");
                if console.exists() {
                    if let Err(e) = std::fs::rename(&console, console.with_extension("log.prev")) {
                        std::fs::remove_file(&console).map_err(|_| {
                            anyhow::anyhow!(
                                "cannot clear the stale console log {}: rename failed ({e}) and removing it failed too",
                                console.display()
                            )
                        })?;
                    }
                }
                let plan = RunPlan {
                    name: &name,
                    image: &self.image,
                    out_dir: &artifacts_dir,
                    linux_script: &self.linux_client_script,
                    linux_game_dir: &self.linux_game_dir,
                    app_dir: &app_dir,
                    cache_dir: &cache_dir,
                    xauthority: self.xauthority.as_deref(),
                    display: &display,
                    capture_layer: layer,
                    // Linux-only: wine_argv never reads it (see `s2sdk_probe_argv`)
                    s2sdk_probe: None,
                };
                (paths, run_argv(spec, &plan))
            }
            _ => {
                let paths = SessionPaths {
                    app_root: artifacts_dir.clone(),
                    discovery: None,
                    console_log: Some(artifacts_dir.join("console.log")),
                    pfx: None,
                };
                let script = match spec.kind {
                    SessionKind::LinuxDedicated => {
                        sibling_script(&self.linux_client_script, LINUX_DEDICATED_SCRIPT_NAME)
                    }
                    _ => self.linux_client_script.clone(),
                };
                let plan = RunPlan {
                    name: &name,
                    image: &self.image,
                    out_dir: &artifacts_dir,
                    linux_script: &script,
                    linux_game_dir: &self.linux_game_dir,
                    app_dir: &app_dir,
                    cache_dir: &cache_dir,
                    xauthority: None,
                    display: &display,
                    capture_layer: layer,
                    s2sdk_probe: self.s2sdk_probe_dir.as_deref(),
                };
                (paths, run_argv(spec, &plan))
            }
        };

        let bridge_log_offset = std::fs::metadata(paths.app_root.join(BRIDGE_LOG_NAME))
            .map(|m| m.len())
            .unwrap_or(0);
        let launched_at = SystemTime::now();
        self.podman_checked(&argv, self.run_timeout).await?;
        let mut full = vec![self.podman_display()];
        full.extend(argv);
        self.invalidate_sessions();
        self.owned.lock().expect("owned lock").push(Owned {
            name: name.clone(),
            spec: spec.clone(),
            paths: paths.clone(),
            artifacts_dir: artifacts_dir.clone(),
            capture_dir: capture_dir.clone(),
            argv: full.clone(),
            launched_at,
            channel_up: false,
            bridge_requested_at: None,
            bridge_log_offset,
            bridge_up: false,
        });
        Ok(Launched {
            container: name,
            argv: full,
            paths,
            artifacts_dir,
            capture_dir,
            reused: false,
        })
    }

    /// Stop a `cs2mcp-*` container (owned or not, e.g. after a server restart) and remove it once
    /// podman reports it exited. Nothing here kills podman: `podman stop -t 0` is detached, and the
    /// exit is awaited with `podman wait` (read-only) for at most `within`. A container still
    /// stopping after that is [`StopOutcome::Pending`]; calling again is safe. Owned state (and a
    /// wine-tools discovery file) is dropped only once the container is gone.
    pub async fn stop_named(&self, name: &str, within: Duration) -> anyhow::Result<StopOutcome> {
        Ok(self.stop_named_report(name, within).await?.outcome)
    }

    /// [`DockerRuntime::stop_named`] with the time each step took ([`StopTiming`]).
    pub async fn stop_named_report(
        &self,
        name: &str,
        within: Duration,
    ) -> anyhow::Result<StopReport> {
        if !name.starts_with(NAME_PREFIX) {
            return Err(RuntimeError::ForeignContainer(name.to_string()).into());
        }
        if !valid_name(name) {
            return Err(RuntimeError::InvalidName(name.to_string()).into());
        }
        let mut clock = StopClock::new();
        let deadline = clock.started + within;
        let outcome = self.stop_and_remove(name, deadline, &mut clock).await?;
        if !matches!(outcome, StopOutcome::Pending { .. }) {
            let owned = self.owned_named(name);
            if let Some(o) = owned {
                // the wine-tools bridge's discovery file goes with its session (wine_paths is the
                // only layout this runtime launches; a foreign container's file is left alone)
                if matches!(o.spec.kind, SessionKind::WineTools { .. })
                    && o.paths.app_root == self.wine_app_dir()
                {
                    if let Some(d) = &o.paths.discovery {
                        let _ = std::fs::remove_file(d);
                    }
                }
            }
            self.owned
                .lock()
                .expect("owned lock")
                .retain(|o| o.name != name);
            self.stop_issued.lock().expect("stop lock").remove(name);
            self.invalidate_sessions();
        }
        clock.timing.total_ms = clock.now_ms();
        Ok(StopReport {
            outcome,
            timing: clock.timing,
        })
    }

    /// The stop itself: issue it when the container runs (or is stopping, and the last stop is old
    /// or unknown), wait for the exit, then `rm` (no `-f`). Nothing is removed while it runs.
    /// A first `inspect` that times out (podman stalled, e.g. on its store's disk) does not abort:
    /// the stop is issued anyway (a stop of an exited or gone container is harmless), so a stalled
    /// podman never leaves the game running.
    async fn stop_and_remove(
        &self,
        name: &str,
        deadline: Instant,
        clock: &mut StopClock,
    ) -> anyhow::Result<StopOutcome> {
        let state = match self.container_state(name).await {
            Ok(st) => Some(st),
            Err(e) if is_podman_timeout(&e) => {
                tracing::warn!(container = %name, error = %e, "state unknown; issuing the stop anyway");
                None
            }
            Err(e) => return Err(e),
        };
        clock.timing.initial_state = state.as_ref().map_or("unknown".into(), state_label);
        let issue = match &state {
            None | Some(ContainerState::Stopping) => self.stop_is_stale(name),
            Some(ContainerState::Running) => true,
            Some(ContainerState::Other(s)) => s == "paused",
            Some(ContainerState::Gone | ContainerState::Exited { .. }) => false,
        };
        if issue {
            self.issue_stop(name).await?;
            clock.timing.stop_issued_ms = Some(clock.now_ms());
        }
        let live = match state {
            Some(ContainerState::Gone) => return Ok(StopOutcome::AlreadyGone),
            None | Some(ContainerState::Running | ContainerState::Stopping) => true,
            Some(ContainerState::Other(s)) => s == "paused",
            Some(ContainerState::Exited { .. }) => false,
        };
        if live {
            match self.wait_exited(name, deadline).await? {
                Exit::Exited => {}
                Exit::Gone => return Ok(StopOutcome::AlreadyGone),
                Exit::Pending => return Ok(self.pending_outcome(name).await),
            }
        }
        clock.timing.exited_ms = Some(clock.now_ms());
        self.remove_confirmed(name, deadline, clock).await
    }

    /// `podman rm <name>` (no `-f`), detached like the stop (nothing kills it), its stderr in
    /// `.podman/rm-<name>.err` (deleted once the outcome is Removed or the error is built; kept
    /// while pending, as rm may still write to it). Confirmed by polling the state, against its own window: at least
    /// [`RM_CONFIRM`] (capped at the short podman timeout), so a stop that used its budget on the
    /// exit still has room for the removal. rm exiting non-zero while the container remains is an
    /// error with rm's stderr. Still present at the end of the window is `Pending`; the next
    /// stop_session issues the removal again (a gone container is `Removed` there too).
    /// An `inspect` that times out here counts as "still there": the window decides.
    async fn remove_confirmed(
        &self,
        name: &str,
        deadline: Instant,
        clock: &mut StopClock,
    ) -> anyhow::Result<StopOutcome> {
        let argv = rm_argv(name);
        let dir = self.project_root.join(".podman");
        std::fs::create_dir_all(&dir)?;
        let err_path = dir.join(format!("rm-{name}.err"));
        let err_file = std::fs::File::create(&err_path)?;
        let mut child = self.spawn_podman(&argv, Some(err_file)).await?;
        clock.timing.rm_issued_ms = Some(clock.now_ms());
        let confirm_until = deadline.max(Instant::now() + RM_CONFIRM.min(self.short_timeout));
        loop {
            // try_wait: the child is never killed here, only observed
            let exit = child.try_wait()?;
            let gone = match self.container_state(name).await {
                Ok(st) => st == ContainerState::Gone,
                Err(e) if is_podman_timeout(&e) => false,
                Err(e) => return Err(e),
            };
            if gone {
                // the stderr file only matters while rm can still fail: gone, it goes
                let _ = std::fs::remove_file(&err_path);
                clock.timing.removed_ms = Some(clock.now_ms());
                return Ok(StopOutcome::Removed);
            }
            if let Some(status) = exit.filter(|s| !s.success()) {
                let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
                let _ = std::fs::remove_file(&err_path);
                return Err(RuntimeError::Podman {
                    args: argv.join(" "),
                    status: status.to_string(),
                    stderr: stderr.trim().to_string(),
                }
                .into());
            }
            let left = confirm_until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(self.pending_outcome(name).await);
            }
            tokio::time::sleep(STOP_POLL.min(left)).await;
        }
    }

    /// What a stop that ran out of budget reports: the state podman has now, and the age of the stop.
    async fn pending_outcome(&self, name: &str) -> StopOutcome {
        let state = match self.container_state(name).await {
            Ok(st) => state_label(&st),
            Err(_) => "unknown".into(),
        };
        StopOutcome::Pending {
            state,
            stop_issued_ms: self.stop_issued_ms(name),
        }
    }

    /// `podman stop -t 0`, detached: nothing waits on it and nothing kills it (a state change, so no
    /// `kill_on_drop`). The wait for the exit decides, not this process's status.
    async fn issue_stop(&self, name: &str) -> anyhow::Result<()> {
        self.stop_issued
            .lock()
            .expect("stop lock")
            .insert(name.to_string(), Instant::now());
        let mut child = self.spawn_podman(&stop_argv(name), None).await?;
        let name = name.to_string();
        // only the exit status is logged: the state wait decides
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) if status.success() => {}
                Ok(status) => tracing::warn!(
                    container = %name, %status,
                    "podman stop did not succeed; the state wait decides"
                ),
                Err(e) => tracing::warn!(container = %name, error = %e, "podman stop wait failed"),
            }
        });
        Ok(())
    }

    /// A `stopping` container is stopped again only when the last stop is old or unknown.
    fn stop_is_stale(&self, name: &str) -> bool {
        self.stop_issued
            .lock()
            .expect("stop lock")
            .get(name)
            .is_none_or(|t| t.elapsed() > STOP_REISSUE_AFTER)
    }

    fn stop_issued_ms(&self, name: &str) -> Option<u64> {
        self.stop_issued
            .lock()
            .expect("stop lock")
            .get(name)
            .map(|t| t.elapsed().as_millis() as u64)
    }

    /// Wait until podman reports the container exited, for at most until `deadline`. Primary path:
    /// `podman wait --condition=exited` (a timeout kills only that read-only wait). Fallback when
    /// the wait command fails for another reason: `inspect` every [`STOP_POLL`] (an `inspect` that
    /// times out is polled again until the deadline).
    async fn wait_exited(&self, name: &str, deadline: Instant) -> anyhow::Result<Exit> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(Exit::Pending);
        }
        match self.podman_raw(&wait_argv(name), left).await {
            Ok(out) if out.status.success() => return Ok(Exit::Exited),
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                if no_such_text(&stderr) {
                    return Ok(Exit::Gone);
                }
                tracing::warn!(container = %name, stderr = %stderr.trim(), "podman wait failed; polling the state");
            }
            Err(e) => match e.downcast_ref::<RuntimeError>() {
                Some(RuntimeError::PodmanTimeout { .. }) => return Ok(Exit::Pending),
                _ if is_no_such(&e) => return Ok(Exit::Gone),
                _ => {
                    tracing::warn!(container = %name, error = %e, "podman wait failed; polling the state")
                }
            },
        }
        loop {
            match self.container_state(name).await {
                Ok(ContainerState::Gone) => return Ok(Exit::Gone),
                Ok(ContainerState::Exited { .. }) => return Ok(Exit::Exited),
                Ok(_) => {}
                Err(e) if is_podman_timeout(&e) => {}
                Err(e) => return Err(e),
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Exit::Pending);
            }
            tokio::time::sleep(STOP_POLL.min(left)).await;
        }
    }

    /// Blocking stop for callers without a budget (run teardown, server shutdown): waits up to the
    /// lock timeout. Returns `true` when this call removed the container and `false` when it was
    /// already gone. A stop that is still pending after that is an error (`StopPending`).
    pub async fn down_named(&self, name: &str) -> anyhow::Result<bool> {
        match self.stop_named(name, LOCK_TIMEOUT).await? {
            StopOutcome::Removed => Ok(true),
            StopOutcome::AlreadyGone => Ok(false),
            StopOutcome::Pending { .. } => Err(RuntimeError::StopPending {
                container: name.to_string(),
                secs: LOCK_TIMEOUT.as_secs(),
            }
            .into()),
        }
    }

    /// `podman` with the runtime's environment, spawned (not run to completion). A state-changing
    /// call is spawned without `kill_on_drop`, so dropping the caller does not kill it.
    async fn spawn_podman(
        &self,
        args: &[String],
        stderr: Option<std::fs::File>,
    ) -> anyhow::Result<tokio::process::Child> {
        let xdg = self.project_root.join(".podman/xdg");
        std::fs::create_dir_all(&xdg)?;
        let mut attempt = 0;
        loop {
            match tokio::process::Command::new(&self.podman)
                .args(args)
                .env("XDG_RUNTIME_DIR", &xdg)
                .stdin(Stdio::null())
                // no stdout pipe, and no stderr pipe either: podman writes warnings to stderr just
                // before it kills a stopping container, and a pipe whose reader is gone would
                // SIGPIPE it before that. rm's stderr goes to a file the caller passes instead.
                .stdout(Stdio::null())
                .stderr(match &stderr {
                    Some(file) => Stdio::from(file.try_clone()?),
                    None => Stdio::null(),
                })
                .spawn()
            {
                Ok(child) => return Ok(child),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(RuntimeError::PodmanUnavailable(self.podman_display()).into())
                }
                Err(e) if e.raw_os_error() == Some(26) && attempt < 5 => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(anyhow::anyhow!("cannot run {}: {e}", self.podman_display())),
            }
        }
    }

    /// Poll until `target` is reached, the container stops, or `timeout` passes. Stages below the
    /// target are checked in order; a stage this runtime cannot check is reported, not waited for.
    /// Container liveness is refreshed every 2 s; console.log is scanned incrementally.
    pub async fn wait_ready(
        &self,
        container: &str,
        target: ReadyStage,
        timeout: Duration,
    ) -> anyhow::Result<Readiness> {
        let owned = self.owned_named(container).ok_or_else(|| {
            RuntimeError::NoSession(format!("{container} was not launched by this server"))
        })?;
        let started = Instant::now();
        let mut rep = Readiness::new(target);
        let mut live = Liveness::default();
        let mut probes = Probes::default();
        'stages: for (stage, check) in readiness::ladder(&owned.spec) {
            if stage > target {
                break;
            }
            if let Check::Unchecked(reason) = check {
                rep.unchecked.push(UncheckedStage { stage, reason });
                if stage == target {
                    rep.note = Some(format!("{} is not checked: {reason}", stage.as_str()));
                    break;
                }
                continue;
            }
            let mut scan = MarkerScan::default();
            loop {
                if let Some(exit) = self.refresh_liveness(container, &mut live).await? {
                    rep.note = Some(format!(
                        "container {container} {} before {} was reached{}",
                        exit.status,
                        stage.as_str(),
                        exit.exit_code
                            .map(|c| format!(" (exit code {c})"))
                            .unwrap_or_default()
                    ));
                    rep.exited = Some(exit);
                    break 'stages;
                }
                let left = timeout.saturating_sub(started.elapsed());
                let done = match check {
                    Check::Container => live.running,
                    Check::File => owned.paths.console_log.as_ref().is_some_and(|p| p.exists()),
                    Check::Marker(m) => match &owned.paths.console_log {
                        Some(p) => scan_for(p, &[m], &mut scan).await?,
                        None => false,
                    },
                    Check::ServerLog(markers) => {
                        let p = owned.artifacts_dir.join(SERVER_LOG_NAME);
                        scan_for(&p, markers, &mut scan).await?
                    }
                    Check::Channel => {
                        self.channel_check(container, &owned, &mut probes, left)
                            .await?
                    }
                    Check::Bridge => {
                        self.bridge_check(container, &owned, &mut probes, left)
                            .await?
                    }
                    Check::Unchecked(_) => false,
                };
                if done {
                    rep.reached.push(stage);
                    rep.target_reached = stage == target;
                    break;
                }
                if started.elapsed() >= timeout {
                    let mut note = format!(
                        "timed out after {} ms waiting for {}",
                        timeout.as_millis(),
                        stage.as_str()
                    );
                    if matches!(check, Check::Channel | Check::Bridge) {
                        if let Some(e) = &probes.last_error {
                            note.push_str(&format!("; last probe error: {e}"));
                        }
                    }
                    if matches!(check, Check::Bridge) {
                        if let Some(tail) = self.bridge_log_tail(container) {
                            note.push_str(&format!("; {BRIDGE_LOG_NAME} since launch: {tail}"));
                        }
                    }
                    rep.note = Some(note);
                    break 'stages;
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            if stage == target {
                break;
            }
        }
        rep.elapsed_ms = started.elapsed().as_millis() as u64;
        Ok(rep)
    }

    /// Wine channel stage (runtime.sh `wait_channel`): one vtmsend probe per [`CHANNEL_PROBE_EVERY`];
    /// true once it prints [`CHANNEL_UP_MARKER`]. The result is kept in `Owned`, so a later wait does
    /// not probe again. A probe that fails is recorded for the timeout note and retried.
    async fn channel_check(
        &self,
        container: &str,
        owned: &Owned,
        probes: &mut Probes,
        left: Duration,
    ) -> anyhow::Result<bool> {
        if self
            .with_owned(container, |o| o.channel_up)
            .unwrap_or(false)
        {
            return Ok(true);
        }
        if left.is_zero()
            || probes
                .channel_at
                .is_some_and(|t| t.elapsed() < CHANNEL_PROBE_EVERY)
        {
            return Ok(false);
        }
        probes.channel_at = Some(Instant::now());
        match self
            .vtmsend_once(
                container,
                &owned.paths.app_root,
                &channel_probe_payload(),
                left,
            )
            .await
        {
            Ok(out) if out.contains(CHANNEL_UP_MARKER) => {
                self.with_owned(container, |o| o.channel_up = true);
                Ok(true)
            }
            Ok(out) => {
                // not up yet: keep vtmsend's last line (e.g. `OpenMutex failed`) for the timeout note
                if let Some(line) = out.lines().rev().find(|l| !l.trim().is_empty()) {
                    probes.last_error = Some(format!("channel probe: {}", line.trim()));
                }
                Ok(false)
            }
            Err(e) => {
                probes.last_error = Some(format!("channel probe: {e:#}"));
                Ok(false)
            }
        }
    }

    /// Wine bridge stage (runtime.sh `load_bridge`). While the discovery file is missing or older
    /// than the launch, the tool_request is posted once and again every [`BRIDGE_REPOST_EVERY`].
    /// A fresh file is then checked with `control.hello` every [`HELLO_EVERY`]; the hello is the
    /// authoritative signal, the file's mtime only says when to ask.
    async fn bridge_check(
        &self,
        container: &str,
        owned: &Owned,
        probes: &mut Probes,
        left: Duration,
    ) -> anyhow::Result<bool> {
        let Some(discovery) = owned.paths.discovery.as_deref() else {
            return Ok(false);
        };
        if self.with_owned(container, |o| o.bridge_up).unwrap_or(false) {
            return Ok(true);
        }
        if !discovery_fresh(discovery, owned.launched_at) {
            // the due check and the stamp happen in one step under the lock, so a concurrent wait
            // cannot post the same request twice
            let may_post = !left.is_zero();
            let post = self
                .with_owned(container, |o| {
                    let due = may_post
                        && o.bridge_requested_at
                            .is_none_or(|t| t.elapsed() >= BRIDGE_REPOST_EVERY);
                    if due {
                        o.bridge_requested_at = Some(Instant::now());
                    }
                    due
                })
                .unwrap_or(false);
            if post {
                if let Err(e) = self
                    .vtmsend_once(
                        container,
                        &owned.paths.app_root,
                        &bridge_request_payload(),
                        left,
                    )
                    .await
                {
                    probes.last_error = Some(format!("tool_request: {e:#}"));
                }
            }
            return Ok(false);
        }
        if left.is_zero() || probes.hello_at.is_some_and(|t| t.elapsed() < HELLO_EVERY) {
            return Ok(false);
        }
        probes.hello_at = Some(Instant::now());
        match bridge_hello(discovery, left).await {
            Hello::Answered | Hello::Busy => {
                // Busy: the bridge's one authenticated slot is held (by our own client, or the
                // game's). A fresh file plus Busy means the bridge is up.
                self.with_owned(container, |o| o.bridge_up = true);
                Ok(true)
            }
            Hello::Failed(why) => {
                probes.last_error = Some(why);
                Ok(false)
            }
        }
    }

    /// Write `payload` as the vtmsend payload and run vtmsend once in the session (runtime.sh
    /// `put_payload` + `vtmsend`). Returns its output (stdout and stderr: vtmsend's exit is not used).
    /// Bounded by `left`, and by the short podman timeout.
    async fn vtmsend_once(
        &self,
        container: &str,
        app_root: &Path,
        payload: &str,
        left: Duration,
    ) -> anyhow::Result<String> {
        write_payload_atomic(app_root, payload)?;
        let out = self
            .podman_raw(&vtmsend_argv(container), left.min(self.short_timeout))
            .await?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The last lines the bridge appended to `mcp_bridge_rs.log` since this launch, for a timeout note.
    fn bridge_log_tail(&self, container: &str) -> Option<String> {
        use std::io::{Read, Seek};
        /// the most of the log read back; a bridge that logs a lot is only quoted from its end
        const TAIL_BYTES: u64 = 64 * 1024;
        let (root, offset) = self.with_owned(container, |o| {
            (o.paths.app_root.clone(), o.bridge_log_offset)
        })?;
        let mut f = std::fs::File::open(root.join(BRIDGE_LOG_NAME)).ok()?;
        let len = f.metadata().ok()?.len();
        let start = if len < offset { 0 } else { offset };
        let start = start.max(len.saturating_sub(TAIL_BYTES));
        f.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            return None;
        }
        Some(lines[lines.len().saturating_sub(10)..].join(" | "))
    }

    /// Refresh the cached liveness at most every [`LIVENESS_EVERY`]. Returns the exit report when
    /// the container is gone for good (`exited`, or no longer present).
    async fn refresh_liveness(
        &self,
        name: &str,
        live: &mut Liveness,
    ) -> anyhow::Result<Option<ExitInfo>> {
        if live.last.is_some_and(|t| t.elapsed() < LIVENESS_EVERY) {
            return Ok(None);
        }
        live.last = Some(Instant::now());
        live.running = self.running_names().await?.iter().any(|n| n == name);
        if live.running {
            return Ok(None);
        }
        Ok(match self.container_state(name).await? {
            // `ps` can lag right after `run -d`: keep waiting
            ContainerState::Running | ContainerState::Stopping | ContainerState::Other(_) => None,
            ContainerState::Exited { code } => Some(ExitInfo {
                status: "exited".into(),
                exit_code: Some(code),
                logs_tail: self.log_tail(name).await,
            }),
            ContainerState::Gone => Some(ExitInfo {
                status: "gone".into(),
                exit_code: None,
                logs_tail: Vec::new(),
            }),
        })
    }

    async fn log_tail(&self, name: &str) -> Vec<String> {
        match self
            .podman_text(&logs_argv(name, EXIT_LOG_LINES), self.short_timeout)
            .await
        {
            Ok(t) => t.lines().map(String::from).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Host layout of a Wine tools session (`workshop/run/app`, run_vtest.sh:21-23).
    pub fn wine_paths(&self) -> SessionPaths {
        let app = self.wine_app_dir();
        let pfx = app.join("compatdata/730/pfx");
        SessionPaths {
            discovery: Some(
                pfx.join("drive_c/users/steamuser/AppData/Local/cs2mcpbridge/session.json"),
            ),
            console_log: Some(app.join("game/csgo/console.log")),
            pfx: Some(pfx),
            app_root: app,
        }
    }
}

/// Result of a throw-away `control.hello`.
enum Hello {
    Answered,
    /// the bridge refused a second client: its one authenticated slot is taken
    Busy,
    Failed(String),
}

/// `control.hello` on the discovery file (a throw-away client: connect, hello with the file's
/// token, close). Bounded by `left` and [`HELLO_DEADLINE`]. No busy retries: a Busy answer is the
/// signal itself, not something to wait out.
async fn bridge_hello(discovery: &Path, left: Duration) -> Hello {
    let mut client = BridgeClient::new(ClientOptions {
        discovery_path: discovery.to_path_buf(),
        connect_timeout_ms: HELLO_CONNECT.as_millis() as u64,
        deadline_ms: HELLO_DEADLINE.as_millis() as u64,
        block_when_stale: false,
        busy_retries: 0,
        ..ClientOptions::default()
    });
    let hello = tokio::time::timeout(left.min(HELLO_DEADLINE), client.connect(false)).await;
    client.close();
    match hello {
        Ok(Ok(_)) => Hello::Answered,
        Ok(Err(e)) if matches!(e.kind, Kind::Busy) => Hello::Busy,
        Ok(Err(e)) => Hello::Failed(format!("control.hello: {}", e.message)),
        Err(_) => Hello::Failed("control.hello: no answer within the wait".into()),
    }
}

/// Scan only the bytes appended since the last call (plus an overlap) for any of `markers`. A
/// shrinking file (`-conclearlog`) restarts the scan from 0.
async fn scan_for(path: &Path, markers: &[&str], scan: &mut MarkerScan) -> anyhow::Result<bool> {
    if scan.found {
        return Ok(true);
    }
    let len = match tokio::fs::metadata(path).await {
        Ok(m) => m.len(),
        Err(_) => return Ok(false),
    };
    if len < scan.offset {
        scan.offset = 0;
    }
    if len == scan.offset {
        return Ok(false);
    }
    let start = scan.offset.saturating_sub(SCAN_OVERLAP);
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(SeekFrom::Start(start)).await?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.take(len - start).read_to_end(&mut buf).await?;
    scan.offset = len;
    let text = String::from_utf8_lossy(&buf);
    scan.found = markers.iter().any(|m| text.contains(m));
    Ok(scan.found)
}

impl Runtime for DockerRuntime {
    fn name(&self) -> &'static str {
        "docker"
    }

    async fn status(&self) -> anyhow::Result<RuntimeStatus> {
        let running = self.running_cs2().await?;
        let container = match self.last_owned() {
            Some(o) if running.contains(&o.name) => Some(o.name),
            _ => running.first().cloned(),
        };
        Ok(RuntimeStatus {
            running: !running.is_empty(),
            container,
            pid_hint: None,
            display: None,
            notes: vec![format!(
                "{} cs2* container(s) running (cap {})",
                running.len(),
                self.max_sessions
            )],
        })
    }

    async fn up(&self, spec: &LaunchSpec) -> anyhow::Result<SessionPaths> {
        Ok(self.launch(spec).await?.paths)
    }

    async fn down(&self) -> anyhow::Result<()> {
        let last = self
            .last_owned()
            .ok_or_else(|| RuntimeError::NoSession("nothing launched".into()))?;
        self.down_named(&last.name).await?;
        Ok(())
    }

    async fn exec(&self, argv: &[String]) -> anyhow::Result<std::process::Output> {
        let last = self
            .last_owned()
            .ok_or_else(|| RuntimeError::NoSession("nothing launched".into()))?;
        self.podman_raw(&exec_argv(&last.name, argv), self.short_timeout)
            .await
    }

    fn paths(&self) -> SessionPaths {
        self.last_owned()
            .map(|o| o.paths)
            .unwrap_or_else(|| self.wine_paths())
    }

    async fn logs(&self, lines: usize) -> anyhow::Result<String> {
        let last = self
            .last_owned()
            .ok_or_else(|| RuntimeError::NoSession("nothing launched".into()))?;
        self.podman_text(&logs_argv(&last.name, lines), self.short_timeout)
            .await
    }
}

/// The VToolMessage payloads a caller posts through `vtmsend` (`vtm_command_payload` is the one
/// `run_map` uses to put `map <name> nomapvalidation=1` into the engine console).
#[cfg(test)]
mod vtm_payloads {
    use super::*;

    #[test]
    fn a_command_payload_is_the_recipe_kv3_file() {
        // the bytes the `vcmd.sh` recipe writes, verbatim
        assert_eq!(
            vtm_command_payload("map probe nomapvalidation=1"),
            "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} \
             format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{\n\t_message = \
             \"command\"\n\t_data = \"map probe nomapvalidation=1\"\n}\n"
        );
        // the channel probe is that same payload
        assert_eq!(
            channel_probe_payload(),
            vtm_command_payload("echo mcp_runtime_channel_probe")
        );
    }

    #[test]
    fn a_command_payload_escapes_the_kv3_string() {
        // the two characters the recipe escapes; a `"` would end the string, a `\` would escape it
        let payload = vtm_command_payload(r#"echo "a b" \ c"#);
        assert!(
            payload.contains(r#"_data = "echo \"a b\" \\ c""#),
            "{payload:?}"
        );
        assert_eq!(payload.matches("\"command\"").count(), 1);
    }

    #[test]
    fn vtmsend_is_run_the_runtime_sh_way() {
        assert_eq!(
            vtmsend_program_argv(),
            vec![
                "bash".to_string(),
                "-c".to_string(),
                "cd /app && ./entrypoint.sh vtmsend.exe 2>&1".to_string()
            ]
        );
        // the full argv a podman call needs: `exec -i <container> <program argv...>`
        assert_eq!(
            vtmsend_argv("cs2tools"),
            vec![
                "exec".to_string(),
                "-i".to_string(),
                "cs2tools".to_string(),
                "bash".to_string(),
                "-c".to_string(),
                "cd /app && ./entrypoint.sh vtmsend.exe 2>&1".to_string()
            ]
        );
    }
}

#[cfg(all(test, unix))]
mod wait_unreaped_rules {
    use super::*;

    /// A short-lived child the caller reaps itself: `waitid` must answer `ECHILD` (the pid is no
    /// longer our child and may be reused) instead of spinning to the deadline.
    #[test]
    fn an_already_reaped_child_returns_gone_reaped_at_once() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn sh");
        child.wait().expect("reap the child");
        let started = std::time::Instant::now();
        let far = started + std::time::Duration::from_secs(60);
        let outcome = InstallGroup::wait_unreaped(child.id() as libc::pid_t, far);
        assert_eq!(outcome, UnreapedWait::GoneReaped);
        // without the ECHILD return this spun to the far deadline
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    /// A child that exits on its own and is NOT reaped: `WNOHANG` reports the state change and
    /// `WNOWAIT` leaves the zombie in place (its pid is still safe to signal through).
    #[test]
    fn an_exited_child_is_left_a_zombie_and_reported_exited_unreaped() {
        // not `mut`: the zombie is reaped below via libc::waitpid, the way terminate does
        #[allow(clippy::zombie_processes)]
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn sh");
        let started = std::time::Instant::now();
        let outcome = InstallGroup::wait_unreaped(
            child.id() as libc::pid_t,
            started + std::time::Duration::from_secs(10),
        );
        assert_eq!(outcome, UnreapedWait::ExitedUnreaped);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        // terminate's last step: reap the zombie WNOWAIT left behind
        let reaped = unsafe { libc::waitpid(child.id() as libc::pid_t, std::ptr::null_mut(), 0) };
        assert_eq!(reaped, child.id() as libc::pid_t);
    }

    /// A running child is still a child: the wait returns Timeout at the deadline, not GoneReaped.
    #[test]
    fn a_running_child_times_out() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .spawn()
            .expect("spawn sh");
        let started = std::time::Instant::now();
        let outcome = InstallGroup::wait_unreaped(
            child.id() as libc::pid_t,
            started + std::time::Duration::from_millis(30),
        );
        assert_eq!(outcome, UnreapedWait::Timeout);
        child.kill().expect("kill the probe child");
        child.wait().expect("reap the probe child");
    }
}

#[cfg(test)]
mod mount_rules {
    use super::*;

    fn mount(kind: &str, source: &str, dest: &str) -> Mount {
        Mount {
            kind: kind.into(),
            name: String::new(),
            source: source.into(),
            destination: dest.into(),
        }
    }

    #[test]
    fn a_bind_mount_gives_the_host_path_and_a_volume_gives_none() {
        let mounts = [mount("volume", "/var/lib/volumes/cs2-home/_data", "/app")];
        let e = SessionEntry::from_mounts(
            "cs2mcp-wine-tools-0badf00d".into(),
            "running".into(),
            &mounts,
        );
        assert_eq!(e.console_log, None);
        assert_eq!(e.app_dir, None);
        assert!(e.note.unwrap().contains("volume"));
    }

    #[test]
    fn a_bind_mount_is_preferred_over_a_volume_on_the_same_destination() {
        let mounts = [
            mount("volume", "/var/lib/volumes/x/_data", "/work"),
            mount("bind", "/host/artifacts/s1", "/work"),
        ];
        let e = SessionEntry::from_mounts(
            "cs2mcp-linux-client-0badf00d".into(),
            "running".into(),
            &mounts,
        );
        assert_eq!(e.artifacts_dir, Some(PathBuf::from("/host/artifacts/s1")));
        assert_eq!(
            e.console_log,
            Some(PathBuf::from("/host/artifacts/s1/console.log"))
        );
    }

    #[test]
    fn both_mounts_bound_is_broken_by_the_name_kind() {
        let mounts = [
            mount("bind", "/host/app", "/app"),
            mount("bind", "/host/art", "/work"),
        ];
        let wine = SessionEntry::from_mounts(
            "cs2mcp-wine-tools-0badf00d".into(),
            "running".into(),
            &mounts,
        );
        assert_eq!(wine.app_dir, Some(PathBuf::from("/host/app")));
        assert_eq!(
            wine.console_log,
            Some(PathBuf::from("/host/app/game/csgo/console.log"))
        );
        let linux = SessionEntry::from_mounts(
            "cs2mcp-linux-client-0badf00d".into(),
            "running".into(),
            &mounts,
        );
        assert_eq!(
            linux.console_log,
            Some(PathBuf::from("/host/art/console.log"))
        );
        let unknown =
            SessionEntry::from_mounts("cs2mcp-mystery-0badf00d".into(), "running".into(), &mounts);
        assert_eq!(unknown.console_log, None);
        assert!(unknown.note.unwrap().contains("does not say which kind"));
    }

    fn linux_spec(kind: SessionKind) -> LaunchSpec {
        LaunchSpec {
            kind,
            testscript: None,
            map: None,
            args: Vec::new(),
            headless: true,
            gpus: false,
            container_name: None,
            capture: false,
            bridge: true,
        }
    }

    #[test]
    fn a_client_and_a_dedicated_server_may_share_the_home_volume_but_two_of_one_kind_may_not() {
        let home = [mount(
            "volume",
            "/var/lib/volumes/cs2-home/_data",
            "/home/steam",
        )];
        let app = Path::new("/unused");
        let client = linux_spec(SessionKind::LinuxClient);
        let server = linux_spec(SessionKind::LinuxDedicated);
        // a running client does not block a server, and the reverse
        assert!(!may_conflict(&server, "linux-client"));
        assert!(!may_conflict(&client, "linux-dedicated"));
        // the same kind does, and so does a name this server cannot classify
        assert!(may_conflict(&server, "linux-dedicated"));
        assert!(may_conflict(&client, "linux-client"));
        assert!(may_conflict(&server, "unknown"));
        assert_eq!(
            shared_mount_conflict(&server, app, &home),
            Some("cs2-home volume")
        );
        assert_eq!(
            shared_mount_conflict(&client, app, &home),
            Some("cs2-home volume")
        );
        // a wine-tools session holds /app, not the home volume
        let wine = linux_spec(SessionKind::WineTools { addon: None });
        assert!(may_conflict(&wine, "linux-client"));
        assert_eq!(shared_mount_conflict(&wine, app, &home), None);
    }

    #[test]
    fn wine_command_puts_addon_before_the_testscript_flag() {
        let mut spec = linux_spec(SessionKind::WineTools {
            addon: Some("cs_script_demo".into()),
        });
        spec.testscript = Some("harness-editor--zoo--smartprop_zoo".into());
        assert_eq!(
            wine_command(Some("cs_script_demo"), &spec),
            "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console -condebug -insecure -addon cs_script_demo -testscript /app/vtest/harness-editor--zoo--smartprop_zoo.txt +engine_no_focus_sleep 0 +r_always_render_all_windows 1"
        );
    }

    #[test]
    fn spec_key_tells_maps_apart_with_the_same_testscript() {
        // the same suite name on both sides: only the map differs
        let mut a = linux_spec(SessionKind::WineTools { addon: None });
        a.testscript = Some("harness-x".into());
        let mut b = a.clone();
        a.map = Some("de_dust2".into());
        b.map = Some("cs_script_demo".into());
        assert_ne!(spec_key(&a), spec_key(&b));
        let mut same = a.clone();
        same.map = Some("de_dust2".into());
        assert_eq!(spec_key(&a), spec_key(&same));
        // and a map is not the same request as none
        b.map = None;
        assert_ne!(spec_key(&a), spec_key(&b));
    }

    #[test]
    fn kind_is_read_from_the_generated_name() {
        assert_eq!(kind_of_name("cs2mcp-wine-tools-1234abcd"), "wine-tools");
        assert_eq!(kind_of_name("cs2mcp-linux-client-1234abcd"), "linux-client");
        assert_eq!(kind_of_name("cs2vtest"), "unknown");
    }
}

#[cfg(test)]
mod container_addon_tests {
    use super::parse_container_addon;

    #[test]
    fn the_label_wins_over_the_launch_command() {
        let text = "ti_convoy|[\"/bin/bash\",\"-c\",\"exec x -tools -addon mcp_test +y\"]";
        assert_eq!(parse_container_addon(text).as_deref(), Some("ti_convoy"));
    }

    #[test]
    fn the_launch_command_is_the_fallback() {
        let text = "|[\"/bin/bash\",\"-c\",\"exec x -tools -console -addon mcp_test +y\"]\n";
        assert_eq!(parse_container_addon(text).as_deref(), Some("mcp_test"));
        assert_eq!(parse_container_addon("<no value>|null"), None);
        assert_eq!(parse_container_addon("|[\"x\",\"-tools\"]"), None);
    }
}
