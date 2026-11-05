//! The M4 GAME lane's gate tests (the brief's negative test, mandatory): with the group's
//! modules unverified, `wiring::BridgeState::exec_request` answers the C's `-32017
//! PrerequisiteFailed` naming the first unmet module, and a body is never called - the reply
//! is byte-for-byte the gate builder's (`data.module` / `check` / `state`), not a body's
//! (`data.reason`).
//!
//! Also here: every name in `ops::game::HANDLERS` is a row of the frozen C table and
//! implemented there (the brief's test 4, at the integration level), and the malformed-params
//! paths of the methods whose validation precedes any game access (the brief's test 2),
//! driven through the real handlers with the gate passed.

use std::sync::Arc;
use std::time::Instant;

use cs2_bridge::control;
use cs2_bridge::dispatch;
use cs2_bridge::generated::pins;
use cs2_bridge::modules::ModuleViews;
use cs2_bridge::ops;
use cs2_bridge::queue::Request;
use cs2_bridge::registry::{self, ModState};
use cs2_bridge::wiring::BridgeState;

/// A module-state source a test constructs: every module in one state, revision fixed (the
/// integration test's `FakeModules` shape).
struct FakeModules {
    state: ModState,
    revision: u64,
}

impl ModuleViews for FakeModules {
    fn state(&self, _module: u8) -> ModState {
        self.state
    }

    fn is_verified(&self, _module: u8) -> bool {
        self.state == ModState::Verified
    }

    fn revision(&self) -> u64 {
        self.revision
    }
}

/// A request for the handler under test.
fn request(method: &str, params: Option<&str>) -> Request {
    Request {
        id: "7".to_string(),
        method: method.to_string(),
        params: params.map(str::to_string),
        received: Instant::now(),
        deadline: None,
        gen: 0,
    }
}

fn handler(method: &str) -> fn(&Request) -> String {
    let (_, h) = ops::game::HANDLERS
        .iter()
        .find(|(name, _)| *name == method)
        .unwrap_or_else(|| panic!("{method} is not in ops::game::HANDLERS"));
    *h
}

/// Every name in `ops::game::HANDLERS` is a row of the frozen C table, implemented there, and
/// the registration made `dispatch::is_ported` serve it (the brief's test 4).
#[test]
fn handlers_are_table_rows_and_are_ported() {
    for (name, _) in ops::game::HANDLERS {
        let row = registry::find(name)
            .unwrap_or_else(|| panic!("HANDLERS lists `{name}`, absent from the method table"));
        assert!(row.implemented, "HANDLERS lists `{name}`, a blocked row");
        assert!(dispatch::is_ported(name), "`{name}` is not served");
    }
    // Every name in the table is served now; game.saveRestore answers from the hooks lane
    // (ops::game::save_restore, the ISource2ServerConfig vptr swap).
    assert!(ops::game::HANDLERS.iter().any(|(n, _)| *n == "game.saveRestore"));
}

/// The brief's negative test: with every module unverified, `exec_request` answers the C's
/// `-32017` for each group method, naming the first unmet need in the C's `bits[]` order
/// (`exec_request:5678-5697`), and no body runs.
#[test]
fn unverified_modules_answer_prerequisite_failed_and_bodies_never_run() {
    let state = BridgeState::new(
        Arc::new(FakeModules { state: ModState::NotLoaded, revision: 0 }),
        Box::new(dispatch::is_ported),
    );
    // The gate module per method: the first unmet need bit in GATE_BITS order.
    for (method, module, pin) in [
        ("test.checkpoint", "tier0.dll", pins::PIN_TIER0),
        ("test.active", "tier0.dll", pins::PIN_TIER0),
        ("console.exec", "engine2.dll", pins::PIN_ENGINE2),
        ("game.tick", "engine2.dll", pins::PIN_ENGINE2),
        // client|engine|schema: engine2 precedes client in the C's bits[] order.
        ("game.local", "engine2.dll", pins::PIN_ENGINE2),
        ("game.weapons", "engine2.dll", pins::PIN_ENGINE2),
        ("game.radar", "engine2.dll", pins::PIN_ENGINE2),
        ("game.rules", "engine2.dll", pins::PIN_ENGINE2),
        ("game.entities", "engine2.dll", pins::PIN_ENGINE2),
        ("game.selectWeapon", "engine2.dll", pins::PIN_ENGINE2),
        ("game.schemaProbe", "schemasystem.dll", pins::PIN_SCHEMA),
    ] {
        let got = state.exec_request(&request(method, Some("{}")));
        let want = control::prerequisite_failed(Some("7"), module, ModState::NotLoaded);
        assert_eq!(got, want, "{method}: the gate's answer, no body ran");
        // The gate's data shape (module/check/state) is not a body's (reason) - a body's
        // answer would differ here, so the exact bytes above prove which answered.
        assert!(got.contains(r#""check":"identity""#), "{method}: {got}");
        let _ = pin;
    }
}

/// The methods whose malformed-params paths precede any game access in the C
/// (`mcp_bridge_game.inc:723-729` for game.entities, `mcp_bridge_game.inc:840-860` for
/// game.selectWeapon): with the gate passed, the real handler answers the C's `-32602` before
/// touching any module or game memory.
#[test]
fn malformed_params_answer_before_any_game_access() {
    let state = BridgeState::new(
        Arc::new(FakeModules { state: ModState::Verified, revision: 0 }),
        Box::new(dispatch::is_ported),
    );
    // game.entities (mcp_bridge_game.inc:723-729).
    for (params, reason) in [
        (r#"{"max":0}"#, "max must be 1..256"),
        (r#"{"max":257}"#, "max must be 1..256"),
        (r#"{"max":-5}"#, "max must be 1..256"),
        (r#"{"max":64,"entindex":0}"#, "entindex must be 1..16383"),
        (r#"{"max":64,"entindex":16384}"#, "entindex must be 1..16383"),
    ] {
        let got = state.exec_request(&request("game.entities", Some(params)));
        let want = control::error_ext(
            Some("7"),
            -32602,
            "InvalidParams",
            &format!(r#"{{"reason":"{reason}"}}"#),
        );
        assert_eq!(got, want, "game.entities {params}");
        // The handler, called directly with the same request, answers the same bytes: the
        // handler is the body the dispatch serves (the brief's test 2 route).
        assert_eq!(handler("game.entities")(&request("game.entities", Some(params))), want);
    }
    // game.selectWeapon (mcp_bridge_game.inc:840-860).
    for (params, reason) in [
        (r#"{"slot":14}"#, "slot must be 0..13"),
        (r#"{"slot":-1}"#, "slot must be 0..13"),
        (
            r#"{"command":"invnextx"}"#,
            "command must be invnext|invprev|invnextselect|invprevselect|lastinv",
        ),
        (r#"{"weapon":"ak47"}"#, "weapon must look like weapon_ak47"),
        (r#"{"weapon":"weapon_"}"#, "weapon must look like weapon_ak47"),
        (r#"{"defIndex":65536}"#, "defIndex must be 0..65535"),
        (r#"{"handle":-1}"#, "handle must be a u32 entity handle"),
        (r#"{"handle":4294967296}"#, "handle must be a u32 entity handle"),
        (r#"{"other":1}"#, "one of slot, command, weapon, defIndex or handle required"),
    ] {
        let got = state.exec_request(&request("game.selectWeapon", Some(params)));
        let want = control::error_ext(
            Some("7"),
            -32602,
            "InvalidParams",
            &format!(r#"{{"reason":"{reason}"}}"#),
        );
        assert_eq!(got, want, "game.selectWeapon {params}");
    }
    // No params at all is the same "one of ..." refusal (the C's final else, `:871-873`).
    let got = state.exec_request(&request("game.selectWeapon", None));
    let want = control::error_ext(
        Some("7"),
        -32602,
        "InvalidParams",
        r#"{"reason":"one of slot, command, weapon, defIndex or handle required"}"#,
    );
    assert_eq!(got, want);
}

/// The body-level identity checks on the host (gate passed, no game): `mod_iface` answers
/// null, so the C's `obj_is` refusals fire with the C's exact messages
/// (`mcp_bridge.c:5072-5076`, `:5082-5086`, `mcp_bridge_game.inc:303-310`, `:1211-1212`).
#[test]
fn bodies_refuse_unpinned_interfaces_with_the_c_messages() {
    let state = BridgeState::new(
        Arc::new(FakeModules { state: ModState::Verified, revision: 0 }),
        Box::new(dispatch::is_ported),
    );
    for (method, message) in [
        ("test.checkpoint", "TestScriptMgr001 is not the pinned CTestScriptMgr"),
        ("test.active", "TestScriptMgr001 is not the pinned CTestScriptMgr"),
        ("game.tick", "Source2EngineToClient001 is not the pinned CEngineClient"),
        ("game.local", "Source2EngineToClient001 is not the pinned CEngineClient"),
        ("game.weapons", "Source2EngineToClient001 is not the pinned CEngineClient"),
        ("game.radar", "Source2EngineToClient001 is not the pinned CEngineClient"),
        ("game.rules", "Source2EngineToClient001 is not the pinned CEngineClient"),
        ("game.entities", "Source2EngineToClient001 is not the pinned CEngineClient"),
        ("game.schemaProbe", "SchemaSystem_001 is not the pinned CSchemaSystem"),
    ] {
        let got = state.exec_request(&request(method, Some("{}")));
        let want = control::error_ext(
            Some("7"),
            -32017,
            "PrerequisiteFailed",
            &format!(r#"{{"reason":"{message}"}}"#),
        );
        assert_eq!(got, want, "{method}: the body's identity refusal");
    }
    // game.selectWeapon's command path answers its input-service refusal; the weapon path
    // needs client.dll and schemasystem.dll pinned (`mcp_bridge_game.inc:862-865`).
    let got = state.exec_request(&request("game.selectWeapon", Some(r#"{"slot":3}"#)));
    let want = control::error_ext(
        Some("7"),
        -32017,
        "PrerequisiteFailed",
        r#"{"reason":"InputService_001 is not the pinned CInputService"}"#,
    );
    assert_eq!(got, want);
    let got = state.exec_request(&request("game.selectWeapon", Some(r#"{"weapon":"weapon_ak47"}"#)));
    let want = control::error_ext(
        Some("7"),
        -32017,
        "PrerequisiteFailed",
        r#"{"reason":"weapon/defIndex/handle selection needs client.dll and schemasystem.dll pinned"}"#,
    );
    assert_eq!(got, want);
}

/// `console.exec` outside a `-testscript`/`-tools` session (this test process's command line has
/// neither flag): the C's `-32011` before any module or param check (`mcp_bridge.c:5062-5066`).
#[test]
fn console_exec_needs_tools_or_testscript() {
    let state = BridgeState::new(
        Arc::new(FakeModules { state: ModState::Verified, revision: 0 }),
        Box::new(dispatch::is_ported),
    );
    let got = state.exec_request(&request("console.exec", Some(r#"{"command":"echo hi"}"#)));
    let want = control::error_ext(
        Some("7"),
        -32011,
        "NotImplemented",
        r#"{"reason":"console.exec is only enabled in -testscript or -tools sessions"}"#,
    );
    assert_eq!(got, want);
}

/// The valid-param bounds get through: game.entities with an in-range max and entindex
/// proceeds to the body's engine-client identity check (the same refusal as above), and
/// game.selectWeapon's slot bound reaches the input-service refusal - no param error.
#[test]
fn valid_params_reach_the_body_identity_checks() {
    let state = BridgeState::new(
        Arc::new(FakeModules { state: ModState::Verified, revision: 0 }),
        Box::new(dispatch::is_ported),
    );
    for (method, params) in [
        ("game.entities", r#"{"max":1,"entindex":16383}"#),
        ("game.entities", r#"{"max":256}"#),
        ("game.selectWeapon", r#"{"slot":0}"#),
        ("game.selectWeapon", r#"{"slot":13}"#),
        ("game.selectWeapon", r#"{"command":"lastinv"}"#),
        ("game.selectWeapon", r#"{"defIndex":7}"#),
    ] {
        let got = state.exec_request(&request(method, Some(params)));
        assert!(
            !got.contains(r#""code":-32602"#),
            "{method} {params}: not a param refusal: {got}"
        );
    }
}
