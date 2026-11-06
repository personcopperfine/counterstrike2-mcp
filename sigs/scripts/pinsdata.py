#!/usr/bin/env python3
"""pinsdata.py - the evidence half of `cargo bridge pins`: resolve and gate every bridge entry,
and print what the pins.rs renderer needs as one JSON document.

`cargo bridge pins` (crates/cs2-bridge-tools/src/pins.rs) writes
crates/cs2-bridge/src/generated/pins.rs. The values in it are never derived in Rust: this script
resolves the locators with sigcheck.py's Resolver, the one resolver `sigcheck.py resolve` and
`sigupdate.py --bridge-gate` use, so the pins and `sigcheck resolve` can never disagree. The Rust
side maps the entries to sections, orders them and renders the file.

Because its output is what gets pinned, this script fails closed on the evidence itself, per
module, Windows and linux alike, before it prints anything:
  * the database passes `sigcheck.py validate` (structure, the fingerprints a bridge entry
    requires, the status enum);
  * a bridge entry marked `status: stale` is refused;
  * the binary is builds[CURRENT_BUILD] (whole-file SHA-256, first 16 hex digits);
  * every bridge entry's expect[CURRENT_BUILD] equals what its locator resolved, and the
    fingerprints it requires match the bytes there (fp_checks / fp_match, the gate's helpers);
  * every `sdk` expression, evaluated against crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h, equals
    the entry's measurement (sdk_measured).
That closes the paths a run of `validate`/`resolve` alone leaves open: a fingerprint key that
exists but does not match the bytes, and an `expect[]` that was dropped or hand-edited. Any
refusal exits non-zero with the message on stderr and prints no JSON.

Modules: the Windows DLLs named on the command line, in that order (the Rust side owns the list
and its PIN_* names), from $CS2_BINS; and every sigs/signatures/lib*.json with platform linux,
sorted by file name, from the linux tree ($CS2_LINUX_BINS, default
target/game-bins-linux-<CURRENT_BUILD>, the resolution `sigcheck.py resolve --platform linux`
uses).

Output (stdout): {"build", "windows": [module], "linux": [module]}; a module is its identity
(sha256, time_stamp, size_of_image, check_sum and the RSDS guid/age on Windows) and the entries
that carry `bridge`, `sdk` or `ident_slot`, in JSON order, each with its kind, bridge name,
locator, notes, resolved `value`, and the facts the renderer cannot read without the binary:
`window` (the 24 bytes at an RVA-valued bridge entry, null outside every section, and the
relocations inside that window) and `ident_target` (the function a vtable's `ident_slot` holds,
read the loader's way: sigcheck.slot_target, .rela-aware on ELF).

Run with plain `python3`, not `python3 -I`: the resolver's numpy lives in the user
site-packages.

Usage: sigs/scripts/pinsdata.py [--sigdir DIR] MODULE.dll...
"""
import argparse
import hashlib
import json
import os
import struct
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import sigcheck  # noqa: E402

WINDOW = 24     # the prologue length the bridge compares (PinBytes.len)


def pe_identity(data, img):
    """The PE identity the bridge gates beyond what sigcheck.parse_pe keeps: the optional
    header's CheckSum, the RSDS (guid, age) of the debug directory, and the base-relocation
    RVAs (a pinned byte window must not overlap one: the loader rewrites those bytes)."""
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    opt = pe + 24
    checksum = struct.unpack_from("<I", data, opt + 64)[0]
    nrv = struct.unpack_from("<I", data, opt + 0x6C)[0]
    dirs = [struct.unpack_from("<II", data, opt + 0x70 + 8 * i) for i in range(nrv)]
    rel = dirs[5] if len(dirs) > 5 else (0, 0)
    debug = dirs[6] if len(dirs) > 6 else (0, 0)

    relocs = set()
    if rel[0] and rel[1]:
        off = sigcheck.rva_to_off(img, rel[0])
        end = off + rel[1]
        while off + 8 <= end:
            page, block = struct.unpack_from("<II", data, off)
            if block < 8:
                break
            for i in range((block - 8) // 2):
                e = struct.unpack_from("<H", data, off + 8 + 2 * i)[0]
                relocs.add(page + (e & 0xFFF))
            off += block

    rsds = None
    if debug[0] and debug[1]:
        off = sigcheck.rva_to_off(img, debug[0])
        for i in range(debug[1] // 28):
            b = off + 28 * i
            if struct.unpack_from("<I", data, b + 12)[0] != 2:      # IMAGE_DEBUG_TYPE_CODEVIEW
                continue
            doff = sigcheck.rva_to_off(img, struct.unpack_from("<I", data, b + 20)[0])
            if data[doff:doff + 4] == b"RSDS":
                rsds = (data[doff + 4:doff + 20], struct.unpack_from("<I", data, doff + 20)[0])
                break
    return checksum, rsds, relocs


def pin_checks(mod, d, db_path, data, img, sha, sdk_defs):
    """The fail-closed checks every pinned module's database is written behind (see the module
    docstring). Returns the resolved values (entry name -> RVA or integer) of the entries with a
    `bridge` or an `sdk` field."""
    # this is the step that feeds the pins, so it checks rather than assuming the caller ran
    # `validate`
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
            # gained a bridge name after it was judged, or an entry the judge left stale and
            # printed as PASS. Either way the locator is not known to land on the right code.
            sys.exit("%s: bridge entry %s (macro %s) is marked status 'stale': it was not SAME/OK "
                     "the last time a person judged it, so its locator is not known to land on the "
                     "right code; re-check the `via` site and clear status before pinning"
                     % (mod, ename, ent["bridge"]))
        if not (has_bridge or ent.get("sdk")):
            continue          # only these are pinned (bridge) or measured for the generated
                              # header (sdk)
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
                     "crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h: the header and the game "
                     "disagree - fix the header (upstream input) and the entry together, never "
                     "one side (workflows/game-update.md A2b)" % (mod, ename, got, ent["sdk"], want))
    return v


def window(data, img, relocs, rva):
    """The pinned byte window at `rva`: its bytes (hex; null when the RVA is in no section) and
    the relocated addresses inside it."""
    try:
        off = sigcheck.rva_to_off(img, rva)
        raw = data[off:off + WINDOW].hex()
    except (ValueError, IndexError):
        raw = None
    return {"bytes": raw, "relocs": [a for a in range(rva, rva + WINDOW) if a in relocs]}


def export_module(mod, d, db_path, bpath, platform, sdk_defs):
    """One module's identity and pin-relevant entries (see the module docstring), after
    pin_checks passed."""
    data = bpath.read_bytes()
    img = sigcheck.load_image(data)
    sha = hashlib.sha256(data).hexdigest()
    out = {"module": mod, "sha256": sha, "time_stamp": img["ts"],
           "size_of_image": img["size_of_image"]}
    if platform == "windows":
        checksum, rsds, relocs = pe_identity(data, img)
        out.update(check_sum=checksum,
                   rsds_guid=rsds[0].hex() if rsds else None,
                   rsds_age=rsds[1] if rsds else 0)
    else:
        relocs = set(img["relocs"])
    v = pin_checks(mod, d, db_path, data, img, sha, sdk_defs)
    entries = []
    for ename, ent in d["entries"].items():
        if not (ent.get("bridge") or ent.get("sdk") or "ident_slot" in ent):
            continue
        loc = ent.get("locator")
        row = {"name": ename, "kind": ent["kind"], "bridge": ent.get("bridge") or None,
               "locator": loc if isinstance(loc, dict) else None, "notes": ent.get("notes"),
               "sdk": bool(ent.get("sdk")), "ident_slot": ent.get("ident_slot"),
               "value": v.get(ename)}
        loc_type = loc.get("type") if isinstance(loc, dict) else None
        if row["bridge"] and row["value"] is not None and \
                (ent["kind"] in ("function", "global") or loc_type == "vslot"):
            row["window"] = window(data, img, relocs, row["value"])
        if "ident_slot" in ent and row["value"] is not None:
            row["ident_target"] = sigcheck.slot_target(data, img, row["value"], ent["ident_slot"])
        entries.append(row)
    out["entries"] = entries
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    ap.add_argument("--sigdir", metavar="DIR", default=None,
                    help="signatures directory to read (default sigs/signatures; test fixture)")
    ap.add_argument("modules", nargs="+", metavar="MODULE.dll",
                    help="the Windows modules to pin, in PIN_* index order")
    a = ap.parse_args()
    sigdir = Path(a.sigdir) if a.sigdir else sigcheck.SIGDIR
    sdk_defs = sigcheck.sdk_defines()

    windows = []
    for mod in a.modules:
        db_path = sigdir / (mod.rsplit(".", 1)[0] + ".json")
        d = json.loads(db_path.read_text())
        bpath = sigcheck.module_path(mod)
        if not bpath.exists():
            sys.exit("%s: binary %s is not staged ($CS2_BINS, default target/game-bins; "
                     "sigs/README.md \"Binaries\")" % (mod, bpath))
        windows.append(export_module(mod, d, db_path, bpath, "windows", sdk_defs))

    linux = []
    linux_root = Path(os.environ.get("CS2_LINUX_BINS", str(
        sigcheck.ROOT / ("target/game-bins-linux-%s" % sigcheck.CURRENT_BUILD))))
    for jf in sorted(sigdir.glob("lib*.json")):
        d = json.loads(jf.read_text())
        if sigcheck.file_platform(d) != "linux":
            continue
        if "module" not in d:
            sys.exit("%s: no top-level 'module' key" % jf)
        mod = d["module"]
        bpath = sigcheck.module_path(mod, root=linux_root, platform="linux")
        if not bpath.exists():
            sys.exit("%s: linux binary %s is not staged (tree %s); stage the linux tree or point "
                     "$CS2_LINUX_BINS at it" % (mod, bpath, linux_root))
        linux.append(export_module(mod, d, jf, bpath, "linux", sdk_defs))

    json.dump({"build": sigcheck.CURRENT_BUILD, "windows": windows, "linux": linux},
              sys.stdout, indent=1)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
