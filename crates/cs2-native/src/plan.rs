//! What goes into the tree, as data: the build artefacts we copy, the text we generate, the one
//! edit. Pure apart from reading the artefacts' presence, so the per-OS, per-mode file sets are
//! unit-tested here and applied by `manifest` (in place) or `overlay` (into the upper dir).

use crate::install::Os;
use crate::layout;
use anyhow::{bail, Context};
use std::path::{Path, PathBuf};

/// `--steam`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SteamMode {
    /// gbe_fork Steam emulator: offline, no account
    Gbe,
    /// the user's own running Steam client
    Client,
}

/// Where the build artefacts come from.
#[derive(Debug, Clone)]
pub struct Artifacts {
    /// `bridge/build-rs.sh`'s output: `win64/{cs2_bridge.dll,cs2mcp_launch.exe,cs2mcp_mmload.dll}`,
    /// `linuxsteamrt64/{libcs2_bridge.so,libcs2mcp_mmload.so}`
    pub bridge: PathBuf,
    /// the Metamod builds: `win64/addons/metamod/...`, `linux/addons/metamod/...`
    pub metamod: PathBuf,
    /// gbe_fork binaries: `win64/steam_api64.dll`, `linux64/{libsteam_api.so,steamclient.so}`
    pub gbe: PathBuf,
    /// the capture layer: `win64/{VkLayer_cs2mcp_capture.json,cs2_vk_capture.dll}`,
    /// `linux64/{...json,libcs2_vk_capture.so}` (not copied into the tree; `capture.rs`)
    pub vklayer: PathBuf,
}

impl Artifacts {
    /// Defaults: `<package dir>/payload/{bridge-rs,metamod,gbe,vklayer}` when there is a package
    /// (`selfexe::package_dir`: next to the exe, or `$APPDIR/opt/cs2mcp` in an AppImage), else
    /// this repo's `target/{bridge-rs,metamod,gbe,vklayer}`. gbe falls back to `fetched`
    /// (`cs2mcp-native fetch-gbe`'s `<state root>/gbe`) when the bundled dir lacks this OS's files.
    pub fn resolve(
        bridge: Option<PathBuf>,
        metamod: Option<PathBuf>,
        gbe: Option<PathBuf>,
        fetched: Option<PathBuf>,
        vklayer: Option<PathBuf>,
    ) -> Artifacts {
        let payload = crate::selfexe::package_dir().map(|d| d.join("payload")).filter(|p| p.is_dir());
        let base = payload.unwrap_or_else(|| {
            PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target"))
        });
        Artifacts {
            bridge: bridge.unwrap_or_else(|| base.join("bridge-rs")),
            metamod: metamod.unwrap_or_else(|| base.join("metamod")),
            gbe: gbe.unwrap_or_else(|| pick_gbe(base.join("gbe"), fetched, Os::host())),
            vklayer: vklayer.unwrap_or_else(|| base.join("vklayer")),
        }
    }
}

/// The bundled gbe dir if it has `os`'s files, else the fetched one if that has them, else the
/// bundled one (so the missing-artefact error names the usual place).
pub fn pick_gbe(bundled: PathBuf, fetched: Option<PathBuf>, os: Os) -> PathBuf {
    if crate::gbe::has_files(&bundled, os) {
        return bundled;
    }
    match fetched {
        Some(f) if crate::gbe::has_files(&f, os) => f,
        _ => bundled,
    }
}

/// Where an op's bytes come from.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// a build artefact, copied byte for byte
    Copy(PathBuf),
    /// generated text
    Text(String),
    /// `layout::sdkenginetools_edit` over the file's current bytes
    EditSdkEngineTools,
}

/// One file to put at `rel` (forward slashes, relative to the install root). Parent directories
/// are created as needed (and recorded as created).
#[derive(Debug, Clone, PartialEq)]
pub struct Op {
    pub rel: String,
    pub source: Source,
}

/// The whole change set.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub ops: Vec<Op>,
    /// files the **game** writes that a user tree may already have; backed up and put back
    pub runtime: Vec<String>,
}

/// The inputs `build` needs besides the artefacts.
#[derive(Debug, Clone)]
pub struct PlanInput<'a> {
    pub os: Os,
    pub steam: SteamMode,
    pub tools: bool,
    /// the install root (only read: the original Steam API library for `steam_interfaces.txt`)
    pub root: &'a Path,
    /// gbe's `local_save_path` (outside the tree)
    pub gbe_save_dir: &'a Path,
}

fn copy(rel: &str, src: PathBuf) -> Op {
    Op {
        rel: rel.to_string(),
        source: Source::Copy(src),
    }
}

fn text(rel: &str, t: impl Into<String>) -> Op {
    Op {
        rel: rel.to_string(),
        source: Source::Text(t.into()),
    }
}

/// The install's Steam API library for `os` (relative to the root).
pub fn steam_api_rel(os: Os) -> &'static str {
    match os {
        Os::Windows => "game/bin/win64/steam_api64.dll",
        Os::Linux => "game/bin/linuxsteamrt64/libsteam_api.so",
    }
}

/// Build the change set. Every artefact must exist; the error names the missing file and what
/// builds it.
pub fn build(input: &PlanInput<'_>, art: &Artifacts) -> anyhow::Result<Plan> {
    let mut ops = Vec::new();
    match input.os {
        Os::Windows => {
            let b = art.bridge.join("win64");
            ops.push(copy("game/bin/win64/tools/cs2_bridge.dll", b.join("cs2_bridge.dll")));
            ops.push(copy("game/bin/win64/cs2mcp_launch.exe", b.join("cs2mcp_launch.exe")));
            ops.push(copy("game/bin/win64/cs2mcp_mmload.dll", b.join("cs2mcp_mmload.dll")));
            let mm = art.metamod.join("win64/addons/metamod");
            for f in ["bin/win64/server.dll", "bin/win64/metamod.2.cs2.dll", "metaplugins.ini"] {
                ops.push(copy(&format!("game/csgo/addons/metamod/{f}"), mm.join(f)));
            }
            ops.push(text("game/csgo/addons/metamod/cs2mcp.vdf", layout::WINDOWS_VDF));
            if input.tools {
                ops.push(Op {
                    rel: "game/bin/sdkenginetools.txt".into(),
                    source: Source::EditSdkEngineTools,
                });
            }
        }
        Os::Linux => {
            let b = art.bridge.join("linuxsteamrt64");
            let dst = "game/csgo/addons/cs2mcp/bin/linuxsteamrt64";
            ops.push(copy(&format!("{dst}/libcs2_bridge.so"), b.join("libcs2_bridge.so")));
            ops.push(copy(&format!("{dst}/libcs2mcp_mmload.so"), b.join("libcs2mcp_mmload.so")));
            // the whole Linux Metamod build, as install-rs.sh --platform linux copies it
            let mm = art.metamod.join("linux/addons/metamod");
            let mut files = Vec::new();
            walk_files(&mm, &mm, &mut files)
                .with_context(|| format!("Metamod build {} (bridge/metamod/fetch-metamod.sh linux fetches it)", mm.display()))?;
            files.sort();
            for f in files {
                ops.push(copy(&format!("game/csgo/addons/metamod/{f}"), mm.join(&f)));
            }
            ops.push(text("game/csgo/addons/metamod/cs2mcp.vdf", layout::LINUX_VDF));
            if input.tools {
                bail!("--tools needs the Windows game (the Workshop Tools are Windows-only)");
            }
        }
    }

    let save = input.gbe_save_dir.to_string_lossy().into_owned();
    match (input.steam, input.os) {
        (SteamMode::Gbe, os) => {
            let api_rel = steam_api_rel(os);
            let original = std::fs::read(input.root.join(api_rel))
                .with_context(|| format!("reading the game's Steam API library {api_rel}"))?;
            if layout::is_gbe_library(&original) {
                bail!(
                    "{api_rel} in this install is already a Steam emulator (gbe); restore Valve's file \
                     (Steam: Verify integrity of game files) so steam_interfaces.txt can be generated from it"
                );
            }
            let bin = match os {
                Os::Windows => "game/bin/win64",
                Os::Linux => "game/bin/linuxsteamrt64",
            };
            match os {
                Os::Windows => ops.push(copy(api_rel, art.gbe.join("win64/steam_api64.dll"))),
                Os::Linux => {
                    ops.push(copy(api_rel, art.gbe.join("linux64/libsteam_api.so")));
                    // the emulator dlopens its steamclient.so from its own directory
                    ops.push(copy(&format!("{bin}/steamclient.so"), art.gbe.join("linux64/steamclient.so")));
                }
            }
            let s = format!("{bin}/steam_settings");
            ops.push(text(&format!("{s}/configs.main.ini"), layout::GBE_CONFIGS_MAIN));
            ops.push(text(&format!("{s}/configs.user.ini"), layout::gbe_configs_user(&save)));
            ops.push(text(&format!("{s}/steam_appid.txt"), layout::STEAM_APPID));
            ops.push(text(&format!("{s}/steam_interfaces.txt"), layout::steam_interfaces(&original)));
        }
        (SteamMode::Client, Os::Windows) => {
            // Nothing for Steam: a direct cs2.exe with the client logged in is adopted, not
            // relaunched (windows-native.md §4.3, seen live: workflows/updates/windows-vm-steam.md).
            if let Ok(api) = std::fs::read(input.root.join(steam_api_rel(Os::Windows))) {
                if layout::is_gbe_library(&api) {
                    bail!("--steam client: this install's steam_api64.dll is a Steam emulator, not Valve's; verify the game files in Steam first");
                }
            }
        }
        (SteamMode::Client, Os::Linux) => {}
    }

    for op in &ops {
        if let Source::Copy(src) = &op.source {
            if !src.is_file() {
                bail!(
                    "missing artefact {} (for {}): {}",
                    src.display(),
                    op.rel,
                    hint_for(&op.rel)
                );
            }
        }
    }
    let mut runtime = vec!["game/csgo/console.log".to_string()];
    if input.os == Os::Windows {
        // CS2 writes `730` here itself about 1 s after every start (windows-vm-steam.md)
        runtime.push("game/bin/win64/steam_appid.txt".into());
    }
    Ok(Plan { ops, runtime })
}

fn hint_for(rel: &str) -> &'static str {
    if rel.contains("steam_api") || rel.contains("steamclient") {
        "run `cs2mcp-native fetch-gbe` (Linux) or crates/cs2-native/scripts/stage-gbe.sh, or pass --gbe-dir"
    } else if rel.contains("addons/metamod") {
        "bridge/metamod/fetch-metamod.sh linux|win64 fetches it, or pass --metamod-dir"
    } else {
        "build it with bridge/build-rs.sh, or pass --bridge-dir"
    }
}

/// Every file under `dir`, relative to `base`, with forward slashes.
pub(crate) fn walk_files(base: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if e.file_type()?.is_dir() {
            walk_files(base, &p, out)?;
        } else {
            let rel = p.strip_prefix(base).unwrap_or(&p);
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{put, scratch};

    fn fixture(name: &str) -> (PathBuf, Artifacts) {
        let base = scratch(name);
        let art = Artifacts {
            bridge: base.join("bridge-rs"),
            metamod: base.join("metamod"),
            gbe: base.join("gbe"),
            vklayer: base.join("vklayer"),
        };
        for f in [
            "bridge-rs/win64/cs2_bridge.dll",
            "bridge-rs/win64/cs2mcp_launch.exe",
            "bridge-rs/win64/cs2mcp_mmload.dll",
            "bridge-rs/linuxsteamrt64/libcs2_bridge.so",
            "bridge-rs/linuxsteamrt64/libcs2mcp_mmload.so",
            "metamod/win64/addons/metamod/bin/win64/server.dll",
            "metamod/win64/addons/metamod/bin/win64/metamod.2.cs2.dll",
            "metamod/win64/addons/metamod/metaplugins.ini",
            "metamod/linux/addons/metamod/bin/linuxsteamrt64/libserver.so",
            "metamod/linux/addons/metamod/metaplugins.ini",
            "gbe/win64/steam_api64.dll",
            "gbe/linux64/libsteam_api.so",
            "gbe/linux64/steamclient.so",
        ] {
            put(&base.join(f), f.as_bytes());
        }
        let root = base.join("install");
        put(&root.join("game/bin/win64/steam_api64.dll"), b"..SteamClient017\0SteamUser023\0");
        put(&root.join("game/bin/linuxsteamrt64/libsteam_api.so"), b"SteamUser023\0");
        (root, art)
    }

    fn rels(p: &Plan) -> Vec<&str> {
        p.ops.iter().map(|o| o.rel.as_str()).collect()
    }

    #[test]
    fn windows_gbe_tools_is_the_install_rs_layout_plus_gbe() {
        let (root, art) = fixture("plan-win-gbe");
        let save = root.join("saves");
        let input = PlanInput { os: Os::Windows, steam: SteamMode::Gbe, tools: true, root: &root, gbe_save_dir: &save };
        let p = build(&input, &art).unwrap();
        assert_eq!(
            rels(&p),
            [
                "game/bin/win64/tools/cs2_bridge.dll",
                "game/bin/win64/cs2mcp_launch.exe",
                "game/bin/win64/cs2mcp_mmload.dll",
                "game/csgo/addons/metamod/bin/win64/server.dll",
                "game/csgo/addons/metamod/bin/win64/metamod.2.cs2.dll",
                "game/csgo/addons/metamod/metaplugins.ini",
                "game/csgo/addons/metamod/cs2mcp.vdf",
                "game/bin/sdkenginetools.txt",
                "game/bin/win64/steam_api64.dll",
                "game/bin/win64/steam_settings/configs.main.ini",
                "game/bin/win64/steam_settings/configs.user.ini",
                "game/bin/win64/steam_settings/steam_appid.txt",
                "game/bin/win64/steam_settings/steam_interfaces.txt",
            ]
        );
        let ifaces = p.ops.iter().find(|o| o.rel.ends_with("steam_interfaces.txt")).unwrap();
        assert_eq!(ifaces.source, Source::Text("SteamClient017\nSteamUser023\n".into()));
        assert_eq!(p.runtime, ["game/csgo/console.log", "game/bin/win64/steam_appid.txt"]);
    }

    #[test]
    fn windows_client_adds_no_steam_files_and_refuses_an_emulated_install() {
        let (root, art) = fixture("plan-win-client");
        let save = root.join("saves");
        let input = PlanInput { os: Os::Windows, steam: SteamMode::Client, tools: false, root: &root, gbe_save_dir: &save };
        let p = build(&input, &art).unwrap();
        assert!(!rels(&p).iter().any(|r| r.contains("steam") || r.contains("sdkenginetools")));
        put(&root.join("game/bin/win64/steam_api64.dll"), b"configs.main.ini");
        assert!(build(&input, &art).unwrap_err().to_string().contains("Steam emulator"));
        let gbe = PlanInput { steam: SteamMode::Gbe, ..input };
        assert!(build(&gbe, &art).unwrap_err().to_string().contains("already a Steam emulator"));
    }

    #[test]
    fn linux_gbe_copies_the_whole_metamod_build_and_refuses_tools() {
        let (root, art) = fixture("plan-linux-gbe");
        let save = root.join("saves");
        let input = PlanInput { os: Os::Linux, steam: SteamMode::Gbe, tools: false, root: &root, gbe_save_dir: &save };
        let p = build(&input, &art).unwrap();
        let r = rels(&p);
        assert!(r.contains(&"game/csgo/addons/metamod/bin/linuxsteamrt64/libserver.so"));
        assert!(r.contains(&"game/csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so"));
        assert!(r.contains(&"game/bin/linuxsteamrt64/steamclient.so"));
        assert!(r.contains(&"game/bin/linuxsteamrt64/steam_settings/steam_interfaces.txt"));
        assert!(build(&PlanInput { tools: true, ..input }, &art).is_err());
    }

    #[test]
    fn gbe_prefers_the_bundled_dir_then_the_fetched_one() {
        let base = scratch("plan-pick-gbe");
        let (bundled, fetched) = (base.join("payload/gbe"), base.join("state/gbe"));
        let pick = |os| pick_gbe(bundled.clone(), Some(fetched.clone()), os);
        assert_eq!(pick(Os::Linux), bundled, "neither has the files: the bundled path, for the error");
        put(&fetched.join("linux64/libsteam_api.so"), b"x");
        put(&fetched.join("linux64/steamclient.so"), b"x");
        assert_eq!(pick(Os::Linux), fetched);
        assert_eq!(pick(Os::Windows), bundled, "fetch-gbe has no win64");
        assert_eq!(pick_gbe(bundled.clone(), None, Os::Linux), bundled);
        put(&bundled.join("linux64/libsteam_api.so"), b"x");
        assert_eq!(pick(Os::Linux), fetched, "a half-staged bundle does not win");
        put(&bundled.join("linux64/steamclient.so"), b"x");
        assert_eq!(pick(Os::Linux), bundled);
    }

    #[test]
    fn a_missing_artefact_names_the_builder() {
        let (root, mut art) = fixture("plan-missing");
        art.bridge = root.join("nope");
        let save = root.join("saves");
        let input = PlanInput { os: Os::Windows, steam: SteamMode::Client, tools: false, root: &root, gbe_save_dir: &save };
        let e = build(&input, &art).unwrap_err().to_string();
        assert!(e.contains("bridge/build-rs.sh"), "{e}");
    }
}
