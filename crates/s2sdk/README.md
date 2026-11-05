# crates/s2sdk/ — Rust bindings for the Source 2 SDK (CS2)

Rust wrapper over [alliedmodders/s2sdk](https://github.com/alliedmodders/s2sdk) (branch `cs2`),
built to follow upstream's fast pace: the bindings are **generated** from the pinned upstream
revision, never written by hand.

| Crate | Role |
|---|---|
| `s2sdk-sys/` | raw `#[repr(C)]` structs, enums and vtable structs generated from the upstream headers; `abi` slot calculator (Itanium + MSVC); layout tests |
| `s2sdk/` | safe layer for code running inside a CS2 process: modules, `CreateInterface`, schema system, convars, game-event listeners, prost protobuf messages |
| `s2sdk-probe/` | `cdylib` loaded with `LD_PRELOAD` into the Linux dedicated server / client; exercises `s2sdk` and reports one `[s2sdk-probe] RESULT ok\|fail stage=…` line plus a JSON report of every check (live test) |

Upstream location: `$S2SDK_DIR`, default the `crates/s2sdk/upstream` git submodule
(`git submodule update --init crates/s2sdk/upstream`). The revision the committed output was generated
from is `s2sdk-sys/UPSTREAM.lock`; the submodule must sit at that commit (`gen.py --check`
exits 2 otherwise). Game-update procedure: [../../workflows/game-update.md](../../workflows/game-update.md).

A **second** upstream, Metamod:Source + KHook, feeds the `metamod` group only (see
[the second upstream](#the-second-upstream-metamod)): `crates/metamod/upstream`, pinned by
`s2sdk-sys/METAMOD.lock` (which pins the nested `third_party/khook` commit too). It is a separate
submodule with its own init commands, and `gen.py --check` holds both checkouts to their locks.

## Repo rules this crate follows

- **No hand-written addresses, slots or offsets in Rust.** Every slot/offset in `s2sdk-sys` comes
  from the generator (clang's record/vtable layout dumps of the upstream headers). MSVC slots are
  computed by `abi`; the bridge compiles the same numbers from the generated `bridge/s2sdk_gen.h`, and `sigs/scripts/sdkcheck.py` holds them to the game binaries.
- Never patch Valve code: listeners are Rust-owned objects registered through the game's own APIs.
- `s2sdk-sys` is generated for both targets the game ships for: the Itanium (Linux x86_64) and
  the MSVC (Windows x64) numbers, each emitted behind the `cfg` that selects it. It is built and
  tested on Linux; the Windows half is compiled with
  `cargo check --target x86_64-pc-windows-msvc -p s2sdk-sys` (no linker needed) because the
  Windows *runtime* consumer of those layouts is the C bridge (`../../bridge/`).
- The safe layer (`s2sdk/`) also covers both targets — see [Windows support](#windows-support).
- Generator scratch output goes under `<repo>/target/s2sdk-gen/`, never `/tmp` (tmpfs).

## Windows support

Both `s2sdk-sys` and `s2sdk` build for `x86_64-pc-windows-msvc`; the Windows half is proven by
`cargo check`/`cargo clippy` (no linker, no game), because the Windows runtime consumers are the
C bridge (`../../bridge/`) and the game process itself.

```sh
rustup target add x86_64-pc-windows-msvc          # once
cargo check  --target x86_64-pc-windows-msvc -p s2sdk-sys -p s2sdk --all-targets
cargo clippy --target x86_64-pc-windows-msvc -p s2sdk-sys -p s2sdk --all-targets
```

What differs per target, and where it lives:

- **Module lookup (`s2sdk/src/module.rs`).** Linux uses `dl_iterate_phdr` +
  `dlopen(RTLD_LAZY | RTLD_NOLOAD)`; Windows uses `GetModuleHandleW` (which never loads, exactly
  like `RTLD_NOLOAD`) + `GetModuleFileNameW` for the mapped path + `GetProcAddress` for symbols,
  through the `windows-sys` crate (pinned to the copy already in `Cargo.lock`, with only
  `Win32_Foundation` + `Win32_System_LibraryLoader` — no new download). The Windows `Module` has
  no `Drop`: a `GetModuleHandleW` handle carries no reference to release. `libc` is a
  `cfg(unix)` dependency.
- **Module names are per target.** `module::names` is the single table: Linux `lib<stem>.so`,
  Windows `<stem>.dll` (`libschemasystem.so`/`schemasystem.dll`, `libserver.so`/`server.dll`,
  `libtier0.so`/`tier0.dll`, `libengine2.so`/`engine2.dll`). Every caller (`schema.rs`,
  `cvar.rs`, `events.rs`) goes through that table; no file name is hard-coded elsewhere. The
  interface *version* strings are target-independent.
- **Vtable shape comes from the generated bindings.** Anything slot-shaped goes through the
  per-ABI `X_vtable` structs, `X_slots` consts and `VClass` statics; no slot number is written by
  hand. The visible difference is destructors: Itanium gives a virtual destructor two slots
  (`dtor_complete` + `dtor_deleting`), MSVC one (`dtor`, the scalar-deleting destructor). MSVC's
  entry is the compiler-generated `void* (this, unsigned int flags)` wrapper,
  which the generated field models exactly (`gen.py`'s `msvc_dtor_type`; the Itanium entries
  keep the declared `void (T*)`). The Rust listener (`events.rs`) therefore builds the matching
  shape from the same generated struct — `FireGameEvent` is slot 2 on Linux and slot 1 on
  Windows, by construction; its destructor slot is a no-op because the object is never
  destroyed through it (Rust owns the listener end-to-end: unregister first, drop in `Drop`).
- **Calling convention.** Every generated `rust_fn` and `CreateInterfaceFn` is `extern "C"`. On
  `x86_64-pc-windows-msvc` that *is* the MS x64 ABI, which is also the ABI MSVC uses for a
  non-static member function (`this` in `RCX`); there is no separate `thiscall` on x64, so the
  generated `extern "C" fn(this, …)` entries are correct on both targets.
- **`s2sdk-probe` stays Linux-only** (the `LD_PRELOAD` payload for the Linux dedicated server).
  Its `tests/live.rs` needs `cs2-mcp-common`, which itself uses Unix-only file-metadata APIs, so
  a Windows workspace check excludes `cs2-mcp-common`, `client-mcp`, `hammer-mcp`,
  `cs2-vk-capture` and `s2sdk-probe`; the dual-target members are `s2sdk-sys` and `s2sdk`.
  `s2sdk`'s Linux-only tests (`tests/mock_dispatch.rs`, the `module.rs` unit tests) are already
  `#[cfg(not(windows))]`, so `--all-targets` compiles clean on Windows.

**Verified by check only, not at runtime:** everything above is compiled for the MSVC target but
never executed here — no Windows CS2 install is exercised by these tests. The Linux numbers and
behaviour stay live-verified (WP6b/WP7a); the MSVC layouts are additionally pinned by the
generator's static-assert TU and by the Windows vtable-length pins, and the Windows module names
are the conventional Valve install spelling, not a live measurement. Runtime confirmation on
Windows is the bridge's job, not this crate's.

## Generator pipeline (`s2sdk-sys/gen/`)

```
gen.py
  1. protoc (upstream devtools/bin/linux/protoc) -> target/s2sdk-gen/pb/*.pb.{h,cc}
  2. probe.cpp includes the allowlisted headers; clang++ (Linux defines from upstream CMakeLists.txt's `*_build_settings` target;
     the consumer include dirs are gen.py's CONSUMER_INCLUDE_DIRS)
       -Xclang -ast-dump=json            -> declarations, enums, method declaration order
       -Xclang -fdump-record-layouts     -> struct sizes / field offsets (Itanium x86_64)
       -Xclang -fdump-vtable-layouts     -> Itanium vtable slots
  3. the same probe through clang with --target=x86_64-pc-windows-msvc -fms-compatibility
     (MSVC defines from the same CMake target, MSVC CRT/STL + Windows SDK headers from the
     sysroot gen/fetch-xwin.sh fetches into target/xwin):
       -fdump-record-layouts             -> MSVC sizes / offsets
       -fdump-vtable-layouts             -> MSVC vtable slots (this one needs codegen)
       the S2SDK_ probe TU               -> MSVC field and enum sizes
  4. -> s2sdk-sys/ir/<group>.json        (committed; both ABIs in one file, the
                                          reviewable diff on an upstream bump)
  5. a generated TU restates every number in the IR as a `static_assert`
       (sizes, alignments, field offsets/sizes, enum sizes and enumerator values,
       and the `[consts]` expressions/strings)
       and clang++ -fsyntax-only re-derives them from the headers - once with the
       Itanium flags against the top-level numbers, once with the MSVC flags
       against the `"msvc"` sub-objects
  6. -> s2sdk-sys/src/generated/<group>.rs + mod.rs + s2sdk-sys/tests/layout.rs
       (committed; rustfmt'ed with a pinned config so `cargo fmt --check` agrees)
       An item the two ABIs disagree on is emitted twice, behind `cfg(not(windows))`
       and `cfg(windows)`; an item they agree on once, ungated.
gen.py --ir-only     stop after step 4
gen.py --no-verify-cxx   skip step 5
gen.py --check   regenerate both ABIs into target/ and fail if ir/, src/generated/ or
                 tests/layout.rs differ from the committed files
gen.py --self-test   the dump parsers (record layouts, both vtable dumps) and the
                     type mapper, on inline samples (suite: gen/gen_selftest.py,
                     which can also run alone as `python3 -I gen_selftest.py`)
gen.py --abi msvc    measure only the Windows side -> target/s2sdk-gen/msvc/
                     (record layouts, vftable slots, enum sizes; see gen/README.md)
```

`gen/allowlist.toml` lists what is generated, grouped (`tier0`, `schema`, `entity`, `cvar`,
`events`, `engine`, `consts`). Growing the binding = adding a line there and regenerating. Three
further probe.cpp row kinds keep the *hand-written* probe in step with the allowlist:
`S2SDK_FORCE_LAYOUT` / `S2SDK_FORCE_VTABLE` must match it exactly, and
`S2SDK_FORCE_SPEC` / `S2SDK_FORCE_SPEC_FIELD` forces the class-template specializations the IR
spells out (`SchemaMetaInfoHandle_t<T>`, and every `specs` entry) to be instantiated, because a
specialization that nothing needs a *definition* of carries no `definitionData`.

The `consts` group is the one that is not a type table: `[consts]` lists macros and constexprs
whose values clang *evaluates* (a generated probe TU takes each expression as a template
argument, and a string one as a `constexpr auto` read back from the AST), under both ABIs. A
value the two ABIs disagree on fails the run unless the entry says `per_abi = true`; a value read
out of clang's record-layout dump (a bitfield width, say) says `derived` instead of `expr`.

### IR format (`s2sdk-sys/ir/<group>.json`) — the contract between generator, emitter and `abi`

```json
{
  "upstream": { "commit": "<sha from UPSTREAM.lock>", "abis": ["itanium-x86_64", "msvc-x86_64"] },
  "platforms": ["windows"],   // ONLY on a Windows-only group (see below); absent = both platforms
  "records": [
    {
      "name": "SchemaClassFieldData_t",
      "header": "public/schemalib/schemaclassinfo_data.h",
      "kind": "struct",                // struct | class | union
      "size": 32, "align": 8,          // the Itanium numbers; "itanium" restates them
      "opaque": false,                 // true: emit as [u8; size] blob with align, no fields
      "pass_in_registers": true,       // clang's definitionData.canPassInRegisters (Itanium)
      "bases": [ { "name": "Base", "offset": 0, "virtual": false } ],
      "has_vptr": false,
      "fields": [
        { "name": "m_pszName", "offset": 0, "size": 8, "cxx_type": "const char *", "rust_type": "*const ::core::ffi::c_char" },
        { "name": "m_nFlags", "offset": 20, "size": 1, "bit_offset": null, "bit_width": null, "cxx_type": "uint8", "rust_type": "u8" }
      ],
      "itanium": { "size": 32, "align": 8, "pass_in_registers": true, "fields": [ /* name/offset/size/bit_offset/bit_width */ ] },
      "msvc":    { "size": 32, "align": 8, "pass_in_registers": false, "fields": [ /* same shape, MSVC numbers */ ] }
    }
  ],
  "enums": [
    { "name": "EConVarType", "header": "...", "underlying": "short", "rust_repr": "i16",
      "fixed_underlying": true,
      "values": [ { "name": "EConVarType_Invalid", "value": -1 } ],
      "itanium": { "size": 2, "underlying": "short", "fixed": true },
      "msvc":    { "size": 2, "underlying": "short" } }
  ],
  "vtables": [
    {
      "class": "ISchemaSystem",
      "header": "public/schemasystem/ischemasystem.h",
      "primary_base": "IAppSystem",    // null if none; secondary bases listed in "secondary_bases"
      "secondary_bases": [],           // [{name, offset, msvc_offset}] per secondary base
      "msvc": { "length": 40, "own_entries": 29 },   // clang's vftable length (RTTI at vt[-1] excluded)
      "methods": [                     // THIS class's own virtuals in declaration order (not inherited)
        { "name": "FindTypeScopeForModule", "decl_index": 0, "is_dtor": false,
          "overrides": null,           // "Base::Name" when it overrides a base virtual (no new slot)
          "itanium_slot": 13,          // from -fdump-vtable-layouts (dtors: the first of the two)
          "msvc_slot": 13,             // from the same dump under the MSVC target (one dtor slot)
          "cxx_signature": "CSchemaSystemTypeScope *(const char *, int *)",
          "rust_fn": "unsafe extern \"C\" fn(this: *mut ISchemaSystem, module: *const ::core::ffi::c_char, unk: *mut i32) -> *mut CSchemaSystemTypeScope" }
      ]
    }
  ],
  "specs": [
    {                                  // a class-template specialization the flat IR cannot name
      "name": "CUtlVector<CEntityHandle>",   // the way C++ source spells it
      "cxx_name": "CUtlVector<CEntityHandle, int, CUtlVectorMemory_Growable<CEntityHandle>>",
      "rust_name": "CUtlVector_CEntityHandle",
      "template": "CUtlVector",
      "kind": "class", "size": 24, "align": 8, "opaque": false,
      "pass_in_registers": false,
      "bases": [], "has_vptr": false,  // the inherited subobjects are flattened away
      "fields": [                      // the *leaf* members, as one flat list
        { "name": "m_Size", "offset": 0, "size": 4, "cxx_type": "int", "rust_type": "i32" },
        { "name": "m_pMemory", "offset": 8, "size": 8, "path": ["m_Memory", "m_pMemory"],
          "cxx_type": "CEntityHandle *", "rust_type": "*mut CEntityHandle" }
      ],
      "members": [ { "name": "m_Memory", "level": 2, "offset": 8, "bit_width": null, ... } ],
      "itanium": { "size": 24, "align": 8, "fields": [ /* name/offset/size */ ] },
      "msvc":    { "size": 24, "align": 8, "fields": [ /* same shape */ ], "members": [ ... ] }
    }
  ],
  "consts": [
    {                                  // a measured macro/constexpr, not a type
      "name": "MAX_EDICTS", "kind": "int", "type": "u32",
      "value": 16384,                  // the Itanium value; "itanium" restates it
      "expr": "MAX_EDICTS",            // what clang evaluated (or "derived")
      "per_abi": false,
      "itanium": { "value": 16384 },
      "msvc": { "value": 16384 }
    }
  ]
}
```

Rules: offsets/sizes are bytes; slots count 8-byte entries from the address point;
`decl_index` counts only this class's own virtual declarations (overrides included, `#if 0`
excluded). Types the emitter cannot express become `*mut c_void` (pointers) or opaque blobs
(values) — never a guess. `specs[].fields` is the flattened leaf list (a nested member carries
the `path` to spell it in C++, and is asserted through `members`); `consts[].value` is a number
for `kind: "int"` and a string for `kind: "string"`. A `consts` entry measured out of a layout
dump (`"derived": "<record>.<member>.bit_width"`) also carries `derived_path`, the C++ member
path to the bitfield (`["m_Parts", "m_EntityIndex"]`), which is what the static-assert TU
re-measures the width through.

### Windows-only groups

The Workshop Tools modules (toolframework2, hammer, pet, modeldoc_editor) have no Linux
build. A group whose allowlist table says `platforms = ["windows"]` carries
`"platforms": ["windows"]` in its IR file (an unrestricted group has no such key, so
existing IR is byte-identical). Its `"itanium"` numbers are still measured - clang
compiles the header under both ABIs, which proves the header portable - but they claim
nothing about any binary: the Rust is emitted from the MSVC numbers only, its module is
`#[cfg(windows)]` in `generated/mod.rs`, the Itanium half of `tests/layout.rs` never names
it, `[checks]` refuses an Itanium key (`size`, `length`, `itanium_slot`, ...) for it, a group
for both platforms may not refer to one of its types, and `sdkcheck.py --platform linux`
SKIPs it with that reason. `bridge/s2sdk_gen.h` is MSVC-only anyway. Consumers that read
the top-level (Itanium) keys must skip a group whose `platforms` lacks `linux`.

### The second upstream (`metamod`)

The `metamod` group is the one group whose headers do **not** come from `crates/s2sdk/upstream`.
It is generated from a second checkout, [alliedmodders/metamod-source](https://github.com/alliedmodders/metamod-source)
(`master`, Metamod:Source 2.0-dev), which is pinned by `s2sdk-sys/METAMOD.lock`:

```toml
repo = "https://github.com/alliedmodders/metamod-source.git"
branch = "master"
commit = "170058a2f0dbd58106b7c76bfe88a94672295356"
date = "2026-10-18"
khook_repo = "https://github.com/Kenzzer/KHook.git"
khook_commit = "0ca2b01363aff9fa11ad73b1b08dfdc2a195fb3a"
khook_date = "2026-10-18"
```

`khook_commit` is there because `IKHook` is not declared in metamod's own tree: it comes from
metamod's nested `third_party/khook` submodule (`include/khook.hpp`), so a metamod bump that moves
only that submodule still changes the bindings. `ir/metamod.json`'s `upstream` block records both
(`commit` and `khook_commit`), and `gen.py --check` refuses a checkout whose HEAD is not the pinned
revision - the SDK, the metamod checkout and the khook submodule, each naming its lock.

Checking it out (the only init this repository needs; `hl2sdk-manifests` and `third_party/amtl`
are not used and stay uninitialised):

```sh
git submodule update --init crates/s2sdk/upstream     # the SDK, as before
git submodule update --init crates/metamod/upstream   # the second upstream
git -C crates/metamod/upstream submodule update --init third_party/khook
```

A bump is the same three steps as for the SDK: update the checkouts (`git -C crates/metamod/upstream
fetch && git -C crates/metamod/upstream checkout <sha>`, then
`git -C crates/metamod/upstream submodule update --init third_party/khook`), edit
`s2sdk-sys/METAMOD.lock` (both commits and both dates, each the pinned commit's own
`git log -1 --format=%cs`), run `python3 -I s2sdk-sys/gen/gen.py`, and review the `metamod` diff.
The other groups' IR and Rust do not change, and neither claim rests on a group-scoped mechanism:
`probe.cpp` is **one** translation unit for every group, so the metamod defines (`META_NO_HL2SDK`,
`META_IS_SOURCE2`, `metamod_defines`) are set for every clang run of the run and the metamod include
dirs are the **first** entries of its include path (`metamod_first_includes`) - no s2sdk header
mentions either define, so the s2sdk groups read the same headers and see the same numbers. What
keeps that include-path order from changing which file a group reads is
`check_no_shadowed_headers`: it refuses a header path that resolves under both upstreams, checked for
every header under the metamod include dirs against every header under the SDK's consumer include dirs
(both directions, not only the paths `[metamod].headers` lists). It compares headers only
(`HEADER_SUFFIXES`): a README both checkouts happen to have at the same path cannot shadow an
`#include`.

What the group contains: `ISmmPlugin` and `IMetamodListener` (`core/ISmmPlugin.h`), `ISmmAPI`
(`core/ISmmAPI.h`) and KHook's `IKHook` (`third_party/khook/include/khook.hpp`, namespace `KHook`).
They are the interfaces a Metamod plugin is called through and calls back into, so a Rust plugin
needs their slots on both ABIs. Two ABI facts are pinned in `allowlist.toml`'s `[checks]` rather
than assumed: `ISmmPlugin` has a virtual destructor right after `GetApiVersion`
(`ISmmPlugin.h:76-84`), which is **two** Itanium entries and **one** MSVC one, so its length is
17 on Itanium and 16 on MSVC and every later Itanium slot is its MSVC twin + 1; `IKHook` is 15
virtuals with no destructor, so it is 15 on both. `ISmmAPI::GetDetourInterface` is its last
virtual (`ISmmAPI.h:449`), asserted with `last_slot = true`. The generated `metamod.rs` has the
same shape as every other group (`ISmmPlugin_vtable`, `ISmmPlugin_slots` including the WP1.D
`_SRET`/`_RET_SIZE`/`_RET_REGS` consts for a class returned by value, `ISmmPlugin_VCLASS`,
layout tests). The group is deliberately **not** in `bridge/s2sdk_gen.h`: the frozen C bridge
cannot name a metamod type, so it has no consumer there (`C_HEADER_EXCLUDED_GROUPS`).

### The two ABIs in the IR

The top-level `size`/`align`/`fields`/`underlying` and `itanium_slot` are **the Itanium
(Linux x86_64) numbers**: they are what the Rust emitter reads, so the dual-ABI shape did not
change a single byte of `src/generated/`. Beside them, every record and enum carries an
`"itanium"` and an `"msvc"` sub-object with the same shape (the `"itanium"` one restates the
top-level numbers, so both ABIs can be read the same way), every vtable method an
`msvc_slot`, and `upstream.abis` lists the ABIs whose numbers the file holds.

`pass_in_registers` is per ABI: Itanium puts a trivially copyable aggregate of up to 16 bytes in
registers (clang's `canPassInRegisters`), while MSVC passes only a trivially copyable aggregate
of 1, 2, 4 or 8 bytes in a register and anything else by a pointer to a copy; a class **return**
always goes through a hidden pointer, the second argument right after `this`. The top-level
`pass_in_registers` stays the Itanium one, which is what a method's single `rust_fn` is emitted
from; read each ABI's answer from its sub-object. For a by-value **parameter** one `rust_fn` is
sound on both ABIs as long as the record is trivially copyable on both (Rust's `extern "C"`
lowers an aggregate the way each target's C++ member call does), and `gen.py` refuses the run
when that stops being true. A class **return** has its own Windows spelling, `msvc_rust_fn`
(rule 7); it exists even when the record is not trivially copyable (a user copy constructor,
like `CUtlSymbolLarge`'s), where the native `rust_fn` is refused and the Itanium vtable field
stays a raw pointer.

- `msvc_slot` is clang's answer for the same declaration under
  `--target=x86_64-pc-windows-msvc`, from the `VFTable indices` block (the class's RTTI
  pointer is `vt[-1]`, so slot 0 is the first virtual). It differs from `itanium_slot`
  whenever the class has a virtual destructor (MSVC: one entry, Itanium: two, so everything
  after it shifts by one) and inside an overload group, which MSVC packs at the position of
  the name's first declaration in reverse declaration order.
- `enums[].msvc` carries the MSVC size/underlying: a fixed underlying type is honoured, an
  enum without one is `int` there, where Itanium uses the smallest type that fits. (No
  allowlisted enum is unfixed *and* smaller than int today, so the two agree.)
- `records[].msvc` is generated by the same clang dump of the same probe, so a record that
  packs differently on Windows (bitfields, `long`) shows up as a diff here.
- `vtables[].msvc` carries the vftable length clang printed for the class (`length`: the full
  `VFTable` minus its RTTI entry, `null` for a class clang emitted no table for) and how many
  slots the class owns. The generated layout test asserts `abi::vtable_len(Abi::Msvc, …)`
  against it, which is the length the bridge's vtable-length pins assume.
- `consts[].msvc` is the same expression evaluated by the same probe under the Windows target;
  a disagreement with `itanium` fails the run unless the entry is `per_abi`.
- `msvc_name` appears on a method when the Windows headers rename the declaration through a
  charset macro (`IGameEventManager2::CreateEvent` is `CreateEventA` in clang's MSVC view).
  Same declaration, same slot, and the spelling is recorded instead of dropped.
- The generated `tests/layout.rs` asserts every `msvc_slot` against
  `abi::slot(Abi::Msvc, &X_VCLASS, "Name#k")`: clang's dump and the ABI rule in `abi.rs` have
  to agree on all of them, and `abi.rs` is also what the bridge's pins are checked against.
  That check lives in the test file's Itanium module, so it runs on Linux; the `msvc` module
  asserts the same thing for the numbers of the `cfg(windows)` bodies.

A virtual destructor that overrides one on the primary-base chain carries
`overrides: "Base::~Base"` (clang's dump names the *derived* destructor as the slot's owner,
so the owner alone cannot tell the two apart): `IGameEventManager2.~IGameEventManager2` ->
`"IBaseInterface::~IBaseInterface"`, while `ISchemaSystem.~ISchemaSystem` stays `null`
because `IAppSystem` declares no destructor.
A class that declares no destructor of its own still has clang's implicit one, which clang lists
among the class's own vtable entries when a base on the primary chain has a virtual destructor
(`IRenderService`, `IInputService`: the destructor `IEngineService` declares). That entry is the
base's destructor under the derived name, not a new virtual, so it is skipped and the class has no
destructor method in the IR; the base's slot is inherited like any other.

### `rust_type` — what maps, and what stays a blob

`rust_type` is exact-or-`null`; `null` means the emitter writes a `[u8; size]` blob whose
bytes are pinned by the same layout dump. The mapping:

1. Fixed arrays: `T[N]` -> `[T'; N]`, nested arrays recurse. `unsigned char[4]` -> `[u8; 4]`,
   `uint8[16]` -> `[u8; 16]`.
2. Multi-level pointers: `T**` -> `*mut *mut T'`, `const char **` -> `*mut *const c_char`.
   An unknown innermost type gives `*mut ::core::ffi::c_void` per level.
3. References share the pointer ABI: `T&` -> `*mut T'`, `const T&` -> `*const T'`; an
   unknown `T` gives `*mut ::core::ffi::c_void`.
4. `char *` -> `*mut ::core::ffi::c_char`, `const char *` -> `*const ::core::ffi::c_char`.
5. Function-pointer types and typedefs (`CreateInterfaceFn`, `SchemaClassManipulatorFn_t`,
   `FnGetEnumInfoHandle_t`, …) -> `*const ::core::ffi::c_void`: pointer sized, the signature
   is not carried (`cxx_signature` keeps it).
6. Typedef/enum desugaring through the AST: a scalar typedef (`SpawnGroupHandle_t` = `uint32`,
   `UtlSymLargeId_t`) becomes the scalar, an enum outside the allowlist
   (`EntityFlags_t : uint32`) becomes its fixed underlying integer (`u32`; with no fixed
   underlying type, `null`), an unknown record stays `null`.
7. A record passed or returned **by value** maps only when it is in the IR *and* clang's
   `definitionData.canPassInRegisters` is true; otherwise the method's `rust_fn` is `null`
   (the caller cannot model the call). A record behind a pointer is unaffected. **Returns are per
   ABI.** MSVC x64 returns *every* class/struct/union (a `SchemaMetaInfoHandle_t<T>` too, which
   maps to `*mut T'`) from an instance method through a hidden pointer that is the second
   argument, right after `this`; the callee returns that pointer in RAX, whatever the record's
   size (clang: `sret` second parameter for 4, 8, 16, 24 bytes, trivial or not). Itanium returns a
   trivially copyable record of up to 16 bytes in registers and passes the `sret` of a larger one
   in RDI **before** `this`; Rust's `extern "C"` on Linux does exactly that, so `rust_fn` is the
   Linux form. Rust's `extern "C"` on Windows would return a record of 1, 2, 4 or 8 bytes in RAX
   and put a larger one's `sret` before `this`, so it cannot spell the MSVC call: such a method
   carries a second signature, `msvc_rust_fn`
   (`unsafe extern "C" fn(this, ret: *mut T, args...) -> *mut T`), and the `cfg(windows)` vtable
   uses it. A caller writes `cfg(windows)` code that passes a `MaybeUninit<T>` for `ret` (see
   `cvar.rs`, `Cvar::find`). Parameters need no such form. Enums and pointers are returned in
   RAX on both ABIs. Probes and the clang IR that fix this: `gen_selftest.py` (checks run clang
   on them).
8. `SchemaMetaInfoHandle_t<T>` by value -> `*mut T'` (`*mut ::core::ffi::c_void` for an
   unknown `T`), and only after the record-layout dump confirms the specialization is 8 bytes
   with a single `m_pObj` at 0 and the AST confirms `canPassInRegisters`.

A mapped type's *width* is checked against the dump as well: a field whose Rust type is not
exactly as wide as clang says fails the run instead of silently moving every field after it.

### Rust shape (`s2sdk-sys/src/generated/`)

One file per group (`<group>.rs`) plus a `mod.rs` that re-exports them; cross-group names
resolve through `use super::*;`. Everything is `#[repr(C)]` with the dump's alignment, so
`align_of` and every field offset are asserted in `tests/layout.rs`.

The file holds **both targets' numbers**. An item whose emitted body the two ABIs agree on
(a record whose size/alignment/offsets/bitfields are the same, an enum, an untangled vtable
struct) is emitted once and ungated; an item they disagree on is emitted twice, the Itanium
body behind `#[cfg(not(windows))]` and the MSVC one behind `#[cfg(windows)]`. What differs
today is entirely in the vtable structs: MSVC gives a virtual destructor one slot where
Itanium gives two, and packs an overload group reversed at the first declaration's slot,
which moves a field's place in the struct. Every allowlisted record, specialization and
enum is laid out identically by both ABIs (their `pass_in_registers` answer is not, which is
what the IR's per-ABI sub-object records).

- **Records** (`opaque: false`): `#[repr(C, align(<align>))] #[derive(Copy, Clone)] pub
  struct Name { … }`. Fields in offset order; a `pub _padN: [u8; gap]` fills every gap and a
  trailing pad reaches `size`, so correctness never depends on Rust's own padding. `has_vptr`
  becomes a first `pub vtable_: *const c_void` (a record that is also a vtable class uses
  `pub vtable`, the name its accessor reads); bases become `pub base: Base` (further bases
  `pub base_<Name>`); a `null` `rust_type` becomes `pub name: [u8; size]`; bitfields of one
  storage unit merge into one `pub _bitsN: [u8; n]` blob; a virtual base makes the whole
  record an opaque blob. Unions are `#[repr(C, align(N))] #[derive(Copy, Clone)] pub union`
  under the same rules.
- **Opaque**: `#[repr(C, align(A))] #[derive(Copy, Clone)] pub struct Name(pub [u8; size]);`
- **Specializations** (`specs[]`, emitted under the `rust_name` tag): the same shape as a
  record, with the inherited members flattened into one `#[repr(C, align(N))] pub struct`.
- **Enums**: a newtype, never a Rust `enum` (the game can hand back values outside the list):
  `#[repr(transparent)] #[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)] pub struct
  EConVarType(pub i16);` with one `pub const` per enumerator.
- **Vtable classes** (`vtables[]`, emitted per ABI from that ABI's `*_slot` numbers):
  a `#[repr(C)] pub struct X { pub vtable: *const X_vtable }` plus
  `impl X { pub unsafe fn vtable<'a>(this: *const Self) -> &'a X_vtable }` for a class that is
  not also a record; `#[repr(C)] #[derive(Copy, Clone)] pub struct X_vtable { pub base:
  Base_vtable, … }` with this class's own new-slot methods at their **slot number** (entries
  are `Option<unsafe extern "C" fn(…)>`, or `*const c_void` when the IR has no `rust_fn`; a
  destructor is `dtor_complete` + `dtor_deleting` on Itanium (both `void (T*)`, as declared) and a
  single `dtor` on MSVC (the scalar-deleting wrapper, `void* (T*, unsigned flags)`); an
  override gets no field because the base field owns the slot; overloads are `Name`, `Name_1`,
  `Name_2`, so under MSVC their `Name_1` may come first in the struct - its slot is the lower
  one; a slot nothing is bound to becomes a `_padN: [u8; n]` blob); an `X_vtable::NULL` const
  for `..X_vtable::NULL` partial initialisation; and `pub mod X_slots` with one
  `pub const NAME: usize` per own **and inherited** slot (`dtor` for the destructor).
- **VClass statics** (all targets, consumed by `abi`): `pub static X_VCLASS: VClass = VClass
  { name, primary_base: Some(&Base_VCLASS), methods: &[VMethod { … }] }`, methods in
  `decl_index` order with `overrides` copied verbatim. A `primary_base` that is not in the IR,
  or a method overriding a virtual of a secondary base (searched through the whole primary chain),
  is a generator error (`secondary_bases` alone is fine: the vtable is the primary table, and a
  secondary base has its own IR vtable). Each secondary base also gets `impl X { pub const
  BASE_OFFSET_<Base>: usize; pub unsafe fn as_<base>(this: *mut Self) -> *mut <Base> }` with the
  per-ABI offset, and the C header gets `S2_BASEOFF_<Class>_<Base>`; callers never hand-write a
  base offset.
- **Consts** (`src/generated/consts.rs`, all targets): one `pub const NAME: <type> = <value>;`
  per `[consts]` entry, with the entry's `doc` as its doc comment. An entry the ABIs disagree
  on is emitted twice, behind `#[cfg(not(windows))]` and `#[cfg(windows)]`.

`tests/layout.rs` is generated from the same IR and closes the loop, once per ABI: clang's
layout dump -> the `#[repr(C, align(N))]` struct -> the slot rule in `abi`. It carries one
`mod` per ABI (`itanium`, `msvc`), each behind the same `cfg` the bodies it asserts are
emitted behind, and each asserts `Abi::native()` - its own ABI - so a Linux test build checks
the Itanium numbers and a Windows one the MSVC numbers. For every record and specialization
it asserts `size_of`, `align_of` and `offset_of!` of every field, base and pad; for every
vtable class it asserts the struct size against `abi::vtable_len(Abi::native(), …) * 8`, the
offset of every member (base, entry and pad) against its slot `* 8`, and `X_slots::NAME ==
slot` == `abi::slot(Abi::native(), &X_VCLASS, "Name#k")` for every own and inherited method.
Every module also restates its vtable sizes and member offsets in one `const _: () = { … }`
block: `cargo check` const-evaluates it, so the half whose tests cannot run on the machine
doing the checking (the MSVC one on a Linux host) still proves its layout -
`cargo check --target x86_64-pc-windows-msvc -p s2sdk-sys --all-targets` fails on a Windows
struct that is one slot short. The Itanium module (the one Linux CI runs) additionally
reproduces every `msvc_slot` through `abi::slot(Abi::Msvc, …)` and `abi::vtable_len(Abi::Msvc,
&X_VCLASS)` against the length clang printed for that class's Windows vftable, and the `msvc`
module asserts those lengths for its own target.

## Live findings (WP6b probe, Linux dedicated server)

`s2sdk-probe` (WP6b) runs its check sequence on a detached thread inside a real
`LinuxDedicated` session and writes `s2sdk-probe.json` next to `server.log` in the session's
artifacts dir; `s2sdk-probe/tests/live.rs` asserts `RESULT ok stage=sdk`, the mandatory checks
(`schemasystem`, `schema_system`, `schema_scope`, `schema_class`, `cvar`) and prints the JSON.
The facts below are from that passing run — each one is also a `data` object in the report:

- **Schema system.** `SchemaSystem_001` is created from `libschemasystem.so` and
  `SchemaSystemIsReady()` is true ≈120 ms after the process starts (one poll).
- **Server type scope.** Registered under `"libserver.so"` (`GetScopeName()` is the same
  string); it appears ≈1.6 s into the boot, when `libserver.so` loads with the map. `"server"`
  and the Windows `"server.dll"` do not resolve — the Linux `libserver.so` does carry
  `"server.dll"` (one module-name accessor), but the Linux scope is not keyed under it.
- **`FindTypeScopeForModule` never returns null on a miss.** It hands back the *global* scope
  (`GetScopeName()` = `"!GlobalTypes"`), so a name probe cannot tell "registered" from "not
  registered yet" by the pointer alone: ask `TypeScope::is_global_scope()` (or poll until it is
  false). Before that was fixed, the probe "found" the server scope at 122 ms and then spent the
  whole deadline looking for `CBaseEntity` in the global scope.
- **Schema reads (`CBaseEntity`, server scope).** size 1928 B, 85 declared fields, all named and
  all with an offset inside the class; `m_iHealth` at 1456. First five declared fields:
  `m_CBodyComponent` @48 (`CBodyComponent*`), `m_NetworkTransmitComponent` @56
  (`CNetworkTransmitComponent`), `m_aThinkFunctions` @1280 (`CUtlVector< thinkfunc_t >`),
  `m_iCurrentThinkContext` @1304 (`int32`), `m_nLastThinkTick` @1308 (`GameTick_t`). The
  `CCSPlayerPawn` base chain reaches `CBaseEntity` in six steps: `CCSPlayerPawn` →
  `CCSPlayerPawnBase` → `CBasePlayerPawn` → `CBaseCombatCharacter` → `CBaseAnimGraph` →
  `CBaseModelEntity` → `CBaseEntity`.
- **Convars.** `VEngineCvar007` is served by **`libtier0.so`** (the module also carries the
  version string; `libengine2.so` exports `CreateInterface` but not that name). Value
  conventions on an unmodified dedicated server (`de_dust2`, casual): `sv_cheats` is
  `EConVarType_Bool` `false`; `mp_roundtime` is `EConVarType_Float32` `5.0` (minutes); an unknown
  name comes back as `Error::NotFound` through the invalid-access-index path, never as a null
  data record.
- **Game events (non-mandatory check).** `GAMEEVENTSMANAGER002` (the spelling Valve's own
  modules use) is **not** served by any module the Linux dedicated server loads — `libengine2.so`,
  `libserver.so` and `libtier0.so` do not contain the string; only `libclient.so` and
  `libmatchmaking.so` do. Strategy A is therefore a dead end on the server and the probe reports
  `events: ok:false` as a finding; the listener path needs strategy B (the `ISource2Server`
  slot-1-relative lookup documented in `s2sdk/src/events.rs`).
- **Threading.** Everything above is a read-only lookup (schema, cvar, event-descriptor) from a
  non-game thread. `AddListener`/`RemoveListener` are deliberately *not* called: the engine's
  listener list is not thread-safe.

## Tests

```sh
cargo test -p s2sdk-sys -p s2sdk                 # no game needed
cargo clippy --workspace --all-targets           # no game needed
cargo check  --target x86_64-pc-windows-msvc -p s2sdk-sys -p s2sdk --all-targets  # the win64 half
python3 -I s2sdk-sys/gen/gen.py --check          # bindings match the pinned upstream
cargo test -p s2sdk-probe --test live -- --ignored   # live: LinuxDedicated in podman
```

The `s2sdk` protobuf tests need `protoc`; it comes from the upstream checkout
(`devtools/bin/linux/protoc`), located automatically or via `S2SDK_DIR`.
