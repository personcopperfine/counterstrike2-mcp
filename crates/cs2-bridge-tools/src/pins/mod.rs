//! `cargo bridge pins [--check] [--sigdir DIR] [--out PATH]`: generate the bridge pins (module
//! identity, vtables, slots, offsets), `crates/cs2-bridge/src/generated/pins.rs`.
//!
//! The bridge fails closed: every module it touches must match the whole-file SHA-256 and PE
//! identity recorded here, every directly called function's prologue bytes, and every virtual
//! slot it calls. The values come from the DLLs (`$CS2_BINS`, see sigs/README.md) and the
//! signature database (`sigs/signatures/<module>.json`); this command keeps no tables of its own
//! beyond the module list and its PIN_* names.
//!
//! # Two halves, one resolver
//!
//! Everything bound by the bridge is an entry in `sigs/signatures/<module>.json` carrying a
//! `bridge` name. `sigs/scripts/pinsdata.py` resolves those entries with sigcheck.py's
//! Resolver - the same one `sigcheck.py resolve` and `sigupdate.py --bridge-gate` use, so pins.rs
//! and `sigcheck resolve` can never disagree - runs the fail-closed checks (validate, a `stale`
//! bridge entry, builds[CURRENT_BUILD].sha256, expect[CURRENT_BUILD] == resolved, the required
//! fingerprints, every `sdk` expression == its measurement in `s2sdk_gen.h`) and prints one JSON
//! document ([`export::Export`]). When a locator stops matching on a new build, its refusal names
//! the `via` site to re-derive in a disassembler. This side checks the installed Wine copies,
//! maps the entries to sections, orders them and renders the file ([`render::render`]).
//!
//! # Sections
//!
//! Entries with a `bridge` field map to sections by kind / locator type:
//! * kind=vtable -> a vtable RVA (derived from MSVC RTTI, locator `rtti`, on Windows);
//! * kind=function, locator vslot -> a virtual slot number (interface layout) + its target;
//! * kind=function -> a direct (non-virtual, non-exported) function;
//! * kind=global -> a module-internal global;
//! * kind=offset -> a class field offset / size;
//! * kind=const -> an interface-layout constant (emitted, never pinned: e.g. a polymorphic slot
//!   whose value lives in the database as `value`).
//!
//! Anything else with a `bridge` name is refused ("no pins.rs mapping"). Vtable entries may carry
//! `ident_slot` (slot contents checked for identity, not called); the identity rows in PIN_SLOTS
//! come from those entries. The doc comment on each pin is the entry's `notes` (vslots, consts,
//! functions, globals); a vtable's names the module and RTTI class. Sections are in the order
//! above, each in module order (the [`WINDOWS_MODULES`] order) and, within a module, in the
//! entry order of its JSON file.
//!
//! A kind=const/offset entry without `bridge` and without `sdk` is emitted nowhere: it is a
//! measurement only sigcheck/sdkcheck use.
//!
//! An entry with `sdk` names the generated SDK expression that owns its number (a slot, offset,
//! size or const in `crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h`, gen.py's tie syntax): the
//! bridge reads that number from `s2sdk_sys`, so the entry is never emitted as a pin const. It is
//! still resolved, gated and fingerprinted like any bridge entry, and a vslot entry still
//! produces its PIN_SLOTS/PIN_BYTES runtime proof rows (named by its `bridge` name). The export
//! refuses when an `sdk` expression and the binary's measurement differ: the fail-closed
//! replacement for a compile-time tie.
//!
//! # Linux
//!
//! `pub mod linux` holds the linux platform's own module list (`sigs/signatures/lib*.json`,
//! "platform": linux): its entries with a `bridge` name are resolved and gated the same way,
//! against the linux tree (`$CS2_LINUX_BINS`, default `target/game-bins-linux-<CURRENT_BUILD>`).
//! No linux entry carries a `bridge` name today, so the module is empty-but-present;
//! `tests/pins.rs` proves the path with fixtures.
//!
//! # Options
//!
//! `--check` writes nothing: it fails, naming the file, when the file on disk differs from what
//! would be written (the build's up-to-date gate; a missing file differs too). `--sigdir` reads
//! the signatures database from a copy (a test fixture; the checks then run against that copy,
//! unchanged). `--out` writes elsewhere than this checkout's pins.rs.

pub mod export;
pub mod render;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

pub use export::Export;
pub use render::{render, Rendered, WinModule};

use crate::root;

/// The Windows modules the bridge pins, in PIN_* index order (the bridge's module table is
/// indexed by these consts, so a new module goes at the end).
pub const WINDOWS_MODULES: &[WinModule] = &[
    WinModule { file: "hammer.dll", pin: "PIN_HAMMER" },
    WinModule { file: "toolframework2.dll", pin: "PIN_TF2" },
    WinModule { file: "scenesystem.dll", pin: "PIN_SCENESYSTEM" },
    WinModule { file: "engine2.dll", pin: "PIN_ENGINE2" },
    WinModule { file: "rendersystemvulkan.dll", pin: "PIN_RSV" },
    WinModule { file: "client.dll", pin: "PIN_CLIENT" },
    WinModule { file: "server.dll", pin: "PIN_SERVER" },
    WinModule { file: "tier0.dll", pin: "PIN_TIER0" },
    WinModule { file: "schemasystem.dll", pin: "PIN_SCHEMA" },
    WinModule { file: "soundsystem.dll", pin: "PIN_SOUND" },
    WinModule { file: "modeldoc_editor.dll", pin: "PIN_MODELDOC" },
    WinModule { file: "pet.dll", pin: "PIN_PET" },
];

/// Where each module sits in the Wine install (`<project root>/workshop/run/app`), which must
/// hold the same bytes as `$CS2_BINS`: the pins are taken from the latter, the bridge runs
/// against the former.
const INSTALLED: &[(&str, &str)] = &[
    ("hammer.dll", "game/bin/win64/tools/hammer.dll"),
    ("toolframework2.dll", "game/bin/win64/toolframework2.dll"),
    ("scenesystem.dll", "game/bin/win64/scenesystem.dll"),
    ("engine2.dll", "game/bin/win64/engine2.dll"),
    ("rendersystemvulkan.dll", "game/bin/win64/rendersystemvulkan.dll"),
    ("client.dll", "game/csgo/bin/win64/client.dll"),
    ("server.dll", "game/csgo/bin/win64/server.dll"),
    ("tier0.dll", "game/bin/win64/tier0.dll"),
    ("schemasystem.dll", "game/bin/win64/schemasystem.dll"),
    ("soundsystem.dll", "game/bin/win64/soundsystem.dll"),
    ("modeldoc_editor.dll", "game/bin/win64/tools/modeldoc_editor.dll"),
    ("pet.dll", "game/bin/win64/tools/pet.dll"),
];

/// The generated file, relative to the checkout.
pub const PINS_RS: &str = "crates/cs2-bridge/src/generated/pins.rs";

/// The command line of `cargo bridge pins`.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// Write nothing: fail when the file on disk differs from the generated output
    #[arg(long)]
    pub check: bool,
    /// Signatures directory to read (default sigs/signatures; a test fixture)
    #[arg(long, value_name = "DIR")]
    pub sigdir: Option<PathBuf>,
    /// Output file (default crates/cs2-bridge/src/generated/pins.rs in this checkout)
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
}

/// Runs the command; `Ok` is the exit code.
pub fn run(args: Args) -> Result<u8> {
    let checkout = root::checkout();
    let out = args.out.unwrap_or_else(|| checkout.join(PINS_RS));
    let export = run_export(&checkout, args.sigdir.as_deref())?;

    let build_file = checkout.join("sigs/CURRENT_BUILD");
    let build = std::fs::read_to_string(&build_file).with_context(|| format!("reading {}", build_file.display()))?;
    if export.build != build.trim() {
        bail!("pinsdata.py checked build {} but {} says {}", export.build, build_file.display(), build.trim());
    }
    for m in export.windows.iter().chain(&export.linux) {
        println!(
            "{:<22} ts={:#x} soi={:#x} sha256={}",
            m.module,
            m.time_stamp,
            m.size_of_image,
            m.sha256.get(..16).unwrap_or(&m.sha256)
        );
    }
    check_installed(&root::project_root().join("workshop/run/app"), &export)?;
    let rendered = render(&export, WINDOWS_MODULES)?;
    println!("{}", rendered.summary);

    if args.check {
        // the up-to-date gate: write nothing, refuse on any difference (a missing file is a
        // difference too: the build could not read it)
        let on_disk = std::fs::read_to_string(&out).ok();
        if on_disk.as_deref() != Some(rendered.text.as_str()) {
            bail!("--check: {} differs from the generated output (regenerate with `cargo bridge pins`)", out.display());
        }
        println!("--check: {} up to date", out.display());
        return Ok(0);
    }
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&out, &rendered.text).with_context(|| format!("writing {}", out.display()))?;
    println!("wrote {} ({} lines)", out.display(), rendered.text.lines().count());
    Ok(0)
}

/// Runs `sigs/scripts/pinsdata.py` and parses its document. Its refusals go to stderr as they
/// are; a refusal is an error here, before anything is written.
fn run_export(checkout: &Path, sigdir: Option<&Path>) -> Result<Export> {
    let script = checkout.join("sigs/scripts/pinsdata.py");
    // plain python3, not -I: the resolver's numpy lives in the user site-packages
    let mut cmd = Command::new("python3");
    cmd.arg(&script);
    if let Some(dir) = sigdir {
        cmd.arg("--sigdir").arg(dir);
    }
    cmd.args(WINDOWS_MODULES.iter().map(|m| m.file));
    let output = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("running python3 {}", script.display()))?;
    if !output.status.success() {
        bail!(
            "{} refused ({}); nothing was written",
            script.strip_prefix(checkout).unwrap_or(&script).display(),
            output.status
        );
    }
    serde_json::from_slice(&output.stdout).context("parsing the pinsdata.py export")
}

/// Refuses when an installed Wine copy of a module differs from the one pinned (a missing
/// install is fine: the pins are for whatever install gets those bytes).
fn check_installed(app: &Path, export: &Export) -> Result<()> {
    for m in &export.windows {
        let Some((_, rel)) = INSTALLED.iter().find(|(file, _)| *file == m.module) else {
            continue;
        };
        let path = app.join(rel);
        if !path.exists() {
            continue;
        }
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let sha: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        if sha != m.sha256 {
            bail!("installed copy differs from repo copy: {}", path.display());
        }
    }
    Ok(())
}
