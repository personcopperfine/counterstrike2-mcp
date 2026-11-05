//! The discovery file: `session.json`, written after the listener is bound and before any
//! client can connect.
//!
//! Ported from `write_discovery` (`mcp_bridge.c:6703-6729`): the exact field order and
//! whitespace, the numbers unquoted, `pipeName` null, and the tmp-file-plus-rename that makes a
//! reader see either the previous file or the complete new one, never a partial write. The
//! directory is created first (the C does it in `start_listener_once`, `:6759`).
//!
//! The production path is the C's on Windows/Proton (`%LOCALAPPDATA%\cs2mcpbridge\session.json`,
//! fallback `C:\users\steamuser\AppData\Local`, `:6746-6758`) and
//! `$XDG_RUNTIME_DIR/cs2mcpbridge/session.json` (fallback
//! `~/.local/share/cs2mcpbridge/session.json`) on native Linux, where the host reads it through
//! `cs2_mcp_common::bridge::discovery` (see `linux_default_path` there).

use std::fmt;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// The bytes `write_discovery` produces, over the values it reads.
///
/// `started_at` is the C's `g_startTick` (`:6714`, a millisecond tick count set at start); it is
/// a parameter here so the file is reproducible in a test.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ticket<'a> {
    /// `GetCurrentProcessId()` (`:6706`).
    pub pid: u32,
    /// The listener's ephemeral port (`:6711`).
    pub port: u16,
    /// The 64 lowercase hex characters of [`super::token`] (`:6712`).
    pub token: &'a str,
    /// The start tick (`:6714`).
    pub started_at: u64,
}

/// The exact text of `write_discovery` (`mcp_bridge.c:6703-6718`).
///
/// Every byte is the C's: the newline after `{`, the two-space indent, the field order, the
/// spaces after the colons, the trailing newline.
pub(crate) fn render(ticket: &Ticket<'_>) -> String {
    format!(
        "{{\n  \"pid\": {pid},\n  \"protocol\": 1,\n  \"bridgeVersion\": \"{build}\",\n  \
         \"gameBuild\": \"{game}\",\n  \"transport\": \"tcp\",\n  \"host\": \"127.0.0.1\",\n  \
         \"port\": {port},\n  \"pipeName\": null,\n  \"token\": \"{token}\",\n  \
         \"startedAt\": {started}\n}}\n",
        pid = ticket.pid,
        build = crate::queue::BRIDGE_BUILD,
        game = crate::generated::pins::PIN_GAME_BUILD,
        port = ticket.port,
        token = ticket.token,
        started = ticket.started_at,
    )
}

/// The production discovery path, or an error when the environment names no base directory.
///
/// Windows/Proton: `%LOCALAPPDATA%\cs2mcpbridge\session.json`, with the C's
/// `C:\users\steamuser\AppData\Local` fallback (`:6746-6754`). Linux:
/// `$XDG_RUNTIME_DIR/cs2mcpbridge/session.json`, fallback
/// `~/.local/share/cs2mcpbridge/session.json`; with neither there is no safe guess (the game
/// process's working directory is not writable state this crate may create), so it is an error.
pub(crate) fn default_path() -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\users\steamuser\AppData\Local"));
        Ok(base.join("cs2mcpbridge").join("session.json"))
    }
    #[cfg(not(windows))]
    {
        let xdg = std::env::var_os("XDG_RUNTIME_DIR");
        let home = std::env::var_os("HOME");
        path_from(xdg.as_deref(), home.as_deref()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "neither XDG_RUNTIME_DIR nor HOME is set; no discovery path",
            )
        })
    }
}

/// The Linux directory rule, shared with the bridge log's default path (`crate::log` puts
/// `bridge.log` in this same directory) so the two cannot drift: `$XDG_RUNTIME_DIR/cs2mcpbridge`,
/// else `$HOME/.local/share/cs2mcpbridge`, else nothing. An empty value counts as unset.
///
/// The dedicated-server container has no `XDG_RUNTIME_DIR` at all (its env is
/// `HOME=/home/steam`, `XDG_CACHE_HOME=/cache`), so the `$HOME` fallback is the rule that runs
/// there - for the log and for `session.json` alike.
#[cfg(not(windows))]
pub(crate) fn linux_dir(
    xdg_runtime_dir: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(dir) = xdg_runtime_dir.filter(|value| !value.is_empty()) {
        return Some(Path::new(dir).join("cs2mcpbridge"));
    }
    home.filter(|value| !value.is_empty())
        .map(|dir| Path::new(dir).join(".local/share/cs2mcpbridge"))
}

/// The Linux rule, split from the environment so it is testable without touching the process
/// environment: `$XDG_RUNTIME_DIR/cs2mcpbridge/session.json`, else
/// `$HOME/.local/share/cs2mcpbridge/session.json`, else nothing.
#[cfg(not(windows))]
fn path_from(
    xdg_runtime_dir: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    linux_dir(xdg_runtime_dir, home).map(|dir| dir.join("session.json"))
}

/// The temporary file `write_discovery` writes first (`:6769`, `session.json.tmp`).
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Writes `text` through a temporary file and a rename (`mcp_bridge.c:6719-6727`), so a reader
/// never observes a partially written `session.json`.
///
/// The file carries the session token, which is a capability: on Unix the temporary file is
/// created `0600` and the directory `0700`, so the `$HOME/.local/share` fallback is no more
/// readable than the `%LOCALAPPDATA%` the C uses on Windows (the `$XDG_RUNTIME_DIR` default is
/// already private). The rename carries the mode to `session.json`.
///
/// The temporary path is **not trusted**: an existing `session.json.tmp` is unlinked first (a
/// symlink is unlinked, never followed), and the new file is created `O_EXCL|O_NOFOLLOW` with
/// mode 0600 and then `fchmod`ed to 0600, so neither a stale file nor a planted symlink can
/// redirect the token into a file this user does not own. On Unix an existing discovery
/// directory must be private to the current user (owner and `mode & 0o077 == 0`) or the write
/// fails with [`InsecureDirectory`]; `start` treats that as fatal.
///
/// `before_rename` runs after the temporary file is complete and before the rename; the
/// atomicity test uses it to observe the two files, which is the only moment both exist.
pub(crate) fn write_atomic(
    path: &Path,
    text: &str,
    before_rename: &mut dyn FnMut(&Path),
) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let tmp = tmp_path(path);
    {
        let mut file = create_private_file(&tmp)?;
        file.write_all(text.as_bytes())?;
        // The C closes the handle before the move (`:6724`); syncing is the same guarantee
        // without relying on the filesystem's writeback timing.
        file.sync_all()?;
    }
    before_rename(&tmp);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// An existing discovery directory that is not private to this user: it belongs to another
/// uid, or it grants group/other access, so the token file written into it could be read or
/// replaced by someone else.
///
/// It is carried as the inner error of an [`io::Error`] (kind `PermissionDenied`) so
/// [`write_atomic`] keeps returning `io::Result`; [`is_insecure_directory`] recognises it and
/// [`super::start`] refuses to serve on it.
#[derive(Debug)]
pub(crate) struct InsecureDirectory {
    dir: PathBuf,
    reason: String,
}

impl fmt::Display for InsecureDirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "discovery directory {} is not private to this user: {}",
            self.dir.display(),
            self.reason
        )
    }
}

impl std::error::Error for InsecureDirectory {}

/// Whether `error` is [`write_atomic`]'s refusal of an insecure discovery directory (rather
/// than an ordinary I/O failure, which the C logs and serves through, `:6364`).
pub(crate) fn is_insecure_directory(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<InsecureDirectory>())
}

/// `CreateDirectoryA` (`mcp_bridge.c:6759`), `0700` on Unix.
///
/// An existing directory is checked first: it must be owned by the effective uid and grant
/// nothing to group or other (`mode & 0o077 == 0`), otherwise the write fails closed with
/// [`InsecureDirectory`]. The check is on the discovery directory alone - its parents
/// (`$HOME`, `$XDG_RUNTIME_DIR`) are not ours to police. (`DirBuilder`'s mode applies to every
/// directory it creates, as it did before this check existed: a missing parent on the `$HOME`
/// fallback, e.g. `~/.local`, is created with that same `0700`.)
fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        check_private_dir(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// The Unix check behind [`create_private_dir`]: owner is the effective uid and no group or
/// other permission bit is set.
#[cfg(unix)]
fn check_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::metadata(dir)?;
    // SAFETY: `geteuid` takes no arguments, has no preconditions and no side effects; it
    // returns the process's effective uid, which is the identity the following write runs as.
    let uid = unsafe { libc::geteuid() };
    let mode = metadata.mode() & 0o777;
    let reason = if !metadata.is_dir() {
        Some("not a directory".to_string())
    } else if metadata.uid() != uid {
        Some(format!("owned by uid {}, not {uid}", metadata.uid()))
    } else if mode & 0o077 != 0 {
        Some(format!("mode {mode:04o} grants group or other access"))
    } else {
        None
    };
    match reason {
        Some(reason) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            InsecureDirectory {
                dir: dir.to_path_buf(),
                reason,
            },
        )),
        None => Ok(()),
    }
}

/// `CreateFileA(..., CREATE_ALWAYS, ...)` (`:6719`), `0600` on Unix.
///
/// Unix: the temporary path is unlinked first (a symlink goes, its target stays), then the
/// file is created `create_new` (`O_CREAT|O_EXCL`) with `O_NOFOLLOW` and mode 0600, and
/// `fchmod`ed to 0600 so the umask cannot widen it. `O_EXCL` alone already refuses a symlink
/// at the final component; `O_NOFOLLOW` is the second lock on the same door, and `create_new`
/// refuses anything another process planted between the unlink and the open.
#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    // `mode` at creation is masked by the umask; `fchmod` sets the exact bits.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// `CreateFileA(..., CREATE_ALWAYS, ...)` (`:6719`) on Windows, where the C's own
/// `session.json.tmp` handling is unchanged.
#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::create(path)
}

/// [`write_atomic`] without an observation hook.
pub(crate) fn write(path: &Path, text: &str) -> io::Result<()> {
    write_atomic(path, text, &mut |_| {})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh scratch directory under `target/scratch/` (never `/tmp`, which is RAM), private
    /// to this user like the directory the production path creates.
    #[cfg(unix)]
    fn scratch(name: &str) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt as _;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../target/scratch/{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("create the scratch dir");
        dir
    }

    /// The 9 permission bits of `path`.
    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    /// A stale `session.json.tmp` - a world-readable regular file from an older run - is
    /// unlinked, and the new temporary file starts from `0600` regardless of the umask; after
    /// the rename the discovery file is `0600` and carries only the new text.
    #[cfg(unix)]
    #[test]
    fn a_stale_tmp_file_is_replaced_with_a_private_one() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = scratch("discovery-stale-tmp");
        let path = dir.join("session.json");
        let tmp = tmp_path(&path);
        std::fs::write(&tmp, "stale, world-readable\n").expect("plant the stale tmp");
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o666))
            .expect("make it 0666");

        write(&path, "fresh token\n").expect("write through the stale tmp");

        assert_eq!(
            std::fs::read_to_string(&path).expect("read the discovery file"),
            "fresh token\n",
            "the stale content is gone"
        );
        assert_eq!(mode(&path), 0o600, "the token file is private");
        assert!(!tmp.exists(), "the stale temporary file was replaced");
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    /// A `session.json.tmp` that is a symlink (to a file this user has, or to one they do not)
    /// is unlinked, never followed: the victim keeps its bytes and the rename writes the
    /// discovery file at the real path.
    #[cfg(unix)]
    #[test]
    fn a_tmp_symlink_does_not_redirect_the_write() {
        let dir = scratch("discovery-tmp-symlink");
        let path = dir.join("session.json");
        let victim = dir.join("victim");
        std::fs::write(&victim, "victim bytes\n").expect("write the victim");
        let tmp = tmp_path(&path);
        std::os::unix::fs::symlink(&victim, &tmp).expect("plant the symlink");

        write(&path, "fresh token\n").expect("write past the symlinked tmp");

        assert_eq!(
            std::fs::read_to_string(&victim).expect("read the victim"),
            "victim bytes\n",
            "the symlink target must not be written through"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read the discovery file"),
            "fresh token\n",
            "the real discovery path got the text"
        );
        assert!(!tmp.exists(), "the symlink was unlinked and its name renamed away");
        assert_eq!(mode(&path), 0o600);
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    /// An existing discovery directory that grants group or other access is refused before
    /// anything is written into it, and the refusal is the kind `start` treats as fatal.
    #[cfg(unix)]
    #[test]
    fn an_insecure_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = scratch("discovery-insecure-dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("widen the directory to 0755");
        let path = dir.join("session.json");

        let error = write(&path, "fresh token\n").expect_err("0755 must be refused");
        assert!(
            is_insecure_directory(&error),
            "the refusal is the fail-closed directory error, not a generic I/O error: {error}"
        );
        assert!(!path.exists(), "nothing is written into an insecure directory");
        assert!(!tmp_path(&path).exists(), "not even a temporary file");
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    /// The C's bytes, field by field (`mcp_bridge.c:6705-6717`). `BRIDGE_BUILD` and the game
    /// build come from the generated pins, as `BRIDGE_BUILD`/`PIN_GAME_BUILD` do in the C.
    #[test]
    fn render_is_the_c_field_set_and_format() {
        let ticket = Ticket {
            pid: 4242,
            port: 51234,
            token: &"ab".repeat(32),
            started_at: 987_654,
        };
        let expected = format!(
            "{{\n  \"pid\": 4242,\n  \"protocol\": 1,\n  \"bridgeVersion\": \"{build}\",\n  \
             \"gameBuild\": \"{game}\",\n  \"transport\": \"tcp\",\n  \"host\": \"127.0.0.1\",\n  \
             \"port\": 51234,\n  \"pipeName\": null,\n  \"token\": \"{token}\",\n  \
             \"startedAt\": 987654\n}}\n",
            build = crate::queue::BRIDGE_BUILD,
            game = crate::generated::pins::PIN_GAME_BUILD,
            token = "ab".repeat(32),
        );
        assert_eq!(render(&ticket), expected);
    }

    /// The rename step: while the temporary file exists, the target is either absent (first
    /// write) or still the previous bytes (rewrite); it is never a partial file.
    #[test]
    fn the_target_is_never_partial() {
        let dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch/discovery-atomic");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("session.json");

        let mut seen = Vec::new();
        write_atomic(&path, "first\n", &mut |tmp| {
            seen.push(tmp.to_path_buf());
            assert!(
                !path.exists(),
                "the target must not exist before the rename"
            );
            assert_eq!(
                std::fs::read_to_string(tmp).expect("read tmp"),
                "first\n",
                "the temporary file holds the complete text"
            );
        })
        .expect("write first");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read target"),
            "first\n"
        );
        assert!(!seen[0].exists(), "the temporary file is renamed away");

        write_atomic(&path, "second, longer\n", &mut |tmp| {
            assert_eq!(
                std::fs::read_to_string(&path).expect("read old target"),
                "first\n",
                "the previous file stays readable until the rename"
            );
            assert_eq!(
                std::fs::read_to_string(tmp).expect("read tmp"),
                "second, longer\n"
            );
        })
        .expect("write second");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read target"),
            "second, longer\n"
        );
        assert!(!tmp_path(&path).exists(), "no .tmp file is left behind");
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    /// The Linux rule (`$XDG_RUNTIME_DIR` first, then `$HOME/.local/share`), with an empty
    /// value treated as unset, as the log path's `resolve` does.
    #[cfg(not(windows))]
    #[test]
    fn linux_path_rule() {
        use std::ffi::OsStr;
        assert_eq!(
            path_from(
                Some(OsStr::new("/run/user/1000")),
                Some(OsStr::new("/home/x"))
            ),
            Some(PathBuf::from("/run/user/1000/cs2mcpbridge/session.json"))
        );
        assert_eq!(
            path_from(Some(OsStr::new("")), Some(OsStr::new("/home/x"))),
            Some(PathBuf::from(
                "/home/x/.local/share/cs2mcpbridge/session.json"
            ))
        );
        assert_eq!(
            path_from(None, Some(OsStr::new("/home/x"))),
            Some(PathBuf::from(
                "/home/x/.local/share/cs2mcpbridge/session.json"
            ))
        );
        assert_eq!(path_from(None, None), None);
    }
}
