//! A native error dialog for the CLI, for the case where a person started it from a desktop and
//! may not be reading its output: `prepare` / `up` / `wait` / `down` / `uninstall` / `open`
//! failed or stopped on a finding, so the game install is not in the state they asked for. The
//! JSON result on stdout is the answer either way; the dialog only repeats it.
//!
//! When one is shown is decided by [`choose`], from [`Facts`] gathered once ([`Facts::gather`]):
//!
//! * never with `--no-dialog` or `CS2MCP_NO_DIALOG` (the GUI passes `--no-dialog`: it shows its
//!   own), never in a container, never for `run` / `supervise` (the caller decides that);
//! * Linux: only when `DISPLAY` or `WAYLAND_DISPLAY` is set; zenity, else kdialog, else none;
//! * Windows: `MessageBoxW`, only in an interactive desktop session: not over ssh, not in
//!   session 0 (services), and only when this process's window station is visible.
//!
//! The dialog never blocks the CLI: it is a detached child (zenity / kdialog, or
//! `cs2mcp-native show-dialog` on Windows) with no handle of ours, so a caller reading our stdout
//! to EOF is not held up by an open dialog.

use crate::findings::Finding;
use crate::install::Os;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Zenity,
    Kdialog,
    MessageBox,
}

/// What the decision depends on. Gathered by [`Facts::gather`]; the tests build it by hand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    pub os: Option<Os>,
    /// `--no-dialog` or `CS2MCP_NO_DIALOG`
    pub disabled: bool,
    /// a container (`/.dockerenv`, `/run/.containerenv`, `$container`)
    pub container: bool,
    /// `DISPLAY` or `WAYLAND_DISPLAY` is set and not empty
    pub display: bool,
    /// `SSH_CONNECTION` / `SSH_CLIENT` / `SSH_TTY` is set
    pub ssh: bool,
    pub zenity: bool,
    pub kdialog: bool,
    /// Windows: the process's session id (0: the services' session)
    pub session_id: Option<u32>,
    /// Windows: this process's window station is visible (`WSF_VISIBLE`)
    pub visible_station: bool,
}

/// The tool to show the dialog with, or `None` (no dialog).
pub fn choose(f: &Facts) -> Option<Tool> {
    if f.disabled || f.container {
        return None;
    }
    match f.os? {
        Os::Linux => {
            if !f.display {
                None
            } else if f.zenity {
                Some(Tool::Zenity)
            } else if f.kdialog {
                Some(Tool::Kdialog)
            } else {
                None
            }
        }
        Os::Windows => {
            let interactive = !f.ssh && f.session_id.is_some_and(|s| s != 0) && f.visible_station;
            interactive.then_some(Tool::MessageBox)
        }
    }
}

/// `CS2MCP_NO_DIALOG`: set and not empty / `0` / `false`.
pub fn env_disables(v: Option<&str>) -> bool {
    v.is_some_and(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false" | "no"))
}

fn set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

fn on_path(exe: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
}

impl Facts {
    pub fn gather(no_dialog: bool) -> Facts {
        let os = Os::host();
        let disabled = no_dialog || env_disables(std::env::var("CS2MCP_NO_DIALOG").ok().as_deref());
        let container = set("container") || Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists();
        let mut f = Facts {
            os: Some(os),
            disabled,
            container: os == Os::Linux && container,
            display: set("DISPLAY") || set("WAYLAND_DISPLAY"),
            ssh: set("SSH_CONNECTION") || set("SSH_CLIENT") || set("SSH_TTY"),
            ..Facts::default()
        };
        if disabled || f.container {
            return f;
        }
        match os {
            Os::Linux => {
                f.zenity = on_path("zenity");
                f.kdialog = on_path("kdialog");
            }
            Os::Windows => {
                #[cfg(windows)]
                {
                    f.session_id = crate::win::session_id();
                    f.visible_station = crate::win::window_station_visible();
                }
            }
        }
        f
    }
}

/// The dialog's text: the error, then each finding with its paths (a few) and advice.
pub fn message(error: &str, findings: &[Finding]) -> String {
    let mut s = error.trim().to_string();
    for f in findings {
        s.push_str(&format!("\n\n• {}", f.title));
        for p in f.paths.iter().take(6) {
            s.push_str(&format!("\n    {p}"));
        }
        if f.paths.len() > 6 {
            s.push_str(&format!("\n    … and {} more", f.paths.len() - 6));
        }
        for a in &f.advice {
            s.push_str(&format!("\n  → {a}"));
        }
    }
    s
}

pub const TITLE: &str = "cs2-mcp";

/// The command line of `tool` (Linux tools; the Windows one runs this binary's `show-dialog`).
pub fn argv(tool: Tool, title: &str, text: &str) -> Vec<String> {
    match tool {
        Tool::Zenity => vec!["zenity".into(), "--error".into(), "--no-markup".into(), format!("--title={title}"), format!("--text={text}")],
        Tool::Kdialog => vec!["kdialog".into(), "--title".into(), title.into(), "--error".into(), text.into()],
        Tool::MessageBox => vec!["show-dialog".into(), "--title".into(), title.into(), "--text".into(), text.into()],
    }
}

/// Show the dialog without waiting for it. Errors are ignored: the dialog is a courtesy, the JSON
/// result already carries the error.
pub fn show(tool: Tool, title: &str, text: &str) {
    use std::process::{Command, Stdio};
    let a = argv(tool, title, text);
    let mut cmd = match tool {
        Tool::MessageBox => {
            let Ok(exe) = std::env::current_exe() else { return };
            let mut c = Command::new(exe);
            c.args(&a);
            c
        }
        _ => {
            let mut c = Command::new(&a[0]);
            c.args(&a[1..]);
            crate::selfexe::scrub_appimage_env(&mut c);
            c
        }
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        crate::win::no_inherit_std_handles();
        cmd.creation_flags(crate::win::DETACHED_PROCESS);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe; the dialog leaves our process group, so a TERM to
        // the caller's group does not close it.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let _ = cmd.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings::{Code, Severity};

    fn linux() -> Facts {
        Facts { os: Some(Os::Linux), display: true, zenity: true, kdialog: true, ..Facts::default() }
    }
    fn windows() -> Facts {
        Facts { os: Some(Os::Windows), session_id: Some(1), visible_station: true, ..Facts::default() }
    }

    #[test]
    fn linux_prefers_zenity_then_kdialog_and_needs_a_display() {
        assert_eq!(choose(&linux()), Some(Tool::Zenity));
        assert_eq!(choose(&Facts { zenity: false, ..linux() }), Some(Tool::Kdialog));
        assert_eq!(choose(&Facts { zenity: false, kdialog: false, ..linux() }), None);
        assert_eq!(choose(&Facts { display: false, ..linux() }), None);
        // ssh does not matter on Linux: a forwarded display is a display
        assert_eq!(choose(&Facts { ssh: true, ..linux() }), Some(Tool::Zenity));
    }

    #[test]
    fn never_when_disabled_or_in_a_container() {
        assert_eq!(choose(&Facts { disabled: true, ..linux() }), None);
        assert_eq!(choose(&Facts { container: true, ..linux() }), None);
        assert_eq!(choose(&Facts { disabled: true, ..windows() }), None);
        assert_eq!(choose(&Facts::default()), None);
    }

    #[test]
    fn windows_only_in_an_interactive_desktop_session() {
        assert_eq!(choose(&windows()), Some(Tool::MessageBox));
        assert_eq!(choose(&Facts { ssh: true, ..windows() }), None);
        assert_eq!(choose(&Facts { session_id: Some(0), ..windows() }), None);
        assert_eq!(choose(&Facts { session_id: None, ..windows() }), None);
        assert_eq!(choose(&Facts { visible_station: false, ..windows() }), None);
        // no display variables on Windows: they are not asked for
        assert_eq!(choose(&Facts { display: false, ..windows() }), Some(Tool::MessageBox));
    }

    #[test]
    fn the_env_var_switches_it_off() {
        assert!(!env_disables(None));
        assert!(!env_disables(Some("")));
        assert!(!env_disables(Some("0")));
        assert!(!env_disables(Some("false")));
        assert!(env_disables(Some("1")));
        assert!(env_disables(Some("yes")));
    }

    #[test]
    fn the_message_lists_the_findings_and_the_command_lines_quote_nothing() {
        let f = Finding::new(Code::MissingBackups, Severity::Blocker, "backups are gone")
            .paths((0..8).map(|i| format!("game/f{i}")))
            .advice("--forget-missing-backups");
        let m = message("stopped", &[f]);
        assert!(m.starts_with("stopped\n\n• backups are gone\n    game/f0"));
        assert!(m.contains("… and 2 more") && m.contains("→ --forget-missing-backups"));
        // the text is one argument, never parsed by a shell or as markup
        let a = argv(Tool::Zenity, TITLE, "a <b> & $(c)");
        assert_eq!(a.last().unwrap(), "--text=a <b> & $(c)");
        assert!(a.contains(&"--no-markup".to_string()));
        assert_eq!(argv(Tool::Kdialog, TITLE, "x"), ["kdialog", "--title", "cs2-mcp", "--error", "x"]);
        assert_eq!(argv(Tool::MessageBox, TITLE, "x")[0], "show-dialog");
    }
}
