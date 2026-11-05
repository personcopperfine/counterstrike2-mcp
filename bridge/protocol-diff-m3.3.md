# Protocol changes in m3.3: the pet group (the particle editor ops, without pet.render)

Bridge build `m3.3` (Rust bridge) = m3.2 + this group.

**Scope**: this lane ports the eleven `pet.*` bodies the C bridge implements in
`bridge/mcp_bridge_pet.inc` (frozen, m2.20) - `pet.load`, `pet.active`, `pet.open`, `pet.reload`,
`pet.save`, `pet.functionTypes`, `pet.schema`, `pet.text`, `pet.apply`, `pet.undo`, `pet.redo` -
so the Rust bridge answers their requests with the C's bytes instead of `-32011 NotImplemented`.
`pet.render` (declared at `mcp_bridge_pet.inc:1123`, defined in `mcp_bridge_pet_render.inc`) is
**not** in this lane: it stays unregistered and keeps answering
`-32011 NotImplemented {"blockedOn":"rust bridge m3.0: not ported yet"}` until the render lane
merges.

The port is in `crates/cs2-bridge/src/ops/pet/` (`mod.rs` = the C's `op_pet` dispatch and arm
tail, `reply.rs` = the pure reply builders, `game.rs` = the method bodies and the game readers).
No method was added or removed; no error code was added.

## The wire, field by field

| Where | m2.20 (C bridge) | this build |
|---|---|---|
| each `pet.*` method's result | the C's object | **the same bytes**, member order included (each builder cites the `sb_add` block it ports) |
| each `pet.*` error's `code` / `message` / `data` | `-32602 InvalidParams`, `-32602 UnloadableFile`, `-32003 NoActiveDocument`, `-32015 UntitledDocument`, `-32016 UnsavedChanges`, `-32017 PrerequisiteFailed`, `-32023 SchemaUnavailable` | unchanged; the `data` scratch is the C's `char data[512]` (`OP_ERR_DATA_CAP`) and the `pet_unloadable` shortening loop is ported with it |
| the result buffer | `static char rbuf[MAX_LINE - 2048]` (`mcp_bridge.c:6222`) | the same capacity (`ops::pet::buffer`), so an overflow answers `-32603 InternalError: pet result too large` exactly where the C does |
| `capabilities.methods` | the twelve `pet.*` rows once their needs verify | the **eleven** ported rows until the render lane lands; `pet.render` appears with it |
| the gate | `-32017 PrerequisiteFailed` naming the first unmet need in `bits[]` order | unchanged (`crates/cs2-bridge/src/registry.rs`, held to the C by `tests/registry.rs`) |

## Differences I could not avoid

1. **The non-Windows half answers a Rust-only `-32603`.** `pet.dll` and every `S2_*` pet type are
   Windows-only in the generated bindings (`crates/s2sdk/s2sdk-sys/src/generated/mod.rs` gates
   `pet`, `pet`'s neighbours `toolutils2`/`toolframework2`). The method bodies are therefore split
   (`ops/pet/game.rs`): the parameter checks and the reply builders are shared and run on every
   target, and `platform` is the Windows object walk. On a non-Windows target the game access
   returns

   ```
   -32603 InternalError {"reason":"pet.* needs the Windows target: this build has no pet.dll half"}
   ```

   That answer is **unreachable in production**: `PIN_PET` and `PIN_TF2` never verify off Windows,
   so the `-32017` gate refuses every row of this group before a body runs. Three arms keep the
   C's own bytes on every target because they need no game object (`pet.active` with no session,
   `pet.reload`/`pet.save` with no session, `pet.open`'s G2 gate with no `<app>` root), and
   `tests/ops_pet_gate.rs` pins them.

2. **`ReadFile` is the one Windows call not made by hand.** `pet_read_vpcf` opens with the C's
   own `CreateFileA` (the path's raw ANSI bytes, the C's share mode/creation/flags), then
   `GetFileType` (a non-disk file - a FIFO would block the main thread - is `read_failed`), then
   `GetFileSize` with the C's `0xFFFFFFFF` sentinel and `hi` check, then reads through
   `std::fs::File::from_raw_handle` (the same `ReadFile`, one byte more than the size to detect a
   file that grew under us). `ReadFile` itself is named in the `windows-sys` file-system module
   only through `Win32_System_IO`'s `OVERLAPPED`, and this lane may add only
   `Win32_Storage_FileSystem` to `Cargo.toml`; the handle is moved into a `File` instead, which
   closes it exactly once. The same two reasons come out (`"read_failed"`, `"too_large"`), and
   the 4 GB sentinel now exists. No wire byte changes.

3. **The command line is rebuilt from the CRT's arguments.** `kv3_addon_name(GetCommandLineA(), ...)`
   (`mcp_bridge_pet.inc:389`) needs `Win32_System_Environment`, which this lane may not add to
   `crates/cs2-bridge/Cargo.toml` (only `Win32_Storage_FileSystem` was allowed).
   `std::env::args_os()` joined with spaces parses identically for `-addon <name>`,
   `-addon=<name>` and `-addon "<name>"` - the shapes `runtime.sh` passes - and differently only
   for a command line whose *quoted argument* contains the text `-addon`. The effect is confined
   to `pet.open`'s G2 gate (`no_addon` instead of a resolved host path).

4. **`<app>` comes from `std::env::current_exe()`** instead of `GetModuleFileNameA(0, exe, 1024)`.
   It is the same loader answer (the full path of the process's image, i.e.
   `<app>\game\bin\win64\cs2.exe`); a name that does not fit 1024 bytes fails, as in the C. The
   conversion is `to_string_lossy`, so an install path that is not valid UTF-8 is decoded before
   the derivation: a non-ASCII (`CP1252`) install directory would then form a different host path
   than the C's ANSI bytes. Real installs in the tool images are ASCII.

5. **`LOCALAPPDATA` is read with `std::env::var`.** `GetEnvironmentVariableA` and `std::env::var`
   read the same variable; the C's `n == 0 || n >= 260` fallback is kept (including the C's exact
   fallback path `C:\users\steamuser\AppData\Local`).

6. **`CPetDoc::m_Selection` is read through the generic `CUtlVector` layout.** The Rust IR has
   `m_Selection: [u8; 24]` (no `CUtlVector<PetDocSelection_t>` instantiation) while the C names
   `S2_CUtlVector`, which is the same generic 24-byte layout the generated
   `CUtlVector_CEntityHandle` carries (`m_Size` @0, `m_pMemory` @8). `pet_clear_selection` casts
   the member to that generated type and never writes a hand-computed offset.

7. **`pet_bind`'s latch is a `OnceLock`.** The C binds the pinned entry points once
   (`g_petBound`); the addresses depend only on the pinned module base, so binding once per
   process is the same thing. Its precondition is the C's: every call site has already passed the
   module gate (`resolve_pet` / `active_pet` / `pet_doc_json`'s `obj_is`).

8. **`pet.load`'s latch is atomics.** `g_petLoadTried` / `g_petLoadIdx` / `g_petLoadOk` are
   `AtomicBool`/`AtomicI32`; the bodies run on the main thread only, as in the C.

## C bugs and dead arms ported as they are

- `op_pet_open`'s `path[0] == '\\'` arm (`:493`) is **unreachable**: the raw-member escape scan
  directly above (`:476-486`) refuses any backslash first. Ported, and the test list notes it.
- `op_pet_open`'s `pl > 512` arm (`:475`) is reachable only through `json_string`'s 519-byte
  copy; ported and tested with a 513-character path.
- `pet_doc_json`'s `no_document` arm (`:219`) is unreachable through `active_pet` (which already
  requires `m_pDoc`). Ported.
- `op_pet_schema`'s "unreachable with the caps above" host-path arm (`:394-400`) is ported as
  `read_failed` on the caller's path.
- `op_pet`'s tail (`:1248`, `-32601 MethodNotFound <method>`) is unreachable in the Rust bridge:
  a name without a body is answered `-32601` by the method table before the fence, and the C's
  own tail is the table's answer.
- **`pet.apply`'s `errp` leak** (`:1060`: "errp is a tier0 heap string: leaked on purpose (the
  bridge has no tier0 free)") is ported as-is: the Rust side reads the string and does not free
  it either. It is a rare path (`kv3_load_failed` only).
- `static u64 flist[4]` (`:1081`, `:1111`) is a process static here too
  (`game::platform::FUNC_LIST`, a main-thread `UnsafeCell`), written in full on every call, as
  the C's static is.
- `pet.functionTypes` reads the seven counts one at a time and returns on the first unusable one
  (`:608-616`), so no later count call is made; the first bad row returns from the whole reply
  (`:624-627`) without walking any later definition. `read_arrays` likewise stops at the first
  unusable count or unreadable child (`:191-194`, `:201-204`). The reply builder marks the
  types the reader never reached `TypeArray::Stopped`, which is refused if it is ever written.

## Memory safety without a wire change

- `m_bIsNew` is read as a byte and compared with zero, not as a Rust `bool` (MSVC stores `bool`
  as one byte whose only valid values are 0/1; a torn or foreign byte would make a Rust `bool`
  load undefined and the C read the same byte). The same rule covers every game vtable flag:
  `IsModified`, `HasEntries`, `IsGlobalScope` and `Save` are declared `-> u8` and compared with
  zero (`bridge/s2sdk.h:627`, `:663`; `bridge/mcp_bridge_game.inc:15`), and `SetModified`'s
  argument is a `u8`, as the header declares it.
- `GetPath`'s bytes are kept raw from the call to `Save`, the host gate and the reply builders
  (`PathText::text`, `DocHead::path`, `TextReply::path`, `reply::unloadable`): the C passes the
  `char *` through unchanged, and `sb_addjson` is the C's byte-wise escaper, so a code-page path
  produces the same reply bytes and opens the same file.
- Every raw game-pointer read is `unsafe` with a `SAFETY:` line naming the identity check
  (`obj_is` against the pinned vtable, or the verified module base) that makes it valid, and the
  `md_str` page probes are ported (`mcp_bridge.c:5169-5186`).
- `SAVE_CB` (the C's writable 64-byte zero callback block) is an `UnsafeCell`, not an immutable
  static, so handing the callee a `*mut` is defined even if it writes.
- `function_types` refuses a zero pet.dll base before reading `PETG_PARTICLE_MGR`; the C reads the
  global unguarded and would fault. The gate makes this unreachable, so no reachable byte
  changes.

## Parity tests (`bridge/test_*.py` under Wine against the Rust bridge)

| Script | Cases that cover this group |
|---|---|
| `bridge/test_pet.py` | step 0 (`pet.load` advertised once toolframework2 verifies), 1 (`pet.load` already-loaded / loaded), 2 (module state `verified`), 3 (the methods advertised - **eleven of twelve until the render lane lands**; the script expects twelve), 4-5 (`pet.open` + `pet.active` agree, `hasRoot`, `arrays`), 6 (`pet.reload`), 7 (`pet.save` clean -> `skippedClean`), 8 (`pet.open` on each malformed path -> `-32602 UnloadableFile`, then `pet.active` still answers), 9-10 (`pet.schema` field/base cross-check against `pet.functionTypes`), 11 (the loadability guard: a malformed copy is refused by `pet.reload` before `ReloadDocument`, `pet.active` still answers) |
| `bridge/test_pet_live.py` | 12 (`pet.text` on the copy: header, class, `bytes == len(text)`, clean, empty history), 13 (`pet.apply` +1 operator: applied, `bytes`, dirty, `canUndo`; `pet.active` agrees), 14 (`pet.undo`/`pet.redo` sequences and the empty-history results), 15-16 (`pet.apply` malformed text -> `UnloadableFile`, braces/labels), 17 (`pet.save` after an apply writes the file), 18 (`pet.reload` with and without `force`) |

`--static`: neither script has one. If one were added it could only check the C source; for this
group the host gates are the Rust equivalent and stronger, because every one of them compares the
Rust bytes:

| Host test | What it pins |
|---|---|
| `crates/cs2-bridge/tests/ops_pet_reply.rs` | every reply/error object, byte for byte, per C line; `pet_unloadable`'s left-shortening loop; `pet_open_gate`'s five refusals from string inputs; `pet.apply`'s unescape + label + KV3 gate |
| `crates/cs2-bridge/tests/ops_pet_params.rs` | every `InvalidParams` arm through the real handlers (`pet.open`, `pet.schema`, `pet.apply`) |
| `crates/cs2-bridge/tests/ops_pet_gate.rs` | **NEGATIVE**: all eleven rows answer `-32017` naming the C's module while nothing verifies; the served set is the eleven and not `pet.render`; the needs of the three multi-module rows |
| `crates/cs2-bridge/tests/ops_pet_host.rs` | `pet_open_gate` with the C's buffer sizes: `app[1024]` accepts a 600-byte app root, `host[2048]` accepts a host path over 512 bytes, `addon[64]` refuses a 64-character addon (`no_addon`); the reply builders carry the game's raw path bytes |
| `crates/cs2-bridge/src/ops/mod.rs` tests | every name in `HANDLERS` is an implemented, gated C row, registered once |

## What to expect live

Under `BRIDGE=rust` against a tools session that has `pet.dll` verified, all eleven methods behave
as at m2.20. Two things to watch:

- `test_pet.py` step 3 will report eleven methods, not twelve, until `pet.render` merges.
- `pet.load` still answers `loaded:false` as a result on a framework failure; the one-shot latch
  is per process, so restarting the tools session is the only retry - unchanged from the C.
