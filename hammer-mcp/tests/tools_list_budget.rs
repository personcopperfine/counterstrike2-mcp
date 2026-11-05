//! Guard against `tools/list` regrowth (streamline WP2.2, workflows/streamline/PLAN.md).
//! Every agent pays for the whole list on every turn, so descriptions stay short; history and
//! implementation notes live in hammer-mcp/AGENT.md ("Tool notes").
#![allow(clippy::result_large_err)]

mod common;

use serde_json::{json, Value};

/// WP2.2 target for the compact tools/list JSON (name + description + inputSchema + annotations):
/// about half of the 93847 B before the streamline project. The text pass alone cannot reach it:
/// the schema structure (names, types, enums, min/max, annotations) is ~41 KB by itself, so
/// reaching the target needs schema changes (see the WP2.2 report). Documentation only.
#[allow(dead_code)]
const TARGET_TOOLS_LIST_BYTES: usize = 47_000;
/// Ceiling for the REAL wire size: the compact JSON of the `tools` array in a `tools/list` reply
/// (what `workflows/streamline/toolsdump.py` measures), annotations included. Set to the size after
/// the WP2.2 review fixes and the shared-text/annotation trim (80679 B), rounded up by < 1 KB
/// (81_500). WP3.1 `look` added 1837 B and WP3.2 `report_ready` 1273 B (with its review fixes);
/// MAX was raised by exactly those; the run-4 fixes (report_ready `mode`, look/compile_map text) added 266 B (83789 -> 84055); P4 `get_playbook` added 686 B (84055 -> 84741 measured after the merge); the review fixes made get_playbook's description 76 B longer (84817, MAX = measured); P5 `find_assets` + the `get_model_info` facing text added 1156 B on top (84817 -> 85973 measured after the merge, MAX = measured); the `cs_script` tool (8 actions, one tool instead of five) plus `js` in compile_asset's extension list added 1451 B (85973 -> 87424, MAX = measured); the `model_import` tool (I1: one description, ten short properties) added 1324 B (87424 -> 88748, MAX = measured); I2 (`prop_materials` check text in report_ready, `materials:\"existing\"` + `materialMap` on model_import) added 470 B (88748 -> 89218, MAX = measured); I3 (`kind:\"hammer_mesh\"` + `origin`/`angles`/`maxTrisPerMesh` on model_import, `mode:\"axes\"` on align_texture) added 645 B (89218 -> 89863, MAX = measured); I4 (`prefab`: four actions in one tool, plus a clause on list_entities) added 1394 B (89863 -> 91257, MAX = measured); the I3/I4 review made `maxTrisPerMesh` minimum 30 (+1 B, 91258); I5 (skinned import: the model_import description names skeleton/animations) added 86 B (91258 -> 91344, MAX = measured); I6 (`kind:\"player\"`, `boneMap`, `rig` on model_import, `skeleton` include and the agents/skeleton paths on get_model_info; the model_import description was trimmed to fit its cap) added 318 B (91344 -> 91662, MAX = measured); I7 (`player_model`: one tool, three actions, three short properties) added 860 B (91662 -> 92522, MAX = measured); I8 (`look` takes a prefab nodeID, `get_playbook` lists `import_model` with `source`/`kind`) added 202 B (92522 -> 92724, MAX = measured); the player attachments/hitboxes (`referenceAgent` on model_import, a clause in its description) added 170 B (92724 -> 92894, MAX = measured). A later WP may only lower it.
const MAX_TOOLS_LIST_BYTES: usize = 92_894;
/// One tool description, and one schema property description, at most this long (WP2.2).
const MAX_DESCRIPTION_CHARS: usize = 700;
const MAX_PROPERTY_DESCRIPTION_CHARS: usize = 120;
/// The hard tools carry result contracts and failure modes an agent cannot get elsewhere (agents
/// read only tools/list), so they get a higher description cap. Named exceptions, not a general raise.
const HARD_TOOL_DESCRIPTION_CHARS: usize = 1_100;
const HARD_TOOLS: &[&str] = &[
    "play_test", "compile_map", "run_map", "new_map", "create_trigger", "create_sound_entities",
];

fn walk(v: &Value, tool: &str, out: &mut Vec<(String, usize, String)>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                match (k.as_str(), x) {
                    ("description", Value::String(s)) => out.push((tool.to_string(), s.chars().count(), s.clone())),
                    _ => walk(x, tool, out),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| walk(x, tool, out)),
        _ => {}
    }
}

#[tokio::test]
async fn tools_list_stays_small() {
    let mb = cs2_mcp_common::mock::MockBridge::start().await.unwrap();
    let mut s = cs2_mcp_common::mock::stdio::Server::spawn(
        env!("CARGO_BIN_EXE_hammer-mcp"),
        &common::hermetic(&["--discovery", mb.discovery_path.to_str().unwrap()]),
    )
    .await;
    s.request(1, "initialize", json!({"protocolVersion": "2025-06-18", "clientInfo": {"name": "test", "version": "0"}, "capabilities": {}})).await;
    s.notify("notifications/initialized", json!({})).await;
    let listing = s.request(2, "tools/list", json!({})).await;
    let tools = listing["result"]["tools"].as_array().unwrap();
    assert!(listing["result"].get("nextCursor").is_none(), "tools/list is paginated; measure every page");
    let total = serde_json::to_string(tools).unwrap().len();

    let mut too_long = Vec::new();
    for t in tools {
        let name = t["name"].as_str().unwrap();
        let desc = t["description"].as_str().unwrap_or("");
        let cap = if HARD_TOOLS.contains(&name) { HARD_TOOL_DESCRIPTION_CHARS } else { MAX_DESCRIPTION_CHARS };
        if desc.chars().count() > cap {
            too_long.push(format!("{name}: description {} chars (cap {cap})", desc.chars().count()));
        }
        let mut props = Vec::new();
        walk(&t["inputSchema"], name, &mut props);
        for (t, n, text) in props {
            if n > MAX_PROPERTY_DESCRIPTION_CHARS {
                too_long.push(format!("{t}: property description {n} chars: {text}"));
            }
        }
    }
    assert!(too_long.is_empty(), "descriptions over the WP2.2 caps:\n{}", too_long.join("\n"));
    assert!(
        total <= MAX_TOOLS_LIST_BYTES,
        "tools/list is {total} B on the wire, over MAX_TOOLS_LIST_BYTES {MAX_TOOLS_LIST_BYTES}; \
         the WP2.2 target is {TARGET_TOOLS_LIST_BYTES}"
    );
}
