//! client-mcp tool set (names are the contract). Every tool is implemented; `mod.rs` maps each one
//! to its module. No documented stubs remain.
use crate::state::ClientState;
use crate::tools::vision;
use cs2_mcp_common::tools::{Mutability::*, Registry, ToolSpec};
use serde_json::json;

pub fn register(r: &mut Registry<ClientState>) {
    r.register(
        ToolSpec::new("session_status", "Runtime + flavour + bridge + build + liveness + tool-group availability; never raises.", ReadOnly, &[]),
        super::session::session_status,
    );
    r.register(
        ToolSpec::new(
            "launch_session",
            "Start a CS2 container: linux-client (headless), wine-tools (host X, -tools, bridge) or wine-client (Windows client, no -tools). Not for: dedicated servers (server_up). map stages a harness and parks the player (wine-tools). Check mapVerified (false + mapNote: map not confirmed); reused:true = an identical running session. wine-client: bridge tools only once a map runs.",
            Mutating,
            &[],
        )
        .with_schema(super::runtime_tools::launch_schema()),
        super::runtime_tools::launch_session,
    );
    r.register(
        ToolSpec::new(
            "wait_session",
            "Wait for a launched session's readiness stage.",
            ReadOnly,
            &[],
        )
        .with_schema(super::runtime_tools::wait_schema()),
        super::runtime_tools::wait_session,
    );
    r.register(
        ToolSpec::new(
            "stop_session",
            "Stop and remove a session container (cs2mcp-* only). pending: true when the stop outlasts the budget (call again).",
            Mutating,
            &[],
        )
        .destructive(true)
        .with_schema(super::runtime_tools::stop_schema()),
        super::runtime_tools::stop_session,
    );
    r.register(
        ToolSpec::new(
            "console_tail",
            "Last N lines of a console.log, optional regex filter. File: --console-log, else session, else the one running cs2mcp-* session.",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "lines": {"type": "integer", "minimum": 1, "maximum": 2000, "default": 50},
                "grep": {"type": "string"},
                "session": {"type": "string", "description": "cs2mcp-* container; required when several run"}
            }
        })),
        super::console::console_tail,
    );
    r.register(
        ToolSpec::new(
            "console_wait",
            "Wait for a new console.log line matching a regex (lines after the call starts, unless fromStart).",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "regex": {"type": "string"},
                "session": {"type": "string"},
                "timeoutMs": {"type": "integer", "minimum": 0, "default": 10000},
                "fromStart": {"type": "boolean", "default": false, "description": "also search the existing file; not with since"},
                "since": {"type": "object", "description": "cursor from console_tail or an earlier observe (console.cursor); not with fromStart", "properties": {"inode": {"type": "integer"}, "offset": {"type": "integer"}, "path": {"type": "string"}}, "required": ["inode", "offset"]}
            },
            "required": ["regex"]
        })),
        super::console::console_wait,
    );
    r.register(
        ToolSpec::new(
            "console_exec",
            "Queue a console command through the bridge (-testscript or -tools sessions; not during a level load). Not for: cvar reads (cvar_get); dedicated servers (server_console_exec). Refuses quit/exit/disconnect/crash/maps. confirm: true waits until the engine processed it and returns warnings (its 'Unknown command:' lines: a typo shows up there). A timeout is not retry-safe.",
            Mutating,
            &["console.exec"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "session": {"type": "string"},
                "confirm": {"type": "boolean", "default": false},
                "confirmTimeoutMs": {"type": "integer", "minimum": 1, "default": 5000}
            },
            "required": ["command"]
        })),
        super::session::console_exec,
    );
    use super::bridge_tools as bt;
    r.register(
        ToolSpec::new(
            "game_tick",
            "Current server tick and input-hook frame count.",
            ReadOnly,
            &["game.tick"],
        ),
        bt::game_tick,
    );
    r.register(
        ToolSpec::new(
            "cvar_get",
            "Read console variables (found, value, type, default, flags). Not for setting: console_exec `<name> <value>`.",
            ReadOnly,
            &["cvar.get"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "names": {"type": "array", "minItems": 1, "maxItems": 64, "items": {"type": "string", "minLength": 1, "maxLength": 128}}
            },
            "required": ["names"],
            "additionalProperties": false
        })),
        bt::cvar_get,
    );
    r.register(
        ToolSpec::new("checkpoint", "Fire a test-script checkpoint. Checkpoints arriving during Test_Wait are dropped; use Test_WaitForCheckPoint barriers.", Mutating, &["test.checkpoint"]).with_schema(bt::checkpoint_schema()),
        bt::checkpoint,
    );
    r.register(
        ToolSpec::new("input_set", "Press/release/tap virtual buttons and set the view-angle override. Not for: teleporting (setpos); timed multi-step input (input_sequence). While an angles override is set, console setang is ignored.", Mutating, &["input.set"]).with_schema(bt::input_set_schema()),
        bt::input_set,
    );
    r.register(
        ToolSpec::new(
            "input_clear",
            "Release every virtual button and disable the angle override.",
            Mutating,
            &["input.clear"],
        )
        .with_schema(bt::input_clear_schema()),
        bt::input_clear,
    );
    use super::testscript as ts;
    r.register(
        ToolSpec::new(
            "run_testscript",
            "Stage a CTestScriptMgr suite (library name or inline script) and launch a wine-tools session with -testscript. Returns {runId, ...} at once; poll test_wait. Not for: in-game assertions (game_test). suite and inline are exclusive.",
            Mutating,
            &[],
        )
        .with_schema(ts::run_schema()),
        ts::run_testscript,
    );
    r.register(
        ToolSpec::new(
            "test_status",
            "State of a run_testscript run: running|passed|failed|error, checkpoint timeline, artifacts once finished.",
            ReadOnly,
            &[],
        )
        .with_schema(ts::status_schema()),
        ts::test_status,
    );
    r.register(
        ToolSpec::new(
            "test_wait",
            "Wait for a run to finish, then return test_status; pending: true when still running.",
            ReadOnly,
            &[],
        )
        .with_schema(ts::wait_schema()),
        ts::test_wait,
    );
    use super::video as dv;
    r.register(
        ToolSpec::new(
            "demo_record_start",
            "Start a SourceTV recording. tv_enable 1 must be set BEFORE the map loads; this does not reload the map.",
            Writer,
            &["console.exec"],
        )
        .with_schema(dv::record_start_schema()),
        dv::demo_record_start,
    );
    r.register(
        ToolSpec::new(
            "demo_record_stop",
            "Stop a SourceTV recording; returns the host .dem path and bytes.",
            Writer,
            &["console.exec"],
        )
        .with_schema(dv::record_stop_schema()),
        dv::demo_record_stop,
    );
    r.register(
        ToolSpec::new(
            "demo_to_video",
            "Turn a demo in csgo/ into a video with the engine encoder (a wine-tools run_testscript; poll test_wait).",
            Writer,
            &[],
        )
        .with_schema(dv::to_video_schema()),
        dv::demo_to_video,
    );
    use super::multiplayer as mp;
    r.register(
        ToolSpec::new(
            "server_up",
            "Launch a headless Linux dedicated server (RCON, host network) on map (default de_dust2) and port (default 27015; PortInUse when bound). Not for: clients (launch_session). Returns rconPassword (random if absent).",
            Mutating,
            &[],
        )
        .with_schema(mp::server_up_schema()),
        mp::server_up,
    );
    r.register(
        ToolSpec::new(
            "client_connect",
            "Send `connect <address>` to a client session. wine-tools only; on linux-client use launch_session {connect}.",
            Mutating,
            &[],
        )
        .with_schema(mp::client_connect_schema()),
        mp::client_connect,
    );
    r.register(
        ToolSpec::new(
            "server_console_exec",
            "Run one console command on a dedicated server over RCON. Not for: client commands (console_exec). Refuses quit/exit/disconnect/sv_shutdown (use stop_session).",
            Mutating,
            &[],
        )
        .with_schema(mp::server_console_exec_schema()),
        mp::server_console_exec,
    );
    r.register(
        ToolSpec::new(
            "script_run",
            "Run the scripts of a compiled addon map: launch or reuse the session, fire entry, match the expect regexes in order over the [cs_script] console lines. passed false is a result, not an error; passed null = budget cut (pending). Only kind cs_script, needs addon (source/path: NotImplementedYet). Passes when every expect matched in order and no script error line.",
            Mutating,
            &[],
        )
        .with_schema(super::script::script_run_schema()),
        super::script::script_run,
    );
    use crate::gametest;
    r.register(
        ToolSpec::new(
            "game_test",
            "Declarative in-game test. The session must be on spec.map (else launched); setup, steps, spec expect, teardown (always runs). Step {tool, args?, label?, expect?}; tool: round_start, setpos, game_snapshot/restore, input_sequence, fire_weapon, look_at, use_entity, ent_fire, console_exec/wait, game_wait, game_state, weapons, sounds_drain, demo_record_start/stop, checkpoint. Assertion {path (e.g. totals.distance), op: eq|ne|lt|le|gt|ge|approx|contains|matches, value (or {\"$ref\": \"label.path\"} into an earlier step), tolerance?, from?: result|game_state|weapons}. Stops at the first failing step unless continueOnFailure; a step result with pending: true or passed: false fails unless asserted on.",
            Mutating,
            &[],
        )
        .with_schema(gametest::game_test_schema()),
        gametest::game_test,
    );
    r.register(
        ToolSpec::new(
            "game_test_results",
            "Stored results of this process's game_test runs: all or one by name. format junit returns {xml}.",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "format": {"type": "string", "enum": ["json", "junit"], "default": "json"}
            }
        })),
        gametest::game_test_results,
    );
    r.register(vision::render_world_spec(), vision::render_world);
    super::game_state::register(r);
    super::game_wait::register(r);
    super::game_actions::register(r);
    super::game_snapshot::register(r);
    super::game_round::register(r);
    r.register(vision::ocr_spec(), vision::ocr);
    use super::capture as cap;
    r.register(cap::screenshot_spec(), cap::screenshot);
    // observe gates game.tick itself (skipped with a note when the bridge does not advertise it)
    r.register(cap::observe_spec(), cap::observe);
}

/// Tool groups reported by `session_status` with the bridge methods each needs.
pub const TOOL_GROUPS: &[(&str, &[&str], &[&str])] = &[
    // (group, tools, required bridge methods)
    ("console_log", &["console_tail", "console_wait"], &[]),
    ("console_exec", &["console_exec"], &["console.exec"]),
    (
        "test",
        &["checkpoint", "game_tick"],
        &["test.checkpoint", "game.tick"],
    ),
    ("cvar", &["cvar_get"], &["cvar.get"]),
    (
        "input",
        &["input_set", "input_clear"],
        &["input.set", "input.clear"],
    ),
    // screenshot and observe need no bridge: frames come from the capture layer (see session_status)
    ("capture", &["screenshot", "observe"], &[]),
    ("render_world", &["render_world"], &["world.render"]),
    (
        "demo_record",
        &["demo_record_start", "demo_record_stop"],
        &["console.exec"],
    ),
    (
        "multiplayer",
        &["server_up", "client_connect", "server_console_exec"],
        &[],
    ),
    (
        "game_state",
        &[
            "game_state",
            "weapons",
            "radar",
            "select_weapon",
            "game_wait",
        ],
        &[
            "game.local",
            "game.weapons",
            "game.radar",
            "game.selectWeapon",
        ],
    ),
    // m2.10: a bridge without game.rules / game.entities keeps the m2.9 game_state tools
    (
        "game_rules",
        &["game_rules", "entity_find"],
        &["game.rules", "game.entities"],
    ),
    ("game_actions", &["setpos"], &["console.exec", "game.local"]),
    (
        "game_round",
        &["round_start"],
        &["console.exec", "game.rules", "game.local"],
    ),
    (
        "input_sequence",
        &["input_sequence"],
        &["input.set", "input.clear", "game.local"],
    ),
    // separate groups, each with only the methods its tool uses: one missing method hides only its tool
    (
        "fire_weapon",
        &["fire_weapon"],
        &["input.set", "game.local"],
    ),
    ("ent_fire", &["ent_fire"], &["console.exec"]),
    (
        "look_at",
        &["look_at"],
        &["input.set", "game.local", "game.entities"],
    ),
    (
        "use_entity",
        &["use_entity"],
        &["input.set", "game.local", "game.entities"],
    ),
    (
        "snapshot",
        &["game_snapshot", "game_restore"],
        &[
            "console.exec",
            "game.local",
            "game.weapons",
            "game.selectWeapon",
        ],
    ),
    (
        "sounds",
        &["sounds_drain", "sounds_status"],
        &["sound.drain", "sound.status"],
    ),
    // no methods, like its registration: each step is capability-gated by its own tool
    ("game_test", &["game_test"], &[]),
    ("game_test_results", &["game_test_results"], &[]),
    (
        "session",
        &[
            "launch_session",
            "wait_session",
            "stop_session",
            "run_testscript",
            "test_status",
            "test_wait",
        ],
        &[],
    ),
];
