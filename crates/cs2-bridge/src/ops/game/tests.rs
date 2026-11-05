//! The game group's unit tests: the pure builders byte-for-byte against the C's `sb_add`
//! sequences, and the pure validators. The expected strings are transcribed from the cited C
//! lines (the brief's test 1 and the lane's extra tests); the handlers' wiring to game memory
//! is exercised by the gate tests in `tests/ops_game.rs`.

use super::builders;
use super::schema::{
    gs_layout_check, layout_decide, layout_entry, layout_mismatch_anchor, layout_mismatch_size, Gs,
};
use super::{
    cmdline_has_in, entities_keep, entities_params_validate, gs_is_weapon_name, jstr, probe_names_source,
    console_exec_allowed, op_console_exec_in, op_test_checkpoint_in,
};
use crate::entity_json::Sb;
use crate::ops::OpErr;

/// Builds a result object in the C's own buffer size and returns its bytes.
fn build(f: impl FnOnce(&mut Sb<'_>)) -> Vec<u8> {
    let mut buf = vec![0u8; 512];
    let mut sb = Sb::new(&mut buf);
    f(&mut sb);
    sb.as_cstr().to_vec()
}

fn err_of(r: Result<(), OpErr>) -> OpErr {
    r.expect_err("expected an error")
}

/// `game.tick` (`mcp_bridge.c:5050-5052`): `sb_add(out, "{\"tick\":"); sb_addi(out, tick);
/// sb_add(out, ",\"frames\":"); sb_addi(out, (int)g_inFrames); sb_add(out, "}");`
#[test]
fn tick_result_bytes() {
    let got = build(|out| builders::tick_result(out, 123, 45));
    assert_eq!(got, br#"{"tick":123,"frames":45}"#);
    let got = build(|out| builders::tick_result(out, 0, -1));
    assert_eq!(got, br#"{"tick":0,"frames":-1}"#);
}

/// `test.checkpoint` / `test.active` (`mcp_bridge.c:5096`).
#[test]
fn active_result_bytes() {
    let got = build(|out| builders::active_result(out, true));
    assert_eq!(got, br#"{"active":true}"#);
    let got = build(|out| builders::active_result(out, false));
    assert_eq!(got, br#"{"active":false}"#);
}

/// `console.exec` (`mcp_bridge.c:5078`).
#[test]
fn queued_result_bytes() {
    let got = build(builders::queued_result);
    assert_eq!(got, br#"{"queued":true}"#);
}

/// `sb_addf3` (`mcp_bridge.c:2648-2655`): three `sb_addf` floats in brackets.
#[test]
fn addf3_bytes() {
    let got = build(|out| builders::addf3(out, &[1.5, 2.25, 3.0]));
    assert_eq!(got, b"[1.500,2.250,3]");
    let got = build(|out| builders::addf3(out, &[0.0, -0.25, 100.0]));
    assert_eq!(got, b"[0,-0.250,100]");
}

/// `gs_weapon_json` (`mcp_bridge_game.inc:368-393`): the fields in the C's order -
/// entindex, handle, name, defIndex, clip1, clip2, reserve, reloading - with the C's
/// unresolved defaults (-1, INVALID_EHANDLE_INDEX = 4294967295, null name).
#[test]
fn weapon_json_bytes() {
    let got = build(|out| {
        builders::weapon_json(
            &builders::WeaponJson {
                entindex: 7,
                handle: 4660,
                name: Some(b"weapon_ak47".to_vec()),
                def_index: 7,
                clip1: 30,
                clip2: -1,
                reserve: 90,
                reloading: false,
            },
            out,
        )
    });
    assert_eq!(
        got,
        br#"{"entindex":7,"handle":4660,"name":"weapon_ak47","defIndex":7,"clip1":30,"clip2":-1,"reserve":90,"reloading":false}"#
    );
    // Every read failed: the C's defaults (`mcp_bridge_game.inc:369-372`).
    let got = build(|out| {
        builders::weapon_json(
            &builders::WeaponJson {
                entindex: -1,
                handle: 0xFFFFFFFF,
                name: None,
                def_index: -1,
                clip1: -1,
                clip2: -1,
                reserve: -1,
                reloading: false,
            },
            out,
        )
    });
    assert_eq!(
        got,
        br#"{"entindex":-1,"handle":4294967295,"name":null,"defIndex":-1,"clip1":-1,"clip2":-1,"reserve":-1,"reloading":false}"#
    );
}

/// `gs_player_json` without details (game.radar, `mcp_bridge_game.inc:434-445`): the eleven
/// members in the C's order.
#[test]
fn player_json_radar_bytes() {
    let got = build(|out| {
        builders::player_json(
            &builders::PlayerJson {
                controller_index: 1,
                pawn_index: 78,
                name: Some(b"player1".to_vec()),
                team: 3,
                alive: true,
                health: 100,
                is_local: true,
                origin: Some([1.5, 2.25, 3.0]),
                eye_angles: Some([0.0, 0.0, 90.0]),
                spotted: true,
                spotted_mask: 4,
                details: None,
            },
            out,
        )
    });
    assert_eq!(
        got,
        br#"{"controllerIndex":1,"pawnIndex":78,"name":"player1","team":3,"alive":true,"health":100,"isLocal":true,"origin":[1.500,2.250,3],"eyeAngles":[0,0,90],"spotted":true,"spottedByMask":4}"#
    );
    // No pawn: the C's zeros and nulls (`mcp_bridge_game.inc:410-445`).
    let got = build(|out| {
        builders::player_json(
            &builders::PlayerJson {
                controller_index: 2,
                pawn_index: -1,
                name: None,
                team: 0,
                alive: false,
                health: 0,
                is_local: false,
                origin: None,
                eye_angles: None,
                spotted: false,
                spotted_mask: 0,
                details: None,
            },
            out,
        )
    });
    assert_eq!(
        got,
        br#"{"controllerIndex":2,"pawnIndex":-1,"name":null,"team":0,"alive":false,"health":0,"isLocal":false,"origin":null,"eyeAngles":null,"spotted":false,"spottedByMask":0}"#
    );
}

/// `gs_player_json` with details (game.local, `mcp_bridge_game.inc:446-471`): the detail
/// members in the C's order, nulls where the reads failed.
#[test]
fn player_json_local_bytes() {
    let got = build(|out| {
        builders::player_json(
            &builders::PlayerJson {
                controller_index: 1,
                pawn_index: 78,
                name: Some(b"player1".to_vec()),
                team: 3,
                alive: true,
                health: 100,
                is_local: true,
                origin: Some([1.5, 2.25, 3.0]),
                eye_angles: Some([0.0, 0.0, 90.0]),
                spotted: true,
                spotted_mask: 4,
                details: Some(builders::PlayerDetails {
                    eye: Some([1.5, 2.25, 4.0]),
                    velocity: Some([0.0, 0.0, -15.0]),
                    life_state: 0,
                    flags: 256,
                    scoped: true,
                    armor: Some(100),
                    helmet: Some(true),
                    money: Some(16000),
                    ducked: None,
                    move_type: Some(8),
                    active_weapon: Some(builders::WeaponJson {
                        entindex: 7,
                        handle: 4660,
                        name: Some(b"weapon_ak47".to_vec()),
                        def_index: 7,
                        clip1: 30,
                        clip2: -1,
                        reserve: 90,
                        reloading: false,
                    }),
                }),
            },
            out,
        )
    });
    assert_eq!(
        got,
        br#"{"controllerIndex":1,"pawnIndex":78,"name":"player1","team":3,"alive":true,"health":100,"isLocal":true,"origin":[1.500,2.250,3],"eyeAngles":[0,0,90],"spotted":true,"spottedByMask":4,"eye":[1.500,2.250,4],"velocity":[0,0,-15],"lifeState":0,"flags":256,"scoped":true,"armor":100,"helmet":true,"money":16000,"ducked":null,"moveType":8,"activeWeapon":{"entindex":7,"handle":4660,"name":"weapon_ak47","defIndex":7,"clip1":30,"clip2":-1,"reserve":90,"reloading":false}}"#
    );
}

/// `gs_session_header` (`mcp_bridge_game.inc:475-484`): no closing brace - every caller
/// continues the object.
#[test]
fn session_header_bytes() {
    let got = build(|out| builders::session_header(out, 7, true, Some(b"de_nuke".as_slice())));
    assert_eq!(got, br#"{"tick":7,"inGame":true,"map":"de_nuke""#);
    // mcp_bridge_game.inc:482: the map is null when unreadable or empty.
    let got = build(|out| builders::session_header(out, 0, false, None));
    assert_eq!(got, br#"{"tick":0,"inGame":false,"map":null"#);
}

/// The planted bomb (`mcp_bridge_game.inc:599-606`).
#[test]
fn bomb_planted_bytes() {
    let got = build(|out| {
        builders::bomb_planted_json(out, 200, Some(&[100.0, 200.0, 300.5]), true, false, 1, 25.5)
    });
    assert_eq!(
        got,
        br#"{"state":"planted","entindex":200,"origin":[100,200,300.500],"ticking":true,"defused":false,"site":1,"blowTime":25.500}"#
    );
}

/// The carried / dropped bomb (`mcp_bridge_game.inc:607-617`).
#[test]
fn bomb_carried_bytes() {
    let got = build(|out| builders::bomb_carried_json(out, true, 21, 78, None));
    assert_eq!(got, br#"{"state":"carried","entindex":21,"ownerPawnIndex":78,"origin":null}"#);
    let got = build(|out| builders::bomb_carried_json(out, false, 21, -1, Some(&[1.0, 2.0, 3.0])));
    assert_eq!(got, br#"{"state":"dropped","entindex":21,"ownerPawnIndex":-1,"origin":[1,2,3]}"#);
}

/// A hostage (`mcp_bridge_game.inc:618-635`).
#[test]
fn hostage_bytes() {
    let got = build(|out| {
        builders::hostage_json(out, 30, Some(&[1.0, 2.0, 3.0]), 3, false, 100, -1)
    });
    assert_eq!(
        got,
        br#"{"entindex":30,"origin":[1,2,3],"state":3,"rescued":false,"health":100,"leaderPawnIndex":-1}"#
    );
}

/// One entity of game.entities (`mcp_bridge_game.inc:750-758`).
#[test]
fn entity_json_bytes() {
    let got = build(|out| {
        builders::entity_json(
            out,
            305,
            &builders::EntityJson {
                class: Some(b"prop_physics".as_slice()),
                name: None,
                origin: Some(&[1.0, 2.0, 3.0]),
                angles: Some(&[0.0, 0.0, 0.0]),
                health: None,
                toggle_state: None,
            },
        )
    });
    assert_eq!(
        got,
        br#"{"entindex":305,"class":"prop_physics","name":null,"origin":[1,2,3],"angles":[0,0,0],"health":null,"toggleState":null}"#
    );
}

/// `op_game_rules`' tail (`mcp_bridge_game.inc:686-701`): the thirteen gs_fld_* rows in the
/// C's order, then the schemaMissing list.
#[test]
fn rules_tail_bytes() {
    let got = build(|out| {
        builders::rules_tail(
            out,
            &builders::RulesJson {
                proxy_index: 300,
                warmup: Some(true),
                freeze: Some(false),
                team_intro: None,
                game_restart: None,
                has_match_started: Some(true),
                game_phase: Some(6),
                rounds_played: Some(11),
                round_start_time: Some(25.5),
                restart_round_time: None,
                round_time_sec: Some(115),
                round_win_status: Some(0),
                round_end_reason: Some(0),
                bomb_planted: None,
                schema_missing: br#""m_bTeamIntroPeriod","m_bGameRestart","m_flRestartRoundTime","m_bBombPlanted""#,
            },
        )
    });
    assert_eq!(
        got,
        br#","proxyIndex":300,"warmup":true,"freeze":false,"teamIntro":null,"gameRestart":null,"hasMatchStarted":true,"gamePhase":6,"roundsPlayed":11,"roundStartTime":25.500,"restartRoundTime":null,"roundTimeSec":115,"roundWinStatus":0,"roundEndReason":0,"bombPlanted":null,"schemaMissing":["m_bTeamIntroPeriod","m_bGameRestart","m_flRestartRoundTime","m_bBombPlanted"]}"#
    );
}

/// `gs_miss_add` (`mcp_bridge_game.inc:337-341`): comma-joined.
#[test]
fn miss_add_bytes() {
    let got = build(|out| {
        builders::miss_add(out, b"m_a");
        builders::miss_add(out, b"m_b");
    });
    assert_eq!(got, br#""m_a","m_b""#);
}

/// A schemaProbe row (`mcp_bridge_game.inc:1236-1239`): isGlobal is -1 when the scope is not
/// the pinned class, else the slot's bool.
#[test]
fn probe_result_bytes() {
    let got = build(|out| {
        builders::probe_result(out, b"client.dll", true, Some(b"client".as_slice()), 0)
    });
    assert_eq!(got, br#"{"name":"client.dll","found":true,"scopeName":"client","isGlobal":0}"#);
    let got = build(|out| {
        builders::probe_result(out, b"Client.dll", false, None, -1)
    });
    assert_eq!(got, br#"{"name":"Client.dll","found":false,"scopeName":null,"isGlobal":-1}"#);
}

/// `game.selectWeapon`'s two results (`mcp_bridge_game.inc:876-878`, `:887`).
#[test]
fn select_result_bytes() {
    let got = build(|out| builders::select_command_result(out, b"slot3"));
    assert_eq!(got, br#"{"queued":true,"via":"command","command":"slot3"}"#);
    let got = build(|out| {
        builders::select_weapon_result(
            out,
            &builders::WeaponJson {
                entindex: 7,
                handle: 4660,
                name: Some(b"weapon_ak47".to_vec()),
                def_index: 7,
                clip1: 30,
                clip2: -1,
                reserve: 90,
                reloading: false,
            },
        )
    });
    assert_eq!(
        got,
        br#"{"queued":true,"via":"MakeWeaponSelection","weapon":{"entindex":7,"handle":4660,"name":"weapon_ak47","defIndex":7,"clip1":30,"clip2":-1,"reserve":90,"reloading":false}}"#
    );
}

/// The F2 mismatch messages (`mcp_bridge_game.inc:168-179`), both forms.
#[test]
fn layout_mismatch_messages() {
    let got = build(|out| layout_mismatch_size(out, 104, 112));
    assert_eq!(
        got,
        b"CEntityIdentity layout mismatch: schema 104 vs header 112 size (update s2sdk upstream)"
    );
    let got = build(|out| layout_mismatch_anchor(out, b"m_name", 28, 24));
    assert_eq!(
        got,
        b"CEntityIdentity layout mismatch: schema m_name 28 vs header 24 offset (update s2sdk upstream)"
    );
}

/// The `-testscript` matcher over a command line (the C's `cmdline_has`,
/// `mcp_bridge.c:4988-4996`): whole tokens only, terminated by NUL / space / quote /
/// end-of-buffer.
#[test]
fn cmdline_matcher() {
    assert!(cmdline_has_in(b"game -testscript foo", b"-testscript"));
    assert!(cmdline_has_in(b"-testscript", b"-testscript"));
    // NUL-separated argv (/proc/self/cmdline): the scan continues past NULs.
    assert!(cmdline_has_in(b"game\0-testscript\0-foo", b"-testscript"));
    assert!(cmdline_has_in(b"\"-testscript\" -x", b"-testscript"));
    assert!(cmdline_has_in(b"a -testscript", b"-testscript"));
    // The C matches at every offset: a flag embedded after one letter, ended by the NUL,
    // still matches (mcp_bridge.c:4991-4994); a longer token does not.
    assert!(cmdline_has_in(b"x-testscript", b"-testscript"));
    assert!(!cmdline_has_in(b"-testscriptextra", b"-testscript"));
    assert!(!cmdline_has_in(b"-testscrip", b"-testscript"));
    assert!(!cmdline_has_in(b"game", b"-testscript"));
    assert!(!cmdline_has_in(b"", b"-testscript"));
    assert!(!cmdline_has_in(b"-testscript", b""));
}

/// `jstr` (`mcp_bridge.c:3164-3178`): quotes, the three escapes, the cap, the failures.
/// The caller (schemaProbe's loop, `mcp_bridge_game.inc:1222-1226`) skips spaces/commas,
/// checks the quote, then calls `jstr` at it - the test mimics that.
#[test]
fn jstr_parses_array_elements() {
    let s = b"[\"a\\\"b\", \"c\"]";
    let mut out = [0u8; 16];
    // Skip '[', then the caller's space/comma skip lands on the first quote.
    let p = jstr(s, 1, &mut out).expect("the first string parses");
    assert_eq!(&out[..4], b"a\"b\0");
    // The caller's skip: ' ' and ',' (mcp_bridge_game.inc:1222), then jstr at the quote.
    let mut q = p;
    while matches!(s.get(q), Some(b' ' | b',')) {
        q += 1;
    }
    let p2 = jstr(s, q, &mut out).expect("the second string parses");
    assert_eq!(&out[..2], b"c\0");
    // The C's jstr stops after the closing quote; the ']' is not consumed.
    assert_eq!(s[p2], b']');
    // No escapes beyond \" \\ \/ (mcp_bridge.c:3171).
    let mut out2 = [0u8; 16];
    assert!(jstr(b"\"a\\nb\"", 0, &mut out2).is_none());
    // Over-long: the C returns 0 when the copy would not fit.
    assert!(jstr(b"\"0123456789\"", 0, &mut [0u8; 8]).is_none());
    assert!(jstr(b"  42", 0, &mut [0u8; 8]).is_none());
}

/// `gs_is_weapon_name` (`mcp_bridge_game.inc:783-789`).
#[test]
fn weapon_name_shape() {
    assert!(gs_is_weapon_name(b"weapon_ak47"));
    assert!(gs_is_weapon_name(b"weapon_knife_t"));
    assert!(gs_is_weapon_name(b"weapon_elite"));
    assert!(!gs_is_weapon_name(b"weapon_")); // s[7] == 0
    assert!(!gs_is_weapon_name(b"weapons_ak47"));
    assert!(!gs_is_weapon_name(b"Weapon_ak47"));
    assert!(!gs_is_weapon_name(b"weapon_ak 47"));
    assert!(!gs_is_weapon_name(b"knife"));
}

/// `op_game_entities`' param validation (`mcp_bridge_game.inc:723-729`): the max bound, then
/// the single-index bound spelled from the const.
#[test]
fn entities_param_bounds() {
    assert!(entities_params_validate(64, false, 0).is_ok());
    assert!(entities_params_validate(1, false, 0).is_ok());
    assert!(entities_params_validate(256, false, 0).is_ok());
    assert_eq!(
        err_of(entities_params_validate(0, false, 0)),
        OpErr::new(-32602, "InvalidParams", "max must be 1..256")
    );
    assert_eq!(
        err_of(entities_params_validate(257, false, 0)),
        OpErr::new(-32602, "InvalidParams", "max must be 1..256")
    );
    // 16383 == MAX_EDICTS - 1, the generated const's bound.
    assert!(entities_params_validate(64, true, 16383).is_ok());
    let err = err_of(entities_params_validate(64, true, 16384));
    assert_eq!(err.code, -32602);
    assert_eq!(err.name, "InvalidParams");
    assert_eq!(err.data, r#"{"reason":"entindex must be 1..16383"}"#);
    assert_eq!(
        err_of(entities_params_validate(64, true, 0)),
        OpErr::new(-32602, "InvalidParams", "entindex must be 1..16383")
    );
}

/// `console.exec`'s body without `-testscript` and with malformed params
/// (`mcp_bridge.c:5062-5071`): the -testscript check precedes the param validation.
#[test]
fn console_exec_param_errors() {
    let mut buf = vec![0u8; 512];
    let mut sb = Sb::new(&mut buf);
    assert_eq!(
        err_of(op_console_exec_in(false, Some(br#"{"command":"echo hi"}"#), &mut sb)),
        OpErr::new(
            -32011,
            "NotImplemented",
            "console.exec is only enabled in -testscript or -tools sessions"
        )
    );
    // In a -testscript session: the command string is required and non-empty.
    assert_eq!(
        err_of(op_console_exec_in(true, None, &mut sb)),
        OpErr::new(-32602, "InvalidParams", "command (string) required")
    );
    assert_eq!(
        err_of(op_console_exec_in(true, Some(b"{}"), &mut sb)),
        OpErr::new(-32602, "InvalidParams", "command (string) required")
    );
    assert_eq!(
        err_of(op_console_exec_in(true, Some(br#"{"command":""}"#), &mut sb)),
        OpErr::new(-32602, "InvalidParams", "command (string) required")
    );
}

/// The `console.exec` session gate: refused on a plain command line, allowed with `-tools` or
/// `-testscript` (either alone), and a longer flag that merely starts with one does not count.
#[test]
fn console_exec_gate_accepts_tools_or_testscript() {
    let console_exec_allowed_in = |cmdline: &[u8]| {
        console_exec_allowed(|flag| cmdline_has_in(cmdline, flag.as_bytes()))
    };
    assert!(!console_exec_allowed_in(b"cs2.exe -game csgo\0"));
    assert!(!console_exec_allowed_in(b"cs2.exe -toolsfoo -testscripts"));
    assert!(!console_exec_allowed_in(b""));
    assert!(console_exec_allowed_in(b"cs2.exe -tools -condebug"));
    assert!(console_exec_allowed_in(b"cs2.exe -testscript x.lua"));
    assert!(console_exec_allowed_in(b"cs2.exe -tools -testscript"));
    assert!(console_exec_allowed_in(b"cs2.exe\0-tools"));
    // the gate's verdict feeds the body: refused, then (allowed) on to the param check
    let mut buf = vec![0u8; 512];
    let mut sb = Sb::new(&mut buf);
    let plain = console_exec_allowed_in(b"cs2.exe -game csgo");
    assert_eq!(
        err_of(op_console_exec_in(plain, Some(br#"{"command":"echo hi"}"#), &mut sb)).name,
        "NotImplemented"
    );
    let tools = console_exec_allowed_in(b"cs2.exe -tools");
    assert_eq!(
        err_of(op_console_exec_in(tools, Some(b"{}"), &mut sb)).name,
        "InvalidParams"
    );
}

/// `test.checkpoint`'s validation + tail (`mcp_bridge.c:5088-5095`), with the checkpoint
/// call recorded: the active flag is read before the name is validated, the name must be a
/// non-empty string, and the reply reports the pre-call `active`.
#[test]
fn checkpoint_validation_and_reply() {
    let mut buf = vec![0u8; 512];
    let mut sb = Sb::new(&mut buf);
    assert_eq!(
        err_of(op_test_checkpoint_in(true, None, |_| panic!("not called"), &mut sb)),
        OpErr::new(-32602, "InvalidParams", "name (string) required")
    );
    assert_eq!(
        err_of(op_test_checkpoint_in(true, Some(b"{}"), |_| panic!("not called"), &mut sb)),
        OpErr::new(-32602, "InvalidParams", "name (string) required")
    );
    assert_eq!(
        err_of(op_test_checkpoint_in(true, Some(br#"{"name":""}"#), |_| panic!("not called"), &mut sb)),
        OpErr::new(-32602, "InvalidParams", "name (string) required")
    );
    let mut passed = Vec::new();
    op_test_checkpoint_in(false, Some(br#"{"name":"cp1"}"#), |n| passed = n.to_vec(), &mut sb)
        .expect("a valid checkpoint runs");
    assert_eq!(passed, b"cp1");
    assert_eq!(sb.as_cstr(), br#"{"active":false}"#);
}

/// The probe's names source (`mcp_bridge_game.inc:1217-1218`): a `names` array hands the
/// scan its elements; anything else (absent, not an array, params ending at `"names":`) is
/// the C's default list - including the trailing-`"names":` case, where `json_find` answers
/// the value's start == len and the C reads a NUL there (`:1218`), so the byte is `get`, not
/// an index.
#[test]
fn probe_names_source_selection() {
    let names = br#"{"names":["client.dll","engine2.dll"]}"#;
    let (src, start) = probe_names_source(Some(names)).expect("an array hands the scan over");
    assert_eq!(src, &names[..]);
    assert_eq!(start, 10); // after `{"names":[`
    // Not an array -> the defaults.
    assert!(probe_names_source(Some(br#"{"names":"client.dll"}"#)).is_none());
    assert!(probe_names_source(Some(br#"{"names" null}"#)).is_none());
    // Absent -> the defaults.
    assert!(probe_names_source(Some(br#"{"max":3}"#)).is_none());
    assert!(probe_names_source(None).is_none());
    // Params ending exactly at `"names":` (no value byte at all): no panic, the defaults.
    assert!(probe_names_source(Some(br#"{"names":"#)).is_none());
    // Same with the colon's trailing spaces consumed by json_find.
    assert!(probe_names_source(Some(br#"{"names":   "#)).is_none());
}

/// `gs_layout_check`'s sticky entry (`mcp_bridge_game.inc:138-139`): a stored mismatch
/// re-answers its own message and refuses; a checked state (1/3) passes; 0 runs the reads.
#[test]
fn layout_entry_sticky() {
    let mut e = OpErr::bare(0, "");
    assert_eq!(layout_entry(0, b"never seen", &mut e), None, "0 runs the reads");
    assert_eq!(layout_entry(1, b"", &mut e), Some(true), "checked-ok passes");
    assert_eq!(layout_entry(3, b"", &mut e), Some(true), "no-evidence passes");
    assert_eq!(layout_entry(2, b"stored mismatch", &mut e), Some(false));
    assert_eq!(
        e,
        OpErr::new(-32023, "SchemaUnavailable", "stored mismatch"),
        "the sticky refusal names the stored message"
    );
}

/// `gs_layout_check`'s decision (`mcp_bridge_game.inc:149-184`), pure over the schema's
/// reads: the ok states, the no-evidence state, the size/anchor mismatch messages and their
/// precedence, and the last-mismatching-anchor rule.
#[test]
fn layout_decision() {
    // The header's real anchor offsets (the generated struct's own, the C's `gen` column,
    // `mcp_bridge_game.inc:143-148`; the F2 banner, `:124`, pins them to 20, 24, 32, 48).
    let gen: [i32; 4] = [
        core::mem::offset_of!(s2sdk_sys::generated::CEntityIdentity, m_nameStringTableIndex) as i32,
        core::mem::offset_of!(s2sdk_sys::generated::CEntityIdentity, m_name) as i32,
        core::mem::offset_of!(s2sdk_sys::generated::CEntityIdentity, m_designerName) as i32,
        core::mem::offset_of!(s2sdk_sys::generated::CEntityIdentity, m_flags) as i32,
    ];
    assert_eq!(gen, [20, 24, 32, 48], "the C's anchor offsets, generated");
    let header_size = core::mem::size_of::<s2sdk_sys::generated::CEntityIdentity>() as i64;
    assert_eq!(header_size, 112, "the C's S2_SIZEOF_CEntityIdentity, generated");
    let anchors = |schema: [i32; 4]| -> super::schema::LayoutAnchors {
        [
            ("m_nameStringTableIndex", gen[0], schema[0]),
            ("m_name", gen[1], schema[1]),
            ("m_designerName", gen[2], schema[2]),
            ("m_flags", gen[3], schema[3]),
        ]
    };
    // An unregistered field keeps the header offset - never a mismatch.
    let none = [gen[0], gen[1], gen[2], gen[3]];
    // Everything agrees -> state 1, with the size as the evidence...
    assert_eq!(layout_decide(true, header_size as i32, &anchors(none), 0, header_size), Ok(1));
    // ...or without a size, with resolved-and-equal anchors.
    assert_eq!(layout_decide(false, 0, &anchors(none), 2, header_size), Ok(1));
    // Nothing comparable (no size, nothing resolved) -> state 3, no evidence either way.
    assert_eq!(layout_decide(false, 0, &anchors(none), 0, header_size), Ok(3));
    // Size mismatch: the size form, and it wins over an anchor mismatch (`:168`).
    assert_eq!(
        layout_decide(true, 104, &anchors(none), 0, header_size),
        Err("CEntityIdentity layout mismatch: schema 104 vs header 112 size (update s2sdk upstream)".to_string())
    );
    // Anchor mismatch, size unresolvable: the anchor form, last mismatching anchor
    // (`:154`, `:174-179`).
    let two_bad = [gen[0], 28, gen[2], 50];
    assert_eq!(
        layout_decide(false, 0, &anchors(two_bad), 2, header_size),
        Err("CEntityIdentity layout mismatch: schema m_flags 50 vs header 48 offset (update s2sdk upstream)".to_string())
    );
    // Size mismatch wins over the anchor mismatch.
    assert_eq!(
        layout_decide(true, 104, &anchors(two_bad), 2, header_size),
        Err("CEntityIdentity layout mismatch: schema 104 vs header 112 size (update s2sdk upstream)".to_string())
    );
}

/// `gs_layout_check` end to end on the host (`mcp_bridge_game.inc:137-185`): with no game,
/// the schema class cannot resolve, so the first run stores state 3 (no evidence either way)
/// and passes; a second run is the sticky entry's pass; and a fabricated mismatch is sticky
/// - the check refuses with the stored message without touching the schema again.
#[test]
fn layout_check_states_and_sticky() {
    let mut gs = Gs::new();
    let mut e = OpErr::bare(0, "");
    // First run: nothing resolves (no scope on the host) -> state 3, pass.
    assert!(gs_layout_check(&mut gs, &mut e), "no evidence either way is a pass");
    // Second run: already checked (3), passes without re-running anything.
    assert!(gs_layout_check(&mut gs, &mut e), "a checked state passes");
    // A mismatch - as the check would store it on the game - is sticky: every later
    // entity-resolving op fails closed with the stored message.
    let msg = "CEntityIdentity layout mismatch: schema 104 vs header 112 size (update s2sdk upstream)";
    let mut gs = Gs::new();
    gs.set_layout_mismatch(msg.as_bytes());
    let mut e = OpErr::bare(0, "");
    assert!(!gs_layout_check(&mut gs, &mut e), "the sticky mismatch refuses");
    assert_eq!(e, OpErr::new(-32023, "SchemaUnavailable", msg));
    // ...and keeps refusing with the same bytes (the sticky entry, `:138`).
    let mut e2 = OpErr::bare(0, "");
    assert!(!gs_layout_check(&mut gs, &mut e2), "still refusing");
    assert_eq!(e2, e);
}

/// `game.entities`' scratch-commit boundary (`mcp_bridge_game.inc:759`): a real entity's
/// bytes (leading comma included) commit when the 64-byte trailer reserve stays free, and
/// roll back to `truncated` at the byte the reserve would cross - both directions of the
/// exact boundary.
#[test]
fn entities_scratch_commit_boundary() {
    // One real entity, with its leading comma (the C writes it before the entity, `:747`).
    let mut scratch_buf = vec![0u8; super::GAME_BUF];
    let mut scratch = Sb::new(&mut scratch_buf);
    scratch.add(b",");
    builders::entity_json(
        &mut scratch,
        305,
        &builders::EntityJson {
            class: Some(b"prop_physics".as_slice()),
            name: None,
            origin: Some(&[1.0, 2.0, 3.0]),
            angles: Some(&[0.0, 0.0, 0.0]),
            health: None,
            toggle_state: None,
        },
    );
    let ent = scratch.as_cstr().len();
    assert!(ent + super::GS_TRAILER_RESERVE < super::GAME_BUF, "one entity fits an empty out");

    let keep_at = |out_len: usize| -> bool {
        let mut ob = vec![0u8; super::GAME_BUF];
        let mut out = Sb::new(&mut ob);
        out.add(&vec![b'x'; out_len]);
        entities_keep(ent, scratch.ovf(), out.as_cstr().len())
    };
    // The boundary: exactly 64 bytes free behind the entity -> keep (`out->len + 64 > cap` is
    // false, `:759`).
    assert!(keep_at(super::GAME_BUF - super::GS_TRAILER_RESERVE - ent), "exactly at the reserve: keep");
    // One byte more of output: the reserve is crossed -> roll back, truncated.
    assert!(
        !keep_at(super::GAME_BUF - super::GS_TRAILER_RESERVE - ent + 1),
        "past the reserve: rollback"
    );
    // An empty out always keeps a normal entity.
    assert!(keep_at(0));
}
