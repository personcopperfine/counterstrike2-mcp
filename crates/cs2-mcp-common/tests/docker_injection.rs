//! Hostile game arguments (newline, `$()`) must reach podman exactly and stay inert in the
//! Wine `bash -c` line.
#![allow(clippy::result_large_err)]
use cs2_mcp_common::mock::podman::FakePodman;
use cs2_mcp_common::runtime::docker::{run_argv, DockerOptions, DockerRuntime, RunPlan};
use cs2_mcp_common::runtime::{LaunchSpec, SessionKind};
use std::path::{Path, PathBuf};

const HOSTILE: &str = "x\n$(touch /tmp/cs2mcp-pwned)";

#[tokio::test]
async fn linux_args_round_trip_through_the_podman_argv() {
    let fake = FakePodman::new("inject");
    let proj = fake.dir().join("project");
    std::fs::create_dir_all(&proj).unwrap();
    let mut opts = DockerOptions::new(proj.clone(), fake.bin.clone(), 2);
    opts.linux_client_script = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../client-mcp/docker/linux-client.sh"
    ));
    // stand-in for bridge/install-rs.sh ensure (the real one belongs to the game install)
    let stub = fake.dir().join("install-rs.sh");
    std::fs::write(&stub, "#!/bin/bash\nexit 0\n").unwrap();
    opts.install_script = Some(stub);
    let rt = DockerRuntime::new(opts);
    let spec = LaunchSpec {
        kind: SessionKind::LinuxClient,
        testscript: None,
        map: None,
        args: vec!["-map".into(), HOSTILE.into()],
        headless: true,
        gpus: false,
        container_name: None,
        capture: false,
        bridge: true,
    };
    rt.launch(&spec).await.unwrap();
    let run = &fake.calls_to("run")[0].argv;
    assert_eq!(run.last().unwrap(), HOSTILE);
    assert_eq!(run[run.len() - 2], "-map");
    assert!(!fake
        .calls()
        .iter()
        .any(|c| c.argv.iter().any(|a| a.contains("pwned")) && c.argv[0] != "run"));
}

#[test]
fn wine_args_are_quoted_inside_the_shell_line() {
    let app = PathBuf::from("/a");
    let cache = PathBuf::from("/c");
    let spec = LaunchSpec {
        kind: SessionKind::WineTools { addon: None },
        testscript: None,
        map: None,
        args: vec![HOSTILE.into()],
        headless: false,
        gpus: true,
        container_name: None,
        capture: false,
        bridge: true,
    };
    let argv = run_argv(
        &spec,
        &RunPlan {
            name: "cs2mcp-wine-tools-00000003",
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
            native_package: None,
        },
    );
    let line = argv.last().unwrap();
    assert!(line.contains("'x\n$(touch /tmp/cs2mcp-pwned)'"), "{line:?}");
    assert!(
        !line.contains(" $(touch"),
        "unquoted substitution in {line:?}"
    );
}
