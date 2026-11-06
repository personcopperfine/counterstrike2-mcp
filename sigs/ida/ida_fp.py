# -*- coding: utf-8 -*-
"""ida_fp.py - masked code fingerprints for every code entry of a sigs/signatures/<module>.json (headless IDA).

    FP_DB=sigs/signatures/<m>.json FP_OUT=sigs/fingerprints/<m>-fp.json FP_BUILD=<build> \
        idat -A -S sigs/ida/ida_fp.py <scratch copy of the module's .i64>

Writes {name: "48 8B ?? ..."} - the first instructions (>= 24 bytes) at each entry's expect[FP_BUILD]
address, masked like ida_sigs (rel32 / RIP-relative / image immediates wildcarded). sigupdate.py compares
these with the bytes at the address a locator resolves to on a new build, so a locator that still
matches uniquely but lands on a different function (vtable slot shift, ICF change) gets flagged.

Two details matter for the bridge gate (sigs/scripts/sigupdate.py --bridge-gate):

* Every entry carrying a `bridge` macro whose target is code gets a fingerprint that is long enough
  to tell it apart from the functions it could silently be confused with - the other slots of the
  same vtable for a `vslot` entry, the other functions in the module otherwise. Without that, two
  neighbouring slots holding functions with the same prologue (a real case: CSchemaSystemTypeScope
  slots 25 and 27 are byte-identical for 201 bytes) would still compare equal after a slot shift.
  The pattern is extended a whole instruction at a time until it no longer matches any rival, and
  never past a terminal instruction or FP_CAP bytes; an entry that is still not unique there is
  reported on stdout as weak and listed under the file's "_weak" key (sigupdate.py never looks
  that key up). A weak entry is one whose fingerprint cannot tell a swap apart - in practice a
  tail-call thunk or a twin function, where the only difference inside the pattern is a call or
  jump target that has to stay masked so the pattern survives a rebuild. Entries without a `bridge`
  macro keep the plain 24-byte prologue.
* A kind=vtable entry with `ident_slot` gets a second fingerprint under "<name>#<slot>": the
  function the slot must still hold. `cargo bridge pins` emits the identity row by reading that slot
  from the same binary, so that check is self-confirming on its own.
"""
import json
import os

import ida_auto
import ida_bytes
import ida_loader
import ida_idp
import ida_pro
import ida_segment
import ida_ua

exec(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "ida_sigs.py")).read())  # noqa: S102

WANT = 24            # bytes: the plain prologue, enough to spot a changed function
CAP = 256            # bytes: never look further than this when telling functions apart
INSNS = 12           # instructions for the plain prologue
EXT_INSNS = 96       # instructions allowed while telling functions apart


def _decode(ea):
    """(raw, mask, size, termininating) for the instruction at ea, or None"""
    insn = ida_ua.insn_t()
    n = ida_ua.decode_insn(insn, ea)
    if n <= 0:
        return None
    raw, mask = _insn_mask(insn)
    term = ida_idp.is_ret_insn(insn) or ida_bytes.get_byte(insn.ea) in (0xE9, 0xEB, 0xCC)
    return raw, mask, n, term


def _collides(raw, mask, rivals):
    """does this masked pattern still match one of the rival functions' bytes?"""
    for blob in rivals:
        if len(blob) >= len(raw) and all((not m) or raw[i] == blob[i] for i, m in enumerate(mask)):
            return True
    return False


def fingerprint(ea, want=WANT, rivals=(), cap=CAP, max_insns=None):
    """(raw, mask) for the masked instruction bytes at ea.

    Without rivals: the prologue, stopping after `want` bytes (or at a terminal instruction).
    With rivals (the bytes at the other functions this one could be mistaken for): extended one
    instruction at a time until the pattern no longer matches any of them, so that a locator
    landing on a rival is a fingerprint mismatch. Stops at a terminal instruction or `cap` bytes;
    the caller reports the entry as weak when it still matches a rival there.
    """
    limit = max_insns or (EXT_INSNS if rivals else INSNS)
    raw, mask, cur, n = b"", [], ea, 0
    while n < limit:
        d = _decode(cur)
        if d is None:
            break
        r, m, size, term = d
        raw += r
        mask += m
        cur += size
        n += 1
        if term or len(raw) >= cap or (len(raw) >= want and not _collides(raw, mask, rivals)):
            break
    return raw, mask


def _bytes_at(rva, n=CAP):
    return ida_bytes.get_bytes(IMAGE_BASE + rva, n) or b""


def _is_code(ea):
    seg = ida_segment.getseg(ea)
    return seg is not None and bool(seg.perm & ida_segment.SEGPERM_EXEC)


def _vtable_slots(vt_rva, limit=256):
    """[rva, ...] of the slots of the vtable at vt_rva (stops where the entries stop being code)"""
    out = []
    for i in range(limit):
        q = ida_bytes.get_qword(IMAGE_BASE + vt_rva + 8 * i)
        if not _is_code(q):
            break
        out.append(q - IMAGE_BASE)
    return out


def _rivals(ents, build, e, mine):
    """the functions this entry could silently be swapped with, as masked-compare byte blobs"""
    loc = e.get("locator") or {}
    if loc.get("type") == "vslot":
        vt = ents.get(loc.get("vtable"), {}).get("expect", {}).get(build)
        if vt is None:
            return []
        slots = _vtable_slots(vt)
        return [_bytes_at(s) for i, s in enumerate(slots) if i != loc["slot"] and s != mine]
    return [_bytes_at(v) for v in sorted(set(v for v in _func_rvas(ents, build) if v != mine))]


def _func_rvas(ents, build):
    for e in ents.values():
        if e["kind"] == "function" and build in e.get("expect", {}):
            yield e["expect"][build]


def main():
    ida_auto.auto_wait()
    build = os.environ.get("FP_BUILD") or _current_build()
    d = json.load(open(os.environ["FP_DB"]))
    ents = d["entries"]
    out, weak, wide = {}, [], []
    for name, e in ents.items():
        if build not in e.get("expect", {}):
            continue
        if e["kind"] == "function":
            mine = e["expect"][build]
            ea = IMAGE_BASE + mine
            rivals = _rivals(ents, build, e, mine) if e.get("bridge") else []
            prow, pmask = fingerprint(ea)
            raw, mask = fingerprint(ea, rivals=rivals) if rivals else (prow, pmask)
            if rivals and _collides(raw, mask, rivals):
                weak.append(name)
            if raw and ida_bytes.is_code(ida_bytes.get_flags(ea)):
                out[name] = _fmt(raw, mask)
                if len(raw) > len(prow):
                    wide.append("%s(%dB, prologue %dB)" % (name, len(raw), len(prow)))
        elif e.get("bridge") and "ident_slot" in e:
            vt = e["expect"][build]
            slots = _vtable_slots(vt)
            if e["ident_slot"] >= len(slots):
                print("ida_fp: %s: ident_slot %d is outside the %d slots of the vtable"
                      % (name, e["ident_slot"], len(slots)))
                continue
            t = slots[e["ident_slot"]]
            rivals = [_bytes_at(s) for s in slots if s != t]
            prow, _pmask = fingerprint(IMAGE_BASE + t)
            raw, mask = fingerprint(IMAGE_BASE + t, rivals=rivals)
            if _collides(raw, mask, rivals):
                weak.append("%s#%d" % (name, e["ident_slot"]))
            out["%s#%d" % (name, e["ident_slot"])] = _fmt(raw, mask)
            if len(raw) > len(prow):
                wide.append("%s#%d (%dB, prologue %dB)" % (name, e["ident_slot"], len(raw), len(prow)))
    out["_weak"] = sorted(weak)
    json.dump(out, open(os.environ["FP_OUT"], "w"), indent=0)
    print("ida_fp: %d fingerprints -> %s (build %s; %d entries, %d identity slots)"
          % (len(out) - 1, os.environ["FP_OUT"], build, sum(1 for k in out if k in ents),
             sum(1 for k in out if "#" in k)))
    print("ida_fp: extended to tell a rival function apart: %s"
          % (", ".join(wide) if wide else "none"))
    print("ida_fp: WEAK (still matches a rival function within %dB): %s"
          % (CAP, ", ".join(weak) if weak else "none"))


try:
    main()
finally:
    ida_loader.set_database_flag(ida_loader.DBFL_KILL)
    ida_pro.qexit(0)
