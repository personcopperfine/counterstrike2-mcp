//! Files of ours (or of an older install of ours) in a tree that no manifest records:
//!
//! * the launcher's state dir was deleted while the tree was prepared: our files are still in it,
//!   and a new `prepare` would see them as the user's own (identical bytes are not recorded), so
//!   they would never be removed;
//! * `cargo bridge install` (formerly `bridge/install-rs.sh`) installed the bridge into this tree
//!   (its current layout, or the shell installer's old gameinfo-era one: `game/metamod/`, `content/metamod/`, the `Game metamod` search path, the
//!   second bridge copy under `game/csgo/addons/cs2mcp/` on Windows, the Linux VSP loader vdfs);
//! * the client image's `setup_gbe.sh` replaced the Linux `libsteam_api.so` with the emulator and
//!   linked `steam_settings` (its own backup of Valve's file is `~/.steam/gbe_orig_libsteam_api.so`).
//!
//! [`scan`] reports them; nothing is removed without [`cleanup`], which the user runs on purpose
//! (`cs2mcp-native cleanup --yes`, the GUI's "Clean up" after a confirmation). Cleanup never
//! deletes: it moves what it takes away into `<state root>/cleanup-<ts>/`, un-edits
//! `sdkenginetools.txt` only when that gives a byte-exact Valve file back, and leaves
//! `gameinfo.gi` to Steam's Verify integrity (it is never edited here).

use crate::findings::{Code, Finding, Severity, VERIFY_ADVICE};
use crate::install::Os;
use crate::layout;
use crate::manifest::{hash_bytes, hash_file, under, write_atomic};
use crate::plan::{steam_api_rel, Artifacts};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Every path this launcher (or `cargo bridge install`) writes, with the bytes it would write today.
#[derive(Debug, Clone, Default)]
pub struct Known {
    /// rel path -> acceptable sha256s
    pub files: BTreeMap<String, BTreeSet<String>>,
    /// paths nobody but our tools writes (a different build there is still ours)
    pub exclusive: BTreeSet<String>,
    /// Metamod's files (a user may have the very same build installed on their own)
    pub metamod: BTreeSet<String>,
}

impl Known {
    fn add(&mut self, rel: &str, bytes_hash: Option<String>, exclusive: bool) {
        let e = self.files.entry(rel.to_string()).or_default();
        if let Some(h) = bytes_hash {
            e.insert(h);
        }
        if exclusive {
            self.exclusive.insert(rel.to_string());
        }
    }
    fn add_file(&mut self, rel: &str, src: &Path, exclusive: bool) {
        self.add(rel, hash_file(src).ok().flatten().map(|m| m.sha256), exclusive);
    }
    fn add_text(&mut self, rel: &str, text: &str) {
        self.add(rel, Some(hash_bytes(text.as_bytes()).sha256), true);
    }
}

/// The steam_settings dir gbe reads next to the Steam API library.
pub fn steam_settings_rel(os: Os) -> &'static str {
    match os {
        Os::Windows => "game/bin/win64/steam_settings",
        Os::Linux => "game/bin/linuxsteamrt64/steam_settings",
    }
}

/// [`Known`] for `os` from the payload `art` and the generated texts.
pub fn known(os: Os, art: &Artifacts, gbe_save_dir: &Path) -> Known {
    let mut k = Known::default();
    let save = gbe_save_dir.to_string_lossy().into_owned();
    match os {
        Os::Windows => {
            let b = art.bridge.join("win64");
            k.add_file("game/bin/win64/tools/cs2_bridge.dll", &b.join("cs2_bridge.dll"), true);
            k.add_file("game/bin/win64/cs2mcp_launch.exe", &b.join("cs2mcp_launch.exe"), true);
            k.add_file("game/bin/win64/cs2mcp_mmload.dll", &b.join("cs2mcp_mmload.dll"), true);
            let mm = art.metamod.join("win64/addons/metamod");
            for f in ["bin/win64/server.dll", "bin/win64/metamod.2.cs2.dll", "metaplugins.ini"] {
                let rel = format!("game/csgo/addons/metamod/{f}");
                k.add_file(&rel, &mm.join(f), false);
                k.metamod.insert(rel);
            }
            k.add_text("game/csgo/addons/metamod/cs2mcp.vdf", layout::WINDOWS_VDF);
        }
        Os::Linux => {
            let b = art.bridge.join("linuxsteamrt64");
            let dst = "game/csgo/addons/cs2mcp/bin/linuxsteamrt64";
            k.add_file(&format!("{dst}/libcs2_bridge.so"), &b.join("libcs2_bridge.so"), true);
            k.add_file(&format!("{dst}/libcs2mcp_mmload.so"), &b.join("libcs2mcp_mmload.so"), true);
            let mm = art.metamod.join("linux/addons/metamod");
            let mut files = Vec::new();
            let _ = crate::plan::walk_files(&mm, &mm, &mut files);
            for f in files {
                let rel = format!("game/csgo/addons/metamod/{f}");
                k.add_file(&rel, &mm.join(&f), false);
                k.metamod.insert(rel);
            }
            k.add_text("game/csgo/addons/metamod/cs2mcp.vdf", layout::LINUX_VDF);
        }
    }
    let s = steam_settings_rel(os);
    k.add_text(&format!("{s}/configs.main.ini"), layout::GBE_CONFIGS_MAIN);
    k.add_text(&format!("{s}/configs.user.ini"), &layout::gbe_configs_user(&save));
    k.add_text(&format!("{s}/steam_appid.txt"), layout::STEAM_APPID);
    // steam_interfaces.txt is generated from the original library (no fixed bytes): it is ours
    // only next to the others, and `plan_cleanup` takes it along with them
    k
}

/// What [`scan`] saw, path by path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    /// byte-identical to what we write today
    pub exact: Vec<String>,
    /// an exclusive path of ours with other bytes (another build)
    pub other_build: Vec<String>,
    /// Metamod files identical to our payload's
    pub metamod_exact: Vec<String>,
    /// `sdkenginetools.txt` carrying our entry
    pub edited_tools_list: bool,
    /// the Steam API library is the gbe emulator
    pub emulated_steam_api: bool,
    /// `~/.steam/gbe_orig_libsteam_api.so` (setup_gbe.sh's copy of Valve's file), when usable
    pub gbe_orig: Option<PathBuf>,
    /// `steam_settings` is a symlink (setup_gbe.sh)
    pub steam_settings_link: bool,
    /// old-layout paths (install-rs.sh's gameinfo era, the legacy second bridge copy)
    pub legacy: Vec<String>,
    /// gameinfo.gi lines an old install-rs.sh wrote
    pub gameinfo_lines: Vec<String>,
}

impl Seen {
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty()
            && self.other_build.is_empty()
            && !self.edited_tools_list
            && !self.emulated_steam_api
            && !self.steam_settings_link
            && self.legacy.is_empty()
            && self.gameinfo_lines.is_empty()
    }
    /// Are the Metamod matches ours? Only next to another file of ours: on their own they may be
    /// the user's own install of the same official build.
    fn metamod_is_ours(&self) -> bool {
        !self.metamod_exact.is_empty() && (self.exact.iter().chain(&self.other_build).any(|p| !p.contains("/addons/metamod/") || p.ends_with("cs2mcp.vdf")))
    }
}

/// Old layouts, relative to the install root.
fn legacy_paths(os: Os) -> &'static [&'static str] {
    match os {
        Os::Windows => &["game/csgo/addons/cs2mcp/bin/win64/cs2_bridge.dll", "game/metamod", "content/metamod"],
        Os::Linux => &["game/csgo/addons/metamod.vdf", "game/csgo/addons/metamod_x64.vdf"],
    }
}

/// The gameinfo.gi search-path lines old install-rs.sh versions added.
fn gameinfo_markers(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| {
            let mut w = l.split_whitespace();
            w.next() == Some("Game") && matches!(w.next(), Some("metamod") | Some("csgo/addons/metamod")) && w.next().is_none()
        })
        .map(|l| format!("game/csgo/gameinfo.gi: `{l}`"))
        .collect()
}

/// Valve's `libsteam_api.so` as the client image's setup_gbe.sh kept it (`$HOME/.steam/`), when
/// that file is there and is not an emulator itself.
pub fn gbe_orig(home: &Path) -> Option<PathBuf> {
    let orig = home.join(".steam/gbe_orig_libsteam_api.so");
    std::fs::read(&orig).is_ok_and(|b| !layout::is_gbe_library(&b)).then_some(orig)
}

/// Look at `root` for files of ours that no manifest records. `home` is where setup_gbe.sh kept
/// its backup (`$HOME`).
pub fn look(root: &Path, os: Os, k: &Known, home: Option<&Path>) -> Seen {
    let mut s = Seen {
        steam_settings_link: std::fs::symlink_metadata(under(root, steam_settings_rel(os))).is_ok_and(|m| m.file_type().is_symlink()),
        ..Default::default()
    };
    for (rel, hashes) in &k.files {
        // setup_gbe.sh's link points out of the install (into $HOME): what is there is not in the tree
        if s.steam_settings_link && rel.starts_with(steam_settings_rel(os)) {
            continue;
        }
        let p = under(root, rel);
        if !p.is_file() {
            continue;
        }
        let h = hash_file(&p).ok().flatten().map(|m| m.sha256);
        let exact = h.as_ref().is_some_and(|h| hashes.contains(h));
        if k.metamod.contains(rel) {
            if exact {
                s.metamod_exact.push(rel.clone());
            }
        } else if exact {
            s.exact.push(rel.clone());
        } else if k.exclusive.contains(rel) {
            s.other_build.push(rel.clone());
        }
    }
    if let Ok(t) = std::fs::read_to_string(under(root, "game/bin/sdkenginetools.txt")) {
        s.edited_tools_list = layout::has_bridge_entry(&t);
    }
    if let Ok(b) = std::fs::read(under(root, steam_api_rel(os))) {
        s.emulated_steam_api = layout::is_gbe_library(&b);
    }
    if os == Os::Linux {
        s.gbe_orig = home.and_then(gbe_orig);
    }
    for rel in legacy_paths(os) {
        if std::fs::symlink_metadata(under(root, rel)).is_ok() {
            s.legacy.push(rel.to_string());
        }
    }
    if let Ok(t) = std::fs::read_to_string(under(root, "game/csgo/gameinfo.gi")) {
        s.gameinfo_lines = gameinfo_markers(&t);
    }
    s
}

/// The findings for what [`look`] saw.
pub fn findings(s: &Seen, os: Os) -> Vec<Finding> {
    let mut v = Vec::new();
    let mut ours: Vec<String> = s.exact.to_vec();
    ours.extend(s.other_build.iter().map(|p| format!("{p} (another build)")));
    if s.metamod_is_ours() {
        ours.extend(s.metamod_exact.iter().cloned());
    }
    if s.edited_tools_list {
        ours.push("game/bin/sdkenginetools.txt (carries our cs2_bridge entry)".into());
    }
    if !ours.is_empty() {
        v.push(
            Finding::new(
                Code::Orphans,
                Severity::Decision,
                "files of cs2-mcp are in this install, but no manifest of this launcher records them (its state dir was deleted while the tree was prepared, or `cargo bridge install` installed them)",
            )
            .paths(ours)
            .advice("`cs2mcp-native cleanup --yes` (GUI: Clean up) moves them out of the install (into the state dir's cleanup-<time>/) and removes our sdkenginetools.txt entry")
            .advice("installed with `cargo bridge install` (or the older bridge/install-rs.sh)? `cargo bridge install uninstall` restores its own backups")
            .advice("--accept-conflicts (GUI: Continue anyway) keeps them: they are treated as the tree's own and left as found after the session"),
        );
    } else if !s.metamod_exact.is_empty() {
        v.push(
            Finding::new(Code::Orphans, Severity::Info, "a Metamod build identical to ours is already installed (yours?); it is left as it is")
                .paths(s.metamod_exact.iter().cloned()),
        );
    }
    if s.emulated_steam_api {
        let f = Finding::new(
            Code::ReplacedGameFile,
            if s.gbe_orig.is_some() { Severity::Decision } else { Severity::Blocker },
            "the game's Steam API library is the gbe Steam emulator, not Valve's file",
        )
        .paths([steam_api_rel(os).to_string()]);
        v.push(match &s.gbe_orig {
            Some(o) => f
                .advice(format!("the image's setup_gbe.sh kept Valve's file at {}: `cs2mcp-native cleanup --yes` puts it back", o.display()))
                .advice("--accept-conflicts keeps the emulator in place: --steam gbe reads the interfaces from that copy, and the session puts the image's file back afterwards"),
            None => f.advice(VERIFY_ADVICE),
        });
    }
    let mut legacy: Vec<String> = s.legacy.clone();
    if s.steam_settings_link {
        legacy.push("steam_settings is a symlink (the client image's setup_gbe.sh)".into());
    }
    if !legacy.is_empty() || !s.gameinfo_lines.is_empty() {
        let mut f = Finding::new(Code::Legacy, Severity::Decision, "an older cs2-mcp install layout is in this tree")
            .paths(legacy.iter().cloned().chain(s.gameinfo_lines.iter().cloned()));
        if !legacy.is_empty() {
            f = f.advice("`cs2mcp-native cleanup --yes` (GUI: Clean up) moves the old files out of the install");
        }
        if !s.gameinfo_lines.is_empty() {
            f = f.advice("the gameinfo.gi lines are never edited here: Verify integrity of game files in Steam reverts them (`cargo bridge install` no longer handles that layout)");
        }
        if s.steam_settings_link {
            f = f.advice("with --accept-conflicts the steam_settings link is taken out for the session (nothing is written through it) and put back afterwards");
        }
        v.push(f.advice("--accept-conflicts (GUI: Continue anyway) leaves them in place"));
    }
    v
}

/// One cleanup step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// move the file (or empty-dir tree) out of the install into the quarantine dir
    Quarantine(String),
    /// take our entry out of `sdkenginetools.txt`
    Unedit(String),
    /// put `src` (Valve's file, kept by setup_gbe.sh) back at the path; the current bytes go to the
    /// quarantine dir
    PutBack { rel: String, src: PathBuf },
    /// remove a symlink (setup_gbe.sh's steam_settings link)
    Unlink(String),
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Action::Quarantine(r) => format!("move out of the install: {r}"),
            Action::Unedit(r) => format!("remove our cs2_bridge entry from {r}"),
            Action::PutBack { rel, src } => format!("put Valve's file back: {} -> {rel}", src.display()),
            Action::Unlink(r) => format!("remove the symlink {r}"),
        }
    }
}

/// What [`cleanup`] would do for `s`.
pub fn plan_cleanup(s: &Seen, os: Os) -> Vec<Action> {
    let mut v: Vec<Action> = Vec::new();
    let mut take: Vec<String> = s.exact.iter().chain(&s.other_build).cloned().collect();
    if s.metamod_is_ours() {
        take.extend(s.metamod_exact.iter().cloned());
    }
    // the generated settings files are only ours as a set: steam_interfaces.txt has no fixed bytes
    let st = steam_settings_rel(os);
    if !s.steam_settings_link && take.iter().any(|p| p.starts_with(st)) && !take.contains(&format!("{st}/steam_interfaces.txt")) {
        take.push(format!("{st}/steam_interfaces.txt"));
    }
    take.sort();
    take.dedup();
    v.extend(take.into_iter().map(Action::Quarantine));
    for l in &s.legacy {
        v.push(Action::Quarantine(l.clone()));
    }
    if s.edited_tools_list {
        v.push(Action::Unedit("game/bin/sdkenginetools.txt".into()));
    }
    if s.steam_settings_link {
        v.push(Action::Unlink(st.to_string()));
    }
    if let (true, Some(o)) = (s.emulated_steam_api, &s.gbe_orig) {
        v.push(Action::PutBack { rel: steam_api_rel(Os::Linux).into(), src: o.clone() });
    }
    v
}

fn move_out(root: &Path, rel: &str, quarantine: &Path) -> anyhow::Result<()> {
    let src = under(root, rel);
    let dst = under(quarantine, rel);
    std::fs::create_dir_all(dst.parent().unwrap_or(quarantine))?;
    if std::fs::rename(&src, &dst).is_err() {
        // another file system: copy, then remove
        if src.is_dir() {
            copy_tree(&src, &dst)?;
            std::fs::remove_dir_all(&src)?;
        } else {
            std::fs::copy(&src, &dst)?;
            crate::manifest::retry_in_use(&src, crate::manifest::IN_USE_GRACE, || std::fs::remove_file(&src))?;
        }
    }
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (s, d) = (e.path(), dst.join(e.file_name()));
        if e.file_type()?.is_dir() {
            copy_tree(&s, &d)?;
        } else {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

/// Remove the now-empty directories above `rel` that only our files lived in (never the game's
/// own `game/bin/...` or `game/csgo`; `game/csgo/addons` only when nothing else is in it).
fn prune(root: &Path, rel: &str) -> Vec<String> {
    const KEEP: [&str; 6] = ["game", "game/bin", "game/bin/win64", "game/bin/linuxsteamrt64", "game/csgo", "content"];
    let mut out = Vec::new();
    let mut parts: Vec<&str> = rel.split('/').collect();
    parts.pop();
    while !parts.is_empty() {
        let d = parts.join("/");
        if KEEP.contains(&d.as_str()) || d == "game/bin/win64/tools" {
            break;
        }
        let p = under(root, &d);
        let empty = std::fs::read_dir(&p).map(|mut it| it.next().is_none()).unwrap_or(false);
        if !empty || std::fs::remove_dir(&p).is_err() {
            break;
        }
        out.push(d);
        parts.pop();
    }
    out
}

/// Carry out `actions` on `root`, moving everything taken away into `quarantine`.
pub fn cleanup(root: &Path, actions: &[Action], quarantine: &Path, ev: &crate::events::Events) -> anyhow::Result<Value> {
    let mut done = Vec::new();
    let mut pruned = Vec::new();
    for a in actions {
        match a {
            Action::Quarantine(rel) => {
                if std::fs::symlink_metadata(under(root, rel)).is_err() {
                    continue;
                }
                move_out(root, rel, quarantine)?;
                pruned.extend(prune(root, rel));
            }
            Action::Unedit(rel) => {
                let p = under(root, rel);
                let t = std::fs::read_to_string(&p)?;
                match layout::sdkenginetools_unedit(&t) {
                    Some(orig) => {
                        let q = under(quarantine, rel);
                        std::fs::create_dir_all(q.parent().unwrap_or(quarantine))?;
                        std::fs::write(&q, t.as_bytes())?;
                        write_atomic(&p, orig.as_bytes())?;
                    }
                    None => {
                        ev.warn(format!("{rel}: our entry is there but not in the form we write; left alone ({VERIFY_ADVICE})"));
                        continue;
                    }
                }
            }
            Action::PutBack { rel, src } => {
                let p = under(root, rel);
                let q = under(quarantine, rel);
                std::fs::create_dir_all(q.parent().unwrap_or(quarantine))?;
                std::fs::copy(&p, &q)?;
                let bytes = std::fs::read(src)?;
                write_atomic(&p, &bytes)?;
            }
            Action::Unlink(rel) => {
                let p = under(root, rel);
                if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
                    std::fs::remove_file(&p)?;
                }
            }
        }
        ev.info(format!("cleanup: {}", a.describe()));
        done.push(a.describe());
    }
    Ok(json!({"done": done, "prunedDirs": pruned, "quarantine": quarantine}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{put, scratch};

    fn art(base: &Path) -> Artifacts {
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
        ] {
            put(&base.join(f), f.as_bytes());
        }
        Artifacts { bridge: base.join("bridge-rs"), metamod: base.join("metamod"), gbe: base.join("gbe"), vklayer: base.join("vklayer") }
    }

    const TOOLS: &str = "\t\t{\n\t\t\tm_Name = \"hammer\"\n\t\t},\n";

    /// The state dir was deleted while a Windows tree was prepared: our files are found, a cleanup
    /// moves them out, un-edits the tools list byte-exactly and leaves the game's dirs.
    #[test]
    fn orphans_of_a_lost_state_dir_are_found_and_cleaned_up() {
        let base = scratch("leftovers-orphans");
        let a = art(&base.join("art"));
        let root = base.join("install");
        let save = base.join("state/gbe-saves");
        put(&root.join("game/bin/win64/cs2.exe"), b"valve");
        put(&root.join("game/bin/win64/tools/hammer.dll"), b"valve");
        put(&root.join("game/bin/sdkenginetools.txt"), TOOLS.as_bytes());
        let k = known(Os::Windows, &a, &save);
        assert!(look(&root, Os::Windows, &k, None).is_empty(), "a clean tree");
        // what a prepare left behind
        put(&root.join("game/bin/win64/tools/cs2_bridge.dll"), b"bridge-rs/win64/cs2_bridge.dll");
        put(&root.join("game/bin/win64/cs2mcp_launch.exe"), b"an older launcher");
        put(&root.join("game/csgo/addons/metamod/cs2mcp.vdf"), layout::WINDOWS_VDF.as_bytes());
        put(&root.join("game/csgo/addons/metamod/bin/win64/server.dll"), b"metamod/win64/addons/metamod/bin/win64/server.dll");
        put(&root.join("game/bin/win64/steam_settings/configs.main.ini"), layout::GBE_CONFIGS_MAIN.as_bytes());
        put(&root.join("game/bin/win64/steam_settings/steam_interfaces.txt"), b"SteamUser023\n");
        put(&root.join("game/bin/sdkenginetools.txt"), layout::sdkenginetools_edit(TOOLS).unwrap().as_bytes());
        let s = look(&root, Os::Windows, &k, None);
        assert!(s.exact.contains(&"game/bin/win64/tools/cs2_bridge.dll".to_string()), "{s:?}");
        assert_eq!(s.other_build, ["game/bin/win64/cs2mcp_launch.exe"]);
        assert!(s.edited_tools_list);
        let f = findings(&s, Os::Windows);
        assert_eq!((f[0].code, f[0].severity), (Code::Orphans, Severity::Decision));
        assert!(f[0].paths.iter().any(|p| p.contains("server.dll")), "Metamod next to our vdf is ours: {:?}", f[0].paths);

        let plan = plan_cleanup(&s, Os::Windows);
        let q = base.join("state/cleanup-1");
        cleanup(&root, &plan, &q, &crate::events::Events::null()).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("game/bin/sdkenginetools.txt")).unwrap(), TOOLS);
        for gone in ["game/bin/win64/tools/cs2_bridge.dll", "game/csgo/addons", "game/bin/win64/steam_settings", "game/bin/win64/cs2mcp_launch.exe"] {
            assert!(!root.join(gone).exists(), "{gone}");
        }
        assert!(root.join("game/bin/win64/tools/hammer.dll").exists(), "the game's own files stay");
        assert!(q.join("game/bin/win64/tools/cs2_bridge.dll").is_file(), "moved, not deleted");
        assert!(look(&root, Os::Windows, &k, None).is_empty());
    }

    #[test]
    fn a_users_own_metamod_alone_is_only_a_note() {
        let base = scratch("leftovers-own-mm");
        let a = art(&base.join("art"));
        let root = base.join("install");
        put(&root.join("game/csgo/addons/metamod/bin/win64/server.dll"), b"metamod/win64/addons/metamod/bin/win64/server.dll");
        let s = look(&root, Os::Windows, &known(Os::Windows, &a, &base), None);
        let f = findings(&s, Os::Windows);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Info);
        assert!(plan_cleanup(&s, Os::Windows).is_empty());
    }

    /// An old container volume: install-rs.sh's legacy vdf, the image's gbe in place of Valve's
    /// library (its backup in ~/.steam), the steam_settings symlink, an old gameinfo line.
    #[cfg(unix)]
    #[test]
    fn the_old_container_layout_is_legacy_and_setup_gbe_is_undone_from_its_backup() {
        let base = scratch("leftovers-legacy");
        let a = art(&base.join("art"));
        let root = base.join("vol/Counter-Strike Global Offensive");
        let home = base.join("home");
        put(&root.join("game/bin/linuxsteamrt64/libsteam_api.so"), b"gbe: configs.main.ini");
        put(&home.join(".steam/gbe_orig_libsteam_api.so"), b"valve's SteamUser023");
        put(&home.join(".steam/sdk64/steam_settings/configs.user.ini"), b"x");
        std::os::unix::fs::symlink(home.join(".steam/sdk64/steam_settings"), root.join("game/bin/linuxsteamrt64/steam_settings")).unwrap();
        put(&root.join("game/csgo/addons/metamod.vdf"), b"\"Plugin\" { \"file\" \"x\" }");
        put(&root.join("game/csgo/gameinfo.gi"), b"\t\t\tGame_LowViolence\tcsgo_lv\n\t\t\tGame\tcsgo/addons/metamod\n\t\t\tGame\tcsgo\n");
        let s = look(&root, Os::Linux, &known(Os::Linux, &a, &base), Some(&home));
        assert!(s.emulated_steam_api && s.steam_settings_link && s.gbe_orig.is_some(), "{s:?}");
        assert_eq!(s.legacy, ["game/csgo/addons/metamod.vdf"]);
        assert_eq!(s.gameinfo_lines, ["game/csgo/gameinfo.gi: `Game\tcsgo/addons/metamod`"]);
        let f = findings(&s, Os::Linux);
        let codes: Vec<_> = f.iter().map(|f| (f.code, f.severity)).collect();
        assert_eq!(codes, [(Code::ReplacedGameFile, Severity::Decision), (Code::Legacy, Severity::Decision)]);
        assert!(f[1].advice.iter().any(|a| a.contains("gameinfo.gi lines") && a.contains("Verify integrity")));

        let plan = plan_cleanup(&s, Os::Linux);
        cleanup(&root, &plan, &base.join("q"), &crate::events::Events::null()).unwrap();
        assert_eq!(std::fs::read(root.join("game/bin/linuxsteamrt64/libsteam_api.so")).unwrap(), b"valve's SteamUser023");
        assert!(std::fs::symlink_metadata(root.join("game/bin/linuxsteamrt64/steam_settings")).is_err());
        assert!(home.join(".steam/sdk64/steam_settings/configs.user.ini").exists(), "the link target is not ours to remove");
        assert!(!root.join("game/csgo/addons/metamod.vdf").exists());
        assert!(std::fs::read_to_string(root.join("game/csgo/gameinfo.gi")).unwrap().contains("csgo/addons/metamod"), "gameinfo is never edited");
        let after = look(&root, Os::Linux, &known(Os::Linux, &a, &base), Some(&home));
        assert!(!after.emulated_steam_api && !after.steam_settings_link && after.legacy.is_empty());
    }
}
