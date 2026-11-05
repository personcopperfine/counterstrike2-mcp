//! The real stdio transport, driven through the built binary.
mod common;

use serde_json::json;

#[tokio::test]
async fn handshake_tools_call_and_errors_over_stdio() {
    let mb = cs2_mcp_common::mock::MockBridge::builder()
        .responder_value("console.exec", json!({"queued": true}))
        .start()
        .await
        .unwrap();
    let mut s = cs2_mcp_common::mock::stdio::Server::spawn(
        env!("CARGO_BIN_EXE_client-mcp"),
        &common::hermetic(&["--discovery", mb.discovery_path.to_str().unwrap()]),
    )
    .await;
    let init = s.request(1, "initialize", json!({"protocolVersion": "2024-11-05", "clientInfo": {"name": "test", "version": "0"}, "capabilities": {}})).await;
    assert_eq!(init["result"]["serverInfo"]["name"], json!("client-mcp"));
    assert_eq!(init["result"]["protocolVersion"], json!("2024-11-05"));
    s.notify("notifications/initialized", json!({})).await;
    let listing = s.request(2, "tools/list", json!({})).await;
    let names: Vec<&str> = listing["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names[0], "session_status");
    let tools = listing["result"]["tools"].as_array().unwrap();
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
    assert_eq!(ann("session_status")["readOnlyHint"], json!(true));
    assert!(ann("console_exec").get("readOnlyHint").is_none(), "false is the MCP default: omitted");
    assert_eq!(ann("console_exec")["destructiveHint"], json!(false));
    for destructive in ["stop_session", "game_restore", "demo_record_start", "demo_to_video"] {
        assert!(ann(destructive).get("destructiveHint").is_none(), "{destructive}: true is the MCP default: omitted");
        assert!(ann(destructive).get("readOnlyHint").is_none(), "{destructive}");
    }
    assert!(
        names.contains(&"console_exec")
            && names.contains(&"input_set")
            && names.contains(&"run_testscript")
    );
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
    assert_eq!(payload["runtime"]["flavour"], json!("wine-tools"));
    assert_eq!(
        payload["toolGroups"]["console_exec"]["available"],
        json!(true)
    );
    let exec = s
        .request(
            4,
            "tools/call",
            json!({"name": "console_exec", "arguments": {"command": "getpos"}}),
        )
        .await;
    assert_eq!(exec["result"]["isError"], json!(false));
    let refused = s
        .request(
            5,
            "tools/call",
            json!({"name": "console_exec", "arguments": {"command": "quit"}}),
        )
        .await;
    assert_eq!(refused["result"]["isError"], json!(true));
    let unknown = s
        .request(
            6,
            "tools/call",
            json!({"name": "control.shutdown", "arguments": {}}),
        )
        .await;
    assert_eq!(unknown["error"]["code"], json!(-32602));
    let (code, stdout_lines, stderr) = s.close().await;
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    for l in &stdout_lines {
        serde_json::from_str::<serde_json::Value>(l)
            .unwrap_or_else(|_| panic!("non-JSON on stdout: {l:?}"));
    }
    assert!(stderr.contains("client-mcp starting"));
}
