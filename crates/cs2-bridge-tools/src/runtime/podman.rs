//! podman: the program, its `XDG_RUNTIME_DIR`, and the argv of every call the runtime makes.
//!
//! The argv builders are pure so the container's shape is testable without podman. That shape
//! is an interface, not a detail: client-mcp and hammer-mcp find the tools session by the
//! `cs2mcp.tool` label and read its addon from the `cs2mcp.addon` label or the `-addon` word
//! of the container command (`crates/cs2-mcp-common/src/runtime/docker.rs`
//! `inspect_addon_argv` / `parse_container_addon`, `container_by_label`).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The label naming the tool a container runs (`hammer` | `modeldoc`).
pub const TOOL_LABEL: &str = "cs2mcp.tool";
/// The label carrying the `-addon` the container was started with.
pub const ADDON_LABEL: &str = "cs2mcp.addon";

/// The tool a tools container runs; the `cs2mcp.tool` label's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Hammer,
    Modeldoc,
}

impl Tool {
    /// `TOOL`'s value; the script's wording on a bad one.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        match text {
            "hammer" => Ok(Tool::Hammer),
            "modeldoc" => Ok(Tool::Modeldoc),
            other => anyhow::bail!("TOOL must be hammer or modeldoc (got '{other}')"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Hammer => "hammer",
            Tool::Modeldoc => "modeldoc",
        }
    }
}

/// The podman program and the runtime dir every call gets (AGENTS.md rule 7: the project's
/// podman state lives in `<project root>/.podman/xdg`, and the ModelDoc check must see it).
#[derive(Debug, Clone)]
pub struct Podman {
    pub program: OsString,
    pub xdg: PathBuf,
}

impl Podman {
    pub fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(args)
            .env("XDG_RUNTIME_DIR", &self.xdg)
            .stdin(Stdio::null());
        cmd
    }

    /// Runs and captures stdout + stderr.
    pub fn output(&self, args: &[String]) -> io::Result<Output> {
        self.command(args).output()
    }

    /// The `cs2mcp.tool` label of container `name`; empty when it is unlabelled, absent, or
    /// podman fails (the script's `|| true`: "not a ModelDoc container").
    pub fn container_tool(&self, name: &str) -> String {
        match self.output(&inspect_tool_argv(name)) {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
            _ => String::new(),
        }
    }
}

fn s(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

pub fn inspect_tool_argv(name: &str) -> Vec<String> {
    s(&[
        "inspect",
        "--format",
        &format!("{{{{ index .Config.Labels \"{TOOL_LABEL}\" }}}}"),
        name,
    ])
}

/// The names of the running containers carrying `label` (`key` or `key=value`).
pub fn ps_names_argv(label: &str) -> Vec<String> {
    s(&[
        "ps",
        "--filter",
        &format!("label={label}"),
        "--format",
        "{{.Names}}",
    ])
}

pub fn ps_status_argv(name: &str) -> Vec<String> {
    s(&[
        "ps",
        "--filter",
        &format!("name=^{name}$"),
        "--format",
        "{{.Status}}",
    ])
}

pub fn rm_argv(name: &str) -> Vec<String> {
    s(&["rm", "-f", name])
}

/// `vtmsend.exe` inside the session: it posts `/app/vtm_payload.kv3` (a fixed path; the
/// program takes no argument) and prints `creator version` once the channel exists.
pub fn vtmsend_argv(name: &str) -> Vec<String> {
    s(&[
        "exec",
        "-i",
        name,
        "bash",
        "-c",
        "cd /app && ./entrypoint.sh vtmsend.exe 2>&1",
    ])
}

/// `EXTRA_ARGS` with `-insecure` appended (Metamod:Source refuses to load without it), or
/// `None` when it already carries the word.
pub fn with_insecure(extra: &str) -> Option<String> {
    if extra.split_whitespace().any(|w| w == "-insecure") {
        None
    } else if extra.is_empty() {
        Some("-insecure".into())
    } else {
        Some(format!("{extra} -insecure"))
    }
}

/// The container's command (`bash -c`). PID 1 is the launcher `cs2mcp_launch.exe`
/// (crates/mm-launch), which starts cs2.exe suspended, injects the Metamod loader, resumes it
/// and exits with the game's code. `extra` (already holding `-insecure`) goes in verbatim, so
/// the container shell word-splits it as the script did. The two `+` convars keep the engine
/// rendering and ticking without window focus; without them the VToolMessage channel never
/// comes up.
pub fn launch_command(addon: Option<&str>, extra: &str) -> String {
    let addon = addon.map(|a| format!("-addon {a}")).unwrap_or_default();
    format!(
        "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console -condebug {addon} {extra} \
         +engine_no_focus_sleep 0 +r_always_render_all_windows 1"
    )
}

/// What `up` starts.
pub struct Launch<'a> {
    pub container: &'a str,
    pub image: &'a str,
    pub tool: Tool,
    /// `None`: no `-addon` and no addon label.
    pub addon: Option<&'a str>,
    /// `EXTRA_ARGS` after [`with_insecure`].
    pub extra_args: &'a str,
    /// `-e NAME=value` pairs (the test plugin's switches, only those that are set).
    pub env: &'a [(&'static str, String)],
    pub app: &'a Path,
    pub cache: &'a Path,
    pub xauth: &'a Path,
}

pub fn run_argv(l: &Launch<'_>) -> Vec<String> {
    let mut a = s(&[
        "run",
        "-d",
        "--gpus",
        "all",
        "--name",
        l.container,
        "--network=host",
    ]);
    a.extend([
        "--label".into(),
        format!("{TOOL_LABEL}={}", l.tool.as_str()),
    ]);
    if let Some(addon) = l.addon {
        a.extend(["--label".into(), format!("{ADDON_LABEL}={addon}")]);
    }
    for (name, value) in l.env {
        a.extend(["-e".into(), format!("{name}={value}")]);
    }
    a.extend([
        "-v".into(),
        format!("{}:/app", l.app.display()),
        "-v".into(),
        format!("{}:/cache", l.cache.display()),
        "-v".into(),
        "/tmp/.X11-unix:/tmp/.X11-unix".into(),
        "-v".into(),
        format!("{}:/root/.Xauthority:ro", l.xauth.display()),
    ]);
    a.extend(s(&[
        "-e",
        "DISPLAY=:0",
        "-e",
        "XAUTHORITY=/root/.Xauthority",
        "--entrypoint",
        "/bin/bash",
    ]));
    a.extend([
        l.image.to_string(),
        "-c".into(),
        launch_command(l.addon, l.extra_args),
    ]);
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insecure_is_added_once() {
        assert_eq!(with_insecure("").as_deref(), Some("-insecure"));
        assert_eq!(with_insecure("-novid").as_deref(), Some("-novid -insecure"));
        assert_eq!(with_insecure("-novid -insecure +x 1"), None);
        assert_eq!(with_insecure("-insecure"), None);
        // a word that only contains it is not it
        assert_eq!(
            with_insecure("-insecurex").as_deref(),
            Some("-insecurex -insecure")
        );
    }

    #[test]
    fn launch_command_matches_the_script() {
        assert_eq!(
            launch_command(Some("mcp_test"), "-insecure"),
            "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console -condebug -addon mcp_test \
             -insecure +engine_no_focus_sleep 0 +r_always_render_all_windows 1"
        );
        // ADDON='': the script's empty `${addon_arg[*]:-}` leaves a double space
        assert_eq!(
            launch_command(None, "-insecure"),
            "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console -condebug  \
             -insecure +engine_no_focus_sleep 0 +r_always_render_all_windows 1"
        );
    }

    #[test]
    fn run_argv_shape() {
        let env = [("CS2MCP_TESTPLUGIN_MODE", "coexist".to_string())];
        let argv = run_argv(&Launch {
            container: "c1",
            image: "img:1",
            tool: Tool::Modeldoc,
            addon: Some("a1"),
            extra_args: "-insecure",
            env: &env,
            app: Path::new("/p/workshop/run/app"),
            cache: Path::new("/p/workshop/run/cache"),
            xauth: Path::new("/x/auth"),
        });
        #[rustfmt::skip]
        let want = [
            "run", "-d", "--gpus", "all", "--name", "c1", "--network=host",
            "--label", "cs2mcp.tool=modeldoc", "--label", "cs2mcp.addon=a1",
            "-e", "CS2MCP_TESTPLUGIN_MODE=coexist",
            "-v", "/p/workshop/run/app:/app", "-v", "/p/workshop/run/cache:/cache",
            "-v", "/tmp/.X11-unix:/tmp/.X11-unix", "-v", "/x/auth:/root/.Xauthority:ro",
            "-e", "DISPLAY=:0", "-e", "XAUTHORITY=/root/.Xauthority", "--entrypoint", "/bin/bash",
            "img:1", "-c",
        ];
        assert_eq!(&argv[..want.len()], &want[..]);
        assert_eq!(argv.len(), want.len() + 1);
        assert!(argv
            .last()
            .unwrap()
            .contains(" -addon a1 -insecure +engine_no_focus_sleep 0"));
    }

    #[test]
    fn tool_parse() {
        assert_eq!(Tool::parse("modeldoc").unwrap(), Tool::Modeldoc);
        assert_eq!(
            Tool::parse("pet").unwrap_err().to_string(),
            "TOOL must be hammer or modeldoc (got 'pet')"
        );
    }

    #[test]
    fn inspect_format() {
        assert_eq!(
            inspect_tool_argv("c")[2],
            "{{ index .Config.Labels \"cs2mcp.tool\" }}"
        );
    }
}
