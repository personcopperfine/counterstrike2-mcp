//! client-mcp tool set: `client.rs` registers the tools; `session.rs` implements `session_status`
//! and `console_exec`; `runtime_tools.rs` holds launch/wait/stop_session; `bridge_tools.rs`
//! implements `game_tick`, `checkpoint`, `input_set`, `input_clear`; `console.rs` implements
//! `console_tail` and `console_wait`; `game_wait.rs` implements `game_wait` (predicate wait over `game.local`);
//! `game_actions.rs` implements `setpos` (console.exec `setpos_exact` verified by the `at` wait) and
//! `input_sequence` (input.set steps held for game ticks, input.clear at the end), `fire_weapon` (attack taps
//! verified by the clip1 ammo wait, sound evidence), `look_at` and `use_entity` (an input.set angle override
//! from `aim_angles`; use_entity taps use), `ent_fire` (console.exec, confirmed by the marker); `game_snapshot`
//! and `game_restore` (save/restore the local player through `snapshot.rs`; restore reuses setpos in process);
//! `game_round.rs` implements `round_start` (ensure or restart the round, verified by the roundLive/roundRestarted waits);
//! `gametest/` (not a tool module) implements `game_test` and `game_test_results` (see client.rs);
//! `vision.rs` implements `render_world`, `ocr` and the shared
//! capture path; `capture.rs` implements `screenshot` and `observe` (frames from the Vulkan layer);
//! `testscript.rs` implements `run_testscript`, `test_status` and `test_wait`; `video.rs` the demo tools,
//! `multiplayer.rs` the dedicated-server tools and `script.rs` `script_run`. No documented stubs remain.
pub mod bridge_tools;
pub mod capture;
pub mod client;
pub mod console;
pub mod game_actions;
pub mod game_round;
pub mod game_snapshot;
pub mod game_state;
pub mod game_wait;
pub mod multiplayer;
pub mod runtime_tools;
pub mod script;
pub mod session;
pub mod testscript;
pub mod video;
pub mod vision;
