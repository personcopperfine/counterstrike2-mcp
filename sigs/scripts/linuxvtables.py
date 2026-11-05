#!/usr/bin/env python3
"""linuxvtables.py - compare the s2sdk IR's Itanium vtable lengths with the game's Linux binaries.

    linuxvtables.py [--tree target/game-bins-linux-2000930] [--modules libtier0.so ...]

For every class with a vtable in `crates/s2sdk/s2sdk-sys/ir/*.json`:
  * the IR's Itanium length: every primary base's length plus this class's own new declarations,
    a destructor counting two slots (the rule `gen.py itanium_vtable_len` implements; this script
    re-implements it and matches gen.py for all 11 IR vtable classes);
  * the game's: the class's `_ZTV…` symbol in the staged Linux modules (`.dynsym` - the game's own
    C++ symbols are hidden, so only a few tier0 utilities are exported), with the length the
    linker records in `st_size` (st_size/8 - 2, the two header words) and the code-pointer scan's
    count (which under-counts a table whose last slot is a null).
Prints MATCH / MISMATCH / not-symbolized per class and a summary, then the same for every `_ZTV`
symbol the IR does not know. A mismatch is a finding to report, not to repair.
"""
import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import sigcheck  # noqa: E402

IR = sigcheck.ROOT / "crates/s2sdk/s2sdk-sys/ir"


def ir_vtables():
    """{class: {"length": itanium length, "header": upstream header}} from the IR groups."""
    groups = {}
    for f in sorted(IR.glob("*.json")):
        groups[f.stem] = json.loads(f.read_text())
    out = {}

    def find(cls):
        for ir in groups.values():
            for vt in ir.get("vtables") or []:
                if vt["class"] == cls:
                    return vt
        return None

    def length(cls):
        chain, seen = [], set()
        cur = cls
        while cur is not None:
            if cur in seen:
                raise SystemExit("cycle in the primary-base chain at %s" % cur)
            seen.add(cur)
            vt = find(cur)
            if vt is None:
                raise SystemExit("%s is not a vtable in the IR" % cur)
            chain.append(vt)
            cur = vt.get("primary_base")
        n = 0
        for vt in reversed(chain):                     # base first, the way vtable_len walks
            for m in vt["methods"]:
                if m.get("overrides") is None:
                    n += 2 if m.get("is_dtor") else 1
        return n

    for ir in groups.values():
        for vt in ir.get("vtables") or []:
            out[vt["class"]] = {"length": length(vt["class"]), "header": vt.get("header")}
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tree", type=Path, default=sigcheck.ROOT / "target/game-bins-linux-2000930")
    ap.add_argument("--modules", nargs="*", default=sorted(sigcheck.LINUX_MODULE_PATHS))
    a = ap.parse_args()
    want = ir_vtables()
    seen_ztv = {}
    ir_hits = 0
    print("%-32s %6s  %-18s %6s %6s  %s" % ("class (s2sdk IR)", "IR", "module", "symbol", "scan", "verdict"))
    bad = 0
    for cls in sorted(want):
        row = None
        for mod in a.modules:
            path = sigcheck.module_path(mod, root=a.tree, platform="linux")
            if not path.exists():
                continue
            data = path.read_bytes()
            img = sigcheck.parse_elf(data)
            for name, v in img["symbols"].items():
                if not name.startswith("_ZTV"):
                    continue
                scan = "-"
                try:
                    scan = str(sigcheck.vtable_len(data, img, v[0] + 16))
                except (LookupError, ValueError):
                    pass
                sym = v[1] // 8 - 2 if v[1] >= 16 and (v[1] - 16) % 8 == 0 else "-"
                seen_ztv.setdefault(name, (mod, v[0], sym, scan))
                if name.endswith("%d%s" % (len(cls), cls)) or name.endswith("N%d%sE" % (len(cls), cls)):
                    if row is None:
                        row = (mod, name, sym, scan)
                        ir_hits += 1
        ir = want[cls]["length"]
        if row is None:
            print("%-32s %6d  %-18s %6s %6s  not symbolized in the Linux binaries" % (cls, ir, "-", "-", "-"))
            continue
        mod, name, sym, scan = row
        ok = sym == ir
        bad += 0 if ok else 1
        print("%-32s %6d  %-18s %6s %6s  %s" % (cls, ir, mod, sym, scan,
                                                "MATCH" if ok else "MISMATCH (IR %d vs game %s)" % (ir, sym)))
    print()
    print("_ZTV symbols the IR has no class for (non-std:: / non-__cxxabiv1):")
    for name, (mod, addr, sym, scan) in sorted(seen_ztv.items()):
        if "std::" in name or "__cxxabiv1" in name or "__gnu_cxx" in name or name.startswith("_ZTVN10") \
           or name.startswith("_ZTVSt") or name.startswith("_ZTVNSt") or name.startswith("_ZTVN9"):
            continue
        print("  %-46s %-18s %#x  symbol %s  scan %s" % (name, mod, addr, sym, scan))
    print("%d IR classes (%d with an exported vtable), %d _ZTV symbols in the tree, %d mismatches"
          % (len(want), ir_hits, len(seen_ztv), bad))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
