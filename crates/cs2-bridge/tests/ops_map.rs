//! The map lane's negative gate and registration, over the real `wiring::BridgeState`.
//!
//! # NEGATIVE (mandatory)
//!
//! With hammer.dll unverified, `exec_request` answers the C's `-32017 PrerequisiteFailed`
//! naming `hammer.dll` (the `map.*` rows' only `needs` bit) and the body is never called.
//! The proof is the answer itself: with the same request shaped so a BODY would have to
//! answer something else (no params at all: `map.open` refuses its own `-32602` shape,
//! `map.reload` and `map.save` their `-32003` NoActiveDocument - save's arm has no
//! `expect_doc_ok`, so no `-32004`), the -32017 can only come from
//! the gate (`mcp_bridge.c:5668-5698`), which runs before any body.
//!
//! Item 4 (every handler is a C row) is the `ops` self-test; this file adds the wire shape.

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

fn request(method: &str, id: &str) -> Request {
    Request {
        id: id.to_string(),
        method: method.to_string(),
        params: None,
        received: Instant::now(),
        deadline: None,
        gen: 1,
    }
}

/// The three rows this lane serves, in the C table's order (`registry.rs:154-156`).
const ROWS: [&str; 3] = ["map.save", "map.reload", "map.open"];

#[test]
fn an_unverified_hammer_fails_closed_before_any_body() {
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
                bridge.exec_request(&request(method, "g1")),
                control::prerequisite_failed(Some("g1"), "hammer.dll", state),
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
fn the_ported_rows_are_advertised_once_hammer_verifies() {
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
    // `map.run` stays a blocked row and is not advertised.
    assert!(
        !caps.contains("\"map.run\""),
        "map.run must not be advertised yet: {caps}"
    );
}
