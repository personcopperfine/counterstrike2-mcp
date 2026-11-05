//! Server-level contract tests against the mock bridge (POC `ServerContract` subset that applies
//! to the bootstrap: tool list, forbidden names, session_status live/dead, capability gating,
//! explicit deadlines).
#![allow(clippy::result_large_err)]
mod common;

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mock::{make_doc, MockBridge};
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::tools::hammer::{ENTITY_TOOL_ORDER, POC_TOOL_ORDER};
use hammer_mcp::tools::jobs::JOB_TOOL_ORDER;
use hammer_mcp::tools::model_render::MODEL_RENDER_TOOL_ORDER;
use hammer_mcp::tools::modeldoc::MODELDOC_TOOL_ORDER;
use hammer_mcp::tools::particles::PET_TOOL_ORDER;
use hammer_mcp::tools::props::PROP_TOOL_ORDER;
use hammer_mcp::tools::selection_live::SELECTION_TOOL_ORDER;
use hammer_mcp::tools::game_live::GAME_TOOL_ORDER;
use hammer_mcp::tools::layout::LAYOUT_TOOL_ORDER;
use hammer_mcp::tools::sounds::SOUND_TOOL_ORDER;
use hammer_mcp::tools::triggers::TRIGGER_TOOL_ORDER;
use serde_json::{json, Value};
use std::path::PathBuf;

struct Host {
    dir: PathBuf,
}
impl Host {
    fn new(tag: &str) -> Host {
        let dir = std::env::temp_dir().join(format!("cs2mcp-hammer-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("content/csgo_addons/mcp_test/maps")).unwrap();
        std::fs::write(
            dir.join("content/csgo_addons/mcp_test/maps/probe.vmap"),
            b"vmap",
        )
        .unwrap();
        Host { dir }
    }
    fn vmap(&self) -> PathBuf {
        self.dir
            .join("content/csgo_addons/mcp_test/maps/probe.vmap")
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn config(discovery: &str, host: Option<&Host>) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let path_map = host.map(|h| format!("z:\\app={}", h.dir.display()));
    let mut args = vec!["--discovery", discovery, "--deadline-ms", "5000"];
    if let Some(p) = &path_map {
        args.extend(["--path-map", p.as_str()]);
    }
    let mut argv = vec!["t"];
    argv.extend(common::hermetic(&args));
    W::parse_from(argv).c
}

async fn call(
    server: &cs2_mcp_common::mcp::McpServer<hammer_mcp::state::HammerState>,
    name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

#[test]
fn tools_list_matches_the_poc_order_then_entities_then_modeldoc_then_particles() {
    let server = hammer_mcp::build_server(&config("/tmp/none/session.json", None)).unwrap();
    let names = server.registry().names();
    let ent_end = POC_TOOL_ORDER.len() + ENTITY_TOOL_ORDER.len();
    let md_end = ent_end + MODELDOC_TOOL_ORDER.len();
    let pet_end = md_end + PET_TOOL_ORDER.len();
    let props_end = pet_end + PROP_TOOL_ORDER.len();
    let trig_end = props_end + TRIGGER_TOOL_ORDER.len();
    let snd_end = trig_end + SOUND_TOOL_ORDER.len();
    let job_end = snd_end + JOB_TOOL_ORDER.len();
    let sel_end = job_end + SELECTION_TOOL_ORDER.len();
    let game_end = sel_end + GAME_TOOL_ORDER.len();
    let addon_end = game_end + hammer_mcp::tools::addon::ADDON_TOOL_ORDER.len();
    assert_eq!(names[game_end..addon_end], hammer_mcp::tools::addon::ADDON_TOOL_ORDER);
    // `align_texture` (WP1.5) registers after the addon tools, then the layout tools (WP1.2).
    // and `new_map` (a document-creating mutator) last.
    let layout_end = addon_end + 1 + LAYOUT_TOOL_ORDER.len();
    // `look` (WP3.1) then `report_ready` (WP3.2) register after `new_map`, last
    let new_map_end = layout_end + hammer_mcp::tools::new_map::NEW_MAP_TOOL_ORDER.len();
    assert_eq!(names.len(), new_map_end + 6);
    assert_eq!(names[new_map_end], "look");
    assert_eq!(names[new_map_end + 1], "report_ready");
    assert_eq!(names[new_map_end + 2], "get_playbook");
    assert_eq!(names[new_map_end + 3], "cs_script"); // the script project tool registers after the playbooks
    assert_eq!(names[new_map_end + 4], "prefab"); // then the prefab tool (I4)
    assert_eq!(names[new_map_end + 5], "player_model"); // then player_model (I7)
    assert_eq!(names[addon_end], "align_texture");
    assert_eq!(names[addon_end + 1..layout_end], LAYOUT_TOOL_ORDER);
    assert_eq!(names[layout_end..new_map_end], hammer_mcp::tools::new_map::NEW_MAP_TOOL_ORDER);
    assert_eq!(names[sel_end..game_end], GAME_TOOL_ORDER);
    assert_eq!(names[snd_end..job_end], JOB_TOOL_ORDER);
    assert_eq!(names[job_end..sel_end], SELECTION_TOOL_ORDER);
    assert_eq!(names[pet_end..props_end], PROP_TOOL_ORDER);
    assert_eq!(names[props_end..trig_end], TRIGGER_TOOL_ORDER);
    assert_eq!(names[trig_end..snd_end], SOUND_TOOL_ORDER);
    assert_eq!(names[..24], POC_TOOL_ORDER);
    assert_eq!(names[24..ent_end], ENTITY_TOOL_ORDER);
    assert_eq!(names[ent_end..md_end], MODELDOC_TOOL_ORDER);
    assert_eq!(names[md_end..pet_end], PET_TOOL_ORDER);
    // the modeldoc block ends with model_render, whose own module order is that one-tool suffix
    assert_eq!(
        names[md_end - MODEL_RENDER_TOOL_ORDER.len()..md_end],
        MODEL_RENDER_TOOL_ORDER
    );
    assert_eq!(
        &MODELDOC_TOOL_ORDER[MODELDOC_TOOL_ORDER.len() - MODEL_RENDER_TOOL_ORDER.len()..],
        MODEL_RENDER_TOOL_ORDER
    );
    let parts = [
        &hammer_mcp::tools::particle_file::PARTICLE_FILE_TOOL_ORDER[..],
        &hammer_mcp::tools::particle_live::PARTICLE_LIVE_TOOL_ORDER[..],
        &hammer_mcp::tools::particle_live_edit::PARTICLE_LIVE_EDIT_TOOL_ORDER[..],
        &hammer_mcp::tools::particle_render::PARTICLE_RENDER_TOOL_ORDER[..],
    ]
    .concat();
    assert_eq!(PET_TOOL_ORDER.to_vec(), parts);
    for forbidden in [
        "console.command",
        "undo.redo",
        "job.cancel",
        "console_command",
        "undo_redo",
        "entity.delete",
        "mesh.setFaceRect",
    ] {
        assert!(!names.contains(&forbidden));
    }
    let writers: Vec<_> = server
        .registry()
        .describe()
        .into_iter()
        .filter(|s| s.mutability == cs2_mcp_common::tools::Mutability::Writer)
        .map(|s| s.name)
        .collect();
    // one writer per document kind (map, editor save) plus the explicit file writers —
    // compile_map joined the map kind (it writes the .vpk, and open_map/reload_map stay Mutating
    // like the rest of the lifecycle); the sound file writers (edit_soundevents,
    // compile_soundevents) follow the file-route particle writers
    assert_eq!(
        writers,
        vec![
            "save_map",
            "compile_map",
            "model_edit",
            "model_create",
            "model_compile",
            "model_save",
            "model_import",
            "particle_edit",
            "particle_create",
            "particle_compile",
            "particle_save",
            "edit_soundevents",
            "compile_soundevents",
            // the job tools (tools/jobs.rs): compile_asset starts a bridge job, job_cancel ends one
            "compile_asset",
            "job_cancel",
            // the script project (tools/cs_script.rs) registers last
            "cs_script",
            // player_model (I7) is a cs_script writer too
            "player_model",
        ]
    );
}

#[tokio::test]
async fn unknown_tool_is_a_protocol_error() {
    let server = hammer_mcp::build_server(&config("/tmp/none/session.json", None)).unwrap();
    assert!(matches!(
        server.call("undo.redo", json!({})).await,
        Err(CallFailure::UnknownTool(_))
    ));
}

#[tokio::test]
async fn session_status_live() {
    let host = Host::new("live");
    let mb = MockBridge::builder()
        .doc(Some(make_doc(
            "z:/app/content/csgo_addons/mcp_test/maps/probe.vmap",
            Some(false),
        )))
        .start()
        .await
        .unwrap();
    let server =
        hammer_mcp::build_server(&config(mb.discovery_path.to_str().unwrap(), Some(&host)))
            .unwrap();
    let p = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(p["bridge"]["running"], json!(true));
    assert_eq!(p["bridge"]["bridgeVersion"], json!(mb.build));
    assert!(
        p["bridge"].get("token").is_none(),
        "token must never leave the client"
    );
    assert!(p["capabilities"]["methods"]
        .as_array()
        .unwrap()
        .contains(&json!("doc.active")));
    assert_eq!(p["document"]["id"], json!("d1"));
    assert_eq!(p["document"]["pathMapped"], json!(true));
    assert_eq!(
        p["document"]["hostPath"],
        json!(host.vmap().display().to_string())
    );
    assert_eq!(p["liveness"]["hammerBlocked"], json!(false));
    assert_eq!(p["unsavedWarning"], Value::Null);
    assert_eq!(p["server"]["unsavedBridgeEdits"], json!(0));
    for r in mb.requests() {
        assert!(r.params["deadlineMs"].as_u64().unwrap_or(0) > 0, "{r:?}");
    }
}

#[tokio::test]
async fn session_status_dead_bridge_never_raises() {
    let server =
        hammer_mcp::build_server(&config("/tmp/cs2mcp-no-bridge/session.json", None)).unwrap();
    let p = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(p["bridge"]["running"], json!(false));
    assert_eq!(p["bridge"]["error"]["name"], json!("BridgeNotRunning"));
    assert!(p["launch"]["command"]
        .as_str()
        .unwrap()
        .ends_with("cargo bridge runtime up"));
    assert!(p.get("document").is_none());
}

#[tokio::test]
async fn session_status_unmapped_path_is_reported_not_guessed() {
    let mb = MockBridge::builder()
        .doc(Some(make_doc("q:/nowhere/probe.vmap", Some(false))))
        .start()
        .await
        .unwrap();
    let server =
        hammer_mcp::build_server(&config(mb.discovery_path.to_str().unwrap(), None)).unwrap();
    let p = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(p["document"]["pathMapped"], json!(false));
    assert_eq!(p["document"]["hostPath"], Value::Null);
}

#[tokio::test]
async fn session_status_hammer_not_loaded() {
    let mb = MockBridge::builder()
        .responder_value(
            "doc.active",
            json!({"doc": null, "reason": "hammer_not_loaded"}),
        )
        .start()
        .await
        .unwrap();
    let server =
        hammer_mcp::build_server(&config(mb.discovery_path.to_str().unwrap(), None)).unwrap();
    let p = call(&server, "session_status", json!({})).await.unwrap();
    assert_eq!(p["documentReason"], json!("hammer not loaded"));
    assert_eq!(p["documentError"]["name"], json!("HammerNotLoaded"));
}

#[tokio::test]
async fn capability_gating_on_restricted_caps() {
    let mb = MockBridge::builder()
        .doc(Some(make_doc("z:/app/x.vmap", Some(false))))
        .methods([
            "control.ping",
            "control.status",
            "control.capabilities",
            "control.version",
            "control.cancel",
            "control.shutdown",
            "doc.active",
        ])
        .start()
        .await
        .unwrap();
    let server =
        hammer_mcp::build_server(&config(mb.discovery_path.to_str().unwrap(), None)).unwrap();
    for (tool, args, missing) in [
        ("save_map", json!({}), vec!["map.save"]),
        (
            "create_entities",
            json!({"entities": [{"class": "light", "origin": [0, 0, 0]}]}),
            vec!["edit.batch", "entity.create"],
        ),
        ("reload_map", json!({}), vec!["map.reload"]),
        (
            "open_map",
            json!({"path": "maps/x.vmap"}),
            vec!["map.open"],
        ),
        ("undo_last", json!({}), vec!["undo.undo"]),
        ("render_view", json!({}), vec!["view.render"]),
        ("create_block", json!({}), vec!["mesh.create"]),
    ] {
        let err = call(&server, tool, args).await.unwrap_err();
        assert_eq!(err.name, "NotImplemented", "{tool}");
        assert_eq!(err.code, Some(-32011), "{tool}");
        assert_eq!(err.data["missing"], json!(missing), "{tool}");
        assert!(err.retry_safe, "{tool}");
    }
    assert!(mb.method_calls("map.save").is_empty());
    assert!(mb.method_calls("edit.batch").is_empty());
}

#[tokio::test]
async fn dead_bridge_is_not_reported_as_missing_capability() {
    let server =
        hammer_mcp::build_server(&config("/tmp/cs2mcp-no-bridge/session.json", None)).unwrap();
    let err = call(&server, "save_map", json!({})).await.unwrap_err();
    assert_eq!(err.name, "BridgeNotRunning");
}

#[tokio::test]
async fn render_tools_run_when_the_capabilities_allow_it() {
    // a tiny real render through the whole server: the mock writes its tiny PNG into the
    // directory the --path-map sends `Z:\app\renders` to, the tool maps + reads it
    let host = Host::new("render-server");
    let mb = MockBridge::builder()
        .renders()
        .render_dir(host.dir.join("renders"))
        .start()
        .await
        .unwrap();
    let server =
        hammer_mcp::build_server(&config(mb.discovery_path.to_str().unwrap(), Some(&host)))
            .unwrap();
    let out = call(&server, "render_view", json!({"inline": false})).await.unwrap();
    assert_eq!(out["camera"]["mode"], json!("viewport"));
    assert_eq!(out["stats"]["nonBlackPixels"], json!(640 * 360));
    assert_eq!(out["inline"], Value::Null);
    assert!(out["hostPath"].is_string());
}
