//! Safe wrappers over [`s2sdk_sys`] for code running inside a CS2 process (Linux client or
//! dedicated server).
//!
//! The layer never hands in addresses, vtable slots or field offsets: every call goes through
//! the generated `*_vtable` structs, `*_slots` consts and `VClass` statics of
//! `s2sdk_sys::generated`. `unsafe` stays inside this crate (plus the crate's own tests);
//! the public API is safe. It builds and runs on both targets the game ships for —
//! `x86_64-unknown-linux-gnu` (Itanium ABI) and `x86_64-pc-windows-msvc` (MSVC ABI) — with the
//! vtable-based code `#[cfg]`-gated on the ABI like the generated bindings, and the module,
//! interface-version and library-name details per target in [`module`].
//!
//! Module map:
//!
//! * [`module`] — find libraries the game has *already* mapped; `CreateInterface` factory calls
//! * [`interface`] — typed pointer wrapper around a created interface
//! * [`schema`] — `ISchemaSystem`: type scopes, declared classes and their fields
//! * [`cvar`] — `ICvar`: read convar metadata and values
//! * [`events`] — `IGameEventManager2` and a Rust-owned `IGameEventListener2`
//! * [`hook`] — KHook detours: `KHookApi`, the `Hook` RAII wrapper and the `khook_thunks!`
//!   macro
//! * [`rtti`] — an object's class from its vtable, through the module's own RTTI (MSVC x64 and
//!   Itanium, both readers compiled on every target)
//! * [`hash`] — the MurmurHash2 lowercase variant the string token uses

#![warn(missing_docs)]

#[cfg(feature = "proto")]
/// Protobuf messages generated from the upstream Source 2 SDK `.proto` sources (WP4).
pub mod proto;

pub mod cvar;
pub mod error;
pub mod events;
pub mod hash;
pub mod hook;
pub mod interface;
pub mod khook_vtable;
pub mod module;
pub mod rtti;
pub mod schema;

pub use error::{Error, Result};
