//! client-mcp: stdio MCP server for the CS2 game client (tests, multiplayer, scripting, demo
//! video, AI playtest). Shared plumbing lives in `cs2-mcp-common`; this crate owns the client tool
//! set, console/test-script/input/video/playtest logic. See AGENT.md and workflows/.
#![allow(clippy::result_large_err)] // rich error payloads by design (see cs2-mcp-common)

pub mod console;
pub mod gametest;
pub mod input;
pub mod playtest;
pub mod script;
pub mod snapshot;
pub mod state;
pub mod testscript;
pub mod tools;
pub mod video;

use cs2_mcp_common::mcp::{McpServer, ServerIdentity};
use cs2_mcp_common::tools::Registry;
use state::{ClientConfig, ClientState};

pub const NAME: &str = "client-mcp";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the agent reads at initialize: purpose, first call, intent gating, traps, definition of
/// done. Guarded by `tests/instructions.rs` (length cap, every tool-like word is registered).
pub const INSTRUCTIONS: &str = "Purpose: drives a CS2 game client for play and tests: console, input, game state, screenshots, test scripts, servers, demos. It is not the map editor (that is the Hammer server).\n\
\n\
First call: session_status. It names the flavour and the available tool groups. WineTools (Windows build with the bridge) has input, console_exec, game_state, game_rules, entity_find, checkpoints and render_world. LinuxClient (native, headless) has no bridge, so only bridge-free tools (console_tail/console_wait, screenshot, observe, server_up/client_connect). Nothing running: launch_session first.\n\
\n\
Intent: if the user asks a question, answer it with read-only tools (annotations carry readOnlyHint) and change nothing. Launch, input and console tools only when asked to act.\n\
\n\
Traps:\n\
- console_exec never accepts quit, exit, disconnect, Test_ExitProcess or crash; end a session with stop_session.\n\
- Sessions are capped (GPU memory): TooManySessions and KindBusy name the container to reuse or stop.\n\
- Waits return pending:true with `next` when the budget ends: call again, do not loop on a short timeout. BridgeBusy: another call holds the bridge; wait for it.\n\
- Do not send console commands during a level load.\n\
- Judge a test on console.log lines, game_state values or screenshots.\n\
- Errors: read next, useInstead, didYouMean, retrySafe (false = it may have run: check state before repeating). Switch on the error type, not the code. NotImplemented with data.missing: nothing was sent.\n\
\n\
Never step outside the tools: if a task needs something no tool does, tell the user and stop.\n\
\n\
Done means: the session is the right flavour and map, the action ran, the result is read back (game_state, entity_find, console_tail or a screenshot) and reported as evidence, and a session you launched is stopped with stop_session.";

/// Every client-mcp tool, registered (aliases included).
pub fn build_registry() -> Registry<ClientState> {
    let mut registry = Registry::new();
    tools::client::register(&mut registry);
    registry
}

pub fn build_server(cfg: &ClientConfig) -> anyhow::Result<McpServer<ClientState>> {
    build_server_with_state(cfg).map(|(server, _)| server)
}

/// The server plus a handle to its state (clone); pass the handle to [`shutdown`] when serving ends.
pub fn build_server_with_state(
    cfg: &ClientConfig,
) -> anyhow::Result<(McpServer<ClientState>, ClientState)> {
    let state = ClientState::from_config(cfg)?;
    let handle = state.clone();
    let registry = build_registry();
    let identity = ServerIdentity {
        name: NAME,
        version: VERSION,
        instructions: INSTRUCTIONS.to_string(),
    };
    let server = McpServer::new(
        registry,
        state,
        identity,
        cfg.common.tool_budget_ms,
        cfg.common.deadline_ms,
    );
    Ok((server, handle))
}

/// Called after the transport closed: best-effort stop of the containers of runs still in progress
/// (see `testscript::run`). Finished runs and launched sessions are left to `stop_session`.
pub async fn shutdown(state: &ClientState) {
    let stopped = testscript::run::stop_running(&state.runs, &state.runtime).await;
    if !stopped.is_empty() {
        tracing::info!(?stopped, "stopped run containers at shutdown");
    }
}
