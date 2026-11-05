//! The particle editor group (`pet.*`, minus `pet.render`): the C bridge's
//! `mcp_bridge_pet.inc` (frozen, m2.20) ported method for method.
//!
//! Eleven rows are served here - `pet.load`, `pet.active`, `pet.open`, `pet.reload`, `pet.save`,
//! `pet.functionTypes`, `pet.schema`, `pet.text`, `pet.apply`, `pet.undo`, `pet.redo`.
//! `pet.render` is declared in the same C file (`mcp_bridge_pet.inc:1123`) but defined in
//! `mcp_bridge_pet_render.inc`; it belongs to the render lane and is deliberately NOT registered
//! (its row answers `-32011 NotImplemented` like any other unported row).
//!
//! # Shape
//!
//! * [`reply`] - the pure reply builders. Every one takes plain input (strings, numbers, small
//!   structs a thin reader filled from game memory) and writes the C's exact bytes into a
//!   [`reply::Buf`]; the host tests drive them without a game.
//! * [`game`] - the thin readers (Windows only) that walk pet.dll's objects and produce those
//!   plain structs, plus the pinned entry points' binding.
//! * this file - the C's `op_pet` dispatch (`mcp_bridge_pet.inc:1125-1250`), the arm tail, and
//!   the parameter validation every method runs before it touches game memory.
//!
//! Everything runs on the game's main thread (the tick's drain), like the C.

pub mod game;
pub mod reply;

use crate::control;
use crate::ops::OpErr;
use crate::queue::Request;

use self::reply::Buf;

/// One method body: the request in, the whole reply frame out.
pub type Handler = fn(&Request) -> String;

/// The names this group serves, in the C's `op_pet` order (`mcp_bridge_pet.inc:1125-1250`).
/// `pet.render` is absent on purpose (see the module docs).
pub static HANDLERS: &[(&str, Handler)] = &[
    ("pet.load", load),
    ("pet.text", text),
    ("pet.apply", apply),
    ("pet.undo", undo),
    ("pet.redo", redo),
    ("pet.active", active),
    ("pet.open", open),
    ("pet.reload", reload),
    ("pet.save", save),
    ("pet.functionTypes", function_types),
    ("pet.schema", schema),
];

/// A fresh reply builder for one `pet.*` call: the C's `static char rbuf[MAX_LINE - 2048]`
/// (`mcp_bridge.c:6222`).
#[must_use]
pub fn buffer() -> Buf {
    Buf::new(control::MAX_LINE as usize - 2048)
}

/// The C's arm tail for `pet.*` (`mcp_bridge.c:6224-6231`): the error with its `data`, the
/// `-32603` overflow, or the result object in the envelope.
///
/// Semantically the same four arms as [`crate::ops::finish`] (`ops/mod.rs:104-111`), which reads
/// [`crate::entity_json::Sb::ovf`]; this group's builder is a [`Buf`] (the C's `SB` plus the
/// mark/rollback the C's `out->len = mark` sites need, which `Sb` does not expose), so the arms
/// are spelled over its `ovf`. The bytes are the same.
#[must_use]
pub fn finish_pet(id: &str, outcome: Result<(), OpErr>, out: &Buf) -> String {
    match outcome {
        Err(e) if e.data.is_empty() => control::error(Some(id), e.code, e.name),
        Err(e) => control::error_ext(Some(id), e.code, e.name, &e.data),
        Ok(()) if out.ovf() => {
            control::error(Some(id), -32603, "InternalError: pet result too large")
        }
        Ok(()) => control::result_ok(Some(id), &String::from_utf8_lossy(out.bytes())),
    }
}

/// One `pet.*` reply: a fresh buffer, the body, the C's arm tail.
fn dispatch(id: &str, run: impl FnOnce(&mut Buf) -> Result<(), OpErr>) -> String {
    let mut out = buffer();
    let outcome = run(&mut out);
    finish_pet(id, outcome, &out)
}

fn load(request: &Request) -> String {
    dispatch(&request.id, game::op_load)
}

/// `pet.active` (`mcp_bridge_pet.inc:1133-1153`) never errors on a missing document: it answers
/// `{"doc":null,"reason":<reason>}`.
fn active(request: &Request) -> String {
    dispatch(&request.id, game::op_active)
}

fn undo(request: &Request) -> String {
    dispatch(&request.id, |out| game::op_undo_redo(out, false))
}

fn redo(request: &Request) -> String {
    dispatch(&request.id, |out| game::op_undo_redo(out, true))
}

fn text(request: &Request) -> String {
    let params = request.params.as_deref();
    dispatch(&request.id, |out| game::op_text(params, out))
}

fn apply(request: &Request) -> String {
    let params = request.params.as_deref();
    dispatch(&request.id, |out| game::op_apply(params, out))
}

fn open(request: &Request) -> String {
    let params = request.params.as_deref();
    dispatch(&request.id, |out| game::op_open(params, out))
}

fn reload(request: &Request) -> String {
    let params = request.params.as_deref();
    dispatch(&request.id, |out| game::op_reload(params, out))
}

fn save(request: &Request) -> String {
    let params = request.params.as_deref();
    dispatch(&request.id, |out| game::op_save(params, out))
}

fn function_types(request: &Request) -> String {
    let params = request.params.as_deref();
    dispatch(&request.id, |out| game::op_function_types(params, out))
}

fn schema(request: &Request) -> String {
    let params = request.params.as_deref();
    dispatch(&request.id, |out| game::op_schema(params, out))
}

#[cfg(test)]
mod tests {
    use super::{buffer, finish_pet, HANDLERS};
    use crate::control;
    use crate::ops::OpErr;
    use crate::registry;

    /// Every name here is a C row this group implements and gates, registered once, and exactly
    /// the eleven the brief lists (no `pet.render`).
    #[test]
    fn handlers_are_the_eleven_pet_rows() {
        let mut seen = std::collections::HashSet::new();
        for (name, _) in HANDLERS {
            let row = registry::find(name)
                .unwrap_or_else(|| panic!("a handler serves `{name}`, absent from the table"));
            assert!(row.implemented, "a handler serves `{name}`, a blocked row");
            assert_ne!(row.needs, 0, "a handler serves `{name}`, a control fast-path row");
            assert!(seen.insert(*name), "`{name}` has two handlers");
        }
        assert!(!seen.contains("pet.render"), "pet.render belongs to the render lane");
        assert_eq!(seen.len(), 11);
    }

    /// The four tails [`finish_pet`] can take, byte for byte (`mcp_bridge.c:6224-6231`).
    #[test]
    fn finish_takes_the_c_arm_tails() {
        let mut out = buffer();
        out.add(b"{\"a\":1}");
        let err = OpErr::new(-32602, "InvalidParams", "bad \"x\"");
        assert_eq!(
            finish_pet("i", Err(err.clone()), &out),
            control::error_ext(Some("i"), -32602, "InvalidParams", &err.data)
        );
        assert_eq!(
            finish_pet("i", Err(OpErr::bare(-32003, "NoActiveDocument")), &out),
            control::error(Some("i"), -32003, "NoActiveDocument")
        );
        assert_eq!(
            finish_pet("i", Ok(()), &out),
            control::result_ok(Some("i"), r#"{"a":1}"#)
        );
        // A buffer smaller than the object overflows exactly where the C's `sb_putc` does.
        let mut small = crate::ops::pet::reply::Buf::new(8);
        small.add(b"{\"a\":12345}");
        assert!(small.ovf(), "8 bytes cannot hold a NUL-terminated 10-byte object");
        assert_eq!(
            finish_pet("i", Ok(()), &small),
            control::error(Some("i"), -32603, "InternalError: pet result too large")
        );
    }
}
