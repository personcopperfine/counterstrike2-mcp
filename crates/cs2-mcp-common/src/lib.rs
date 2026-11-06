//! cs2-mcp-common: everything the two servers share.
//!
//! * [`bridge`]   — client for the in-process bridge (`mcp_bridge.c`, protocol
//!   `../idabins/docs/data/bridge-protocol.md`): discovery, hello, NDJSON, deadlines, the modal
//!   gate, capability refresh and the error taxonomy (port of the Python POC `bridge_client.py`).
//! * [`tools`]    — transport-agnostic tool registry with the shared policies (capability gating,
//!   per-call budget, no raw handles in outputs, uniform error payloads).
//! * [`mcp`]      — the only module that imports `rmcp`: stdio server over a registry.
//! * [`runtime`]  — launcher abstraction: `docker` (podman) now, `local` later.
//! * [`console`]  — the console command refusal guard both servers share.
//! * [`rcon`]     — minimal Source RCON client (the dedicated server's console).
//! * [`sigs`]     — signature-DB reader (names only; this crate contains no addresses).
//! * [`capture`]  — screenshot/render sources + OCR.
//! * [`config`]   — the flags/env both servers accept.
//! * [`pathmap`]  — Windows (`Z:\\app\\...`) -> host path mapping for bridge-written files.
//! * [`mock`]     — (feature `mock`) a Rust mock bridge replaying the recorded fixtures with the
//!   failure knobs the contract tests need.
//!
//! Rule for every module: stdout belongs to the MCP transport; log with `tracing` to stderr.

// Error payloads are deliberately rich (the POC's taxonomy dict); they are not on a hot path.
#![allow(clippy::result_large_err)]

pub mod bridge;
pub mod capture;
pub mod config;
pub mod console;
pub mod mcp;
pub mod pathmap;
pub mod play;
pub mod prompts;
pub mod rcon;
pub mod runtime;
pub mod sigs;
pub mod status;
pub mod tools;

#[cfg(feature = "mock")]
pub mod mock;

/// Host layout default: the project root is this checkout (the crate lives at
/// `<root>/crates/cs2-mcp-common`). The root carries the `workshop/` submodule and the runtime state under it (`workshop/run/app`, `.podman/xdg`). `--project-root` /
/// `CS2_MCP_PROJECT_ROOT` points elsewhere; [`mcp_repo_dir`] also accepts the older layout, a
/// parent directory that holds this repo as `cs2-mcp/`.
pub const DEFAULT_PROJECT_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// This repo's own files (research snapshots, `target/`, `cargo bridge runtime`) under a project
/// root: `<root>/cs2-mcp` when the root is the older parent layout (a directory holding this
/// repo's checkout as `cs2-mcp/`), else the root itself (the default: the root is this checkout).
pub fn mcp_repo_dir(project_root: &std::path::Path) -> std::path::PathBuf {
    let nested = project_root.join("cs2-mcp");
    if nested.join("Cargo.toml").is_file() {
        nested
    } else {
        project_root.to_path_buf()
    }
}

/// Initialise `tracing` to **stderr** honouring `RUST_LOG` (default `info`).
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
