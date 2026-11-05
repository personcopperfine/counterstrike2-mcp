//! Vision: get pixels to the model. Sources:
//! 1. bridge offscreen renders (`world.render`) -> PNG on disk, no display needed [proven];
//! 2. game screenshots via console/test script (`screenshot`, `png_screenshot`; R/B swapped PNGs) [proven];
//! 3. frames read back from the game's swapchain by our Vulkan layer (`cs2-vk-capture`, protocol
//!    client in `vklayer.rs`). X server grabs (Xvfb/XWD, xdump, host-X screenshots) are
//!    deliberately not a source.
//!
//! OCR: `ocr.rs` (tesseract CLI first).
//!
//! Decoding and policy: `frame.rs` (RGB buffer, stats, R/B swap, downscale, PNG I/O, inline
//! policy).
pub mod frame;
pub mod ocr;
pub mod vklayer;
#[cfg(windows)]
mod winsec;

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum Source {
    BridgeRender,
    GameScreenshot,
    /// frames read back from the game's swapchain by our Vulkan layer (planned)
    VulkanLayer,
}

#[derive(Debug, Clone)]
pub struct Capture {
    pub source: Source,
    pub png: PathBuf,
    pub width: u32,
    pub height: u32,
    /// inline into the MCP result as an `image` block (base64) when below the size cap
    pub inline: bool,
}

/// Inline cap carried over from the POC (`render_world` inlines up to 1920x1080).
pub const INLINE_MAX_PIXELS: u64 = 1920 * 1080;
