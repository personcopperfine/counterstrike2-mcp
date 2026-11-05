//! hammer-mcp: stdio MCP server for the CS2 Workshop Tools (Hammer first; ModelDoc, pet, asset
//! browser next). Shared plumbing lives in `cs2-mcp-common`; this crate owns the Hammer tool set,
//! the Windows->host path map and the saved-`.vmap` reads. See AGENT.md and workflows/.
#![allow(clippy::result_large_err)] // rich error payloads by design (see cs2-mcp-common)

pub mod cs_script;
pub mod fgd;
pub mod geometry;
pub mod import;
pub mod playbooks;
pub mod resource;
pub mod soundevents;
pub mod state;
pub mod tools;
pub mod vmap;
pub mod vmdl;
pub mod vpcf;
pub mod vpk;

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::{McpServer, ServerIdentity};
use cs2_mcp_common::tools::Registry;
use state::HammerState;

pub const NAME: &str = "hammer-mcp";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the agent reads at initialize: purpose, first call, intent gating, traps, definition of
/// done. Guarded by `tests/instructions.rs` (length cap, every tool-like word is registered).
pub const INSTRUCTIONS: &str = "Purpose: drives a live CS2 Hammer session (the map editor) and the workshop tools (ModelDoc models, the particle editor, soundevents) through an in-process bridge, and play-tests the map you compiled in the tools engine. It is not a general game client and not an offline file editor.\n\
\n\
First call: session_status. It shows liveness, the active document, unsaved edits, the addon and `next`. With no document open: an addon with no map -> new_map; an addon with maps -> open_map. A blocked or dead bridge means Hammer is not ready: tell the user, do not retry in a loop.\n\
\n\
Intent: if the user asks a question, answer it with read-only tools (annotations carry readOnlyHint) and change nothing. Call a mutating tool only when asked to change something.\n\
\n\
Traps:\n\
- Nothing auto-saves. save_map is the only writer; compile_map refuses UnsavedChanges until you save.\n\
- Compiles are jobs. compile_map {waitMs:30000} starts and waits; repeat compile_map {jobId, waitMs:30000} while done is false. Never busy-poll.\n\
- new_map's template already has meshes and spawns: remove them (delete_nodes, or new_map clear) or reuse them.\n\
- Document reads vs game reads: get_entity, find_entities, get_scene_info read the Hammer document (source live) or the saved file (source saved); entity_find, game_state, game_rules read the running game.\n\
- To see the map use look (framed views of an entity, point, box or the whole map; it moves no camera). Describe what the images show before judging.\n\
- run_map loads the COMPILED map (unsaved edits are not in it). The player starts dead on team 0: use run_map {team:\"ct\"} or \"t\" to spawn alive.\n\
- logic_auto OnMapSpawn fires at map load, not at round start. To act when play begins use a logic_eventlistener on round_freeze_end. Freeze time blocks movement (game_rules shows it).\n\
- BridgeBusy: another call holds the bridge. Wait for it; do not retry in a loop.\n\
- Errors: read next, useInstead, didYouMean, retrySafe (false = it may have run: check state before repeating). Switch on the error type, not the code. NotImplemented with data.missing: the bridge lacks that method; nothing was sent.\n\
- ModelDoc and particles: edit the .vmdl/.vpcf on disk (model_edit, particle_edit), then model_reload/particle_reload; compile with model_compile/particle_compile (each waits for its result).\n\
- Map scripts (TypeScript): cs_script writes, builds and compiles them; no shell needed.\n\
\n\
For a whole task (a blockout, the edit-compile-test loop, a script, particles, importing a model, a release) read the matching playbook first: get_playbook {name}, or the MCP prompt of the same name.\n\
\n\
Never step outside the tools: if a task needs something no tool does, tell the user and stop. Do not write scripts or reach the bridge another way.\n\
\n\
Done means: saved, compiled with 0 failed, loaded with run_map, the requested behaviour checked live with play_test or entity_find, and report_ready passes (or you explain every non-pass). Report with that evidence.";

/// Every hammer-mcp tool, registered and wired (`next` steps checked against the registry).
pub fn build_registry() -> Registry<HammerState> {
    let mut registry = Registry::new();
    tools::hammer::register(&mut registry);
    tools::modeldoc::register(&mut registry);
    tools::particles::register(&mut registry);
    tools::props::register(&mut registry);
    tools::triggers::register(&mut registry);
    tools::sounds::register(&mut registry);
    tools::jobs::register(&mut registry);
    tools::selection_live::register(&mut registry);
    tools::game_live::register(&mut registry);
    tools::addon::register(&mut registry);
    tools::mesh_live::register_align_texture(&mut registry);
    tools::layout::register(&mut registry);
    tools::new_map::register(&mut registry);
    tools::look::register(&mut registry);
    tools::report_ready::register(&mut registry);
    playbooks::register(&mut registry);
    tools::cs_script::register(&mut registry);
    tools::prefab::register(&mut registry);
    tools::player_model::register(&mut registry);
    tools::wire_next_steps(&mut registry);
    registry
}

/// Build the server (registry + state) from the common config; `serve_stdio` runs it.
pub fn build_server(cfg: &CommonConfig) -> anyhow::Result<McpServer<HammerState>> {
    let state = HammerState::from_config(cfg)?;
    let registry = build_registry();
    let identity = ServerIdentity {
        name: NAME,
        version: VERSION,
        instructions: INSTRUCTIONS.to_string(),
    };
    Ok(McpServer::new(
        registry,
        state,
        identity,
        cfg.tool_budget_ms,
        cfg.deadline_ms,
    )
    .with_prompts(playbooks::prompt_set()))
}

/// What the binary runs after the transport is done (stdin EOF, SIGTERM, SIGINT): end the
/// container-side process tree of every `compile_map` job this server still has running. A compile
/// that outlives the process is orphaned — nobody watches it, its `CS2MCP_COMPILE_JOB=<jobId>` tag
/// has no owner left, and a later server cannot tell it from a live compile (it reports the tag and
/// refuses to start a second one). Bounded; returns the report to log. See
/// [`tools::compile_map::shutdown_cleanup`].
pub async fn shutdown_cleanup(state: &HammerState) -> serde_json::Value {
    tools::compile_map::shutdown_cleanup(state).await
}
