//! server_up / client_connect / server_console_exec, and launch_session {connect}, against a fake
//! podman (cs2_mcp_common::mock::podman) and a fake RCON server (cs2_mcp_common::mock::rcon). No
//! container runtime and no game.
//!
//! The dedicated server's own console (`server.log`) is written by a helper thread into the
//! session's artifacts dir, as the dedicated script's mirror would. The fake RCON server binds on a
//! helper thread after `server_up` has checked the port (a bound port would be refused).
#![allow(clippy::result_large_err)]
use client_mcp::state::ClientConfig;
use cs2_mcp_common::mock::podman::FakePodman;
use cs2_mcp_common::mock::rcon::{CommandLog, FakeRcon, Mode};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The markers a live CS2 dedicated server printed (see readiness.rs SERVER_LIVE_MARKERS).
const LIVE_LOG: &str = "[Networking] Network socket 'server' opened on port 27015\n\
[Server] CNetworkGameServerBase::SetServerState (ss_loading -> ss_active)\n";
/// `podman inspect {{json .Mounts}}` of a container that holds the shared `cs2-home` volume.
const HOME_VOLUME: &str = r#"[{"Type":"volume","Name":"cs2-home","Source":"/var/lib/containers/storage/volumes/cs2-home/_data","Destination":"/home/steam"}]"#;

mod common;

fn config(args: &[&str]) -> ClientConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: ClientConfig,
    }
    let mut argv = vec!["t"];
    argv.extend(common::hermetic(args));
    W::parse_from(argv).c
}

fn project(fake: &FakePodman) -> PathBuf {
    let p = fake.dir().join("project");
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn server(
    fake: &FakePodman,
    project: &Path,
    max: usize,
) -> cs2_mcp_common::mcp::McpServer<client_mcp::state::ClientState> {
    let bin = fake.bin.display().to_string();
    let proj = project.display().to_string();
    let max = max.to_string();
    client_mcp::build_server(&config(&[
        "--discovery",
        "/tmp/cs2mcp-no-bridge/session.json",
        "--console-log",
        "/tmp/cs2mcp-no-console.log",
        "--podman",
        &bin,
        "--project-root",
        &proj,
        "--max-sessions",
        &max,
    ]))
    .unwrap()
}

async fn call(
    server: &cs2_mcp_common::mcp::McpServer<client_mcp::state::ClientState>,
    name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

/// A game port for one test, unique within this test binary: an atomic counter over 20000..32000,
/// below the Linux ephemeral range (32768..=60999). Outgoing RCON connects take their source ports
/// from the ephemeral range, so a port from it can be held by a client socket for a moment.
///
/// The port is not probed by binding it. A probe that binds and drops a listener leaves the port
/// in LISTEN for a moment after the close (under parallel load the next bind fails with
/// EADDRINUSE), so the probe itself made `server_up`'s own check fail. The counter starts at a
/// per-process offset, so concurrent test processes (other sessions, other worktrees) do not hand
/// out the same ports in the same order.
fn free_port() -> u16 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    static BASE: OnceLock<u32> = OnceLock::new();
    let base = *BASE.get_or_init(|| std::process::id().wrapping_mul(7919) % 12000);
    20000 + ((base + NEXT.fetch_add(1, Ordering::Relaxed)) % 12000) as u16
}

/// The artifacts dir of the first `cs2mcp-linux-dedicated-*` session under `project`, if it exists.
fn find_dedicated_dir(project: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(project.join("artifacts")).ok()?;
    entries.flatten().map(|e| e.path()).find(|p| {
        p.file_name()
            .map(|n| n.to_string_lossy().starts_with("cs2mcp-linux-dedicated-"))
            .unwrap_or(false)
    })
}

/// A live-looking dedicated server for one test: once the session's artifacts dir exists (the
/// launch has passed the port check), it binds an RCON server on `port` when `rcon` is set, then
/// writes the live markers into the session's `server.log`. Stops (and unbinds) on drop.
struct LiveServer {
    stop: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
    commands: CommandLog,
}

impl LiveServer {
    fn start(project: PathBuf, port: u16, rcon: Option<(&'static str, Mode)>) -> LiveServer {
        let (stop_tx, stop_rx) = channel::<()>();
        let commands = CommandLog::default();
        let log = commands.clone();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let mut rcon = rcon;
                let mut _fake = None;
                let mut written = false;
                let until = Instant::now() + Duration::from_secs(30);
                while Instant::now() < until && stop_rx.try_recv().is_err() {
                    if !written {
                        if let Some(dir) = find_dedicated_dir(&project) {
                            // the artifacts dir is made by the launch, which comes after server_up's
                            // port check: binding the RCON port now cannot make that check fail.
                            // The log is written after the bind, so a client that reads ServerLive
                            // finds the RCON port already listening.
                            if let Some((pw, mode)) = rcon.take() {
                                _fake = Some(
                                    FakeRcon::bind(
                                        &format!("127.0.0.1:{port}"),
                                        pw,
                                        mode,
                                        log.clone(),
                                    )
                                    .await,
                                );
                            }
                            std::fs::write(dir.join("server.log"), LIVE_LOG).unwrap();
                            written = true;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            });
        });
        LiveServer {
            stop: Some(stop_tx),
            handle: Some(handle),
            commands,
        }
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

impl Drop for LiveServer {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The `run` argv of the n-th launch recorded by the fake podman.
fn run_argv(fake: &FakePodman, n: usize) -> Vec<String> {
    fake.calls_to("run")[n].argv.clone()
}

#[tokio::test]
async fn server_up_launches_a_dedicated_server_and_reports_live() {
    let fake = FakePodman::new("mp-up");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    let port = free_port();
    let live = LiveServer::start(proj.clone(), port, None);
    let out = call(
        &srv,
        "server_up",
        json!({"rconPassword": "pw-1", "port": port, "timeoutMs": 15000}),
    )
    .await
    .unwrap();
    drop(live);

    let name = out["container"].as_str().unwrap();
    assert!(name.starts_with("cs2mcp-linux-dedicated-"), "{name}");
    assert_eq!(out["kind"], json!("linux-dedicated"));
    assert_eq!(out["map"], json!("de_dust2"));
    assert_eq!(out["port"], json!(port));
    assert_eq!(out["rconPassword"], json!("pw-1"));
    assert_eq!(out["readiness"]["targetReached"], json!(true));
    assert_eq!(out["readiness"]["target"], json!("ServerLive"));
    assert_eq!(out["pending"], json!(false));
    assert!(out.get("bots").is_none());

    let run = run_argv(&fake, 0);
    // the tool's game args are the tail: +sv_hibernate_when_empty 0 (the bridge must tick on an
    // empty server), then the password, then the port (non-default ports only).
    // No -condebug and no con_logfile: the full log goes to stdout.
    let tail = [
        "+rcon_password".to_string(),
        "pw-1".to_string(),
        "+sv_hibernate_when_empty".to_string(),
        "0".to_string(),
        "+hostport".to_string(),
        port.to_string(),
    ];
    assert_eq!(run[run.len() - 6..].to_vec(), tail.to_vec());
    assert!(!run.iter().any(|a| a == "-condebug" || a == "+con_logfile"));
    assert!(run.iter().any(|a| a == "--network=host"));
    assert!(run.iter().any(
        |a| a.ends_with("/client-mcp/docker/linux-dedicated.sh:/cs2mcp/linux-dedicated.sh:ro")
    ));
    assert!(run
        .iter()
        .any(|a| a.ends_with("/docker/lib-preamble.sh:/cs2mcp/lib-preamble.sh:ro")));
    assert!(!run.iter().any(|a| a.contains("shadercache")));
    assert!(run.iter().any(|a| a == "/cs2mcp/linux-dedicated.sh"));
}

/// The argv both ways (live finding 2026-10-20: an empty dedicated server hibernates by default,
/// so the bridge never ticked): by default `+sv_hibernate_when_empty 0` lands before the caller's
/// args, and with `hibernate: true` the convar is left at the game default.
#[tokio::test]
async fn server_up_passes_hibernate_off_by_default_and_args_win() {
    let fake = FakePodman::new("mp-hibernate");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    let out = call(
        &srv,
        "server_up",
        json!({
            "port": free_port(),
            "args": ["+sv_hibernate_when_empty", "1"],
            "timeoutMs": 300
        }),
    )
    .await
    .unwrap();
    let run = run_argv(&fake, 0);
    // the tool's own convar first …
    let at = run
        .iter()
        .position(|a| a == "+sv_hibernate_when_empty")
        .expect("+sv_hibernate_when_empty in the run argv");
    assert_eq!(run[at + 1], "0");
    // … and the caller's arg after it (appended last, so the user value wins)
    let user_at = run
        .iter()
        .rposition(|a| a == "+sv_hibernate_when_empty")
        .expect("the caller's +sv_hibernate_when_empty");
    assert!(user_at > at);
    assert_eq!(run[user_at + 1], "1");
    assert_eq!(run[user_at..].to_vec(), ["+sv_hibernate_when_empty", "1"].map(String::from));
    assert_eq!(out["pending"], json!(true));
}

#[tokio::test]
async fn server_up_leaves_hibernation_at_the_game_default_when_asked() {
    let fake = FakePodman::new("mp-hibernate-on");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    let out = call(
        &srv,
        "server_up",
        json!({"port": free_port(), "hibernate": true, "timeoutMs": 300}),
    )
    .await
    .unwrap();
    assert!(!run_argv(&fake, 0)
        .iter()
        .any(|a| a.contains("sv_hibernate_when_empty")));
    assert_eq!(out["pending"], json!(true));
}

#[tokio::test]
async fn server_up_saves_the_session_file_with_mode_0600() {
    let fake = FakePodman::new("mp-session");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    let port = free_port();
    let live = LiveServer::start(proj.clone(), port, None);
    let out = call(
        &srv,
        "server_up",
        json!({"rconPassword": "pw-s", "port": port, "timeoutMs": 15000}),
    )
    .await
    .unwrap();
    drop(live);
    let file = PathBuf::from(out["sessionFile"].as_str().unwrap());
    let raw: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(raw["rconPassword"], json!("pw-s"));
    assert_eq!(raw["port"], json!(port));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
async fn server_up_reports_pending_until_the_marker_appears() {
    let fake = FakePodman::new("mp-pending");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    let out = call(
        &srv,
        "server_up",
        json!({"bots": {"ct": 2}, "port": free_port(), "timeoutMs": 300}),
    )
    .await
    .unwrap();
    assert_eq!(out["pending"], json!(true));
    assert_eq!(out["readiness"]["targetReached"], json!(false));
    assert_eq!(out["next"], json!("wait_session {container, waitFor}"));
    // bots wait for the live server: nothing is sent, the reason says so
    assert_eq!(out["bots"]["applied"], json!(false));
    assert!(out["bots"]["reason"].as_str().unwrap().contains("not live"));
}

#[tokio::test]
async fn server_up_adds_bots_over_rcon_once_live() {
    let fake = FakePodman::new("mp-bots");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    let port = free_port();
    let live = LiveServer::start(proj.clone(), port, Some(("pw-2", Mode::LoneAuthResponse)));
    let out = call(
        &srv,
        "server_up",
        json!({
            "rconPassword": "pw-2",
            "port": port,
            "bots": {"ct": 2, "t": 1},
            "timeoutMs": 15000
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["bots"]["applied"], json!(true), "{}", out["bots"]);
    assert_eq!(out["bots"]["replies"].as_array().unwrap().len(), 3);
    assert_eq!(
        live.commands(),
        ["bot_add_ct", "bot_add_ct", "bot_add_t"].map(String::from)
    );
}

#[tokio::test]
async fn server_console_exec_runs_over_rcon_and_refuses_quit() {
    let fake = FakePodman::new("mp-exec");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    let port = free_port();
    let live = LiveServer::start(proj.clone(), port, Some(("pw-3", Mode::LoneAuthResponse)));
    let up = call(
        &srv,
        "server_up",
        json!({"rconPassword": "pw-3", "port": port, "timeoutMs": 15000}),
    )
    .await
    .unwrap();
    let name = up["container"].as_str().unwrap().to_string();

    let out = call(
        &srv,
        "server_console_exec",
        json!({"command": "sv_cheats 1"}),
    )
    .await
    .unwrap();
    assert_eq!(out["response"], json!("ran sv_cheats 1"));
    assert_eq!(out["route"], json!("rcon"));
    assert_eq!(out["container"], json!(name));

    // the refusal guard runs before anything is sent
    let err = call(
        &srv,
        "server_console_exec",
        json!({"command": "echo hi; quit"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "ForbiddenCommand");
    assert_eq!(live.commands(), vec!["sv_cheats 1".to_string()]);

    // REVIEW 9: a command over the engine's 510 bytes is refused before anything is sent
    let err = call(
        &srv,
        "server_console_exec",
        json!({"command": format!("echo {}", "a".repeat(520))}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments", "{err:?}");
    assert_eq!(live.commands(), vec!["sv_cheats 1".to_string()]);

    let err = call(
        &srv,
        "server_console_exec",
        json!({"command": "status", "container": "cs2mcp-linux-dedicated-ffffffff"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "NoSession");
}

#[tokio::test]
async fn a_new_mcp_process_finds_the_server_through_the_session_file() {
    let fake = FakePodman::new("mp-newproc");
    let proj = project(&fake);
    let port = free_port();
    let live = {
        let first = server(&fake, &proj, 3);
        let l = LiveServer::start(proj.clone(), port, Some(("pw-n", Mode::Sentinel)));
        call(
            &first,
            "server_up",
            json!({"rconPassword": "pw-n", "port": port, "timeoutMs": 15000}),
        )
        .await
        .unwrap();
        l
    };
    // a second server instance (another MCP process) knows nothing about the launch
    let second = server(&fake, &proj, 3);
    let out = call(&second, "server_console_exec", json!({"command": "status"}))
        .await
        .unwrap();
    assert_eq!(out["response"], json!("ran status"));
    drop(live);
}

#[tokio::test]
async fn a_wrong_rcon_password_is_an_auth_failure_not_a_hang() {
    let fake = FakePodman::new("mp-auth");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    let port = free_port();
    let live = LiveServer::start(
        proj.clone(),
        port,
        Some(("the-real-one", Mode::LoneAuthResponse)),
    );
    let up = call(
        &srv,
        "server_up",
        json!({
            "rconPassword": "wrong",
            "port": port,
            "bots": {"t": 1},
            "timeoutMs": 15000
        }),
    )
    .await
    .unwrap();
    assert_eq!(up["bots"]["applied"], json!(false));
    assert_eq!(up["bots"]["phase"], json!("auth"));

    let err = call(&srv, "server_console_exec", json!({"command": "status"}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "RconAuthFailed");
    assert!(err.retry_safe);
    assert!(live.commands().is_empty());
}

#[tokio::test]
async fn a_port_that_is_bound_is_refused_before_anything_launches() {
    let fake = FakePodman::new("mp-port");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    let held = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let err = call(&srv, "server_up", json!({"port": port, "timeoutMs": 300}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "PortInUse");
    assert!(fake.calls_to("run").is_empty());
}

#[tokio::test]
async fn server_up_refuses_args_the_tool_sets_itself() {
    let fake = FakePodman::new("mp-reserved");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    for args in [
        json!(["+rcon_password", "x"]),
        json!(["+hostport", "27020"]),
        json!(["-port", "27020"]),
    ] {
        let err = call(
            &srv,
            "server_up",
            json!({"args": args, "port": free_port()}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{args}");
    }
    assert!(fake.calls_to("run").is_empty());
}

#[tokio::test]
async fn one_dedicated_server_per_home_volume_but_a_client_may_share_it() {
    let fake = FakePodman::new("mp-busy");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    fake.add_running("cs2mcp-linux-dedicated-00000001");
    fake.set_mounts("cs2mcp-linux-dedicated-00000001", HOME_VOLUME);
    let err = call(
        &srv,
        "server_up",
        json!({"port": free_port(), "timeoutMs": 300}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "KindBusy");
    assert_eq!(err.data["kind"], json!("linux-dedicated"));
    assert!(fake.calls_to("run").is_empty());

    // a running client on the same volume does not block a server
    let fake2 = FakePodman::new("mp-share");
    let proj2 = project(&fake2);
    let srv2 = server(&fake2, &proj2, 3);
    fake2.add_running("cs2mcp-linux-client-00000002");
    fake2.set_mounts("cs2mcp-linux-client-00000002", HOME_VOLUME);
    let out = call(
        &srv2,
        "server_up",
        json!({"port": free_port(), "timeoutMs": 300}),
    )
    .await
    .unwrap();
    assert!(out["container"]
        .as_str()
        .unwrap()
        .starts_with("cs2mcp-linux-dedicated-"));
    assert_eq!(fake2.calls_to("run").len(), 1);
}

#[tokio::test]
async fn launch_session_connect_appends_the_connect_arg_for_linux_clients_only() {
    let fake = FakePodman::new("mp-connect");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    let out = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "waitFor": "ContainerUp", "timeoutMs": 2000, "connect": "127.0.0.1:27015"}),
    )
    .await
    .unwrap();
    assert!(out["container"]
        .as_str()
        .unwrap()
        .starts_with("cs2mcp-linux-client-"));
    let argv = out["argv"].as_array().unwrap();
    assert_eq!(argv[argv.len() - 2], json!("+connect"));
    assert_eq!(argv[argv.len() - 1], json!("127.0.0.1:27015"));

    let err = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "connect": "no port here"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");

    let err = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "connect": "127.0.0.1:27015"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(err.message.contains("linux-client only"));
}

#[tokio::test]
async fn client_connect_on_a_linux_client_says_what_to_do_instead() {
    let fake = FakePodman::new("mp-cc");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "waitFor": "ContainerUp", "timeoutMs": 2000}),
    )
    .await
    .unwrap();
    let err = call(
        &srv,
        "client_connect",
        json!({"address": "127.0.0.1:27015"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "NotImplementedYet");
    assert!(err.data["note"].as_str().unwrap().contains("does NOT work"));

    let err = call(&srv, "client_connect", json!({"address": "not-an-address"}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
}

#[tokio::test]
async fn client_connect_refuses_a_dedicated_server_as_the_client() {
    let fake = FakePodman::new("mp-cc-srv");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    let out = call(
        &srv,
        "server_up",
        json!({"port": free_port(), "timeoutMs": 300}),
    )
    .await
    .unwrap();
    let name = out["container"].as_str().unwrap().to_string();
    let err = call(&srv, "client_connect", json!({"container": name}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
}

#[tokio::test]
async fn session_status_lists_the_multiplayer_group() {
    let fake = FakePodman::new("mp-status");
    let proj = project(&fake);
    let srv = server(&fake, &proj, 3);
    let out = call(&srv, "session_status", json!({})).await.unwrap();
    assert_eq!(out["toolGroups"]["multiplayer"]["available"], json!(true));
    assert_eq!(
        out["toolGroups"]["multiplayer"]["tools"],
        json!(["server_up", "client_connect", "server_console_exec"])
    );
}
