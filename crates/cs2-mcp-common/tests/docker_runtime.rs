//! Docker runtime: argv golden vectors (pure), name/mount helpers, and the runtime against a fake
//! podman script (cap, kind busy, idempotent launch, up/down/exec/logs, exit reporting, readiness).
#![allow(clippy::result_large_err)]
use cs2_mcp_common::bridge::{BridgeClient, ClientOptions};
use cs2_mcp_common::mock::podman::FakePodman;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::runtime::docker::{
    bridge_request_payload, channel_probe_payload, ensure_argv, exec_argv, exec_in_argv,
    install_tool_beside, logs_argv, newest_xauth_in, parse_mounts, parse_ps, parse_ps_entries,
    ps_argv, ps_label_argv, rm_argv, run_argv, sh_quote, shared_mount_conflict, stderr_tail,
    stop_argv, valid_container_name, valid_name, vtmsend_argv, wait_argv, DockerOptions,
    DockerRuntime, RunPlan, RuntimeError, StopOutcome, DEFAULT_LINUX_GAME_DIR,
};
use cs2_mcp_common::runtime::readiness::ReadyStage;
use cs2_mcp_common::runtime::{LaunchSpec, Runtime, SessionKind};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The real script this crate's runtime mounts (client-mcp/docker/linux-client.sh).
fn linux_script() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../client-mcp/docker/linux-client.sh"
    ))
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn linux_spec(gpus: bool, args: &[&str]) -> LaunchSpec {
    LaunchSpec {
        kind: SessionKind::LinuxClient,
        testscript: None,
        map: None,
        args: s(args),
        headless: true,
        gpus,
        container_name: None,
        capture: false,
        bridge: true,
    }
}

fn wine_spec(testscript: Option<&str>, args: &[&str]) -> LaunchSpec {
    LaunchSpec {
        kind: SessionKind::WineTools { addon: None },
        testscript: testscript.map(String::from),
        map: None,
        args: s(args),
        headless: false,
        gpus: true,
        container_name: None,
        capture: false,
        bridge: true,
    }
}

fn wine_client_spec(args: &[&str]) -> LaunchSpec {
    LaunchSpec {
        kind: SessionKind::WineClient,
        testscript: None,
        map: None,
        args: s(args),
        headless: false,
        gpus: true,
        container_name: None,
        capture: false,
        // no bridge RPC on a wine client: the ladder never reaches a bridge check
        bridge: false,
    }
}

// ----- argv golden vectors -------------------------------------------------------------------

#[test]
fn golden_linux_client_argv() {
    let out = PathBuf::from("/proj/artifacts/cs2mcp-linux-client-deadbeef-1700000000");
    let script = PathBuf::from("/repo/client-mcp/docker/linux-client.sh");
    let argv = run_argv(
        &linux_spec(false, &[]),
        &RunPlan {
            name: "cs2mcp-linux-client-deadbeef",
            image: "localhost/cs2-docker",
            out_dir: &out,
            linux_script: &script,
            linux_game_dir: DEFAULT_LINUX_GAME_DIR,
            app_dir: Path::new("/unused"),
            cache_dir: Path::new("/unused"),
            xauthority: None,
            display: ":99",
            capture_layer: None,
            s2sdk_probe: None,
        },
    );
    assert_eq!(
        argv,
        s(&[
            "run",
            "-d",
            "--name",
            "cs2mcp-linux-client-deadbeef",
            "--network=host",
            "-v",
            "cs2-home:/home/steam",
            "-v",
            "cs2-shadercache:/cache",
            "-v",
            "/proj/artifacts/cs2mcp-linux-client-deadbeef-1700000000:/work",
            "-v",
            "/repo/client-mcp/docker/linux-client.sh:/cs2mcp/linux-client.sh:ro",
            "-v",
            "/repo/client-mcp/docker/lib-preamble.sh:/cs2mcp/lib-preamble.sh:ro",
            "-e",
            "CS2_GAME_DIR=/home/steam/steamapps/common/Counter-Strike Global Offensive/game",
            "-u",
            "0",
            "--entrypoint",
            "bash",
            "localhost/cs2-docker",
            "/cs2mcp/linux-client.sh",
        ])
    );
}

#[test]
fn golden_linux_dedicated_argv() {
    use cs2_mcp_common::runtime::docker::server_game_args;
    let out = PathBuf::from("/proj/artifacts/cs2mcp-linux-dedicated-deadbeef-1700000000");
    let script = PathBuf::from("/repo/client-mcp/docker/linux-dedicated.sh");
    let spec = LaunchSpec {
        kind: SessionKind::LinuxDedicated,
        testscript: None,
        map: None,
        args: server_game_args("de_dust2", Some("pw1"), &s(&["-tickrate", "64"])),
        headless: true,
        gpus: false,
        container_name: None,
        capture: false,
        bridge: true,
    };
    let argv = run_argv(
        &spec,
        &RunPlan {
            name: "cs2mcp-linux-dedicated-deadbeef",
            image: "localhost/cs2-docker",
            out_dir: &out,
            linux_script: &script,
            linux_game_dir: DEFAULT_LINUX_GAME_DIR,
            app_dir: Path::new("/unused"),
            cache_dir: Path::new("/unused"),
            xauthority: None,
            display: ":99",
            capture_layer: None,
            s2sdk_probe: None,
        },
    );
    assert_eq!(
        argv,
        s(&[
            "run",
            "-d",
            "--name",
            "cs2mcp-linux-dedicated-deadbeef",
            "--network=host",
            "-v",
            "cs2-home:/home/steam",
            "-v",
            "/proj/artifacts/cs2mcp-linux-dedicated-deadbeef-1700000000:/work",
            "-v",
            "/repo/client-mcp/docker/linux-dedicated.sh:/cs2mcp/linux-dedicated.sh:ro",
            "-v",
            "/repo/client-mcp/docker/lib-preamble.sh:/cs2mcp/lib-preamble.sh:ro",
            "-e",
            "CS2_GAME_DIR=/home/steam/steamapps/common/Counter-Strike Global Offensive/game",
            "-u",
            "0",
            "--entrypoint",
            "bash",
            "localhost/cs2-docker",
            "/cs2mcp/linux-dedicated.sh",
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
            "de_dust2",
            "+rcon_password",
            "pw1",
            "-tickrate",
            "64",
        ])
    );
}

#[test]
fn server_game_args_without_rcon_or_extras() {
    use cs2_mcp_common::runtime::docker::server_game_args;
    let a = server_game_args("de_dust2", None, &[]);
    assert_eq!(
        a,
        s(&[
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
            "de_dust2",
        ])
    );
}

#[test]
fn golden_linux_client_gpus_and_game_args() {
    let out = PathBuf::from("/o");
    let script = PathBuf::from("/s.sh");
    let argv = run_argv(
        &linux_spec(true, &["-map", "de dust2"]),
        &RunPlan {
            name: "cs2mcp-linux-client-00000001",
            image: "img",
            out_dir: &out,
            linux_script: &script,
            linux_game_dir: "/g",
            app_dir: Path::new("/unused"),
            cache_dir: Path::new("/unused"),
            xauthority: None,
            display: ":99",
            capture_layer: None,
            s2sdk_probe: None,
        },
    );
    assert_eq!(&argv[..4], &s(&["run", "-d", "--gpus", "all"])[..]);
    // game args follow the script path; podman does not interpret them, so no shell quoting
    assert_eq!(
        &argv[argv.len() - 4..],
        &s(&["img", "/cs2mcp/linux-client.sh", "-map", "de dust2"])[..]
    );
}

#[test]
fn golden_wine_tools_argv() {
    let app = PathBuf::from("/proj/workshop/run/app");
    let cache = PathBuf::from("/proj/workshop/run/cache");
    let xa = PathBuf::from("/home/u/.Xauthority");
    let argv = run_argv(
        &wine_spec(Some("sandbox"), &[]),
        &RunPlan {
            name: "cs2mcp-wine-tools-1234abcd",
            image: "localhost/cs2-docker",
            out_dir: Path::new("/unused"),
            linux_script: Path::new("/unused"),
            linux_game_dir: "/unused",
            app_dir: &app,
            cache_dir: &cache,
            xauthority: Some(&xa),
            display: ":0",
            capture_layer: None,
            s2sdk_probe: None,
        },
    );
    assert_eq!(
        argv,
        s(&[
            "run",
            "-d",
            "--gpus",
            "all",
            "--name",
            "cs2mcp-wine-tools-1234abcd",
            "--network=host",
            "-v",
            "/proj/workshop/run/app:/app",
            "-v",
            "/proj/workshop/run/cache:/cache",
            "-v",
            "/tmp/.X11-unix:/tmp/.X11-unix",
            "-v",
            "/home/u/.Xauthority:/root/.Xauthority:ro",
            "-e",
            "DISPLAY=:0",
            "-e",
            "XAUTHORITY=/root/.Xauthority",
            "--entrypoint",
            "/bin/bash",
            "localhost/cs2-docker",
            "-c",
            "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console -condebug -insecure -testscript /app/vtest/sandbox.txt +engine_no_focus_sleep 0 +r_always_render_all_windows 1",
        ])
    );
}

#[test]
fn golden_wine_user_args_are_quoted_inside_the_shell_line() {
    let app = PathBuf::from("/a");
    let cache = PathBuf::from("/c");
    let argv = run_argv(
        &wine_spec(None, &["+map", "de dust2", "it's"]),
        &RunPlan {
            name: "cs2mcp-wine-tools-00000002",
            image: "img",
            out_dir: Path::new("/unused"),
            linux_script: Path::new("/unused"),
            linux_game_dir: "/unused",
            app_dir: &app,
            cache_dir: &cache,
            xauthority: None,
            display: ":0",
            capture_layer: None,
            s2sdk_probe: None,
        },
    );
    // no xauthority: no XAUTHORITY mount or env
    assert!(!argv.iter().any(|a| a.contains("Xauthority:ro")));
    assert!(!argv.iter().any(|a| a.starts_with("XAUTHORITY=")));
    assert_eq!(
        argv.last().unwrap(),
        "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console -condebug -insecure +map 'de dust2' 'it'\\''s' +engine_no_focus_sleep 0 +r_always_render_all_windows 1"
    );
}

#[test]
fn golden_wine_client_argv() {
    let app = PathBuf::from("/proj/workshop/run/app");
    let cache = PathBuf::from("/proj/workshop/run/cache");
    let xa = PathBuf::from("/home/u/.Xauthority");
    let argv = run_argv(
        &wine_client_spec(&["+map", "de dust2"]),
        &RunPlan {
            name: "cs2mcp-wine-client-1234abcd",
            image: "localhost/cs2-docker",
            out_dir: Path::new("/unused"),
            linux_script: Path::new("/unused"),
            linux_game_dir: "/unused",
            app_dir: &app,
            cache_dir: &cache,
            xauthority: Some(&xa),
            display: ":0",
            capture_layer: None,
            s2sdk_probe: None,
        },
    );
    assert_eq!(
        argv,
        s(&[
            "run",
            "-d",
            "--gpus",
            "all",
            "--name",
            "cs2mcp-wine-client-1234abcd",
            "--network=host",
            "-v",
            "/proj/workshop/run/app:/app",
            "-v",
            "/proj/workshop/run/cache:/cache",
            "-v",
            "/tmp/.X11-unix:/tmp/.X11-unix",
            "-v",
            "/home/u/.Xauthority:/root/.Xauthority:ro",
            "-e",
            "DISPLAY=:0",
            "-e",
            "XAUTHORITY=/root/.Xauthority",
            "--entrypoint",
            "/bin/bash",
            "localhost/cs2-docker",
            "-c",
            "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -vulkan -console -condebug -insecure +map 'de dust2' +engine_no_focus_sleep 0 +r_always_render_all_windows 1",
        ])
    );
}

#[test]
fn wine_client_command_is_the_retail_line_without_the_tools_extras() {
    use cs2_mcp_common::runtime::docker::wine_client_command;
    let mut spec = wine_client_spec(&["+map", "de dust2"]);
    assert_eq!(
        wine_client_command(&spec),
        "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -vulkan -console -condebug -insecure +map 'de dust2' +engine_no_focus_sleep 0 +r_always_render_all_windows 1"
    );
    // no -tools, no -addon, no -testscript: validate refuses a suite for this kind first, and the
    // command builder never emits one even if a spec carries it
    spec.testscript = Some("sandbox".into());
    let cmd = wine_client_command(&spec);
    assert!(!cmd.contains("-tools"), "{cmd}");
    assert!(!cmd.contains("testscript"), "{cmd}");
    assert!(!cmd.contains("-addon"), "{cmd}");
}

#[test]
fn golden_control_argv() {
    assert_eq!(
        ps_argv(),
        s(&["ps", "--filter", "name=cs2", "--format", "json"])
    );
    // no `-f`: a removal never kills a running container; the stop is `-t 0` (SIGKILL at once)
    assert_eq!(rm_argv("cs2mcp-x"), s(&["rm", "cs2mcp-x"]));
    assert_eq!(stop_argv("cs2mcp-x"), s(&["stop", "-t", "0", "cs2mcp-x"]));
    assert_eq!(
        wait_argv("cs2mcp-x"),
        s(&["wait", "--condition=exited", "cs2mcp-x"])
    );
    assert_eq!(
        vtmsend_argv("cs2mcp-x"),
        s(&[
            "exec",
            "-i",
            "cs2mcp-x",
            "bash",
            "-c",
            "cd /app && ./entrypoint.sh vtmsend.exe 2>&1"
        ])
    );
    assert_eq!(
        exec_argv("cs2mcp-x", &s(&["vtmsend.exe"])),
        s(&["exec", "-i", "cs2mcp-x", "vtmsend.exe"])
    );
    assert_eq!(
        logs_argv("cs2mcp-x", 40),
        s(&["logs", "--tail", "40", "cs2mcp-x"])
    );
}

#[test]
fn sh_quote_only_quotes_when_needed() {
    assert_eq!(sh_quote("+engine_no_focus_sleep"), "+engine_no_focus_sleep");
    assert_eq!(sh_quote("/app/vtest/a.txt"), "/app/vtest/a.txt");
    assert_eq!(sh_quote(""), "''");
    assert_eq!(sh_quote("a b"), "'a b'");
    assert_eq!(sh_quote("$(rm -rf /)"), "'$(rm -rf /)'");
}

#[test]
fn container_name_validation() {
    assert!(valid_name("cs2mcp-linux-client-1a2b3c4d"));
    assert!(valid_name("cs2mcp-x"));
    for bad in [
        "cs2mcp-",
        "cs2mcp-Upper",
        "cs2mcp--lead",
        "cs2mcp-under_score",
        "cs2vtest",
        "cs2mcp-a b",
        "cs2mcp-$(id)",
        "cs2mcp-a\nb",
    ] {
        assert!(!valid_name(bad), "{bad:?}");
    }
}

#[test]
fn parse_ps_accepts_array_and_string_names_and_skips_non_running() {
    let json = r#"[
        {"Id":"a","Names":["cs2mcp-linux-client-1"],"State":"running"},
        {"Id":"b","Names":"cs2vtest","State":"running"},
        {"Id":"c","Names":["cs2old"],"State":"exited"}
    ]"#;
    assert_eq!(
        parse_ps(json).unwrap(),
        vec!["cs2mcp-linux-client-1", "cs2vtest"]
    );
    assert!(parse_ps("").unwrap().is_empty());
    assert!(parse_ps("[]").unwrap().is_empty());
}

#[test]
fn kind_busy_matches_the_shared_mount_only() {
    let app = PathBuf::from("/proj/workshop/run/app");
    let wine = wine_spec(None, &[]);
    let linux = linux_spec(false, &[]);
    let wine_mounts = parse_mounts(
        r#"[{"Type":"bind","Name":"","Source":"/proj/workshop/run/app","Destination":"/app"}]"#,
    )
    .unwrap();
    assert_eq!(
        shared_mount_conflict(&wine, &app, &wine_mounts),
        Some("app dir")
    );
    assert_eq!(shared_mount_conflict(&linux, &app, &wine_mounts), None);
    let home = parse_mounts(
        r#"[{"Type":"volume","Name":"cs2-home","Source":"/v/_data","Destination":"/home/steam"}]"#,
    )
    .unwrap();
    assert_eq!(
        shared_mount_conflict(&linux, &app, &home),
        Some("cs2-home volume")
    );
    assert_eq!(shared_mount_conflict(&wine, &app, &home), None);
    assert!(parse_mounts("null").unwrap().is_empty());
}

#[test]
fn newest_xauth_is_picked_by_mtime() {
    let dir = std::env::temp_dir().join(format!("xauth-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("xauth_old");
    let new = dir.join("xauth_new");
    let other = dir.join("unrelated");
    for p in [&old, &new, &other] {
        std::fs::write(p, "x").unwrap();
    }
    let base = SystemTime::now();
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(base - Duration::from_secs(60))
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&new)
        .unwrap()
        .set_modified(base)
        .unwrap();
    assert_eq!(newest_xauth_in(&dir), Some(new.clone()));
    let _ = std::fs::remove_dir_all(&dir);
}

// ----- capture layer ----------------------------------------------------------------------------

#[test]
fn golden_linux_client_argv_with_capture_layer() {
    let out = PathBuf::from("/proj/artifacts/cs2mcp-linux-client-deadbeef-1700000000");
    let script = PathBuf::from("/repo/client-mcp/docker/linux-client.sh");
    let layer = PathBuf::from("/repo/target/vklayer");
    let argv = run_argv(
        &linux_spec(false, &[]),
        &RunPlan {
            name: "cs2mcp-linux-client-deadbeef",
            image: "localhost/cs2-docker",
            out_dir: &out,
            linux_script: &script,
            linux_game_dir: DEFAULT_LINUX_GAME_DIR,
            app_dir: Path::new("/unused"),
            cache_dir: Path::new("/unused"),
            xauthority: None,
            display: ":99",
            capture_layer: Some(&layer),
            s2sdk_probe: None,
        },
    );
    assert_eq!(
        argv,
        s(&[
            "run",
            "-d",
            "--name",
            "cs2mcp-linux-client-deadbeef",
            "--network=host",
            "-v",
            "cs2-home:/home/steam",
            "-v",
            "cs2-shadercache:/cache",
            "-v",
            "/proj/artifacts/cs2mcp-linux-client-deadbeef-1700000000:/work",
            "-v",
            "/repo/client-mcp/docker/linux-client.sh:/cs2mcp/linux-client.sh:ro",
            "-v",
            "/repo/client-mcp/docker/lib-preamble.sh:/cs2mcp/lib-preamble.sh:ro",
            "-v",
            "/repo/target/vklayer:/cs2mcp/vk:ro",
            "-e",
            "VK_ADD_LAYER_PATH=/cs2mcp/vk",
            "-e",
            "VK_INSTANCE_LAYERS=VK_LAYER_CS2MCP_capture",
            "-e",
            "CS2MCP_CAPTURE_DIR=/work/capture",
            "-e",
            "CS2_GAME_DIR=/home/steam/steamapps/common/Counter-Strike Global Offensive/game",
            "-u",
            "0",
            "--entrypoint",
            "bash",
            "localhost/cs2-docker",
            "/cs2mcp/linux-client.sh",
        ])
    );
}

#[test]
fn golden_wine_tools_argv_with_capture_layer() {
    let app = PathBuf::from("/proj/workshop/run/app");
    let cache = PathBuf::from("/proj/workshop/run/cache");
    let out = PathBuf::from("/proj/artifacts/cs2mcp-wine-tools-1234abcd-1700000000");
    let layer = PathBuf::from("/repo/target/vklayer");
    let argv = run_argv(
        &wine_spec(None, &[]),
        &RunPlan {
            name: "cs2mcp-wine-tools-1234abcd",
            image: "localhost/cs2-docker",
            out_dir: &out,
            linux_script: Path::new("/unused"),
            linux_game_dir: "/unused",
            app_dir: &app,
            cache_dir: &cache,
            xauthority: None,
            display: ":0",
            capture_layer: Some(&layer),
            s2sdk_probe: None,
        },
    );
    // the artifacts dir is mounted at /work (only when capturing), then the layer and its env;
    // the Wine command line itself is unchanged
    assert_eq!(
        argv,
        s(&[
            "run",
            "-d",
            "--gpus",
            "all",
            "--name",
            "cs2mcp-wine-tools-1234abcd",
            "--network=host",
            "-v",
            "/proj/workshop/run/app:/app",
            "-v",
            "/proj/workshop/run/cache:/cache",
            "-v",
            "/tmp/.X11-unix:/tmp/.X11-unix",
            "-v",
            "/proj/artifacts/cs2mcp-wine-tools-1234abcd-1700000000:/work",
            "-v",
            "/repo/target/vklayer:/cs2mcp/vk:ro",
            "-e",
            "VK_ADD_LAYER_PATH=/cs2mcp/vk",
            "-e",
            "VK_INSTANCE_LAYERS=VK_LAYER_CS2MCP_capture",
            "-e",
            "CS2MCP_CAPTURE_DIR=/work/capture",
            "-e",
            "DISPLAY=:0",
            "--entrypoint",
            "/bin/bash",
            "localhost/cs2-docker",
            "-c",
            "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console -condebug -insecure +engine_no_focus_sleep 0 +r_always_render_all_windows 1",
        ])
    );
}

/// The s2sdk probe dir: the three new arguments (mount, `CS2MCP_PRELOAD`, `S2SDK_PROBE_OUT`)
/// appear on a Linux argv only when `RunPlan::s2sdk_probe` is set (dedicated; the client shape is
/// the same), and an unset probe leaves the argv without them.
#[test]
fn golden_linux_dedicated_argv_with_s2sdk_probe() {
    let out = PathBuf::from("/proj/artifacts/cs2mcp-linux-dedicated-deadbeef-1700000000");
    let script = PathBuf::from("/repo/client-mcp/docker/linux-dedicated.sh");
    let probe = PathBuf::from("/repo/target/s2sdk-probe");
    let spec = LaunchSpec {
        kind: SessionKind::LinuxDedicated,
        testscript: None,
        map: None,
        args: s(&[]),
        headless: true,
        gpus: false,
        container_name: None,
        capture: false,
        bridge: true,
    };
    fn plan<'a>(probe: Option<&'a Path>, out: &'a Path, script: &'a Path) -> RunPlan<'a> {
        RunPlan {
            name: "cs2mcp-linux-dedicated-deadbeef",
            image: "localhost/cs2-docker",
            out_dir: out,
            linux_script: script,
            linux_game_dir: DEFAULT_LINUX_GAME_DIR,
            app_dir: Path::new("/unused"),
            cache_dir: Path::new("/unused"),
            xauthority: None,
            display: ":99",
            capture_layer: None,
            s2sdk_probe: probe,
        }
    }
    assert_eq!(
        run_argv(&spec, &plan(Some(&probe), &out, &script)),
        s(&[
            "run",
            "-d",
            "--name",
            "cs2mcp-linux-dedicated-deadbeef",
            "--network=host",
            "-v",
            "cs2-home:/home/steam",
            "-v",
            "/proj/artifacts/cs2mcp-linux-dedicated-deadbeef-1700000000:/work",
            "-v",
            "/repo/client-mcp/docker/linux-dedicated.sh:/cs2mcp/linux-dedicated.sh:ro",
            "-v",
            "/repo/client-mcp/docker/lib-preamble.sh:/cs2mcp/lib-preamble.sh:ro",
            "-v",
            "/repo/target/s2sdk-probe:/cs2mcp/s2sdk:ro",
            "-e",
            "CS2MCP_PRELOAD=/cs2mcp/s2sdk/libs2sdk_probe.so",
            "-e",
            "S2SDK_PROBE_OUT=/work/s2sdk-probe.json",
            "-e",
            "CS2_GAME_DIR=/home/steam/steamapps/common/Counter-Strike Global Offensive/game",
            "-u",
            "0",
            "--entrypoint",
            "bash",
            "localhost/cs2-docker",
            "/cs2mcp/linux-dedicated.sh",
        ])
    );
    // unset: no probe mount, no preload env, no report env
    let plain = run_argv(&spec, &plan(None, &out, &script));
    assert!(!plain.iter().any(|a| a.contains("/cs2mcp/s2sdk")));
    assert!(!plain
        .iter()
        .any(|a| a.starts_with("CS2MCP_PRELOAD=") || a.starts_with("S2SDK_PROBE_OUT=")));
}

/// A directory that passes `layer_status`: both files present.
fn write_layer(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("libcs2_vk_capture.so"), b"so").unwrap();
    std::fs::write(dir.join("VkLayer_cs2mcp_capture.json"), b"{}").unwrap();
    dir.to_path_buf()
}

#[tokio::test]
async fn launch_mounts_the_layer_and_makes_a_private_capture_dir_only_when_enabled() {
    let fake = FakePodman::new("capture-on");
    let proj = project(&fake);
    let layer = write_layer(&fake.dir().join("layer"));
    let mut opts = options(&fake, &proj, 3);
    opts.vk_layer_dir = Some(layer.clone());
    let rt = DockerRuntime::new(opts);
    let mut spec = linux_spec(false, &[]);
    spec.capture = true;
    let launched = rt.launch(&spec).await.unwrap();
    let capture = launched.capture_dir.clone().expect("capture dir");
    assert_eq!(capture, launched.artifacts_dir.join("capture"));
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&capture).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    let run = &fake.calls_to("run")[0].argv;
    assert!(run.contains(&format!("{}:/cs2mcp/vk:ro", layer.display())));
    assert!(run.contains(&"CS2MCP_CAPTURE_DIR=/work/capture".to_string()));
}

#[tokio::test]
async fn opt_out_and_no_layer_dir_both_launch_without_the_layer() {
    // capture: false on a runtime that has a valid layer: no mount, no capture dir
    let fake = FakePodman::new("capture-off");
    let proj = project(&fake);
    let mut opts = options(&fake, &proj, 3);
    opts.vk_layer_dir = Some(write_layer(&fake.dir().join("layer")));
    let rt = DockerRuntime::new(opts);
    let mut spec = linux_spec(false, &[]);
    spec.capture = false;
    let launched = rt.launch(&spec).await.unwrap();
    assert!(launched.capture_dir.is_none());
    assert!(!launched.artifacts_dir.join("capture").exists());
    assert!(!fake
        .calls_to("run")
        .iter()
        .any(|c| c.argv.iter().any(|a| a.contains("VK_INSTANCE_LAYERS"))));

    // a runtime whose layer dir is not valid (no files) also launches plain
    let fake2 = FakePodman::new("capture-bad-dir");
    let proj2 = project(&fake2);
    let mut opts2 = options(&fake2, &proj2, 3);
    opts2.vk_layer_dir = None;
    let rt2 = DockerRuntime::new(opts2);
    let launched2 = rt2.launch(&linux_spec(false, &[])).await.unwrap();
    assert!(launched2.capture_dir.is_none());
    assert!(!fake2
        .calls_to("run")
        .iter()
        .any(|c| c.argv.iter().any(|a| a.contains("/cs2mcp/vk"))));
}

// ----- runtime against the fake podman ---------------------------------------------------------

fn options(fake: &FakePodman, project: &Path, max: usize) -> DockerOptions {
    DockerOptions {
        linux_client_script: linux_script(),
        // a stand-in for `cs2-bridge-tools install ... ensure` (its own tests own the real one)
        install_tool: Some(install_stub(fake, project, 0, "", 0)),
        ..DockerOptions::new(project.to_path_buf(), fake.bin.clone(), max)
    }
}

/// A stand-in for `cs2-bridge-tools install --platform <p> ensure`: records its argv, whether the launch lock is
/// held, the podman env it got, then exits `rc` after printing `stderr` (and sleeping `sleep_secs`).
fn install_stub(
    fake: &FakePodman,
    project: &Path,
    rc: i32,
    stderr: &str,
    sleep_secs: u32,
) -> PathBuf {
    let dir = fake.dir();
    let path = dir.join(format!("install-tool-{rc}-{sleep_secs}"));
    let log = dir.join("install.log");
    let lock = project.join(".podman/cs2mcp-launch.lock");
    let body = format!(
        "#!/bin/bash\n\
         LOG='{}'\n\
         printf 'argv=%s\\n' \"$*\" >> \"$LOG\"\n\
         if flock -n '{}' true 2>/dev/null; then echo held=no >> \"$LOG\"; else echo held=yes >> \"$LOG\"; fi\n\
         printf 'PODMAN_XDG=%s\\n' \"${{PODMAN_XDG:-}}\" >> \"$LOG\"\n\
         printf 'XDG=%s\\n' \"${{XDG_RUNTIME_DIR:-}}\" >> \"$LOG\"\n\
         printf 'PODMAN=%s\\n' \"${{PODMAN:-}}\" >> \"$LOG\"\n\
         printf 'CS2_GAME=%s\\n' \"${{CS2_GAME:-}}\" >> \"$LOG\"\n\
         sleep {}\n\
         printf '%s\\n' '{}' >&2\n\
         exit {}\n",
        log.display(),
        lock.display(),
        sleep_secs,
        stderr,
        rc
    );
    write_executable(&path, &body);
    path
}

/// A stand-in for a repairing install ensure: starts a long-lived grandchild,
/// records the script's pid, its process group id, the grandchild's pid and whether the launch lock
/// is held, then blocks for a minute (the test times it out / cancels it and checks the group died).
fn install_group_stub(fake: &FakePodman, project: &Path) -> PathBuf {
    let dir = fake.dir();
    let path = dir.join("install-tool-group");
    let pids = dir.join("install-group.txt");
    let lock = project.join(".podman/cs2mcp-launch.lock");
    let body = format!(
        "#!/bin/bash\n\
         PIDS='{}'\n\
         sleep 300 &\n\
         held=no; flock -n '{}' true 2>/dev/null || held=yes\n\
         printf 'script=%s pgid=%s child=%s held=%s\\n' $$ \"$(ps -o pgid= -p $$ | tr -d ' ')\" $! \"$held\" > \"$PIDS\"\n\
         sleep 60\n",
        pids.display(),
        lock.display()
    );
    write_executable(&path, &body);
    path
}

/// Write a stub the runtime executes directly (no `bash <file>`): it needs its exec bit.
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The pids `install_group_stub` recorded.
struct GroupPids {
    script: i32,
    pgid: i32,
    child: i32,
    held: String,
}

/// Poll for the stub's pid file (it is written at script start, so this only waits out the spawn).
async fn group_pids(path: &Path) -> GroupPids {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            let (mut script, mut pgid, mut child, mut held) = (None, None, None, None);
            for field in text.split_whitespace() {
                if let Some(v) = field.strip_prefix("script=") {
                    script = v.parse::<i32>().ok();
                } else if let Some(v) = field.strip_prefix("pgid=") {
                    pgid = v.parse::<i32>().ok();
                } else if let Some(v) = field.strip_prefix("child=") {
                    child = v.parse::<i32>().ok();
                } else if let Some(v) = field.strip_prefix("held=") {
                    held = Some(v.to_string());
                }
            }
            if let (Some(script), Some(pgid), Some(child), Some(held)) = (script, pgid, child, held)
            {
                return GroupPids {
                    script,
                    pgid,
                    child,
                    held,
                };
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the ensure stub never recorded its pids at {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `kill(pid, 0)`: true while the pid exists (EPERM means it exists but is not ours).
fn pid_alive(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Poll until the pid is gone (ESRCH), false when it outlives `within`. Note: `kill(pid, 0)` still
/// succeeds while the pid is a zombie, so this relies on the host's init reaping the dead process.
async fn pid_gone(pid: i32, within: Duration) -> bool {
    let deadline = std::time::Instant::now() + within;
    while pid_alive(pid) {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// Every ensure invocation the stub recorded, oldest first (`argv=...` lines only).
fn install_calls(fake: &FakePodman) -> Vec<String> {
    std::fs::read_to_string(fake.dir().join("install.log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("argv="))
        .map(String::from)
        .collect()
}

fn install_log(fake: &FakePodman) -> String {
    std::fs::read_to_string(fake.dir().join("install.log")).unwrap_or_default()
}

fn runtime(fake: &FakePodman, project: &Path, max: usize) -> DockerRuntime {
    DockerRuntime::new(options(fake, project, max))
}

fn project(fake: &FakePodman) -> PathBuf {
    let p = fake.dir().join("project");
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn downcast(e: &anyhow::Error) -> &RuntimeError {
    e.downcast_ref::<RuntimeError>()
        .unwrap_or_else(|| panic!("not a RuntimeError: {e:#}"))
}

#[tokio::test]
async fn cap_refuses_at_max_and_names_the_running_containers() {
    let fake = FakePodman::new("cap");
    fake.add_running("cs2vtest");
    fake.add_running("cs2mcp-wine-tools-aaaaaaaa");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let err = rt.launch(&linux_spec(false, &[])).await.unwrap_err();
    match downcast(&err) {
        RuntimeError::TooManySessions { running, max } => {
            assert_eq!(*max, 2);
            assert_eq!(running.len(), 2);
            assert!(running.contains(&"cs2vtest".to_string()));
        }
        other => panic!("{other:?}"),
    }
    assert!(fake.calls_to("run").is_empty(), "nothing may be started");
    assert!(rt.owned().is_empty());
}

#[tokio::test]
async fn cap_counts_only_cs2_names_and_launches_below_max() {
    let fake = FakePodman::new("below");
    fake.add_running("cs2vtest");
    fake.add_running("someothercontainer"); // not cs2*: not counted
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    assert!(!launched.reused);
    assert!(launched.container.starts_with("cs2mcp-linux-client-"));
    assert!(fake.is_running(&launched.container));
    let runs = fake.calls_to("run");
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].xdg_runtime_dir,
        proj.join(".podman/xdg").display().to_string()
    );
    assert!(launched.artifacts_dir.is_dir());
    assert!(launched
        .artifacts_dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with(&launched.container));
    assert_eq!(launched.argv[0], fake.bin.display().to_string());
    assert!(proj.join(".podman/cs2mcp-launch.lock").is_file());
}

#[tokio::test]
async fn one_linux_client_per_home_volume_and_identical_request_is_reused() {
    let fake = FakePodman::new("busy");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 3);
    let first = rt.launch(&linux_spec(false, &[])).await.unwrap();
    // identical request: the running session comes back, nothing new starts
    let again = rt.launch(&linux_spec(false, &[])).await.unwrap();
    assert!(again.reused);
    assert_eq!(again.container, first.container);
    assert_eq!(fake.calls_to("run").len(), 1);
    // a different linux request shares cs2-home with the running one: refused
    let err = rt
        .launch(&linux_spec(false, &["-novid"]))
        .await
        .unwrap_err();
    match downcast(&err) {
        RuntimeError::KindBusy {
            kind,
            container,
            shared,
        } => {
            assert_eq!(*kind, "linux-client");
            assert_eq!(container, &first.container);
            assert_eq!(*shared, "cs2-home volume");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fake.calls_to("run").len(), 1);
}

#[tokio::test]
async fn kind_busy_for_a_foreign_wine_container_on_the_same_app_dir() {
    let fake = FakePodman::new("winebusy");
    let proj = project(&fake);
    let app = proj.join("workshop/run/app");
    fake.add_running("cs2vtest");
    fake.set_mounts(
        "cs2vtest",
        &format!(
            r#"[{{"Type":"bind","Name":"","Source":"{}","Destination":"/app"}}]"#,
            app.display()
        ),
    );
    let xa = proj.join("xauth");
    std::fs::write(&xa, "cookie").unwrap();
    let mut opts = options(&fake, &proj, 3);
    opts.xauthority = Some(xa);
    let rt = DockerRuntime::new(opts);
    let err = rt.launch(&wine_spec(None, &[])).await.unwrap_err();
    match downcast(&err) {
        RuntimeError::KindBusy {
            container, shared, ..
        } => {
            assert_eq!(container, "cs2vtest");
            assert_eq!(*shared, "app dir");
        }
        other => panic!("{other:?}"),
    }
    assert!(fake.calls_to("run").is_empty());
}

#[tokio::test]
async fn up_exec_logs_down_round_trip() {
    let fake = FakePodman::new("roundtrip");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 3);
    let paths = rt.up(&linux_spec(false, &[])).await.unwrap();
    assert_eq!(
        paths.console_log.as_ref().unwrap().file_name().unwrap(),
        "console.log"
    );
    let owned = rt.owned();
    assert_eq!(owned.len(), 1);
    let name = owned[0].name.clone();

    let out = rt.exec(&s(&["vtmsend.exe"])).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "exec-ok");
    assert_eq!(
        fake.calls_to("exec")[0].argv,
        s(&["exec", "-i", &name, "vtmsend.exe"])
    );

    fake.set_logs(&name, "a\nb\nc\n");
    assert_eq!(rt.logs(2).await.unwrap(), "b\nc\n");
    assert_eq!(
        fake.calls_to("logs")[0].argv,
        s(&["logs", "--tail", "2", &name])
    );

    let st = rt.status().await.unwrap();
    assert!(st.running);
    assert_eq!(st.container.as_deref(), Some(name.as_str()));

    rt.down().await.unwrap();
    assert!(!fake.exists(&name), "rm -f removed it");
    assert!(rt.owned().is_empty());
    assert!(rt.down().await.is_err(), "nothing left to stop");
}

#[tokio::test]
async fn stop_of_a_container_that_is_already_gone_prunes_it() {
    let fake = FakePodman::new("gone");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 3);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    // removed behind our back
    std::fs::remove_file(fake.dir().join("state").join(&launched.container)).unwrap();
    let removed = rt.down_named(&launched.container).await.unwrap();
    assert!(!removed, "reported as already gone");
    assert!(rt.owned().is_empty(), "pruned from the owned list");
}

#[tokio::test]
async fn down_named_refuses_foreign_and_malformed_names_without_calling_podman() {
    let fake = FakePodman::new("foreign");
    fake.add_running("cs2vtest");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let err = rt.down_named("cs2vtest").await.unwrap_err();
    assert!(matches!(downcast(&err), RuntimeError::ForeignContainer(n) if n == "cs2vtest"));
    let err = rt.down_named("cs2mcp-Bad_Name").await.unwrap_err();
    assert!(matches!(downcast(&err), RuntimeError::InvalidName(_)));
    assert!(fake.calls_to("rm").is_empty());
    assert!(fake.is_running("cs2vtest"));
}

#[tokio::test]
async fn explicit_names_are_validated_before_anything_runs() {
    let fake = FakePodman::new("badname");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    for bad in ["cs2vtest", "cs2mcp-Upper", "cs2mcp-x;rm"] {
        let mut spec = linux_spec(false, &[]);
        spec.container_name = Some(bad.into());
        let err = rt.launch(&spec).await.unwrap_err();
        assert!(
            matches!(downcast(&err), RuntimeError::InvalidName(_)),
            "{bad}"
        );
    }
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn unsupported_combinations_are_refused_before_any_podman_call() {
    let fake = FakePodman::new("unsupported");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let mut wine = wine_spec(None, &[]);
    wine.headless = true;
    let err = rt.launch(&wine).await.unwrap_err();
    assert!(matches!(downcast(&err), RuntimeError::Unsupported(_)));
    let mut linux = linux_spec(false, &[]);
    linux.headless = false;
    let err = rt.launch(&linux).await.unwrap_err();
    assert!(matches!(downcast(&err), RuntimeError::Unsupported(_)));
    rt.launch(&linux_spec(false, &[]))
        .await
        .expect("control: the valid request launches");
    let mut testscript_linux = linux_spec(false, &[]);
    testscript_linux.testscript = Some("sandbox".into());
    let err = rt.launch(&testscript_linux).await.unwrap_err();
    assert!(matches!(downcast(&err), RuntimeError::Unsupported(_)));
    assert_eq!(fake.calls_to("run").len(), 1);
}

#[tokio::test]
async fn missing_podman_is_podman_unavailable() {
    let proj = std::env::temp_dir().join(format!("cs2mcp-noppm-{}", std::process::id()));
    std::fs::create_dir_all(&proj).unwrap();
    let mut opts = DockerOptions::new(proj.clone(), PathBuf::from("/nonexistent/podman-xyz"), 2);
    opts.linux_client_script = linux_script();
    let rt = DockerRuntime::new(opts);
    let err = rt.launch(&linux_spec(false, &[])).await.unwrap_err();
    assert!(
        matches!(downcast(&err), RuntimeError::PodmanUnavailable(_)),
        "{err:#}"
    );
    let _ = std::fs::remove_dir_all(&proj);
}

#[tokio::test]
async fn slow_podman_times_out() {
    let proj = std::env::temp_dir().join(format!("cs2mcp-slow-{}", std::process::id()));
    std::fs::create_dir_all(&proj).unwrap();
    let slow = proj.join("slow-podman");
    std::fs::write(&slow, "#!/bin/sh\nsleep 5\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut opts = DockerOptions::new(proj.clone(), slow, 2);
    opts.linux_client_script = linux_script();
    opts.short_timeout = Duration::from_millis(200);
    let rt = DockerRuntime::new(opts);
    let err = rt.launch(&linux_spec(false, &[])).await.unwrap_err();
    assert!(
        matches!(downcast(&err), RuntimeError::PodmanTimeout { .. }),
        "{err:#}"
    );
    let _ = std::fs::remove_dir_all(&proj);
}

#[tokio::test]
async fn wine_launch_checks_cookie_and_staged_suite_then_parks() {
    let fake = FakePodman::new("wine");
    let proj = project(&fake);

    // no cookie: refused before anything starts
    let rt = runtime(&fake, &proj, 2);
    let err = rt.launch(&wine_spec(None, &[])).await.unwrap_err();
    assert!(matches!(downcast(&err), RuntimeError::MissingXauthority(_)));

    let xa = proj.join("xauth");
    std::fs::write(&xa, "cookie").unwrap();
    let mut opts = options(&fake, &proj, 2);
    opts.xauthority = Some(xa.clone());
    let rt = DockerRuntime::new(opts);

    // suite not staged: fail fast, no podman call
    let err = rt
        .launch(&wine_spec(Some("sandbox"), &[]))
        .await
        .unwrap_err();
    assert!(matches!(downcast(&err), RuntimeError::SuiteNotStaged(_)));
    assert!(fake.calls_to("run").is_empty());

    let vtest = proj.join("workshop/run/app/vtest");
    std::fs::create_dir_all(&vtest).unwrap();
    std::fs::write(vtest.join("sandbox.txt"), "Test_Run mark VTEST_PARKED\n").unwrap();

    // bridge: false keeps this check to the channel (vtmsend answers) and the harness marker
    let mut spec = wine_spec(Some("sandbox"), &[]);
    spec.bridge = false;
    let launched = rt.launch(&spec).await.unwrap();
    assert!(launched.container.starts_with("cs2mcp-wine-tools-"));
    let console = launched.paths.console_log.clone().unwrap();
    assert_eq!(console, proj.join("workshop/run/app/game/csgo/console.log"));
    fake.set_exec(&launched.container, "vtmsend: creator version 1\n", 0);

    // the harness parks: the marker shows up in the engine's console.log
    let writer = console.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::create_dir_all(writer.parent().unwrap()).unwrap();
        std::fs::write(&writer, "TESTSCRIPT: CheckPoint -> 'VTEST_PARKED'\n").unwrap();
    });
    let r = rt
        .wait_ready(
            &launched.container,
            ReadyStage::Parked,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(r.target_reached, "{r:?}");
    assert_eq!(
        r.reached,
        vec![
            ReadyStage::ContainerUp,
            ReadyStage::ConsoleLogExists,
            ReadyStage::MenuReached,
            ReadyStage::Parked
        ]
    );
    let unchecked: Vec<_> = r.unchecked.iter().map(|u| u.stage).collect();
    assert_eq!(unchecked, vec![ReadyStage::BridgeHello]);
}

#[tokio::test]
async fn readiness_polls_console_log_until_the_menu_marker() {
    let fake = FakePodman::new("ready");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    let console = launched.paths.console_log.clone().unwrap();
    assert!(!console.exists());

    let writer = console.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(&writer, "Loading...\n").unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        std::fs::write(&writer, "Loading...\nSwitchToLoop game\n").unwrap();
    });
    let r = rt
        .wait_ready(
            &launched.container,
            ReadyStage::MenuReached,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(r.target_reached, "{r:?}");
    assert_eq!(
        r.reached,
        vec![
            ReadyStage::ContainerUp,
            ReadyStage::ConsoleLogExists,
            ReadyStage::MenuReached
        ]
    );
    assert!(r.note.is_none() && r.exited.is_none());
}

#[tokio::test]
async fn readiness_timeout_is_a_report_not_an_error() {
    let fake = FakePodman::new("timeout");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    let r = rt
        .wait_ready(
            &launched.container,
            ReadyStage::MenuReached,
            Duration::from_millis(600),
        )
        .await
        .unwrap();
    assert!(!r.target_reached);
    assert_eq!(r.reached, vec![ReadyStage::ContainerUp]);
    assert!(
        r.note.as_deref().unwrap().contains("ConsoleLogExists"),
        "{r:?}"
    );
    assert!(fake.is_running(&launched.container), "the session stays up");
}

#[tokio::test]
async fn container_that_exits_before_up_is_reported_with_code_and_log_tail() {
    let fake = FakePodman::new("exited");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    fake.set_state(&launched.container, "exited 3");
    let log: String = (1..=25).map(|i| format!("line {i}\n")).collect();
    fake.set_logs(&launched.container, &log);
    let started = std::time::Instant::now();
    let r = rt
        .wait_ready(
            &launched.container,
            ReadyStage::MenuReached,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "stops at once");
    assert!(!r.target_reached);
    let exit = r.exited.expect("exit reported");
    assert_eq!(exit.status, "exited");
    assert_eq!(exit.exit_code, Some(3));
    assert_eq!(exit.logs_tail.len(), 20);
    assert_eq!(exit.logs_tail.last().unwrap(), "line 25");
    assert!(r.note.unwrap().contains("exit code 3"));
}

#[tokio::test]
async fn container_removed_while_waiting_is_gone() {
    let fake = FakePodman::new("removed");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    std::fs::remove_file(fake.dir().join("state").join(&launched.container)).unwrap();
    let r = rt
        .wait_ready(
            &launched.container,
            ReadyStage::ConsoleLogExists,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert_eq!(r.exited.unwrap().status, "gone");
}

#[tokio::test]
async fn readiness_reports_uncheckable_targets_without_waiting() {
    let fake = FakePodman::new("unchecked");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    // the earlier stages pass (console.log with the menu marker), then BridgeHello is unchecked
    let console = launched.paths.console_log.clone().unwrap();
    std::fs::write(&console, "SwitchToLoop game\n").unwrap();
    let started = std::time::Instant::now();
    let r = rt
        .wait_ready(
            &launched.container,
            ReadyStage::BridgeHello,
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert!(!r.target_reached);
    assert_eq!(
        r.reached,
        vec![
            ReadyStage::ContainerUp,
            ReadyStage::ConsoleLogExists,
            ReadyStage::MenuReached
        ]
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(r.note.as_deref().unwrap().contains("not checked"));
}

#[tokio::test]
async fn wait_for_an_unknown_container_is_no_session() {
    let fake = FakePodman::new("unknown");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let err = rt
        .wait_ready(
            "cs2mcp-linux-client-00000000",
            ReadyStage::ContainerUp,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
    assert!(matches!(downcast(&err), RuntimeError::NoSession(_)));
}

// ----- wine readiness: channel probe, bridge load, stale discovery -----------------------------

/// A wine-tools runtime on the fake podman: the cookie file and the app dir exist (launch checks
/// both), so the probes can write the vtmsend payload.
fn wine_runtime(fake: &FakePodman, proj: &Path) -> DockerRuntime {
    let xa = proj.join("xauth");
    std::fs::write(&xa, "cookie").unwrap();
    std::fs::create_dir_all(proj.join("workshop/run/app")).unwrap();
    let mut opts = options(fake, proj, 3);
    opts.xauthority = Some(xa);
    DockerRuntime::new(opts)
}

/// `ConsoleLogExists` is the stage before the channel: the engine has created its console.log.
fn create_console(launched: &cs2_mcp_common::runtime::docker::Launched) {
    let console = launched.paths.console_log.clone().unwrap();
    std::fs::create_dir_all(console.parent().unwrap()).unwrap();
    std::fs::write(&console, "").unwrap();
}

fn app_payload(proj: &Path) -> String {
    std::fs::read_to_string(proj.join("workshop/run/app/vtm_payload.kv3")).unwrap()
}

/// A discovery file body for a bridge on `port` (the keys the mock writes).
fn json_disc(port: u16) -> String {
    format!(
        r#"{{"pid": 1, "protocol": 1, "bridgeVersion": "x", "gameBuild": "x", "transport": "tcp", "host": "127.0.0.1", "port": {port}, "token": "{}", "startedAt": 0}}"#,
        "11".repeat(32)
    )
}

#[tokio::test]
async fn channel_probe_is_pending_until_vtmsend_reports_the_creator_version() {
    let fake = FakePodman::new("channel");
    let proj = project(&fake);
    let rt = wine_runtime(&fake, &proj);
    let launched = rt.launch(&wine_spec(None, &[])).await.unwrap();
    let c = launched.container.clone();
    create_console(&launched);

    // before toolframework2 Init: vtmsend exits 1 with OpenMutex failed
    fake.set_exec(
        &c,
        "vtmsend: OpenMutex failed - is a -tools session running?\n",
        1,
    );
    // Each wait is short (1.2 s), but a loaded host (the workspace's tests in parallel, every
    // podman call a bash fork) can spend a whole one before the first vtmsend answer is back. So
    // the short wait is repeated until one of them has seen vtmsend's answer, under an outer
    // deadline; every attempt must still refuse the stage.
    let outer = std::time::Instant::now();
    let r = loop {
        let r = rt
            .wait_ready(&c, ReadyStage::MenuReached, Duration::from_millis(1200))
            .await
            .unwrap();
        assert!(!r.target_reached, "OpenMutex failed never reaches the stage: {r:?}");
        let probed = r.note.as_deref().is_some_and(|n| n.contains("OpenMutex failed"));
        if probed || outer.elapsed() > Duration::from_secs(30) {
            break r;
        }
    };
    assert_eq!(
        r.reached,
        vec![ReadyStage::ContainerUp, ReadyStage::ConsoleLogExists]
    );
    let note = r.note.as_deref().unwrap();
    assert!(note.contains("MenuReached"), "{r:?}");
    // vtmsend's last line says why the channel is not up yet
    assert!(note.contains("OpenMutex failed"), "{r:?}");
    assert!(fake.is_running(&c), "the session stays up");
    // the probe is the channel command, written where vtmsend reads it
    assert_eq!(app_payload(&proj), channel_probe_payload());
    assert!(channel_probe_payload().contains("echo mcp_runtime_channel_probe"));
    assert!(!fake.calls_to("exec").is_empty());

    // the channel comes up: the next wait reaches the stage
    fake.set_exec(&c, "vtmsend: creator version 1\n", 0);
    let r = rt
        .wait_ready(&c, ReadyStage::MenuReached, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(r.target_reached, "{r:?}");
    assert!(r.reached.contains(&ReadyStage::MenuReached));

    // remembered in the owned session: a later wait does not probe again
    let before = fake.calls_to("exec").len();
    let r = rt
        .wait_ready(&c, ReadyStage::MenuReached, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(r.target_reached);
    assert_eq!(fake.calls_to("exec").len(), before);
}

#[tokio::test]
async fn bridge_request_is_posted_once_then_every_five_seconds_while_no_discovery_appears() {
    let fake = FakePodman::new("repost");
    let proj = project(&fake);
    let rt = wine_runtime(&fake, &proj);
    let launched = rt.launch(&wine_spec(None, &[])).await.unwrap();
    let c = launched.container.clone();
    create_console(&launched);
    fake.set_exec(&c, "vtmsend: creator version 1\n", 0);

    // no bridge loads: the wait runs its whole timeout (7.5 s: the second re-post is due at 10 s)
    let r = rt
        .wait_ready(&c, ReadyStage::BridgeHello, Duration::from_millis(7500))
        .await
        .unwrap();
    assert!(!r.target_reached, "{r:?}");
    assert_eq!(
        r.reached,
        vec![
            ReadyStage::ContainerUp,
            ReadyStage::ConsoleLogExists,
            ReadyStage::MenuReached
        ]
    );
    assert!(r.note.as_deref().unwrap().contains("BridgeHello"));
    // one channel probe, then the tool_request right after the channel came up, then one re-post
    // about 5 s later: three vtmsend calls in 7.5 s, not one per poll. The calls are told apart by
    // their timestamps (the channel probe is the only one before the first request).
    let execs = fake.calls_to("exec");
    assert_eq!(execs.len(), 3, "{execs:?}");
    let at = fake.exec_times();
    assert_eq!(at.len(), 3, "{at:?}");
    let first_request_gap = Duration::from_nanos((at[1] - at[0]) as u64);
    let repost_gap = Duration::from_nanos((at[2] - at[1]) as u64);
    assert!(
        first_request_gap < Duration::from_secs(2),
        "{first_request_gap:?}"
    );
    assert!(repost_gap >= Duration::from_millis(4900), "{repost_gap:?}");
    let payload = app_payload(&proj);
    assert!(
        payload.contains("tool_request") && payload.contains("cs2_bridge"),
        "{payload}"
    );
    assert_eq!(payload, bridge_request_payload());
}

#[tokio::test]
async fn bridge_hello_is_reached_when_the_bridge_writes_its_discovery_and_answers() {
    let fake = FakePodman::new("hello");
    let proj = project(&fake);
    let rt = wine_runtime(&fake, &proj);
    let launched = rt.launch(&wine_spec(None, &[])).await.unwrap();
    let c = launched.container.clone();
    create_console(&launched);
    fake.set_exec(&c, "vtmsend: creator version 1\n", 0);

    // the game's bridge writes session.json where the runtime looks for it (after the launch)
    let disc = rt.wine_paths().discovery.unwrap();
    let dir = disc.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    let mb = MockBridge::builder()
        .discovery_dir(dir)
        .start()
        .await
        .unwrap();
    assert_eq!(mb.discovery_path, disc);

    let r = rt
        .wait_ready(&c, ReadyStage::BridgeHello, Duration::from_secs(10))
        .await
        .unwrap();
    assert!(r.target_reached, "{r:?}");
    assert_eq!(
        r.reached,
        vec![
            ReadyStage::ContainerUp,
            ReadyStage::ConsoleLogExists,
            ReadyStage::MenuReached,
            ReadyStage::BridgeHello
        ]
    );
}

#[tokio::test]
async fn a_bridge_held_by_another_client_counts_as_up_and_stays_up() {
    let fake = FakePodman::new("busyhello");
    let proj = project(&fake);
    let rt = wine_runtime(&fake, &proj);
    let launched = rt.launch(&wine_spec(None, &[])).await.unwrap();
    let c = launched.container.clone();
    create_console(&launched);
    fake.set_exec(&c, "vtmsend: creator version 1\n", 0);
    let disc = rt.wine_paths().discovery.unwrap();
    std::fs::create_dir_all(disc.parent().unwrap()).unwrap();
    let mb = MockBridge::builder()
        .discovery_dir(disc.parent().unwrap().to_path_buf())
        .start()
        .await
        .unwrap();

    // the game's (or another client's) one authenticated connection holds the bridge
    let mut holder = BridgeClient::new(ClientOptions {
        discovery_path: mb.discovery_path.clone(),
        ..Default::default()
    });
    holder.connect(false).await.unwrap();

    // our throw-away hello is refused as busy: that means the bridge is up
    let r = rt
        .wait_ready(&c, ReadyStage::BridgeHello, Duration::from_secs(10))
        .await
        .unwrap();
    assert!(r.target_reached, "{r:?}");
    // the refusal really came from the bridge's busy answer, not a plain successful hello
    assert!(mb.busy_count() >= 1, "busy_count {}", mb.busy_count());

    // the holder leaves; the bridge stays reached without another hello (remembered in Owned)
    holder.close();
    let started = std::time::Instant::now();
    let r = rt
        .wait_ready(&c, ReadyStage::BridgeHello, Duration::from_secs(10))
        .await
        .unwrap();
    assert!(r.target_reached, "{r:?}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_stale_discovery_file_is_removed_by_the_launch_before_any_probe() {
    let fake = FakePodman::new("stale");
    let proj = project(&fake);
    let rt = wine_runtime(&fake, &proj);
    let disc = rt.wine_paths().discovery.unwrap();
    std::fs::create_dir_all(disc.parent().unwrap()).unwrap();
    // left behind by a bridge that was SIGKILLed (it never deletes the file)
    std::fs::write(&disc, r#"{"pid": 1, "port": 1, "token": "stale"}"#).unwrap();

    let launched = rt.launch(&wine_spec(None, &[])).await.unwrap();
    assert!(launched.container.starts_with("cs2mcp-wine-tools-"));
    assert!(!disc.exists(), "the stale session.json is gone");
    assert!(fake.calls_to("exec").is_empty(), "no probe at launch");
}

// ----- wine client: launch, readiness, conflicts ------------------------------------------------

#[tokio::test]
async fn wine_client_launch_runs_ensure_then_run_with_the_wine_client_argv() {
    let fake = FakePodman::new("wineclient");
    let proj = project(&fake);
    let rt = wine_runtime(&fake, &proj);
    let launched = rt.launch(&wine_client_spec(&[])).await.unwrap();
    assert!(launched.container.starts_with("cs2mcp-wine-client-"));
    // the shared wine tree is ensured first (wine platform, CS2_GAME named), under the lock
    let log = install_log(&fake);
    assert!(log.contains("argv=install --platform wine ensure"), "{log}");
    assert!(log.contains("held=yes"), "{log}");
    assert!(
        log.contains(&format!(
            "CS2_GAME={}/game",
            std::fs::canonicalize(proj.join("workshop/run/app"))
                .expect("the runtime's app dir")
                .display()
        )),
        "{log}"
    );
    // then one `podman run` whose shell line is the retail-like command
    let runs = fake.calls_to("run");
    assert_eq!(runs.len(), 1);
    let argv = &runs[0].argv;
    assert_eq!(
        argv.last().unwrap(),
        "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -vulkan -console -condebug -insecure +engine_no_focus_sleep 0 +r_always_render_all_windows 1"
    );
    assert!(
        argv.contains(&format!("{}:/app", proj.join("workshop/run/app").display())),
        "{argv:?}"
    );
    assert!(!argv.iter().any(|a| a.contains("-tools")), "{argv:?}");
    // the console.log is the app tree's (the readiness marker file); no discovery file: nothing
    // listens on a wine client
    assert_eq!(
        launched.paths.console_log.as_deref(),
        Some(proj.join("workshop/run/app/game/csgo/console.log").as_path())
    );
    assert!(launched.paths.discovery.is_none(), "{:?}", launched.paths);
}

#[tokio::test]
async fn wine_client_refuses_a_testscript_headless_and_a_missing_cookie() {
    let fake = FakePodman::new("wineclientref");
    let proj = project(&fake);
    // no cookie: refused like wine-tools, before anything starts
    let rt = runtime(&fake, &proj, 2);
    let err = rt.launch(&wine_client_spec(&[])).await.unwrap_err();
    assert!(matches!(downcast(&err), RuntimeError::MissingXauthority(_)));

    let rt = wine_runtime(&fake, &proj);
    let mut ts = wine_client_spec(&[]);
    ts.testscript = Some("sandbox".into());
    let err = rt.launch(&ts).await.unwrap_err();
    assert!(
        matches!(downcast(&err), RuntimeError::Unsupported(m) if m.contains("testscript")),
        "{err:#}"
    );
    let mut hl = wine_client_spec(&[]);
    hl.headless = true;
    let err = rt.launch(&hl).await.unwrap_err();
    assert!(matches!(downcast(&err), RuntimeError::Unsupported(_)));
    // validate refuses before the lock, the ensure and any podman call
    assert!(fake.calls_to("run").is_empty());
    assert!(install_calls(&fake).is_empty());
}

#[tokio::test]
async fn a_stale_wine_client_console_log_moves_aside_and_cannot_satisfy_the_marker() {
    use cs2_mcp_common::runtime::readiness::WINE_CLIENT_MENU_MARKER;
    let fake = FakePodman::new("wineclient-stale-log");
    let proj = project(&fake);
    let rt = wine_runtime(&fake, &proj);
    let console = rt.wine_paths().console_log.clone().unwrap();
    std::fs::create_dir_all(console.parent().unwrap()).unwrap();
    // a previous run reached the menu: its line in a surviving console.log would satisfy this
    // launch's readiness marker without the game ever starting
    std::fs::write(&console, format!("{WINE_CLIENT_MENU_MARKER}\n")).unwrap();

    let launched = rt.launch(&wine_client_spec(&[])).await.unwrap();
    let prev = console.with_extension("log.prev");
    assert_eq!(
        std::fs::read_to_string(&prev).unwrap(),
        format!("{WINE_CLIENT_MENU_MARKER}\n"),
        "the stale console.log moved aside"
    );
    assert!(!console.exists(), "the stale console.log is not the new one");
    // bounded: the previous run's marker line is gone, so the menu is not "reached"
    let r = rt
        .wait_ready(
            &launched.container,
            ReadyStage::MenuReached,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert!(!r.target_reached, "{r:?}");
}

#[tokio::test]
async fn a_stale_discovery_file_is_removed_for_a_wine_client_launch_which_reports_none() {
    let fake = FakePodman::new("wineclient-stale-disc");
    let proj = project(&fake);
    let rt = wine_runtime(&fake, &proj);
    let disc = rt.wine_paths().discovery.clone().unwrap();
    std::fs::create_dir_all(disc.parent().unwrap()).unwrap();
    // left behind by a bridge that was SIGKILLed (it never deletes the file); a wine client runs
    // the bridge as a plain Metamod plugin, so nothing would ever clean it up at wait time
    std::fs::write(&disc, r#"{"pid": 1, "port": 1, "token": "stale"}"#).unwrap();

    let launched = rt.launch(&wine_client_spec(&[])).await.unwrap();
    assert!(
        !disc.exists(),
        "the stale session.json is gone even for a wine client"
    );
    assert!(
        launched.paths.discovery.is_none(),
        "{:?}",
        launched.paths
    );
}

#[tokio::test]
async fn wine_client_readiness_waits_for_the_menu_marker_in_the_app_console_log() {
    use cs2_mcp_common::runtime::readiness::WINE_CLIENT_MENU_MARKER;
    let fake = FakePodman::new("wineclientready");
    let proj = project(&fake);
    let rt = wine_runtime(&fake, &proj);
    let launched = rt.launch(&wine_client_spec(&[])).await.unwrap();
    let console = launched.paths.console_log.clone().unwrap();
    assert!(!console.exists());

    let writer = console.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::create_dir_all(writer.parent().unwrap()).unwrap();
        std::fs::write(&writer, "Loading...\n").unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(&writer, "Loading...\nSwitchToLoop game\n").unwrap();
    });
    let r = rt
        .wait_ready(
            &launched.container,
            ReadyStage::MenuReached,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(r.target_reached, "{r:?}");
    assert_eq!(
        r.reached,
        vec![
            ReadyStage::ContainerUp,
            ReadyStage::ConsoleLogExists,
            ReadyStage::MenuReached
        ]
    );
    let unchecked: Vec<_> = r.unchecked.iter().map(|u| u.stage).collect();
    assert!(unchecked.is_empty(), "{r:?}");
    assert!(r.note.is_none() && r.exited.is_none(), "{r:?}");
    // a wait aimed past the menu reports BridgeHello as unchecked (no bridge RPC on a wine
    // client) instead of waiting for it
    let started = std::time::Instant::now();
    let r = rt
        .wait_ready(
            &launched.container,
            ReadyStage::BridgeHello,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(!r.target_reached, "{r:?}");
    let unchecked: Vec<_> = r.unchecked.iter().map(|u| u.stage).collect();
    assert_eq!(unchecked, vec![ReadyStage::BridgeHello]);
    assert!(r.note.as_deref().unwrap().contains("not checked"));
    assert!(started.elapsed() < Duration::from_secs(5), "no wait happened");
    assert_eq!(WINE_CLIENT_MENU_MARKER, "SwitchToLoop game");
}

/// NEGATIVE: at most one Wine session per app tree. A wine-client launch while a wine-tools
/// session runs is refused by the existing conflict check, before the ensure and the run.
#[tokio::test]
async fn wine_client_while_a_wine_tools_session_runs_is_kind_busy_with_no_ensure_and_no_run() {
    let fake = FakePodman::new("wineconflict");
    let proj = project(&fake);
    let rt = wine_runtime(&fake, &proj);
    let first = rt.launch(&wine_spec(None, &[])).await.unwrap();
    assert!(first.container.starts_with("cs2mcp-wine-tools-"));
    let runs_before = fake.calls_to("run").len();
    let ensure_before = install_calls(&fake).len();

    let err = rt.launch(&wine_client_spec(&[])).await.unwrap_err();
    match downcast(&err) {
        RuntimeError::KindBusy {
            kind,
            container,
            shared,
        } => {
            assert_eq!(*kind, "wine-client");
            assert_eq!(container, &first.container);
            assert_eq!(*shared, "app dir");
        }
        other => panic!("{other:?}"),
    }
    assert!(err.to_string().contains("wine-client"), "{err:#}");
    assert_eq!(fake.calls_to("run").len(), runs_before, "no second run");
    assert_eq!(install_calls(&fake).len(), ensure_before, "no second ensure");
}

/// The reverse direction conflicts too: wine-tools cannot take the app tree of a running client.
#[tokio::test]
async fn wine_tools_while_a_wine_client_session_runs_is_kind_busy() {
    let fake = FakePodman::new("wineconflict2");
    let proj = project(&fake);
    let rt = wine_runtime(&fake, &proj);
    let first = rt.launch(&wine_client_spec(&[])).await.unwrap();
    let err = rt.launch(&wine_spec(None, &[])).await.unwrap_err();
    match downcast(&err) {
        RuntimeError::KindBusy {
            kind,
            container,
            shared,
        } => {
            assert_eq!(*kind, "wine-tools");
            assert_eq!(container, &first.container);
            assert_eq!(*shared, "app dir");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fake.calls_to("run").len(), 1);
}

#[tokio::test]
async fn a_fresh_discovery_at_a_closed_port_is_pending_with_the_connect_failure() {
    let fake = FakePodman::new("refused");
    let proj = project(&fake);
    let rt = wine_runtime(&fake, &proj);
    let launched = rt.launch(&wine_spec(None, &[])).await.unwrap();
    let c = launched.container.clone();
    create_console(&launched);
    fake.set_exec(&c, "vtmsend: creator version 1\n", 0);

    // a port nothing listens on: bind it, note it, let it go
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let disc = rt.wine_paths().discovery.unwrap();
    std::fs::create_dir_all(disc.parent().unwrap()).unwrap();
    std::fs::write(&disc, json_disc(port)).unwrap();

    let r = rt
        .wait_ready(&c, ReadyStage::BridgeHello, Duration::from_millis(2500))
        .await
        .unwrap();
    assert!(!r.target_reached, "{r:?}");
    assert!(r.exited.is_none(), "the session is not an error");
    let note = r.note.unwrap();
    assert!(note.contains("control.hello"), "{note}");
    assert!(note.contains(&port.to_string()), "{note}");
}

// ----- stop: no kill mid-stop, no blind waits --------------------------------------------------

#[tokio::test]
async fn a_fast_stop_removes_the_container_with_plain_rm() {
    let fake = FakePodman::new("faststop");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    let c = launched.container.clone();

    let out = rt.stop_named(&c, Duration::from_secs(5)).await.unwrap();
    assert_eq!(out, StopOutcome::Removed);
    assert_eq!(fake.calls_to("stop")[0].argv, s(&["stop", "-t", "0", &c]));
    assert_eq!(fake.calls_to("rm")[0].argv, s(&["rm", &c]));
    assert!(!fake.exists(&c));
    assert!(rt.owned().is_empty());
    assert!(
        !proj.join(format!(".podman/rm-{c}.err")).exists(),
        "the rm stderr file is gone after a successful stop"
    );
}

#[tokio::test]
async fn a_removal_podman_refuses_is_an_error_with_its_stderr() {
    let fake = FakePodman::new("rmfails");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    let c = launched.container.clone();
    fake.fail_verb("rm");

    let err = rt.stop_named(&c, Duration::from_secs(5)).await.unwrap_err();
    match downcast(&err) {
        RuntimeError::Podman { stderr, .. } => {
            assert!(stderr.contains("rm is failing"), "{stderr}")
        }
        other => panic!("{other:?}"),
    }
    assert!(fake.exists(&c), "still there");
    assert_eq!(rt.owned().len(), 1, "still owned: the stop did not finish");
}

#[tokio::test]
async fn a_stop_that_exits_near_its_deadline_still_confirms_the_removal() {
    let fake = FakePodman::new("rmwindow");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    let c = launched.container.clone();
    // already exited: no wait at all, so with a zero window only the removal's own window can
    // confirm it
    fake.set_state(&c, "exited 0");
    let out = rt.stop_named(&c, Duration::ZERO).await.unwrap();
    assert_eq!(out, StopOutcome::Removed);
    assert!(!fake.exists(&c));
}

#[tokio::test]
async fn a_slow_stop_is_pending_within_the_budget_and_is_not_reissued_or_killed() {
    let fake = FakePodman::new("slowstop");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    let c = launched.container.clone();
    fake.set_slow_stop(&c, 3);

    let first = rt.stop_named(&c, Duration::from_secs(1)).await.unwrap();
    match first {
        StopOutcome::Pending {
            state,
            stop_issued_ms,
        } => {
            assert_eq!(state, "stopping");
            assert!(stop_issued_ms.is_some());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fake.calls_to("stop").len(), 1);
    assert!(
        fake.calls_to("rm").is_empty(),
        "nothing removed while it stops"
    );
    assert!(fake.exists(&c));
    assert_eq!(rt.owned().len(), 1, "still owned until it is gone");

    // the next call waits for the same stop: no second stop, then removed
    let second = rt.stop_named(&c, Duration::from_secs(10)).await.unwrap();
    assert_eq!(second, StopOutcome::Removed);
    assert_eq!(fake.calls_to("stop").len(), 1, "the stop was issued once");
    assert_eq!(fake.calls_to("rm").len(), 1);
    assert!(rt.owned().is_empty());
}

#[tokio::test]
async fn a_stop_reports_when_it_was_issued_exited_and_removed() {
    let fake = FakePodman::new("stoptiming");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    let c = launched.container.clone();
    fake.set_slow_stop(&c, 1);

    let report = rt
        .stop_named_report(&c, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(report.outcome, StopOutcome::Removed);
    let t = &report.timing;
    assert_eq!(t.initial_state, "running");
    let issued = t.stop_issued_ms.expect("stop issued");
    let exited = t.exited_ms.expect("exited");
    let rm = t.rm_issued_ms.expect("rm issued");
    let removed = t.removed_ms.expect("removed");
    assert!(
        issued <= exited && exited <= rm && rm <= removed && removed <= t.total_ms,
        "{t:?}"
    );
    // the fake reports the exit 1 s after the stop
    assert!(exited >= issued + 900, "{t:?}");
}

#[tokio::test]
async fn an_exited_container_is_removed_without_a_stop_and_says_so() {
    let fake = FakePodman::new("stoptimingexited");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    let c = launched.container.clone();
    fake.set_state(&c, "exited 0");

    let report = rt
        .stop_named_report(&c, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(report.outcome, StopOutcome::Removed);
    assert_eq!(report.timing.initial_state, "exited");
    assert_eq!(report.timing.stop_issued_ms, None);
    assert!(report.timing.exited_ms.is_some() && report.timing.removed_ms.is_some());
    assert!(fake.calls_to("stop").is_empty());
}

#[tokio::test]
async fn a_stalled_first_inspect_still_issues_the_stop() {
    // live (2026-10-12): podman inspect stalled > 10 s while the container store's disk synced,
    // and stop_session failed with PodmanTimeout before any stop: the game kept running
    let fake = FakePodman::new("stalledinspect");
    let proj = project(&fake);
    let mut opts = options(&fake, &proj, 2);
    opts.short_timeout = Duration::from_millis(500);
    let rt = DockerRuntime::new(opts);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    let c = launched.container.clone();
    fake.set_slow_inspect_once(&c, 3);

    let report = rt
        .stop_named_report(&c, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(report.outcome, StopOutcome::Removed, "{report:?}");
    assert_eq!(report.timing.initial_state, "unknown");
    assert!(report.timing.stop_issued_ms.is_some(), "{report:?}");
    assert_eq!(fake.calls_to("stop").len(), 1);
    assert!(!fake.exists(&c));
    assert!(rt.owned().is_empty());
}

#[tokio::test]
async fn a_stalled_inspect_of_a_gone_container_is_already_gone() {
    let fake = FakePodman::new("stalledgone");
    let proj = project(&fake);
    let mut opts = options(&fake, &proj, 2);
    opts.short_timeout = Duration::from_millis(500);
    let rt = DockerRuntime::new(opts);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    let c = launched.container.clone();
    fake.set_slow_inspect_once(&c, 3);
    fake.remove(&c);

    // the stop of a gone container fails in the background; podman wait says it is gone
    let report = rt
        .stop_named_report(&c, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(report.outcome, StopOutcome::AlreadyGone, "{report:?}");
    assert!(fake.calls_to("rm").is_empty());
    assert!(rt.owned().is_empty());
}

#[tokio::test]
async fn when_podman_wait_fails_the_stop_polls_the_state_instead() {
    let fake = FakePodman::new("waitfallback");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    let c = launched.container.clone();
    fake.fail_verb("wait");
    fake.set_slow_stop(&c, 1);

    let out = rt.stop_named(&c, Duration::from_secs(10)).await.unwrap();
    assert_eq!(out, StopOutcome::Removed);
    assert_eq!(fake.calls_to("rm").len(), 1);
}

#[tokio::test]
async fn a_stopping_container_is_not_reused_and_still_holds_its_slot() {
    let fake = FakePodman::new("stopbusy");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    let c = launched.container.clone();
    fake.set_slow_stop(&c, 2);
    let pending = rt.stop_named(&c, Duration::from_millis(300)).await.unwrap();
    assert!(
        matches!(pending, StopOutcome::Pending { .. }),
        "{pending:?}"
    );

    // the same request while it stops: refused (its mount is still held), never a second run
    let err = rt.launch(&linux_spec(false, &[])).await.unwrap_err();
    match downcast(&err) {
        RuntimeError::KindBusy { container, .. } => assert_eq!(container, &c),
        other => panic!("{other:?}"),
    }
    assert_eq!(fake.calls_to("run").len(), 1);
}

#[tokio::test]
async fn a_confirmed_stop_of_a_wine_session_removes_its_discovery_file() {
    let fake = FakePodman::new("winestop");
    let proj = project(&fake);
    let rt = wine_runtime(&fake, &proj);
    let launched = rt.launch(&wine_spec(None, &[])).await.unwrap();
    let disc = rt.wine_paths().discovery.unwrap();
    std::fs::create_dir_all(disc.parent().unwrap()).unwrap();
    std::fs::write(&disc, json_disc(1)).unwrap();

    let out = rt
        .stop_named(&launched.container, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(out, StopOutcome::Removed);
    assert!(!disc.exists(), "the bridge's file goes with its session");
}

// ----- ModelDoc: exec/lookup on a container this runtime did not launch (WP4) ------------------

#[test]
fn golden_label_and_exec_in_argv() {
    assert_eq!(
        ps_label_argv("cs2mcp.tool=modeldoc"),
        s(&[
            "ps",
            "--filter",
            "label=cs2mcp.tool=modeldoc",
            "--format",
            "json"
        ])
    );
    assert_eq!(
        exec_in_argv(
            "cs2tools",
            &s(&["bash", "-c", "cd /app && ./entrypoint.sh resourcecompiler.exe -nop4 x.vmdl"])
        ),
        s(&[
            "exec",
            "-i",
            "cs2tools",
            "bash",
            "-c",
            "cd /app && ./entrypoint.sh resourcecompiler.exe -nop4 x.vmdl"
        ])
    );
    // a name is NOT required to be a cs2mcp-* session name (the ModelDoc container is `cs2tools`)
    assert!(valid_container_name("cs2tools"));
    assert!(valid_container_name("cs2mcp-wine-tools-1a2b3c4d"));
    let too_long = "x".repeat(129);
    for bad in ["", "-lead", ".dot", "a b", "a;b", "a\nb", "tool$()", &too_long] {
        assert!(!valid_container_name(bad), "{bad:?}");
    }
}

#[test]
fn parse_ps_entries_on_a_label_filter_sample() {
    // `podman ps --filter label=cs2mcp.tool=modeldoc --format json` output shape (podman itself
    // does the filtering; exited ones can only appear via a stale array in the sample)
    let json = r#"[
        {"Id":"x","Names":["cs2tools"],"State":"running","Labels":{"cs2mcp.tool":"modeldoc"}},
        {"Id":"y","Names":"cs2old","State":"exited"}
    ]"#;
    assert_eq!(
        parse_ps_entries(json).unwrap(),
        vec![
            ("cs2tools".to_string(), "running".to_string()),
            ("cs2old".to_string(), "exited".to_string())
        ]
    );
}

#[tokio::test]
async fn container_by_label_answers_none_and_uses_the_label_filter() {
    let fake = FakePodman::new("mdlabel");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    assert_eq!(
        rt.container_by_label("cs2mcp.tool=modeldoc").await.unwrap(),
        None
    );
    let ps = fake.calls_to("ps");
    assert_eq!(ps.len(), 1);
    assert!(ps[0].argv.contains(&"label=cs2mcp.tool=modeldoc".to_string()));
}

#[tokio::test]
async fn exec_in_runs_on_a_foreign_container_with_the_xdg_dir() {
    let fake = FakePodman::new("mdexec");
    let proj = project(&fake);
    fake.add_running("cs2tools");
    fake.set_exec("cs2tools", "OK: 1 compiled, 0 failed, 0 skipped, 0m:03s\n", 0);
    let rt = runtime(&fake, &proj, 2);
    let out = rt
        .exec_in(
            "cs2tools",
            &s(&[
                "bash",
                "-c",
                "cd /app && ./entrypoint.sh resourcecompiler.exe -nop4 x.vmdl",
            ]),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "OK: 1 compiled, 0 failed, 0 skipped, 0m:03s"
    );
    let execs = fake.calls_to("exec");
    assert_eq!(execs.len(), 1);
    assert_eq!(
        execs[0].argv,
        s(&[
            "exec",
            "-i",
            "cs2tools",
            "bash",
            "-c",
            "cd /app && ./entrypoint.sh resourcecompiler.exe -nop4 x.vmdl"
        ])
    );
    assert_eq!(
        execs[0].xdg_runtime_dir,
        proj.join(".podman/xdg").display().to_string()
    );
}

#[tokio::test]
async fn exec_in_refuses_an_invalid_name_without_calling_podman() {
    let fake = FakePodman::new("mdexecname");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 2);
    for bad in ["", "a b", "-lead"] {
        let err = rt
            .exec_in(bad, &s(&["true"]), Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(
            matches!(downcast(&err), RuntimeError::InvalidExecName(_)),
            "{err:#}"
        );
    }
    assert!(fake.calls().is_empty(), "podman was never called");
}

#[tokio::test]
async fn exec_in_with_a_hermetic_missing_podman_is_podman_unavailable() {
    let proj = std::env::temp_dir().join(format!("cs2mcp-mdexec-{}", std::process::id()));
    std::fs::create_dir_all(&proj).unwrap();
    let rt = DockerRuntime::new(DockerOptions::new(
        proj.clone(),
        PathBuf::from("/nonexistent/podman-mdxyz"),
        2,
    ));
    let err = rt
        .exec_in(
            "cs2tools",
            &s(&["bash", "-c", "true"]),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(downcast(&err), RuntimeError::PodmanUnavailable(_)),
        "{err:#}"
    );
    let _ = std::fs::remove_dir_all(&proj);
}

#[tokio::test]
async fn exec_in_background_streams_into_the_log_and_kills() {
    let proj = std::env::temp_dir().join(format!("cs2mcp-mdexec-bg-{}", std::process::id()));
    std::fs::create_dir_all(&proj).unwrap();
    // a streaming helper: line 1, a pause, line 2 (never reached when the kill lands)
    let dir = proj.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("podman");
    std::fs::write(
        &fake,
        r#"#!/bin/sh
echo "line-one"
sleep 30
echo "line-two"
"#,
    )
    .unwrap();
    #[allow(clippy::permissions_set_readonly_false)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let rt = DockerRuntime::new(DockerOptions::new(proj.clone(), fake, 2));
    let log = proj.join("logs/rc.log");
    let mut handle = rt
        .exec_in_background(
            "cs2tools",
            &s(&["bash", "-c", "echo hi"]),
            &log,
        )
        .await
        .expect("background exec");
    // the log grows while the exec runs (that is why the tool polls it, not the process)
    let at = std::time::Instant::now();
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("line-one") {
            break;
        }
        assert!(at.elapsed() < Duration::from_secs(5), "no output streamed: {text:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        handle.finished().expect("a probe cannot fail while the child lives").is_none(),
        "podman is still streaming (the tool watches the log, not the exit)"
    );
    // the kill: the streaming stops (line-two is never written), like the compile recipe
    handle.kill();
    handle.withdrawn().await;
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        !text.contains("line-two"),
        "the kill ended the exec before the sleep finished: {text:?}"
    );
    assert!(
        handle.finished().expect("a probe cannot fail while the child lives").is_some(),
        "killed, so a status exists"
    );
    let _ = std::fs::remove_dir_all(&proj);
}

#[tokio::test]
async fn exec_in_background_refs_a_missing_podman_as_unavailable_and_a_bad_name() {
    let proj = std::env::temp_dir().join(format!("cs2mcp-mdexec-bg-err-{}", std::process::id()));
    std::fs::create_dir_all(&proj).unwrap();
    let rt = DockerRuntime::new(DockerOptions::new(
        proj.clone(),
        PathBuf::from("/nonexistent/podman-mdxyz"),
        2,
    ));
    let log = proj.join("rc.log");
    let err = rt
        .exec_in_background("cs2tools", &s(&["bash", "-c", "true"]), &log)
        .await
        .unwrap_err();
    assert!(
        matches!(downcast(&err), RuntimeError::PodmanUnavailable(_)),
        "{err:#}"
    );
    let err = rt
        .exec_in_background("bad name", &s(&["bash", "-c", "true"]), &log)
        .await
        .unwrap_err();
    assert!(matches!(downcast(&err), RuntimeError::InvalidExecName(_)), "{err:#}");
    let _ = std::fs::remove_dir_all(&proj);
}

// ----- the install ensure at launch ------------------------------------------------------------

#[test]
fn ensure_argv_names_the_platform_for_every_launch_kind() {
    assert_eq!(
        ensure_argv(&SessionKind::WineTools { addon: None }),
        s(&["install", "--platform", "wine", "ensure"])
    );
    // the WineClient launcher (lane F) reuses the same Wine tree ensure
    assert_eq!(
        ensure_argv(&SessionKind::WineClient),
        s(&["install", "--platform", "wine", "ensure"])
    );
    assert_eq!(
        ensure_argv(&SessionKind::LinuxClient),
        s(&["install", "--platform", "linux", "ensure"])
    );
    assert_eq!(
        ensure_argv(&SessionKind::LinuxDedicated),
        s(&["install", "--platform", "linux", "ensure"])
    );
}

#[test]
fn the_default_install_tool_is_the_cs2_bridge_tools_next_to_the_server() {
    // cargo builds every binary of a profile into one directory
    assert_eq!(
        install_tool_beside(Path::new("/repo/target/release/client-mcp")),
        PathBuf::from("/repo/target/release/cs2-bridge-tools")
    );
    assert_eq!(
        install_tool_beside(Path::new("hammer-mcp")),
        PathBuf::from("cs2-bridge-tools")
    );
}

#[test]
fn stderr_tail_keeps_the_last_lines_only() {
    let text: String = (1..=25).map(|i| format!("line{i}\n")).collect();
    let tail = stderr_tail(&text, 20);
    assert!(tail.starts_with("line6\n"), "{tail:?}");
    assert!(tail.ends_with("line25"), "{tail:?}");
    assert_eq!(tail.lines().count(), 20);
    assert_eq!(stderr_tail("  one line  \n", 20), "one line");
    assert_eq!(stderr_tail("", 20), "");
}

#[tokio::test]
async fn launch_runs_ensure_under_the_lock_and_before_podman_run() {
    let fake = FakePodman::new("ensure-order");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 3);
    let launched = rt.launch(&linux_spec(false, &[])).await.unwrap();
    let log = install_log(&fake);
    assert!(
        log.contains("argv=install --platform linux ensure"),
        "{log}"
    );
    // `ensure` ran while this process held the cross-process launch lock
    assert!(
        log.contains("held=yes"),
        "ensure did not see the launch lock held: {log}"
    );
    let xdg = proj.join(".podman/xdg").display().to_string();
    assert!(log.contains(&format!("PODMAN_XDG={xdg}")), "{log}");
    assert!(log.contains(&format!("XDG={xdg}")), "{log}");
    // the runtime's podman binary is forwarded to the script (its guards run podman themselves)
    assert!(
        log.contains(&format!("PODMAN={}", fake.bin.display())),
        "{log}"
    );
    // a Linux tree is the cs2-home volume: no CS2_GAME override
    assert!(log.contains("CS2_GAME=\n"), "{log}");
    assert_eq!(fake.calls_to("run").len(), 1);
    assert!(fake.is_running(&launched.container));
}

#[tokio::test]
async fn ensure_for_wine_names_the_runtime_wine_tree_outside_the_canonical_checkout() {
    let fake = FakePodman::new("ensure-wine");
    let proj = project(&fake);
    let rt = wine_runtime(&fake, &proj);
    rt.launch(&wine_spec(None, &[])).await.unwrap();
    let log = install_log(&fake);
    assert!(log.contains("argv=install --platform wine ensure"), "{log}");
    assert!(log.contains("held=yes"), "{log}");
    let app = std::fs::canonicalize(proj.join("workshop/run/app")).expect("the runtime's app dir");
    assert!(
        log.contains(&format!("CS2_GAME={}/game", app.display())),
        "{log}"
    );
}

#[tokio::test]
async fn a_missing_wine_app_dir_fails_the_launch_naming_the_path() {
    let fake = FakePodman::new("ensure-nogame");
    let proj = project(&fake);
    let xa = proj.join("xauth");
    std::fs::write(&xa, "cookie").unwrap();
    let mut opts = options(&fake, &proj, 3);
    opts.xauthority = Some(xa);
    let rt = DockerRuntime::new(opts);
    // no workshop/run/app: the ensure call cannot name a canonical tree, so the launch is refused
    let err = rt.launch(&wine_spec(None, &[])).await.unwrap_err();
    let app = proj.join("workshop/run/app").display().to_string();
    assert!(err.to_string().contains(&app), "{err:#}");
    assert!(fake.calls_to("run").is_empty(), "no podman run");
    assert!(install_calls(&fake).is_empty(), "ensure never started");
}

#[tokio::test]
async fn reused_refused_and_busy_launches_never_run_ensure() {
    let fake = FakePodman::new("ensure-skip");
    let proj = project(&fake);
    let rt = runtime(&fake, &proj, 3);
    rt.launch(&linux_spec(false, &[])).await.unwrap();
    assert_eq!(install_calls(&fake).len(), 1);

    // identical request: the running session comes back and nothing is ensured again
    let again = rt.launch(&linux_spec(false, &[])).await.unwrap();
    assert!(again.reused);
    assert_eq!(install_calls(&fake).len(), 1);

    // KindBusy (same volume, different args): refused before ensure
    let err = rt
        .launch(&linux_spec(false, &["-novid"]))
        .await
        .unwrap_err();
    assert!(
        matches!(downcast(&err), RuntimeError::KindBusy { .. }),
        "{err:#}"
    );
    assert_eq!(install_calls(&fake).len(), 1);

    // TooManySessions: refused before ensure
    let fake2 = FakePodman::new("ensure-cap");
    fake2.add_running("cs2vtest");
    let rt2 = runtime(&fake2, &project(&fake2), 1);
    let err = rt2.launch(&linux_spec(false, &[])).await.unwrap_err();
    assert!(
        matches!(downcast(&err), RuntimeError::TooManySessions { .. }),
        "{err:#}"
    );
    assert!(install_calls(&fake2).is_empty());
}

#[tokio::test]
async fn ensure_exit_4_stops_the_launch_names_the_containers_and_starts_nothing() {
    let fake = FakePodman::new("ensure-busy");
    let proj = project(&fake);
    let stderr = "[install] FAIL: cs2-home is mounted by cs2tools, cs2mcp-linux-client-aaaa; \
                  stale: cs2_bridge.dll for build 2000933";
    let mut opts = options(&fake, &proj, 3);
    opts.install_tool = Some(install_stub(&fake, &proj, 4, stderr, 0));
    let rt = DockerRuntime::new(opts);
    let err = rt.launch(&linux_spec(false, &[])).await.unwrap_err();
    match downcast(&err) {
        RuntimeError::InstallBusy { stderr } => {
            assert!(stderr.contains("cs2tools"), "{stderr}");
            assert!(stderr.contains("cs2mcp-linux-client-aaaa"), "{stderr}");
        }
        other => panic!("{other:?}"),
    }
    let msg = err.to_string();
    assert!(msg.contains("the game tree needs (re)installing"), "{msg}");
    assert!(
        msg.contains("cs2tools") && msg.contains("cs2mcp-linux-client-aaaa"),
        "{msg}"
    );
    assert!(msg.contains("stop them or run"), "{msg}");
    assert!(
        fake.calls_to("run").is_empty(),
        "no podman run after a refused ensure"
    );
    assert!(rt.owned().is_empty());
}

#[tokio::test]
async fn ensure_exit_1_carries_the_code_and_stderr_and_starts_nothing() {
    let fake = FakePodman::new("ensure-fail");
    let proj = project(&fake);
    let mut opts = options(&fake, &proj, 3);
    let stub = install_stub(
        &fake,
        &proj,
        1,
        "[install] FAIL: target/bridge-rs/win64/cs2_bridge.dll is missing",
        0,
    );
    opts.install_tool = Some(stub.clone());
    let rt = DockerRuntime::new(opts);
    let err = rt.launch(&linux_spec(false, &[])).await.unwrap_err();
    match downcast(&err) {
        RuntimeError::InstallFailed {
            code,
            stderr,
            args,
            path,
        } => {
            assert_eq!(*code, 1);
            assert_eq!(args, "install --platform linux ensure");
            assert_eq!(path, &stub.display().to_string());
            assert!(stderr.contains("cs2_bridge.dll is missing"), "{stderr}");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        fake.calls_to("run").is_empty(),
        "no podman run after a failed ensure"
    );
}

#[tokio::test]
async fn a_timed_out_ensure_stops_the_launch_and_starts_nothing() {
    let fake = FakePodman::new("ensure-timeout");
    let proj = project(&fake);
    let mut opts = options(&fake, &proj, 3);
    opts.install_tool = Some(install_stub(&fake, &proj, 0, "", 30));
    opts.install_timeout = Duration::from_secs(1);
    let rt = DockerRuntime::new(opts);
    let err = rt.launch(&linux_spec(false, &[])).await.unwrap_err();
    match downcast(&err) {
        RuntimeError::InstallTimeout { secs, args, .. } => {
            assert_eq!(*secs, 1);
            assert_eq!(args, "install --platform linux ensure");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        err.to_string().contains("did not finish within 1 s"),
        "{err:#}"
    );
    assert!(
        fake.calls_to("run").is_empty(),
        "no podman run after a timed-out ensure"
    );
}

#[tokio::test]
async fn a_timed_out_ensure_kills_the_whole_process_group() {
    let fake = FakePodman::new("ensure-pg");
    let proj = project(&fake);
    let mut opts = options(&fake, &proj, 3);
    opts.install_tool = Some(install_group_stub(&fake, &proj));
    opts.install_timeout = Duration::from_secs(1);
    let rt = DockerRuntime::new(opts);
    // an outer bound: a regression that fails to kill the group would otherwise run the stub out
    let err = tokio::time::timeout(Duration::from_secs(20), rt.launch(&linux_spec(false, &[])))
        .await
        .expect("the timed-out ensure must be killed within its grace, not run to completion")
        .unwrap_err();
    assert!(
        matches!(downcast(&err), RuntimeError::InstallTimeout { .. }),
        "{err:#}"
    );
    let pids = group_pids(&fake.dir().join("install-group.txt")).await;
    assert_eq!(
        pids.script, pids.pgid,
        "the script must lead its own process group"
    );
    assert_eq!(
        pids.held, "yes",
        "the ensure stub did not see the launch lock held (this says the ensure ran under the lock; it does not say when the kill ran)"
    );
    assert!(
        pid_gone(pids.script, Duration::from_secs(5)).await,
        "the ensure script {} outlived the timeout",
        pids.script
    );
    assert!(
        pid_gone(pids.child, Duration::from_secs(5)).await,
        "the writer {} the script spawned outlived the timeout",
        pids.child
    );
    assert!(
        fake.calls_to("run").is_empty(),
        "no podman run after a timed-out ensure"
    );
}

#[tokio::test]
async fn a_cancelled_launch_kills_the_ensure_process_group_too() {
    // the caller drops the launch future once the stub is running: the drop releases the launch
    // lock, so the writers it spawned must go with it (the next ensure repairs a partial install).
    // Driven by the stub's pid file, not a fixed timer.
    let fake = FakePodman::new("ensure-cancel");
    let proj = project(&fake);
    let mut opts = options(&fake, &proj, 3);
    opts.install_tool = Some(install_group_stub(&fake, &proj));
    opts.install_timeout = Duration::from_secs(120);
    let rt = DockerRuntime::new(opts);
    let pids_path = fake.dir().join("install-group.txt");
    // the stub is mid-ensure once its pid file exists: drop the launch future (this is the
    // caller's cancellation) at the end of this block, releasing the launch lock
    let linux = linux_spec(false, &[]);
    {
        let launch_fut = rt.launch(&linux);
        let mut launch = std::pin::pin!(launch_fut);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::select! {
                res = &mut launch => panic!("the stub blocks, so the launch must not finish: {res:?}"),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {
                    if pids_path.exists() {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the ensure stub never recorded its pids at {}",
                        pids_path.display()
                    );
                }
            }
        }
    }
    let pids = group_pids(&pids_path).await;
    assert!(
        pid_gone(pids.script, Duration::from_secs(5)).await,
        "the cancelled ensure script {} outlived the dropped launch",
        pids.script
    );
    assert!(
        pid_gone(pids.child, Duration::from_secs(5)).await,
        "the writer {} kept running after the launch lock was released",
        pids.child
    );
    assert!(fake.calls_to("run").is_empty());
}

/// A stand-in for a repairing ensure whose group member ignores SIGTERM (`trap '' TERM`) and keeps
/// writing: the leader dies on SIGTERM as usual, so a terminate that only SIGKILLs after the grace
/// when the leader is still alive leaves this writer running past the launch lock.
fn install_sigterm_ignoring_stub(fake: &FakePodman, project: &Path) -> PathBuf {
    let dir = fake.dir();
    let path = dir.join("install-tool-term-ignore");
    let pids = dir.join("install-group.txt");
    let marker = dir.join("term-ignore-marker");
    let lock = project.join(".podman/cs2mcp-launch.lock");
    let body = format!(
        "#!/bin/bash\n\
         PIDS='{}'\n\
         MARKER='{}'\n\
         bash -c 'trap \"\" TERM; while :; do echo x >> \"$1\"; sleep 0.2; done' ignore-term \"$MARKER\" &\n\
         held=no; flock -n '{}' true 2>/dev/null || held=yes\n\
         printf 'script=%s pgid=%s child=%s held=%s\\n' $$ \"$(ps -o pgid= -p $$ | tr -d ' ')\" $! \"$held\" > \"$PIDS\"\n\
         sleep 60\n",
        pids.display(),
        marker.display(),
        lock.display()
    );
    write_executable(&path, &body);
    path
}

#[tokio::test]
async fn a_timed_out_ensure_kills_members_that_ignore_sigterm() {
    // the leader dies on SIGTERM (plain bash), so waiting for the leader alone finds nothing to
    // kill after the grace: a member that handles or ignores SIGTERM would survive its leader and
    // keep writing after the launch lock is gone. terminate must ALWAYS SIGKILL the group while
    // the leader is still unreaped.
    let fake = FakePodman::new("ensure-term-ignore");
    let proj = project(&fake);
    let mut opts = options(&fake, &proj, 3);
    opts.install_tool = Some(install_sigterm_ignoring_stub(&fake, &proj));
    opts.install_timeout = Duration::from_secs(1);
    let rt = DockerRuntime::new(opts);
    let err = tokio::time::timeout(Duration::from_secs(20), rt.launch(&linux_spec(false, &[])))
        .await
        .expect("the timed-out ensure must be killed within its grace, not run to completion")
        .unwrap_err();
    assert!(
        matches!(downcast(&err), RuntimeError::InstallTimeout { .. }),
        "{err:#}"
    );
    let pids = group_pids(&fake.dir().join("install-group.txt")).await;
    assert!(
        pid_gone(pids.child, Duration::from_secs(5)).await,
        "the TERM-ignoring writer {} outlived the timed-out ensure",
        pids.child
    );
    assert!(
        fake.calls_to("run").is_empty(),
        "no podman run after a timed-out ensure"
    );
}

#[tokio::test]
async fn a_missing_install_tool_fails_the_launch_instead_of_skipping_ensure() {
    let fake = FakePodman::new("ensure-missing");
    let proj = project(&fake);
    let mut opts = options(&fake, &proj, 3);
    let missing = proj.join("no-such-cs2-bridge-tools");
    opts.install_tool = Some(missing.clone());
    let rt = DockerRuntime::new(opts);
    let err = rt.launch(&linux_spec(false, &[])).await.unwrap_err();
    match downcast(&err) {
        RuntimeError::InstallToolMissing { path } => {
            assert_eq!(path, &missing.display().to_string());
        }
        other => panic!("{other:?}"),
    }
    assert!(fake.calls_to("run").is_empty());
}
