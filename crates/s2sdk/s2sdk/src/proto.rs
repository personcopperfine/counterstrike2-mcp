//! Generated protobuf messages from the upstream Source 2 SDK (`.proto` sources under
//! `$S2SDK_DIR`, compiled by `build.rs` with upstream's `protoc` and prost-build).
//!
//! All generated code is re-exported into this module. This is raw prost output — no
//! hand-written addresses, no hand-written field numbers; regenerate via `cargo build`.

#![allow(clippy::all)]
#![allow(missing_docs, rustdoc::all, missing_copy_implementations)]

// The upstream protos have no `package` statements, so everything (all files, no name
// collisions in the compiled set) lands in a single `_.rs` module in OUT_DIR.
include!(concat!(env!("OUT_DIR"), "/_.rs"));
