//! The `pet.*` gate: the C's `-32017 PrerequisiteFailed` arm, and what this build does once the
//! gate passes.
//!
//! # NEGATIVE (mandatory)
//!
//! With the group's modules unverified, `wiring::BridgeState::exec_request` answers the C's
//! `-32017 PrerequisiteFailed` naming the first unmet need in the C's `bits[]` order
//! (`mcp_bridge.c:5676-5698`) - and the body is never called. The proof is the answer itself:
//! every `pet.*` body on this target answers something else entirely (below), so a body that ran
//! could not produce these bytes.
//!
//! # The pass-the-gate answers on a non-Windows host
//!
//! `pet.dll` is a Windows DLL, so this crate's other half answers the Rust-only `-32603` naming
//! the missing platform (see `ops::pet::game`); `pet.active`, `pet.open` and the two document
//! errors still give the C's own shapes because no game object is needed for them. Those cases
//! are asserted here so the platform split cannot drift silently, and they are listed in
//! `bridge/protocol-diff-m3-pet.md`.

use std::sync::Arc;
use std::time::Instant;

use cs2_bridge::control;
use cs2_bridge::dispatch;
use cs2_bridge::modules::ModuleViews;
use cs2_bridge::queue::Request;
use cs2_bridge::registry::{ModState, NEED_PET, NEED_SCHEMA, NEED_TF2, NEED_TIER0};
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

/// The eleven rows this group serves, in the C's `op_pet` order, with the module the C's `bits[]`
/// loop names first when nothing is verified.
const ROWS: &[(&str, &str)] = &[
    ("pet.load", "toolframework2.dll"),
    ("pet.text", "pet.dll"),
    ("pet.apply", "tier0.dll"),
    ("pet.undo", "pet.dll"),
    ("pet.redo", "pet.dll"),
    ("pet.active", "pet.dll"),
    ("pet.open", "pet.dll"),
    ("pet.reload", "pet.dll"),
    ("pet.save", "pet.dll"),
    ("pet.functionTypes", "pet.dll"),
    ("pet.schema", "schemasystem.dll"),
];

/// The served names are exactly the eleven; `pet.render` is served by the render lane, not by
/// this group's table (`ops::pet::HANDLERS`).
#[test]
fn the_group_serves_eleven_rows_and_not_render() {
    scratch_log();
    assert_eq!(ROWS.len(), 11);
    for (method, _) in ROWS {
        assert!(dispatch::is_ported(method), "`{method}` must be served");
        assert!(
            cs2_bridge::ops::lookup(method).is_some(),
            "`{method}` must have a body"
        );
    }
    // `pet.render` is served - by the render lane's table, not this group's (`ops::pet` has no
    // row for it, so the duplicate would be a second body for the same method).
    assert!(
        dispatch::is_ported("pet.render"),
        "pet.render must be served by the render lane"
    );
    assert!(
        cs2_bridge::ops::lookup("pet.render").is_some(),
        "pet.render must have a body"
    );
}

/// NEGATIVE: every row answers `-32017` naming the C's module while nothing verifies, and the
/// body is not reached.
#[test]
fn unverified_modules_refuse_every_row_before_the_body() {
    scratch_log();
    // The predicate says every row is ported, so only the needs check can refuse.
    let state = BridgeState::new(
        Arc::new(FakeModules {
            state: ModState::NotLoaded,
        }),
        Box::new(|_: &str| true),
    );
    for (method, module) in ROWS {
        let id = format!("n-{method}");
        assert_eq!(
            state.exec_request(&request(method, &id, None)),
            control::prerequisite_failed(Some(&id), module, ModState::NotLoaded),
            "`{method}`: the C's -32017 arm, module from the priority table"
        );
    }
}

/// The needs each row carries are the C table's (`mcp_bridge.c:1128-1139` via `registry`): this
/// pins the three multi-module rows the C's `bits[]` order picks apart.
#[test]
fn the_needs_are_the_c_tables() {
    scratch_log();
    let needs = |name: &str| {
        cs2_bridge::registry::find(name)
            .unwrap_or_else(|| panic!("`{name}` is not in the table"))
            .needs
    };
    assert_eq!(needs("pet.load"), NEED_TF2, "toolframework2 only: pet.dll is not loaded yet");
    for method in [
        "pet.active",
        "pet.open",
        "pet.reload",
        "pet.save",
        "pet.text",
        "pet.undo",
        "pet.redo",
        "pet.functionTypes",
    ] {
        assert_eq!(needs(method), NEED_PET, "{method}");
    }
    assert_eq!(needs("pet.apply"), NEED_PET | NEED_TIER0);
    assert_eq!(needs("pet.schema"), NEED_PET | NEED_SCHEMA);
}

/// Once every module verifies the gate lets the body through: the served rows appear in
/// `control.capabilities` and each answers its body's reply, not the gate's.
#[test]
fn verified_modules_reach_the_body() {
    scratch_log();
    let state = BridgeState::new(
        Arc::new(FakeModules {
            state: ModState::Verified,
        }),
        Box::new(dispatch::is_ported),
    );
    let caps = state.capabilities();
    for (method, _) in ROWS {
        assert!(
            caps.contains(&format!("\"{method}\"")),
            "`{method}` must be advertised once its needs are met"
        );
    }
    // `pet.render` is advertised too, through the render lane's ported row.
    assert!(
        caps.contains("\"pet.render\""),
        "pet.render must be advertised by the render lane"
    );

    // The bodies, which on this target cannot answer the gate's -32017.
    let unsupported = control::error_ext(
        Some("b1"),
        -32603,
        "InternalError",
        r#"{"reason":"pet.* needs the Windows target: this build has no pet.dll half"}"#,
    );
    for method in [
        "pet.load",
        "pet.text",
        "pet.undo",
        "pet.redo",
        "pet.functionTypes",
        "pet.schema",
    ] {
        let params = if method == "pet.schema" {
            Some(r#"{"class":"C_OP_Foo"}"#)
        } else {
            None
        };
        let reply = state.exec_request(&request(method, "b1", params));
        assert_eq!(reply, unsupported, "`{method}` on a target with no pet.dll");
        assert!(
            !reply.contains("-32017"),
            "`{method}` reached its body, so the gate had already passed"
        );
    }

    // `pet.apply` with text that passes the KV3 gate reaches the same platform refusal.
    let doc = r#"<!-- kv3 encoding:text:format:vpcf -->\n{ _class = \"CParticleSystemDefinition\" }"#;
    assert_eq!(
        state.exec_request(&request("pet.apply", "b1", Some(&format!(r#"{{"text":"{doc}"}}"#)))),
        unsupported
    );

    // The three C-shaped answers a host without pet.dll can still give.
    assert_eq!(
        state.exec_request(&request("pet.active", "b2", None)),
        control::result_ok(Some("b2"), r#"{"doc":null,"reason":"pet_not_loaded"}"#),
        "pet.active is a result, not an error (`mcp_bridge_pet.inc:1137-1139`)"
    );
    for method in ["pet.reload", "pet.save"] {
        assert_eq!(
            state.exec_request(&request(method, "b3", None)),
            control::error_ext(
                Some("b3"),
                -32003,
                "NoActiveDocument",
                r#"{"reason":"pet_not_loaded"}"#
            ),
            "`{method}` needs an active session (`:1156-1158`, `:1214-1216`)"
        );
    }
    assert_eq!(
        state.exec_request(&request("pet.open", "b4", Some(r#"{"path":"particles/x.vpcf"}"#))),
        control::error_ext(
            Some("b4"),
            -32602,
            "UnloadableFile",
            r#"{"reason":"no_app_root","path":"particles/x.vpcf"}"#
        ),
        "the G2 gate cannot find <app>\\game\\bin\\win64\\cs2.exe here (`:384-388`)"
    );
}

/// Point the bridge log at `target/scratch/` for this test binary, and answer the path.
///
/// An integration test links the library **without** `cfg(test)`, so `log::line`'s default
/// path would be the user's real `$XDG_RUNTIME_DIR/cs2mcpbridge/bridge.log` (`src/log.rs`). A
/// `cargo test` must never append to that file, so every case that can log points
/// `CS2_BRIDGE_LOG` here first. Process-wide and idempotent: the first call sets the variable
/// and every later call answers the same path (a case that logs to its own file restores it
/// with this).
fn scratch_log() -> std::path::PathBuf {
    static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch");
        std::fs::create_dir_all(&dir).expect("target/scratch is writable");
        let path = dir.join("cs2-bridge-ops-pet-gate-test.log");
        std::env::set_var("CS2_BRIDGE_LOG", &path);
        path
    })
    .clone()
}
