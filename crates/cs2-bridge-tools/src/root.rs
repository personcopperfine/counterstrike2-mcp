//! Where this repo's checkout and the project root are (the rule the shell tools had in
//! `lib-root.sh`, now `crates/cs2-native/scripts/lib-root.sh` for the remaining shell callers).
//!
//! * [`checkout`]: the checkout this binary was built from (its workspace root). Repo files
//!   (`sigs/`, `crates/`, the build's `target/`) are read from here.
//! * [`main_checkout`]: for a linked git worktree the main worktree, which holds the shared
//!   runtime state and builds; else the checkout itself (also without git, or when the main
//!   worktree is not this repo).
//! * [`project_root`]: `$CS2_MCP_PROJECT_ROOT`, else `$CS2DOCKER_ROOT` (deprecated alias), else
//!   the main checkout. The project root holds `workshop/run/app` (the Wine game tree),
//!   `workshop/run/cache` and `.podman/` (podman's `XDG_RUNTIME_DIR` and the install locks).
//!
//! Older layout: a parent directory holding this repo as `cs2-mcp/` next to `workshop/` - set
//! `CS2_MCP_PROJECT_ROOT` to that directory.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The file that marks a directory as a checkout of this repo.
const MARKER: &str = "crates/cs2-bridge/Cargo.toml";

/// The checkout this binary was built from.
pub fn checkout() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize().unwrap_or(root)
}

/// The repo's main checkout for `checkout` (see the module docs).
pub fn main_checkout(checkout: &Path) -> PathBuf {
    let common = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .stderr(std::process::Stdio::null())
        .output();
    if let Ok(out) = common {
        if out.status.success() {
            let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
            if let Some(main) = common.parent() {
                if main.join(MARKER).is_file() {
                    return main.to_path_buf();
                }
            }
        }
    }
    checkout.to_path_buf()
}

/// The project root (see the module docs).
pub fn project_root() -> PathBuf {
    for var in ["CS2_MCP_PROJECT_ROOT", "CS2DOCKER_ROOT"] {
        if let Some(value) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            return PathBuf::from(value);
        }
    }
    main_checkout(&checkout())
}

/// This repo's own files under a project root: `<root>/cs2-mcp` in the older parent layout,
/// else the root itself (the same rule as `cs2_mcp_common::mcp_repo_dir`).
pub fn repo_dir(project_root: &Path) -> PathBuf {
    let nested = project_root.join("cs2-mcp");
    if nested.join("Cargo.toml").is_file() {
        nested
    } else {
        project_root.to_path_buf()
    }
}

/// podman's `XDG_RUNTIME_DIR` under a project root.
pub fn podman_xdg(project_root: &Path) -> PathBuf {
    project_root.join(".podman/xdg")
}
