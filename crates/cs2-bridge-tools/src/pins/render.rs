//! From the export to pins.rs: map each entry to its section, order the pins and the four
//! tables' rows, and render the file. Nothing here reads a binary; every byte and value comes
//! from the export, which `sigs/scripts/pinsdata.py` printed only after its checks passed.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use anyhow::{anyhow, bail, Context, Result};

use super::export::{Entry, Export, Module};

/// The prologue window the bridge compares (`PinBytes::len`).
const WINDOW: usize = 24;

/// One Windows module the bridge gates: its file name and the PIN_* const naming its index.
pub struct WinModule {
    pub file: &'static str,
    pub pin: &'static str,
}

/// The rendered file and the one-line summary of what went into it.
pub struct Rendered {
    pub text: String,
    pub summary: String,
}

/// The section a pin const belongs to. The sections are emitted in this order, each one module
/// by module and, within a module, in the database's JSON order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Vtable,
    Vslot,
    Const,
    Function,
    Global,
    Offset,
}

impl Kind {
    /// The `kind` string the bridge reads back from a `Pin`.
    fn name(self) -> &'static str {
        match self {
            Kind::Vtable => "vtable",
            Kind::Vslot => "vslot",
            Kind::Const => "const",
            Kind::Function => "function",
            Kind::Global => "global",
            Kind::Offset => "offset",
        }
    }

    /// The `// ...` header opening the section; consts have none and so continue the vslot
    /// section (both are interface-layout numbers).
    fn header(self) -> Option<&'static str> {
        match self {
            Kind::Vtable => Some("vtables (RVA), derived from MSVC RTTI class names"),
            Kind::Vslot => Some(
                "virtual slot numbers (interface layout; targets read from the vtable and pinned below)",
            ),
            Kind::Const => None,
            Kind::Function => Some(
                "direct functions (RVA): non-virtual, non-exported internals - no interface exists; the signatures database",
            ),
            Kind::Global => Some("module-internal globals (RVA); the signatures database"),
            Kind::Offset => Some("class field offsets / sizes; the signatures database"),
        }
    }
}

/// One `pub const NAME: Pin`.
struct PinConst<'a> {
    sym: &'a str,
    module: &'a str,
    value: u64,
    kind: Kind,
    note: Option<String>,
}

/// One platform's pins and table rows.
struct Side<'a> {
    pins: Vec<PinConst<'a>>,
    /// (module index, rva, the 24 bytes, what)
    bytes: Vec<(usize, u64, Vec<u8>, &'a str)>,
    /// (module index, vtable const, slot, target rva, what)
    slots: Vec<(usize, &'a str, u64, u64, String)>,
    /// (module index, pin const naming the rva, what)
    idents: Vec<(usize, &'a str, &'a str)>,
    summary: String,
}

/// A vslot bridge entry: its pin name, the vtable pin it reads, the slot.
struct VSlot<'a> {
    sym: &'a str,
    vtable: &'a str,
    slot: u64,
    note: Option<&'a str>,
    sdk: bool,
}

/// `notes` as a doc comment: none when absent or empty.
fn note(e: &Entry) -> Option<&str> {
    e.notes.as_deref().filter(|n| !n.is_empty())
}

/// The resolved value of a bridge entry (pinsdata.py resolves every one, or refuses).
fn value(m: &Module, e: &Entry) -> Result<u64> {
    e.value.ok_or_else(|| anyhow!("{}: bridge entry {} has no resolved value in the export", m.module, e.name))
}

/// Escapes a string for a Rust string literal (sigs notes may contain double quotes).
fn lit(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `0x..` comma list of bytes.
fn hex_list(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("0x{b:02x}")).collect::<Vec<_>>().join(", ")
}

fn unhex(m: &Module, what: &str, text: &str) -> Result<Vec<u8>> {
    let bad = || anyhow!("{}: {what}: {text:?} is not hex in the export", m.module);
    if !text.len().is_multiple_of(2) {
        return Err(bad());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2).ok_or_else(bad)?, 16).map_err(|_| bad()))
        .collect()
}

/// The 24 bytes at a bridge entry's RVA. `relocs_ok` is false for a directly called function:
/// a window the loader rewrites cannot be compared, so it is refused; a vslot target's
/// overlapping window returns `None` instead (the slot pin covers it).
fn window_bytes(m: &Module, e: &Entry, rva: u64, relocs_ok: bool) -> Result<Option<Vec<u8>>> {
    let w = e
        .window
        .as_ref()
        .ok_or_else(|| anyhow!("{}: bridge entry {} at {rva:#x} has no byte window in the export", m.module, e.name))?;
    if !w.relocs.is_empty() {
        if relocs_ok {
            return Ok(None);
        }
        bail!("pinned window {} {rva:#x} overlaps relocations {:?}", m.module, w.relocs);
    }
    let text = w.bytes.as_deref().ok_or_else(|| anyhow!("{}: rva {rva:#x} not in any section", m.module))?;
    let bytes = unhex(m, &e.name, text)?;
    if bytes.len() != WINDOW {
        bail!("{}: the window at {rva:#x} holds {} bytes, not {WINDOW}", m.module, bytes.len());
    }
    Ok(Some(bytes))
}

/// The pins and rows of one platform's modules (`pins` names each module's PIN_* const).
fn derive<'a>(modules: &'a [Module], pin_names: &'a [String], linux: bool) -> Result<Side<'a>> {
    let n = modules.len();
    let mut vtables: Vec<(&str, usize, String)> = Vec::new();
    let mut vslots: Vec<VSlot> = Vec::new();
    let mut idents: Vec<(&str, usize, u64, Option<u64>, bool)> = Vec::new();
    let mut consts: Vec<(usize, &str, u64, Option<&str>)> = Vec::new();
    let mut fns: Vec<Vec<(&str, &Entry)>> = vec![Vec::new(); n];
    let mut globs: Vec<Vec<(&str, &Entry)>> = vec![Vec::new(); n];
    let mut offsets: Vec<(usize, &str, u64)> = Vec::new();
    let mut n_sdk = 0;
    // the entries keyed by bridge name: an entry's *key* need not be its bridge name
    // (toolframework2.json "CToolFramework2::LoadToolSystem" -> TFVS_LOAD_TOOL_SYSTEM), and the
    // cross-entry lookups (a vtable by its pin, a vslot's target) are always by bridge name
    let mut by_bridge: Vec<HashMap<&str, &Entry>> = vec![HashMap::new(); n];

    for (idx, m) in modules.iter().enumerate() {
        for e in &m.entries {
            let b = e.bridge.as_deref();
            if let (Some(b), Some(_)) = (b, e.value) {
                by_bridge[idx].insert(b, e);
            }
            let loc = e.locator.as_ref();
            let loc_type = loc.and_then(|l| l.kind.as_deref());
            if e.kind == "vtable" {
                if let Some(slot) = e.ident_slot {
                    idents.push((b.unwrap_or(&e.name), idx, slot, e.ident_target, b.is_some()));
                }
                if let Some(b) = b {
                    // a Windows vtable is located by its MSVC RTTI class name (the locator's
                    // "class"); a linux vtable has none (sig/itanium_vtable/base locators), so
                    // the note text is the entry's `notes` there, else the locator's symbol
                    let cls = if linux {
                        note(e)
                            .or_else(|| loc.and_then(|l| l.symbol.as_deref()))
                            .unwrap_or(&e.name)
                            .to_string()
                    } else {
                        loc.and_then(|l| l.class.clone()).ok_or_else(|| {
                            anyhow!("{}: vtable entry {} (bridge {b}) has no RTTI class", m.module, e.name)
                        })?
                    };
                    vtables.push((b, idx, cls));
                }
            } else if let (Some(b), Some("vslot")) = (b, loc_type) {
                let l = loc.expect("a vslot locator");
                let (Some(vtable), Some(slot)) = (l.vtable.as_deref(), l.slot) else {
                    bail!("{}: vslot entry {} (bridge {b}) has no vtable/slot", m.module, e.name);
                };
                vslots.push(VSlot { sym: b, vtable, slot, note: note(e), sdk: e.sdk });
            } else if let (Some(b), "function") = (b, e.kind.as_str()) {
                fns[idx].push((b, e));
            } else if let (Some(b), "global") = (b, e.kind.as_str()) {
                globs[idx].push((b, e));
            } else if e.sdk {
                n_sdk += 1; // owned by the generated SDK header: measured by pinsdata.py, not emitted
            } else if let (Some(b), "offset") = (b, e.kind.as_str()) {
                offsets.push((idx, b, value(m, e)?));
            } else if let (Some(b), "const") = (b, e.kind.as_str()) {
                consts.push((idx, b, value(m, e)?, note(e)));
            } else if let Some(b) = b {
                bail!("entry {}/{} (bridge {b}) has no pins.rs mapping", m.module, e.name);
            }
        }
    }

    let vt_mod: HashMap<&str, usize> = vtables.iter().map(|(sym, idx, _)| (*sym, *idx)).collect();
    let mut derived: HashMap<&str, u64> = HashMap::new();
    for (sym, idx, _) in &vtables {
        derived.insert(sym, value(&modules[*idx], by_bridge[*idx][sym])?);
    }
    for v in &vslots {
        if !vt_mod.contains_key(v.vtable) {
            bail!("bridge vslot {} references vtable {}, which has no bridge entry in signatures/", v.sym, v.vtable);
        }
    }
    for (vsym, _, _, _, has_bridge) in &idents {
        if !has_bridge || !vt_mod.contains_key(vsym) {
            bail!("identity slot on vtable {vsym}, which has no bridge entry in signatures/");
        }
    }
    // a vslot's target is the vslot entry's own value, in the module of the vtable it reads
    let mut slot_targets: HashMap<&str, (u64, &Entry)> = HashMap::new();
    for v in &vslots {
        let idx = vt_mod[v.vtable];
        let e = by_bridge[idx].get(v.sym).ok_or_else(|| {
            anyhow!("bridge vslot {} is not in {}, the module of its vtable {}", v.sym, modules[idx].module, v.vtable)
        })?;
        slot_targets.insert(v.sym, (value(&modules[idx], e)?, e));
    }
    let mut ident_targets: Vec<(&str, u64, u64)> = Vec::new();
    for (vsym, _, slot, target, _) in &idents {
        let Some(target) = target else {
            bail!(
                "{}vtable {:#x} slot {slot} is not an image pointer",
                if linux { "linux " } else { "" },
                derived[vsym]
            );
        };
        ident_targets.push((vsym, *slot, *target));
    }

    // the pin consts: vtables, vslots (an `sdk`-owned vslot is not emitted; the tables below
    // still carry its runtime rows), consts, functions, globals, offsets
    let mut pins = Vec::new();
    for (sym, idx, cls) in &vtables {
        let note = if linux { cls.clone() } else { format!("{} .?AV{cls}@@", modules[*idx].module) };
        pins.push(PinConst { sym, module: &pin_names[*idx], value: derived[sym], kind: Kind::Vtable, note: Some(note) });
    }
    for v in vslots.iter().filter(|v| !v.sdk) {
        pins.push(PinConst {
            sym: v.sym,
            module: &pin_names[vt_mod[v.vtable]],
            value: v.slot,
            kind: Kind::Vslot,
            note: v.note.map(str::to_string),
        });
    }
    for (idx, sym, value, note) in &consts {
        pins.push(PinConst { sym, module: &pin_names[*idx], value: *value, kind: Kind::Const, note: note.map(str::to_string) });
    }
    for (kind, list) in [(Kind::Function, &fns), (Kind::Global, &globs)] {
        for (idx, entries) in list.iter().enumerate() {
            for (sym, e) in entries {
                pins.push(PinConst {
                    sym,
                    module: &pin_names[idx],
                    value: value(&modules[idx], e)?,
                    kind,
                    note: note(e).map(str::to_string),
                });
            }
        }
    }
    for (idx, sym, value) in &offsets {
        pins.push(PinConst { sym, module: &pin_names[*idx], value: *value, kind: Kind::Offset, note: None });
    }

    // the four tables' rows
    let mut bytes = Vec::new();
    let mut slots = Vec::new();
    let mut ident_rows = Vec::new();
    for (idx, m) in modules.iter().enumerate() {
        for (sym, e) in &fns[idx] {
            let rva = value(m, e)?;
            let raw = window_bytes(m, e, rva, false)?.expect("a function window with relocations is refused");
            bytes.push((idx, rva, raw, *sym));
        }
        // prologues of called virtual targets (redundant with the slot pins; cheap); one row per
        // target, and none for a target whose window the loader rewrites
        let mut seen = HashSet::new();
        for v in &vslots {
            let (rva, e) = slot_targets[v.sym];
            if vt_mod[v.vtable] != idx || seen.contains(&rva) {
                continue;
            }
            let Some(raw) = window_bytes(m, e, rva, true)? else { continue };
            seen.insert(rva);
            bytes.push((idx, rva, raw, v.sym));
        }
    }
    for v in &vslots {
        slots.push((vt_mod[v.vtable], v.vtable, v.slot, slot_targets[v.sym].0, v.note.unwrap_or("").to_string()));
    }
    for (vsym, slot, target) in &ident_targets {
        slots.push((vt_mod[vsym], *vsym, *slot, *target, format!("{vsym} slot {slot} (identity)")));
    }
    for (idx, m) in modules.iter().enumerate() {
        for (sym, e) in &globs[idx] {
            let in_section = e.window.as_ref().is_some_and(|w| w.bytes.is_some());
            if !in_section {
                bail!("{}: rva {:#x} not in any section", m.module, value(m, e)?);
            }
            ident_rows.push((idx, *sym, *sym));
        }
        for (sym, vidx, _) in &vtables {
            if *vidx == idx {
                ident_rows.push((idx, *sym, *sym));
            }
        }
    }

    let summary = format!(
        "derived {} vtables, {} slot targets ({} identity), {} direct fns, {} globals, {} offsets, \
         {} bridge consts; {} slots + {} offsets/consts owned by \
         crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h (sdk, not emitted)",
        derived.len(),
        slot_targets.len(),
        ident_targets.len(),
        fns.iter().map(Vec::len).sum::<usize>(),
        globs.iter().map(Vec::len).sum::<usize>(),
        offsets.len(),
        consts.len(),
        vslots.iter().filter(|v| v.sdk).count(),
        n_sdk,
    );
    Ok(Side { pins, bytes, slots, idents: ident_rows, summary })
}

/// The `pub const NAME: Pin` lines of one side. `qual` is the path of the Pin struct from the
/// pins being emitted: only the top-level module holds the struct definitions, so
/// `pub mod linux` names them `super::Pin` (a bare `Pin` there fails rustc with E0425).
fn emit_pins(out: &mut String, pins: &[PinConst], indent: &str, qual: &str) {
    let mut prev = None;
    for p in pins {
        if prev != Some(p.kind) {
            prev = Some(p.kind);
            if let Some(header) = p.kind.header() {
                out.push('\n');
                let _ = writeln!(out, "{indent}// {header}");
            }
        }
        if let Some(note) = &p.note {
            let _ = writeln!(out, "{indent}/// {note}");
        }
        let value = if matches!(p.kind, Kind::Vslot | Kind::Const) {
            p.value.to_string()
        } else {
            format!("0x{:x}", p.value)
        };
        let _ = writeln!(
            out,
            "{indent}pub const {}: {qual}Pin = {qual}Pin {{ module: {}, value: {value}, kind: \"{}\" }};",
            p.sym,
            p.module,
            p.kind.name()
        );
    }
}

/// The table rows of one side, `indent`ed and with the struct paths `qual`ified.
fn emit_rows(out: &mut String, side: &Side, indent: &str, qual: &str) {
    for (idx, rva, raw, what) in &side.bytes {
        let _ = writeln!(
            out,
            "{indent}{qual}PinBytes {{ module: {idx}, rva: 0x{rva:x}, len: 24, bytes: [{}], what: \"{}\" }},",
            hex_list(raw),
            lit(what)
        );
    }
}

fn emit_slots(out: &mut String, side: &Side, indent: &str, qual: &str) {
    for (idx, vsym, slot, target, what) in &side.slots {
        let _ = writeln!(
            out,
            "{indent}{qual}PinSlot {{ module: {idx}, vt_rva: {vsym}.value as u32, slot: {slot}, target_rva: 0x{target:x}, what: \"{}\" }},",
            lit(what)
        );
    }
}

fn emit_idents(out: &mut String, side: &Side, indent: &str, qual: &str) {
    for (idx, sym, what) in &side.idents {
        let _ = writeln!(out, "{indent}{qual}PinIdent {{ module: {idx}, rva: {sym}.value as u32, what: \"{}\" }},", lit(what));
    }
}

/// The PIN_* const of a linux module: `libclient.so` -> `PIN_LIBCLIENT`.
fn linux_pin(module: &str) -> String {
    let stem = module.rsplit_once('.').map_or(module, |(stem, _)| stem);
    format!("PIN_{}", stem.to_ascii_uppercase())
}

/// The struct definitions every pins.rs carries (the rows' types).
const STRUCTS: &str = r#"/// A pinned value: `module` is the module's index (the PIN_* consts), `value` the number
/// the entry's locator resolved to (an RVA for kind "vtable"/"function"/
/// "global", a slot number for "vslot", an offset or const value otherwise).
pub struct Pin {
    pub module: u8,
    pub value: u64,
    pub kind: &'static str,
}

/// module identity: the whole-file SHA-256 and PE identity the bridge gates before it
/// touches a module (rows: PIN_MODULES; index = the PIN_* consts)
pub struct PinModule {
    pub name: &'static str,
    pub sha256: [u8; 32],
    pub time_stamp: u32,
    pub size_of_image: u32,
    pub check_sum: u32,
    pub rsds_guid: [u8; 16],
    pub rsds_age: u32,
}

/// prologue bytes of a pinned function (rows: PIN_BYTES)
pub struct PinBytes {
    pub module: u8,
    pub rva: u32,
    pub len: u8,
    pub bytes: [u8; 24],
    pub what: &'static str,
}

/// a called virtual slot: vtable RVA, slot, target RVA (rows: PIN_SLOTS)
pub struct PinSlot {
    pub module: u8,
    pub vt_rva: u32,
    pub slot: u16,
    pub target_rva: u32,
    pub what: &'static str,
}

/// a module-internal global the bridge reads directly (rows: PIN_IDENTS)
pub struct PinIdent {
    pub module: u8,
    pub rva: u32,
    pub what: &'static str,
}
"#;

/// Renders pins.rs from an export. `windows` is the Windows module list with its PIN_* names;
/// the export must hold exactly those modules, in that order.
pub fn render(export: &Export, windows: &[WinModule]) -> Result<Rendered> {
    let got: Vec<&str> = export.windows.iter().map(|m| m.module.as_str()).collect();
    let want: Vec<&str> = windows.iter().map(|m| m.file).collect();
    if got != want {
        bail!("the export holds the Windows modules {got:?}, not {want:?}");
    }
    let win_pins: Vec<String> = windows.iter().map(|m| m.pin.to_string()).collect();
    let linux_pins: Vec<String> = export.linux.iter().map(|m| linux_pin(&m.module)).collect();
    let win = derive(&export.windows, &win_pins, false)?;
    let lin = derive(&export.linux, &linux_pins, true)?;
    let build = &export.build;

    let mut r = String::new();
    r.push_str("// Generated by `cargo bridge pins` from sigs/signatures. Do not edit.\n");
    r.push_str("//\n");
    r.push_str("// The Windows half (the DLL modules the Wine bridge gates) comes first, module by module in\n");
    r.push_str("// signatures-database order; the linux platform is in `pub mod linux` below.\n");
    r.push('\n');
    let _ = writeln!(r, "pub const PIN_GAME_BUILD: &str = \"{build}\";");
    let _ = writeln!(r, "pub const PIN_MODULE_COUNT: usize = {};", windows.len());
    r.push('\n');
    for (i, m) in windows.iter().enumerate() {
        let _ = writeln!(r, "/// module index: {}", lit(m.file));
        let _ = writeln!(r, "pub const {}: u8 = {i};", m.pin);
    }
    r.push('\n');
    r.push_str(STRUCTS);
    r.push('\n');
    emit_pins(&mut r, &win.pins, "", "");

    r.push('\n');
    r.push_str("/// module identity rows (PinModule; index = the PIN_* consts)\n");
    r.push_str("pub static PIN_MODULES: &[PinModule] = &[\n");
    for m in &export.windows {
        let sha = unhex(m, "sha256", &m.sha256)?;
        if sha.len() != 32 {
            bail!("{}: sha256 {:?} is not 32 bytes", m.module, m.sha256);
        }
        let guid = match &m.rsds_guid {
            Some(g) => {
                let g = unhex(m, "rsds_guid", g)?;
                if g.len() != 16 {
                    bail!("{}: rsds_guid is not 16 bytes", m.module);
                }
                hex_list(&g)
            }
            None => vec!["0"; 16].join(", "),
        };
        let soi = u32::try_from(m.size_of_image).with_context(|| format!("{}: SizeOfImage", m.module))?;
        let _ = writeln!(
            r,
            "    PinModule {{ name: \"{}\", sha256: [{}], time_stamp: 0x{:08x}, size_of_image: 0x{soi:08x}, \
             check_sum: 0x{:08x}, rsds_guid: [{guid}], rsds_age: 0x{:x} }},",
            lit(&m.module),
            hex_list(&sha),
            m.time_stamp,
            m.check_sum,
            m.rsds_age
        );
    }
    r.push_str("];\n\n");
    r.push_str("/// prologue bytes of pinned functions (PinBytes)\n");
    r.push_str("pub static PIN_BYTES: &[PinBytes] = &[\n");
    emit_rows(&mut r, &win, "    ", "");
    r.push_str("];\n\n");
    r.push_str("/// called virtual slots, the vtable pinned by name (PinSlot)\n");
    r.push_str("pub static PIN_SLOTS: &[PinSlot] = &[\n");
    emit_slots(&mut r, &win, "    ", "");
    r.push_str("];\n\n");
    r.push_str("/// module-internal globals and pinned vtables, by name (PinIdent)\n");
    r.push_str("pub static PIN_IDENTS: &[PinIdent] = &[\n");
    emit_idents(&mut r, &win, "    ", "");
    r.push_str("];\n\n");

    r.push_str("/// Linux pins: sigs/signatures/lib*.json (\"platform\": linux) entries with a `bridge`\n");
    r.push_str("/// name, verified the same way as the Windows modules but against the linux tree\n");
    r.push_str("/// ($CS2_LINUX_BINS, default target/game-bins-linux-<PIN_GAME_BUILD>; sigcheck.py's\n");
    r.push_str("/// linux platform resolution). No linux entry carries a `bridge` name yet, so this\n");
    r.push_str("/// module is empty-but-present (cs2-bridge-tools' `pins` test proves the path with a fixture).\n");
    r.push_str("pub mod linux {\n");
    let _ = writeln!(r, "    pub const PIN_GAME_BUILD: &str = \"{build}\";");
    let _ = writeln!(r, "    pub const PIN_MODULE_COUNT: usize = {};", export.linux.len());
    r.push('\n');
    for (i, (m, pin)) in export.linux.iter().zip(&linux_pins).enumerate() {
        let _ = writeln!(r, "    /// module index: {}", lit(&m.module));
        let _ = writeln!(r, "    pub const {pin}: u8 = {i};");
    }
    emit_pins(&mut r, &lin.pins, "    ", "super::");
    r.push('\n');
    r.push_str("    /// module names, by index (the PIN_* consts in this module)\n");
    let names: Vec<String> = export.linux.iter().map(|m| format!("\"{}\"", lit(&m.module))).collect();
    let _ = writeln!(r, "    pub static MODULES: &[&str] = &[{}];", names.join(", "));
    r.push('\n');
    r.push_str("    /// prologue bytes of pinned functions (empty: no linux entry carries a `bridge` name)\n");
    r.push_str("    pub static PIN_BYTES: &[super::PinBytes] = &[\n");
    emit_rows(&mut r, &lin, "        ", "super::");
    r.push_str("    ];\n\n");
    r.push_str("    /// called virtual slots (empty: no linux entry carries a `bridge` name)\n");
    r.push_str("    pub static PIN_SLOTS: &[super::PinSlot] = &[\n");
    emit_slots(&mut r, &lin, "        ", "super::");
    r.push_str("    ];\n\n");
    r.push_str("    /// module-internal globals and pinned vtables, by name (empty: no linux `bridge` name)\n");
    r.push_str("    pub static PIN_IDENTS: &[super::PinIdent] = &[\n");
    emit_idents(&mut r, &lin, "        ", "super::");
    r.push_str("    ];\n");
    r.push_str("}\n");
    Ok(Rendered { text: r, summary: win.summary })
}
