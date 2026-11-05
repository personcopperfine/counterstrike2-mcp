//! The method bodies (M4): the arms of the C's `exec_request` dispatch chain
//! (`mcp_bridge.c:5698-6279`) and the `op_*` functions they call, one module per method
//! group.
//!
//! A group module exports `pub static HANDLERS: &[(&str, Handler)]` and is listed once in
//! [`GROUPS`]. That is the whole registration: [`lookup`] finds the body,
//! [`crate::dispatch::is_ported`] serves every name that has one, and the tick's drain calls it
//! on the main thread once the gate passed (unknown, blocked and `-32017` are answered before a
//! body runs, as in the C). A panic in a body is caught by the drain and answered `-32603`.
//!
//! A body gets the request and returns the whole reply line, built with [`crate::control`]'s
//! `result_ok` / `error` / `error_ext` so the envelope (`bridge` member, id echo) is the C's.

pub mod cvar;
pub mod game;
pub mod ident;
pub mod hammer;
pub mod input;
pub mod job;
pub mod map;
pub mod metamod;
pub mod mesh;
pub mod modeldoc;
pub mod pet;
pub mod prefab;
pub mod edit;
pub mod render;
pub mod selection;
pub mod sound;

// `job` has no `HANDLERS` entry either: `job.start`/`job.status`/`job.cancel` are `needs == 0`
// lane-answered rows (`ops::job::answer`, called from `wiring::BridgeState::route_with_params`).
//
// `metamod` has deliberately NO `HANDLERS` entry: `metamod.plugins` is a control-plane row
// (`needs == 0`), answered on the IPC lane from its snapshot (`ops::metamod::plugins_answer`,
// called from `wiring::BridgeState::route`) - the one shape a queued main-thread body cannot
// give it ("works when nothing ticks"). `ops::mod`'s own test pins every `HANDLERS` row to
// `needs != 0`, which is the same statement from the other side.

use crate::control;
use crate::entity_json::Sb;
use crate::queue::Request;

/// One method body: the request in, the whole reply frame out (newline included, as every
/// [`crate::control`] builder writes it).
pub type Handler = fn(&Request) -> String;

/// Every group's handler table. A group adds one line here.
static GROUPS: &[&[(&str, Handler)]] = &[
    game::HANDLERS,
    cvar::HANDLERS,
    hammer::HANDLERS,
    input::HANDLERS,
    pet::HANDLERS,
    modeldoc::HANDLERS,
    map::HANDLERS,
    edit::HANDLERS,
    mesh::HANDLERS,
    prefab::HANDLERS,
    render::HANDLERS,
    selection::HANDLERS,
    sound::HANDLERS,
];

/// The body serving `name`, if this build has one.
#[must_use]
pub fn lookup(name: &str) -> Option<Handler> {
    GROUPS
        .iter()
        .flat_map(|group| group.iter())
        .find(|(method, _)| *method == name)
        .map(|(_, handler)| *handler)
}

/// Every name with a body, in table order.
pub fn names() -> impl Iterator<Item = &'static str> {
    GROUPS.iter().flat_map(|group| group.iter()).map(|(name, _)| *name)
}

/// The C's `OpErr` (`mcp_bridge.c:1988`): an op's error code, name and `data` object (empty =
/// no `data` member, the C's `e.data[0] ? e.data : 0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpErr {
    /// The JSON-RPC error code.
    pub code: i64,
    /// The error message (`OpErr.name`).
    pub name: &'static str,
    /// The pre-serialized `data` object, `""` for none.
    pub data: String,
}

/// `OpErr.data`'s capacity (`char data[512]`, `mcp_bridge.c:1988`).
pub const OP_ERR_DATA_CAP: usize = 512;

impl OpErr {
    /// `op_err` (`mcp_bridge.c:1990-1996`): `{"reason":<reason>}`, built in the C's 512 bytes.
    #[must_use]
    pub fn new(code: i64, name: &'static str, reason: &str) -> Self {
        Self::with_data(code, name, |db| {
            db.add(b"{\"reason\":");
            db.addjson(reason.as_bytes());
            db.add(b"}");
        })
    }

    /// `op_err_maybe` (`mcp_bridge.c:2000-2007`): `{"reason":..,"maybeExecuted":true}`.
    #[must_use]
    pub fn maybe(code: i64, name: &'static str, reason: &str) -> Self {
        Self::with_data(code, name, |db| {
            db.add(b"{\"reason\":");
            db.addjson(reason.as_bytes());
            db.add(b",\"maybeExecuted\":true}");
        })
    }

    /// An error whose `data` the caller writes into the C's 512-byte scratch, as the C's
    /// hand-built `e->data` sites do (same truncation on overflow).
    #[must_use]
    pub fn with_data(code: i64, name: &'static str, write: impl FnOnce(&mut Sb)) -> Self {
        let mut buf = vec![0u8; OP_ERR_DATA_CAP];
        let mut db = Sb::new(&mut buf);
        write(&mut db);
        let data = String::from_utf8_lossy(db.as_cstr()).into_owned();
        Self { code, name, data }
    }

    /// No `data` member (the C's `e.data[0] == 0`).
    #[must_use]
    pub fn bare(code: i64, name: &'static str) -> Self {
        Self {
            code,
            name,
            data: String::new(),
        }
    }
}

/// The C's dispatch-arm tail around an `op_*` call (e.g. `mcp_bridge.c:6013-6024`): the error
/// with its `data`, or `-32603 <overflow>` when the result builder overflowed its buffer, or
/// the result object wrapped in the envelope.
#[must_use]
pub fn finish(id: &str, outcome: Result<(), OpErr>, result: &Sb, overflow: &str) -> String {
    match outcome {
        Err(e) if e.data.is_empty() => control::error(Some(id), e.code, e.name),
        Err(e) => control::error_ext(Some(id), e.code, e.name, &e.data),
        Ok(()) if result.ovf() => control::error(Some(id), -32603, overflow),
        Ok(()) => control::result_ok(Some(id), &String::from_utf8_lossy(result.as_cstr())),
    }
}

#[cfg(test)]
mod tests {
    use super::{finish, names, OpErr};
    use crate::control;
    use crate::entity_json::Sb;
    use crate::registry;

    /// The four tails of a C dispatch arm: error with `data`, bare error, overflow, result.
    #[test]
    fn finish_builds_the_c_arm_tails() {
        let mut buf = vec![0u8; 7];
        let mut rb = Sb::new(&mut buf);
        rb.add(b"{\"a\":1}");
        assert!(rb.ovf(), "7 bytes cannot hold 7 + NUL (the C putc rule)");
        let err = OpErr::new(-32602, "InvalidParams", "bad \"x\"");
        assert_eq!(err.data, r#"{"reason":"bad \"x\""}"#);
        assert_eq!(
            finish("i", Err(err.clone()), &rb, "too large"),
            control::error_ext(Some("i"), -32602, "InvalidParams", &err.data)
        );
        assert_eq!(
            finish("i", Err(OpErr::bare(-32003, "NoActiveDocument")), &rb, "too large"),
            control::error(Some("i"), -32003, "NoActiveDocument")
        );
        assert_eq!(
            finish("i", Ok(()), &rb, "too large"),
            control::error(Some("i"), -32603, "too large")
        );
        let mut buf = vec![0u8; 64];
        let mut rb = Sb::new(&mut buf);
        rb.add(b"{\"a\":1}");
        assert_eq!(
            finish("i", Ok(()), &rb, "too large"),
            control::result_ok(Some("i"), r#"{"a":1}"#)
        );
        assert_eq!(
            OpErr::maybe(-32012, "PartialFailure", "r").data,
            r#"{"reason":"r","maybeExecuted":true}"#
        );
    }

    /// Every body serves a row the C implements and gates (`needs != 0`: the `control.*` fast
    /// path is answered by the IPC lane, never by a body), and no name is registered twice.
    #[test]
    fn every_handler_is_an_implemented_gated_row_registered_once() {
        let mut seen = std::collections::HashSet::new();
        for name in names() {
            let row = registry::find(name)
                .unwrap_or_else(|| panic!("a handler serves `{name}`, absent from the table"));
            assert!(row.implemented, "a handler serves `{name}`, a blocked row");
            assert_ne!(row.needs, 0, "a handler serves `{name}`, a control fast-path row");
            assert!(seen.insert(name), "`{name}` has two handlers");
        }
    }

    /// M4 is complete: every row the C implements and gates has a body, so a build serves the
    /// C's whole method table (the `control.*` fast path is the IPC lane's).
    #[test]
    fn every_implemented_gated_row_has_a_handler() {
        for row in registry::METHODS {
            if row.implemented && row.needs != 0 {
                assert!(
                    super::lookup(row.name).is_some(),
                    "`{}` is implemented in the C but has no body",
                    row.name
                );
            }
        }
    }
}
