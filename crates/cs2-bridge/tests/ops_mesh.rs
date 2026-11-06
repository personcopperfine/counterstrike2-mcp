//! The MESH lane's negative gate and registration, over the real `wiring::BridgeState`.
//!
//! The brief's gate item 3: with hammer.dll unverified, `exec_request` answers the C's
//! `-32017 PrerequisiteFailed` naming `hammer.dll` (the first unmet `needs` bit) and the body
//! is never called. The ported predicate is forced to `true`, so the answer can only come from
//! the gate (`exec_request:5668-5698`); the body, were it called, answers a different error
//! (on a non-Windows host the `geo_dispatch` head reports `hammer_not_loaded` -> `-32003`).
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

/// The rows this lane serves (`mcp_bridge.c:5972-5977`).
const ROWS: [&str; 3] = ["mesh.create", "mesh.setGeometry", "mesh.setMaterial"];

#[test]
fn an_unverified_hammer_fails_closed_before_any_mesh_body() {
    for state in [ModState::NotLoaded, ModState::Mismatch] {
        let bridge = BridgeState::new(
            Arc::new(FakeModules {
                state,
                revision: 2,
            }),
            // The rows are served by this build; the gate must answer -32017 anyway.
            Box::new(|_: &str| true),
        );
        for method in ROWS {
            assert!(
                dispatch::is_ported(method),
                "{method} must be served by this build"
            );
            assert_eq!(
                bridge.exec_request(&request(method, "m1")),
                control::prerequisite_failed(Some("m1"), "hammer.dll", state),
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
fn the_mesh_bodies_answer_no_active_document_when_called_directly() {
    for method in ROWS {
        let handler = cs2_bridge::ops::lookup(method).expect("the row is served");
        let body = handler(&request(method, "m2"));
        assert!(
            body.contains("\"code\":-32003"),
            "{method}: the body's own answer is NoActiveDocument: {body}"
        );
        assert!(
            !body.contains("\"code\":-32017"),
            "{method}: the -32017 must come from the gate, never the body: {body}"
        );
    }
}

#[test]
fn the_mesh_rows_are_advertised_once_hammer_verifies() {
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
