//! `cs2mcp-native`: run cs2-mcp against a user's own Steam CS2 install (Linux and Windows)
//! without containers. See `README.md` for the design (modes, overlay / manifest strategies,
//! the `-insecure` rule, the MCP config).

pub mod args;
pub mod capture;
pub mod events;
pub mod findings;
pub mod gbe;
pub mod install;
pub mod layout;
pub mod leftovers;
pub mod manifest;
pub mod mcpconfig;
#[cfg(target_os = "linux")]
pub mod overlay;
pub mod payload;
pub mod plan;
pub mod probe;
pub mod procid;
pub mod run;
pub mod selfexe;
pub mod session;
pub mod state;
pub mod tree;
pub mod vtm;
#[cfg(windows)]
pub mod win;

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};

    /// A fresh scratch dir under `<repo>/target/scratch/cs2-native/` (never /tmp: it is RAM).
    pub fn scratch(name: &str) -> PathBuf {
        let d = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/scratch/cs2-native"))
            .join(format!("{name}-{}", std::process::id()));
        if d.exists() {
            std::fs::remove_dir_all(&d).unwrap();
        }
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub fn put(p: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
}
