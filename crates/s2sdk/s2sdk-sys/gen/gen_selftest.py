#!/usr/bin/env python3
"""gen.py's self-test suite, split out of gen.py so the generator itself
stays reviewable. Same directory, two entry points:

    python3 -I gen.py            ... the generator (`--self-test` runs this
                                     suite as part of gen.py's CLI)
    python3 -I gen_selftest.py   ... the suite alone

The suite is still gen.py's code in every other respect: it runs against the
module object of the gen.py that is *executing*:

  - `gen.py --self-test` imports gen_selftest and hands it its own module
    object (`sys.modules[__name__]`, `__main__` when run as a script), not a
    second import of gen.py;
  - run directly, gen_selftest imports gen.py itself (`_load_gen`).

That matters because the suite monkeypatches gen.py globals (sdk_head) whose
reader (locked_heads) is a function defined in gen.py and therefore reads its
own module's globals. A patch that lands on any other module object (e.g. on
gen_selftest's copies created by `_inherit`) is a silent no-op: the code under
test sees the unpatched value and checks that should fail would pass. Every
patch below therefore writes `gen.<GLOBAL> = ...` and restores it in a finally;
see self_test.

`-I` (isolated mode) puts neither script's directory on sys.path, so each file
computes its directory from __file__ and inserts it explicitly - the import of
a sibling under `python3 -I` has to be handled that way; there was no existing
sibling import in gen/ to copy the pattern from.
"""

from __future__ import annotations

import copy
import shutil
import subprocess
import sys
import types
from pathlib import Path


def _load_gen() -> types.ModuleType:
    """Import the sibling gen.py for a standalone run (`python3 -I
    gen_selftest.py`): isolated mode leaves the script directory off sys.path,
    so the generator is reachable only if gen_selftest inserts its own
    directory explicitly."""
    here = str(Path(__file__).resolve().parent)
    if str(here) not in sys.path:
        sys.path.insert(0, here)
    import gen
    return gen


def _inherit(gen: types.ModuleType) -> None:
    """Pull the host gen.py's public names into this module's globals, so the
    moved suite body keeps referring to parse_record_layouts, emit_c_header,
    GenError etc. by bare name (the move is otherwise verbatim). Functions and
    classes come by reference: the suite exercises exactly the code gen.py
    runs. A monkeypatched global is deliberately NOT touched through
    these copies - self_test writes `gen.<GLOBAL>` and gen.py's functions
    read the same attribute (module docstring)."""
    for name, value in vars(gen).items():
        if name.startswith("__"):
            continue
        globals()[name] = value


_LAYOUT_SAMPLE = """\
*** Dumping AST Record Layout
         0 | class S2Sample
         0 |   class S2Base (primary base)
         0 |     int m_base
         4 |   (S2Sample vtable pointer)
        12 |   const char * m_pszName
        20 |   char[8] m_szBuf
        24 |   union S2Sample::(anonymous at /sdk/x.h:9:2) 
        24 |     uint32 m_Index
        28 |     struct S2Sample::(unnamed at /sdk/x.h:12:3) m_Parts
        28:0-14 |       uint32 m_EntityIndex
        30:7-23 |       uint32 m_Serial
        32 |   int m_nTail
           | [sizeof=40, dsize=40, align=8,
           |  nvsize=40, nvalign=8]

*** Dumping AST Record Layout
         0 | struct S2Plain
         0 |   int m_a
         4 |   int m_b
           | [sizeof=8, dsize=8, align=4,
           |  nvsize=8, nvalign=4]

*** Dumping IRgen Record Layout
Record: CXXRecordDecl 0x1 </sdk/x.h:1:1> class S2Ignored definition
"""

# The same record as clang prints it for --target=x86_64-pc-windows-msvc: the
# vtable pointer reads "vftable pointer", the [sizeof=...] line has no `dsize=`,
# and MSVC packs the bitfields differently (m_Serial ends byte 3, not 4).
_LAYOUT_MSVC_SAMPLE = """\
*** Dumping AST Record Layout
         0 | class S2MSample
         0 |   (S2MSample vftable pointer)
         8 |   const char * m_pszName
        16 |   struct S2Parts::(unnamed at /sdk/x.h:12:3) m_Parts
    16:0-14 |     uint32 m_EntityIndex
    17:7-23 |     uint32 m_Serial
        20 |   int m_nTail
           | [sizeof=24, align=8,
           |  nvsize=24, nvalign=8]

*** Dumping AST Record Layout
         0 | class S2MDerived
         0 |   class S2MEmpty (base) (empty)
         0 |   (S2MDerived vftable pointer)
         8 |   int m_x
           | [sizeof=16, align=8,
           |  nvsize=16, nvalign=8]
"""

_VTABLE_SAMPLE = """\
Original map
Vtable for 'S2Iface' (7 entries).
   0 | offset_to_top (0)
   1 | S2Iface RTTI
       -- (S2Base, 0) vtable address --
   2 | bool S2Base::BaseOnly(int) [pure]
   3 | S2Iface::~S2Iface() [complete]
   4 | S2Iface::~S2Iface() [deleting]
   5 | void S2Iface::Ping(CBufferString &, bool) [pure]
   6 | bool S2Iface::IsOk() const

VTable indices for 'S2Iface' (5 entries).
   0 | bool S2Base::BaseOnly(int)
   1 | S2Iface::~S2Iface() [complete]
   2 | S2Iface::~S2Iface() [deleting]
   3 | void S2Iface::Ping(CBufferString &, bool)
   4 | bool S2Iface::IsOk() const
"""

# The same dump as clang prints it for `--target=x86_64-pc-windows-msvc`: the
# block says "VFTable", the class's RTTI pointer is entry 0 of the full block
# (there is no offset_to_top), the full block has exactly one entry more than the
# vftable (whose entries the indices block numbers from the address point), an
# overload group is packed in *reverse* declaration order, a destructor is a
# single `[scalar deleting]` entry, and a declaration can carry a Windows
# charset-macro spelling (`Create` -> `CreateA`).
_VFTABLE_MSVC_SAMPLE = """\
VFTable for 'S2MIface' (3 entries).
   0 | S2MIface RTTI
   1 | void S2MIface::Over(char *)
   2 | void S2MIface::Over(int)

VFTable indices for 'S2MIface' (2 entries).
   0 | void S2MIface::Over(char *)
   1 | void S2MIface::Over(int)

VFTable for 'S2MDtor' (3 entries).
   0 | S2MDtor RTTI
   1 | S2MDtor::~S2MDtor() [scalar deleting]
   2 | bool S2MDtor::CreateA(const char *, bool)

VFTable indices for 'S2MDtor' (2 entries).
   0 | S2MDtor::~S2MDtor() [scalar deleting]
   1 | bool S2MDtor::CreateA(const char *, bool)
"""

# A group clang laid out in declaration order: the generator has to refuse it
# rather than emit slots the MSVC rule (abi::slot) would disagree with.
_VFTABLE_MSVC_UNREVERSED_SAMPLE = """\
VFTable for 'S2MAsc' (3 entries).
   0 | S2MAsc RTTI
   1 | void S2MAsc::Over(int)
   2 | void S2MAsc::Over(char *)

VFTable indices for 'S2MAsc' (2 entries).
   0 | void S2MAsc::Over(int)
   1 | void S2MAsc::Over(char *)
"""

# this-adjusting thunk, a vbtable (virtual base) and a second vfptr (secondary
# base): shapes the IR does not model, and each one has to fail loudly.
_VFTABLE_MSVC_THUNK_SAMPLE = """\
VFTable indices for 'S2MThunk' (2 entries).
   0 | void S2MThunk::Ping()
   1 | void S2MThunk::Ping() [this adjustment: -8 non-virtual]
"""

_VFTABLE_MSVC_VBASE_SAMPLE = """\
VFTable indices for 'S2MVbase' (1 entry).
 -- accessible via vbtable index 1, vfptr at offset 0 --
   0 | void S2MVbase::Ping()

Virtual base offset offsets for 'S2MVbase' (1 entry).
   S2MBase | -56
"""

_VFTABLE_MSVC_SECTION_SAMPLE = """\
VFTable for 'S2MMulti' (2 entries).
   0 | S2MMulti RTTI

VFTable indices for 'S2MMulti' (2 entries).
 -- accessible via vfptr at offset 0 --
   0 | void S2MMulti::Ping()
 -- accessible via vfptr at offset 8 --
   0 | void S2MMulti::Pong()
"""

# The Itanium dump's `[complete]` destructor, i.e. not the MSVC scalar deleting one.
_VFTABLE_MSVC_DTOR_SAMPLE = """\
VFTable for 'S2MBad' (2 entries).
   0 | S2MBad RTTI
   1 | S2MBad::~S2MBad() [complete]

VFTable indices for 'S2MBad' (1 entry).
   0 | S2MBad::~S2MBad() [complete]
"""

# A class with a secondary base: clang prints the primary table named for the
# root of the primary chain, and the secondary base's table next to it
# (`VFTable for 'S2MB' in 'S2MC'`); only the indices block of S2MC lists its own
# virtuals.
_VFTABLE_MSVC_SECONDARY_SAMPLE = """\
VFTable for 'S2MA' in 'S2MC' (3 entries).
   0 | S2MC RTTI
   1 | void S2MA::Ping()
   2 | void S2MC::Own()

VFTable for 'S2MB' in 'S2MC' (2 entries).
   0 | S2MC RTTI
   1 | void S2MB::Pong()

VFTable indices for 'S2MC' (1 entry).
   1 | void S2MC::Own()
"""

# The same one level further down: E : C, C : A, B. Both tables are spelled with
# the whole chain, and the primary one is named for the chain's root (A), not for
# E's primary base (C).
_VFTABLE_MSVC_SECONDARY_3LEVEL_SAMPLE = """\
VFTable for 'S2MA' in 'S2MC' in 'S2ME' (4 entries).
   0 | S2ME RTTI
   1 | void S2MA::Ping()
   2 | void S2MC::Own()
   3 | void S2ME::Leaf()

VFTable for 'S2MB' in 'S2MC' in 'S2ME' (2 entries).
   0 | S2ME RTTI
   1 | void S2MB::Pong()

VFTable indices for 'S2ME' (1 entry).
   2 | void S2ME::Leaf()
"""

# Itanium's `VTable indices`: an override of a secondary base's virtual is an own
# entry of the derived class, indistinguishable from a new virtual by the dump.
_VTABLE_ITANIUM_SECONDARY_SAMPLE = """\
VTable indices for 'S2IC' (2 entries).
   1 | void S2IC::Pong()
   2 | void S2IC::Own()

VTable indices for 'S2IC1' (1 entries).
   1 | void S2IC1::Own()

VTable indices for 'S2IE' (1 entries).
   2 | void S2IE::Pong()
"""

# A class that declares no destructor while a base on the primary chain does: clang lists the
# implicit destructor (Itanium: the complete/deleting pair) as the class's first own entries.
_VTABLE_ITANIUM_IMPLICIT_DTOR_SAMPLE = """\
VTable indices for 'S2DB' (4 entries).
   1 | S2DB::~S2DB() [complete]
   2 | S2DB::~S2DB() [deleting]
   3 | void S2DB::Own()

VTable indices for 'S2DC' (3 entries).
   1 | S2DC::~S2DC() [complete]
   2 | S2DC::~S2DC() [deleting]
   3 | void S2DC::Own()

VTable indices for 'S2DE' (2 entries).
   1 | S2DE::~S2DE() [complete]
   2 | S2DE::~S2DE() [deleting]

VTable indices for 'S2DI' (3 entries).
   0 | void S2DI::Foo()
   1 | S2DI::~S2DI() [complete]
   2 | S2DI::~S2DI() [deleting]
"""

# A three-level inheritance chain: clang names the most-derived class's full
# table with the whole chain, `VFTable for 'A' in 'B' in 'C'`, and a block regex
# that accepts only one ` in '...'` drops it (the misleading "0 full blocks").
_VFTABLE_MSVC_3LEVEL_SAMPLE = """\
VFTable for 'S2MRoot' in 'S2MMid' in 'S2MLeaf' (4 entries).
   0 | S2MLeaf RTTI
   1 | void S2MRoot::Ping()
   2 | void S2MMid::Middle()
   3 | void S2MLeaf::End()

VFTable indices for 'S2MLeaf' (3 entries).
   0 | void S2MRoot::Ping()
   1 | void S2MMid::Middle()
   2 | void S2MLeaf::End()
"""

# An overriding declaration sits in the base's slot (its indices entry is the
# derived spelling of the override); a *new* overload of the same name takes the
# next free slot. The two declarations of `F` are not a two-entry group clang
# would have had to reverse - abi's `takes_new_slot` excludes the override.
_VFTABLE_MSVC_OVERRIDE_SAMPLE = """\
VFTable for 'S2MBase' in 'S2MOvri' (4 entries).
   0 | S2MOvri RTTI
   1 | void S2MBase::F(int)
   2 | void S2MOvri::F(int)
   3 | void S2MOvri::F(char *)

VFTable indices for 'S2MOvri' (3 entries).
   0 | void S2MBase::F(int)
   1 | void S2MOvri::F(int)
   2 | void S2MOvri::F(char *)
"""

# A derived class that declares no destructor of its own: MSVC still gives its
# implicit virtual destructor the base chain's destructor slot, and clang spells
# that indices entry `~<derived>() [scalar deleting]`. The IR keeps no
# isImplicit declaration, so the slot belongs to no declaration of the class -
# which only the implicit destructor may own.
_VFTABLE_MSVC_IMPLICIT_DTOR_SAMPLE = """\
VFTable for 'S2MBaseD' in 'S2MImp' (4 entries).
   0 | S2MImp RTTI
   1 | S2MBaseD::~S2MBaseD() [scalar deleting]
   2 | bool S2MBaseD::BaseThing()
   3 | void S2MImp::Ping()

VFTable indices for 'S2MImp' (3 entries).
   0 | S2MImp::~S2MImp() [scalar deleting]
   1 | bool S2MBaseD::BaseThing()
   2 | void S2MImp::Ping()
"""

# A leftover slot that is not the implicit destructor's: still refused.
_VFTABLE_MSVC_EXTRA_SLOT_SAMPLE = """\
VFTable for 'S2MExtra' (3 entries).
   0 | S2MExtra RTTI
   1 | void S2MExtra::ExtraWish()
   2 | void S2MExtra::Ping()

VFTable indices for 'S2MExtra' (2 entries).
   0 | void S2MExtra::ExtraWish()
   1 | void S2MExtra::Ping()
"""

# A dump with the scopes a real one has, for the qualified-name keying: a
# namespace (`A::Action`), a global declaration sharing its bare name
# (`Action`), a second namespace (`KHook::Action`, `KHook::IndexType_t`) and a
# nested typedef (`Widget::BaseClass`). The underlying types differ per
# declaration, so a lookup that lands on the wrong one is visible, not just a
# missing lookup.
_SCOPE_AST = {
    "kind": "TranslationUnitDecl",
    "inner": [
        {
            "kind": "NamespaceDecl",
            "name": "A",
            "inner": [
                {
                    "kind": "EnumDecl",
                    "name": "Action",
                    "loc": {"file": "a.h", "line": 3, "col": 1},
                    "fixedUnderlyingType": {"qualType": "uint8_t", "desugaredQualType": "unsigned char"},
                    "inner": [],
                }
            ],
        },
        {
            "kind": "EnumDecl",
            "name": "Action",
            "loc": {"file": "b.h", "line": 5, "col": 1},
            "fixedUnderlyingType": {"qualType": "int"},
            "inner": [],
        },
        {
            "kind": "NamespaceDecl",
            "name": "KHook",
            "inner": [
                {
                    "kind": "EnumDecl",
                    "name": "Action",
                    "loc": {"file": "khook.hpp", "line": 9, "col": 1},
                    "fixedUnderlyingType": {"qualType": "unsigned char"},
                    "inner": [],
                },
                {
                    "kind": "TypedefDecl",
                    "name": "IndexType_t",
                    "loc": {"file": "khook.hpp", "line": 12, "col": 1},
                    "type": {"qualType": "unsigned int"},
                    "inner": [],
                },
            ],
        },
        {
            "kind": "CXXRecordDecl",
            "name": "Widget",
            "loc": {"file": "w.h", "line": 20, "col": 1},
            "completeDefinition": True,
            "inner": [
                {
                    "kind": "TypedefDecl",
                    "name": "BaseClass",
                    "loc": {"file": "w.h", "line": 22, "col": 3},
                    "type": {"qualType": "int"},
                    "inner": [],
                }
            ],
        },
    ],
}

_AST_SAMPLE = {
    "kind": "TranslationUnitDecl",
    "inner": [
        {
            "kind": "CXXRecordDecl",
            "name": "First",
            "loc": {"offset": 10, "file": "a.h", "line": 3, "col": 7},
            "range": {"begin": {"offset": 10, "line": 3}, "end": {"offset": 40, "line": 9}},
            "completeDefinition": True,
            # a field whose type names the enum before the enum is dumped: a bare back-reference
            "inner": [
                {
                    "kind": "FieldDecl",
                    "name": "m_x",
                    "inner": [
                        {"kind": "EnumType", "decl": {"id": "0x2", "kind": "EnumDecl", "name": "Third"}}
                    ],
                }
            ],
        },
        {
            "kind": "CXXRecordDecl",
            "name": "Second",
            "loc": {"offset": 50, "line": 30, "col": 7},
            "completeDefinition": True,
            "inner": [],
        },
        {
            "kind": "EnumDecl",
            "name": "Third",
            "loc": {"file": "c.h", "line": 12, "col": 6},
            "inner": [],
        },
        {
            "kind": "RecordDecl",
            "name": "ForwardOnly",
            "loc": {"offset": 70, "line": 40, "col": 8},
            "inner": [],
        },
        {
            "kind": "CXXRecordDecl",
            "name": "MacroDecl",
            "loc": {
                "spellingLoc": {"offset": 5, "file": "platform.h", "line": 46, "col": 30},
                "expansionLoc": {"offset": 9, "file": "d.h", "line": 20, "col": 9},
            },
            "completeDefinition": True,
            "inner": [],
        },
    ],
}


def self_test(gen: types.ModuleType | None = None) -> int:
    """Run the suite against the gen module `gen`; imports gen.py when None.

    `gen` must be the module object of the gen.py the caller executes, because
    the monkeypatches below (sdk_head) write attributes on it
    and gen.py's emit functions read the same attributes from their own module:
    a patch on any other module object is a silent no-op (module docstring)."""
    if gen is None:
        gen = _load_gen()
    _inherit(gen)
    checks = 0

    def ok(condition, what):
        nonlocal checks
        if not condition:
            raise GenError(f"self-test failed: {what}")
        checks += 1

    # record layouts -------------------------------------------------------- #
    layouts = parse_record_layouts(_LAYOUT_SAMPLE)
    sample = layouts["S2Sample"]
    ok(sample.size == 40, f"S2Sample size {sample.size}")
    ok(sample.align == 8, f"S2Sample align {sample.align}")
    ok(sample.has_vptr, "S2Sample has a vtable pointer")
    ok([b.name for b in sample.bases] == ["S2Base"], "S2Sample base name")
    ok(sample.bases[0].offset == 0 and sample.bases[0].primary, "S2Sample base offset")
    ok(
        [m.name for m in sample.own_members]
        == ["m_pszName", "m_szBuf", None, "m_nTail"],
        f"S2Sample direct members {[m.name for m in sample.own_members]}",
    )
    ok(sample.own_members[0].offset == 12 and sample.own_members[0].type == "const char *", "member type/offset")
    ok(sample.own_members[1].type == "char[8]", "array member type")
    ok(sample.own_members[2].type.startswith("union S2Sample::(anonymous"), "anonymous member")
    parts = [m for m in sample.members if m.name == "m_Parts"]
    ok(len(parts) == 1 and parts[0].offset == 28, "nested struct member offset")
    bits = [m for m in sample.members if m.name == "m_EntityIndex"]
    ok(len(bits) == 1 and bits[0].bit_offset == 28 * 8 and bits[0].bit_width == 15, "bitfield offset/width")
    bits = [m for m in sample.members if m.name == "m_Serial"]
    ok(bits[0].bit_offset == 30 * 8 + 7 and bits[0].bit_width == 17, "bitfield across bytes")
    ok("S2Ignored" not in layouts, "IRgen blocks are ignored")
    ok(layouts["S2Plain"].own_members[1].offset == 4, "plain record offsets")

    # the same, as clang prints it for the MSVC target ----------------------- #
    msvc = parse_record_layouts(_LAYOUT_MSVC_SAMPLE)
    msample = msvc["S2MSample"]
    ok(msample.size == 24 and msample.align == 8, f"S2MSample size/align {msample.size}/{msample.align}")
    ok(msample.has_vptr, "'vftable pointer' is a vtable pointer")
    ok(
        [m.name for m in msample.own_members] == ["m_pszName", "m_Parts", "m_nTail"]
        and msample.own_members[0].offset == 8,
        f"S2MSample direct members {[(m.name, m.offset) for m in msample.own_members]}",
    )
    mbits = {m.name: m for m in msample.members if m.bit_width is not None}
    ok(
        mbits["m_EntityIndex"].bit_offset == 16 * 8
        and mbits["m_EntityIndex"].bit_width == 15,
        "MSVC first bitfield",
    )
    ok(
        mbits["m_Serial"].bit_offset == 17 * 8 + 7 and mbits["m_Serial"].bit_width == 17,
        "MSVC packed bitfield (ends in byte 3, not byte 4)",
    )
    mderived = msvc["S2MDerived"]
    ok(
        [b.name for b in mderived.bases] == ["S2MEmpty"]
        and mderived.bases[0].offset == 0
        and not mderived.bases[0].virtual,
        "MSVC '(base) (empty)' base line",
    )
    ok(mderived.has_vptr and mderived.size == 16, "S2MDerived size/vptr")

    # vtables --------------------------------------------------------------- #
    tables = parse_vtables(_VTABLE_SAMPLE)
    indices = tables["S2Iface"]["indices"]
    ok([e.index for e in indices] == [0, 1, 2, 3, 4], "slot numbering")
    ok(
        [e.name for e in indices] == ["BaseOnly", "~S2Iface", "~S2Iface", "Ping", "IsOk"],
        f"entry names {[e.name for e in indices]}",
    )
    ok(indices[0].owner == "S2Base" and indices[0].name == "BaseOnly", "inherited entry owner")
    ok(indices[1].is_dtor and indices[2].is_dtor, "dtor slots")
    ok(indices[3].name == "Ping" and indices[3].owner == "S2Iface", "own entry")
    ok(indices[4].name == "IsOk", "const method name")
    ok("[pure]" in tables["S2Iface"]["layout"][5].markers, "markers captured")
    ok(tables["S2Iface"]["layout"][2].text.startswith("bool S2Base::BaseOnly"), "layout entry text")
    ok(len(tables["S2Iface"]["layout"]) == 7, "layout block entries")
    ok(sum(1 for e in indices if e.owner == "S2Iface") == 4, "own entry count")

    # the entry's two halves: the MSVC pass tells overloads apart by signature
    entry = parse_vtable_entry(5, "void S2Iface::Ping(CBufferString &, bool) [pure]")
    ok(entry.owner == "S2Iface" and entry.name == "Ping", "entry owner/name")
    ok(entry.ret == "void" and entry.params == "(CBufferString &, bool)", "entry ret/params")
    ok(entry.signature == "void(CBufferString&,bool)", f"entry signature {entry.signature!r}")
    ok(entry.markers == ("[pure]",), "entry markers")
    entry = parse_vtable_entry(6, "const ChangeAccessorFieldPathIndexInfo_t *S2X::Path() const")
    ok(
        entry.ret == "const ChangeAccessorFieldPathIndexInfo_t *"
        and entry.signature == "constChangeAccessorFieldPathIndexInfo_t*()const",
        f"a const overload keeps its qualifier: {entry.signature!r}",
    )
    entry = parse_vtable_entry(0, "offset_to_top (0)")
    ok(entry.name == "offset_to_top" and entry.params is None, "a non-function slot")
    entry = parse_vtable_entry(1, "0 | S2Iface RTTI" if False else "S2Iface RTTI")
    ok(entry.name == "RTTI" and entry.owner is None, "the RTTI slot")

    # the MSVC vftable dump: blocks, overload groups, dtors and thunks -------- #
    blocks = parse_vtable_blocks(_VFTABLE_MSVC_SAMPLE)
    kinds = [(block.kind, block.name, block.in_, block.count) for block in blocks]
    ok(
        kinds
        == [
            ("layout", "S2MIface", None, 3),
            ("indices", "S2MIface", None, 2),
            ("layout", "S2MDtor", None, 3),
            ("indices", "S2MDtor", None, 2),
        ],
        f"MSVC blocks {kinds}",
    )

    def msvc_method(name, decl_index, signature, dtor=False, overrides=None):
        return {
            "name": name,
            "decl_index": decl_index,
            "is_dtor": dtor,
            "overrides": overrides,
            "cxx_signature": signature,
        }

    def msvc_vtable_of(sample, class_name, methods):
        return msvc_vtable(class_name, methods, None, parse_vtable_blocks(sample))

    slots, names, summary = msvc_vtable_of(
        _VFTABLE_MSVC_SAMPLE,
        "S2MIface",
        [msvc_method("Over", 0, "void (int)"), msvc_method("Over", 1, "void (char *)")],
    )
    ok(slots == {0: 1, 1: 0}, f"the MSVC overload group is reversed: {slots}")
    ok(names == {}, f"an unrenamed declaration records no msvc_name: {names}")
    ok(
        summary["full_entries"] == 3 and summary["length"] == 2 and summary["own_entries"] == 2,
        f"MSVC full/own entry counts {summary}",
    )
    slots, names, summary = msvc_vtable_of(
        _VFTABLE_MSVC_SAMPLE,
        "S2MDtor",
        [
            msvc_method("~S2MDtor", 0, "void ()", dtor=True),
            msvc_method("Create", 1, "bool (const char *, bool)"),
        ],
    )
    ok(slots == {0: 0, 1: 1}, f"the MSVC destructor is one slot: {slots}")
    ok(names == {1: "CreateA"}, f"the macro-renamed spelling is kept: {names}")

    def fails(call) -> bool:
        try:
            call()
        except GenError:
            return True
        return False

    ok(
        fails(
            lambda: msvc_vtable_of(
                _VFTABLE_MSVC_UNREVERSED_SAMPLE,
                "S2MAsc",
                [msvc_method("Over", 0, "void (int)"), msvc_method("Over", 1, "void (char *)")],
            )
        ),
        "an overload group in declaration order is refused",
    )
    ok(
        fails(
            lambda: msvc_vtable_of(
                _VFTABLE_MSVC_THUNK_SAMPLE, "S2MThunk", [msvc_method("Ping", 0, "void ()")]
            )
        ),
        "a this-adjusting thunk is refused",
    )
    ok(
        fails(
            lambda: msvc_vtable_of(
                _VFTABLE_MSVC_VBASE_SAMPLE, "S2MVbase", [msvc_method("Ping", 0, "void ()")]
            )
        ),
        "a vbtable (virtual base) is refused",
    )
    ok(
        fails(
            lambda: msvc_vtable_of(
                _VFTABLE_MSVC_SECTION_SAMPLE,
                "S2MMulti",
                [msvc_method("Ping", 0, "void ()"), msvc_method("Pong", 1, "void ()")],
            )
        ),
        "an override of a secondary-base virtual (a second vfptr) is refused",
    )
    # An override of a secondary base's virtual (Itanium: an own entry of the
    # derived class, MSVC: the second vfptr) is refused while the vtable is built,
    # directly or through the primary chain; a class that only adds virtuals
    # next to a secondary base is accepted, with the base's offset recorded.
    def vmethod(name, sig="void ()"):
        return {"kind": "CXXMethodDecl", "name": name, "virtual": True, "type": {"qualType": sig}}

    def vnode(name, base_names, *methods):
        return {
            "name": name,
            "bases": [{"type": {"qualType": f"class {b}"}} for b in base_names],
            "inner": list(methods),
        }

    ast_nodes = {
        "S2IA": vnode("S2IA", [], vmethod("Ping")),
        "S2IB": vnode("S2IB", [], vmethod("Pong")),
        "S2IC": vnode("S2IC", ["S2IA", "S2IB"], vmethod("Pong"), vmethod("Own")),
        "S2IC0": vnode("S2IC0", ["S2IA", "S2IB"]),
        "S2IC1": vnode("S2IC1", ["S2IA", "S2IB"], vmethod("Own")),
        "S2IE": vnode("S2IE", ["S2IC0"], vmethod("Pong")),
        "S2INoAst": vnode("S2INoAst", ["S2IA", "S2IGone"], vmethod("Own")),
    }
    ast_layouts = {
        name: RecordLayout(
            name=name,
            kind="class",
            size=16,
            align=8,
            has_vptr=True,
            bases=[
                LayoutBase(base, 8 * i, False, i == 0)
                for i, base in enumerate(
                    [b["type"]["qualType"].split()[-1] for b in node["bases"]]
                )
            ],
            members=[],
        )
        for name, node in ast_nodes.items()
    }
    ast_sink = AstSink()
    ast_sink.records = {name: (node, "s2.h", 1) for name, node in ast_nodes.items()}

    class SecondaryBuilder(Builder):
        def verify_source(self, name, header, line, group):
            pass

        def relative_header(self, file, name):
            return "s2.h"

        def build_method(self, class_name, decl, decl_index, is_dtor, overrides, slot):
            return {"name": decl["name"], "overrides": overrides, "itanium_slot": slot}

    secondary_tables = {
        name: {"indices": entries["indices"]}
        for name, entries in parse_vtables(_VTABLE_ITANIUM_SECONDARY_SAMPLE).items()
    }
    ast_builder = SecondaryBuilder(Path("."), Path("."), {"tier0": {"vtables": sorted(ast_nodes)}}, ast_layouts, secondary_tables, ast_sink, [])
    ok(
        fails(lambda: ast_builder.build_vtable("S2IC", "g")),
        "an override of a secondary-base virtual is refused (Itanium: an own entry)",
    )
    ok(
        fails(lambda: ast_builder.build_vtable("S2IE", "g")),
        "an override of a virtual of a secondary base of the primary chain is refused",
    )
    ok(
        fails(lambda: ast_builder.build_vtable("S2INoAst", "g")),
        "a secondary base with no complete declaration is refused, not skipped",
    )
    accepted = ast_builder.build_vtable("S2IC1", "g")
    ok(
        accepted["primary_base"] == "S2IA"
        and accepted["secondary_bases"] == [{"name": "S2IB", "offset": 8}]
        and [(m["name"], m["itanium_slot"]) for m in accepted["methods"]] == [("Own", 1)],
        f"new virtuals next to a secondary base are accepted: {accepted}",
    )
    unlisted_builder = SecondaryBuilder(Path("."), Path("."), {}, ast_layouts, secondary_tables, ast_sink, [])
    ok(
        fails(lambda: unlisted_builder.build_vtable("S2IC1", "g")),
        "a primary-chain class with virtuals that is not an allowlisted vtable is refused",
    )
    # the implicit destructor a class inherits is no method of its own
    def vdtor(name):
        return {"kind": "CXXDestructorDecl", "name": name, "virtual": True, "type": {"qualType": "void ()"}}

    dtor_nodes = {
        "S2DA": vnode("S2DA", [], vdtor("~S2DA")),
        "S2DB": vnode("S2DB", ["S2DA"], vmethod("Own")),
        "S2DC": vnode("S2DC", ["S2DA"], vdtor("~S2DC"), vmethod("Own")),
        "S2DE": vnode("S2DE", ["S2DA"]),
        "S2DF": vnode("S2DF", [], vmethod("Own")),
        "S2DH": vnode("S2DH", [], vmethod("Foo"), vdtor("~S2DH")),
        "S2DI": vnode("S2DI", ["S2DH"], vmethod("Foo")),
    }
    dtor_layouts = {
        name: RecordLayout(
            name=name, kind="class", size=8, align=8, has_vptr=True,
            bases=[LayoutBase(b["type"]["qualType"].split()[-1], 0, False, True) for b in node["bases"]],
            members=[],
        )
        for name, node in dtor_nodes.items()
    }
    dtor_sink = AstSink()
    dtor_sink.records = {name: (node, "s2.h", 1) for name, node in dtor_nodes.items()}
    dtor_tables = {
        name: {"indices": entries["indices"]}
        for name, entries in parse_vtables(_VTABLE_ITANIUM_IMPLICIT_DTOR_SAMPLE).items()
    }
    dtor_builder = SecondaryBuilder(Path("."), Path("."), {"tier0": {"vtables": sorted(dtor_nodes)}}, dtor_layouts, dtor_tables, dtor_sink, [])
    built = dtor_builder.build_vtable("S2DB", "g")
    ok(
        [(m["name"], m["itanium_slot"]) for m in built["methods"]] == [("Own", 3)],
        f"an implicit destructor over a base destructor is skipped: {built['methods']}",
    )
    built = dtor_builder.build_vtable("S2DC", "g")
    ok(
        [(m["name"], m["itanium_slot"], m["overrides"]) for m in built["methods"]]
        == [("~S2DC", 1, "S2DA::~S2DA"), ("Own", 3, None)],
        f"an explicit destructor still carries its override: {built['methods']}",
    )
    built = dtor_builder.build_vtable("S2DE", "g")
    ok(built["methods"] == [], "a class with only the implicit destructor has no methods")
    # the implicit destructor listed AFTER an override is skipped by the end-of-table pass
    built = dtor_builder.build_vtable("S2DI", "g")
    ok(
        [(m["name"], m["itanium_slot"]) for m in built["methods"]] == [("Foo", 0)],
        f"an implicit destructor listed after an override is skipped: {built['methods']}",
    )
    dtor_tables["S2DF"] = {"indices": list(dtor_tables["S2DB"]["indices"])}
    ok(
        fails(lambda: dtor_builder.build_vtable("S2DF", "g")),
        "a destructor entry with no virtual destructor on the primary chain is refused",
    )
    blocks = parse_vtable_blocks(_VFTABLE_MSVC_SECONDARY_SAMPLE)
    ok(
        msvc_full_block(blocks, "S2MC", "S2MA").name == "S2MA",
        "the primary table is picked out of a class with a secondary base",
    )
    slots, names, summary = msvc_vtable("S2MC", [msvc_method("Own", 0, "void ()")], "S2MA", blocks)
    ok(
        slots == {0: 1} and summary["length"] == 2,
        f"MSVC: the accepted secondary-base shape gives Own slot 1: {slots} {summary}",
    )
    blocks = parse_vtable_blocks(_VFTABLE_MSVC_SECONDARY_3LEVEL_SAMPLE)
    ok(
        msvc_full_block(blocks, "S2ME", "S2MC", "S2MA").name == "S2MA",
        "the primary table of a class whose primary base is itself derived is found by the chain root",
    )
    ok(
        fails(lambda: msvc_full_block(blocks, "S2ME", "S2MC")),
        "...and is not found by the primary base's own name",
    )
    chain_ir = {
        "g": {
            "vtables": [
                {"class": "S2MA", "primary_base": None},
                {"class": "S2MC", "primary_base": "S2MA"},
                {"class": "S2ME", "primary_base": "S2MC"},
            ]
        }
    }
    ok(
        primary_chain_root(chain_ir, "S2MC") == "S2MA"
        and primary_chain_root(chain_ir, "S2MA") == "S2MA"
        and primary_chain_root(chain_ir, None) is None,
        "the root of a primary chain is found by walking primary_base",
    )
    # (f) a chain class that is no IR vtable (a class without virtuals of its own, a class
    # template): its layout names the next class; a base off offset 0 or a virtual base ends it
    root_ir = {"g": {"vtables": [{"class": "S2MA", "primary_base": None}]}}
    root_layouts = {
        "S2MT": RecordLayout("S2MT", "class", 8, 8, True, [LayoutBase("S2MA", 0, False, True)], []),
        "S2MU": RecordLayout("S2MU", "class", 8, 8, True, [LayoutBase("S2MT", 0, False, True)], []),
        "S2MV": RecordLayout("S2MV", "class", 16, 8, True, [LayoutBase("S2MA", 8, False, False)], []),
        "S2MW": RecordLayout("S2MW", "class", 16, 8, True, [LayoutBase("S2MA", 0, True, False)], []),
    }
    ok(
        primary_chain_root(root_ir, "S2MT", root_layouts) == "S2MA"
        and primary_chain_root(root_ir, "S2MU", root_layouts) == "S2MA",
        "the chain root is found through classes that are no IR vtable, by their layouts",
    )
    ok(
        primary_chain_root(root_ir, "S2MT") == "S2MT"
        and primary_chain_root(root_ir, "S2MV", root_layouts) == "S2MV"
        and primary_chain_root(root_ir, "S2MW", root_layouts) == "S2MW",
        "...and without layouts, or past a base off offset 0 or a virtual base, the chain ends there",
    )
    # (a) a secondary vfptr section that holds only a destructor is no refusal; a method
    # there still is
    dtor_only = msvc_indices_block(
        parse_vtable_blocks(
            "VFTable indices for 'S2MSd' (1 entry).\n"
            " -- accessible via vfptr at offset 0 --\n"
            "   0 | void S2MSd::Own()\n"
            " -- accessible via vfptr at offset 8 --\n"
            "   0 | S2MSd::~S2MSd() [scalar deleting]\n"
        ),
        "S2MSd",
    )
    ok(
        [e.name for e in dtor_only.entries] == ["Own"]
        and len(dtor_only.secondary_entries) == 1
        and dtor_only.secondary_entries[0].is_dtor,
        "a secondary vfptr section that only holds a destructor is accepted and kept apart",
    )
    ok(
        fails(
            lambda: msvc_indices_block(
                parse_vtable_blocks(
                    "VFTable indices for 'S2MSm' (1 entry).\n"
                    " -- accessible via vfptr at offset 0 --\n"
                    "   0 | void S2MSm::Own()\n"
                    " -- accessible via vfptr at offset 8 --\n"
                    "   0 | S2MSm::~S2MSm() [scalar deleting]\n"
                    "   1 | void S2MSm::Pong()\n"
                ),
                "S2MSm",
            )
        ),
        "a secondary vfptr section with a destructor and a method is refused",
    )
    # (b) an override-only class on the primary chain (a template layer) is skipped: the
    # nearest allowlisted ancestor is the primary base the IR records
    chain_nodes = {
        "S2PA": vnode("S2PA", [], vmethod("Ping")),
        "S2PT": vnode("S2PT", ["S2PA"], vmethod("Ping")),
        "S2PD": vnode("S2PD", ["S2PT"], vmethod("Own")),
        "S2PN": vnode("S2PN", ["S2PA"], vmethod("Extra")),
        "S2PE": vnode("S2PE", ["S2PN"], vmethod("Own")),
    }
    chain_layouts = {
        name: RecordLayout(
            name, "class", 8, 8, True,
            [LayoutBase(b["type"]["qualType"].split()[-1], 0, False, True) for b in node["bases"]],
            [],
        )
        for name, node in chain_nodes.items()
    }
    chain_sink = AstSink()
    chain_sink.records = {name: (node, "s2.h", 1) for name, node in chain_nodes.items()}
    chain_builder = SecondaryBuilder(
        Path("."), Path("."), {"tier0": {"vtables": ["S2PA", "S2PD", "S2PE"]}}, chain_layouts, {}, chain_sink, []
    )
    ok(
        chain_builder.vtable_primary_base("S2PD", "S2PT") == "S2PA"
        and chain_builder.vtable_primary_base("S2PD", "S2PA") == "S2PA",
        "an override-only class between a vtable and its listed ancestor is skipped",
    )
    ok(
        fails(lambda: chain_builder.vtable_primary_base("S2PE", "S2PN")),
        "...but a class between that declares a virtual of its own is refused",
    )
    # (c) a base subobject that sits elsewhere under MSVC: merge_msvc records the offsets
    # only where they differ, abi_view applies them to the MSVC view only
    base_group_ir = {
        group: {"records": [], "specs": [], "enums": [], "consts": [], "vtables": []}
        for group in GROUPS
    }

    def base_record(name):
        return {
            "name": name, "size": 16, "align": 8, "pass_in_registers": False, "fields": [],
            "bases": [{"name": "S2BA", "offset": 0}, {"name": "S2BB", "offset": 8}],
        }

    base_group_ir["tier0"]["records"] = [base_record("S2BMoved"), base_record("S2BSame")]

    def base_facts(moved):
        return MsvcFacts(
            records={
                "S2BMoved": {"size": 24, "align": 8, "fields": [], "base_offsets": moved},
                "S2BSame": {"size": 16, "align": 8, "fields": [], "base_offsets": [0, 8]},
            },
            enums={}, slots={}, names={}, vtables={}, specs={}, consts={}, bases={},
        )

    merge_msvc(base_group_ir, base_facts([0, 16]))
    moved, same = base_group_ir["tier0"]["records"]
    ok(
        moved["msvc"]["base_offsets"] == [0, 16] and "base_offsets" not in same["msvc"],
        "merge_msvc records base_offsets only for the record whose MSVC offsets differ",
    )
    ok(
        [b["offset"] for b in abi_view(moved, ABI_MSVC)["bases"]] == [0, 16]
        and [b["offset"] for b in abi_view(moved, "itanium")["bases"]] == [0, 8]
        and [b["offset"] for b in abi_view(same, ABI_MSVC)["bases"]] == [0, 8],
        "abi_view applies base_offsets to the MSVC view only",
    )
    ok(
        fails(lambda: merge_msvc(base_group_ir, base_facts([0]))),
        "merge_msvc refuses a base count that differs between the layouts",
    )
    # (d) an integral non-type template argument is spelled into the specialization name
    ok(
        specialization_name(
            {
                "name": "S2Tmpl",
                "inner": [
                    {"kind": "TemplateArgument", "type": {"qualType": "IToolSystem2"}},
                    {"kind": "TemplateArgument", "value": 0},
                ],
            }
        )
        == "S2Tmpl<IToolSystem2, 0>"
        and specialization_name(
            {"name": "S2Tmpl", "inner": [{"kind": "TemplateArgument", "type": {"qualType": "int"}}]}
        )
        == "S2Tmpl<int>"
        and specialization_name({"name": "S2Tmpl", "inner": [{"kind": "TemplateArgument"}]}) is None,
        "specialization_name spells an integral non-type argument and refuses an unreadable one",
    )
    slots, names, summary = msvc_vtable(
        "S2ME", [msvc_method("Leaf", 0, "void ()")], "S2MC", blocks, "S2MA"
    )
    ok(slots == {0: 2}, f"MSVC: a leaf over a class with a secondary base: {slots}")
    ok(
        fails(
            lambda: msvc_vtable_of(
                _VFTABLE_MSVC_DTOR_SAMPLE,
                "S2MBad",
                [msvc_method("~S2MBad", 0, "void ()", dtor=True)],
            )
        ),
        "a destructor that is not [scalar deleting] is refused",
    )
    ok(
        fails(
            lambda: msvc_vtable_of(
                _VFTABLE_MSVC_SAMPLE, "S2MIface", [msvc_method("Nope", 0, "void ()")]
            )
        ),
        "a declaration with no MSVC entry is refused",
    )
    ok(
        fails(
            lambda: msvc_vtable_of(
                _VFTABLE_MSVC_SAMPLE,
                "S2MNope",
                [msvc_method("Over", 0, "void (int)"), msvc_method("Over", 1, "void (char *)")],
            )
        ),
        "a class with no MSVC block is refused",
    )

    # F1: the most-derived class's full table is spelled with the whole chain.
    blocks = parse_vtable_blocks(_VFTABLE_MSVC_3LEVEL_SAMPLE)
    kinds = [(block.kind, block.name, block.in_, block.count) for block in blocks]
    ok(
        kinds
        == [
            ("layout", "S2MRoot", "S2MLeaf", 4),
            ("indices", "S2MLeaf", None, 3),
        ],
        f"the 3-level chain block parses, `in` is the most-derived class: {kinds}",
    )
    slots, names, summary = msvc_vtable(
        "S2MLeaf",
        [msvc_method("End", 0, "void ()")],
        "S2MMid",
        blocks,
    )
    ok(slots == {0: 2}, f"the 3-level leaf's own slot: {slots}")
    ok(
        summary["full_entries"] == 4 and summary["length"] == 3 and summary["own_entries"] == 1,
        f"the 3-level chain's entry counts {summary}",
    )
    ok(
        msvc_vtable("S2MLeaf", [], "S2MMid", blocks) == ({}, {}, {"own_entries": 0, "full_entries": None, "length": None, "slots": {}}),
        "a 3-level class with no own virtuals still skips the MSVC pass",
    )
    # without the multi-level chain, the same dump is unparsable: reproduce the
    # old failure shape with a chain the regex would have to reject
    ok(
        fails(
            lambda: msvc_full_block(
                parse_vtable_blocks(
                    _VFTABLE_MSVC_3LEVEL_SAMPLE.replace(
                        "VFTable for 'S2MRoot' in 'S2MMid' in 'S2MLeaf' (4 entries).",
                        "VFTable for 'S2MRoot' in 'S2MMid' in 'S2MLeaf' (4 entries). extra",
                    )
                ),
                "S2MLeaf",
                "S2MMid",
            )
        ),
        "a full block line that is not a block header is not matched",
    )

    # F2: override + new overload of the same name is not an unreversed group.
    slots, names, summary = msvc_vtable(
        "S2MOvri",
        [
            msvc_method("F", 0, "void (int)", overrides="S2MBase::F"),
            msvc_method("F", 1, "void (char *)"),
        ],
        "S2MBase",
        parse_vtable_blocks(_VFTABLE_MSVC_OVERRIDE_SAMPLE),
    )
    ok(
        slots == {0: 1, 1: 2},
        f"the override reuses the base's slot, the new overload takes the next: {slots}",
    )
    ok(summary["own_entries"] == 2, f"both own entries matched: {summary}")

    # F3: a class without a declared virtual destructor still has the implicit
    # one in clang's dump; its slot belongs to no declaration of the IR.
    slots, names, summary = msvc_vtable(
        "S2MImp",
        [msvc_method("Ping", 0, "void ()")],
        "S2MBaseD",
        parse_vtable_blocks(_VFTABLE_MSVC_IMPLICIT_DTOR_SAMPLE),
    )
    ok(
        slots == {0: 2},
        f"the implicit destructor's slot is not a declaration's: {slots}",
    )
    ok(
        summary["own_entries"] == 1 and summary["length"] == 3,
        f"the implicit-destructor class's summary {summary}",
    )
    ok(
        fails(
            lambda: msvc_vtable(
                "S2MExtra",
                [msvc_method("Ping", 0, "void ()")],
                None,
                parse_vtable_blocks(_VFTABLE_MSVC_EXTRA_SLOT_SAMPLE),
            )
        ),
        "a leftover slot that is not an implicit destructor is refused",
    )

    # F7: the predicted MSVC own layout, including an interleaved group and a
    # new-slot destructor - the fixture of abi.rs's overload_groups_both_abis.
    fixture = [
        msvc_method("A", 0, "void ()"),
        msvc_method("Ov", 1, "void (int)"),
        msvc_method("B", 2, "void ()"),
        msvc_method("Ov", 3, "void (char *)"),
        msvc_method("C", 4, "void ()"),
        msvc_method("Ov", 5, "void (double)"),
        msvc_method("~X", 6, "void ()", dtor=True),
    ]
    predicted = msvc_predicted_slots(fixture, 7)
    ok(
        predicted == {0: 0, 1: 3, 2: 4, 3: 2, 4: 5, 5: 1, 6: 6},
        f"predicted slots (A=0, Ov#0=3, Ov#1=2, Ov#2=1, B=4, C=5, ~X=6): {predicted}",
    )
    # the anchor is verified, not just the group's internal shape: a group laid
    # out reversed+contiguous but at the wrong offset is refused
    ok(
        fails(
            lambda: check_msvc_overload_groups(
                "S2MX",
                [
                    msvc_method("A", 0, "void ()"),
                    msvc_method("Ov", 1, "void (int)"),
                    msvc_method("Ov", 2, "void (char *)"),
                    msvc_method("B", 3, "void ()"),
                ],
                {0: 1, 1: 3, 2: 2, 3: 0},
                4,
            )
        ),
        "an overload group that sits at the wrong anchor is refused",
    )
    ok(
        not fails(
            lambda: check_msvc_overload_groups(
                "S2MX",
                [
                    msvc_method("A", 0, "void ()"),
                    msvc_method("Ov", 1, "void (int)"),
                    msvc_method("Ov", 2, "void (char *)"),
                    msvc_method("B", 3, "void ()"),
                ],
                {0: 0, 1: 2, 2: 1, 3: 3},
                4,
            )
        ),
        "a correctly anchored reversed group is accepted",
    )

    def layout_with(**kwargs) -> RecordLayout:
        base = dict(name="S2MRec", kind="struct", size=16, align=8, has_vptr=False, bases=[], members=[])
        base.update(kwargs)
        return RecordLayout(**base)

    ok(not fails(lambda: check_msvc_layout("S2MRec", layout_with())), "an ordinary layout passes")
    ok(
        not fails(
            lambda: check_msvc_layout(
                "S2MRec",
                layout_with(
                    members=[
                        LayoutMember(level=1, offset=0, bit_from=None, bit_to=None, type="int", name="m_x")
                    ]
                ),
            )
        ),
        "a layout with a plain member passes",
    )
    ok(
        fails(
            lambda: check_msvc_layout(
                "S2MRec",
                layout_with(bases=[LayoutBase(name="S2MBase", offset=16, virtual=True, primary=False)]),
            )
        ),
        "an MSVC virtual base is refused",
    )
    ok(
        fails(
            lambda: check_msvc_layout(
                "S2MRec",
                layout_with(
                    members=[
                        LayoutMember(
                            level=1,
                            offset=0,
                            bit_from=None,
                            bit_to=None,
                            type="(S2MRec vbtable pointer)",
                            name=None,
                            is_vptr=True,
                        )
                    ]
                ),
            )
        ),
        "an MSVC vbtable pointer is refused",
    )

    # location replay ------------------------------------------------------- #
    sink = AstSink()
    ast_walk(_AST_SAMPLE, sink)
    ok(sink.records["First"][1:] == ("a.h", 3), f"First at {sink.records['First'][1:]}")
    ok(sink.records["Second"][1:] == ("a.h", 30), f"Second at {sink.records['Second'][1:]}")
    ok(sink.enums["Third"][1:] == ("c.h", 12), f"Third at {sink.enums['Third'][1:]} (a bare back-reference seen first is skipped)")
    ok(
        sink.records["MacroDecl"][1:] == ("d.h", 20),
        f"macro declaration at {sink.records['MacroDecl'][1:]} (the expansion location)",
    )
    ok("ForwardOnly" not in sink.records, "forward declarations are ignored")

    # rust types ------------------------------------------------------------ #
    known = {"Vector", "EConVarType", "ISchemaSystem", "CUtlString"}
    cases = {
        "const char *": "*const ::core::ffi::c_char",
        "char *": "*mut ::core::ffi::c_char",
        "const char **": "*mut *const ::core::ffi::c_char",
        "uint8": "u8",
        "uint16": "u16",
        "int": "i32",
        "uint": "u32",
        "vec_t": "f32",
        "bool": "bool",
        "Vector": "Vector",
        "EConVarType": "EConVarType",
        "Vector *": "*mut Vector",
        "const Vector *": "*const Vector",
        "Vector &": "*mut Vector",
        "const Vector &": "*const Vector",
        "void *": "*mut ::core::ffi::c_void",
        "void **": "*mut *mut ::core::ffi::c_void",
        "Foo *": "*mut ::core::ffi::c_void",
        "Foo **": "*mut *mut ::core::ffi::c_void",
        "unsigned char[4]": "[u8; 4]",
        "uint8[16]": "[u8; 16]",
        "int[4][4]": "[[i32; 4]; 4]",
        "Foo[8]": None,
        "void *(*)(const char *, int *)": "*const ::core::ffi::c_void",
        "CBufferString &": "*mut ::core::ffi::c_void",
    }
    for cxx, expected in cases.items():
        got = rust_type(cxx, known)
        ok(got == expected, f"rust_type({cxx!r}) = {got!r}, expected {expected!r}")

    # the AST-derived rules (6, 7 and 8) need the sugar tables --------------- #
    handle_layout = RecordLayout(
        name="SchemaMetaInfoHandle_t<CSchemaClassInfo>",
        kind="struct",
        size=8,
        align=8,
        has_vptr=False,
        bases=[],
        members=[
            LayoutMember(
                level=1,
                offset=0,
                bit_from=None,
                bit_to=None,
                type="class CSchemaClassInfo *",
                name="m_pObj",
            )
        ],
    )
    sugar_tables = dict(
        known={"CKV3MemberName", "CSchemaClassInfo", "SchemaTypeScope_t"},
        records={"CKV3MemberName", "CSchemaClassInfo"},
        enums={"SchemaTypeScope_t"},
        pass_in_registers={"CKV3MemberName": True, "CSchemaClassInfo": False},
        typedefs={
            "GameEventKeySymbol_t": "CKV3MemberName",
            "SpawnGroupHandle_t": "unsigned int",
            "CreateInterfaceFn": "void *(*)(const char *, int *)",
        },
        enum_underlying={"EntityFlags_t": "unsigned int", "InitReturnVal_t": None},
        record_sizes={"CKV3MemberName": 16, "CSchemaClassInfo": 112},
        enum_sizes={"SchemaTypeScope_t": 1},
        specializations={"SchemaMetaInfoHandle_t<CSchemaClassInfo>": True},
        spec_layouts={"SchemaMetaInfoHandle_t<CSchemaClassInfo>": handle_layout},
    )

    def sugar_mapper(**overrides) -> TypeMapper:
        tables = dict(sugar_tables)
        tables.update(overrides)
        return TypeMapper(**tables)

    sugar = sugar_mapper()
    sugar_cases = {
        "const GameEventKeySymbol_t &": "*const CKV3MemberName",
        "GameEventKeySymbol_t &": "*mut CKV3MemberName",
        "GameEventKeySymbol_t": "CKV3MemberName",
        "SpawnGroupHandle_t": "u32",
        "const SpawnGroupHandle_t": "u32",
        "EntityFlags_t": "u32",
        "InitReturnVal_t": None,
        "CreateInterfaceFn": "*const ::core::ffi::c_void",
        "const CreateInterfaceFn": "*const ::core::ffi::c_void",
        "SchemaMetaInfoHandle_t<CSchemaClassInfo>": "*mut CSchemaClassInfo",
        "SchemaTypeScope_t": "SchemaTypeScope_t",
    }
    for cxx, expected in sugar_cases.items():
        got = sugar.rust_type(cxx)
        ok(got == expected, f"sugar rust_type({cxx!r}) = {got!r}, expected {expected!r}")
    ok(sugar.rust_size("GameEventKeySymbol_t") == 16, "typedef size")
    ok(sugar.rust_size("SpawnGroupHandle_t") == 4, "scalar typedef size")
    ok(sugar.rust_size("uint8[16]") == 16, "array size")
    ok(sugar.rust_size("void **") == 8, "pointer size")
    # (7) a record that the ABI does not pass in registers cannot be a by-value
    # parameter or return, but it is still fine as a field.
    ok(sugar.rust_type("CSchemaClassInfo") == "CSchemaClassInfo", "by-value field")
    ok(sugar.rust_type("CSchemaClassInfo", signature=True) is None, "by-value signature")
    ok(
        sugar.rust_type("CKV3MemberName", signature=True) == "CKV3MemberName",
        "pass-in-registers signature",
    )
    ok(sugar.rust_type("CKV3MemberName *", signature=True) == "*mut CKV3MemberName", "behind a pointer is fine")
    # (8) the handle mapping is only allowed once the layout dump and the AST
    # both confirm it.
    def raises(mapper: TypeMapper, cxx: str) -> bool:
        try:
            mapper.rust_type(cxx)
        except GenError:
            return True
        return False

    handle = "SchemaMetaInfoHandle_t<CSchemaClassInfo>"
    ok(not raises(sugar, handle), "the verified handle maps")
    broken_layout = dict(sugar_tables["spec_layouts"])
    broken_layout[handle] = dataclasses.replace(handle_layout, size=16)
    ok(
        raises(sugar_mapper(spec_layouts=broken_layout), handle),
        "a handle whose layout is not one pointer is rejected",
    )
    ok(
        raises(sugar_mapper(specializations={}), handle),
        "a handle the AST does not describe is rejected",
    )

    # MSVC hidden return pointer ------------------------------------------- #
    # is_class_value: what MSVC returns through the hidden pointer.
    class_cases = {
        "CKV3MemberName": True,  # an allowlisted record
        "const CKV3MemberName": True,
        "CSchemaClassInfo": True,  # non-trivial too (rule 7 refuses it elsewhere)
        "GameEventKeySymbol_t": True,  # typedef of a record
        "SchemaMetaInfoHandle_t<CSchemaClassInfo>": True,  # a class that maps to `*mut T`
        "SchemaTypeScope_t": False,  # enum: RAX
        "SpawnGroupHandle_t": False,  # typedef of unsigned int
        "int": False,
        "CKV3MemberName *": False,
        "const CKV3MemberName &": False,
        "CreateInterfaceFn": False,  # function pointer
        "": False,
    }
    for cxx, expected in class_cases.items():
        got = sugar.is_class_value(cxx)
        ok(got == expected, f"is_class_value({cxx!r}) = {got}, expected {expected}")
    native = 'unsafe extern "C" fn(this: *mut C, a: i32, b: CKV3MemberName) -> CKV3MemberName'
    ok(
        msvc_hidden_return_fn(native, "C", True)
        == 'unsafe extern "C" fn(this: *mut C, ret: *mut CKV3MemberName, a: i32, b: CKV3MemberName)'
        " -> *mut CKV3MemberName",
        "a class return gets the hidden pointer right after `this` and returns it",
    )
    ok(
        msvc_hidden_return_fn(native.replace("a: i32", "ret: i32"), "C", True)
        == 'unsafe extern "C" fn(this: *mut C, ret_: *mut CKV3MemberName, ret: i32, b: CKV3MemberName)'
        " -> *mut CKV3MemberName",
        "the hidden pointer's name avoids the method's own parameter names",
    )
    ok(
        msvc_hidden_return_fn(native, "C", False) is None
        and msvc_hidden_return_fn(None, "C", True) is None
        and msvc_hidden_return_fn('unsafe extern "C" fn(this: *mut C)', "C", True) is None,
        "no hidden pointer for a non-class, an unmapped or a void method",
    )
    ok(
        msvc_hidden_return_fn('unsafe extern "C" fn(this: *mut C) -> *mut X', "C", True)
        == 'unsafe extern "C" fn(this: *mut C, ret: *mut *mut X) -> *mut *mut X',
        "a handle class (`*mut X`) is returned through `*mut *mut X`",
    )
    handle_method: dict = {"is_dtor": False, "rust_fn": native}
    attach_msvc_rust_fn(
        handle_method,
        "C",
        {"type": {"qualType": "GameEventKeySymbol_t (int, GameEventKeySymbol_t)"}},
        sugar,
    )
    plain_method: dict = {"is_dtor": False, "rust_fn": native}
    attach_msvc_rust_fn(plain_method, "C", {"type": {"qualType": "int (int)"}}, sugar)
    enum_method: dict = {"is_dtor": False, "rust_fn": native}
    attach_msvc_rust_fn(enum_method, "C", {"type": {"qualType": "SchemaTypeScope_t ()"}}, sugar)
    ok(
        "msvc_rust_fn" in handle_method
        and "msvc_rust_fn" not in plain_method
        and "msvc_rust_fn" not in enum_method,
        "attach_msvc_rust_fn sets the key only for a class return",
    )
    # A record rule 7 refuses by value (not passed in registers, e.g. a user copy
    # ctor) has no native `rust_fn`, but MSVC's hidden-pointer return still exists.
    nontrivial_decl = {
        "name": "F",
        "type": {"qualType": "CSchemaClassInfo (int)"},
        "inner": [{"kind": "ParmVarDecl", "name": "n"}],
    }
    nontrivial_method: dict = {"is_dtor": False, "rust_fn": build_rust_fn("C", nontrivial_decl, sugar)}
    attach_msvc_rust_fn(nontrivial_method, "C", nontrivial_decl, sugar)
    ok(
        nontrivial_method["rust_fn"] is None
        and nontrivial_method.get("msvc_rust_fn")
        == 'unsafe extern "C" fn(this: *mut C, ret: *mut CSchemaClassInfo, n: i32) -> *mut CSchemaClassInfo',
        "a non-trivial class return keeps no native form but gets the MSVC hidden-pointer one",
    )
    # The conventions themselves, read back from clang's IR (the same shapes
    # as target/g2-probe/p.cpp): an MSVC instance method returns every class
    # through `sret` as the *second* parameter (4, 8, 16, 24 bytes, trivial or
    # not); Itanium returns a trivially copyable <= 16-byte record in
    # registers (the native by-value Rust form) and puts the sret of a larger
    # or non-trivial one FIRST, before `this` (also what Rust's extern "C"
    # does on Linux, so rust_fn needs no hidden form there).
    clang = shutil.which("clang++")
    if clang:
        probe = (
            "struct S4{unsigned v;};struct S8{const void*p;};struct S16{unsigned long long a,b;};"
            "struct S24{unsigned long long a,b,c;};struct NT{NT(const NT&);~NT();void*p;};"
            "enum class En:int{a};struct I{virtual S4 f4(int);virtual S8 f8(int);"
            "virtual S16 f16(int);virtual S24 f24(int);virtual NT fnt(int);virtual En fen(int);};"
            "S4 I::f4(int){return {1};}S8 I::f8(int){return {0};}S16 I::f16(int){return {1,2};}"
            "S24 I::f24(int){return {1,2,3};}NT I::fnt(int){return *(NT*)0;}En I::fen(int){return En::a;}"
        )

        def ir_defines(target: str) -> dict[str, str]:
            run = subprocess.run(
                [clang, f"--target={target}", "-std=c++20", "-x", "c++", "-S", "-emit-llvm", "-O0", "-o", "-", "-"],
                input=probe,
                capture_output=True,
                text=True,
            )
            found = {}
            for line in run.stdout.splitlines():
                if not line.startswith("define"):
                    continue
                for name in ("f4", "f8", "f16", "f24", "fnt", "fen"):
                    if f"?{name}@I@@" in line or f"_ZN1I{len(name)}{name}Ei" in line:
                        found[name] = line
            return found

        msvc_ir = ir_defines("x86_64-pc-windows-msvc")
        itanium_ir = ir_defines("x86_64-linux-gnu")
        for name in ("f4", "f8", "f16", "f24", "fnt"):
            line = msvc_ir.get(name, "")
            args = line.split("(", 1)[-1]
            ok(
                line.startswith("define dso_local void") and args.index("sret") > args.index("%0")
                and args.index("sret") < args.index("i32"),
                f"clang: MSVC {name} returns through sret after `this`",
            )
        ok(
            msvc_ir.get("fen", "").startswith("define dso_local noundef i32"),
            "clang: an MSVC enum return stays in EAX",
        )
        ok(
            itanium_ir.get("f4", "").startswith("define dso_local i32")
            and itanium_ir.get("f8", "").startswith("define dso_local ptr")
            and itanium_ir.get("f16", "").startswith("define dso_local { i64, i64 }"),
            "clang: Itanium returns a trivial record of 4, 8, 16 bytes in registers",
        )
        for name in ("f24", "fnt"):
            line = itanium_ir.get(name, "")
            args = line.split("(", 1)[-1]
            ok(
                line.startswith("define dso_local void") and args.startswith("ptr dead_on_unwind noalias writable sret"),
                f"clang: Itanium {name} (over 16 bytes / non-trivial) has sret FIRST, before `this`",
            )

    # WP1.D: the return transport of a by-value class return ---------------- #
    # The four probe types of the rule, read back from clang's IR (-S
    # -emit-llvm): an MSVC instance method returns EVERY class through `sret`
    # as the second argument, right after `this`; Itanium returns a trivially
    # copyable record of at most 16 bytes in registers - an 8-byte integer pair
    # in RAX, a 12-byte float[3] in the SSE pair - and puts the sret of a
    # 24-byte record and of an 8-byte one with a user copy constructor FIRST,
    # before `this`. A float and an int share the first eightbyte, and INTEGER
    # wins it (AMD64 3.2.3 step 4): RAX, not the SSE pair - the `mixed` case
    # the IR's `regs` can spell never reaches a return of such a record.
    clang = shutil.which("clang++")
    if clang:
        transport_probe = (
            "struct P8{int a,b;};struct F12{float v[3];};"
            "struct NT8{int a,b;NT8(const NT8&);~NT8();};struct P24{long long a,b,c;};"
            "struct MIX{float f;unsigned u;};"
            "struct I2{virtual P8 f8();virtual F12 f12();virtual NT8 fnt();"
            "virtual P24 f24();virtual MIX fmix();};"
            "P8 I2::f8(){return {1,2};}F12 I2::f12(){return {1,2,3};}"
            "NT8 I2::fnt(){return *(NT8*)0;}P24 I2::f24(){return {1,2,3};}"
            "MIX I2::fmix(){return {1,2};}"
        )

        def transport_defines(target: str) -> dict[str, str]:
            run = subprocess.run(
                [clang, f"--target={target}", "-std=c++20", "-x", "c++", "-S", "-emit-llvm", "-O0", "-o", "-", "-"],
                input=transport_probe,
                capture_output=True,
                text=True,
            )
            found = {}
            for line in run.stdout.splitlines():
                if not line.startswith("define"):
                    continue
                for name in ("f8", "f12", "fnt", "f24", "fmix"):
                    if f"?{name}@I2@@" in line or f"_ZN2I2{len(name)}{name}E" in line:
                        found[name] = line
            return found

        msvc_ir = transport_defines("x86_64-pc-windows-msvc")
        itanium_ir = transport_defines("x86_64-linux-gnu")
        for name in ("f8", "f12", "fnt", "f24"):
            line = msvc_ir.get(name, "")
            args = line.split("(", 1)[-1]
            ok(
                line.startswith("define dso_local void") and "sret" in args
                and args.index("sret") > args.index("%0"),
                f"clang: MSVC {name} (8/12/8+copy-ctor/24 bytes) returns through sret after `this`",
            )
        ok(
            itanium_ir.get("f8", "").startswith("define dso_local i64"),
            "clang: Itanium returns the 8-byte integer pair in RAX (sret false, regs int)",
        )
        ok(
            itanium_ir.get("f12", "").startswith("define dso_local { <2 x float>, float }"),
            "clang: Itanium returns the 12-byte float[3] in the SSE pair (sret false, regs sse)",
        )
        for name in ("fnt", "f24"):
            line = itanium_ir.get(name, "")
            args = line.split("(", 1)[-1]
            ok(
                line.startswith("define dso_local void")
                and args.startswith("ptr dead_on_unwind noalias writable sret"),
                f"clang: Itanium {name} (non-trivial / 24 bytes) has sret FIRST, before `this`",
            )
        ok(
            itanium_ir.get("fmix", "").startswith("define dso_local i64"),
            "clang: Itanium returns the float+int pair in RAX - INTEGER wins the shared eightbyte",
        )

    # ...and the classification gen.py derives from the flat IR, on records
    # that cite the probe shapes above.
    def ir_record(name, kind, size, fields, bases=()):
        return {
            "name": name,
            "kind": kind,
            "size": size,
            "bases": [{"name": base, "offset": 0, "virtual": False} for base in bases],
            "pass_in_registers": True,
            "fields": fields,
        }

    def plain_field(name, offset, size, cxx, rust):
        return {
            "name": name,
            "offset": offset,
            "size": size,
            "bit_width": None,
            "cxx_type": cxx,
            "rust_type": rust,
        }

    int_pair = ir_record("IntPair", "struct", 8, [plain_field("a", 0, 4, "int", "i32"), plain_field("b", 4, 4, "int", "i32")])
    float_triple = ir_record("FloatTriple", "struct", 12, [plain_field("v", 0, 12, "float [3]", "[f32; 3]")])
    sse_then_int = ir_record(
        "SseThenInt", "struct", 16,
        [plain_field("f", 0, 8, "double", "f64"), plain_field("a", 8, 4, "int", "i32")],
    )
    mixed_eightbyte = ir_record(
        "MixedEightbyte", "struct", 8,
        [plain_field("f", 0, 4, "float", "f32"), plain_field("u", 4, 4, "unsigned int", "u32")],
    )
    nested = ir_record(
        "Nested", "struct", 16,
        [plain_field("v", 0, 12, "FloatTriple", "FloatTriple"), plain_field("a", 12, 4, "int", "i32")],
    )
    catalog = {r["name"]: r for r in (int_pair, float_triple, sse_then_int, mixed_eightbyte, nested)}
    ok(itanium_return_regs(int_pair, catalog, "X::f8#0") == "int", "regs int: the 8-byte integer pair")
    ok(itanium_return_regs(float_triple, catalog, "X::f12#0") == "sse", "regs sse: the 12-byte float[3]")
    ok(itanium_return_regs(sse_then_int, catalog, "X::fs#0") == "mixed", "regs mixed: SSE then INTEGER eightbyte")
    ok(itanium_return_regs(mixed_eightbyte, catalog, "X::fmix#0") == "int", "regs int: INTEGER wins the shared eightbyte")
    ok(itanium_return_regs(nested, catalog, "X::fn#0") == "mixed", "regs mixed: a record field is walked into, the tail int shares its eightbyte")

    # the transport attach on a synthetic group: the two ABIs' answers, the
    # non-class returns left alone, and the unknown rules failing loudly
    ret_types = TypeMapper(
        known={"QAngle", "Big", "Empty", "AUnion", "CEntityHandle"},
        records={"QAngle", "Big", "Empty", "AUnion", "CEntityHandle"},
        enums=set(),
        pass_in_registers={"QAngle": True, "Big": True, "Empty": True, "AUnion": True, "CEntityHandle": True},
        typedefs={},
        enum_underlying={},
        record_sizes={"QAngle": 12, "Big": 24, "Empty": 1, "AUnion": 4, "CEntityHandle": 4},
        enum_sizes={},
        specializations={"SchemaMetaInfoHandle_t<CSchemaClassInfo>": True},
        spec_layouts={"SchemaMetaInfoHandle_t<CSchemaClassInfo>": handle_layout},
    )
    qangle_record = ir_record(
        "QAngle", "class", 12,
        [plain_field("x", 0, 4, "vec_t", "f32"), plain_field("y", 4, 4, "vec_t", "f32"), plain_field("z", 8, 4, "vec_t", "f32")],
    )
    qangle_record["msvc"] = {"size": 12, "align": 4}
    empty_record = ir_record("Empty", "class", 1, [])
    union_record = ir_record("AUnion", "union", 4, [plain_field("u", 0, 4, "unsigned int", "u32")])

    def ret_vtable(class_name, method_name, return_cxx, decl_index=0):
        return {
            "class": class_name,
            "primary_base": None,
            "methods": [
                {
                    "name": method_name,
                    "decl_index": decl_index,
                    "is_dtor": False,
                    "overrides": None,
                    "itanium_slot": 0,
                    "msvc_slot": 0,
                    "cxx_signature": f"{return_cxx} ()",
                    "rust_fn": None,
                }
            ],
        }

    def ret_ir(record, vtable):
        return {"records": [record], "enums": [], "vtables": [vtable], "specs": []}

    empty_groups = lambda: {
        group: {"records": [], "enums": [], "vtables": [], "specs": []}
        for group in GROUPS
    }
    good = empty_groups()
    good["tier0"] = ret_ir(qangle_record, ret_vtable("V", "GetAngles", "QAngle"))
    got = copy.deepcopy(good)
    attach_return_transports(got, ret_types, {})
    ok(
        got["tier0"]["vtables"][0]["methods"][0]["ret"]
        == {"itanium": {"sret": False, "size": 12, "regs": "sse"}, "msvc": {"sret": True, "size": 12}},
        f"QAngle: Itanium registers (sse), MSVC sret, got {got['tier0']['vtables'][0]['methods'][0]['ret']}",
    )
    big_record = dict(qangle_record, name="Big", size=24)
    big_record["msvc"] = {"size": 24, "align": 8}
    got = empty_groups()
    got["tier0"] = ret_ir(big_record, ret_vtable("V", "GetBig", "Big"))
    attach_return_transports(got, ret_types, {})
    ok(
        got["tier0"]["vtables"][0]["methods"][0]["ret"]["itanium"]
        == {"sret": True, "size": 24, "regs": "memory"},
        "a 24-byte return is sret/memory on Itanium too",
    )
    got = empty_groups()
    got["tier0"] = ret_ir(qangle_record, ret_vtable("V", "GetHandle", "SchemaMetaInfoHandle_t<CSchemaClassInfo>"))
    attach_return_transports(got, ret_types, {})
    ok(
        got["tier0"]["vtables"][0]["methods"][0]["ret"]
        == {"itanium": {"sret": False, "size": 8, "regs": "int"}, "msvc": {"sret": True, "size": 8}},
        "the one-pointer handle wrapper: a register return on Itanium, MSVC sret",
    )
    got = empty_groups()
    got["tier0"] = ret_ir(qangle_record, ret_vtable("V", "GetOther", "NotARecord"))
    attach_return_transports(got, ret_types, {})
    ok(
        "ret" not in got["tier0"]["vtables"][0]["methods"][0],
        "a return that is not a class by value gets no 'ret' object",
    )

    # The transport is group-agnostic: the second upstream's `metamod` group is
    # walked like any other, so a metamod header bump that returns a class by
    # value gets its `ret` object (and the emitter its _SRET/_RET_SIZE/_RET_REGS
    # consts) with no generator change. No metamod method returns a class today
    # (the interfaces return void/bool/int/pointers/HookID_t/size_t), which is
    # why ir/metamod.json has no `ret` object - not because the group is skipped.
    got = empty_groups()
    got["metamod"] = ret_ir(qangle_record, ret_vtable("IKHook", "GetAngles", "QAngle"))
    attach_return_transports(got, ret_types, {})
    ok(
        got["metamod"]["vtables"][0]["methods"][0]["ret"]
        == {"itanium": {"sret": False, "size": 12, "regs": "sse"}, "msvc": {"sret": True, "size": 12}},
        f"the metamod group gets the same 'ret' object as every other group: "
        f"{got['metamod']['vtables'][0]['methods'][0].get('ret')}",
    )
    ok(
        "metamod" in gen.C_HEADER_EXCLUDED_GROUPS,
        "the metamod group stays out of crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h (frozen at the s2sdk groups)",
    )

    def raised(call) -> str:
        try:
            call()
        except GenError as exc:
            return str(exc)
        return ""

    # an opaque record (CEntityHandle: its one member is the anonymous union
    # the flat IR cannot name) classifies from the record-layout dump
    ehandle = ir_record("CEntityHandle", "class", 4, [])
    ehandle["opaque"] = True
    ehandle["msvc"] = {"size": 4, "align": 4}
    ehandle_layout = RecordLayout(
        name="CEntityHandle",
        kind="class",
        size=4,
        align=4,
        has_vptr=False,
        bases=[],
        members=[
            LayoutMember(level=1, offset=0, bit_from=None, bit_to=None, type="union CEntityHandle::(anonymous at /sdk/x.h:9:2)", name=None),
            LayoutMember(level=2, offset=0, bit_from=None, bit_to=None, type="uint32", name="m_Index"),
            LayoutMember(level=2, offset=0, bit_from=0, bit_to=14, type="uint32", name="m_EntityIndex"),
        ],
    )
    got = empty_groups()
    got["tier0"] = ret_ir(ehandle, ret_vtable("V", "GetEHandle", "CEntityHandle"))
    attach_return_transports(got, ret_types, {"CEntityHandle": ehandle_layout})
    ok(
        got["tier0"]["vtables"][0]["methods"][0]["ret"]
        == {"itanium": {"sret": False, "size": 4, "regs": "int"}, "msvc": {"sret": True, "size": 4}},
        f"an opaque record classifies from the layout dump: {got['tier0']['vtables'][0]['methods'][0]['ret']}",
    )
    got = empty_groups()
    got["tier0"] = ret_ir(ehandle, ret_vtable("V", "GetEHandle", "CEntityHandle"))
    message = raised(lambda: attach_return_transports(got, ret_types, {}))
    ok(
        "V::GetEHandle#0" in message and "no record layout" in message,
        f"an opaque record without a layout is a loud failure: {message!r}",
    )

    ir = empty_groups()
    ir["tier0"] = ret_ir(qangle_record, ret_vtable("V", "GetAngles", "QAngle"))
    ir["events"] = ret_ir(empty_record, ret_vtable("W", "GetEmpty", "Empty"))
    message = raised(lambda: attach_return_transports(copy.deepcopy(ir), ret_types, {}))
    ok(
        "W::GetEmpty#0" in message and "Empty" in message,
        f"an empty class return is a loud failure naming the method: {message!r}",
    )
    union_ir = empty_groups()
    union_ir["tier0"] = ret_ir(union_record, ret_vtable("V", "GetUnion", "AUnion"))
    message = raised(lambda: attach_return_transports(union_ir, ret_types, {}))
    ok("V::GetUnion#0" in message, f"a union return is a loud failure: {message!r}")

    hidden_method = {
        "is_dtor": False,
        "cxx_signature": "CKV3MemberName ()",
        "rust_fn": 'unsafe extern "C" fn(this: *mut C) -> CKV3MemberName',
        "msvc_rust_fn": 'unsafe extern "C" fn(this: *mut C, ret: *mut CKV3MemberName) -> *mut CKV3MemberName',
    }
    ok(
        vtable_field_lines("C", hidden_method, "Get", 0, ABI_DEFAULT)
        == ['    pub Get: Option<unsafe extern "C" fn(this: *mut C) -> CKV3MemberName>,']
        and vtable_field_lines("C", hidden_method, "Get", 0, ABI_MSVC)
        == [
            "    pub Get: Option<unsafe extern \"C\" fn(this: *mut C, ret: *mut CKV3MemberName)"
            " -> *mut CKV3MemberName>,"
        ],
        "the vtable field is the native form on Itanium and the hidden-pointer form on MSVC",
    )

    # underlying type rule -------------------------------------------------- #
    ok(choose_underlying_type([0, 14]) == "int", "small values -> int")
    ok(choose_underlying_type([-1, 0]) == "int", "negative -> int")
    ok(choose_underlying_type([0, 2**31]) == "unsigned int", "large -> unsigned int")
    ok(choose_underlying_type([0, 2**40]) == "long", "huge -> long")

    # the two ABIs' command lines ------------------------------------------- #
    plain = CmakeFlags(std="-std=c++20", includes=["/p"], defines=["A"]).clang_flags()
    ok(plain == ["-std=c++20", "-DA", "-I/p"], f"the Itanium flag line is unchanged: {plain}")
    msvc_flags = CmakeFlags(
        std="-std=c++20",
        includes=["/p"],
        defines=["COMPILER_MSVC"],
        target=MSVC_TARGET,
        system_includes=("/x/crt/include",),
    ).clang_flags()
    ok(msvc_flags[:3] == ["-std=c++20", *MSVC_TARGET], f"MSVC target flags: {msvc_flags}")
    ok("-isystem" in msvc_flags and "/x/crt/include" in msvc_flags, "MSVC sysroot is an -isystem")
    ok("-I/p" in msvc_flags, "MSVC keeps the SDK include dirs")
    ok(not any(f.startswith("-DGCC") for f in msvc_flags), "no GCC defines under MSVC")

    # the IR header names both ABIs (F4), and the by-value rule is per ABI (F6) - #
    header = ir_upstream_header("abc123")
    ok(
        header
        == {"commit": "abc123", "abis": ["itanium-x86_64", "msvc-x86_64"]},
        f"ir upstream header {header}",
    )
    ok(msvc_pass_in_registers(True, 1) and msvc_pass_in_registers(True, 8), "a trivial aggregate of up to 8 bytes passes in registers")
    ok(not msvc_pass_in_registers(True, 9), "an >8-byte aggregate goes through the hidden pointer")
    ok(not msvc_pass_in_registers(False, 4), "a non-trivial aggregate goes through the hidden pointer")

    # helpers --------------------------------------------------------------- #
    ok(split_top_level("int, CUtlMap<int, Foo *>, char") == ["int", "CUtlMap<int, Foo *>", "char"], "template-aware split")
    ok(strip_method_name("void (int) const") == "(int) const", "signature tail")
    objects = parse_ast_json_stream('{"kind": "A"}\n{"kind": "B"}')
    ok([o["kind"] for o in objects] == ["A", "B"], "JSON object stream")

    # the [consts] kind ----------------------------------------------------- #
    constant = parse_const({"name": "THING", "expr": "THING", "type": "u32"})
    ok(constant.kind == "int" and constant.rust_type == "u32" and constant.tag == "s2sdk_const_THING", "an int const")
    ok(
        parse_const({"name": "VERSION", "expr": "V", "kind": "string"}).rust_type == "&str",
        "a string const",
    )
    ok(
        fails(lambda: parse_const({"name": "X", "expr": "X"})),
        "an int const without a type is refused",
    )
    ok(
        fails(lambda: parse_const({"name": "X", "expr": "X", "type": "u32", "derived": "A.b.c"})),
        "expr and derived together are refused",
    )
    ok(
        fails(lambda: parse_const({"name": "X", "derived": "A.b.width", "type": "u32"})),
        "an unknown derived `what` is refused",
    )
    ok(
        fails(lambda: parse_const({"name": "X", "expr": "X", "type": "int"})),
        "a type outside CONST_TYPES is refused",
    )
    ok(
        parse_const({"name": "X", "derived": "A.b.bit_width", "type": "u32", "transform": "mask"}).transform
        == "mask",
        "a derived const with a transform",
    )
    ok(fails(lambda: parse_const({"name": "X", "expr": "X", "type": "u32", "transform": "mask"})), "transform without derived is refused")
    ok(spec_tag("CUtlVector<CEntityHandle>") == "CUtlVector_CEntityHandle", "spec_tag")
    ok(rust_int_fits("u32", 0xFFFFFFFF) and not rust_int_fits("u32", -1), "u32 range")
    ok(rust_int_fits("i64", -(2**63)) and not rust_int_fits("i64", 2**63), "i64 range")
    ok(apply_transform("X", 15, "mask") == 0x7FFF and apply_transform("X", 15, None) == 15, "transform")

    derived_layout = parse_record_layouts(_LAYOUT_SAMPLE)["S2Sample"]
    ok(derive_member_value(derived_layout, "m_EntityIndex", "bit_width") == 15, "derived bit_width")
    ok(
        derive_member_value(derived_layout, "m_EntityIndex", "bit_offset") == 28 * 8,
        "derived bit_offset of a nested bitfield",
    )
    ok(fails(lambda: derive_member_value(derived_layout, "m_pszName", "bit_width")), "a non-bitfield has no width")
    ok(fails(lambda: derive_member_value(derived_layout, "nope", "offset")), "an unknown member is refused")

    # the flattening a specialization's fields come from --------------------- #
    flat = [path for path, _member in flatten_leaf_members(derived_layout)]
    ok(
        flat
        == [
            ("m_base",),
            ("m_pszName",),
            ("m_szBuf",),
            ("m_Index",),
            ("m_Parts", "m_EntityIndex"),
            ("m_Parts", "m_Serial"),
            ("m_nTail",),
        ],
        f"flattened leaves of S2Sample: {flat}",
    )
    ok(
        member_path_expr("S2Sample", ("m_Parts", "m_Serial"))
        == "((S2Sample*)0)->m_Parts.m_Serial",
        "the access path of a nested leaf",
    )

    # the string probes are read out of the AST ------------------------------ #
    string_ast = [
        {
            "kind": "NamespaceDecl",
            "name": PROBE_STRING_NAMESPACE,
            "inner": [
                {
                    "kind": "VarDecl",
                    "name": "s2sdk_const_VERSION",
                    "inner": [
                        {"kind": "ImplicitCastExpr", "inner": [{"kind": "StringLiteral", "value": '"SceneSystem_002"'}]}
                    ],
                }
            ],
        }
    ]
    strings = parse_probe_strings(string_ast)
    ok(strings == {"s2sdk_const_VERSION": "SceneSystem_002"}, f"string probes: {strings}")
    ok(
        parse_probe_strings([{"kind": "NamespaceDecl", "name": "other", "inner": []}]) == {},
        "another namespace is ignored",
    )
    consts = [
        ConstEntry(
            name="COUNT",
            kind="int",
            rust_type="u32",
            expr="COUNT",
            derived=None,
            transform=None,
            per_abi=False,
            doc="doc",
        ),
        ConstEntry(
            name="BITS",
            kind="int",
            rust_type="u32",
            expr=None,
            derived=("S2Sample", "m_EntityIndex", "bit_width"),
            transform=None,
            per_abi=False,
            doc=None,
        ),
        ConstEntry(
            name="MASK",
            kind="int",
            rust_type="u32",
            expr=None,
            derived=("S2Sample", "m_EntityIndex", "bit_width"),
            transform="mask",
            per_abi=False,
            doc=None,
        ),
        ConstEntry(
            name="VERSION",
            kind="string",
            rust_type="&str",
            expr="VERSION",
            derived=None,
            transform=None,
            per_abi=False,
            doc=None,
        ),
    ]
    measured = measure_consts(
        consts,
        {"s2sdk_const_COUNT": 7},
        {"s2sdk_const_VERSION": "SceneSystem_002"},
        {"S2Sample": derived_layout},
    )
    ok(
        measured == {"COUNT": 7, "BITS": 15, "MASK": 0x7FFF, "VERSION": "SceneSystem_002"},
        f"measured consts: {measured}",
    )
    ok(
        fails(lambda: measure_consts(consts, {}, {}, {"S2Sample": derived_layout})),
        "a const with no measured value is refused",
    )
    ok(
        fails(lambda: [measure_consts([c], {"s2sdk_const_COUNT": 2**40}, {}, {}) for c in consts[:1]]),
        "a value that does not fit its type is refused",
    )

    # the emitted consts ----------------------------------------------------- #
    const_paths = {
        "BITS": ["m_Parts", "m_EntityIndex"],
        "MASK": ["m_Parts", "m_EntityIndex"],
    }
    ok(
        measure_derived_paths(consts, {"S2Sample": derived_layout}) == const_paths,
        "the C++ path of a derived bitfield",
    )
    const_ir = build_consts(consts, measured, const_paths)
    ok(const_ir[0]["value"] == 7 and const_ir[0]["expr"] == "COUNT", "build_consts keeps the expression")
    ok(const_ir[2]["derived"] == "S2Sample.m_EntityIndex.bit_width", "build_consts keeps the derivation")
    ok(
        const_ir[2]["derived_path"] == ["m_Parts", "m_EntityIndex"],
        "build_consts keeps the derived member path",
    )
    emit_const = emit_const_lines({**const_ir[0], "msvc": {"value": 7}})
    ok(emit_const[-1] == "pub const COUNT: u32 = 7;", f"emitted const: {emit_const[-1]}")
    emit_const_per_abi = emit_const_lines(
        {**const_ir[0], "per_abi": True, "msvc": {"value": 9}}
    )
    ok(
        emit_const_per_abi[1] == "#[cfg(not(windows))]"
        and emit_const_per_abi[2] == "pub const COUNT: u32 = 7;"
        and emit_const_per_abi[3] == "#[cfg(windows)]"
        and emit_const_per_abi[4] == "pub const COUNT: u32 = 9;",
        f"a per_abi const is emitted twice: {emit_const_per_abi}",
    )

    # the static-assert TU re-derives the consts too ------------------------- #
    empty_group = {"records": [], "enums": [], "vtables": [], "specs": []}
    verify_ir = {group: dict(empty_group) for group in GROUPS}
    verify_ir[CONST_GROUP] = {
        **empty_group,
        "consts": build_consts(consts, measured, const_paths),
    }
    for const in verify_ir[CONST_GROUP]["consts"]:
        const["msvc"] = {"value": const["itanium"]["value"]}
    verify_text = gen_verify_tu(verify_ir)
    ok('static_assert(((long long)(COUNT)) == 7, "COUNT: value");' in verify_text, "an int const is asserted")
    ok("constexpr auto s2sdk_verify_VERSION = (VERSION);" in verify_text, "a string const is re-read")
    ok("s2sdk_verify_VERSION[0] == 'S'" in verify_text, "a string const is compared char by char")
    ok(
        f"s2sdk_verify_VERSION[{len('SceneSystem_002')}] == 0" in verify_text,
        "the terminator is checked too",
    )
    ok(
        "using s2sdk_verify_BITS_parent = decltype(S2Sample::m_Parts);" in verify_text
        and "value.m_EntityIndex = static_cast<decltype(value.m_EntityIndex)>(~0ull);" in verify_text
        and "s2sdk_verify_bit_width(value.m_EntityIndex)" in verify_text
        and 'static_assert(s2sdk_verify_BITS == 15, "BITS: value");' in verify_text,
        "a derived bit width is re-measured through its member path",
    )
    ok(
        "static_cast<unsigned long long>(value.m_EntityIndex)" in verify_text
        and 'static_assert(s2sdk_verify_MASK == 32767, "MASK: value");' in verify_text,
        "a derived mask is compared to the all-ones field",
    )
    ok(
        fails(
            lambda: gen_verify_tu(
                {
                    **verify_ir,
                    CONST_GROUP: {
                        **empty_group,
                        "consts": [
                            {**c, "derived_path": None} if c.get("derived") else c
                            for c in verify_ir[CONST_GROUP]["consts"]
                        ],
                    },
                }
            )
        ),
        "a derived const without a member path is refused",
    )

    # the vtable-length check kind ------------------------------------------- #
    def length_method(name, slot, dtor=False, overrides=None):
        return {
            "name": name,
            "itanium_slot": slot,
            "msvc_slot": slot,
            "decl_index": 0,
            "is_dtor": dtor,
            "overrides": overrides,
        }

    def length_ir_with(derived_methods):
        return {
            **{group: dict(empty_group) for group in GROUPS},
            "tier0": {
                **empty_group,
                "vtables": [
                    # A destructor at slot 0: two Itanium entries, one MSVC one.
                    {
                        "class": "S2Base",
                        "primary_base": None,
                        "methods": [length_method("~S2Base", 0, dtor=True)],
                        "msvc": {"length": 1},
                    },
                    {
                        "class": "S2Derived",
                        "primary_base": "S2Base",
                        "methods": derived_methods,
                        "msvc": {"length": 2},
                    },
                ],
            },
        }

    length_ir = length_ir_with([length_method("B", 2)])
    ok(
        itanium_vtable_len(length_ir, "S2Base") == 2,
        "an Itanium destructor counts two vtable entries",
    )
    ok(
        itanium_vtable_len(length_ir, "S2Derived") == 3,
        "the primary base's length is part of the class's own",
    )
    ok(
        not fails(
            lambda: run_checks(
                {"checks": {"tier0.S2Derived": {"length": 3, "msvc_length": 2}}}, length_ir
            )
        ),
        "a matching vtable length passes",
    )
    ok(
        fails(lambda: run_checks({"checks": {"tier0.S2Derived": {"length": 2}}}, length_ir)),
        "a length that is one short (a destructor's second entry) is refused",
    )
    ok(
        fails(lambda: run_checks({"checks": {"tier0.S2Derived": {"msvc_length": 3}}}, length_ir)),
        "a wrong MSVC vtable length is refused",
    )
    ok(
        fails(lambda: run_checks({"checks": {"tier0.S2Derived": {"length": True}}}, length_ir)),
        "a boolean vtable length is refused",
    )
    ok(
        fails(lambda: run_checks({"checks": {"tier0.S2Derived": {}}}, length_ir)),
        "a two-part vtable key that names no method and no length is refused",
    )
    # a class that is a record *and* a vtable class carries its length in the record's row
    rec_ir = length_ir_with([length_method("B", 2)])
    rec_ir["tier0"]["records"] = [
        {"name": n, "size": 8, "align": 8, "fields": [], "msvc": {"size": 8, "align": 8, "fields": []}}
        for n in ("S2Derived", "S2Plain")
    ]
    ok(
        not fails(lambda: run_checks({"checks": {"tier0.S2Derived": {"size": 8, "length": 3, "msvc_length": 2}}}, rec_ir)),
        "a record that is also a vtable class passes a matching length row",
    )
    ok(
        fails(lambda: run_checks({"checks": {"tier0.S2Derived": {"length": 4}}}, rec_ir)),
        "a record that is also a vtable class refuses a wrong length",
    )
    ok(
        fails(lambda: run_checks({"checks": {"tier0.S2Plain": {"length": 4}}}, rec_ir)),
        "a record with no vtable refuses a length row",
    )
    ok(
        fails(lambda: check_no_vtable_length("tier0.S2Base", {"length": 4}, "the record S2Base")),
        "a length on a key that is not a vtable is refused",
    )
    ok(
        fails(lambda: itanium_vtable_len(length_ir_with([length_method("B", None)]), "S2Derived")),
        "a method with no Itanium slot is refused",
    )
    ok(
        fails(lambda: itanium_vtable_len(length_ir_with([length_method("B", 3)]), "S2Derived")),
        "a slot list with a gap fails closed",
    )
    ok(
        fails(lambda: itanium_vtable_len(length_ir_with([length_method("B", 2, overrides="S2Base::B")]), "S2Derived")),
        "an override that claims a new slot fails closed",
    )

    # the C header, s2sdk_gen.h (WP C1) ---------------------------------------- #
    def c_ir() -> dict:
        """The committed dual-ABI IR with one struct mutated to leave a gap, so
        the pad-and-assert code path is exercised against the real
        C_EMIT_STRUCTS list (no sysroot is needed for that)."""
        ir = load_committed_ir()
        vector = next(r for r in ir["tier0"]["records"] if r["name"] == "Vector")
        for where in (vector, vector["itanium"], vector["msvc"]):
            where["size"] = 16
        return ir

    ok(
        c_method_name(
            {"name": "CreateEvent", "msvc_name": "CreateEventA", "is_dtor": False}
        ) == "CreateEventA",
        "a Windows-renamed method is emitted under its msvc_name",
    )
    ok(
        c_method_name({"name": "~X", "msvc_name": None, "is_dtor": True}) == "dtor",
        "a destructor shares the dtor stem",
    )
    ok(c_int_literal(16384, True) == "16384u", "an unsigned int const keeps the u suffix")
    ok(c_int_literal(0xFFFFFFFF, True) == "0xFFFFFFFFu", "a wide const is spelled in hex")
    ok(c_int_literal(29, False) == "29", "a signed int const has no suffix")
    c_header = emit_c_header(c_ir())
    ok(
        "typedef struct S2_Vector S2_Vector;" in c_header,
        "the tags are pre-declared",
    )
    ok(
        "#define S2_SIZEOF_Vector 16" in c_header
        and "#define S2_ALIGNOF_Vector 4" in c_header,
        "size/align defines",
    )
    ok("    u8 _p0[4];" in c_header, "the trailing pad is spelled out")
    ok(
        "_Static_assert(sizeof(S2_Vector) == S2_SIZEOF_Vector," in c_header
        and '_Static_assert(sizeof(((S2_Vector *)0)->x) == 4,' in c_header,
        "struct asserts",
    )
    ok(
        "#define S2_SIZEOF_CUtlSymbolLarge 8" in c_header
        and "#define S2_OFF_CUtlVector_CEntityHandle_m_pMemory 8" in c_header,
        "the real emitted structs are spelled",
    )

    # whole-object structs (WP6): vptr pad, flattened primary-base fields, secondary bases as pads
    ok(
        "typedef struct S2_CMapNode {" in c_header
        and "_Alignas(8) u8 _p0[8];" in c_header
        and "S2_Vector m_Origin;" in c_header
        and "_Static_assert(__builtin_offsetof(S2_CMapNode, m_pAttributes)"
        " == S2_OFF_CDmElement_m_pAttributes," in c_header
        and "S2_CUtlVector m_Children;" in c_header,
        "an object struct flattens its primary-base chain (CDmElement, CMapPoint) and pads the vptr",
    )
    ok(
        c_header.count("m_unk001") > 0
        and "struct CBaseEditorSession *m_pActiveSession;" in c_header
        and "typedef struct CBaseEditorSession CBaseEditorSession;" in c_header,
        "an object's pointer member to another IR class names the bridge's own type",
    )
    obj_field = lambda name, offset, size, rust="u32", bits=None: {
        "name": name, "offset": offset, "size": size, "rust_type": rust,
        "cxx_type": "unsigned", "bit_width": bits, "bit_offset": None,
    }
    def obj_item(name, bases, fields, size, vptr=False):
        return {
            "name": name, "has_vptr": vptr, "bases": bases,
            "fields": [{k: v for k, v in f.items() if k != "offset"} for f in fields],
            "msvc": {"size": size, "align": 8, "fields": fields},
        }
    obj_base = obj_item("S2Base", [], [obj_field("m_a", 8, 4)], 16, vptr=True)
    obj_sec = obj_item("S2Sec", [], [obj_field("m_s", 8, 4)], 16, vptr=True)
    obj_ctx = {"enums": {}, "objects": set(), "records": {"S2Base": obj_base, "S2Sec": obj_sec}}
    obj_derived = obj_item(
        "S2Derived",
        [{"name": "S2Base", "offset": 0, "virtual": False}, {"name": "S2Sec", "offset": 16, "virtual": False}],
        [obj_field("m_b", 32, 4)], 40,
    )
    block = "\n".join(c_struct_block("S2Derived", obj_derived, {}, obj_ctx))
    ok(
        "_Alignas(8) u8 _p0[8];" in block
        and "u32 m_a;" in block
        and "u32 m_b;" in block
        and "m_s" not in block
        and "== S2_OFF_S2Base_m_a" in block,
        "a secondary base's subobject stays a pad, a primary base's field is flattened under its own offset macro",
    )
    obj_clash = obj_item("S2Clash", [{"name": "S2Base", "offset": 0, "virtual": False}], [obj_field("m_a", 24, 4)], 32)
    ok(
        fails(lambda: c_struct_block("S2Clash", obj_clash, {}, obj_ctx)),
        "a member name in both a base and the derived record is refused",
    )
    obj_bits = obj_item("S2Bits", [], [obj_field("m_f", 8, 4, bits=3)], 16, vptr=True)
    ok(
        fails(lambda: c_struct_block("S2Bits", obj_bits, {}, obj_ctx)),
        "a bitfield in an object is refused",
    )
    ok(
        fails(lambda: c_struct_block("S2Base", obj_base, {}, None)),
        "an object struct without the IR context is refused",
    )
    ok(
        "#define S2_SLOT_ISchemaSystemTypeScope_FindRawClassBinding 25" in c_header,
        "the first overload carries no suffix and MSVC's own slot",
    )
    ok(
        "#define S2_SLOT_ISchemaSystemTypeScope_FindRawClassBinding_1 24" in c_header,
        "the second overload is suffixed",
    )
    ok(
        "#define S2_SLOT_IGameEventManager2_dtor 0" in c_header
        and "#define S2_SLOT_IGameEventManager2_CreateEventA 6" in c_header,
        "dtor stem and msvc_name spelling",
    )
    ok(
        "#define S2_VTLEN_ISource2ServerConfig 29" in c_header
        and "/* S2_VTLEN_CSchemaSystemTypeScope" in c_header,
        "the vftable length, and a null length is commented not guessed",
    )
    ok(
        '#define S2_IFACE_SCHEMASYSTEM "SchemaSystem_001"' in c_header,
        "an interface version becomes S2_IFACE_*",
    )
    ok("#define S2_CONST_MAX_EDICTS 16384u" in c_header, "an int const becomes S2_CONST_*")
    ok(
        "#define S2_CONST_TOKEN_TOOLS 0x785BE7A7u" in c_header,
        "a wide const is spelled in hex like the bridge's literals",
    )
    # the sdk field's grammar (tie_terms / split_tie_scale): the tie tables are gone with the C
    # bridge, but sigcheck.sdk_eval and sdkcheck read the same syntax
    ok(
        gen.split_tie_scale(gen.slot_bytes("S2_SLOT_A_b")) == ("S2_SLOT_A_b", 8)
        and gen.split_tie_scale("S2_OFF_A_b") == ("S2_OFF_A_b", 1),
        "split_tie_scale reads back slot_bytes() and leaves a plain macro alone",
    )
    ok(
        fails(lambda: gen.split_tie_scale("S2_SLOT_A_b*4"))
        and fails(lambda: gen.split_tie_scale("S2_OFF_A_b*8")),
        "only S2_SLOT_*`*8` may carry a factor",
    )
    ok(
        gen.tie_terms("S2_OFF_A_b+8") == ("S2_OFF_A_b", 1, 8)
        and gen.tie_terms(gen.slot_bytes("S2_SLOT_A_b")) == ("S2_SLOT_A_b", 8, 0)
        and gen.tie_terms("S2_OFF_A_b") == ("S2_OFF_A_b", 1, 0)
        and gen.tie_terms("S2_SLOT_A_b+8") == ("S2_SLOT_A_b+8", 1, 0)
        and fails(lambda: gen.tie_terms("S2_OFF_A_b*8")),
        "tie_terms reads the +N member-offset form only; any other spelling stays a macro name that is not emitted, or a refused factor",
    )
    ok(
        gen.tie_terms("S2_OFF_A_b+S2_OFF_C_d", {"S2_OFF_C_d": 16}) == ("S2_OFF_A_b", 1, 16)
        and fails(lambda: gen.tie_terms("S2_OFF_A_b+S2_OFF_C_d"))
        and fails(lambda: gen.tie_terms("S2_OFF_A_b+S2_OFF_C_d", {"S2_OFF_E_f": 16})),
        "tie_terms reads the two-member form (a member of an embedded object) from the emitted values and refuses it without them",
    )

    committed_ir = load_committed_ir()
    committed_header = emit_c_header(committed_ir)
    ok(
        "S2SDK_GEN_REQUIRE_PINS" not in committed_header
        and "_Static_assert(S2_SLOT_" not in committed_header
        and "ties: generated" not in committed_header,
        "WP5: s2sdk_gen.h carries no pin-tie block and no REQUIRE_PINS machinery",
    )

    ok(
        "SRVCFG_VT_SLOTS" not in committed_header,
        "the pruned SRVCFG_VT_SLOTS anchor is gone from the generated header "
        "(the code uses S2_VTLEN_ISource2ServerConfig; the runtime copy is "
        "built from the pinned vftable)",
    )
    ok(
        "s2sdk.h" not in committed_header
        and "build.sh" not in committed_header
        and "s2sdk_gen_ties" not in committed_header
        and "_Static_assert(sizeof(S2HandleVec)" not in committed_header,
        "the header names no file of the retired C bridge and carries none of its ties",
    )
    ok(
        all(line not in committed_header for line in C_SMOKE_TYPEDEFS),
        "the header stays freestanding: the u8/u16/u32/u64 typedefs come from its includer",
    )

    # the smoke compile --check runs, as an actual clang compile: the committed
    # header passes, and a drifted number trips its own _Static_assert. Skipped
    # only when no clang is on PATH; --check needs clang anyway.
    cc = os.environ.get("CC", "clang")
    if shutil.which(cc):
        smoke_scratch = SCRATCH / "selftest-c-header-smoke"
        ok(
            not fails(lambda: smoke_compile_c_header(committed_header, smoke_scratch)),
            "the committed header compiles freestanding on the MSVC target",
        )
        drifted = committed_header.replace(
            "#define S2_SIZEOF_Vector 12\n", "#define S2_SIZEOF_Vector 16\n"
        )
        ok(
            drifted != committed_header
            and fails(lambda: smoke_compile_c_header(drifted, smoke_scratch)),
            "a size macro that disagrees with its padded struct fails the smoke compile",
        )
        redefined = committed_header.replace(
            "#endif /* S2SDK_GEN_H */", "#define S2_SIZEOF_Vector 13\n#endif /* S2SDK_GEN_H */"
        )
        ok(
            fails(lambda: smoke_compile_c_header(redefined, smoke_scratch)),
            "a macro redefinition is an error in the smoke compile, not a warning",
        )

    # the per-ABI emitters --------------------------------------------------- #
    # A tiny two-ABI IR with the two really different shapes: a record the ABIs
    # agree on, a record they do not, and a vtable class with an interleaved
    # overload group and a new-slot destructor (abi.rs's OVERLOADS fixture).
    def abi_field(name, offset, size, cxx="int", rust="i32", bit_offset=None, bit_width=None):
        return {
            "name": name,
            "offset": offset,
            "size": size,
            "cxx_type": cxx,
            "rust_type": rust,
            "bit_offset": bit_offset,
            "bit_width": bit_width,
        }

    def summary(field):
        return abi_field_summary(field)

    def per_abi_record(name, itanium_fields, msvc_fields, size):
        return {
            "name": name,
            "header": "s2.h",
            "kind": "struct",
            "size": size,
            "align": 8,
            "opaque": False,
            "pass_in_registers": True,
            "bases": [],
            "has_vptr": False,
            "fields": itanium_fields,
            "itanium": {
                "size": size,
                "align": 8,
                "pass_in_registers": True,
                "fields": [summary(field) for field in itanium_fields],
            },
            "msvc": {
                "size": size,
                "align": 8,
                "pass_in_registers": True,
                "fields": [summary(field) for field in msvc_fields],
            },
        }

    same_fields = [abi_field("m_a", 0, 4), abi_field("m_b", 4, 4)]
    fixture = {
        group: {"records": [], "enums": [], "vtables": [], "specs": []} for group in GROUPS
    }
    fixture[CONST_GROUP] = {
        "records": [],
        "enums": [],
        "vtables": [],
        "specs": [],
        "consts": [],
    }
    fixture["tier0"]["records"].append(
        per_abi_record("S2Same", same_fields, same_fields, 8)
    )
    fixture["tier0"]["records"].append(
        per_abi_record(
            "S2Moved",
            [abi_field("m_a", 0, 4), abi_field("m_b", 4, 4)],
            [abi_field("m_a", 0, 4), abi_field("m_b", 8, 4)],
            16,
        )
    )
    fixture["events"]["vtables"].append(
        {
            "class": "S2X",
            "header": "s2.h",
            "primary_base": None,
            "secondary_bases": [],
            "msvc": {"length": 4, "own_entries": 4},
            "methods": [
                {
                    "name": "A",
                    "decl_index": 0,
                    "is_dtor": False,
                    "overrides": None,
                    "itanium_slot": 0,
                    "msvc_slot": 0,
                    "cxx_signature": "void ()",
                    "rust_fn": None,
                },
                {
                    "name": "Ov",
                    "decl_index": 1,
                    "is_dtor": False,
                    "overrides": None,
                    "itanium_slot": 1,
                    "msvc_slot": 3,
                    "cxx_signature": "void (int)",
                    "rust_fn": None,
                },
                {
                    "name": "Ov",
                    "decl_index": 2,
                    "is_dtor": False,
                    "overrides": None,
                    "itanium_slot": 2,
                    "msvc_slot": 2,
                    "cxx_signature": "void (char *)",
                    "rust_fn": None,
                },
                {
                    "name": "~S2X",
                    "decl_index": 3,
                    "is_dtor": True,
                    "overrides": None,
                    "itanium_slot": 3,
                    "msvc_slot": 1,
                    "cxx_signature": "void () noexcept",
                    "rust_fn": None,
                },
            ],
        }
    )
    # A class whose MSVC table has a slot no declaration of the IR owns (the
    # implicit destructor of a class that declares none, say: clang prints the
    # entry, the IR has no declaration for it). Its two slots are not contiguous,
    # so the emitted struct has to pad the hole - and clang's printed length is
    # longer than the declarations bind, which the generated
    # `msvc_vtable_lengths_match_clangs_dump` is there to flag.
    fixture["events"]["vtables"].append(
        {
            "class": "S2Pad",
            "header": "s2.h",
            "primary_base": None,
            "secondary_bases": [],
            "msvc": {"length": 3, "own_entries": 2},
            "methods": [
                {
                    "name": "A",
                    "decl_index": 0,
                    "is_dtor": False,
                    "overrides": None,
                    "itanium_slot": 0,
                    "msvc_slot": 0,
                    "cxx_signature": "void ()",
                    "rust_fn": None,
                },
                {
                    "name": "B",
                    "decl_index": 1,
                    "is_dtor": False,
                    "overrides": None,
                    "itanium_slot": 1,
                    "msvc_slot": 2,
                    "cxx_signature": "void ()",
                    "rust_fn": None,
                },
            ],
        }
    )

    ok(
        abi_view(fixture["tier0"]["records"][1], ABI_MSVC)["fields"][1]["offset"] == 8,
        "abi_view reads the MSVC offsets",
    )
    ok(
        abi_sub_object(fixture["tier0"]["records"][0], ABI_DEFAULT)["size"] == 8,
        "abi_sub_object reads the itanium half",
    )
    ok(
        fails(lambda: abi_sub_object({"name": "S2Nothing"}, ABI_MSVC)),
        "a missing sub-object is refused, never a fallback",
    )
    fixture_sizes = {abi: abi_sizes(fixture, abi) for abi in ABIS}
    ok(
        fixture_sizes[ABI_DEFAULT] == fixture_sizes[ABI_MSVC],
        "the fixture records are the same size under both ABIs",
    )

    def emit_fixture_record(record):
        return emit_per_abi(
            lambda abi: emit_record(abi_view(record, abi), fixture_sizes[abi])
        )

    same_lines = emit_fixture_record(fixture["tier0"]["records"][0])
    ok(
        not any(line.startswith("#[cfg") for line in same_lines),
        f"a record the ABIs agree on is emitted once: {same_lines[:2]}",
    )
    moved_lines = emit_fixture_record(fixture["tier0"]["records"][1])
    ok(
        moved_lines[0] == CFG_ITANIUM
        and CFG_MSVC in moved_lines
        and moved_lines[-1] == "}",
        f"a record the ABIs disagree on is emitted twice: {moved_lines[:3]}",
    )

    shapes = vtable_shapes(fixture)
    shape = shapes["S2X"]
    ok(
        shape["slots"][ABI_DEFAULT] == {"A": 0, "Ov": 1, "Ov_1": 2, "dtor": 3}
        and shape["slots"][ABI_MSVC] == {"A": 0, "Ov": 3, "Ov_1": 2, "dtor": 1},
        f"the two ABIs' slot consts: {shape['slots']}",
    )
    ok(
        shape["length"] == {ABI_DEFAULT: 5, ABI_MSVC: 4},
        f"the two ABIs' vtable lengths: {shape['length']}",
    )
    ok(
        [item.name for item in vtable_slot_items(shape, ABI_DEFAULT)]
        == ["A", "Ov", "Ov_1", "dtor_complete", "dtor_deleting"],
        f"the itanium struct is in declaration order: "
        f"{[item.name for item in vtable_slot_items(shape, ABI_DEFAULT)]}",
    )
    ok(
        [item.name for item in vtable_slot_items(shape, ABI_MSVC)]
        == ["A", "dtor", "Ov_1", "Ov"],
        f"the MSVC struct is in slot order: "
        f"{[item.name for item in vtable_slot_items(shape, ABI_MSVC)]}",
    )
    ok(
        [item.offset for item in vtable_slot_items(shape, ABI_MSVC)] == [0, 8, 16, 24],
        "the MSVC struct sits at its slots",
    )

    pad_shape = shapes["S2Pad"]
    ok(
        [item.name for item in vtable_slot_items(pad_shape, ABI_MSVC)] == ["A", "_pad0", "B"],
        f"a slot nothing is bound to becomes a pad: "
        f"{[item.name for item in vtable_slot_items(pad_shape, ABI_MSVC)]}",
    )
    ok(
        [item.offset for item in vtable_slot_items(pad_shape, ABI_MSVC)] == [0, 8, 16],
        "the pad fills the hole before the next declaration's slot",
    )
    longer = {
        **pad_shape,
        "vtable": {**pad_shape["vtable"], "msvc": {"length": 4, "own_entries": 2}},
    }
    ok(
        [item.name for item in vtable_slot_items(longer, ABI_MSVC)]
        == ["A", "_pad0", "B", "_pad1"]
        and [item.offset for item in vtable_slot_items(longer, ABI_MSVC)] == [0, 8, 16, 24],
        "a slot clang printed and no declaration binds pads the end of the struct",
    )

    # the return-transport consts sit next to the slots they belong to (WP1.D)
    fixture["events"]["vtables"][0]["methods"][0]["ret"] = {
        "itanium": {"sret": False, "size": 12, "regs": "sse"},
        "msvc": {"sret": True, "size": 12},
    }
    slots_module = "\n".join(emit_vtable_slots(shapes["S2X"], ABI_DEFAULT))
    msvc_slots_module = "\n".join(emit_vtable_slots(shapes["S2X"], ABI_MSVC))
    ok(
        "pub const A_SRET: bool = false" in slots_module
        and "pub const A_RET_SIZE: usize = 12" in slots_module
        and 'pub const A_RET_REGS: &str = "sse"' in slots_module,
        f"the Itanium slots module carries the return transport: {slots_module}",
    )
    ok(
        "pub const A_SRET: bool = true" in msvc_slots_module
        and "pub const A_RET_SIZE: usize = 12" in msvc_slots_module
        and "RET_REGS" not in msvc_slots_module,
        "the MSVC slots module carries sret/size and no regs",
    )
    # VTLEN's doc cites the C header's S2_VTLEN_<Class> define only for a group
    # that is emitted into it: the metamod group (C_HEADER_EXCLUDED_GROUPS) is
    # not, so its const must not claim a define that does not exist.
    ok(
        "S2_VTLEN_S2X" in slots_module,
        f"a group in the C header cites its S2_VTLEN_ define: {slots_module}",
    )
    excluded_slots = "\n".join(emit_vtable_slots(shapes["S2X"], ABI_DEFAULT, False))
    ok(
        "the same number the C header's" not in excluded_slots
        and "not emitted into" in excluded_slots
        and "there is no `S2_VTLEN_S2X` define" in excluded_slots
        and "pub const VTLEN: usize" in excluded_slots,
        f"an excluded group says so instead of naming a C define: {excluded_slots}",
    )
    ok(
        not fails(lambda: check_dual_abi(copy.deepcopy(fixture))),
        "check_dual_abi accepts a valid 'ret' object",
    )
    bad = copy.deepcopy(fixture)
    bad["events"]["vtables"][0]["methods"][0]["ret"]["itanium"]["regs"] = "xmm"
    ok(fails(lambda: check_dual_abi(bad)), "check_dual_abi refuses an unknown regs class")
    bad = copy.deepcopy(fixture)
    bad["events"]["vtables"][0]["methods"][0]["ret"]["msvc"]["sret"] = False
    ok(fails(lambda: check_dual_abi(bad)), "check_dual_abi refuses an MSVC return without sret")
    bad = copy.deepcopy(fixture)
    del bad["events"]["vtables"][0]["methods"][0]["ret"]["itanium"]
    ok(fails(lambda: check_dual_abi(bad)), "check_dual_abi refuses a one-sided 'ret' object")
    del fixture["events"]["vtables"][0]["methods"][0]["ret"]

    vtable_lines = emit_vtable(shape, fixture_sizes[ABI_MSVC])
    ok(
        vtable_lines[0] == "/// The C++ object only, as it looks from the vtable's side: it is only",
        "the object wrapper is emitted first",
    )
    joined = "\n".join(vtable_lines)
    ok(
        "pub struct S2X_vtable {" in joined and joined.count("pub struct S2X_vtable {") == 2,
        "the two ABIs' vtable structs are both emitted",
    )
    ok(
        "pub dtor_deleting: Option" in joined
        and "\n    pub dtor: Option" in joined
        and "scalar-deleting" in joined,
        "an Itanium destructor is two entries, an MSVC one the scalar-deleting slot",
    )
    ok(
        'pub dtor: Option<unsafe extern "C" fn(this: *mut S2X, flags: u32)'
        " -> *mut ::core::ffi::c_void>," in joined
        and 'pub dtor_complete: Option<unsafe extern "C" fn(this: *mut S2X)>,' in joined
        and 'pub dtor_deleting: Option<unsafe extern "C" fn(this: *mut S2X)>,' in joined,
        "the MSVC dtor keeps the scalar-deleting wrapper's signature "
        "(void* (T*, unsigned flags) - msvc_dtor_type) while the Itanium two "
        "keep the declared void (T*)",
    )
    # The other check is a substring search either way: count the wrapper's
    # signature so only the one MSVC dtor can carry it.
    ok(
        joined.count("flags: u32) -> *mut ::core::ffi::c_void") == 1,
        "only the MSVC dtor carries the wrapper's signature",
    )
    ok(
        joined.count("pub mod S2X_slots {") == 2 and "pub const dtor: usize = 3;" in joined,
        "the slots module is per ABI too",
    )

    # a record that is also a vtable class but derives its vptr from a record base
    # (CCSGOInput : CClientInput) reads the pointer through the base
    chain_records = {
        r["name"]: r
        for r in [
            {"name": "S2Root", "has_vptr": True, "bases": []},
            {"name": "S2Mid", "has_vptr": False, "bases": [{"name": "S2Root", "offset": 0}]},
            {"name": "S2Leaf", "has_vptr": False, "bases": [{"name": "S2Mid", "offset": 0}]},
            # a primary base with no record (an opaque class): the pointer is read at offset 0
            {"name": "S2OpaqueDerived", "has_vptr": False, "bases": [{"name": "S2Opaque", "offset": 0}]},
            {"name": "S2OpaqueLate", "has_vptr": False, "bases": [{"name": "S2Opaque", "offset": 8}]},
        ]
    }
    ok(
        inherited_vptr_path("S2Mid", chain_records, {"S2Root": {}}) == "base.vtable"
        and inherited_vptr_path("S2Leaf", chain_records, {"S2Root": {}}) == "base.base.vtable"
        and inherited_vptr_path("S2Mid", chain_records, {}) == "base.vtable_",
        "the vptr path walks the first bases to the record that owns the pointer",
    )
    ok(
        inherited_vptr_path("S2Root", chain_records, {}) is None
        and inherited_vptr_path("S2Nowhere", chain_records, {}) is None,
        "a record owning its vptr, or a class that is no record here, has no inherited path",
    )
    chain_records["S2Plain"] = {"name": "S2Plain", "has_vptr": False, "bases": []}
    chain_records["S2OverPlain"] = {"name": "S2OverPlain", "has_vptr": False, "bases": [{"name": "S2Plain", "offset": 0}]}
    ok(
        inherited_vptr_path("S2OverPlain", chain_records, {}) is None,
        "a chain that ends on a non-polymorphic record has no vptr path",
    )
    ok(
        inherited_vptr_path("S2OpaqueDerived", chain_records, {}) == RAW_VPTR
        and inherited_vptr_path("S2OpaqueLate", chain_records, {}) is None,
        "a primary base without a record is read raw at offset 0, a base elsewhere is refused",
    )
    ok(
        "vtable_len(Abi::native()" not in joined,
        "the VClass static is ABI-independent and emitted once",
    )
    ok(
        joined.count("pub static S2X_VCLASS") == 1,
        "exactly one VClass static",
    )

    # a slot two declarations are bound to is a generator error, not a struct
    # whose second field silently overlaps the first
    overlapping = {
        **shape,
        "entries": [
            dataclasses.replace(shape["entries"][0], slots={abi: 0 for abi in ABIS}),
            dataclasses.replace(shape["entries"][1], slots={abi: 0 for abi in ABIS}),
        ],
    }
    ok(
        fails(lambda: vtable_slot_items(overlapping, ABI_MSVC)),
        "two declarations bound to one slot are refused",
    )

    test_text = emit_layout_tests(fixture, "abc123", shapes, fixture_sizes)
    ok(
        LAYOUT_TEST_CFG[ABI_DEFAULT] in test_text
        and LAYOUT_TEST_CFG[ABI_MSVC] in test_text
        and "mod itanium {" in test_text
        and "mod msvc {" in test_text,
        "the layout test carries one cfg-gated module per ABI",
    )
    ok(
        "vtable_len(Abi::native(), &S2X_VCLASS)" in test_text
        and "slot(Abi::native(), &S2X_VCLASS" in test_text
        and "slot(Abi::Msvc, &S2X_VCLASS" in test_text,
        "each module asserts its own ABI, the Itanium one the MSVC rule too",
    )
    ok(
        "assert_eq!(offset_of!(S2X_vtable, dtor_deleting), 32);" in test_text
        and "assert_eq!(offset_of!(S2X_vtable, dtor), 8);" in test_text,
        "the emitted struct's slots are asserted per ABI",
    )
    ok(
        "assert_eq!(offset_of!(S2Moved, m_b), 8);" in test_text,
        "the layout test uses the ABI's field offsets",
    )
    # the compile-time block: `cargo check` evaluates it, which is what proves
    # the MSVC half's layout on a machine that cannot run its tests
    ok(
        "const _: () = {" in test_text
        and "assert!(size_of::<S2X_vtable>() == 40);" in test_text  # itanium: 5 slots
        and "assert!(size_of::<S2X_vtable>() == 32);" in test_text  # msvc: 4 slots
        and "assert!(offset_of!(S2X_vtable, dtor) == 8);" in test_text,
        "each module restates its vtable layout as const assertions",
    )
    ok(
        "assert!(size_of::<S2Pad_vtable>() == 24);" in test_text
        and "assert!(offset_of!(S2Pad_vtable, _pad0) == 8);" in test_text,
        "the const assertions include the pads",
    )

    # `rust_fn` is modelled from the Itanium answer: a by-value record the two
    # ABIs disagree on cannot be spelled by one signature, so the run fails
    # instead of emitting an Itanium signature for a Windows vtable.
    by_value = {group: {"records": [], "enums": [], "vtables": [], "specs": []} for group in GROUPS}
    by_value[CONST_GROUP] = {**by_value["tier0"], "consts": []}
    record = per_abi_record("S2Wide", same_fields, same_fields, 8)
    record["itanium"]["pass_in_registers"] = True
    record["msvc"]["pass_in_registers"] = False
    by_value["tier0"]["records"].append(record)
    by_value["events"]["vtables"].append(
        {
            "class": "S2Y",
            "header": "s2.h",
            "primary_base": None,
            "secondary_bases": [],
            "msvc": {"length": 1, "own_entries": 1},
            "methods": [
                {
                    "name": "Wide",
                    "decl_index": 0,
                    "is_dtor": False,
                    "overrides": None,
                    "itanium_slot": 0,
                    "msvc_slot": 0,
                    "cxx_signature": "S2Wide ()",
                    "rust_fn": 'unsafe extern "C" fn(this: *mut S2Y, a: S2Wide) -> S2Wide',
                }
            ],
        }
    )
    ok(
        by_value_signature_types(
            'unsafe extern "C" fn(this: *mut S2Y, a: S2Wide, b: *mut S2Wide) -> S2Wide'
        )
        == ["S2Wide", "S2Wide"],
        "the by-value scanner skips the `this` pointer and pointer parameters",
    )
    ok(
        fails(lambda: check_by_value_signatures(by_value)),
        "a by-value record the ABIs disagree on is refused",
    )
    by_value["tier0"]["records"][0]["msvc"]["pass_in_registers"] = True
    ok(
        not fails(lambda: check_by_value_signatures(by_value)),
        "the same record with both ABIs' answers agreeing passes",
    )
    # A trivially copyable record over 8 bytes is not "in registers" on MSVC, yet its
    # by-value parameter is spelled right by the one `rust_fn` (rustc lowers it as C++ does).
    by_value["tier0"]["records"][0]["msvc"]["pass_in_registers"] = False
    by_value["tier0"]["records"][0]["msvc"]["size"] = 16
    ok(
        not fails(lambda: check_by_value_signatures(by_value)),
        "a trivially copyable 16-byte by-value parameter is accepted",
    )

    # The enum-size refusal is its sibling's (the by-value one) mirrored case:
    # one Rust newtype per enum name, so an enum the ABIs size differently can
    # never be emitted - the run fails before the bindings change. It passes
    # both an unfixed enum (`int` by the MSVC rule) and a fixed one.
    fixed_enum = {"name": "S2Fixed", "underlying": "unsigned char", "fixed_underlying": True}
    unfixed_enum = {"name": "S2Unfixed", "underlying": None, "fixed_underlying": False}
    ok(
        msvc_enum_size(fixed_enum, 1) == {"size": 1, "underlying": "unsigned char"}
        and msvc_enum_size(unfixed_enum, 4) == {"size": 4, "underlying": "int"},
        "the MSVC enum rule holds for a fixed and an unfixed enum",
    )
    ok(
        fails(lambda: msvc_enum_size(fixed_enum, 2))
        and fails(lambda: msvc_enum_size(unfixed_enum, 2)),
        "an enum the MSVC pass sizes differently than its rule predicts is refused",
    )
    ok(
        fails(lambda: msvc_enum_size({"name": "S2Long", "underlying": "long", "fixed_underlying": True}, 8)),
        "a fixed underlying type MSVC would reinterpret (long) is refused, not assumed",
    )

    # Windows-only groups ---------------------------------------------------- #
    # `platforms = ["windows"]` on a group: its Itanium numbers prove the header
    # is portable but claim nothing, so the Rust is MSVC-only behind cfg(windows),
    # the layout test's Itanium module never names it, and `[checks]` refuses an
    # Itanium number there.
    ok(
        group_platforms({"g": {}}, "g") == ["windows", "linux"]
        and group_platforms({"g": {"platforms": ["windows", "linux"]}}, "g")
        == ["windows", "linux"]
        and group_platforms({"g": {"platforms": ["linux", "windows"]}}, "g")
        == ["windows", "linux"]
        and group_platforms({"g": {"platforms": ["windows"]}}, "g") == ["windows"],
        "platforms: absent and both mean both, [windows] is Windows-only",
    )
    for bad in ([], ["linux"], ["bsd"], ["windows", "windows"], "windows", [1]):
        ok(
            fails(lambda bad=bad: group_platforms({"g": {"platforms": bad}}, "g")),
            f"platforms = {bad!r} is refused",
        )
    ok(
        fails(
            lambda: validate_allowlist(
                {
                    **{g: {} for g in GROUPS},
                    CONST_GROUP: {"headers": [], "entries": [], "platforms": ["windows"]},
                }
            )
        ),
        "the consts group may not carry `platforms`",
    )

    wo = copy.deepcopy(fixture)
    wo["events"]["platforms"] = ["windows"]
    ok(windows_only_groups(wo) == ["events"] and windows_only_groups(fixture) == [],
       "windows_only_groups reads the IR marker")
    wo_shapes = vtable_shapes(wo)
    wo_sizes = {abi: abi_sizes(wo, abi) for abi in ABIS}
    wo_mod = emit_mod_rs("abc123", windows_only_groups(wo))
    ok(
        f"{CFG_MSVC}\nmod events;" in wo_mod
        and f"{CFG_MSVC}\npub use self::events::*;" in wo_mod
        and "\nmod tier0;" in wo_mod
        and f"{CFG_MSVC}\nmod tier0;" not in wo_mod,
        "mod.rs gates only the Windows-only group on cfg(windows)",
    )
    ok(
        "cfg" not in emit_mod_rs("abc123"),
        "with no Windows-only group mod.rs has no cfg at all",
    )
    wo_rs = emit_group_rs("events", wo["events"], "abc123", wo_shapes, wo_sizes)
    ok(
        CFG_ITANIUM not in wo_rs and "pub struct S2X_vtable {" in wo_rs
        and wo_rs.count("pub struct S2X_vtable {") == 1
        and "pub dtor: Option" in wo_rs and "dtor_deleting" not in wo_rs
        and "Windows-only group" in wo_rs,
        "a Windows-only group is emitted once, from the MSVC numbers",
    )
    both_rs = emit_group_rs("events", fixture["events"], "abc123", shapes, fixture_sizes)
    ok(
        both_rs.count("pub struct S2X_vtable {") == 2 and CFG_ITANIUM in both_rs,
        "the same group for both platforms is still emitted per ABI",
    )
    wo_tests = emit_layout_tests(wo, "abc123", wo_shapes, wo_sizes)
    itanium_half, msvc_half = wo_tests.split("mod msvc {")
    ok(
        "S2X" not in itanium_half and "S2Pad" not in itanium_half
        and "S2Moved" in itanium_half
        and "S2X_vtable" in msvc_half and "S2Pad_vtable" in msvc_half,
        "the layout test's Itanium module never names a Windows-only item, the MSVC one does",
    )
    ok(
        "S2X" in emit_layout_tests(fixture, "abc123", shapes, fixture_sizes).split("mod msvc {")[0],
        "...while a group for both platforms is in the Itanium module",
    )

    # a group for both platforms may not depend on a Windows-only one
    dep = copy.deepcopy(wo)
    ok(not fails(lambda: check_platform_references(dep)), "no cross-platform reference: passes")
    dep["tier0"]["records"][0]["fields"][0]["rust_type"] = "S2X"
    ok(fails(lambda: check_platform_references(dep)),
       "a both-platform record naming a Windows-only type is refused")
    dep = copy.deepcopy(wo)
    dep["tier0"]["vtables"].append(
        {"class": "S2Z", "primary_base": "S2X", "methods": [], "header": "s2.h"}
    )
    ok(fails(lambda: check_platform_references(dep)),
       "a both-platform vtable with a Windows-only primary base is refused")
    dep = copy.deepcopy(wo)
    dep["tier0"]["vtables"].append(
        {"class": "S2Z", "primary_base": None, "header": "s2.h", "methods": [
            {"rust_fn": 'unsafe extern "C" fn(this: *mut S2Z, a: *mut S2Pad)'}]}
    )
    ok(fails(lambda: check_platform_references(dep)),
       "a both-platform method signature naming a Windows-only type is refused")
    dep = copy.deepcopy(wo)
    dep["events"]["records"].append(
        per_abi_record("S2WinUser", same_fields, same_fields, 8)
    )
    dep["events"]["records"][0]["fields"][0]["rust_type"] = "S2Same"
    ok(not fails(lambda: check_platform_references(dep)),
       "a Windows-only group may name a both-platform type")

    # [checks]: only the msvc_* twins in a Windows-only group
    ok(fails(lambda: run_checks({"checks": {"events.S2X": {"length": 5}}}, wo)),
       "[checks] length (Itanium) on a Windows-only group is refused")
    ok(fails(lambda: run_checks({"checks": {"tier0.S2Same": {"size": 8}, "events.S2X.A": {"itanium_slot": 0}}}, wo)),
       "[checks] itanium_slot on a Windows-only group is refused")
    ok(not fails(lambda: run_checks(
        {"checks": {"events.S2X": {"msvc_length": 4}, "events.S2X.Ov": {"msvc_slot": 3, "decl_index": 1}}}, wo)),
       "[checks] msvc_* keys on a Windows-only group pass")
    ok(not fails(lambda: run_checks({"checks": {"events.S2X": {"length": 5}}}, fixture)),
       "the same Itanium key on a group for both platforms still passes")

    # METAMOD.lock: the second upstream's two checkouts ----------------------- #
    # The lock check is what `gen.py --check` refuses on, and it covers the KHook
    # submodule too: a metamod bump that moves third_party/khook moves IKHook's
    # slots. `sdk_head` is patched on the gen module (the documented pattern above)
    # because `locked_heads` runs git through it.
    metamod_root = gen.REPO_ROOT / gen.METAMOD_SUBMODULE
    khook_root = metamod_root / gen.KHOOK_SUBDIR
    sdk_root = gen.REPO_ROOT / "crates" / "s2sdk" / "upstream"
    locks = {
        "metamod": gen.read_metamod_lock(),
        "sdk": gen.read_lock(),
    }
    heads = {
        sdk_root: locks["sdk"]["commit"],
        metamod_root: locks["metamod"]["commit"],
        khook_root: locks["metamod"]["khook_commit"],
    }
    real_head = gen.sdk_head
    try:
        gen.sdk_head = lambda path: heads.get(Path(path))
        entries = gen.locked_heads(sdk_root, metamod_root)
        ok(
            [(entry.lock, entry.root) for entry in entries]
            == [
                ("UPSTREAM.lock", sdk_root),
                ("METAMOD.lock", metamod_root),
                ("METAMOD.lock", khook_root),
            ],
            f"the locked checkouts are the SDK, metamod and its khook submodule",
        )
        ok(
            entries[1].commit == locks["metamod"]["commit"]
            and entries[2].commit == locks["metamod"]["khook_commit"],
            "METAMOD.lock pins both the metamod commit and the khook one",
        )
        ok(
            not gen.lock_mismatches(entries),
            "checkouts at their locked revisions are no mismatch",
        )
        heads[khook_root] = "0" * 40
        mismatches = gen.lock_mismatches(gen.locked_heads(sdk_root, metamod_root))
        ok(
            len(mismatches) == 1
            and "METAMOD.lock" in mismatches[0]
            and "khook" in mismatches[0]
            and "pins" in mismatches[0],
            f"a moved khook submodule is refused, naming METAMOD.lock: {mismatches}",
        )
        heads[sdk_root] = None
        mismatches = gen.lock_mismatches(gen.locked_heads(sdk_root, metamod_root))
        ok(
            len(mismatches) == 2
            and any("cannot read the git revision" in line and "UPSTREAM.lock" in line for line in mismatches),
            f"a checkout whose revision cannot be read is a mismatch too: {mismatches}",
        )
    finally:
        gen.sdk_head = real_head

    # the second upstream's include/define order and its name handling --------- #
    ok(
        gen.metamod_first_includes(Path("/mm"), ["/a", "/b"])
        == ["/mm", "/mm/third_party/khook/include", "/a", "/b"],
        "the metamod include dirs come before the s2sdk ones",
    )
    ok(
        gen.metamod_defines(["S2"]) == ["S2", "META_NO_HL2SDK", "META_IS_SOURCE2"],
        "both metamod defines are appended for either ABI",
    )
    ok(
        gen.strip_namespace("SourceMM::ISmmPlugin") == "ISmmPlugin",
        "a leading namespace is stripped",
    )
    ok(
        gen.strip_namespace("std::chrono::duration<long double, std::ratio<1, 1000000000> >")
        == "duration<long double, std::ratio<1, 1000000000> >",
        "only the *leading* qualifiers go: a template argument keeps its own `::`",
    )
    ok(
        gen.bare_keyed({"ConVarRegList::Entry_t": 1, "Entry_t": 2}, "layouts")["Entry_t"] == 2,
        "an exact key wins over a qualified key's bare name",
    )
    ok(
        "Entry_t"
        not in gen.bare_keyed(
            {"ConVarRegList::Entry_t": 1, "ConCommandRegList::Entry_t": 2, "Other": 3}, "layouts"
        ),
        "two qualified keys sharing a bare name get no alias, so a lookup misses",
    )
    ok(
        gen.same_class("SourceMM::ISmmPlugin", "ISmmPlugin")
        and not gen.same_class("SourceMM::ISmmPlugin", "ISmmAPI"),
        "same_class compares the bare names",
    )
    types = TypeMapper(
        known={"ISmmAPI"},
        records={"ISmmAPI"},
        enums=set(),
        pass_in_registers={"ISmmAPI": False},
        typedefs={"size_t": "unsigned long"},
        enum_underlying={"Action": "uint8_t"},
        record_sizes={},
        enum_sizes={},
        specializations={},
        spec_layouts={},
    )
    ok(types.rust_type("std::size_t") == "usize", "std::size_t maps through its bare name")
    # A primitive's bare spelling beats an *exact* key: the second upstream's
    # headers spell `std::size_t`, and libstdc++/the MSVC STL declare it as a
    # typedef of `unsigned long`/`unsigned long long`. The contract maps `size_t`
    # to `usize` on both ABIs, so the exact key must not desugar it to the
    # platform's integer (which would also differ between the two ABIs).
    exact_primitive = TypeMapper(
        known=set(),
        records=set(),
        enums=set(),
        pass_in_registers={},
        typedefs={"std::size_t": "unsigned long", "size_t": "unsigned long"},
        enum_underlying={},
        record_sizes={},
        enum_sizes={},
        specializations={},
        spec_layouts={},
    )
    ok(
        exact_primitive.rust_type("std::size_t") == "usize"
        and exact_primitive.rust_type("size_t") == "usize",
        f"a primitive wins over an exact typedef key: "
        f"{exact_primitive.rust_type('std::size_t')!r}",
    )
    ok(types.rust_type("KHook::Action") == "u8", "KHook::Action maps through its enum's underlying type")
    ok(types.rust_type("SourceMM::ISmmAPI *") == "*mut ISmmAPI", "a qualified pointer names the record")

    # a bare name two declarations carry does not resolve ---------------------- #
    # The AST's enums/typedefs are keyed by their qualified name: a table keyed by
    # the bare name answers with whichever declaration clang dumped first, so an
    # s2sdk global (or a libstdc++/MSVC STL) name would silently take over the
    # second upstream's - `KHook::Action` here. `_SCOPE_AST` is the same synthetic
    # dump shape as `_AST_SAMPLE`, with the scopes a real one has.
    scoped = AstSink()
    ast_walk(_SCOPE_AST, scoped)
    ok(
        sorted(scoped.enums) == ["A::Action", "Action", "KHook::Action"]
        and sorted(scoped.typedefs) == ["KHook::IndexType_t", "Widget::BaseClass"],
        f"enum/typedef declarations are keyed by their qualified name: "
        f"{sorted(scoped.enums)} {sorted(scoped.typedefs)}",
    )
    ok(
        scoped.ambiguous == frozenset({"Action"})
        and "Action" in scoped.enum_table
        and "A::Action" in scoped.enum_table,
        f"the bare name two declarations carry is ambiguous, an exact one still resolves: "
        f"{sorted(scoped.ambiguous)}",
    )
    ok(
        "BaseClass" in scoped.typedef_table and "IndexType_t" in scoped.typedef_table,
        "a bare name only one declaration carries gets an alias",
    )
    scoped_typedefs, scoped_enums = type_sugar_tables(scoped, [])
    scoped_types = TypeMapper(
        known=set(),
        records=set(),
        enums=set(),
        pass_in_registers={},
        typedefs=scoped_typedefs,
        enum_underlying=scoped_enums,
        record_sizes={},
        enum_sizes={},
        specializations={},
        spec_layouts={},
        ambiguous=scoped.ambiguous,
    )
    ok(
        scoped_types.rust_type("A::Action") == "u8"
        and scoped_types.rust_type("KHook::Action") == "u8"
        and scoped_types.rust_type("KHook::IndexType_t") == "u32"
        and scoped_types.rust_type("IndexType_t") == "u32",
        "an exact qualified spelling resolves",
    )
    ok(
        scoped_types.rust_type("Other::Action") is None
        and scoped_types.rust_type("Action") == "i32",
        "a qualified spelling no declaration has does NOT resolve to the same-named "
        "declaration in another scope (the bare spelling itself still does)",
    )
    ok(
        scoped_types.rust_type("Widget::BaseClass") == "i32"
        and scoped_types.rust_type("BaseClass") == "i32",
        "a nested typedef resolves under both spellings",
    )

    # no header path may resolve under both upstreams -------------------------- #
    # `check_no_shadowed_headers` walks *every* file under the metamod include
    # dirs against every file under the SDK's, in both directions: a collision in
    # a header either upstream includes indirectly is as unreadable as one in the
    # allowlist.
    shadow_root = SCRATCH / "selftest-shadowed-headers"

    def shadow_error(mutate) -> str:
        shutil.rmtree(shadow_root, ignore_errors=True)
        metamod_dir = shadow_root / "metamod"
        (metamod_dir / "core").mkdir(parents=True)
        (metamod_dir / "core" / "ISmmAPI.h").write_text("// the marker metamod's group needs\n")
        sdk_dir = shadow_root / "sdk" / "public"
        (sdk_dir / "tier0").mkdir(parents=True)
        mutate(metamod_dir, sdk_dir)
        try:
            check_no_shadowed_headers(metamod_dir, [str(sdk_dir)])
        except GenError as exc:
            return str(exc)
        return ""

    failed = shadow_error(
        lambda metamod_dir, sdk_dir: (
            (metamod_dir / "tier0").mkdir(),
            (metamod_dir / "tier0" / "platform.h").write_text("// metamod's\n"),
            (sdk_dir / "tier0" / "platform.h").write_text("// the SDK's\n"),
        )
    )
    ok(
        "tier0/platform.h" in failed
        and str(shadow_root / "metamod" / "tier0" / "platform.h") in failed
        and str(shadow_root / "sdk" / "public" / "tier0" / "platform.h") in failed,
        f"a metamod-dir header named like an s2sdk-included one is refused, naming "
        f"both paths: {failed!r}",
    )
    failed = shadow_error(
        lambda metamod_dir, sdk_dir: (
            (metamod_dir / "platform.h").write_text("// metamod's\n"),
            (sdk_dir / "platform.h").write_text("// the SDK's\n"),
        )
    )
    ok(
        str(shadow_root / "metamod" / "platform.h") in failed
        and str(shadow_root / "sdk" / "public" / "platform.h") in failed,
        f"the other direction too (an SDK-dir header the metamod dir also carries "
        f"at that path): {failed!r}",
    )
    # A file that is not a header cannot shadow an include (the two real
    # checkouts both have a README.md at their root and both must pass).
    failed = shadow_error(
        lambda metamod_dir, sdk_dir: (
            (metamod_dir / "README.md").write_text("// metamod's\n"),
            (sdk_dir / "README.md").write_text("// the SDK's\n"),
        )
    )
    ok(
        failed == "",
        f"a non-header file both checkouts carry at the same path is not a collision: "
        f"{failed!r}",
    )
    shutil.rmtree(shadow_root, ignore_errors=True)

    # an uninitialised khook submodule is refused, not read as the metamod HEAD - #
    khook_probe = SCRATCH / "selftest-khook-init"
    shutil.rmtree(khook_probe, ignore_errors=True)
    (khook_probe / gen.KHOOK_SUBDIR).mkdir(parents=True)
    missing = gen.khook_checkout_missing(khook_probe)
    ok(
        missing is not None
        and "khook submodule not initialised" in missing
        and f"git -C {gen.METAMOD_SUBMODULE} submodule update --init {gen.KHOOK_SUBDIR}" in missing,
        f"an uninitialised khook submodule names the init command: {missing!r}",
    )
    (khook_probe / gen.KHOOK_SUBDIR / ".git").write_text("gitdir: /nowhere\n")
    ok(
        gen.khook_checkout_missing(khook_probe) is None,
        "a khook directory with a .git (the file a submodule checkout has) passes",
    )
    ok(
        gen.khook_checkout_missing(gen.REPO_ROOT / gen.METAMOD_SUBMODULE) is None,
        "the checked-out khook submodule of this repo passes",
    )
    shutil.rmtree(khook_probe, ignore_errors=True)

    print(f"gen.py: self-test: {checks} checks passed")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(self_test())
    except Exception as exc:  # mirrors gen.py's dispatch shape (GenError included)
        print(f"gen_selftest.py: error: {exc}", file=sys.stderr)
        sys.exit(1)
