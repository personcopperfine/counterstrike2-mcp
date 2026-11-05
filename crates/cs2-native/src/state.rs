//! The launcher's own state, outside the game tree: `%LOCALAPPDATA%\cs2mcp-native\<id>\` /
//! `$XDG_STATE_HOME/cs2mcp-native/<id>/` (README "Windows ... backup + restore").

use crate::install::Os;
use crate::manifest::{under, write_atomic, Store};
use crate::plan::SteamMode;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The state root: `--state-dir`, else `CS2MCP_NATIVE_STATE`, else the platform default.
pub fn state_root(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os("CS2MCP_NATIVE_STATE").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()).context("LOCALAPPDATA is not set")?;
        Ok(PathBuf::from(base).join("cs2mcp-native"))
    }
    #[cfg(not(windows))]
    {
        if let Some(x) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(x).join("cs2mcp-native"));
        }
        let home = std::env::var_os("HOME").filter(|v| !v.is_empty()).context("HOME is not set")?;
        Ok(PathBuf::from(home).join(".local/state/cs2mcp-native"))
    }
}

/// First 16 hex digits of the SHA-256 of the canonical install root (lower-cased on Windows,
/// where paths are case-insensitive).
pub fn install_id(root: &Path) -> String {
    let c = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut s = c.to_string_lossy().into_owned();
    if cfg!(windows) {
        s = s.to_lowercase();
    }
    crate::manifest::hex(&Sha256::digest(s.as_bytes()))[..16].to_string()
}

/// How the files reach the game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Linux: overlay on Linux, in-place on Windows
    Auto,
    /// a private user+mount namespace with overlayfs over the install (Linux)
    Overlay,
    /// write into the tree with a manifest + backups, restored on down/uninstall
    InPlace,
}

impl Strategy {
    pub fn resolve(self, os: Os) -> anyhow::Result<Strategy> {
        match (self, os) {
            (Strategy::Auto, Os::Linux) => Ok(Strategy::Overlay),
            (Strategy::Auto, Os::Windows) => Ok(Strategy::InPlace),
            (Strategy::Overlay, Os::Windows) => {
                anyhow::bail!("--strategy overlay is Linux-only (Windows has no unprivileged overlay); use in-place")
            }
            (s, _) => Ok(s),
        }
    }
}

/// A held [`Dirs::lock`]; released on drop (or when the process ends).
#[derive(Debug)]
pub struct StateLock {
    _file: std::fs::File,
}

/// The per-install state directory.
#[derive(Debug, Clone)]
pub struct Dirs {
    pub dir: PathBuf,
}

impl Dirs {
    pub fn new(state_root: &Path, install_root: &Path) -> Dirs {
        Dirs { dir: state_root.join(install_id(install_root)) }
    }
    pub fn store(&self) -> Store {
        Store { dir: self.dir.clone() }
    }
    pub fn logs(&self) -> PathBuf {
        self.dir.join("logs")
    }
    pub fn session_file(&self) -> PathBuf {
        self.dir.join("session.json")
    }
    /// `<state>/lock`, see [`Dirs::lock`]
    pub fn lock_file(&self) -> PathBuf {
        self.dir.join("lock")
    }
    /// Take the install's state lock, polling until `timeout` (FIXME 6). `prepare`, `up` (until
    /// the supervisor has recorded its pid and the launcher's), `down` and `uninstall` hold it,
    /// so a `down` can never read a session that has no pids yet and restore under a starting
    /// game. The supervisor does
    /// not take it (`down` holds it while it waits for the supervisor's restore). An OS file lock
    /// (`flock` / `LockFileEx`): it goes away with the process that held it, so a crash leaves no
    /// stale lock; the file itself stays.
    pub fn lock(&self, timeout: Duration) -> anyhow::Result<StateLock> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.lock_file();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let until = Instant::now() + timeout;
        let mut said = false;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(StateLock { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < until => {
                    if !said {
                        said = true;
                        eprintln!("[cs2mcp-native] waiting for {} (another cs2mcp-native command holds it)", path.display());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
                    "another cs2mcp-native command (up, prepare, down or uninstall) still holds {} after {} s; wait for it to finish",
                    path.display(),
                    timeout.as_secs()
                ),
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("lock {}", path.display()))
                }
            }
        }
    }
    /// the capture layer's `CS2MCP_CAPTURE_DIR` (client-mcp `--capture-dir`)
    pub fn capture(&self) -> PathBuf {
        self.dir.join("capture")
    }
    pub fn upper(&self) -> PathBuf {
        self.dir.join("overlay/upper")
    }
    pub fn work(&self) -> PathBuf {
        self.dir.join("overlay/work")
    }
    pub fn gbe_saves(&self) -> PathBuf {
        self.dir.join("gbe-saves")
    }
    pub fn launch_log(&self) -> PathBuf {
        self.logs().join("launch.log")
    }
    pub fn bridge_log(&self) -> PathBuf {
        self.logs().join("bridge.log")
    }
    /// where the session's `console.log` is written
    pub fn console_log(&self, install_root: &Path, strategy: Strategy) -> PathBuf {
        match strategy {
            Strategy::Overlay => under(&self.upper(), "game/csgo/console.log"),
            _ => under(install_root, "game/csgo/console.log"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// supervisor started, game not yet
    Starting,
    Running,
    /// the game exited; the supervisor is restoring
    Exited,
    /// finished (tree restored when in-place)
    Done,
    /// the launch failed before the game ran
    Failed,
}

pub const SESSION_FORMAT: &str = "cs2mcp-native/session/1";

/// `session.json`: written by `up` (before the supervisor starts) and by the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub format: String,
    pub install_root: PathBuf,
    pub os: Os,
    pub strategy: Strategy,
    pub steam: SteamMode,
    pub tools: bool,
    pub map: Option<String>,
    /// the game arguments (after the program)
    pub args: Vec<String>,
    /// unix seconds
    pub started_at: f64,
    pub supervisor_pid: Option<u32>,
    /// the launcher (Windows `cs2mcp_launch.exe`, Linux `cs2.sh`) and the game
    pub game_pids: Vec<u32>,
    pub phase: Phase,
    pub exit_code: Option<i32>,
    /// keep the tree prepared after the game exits (in-place only)
    pub keep_installed: bool,
    pub discovery: Option<PathBuf>,
    pub console_log: PathBuf,
    /// the supervisor's restore result, when it ran one
    pub restore: Option<serde_json::Value>,
    pub error: Option<String>,
    /// Windows: the supervisor stayed in `up`'s job (breakaway refused, FIXME 10), so closing
    /// that job kills it and the game before the restore
    #[serde(default)]
    pub in_parent_job: bool,
    /// the capture layer the supervisor names in the game's environment (`None`: no layer, or
    /// `up --no-capture`)
    #[serde(default)]
    pub capture: Option<crate::capture::Capture>,
}

impl Session {
    pub fn load(path: &Path) -> anyhow::Result<Option<Session>> {
        match std::fs::read(path) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b).with_context(|| format!("parse {}", path.display()))?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }
}

pub fn now_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_lock_is_exclusive_and_released_on_drop() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch/cs2-native-lock");
        let _ = std::fs::remove_dir_all(&dir);
        let d = Dirs { dir };
        let held = d.lock(Duration::from_millis(0)).expect("first lock");
        let e = d.lock(Duration::from_millis(300)).expect_err("second lock while held");
        assert!(e.to_string().contains("still holds"), "{e}");
        drop(held);
        d.lock(Duration::from_millis(0)).expect("free again after drop");
    }
}
