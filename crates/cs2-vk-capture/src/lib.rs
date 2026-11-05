//! `VK_LAYER_CS2MCP_capture`: a Vulkan layer that copies the swapchain image a game presents to
//! disk when a capture request appears, so client-mcp screenshots come from the game's own
//! renderer rather than from an X server grab.
//!
//! Why a Vulkan layer: CS2 renders with Vulkan both as a native Linux client (host loader, lavapipe
//! or NVIDIA) and as `cs2.exe -vulkan` under Proton, where winevulkan forwards to the host Linux
//! loader. One layer loaded by that loader sees the swapchain in both cases. The layer only uses
//! the Vulkan dispatch chain: no game addresses, no signatures.
//!
//! Protocol (file based, see `README.md`): the server writes `<dir>/request.json`; the layer takes
//! it on a later `vkQueuePresentKHR` of a capturable swapchain, copies that image, and writes
//! `<dir>/<id>.png` plus `<dir>/<id>.json` (or `{id, error}`).
//!
//! Rules: the only exported symbol is `vkNegotiateLoaderLayerInterfaceVersion`; the other entry
//! points are reached through the GIPA/GDPA it returns. No panic crosses FFI. Every failure
//! becomes `{id, error}` or a pass-through, never a hang or a failed call the game did not expect.
//! Logs go to stderr only when `CS2MCP_CAPTURE_DEBUG=1`.
#![allow(clippy::missing_safety_doc)] // the entry points are the Vulkan loader ABI

/// Debug log to stderr, only when `CS2MCP_CAPTURE_DEBUG=1`. Write errors are ignored: a layer
/// must never fail because stderr is closed.
macro_rules! dlog {
    ($($arg:tt)*) => {
        if $crate::debug::enabled() {
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "[cs2-vk-capture] {}", format_args!($($arg)*));
        }
    };
}

pub(crate) mod debug {
    use std::sync::OnceLock;

    pub fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var("CS2MCP_CAPTURE_DEBUG").is_ok_and(|v| v == "1"))
    }
}

mod capture;
mod convert;
mod dispatch;
mod layer;
mod output;
mod registry;
#[cfg(windows)]
mod winsec;
/// Deterministic frame pattern shared by the harness and the tests (not used by the layer).
pub mod testpattern;

/// Run `f`, turning a panic into an error. Used around the capture path before the GPU submit.
pub(crate) fn caught<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => Err("internal panic in the capture path".into()),
    }
}
