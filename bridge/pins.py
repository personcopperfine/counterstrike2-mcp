#!/usr/bin/env python3
"""pins.py - generate the bridge pins (module identity, vtables, slots, offsets) for the Rust
bridge: bridge_pins.h and, with --rust, crates/cs2-bridge/src/generated/pins.rs (the header the
Rust crate embeds is generated/; the C header stays for sdkcheck.py, gen.py's ties and history).

The bridge fails closed: every module it touches must match the whole-file SHA-256 and PE
identity recorded here, every directly called function's prologue bytes, and every virtual
slot it calls. This script reads the DLLs ($CS2_BINS, see sigs/README.md) and the signature
database (sigs/signatures/<module>.json) and emits the pins consumed by crates/cs2-bridge
(plus the generated C header kept for sdkcheck/history).

Everything bound by the bridge is an entry in sigs/signatures/<module>.json carrying a
`bridge` macro name; this script is a *consumer* of that one database (the same one
sigs/scripts/sigcheck.py validates/resolves) and keeps no tables of its own. Entries with a
`bridge` field map to header sections by kind / locator type:
  * kind=vtable                       -> a vtable RVA, derived from MSVC RTTI (locator rtti)
  * kind=function, locator vslot      -> a virtual slot number (interface layout) + target
  * kind=function                     -> a direct (non-virtual, non-exported) function
  * kind=global                       -> a module-internal global
  * kind=offset                       -> a class field offset / size
  * kind=const                        -> an interface-layout constant (emitted, never pinned:
                                         e.g. a polymorphic slot like HVS_NODE_ISA, whose value
                                         lives in the DB as `value`)
Vtable entries may carry `ident_slot` (slot contents checked for identity, not called); the
identity rows in k_pinSlots come from those entries, not from a table here.
The /* comment */ on each macro is the entry's `notes` (vslots/functions/globals); vtable
comments name the module and RTTI class. Section order is the module order below, and the
entry order within sigs/signatures/<module>.json.

A kind=const/offset entry WITHOUT a `bridge` field and without `sdk` is emitted nowhere: it is a
measurement only sigcheck/sdkcheck use (the old generated bridge_structs.h was retired by
de-hardcode WP1b; every number it carried is now a generated macro, a bridge_pins.h macro, or a
bridge-owned literal).

An entry with `sdk` names the generated SDK expression that owns its number (a slot, offset, size
or const in bridge/s2sdk_gen.h, gen.py's tie syntax): the bridge spells that S2_* macro, so the
entry is never emitted as a #define. It is still
resolved, gated and fingerprinted like any bridge entry, and a vslot entry still produces its
k_pinSlots/k_pinBytes runtime proof rows (named by its `bridge` name). Before anything is written,
every `sdk` expression is evaluated against bridge/s2sdk_gen.h and must equal the entry's
measurement (the vslot's slot index, else the resolved value): the fail-closed replacement for the
compile-time tie the macro used to carry.

Values are derived, never hand-entered: the locators in sigs/signatures/ are resolved against the
on-disk DLL by the same resolver sigcheck.py uses, so the header and `sigcheck resolve` can
never disagree. When a locator stops matching on a new build the resolver names the `via`
site to re-derive in a disassembler.

Because this is the step that writes the header, it fails closed on the evidence itself: each
module's database must pass `sigcheck.py validate` (structure, the fingerprints a bridge entry
requires, the status enum), a bridge entry marked `status: stale` is refused, and every bridge
entry that pins code must have `expect[CURRENT_BUILD]` equal to the value its locator resolved and
must match the fingerprints it requires (the same helpers `sigupdate.py --bridge-gate` judges with).
That closes the paths a run of `validate`/`resolve` alone leaves open: a fingerprint key that
exists but does not match the bytes, and an `expect[]` that was dropped or hand-edited.

`--rust <path>` additionally writes the same pins as Rust (crates/cs2-bridge, the Rust
bridge): one `pub const <macro>: Pin` per emitted #define (same names, values, comments and
order), the module identity / fingerprint / slot / ident tables as `pub static` arrays with
the same rows in the same order, and a `pub mod linux` for the linux platform's own module
list (sigs/signatures/lib*.json, "platform": linux) - its entries with a `bridge` name are
resolved and gated the same way, but against the linux tree ($CS2_LINUX_BINS, default
target/game-bins-linux-<CURRENT_BUILD>), not the DLL tree. No linux entry carries a `bridge`
name today, so `pub mod linux` is empty-but-present.

`--check` writes nothing: it exits 1 when the header and/or pins.rs on disk differ from what
would be written (the build's up-to-date gate). `--sigdir` reads the signatures database from
a copy (test fixture; the checks then run against that copy, unchanged).

Usage:
  bridge/pins.py [--out bridge/bridge_pins.h] [--rust crates/cs2-bridge/src/generated/pins.rs]
                 [--check] [--sigdir DIR]
"""
import argparse
import hashlib
import json
import struct
import sys
import os
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent                                   # cs2-mcp
sys.path.insert(0, str(ROOT / "sigs" / "scripts"))
import sigcheck  # noqa: E402  (reuse the resolver: one derivation for header and sigcheck)
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))  # noqa: E402
from cs2mcp_root import PROJECT_ROOT  # noqa: E402

IMAGE_BASE = 0x180000000
SIGDIR = sigcheck.SIGDIR
MODULES = [(n, sigcheck.module_path(n)) for n in [
    "hammer.dll",
    "toolframework2.dll",
    "scenesystem.dll",
    "engine2.dll",
    "rendersystemvulkan.dll",
    "client.dll",
    "server.dll",
    "tier0.dll",
    "schemasystem.dll",
    "soundsystem.dll",
    "modeldoc_editor.dll",
    "pet.dll",
]]
PIN_NAMES = ["PIN_HAMMER", "PIN_TF2", "PIN_SCENESYSTEM", "PIN_ENGINE2", "PIN_RSV", "PIN_CLIENT", "PIN_SERVER", "PIN_TIER0",
             "PIN_SCHEMA", "PIN_SOUND", "PIN_MODELDOC", "PIN_PET"]
# the Wine install the bridge runs against must hold the same bytes as $CS2_BINS
_RT = Path(PROJECT_ROOT) / "workshop/run/app/game/bin/win64"
INSTALLED = {
    "hammer.dll": _RT / "tools/hammer.dll",
    "toolframework2.dll": _RT / "toolframework2.dll",
    "scenesystem.dll": _RT / "scenesystem.dll",
    "engine2.dll": _RT / "engine2.dll",
    "rendersystemvulkan.dll": _RT / "rendersystemvulkan.dll",
    "client.dll": _RT.parents[1] / "csgo/bin/win64/client.dll",
    "server.dll": _RT.parents[1] / "csgo/bin/win64/server.dll",
    "tier0.dll": _RT / "tier0.dll",
    "schemasystem.dll": _RT / "schemasystem.dll",
    "soundsystem.dll": _RT / "soundsystem.dll",
    "modeldoc_editor.dll": _RT / "tools/modeldoc_editor.dll",
    "pet.dll": _RT / "tools/pet.dll",
}

# (no hand-entered tables: SLOT_ONLY and VSLOT_IDENT live in the database now - HVS_NODE_ISA as
# a kind=const entry in hammer.json, the identity slots as `ident_slot` on the vtable entries)


def parse_pe(data: bytes):
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    assert data[pe:pe + 4] == b"PE\0\0", "not a PE"
    nsec = struct.unpack_from("<H", data, pe + 6)[0]
    ts = struct.unpack_from("<I", data, pe + 8)[0]
    opt = pe + 24
    magic = struct.unpack_from("<H", data, opt)[0]
    assert magic == 0x20B, "not PE32+"
    size_of_image = struct.unpack_from("<I", data, opt + 56)[0]
    checksum = struct.unpack_from("<I", data, opt + 64)[0]
    sect = []
    so = opt + struct.unpack_from("<H", data, pe + 20)[0]
    for i in range(nsec):
        b = so + 40 * i
        name = data[b:b + 8].rstrip(b"\0").decode()
        vsize, va, rsize, rptr = struct.unpack_from("<IIII", data, b + 8)
        sect.append((name, va, vsize, rptr, rsize))
    nrv = struct.unpack_from("<I", data, opt + 0x6C)[0]
    dirs = []
    for i in range(nrv):
        rva, size = struct.unpack_from("<II", data, opt + 0x70 + 8 * i)
        dirs.append((rva, size))
    rel = dirs[5] if len(dirs) > 5 else (0, 0)
    debug = dirs[6] if len(dirs) > 6 else (0, 0)
    export = dirs[0] if dirs else (0, 0)
    return {"ts": ts, "size_of_image": size_of_image, "checksum": checksum,
            "sections": sect, "reloc": rel, "debug": debug, "export": export}


def rva_to_off(pe, rva):
    for name, va, vsize, rptr, rsize in pe["sections"]:
        if va <= rva < va + max(vsize, rsize):
            return rptr + (rva - va)
    raise ValueError("rva %#x not in any section" % rva)


def reloc_rvas(data, pe):
    rva, size = pe["reloc"]
    out = set()
    if not rva or not size:
        return out
    off = rva_to_off(pe, rva)
    end = off + size
    while off + 8 <= end:
        page, block = struct.unpack_from("<II", data, off)
        if block < 8:
            break
        n = (block - 8) // 2
        for i in range(n):
            e = struct.unpack_from("<H", data, off + 8 + 2 * i)[0]
            out.add(page + (e & 0xFFF))
        off += block
    return out


def rsds(data, pe):
    rva, size = pe["debug"]
    if not rva or not size:
        return None
    off = rva_to_off(pe, rva)
    for i in range(size // 28):
        b = off + 28 * i
        typ = struct.unpack_from("<I", data, b + 12)[0]
        if typ != 2:
            continue
        raw = struct.unpack_from("<II", data, b + 20)
        doff = rva_to_off(pe, raw[0])
        if data[doff:doff + 4] == b"RSDS":
            guid = data[doff + 4:doff + 20]
            age = struct.unpack_from("<I", data, doff + 20)[0]
            return guid, age
    return None


def slot_ptr(data, pe, vt, slot):
    """target of one virtual slot (RVA); exits if it is not an image pointer"""
    v = struct.unpack_from("<Q", data, rva_to_off(pe, vt + 8 * slot))[0]
    if not IMAGE_BASE <= v < IMAGE_BASE + pe["size_of_image"]:
        sys.exit("vtable %#x slot %d is not an image pointer" % (vt, slot))
    return v - IMAGE_BASE


def cbytes(b, n=24):
    b = b[:n]
    return ", ".join("0x%02x" % x for x in b)


def cstr(s):
    """escape a Python string for a C string literal (sigs notes may contain double quotes)"""
    return s.replace("\\", "\\\\").replace('"', '\\"')


def pin_checks(mod, d, db_path, data, img, sha, sdk_defs):
    """the fail-closed checks every pinned module's database is written behind, Windows and
    linux alike: the database passes `sigcheck.py validate` (structure, the fingerprints a
    bridge entry requires, the status enum), the binary is builds[CURRENT_BUILD] (the game
    build the bridge reports), every bridge entry's expect[CURRENT_BUILD] equals what its
    locator resolved and its fingerprints match the bytes there (the same helpers
    `sigupdate.py --bridge-gate` judges with), and every `sdk` expression equals the generated
    header's number. Returns the resolved values (entry name -> RVA or integer)."""
    # pins.py is the step that writes the pins, so it checks rather than assuming the caller
    # ran `validate`
    errs = sigcheck.validate_file(db_path, d)
    if errs:
        sys.exit("%s: %d validation error(s), refusing to pin:\n  %s"
                 % (mod, len(errs), "\n  ".join(errs)))
    rec = d.get("builds", {}).get(sigcheck.CURRENT_BUILD, {})
    if rec.get("sha256") != sha[:16]:
        sys.exit("%s: sha256 %s is not builds[%s] (%s) in signatures/; record the build first "
                 "with `python3 sigs/scripts/sigupdate.py record --tree <bins> --build %s` (it "
                 "refuses unless every bridge entry is SAME/OK), or bump sigs/CURRENT_BUILD"
                 % (mod, sha[:16], sigcheck.CURRENT_BUILD, rec.get("sha256"), sigcheck.CURRENT_BUILD))
    r = sigcheck.Resolver(data, img)
    v = {}
    for ename, ent in d["entries"].items():
        has_bridge = bool(ent.get("bridge"))
        if has_bridge and ent.get("status") == "stale":
            # a bridge entry does not go stale through sigupdate.py record (that refuses to
            # write while any bridge entry is not SAME/OK): it is a hand edit, an entry that
            # gained a bridge macro after it was judged, or an entry the judge left stale and
            # printed as PASS. Either way the locator is not known to land on the right code.
            sys.exit("%s: bridge entry %s (macro %s) is marked status 'stale': it was not SAME/OK "
                     "the last time a person judged it, so its locator is not known to land on the "
                     "right code; re-check the `via` site and clear status before pinning"
                     % (mod, ename, ent["bridge"]))
        if not (has_bridge or ent.get("sdk")):
            continue          # only these are emitted/pinned (bridge) or measured for the
                              # generated header (sdk)
        try:
            v[ename] = r.resolve(ename, ent, d["entries"])
        except Exception as ex:  # noqa: BLE001 - report the via so it can be re-derived
            sys.exit("resolve %s / %s failed: %s (via: %s)" % (mod, ename, ex, ent.get("via", "?")))
    # fail closed on the pin evidence, with the same helpers the gate uses. Every bridge entry
    # must carry expect[CURRENT_BUILD] equal to what its locator resolved (the gate's SAME test,
    # and the only thing that says a sig/export/base-located bridge value did not move), and
    # every bridge entry that pins code must have the fingerprints it requires match the bytes
    # there. `sigcheck.py resolve --strict` compares an expect[] only when one is present and
    # `validate` has no binaries, so this is the check that stops a hand-edited locator, a
    # wrong-pattern fingerprint or a dropped expect[] from being pinned.
    fps = sigcheck.load_fps(mod)
    for ename, ent in d["entries"].items():
        if not ent.get("bridge"):
            continue
        value = v.get(ename)
        want = ent.get("expect", {}).get(sigcheck.CURRENT_BUILD)
        if want != value:
            sys.exit("%s: bridge entry %s (macro %s): expect[%s] is %s but the locator resolves "
                     "%#x - the value is not recorded for this build, or was edited; record the "
                     "build with sigupdate.py record"
                     % (mod, ename, ent["bridge"], sigcheck.CURRENT_BUILD,
                        "missing" if want is None else "%#x" % want,
                        value if value is not None else 0))
        for key, rva, present in sigcheck.fp_checks(ename, ent, value, data, img, fps):
            where = "the slot is not an image pointer" if rva is None else \
                    "does not match the bytes at %#x" % (img.get("base", sigcheck.IMAGE_BASE) + rva)
            if not present:
                sys.exit("%s: bridge entry %s: no fingerprint %r in sigs/fingerprints/%s"
                         % (mod, ename, key, sigcheck.fp_file(mod).name))
            if rva is None or not sigcheck.fp_match(fps[key], data, img, rva):
                sys.exit("%s: bridge entry %s: fingerprint %r %s (a stale, hand-edited or "
                         "wrong-pattern fingerprint, or a locator on the wrong function); "
                         "re-take it with sigs/ida/ida_fp.py" % (mod, ename, key, where))
    # an `sdk` entry is not emitted: the bridge compiles the generated number, so this is the
    # step that proves it equals the binary's (refuse, like the expect/fingerprint checks above)
    for ename, ent in d["entries"].items():
        if not ent.get("sdk"):
            continue
        try:
            want = sigcheck.sdk_eval(ent["sdk"], sdk_defs)
        except (LookupError, ValueError) as ex:
            sys.exit("%s: entry %s: %s" % (mod, ename, ex))
        got = sigcheck.sdk_measured(ent, v[ename])
        if got != want:
            sys.exit("%s: entry %s measures %#x in the binary but its sdk %s = %#x in "
                     "bridge/s2sdk_gen.h: the header and the game disagree - fix the header "
                     "(upstream input) and the entry together, never one side "
                     "(workflows/game-update.md A2b)" % (mod, ename, got, ent["sdk"], want))
    return v


def collect(name, d, v):
    """the header sections one module's database entries map to (JSON order); both writers
    (bridge_pins.h and pins.rs) render from these, so one derivation feeds both"""
    vtables = []            # (macro, module, rtti class)
    vslots = []             # (macro, vtable macro, slot, what, sdk-owned)
    idents = []             # (vtable macro, module, slot) - identity-checked slots
    consts = []             # (macro, value, what) - bridge consts, emitted but never pinned
    fns = []                # (macro, rva, what)
    globs = []              # (macro, rva, what)
    offsets = []            # (macro, value)
    n_sdk = 0               # offset/const entries owned by the generated header (not emitted)
    for ename, ent in d["entries"].items():
        b = ent.get("bridge")
        loc = ent.get("locator")
        loc_type = loc.get("type") if isinstance(loc, dict) else None
        if ent["kind"] == "vtable":
            if "ident_slot" in ent:
                idents.append((b or ename, name, ent["ident_slot"]))
            if b:
                # a Windows vtable is located by its MSVC RTTI class name (the locator's
                # "class"); a linux vtable has none (sig/itanium_vtable/base locators), so
                # the note text is the entry's `notes` there, else the locator's symbol
                cls = loc["class"] if d.get("platform") != "linux" else \
                    (ent.get("notes") or loc.get("symbol", ename))
                vtables.append((b, name, cls))
        elif b and loc_type == "vslot":
            vslots.append((b, loc["vtable"], loc["slot"], ent.get("notes", ""),
                           bool(ent.get("sdk"))))
        elif b and ent["kind"] == "function":
            fns.append((b, v[ename], ent.get("notes", "")))
        elif b and ent["kind"] == "global":
            globs.append((b, v[ename], ent.get("notes", "")))
        elif ent.get("sdk"):
            n_sdk += 1        # owned by the generated header: measured above, not emitted
        elif b and ent["kind"] == "offset":
            offsets.append((b, v[ename]))
        elif b and ent["kind"] == "const":
            consts.append((b, v[ename], ent.get("notes", "")))
        elif b:
            sys.exit("entry %s/%s (bridge %s) has no header mapping" % (name, ename, b))
    return {"vtables": vtables, "vslots": vslots, "idents": idents, "consts": consts,
            "fns": fns, "globs": globs, "offsets": offsets, "n_sdk": n_sdk}


def pin_rows(names, mods, direct, globs, order_vtables, order_vslots, derived, vt_mod,
             slot_targets, ident_targets, check_window):
    """the fingerprint / slot / ident table rows (module index, rva, bytes, what) - the C
    tables' rows and order, one derivation for both writers. `mods[name]["rel"]` is the image's
    relocation set (PE base relocations, or the ELF .rela addresses) and `check_window` refuses
    a pinned window that overlaps one."""
    pb = []                 # (module index, rva, 24 bytes, what)
    ps = []                 # (module index, vtable macro, slot, target rva, what)
    pi = []                 # (module index, macro, rva, what)
    for idx, name in enumerate(names):
        d, pe = mods[name]["data"], mods[name]["pe"]
        for sym, rva, _what in direct[name]:
            check_window(mods, name, rva, 24)
            off = rva_to_off(pe, rva)
            pb.append((idx, rva, d[off:off + 24], sym))
        # prologues of called virtual targets (redundant with the slot pins; cheap)
        seen = set()
        for sdef, vsym, _slot, _what, _sdk in order_vslots:
            if vt_mod[vsym] != name or slot_targets[sdef] in seen:
                continue
            rva = slot_targets[sdef]
            if any(rva <= r < rva + 24 for r in mods[name]["rel"]):
                continue
            seen.add(rva)
            off = rva_to_off(pe, rva)
            pb.append((idx, rva, d[off:off + 24], sdef))
    for sdef, vsym, slot, what, _sdk in order_vslots:
        ps.append((names.index(vt_mod[vsym]), vsym, slot, slot_targets[sdef], what))
    for vsym, slot, target in ident_targets:
        ps.append((names.index(vt_mod[vsym]), vsym, slot, target, "%s slot %d (identity)" % (vsym, slot)))
    for name in names:
        idx = names.index(name)
        for sym, rva, _what in globs[name]:
            rva_to_off(mods[name]["pe"], rva)
            pi.append((idx, sym, rva, sym))
        for sym, mod, _cls in order_vtables:
            if mod == name:
                pi.append((idx, sym, derived[sym], sym))
    return pb, ps, pi


# the section headers the two writers share (the C `/* ... */` section comments; a kind without
# one continues the previous section, like the C's vslots and consts do)
RUST_SECTIONS = {
    "vtable": "vtables (RVA), derived from MSVC RTTI class names",
    "vslot": "virtual slot numbers (interface layout; targets read from the vtable and pinned below)",
    "const": None,
    "function": "direct functions (RVA): non-virtual, non-exported internals - no interface exists; the signatures database",
    "global": "module-internal globals (RVA); the signatures database",
    "offset": "class field offsets / sizes; the signatures database",
}


def emit_rust(build, names, pin_names, mods, rust_pins, pb_rows, ps_rows, pi_rows,
              linux_names, linux_rust_pins, linux_pb, linux_ps, linux_pi):
    """pins.rs: the same pins as the C header, one derivation behind the same checks - one
    `pub const <macro>: Pin` per emitted #define (same names, values, comments and order), the
    module identity / fingerprint / slot / ident tables as `pub static` arrays with the same
    rows in the same order, and the linux platform's own module list as `pub mod linux`."""
    r = []
    w = r.append
    w("// Generated by bridge/pins.py from sigs/signatures. Do not edit.")
    w("//")
    w("// The Windows half (the DLL modules the Wine bridge gates) carries the same rows, in the")
    w("// same order, as bridge/bridge_pins.h; the linux platform is in `pub mod linux` below.")
    w("")
    w("pub const PIN_GAME_BUILD: &str = \"%s\";" % build)
    w("pub const PIN_MODULE_COUNT: usize = %d;" % len(names))
    w("")
    for i, nm in enumerate(pin_names):
        w("/// module index: %s" % cstr(names[i]))
        w("pub const %s: u8 = %d;" % (nm, i))
    w("")
    w("/// A pinned value: `module` is the module's index (the PIN_* consts), `value` the number")
    w("/// the C header #defines by the same name (an RVA for kind \"vtable\"/\"function\"/")
    w("/// \"global\", a slot number for \"vslot\", an offset or const value otherwise).")
    w("pub struct Pin {")
    w("    pub module: u8,")
    w("    pub value: u64,")
    w("    pub kind: &'static str,")
    w("}")
    w("")
    w("/// module identity: the whole-file SHA-256 and PE identity the bridge gates before it")
    w("/// touches a module (bridge_pins.h PinModule; index = the PIN_* consts)")
    w("pub struct PinModule {")
    w("    pub name: &'static str,")
    w("    pub sha256: [u8; 32],")
    w("    pub time_stamp: u32,")
    w("    pub size_of_image: u32,")
    w("    pub check_sum: u32,")
    w("    pub rsds_guid: [u8; 16],")
    w("    pub rsds_age: u32,")
    w("}")
    w("")
    w("/// prologue bytes of a pinned function (bridge_pins.h PinBytes)")
    w("pub struct PinBytes {")
    w("    pub module: u8,")
    w("    pub rva: u32,")
    w("    pub len: u8,")
    w("    pub bytes: [u8; 24],")
    w("    pub what: &'static str,")
    w("}")
    w("")
    w("/// a called virtual slot: vtable RVA, slot, target RVA (bridge_pins.h PinSlot)")
    w("pub struct PinSlot {")
    w("    pub module: u8,")
    w("    pub vt_rva: u32,")
    w("    pub slot: u16,")
    w("    pub target_rva: u32,")
    w("    pub what: &'static str,")
    w("}")
    w("")
    w("/// a module-internal global the bridge reads directly (bridge_pins.h PinIdent)")
    w("pub struct PinIdent {")
    w("    pub module: u8,")
    w("    pub rva: u32,")
    w("    pub what: &'static str,")
    w("}")
    w("")

    def emit_pins(pins, indent="", qual=""):
        # `qual` is the path of the Pin struct from the pins being emitted ("", "super::"):
        # only the top-level module holds the struct definitions, so `pub mod linux` names
        # them `super::Pin` (a reviewer finding: its bare `Pin` failed rustc with E0425).
        prev = None
        for sym, mc, value, kind, note in pins:
            if kind != prev:
                prev = kind
                if RUST_SECTIONS[kind]:
                    w("")
                    w("%s// %s" % (indent, RUST_SECTIONS[kind]))
            if note:
                w("%s/// %s" % (indent, note))
            if kind in ("vslot", "const"):
                w('%spub const %s: %sPin = %sPin { module: %s, value: %d, kind: "%s" };'
                  % (indent, sym, qual, qual, mc, value, kind))
            else:
                w('%spub const %s: %sPin = %sPin { module: %s, value: 0x%x, kind: "%s" };'
                  % (indent, sym, qual, qual, mc, value, kind))

    emit_pins(rust_pins)

    w("")
    w("/// module identity rows (the C k_pinModules table; index = the PIN_* consts)")
    w("pub static PIN_MODULES: &[PinModule] = &[")
    for name in names:
        m = mods[name]
        sha = ", ".join("0x%02x" % b for b in bytes.fromhex(m["sha"]))
        guid = ", ".join("0x%02x" % b for b in m["rsds"][0]) if m["rsds"] else ", ".join(["0"] * 16)
        age = m["rsds"][1] if m["rsds"] else 0
        w('    PinModule { name: "%s", sha256: [%s], time_stamp: 0x%08x, size_of_image: 0x%08x, '
          'check_sum: 0x%08x, rsds_guid: [%s], rsds_age: 0x%x },' % (
            cstr(name), sha, m["pe"]["ts"], m["pe"]["size_of_image"], m["pe"]["checksum"], guid, age))
    w("];")
    w("")
    w("/// prologue bytes of pinned functions (the C k_pinBytes table)")
    w("pub static PIN_BYTES: &[PinBytes] = &[")
    for idx, rva, raw, what in pb_rows:
        w('    PinBytes { module: %d, rva: 0x%x, len: 24, bytes: [%s], what: "%s" },'
          % (idx, rva, cbytes(raw), cstr(what)))
    w("];")
    w("")
    w("/// called virtual slots, the vtable pinned by name (the C k_pinSlots table)")
    w("pub static PIN_SLOTS: &[PinSlot] = &[")
    for idx, vsym, slot, target, what in ps_rows:
        w('    PinSlot { module: %d, vt_rva: %s.value as u32, slot: %d, target_rva: 0x%x, what: "%s" },'
          % (idx, vsym, slot, target, cstr(what)))
    w("];")
    w("")
    w("/// module-internal globals and pinned vtables, by name (the C k_pinIdents table)")
    w("pub static PIN_IDENTS: &[PinIdent] = &[")
    for idx, sym, _rva, what in pi_rows:
        w('    PinIdent { module: %d, rva: %s.value as u32, what: "%s" },' % (idx, sym, cstr(what)))
    w("];")

    w("")
    w("/// Linux pins: sigs/signatures/lib*.json (\"platform\": linux) entries with a `bridge`")
    w("/// name, verified the same way as the Windows modules but against the linux tree")
    w("/// ($CS2_LINUX_BINS, default target/game-bins-linux-<PIN_GAME_BUILD>; sigcheck.py's")
    w("/// linux platform resolution). No linux entry carries a `bridge` name yet, so this")
    w("/// module is empty-but-present (bridge/test_pins_rs.py proves the path with a fixture).")
    w("pub mod linux {")
    w("    pub const PIN_GAME_BUILD: &str = \"%s\";" % build)
    w("    pub const PIN_MODULE_COUNT: usize = %d;" % len(linux_names))
    w("")
    for i, mod in enumerate(linux_names):
        w("    /// module index: %s" % cstr(mod))
        w("    pub const PIN_%s: u8 = %d;" % (mod.rsplit(".", 1)[0].upper(), i))
    emit_pins(linux_rust_pins, indent="    ", qual="super::")
    w("")
    w("    /// module names, by index (the PIN_* consts in this module)")
    w("    pub static MODULES: &[&str] = &[%s];"
      % ", ".join('"%s"' % cstr(n) for n in linux_names))
    w("")
    w("    /// prologue bytes of pinned functions (empty: no linux entry carries a `bridge` name)")
    w("    pub static PIN_BYTES: &[super::PinBytes] = &[")
    for idx, rva, raw, what in linux_pb:
        w('        super::PinBytes { module: %d, rva: 0x%x, len: 24, bytes: [%s], what: "%s" },'
          % (idx, rva, cbytes(raw), cstr(what)))
    w("    ];")
    w("")
    w("    /// called virtual slots (empty: no linux entry carries a `bridge` name)")
    w("    pub static PIN_SLOTS: &[super::PinSlot] = &[")
    for idx, vsym, slot, target, what in linux_ps:
        w('        super::PinSlot { module: %d, vt_rva: %s.value as u32, slot: %d, target_rva: 0x%x, what: "%s" },'
          % (idx, vsym, slot, target, cstr(what)))
    w("    ];")
    w("")
    w("    /// module-internal globals and pinned vtables, by name (empty: no linux `bridge` name)")
    w("    pub static PIN_IDENTS: &[super::PinIdent] = &[")
    for idx, sym, _rva, what in linux_pi:
        w('        super::PinIdent { module: %d, rva: %s.value as u32, what: "%s" },'
          % (idx, sym, cstr(what)))
    w("    ];")
    w("}")
    return "\n".join(r) + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=str(HERE / "bridge_pins.h"))
    ap.add_argument("--rust", metavar="PATH", default=None,
                    help="also write the same pins as Rust (crates/cs2-bridge/src/generated/pins.rs)")
    ap.add_argument("--check", action="store_true",
                    help="write nothing: exit 1 when a file on disk differs from the output")
    ap.add_argument("--sigdir", metavar="DIR", default=None,
                    help="signatures directory to read (default sigs/signatures; test fixture)")
    a = ap.parse_args()
    sigdir = Path(a.sigdir) if a.sigdir else SIGDIR

    names = [n for n, _ in MODULES]
    mods = {}
    dbs = {}
    vals = {}
    macvals = {}    # macro -> resolved value; see below
    sdk_defs = sigcheck.sdk_defines()   # the generated header `sdk` expressions evaluate against
    for name, path in MODULES:
        data = path.read_bytes()
        pe = parse_pe(data)
        rs = rsds(data, pe)
        sha = hashlib.sha256(data).hexdigest()
        mods[name] = {"data": data, "pe": pe, "sha": sha, "rsds": rs, "rel": reloc_rvas(data, pe)}
        print("%-22s ts=%#x soi=%#x sha256=%s" % (name, pe["ts"], pe["size_of_image"], sha[:16]))
        inst = INSTALLED.get(name)
        if inst and inst.exists() and hashlib.sha256(inst.read_bytes()).hexdigest() != sha:
            sys.exit("installed copy differs from repo copy: %s" % inst)
        d = json.loads((sigdir / (name.replace(".dll", "") + ".json")).read_text())
        dbs[name] = d
        pe_resolver = dict(pe)
        pe_resolver.setdefault("export", (0, 0))
        v = pin_checks(name, d, sigdir / (name.replace(".dll", "") + ".json"), data, pe_resolver, sha, sdk_defs)
        vals[name] = v
        # the same values keyed by bridge macro: a bridge entry's *name* need not be its macro
        # (toolframework2.json "CToolFramework2::LoadToolSystem" -> TFVS_LOAD_TOOL_SYSTEM), and the
        # cross-entry lookups below (a vtable by its macro, a vslot's target) are always by macro.
        macvals[name] = {ent["bridge"]: v[ename] for ename, ent in d["entries"].items()
                         if ent.get("bridge") and ename in v}

    # collect every bridge macro and every literal (b) entry, in (module order, JSON order)
    order_vtables = []          # (macro, module, rtti class)
    order_vslots = []           # (macro, vtable macro, slot, what, sdk-owned)
    order_idents = []           # (vtable macro, module, slot) - identity-checked slots
    order_consts = []           # (module, macro, value, what) - bridge consts, emitted but never pinned
    direct = {m: [] for m in names}   # (macro, rva, what)
    globs = {m: [] for m in names}    # (macro, rva, what)
    offsets = []                # (module, macro, value)
    n_sdk = 0                   # offset/const entries owned by the generated header (not emitted)
    for name in names:
        c = collect(name, dbs[name], vals[name])
        order_vtables += c["vtables"]
        order_vslots += c["vslots"]
        order_idents += c["idents"]
        order_consts += [(name, b, val, what) for b, val, what in c["consts"]]
        direct[name] = c["fns"]
        globs[name] = c["globs"]
        offsets += [(name, b, val) for b, val in c["offsets"]]
        n_sdk += c["n_sdk"]

    derived = {sym: macvals[mod][sym] for sym, mod, _cls in order_vtables}
    vt_mod = {sym: mod for sym, mod, _cls in order_vtables}
    for sdef, vsym, _slot, _what, _sdk in order_vslots:
        if vsym not in vt_mod:
            sys.exit("bridge vslot %s references vtable %s, which has no bridge entry in signatures/" % (sdef, vsym))
    for vsym, _mod, _slot in order_idents:
        if vsym not in vt_mod:
            sys.exit("identity slot on vtable %s, which has no bridge entry in signatures/" % vsym)
    slot_targets = {sdef: macvals[vt_mod[vsym]][sdef] for sdef, vsym, _slot, _what, _sdk in order_vslots}
    ident_targets = []
    for vsym, mod, slot in order_idents:
        ident_targets.append((vsym, slot, slot_ptr(mods[mod]["data"], mods[mod]["pe"], derived[vsym], slot)))

    def check_window(pins_mods, mod, rva, length):
        rva_to_off(pins_mods[mod]["pe"], rva)
        bad = [r for r in pins_mods[mod]["rel"] if rva <= r < rva + length]
        if bad:
            sys.exit("pinned window %s %#x overlaps relocations %s" % (mod, rva, bad))

    n_direct = sum(len(v) for v in direct.values())
    n_glob = sum(len(v) for v in globs.values())
    print("derived %d vtables, %d slot targets (%d identity), %d direct fns, %d globals, %d offsets, "
          "%d bridge consts; %d slots + %d offsets/consts owned by "
          "bridge/s2sdk_gen.h (sdk, not emitted)" % (
        len(derived), len(slot_targets), len(ident_targets), n_direct, n_glob, len(offsets),
        len(order_consts), sum(1 for row in order_vslots if row[4]), n_sdk))

    # ------------------------------------------------------------------ linux (the Rust half)
    # The Rust bridge gets the linux platform's pins as `pub mod linux`: entries under
    # sigs/signatures/lib*.json ("platform": linux) carrying a `bridge` name, verified here the
    # same way as the Windows modules (pin_checks) but against the linux tree - the linux
    # platform's binaries root, $CS2_LINUX_BINS or target/game-bins-linux-<CURRENT_BUILD>, the
    # same resolution sigcheck.py resolve --platform linux uses. No linux entry carries a
    # `bridge` name today (the DLL bridge is the only pins consumer), so `pub mod linux` is
    # empty-but-present; bridge/test_pins_rs.py proves the path with a fixture.
    #
    # The linux verification runs only when Rust output is wanted (--rust, or --check with
    # --rust): the C header's Linux story is "windows output only" - a linux entry never
    # reaches bridge_pins.h - and the C-only output (no --rust) needs neither the linux
    # tree nor its binaries (a reviewer finding: the block used to run on a plain `--out`).
    linux_names, linux_mods = [], {}
    linux_vtables, linux_vslots, linux_idents, linux_consts, linux_offsets = [], [], [], [], []
    linux_direct, linux_globs, linux_macvals, linux_pb, linux_ps, linux_pi = {}, {}, {}, [], [], []
    linux_derived, linux_vt_mod = {}, {}       # empty when no --rust: the linux_row building
    linux_slot_targets = {}                    # below then renders nothing (see the C header)
    linux_ident_targets = []
    if a.rust:
        linux_root = Path(os.environ.get("CS2_LINUX_BINS", str(
            sigcheck.ROOT / ("target/game-bins-linux-%s" % sigcheck.CURRENT_BUILD))))
        for jf in sorted(sigdir.glob("lib*.json")):
            d = json.loads(jf.read_text())
            if sigcheck.file_platform(d) != "linux":
                continue
            if "module" not in d:
                sys.exit("%s: no top-level 'module' key" % jf)
            mod = d["module"]
            linux_names.append(mod)
            bpath = sigcheck.module_path(mod, root=linux_root, platform="linux")
            if not bpath.exists():
                sys.exit("%s: linux binary %s is not staged (tree %s); stage the linux tree or point "
                         "$CS2_LINUX_BINS at it" % (mod, bpath, linux_root))
            data = bpath.read_bytes()
            img = sigcheck.load_image(data)
            sha = hashlib.sha256(data).hexdigest()
            linux_mods[mod] = {"data": data, "pe": img, "sha": sha, "rel": set(img["relocs"])}
            print("%-22s ts=%#x soi=%#x sha256=%s" % (mod, img["ts"], img["size_of_image"], sha[:16]))
            v = pin_checks(mod, d, jf, data, img, sha, sdk_defs)
            c = collect(mod, d, v)
            linux_vtables += c["vtables"]
            linux_vslots += c["vslots"]
            linux_idents += c["idents"]
            linux_consts += [(mod, b, val, what) for b, val, what in c["consts"]]
            linux_offsets += [(mod, b, val) for b, val in c["offsets"]]
            linux_direct[mod] = c["fns"]
            linux_globs[mod] = c["globs"]
            # the same values keyed by bridge macro (the vslot cross-lookups are always by macro)
            linux_macvals[mod] = {ent["bridge"]: v[ename] for ename, ent in d["entries"].items()
                                  if ent.get("bridge") and ename in v}
        linux_derived = {sym: linux_macvals[mod][sym] for sym, mod, _cls in linux_vtables}
        linux_vt_mod = {sym: mod for sym, mod, _cls in linux_vtables}
        for sdef, vsym, _slot, _what, _sdk in linux_vslots:
            if vsym not in linux_vt_mod:
                sys.exit("bridge vslot %s references vtable %s, which has no bridge entry in signatures/" % (sdef, vsym))
        for vsym, _mod, _slot in linux_idents:
            if vsym not in linux_vt_mod:
                sys.exit("identity slot on vtable %s, which has no bridge entry in signatures/" % vsym)
        linux_slot_targets = {sdef: linux_macvals[linux_vt_mod[vsym]][sdef]
                              for sdef, vsym, _slot, _what, _sdk in linux_vslots}
        for vsym, mod, slot in linux_idents:
            # the slot's qword read the way the loader leaves it (ELF-aware: sigcheck's
            # slot_target goes through image_qword/.rela - a .so keeps relocated vtable slots
            # zero in the file, the old read would see that zero) and against the image's own
            # base/size (a .so's base 0 vs a PE's 0x180000000)
            target = sigcheck.slot_target(linux_mods[mod]["data"], linux_mods[mod]["pe"],
                                          linux_derived[vsym], slot)
            if target is None:
                sys.exit("linux vtable %#x slot %d is not an image pointer" % (linux_derived[vsym], slot))
            linux_ident_targets.append((vsym, slot, target))

    # the Rust `pub const <macro>: Pin` rows, the C #defines' order and comments (an `sdk`-owned
    # vslot is not emitted, like the C header; the tables below still carry its runtime rows)
    rust_pins = []
    for sym, mod, cls in order_vtables:
        rust_pins.append((sym, PIN_NAMES[names.index(mod)], derived[sym], "vtable",
                          "%s .?AV%s@@" % (mod, cls)))
    for sdef, vsym, slot, what, sdk in order_vslots:
        if not sdk:
            rust_pins.append((sdef, PIN_NAMES[names.index(vt_mod[vsym])], slot, "vslot", what))
    for mod, sdef, value, what in order_consts:
        rust_pins.append((sdef, PIN_NAMES[names.index(mod)], value, "const", what))
    for mod in names:
        for sym, rva, what in direct[mod]:
            rust_pins.append((sym, PIN_NAMES[names.index(mod)], rva, "function", what))
    for mod in names:
        for sym, rva, what in globs[mod]:
            rust_pins.append((sym, PIN_NAMES[names.index(mod)], rva, "global", what))
    for mod, sym, v in offsets:
        rust_pins.append((sym, PIN_NAMES[names.index(mod)], v, "offset", None))

    def linux_pin_const(mod):
        return "PIN_" + mod.rsplit(".", 1)[0].upper()

    linux_rust_pins = []
    for sym, mod, cls in linux_vtables:
        # cls here is the entry's `notes` (or the itanium symbol), not an MSVC RTTI class
        linux_rust_pins.append((sym, linux_pin_const(mod), linux_derived[sym], "vtable", cls))
    for sdef, vsym, slot, what, sdk in linux_vslots:
        if not sdk:
            linux_rust_pins.append((sdef, linux_pin_const(linux_vt_mod[vsym]), slot, "vslot", what))
    for mod, sdef, value, what in linux_consts:
        linux_rust_pins.append((sdef, linux_pin_const(mod), value, "const", what))
    for mod in linux_names:
        for sym, rva, what in linux_direct[mod]:
            linux_rust_pins.append((sym, linux_pin_const(mod), rva, "function", what))
    for mod in linux_names:
        for sym, rva, what in linux_globs[mod]:
            linux_rust_pins.append((sym, linux_pin_const(mod), rva, "global", what))
    for mod, sym, v in linux_offsets:
        linux_rust_pins.append((sym, linux_pin_const(mod), v, "offset", None))

    # the four tables' rows, once: the C header and pins.rs render the same rows in the same order
    pb_rows, ps_rows, pi_rows = pin_rows(names, mods, direct, globs, order_vtables, order_vslots,
                                         derived, vt_mod, slot_targets, ident_targets, check_window)
    linux_pb, linux_ps, linux_pi = pin_rows(linux_names, linux_mods, linux_direct, linux_globs,
                                            linux_vtables, linux_vslots, linux_derived,
                                            linux_vt_mod, linux_slot_targets, linux_ident_targets,
                                            check_window)

    out = []
    w = out.append
    w("/* Generated by bridge/pins.py - do not edit. */")
    w("#ifndef BRIDGE_PINS_H")
    w("#define BRIDGE_PINS_H")
    w("")
    for i, nm in enumerate(PIN_NAMES):
        w("#define %s %d" % (nm, i))
    w("#define PIN_MODULE_COUNT %d" % len(MODULES))
    w("#define PIN_GAME_BUILD \"%s\"   /* CS2 ClientVersion of the pinned binaries (sigs/CURRENT_BUILD) */" % sigcheck.CURRENT_BUILD)
    w("")
    w("/* vtables (RVA), derived from MSVC RTTI class names */")
    for sym, mod, cls in order_vtables:
        w("#define %-26s 0x%x   /* %s .?AV%s@@ */" % (sym, derived[sym], mod, cls))
    w("")
    w("/* virtual slot numbers (interface layout; targets read from the vtable and pinned below) */")
    for sdef, vsym, slot, what, sdk in order_vslots:
        if not sdk:
            w("#define %-30s %d   /* %s */" % (sdef, slot, what))
    for _mod, sdef, _value, what in order_consts:
        w("#define %-30s %d   /* %s */" % (sdef, _value, what))
    w("")
    w("/* direct functions (RVA): non-virtual, non-exported internals - no interface exists; the signatures database */")
    for mod in names:
        for sym, rva, what in direct[mod]:
            w("#define %-30s 0x%x   /* %s */" % (sym, rva, what))
    w("")
    w("/* module-internal globals (RVA); the signatures database */")
    for mod in names:
        for sym, rva, what in globs[mod]:
            w("#define %-30s 0x%x   /* %s */" % (sym, rva, what))
    w("")
    w("/* class field offsets / sizes; the signatures database */")
    for _mod, sym, v in offsets:
        w("#define %-30s 0x%x" % (sym, v))
    w("")
    w("typedef struct {")
    w("    const char *name;")
    w("    u8  sha256[32];")
    w("    u32 timeStamp;")
    w("    u32 sizeOfImage;")
    w("    u32 checkSum;")
    w("    u8  rsdsGuid[16];")
    w("    u32 rsdsAge;")
    w("} PinModule;")
    w("")
    w("typedef struct { u8 mod; u32 rva; u8 len; u8 bytes[24]; const char *what; } PinBytes;")
    w("typedef struct { u8 mod; u32 vtRva; u16 slot; u32 targetRva; const char *what; } PinSlot;")
    w("typedef struct { u8 mod; u32 rva; const char *what; } PinIdent;")
    w("")
    w("static const PinModule k_pinModules[PIN_MODULE_COUNT] = {")
    for name in names:
        m = mods[name]
        sha = ", ".join("0x%02x" % b for b in bytes.fromhex(m["sha"]))
        guid = ", ".join("0x%02x" % b for b in m["rsds"][0]) if m["rsds"] else ", ".join(["0"] * 16)
        age = m["rsds"][1] if m["rsds"] else 0
        w('    { "%s", {%s}, 0x%08x, 0x%08x, 0x%08x, {%s}, 0x%x },' % (
            name, sha, m["pe"]["ts"], m["pe"]["size_of_image"], m["pe"]["checksum"], guid, age))
    w("};")
    w("")
    w("static const PinBytes k_pinBytes[] = {")
    for idx, rva, raw, what in pb_rows:
        w('    { %d, 0x%x, 24, { %s }, "%s" },' % (idx, rva, cbytes(raw), cstr(what)))
    w("};")
    w("")
    w("static const PinSlot k_pinSlots[] = {")
    for idx, vsym, slot, target, what in ps_rows:
        w('    { %d, %s, %d, 0x%x, "%s" },' % (idx, vsym, slot, target, cstr(what)))
    w("};")
    w("")
    w("static const PinIdent k_pinIdents[] = {")
    for idx, sym, _rva, what in pi_rows:
        w('    { %d, %s, "%s" },' % (idx, sym, cstr(what)))
    w("};")
    w("")
    w("#endif /* BRIDGE_PINS_H */")
    c_text = "\n".join(out) + "\n"

    rust_text = emit_rust(sigcheck.CURRENT_BUILD, names, PIN_NAMES, mods, rust_pins,
                          pb_rows, ps_rows, pi_rows,
                          linux_names, linux_rust_pins, linux_pb, linux_ps, linux_pi)
    if a.check:
        # the up-to-date gate: write nothing, refuse on any difference (a file that is missing
        # is a difference too - the build would not be able to read it)
        bad = [str(p) for p, text in [(Path(a.out), c_text)] + ([(Path(a.rust), rust_text)] if a.rust else [])
               if not p.exists() or p.read_text() != text]
        if bad:
            sys.exit("--check: %s differ(s) from the generated output" % ", ".join(bad))
        print("--check: %s up to date" % (", ".join(str(p) for p, _ in
                                                    [(Path(a.out), c_text)] +
                                                    ([(Path(a.rust), rust_text)] if a.rust else []))))
        return
    outpath = Path(a.out)
    outpath.write_text(c_text)
    print("wrote %s (%d lines)" % (outpath, len(out)))
    if a.rust:
        rustpath = Path(a.rust)
        rustpath.parent.mkdir(parents=True, exist_ok=True)
        rustpath.write_text(rust_text)
        print("wrote %s (%d lines)" % (rustpath, rust_text.count("\n")))


if __name__ == "__main__":
    main()
