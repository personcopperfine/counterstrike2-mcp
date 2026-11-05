//! launch_session / wait_session / stop_session / session_status(session group) against a fake
//! podman. No environment mutation: XAUTHORITY and the podman path come in as flags.
#![allow(clippy::result_large_err)]
use client_mcp::state::ClientConfig;
use cs2_mcp_common::mock::podman::FakePodman;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

struct Opts<'a> {
    max: usize,
    xauthority: Option<&'a Path>,
    tool_budget_ms: Option<u64>,
}

impl Default for Opts<'_> {
    fn default() -> Self {
        Opts {
            max: 3,
            xauthority: None,
            tool_budget_ms: None,
        }
    }
}

/// Server wired to the fake podman, a temp project root, and a dead bridge.
fn server(
    fake: &FakePodman,
    project: &Path,
    opts: Opts,
) -> cs2_mcp_common::mcp::McpServer<client_mcp::state::ClientState> {
    let bin = fake.bin.display().to_string();
    let proj = project.display().to_string();
    let max = opts.max.to_string();
    let budget = opts.tool_budget_ms.map(|b| b.to_string());
    let xa = opts.xauthority.map(|p| p.display().to_string());
    let mut argv: Vec<&str> = vec![
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
    ];
    if let Some(x) = &xa {
        argv.extend(["--xauthority", x]);
    }
    if let Some(b) = &budget {
        argv.extend(["--tool-budget-ms", b]);
    }
    client_mcp::build_server(&config(&argv)).unwrap()
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

#[tokio::test]
async fn launch_refused_at_the_cap_with_too_many_sessions() {
    let fake = FakePodman::new("tool-cap");
    fake.add_running("cs2vtest");
    fake.add_running("cs2mcp-linux-client-00000001");
    let proj = project(&fake);
    let srv = server(
        &fake,
        &proj,
        Opts {
            max: 2,
            ..Default::default()
        },
    );
    let err = call(&srv, "launch_session", json!({"kind": "linux-client"}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "TooManySessions");
    assert_eq!(err.data["max"], json!(2));
    assert_eq!(err.data["runningContainers"].as_array().unwrap().len(), 2);
    assert!(err.data["hint"].as_str().unwrap().contains("Foreign cs2*"));
    assert!(err.retry_safe);
    assert!(fake.calls_to("run").is_empty());
}

#[tokio::test]
async fn launch_linux_client_reports_the_full_shape() {
    let fake = FakePodman::new("tool-launch");
    let proj = project(&fake);
    let srv = server(&fake, &proj, Opts::default());
    let out = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "waitFor": "ContainerUp", "timeoutMs": 5000}),
    )
    .await
    .unwrap();
    let name = out["container"].as_str().unwrap();
    assert!(name.starts_with("cs2mcp-linux-client-"), "{name}");
    assert_eq!(out["kind"], json!("linux-client"));
    assert_eq!(out["reused"], json!(false));
    assert_eq!(out["pending"], json!(false));
    assert_eq!(out["argv"][0], json!(fake.bin.display().to_string()));
    assert_eq!(out["argv"][1], json!("run"));
    assert!(out["paths"]["consoleLog"]
        .as_str()
        .unwrap()
        .ends_with("console.log"));
    let artifacts = PathBuf::from(out["artifactsDir"].as_str().unwrap());
    assert!(artifacts.is_dir());
    assert!(artifacts.starts_with(proj.join("artifacts")));
    assert_eq!(out["readiness"]["target"], json!("ContainerUp"));
    assert_eq!(out["readiness"]["targetReached"], json!(true));
    assert_eq!(out["readiness"]["reached"], json!(["ContainerUp"]));
    let runs = fake.calls_to("run");
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].xdg_runtime_dir,
        proj.join(".podman/xdg").display().to_string()
    );
    assert!(fake.is_running(name));
}

#[tokio::test]
async fn launch_is_clamped_to_the_call_budget_and_reports_pending() {
    let fake = FakePodman::new("tool-pending");
    let proj = project(&fake);
    // 3 s budget, 2 s reserved: the 900 s request becomes ~1 s; no console.log ever appears
    let srv = server(
        &fake,
        &proj,
        Opts {
            tool_budget_ms: Some(3000),
            ..Default::default()
        },
    );
    let started = Instant::now();
    let out = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "timeoutMs": 900000}),
    )
    .await
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "returned inside the budget"
    );
    assert_eq!(out["pending"], json!(true));
    assert_eq!(out["next"], json!("wait_session {container, waitFor}"));
    assert_eq!(out["readiness"]["targetReached"], json!(false));
    assert!(out["readiness"]["note"]
        .as_str()
        .unwrap()
        .contains("timed out"));
    assert!(
        fake.is_running(out["container"].as_str().unwrap()),
        "the session stays up"
    );
}

#[tokio::test]
async fn wait_session_continues_a_pending_launch() {
    let fake = FakePodman::new("tool-wait");
    let proj = project(&fake);
    let srv = server(
        &fake,
        &proj,
        Opts {
            tool_budget_ms: Some(3000),
            ..Default::default()
        },
    );
    let launched = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "waitFor": "ContainerUp"}),
    )
    .await
    .unwrap();
    assert_eq!(launched["pending"], json!(false));
    let name = launched["container"].as_str().unwrap().to_string();
    let console = PathBuf::from(launched["paths"]["consoleLog"].as_str().unwrap());
    let writer = console.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(&writer, "SwitchToLoop game\n").unwrap();
    });
    let out = call(
        &srv,
        "wait_session",
        json!({"container": name, "waitFor": "MenuReached"}),
    )
    .await
    .unwrap();
    assert_eq!(out["readiness"]["targetReached"], json!(true), "{out}");
    assert_eq!(out["pending"], json!(false));
    // default container and default stage: the same session, already satisfied
    let again = call(&srv, "wait_session", json!({})).await.unwrap();
    assert_eq!(again["container"], json!(name));
    assert_eq!(again["readiness"]["target"], json!("MenuReached"));
}

#[tokio::test]
async fn identical_launch_returns_the_running_session_with_reused_true() {
    let fake = FakePodman::new("tool-reuse");
    let proj = project(&fake);
    let srv = server(&fake, &proj, Opts::default());
    let first = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "waitFor": "ContainerUp"}),
    )
    .await
    .unwrap();
    let second = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "waitFor": "ContainerUp"}),
    )
    .await
    .unwrap();
    assert_eq!(second["reused"], json!(true));
    assert_eq!(second["container"], first["container"]);
    assert_eq!(fake.calls_to("run").len(), 1);
}

#[tokio::test]
async fn second_linux_client_on_the_same_home_volume_is_kind_busy() {
    let fake = FakePodman::new("tool-busy");
    let proj = project(&fake);
    let srv = server(&fake, &proj, Opts::default());
    let first = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "waitFor": "ContainerUp"}),
    )
    .await
    .unwrap();
    let err = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "args": ["-novid"], "waitFor": "ContainerUp"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "KindBusy");
    assert_eq!(err.data["container"], first["container"]);
    assert_eq!(err.data["kind"], json!("linux-client"));
}

#[tokio::test]
async fn launch_validates_arguments_before_touching_podman() {
    let fake = FakePodman::new("tool-validate");
    let proj = project(&fake);
    let srv = server(
        &fake,
        &proj,
        Opts {
            max: 2,
            ..Default::default()
        },
    );
    for bad in [
        json!({}),
        json!({"kind": "dedicated"}),
        json!({"kind": "linux-client", "testscript": "sandbox"}),
        json!({"kind": "wine-tools", "testscript": "../etc"}),
        json!({"kind": "linux-client", "waitFor": "Lunch"}),
        json!({"kind": "linux-client", "timeoutMs": 0}),
        json!({"kind": "linux-client", "args": "-novid"}),
    ] {
        let err = call(&srv, "launch_session", bad.clone()).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
    let err = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "headless": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "NotSupported");
    let err = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "headless": false}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "NotSupported");
    assert!(fake.calls_to("run").is_empty());
}

#[tokio::test]
async fn wine_tools_needs_the_cookie_stages_the_suite_and_parks() {
    let fake = FakePodman::new("tool-wine");
    let proj = project(&fake);

    // no cookie file: DisplayUnavailable, nothing started
    let missing = proj.join("no-such-cookie");
    let srv = server(
        &fake,
        &proj,
        Opts {
            xauthority: Some(&missing),
            ..Default::default()
        },
    );
    let err = call(&srv, "launch_session", json!({"kind": "wine-tools"}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "DisplayUnavailable");
    assert!(fake.calls_to("run").is_empty());

    let cookie = proj.join("cookie");
    std::fs::write(&cookie, "x").unwrap();
    let srv = server(
        &fake,
        &proj,
        Opts {
            xauthority: Some(&cookie),
            ..Default::default()
        },
    );

    // suite not staged: fail fast with the path in data
    let err = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "testscript": "sandbox"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "SuiteNotStaged");
    assert!(err.data["path"]
        .as_str()
        .unwrap()
        .ends_with("vtest/sandbox.txt"));
    assert!(fake.calls_to("run").is_empty());

    let vtest = proj.join("workshop/run/app/vtest");
    std::fs::create_dir_all(&vtest).unwrap();
    std::fs::write(vtest.join("sandbox.txt"), "Test_Run mark VTEST_PARKED\n").unwrap();

    // the harness parks: the marker lands in the engine's console.log (written after the launch
    // has moved any previous one aside)
    let console = proj.join("workshop/run/app/game/csgo/console.log");
    let writer = console.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::create_dir_all(writer.parent().unwrap()).unwrap();
        std::fs::write(&writer, "TESTSCRIPT: CheckPoint -> 'VTEST_PARKED'\n").unwrap();
    });
    // the channel answers (vtmsend); bridge false keeps the wait at the harness marker
    fake.set_exec_default("vtmsend: creator version 1\n", 0);
    let out = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "testscript": "sandbox", "bridge": false, "timeoutMs": 10000}),
    )
    .await
    .unwrap();
    assert_eq!(out["kind"], json!("wine-tools"));
    assert_eq!(out["readiness"]["target"], json!("Parked"));
    assert_eq!(out["readiness"]["targetReached"], json!(true));
    let argv: Vec<&str> = out["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        *argv.last().unwrap(),
        "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console -condebug -insecure -testscript /app/vtest/sandbox.txt +engine_no_focus_sleep 0 +r_always_render_all_windows 1"
    );
    assert!(argv.contains(&"--gpus"));
    assert!(argv.contains(&"XAUTHORITY=/root/.Xauthority"));
}

#[tokio::test]
async fn stop_refuses_foreign_containers() {
    let fake = FakePodman::new("tool-foreign");
    fake.add_running("cs2vtest");
    let proj = project(&fake);
    let srv = server(
        &fake,
        &proj,
        Opts {
            max: 2,
            ..Default::default()
        },
    );
    let err = call(&srv, "stop_session", json!({"container": "cs2vtest"}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "ForeignContainer");
    assert_eq!(err.data["allowedPrefix"], json!("cs2mcp-"));
    assert!(fake.calls_to("rm").is_empty());
    assert!(fake.is_running("cs2vtest"));
    let err = call(
        &srv,
        "stop_session",
        json!({"container": "cs2mcp-Bad_Name"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
}

#[tokio::test]
async fn stop_defaults_to_the_session_this_server_launched() {
    let fake = FakePodman::new("tool-stop");
    let proj = project(&fake);
    let srv = server(&fake, &proj, Opts::default());
    let err = call(&srv, "stop_session", json!({})).await.unwrap_err();
    assert_eq!(err.type_, "NoSession");

    let out = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "waitFor": "ContainerUp"}),
    )
    .await
    .unwrap();
    let name = out["container"].as_str().unwrap().to_string();
    let stopped = call(&srv, "stop_session", json!({})).await.unwrap();
    assert_eq!(stopped["stopped"], json!(name));
    assert_eq!(stopped["alreadyGone"], json!(false));
    // where the stop spent its time: issued, exited, rm issued, removed, in order
    let t = &stopped["timing"];
    assert_eq!(t["initialState"], json!("running"), "{stopped}");
    let ms = |k: &str| {
        t[k].as_u64()
            .unwrap_or_else(|| panic!("timing.{k} missing: {stopped}"))
    };
    assert!(
        ms("stopIssuedMs") <= ms("exitedMs")
            && ms("exitedMs") <= ms("rmIssuedMs")
            && ms("rmIssuedMs") <= ms("removedMs")
            && ms("removedMs") <= ms("totalMs"),
        "{stopped}"
    );
    assert!(!fake.exists(&name));
    assert_eq!(fake.calls_to("rm")[0].argv, vec!["rm", name.as_str()]);
    assert!(call(&srv, "stop_session", json!({})).await.is_err());
}

#[tokio::test]
async fn stop_accepts_an_unowned_cs2mcp_container() {
    let fake = FakePodman::new("tool-orphan");
    fake.add_running("cs2mcp-linux-client-0badf00d");
    let proj = project(&fake);
    let srv = server(
        &fake,
        &proj,
        Opts {
            max: 2,
            ..Default::default()
        },
    );
    let out = call(
        &srv,
        "stop_session",
        json!({"container": "cs2mcp-linux-client-0badf00d"}),
    )
    .await
    .unwrap();
    assert_eq!(out["stopped"], json!("cs2mcp-linux-client-0badf00d"));
    assert!(!fake.exists("cs2mcp-linux-client-0badf00d"));
}

#[tokio::test]
async fn session_group_reports_podman_availability_truthfully() {
    let fake = FakePodman::new("tool-group");
    let proj = project(&fake);
    let srv = server(
        &fake,
        &proj,
        Opts {
            max: 2,
            ..Default::default()
        },
    );
    let p = call(&srv, "session_status", json!({})).await.unwrap();
    assert_eq!(p["toolGroups"]["session"]["available"], json!(true), "{p}");
    assert_eq!(p["runtime"]["maxSessions"], json!(2));

    let cfg = config(&[
        "--discovery",
        "/tmp/cs2mcp-no-bridge/session.json",
        "--podman",
        "/nonexistent/podman-binary",
    ]);
    let srv = client_mcp::build_server(&cfg).unwrap();
    let p = call(&srv, "session_status", json!({})).await.unwrap();
    assert_eq!(p["toolGroups"]["session"]["available"], json!(false));
    assert!(p["toolGroups"]["session"]["reason"]
        .as_str()
        .unwrap()
        .contains("podman"));
}

#[tokio::test]
async fn exited_container_is_reported_with_its_exit_code() {
    let fake = FakePodman::new("tool-exit");
    let proj = project(&fake);
    let srv = server(&fake, &proj, Opts::default());
    let out = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "waitFor": "ContainerUp"}),
    )
    .await
    .unwrap();
    let name = out["container"].as_str().unwrap().to_string();
    fake.set_state(&name, "exited 137");
    fake.set_logs(&name, "boot failed\n");
    let waited = call(
        &srv,
        "wait_session",
        json!({"container": name, "waitFor": "MenuReached"}),
    )
    .await
    .unwrap();
    assert_eq!(waited["pending"], json!(false));
    assert_eq!(waited["readiness"]["exited"]["exitCode"], json!(137));
    assert_eq!(
        waited["readiness"]["exited"]["logsTail"],
        json!(["boot failed"])
    );
}

#[tokio::test]
async fn launch_and_stop_are_not_stubs_any_more() {
    let mb = MockBridge::start().await.unwrap();
    let cfg = config(&["--discovery", mb.discovery_path.to_str().unwrap()]);
    let srv = client_mcp::build_server(&cfg).unwrap();
    // stop refuses foreign names before any podman call
    let err = call(&srv, "stop_session", json!({"container": "cs2vtest"}))
        .await
        .unwrap_err();
    assert_eq!(err.type_, "ForeignContainer");
}

#[tokio::test]
async fn wine_map_stages_the_per_map_harness_and_launches_it_with_the_addon() {
    let fake = FakePodman::new("tool-wine-map");
    let proj = project(&fake);
    let app = proj.join("workshop/run/app");
    std::fs::create_dir_all(&app).unwrap();
    let srv = server(&fake, &proj, Opts::default());
    let out = call(
        &srv,
        "launch_session",
        json!({
            "kind": "wine-tools",
            "map": "editor/zoo/smartprop_zoo",
            "addon": "cs_script_demo",
            "waitFor": "ContainerUp",
            "timeoutMs": 5000,
        }),
    )
    .await
    .unwrap();
    let staged = app.join("vtest/harness-editor--zoo--smartprop_zoo.txt");
    assert_eq!(out["map"], json!("editor/zoo/smartprop_zoo"));
    assert_eq!(out["addon"], json!("cs_script_demo"));
    assert_eq!(out["harness"], json!("harness-editor--zoo--smartprop_zoo"));
    assert_eq!(out["stagedPath"], json!(staged.display().to_string()));

    let text = std::fs::read_to_string(&staged).unwrap();
    // the addon is not this map's own (no csgo_addons/cs_script_demo/maps file): plain map
    assert!(text.contains("\nmap editor/zoo/smartprop_zoo\n"), "{text}");
    assert!(!text.contains("map_workshop"), "{text}");
    assert_eq!(out["mapCommand"], json!("map"));
    assert!(text.contains("Test_Run mark VTEST_MAP_LOADED\n"), "{text}");
    assert!(text.ends_with("quit\nTest_Wait 30\nTest_ExitProcess 3\n"));
    // the fragments the harness calls are staged next to it, verbatim
    let setup = std::fs::read_to_string(app.join("vtest/setup_player.vtest")).unwrap();
    assert!(setup.contains("mp_round_restart_delay 0"));

    let argv: Vec<&str> = out["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        *argv.last().unwrap(),
        "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console -condebug -insecure -addon cs_script_demo -testscript /app/vtest/harness-editor--zoo--smartprop_zoo.txt +engine_no_focus_sleep 0 +r_always_render_all_windows 1"
    );
}

#[tokio::test]
async fn map_and_addon_are_validated_before_podman_or_disk() {
    let fake = FakePodman::new("tool-map-validate");
    let proj = project(&fake);
    let app = proj.join("workshop/run/app");
    std::fs::create_dir_all(&app).unwrap();
    let srv = server(&fake, &proj, Opts::default());
    let too_long = "a".repeat(129);
    let too_long_addon = "a".repeat(65);
    for bad in [
        json!({"kind": "wine-tools", "map": "../x"}),
        json!({"kind": "wine-tools", "map": "a//b"}),
        json!({"kind": "wine-tools", "map": "C:\\x"}),
        json!({"kind": "wine-tools", "map": "/x"}),
        json!({"kind": "wine-tools", "map": too_long}),
        json!({"kind": "wine-tools", "map": 7}),
        json!({"kind": "wine-tools", "map": "de_dust2", "testscript": "sandbox"}),
        json!({"kind": "wine-tools", "addon": ""}),
        json!({"kind": "wine-tools", "addon": "cs-script"}),
        json!({"kind": "wine-tools", "addon": too_long_addon}),
        json!({"kind": "linux-client", "map": "de_dust2"}),
        json!({"kind": "linux-client", "addon": "cs_script_demo"}),
    ] {
        let err = call(&srv, "launch_session", bad.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
    }
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    assert!(!app.join("vtest").exists());
}

/// Methods the mock bridge advertises for the map checks (hello and game.local).
const MAP_BRIDGE_METHODS: [&str; 5] = [
    "control.ping",
    "control.status",
    "control.capabilities",
    "control.version",
    "game.local",
];

/// True once the fake podman has started a wine-tools container (its state file exists).
fn wine_started(fake_state: &Path) -> bool {
    std::fs::read_dir(fake_state).is_ok_and(|d| {
        d.flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("cs2mcp-wine-tools-")
        })
    })
}

/// Plays the engine once the fake podman has started the container: optionally starts a mock bridge
/// in `bridge_dir` (its discovery file is then fresh for the launch), then writes console.log. The
/// poll is on the container's state file, with a deadline; the mock is returned so the test keeps
/// it alive until the launch has used it.
fn engine_once_started(
    fake_state: PathBuf,
    console: PathBuf,
    console_text: &'static [u8],
    bridge: Option<(PathBuf, Reply)>,
) -> tokio::task::JoinHandle<Option<MockBridge>> {
    tokio::spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !wine_started(&fake_state) {
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mock = match bridge {
            Some((dir, reply)) => {
                let builder = MockBridge::builder().methods(MAP_BRIDGE_METHODS);
                let builder = match reply {
                    // the n-th game.local gets answers[n], the last one from then on
                    Reply::Answers(answers) => {
                        let answers = Arc::new(answers);
                        let calls = Arc::new(AtomicUsize::new(0));
                        builder.responder("game.local", move |_| {
                            let n = calls.fetch_add(1, Ordering::SeqCst).min(answers.len() - 1);
                            answers[n].clone()
                        })
                    }
                    Reply::Error(code, name) => {
                        builder.error("game.local", code, name, Some(json!({"reason": "mock"})))
                    }
                };
                Some(builder.discovery_dir(dir).start().await.unwrap())
            }
            None => None,
        };
        std::fs::create_dir_all(console.parent().unwrap()).unwrap();
        std::fs::write(&console, console_text).unwrap();
        mock
    })
}

/// What the mock bridge answers to `game.local`.
enum Reply {
    Answers(Vec<Value>),
    /// every game.local fails with this wire code and name
    Error(i64, &'static str),
}

/// A parked harness: the marker line the wait for Parked reads.
const PARKED_LOG: &[u8] = b"TESTSCRIPT: CheckPoint -> 'VTEST_PARKED'\n";

/// The game's answer for a player on `map`.
fn in_game(map: &str) -> Value {
    json!({"tick": 5, "inGame": true, "map": map})
}

/// The answer for a session that is not in a game (the menu, or a map that did not load).
fn not_in_game(map: &str) -> Value {
    json!({"tick": 0, "inGame": false, "map": map})
}

/// Writes an addon's own map where the harness looks for it (`map_workshop` only loads those).
fn own_addon_map(proj: &Path, addon: &str, map: &str) {
    let dir = proj
        .join("workshop/run/app/game/csgo_addons")
        .join(addon)
        .join("maps");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{map}.vpk")), "vpk").unwrap();
}

#[tokio::test]
async fn an_engine_refusal_of_the_map_is_map_load_failed_even_when_the_harness_parks() {
    let fake = FakePodman::new("tool-map-refused-engine");
    let proj = project(&fake);
    std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
    let srv = server(&fake, &proj, Opts::default());
    fake.set_exec_default("vtmsend: creator version 1\n", 0);
    let console = proj.join("workshop/run/app/game/csgo/console.log");
    // live: the harness parks after `map <invalid>`, so the marker and the refusal are both there
    let engine = engine_once_started(
        fake.dir().join("state"),
        console,
        b"map cs_script_demo:  invalid map name\nTESTSCRIPT: CheckPoint -> 'VTEST_PARKED'\n",
        None,
    );
    let err = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "map": "cs_script_demo", "addon": "cs_script_demo", "bridge": false, "timeoutMs": 20000}),
    )
    .await
    .unwrap_err();
    engine.await.unwrap();
    assert_eq!(err.type_, "MapLoadFailed");
    assert!(
        !err.retry_safe,
        "relaunching the same args reuses the session"
    );
    assert!(err.message.contains("left running"), "{}", err.message);
    assert_eq!(err.data["map"], json!("cs_script_demo"));
    assert_eq!(err.data["addon"], json!("cs_script_demo"));
    assert!(err.data["evidence"][0]
        .as_str()
        .unwrap()
        .contains("map cs_script_demo:  invalid map name"));
    let container = err.data["container"].as_str().unwrap();
    assert!(container.starts_with("cs2mcp-wine-tools-"));
    assert_eq!(err.data["next"], json!("stop_session {container}"));
    assert_eq!(err.data["readiness"]["targetReached"], json!(true));
    assert!(err.data["argv"].as_array().is_some_and(|a| !a.is_empty()));
}

#[tokio::test]
async fn without_a_bridge_the_parked_map_is_reported_unverified_not_failed() {
    let fake = FakePodman::new("tool-map-nobridge");
    let proj = project(&fake);
    std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
    let srv = server(&fake, &proj, Opts::default());
    fake.set_exec_default("vtmsend: creator version 1\n", 0);
    let console = proj.join("workshop/run/app/game/csgo/console.log");
    let engine = engine_once_started(fake.dir().join("state"), console, PARKED_LOG, None);
    let out = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "map": "de_dust2", "bridge": false, "timeoutMs": 20000}),
    )
    .await
    .unwrap();
    engine.await.unwrap();
    assert_eq!(out["readiness"]["targetReached"], json!(true));
    assert_eq!(out["mapVerified"], json!(false));
    assert!(out["mapNote"].as_str().unwrap().contains("bridge false"));
    assert!(out.get("mapSeen").is_none());
}

#[tokio::test]
async fn a_game_local_on_another_map_is_map_load_failed() {
    let fake = FakePodman::new("tool-map-mismatch");
    let proj = project(&fake);
    std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
    let disc_dir = wine_bridge_dir(&proj);
    let srv = server_with_bridge(&fake, &proj, &disc_dir.join("session.json"));
    fake.set_exec_default("vtmsend: creator version 1\n", 0);
    let engine = engine_once_started(
        fake.dir().join("state"),
        proj.join("workshop/run/app/game/csgo/console.log"),
        PARKED_LOG,
        Some((disc_dir, Reply::Answers(vec![in_game("de_dust2")]))),
    );
    let err = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "map": "cs_script_demo", "timeoutMs": 20000}),
    )
    .await
    .unwrap_err();
    let mock = engine.await.unwrap().expect("the mock bridge came up");
    assert_eq!(err.type_, "MapLoadFailed");
    assert!(!err.retry_safe);
    assert_eq!(err.data["seen"]["map"], json!("de_dust2"));
    assert_eq!(err.data["seen"]["inGame"], json!(true));
    assert_eq!(err.data["map"], json!("cs_script_demo"));
    assert!(
        mock.method_calls("game.local").len() >= 2,
        "polled, not asked once"
    );
}

#[tokio::test]
async fn a_player_out_of_game_for_the_whole_window_is_map_load_failed() {
    let fake = FakePodman::new("tool-map-not-in-game");
    let proj = project(&fake);
    std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
    let disc_dir = wine_bridge_dir(&proj);
    let srv = server_with_bridge(&fake, &proj, &disc_dir.join("session.json"));
    fake.set_exec_default("vtmsend: creator version 1\n", 0);
    let engine = engine_once_started(
        fake.dir().join("state"),
        proj.join("workshop/run/app/game/csgo/console.log"),
        PARKED_LOG,
        Some((
            disc_dir,
            Reply::Answers(vec![not_in_game("cs_script_demo")]),
        )),
    );
    let err = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "map": "cs_script_demo", "timeoutMs": 20000}),
    )
    .await
    .unwrap_err();
    engine.await.unwrap();
    assert_eq!(err.type_, "MapLoadFailed");
    assert_eq!(err.data["seen"]["inGame"], json!(false));
}

#[tokio::test]
async fn no_local_player_for_the_whole_window_is_map_load_failed() {
    let fake = FakePodman::new("tool-map-no-local");
    let proj = project(&fake);
    std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
    let disc_dir = wine_bridge_dir(&proj);
    let srv = server_with_bridge(&fake, &proj, &disc_dir.join("session.json"));
    fake.set_exec_default("vtmsend: creator version 1\n", 0);
    let engine = engine_once_started(
        fake.dir().join("state"),
        proj.join("workshop/run/app/game/csgo/console.log"),
        PARKED_LOG,
        Some((disc_dir, Reply::Error(-32024, "NoLocalPlayer"))),
    );
    let err = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "map": "cs_script_demo", "addon": "cs_script_demo", "timeoutMs": 20000}),
    )
    .await
    .unwrap_err();
    engine.await.unwrap();
    assert_eq!(err.type_, "MapLoadFailed");
    assert_eq!(err.data["seen"]["error"], json!("NoLocalPlayer"));
}

#[tokio::test]
async fn an_error_that_says_nothing_about_the_map_is_unverified_not_failed() {
    let fake = FakePodman::new("tool-map-schema");
    let proj = project(&fake);
    std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
    let disc_dir = wine_bridge_dir(&proj);
    let srv = server_with_bridge(&fake, &proj, &disc_dir.join("session.json"));
    fake.set_exec_default("vtmsend: creator version 1\n", 0);
    let engine = engine_once_started(
        fake.dir().join("state"),
        proj.join("workshop/run/app/game/csgo/console.log"),
        PARKED_LOG,
        Some((disc_dir, Reply::Error(-32023, "SchemaUnavailable"))),
    );
    let out = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "map": "de_dust2", "timeoutMs": 20000}),
    )
    .await
    .unwrap();
    engine.await.unwrap();
    assert_eq!(out["mapVerified"], json!(false), "{out}");
    assert!(
        out["mapNote"]
            .as_str()
            .unwrap()
            .contains("SchemaUnavailable"),
        "{out}"
    );
}

#[tokio::test]
async fn a_map_that_comes_into_game_within_the_window_is_verified() {
    let fake = FakePodman::new("tool-map-late");
    let proj = project(&fake);
    std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
    let disc_dir = wine_bridge_dir(&proj);
    let srv = server_with_bridge(&fake, &proj, &disc_dir.join("session.json"));
    fake.set_exec_default("vtmsend: creator version 1\n", 0);
    let engine = engine_once_started(
        fake.dir().join("state"),
        proj.join("workshop/run/app/game/csgo/console.log"),
        PARKED_LOG,
        Some((
            disc_dir,
            Reply::Answers(vec![
                not_in_game(""),
                not_in_game(""),
                in_game("cs_script_demo"),
            ]),
        )),
    );
    let out = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "map": "cs_script_demo", "timeoutMs": 20000}),
    )
    .await
    .unwrap();
    let mock = engine.await.unwrap().expect("the mock bridge came up");
    assert_eq!(out["mapVerified"], json!(true));
    assert_eq!(out["mapSeen"]["map"], json!("cs_script_demo"));
    assert!(mock.method_calls("game.local").len() >= 3);
}

#[tokio::test]
async fn an_addons_own_map_loads_with_map_workshop_and_is_verified() {
    let fake = FakePodman::new("tool-map-verified");
    let proj = project(&fake);
    let app = proj.join("workshop/run/app");
    std::fs::create_dir_all(&app).unwrap();
    own_addon_map(&proj, "cs_script_demo", "cs_script_demo");
    let disc_dir = wine_bridge_dir(&proj);
    let srv = server_with_bridge(&fake, &proj, &disc_dir.join("session.json"));
    fake.set_exec_default("vtmsend: creator version 1\n", 0);
    let engine = engine_once_started(
        fake.dir().join("state"),
        app.join("game/csgo/console.log"),
        PARKED_LOG,
        Some((disc_dir, Reply::Answers(vec![in_game("cs_script_demo")]))),
    );
    let out = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "map": "cs_script_demo", "addon": "cs_script_demo", "timeoutMs": 20000}),
    )
    .await
    .unwrap();
    let mock = engine.await.unwrap().expect("the mock bridge came up");
    assert_eq!(out["mapVerified"], json!(true));
    assert_eq!(out["mapCommand"], json!("map_workshop"));
    assert_eq!(out["mapSeen"]["map"], json!("cs_script_demo"));
    assert_eq!(
        out["harness"],
        json!("harness-cs_script_demo--cs_script_demo")
    );
    let staged =
        std::fs::read_to_string(app.join("vtest/harness-cs_script_demo--cs_script_demo.txt"))
            .unwrap();
    assert!(
        staged.contains("\nmap_workshop cs_script_demo cs_script_demo\n"),
        "{staged}"
    );
    assert!(!mock.method_calls("game.local").is_empty());
}

#[tokio::test]
async fn an_addon_with_a_map_that_is_not_its_own_loads_with_plain_map() {
    let fake = FakePodman::new("tool-map-not-own");
    let proj = project(&fake);
    let app = proj.join("workshop/run/app");
    std::fs::create_dir_all(&app).unwrap();
    let srv = server(&fake, &proj, Opts::default());
    fake.set_exec_default("vtmsend: creator version 1\n", 0);
    let engine = engine_once_started(
        fake.dir().join("state"),
        app.join("game/csgo/console.log"),
        PARKED_LOG,
        None,
    );
    let out = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "map": "de_dust2", "addon": "cs_script_demo", "bridge": false, "timeoutMs": 20000}),
    )
    .await
    .unwrap();
    engine.await.unwrap();
    assert_eq!(out["mapCommand"], json!("map"));
    assert_eq!(out["harness"], json!("harness-de_dust2"));
    let staged = std::fs::read_to_string(app.join("vtest/harness-de_dust2.txt")).unwrap();
    assert!(staged.contains("\nmap de_dust2\n"), "{staged}");
    assert!(!staged.contains("map_workshop"), "{staged}");
    // the addon still rides on the engine line
    assert_eq!(out["addon"], json!("cs_script_demo"));
}

#[tokio::test]
async fn a_reused_session_is_neither_scanned_nor_polled() {
    let fake = FakePodman::new("tool-map-reused");
    let proj = project(&fake);
    let app = proj.join("workshop/run/app");
    std::fs::create_dir_all(&app).unwrap();
    let srv = server(&fake, &proj, Opts::default());
    let args =
        json!({"kind": "wine-tools", "map": "de_dust2", "bridge": false, "waitFor": "ContainerUp"});
    let first = call(&srv, "launch_session", args.clone()).await.unwrap();
    assert_eq!(first["mapVerified"], json!(false));
    // a manual `map` mistake in the session someone keeps using, after the first launch
    let console = app.join("game/csgo/console.log");
    std::fs::create_dir_all(console.parent().unwrap()).unwrap();
    std::fs::write(&console, "map de_dust2:  invalid map name\n").unwrap();
    let second = call(&srv, "launch_session", args).await.unwrap();
    assert_eq!(second["reused"], json!(true));
    assert_eq!(second["mapVerified"], json!(false));
    assert!(second["mapNote"]
        .as_str()
        .unwrap()
        .contains("reused session"));
}

#[tokio::test]
async fn a_refusal_after_the_parked_mark_is_not_this_launchs_failure() {
    let fake = FakePodman::new("tool-map-late-refusal");
    let proj = project(&fake);
    std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
    let srv = server(&fake, &proj, Opts::default());
    fake.set_exec_default("vtmsend: creator version 1\n", 0);
    let engine = engine_once_started(
        fake.dir().join("state"),
        proj.join("workshop/run/app/game/csgo/console.log"),
        b"TESTSCRIPT: CheckPoint -> 'VTEST_PARKED'\nmap cs_script_demo:  invalid map name\n",
        None,
    );
    let out = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "map": "cs_script_demo", "bridge": false, "timeoutMs": 20000}),
    )
    .await
    .unwrap();
    engine.await.unwrap();
    assert_eq!(out["mapVerified"], json!(false));
}

#[tokio::test]
async fn a_stray_byte_in_console_log_does_not_hide_the_refusal() {
    let fake = FakePodman::new("tool-map-bytes");
    let proj = project(&fake);
    std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
    let srv = server(&fake, &proj, Opts::default());
    fake.set_exec_default("vtmsend: creator version 1\n", 0);
    let engine = engine_once_started(
        fake.dir().join("state"),
        proj.join("workshop/run/app/game/csgo/console.log"),
        b"\xff\xfe engine noise\nmap cs_script_demo:  invalid map name\nTESTSCRIPT: CheckPoint -> 'VTEST_PARKED'\n",
        None,
    );
    let err = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "map": "cs_script_demo", "bridge": false, "timeoutMs": 20000}),
    )
    .await
    .unwrap_err();
    engine.await.unwrap();
    assert_eq!(err.type_, "MapLoadFailed");
}

/// Mounts of a running wine-tools container: its app dir bound at /app, as podman inspect reports.
fn wine_mounts(proj: &Path) -> String {
    let app = proj.join("workshop/run/app");
    json!([{"Type": "bind", "Name": "", "Source": app.display().to_string(), "Destination": "/app"}])
        .to_string()
}

#[tokio::test]
async fn a_runtime_refusal_stages_no_harness() {
    let fake = FakePodman::new("tool-map-refused");
    let proj = project(&fake);
    let app = proj.join("workshop/run/app");
    std::fs::create_dir_all(&app).unwrap();
    let srv = server(&fake, &proj, Opts::default());
    // headless wine-tools is refused by the runtime's own validation, after the arguments pass
    let err = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "map": "de_dust2", "headless": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "NotSupported");
    assert!(
        !app.join("vtest").exists(),
        "a refused launch staged a harness"
    );
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

#[tokio::test]
async fn an_identical_map_launch_reuses_the_running_session() {
    let fake = FakePodman::new("tool-map-reuse");
    let proj = project(&fake);
    std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
    let srv = server(&fake, &proj, Opts::default());
    let args = json!({"kind": "wine-tools", "map": "de_dust2", "waitFor": "ContainerUp"});
    let first = call(&srv, "launch_session", args.clone()).await.unwrap();
    fake.set_mounts(first["container"].as_str().unwrap(), &wine_mounts(&proj));
    let second = call(&srv, "launch_session", args).await.unwrap();
    assert_eq!(second["reused"], json!(true));
    assert_eq!(second["container"], first["container"]);
    assert_eq!(fake.calls_to("run").len(), 1);
}

#[tokio::test]
async fn a_different_map_or_addon_while_one_runs_is_kind_busy() {
    let fake = FakePodman::new("tool-map-busy");
    let proj = project(&fake);
    let app = proj.join("workshop/run/app");
    std::fs::create_dir_all(&app).unwrap();
    let srv = server(&fake, &proj, Opts::default());
    let first = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "map": "de_dust2", "waitFor": "ContainerUp"}),
    )
    .await
    .unwrap();
    fake.set_mounts(first["container"].as_str().unwrap(), &wine_mounts(&proj));
    for other in [
        json!({"kind": "wine-tools", "map": "cs_script_demo", "waitFor": "ContainerUp"}),
        json!({"kind": "wine-tools", "map": "de_dust2", "addon": "cs_script_demo", "waitFor": "ContainerUp"}),
    ] {
        let err = call(&srv, "launch_session", other.clone())
            .await
            .unwrap_err();
        assert_eq!(err.type_, "KindBusy", "{other}");
        assert_eq!(err.data["container"], first["container"], "{other}");
    }
    // the refused map was not staged, and only the first run started
    assert!(!app.join("vtest/harness-cs_script_demo.txt").exists());
    assert_eq!(fake.calls_to("run").len(), 1);
}

/// A running wine-tools container the way podman reports it: its app dir bind-mounted at /app.
fn running_wine(fake: &FakePodman, proj: &Path, name: &str) {
    fake.add_running(name);
    let app = proj.join("workshop/run/app");
    fake.set_mounts(
        name,
        &json!([{"Type": "bind", "Name": "", "Source": app.display().to_string(), "Destination": "/app"}])
            .to_string(),
    );
}

/// Where the game's bridge writes its discovery file: inside the wine prefix of the app dir.
fn wine_bridge_dir(proj: &Path) -> PathBuf {
    proj.join(
        "workshop/run/app/compatdata/730/pfx/drive_c/users/steamuser/AppData/Local/cs2mcpbridge",
    )
}

/// Server wired to the fake podman and to a mock bridge's discovery file.
fn server_with_bridge(
    fake: &FakePodman,
    project: &Path,
    discovery: &Path,
) -> cs2_mcp_common::mcp::McpServer<client_mcp::state::ClientState> {
    let bin = fake.bin.display().to_string();
    let proj = project.display().to_string();
    let disc = discovery.display().to_string();
    client_mcp::build_server(&config(&[
        "--discovery",
        disc.as_str(),
        "--podman",
        bin.as_str(),
        "--project-root",
        proj.as_str(),
        "--max-sessions",
        "3",
    ]))
    .unwrap()
}

/// Writes the engine's console.log once the launch has moved any previous one aside (the
/// `ConsoleLogExists` stage every wine-tools wait passes first).
fn write_console_after_launch(proj: &Path) {
    let console = proj.join("workshop/run/app/game/csgo/console.log");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::create_dir_all(console.parent().unwrap()).unwrap();
        std::fs::write(&console, "").unwrap();
    });
}

#[tokio::test]
async fn wine_tools_waits_for_the_bridge_by_default_and_names_the_channel_when_it_is_down() {
    let fake = FakePodman::new("tool-wine-default-wait");
    let proj = project(&fake);
    let xa = proj.join("xauth");
    std::fs::write(&xa, "cookie").unwrap();
    std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
    let srv = server(
        &fake,
        &proj,
        Opts {
            xauthority: Some(&xa),
            tool_budget_ms: Some(6000),
            ..Default::default()
        },
    );
    // vtmsend before toolframework2 Init: the channel never comes up in this test
    fake.set_exec_default(
        "vtmsend: OpenMutex failed - is a -tools session running?\n",
        1,
    );
    write_console_after_launch(&proj);
    let out = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "timeoutMs": 1000}),
    )
    .await
    .unwrap();
    assert_eq!(out["readiness"]["target"], json!("BridgeHello"));
    assert_eq!(out["readiness"]["targetReached"], json!(false));
    assert_eq!(out["pending"], json!(true));
    let note = out["readiness"]["note"].as_str().unwrap();
    assert!(note.contains("MenuReached"), "{note}");
    assert!(!fake.calls_to("exec").is_empty(), "the channel was probed");
}

#[tokio::test]
async fn bridge_false_stops_the_wait_at_the_channel_and_lists_bridge_hello_unchecked() {
    let fake = FakePodman::new("tool-wine-no-bridge");
    let proj = project(&fake);
    let xa = proj.join("xauth");
    std::fs::write(&xa, "cookie").unwrap();
    std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
    let srv = server(
        &fake,
        &proj,
        Opts {
            xauthority: Some(&xa),
            ..Default::default()
        },
    );
    fake.set_exec_default("vtmsend: creator version 1\n", 0);
    write_console_after_launch(&proj);
    // the default target is the channel; asking for BridgeHello reports it as not checked
    let out = call(
        &srv,
        "launch_session",
        json!({"kind": "wine-tools", "bridge": false, "waitFor": "BridgeHello", "timeoutMs": 5000}),
    )
    .await
    .unwrap();
    assert_eq!(out["readiness"]["target"], json!("BridgeHello"));
    assert_eq!(out["readiness"]["targetReached"], json!(false));
    assert!(out["readiness"]["note"]
        .as_str()
        .unwrap()
        .contains("bridge: false"));
    assert!(out["readiness"]["reached"]
        .as_array()
        .unwrap()
        .contains(&json!("MenuReached")));
    let unchecked: Vec<&str> = out["readiness"]["unchecked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["stage"].as_str().unwrap())
        .collect();
    assert_eq!(unchecked, vec!["BridgeHello"]);
    // the bridge was never asked for: no tool_request in the payload file
    let payload = std::fs::read_to_string(proj.join("workshop/run/app/vtm_payload.kv3")).unwrap();
    assert!(!payload.contains("tool_request"), "{payload}");
}

#[tokio::test]
async fn a_slow_stop_is_a_pending_report_not_a_podman_timeout_and_the_next_call_finishes_it() {
    let fake = FakePodman::new("tool-slow-stop");
    let proj = project(&fake);
    // 16 s budget: about 4 s of it is left for the stop's wait (2 s reserve, 10 s podman timeout)
    let srv = server(
        &fake,
        &proj,
        Opts {
            tool_budget_ms: Some(16000),
            ..Default::default()
        },
    );
    let launched = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "waitFor": "ContainerUp"}),
    )
    .await
    .unwrap();
    let name = launched["container"].as_str().unwrap().to_string();
    fake.set_slow_stop(&name, 6);

    let first = call(&srv, "stop_session", json!({})).await.unwrap();
    assert_eq!(first["stopped"], json!(false), "{first}");
    assert_eq!(first["pending"], json!(true), "{first}");
    assert_eq!(first["state"], json!("stopping"), "{first}");
    assert_eq!(first["container"], json!(name));
    assert_eq!(first["next"], json!("stop_session {container}"));
    assert!(first["stopIssuedMs"].is_number(), "{first}");
    assert!(first["timing"]["stopIssuedMs"].is_number(), "{first}");
    assert!(first["timing"]["exitedMs"].is_null(), "{first}");
    assert!(fake.exists(&name), "still there while it stops");
    assert_eq!(fake.calls_to("stop").len(), 1);

    // the next call waits for the same stop (not a second one) and removes the container
    let second = call(&srv, "stop_session", json!({})).await.unwrap();
    assert_eq!(second["stopped"], json!(name), "{second}");
    assert_eq!(second["alreadyGone"], json!(false));
    assert_eq!(fake.calls_to("stop").len(), 1, "stop issued once");
    assert!(!fake.exists(&name));
}

/// Methods the mock bridge advertises for these tests (input hook plus session plumbing).
const BRIDGE_METHODS: &[&str] = &[
    "control.ping",
    "control.status",
    "control.capabilities",
    "control.version",
    "doc.active",
    "input.set",
    "input.clear",
    "input.status",
    "input.uninstall",
];

#[tokio::test]
async fn stop_reports_already_gone_when_the_container_was_removed_outside() {
    let fake = FakePodman::new("tool-gone");
    let proj = project(&fake);
    let srv = server(&fake, &proj, Opts::default());
    let launched = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "waitFor": "ContainerUp"}),
    )
    .await
    .unwrap();
    let name = launched["container"].as_str().unwrap().to_string();
    // removed behind this server's back: `ps -a` no longer lists it
    fake.remove(&name);
    let rm_before = fake.calls_to("rm").len();
    let out = call(&srv, "stop_session", json!({})).await.unwrap();
    assert_eq!(out["stopped"], json!(name));
    assert_eq!(out["alreadyGone"], json!(true));
    assert_eq!(
        fake.calls_to("rm").len(),
        rm_before,
        "nothing listed, so nothing to remove"
    );
    // the owned entry is dropped either way
    assert!(call(&srv, "stop_session", json!({})).await.is_err());
}

#[tokio::test]
async fn session_status_lists_owned_and_discovered_sessions() {
    let fake = FakePodman::new("tool-sessions");
    let proj = project(&fake);
    let srv = server(
        &fake,
        &proj,
        Opts {
            max: 4,
            ..Default::default()
        },
    );
    let launched = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "waitFor": "ContainerUp"}),
    )
    .await
    .unwrap();
    let owned = launched["container"].as_str().unwrap().to_string();

    // discovered Linux session: a container this server did not launch, artifacts mounted at /work
    let linux = "cs2mcp-linux-client-feedface";
    let work = proj.join("artifacts/cs2mcp-linux-client-feedface-1");
    std::fs::create_dir_all(&work).unwrap();
    fake.add_running(linux);
    fake.set_mounts(
        linux,
        &json!([{"Type": "bind", "Name": "", "Source": work.display().to_string(), "Destination": "/work"}]).to_string(),
    );
    // discovered Wine session: the app dir mounted at /app
    let wine = "cs2mcp-wine-tools-0badf00d";
    let app = proj.join("workshop/run/app");
    fake.add_running(wine);
    fake.set_mounts(
        wine,
        &json!([{"Type": "bind", "Name": "", "Source": app.display().to_string(), "Destination": "/app"}]).to_string(),
    );
    // a stopped session keeps its logs and is still listed
    let exited = "cs2mcp-linux-client-deadbeef";
    fake.set_state(exited, "exited 0");
    fake.set_mounts(exited, "[]");

    let p = call(&srv, "session_status", json!({})).await.unwrap();
    let sessions = p["sessions"].as_array().unwrap().clone();
    let find = |n: &str| {
        sessions
            .iter()
            .find(|s| s["container"] == json!(n))
            .unwrap_or_else(|| panic!("{n} missing from {p}"))
            .clone()
    };

    let o = find(&owned);
    assert_eq!(o["owned"], json!(true));
    assert_eq!(o["kind"], json!("linux-client"));
    assert_eq!(o["state"], json!("running"));
    assert!(o["artifactsDir"]
        .as_str()
        .unwrap()
        .starts_with(&proj.join("artifacts").display().to_string()));
    assert!(o["consoleLog"].as_str().unwrap().ends_with("console.log"));

    let d = find(linux);
    assert_eq!(d["owned"], json!(false));
    assert_eq!(d["kind"], json!("linux-client"));
    assert_eq!(d["artifactsDir"], json!(work.display().to_string()));
    assert_eq!(
        d["consoleLog"],
        json!(work.join("console.log").display().to_string())
    );

    let w = find(wine);
    assert_eq!(w["kind"], json!("wine-tools"));
    assert_eq!(w["artifactsDir"], Value::Null);
    assert_eq!(
        w["consoleLog"],
        json!(app.join("game/csgo/console.log").display().to_string())
    );

    let e = find(exited);
    assert_eq!(e["state"], json!("exited"));

    assert_eq!(p["toolGroups"]["session"]["available"], json!(true), "{p}");
    assert!(p.get("sessionsNote").is_none());
    assert_eq!(p["runtime"]["consoleLogSource"], json!("flag"));
}

#[tokio::test]
async fn session_status_never_raises_when_podman_cannot_list() {
    let fake = FakePodman::new("tool-sessions-down");
    let proj = project(&fake);
    let cfg = config(&[
        "--discovery",
        "/tmp/cs2mcp-no-bridge/session.json",
        "--podman",
        "/nonexistent/podman-binary",
        "--project-root",
        proj.to_str().unwrap(),
    ]);
    let srv = client_mcp::build_server(&cfg).unwrap();
    let p = call(&srv, "session_status", json!({})).await.unwrap();
    assert_eq!(p["sessions"], json!([]));
    assert!(p["sessionsNote"]
        .as_str()
        .unwrap()
        .contains("podman-binary"));
    assert_eq!(p["toolGroups"]["session"]["available"], json!(false));
}

#[tokio::test]
async fn stop_clears_the_input_hook_before_removing_the_container() {
    let fake = FakePodman::new("tool-input");
    let proj = project(&fake);
    let name = "cs2mcp-wine-tools-0badf00d";
    running_wine(&fake, &proj, name);
    let container_file = fake.dir().join("state").join(name);
    let alive_at_clear = Arc::new(AtomicBool::new(false));
    let probe = Arc::clone(&alive_at_clear);
    let file = container_file.clone();
    let mb = MockBridge::builder()
        .discovery_dir(wine_bridge_dir(&proj))
        .methods(BRIDGE_METHODS.iter().copied())
        .responder("input.clear", move |_| {
            probe.store(file.exists(), Ordering::SeqCst);
            json!({"cleared": true})
        })
        .responder_value("input.uninstall", json!({"uninstalled": true}))
        .start()
        .await
        .unwrap();
    let srv = server_with_bridge(&fake, &proj, &mb.discovery_path);
    // a session_status connects the bridge, as it would in a live session
    call(&srv, "session_status", json!({})).await.unwrap();

    let out = call(&srv, "stop_session", json!({"container": name}))
        .await
        .unwrap();
    assert_eq!(out["inputCleared"], json!(true), "{out}");
    assert_eq!(out["inputUninstalled"], json!(true), "{out}");
    assert_eq!(out["alreadyGone"], json!(false));
    assert!(
        alive_at_clear.load(Ordering::SeqCst),
        "input.clear went out while the container still ran"
    );
    assert!(!fake.exists(name));
    assert_eq!(mb.method_calls("input.clear").len(), 1);
    assert_eq!(mb.method_calls("input.uninstall").len(), 1);
}

#[tokio::test]
async fn stop_still_removes_the_container_when_input_clear_fails() {
    let fake = FakePodman::new("tool-input-fail");
    let proj = project(&fake);
    let name = "cs2mcp-wine-tools-0badf00d";
    running_wine(&fake, &proj, name);
    let mb = MockBridge::builder()
        .discovery_dir(wine_bridge_dir(&proj))
        .methods(BRIDGE_METHODS.iter().copied())
        .error("input.clear", -32603, "hook is wedged", None)
        .responder_value("input.uninstall", json!({"uninstalled": true}))
        .start()
        .await
        .unwrap();
    let srv = server_with_bridge(&fake, &proj, &mb.discovery_path);
    call(&srv, "session_status", json!({})).await.unwrap();
    let out = call(&srv, "stop_session", json!({"container": name}))
        .await
        .unwrap();
    assert_eq!(out["inputCleared"], json!(false));
    assert!(out["inputNote"].as_str().unwrap().contains("input.clear"));
    assert_eq!(out["inputUninstalled"], json!(true));
    assert!(!fake.exists(name));
}

#[tokio::test]
async fn stop_skips_input_clear_when_the_bridge_does_not_advertise_it() {
    let fake = FakePodman::new("tool-input-missing");
    let proj = project(&fake);
    let name = "cs2mcp-wine-tools-0badf00d";
    running_wine(&fake, &proj, name);
    let methods: Vec<&str> = BRIDGE_METHODS
        .iter()
        .copied()
        .filter(|m| *m != "input.clear" && *m != "input.uninstall")
        .collect();
    let mb = MockBridge::builder()
        .discovery_dir(wine_bridge_dir(&proj))
        .methods(methods)
        .start()
        .await
        .unwrap();
    let srv = server_with_bridge(&fake, &proj, &mb.discovery_path);
    call(&srv, "session_status", json!({})).await.unwrap();
    let out = call(&srv, "stop_session", json!({"container": name}))
        .await
        .unwrap();
    assert_eq!(out["inputCleared"], json!(false));
    assert_eq!(out["inputUninstalled"], json!(false));
    assert!(mb.method_calls("input.clear").is_empty());
    assert!(!fake.exists(name));
}

#[tokio::test]
async fn stop_without_a_bridge_reports_the_input_hook_as_not_cleared() {
    let fake = FakePodman::new("tool-input-nobridge");
    let proj = project(&fake);
    let name = "cs2mcp-wine-tools-0badf00d";
    running_wine(&fake, &proj, name);
    let srv = server(&fake, &proj, Opts::default());
    let out = call(&srv, "stop_session", json!({"container": name}))
        .await
        .unwrap();
    assert_eq!(out["inputCleared"], json!(false));
    // the dead bridge's discovery file is not inside this container's app dir: not its session
    assert!(out["inputNote"]
        .as_str()
        .unwrap()
        .contains("not the bridge's session"));
    assert!(!fake.exists(name));
}

#[tokio::test]
async fn stop_of_a_linux_client_never_touches_the_input_hook() {
    let fake = FakePodman::new("tool-input-linux");
    let proj = project(&fake);
    let linux = "cs2mcp-linux-client-0badf00d";
    fake.add_running(linux);
    let mb = MockBridge::builder()
        .discovery_dir(wine_bridge_dir(&proj))
        .methods(BRIDGE_METHODS.iter().copied())
        .responder_value("input.clear", json!({"cleared": true}))
        .responder_value("input.uninstall", json!({"uninstalled": true}))
        .start()
        .await
        .unwrap();
    let srv = server_with_bridge(&fake, &proj, &mb.discovery_path);
    call(&srv, "session_status", json!({})).await.unwrap();
    assert!(mb.requests().iter().all(|r| r.method != "input.clear"));

    let out = call(&srv, "stop_session", json!({"container": linux}))
        .await
        .unwrap();
    assert_eq!(out["inputCleared"], json!(false));
    assert_eq!(out["inputUninstalled"], json!(false));
    assert!(out["inputNote"]
        .as_str()
        .unwrap()
        .contains("target is not the bridge's session; input hook untouched"));
    assert!(
        mb.requests()
            .iter()
            .all(|r| !r.method.starts_with("input.")),
        "no input.* method was sent: {:?}",
        mb.requests()
    );
    assert!(!fake.exists(linux));
}

#[tokio::test]
async fn stop_with_a_bridge_for_another_app_dir_leaves_the_input_hook_alone() {
    let fake = FakePodman::new("tool-input-other");
    let proj = project(&fake);
    let name = "cs2mcp-wine-tools-0badf00d";
    // a wine session on a different app dir than the bridge's
    fake.add_running(name);
    let other = proj.join("elsewhere/app");
    fake.set_mounts(
        name,
        &json!([{"Type": "bind", "Name": "", "Source": other.display().to_string(), "Destination": "/app"}])
            .to_string(),
    );
    let mb = MockBridge::builder()
        .discovery_dir(wine_bridge_dir(&proj))
        .methods(BRIDGE_METHODS.iter().copied())
        .responder_value("input.clear", json!({"cleared": true}))
        .start()
        .await
        .unwrap();
    let srv = server_with_bridge(&fake, &proj, &mb.discovery_path);
    call(&srv, "session_status", json!({})).await.unwrap();
    let out = call(&srv, "stop_session", json!({"container": name}))
        .await
        .unwrap();
    assert_eq!(out["inputCleared"], json!(false));
    assert!(mb.method_calls("input.clear").is_empty());
    assert!(!fake.exists(name));
}

#[tokio::test]
async fn stop_fails_cleanly_when_podman_cannot_inspect_and_keeps_the_session_owned() {
    let fake = FakePodman::new("tool-inspect-broken");
    let proj = project(&fake);
    let srv = server(&fake, &proj, Opts::default());
    let launched = call(
        &srv,
        "launch_session",
        json!({"kind": "linux-client", "waitFor": "ContainerUp"}),
    )
    .await
    .unwrap();
    let name = launched["container"].as_str().unwrap().to_string();
    // the state is what decides a removal: without it nothing is stopped or removed
    fake.fail_verb("inspect");
    let rm_before = fake.calls_to("rm").len();
    let err = call(&srv, "stop_session", json!({})).await.unwrap_err();
    assert_eq!(err.name, "PodmanFailed", "{err:?}");
    assert_eq!(
        fake.calls_to("rm").len(),
        rm_before,
        "no rm without knowing the state"
    );
    assert!(fake.exists(&name), "nothing was removed");
    // still owned: the default target is the same session once podman works again
    std::fs::remove_file(fake.dir().join("fail-inspect")).unwrap();
    let out = call(&srv, "stop_session", json!({})).await.unwrap();
    assert_eq!(out["stopped"], json!(name));
}
