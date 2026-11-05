//! `cargo bridge install` against fake trees (the port of the shell tool's host test). No game,
//! no containers, no network: every case builds its own fake Wine/Linux trees, Metamod builds and
//! artefacts under the cargo target tmpdir, and runs the real binary with a stub podman that
//! answers `ps` / `inspect` / `volume inspect` from its environment and turns `unshare <cmd>`
//! into a plain exec of `<cmd>` (so the whole `--platform linux` flow runs on a plain
//! directory). The real trees are never named: the project root, the backup root and podman's
//! XDG dir all point into the case's scratch dir, and the environment is cleared first.
//!
//! The bridge artefacts are synthesized images that the real export gate (`cargo bridge
//! check-exports`, spawned by install) accepts: a PE32+ DLL and an ELF64 .so whose only export is
//! `CreateInterface`. Every install runs that gate.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::time::SystemTime;

const BIN: &str = env!("CARGO_BIN_EXE_cs2-bridge-tools");
/// The busy container the stub fabricates.
const NAME: &str = "cs2mm-spike-cl";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/install")
        .join(name)
}

// ----- the synthesized artefacts --------------------------------------------------------------------

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// A PE32+ DLL with one section holding an export directory: one entry, named `CreateInterface`;
/// no imports.
fn fake_pe() -> Vec<u8> {
    let mut b = vec![0u8; 0x400];
    b[0..2].copy_from_slice(b"MZ");
    put32(&mut b, 0x3c, 0x40);
    b[0x40..0x44].copy_from_slice(b"PE\0\0");
    let coff = 0x44;
    put16(&mut b, coff, 0x8664); // AMD64
    put16(&mut b, coff + 2, 1); // one section
    put16(&mut b, coff + 16, 0xf0); // SizeOfOptionalHeader
    put16(&mut b, coff + 18, 0x2022); // EXECUTABLE_IMAGE | LARGE_ADDRESS_AWARE | DLL
    let opt = coff + 20;
    put16(&mut b, opt, 0x20b); // PE32+
    put32(&mut b, opt + 20, 0x1000); // BaseOfCode
    put64(&mut b, opt + 24, 0x1_8000_0000); // ImageBase
    put32(&mut b, opt + 32, 0x1000); // SectionAlignment
    put32(&mut b, opt + 36, 0x200); // FileAlignment
    put16(&mut b, opt + 40, 6);
    put16(&mut b, opt + 48, 6);
    put32(&mut b, opt + 56, 0x2000); // SizeOfImage
    put32(&mut b, opt + 60, 0x200); // SizeOfHeaders
    put16(&mut b, opt + 68, 2); // GUI
    put16(&mut b, opt + 70, 0x160);
    put64(&mut b, opt + 72, 0x10_0000);
    put64(&mut b, opt + 80, 0x1000);
    put64(&mut b, opt + 88, 0x10_0000);
    put64(&mut b, opt + 96, 0x1000);
    put32(&mut b, opt + 108, 16); // NumberOfRvaAndSizes
    put32(&mut b, opt + 112, 0x1000); // export directory RVA
    put32(&mut b, opt + 116, 0x60); // and size
    let sec = opt + 0xf0;
    b[sec..sec + 6].copy_from_slice(b".edata");
    put32(&mut b, sec + 8, 0x100); // VirtualSize
    put32(&mut b, sec + 12, 0x1000); // VirtualAddress
    put32(&mut b, sec + 16, 0x200); // SizeOfRawData
    put32(&mut b, sec + 20, 0x200); // PointerToRawData
    put32(&mut b, sec + 36, 0x4000_0040); // INITIALIZED_DATA | MEM_READ
    let ed = 0x200;
    put32(&mut b, ed + 12, 0x1040); // Name
    put32(&mut b, ed + 16, 1); // Base
    put32(&mut b, ed + 20, 1); // NumberOfFunctions
    put32(&mut b, ed + 24, 1); // NumberOfNames
    put32(&mut b, ed + 28, 0x1028); // AddressOfFunctions
    put32(&mut b, ed + 32, 0x102c); // AddressOfNames
    put32(&mut b, ed + 36, 0x1030); // AddressOfNameOrdinals
    put32(&mut b, 0x228, 0x1080); // the function (outside the directory: not a forwarder)
    put32(&mut b, 0x22c, 0x1050); // its name
    put16(&mut b, 0x230, 0); // its ordinal index
    b[0x240..0x249].copy_from_slice(b"fake.dll\0");
    b[0x250..0x260].copy_from_slice(b"CreateInterface\0");
    b
}

/// An ELF64 shared object whose `.dynsym` defines exactly `CreateInterface`.
fn fake_elf() -> Vec<u8> {
    let mut b = vec![0u8; 0xc0 + 4 * 64];
    b[0..4].copy_from_slice(b"\x7fELF");
    b[4] = 2; // ELFCLASS64
    b[5] = 1; // little endian
    b[6] = 1;
    put16(&mut b, 16, 3); // ET_DYN
    put16(&mut b, 18, 62); // x86-64
    put32(&mut b, 20, 1);
    put64(&mut b, 40, 0xc0); // e_shoff
    put16(&mut b, 52, 64);
    put16(&mut b, 58, 64); // e_shentsize
    put16(&mut b, 60, 4); // e_shnum
    put16(&mut b, 62, 3); // e_shstrndx
    let dynstr = b"\0CreateInterface\0";
    b[64..64 + dynstr.len()].copy_from_slice(dynstr);
    let sym = 0x60 + 24; // symbol 1 (symbol 0 is the null symbol)
    put32(&mut b, sym, 1);
    b[sym + 4] = 0x12; // GLOBAL FUNC
    put16(&mut b, sym + 6, 4); // defined (a section index != SHN_UNDEF)
    let shstr = b"\0.dynstr\0.dynsym\0.shstrtab\0";
    b[0x90..0x90 + shstr.len()].copy_from_slice(shstr);
    let sh = |i: usize| 0xc0 + i * 64;
    let (s1, s2, s3) = (sh(1), sh(2), sh(3));
    put32(&mut b, s1, 1);
    put32(&mut b, s1 + 4, 3); // SHT_STRTAB
    put64(&mut b, s1 + 24, 64);
    put64(&mut b, s1 + 32, dynstr.len() as u64);
    put32(&mut b, s2, 9);
    put32(&mut b, s2 + 4, 11); // SHT_DYNSYM
    put64(&mut b, s2 + 24, 0x60);
    put64(&mut b, s2 + 32, 48);
    put32(&mut b, s2 + 40, 1); // sh_link: .dynstr
    put32(&mut b, s2 + 44, 1);
    put64(&mut b, s2 + 56, 24);
    put32(&mut b, s3, 17);
    put32(&mut b, s3 + 4, 3);
    put64(&mut b, s3 + 24, 0x90);
    put64(&mut b, s3 + 32, shstr.len() as u64);
    b
}

// ----- the stub podman ----------------------------------------------------------------------------------

/// One stub for the whole test binary, written before any case spawns anything (a script
/// written while another thread forks can fail to exec with ETXTBSY). Its behaviour comes from
/// the environment the binary passes down: `STUB_MODE` (none | busy | ps-fail), `STUB_SRC` (the
/// bind source the busy container reports), `STUB_VMP` (the volume's mountpoint).
fn stub_podman() -> PathBuf {
    static ONCE: Once = Once::new();
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("install-podman-stub");
    ONCE.call_once(|| {
        let body = format!(
            r#"#!/bin/bash
cmd="${{1:-}}"; shift
case "$cmd" in
  ps)
    if [ "$STUB_MODE" = ps-fail ]; then echo "stub podman: ps fails" >&2; exit 1; fi
    if [ "$STUB_MODE" = busy ]; then echo "container_id_one"; fi
    ;;
  inspect)
    case "$2" in
      '{{{{.Name}}}}') echo "{NAME}" ;;
      '{{{{json .Mounts}}}}')
        # an unrelated named volume FIRST (the wine matcher must ignore it), then the bind
        if [ -n "$STUB_SRC" ]; then
          printf '[{{"Type":"volume","Name":"cs2-home","Destination":"/home/steam"}},{{"Type":"bind","Source":"%s","Destination":"/app"}}]\n' "$STUB_SRC"
        else
          echo '[]'
        fi ;;
      *) echo "stub: unsupported format: $2" >&2; exit 1 ;;
    esac ;;
  volume)
    case "$1/$4" in
      'inspect/{{{{.Mountpoint}}}}') echo "$STUB_VMP" ;;
      *) echo "stub: unsupported volume call: $*" >&2; exit 1 ;;
    esac ;;
  unshare) exec "$@" ;;
  *) echo "stub: unsupported: $cmd" >&2; exit 2 ;;
esac
"#
        );
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, body).unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&tmp, &path).unwrap();
    });
    path
}

// ----- the scratch world ---------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum P {
    Wine,
    Linux,
}

/// One case's scratch dir: artefacts in `bin/`, Metamod builds in `builds/`, trees in `trees/`,
/// backups in `target/bridge/` (CS2_MCP_DIR), the lock + XDG under `project/`.
struct W {
    w: PathBuf,
}

struct Out {
    rc: i32,
    text: String,
}

impl Out {
    fn has(&self, s: &str) -> bool {
        self.text.contains(s)
    }
}

impl std::fmt::Display for Out {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rc={} output:\n{}", self.rc, self.text)
    }
}

/// The environment of one run (starts empty: only PATH is inherited).
#[derive(Clone)]
struct Env(BTreeMap<String, OsString>);

impl Env {
    fn set(mut self, k: &str, v: impl Into<OsString>) -> Env {
        self.0.insert(k.into(), v.into());
        self
    }

    fn run(&self, args: &[&str]) -> Out {
        let out = Command::new(BIN)
            .arg("install")
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .envs(&self.0)
            .output()
            .expect("run cs2-bridge-tools");
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Out {
            rc: out.status.code().unwrap_or(-1),
            text,
        }
    }
}

fn write(p: &Path, bytes: impl AsRef<[u8]>) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

fn make_writable(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                make_writable(&e.path());
            }
        }
    }
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
}

impl W {
    fn new(case: &str) -> W {
        // every case comes here before it spawns anything: the stub is complete before any fork
        stub_podman();
        let w = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("install")
            .join(case);
        if w.exists() {
            // a killed earlier run can leave read-only dirs behind (the read-only tree case)
            make_writable(&w);
            fs::remove_dir_all(&w).unwrap();
        }
        for d in ["bin", "common", "podman-xdg", "project", "trees"] {
            fs::create_dir_all(w.join(d)).unwrap();
        }
        write(&w.join("bin/fake.dll"), fake_pe());
        write(&w.join("bin/fake.so"), fake_elf());
        // the launcher artefacts install copies verbatim (not export-gated): distinct bytes each
        write(&w.join("bin/launch.exe"), "stub cs2mcp_launch.exe\n");
        write(&w.join("bin/mmload.dll"), "stub cs2mcp_mmload.dll\n");
        write(&w.join("bin/mmload.so"), "stub libcs2mcp_mmload.so\n");
        // the Metamod builds (copied as they are; only the bridge is gated)
        let mw = w.join("builds/mmwin/metamod");
        write(&mw.join("bin/win64/server.dll"), fake_pe());
        write(&mw.join("bin/win64/metamod.2.cs2.dll"), fake_pe());
        write(&mw.join("metaplugins.ini"), "mm Plugins Load\n");
        let ml = w.join("builds/mmlinux/metamod");
        write(&ml.join("bin/linux64/server.so"), fake_elf());
        write(&ml.join("bin/linuxsteamrt64/libserver.so"), fake_elf());
        write(&ml.join("bin/linuxsteamrt64/metamod.2.cs2.so"), fake_elf());
        write(&ml.join("metaplugins.ini"), "mm Plugins Load\n");
        write(
            &ml.join("README.txt"),
            "Metamod:Source 2.0-dev (linux build)\n",
        );
        W { w }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.w.join(rel)
    }

    /// The volume's (fake) mountpoint the stub reports.
    fn vmp(&self) -> PathBuf {
        self.p("common/cs2-home")
    }

    /// The env for a tree: a wine tree root holds `game/`; a linux tree root holds `game/` too
    /// (the Linux game dir).
    fn env(&self, tree: &Path, p: P) -> Env {
        let e = Env(BTreeMap::new())
            .set("CS2_MCP_DIR", self.w.as_os_str())
            .set("CS2_MCP_PROJECT_ROOT", self.p("project"))
            .set("PODMAN", stub_podman())
            .set("PODMAN_XDG", self.p("podman-xdg"))
            .set("STUB_MODE", "none")
            .set("STUB_VMP", self.vmp());
        match p {
            P::Wine => e
                .set("CS2_GAME", tree.join("game"))
                .set("CS2_MM", self.p("builds/mmwin"))
                .set("CS2_BRIDGE_DLL", self.p("bin/fake.dll"))
                .set("CS2_MM_EXE", self.p("bin/launch.exe"))
                .set("CS2_MM_DLL", self.p("bin/mmload.dll")),
            P::Linux => e
                .set("CS2_LINUX_GAME", tree.join("game"))
                .set("CS2_MM_LINUX", self.p("builds/mmlinux"))
                .set("CS2_BRIDGE_LINUX_SO", self.p("bin/fake.so"))
                .set("CS2_MMLOAD_SO", self.p("bin/mmload.so")),
        }
    }

    /// A Wine app dir: `game/bin/sdkenginetools.txt` + `game/csgo/gameinfo.gi` (CRLF) and Valve's
    /// `content/csgo` (never ours to remove, even empty).
    fn fresh_wine(&self, name: &str) -> PathBuf {
        let t = self.p("trees").join(name);
        fs::create_dir_all(t.join("content/csgo")).unwrap();
        write(
            &t.join("game/bin/sdkenginetools.txt"),
            fs::read(fixture("sdkenginetools.txt")).unwrap(),
        );
        write(
            &t.join("game/csgo/gameinfo.gi"),
            fs::read(fixture("gameinfo.gi")).unwrap(),
        );
        t
    }

    /// A Linux tree: `game/csgo/gameinfo.gi` + one unrelated pre-existing addons file (uninstall
    /// keeps what it does not own).
    fn fresh_linux(&self, name: &str, lf: bool) -> PathBuf {
        let t = self.p("trees").join(name);
        let gi = if lf { "gameinfo-lf.gi" } else { "gameinfo.gi" };
        write(
            &t.join("game/csgo/gameinfo.gi"),
            fs::read(fixture(gi)).unwrap(),
        );
        write(
            &t.join("game/csgo/addons/unrelated-stays.txt"),
            "unrelated pre-existing content\n",
        );
        t
    }

    fn backups(&self, prefix: &str) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = fs::read_dir(self.p("target/bridge"))
            .map(|r| r.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        out.retain(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix));
        out.sort();
        out
    }

    /// The newest backup whose APP_TREE is `key`.
    fn newest_backup(&self, key: &Path) -> PathBuf {
        self.backups("backup-")
            .into_iter()
            .rfind(|b| {
                fs::read_to_string(b.join("APP_TREE"))
                    .ok()
                    .as_deref()
                    .map(str::trim_end)
                    == Some(&*key.to_string_lossy())
            })
            .unwrap_or_else(|| panic!("no backup of {}", key.display()))
    }
}

// ----- tree comparisons ------------------------------------------------------------------------------------

/// Every entry under `dir` (dirs as `None`, files with their bytes): `diff -r` equality.
fn listing(dir: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(root: &Path, d: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        for e in fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if e.file_type().unwrap().is_dir() {
                out.insert(rel, None);
                walk(root, &p, out);
            } else {
                out.insert(rel, Some(fs::read(&p).unwrap_or_default()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// Regular files only (`find -type f`), with their bytes.
fn snap(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    listing(dir)
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k, v)))
        .collect()
}

/// Regular files with mtime + size: "touched nothing", not even a rewrite with equal bytes.
fn mtimes(dir: &Path) -> BTreeMap<String, (SystemTime, u64)> {
    snap(dir)
        .into_keys()
        .map(|k| {
            let m = fs::metadata(dir.join(&k)).unwrap();
            (k, (m.modified().unwrap(), m.len()))
        })
        .collect()
}

fn files(dir: &Path) -> BTreeSet<String> {
    snap(dir).into_keys().collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap().flatten() {
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &to);
        } else {
            fs::copy(e.path(), &to).unwrap();
        }
    }
}

/// A pre-install copy of a tree (uninstall's reference).
fn pristine(w: &W, t: &Path, name: &str) -> PathBuf {
    let p = w.p("common").join(name);
    copy_tree(t, &p);
    p
}

fn same(a: &Path, b: &Path) -> bool {
    fs::read(a).ok() == fs::read(b).ok()
}

fn temps_in(dir: &Path) -> Vec<String> {
    files(dir)
        .into_iter()
        .filter(|f| f.contains(".tmp."))
        .collect()
}

fn read(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_default()
}

const MAN_ANCHOR: &str = "\t\t{\n\t\t\tm_Name = \"hammer\"";
const OUR_ENTRY: &str = "\t\t{\n\t\t\tm_Name = \"cs2_bridge\"\n\t\t\tm_FriendlyName = \"CS2 Bridge (Rust)\"\n\t\t\tm_Library = \"tools/cs2_bridge.dll\"\n\t\t\tm_bShowInToolsMenu = false\n\t\t},\n";
const WINE_VDF: &str = "\"Metamod Plugin\"\n{\n\t\"alias\"\t\t\"cs2mcp\"\n\t\"file\"\t\t\"../bin/win64/tools/cs2_bridge.dll\"\n}\n";
const LINUX_VDF: &str = "\"Metamod Plugin\"\n{\n\t\"alias\"\t\"cs2mcp\"\n\t\"file\"\t\"addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so\"\n}\n";
const WINE_SET: [&str; 9] = [
    "game/bin/win64/tools/cs2_bridge.dll",
    "game/bin/sdkenginetools.txt",
    "game/csgo/gameinfo.gi",
    "game/bin/win64/cs2mcp_launch.exe",
    "game/bin/win64/cs2mcp_mmload.dll",
    "game/csgo/addons/metamod/bin/win64/server.dll",
    "game/csgo/addons/metamod/bin/win64/metamod.2.cs2.dll",
    "game/csgo/addons/metamod/metaplugins.ini",
    "game/csgo/addons/metamod/cs2mcp.vdf",
];
const LINUX_SET: [&str; 10] = [
    "csgo/gameinfo.gi",
    "csgo/addons/unrelated-stays.txt",
    "csgo/addons/metamod/bin/linux64/server.so",
    "csgo/addons/metamod/bin/linuxsteamrt64/libserver.so",
    "csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so",
    "csgo/addons/metamod/metaplugins.ini",
    "csgo/addons/metamod/README.txt",
    "csgo/addons/metamod/cs2mcp.vdf",
    "csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so",
    "csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so",
];

fn sha(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// =========================================================================== cases =========

#[test]
fn the_synthesized_artefacts_pass_the_real_export_gate() {
    let w = W::new("gate");
    for f in ["bin/fake.dll", "bin/fake.so"] {
        let out = Command::new(BIN)
            .arg("check-exports")
            .arg(w.p(f))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{f}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn wine_install_reinstall_uninstall() {
    let w = W::new("wine-main");
    let t = w.fresh_wine("wine-main");
    let pre = pristine(&w, &t, "wine.pristine");
    let env = w.env(&t, P::Wine);
    let out = env.run(&["install"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(
        files(&t),
        set(&WINE_SET),
        "the standard addons layout + launcher, nothing else"
    );
    for p in ["game/csgo/addons/cs2mcp", "game/metamod", "content/metamod"] {
        assert!(
            !t.join(p).exists(),
            "{p}: no second copy, no old -tools layout"
        );
    }
    assert_eq!(
        read(&t.join("game/csgo/addons/metamod/cs2mcp.vdf")),
        WINE_VDF
    );
    assert!(same(
        &t.join("game/bin/win64/tools/cs2_bridge.dll"),
        &w.p("bin/fake.dll")
    ));
    let man = read(&t.join("game/bin/sdkenginetools.txt"));
    assert!(man.contains(&format!("{OUR_ENTRY}{MAN_ANCHOR}")), "{man}");
    assert!(
        same(&fixture("gameinfo.gi"), &t.join("game/csgo/gameinfo.gi")),
        "gameinfo.gi is never touched"
    );
    assert!(same(
        &w.p("bin/launch.exe"),
        &t.join("game/bin/win64/cs2mcp_launch.exe")
    ));
    assert!(same(
        &w.p("bin/mmload.dll"),
        &t.join("game/bin/win64/cs2mcp_mmload.dll")
    ));
    assert!(same(
        &w.p("builds/mmwin/metamod/bin/win64/server.dll"),
        &t.join("game/csgo/addons/metamod/bin/win64/server.dll")
    ));

    // the backup, in the format the shell tool wrote (byte-compatible both ways)
    let bk = w.newest_backup(&t);
    assert_eq!(read(&bk.join("APP_TREE")), format!("{}\n", t.display()));
    let man_sha = sha(&fs::read(fixture("sdkenginetools.txt")).unwrap());
    assert_eq!(
        read(&bk.join("SHA256SUMS")),
        format!("{man_sha}  game/bin/sdkenginetools.txt\n")
    );
    assert!(same(
        &bk.join("game/bin/sdkenginetools.txt"),
        &fixture("sdkenginetools.txt")
    ));
    assert_eq!(read(&bk.join("EDITS_WROTE")), "manifest:cs2_bridge\n");
    assert_eq!(read(&bk.join("ANCHORS")), "manifest\tm_Name = \"hammer\"\n");
    let mp_sha = sha(b"mm Plugins Load\n");
    assert_eq!(
        read(&bk.join("INSTALLED")),
        format!(
            "game/bin/win64/tools/cs2_bridge.dll\ngame/bin/win64/cs2mcp_launch.exe\ngame/bin/win64/cs2mcp_mmload.dll\n\
             game/csgo/addons/metamod/bin/win64/server.dll\ngame/csgo/addons/metamod/bin/win64/metamod.2.cs2.dll\n\
             game/csgo/addons/metamod/metaplugins.ini {mp_sha}\ngame/csgo/addons/metamod/cs2mcp.vdf\n"
        )
    );
    assert_eq!(read(&bk.join("WROTE_OVER")), "");

    // install leaves every artefact ensure-current: ensure touches nothing, makes no backup
    let n = w.backups("backup-rs-").len();
    let before = mtimes(&t.join("game"));
    let out = env.run(&["ensure"]);
    assert!(out.rc == 0 && out.has("all artefacts current"), "{out}");
    assert_eq!(
        w.backups("backup-rs-").len(),
        n,
        "ensure all-current creates no backup"
    );
    assert_eq!(
        mtimes(&t.join("game")),
        before,
        "ensure all-current: byte+mtime identical"
    );

    // re-install: a byte-identical no-op
    let before = snap(&t.join("game"));
    assert_eq!(env.run(&["install"]).rc, 0);
    assert_eq!(
        snap(&t.join("game")),
        before,
        "re-install is a byte-identical no-op"
    );

    // uninstall: back to the pre-install tree, install's dirs gone
    let out = env.run(&["uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(
        listing(&t),
        listing(&pre),
        "uninstall restores the pre-install tree"
    );
    for p in [
        "game/csgo/addons/metamod",
        "game/csgo/addons",
        "game/bin/win64/tools",
        "game/bin/win64",
    ] {
        assert!(!t.join(p).exists(), "{p}");
    }
    assert!(
        w.backups("backup-rs-").is_empty(),
        "uninstall retires the backups"
    );
    assert!(w.p("target/bridge/retired").is_dir());
}

#[test]
fn wine_uninstall_removes_the_legacy_second_copy() {
    let w = W::new("wine-legacy");
    let t = w.fresh_wine("wine-legacy");
    write(
        &t.join("game/csgo/addons/cs2mcp/bin/win64/cs2_bridge.dll"),
        "legacy second copy from an OLD install\n",
    );
    let env = w.env(&t, P::Wine);
    let out = env.run(&["install"]);
    assert_eq!(out.rc, 0, "legacy copy tolerated: {out}");
    let out = env.run(&["uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert!(
        !t.join("game/csgo/addons/cs2mcp").exists(),
        "the legacy copy and its dirs are removed: {out}"
    );
}

#[test]
fn linux_install_uninstall() {
    let w = W::new("linux-main");
    let t = w.fresh_linux("linux-main", false);
    let pre = pristine(&w, &t, "linux.pristine");
    let env = w.env(&t, P::Linux);
    let out = env.run(&["--platform", "linux", "install"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(
        files(&t.join("game")),
        set(&LINUX_SET),
        "bridge + mmload .so, no loader vdfs"
    );
    assert_eq!(
        read(&t.join("game/csgo/addons/metamod/cs2mcp.vdf")),
        LINUX_VDF
    );
    assert!(same(
        &t.join("game/csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so"),
        &w.p("bin/fake.so")
    ));
    assert!(
        same(&fixture("gameinfo.gi"), &t.join("game/csgo/gameinfo.gi")),
        "gameinfo.gi untouched"
    );
    assert!(same(
        &w.p("bin/mmload.so"),
        &t.join("game/csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so")
    ));
    let out = env.run(&["--platform", "linux", "ensure"]);
    assert!(out.rc == 0 && out.has("all artefacts current"), "{out}");
    // the backup: APP_TREE + SHA256SUMS + an EMPTY EDITS_WROTE (no edit; gameinfo not backed up)
    let bk = w.newest_backup(&t.join("game"));
    assert_eq!(
        read(&bk.join("APP_TREE")),
        format!("{}\n", t.join("game").display())
    );
    assert_eq!(
        read(&bk.join("SHA256SUMS")),
        "",
        "nothing of ours pre-existed"
    );
    assert_eq!(read(&bk.join("EDITS_WROTE")), "");
    assert_eq!(read(&bk.join("ANCHORS")), "");
    assert_eq!(read(&bk.join("WROTE_OVER")), "");
    let mp_sha = sha(b"mm Plugins Load\n");
    let installed: BTreeSet<String> = read(&bk.join("INSTALLED"))
        .lines()
        .map(String::from)
        .collect();
    let mut want: BTreeSet<String> = LINUX_SET[2..].iter().map(|s| s.to_string()).collect();
    want.remove("csgo/addons/metamod/metaplugins.ini");
    want.insert(format!("csgo/addons/metamod/metaplugins.ini {mp_sha}"));
    assert_eq!(installed, want);
    assert!(read(&bk.join("INSTALLED")).ends_with('\n'));
    assert!(!bk.join("csgo/gameinfo.gi").exists());

    // an LF gameinfo is untouched too
    let t2 = w.fresh_linux("linux-lf", true);
    assert_eq!(
        w.env(&t2, P::Linux)
            .run(&["--platform", "linux", "install"])
            .rc,
        0
    );
    assert!(same(
        &fixture("gameinfo-lf.gi"),
        &t2.join("game/csgo/gameinfo.gi")
    ));

    let out = env.run(&["--platform", "linux", "uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(
        listing(&t),
        listing(&pre),
        "uninstall restores the pre-install tree (unrelated file kept)"
    );
    assert!(!t.join("game/csgo/addons/metamod").exists());
    assert!(!t.join("game/csgo/addons/cs2mcp").exists());
}

#[test]
fn ensure_installs_checks_repairs_and_refuses_while_busy() {
    let w = W::new("ensure");
    let t = w.fresh_wine("ensure-main");
    let env = w.env(&t, P::Wine);
    let out = env.run(&["ensure"]);
    assert_eq!(out.rc, 0, "{out}");
    assert!(
        out.has("installing to repair") && t.join("game/bin/win64/tools/cs2_bridge.dll").is_file(),
        "{out}"
    );
    let before = snap(&t.join("game"));
    let out = env.run(&["ensure"]);
    assert!(out.rc == 0 && out.has("all artefacts current"), "{out}");
    assert_eq!(snap(&t.join("game")), before, "all-current touches nothing");

    // a Steam update rewrote sdkenginetools.txt (our entry gone): stale, repaired
    fs::copy(
        fixture("sdkenginetools.txt"),
        t.join("game/bin/sdkenginetools.txt"),
    )
    .unwrap();
    let out = env.run(&["ensure"]);
    assert!(out.rc == 0 && out.has("stale:"), "{out}");
    assert!(read(&t.join("game/bin/sdkenginetools.txt")).contains("m_Name = \"cs2_bridge\""));
    assert!(same(
        &fixture("gameinfo.gi"),
        &t.join("game/csgo/gameinfo.gi")
    ));

    // busy + current: exit 0, untouched
    let busy = env
        .clone()
        .set("STUB_MODE", "busy")
        .set("STUB_SRC", t.join("app"))
        .set("GUARD_FAKE", "1");
    let before = snap(&t.join("game"));
    let out = busy.run(&["ensure"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(snap(&t.join("game")), before);

    // busy + stale: exit 4, the container named, untouched
    fs::copy(
        fixture("sdkenginetools.txt"),
        t.join("game/bin/sdkenginetools.txt"),
    )
    .unwrap();
    let before = snap(&t.join("game"));
    let out = busy.run(&["ensure"]);
    assert_eq!(out.rc, 4, "{out}");
    assert!(out.has(NAME), "{out}");
    assert_eq!(snap(&t.join("game")), before);
}

#[test]
fn a_failing_podman_or_a_missing_build_refuses_before_any_change() {
    let w = W::new("negative");
    // podman ps fails (GUARD_FAKE=1): install and ensure's repair refuse, the tree untouched
    let t = w.fresh_wine("neg-wine");
    let env = w
        .env(&t, P::Wine)
        .set("STUB_MODE", "ps-fail")
        .set("GUARD_FAKE", "1");
    let before = snap(&t.join("game"));
    let out = env.run(&["install"]);
    assert_ne!(out.rc, 0, "{out}");
    assert_eq!(snap(&t.join("game")), before);
    let out = env.run(&["ensure"]);
    assert_ne!(out.rc, 0, "{out}");
    assert_eq!(snap(&t.join("game")), before);

    let tl = w.fresh_linux("neg-linux", false);
    let envl = w
        .env(&tl, P::Linux)
        .set("STUB_MODE", "ps-fail")
        .set("GUARD_FAKE", "1");
    let before = snap(&tl.join("game"));
    let out = envl.run(&["--platform", "linux", "install"]);
    assert_ne!(out.rc, 0, "{out}");
    assert_eq!(snap(&tl.join("game")), before);
    // a stale tree needs the repair path, whose probe refuses: exit 1 (a podman failure, not busy)
    let out = envl.run(&["--platform", "linux", "ensure"]);
    assert_eq!(out.rc, 1, "{out}");
    assert!(out.has("podman failed"), "{out}");
    assert_eq!(snap(&tl.join("game")), before);

    // a missing Metamod build file: preflight refuses before any change (both platforms)
    let tw = w.fresh_wine("neg-wine-mm");
    fs::remove_file(w.p("builds/mmwin/metamod/metaplugins.ini")).unwrap();
    let before = snap(&tw.join("game"));
    let out = w.env(&tw, P::Wine).run(&["install"]);
    assert!(
        out.rc != 0 && out.has("missing metamod build file"),
        "{out}"
    );
    assert_eq!(snap(&tw.join("game")), before);
    write(
        &w.p("builds/mmwin/metamod/metaplugins.ini"),
        "mm Plugins Load\n",
    );

    let tll = w.fresh_linux("neg-linux-mm", false);
    fs::remove_file(w.p("builds/mmlinux/metamod/bin/linuxsteamrt64/libserver.so")).unwrap();
    let before = snap(&tll.join("game"));
    let out = w
        .env(&tll, P::Linux)
        .run(&["--platform", "linux", "install"]);
    assert!(
        out.rc != 0 && out.has("missing metamod build file"),
        "{out}"
    );
    assert_eq!(snap(&tll.join("game")), before);
    write(
        &w.p("builds/mmlinux/metamod/bin/linuxsteamrt64/libserver.so"),
        fake_elf(),
    );

    // a missing launcher artefact with nothing installed: refused, says to build
    fs::remove_file(w.p("bin/mmload.dll")).unwrap();
    let out = w.env(&tw, P::Wine).run(&["install"]);
    assert!(
        out.rc != 0 && out.has("BUILD IT FIRST (run `cargo bridge build`)"),
        "{out}"
    );
    assert_eq!(snap(&tw.join("game")), before_wine(&tw));
    fs::remove_file(w.p("bin/mmload.so")).unwrap();
    let before = snap(&tll.join("game"));
    let out = w
        .env(&tll, P::Linux)
        .run(&["--platform", "linux", "install"]);
    assert!(
        out.rc != 0 && out.has("BUILD IT FIRST (run `cargo bridge build`"),
        "{out}"
    );
    assert_eq!(snap(&tll.join("game")), before);
}

/// A fresh wine tree's game files (what a refused install must leave).
fn before_wine(t: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut m = BTreeMap::new();
    m.insert(
        "bin/sdkenginetools.txt".to_string(),
        fs::read(fixture("sdkenginetools.txt")).unwrap(),
    );
    m.insert(
        "csgo/gameinfo.gi".to_string(),
        fs::read(fixture("gameinfo.gi")).unwrap(),
    );
    assert_eq!(snap(&t.join("game")).len(), 2);
    m
}

#[test]
fn linux_ensure_current_busy_and_stale() {
    let w = W::new("ensure-linux");
    let t = w.fresh_linux("ensure-linux", false);
    let env = w.env(&t, P::Linux);
    let out = env.run(&["--platform", "linux", "ensure"]);
    assert_eq!(out.rc, 0, "a fresh tree is installed: {out}");
    // install's vdf writer and ensure's producer share the bytes: all-current right after,
    // no new backup, not even an mtime changed
    let n = w.backups("backup-linux-").len();
    let before = mtimes(&t.join("game"));
    let out = env.run(&["--platform", "linux", "ensure"]);
    assert!(out.rc == 0 && out.has("all artefacts current"), "{out}");
    assert_eq!(w.backups("backup-linux-").len(), n);
    assert_eq!(mtimes(&t.join("game")), before);
    // busy + current: exit 0 untouched
    let busy = env
        .clone()
        .set("STUB_MODE", "busy")
        .set("STUB_SRC", w.vmp())
        .set("GUARD_FAKE", "1");
    let out = busy.run(&["--platform", "linux", "ensure"]);
    assert!(out.rc == 0 && out.has("all artefacts current"), "{out}");
    assert_eq!(mtimes(&t.join("game")), before);
    // busy + stale (the mmload .so changed): exit 4, named, untouched
    let mm = t.join("game/csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so");
    fs::write(&mm, [fs::read(&mm).unwrap(), b"stale\n".to_vec()].concat()).unwrap();
    let before = snap(&t.join("game"));
    let out = busy.run(&["--platform", "linux", "ensure"]);
    assert_eq!(out.rc, 4, "{out}");
    assert!(out.has(NAME), "{out}");
    assert_eq!(snap(&t.join("game")), before);
}

#[test]
fn a_killed_install_leaves_no_half_file_no_temp_no_partial_and_ensure_repairs() {
    let w = W::new("fault");
    // wine: killed between the manifest temp and its rename
    let t = w.fresh_wine("fault-wine");
    let env = w.env(&t, P::Wine);
    let out = env
        .clone()
        .set("INSTALL_RS_FAULT", "pre-mv:game/bin/sdkenginetools.txt")
        .run(&["install"]);
    assert_eq!(out.rc, 99, "{out}");
    assert!(
        same(
            &fixture("sdkenginetools.txt"),
            &t.join("game/bin/sdkenginetools.txt")
        ),
        "original intact"
    );
    assert!(temps_in(&t).is_empty(), "{:?}", temps_in(&t));
    assert_eq!(env.run(&["ensure"]).rc, 0);
    let out = env.run(&["ensure"]);
    assert!(out.rc == 0 && out.has("all artefacts current"), "{out}");

    // linux: killed at the vdf rename inside the namespace op
    let tl = w.fresh_linux("fault-linux", false);
    let envl = w.env(&tl, P::Linux);
    let out = envl
        .clone()
        .set("INSTALL_RS_FAULT", "pre-mv:csgo/addons/metamod/cs2mcp.vdf")
        .run(&["--platform", "linux", "install"]);
    assert_eq!(out.rc, 99, "{out}");
    assert!(
        !tl.join("game/csgo/addons/metamod/cs2mcp.vdf").exists(),
        "no half-written vdf"
    );
    assert!(same(
        &fixture("gameinfo.gi"),
        &tl.join("game/csgo/gameinfo.gi")
    ));
    assert!(temps_in(&tl).is_empty(), "{:?}", temps_in(&tl));
    assert_eq!(envl.run(&["--platform", "linux", "ensure"]).rc, 0);
    let out = envl.run(&["--platform", "linux", "ensure"]);
    assert!(out.rc == 0 && out.has("all artefacts current"), "{out}");

    // a kill mid-backup leaves no backup (final or .partial); the next install works
    let tb = w.fresh_wine("fault-backup");
    let envb = w.env(&tb, P::Wine);
    let n = w.backups("backup-rs-").len();
    let out = envb
        .clone()
        .set("INSTALL_RS_FAULT", "mid-backup")
        .run(&["install"]);
    assert_eq!(out.rc, 99, "{out}");
    assert_eq!(w.backups("backup-rs-").len(), n);
    assert!(!w
        .backups("backup-")
        .iter()
        .any(|b| b.to_string_lossy().ends_with(".partial")));
    assert_eq!(envb.run(&["install"]).rc, 0);
    let out = envb.run(&["uninstall"]);
    assert_eq!(out.rc, 0, "{out}");

    let tbl = w.fresh_linux("fault-backup-linux", false);
    let envbl = w.env(&tbl, P::Linux);
    let n = w.backups("backup-linux-").len();
    let out = envbl.clone().set("INSTALL_RS_FAULT", "mid-backup").run(&[
        "--platform",
        "linux",
        "install",
    ]);
    assert_eq!(out.rc, 99, "{out}");
    assert_eq!(w.backups("backup-linux-").len(), n);
    assert_eq!(envbl.run(&["--platform", "linux", "install"]).rc, 0);
    // INSTALLED lists what install created only; EDITS_WROTE is empty
    let bk = w.newest_backup(&tbl.join("game"));
    let inst = read(&bk.join("INSTALLED"));
    assert!(inst.contains("csgo/addons/metamod/cs2mcp.vdf"), "{inst}");
    assert!(
        inst.contains("csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so"),
        "{inst}"
    );
    assert!(!inst.contains("unrelated-stays"), "{inst}");
    assert_eq!(read(&bk.join("EDITS_WROTE")), "");
}

#[test]
fn uninstall_reverts_our_entry_in_the_current_manifest() {
    let w = W::new("revert");
    let t = w.fresh_wine("revert-wine");
    let env = w.env(&t, P::Wine);
    assert_eq!(env.run(&["install"]).rc, 0);
    // a Steam update adds an entry of its own after install
    let man = t.join("game/bin/sdkenginetools.txt");
    let text = read(&man).replacen(
        MAN_ANCHOR,
        &format!("\t\t{{\n\t\t\tm_Name = \"steam_update_tool\"\n\t\t}},\n{MAN_ANCHOR}"),
        1,
    );
    fs::write(&man, text).unwrap();
    let out = env.run(&["uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    let text = read(&man);
    assert!(
        text.contains("m_Name = \"steam_update_tool\""),
        "the Steam modification stays"
    );
    assert!(
        !text.contains("m_Name = \"cs2_bridge\""),
        "exactly our entry is removed"
    );
    assert!(same(
        &fixture("gameinfo.gi"),
        &t.join("game/csgo/gameinfo.gi")
    ));

    // a manifest that ALREADY carried our entry before the first install: not recorded as ours,
    // so uninstall leaves it
    let tp = w.p("trees/revert-preentry");
    write(
        &tp.join("game/csgo/gameinfo.gi"),
        fs::read(fixture("gameinfo.gi")).unwrap(),
    );
    let pre = read(&fixture("sdkenginetools.txt")).replacen(
        MAN_ANCHOR,
        &format!("{OUR_ENTRY}{MAN_ANCHOR}"),
        1,
    );
    write(&tp.join("game/bin/sdkenginetools.txt"), &pre);
    let envp = w.env(&tp, P::Wine);
    assert_eq!(envp.run(&["install"]).rc, 0);
    let out = envp.run(&["uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(read(&tp.join("game/bin/sdkenginetools.txt")), pre);
}

#[test]
fn uninstall_is_driven_by_the_installed_list() {
    let w = W::new("installed-list");
    // the Metamod build is gone by uninstall time: the recorded list still removes every file
    let t = w.fresh_linux("installed-list", false);
    let pre = pristine(&w, &t, "il.pristine");
    copy_tree(&w.p("builds/mmlinux"), &w.p("builds/mmlinux-copy"));
    let env = w
        .env(&t, P::Linux)
        .set("CS2_MM_LINUX", w.p("builds/mmlinux-copy"));
    assert_eq!(env.run(&["--platform", "linux", "install"]).rc, 0);
    fs::remove_dir_all(w.p("builds/mmlinux-copy")).unwrap();
    let out = env.run(&["--platform", "linux", "uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(listing(&t), listing(&pre));

    // no INSTALLED list anywhere: refuse, touch nothing
    let tn = w.fresh_wine("installed-list-missing");
    let envn = w.env(&tn, P::Wine);
    assert_eq!(envn.run(&["install"]).rc, 0);
    let bk = w.newest_backup(&tn);
    fs::remove_file(bk.join("INSTALLED")).unwrap();
    let before = snap(&tn.join("game"));
    let out = envn.run(&["uninstall"]);
    assert!(out.rc != 0 && out.has("INSTALLED"), "{out}");
    assert_eq!(snap(&tn.join("game")), before);
}

#[test]
fn ensure_without_a_build_repairs_config_or_says_build_it_first() {
    let w = W::new("nobuild");
    let t = w.fresh_wine("ensure-nobuild");
    let env = w.env(&t, P::Wine);
    assert_eq!(env.run(&["install"]).rc, 0);
    for f in ["bin/fake.dll", "bin/launch.exe", "bin/mmload.dll"] {
        fs::rename(w.p(f), w.p(&format!("{f}.hidden"))).unwrap();
    }
    fs::copy(
        fixture("sdkenginetools.txt"),
        t.join("game/bin/sdkenginetools.txt"),
    )
    .unwrap();
    let bins = snap(&t.join("game/bin/win64"));
    let out = env.run(&["ensure"]);
    assert_eq!(out.rc, 0, "{out}");
    assert!(read(&t.join("game/bin/sdkenginetools.txt")).contains("m_Name = \"cs2_bridge\""));
    assert_eq!(
        snap(&t.join("game/bin/win64")),
        bins,
        "the installed binaries stay byte-identical"
    );
    for f in ["bin/fake.dll", "bin/launch.exe", "bin/mmload.dll"] {
        fs::rename(w.p(&format!("{f}.hidden")), w.p(f)).unwrap();
    }

    // nothing installed + no build: exit 1, build it first
    let tn = w.fresh_wine("ensure-nobuild-neg");
    fs::rename(w.p("bin/fake.dll"), w.p("bin/fake.dll.hidden")).unwrap();
    let out = w.env(&tn, P::Wine).run(&["ensure"]);
    assert!(out.rc == 1 && out.has("BUILD IT FIRST"), "{out}");
    fs::rename(w.p("bin/fake.dll.hidden"), w.p("bin/fake.dll")).unwrap();
    // the DLL is built, the launcher is not and nothing is installed: exit 1
    let tnl = w.fresh_wine("ensure-nobuild-launcher");
    fs::rename(w.p("bin/launch.exe"), w.p("bin/launch.exe.hidden")).unwrap();
    let out = w.env(&tnl, P::Wine).run(&["ensure"]);
    assert!(out.rc == 1 && out.has("BUILD IT FIRST"), "{out}");
    fs::rename(w.p("bin/launch.exe.hidden"), w.p("bin/launch.exe")).unwrap();
    // linux: the same contract for the bridge .so and the mmload .so
    let tl = w.fresh_linux("ensure-nobuild-linux", false);
    fs::rename(w.p("bin/fake.so"), w.p("bin/fake.so.hidden")).unwrap();
    let out = w.env(&tl, P::Linux).run(&["--platform", "linux", "ensure"]);
    assert!(out.rc == 1 && out.has("BUILD IT FIRST"), "{out}");
    fs::rename(w.p("bin/fake.so.hidden"), w.p("bin/fake.so")).unwrap();
    fs::rename(w.p("bin/mmload.so"), w.p("bin/mmload.so.hidden")).unwrap();
    let out = w.env(&tl, P::Linux).run(&["--platform", "linux", "ensure"]);
    assert!(out.rc == 1 && out.has("BUILD IT FIRST"), "{out}");
}

#[test]
fn a_foreign_metaplugins_ini_is_never_ours() {
    let w = W::new("metaplugins");
    let foreign = "plugin_other  ../bin/win64/someotherplugin.dll\n";
    let t = w.fresh_wine("metaplugins");
    write(&t.join("game/csgo/addons/metamod/metaplugins.ini"), foreign);
    let env = w.env(&t, P::Wine);
    assert_eq!(env.run(&["install"]).rc, 0);
    assert_eq!(
        read(&t.join("game/csgo/addons/metamod/metaplugins.ini")),
        foreign
    );
    let out = env.run(&["ensure"]);
    assert!(
        out.rc == 0 && out.has("all artefacts current"),
        "presence only: {out}"
    );
    assert_eq!(env.run(&["uninstall"]).rc, 0);
    assert_eq!(
        read(&t.join("game/csgo/addons/metamod/metaplugins.ini")),
        foreign
    );

    let tl = w.fresh_linux("metaplugins-linux", false);
    write(
        &tl.join("game/csgo/addons/metamod/metaplugins.ini"),
        foreign,
    );
    let envl = w.env(&tl, P::Linux);
    assert_eq!(envl.run(&["--platform", "linux", "install"]).rc, 0);
    assert_eq!(
        read(&tl.join("game/csgo/addons/metamod/metaplugins.ini")),
        foreign
    );
    let out = envl.run(&["--platform", "linux", "ensure"]);
    assert!(out.rc == 0 && out.has("all artefacts current"), "{out}");
    assert_eq!(envl.run(&["--platform", "linux", "uninstall"]).rc, 0);
    assert_eq!(
        read(&tl.join("game/csgo/addons/metamod/metaplugins.ini")),
        foreign
    );
}

#[test]
fn the_lock_serialises_writers_and_lets_shared_readers_through() {
    let w = W::new("lock");
    let t = w.fresh_wine("lock");
    let env = w.env(&t, P::Wine);
    assert_eq!(env.run(&["install"]).rc, 0);
    // hold the lock the way another install (another checkout, another session) would
    let lock = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(w.p("project/.podman/install-rs-wine.lock"))
        .unwrap();
    lock.lock().unwrap();
    let quick = env.clone().set("INSTALL_RS_LOCK_WAIT", "1");
    let out = quick.run(&["ensure"]);
    assert!(
        out.rc == 4 && out.has("holds") && out.has("install-rs-wine.lock"),
        "{out}"
    );
    // a shared holder does not block --dry-run (shared)
    lock.unlock().unwrap();
    lock.lock_shared().unwrap();
    let out = quick.run(&["--dry-run", "ensure"]);
    assert_eq!(out.rc, 0, "{out}");
    lock.unlock().unwrap();
    assert_eq!(env.run(&["ensure"]).rc, 0);
}

#[test]
fn the_real_tree_decision_is_made_on_realpath() {
    let w = W::new("realpath");
    // the "real" Wine app dir is faked; the stub reports a container mounting it
    let t = w.fresh_wine("realpath");
    let real = t.join("app");
    let env = w
        .env(&t, P::Wine)
        .set("STUB_MODE", "busy")
        .set("STUB_SRC", &real)
        .set("INSTALL_RS_REAL_APP", &real);
    for spelling in [
        format!("{}/game/", real.display()),
        format!("{}/game/x/..", real.display()),
    ] {
        let out = env.clone().set("CS2_GAME", &spelling).run(&["install"]);
        assert!(out.rc == 4 && out.has(NAME), "{spelling}: {out}");
    }
    std::os::unix::fs::symlink(&real, w.p("trees/link-app")).unwrap();
    let out = env
        .clone()
        .set("CS2_GAME", w.p("trees/link-app/game"))
        .run(&["install"]);
    assert_eq!(out.rc, 4, "a symlink spelling: {out}");
    // a different tree skips the guard
    let other = w.fresh_wine("realpath-other");
    let out = env
        .clone()
        .set("CS2_GAME", other.join("game"))
        .run(&["install"]);
    assert_eq!(out.rc, 0, "{out}");

    // linux: the same decision against the stub volume's mountpoint
    let tl = w.fresh_linux("realpath-linux", false);
    let envl = w
        .env(&tl, P::Linux)
        .set("STUB_MODE", "busy")
        .set("STUB_SRC", w.vmp());
    let vol_game = format!(
        "{}/steamapps/common/Counter-Strike Global Offensive/game/",
        w.vmp().display()
    );
    let out =
        envl.clone()
            .set("CS2_LINUX_GAME", &vol_game)
            .run(&["--platform", "linux", "install"]);
    assert!(out.rc == 4 && out.has(NAME), "{out}");
    std::os::unix::fs::symlink(w.vmp(), w.p("trees/link-vol")).unwrap();
    let via_link = w.p("trees/link-vol/steamapps/common/Counter-Strike Global Offensive/game");
    let out = envl
        .clone()
        .set("CS2_LINUX_GAME", via_link)
        .run(&["--platform", "linux", "install"]);
    assert_eq!(out.rc, 4, "{out}");
    let other = w.fresh_linux("realpath-linux-other", false);
    let out = envl
        .clone()
        .set("CS2_LINUX_GAME", other.join("game"))
        .run(&["--platform", "linux", "install"]);
    assert_eq!(out.rc, 0, "{out}");
}

#[test]
fn current_means_what_the_edit_would_produce() {
    let w = W::new("content");
    let t = w.fresh_wine("truncated");
    let env = w.env(&t, P::Wine);
    assert_eq!(env.run(&["install"]).rc, 0);
    fs::copy(
        fixture("sdkenginetools.txt"),
        t.join("game/bin/sdkenginetools.txt"),
    )
    .unwrap();
    let out = env.run(&["ensure"]);
    assert!(out.rc == 0 && out.has("is not what install"), "{out}");
    assert!(read(&t.join("game/bin/sdkenginetools.txt")).contains("m_Name = \"cs2_bridge\""));

    // no anchor at all: fatal "game file damaged" (not "build it first")
    let tn = w.fresh_wine("dmg-truncated");
    let envn = w.env(&tn, P::Wine);
    assert_eq!(envn.run(&["install"]).rc, 0);
    fs::write(tn.join("game/bin/sdkenginetools.txt"), "{\n}\n").unwrap();
    let out = envn.run(&["ensure"]);
    assert!(out.rc == 1 && out.has("game file damaged"), "{out}");

    // --dry-run ensure: exit 3 when stale, touching nothing
    let t3 = w.fresh_wine("dryrun");
    let env3 = w.env(&t3, P::Wine);
    assert_eq!(env3.run(&["install"]).rc, 0);
    fs::copy(
        fixture("sdkenginetools.txt"),
        t3.join("game/bin/sdkenginetools.txt"),
    )
    .unwrap();
    let before = snap(&t3.join("game"));
    let out = env3.run(&["--dry-run", "ensure"]);
    assert_eq!(out.rc, 3, "{out}");
    assert_eq!(snap(&t3.join("game")), before);

    // gameinfo.gi is not read at all: absent (linux) or garbage (wine) still reads current
    let tl = w.fresh_linux("nogameinfo-linux", false);
    let envl = w.env(&tl, P::Linux);
    assert_eq!(envl.run(&["--platform", "linux", "install"]).rc, 0);
    fs::remove_file(tl.join("game/csgo/gameinfo.gi")).unwrap();
    let out = envl.run(&["--platform", "linux", "ensure"]);
    assert!(out.rc == 0 && out.has("all artefacts current"), "{out}");
    let tw = w.fresh_wine("nogameinfo-wine");
    let envw = w.env(&tw, P::Wine);
    assert_eq!(envw.run(&["install"]).rc, 0);
    fs::write(
        tw.join("game/csgo/gameinfo.gi"),
        "garbage, not a gameinfo\n",
    )
    .unwrap();
    let out = envw.run(&["ensure"]);
    assert!(out.rc == 0 && out.has("all artefacts current"), "{out}");
}

#[test]
fn damaged_is_rare_and_actionable() {
    let w = W::new("dmg-rare");
    // normalised anchors: a re-spaced hammer line + a trailing comment/blank are not damage
    let t = w.fresh_wine("dmg-rare");
    let env = w.env(&t, P::Wine);
    assert_eq!(env.run(&["install"]).rc, 0);
    let man = t.join("game/bin/sdkenginetools.txt");
    let text = read(&man).replacen("m_Name = \"hammer\"", "m_Name     =    \"hammer\"", 1)
        + "\n// a trailing Valve comment\n\n";
    fs::write(&man, text).unwrap();
    let out = env.run(&["--dry-run", "ensure"]);
    assert!(out.rc == 0 && !out.has("damaged"), "{out}");

    // a real anchor missing: damaged, the message names it and says what to do
    let t3 = w.fresh_wine("dmg-anchor-msg");
    let env3 = w.env(&t3, P::Wine);
    assert_eq!(env3.run(&["install"]).rc, 0);
    let man = t3.join("game/bin/sdkenginetools.txt");
    let text = read(&man);
    let start = text.find(MAN_ANCHOR).unwrap();
    let end = start + text[start..].find("\t\t},\n").unwrap() + "\t\t},\n".len();
    fs::write(&man, format!("{}{}", &text[..start], &text[end..])).unwrap();
    let out = env3.run(&["--dry-run", "ensure"]);
    assert_eq!(out.rc, 1, "{out}");
    assert!(out.has("missing anchor 'm_Name = \"hammer\"'"), "{out}");
    assert!(
        out.has("verify the game files; if Valve changed this line, update the manifest anchor"),
        "{out}"
    );
}

#[test]
fn a_truncated_manifest_with_our_entry_reads_damaged() {
    let w = W::new("anchors");
    let t = w.fresh_wine("anchors-manifest");
    let env = w.env(&t, P::Wine);
    assert_eq!(env.run(&["install"]).rc, 0);
    let bk = w.newest_backup(&t);
    let anchors = read(&bk.join("ANCHORS"));
    assert_eq!(
        anchors, "manifest\tm_Name = \"hammer\"\n",
        "the manifest anchor only"
    );
    let man = t.join("game/bin/sdkenginetools.txt");
    let text = read(&man);
    let i = text.find("m_Name = \"hammer\"").unwrap();
    fs::write(&man, &text[..i]).unwrap();
    let before = snap(&t.join("game"));
    let out = env.run(&["--dry-run", "ensure"]);
    assert!(out.rc == 1 && out.has("game file damaged"), "{out}");
    assert_eq!(
        snap(&t.join("game")),
        before,
        "the damaged manifest is not touched"
    );
}

#[test]
fn ensure_needs_no_write_access_to_the_tree() {
    let w = W::new("readonly");
    let t = w.fresh_linux("ensure-readonly", false);
    let env = w.env(&t, P::Linux);
    assert_eq!(env.run(&["--platform", "linux", "install"]).rc, 0);
    fn chmod_dirs(d: &Path, mode: u32) {
        for e in fs::read_dir(d).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                chmod_dirs(&e.path(), mode);
            }
        }
        fs::set_permissions(d, fs::Permissions::from_mode(mode)).unwrap();
    }
    let before = listing(&t.join("game"));
    chmod_dirs(&t.join("game"), 0o555);
    let dry = env.run(&["--platform", "linux", "--dry-run", "ensure"]);
    let plain = env.run(&["--platform", "linux", "ensure"]);
    chmod_dirs(&t.join("game"), 0o755);
    assert_eq!(dry.rc, 0, "{dry}");
    assert!(
        plain.rc == 0 && plain.has("all artefacts current"),
        "{plain}"
    );
    assert_eq!(
        listing(&t.join("game")),
        before,
        "ensure created nothing in the tree"
    );
}

#[test]
fn a_created_metaplugins_ini_is_removed_only_while_unchanged() {
    let w = W::new("mpgate");
    let other = "plugin_other  ../bin/win64/someotherplugin.dll\n";
    let t = w.fresh_linux("mpgate-linux", false);
    let env = w.env(&t, P::Linux);
    assert_eq!(env.run(&["--platform", "linux", "install"]).rc, 0);
    let mp = t.join("game/csgo/addons/metamod/metaplugins.ini");
    fs::write(&mp, read(&mp) + other).unwrap();
    let out = env.run(&["--platform", "linux", "uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert!(
        read(&mp).contains("plugin_other"),
        "another plugin's line survives"
    );
    assert!(
        out.has("kept: csgo/addons/metamod/metaplugins.ini"),
        "{out}"
    );

    let t2 = w.fresh_linux("mpgate-linux-clean", false);
    let env2 = w.env(&t2, P::Linux);
    assert_eq!(env2.run(&["--platform", "linux", "install"]).rc, 0);
    let out = env2.run(&["--platform", "linux", "uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert!(!t2.join("game/csgo/addons/metamod/metaplugins.ini").exists());
    assert!(
        out.has("removed (added by install): csgo/addons/metamod/metaplugins.ini"),
        "{out}"
    );

    let tw = w.fresh_wine("mpgate-wine");
    let envw = w.env(&tw, P::Wine);
    assert_eq!(envw.run(&["install"]).rc, 0);
    let mp = tw.join("game/csgo/addons/metamod/metaplugins.ini");
    fs::write(&mp, read(&mp) + other).unwrap();
    assert_eq!(envw.run(&["uninstall"]).rc, 0);
    assert!(read(&mp).contains("plugin_other"));
}

#[test]
fn a_killed_install_stays_uninstallable() {
    let w = W::new("killed");
    let t = w.fresh_wine("s1-wine");
    let pre = pristine(&w, &t, "s1.pristine");
    let env = w.env(&t, P::Wine);
    let out = env
        .clone()
        .set("INSTALL_RS_FAULT", "pre-mv:game/bin/sdkenginetools.txt")
        .run(&["install"]);
    assert_eq!(out.rc, 99, "{out}");
    let bk = w.newest_backup(&t);
    assert!(
        !read(&bk.join("INSTALLED")).is_empty(),
        "the finalised backup carries the PLANNED list"
    );
    assert_eq!(env.run(&["ensure"]).rc, 0);
    let out = env.run(&["uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(listing(&t), listing(&pre));

    let tl = w.fresh_linux("s1-linux", false);
    let prel = pristine(&w, &tl, "s1l.pristine");
    let envl = w.env(&tl, P::Linux);
    let out = envl
        .clone()
        .set("INSTALL_RS_FAULT", "pre-mv:csgo/addons/metamod/cs2mcp.vdf")
        .run(&["--platform", "linux", "install"]);
    assert_eq!(out.rc, 99, "{out}");
    let bk = w.newest_backup(&tl.join("game"));
    assert!(!read(&bk.join("INSTALLED")).is_empty());
    assert_eq!(envl.run(&["--platform", "linux", "ensure"]).rc, 0);
    let out = envl.run(&["--platform", "linux", "uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(listing(&tl), listing(&prel));
}

#[test]
fn uninstall_reads_installed_from_every_backup() {
    let w = W::new("union");
    let t = w.fresh_wine("installed-union");
    let env = w.env(&t, P::Wine);
    assert_eq!(env.run(&["install"]).rc, 0);
    let bk1 = w.newest_backup(&t);
    fs::remove_file(bk1.join("INSTALLED")).unwrap(); // an old-format backup
    fs::remove_file(t.join("game/csgo/addons/metamod/cs2mcp.vdf")).unwrap(); // lost again
    assert_eq!(env.run(&["ensure"]).rc, 0);
    let out = env.run(&["uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert!(!t.join("game/csgo/addons/metamod/cs2mcp.vdf").exists());
    assert!(
        out.has("removed (created by install): game/csgo/addons/metamod/cs2mcp.vdf"),
        "{out}"
    );
    assert!(out.has("WARN: no INSTALLED list"), "{out}");
}

#[test]
fn a_killed_uninstall_is_atomic_and_the_next_one_completes() {
    let w = W::new("uninst-atomic");
    let t = w.fresh_wine("ua-wine");
    write(
        &t.join("game/csgo/addons/metamod/metaplugins.ini"),
        "foreign metaplugins (pre-existing)\n",
    );
    let pre = pristine(&w, &t, "ua.pristine");
    let env = w.env(&t, P::Wine);
    assert_eq!(env.run(&["install"]).rc, 0);
    let out = env
        .clone()
        .set(
            "INSTALL_RS_FAULT",
            "pre-mv:uninstall:game/bin/sdkenginetools.txt",
        )
        .run(&["uninstall"]);
    assert_eq!(out.rc, 99, "{out}");
    assert!(
        read(&t.join("game/bin/sdkenginetools.txt")).contains("m_Name = \"cs2_bridge\""),
        "intact"
    );
    assert!(temps_in(&t).is_empty(), "{:?}", temps_in(&t));
    let out = env.run(&["uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(listing(&t), listing(&pre));

    // a kill at a restore's rename (a pre-seeded server.dll install overwrote)
    let t2 = w.fresh_wine("ua-wine2");
    write(
        &t2.join("game/csgo/addons/metamod/metaplugins.ini"),
        "foreign metaplugins (pre-existing)\n",
    );
    write(
        &t2.join("game/csgo/addons/metamod/bin/win64/server.dll"),
        "OLD MM CORE\n",
    );
    let pre2 = pristine(&w, &t2, "ua2.pristine");
    let env2 = w.env(&t2, P::Wine);
    assert_eq!(env2.run(&["install"]).rc, 0);
    assert!(
        same(
            &t2.join("game/csgo/addons/metamod/bin/win64/server.dll"),
            &w.p("bin/fake.dll")
        ),
        "test sound"
    );
    let out = env2
        .clone()
        .set(
            "INSTALL_RS_FAULT",
            "pre-mv:uninstall-restore:game/csgo/addons/metamod/bin/win64/server.dll",
        )
        .run(&["uninstall"]);
    assert_eq!(out.rc, 99, "{out}");
    assert!(temps_in(&t2).is_empty(), "{:?}", temps_in(&t2));
    let out = env2.run(&["uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(listing(&t2), listing(&pre2));

    // linux: the restore's temp is registered too
    let tl = w.fresh_linux("ua-linux", false);
    write(
        &tl.join("game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"),
        "FOREIGN-MM\n",
    );
    let prel = pristine(&w, &tl, "ual.pristine");
    let envl = w.env(&tl, P::Linux);
    assert_eq!(envl.run(&["--platform", "linux", "install"]).rc, 0);
    let out = envl
        .clone()
        .set(
            "INSTALL_RS_FAULT",
            "pre-mv:uninstall-restore:csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so",
        )
        .run(&["--platform", "linux", "uninstall"]);
    assert_eq!(out.rc, 99, "{out}");
    assert!(same(
        &w.p("bin/fake.so"),
        &tl.join("game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so")
    ));
    assert!(temps_in(&tl).is_empty(), "{:?}", temps_in(&tl));
    let out = envl.run(&["--platform", "linux", "uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(listing(&tl), listing(&prel));
}

#[test]
fn uninstall_restores_an_overwritten_pre_existing_file() {
    let w = W::new("s2");
    let t = w.fresh_linux("s2-linux", false);
    write(
        &t.join("game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"),
        "OLD MM CORE\n",
    );
    let pre = pristine(&w, &t, "s2.pristine");
    let env = w.env(&t, P::Linux);
    assert_eq!(env.run(&["--platform", "linux", "install"]).rc, 0);
    let core = t.join("game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so");
    assert!(
        same(&core, &w.p("bin/fake.so")),
        "install really overwrote it"
    );
    // the backup lists it (SHA256SUMS + WROTE_OVER with the sha of what we wrote)
    let bk = w.newest_backup(&t.join("game"));
    let rel = "csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so";
    assert_eq!(
        read(&bk.join("SHA256SUMS")),
        format!("{}  {rel}\n", sha(b"OLD MM CORE\n"))
    );
    assert_eq!(
        read(&bk.join("WROTE_OVER")),
        format!("{rel} {}\n", sha(&fake_elf()))
    );
    let out = env.run(&["--platform", "linux", "uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(read(&core), "OLD MM CORE\n");
    assert_eq!(listing(&t), listing(&pre));
}

#[test]
fn only_what_install_overwrote_is_restored() {
    let w = W::new("wro");
    // (i) a pre-existing metaplugins.ini edited after install: the edit survives
    let t = w.fresh_wine("wro-metaplugins");
    write(
        &t.join("game/csgo/addons/metamod/metaplugins.ini"),
        "plugin_other line\n",
    );
    let env = w.env(&t, P::Wine);
    assert_eq!(env.run(&["install"]).rc, 0);
    let mp = t.join("game/csgo/addons/metamod/metaplugins.ini");
    fs::write(&mp, read(&mp) + "plugin_other EDITED after install\n").unwrap();
    assert_eq!(env.run(&["uninstall"]).rc, 0);
    assert!(read(&mp).contains("EDITED after install"));

    // (ii) linux: a foreign vdf its owner removed after install stays removed
    let tl = w.fresh_linux("wro-foreign-removed", false);
    write(
        &tl.join("game/csgo/addons/metamod/x.vdf"),
        "a foreign plugin vdf\n",
    );
    let envl = w.env(&tl, P::Linux);
    assert_eq!(envl.run(&["--platform", "linux", "install"]).rc, 0);
    fs::remove_file(tl.join("game/csgo/addons/metamod/x.vdf")).unwrap();
    assert_eq!(envl.run(&["--platform", "linux", "uninstall"]).rc, 0);
    assert!(!tl.join("game/csgo/addons/metamod/x.vdf").exists());

    // (iii) linux: a foreign subdir removed after install: uninstall completes
    let t3 = w.fresh_linux("wro-foreign-subdir", false);
    write(
        &t3.join("game/csgo/addons/metamod/sub/foo.vdf"),
        "foreign sub file\n",
    );
    let env3 = w.env(&t3, P::Linux);
    assert_eq!(env3.run(&["--platform", "linux", "install"]).rc, 0);
    fs::remove_dir_all(t3.join("game/csgo/addons/metamod/sub")).unwrap();
    let out = env3.run(&["--platform", "linux", "uninstall"]);
    assert_eq!(out.rc, 0, "{out}");

    // (v) a WROTE_OVER file changed after install: left alone, with the message
    let t4 = w.fresh_wine("wro-changed");
    write(
        &t4.join("game/csgo/addons/metamod/bin/win64/server.dll"),
        "OLD MM CORE\n",
    );
    let env4 = w.env(&t4, P::Wine);
    assert_eq!(env4.run(&["install"]).rc, 0);
    let srv = t4.join("game/csgo/addons/metamod/bin/win64/server.dll");
    fs::write(&srv, "CHANGED AFTER INSTALL\n").unwrap();
    let out = env4.run(&["uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert!(
        out.has("not restored: game/csgo/addons/metamod/bin/win64/server.dll"),
        "{out}"
    );
    assert_eq!(read(&srv), "CHANGED AFTER INSTALL\n");

    // control: the unchanged overwritten file IS restored
    let t5 = w.fresh_wine("wro-restored");
    write(
        &t5.join("game/csgo/addons/metamod/bin/win64/server.dll"),
        "OLD MM CORE\n",
    );
    let env5 = w.env(&t5, P::Wine);
    assert_eq!(env5.run(&["install"]).rc, 0);
    assert_eq!(env5.run(&["uninstall"]).rc, 0);
    assert_eq!(
        read(&t5.join("game/csgo/addons/metamod/bin/win64/server.dll")),
        "OLD MM CORE\n"
    );
}

#[test]
fn the_installed_union_never_deletes_a_pre_install_file() {
    let w = W::new("nopredelete");
    let t = w.fresh_wine("npd-wine");
    write(
        &t.join("game/csgo/addons/metamod/cs2mcp.vdf"),
        "PRE-INSTALL vdf bytes\n",
    );
    let pre = pristine(&w, &t, "npd.pristine");
    let env = w.env(&t, P::Wine);
    assert_eq!(env.run(&["install"]).rc, 0);
    fs::remove_file(t.join("game/csgo/addons/metamod/cs2mcp.vdf")).unwrap(); // the owner deletes it
    assert_eq!(env.run(&["ensure"]).rc, 0);
    assert_eq!(
        read(&t.join("game/csgo/addons/metamod/cs2mcp.vdf")),
        WINE_VDF,
        "ensure recreated ours"
    );
    let out = env.run(&["uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(listing(&t), listing(&pre));
    assert!(
        out.has("pre-existed the first install - NOT deleted"),
        "{out}"
    );

    let tl = w.fresh_linux("npd-linux", false);
    write(
        &tl.join("game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"),
        "PRE-INSTALL MM CORE\n",
    );
    let prel = pristine(&w, &tl, "npdl.pristine");
    let envl = w.env(&tl, P::Linux);
    assert_eq!(envl.run(&["--platform", "linux", "install"]).rc, 0);
    fs::remove_file(tl.join("game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"))
        .unwrap();
    assert_eq!(envl.run(&["--platform", "linux", "ensure"]).rc, 0);
    let out = envl.run(&["--platform", "linux", "uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(listing(&tl), listing(&prel));
}

#[test]
fn uninstall_walks_the_wrote_over_chain() {
    let w = W::new("chain");
    // (a) wine: foreign file -> install -> rebuild -> ensure -> uninstall = the pre-install bytes
    let t = w.fresh_wine("chain-rebuild-wine");
    write(
        &t.join("game/csgo/addons/metamod/bin/win64/server.dll"),
        "FOREIGN-SERVER\n",
    );
    let pre = pristine(&w, &t, "chain-w.pristine");
    let env = w.env(&t, P::Wine);
    assert_eq!(env.run(&["install"]).rc, 0);
    copy_tree(&w.p("builds/mmwin"), &w.p("builds/mmwin-chain"));
    let rebuilt = w.p("builds/mmwin-chain/metamod/bin/win64/server.dll");
    fs::write(
        &rebuilt,
        [fs::read(&rebuilt).unwrap(), b"rebuilt\n".to_vec()].concat(),
    )
    .unwrap();
    let envc = env.clone().set("CS2_MM", w.p("builds/mmwin-chain"));
    assert_eq!(envc.run(&["ensure"]).rc, 0);
    assert!(
        same(
            &t.join("game/csgo/addons/metamod/bin/win64/server.dll"),
            &rebuilt
        ),
        "test sound"
    );
    let out = envc.run(&["uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(
        read(&t.join("game/csgo/addons/metamod/bin/win64/server.dll")),
        "FOREIGN-SERVER\n"
    );
    assert_eq!(listing(&t), listing(&pre));

    // (b) linux: the same with metamod.2.cs2.so
    let tl = w.fresh_linux("chain-rebuild-linux", false);
    let core = "game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so";
    write(&tl.join(core), "FOREIGN-MM\n");
    let prel = pristine(&w, &tl, "chain-l.pristine");
    let envl = w.env(&tl, P::Linux);
    assert_eq!(envl.run(&["--platform", "linux", "install"]).rc, 0);
    copy_tree(&w.p("builds/mmlinux"), &w.p("builds/mmlinux-chain"));
    let rebuilt = w.p("builds/mmlinux-chain/metamod/bin/linuxsteamrt64/metamod.2.cs2.so");
    fs::write(
        &rebuilt,
        [fs::read(&rebuilt).unwrap(), b"rebuilt\n".to_vec()].concat(),
    )
    .unwrap();
    let envlc = envl
        .clone()
        .set("CS2_MM_LINUX", w.p("builds/mmlinux-chain"));
    assert_eq!(envlc.run(&["--platform", "linux", "ensure"]).rc, 0);
    assert!(same(&tl.join(core), &rebuilt), "test sound");
    let out = envlc.run(&["--platform", "linux", "uninstall"]);
    assert_eq!(out.rc, 0, "{out}");
    assert_eq!(read(&tl.join(core)), "FOREIGN-MM\n");
    assert_eq!(listing(&tl), listing(&prel));

    // (c) wine: P pre-existing -> the user replaces it with Q -> ensure overwrites Q ->
    //     uninstall puts Q back (not P, not ours)
    let t2 = w.fresh_wine("chain-userq-wine");
    let srv = t2.join("game/csgo/addons/metamod/bin/win64/server.dll");
    write(&srv, "P-ORIGINAL\n");
    let env2 = w.env(&t2, P::Wine);
    assert_eq!(env2.run(&["install"]).rc, 0);
    fs::write(&srv, "Q-USER-REPLACED\n").unwrap();
    assert_eq!(env2.run(&["ensure"]).rc, 0);
    assert_eq!(env2.run(&["uninstall"]).rc, 0);
    assert_eq!(read(&srv), "Q-USER-REPLACED\n");

    // (d) linux: the same
    let t3 = w.fresh_linux("chain-userq-linux", false);
    write(&t3.join(core), "P-ORIGINAL\n");
    let env3 = w.env(&t3, P::Linux);
    assert_eq!(env3.run(&["--platform", "linux", "install"]).rc, 0);
    fs::write(t3.join(core), "Q-USER-REPLACED\n").unwrap();
    assert_eq!(env3.run(&["--platform", "linux", "ensure"]).rc, 0);
    assert_eq!(env3.run(&["--platform", "linux", "uninstall"]).rc, 0);
    assert_eq!(read(&t3.join(core)), "Q-USER-REPLACED\n");

    // (e) a tampered NEWER backup is caught (every contributing backup is verified)
    let t4 = w.fresh_wine("chain-shaverify");
    let env4 = w.env(&t4, P::Wine);
    assert_eq!(env4.run(&["install"]).rc, 0);
    fs::copy(
        fixture("sdkenginetools.txt"),
        t4.join("game/bin/sdkenginetools.txt"),
    )
    .unwrap();
    assert_eq!(env4.run(&["ensure"]).rc, 0); // repairs: a second backup (of the manifest)
    let bk2 = w.newest_backup(&t4);
    let copy = bk2.join("game/bin/sdkenginetools.txt");
    fs::write(&copy, read(&copy) + "TAMPERED\n").unwrap();
    let out = env4.run(&["uninstall"]);
    assert!(
        out.rc != 0 && out.has("backup SHA256SUMS mismatch"),
        "{out}"
    );
}

#[test]
fn stray_temps_lock_wait_and_status() {
    let w = W::new("nits");
    // a SIGKILLed run's <dest>.tmp.<digits> is swept by the next install; a foreign one stays
    let t = w.fresh_wine("nit-stray-wine");
    write(&t.join("game/bin/sdkenginetools.txt.tmp.424242"), "stray\n");
    write(&t.join("game/csgo/other.tmp.notdigits"), "foreign\n");
    let env = w.env(&t, P::Wine);
    let out = env.run(&["install"]);
    assert_eq!(out.rc, 0, "{out}");
    assert!(!t.join("game/bin/sdkenginetools.txt.tmp.424242").exists());
    assert!(t.join("game/csgo/other.tmp.notdigits").is_file());
    assert!(out.has("removed a killed run's stray temp"), "{out}");

    let tl = w.fresh_linux("nit-stray-linux", false);
    write(
        &tl.join("game/csgo/addons/metamod/README.txt.tmp.888888"),
        "stray2\n",
    );
    let envl = w.env(&tl, P::Linux);
    assert_eq!(envl.run(&["--platform", "linux", "install"]).rc, 0);
    assert!(!tl
        .join("game/csgo/addons/metamod/README.txt.tmp.888888")
        .exists());
    let bk = w.newest_backup(&tl.join("game"));
    assert!(
        temps_in(&bk).is_empty(),
        "the backup never holds a stray temp: {:?}",
        temps_in(&bk)
    );

    // a non-numeric lock wait is a usage error
    let out = env
        .clone()
        .set("INSTALL_RS_LOCK_WAIT", "abc")
        .run(&["status"]);
    assert!(out.rc == 2 && out.has("INSTALL_RS_LOCK_WAIT"), "{out}");

    // status: the REAL label is decided on realpath (a trailing-slash spelling still says REAL)
    let ts = w.fresh_wine("nit-status");
    let envs = w.env(&ts, P::Wine);
    let out = envs.run(&["status"]);
    assert!(
        out.rc == 0 && out.has("[test tree]") && !out.has("gameinfo-era"),
        "{out}"
    );
    let out = envs
        .clone()
        .set("INSTALL_RS_REAL_APP", ts.join("app"))
        .set("CS2_GAME", format!("{}/app/game/", ts.display()))
        .run(&["status"]);
    assert!(out.has("REAL tools install"), "{out}");
    // status: the installed DLL and launcher say which build they match
    assert_eq!(envs.run(&["install"]).rc, 0);
    let out = envs.run(&["status"]);
    let line = |needle: &str| {
        out.text
            .lines()
            .find(|l| l.contains(needle))
            .unwrap_or("")
            .to_string()
    };
    assert!(line("tool dll").contains("matches build "), "{out}");
    assert!(
        line("cs2mcp_launch.exe").contains("matches build "),
        "{out}"
    );
    // linux status: both libs say which build they match
    let tls = w.fresh_linux("nit-status-linux", false);
    let envls = w.env(&tls, P::Linux);
    assert_eq!(envls.run(&["--platform", "linux", "install"]).rc, 0);
    let out = envls.run(&["--platform", "linux", "status"]);
    let line = |needle: &str| {
        out.text
            .lines()
            .find(|l| l.contains(needle))
            .unwrap_or("")
            .to_string()
    };
    assert!(
        line("libcs2mcp_mmload.so  present  sha256=").contains("matches build "),
        "{out}"
    );
    assert!(
        line("libcs2_bridge.so  present  sha256=").contains("matches build "),
        "{out}"
    );
}

#[test]
fn dry_run_touches_nothing_anywhere() {
    let w = W::new("dry");
    let t = w.fresh_wine("dry-wine");
    let env = w.env(&t, P::Wine);
    let before = listing(&t);
    let out = env.run(&["--dry-run", "install"]);
    assert_eq!(out.rc, 0, "{out}");
    assert!(out.has("DRY: cp"), "{out}");
    let out = env.run(&["--dry-run", "ensure"]);
    assert_eq!(out.rc, 3, "{out}");
    assert_eq!(listing(&t), before);
    assert!(w.backups("backup-").is_empty(), "no backup either");

    let tl = w.fresh_linux("dry-linux", false);
    let envl = w.env(&tl, P::Linux);
    let before = listing(&tl);
    assert_eq!(
        envl.run(&["--platform", "linux", "--dry-run", "install"])
            .rc,
        0
    );
    assert_eq!(
        envl.run(&["--platform", "linux", "--dry-run", "ensure"]).rc,
        3
    );
    assert_eq!(listing(&tl), before);
    // after a real install, --dry-run uninstall says what it would do and keeps everything
    assert_eq!(envl.run(&["--platform", "linux", "install"]).rc, 0);
    let before = listing(&tl);
    let out = envl.run(&["--platform", "linux", "--dry-run", "uninstall"]);
    assert!(out.rc == 0 && out.has("dry-run: would uninstall"), "{out}");
    assert_eq!(listing(&tl), before);
}

#[test]
fn un_migrated_gameinfo_era_records_are_refused() {
    let w = W::new("gi-records");
    let t = w.fresh_wine("gi-records");
    // a backup the gameinfo-era tool wrote: it recorded our `Game metamod` line
    let bk = w.p("target/bridge/backup-rs-20260101T000000.000000000Z");
    write(&bk.join("APP_TREE"), format!("{}\n", t.display()));
    write(&bk.join("SHA256SUMS"), "");
    write(
        &bk.join("EDITS_WROTE"),
        "gameinfo:Game metamod\ngameinfo:Write csgo\n",
    );
    write(&bk.join("INSTALLED"), "game/metamod/bin/win64/server.dll\n");
    let env = w.env(&t, P::Wine);
    let before = listing(&t);
    for cmd in ["install", "ensure", "uninstall"] {
        let out = env.run(&[cmd]);
        assert!(out.rc == 1 && out.has("gameinfo-era"), "{cmd}: {out}");
        assert_eq!(listing(&t), before, "{cmd} touched nothing");
    }
    let out = env.run(&["status"]);
    assert!(out.rc == 0 && out.has("gameinfo-era"), "{out}");
    // a later backup that recorded the migration's `reverted:` lines cancels them
    let bk2 = w.p("target/bridge/backup-rs-20260102T000000.000000000Z");
    write(&bk2.join("APP_TREE"), format!("{}\n", t.display()));
    write(&bk2.join("SHA256SUMS"), "");
    write(
        &bk2.join("EDITS_WROTE"),
        "reverted:gameinfo:Game metamod\nreverted:gameinfo:Write csgo\n",
    );
    let out = env.run(&["install"]);
    assert_eq!(out.rc, 0, "{out}");
}

#[test]
fn usage_errors_exit_2() {
    let w = W::new("usage");
    let t = w.fresh_wine("usage");
    let env = w.env(&t, P::Wine);
    assert_eq!(env.run(&[]).rc, 2);
    assert_eq!(env.run(&["frobnicate"]).rc, 2);
    assert_eq!(env.run(&["--platform", "mac", "status"]).rc, 2);
    assert_eq!(env.run(&["install", "uninstall"]).rc, 2);
    assert_eq!(
        env.run(&["--metamod", "install"]).rc,
        2,
        "the --metamod no-op alias is gone"
    );
}
