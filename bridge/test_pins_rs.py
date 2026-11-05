#!/usr/bin/env python3
"""test_pins_rs.py - parity, compile and fixture tests for bridge/pins.py's Rust output.

All checks run against the real database and binaries (they exercise the same resolution
and fail-closed checks pins.py always runs):

  1. regen            - pins.py run to scratch paths re-writes bridge/bridge_pins.h
                        byte-identical
  2. parity           - every `#define NAME VALUE` of bridge_pins.h has a `pub const NAME`
                        in crates/cs2-bridge/src/generated/pins.rs with the same value (and
                        vice versa), in the SAME ORDER, PIN_GAME_BUILD matches, and the four
                        tables have equal row counts
  3. compile          - `rustc --edition 2021 --crate-type rlib -D warnings` builds the
                        committed pins.rs, and every fixture's output below (an output that
                        fails rustc, e.g. an unqualified `Pin` in `pub mod linux`, FAILs here)
  4. linux fixture    - a scratch copy of the signatures dir where ONE libclient.json entry
                        gains `"bridge": "TEST_LINUX_PIN"` emits it in pins.rs's
                        `pub mod linux` with the value expect[CURRENT_BUILD] records
  5. linux ident      - the real libclient.json CCSGOInput vtable entry gains `"bridge"` +
                        an `ident_slot`: it is emitted as a `kind: "vtable"` Pin, the linux
                        PIN_SLOTS identity row carries the slot target resolved through the
                        .rela table (sigcheck.slot_target's ELF path - the raw file bytes of
                        a relocated slot are zero), and a deliberately WRONG ident_slot is
                        refused. The fixture needs a fingerprint for key "CCSGOInput#<slot>"
                        (fp_keys' rule for an entry with ident_slot); the test derives it
                        from the binary itself (the concrete bytes at the slot's target) and
                        writes/retires its own sigs/fingerprints/libclient-fp.json.
  6. negative         - a scratch copy where one module's builds[CURRENT_BUILD] record is
                        deleted makes pins.py refuse with the builds[] message and write
                        NEITHER file

Usage: python3 bridge/test_pins_rs.py   (needs the binaries: $CS2_BINS, the linux tree)
"""
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
PINS = HERE / "pins.py"
C_HEADER = HERE / "bridge_pins.h"
RUST = ROOT / "crates/cs2-bridge/src/generated/pins.rs"
SCRATCH = ROOT / "target/scratch/test_pins_rs"
SIGDIR = ROOT / "sigs/signatures"
CURRENT_BUILD = (ROOT / "sigs/CURRENT_BUILD").read_text().strip()
LINUX_ROOT = ROOT / ("target/game-bins-linux-%s" % CURRENT_BUILD)

sys.path.insert(0, str(ROOT / "sigs" / "scripts"))
import sigcheck  # noqa: E402  (only to derive the fixture's expected numbers, the same helpers)

failures = []


def check(ok, what, detail=""):
    print("%-4s %s%s" % ("PASS" if ok else "FAIL", what, (" - " + detail) if detail else ""))
    if not ok:
        failures.append(what)
    return ok


def run_pins(*args):
    """pins.py as a subprocess (the interpreter running this script: build-rs.sh runs pins.py the
    same way, and the resolver's numpy lives in the user site-packages - `python3 -I` does not
    see it). Returns (returncode, output)."""
    p = subprocess.run([sys.executable, str(PINS), *args], capture_output=True, text=True)
    return p.returncode, (p.stdout + p.stderr)


def rustc_ok(path, what):
    """rustc --edition 2021 --crate-type rlib -D warnings on `path` (the output goes under
    target/scratch): False, naming the error, when it does not compile."""
    if shutil.which("rustc") is None:
        return check(False, "compile: rustc", "rustc is not on PATH - cannot verify compilation")
    out = SCRATCH / (path.stem + "-par.rlib")
    p = subprocess.run(["rustc", "--edition", "2021", "--crate-type", "rlib", "-D", "warnings",
                        "-o", str(out), str(path)], capture_output=True, text=True)
    return check(p.returncode == 0, "compile: rustc -D warnings builds %s" % what,
                 "" if p.returncode == 0 else (p.stdout + p.stderr)[-2000:])


DEFINE_RX = re.compile(r"^#define (\S+) (.+?)(?:\s+/\*.*\*/)?\s*$", re.M)
RUST_MOD_CONST_RX = re.compile(r"^pub const (\w+): u8 = (\d+);", re.M)
RUST_USIZE_RX = re.compile(r"^pub const (\w+): usize = (\d+);", re.M)
RUST_PIN_RX = re.compile(r'^pub const (\w+): Pin = Pin \{ module: (\w+), value: (0x[0-9a-f]+|\d+), kind: "(\w+)" \};', re.M)
RUST_BUILD_RX = re.compile(r'^pub const PIN_GAME_BUILD: &str = "([^"]*)";', re.M)


def c_defines(text):
    """`#define NAME VALUE` -> {name: value} (int when numeric, else the raw string)"""
    out = {}
    for m in DEFINE_RX.finditer(text):
        name, val = m.group(1), m.group(2).strip()
        out[name] = int(val, 0) if re.fullmatch(r"(0[xX][0-9a-fA-F]+|\d+)", val) else val.strip('"')
    return out


def c_pin_order(text):
    """the emitted #define names of bridge_pins.h, in emission order: the defines between the
    vtables section header and the table structs (the writers' shared derivation is that
    order - vtables, vslots, consts, functions, globals, offsets, module by module)."""
    _head, sep, region = text.partition("/* vtables (RVA), derived from MSVC RTTI class names */")
    if not sep:
        return None
    region = region.split("typedef struct {", 1)[0]
    return re.findall(r"^#define (\S+) ", region, re.M)


def c_table_rows(text, table):
    """rows of one C table: the lines between `static const <T> <table>[] = {` and `};`"""
    body = re.search(r"static const \w+ %s\[\w*\] = \{\n(.*?)\n\};" % table, text, re.S)
    return [ln for ln in body.group(1).splitlines() if ln.strip().startswith("{")]


def rust_consts(rust):
    """every top-level `pub const NAME` of pins.rs -> {name: value} (module consts and Pin
    consts alike; the linux module's are indented and so not matched)"""
    out = {m.group(1): int(m.group(2), 0) for m in RUST_MOD_CONST_RX.finditer(rust)}
    out.update({m.group(1): int(m.group(2), 0) for m in RUST_USIZE_RX.finditer(rust)})
    for m in RUST_PIN_RX.finditer(rust):
        out[m.group(1)] = int(m.group(3), 0)
    m = RUST_BUILD_RX.search(rust)
    out["PIN_GAME_BUILD"] = m.group(1)
    return out


def rust_table_rows(rust, table, struct):
    """rows of one top-level Rust table (`pub static <TABLE>: &[<Struct>]` - the linux module's
    tables are typed `&[super::<Struct>]` and so not matched)"""
    body = re.search(r"pub static %s: &\[%s\] = &\[\n(.*?)\n\];" % (table, struct), rust, re.S)
    return [ln for ln in body.group(1).splitlines() if ln.strip().startswith("Pin")]


def linux_mod_section(rust):
    """the linux module's text: everything from `pub mod linux` on"""
    off = rust.index("pub mod linux")
    return rust[off:]


def scratch_sigdir(name):
    """a scratch copy of sigs/signatures to point --sigdir at"""
    d = SCRATCH / name
    if d.exists():
        shutil.rmtree(d)
    d.mkdir(parents=True)
    for jf in SIGDIR.glob("*.json"):
        shutil.copy(jf, d / jf.name)
    return d


def main():
    SCRATCH.mkdir(parents=True, exist_ok=True)
    if not LINUX_ROOT.exists():
        print("FAIL target/game-bins-linux-%s is not staged - refusing to run "
              "(a gate that cannot run must fail loudly)" % CURRENT_BUILD)
        sys.exit(1)

    # ---- 1. regen: the committed C header is what pins.py writes
    c_out = SCRATCH / "bridge_pins.h"
    rust_out = SCRATCH / "pins.rs"
    rc, out = run_pins("--out", str(c_out), "--rust", str(rust_out))
    if not check(rc == 0, "regen: pins.py --out scratch --rust scratch exits 0", out.strip().splitlines()[-1] if out.strip() else ""):
        print(out)
        return
    check(c_out.read_text() == C_HEADER.read_text(),
          "regen: bridge/bridge_pins.h byte-identical to what pins.py writes")
    check(rust_out.read_text() == RUST.read_text(),
          "regen: the scratch pins.rs equals the committed crates/cs2-bridge/src/generated/pins.rs")

    # ---- 2. parity: C #defines <-> Rust pub consts, same order, and the four tables' rows
    c = c_defines(C_HEADER.read_text())
    rs = rust_consts(rust_out.read_text())
    missing = sorted(set(c) - set(rs))
    extra = sorted(set(rs) - set(c))
    bad = ["%s: C=%r rust=%r" % (n, c[n], rs[n]) for n in sorted(set(c) & set(rs))
           if c[n] != rs[n]]
    check(not missing and not extra and not bad,
          "parity: %d C #defines <-> %d Rust pub consts, 0 mismatches" % (len(c), len(rs)),
          "" if not (missing or extra or bad) else
          "missing %s; rust-only %s; mismatched %s" % (missing, extra, bad))
    c_order = c_pin_order(C_HEADER.read_text())
    r_order = [m.group(1) for m in RUST_PIN_RX.finditer(rust_out.read_text())]
    if c_order is not None and c_order == r_order:
        check(True, "order: the %d emitted Pin consts follow the C #define order exactly" % len(r_order))
    else:
        first = next((i for i, (a, b) in enumerate(zip(c_order or [], r_order)) if a != b),
                     max(len(c_order or []), len(r_order)))
        check(False, "order: the emitted Pin consts follow the C #define order",
              "first difference at %d: C=%r rust=%r (len C=%s rust=%d)"
              % (first,
                 (c_order or [None] * (first + 1))[first] if len(c_order or []) > first else None,
                 r_order[first] if len(r_order) > first else None,
                 len(c_order or []), len(r_order)))
    rust_tables = {"k_pinModules": "PIN_MODULES", "k_pinBytes": "PIN_BYTES",
                   "k_pinSlots": "PIN_SLOTS", "k_pinIdents": "PIN_IDENTS"}
    for table, struct in [("k_pinModules", "PinModule"), ("k_pinBytes", "PinBytes"),
                          ("k_pinSlots", "PinSlot"), ("k_pinIdents", "PinIdent")]:
        n_c = len(c_table_rows(C_HEADER.read_text(), table))
        n_r = len(rust_table_rows(rust_out.read_text(), rust_tables[table], struct))
        check(n_c == n_r and n_c > 0, "parity: %s row count %d == pins.rs %s row count %d"
              % (table, n_c, rust_tables[table], n_r))
    rustc_ok(RUST, "the committed pins.rs")

    # ---- 3. linux fixture: one libclient.json entry gains a `bridge` name
    fix = scratch_sigdir("fixture")
    libclient = fix / "libclient.json"
    d = json.loads(libclient.read_text())
    ent_name = "CCONST_IDENTITY_CHUNK_BITS"
    ent = d["entries"][ent_name]
    want = ent["expect"][CURRENT_BUILD]
    ent["bridge"] = "TEST_LINUX_PIN"
    libclient.write_text(json.dumps(d, indent=1))
    fx_c, fx_r = SCRATCH / "fixture_bridge_pins.h", SCRATCH / "fixture_pins.rs"
    rc, out = run_pins("--sigdir", str(fix), "--out", str(fx_c), "--rust", str(fx_r))
    if not check(rc == 0, "fixture: pins.py --sigdir scratch (libclient.json + TEST_LINUX_PIN) exits 0",
                 out.strip().splitlines()[-1] if out.strip() else ""):
        print(out)
        return
    fx_rust = fx_r.read_text()
    mod = linux_mod_section(fx_rust)
    ok = re.search(r'pub const TEST_LINUX_PIN: super::Pin = super::Pin \{ module: PIN_LIBCLIENT, value: (0x[0-9a-f]+|\d+), kind: "const" \};', mod)
    check(bool(ok) and int(ok.group(1), 0) == want,
          "fixture: TEST_LINUX_PIN in pub mod linux with value %d (expect[%s])" % (want, CURRENT_BUILD),
          "" if ok else "not found in pub mod linux")
    check(bool(re.search(r"pub const PIN_GAME_BUILD", mod)), "fixture: pub mod linux carries its own PIN_GAME_BUILD")
    check(fx_c.read_text() == C_HEADER.read_text(),
          "fixture: the C header is unchanged by a linux bridge entry (windows output only)")
    rustc_ok(fx_r, "the linux-fixture pins.rs (pub mod linux with TEST_LINUX_PIN)")

    # ---- 4. linux ident fixture: the real CCSGOInput vtable entry gains `bridge` + ident_slot
    # (fp_keys' rule: an entry with ident_slot needs key "CCSGOInput#<slot>" in the module's
    # fingerprint file, so the test retires its own sigs/fingerprints/libclient-fp.json).
    ident_dir = scratch_sigdir("fixture-ident")
    fpfile = ROOT / "sigs/fingerprints/libclient-fp.json"
    temp_fp = not fpfile.exists()
    try:
        d = json.loads((ident_dir / "libclient.json").read_text())
        vt = d["entries"]["CCSGOInput"]
        vt_want = vt["expect"][CURRENT_BUILD]
        binpath = sigcheck.module_path("libclient.so", root=LINUX_ROOT, platform="linux")
        data = binpath.read_bytes()
        img = sigcheck.load_image(data)
        SLOT = 3                     # any slot of the table; 3 is a real virtual target
        target = sigcheck.slot_target(data, img, vt_want, SLOT)
        if not check(target is not None, "ident fixture: sigcheck.slot_target (ELF path) reads the "
                     "relocation-resolved pointer of libclient.so CCSGOInput slot %d" % SLOT):
            print("the old PE-style read saw 0 here (a .so keeps relocated slots zero in the file)")
            return
        off = sigcheck.rva_to_off(img, target)
        pattern = " ".join("%02X" % b for b in data[off:off + 24])
        if temp_fp:
            fpfile.write_text(json.dumps({"CCSGOInput#%d" % SLOT: pattern}, indent=1))
        vt["bridge"] = "TEST_LINUX_VT"
        vt["ident_slot"] = SLOT
        (ident_dir / "libclient.json").write_text(json.dumps(d, indent=1))
        id_c, id_r = SCRATCH / "ident_bridge_pins.h", SCRATCH / "ident_pins.rs"
        rc, out = run_pins("--sigdir", str(ident_dir), "--out", str(id_c), "--rust", str(id_r))
        if not check(rc == 0, "ident fixture: pins.py --sigdir scratch (CCSGOInput + TEST_LINUX_VT "
                     "+ ident_slot %d) exits 0" % SLOT,
                     out.strip().splitlines()[-1] if out.strip() else ""):
            print(out)
            return
        id_rust = id_r.read_text()
        mod = linux_mod_section(id_rust)
        ok = re.search(r'pub const TEST_LINUX_VT: super::Pin = super::Pin \{ module: PIN_LIBCLIENT, value: (0x[0-9a-f]+|\d+), kind: "vtable" \};', mod)
        check(bool(ok) and int(ok.group(1), 0) == vt_want,
              "ident fixture: TEST_LINUX_VT in pub mod linux, value %d (= expect[%s], the vtable RVA)"
              % (vt_want, CURRENT_BUILD), "" if ok else "not found or off value")
        ok = re.search(r'super::PinSlot \{ module: \d+, vt_rva: TEST_LINUX_VT\.value as u32, slot: %d, '
                       r'target_rva: (0x[0-9a-f]+|\d+), what: "TEST_LINUX_VT slot %d \(identity\)" \}' % (SLOT, SLOT), mod)
        check(bool(ok) and int(ok.group(1), 0) == target,
              "ident fixture: the linux PIN_SLOTS identity row carries target %#x "
              "(slot_target's ELF/.rela read)" % target,
              "" if ok else "no identity row with slot %d" % SLOT)
        check(id_c.read_text() == C_HEADER.read_text(),
              "ident fixture: the C header is unchanged by linux-only entries")
        rustc_ok(id_r, "the linux-ident-fixture pins.rs")

        # negative: a deliberately wrong ident_slot is refused (and neither file written)
        vt["ident_slot"] = 300          # beyond the table: not an address point any more
        (ident_dir / "libclient.json").write_text(json.dumps(d, indent=1))
        wn_c, wn_r = SCRATCH / "wrong_bridge_pins.h", SCRATCH / "wrong_pins.rs"
        for p in (wn_c, wn_r):
            p.unlink(missing_ok=True)
        rc, out = run_pins("--sigdir", str(ident_dir), "--out", str(wn_c), "--rust", str(wn_r))
        check(rc != 0, "ident fixture NEGATIVE: a deliberately wrong ident_slot (300) is refused (exit %d)" % rc,
              (out.strip().splitlines()[-1] if out.strip() else ""))
        check(not wn_c.exists() and not wn_r.exists(),
              "ident fixture NEGATIVE: NEITHER the C header nor pins.rs was written")
    finally:
        if temp_fp and fpfile.exists():
            fpfile.unlink()

    # ---- 5. negative: a module whose builds[CURRENT_BUILD] record is gone is refused, neither file written
    neg = scratch_sigdir("negative")
    tier0 = neg / "tier0.json"
    d = json.loads(tier0.read_text())
    del d["builds"][CURRENT_BUILD]
    tier0.write_text(json.dumps(d, indent=1))
    ng_c, ng_r = SCRATCH / "negative_bridge_pins.h", SCRATCH / "negative_pins.rs"
    for p in (ng_c, ng_r):
        p.unlink(missing_ok=True)
    rc, out = run_pins("--sigdir", str(neg), "--out", str(ng_c), "--rust", str(ng_r))
    check(rc != 0 and "is not builds[%s]" % CURRENT_BUILD in out,
          "negative: pins.py refuses with the builds[%s] message (exit %d)" % (CURRENT_BUILD, rc),
          [ln for ln in out.splitlines() if "builds[" in ln][-1] if "builds[" in out else out[-200:])
    check(not ng_c.exists() and not ng_r.exists(),
          "negative: NEITHER the C header nor pins.rs was written")

    if failures:
        print("\nFAILED:", *("  " + f for f in failures), sep="\n")
        sys.exit(1)
    print("\nall pins.rs parity, compile and fixture checks passed")


if __name__ == "__main__":
    main()
