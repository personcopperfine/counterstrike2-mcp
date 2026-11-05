#!/usr/bin/env python3
"""sigcheck.py - validate and resolve the signature database in sigs/signatures/.

The database replaces raw RVAs with build-independent locators (masked byte patterns, RTTI class
names, vtable slots, exports, string references). Format: sigs/README.md and
sigs/signatures/SCHEMA.json.

Commands
  validate   [files]                structure check (no binaries needed); also requires that every
                                    bridge entry which pins code has a fingerprint in
                                    sigs/fingerprints/<module>-fp.json (the bridge gate's rule)
  resolve    [files] [--module M] [--binary PATH] [--platform P] [--tree DIR] [--record BUILD]
             [--strict] [-v]
                                    resolve every entry against the binary under $CS2_BINS; compare
                                    with expect[<build of that binary>] when present; --record stores
                                    the result under expect[BUILD] and refuses a file with bridge
                                    entries, whose build only `sigupdate.py record` (bridge gate)
                                    may write (`cargo bridge pins` re-checks builds[BUILD].sha256
                                    only, so recording here would be a side door around that gate)
  selftest                          check the vtlen scan against known vtable lengths in $CS2_BINS
                                    (CSchemaSystem 40, CSchemaSystemTypeScope 35)

Binaries: $CS2_BINS (default <cs2-mcp>/target/game-bins), laid out like the game install
(windows/game/bin/win64/engine2.dll, windows/game/csgo/bin/win64/client.dll, ...). They are Valve's
files: never commit them. sigs/scripts/stagebins.py fills the directory from a game install or a
depot download.

Platforms: entries are Windows (PE, MSVC RTTI, image base 0x180000000) unless their file says
otherwise. A file whose `module` ends in `.so` is `platform: linux`: its entries resolve against
the ELF64 module in the same layout under a Linux tree (`linux/game/csgo/bin/linuxsteamrt64/
libclient.so`), with image base 0, `itanium_vtable` (`_ZTV…` symbol) instead of MSVC `rtti`, and
the Linux build tag (`steam.inf` ClientVersion of that install, e.g. 2000908) as the `builds`/
`expect` key. `resolve --platform linux --tree target/game-bins-linux-2000930` judges that half; the
Windows bridge gate only covers the Windows entries.

Exit status is non-zero when any entry fails to resolve uniquely (resolve) or any file fails
validation (validate). Nothing here writes to a binary.
"""
import argparse
import hashlib
import json
import os
import re
import struct
import sys
from pathlib import Path

SIGS = Path(__file__).resolve().parents[1]          # cs2-mcp/sigs
ROOT = SIGS.parent                                   # cs2-mcp
SIGDIR = SIGS / "signatures"
BINS = Path(os.environ.get("CS2_BINS", ROOT / "target/game-bins")).resolve()
IMAGE_BASE = 0x180000000
# CS2 ClientVersion (steam.inf) of the binaries under $CS2_BINS; the game-update workflow bumps it
# (workflows/game-update.md). `cargo bridge pins` emits it as PIN_GAME_BUILD.
CURRENT_BUILD = (SIGS / "CURRENT_BUILD").read_text().strip()

# where each module lives under $CS2_BINS (same layout as the game install, under windows/)
MODULE_PATHS = {
    "hammer.dll": "windows/game/bin/win64/tools/hammer.dll",
    "modeldoc_editor.dll": "windows/game/bin/win64/tools/modeldoc_editor.dll",
    "pet.dll": "windows/game/bin/win64/tools/pet.dll",
    "toolframework2.dll": "windows/game/bin/win64/toolframework2.dll",
    "scenesystem.dll": "windows/game/bin/win64/scenesystem.dll",
    "engine2.dll": "windows/game/bin/win64/engine2.dll",
    "rendersystemvulkan.dll": "windows/game/bin/win64/rendersystemvulkan.dll",
    "client.dll": "windows/game/csgo/bin/win64/client.dll",
    "server.dll": "windows/game/csgo/bin/win64/server.dll",
    "tier0.dll": "windows/game/bin/win64/tier0.dll",
    "schemasystem.dll": "windows/game/bin/win64/schemasystem.dll",
    "soundsystem.dll": "windows/game/bin/win64/soundsystem.dll",
}

# the Linux analogues (Linux entry files, `platform: linux`): a *different* game build with its own
# ClientVersion, staged under a `linux/` tree (sigs/README.md "Linux binaries")
LINUX_MODULE_PATHS = {
    "libclient.so": "linux/game/csgo/bin/linuxsteamrt64/libclient.so",
    "libserver.so": "linux/game/csgo/bin/linuxsteamrt64/libserver.so",
    "libengine2.so": "linux/game/bin/linuxsteamrt64/libengine2.so",
    "libschemasystem.so": "linux/game/bin/linuxsteamrt64/libschemasystem.so",
    "libsoundsystem.so": "linux/game/bin/linuxsteamrt64/libsoundsystem.so",
    "libtier0.so": "linux/game/bin/linuxsteamrt64/libtier0.so",
    "libscenesystem.so": "linux/game/bin/linuxsteamrt64/libscenesystem.so",
    "librendersystemvulkan.so": "linux/game/bin/linuxsteamrt64/librendersystemvulkan.so",
    "libparticles.so": "linux/game/bin/linuxsteamrt64/libparticles.so",
}

PLATFORMS = ("windows", "linux")


def module_path(mod, root=None, platform="windows"):
    root = Path(root) if root else BINS
    if platform == "linux":
        if mod in LINUX_MODULE_PATHS:
            return root / LINUX_MODULE_PATHS[mod]
        p = root / "linux/game/bin/linuxsteamrt64" / mod
        if p.exists():
            return p
        q = root / "linux/game/csgo/bin/linuxsteamrt64" / mod
        return q if q.exists() else p
    if mod in MODULE_PATHS:
        return root / MODULE_PATHS[mod]
    p = root / "windows/game/bin/win64" / mod
    if p.exists():
        return p
    for sub in ("windows/game/bin/win64/tools", "windows/game/csgo/bin/win64"):
        q = root / sub / mod
        if q.exists():
            return q
    return p


# ------------------------------------------------------------------ PE helpers
def parse_pe(data):
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    assert data[pe:pe + 4] == b"PE\0\0", "not a PE"
    nsec = struct.unpack_from("<H", data, pe + 6)[0]
    ts = struct.unpack_from("<I", data, pe + 8)[0]
    opt = pe + 24
    assert struct.unpack_from("<H", data, opt)[0] == 0x20B, "not PE32+"
    size_of_image = struct.unpack_from("<I", data, opt + 56)[0]
    sect = []
    so = opt + struct.unpack_from("<H", data, pe + 20)[0]
    for i in range(nsec):
        b = so + 40 * i
        name = data[b:b + 8].rstrip(b"\0").decode()
        vsize, va, rsize, rptr = struct.unpack_from("<IIII", data, b + 8)
        sect.append((name, va, vsize, rptr, rsize))
    nrv = struct.unpack_from("<I", data, opt + 0x6C)[0]
    dirs = [struct.unpack_from("<II", data, opt + 0x70 + 8 * i) for i in range(nrv)]
    return {"kind": "pe", "base": IMAGE_BASE, "ts": ts, "size_of_image": size_of_image, "sections": sect,
            "exec": [(va, va + max(vs, rs)) for name, va, vs, rptr, rs in sect if name == ".text"],
            "export": dirs[0] if dirs else (0, 0)}


def parse_elf(data):
    """ELF64 little-endian image in the same shape as parse_pe: `sections` are
    (name, addr, size, offset, filesize) with the address space (not the file) as the RVA space, so
    rva_to_off/section/section_range work unchanged; `base` is 0 (a .so's vaddrs *are* the RVAs),
    `exec` lists the executable sections (SHF_EXECINSTR) and `symbols` the static/dynamic symbol
    tables (itanium_vtable and the `export` locator read them)."""
    if data[:4] != b"\x7fELF":
        raise ValueError("not an ELF")
    if data[4] != 2 or data[5] != 1:
        raise ValueError("not a 64-bit little-endian ELF")
    shoff = struct.unpack_from("<Q", data, 0x28)[0]
    shentsize = struct.unpack_from("<H", data, 0x3A)[0]
    shnum = struct.unpack_from("<H", data, 0x3C)[0]
    shstrndx = struct.unpack_from("<H", data, 0x3E)[0]
    raw = []
    for i in range(shnum):
        o = shoff + shentsize * i
        nm, typ, flags, addr, off, size, link, info, align, entsize = struct.unpack_from("<IIQQQQIIQQ", data, o)
        raw.append({"nameoff": nm, "type": typ, "flags": flags, "addr": addr, "off": off,
                    "size": size, "link": link, "entsize": entsize})
    names = b""
    if shstrndx < len(raw):
        s = raw[shstrndx]
        names = data[s["off"]:s["off"] + s["size"]]
    for s in raw:
        end = names.find(b"\0", s["nameoff"])
        s["name"] = names[s["nameoff"]:end].decode("latin-1") if end >= 0 else ""
    SHT_NOBITS = 8
    sections = [(s["name"], s["addr"], s["size"],
                 0 if s["type"] == SHT_NOBITS else s["off"], 0 if s["type"] == SHT_NOBITS else s["size"])
                for s in raw]
    exec_ = [(s["addr"], s["addr"] + s["size"]) for s in raw if s["flags"] & 0x4 and s["size"]]
    size_of_image = max([addr + size for _n, addr, size, _o, _sz in sections] or [0])
    return {"kind": "elf", "base": 0, "ts": 0, "sections": sections, "exec": exec_,
            "elf": raw, "symbols": elf_symbols(data, raw), "relocs": elf_relocs(data, raw),
            "dynsym_index": elf_symbol_index(data, raw),
            "size_of_image": size_of_image}


def elf_symbol_index(data, raw):
    """The .dynsym entries by symbol index, as (value, size, name, shndx): a relocation's symbol
    index addresses this table."""
    out = []
    for s in raw:
        if s["name"] != ".dynsym" or not s["size"]:
            continue
        strings = elf_strings(data, raw, s)
        for o in range(s["off"], s["off"] + s["size"], s["entsize"] or 24):
            nm, info, other, shndx, value, size = struct.unpack_from("<IBBHQQ", data, o)
            out.append((value, size, elf_cstring(strings, nm), shndx))
    return out


def elf_relocs(data, raw):
    """{address: (type, symbol index, addend)} from every SHT_RELA section (.rela.dyn, .rela.plt).

    A .so keeps relocated words in .data.rel.ro as zero (or as the addend) in the file: the loader
    writes the real value from the relocation, so a vtable slot must be read through this table and
    not from the raw bytes (sigs/README.md "Linux binaries")."""
    out = {}
    for s in raw:
        if s["type"] != 4 or not s["size"]:
            continue
        for o in range(s["off"], s["off"] + s["size"], s["entsize"] or 24):
            r_off, r_info, r_add = struct.unpack_from("<QQq", data, o)
            out[r_off] = (r_info & 0xFFFFFFFF, r_info >> 32, r_add)
    return out


def image_qword(img, data, va):
    """The 8-byte value at `va` as the loader would leave it: from the relocation table when the
    word is relocated (ELF), else the file content."""
    rel = (img.get("relocs") or {}).get(va)
    if rel is not None:
        rtype, sym, r_add = rel
        if rtype in (1, 6, 7) and sym:                       # R_X86_64_64 / GLOB_DAT / JUMP_SLOT
            tab = img.get("dynsym_index") or []
            if sym < len(tab):
                return (tab[sym][0] + r_add) & 0xFFFFFFFFFFFFFFFF
            return r_add & 0xFFFFFFFFFFFFFFFF
        if rtype == 8:                                       # R_X86_64_RELATIVE
            return r_add & 0xFFFFFFFFFFFFFFFF
    off = rva_to_off(img, va)
    return int.from_bytes(data[off:off + 8], "little")


def elf_strings(data, raw, section):
    tab = raw[section["link"]] if section["link"] < len(raw) else None
    return data[tab["off"]:tab["off"] + tab["size"]] if tab else b""


def elf_cstring(strings, off):
    end = strings.find(b"\0", off)
    return strings[off:end].decode("latin-1") if end >= 0 else ""


def elf_symbols(data, raw):
    """{name: (value, size, bind, type, dynamic)} from every symbol table in the image, .dynsym
    first: .so files are usually stripped of .symtab, so `_ZTV…`/`_ZTI…` are only in .dynsym."""
    out = {}
    for s in raw:
        if s["type"] not in (2, 11) or not s["size"]:            # SHT_SYMTAB, SHT_DYNSYM
            continue
        strtab = raw[s["link"]] if s["link"] < len(raw) else None
        strings = data[strtab["off"]:strtab["off"] + strtab["size"]] if strtab else b""
        entsize = s["entsize"] or 24
        for o in range(s["off"], s["off"] + s["size"], entsize):
            nm, info, other, shndx, value, size = struct.unpack_from("<IBBHQQ", data, o)
            if shndx == 0:                                        # SHN_UNDEF: an import, not a definition
                continue
            end = strings.find(b"\0", nm)
            name = strings[nm:end].decode("latin-1") if end >= 0 else ""
            if name and name not in out:
                out[name] = (value, size, info >> 4, info & 0xF, s["type"] == 11)
    return out


def load_image(data):
    """parse_pe or parse_elf, by magic."""
    return parse_elf(data) if data[:4] == b"\x7fELF" else parse_pe(data)


def elf_vtable_symbol(name):
    """`_ZTV` symbol of an Itanium class name: `CSchemaSystem` -> `_ZTV13CSchemaSystem`,
    `A::B` -> `_ZTVN1A1BE`. A name already starting with `_Z` is taken as the symbol itself."""
    if name.startswith("_Z"):
        return name
    parts = name.split("::")
    inner = "".join("%d%s" % (len(p), p) for p in parts)
    return "_ZTV" + (inner if len(parts) == 1 else "N" + inner + "E")


def rva_to_off(pe, rva):
    for name, va, vsize, rptr, rsize in pe["sections"]:
        if va <= rva < va + max(vsize, rsize):
            return rptr + (rva - va)
    raise ValueError("rva %#x not in any section" % rva)


def section(pe, name):
    for s in pe["sections"]:
        if s[0] == name:
            return s
    raise ValueError("no section %s" % name)


def section_range(pe, name):
    """(file start, file end, rva start) of a section, or of the whole image for 'any'"""
    if name == "any":
        starts = [(rptr, rptr + min(vs, rs), va) for _, va, vs, rptr, rs in pe["sections"] if rs]
        return starts
    _, va, vs, rptr, rs = section(pe, name)
    return [(rptr, rptr + min(vs, rs), va)]


def exports(data, img):
    """The image's export table: PE by RVA, ELF by the *dynamic* symbol table (.dynsym, defined
    global/weak symbols) - a .so's export set."""
    if img["kind"] == "elf":
        return {n: v[0] for n, v in img["symbols"].items() if v[4] and v[2] in (1, 2)}
    rva, size = img["export"]
    out = {}
    if not rva:
        return out
    o = rva_to_off(img, rva)
    nfun, nnames, addr_tbl, name_tbl, ord_tbl = struct.unpack_from("<IIIII", data, o + 20)
    for i in range(nnames):
        nrva = struct.unpack_from("<I", data, rva_to_off(img, name_tbl) + 4 * i)[0]
        ordn = struct.unpack_from("<H", data, rva_to_off(img, ord_tbl) + 2 * i)[0]
        frva = struct.unpack_from("<I", data, rva_to_off(img, addr_tbl) + 4 * ordn)[0]
        out[cstring_at(data, img, nrva).decode()] = frva
    return out


def cstring_at(data, pe, rva):
    off = rva_to_off(pe, rva)
    return data[off:data.index(b"\0", off)]


_RTTI_CACHE = {}


def rtti_index(data, pe):
    """{(TypeDescriptor name, col_offset): [vtable RVA, ...]} for every COL in .rdata (built once per binary)"""
    key = id(data)
    if key in _RTTI_CACHE and _RTTI_CACHE[key][0] is data:
        return _RTTI_CACHE[key][1]
    import numpy as np
    _, rva, rvs, rrp, rrs = section(pe, ".rdata")
    n = min(rvs, rrs) // 8 * 8
    raw = np.frombuffer(data, dtype=np.uint8, count=n, offset=rrp)
    d32 = raw.view("<u4")
    # COL: signature(1) offset cdOffset pTypeDescriptor pClassDescriptor pSelf, 4-aligned
    cand = np.nonzero(d32[:-6] == 1)[0]
    cand = cand[d32[cand + 5] == (rva + cand * 4).astype(np.uint64)]
    cols = {}
    for i in cand.tolist():
        cols[IMAGE_BASE + rva + i * 4] = (int(d32[i + 1]), int(d32[i + 3]))
    q = raw.view("<u8")
    colva = np.fromiter(cols.keys(), dtype=np.uint64, count=len(cols))
    hits = np.nonzero(np.isin(q, colva))[0]
    idx = {}
    for j in hits.tolist():
        off, td = cols[int(q[j])]
        try:
            name = cstring_at(data, pe, td + 16).decode("latin-1")
        except (ValueError, IndexError):
            continue
        idx.setdefault((name, off), []).append(rva + j * 8 + 8)
    _RTTI_CACHE[key] = (data, idx)
    return idx


def rtti_vtable(data, pe, cls, col_offset=0):
    """vtable RVA of class `cls` whose complete-object locator has offset col_offset; unique.
    `cls` is the class name (TypeDescriptor `.?AV<cls>@@`) or a full TypeDescriptor name
    starting with `.?A` (structs: `.?AUName@@`)."""
    tdname = cls if cls.startswith(".?A") else ".?AV%s@@" % cls
    found = rtti_index(data, pe).get((tdname, col_offset), [])
    if len(found) != 1:
        raise LookupError("RTTI: %s@%d has %d vtables %s" % (cls, col_offset, len(found), [hex(x) for x in found]))
    return found[0]


# a vftable longer than this is not a vftable; the scan is bounded so a stray run of code
# pointers cannot loop over the section
VTABLE_MAX_SLOTS = 4096


def exec_ranges(img):
    """[(lo, hi)] of the image's executable sections. parse_pe/parse_elf fill `exec`; a hand-built
    image dict falls back to the PE .text section."""
    rng = img.get("exec")
    if rng is None:
        rng = [(va, va + max(vs, rs)) for name, va, vs, rptr, rs in img.get("sections", [])
               if name == ".text"]
    return rng


def vtable_len(data, img, vt):
    """Length of the vtable at address `vt`, in slots (8-byte entries).

    MSVC lays a vftable out in .rdata as: the complete-object-locator pointer (`vt[-1]`), then a
    run of 8-byte function pointers into .text. The run ends at the first slot that is not an
    image pointer into .text - the next table's COL pointer, a data/string pointer, a null, or the
    end of a section. So the length is the number of leading .text-pointing slots, counted from
    the first slot after the locator. Itanium lays each vtable of a group behind its two-word
    header (offset-to-top, typeinfo) and `vt` is the group table's address point (the first
    virtual function), so the same test gives the group table's length: the run ends at the next
    table's header word (an offset, never a code pointer), a data pointer or a null. False
    positives inside the run (a jmp thunk or a code address stored as data) are not
    distinguishable this way, which is why this locator is only used for vtables whose length is
    also recorded in the entry's `expect` and `via`. Slots are read through the relocation table
    (ELF .data.rel.ro is zero in the file), so a relocated vtable measures correctly.
    """
    if not exec_ranges(img):
        raise LookupError("vtlen: no executable section")
    base = img.get("base", IMAGE_BASE)
    n = 0
    while n < VTABLE_MAX_SLOTS:
        try:
            v = image_qword(img, data, vt + 8 * n) - base
        except (ValueError, IndexError):
            break                      # ran off the end of the image
        if not any(lo <= v < hi for lo, hi in exec_ranges(img)):
            break                      # not a code pointer: end of the table
        n += 1
    if n == 0:
        raise LookupError("vtlen: %#x has no code pointer in slot 0 (not a vftable)" % vt)
    return n


def sha16(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()[:16]


# ------------------------------------------------------------------ pattern + resolution
PAT_TOKEN = re.compile(r"^([0-9A-Fa-f]{2}|\?\?)$")


def pattern(text):
    toks = text.split()
    if not toks:
        raise ValueError("empty pattern")
    for t in toks:
        if not PAT_TOKEN.match(t):
            raise ValueError("bad pattern token %r" % t)
    rx = b"".join(b"." if t == "??" else re.escape(bytes([int(t, 16)])) for t in toks)
    # zero-width lookahead: finditer then also reports OVERLAPPING occurrences, so a self-repeating
    # pattern (e.g. `E8 ?? ?? ?? ?? 48 8B CF` x7) cannot pass as unique while matching twice
    return re.compile(b"(?=" + rx + b")", re.S), [None if t == "??" else int(t, 16) for t in toks]


class Resolver:
    def __init__(self, data, img):
        self.data, self.img = data, img
        # a hand-built image dict may not carry the platform tag: it is a PE
        self.base = img.get("base", IMAGE_BASE)
        self.exports = None
        self.done = {}

    def rd(self, rva, size):
        o = rva_to_off(self.img, rva)
        return int.from_bytes(self.data[o:o + size], "little", signed=True)

    def symbol(self, name):
        """A defined symbol of the image (ELF `_ZTV…`/`_ZTI…`/export symbols): its address."""
        syms = self.img.get("symbols") or {}
        if name not in syms:
            raise LookupError("symbol %s not defined in the image" % name)
        return syms[name][0]

    def scan(self, sig, sect, refs):
        rx, _ = pattern(sig)
        hits = []
        for start, end, va in section_range(self.img, sect):
            for m in rx.finditer(self.data, start, end):
                rva = va + (m.start() - start)
                ok = True
                for ref in refs:
                    try:
                        tgt = rva + ref["next"] + self.rd(rva + ref["at"], 4)
                        ok = ok and cstring_at(self.data, self.img, tgt) == ref["string"].encode()
                    except (ValueError, IndexError):
                        ok = False
                if ok:
                    hits.append(rva)
        return hits

    def steps(self, cur, steps, name):
        value = None
        for st in steps:
            if "call" in st:
                at = cur + st["call"]
                if self.data[rva_to_off(self.img, at)] != 0xE8:
                    raise LookupError("%s: no E8 call at +%#x (rva %#x)" % (name, st["call"], at))
                cur = at + 5 + self.rd(at + 1, 4)
            elif "rip" in st:
                cur = cur + st["next"] + self.rd(cur + st["rip"], 4)
            elif "check" in st:
                _, vals = pattern(st["check"])
                o = rva_to_off(self.img, cur)
                got = self.data[o:o + len(vals)]
                if any(v is not None and v != g for v, g in zip(vals, got)):
                    raise LookupError("%s: check pattern failed at %#x" % (name, cur))
            elif "disp" in st:
                value = self.rd(cur + st["disp"], st["size"])
            elif "div" in st:
                # a byte offset into a table of 8-byte entries -> the slot index
                if value is None or st["div"] <= 0 or value % st["div"]:
                    raise LookupError("%s: value %r is not a multiple of %r" % (name, value, st["div"]))
                value //= st["div"]
            elif "add" in st:
                if value is None:
                    cur += st["add"]
                else:
                    value += st["add"]
            elif "deref" in st:
                v = image_qword(self.img, self.data, cur + st["deref"])
                cur = v - self.base
            elif "rva" in st:
                cur = self.rd(cur + st["rva"], 4) & 0xFFFFFFFF
            else:
                raise LookupError("%s: unknown step %r" % (name, st))
        return value if value is not None else cur

    def itanium_vtable(self, loc):
        """Address point of an Itanium vtable: the `_ZTV…` symbol + 16. The symbol names the table
        group's start - [offset-to-top][typeinfo][first virtual function] - and its st_size covers
        that header too (so st_size/8 - 2 is the length the ELF symbol records; the entry's `via`
        states that number when a person checks the measured length). `symbol` names the mangled
        symbol, `class` the class name (`CSchemaSystem`, `A::B`). With `measure`, the locator
        instead resolves the table's length: `symbol` (exact, from the linker's st_size) or `scan`
        (the code-pointer run, which under-counts when the table's last slot is a null - a pure
        virtual the linker left at 0 - rather than a thunk)."""
        sym = loc.get("symbol") or elf_vtable_symbol(loc["class"])
        syms = self.img.get("symbols") or {}
        if loc.get("measure") == "symbol":
            if sym not in syms:
                raise LookupError("symbol %s not defined in the image" % sym)
            size = syms[sym][1]
            if size < 16 or (size - 16) % 8:
                raise LookupError("%s: st_size %d is not a vtable group (16 + 8*n)" % (sym, size))
            return size // 8 - 2
        point = self.symbol(sym) + 16
        if loc.get("measure") == "scan":
            return vtable_len(self.data, self.img, point)
        return point

    def resolve(self, name, entry, table):
        """entry -> RVA (function/vtable/global/string) or integer value (offset/const)"""
        if name in self.done:
            return self.done[name]
        loc = entry.get("locator")
        if loc is None:
            # literal entry (kind=const/offset with `value`): a layout number sourced from
            # verified bridge code (via = file:line); no locator derives it. Validated against
            # expect[build] like any resolved entry.
            if "value" not in entry:
                raise LookupError("%s: no locator and no literal value" % name)
            self.done[name] = entry["value"]
            return entry["value"]
        kind = loc["type"]
        if kind == "sig":
            hits = self.scan(loc["sig"], loc.get("section", ".text"), loc.get("refs", []))
            if len(hits) != 1:
                raise LookupError("%s: pattern matched %d times (must be exactly 1)" % (name, len(hits)))
            cur = hits[0]
        elif kind == "rtti":
            cur = rtti_vtable(self.data, self.img, loc["class"], loc.get("col_offset", 0))
        elif kind == "itanium_vtable":
            v = self.itanium_vtable(loc)
            if loc.get("measure"):
                # a measured length is the entry's value, not a position: no steps
                self.done[name] = v
                return v
            cur = v
        elif kind == "vslot":
            vt = self.resolve(loc["vtable"], table[loc["vtable"]], table)
            v = image_qword(self.img, self.data, vt + 8 * loc["slot"])
            if not self.base <= v < self.base + self.img["size_of_image"]:
                raise LookupError("%s: vtable %#x slot %d is not an image pointer" % (name, vt, loc["slot"]))
            cur = v - self.base
        elif kind == "export":
            if self.exports is None:
                self.exports = exports(self.data, self.img)
            if loc["name"] not in self.exports:
                raise LookupError("%s: export %s not found" % (name, loc["name"]))
            cur = self.exports[loc["name"]]
        elif kind == "string":
            needle = loc["string"].encode() + b"\0"
            hits = []
            for start, end, va in section_range(self.img, loc.get("section", ".rdata")):
                j = self.data.find(needle, start, end)
                while j != -1:
                    # must start a string: previous byte is NUL or section start
                    if j == start or self.data[j - 1] == 0:
                        hits.append(va + (j - start))
                    j = self.data.find(needle, j + 1, end)
            if len(hits) != 1:
                raise LookupError("%s: string occurs %d times" % (name, len(hits)))
            cur = hits[0]
        elif kind == "base":
            if loc["base"] not in table:
                raise LookupError("%s: base %s is not an entry" % (name, loc["base"]))
            cur = self.resolve(loc["base"], table[loc["base"]], table)
        elif kind == "vtlen":
            # length (in slots) of the vtable of a kind=vtable entry of the same file
            if loc["vtable"] not in table:
                raise LookupError("%s: vtlen.vtable %s is not an entry" % (name, loc["vtable"]))
            cur = vtable_len(self.data, self.img,
                             self.resolve(loc["vtable"], table[loc["vtable"]], table))
        else:
            raise LookupError("%s: unknown locator type %s" % (name, kind))
        out = self.steps(cur, loc.get("steps", []), name)
        self.done[name] = out
        return out


# ------------------------------------------------------------------ fingerprints
# sigs/fingerprints/<module>-fp.json holds the masked code at every entry's expect address on the
# build in fingerprints/BUILD (sigs/ida/ida_fp.py). sigupdate.py --bridge-gate compares them with
# the bytes a locator resolves to on a new build, and `validate` requires them: fp_keys() is the
# one definition of which keys an entry is checked against, required_fp_keys() of which of those a
# bridge entry *must* have (sigs/README.md "The bridge gate").
FPDIR = SIGS / "fingerprints"


def fp_file(mod):
    """The fingerprint file of a module: sigs/fingerprints/<module stem>-fp.json"""
    return FPDIR / (mod.rsplit(".", 1)[0] + "-fp.json")


def load_fps(mod):
    """The module's fingerprint file as {key: pattern}, plus its reserved `_weak` list; {} when
    there is none (every required key is then missing, so the gate and validate fail closed)."""
    f = fp_file(mod)
    return json.loads(f.read_text()) if f.exists() else {}


def fp_keys(name, e):
    """[(key, what)] - the fingerprints an entry's resolved value is checked against.

    kind=function -> the entry itself (its target's masked prologue); a `vslot` locator is pinned
    by `cargo bridge pins` as a *slot target* whatever its kind says, so it needs one for the same
    reason. A vtable entry with `ident_slot` (`cargo bridge pins` emits a PinSlot identity row for it,
    whose target it reads from that slot) -> "<entry>#<slot>", the function the slot must still
    hold. Bridge entries located by sig/rtti/string/export have no such key: their locator is the
    evidence, and what they resolve to is an RVA or an integer rather than code the bridge calls.

    Read defensively (`.get`, an int check that rejects bool): `validate` calls it on files that may
    be broken (a missing `kind` is reported by validate itself, and must not raise here instead).
    """
    loc = e.get("locator")
    loc = loc if isinstance(loc, dict) else {}
    out = []
    if e.get("kind") == "function" or loc.get("type") == "vslot":
        out.append((name, "target"))
    if isinstance(e.get("ident_slot"), int) and not isinstance(e.get("ident_slot"), bool):
        out.append(("%s#%d" % (name, e["ident_slot"]), "slot"))
    return out


def required_fp_keys(name, e):
    """The fingerprint keys a bridge entry must have, i.e. the keys that have to exist in the
    module's fingerprint file for the entry to be pinnable.

    [] for an entry without a `bridge` macro (nothing pins it) and for a bridge entry whose locator
    carries its own evidence (sig/rtti/string/export resolve to an RVA or an integer rather than to
    code). This is *the* definition of "requires a fingerprint": sigupdate.py --bridge-gate's
    BRIDGE COVERAGE counts these, and `validate` refuses a file where one is not in
    sigs/fingerprints/<module>-fp.json.
    """
    if not e.get("bridge"):
        return []
    return [k for k, _what in fp_keys(name, e)]


def fp_weak(keys, fps):
    """The subset of `keys` the fingerprint file itself marks weak (its `_weak` list): ida_fp.py
    could not tell the pattern apart from the functions the entry could silently be swapped with
    (a tail-call thunk or a twin function that differs only in a masked target), so the gate's
    comparison would still pass after such a swap. Only a live check or a person covers those, so
    the count is printed next to the missing ones instead of being hidden.

    A key has to be in the file to be weak, so the three buckets stay disjoint and add up:
    required = missing + weak + strong. Read defensively: the gate does not run `validate` first."""
    listed = fps.get("_weak")
    weak = {w for w in listed if isinstance(w, str)} if isinstance(listed, list) else set()
    return [k for k in keys if k in weak and k in fps]


def fp_match(pattern, data, img, rva):
    """Do the masked bytes of a fingerprint match the image at `rva`?"""
    toks = pattern.split()
    try:
        off = rva_to_off(img, rva)
    except (ValueError, IndexError):
        return False
    got = data[off:off + len(toks)]
    return len(got) == len(toks) and all(t == "??" or int(t, 16) == b for t, b in zip(toks, got))


def slot_target(data, img, vt, slot):
    """RVA the vtable at `vt` holds in `slot`, or None when it is not an image pointer.

    ELF-aware: a .so keeps its relocated vtable slots zero in the file (the loader fills them
    from .rela), so the qword is read the way the loader would leave it (`image_qword`)
    whenever the image is an ELF, and checked against the image's own base/size (a .so's
    base 0; a PE's IMAGE_BASE). Windows images keep the previous file-content read, and a
    hand-built PE img dict (parse_pe pieces without `kind`) takes the same path by default."""
    va = vt + 8 * slot
    try:
        if img.get("kind") == "elf":
            v = image_qword(img, data, va)
        else:
            off = rva_to_off(img, va)
            v = int.from_bytes(data[off:off + 8], "little")
    except (ValueError, IndexError):
        return None
    base = img.get("base", IMAGE_BASE)
    if not base <= v < base + img["size_of_image"]:
        return None
    return v - base


def fp_checks(name, e, value, data, img, fps):
    """[(key, rva or None, present)] - where each of the entry's fingerprints has to match.

    The one place the "where" of a fingerprint is decided: sigupdate.py --bridge-gate judges
    SAME/OK/DRIFT/FAIL with it, and `cargo bridge pins` (pinsdata.py) checks the same thing before
    it writes pins.rs (so a hand-edited or partially recorded database cannot be pinned either way).
    """
    out = []
    for key, what in fp_keys(name, e):
        if what == "target":
            out.append((key, value, key in fps))
        else:
            out.append((key, slot_target(data, img, value, e["ident_slot"]), key in fps))
    return out


# ------------------------------------------------------------------ sdk (generated SDK owner)
# An entry's optional `sdk` names the generated expression (crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h, gen.py's tie
# syntax) that owns the number: the bridge spells that S2_* macro, and the entry stays as the
# binary-side measurement. `cargo bridge pins` does not emit an `sdk` entry as a pin and refuses to
# write when the expression and the measurement differ; sdkcheck.py reports one row per entry.
# The expression is a sum of terms; a term is a generated macro name, `S2_SLOT_<...>*8` (a slot the
# game calls as a vtable byte displacement) or a decimal addend (`S2_OFF_CMapNode_m_Children+8`, the
# data pointer of a vector member).
SDK_GEN_H = ROOT / "crates" / "s2sdk" / "s2sdk-sys" / "include" / "s2sdk_gen.h"
SDK_TERM_RX = re.compile(r"^(?:(S2_[A-Za-z0-9_]+)(?:\*(8))?|(\d+))$")
SDK_DEFINE_RX = re.compile(r"^#define\s+(S2_[A-Za-z0-9_]+)\s+(0x[0-9a-fA-F]+|\d+)[uUlL]*\b", re.M)
SDK_KINDS = ("function", "offset", "const")


def sdk_terms(expr):
    """`sdk` text -> [(macro or None, factor, literal)]; ValueError when it is not the syntax.
    A factor is only allowed on an S2_SLOT_ term (gen.py split_tie_scale's rule), and the sum must
    name at least one generated macro (a pure literal would be a hand copy, not an owner)."""
    if not isinstance(expr, str) or not expr:
        raise ValueError("sdk must be a non-empty string")
    out = []
    for term in expr.split("+"):
        m = SDK_TERM_RX.match(term)
        if not m:
            raise ValueError("sdk term %r is not S2_<NAME>, S2_SLOT_<NAME>*8 or a decimal" % term)
        if m.group(2) and not m.group(1).startswith("S2_SLOT_"):
            raise ValueError("sdk term %r: only an S2_SLOT_ macro may carry *8" % term)
        out.append((m.group(1), int(m.group(2) or 1), int(m.group(3) or 0)))
    if not any(mac for mac, _f, _l in out):
        raise ValueError("sdk %r names no generated macro" % expr)
    return out


_SDK_DEFINES = {}


def sdk_defines(path=SDK_GEN_H):
    """Every integer S2_* #define of the generated header (gen.py --check proves it is the pinned
    upstream's): the values `sdk` expressions are evaluated against. Cached per path."""
    if path not in _SDK_DEFINES:
        _SDK_DEFINES[path] = {m.group(1): int(m.group(2), 0)
                              for m in SDK_DEFINE_RX.finditer(path.read_text())}
    return _SDK_DEFINES[path]


def sdk_eval(expr, defines):
    """The value of an `sdk` expression; LookupError naming a macro the header does not emit."""
    total = 0
    for mac, factor, lit in sdk_terms(expr):
        if mac is None:
            total += lit
        elif mac not in defines:
            raise LookupError("sdk %r: %s is not emitted by crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h" % (expr, mac))
        else:
            total += defines[mac] * factor
    return total


def sdk_measured(ent, value):
    """The number an `sdk` entry's measurement gives: the slot index of a vslot locator (its
    resolved value is the slot *target*, an RVA), else the resolved value itself."""
    loc = ent.get("locator")
    if isinstance(loc, dict) and loc.get("type") == "vslot":
        return loc["slot"]
    return value


# ------------------------------------------------------------------ database
KINDS = {"function", "vtable", "global", "offset", "string", "const"}
# kinds that may carry a literal `value` instead of a locator (a layout number sourced from
# verified bridge code, `via` = file:line; sigcheck resolve returns it and checks expect)
VALUE_KINDS = {"const", "offset"}
LOCATORS = {"sig", "rtti", "itanium_vtable", "vslot", "export", "string", "base", "vtlen"}
# locators that only make sense on one platform's ABI (MSVC RTTI vs Itanium vtables)
PEER_LOCATORS = {"windows": {"rtti"}, "linux": {"itanium_vtable"}}
TAGS = {"V", "I", "RTTI", "S", "D", "?"}
# entry lifecycle (sigs/signatures/SCHEMA.json): proposed -> verified -> stale. `stale` is what
# sigupdate.py record sets on an entry that failed on the newest build.
STATUSES = {"proposed", "verified", "stale"}
NAME_RX = re.compile(r"^[A-Za-z_][A-Za-z0-9_:<>,~ *&]*$")


def file_platform(d):
    """A database file's platform: the explicit top-level `platform`, else by module suffix
    (`.so` -> linux, `.dll`/`.exe` -> windows, so every pre-existing file stays windows)."""
    if d.get("platform"):
        return d["platform"]
    return "linux" if d["module"].endswith(".so") else "windows"


def entry_platform(d, ent):
    return ent.get("platform") or file_platform(d)


def load_db(files=None, platform=None):
    """The database files, optionally only those of one platform (a `resolve --platform`)."""
    files = [Path(f) for f in files] if files else sorted(SIGDIR.glob("*.json"))
    db = {}
    for f in files:
        if f.name == "SCHEMA.json":
            continue
        d = json.loads(f.read_text())
        if platform and file_platform(d) != platform:
            continue
        db[f] = d
    return db


def validate_file(path, d):
    errs = []

    def e(msg):
        errs.append("%s: %s" % (path.name, msg))

    for key in ("module", "image_base", "entries"):
        if key not in d:
            e("missing top-level key %r" % key)
    if errs:
        return errs
    platform = file_platform(d)
    if platform not in PLATFORMS:
        e("platform must be one of %s" % sorted(PLATFORMS))
    if d.get("platform") and platform == "linux" and not d["module"].endswith(".so"):
        e("module %s is not a .so but the file says platform=linux" % d["module"])
    stem = d["module"].rsplit(".", 1)[0]
    if d["module"] + ".json" != path.name and path.name != stem + ".json":
        e("file name should be <module>.json for module %s" % d["module"])
    if platform == "linux":
        if d["image_base"] != 0:
            e("image_base must be 0 for an ELF (.so) module")
    elif d["image_base"] != IMAGE_BASE:
        e("image_base must be %#x" % IMAGE_BASE)
    entries = d["entries"]
    if not isinstance(entries, dict):
        e("entries must be an object keyed by name")
        return errs
    for name, ent in entries.items():
        where = "entry %r" % name
        if not NAME_RX.match(name):
            e("%s: bad name" % where)
        elif name == "_weak":
            e("%s: bad name ('_weak' is reserved for the fingerprint files' weak list)" % where)
        for key in ("kind", "via", "tag"):
            if key not in ent:
                e("%s: missing %r" % (where, key))
        if ent.get("platform") not in (None, *PLATFORMS):
            e("%s: platform must be one of %s" % (where, sorted(PLATFORMS)))
        elif ent.get("platform") and ent["platform"] != platform:
            e("%s: entry platform %s does not match the file's %s (one binary per file)"
              % (where, ent["platform"], platform))
        if ent.get("kind") not in KINDS:
            e("%s: kind must be one of %s" % (where, sorted(KINDS)))
        if ent.get("tag") not in TAGS:
            e("%s: tag must be one of %s" % (where, sorted(TAGS)))
        if ent.get("status") not in (None, *STATUSES):
            e("%s: status must be one of %s" % (where, sorted(STATUSES)))
        if "sdk" in ent:
            loc = ent.get("locator") if isinstance(ent.get("locator"), dict) else {}
            try:
                terms = sdk_terms(ent["sdk"])
            except ValueError as ex:
                e("%s: %s" % (where, ex))
            else:
                if ent.get("kind") not in SDK_KINDS:
                    e("%s: sdk is only allowed on kind=%s" % (where, "/".join(SDK_KINDS)))
                elif ent.get("kind") == "function" and loc.get("type") != "vslot":
                    # a direct function's number is an address: no header can own it
                    e("%s: sdk on kind=function needs a vslot locator (the slot is what the "
                      "header owns)" % where)
                elif loc.get("type") == "vslot" and (
                        len(terms) != 1 or terms[0][1] != 1
                        or not (terms[0][0] or "").startswith("S2_SLOT_")):
                    e("%s: a vslot entry's sdk must be one bare S2_SLOT_ macro (its slot index)"
                      % where)
                else:
                    try:
                        sdk_eval(ent["sdk"], sdk_defines())
                    except LookupError as ex:
                        e("%s: %s" % (where, ex))
            if platform != "windows":
                e("%s: sdk is a Windows-entry field (crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h is the MSVC side)" % where)
        # ident_slot is a vtable-only extension (identity probe slot; `cargo bridge pins` emits the check)
        if "ident_slot" in ent:
            if ent.get("kind") != "vtable":
                e("%s: ident_slot is only allowed on kind=vtable" % where)
            elif (not isinstance(ent["ident_slot"], int) or isinstance(ent["ident_slot"], bool)
                  or ent["ident_slot"] < 0):
                e("%s: ident_slot must be a non-negative integer" % where)
        # struct/field/ctype is offset-only grouping metadata for the struct emitter
        for k in ("struct", "field", "ctype"):
            if k in ent:
                if ent.get("kind") != "offset":
                    e("%s: %r is only allowed on kind=offset" % (where, k))
                elif not isinstance(ent[k], str) or not ent[k]:
                    e("%s: %s must be a non-empty string" % (where, k))
        # `value` is the literal for a locator-less const/offset entry only: when a locator is
        # present the Resolver derives the value and would silently ignore `value`, and kinds
        # that resolve to an RVA (function/vtable/global/string) cannot carry a literal value
        loc = ent.get("locator")
        if "value" in ent:
            if loc is not None:
                e("%s: 'value' and 'locator' are mutually exclusive (the locator's result wins; drop one)" % where)
            elif ent.get("kind") not in VALUE_KINDS:
                e("%s: 'value' is only allowed on kind in %s" % (where, sorted(VALUE_KINDS)))
        # a kind=const/offset entry without a locator carries its resolved value literally
        t = loc.get("type") if isinstance(loc, dict) else None
        if t not in LOCATORS:
            if loc is None and ent.get("kind") in VALUE_KINDS and "value" in ent:
                if not isinstance(ent["value"], int):
                    e("%s: value must be an integer" % where)
                if not ent.get("expect"):
                    e("%s: literal (locator-less) entry needs expect[<build>]" % where)
                else:
                    for b, v in ent["expect"].items():
                        if not isinstance(v, int):
                            e("%s: expect[%s] must be an integer" % (where, b))
                        elif v != ent["value"]:
                            e("%s: expect[%s]=%d disagrees with value=%d" % (where, b, v, ent["value"]))
            else:
                e("%s: locator.type must be one of %s" % (where, sorted(LOCATORS)))
                continue
        else:
            try:
                if t == "sig":
                    _, vals = pattern(loc["sig"])
                    if len(vals) < 8:
                        e("%s: pattern shorter than 8 bytes" % where)
                    if vals[0] is None:
                        e("%s: pattern must not start with ??" % where)
                    if all(v is None for v in vals):
                        e("%s: pattern is all wildcards" % where)
                elif t == "rtti":
                    loc["class"]
                elif t == "itanium_vtable":
                    if not (loc.get("symbol") or loc.get("class")):
                        e("%s: itanium_vtable needs 'symbol' or 'class'" % where)
                    if loc.get("measure") not in (None, "symbol", "scan"):
                        e("%s: itanium_vtable measure must be 'symbol' or 'scan' when present" % where)
                    elif loc.get("measure") and ent.get("kind") != "const":
                        e("%s: itanium_vtable measure resolves to a slot count: kind must be const" % where)
                    elif loc.get("measure") and loc.get("steps"):
                        e("%s: itanium_vtable measure takes no steps (it resolves the length)" % where)
                elif t == "vslot":
                    if loc["vtable"] not in entries:
                        e("%s: vslot.vtable %r is not an entry in this file" % (where, loc["vtable"]))
                    elif entries[loc["vtable"]].get("kind") != "vtable":
                        e("%s: vslot.vtable %r is not kind=vtable" % (where, loc["vtable"]))
                    int(loc["slot"])
                elif t == "export":
                    loc["name"]
                elif t == "string":
                    loc["string"]
                elif t == "base":
                    if loc["base"] not in entries:
                        e("%s: base %r is not an entry in this file" % (where, loc["base"]))
                elif t == "vtlen":
                    # a vtable length is an integer: const, from a vtable entry of this file
                    if ent.get("kind") != "const":
                        e("%s: vtlen resolves to a slot count: kind must be const" % where)
                    if loc["vtable"] not in entries:
                        e("%s: vtlen.vtable %r is not an entry in this file" % (where, loc["vtable"]))
                    elif entries[loc["vtable"]].get("kind") != "vtable":
                        e("%s: vtlen.vtable %r is not kind=vtable" % (where, loc["vtable"]))
            except (KeyError, ValueError) as ex:
                e("%s: locator incomplete: %s" % (where, ex))
            other = "linux" if platform == "windows" else "windows"
            if t in PEER_LOCATORS[other]:
                e("%s: locator %s is a %s-ABI locator, not a %s one" % (where, t, other, platform))
            for st in loc.get("steps", []):
                keys = set(st) & {"call", "rip", "check", "disp", "add", "div", "deref", "rva"}
                if len(keys) != 1:
                    e("%s: step %r must have exactly one of call/rip/check/disp/add/div/deref/rva" % (where, st))
                if "div" in st and (not isinstance(st["div"], int) or isinstance(st["div"], bool) or st["div"] <= 0):
                    e("%s: div step needs a positive integer" % where)
                if "rip" in st and "next" not in st:
                    e("%s: rip step needs 'next'" % where)
                if "disp" in st and st.get("size") not in (1, 2, 4, 8):
                    e("%s: disp step needs size in 1/2/4/8" % where)
            if ent.get("kind") == "offset" and not any("disp" in st for st in loc.get("steps", [])) and t != "base":
                e("%s: kind=offset needs a disp step" % where)
            for ref in loc.get("refs", []):
                if not {"at", "next", "string"} <= set(ref):
                    e("%s: ref needs at/next/string" % where)
        for b, v in ent.get("expect", {}).items():
            if not isinstance(v, int):
                e("%s: expect[%s] must be an integer" % (where, b))
    # every bridge entry that pins code needs a fingerprint in sigs/fingerprints/<module>-fp.json:
    # without one nothing on a new build says the locator still found the right function, so
    # sigupdate.py --bridge-gate calls it UNVERIFIED and `cargo bridge pins` would re-pin whatever the
    # locator resolved to. Same definition of "requires a fingerprint" as the gate's coverage:
    # fp_keys/required_fp_keys. Windows only: a `platform: linux` file has no bridge entries.
    if platform == "windows":
        fps = load_fps(d["module"])
        fname = fp_file(d["module"]).name
        listed = fps.get("_weak")
        if listed is not None and (not isinstance(listed, list)
                                   or any(not isinstance(w, str) for w in listed)
                                   or any(w not in fps for w in listed if isinstance(w, str))):
            e("sigs/fingerprints/%s: _weak must be a list of keys of that file" % fname)
        for key, pat in fps.items():
            # a fingerprint is evidence only if it can fail: an empty pattern, an all-wildcard one
            # or a token that is not hex would match anything (or raise) in fp_match
            if key == "_weak":
                continue
            try:
                _rx, vals = pattern(pat)
            except (ValueError, AttributeError, TypeError):
                e("sigs/fingerprints/%s: fingerprint %r is not a pattern (%r)" % (fname, key, pat))
                continue
            if all(v is None for v in vals):
                e("sigs/fingerprints/%s: fingerprint %r is all wildcards (it matches anything)"
                  % (fname, key))
            elif vals[0] is None:
                e("sigs/fingerprints/%s: fingerprint %r must not start with ??" % (fname, key))
        for name, ent in entries.items():
            for key in required_fp_keys(name, ent):
                if key not in fps:
                    e("entry %r pins code but has no fingerprint %r in sigs/fingerprints/%s"
                      % (name, key, fname))
            loc = ent.get("locator") if isinstance(ent.get("locator"), dict) else {}
            if not ent.get("bridge"):
                continue
            if loc.get("type") == "vslot":
                # pins.rs carries the value a bridge entry resolved to as the PinSlot target_rva and
                # the address its fingerprint is checked at: only a plain kind=function slot read
                # produces that (a value/dimension step would pin an integer, and any other kind
                # would be emitted as a slot number or crash on loc["class"])
                if ent.get("kind") != "function":
                    e("entry %r (bridge) reads a vslot but is kind=%s: `cargo bridge pins` would pin its value as "
                      "a slot target" % (name, ent.get("kind")))
                elif any(set(st) - {"check"} for st in loc.get("steps", [])):
                    e("entry %r (bridge) takes steps after its vslot read: `cargo bridge pins` pins the stepped "
                      "value as the slot target" % name)
                elif loc.get("vtable") in entries and not entries[loc["vtable"]].get("bridge"):
                    e("entry %r (bridge) reads its slot from %r, which has no bridge name: `cargo bridge pins` "
                      "would not pin that vtable" % (name, loc["vtable"]))
            # a bridge entry is only as good as the entry it is measured from (base/vtlen/vslot):
            # that entry must be on record for the build being pinned and must not have failed on it
            anchor = loc.get("base") if loc.get("type") == "base" else (
                loc.get("vtable") if loc.get("type") in ("vtlen", "vslot") else None)
            if anchor is not None and anchor in entries:
                a = entries[anchor]
                if a.get("status") == "stale":
                    e("entry %r (bridge) is measured from %r, which is stale: it failed on the newest "
                      "build, so nothing says the value it gives is right" % (name, anchor))
                elif CURRENT_BUILD not in a.get("expect", {}):
                    e("entry %r (bridge) is measured from %r, which has no expect[%s]: its value is not "
                      "on record for the build being pinned" % (name, anchor, CURRENT_BUILD))
    return errs


def cmd_validate(args):
    db = load_db(args.files)
    errs = []
    n = 0
    for path, d in db.items():
        errs += validate_file(path, d)
        n += len(d.get("entries", {}))
    for x in errs:
        print(x)
    print("%d files, %d entries, %d problems" % (len(db), n, len(errs)))
    return 1 if errs else 0


# vtlen self-test: vtable lengths that are known independently of the database (class layouts
# from the sigs worknotes). Only these two are checked; every DB entry that uses the vtlen
# locator is checked by `resolve`.
VT_SELFTEST = [
    ("schemasystem.dll", "CSchemaSystem", 40),
    ("schemasystem.dll", "CSchemaSystemTypeScope", 35),
]

# Linux: the code-pointer scan against the length the `_ZTV` symbol records (st_size/8 - 2). They
# agree except where the table's last slot is a null (a pure virtual the linker left at 0 instead
# of a thunk): IKeyValuesDumpContextAsText is the known case, which is why a symbolized vtable
# uses `measure: "symbol"` (s2sdk S2 report). (module, class, scan, symbol)
VT_SELFTEST_LINUX = [
    ("libtier0.so", "CStdIOFormatOutputStream", 17, 17),
    ("libtier0.so", "CKeyValuesDumpContextAsDevMsg", 5, 5),
    ("libtier0.so", "IKeyValuesDumpContextAsText", 4, 5),
]


def div_selftest():
    """The `div` step on a hand-built image: divisible values divide, the rest are refused, and a
    step list without a prior disp (no value yet) is refused too. Returns the number of problems."""
    r = Resolver(b"\x00" * 8 + b"\x20\x24", {"base": 0, "sections": []})
    r.rd = lambda rva, size: int.from_bytes(r.data[rva:rva + size], "little", signed=True)
    bad = 0
    for label, cur, steps, want in [
        ("disp 0x20 / 8", 8, [{"disp": 0, "size": 1}, {"div": 8}], 4),
        ("disp 0x24 / 8 (not a multiple)", 8, [{"disp": 1, "size": 1}, {"div": 8}], None),
        ("div without a value", 8, [{"div": 8}], None),
    ]:
        try:
            got = r.steps(cur, steps, "selftest")
        except LookupError:
            got = None
        ok = got == want
        print("div step %-34s got %-5s want %-5s %s" % (label, got, want, "ok" if ok else "FAIL"))
        bad += 0 if ok else 1
    return bad


def cmd_selftest(args):
    """check the vtlen scan against known vtable lengths in $CS2_BINS (needs the binaries)"""
    bad = div_selftest()
    for mod, cls, want in VT_SELFTEST:
        binary = module_path(mod)
        data = binary.read_bytes()
        img = parse_pe(data)
        try:
            vt = rtti_vtable(data, img, cls)
            got = vtable_len(data, img, vt)
        except (LookupError, ValueError) as ex:
            print("%-18s %-24s FAIL %s" % (mod, cls, ex))
            bad += 1
            continue
        ok = got == want
        print("%-18s %-24s vt %#x  len %3d  want %3d  %s" % (mod, cls, vt, got, want, "ok" if ok else "FAIL"))
        bad += 0 if ok else 1
    linux = Path(os.environ.get("CS2_LINUX_BINS", ROOT / "target/game-bins-linux-2000930"))
    for mod, cls, want_scan, want_sym in VT_SELFTEST_LINUX:
        binary = module_path(mod, root=linux, platform="linux")
        if not binary.exists():
            print("%-18s %-24s skipped (%s is not staged)" % (mod, cls, binary))
            continue
        data = binary.read_bytes()
        img = parse_elf(data)
        sym = elf_vtable_symbol(cls)
        try:
            got = vtable_len(data, img, img["symbols"][sym][0] + 16)
        except (KeyError, LookupError, ValueError) as ex:
            print("%-18s %-24s FAIL %s" % (mod, cls, ex))
            bad += 1
            continue
        symbol_len = img["symbols"][sym][1] // 8 - 2
        ok = got == want_scan and symbol_len == want_sym
        print("%-18s %-24s scan %3d  symbol %3d  want %3d/%3d  %s"
              % (mod, cls, got, symbol_len, want_scan, want_sym, "ok" if ok else "FAIL"))
        bad += 0 if ok else 1
    print("%d vtlen checks + 3 div-step checks, %d problems" % (len(VT_SELFTEST) + len(VT_SELFTEST_LINUX), bad))
    return 1 if bad else 0


def build_of(d, binary):
    """build tag of the binary per the file's 'builds' table (sha256 prefix -> tag), else None"""
    h = sha16(binary)
    tags = [tag for tag, info in d.get("builds", {}).items() if info.get("sha256") == h]
    if CURRENT_BUILD in tags:        # an unchanged module carries several builds; the current one wins
        return CURRENT_BUILD
    return tags[-1] if tags else None


def cmd_resolve(args):
    platform = args.platform
    root = Path(args.tree).resolve() if args.tree else BINS
    db = load_db(args.files, platform=platform)
    if args.module:
        db = {p: d for p, d in db.items() if d["module"] == args.module}
    if args.record:
        # the side door around the bridge gate: `cargo bridge pins` only re-checks builds[BUILD].sha256, so a
        # build recorded here would be pinned with whatever the locators found, gate or no gate.
        # Only sigupdate.py record may write a file with bridge entries (it refuses while any of
        # them is not SAME/OK). Refused up front for the whole run, so a refused run cannot leave
        # some files recorded and others untouched.
        blocked = [(p, d) for p, d in db.items()
                   if any(ent.get("bridge") for ent in d["entries"].values())]
        for _p, d in blocked:
            bridge = [n for n, ent in d["entries"].items() if ent.get("bridge")]
            print("%s: REFUSE --record: %d bridge entries (%s%s) are pinned in pins.rs; their build must be "
                  "recorded by sigs/scripts/sigupdate.py record (the bridge gate), not here"
                  % (d["module"], len(bridge), ", ".join(bridge[:4]), ", ..." if len(bridge) > 4 else ""))
        if blocked:
            print("nothing was written: %d of the %d file(s) selected pin the bridge; use "
                  "sigs/scripts/sigupdate.py record" % (len(blocked), len(db)))
            return 1
    bad = 0
    for path, d in db.items():
        mod = d["module"]
        binary = Path(args.binary) if args.binary else module_path(mod, root=root, platform=platform)
        if not binary.exists():
            print("%s: binary %s missing, skipped" % (mod, binary))
            continue
        data = binary.read_bytes()
        img = load_image(data)
        build = args.record or build_of(d, binary)
        if build is None and not args.record:
            print("== %s: binary sha256:%s is not in 'builds'; cannot compare expect (pass --record BUILD)" % (mod, sha16(binary)))
            if args.strict:
                bad += 1
                continue
        where = binary.relative_to(root) if binary.is_relative_to(root) else binary
        print("== %s (%s, sha256:%s, build %s)" % (mod, where, sha16(binary), build or "unknown"))
        r = Resolver(data, img)
        ok = stale = 0
        for name, ent in d["entries"].items():
            try:
                v = r.resolve(name, ent, d["entries"])
            except (LookupError, ValueError, IndexError, AssertionError) as ex:
                if ent.get("status") == "stale" and not ent.get("bridge"):
                    # known broken since the last update (sigupdate.py record); the bridge does not use it
                    print("  STALE %-49s %s   via: %s" % (name, ex, ent.get("via", "?")))
                    stale += 1
                    continue
                print("  FAIL %-50s %s   via: %s" % (name, ex, ent.get("via", "?")))
                bad += 1
                continue
            want = ent.get("expect", {}).get(build) if build else None
            fmt = "%#x" if ent["kind"] != "offset" and ent["kind"] != "const" else "%d (%#x)"
            shown = fmt % ((v, v) if "%d" in fmt else v)
            if want is not None and want != v:
                print("  DIFF %-50s got %s, expect[%s]=%#x" % (name, shown, build, want))
                bad += 1
            else:
                ok += 1
                if args.verbose:
                    print("  ok   %-50s %s" % (name, shown))
            if args.record:
                prev = ent.get("expect", {}).get(args.record)
                if prev is not None and prev != v:
                    print("  REFUSE %-50s expect[%s] already %#x, locator now %#x (do not overwrite silently)"
                          % (name, args.record, prev, v))
                    bad += 1
                    continue
                ent.setdefault("expect", {})[args.record] = v
        if args.record:
            build_info = {"sha256": sha16(binary)}
            if img["kind"] == "pe":
                build_info["timestamp"] = img["ts"]
            d.setdefault("builds", {})[args.record] = build_info
            path.write_text(json.dumps(d, indent=1, ensure_ascii=False) + "\n")
            print("  recorded expect[%s] for %d entries" % (args.record, ok))
        print("  %d ok, %d stale (not used by the bridge), %d entries" % (ok, stale, len(d["entries"])))
    return 1 if (bad and (args.strict or not args.record)) else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sp = ap.add_subparsers(dest="cmd", required=True)
    p = sp.add_parser("validate"); p.add_argument("files", nargs="*"); p.set_defaults(fn=cmd_validate)
    p = sp.add_parser("resolve"); p.add_argument("files", nargs="*"); p.add_argument("--module")
    p.add_argument("--binary"); p.add_argument("--record", metavar="BUILD"); p.add_argument("--strict", action="store_true")
    p.add_argument("--platform", choices=PLATFORMS, default="windows",
                   help="which half of the database to resolve (default windows)")
    p.add_argument("--tree", help="binaries root for --platform linux (default $CS2_BINS)")
    p.add_argument("-v", "--verbose", action="store_true"); p.set_defaults(fn=cmd_resolve)
    sp.add_parser("selftest").set_defaults(fn=cmd_selftest)
    args = ap.parse_args()
    sys.exit(args.fn(args))


if __name__ == "__main__":
    main()
