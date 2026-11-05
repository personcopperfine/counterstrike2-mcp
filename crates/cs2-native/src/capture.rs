//! The capture layer (`crates/cs2-vk-capture`, `VK_LAYER_CS2MCP_capture`) for the game only: the
//! supervisor names it in the game's environment as an explicit layer (no registry key, no
//! implicit layer, nothing outside the session), and client-mcp reads the frames from
//! `<state>/capture` (`--capture-dir`, which `mcp-config` passes). README "Capture".

use crate::install::Os;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const LAYER_NAME: &str = "VK_LAYER_CS2MCP_capture";
pub const MANIFEST: &str = "VkLayer_cs2mcp_capture.json";

/// What `up` records for the supervisor (`Session::capture`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capture {
    /// the layer's directory: the manifest and the library side by side
    pub layer_dir: PathBuf,
    /// `CS2MCP_CAPTURE_DIR`
    pub dir: PathBuf,
}

fn plat(os: Os) -> &'static str {
    match os {
        Os::Windows => "win64",
        Os::Linux => "linux64",
    }
}

fn library(os: Os) -> &'static str {
    match os {
        Os::Windows => "cs2_vk_capture.dll",
        Os::Linux => "libcs2_vk_capture.so",
    }
}

/// `<vklayer>/<win64|linux64>` when both the manifest and the library are there.
pub fn layer_dir(vklayer: &Path, os: Os) -> Option<PathBuf> {
    let d = vklayer.join(plat(os));
    (d.join(MANIFEST).is_file() && d.join(library(os)).is_file()).then_some(d)
}

/// Why [`layer_dir`] found nothing (for the `up` message).
pub fn missing(vklayer: &Path, os: Os) -> String {
    let d = vklayer.join(plat(os));
    format!("no capture layer in {} ({} + {}; scripts/package.sh ships it)", d.display(), MANIFEST, library(os))
}

/// The variables the game gets: the layer directory added to the loader's search path, the
/// layer enabled by name (ahead of any the user already names), and the capture dir. `add_path`
/// and `layers` are the current `VK_ADD_LAYER_PATH` / `VK_INSTANCE_LAYERS`, kept after ours.
pub fn game_env(c: &Capture, os: Os, add_path: Option<OsString>, layers: Option<OsString>) -> Vec<(&'static str, OsString)> {
    let sep = if os == Os::Windows { ";" } else { ":" };
    let join = |ours: OsString, theirs: Option<OsString>| match theirs.filter(|v| !v.is_empty()) {
        Some(t) => {
            let mut v = ours;
            v.push(sep);
            v.push(t);
            v
        }
        None => ours,
    };
    vec![
        ("VK_ADD_LAYER_PATH", join(c.layer_dir.clone().into_os_string(), add_path)),
        ("VK_INSTANCE_LAYERS", join(OsString::from(LAYER_NAME), layers)),
        ("CS2MCP_CAPTURE_DIR", c.dir.clone().into_os_string()),
    ]
}

/// Create the capture dir before the game starts (the layer never creates it). Unix: 0700, the
/// layer refuses anything else; Windows: the state dir's ACL (under `%LOCALAPPDATA%`, private).
pub fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layer_goes_first_and_the_users_values_stay() {
        let c = Capture { layer_dir: PathBuf::from("C:/p/vklayer/win64"), dir: PathBuf::from("C:/s/capture") };
        let env = game_env(&c, Os::Windows, Some("D:/mine".into()), Some("VK_LAYER_KHRONOS_validation".into()));
        assert_eq!(env[0], ("VK_ADD_LAYER_PATH", OsString::from("C:/p/vklayer/win64;D:/mine")));
        assert_eq!(env[1], ("VK_INSTANCE_LAYERS", OsString::from("VK_LAYER_CS2MCP_capture;VK_LAYER_KHRONOS_validation")));
        assert_eq!(env[2], ("CS2MCP_CAPTURE_DIR", OsString::from("C:/s/capture")));
        let bare = game_env(&c, Os::Linux, None, Some("".into()));
        assert_eq!(bare[0].1, OsString::from("C:/p/vklayer/win64"));
        assert_eq!(bare[1].1, OsString::from(LAYER_NAME));
    }

    #[test]
    fn a_layer_dir_needs_the_manifest_and_the_library() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch/cs2-native-vklayer");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("win64")).unwrap();
        std::fs::write(root.join("win64").join(MANIFEST), "{}").unwrap();
        assert_eq!(layer_dir(&root, Os::Windows), None);
        std::fs::write(root.join("win64/cs2_vk_capture.dll"), "").unwrap();
        assert_eq!(layer_dir(&root, Os::Windows), Some(root.join("win64")));
        assert_eq!(layer_dir(&root, Os::Linux), None);
    }
}
