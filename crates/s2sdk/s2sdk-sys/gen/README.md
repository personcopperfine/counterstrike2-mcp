# s2sdk-sys generator

`s2sdk-sys` is generated, never hand-written: these files turn the pinned
upstream headers (`../UPSTREAM.lock`, the `crates/s2sdk/upstream` submodule) into
`../ir/*.json`, `../src/generated/*.rs` and `../tests/layout.rs`.

| file | what it is |
|---|---|
| `gen.py` | the generator (stdlib only; run it as `python3 -I gen.py`) |
| `gen_selftest.py` | its self-test suite (`gen.py --self-test`, or `python3 -I gen_selftest.py` alone) |
| `allowlist.toml` | what is generated, grouped `tier0`/`schema`/`entity`/`cvar`/`events`/`engine`/`toolframework2`/`rendersystem`/`scenesystem`/`soundsystem`/`particles`/`client`/`datamodel`/`toolutils2`/`pet`/`modeldoc_editor`/`mapdoclib`/`hammer`/`metamod`/`consts` |
| `probe.cpp` | the translation unit clang is pointed at; it *uses* the allowlisted types, because clang only prints a layout or a vtable for a type it was asked to lay out |
| `fetch-xwin.sh` | fetches the Windows sysroot the `--abi msvc` pass needs into `<repo>/target/xwin` |

Besides `../ir/*.json` and the Rust bindings, a full run (or `--check`) also emits
`crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h` from the IR's **msvc** side: the flat macro set
(`S2_SIZEOF_`/`S2_ALIGNOF_`/`S2_OFF_`, `S2_SLOT_`/`S2_VTLEN_`,
`S2_CONST_`/`S2_IFACE_`) and padded structs for the records the C bridge read
member-wise. The C bridge that compiled it is retired (its source is in git history); the Rust
bridge reads `s2sdk_sys` directly. The header stays because it is what a sigs entry's `sdk`
field is evaluated against: `sigs/scripts/sigcheck.py` (`sdk_defines`/`sdk_eval`), `sdkcheck.py`,
`sdkadopt.py` and the pins generator read its macros, and the entry is the binary measurement
that holds a generated number to the game (`sigs/README.md`).

The syntax `sdk` entries use is gen.py's former tie syntax and stays here: a plain macro,
`S2_SLOT_X*8` for a pin that is a vtable *byte offset* (pet's `call [rax+0C0h]`;
`slot_bytes("S2_SLOT_X_Y")`, read back by `split_tie_scale()`, only `S2_SLOT_*` with the
factor 8), `S2_OFF_X+8`, and `S2_OFF_A+S2_OFF_B` (`tie_terms()`); `sigcheck.sdk_eval`
evaluates it against the emitted header. The tie tables, the late-ties header
(`s2sdk_gen_ties.h`) and the name-disjointness scan of the bridge sources went with the C bridge.
The header is freestanding C (no `#include`; written against `u8/u16/u32/u64` typedefs of the
MSVC x64 widths, `C_SMOKE_TYPEDEFS`), committed but never hand-edited; `gen.py --check` (in the
AGENTS.md checks list) verifies it byte for byte and smoke-compiles it with clang on the MSVC
target (`-ffreestanding -fsyntax-only -Werror=macro-redefined`), so every padded struct's
`_Static_assert`s run.

`gen.py --self-test` exercises the dump parsers and the emitters on inline
samples (the suite lives in `gen_selftest.py`, next to `gen.py`; it can also be
run alone as `python3 -I gen_selftest.py`); `gen.py --check` regenerates into
`target/` and fails if anything committed differs. Do not edit `../ir/`, `../src/generated/` or
`../tests/layout.rs` by hand - change the headers or the allowlist and
regenerate. See `../README.md` for the IR contract.

## The second upstream (`metamod`)

`crates/metamod/upstream` (Metamod:Source 2.0-dev, pinned by `../METAMOD.lock`
together with its `third_party/khook` submodule) is a *second* header source. The
`metamod` group is the four plugin-facing interfaces it declares: `ISmmPlugin`
and `IMetamodListener` (`core/ISmmPlugin.h`), `ISmmAPI` (`core/ISmmAPI.h`) and
KHook's `IKHook` (`third_party/khook/include/khook.hpp`).

What makes it different from every other group, and where each difference is
handled:

* **Two checkouts, one TU.** The group's include dirs come *before* the s2sdk
  ones (`metamod_first_includes`), and its defines are appended to the SDK's
  (`metamod_defines`): `META_NO_HL2SDK` skips the hl2sdk includes this repository
  does not carry (`ISmmAPI.h:40-61`, `ISmmPlugin.h:41-47`) and `META_IS_SOURCE2`
  selects the Source 2 (CS2) reading of what is left. `playerslot.h` - an *s2sdk*
  header - is the group's first header, because `ISmmAPI.h:68-70` constructs a
  `CPlayerSlot` under `META_IS_SOURCE2`. `check_no_shadowed_headers` refuses a
  header path both upstreams would resolve - it compares *every* header
  (`HEADER_SUFFIXES`) under the metamod include dirs with every header under the
  SDK's consumer dirs, in both directions, not only the allowlisted headers -
  because a collision would make which file clang read depend on that order and
  the IR's header field name a file that is in only one upstream. A non-header
  both checkouts carry at the same path (both have a `README.md`) cannot shadow an
  `#include`, so it is not a collision. Override the checkout with
  `$S2SDK_METAMOD_DIR` (the same role `$S2SDK_DIR` has), and `$S2SDK_XWIN` is
  still the Windows sysroot. The nested khook submodule is checked for being a
  checkout at all (`khook_checkout_missing`, asked of its `.git`): a git command
  in an uninitialised submodule reports the *metamod* repository's HEAD, which
  would satisfy `METAMOD.lock`'s separate khook pin with the wrong revision.
* **Names clang spells qualified.** The AST's `name`, the allowlist and the IR are
  unqualified, but clang prints a class in a namespace with that namespace in the
  record-layout dump (`class SourceMM::ISmmPlugin`), the vtable dumps (`VTable for
  'SourceMM::ISmmPlugin'`, `bool SourceMM::ISmmPlugin::Load(...)`) and - inside
  another namespace - in a signature (`KHook::Action`, `std::size_t`).
  `bare_type_name`/`strip_namespace`/`bare_keyed`/`same_class` are the whole of
  the handling: clang's record-layout and vtable dumps are re-keyed by the bare
  name (failing on a name two classes would share), and
  `TypeMapper.resolve_known` falls back to the bare name when only that spelling
  is in the tables. A *leading* namespace only, so a template-id's own `::` is
  part of the name.
  The AST's **enums and typedefs are keyed by their qualified name**
  (`AstSink.enums`/`typedefs`; `ast_walk` tracks the enclosing namespace,
  record and enum), because a table keyed by the bare name answers with
  whichever declaration clang dumped first: the second upstream pulls in
  libstdc++/the MSVC STL, where `IndexType_t` has 8 declarations and `BaseClass`
  25, so e.g. a new s2sdk global `Action` would silently take over `KHook::Action`
  in IKHook's signature. `AstSink.rekey` then gives the mapper an exact key per
  spelling and a bare alias only while exactly one declaration carries the bare
  name; `TypeMapper.resolve_known` refuses the bare names in
  `AstSink.ambiguous`, so a qualified spelling that is no declaration of its own
  (`KHook::Action`, when only `A::Action` and the global `Action` exist) does not
  resolve at all - the method's type fails closed, exactly as an unknown name
  did before the second upstream existed, instead of landing on the wrong
  declaration.
* **Not in the C header.** `crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h` is generated from
  `c_header_groups()`, which excludes the group (`C_HEADER_EXCLUDED_GROUPS`): the
  header is frozen at the s2sdk IR the retired C bridge consumed, and no sigs `sdk`
  expression names a metamod type, so a `S2_SIZEOF_ISmmAPI` would be a change to a
  checked-in generated header for no consumer. The same holds per `[consts]` entry: a value measured from the
  metamod headers is listed in `C_HEADER_EXCLUDED_CONSTS`, so
  `S2_CONST_METAMOD_PLAPI_VERSION` is never emitted (`c_header_consts`), while the
  entry stays in `consts.json`/`consts.rs` for the Rust plugin. Everything else is
  the same: `ir/metamod.json`,
  `src/generated/metamod.rs`, the `mod.rs` re-export and the layout tests. Its
  `upstream` block records the metamod commit *and* `khook_commit`, because
  IKHook's slots come out of the submodule, and `metamod.rs`'s header stamps the
  two (`record_header`).
* **`[checks]` with a relation.** `metamod.ISmmPlugin` pins
  `{ length = 17, msvc_length = 16 }`: the class declares a virtual destructor
  right after `GetApiVersion`, which is two Itanium entries and one MSVC one, so
  every later Itanium slot is one past its MSVC twin. The generated tests assert
  both numbers *and* that relation (`checks_pinned_vtable_test`); a
  `last_slot = true` method key asserts the method's slot is the vtable's last
  entry on both ABIs (`_check_last_slot`).
* **Nothing group-specific past the headers.** Once the two checkouts are on the
  include path the group goes through the same pipeline as every s2sdk group:
  `attach_return_transports` walks all of `GROUPS`, so a metamod method that
  returns a class by value gets its `"ret"` object (and the emitter its
  `_SRET`/`_RET_SIZE`/`_RET_REGS` consts) exactly like a schema one. No metamod
  method does today - the interfaces return `void`, `bool`, `int`, pointers,
  `HookID_t`/`size_t` and `CreateInterfaceFn` - so `metamod.rs` carries no return
  consts yet; a header bump that adds one needs no generator change.

Checking the second upstream out (only these two, in this order; the nested
`hl2sdk-manifests`/`third_party/amtl` submodules are not initialised):

```sh
git submodule update --init crates/metamod/upstream
git -C crates/metamod/upstream submodule update --init third_party/khook
```

`METAMOD.lock` (next to `UPSTREAM.lock`) pins both commits:

```toml
repo = "https://github.com/alliedmodders/metamod-source.git"
branch = "master"
commit = "170058a2f0dbd58106b7c76bfe88a94672295356"
date = "2026-10-18"
khook_repo = "https://github.com/Kenzzer/KHook.git"
khook_commit = "0ca2b01363aff9fa11ad73b1b08dfdc2a195fb3a"
khook_date = "2026-10-18"
```

`ir/metamod.json`'s `upstream` block restates both (`"commit"` and
`"khook_commit"`, beside `"abis"`), so a reader of the IR knows which two
revisions the slots describe without opening the lock.

The negative check for the whole second-upstream path is a mutated copy of the
checkout:

```sh
cp -r crates/metamod/upstream target/scratch/metamod-mutated
$EDITOR target/scratch/metamod-mutated/third_party/khook/include/khook.hpp  # swap two IKHook virtuals
S2SDK_METAMOD_DIR=$PWD/target/scratch/metamod-mutated python3 -I gen.py --check
# -> fails, naming IKHook: the committed ir/metamod.json no longer matches
#    e.g. "[checks] metamod.IKHook.GetOriginalFunction: itanium_slot is 5, expected 4"
#    (after swapping GetOriginalFunction and GetOriginalValuePtr)
```

`cp -r` keeps the checkout's `.git` files, whose `gitdir:` paths resolve from the
copy's new location the same way they did from the original (both are six and
eight levels below the worktree root), so the copy still passes the lock check
and the mutation is the only difference the run sees. A copy that *drops* them (a
`cp -r crates/metamod/upstream/* ...`, say) fails before clang runs instead:
`khook_checkout_missing` sees no `third_party/khook/.git` and names the init
command (see the first bullet), rather than letting git answer the metamod
repository's revision for the khook submodule.

`--check` (and the run itself) also compares each checkout's HEAD against
`UPSTREAM.lock` / `METAMOD.lock` (`locked_heads`, `lock_mismatches`); a wrong hash
in either lock file fails `--check` with exit 2, naming the lock. An
uninitialised khook submodule fails the run before that, naming the init command
(`khook_checkout_missing`; see the first bullet above).

The generated files say which checkpoint of the two upstreams they describe:
every file but the metamod group's stamps `UPSTREAM.lock`'s commit, and
`src/generated/metamod.rs` stamps `METAMOD.lock`'s metamod **and** khook commits
(`record_header`), because its slots come from both checkouts.

## The two ABIs

`ir/<group>.json` carries both: the Itanium (Linux x86_64) numbers (also the
top-level keys) and the MSVC (Windows x64) ones the bridge is compiled with.
Every record and enum has an `"itanium"` and an `"msvc"` sub-object
(size/align/fields, size/underlying), every vtable method an `msvc_slot` next to
its `itanium_slot`, and `upstream.abis` names both. The top-level keys stay the
Itanium numbers, byte for byte, so a consumer that does not care about the ABI
reads them and a dual-ABI diff is additions only.

The Rust emitter reads a sub-object, never the top-level half, so it emits **the
right numbers for both targets**: a record, specialization or vtable whose body
the two ABIs agree on is emitted once, ungated; an item they disagree on is
emitted twice, the Itanium body behind `#[cfg(not(windows))]` and the MSVC one
behind `#[cfg(windows)]`. What differs is the vtable structs (MSVC's one-slot
destructor, its reversed overload groups) and, in the IR only, the
`pass_in_registers` answer, which reaches the emitted code through the `rust_fn`
of a method that passes a record by value. `gen.py` refuses the run when such a
method's two ABIs disagree on the triviality of a by-value parameter. A class
**returned** by value is spelled per ABI instead: the method's `rust_fn` is the
Itanium (and rustc-on-Linux) form, and a method with a class return also has an
`msvc_rust_fn` with MSVC's hidden return pointer as an explicit second
parameter, which the `cfg(windows)` vtable uses (see `../README.md`, rule 7). `tests/layout.rs` is
generated the same way: one `mod` per ABI, each gated on its own `cfg` and each
asserting its own ABI's numbers against `abi::slot(Abi::native(), ..)`.

The same facts exist machine-readably: a method whose return is a class by
value carries a `"ret"` object,
`{"itanium": {"sret": bool, "size": N, "regs": "int"|"sse"|"mixed"|"memory"},
"msvc": {"sret": true, "size": N}}`, and every `X_slots` module emits the same
numbers as consts (`Foo_SRET` / `Foo_RET_SIZE`, plus `Foo_RET_REGS` on
Itanium). The rules, read back from clang's `-S -emit-llvm` output (the
self-test proves them on 8-, 12-, 24-byte and non-trivial probes): MSVC
returns **every** class through the hidden pointer after `this` (`sret: true`,
even a 4-byte record); Itanium returns a trivially copyable record of at most
16 bytes in registers (`regs` says how - `"memory"` is the sret case) and gives
`sret` to anything else. A return that is not a class by value gets no `"ret"`
object and no consts. A record whose transport gen.py cannot decide (a union,
a derived record, an empty record, a field type with no class, an opaque record
with no layout dump) fails the run naming the method - it never guesses.

Both halves come from clang on the same probe, with upstream's own flags (its CMake
`${S2SDK_TARGET_PREFIX}_build_settings` target for the standard and defines, plus
gen.py's `CONSUMER_INCLUDE_DIRS` for the include dirs a consumer of the headers gets):

```sh
./s2sdk/s2sdk-sys/gen/fetch-xwin.sh            # once, from the repo root
python3 -I crates/s2sdk/s2sdk-sys/gen/gen.py          # both ABIs, then the Rust emit
python3 -I crates/s2sdk/s2sdk-sys/gen/gen.py --check  # both ABIs, fail on any diff
python3 -I crates/s2sdk/s2sdk-sys/gen/gen.py --abi msvc       # measure the MSVC side only
```

`fetch-xwin.sh` installs `xwin` and downloads Microsoft's Windows SDK + MSVC
CRT/STL from Microsoft's servers into `<repo>/target/xwin` (gitignored, like
everything under `target/`). It passes `xwin --accept-license`, i.e. it accepts
Microsoft's license for you: run it yourself, or point `$S2SDK_XWIN` at a
sysroot you already have, if you would rather accept that yourself. `gen.py`
fails with the script's name before clang runs when the sysroot is missing -
every run needs it, because every committed IR file holds both ABIs.

The MSVC pass is three clang runs on the same `gen/probe.cpp`:

| pass | flag | what it gives |
|---|---|---|
| record layouts | `-fsyntax-only -Xclang -fdump-record-layouts` | size, align, field offsets, bit positions |
| vftable | `-S -Xclang -fdump-vtable-layouts` | one slot per method from the address point |
| sizes | the `S2SDK_` probe TU, `-ast-dump-filter=s2sdk_` | field sizes, enum sizes, the `[consts]` values |

The vftable pass needs codegen (clang only prints a vtable it emits), so unlike
the other two it is not `-fsyntax-only`. It is a text dump on purpose: the JSON
AST of the full probe is ~782 MB, and it has no slots in it anyway.

The MSVC probe TU reaches private members with `-fno-access-control` instead of
the Itanium TU's `#define private public`: the MSVC STL's `xkeycheck.h` turns a
`private` macro into a hard error.

## Windows-only groups (`platforms`)

A group table may say `platforms = ["windows"]` (default: both; `["windows"]` is the only
restriction supported; the `consts` group may not carry it). Use it for a group whose
classes exist in no Linux build - the tool modules (toolframework2, hammer, pet,
modeldoc_editor). To add one: add the group name to `GROUPS` in gen.py, a `[group]` table to
`allowlist.toml` with `platforms = ["windows"]` and the usual `headers`/`records`/`opaque`/
`enums`/`vtables` keys (probe.cpp rows as for any group), and regenerate. What changes:

- measuring: both ABIs still run (the Itanium pass proves the header compiles under
  both clang ABIs), and the Itanium numbers stay in the IR - but they are a portability
  proof, not a claim about any binary;
- the IR file gets `"platforms": ["windows"]` (no key for an unrestricted group);
- `src/generated/<group>.rs` is emitted from the MSVC numbers only (no
  `cfg(not(windows))` body), `mod.rs` gates `mod <group>;` and `pub use` on `cfg(windows)`,
  and `tests/layout.rs` leaves the group out of its Itanium module;
- `[checks]` keys for the group may use only the `msvc_*` fields (`msvc_size`, `msvc_slot`,
  `msvc_length`, ...) and `decl_index`; an Itanium key is a generator error;
- a group for both platforms whose record/vtable names a type of a Windows-only group
  (base, field type, method signature) is a generator error;
- `crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h` is built from the msvc side only, so it needs nothing;
- `sdkcheck.py --platform linux` SKIPs the group (`group:<name>` and its `vt:` rows)
  with the reason; the windows lane checks it like any other group.

Granularity is the group on purpose: an item-level flag would need per-item cfg handling
across records, specs and vtable chains, and a group costs one table.

## The `consts` kind

`[consts]` is the one allowlist table that is not a list of types: it lists the
macros and constexprs a consumer would otherwise re-spell by hand (a bit count,
an ehandle sentinel, an interface version string, a `CUtlStringToken` hash), and
`gen.py` never enters their values. Each entry is measured by clang, under both
ABIs, in the generated probe TU:

- `{ name = "X", expr = "<C++ expression>", type = "u32" }` - the expression
  becomes a non-type template argument (`template struct
  s2sdk_const_probe<struct s2sdk_const_X, (long long)(<expr>)>;`), so the
  compiler folds it and the value comes back from the JSON AST. The entry's
  `type` is the Rust width it is emitted with, and a measured value that does not
  fit it fails the run.
- `{ name = "X", expr = "<macro>", kind = "string" }` - a template argument
  cannot carry a string, so the probe TU defines `constexpr auto <tag> = (<expr>)`
  in a namespace and the generator reads its `StringLiteral` out of the AST.
- `{ name = "X", derived = "<record>.<member>.bit_width", type = "u32" }` - the
  value comes out of clang's record-layout dump instead of an expression, which
  is the only way to have a bitfield's *width* (`CEntityHandle::m_EntityIndex`
  is 15 bits). `transform = "mask"` turns a width into `(1 << width) - 1`, and
  `per_abi = true` is required for an entry whose two ABIs may disagree.

Every value also lands in the static-assert TU (§ verify), so the compiler
re-derives it: `static_assert(((long long)(<expr>)) == <value>)` for an int, and
a character-by-character comparison (terminator included) for a string. A
`derived` entry has no C++ expression for its number, so the IR also carries
`derived_path`, the C++ member path to the bitfield (`["m_Parts",
"m_EntityIndex"]`), and the verify TU sets every bit of that field in a copy of
its enclosing subobject and counts what the field kept (`[checks]` pins the
same number a second time).

The values are emitted into `src/generated/consts.rs` (all targets; a `per_abi`
entry becomes two `cfg`-gated consts) and their expectations live in `[checks]`
under the `consts` group, where `value` may be a number or a string. They are also
emitted into `crates/s2sdk/s2sdk-sys/include/s2sdk_gen.h` as `S2_CONST_<name>` (`S2_IFACE_<name>` for a
string whose name ends in `_INTERFACE_VERSION`), except the names in
`C_HEADER_EXCLUDED_CONSTS`: a value measured from the **second upstream**
(`METAMOD_PLAPI_VERSION`, `METAMOD_PLAPI_NAME` from `core/ISmmPluginExt.h`,
`INVALID_HOOK` from `khook.hpp`) is out of that frozen header for the same reason
the `metamod` group is (`C_HEADER_EXCLUDED_GROUPS`). `[consts].headers` lists the
headers those expressions need - including the second upstream's, because the
group is measured in the same probe TU (see the include block in `probe.cpp`).

## Specializations (`specs`)

The flat IR cannot name a C++ template-id, so a class-template specialization is
listed per group under `specs` and emitted under a Rust-safe tag
(`CUtlVector<CEntityHandle>` -> `CUtlVector_CEntityHandle`), with probe.cpp
forcing its definition (`S2SDK_FORCE_SPEC` / `S2SDK_FORCE_SPEC_FIELD`; the checker
fails the run if a row is missing, misspelled, or forces nothing).

A specialization inherits everything it has, so its IR `fields` are the *leaf*
members clang laid out, flattened into one list (`CUtlVector<CEntityHandle>`:
`m_Size` at 0 and the allocator's `m_pMemory`/`m_nAllocationCount`/`m_nGrowSize`).
A nested leaf carries the `path` to spell it in C++ (`["m_Memory", "m_pMemory"]`),
which is what the probe, the static assertions and `[checks]` use; the full
nested list stays in `members`. Two leaves with the same name are a generator
error rather than a silently wrong struct.

`--abi msvc` on its own measures the Windows side and writes, under
`target/s2sdk-gen/msvc/`:

- `<group>.json` - one file per allowlist group, holding that group's
  allowlisted records. Each record has `size`, `align`, `has_vptr`, `bases`,
  `fields` (the direct members: name, type, offset, bit position) and `members`
  (every member clang printed, nested ones included, with `level`).
- `records.json` - every record clang laid out for the probe, Windows CRT/SDK
  internals included, sorted by name. For reading and diffing by hand only:
  nothing builds on it, and it has no Itanium counterpart.
- `facts.json` - the vftable, specialization and `[consts]` side: each class's
  own and full entry counts, the `Name#k -> slot` map, each enum's measured size,
  and the values the Windows target measured. This is the human-readable form of
  the numbers that go into `ir/`.

`--abi msvc` reads the committed `ir/` for the declaration order it matches the
dump against, so run a full `gen.py` (or `--check`) first when the headers moved.

All three files carry a header with the upstream commit, the ABI, the clang
version and the sysroot. Because neither the sysroot nor clang's MS compatibility
version (`_MSC_VER`) is pinned, the header is what tells a diff that is really a
toolchain change from an upstream bump; pinning both is a later work package.

What the MSVC pass refuses instead of guessing (all of them are generator errors
for an allowlisted class, never a silently wrong slot):

- a this-adjusting thunk (`[this adjustment: ...]`),
- a vbtable (a virtual base; `-- accessible via vbtable index 1 ... --`),
- a class that overrides a virtual of one of its secondary bases, found while the
  vtable is built from the Itanium data by searching every non-primary base on the
  primary chain (Itanium adds an entry to the primary table, MSVC uses the second
  vfptr; the flat IR cannot express both). A class that only declares new virtuals
  next to a secondary base is fine: its IR vtable is the primary table, the
  secondary base has an IR vtable of its own, and the base's offset in each ABI is
  emitted (`secondary_bases[].offset/msvc_offset`, `S2_BASEOFF_<Class>_<Base>`,
  `X::BASE_OFFSET_<Base>`, `X::as_<base>()`). A second vfptr in the MSVC dump is
  still refused as a backstop,
- a destructor entry that is not `[scalar deleting]` (MSVC gives a virtual
  destructor one slot, Itanium two),
- an overload group clang did not lay out reversed, contiguous *and anchored at
  the name's first declaration* (the rule `abi::slot(Abi::Msvc, ..)` implements;
  a declaration that overrides a base virtual sits in the base's slot and is not
  part of its name's group; `tests/layout.rs` then asserts every generated
  `msvc_slot` against that rule).

The one tolerated declaration-less slot is the implicit virtual destructor of a
class that declares none: clang still prints its `[scalar deleting]` entry at
the base chain's destructor slot, and the IR keeps no `isImplicit` declaration.

Two spellings clang uses only for MSVC are parsed: the vtable pointer prints as
`(X vftable pointer)`, and the `[sizeof=...]` line has no `dsize=`. Absolute paths
in record names and field types are rewritten to `s2sdk/upstream/...` and
`target/xwin/...`, so the same revision produces the same bytes in any checkout.
