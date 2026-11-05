//! The `mcpServers` block for the user's MCP client (README "MCP servers"). The servers attach
//! to the launcher's session through the discovery file; they are never asked to launch.

use crate::install::Os;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Which {
    Hammer,
    Client,
    Both,
}

#[derive(Debug, Clone)]
pub struct Input<'a> {
    pub os: Os,
    /// directory with `hammer-mcp[.exe]` / `client-mcp[.exe]`
    pub server_dir: &'a Path,
    pub discovery: &'a Path,
    pub console_log: &'a Path,
    /// `--app-root` (the game tree the file tools read): the install root for an in-place
    /// session; `None` for the Linux overlay (the merged tree exists only inside the namespace)
    pub app_root: Option<&'a Path>,
    /// `--project-root` (where the servers put `artifacts/`): the launcher's state dir on
    /// Windows, where the servers' built-in default is the build machine's checkout
    pub project_root: Option<&'a Path>,
    pub build: Option<&'a str>,
    /// client-mcp `--capture-dir` (`<state>/capture`) when the payload has the capture layer
    pub capture_dir: Option<&'a Path>,
    pub which: Which,
}

/// Default server dir: next to this launcher when the servers are there, else the `target/release`
/// of the repo it was built in (when that exists on this machine), else next to this launcher.
pub fn default_server_dir() -> PathBuf {
    let exe = if cfg!(windows) { "hammer-mcp.exe" } else { "hammer-mcp" };
    let here = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
    if let Some(d) = &here {
        if d.join(exe).is_file() {
            return d.clone();
        }
    }
    let repo = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/release"));
    if repo.is_dir() {
        return repo;
    }
    here.unwrap_or(repo)
}

fn exe(os: Os, dir: &Path, name: &str) -> String {
    let file = if os == Os::Windows { format!("{name}.exe") } else { name.to_string() };
    dir.join(file).display().to_string()
}

pub fn render(i: &Input<'_>) -> Value {
    let mut servers = Map::new();
    let mut common = vec!["--discovery".to_string(), i.discovery.display().to_string()];
    if let Some(b) = i.build {
        common.push("--expect-build".into());
        common.push(b.to_string());
    }
    if let Some(r) = i.app_root {
        common.push("--app-root".into());
        common.push(r.display().to_string());
    }
    if let Some(r) = i.project_root {
        common.push("--project-root".into());
        common.push(r.display().to_string());
    }
    if matches!(i.which, Which::Hammer | Which::Both) {
        servers.insert(
            "cs2-hammer".into(),
            json!({"command": exe(i.os, i.server_dir, "hammer-mcp"), "args": common}),
        );
    }
    if matches!(i.which, Which::Client | Which::Both) {
        let mut a = common.clone();
        a.push("--console-log".into());
        a.push(i.console_log.display().to_string());
        if let Some(d) = i.capture_dir {
            a.push("--capture-dir".into());
            a.push(d.display().to_string());
        }
        servers.insert(
            "cs2-client".into(),
            json!({"command": exe(i.os, i.server_dir, "client-mcp"), "args": a}),
        );
    }
    json!({ "mcpServers": servers })
}

/// Caveats printed next to the JSON (stderr).
pub fn notes(os: Os, server_dir: &Path) -> Vec<String> {
    let mut n = vec!["the servers attach to this launcher's session (no --launch); start it with `cs2mcp-native up` first".to_string()];
    if os == Os::Windows {
        n.push("on Windows the servers attach only (no launch_session/server_up; screenshot/observe read the capture layer `up` loads into the game); hammer-mcp posts its VToolMessage lines (console fallback, run_map, the first-map open) natively, which only works when hammer-mcp runs in the game's logon session (an MCP client on this desktop, not over ssh); model/particle/map compiles run as bridge jobs".into());
    }
    let probe = exe(os, server_dir, "hammer-mcp");
    if !Path::new(&probe).is_file() {
        n.push(format!("{probe} does not exist (cargo build --release -p hammer-mcp -p client-mcp, or pass --server-dir)"));
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_servers_get_the_discovery_and_the_client_its_log() {
        let i = Input {
            os: Os::Linux,
            server_dir: Path::new("/r/target/release"),
            discovery: Path::new("/run/user/1/cs2mcpbridge/session.json"),
            console_log: Path::new("/s/overlay/upper/game/csgo/console.log"),
            app_root: None,
            project_root: None,
            build: Some("2000940"),
            capture_dir: None,
            which: Which::Both,
        };
        let v = render(&i);
        assert_eq!(v["mcpServers"]["cs2-hammer"]["command"], "/r/target/release/hammer-mcp");
        assert_eq!(
            v["mcpServers"]["cs2-hammer"]["args"],
            json!(["--discovery", "/run/user/1/cs2mcpbridge/session.json", "--expect-build", "2000940"])
        );
        let a = v["mcpServers"]["cs2-client"]["args"].as_array().unwrap();
        assert_eq!(a[a.len() - 1], "/s/overlay/upper/game/csgo/console.log");
        let w = render(&Input {
            os: Os::Windows,
            which: Which::Hammer,
            app_root: Some(Path::new("C:/g")),
            project_root: Some(Path::new("C:/s/mcp")),
            ..i
        });
        assert!(w["mcpServers"]["cs2-hammer"]["command"].as_str().unwrap().ends_with("hammer-mcp.exe"));
        let wa = w["mcpServers"]["cs2-hammer"]["args"].as_array().unwrap();
        assert_eq!(wa[4..], [json!("--app-root"), json!("C:/g"), json!("--project-root"), json!("C:/s/mcp")]);
        assert!(w["mcpServers"].get("cs2-client").is_none());
        let c = render(&Input { which: Which::Client, capture_dir: Some(Path::new("C:/s/capture")), ..i });
        let ca = c["mcpServers"]["cs2-client"]["args"].as_array().unwrap();
        assert_eq!(ca[ca.len() - 2..], [json!("--capture-dir"), json!("C:/s/capture")]);
    }
}
