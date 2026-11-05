//! The real stdio transport, driven through the built binary (POC `StdioEndToEnd`).
mod common;

use serde_json::json;
use std::path::PathBuf;

#[tokio::test]
async fn handshake_tools_call_and_errors_over_stdio() {
    let mb = cs2_mcp_common::mock::MockBridge::start().await.unwrap();
    let mut s = cs2_mcp_common::mock::stdio::Server::spawn(
        env!("CARGO_BIN_EXE_hammer-mcp"),
        &common::hermetic(&["--discovery", mb.discovery_path.to_str().unwrap()]),
    )
    .await;
    let init = s.request(1, "initialize", json!({"protocolVersion": "2025-06-18", "clientInfo": {"name": "test", "version": "0"}, "capabilities": {}})).await;
    assert_eq!(init["result"]["serverInfo"]["name"], json!("hammer-mcp"));
    assert_eq!(init["result"]["protocolVersion"], json!("2025-06-18"));
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(init["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("save_map"));
    s.notify("notifications/initialized", json!({})).await;
    let listing = s.request(2, "tools/list", json!({})).await;
    let tools = listing["result"]["tools"].as_array().unwrap();
    // the last block is the five-sound-tool block, exactly `SOUND_TOOL_ORDER` (the
    // `tests/server.rs` order pin)
    assert_eq!(
        tools.len(),
        26 + hammer_mcp::tools::hammer::ENTITY_TOOL_ORDER.len()
            + hammer_mcp::tools::modeldoc::MODELDOC_TOOL_ORDER.len()
            + hammer_mcp::tools::particles::PET_TOOL_ORDER.len()
            + hammer_mcp::tools::props::PROP_TOOL_ORDER.len()
            + hammer_mcp::tools::triggers::TRIGGER_TOOL_ORDER.len()
            + hammer_mcp::tools::sounds::SOUND_TOOL_ORDER.len()
            + hammer_mcp::tools::jobs::JOB_TOOL_ORDER.len()
            + hammer_mcp::tools::selection_live::SELECTION_TOOL_ORDER.len()
            + hammer_mcp::tools::game_live::GAME_TOOL_ORDER.len()
            + hammer_mcp::tools::addon::ADDON_TOOL_ORDER.len()
            + 1 // align_texture (WP1.5), then the layout tools (WP1.2) register last
            + hammer_mcp::tools::layout::LAYOUT_TOOL_ORDER.len()
            + hammer_mcp::tools::new_map::NEW_MAP_TOOL_ORDER.len() // new_map registers after the layout tools
            + 4 // look (WP3.1), report_ready (WP3.2), get_playbook (P4), cs_script register last
    );
    // the game block is the tail of the listing, then (backwards) the selection, job and sound
    // blocks
    let names: Vec<&str> = tools.iter().filter_map(|tool| tool["name"].as_str()).collect();
    let sel = hammer_mcp::tools::selection_live::SELECTION_TOOL_ORDER.len();
    let job = hammer_mcp::tools::jobs::JOB_TOOL_ORDER.len();
    let snd = hammer_mcp::tools::sounds::SOUND_TOOL_ORDER.len();
    let game = hammer_mcp::tools::game_live::GAME_TOOL_ORDER.len();
    let layout = hammer_mcp::tools::layout::LAYOUT_TOOL_ORDER.len();
    let new_map = hammer_mcp::tools::new_map::NEW_MAP_TOOL_ORDER.len();
    assert_eq!(names[names.len() - 6..], ["look", "report_ready", "get_playbook", "cs_script", "prefab", "player_model"]);
    let names = &names[..names.len() - 6];
    assert_eq!(names[names.len() - new_map..], hammer_mcp::tools::new_map::NEW_MAP_TOOL_ORDER);
    let names = &names[..names.len() - new_map];
    assert_eq!(names[names.len() - layout..], hammer_mcp::tools::layout::LAYOUT_TOOL_ORDER);
    let names = &names[..names.len() - layout];
    let (align, names) = names.split_last().unwrap();
    assert_eq!(*align, "align_texture");
    let addon = hammer_mcp::tools::addon::ADDON_TOOL_ORDER.len();
    assert_eq!(names[names.len() - addon..], hammer_mcp::tools::addon::ADDON_TOOL_ORDER);
    let names = &names[..names.len() - addon];
    assert_eq!(names[names.len() - game..], hammer_mcp::tools::game_live::GAME_TOOL_ORDER);
    let n = names.len() - game;
    assert_eq!(names[n - sel..n], hammer_mcp::tools::selection_live::SELECTION_TOOL_ORDER);
    assert_eq!(names[n - sel - job..n - sel], hammer_mcp::tools::jobs::JOB_TOOL_ORDER);
    assert_eq!(names[n - sel - job - snd..n - sel - job], hammer_mcp::tools::sounds::SOUND_TOOL_ORDER);
    assert_eq!(tools[0]["name"], json!("session_status"));
    assert!(tools.iter().all(|t| t.get("inputSchema").is_some()));
    // every tool carries annotations from its Mutability: openWorldHint:false everywhere
    assert!(tools
        .iter()
        .all(|t| t["annotations"]["openWorldHint"] == json!(false)));
    let ann = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == json!(name))
            .unwrap_or_else(|| panic!("tool {name}"))["annotations"]
            .clone()
    };
    assert_eq!(ann("session_status")["readOnlyHint"], json!(true), "ReadOnly");
    assert!(ann("save_map").get("readOnlyHint").is_none(), "false is the MCP default: omitted");
    assert!(ann("save_map").get("destructiveHint").is_none(), "Writer: destructive is the MCP default: omitted");
    assert!(ann("set_node_transform").get("readOnlyHint").is_none());
    assert_eq!(
        ann("set_node_transform")["destructiveHint"],
        json!(false),
        "a live edit is undoable and unsaved"
    );
    for discarding in ["reload_map", "model_reload", "particle_reload"] {
        assert!(ann(discarding).get("destructiveHint").is_none(), "{discarding} drops unsaved work (destructive = MCP default, omitted)");
        assert!(ann(discarding).get("readOnlyHint").is_none(), "{discarding}");
    }
    let status = s
        .request(
            3,
            "tools/call",
            json!({"name": "session_status", "arguments": {}}),
        )
        .await;
    assert_eq!(status["result"]["isError"], json!(false));
    let payload: serde_json::Value =
        serde_json::from_str(status["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["bridge"]["running"], json!(true));
    // gated tool -> tool-level error result, not a protocol error (the recorded baseline does
    // not advertise view.render; run_map is no longer gated: it needs no bridge method)
    let gated = s
        .request(4, "tools/call", json!({"name": "render_view", "arguments": {}}))
        .await;
    assert_eq!(gated["result"]["isError"], json!(true));
    let err: serde_json::Value =
        serde_json::from_str(gated["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(err["error"]["name"], json!("NotImplemented"));
    assert_eq!(err["error"]["retrySafe"], json!(true));
    // unknown tool -> JSON-RPC invalid params; unknown method -> method not found
    let unknown = s
        .request(
            5,
            "tools/call",
            json!({"name": "undo.redo", "arguments": {}}),
        )
        .await;
    assert_eq!(unknown["error"]["code"], json!(-32602));
    assert!(unknown["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Unknown tool"));
    // a misspelt name is told the nearest real one; a nonsense name gets no suggestion
    let typo = s
        .request(8, "tools/call", json!({"name": "save_mpa", "arguments": {}}))
        .await;
    assert_eq!(typo["error"]["code"], json!(-32602));
    assert_eq!(typo["error"]["data"]["type"], json!("UnknownTool"));
    assert!(typo["error"]["data"]["didYouMean"]
        .as_array()
        .unwrap()
        .contains(&json!("save_map")));
    assert!(typo["error"]["message"].as_str().unwrap().contains("save_map"));
    let nonsense = s
        .request(9, "tools/call", json!({"name": "frobnicate_widgets", "arguments": {}}))
        .await;
    assert_eq!(nonsense["error"]["code"], json!(-32602));
    assert!(nonsense["error"]["data"].get("didYouMean").is_none());
    assert!(!nonsense["error"]["message"].as_str().unwrap().contains("did you mean"));
    let bogus = s.request(6, "bogus", json!({})).await;
    assert_eq!(bogus["error"]["code"], json!(-32601));
    let ping = s.request(7, "ping", json!({})).await;
    assert!(ping.get("result").is_some());
    // clean shutdown on EOF; stdout carried nothing but JSON-RPC frames
    let (code, stdout_lines, stderr) = s.close().await;
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    for l in &stdout_lines {
        serde_json::from_str::<serde_json::Value>(l)
            .unwrap_or_else(|_| panic!("non-JSON on stdout: {l:?}"));
    }
    assert!(stderr.contains("hammer-mcp starting"), "logs go to stderr");
}

// -------------------------------------------------------------------------------------------
// (a) the shutdown cleanup, over the real stdio transport (stdin EOF)
// -------------------------------------------------------------------------------------------

/// A project tree with a `maps/probe.vmap` and a fake podman: `ps` reports one tools container,/// `exec` answers the orphan scan, records the tagged kill, and leaves a compile exec RUNNING
/// (the compile streams for minutes; the point of this test is a job that is still in flight when
/// the client goes away).
fn shutdown_tree(tag: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "stdio-shutdown-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("workshop/run/app/content/csgo_addons/mcp_test/maps"))
        .expect("tree");
    std::fs::write(
        root.join("workshop/run/app/content/csgo_addons/mcp_test/maps/probe.vmap"),
        b"fake vmap",
    )
    .expect("the source map");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let body = format!(
        r#"#!/bin/bash
DIR='{}'
case "$1" in
  ps)
    printf '[{{"Names":["cs2tools"],"State":"running"}}]'
    exit 0 ;;
  exec)
    last="${{!#}}"
    case "$last" in
      *tagged-scan*) printf 'tagged-scan: jobs=\n'; exit 0 ;;
      *tagged-kill*) printf '%s' "$last" > "$DIR/kill-argv"; printf 'tagged-kill: matched=1 killed=1 remaining=0\n'; exit 0 ;;
    esac
    printf '%s' "$last" > "$DIR/compile-argv"
    sleep 10 ;;
esac
"#,
        bin.display()
    );
    std::fs::write(bin.join("podman"), body).expect("fake podman");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(bin.join("podman"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    root
}

/// (a): a compile job that is still RUNNING when the client closes stdin must not leave its
/// compiler in the container: the binary runs the tagged kill (bounded) before it exits.
#[tokio::test]
async fn stdin_eof_kills_the_tagged_tree_of_a_running_compile_job() {
    let root = shutdown_tree("eof");
    let podman = root.join("bin/podman");
    let root_arg = root.display().to_string();
    let podman_arg = podman.display().to_string();
    let args = [
        "--project-root",
        &root_arg,
        "--podman",
        &podman_arg,
        "--vk-layer-dir",
        "/nonexistent/cs2mcp-stdio-vklayer",
        "--xauthority",
        "/nonexistent/cs2mcp-stdio-xauth",
    ];
    let mut s = cs2_mcp_common::mock::stdio::Server::spawn(
        env!("CARGO_BIN_EXE_hammer-mcp"),
        &args,
    )
    .await;
    let init = s
        .request(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18", "clientInfo": {"name": "test", "version": "0"}, "capabilities": {}}),
        )
        .await;
    assert_eq!(init["result"]["serverInfo"]["name"], json!("hammer-mcp"));
    s.notify("notifications/initialized", json!({})).await;
    let started = s
        .request(
            2,
            "tools/call",
            json!({"name": "compile_map", "arguments": {"path": "maps/probe.vmap", "timeoutMs": 60_000}}),
        )
        .await;
    let payload: serde_json::Value =
        serde_json::from_str(started["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["state"], json!("running"), "{payload}");
    let job_id = payload["jobId"].as_str().unwrap().to_string();

    // the job's exec really started (bounded wait, no blind sleep): the job is in flight now
    let compile_argv = root.join("bin/compile-argv");
    let at = std::time::Instant::now();
    while !compile_argv.is_file() {
        assert!(at.elapsed() < std::time::Duration::from_secs(20), "no compile exec ran");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    // N1: the tag VALUE is unique per process: it is prefixed by the job's id but is NOT the
    // poll handle itself
    let compiled = std::fs::read_to_string(&compile_argv).unwrap();
    let tag = compiled
        .split("export CS2MCP_COMPILE_JOB=")
        .nth(1)
        .and_then(|rest| rest.split(';').next())
        .unwrap_or_default()
        .to_string();
    assert!(
        tag.starts_with(&format!("{job_id}-")) && tag.len() > job_id.len() + 2,
        "the compile exec carries a per-process job tag: {compiled}"
    );

    // the client goes away: the binary must end this job's tagged tree before it exits
    let (code, _, stderr) = s.close().await;
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let kill = root.join("bin/kill-argv");
    assert!(
        kill.is_file(),
        "the shutdown cleanup ran the tagged kill\nstderr:\n{stderr}"
    );
    let kill_argv = std::fs::read_to_string(&kill).unwrap();
    assert!(
        kill_argv.contains(&format!("for want in {tag}")),
        "the kill targets exactly this job's per-process tag: {kill_argv}"
    );
    assert!(kill_argv.contains("tagged-kill"), "{kill_argv}");
    assert!(
        stderr.contains("compile-job cleanup"),
        "the cleanup is reported in the log: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// (a): the same cleanup on SIGTERM (`kill -TERM`), which is how an orchestrator stops a server.
/// Hand-rolled over `std::process` because the signal has to go to the child's pid.
#[test]
fn sigterm_kills_the_tagged_tree_of_a_running_compile_job() {
    use std::io::{BufRead, BufReader, Write};

    let root = shutdown_tree("sigterm");
    let root_arg = root.display().to_string();
    let podman_arg = root.join("bin/podman").display().to_string();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hammer-mcp"))
        .args([
            "--project-root",
            &root_arg,
            "--podman",
            &podman_arg,
            "--vk-layer-dir",
            "/nonexistent/cs2mcp-stdio-sig-vklayer",
            "--xauthority",
            "/nonexistent/cs2mcp-stdio-sig-xauth",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the server");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    for frame in [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "test", "version": "0"}, "capabilities": {}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "compile_map", "arguments": {"path": "maps/probe.vmap", "timeoutMs": 60_000}}}),
    ] {
        writeln!(stdin, "{frame}").expect("write a frame");
    }
    stdin.flush().expect("flush");
    // read until the tools/call reply (its payload carries the running job's id)
    let job_id;
    loop {
        let mut line = String::new();
        assert!(
            stdout.read_line(&mut line).expect("read a frame") > 0,
            "the server closed stdout before the compile_map reply"
        );
        let value: serde_json::Value = serde_json::from_str(&line).expect("JSON-RPC frame");
        if value.get("id") == Some(&json!(2)) {
            let payload: serde_json::Value = serde_json::from_str(
                value["result"]["content"][0]["text"].as_str().unwrap(),
            )
            .unwrap();
            assert_eq!(payload["state"], json!("running"), "{payload}");
            job_id = payload["jobId"].as_str().unwrap().to_string();
            break;
        }
    }
    let compile_argv = root.join("bin/compile-argv");
    let at = std::time::Instant::now();
    while !compile_argv.is_file() {
        assert!(at.elapsed() < std::time::Duration::from_secs(20), "no compile exec ran");
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let compiled = std::fs::read_to_string(&compile_argv).unwrap();
    let tag = compiled
        .split("export CS2MCP_COMPILE_JOB=")
        .nth(1)
        .and_then(|rest| rest.split(';').next())
        .unwrap_or_default()
        .to_string();
    assert!(
        tag.starts_with(&format!("{job_id}-")),
        "the compile exec carries a per-process job tag: {compiled}"
    );

    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("kill -TERM");
    assert!(status.success(), "kill -TERM failed");
    // bounded wait for the exit (no blind sleep: the process must really be gone)
    let at = std::time::Instant::now();
    loop {
        if let Some(exit) = child.try_wait().expect("try_wait") {
            assert_eq!(
                exit.code(),
                Some(128 + 15),
                "the signal path leaves the conventional 128+SIGTERM status (review-2 N7)"
            );
            break;
        }
        assert!(
            at.elapsed() < std::time::Duration::from_secs(20),
            "the server did not exit after SIGTERM"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let kill = root.join("bin/kill-argv");
    assert!(
        kill.is_file(),
        "SIGTERM ran the shutdown cleanup too (kill-argv missing)"
    );
    let kill_argv = std::fs::read_to_string(&kill).unwrap();
    assert!(
        kill_argv.contains(&format!("for want in {tag}")),
        "the kill targets the running job's per-process tag: {kill_argv}"
    );
    let _ = std::fs::remove_dir_all(&root);
}


