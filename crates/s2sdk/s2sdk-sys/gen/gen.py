#!/usr/bin/env python3
"""s2sdk-sys binding generator: upstream SDK headers -> IR JSON -> Rust.

Run as `python3 -I gen.py` (isolated mode; only the standard library is used).

    python3 -I gen.py                 write ir/ (both ABIs), src/generated/ and tests/layout.rs
    python3 -I gen.py --ir-only       stop after ir/<group>.json
    python3 -I gen.py --abi msvc      measure the Windows x64 (MSVC) side only -> target/s2sdk-gen/msvc/
    python3 -I gen.py --check         regenerate both ABIs into target/ and fail on any diff
    python3 -I gen.py --no-verify-cxx skip the clang static_assert re-derivation
    python3 -I gen.py --self-test     assert the dump parsers and emitters
                                      (the suite lives in gen_selftest.py)

The MSVC half of the IR (record layouts, vftable slots, enum sizes) needs the
Windows sysroot gen/fetch-xwin.sh installs into <repo>/target/xwin; point
$S2SDK_XWIN at an existing `xwin splat` directory to use another one. Every run
does both ABIs, because ir/<group>.json holds both.

What the pipeline is
--------------------
1. Locate the SDK: $S2SDK_DIR, else the `crates/s2sdk/upstream` git submodule of
   this repo (`git submodule update --init crates/s2sdk/upstream`). Read s2sdk-sys/UPSTREAM.lock
   and warn when the checkout's HEAD is not the pinned revision (`--check`
   exits 2 on a mismatch).
2. protoc: `devtools/bin/linux/protoc` (upstream's Linux build) generates the
   *.pb.h of every protobuf message a transitively included header needs into
   target/s2sdk-gen/pb/, which is then on every clang pass's include path
   (`with_pb_include`, both ABIs). Today one header needs it:
   engine2/isource2server.h includes network_connection.pb.h.
3. clang++ on gen/probe.cpp with the Linux include dirs and defines that are
   taken from the SDK's own CMakeLists.txt (its `${S2SDK_TARGET_PREFIX}_build_settings`
   target: C++ standard and defines; its three include dirs) plus the consumer
   include dirs (CONSUMER_INCLUDE_DIRS, the set hl2sdk-manifests gives a consumer),
   four passes:
     a. -Xclang -ast-dump=json                declarations, field types, virtual
                                              declaration order, enumerators;
     b. -Xclang -fdump-record-layouts         sizes, alignment, field offsets,
                                              base offsets, vtable pointer;
     c. -Xclang -fdump-vtable-layouts -S      Itanium vtable slots;
     d. a generated TU that instantiates one template per field size, enum size
        and enumerator value, dumped with -ast-dump-filter, because neither the
        layout dump nor the JSON AST prints those numbers.
   Scratch files live in <repo>/target/s2sdk-gen/, never /tmp.
4. Assemble s2sdk-sys/ir/<group>.json, one file per allowlist group, per the IR
   contract in crates/s2sdk/README.md.
5. Verify the IR against the compiler (`verify_ir_cxx`): a generated TU restates
   every size / alignment / field offset / field size / enum size / enumerator
   value as a `static_assert`, compiled with `clang++ -fsyntax-only`. The
   numbers the script parsed out of clang's *text* dumps are therefore
   re-derived by the compiler, not merely re-read.
6. Emit s2sdk-sys/src/generated/<group>.rs (+ mod.rs) and
   s2sdk-sys/tests/layout.rs from the IR, rustfmt'ed with a pinned config
   so `cargo fmt --check` stays green. Nothing under src/generated/ or
   tests/layout.rs is ever written by hand.

The emitted Rust covers both ABIs. A record, specialization or vtable whose
numbers the two ABIs disagree on is emitted twice, behind `#[cfg(not(windows))]`
(the Itanium body) and `#[cfg(windows)]` (the MSVC one); an item the two agree
on keeps a single ungated body, so the Linux bindings of an identical layout are
byte-for-byte what they were. A Windows vtable struct is laid out by `msvc_slot`
(a destructor is one entry, an overload group is reversed, a slot nothing is
bound to becomes a `_padN` blob), an Itanium one by `itanium_slot` (a destructor
is two entries). `tests/layout.rs` carries one `mod` per ABI, each gated on its
own `cfg` and each asserting that ABI's numbers against `abi::slot(Abi::native(),
..)`; the Itanium module additionally reproduces every `msvc_slot` through
`abi::slot(Abi::Msvc, ..)`, so the MSVC rule is checked from Linux too.

Two kinds of entry are not types. `[consts]` is a group with no records at all:
each entry's expression is evaluated by clang in the generated probe TU (a string
one through a `constexpr auto` read back from the AST, a `derived` one out of the
record-layout dump), under both ABIs, and lands in ir/consts.json and
src/generated/consts.rs - see gen/README.md, "the consts kind"; the C header
carries all of them except `C_HEADER_EXCLUDED_CONSTS`. `specs` (optional
per group) lists class-template specializations; a specialization's inherited
members are flattened into a record-shaped entry that is emitted under a
Rust-safe tag (`CUtlVector<CEntityHandle>` -> `CUtlVector_CEntityHandle`).

Two upstreams
-------------
Eighteen of the groups come from `crates/s2sdk/upstream` (pinned by
`UPSTREAM.lock`); the nineteenth, `metamod`, comes from a *second* checkout,
`crates/metamod/upstream` (Metamod:Source 2.0-dev, pinned by `METAMOD.lock`
together with its `third_party/khook` submodule - IKHook's vtable is generated
out of the submodule). The override is `$S2SDK_METAMOD_DIR`, the way `$S2SDK_DIR`
overrides the SDK, and `--check` holds *both* checkouts' HEADs (the khook
submodule included) to their locks (`locked_heads`, `lock_mismatches`).

The second upstream changes three things and nothing else:

1. its include dirs come first and its defines are appended: `META_NO_HL2SDK`
   keeps metamod's hl2sdk includes out of the TU (this repository has no hl2sdk)
   and `META_IS_SOURCE2` selects the CS2 reading of the rest, which is why the
   group's first header is the *s2sdk* `playerslot.h` (`ISmmAPI.h:70` constructs a
   `CPlayerSlot`). `parse_cmake` still describes the SDK alone; a header path both
   upstreams would resolve - checked for every header under their include dirs, in
   both directions, so a header either one includes indirectly counts too - fails
   the run (`check_no_shadowed_headers`), and an uninitialised khook submodule
   fails it before that (`khook_checkout_missing`: git would otherwise report the
   metamod repository's HEAD for it);
2. a class clang spells with its namespace (`SourceMM::ISmmPlugin`) is looked up
   by the bare name the AST, the allowlist and the IR use (`bare_type_name`,
   `bare_keyed`, `same_class`), and a *type* spelled that way inside a signature
   (`KHook::Action`, `std::size_t`) resolves through `TypeMapper.resolve_known` -
   which refuses a bare name more than one declaration carries (`AstSink.rekey`,
   `ambiguous_bare_names`), because the second upstream pulls in libstdc++/the
   MSVC STL where `BaseClass` and `IndexType_t` each have a couple of dozen
   declarations;
3. its numbers are not in the C header: `crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h` is generated from
   `c_header_groups()` (every group except `C_HEADER_EXCLUDED_GROUPS`), because the
   header is frozen at the s2sdk groups the retired C bridge consumed and must not
   move for a group no sigs `sdk` expression names. The same holds per `[consts]` entry measured from the metamod
   headers (`C_HEADER_EXCLUDED_CONSTS`, `c_header_consts`).

Everything else - the IR shape, the two ABIs, the emitters, the layout tests -
treats the group like any other, so `ir/metamod.json`, `src/generated/metamod.rs`
and the `mod.rs` re-export are produced by the same code paths (see
gen/README.md, "The second upstream").

Two ABIs, one IR
----------------
Every run generates both ABIs: the default one (the Itanium pass above, which
builds records, enums, vtables and the Rust bindings) and the MSVC pass, the
Windows x64 layout the bridge is compiled with. Both come from upstream: the
MSVC target is `--target=x86_64-pc-windows-msvc -fms-compatibility`, its defines
are the `COMPILER_MSVC` branch of the SDK's own CMakeLists.txt (parsed, never
hand-entered), and the MSVC CRT/STL + Windows SDK headers come from a sysroot
that gen/fetch-xwin.sh fetches into target/xwin (override with $S2SDK_XWIN;
parse_cmake fails loudly, naming the script, when it is missing).

What the MSVC pass measures is `--abi msvc`'s three passes:

1. `-fsyntax-only -Xclang -fdump-record-layouts` - record sizes, alignments and
   field offsets (target/s2sdk-gen/msvc/<group>.json, records.json);
2. `-S -Xclang -fdump-vtable-layouts` - the `VFTable` blocks, i.e. every
   method's slot from the address point. clang numbers the entries of the
   `VFTable indices for ...` block exactly like the Itanium dump numbers its
   `VTable indices` (the class's RTTI pointer sits at `vt[-1]`), which is why
   the two slot numbers are comparable at all;
3. the `S2SDK_` probe TU with `-ast-dump-filter=s2sdk_` - field sizes, enum
   sizes and the `[consts]` values, because neither dump prints those numbers.

The result is merged into the committed `ir/<group>.json`: every record and enum
gains an `"itanium"` and an `"msvc"` sub-object (size/align/fields, size/
underlying), and every vtable method an `msvc_slot` next to its `itanium_slot`.
The top-level numbers stay exactly what they were and stay the Itanium ones;
they are what a consumer that does not care about the ABI reads, and the Rust
emitter reads the sub-object of the ABI it is emitting, so a body whose two ABIs
agree is still byte-identical to what a single-ABI emitter produced. `facts.json`
under target/s2sdk-gen/msvc/ holds the dump summary (per-class entry counts and
the slot map) for reading by hand; it is scratch, like everything under target/.

Three things the MSVC side does differently, and what the generator does about
them:

* A virtual destructor is **one** entry (`[scalar deleting]`), where Itanium
  lays out two (complete + deleting). A destructor entry without that marker is
  an error. The entry the Rust emitter types is the compiler-generated scalar-
  deleting wrapper (`void* (T*, unsigned flags)`, see `msvc_dtor_type`), not
  the declared `~T()`.
* The overloads of one name are **packed together at the position of the first
  declaration, in reverse declaration order** (this is why the bridge pins
  `FindRawClassBinding` at slot 25 while Itanium has it at 24). The generator
  checks clang's dump against that rule per name group and fails if a group is
  not reversed and contiguous, rather than emit a slot the rule disagrees with;
  the generated `tests/layout.rs` then asserts every `msvc_slot` against
  `abi::slot(Abi::Msvc, ..)`, which is where the rule lives.
* A vftable that needs a this-adjusting thunk or a vbtable (virtual base)
  cannot be expressed in the IR at all: the pass fails on any of them for an
  allowlisted class (`[this adjustment: ...]`, the `-- accessible via vbtable
  ... --` section headers of an indices block, and the record layouts' `vbtable
  pointer` / virtual base lines). A secondary base (a second vfptr) is modelled
  as the primary chain only: the class's IR vtable is its primary table, the
  secondary base is its own IR vtable (ISoundOpSystem : IAppSystem,
  ISoundEventManager), and the class may not override a secondary base's
  virtual, however deep in the primary chain (GCC would add an entry to the
  primary table that MSVC does not; build_vtable refuses it, the MSVC indices
  check is the backstop). The base subobject's offset per ABI is emitted.

Two more MSVC-only spellings are parsed: the vtable pointer prints as `(X
vftable pointer)` and the `[sizeof=...]` line has no `dsize=`. The MSVC probe TU
cannot use the `#define private public` trick the Itanium one uses (the MSVC
STL's `xkeycheck.h` makes that a hard error), so it reaches private members with
`-fno-access-control` instead. Paths in record names and field types are
rewritten relative to the repository, because clang spells an anonymous record
with the absolute path of the header it was declared in.

The pass parses clang's text dumps and the compiler then re-derives every number
it read: `verify_ir.cpp` (static_asserts) is compiled once with the Itanium flags
against the top-level numbers and once with the MSVC flags against the `"msvc"`
sub-objects.

What clang gives and what it does not (the parts that are easy to get wrong)
---------------------------------------------------------------------------
* Record layouts are text (`-fdump-record-layouts`), not JSON: the JSON AST has
  no offsets. A record is only dumped once something lays it out, so probe.cpp
  takes `sizeof` of every allowlisted record.
* Vtables are text too, and clang only emits them at codegen time, and only for
  a class the TU uses: pass (c) compiles to assembly and probe.cpp takes the
  address of one virtual that the class declares itself (an inherited method
  only marks the base's vtable). `--verbose` reports a class whose vtable clang
  did not print.
* The JSON AST prints a location's `file`/`line` only when they differ from the
  *previously dumped* location (clang's size optimisation), so a decl's file is
  not in its own node. This script replays clang's location de-duplication in
  dump order (`ast_walk`) and then verifies every attribution against the
  source: the declared name has to appear on the recorded line of the recorded
  header, and that header has to be one the allowlist lists for the group. A
  desync fails the run instead of mislabelling a header.
* The JSON AST has no enumerator values and no field sizes; both come from pass
  (d), where the value is an integral template argument, so the compiler
  evaluates it (never this script).
* `#if 0` blocks and commented-out declarations never reach the AST, so they are
  excluded from vtable slots by construction; overloads keep declaration order.

`--check` regenerates into target/s2sdk-gen/check/ and diffs against the
committed ir/, so an upstream bump that changes a layout fails the check. The
Rust output is diffed the same way, which is what keeps the committed bindings
and the committed tests in step with the committed IR.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import re
import shutil
import subprocess
import sys
import tomllib
from collections.abc import Callable, Collection, Mapping, Sequence
from pathlib import Path
from typing import TypeVar

# --------------------------------------------------------------------------- #
# paths
# --------------------------------------------------------------------------- #

GEN_DIR = Path(__file__).resolve().parent
S2SDK_SYS_DIR = GEN_DIR.parent
REPO_ROOT = S2SDK_SYS_DIR.parent.parent.parent
IR_DIR = S2SDK_SYS_DIR / "ir"
GENERATED_DIR = S2SDK_SYS_DIR / "src" / "generated"
LAYOUT_TEST = S2SDK_SYS_DIR / "tests" / "layout.rs"
PROBE_CPP = GEN_DIR / "probe.cpp"
ALLOWLIST_TOML = GEN_DIR / "allowlist.toml"
UPSTREAM_LOCK = S2SDK_SYS_DIR / "UPSTREAM.lock"
METAMOD_LOCK = S2SDK_SYS_DIR / "METAMOD.lock"
SCRATCH = REPO_ROOT / "target" / "s2sdk-gen"
FETCH_XWIN_SH = GEN_DIR / "fetch-xwin.sh"

# `python3 -I` (isolated mode) leaves the script's directory off sys.path, and
# there was no existing sibling import in gen/ to take the pattern from: the
# self-test suite (gen_selftest.py) sits next to gen.py, so the directory has
# to be inserted explicitly for the import below to work at all.
if str(GEN_DIR) not in sys.path:
    sys.path.insert(0, str(GEN_DIR))
import gen_selftest  # noqa: E402 - after the paths block, by design (see above)

GROUPS = ("tier0", "schema", "entity", "cvar", "events", "engine", "toolframework2", "rendersystem", "scenesystem", "soundsystem", "particles", "client", "datamodel", "toolutils2", "pet", "modeldoc_editor", "mapdoclib", "hammer", "metamod", "consts")

# The second upstream: the group whose headers come from crates/metamod/upstream
# (Metamod:Source 2.0-dev) and its third_party/khook submodule, not from
# crates/s2sdk/upstream. It sits after every s2sdk group - the metamod headers
# need the s2sdk ones (`playerslot.h` for `CPlayerSlot`) to be included first -
# and before `consts`, which is measured last by design.
METAMOD_GROUP = "metamod"

# Groups the *C* header (crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h) is not generated from. The C
# bridge (frozen at m2.20, since retired) was compiled from the s2sdk IR only, and
# the header still carries exactly that: the sigs `sdk` expressions evaluated
# against it name s2sdk types, and the Rust Metamod plugin reads metamod.rs, so
# emitting the metamod group into the header would be a gratuitous change to a
# file `gen.py --check` holds byte for byte.
# Removing a group from this tuple is what makes the C header carry it.
C_HEADER_EXCLUDED_GROUPS = (METAMOD_GROUP,)


def c_header_groups() -> tuple[str, ...]:
    """The groups the C header is generated from, in GROUPS order."""
    return tuple(group for group in GROUPS if group not in C_HEADER_EXCLUDED_GROUPS)


# The `[consts]` entries the *C* header is not generated from, for the same
# reason the metamod group is not in it: they are measured from the
# Metamod:Source checkout (`core/ISmmPluginExt.h`, khook.hpp), and the header
# carries the s2sdk side only. They stay in `consts.json`
# and `src/generated/consts.rs` - a Rust Metamod plugin is their consumer.
# Removing a name from this tuple is what makes the C header carry it.
C_HEADER_EXCLUDED_CONSTS = (
    "METAMOD_PLAPI_VERSION",
    "METAMOD_PLAPI_NAME",
    "MMIFACE_PLMANAGER",
    "INVALID_HOOK",
    "PL_MIN_ID",
)


def c_header_consts(ir_by_group: dict) -> list[dict]:
    """The `[consts]` entries `emit_c_header` emits into the C
    header: every measured value the C header may carry (see
    C_HEADER_EXCLUDED_CONSTS)."""
    return [
        const
        for const in ir_by_group[CONST_GROUP]["consts"]
        if const["name"] not in C_HEADER_EXCLUDED_CONSTS
    ]

# The keys every type group carries. `consts` is the exception: it has no
# records/enums/vtables at all, only `entries` (macros/constexprs whose values
# clang measures). It is still a group so its header list joins probe.cpp's
# includes and its IR lands in ir/consts.json + src/generated/consts.rs.
GROUP_TYPE_KEYS = ("records", "opaque", "enums", "vtables")
# A group's optional `platforms = [...]` (default: both). The only restriction
# the pipeline models is Windows-only: the tool modules (toolframework2, hammer,
# pet, modeldoc_editor) have no Linux build, so no Itanium number for them is a
# claim about any binary. The IR carries `"platforms": ["windows"]` for such a
# group (and no key at all for an unrestricted one, so existing IR files stay
# byte-identical).
PLATFORM_WINDOWS = "windows"
PLATFORM_LINUX = "linux"
PLATFORMS_ALL = [PLATFORM_WINDOWS, PLATFORM_LINUX]
PLATFORMS_WINDOWS_ONLY = [PLATFORM_WINDOWS]
CONST_GROUP = "consts"
CONST_KINDS = ("int", "string")
# The Rust types a measured `[consts]` value may be emitted as. The generator
# checks the measured value fits, so a wrong width fails the run.
CONST_TYPES = ("i32", "u32", "i64", "u64")
CONST_TRANSFORMS = ("mask",)

# What a `[checks]` expectation may ask for: the top-level (Itanium) number, its
# `msvc_*` twin from the `"msvc"` sub-object, the two slots per method, a vtable
# length (`length` = Itanium, `msvc_length` = the Windows vftable clang printed)
# and a `[consts]` value (`value` = the Itanium one, `msvc_value` = the Windows
# one).
CHECK_FIELDS = frozenset(
    {
        "size",
        "align",
        "offset",
        "bit_offset",
        "bit_width",
        "msvc_size",
        "msvc_align",
        "msvc_offset",
        "msvc_bit_offset",
        "msvc_bit_width",
        "decl_index",
        "itanium_slot",
        "msvc_slot",
        "length",
        "msvc_length",
        "value",
        "itanium_value",
        "msvc_value",
        "last_slot",
    }
)

# The two CHECK_FIELDS that only a `<group>.<vtable>` key may carry.
VTABLE_LENGTH_FIELDS = ("length", "msvc_length")

# The one CHECK_FIELD that only a `<group>.<vtable>.<method>` key may carry:
# "this method is the vtable's last entry" (the slot the ABI's last one starts
# at, so a destructor - two Itanium entries, one MSVC one - is spelled too).
LAST_SLOT_FIELD = "last_slot"

# `abi::dtor_slots` for the Itanium ABI: a destructor occupies two entries (the
# complete-object and the deleting one). Only the Itanium vtable-length walk
# needs it; the MSVC length comes from clang's vftable dump, where a destructor
# is one slot.
ITANIUM_DTOR_SLOTS = 2

# The two ABIs the committed IR holds numbers for. The Itanium (Linux x86_64)
# one is the default and the top-level numbers of ir/; the MSVC (Windows x64)
# one is what the bridge is compiled with. Both ABIs' Rust is emitted: a
# record/vtable body the numbers reach is emitted per ABI and cfg-gated when the
# two disagree.
ABI_DEFAULT = "itanium"
ABI_MSVC = "msvc"
ABIS = (ABI_DEFAULT, ABI_MSVC)
ABI = "itanium-x86_64"
ABI_MSVC_ABI = "msvc-x86_64"

# The `--abi msvc` target line and sysroot. XWIN is xwin's `splat` layout: the
# MSVC CRT + STL under crt/include and the Windows SDK headers under sdk/.
MSVC_TARGET = ("--target=x86_64-pc-windows-msvc", "-fms-compatibility")
XWIN_ENV = "S2SDK_XWIN"
XWIN_DEFAULT = REPO_ROOT / "target" / "xwin"
XWIN_INCLUDE_SUBDIRS = (
    "crt/include",
    "sdk/include/ucrt",
    "sdk/include/um",
    "sdk/include/shared",
)

# The second upstream (see the module docstring, "two upstreams"): where the
# checkout lives, the marker header that says it is one, and the KHook submodule
# IKHook is generated from. The env override exists for the negative self-check
# (a mutated copy under target/scratch/), the same way S2SDK_DIR does for the SDK.
METAMOD_ENV = "S2SDK_METAMOD_DIR"
METAMOD_SUBMODULE = Path("crates") / "metamod" / "upstream"
METAMOD_MARKER = Path("core") / "ISmmAPI.h"
KHOOK_SUBDIR = Path("third_party") / "khook"
# The files `check_no_shadowed_headers` compares: a path both upstreams carry
# under one of these suffixes is one `#include` could reach either way. A
# README/CMakeLists that both checkouts have at the same relative path is not a
# header and cannot shadow anything.
HEADER_SUFFIXES = (".h", ".hh", ".hpp", ".hxx", ".inc", ".inl", ".ipp", ".tpp")

# rustfmt is part of the toolchain the workspace pins; every emitted .rs goes
# through it with exactly these options so `cargo fmt --check` cannot flap.
# `skip_children` keeps rustfmt from following `mod tier0;` out of the scratch
# file it is formatting (the module tree is only complete after the write).
RUSTFMT = (
    "rustfmt",
    "--edition",
    "2021",
    "--config",
    "newline_style=Unix,skip_children=true",
)

VERBOSE = False
QUIET = False
VERIFY_CXX = True


class GenError(Exception):
    """A reason the run cannot produce trustworthy IR."""


def log(msg: str) -> None:
    if not QUIET:
        print(f"gen.py: {msg}", file=sys.stderr)


def vlog(msg: str) -> None:
    if VERBOSE:
        log(msg)


# --------------------------------------------------------------------------- #
# configuration: SDK location, lock, CMake flags
# --------------------------------------------------------------------------- #


@dataclasses.dataclass
class CmakeFlags:
    std: str
    includes: list[str]
    defines: list[str]
    # Only the MSVC pass sets these: the Itanium pass leaves them empty, which
    # keeps its command line exactly what it was before the second ABI existed.
    target: tuple[str, ...] = ()
    system_includes: tuple[str, ...] = ()
    sysroot: Path | None = None

    def clang_flags(self, extra_includes: list[str] = ()) -> list[str]:
        flags = [self.std]
        flags += list(self.target)
        flags += [f"-D{d}" for d in self.defines]
        for directory in self.system_includes:
            flags += ["-isystem", directory]
        flags += [f"-I{p}" for p in list(self.includes) + list(extra_includes)]
        return flags


def validate_allowlist(allowlist: dict) -> None:
    """Every group the pipeline knows about needs all five tables; a missing key
    is a broken allowlist, not a crash. The `consts` group is the exception: it
    has headers and `entries`, and must not have type tables."""
    for group in GROUPS:
        if group not in allowlist:
            raise GenError(f"allowlist.toml has no [{group}] table")
        if group == CONST_GROUP:
            if "platforms" in allowlist[group]:
                raise GenError(
                    f"allowlist.toml [{group}]: `platforms` is not allowed on the consts "
                    f"group (its values are measured under both ABIs)"
                )
            for key in ("headers", "entries"):
                if key not in allowlist[group]:
                    raise GenError(f"allowlist.toml [{group}] has no '{key}'")
            validate_consts(allowlist[group])
            continue
        group_platforms(allowlist, group)
        for key in ("headers", *GROUP_TYPE_KEYS):
            if key not in allowlist[group]:
                raise GenError(f"allowlist.toml [{group}] has no '{key}'")
            if not isinstance(allowlist[group][key], list):
                raise GenError(f"allowlist.toml [{group}].{key} must be a list")
            for name in allowlist[group][key]:
                if not isinstance(name, str):
                    raise GenError(f"allowlist.toml [{group}].{key} has a non-string entry")
        overlap = set(allowlist[group]["records"]) & set(allowlist[group]["opaque"])
        if overlap:
            raise GenError(
                f"allowlist.toml [{group}]: {sorted(overlap)} is both a record and opaque"
            )
        specs = group_specs(allowlist, group)
        for name in specs:
            if not isinstance(name, str) or "<" not in name or not name.endswith(">"):
                raise GenError(
                    f"allowlist.toml [{group}].specs: {name!r} is not a class-template "
                    f"specialization spelling (`CUtlVector<CEntityHandle>`)"
                )
    for key, value in allowlist.get("checks", {}).items():
        if not isinstance(value, dict) or len(key.split(".")) not in (2, 3):
            raise GenError(f"allowlist.toml [checks] {key}: expected <group>.<type>[.<member>]")
        unknown = set(value) - CHECK_FIELDS
        if unknown:
            # Silently ignoring a key would let `msvc_slots = 25` pass as a check.
            raise GenError(
                f"allowlist.toml [checks] {key}: unknown {sorted(unknown)}; known: "
                f"{', '.join(sorted(CHECK_FIELDS))}"
            )


def group_platforms(allowlist: dict, group: str) -> list[str]:
    """The group's `platforms`: absent means both; `["windows"]` is the one
    restriction modelled (see PLATFORM_WINDOWS). Anything else is an error rather
    than a silently ignored key."""
    table = allowlist.get(group) or {}
    raw = table.get("platforms")
    if raw is None:
        return list(PLATFORMS_ALL)
    if (
        not isinstance(raw, list)
        or not raw
        or not all(isinstance(p, str) for p in raw)
        or len(set(raw)) != len(raw)
    ):
        raise GenError(
            f"allowlist.toml [{group}].platforms must be a non-empty list of distinct strings"
        )
    unknown = sorted(set(raw) - set(PLATFORMS_ALL))
    if unknown:
        raise GenError(
            f"allowlist.toml [{group}].platforms: unknown {unknown}; known: {PLATFORMS_ALL}"
        )
    if sorted(raw) == sorted(PLATFORMS_ALL):
        return list(PLATFORMS_ALL)
    if raw != PLATFORMS_WINDOWS_ONLY:
        raise GenError(
            f"allowlist.toml [{group}].platforms = {raw}: only both platforms or "
            f'["windows"] are supported (a Linux-only group has no MSVC numbers to merge)'
        )
    return list(PLATFORMS_WINDOWS_ONLY)


def ir_windows_only(ir_group: dict) -> bool:
    """True for a group the IR marks Windows-only (`"platforms": ["windows"]`):
    its Itanium numbers are a portability proof of the header, not a claim."""
    return ir_group.get("platforms") == PLATFORMS_WINDOWS_ONLY


def windows_only_groups(ir_by_group: dict) -> list[str]:
    return [g for g in GROUPS if g in ir_by_group and ir_windows_only(ir_by_group[g])]


def group_specs(allowlist: dict, group: str) -> list:
    """The group's `specs` list, or []: the key is optional (only the groups that
    have a class-template specialization in the IR carry it)."""
    table = allowlist.get(group) or {}
    value = table.get("specs")
    if value is None:
        return []
    if not isinstance(value, list):
        raise GenError(f"allowlist.toml [{group}].specs must be a list")
    return value


@dataclasses.dataclass(frozen=True)
class ConstEntry:
    """One `[consts]` entry: a value clang measures, either by evaluating `expr`
    or by reading `derived` (`<record>.<member>.<what>`) out of its layout dump."""

    name: str
    kind: str  # "int" | "string"
    rust_type: str  # "u32" ... for an int, "&str" for a string
    expr: str | None
    derived: tuple[str, str, str] | None
    transform: str | None
    per_abi: bool
    doc: str | None

    @property
    def tag(self) -> str:
        """The probe tag the value is measured under."""
        return f"s2sdk_const_{self.name}"


_CONST_FIELDS = frozenset(
    {"name", "expr", "derived", "kind", "type", "transform", "per_abi", "doc"}
)
_CONST_WHAT = ("offset", "bit_offset", "bit_width")


def parse_consts(table: dict) -> list[ConstEntry]:
    """The validated `[consts]` entries, in file order."""
    entries = []
    for raw in table.get("entries") or []:
        entries.append(parse_const(raw))
    if not entries:
        raise GenError("allowlist.toml [consts].entries is empty")
    seen = set()
    for entry in entries:
        if entry.name in seen:
            raise GenError(f"allowlist.toml [consts]: {entry.name} is listed twice")
        seen.add(entry.name)
    return entries


def validate_consts(table: dict) -> None:
    """A `[consts]` table is valid when every entry parses."""
    parse_consts(table)


def parse_const(raw: dict) -> ConstEntry:
    if not isinstance(raw, dict):
        raise GenError(f"allowlist.toml [consts]: {raw!r} is not a table")
    unknown = set(raw) - _CONST_FIELDS
    if unknown:
        raise GenError(f"allowlist.toml [consts]: unknown key(s) {sorted(unknown)}")
    name = raw.get("name")
    if not isinstance(name, str) or not name.isidentifier() or name in RUST_KEYWORDS:
        raise GenError(f"allowlist.toml [consts]: {name!r} is not a Rust identifier")
    has_expr, has_derived = "expr" in raw, "derived" in raw
    if has_expr == has_derived:
        raise GenError(f"allowlist.toml [consts] {name}: give exactly one of `expr`, `derived`")
    if has_expr and not isinstance(raw["expr"], str):
        raise GenError(f"allowlist.toml [consts] {name}: `expr` must be a string")

    kind = raw.get("kind", "int")
    if kind not in CONST_KINDS:
        raise GenError(f"allowlist.toml [consts] {name}: kind {kind!r} is not one of {CONST_KINDS}")
    declared = raw.get("type")
    if has_derived and declared is None:
        raise GenError(f"allowlist.toml [consts] {name}: `derived` needs a `type`")
    if kind == "string":
        if declared is not None:
            raise GenError(f"allowlist.toml [consts] {name}: a string const has no `type`")
        rust_type = "&str"
    else:
        if declared not in CONST_TYPES:
            raise GenError(
                f"allowlist.toml [consts] {name}: type {declared!r} is not one of {CONST_TYPES}"
            )
        rust_type = declared

    transform = raw.get("transform")
    if transform is not None:
        if not has_derived:
            raise GenError(f"allowlist.toml [consts] {name}: `transform` needs `derived`")
        if transform not in CONST_TRANSFORMS:
            raise GenError(
                f"allowlist.toml [consts] {name}: transform {transform!r} is not one of "
                f"{CONST_TRANSFORMS}"
            )

    derived = None
    if has_derived:
        parts = str(raw["derived"]).split(".")
        if len(parts) != 3 or parts[2] not in _CONST_WHAT:
            raise GenError(
                f"allowlist.toml [consts] {name}: derived {raw['derived']!r} is not "
                f"<record>.<member>.{'|'.join(_CONST_WHAT)}"
            )
        derived = (parts[0], parts[1], parts[2])
    per_abi = raw.get("per_abi", False)
    if not isinstance(per_abi, bool):
        raise GenError(f"allowlist.toml [consts] {name}: per_abi must be true or false")
    if kind == "string" and per_abi:
        raise GenError(f"allowlist.toml [consts] {name}: a string const cannot be per_abi yet")

    doc = raw.get("doc")
    if doc is not None and not isinstance(doc, str):
        raise GenError(f"allowlist.toml [consts] {name}: doc must be a string")
    return ConstEntry(
        name=name,
        kind=kind,
        rust_type=rust_type,
        expr=raw.get("expr"),
        derived=derived,
        transform=transform,
        per_abi=per_abi,
        doc=doc,
    )


def spec_tag(name: str) -> str:
    """The Rust-safe name a specialization is emitted under:
    `CUtlVector<CEntityHandle>` -> `CUtlVector_CEntityHandle`. Also the tag its
    probe.cpp S2SDK_FORCE_SPEC row has to carry, so the two stay in step."""
    return re.sub(r"\W+", "_", name).strip("_")


def find_specialization(sink: AstSink, name: str) -> tuple[str, dict] | None:
    """The AST's spelling of the specialization `name` (allowlist spelling, the
    way C++ source writes it) plus its node, or None.

    clang names a ClassTemplateSpecializationDecl with *every* template argument,
    defaults included: `CUtlVector<CEntityHandle>` is
    `CUtlVector<CEntityHandle, int, CUtlVectorMemory_Growable<CEntityHandle>>`
    there. The record-layout dump, on the other hand, prints the short spelling
    the code used, which is what the allowlist names - only the AST lookup needs
    the resolved one."""
    if name in sink.specializations:
        return name, sink.specializations[name][0]
    head = name[:-1] if name.endswith(">") else name
    matches = [
        full
        for full in sink.specializations
        if full.startswith(head) and full[len(head) : len(head) + 1] == ","
    ]
    if len(matches) != 1:
        return None
    return matches[0], sink.specializations[matches[0]][0]


def rust_int_fits(rust_type: str, value: int) -> bool:
    bits = {"i32": 32, "u32": 32, "i64": 64, "u64": 64}[rust_type]
    if rust_type.startswith("u"):
        return 0 <= value < (1 << bits)
    return -(1 << (bits - 1)) <= value < (1 << (bits - 1))


def find_sdk_dir() -> Path:
    env = os.environ.get("S2SDK_DIR")
    if env:
        sdk = Path(env).resolve()
        if not sdk.is_dir():
            raise GenError(f"S2SDK_DIR={env} is not a directory")
        return sdk

    candidate = REPO_ROOT / "crates" / "s2sdk" / "upstream"
    if (candidate / "public").is_dir():
        return candidate.resolve()
    raise GenError(
        "could not find the SDK: run `git submodule update --init crates/s2sdk/upstream`, "
        "or set S2SDK_DIR to an s2sdk checkout"
    )


def find_metamod_dir() -> Path:
    """The Metamod:Source checkout the `metamod` group is generated from:
    $S2SDK_METAMOD_DIR, else the `crates/metamod/upstream` submodule of this repo.
    The override is what the negative self-check uses to point the generator at a
    mutated copy of the headers under target/scratch/ (see gen/README.md, "the
    second upstream"); without the submodule there is no metamod group to
    generate, which is a failure naming the command, not an empty group."""
    env = os.environ.get(METAMOD_ENV)
    root = Path(env).expanduser().resolve() if env else (REPO_ROOT / METAMOD_SUBMODULE)
    where = f"{METAMOD_ENV}={env}" if env else str(root)
    if not (root / METAMOD_MARKER).is_file():
        raise GenError(
            f"could not find Metamod:Source: {where} has no {METAMOD_MARKER}.\n"
            f"Run `git submodule update --init {METAMOD_SUBMODULE}` (then "
            f"`git -C {METAMOD_SUBMODULE} submodule update --init {KHOOK_SUBDIR}`), "
            f"or point {METAMOD_ENV} at a Metamod:Source checkout."
        )
    uninitialised = khook_checkout_missing(root)
    if uninitialised:
        raise GenError(uninitialised)
    return root


def khook_checkout_missing(metamod: Path) -> str | None:
    """The message for an uninitialised (or empty) KHook submodule of `metamod`,
    or None when it is a checkout.

    Asked of the directory itself, not of git: `git -C
    <metamod>/third_party/khook rev-parse HEAD` walks up to the metamod
    repository and reports *its* HEAD, so an uninitialised submodule reads
    exactly like one that is at the metamod commit - and `METAMOD.lock` pins the
    khook revision separately, which that answer would silently satisfy. git
    leaves a `.git` file (or, for an old-style checkout, a directory) in the
    submodule it initialised; without it there is no khook checkout here."""
    khook = metamod / KHOOK_SUBDIR
    if (khook / ".git").exists():
        return None
    return (
        f"{khook} is not a git checkout, so the KHook submodule is not "
        f"initialised (a git command there would report the metamod "
        f"repository's revision). khook submodule not initialised: run "
        f"`git -C {METAMOD_SUBMODULE} submodule update --init {KHOOK_SUBDIR}`"
    )


def metamod_includes(metamod: Path) -> list[str]:
    """The include dirs the second upstream's headers need, first: the checkout
    itself (the allowlist spells its headers `core/ISmmAPI.h`) and KHook's
    `include/`, which is where `khook.hpp` lives (ISmmPlugin.h includes it bare,
    and metamod's own build puts that directory on the path the same way).

    The s2sdk dirs come *after* these (see `metamod_first_includes`): nothing a
    metamod header includes may be shadowed by an s2sdk header of the same name,
    so the upstream that owns the name gets its own file."""
    return [str(metamod), str(metamod / KHOOK_SUBDIR / "include")]


def metamod_first_includes(metamod: Path, includes: list[str]) -> list[str]:
    """`includes` (the s2sdk consumer dirs, in their pinned order) with the
    metamod dirs prepended. Pure, so `--self-test` can assert the order."""
    return metamod_includes(metamod) + list(includes)


def metamod_defines(defines: list[str]) -> list[str]:
    """The second upstream's defines, appended to the ones `parse_cmake` read out
    of the SDK's CMakeLists.txt. Both names are metamod's own - nothing in the
    s2sdk upstream mentions either - so they cannot change an existing group:

      * META_NO_HL2SDK (ISmmAPI.h:40, ISmmPlugin.h:41): skip the hl2sdk includes
        (`interfaces/interfaces.h`, `tier1/interface.h`, `convar.h`, `eiface.h`),
        which this repository does not carry. `edict_t`, `CGlobalVars` and
        `ConCommandBase` become the forward declarations/metamod typedefs the
        same #ifdef provides (ProviderConVar/ProviderConCommand = ConCommandBase).
      * META_IS_SOURCE2 (ISmmAPI.h:48, ISmmPlugin.h:42): the Source 2 (CS2) side
        of that, which this repository's headers are: `MMSPlayer_t` is
        `CPlayerSlot` (constructed at ISmmAPI.h:70, so `playerslot.h` is
        included first - see [metamod] in allowlist.toml).

    Adding them for *both* ABIs is the point: the group has to describe the same
    headers on both, and a define that only one ABI saw would make the two ABIs'
    numbers a claim about two different APIs."""
    return [*defines, "META_NO_HL2SDK", "META_IS_SOURCE2"]


def check_no_shadowed_headers(metamod: Path, sdk_includes: Sequence[str]) -> None:
    """No header path may resolve under both upstreams.

    The metamod include dirs go *first* on the path of every clang run of the
    run (`metamod_first_includes`), so a path both checkouts carry would make
    which file clang read depend on that order - and the IR records the header a
    declaration came from, which would then name a file the reader cannot place
    in one upstream. The metamod headers are spelled `core/...`/`khook.hpp`, so
    a collision means the second upstream grew a header the first one also has,
    or the first one a header the second one has: worth failing over, not
    guessing.

    Every *header* (HEADER_SUFFIXES; a README neither upstream includes is not
    one) under the metamod include dirs is compared against every one under the
    s2sdk include dirs by its path relative to the directory that holds it, **in
    both directions** - not only the allowlisted headers, because a header
    either upstream includes *indirectly* collides just as badly. The failure
    names both files."""
    metamod_dirs = [Path(p).resolve() for p in metamod_includes(metamod)]
    sdk_dirs = [Path(p).resolve() for p in sdk_includes]

    def files_by_relative_path(directory: Path) -> dict[str, Path]:
        if not directory.is_dir():
            return {}
        return {
            str(path.relative_to(directory)): path
            for path in sorted(directory.rglob("*"))
            if path.is_file() and path.suffix in HEADER_SUFFIXES
        }

    checked = [(directory, files_by_relative_path(directory)) for directory in metamod_dirs]
    against = [(directory, files_by_relative_path(directory)) for directory in sdk_dirs]
    seen: set[tuple[str, str]] = set()
    for direction in (checked, against):
        other = against if direction is checked else checked
        for directory, files in direction:
            for relative, path in files.items():
                for other_dir, other_files in other:
                    clash = other_files.get(relative)
                    if clash is None:
                        continue
                    pair = tuple(sorted((str(path), str(clash))))
                    if pair in seen:
                        continue
                    seen.add(pair)
                    raise GenError(
                        f"{path} and {clash} are the same header path relative to "
                        f"{directory} and {other_dir}; the two upstreams must not "
                        f"ship the same header path - which file a run read would "
                        f"depend on the include order, and the IR's header field "
                        f"would name a file that is in only one of them"
                    )


def read_lock() -> dict:
    return read_lock_file(UPSTREAM_LOCK, ("repo", "branch", "commit", "date"))


def read_metamod_lock() -> dict:
    """METAMOD.lock: the second upstream. Same shape as UPSTREAM.lock plus the
    KHook submodule's commit, because IKHook's vtable is generated out of
    third_party/khook (see `locked_heads`)."""
    return read_lock_file(
        METAMOD_LOCK,
        ("repo", "branch", "commit", "date", "khook_repo", "khook_commit", "khook_date"),
    )


def read_lock_file(path: Path, keys: Sequence[str]) -> dict:
    if not path.is_file():
        raise GenError(f"{path.relative_to(REPO_ROOT)} is missing")
    with path.open("rb") as handle:
        lock = tomllib.load(handle)
    for key in keys:
        if key not in lock:
            raise GenError(f"{path.name} has no '{key}'")
    return lock


@dataclasses.dataclass(frozen=True)
class LockedHead:
    """One checkout a lock file pins: the lock's name, where the checkout is, the
    pinned commit and the revision measured there (None when it is not a git
    checkout at all)."""

    lock: str
    what: str
    root: Path
    commit: str
    head: str | None


def locked_heads(sdk: Path, metamod: Path) -> list[LockedHead]:
    """Every checkout the run generates from, with the revision its lock pins.
    The KHook submodule is one of them: a metamod bump that moves third_party/
    khook moves IKHook's slots, so the lock has to cover both."""
    lock = read_lock()
    metamod_lock = read_metamod_lock()
    return [
        LockedHead("UPSTREAM.lock", str(sdk), sdk, lock["commit"], sdk_head(sdk)),
        LockedHead("METAMOD.lock", str(metamod), metamod, metamod_lock["commit"], sdk_head(metamod)),
        LockedHead(
            "METAMOD.lock",
            f"{metamod} (third_party/khook)",
            metamod / KHOOK_SUBDIR,
            metamod_lock["khook_commit"],
            sdk_head(metamod / KHOOK_SUBDIR),
        ),
    ]


def lock_mismatches(heads: Sequence[LockedHead]) -> list[str]:
    """One line per pinned checkout that is not at its locked revision. A
    checkout whose revision cannot be read counts as a mismatch: a lock nothing
    can be verified against is not a lock. Pure (the revisions are already
    measured), so `--self-test` can drive it with invented ones."""
    lines = []
    for entry in heads:
        if entry.head is None:
            lines.append(f"cannot read the git revision of {entry.root} ({entry.lock})")
        elif entry.head != entry.commit:
            lines.append(
                f"{entry.what} is at {entry.head[:12]}, {entry.lock} pins "
                f"{entry.commit[:12]}"
            )
    return lines


def sdk_head(sdk: Path) -> str | None:
    """The checkout's HEAD, or None when it is not a git checkout."""
    try:
        out = subprocess.run(
            ["git", "-C", str(sdk), "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
        )
    except OSError as exc:  # no git
        vlog(f"git rev-parse failed: {exc}")
        return None
    if out.returncode != 0:
        vlog(f"git rev-parse failed: {out.stderr.strip()}")
        return None
    return out.stdout.strip()


_CMAKE_COMMENT_RE = re.compile(r"#[^\n]*")
# The CMake target whose settings parse_cmake reads, and the include dirs a
# consumer of the headers gets (hl2sdk-manifests' cs2 manifest; upstream's
# CMakeLists lists only the first three). game/client and tools are gen.py's own
# additions, for probe.cpp's `#include "clientinput.h"` (game/client) and its
# `#include "pet/petapp.h"`, `#include "modeldoc_editor/modeldoc_editor_app.h"` and
# `#include "hammer/mapviewmgr.h"` (tools, which holds the ModelDoc and Hammer
# headers too): no consumer's include path has them.
CMAKE_SETTINGS_TARGET = "${S2SDK_TARGET_PREFIX}_build_settings"
CONSUMER_INCLUDE_DIRS = (
    "thirdparty/protobuf-3.21.8/src",
    "public",
    "public/engine",
    "public/mathlib",
    "public/tier0",
    "public/tier1",
    "public/entity2",
    "game/shared",
    "game/client",
    "tools",
    "game/server",
    "common",
)
_CMAKE_WORD_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*(?:=.*)?$")


def cmake_call_bodies(text: str, call: str) -> list[str]:
    """Every `call( ... )` body in the file, paren-counted (CMake calls can span
    lines and nest parens)."""
    bodies = []
    start = text.find(call)
    while start != -1:
        open_at = text.find("(", start)
        if open_at < 0:
            break
        depth = 0
        end = None
        for pos in range(open_at, len(text)):
            char = text[pos]
            if char == "(":
                depth += 1
            elif char == ")":
                depth -= 1
                if depth == 0:
                    end = pos
                    break
        if end is None:
            raise GenError(f"CMakeLists.txt: unbalanced {call}( )")
        bodies.append(text[open_at + 1 : end])
        start = text.find(call, end)
    return bodies


def find_xwin() -> Path:
    """The Windows sysroot the MSVC pass needs: $S2SDK_XWIN, else <repo>/target/xwin.
    Failing loudly here (before clang runs) is the point: the MSVC CRT/STL and
    Windows SDK headers cannot be improvised, and glibc/libstdc++ do not work
    with the MSVC target."""
    env = os.environ.get(XWIN_ENV)
    root = Path(env).expanduser().resolve() if env else XWIN_DEFAULT
    missing = [sub for sub in XWIN_INCLUDE_SUBDIRS if not (root / sub).is_dir()]
    if missing:
        where = f"{XWIN_ENV}={env}" if env else str(root)
        raise GenError(
            f"the MSVC ABI (--abi msvc) needs the Windows sysroot, but {where} has "
            f"no {', '.join(missing)}.\n"
            f"Fetch it (this downloads from Microsoft: MSVC CRT/STL + Windows SDK) with\n"
            f"    {os.path.relpath(FETCH_XWIN_SH, Path.cwd())}\n"
            f"or point {XWIN_ENV} at an existing xwin `splat` directory."
        )
    return root


def parse_cmake(sdk: Path, abi: str = ABI_DEFAULT) -> CmakeFlags:
    """The include dirs / defines the SDK gives its consumers, straight from
    upstream's CMakeLists.txt. Parsed (not hardcoded) so a bump that adds an
    include dir or define cannot silently change what the probe sees; failing to
    find them fails the run.

    The file has one `target_compile_definitions(s2sdk_headers ...)` per
    compiler, under `if(MSVC)`/`else()`, plus an unconditional one: `abi`
    selects which branch is read, so the MSVC pass gets `COMPILER_MSVC
    COMPILER_MSVC64 WIN32 WIN64 _ITERATOR_DEBUG_LEVEL=0` and never the GCC
    shims (`stricmp=strcasecmp`, `_GLIBCXX_USE_CXX11_ABI=0`, ...)."""
    if abi not in (ABI_DEFAULT, ABI_MSVC):
        raise GenError(f"unknown ABI {abi!r}; expected {ABI_DEFAULT} or {ABI_MSVC}")
    cmakelists = sdk / "CMakeLists.txt"
    if not cmakelists.is_file():
        raise GenError(f"{cmakelists} is missing")
    raw = cmakelists.read_text(encoding="utf-8", errors="replace")
    # Comments can contain parens; drop them before counting.
    text = _CMAKE_COMMENT_RE.sub("", raw)

    features = [
        body
        for body in cmake_call_bodies(text, "target_compile_features(" + CMAKE_SETTINGS_TARGET)
    ]
    if not features:
        raise GenError("CMakeLists.txt: no s2sdk_headers compile features")
    std_matches = [re.search(r"cxx_std_(\d+)", body) for body in features]
    std_matches = [m for m in std_matches if m]
    if not std_matches:
        raise GenError("CMakeLists.txt: s2sdk_headers has no cxx_std_NN feature")
    std = f"-std=c++{std_matches[0].group(1)}"

    cmake_includes = []
    for body in cmake_call_bodies(text, "target_include_directories(" + CMAKE_SETTINGS_TARGET):
        for raw_path in re.findall(r'"([^"]+)"', body):
            cmake_includes.append(
                raw_path.replace("${S2SDK_DIR}", str(sdk)).replace(
                    "${CMAKE_CURRENT_SOURCE_DIR}", str(sdk)
                )
            )
    if not any(p.endswith("/public") for p in cmake_includes):
        raise GenError(f"CMakeLists.txt: unusable include dirs: {cmake_includes}")
    # Since s2sdk 126ac2c8 the CMakeLists only describes the SDK's own build
    # (public, tier0, tier1); consumers get the rest of the include dirs from
    # hl2sdk-manifests, which we do not have, so the consumer set is spelled here
    # (the dirs the probe has always been compiled with, in the old order). A dir
    # upstream adds to its own build settings and that is not in the set fails
    # the run instead of silently dropping out of the probe.
    includes = [str(sdk / rel) for rel in CONSUMER_INCLUDE_DIRS]
    unknown = [p for p in cmake_includes if p not in includes]
    if unknown:
        raise GenError(
            f"CMakeLists.txt: include dirs {unknown} are not in CONSUMER_INCLUDE_DIRS"
        )

    # Each ABI reads the branch it compiles under and skips the other one; the
    # unconditional call (`X64BITS PLATFORM_64BITS`) is in neither and lands in
    # both.
    other = "COMPILER_GCC" if abi == ABI_MSVC else "COMPILER_MSVC"
    defines = []
    for body in cmake_call_bodies(text, "target_compile_definitions(" + CMAKE_SETTINGS_TARGET):
        if other in body:
            continue  # the other ABI's branch
        for token in body.split():
            if token in ("s2sdk_headers", "INTERFACE"):
                continue  # the call's own arguments
            if _CMAKE_WORD_RE.match(token):
                defines.append(token)
    if abi == ABI_DEFAULT:
        # libprotobuf.a and tier0 use the old std::string ABI (upstream's CMake
        # dropped this define along with the consumer settings)
        defines.append("_GLIBCXX_USE_CXX11_ABI=0")
    if not defines:
        raise GenError(f"CMakeLists.txt: no s2sdk_headers defines for the {abi} ABI")
    if abi == ABI_MSVC:
        required = (
            "X64BITS",
            "PLATFORM_64BITS",
            "COMPILER_MSVC",
            "COMPILER_MSVC64",
            "WIN32",
            "WIN64",
            "_ITERATOR_DEBUG_LEVEL=0",
        )
    else:
        required = ("X64BITS", "PLATFORM_64BITS", "_LINUX", "_GLIBCXX_USE_CXX11_ABI=0")
    for needed in required:
        if not any(d == needed or d.startswith(needed) for d in defines):
            raise GenError(
                f"CMakeLists.txt: s2sdk_headers defines for the {abi} ABI lack {needed}"
            )
    # The branch above is picked by the compiler marker each call body carries,
    # not by walking the if()/else()/endif() structure, so cross-check that no
    # define of the other ABI came along: a new MSVC-only call that forgot the
    # marker would otherwise silently land in the Linux command line.
    foreign = {
        ABI_MSVC: ("COMPILER_GCC", "_LINUX", "LINUX", "POSIX", "GNUC", "_GLIBCXX_USE_CXX11_ABI"),
        ABI_DEFAULT: ("COMPILER_MSVC", "COMPILER_MSVC64", "WIN32", "WIN64", "_ITERATOR_DEBUG_LEVEL"),
    }[abi]
    leaked = [d for d in defines if any(d == f or d.startswith(f) for f in foreign)]
    if leaked:
        raise GenError(
            f"CMakeLists.txt: the {abi} ABI picked up {leaked}, which belongs to "
            f"the other compiler's branch; the per-call marker check in "
            f"parse_cmake needs to look at the if()/else() structure instead"
        )

    if abi == ABI_MSVC:
        sysroot = find_xwin()
        return CmakeFlags(
            std=std,
            includes=includes,
            defines=defines,
            target=MSVC_TARGET,
            system_includes=tuple(str(sysroot / sub) for sub in XWIN_INCLUDE_SUBDIRS),
            sysroot=sysroot,
        )
    return CmakeFlags(std=std, includes=includes, defines=defines)


# --------------------------------------------------------------------------- #
# running the compiler
# --------------------------------------------------------------------------- #


def run(cmd: list[str], stdout_path: Path | None = None) -> str:
    vlog("run: " + " ".join(cmd))
    try:
        if stdout_path is not None:
            with stdout_path.open("w") as out:
                done = subprocess.run(cmd, stdout=out, stderr=subprocess.PIPE, text=True)
            captured = ""
        else:
            done = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            captured = done.stdout
    except FileNotFoundError as exc:
        raise GenError(f"{cmd[0]} not found ({exc}); install clang") from exc
    if done.returncode != 0:
        tail = "\n".join(done.stderr.strip().splitlines()[-20:])
        raise GenError(f"{cmd[0]} failed (exit {done.returncode}):\n{tail}")
    return captured


def clang(cfg: CmakeFlags, extra: list[str], source: Path, stdout_path: Path | None = None) -> str:
    cmd = ["clang++", *cfg.clang_flags(), *extra, str(source)]
    return run(cmd, stdout_path)


def record_layout_dump(cfg: CmakeFlags) -> str:
    """The `-fdump-record-layouts` text of gen/probe.cpp (clang pass 2).

    One invocation for both ABIs: everything that differs - the host target vs
    `--target=x86_64-pc-windows-msvc`, the defines, the sysroot - is already in
    `cfg`."""
    return clang(cfg, ["-fsyntax-only", "-Xclang", "-fdump-record-layouts"], PROBE_CPP)


# --------------------------------------------------------------------------- #
# step 2: protobuf headers
# --------------------------------------------------------------------------- #

_INCLUDE_RE = re.compile(r'^\s*#\s*include\s+"([^"]+)"', re.M)


def scan_needed_pb_headers(source: Path, include_dirs: list[str]) -> list[str]:
    """Every *.pb.h the source's transitive includes ask for, in first-seen
    order."""
    seen: set[Path] = set()
    found: list[str] = []
    stack = [source]
    while stack:
        current = stack.pop()
        try:
            text = current.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for name in _INCLUDE_RE.findall(text):
            # A *.pb.h is what protoc_step generates, so it does not exist on
            # the include path before the step runs: record it by name rather
            # than by resolving it (a checkout that ships one would not need
            # it generated either, but generating it into scratch/pb is
            # harmless and keeps the run independent of stray files).
            if name.endswith(".pb.h"):
                if name not in found:
                    found.append(name)
                continue
            candidates = [current.parent / name] + [Path(d) / name for d in include_dirs]
            for candidate in candidates:
                if candidate.is_file():
                    resolved = candidate.resolve()
                    if resolved in seen:
                        break
                    seen.add(resolved)
                    stack.append(resolved)
                    break
    return found


def find_proto(pb_header: str, sdk: Path) -> list[Path]:
    stem = pb_header[: -len(".pb.h")]
    candidates = [sdk / "common" / f"{stem}.proto"]
    candidates += sorted(sdk.glob(f"**/{stem}.proto"))
    existing = [c for c in candidates if c.is_file()]
    if not existing:
        raise GenError(f"no .proto for {pb_header} under {sdk}")
    return existing


def with_pb_include(cfg: CmakeFlags, scratch: Path) -> CmakeFlags:
    """`cfg` with protoc_step's output dir (scratch/pb) appended to its include
    path, so a header that includes a generated *.pb.h compiles in every clang
    pass of the run. The dir is the Itanium run's: the MSVC pass compiles the
    same probe.cpp and reads the same generated headers (protoc's output does
    not depend on the target)."""
    pb_dir = str(scratch / "pb")
    if pb_dir in cfg.includes:
        return cfg
    return dataclasses.replace(cfg, includes=[*cfg.includes, pb_dir])


def protoc_step(cfg: CmakeFlags, sdk: Path, scratch: Path) -> list[str]:
    """Generate the *.pb.h the probe's includes need into scratch/pb."""
    pb_dir = scratch / "pb"
    pb_dir.mkdir(parents=True, exist_ok=True)
    needed = scan_needed_pb_headers(PROBE_CPP, cfg.includes)
    if not needed:
        vlog(f"protoc: no *.pb.h in the probe's includes, nothing to generate")
        return []

    protoc = sdk / "devtools" / "bin" / "linux" / "protoc"
    if not protoc.is_file():
        raise GenError(f"{needed} need protoc, but {protoc} is missing")
    for pb_header in needed:
        for proto in find_proto(pb_header, sdk):
            proto_paths = [proto.parent] + [Path(d) for d in cfg.includes]
            cmd = [str(protoc)]
            cmd += [f"--proto_path={p}" for p in dict.fromkeys(proto_paths)]
            cmd += [f"--cpp_out={pb_dir}", str(proto)]
            run(cmd)
    log(f"protoc: generated {', '.join(needed)} into {pb_dir.relative_to(REPO_ROOT)}")
    return needed


# --------------------------------------------------------------------------- #
# step 3b: the record layout dump
# --------------------------------------------------------------------------- #


@dataclasses.dataclass
class LayoutMember:
    level: int
    offset: int
    bit_from: int | None
    bit_to: int | None
    type: str
    name: str | None
    is_vptr: bool = False

    @property
    def is_bitfield(self) -> bool:
        return self.bit_from is not None

    @property
    def bit_offset(self) -> int | None:
        return None if self.bit_from is None else self.offset * 8 + self.bit_from

    @property
    def bit_width(self) -> int | None:
        return None if self.bit_from is None else self.bit_to - self.bit_from + 1


@dataclasses.dataclass
class LayoutBase:
    name: str
    offset: int
    virtual: bool
    primary: bool


@dataclasses.dataclass
class RecordLayout:
    name: str
    kind: str
    size: int
    align: int
    has_vptr: bool
    bases: list[LayoutBase]
    members: list[LayoutMember]

    @property
    def own_members(self) -> list[LayoutMember]:
        """Direct members only: base subobjects and nested records dump their
        members at a deeper indent."""
        return [m for m in self.members if m.level == 1 and not m.is_vptr]


_LAYOUT_LINE_RE = re.compile(
    r"^\s*(?P<off>\d+)(?::(?P<bit_from>\d+)-(?P<bit_to>\d+))? \|(?P<indent> *)(?P<content>.*)$"
)
_LAYOUT_ATTR_RE = re.compile(r"^\s*\| (?P<content>.*)$")
_LAYOUT_HEADER_RE = re.compile(
    r"^(?:(?P<kind>struct|class|union) )?(?P<name>.+?)(?P<empty> \(empty\))?$"
)
_LAYOUT_SIZE_RE = re.compile(
    # Itanium prints `[sizeof=40, dsize=40, align=8,`; MSVC omits the `dsize`.
    r"^\[sizeof=(?P<size>\d+), (?:dsize=\d+, )?align=(?P<align>\d+),$"
)
_LAYOUT_BASE_RE = re.compile(
    r"^(?:struct|class|union) (?P<name>.+?) "
    r"\((?P<base>primary base|base|virtual base)\)(?P<empty> \(empty\))?$"
)
# Itanium spells it "vtable pointer", MSVC "vftable pointer".
_LAYOUT_VPTR_RE = re.compile(r"^\((?P<name>.+?) v(?:f)?table pointer\)$")


def parse_record_layouts(text: str) -> dict[str, RecordLayout]:
    """Parse `-fdump-record-layouts` output (AST layouts; IRgen blocks, which
    are verbose trees without offsets, are ignored)."""
    layouts: dict[str, RecordLayout] = {}
    for chunk in text.split("*** Dumping ")[1:]:
        lines = chunk.splitlines()
        if not lines or lines[0].strip() != "AST Record Layout":
            continue
        name = None
        kind = None
        size = align = None
        bases: list[LayoutBase] = []
        members: list[LayoutMember] = []
        has_vptr = False
        for line in lines[1:]:
            if not line.strip():
                break
            attribute = _LAYOUT_ATTR_RE.match(line)
            if attribute:
                content = attribute.group("content").strip()
                size_match = _LAYOUT_SIZE_RE.match(content)
                if size_match:
                    size = int(size_match.group("size"))
                    align = int(size_match.group("align"))
                continue
            match = _LAYOUT_LINE_RE.match(line)
            if not match:
                continue
            content = match.group("content")
            indent = len(match.group("indent"))
            if not content.strip():
                continue
            level = max(0, (indent - 1) // 2)
            if level == 0:
                head = _LAYOUT_HEADER_RE.match(content.strip())
                if not head:
                    raise GenError(f"record layout: cannot read header {content!r}")
                kind = head.group("kind")
                name = head.group("name").strip()
                continue
            if name is None:
                continue
            offset = int(match.group("off"))
            bit_from = match.group("bit_from")
            bit_to = match.group("bit_to")
            stripped = content.rstrip()
            if level == 1:
                vptr = _LAYOUT_VPTR_RE.match(stripped)
                if vptr:
                    has_vptr = True
                    members.append(
                        LayoutMember(
                            level=level,
                            offset=offset,
                            bit_from=None,
                            bit_to=None,
                            type=stripped,
                            name=None,
                            is_vptr=True,
                        )
                    )
                    continue
                base = _LAYOUT_BASE_RE.match(stripped)
                if base:
                    bases.append(
                        LayoutBase(
                            name=base.group("name").strip(),
                            offset=offset,
                            virtual=base.group("base").startswith("virtual"),
                            primary=base.group("base") == "primary base",
                        )
                    )
                    continue
            if stripped.endswith(")") or content != stripped:
                # An unnamed member (an anonymous struct/union, or an unnamed
                # bitfield): clang prints the type and nothing after it.
                member_type, member_name = stripped, None
            else:
                member_type, _, member_name = stripped.rpartition(" ")
                member_type = member_type.strip()
            members.append(
                LayoutMember(
                    level=level,
                    offset=offset,
                    bit_from=int(bit_from) if bit_from is not None else None,
                    bit_to=int(bit_to) if bit_to is not None else None,
                    type=member_type,
                    name=member_name,
                )
            )
        if name is None:
            continue
        if name.startswith("(lambda at "):
            # A closure type: no header can name it, and a lambda inside a
            # function template is one type per instantiation under the same
            # name (libstdc++'s atomic_wait.h since upstream 89096c5d).
            continue
        if size is None:
            raise GenError(f"record layout: {name}: no [sizeof=...] line")
        layout = RecordLayout(
            name=name,
            kind=kind,
            size=size,
            align=align,
            has_vptr=has_vptr,
            bases=bases,
            members=members,
        )
        if name in layouts and layouts[name] != layout:
            raise GenError(f"record layout: {name} dumped twice, differently")
        layouts.setdefault(name, layout)
    return layouts


# --------------------------------------------------------------------------- #
# step 3c: the vtable dump
# --------------------------------------------------------------------------- #


@dataclasses.dataclass
class VtableEntry:
    """One slot of a vtable, as clang prints it."""

    index: int
    text: str
    owner: str | None
    name: str | None
    is_dtor: bool
    markers: tuple[str, ...]
    # The entry with its `[...]` markers stripped but its trailing qualifiers
    # (`const`, ...) kept, plus the two halves of the function it names:
    # `<ret> <owner>::<name><params>`. `signature` is how the MSVC pass tells two
    # overloads apart (`const` is the only difference between some of them).
    body: str = ""
    owner_expr: str | None = None
    ret: str = ""
    params: str | None = None

    @property
    def qualified(self) -> str:
        return f"{self.owner}::{self.name}" if self.owner else (self.name or "")

    @property
    def signature(self) -> str:
        """`<return type>(<parameters>)<qualifiers>`, whitespace removed: the IR's
        `cxx_signature` spells the same function the same way."""
        if self.params is None or not self.owner_expr:
            return ""
        return _squash(self.ret + self.params)


# Both ABIs spell these blocks; Itanium says "Vtable"/"VTable indices", the
# MSVC dump "VFTable"/"VFTable indices" (and puts the class's own RTTI in slot
# 0 of the full block instead of an offset_to_top/RTTI preamble).
#
# MSVC names a full table by its chain: `VFTable for 'A' in 'B' in 'C'` is the
# most-derived class C's table (introduced by root A). The repeated ` in '...'
# group captures the LAST name, i.e. the class the dump is about - which is
# what msvc_full_block matches on (a single-level chain and Itanium's
# chain-free spellings still match, zero repetitions included).
_VTABLE_BLOCK_RE = re.compile(
    r"^(?P<kind>Vtable for|VTable indices for|VFTable for|VFTable indices for|"
    r"Construction vtable for) "
    r"'(?P<name>[^']+)'(?: in '(?P<in>[^']+)')* \((?P<count>\d+) entr(?:y|ies)\)\.$"
)
_THUNKS_BLOCK_RE = re.compile(
    r"^Thunks for '(?P<name>[^']+)' \((?P<count>\d+) entr(?:y|ies)\)\.$"
)
_VBASE_BLOCK_RE = re.compile(
    r"^Virtual base offset offsets for '(?P<name>[^']+)' "
    r"\((?P<count>\d+) entr(?:y|ies)\)\.$"
)
_VTABLE_LAYOUT_KINDS = ("Vtable for", "VFTable for")
_VTABLE_INDICES_KINDS = ("VTable indices for", "VFTable indices for")
# "-- accessible via vfptr at offset 8 --" / "-- accessible via vbtable index 1,
# vfptr at offset 0 --": a second vfptr (secondary base) or a vbtable (virtual
# base). Neither is modelled, so an allowlisted class that has one is an error.
_VTABLE_SECTION_RE = re.compile(r"^\s*--\s*(?P<what>.+?)\s*--\s*$")
# a section of a secondary vfptr (offset other than 0) that is not behind a vbtable
_VTABLE_SECONDARY_VFPTR_RE = re.compile(r"^accessible via vfptr at offset (?!0$)\d+$")
_THUNK_MARKER = "this adjustment"
_VBTABLE_MARKER = "vbtable"
_VTABLE_ENTRY_RE = re.compile(r"^\s*(?P<index>\d+) \| (?P<body>.+)$")
_MARKER_RE = re.compile(r"\s*\[[^\]]*\]\s*$")
_TRAILING_QUALIFIERS = ("const", "volatile", "noexcept", "override", "final", "&", "&&")

def _squash(text: str) -> str:
    """The canonical form of a C++ spelling: all whitespace removed. Two dumps
    that spell the same type with different spacing still compare equal."""
    return re.sub(r"\s+", "", text)


def _matching_paren(text: str, close_at: int) -> int | None:
    """Index of the `(` that `text[close_at]` (a `)`) closes."""
    depth = 0
    for pos in range(close_at, -1, -1):
        char = text[pos]
        if char == ")":
            depth += 1
        elif char == "(":
            depth -= 1
            if depth == 0:
                return pos
    return None


def parse_vtable_entry(index: int, text: str) -> VtableEntry:
    """`bool CSchemaType::IsValid()` -> owner CSchemaType, name IsValid."""
    body = text.strip()
    markers: list[str] = []
    while True:
        match = _MARKER_RE.search(body)
        if not match:
            break
        markers.append(body[match.start() :].strip())
        body = body[: match.start()].rstrip()
    # `body` now keeps the trailing qualifiers; `params`/`ret` need them to tell
    # `GetChangeAccessorPathInfo() const` from its non-const overload.
    qualified_body = body
    # Ref/const/noexcept qualifiers sit after the parameter list.
    stripped = True
    while stripped:
        stripped = False
        for qualifier in _TRAILING_QUALIFIERS:
            if body.endswith(" " + qualifier):
                body = body[: -(len(qualifier) + 1)].rstrip()
                stripped = True
                break

    owner = name = None
    owner_expr = None
    ret = ""
    params = None
    close_at = qualified_body.rfind(")")
    open_at = _matching_paren(qualified_body, close_at) if close_at >= 0 else None
    if close_at >= 0 and open_at is None:
        raise GenError(f"vtable entry: unbalanced parens: {text!r}")
    if open_at is not None:
        params = qualified_body[open_at:]
        head = qualified_body[:open_at].rstrip()
        if "::" in head:
            owner_expr, _, name = head.rpartition("::")
            token = owner_expr.split()[-1] if owner_expr.split() else owner_expr
            owner = token.lstrip("*&")
            # Drop the class name *as a suffix*: a return type that ends in `*`
            # abuts it (`void *ISchemaSystem::F`), so the `*` stays in `ret`.
            ret = owner_expr[: len(owner_expr) - len(owner)].strip()
    if not name:
        # Non-function slots: "offset_to_top (0)", "X RTTI", "vbase offset ...".
        head = body.split("(")[0].strip()
        name = head.split()[-1] if head.split() else head
        owner = None
        owner_expr = None
        params = None
        ret = ""
    return VtableEntry(
        index=index,
        text=text,
        owner=owner,
        name=name.strip(),
        is_dtor=name.strip().startswith("~"),
        markers=tuple(markers),
        body=qualified_body,
        owner_expr=owner_expr,
        ret=ret,
        params=params,
    )


@dataclasses.dataclass
class VtableBlock:
    """One `<kind> ... for 'X' [in 'Y'] (N entries).` block of a vtable dump.

    `kind` is `"layout"` (the full table, RTTI/offset_to_top preamble included),
    `"indices"` (the entries numbered from the address point), `"construction"`
    (an Itanium construction vtable for a virtual base), `"thunks"` or
    `"vbase"`; the last three are parsed only so an allowlisted class that needs
    one is rejected instead of silently misread."""

    kind: str
    name: str
    in_: str | None
    count: int
    entries: list[VtableEntry] = dataclasses.field(default_factory=list)
    sections: list[str] = dataclasses.field(default_factory=list)
    # entries listed under a `-- accessible via vfptr at offset N --` section with N != 0
    # (MSVC prints one section per vfptr when a class has a secondary base): not slots of
    # the primary table, kept apart so the class's primary slots stay readable
    secondary_entries: list[VtableEntry] = dataclasses.field(default_factory=list)
    extra: list[str] = dataclasses.field(default_factory=list)


def parse_vtable_blocks(text: str) -> list[VtableBlock]:
    """Every block of a `-fdump-vtable-layouts` dump, in dump order."""
    blocks: list[VtableBlock] = []
    current: VtableBlock | None = None
    in_secondary = False
    for line in text.splitlines():
        block = _VTABLE_BLOCK_RE.match(line)
        if block:
            kind = (
                "layout"
                if block.group("kind") in _VTABLE_LAYOUT_KINDS
                else "indices"
                if block.group("kind") in _VTABLE_INDICES_KINDS
                else "construction"
            )
            current = VtableBlock(
                kind=kind,
                name=block.group("name"),
                in_=block.group("in"),
                count=int(block.group("count")),
            )
            blocks.append(current)
            in_secondary = False
            continue
        thunks = _THUNKS_BLOCK_RE.match(line)
        if thunks:
            current = VtableBlock(
                kind="thunks", name=thunks.group("name"), in_=None, count=int(thunks.group("count"))
            )
            blocks.append(current)
            continue
        vbase = _VBASE_BLOCK_RE.match(line)
        if vbase:
            current = VtableBlock(
                kind="vbase", name=vbase.group("name"), in_=None, count=int(vbase.group("count"))
            )
            blocks.append(current)
            continue
        if not line.strip():
            current = None
            continue
        if current is None:
            continue
        section = _VTABLE_SECTION_RE.match(line)
        if section:
            current.sections.append(section.group("what"))
            in_secondary = _VTABLE_SECONDARY_VFPTR_RE.search(section.group("what")) is not None
            continue
        entry = _VTABLE_ENTRY_RE.match(line)
        if entry:
            parsed = parse_vtable_entry(int(entry.group("index")), entry.group("body"))
            (current.secondary_entries if in_secondary else current.entries).append(parsed)
            continue
        current.extra.append(line.strip())
    return blocks


def parse_vtables(text: str) -> dict[str, dict[str, list[VtableEntry]]]:
    """{class: {'layout': entries, 'indices': entries}} - 'indices' numbers slots
    from the address point, 'layout' includes the offset_to_top/RTTI preamble."""
    tables: dict[str, dict[str, list[VtableEntry]]] = {}
    for block in parse_vtable_blocks(text):
        if block.kind not in ("layout", "indices"):
            continue  # a construction vtable belongs to a virtual base
        tables.setdefault(block.name, {"layout": [], "indices": []})[block.kind].extend(
            block.entries
        )
    return tables


# --------------------------------------------------------------------------- #
# step 3a: the JSON AST, with clang's location de-duplication replayed
# --------------------------------------------------------------------------- #

RECORD_KINDS = ("CXXRecordDecl", "RecordDecl")


def is_location(obj: dict) -> bool:
    return (
        "offset" in obj
        or "tokLen" in obj
        or ("file" in obj and "line" in obj)
        # A location expanded from a macro is only the two sub-locations.
        or "spellingLoc" in obj
        or "expansionLoc" in obj
    )


def apply_location(obj: dict, state: list) -> None:
    """Update the 'file'/'line' state exactly the way clang's dumper does:
    listed keys are in dump order, `includedFrom` never touches the state."""
    for key, value in obj.items():
        if key in ("file", "line"):
            state[0 if key == "file" else 1] = value
        elif key == "includedFrom":
            continue
        elif key in ("spellingLoc", "expansionLoc", "begin", "end") and isinstance(value, dict):
            apply_location(value, state)


class AstSink:
    """Collects the declarations the generator needs, with the file/line clang
    was printing when it dumped them.

    `enums` and `typedefs` are keyed by the declaration's *qualified* name
    (`KHook::Action`, `SourceMM::ISmmPlugin::SomeTypedef`): the same bare name
    is routinely declared in more than one scope (the second upstream's headers
    pull in libstdc++/the MSVC STL, where `IndexType_t` and `BaseClass` each
    have a couple of dozen declarations), and a table keyed by the bare name
    silently answers with whichever declaration clang dumped first. `rekey()`
    turns them into the lookup tables the type mapper reads.
    """

    def __init__(self) -> None:
        self.records: dict[str, tuple[dict, str | None, int | None]] = {}
        self.enums: dict[str, tuple[dict, str | None, int | None]] = {}
        # Type-sugar tables (rule 6 of the IR contract): a typedef/alias name ->
        # the type clang desugars it to, and a full record-template
        # specialization name (`SchemaMetaInfoHandle_t<CSchemaClassInfo>`) -> its
        # definition, which carries `definitionData.canPassInRegisters`.
        self.typedefs: dict[str, tuple[dict, str | None, int | None]] = {}
        self.specializations: dict[str, tuple[dict, str | None, int | None]] = {}
        # Set by `rekey()`: the qualified tables with a bare alias per name only
        # one declaration carries, and the bare names that more than one does.
        self.enum_table: dict[str, tuple[dict, str | None, int | None]] = {}
        self.typedef_table: dict[str, tuple[dict, str | None, int | None]] = {}
        self.ambiguous: frozenset[str] = frozenset()

    def record(self, kind, name, node, loc, scope: tuple[str, ...] = ()) -> None:
        if name is None:
            return
        # Nodes reached through a `decl` back-reference carry only id/kind/name;
        # the definition is the one with the payload.
        if kind in RECORD_KINDS and node.get("completeDefinition"):
            # Records stay keyed by the bare name: the allowlist, the IR and the
            # record-layout dump all spell a class the way its own declaration
            # does (`ISmmPlugin`, not `SourceMM::ISmmPlugin`), and `bare_keyed`
            # is what makes the layout dump's qualified spelling find it.
            self.records.setdefault(name, (node, loc[0], loc[1]))
        elif kind == "EnumDecl":
            # A bare back-reference (id/kind/name, no location) comes first when a template
            # argument or a field names the enum before its declaration is dumped; it carries
            # neither the file nor the enumerators, so it must not win the setdefault.
            if "loc" in node:
                self.enums.setdefault(qualify(scope, name), (node, loc[0], loc[1]))
        elif kind in ("TypedefDecl", "TypeAliasDecl"):
            if isinstance(node.get("type"), dict):
                self.typedefs.setdefault(qualify(scope, name), (node, loc[0], loc[1]))
        elif kind == "ClassTemplateSpecializationDecl" and node.get("completeDefinition"):
            full = specialization_name(node)
            if full is not None:
                self.specializations.setdefault(full, (node, loc[0], loc[1]))

    def rekey(self) -> None:
        """Build the lookup tables the type mapper and the emitters read, from
        the qualified tables `record()` filled in.

        `bare_keyed`'s rule: an exact key wins, a bare alias is added only while
        exactly one declaration carries the bare name, and `ambiguous` names the
        bare names that more than one does - `TypeMapper.resolve_known` refuses
        those, so a qualified spelling that is not itself a declaration fails
        closed instead of resolving to a same-named declaration from another
        scope."""
        self.enum_table = bare_keyed(self.enums, "enum declarations")
        self.typedef_table = bare_keyed(self.typedefs, "typedef declarations")
        self.ambiguous = ambiguous_bare_names((*self.enums, *self.typedefs))


def qualify(scope: tuple[str, ...], name: str) -> str:
    """A declaration's fully qualified name: the enclosing namespaces/classes
    clang dumped around it, then its own name."""
    return "::".join((*scope, name))


def declaration_scope(node: dict) -> str | None:
    """The name `node`'s children are qualified by - a namespace (`KHook`,
    `SourceMM`) or a class/struct/union/enum name - else None for a node that
    declares no scope and for an anonymous one."""
    if node.get("kind") not in ("NamespaceDecl", "EnumDecl", *RECORD_KINDS):
        return None
    name = node.get("name")
    return name if isinstance(name, str) and name else None


def specialization_name(node: dict) -> str | None:
    """`SchemaMetaInfoHandle_t<CSchemaClassInfo>` for a class-template
    specialization node, spelled the way the IR spells the type."""
    name = node.get("name")
    if not name:
        return None
    args = []
    for child in node.get("inner", []):
        if child.get("kind") != "TemplateArgument":
            break
        arg_type = child.get("type") or {}
        qual = arg_type.get("qualType")
        if not qual and isinstance(child.get("value"), int):
            # an integral non-type argument (`CTier3AppSystem<IToolSystem2, 0>`)
            args.append(str(child["value"]))
            continue
        if not qual:
            return None
        args.append(qual.strip())
    if not args:
        return None
    return f"{name}<{', '.join(args)}>"


def ast_walk(root: dict, sink: AstSink) -> None:
    """Walk the JSON AST in dump order (clang emits keys in dump order, which is
    what makes the location state replayable), collecting declarations.

    `scope` is the stack of enclosing declaration names (namespaces and
    records/enums): a declaration is recorded under its qualified name, so two
    `Entry_t`s in different scopes stay apart in `AstSink.enums`/`typedefs`.
    The stack is trimmed to the depth the node was entered with when its
    "after" entry is popped, so a node's own name never qualifies itself."""
    state: list = [None, None]
    scope: list[str] = []
    stack: list[tuple] = [("node", root)]
    while stack:
        item = stack.pop()
        if item[0] == "after":
            _, kind, name, node, loc, outer = item
            del scope[outer:]
            sink.record(kind, name, node, loc, tuple(scope))
            continue
        node = item[1]
        entry_state = state[:]
        loc = entry_state
        children: list[dict] = []
        for key, value in node.items():
            if key == "includedFrom":
                continue
            if isinstance(value, dict):
                if is_location(value):
                    apply_location(value, state)
                    if key == "loc":
                        # After clang's own de-duplication the state *is* this
                        # declaration's file/line: an omitted "file"/"line"
                        # means "same as the previous one", and a macro
                        # expansion leaves the expansion location behind (the
                        # file the declaration is written in).
                        loc = state[:]
                else:
                    children.append(value)
            elif isinstance(value, list):
                children.extend(c for c in value if isinstance(c, dict))
        outer = len(scope)
        scope_name = declaration_scope(node)
        if scope_name is not None:
            scope.append(scope_name)
        stack.append(("after", node.get("kind"), node.get("name"), node, tuple(loc), outer))
        for child in reversed(children):
            stack.append(("node", child))
    sink.rekey()


def parse_ast_json_stream(text: str) -> list[dict]:
    """-ast-dump=json writes one object; with -ast-dump-filter it writes one per
    match, back to back."""
    decoder = json.JSONDecoder()
    objects = []
    index = 0
    while True:
        while index < len(text) and text[index] in " \t\r\n":
            index += 1
        if index >= len(text):
            break
        try:
            obj, index = decoder.raw_decode(text, index)
        except ValueError as exc:
            raise GenError(f"cannot parse the clang JSON dump: {exc}") from exc
        objects.append(obj)
    return objects


def parse_probe_values(objects: list[dict], templates: dict[str, str]) -> dict[str, int]:
    """{tag: value} from the integral template arguments of the generated
    probe TU: clang evaluates them, this script only reads the numbers."""
    values: dict[str, int] = {}
    for obj in objects:
        if obj.get("kind") != "ClassTemplateSpecializationDecl":
            continue
        if obj.get("name") not in templates:
            continue
        args = [c for c in obj.get("inner", []) if c.get("kind") == "TemplateArgument"]
        if len(args) < 2:
            continue
        tag_obj = args[0].get("type") or {}
        tag = tag_obj.get("qualType") or tag_obj.get("desugaredQualType")
        value = args[1].get("value")
        if tag is None or value is None:
            continue
        tag = tag.split("::")[-1].strip()
        values[tag] = value
    return values


# --------------------------------------------------------------------------- #
# rust type mapping
# --------------------------------------------------------------------------- #

PRIMITIVES = {
    "bool": "bool",
    "_Bool": "bool",
    "char": "::core::ffi::c_char",
    "signed char": "i8",
    "unsigned char": "u8",
    "int8": "i8",
    "uint8": "u8",
    "int8_t": "i8",
    "uint8_t": "u8",
    "int16": "i16",
    "uint16": "u16",
    "int16_t": "i16",
    "uint16_t": "u16",
    "short": "i16",
    "short int": "i16",
    "signed short": "i16",
    "signed short int": "i16",
    "unsigned short": "u16",
    "unsigned short int": "u16",
    "int32": "i32",
    "uint32": "u32",
    "int32_t": "i32",
    "uint32_t": "u32",
    "int": "i32",
    "signed": "i32",
    "signed int": "i32",
    "uint": "u32",
    "unsigned": "u32",
    "unsigned int": "u32",
    "int64": "i64",
    "uint64": "u64",
    "int64_t": "i64",
    "uint64_t": "u64",
    "long long": "i64",
    "long long int": "i64",
    "signed long long": "i64",
    "signed long long int": "i64",
    "unsigned long long": "u64",
    "unsigned long long int": "u64",
    "long": "i64",
    "unsigned long": "u64",
    "intp": "isize",
    "uintp": "usize",
    "intptr_t": "isize",
    "uintptr_t": "usize",
    "size_t": "usize",
    "ssize_t": "isize",
    "float": "f32",
    "float32": "f32",
    "vec_t": "f32",
    "double": "f64",
    "float64": "f64",
}

_ELABORATED_RE = re.compile(r"\b(?:struct|class|union|enum)\s+(?=[A-Za-z_])")


def normalize_cxx_type_name(text: str) -> str:
    """`struct SchemaMetaInfoHandle_t<class CSchemaType>` -> the name the IR and
    the Rust source spell. Used to look clang's record-layout blocks up by the
    type name the AST hands us."""
    previous = None
    while previous != text:
        previous = text
        text = _ELABORATED_RE.sub("", text)
    return text.strip()


_NAMESPACE_QUALIFIER_RE = re.compile(r"^(?:[A-Za-z_][A-Za-z0-9_]*::)+")

# The tables `bare_keyed` re-keys are keyed by a string and hold anything.
V = TypeVar("V")


def strip_namespace(name: str) -> str:
    """`KHook::Action` -> `Action`, `std::size_t` -> `size_t`, `SourceMM::ISmmPlugin`
    -> `ISmmPlugin`: the leading namespace qualifiers of a C++ name removed.

    Only the *leading* ones: a template argument carries its own `::`
    (`std::chrono::duration<long double, std::ratio<1, 1000000000> >` keeps its
    argument list, and a class name that is a template-id is never a bare name
    the IR looks up by hand)."""
    return _NAMESPACE_QUALIFIER_RE.sub("", name)


def bare_type_name(name: str) -> str:
    """The unqualified spelling of a C++ name clang prints with its namespace:
    `SourceMM::ISmmPlugin` -> `ISmmPlugin`, `KHook::Action` -> `Action`.

    The allowlist, the IR and the AST's own `name` field are all unqualified,
    while clang spells a class fully qualified in the record-layout dump
    (`class SourceMM::ISmmPlugin`), in the vtable dumps (`VTable for
    'SourceMM::ISmmPlugin'`, `bool SourceMM::ISmmPlugin::Load(...)`) and, inside
    another namespace, in a method's parameter list (`KHook::Action`,
    `std::size_t`). Only the second upstream needs this: every class the s2sdk
    headers declare sits in the global namespace."""
    return strip_namespace(normalize_cxx_type_name(name))


def bare_keyed(items: Mapping[str, V], what: str) -> dict[str, V]:
    """`items` keyed by the name the IR uses, for the tables clang spelled a class
    in (record layouts, vtable dumps).

    A key without a namespace is already the IR's name. A key *with* one
    (`SourceMM::ISmmPlugin`, which is how the second upstream's classes come out of
    clang) also gets a bare-keyed entry, so the IR's unqualified name finds it -
    but the exact spelling always wins: the s2sdk headers declare a global
    `Entry_t` next to a nested `ConVarRegList::Entry_t`, and the IR names the
    global one. When two *qualified* keys want the same bare name (both nested
    `Entry_t`s, in the same upstream), no alias is added at all: the lookup then
    misses, and the caller fails naming the class, instead of a record or slot
    coming from the class that happened to be dumped first."""
    merged = {normalize_cxx_type_name(key): value for key, value in items.items()}
    exact = set(merged)
    aliases: dict[str, str] = {}
    ambiguous: set[str] = set()
    for key, value in items.items():
        normalized = normalize_cxx_type_name(key)
        bare = strip_namespace(normalized)
        if bare == normalized or bare in exact or bare in ambiguous:
            continue
        claimed = aliases.get(bare)
        if claimed is None:
            aliases[bare] = normalized
            merged[bare] = value
            continue
        del merged[bare]
        del aliases[bare]
        ambiguous.add(bare)
        vlog(f"{what}: {claimed} and {normalized} both shorten to {bare}; no bare alias")
    return merged


def type_sugar_tables(
    sink: AstSink, allowlist_enums: Collection[str]
) -> tuple[dict[str, str], dict[str, str | None]]:
    """The type-sugar tables (rule 6 of the IR contract) the TypeMapper reads,
    from the AST's qualified enum/typedef declarations:

      * `typedefs`: a typedef/alias name -> the type clang desugars it to. The
        keys are `AstSink.typedef_table`'s: an exact spelling per declaration,
        plus a bare alias per name exactly one declaration carries.
      * `enum_underlying`: an enum outside the allowlist -> its fixed underlying
        type, or None when it has none. An allowlisted enum is left out: it has
        a newtype of its own in the IR and is never mapped to its underlying
        integer.

    Pure (the sink is already walked), so `--self-test` drives exactly the
    tables the run builds."""
    typedefs: dict[str, str] = {}
    for name, (node, _, _) in sorted(sink.typedef_table.items()):
        node_type = node.get("type") or {}
        target = node_type.get("desugaredQualType") or node_type.get("qualType")
        if target and name not in PRIMITIVES:
            typedefs[name] = target.strip()

    enum_underlying: dict[str, str | None] = {}
    for name, (node, _, _) in sorted(sink.enum_table.items()):
        if strip_namespace(name) in allowlist_enums:
            continue
        fixed = node.get("fixedUnderlyingType")
        if not fixed:
            enum_underlying[name] = None
            continue
        underlying = fixed.get("desugaredQualType") or fixed.get("qualType")
        enum_underlying[name] = normalize_cxx_type_name(underlying or "")
    return typedefs, enum_underlying


def ambiguous_bare_names(items: Collection[str]) -> frozenset[str]:
    """The bare names more than one of `items` carries (`A::Action` and
    `Action`: "Action"), the way `bare_keyed` computes them while it decides
    whether to add a bare alias.

    `bare_keyed` drops the *alias* of such a name but keeps an exact key if one
    declaration is spelled bare - which is right for a table a bare lookup reads
    (`<group>.Entry_t` in the IR). A *qualified* spelling that is not a
    declaration of its own (`KHook::Action`, when only `A::Action` and the
    global `Action` exist) must not silently land on the bare declaration
    either: this is the set `TypeMapper.resolve_known` refuses."""
    owners: dict[str, set[str]] = {}
    for key in items:
        normalized = normalize_cxx_type_name(key)
        owners.setdefault(strip_namespace(normalized), set()).add(normalized)
    return frozenset(bare for bare, names in owners.items() if len(names) > 1)


def same_class(printed: str | None, name: str) -> bool:
    """Whether the class name clang printed in a dump (`SourceMM::ISmmPlugin`) is
    the IR's `name` (`ISmmPlugin`)."""
    return printed is not None and bare_type_name(printed) == normalize_cxx_type_name(name)


def cxx_type_is_const(text: str) -> bool:
    """Whether the type spelled by `text` is const-qualified at its own top
    level (not the level of what it points at)."""
    stripped = text.strip()
    array = _ARRAY_SUFFIX_RE.match(stripped)
    if array:
        return cxx_type_is_const(array.group("base"))
    declarator = _DECLARATOR_SUFFIX_RE.match(stripped)
    if declarator:
        quals = declarator.group("quals") or ""
        return "const" in quals
    return stripped.startswith(("const ", "volatile "))


_ARRAY_SUFFIX_RE = re.compile(r"^(?P<base>.+?)\s*\[\s*(?P<n>\d+)\s*\]$")
_DECLARATOR_SUFFIX_RE = re.compile(
    r"^(?P<inner>.+)\s*(?P<ops>\*+|&{1,2})\s*(?P<quals>(?:(?:const|volatile)\b[^&*]*?)*)$"
)
_FUNCTION_POINTER_RE = re.compile(r"\(\s*\*")
_MAX_TYPE_DEPTH = 8

# The Rust type a function pointer becomes: pointer sized (align 8), the
# signature is not carried (the IR keeps `cxx_signature` for that).
FN_POINTER_RUST = "*const ::core::ffi::c_void"
C_POINTER_RUST = "*mut ::core::ffi::c_void"


class TypeMapper:
    """C++ type spelling -> Rust type, the mapping the IR contract fixes.

    Every extension the contract names is exact-or-None: an unmappable type
    becomes `None`, which the emitter turns into a byte blob, never a guess.
    """

    def __init__(
        self,
        *,
        known: set[str],
        records: set[str],
        enums: set[str],
        pass_in_registers: dict[str, bool],
        typedefs: dict[str, str],
        enum_underlying: dict[str, str | None],
        record_sizes: dict[str, int],
        enum_sizes: dict[str, int],
        specializations: dict[str, bool],
        spec_layouts: dict[str, RecordLayout],
        handle: str = "SchemaMetaInfoHandle_t",
        ambiguous: Collection[str] = (),
    ) -> None:
        self.known = known
        self.records = records
        self.enums = enums
        self.pass_in_registers = pass_in_registers
        self.typedefs = typedefs
        self.enum_underlying = enum_underlying
        self.record_sizes = record_sizes
        self.enum_sizes = enum_sizes
        self.specializations = specializations
        self.spec_layouts = spec_layouts
        self.handle = handle
        # The bare names more than one declaration carries (see
        # `ambiguous_bare_names`): `typedefs`/`enum_underlying` carry the exact
        # spellings and one bare alias per unambiguous bare name, and this is
        # what keeps a *qualified* spelling that is none of them from landing on
        # a same-named declaration in another scope.
        self.ambiguous = frozenset(ambiguous)

    # -- entry point -------------------------------------------------------- #

    def rust_type(
        self,
        cxx: str | None,
        *,
        signature: bool = False,
        what: str = "type",
    ) -> str | None:
        """The Rust spelling of `cxx`, or None when the contract has no exact
        answer. `signature` selects the by-value rule (rule 7): a record passed
        or returned by value needs `canPassInRegisters`."""
        rendered, _ = self._map(cxx, signature, 0)
        return rendered

    def rust_size(
        self,
        cxx: str | None,
        *,
        signature: bool = False,
    ) -> int | None:
        """The size in bytes of the Rust type `cxx` maps to, when known."""
        rendered, size = self._map(cxx, signature, 0)
        if rendered is None:
            return None
        return size

    def class_value_base(self, cxx: str | None, depth: int = 0) -> str | None:
        """The normalized name of the class/struct/union `cxx` returns by value,
        or None: an allowlisted record, a `SchemaMetaInfoHandle_t<T>`
        specialization (a class, even though it maps to `*mut T`) or a typedef
        of one. MSVC returns exactly these through a hidden pointer from an
        instance method; a pointer, a reference, a function pointer, an enum or
        a primitive is not one. `is_class_value` is this answer as a bool."""
        text = (cxx or "").strip()
        if not text or depth > _MAX_TYPE_DEPTH:
            return None
        if _FUNCTION_POINTER_RE.search(text) or _ARRAY_SUFFIX_RE.match(text):
            return None
        if _DECLARATOR_SUFFIX_RE.match(text):
            return None
        base = text
        for keyword in ("const volatile ", "volatile ", "const "):
            while base.startswith(keyword):
                base = base[len(keyword) :].strip()
        base = normalize_cxx_type_name(base)
        base = self.resolve_known(base)
        if base in PRIMITIVES or base in self.enums:
            return None
        if base in self.records:
            return base
        if self._handle_specialization(base) is not None:
            return base
        target = self.typedefs.get(base)
        if target is not None:
            return self.class_value_base(target, depth + 1)
        return None

    def is_class_value(self, cxx: str | None, depth: int = 0) -> bool:
        """Whether `cxx`, as a return type, is a class/struct/union by value
        (`class_value_base` finds none)."""
        return self.class_value_base(cxx, depth) is not None

    # -- the mapping -------------------------------------------------------- #

    def resolve_known(self, base: str) -> str:
        """`base`, or the name the tables know it under.

        Only the second upstream (Metamod:Source/KHook) needs this: inside
        another namespace clang spells a type qualified (`KHook::Action`,
        `std::size_t` in IKHook's signature), while the IR's own tables are keyed
        by the spelling the IR uses.

        A spelling that *is* a key always wins - except that a primitive's bare
        spelling wins over an exact key (see the last paragraph). Otherwise the
        leading qualifiers are stripped only for a name the tables hold under
        that bare name unambiguously: a bare name more than one declaration
        carries (`ambiguous`) does not resolve at all, so an unknown qualified
        spelling fails closed - the caller keeps `KHook::Action` and reports it
        as unmappable - instead of silently taking whichever declaration clang
        dumped first.

        `PRIMITIVES` comes first, the way `_value_type` reads it: `std::size_t`
        is the contract's `size_t`, which maps to `usize` on both ABIs, not
        whatever `unsigned long`/`unsigned long long` a libstdc++/STL typedef of
        it desugars to (the bare spelling answered that way before the second
        upstream existed)."""
        if "::" not in base:
            return base
        bare = strip_namespace(base)
        if bare == base:
            return base
        if bare in PRIMITIVES:
            return bare
        if base in self.known or base in self.typedefs or base in self.enum_underlying:
            return base
        if bare in self.ambiguous:
            return base
        if bare in self.known or bare in self.typedefs or bare in self.enum_underlying:
            return bare
        return base

    def _map(
        self,
        cxx: str | None,
        signature: bool,
        depth: int,
        behind_pointer: bool = False,
    ) -> tuple[str | None, int | None]:
        text = (cxx or "").strip()
        if not text or depth > _MAX_TYPE_DEPTH:
            return None, None

        # (5) Function pointer types/typedefs: pointer sized, signature lost.
        if _FUNCTION_POINTER_RE.search(text):
            return FN_POINTER_RUST, 8

        # (1) Fixed-size arrays, nested arrays recurse through this branch.
        array = _ARRAY_SUFFIX_RE.match(text)
        if array:
            inner, size = self._map(array.group("base"), signature, depth + 1)
            if inner is None:
                return None, None
            count = int(array.group("n"))
            return f"[{inner}; {count}]", (size * count if size is not None else None)

        # (2)/(3)/(4) Pointers and references share one ABI and one mapping.
        declarator = _DECLARATOR_SUFFIX_RE.match(text)
        if declarator:
            inner_text = declarator.group("inner").strip()
            inner, _ = self._map(inner_text, signature, depth + 1, behind_pointer=True)
            if inner is None:
                # (2) unknown innermost type: a generic pointer, and a generic
                # pointer to that for `T **`.
                return C_POINTER_RUST, 8
            # Rust's `*const T` says "T is const": that is the const-ness of
            # the pointee, not of the pointer.
            mutability = "const" if cxx_type_is_const(inner_text) else "mut"
            return f"*{mutability} {inner}", 8

        # (6)/(7)/(8) A value type: primitive, allowlisted record/enum, a
        # typedef or enum desugared through the AST, or a handle specialization.
        return self._value_type(text, signature, depth, behind_pointer)

    def _value_type(
        self, text: str, signature: bool, depth: int, behind_pointer: bool
    ) -> tuple[str | None, int | None]:
        prefix = ""
        base = text.strip()
        while True:
            for keyword in ("const volatile ", "volatile ", "const "):
                if base.startswith(keyword):
                    prefix += keyword
                    base = base[len(keyword) :].strip()
                    break
            else:
                break
        base = normalize_cxx_type_name(base)
        base = self.resolve_known(base)

        if base in PRIMITIVES:
            rust = PRIMITIVES[base]
            return rust, RUST_SIZES.get(rust)

        if base in self.known:
            if base in self.records:
                # (7) by-value record: only when the ABI can pass it in
                # registers, otherwise the caller cannot model the call. A
                # record behind a pointer is not passed by value, so the rule
                # does not reach it.
                if signature and not behind_pointer and not self.pass_in_registers.get(base):
                    return None, None
                return base, self.record_sizes.get(base)
            if base in self.enums:
                return base, self.enum_sizes.get(base)
            # A vtable class has no layout of its own: the generated type is
            # nothing but the vtable pointer, so it only names a type behind a
            # pointer (`IThing *`), never by value.
            if behind_pointer:
                return base, None
            return None, None

        # (8) `SchemaMetaInfoHandle_t<T>` is a one-pointer wrapper; the contract
        # only allows it after the layout dump confirms the shape.
        handle = self._handle_specialization(base)
        if handle is not None:
            # The wrapper is one pointer: the class it points at is never passed by value.
            inner, _ = self._map(handle, signature, depth + 1, behind_pointer=True)
            if inner is None:
                return C_POINTER_RUST, 8
            return f"*mut {inner}", 8

        # (6) Typedef/alias: clang's own desugared spelling, recursively.
        target = self.typedefs.get(base)
        if target is not None:
            if depth > _MAX_TYPE_DEPTH:
                return None, None
            return self._map(prefix + target, signature, depth + 1, behind_pointer)

        # (6) An enum outside the allowlist: its fixed underlying integer.
        if base in self.enum_underlying:
            underlying = self.enum_underlying[base]
            if underlying is None:
                return None, None
            if depth > _MAX_TYPE_DEPTH:
                return None, None
            return self._map(prefix + underlying, signature, depth + 1, behind_pointer)

        return None, None

    def _handle_specialization(self, base: str) -> str | None:
        """`T` for a `SchemaMetaInfoHandle_t<T>` the contract lets through, else
        None. The template is only trusted once the record-layout dump shows the
        single `m_pObj` pointer the mapping assumes."""
        open_at = base.find("<")
        if open_at < 0 or base[:open_at].strip() != self.handle or not base.endswith(">"):
            return None
        inner = base[open_at + 1 : -1].strip()
        if not inner or "<" in inner or "," in inner:
            return None
        if self.specializations.get(base) is not True:
            raise GenError(
                f"{base}: the AST does not report canPassInRegisters for it, so "
                f"the mapping to `*mut {inner}` is not allowed; add "
                f"S2SDK_FORCE_SPEC( <tag>, {base}, m_pObj ) to gen/probe.cpp so "
                f"clang instantiates the specialization"
            )
        layout = self.spec_layouts.get(base)
        if layout is None:
            raise GenError(
                f"{base}: clang printed no record layout, so the mapping to "
                f"`*mut {inner}` cannot be verified"
            )
        members = layout.own_members
        if layout.size != 8 or len(members) != 1 or members[0].name != "m_pObj" or members[0].offset != 0:
            raise GenError(
                f"{base}: expected one pointer member m_pObj at 0 and size 8, "
                f"clang says size {layout.size} with "
                f"{[(m.name, m.offset) for m in members]}"
            )
        return inner


def rust_type(cxx: str, known: set[str]) -> str | None:
    """The IR contract's value mapping for a bare allowlisted name, without the
    AST-derived sugar tables (the self-test and anything that only has the
    allowlist)."""
    return TypeMapper(
        known=known,
        records=known,
        enums=set(),
        pass_in_registers=dict.fromkeys(known, True),
        typedefs={},
        enum_underlying={},
        record_sizes={},
        enum_sizes={},
        specializations={},
        spec_layouts={},
    ).rust_type(cxx)


# --------------------------------------------------------------------------- #
# building the IR
# --------------------------------------------------------------------------- #


class Builder:
    def __init__(
        self,
        sdk: Path,
        metamod: Path,
        allowlist: dict,
        layouts,
        vtables,
        sink: AstSink,
        include_dirs,
    ):
        self.sdk = sdk
        # The second upstream. Every table lookup goes through the bare name
        # (see `bare_keyed`), so this is only needed to place a header back in the
        # checkout clang read it from and to rewrite the dump's paths.
        self.metamod = metamod
        self.allowlist = allowlist
        self.layouts = layouts
        # clang spells a nested record's name with its elaborated keywords
        # (`class CSchemaType *`) and, for the second upstream, a class in a
        # namespace with that namespace (`class SourceMM::ISmmPlugin`); the AST
        # and the IR spell both without.
        self.layouts_by_name = bare_keyed(layouts, "record layouts")
        self.vtables = bare_keyed(vtables, "vtable dump")
        self.sink = sink
        self.include_dirs = include_dirs
        self._header_sets: dict[str, set[Path]] = {}
        self.field_sizes: dict[str, int] = {}  # f"{record}.{field}" -> size
        self.type_sizes: dict[str, int] = {}  # probe tag -> size
        self.enum_sizes: dict[str, int] = {}  # enum name -> sizeof
        self.enum_values: dict[str, int] = {}  # probe tag -> value
        # probe tag -> measured value for the `[consts]` expressions (the tag is
        # `s2sdk_const_<name>`, the one keying the probe TU, the MSVC pass and the
        # string probes all use).
        self.const_values: dict[str, int] = {}
        self.known: set[str] = set()
        self.records: set[str] = set()
        self.enums: set[str] = set()
        self.pass_in_registers: dict[str, bool] = {}
        self.record_sizes: dict[str, int] = {}
        self.typedefs: dict[str, str] = {}
        self.enum_underlying: dict[str, str | None] = {}
        self.types: TypeMapper | None = None
        self.header_checks: list[str] = []

    # -- the type mapper ---------------------------------------------------- #

    def build_type_mapper(self) -> TypeMapper:
        """Assemble the AST-derived sugar tables the IR contract's rules 6-8
        need, then hand them to `TypeMapper`."""
        self.records = set()
        for group in GROUPS:
            self.records |= set(self.group_names(group, "records"))
            self.records |= set(self.group_names(group, "opaque"))
        self.enums = set()
        for group in GROUPS:
            self.enums |= set(self.group_names(group, "enums"))

        for name in sorted(self.records):
            node, _, _ = self.record_node(name)
            if "definitionData" not in node:
                raise GenError(
                    f"{name}: the AST prints no definitionData, so the by-value "
                    f"rule cannot be checked"
                )
            # clang prints `canPassInRegisters` only when it is true; absent
            # means the class is passed in memory (its copy ctor or dtor is
            # non-trivial), which is exactly what rule 7 rejects.
            definition = node.get("definitionData") or {}
            self.pass_in_registers[name] = bool(definition.get("canPassInRegisters"))
            self.record_sizes[name] = self.layout_of(name).size

        self.typedefs, self.enum_underlying = type_sugar_tables(self.sink, self.enums)

        spec_layouts = {}
        for full in self.sink.specializations:
            layout = self.layouts_by_name.get(normalize_cxx_type_name(full))
            if layout is not None:
                spec_layouts[full] = layout
        self.types = TypeMapper(
            known=self.known,
            records=set(self.records),
            enums=set(self.enums),
            pass_in_registers=dict(self.pass_in_registers),
            typedefs=dict(self.typedefs),
            enum_underlying=dict(self.enum_underlying),
            record_sizes=dict(self.record_sizes),
            enum_sizes=dict(self.enum_sizes),
            specializations={
                name: bool((node.get("definitionData") or {}).get("canPassInRegisters"))
                for name, (node, _, _) in self.sink.specializations.items()
            },
            spec_layouts=spec_layouts,
            ambiguous=self.sink.ambiguous,
        )
        return self.types

    # -- allowlist ---------------------------------------------------------- #

    def group_names(self, group: str, key: str) -> list[str]:
        table = self.allowlist.get(group)
        if not isinstance(table, dict):
            raise GenError(f"allowlist.toml has no [{group}] table")
        if key not in table:
            if group == CONST_GROUP and key in GROUP_TYPE_KEYS:
                return []  # the consts group has no types, only measured values
            raise GenError(f"allowlist.toml [{group}] has no '{key}'")
        value = table[key]
        if not isinstance(value, list):
            raise GenError(f"allowlist.toml [{group}].{key} must be a list")
        return value

    def spec_names(self, group: str) -> list[str]:
        """The group's class-template specializations: `specs` is optional, only
        the groups that have one carry it."""
        return group_specs(self.allowlist, group)

    # -- lookup helpers ----------------------------------------------------- #

    def allowlist_header_paths(self, group: str) -> set[Path]:
        """The group's headers, resolved through the SDK include dirs (the
        allowlist spells them the way probe.cpp includes them, the IR stores the
        SDK-relative path clang reports)."""
        if group not in self._header_sets:
            paths = set()
            for header in self.group_names(group, "headers"):
                for directory in self.include_dirs:
                    candidate = Path(directory) / header
                    if candidate.is_file():
                        paths.add(candidate.resolve())
                        break
                else:
                    raise GenError(
                        f"allowlist.toml [{group}].headers: {header} is not "
                        f"under any of the SDK include dirs"
                    )
            self._header_sets[group] = paths
        return self._header_sets[group]

    def roots(self) -> tuple[Path, ...]:
        """The checkouts the probe's headers come from, in the order gen.py puts
        their include dirs: the second upstream first, then the SDK (see
        `metamod_first_includes`). A header path clang printed is relative to the
        one that owns it."""
        return (self.metamod, self.sdk)

    def resolve_header(self, header: str) -> Path:
        """The file a probe.cpp include spelling names, looked up under the two
        upstreams in include order."""
        for root in self.roots():
            candidate = root / header
            if candidate.is_file():
                return candidate.resolve()
        raise GenError(
            f"{header}: no such header under any of the upstreams "
            f"({', '.join(str(root) for root in self.roots())})"
        )

    def relative_header(self, file: str | None, name: str) -> str:
        if not file:
            raise GenError(f"{name}: clang printed no source file for it")
        path = Path(file)
        if not path.is_absolute():
            path = (REPO_ROOT / path).resolve()
        else:
            path = path.resolve()
        for root in self.roots():
            try:
                return path.relative_to(root.resolve()).as_posix()
            except ValueError:
                continue
        raise GenError(
            f"{name}: clang says it is declared in {path}, outside every upstream "
            f"({', '.join(str(root) for root in self.roots())})"
        )

    def verify_source(self, name: str, header: str, line: int | None, group: str) -> None:
        """The header/line clang reported has to be one the group lists and the
        line has to actually declare the name: this is what makes the replayed
        file de-duplication trustworthy."""
        declared = self.resolve_header(header)
        listed = self.allowlist_header_paths(group)
        if declared not in listed:
            raise GenError(
                f"{name}: declared in {header}, which is not in "
                f"allowlist.toml [{group}].headers "
                f"({', '.join(self.group_names(group, 'headers'))})"
            )
        if line is None:
            raise GenError(f"{name}: no line number for {header}")
        path = declared
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        if not (1 <= line <= len(lines)):
            raise GenError(f"{name}: line {line} is outside {header}")
        if name not in lines[line - 1]:
            raise GenError(
                f"{name}: {header}:{line} does not mention it "
                f"({lines[line - 1].strip()!r}); the clang location replay is off"
            )
        self.header_checks.append(f"{group}:{name} in {header}:{line}")

    def class_node(self, name: str) -> dict | None:
        """The definition of a record or of a class-template specialization (a
        template base such as `CTier3AppSystem<IToolSystem2>`), else None."""
        normalized = normalize_cxx_type_name(name)
        if normalized in self.sink.records:
            return self.sink.records[normalized][0]
        found = find_specialization(self.sink, normalized)
        return found[1] if found else None

    def record_node(self, name: str) -> tuple[dict, str, int]:
        if name not in self.sink.records:
            raise GenError(
                f"{name}: no complete definition in the AST; is it in probe.cpp "
                f"(included by a header the allowlist lists)?"
            )
        return self.sink.records[name]

    def enum_node(self, name: str) -> tuple[dict, str | None, int | None]:
        """The declaration of the enum an allowlist/IR name spells: its bare
        name while exactly one declaration carries it, else the qualified
        spelling itself (`SourceMM::EPluginType`). Two declarations sharing the
        bare name leave no bare alias (see `AstSink.rekey`), which fails here
        naming both spellings instead of answering with the wrong enum."""
        found = self.sink.enum_table.get(name)
        if found is None:
            if strip_namespace(name) in self.sink.ambiguous:
                raise GenError(
                    f"{name}: more than one enum declaration carries that bare "
                    f"name; spell the allowlist entry's qualified name. The AST "
                    f"has "
                    f"{sorted(key for key in self.sink.enums if strip_namespace(key) == strip_namespace(name))}"
                )
            raise GenError(f"{name}: no enum declaration in the AST")
        return found

    def layout_of(self, name: str) -> RecordLayout:
        layout = self.layouts_by_name.get(normalize_cxx_type_name(name))
        if layout is None:
            raise GenError(
                f"{name}: clang printed no record layout; add "
                f"S2SDK_FORCE_LAYOUT( {name} ) to gen/probe.cpp"
            )
        return layout

    # -- records ------------------------------------------------------------ #

    def build_record(self, name: str, group: str, opaque: bool) -> dict:
        node, file, line = self.record_node(name)
        header = self.relative_header(file, name)
        self.verify_source(name, header, line, group)
        layout = self.layout_of(name)

        kind = node.get("tagUsed") or layout.kind
        bases = self.build_bases(name, node, layout)
        if opaque:
            fields = []
        else:
            fields = self.build_fields(name, node, layout)

        return {
            "name": name,
            "header": header,
            "kind": kind,
            "size": layout.size,
            "align": layout.align,
            "opaque": opaque,
            "pass_in_registers": bool(
                (node.get("definitionData") or {}).get("canPassInRegisters")
            ),
            "bases": bases,
            "has_vptr": layout.has_vptr,
            "fields": fields,
        }

    def build_bases(self, name: str, node: dict, layout: RecordLayout) -> list[dict]:
        ast_bases = node.get("bases", []) or []
        # clang spells a template argument with its elaborated keyword in the layout dump
        # (`CTier3AppSystem<class IToolSystem2>`) and without it in the AST; compare the
        # normalized spellings, keep the layout's name (what the record-layout blocks are keyed by)
        ast_names = [normalize_cxx_type_name(b.get("type", {}).get("qualType", "")) for b in ast_bases]
        layout_names = [normalize_cxx_type_name(b.name) for b in layout.bases]
        if ast_names != layout_names:
            raise GenError(
                f"{name}: bases disagree: AST {ast_names} vs layout {layout_names}"
            )
        return [
            {"name": normalize_cxx_type_name(b.name), "offset": b.offset, "virtual": b.virtual}
            for b in layout.bases
        ]

    def build_fields(self, name: str, node: dict, layout: RecordLayout) -> list[dict]:
        ast_fields = [c for c in node.get("inner", []) if c.get("kind") == "FieldDecl"]
        members = layout.own_members
        if len(ast_fields) != len(members):
            raise GenError(
                f"{name}: {len(ast_fields)} field declarations but "
                f"{len(members)} layout members "
                f"({[f.get('name') for f in ast_fields]} vs {[m.name for m in members]})"
            )
        fields = []
        for ast_field, member in zip(ast_fields, members):
            field_name = ast_field.get("name")
            if field_name != member.name:
                raise GenError(
                    f"{name}: field order/name mismatch: AST {field_name!r} vs "
                    f"layout {member.name!r}"
                )
            cxx_type = ast_field.get("type", {}).get("qualType")
            if not cxx_type:
                raise GenError(f"{name}.{field_name}: clang printed no type")
            if field_name is None:
                raise GenError(
                    f"{name}: has an anonymous member at offset {member.offset}; "
                    f"the flat IR cannot express it - list {name} under 'opaque' "
                    f"in allowlist.toml"
                )
            if member.is_bitfield:
                key = f"{name}.{field_name}"
                size = self.type_sizes.get(f"bitfield:{key}")
                if size is None:
                    raise GenError(f"{key}: no size probed for bitfield type {cxx_type}")
            else:
                key = f"{name}.{field_name}"
                size = self.field_sizes.get(key)
                if size is None:
                    raise GenError(f"{key}: field size was not probed")
            fields.append(
                {
                    "name": field_name,
                    "offset": member.offset,
                    "size": size,
                    "bit_offset": member.bit_offset,
                    "bit_width": member.bit_width,
                    "cxx_type": cxx_type,
                    "rust_type": self.map_field_type(f"{name}.{field_name}", cxx_type, size),
                }
            )
        return fields

    def map_field_type(self, what: str, cxx_type: str, size: int) -> str | None:
        """`rust_type` for a field, with the mapped type's size checked against
        clang's answer: a mapping that is the wrong width would silently move
        every field after it."""
        assert self.types is not None
        mapped = self.types.rust_type(cxx_type, what=what)
        if mapped is None:
            return None
        mapped_size = self.types.rust_size(cxx_type)
        if mapped_size is not None and mapped_size != size:
            raise GenError(
                f"{what}: {cxx_type} maps to {mapped} ({mapped_size} bytes) but "
                f"clang says the field is {size} bytes"
            )
        return mapped

    # -- class-template specializations -------------------------------------- #

    def spec_layout(self, name: str) -> RecordLayout:
        layout = self.layouts_by_name.get(normalize_cxx_type_name(name))
        if layout is None:
            raise GenError(
                f"{name}: clang printed no record layout for the specialization; is "
                f"there a S2SDK_FORCE_SPEC(_FIELD) row for it in gen/probe.cpp?"
            )
        return layout

    def build_spec(self, name: str, group: str) -> dict:
        """The IR of one `specs` entry: record-shaped, so the emitter can emit it
        as a struct (under `spec_tag(name)`) and `[checks]` can name its members.

        The IR has one flat field list per record, and a specialization's own
        members are usually all inherited (`CUtlVector<CEntityHandle>` holds
        nothing itself), so the fields are the *leaf* members clang laid out:
        `m_Size` at 0, the nested allocator's `m_pMemory`/`m_nAllocationCount`/
        `m_nGrowSize`. `members` keeps clang's full list, nested levels included,
        which is where a check for a subobject offset points."""
        found = find_specialization(self.sink, name)
        if found is None:
            raise GenError(
                f"{name}: no complete definition in the AST; add "
                f"S2SDK_FORCE_SPEC( {spec_tag(name)}, {name}, <a member> ) to gen/probe.cpp"
            )
        cxx_name, node = found
        if node.get("definitionData") is None:
            raise GenError(
                f"{name}: the AST prints no definitionData for {cxx_name}, so the "
                f"by-value rule cannot be checked"
            )
        layout = self.spec_layout(name)
        if layout.has_vptr:
            raise GenError(
                f"{name}: the specialization is polymorphic; a vtable pointer has no "
                f"field name to emit, so it is out of scope for `specs`"
            )
        fields = []
        used: dict[str, tuple[str, ...]] = {}
        for path, member in flatten_leaf_members(layout):
            key = f"{name}.{member.name}"
            size = self.field_sizes.get(key)
            if size is None:
                raise GenError(f"{key}: field size was not probed")
            clash = used.get(member.name)
            if clash is not None:
                raise GenError(
                    f"{name}: two leaves are called {member.name} "
                    f"({' and '.join('.'.join(p) for p in (clash, path))}); the flat "
                    f"IR cannot spell both, so this specialization needs a "
                    f"hand-written binding instead"
                )
            used[member.name] = path
            field = {
                "name": member.name,
                "offset": member.offset,
                "size": size,
                "bit_offset": member.bit_offset,
                "bit_width": member.bit_width,
                "cxx_type": member.type,
                "rust_type": self.map_field_type(key, member.type, size),
            }
            if len(path) > 1:
                field["path"] = list(path)
            fields.append(field)
        return {
            "name": name,
            "cxx_name": cxx_name,
            "rust_name": spec_tag(name),
            "template": name[: name.index("<")],
            "kind": layout.kind or "struct",
            "size": layout.size,
            "align": layout.align,
            "opaque": False,
            "pass_in_registers": bool(
                (node.get("definitionData") or {}).get("canPassInRegisters")
            ),
            "bases": [],
            "has_vptr": False,
            "fields": fields,
            "members": [
                {
                    "name": member.name,
                    "level": member.level,
                    "offset": member.offset,
                    "bit_offset": member.bit_offset,
                    "bit_width": member.bit_width,
                    "type": member.type,
                }
                for member in layout.members
            ],
        }

    # -- enums -------------------------------------------------------------- #

    def build_enum(self, name: str, group: str) -> dict:
        node, file, line = self.enum_node(name)
        header = self.relative_header(file, name)
        self.verify_source(name, header, line, group)

        constants = [c for c in node.get("inner", []) if c.get("kind") == "EnumConstantDecl"]
        if not constants:
            raise GenError(f"{name}: no enumerators")
        values = []
        for constant in constants:
            constant_name = constant.get("name")
            key = f"{name}.{constant_name}"
            if key not in self.enum_values:
                raise GenError(f"{key}: value was not probed")
            values.append({"name": constant_name, "value": self.enum_values[key]})

        fixed = node.get("fixedUnderlyingType")
        if fixed:
            underlying = fixed.get("desugaredQualType") or fixed.get("qualType")
        else:
            underlying = choose_underlying_type([v["value"] for v in values])
        rust_repr = PRIMITIVES.get(underlying)
        if rust_repr is None:
            raise GenError(f"{name}: underlying type {underlying!r} has no Rust repr")

        size = self.enum_sizes.get(name)
        if size is None:
            raise GenError(f"{name}: enum size was not probed")
        if fixed:
            expected = RUST_SIZES[rust_repr]
        else:
            expected = PRIMITIVE_SIZES[underlying]
        if size != expected:
            raise GenError(
                f"{name}: underlying {underlying!r} is {expected} bytes but clang "
                f"says sizeof({name}) == {size}"
            )

        return {
            "name": name,
            "header": header,
            "underlying": underlying,
            "rust_repr": rust_repr,
            # Whether the header fixes the enum's underlying type: the MSVC pass
            # needs it (an unfixed enum is `int` there, the smallest fitting type
            # under Itanium), and it is a fact about the declaration, not the ABI.
            "fixed_underlying": fixed is not None,
            "values": values,
        }

    # -- vtables ------------------------------------------------------------ #

    def own_virtuals(self, node: dict) -> list[dict]:
        return [
            c
            for c in node.get("inner", [])
            if c.get("kind") in ("CXXMethodDecl", "CXXDestructorDecl")
            and c.get("virtual")
            and not c.get("isImplicit")
        ]

    def base_virtuals(self, node: dict) -> list[tuple[str, dict]]:
        """(base name, virtual decl) for every direct base's own virtuals."""
        found = []
        for base in node.get("bases", []) or []:
            base_name = base.get("type", {}).get("qualType", "")
            for keyword in ("struct ", "class ", "union "):
                if base_name.startswith(keyword):
                    base_name = base_name[len(keyword) :]
            base_name = base_name.strip()
            base_node = self.class_node(base_name)
            if base_node is not None:
                found += [(base_name, v) for v in self.own_virtuals(base_node)]
                found += [(name, v) for name, v in self.base_virtuals(base_node)]
        return found

    @staticmethod
    def method_params(decl: dict) -> list[dict]:
        return [c for c in decl.get("inner", []) if c.get("kind") == "ParmVarDecl"]

    @classmethod
    def same_signature(cls, left: dict, right: dict) -> bool:
        left_type = left.get("type", {}).get("qualType", "")
        right_type = right.get("type", {}).get("qualType", "")
        return strip_method_name(left_type) == strip_method_name(right_type)

    def secondary_virtuals(self, name: str) -> list[tuple[str, dict]]:
        """(base name, virtual decl) for every virtual of every non-primary base
        on `name`'s primary-base chain, transitively (`E : C`, `C : A, B` yields
        B's virtuals for E). A virtual of the same name and signature declared in
        the class means it overrides a secondary base's virtual."""
        found: list[tuple[str, dict]] = []
        current = name
        for _ in range(64):
            layout = self.layout_of(current)
            primary = next((b for b in layout.bases if b.offset == 0 and not b.virtual), None)
            for base in layout.bases:
                if base is primary:
                    continue
                base_node = self.class_node(base.name)
                if base_node is None:
                    raise GenError(
                        f"{name}: the secondary base {base.name} (of {current}) has no "
                        f"complete declaration; its virtuals cannot be checked"
                    )
                found += [(base.name, v) for v in self.own_virtuals(base_node)]
                found += self.base_virtuals(base_node)
            if primary is None or self.class_node(primary.name) is None:
                break
            current = normalize_cxx_type_name(primary.name)
        return found

    def refuse_secondary_override(self, name: str, decl: dict, secondary: list[tuple[str, dict]]) -> None:
        """An override of a virtual of a secondary base lands in the primary table
        on Itanium (an own entry of its own) and in the secondary vfptr on MSVC; the
        flat IR cannot express both, so the class is refused."""
        is_dtor = decl.get("kind") == "CXXDestructorDecl"
        for base_name, other in secondary:
            if is_dtor:
                same = other.get("kind") == "CXXDestructorDecl"
            else:
                same = other.get("name") == decl.get("name") and self.same_signature(decl, other)
            if same:
                raise GenError(
                    f"{name}::{decl.get('name', '')}: overrides a virtual of the secondary "
                    f"base {base_name} (Itanium adds an entry to the primary table, MSVC "
                    f"uses the second vfptr); only the primary-base chain is modelled"
                )

    def build_vtable(self, name: str, group: str) -> dict:
        node, file, line = self.record_node(name)
        header = self.relative_header(file, name)
        self.verify_source(name, header, line, group)
        layout = self.layout_of(name)

        decls = self.own_virtuals(node)
        table = self.vtables.get(name, {}).get("indices")
        if table is None:
            if decls:
                raise GenError(
                    f"{name}: clang printed no vtable; add "
                    f"S2SDK_FORCE_VTABLE( {name}, <a virtual it declares> ) to "
                    f"gen/probe.cpp"
                )
            entries: list[VtableEntry] = []
        else:
            entries = list(table)
        own_entries = [e for e in entries if same_class(e.owner, name)]

        secondary = self.secondary_virtuals(name)
        methods = []
        cursor = 0
        # A class that declares no destructor of its own still gets clang's implicit one,
        # and clang lists it among the class's own entries when a base on the primary chain
        # has a virtual destructor (Itanium: the complete/deleting pair, MSVC: the scalar
        # deleting one). It is that base's destructor under the derived name, not a new
        # virtual, so it is no method of this class.
        declares_dtor = any(
            d.get("kind") == "CXXDestructorDecl" or str(d.get("name", "")).startswith("~")
            for d in decls
        )

        def skip_implicit_dtor() -> None:
            nonlocal cursor
            if declares_dtor:
                return
            while cursor < len(own_entries) and own_entries[cursor].is_dtor:
                # Itanium also lists the implicit destructor as an own entry when only a
                # secondary base has a virtual destructor (it overrides that one); it is no
                # method of this class either
                if self.dtor_override(name, node, layout) is None and not any(
                    other.get("kind") == "CXXDestructorDecl" for _, other in secondary
                ):
                    raise GenError(
                        f"{name}: clang lists the destructor as an own vtable entry, but "
                        f"no base has a virtual destructor"
                    )
                cursor += 1

        for decl_index, decl in enumerate(decls):
            self.refuse_secondary_override(name, decl, secondary)
            method_name = decl.get("name", "")
            is_dtor = decl.get("kind") == "CXXDestructorDecl" or method_name.startswith("~")
            skip_implicit_dtor()
            entry = own_entries[cursor] if cursor < len(own_entries) else None
            if entry is not None and entry.name == method_name and entry.is_dtor == is_dtor:
                slot = entry.index
                cursor += 1
                overrides = self.dtor_override(name, node, layout) if is_dtor else None
                if is_dtor:
                    second = own_entries[cursor] if cursor < len(own_entries) else None
                    if second is None or second.name != method_name:
                        raise GenError(
                            f"{name}.{method_name}: expected the deleting destructor "
                            f"slot right after the complete one"
                        )
                    cursor += 1
            else:
                overrides = self.find_override(node, decl)
                if overrides is None:
                    raise GenError(
                        f"{name}.{method_name}: no vtable slot found "
                        f"(own entries: {[e.qualified for e in own_entries]})"
                    )
                slot = next(
                    (e.index for e in entries if e.qualified == overrides),
                    None,
                )
                if slot is None:
                    raise GenError(f"{name}.{method_name}: no slot for {overrides}")
            methods.append(self.build_method(name, decl, decl_index, is_dtor, overrides, slot))
        skip_implicit_dtor()
        if cursor != len(own_entries):
            raise GenError(
                f"{name}: {len(own_entries) - cursor} vtable slots left over "
                f"({[e.qualified for e in own_entries[cursor:]]})"
            )

        bases = layout.bases
        primary = next((b for b in bases if b.offset == 0 and not b.virtual), None)
        primary_name = self.vtable_primary_base(name, primary.name) if primary else None
        return {
            "class": name,
            "header": header,
            "primary_base": primary_name,
            # {name, offset}: where the secondary base subobject sits in the
            # object (Itanium; the MSVC pass adds `msvc_offset`).
            "secondary_bases": [
                {"name": b.name, "offset": b.offset} for b in bases if b is not primary
            ],
            "methods": methods,
        }

    def vtable_primary_base(self, name: str, primary: str) -> str:
        """The primary base the IR records for a vtable class: the nearest class on the
        primary chain that is itself an allowlisted vtable. The classes in between (a
        class without virtuals of its own, a class template such as
        `CTier3AppSystem<IToolSystem2>` that only overrides) leave the table's layout
        unchanged, so the generated vtable struct embeds the nearest listed ancestor's;
        a class between that declares a virtual of its own is refused."""
        listed = {
            normalize_cxx_type_name(n)
            for group in GROUPS
            for n in (self.allowlist.get(group, {}).get("vtables") or [])
        }
        current = primary
        for _ in range(64):
            if normalize_cxx_type_name(current) in listed:
                return current
            node = self.class_node(current)
            layout = self.layouts_by_name.get(normalize_cxx_type_name(current))
            if node is None or layout is None:
                return primary
            for decl in self.own_virtuals(node):
                if self.find_override(node, decl) is None:
                    raise GenError(
                        f"{name}: {current} on the primary chain declares the virtual "
                        f"{decl.get('name')} and is not in the allowlist's vtables, so the "
                        f"generated vtable struct would miss its slot; list {current}"
                    )
            parent = next((b for b in layout.bases if b.offset == 0 and not b.virtual), None)
            if parent is None:
                return primary
            current = parent.name
        return primary

    def dtor_override(self, name: str, node: dict, layout: RecordLayout) -> str | None:
        """`Base::~Base` when the class's own virtual destructor overrides one on
        the primary-base chain; clang's dump names the derived destructor as the
        slot's owner, so the owner alone cannot tell the two apart. The nearest
        base that declares a virtual destructor is the override target no matter
        how many no-dtor classes sit between it and this one."""
        current = node
        current_layout = layout
        for _ in range(64):
            primary = next(
                (b for b in current_layout.bases if b.offset == 0 and not b.virtual),
                None,
            )
            if primary is None:
                return None
            base_name = normalize_cxx_type_name(primary.name)
            base_node = self.class_node(base_name)
            if base_node is None:
                raise GenError(
                    f"{name}: primary base {base_name} has no complete definition "
                    f"in the AST, so its destructor cannot be checked"
                )
            for decl in self.own_virtuals(base_node):
                if decl.get("kind") == "CXXDestructorDecl" or str(decl.get("name", "")).startswith("~"):
                    return f"{base_name}::~{base_name}"
            current = base_node
            current_layout = self.layout_of(base_name)
        raise GenError(f"{name}: the primary-base chain does not terminate")

    def find_override(self, node: dict, decl: dict) -> str | None:
        """`Base::Name` when the declaration is the final overrider of a base
        virtual (no slot of its own)."""
        for base_name, base_decl in self.base_virtuals(node):
            if base_decl.get("name") == decl.get("name") and self.same_signature(base_decl, decl):
                return f"{base_name}::{decl.get('name')}"
        return None

    def build_method(self, class_name: str, decl: dict, decl_index: int, is_dtor: bool, overrides, slot) -> dict:
        method = {
            "name": decl.get("name", ""),
            "decl_index": decl_index,
            "is_dtor": is_dtor,
            "overrides": overrides,
            "itanium_slot": slot,
            "cxx_signature": decl.get("type", {}).get("qualType", ""),
            "rust_fn": build_rust_fn(class_name, decl, self.types),
        }
        attach_msvc_rust_fn(method, class_name, decl, self.types)
        return method


PRIMITIVE_SIZES = {
    "bool": 1,
    "_Bool": 1,
    "char": 1,
    "signed char": 1,
    "unsigned char": 1,
    "short": 2,
    "unsigned short": 2,
    "short int": 2,
    "unsigned short int": 2,
    "int": 4,
    "unsigned int": 4,
    "unsigned": 4,
    "signed": 4,
    "long": 8,
    "unsigned long": 8,
    "long long": 8,
    "unsigned long long": 8,
    "long long int": 8,
    "unsigned long long int": 8,
}

# `long` is the one primitive width the two ABIs disagree on: 8 bytes under
# Itanium, 4 under MSVC. Everything else in PRIMITIVE_SIZES is the same.
MSVC_PRIMITIVE_SIZES = {**PRIMITIVE_SIZES, "long": 4, "unsigned long": 4}


RUST_SIZES = {
    "bool": 1,
    "::core::ffi::c_char": 1,
    "i8": 1,
    "u8": 1,
    "i16": 2,
    "u16": 2,
    "i32": 4,
    "u32": 4,
    "f32": 4,
    "i64": 8,
    "u64": 8,
    "f64": 8,
    "isize": 8,
    "usize": 8,
}

_UNDERLYING_CANDIDATES = (
    ("int", 4),
    ("unsigned int", 4),
    ("long", 8),
    ("unsigned long", 8),
    ("long long", 8),
    ("unsigned long long", 8),
)


def choose_underlying_type(values: list[int]) -> str:
    """The standard's rule for an enum without a fixed underlying type: the
    first of int / unsigned int / long / unsigned long / long long /
    unsigned long long that can represent every enumerator."""
    for candidate, _ in _UNDERLYING_CANDIDATES:
        unsigned = candidate.startswith("unsigned")
        bits = 32 if "int" in candidate else 64
        low = 0 if unsigned else -(1 << (bits - 1))
        high = (1 << bits) - 1 if unsigned else (1 << (bits - 1)) - 1
        if all(low <= value <= high for value in values):
            return candidate
    raise GenError(f"no underlying type can represent {values}")


def strip_method_name(qual_type: str) -> str:
    """`void (int) const` -> the part that decides whether one virtual overrides
    another (return type and name excluded, parameter types and qualifiers
    kept)."""
    text = qual_type.strip()
    end = text.find("(")
    if end < 0:
        return text
    return text[end:]


# --------------------------------------------------------------------------- #
# probe TU for sizes / enumerator values
# --------------------------------------------------------------------------- #


@dataclasses.dataclass
class Probe:
    tag: str
    kind: str
    expr: str
    result: tuple


def gen_probe_tu(
    probes: list[Probe], msvc: bool = False, strings: list[tuple[str, str]] = ()
) -> str:
    """The probe TU: one template instantiation per number clang will not print
    as text or JSON (field sizes, bitfield type sizes, enum sizes, enumerator
    values, `[consts]` expressions), plus one `constexpr auto` per string const
    - a template argument cannot carry a string, so those are read back from the
    AST instead (`parse_probe_strings`)."""
    lines = [
        "// generated by s2sdk-sys/gen/gen.py - do not edit, not committed",
        "// Instantiates one template per number clang will not print as text or",
        "// JSON: field sizes, bitfield type sizes, enum sizes, enumerator",
        "// values and the [consts] expressions. The values land in the JSON AST",
        "// as integral template arguments, so the compiler does the arithmetic.",
    ]
    if msvc:
        # The MSVC CRT/STL makes `#define private` a hard error (xkeycheck.h);
        # the MSVC pass reaches private members with -fno-access-control instead.
        lines.append("// compiled with -fno-access-control")
    else:
        lines += ["#define private public", "#define protected public"]
    lines.append(f'#include "{PROBE_CPP}"')
    if not msvc:
        lines += ["#undef private", "#undef protected"]
    lines += [
        "",
        "template <class Tag, unsigned long V> struct s2sdk_field_size_probe {};",
        "template <class Tag, unsigned long V> struct s2sdk_type_size_probe {};",
        "template <class Tag, unsigned long V> struct s2sdk_enum_size_probe {};",
        "template <class Tag, long long V> struct s2sdk_enum_value_probe {};",
        "template <class Tag, long long V> struct s2sdk_const_probe {};",
        "",
    ]
    names = {
        "field": "s2sdk_field_size_probe",
        "type": "s2sdk_type_size_probe",
        "enumsize": "s2sdk_enum_size_probe",
        "enumvalue": "s2sdk_enum_value_probe",
        "const": "s2sdk_const_probe",
    }
    for probe in probes:
        lines.append(
            f"template struct {names[probe.kind]}<struct {probe.tag}, {probe.expr}>;"
        )
    if strings:
        lines += ["", "namespace s2sdk_const_strings {"]
        for tag, expr in strings:
            lines.append(f"constexpr auto {tag} = ({expr});")
        lines += ["}", ""]
    lines.append("")
    return "\n".join(lines)


PROBE_TEMPLATE_NAMES = {
    "s2sdk_field_size_probe",
    "s2sdk_type_size_probe",
    "s2sdk_enum_size_probe",
    "s2sdk_enum_value_probe",
    "s2sdk_const_probe",
}

# The namespace `gen_probe_tu` puts the string consts in; `parse_probe_strings`
# reads the `StringLiteral` of every VarDecl below it.
PROBE_STRING_NAMESPACE = "s2sdk_const_strings"


def const_strings(consts: list[ConstEntry]) -> list[tuple[str, str]]:
    """(tag, expression) for every string const: the `constexpr auto` rows the
    probe TU evaluates, in allowlist order."""
    return [(const.tag, const.expr or "") for const in consts if const.kind == "string"]


def const_probes(consts: list[ConstEntry]) -> list[Probe]:
    """One probe per `[consts]` entry that has an expression. The cast makes the
    template argument a `long long` whatever the macro's own type is; a `derived`
    entry has no expression to evaluate (its value comes from the layout dump)."""
    probes = []
    for const in consts:
        if const.kind == "string" or const.expr is None:
            continue
        probes.append(
            Probe(
                tag=const.tag,
                kind="const",
                expr=f"(long long)({const.expr})",
                result=("constvalue", const.tag),
            )
        )
    return probes


def measure_consts(
    consts: list[ConstEntry],
    probe_values: dict[str, int],
    string_values: dict[str, str],
    layouts_by_name: dict[str, RecordLayout],
) -> dict[str, int | str]:
    """{name: value} for every `[consts]` entry, from this ABI's measurements.

    A missing measurement is a generator error, never a default: the point of the
    kind is that the compiler owns every value."""
    values: dict[str, int | str] = {}
    for const in consts:
        if const.kind == "string":
            if const.tag not in string_values:
                raise GenError(
                    f"[consts] {const.name}: clang reported no value for the "
                    f"string probe {const.tag}"
                )
            values[const.name] = string_values[const.tag]
            continue
        if const.derived is None:
            if const.tag not in probe_values:
                raise GenError(
                    f"[consts] {const.name}: clang reported no value for the probe "
                    f"{const.tag}"
                )
            value = probe_values[const.tag]
        else:
            record, member, what = const.derived
            layout = layouts_by_name.get(normalize_cxx_type_name(record))
            if layout is None:
                raise GenError(
                    f"[consts] {const.name}: clang printed no layout for {record}, so "
                    f"{record}.{member}.{what} cannot be derived"
                )
            value = derive_member_value(layout, member, what)
        value = apply_transform(const.name, value, const.transform)
        if not rust_int_fits(const.rust_type, value):
            raise GenError(
                f"[consts] {const.name}: the measured value {value} does not fit "
                f"{const.rust_type}; fix the entry's `type`"
            )
        values[const.name] = value
    return values


def derive_member_value(layout: RecordLayout, member: str, what: str) -> int:
    """The `what` of `layout`'s member `member`, from clang's layout dump. Every
    member clang printed is searchable (nested ones too), so a bitfield inside an
    anonymous union - which the flat IR cannot express - is still readable."""
    found = [m for m in layout.members if m.name == member]
    if not found:
        raise GenError(
            f"{layout.name}: clang printed no member {member!r} "
            f"({[m.name for m in layout.members if m.name]})"
        )
    if len(found) > 1:
        raise GenError(f"{layout.name}: {len(found)} members are called {member!r}")
    value = getattr(found[0], what)
    if value is None:
        raise GenError(
            f"{layout.name}.{member}: clang printed no {what} for it (it is not a "
            f"bitfield, or not a field at all)"
        )
    return int(value)


def apply_transform(name: str, value: int, transform: str | None) -> int:
    if transform is None:
        return value
    if transform == "mask":
        return (1 << value) - 1
    raise GenError(f"[consts] {name}: unknown transform {transform!r}")


def _find_first(node: dict, kind: str) -> dict | None:
    if node.get("kind") == kind:
        return node
    for child in node.get("inner") or []:
        if isinstance(child, dict):
            found = _find_first(child, kind)
            if found is not None:
                return found
    return None


def parse_probe_strings(objects: list[dict]) -> dict[str, str]:
    """{tag: value} for the `constexpr auto` string probes of the generated TU,
    from the `StringLiteral` clang evaluated them to."""
    values: dict[str, str] = {}
    for obj in objects:
        if obj.get("kind") != "NamespaceDecl" or obj.get("name") != PROBE_STRING_NAMESPACE:
            continue
        for child in obj.get("inner") or []:
            if not isinstance(child, dict) or child.get("kind") != "VarDecl":
                continue
            tag = child.get("name")
            literal = _find_first(child, "StringLiteral")
            if tag is None or literal is None:
                continue
            text = literal.get("value")
            if not isinstance(text, str):
                raise GenError(f"string probe {tag}: clang printed no literal value")
            try:
                # clang spells the literal as JSON does, quotes included.
                values[tag] = json.loads(text)
            except ValueError as exc:
                raise GenError(f"string probe {tag}: cannot read {text!r}: {exc}") from exc
    return values


def flatten_leaf_members(layout: RecordLayout) -> list[tuple[tuple[str, ...], LayoutMember]]:
    """[(access path, member)] for the data members a caller reads off the
    record, with nested subobjects flattened to their leaves.

    clang prints a nested member right after its own members at a deeper indent,
    so a member whose next line is deeper is a subobject: its leaves are taken
    instead, and the path to each of them is the chain of names to go through
    (`m_Memory.m_pMemory`). An anonymous subobject contributes no path component
    (its members are injected into the enclosing class, so `->m_Index` works),
    which is what makes an inheriting layout come out as one flat field list -
    the shape the IR's `fields` has."""
    leaves: list[tuple[tuple[str, ...], LayoutMember]] = []
    stack: list[tuple[int, str]] = []  # enclosing subobjects: (level, name)
    for index, member in enumerate(layout.members):
        while stack and member.level <= stack[-1][0]:
            stack.pop()
        if member.is_vptr or not member.name:
            continue  # a vtable pointer, or an anonymous subobject's own line
        following = layout.members[index + 1 :]
        if following and following[0].level > member.level:
            stack.append((member.level, member.name))  # a subobject: take its leaves
            continue
        path = tuple(name for _, name in stack) + (member.name,)
        leaves.append((path, member))
    return leaves


def member_path_expr(name: str, path: tuple[str, ...]) -> str:
    """The C++ expression that names a flattened member:
    `((CUtlVector<CEntityHandle>*)0)->m_Memory.m_pMemory`."""
    return f"(({name}*)0)->" + ".".join(path)


def measure_derived_paths(consts: list[ConstEntry], layouts_by_name: dict) -> dict[str, list[str]]:
    """{const name: the C++ member path to the bitfield its value came from} for
    every `derived` entry.

    A `derived` value is a bitfield width, which only clang's layout dump has, so
    the verify TU cannot re-derive it from the entry's expression - it needs the
    path to spell the field (`["m_Parts", "m_EntityIndex"]`). The IR keeps that
    path next to the value; this is where it comes from.

    The path is read out of the layout clang printed for the default (Itanium)
    ABI. It is a member *path*, so it is the same spelling under both ABIs (only
    the offsets differ), and both verify TUs use it - a path that only spelled
    one ABI's layout would fail the other TU's compile."""
    paths: dict[str, list[str]] = {}
    for const in consts:
        if const.derived is None:
            continue
        record, member, _what = const.derived
        layout = layouts_by_name.get(normalize_cxx_type_name(record))
        if layout is None:
            continue  # measure_consts reports the missing layout with a real message
        matches = [path for path, leaf in flatten_leaf_members(layout) if leaf.name == member]
        if len(matches) != 1:
            raise GenError(
                f"[consts] {const.name}: cannot spell {record}.{member} in C++: "
                f"{len(matches)} member path(s) match"
            )
        paths[const.name] = list(matches[0])
    return paths


def collect_probes(builder: Builder, consts: list[ConstEntry]) -> list[Probe]:
    probes: list[Probe] = []
    for group in GROUPS:
        for name in builder.group_names(group, "records"):
            if name in builder.group_names(group, "opaque"):
                continue
            node, _, _ = builder.record_node(name)
            layout = builder.layout_of(name)
            ast_fields = [c for c in node.get("inner", []) if c.get("kind") == "FieldDecl"]
            for ast_field, member in zip(ast_fields, layout.own_members):
                field_name = ast_field.get("name")
                if field_name is None:
                    continue  # build_fields reports this with a real message
                key = f"{name}.{field_name}"
                tag = f"s2sdk_fs_{len(probes)}"
                if member.is_bitfield:
                    probes.append(
                        Probe(
                            tag=tag,
                            kind="type",
                            expr=f"sizeof({member.type})",
                            result=("bitsize", key),
                        )
                    )
                else:
                    probes.append(
                        Probe(
                            tag=tag,
                            kind="field",
                            expr=f"sizeof((({name}*)0)->{field_name})",
                            result=("fieldsize", key),
                        )
                    )
        for name in builder.spec_names(group):
            # A specialization's fields are its inherited ones: every leaf the
            # layout dump shows, laid out flat.
            layout = builder.spec_layout(name)
            for path, member in flatten_leaf_members(layout):
                probes.append(
                    Probe(
                        tag=f"s2sdk_fs_{len(probes)}",
                        kind="field",
                        expr=f"sizeof({member_path_expr(name, path)})",
                        result=("fieldsize", f"{name}.{member.name}"),
                    )
                )
        for name in builder.group_names(group, "enums"):
            node, _, _ = builder.enum_node(name)
            tag = f"s2sdk_es_{len(probes)}"
            probes.append(
                Probe(tag=tag, kind="enumsize", expr=f"sizeof({name})", result=("enumsize", name))
            )
            scoped = "scopedEnumTag" in node
            for constant in node.get("inner", []):
                if constant.get("kind") != "EnumConstantDecl":
                    continue
                constant_name = constant.get("name")
                expr_name = f"{name}::{constant_name}" if scoped else constant_name
                tag = f"s2sdk_ev_{len(probes)}"
                probes.append(
                    Probe(
                        tag=tag,
                        kind="enumvalue",
                        expr=f"(long long){expr_name}",
                        result=("enumvalue", f"{name}.{constant_name}"),
                    )
                )
    probes += const_probes(consts)
    return probes


def apply_probe_values(builder: Builder, probes: list[Probe], values: dict[str, int]) -> None:
    for probe in probes:
        if probe.tag not in values:
            what = probe.result[1]
            raise GenError(f"{what}: clang reported no value for probe {probe.tag}")
        value = values[probe.tag]
        kind, key = probe.result
        if kind == "fieldsize":
            builder.field_sizes[key] = value
        elif kind == "bitsize":
            builder.type_sizes[f"bitfield:{key}"] = value
        elif kind == "enumsize":
            builder.enum_sizes[key] = value
        elif kind == "enumvalue":
            builder.enum_values[key] = value
        elif kind == "constvalue":
            builder.const_values[key] = value


# --------------------------------------------------------------------------- #
# probe.cpp <-> allowlist.toml
# --------------------------------------------------------------------------- #

_FORCE_LAYOUT_RE = re.compile(r"^\s*S2SDK_FORCE_LAYOUT\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)\s*;", re.M)
_FORCE_VTABLE_RE = re.compile(
    r"^\s*S2SDK_FORCE_VTABLE\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*,\s*(~?[A-Za-z_][A-Za-z0-9_]*)\s*\)\s*;",
    re.M,
)
_FORCE_VTABLE_DTOR_RE = re.compile(
    r"^\s*S2SDK_FORCE_VTABLE_DTOR\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)\s*;", re.M
)
_FORCE_SPEC_RE = re.compile(
    r"^\s*S2SDK_FORCE_SPEC(?:_FIELD)?\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*,\s*([^,]+?)\s*,"
    r"\s*([A-Za-z_][A-Za-z0-9_]*(?:\(\))?)\s*\)\s*;",
    re.M,
)
_PROBE_INCLUDE_RE = re.compile(r'^\s*#\s*include\s+"([^"]+)"', re.M)


def check_probe_against_allowlist(allowlist: dict, sink: AstSink) -> None:
    text = PROBE_CPP.read_text(encoding="utf-8")
    forced_layouts = set(_FORCE_LAYOUT_RE.findall(text))
    forced_vtables = _FORCE_VTABLE_RE.findall(text)
    forced_vtables += [
        (name, f"~{name}") for name in _FORCE_VTABLE_DTOR_RE.findall(text)
    ]
    forced_specs = _FORCE_SPEC_RE.findall(text)
    includes = _PROBE_INCLUDE_RE.findall(text)

    wanted_headers = []
    for group in GROUPS:
        wanted_headers += allowlist[group]["headers"]
    if includes != wanted_headers:
        raise GenError(
            "probe.cpp includes do not match allowlist.toml headers (in order):\n"
            f"  probe.cpp:      {includes}\n"
            f"  allowlist.toml: {wanted_headers}"
        )

    want_layouts = set()
    for group in GROUPS:
        for key in ("records", "opaque", "vtables"):
            want_layouts |= set(allowlist[group].get(key) or [])
    if forced_layouts != want_layouts:
        raise GenError(
            "probe.cpp S2SDK_FORCE_LAYOUT does not match allowlist.toml:\n"
            f"  missing: {sorted(want_layouts - forced_layouts)}\n"
            f"  extra:   {sorted(forced_layouts - want_layouts)}"
        )

    # Class-template specializations: one row per `specs` entry, under the tag
    # the IR emits it as, so a misspelled specialization cannot silently drop out
    # of the IR. probe.cpp may force more (the `SchemaMetaInfoHandle_t<T>` ones
    # the type mapper needs) - only the `specs` entries are checked against it.
    want_specs = {}
    for group in GROUPS:
        for name in group_specs(allowlist, group):
            want_specs[spec_tag(name)] = name
    got_specs = {}
    for tag, written, _member in forced_specs:
        if tag in got_specs and normalize_cxx_type_name(got_specs[tag]) != normalize_cxx_type_name(
            written
        ):
            raise GenError(f"probe.cpp: duplicate S2SDK_FORCE_SPEC( {tag}, ... )")
        got_specs[tag] = written
    missing = sorted(set(want_specs) - set(got_specs))
    if missing:
        raise GenError(
            f"probe.cpp has no S2SDK_FORCE_SPEC row for {missing}; the tag is the "
            f"specialization with every non-identifier byte replaced by `_` "
            f"({spec_tag('CUtlVector<CEntityHandle>')})"
        )
    for tag, want in want_specs.items():
        if normalize_cxx_type_name(got_specs[tag]) != normalize_cxx_type_name(want):
            raise GenError(
                f"probe.cpp: S2SDK_FORCE_SPEC( {tag}, {got_specs[tag]} ) spells the type "
                f"differently than allowlist.toml's {want}"
            )
        if find_specialization(sink, want) is None:
            raise GenError(
                f"probe.cpp: {want} is not instantiated in the AST; the "
                f"S2SDK_FORCE_SPEC row has to force its definition"
            )

    seen = set()
    for name, method in forced_vtables:
        if (name, method) in seen:
            raise GenError(f"probe.cpp: duplicate S2SDK_FORCE_VTABLE( {name}, {method} )")
        seen.add((name, method))
        if name not in sink.records:
            raise GenError(f"probe.cpp: S2SDK_FORCE_VTABLE( {name}, ... ) is not a class")
        node = sink.records[name][0]
        own = [
            c
            for c in node.get("inner", [])
            if c.get("kind") in ("CXXMethodDecl", "CXXDestructorDecl")
            and c.get("virtual")
            and not c.get("isImplicit")
        ]
        matching = [c for c in own if c.get("name") == method]
        if not matching:
            raise GenError(
                f"probe.cpp: {name} declares no virtual {method} "
                f"(it declares {[c.get('name') for c in own]}); take the address "
                f"of a method the class declares itself, so clang emits its vtable"
            )
        if len(matching) > 1:
            raise GenError(
                f"probe.cpp: {name}::{method} is overloaded, so `&{name}::{method}` "
                f"is ambiguous; pick a method with a unique name"
            )

    for group in GROUPS:
        for name in allowlist[group].get("vtables") or []:
            if name not in sink.records:
                raise GenError(f"{name}: no complete definition in the AST")
            node = sink.records[name][0]
            own = [
                c
                for c in node.get("inner", [])
                if c.get("kind") in ("CXXMethodDecl", "CXXDestructorDecl")
                and c.get("virtual")
                and not c.get("isImplicit")
            ]
            has_row = any(row_name == name for row_name, _ in forced_vtables)
            if own and not has_row:
                raise GenError(
                    f"{name} declares {len(own)} virtuals but probe.cpp has no "
                    f"S2SDK_FORCE_VTABLE( {name}, ... ) row, so clang will not "
                    f"print its vtable"
                )
            if not own and has_row:
                raise GenError(
                    f"{name} declares no virtual of its own; its vtable is its "
                    f"base's. Drop the S2SDK_FORCE_VTABLE row."
                )


# --------------------------------------------------------------------------- #
# rust_fn for vtables
# --------------------------------------------------------------------------- #

RUST_KEYWORDS = {
    "as", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
    "move", "mut", "pub", "ref", "return", "self", "static", "struct", "super",
    "trait", "true", "type", "unsafe", "use", "where", "while", "async", "await",
}


def split_top_level(text: str, sep: str = ",") -> list[str]:
    parts = []
    depth = 0
    current = ""
    for char in text:
        if char in "(<[":
            depth += 1
        elif char in ")>]":
            depth -= 1
        if char == sep and depth == 0:
            parts.append(current.strip())
            current = ""
        else:
            current += char
    if current.strip():
        parts.append(current.strip())
    return parts


def rust_param_name(name: str | None, index: int, used: set[str]) -> str:
    if not name:
        candidate = f"arg{index}"
    else:
        candidate = re.sub(r"[^0-9A-Za-z_]", "_", name)
        if not candidate or candidate[0].isdigit():
            candidate = f"_{candidate}"
    if candidate in RUST_KEYWORDS:
        candidate += "_"
    while candidate in used:
        candidate += "_"
    used.add(candidate)
    return candidate


def build_rust_fn(
    class_name: str, decl: dict, types: TypeMapper, class_return: bool = False
) -> str | None:
    """The `unsafe extern "C" fn` spelling of a virtual, or None when a by-value
    parameter/return type has no exact mapping (rule 7).

    `class_return` lifts rule 7 for the *return* only: a record that is not
    trivially copyable maps to its own name there. That spelling is no ABI's
    native form; it is the input of `msvc_hidden_return_fn`, which rewrites the
    return into MSVC's hidden pointer (sound for a non-trivial record too)."""
    qual_type = decl.get("type", {}).get("qualType")
    if not qual_type:
        return None
    open_at = qual_type.find("(")
    if open_at < 0:
        return None
    close_at = qual_type.rfind(")")
    if close_at < open_at:
        return None
    keyword = "unsafe extern \"C\" fn"
    returns = qual_type[:open_at].strip()
    params_text = qual_type[open_at + 1 : close_at].strip()
    if params_text == "void":
        params_text = ""

    what = f"{class_name}::{decl.get('name')}"
    rendered = [f"this: *mut {class_name}"]
    used = {"this"}
    decl_params = [c for c in decl.get("inner", []) if c.get("kind") == "ParmVarDecl"]
    for index, param in enumerate(split_top_level(params_text)):
        if not param or param == "...":
            return None
        mapped = types.rust_type(param, signature=True, what=what)
        if mapped is None:
            return None
        name = decl_params[index].get("name") if index < len(decl_params) else None
        rendered.append(f"{rust_param_name(name, index, used)}: {mapped}")

    ret = "()"
    if returns != "void":
        mapped = types.rust_type(returns, signature=not class_return, what=what)
        if mapped is None:
            return None
        ret = mapped
    signature = f"{keyword}({', '.join(rendered)})"
    if ret != "()":
        signature += f" -> {ret}"
    return signature


def attach_msvc_rust_fn(method: dict, class_name: str, decl: dict, types: TypeMapper) -> None:
    """Set `method["msvc_rust_fn"]` when the Windows form differs from `rust_fn`
    (a class returned by value, see `msvc_hidden_return_fn`); absent otherwise."""
    qual_type = decl.get("type", {}).get("qualType") or ""
    open_at = qual_type.find("(")
    returns = qual_type[:open_at].strip() if open_at >= 0 else ""
    if method.get("is_dtor") or not returns or returns == "void":
        return
    returns_class = types.is_class_value(returns)
    rust_fn = method.get("rust_fn")
    if rust_fn is None and returns_class:
        # rule 7 refused the native form, e.g. a record with a user copy ctor
        # (`CUtlSymbolLarge`): Itanium returns that through an sret placed before
        # `this`, which no Rust spelling matches. MSVC returns every class through
        # the hidden pointer after `this`, trivial or not (the `clang: MSVC ... 8+
        # copy-ctor` selftest), so its form exists whenever the parameters map.
        rust_fn = build_rust_fn(class_name, decl, types, class_return=True)
    hidden = msvc_hidden_return_fn(rust_fn, class_name, returns_class)
    if hidden:
        method["msvc_rust_fn"] = hidden


def msvc_hidden_return_fn(rust_fn: str | None, class_name: str, returns_class: bool) -> str | None:
    """The MSVC x64 spelling of `rust_fn` when it returns a class by value, else None.

    An MSVC instance method returns *every* class/struct/union through a hidden
    pointer that is the second argument, right after `this` (clang:
    `sret(%T)` as the second parameter, even for a 4- or 8-byte record); the
    callee hands the pointer back in RAX. Rust's `extern "C"` on windows would
    return a record of 1, 2, 4 or 8 bytes in RAX and put the hidden pointer of a
    larger one *before* `this`, so neither can spell it: the hidden pointer is
    written out as a parameter and the result is that pointer. Itanium keeps the
    native by-value form (`rust_fn`) - a trivially copyable record travels in
    registers or in an sret that Rust places before `this`, as C++ does."""
    if not rust_fn or not returns_class:
        return None
    open_at = rust_fn.find("fn(")
    if open_at < 0:
        return None
    close_at = find_matching_paren(rust_fn, open_at + 2)
    if close_at is None:
        return None
    tail = rust_fn[close_at + 1 :].strip()
    if not tail.startswith("->"):
        return None
    ret_type = tail[2:].strip()
    params = split_top_level(rust_fn[open_at + 3 : close_at])
    used = {param.partition(":")[0].strip() for param in params}
    ret_name = rust_param_name("ret", 0, used)
    params.insert(1, f"{ret_name}: *mut {ret_type}")
    return f"{rust_fn[: open_at + 3]}{', '.join(params)}) -> *mut {ret_type}"


# --------------------------------------------------------------------------- #
# the return transport of a class returned by value (WP1.D)
# --------------------------------------------------------------------------- #

# The Rust spellings of the C++ `float`/`double` (the PRIMITIVES map): the only
# leaf types the x86-64 System V ABI classifies SSE.
SSE_RUST_TYPES = frozenset({"f32", "f64"})


def method_return_class(method: dict, types: TypeMapper) -> str | None:
    """The name of the class/struct/union the method returns by value, or None.

    The return type is the text of `cxx_signature` before the first
    parenthesis - the same parse `attach_msvc_rust_fn` does - resolved through
    `TypeMapper.class_value_base`, so the answer is exactly the one the
    `rust_fn`/`msvc_rust_fn` spellings were built on. A destructor returns
    nothing (its `cxx_signature` is `~T ()`'s, a pointer return is not a class
    by value), and a function-pointer return mis-parses to a non-class, which
    `class_value_base` refuses."""
    if method.get("is_dtor"):
        return None
    signature = method.get("cxx_signature") or ""
    open_at = signature.find("(")
    if open_at < 0:
        return None
    return types.class_value_base(signature[:open_at])


def return_leaves(field: dict, offset: int, records: dict[str, dict], what: str, depth: int = 0) -> list[tuple[int, int, str]]:
    """The `(offset, size, class)` leaves of one IR field at byte `offset`,
    classed `"sse"` or `"int"` for `itanium_return_regs`.

    A bitfield is INTEGER; a `float`/`double` leaf (`rust_type` f32/f64) is
    SSE; an array's element type and a field whose type is a record the IR
    knows are walked into (the element count does not matter to the class,
    only to the span); a pointer or reference is INTEGER; anything else
    (`rust_type` None, a type neither primitive nor a known record) is a loud
    failure - an unclassifiable leaf must not quietly become INTEGER."""
    if depth > _MAX_TYPE_DEPTH:
        raise GenError(f"{what}: field {field.get('name')!r} nests deeper than {_MAX_TYPE_DEPTH}")
    if field.get("bit_width") is not None:
        return [(offset, field["size"], "int")]
    cxx = (field.get("cxx_type") or "").strip()
    size = field["size"]
    count = 1
    array = _ARRAY_SUFFIX_RE.match(cxx)
    if array:
        cxx = array.group("base").strip()
        count = int(array.group("n"))
        if count <= 0 or size % count:
            raise GenError(f"{what}: field {field.get('name')!r}: array of {count} in {size} byte(s)")
        element_size = size // count
    else:
        element_size = size
    base = normalize_cxx_type_name(cxx)
    if base and base in records:
        # a record-typed member: its own fields, shifted into this record and
        # repeated per array element (the element size is sizeof, padding and all)
        sub = records[base]
        if sub.get("kind") == "union":
            raise GenError(f"{what}: field {field.get('name')!r} is the union {base}")
        if sub["bases"]:
            raise GenError(f"{what}: field {field.get('name')!r} is {base}, which has bases")
        one = []
        for sub_field in sub["fields"]:
            one += return_leaves(sub_field, offset + sub_field["offset"], records, what, depth + 1)
        return [
            (off + k * element_size, span, cls)
            for k in range(count)
            for off, span, cls in one
        ]
    rust = field.get("rust_type")
    if array:
        # an array of primitives, as the emitter spells them (`[f32; 3]`): the
        # element's Rust type decides, the spans repeat per element
        element = re.match(r"^\[([^;\]]+);", rust or "")
        if element is None:
            raise GenError(
                f"{what}: field {field.get('name')!r} of type {field.get('cxx_type')!r} "
                f"has no element class: {rust!r} is not a Rust array spelling"
            )
        cls = "sse" if element.group(1).strip() in SSE_RUST_TYPES else "int"
        return [(offset + k * element_size, element_size, cls) for k in range(count)]
    if rust in SSE_RUST_TYPES:
        return [(offset, size, "sse")]
    if rust is None:
        if _DECLARATOR_SUFFIX_RE.match(cxx):
            # a pointer/reference to a type the contract does not map is still INTEGER
            return [(offset, size, "int")]
        raise GenError(
            f"{what}: field {field.get('name')!r} of type {cxx!r} has no class: it is "
            f"neither a float/double, an integer, a pointer nor a record of the IR"
        )
    return [(offset, size, "int")]


def member_span(cxx: str, what: str) -> int:
    """The byte span of a member type as the record-layout dump spells it: an
    array multiplies the element span, a pointer is 8, a primitive its width.
    Anything else has no span the dump alone can give."""
    array = _ARRAY_SUFFIX_RE.match(cxx)
    if array:
        return int(array.group("n")) * member_span(array.group("base").strip(), what)
    if _DECLARATOR_SUFFIX_RE.match(cxx):
        return 8
    mapped = PRIMITIVES.get(normalize_cxx_type_name(cxx))
    if mapped is None or mapped not in RUST_SIZES:
        raise GenError(f"{what}: {cxx!r} has no member span the record-layout dump can give")
    return RUST_SIZES[mapped]


def regs_from_leaves(leaves: list[tuple[int, int, str]], size: int, name: str, what: str) -> str:
    """`"int"` / `"sse"` / `"mixed"` for the leaf spans of a record of `size`
    bytes: the AMD64 ABI (3.2.3, classification, steps 4-5) classifies every
    eightbyte INTEGER or SSE - an eightbyte holding both is INTEGER - and the
    pair of classes is what `regs` spells. An eightbyte no leaf covers is a
    loud failure, never a silent `"int"`."""
    classes = []
    for lo in range(0, size, 8):
        seen = {cls for off, span, cls in leaves if off < lo + 8 and off + span > lo}
        if not seen:
            raise GenError(f"{what}: no field of {name} covers the eightbyte at {lo}")
        classes.append("sse" if seen == {"sse"} else "int")
    if all(cls == "int" for cls in classes):
        return "int"
    if all(cls == "sse" for cls in classes):
        return "sse"
    return "mixed"


def itanium_return_regs(record: dict, records: dict[str, dict], what: str) -> str:
    """`"int"` / `"sse"` / `"mixed"` for a trivially copyable record of at most
    16 bytes returned by value on x86_64 (Itanium/SysV) - the register classes
    `regs_from_leaves` merges, with the leaves from the flat IR's field list
    (`return_leaves`), so nested record members classify with their own fields.

    A record whose class gen.py cannot decide from the flat IR is a loud
    failure naming the call site `what` - a union (the flat IR cannot say
    which member is live), a record with bases (the base subobject's fields
    are not in this record's list), an empty record (no field classifies an
    eightbyte). Never a silent `"int"`."""
    if record.get("kind") == "union":
        raise GenError(f"{what}: {record['name']} is a union; the return-transport rule does not decide a union")
    if record["bases"]:
        raise GenError(f"{what}: {record['name']} has bases; the return-transport rule does not decide a derived record")
    fields = record["fields"]
    if not fields:
        raise GenError(f"{what}: {record['name']} has no fields; the return-transport rule does not decide an empty record")
    leaves = []
    for field in fields:
        leaves += return_leaves(field, field["offset"], records, what)
    return regs_from_leaves(leaves, record["size"], record["name"], what)


def opaque_return_regs(layout: RecordLayout, what: str) -> str:
    """The register classes of an **opaque** record (the flat IR spells no
    fields for it - an anonymous member the IR cannot name, say), from clang's
    own record-layout dump the generator already parsed: every named member
    whose type is a primitive, a bitfield or a pointer is a leaf
    (`member_span` gives the span), a vtable pointer is INTEGER, and a named
    member of record type is skipped - the dump lists its members at a deeper
    level, so its leaves are in the list too. A leaf the dump cannot span is a
    loud failure naming the call site `what`."""
    leaves = []
    for member in layout.members:
        if member.is_vptr:
            leaves.append((member.offset, 8, "int"))
            continue
        if member.name is None or member.is_bitfield:
            # an anonymous container holds no class of its own (its named
            # children are members too); a bitfield is INTEGER
            if member.is_bitfield:
                leaves.append((member.offset, member_span(member.type, what), "int"))
            continue
        cxx = member.type.strip()
        if _DECLARATOR_SUFFIX_RE.match(cxx):
            leaves.append((member.offset, 8, "int"))
            continue
        mapped = PRIMITIVES.get(normalize_cxx_type_name(cxx))
        if mapped is None:
            continue  # a record-typed member: its own leaves are in the dump too
        leaves.append((member.offset, member_span(cxx, what), "sse" if mapped in SSE_RUST_TYPES else "int"))
    return regs_from_leaves(leaves, layout.size, layout.name, what)


def attach_return_transports(ir_by_group: dict, types: TypeMapper, layouts: dict[str, RecordLayout]) -> None:
    """Add every vtable method's `ret` object: the per-ABI return transport of
    a class returned by value - the machine-readable answer the KHook thunk
    generator reads, the same facts `rust_fn`/`msvc_rust_fn` spell out as Rust.
    A method whose return is not a class by value gets no `ret` object.

    The rules, read back from clang's `-S -emit-llvm` output on both targets
    (the `clang: MSVC ...` checks in gen_selftest.py):

    * MSVC x64: an instance method returns EVERY class/struct/union through a
      hidden pointer that is the second argument, right after `this`
      (`sret: true`), even a 1-, 2-, 4- or 8-byte record - the callee hands the
      pointer back in RAX. The size is the record's MSVC size.
    * Itanium (SysV x86_64): `sret` iff the record is not trivially copyable
      (clang's `canPassInRegisters`) or bigger than 16 bytes; a register return
      travels as `regs` says (`itanium_return_regs`). The size is the record's
      Itanium size.
    * A `SchemaMetaInfoHandle_t<T>` specialization is a one-pointer wrapper
      (the shape `_handle_specialization` proves): 8 bytes on both ABIs, the
      pointer is INTEGER, and MSVC's hidden-pointer rule applies like to any
      class.

    `layouts` is clang's parsed record-layout dump (`builder.layouts_by_name`),
    which an **opaque** record's register classes come from - the flat IR
    spells no fields for one.

    Runs after `merge_msvc` (the MSVC sizes come from the records' `"msvc"`
    sub-objects), so a record whose transport cannot be decided fails the run
    naming the method (`itanium_return_regs`, `opaque_return_regs`, the spec
    checks) - never emits a guessed answer."""
    records = {
        record["name"]: record
        for group in GROUPS
        for record in ir_by_group[group]["records"]
    }
    for group in GROUPS:
        for vtable in ir_by_group[group]["vtables"]:
            for method in vtable["methods"]:
                what = f"{vtable['class']}::{method['name']}#{method['decl_index']}"
                base = method_return_class(method, types)
                if base is None:
                    continue
                if base in records:
                    record = records[base]
                    # `pass_in_registers` is clang's own answer for the record, the
                    # same flag `rust_fn`'s by-value rule reads; an AST without the
                    # definitionData half already failed the run in
                    # `build_type_mapper`, so absence here is a decided `False`
                    # (clang prints `canPassInRegisters` only when it is true).
                    pir = record["pass_in_registers"]
                    size = record["size"]
                    sret = not pir or size > 16
                    if sret:
                        regs = "memory"
                    elif record.get("opaque"):
                        # the flat IR spells no fields for an opaque record (an
                        # anonymous member the IR cannot name): clang's record-
                        # layout dump the generator parsed is the other half of
                        # the same measurement, so the classes come from there
                        layout = layouts.get(base)
                        if layout is None:
                            raise GenError(f"{what}: {base} is opaque and clang printed no record layout for it")
                        regs = opaque_return_regs(layout, what)
                    else:
                        regs = itanium_return_regs(record, records, what)
                    itanium = {"sret": sret, "size": size, "regs": regs}
                    msvc_size = record["msvc"]["size"]
                else:
                    # the one-pointer handle wrapper: `_handle_specialization` has
                    # proven the shape (size 8, `m_pObj` at 0, canPassInRegisters)
                    if types._handle_specialization(base) is None:  # noqa: SLF001
                        raise GenError(f"{what}: {base} is neither a record of the IR nor a proven handle specialization")
                    itanium = {"sret": False, "size": 8, "regs": "int"}
                    msvc_size = 8
                method["ret"] = {
                    "itanium": itanium,
                    "msvc": {"sret": True, "size": msvc_size},
                }


# --------------------------------------------------------------------------- #
# acceptance checks from allowlist.toml
# --------------------------------------------------------------------------- #


def check_method_key(what: str, key: str, methods: list[dict]) -> dict:
    """Resolve a `[checks]` method name: `"Name"` is `"Name#0"`, and `#k` counts
    the declarations of that name in declaration order - the same addressing
    `abi::slot` uses, so an overload is checkable by index."""
    name, sep, index_text = key.partition("#")
    index = 0
    if sep:
        if not index_text.isdigit():
            raise GenError(f"[checks] {what}: {key!r} has no overload index")
        index = int(index_text)
    same = [method for method in methods if method["name"] == name]
    if index >= len(same):
        raise GenError(
            f"[checks] {what}: {key} is none of the {len(same)} declaration(s) of "
            f"{name!r}"
        )
    return same[index]


def check_value(what: str, got, expected, field: str) -> None:
    if got != expected:
        raise GenError(f"[checks] {what}: {field} is {got}, expected {expected}")


def find_vtable(ir: dict, class_name: str) -> dict | None:
    return next((v for v in ir["vtables"] if v["class"] == class_name), None)


def find_vtable_any(ir_by_group: dict, class_name: str) -> dict | None:
    """The vtable IR for `class_name`, wherever its group is (a `[checks]` length
    key only names the class, and a primary base can live in another group)."""
    for group in GROUPS:
        for vtable in (ir_by_group.get(group) or {}).get("vtables") or []:
            if vtable["class"] == class_name:
                return vtable
    return None


def itanium_vtable_len(ir_by_group: dict, class_name: str) -> int:
    """The Itanium length of `class_name`'s vtable (entries from the address
    point), as `abi::vtable_len` computes it: every primary base's length, plus
    this class's own new declarations (an override reuses its base's slot), with
    an Itanium destructor taking `ITANIUM_DTOR_SLOTS` entries.

    The IR carries no Itanium length of its own (the emitted vtable struct's size
    is what tests/layout.rs asserts), so this is what a `length` key pins:
    the number a consumer relies on, against a hand-entered expectation, so an
    upstream bump that adds, removes or reorders a virtual fails the run instead
    of being regenerated into both the IR and the test. The two readings of the
    same IR - the declaration count and one past clang's highest slot - have to
    agree, or the slot list is not the dense one the ABI rule produces and the
    run fails."""
    chain = []
    seen = set()
    current: str | None = class_name
    while current is not None:
        if current in seen:
            raise GenError(f"[checks] {class_name}: the primary-base chain has a cycle at {current}")
        seen.add(current)
        vtable = find_vtable_any(ir_by_group, current)
        if vtable is None:
            raise GenError(
                f"[checks] {class_name}: {current} is not a vtable in the IR, so its "
                f"Itanium length cannot be derived"
            )
        chain.append(vtable)
        current = vtable["primary_base"]

    def slots_of(method: dict) -> int:
        return ITANIUM_DTOR_SLOTS if method["is_dtor"] else 1

    declared = 0
    for vtable in reversed(chain):  # base first, the way `vtable_len` walks
        for method in vtable["methods"]:
            if method["overrides"] is None:
                declared += slots_of(method)
    by_slot = 0
    for vtable in chain:
        for method in vtable["methods"]:
            slot = method["itanium_slot"]
            if slot is None:
                raise GenError(
                    f"[checks] {class_name}: {vtable['class']}::{method['name']} has no "
                    f"Itanium slot in the IR"
                )
            by_slot = max(by_slot, slot + slots_of(method))
    if declared == 0:
        raise GenError(f"[checks] {class_name}: the IR has no Itanium slot to take a length from")
    if declared != by_slot:
        raise GenError(
            f"[checks] {class_name}: its declarations add up to {declared} Itanium vtable "
            f"entries but the highest slot ends at {by_slot}; the IR's slots are not dense"
        )
    return declared


def check_vtable_length(key: str, ir_by_group: dict, vtable: dict, expected: dict) -> None:
    """A two-part `<group>.<vtable>` key: `length` (Itanium, derived from the
    slots the IR carries) and `msvc_length` (the length clang's Windows vftable
    pass printed, which is what the bridge's vtable-length pins assume)."""
    if not any(field in expected for field in VTABLE_LENGTH_FIELDS):
        raise GenError(
            f"[checks] {key}: {vtable['class']} is a vtable, name a method (or "
            f"'{VTABLE_LENGTH_FIELDS[0]}'/'{VTABLE_LENGTH_FIELDS[1]}')"
        )
    for field in VTABLE_LENGTH_FIELDS:
        value = expected.get(field)
        if field in expected and (isinstance(value, bool) or not isinstance(value, int)):
            # `true` would otherwise compare equal to a length of 1.
            raise GenError(f"[checks] {key}: {field} must be an integer, not {value!r}")
    if "length" in expected:
        check_value(key, itanium_vtable_len(ir_by_group, vtable["class"]), expected["length"], "length")
    if "msvc_length" in expected:
        measured = (vtable.get("msvc") or {}).get("length")
        if measured is None:
            raise GenError(f"[checks] {key}: the IR carries no MSVC length for {vtable['class']}")
        check_value(key, measured, expected["msvc_length"], "msvc_length")


def check_no_vtable_length(key: str, expected: dict, what: str) -> None:
    """A vtable length is only meaningful on a vtable; refusing it elsewhere keeps
    a key like `"entity.CEntityIdentity" = { length = 112 }` from being accepted
    and checking nothing."""
    given = [field for field in VTABLE_LENGTH_FIELDS if field in expected]
    if given:
        raise GenError(f"[checks] {key}: {what} has no vtable, so {given} means nothing")


def check_vtable_method(key: str, spec: str, vtable: dict, expected: dict, ir_by_group: dict) -> None:
    """One `[checks]` method name against a vtable: `itanium_slot`, `msvc_slot`,
    `decl_index` and `last_slot` (the method is the vtable's last entry). `@<Base>`
    instead of a method name checks a secondary base's `offset` / `msvc_offset`
    (where the base subobject sits in the object)."""
    if spec.startswith("@"):
        base = next((b for b in vtable["secondary_bases"] if b["name"] == spec[1:]), None)
        if base is None:
            raise GenError(f"[checks] {key}: {vtable['class']} has no secondary base {spec[1:]}")
        for field in ("offset", "msvc_offset"):
            if field in expected:
                check_value(key, base[field], expected[field], field)
        return
    method = check_method_key(key, spec, vtable["methods"])
    for field in ("itanium_slot", "msvc_slot", "decl_index"):
        if field in expected:
            check_value(key, method[field], expected[field], field)
    if LAST_SLOT_FIELD in expected:
        if expected[LAST_SLOT_FIELD] is not True:
            raise GenError(
                f"[checks] {key}: {LAST_SLOT_FIELD} is a statement about the method, "
                f"so it is `true` or not given (got {expected[LAST_SLOT_FIELD]!r})"
            )
        _check_last_slot(key, method, vtable, ir_by_group)


_IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def _check_last_slot(key: str, method: dict, vtable: dict, ir_by_group: dict) -> None:
    """`last_slot`: the method's first slot is the last entry of its vtable, on
    both ABIs.

    A destructor takes two Itanium entries and one MSVC one, so "last" means the
    slot the ABI's last entry starts at: Itanium `length - 2` for a destructor
    and `length - 1` for anything else, MSVC always `msvc_length - 1`. The
    Itanium length is `itanium_vtable_len`'s (derived from the IR's slots, the
    same number `abi::vtable_len` computes) and the MSVC one is the vftable length
    clang printed, so the check fails when the class grew a virtual after the
    named one - which is exactly what a consumer pinning the last slot of an
    interface needs to know."""
    if method["is_dtor"]:
        raise GenError(
            f"[checks] {key}: {LAST_SLOT_FIELD} cannot name a destructor (clang "
            f"prints two Itanium entries for it, and the slot that follows is a "
            f"virtual the class declares; name that one)"
        )
    class_name = vtable["class"]
    itanium = itanium_vtable_len(ir_by_group, class_name)
    measured = (vtable.get("msvc") or {}).get("length")
    if measured is None:
        raise GenError(
            f"[checks] {key}: the IR carries no MSVC length for {class_name}, so "
            f"{LAST_SLOT_FIELD} cannot be checked on the Windows ABI"
        )
    check_value(key, method["itanium_slot"], itanium - 1, f"{LAST_SLOT_FIELD} (Itanium)")
    check_value(key, method["msvc_slot"], measured - 1, f"{LAST_SLOT_FIELD} (MSVC)")


def check_platform_references(ir_by_group: dict) -> None:
    """A Windows-only group's items exist only under `cfg(windows)`, so an item of
    a group for both platforms may not name one (a base, a field type, a method
    signature): the Linux build of the bindings would not compile. The IR spells
    every such reference in `bases`, `primary_base`, a field's `rust_type`, or a
    method's `rust_fn`, so a name scan of those is complete."""
    windows_only: dict[str, str] = {}
    for group in windows_only_groups(ir_by_group):
        ir = ir_by_group[group]
        for key, name_key in (("records", "name"), ("enums", "name"), ("vtables", "class"), ("specs", "rust_name")):
            for item in ir.get(key) or []:
                windows_only[item[name_key]] = group
    if not windows_only:
        return
    for group in GROUPS:
        ir = ir_by_group.get(group)
        if ir is None or ir_windows_only(ir):
            continue
        mentions: list[tuple[str, str]] = []
        for item in [*(ir.get("records") or []), *(ir.get("specs") or [])]:
            what = item.get("name") or item.get("rust_name")
            for base in item.get("bases") or []:
                mentions.append((what, base if isinstance(base, str) else base.get("name", "")))
            for field in item.get("fields") or []:
                mentions += [(what, t) for t in _IDENT.findall(field.get("rust_type") or "")]
        for vtable in ir.get("vtables") or []:
            what = vtable["class"]
            if vtable.get("primary_base"):
                mentions.append((what, vtable["primary_base"]))
            for method in vtable["methods"]:
                for key in ("rust_fn", "msvc_rust_fn"):
                    mentions += [(what, t) for t in _IDENT.findall(method.get(key) or "")]
        for what, name in mentions:
            if name in windows_only:
                raise GenError(
                    f"[{group}] {what} refers to {name}, which is in the Windows-only "
                    f"group [{windows_only[name]}]: a group for both platforms may not "
                    f"depend on a Windows-only one (move {what} into a Windows-only group)"
                )


# `[checks]` keys that assert an Itanium number. A Windows-only group claims none,
# so such a key is refused there instead of pinning a number nothing measures.
ITANIUM_CHECK_KEYS = (
    "size", "align", "offset", "bit_offset", "bit_width",
    "itanium_slot", "length", "value", "itanium_value",
)


def run_checks(allowlist: dict, ir_by_group: dict) -> None:
    """Every `[checks]` key, against the IR. A record key checks the Itanium
    numbers (the top level) and its `msvc_*` twin the `"msvc"` sub-object; a
    vtable key checks `itanium_slot`, `msvc_slot` and `decl_index`, or its
    `length`/`msvc_length`; a `[consts]` key checks the measured `value`,
    `itanium_value` or `msvc_value`."""
    checks = allowlist.get("checks", {})
    for key, expected in checks.items():
        parts = key.split(".")
        if len(parts) not in (2, 3):
            raise GenError(f"[checks] {key}: expected <group>.<type>[.<member>]")
        group, type_name = parts[0], parts[1]
        if group not in ir_by_group:
            raise GenError(f"[checks] {key}: no group {group}")
        if ir_windows_only(ir_by_group[group]):
            itanium_keys = sorted(set(expected) & set(ITANIUM_CHECK_KEYS))
            if itanium_keys:
                raise GenError(
                    f"[checks] {key}: [{group}] is Windows-only, so {itanium_keys} (an Itanium "
                    f"number) claims nothing about any binary; use the msvc_* twin"
                )
        if LAST_SLOT_FIELD in expected and len(parts) != 3:
            raise GenError(
                f"[checks] {key}: {LAST_SLOT_FIELD} says a method is its vtable's last "
                f"entry, so it needs a <group>.<vtable>.<method> key"
            )
        if group == CONST_GROUP:
            const = next(
                (c for c in ir_by_group[group].get("consts") or [] if c["name"] == type_name),
                None,
            )
            if const is None:
                raise GenError(f"[checks] {key}: no const {type_name} in [{group}]")
            if len(parts) != 2:
                raise GenError(f"[checks] {key}: a const has no members")
            sources = {
                "value": const["value"],
                "itanium_value": const["itanium"]["value"],
                "msvc_value": const["msvc"]["value"],
            }
            for field, got in sources.items():
                if field in expected:
                    check_value(key, got, expected[field], field)
            continue
        found = None
        for record in ir_by_group[group]["records"]:
            if record["name"] == type_name:
                found = record
        if found is not None:
            if len(parts) == 2:
                # a class that is a record *and* a vtable class (a base with virtuals) may carry
                # its size and its vtable length in one row
                own_vtable = find_vtable(ir_by_group[group], type_name)
                if own_vtable is None:
                    check_no_vtable_length(key, expected, f"the record {type_name}")
                elif any(field in expected for field in VTABLE_LENGTH_FIELDS):
                    check_vtable_length(key, ir_by_group, own_vtable, expected)
                for field in ("size", "align"):
                    if field in expected:
                        check_value(key, found[field], expected[field], field)
                for field in ("msvc_size", "msvc_align"):
                    if field in expected:
                        check_value(
                            key, found["msvc"][field.split("_", 1)[1]], expected[field], field
                        )
            else:
                member = next((f for f in found["fields"] if f["name"] == parts[2]), None)
                if member is None:
                    # CEntityInstance is a record *and* a vtable class: a name
                    # that is not one of its fields is one of its methods.
                    vtable = find_vtable(ir_by_group[group], type_name)
                    if vtable is None:
                        raise GenError(f"[checks] {key}: {type_name} has no field {parts[2]}")
                    check_vtable_method(key, parts[2], vtable, expected, ir_by_group)
                    continue
                msvc = next(
                    (f for f in found["msvc"]["fields"] if f["name"] == parts[2]), None
                )
                if LAST_SLOT_FIELD in expected:
                    raise GenError(
                        f"[checks] {key}: {LAST_SLOT_FIELD} names a virtual of a "
                        f"vtable, and {type_name}.{parts[2]} is a data member"
                    )
                for field in ("offset", "size", "bit_offset", "bit_width"):
                    if field in expected:
                        check_value(key, member[field], expected[field], field)
                    msvc_field = f"msvc_{field}"
                    if msvc_field in expected:
                        check_value(key, msvc[field], expected[msvc_field], msvc_field)
            continue
        spec = next(
            (s for s in ir_by_group[group].get("specs") or [] if s["name"] == type_name), None
        )
        if spec is not None:
            if len(parts) == 2:
                check_no_vtable_length(key, expected, f"the specialization {type_name}")
                for field in ("size", "align"):
                    if field in expected:
                        check_value(key, spec[field], expected[field], field)
                for field in ("msvc_size", "msvc_align"):
                    if field in expected:
                        check_value(
                            key, spec["msvc"][field.split("_", 1)[1]], expected[field], field
                        )
                continue
            # A specialization's members are the flattened leaves (`fields`) or,
            # for a subobject the flattening walked through, one of the entries of
            # clang's full list (`members`).
            itanium_member = next((f for f in spec["fields"] if f["name"] == parts[2]), None)
            msvc_member = next(
                (f for f in spec["msvc"]["fields"] if f["name"] == parts[2]), None
            )
            if itanium_member is None:
                itanium_member = next(
                    (f for f in spec["members"] if f["name"] == parts[2]), None
                )
            if msvc_member is None:
                msvc_member = next(
                    (f for f in spec["msvc"]["members"] if f["name"] == parts[2]), None
                )
            if itanium_member is None or msvc_member is None:
                raise GenError(f"[checks] {key}: {type_name} has no member {parts[2]}")
            if LAST_SLOT_FIELD in expected:
                raise GenError(
                    f"[checks] {key}: {LAST_SLOT_FIELD} names a virtual of a vtable, "
                    f"and {type_name}.{parts[2]} is a data member"
                )
            for field in ("offset", "size", "bit_offset", "bit_width"):
                if field in expected:
                    if itanium_member.get(field) is None:
                        raise GenError(
                            f"[checks] {key}: clang printed no {field} for {parts[2]}"
                        )
                    check_value(key, itanium_member[field], expected[field], field)
                msvc_field = f"msvc_{field}"
                if msvc_field in expected:
                    if msvc_member.get(field) is None:
                        raise GenError(
                            f"[checks] {key}: the MSVC dump has no {field} for {parts[2]}"
                        )
                    check_value(key, msvc_member[field], expected[msvc_field], msvc_field)
            continue
        vtable = find_vtable(ir_by_group[group], type_name)
        if vtable is None:
            raise GenError(f"[checks] {key}: no record, spec or vtable {type_name} in [{group}]")
        if len(parts) == 2:
            check_vtable_length(key, ir_by_group, vtable, expected)
            continue
        check_vtable_method(key, parts[2], vtable, expected, ir_by_group)
    vlog(f"acceptance checks: {len(checks)} okay")


# --------------------------------------------------------------------------- #
# the compiler re-derives the IR (verify_ir_cxx)
# --------------------------------------------------------------------------- #


def gen_verify_tu(ir_by_group: dict, abi: str = ABI_DEFAULT) -> str:
    """A TU that restates every number in the IR as a `static_assert`: sizes,
    alignments, field offsets, field sizes, enum sizes and enumerator values.
    The generator parsed those numbers out of clang's *text* dumps; compiling
    this TU makes the compiler derive them again from the headers, so a misread
    dump fails the run instead of shipping.

    `abi` picks the numbers: the top-level Itanium ones (the Itanium command line
    compiles it) or the `"msvc"` sub-objects (the Windows x64 command line, with
    `-fno-access-control` instead of the `private` macro the MSVC STL rejects)."""
    msvc = abi == ABI_MSVC
    lines = [
        "// generated by s2sdk-sys/gen/gen.py - do not edit, not committed",
        "// Restates every number in ir/*.json as a static_assert, so the compiler -",
        "// not the generator - confirms what was parsed out of clang's text dumps.",
    ]
    if msvc:
        lines.append("// compiled with -fno-access-control (the MSVC CRT/STL rejects")
        lines.append("// `#define private public`, which the Itanium TU uses)")
    else:
        lines += ["#define private public", "#define protected public"]
    lines.append(f'#include "{PROBE_CPP}"')
    if not msvc:
        lines += ["#undef private", "#undef protected"]
    lines += [
        "#include <cstddef>",
        "#include <type_traits>",
        "",
    ]
    for group in GROUPS:
        ir = ir_by_group[group]
        lines.append(f"// --- {group} ---")
        for record in ir["records"]:
            name = record["name"]
            numbers = record["msvc"] if msvc else record["itanium"]
            lines.append(f'static_assert(sizeof({name}) == {numbers["size"]}, "{name}: size");')
            lines.append(f'static_assert(alignof({name}) == {numbers["align"]}, "{name}: align");')
            for base in record["bases"]:
                base_name = base["name"]
                # A base subobject's offset is not a constant expression in C++
                # (offsetof cannot name one). The offset the generator emitted is
                # asserted by the generated Rust layout test instead, which is
                # the same clang dump -> binding check for one level less.
                lines.append(
                    f'static_assert(std::is_base_of_v<{base_name}, {name}>, '
                    f'"{name}: base {base_name}");'
                )
                lines.append(
                    f'static_assert(sizeof({name}) >= sizeof({base_name}), '
                    f'"{name}: base {base_name} fits");'
                )
            if record["opaque"]:
                continue
            for field in numbers["fields"]:
                field_name = field["name"]
                lines.append(
                    f'static_assert(sizeof((({name}*)0)->{field_name}) == {field["size"]}, '
                    f'"{name}::{field_name}: size");'
                )
                if record["kind"] == "union":
                    continue  # every member of a union sits at offset 0
                if field["bit_width"] is not None:
                    continue  # a bitfield cannot be named by offsetof
                lines.append(
                    f'static_assert(offsetof({name}, {field_name}) == {field["offset"]}, '
                    f'"{name}::{field_name}: offset");'
                )
        for enum in ir["enums"]:
            name = enum["name"]
            size = enum["msvc"]["size"] if msvc else RUST_SIZES[enum["rust_repr"]]
            lines.append(f'static_assert(sizeof({name}) == {size}, "{name}: size");')
            for value in enum["values"]:
                lines.append(
                    f'static_assert((long long)({value["name"]}) == {value["value"]}, '
                    f'"{name}::{value["name"]}");'
                )
        for spec in ir.get("specs") or []:
            name = spec["name"]
            numbers = spec["msvc"] if msvc else spec["itanium"]
            # The fields are clang's leaves; a nested one is reached through the
            # subobject chain the top-level entry records as its `path`.
            paths = {
                field["name"]: tuple(field.get("path") or [field["name"]])
                for field in spec["fields"]
            }
            lines.append(f'static_assert(sizeof({name}) == {numbers["size"]}, "{name}: size");')
            lines.append(f'static_assert(alignof({name}) == {numbers["align"]}, "{name}: align");')
            for field in numbers["fields"]:
                expression = member_path_expr(name, paths[field["name"]])
                lines.append(
                    f'static_assert(sizeof({expression}) == {field["size"]}, '
                    f'"{name}::{field["name"]}: size");'
                )

    # The `[consts]`: an expression is re-evaluated here, and a string is
    # compared character by character (a template argument cannot carry it, and
    # this way the terminator is checked too, so a longer or shorter string fails
    # rather than only a differently spelled one). A `derived` bit-width has no
    # expression at all, so it is re-measured through the member path the IR
    # recorded (`derived_path`): every bit of the field is set in a copy of its
    # enclosing subobject and the field reads back its own mask.
    consts = ir_by_group[CONST_GROUP]["consts"]
    lines.append("// --- consts ---")
    if any(c.get("derived") and c.get("transform") != "mask" for c in consts):
        lines += [
            "// `derived` bit widths: the field is set to all ones (through its own",
            "// declared type) and what it keeps is counted - that is the width clang",
            "// printed for it.",
            "constexpr unsigned long long s2sdk_verify_bit_width(unsigned long long value) {",
            "    unsigned long long bits = 0;",
            "    while (value != 0) {",
            "        value >>= 1;",
            "        ++bits;",
            "    }",
            "    return bits;",
            "}",
            "",
        ]
    for const in consts:
        name = const["name"]
        value = const["msvc"]["value"] if msvc else const["itanium"]["value"]
        if const["kind"] == "string":
            variable = f"s2sdk_verify_{name}"
            expected = [f"{variable}[{index}] == {c_char_literal(char)}" for index, char in enumerate(value)]
            expected.append(f"{variable}[{len(value)}] == 0")
            lines += [
                f'constexpr auto {variable} = ({const["expr"]});',
                f'static_assert({" && ".join(expected)}, "{name}: value");',
            ]
            continue
        if const.get("expr"):
            lines.append(
                f'static_assert(((long long)({const["expr"]})) == {value}, "{name}: value");'
            )
            continue
        lines += verify_derived_const(const, value)
    lines.append("")
    return "\n".join(lines)


def verify_derived_const(const: dict, value: int) -> list[str]:
    """The static_assert lines for one `derived` const: a bitfield width (or the
    mask it implies) re-measured by the compiler.

    The value is the number the generator read out of clang's *layout dump*;
    neither clang's AST nor a template argument carries a bitfield's width, so
    the field is reached through the IR's `derived_path` (the C++ spelling,
    `m_Parts.m_EntityIndex`) and set to all ones. Counting what survives is the
    width; for a `mask` entry the all-ones value is the expected number itself."""
    name = const["name"]
    record, member, what = str(const["derived"]).split(".")
    if what != "bit_width":
        raise GenError(
            f"[consts] {name}: the verify TU can only re-measure a `bit_width` "
            f"derivation, not `{what}`"
        )
    path = const.get("derived_path")
    if not path:
        raise GenError(
            f"[consts] {name}: the IR has no derived_path for {record}.{member}, so "
            f"the static-assert TU cannot spell the bitfield (run a full gen.py)"
        )
    parent = ".".join(path[:-1])
    # A path with no named parent is a member of the record itself (an anonymous
    # subobject contributes no path component); it has to be constexpr-
    # constructible for this to compile, which fails the run loudly otherwise.
    holder = f"decltype({record}::{parent})" if parent else record
    field = path[-1]
    variable = f"s2sdk_verify_{name}"
    if const.get("transform") == "mask":
        measured = f"static_cast<unsigned long long>(value.{field})"
    else:
        measured = f"s2sdk_verify_bit_width(value.{field})"
    return [
        f"// {name} == {value}: clang's {const['derived']}, re-measured through",
        f"// {holder} (the compiler folds the field's own mask).",
        f"constexpr unsigned long long {variable} = [] {{",
        f"    using {variable}_parent = {holder};",
        f"    {variable}_parent value{{}};",
        # The declared type's cast, so a bitfield wider than `unsigned` is not cut
        # short by the literal (the assignment itself keeps only the field's bits).
        f"    value.{field} = static_cast<decltype(value.{field})>(~0ull);",
        f"    return {measured};",
        "}();",
        f'static_assert({variable} == {value}, "{name}: value");',
    ]


def c_char_literal(char: str) -> str:
    """A C++ character literal for one character of a measured string."""
    if char == "'":
        return "'\\''"
    if char == "\\":
        return "'\\\\'"
    if " " <= char <= "~":
        return f"'{char}'"
    return f"'\\x{ord(char):02x}'"


def verify_ir_cxx(ir_by_group: dict, cfg: CmakeFlags, scratch: Path, abi: str = ABI_DEFAULT) -> None:
    """Fold the ad-hoc `clang++ -fsyntax-only` cross-check into the pipeline: it
    runs in every mode (except `--no-verify-cxx`)."""
    msvc = abi == ABI_MSVC
    text = gen_verify_tu(ir_by_group, abi)
    tu = scratch / f"verify_ir_{ABI_MSVC_ABI if msvc else ABI}.cpp"
    tu.write_text(text, encoding="utf-8")
    asserts = text.count("static_assert(")
    extra = ["-fno-access-control"] if msvc else []
    vlog(f"verify: clang++ -fsyntax-only {tu} ({asserts} static_asserts)")
    clang(cfg, [*extra, "-fsyntax-only"], tu)
    log(f"verify: clang re-derived {asserts} IR facts for {ABI_MSVC_ABI if msvc else ABI}")


# --------------------------------------------------------------------------- #
# emitting
# --------------------------------------------------------------------------- #


def build_consts(
    consts: list[ConstEntry], values: dict[str, int | str], paths: dict[str, list[str]] | None = None
) -> list[dict]:
    """The `[consts]` IR entries for this ABI's measurements. The top-level
    `value` is the Itanium one (like every other top-level number in the IR) and
    the `"itanium"` sub-object restates it, so both ABIs read the same way.

    `expr` (or `derived`) is kept: the static-assert TU re-derives the value from
    it, and it is what tells a reader where the number came from. A `derived`
    entry also keeps `derived_path`, the C++ member path the verify TU needs to
    spell the bitfield (`measure_derived_paths`)."""
    entries = []
    for const in consts:
        value = values[const.name]
        entry = {
            "name": const.name,
            "kind": const.kind,
            "type": const.rust_type,
            "value": value,
            "per_abi": const.per_abi,
            "itanium": {"value": value},
        }
        if const.doc is not None:
            entry["doc"] = const.doc
        if const.derived is None:
            entry["expr"] = const.expr
        else:
            entry["derived"] = ".".join(const.derived)
            if const.transform is not None:
                entry["transform"] = const.transform
            path = (paths or {}).get(const.name)
            if path is not None:
                entry["derived_path"] = path
        entries.append(entry)
    return entries


def dump_ir(ir: dict) -> str:
    return json.dumps(ir, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def print_summary(ir_by_group: dict, protos: list[str]) -> None:
    records = enums = vtables = specs = consts = 0
    for group in GROUPS:
        ir = ir_by_group[group]
        records += len(ir["records"])
        enums += len(ir["enums"])
        vtables += len(ir["vtables"])
        specs += len(ir.get("specs") or [])
        consts += len(ir.get("consts") or [])
        log(
            f"{group}.json: {len(ir['records'])} records, {len(ir['enums'])} enums, "
            f"{len(ir['vtables'])} vtables, {len(ir.get('specs') or [])} specs, "
            f"{len(ir.get('consts') or [])} consts"
        )
    log(
        f"total: {records} records, {enums} enums, {vtables} vtables, {specs} specs, "
        f"{consts} consts, {len(protos)} generated protobuf headers"
    )


# --------------------------------------------------------------------------- #
# the Rust emitter
# --------------------------------------------------------------------------- #

_RESERVED_IDENTS = ("self", "Self", "super", "crate")

RECORD_HEADER = "//! Generated by gen/gen.py from UPSTREAM.lock {commit}. Do not edit."
# The second upstream's group is generated from METAMOD.lock, whose two
# checkouts both decide its slots (IKHook comes out of the khook submodule), so
# its file says which two revisions it describes instead of the SDK's.
METAMOD_RECORD_HEADER = (
    "//! Generated by gen/gen.py from METAMOD.lock (metamod {commit}, "
    "khook {khook_commit}). Do not edit."
)


def record_header(group: str, commit: str) -> str:
    """The `//!` line a generated file starts with: the metamod group stamps the
    commits of the two checkouts it is generated from, every other file the SDK
    commit the rest of the IR is keyed by."""
    if group != METAMOD_GROUP:
        return RECORD_HEADER.format(commit=commit)
    lock = read_metamod_lock()
    return METAMOD_RECORD_HEADER.format(
        commit=lock["commit"], khook_commit=lock["khook_commit"]
    )

# The set `lib.rs` allows, plus the vtable structs' function-pointer fields: a virtual with
# a dozen parameters is one honest `Option<unsafe extern "C" fn(..)>` that clippy calls "very
# complex". The generated files are as raw as the engine's headers.
RUST_ALLOW = (
    "#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, "
    "clippy::type_complexity)]"
)
# Cross-group names (a `CSchemaType` field of type `CUtlString`, say) resolve
# through the parent module's re-exports. A group that only mentions its own
# types (tier0 today) leaves the glob unused, which is not an error here.
RUST_SUPER_USE = [
    "#[allow(unused_imports)]",
    "use super::*;",
]

# What a per-ABI body is emitted behind. The Itanium body is the `not(windows)`
# one: it is what a Linux (and any other non-Windows) target lays out, and the
# only non-Windows consumer of these bindings is the Linux one.
CFG_ITANIUM = "#[cfg(not(windows))]"
CFG_MSVC = "#[cfg(windows)]"
CFG_ATTR = {ABI_DEFAULT: CFG_ITANIUM, ABI_MSVC: CFG_MSVC}


def rust_ident(name: str) -> str:
    """A Rust identifier for a C++ field or method name."""
    if name in _RESERVED_IDENTS:
        raise GenError(f"{name!r} cannot be spelled as a Rust identifier")
    if name in RUST_KEYWORDS:
        return f"r#{name}"
    return name


def rust_str(text: str) -> str:
    """A Rust string literal for an IR string."""
    out = ['"']
    for char in text:
        if char == '"':
            out.append('\\"')
        elif char == "\\":
            out.append("\\\\")
        elif " " <= char <= "~":
            out.append(char)
        else:
            out.append(f"\\u{{{ord(char):x}}}")
    out.append('"')
    return "".join(out)


def abi_sizes(ir_by_group: dict, abi: str) -> dict[str, int]:
    """Every allowlisted record/opaque/enum by name -> the size in bytes under
    one ABI: a record's size comes from its sub-object, an enum's from the width
    of the `rust_repr` it is emitted with (which `check_dual_abi` has checked
    both ABIs' measured sizes against)."""
    sizes: dict[str, int] = {}
    for group in GROUPS:
        for record in ir_by_group[group]["records"]:
            sizes[record["name"]] = abi_sub_object(record, abi)["size"]
        for enum in ir_by_group[group]["enums"]:
            sizes[enum["name"]] = RUST_SIZES[enum["rust_repr"]]
    return sizes


def abi_sub_object(item: dict, abi: str) -> dict:
    """One record/spec/enum/const's `"itanium"`/`"msvc"` sub-object. A missing
    one is an error: the emitter must not fall back to the other ABI's numbers
    (a binding that silently mixes two ABIs is worse than no binding)."""
    sub = item.get(abi)
    if not isinstance(sub, dict):
        raise GenError(
            f"{item.get('name')}: the IR has no {abi!r} sub-object; "
            f"regenerate the IR with gen.py"
        )
    return sub


def abi_view(item: dict, abi: str) -> dict:
    """A record/specialization as one ABI lays it out: the sub-object's
    size/align/field numbers under the top-level names, `rust_type`s and
    `cxx_type`s (the type *mapping* is not per ABI, the numbers are)."""
    sub = abi_sub_object(item, abi)
    if len(sub["fields"]) != len(item["fields"]):
        raise GenError(
            f"{item['name']}: the {abi!r} field list has {len(sub['fields'])} "
            f"entries, the record has {len(item['fields'])}"
        )
    fields = []
    for field, abi_field in zip(item["fields"], sub["fields"]):
        if field["name"] != abi_field["name"]:
            raise GenError(
                f"{item['name']}: the {abi!r} field list is not the record's "
                f"({abi_field['name']!r} where {field['name']!r} is)"
            )
        fields.append({**field, **abi_field})
    view = dict(item)
    view["size"] = sub["size"]
    view["align"] = sub["align"]
    view["fields"] = fields
    if sub.get("base_offsets"):
        view["bases"] = [
            {**base, "offset": offset}
            for base, offset in zip(item["bases"], sub["base_offsets"])
        ]
    return view


def emit_per_abi(emit: Callable[[str], list[str]]) -> list[str]:
    """The lines of one item, emitted for both ABIs: a single body when the two
    bodies are identical, otherwise the Itanium one behind `cfg(not(windows))`
    and the MSVC one behind `cfg(windows)`.

    `emit` returns the body's own attributes too, so the cfg line is prepended
    to the whole block: an attribute the two ABIs disagree on (a different
    alignment, say) is part of the difference.

    For a vtable struct, "identical" is enough: its body is the base sub-struct
    followed by a fixed sequence of entries, and the base sub-struct is the one
    the `cfg` selects. A class whose own entries shift by a slot (an inherited
    destructor's second entry, say) but whose sequence is the same - no pad in
    either ABI - therefore needs no cfg of its own: the base struct carries the
    ABI into it, and the generated layout test asserts the result per ABI. A
    difference in *any* item (a pad for a slot nothing is bound to, a reversed
    overload group) is a difference in the emitted lines."""
    itanium = emit(ABI_DEFAULT)
    msvc = emit(ABI_MSVC)
    if itanium == msvc:
        return itanium
    return [CFG_ATTR[ABI_DEFAULT], *itanium, CFG_ATTR[ABI_MSVC], *msvc]


@dataclasses.dataclass
class Member:
    """One emitted struct field: its Rust name, where it sits and how wide the
    C++ member it stands for is."""

    name: str
    offset: int
    rust_type: str
    size: int
    doc: str | None = None


# The field name of a record's vtable pointer. A record that is *also* a vtable
# class has to use the name its `impl { fn vtable(..) }` helper reads (`vtable`);
# every other record keeps `vtable_`, which is what the committed bindings have.
VPTR_FIELD = "vtable_"
VPTR_FIELD_VTABLE_CLASS = "vtable"
# inherited_vptr_path: read the vtable pointer at offset 0 of the object
RAW_VPTR = "<raw>"


def vptr_field_name(record: dict, shapes: dict[str, dict]) -> str:
    return VPTR_FIELD_VTABLE_CLASS if record["name"] in shapes else VPTR_FIELD


def inherited_vptr_path(class_name: str, records: Mapping[str, dict], shapes: dict[str, dict]) -> str | None:
    """The field path to the vtable pointer of a record that is also a vtable
    class but derives its vptr from a record base (`base.vtable`: CCSGOInput
    starts with a CClientInput). `records` holds the records of every group (a
    base can live in another one). None when the record holds the pointer
    itself, or when the class is not a record. RAW_VPTR when the chain ends in a
    primary base that has no record (an opaque class: its bytes are kept, so the
    pointer is read at offset 0)."""
    record = records.get(class_name)
    path: list[str] = []
    while record is not None and not record["has_vptr"] and record["bases"]:
        path.append("base")
        first = record["bases"][0]
        record = records.get(first["name"])
        if record is None:
            if first["offset"] != 0 or first.get("virtual"):
                return None
            return RAW_VPTR
    if record is None or not path or not record["has_vptr"]:
        # a chain that ends on a non-polymorphic record has no pointer to read
        return None
    return ".".join([*path, vptr_field_name(record, shapes)])


def record_members(
    record: dict, sizes: dict[str, int], vptr: str = VPTR_FIELD
) -> list[Member]:
    """The emitted fields of a record: the vtable pointer, the bases, the
    fields, and a `_padN` byte blob wherever the next member does not start
    where the previous one ended. Correctness never depends on Rust's own
    padding rules, so a gap can never be silently absorbed."""
    name = record["name"]
    members: list[Member] = []
    cursor = 0

    def pad_to(offset: int, what: str) -> None:
        nonlocal cursor
        if offset < cursor:
            raise GenError(
                f"{name}: {what} starts at {offset}, inside the previous member "
                f"(which ends at {cursor}); a primary base that shares the derived "
                f"object's vtable pointer is not modelled yet"
            )
        if offset > cursor:
            members.append(
                Member(f"_pad{len(members)}", cursor, f"[u8; {offset - cursor}]", offset - cursor)
            )
        cursor = offset

    if record["has_vptr"]:
        pad_to(0, "the vtable pointer")
        members.append(Member(vptr, 0, "*const ::core::ffi::c_void", 8))
        cursor = 8

    for index, base in enumerate(record["bases"]):
        base_name = base["name"]
        if base["virtual"]:
            raise GenError(
                f"{name}: {base_name} is a virtual base; the caller emits the "
                f"record as an opaque blob instead"
            )
        if base_name not in sizes:
            raise GenError(
                f"{name}: base {base_name} is not in the IR; add it to allowlist.toml"
            )
        pad_to(base["offset"], f"base {base_name}")
        field = "base" if index == 0 else f"base_{base_name}"
        members.append(Member(field, base["offset"], base_name, sizes[base_name]))
        cursor = base["offset"] + sizes[base_name]

    bit_run = 0
    index = 0
    fields = record["fields"]
    while index < len(fields):
        field = fields[index]
        if field["bit_width"] is not None:
            run = []
            while index < len(fields) and fields[index]["bit_width"] is not None:
                run.append(fields[index])
                index += 1
            start = run[0]["offset"]
            end = max((f["bit_offset"] + f["bit_width"] + 7) // 8 for f in run)
            pad_to(start, "a bitfield run")
            members.append(Member(f"_bits{bit_run}", start, f"[u8; {end - start}]", end - start))
            cursor = end
            bit_run += 1
            continue
        pad_to(field["offset"], f"field {field['name']}")
        rust = field["rust_type"] or f"[u8; {field['size']}]"
        members.append(
            Member(
                rust_ident(field["name"]),
                field["offset"],
                rust,
                field["size"],
                doc=None if field["rust_type"] else field["cxx_type"],
            )
        )
        cursor = field["offset"] + field["size"]
        index += 1

    pad_to(record["size"], "the trailing pad")
    return members


def union_members(record: dict) -> list[Member]:
    """Every member of a C++ union sits at offset 0; the union's size is the
    widest member rounded up to the alignment, so a blob member only has to be
    added when the fields do not already reach the dump's size."""
    members: list[Member] = []
    widest = 0
    for field in record["fields"]:
        if field["bit_width"] is not None:
            raise GenError(f"{record['name']}: bitfields in a union are not modelled")
        rust = field["rust_type"] or f"[u8; {field['size']}]"
        members.append(
            Member(
                rust_ident(field["name"]),
                0,
                rust,
                field["size"],
                doc=None if field["rust_type"] else field["cxx_type"],
            )
        )
        widest = max(widest, field["size"])
    if widest < record["size"]:
        members.append(Member("_pad", 0, f"[u8; {record['size']}]", record["size"]))
    return members


def opaque_record(record: dict) -> bool:
    """A record the flat IR cannot spell: a virtual base, or the allowlist's
    `opaque` list."""
    if record["opaque"]:
        return True
    if any(base["virtual"] for base in record["bases"]):
        log(
            f"warning: {record['name']} has a virtual base; emitting it as an "
            f"opaque [u8; {record['size']}] blob"
        )
        return True
    return False


def emit_record(
    record: dict, sizes: dict[str, int], vptr: str = VPTR_FIELD
) -> list[str]:
    name = record["name"]
    if opaque_record(record):
        return [
            f"#[repr(C, align({record['align']}))]",
            "#[derive(Copy, Clone)]",
            f"pub struct {name}(pub [u8; {record['size']}]);",
        ]

    if record["kind"] == "union":
        members = union_members(record)
        lines = [
            f"#[repr(C, align({record['align']}))]",
            "#[derive(Copy, Clone)]",
            f"pub union {name} {{",
        ]
    else:
        members = record_members(record, sizes, vptr)
        lines = [
            f"#[repr(C, align({record['align']}))]",
            "#[derive(Copy, Clone)]",
            f"pub struct {name} {{",
        ]
    for member in members:
        if member.doc:
            lines.append(f"    /// `{member.doc}`: no exact Rust type, so the bytes are kept.")
        lines.append(f"    pub {member.name}: {member.rust_type},")
    lines.append("}")
    return lines


def emit_enum(enum: dict) -> list[str]:
    name = enum["name"]
    repr_ = enum["rust_repr"]
    lines = [
        "#[repr(transparent)]",
        "#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]",
        f"pub struct {name}(pub {repr_});",
        "",
        f"impl {name} {{",
    ]
    for value in enum["values"]:
        lines.append(f"    pub const {rust_ident(value['name'])}: Self = Self({value['value']});")
    lines.append("}")
    return lines


@dataclasses.dataclass
class VEntry:
    """One own virtual of a vtable class, with the Rust names it is emitted
    under, the key `abi::slot` resolves it by, and the two ABI-specific halves:
    the slot its first field sits at and the field list the ABI's vtable struct
    declares (an override has no field - the base's owns the slot - and a
    destructor has two on Itanium, one on MSVC)."""

    method: dict
    const: str
    key: str
    slots: dict[str, int]
    fields: dict[str, list[str]]


def vtable_entries(vtable: dict) -> list[VEntry]:
    class_name = vtable["class"]
    counts: dict[str, int] = {}
    entries = []
    for method in vtable["methods"]:
        stem = "dtor" if method["is_dtor"] else rust_ident(method["name"])
        k = counts.get(stem, 0)
        counts[stem] = k + 1
        suffix = "" if k == 0 else f"_{k}"
        key = method["name"] if k == 0 else f"{method['name']}#{k}"
        if method["overrides"]:
            fields = {abi: [] for abi in ABIS}  # the base field owns this slot
        elif method["is_dtor"]:
            # Itanium lays out two consecutive slots (complete-object, then
            # deleting); MSVC gives a destructor one, named for its slot const.
            fields = {ABI_DEFAULT: ["dtor_complete", "dtor_deleting"], ABI_MSVC: ["dtor"]}
        else:
            fields = {abi: [stem + suffix] for abi in ABIS}
        if method["is_dtor"] and not method["overrides"]:
            const = "dtor"
        else:
            const = stem + suffix
        slots = {abi: method[f"{abi}_slot"] for abi in ABIS}
        entries.append(VEntry(method=method, const=const, key=key, slots=slots, fields=fields))
    vlog(f"{class_name}: {len(entries)} own virtuals")
    return entries


def vtable_shapes(ir_by_group: dict) -> dict[str, dict]:
    """{class: {"group", "vtable", "entries", "slots", "slot_checks",
    "base_len", "length"}} for every allowlisted vtable, per ABI.

    * `slots` - abi -> the flat const -> slot map the emitted module declares
      (own + inherited);
    * `slot_checks` - abi -> the same information as (const, slot, abi key)
      triples for the generated layout test, which re-derives every one of them
      through `abi::slot`;
    * `base_len` / `length` - abi -> how many slots the primary base's vtable
      struct and this class's own one hold. A destructor contributes two slots
      on Itanium and one on MSVC, which is why the chain's lengths are per ABI."""
    shapes: dict[str, dict] = {}
    for group in GROUPS:
        for vtable in ir_by_group[group]["vtables"]:
            class_name = vtable["class"]
            if class_name in shapes:
                raise GenError(f"{class_name}: listed as a vtable twice")
            shapes[class_name] = {
                "group": group,
                "vtable": vtable,
                "entries": vtable_entries(vtable),
                "slots": {},
                "slot_checks": {},
                "base_len": {},
                "length": {},
            }

    for class_name in shapes:
        shape = shapes[class_name]
        base = shape["vtable"]["primary_base"]
        if base is None:
            shape["base_len"] = dict.fromkeys(ABIS, 0)
        else:
            if base not in shapes:
                raise GenError(
                    f"{class_name}: primary_base {base} is not in the IR; add {base} "
                    f"to allowlist.toml (a VClass static has to name its base)"
                )
            shape["base_len"] = dict(shapes[base]["length"])
        for abi in ABIS:
            slots: dict[str, int] = {}
            checks: list[tuple[str, int, str]] = []
            if base is not None:
                slots.update(shapes[base]["slots"][abi])
                checks.extend(shapes[base]["slot_checks"][abi])
            own = shape["base_len"][abi]
            for entry in shape["entries"]:
                const = entry.const
                slot = entry.slots[abi]
                if const in slots and slots[const] != slot:
                    raise GenError(
                        f"{class_name}: {const} inherits slot {slots[const]} but its own "
                        f"declaration is at {slot}; the overload/override naming is "
                        f"ambiguous"
                    )
                slots[const] = slot
                checks.append((const, slot, entry.key))
                own += len(entry.fields[abi])
            shape["slots"][abi] = slots
            shape["slot_checks"][abi] = checks
            shape["length"][abi] = own
    return shapes


@dataclasses.dataclass
class VSlot:
    """One member of an emitted vtable struct: the primary base's sub-struct, one
    declaration's vtable entry, or a `_padN` blob standing for a slot of the
    ABI's table that no declaration of the IR is bound to."""

    name: str
    offset: int
    kind: str  # "base" | "field" | "pad"
    size: int = 8
    method: dict | None = None
    index: int = 0


def vtable_slot_items(shape: dict, abi: str) -> list[VSlot]:
    """The members of one ABI's vtable struct, in offset order.

    The primary base's sub-struct comes first and covers its slots; this class's
    own entries are then laid out by **this ABI's** slot number, which is where
    the two halves really differ: MSVC packs an overload group reversed at the
    first declaration's position (so a field's declaration order and its slot
    order disagree) and gives a destructor one entry where Itanium gives two.
    A slot nothing is bound to (clang prints a slot for the implicit destructor
    of a class that declares none, say) becomes a `_padN` blob, so the struct is
    always as long as the ABI's table - the generated test asserts its size
    against `abi::vtable_len` and, for MSVC, against clang's printed length."""
    vtable = shape["vtable"]
    class_name = vtable["class"]
    base = vtable["primary_base"]
    items: list[VSlot] = []
    cursor = 0
    if base is not None:
        items.append(VSlot(name="base", offset=0, kind="base", size=shape["base_len"][abi] * 8))
        cursor = shape["base_len"][abi] * 8

    bound: list[tuple[int, VEntry, str, int]] = []
    for entry in shape["entries"]:
        for index, field in enumerate(entry.fields[abi]):
            bound.append((entry.slots[abi] + index, entry, field, index))
    bound.sort(key=lambda item: item[0])

    pads = 0
    for slot, entry, field, index in bound:
        offset = slot * 8
        if offset < cursor:
            raise GenError(
                f"{class_name}: {abi}: {field} is bound to slot {slot}, which is "
                f"inside the member that ends at byte {cursor}; the declarations "
                f"and the ABI's slot numbers disagree"
            )
        if offset > cursor:
            items.append(VSlot(name=f"_pad{pads}", offset=cursor, kind="pad", size=offset - cursor))
            pads += 1
        items.append(VSlot(name=field, offset=offset, kind="field", method=entry.method, index=index))
        cursor = offset + 8

    # clang's own vftable length for the MSVC side (the IR has no Itanium one:
    # that half's length is exactly the slots the declarations bind). A slot
    # clang printed and no declaration owns pads the struct rather than leaving
    # it short - and the generated `msvc_slots_match_the_abi_rules` still
    # asserts `vtable_len(Msvc, ..)` against clang's number, so a table the IR
    # does not describe stays a failure.
    if abi == ABI_MSVC:
        printed = (vtable.get(ABI_MSVC) or {}).get("length")
        total = shape["length"][abi] * 8
        if printed is not None and printed * 8 > total:
            total = printed * 8
        if cursor < total:
            items.append(VSlot(name=f"_pad{pads}", offset=cursor, kind="pad", size=total - cursor))
    return items


def emit_for(windows_only: bool, emit: Callable[[str], list[str]]) -> list[str]:
    """`emit_per_abi`, except that a Windows-only item is emitted once, from its
    MSVC numbers: its module is gated on `cfg(windows)` as a whole, so no
    `cfg(not(windows))` body (an Itanium claim) is ever written."""
    if windows_only:
        return emit(ABI_MSVC)
    return emit_per_abi(emit)


def emit_secondary_bases(vtable: dict, known: Collection[str], abi: str) -> list[str]:
    """`impl X { BASE_OFFSET_<Base>; as_<base>() }` for each secondary base: where
    the base subobject sits in this ABI's object (clang's record layout), and the
    pointer adjustment a caller needs to use the base's interface on the object.
    The cast is only emitted for a base that has a generated type."""
    bases = vtable["secondary_bases"]
    if not bases:
        return []
    name = rust_ident(vtable["class"])
    lines = [f"impl {name} {{"]
    for base in bases:
        offset = base["msvc_offset"] if abi == ABI_MSVC else base["offset"]
        base_name = base["name"]
        lines += [
            f"    /// Byte offset of the `{base_name}` base subobject in a `{vtable['class']}`.",
            f"    pub const BASE_OFFSET_{base_name}: usize = {offset};",
        ]
        if base_name in known:
            lines += [
                "",
                f"    /// The `{base_name}` subobject of `this`.",
                "    ///",
                "    /// # Safety",
                "    ///",
                f"    /// `this` must point at a live `{vtable['class']}`.",
                f"    pub unsafe fn as_{base_name.lower()}(this: *mut Self) -> *mut {rust_ident(base_name)} {{",
                "        // SAFETY: the caller guarantees `this` points at the whole object, and",
                "        // the base subobject lies inside it.",
                f"        unsafe {{ this.byte_add(Self::BASE_OFFSET_{base_name}).cast() }}",
                "    }",
            ]
    lines.append("}")
    return lines


def emit_vtable(
    shape: dict,
    sizes: dict[str, int],
    windows_only: bool = False,
    known: Collection[str] = (),
    vptr_path: str | None = None,
    c_header: bool = True,
) -> list[str]:
    vtable = shape["vtable"]
    class_name = vtable["class"]
    base = vtable["primary_base"]
    name = rust_ident(class_name)
    lines = []

    # A class that is also a record keeps its vtable pointer in the emitted
    # record (`vptr_field_name`), as a plain `*const c_void`. The accessor
    # casts; every other vtable class owns the pointer in the wrapper struct
    # emitted just below, where it is already typed. Both are ABI-independent:
    # the pointer's target type is the ABI's `X_vtable`, whichever cfg is on.
    if class_name in sizes and vptr_path == RAW_VPTR:
        vptr_access = f"(*(this as *const *const ::core::ffi::c_void) as *const {class_name}_vtable)"
    elif class_name in sizes:
        vptr_access = (
            f"((*this).{vptr_path or VPTR_FIELD_VTABLE_CLASS} as *const {class_name}_vtable)"
        )
    else:
        vptr_access = "(*this).vtable"

    if class_name not in sizes:
        lines += [
            "/// The C++ object only, as it looks from the vtable's side: it is only",
            "/// ever used behind a pointer, and the real object is larger.",
            "#[repr(C)]",
            f"pub struct {name} {{",
            f"    pub vtable: *const {class_name}_vtable,",
            "}",
            "",
        ]
    lines += [
        f"impl {name} {{",
        "    /// The vtable this object points at.",
        "    ///",
        "    /// # Safety",
        "    ///",
        "    /// `this` must point at a live object whose vtable pointer is non-null",
        f"    /// and points at a valid `{class_name}_vtable`.",
        f"    pub unsafe fn vtable<'a>(this: *const Self) -> &'a {class_name}_vtable {{",
        "        // SAFETY: the caller guarantees `this` and its vtable pointer are",
        "        // valid; `&*` reborrows the pointee for the caller's lifetime.",
        f"        unsafe {{ &*{vptr_access} }}",
        "    }",
        "}",
        "",
    ]
    secondary = emit_for(windows_only, lambda abi: emit_secondary_bases(vtable, known, abi))
    if secondary:
        lines += secondary
        lines.append("")
    lines += emit_for(windows_only, lambda abi: emit_vtable_struct(shape, abi))
    lines.append("")
    lines += emit_for(windows_only, lambda abi: emit_vtable_null(shape, abi))
    lines.append("")
    lines += emit_for(windows_only, lambda abi: emit_vtable_slots(shape, abi, c_header))
    lines.append("")

    methods = vtable["methods"]
    lines.append(f"pub static {class_name}_VCLASS: VClass = VClass {{")
    lines.append(f"    name: {rust_str(class_name)},")
    if base is not None:
        lines.append(f"    primary_base: Some(&{base}_VCLASS),")
    else:
        lines.append("    primary_base: None,")
    if not methods:
        lines.append("    methods: &[],")
    else:
        lines.append("    methods: &[")
        for method in methods:
            overrides = (
                f"Some({rust_str(method['overrides'])})" if method["overrides"] else "None"
            )
            lines.append(
                f"        VMethod {{ name: {rust_str(method['name'])}, "
                f"is_dtor: {str(bool(method['is_dtor'])).lower()}, overrides: {overrides} }},"
            )
        lines.append("    ],")
    lines.append("};")
    return lines


def emit_vtable_struct(shape: dict, abi: str) -> list[str]:
    """`X_vtable` as the ABI lays it out: the base sub-struct, this class's own
    entries at their slot numbers, and a pad for every slot nothing is bound
    to."""
    class_name = shape["vtable"]["class"]
    lines = [
        "#[repr(C)]",
        "#[derive(Copy, Clone)]",
        f"pub struct {class_name}_vtable {{",
    ]
    for item in vtable_slot_items(shape, abi):
        if item.kind == "base":
            base = shape["vtable"]["primary_base"]
            lines.append(f"    pub base: {base}_vtable,")
        elif item.kind == "pad":
            lines += [
                f"    /// A slot of the {abi} table no declaration is bound to.",
                f"    pub {item.name}: [u8; {item.size}],",
            ]
        else:
            lines += vtable_field_lines(class_name, item.method, item.name, item.index, abi)
    lines.append("}")
    return lines


def emit_vtable_null(shape: dict, abi: str) -> list[str]:
    """`X_vtable::NULL`: every entry null, the `Assert`-style partial
    initialisation (`Foo_vtable { FindTypeScopeForModule: Some(f),
    ..Foo_vtable::NULL }`)."""
    class_name = shape["vtable"]["class"]
    lines = [
        f"impl {class_name}_vtable {{",
        "    /// Every entry null: `Assert`-style partial initialisation for tests",
        "    /// (`Foo_vtable { FindTypeScopeForModule: Some(f), ..Foo_vtable::NULL }`).",
        "    pub const NULL: Self = Self {",
    ]
    for item in vtable_slot_items(shape, abi):
        if item.kind == "base":
            base = shape["vtable"]["primary_base"]
            lines.append(f"        base: {base}_vtable::NULL,")
        elif item.kind == "pad":
            lines.append(f"        {item.name}: [0; {item.size}],")
        elif (
            item.method["is_dtor"]
            or item.method["rust_fn"]
            or (abi == ABI_MSVC and item.method.get("msvc_rust_fn"))
        ):
            lines.append(f"        {item.name}: None,")
        else:
            lines.append(f"        {item.name}: ::core::ptr::null(),")
    lines.append("    };")
    lines.append("}")
    return lines


def vtable_length_const(shape: dict, abi: str) -> int:
    """The number the emitted `VTLEN` const carries, per ABI - the same source
    of truth the C header's `S2_VTLEN_<Class>` uses, never a recompute:

    * MSVC: clang's own vftable length (`vtable["msvc"]["length"]`, the exact
      expression `emit_c_header`'s `S2_VTLEN_<Class>` define reads) when clang
      printed a vftable; the ABI-rule walk (`shape["length"]`) only when it
      printed none - the case the C header spells as a comment.
    * Itanium: the ABI-rule walk. The IR has no Itanium vtable dump (see
      `vtable_slot_items`); the walk is what the emitted vtable struct's size
      is asserted against, so it is the only Itanium length gen.py has.

    A printed MSVC length below the walk is a generator bug (the walk binds a
    subset of the real table's slots) and fails the run - the emitted const
    must never be a silently picked one of the two."""
    vtable = shape["vtable"]
    walked = shape["length"][abi]
    if abi == ABI_MSVC:
        printed = (vtable.get(ABI_MSVC) or {}).get("length")
        if printed is not None:
            if printed < walked:
                raise GenError(
                    f"{vtable['class']}: clang's vftable length {printed} is below the "
                    f"slot walk {walked}; the walk binds slots the table does not have"
                )
            return printed
    return walked


def emit_vtable_slots(shape: dict, abi: str, c_header: bool = True) -> list[str]:
    """`X_slots`: one const per own and inherited slot, as this ABI numbers
    them (`dtor` for the destructor), plus - for every own method that returns
    a class by value - this ABI's return transport as `{name}_SRET` /
    `{name}_RET_SIZE` (and `{name}_RET_REGS` on Itanium, where a register
    return has a class to spell): the same facts the IR's `ret` object
    carries, for a consumer that reads the emitted Rust and not the IR.
    The module ends with this ABI's `VTLEN` (`vtable_length_const`), whose
    doc comment cites the C header's `S2_VTLEN_<Class>` define when the
    class's group is emitted into it (`c_header`; the metamod group is not,
    so its `VTLEN` says so instead of naming a define that does not exist)."""
    class_name = shape["vtable"]["class"]
    lines = [
        f"pub mod {class_name}_slots {{",
        "    //! Slots from the address point (slot 0 = the first entry).",
    ]
    for const, slot in shape["slots"][abi].items():
        lines.append(f"    pub const {const}: usize = {slot};")
    for entry in shape["entries"]:
        method = entry.method
        ret = method.get("ret")
        if not ret or method["is_dtor"]:
            continue
        lines += [
            "",
            f"    /// `{method['name']}` returns a class by value: this ABI's transport",
            f"    /// (the IR's `ret` object; `{entry.const}_SRET` is the hidden sret",
            "    /// pointer, after `this` on MSVC, first on Itanium).",
            f"    pub const {entry.const}_SRET: bool = {'true' if ret[abi]['sret'] else 'false'};",
            f"    pub const {entry.const}_RET_SIZE: usize = {ret[abi]['size']};",
        ]
        if abi != ABI_MSVC:
            lines.append(f"    pub const {entry.const}_RET_REGS: &str = {rust_str(ret[abi]['regs'])};")
    length = vtable_length_const(shape, abi)
    lines += ["", "    /// This ABI's vtable length in slots (the RTTI entry at vt[-1] excluded):"]
    if c_header:
        lines += [
            "    /// the same number the C header's `S2_VTLEN_"
            + class_name
            + "` define carries -",
            "    /// clang's own vftable length for MSVC, the ABI-rule walk for Itanium",
            "    /// (where the IR has no dump). A consumer reads this instead of",
            "    /// recomputing a length as the last slot + 1.",
        ]
    else:
        lines += [
            "    /// clang's own vftable length for MSVC, the ABI-rule walk for Itanium",
            "    /// (where the IR has no dump). This class's group is not emitted into",
            "    /// `crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h`, so there is no `S2_VTLEN_"
            + class_name
            + "` define;",
            "    /// a consumer reads this instead of recomputing a length as the last",
            "    /// slot + 1.",
        ]
    lines += [f"    pub const VTLEN: usize = {length};"]
    lines.append("}")
    return lines


MSVC_DTOR_RTYPE = "::core::ffi::c_void"


def msvc_dtor_type(class_name: str) -> str:
    """The Rust fn type an MSVC `dtor` slot points at: MSVC's single virtual-
    destructor entry is not `~T()` itself but the compiler-generated *scalar-
    deleting* wrapper around it, whose signature no AST declaration carries
    (the IR's `cxx_signature` is `~T()`'s, `void ()`). The wrapper is easy to
    derive, and the MS ABI rule is fixed, so the emitter spells it once here
    instead of trusting nothing: clang mangles the wrapper
    `??_G<member-with-class>@@UEAAPEAXI@Z`, which demangles to
    `void* __cdecl(T* this, unsigned int flags)`; `flags & 1` - the deleting
    flag - tells the callee to run `operator delete` on `this` after
    destroying it, and the return value hands back the adjusted `this`
    (`*mut c_void` in Rust terms). `extern "C"` is right on x64, which passes
    `this` in RCX like the C ABI and has no separate thiscall."""
    return f'Option<unsafe extern "C" fn(this: *mut {class_name}, flags: u32) -> *mut {MSVC_DTOR_RTYPE}>'


def vtable_field_lines(
    class_name: str, method: dict, field: str, index: int, abi: str
) -> list[str]:
    """One vtable struct field. An Itanium destructor contributes two entries
    (complete and deleting), an MSVC one a single `dtor` (MSVC's `[scalar
    deleting]` entry - its signature is the compiler-generated wrapper's, see
    `msvc_dtor_type`); every entry is an `Option<fn>` so a null slot is not
    UB."""
    if method["is_dtor"]:
        # `this` is the object, not the vtable: `Self` here would name the
        # vtable struct, which is not what a destructor receives.
        if abi == ABI_MSVC:
            return [
                "    /// The scalar-deleting destructor: MSVC's compiler-generated wrapper around",
                f"    /// the declared destructor `{method['cxx_signature']}` (clang mangles the",
                f"    /// wrapper `??_G{class_name}@@UEAAPEAXI@Z`; see `msvc_dtor_type`).",
                f"    pub {field}: {msvc_dtor_type(class_name)},",
            ]
        rust = f"Option<unsafe extern \"C\" fn(this: *mut {class_name})>"
        what = "complete-object" if index == 0 else "deleting"
        return [f"    /// The {what} destructor. `{method['cxx_signature']}`", f"    pub {field}: {rust},"]
    # A class returned by value has its own Windows form (the hidden return pointer),
    # which exists even when the native form does not (`attach_msvc_rust_fn`).
    rust_fn = (method.get("msvc_rust_fn") if abi == ABI_MSVC else None) or method["rust_fn"]
    if rust_fn:
        return [f"    pub {field}: Option<{rust_fn}>,"]
    return [
        f"    /// `{method['cxx_signature']}`: no exact Rust type for the signature,",
        f"    /// call through the raw pointer.",
        f"    pub {field}: *const ::core::ffi::c_void,",
    ]


def emit_const_lines(const: dict) -> list[str]:
    """One `[consts]` entry as a Rust const. A `per_abi` entry is emitted twice,
    behind `cfg(windows)` and `cfg(not(windows))`; every other one has a single
    value both ABIs were measured on."""
    lines = []
    if const.get("doc"):
        lines.append(f"/// {const['doc']}")
    ty = const["type"]

    def value(abi: str) -> str:
        raw = const[abi]["value"]
        return rust_str(raw) if isinstance(raw, str) else str(raw)

    if const["per_abi"]:
        lines += [
            "#[cfg(not(windows))]",
            f"pub const {const['name']}: {ty} = {value('itanium')};",
            "#[cfg(windows)]",
            f"pub const {const['name']}: {ty} = {value('msvc')};",
        ]
    else:
        lines.append(f"pub const {const['name']}: {ty} = {value('itanium')};")
    return lines


def emit_consts_rs(ir: dict, commit: str) -> str:
    """src/generated/consts.rs: the `[consts]` group. It has no types, so the
    file is nothing but the measured values."""
    lines = [
        RECORD_HEADER.format(commit=commit),
        RUST_ALLOW,
        "",
        "//! The `[consts]` kind: macros and constexprs a consumer would otherwise",
        "//! re-spell by hand. Every value is produced by clang from the expression",
        "//! in gen/allowlist.toml (see gen/README.md); nothing here is entered by",
        "//! hand, and an entry the two ABIs disagree on has to say `per_abi`.",
        "",
    ]
    for const in ir["consts"]:
        lines += emit_const_lines(const)
        lines.append("")
    return "\n".join(lines)


def emit_spec(spec: dict, sizes: dict[str, int]) -> list[str]:
    """One class-template specialization, emitted like a record under the
    Rust-safe tag `spec_tag` names, with the inherited members flattened."""
    name = spec["rust_name"]
    # canPassInRegisters is clang's "trivial for the purposes of calls": a spec with a
    # user-provided copy constructor or destructor (CStrongHandle's reference count,
    # CUtlVector's allocation) is not a bit-copyable value, so it is not Copy/Clone.
    trivial = bool(spec.get("pass_in_registers"))
    lines = [
        f"/// `{spec['name']}`, as clang lays it out with its subobjects flattened.",
        "/// A C++ template-id has no Rust spelling of its own, so this is the layout only.",
    ]
    if trivial:
        lines.append("/// Meant to be used behind a pointer.")
    else:
        lines.append("/// It owns a resource (copy constructor / destructor), so it is neither Copy nor Clone.")
    lines.append(f"#[repr(C, align({spec['align']}))]")
    if trivial:
        lines.append("#[derive(Copy, Clone)]")
    lines.append(f"pub struct {name} {{")
    for member in record_members(spec, sizes):
        if member.doc:
            lines.append(f"    /// `{member.doc}`: no exact Rust type, so the bytes are kept.")
        lines.append(f"    pub {member.name}: {member.rust_type},")
    lines.append("}")
    return lines


def emit_group_rs(
    group: str,
    ir: dict,
    commit: str,
    shapes: dict[str, dict],
    sizes_by_abi: dict[str, dict[str, int]],
    all_records: Mapping[str, dict] | None = None,
) -> str:
    if group == CONST_GROUP:
        return emit_consts_rs(ir, commit)
    windows_only = ir_windows_only(ir)
    lines = [
        record_header(group, commit),
        *(
            [
                "//! Windows-only group: mod.rs gates this module on `cfg(windows)`; every",
                "//! number below is the MSVC one (no Linux build of these modules exists).",
            ]
            if windows_only
            else []
        ),
        RUST_ALLOW,
        "",
        "use crate::abi::{VClass, VMethod};",
        *RUST_SUPER_USE,
        "",
    ]
    for record in ir["records"]:
        vptr = vptr_field_name(record, shapes)
        lines += emit_for(
            windows_only,
            lambda abi: emit_record(abi_view(record, abi), sizes_by_abi[abi], vptr),
        )
        lines.append("")
    for spec in ir.get("specs") or []:
        lines += emit_for(
            windows_only, lambda abi: emit_spec(abi_view(spec, abi), sizes_by_abi[abi])
        )
        lines.append("")
    for enum in ir["enums"]:
        # An enum's body is its `rust_repr` and its enumerator values, neither of
        # which the IR carries per ABI: `check_dual_abi` refuses an enum whose
        # MSVC size is not the emitted repr's, so there is one body to emit.
        lines += emit_enum(enum)
        lines.append("")
    for vtable in ir["vtables"]:
        lines += emit_vtable(
                shapes[vtable["class"]],
                sizes_by_abi[ABI_DEFAULT],
                windows_only,
                shapes.keys(),
                inherited_vptr_path(
                    vtable["class"],
                    all_records or {r["name"]: r for r in ir["records"]},
                    shapes,
                ),
                group not in C_HEADER_EXCLUDED_GROUPS,
            )
        lines.append("")
    return "\n".join(lines)


def emit_mod_rs(commit: str, windows_only: Sequence[str] = ()) -> str:
    """mod.rs. A Windows-only group's module and re-export are gated on
    `cfg(windows)`: on Linux the group does not exist."""

    def gate(group: str) -> list[str]:
        return [CFG_MSVC] if group in windows_only else []

    lines = [RECORD_HEADER.format(commit=commit), ""]
    for group in GROUPS:
        lines += [*gate(group), f"mod {group};"]
    lines.append("")
    for group in GROUPS:
        lines += [*gate(group), f"pub use self::{group}::*;"]
    lines.append("")
    return "\n".join(lines)


def emit_layout_tests(
    ir_by_group: dict,
    commit: str,
    shapes: dict[str, dict],
    sizes_by_abi: dict[str, dict[str, int]],
    checks: Mapping[str, dict] | None = None,
) -> str:
    """tests/layout.rs: one `mod` per ABI, each gated on the `cfg` whose
    generated bodies it asserts.

    Every number in it is the compiler's: clang's layout dump (per ABI) -> the
    `#[repr(C, align(N))]` struct -> the slot rule in `s2sdk_sys::abi`. The
    Itanium module additionally reproduces every `msvc_slot` through
    `abi::slot(Abi::Msvc, ..)` and checks `vtable_len(Msvc, ..)` against the
    length clang printed for the class's MSVC vftable, so the MSVC rule (and the
    bridge's vtable-length pins) are checked from Linux too."""
    lines = [
        RECORD_HEADER.format(commit=commit),
        "//!",
        "//! The generator/ABI consistency triangle, once per ABI: clang's layout",
        "//! dump -> the `#[repr(C, align(N))]` struct body -> the slot rule in",
        "//! `s2sdk_sys::abi`. Each module is gated on the same `cfg` the bodies it",
        "//! asserts are emitted behind, so a Windows test build asserts the MSVC",
        "//! numbers and a Linux one the Itanium numbers.",
        "//!",
        "//! The MSVC numbers of the *Itanium* module are the cross-check: the",
        "//! `msvc_slot` of every method comes from clang's `-fdump-vtable-layouts`",
        "//! under `--target=x86_64-pc-windows-msvc` and has to be what",
        "//! `abi::slot(Abi::Msvc, ..)` computes from the same declarations.",
        "",
    ]
    lines += emit_layout_test_module(ABI_DEFAULT, ir_by_group, shapes, sizes_by_abi, checks)
    lines.append("")
    lines += emit_layout_test_module(ABI_MSVC, ir_by_group, shapes, sizes_by_abi, checks)
    return "\n".join(lines)


# The OS/arch each ABI's half of the layout test is meaningful on: the Itanium
# numbers come from an x86_64 Linux clang, the MSVC ones from an x86_64 Windows
# target. `Abi::native()` inside a module is that module's ABI.
LAYOUT_TEST_CFG = {
    ABI_DEFAULT: '#[cfg(all(not(windows), target_os = "linux", target_arch = "x86_64"))]',
    ABI_MSVC: '#[cfg(all(windows, target_arch = "x86_64"))]',
}


def emit_layout_test_module(
    abi: str,
    ir_by_group: dict,
    shapes: dict[str, dict],
    sizes_by_abi: dict[str, dict[str, int]],
    checks: Mapping[str, dict] | None = None,
) -> list[str]:
    """One ABI's test module: its records/specializations/vtables and, on the
    Itanium side, the MSVC cross-check."""
    sizes = sizes_by_abi[abi]
    # A Windows-only group does not exist in the Itanium (Linux) build, so its
    # Itanium module neither names its types nor asserts its numbers.
    groups = [
        g for g in GROUPS if abi == ABI_MSVC or not ir_windows_only(ir_by_group[g])
    ]
    lines = [
        LAYOUT_TEST_CFG[abi],
        f"mod {abi} {{",
        "use core::mem::{align_of, offset_of, size_of};",
        "",
        "use s2sdk_sys::abi::{Abi, slot, vtable_len};",
        "use s2sdk_sys::generated::*;",
        "",
        "/// Every record and specialization this ABI's IR was emitted from, against",
        "/// the sizes, alignments and offsets clang printed for that ABI.",
        "#[test]",
        "fn records_match_the_clang_layout_dump() {",
    ]
    for group in groups:
        for record in ir_by_group[group]["records"]:
            name = record["name"]
            view = abi_view(record, abi)
            lines.append(f"// {group}: {record['header']}")
            lines.append(f"assert_eq!(size_of::<{name}>(), {view['size']});")
            lines.append(f"assert_eq!(align_of::<{name}>(), {view['align']});")
            if opaque_record(view) or view["kind"] == "union":
                continue
            for member in record_members(view, sizes, vptr_field_name(view, shapes)):
                lines.append(f"assert_eq!(offset_of!({name}, {member.name}), {member.offset});")
        for spec in ir_by_group[group].get("specs") or []:
            name = spec["rust_name"]
            view = abi_view(spec, abi)
            lines.append(f"// {group}: {spec['name']} (flattened, clang's layout)")
            lines.append(f"assert_eq!(size_of::<{name}>(), {view['size']});")
            lines.append(f"assert_eq!(align_of::<{name}>(), {view['align']});")
            for member in record_members(view, sizes):
                lines.append(f"assert_eq!(offset_of!({name}, {member.name}), {member.offset});")
    lines.append("}")
    lines.append("")

    lines += [
        "/// Every vtable struct this ABI was emitted from: its size is the ABI",
        "/// rule's (the primary base's slots plus this class's own), every field",
        "/// sits at its slot, and every `X_slots` const is what `abi::slot`",
        "/// computes from the generated declaration-order `VClass`.",
        "#[test]",
        "fn vtables_match_the_abi_rules() {",
    ]
    for group in groups:
        for vtable in ir_by_group[group]["vtables"]:
            class_name = vtable["class"]
            shape = shapes[class_name]
            lines.append(f"// {group}: {vtable['header']}")
            lines.append(
                f"assert_eq!(size_of::<{class_name}_vtable>(), "
                f"vtable_len(Abi::native(), &{class_name}_VCLASS) * 8);"
            )
            for item in vtable_slot_items(shape, abi):
                lines.append(
                    f"assert_eq!(offset_of!({class_name}_vtable, {item.name}), {item.offset});"
                )
            for const, slot, key in shape["slot_checks"][abi]:
                lines.append(f"assert_eq!({class_name}_slots::{const}, {slot});")
                lines.append(
                    f"assert_eq!(slot(Abi::native(), &{class_name}_VCLASS, "
                    f"{rust_str(key)}), Some({slot}));"
                )
    lines.append("}")
    lines.append("")

    # The same triangle once more, at compile time. A `const` item is
    # const-evaluated by rustc even when the crate is only checked, so this
    # proves the emitted vtable structs against the IR's slots on a target whose
    # tests cannot run here - the MSVC half on a Linux host, which is exactly
    # what `cargo check --target x86_64-pc-windows-msvc` is the acceptance for.
    # The size is the emitted member list's (the ABI rule's `vtable_len` is
    # asserted against it at runtime, on the target the half belongs to; the
    # size is a literal here because `vtable_len` is a plain `fn` and the slot
    # rule needs loops). (A `//` note, not `///`: `const _` may take no doc.)
    lines += [
        "// The emitted vtable structs against the IR's slots, at compile time:",
        "// `cargo check` evaluates a `const` item, so the half whose tests cannot",
        "// run here (the MSVC one on a Linux host) still proves its size and every",
        "// member offset instead of merely compiling.",
        "const _: () = {",
    ]
    for group in groups:
        for vtable in ir_by_group[group]["vtables"]:
            class_name = vtable["class"]
            shape = shapes[class_name]
            items = vtable_slot_items(shape, abi)
            total = items[-1].offset + items[-1].size if items else 0
            lines.append(
                f"assert!(size_of::<{class_name}_vtable>() == {total});"
            )
            for item in items:
                lines.append(
                    f"assert!(offset_of!({class_name}_vtable, {item.name}) == {item.offset});"
                )
    lines.append("};")
    lines.append("")

    if abi == ABI_MSVC:
        # On Windows `vtables_match_the_abi_rules` is the native half, but the
        # number clang printed for the class's vftable is the one the bridge's
        # vtable-length pins assume; assert it there too.
        lengths = [
            (group, vtable, (vtable.get(ABI_MSVC) or {}).get("length"))
            for group in groups
            for vtable in ir_by_group[group]["vtables"]
        ]
        if any(length is not None for _, _, length in lengths):
            lines += [
                "/// clang's own vftable length for the class is the MSVC ABI rule's",
                "/// `vtable_len` (the full `VFTable` minus its RTTI entry).",
                "#[test]",
                "fn msvc_vtable_lengths_match_clangs_dump() {",
            ]
            for group, vtable, length in lengths:
                if length is None:
                    continue
                class_name = vtable["class"]
                lines.append(f"// {group}: {vtable['header']}")
                lines.append(
                    f"assert_eq!(vtable_len(Abi::Msvc, &{class_name}_VCLASS), {length});"
                )
            lines.append("}")
            lines.append("")

    if abi == ABI_DEFAULT:
        lines += [
            "/// `msvc_slot` is clang's answer (the MSVC vftable pass) and `abi::slot`",
            "/// is the ABI rule: `abi.rs` has to reproduce every one of them, including",
            "/// the overload reversal and the one-slot destructor. `vtable_len(Msvc, ..)`",
            "/// is checked against the length clang printed for the class's vftable",
            "/// (the full VFTable minus its RTTI entry) - the number the bridge's",
            "/// vtable-length pins assume. This is the Linux-side half of the MSVC",
            "/// triangle; the Windows half is the `msvc` module's own test.",
            "#[test]",
            "fn msvc_slots_match_the_abi_rules() {",
        ]
        for group in groups:
            for vtable in ir_by_group[group]["vtables"]:
                class_name = vtable["class"]
                shape = shapes[class_name]
                lines.append(f"// {group}: {vtable['header']}")
                length = (vtable.get(ABI_MSVC) or {}).get("length")
                if length is not None:
                    lines.append(
                        f"assert_eq!(vtable_len(Abi::Msvc, &{class_name}_VCLASS), {length});"
                    )
                for entry in shape["entries"]:
                    lines.append(
                        f"assert_eq!(slot(Abi::Msvc, &{class_name}_VCLASS, "
                        f"{rust_str(entry.key)}), Some({entry.slots[ABI_MSVC]}));"
                    )
        lines.append("}")
        lines.append("")

    lines += checks_pinned_vtable_test(checks, ir_by_group, shapes, abi)
    lines.append("}")
    return lines


def checks_pinned_vtable_test(
    checks: Mapping[str, dict] | None,
    ir_by_group: dict,
    shapes: dict[str, dict],
    abi: str,
) -> list[str]:
    """The vtable facts `[checks]` pins, as compile-time-shaped assertions.

    `[checks]` is where a number is stated against a hand-written expectation (so
    an upstream bump that moves a slot fails the run); the emitted tests then
    assert the same numbers against the compiled structs. This is the same
    triangle for the two facts a length *relation* carries:

    * a `<group>.<vtable>` key pinning `length` and `msvc_length` asserts both
      numbers (`vtable_len`), and - when they differ - the relation between them
      (the Itanium destructor's two entries against MSVC's one), which is a
      statement about the ABI rule that no single-ABI assertion makes;
    * a `<group>.<vtable>.<method>` key with `last_slot` asserts that the method's
      slot is the vtable's last entry, i.e. `slot + 1 == vtable_len` (see
      `_check_last_slot`).

    Only the Itanium module can assert the MSVC half (it is the module that runs
    on this host); the MSVC module asserts its own `native()` (`= Msvc`) numbers
    and no Itanium-only pin. A Windows-only group never appears in the Itanium
    module, exactly like the other tests in it."""
    if not checks:
        return []
    lines: list[str] = []
    for key, expected in checks.items():
        parts = key.split(".")
        if len(parts) not in (2, 3):
            continue
        group, class_name = parts[0], parts[1]
        ir = ir_by_group.get(group)
        if ir is None:
            continue  # an unknown group is `run_checks`' failure, not the emitter's
        windows_only = ir_windows_only(ir)
        if abi == ABI_DEFAULT and windows_only:
            continue
        vtable = find_vtable(ir, class_name)
        if vtable is None:
            continue  # a record/enum/spec key: asserted by the record block
        shape = shapes[class_name]
        if len(parts) == 2:
            itanium = expected.get("length")
            msvc = expected.get("msvc_length")
            if itanium is not None and abi == ABI_DEFAULT:
                lines.append(
                    f"// [checks] {key}: length\n"
                    f"assert_eq!(vtable_len(Abi::native(), &{class_name}_VCLASS), {itanium});"
                )
            if msvc is not None:
                native = "Abi::Msvc" if abi == ABI_DEFAULT else "Abi::native()"
                lines.append(
                    f"// [checks] {key}: msvc_length\n"
                    f"assert_eq!(vtable_len({native}, &{class_name}_VCLASS), {msvc});"
                )
            if itanium is not None and msvc is not None and itanium != msvc and abi == ABI_DEFAULT:
                sign = "+" if itanium > msvc else "-"
                lines.append(
                    f"// [checks] {key}: the two ABI lengths' relation\n"
                    f"assert_eq!(\n"
                    f"    vtable_len(Abi::native(), &{class_name}_VCLASS),\n"
                    f"    vtable_len(Abi::Msvc, &{class_name}_VCLASS) {sign} {abs(itanium - msvc)}\n"
                    f");"
                )
            continue
        if not expected.get(LAST_SLOT_FIELD):
            continue
        const = next(
            (
                slot_const
                for slot_const, _slot, slot_key in shape["slot_checks"][abi]
                if slot_key == parts[2]
            ),
            None,
        )
        if const is None:
            raise GenError(
                f"[checks] {key}: the emitted {class_name} vtable has no slot named "
                f"{parts[2]!r} for the {abi} ABI, so `{LAST_SLOT_FIELD}` cannot be "
                f"asserted"
            )
        native = "Abi::native()"
        lines.append(
            f"// [checks] {key}: last_slot (slot + 1 is the {abi} vtable's length)\n"
            f"assert_eq!({class_name}_slots::{const} + 1, vtable_len({native}, &{class_name}_VCLASS));"
        )
        if abi == ABI_DEFAULT:
            lines.append(
                f"assert_eq!({class_name}_slots::{const} + 1, vtable_len(Abi::Msvc, &{class_name}_VCLASS));"
            )
    if not lines:
        return []
    return [
        "/// The `[checks]` entries that pin a vtable fact the record/vtable tests",
        "/// above cannot state on their own: both ABI lengths of a class (and their",
        "/// relation) and a method that is the last entry of its vtable.",
        "#[test]",
        "fn checks_pinned_vtable_facts() {",
        *lines,
        "}",
        "",
    ]


def rustfmt_source(name: str, text: str) -> str:
    """Format an emitted file with the pinned rustfmt options, so what gen.py
    writes is byte-for-byte what `cargo fmt --check` expects."""
    path = SCRATCH / "rustfmt" / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    run([*RUSTFMT, str(path)])
    return path.read_text(encoding="utf-8")


def emit_all(ir_by_group: dict, commit: str, checks: Mapping[str, dict] | None = None) -> dict[str, str]:
    """Every generated file, keyed by its path relative to s2sdk-sys/."""
    shapes = vtable_shapes(ir_by_group)
    sizes_by_abi = {abi: abi_sizes(ir_by_group, abi) for abi in ABIS}
    all_records = {
        record["name"]: record
        for group in GROUPS
        if group != CONST_GROUP
        for record in ir_by_group[group]["records"]
    }
    files = {
        f"src/generated/{group}.rs": emit_group_rs(
            group, ir_by_group[group], commit, shapes, sizes_by_abi, all_records
        )
        for group in GROUPS
    }
    files["src/generated/mod.rs"] = emit_mod_rs(commit, windows_only_groups(ir_by_group))
    files["tests/layout.rs"] = emit_layout_tests(
        ir_by_group, commit, shapes, sizes_by_abi, checks
    )
    return {name: rustfmt_source(name.replace("/", "_"), text) for name, text in files.items()}


def write_rust(files: dict[str, str], target: Path) -> None:
    for name, text in files.items():
        path = target / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")


def check_rust(files: dict[str, str]) -> int:
    import difflib

    failures = 0
    for name, wanted in files.items():
        path = S2SDK_SYS_DIR / name
        if not path.is_file():
            log(f"check: s2sdk-sys/{name} is missing")
            failures += 1
            continue
        current = path.read_text(encoding="utf-8")
        if current == wanted:
            continue
        failures += 1
        log(f"check: s2sdk-sys/{name} differs from the pinned upstream")
        diff = difflib.unified_diff(
            current.splitlines(),
            wanted.splitlines(),
            fromfile=f"{name} (committed)",
            tofile=f"{name} (regenerated)",
            lineterm="",
            n=2,
        )
        for line in list(diff)[:40]:
            log(line)
    return failures


# --------------------------------------------------------------------------- #
# the generated C header, s2sdk_gen.h (WP C1; design 2.3, row C1)
# --------------------------------------------------------------------------- #

C_HEADER_PATH = REPO_ROOT / "crates" / "s2sdk" / "s2sdk-sys" / "include" / "s2sdk_gen.h"

# The records the C bridge read member-by-member at pinned offsets (design
# 1.1/1.2/1.3 rows marked "(a)" and "(c)"): these are padded structs in
# crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h, with a _Static_assert on every offset and size.
# Everything else in the IR lands as S2_SIZEOF_/S2_ALIGNOF_/S2_OFF_ macros only. The list
# stays as the C bridge left it: the Rust bridge reads s2sdk_sys, and the header's macros
# are what sigs' `sdk` expressions evaluate against.
C_EMIT_STRUCTS = (
    "Vector",
    "QAngle",
    "CUtlSymbolLarge",
    "CUtlString",
    "SchemaClassInfoData_t",
    "SchemaClassFieldData_t",
    "SchemaBaseClassInfoData_t",
    "CEntityIdentity",
    "RenderViewport_t",
    "UndoGroup_t",
    # Game-internal object layouts (WP6): whole-object structs with the base-class fields
    # flattened in at their offsets (c_flat_fields). Order matters where one is embedded by value
    # in the next (CEditScopeStack -> CMapDoc -> CMapDocSession).
    "CBaseEditorApp",
    "CBaseEditorSharedWindowApp",
    "CBaseEditorSession",
    "CUndoHistory",
    "CHammerEditorSession",
    "CEditScopeStack",
    "CMapDoc",
    "CMapDocSession",
    "CMapRootElement",
    "CMapDocHistory",
    "CMapNode",
    "CMapEntityIONode",
    "CMapView",
    "CMapViewMgr",
    "CPetDoc",
    "CPetSession",
    "CModelDocEditorApp",
)
C_EMIT_SPECS = ("CUtlVector<CEntityHandle>",)

# Scalar spellings: the u8/u16/u32/u64 typedefs (C_SMOKE_TYPEDEFS; freestanding C
# has no stdint.h reachable - design R7) plus the C builtins whose MSVC x64 widths the
# member-size asserts verify. A rust_type outside this table becomes a byte
# blob, never a guess.
_C_SCALARS = {
    "u8": "u8",
    "u16": "u16",
    "u32": "u32",
    "u64": "u64",
    "i8": "signed char",
    "i16": "short",
    "i32": "int",
    "i64": "long long",
    "f32": "float",
    "f64": "double",
    # MSVC bool is one byte, spelled u8 - the member-size assert keeps the
    # spelling honest.
    "bool": "u8",
}

# An `sdk` expression (a tie row's `generated`, in the C bridge's day) is normally one
# emitted macro. A pin that is a vtable BYTE offset (the particle editor's
# `call [rax+0C0h]`) is slot * 8 instead: slot_bytes() spells that as the C expression
# `S2_SLOT_X*8`, split_tie_scale() reads it back, and sigcheck.sdk_eval and sdkcheck
# compare the scaled value.
SLOT_BYTES = 8


def slot_bytes(slot_macro: str) -> str:
    return f"{slot_macro}*{SLOT_BYTES}"


def split_tie_scale(generated: str) -> tuple[str, int]:
    """An `sdk` expression -> (the emitted macro, the factor its value is
    multiplied by to give the pin). Only slot_bytes() spellings carry a factor."""
    macro, star, factor = generated.partition("*")
    if not star:
        return generated, 1
    if not macro.startswith("S2_SLOT_") or factor != str(SLOT_BYTES):
        raise GenError(
            f"sdk expression {generated!r}: only S2_SLOT_<class>_<method>*{SLOT_BYTES} "
            "(slot_bytes()) may carry a factor"
        )
    return macro, SLOT_BYTES


_TIE_ADDEND_RE = re.compile(r"(S2_OFF_\w+)\+(\d+)")
_TIE_SUM_RE = re.compile(r"(S2_OFF_\w+)\+(S2_OFF_\w+)")


def tie_terms(generated: str, values: dict | None = None) -> tuple[str, int, int]:
    """An `sdk` expression -> (the emitted macro, factor, addend): the pin is
    `macro * factor + addend`. Besides the slot_bytes() factor, a member offset may
    carry a `+N` (`S2_OFF_CPetDoc_m_Selection+8`): a pin that points inside a member
    (the data pointer of a vector) is still anchored to the generated offset. It may
    also carry another member's offset (`S2_OFF_A_m_x+S2_OFF_B_m_y`): a pin that
    reaches a member of an embedded object; the addend is then read from `values`
    (the emitted macros), which the caller has to pass."""
    match = _TIE_ADDEND_RE.fullmatch(generated)
    if match:
        return match.group(1), 1, int(match.group(2))
    match = _TIE_SUM_RE.fullmatch(generated)
    if match:
        if values is None or match.group(2) not in values:
            raise GenError(
                f"sdk expression {generated!r}: {match.group(2)} is not emitted; is it still in the IR?"
            )
        return match.group(1), 1, int(values[match.group(2)])
    macro, scale = split_tie_scale(generated)
    return macro, scale, 0


# The `sdk` field's grammar (sigs/signatures/SCHEMA.json, sigcheck.sdk_eval): a
# sigs entry names a generated expression instead of carrying a number, and
# the pins generator and sdkcheck.py hold it to the binary measurement. The
# spellings are slot_bytes / split_tie_scale / tie_terms above: a plain macro,
# S2_SLOT_X*8, S2_OFF_X+N and S2_OFF_A+S2_OFF_B. They were the C bridge's tie
# rows once; the tie tables and the late-ties header went with the C bridge.

# The u8/u16/u32/u64 typedefs the header is written against (_C_SCALARS): it
# carries no #include (freestanding C has no stdint.h reachable - design R7), so
# the smoke compile puts these MSVC x64 widths in front of it.
C_SMOKE_TYPEDEFS = (
    "typedef unsigned char u8;",
    "typedef unsigned short u16;",
    "typedef unsigned int u32;",
    "typedef unsigned long long u64;",
)


def c_tag(name: str) -> str:
    """The C spelling of a record/spec: `S2_` + the Rust-safe tag (`Vector` ->
    `S2_Vector`, `CUtlVector<CEntityHandle>` -> `S2_CUtlVector_CEntityHandle`)."""
    return "S2_" + spec_tag(name)


def c_method_name(method: dict) -> str:
    """The C spelling an own virtual is emitted under. A `~` is not a valid
    chunk of a macro name, so destructors share the `dtor` stem. And a
    declaration the Windows headers rename through a charset macro is emitted
    under clang's MSVC spelling (IR `msvc_name`, e.g. IGameEventManager2
    CreateEvent -> CreateEventA): the bare C++ name is itself a Windows macro,
    so the generated spelling must take the renamed one."""
    if method["is_dtor"]:
        return "dtor"
    msvc_name = method.get("msvc_name")
    if msvc_name and msvc_name != method["name"]:
        return msvc_name
    return method["name"]


def c_int_literal(value: int, unsigned: bool) -> str:
    """The #define spelling of a measured int: hex (readable like the bridge's
    literals) past six digits, and a `u` suffix where the entry says unsigned."""
    text = f"0x{value:X}" if value >= 0x100000 else str(value)
    return text + ("u" if unsigned else "")


# An object struct's by-value members that the C header spells as the generic CUtlVector layout:
# every CUtlVector<T> is the same 24 bytes (m_Size, m_pMemory, m_nAllocationCount, m_nGrowSize), so one
# emitted spec stands for all of them (the member-size assert keeps the claim honest).
C_GENERIC_VECTOR = "S2_CUtlVector"


def c_field_type(
    field: dict, emitted: dict[str, str], ctx: dict | None = None
) -> tuple[str, str, str | None]:
    """C type of one field as (declared type, array suffix, note), exact-or-blob:
    the same contract the IR's rust_type fixes for Rust. A pointer to an emitted
    struct keeps the struct type (the `*` ends the type, so the declared name
    follows it); a scalar becomes the C builtin of the same width; anything else
    becomes a `u8[n]` blob. `ctx` (object structs only) adds: fixed-underlying enums
    spelled as their integer, any `CUtlVector<T>` as the generic vector, and a pointer to
    another IR record as `struct <Name> *` (the header forward-typedefs those names)."""
    ctx = ctx or {"enums": {}, "records": {}, "objects": set()}
    rust = field["rust_type"]
    if rust is None:
        if field["cxx_type"].startswith("CUtlVector<") and field["size"] == 24:
            return (
                C_GENERIC_VECTOR,
                "",
                f"`{field['cxx_type']}`: the generic CUtlVector layout, the member-size assert pins it",
            )
        return (
            "u8",
            f"[{field['size']}]",
            f"`{field['cxx_type']}`: no exact C type, the bytes are kept",
        )
    if rust in _C_SCALARS:
        note = (
            f"`{field['cxx_type']}`: bool is one byte under MSVC, spelled u8 "
            "(the member-size assert pins it)" if rust == "bool" else None
        )
        return _C_SCALARS[rust], "", note
    if rust in ctx["enums"] and ctx["enums"][rust] in _C_SCALARS:
        return (
            _C_SCALARS[ctx["enums"][rust]],
            "",
            f"`{field['cxx_type']}`: fixed-underlying enum, spelled as its integer",
        )
    if rust.startswith(("*const ", "*mut ")):
        const = rust.startswith("*const ")
        inner = rust[len("*const " if const else "*mut ") :]
        cv = "const " if const else ""
        if inner == "::core::ffi::c_char":
            return f"{cv}char *", "", None
        if inner == "::core::ffi::c_void":
            return f"{cv}void *", "", None
        tag = emitted.get(inner)
        if tag is not None and inner not in ctx["objects"]:
            return f"{cv}{tag} *", "", None
        if inner in ctx["records"]:
            return (
                f"{cv}struct {inner} *",
                "",
                f"`{inner}` is the bridge's own type of that name (see the forward typedefs)",
            )
        return (
            f"{cv}void *",
            "",
            f"pointee `{field['cxx_type']}` is not an emitted struct",
        )
    if rust in emitted:
        return emitted[rust], "", None
    return (
        "u8",
        f"[{field['size']}]",
        f"`{field['cxx_type']}`: not an emitted struct, the bytes are kept",
    )


def c_flat_fields(
    name: str, item: dict, ctx: dict, base_offset: int = 0
) -> list[tuple[dict, dict, str, int]]:
    """The fields of an object record with its base classes' fields flattened in:
    (typed IR field, msvc field, owning record, absolute offset). A base the IR has no
    record for (a pure interface, an unmodelled engine class) contributes nothing: its bytes
    stay a pad, and so does a secondary base (offset != 0: its subobject, vptr included, is a
    pad; no consumer reads a field out of one - reach it with `S2_BASEOFF_`)."""
    out: list[tuple[dict, dict, str, int]] = []
    for base in item.get("bases", []):
        if base.get("virtual"):
            raise GenError(f"{name}: virtual base {base['name']}; the C emitter spells none")
        record = ctx["records"].get(base["name"])
        if record is None or base["offset"] != 0:
            continue
        out.extend(c_flat_fields(base["name"], record, ctx, base_offset))
    typed = {field["name"]: field for field in item["fields"]}
    for field in item["msvc"]["fields"]:
        out.append((typed[field["name"]], field, name, base_offset + field["offset"]))
    return out


def c_struct_block(
    name: str, item: dict, emitted: dict[str, str], ctx: dict | None = None
) -> list[str]:
    """One emitted struct with its asserts: padded members (`_p<n>` byte blobs
    in the gaps, the way record_members pads the Rust side), sizeof/alignof,
    and one offset/size assert per IR field. `item` is a record or a spec.
    A record with a vtable pointer or bases is a whole-object struct (`ctx` carries the IR):
    its vptr and any secondary-base subobjects are pads, the fields of its bases are flattened
    in, and a consumer overlays the typed vtable pointer itself (the C bridge's s2sdk.h did)."""
    tag = c_tag(name)
    msvc = item["msvc"]
    typed = {field["name"]: field for field in item["fields"]}
    if typed.keys() != {field["name"] for field in msvc["fields"]}:
        raise GenError(
            f"{name}: the MSVC field set is not the IR field set "
            f"({sorted(typed)} vs {sorted(f['name'] for f in msvc['fields'])})"
        )
    is_object = bool(item.get("has_vptr") or item.get("bases"))
    if is_object and ctx is None:
        raise GenError(f"{name}: has a vtable pointer or bases; the C emitter needs the IR context")
    if is_object:
        flat = c_flat_fields(name, item, ctx)
    else:
        flat = [(typed[f["name"]], f, name, f["offset"]) for f in msvc["fields"]]
    for _, field, owner, _ in flat:
        if field["bit_width"] is not None:
            raise GenError(
                f"{name}: field {owner}::{field['name']} is a bitfield; the flat C struct "
                "cannot name it - extend the C emitter instead of guessing"
            )
    seen_names: dict[str, str] = {}
    for _, field, owner, _ in flat:
        if field["name"] in seen_names:
            raise GenError(
                f"{name}: member {field['name']} is in both {seen_names[field['name']]} and {owner}"
            )
        seen_names[field["name"]] = owner
    flat.sort(key=lambda entry: entry[3])

    members: list[tuple[str, str, str, int, str | None]] = []
    cursor = 0
    pads = 0

    def pad_to(offset: int, what: str) -> None:
        nonlocal cursor, pads
        if offset < cursor:
            raise GenError(
                f"{name}: {what} starts at {offset}, inside the previous member "
                f"(which ends at {cursor})"
            )
        if offset > cursor:
            # an object's first pad is its vptr (+ whatever the bases hold): it carries the
            # object's alignment, which a struct of ints alone would not
            align = f"_Alignas({msvc['align']}) " if is_object and cursor == 0 else ""
            members.append((f"_p{pads}", f"{align}u8", f"[{offset - cursor}]", cursor, None))
            pads += 1
        cursor = offset

    for typed_field, field, owner, offset in flat:
        pad_to(offset, f"field {owner}::{field['name']}")
        ctype, arr, note = c_field_type(typed_field, emitted, ctx if is_object else None)
        members.append((field["name"], ctype, arr, offset, note))
        cursor = offset + field["size"]
    pad_to(msvc["size"], "the trailing pad")

    lines = [f"/* {name}: {msvc['size']} B, align {msvc['align']} */"]
    lines.append(f"typedef struct {tag} {{")
    for member_name, ctype, arr, offset, note in members:
        if ctype.endswith("*"):
            decl = f"    {ctype}{member_name}{arr};"
        else:
            decl = f"    {ctype} {member_name}{arr};"
        comment = f"{offset}" if note is None else f"{offset}: {note}"
        comment = f"/* {comment} */"
        padding = max(1, 46 - len(decl) - 1)
        lines.append(decl + " " * padding + comment)
    lines.append(f"}} {tag};")
    lines.append(
        f'_Static_assert(sizeof({tag}) == S2_SIZEOF_{spec_tag(name)}, "{tag} size");'
    )
    lines.append(
        f'_Static_assert(_Alignof({tag}) == S2_ALIGNOF_{spec_tag(name)}, "{tag} align");'
    )
    for _, field, owner, _ in flat:
        field_name = field["name"]
        lines.append(
            f"_Static_assert(__builtin_offsetof({tag}, {field_name})"
            f" == S2_OFF_{spec_tag(owner)}_{field_name},"
            f' "{tag}::{field_name} offset");'
        )
        lines.append(
            f'_Static_assert(sizeof((({tag} *)0)->{field_name}) == {field["size"]},'
            f' "{tag}::{field_name} size");'
        )
    lines.append("")
    return lines


def emit_c_header(
    ir_by_group: dict,
) -> str:
    """crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h: every number the flat IR carries, spelled as
    C macros (MSVC x64 side only - the tool DLLs are Windows-only, design 2.3). The
    C bridge compiled it once; today sigs/scripts/sigcheck.py's sdk_defines/sdk_eval read
    its S2_SLOT_/S2_OFF_/S2_SIZEOF_/S2_VTLEN_/S2_CONST_/S2_IFACE_ macros, so a sigs `sdk`
    expression and sdkcheck.py name exactly what clang measured. Freestanding (design R7,
    no #include at all): it is written against the u8/u16/u32/u64 typedefs
    (C_SMOKE_TYPEDEFS). Committed and regenerated (AGENTS.md rule 1): a header
    `gen.py --check` disagrees with is a stale header."""
    commit = None
    for group in c_header_groups():
        commit = ir_by_group[group]["upstream"].get("commit")
        if commit is not None:
            break
    if commit is None:
        raise GenError("C header: no upstream commit in the IR")

    ir_items: dict[tuple[str, str], dict] = {}
    for group in c_header_groups():
        if group == CONST_GROUP:
            continue
        for record in ir_by_group[group]["records"]:
            ir_items[("records", record["name"])] = record
        for spec in ir_by_group[group]["specs"]:
            ir_items[("specs", spec["name"])] = spec

    # the structs spelled out, and how a Rust-side inner name reaches their tag
    emitted: dict[str, str] = {}
    structs: list[dict] = []
    for struct_name in C_EMIT_STRUCTS:
        record = ir_items.get(("records", struct_name))
        if record is None:
            raise GenError(f"C header: {struct_name} is not a record in the IR")
        emitted[struct_name] = c_tag(struct_name)
        structs.append(record)
    for spec_name in C_EMIT_SPECS:
        spec = ir_items.get(("specs", spec_name))
        if spec is None:
            raise GenError(f"C header: {spec_name} is not a spec in the IR")
        emitted[spec["rust_name"]] = c_tag(spec_name)
        structs.append(spec)
    if len(C_EMIT_STRUCTS) + len(C_EMIT_SPECS) != len(structs):
        raise GenError("C header: a struct list entry and the IR disagree")
    # the specs go first: an object struct embeds the generic CUtlVector by value
    structs = structs[len(C_EMIT_STRUCTS) :] + structs[: len(C_EMIT_STRUCTS)]
    ctx = {
        "records": {name: item for (kind, name), item in ir_items.items() if kind == "records"},
        "enums": {
            enum["name"]: enum["rust_repr"]
            for group in c_header_groups()
            if group != CONST_GROUP
            for enum in ir_by_group[group].get("enums", [])
        },
        "objects": {
            name
            for name in C_EMIT_STRUCTS
            if ir_items[("records", name)].get("has_vptr") or ir_items[("records", name)].get("bases")
        },
    }

    lines = [
        "/* Generated by crates/s2sdk/s2sdk-sys/gen/gen.py (emit_c_header) - do not edit.",
        " *",
        f" * Source: crates/s2sdk/s2sdk-sys/ir/<group>.json, the MSVC (Windows x64) sub-objects;",
        f" * upstream {commit} (crates/s2sdk/s2sdk-sys/UPSTREAM.lock). Committed but never hand-",
        " * edited (AGENTS.md rule 1): `gen.py --check` - in the AGENTS.md checks list -",
        " * regenerates it and fails when the committed copy differs, so a",
        " * number here is always clang's answer for the pinned headers.",
        " *",
        " * Contents (design 2.3, work package C1):",
        " *   S2_SIZEOF_<T> / S2_ALIGNOF_<T> / S2_OFF_<T>_<field>   every record and spec",
        " *   S2_SLOT_<Class>_<Method>[_<k>]  the MSVC vftable slot of each own virtual",
        " *     (the suffix is the overload index by declaration order; a destructor is",
        " *     `dtor`, one MSVC slot; a Windows-renamed declaration is emitted under",
        " *     its msvc_name, e.g. IGameEventManager2 CreateEvent -> CreateEventA)",
        " *   S2_BASEOFF_<Class>_<Base>       offset of a secondary base subobject (the",
        " *     second vfptr of a class with two bases), for adjusting `this` to <Base>",
        " *   S2_VTLEN_<Class>                vftable length, the RTTI entry at vt[-1] excluded",
        " *   S2_CONST_* / S2_IFACE_*         the measured [consts] values",
        " *   S2_<Type>                       padded structs for the records bridge code reads",
        " *",
        " * Freestanding (design R7): no #include at all - include after u8/u16/u32/u64",
        " * typedefs of the MSVC x64 widths. No #pragma pack either: every",
        " * gap is a spelled-out _p<n> member and every offset is a _Static_assert, so",
        " * the compiler's own layout is proven, never assumed.",
        " */",
        "#ifndef S2SDK_GEN_H",
        "#define S2SDK_GEN_H",
        "",
    ]

    macro_values: dict[str, int | str] = {}

    def add_macro(name: str, value: int | str) -> None:
        if name in macro_values:
            raise GenError(f"C header: {name} is emitted twice")
        macro_values[name] = value

    for group in c_header_groups():
        if group == CONST_GROUP:
            continue
        lines.append(f"/* ---- {group} ---- */")
        for record in ir_by_group[group]["records"]:
            tag = spec_tag(record["name"])
            add_macro(f"S2_SIZEOF_{tag}", record["msvc"]["size"])
            add_macro(f"S2_ALIGNOF_{tag}", record["msvc"]["align"])
            for field in record["msvc"]["fields"]:
                add_macro(f"S2_OFF_{tag}_{field['name']}", field["offset"])
                lines.append(f"#define S2_OFF_{tag}_{field['name']} {field['offset']}")
            lines.append(f"#define S2_ALIGNOF_{tag} {record['msvc']['align']}")
            lines.append(f"#define S2_SIZEOF_{tag} {record['msvc']['size']}")
        for spec in ir_by_group[group]["specs"]:
            tag = spec_tag(spec["name"])
            add_macro(f"S2_SIZEOF_{tag}", spec["msvc"]["size"])
            add_macro(f"S2_ALIGNOF_{tag}", spec["msvc"]["align"])
            for field in spec["msvc"]["fields"]:
                add_macro(f"S2_OFF_{tag}_{field['name']}", field["offset"])
                lines.append(f"#define S2_OFF_{tag}_{field['name']} {field['offset']}")
            lines.append(f"#define S2_ALIGNOF_{tag} {spec['msvc']['align']}")
            lines.append(f"#define S2_SIZEOF_{tag} {spec['msvc']['size']}")
        for vtable in ir_by_group[group]["vtables"]:
            class_name = vtable["class"]
            counts: dict[str, int] = {}
            for method in vtable["methods"]:
                stem = c_method_name(method)
                count = counts.get(stem, 0)
                counts[stem] = count + 1
                suffix = "" if count == 0 else f"_{count}"
                add_macro(f"S2_SLOT_{class_name}_{stem}{suffix}", method["msvc_slot"])
                lines.append(
                    f"#define S2_SLOT_{class_name}_{stem}{suffix} {method['msvc_slot']}"
                )
            for base in vtable["secondary_bases"]:
                macro = f"S2_BASEOFF_{class_name}_{base['name']}"
                add_macro(macro, base["msvc_offset"])
                lines.append(f"#define {macro} {base['msvc_offset']}")
            length = vtable.get("msvc", {}).get("length")
            if length is None:
                lines.append(
                    f"/* S2_VTLEN_{class_name}: clang emitted no vftable for the class */"
                )
            else:
                add_macro(f"S2_VTLEN_{class_name}", length)
                lines.append(f"#define S2_VTLEN_{class_name} {length}")

    lines.append("")
    lines.append(
        "/* ---- the structs bridge code reads member-wise (design 2.3; the ----\n"
        "   ---- tags are pre-declared so pointer members can refer across) ---- */"
    )
    for struct in structs:
        tag = c_tag(struct["name"])
        lines.append(f"typedef struct {tag} {tag};")
    lines.append("")
    lines.append("")
    lines.append(
        "/* the bridge's own spellings: a pointer member to another game class names that class "
        "(an opaque forward typedef; a repeated typedef is fine in C11) */"
    )
    pointees = sorted(
        {
            field["rust_type"].split(" ", 1)[1]
            for struct in structs
            if struct["name"] in ctx["objects"]
            for field in struct["fields"]
            if (field["rust_type"] or "").startswith(("*const ", "*mut "))
            and field["rust_type"].split(" ", 1)[1] in ctx["records"]
            and not (
                field["rust_type"].split(" ", 1)[1] in emitted
                and field["rust_type"].split(" ", 1)[1] not in ctx["objects"]
            )
        }
    )
    for pointee in pointees:
        lines.append(f"typedef struct {pointee} {pointee};")
    lines.append("")
    for struct in structs:
        lines.extend(c_struct_block(struct["name"], struct, emitted, ctx))
        if struct["name"] == "CUtlVector<CEntityHandle>":
            lines.append(
                f"typedef {c_tag(struct['name'])} {C_GENERIC_VECTOR};   /* every CUtlVector<T> has this layout */"
            )
            lines.append("")

    consts = c_header_consts(ir_by_group)
    lines.append("")
    lines.append("/* ---- the measured [consts] (gen/README.md, 'the consts kind') ---- */")
    for const in consts:
        value = const["msvc"]["value"]
        if const["kind"] == "string":
            if const["name"].endswith("_INTERFACE_VERSION"):
                macro = "S2_IFACE_" + const["name"][: -len("_INTERFACE_VERSION")]
            else:
                macro = "S2_CONST_" + const["name"]
            add_macro(macro, value)
            lines.append(f'#define {macro} "{value}"')
        else:
            unsigned = const["type"] in ("u32", "u64")
            macro = f"S2_CONST_{const['name']}"
            literal = c_int_literal(value, unsigned)
            add_macro(macro, literal)
            lines.append(f"#define {macro} {literal}")

    lines.append("")
    lines.append("#endif /* S2SDK_GEN_H */")
    return "\n".join(lines) + "\n"


def write_c_header(text: str) -> None:
    C_HEADER_PATH.write_text(text, encoding="utf-8")


def check_generated_header(path: Path, text: str) -> int:
    import difflib

    if not path.is_file():
        log(f"check: {path.relative_to(REPO_ROOT)} is missing")
        return 1
    current = path.read_text(encoding="utf-8")
    if current == text:
        return 0
    log(f"check: {path.relative_to(REPO_ROOT)} differs from the pinned upstream")
    diff = difflib.unified_diff(
        current.splitlines(),
        text.splitlines(),
        fromfile=f"{path.relative_to(REPO_ROOT)} (committed)",
        tofile=f"{path.relative_to(REPO_ROOT)} (regenerated)",
        lineterm="",
        n=2,
    )
    for line in list(diff)[:40]:
        log(line)
    return 1


def check_c_header(text: str) -> int:
    return check_generated_header(C_HEADER_PATH, text)


def smoke_compile_c_header(text: str, scratch: Path) -> None:
    """The header alone, with C_SMOKE_TYPEDEFS prepended, compiled freestanding on
    the MSVC target with -fsyntax-only (design R7: it must compile with nothing
    else in front of it). Every padded struct's _Static_asserts run here, so
    `--check` proves the C spelling of each layout, not only the macro text.
    No C consumer compiles the header any more (the Rust bridge reads s2sdk_sys);
    sigs/scripts/sigcheck.py's sdk_eval parses its #defines, and this compile is
    what keeps those #defines a well-formed C header."""
    scratch.mkdir(parents=True, exist_ok=True)
    cc = os.environ.get("CC", "clang")
    tu = scratch / "s2sdk_gen_smoke.c"
    tu.write_text("\n".join([*C_SMOKE_TYPEDEFS, "", text]), encoding="utf-8")
    run(
        [
            cc,
            "--target=x86_64-pc-windows-msvc",
            "-ffreestanding",
            "-fsyntax-only",
            # add_macro refuses a name emitted twice at generation time; a
            # redefinition that slipped past it must not pass as a warning.
            "-Werror=macro-redefined",
            str(tu),
        ]
    )
    vlog(f"C header smoke compile ok: {tu}")


# --------------------------------------------------------------------------- #
# pipeline
# --------------------------------------------------------------------------- #


@dataclasses.dataclass
class RunResult:
    ir_by_group: dict
    protos: list[str]


def generate(
    sdk: Path,
    metamod: Path,
    allowlist: dict,
    scratch: Path,
    cfg: CmakeFlags,
    attach: Callable[[dict], None] | None = None,
) -> RunResult:
    """The Itanium (Linux x86_64) pipeline. `attach` is handed the assembled
    IR, the TypeMapper and the parsed record layouts before the acceptance
    checks run, so the dual-ABI run can merge the MSVC numbers in, attach the
    methods' return transports, and every `[checks]` key sees both ABIs."""
    validate_allowlist(allowlist)
    consts = parse_consts(allowlist[CONST_GROUP])
    lock_commit = read_lock()["commit"]
    scratch.mkdir(parents=True, exist_ok=True)

    protos = protoc_step(cfg, sdk, scratch)
    cfg = with_pb_include(cfg, scratch)

    vlog("clang pass 1/4: JSON AST")
    ast_json = scratch / "ast.json"
    clang(cfg, ["-fsyntax-only", "-Xclang", "-ast-dump=json"], PROBE_CPP, ast_json)

    vlog("clang pass 2/4: record layouts")
    layouts_text = record_layout_dump(cfg)

    vlog("clang pass 3/4: vtable layouts")
    vtable_text = clang(
        cfg,
        [
            "-S",
            "-o",
            str(scratch / "probe.s"),
            "-Xclang",
            "-fdump-vtable-layouts",
        ],
        PROBE_CPP,
    )

    vlog("parsing the AST")
    sink = AstSink()
    ast_walk(json.loads(ast_json.read_text(encoding="utf-8")), sink)
    layouts = parse_record_layouts(layouts_text)
    vtables = parse_vtables(vtable_text)
    vlog(
        f"parsed {len(sink.records)} record and {len(sink.enums)} enum "
        f"declarations, {len(layouts)} record layouts, {len(vtables)} vtables"
    )

    builder = Builder(sdk, metamod, allowlist, layouts, vtables, sink, cfg.includes)

    known = set()
    for group in GROUPS:
        for key in ("records", "opaque", "enums", "vtables"):
            known |= set(builder.group_names(group, key))
    builder.known = known

    check_probe_against_allowlist(allowlist, sink)

    vlog("clang pass 4/4: field sizes, enum sizes, enumerator values and consts")
    probes = collect_probes(builder, consts)
    probe_tu = scratch / "probe_values.cpp"
    probe_tu.write_text(gen_probe_tu(probes, strings=const_strings(consts)), encoding="utf-8")
    values_text = clang(
        cfg,
        [
            "-fsyntax-only",
            "-Xclang",
            "-ast-dump=json",
            "-Xclang",
            "-ast-dump-filter=s2sdk_",
        ],
        probe_tu,
    )
    template_names = PROBE_TEMPLATE_NAMES
    value_objects = parse_ast_json_stream(values_text)
    values = parse_probe_values(value_objects, template_names)
    string_values = parse_probe_strings(value_objects)
    apply_probe_values(builder, probes, values)
    vlog(f"probed {len(probes)} sizes/values and {len(string_values)} string const(s)")

    builder.build_type_mapper()

    ir_by_group = {}
    for group in GROUPS:
        if group == CONST_GROUP:
            ir_by_group[group] = {
                "upstream": {"commit": lock_commit, "abi": ABI},
                "records": [],
                "enums": [],
                "vtables": [],
                "consts": build_consts(
                    consts,
                    measure_consts(
                        consts, builder.const_values, string_values, builder.layouts_by_name
                    ),
                    measure_derived_paths(consts, builder.layouts_by_name),
                ),
            }
            continue
        records = [
            builder.build_record(name, group, opaque=False)
            for name in builder.group_names(group, "records")
        ]
        records += [
            builder.build_record(name, group, opaque=True)
            for name in builder.group_names(group, "opaque")
        ]
        enums = [builder.build_enum(name, group) for name in builder.group_names(group, "enums")]
        vtable_classes = [builder.build_vtable(name, group) for name in builder.group_names(group, "vtables")]
        for vtable in vtable_classes:
            for method in vtable["methods"]:
                decl = next(
                    c
                    for c in sink.records[vtable["class"]][0].get("inner", [])
                    if c.get("kind") in ("CXXMethodDecl", "CXXDestructorDecl")
                    and not c.get("isImplicit")
                    and c.get("virtual")
                    and c.get("name") == method["name"]
                    and strip_method_name(c.get("type", {}).get("qualType", ""))
                    == strip_method_name(method["cxx_signature"])
                )
                method["rust_fn"] = build_rust_fn(vtable["class"], decl, builder.types)
                method.pop("msvc_rust_fn", None)
                attach_msvc_rust_fn(method, vtable["class"], decl, builder.types)
        ir_by_group[group] = {
            "upstream": (
                metamod_upstream_header() if group == METAMOD_GROUP else ir_upstream_header(lock_commit)
            ),
            "records": records,
            "enums": enums,
            "vtables": vtable_classes,
            "specs": [builder.build_spec(name, group) for name in builder.spec_names(group)],
        }
        if group_platforms(allowlist, group) == PLATFORMS_WINDOWS_ONLY:
            ir_by_group[group]["platforms"] = list(PLATFORMS_WINDOWS_ONLY)

    if attach is not None:
        attach(ir_by_group, builder.types, builder.layouts_by_name)
        check_dual_abi(ir_by_group)
        check_by_value_signatures(ir_by_group)
    check_platform_references(ir_by_group)
    run_checks(allowlist, ir_by_group)
    if VERIFY_CXX:
        verify_ir_cxx(ir_by_group, cfg, scratch)
    vlog(f"verified {len(builder.header_checks)} declaration headers against their source lines")
    return RunResult(ir_by_group=ir_by_group, protos=protos)


def generate_dual(
    sdk: Path,
    metamod: Path,
    allowlist: dict,
    scratch: Path,
    itanium_cfg: CmakeFlags,
    msvc_cfg: CmakeFlags,
) -> RunResult:
    """Both ABIs in one run: the Itanium pipeline builds the IR, the MSVC pass
    measures the Windows x64 side of the same probe, and the numbers are merged
    into one IR file per group - the only form the committed ir/ has."""
    commit = read_lock()["commit"]
    # generate() runs protoc_step into scratch/pb before it calls attach.
    msvc_cfg = with_pb_include(msvc_cfg, scratch)

    def attach(ir_by_group: dict, types: TypeMapper, layouts) -> None:
        facts = gather_msvc_facts(
            metamod, sdk, allowlist, ir_by_group, msvc_cfg, scratch / "msvc", commit
        )
        write_msvc_facts(metamod, facts, commit, msvc_cfg, scratch / "msvc")
        merge_msvc(ir_by_group, facts)
        attach_return_transports(ir_by_group, types, layouts)
        log(
            f"msvc: {len(facts.records)} record(s), "
            f"{sum(len(slots) for slots in facts.slots.values())} vftable slot(s), "
            f"{len(facts.enums)} enum size(s) merged into the IR"
        )
        if VERIFY_CXX:
            verify_ir_cxx(ir_by_group, msvc_cfg, scratch / "msvc", ABI_MSVC)

    return generate(sdk, metamod, allowlist, scratch, itanium_cfg, attach=attach)

def write_ir(ir_by_group: dict, target: Path) -> None:
    target.mkdir(parents=True, exist_ok=True)
    for group in GROUPS:
        (target / f"{group}.json").write_text(dump_ir(ir_by_group[group]), encoding="utf-8")


def check_ir(ir_by_group: dict) -> int:
    import difflib

    failures = 0
    for group in GROUPS:
        wanted = dump_ir(ir_by_group[group])
        path = IR_DIR / f"{group}.json"
        if not path.is_file():
            log(f"check: {path.relative_to(REPO_ROOT)} is missing")
            failures += 1
            continue
        current = path.read_text(encoding="utf-8")
        if current != wanted:
            failures += 1
            log(f"check: {path.relative_to(REPO_ROOT)} differs from the pinned upstream")
            diff = difflib.unified_diff(
                current.splitlines(),
                wanted.splitlines(),
                fromfile=f"ir/{group}.json (committed)",
                tofile=f"ir/{group}.json (regenerated)",
                lineterm="",
                n=2,
            )
            for line in list(diff)[:40]:
                log(line)
    return failures


# --------------------------------------------------------------------------- #
# the MSVC (Windows x64) record-layout pass
# --------------------------------------------------------------------------- #

MSVC_DIR = SCRATCH / "msvc"


def path_rewriter(
    metamod: Path, sdk: Path, cfg: CmakeFlags
) -> Callable[[str | None], str | None]:
    """Rewrite the absolute paths clang prints into checkout-independent ones.

    The dump spells an anonymous/nested record with the header it sits in and
    the absolute path clang opened it through:

        union CEntityHandle::(anonymous at /abs/checkout/.../entityhandle.h:58:2)

    Those differ per checkout (the include dirs are absolute, and the sysroot can
    be a different checkout's), so a file that has to be diffable across machines
    cannot quote them: the SDK becomes `s2sdk/upstream/...`, the second upstream
    `metamod/upstream/...` and the sysroot `target/xwin/...` wherever any of them
    lives, because it is their content that decides the numbers. What identifies
    the toolchain is the `clang` field both files record, not the path."""
    pairs = []
    if cfg.sysroot is not None:
        # The most specific prefixes go first: a sysroot that lives somewhere
        # else in the repository still has to come out as target/xwin/...
        root = str(Path(cfg.sysroot).resolve())
        pairs.append((root + os.sep, "target/xwin/"))
        pairs.append((root, "target/xwin"))
    # Both upstreams sit under REPO_ROOT; their own prefixes come first so a
    # second checkout (S2SDK_METAMOD_DIR, S2SDK_DIR) is rewritten by content too.
    pairs.append((str(metamod.resolve()) + os.sep, "metamod/upstream/"))
    pairs.append((str(metamod.resolve()), "metamod/upstream"))
    pairs.append((str(sdk.resolve()) + os.sep, "s2sdk/upstream/"))
    pairs.append((str(REPO_ROOT) + os.sep, ""))

    def rewrite(text: str | None) -> str | None:
        if text is None:
            return None
        for prefix, replacement in pairs:
            text = text.replace(prefix, replacement)
        return text

    return rewrite


def layout_to_json(layout: RecordLayout, rewrite: Callable[[str | None], str | None]) -> dict:
    """One record's layout as clang printed it, in two views:

    * `fields` - the direct, named members, which is what the IR's flat field
      list merges an MSVC offset into (keyed by name);
    * `members` - every member clang printed, nested ones included (`level` is
      how deep one sits), which is where a nested record's or a bitfield's real
      offset lives.
    """
    return {
        "name": rewrite(layout.name),
        "kind": layout.kind,
        "size": layout.size,
        "align": layout.align,
        "has_vptr": layout.has_vptr,
        "bases": [
            {
                "name": rewrite(base.name),
                "offset": base.offset,
                "virtual": base.virtual,
                "primary": base.primary,
            }
            for base in layout.bases
        ],
        "fields": [
            {
                "name": member.name,
                "type": rewrite(member.type),
                "offset": member.offset,
                "bit_offset": member.bit_offset,
                "bit_width": member.bit_width,
            }
            for member in layout.own_members
        ],
        "members": [
            {
                "level": member.level,
                "offset": member.offset,
                "bit_offset": member.bit_offset,
                "bit_width": member.bit_width,
                "type": rewrite(member.type),
                "name": member.name,
                "is_vptr": member.is_vptr,
            }
            for member in layout.members
        ],
    }


def msvc_records(
    allowlist: dict, layouts: dict[str, RecordLayout], rewrite: Callable[[str | None], str | None]
) -> dict[str, list[dict]]:
    """{group: [layout, ...]} for the allowlisted records of each group.

    The names are the ones the allowlist (and the Itanium IR) use; clang spells
    a nested record with its elaborated keyword (`class CUtlString`) and a class in
    a namespace with that namespace (`class SourceMM::ISmmPlugin`), so the lookup
    goes through `bare_keyed`, the same normalisation the Builder uses. A name the
    MSVC pass did not lay out is an error, not an omission: probe.cpp has to force
    it (S2SDK_FORCE_LAYOUT) for either ABI to print it."""
    by_name = bare_keyed(layouts, "MSVC record layouts")
    by_group: dict[str, list[dict]] = {}
    for group in GROUPS:
        records = []
        for key in ("records", "opaque"):
            for name in list(allowlist[group].get(key) or []):
                layout = by_name.get(normalize_cxx_type_name(name))
                if layout is None:
                    raise GenError(
                        f"{name}: the MSVC pass laid out no record for it; is "
                        f"S2SDK_FORCE_LAYOUT( {name} ) in gen/probe.cpp?"
                    )
                records.append(layout_to_json(layout, rewrite))
        by_group[group] = records
    return by_group


def clang_version() -> str:
    """The first line of `clang++ --version`: which MS compatibility version
    (`_MSC_VER`) and which STL a layout came from is not in the numbers, and the
    sysroot is not pinned, so the safest a scratch artifact can do is record it."""
    return run(["clang++", "--version"]).splitlines()[0].strip()


def write_msvc_layouts(
    metamod: Path,
    sdk: Path,
    allowlist: dict,
    layouts: dict[str, RecordLayout],
    commit: str,
    cfg: CmakeFlags,
    out_dir: Path = MSVC_DIR,
) -> None:
    """target/s2sdk-gen/msvc/: <group>.json per allowlist group, and
    records.json with every record clang laid out for the probe.

    Neither is committed: the MSVC numbers enter the committed IR only once the
    dual-ABI work package emits ir/<group>.json's "msvc" sub-objects, which is
    also why nothing here may touch ir/ or src/generated (the committed bindings
    stay the Itanium ones). records.json is the whole dump - Windows CRT/SDK
    internals included - for diffing by hand; nothing builds on it, because it
    has no Itanium counterpart and no field names the IR could merge."""
    out_dir.mkdir(parents=True, exist_ok=True)
    rewrite = path_rewriter(metamod, sdk, cfg)
    header = {
        "abi": ABI_MSVC_ABI,
        "commit": commit,
        # Neither the sysroot nor clang's MS compatibility version is pinned, so
        # record both: a diff that is not a header change has to be visible as a
        # toolchain change instead of looking like an upstream bump.
        "clang": clang_version(),
        "sysroot": rewrite(str(cfg.sysroot)) if cfg.sysroot else None,
    }
    by_group = msvc_records(allowlist, layouts, rewrite)
    for group in GROUPS:
        (out_dir / f"{group}.json").write_text(
            dump_ir({**header, "records": by_group[group]}), encoding="utf-8"
        )
    everything = {
        **header,
        "records": [layout_to_json(layouts[name], rewrite) for name in sorted(layouts)],
    }
    (out_dir / "records.json").write_text(dump_ir(everything), encoding="utf-8")


def run_msvc_pass(
    metamod: Path, allowlist: dict, commit: str, cfg: CmakeFlags, ir_by_group: dict
) -> None:
    """`--abi msvc` on its own: measure the Windows x64 side of the probe and
    write target/s2sdk-gen/msvc/ (record layouts, and facts.json with the vtable
    slots and the per-enum sizes). The committed numbers live in ir/<group>.json,
    which the dual-ABI run (or `--check`) writes; this pass reads the committed IR
    for the declaration order, so run it after a full run when the headers moved."""
    validate_allowlist(allowlist)
    facts = gather_msvc_facts(
        metamod, find_sdk_dir(), allowlist, ir_by_group, cfg, MSVC_DIR, commit
    )
    write_msvc_facts(metamod, facts, commit, cfg)
    allowlisted = len(facts.records)
    log(
        f"msvc: {allowlisted} allowlisted record(s), "
        f"{sum(len(slots) for slots in facts.slots.values())} vftable slot(s) and "
        f"{len(facts.enums)} enum size(s); wrote "
        f"{MSVC_DIR.relative_to(REPO_ROOT)}/{{{'|'.join(GROUPS)}}}.json, records.json "
        f"and facts.json"
    )


# --------------------------------------------------------------------------- #
# the MSVC vftable pass: the msvc_slot half of the dual-ABI IR
# --------------------------------------------------------------------------- #

# The Windows headers the MSVC pass sees define `CreateEvent` (and others) as a
# macro that appends the charset suffix, so clang's MSVC view of a method can
# carry a different spelling than the Itanium one. The entry is still the same
# declaration (same signature, same slot), so the match accepts exactly these
# suffixes and nothing else - and records the dump's spelling in `msvc_name`.
MSVC_MACRO_SUFFIXES = ("A", "W")
MSVC_SCALAR_DELETING = "scalar deleting"


@dataclasses.dataclass
class MsvcFacts:
    """What the MSVC passes measured, keyed the way ir/ spells the names: the
    per-record/per-enum sub-objects and the per-method slot of the dual-ABI IR,
    plus the dump summary facts.json keeps for reading by hand."""

    records: dict[str, dict]
    enums: dict[str, dict]
    slots: dict[str, dict[int, int]]
    names: dict[str, dict[int, str]]
    vtables: dict[str, dict]
    specs: dict[str, dict]
    consts: dict[str, int | str]
    # class -> {secondary base name: its offset in the MSVC object}
    bases: dict[str, dict[str, int]]


def msvc_indices_block(blocks: list[VtableBlock], class_name: str) -> VtableBlock:
    """The class's own `VFTable indices for '<class>'` block.

    MSVC numbers these entries from the address point (the class's RTTI pointer
    sits at `vt[-1]`, which is why slot 0 here is the first virtual), exactly the
    numbering the IR's slots and `abi::slot` use. Everything the IR cannot
    express is refused here instead of being read as a slot: a second vfptr (a
    secondary base), a vbtable (a virtual base) and a `[this adjustment: ...]`
    thunk."""
    found = [
        block
        for block in blocks
        if block.kind == "indices" and same_class(block.name, class_name) and block.in_ is None
    ]
    if len(found) != 1:
        raise GenError(
            f"{class_name}: clang printed {len(found)} 'VFTable indices' blocks for "
            f"it; expected exactly one"
        )
    block = found[0]
    # A class whose implicit (or own) destructor overrides the destructor of a secondary base
    # prints that destructor under the secondary vfptr, behind a this-adjusting thunk: not a
    # slot of the primary table, so it is fine. Any other entry there is a method overriding a
    # secondary base's virtual, which the IR cannot express.
    secondary_ok = all(
        section == "accessible via vfptr at offset 0"
        or _VTABLE_SECONDARY_VFPTR_RE.search(section) is not None
        for section in block.sections
    ) and all(entry.is_dtor for entry in block.secondary_entries)
    if block.sections and not secondary_ok:
        raise GenError(
            f"{class_name}: the MSVC object has more than one vfptr or a vbtable "
            f"({'; '.join(block.sections)}); a method overriding a virtual of a "
            f"secondary base (second vfptr) or a virtual base is out of scope for the "
            f"IR and a slot cannot be derived from this dump"
        )
    for entry in block.entries:
        if any(_THUNK_MARKER in marker for marker in entry.markers):
            raise GenError(
                f"{class_name}: slot {entry.index} is a this-adjusting thunk "
                f"({entry.text!r}); thunks only appear on secondary or virtual "
                f"bases, which the IR does not model"
            )
    return block


def msvc_full_block(
    blocks: list[VtableBlock],
    class_name: str,
    primary_base: str | None,
    primary_root: str | None = None,
) -> VtableBlock:
    """The class's full `VFTable for ...` block. clang prints the most-derived
    table under the *primary base's* name, spelled as the chain it sits in -
    `VFTable for 'IAppSystem' in 'ISchemaSystem'`, and one ` in '...'` more per
    level of inheritance in between (`_VTABLE_BLOCK_RE` leaves the chain's last
    name, the most-derived class, in `in_`) - or, when the class has no base,
    under its own; its entry 0 is the class's RTTI pointer, so it has one entry
    more than the vftable. With a secondary base there are several such blocks
    (the secondary base's is `VFTable for 'B' in 'C'`); the primary one is named
    for the root of the primary chain, `primary_root` (the primary base itself
    when it has no base of its own)."""
    primary_root = primary_root or primary_base
    found = [block for block in blocks if block.kind == "layout" and same_class(block.in_, class_name)]
    if not found and primary_base is None:
        found = [
            block
            for block in blocks
            if block.kind == "layout" and same_class(block.name, class_name) and block.in_ is None
        ]
    if len(found) > 1 and primary_base is not None:
        # A secondary base prints its own full table (`VFTable for 'B' in 'C'`) next
        # to the primary one; the class's own virtuals live in the primary base's.
        found = [block for block in found if same_class(block.name, primary_root)]
    if len(found) != 1:
        raise GenError(
            f"{class_name}: clang printed {len(found)} full 'VFTable for' blocks for "
            f"it (primary base {primary_base}); the MS vftable pass needs exactly one"
        )
    for entry in found[0].entries:
        if any(_THUNK_MARKER in marker for marker in entry.markers):
            raise GenError(
                f"{class_name}: slot {entry.index} of the full table is a "
                f"this-adjusting thunk ({entry.text!r}); thunks only appear on "
                f"secondary or virtual bases, which the IR does not model"
            )
    return found[0]


def msvc_method_entries(
    class_name: str, methods: list[dict], block: VtableBlock
) -> dict[int, tuple[VtableEntry, str]]:
    """{decl_index: (own entry, the name the MSVC dump spells it with)}.

    Entries are matched by name, by signature when one name has several
    declarations, and - only for a Windows macro rename (`CreateEvent` ->
    `CreateEventA`) - by the charset suffix. A destructor is matched by name and
    has to be clang's single `[scalar deleting]` slot: MSVC gives a virtual
    destructor one entry, Itanium two."""
    entries = [entry for entry in block.entries if same_class(entry.owner, class_name)]
    by_name: dict[str, list[VtableEntry]] = {}
    for entry in entries:
        if not entry.name:
            raise GenError(
                f"{class_name}: vftable slot {entry.index} names no function: {entry.text!r}"
            )
        by_name.setdefault(entry.name, []).append(entry)

    matched: dict[int, tuple[VtableEntry, str]] = {}
    for method in methods:
        name = method["name"]
        if method["is_dtor"]:
            candidates = [entry for entry in entries if entry.name == name and entry.is_dtor]
            if len(candidates) != 1:
                raise GenError(
                    f"{class_name}::{name}: clang printed {len(candidates)} MSVC slots "
                    f"for the destructor; expected exactly one"
                )
            if not any(MSVC_SCALAR_DELETING in marker for marker in candidates[0].markers):
                raise GenError(
                    f"{class_name}::{name}: the MSVC destructor slot is "
                    f"{list(candidates[0].markers)}, not a scalar deleting destructor"
                )
        else:
            candidates = list(by_name.get(name, []))
            if not candidates:
                renamed = [
                    entry
                    for entry_name, group in by_name.items()
                    if entry_name.startswith(name)
                    and entry_name[len(name) :] in MSVC_MACRO_SUFFIXES
                    for entry in group
                ]
                if len(renamed) == 1:
                    candidates = renamed
            if len(candidates) > 1:
                wanted = _squash(method["cxx_signature"])
                candidates = [entry for entry in candidates if entry.signature == wanted]
            if len(candidates) != 1:
                raise GenError(
                    f"{class_name}::{name}#{method['decl_index']}: {len(candidates)} MSVC "
                    f"vftable entries match ({[entry.text for entry in candidates]})"
                )
        entry = candidates[0]
        claimed = {matched_entry.index for matched_entry, _ in matched.values()}
        if entry.index in claimed:
            raise GenError(f"{class_name}: vftable slot {entry.index} matched twice")
        matched[method["decl_index"]] = (entry, entry.name or name)

    owned = {entry.index for entry in entries}
    left = owned - {entry.index for entry, _ in matched.values()}
    if left:
        # An implicit virtual destructor: the class declares none (the AST
        # keeps no isImplicit declaration), but clang still gives the class's
        # own [scalar deleting] destructor the base chain's destructor slot.
        # Attribute the slot to that implicit declaration: it must be the only
        # leftover, spelled ~<class>, and a scalar deleting destructor.
        implicit = [
            entry
            for entry in entries
            if entry.index in left
            and entry.name == f"~{class_name}"
            and entry.is_dtor
            and any(MSVC_SCALAR_DELETING in marker for marker in entry.markers)
        ]
        if {entry.index for entry in implicit} != left:
            raise GenError(
                f"{class_name}: MSVC vftable slots {sorted(left)} belong to no "
                f"declaration of the class "
                f"({[entry.text for entry in entries if entry.index in left]})"
            )
        vlog(
            f"{class_name}: MSVC slot {next(iter(left))} is the implicit virtual "
            f"destructor's [scalar deleting] entry; the IR records no declaration "
            f"for it"
        )
    return matched


def msvc_predicted_slots(methods: list[dict], length: int) -> dict[int, int]:
    """{decl_index: predicted msvc slot} for a class's own new-slot declarations,
    from the rule `abi::slot(Abi::Msvc, ..)` implements, using only facts this
    pass has: a class's new slots are the last `len(new)` entries of its vftable
    (`msvc_vtable`'s tail check pins that), every MSVC slot is one entry, and the
    overloads of one name form a group anchored at the name's first declaration,
    in reverse declaration order. Overriding declarations take the base's slot
    and are excluded, exactly like `abi`'s `takes_new_slot`.

    This is the whole MSVC own-layout rule, so checking the dump against it (not
    just a group's internal shape) catches a clang regression before the
    generated Rust test would."""
    new_all = [method for method in methods if method["overrides"] is None]
    first_decl: dict[str, int] = {}
    for method in new_all:
        first_decl.setdefault(method["name"], method["decl_index"])
    begin = length - len(new_all)
    predicted = {}
    for name, anchor_decl in first_decl.items():
        # The group's anchor: the anchor declaration's position counts every
        # new-slot declaration whose *group* starts earlier (groups are laid out
        # one after another in first-declaration order, whatever their sizes).
        anchor = begin + sum(
            1 for method in new_all if first_decl[method["name"]] < anchor_decl
        )
        group = [m["decl_index"] for m in new_all if m["name"] == name]
        for position, decl_index in enumerate(group):
            # Reverse declaration order within the group: the first declaration
            # gets the group's last slot.
            predicted[decl_index] = anchor + len(group) - 1 - position
    return predicted


def check_msvc_overload_groups(
    class_name: str, methods: list[dict], slots: dict[int, int], length: int
) -> None:
    """MSVC packs the overloads of one name into consecutive vtable entries at the
    position of the name's first declaration, in reverse declaration order - the
    rule `abi::slot(Abi::Msvc, ..)` implements. clang's dump has to agree: an
    overload group that is not reversed (or not contiguous) would make every
    generated `msvc_slot` past it a guess.

    A declaration that *overrides* a base virtual sits in the base's slot and is
    not part of the group (abi's `takes_new_slot`); an override plus a new
    overload of the same name is therefore not a two-declaration group clang
    would have had to reverse. And the group check verifies the anchor too:
    every new-slot declaration's slot has to be exactly what the rule predicts
    from the first declaration's position (`msvc_predicted_slots`), not merely
    reversed and contiguous."""
    new_all = [method for method in methods if method["overrides"] is None]
    predicted = msvc_predicted_slots(methods, length)
    for name in dict.fromkeys(
        method["name"] for method in new_all if not method["is_dtor"]
    ):
        decls = [m for m in new_all if m["name"] == name]
        if len(decls) < 2:
            continue
        order = [slots[m["decl_index"]] for m in decls]
        if order != sorted(order, reverse=True) or sorted(order) != list(
            range(min(order), min(order) + len(order))
        ):
            raise GenError(
                f"{class_name}::{name}: clang laid the overloads out at {order} in "
                f"declaration order; the MSVC rule (reverse, contiguous) that "
                f"abi::slot implements does not hold here"
            )
        if order[0] != predicted[decls[0]["decl_index"]]:
            raise GenError(
                f"{class_name}::{name}: clang anchored the overload group at slot "
                f"{order[0]}, but the group's first declaration's position is "
                f"{predicted[decls[0]['decl_index']]}; MSVC packs a reversed group "
                f"at the first declaration of the name, which abi::slot computes"
            )


def primary_chain_root(
    ir_by_group: dict, primary_base: str | None, layouts: dict | None = None
) -> str | None:
    """The root class of the primary-base chain that starts at `primary_base`: clang
    names a class's full MSVC table by it (`VFTable for '<root>' in '<class>'`)."""
    primary_of = {
        vtable["class"]: vtable["primary_base"]
        for group in ir_by_group.values()
        for vtable in group["vtables"]
    }
    root = primary_base
    for _ in range(64):
        parent = primary_of.get(root) if root is not None else None
        if parent is None and layouts is not None and root is not None:
            # a class on the chain that is not an IR vtable (a class without virtuals of its
            # own, or a class template such as CTier3AppSystem<IToolSystem2>): its layout
            # names its first base
            layout = layouts.get(normalize_cxx_type_name(root))
            first = next(
                (b for b in layout.bases if b.offset == 0 and not b.virtual), None
            ) if layout is not None else None
            parent = normalize_cxx_type_name(first.name) if first is not None else None
        if parent is None:
            break
        root = parent
    return root


def msvc_vtable(
    class_name: str,
    methods: list[dict],
    primary_base: str | None,
    blocks: list[VtableBlock],
    primary_root: str | None = None,
) -> tuple[dict[int, int], dict[int, str], dict]:
    """One class's msvc slots plus the dump summary facts.json records.

    A class whose own virtuals clang did not print (a class that only inherits
    its base's vtable, like CSchemaSystemTypeScope) has no entries and no slots:
    its slots are its primary base's, which `abi` derives. clang prints no
    vftable for it either (nothing emits one), so there is no measured length to
    check `vtable_len` against and the summary records `null`."""
    if not methods:
        return {}, {}, {"own_entries": 0, "full_entries": None, "length": None, "slots": {}}
    block = msvc_indices_block(blocks, class_name)
    full = msvc_full_block(blocks, class_name, primary_base, primary_root)
    matched = msvc_method_entries(class_name, methods, block)
    slots = {index: entry.index for index, (entry, _) in matched.items()}
    names = {
        index: spelled
        for index, (_, spelled) in matched.items()
        if spelled != methods_by_index(methods, index)
    }
    check_msvc_overload_groups(class_name, methods, slots, full.count - 1)

    # The full block starts with the class's RTTI pointer, so its N entries cover
    # the N-1 entries of the vftable from the address point. Every own slot has to
    # be inside it, and when the class adds a slot of its own (rather than only
    # overriding base ones) its new virtuals are laid out after every base's, so
    # the last own slot is the last entry of the table.
    length = full.count - 1
    outside = sorted(index for index in slots.values() if not 0 <= index < length)
    if outside:
        raise GenError(
            f"{class_name}: MSVC vftable slots {outside} fall outside the full table "
            f"({full.count} entries = {length} from the address point, RTTI at vt[-1])"
        )
    if any(method["overrides"] is None for method in methods) and max(slots.values()) != length - 1:
        raise GenError(
            f"{class_name}: clang's full VFTable has {full.count} entries ({length} from "
            f"the address point) but the last own slot is {max(slots.values())}; the "
            f"address point was read wrong"
        )

    summary = {
        "own_entries": len(slots),
        "full_entries": full.count,
        "length": length,
        "slots": {str(method_key(methods, index)): slot for index, slot in sorted(slots.items())},
    }
    return slots, names, summary


def methods_by_index(methods: list[dict], index: int) -> str:
    for method in methods:
        if method["decl_index"] == index:
            return method["name"]
    return ""


def method_key(methods: list[dict], index: int) -> str:
    """`Name#k` the way `abi::slot` addresses a method: k counts the declarations
    of that name in this class, in declaration order."""
    name = methods_by_index(methods, index)
    same = [m for m in methods if m["name"] == name]
    if len(same) > 1:
        return f"{name}#{[m['decl_index'] for m in same].index(index)}"
    return name


def check_msvc_layout(name: str, layout: RecordLayout) -> None:
    """A vbtable or a virtual base in an allowlisted class's MSVC layout is an
    error: the flat IR has no way to express a virtual base subobject, and the
    vftable of a class with one needs vbase/vcall offset slots the IR does not
    model (the Itanium pass turns such a record into an opaque blob, which would
    hide the difference on the Windows side)."""
    virtual_bases = [base.name for base in layout.bases if base.virtual]
    vbtable = [member.type for member in layout.members if _VBTABLE_MARKER in member.type]
    if virtual_bases or vbtable:
        what = ", ".join(
            filter(
                None,
                [
                    f"virtual base {virtual_bases}" if virtual_bases else "",
                    f"vbtable {vbtable}" if vbtable else "",
                ],
            )
        )
        raise GenError(
            f"{name}: the MSVC layout has a {what}; virtual bases (and the vbtable/"
            f"vcall slots they need) are out of scope for the IR"
        )


def msvc_enum_values(ir_by_group: dict, cfg: CmakeFlags, scratch: Path) -> dict[str, int]:
    """sizeof(<enum>) as MSVC lays it out: a fixed underlying type is honoured,
    an unfixed one is `int` (the MSVC rule, which the measured size confirms).
    Itanium takes the smallest type that fits, so this can differ per ABI."""
    probes = []
    for group in GROUPS:
        for enum in ir_by_group[group]["enums"]:
            name = enum["name"]
            probes.append(
                Probe(
                    tag=f"s2sdk_es_{len(probes)}",
                    kind="enumsize",
                    expr=f"sizeof({name})",
                    result=("enumsize", name),
                )
            )
    return msvc_probes(probes, cfg, scratch, "enums")


def msvc_field_sizes(ir_by_group: dict, cfg: CmakeFlags, scratch: Path) -> dict[str, int]:
    """sizeof(<record>::<field>) under MSVC, keyed "<record>.<field>". A bitfield
    has no sizeof of its own, so its storage type is measured (as in the Itanium
    pass). A specialization's fields are the leaves the Itanium pass flattened
    into its `fields`, so the same keys are measured for them."""
    probes = []
    for group in GROUPS:
        for record in ir_by_group[group]["records"]:
            if record["opaque"]:
                continue
            for field in record["fields"]:
                key = f"{record['name']}.{field['name']}"
                if field["bit_width"] is not None:
                    probes.append(
                        Probe(
                            tag=f"s2sdk_fs_{len(probes)}",
                            kind="type",
                            expr=f"sizeof({field['cxx_type']})",
                            result=("bitsize", key),
                        )
                    )
                else:
                    probes.append(
                        Probe(
                            tag=f"s2sdk_fs_{len(probes)}",
                            kind="field",
                            expr=f"sizeof((({record['name']}*)0)->{field['name']})",
                            result=("fieldsize", key),
                        )
                    )
        for spec in ir_by_group[group].get("specs") or []:
            for field in spec["fields"]:
                path = tuple(field.get("path") or [field["name"]])
                probes.append(
                    Probe(
                        tag=f"s2sdk_fs_{len(probes)}",
                        kind="field",
                        expr=f"sizeof({member_path_expr(spec['name'], path)})",
                        result=("fieldsize", f"{spec['name']}.{field['name']}"),
                    )
                )
    return msvc_probes(probes, cfg, scratch, "field_sizes")


def msvc_probes(probes: list[Probe], cfg: CmakeFlags, scratch: Path, tag: str) -> dict[str, int]:
    """Run the `S2SDK_` probe TU under the MSVC target and return {key: value}.

    The Itanium pass reaches private members with `#define private public`; the
    MSVC CRT/STL rejects that (`xkeycheck.h` makes it a hard error), so the MSVC
    probe TU takes `-fno-access-control` instead - no macro, same access."""
    tu = scratch / f"probe_msvc_{tag}.cpp"
    tu.write_text(gen_probe_tu(probes, msvc=True), encoding="utf-8")
    out = tu.with_suffix(".json")
    clang(
        cfg,
        [
            "-fno-access-control",
            "-fsyntax-only",
            "-Xclang",
            "-ast-dump=json",
            "-Xclang",
            "-ast-dump-filter=s2sdk_",
        ],
        tu,
        out,
    )
    values = parse_probe_values(
        parse_ast_json_stream(out.read_text(encoding="utf-8")), PROBE_TEMPLATE_NAMES
    )
    result = {}
    for probe in probes:
        if probe.tag not in values:
            raise GenError(f"{probe.result[1]}: clang reported no MSVC value for probe {probe.tag}")
        result[probe.result[1]] = values[probe.tag]
    return result


def msvc_vtable_dump(cfg: CmakeFlags, scratch: Path) -> str:
    """The `-fdump-vtable-layouts` text of the probe under the MSVC target. It
    needs codegen (clang only prints a vtable it emits), so this is not an
    `-fsyntax-only` pass."""
    return clang(
        cfg,
        ["-S", "-o", str(scratch / "probe-msvc.s"), "-Xclang", "-fdump-vtable-layouts"],
        PROBE_CPP,
    )


def msvc_const_values(
    consts: list[ConstEntry], cfg: CmakeFlags, scratch: Path
) -> tuple[dict[str, int], dict[str, str]]:
    """Measure the `[consts]` expressions under the MSVC target: the same
    generated probe TU the Itanium pass uses, so the value is the compiler's on
    both ABIs (a disagreement is what `per_abi` is about)."""
    probes = const_probes(consts)
    strings = const_strings(consts)
    if not probes and not strings:
        return {}, {}
    tu = scratch / "probe_msvc_consts.cpp"
    tu.write_text(gen_probe_tu(probes, msvc=True, strings=strings), encoding="utf-8")
    out = tu.with_suffix(".json")
    clang(
        cfg,
        [
            "-fno-access-control",
            "-fsyntax-only",
            "-Xclang",
            "-ast-dump=json",
            "-Xclang",
            "-ast-dump-filter=s2sdk_",
        ],
        tu,
        out,
    )
    objects = parse_ast_json_stream(out.read_text(encoding="utf-8"))
    values = parse_probe_values(objects, PROBE_TEMPLATE_NAMES)
    return values, parse_probe_strings(objects)


def msvc_spec_facts(
    ir_by_group: dict,
    layouts: dict[str, RecordLayout],
    field_sizes: dict[str, int],
) -> dict[str, dict]:
    """{specialization: {size, align, fields, members}} for the `specs` of every
    group, from the MSVC layout dump - the same shape as a record's, plus
    `members` (clang's full, nested list) for the `[checks]` that name a
    subobject."""
    facts: dict[str, dict] = {}
    for group in GROUPS:
        for spec in ir_by_group[group].get("specs") or []:
            name = spec["name"]
            layout = layouts.get(normalize_cxx_type_name(name))
            if layout is None:
                raise GenError(
                    f"{name}: the MSVC pass laid out no record for the specialization; "
                    f"is there a S2SDK_FORCE_SPEC(_FIELD) row for it in gen/probe.cpp?"
                )
            check_msvc_layout(name, layout)
            leaves = {path: member for path, member in flatten_leaf_members(layout)}
            fields = []
            for field in spec["fields"]:
                key = f"{name}.{field['name']}"
                size = field_sizes.get(key)
                if size is None:
                    raise GenError(f"{key}: the MSVC pass measured no field size")
                member = leaves.get(tuple(field.get("path") or [field["name"]]))
                if member is None:
                    raise GenError(
                        f"{key}: clang printed no such member in the MSVC layout of {name}"
                    )
                fields.append(
                    {
                        "name": field["name"],
                        "offset": member.offset,
                        "size": size,
                        "bit_offset": member.bit_offset,
                        "bit_width": member.bit_width,
                    }
                )
            facts[name] = {
                "size": layout.size,
                "align": layout.align,
                "fields": fields,
                "members": [
                    {
                        "name": member.name,
                        "level": member.level,
                        "offset": member.offset,
                        "bit_offset": member.bit_offset,
                        "bit_width": member.bit_width,
                    }
                    for member in layout.members
                ],
            }
    return facts


def msvc_enum_size(enum: dict, size: int) -> dict:
    """The `facts.enums` entry after the MSVC enum-size rule: an unfixed enum
    is `int` (the MSVC rule, which the measured size confirms), a fixed one is
    its declared underlying type, and the measured size must equal the width
    that rule predicts. An enum the two ABIs size differently would be a
    silently split type behind one Rust name, so it is refused (the self-test
    fires this both ways) instead of emitted."""
    name = enum["name"]
    fixed = bool(enum["fixed_underlying"])
    underlying = enum["underlying"] if fixed else "int"
    expected = MSVC_PRIMITIVE_SIZES.get(underlying)
    if expected is None:
        raise GenError(f"{name}: no size known for the MSVC underlying type {underlying!r}")
    if size != expected:
        raise GenError(
            f"{name}: MSVC lays it out as {size} bytes but "
            f"{'its fixed underlying type' if fixed else 'the MSVC rule'} "
            f"{underlying!r} is {expected}; the ABI rule needs revisiting"
        )
    return {"size": size, "underlying": underlying}


def gather_msvc_facts(
    metamod: Path,
    sdk: Path,
    allowlist: dict,
    ir_by_group: dict,
    cfg: CmakeFlags,
    scratch: Path,
    commit: str,
) -> MsvcFacts:
    """Run every MSVC pass (`--abi msvc`) and key what they measured so it can be
    merged into the Itanium IR. Writes the layout artifacts of the pass under
    scratch/ (target/s2sdk-gen/msvc/ in a normal run)."""
    validate_allowlist(allowlist)
    scratch.mkdir(parents=True, exist_ok=True)
    vlog("msvc pass 1/3: record layouts")
    text = record_layout_dump(cfg)
    if "vftable pointer" not in text or "vtable pointer" in text:
        raise GenError(
            "the MSVC pass printed no MSVC record layout (no 'vftable pointer'); "
            "clang was not given --target=x86_64-pc-windows-msvc"
        )
    unnormalized = parse_record_layouts(text)
    if not unnormalized:
        raise GenError(
            "the MSVC pass printed no record layout at all; is gen/probe.cpp still "
            "taking sizeof of the allowlisted records?"
        )
    write_msvc_layouts(metamod, sdk, allowlist, unnormalized, commit, cfg, scratch)
    layouts = bare_keyed(unnormalized, "MSVC record layouts")

    vlog("msvc pass 2/3: vftable layouts")
    vtable_text = msvc_vtable_dump(cfg, scratch)
    blocks = parse_vtable_blocks(vtable_text)
    if not any(block.kind == "indices" for block in blocks):
        raise GenError(
            "the MSVC pass printed no 'VFTable indices' block; clang emitted no "
            "vftable (does gen/probe.cpp still take the address of one virtual per "
            "allowlisted class?)"
        )

    vlog("msvc pass 3/3: field and enum sizes, specializations and consts")
    field_sizes = msvc_field_sizes(ir_by_group, cfg, scratch)
    enum_sizes = msvc_enum_values(ir_by_group, cfg, scratch)
    spec_facts = msvc_spec_facts(ir_by_group, layouts, field_sizes)
    consts = parse_consts(allowlist[CONST_GROUP])
    const_values, const_string_values = msvc_const_values(consts, cfg, scratch)
    measured_consts = measure_consts(consts, const_values, const_string_values, layouts)

    # The standalone `--abi msvc` run re-measures the committed IR (which already
    # carries both ABIs): a value that moved is a failed run, not a new file. In
    # the dual-ABI run the IR has no `"msvc"` sub-objects yet - merge_msvc does
    # this comparison instead.
    for name, value in measured_consts.items():
        committed = next(
            (
                const
                for const in ir_by_group[CONST_GROUP].get("consts") or []
                if const["name"] == name
            ),
            None,
        )
        if committed is None:
            raise GenError(
                f"[consts] {name}: the committed ir/{CONST_GROUP}.json has no such entry; "
                f"run a full gen.py (or --check) to regenerate it"
            )
        expected = (committed.get("msvc") or {}).get("value")
        if expected is not None and value != expected:
            raise GenError(
                f"[consts] {name}: the MSVC pass measures {value!r} but the committed "
                f"IR says {expected!r}; regenerate with gen.py"
            )
    for name, spec_fact in spec_facts.items():
        committed = next(
            (
                spec
                for group in GROUPS
                for spec in ir_by_group[group].get("specs") or []
                if spec["name"] == name
            ),
            None,
        )
        if committed is None:
            raise GenError(f"{name}: the committed IR has no such specialization")
        expected = (committed.get("msvc") or {}).get("size")
        if expected is not None and spec_fact["size"] != expected:
            raise GenError(
                f"{name}: the MSVC pass lays it out as {spec_fact['size']} bytes but the "
                f"committed IR says {expected}; regenerate with gen.py"
            )

    for group in GROUPS:
        for name in (
            list(allowlist[group].get("records") or [])
            + list(allowlist[group].get("opaque") or [])
            + list(allowlist[group].get("vtables") or [])
        ):
            layout = layouts.get(normalize_cxx_type_name(name))
            if layout is None:
                raise GenError(
                    f"{name}: the MSVC pass laid out no record for it; is "
                    f"S2SDK_FORCE_LAYOUT( {name} ) in gen/probe.cpp?"
                )
            check_msvc_layout(name, layout)

    records: dict[str, dict] = {}
    enums: dict[str, dict] = {}
    slots: dict[str, dict[int, int]] = {}
    names: dict[str, dict[int, str]] = {}
    vtables: dict[str, dict] = {}
    bases: dict[str, dict[str, int]] = {}

    for group in GROUPS:
        for record in ir_by_group[group]["records"]:
            name = record["name"]
            layout = layouts.get(normalize_cxx_type_name(name))
            if layout is None:
                raise GenError(
                    f"{name}: the MSVC pass laid out no record for it; is "
                    f"S2SDK_FORCE_LAYOUT( {name} ) in gen/probe.cpp?"
                )
            members = {member.name: member for member in layout.own_members if member.name}
            fields = []
            for field in record["fields"]:
                key = f"{name}.{field['name']}"
                size = field_sizes.get(key)
                if size is None:
                    raise GenError(f"{key}: the MSVC pass measured no field size")
                member = members.get(field["name"])
                if member is None:
                    raise GenError(
                        f"{key}: clang printed no such member in the MSVC layout of {name}"
                    )
                fields.append(
                    {
                        "name": field["name"],
                        "offset": member.offset,
                        "size": size,
                        "bit_offset": member.bit_offset,
                        "bit_width": member.bit_width,
                    }
                )
            records[name] = {
                "size": layout.size,
                "align": layout.align,
                "fields": fields,
                "base_offsets": [base.offset for base in layout.bases],
            }
        for enum in ir_by_group[group]["enums"]:
            name = enum["name"]
            size = enum_sizes.get(name)
            if size is None:
                raise GenError(f"{name}: the MSVC pass measured no enum size")
            enums[name] = msvc_enum_size(enum, size)
        for vtable in ir_by_group[group]["vtables"]:
            class_name = vtable["class"]
            class_slots, class_names, summary = msvc_vtable(
                class_name,
                vtable["methods"],
                vtable["primary_base"],
                blocks,
                primary_chain_root(ir_by_group, vtable["primary_base"], layouts),
            )
            slots[class_name] = class_slots
            names[class_name] = class_names
            vtables[class_name] = summary
            class_layout = layouts.get(normalize_cxx_type_name(class_name))
            if class_layout is None:
                raise GenError(f"{class_name}: the MSVC pass laid out no record for it")
            bases[class_name] = {
                base.name: base.offset
                for base in class_layout.bases
                if not (base.offset == 0 and not base.virtual)
            }

    return MsvcFacts(
        records=records,
        enums=enums,
        slots=slots,
        names=names,
        vtables=vtables,
        specs=spec_facts,
        consts=measured_consts,
        bases=bases,
    )


def write_msvc_facts(
    metamod: Path,
    facts: MsvcFacts,
    commit: str,
    cfg: CmakeFlags,
    out_dir: Path = MSVC_DIR,
) -> None:
    """target/s2sdk-gen/msvc/facts.json: the vftable/slot, specialization and
    `[consts]` side of the MSVC pass (the record layouts are in <group>.json and
    records.json). Not committed - the committed numbers are the `"msvc"`
    sub-objects, `msvc_slot`s and `[consts]` values of ir/."""
    out_dir.mkdir(parents=True, exist_ok=True)
    payload = {
        "abi": ABI_MSVC_ABI,
        "commit": commit,
        "clang": clang_version(),
        "sysroot": path_rewriter(metamod, find_sdk_dir(), cfg)(str(cfg.sysroot))
        if cfg.sysroot
        else None,
        "enums": facts.enums,
        "vtables": facts.vtables,
        "specs": facts.specs,
        "consts": facts.consts,
    }
    (out_dir / "facts.json").write_text(dump_ir(payload), encoding="utf-8")


# --------------------------------------------------------------------------- #
# the dual-ABI IR
# --------------------------------------------------------------------------- #


def abi_field_summary(field: dict) -> dict:
    return {
        "name": field["name"],
        "offset": field["offset"],
        "size": field["size"],
        "bit_offset": field["bit_offset"],
        "bit_width": field["bit_width"],
    }


def msvc_pass_in_registers(pass_in_registers: bool, size: int) -> bool:
    """MSVC's answer for a record passed or returned by value (design 2.1): a
    trivially copyable aggregate of at most 8 bytes goes in one integer
    register; a non-trivial or >8-byte aggregate travels through a hidden
    pointer, the second argument right after `this`.

    The triviality half comes from the AST's `canPassInRegisters` flag (clang
    computes it per target; the Itanium pass's flag is the only one there is),
    the size half is the record's own MSVC size. The top-level number stays the
    Itanium one - the Rust emitter reads it - so the two answers have to live on
    their sub-objects."""
    return pass_in_registers and size <= 8


def merge_msvc(ir_by_group: dict, facts: MsvcFacts) -> None:
    """Add every type's `"itanium"` / `"msvc"` sub-object and every method's
    `msvc_slot` to the Itanium IR.

    The top-level numbers stay exactly what they were and stay the Itanium ones:
    consumers that do not care about the ABI read them, and an item whose emitted
    body the two ABIs agree on stays byte-for-byte what the single-ABI emitter
    produced. The Rust emitter reads each ABI's own sub-object. The `"itanium"`
    sub-object restates them, so a consumer can read both ABIs the same way."""
    for group in GROUPS:
        ir = ir_by_group[group]
        for record in ir["records"]:
            name = record["name"]
            measured = facts.records.get(name)
            if measured is None:
                raise GenError(f"{name}: the MSVC pass produced no record facts for it")
            record["itanium"] = {
                "size": record["size"],
                "align": record["align"],
                "pass_in_registers": record["pass_in_registers"],
                "fields": [abi_field_summary(field) for field in record["fields"]],
            }
            record["msvc"] = {
                "size": measured["size"],
                "align": measured["align"],
                "pass_in_registers": msvc_pass_in_registers(
                    record["pass_in_registers"], measured["size"]
                ),
                "fields": measured["fields"],
            }
            # a base subobject can sit elsewhere under MSVC (Itanium reuses the tail padding of
            # a preceding base); recorded only where the two ABIs differ, so the IR of every
            # record whose bases agree is unchanged
            base_offsets = measured.get("base_offsets") or []
            if len(base_offsets) != len(record["bases"]):
                raise GenError(
                    f"{name}: the MSVC layout has {len(base_offsets)} bases, the record "
                    f"{len(record['bases'])}"
                )
            if base_offsets != [base["offset"] for base in record["bases"]]:
                record["msvc"]["base_offsets"] = base_offsets
        for spec in ir.get("specs") or []:
            name = spec["name"]
            measured = facts.specs.get(name)
            if measured is None:
                raise GenError(f"{name}: the MSVC pass produced no facts for the specialization")
            spec["itanium"] = {
                "size": spec["size"],
                "align": spec["align"],
                "fields": [abi_field_summary(field) for field in spec["fields"]],
            }
            spec["msvc"] = measured
        for enum in ir["enums"]:
            name = enum["name"]
            measured = facts.enums.get(name)
            if measured is None:
                raise GenError(f"{name}: the MSVC pass produced no enum facts for it")
            enum["itanium"] = {
                "size": RUST_SIZES[enum["rust_repr"]],
                "underlying": enum["underlying"],
                "fixed": enum["fixed_underlying"],
            }
            enum["msvc"] = measured
        for const in ir.get("consts") or []:
            name = const["name"]
            measured = facts.consts.get(name)
            if measured is None:
                raise GenError(f"[consts] {name}: the MSVC pass measured no value")
            itanium = const["itanium"]["value"]
            if measured != itanium and not const["per_abi"]:
                raise GenError(
                    f"[consts] {name}: the two ABIs disagree ({itanium!r} on Itanium, "
                    f"{measured!r} on MSVC) and the entry is not marked "
                    f"per_abi = true"
                )
            const["msvc"] = {"value": measured}
        for vtable in ir["vtables"]:
            class_name = vtable["class"]
            class_slots = facts.slots.get(class_name)
            if class_slots is None:
                raise GenError(f"{class_name}: the MSVC pass produced no vtable facts for it")
            for method in vtable["methods"]:
                slot = class_slots.get(method["decl_index"])
                if slot is None:
                    raise GenError(
                        f"{class_name}::{method['name']}#{method['decl_index']}: no MSVC slot "
                        f"was derived"
                    )
                method["msvc_slot"] = slot
                spelling = facts.names.get(class_name, {}).get(method["decl_index"])
                if spelling is not None and spelling != method["name"]:
                    # A Windows header macro renamed the declaration (`CreateEvent`
                    # -> `CreateEventA`); it is the same slot either way.
                    method["msvc_name"] = spelling
            msvc_bases = facts.bases.get(class_name) or {}
            if sorted(msvc_bases) != sorted(b["name"] for b in vtable["secondary_bases"]):
                raise GenError(
                    f"{class_name}: the secondary bases differ between the ABIs "
                    f"(Itanium {[b['name'] for b in vtable['secondary_bases']]}, "
                    f"MSVC {sorted(msvc_bases)})"
                )
            for base in vtable["secondary_bases"]:
                base["msvc_offset"] = msvc_bases[base["name"]]
            summary = facts.vtables.get(class_name) or {}
            # clang's own vftable length (the full VFTable minus its RTTI entry).
            # The generated layout test asserts `vtable_len(Msvc, ..)` against it,
            # so the vtable_len the bridge's pins assume is checked by clang.
            vtable["msvc"] = {
                "length": summary.get("length"),
                "own_entries": summary.get("own_entries"),
            }


def check_dual_abi(ir_by_group: dict) -> None:
    """Every type and method has to carry both ABIs - the acceptance criterion
    `every allowlisted method has both slots` - and the `"itanium"` sub-object has
    to be the top-level numbers, so an emitter can read either one."""
    for group in GROUPS:
        ir = ir_by_group[group]
        for record in ir["records"]:
            name = record["name"]
            for abi in ("itanium", "msvc"):
                if abi not in record:
                    raise GenError(f"{name}: the IR has no {abi!r} sub-object")
                # `base_offsets` (MSVC only) is present only where a base subobject sits at a
                # different offset than on Itanium
                allowed = {"size", "align", "fields", "pass_in_registers"}
                if abi == "msvc" and "base_offsets" in record[abi]:
                    allowed.add("base_offsets")
                if set(record[abi]) != allowed:
                    raise GenError(f"{name}: the {abi!r} sub-object has keys {sorted(record[abi])}")
            itanium = record["itanium"]
            if itanium["size"] != record["size"] or itanium["align"] != record["align"]:
                raise GenError(f"{name}: the itanium sub-object is not the top-level size/align")
            if itanium["pass_in_registers"] != record["pass_in_registers"]:
                raise GenError(
                    f"{name}: the itanium pass_in_registers is not the top-level one"
                )
            for abi in ("itanium", "msvc"):
                fields = record[abi]["fields"]
                if [field["name"] for field in fields] != [f["name"] for f in record["fields"]]:
                    raise GenError(
                        f"{name}: the {abi!r} field list is not the record's field list "
                        f"({[field['name'] for field in fields]})"
                    )
        for spec in ir.get("specs") or []:
            name = spec["name"]
            for abi in ("itanium", "msvc"):
                if abi not in spec:
                    raise GenError(f"{name}: the specialization has no {abi!r} sub-object")
            if set(spec["itanium"]) != {"size", "align", "fields"}:
                raise GenError(f"{name}: the itanium sub-object has keys {sorted(spec['itanium'])}")
            if set(spec["msvc"]) != {"size", "align", "fields", "members"}:
                raise GenError(f"{name}: the msvc sub-object has keys {sorted(spec['msvc'])}")
            if spec["itanium"]["size"] != spec["size"] or spec["itanium"]["align"] != spec["align"]:
                raise GenError(f"{name}: the itanium sub-object is not the top-level size/align")
            names = [field["name"] for field in spec["fields"]]
            if [field["name"] for field in spec["itanium"]["fields"]] != names:
                raise GenError(f"{name}: the itanium field list is not the spec's field list")
            if [field["name"] for field in spec["msvc"]["fields"]] != names:
                raise GenError(f"{name}: the msvc field list is not the spec's field list")
        for enum in ir["enums"]:
            name = enum["name"]
            for abi in ("itanium", "msvc"):
                if abi not in enum:
                    raise GenError(f"{name}: the IR has no {abi!r} sub-object")
                for key in ("size", "underlying"):
                    if key not in enum[abi]:
                        raise GenError(f"{name}: the {abi!r} sub-object has no {key!r}")
            if enum["itanium"]["size"] != RUST_SIZES[enum["rust_repr"]]:
                raise GenError(f"{name}: the itanium size is not the emitted repr's size")
            if enum["itanium"]["underlying"] != enum["underlying"]:
                raise GenError(f"{name}: the itanium sub-object is not the top-level underlying")
            if enum["msvc"]["size"] != RUST_SIZES[enum["rust_repr"]]:
                # The emitted enum is a newtype over one `rust_repr`, so the two
                # ABIs' widths have to agree with it: an unfixed enum of fewer
                # than 4 bytes is `int` under MSVC and the smallest fitting type
                # under Itanium, and a single repr cannot spell both.
                raise GenError(
                    f"{name}: MSVC lays the enum out as {enum['msvc']['size']} bytes "
                    f"but the emitted repr {enum['rust_repr']!r} is "
                    f"{RUST_SIZES[enum['rust_repr']]} bytes; a per-ABI repr would be "
                    f"needed"
                )
        for const in ir.get("consts") or []:
            name = const["name"]
            for abi in ("itanium", "msvc"):
                if abi not in const:
                    raise GenError(f"[consts] {name}: the IR has no {abi!r} sub-object")
            if const["itanium"]["value"] != const["value"]:
                raise GenError(f"[consts] {name}: the itanium value is not the top-level one")
            if const["itanium"]["value"] != const["msvc"]["value"] and not const["per_abi"]:
                raise GenError(f"[consts] {name}: the ABIs disagree and it is not per_abi")
        for vtable in ir["vtables"]:
            class_name = vtable["class"]
            if "msvc" not in vtable:
                raise GenError(f"{class_name}: the vtable has no msvc sub-object")
            for method in vtable["methods"]:
                what = f"{vtable['class']}::{method['name']}#{method['decl_index']}"
                if method.get("itanium_slot") is None:
                    raise GenError(f"{what}: no itanium slot")
                if method.get("msvc_slot") is None:
                    raise GenError(f"{what}: no msvc slot")
                ret = method.get("ret")
                if ret is None:
                    continue
                # a by-value class return carries both ABIs' transports; the
                # sizes are the record's own two (`attach_return_transports`
                # reads them off the record's IR objects), so a structural
                # check here and the derivation's own loud failures cover them
                if set(ret) != {"itanium", "msvc"}:
                    raise GenError(f"{what}: the 'ret' object has keys {sorted(ret)}")
                if set(ret["itanium"]) != {"sret", "size", "regs"}:
                    raise GenError(f"{what}: the itanium 'ret' has keys {sorted(ret['itanium'])}")
                if set(ret["msvc"]) != {"sret", "size"}:
                    raise GenError(f"{what}: the msvc 'ret' has keys {sorted(ret['msvc'])}")
                if ret["itanium"]["regs"] not in ("int", "sse", "mixed", "memory"):
                    raise GenError(f"{what}: the itanium 'ret' regs are {ret['itanium']['regs']!r}")
                if ret["itanium"]["sret"] != (ret["itanium"]["regs"] == "memory"):
                    raise GenError(
                        f"{what}: the itanium 'ret' says sret={ret['itanium']['sret']} "
                        f"with regs {ret['itanium']['regs']!r}"
                    )
                if ret["msvc"]["sret"] is not True:
                    raise GenError(f"{what}: the msvc 'ret' says sret={ret['msvc']['sret']!r}")
                for abi in ("itanium", "msvc"):
                    size = ret[abi]["size"]
                    if not isinstance(size, int) or isinstance(size, bool) or size <= 0:
                        raise GenError(f"{what}: the {abi} 'ret' size is {size!r}")


def check_by_value_signatures(ir_by_group: dict) -> None:
    """A method's `rust_fn` is modelled from the *Itanium* `pass_in_registers`
    (rule 7), and there is one `rust_fn` per method, not one per ABI. For a
    **parameter** that is sound as long as the record is trivially copyable on
    both ABIs: Rust's `extern "C"` lowers a by-value aggregate the way each
    target's C++ member call does (a pointer to a copy in the same position on
    Windows, registers on Linux), so the one spelling is right for both. A
    non-trivial record is passed through a hidden pointer on both, which
    `rust_fn` cannot spell, so the triviality of the two ABIs has to agree.

    Only the triviality is compared: the size half of `msvc_pass_in_registers`
    does not matter for a parameter. A **return** by value is a different
    matter (MSVC always returns a class through a hidden pointer after `this`);
    that is the method's `msvc_rust_fn` (`msvc_hidden_return_fn`), see
    crates/s2sdk/README.md rule 7, so there is nothing to compare for it."""
    records = {
        record["name"]: (record["itanium"], record["msvc"])
        for group in GROUPS
        for record in ir_by_group[group]["records"]
    }
    for group in GROUPS:
        for vtable in ir_by_group[group]["vtables"]:
            for method in vtable["methods"]:
                rust_fn = method.get("rust_fn")
                if not rust_fn:
                    continue
                for cxx in by_value_signature_types(rust_fn, parameters_only=True):
                    halves = records.get(cxx)
                    if halves is None:
                        continue
                    itanium_trivial = halves[0]["pass_in_registers"]
                    # MSVC's answer folds the size in; a record over 8 bytes is
                    # trivial there as long as the Itanium flag says so.
                    msvc_trivial = halves[1]["pass_in_registers"] or (
                        halves[1]["size"] > 8 and itanium_trivial
                    )
                    if itanium_trivial == msvc_trivial:
                        continue
                    what = f"{vtable['class']}::{method['name']}#{method['decl_index']}"
                    raise GenError(
                        f"{what}: {cxx} is passed by value and the two ABIs disagree "
                        f"on whether it is trivially copyable (itanium={itanium_trivial}, "
                        f"msvc={msvc_trivial}), so one `rust_fn` cannot spell both ABIs"
                    )


def by_value_signature_types(rust_fn: str, parameters_only: bool = False) -> list[str]:
    """The parameters (and, unless `parameters_only`, the return type) of an
    emitted `rust_fn` that are not behind a pointer: the types rule 7's
    `pass_in_registers` answer applies to.
    The strings come from `build_rust_fn`, so their shape is known."""
    open_at = rust_fn.find("fn(")
    if open_at < 0:
        return []
    close_at = find_matching_paren(rust_fn, open_at + 2)
    if close_at is None:
        return []
    types = []
    for param in split_top_level(rust_fn[open_at + 3 : close_at]):
        _, _, spelled = param.partition(":")
        if spelled:
            types.append(spelled.strip())
    tail = rust_fn[close_at + 1 :].strip()
    if tail.startswith("->") and not parameters_only:
        types.append(tail[2:].strip())
    return [spelled for spelled in types if not spelled.startswith("*")]


def find_matching_paren(text: str, open_at: int) -> int | None:
    """Index of the `)` matching `text[open_at] == '('`."""
    if open_at >= len(text) or text[open_at] != "(":
        return None
    depth = 0
    for index in range(open_at, len(text)):
        if text[index] == "(":
            depth += 1
        elif text[index] == ")":
            depth -= 1
            if depth == 0:
                return index
    return None


def ir_upstream_header(commit: str) -> dict:
    """The `upstream` block of an ir/<group>.json file: the pinned commit and
    the ABIs the file's sub-objects carry the numbers for (the design asked for
    the list, not a single `abi`)."""
    return {"commit": commit, "abis": [ABI, ABI_MSVC_ABI]}


def metamod_upstream_header() -> dict:
    """The `upstream` block of ir/metamod.json: the second upstream's own commit
    (not the SDK's, which every other group records) plus the KHook commit, since
    IKHook's slots come out of metamod's third_party/khook submodule, which the
    metamod commit does not pin by itself."""
    lock = read_metamod_lock()
    return {
        "commit": lock["commit"],
        "abis": [ABI, ABI_MSVC_ABI],
        "khook_commit": lock["khook_commit"],
    }


# --------------------------------------------------------------------------- #
# self-test: the dump parsers, on inline samples
#
# gen.py's own suite lives in gen_selftest.py (same directory). It is
# imported right after the paths block (an isolated `python3 -I` run has
# neither script's directory on sys.path, so that import happens with the
# directory inserted first) and `--self-test` dispatches to it with
# `sys.modules[__name__]` as the host module - the suite monkeypatches
# globals on THAT object (sdk_head), because the functions under test
# (locked_heads) read gen.py's own globals.
# --------------------------------------------------------------------------- #



# --------------------------------------------------------------------------- #
# entry point
# --------------------------------------------------------------------------- #


def load_committed_ir() -> dict:
    """The committed ir/<group>.json, for the passes that need a declaration order
    to match a dump against (the standalone `--abi msvc` run)."""
    ir_by_group = {}
    for group in GROUPS:
        path = IR_DIR / f"{group}.json"
        if not path.is_file():
            raise GenError(f"{path.relative_to(REPO_ROOT)} is missing; run gen.py first")
        with path.open(encoding="utf-8") as handle:
            ir_by_group[group] = json.load(handle)
    return ir_by_group


def main(argv: list[str]) -> int:
    global VERBOSE, QUIET, VERIFY_CXX
    parser = argparse.ArgumentParser(
        prog="gen.py",
        description="Generate s2sdk-sys IR and Rust bindings from the pinned upstream SDK headers.",
    )
    parser.add_argument(
        "--ir-only",
        action="store_true",
        help="stop after the IR (skip the Rust emitter)",
    )
    parser.add_argument(
        "--abi",
        choices=(ABI_DEFAULT, ABI_MSVC),
        default=ABI_DEFAULT,
        help="which ABI to generate for: itanium (Linux x86_64, the default) runs "
        "both ABIs and writes the dual-ABI ir/; msvc only measures Windows x64 "
        "and writes target/s2sdk-gen/msvc/",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="regenerate BOTH ABIs into target/ and fail if ir/, src/generated/, "
        "tests/layout.rs or crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h differ",
    )
    parser.add_argument(
        "--no-verify-cxx",
        action="store_true",
        help="skip the clang static_assert re-derivation of the IR",
    )
    parser.add_argument(
        "--self-test", action="store_true", help="assert the dump parsers and the emitters"
    )
    parser.add_argument("--verbose", "-v", action="store_true", help="log every step")
    parser.add_argument("--quiet", "-q", action="store_true", help="only errors")
    args = parser.parse_args(argv)
    VERBOSE = args.verbose
    QUIET = args.quiet
    VERIFY_CXX = not args.no_verify_cxx

    if args.self_test:
        try:
            return gen_selftest.self_test(sys.modules[__name__])
        except GenError as exc:
            print(f"gen.py: error: {exc}", file=sys.stderr)
            return 1

    try:
        sdk = find_sdk_dir()
        metamod = find_metamod_dir()
        lock = read_lock()
        mismatches = lock_mismatches(locked_heads(sdk, metamod))
        for line in mismatches:
            log(f"warning: {line}")
        if mismatches and args.check:
            log("check: a checkout is not the revision its lock pins")
            return 2
        vlog(f"SDK: {sdk}")
        vlog(f"Metamod:Source: {metamod}")

        allowlist_path = ALLOWLIST_TOML
        with allowlist_path.open("rb") as handle:
            allowlist = tomllib.load(handle)

        # The MSVC half is part of every generation now: ir/<group>.json carries
        # both ABIs, so a run that only saw one of them could not check, compare
        # or commit a complete file. parse_cmake(.., "msvc") is what fails loudly
        # (naming fetch-xwin.sh) when the Windows sysroot is missing.
        itanium_cfg = parse_cmake(sdk, ABI_DEFAULT)
        msvc_cfg = parse_cmake(sdk, ABI_MSVC)
        check_no_shadowed_headers(metamod, itanium_cfg.includes)
        # The second upstream's include dirs, in front of the SDK's: see
        # metamod_first_includes. Every clang invocation of the run (both ABIs,
        # the probe TU, the MSVC vftable pass and the static_assert TU) compiles
        # probe.cpp, so they all see the same headers.
        itanium_cfg = dataclasses.replace(
            itanium_cfg,
            includes=metamod_first_includes(metamod, itanium_cfg.includes),
            defines=metamod_defines(itanium_cfg.defines),
        )
        msvc_cfg = dataclasses.replace(
            msvc_cfg,
            includes=metamod_first_includes(metamod, msvc_cfg.includes),
            defines=metamod_defines(msvc_cfg.defines),
        )
        vlog(f"clang flags (itanium): {' '.join(itanium_cfg.clang_flags())}")
        vlog(f"clang flags (msvc): {' '.join(msvc_cfg.clang_flags())}")

        if args.check:
            scratch = SCRATCH / "check"
            result = generate_dual(sdk, metamod, allowlist, scratch, itanium_cfg, msvc_cfg)
            print_summary(result.ir_by_group, result.protos)
            files = emit_all(result.ir_by_group, lock["commit"], allowlist.get("checks"))
            c_header = emit_c_header(result.ir_by_group)
            write_ir(result.ir_by_group, scratch / "ir")
            write_rust(files, scratch)
            smoke_compile_c_header(c_header, scratch / "c_header_smoke")
            failures = (
                check_ir(result.ir_by_group)
                + check_rust(files)
                + check_c_header(c_header)
            )
            if failures:
                log(f"check: {failures} generated file(s) differ")
                return 1
            log(
                "check: the committed dual-ABI IR, Rust bindings and "
                "crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h match the pinned upstream"
            )
            return 0

        if args.abi == ABI_MSVC:
            if args.ir_only:
                log("--abi msvc is layouts/slots only: there is nothing for --ir-only to skip")
            run_msvc_pass(metamod, allowlist, lock["commit"], msvc_cfg, load_committed_ir())
            log(
                "msvc: the committed numbers live in ir/<group>.json's \"msvc\" "
                "sub-objects and msvc_slot keys; run gen.py (or gen.py --check) to "
                "regenerate both ABIs"
            )
            return 0

        result = generate_dual(sdk, metamod, allowlist, SCRATCH, itanium_cfg, msvc_cfg)
        print_summary(result.ir_by_group, result.protos)
        write_ir(result.ir_by_group, IR_DIR)
        log(f"wrote {IR_DIR.relative_to(REPO_ROOT)}/{{{'|'.join(GROUPS)}}}.json")
        if args.ir_only:
            return 0
        files = emit_all(result.ir_by_group, lock["commit"], allowlist.get("checks"))
        c_header = emit_c_header(result.ir_by_group)
        write_rust(files, S2SDK_SYS_DIR)
        write_c_header(c_header)
        smoke_compile_c_header(c_header, SCRATCH / "c_header_smoke")
        log(
            "wrote "
            + ", ".join(sorted(files))
            + " (rustfmt --edition 2021 --config newline_style=Unix)"
        )
        log(
            f"wrote {C_HEADER_PATH.relative_to(REPO_ROOT)} (smoke-compiled freestanding "
            "on the MSVC target)"
        )
        return 0
    except GenError as exc:
        print(f"gen.py: error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
