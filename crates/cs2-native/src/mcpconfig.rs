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

/// How the MCP client talks to the servers. Only stdio exists today: the client spawns the
/// server process itself and speaks over its stdin/stdout. HTTP is planned (a server the
/// launcher would run, clients connect to a URL); it is listed so the UI can show it, disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Transport {
    #[default]
    Stdio,
    Http,
}

impl Transport {
    pub fn available(self) -> bool {
        self == Transport::Stdio
    }
    pub fn label(self) -> &'static str {
        match self {
            Transport::Stdio => "stdio (the MCP client starts the servers)",
            Transport::Http => "HTTP (coming)",
        }
    }
}

/// How the servers are started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Servers {
    /// `<dir>/hammer-mcp[.exe]`, `<dir>/client-mcp[.exe]`
    Dir(PathBuf),
    /// a multicall AppImage: `<appimage> hammer-mcp ...` (its path is stable; the mount is not)
    Multicall(PathBuf),
}

impl Servers {
    /// The command and the arguments before the server's own for `name`.
    pub fn invocation(&self, os: Os, name: &str) -> (String, Vec<String>) {
        match self {
            Servers::Dir(d) => (exe(os, d, name), Vec::new()),
            Servers::Multicall(img) => (img.display().to_string(), vec![name.to_string()]),
        }
    }
    /// Does the server binary exist?
    pub fn present(&self, os: Os) -> bool {
        match self {
            Servers::Dir(d) => Path::new(&exe(os, d, "hammer-mcp")).is_file(),
            Servers::Multicall(img) => img.is_file(),
        }
    }
}

/// The servers of this launcher: the AppImage itself when run from one, else
/// [`default_server_dir`].
pub fn default_servers() -> Servers {
    match crate::selfexe::appimage() {
        Some((_, image)) => Servers::Multicall(image),
        None => Servers::Dir(default_server_dir()),
    }
}

#[derive(Debug, Clone)]
pub struct Input<'a> {
    pub os: Os,
    /// where `hammer-mcp[.exe]` / `client-mcp[.exe]` come from
    pub servers: &'a Servers,
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

/// Default server dir: the package dir when the servers are there (next to this launcher, or
/// `$APPDIR/opt/cs2mcp`), else the `target/release` of the repo it was built in (when that exists
/// on this machine), else next to this launcher.
pub fn default_server_dir() -> PathBuf {
    let exe = if cfg!(windows) { "hammer-mcp.exe" } else { "hammer-mcp" };
    let here = crate::selfexe::package_dir().or_else(|| std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)));
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
    let entry = |name: &str, args: Vec<String>| {
        let (command, mut a) = i.servers.invocation(i.os, name);
        a.extend(args);
        json!({"command": command, "args": a})
    };
    if matches!(i.which, Which::Hammer | Which::Both) {
        servers.insert("cs2-hammer".into(), entry("hammer-mcp", common.clone()));
    }
    if matches!(i.which, Which::Client | Which::Both) {
        let mut a = common.clone();
        a.push("--console-log".into());
        a.push(i.console_log.display().to_string());
        if let Some(d) = i.capture_dir {
            a.push("--capture-dir".into());
            a.push(d.display().to_string());
        }
        servers.insert("cs2-client".into(), entry("client-mcp", a));
    }
    json!({ "mcpServers": servers })
}

/// Caveats printed next to the JSON (stderr).
pub fn notes(os: Os, servers: &Servers) -> Vec<String> {
    let mut n = vec![
        "the servers attach to this launcher's session (no --launch); start it with `cs2mcp-native up` first".to_string(),
        "stdio servers are started by the MCP client (Claude Desktop, Claude Code, ...) from this config, not by the launcher".to_string(),
    ];
    if os == Os::Windows {
        n.push("on Windows the servers attach only (no launch_session/server_up; screenshot/observe read the capture layer `up` loads into the game); hammer-mcp posts its VToolMessage lines (console fallback, run_map, the first-map open) natively, which only works when hammer-mcp runs in the game's logon session (an MCP client on this desktop, not over ssh); model/particle/map compiles run as bridge jobs".into());
    }
    if !servers.present(os) {
        let (probe, _) = servers.invocation(os, "hammer-mcp");
        n.push(format!("{probe} does not exist (cargo build --release -p hammer-mcp -p client-mcp, or pass --server-dir)"));
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_servers_get_the_discovery_and_the_client_its_log() {
        let dir = Servers::Dir(PathBuf::from("/r/target/release"));
        let i = Input {
            os: Os::Linux,
            servers: &dir,
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
        // the AppImage: its own stable path, the server named first
        let img = Servers::Multicall(PathBuf::from("/home/u/Apps/cs2-mcp.AppImage"));
        let m = render(&Input { servers: &img, ..i });
        assert_eq!(m["mcpServers"]["cs2-hammer"]["command"], "/home/u/Apps/cs2-mcp.AppImage");
        assert_eq!(m["mcpServers"]["cs2-hammer"]["args"][0], "hammer-mcp");
        assert_eq!(m["mcpServers"]["cs2-client"]["args"][0], "client-mcp");
        assert_eq!(m["mcpServers"]["cs2-client"]["args"][1], "--discovery");
        assert!(Transport::Stdio.available() && !Transport::Http.available());
    }
}
