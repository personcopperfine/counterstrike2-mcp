//! Client for the in-process bridge (`mcp_bridge.c`), protocol
//! `../idabins/docs/data/bridge-protocol.md` (bridge m2.8, protocol 1).
//!
//! Port of the POC `bridge_client.py`: discovery -> connect -> `control.hello` -> request/response
//! with explicit `deadlineMs`, capability gating, liveness (modal) gate, and the error taxonomy.
//! NDJSON, one JSON object per line, max line = advertised `limits.maxLineBytes` (1 MiB in m2.8).

pub mod client;
pub mod discovery;
pub mod errors;
pub mod protocol;

pub use client::{BridgeClient, ClientOptions, Liveness, RequestOpts, SharedBridge};
pub use discovery::Discovery;
pub use errors::{BridgeError, Kind};
pub use protocol::PROTOCOL;
