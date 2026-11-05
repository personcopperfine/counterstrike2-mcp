//! `script_run` against a fake podman: both kinds are blocked, so a valid call must refuse with
//! NotImplementedYet and leave nothing behind (no container call, no file written anywhere under
//! the project, no staged script). Invalid arguments are InvalidArguments.
#![allow(clippy::result_large_err)]
use client_mcp::state::ClientConfig;
use cs2_mcp_common::mock::podman::FakePodman;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

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

fn server(
    fake: &FakePodman,
    project: &Path,
) -> cs2_mcp_common::mcp::McpServer<client_mcp::state::ClientState> {
    let bin = fake.bin.display().to_string();
    let proj = project.display().to_string();
    client_mcp::build_server(&config(&[
        "--discovery",
        "/tmp/cs2mcp-no-bridge/session.json",
        "--console-log",
        "/tmp/cs2mcp-no-console.log",
        "--podman",
        &bin,
        "--project-root",
        &proj,
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

/// Every file path under `dir`, sorted (for "nothing was written" checks).
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn script_run_is_registered_and_refuses_both_kinds_before_anything_happens() {
    let fake = FakePodman::new("script-blocked");
    let proj = fake.dir().join("project");
    std::fs::create_dir_all(&proj).unwrap();
    let before = files_under(&proj);
    let srv = server(&fake, &proj);
    assert!(srv.registry().names().contains(&"script_run"));

    for (kind, want_missing) in [("cs_script", "resourcecompiler"), ("vscript", "CNopVM")] {
        let err = call(
            &srv,
            "script_run",
            json!({
                "kind": kind,
                "source": "Instance.Msg('SCRIPT_TEST_PASS smoke');",
                "map": "de_dust2",
                "expect": ["SCRIPT_TEST_PASS smoke"],
                "timeoutMs": 60000
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.type_, "NotImplementedYet", "{kind}");
        assert_eq!(
            err.data["blockedOn"],
            json!("workflows/06-cs-script-vscript.md"),
            "{kind}"
        );
        assert_eq!(err.data["kind"], json!(kind));
        assert!(
            err.data["missing"].as_str().unwrap().contains(want_missing),
            "{kind}: {}",
            err.data["missing"]
        );
        assert!(!err.data["evidence"].as_array().unwrap().is_empty());
        assert_eq!(err.data["staged"], json!(false));
        assert_eq!(err.data["launched"], json!(false));
        assert_eq!(err.data["sent"], json!(false));
        assert_eq!(err.data["requested"]["hasSource"], json!(true));
        assert_eq!(err.data["requested"]["timeoutMs"], json!(60000));
    }

    // Refusal comes before any runtime call: no podman verb, no file written under the project.
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    assert_eq!(files_under(&proj), before);
}

#[tokio::test]
async fn script_run_path_form_is_refused_without_reading_the_file() {
    let fake = FakePodman::new("script-path");
    let proj = fake.dir().join("project");
    std::fs::create_dir_all(&proj).unwrap();
    let srv = server(&fake, &proj);
    // The path does not exist: validation accepts it (only the shape is checked), and nothing is read.
    let err = call(
        &srv,
        "script_run",
        json!({"kind": "vscript", "path": "/nonexistent/probe.lua", "expect": ["x"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "NotImplementedYet");
    assert_eq!(
        err.data["requested"]["path"],
        json!("/nonexistent/probe.lua")
    );
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn script_run_argument_errors_are_invalid_arguments() {
    let fake = FakePodman::new("script-args");
    let proj = fake.dir().join("project");
    std::fs::create_dir_all(&proj).unwrap();
    let srv = server(&fake, &proj);
    for bad in [
        json!({"kind": "lua", "source": "x", "expect": ["x"]}),
        json!({"kind": "cs_script", "expect": ["x"]}),
        json!({"kind": "cs_script", "source": "x", "path": "/x.js", "expect": ["x"]}),
        json!({"kind": "cs_script", "source": "x"}),
        json!({"kind": "cs_script", "source": "x", "expect": []}),
        json!({"kind": "cs_script", "source": "x", "expect": ["(unclosed"]}),
        json!({"kind": "cs_script", "source": "x", "expect": ["x"], "map": "../etc"}),
        json!({"kind": "cs_script", "source": "x", "expect": ["x"], "timeoutMs": 0}),
        json!({"kind": "cs_script", "addon": "bad-name", "expect": ["x"]}),
        json!({"kind": "cs_script", "addon": "cs_script_demo", "expect": ["x"], "entry": "hello"}),
        json!({"kind": "cs_script", "addon": "cs_script_demo", "expect": ["x"], "entry": {"target": "hello"}}),
        json!({"kind": "cs_script", "addon": "cs_script_demo", "expect": ["x"], "anyLine": "yes"}),
        json!({"kind": "vscript", "addon": "cs_script_demo", "expect": ["x"], "source": "print(1)"}),
    ] {
        let err = call(&srv, "script_run", bad.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad}");
    }
    assert!(fake.calls().is_empty());
}
