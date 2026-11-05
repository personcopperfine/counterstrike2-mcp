#!/usr/bin/env python3
"""sigupdate.py - resolve the whole signature database against a new game build and judge the results.

    sigupdate.py check  --tree NEW_BINS --build NEW [--old OLD] [--json out.json] [--bridge-gate]
    sigupdate.py record --tree NEW_BINS --build NEW [--old OLD] [--accept-drift NAMES_FILE]

NEW_BINS has the $CS2_BINS layout (NEW_BINS/windows/game/bin/win64/engine2.dll, ...;
sigs/scripts/stagebins.py makes it). OLD defaults to sigs/CURRENT_BUILD, the build of the binaries
under $CS2_BINS. --platform picks the half of the database that is judged (default windows): the
`platform: linux` files resolve against the Linux game build, which has its own ClientVersion and
its own tree (`--tree target/game-bins-linux-<ClientVersion>`, the layout `stagebins.py --linux`
stages, with the same verdicts and the same bridge gate). The Linux binaries the expect[] being
compared was recorded on live in the tree of the OLD build: $CS2_LINUX_BINS if set, else
target/game-bins-linux-<OLD> (pass --old when the Linux build differs from sigs/CURRENT_BUILD).
For every entry:
  SAME   module binary unchanged: value must equal expect[OLD] (and, if the entry has a
         fingerprint, the bytes at that value must match it - this is how the fingerprint files
         are checked against the build they were taken on)
  OK     resolved uniquely and (code entries) the masked fingerprint
         (sigs/fingerprints/<m>-fp.json) matches the bytes at the new address
  DRIFT  resolved uniquely but the fingerprint differs: the function changed or the locator now
         lands elsewhere (vtable slot shift); needs a person with a disassembler before recording
  FAIL   locator did not resolve uniquely, or an unchanged binary whose value moved
  UNVERIFIED  a bridge entry whose code has no fingerprint, so nothing in this run proves the
         locator found the right function (see "the bridge gate" below)
--bridge-gate: also list every entry with a `bridge` macro that is not SAME/OK and exit 3 if there
is one (the bridge must not be rebuilt from such a database).

The bridge gate is fail closed:
  * Every bridge entry that pins *code* needs a fingerprint - a kind=function entry (its own name
    in the fingerprint file) and every identity-checked vtable slot (`<entry>#<slot>`, the
    function the slot must still hold). Without one it is UNVERIFIED, never OK. A vslot locator
    alone proves nothing: it reads vt[N] and accepts any image pointer, so if the game moves the
    slot the value looks fine, pins.py re-pins the wrong target and the runtime check compares
    vt[N] with itself. The fingerprint is what says "vt[N] is still this function".
  * A module that is unchanged AND has no expect[OLD] is only OK for a bridge entry when the
    fingerprints it needs are present and match; an entry added to the database with a bridge
    macro and no fingerprint cannot pass.
  * Bridge entries located by a value pattern (sig/rtti/string/export) carry their own evidence
    in the locator and pin an RVA or an integer, not code bytes; they have no fingerprint and are
    not required to have one. The gate reports the split (BRIDGE COVERAGE) so the difference
    between "checked" and "assumed" stays visible, and counts the required fingerprints the
    fingerprint file itself marks *weak* (its `_weak` key: ida_fp.py could not tell the pattern
    apart from a function the entry could be swapped with, so a swap would still print OK) - those
    names are printed with the count instead of hiding behind "present".
`record` writes expect[NEW] + builds[NEW] for SAME/OK entries (and DRIFT entries listed in
--accept-drift), and marks FAIL / unaccepted DRIFT entries status=stale. It refuses to write
anything when a bridge entry is not SAME/OK, with or without --bridge-gate: pins.py only re-checks
builds[NEW].sha256, so a recorded build with a bad bridge entry would be pinned silently.
--accept-drift cannot accept an UNVERIFIED entry (the fix is to take its fingerprint on the old
build, not to accept it).
"""
import argparse
import json
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import sigcheck  # noqa: E402

MISSING = []   # modules in the database with no binary in --tree


def old_root_linux(old_build):
    """The tree the OLD Linux build is staged in (the .so files the expect[] being compared was
    recorded on): $CS2_LINUX_BINS if set, else the tree named after the OLD build tag - Linux
    trees are named target/game-bins-linux-<ClientVersion> (sigs/README.md "Linux binaries")."""
    return Path(os.environ.get("CS2_LINUX_BINS")
                or sigcheck.ROOT / ("target/game-bins-linux-%s" % old_build))


def judge(tree, old_build, platform="windows"):
    """Resolve every entry of one platform's half of the database against --tree and judge it.
    Windows compares the binary under $CS2_BINS; Linux compares the OLD build's tree
    (old_root_linux), the build tag the Linux files' expect[] is keyed by."""
    db = sigcheck.load_db(None, platform=platform)
    res = {}
    for path, d in db.items():
        mod = d["module"]
        if platform == "linux":
            old = sigcheck.module_path(mod, root=old_root_linux(old_build), platform=platform)
            new = sigcheck.module_path(mod, root=Path(tree), platform=platform)
        else:
            old = sigcheck.module_path(mod)
            new = Path(tree) / old.relative_to(sigcheck.BINS)
        if not new.exists():
            MISSING.append(mod)
            continue
        if not old.exists():
            if platform == "linux":
                sys.exit("%s: the current binary %s is missing; stage the old Linux build %s there "
                         "first (sigs/scripts/stagebins.py --linux --out %s) - the judge compares "
                         "old and new" % (mod, old, old_build, old.parent))
            sys.exit("%s: the current binary %s is missing; stage the current build into $CS2_BINS first "
                     "(sigs/scripts/stagebins.py) - the judge compares old and new" % (mod, old))
        odata, ndata = old.read_bytes(), new.read_bytes()
        img = sigcheck.load_image(ndata)
        r = sigcheck.Resolver(ndata, img)
        fps = sigcheck.load_fps(mod)
        same = odata == ndata
        out = res[str(path)] = {"module": mod, "binary": str(new), "same": same, "entries": {},
                                "fp_required": 0, "fp_missing": 0, "fp_weak": []}
        for name, e in d["entries"].items():
            want = e.get("expect", {}).get(old_build)
            try:
                v = r.resolve(name, e, d["entries"])
            except (LookupError, ValueError, IndexError, AssertionError, KeyError) as ex:
                out["entries"][name] = ("FAIL", None, str(ex))
                continue
            checks = sigcheck.fp_checks(name, e, v, ndata, img, fps)
            lacking = [k for k, _rva, present in checks if not present]
            if e.get("bridge"):
                out["fp_required"] += len(checks)
                out["fp_missing"] += len(lacking)
                # present but weak: the pattern cannot tell the function apart from the one it
                # could be swapped with, so a swap would still print OK (BRIDGE COVERAGE prints
                # the count and the names; sigs/README.md "The bridge gate")
                out["fp_weak"] += [(mod, k) for k in sigcheck.fp_weak([k for k, _rva, _p in checks], fps)]
                if lacking:
                    # a bridge entry that pins code with no fingerprint can never be OK: the
                    # locator resolved something, but nothing here says it is the right function
                    out["entries"][name] = ("UNVERIFIED", v, "no fingerprint for %s" % ", ".join(lacking))
                    continue
            bad = [(k, rva) for k, rva, _present in checks
                   if k in fps and (rva is None or not sigcheck.fp_match(fps[k], ndata, img, rva))]
            if same and want is None and not bad:
                out["entries"][name] = ("OK", v, "unchanged binary, no expect[%s] to compare" % old_build)
            elif same and want is not None and v != want:
                out["entries"][name] = ("FAIL", v, "unchanged binary, value moved")
            elif same and bad:
                out["entries"][name] = ("FAIL", v, "unchanged binary, fingerprint %s does not match the "
                                        "bytes there (stale or wrong fingerprint)" % bad[0][0])
            elif same:
                out["entries"][name] = ("SAME", v, "")
            elif bad:
                where = "the slot is not an image pointer" if bad[0][1] is None else \
                        "fingerprint differs at %#x" % (img["base"] + bad[0][1])
                out["entries"][name] = ("DRIFT", v, "%s: %s" % (bad[0][0], where))
            else:
                out["entries"][name] = ("OK", v, "")
    return db, res


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["check", "record"])
    ap.add_argument("--tree", required=True)
    ap.add_argument("--build", required=True)
    ap.add_argument("--platform", choices=sigcheck.PLATFORMS, default="windows",
                    help="which half of the database to judge (default windows); linux compares "
                         "the OLD build's Linux tree (old_root_linux) with --tree")
    ap.add_argument("--old", default=sigcheck.CURRENT_BUILD,
                    help="build tag of the binaries the expect[] being compared is keyed by "
                         "(default sigs/CURRENT_BUILD; for --platform linux, the build of the old "
                         "Linux tree when it differs)")
    ap.add_argument("--json")
    ap.add_argument("--accept-drift")
    ap.add_argument("--bridge-gate", action="store_true")
    a = ap.parse_args()
    db, res = judge(a.tree, a.old, platform=a.platform)
    tot = {}
    for p, r in sorted(res.items()):
        c = {}
        for st, _v, _why in r["entries"].values():
            c[st] = c.get(st, 0) + 1
            tot[st] = tot.get(st, 0) + 1
        print("%-26s %s %s" % (r["module"], "unchanged" if r["same"] else "CHANGED  ", " ".join("%s=%d" % kv for kv in sorted(c.items()))))
    print("TOTAL", " ".join("%s=%d" % kv for kv in sorted(tot.items())))
    if a.json:
        Path(a.json).write_text(json.dumps(res, indent=1))
    gate = []
    for p, r in sorted(res.items()):
        ents = db[Path(p)]["entries"]
        for name, (st, _v, why) in r["entries"].items():
            e = ents[name]
            if e.get("bridge") and st not in ("SAME", "OK"):
                gate.append("%-5s %-22s %-30s %s  %s  via: %s" % (st, r["module"], e["bridge"], name, why, e.get("via", "?")))
    for mod in MISSING:
        print("MISSING %s: not in --tree, not judged" % mod)
        if any(e.get("bridge") for d in db.values() if d["module"] == mod for e in d["entries"].values()):
            gate.append("MISSING %-22s the new tree has no %s (the bridge pins it)" % (mod, mod))
    if a.bridge_gate:
        nb = sum(1 for p, r in res.items() for n in r["entries"] if db[Path(p)]["entries"][n].get("bridge"))
        req = sum(r["fp_required"] for r in res.values())
        miss = sum(r["fp_missing"] for r in res.values())
        weak = sorted({"%s:%s" % (mod, k) for r in res.values() for mod, k in r["fp_weak"]})
        print("BRIDGE COVERAGE: %d fingerprints required by bridge entries, %d missing, %d weak%s%s" % (
            req, miss, len(weak), (": " + ", ".join(weak)) if weak else "",
            " - every bridge entry that pins code must have one (sigs/README.md)" if miss else ""))
        if weak:
            print("  weak = the pattern still matches the function the entry could be swapped with; a "
                  "slot swap of those is only caught live (sigs/README.md \"The bridge gate\")")
        print("BRIDGE GATE: %d bridge entries judged, %d not SAME/OK" % (nb, len(gate)))
        for line in gate:
            print("  " + line)
        print("BRIDGE GATE " + ("FAIL" if gate else "PASS"))
    if a.cmd == "record" and gate:
        print("not recording: %d bridge entries are not SAME/OK (nothing was written); pins.py would "
              "re-pin them from this database" % len(gate))
        sys.exit(3)
    if a.cmd == "record":
        accept = set()
        if a.accept_drift:
            accept = {l.strip() for l in Path(a.accept_drift).read_text().splitlines() if l.strip() and not l.startswith("#")}
        for p, r in res.items():
            d = db[Path(p)]
            for name, (st, v, _why) in r["entries"].items():
                e = d["entries"][name]
                if st in ("SAME", "OK") or (st == "DRIFT" and name in accept):
                    prev = e.get("expect", {}).get(a.build)
                    if prev is not None and prev != v:
                        print("REFUSE %s %s: expect[%s] already %#x, now %#x" % (r["module"], name, a.build, prev, v))
                        continue
                    e.setdefault("expect", {})[a.build] = v
                    if e.get("status") == "stale":
                        # resolving again is not proof it is the right place; a person re-checks it
                        print("STILL STALE %s %s: resolves again at %#x, left stale" % (r["module"], name, v))
                else:
                    e["status"] = "stale"
            binary = Path(r["binary"])
            data = binary.read_bytes()
            info = {"sha256": sigcheck.sha16(binary)}
            if sigcheck.load_image(data)["kind"] == "pe":     # an ELF .so has no PE timestamp
                info["timestamp"] = sigcheck.parse_pe(data)["ts"]
            d.setdefault("builds", {})[a.build] = info
            Path(p).write_text(json.dumps(d, indent=1, ensure_ascii=False) + "\n")
        print("recorded expect[%s]" % a.build)
    if a.bridge_gate and gate:
        sys.exit(3)


if __name__ == "__main__":
    main()
