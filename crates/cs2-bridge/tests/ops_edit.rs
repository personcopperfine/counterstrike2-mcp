//! The EDIT lane's negative gate and registration, over the real `wiring::BridgeState` — the
//! same shape `tests/ops_hammer.rs` holds for the write rows, now for `edit.batch`,
//! `entity.tieMeshes` and `entity.untie` (`mcp_bridge.c:5855-5970`, `:5988`, `:5990`).
//!
//! The brief's gate item 3: with hammer.dll unverified, `exec_request` answers the C's
//! `-32017 PrerequisiteFailed` naming `hammer.dll` (the first unmet `needs` bit) and the body
//! is never called. The ported predicate is forced to `true`, so the answer can only come from
//! the gate (`exec_request:5668-5698`); the body, were it called, answers a different error
//! (on a non-Windows host `active_doc` reports `hammer_not_loaded` -> `-32003`).

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

/// The rows this lane serves, in the C dispatch chain's order (`:5855`, `:5988`, `:5990`).
const ROWS: [&str; 3] = ["edit.batch", "entity.tieMeshes", "entity.untie"];

#[test]
fn an_unverified_hammer_refuses_every_edit_row_before_its_body() {
    // The bodies themselves answer `-32003 NoActiveDocument` on this host (there is no
    // hammer.dll to resolve), so the `-32017` below can only come from the gate
    // (`exec_request:5668-5698`), which is the proof that no body ran.
    for method in ROWS {
        let bridge = BridgeState::new(
            Arc::new(FakeModules {
                state: ModState::Mismatch,
                revision: 4,
            }),
            Box::new(|_: &str| true),
        );
        assert!(dispatch::is_ported(method), "{method} must be served");
        assert_eq!(
            bridge.exec_request(&request(method, "e1")),
            control::prerequisite_failed(Some("e1"), "hammer.dll", ModState::Mismatch),
            "{method}: the C's -32017 object"
        );
        assert_eq!(
            bridge.route(method),
            Route::Queue,
            "{method}: the IPC lane defers the needs check to the main thread (:6623)"
        );
        let handler = cs2_bridge::ops::lookup(method).expect("the row is served");
        let body = handler(&request(method, "e2"));
        assert!(
            body.contains("\"code\":-32003"),
            "{method}: the body's own answer is NoActiveDocument: {body}"
        );
        assert!(
            !body.contains("\"code\":-32017"),
            "{method}: the -32017 above cannot have come from the body: {body}"
        );
    }
}

#[test]
fn the_edit_rows_are_advertised_once_hammer_verifies() {
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
