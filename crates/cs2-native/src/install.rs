//! Finding the user's CS2 install: an explicit `--game-root`, or the Steam libraries
//! (`libraryfolders.vdf` → the library holding `appmanifest_730.acf` → `common/<installdir>`).
//!
//! The parsers are pure and take text, so the fixtures in the tests stand in for a Steam tree.

use anyhow::{bail, Context};
use std::path::{Path, PathBuf};

/// CS2's Steam app id.
pub const APP_ID: &str = "730";

/// Which half of the game the install carries (and the launcher drives).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Windows,
    Linux,
}

impl Os {
    /// The platform this binary was built for: the game tree it can launch.
    pub const fn host() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        }
    }
}

/// A located install.
#[derive(Debug, Clone)]
pub struct Install {
    /// `.../Counter-Strike Global Offensive` (holds `game/`)
    pub root: PathBuf,
    /// the Steam library that holds it, when found through one
    pub library: Option<PathBuf>,
    /// `ClientVersion` from `game/csgo/steam.inf`
    pub build: Option<String>,
    /// the Workshop Tools DLC: `game/bin/win64/tools/hammer.dll` + `game/bin/sdkenginetools.txt`
    pub has_tools: bool,
    /// the game binary for [`Os::host`] exists (`cs2.exe` / `cs2.sh`)
    pub has_game_binary: bool,
}

impl Install {
    pub fn game(&self) -> PathBuf {
        self.root.join("game")
    }

    /// Inspect `root` (the install root, or its `game/` dir, which is accepted too).
    pub fn at(root: &Path, library: Option<PathBuf>) -> anyhow::Result<Install> {
        let root = if root.join("csgo").is_dir() && root.file_name().is_some_and(|n| n == "game") {
            root.parent().map(Path::to_path_buf).unwrap_or_else(|| root.to_path_buf())
        } else {
            root.to_path_buf()
        };
        let game = root.join("game");
        if !game.join("csgo").is_dir() {
            bail!(
                "{} is not a CS2 install (no game/csgo directory)",
                root.display()
            );
        }
        let build = std::fs::read_to_string(game.join("csgo/steam.inf"))
            .ok()
            .and_then(|t| build_from_steam_inf(&t));
        let has_tools = game.join("bin/win64/tools/hammer.dll").is_file()
            && game.join("bin/sdkenginetools.txt").is_file();
        let has_game_binary = match Os::host() {
            Os::Windows => game.join("bin/win64/cs2.exe").is_file(),
            Os::Linux => game.join("cs2.sh").is_file(),
        };
        Ok(Install {
            root,
            library,
            build,
            has_tools,
            has_game_binary,
        })
    }
}

/// The game build the shipped signatures and bridge pins were made for (`sigs/CURRENT_BUILD`, the
/// same number as `PIN_GAME_BUILD` in the bridge's generated pins; a test holds them equal).
pub fn target_build() -> &'static str {
    include_str!("../../../sigs/CURRENT_BUILD").trim()
}

/// The finding for an install whose build is not [`target_build`]: allowed, with the note.
pub fn build_finding(build: Option<&str>) -> Option<crate::findings::Finding> {
    use crate::findings::{Code, Finding, Severity};
    let want = target_build();
    match build {
        Some(b) if b == want => None,
        b => Some(
            Finding::new(
                Code::BuildMismatch,
                Severity::Warn,
                format!(
                    "this install is game build {}, this release's signatures and bridge pins are for build {want}",
                    b.unwrap_or("(unknown: no game/csgo/steam.inf)")
                ),
            )
            .advice("the session still starts; the bridge refuses every game module it cannot verify, so the tools that need one answer not_loaded/mismatch until a release for this build exists"),
        ),
    }
}

/// `ClientVersion=2000940` from `steam.inf`.
pub fn build_from_steam_inf(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let l = l.trim();
        l.strip_prefix("ClientVersion=")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

fn kv1(text: &str) -> anyhow::Result<cs2_kv1::Document> {
    // Steam writes Windows paths with escaped backslashes (`"C:\\SteamLibrary"`).
    let opts = cs2_kv1::text::ReadOptions {
        escapes: true,
        ..Default::default()
    };
    cs2_kv1::text::parse_with(text, &opts).map_err(|e| anyhow::anyhow!("{e}"))
}

/// The library paths in `libraryfolders.vdf` (both the current `"0" { "path" ... }` form and the
/// old `"1" "D:\\Lib"` form), in file order.
pub fn libraries_from_vdf(text: &str) -> anyhow::Result<Vec<PathBuf>> {
    let doc = kv1(text)?;
    let root = doc
        .value
        .as_object()
        .context("libraryfolders.vdf: root is not an object")?;
    let mut out = Vec::new();
    for kv in &root.entries {
        if !kv.key.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        match &kv.value {
            cs2_kv1::Value::Object(o) => {
                if let Some(p) = o.get("path").and_then(cs2_kv1::Value::as_str) {
                    out.push(PathBuf::from(p));
                }
            }
            cs2_kv1::Value::String(p) => out.push(PathBuf::from(p)),
            _ => {}
        }
    }
    Ok(out)
}

/// `installdir` of an `appmanifest_730.acf` (`"AppState" { "installdir" "Counter-Strike Global Offensive" }`).
pub fn installdir_from_acf(text: &str) -> anyhow::Result<String> {
    let doc = kv1(text)?;
    let root = doc
        .value
        .as_object()
        .context("appmanifest: root is not an object")?;
    root.get("installdir")
        .and_then(cs2_kv1::Value::as_str)
        .map(str::to_string)
        .context("appmanifest has no installdir")
}

/// Steam client roots to look in, most likely first. Only directories that exist are returned.
pub fn steam_roots() -> Vec<PathBuf> {
    let mut v = Vec::new();
    #[cfg(windows)]
    {
        v.extend(crate::win::steam_path_from_registry());
        for p in [r"C:\Program Files (x86)\Steam", r"C:\Program Files\Steam"] {
            v.push(PathBuf::from(p));
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            for rel in [
                ".local/share/Steam",
                ".steam/steam",
                ".steam/root",
                ".var/app/com.valvesoftware.Steam/.local/share/Steam",
            ] {
                v.push(home.join(rel));
            }
        }
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    v.into_iter()
        .filter(|p| p.join("steamapps").is_dir())
        .filter(|p| {
            let c = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            if seen.contains(&c) {
                false
            } else {
                seen.push(c);
                true
            }
        })
        .collect()
}

/// Libraries named by the Steam roots' `libraryfolders.vdf`, plus the roots themselves (the root
/// is always library 0, but an old vdf may not say so).
pub fn libraries(steam_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in steam_roots {
        out.push(root.clone());
        if let Ok(t) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            if let Ok(libs) = libraries_from_vdf(&t) {
                out.extend(libs);
            }
        }
    }
    out
}

/// The CS2 install in `library` (its `steamapps/appmanifest_730.acf`), if it has one.
pub fn install_in_library(library: &Path) -> Option<PathBuf> {
    let acf = std::fs::read_to_string(library.join("steamapps/appmanifest_730.acf")).ok()?;
    let dir = installdir_from_acf(&acf).ok()?;
    let root = library.join("steamapps").join("common").join(dir);
    root.join("game").is_dir().then_some(root)
}

/// Resolve the install: `--game-root` wins, then every `--library`, then the Steam libraries.
pub fn discover(game_root: Option<&Path>, extra_libraries: &[PathBuf]) -> anyhow::Result<Install> {
    discover_with_others(game_root, extra_libraries).map(|(i, _)| i)
}

/// [`discover`], plus the roots of every other CS2 install the libraries name (two Steam
/// libraries can both hold one; the launcher says which one it took).
pub fn discover_with_others(game_root: Option<&Path>, extra_libraries: &[PathBuf]) -> anyhow::Result<(Install, Vec<PathBuf>)> {
    if let Some(root) = game_root {
        return Ok((Install::at(root, None)?, Vec::new()));
    }
    let mut libs: Vec<PathBuf> = extra_libraries.to_vec();
    libs.extend(libraries(&steam_roots()));
    let mut found: Vec<(PathBuf, PathBuf)> = Vec::new();
    for lib in &libs {
        if let Some(root) = install_in_library(lib) {
            let c = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
            if !found.iter().any(|(r, _)| std::fs::canonicalize(r).unwrap_or_else(|_| r.clone()) == c) {
                found.push((root, lib.clone()));
            }
        }
    }
    if let Some((root, lib)) = found.first() {
        let others = found[1..].iter().map(|(r, _)| r.clone()).collect();
        return Ok((Install::at(root, Some(lib.clone()))?, others));
    }
    bail!(
        "no CS2 install found (looked in {} Steam librar{}: {}); pass --game-root <.../Counter-Strike Global Offensive> or --library <steam library>",
        libs.len(),
        if libs.len() == 1 { "y" } else { "ies" },
        libs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_build_is_the_bridge_pins_build_and_another_build_is_a_warning() {
        let pins = include_str!("../../cs2-bridge/src/generated/pins.rs");
        assert!(pins.contains(&format!("pub const PIN_GAME_BUILD: &str = \"{}\";", target_build())));
        assert!(build_finding(Some(target_build())).is_none());
        let f = build_finding(Some("1")).unwrap();
        assert_eq!(f.severity, crate::findings::Severity::Warn);
        assert!(build_finding(None).is_some());
    }

    #[test]
    fn steam_inf_build() {
        let t = "ClientVersion=2000940\r\nServerVersion=2000940\r\nPatchVersion=1.41.9.5\r\n";
        assert_eq!(build_from_steam_inf(t).as_deref(), Some("2000940"));
        assert_eq!(build_from_steam_inf("PatchVersion=1\n"), None);
    }

    #[test]
    fn libraryfolders_current_and_old_forms() {
        let t = r#""libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"label"		""
		"apps"
		{
			"228980"		"411046883"
		}
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
		"apps"
		{
			"730"		"64000000000"
		}
	}
}
"#;
        assert_eq!(
            libraries_from_vdf(t).unwrap(),
            [
                PathBuf::from(r"C:\Program Files (x86)\Steam"),
                PathBuf::from(r"D:\SteamLibrary")
            ]
        );
        let old = "\"LibraryFolders\"\n{\n\t\"TimeNextStatsReport\"\t\"1\"\n\t\"ContentStatsID\"\t\"2\"\n\t\"1\"\t\"/home/u/games\"\n}\n";
        assert_eq!(libraries_from_vdf(old).unwrap(), [PathBuf::from("/home/u/games")]);
    }

    #[test]
    fn appmanifest_installdir() {
        let t = "\"AppState\"\n{\n\t\"appid\"\t\t\"730\"\n\t\"name\"\t\t\"Counter-Strike 2\"\n\t\"installdir\"\t\t\"Counter-Strike Global Offensive\"\n}\n";
        assert_eq!(installdir_from_acf(t).unwrap(), "Counter-Strike Global Offensive");
        assert!(installdir_from_acf("\"AppState\"\n{\n}\n").is_err());
    }

    #[test]
    fn a_fixture_library_resolves_to_its_install() {
        let base = crate::testutil::scratch("install-lib");
        let root = base.join("steamapps/common/Counter-Strike Global Offensive");
        std::fs::create_dir_all(root.join("game/csgo")).unwrap();
        std::fs::write(root.join("game/csgo/steam.inf"), "ClientVersion=123\n").unwrap();
        std::fs::write(
            base.join("steamapps/appmanifest_730.acf"),
            "\"AppState\"\n{\n\t\"installdir\"\t\"Counter-Strike Global Offensive\"\n}\n",
        )
        .unwrap();
        let found = discover(None, std::slice::from_ref(&base)).unwrap();
        assert_eq!(found.root, root);
        assert_eq!(found.build.as_deref(), Some("123"));
        assert!(!found.has_tools);
        // the game dir is accepted for the root as well
        assert_eq!(Install::at(&root.join("game"), None).unwrap().root, root);
    }
}
