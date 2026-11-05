//! Raw Source 2 SDK bindings for CS2, generated from the alliedmodders/s2sdk revision pinned in
//! `UPSTREAM.lock` by `gen/gen.py` (see `../README.md`).
//!
//! `generated/` is output only: change the allowlist or the generator, never the files.
//! It holds the numbers of both targets the game ships for: an item whose layout the
//! Itanium (Linux x86_64) and MSVC (Windows x64) ABIs agree on is emitted once, an item
//! they disagree on (a vtable struct, a one-slot destructor, a reversed overload group,
//! an alignment) twice, behind `cfg(not(windows))` and `cfg(windows)`.
//! `abi` computes vtable slots for both ABIs from the declaration-order method lists the
//! generator records; `Abi::native` is the ABI of the `cfg` the crate is built for.
#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]

pub mod abi;
pub mod generated;
