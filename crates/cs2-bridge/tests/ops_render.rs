//! The render lane's gate and its bodies on a host with no game (M4 lane RENDER).
//!
//! # NEGATIVE (mandatory)
//!
//! With the group's modules unverified, `wiring::BridgeState::exec_request` answers the C's
//! `-32017 PrerequisiteFailed` naming the first unmet need in the C's `bits[]` order
//! (`mcp_bridge.c:5676-5698`) - and the body is never called. The proof is the answer itself:
//! every render body on this target answers something else entirely (below), so a body that ran
//! could not produce these bytes.
//!
//! # What the bodies answer on a host without the game
//!
//! `view.render` resolves the document first (`mcp_bridge.c:6064`), `world.render` resolves the
//! engine interfaces first (`:2919`), and `pet.render` / `modeldoc.render` validate their
//! parameters first (`mcp_bridge_pet_render.inc:57`, `:204`). Those orders are the C's, so the
//! answers below pin them: a port that validated earlier or later would answer a different code.
//! The parameter matrices themselves are unit tests in `ops::render` (`view_params`,
//! `world_params`, `toolscene_params`) because `view.render`'s and `world.render`'s checks are
//! only reachable behind their game walks.

use std::sync::Arc;
use std::time::Instant;

use cs2_bridge::control;
use cs2_bridge::dispatch;
use cs2_bridge::modules::ModuleViews;
use cs2_bridge::queue::Request;
use cs2_bridge::registry::{ModState, NEED_HAMMER, NEED_MODELDOC, NEED_PET, NEED_RENDER};
use cs2_bridge::wiring::BridgeState;

/// A module-state source a test constructs: every module in one state.
struct FakeModules {
    state: ModState,
}

impl ModuleViews for FakeModules {
    fn state(&self, _module: u8) -> ModState {
        self.state
    }

    fn is_verified(&self, _module: u8) -> bool {
        self.state == ModState::Verified
    }

    fn revision(&self) -> u64 {
        1
    }
}

fn request(method: &str, id: &str, params: Option<&str>) -> Request {
    Request {
        id: id.to_string(),
        method: method.to_string(),
        params: params.map(std::string::ToString::to_string),
        received: Instant::now(),
        deadline: None,
        gen: 1,
    }
}

/// The four rows and the module the C's `bits[]` loop names first when nothing is verified:
/// `world.render` needs `NEED_RENDER` (scene | engine | rsv) and `scene` comes before `engine`.
const ROWS: &[(&str, u32, &str)] = &[
    ("view.render", NEED_HAMMER, "hammer.dll"),
    ("world.render", NEED_RENDER, "scenesystem.dll"),
    ("pet.render", NEED_PET, "pet.dll"),
    ("modeldoc.render", NEED_MODELDOC, "modeldoc_editor.dll"),
];

/// The four names are served, have a body, and carry the C table's `needs`.
#[test]
fn the_group_serves_the_four_render_rows() {
    for (method, needs, _) in ROWS {
        assert!(dispatch::is_ported(method), "`{method}` must be served");
        assert!(
            cs2_bridge::ops::lookup(method).is_some(),
            "`{method}` must have a body"
        );
        let row = cs2_bridge::registry::find(method)
            .unwrap_or_else(|| panic!("`{method}` is not in the method table"));
        assert!(row.implemented, "`{method}` must be an implemented row");
        assert_eq!(row.needs, *needs, "`{method}`: the C table's needs");
    }
}

/// NEGATIVE: every row answers `-32017` naming the C's module while nothing verifies, and the
/// body is not reached (its own answer is a different code, asserted below).
#[test]
fn unverified_modules_refuse_every_row_before_the_body() {
    let state = BridgeState::new(
        Arc::new(FakeModules {
            state: ModState::NotLoaded,
        }),
        Box::new(|_: &str| true),
    );
    for (method, _, module) in ROWS {
        let id = format!("n-{method}");
        assert_eq!(
            state.exec_request(&request(method, &id, None)),
            control::prerequisite_failed(Some(&id), module, ModState::NotLoaded),
            "`{method}`: the C's -32017 arm, module from the priority table"
        );
    }
}

/// Once every module verifies, the gate lets the bodies through: the four rows are advertised and
/// each answers its body's reply - none of them the gate's.
#[test]
fn verified_modules_reach_the_body() {
    let state = BridgeState::new(
        Arc::new(FakeModules {
            state: ModState::Verified,
        }),
        Box::new(dispatch::is_ported),
    );
    let caps = state.capabilities();
    for (method, _, _) in ROWS {
        assert!(
            caps.contains(&format!("\"{method}\"")),
            "`{method}` must be advertised once its needs are met"
        );
    }

    // `view.render`: the C arm resolves the document before it parses a parameter
    // (`mcp_bridge.c:6063-6077`). On a host with no hammer.dll `active_doc` answers
    // `hammer_not_loaded`, so even a malformed request gets the arm's -32003.
    let no_doc = control::error_ext(
        Some("v1"),
        -32003,
        "NoActiveDocument",
        r#"{"reason":"hammer_not_loaded"}"#,
    );
    for params in [None, Some(r#"{"width":8}"#)] {
        assert_eq!(
            state.exec_request(&request("view.render", "v1", params)),
            no_doc,
            "view.render: active_doc comes before the parameters"
        );
    }

    // `world.render`: the engine interfaces are resolved before the parameters (`:2919`); with no
    // `Connect` factory on this target that is the C's own refusal.
    for params in [
        None,
        Some(r#"{"origin":[0,0,0],"angles":[0,0,0]}"#),
        Some(r#"{"width":8}"#),
    ] {
        assert_eq!(
            state.exec_request(&request("world.render", "w1", params)),
            control::error_ext(
                Some("w1"),
                -32020,
                "RenderFailed",
                r#"{"reason":"engine interface not found (Connect factory)"}"#
            ),
            "world.render: the engine walk comes before the parameters"
        );
    }

    // `pet.render` / `modeldoc.render`: the parameters come first, then the object chain.
    let invalid = control::error_ext(
        Some("p1"),
        -32602,
        "InvalidParams",
        r#"{"reason":"width/height: 16..4096 each, at most 8388608 pixels"}"#,
    );
    for method in ["pet.render", "modeldoc.render"] {
        let id = format!("p-{method}");
        let expected = invalid.replace("\"p1\"", &format!("\"{id}\""));
        assert_eq!(
            state.exec_request(&request(method, &id, Some(r#"{"width":8}"#))),
            expected,
            "`{method}`: a bad width is refused before any object is touched"
        );
        assert_eq!(
            state.exec_request(&request(method, &id, Some(r#"{"width":4096,"height":4096}"#))),
            expected,
            "`{method}`: the pixel product bound"
        );
        // A name that fails the character rule (`mcp_bridge_pet_render.inc:65-68`).
        assert_eq!(
            state.exec_request(&request(method, &id, Some(r#"{"name":".x"}"#))),
            control::error_ext(
                Some(&id),
                -32602,
                "InvalidParams",
                r#"{"reason":"name: [A-Za-z0-9._-], 1..64 chars, no leading dot"}"#
            ),
            "`{method}`: bad name"
        );
    }
    assert_eq!(
        state.exec_request(&request("pet.render", "p2", None)),
        control::error_ext(
            Some("p2"),
            -32019,
            "NoViewport",
            r#"{"reason":"pet_unavailable"}"#
        ),
        "pet.render: no pet.dll, the C's own chain reason"
    );
    assert_eq!(
        state.exec_request(&request("modeldoc.render", "m2", None)),
        control::error_ext(
            Some("m2"),
            -32019,
            "NoViewport",
            r#"{"reason":"modeldoc_not_loaded"}"#
        ),
        "modeldoc.render: the module-state reason, the C's own"
    );
}
