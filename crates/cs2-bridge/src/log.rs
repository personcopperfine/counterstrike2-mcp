//! The bridge log.
//!
//! One prefixed line per call, opened/appended/closed each time, exactly like the C bridge's
//! `logline` (`mcp_bridge.c:198`: `Z:\app\mcp_bridge.log`, append, share read) - the
//! Wine tools can be killed at any moment, so a buffered handle would lose the last lines that
//! prove the plugin was ticked.
//!
//! The prefix carries the image's [`Roles`] tag (`[cs2-bridge mm] `, `[cs2-bridge tool] `,
//! `[cs2-bridge mm+tool] `, `[cs2-bridge none] `); see [`Roles`].
//!
//! Writing the log is the *only* silent failure in this crate: a log line that cannot be
//! written must not disturb the game. Everything else fails loudly (`cargo bridge check-exports`, the
//! tests).

use core::sync::atomic::{AtomicU8, Ordering};
use std::path::{Path, PathBuf};

/// Which entry points this image's state serves.
///
/// There is now **one image with a role set**, not two images: the Wine tools' Metamod vdf
/// points at `../bin/win64/tools/cs2_bridge.dll`, so the Metamod plugin and the tools framework
/// load the same file - one mapping, one set of statics - and Metamod's `Load` runs first (at
/// server-lib init), then the tools framework's `CreateInterface("ToolSystem2_001")`, `Connect`,
/// `Init` and ticks. On the Linux client and dedicated server there is only the Metamod role.
/// The image therefore records the roles it has served rather than "the first entry point wins":
///
/// * [`Role::Metamod`] - `CreateInterface("ISmmPlugin")` was served in this image, or a `Load`
///   completed in it. The entry point names it before `Load`, so a refused `Load` and the lines
///   around it carry the tag too (`src/lib.rs`, [`crate::plugin`]).
/// * [`Role::Tool`] - `CreateInterface("ToolSystem2_001")` was served: the tools framework's
///   object was handed out.
///
/// Every log line is tagged with the set, and `McpBridge_001` reports it as `"roles"`.
///
/// # The tag is a contract
///
/// `cargo bridge runtime` waits for `[cs2-bridge (mm+)?tool] Init`: the tool role must be in the
/// tag for the tool half to be considered up, and `mm` may precede it. The exact forms are
/// `[cs2-bridge mm]`, `[cs2-bridge tool]`, `[cs2-bridge mm+tool]` and `[cs2-bridge none]`,
/// roles joined by `+` in that order (`mm` first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    /// The Metamod plugin role: `CreateInterface("ISmmPlugin")` or a completed `Load`.
    Metamod = 1,
    /// The tools framework's tool-plugin role: `CreateInterface("ToolSystem2_001")`.
    Tool = 2,
}

impl Role {
    /// This role's bit in [`Roles`].
    #[must_use]
    pub const fn bit(self) -> u8 {
        self as u8
    }

    /// The name this role is reported under (`"mm"`, `"tool"`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Metamod => "mm",
            Self::Tool => "tool",
        }
    }
}

/// The set of [`Role`]s this image serves, as a bit set.
///
/// The order roles are reported in is fixed (`mm` before `tool`) and does not depend on which
/// entry point ran first: see [`Role`] for why the old first-wins image enum is gone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Roles(u8);

impl Roles {
    /// No role has run yet (a probe that only `dlsym`ed `CreateInterface`).
    pub const NONE: Self = Self(0);

    /// The set with `role` added.
    #[must_use]
    pub const fn with(self, role: Role) -> Self {
        Self(self.0 | role.bit())
    }

    /// Whether `role` is in the set.
    #[must_use]
    pub const fn contains(self, role: Role) -> bool {
        self.0 & role.bit() != 0
    }

    /// Whether no role has run yet.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The role names in the fixed order (`mm`, then `tool`).
    pub fn names(self) -> impl Iterator<Item = &'static str> {
        [Role::Metamod, Role::Tool]
            .into_iter()
            .filter(move |role| self.contains(*role))
            .map(Role::name)
    }

    /// The shorter form every log line carries: the names joined by `+`, `"none"` when empty.
    ///
    /// This is the `cargo bridge runtime` contract; see [`Role`].
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self.0 {
            0 => "none",
            1 => "mm",
            2 => "tool",
            _ => "mm+tool",
        }
    }

    /// The `"roles"` member of the `McpBridge_001` status line: a JSON array of the names in the
    /// same fixed order, `[]` when empty. Not `#[must_use]`: a caller that only wants the tag
    /// should not pay for this.
    #[must_use]
    pub fn json(self) -> String {
        let mut out = String::from("[");
        for (index, name) in self.names().enumerate() {
            if index != 0 {
                out.push(',');
            }
            out.push('"');
            out.push_str(name);
            out.push('"');
        }
        out.push(']');
        out
    }
}

/// Bitwise union, so a caller can build a set for a test or a report without naming bits.
impl core::ops::BitOr for Roles {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// The roles this image's state serves; empty until an entry point runs.
static ROLES: AtomicU8 = AtomicU8::new(0);

/// Record that this image serves `role` (a bit set: a second, different role adds to the set).
pub fn add_role(role: Role) {
    ROLES.fetch_or(role.bit(), Ordering::AcqRel);
}

/// The roles [`add_role`] recorded.
#[must_use]
pub fn roles() -> Roles {
    Roles(ROLES.load(Ordering::Acquire))
}

/// Append one line to the bridge log (`CS2_BRIDGE_LOG`, else the platform default).
///
/// Silent by design: see the module docs.
pub fn line(msg: &str) {
    let Some(path) = default_path() else {
        return;
    };
    let _ = append(&path, &format_line(roles(), msg));
}

/// The log path for this process, in a **test** build: `CS2_BRIDGE_LOG` only.
///
/// A unit test writes only where the case points it - `CS2_BRIDGE_LOG`, set by
/// `hooks::test_guard::scratch_log` under `target/scratch/`. The *default* path is the user's
/// real log (`$XDG_RUNTIME_DIR/cs2mcpbridge/bridge.log`, or `Z:\app\...`), and a `cargo test`
/// must never append to it: a case that only cares about its own bytes would otherwise grow the
/// developer's log with lines nobody asked for (review round 2: `cargo test --lib` left
/// `verify:` / `pet:` / teardown lines in it). Integration tests and the shipped image use the
/// production rule below.
#[cfg(test)]
fn default_path() -> Option<PathBuf> {
    let explicit = std::env::var_os("CS2_BRIDGE_LOG");
    if explicit.as_deref().is_none_or(std::ffi::OsStr::is_empty) {
        return None;
    }
    // Only `CS2_BRIDGE_LOG` is consulted in a test build: the other two arguments are the
    // user's real directories, which a case must never fall back into.
    resolve(explicit.as_deref(), None, None)
}

/// The log path for this process: `CS2_BRIDGE_LOG` if set, else the platform default.
#[cfg(not(test))]
fn default_path() -> Option<PathBuf> {
    resolve(
        std::env::var_os("CS2_BRIDGE_LOG").as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// The path rule, split from the environment so it is testable without touching the process
/// environment (and without racing the other tests).
///
/// * `CS2_BRIDGE_LOG` wins when set (and non-empty) - the switch the orchestrator and the tests
///   use to point the bridge somewhere writable.
/// * Windows: `Z:\app\mcp_bridge_rs.log`, the C bridge's directory with the Rust name
///   (`mcp_bridge.c:198` uses `Z:\app\mcp_bridge.log`; the two must not interleave), when
///   `Z:\app` exists (the container); else `%LOCALAPPDATA%\cs2mcpbridge\bridge.log` (a native
///   install, next to `session.json`).
/// * Linux: the discovery directory rule ([`crate::ipc::discovery::linux_dir`], the one helper
///   both paths go through, so log and `session.json` cannot drift) plus `bridge.log`:
///   `$XDG_RUNTIME_DIR/cs2mcpbridge/bridge.log`, else
///   `$HOME/.local/share/cs2mcpbridge/bridge.log`, else no log. The `$HOME` fallback is not a
///   guess - it is the same directory `session.json` falls back to, and it is the case that
///   matters on the Linux dedicated server, whose container has no `XDG_RUNTIME_DIR` at all
///   (before the fallback existed the bridge wrote no log there and every line was lost).
fn resolve(
    cs2_bridge_log: Option<&std::ffi::OsStr>,
    xdg_runtime_dir: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(p) = cs2_bridge_log.filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    #[cfg(windows)]
    {
        let _ = (xdg_runtime_dir, home);
        // the container (Wine `Z:` = its root, `/app` mounted): unchanged; a native install has no
        // `Z:\app` (the line was lost, e.g. a Steam-UI session's `-insecure` refusal): the
        // discovery file's directory
        if std::path::Path::new(r"Z:\app").is_dir() {
            return Some(PathBuf::from(r"Z:\app\mcp_bridge_rs.log"));
        }
        std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(|dir| PathBuf::from(dir).join("cs2mcpbridge").join("bridge.log"))
            .or_else(|| Some(PathBuf::from(r"Z:\app\mcp_bridge_rs.log")))
    }
    #[cfg(not(windows))]
    {
        crate::ipc::discovery::linux_dir(xdg_runtime_dir, home).map(|dir| dir.join("bridge.log"))
    }
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
/// On Linux the log lives in the same directory as `session.json` (the discovery rule,
/// [`crate::ipc::discovery::linux_dir`] - `$XDG_RUNTIME_DIR/cs2mcpbridge`, else
/// `$HOME/.local/share/cs2mcpbridge`), which carries the session token: the discovery writer
/// refuses to start when that directory grants group or other access, and the log is usually the
/// first writer to create it (a `wiring::tick` logs before the listener starts). Creating it
/// `0700` keeps the two in agreement instead of letting the log make the discovery path fail
/// closed. Failure is ignored here like everywhere else in this module: a lost log line must not
/// disturb the game.
#[cfg(unix)]
fn create_log_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// [`create_log_dir`] on Windows, where the C's `Z:\app` directory is used and no mode exists.
#[cfg(not(unix))]
fn create_log_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// One log line: the role tag, the message, the terminator.
fn format_line(roles: Roles, msg: &str) -> String {
    format!("[cs2-bridge {}] {msg}\n", roles.tag())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn cs2_bridge_log_overrides_both_defaults() {
        assert_eq!(
            resolve(
                Some(OsStr::new("/scratch/bridge.log")),
                Some(OsStr::new("/run/user/1000")),
                Some(OsStr::new("/home/x")),
            ),
            Some(PathBuf::from("/scratch/bridge.log"))
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_default_is_the_discovery_directory_rule() {
        // `XDG_RUNTIME_DIR` set: the same directory `session.json` lives in.
        assert_eq!(
            resolve(
                None,
                Some(OsStr::new("/run/user/1000")),
                Some(OsStr::new("/home/x")),
            ),
            Some(PathBuf::from("/run/user/1000/cs2mcpbridge/bridge.log"))
        );
        // An empty env value is not a path: the next rule applies.
        assert_eq!(
            resolve(Some(OsStr::new("")), Some(OsStr::new("/run/user/1000")), None),
            Some(PathBuf::from("/run/user/1000/cs2mcpbridge/bridge.log"))
        );
        // No `XDG_RUNTIME_DIR`: the `$HOME/.local/share` fallback - the dedicated-server
        // container's case (`HOME=/home/steam`, no `XDG_RUNTIME_DIR`), where the bridge
        // previously wrote no log at all.
        assert_eq!(
            resolve(None, None, Some(OsStr::new("/home/steam"))),
            Some(PathBuf::from(
                "/home/steam/.local/share/cs2mcpbridge/bridge.log"
            ))
        );
        // An empty `XDG_RUNTIME_DIR` counts as unset: the fallback applies too.
        assert_eq!(
            resolve(None, Some(OsStr::new("")), Some(OsStr::new("/home/steam"))),
            Some(PathBuf::from(
                "/home/steam/.local/share/cs2mcpbridge/bridge.log"
            ))
        );
        // An empty `HOME` is as good as none. Neither variable: no path at all (logged
        // nowhere), never a guess.
        assert_eq!(
            resolve(None, None, Some(OsStr::new(""))),
            None,
            "an empty HOME is not a path"
        );
        assert_eq!(resolve(None, None, None), None);
    }

    /// Log and discovery resolve their directory through the one helper
    /// ([`crate::ipc::discovery::linux_dir`]): for the same environment the log's parent is the
    /// directory `session.json` is written into - on every branch of the rule, fallback
    /// included.
    #[cfg(not(windows))]
    #[test]
    fn log_and_discovery_agree_on_the_directory() {
        let cases: [(Option<&OsStr>, Option<&OsStr>); 3] = [
            (Some(OsStr::new("/run/user/1000")), Some(OsStr::new("/home/x"))),
            (None, Some(OsStr::new("/home/x"))),
            (Some(OsStr::new("")), Some(OsStr::new("/home/x"))),
        ];
        for (xdg, home) in cases {
            let log = resolve(None, xdg, home).expect("a log path");
            let session = crate::ipc::discovery::linux_dir(xdg, home)
                .expect("a discovery directory")
                .join("session.json");
            assert_eq!(
                log.parent(),
                session.parent(),
                "log and session.json share the directory (xdg={xdg:?}, home={home:?})"
            );
        }
    }

    /// [`resolve`] with the values the process env holds: the production rule, for the cases
    /// that pin where the log lands - the test build's [`default_path`] consults only
    /// `CS2_BRIDGE_LOG`, so it cannot answer them.
    #[cfg(not(windows))]
    fn disk_rule() -> Option<PathBuf> {
        resolve(
            std::env::var_os("CS2_BRIDGE_LOG").as_deref(),
            std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }

    /// The process path rule - the one the production build feeds from the env - agrees with
    /// `session.json`'s: with `CS2_BRIDGE_LOG` unset and `XDG_RUNTIME_DIR` pointing at a scratch
    /// directory, both resolve under that scratch directory and nowhere else.
    ///
    /// The rule is asked through [`resolve`] with the values the env holds: a **test** build's
    /// [`default_path`] consults only `CS2_BRIDGE_LOG` (a unit test must never append to the
    /// user's log, review round 2), while the discovery side has no such variant.
    #[cfg(not(windows))]
    #[test]
    fn the_process_default_paths_agree() {
        let _guard = crate::hooks::test_guard::lock();
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch");
        let xdg = scratch.join("log-discovery-agree-xdg");
        std::fs::create_dir_all(&xdg).expect("the scratch XDG dir exists");
        let home = scratch.join("log-discovery-agree-home");

        // Restores the process env even when an assertion fails: the env is process-wide and
        // the other cases of this binary run after this one (the guard above serialises them).
        struct Env(Option<std::ffi::OsString>, Option<std::ffi::OsString>, Option<std::ffi::OsString>);
        impl Drop for Env {
            fn drop(&mut self) {
                fn restore(name: &str, value: &mut Option<std::ffi::OsString>) {
                    match value.take() {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
                restore("CS2_BRIDGE_LOG", &mut self.0);
                restore("XDG_RUNTIME_DIR", &mut self.1);
                restore("HOME", &mut self.2);
            }
        }
        let _env = Env(
            std::env::var_os("CS2_BRIDGE_LOG"),
            std::env::var_os("XDG_RUNTIME_DIR"),
            std::env::var_os("HOME"),
        );

        std::env::remove_var("CS2_BRIDGE_LOG");
        std::env::set_var("XDG_RUNTIME_DIR", &xdg);

        let log = disk_rule().expect("a log path under the scratch XDG");
        let session = crate::ipc::discovery::default_path()
            .expect("a discovery path under the scratch XDG");
        let dir = xdg.join("cs2mcpbridge");
        assert_eq!(
            log.parent(),
            session.parent(),
            "both defaults resolve into the same directory"
        );
        assert_eq!(
            log.parent(),
            Some(dir.as_path()),
            "and that directory is the scratch XDG's cs2mcpbridge"
        );

        // And with no `XDG_RUNTIME_DIR` either, both fall to the scratch `$HOME` together (the
        // dedicated-server container's shape, pointed at `target/`, never the user's home).
        std::env::remove_var("XDG_RUNTIME_DIR");
        std::env::set_var("HOME", &home);
        let log = disk_rule().expect("a log path under the scratch HOME");
        let session = crate::ipc::discovery::default_path()
            .expect("a discovery path under the scratch HOME");
        assert_eq!(log.parent(), session.parent());
        assert!(log.starts_with(&scratch), "never the user's real home: {log:?}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_default_is_the_c_bridge_directory() {
        assert_eq!(
            resolve(None, None, None),
            Some(PathBuf::from(r"Z:\app\mcp_bridge_rs.log"))
        );
    }

    #[test]
    fn append_writes_prefixed_lines_in_order() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch/cs2-bridge-log-test.log");
        let _ = std::fs::remove_file(&path);
        append(&path, &format_line(Roles::NONE.with(Role::Tool), "first")).expect("write line 1");
        append(&path, &format_line(Roles::NONE.with(Role::Metamod), "second"))
            .expect("write line 2");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(
            text,
            "[cs2-bridge tool] first\n[cs2-bridge mm] second\n",
            "each line carries the roles it came from"
        );
        std::fs::remove_file(&path).expect("clean up");
    }

    /// The four role sets' tags, byte for byte: this exact form is the `cargo bridge runtime`
    /// contract (`grep -E '\[cs2-bridge (mm\+)?tool\] Init'`), and the JSON array the status
    /// line reports is the same set in the same order.
    #[test]
    fn role_tags_and_json_for_all_four_sets_are_exact_bytes() {
        let none = Roles::NONE;
        let mm = Roles::NONE.with(Role::Metamod);
        let tool = Roles::NONE.with(Role::Tool);
        let both = Roles::NONE.with(Role::Tool).with(Role::Metamod);

        assert_eq!(format_line(none, "Init"), "[cs2-bridge none] Init\n");
        assert_eq!(format_line(mm, "Init"), "[cs2-bridge mm] Init\n");
        assert_eq!(format_line(tool, "Init"), "[cs2-bridge tool] Init\n");
        assert_eq!(format_line(both, "Init"), "[cs2-bridge mm+tool] Init\n");

        assert_eq!(none.json(), "[]");
        assert_eq!(mm.json(), "[\"mm\"]");
        assert_eq!(tool.json(), "[\"tool\"]");
        assert_eq!(both.json(), "[\"mm\",\"tool\"]");

        // The set is a set: adding one role twice changes nothing, and the order of the adds
        // does not matter.
        assert_eq!(both, Roles::NONE.with(Role::Metamod).with(Role::Tool));
        assert_eq!(both.tag(), "mm+tool");
        assert!(both.contains(Role::Metamod) && both.contains(Role::Tool));
        assert!(!none.contains(Role::Tool) && none.is_empty());
        assert_eq!(none.names().count(), 0);
        assert_eq!(both.names().collect::<Vec<_>>(), vec!["mm", "tool"]);
    }
}
