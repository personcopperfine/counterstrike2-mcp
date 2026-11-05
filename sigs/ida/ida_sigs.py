# -*- coding: utf-8 -*-
"""ida_sigs.py - generate signature-database entries from inside IDA (IDAPython, IDA 9.x, x64 PE).

Run through ida-mcp `run_script` (the IDB must already be open) or from IDA's script box:

    exec(open('<cs2-mcp>/sigs/ida/ida_sigs.py').read())   # set CS2_MCP_ROOT=<cs2-mcp> first
    print(dump([
        ("CMapDoc::GetUndoHistory", 0x180885BB0, "function", "docs/interfaces/CMapDoc.md"),
        ("g_pHammerApp",            0x18677AC80, "global",   "docs/interfaces/CHammerApp.md"),
        ("CMapDoc",                 0x181764188, "vtable",   "docs/interfaces/CMapDoc.md"),
        ("CMapDoc::m_pHistory",     0x180885BC4, "offset",   "docs/interfaces/CMapDoc.md"),  # ea of the mov that uses the field
    ]))

`dump` prints a JSON object whose keys are entry names and whose values are entries in the
sigs/README.md shape (kind, locator, via, tag, expect; drop the `docs` field). Paste them into
sigs/signatures/<module>.json under "entries" and run `sigs/scripts/sigcheck.py validate` and
`sigs/scripts/sigcheck.py resolve --module <module>.dll`; an entry is done only when sigcheck resolves
it to the same RVA this script reports under expect.

Rules the generator follows (so the output is stable across builds):
  * function  : masked prologue pattern starting at the function entry, extended one instruction
                at a time until it matches exactly once in .text. Relative branch/call targets,
                RIP-relative displacements and image-address immediates are wildcarded (??);
                struct displacements and small immediates are kept.
  * global    : the first code reference that reads the global with a RIP-relative operand gives
                the site; the pattern starts at that instruction and a {"rip", "next"} step
                recovers the global's RVA. If no site pattern is unique, the enclosing function's
                prologue is used as the base with a {"rip"} step at the instruction's offset.
  * vtable    : RTTI locator by class name ({"type":"rtti","class":...}); no bytes involved.
  * offset    : pattern at the given instruction (ea must be the instruction that encodes the
                displacement) plus a {"disp","size"} step reading it back.
  * string    : {"type":"string"} locator; just the C string.

Every entry carries `expect[BUILD]` = the RVA (or value) in this IDB so sigcheck can regression
check the derivation. BUILD defaults to sigs/CURRENT_BUILD (the build of the binaries under $CS2_BINS); pass build="..." for another database.
"""
import json
import re

import ida_bytes
import ida_funcs
import ida_ida
import ida_name
import ida_nalt
import ida_segment
import ida_ua
import idautils

IMAGE_BASE = ida_nalt.get_imagebase()


def _current_build():
    """sigs/CURRENT_BUILD of the cs2-mcp checkout: $CS2_MCP_ROOT, else derived from this file."""
    import os
    root = os.environ.get("CS2_MCP_ROOT")
    if not root and "__file__" in globals():
        root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    if not root:
        raise RuntimeError("set CS2_MCP_ROOT to the cs2-mcp checkout, or pass build=...")
    with open(os.path.join(root, "sigs", "CURRENT_BUILD")) as f:
        return f.read().strip()
MIN_LEN = 8          # bytes
MAX_INSNS = 40       # give up on a function prologue after this many instructions

_TEXT = None         # (start_ea, bytes)


def _text():
    global _TEXT
    if _TEXT is None:
        seg = ida_segment.get_segm_by_name(".text")
        if seg is None:
            raise RuntimeError("no .text segment")
        _TEXT = (seg.start_ea, ida_bytes.get_bytes(seg.start_ea, seg.end_ea - seg.start_ea))
    return _TEXT


def _image_size():
    return ida_ida.inf_get_max_ea() - IMAGE_BASE


def _in_image(v):
    return IMAGE_BASE <= v < IMAGE_BASE + _image_size()


def _insn_mask(insn):
    """bytes, mask for one instruction; mask[i]=False means wildcard"""
    raw = ida_bytes.get_bytes(insn.ea, insn.size)
    mask = [True] * insn.size

    def wild(off, n):
        for i in range(off, min(off + n, insn.size)):
            mask[i] = False

    for op in insn.ops:
        if op.type == ida_ua.o_void:
            break
        if op.type in (ida_ua.o_near, ida_ua.o_far):
            wild(op.offb, insn.size - op.offb)            # rel8/rel32 branch or call target
        elif op.type == ida_ua.o_mem:
            wild(op.offb, 4)                              # RIP-relative disp32 (x64 absolute data ref)
        elif op.type == ida_ua.o_imm and _in_image(op.value):
            wild(op.offb, ida_ua.get_dtype_size(op.dtype))  # address constant
        # o_displ (struct field displacements) and small immediates are kept on purpose;
        # a call/jmp through [rip+x] is o_mem and already handled
    return raw, mask


def _fmt(raw, mask):
    return " ".join("%02X" % b if m else "??" for b, m in zip(raw, mask))


def _regex(raw, mask):
    # lookahead so finditer counts overlapping occurrences (self-repeating patterns)
    return re.compile(b"(?=" + b"".join(re.escape(bytes([b])) if m else b"." for b, m in zip(raw, mask)) + b")", re.S)


def _count(raw, mask, limit=2):
    start, blob = _text()
    rx = _regex(raw, mask)
    n = 0
    for _ in rx.finditer(blob):
        n += 1
        if n >= limit:
            break
    return n


def _grow_unique(ea, max_insns=MAX_INSNS, stop_at_func_end=True):
    """pattern starting at ea, extended by whole instructions until unique in .text"""
    f = ida_funcs.get_func(ea)
    raw = b""
    mask = []
    cur = ea
    for _ in range(max_insns):
        if stop_at_func_end and f is not None and cur >= f.end_ea:
            break
        insn = ida_ua.insn_t()
        n = ida_ua.decode_insn(insn, cur)
        if n <= 0:
            break
        r, m = _insn_mask(insn)
        raw += r
        mask += m
        cur += n
        if len(raw) >= MIN_LEN and any(mask) and _count(raw, mask) == 1:
            return _fmt(raw, mask), len(raw)
    return None, len(raw)


def _func_name(ea):
    return ida_name.get_name(ea) or "sub_%X" % ea


def _via(ea, what):
    return "%s!0x%X (%s)" % (_module_short(), ea, what)


def _module_short():
    return ida_nalt.get_root_filename().replace(".dll", "")


def _entry(kind, locator, via, expect, build, docs, tag="V"):
    e = {"kind": kind, "locator": locator, "via": via, "tag": tag, "expect": {build: expect}}
    if docs:
        e["docs"] = [docs] if isinstance(docs, str) else list(docs)
    return e


def sig_function(ea, build, docs=None):
    f = ida_funcs.get_func(ea)
    if f is None or f.start_ea != ea:
        raise ValueError("0x%X is not a function start" % ea)
    sig, n = _grow_unique(ea)
    if sig is None:
        raise ValueError("0x%X: no unique prologue within %d bytes" % (ea, n))
    return _entry("function", {"type": "sig", "sig": sig}, _via(ea, _func_name(ea)), ea - IMAGE_BASE, build, docs)


def sig_global(ea, build, docs=None):
    """find a RIP-relative code reference to the global and anchor on it"""
    tried = 0
    for x in idautils.XrefsTo(ea):
        if not ida_funcs.get_func(x.frm):
            continue
        insn = ida_ua.insn_t()
        if ida_ua.decode_insn(insn, x.frm) <= 0:
            continue
        rip_off = None
        for op in insn.ops:
            if op.type == ida_ua.o_void:
                break
            if op.type in (ida_ua.o_mem,) and op.addr == ea:
                rip_off = op.offb
        if rip_off is None:
            continue
        tried += 1
        sig, n = _grow_unique(x.frm)
        if sig is not None:
            loc = {"type": "sig", "sig": sig, "steps": [{"rip": rip_off, "next": insn.size}]}
            return _entry("global", loc, _via(x.frm, "%s reads %s" % (_func_name(ida_funcs.get_func(x.frm).start_ea), _func_name(ea))),
                          ea - IMAGE_BASE, build, docs)
        if tried >= 8:
            break
    raise ValueError("0x%X: no unique RIP-relative reference site found (%d tried)" % (ea, tried))


def sig_vtable(ea, build, docs=None, cls=None):
    """ea = first slot of the vtable (IDA name ??_7Class@@6B@) or pass cls explicitly"""
    if cls is None:
        nm = ida_name.get_name(ea) or ""
        if re.search(r"_\d+$", nm) or re.search(r"@@6B@\d+_", nm):
            raise ValueError("0x%X: %s looks like a secondary/adjustor vtable (name suffix); the rtti locator needs col_offset - set it by hand" % (ea, nm))
        m = re.match(r"\?\?_7(.+?)@@6B(?:@|(.+?)@@)", nm)
        if not m:
            raise ValueError("0x%X: not an MSVC vtable symbol (%r); pass cls=" % (ea, nm))
        if m.group(2):
            raise ValueError("0x%X: %s is a secondary vtable (for base %s); the rtti locator needs col_offset - set it by hand" % (ea, nm, m.group(2)))
        cls = m.group(1)
    return _entry("vtable", {"type": "rtti", "class": cls}, _via(ea, "??_7%s@@6B@" % cls), ea - IMAGE_BASE, build, docs, tag="RTTI")


def sig_vslot(vtable_entry_name, slot, target_ea, build, docs=None):
    return _entry("function", {"type": "vslot", "vtable": vtable_entry_name, "slot": slot},
                  _via(target_ea, "%s slot %d" % (vtable_entry_name, slot)), target_ea - IMAGE_BASE, build, docs)


def sig_offset(ea, build, docs=None, operand=None):
    """ea = instruction whose displacement is the field offset; returns kind=offset entry"""
    insn = ida_ua.insn_t()
    if ida_ua.decode_insn(insn, ea) <= 0:
        raise ValueError("0x%X: cannot decode" % ea)
    cands = []
    for i, op in enumerate(insn.ops):
        if op.type == ida_ua.o_void:
            break
        if op.type == ida_ua.o_displ and (operand is None or operand == i):
            size = insn.size - op.offb
            if op.offo:  # an immediate follows the displacement
                size = op.offo - op.offb
            size = 1 if size == 1 else 4
            cands.append((op.offb, size, op.addr))
    if not cands:
        raise ValueError("0x%X: no displacement operand" % ea)
    offb, size, value = cands[0]
    sig, n = _grow_unique(ea, stop_at_func_end=False)
    if sig is None:
        raise ValueError("0x%X: no unique pattern at the site" % ea)
    value &= (1 << (8 * size)) - 1                      # op.addr is 64-bit; keep the encoded field
    val = value if value < 2 ** (8 * size - 1) else value - 2 ** (8 * size)
    f = ida_funcs.get_func(ea)
    where = _func_name(f.start_ea) if f else "?"
    return _entry("offset", {"type": "sig", "sig": sig, "steps": [{"disp": offb, "size": size}]},
                  _via(ea, "%s: %s" % (where, _disasm(ea))), val, build, docs)


def _disasm(ea):
    import idc
    return idc.generate_disasm_line(ea, 0)


def sig_string(ea, build, docs=None):
    s = ida_bytes.get_strlit_contents(ea, -1, ida_nalt.STRTYPE_C)
    if not s:
        raise ValueError("0x%X: no C string" % ea)
    return _entry("string", {"type": "string", "string": s.decode("utf-8", "replace")}, _via(ea, "string"), ea - IMAGE_BASE, build, docs)


def sig_export(ea, build, docs=None, name=None):
    """exported function: {type: export, name}; expect is this IDB's RVA (resolver re-reads the export table)"""
    if name is None:
        name = ida_name.get_name(ea) or ""
        if not name:
            raise ValueError("0x%X: no name for export" % ea)
    return _entry("function", {"type": "export", "name": name}, _via(ea, "export %s" % name), ea - IMAGE_BASE, build, docs)


def sig_site(ea, build, docs=None):
    """an address that is not a function start (an instruction inside one, or a jump thunk body);
    kind=function so coverage/sigcheck accept it, locator is a unique pattern at the site itself"""
    sig, n = _grow_unique(ea, stop_at_func_end=False)
    if sig is None:
        raise ValueError("0x%X: no unique pattern at the site within %d bytes" % (ea, n))
    return _entry("function", {"type": "sig", "sig": sig}, _via(ea, _disasm(ea)), ea - IMAGE_BASE, build, docs)


def _rtti_name(ea):
    nm = ida_name.get_name(ea) or ""
    if nm.startswith("??_7"):
        return nm
    if nm.startswith(("??_R0", "??_R1", "??_R2", "??_R3", "??_R4")):
        raise ValueError("RTTI plumbing (%s): derive from the class vtable, do not signature" % nm)
    if nm.startswith("??_C@") or nm.startswith("??_8"):
        raise ValueError("RTTI string/other data (%s): use a string locator" % nm)
    return None


def _export_name(ea):
    """PE export name at ea, or None (so auto_classify prefers an `export` locator)"""
    try:
        import ida_entry
        for i in range(ida_entry.get_entry_qty()):
            o = ida_entry.get_entry_ordinal(i)
            if ida_entry.get_entry(o) == ea:
                return ida_entry.get_entry_name(o) or None
    except Exception:  # noqa: BLE001 - older/newer IDA API differences: fall back to name
        pass
    return None


def auto_classify(ea):
    """read the IDB to decide the locator kind at ea (never guesses): export/vtable/function/global/site"""
    if _export_name(ea):
        return "export"
    nm = ida_name.get_name(ea) or ""
    if nm.startswith("??_7"):
        return "vtable"
    f = ida_funcs.get_func(ea)
    if f is not None and f.start_ea == ea:
        return "function"
    _rtti_name(ea)                      # raises for RTTI plumbing we must not pattern
    seg = ida_segment.getseg(ea)
    if seg is not None and seg.type != ida_segment.SEG_CODE:
        return "tdata"
    return "site"


def _auto_entry(name, ea, build, docs):
    kind = auto_classify(ea)
    if kind == "export":
        return sig_export(ea, build, docs, name=_export_name(ea))
    if kind == "vtable":
        return sig_vtable(ea, build, docs)
    if kind == "function":
        return sig_function(ea, build, docs)
    if kind == "tdata":
        try:
            return sig_global(ea, build, docs)
        except ValueError:
            raise ValueError("0x%X: data with no unique RIP-relative reader; use a base+rip chain by hand" % ea)
    return sig_site(ea, build, docs)


def _unique_name(out, name, ea):
    name = re.sub(r"[^A-Za-z0-9_:<>,~ *&]", "_", name) or "sub_%X" % ea
    if not re.match(r"^[A-Za-z_]", name):
        name = "x_" + name
    if name not in out:
        return name
    return "%s_%X" % (name.strip("_") or "sub", ea)


def dump_auto(items, build=None):
    """items: [(name, ea, kind_hint, docs_or_None), ...] -> JSON {name: entry}; kind is read from
    the IDB (auto_classify) so the doc hint is a fallback label only; failures go to _errors."""
    build = build or _current_build()
    out, errors = {}, {}
    for it in items:
        hint, ea = it[0], it[1]
        docs = it[3] if len(it) > 3 else None
        name = _unique_name(out, hint, ea)
        try:
            out[name] = _auto_entry(name, ea, build, docs)
        except Exception as ex:  # noqa: BLE001 - report, don't abort the batch
            errors[name] = "0x%X %s: %s" % (ea, it[2] if len(it) > 2 else "?", ex)
    if errors:
        out["_errors"] = errors
    return json.dumps(out, indent=1)



def dump(items, build=None):
    """items: [(name, ea, kind, docs_or_None), ...] -> JSON text of {name: entry}; failures go to _errors"""
    build = build or _current_build()
    out, errors = {}, {}
    for it in items:
        name, ea, kind = it[0], it[1], it[2]
        docs = it[3] if len(it) > 3 else None
        try:
            if kind == "function":
                out[name] = sig_function(ea, build, docs)
            elif kind == "global":
                out[name] = sig_global(ea, build, docs)
            elif kind == "vtable":
                out[name] = sig_vtable(ea, build, docs)
            elif kind == "offset":
                out[name] = sig_offset(ea, build, docs)
            elif kind == "string":
                out[name] = sig_string(ea, build, docs)
            else:
                raise ValueError("unknown kind %r" % kind)
        except Exception as ex:  # noqa: BLE001 - report, don't abort the batch
            errors[name] = "0x%X %s: %s" % (ea, kind, ex)
    if errors:
        out["_errors"] = errors
    return json.dumps(out, indent=1)
