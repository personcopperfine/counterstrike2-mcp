//! Where this launcher's own files are, however it was installed:
//!
//! * the portable zip / tarball and the MSI: `scripts/package.sh`'s layout, every binary next to
//!   `payload/`, `sigs/` and `SHA256SUMS` (the "package dir");
//! * the .deb: the same layout at `/opt/cs2mcp/`, the GUI at `/usr/bin/` (both found as
//!   `<exe dir>/../../opt/cs2mcp`, which holds inside the AppImage too);
//! * the AppImage: the same layout at `$APPDIR/opt/cs2mcp/`, the GUI at `$APPDIR/usr/bin/`. The
//!   mount path changes on every start and vanishes when the AppImage exits, so anything that has
//!   to outlive this process (the supervisor, the MCP config) names `$APPIMAGE` itself, a
//!   multicall entry: `<AppImage> cs2mcp-native ...`, `<AppImage> client-mcp ...`,
//!   `<AppImage> hammer-mcp ...` (no argument: the GUI);
//! * a development build: the repo's `target/` (see `plan::Artifacts::resolve`).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The package layout's place inside an AppImage's AppDir.
pub const APPDIR_PACKAGE: &str = "opt/cs2mcp";

/// The binaries a multicall AppImage dispatches to (first argument).
pub const MULTICALL: [&str; 3] = ["cs2mcp-native", "client-mcp", "hammer-mcp"];

pub fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// `$APPDIR` and `$APPIMAGE` when this process runs from an AppImage: both set, and this
/// executable inside `$APPDIR`.
pub fn appimage() -> Option<(PathBuf, PathBuf)> {
    appimage_from(std::env::var_os("APPDIR"), std::env::var_os("APPIMAGE"), std::env::current_exe().ok())
}

fn appimage_from(appdir: Option<OsString>, appimage: Option<OsString>, exe: Option<PathBuf>) -> Option<(PathBuf, PathBuf)> {
    let appdir = PathBuf::from(appdir.filter(|v| !v.is_empty())?);
    let image = PathBuf::from(appimage.filter(|v| !v.is_empty())?);
    let exe = exe?;
    exe.starts_with(&appdir).then_some((appdir, image))
}

/// Does `d` hold the package layout (`payload/` or the `cs2mcp-native` binary)?
fn is_package(d: &Path) -> bool {
    d.join("payload").is_dir() || d.join(exe_name("cs2mcp-native")).is_file()
}

/// The package dir for an executable in `exe_dir`: that dir when the layout is there, else
/// `<exe dir>/../../opt/cs2mcp` (`/usr/bin` -> `/opt/cs2mcp`; `$APPDIR/usr/bin` the same).
fn package_dir_of(exe_dir: &Path) -> Option<PathBuf> {
    if is_package(exe_dir) {
        return Some(exe_dir.to_path_buf());
    }
    let opt = exe_dir.parent()?.parent()?.join(APPDIR_PACKAGE);
    is_package(&opt).then_some(opt)
}

/// The package dir: next to this executable, or the `opt/cs2mcp` of a .deb / an AppImage
/// ([`package_dir_of`]), else `$APPDIR/opt/cs2mcp` inside an AppImage, else `None` (a development
/// build).
pub fn package_dir() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
    if let Some(d) = exe_dir.as_deref().and_then(package_dir_of) {
        return Some(d);
    }
    if let Some((appdir, _)) = appimage() {
        let d = appdir.join(APPDIR_PACKAGE);
        if d.is_dir() {
            return Some(d);
        }
    }
    None
}

/// How to run `cs2mcp-native` so that it outlives the caller (the supervisor is started this way).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeCmd {
    pub program: PathBuf,
    /// arguments before the subcommand (`["cs2mcp-native"]` for the multicall AppImage)
    pub prefix: Vec<OsString>,
}

impl NativeCmd {
    pub fn command(&self) -> std::process::Command {
        let mut c = std::process::Command::new(&self.program);
        c.args(&self.prefix);
        c
    }
    /// This executable as it is (the CLI).
    pub fn current() -> std::io::Result<NativeCmd> {
        Ok(NativeCmd { program: std::env::current_exe()?, prefix: Vec::new() })
    }
}

/// `cs2mcp-native` for a caller that is not necessarily it (the GUI): `$APPIMAGE cs2mcp-native`
/// in an AppImage, the sibling `cs2mcp-native[.exe]` when there is one (the packaged, live-tested
/// supervisor), else this executable (which must then handle `supervise` itself).
pub fn native_command() -> std::io::Result<NativeCmd> {
    if let Some((_, image)) = appimage() {
        return Ok(NativeCmd { program: image, prefix: vec![OsString::from("cs2mcp-native")] });
    }
    let exe = std::env::current_exe()?;
    if exe.file_stem() == Some(OsStr::new("cs2mcp-native")) {
        return Ok(NativeCmd { program: exe, prefix: Vec::new() });
    }
    if let Some(d) = exe.parent() {
        let sib = d.join(exe_name("cs2mcp-native"));
        if sib.is_file() {
            return Ok(NativeCmd { program: sib, prefix: Vec::new() });
        }
    }
    Ok(NativeCmd { program: exe, prefix: Vec::new() })
}

/// Variables the classic AppImage `AppRun` sets or prefixes with `$APPDIR` paths. They must not
/// reach the game (a bundled library dir in `LD_LIBRARY_PATH` changes what CS2 links against)
/// or the MCP servers' children.
const APPIMAGE_VARS: [&str; 6] = ["APPDIR", "APPIMAGE", "ARGV0", "OWD", "APPIMAGE_UUID", "APPIMAGE_EXTRACT_AND_RUN"];
const APPIMAGE_PATH_LISTS: [&str; 10] = [
    "PATH",
    "LD_LIBRARY_PATH",
    "XDG_DATA_DIRS",
    "PYTHONPATH",
    "PERLLIB",
    "GSETTINGS_SCHEMA_DIR",
    "QT_PLUGIN_PATH",
    "GST_PLUGIN_SYSTEM_PATH",
    "GST_PLUGIN_SYSTEM_PATH_1_0",
    "PYTHONHOME",
];

/// What to change in a child's environment so that nothing of the AppImage leaks into it:
/// `(name, Some(value))` to set, `(name, None)` to remove. Pure (takes the environment).
pub fn appimage_scrub(appdir: &Path, env: &[(OsString, OsString)]) -> Vec<(OsString, Option<OsString>)> {
    let mut out = Vec::new();
    // AppImages are Linux-only: `:`-separated lists
    let sep = ':';
    for (k, v) in env {
        let name = k.to_string_lossy();
        if APPIMAGE_VARS.contains(&name.as_ref()) {
            out.push((k.clone(), None));
        } else if APPIMAGE_PATH_LISTS.contains(&name.as_ref()) {
            let s = v.to_string_lossy();
            let kept: Vec<&str> = s.split(sep).filter(|p| !p.is_empty() && !Path::new(p).starts_with(appdir)).collect();
            if kept.len() != s.split(sep).filter(|p| !p.is_empty()).count() {
                if kept.is_empty() {
                    out.push((k.clone(), None));
                } else {
                    out.push((k.clone(), Some(OsString::from(kept.join(&sep.to_string())))));
                }
            }
        }
    }
    out
}

/// Apply [`appimage_scrub`] to `cmd` when this process runs from an AppImage.
pub fn scrub_appimage_env(cmd: &mut std::process::Command) {
    let Some(appdir) = std::env::var_os("APPDIR").filter(|v| !v.is_empty()) else { return };
    let env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    for (k, v) in appimage_scrub(Path::new(&appdir), &env) {
        match v {
            Some(v) => {
                cmd.env(k, v);
            }
            None => {
                cmd.env_remove(k);
            }
        }
    }
}

/// The multicall dispatch of the AppImage's entry binary: `Some((binary path, remaining args))`
/// when the first argument names one of [`MULTICALL`] and the package dir has it.
pub fn multicall_target(args: &[OsString]) -> Option<(PathBuf, Vec<OsString>)> {
    let first = args.get(1)?.to_str()?;
    if !MULTICALL.contains(&first) {
        return None;
    }
    let bin = package_dir()?.join(exe_name(first));
    bin.is_file().then(|| (bin, args[2..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appimage_needs_both_vars_and_the_exe_inside_the_mount() {
        let exe = Some(PathBuf::from("/tmp/.mount_x/usr/bin/cs2mcp-launcher"));
        let got = appimage_from(Some("/tmp/.mount_x".into()), Some("/home/u/cs2mcp.AppImage".into()), exe.clone());
        assert_eq!(got, Some((PathBuf::from("/tmp/.mount_x"), PathBuf::from("/home/u/cs2mcp.AppImage"))));
        assert_eq!(appimage_from(Some("/tmp/.mount_x".into()), None, exe.clone()), None);
        // inherited variables in a process that is not the AppImage's (the game, a server)
        assert_eq!(appimage_from(Some("/tmp/.mount_y".into()), Some("/a".into()), exe), None);
    }

    #[test]
    fn the_package_is_next_to_the_exe_or_in_opt() {
        let base = crate::testutil::scratch("selfexe-pkg");
        std::fs::create_dir_all(base.join("zip/payload")).unwrap();
        assert_eq!(package_dir_of(&base.join("zip")), Some(base.join("zip")));
        // the .deb and the AppImage: usr/bin/<gui>, opt/cs2mcp/<package>
        std::fs::create_dir_all(base.join("root/usr/bin")).unwrap();
        assert_eq!(package_dir_of(&base.join("root/usr/bin")), None);
        std::fs::create_dir_all(base.join("root/opt/cs2mcp/payload")).unwrap();
        assert_eq!(package_dir_of(&base.join("root/usr/bin")), Some(base.join("root/opt/cs2mcp")));
    }

    #[test]
    fn the_scrub_removes_appimage_vars_and_its_path_entries_only() {
        let env: Vec<(OsString, OsString)> = [
            ("APPDIR", "/tmp/.mount_x"),
            ("APPIMAGE", "/home/u/a.AppImage"),
            ("LD_LIBRARY_PATH", "/tmp/.mount_x/usr/lib/:/tmp/.mount_x/usr/lib64/"),
            ("PATH", "/tmp/.mount_x/usr/bin/:/usr/local/bin:/usr/bin"),
            ("XDG_DATA_DIRS", "/usr/share"),
            ("HOME", "/home/u"),
        ]
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect();
        let s = appimage_scrub(Path::new("/tmp/.mount_x"), &env);
        let get = |k: &str| s.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("APPDIR"), Some(None));
        assert_eq!(get("APPIMAGE"), Some(None));
        assert_eq!(get("LD_LIBRARY_PATH"), Some(None));
        assert_eq!(get("PATH"), Some(Some(OsString::from("/usr/local/bin:/usr/bin"))));
        assert_eq!(get("XDG_DATA_DIRS"), None, "untouched");
        assert_eq!(get("HOME"), None);
    }
}
