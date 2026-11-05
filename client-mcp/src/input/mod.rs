//! usercmd-level input through the bridge hook (`input.set/clear/status/uninstall`). The types now
//! live in `cs2-mcp-common` (`play::input`), shared with hammer-mcp.
pub use cs2_mcp_common::play::input::InputSet;
