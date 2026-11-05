//! Which dynamic-library request is the engine's own server image, and where Metamod's loader
//! sits relative to it.
//!
//! Deliberately `cfg`-free: the same rules compile into both targets and are unit-tested on the
//! host, so the redirect *decision* is checked without a game, a Wine prefix or a container.
//!
//! The two rules differ on purpose (DESIGN §0):
//!
//! * **Windows** - loose: either separator, any case. The engine builds the name itself
//!   (`GetModuleFileNameW` plus `..` segments, mostly lower-case `csgo`), and the spike saw the
//!   one relevant call as `...\csgo\bin\win64\server.dll` in the tools boot and in the client.
//! * **Linux** - exact and case-sensitive: `dlopen` receives a byte string the engine assembled
//!   from absolute paths (`/csgo/bin/linuxsteamrt64/libserver.so`, DESIGN §0), so a case-insensitive
//!   match would only hide a real change.

use std::path::{Path, PathBuf};

/// The engine's own server image on Windows, as a path suffix, already normalised
/// (`/` folded to `\`, lower case - see [`windows_key`]).
pub const WINDOWS_SERVER_SUFFIX: &str = r"\csgo\bin\win64\server.dll";

/// The engine's own server image on Linux, as an exact suffix.
pub const LINUX_SERVER_SUFFIX: &str = "/csgo/bin/linuxsteamrt64/libserver.so";

/// Metamod's loader on Linux, relative to the same `csgo` directory the engine's image lives
/// under: `<csgo>/addons/metamod/bin/linuxsteamrt64/libserver.so`.
pub const LINUX_METAMOD_SUFFIX: &str = "/csgo/addons/metamod/bin/linuxsteamrt64/libserver.so";

/// The form a Windows path is compared in: `/` folded to `\`, ASCII-lowercased.
///
/// Only ASCII is folded: the paths that matter are ASCII, and a full Unicode case fold would make
/// the rule depend on the host's locale data.
pub fn windows_key(path: &str) -> String {
    path.replace('/', "\\").to_ascii_lowercase()
}

/// Is `path` the engine's own `server.dll`? (Windows: any case, either separator.)
///
/// A suffix match, not equality: the engine hands `LoadLibraryExW` an absolute path built from
/// its install root, which differs per tree.
pub fn is_windows_server_dll(path: &str) -> bool {
    windows_key(path).ends_with(WINDOWS_SERVER_SUFFIX)
}

/// Is `path` the engine's own `libserver.so`? (Linux: exact, case-sensitive suffix.)
pub fn is_linux_server_so(path: &str) -> bool {
    path.ends_with(LINUX_SERVER_SUFFIX)
}

/// Metamod's loader for the `csgo` directory that the engine's own `libserver.so` at `path` lives
/// under, or `None` if `path` is not the engine's image.
///
/// The `csgo` root is taken from the *matched suffix* (`<root>/csgo/bin/...`), never from a
/// `find("csgo")`: the install path itself can contain `csgo` (it does on our machines), and the
/// kernel's `LoadLibraryExW` window is the one place a wrong guess would be invisible.
pub fn linux_metamod_loader(path: &str) -> Option<String> {
    let csgo = path.strip_suffix(LINUX_SERVER_SUFFIX)?;
    Some(format!("{csgo}{LINUX_METAMOD_SUFFIX}"))
}

/// Metamod's loader on Windows, derived from the directory the launcher lives in: the launcher is
/// installed in `<game>\bin\win64`, Metamod in `<game>\csgo\addons\metamod\bin\win64`
/// (DESIGN §1/§2, the standard Metamod layout).
///
/// The `..` segments are left in place: every Windows API resolves them, and rewriting them
/// lexically here would be one more place to get a junction or a short name wrong. The launcher
/// passes the result to the DLL through `CS2MCP_MM_LOADER`; the DLL never derives it itself.
pub fn windows_metamod_loader_for_exe_dir(exe_dir: &Path) -> PathBuf {
    exe_dir
        .join("..")
        .join("..")
        .join("csgo")
        .join("addons")
        .join("metamod")
        .join("bin")
        .join("win64")
        .join("server.dll")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn windows_key_folds_separators_and_case() {
        assert_eq!(
            windows_key(r"C:\Game\CSGO\bin\Win64\server.dll"),
            r"c:\game\csgo\bin\win64\server.dll"
        );
        assert_eq!(
            windows_key("C:/game/csgo/bin/win64/SERVER.DLL"),
            r"c:\game\csgo\bin\win64\server.dll"
        );
    }

    #[test]
    fn windows_server_dll_matches_any_case_and_either_separator() {
        for path in [
            r"C:\game\csgo\bin\win64\server.dll",
            r"C:\GAME\CSGO\BIN\WIN64\SERVER.DLL",
            "C:/game/csgo/bin/win64/server.dll",
            r"\\?\C:\Program Files (x86)\Steam\steamapps\common\Counter-Strike Global Offensive\game\csgo\bin\win64\server.dll",
            // The engine reaches it through `..` segments in some boots; the tail is what counts.
            r"C:\game\bin\win64\..\..\csgo\bin\win64\server.dll",
        ] {
            assert!(is_windows_server_dll(path), "{path}");
        }
    }

    #[test]
    fn windows_server_dll_rejects_everything_else() {
        for path in [
            "",
            "server.dll",
            r"C:\game\csgo\bin\win64\client.dll",
            // A different architecture directory of the same tree.
            r"C:\game\csgo\bin\linuxsteamrt64\server.dll",
            // Only the tail counts: a file whose name merely ends in the same text, but not at a
            // component boundary, is not the engine's image.
            r"C:\game\csgo\bin\win64\notserver.dll",
            r"C:\game\csgo\bin\win64\server.dll.bak",
            r"C:\game\csgo\bin\win64\server.dll_backup",
            // `csgo\bin\win64` without the `\` before it.
            r"C:\game\mycsgo\bin\win64\server.dll",
        ] {
            assert!(!is_windows_server_dll(path), "{path}");
        }
    }

    #[test]
    fn linux_server_so_is_an_exact_case_sensitive_suffix() {
        for path in [
            "/game/csgo/bin/linuxsteamrt64/libserver.so",
            "/home/cs2/game/csgo/bin/linuxsteamrt64/libserver.so",
            // Hypothetical single-slash form of a `../..` path: it does match, so only the one shot
            // keeps such a request from being redirected again.
            "/game/bin/linuxsteamrt64/../../csgo/bin/linuxsteamrt64/libserver.so",
        ] {
            assert!(is_linux_server_so(path), "{path}");
        }
        for path in [
            "",
            "libserver.so",
            "/game/csgo/bin/linuxsteamrt64/libserver.so.bak",
            // Case matters: this is a different file, and a `dlopen` of it is not the engine's.
            "/game/csgo/bin/linuxsteamrt64/libServer.so",
            "/game/csgo/bin/win64/libserver.so",
            "/game/csgo/bin/linuxsteamrt64/libclient.so",
            // No component boundary before `csgo`.
            "/game/xcsgo/bin/linuxsteamrt64/libserver.so",
            // Metamod's loader's own load of the real image, exactly as seen live (2026-10-20).
            "/home/steam/steamapps/common/Counter-Strike Global Offensive/game/bin/linuxsteamrt64/../../csgo/bin/linuxsteamrt64//libserver.so",
            // Metamod's loader itself: matching it would redirect Metamod onto itself.
            "/game/csgo/addons/metamod/bin/linuxsteamrt64/libserver.so",
        ] {
            assert!(!is_linux_server_so(path), "{path}");
        }
    }

    #[test]
    fn metamod_loader_paths_never_match_on_windows() {
        for path in [
            r"Z:\app\game\csgo\addons\metamod\bin\win64\server.dll",
            "Z:/app/game/csgo/addons/metamod/bin/win64/server.dll",
            r"Z:\app\game\bin\win64\..\..\csgo\addons\metamod\bin\win64\server.dll",
        ] {
            assert!(!is_windows_server_dll(path), "{path}");
        }
    }

    #[test]
    fn linux_metamod_loader_takes_the_csgo_root_from_the_matched_suffix() {
        assert_eq!(
            linux_metamod_loader("/game/csgo/bin/linuxsteamrt64/libserver.so").as_deref(),
            Some("/game/csgo/addons/metamod/bin/linuxsteamrt64/libserver.so")
        );
        // The install path contains `csgo` too: the *last* component group is the one that moved.
        assert_eq!(
            linux_metamod_loader("/home/user/csgo/game/csgo/bin/linuxsteamrt64/libserver.so")
                .as_deref(),
            Some("/home/user/csgo/game/csgo/addons/metamod/bin/linuxsteamrt64/libserver.so")
        );
        assert_eq!(
            linux_metamod_loader("/game/bin/linuxsteamrt64/../../csgo/bin/linuxsteamrt64/libserver.so")
                .as_deref(),
            Some("/game/bin/linuxsteamrt64/../../csgo/addons/metamod/bin/linuxsteamrt64/libserver.so")
        );
        assert_eq!(linux_metamod_loader("/game/csgo/bin/linuxsteamrt64/libclient.so"), None);
    }

    #[test]
    fn windows_metamod_loader_is_two_levels_up_from_the_exe_dir() {
        let loader = windows_metamod_loader_for_exe_dir(Path::new("/game/bin/win64"));
        let tail: Vec<String> = loader
            .components()
            .rev()
            .take(8)
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert_eq!(
            tail,
            ["..", "..", "csgo", "addons", "metamod", "bin", "win64", "server.dll"]
        );
        assert!(loader.starts_with("/game/bin/win64"), "{}", loader.display());
    }
}
