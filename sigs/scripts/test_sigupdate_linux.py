#!/usr/bin/env python3
"""test_sigupdate_linux.py - the Linux judge of sigupdate.py reports drift and the bridge gate fails closed.

    python3 -I sigs/scripts/test_sigupdate_linux.py

Needs the staged Linux tree (sigs/README.md "Linux binaries"): $CS2_LINUX_BINS, else
target/game-bins-linux-2000930. Everything it writes goes under target/scratch/ (never /tmp, never
git). Three runs of sigupdate.py check --platform linux against a scratch copy of sigs/:

  control  unedited expect[], unmodified .so         -> every entry SAME, exit 0 (with and without
                                                        --bridge-gate, bridge macro present)
  drift    expect[2000930] edited to a wrong value
           and one byte of the .so changed at the
           entry's address                           -> the entry DRIFTs (its fingerprint - the bytes
                                                        recorded from the *old* binary - no longer
                                                        match), and with --bridge-gate the bridge
                                                        gate FAILs with exit 3

The probed entry (libclient.json CClientInput, the IVEngineClient... vtable) is a kind=vtable entry;
the scratch copy makes it kind=function because that is what a bridge-pinned entry pins (fp_keys
fingerprints a kind=function entry's target, sigs/README.md "The bridge gate") - without a
fingerprint a changed Linux binary can only come out OK, never DRIFT. libclient.json has no
kind=function entry yet, so the fixture promotes it; the fingerprint is the real bytes at the real
address (the raw .tdata bytes of the vtable - fp_match reads them raw), not invented ones.
"""
import json
import os
import shutil
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import sigcheck  # noqa: E402

ENTRY = "CClientInput"
FP_BYTES = 16                      # fingerprint length at the entry's address
SCRATCH = sigcheck.ROOT / "target/scratch/test_sigupdate_linux"


def linux_tree():
    """The staged Linux tree: $CS2_LINUX_BINS, else the default 2000930 tree (sigs/README.md)."""
    return Path(os.environ.get("CS2_LINUX_BINS")
                or sigcheck.ROOT / "target/game-bins-linux-2000930")


def run_sigupdate(sigs_dir, tree, *extra):
    env = dict(os.environ, CS2_LINUX_BINS=str(linux_tree()))
    cmd = [sys.executable, "-I", str(sigs_dir / "scripts/sigupdate.py"),
           "check", "--platform", "linux", "--tree", str(tree), "--build", "2000930", *extra]
    return subprocess.run(cmd, capture_output=True, text=True, env=env, cwd=str(sigs_dir))


def scratch_sigs(name):
    """A copy of sigs/ (scripts, signatures, fingerprints, CURRENT_BUILD) under target/scratch/:
    the scripts' own sigcheck resolves SIGS relative to __file__, so a copied tree is a separate
    database, edited without touching the real one."""
    dst = SCRATCH / name
    if dst.exists():
        shutil.rmtree(dst)
    shutil.copytree(sigcheck.SIGS, dst / "sigs",
                    ignore=shutil.ignore_patterns("ida", "*.md"))
    return dst / "sigs"


def patch_libclient(libclient, edits):
    d = json.loads(libclient.read_text())
    edits(d)
    libclient.write_text(json.dumps(d, indent=1, ensure_ascii=False) + "\n")


def build_fixture(name, expect_value):
    """Scratch sigs + a patched libclient.so copy: fingerprint file from the *real* bytes at the
    entry's address (nothing invented), entry promoted to kind=function with a bridge macro, and
    expect[2000930] set to `expect_value` (the real one for the control, a wrong one for drift).
    Returns (sigs dir, patched tree)."""
    tree = linux_tree()
    so = sigcheck.module_path("libclient.so", root=tree, platform="linux")
    data = bytearray(so.read_bytes())
    img = sigcheck.parse_elf(bytes(data))

    sigs = scratch_sigs(name)
    libclient = sigs / "signatures/libclient.json"
    d = json.loads(libclient.read_text())
    rva = d["entries"][ENTRY]["expect"]["2000930"]

    # the fingerprint is the real bytes at the real address of the unmodified binary
    off = sigcheck.rva_to_off(img, rva)
    pat = " ".join("%02X" % b for b in data[off:off + FP_BYTES])
    (sigs / "fingerprints/libclient-fp.json").write_text(
        json.dumps({"_doc": ["test fixture: the bytes at %s's address on the unmodified tree" % ENTRY],
                    ENTRY: pat}, indent=1) + "\n")

    def edits(d):
        e = d["entries"][ENTRY]
        e["kind"] = "function"          # what a bridge entry pins code with (fp_keys)
        e["bridge"] = "S2_TEST_BRIDGE_GATE"
        e.setdefault("notes", "")
        e["notes"] = "test fixture: promoted to kind=function with a bridge macro for the gate"
        e.setdefault("expect", {})["2000930"] = expect_value(rva)
    patch_libclient(libclient, edits)

    # the new build's binary: one byte at the entry's address changed (in a file-backed section -
    # a .bss address has no file bytes, and the test refuses to build on one)
    for sname, va, vsize, rptr, rsize in img["sections"]:
        if va <= rva < va + max(vsize, rsize):
            assert rsize > 0 and not (rptr == 0 and rva != va), \
                "%s's address %#x is not file-backed (%s); the fixture needs real bytes" % (ENTRY, rva, sname)
            break
    else:
        raise AssertionError("%s's address %#x is not in any section" % (ENTRY, rva))
    data[off] = data[off] ^ 0xFF if data[off] != 0xFF else 0xAA
    new_tree = SCRATCH / name / "tree"
    new_so = sigcheck.module_path("libclient.so", root=new_tree, platform="linux")
    new_so.parent.mkdir(parents=True, exist_ok=True)
    new_so.write_bytes(bytes(data))
    return sigs, new_tree


class TestSigupdateLinux(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tree = linux_tree()
        if not cls.tree.exists():
            raise AssertionError("no staged Linux tree at %s - stage it first "
                                 "(sigs/scripts/stagebins.py --linux, sigs/README.md)" % cls.tree)
        # control: real expect, unmodified binary -> SAME
        cls.control_sigs, _ = build_fixture("control", lambda rva: rva)
        # drift: expect[2000930] edited to a wrong value, one byte of the .so changed
        cls.drift_sigs, cls.drift_tree = build_fixture("drift", lambda rva: rva + 0x10)

    def test_control_unmodified_reports_same_exit_0(self):
        """Unmodified tree, expect intact: every entry SAME, exit 0 - with and without --bridge-gate."""
        for extra in ([], ["--bridge-gate"]):
            p = run_sigupdate(self.control_sigs, self.tree, *extra)
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
            self.assertIn("libclient.so               unchanged SAME=11", p.stdout)
            self.assertIn("TOTAL SAME=41", p.stdout)
            if extra:
                self.assertIn("BRIDGE GATE PASS", p.stdout)

    def test_drift_reported(self):
        """Edited expect + a changed byte at the entry's address: the entry is DRIFT (the fingerprint
        recorded from the old binary no longer matches), not OK, not FAIL."""
        p = run_sigupdate(self.drift_sigs, self.drift_tree)
        self.assertIn("DRIFT=1", p.stdout, p.stdout)
        self.assertNotIn("FAIL=1", p.stdout, p.stdout)
        self.assertIn("libclient.so", p.stdout)

    def test_drift_bridge_gate_fails_closed_exit_3(self):
        """Same drift with --bridge-gate: the bridge entry is listed, the gate FAILs, exit 3."""
        p = run_sigupdate(self.drift_sigs, self.drift_tree, "--bridge-gate")
        self.assertEqual(p.returncode, 3, p.stdout + p.stderr)
        self.assertIn("BRIDGE GATE FAIL", p.stdout)
        self.assertIn(ENTRY, p.stdout)
        self.assertIn("S2_TEST_BRIDGE_GATE", p.stdout)


if __name__ == "__main__":
    unittest.main()
