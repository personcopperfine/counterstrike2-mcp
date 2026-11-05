//! VToolMessage: the `vtmsend.exe` port lives in `crates/cs2-vtm` (shared with hammer-mcp on a
//! Windows host); this module is the launcher's view of it.

pub use cs2_vtm::{bridge_request, channel_up, command, open_asset, payload};

/// Post `text`; `Err` (the reason as text) when the channel is not there or refused it.
pub fn post(text: &str) -> Result<u64, String> {
    cs2_vtm::post(text).map_err(|e| e.to_string())
}
