//! Readiness ladder (workflows/01-docker-runtime.md step 3) as typed data.
//!
//! The ladder is pure: [`ladder`] says, per session kind, which stages exist, how each is checked
//! or why it is not checked. The polling lives in `DockerRuntime::wait_ready`.

use super::{LaunchSpec, SessionKind};
use serde::Serialize;

/// Stages in order. A later stage implies the earlier ones were reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum ReadyStage {
    /// the container is in state `running` (`podman ps`)
    ContainerUp,
    /// the engine created `csgo/console.log` (host path in `SessionPaths::console_log`)
    ConsoleLogExists,
    /// Linux: `SwitchToLoop game` in console.log (tests/cs2/t30_cs2_boot.sh:166).
    /// Wine tools: the VToolMessage channel is up (vtmsend prints `creator version`; `cargo bridge runtime` `wait_channel`).
    /// Wine client: `SwitchToLoop game` in the app tree's console.log ([`WINE_CLIENT_MENU_MARKER`];
    /// unverified live, see the const).
    MenuReached,
    /// Wine: the `cs2_bridge` tool is loaded and `control.hello` answers with the discovery token
    /// (`cargo bridge runtime`'s `load_bridge`). Not checked for `bridge: false` sessions.
    BridgeHello,
    /// Wine `-testscript` harness parked: `TESTSCRIPT: CheckPoint -> 'VTEST_PARKED'`
    /// (sandbox.txt / harness.txt `Test_Run mark VTEST_PARKED`; AGENT.md rule 9 line format)
    Parked,
    /// Linux dedicated server: the map is up and the server is activating (`server.log`, see
    /// [`SERVER_LIVE_MARKERS`]). Only a LinuxDedicated session reaches it; the others report it as
    /// unchecked.
    ServerLive,
}

impl ReadyStage {
    pub const ALL: [ReadyStage; 6] = [
        ReadyStage::ContainerUp,
        ReadyStage::ConsoleLogExists,
        ReadyStage::MenuReached,
        ReadyStage::BridgeHello,
        ReadyStage::Parked,
        ReadyStage::ServerLive,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ReadyStage::ContainerUp => "ContainerUp",
            ReadyStage::ConsoleLogExists => "ConsoleLogExists",
            ReadyStage::MenuReached => "MenuReached",
            ReadyStage::BridgeHello => "BridgeHello",
            ReadyStage::Parked => "Parked",
            ReadyStage::ServerLive => "ServerLive",
        }
    }

    /// Case-insensitive; `-`, `_` and spaces are ignored (`"menu-reached"` works).
    pub fn parse(s: &str) -> Option<Self> {
        let norm: String = s
            .chars()
            .filter(|c| !matches!(c, '-' | '_' | ' '))
            .collect::<String>()
            .to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|st| st.as_str().to_ascii_lowercase() == norm)
    }
}

/// Linux menu marker (tests/cs2/t30_cs2_boot.sh:166).
pub const LINUX_MENU_MARKER: &str = "SwitchToLoop game";
/// Wine client menu marker. Same string as [`LINUX_MENU_MARKER`]: `SwitchToLoop` is the
/// platform-independent `IEngineServiceMgr` entry point (`iengineservicemgr.h`), and the engine
/// prints the loop switch to `console.log` when the main menu is up. **Unverified on the Wine
/// client** (no client has been run live yet; the string's evidence is the Linux boot test and the
/// engine header): the orchestrator should confirm the line in the Wine client's
/// `game/csgo/console.log` (`-condebug`) and amend this const if the wording differs.
pub const WINE_CLIENT_MENU_MARKER: &str = LINUX_MENU_MARKER;
/// Output of `vtmsend.exe` once toolframework2 Init created the VToolMessage mutex (`cargo bridge runtime`
/// `wait_channel`). Before that vtmsend exits 1 with `OpenMutex failed`, which is not this text.
pub const CHANNEL_UP_MARKER: &str = "creator version";
/// Parked-harness marker (test-script CheckPoint line; see [`ReadyStage::Parked`]).
pub const PARKED_MARKER: &str = "CheckPoint -> 'VTEST_PARKED'";
/// Lines a live CS2 dedicated server (LAN, anonymous, gbe) printed once its map was up, observed on
/// the host (stdout of a server started without `-condebug`): the server state change to active, the
/// listening socket, and the hibernation line the server prints while it has no clients. Any one
/// counts. The server log is stdout, teed to [`SERVER_LOG_NAME`].
pub const SERVER_LIVE_MARKERS: &[&str] = &[
    "CNetworkGameServerBase::SetServerState (ss_loading -> ss_active)",
    "Network socket 'server' opened on port",
    "Server is hibernating",
];
/// The dedicated server's full log, inside the session artifacts dir. CS2 has no `con_logfile`, and
/// without `-condebug` the server writes its log to stdout; linux-dedicated.sh tees stdout here.
/// The markers are plain substrings (no timestamps or channel prefixes on stdout).
pub const SERVER_LOG_NAME: &str = "server.log";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// container listed as running
    Container,
    /// the console.log path exists
    File,
    /// console.log contains this text
    Marker(&'static str),
    /// `server.log` in the artifacts dir contains any of these texts
    ServerLog(&'static [&'static str]),
    /// Wine: the VToolMessage channel answers a probe (`DockerRuntime::wait_ready` posts it via exec)
    Channel,
    /// Wine: the bridge discovery file is fresh and `control.hello` answers (the runtime connects)
    Bridge,
    /// cannot be checked by this runtime; the reason goes into the readiness report
    Unchecked(&'static str),
}

/// The ladder for a session, in order. `bridge: false` (wine-tools only) leaves BridgeHello unchecked.
pub fn ladder(spec: &LaunchSpec) -> Vec<(ReadyStage, Check)> {
    match &spec.kind {
        SessionKind::LinuxClient => vec![
            (ReadyStage::ContainerUp, Check::Container),
            (ReadyStage::ConsoleLogExists, Check::File),
            (ReadyStage::MenuReached, Check::Marker(LINUX_MENU_MARKER)),
            (
                ReadyStage::BridgeHello,
                Check::Unchecked("Linux client has no bridge"),
            ),
            (
                ReadyStage::Parked,
                Check::Unchecked("the parked test-script harness is Wine-only (run_vtest.sh / harness.txt)"),
            ),
            (
                ReadyStage::ServerLive,
                Check::Unchecked("a dedicated server stage; this is a client session"),
            ),
        ],
        SessionKind::WineTools { .. } => vec![
            (ReadyStage::ContainerUp, Check::Container),
            (ReadyStage::ConsoleLogExists, Check::File),
            (ReadyStage::MenuReached, Check::Channel),
            (
                ReadyStage::BridgeHello,
                if spec.bridge {
                    Check::Bridge
                } else {
                    Check::Unchecked("bridge: false (cs2_bridge is not loaded)")
                },
            ),
            (ReadyStage::Parked, Check::Marker(PARKED_MARKER)),
            (
                ReadyStage::ServerLive,
                Check::Unchecked("a dedicated server stage; this is a Wine tools session"),
            ),
        ],
        SessionKind::WineClient => vec![
            (ReadyStage::ContainerUp, Check::Container),
            (ReadyStage::ConsoleLogExists, Check::File),
            (
                ReadyStage::MenuReached,
                Check::Marker(WINE_CLIENT_MENU_MARKER),
            ),
            (
                ReadyStage::BridgeHello,
                Check::Unchecked(
                    "no bridge RPC on a wine client: the bridge runs as a plain Metamod plugin (needs the GameFrame tick first)",
                ),
            ),
            (
                ReadyStage::Parked,
                Check::Unchecked("the parked test-script harness is Wine-tools-only (run_vtest.sh / harness.txt)"),
            ),
            (
                ReadyStage::ServerLive,
                Check::Unchecked("a dedicated server stage; this is a Wine client session"),
            ),
        ],
        SessionKind::LinuxDedicated => vec![
            (ReadyStage::ContainerUp, Check::Container),
            (
                ReadyStage::ConsoleLogExists,
                Check::Unchecked("the server log is server.log (stdout); console.log is the shared game-dir file and is not waited for"),
            ),
            (
                ReadyStage::MenuReached,
                Check::Unchecked("a dedicated server has no menu"),
            ),
            (
                ReadyStage::BridgeHello,
                Check::Unchecked("no bridge on a Linux dedicated server"),
            ),
            (
                ReadyStage::Parked,
                Check::Unchecked("the parked test-script harness is Wine-only"),
            ),
            (ReadyStage::ServerLive, Check::ServerLog(SERVER_LIVE_MARKERS)),
        ],
    }
}

/// The stage a `launch_session` waits for when the caller gives no `waitFor`.
pub fn default_wait(spec: &LaunchSpec) -> ReadyStage {
    match spec.kind {
        SessionKind::LinuxClient => ReadyStage::MenuReached,
        SessionKind::WineTools { .. } if spec.testscript.is_some() => ReadyStage::Parked,
        SessionKind::WineTools { .. } if spec.bridge => ReadyStage::BridgeHello,
        // without the bridge the last stage that can be checked is the channel
        SessionKind::WineTools { .. } => ReadyStage::MenuReached,
        SessionKind::LinuxDedicated => ReadyStage::ServerLive,
        SessionKind::WineClient => ReadyStage::MenuReached,
    }
}

/// A container that stopped while waiting: its state, exit code and the last log lines.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitInfo {
    /// podman state: `exited`, or `gone` when the container no longer exists
    pub status: String,
    pub exit_code: Option<i64>,
    pub logs_tail: Vec<String>,
}

/// Result of `wait_ready`. A timeout is a report, not an error: the session stays up.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Readiness {
    pub target: ReadyStage,
    pub target_reached: bool,
    pub reached: Vec<ReadyStage>,
    pub unchecked: Vec<UncheckedStage>,
    pub elapsed_ms: u64,
    pub note: Option<String>,
    /// set when the container stopped before the target (the wait ends there)
    pub exited: Option<ExitInfo>,
}

impl Readiness {
    pub fn new(target: ReadyStage) -> Self {
        Readiness {
            target,
            target_reached: false,
            reached: Vec::new(),
            unchecked: Vec::new(),
            elapsed_ms: 0,
            note: None,
            exited: None,
        }
    }

    /// The readiness report when the wait itself failed (the container stays known to the caller).
    pub fn failed(target: ReadyStage, error: impl Into<String>) -> Self {
        let mut r = Readiness::new(target);
        r.note = Some(error.into());
        r
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UncheckedStage {
    pub stage: ReadyStage,
    pub reason: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_is_forgiving() {
        assert_eq!(
            ReadyStage::parse("MenuReached"),
            Some(ReadyStage::MenuReached)
        );
        assert_eq!(
            ReadyStage::parse("menu-reached"),
            Some(ReadyStage::MenuReached)
        );
        assert_eq!(
            ReadyStage::parse("container_up"),
            Some(ReadyStage::ContainerUp)
        );
        assert_eq!(ReadyStage::parse("nope"), None);
    }

    #[test]
    fn stages_are_ordered_and_ladders_end_at_the_target_stage_set() {
        assert!(ReadyStage::ContainerUp < ReadyStage::Parked);
        for kind in [
            SessionKind::LinuxClient,
            SessionKind::WineTools { addon: None },
            SessionKind::WineClient,
            SessionKind::LinuxDedicated,
        ] {
            let spec = LaunchSpec {
                kind: kind.clone(),
                testscript: None,
                map: None,
                args: Vec::new(),
                headless: true,
                gpus: false,
                container_name: None,
                capture: false,
                bridge: true,
            };
            let l = ladder(&spec);
            let stages: Vec<_> = l.iter().map(|(s, _)| *s).collect();
            assert_eq!(stages, ReadyStage::ALL.to_vec(), "{kind:?}");
        }
    }
}
