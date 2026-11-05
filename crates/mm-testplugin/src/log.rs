//! The test plugin's log.
//!
//! One prefixed line per event, opened/appended/closed each time - the same shape as the
//! bridge's log (`crates/cs2-bridge/src/log.rs`, whose `logline` model is the C's
//! `mcp_bridge.c:198`): the game can be killed at any moment, so a buffered handle would lose
//! the last lines that prove the hook fired. This file is a deliberate small copy rather than
//! a dependency: a fixture plugin must depend on `s2sdk` only, and the path rule is
//! plugin-specific.
//!
//! Every line is prefixed `[mm-testplugin] ` - the tag Metamod shows for this plugin's own
//! log lines (`GetLogTag`, `plugin.rs`) - so the orchestrator's live test can grep the file
//! for this plugin's events alone.
//!
//! Writing the log is the only silent failure in this crate: a log line that cannot be written
//! must not disturb the game. Everything else fails loudly (the tests, `check-exports.sh`).

use std::path::{Path, PathBuf};

/// Append one line to the plugin log (`CS2MCP_TESTPLUGIN_LOG`, else the platform default).
///
/// Silent by design: see the module docs.
pub fn line(msg: &str) {
    let Some(path) = default_path() else {
        return;
    };
    let _ = append(&path, &format!("[mm-testplugin] {msg}\n"));
}

/// The log path for this process: `CS2MCP_TESTPLUGIN_LOG` if set, else the platform default.
fn default_path() -> Option<PathBuf> {
    resolve(
        std::env::var("CS2MCP_TESTPLUGIN_LOG").ok().as_deref(),
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
    )
}

/// The path rule, split from the environment so it is testable without touching the process
/// environment (and without racing the other tests).
///
/// * `CS2MCP_TESTPLUGIN_LOG` wins when set (and non-empty) - the switch the orchestrator and
///   the tests use to point this plugin somewhere writable.
/// * Windows: `Z:\app\mm_testplugin.log`, next to the bridge's own rule (`Z:\app\`,
///   `mcp_bridge.c:198`; the Rust bridge's log lives there too, under its own file name).
/// * Linux: `$XDG_RUNTIME_DIR/cs2mcpbridge/mm_testplugin.log`, next to the bridge's log in the
///   same directory; unset `XDG_RUNTIME_DIR` means no log (there is no safe default: the
///   working directory of a game process is not writable state this plugin may create).
fn resolve(
    cs2mcp_testplugin_log: Option<&str>,
    xdg_runtime_dir: Option<&str>,
) -> Option<PathBuf> {
    if let Some(p) = cs2mcp_testplugin_log.filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    if cfg!(windows) {
        return Some(PathBuf::from(r"Z:\app\mm_testplugin.log"));
    }
    xdg_runtime_dir
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join("cs2mcpbridge").join("mm_testplugin.log"))
}

/// Append `line` (already prefixed and terminated) to the log. The caller decides what an
/// error means; here it is always "the log line is lost".
fn append(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    if let Some(dir) = path.parent() {
        // A missing log directory is created; a failure to do so surfaces as the open error.
        let _ = create_log_dir(dir);
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())
}

/// Creates the log's directory, privately on Unix.
///
/// On Linux the log lives in the same `$XDG_RUNTIME_DIR/cs2mcpbridge` directory as the
/// bridge's, which carries the session token: the discovery writer refuses to start when that
/// directory grants group or other access, and a log is usually the first writer to create
/// it. Creating it `0700` keeps the two in agreement instead of letting this log make the
/// discovery path fail closed. Failure is ignored here like everywhere else in this module: a
/// lost log line must not disturb the game.
#[cfg(unix)]
fn create_log_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// [`create_log_dir`] on Windows, where the `Z:\app` directory is used and no mode exists.
#[cfg(not(unix))]
fn create_log_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cs2mcp_testplugin_log_overrides_both_defaults() {
        assert_eq!(
            resolve(Some("/scratch/testplugin.log"), Some("/run/user/1000")),
            Some(PathBuf::from("/scratch/testplugin.log"))
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_default_is_next_to_the_bridge_log() {
        assert_eq!(
            resolve(None, Some("/run/user/1000")),
            Some(PathBuf::from("/run/user/1000/cs2mcpbridge/mm_testplugin.log"))
        );
        // An empty env value is not a path: the next rule applies.
        assert_eq!(
            resolve(Some(""), Some("/run/user/1000")),
            Some(PathBuf::from("/run/user/1000/cs2mcpbridge/mm_testplugin.log"))
        );
        // No XDG_RUNTIME_DIR: no path at all (logged nowhere), never a guess.
        assert_eq!(resolve(None, None), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_default_is_the_app_drive() {
        assert_eq!(resolve(None, None), Some(PathBuf::from(r"Z:\app\mm_testplugin.log")));
    }
}
