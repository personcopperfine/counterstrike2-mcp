# sigs/ — signature database for the bridge

The bridge DLL (`bridge/`) calls into Valve code at fixed places: vtables, virtual slots, a few
non-virtual functions, globals and struct field offsets. Their RVAs change with every game build.
This directory stores each one as a **locator** (how to find it again) rather than a number, and the
tools here re-find them on a new build. `bridge/pins.py` turns the result into `bridge_pins.h`
(RVAs, Qt chains, literals). Slots, member offsets, sizes and consts the s2sdk headers describe are
generated into `bridge/s2sdk_gen.h` instead; the entry for such a number carries an `sdk` field and
stays here as the binary measurement that `pins.py` and `sdkcheck.py` hold the header to.

The same run can also emit the pins as Rust: `bridge/pins.py --rust crates/cs2-bridge/src/generated/pins.rs`
writes one `pub const <macro>: Pin` per emitted `#define` (same names, values, comments and order), the
module identity / fingerprint / slot / ident tables as `pub static` arrays with the same rows, and a
`pub mod linux` holding the Linux platform's own module list (`lib<module>.json`): a Linux entry with a
`bridge` name is resolved and gated by the same checks (`pin_checks`), but against the Linux tree
(`$CS2_LINUX_BINS`, default `target/game-bins-linux-<CURRENT_BUILD>`). No Linux entry carries a `bridge`
name yet, so that module is empty-but-present; its vtable text (a Rust `kind: "vtable"` Pin's
comment) is the entry's `notes`, there being no MSVC RTTI class on Linux. `bridge/test_pins_rs.py`
checks the C↔Rust parity **and order**, compiles pins.rs (committed and fixture output) with
`rustc -D warnings`, and covers the Linux fixture (a plain const, and a real vtable entry with an
`ident_slot` whose slot target is read through the `.rela` table), the narrowed linux block
(runs only with `--rust`; a C-only run needs neither the Linux tree nor its binaries) and the
refusal paths.

| Path | What |
|---|---|
| `signatures/<module>.json` | one file per DLL the bridge pins (hammer, toolframework2, scenesystem, engine2, rendersystemvulkan, client, server, tier0, schemasystem, soundsystem) |
| `signatures/lib<module>.json` | one file per **Linux** `.so` (`platform: linux`): `libclient`, `libserver`, `libengine2`, `librendersystemvulkan`, `libschemasystem`, `libsoundsystem`, `libparticles`, `libtier0`; same schema, separate files because each is a *different* game build |
| `signatures/SCHEMA.json` | JSON Schema of those files |
| `CURRENT_BUILD` | CS2 `ClientVersion` (from `game/csgo/steam.inf`) the database and `bridge_pins.h` are on |
| `fingerprints/BUILD` | the build the fingerprints were taken on (a fingerprint stays valid across builds as long as the function's code does not change: relative branches, RIP displacements and image immediates are masked out) |
| `fingerprints/<m>-fp.json` | masked code at every function entry, taken in IDA on the last IDA-checked build; `sigupdate.py` uses them to spot a locator that still matches but lands on changed code or on a different function. Entries the bridge consumes (`bridge` macro, `kind=function`, or any bridge entry located by a `vslot`) get a pattern long enough to be told apart from the functions they could silently be swapped with, so two neighbouring slots holding similar functions cannot compare equal; `ida_fp.py` prints what it had to extend and lists the entries it could not tell apart under the reserved `_weak` key (no entry lookup uses it; a weak entry is the gate's known blind spot, see "The bridge gate"). `validate` refuses a bridge entry that pins code and has no key here (and a value that could not
fail: empty or all-`??`), and `bridge/pins.py` additionally refuses one whose key does not match the
bytes. A `kind=vtable` entry with `ident_slot` also gets `<entry>#<slot>`, the function the identity-checked slot must still hold |
| `scripts/sigcheck.py` | `validate` (no binaries; also requires the fingerprints a bridge entry needs) and `resolve` (against `$CS2_BINS`, or a Linux tree with `--platform linux --tree`) |
| `scripts/sigupdate.py` | judge a new build: SAME / OK / DRIFT / FAIL / UNVERIFIED per entry, `--bridge-gate`, `record`; see "The bridge gate" below. `--platform linux --tree <linux tree>` judges the Linux half the same way (see "Linux binaries") |
| `scripts/stagebins.py` | copy the pinned DLLs out of a game install or depot download into a bins tree (`--linux` for the `.so` modules) |
| `scripts/sdkcheck.py` | the s2sdk generator's own check: the dual-ABI IR (`crates/s2sdk/s2sdk-sys/ir/`, emitted as `S2_VTLEN_*`/`S2_SLOT_*`) vs the game binaries through this directory's Resolver (imported, never duplicated); Windows by default, `--platform linux --tree target/game-bins-linux-$(cat sigs/CURRENT_BUILD)` for the Linux build - see "sdkcheck.py" below |
| `scripts/linuxvtables.py` | compare the s2sdk IR's Itanium vtable lengths with the `_ZTV` symbols of a staged Linux tree |
| `ida/ida_sigs.py`, `ida/ida_fp.py` | IDA-only: generate new locators / regenerate fingerprints (needs an IDA Pro licence and an IDB of the module) |

## Binaries

The resolver reads the DLLs from `$CS2_BINS` (default `target/game-bins/`, gitignored), laid out
like the install under `windows/`: `windows/game/bin/win64/engine2.dll`,
`windows/game/bin/win64/tools/hammer.dll`, `windows/game/csgo/bin/win64/client.dll`, …

```sh
python3 sigs/scripts/stagebins.py --game "${CS2_MCP_PROJECT_ROOT:-.}/workshop/run/app" --out target/game-bins
```

These are Valve's files. **Never commit them**, and never add them to a repo that has a remote.

## Linux binaries

The `platform: linux` entries need the **Linux** game build, which is a *different* build from the
Windows one unless you stage the matching one: the build tag is that install's own
`game/csgo/steam.inf` `ClientVersion`, and it is the key of `builds`/`expect` in the Linux files.
The trees are named after that build, `target/game-bins-linux-<ClientVersion>`. The default tree is
`target/game-bins-linux-<CURRENT_BUILD>`: the workflow updates the `cs2-home` volume to the same
build as the Windows DLLs, so the Linux tree carries the same number. Every Linux file is keyed
2000933 (libtier0/libsoundsystem/libclient also keep their 2000908 and 2000930 records; the
2000908 tree is `target/game-bins-linux`). The Linux install lives in the `cs2-home` podman volume, owned by a subuid, so it is
only readable inside a user namespace - pass `--unshare` and every read/copy goes through
`podman unshare` (`XDG_RUNTIME_DIR=<project root>/.podman/xdg` on the call):

```sh
XDG_RUNTIME_DIR="${CS2_MCP_PROJECT_ROOT:-.}/.podman/xdg" \
python3 sigs/scripts/stagebins.py --linux --unshare \
  --game "$HOME/.local/share/containers/storage/volumes/cs2-home/_data/steamapps/common/Counter-Strike Global Offensive" \
  --out target/game-bins-linux-<ClientVersion>
python3 sigs/scripts/sigcheck.py resolve --strict --platform linux --tree target/game-bins-linux-<ClientVersion>
```

The tree is laid out like the install under `linux/`: `linux/game/bin/linuxsteamrt64/libengine2.so`,
`linux/game/csgo/bin/linuxsteamrt64/libclient.so`, ... (`linuxtest`, `linux` is also what
`--linux` copies: the module set of `sigcheck.LINUX_MODULE_PATHS`, staged before the entries that
name those modules exist). Same rule as the DLLs: **never commit them**.

What an ELF reader has to do differently from the PE one, and what the entries look like:

* **The symbol tables are stripped of everything but `.dynsym`.** The game's own C++ classes are
  hidden there: of 7332 dynamic symbols in `libclient.so`, 178 are `_ZTV…` and all of them belong
  to statically linked libstdc++/libc++/third-party code. A tier0 utility class (`CThread`,
  `CUtlCharConversion`, the `KeyValues` dump contexts) is the exception - it has an exported
  `_ZTV` symbol - but every s2sdk IR vtable class is an abstract interface with no emitted vtable
  at all, so **no `_ZTV` symbol exists for any of them** (`linuxvtables.py` prints exactly that).
  A game class' vtable therefore has to be found through the code that uses it: the interface
  registry (`CreateInterface` → `InterfaceReg` → factory → singleton → constructor) or the
  constructor's own store, which is how `libsoundsystem.json` reaches `CSoundOpSystem`.
* **`.data.rel.ro` is not readable raw.** Relocated words are zero (or hold only an addend) in the
  file - the loader writes the value from `R_X86_64_RELATIVE`/`R_X86_64_64` at startup - so
  `sigcheck` reads every 8-byte slot through the `.rela.dyn` table (`image_qword`). Without that,
  every `_ZTV` vtable in `libtier0.so` reads as six zeroes. `slot_target` (the `ident_slot` read
  pins.py's identity rows come from) takes the same path, and compares the value against the
  image's own base (a `.so`'s 0, not the PE's `0x180000000`).
* **No RTTI, no MSVC COL.** There is no `.?AV…@@` type descriptor and no complete-object locator,
  so the `rtti` locator is Windows-only (the validator rejects it in a Linux file) and
  `itanium_vtable` takes its place: the class' `_ZTV…` symbol + 16 is the vtable's *address point*
  (the symbol names the group's two-word `[offset-to-top][typeinfo]` header and its `st_size`
  covers header + slots, so `st_size/8 - 2` is the length the linker records - an exact number,
  where the code-pointer scan can under-count a table whose last slot is a null).
* **Only the field offsets that actually appear can be pinned.** A `sig` + `{"disp"}` step reads a
  displacement out of the instructions that use a field, so the Linux `SNOFF_SEP_HASH` /
  `SNOFF_SEP_ENTINDEX` point at the `StartSoundEventWithParams` implementation that copies the
  params struct into the internal sound-event object - the same evidence the Windows entries use.
  Where the two ABIs disagree, the Linux number is what the Linux code says and the entry's `notes`
  records the difference (the internal object's field offsets differ; the params struct's do not).

The update gate covers the Linux half too: `sigupdate.py check --platform linux --tree
target/game-bins-linux-<NEW> --build <NEW>` judges the `platform: linux` files against the new
build with the same verdicts and the same fail-closed bridge gate as the Windows half (which stays
the default and is unchanged). The binary the `expect[]` being compared was recorded on lives in
the OLD build's tree: `$CS2_LINUX_BINS` if set, else `target/game-bins-linux-<OLD>` - pass
`--old` when that build differs from `sigs/CURRENT_BUILD`. `record --platform linux` writes
`expect[NEW]` and the `builds` sha256 the same way (a `.so` has no PE timestamp). There are no
Linux fingerprint files yet (`fingerprints/` holds IDA-taken Windows code), so a *changed* Linux
module's entries come out OK, not DRIFT - a person with a disassembler plus
`sdkcheck.py --platform linux` is the drift detector there. The DRIFT path and the gate's exit 3
are covered for Linux by `scripts/test_sigupdate_linux.py` (a scratch database under
`target/scratch/`, a fingerprint, an edited `expect`, one flipped byte).


## Format

One object per file:

| Key | Required | Meaning |
|---|:-:|---|
| `module` | yes | `hammer.dll`, `engine2.dll`, … (`libclient.so` for a Linux module) |
| `platform` | no | `windows` (default: `.dll`/`.exe`) or `linux` (`.so`); see "Linux binaries" |
| `image_base` | yes | always `6442450944` (0x180000000, IDA's default base; `via` addresses use it); `0` for a Linux `.so`, whose vaddrs are the RVAs |
| `builds` | no | `{"2000930": {"sha256": "<first 16 hex>", "timestamp": <PE TimeDateStamp>}}`, key = `ClientVersion`. A Linux `.so` has no PE timestamp: only `sha256` |
| `entries` | yes | name → entry |
| `_doc` | no | free text |

Entry:

| Key | Required | Meaning |
|---|:-:|---|
| `kind` | yes | `function` \| `vtable` \| `global` \| `string` (resolve to an RVA) · `offset` \| `const` (resolve to an integer) |
| `locator` | (1) | how to find it (below); a `kind=offset|const` entry may omit it and carry its resolved integer as `value` instead |
| `value` | no | literal resolved integer for a locator-less `offset`/`const` entry: a layout number sourced only from bridge code verified live; `via` states file:line. `expect` must repeat it. **Not a binary check**: with no locator, `resolve` returns it as-is and can never fail on it — `expect` re-checking it is self-consistency only, and `sigupdate.py` reports SAME for it on every build. |
| `via` | yes | `module!0x18… (Class::Method, what the site does)` on the build it was chosen. **The hint a person uses to re-find it** when the locator breaks. |
| `tag` | yes | evidence for the *name's meaning*: `V` verified in disassembly, `I` inferred, `RTTI`, `S` SDK header, `D` GameTracking dump, `?` |
| `expect` | no | build → resolved value; written by `sigupdate.py record`. `sigcheck.py resolve --record` writes it too, but a run that selects a file with bridge entries refuses `--record` before writing anything and points at `sigupdate.py record` |
| `bridge` | no | the `bridge_pins.h` macro; only entries with this field reach the header (as a `#define` unless they carry `sdk`); with `sdk` it is just the entry's name in the `k_pinSlots` rows and the sigupdate output |
| `sdk` | no | the generated SDK expression that owns the number: a string in tie syntax over the macros `bridge/s2sdk_gen.h` emits - `S2_SLOT_ISceneSystem_AddView`, `S2_SLOT_X*8` (a slot the game calls as a vtable byte displacement, e.g. `S2_SLOT_IParticleSystemMgr_GetFunctionDefinition*8`), `S2_OFF_CMapNode_m_Children+8`, or `S2_OFF_A+S2_OFF_B` (a member of an embedded object). The bridge spells that macro, so `pins.py` emits **no `#define`** for the entry; the entry stays resolved, gated and fingerprinted, and a vslot entry keeps its `k_pinSlots`/`k_pinBytes` runtime rows. `pins.py` refuses to write when the expression and the measurement (a vslot's slot index, else the resolved value) disagree, and `sdkcheck.py` reports an `sdk:<module>/<entry>` row (PASS, or SKIP (LITERAL) when the entry has no locator). Allowed on Windows `kind=function` + `vslot` (one bare `S2_SLOT_` macro), `offset`, `const`; `validate` checks the syntax. Written by `scripts/sdkadopt.py PIN=S2_EXPR`, which checks the macro exists and equals the measurement; never typed |
| `ident_slot` | no | `kind=vtable` only: slot whose function pointer the bridge checks for identity (contents compared, not called). bridge/pins.py emits the check from this field. |
| `struct` / `field` / `ctype` | no | `kind=offset` only: grouping metadata naming the C struct, field and type the offset is asserted against (e.g. `"CMapDoc"`, `"root"`, `"CMapRootElement *"`); a future struct emitter groups them into typedef blocks |
| `structs` | no | retired (it kept an entry out of the old `bridge_structs.h`, which no longer exists); changes nothing, still validates |
| `status` | no | `proposed` → `verified` → `stale` (failed on the newest build); `validate` rejects anything else. `bridge/pins.py` refuses to pin a bridge entry left `stale` |
| `notes` | no | free text; `bridge_pins.h` uses it as the macro comment for vslots/functions/globals |

(1) required for `function`/`vtable`/`global`/`string`; for `offset`/`const` either `locator` or `value` must be present.

Entries without a `bridge` macro do not reach `bridge_pins.h`: a `kind=offset`/`const` entry without
`bridge` or `sdk` is a measurement only `sigcheck`/`sdkcheck` use (the generated `bridge_structs.h`
was retired by de-hardcode WP1b; its numbers are now generated macros, `bridge_pins.h` macros or
bridge literals). A value that comes from a
locator is a binary-checked fact, because `resolve` derives it from the module and `expect`
compares; a literal (`value`, no locator) remains a named, sourced assumption that `resolve`
cannot fail on (self-consistency check only).

Locator types, most stable first. Use the first one that applies; byte patterns are the fallback:

| `type` | Fields | Resolves to |
|---|---|---|
| `export` | `name` | the export's RVA (PE export table, or the ELF `.dynsym` definition) |
| `rtti` | `class` (or a full `.?AU…@@` name), `col_offset` (default 0) | the vtable's first slot (MSVC RTTI; **Windows only**) |
| `itanium_vtable` | `class` (or the mangled `symbol`), `measure` | the address point of the class' Itanium vtable: the `_ZTV…` symbol + 16 (skipping the `[offset-to-top][typeinfo]` header). With `measure: "symbol"` (exact, from the symbol's `st_size`) or `measure: "scan"` it resolves the table's length instead (kind must be `const`) |
| `vslot` | `vtable` (a `kind=vtable` entry in the same file), `slot` | the function pointer in that slot |
| `vtlen` | `vtable` (a `kind=vtable` entry in the same file) | the vtable's length in slots: the number of leading `.text`-pointing 8-byte entries, counted from the first slot after the RTTI locator until the first entry that is not a code pointer (the next table's COL pointer, a data/string pointer, a null, or the end of the image). On an ELF the same scan runs from the address point and stops at the next table's header word; a table whose *last* slot is a null (a pure virtual the linker left at 0) is under-counted by one, so a `_ZTV` symbol's `measure: "symbol"` is preferred where one exists |
| `string` | `string`, `section` (default `.rdata`, or `.rodata`/`any` on ELF) | the unique C string |
| `sig` | `sig`, `section` (default `.text`), `refs` | the unique match of a masked byte pattern |
| `base` | `base` (another entry's name) | that entry's value, then `steps` |

`vtlen` derives a **vtable length** (an integer, so the entry is `kind=const`). It is only as honest
as the table's tail: a slot that happens to hold a code address (a thunk, or code stored as data)
would be counted, so it is used only for vtables whose length is also recorded in `expect` and whose
`via` names the table. `sigcheck.py selftest` checks the scan against two vtables whose lengths are
known independently of the database.

`sig` grammar: hex bytes or `??`, space separated (`48 8B 05 ?? ?? ?? ??`). At least 8 bytes, literal
first byte, exactly one match. Branch/call targets, RIP-relative displacements and image addresses
are wildcarded; struct displacements and small immediates are kept. `refs`:
`[{"at": k, "next": n, "string": "..."}]` = the RIP-relative operand at `+k` must point at that string.

`locator.steps`, applied in order (one key per step):

| Step | Effect |
|---|---|
| `{"call": k}` | position = target of the `E8 rel32` at `+k` |
| `{"rip": k, "next": n}` | position = position + n + disp32 at `+k` |
| `{"deref": k}` | position = image pointer stored at `+k` minus image base |
| `{"rva": k}` | position = 4-byte image-relative offset at `+k` |
| `{"check": "<pattern>"}` | the pattern must match here; position unchanged |
| `{"disp": k, "size": 1\|2\|4\|8}` | value = signed integer at `+k` (field offset / constant) |
| `{"add": n}` | value += n once a value exists, else position += n |
| `{"div": n}` | value /= n; the value must exist and be a multiple of n (a vtable byte offset -> slot index) |

Rules:
1. Virtual functions are `vslot` off a vtable entry, never byte patterns. CRT initializer tables
   (`_initterm` arrays) are not vtables.
2. Never rename an entry; bridge entries keep their macro-style names.
3. `expect` is keyed by `ClientVersion`, never by date or hash.
4. An entry with a locator is added only after `sigcheck.py resolve` gives the value seen in the
   disassembler. A literal `value` entry has no locator and cannot meet that rule: it is added
   only when the value is verbatim from bridge code that was verified live (tag V), and it stays
   a documented assumption — `resolve` cannot fail on it (self-consistency check only).
5. The database records game facts, not bridge-side choices. A constant the bridge chose itself
   (e.g. the sink-listener vtable buffer size, `SND_LISTENER_SLOTS 16` in mcp_bridge_game.inc —
   the game calls slots 0..5) stays a bridge-source constant and never becomes a DB entry.
6. An entry carrying a locator must not also carry `value` (the locator's result wins), and
   `value` is only allowed on `kind=offset`/`kind=const`; the validator rejects both.
7. A `platform: linux` entry is located in the **Linux** binary and its `expect` is keyed by the
   Linux build (`steam.inf` `ClientVersion` of that install). A Windows number is never copied
   into it: where the two ABIs disagree the Linux value is what the Linux code says, and the
   entry's `notes` records the difference (S2 report: the sound params struct agrees, the
   `CSoundOpSystem` primary vtable length does not).

## Commands

```sh
python3 sigs/scripts/sigcheck.py validate                     # structure + the bridge entries' fingerprints
python3 sigs/scripts/sigcheck.py resolve --strict             # every entry vs $CS2_BINS and expect[CURRENT_BUILD]
python3 sigs/scripts/sigcheck.py resolve --module hammer.dll -v
python3 sigs/scripts/sigcheck.py resolve --strict --platform linux --tree target/game-bins-linux-$(cat sigs/CURRENT_BUILD)
python3 sigs/scripts/sigcheck.py resolve --record <BUILD>     # refused for a run that selects a file
                                                              # with bridge entries (use sigupdate.py record)
python3 sigs/scripts/sigcheck.py selftest                     # vtlen scan vs known vtable lengths (needs $CS2_BINS)
python3 sigs/scripts/linuxvtables.py                          # IR Itanium VTLEN vs the Linux tree's _ZTV symbols
python3 sigs/scripts/sdkcheck.py                              # IR vs $CS2_BINS (Windows); --platform linux --tree ... for the Linux build
python3 sigs/scripts/sdkcheck.py --self-test                  # perturbed in-memory copies must FAIL
python3 sigs/scripts/sdkadopt.py [--dry-run] PIN=S2_EXPR      # write an entry's `sdk` (validated against s2sdk_gen.h and the binary)
python3 sigs/scripts/sigupdate.py check --tree <new bins> --build <NEW> --bridge-gate --json <out.json>
python3 sigs/scripts/sigupdate.py check --platform linux --tree target/game-bins-linux-<NEW> --build <NEW> --bridge-gate
```

## sdkcheck.py

`s2sdk`'s check, run before `bridge/build-rs.sh` (an `sdk` entry has no compile-time tie: the build's
`pins.py --check` gate does not hold the header to the binary) and of `workflows/game-update.md`
lanes A (A2b) and B (B5, before `pins.py`): one line
per fact `PASS|FAIL|SKIP <what> upstream=<n> game=<n> <why>`, exit 0 iff no FAIL. The default mode
needs `target/game-bins` and checks the MSVC numbers; the `--platform
linux --tree target/game-bins-linux-$(cat sigs/CURRENT_BUILD)` mode checks the Linux half the same way (a *different* build,
staged per "Linux binaries"), through the same Resolver:

* **(a) vtable lengths**: every DB-measured Itanium VTLEN (`itanium_vtable` + `measure`, e.g.
  libtier0's `VTLEN_IKeyValuesDumpContextAsText`) compares with gen.py's `itanium_vtable_len`
  under rule `==` - the locator is the class's own exact `_ZTV` symbol and `measure: "symbol"`
  reads the linker's `st_size` of that table group (header + every slot, no scan under-count),
  so measured == upstream's Itanium length is the claim. A class with a measured VTLEN but no
  IR vtable (the keyvalues dump contexts - not in gen.py's allowlist) SKIPs: gen.py derives an
  Itanium length only from IR vtables and adding the class would change the generated IR. Every
  IR vtable class itself also gets a line, backed by a symbol-table probe of the staged modules:
  no `_ZTV` symbol exists for any of them (the game's C++ symbols are hidden; `linuxvtables.py`
  prints that survey), so the row SKIPs saying so.
* **(i) the entity-identity chunk geometry**: libclient.so's `CG_ENTITY_CHUNKS` /
  `CCONST_IDENTITY_CHUNK_BITS` resolve and are shown; the identity stride (`imul 0x70` = 112) comes
  from `libclient.json` `CCONST_IDENTITY_STRIDE` (the same lookup, its sig anchored on the imul and
  a `disp` step reading the immediate) and compares `==` with the IR's Itanium
  `sizeof(CEntityIdentity)` (= 112), the number `S2_SIZEOF_CEntityIdentity` pins on the MSVC side.
  With no `*STRIDE*` const/offset entry in the DB the row SKIPs naming the entry it expects, so a
  renamed or removed entry cannot look like a pass (this script adds no DB entries).
* **(c) consts with a Linux-measured counterpart**: none yet; every located const without a
  generated counterpart comes out SKIP with its resolved value, and a resolution failure is FAIL.

* **(d) `sdk` entries** (`sdk:<module>/<entry>`, Windows lane): every sigs entry carrying `sdk`
  has its expression evaluated against `bridge/s2sdk_gen.h` (which `gen.py --check` proves is the
  pinned upstream's) and compared with the entry's measurement: a vslot locator's slot index, else
  the resolved value. A located entry is PASS; an entry with no locator (a bridge-side shape no
  binary backs) is SKIP (LITERAL) on equality; a difference is FAIL ("fix the header and the entry
  together", `workflows/game-update.md` A2b). These rows are what replaced the compile-time pin
  ties: the bridge compiles the generated number, this row holds it to the binary. Today 159
  entries carry `sdk` (`python3 sigs/scripts/sdkadopt.py --list`). A byte-offset entry
  (`S2_SLOT_X*8`, the displacement of `call [rax+0C0h]`) is compared with the IR's `msvc_slot * 8`.
  The Linux lane has no `sdk` rows.

* **Windows-only groups**: an IR group marked `"platforms": ["windows"]` (the tool modules have
  no Linux build) is checked by the Windows lane like any other and SKIPped by `--platform
  linux` with that reason (a `group:<name>` row plus its `vt:` rows) - never PASS, never
  FAIL.
* **Concrete-class vtable-length rows** need no new table shape: a `VT_PAIRS` row whose sigs entry
  is a `kind=vtable` + `rtti` locator of the *implementing* class (its COL is read back and must
  name it) compared with the IR's msvc length, rule `==` when the class adds no virtual, `>=`
  otherwise.

`--self-test` perturbs in-memory copies (IR values and each `sdk` row's header value on Windows; on Linux a synthetic stale-upstream
IR vtable and a doctored in-memory DB copy) and asserts each turns its check FAIL - nothing on disk
is touched.

## The bridge gate (fail closed)

`bridge/pins.py` emits the header the bridge DLL is built from, and `--bridge-gate` decides whether
the database is good enough for that. Per entry, on the new binaries:

| State | Meaning | Gate |
|---|---|---|
| `SAME` | binary unchanged, value still `expect[OLD]`, every fingerprint matches the bytes there | pass |
| `OK` | binary changed, the locator resolved uniquely and any fingerprint matches | pass |
| `DRIFT` | resolved, but the fingerprint differs: the function changed, or the locator landed on a different one (a moved vtable slot) | **fail** |
| `FAIL` | the locator did not resolve uniquely, an unchanged binary whose value moved, or a fingerprint that does not match the build it was taken on | **fail** |
| `UNVERIFIED` | a bridge entry that pins code has no fingerprint, so nothing here proves the locator found the right function | **fail** |

The last row is the one that keeps the gate honest. A `vslot` locator proves nothing by itself: it
reads `vt[N]` and accepts any image pointer, so when the game inserts or moves a virtual the value
still looks fine, `pins.py` re-pins whatever it found together with that target's first bytes, and
the runtime `PinSlot`/`PinBytes` checks compare `vt[N]` with a value read from `vt[N]` — a
tautology end to end. What says "`vt[N]` still holds this function" is the fingerprint taken on the
last IDA-checked build. So every bridge entry whose target is *code* — `kind=function`, which is
exactly what `k_pinBytes`/`k_pinSlots` pin — and every identity-checked slot (`ident_slot`, whose
target `pins.py` likewise reads from the binary) must have one, whatever the binary looks like; a
miss is `UNVERIFIED`, never `OK`.

Bridge entries located by a value pattern (`sig`, `rtti`, `string`, `export`) carry their evidence
in the locator instead and resolve to an RVA or an integer rather than code the bridge calls; they
have no fingerprint and are not required to have one. The `BRIDGE COVERAGE` line of the gate output
counts the fingerprints the bridge entries require, how many are missing, and how many of the ones
present are *weak* (naming them), so "checked", "assumed" and "cannot tell a swap apart" stay
distinguishable:

```
BRIDGE COVERAGE: 100 fingerprints required by bridge entries, 0 missing, 9 weak: client.dll:CVS_INPUT_FRAME_BEGIN, ..., soundsystem.dll:SNVS_SOS_ADD_LISTENER
```

(A weak name is printed as `<module>:<entry>`, because the same key can exist in several modules.)

A **weak** fingerprint (the `_weak` key of the file, written by `ida_fp.py`) is one whose masked
pattern still matches another function the entry could silently be swapped with. In practice that is
a tail-call thunk or a twin function that differs from its neighbour only in a call or jump target,
which has to stay masked for the pattern to survive a rebuild. `GSVS_ALLOW_SAVE_RESTORE` is the
clearest case: it pins slot 21 of the `ISource2ServerConfig` view of `CCSGameConfiguration`, its
fingerprint is `32 C0 C3` (`xor eax, eax; ret`), and slot 16 of that same table holds a function
whose first bytes are identical (both are `32 C0 C3`, read from server.dll on 2000930). So if a new
build inserted a virtual before slot 16 and the two slots ended up swapped, the value resolved at
slot 21 would change (it could not be `SAME`) but the fingerprint would still match whatever sits
there now, and the gate would print `OK` for it: nothing else in it could tell. Weak does not mean
absent: the fingerprint is there and satisfies `validate` and the coverage count. It means a pure
slot swap of a weak entry is covered only by a live run (the bridge calls the slot, so a wrong
target that matters breaks visibly) or by a person reading the entry's `via` site in a disassembler.
Weak entries count towards "required"; the fix for one is a longer fingerprint where the tie can be
broken without masking a target, otherwise the count and the names are printed so a blind spot is
never mistaken for a proof.

Fingerprints are taken with `sigs/ida/ida_fp.py` (IDA Pro) on the build in `fingerprints/BUILD`.
They are not regenerated per update: one stays valid while the function's code does, because
relative branches, RIP displacements and image immediates are masked out. It covers the entries the
gate requires, extended instruction by instruction until it no longer matches the functions the
entry could silently be swapped with (the other slots of the same vtable for a `vslot` entry, the
other functions of the module otherwise). `ida_fp.py` prints what it had to extend, and lists the
entries it could not tell apart within its cap under the file's `_weak` key. An entry added with a
`bridge` macro and no fingerprint fails the gate — and `sigupdate.py record`, which refuses to write
a build while any bridge entry is not SAME/OK — until someone with IDA adds one; the same entry
makes `sigcheck.py validate` fail, and `bridge/pins.py` refuses as well, so the header cannot be
built from it. To check the gate on a moved slot, swap two 8-byte pointers of a bridge-pinned vtable
in a scratch copy of the DLL under `target/` and judge a tree containing it: the affected entry must
come out `DRIFT`, never `OK`.

### What `bridge/pins.py` checks itself

The gate judges a *new* build; `pins.py` writes the header from the *current* one, so it re-checks
the same evidence instead of trusting that someone ran the gate:

* `sigcheck.validate_file` on each module's JSON: structure, the status enum, the fingerprints each
  bridge entry requires (and that every fingerprint value is a pattern that could actually fail —
  not empty, not all-`??`, a real first byte), that a bridge `vslot` entry is a plain
  `kind=function` slot read, and that a bridge entry is not measured from an anchor (`base`,
  `vtlen`, `vslot`) that is stale or has no `expect[CURRENT_BUILD]`;
* `builds[CURRENT_BUILD].sha256` against the DLL it is about to read (its own long-standing check);
* `status: stale` on any bridge entry — refused, because a stale bridge entry means the last person
  who judged it did not confirm the locator (it cannot come out of `sigupdate.py record`, which
  refuses to write while a bridge entry is not SAME/OK: it is a hand edit, an entry that gained a
  `bridge` macro after being judged, or one the judge left stale and printed as PASS);
* every bridge entry: `expect[CURRENT_BUILD]` must equal the value the locator resolves — the gate's
  own SAME test, and the only thing that says a `sig`/`export`/`base`-located bridge value did not
  move — and, for those that pin code, every fingerprint they require must be present *and match*
  the bytes there (`sigcheck.fp_checks`/`fp_match`, the helpers the gate judges with).

The last two are what a run of `validate` + `resolve --strict` alone does not cover: `resolve`
compares an `expect[]` only when one is present, and `validate` has no binaries, so without them a
hand-edited pattern, an edited locator or a dropped `expect[]` would be pinned silently.

### Known gaps

* **The recorded evidence is self-asserted.** `expect[]`, the locator and the fingerprint are all in
  the same files, so a person who edits all three consistently (for example by re-taking the
  fingerprint with `ida_fp.py` at the wrong address) is accepted by the gate and by `pins.py`
  alike: nothing proves the recorded build was produced by `sigupdate.py record`. A future package
  could have `record` also write a digest over the bridge entries it judged (locator, `expect[CUR]`,
  the required fingerprints, `_weak`) into `builds[CUR]` and have `pins.py` recompute it, which
  would turn "only `record` may write a bridge build" from a convention into a check.
* **A bridge `kind=const` slot number is outside the gate.** `HVS_NODE_ISA`
  (`sigs/signatures/hammer.json`, `status: verified`, `sdk: S2_SLOT_CDmElement_IsA`) is the
  `CMapNode::IsA` slot the bridge calls (`e->vt->IsA(...)`) through the struct in `bridge/s2sdk.h`.
  It has no `k_pinSlots` row and no `k_pinBytes` prologue, so no fingerprint is required and the
  `BRIDGE COVERAGE` count does not include it: if a build inserted a virtual into `CMapNode` the
  header would keep saying 4 and the bridge would call the wrong slot silently. What measures it is
  its locator, the IsA call in CMapDocHistory's node-tree recorder (`mov r8,[rax+20h]; call r8`:
  `disp` 0x20, then `div` 8 = slot 4; the pattern matches exactly once), and its `sdk:` row in
  `sdkcheck.py`, which compares it with the generated `CDmElement::IsA` slot. Other polymorphic
  bridge slots can follow the same recipe; a `kind=vtable`+`ident_slot` form would bring them under
  the coverage rule.

Game updates: [../workflows/game-update.md](../workflows/game-update.md).
