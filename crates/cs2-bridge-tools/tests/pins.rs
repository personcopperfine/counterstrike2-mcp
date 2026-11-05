//! `cargo bridge pins`: the renderer on a canned export (no game needed), and the generator end
//! to end against the real database and binaries (`#[ignore]`d: they need `$CS2_BINS`, default
//! `target/game-bins`, and the linux tree `target/game-bins-linux-<sigs/CURRENT_BUILD>`; run them
//! with `cargo test -p cs2-bridge-tools --test pins -- --ignored`).
//!
//! The game tests exercise the same resolution and fail-closed checks every run does:
//! * regen: the generator re-writes the committed pins.rs byte-identical, and `--check` agrees;
//! * linux fixture: a scratch signatures dir where one libclient.json entry gains
//!   `"bridge": "TEST_LINUX_PIN"` emits it in `pub mod linux` with the value expect[] records;
//! * linux ident: the libclient.json CCSGOInput vtable entry gains a `bridge` name and an
//!   `ident_slot`, whose identity row carries the slot target resolved through the .rela table
//!   (a .so keeps relocated vtable slots zero in the file); a wrong `ident_slot` is refused;
//! * negative: a module without its builds[CURRENT_BUILD] record is refused, nothing written.
//!
//! Every output, the committed pins.rs included, must build with
//! `rustc --edition 2021 --crate-type rlib -D warnings` (an unqualified `Pin` in `pub mod linux`
//! once failed exactly there).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;

use cs2_bridge_tools::pins::{render, Export, WinModule};

/// The checkout (the crate is `crates/cs2-bridge-tools`).
fn checkout() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/pins").join(name)
}

/// A fresh scratch directory under the target dir (never `/tmp`: it may be a RAM tmpfs).
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("pins").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `rustc --edition 2021 --crate-type rlib -D warnings` on `path`; panics with rustc's output
/// when it does not compile.
fn assert_compiles(path: &Path, what: &str) {
    let out = scratch(&format!("rustc-{what}")).join("pins.rlib");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let res = Command::new(rustc)
        .args(["--edition", "2021", "--crate-type", "rlib", "-D", "warnings", "-o"])
        .arg(&out)
        .arg(path)
        .output()
        .expect("running rustc");
    assert!(
        res.status.success(),
        "rustc -D warnings does not build {what} ({}):\n{}",
        path.display(),
        String::from_utf8_lossy(&res.stderr)
    );
}

// ------------------------------------------------------------------ renderer (no game)

/// The canned export's Windows module table.
const CANNED_MODULES: &[WinModule] =
    &[WinModule { file: "alpha.dll", pin: "PIN_ALPHA" }, WinModule { file: "beta.dll", pin: "PIN_BETA" }];

fn canned() -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(data("canned.json")).unwrap()).unwrap()
}

fn render_value(doc: serde_json::Value) -> anyhow::Result<String> {
    let export: Export = serde_json::from_value(doc)?;
    Ok(render(&export, CANNED_MODULES)?.text)
}

/// The canned export (tests/data/pins/canned.json: every section, an sdk-owned vslot sharing a
/// target, a relocated vslot prologue, identity slots on both platforms, a quoted note) renders
/// exactly tests/data/pins/canned.rs, and that compiles. After an intended renderer change,
/// regenerate the expected file with `PINS_BLESS=1 cargo test -p cs2-bridge-tools --test pins`
/// and review its diff.
#[test]
fn canned_export_renders_the_expected_file() {
    let text = render_value(canned()).unwrap();
    let expected = data("canned.rs");
    if std::env::var_os("PINS_BLESS").is_some() {
        std::fs::write(&expected, &text).unwrap();
    }
    assert_eq!(text, std::fs::read_to_string(&expected).unwrap(), "the canned render differs from tests/data/pins/canned.rs");
    assert_compiles(&expected, "canned");
}

/// The canned render's sections and rows, spelled out (what canned.rs is reviewed for).
#[test]
fn canned_export_rows() {
    let text = render_value(canned()).unwrap();
    // the sdk-owned vslot is not a const, but keeps its slot row; Foo and Bar share one prologue
    // row, Baz (relocated window) has none
    assert!(text.contains("pub const AVS_FOO: Pin = Pin { module: PIN_ALPHA, value: 3, kind: \"vslot\" };"));
    assert!(!text.contains("pub const AVS_BAR"));
    assert!(text.contains("PinSlot { module: 0, vt_rva: AVT.value as u32, slot: 4, target_rva: 0x2000,"));
    assert_eq!(text.matches("rva: 0x2000, len: 24").count(), 1);
    assert!(!text.contains("rva: 0x2100, len: 24"));
    assert!(text.contains("what: \"AVT slot 2 (identity)\""));
    // the windows vtable note names the RTTI class; the linux one the symbol
    assert!(text.contains("/// alpha.dll .?AVCAlpha@@\n"));
    assert!(text.contains("    /// _ZTV6CAlpha\n    pub const LVT: super::Pin = super::Pin { module: PIN_LIBALPHA, value: 0x100, kind: \"vtable\" };"));
    assert!(text.contains("super::PinSlot { module: 0, vt_rva: LVT.value as u32, slot: 1, target_rva: 0x400, what: \"LVT slot 1 (identity)\" }"));
    // an sdk-only offset is not emitted, a bridge offset is, without a doc comment
    assert!(!text.contains("m_sdk"));
    assert!(text.contains("// class field offsets / sizes; the signatures database\npub const A_OFF: Pin"));
    // a module without an RSDS record pins zeros
    assert!(text.contains("name: \"beta.dll\""));
    assert!(text.contains(&format!("rsds_guid: [{}], rsds_age: 0x0", vec!["0"; 16].join(", "))));
}

/// Runs `edit` on the canned document and expects the render to refuse with `want` in the message.
fn assert_refused(want: &str, edit: impl FnOnce(&mut serde_json::Value)) {
    let mut doc = canned();
    edit(&mut doc);
    let err = render_value(doc).expect_err("the render must refuse").to_string();
    assert!(err.contains(want), "refusal {err:?} does not say {want:?}");
}

fn alpha_entry<'a>(doc: &'a mut serde_json::Value, name: &str) -> &'a mut serde_json::Value {
    doc["windows"][0]["entries"].as_array_mut().unwrap().iter_mut().find(|e| e["name"] == name).unwrap()
}

#[test]
fn refuses_a_bridge_entry_without_a_mapping() {
    assert_refused("entry alpha.dll/CAlpha::m_off (bridge A_OFF) has no pins.rs mapping", |d| {
        alpha_entry(d, "CAlpha::m_off")["kind"] = "string".into();
    });
}

#[test]
fn refuses_a_vslot_on_a_vtable_without_a_bridge_name() {
    assert_refused("bridge vslot AVS_FOO references vtable NOPE, which has no bridge entry", |d| {
        alpha_entry(d, "CAlpha::Foo")["locator"]["vtable"] = "NOPE".into();
    });
}

#[test]
fn refuses_an_identity_slot_on_a_vtable_without_a_bridge_name() {
    assert_refused("identity slot on vtable AVT, which has no bridge entry", |d| {
        alpha_entry(d, "AVT")["bridge"] = serde_json::Value::Null;
        // its vslots would refuse first otherwise
        d["windows"][0]["entries"].as_array_mut().unwrap().retain(|e| e["locator"]["type"] != "vslot");
    });
}

#[test]
fn refuses_an_identity_slot_that_is_not_an_image_pointer() {
    assert_refused("vtable 0x1000 slot 2 is not an image pointer", |d| {
        alpha_entry(d, "AVT")["ident_target"] = serde_json::Value::Null;
    });
}

#[test]
fn refuses_a_direct_function_window_over_a_relocation() {
    assert_refused("pinned window alpha.dll 0x3000 overlaps relocations [12292]", |d| {
        alpha_entry(d, "CAlpha::Do")["window"]["relocs"] = serde_json::json!([0x3004]);
    });
}

#[test]
fn refuses_a_global_outside_every_section() {
    assert_refused("beta.dll: rva 0x6000 not in any section", |d| {
        d["windows"][1]["entries"][1]["window"]["bytes"] = serde_json::Value::Null;
    });
}

#[test]
fn refuses_another_module_list() {
    assert_refused("not [\"alpha.dll\", \"beta.dll\"]", |d| {
        d["windows"].as_array_mut().unwrap().reverse();
    });
}

/// The committed pins.rs builds on its own (no game needed: it is a checked-in file).
#[test]
fn committed_pins_rs_compiles() {
    assert_compiles(&checkout().join("crates/cs2-bridge/src/generated/pins.rs"), "committed");
}

// ------------------------------------------------------------------ the generator (game binaries)

/// The game tests share sigs/fingerprints (the ident fixture writes into it), so they run one
/// at a time.
static GAME: Mutex<()> = Mutex::new(());

fn current_build() -> String {
    std::fs::read_to_string(checkout().join("sigs/CURRENT_BUILD")).unwrap().trim().to_string()
}

/// Refuses to run without the linux tree: a gate that cannot run must fail loudly.
fn require_trees() {
    let linux = checkout().join(format!("target/game-bins-linux-{}", current_build()));
    assert!(linux.is_dir(), "{} is not staged (sigs/README.md \"Binaries\")", linux.display());
}

/// An owned command-line argument.
fn os(s: impl AsRef<OsStr>) -> OsString {
    s.as_ref().to_owned()
}

/// `cargo bridge pins <args>`.
fn pins(args: &[OsString]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cs2-bridge-tools")).arg("pins").args(args).output().expect("running cs2-bridge-tools pins")
}

fn text(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// `cargo bridge pins --sigdir <sigdir> --out <out>`.
fn pins_to(sigdir: Option<&Path>, out: &Path) -> Output {
    let mut args = vec![os("--out"), os(out)];
    if let Some(dir) = sigdir {
        args.extend([os("--sigdir"), os(dir)]);
    }
    pins(&args)
}

/// A scratch copy of sigs/signatures to point `--sigdir` at.
fn scratch_sigdir(name: &str) -> PathBuf {
    let dir = scratch(name).join("signatures");
    std::fs::create_dir_all(&dir).unwrap();
    for f in std::fs::read_dir(checkout().join("sigs/signatures")).unwrap() {
        let f = f.unwrap().path();
        if f.extension().is_some_and(|e| e == "json") {
            std::fs::copy(&f, dir.join(f.file_name().unwrap())).unwrap();
        }
    }
    dir
}

/// Runs a Python snippet with `sigs/scripts` importable (sigcheck's helpers derive the fixture
/// numbers, and json.dumps keeps a database file's entry order) and returns its stdout.
fn python(code: &str, args: &[OsString]) -> String {
    let scripts = checkout().join("sigs/scripts");
    let out = Command::new("python3")
        .arg("-c")
        .arg(format!("import sys; sys.path.insert(0, {:?})\n{code}", scripts.to_str().unwrap()))
        .args(args)
        .output()
        .expect("running python3");
    assert!(out.status.success(), "python3 failed:\n{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// Sets `entries[<entry>][<key>] = <json value>` for each edit of a database file, keeping its
/// order.
fn edit_db(file: &Path, edits: &[(&str, &str, &str)]) {
    let mut args = vec![os(file)];
    for (entry, key, value) in edits {
        args.extend([os(entry), os(key), os(value)]);
    }
    python(
        "import json\n\
         p, a = sys.argv[1], sys.argv[2:]\n\
         d = json.load(open(p))\n\
         for i in range(0, len(a), 3):\n    d['entries'][a[i]][a[i + 1]] = json.loads(a[i + 2])\n\
         open(p, 'w').write(json.dumps(d, indent=1))",
        &args,
    );
}

/// `pub mod linux` of a rendered file.
fn linux_section(text: &str) -> &str {
    &text[text.find("pub mod linux").expect("pub mod linux")..]
}

/// The Windows half of a rendered file (a linux entry must not change it).
fn windows_half(text: &str) -> &str {
    &text[..text.find("pub mod linux").expect("pub mod linux")]
}

/// regen: the generator re-writes the committed pins.rs byte-identical. Needs the game binaries:
/// `cargo test -p cs2-bridge-tools --test pins -- --ignored`.
#[test]
#[ignore = "needs the game binaries ($CS2_BINS and the linux tree)"]
fn regen_matches_the_committed_file() {
    let _game = GAME.lock().unwrap_or_else(|e| e.into_inner());
    require_trees();
    let out = scratch("regen").join("pins.rs");
    let res = pins_to(None, &out);
    assert!(res.status.success(), "pins failed:\n{}", text(&res));
    let committed = checkout().join("crates/cs2-bridge/src/generated/pins.rs");
    assert!(
        std::fs::read_to_string(&out).unwrap() == std::fs::read_to_string(&committed).unwrap(),
        "the generated pins.rs differs from the committed {}",
        committed.display()
    );
    let res = pins(&[os("--check")]);
    assert!(res.status.success(), "pins --check failed:\n{}", text(&res));
}

/// linux fixture: one libclient.json entry gains a `bridge` name and appears in `pub mod linux`
/// with its expect[] value; the Windows half is unchanged. Needs the game binaries:
/// `cargo test -p cs2-bridge-tools --test pins -- --ignored`.
#[test]
#[ignore = "needs the game binaries ($CS2_BINS and the linux tree)"]
fn linux_fixture_emits_a_linux_pin() {
    let _game = GAME.lock().unwrap_or_else(|e| e.into_inner());
    require_trees();
    let sigdir = scratch_sigdir("fixture");
    let db = sigdir.join("libclient.json");
    edit_db(&db, &[("CCONST_IDENTITY_CHUNK_BITS", "bridge", "\"TEST_LINUX_PIN\"")]);
    let want: u64 = python(
        "import json; d = json.load(open(sys.argv[1])); \
         print(d['entries']['CCONST_IDENTITY_CHUNK_BITS']['expect'][sys.argv[2]])",
        &[os(&db), os(current_build())],
    )
    .trim()
    .parse()
    .unwrap();
    let out = sigdir.parent().unwrap().join("pins.rs");
    let res = pins_to(Some(&sigdir), &out);
    assert!(res.status.success(), "pins --sigdir fixture failed:\n{}", text(&res));
    let rendered = std::fs::read_to_string(&out).unwrap();
    let linux = linux_section(&rendered);
    let line = format!("pub const TEST_LINUX_PIN: super::Pin = super::Pin {{ module: PIN_LIBCLIENT, value: {want}, kind: \"const\" }};");
    assert!(linux.contains(&line), "pub mod linux has no {line:?}");
    assert!(linux.contains("pub const PIN_GAME_BUILD"), "pub mod linux carries no PIN_GAME_BUILD");
    let committed = std::fs::read_to_string(checkout().join("crates/cs2-bridge/src/generated/pins.rs")).unwrap();
    assert_eq!(windows_half(&rendered), windows_half(&committed), "a linux bridge entry changed the Windows half");
    assert_compiles(&out, "linux-fixture");
}

/// Restores (or removes) a fingerprint file the ident fixture wrote into sigs/fingerprints.
struct RestoreFile {
    path: PathBuf,
    before: Option<Vec<u8>>,
}

impl Drop for RestoreFile {
    fn drop(&mut self) {
        match &self.before {
            Some(bytes) => std::fs::write(&self.path, bytes).unwrap(),
            None => {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

/// linux ident: the libclient.json CCSGOInput vtable gains a `bridge` name and `ident_slot` 3;
/// its identity row carries the target sigcheck.slot_target reads through .rela, and a wrong
/// `ident_slot` (300, beyond the table) is refused with nothing written. The required
/// fingerprint ("CCSGOInput#3", fp_keys' rule) is taken from the binary itself and written into
/// sigs/fingerprints/libclient-fp.json for the test only. Needs the game binaries:
/// `cargo test -p cs2-bridge-tools --test pins -- --ignored`.
#[test]
#[ignore = "needs the game binaries ($CS2_BINS and the linux tree)"]
fn linux_ident_slot_through_rela() {
    const SLOT: u64 = 3; // any slot of the table; 3 is a real virtual target
    let _game = GAME.lock().unwrap_or_else(|e| e.into_inner());
    require_trees();
    let sigdir = scratch_sigdir("fixture-ident");
    let db = sigdir.join("libclient.json");
    let linux_root = checkout().join(format!("target/game-bins-linux-{}", current_build()));
    // vt, target and the 24 concrete bytes at the target, by sigcheck's own helpers
    let probe = python(
        "import json, sigcheck\n\
         d = json.load(open(sys.argv[1])); vt = d['entries']['CCSGOInput']['expect'][sigcheck.CURRENT_BUILD]\n\
         data = sigcheck.module_path('libclient.so', root=sys.argv[2], platform='linux').read_bytes()\n\
         img = sigcheck.load_image(data); t = sigcheck.slot_target(data, img, vt, int(sys.argv[3]))\n\
         off = sigcheck.rva_to_off(img, t) if t is not None else 0\n\
         print(json.dumps({'vt': vt, 'target': t, 'pattern': ' '.join('%02X' % b for b in data[off:off + 24])}))",
        &[os(&db), os(&linux_root), os(SLOT.to_string())],
    );
    let probe: serde_json::Value = serde_json::from_str(&probe).unwrap();
    let (vt, target) = (probe["vt"].as_u64().unwrap(), probe["target"].as_u64());
    let target = target.expect("sigcheck.slot_target (ELF path) reads no image pointer in libclient.so CCSGOInput slot 3");

    let fp_path = checkout().join("sigs/fingerprints/libclient-fp.json");
    let restore = RestoreFile { before: std::fs::read(&fp_path).ok(), path: fp_path.clone() };
    let mut fps: serde_json::Map<String, serde_json::Value> = match &restore.before {
        Some(bytes) => serde_json::from_slice(bytes).unwrap(),
        None => serde_json::Map::new(),
    };
    fps.insert(format!("CCSGOInput#{SLOT}"), probe["pattern"].clone());
    std::fs::write(&fp_path, serde_json::to_string_pretty(&fps).unwrap()).unwrap();

    edit_db(&db, &[("CCSGOInput", "bridge", "\"TEST_LINUX_VT\""), ("CCSGOInput", "ident_slot", &SLOT.to_string())]);
    let out = sigdir.parent().unwrap().join("pins.rs");
    let res = pins_to(Some(&sigdir), &out);
    assert!(res.status.success(), "pins --sigdir ident fixture failed:\n{}", text(&res));
    let rendered = std::fs::read_to_string(&out).unwrap();
    let linux = linux_section(&rendered);
    let vt_line = format!("pub const TEST_LINUX_VT: super::Pin = super::Pin {{ module: PIN_LIBCLIENT, value: 0x{vt:x}, kind: \"vtable\" }};");
    assert!(linux.contains(&vt_line), "pub mod linux has no {vt_line:?}");
    let slot_row = format!(
        "super::PinSlot {{ module: 0, vt_rva: TEST_LINUX_VT.value as u32, slot: {SLOT}, target_rva: 0x{target:x}, what: \"TEST_LINUX_VT slot {SLOT} (identity)\" }}"
    );
    assert!(linux.contains(&slot_row), "pub mod linux has no identity row {slot_row:?}");
    let committed = std::fs::read_to_string(checkout().join("crates/cs2-bridge/src/generated/pins.rs")).unwrap();
    assert_eq!(windows_half(&rendered), windows_half(&committed), "linux-only entries changed the Windows half");
    assert_compiles(&out, "linux-ident-fixture");

    // a deliberately wrong ident_slot: beyond the table, not an address point any more. Its
    // fingerprint key exists (the slot-3 pattern), so the refusal is the slot check itself, not
    // a missing key
    fps.insert("CCSGOInput#300".to_string(), probe["pattern"].clone());
    std::fs::write(&fp_path, serde_json::to_string_pretty(&fps).unwrap()).unwrap();
    edit_db(&db, &[("CCSGOInput", "ident_slot", "300")]);
    let wrong = sigdir.parent().unwrap().join("wrong-pins.rs");
    let res = pins_to(Some(&sigdir), &wrong);
    assert!(!res.status.success(), "a wrong ident_slot (300) was not refused:\n{}", text(&res));
    assert!(
        text(&res).contains("fingerprint 'CCSGOInput#300'"),
        "the wrong ident_slot was refused for another reason:\n{}",
        text(&res)
    );
    assert!(!wrong.exists(), "a refused run wrote {}", wrong.display());
    let before = restore.before.clone();
    drop(restore);
    assert_eq!(std::fs::read(&fp_path).ok(), before, "sigs/fingerprints/libclient-fp.json was not left as found");
}

/// negative: a module whose builds[CURRENT_BUILD] record is gone is refused with the builds[]
/// message, and nothing is written. Needs the game binaries:
/// `cargo test -p cs2-bridge-tools --test pins -- --ignored`.
#[test]
#[ignore = "needs the game binaries ($CS2_BINS and the linux tree)"]
fn refuses_a_module_without_its_build_record() {
    let _game = GAME.lock().unwrap_or_else(|e| e.into_inner());
    require_trees();
    let sigdir = scratch_sigdir("negative");
    let db = sigdir.join("tier0.json");
    python(
        "import json\n\
         d = json.load(open(sys.argv[1])); del d['builds'][sys.argv[2]]\n\
         open(sys.argv[1], 'w').write(json.dumps(d, indent=1))",
        &[os(&db), os(current_build())],
    );
    let out = sigdir.parent().unwrap().join("pins.rs");
    let res = pins_to(Some(&sigdir), &out);
    let all = text(&res);
    assert!(!res.status.success(), "a module without builds[] was not refused:\n{all}");
    assert!(all.contains(&format!("is not builds[{}]", current_build())), "no builds[] message in:\n{all}");
    assert!(!out.exists(), "a refused run wrote {}", out.display());
}
