//! The MODELDOC lane's negative gate, registration and wire shapes, over the real
//! `wiring::BridgeState`.
//!
//! The brief's gate item 3: with modeldoc_editor.dll unverified, `exec_request` answers the
//! C's `-32017 PrerequisiteFailed` naming `modeldoc_editor.dll` (the first unmet `needs` bit)
//! and the body is never called. The ported predicate is forced to `true`, so the answer can
//! only come from the gate (`exec_request:5668-5698`); a body, were it called, answers a
//! different error (on a non-Windows host `active_modeldoc` reports `modeldoc_not_loaded` ->
//! `-32003`).
//!
//! Item 4 (every handler is a C row) is the `ops` self-test; this file adds the wire shape and
//! `modeldoc.open`'s param validation, which the C runs before any game access
//! (`mcp_bridge.c:5217-5256`) - so it is deterministic over the host reader.

use std::sync::Arc;
use std::time::Instant;

use cs2_bridge::control;
use cs2_bridge::dispatch;
use cs2_bridge::ipc::Route;
use cs2_bridge::modules::ModuleViews;
use cs2_bridge::queue::Request;
use cs2_bridge::registry::ModState;
use cs2_bridge::wiring::BridgeState;

/// Every module in one state, revision fixed (the same fake `tests/integration.rs` uses).
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

fn request(method: &str, id: &str, params: Option<&str>) -> Request {
    Request {
        id: id.to_string(),
        method: method.to_string(),
        params: params.map(str::to_string),
        received: Instant::now(),
        deadline: None,
        gen: 1,
    }
}

/// The six rows this lane serves, in the C table's order (`registry.rs:183-188`).
const ROWS: [&str; 6] = [
    "modeldoc.active",
    "modeldoc.open",
    "modeldoc.reload",
    "modeldoc.save",
    "modeldoc.compile",
    "modeldoc.nodeTypes",
];

#[test]
fn an_unverified_modeldoc_fails_closed_before_any_body() {
    for state in [ModState::NotLoaded, ModState::Mismatch] {
        let bridge = BridgeState::new(
            Arc::new(FakeModules {
                state,
                revision: 2,
            }),
            // The row is served by this build; the gate must answer -32017 anyway.
            Box::new(|_: &str| true),
        );
        for method in ROWS {
            assert!(
                dispatch::is_ported(method),
                "{method} must be served by this build"
            );
            assert_eq!(
                bridge.exec_request(&request(method, "g1", None)),
                control::prerequisite_failed(Some("g1"), "modeldoc_editor.dll", state),
                "{method}: the C's -32017 object, module from the needs table"
            );
            assert_eq!(
                bridge.route(method),
                Route::Queue,
                "{method}: the IPC lane defers the needs check to the main thread (:6623)"
            );
        }
    }
}

#[test]
fn the_ported_rows_are_advertised_once_modeldoc_verifies() {
    let bridge = BridgeState::new(
        Arc::new(FakeModules {
            state: ModState::Verified,
            revision: 5,
        }),
        Box::new(dispatch::is_ported),
    );
    let caps = bridge.capabilities();
    for method in ROWS {
        assert!(
            caps.contains(&format!("\"{method}\"")),
            "{method} missing from capabilities: {caps}"
        );
    }
}

#[test]
fn open_validates_params_before_any_game_access() {
    let bridge = BridgeState::new(
        Arc::new(FakeModules {
            state: ModState::Verified,
            revision: 5,
        }),
        Box::new(dispatch::is_ported),
    );
    // The C's first refusal (`mcp_bridge.c:5218`): no params / no `path` member / empty path.
    for params in [None, Some("{}"), Some(r#"{"path":""}"#)] {
        assert_eq!(
            bridge.exec_request(&request("modeldoc.open", "g2", params)),
            control::error_ext(
                Some("g2"),
                -32602,
                "InvalidParams",
                r#"{"reason":"path: relative to the addon content root (models/...), 1..512 chars, no '..'"}"#
            ),
            "{params:?}: the C's InvalidParams object, before any game access"
        );
    }
    // The later refusals in the C's order (`mcp_bridge.c:5222`, `:5229`, `:5240`, `:5244`,
    // `:5253`).
    for (params, reason) in [
        (format!(r#"{{"path":"{}"}}"#, "a".repeat(513)), "path: 1..512 chars"),
        (r#"{"path":"models\\kit.vmdl"}"#.to_string(), "path: no JSON escapes (use '/' separators)"),
        (format!(r#"{{"path":"a{}b.vmdl"}}"#, "\u{1}"), "path: no control characters"),
        (r#"{"path":"/models/kit.vmdl"}"#.to_string(), "path: relative to the addon content root (models/...), not absolute"),
        (r#"{"path":"models/kit.vmap"}"#.to_string(), "path: a .vmdl file"),
        (r#"{"path":"models/../kit.vmdl"}"#.to_string(), "path: no '..'"),
    ] {
        assert_eq!(
            bridge.exec_request(&request("modeldoc.open", "g2", Some(&params))),
            control::error_ext(
                Some("g2"),
                -32602,
                "InvalidParams",
                &format!(r#"{{"reason":"{reason}"}}"#)
            ),
            "{params}: the C's refusal at its position in the check order"
        );
    }
    // A valid path passes validation and reaches the reader, which (on a host without
    // modeldoc_editor.dll) answers the C's no-app refusal (`mcp_bridge.c:5258`).
    assert_eq!(
        bridge.exec_request(&request("modeldoc.open", "g2", Some(r#"{"path":"models/kit.vmdl"}"#))),
        control::error_ext(
            Some("g2"),
            -32003,
            "NoActiveDocument",
            r#"{"reason":"modeldoc_unavailable"}"#
        ),
        "validation passed; the C's -32003 object from the reader's resolve step"
    );
}

#[test]
fn the_no_module_shapes_are_the_c_answers_without_a_module() {
    let bridge = BridgeState::new(
        Arc::new(FakeModules {
            state: ModState::Verified,
            revision: 5,
        }),
        Box::new(dispatch::is_ported),
    );
    // `modeldoc.active` never errors on no doc (`mcp_bridge.c:5202-5207`).
    assert_eq!(
        bridge.exec_request(&request("modeldoc.active", "g3", None)),
        control::result_ok(Some("g3"), "{\"doc\":null,\"reason\":\"modeldoc_not_loaded\"}"),
        "the C's no-doc shape with the module-state reason"
    );
    // `reload` / `save` / `compile` refuse through `active_modeldoc`'s reason
    // (`mcp_bridge.c:5276`, `:5309`, `:5337`).
    for method in ["modeldoc.reload", "modeldoc.save", "modeldoc.compile"] {
        assert_eq!(
            bridge.exec_request(&request(method, "g3", None)),
            control::error_ext(
                Some("g3"),
                -32003,
                "NoActiveDocument",
                r#"{"reason":"modeldoc_not_loaded"}"#
            ),
            "{method}: the C's -32003 object with the module-state reason"
        );
    }
}
