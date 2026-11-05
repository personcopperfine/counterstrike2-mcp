//! The bridge half of every server's `session_status` (port of the POC `h_session_status`):
//! never fails; reports connectivity, discovery (secret-free), capabilities/version/status,
//! liveness and — when the bridge is down — the launch command (attach-only policy).
use crate::bridge::{discovery::pid_alive, BridgeClient};
use serde_json::{json, Map, Value};

pub struct BridgeSection {
    /// `true` when hello succeeded on this probe
    pub connected: bool,
    /// members to merge into the tool result: `bridge`, `notes`, `launch?`, `capabilities?`, `version?`, `status?`, `liveness?`
    pub fields: Map<String, Value>,
}

/// The "bridge not reachable" note: the container launch path, or on a Windows host the native
/// launcher (crates/cs2-native FIXME 18).
fn unreachable_note() -> &'static str {
    if cfg!(windows) {
        "bridge not reachable: start the session with `cs2mcp-native up` (it prepares the install, \
         passes -insecure and loads the bridge; `cs2mcp-native status` shows its readiness), or load \
         it in a running -insecure -tools session (tool_request cs2_bridge)"
    } else {
        "bridge not reachable: load it in a live tools session (tool_request cs2_bridge {}) or use the launch path (bridge/runtime.sh up); see docs/WORKFLOW.md §11"
    }
}

pub async fn collect(client: &mut BridgeClient, launch_command: &str) -> BridgeSection {
    let mut out = Map::new();
    let mut notes: Vec<Value> = Vec::new();
    let mut bridge = json!({
        "running": false,
        "connected": client.connected(),
        "discoveryPath": client.discovery_path().display().to_string(),
    });
    let disc = match client.ensure_connected().await {
        Ok(d) => d,
        Err(e) => {
            bridge["error"] = e.to_json();
            notes.push(json!(unreachable_note()));
            out.insert("bridge".into(), bridge);
            out.insert("notes".into(), Value::Array(notes));
            out.insert(
                "launch".into(),
                json!({
                    "command": launch_command,
                    "note": "the server is attach-only: it runs the launch command once at startup only with --launch",
                }),
            );
            return BridgeSection {
                connected: false,
                fields: out,
            };
        }
    };
    bridge["running"] = json!(true);
    bridge["connected"] = json!(true);
    if let Value::Object(d) = disc.to_json() {
        for (k, v) in d {
            bridge[k] = v;
        }
    }
    bridge["pidAlive"] = json!(pid_alive(disc.pid));
    out.insert("bridge".into(), bridge);
    match client.capabilities().await {
        Ok(v) => out.insert("capabilities".into(), v),
        Err(e) => out.insert("capabilitiesError".into(), e.to_json()),
    };
    match client.version().await {
        Ok(v) => out.insert("version".into(), v),
        Err(e) => out.insert("versionError".into(), e.to_json()),
    };
    match client.status().await {
        Ok(v) => out.insert("status".into(), v),
        Err(e) => out.insert("statusError".into(), e.to_json()),
    };
    out.insert("liveness".into(), client.liveness().await.to_json());
    out.insert("notes".into(), Value::Array(notes));
    BridgeSection {
        connected: true,
        fields: out,
    }
}
