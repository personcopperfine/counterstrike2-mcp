//! Discovery file: `%LOCALAPPDATA%\cs2mcpbridge\session.json` inside the Wine prefix
//! (host: `<pfx>/drive_c/users/steamuser/AppData/Local/cs2mcpbridge/session.json`).
//! The token rotates per bridge load; `pid` is a Wine/container pid (hint only; hello is authoritative).
use super::errors::{BridgeError, Kind};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct Discovery {
    pub path: PathBuf,
    pub pid: Option<i64>,
    pub protocol: Option<i64>,
    pub bridge_version: Option<String>,
    pub game_build: Option<String>,
    pub transport: String,
    pub host: String,
    pub port: u16,
    pub pipe_name: Option<String>,
    pub token: String,
    pub started_at: Value,
    pub loaded_at: f64,
}

pub fn default_path(pfx: &Path) -> PathBuf {
    pfx.join("drive_c/users/steamuser/AppData/Local/cs2mcpbridge/session.json")
}

/// The discovery path of a bridge on **native Windows** (a server running on the same Windows
/// host as the game, `cs2mcp-native`): `%LOCALAPPDATA%\cs2mcpbridge\session.json`, the path the
/// bridge writes (`crates/cs2-bridge/src/ipc/discovery.rs`). `None` without `LOCALAPPDATA`.
pub fn windows_default_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(|dir| Path::new(&dir).join("cs2mcpbridge").join("session.json"))
}

/// The discovery path of a **native Linux** bridge (the dedicated-server image, where the Rust
/// bridge replaces the C DLL): `$XDG_RUNTIME_DIR/cs2mcpbridge/session.json`, falling back to
/// `$HOME/.local/share/cs2mcpbridge/session.json`.
///
/// The Linux counterpart of the C's `%LOCALAPPDATA%` rule (`mcp_bridge.c:6746-6758`);
/// `crates/cs2-bridge/src/ipc/discovery.rs` writes the same path. [`default_path`] (the
/// Wine/Proton prefix the tools load from) is unchanged, so existing host behaviour is.
pub fn linux_default_path() -> Option<PathBuf> {
    linux_path_from(
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// [`linux_default_path`] with the environment passed in, so the rule is testable without
/// touching the process environment. An empty value counts as unset.
fn linux_path_from(
    xdg_runtime_dir: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(dir) = xdg_runtime_dir.filter(|value| !value.is_empty()) {
        return Some(Path::new(dir).join("cs2mcpbridge").join("session.json"));
    }
    home.filter(|value| !value.is_empty())
        .map(|dir| Path::new(dir).join(".local/share/cs2mcpbridge/session.json"))
}

/// Best-effort pid liveness: `/proc/<pid>` on Linux. The bridge pid is usually a container/Wine
/// pid, so `Some(false)` is a hint, never proof (the POC caveat).
pub fn pid_alive(pid: Option<i64>) -> Option<bool> {
    let pid = pid?;
    if pid <= 0 {
        return None;
    }
    if cfg!(target_os = "linux") {
        Some(Path::new(&format!("/proc/{pid}")).exists())
    } else {
        None
    }
}

impl Discovery {
    pub fn load(path: &Path) -> Result<Discovery, BridgeError> {
        let data = |p: &Path| json!({"path": p.display().to_string()});
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BridgeError::new(
                    Kind::BridgeNotRunning,
                    format!(
                        "no discovery file at {} (bridge not loaded / Hammer closed)",
                        path.display()
                    ),
                )
                .with_data(data(path)))
            }
            Err(e) => {
                return Err(BridgeError::new(
                    Kind::BridgeNotRunning,
                    format!("discovery file unreadable at {}: {e}", path.display()),
                )
                .with_data(data(path)))
            }
        };
        let raw: Value = serde_json::from_str(&text).map_err(|e| {
            BridgeError::new(
                Kind::BridgeNotRunning,
                format!("discovery file unreadable at {}: {e}", path.display()),
            )
            .with_data(data(path))
        })?;
        let obj = raw.as_object().ok_or_else(|| {
            BridgeError::new(
                Kind::BridgeNotRunning,
                format!("discovery file malformed at {}", path.display()),
            )
            .with_data(data(path))
        })?;
        let s = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);
        Ok(Discovery {
            path: path.to_path_buf(),
            pid: obj.get("pid").and_then(Value::as_i64),
            protocol: obj.get("protocol").and_then(Value::as_i64),
            bridge_version: s("bridgeVersion"),
            game_build: obj.get("gameBuild").map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            }),
            transport: s("transport").unwrap_or_else(|| "tcp".into()),
            host: s("host").unwrap_or_else(|| "127.0.0.1".into()),
            port: obj.get("port").and_then(Value::as_u64).unwrap_or(0) as u16,
            pipe_name: s("pipeName"),
            token: s("token").unwrap_or_default(),
            started_at: obj.get("startedAt").cloned().unwrap_or(Value::Null),
            loaded_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
        })
    }

    /// Secret-free view (the token never leaves the client).
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path.display().to_string(), "pid": self.pid, "protocol": self.protocol,
            "bridgeVersion": self.bridge_version, "gameBuild": self.game_build,
            "transport": self.transport, "host": self.host, "port": self.port,
            "pipeName": self.pipe_name, "startedAt": self.started_at, "loadedAt": self.loaded_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{default_path, linux_path_from};
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    /// The Wine/Proton path is the one the host has always read and must not move.
    #[test]
    fn wine_prefix_path_is_unchanged() {
        assert_eq!(
            default_path(Path::new("/w/prefix")),
            PathBuf::from("/w/prefix/drive_c/users/steamuser/AppData/Local/cs2mcpbridge/session.json")
        );
    }

    /// The native-Linux rule: `$XDG_RUNTIME_DIR` first, then `$HOME/.local/share`, then none.
    #[test]
    fn linux_rule_prefers_xdg_then_home() {
        assert_eq!(
            linux_path_from(
                Some(OsStr::new("/run/user/1000")),
                Some(OsStr::new("/home/user"))
            ),
            Some(PathBuf::from("/run/user/1000/cs2mcpbridge/session.json"))
        );
        assert_eq!(
            linux_path_from(Some(OsStr::new("")), Some(OsStr::new("/home/user"))),
            Some(PathBuf::from("/home/user/.local/share/cs2mcpbridge/session.json"))
        );
        assert_eq!(
            linux_path_from(None, Some(OsStr::new("/home/user"))),
            Some(PathBuf::from("/home/user/.local/share/cs2mcpbridge/session.json"))
        );
        assert_eq!(linux_path_from(None, None), None);
    }
}
