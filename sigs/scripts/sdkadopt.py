#!/usr/bin/env python3
"""sdkadopt.py - hand a sigs entry's number to the generated SDK header (`sdk` field).

The bridge uses the generated number (S2_SLOT_CBaseEditorSession_Save, in Rust
`CBaseEditorSession_slots::Save`) instead of a sigs-derived pin (HVS_SAVE); the entry stays as the
binary-side measurement. Adopting writes the expression into the entry's `sdk` field. From then on
`cargo bridge pins` does not emit a pin for it, refuses when the expression and the binary
disagree, and sdkcheck.py reports an `sdk:` row.

The expression is given explicitly and validated at once, before anything is written:
  * every macro in it is emitted by crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h (sigcheck.sdk_eval; MSVC packs overload
    groups in reverse, so a name picked by eye is easily the wrong slot: StartSoundEvent_2 is
    slot 11, copy the exact name from s2sdk_gen.h);
  * the entry has a measurement locator, and sigcheck.sdk_measured (a vslot's slot index, else
    the resolved value, read from the game binary) equals the evaluated expression. An entry
    without a locator (a literal) backs nothing in a binary and is refused.

A PIN is a bridge macro or an entry name. Entries that already carry the same `sdk` are left alone
((already)); a different one is refused.

Usage:
  sigs/scripts/sdkadopt.py [--dry-run] PIN=S2_EXPR [PIN=S2_EXPR ...]
  sigs/scripts/sdkadopt.py --list            # every entry carrying `sdk`
Needs the game binaries ($CS2_BINS, sigs/README.md "Binaries"); run with plain python3 (numpy).
"""
import argparse
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import sigcheck as sc  # noqa: E402
import sdkcheck  # noqa: E402


def entries_by_pin(db):
    """pin -> (path, doc, entry name) over every Windows file"""
    out = {}
    for path, d in db.items():
        for name, ent in d["entries"].items():
            pin = ent.get("bridge") or name
            if pin in out:
                continue   # an unbridged entry that shares a name with a macro: the macro wins
            out[pin] = (path, d, name)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("pins", nargs="*", metavar="PIN=S2_EXPR")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--list", action="store_true")
    a = ap.parse_args()

    db = sc.load_db(platform="windows")
    by_pin = entries_by_pin(db)

    if a.list:
        for pin, (path, d, name) in by_pin.items():
            expr = d["entries"][name].get("sdk")
            if expr:
                print("%-34s <- %s" % (pin, expr))
        return 0
    if not a.pins:
        ap.error("name at least one PIN=S2_EXPR (or --list)")

    wanted = []
    for arg in a.pins:
        pin, eq, expr = arg.partition("=")
        if not eq:
            sys.exit("%s: give PIN=S2_EXPR" % pin)
        if not pin or not expr:
            sys.exit("%r: expected PIN=S2_EXPR" % arg)
        wanted.append((pin, expr))

    defines = sc.sdk_defines()
    probes = sdkcheck.Probes(db)
    touched = {}
    for pin, expr in wanted:
        if pin not in by_pin:
            sys.exit("%s: no Windows sigs entry is named or bridged as it" % pin)
        path, d, name = by_pin[pin]
        ent = d["entries"][name]
        try:
            want = sc.sdk_eval(expr, defines)     # every macro must be emitted by s2sdk_gen.h
        except (LookupError, ValueError) as ex:
            sys.exit("%s: refused: %s" % (pin, ex))
        if not ent.get("locator"):
            sys.exit("%s: refused: %s/%s has no measurement locator (a literal backs nothing in a "
                     "binary); `sdk` is the header held to a measurement" % (pin, path.name, name))
        try:
            game = sc.sdk_measured(ent, probes.resolve(d["module"], name))
        except (LookupError, ValueError, AssertionError, IndexError) as ex:
            sys.exit("%s: refused: the entry does not resolve on build %s: %s" % (pin, sc.CURRENT_BUILD, ex))
        if game != want:
            sys.exit("%s: refused: %s = %d but %s/%s measures %d in the binary"
                     % (pin, expr, want, path.name, name, game))
        if ent.get("sdk") == expr:
            print("= %-30s %s (already)" % (pin, expr))
            continue
        if ent.get("sdk"):
            sys.exit("%s/%s already carries sdk %s, not %s" % (path.name, name, ent["sdk"], expr))
        # keep `sdk` next to `bridge` (or `kind` when there is none) for readable diffs
        anchor = "bridge" if "bridge" in ent else "kind"
        new = {}
        for k, v in ent.items():
            new[k] = v
            if k == anchor:
                new["sdk"] = expr
        d["entries"][name] = new
        touched[path] = d
        print("+ %-30s %s = %d  (%s/%s)" % (pin, expr, want, path.name, name))

    if a.dry_run:
        print("dry run: %d file(s) not written" % len(touched))
        return 0
    for path, d in touched.items():
        errs = sc.validate_file(path, d)
        if errs:
            sys.exit("refusing to write %s:\n  %s" % (path.name, "\n  ".join(errs)))
        # keep the file's own indentation (pet.json is indent=2, the rest indent=1)
        lines = path.read_text().splitlines()
        indent = len(lines[1]) - len(lines[1].lstrip(" ")) if len(lines) > 1 else 1
        path.write_text(json.dumps(d, indent=indent or 1, ensure_ascii=False) + "\n")
    print("wrote %d file(s); next: `cargo bridge pins`, sigs/scripts/sdkcheck.py, then switch the C code "
          "to the S2_* macro" % len(touched))
    return 0


if __name__ == "__main__":
    sys.exit(main())
