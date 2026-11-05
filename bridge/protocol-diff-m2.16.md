# Proposed protocol changes for bridge m2.16 (map.open: the first map-opening primitive, session-leak safe)

Additive over m2.15; no major bump. One documented-but-blocked row is replaced by a real method
(`file.open_asset` -> `map.open`) and one **error code is used by the bridge for the first time**
(`-32014 AssetNotFound`, already in the client taxonomy). `map.open` is the first primitive that
opens a `.vmap` in Hammer: `file.open_asset` (the VToolMessage asset route) was never implemented
because it is not an open/reload primitive — see `../../idabins/docs/flows/file-edit-reload-compile.md`
§B, the source of every fact below. Sources read for this pass: that §B, `mcp_bridge.c`'s
`modeldoc.open` / `pet.open` arms (the path + host gate pattern this one copies),
`../../idabins/docs/interfaces/CHammerApp.md` (the app/session/document chain and the `+0x58`/`+0x60`
session vector) and `sigs/signatures/hammer.json`. Consumer: `hammer-mcp`'s `open_map` behind the
existing capability gating; nothing in Rust ever sees an address.

**Status.** Everything below is `[I: offline]`: `./bridge/build.sh` (host unit test
`bridge/test_map_path.c` + pins + the DLL link) compiles it warning-free and
`bridge/test_entities.py --static` still passes. The live promotion is left to the orchestrator,
which opens a map through a running Hammer and records the runtime evidence in this file — the
bridge cannot claim runtime evidence for itself, so nothing here does.

**The address in the task's brief is a name artifact.** The task named
`hammer!0x181480850 OpenFile` / `0x181480920 OpenFileInternal`. Those are the stale `sub_…`
addresses in the IDB function names (idabins' own symbol table has the same 0x1480 shift between the
`sub_` name and the address column). The functions are `hammer!0x181481CD0` and `0x181481DA0`, and
`sigs/signatures/hammer.json` already records them as
`CBaseEditorApp::sub_181480850_OpenFile` (`expect[2000930] = 21503184 = 0x1481CD0`, vslot 18 of the
`CBaseEditorApp` vtable). The STOP condition ("if CHammerApp's slot 18 is not that function") does
**not** fire: `HV_HAMMERAPP_VT` slot 18 resolves to `0x1481CD0` (`sigcheck.py resolve`), and the
disassembly there is exactly the function §B describes — `cmp byte [rcx+0x1A3], 0` at
`0x181481CDF`, the log string `"Ignoring request to open file %s because the editor is currently in
the process of opening another file."` at `hammer!0x181A43AB0`, the call to `0x181481DA0`, and that
function's session loop (`movslq 0x58(%rsi),%r13` at `0x1814824C9`, `mov 0x60(%rsi),%rax` at
`0x181482506`, slot 35 `0x118(%rax)` at `0x1814824E3`).

## §7 method table — changed rows

| Method | Status | Bridge m2.16 | Params | Result | Call | Notes |
|---|---|---|---|---|---|---|
| `map.open` | implemented `[I: offline]` — `./bridge/build.sh` + `bridge/test_map_path.c` + `bridge/test_entities.py --static` pass; the orchestrator's live open promotes it | gate `NEED_HAMMER`; four pre-call gates (path shape, host file, binary-v9 DMX header, already-open session) | `{path:string (addon-content-relative and canonical, e.g. "maps/x.vmap" — forward slashes only)}` | `{opened:true, existing:false, path:"<wine path of the session OpenFile returned>", epoch:int}` · `{opened:false, existing:true, path:"<wine path the open session holds>", active:bool, epoch:int}` · `{opened:false, existing:false, path:"<the requested path>", reason:"open_failed", epoch:int}` | `CBaseEditorApp::OpenFile(app, path, 1)` — `HVS_APP_OPENFILE`, `HV_HAMMERAPP_VT` slot 18, `hammer!0x1481CD0` (`flags 1` = what the UI passes); the session walk is `app->sessions`/`app->sessionCount` (`HOFF_APP_SESSIONS_DATA`/`_COUNT`, `0x60`/`0x58`) | The **path gate** is `bridge/map_path.h`'s `map_open_path_ok`: non-empty, `<= 512` bytes, no control characters, no `:`, not absolute (`/`, `\`, `C:`), **no `\`** (canonical spelling: forward slashes only), `.vmap` (case-insensitive), under `maps/`, a basename, no `..` (the same rule `modeldoc.open`/`pet.open` apply), and the **canonical-segment rules of review M1**: no empty segment (`//`), no `.`/`..` segment, no segment ending in `.` or a space. Win32 opens all of those spellings for the same file, so a second spelling would miss the session gate and leak a session; the gate refuses them instead of normalising. A refusal is `-32602 InvalidParams {reason:"<the sentence>"}` and it happens **before** anything is called: `OpenFile` pops a modal box on a bad path and blocks the tools main thread. The **host gate** resolves the addon content root exactly like `pet.open` (`editor_app_root` = `<app>` out of `GetModuleFileNameA`, `kv3_addon_name` = `-addon` out of `GetCommandLineA`, `kv3_host_path`) and requires the file to exist and be a disk file (`CreateFileA`/`GetFileType`): `-32014 AssetNotFound {reason:"no_such_file"/"not_a_file"}`, `-32017 PrerequisiteFailed {reason:"no_app_root"/"no_addon"}` when the addon cannot be resolved. The **content gate** (review H1) is `map_path.h`'s `map_vmap_header_ok` over the file's first 512 bytes: the DMX header the crate's own reader accepts for binary v9 (`<!-- dmx encoding binary 9 format <fmt> <ver> -->` plus the NUL that ends it), compared word by word — the first word `encoding`, the second `binary`, the third the integer `9`, and at least six words in all. A 16-byte prefix cannot tell the accepted header from `keyvalues2 1` or `binary 8` (`<!-- dmx encodin` is shared), which is exactly why the words are compared. Anything else is `-32014 AssetNotFound {reason:"InvalidMapFile"}`: a load failure reaches OpenFileInternal's code 0/3 path, which calls `Qt5Widgets!QMessageBox::warning` (IAT `hammer!0x181698480`) with the string "Error opening file" (`hammer!0x1A43C60`) on the tools main thread, and the drain's re-entrancy guard then blocks every queued request with nobody to click it. The Rust tool parses the WHOLE file with `hammer-mcp/src/vmap` before it sends anything (`InvalidMapFile`, nothing dispatched) — this gate is the defence in depth for a raw bridge client. The **session-leak gate**: `OpenFileInternal`'s session-match slot (35, `+0x118`) is the `xor al,al; ret` stub `hammer!0x18009DBC0` in this build, so it never matches ANY session — calling `OpenFile` for a file that is already open creates a SECOND session for it and leaks the first (its undo history, views and `CMapDocSession` stay alive). The bridge walks the vector itself, compares each session's map path with the resolved host path (`map_path_eq`: case- and separator-insensitive; a trailing separator is dropped and after that the two must be EQUAL — review L2 dropped the relative-tail rule, because `OpenFileInternal` always stores a full path and the host path is always absolute, so a tail match could only claim a different file, e.g. another addon's `maps/x.vmap`) and answers `existing:true` **without calling `OpenFile`**. The per-session path TEXT is the docSession's `char *` at `+0x6B0` (`CMapDocSession_Path`, the same member `doc_fingerprint`/`map.save` read), NOT the value session slot 26 returns: `GetMapFilePath` (`HVS_GET_MAP_FILE_PATH`) is `mov rcx,[rbx+0x138]; lea rax,[rcx+0x6B0]; test rcx,rcx; cmove rax,<a static default CUtlString>` at `hammer!0x13ED70` — the ADDRESS of the CUtlString member (a set `cmove` substitutes a static default CUtlString when there is no doc, whose `m_pString == 0` is the empty string). Reading that as `const char *` copied the bytes of a heap pointer, so the leak gate could never fire and every repeat open leaked a session (review C1; the prototype in `s2sdk.h` is now `const CUtlString *`). An unreadable vector (count over 4096, a bad data pointer, a vector entry that is not a `HV_SESSION_VT` object) is `-32603 InternalError {reason:"session_vector_unreadable"}`: a stale layout must fail loudly, never leak. A session whose path cannot be read (no doc, untitled, cut storage) is SKIPPED by the walk, not an error (review L5: an earlier revision of this row claimed -32603 there, which is doc drift — the code `continue`s, and the untitled session is a normal state). There is no `force` parameter (review L1: it was unreachable from the tool, could not bypass the unreadable-vector refusal, and its only effect was to create the duplicated session the gate exists to prevent). `epoch` is `doc_fingerprint()`'s epoch after the call (a new session bumps it); on success `path` is the session `OpenFile` returned (review L6: the active session is only the fallback), on failure the path the caller named; `opened:false` without `existing` is `OpenFile` returning 0 — the re-entrancy guard (`app+0x1A3`) or a load failure, indistinguishable after the fact, so the reason is the generic `open_failed`. The bridge answers that as a RESULT (like `modeldoc.open`/`pet.open`): the client's `open_map` turns it into `OpenFailed`, which is NOT retry-safe (review H1: the load failure that popped the modal is what a retry pops again). Result buffer 2048 bytes (fixed shape). |
| `file.open_asset` | **stays `impl 0`**, blockedOn re-pointed: `"superseded by map.open (m2.16): the VToolMessage asset route is not an open/reload primitive (file-edit-reload-compile.md §B)"` | unchanged: `NEED_HAMMER, 0, 0, <blockedOn>` | — | `-32011 NotImplemented {blockedOn}` | — | Kept registered so `-32011` stays one row of the table (and `mesh.*`-style superseded rows stay greppable); it now names its replacement. The asset route resolves an asset -> path -> `OpenFile` with the *same* stub match slot, so it has the session-leak problem plus a modal on a missing asset: nothing is added by implementing it. |

## §8 error codes

`-32014 **AssetNotFound**` is used by the bridge for the first time (the code and its name are
already in the client taxonomy, `crates/cs2-mcp-common/src/bridge/errors.rs`, so no client change is
needed). Reused: `-32602 InvalidParams` (the path-shape sentences, unchanged in wording from
`modeldoc.open`/`pet.open` where they overlap), `-32017 PrerequisiteFailed` (`no_app_root`,
`no_addon`), `-32003 NoActiveDocument` (`hammer_unavailable`), `-32603 InternalError`
(`session_vector_unreadable`, `host_path_too_long`, the result-buffer overflow). No code is retired.
The same `-32014` carries the content gate's refusal, `{reason:"InvalidMapFile"}` (review H1: the file
is there, but it is not a binary-v9 DMX map and `OpenFile` would fail it into the modal
"Error opening file").

## §5/§6 notes

- Method count: **79 registered / 63 implemented** (m2.15: 78/62; the flip is `map.open`, plus the
  `file.open_asset` row that stays `impl 0` and keeps its registration). `control.version.bridgeVersion`
  and every `"build"` field are **`m2.16`**.
- **New pins** (`sigs/signatures/hammer.json`, regenerated `bridge/bridge_pins.h`):
  `HVS_APP_OPENFILE` = `HV_HAMMERAPP_VT` slot 18 (`expect[2000930] = 21503184`, vslot locator),
  `HOFF_APP_SESSIONS_COUNT` = `0x58` and `HOFF_APP_SESSIONS_DATA` = `0x60`, both `kind=offset` with a
  `sig` locator inside `OpenFileInternal` and a `disp` step (`0x1814824C9: movslq 0x58(%rsi),%r13`,
  `0x181482506: mov 0x60(%rsi),%rax`). `sigs/fingerprints/hammer-fp.json` gains
  `HVS_APP_OPENFILE` (the 25-byte prologue `48 89 5C 24 08 … 41 8B F8`, strong: it matches no other
  slot of the pinned vtable and exactly one location in the image; the same bytes are `pet.dll`'s
  IDA-taken `PETVS_APP_OPENFILE` fingerprint, since it is the same `CBaseEditorApp::OpenFile` code).
  All three entries are `status: proposed` until the orchestrator's live run.
- `PIN_MODULE_COUNT` stays 12 (no new module). `bridge/s2sdk.h` gains the `CHammerAppVtbl` (slot 18
  `OpenFile`), the two vector fields with their `S2_FIELD` asserts and `S2_SLOT(...OpenFile,
  HVS_APP_OPENFILE)`; `struct CHammerApp`'s `vt` is now typed `const CHammerAppVtbl *`.
- The diff against the base branch introduces **no `0x` literal** in `bridge/mcp_bridge.c`
  (`bridge/test_entities.py --static`, `static_no_new_hex_literals`): every number is a
  `bridge_pins.h` macro. The new code lives in `bridge/mcp_bridge_map.inc` (included after
  `mcp_bridge_pet.inc`, whose `editor_app_root`/`kv3_*` helpers it reuses — `pet_app_root` was
  renamed `editor_app_root` because two gates share it now) and `bridge/map_path.h` (pure, no
  Windows API: the path-shape gate, the DMX-header gate and the same-file compare, host-tested by
  `bridge/test_map_path.c`, wired into `build.sh` like `test_json_text.c`/`test_kv3_check.c`).
- Version bump outside the new code: `bridge/test_entities.py`, `test_modeldoc.py`,
  `test_modeldoc_render_live.py`, `test_pet.py` and `test_pet_live.py` assert the bridge build and
  are bumped to `m2.16` in this pass. `test_entities.py` keeps reading its own pass's
  `protocol-diff-m2.15.md` (the file keeps the version of the pass that wrote it), and its
  `--static` diff base became a parameter: `--base-ref` (default `main`; the `hammer/entities`
  branch it used to name is merged).
- Live probe list for the orchestrator (a Hammer with an addon open, `-addon mcp_test`):
  1. `control.version` -> `bridgeVersion == "m2.16"`, `control.capabilities` advertises `map.open`
     (and `file.open_asset` stays advertised as `-32011`).
  2. `map.open {path:"maps/probe.vmap"}` on a Hammer that already has that file open ->
     `{opened:false, existing:true, active:true, path:"<the open doc's path>", epoch:N}` and the
     **session count does not grow** (the point of the gate: `doc.active` id/path unchanged, the
     in-editor undo history intact, no second tab).
  3. `map.open {path:"maps/<other>.vmap"}` for a file that exists under the addon's `maps/` ->
     `{opened:true, existing:false, path:"Z:\\app\\content\\csgo_addons\\mcp_test\\maps\\<other>.vmap"}`
     and Hammer reports the new document (`map.save`/`doc.active` follow it). Then
     `map.open {path:"maps/probe.vmap"}` again (the file from 2) ->
     `{opened:false, existing:true, active:false}`: the first file's session is still in the app
     (review M2's A -> B -> A).
  4. `map.open {path:"maps/nope.vmap"}` -> `-32014 AssetNotFound {reason:"no_such_file"}` with no
     modal on screen; `map.open {path:"maps/half_written.vmap"}` (a copy of a real map truncated to
     a few bytes, or a renamed `.vpcf`) -> `-32014 AssetNotFound {reason:"InvalidMapFile"}` and NO
     "Error opening file" box anywhere (review H1: the point is that the modal never fires);
     `map.open {path:"models/x.vmap"}` -> `-32602 InvalidParams`; `map.open {path:"maps//x.vmap"}`
     and `map.open {path:"maps\\x.vmap"}` -> `-32602 InvalidParams` (review M1); then
     `map.open {path}` of the file from 3 again -> `existing:true` (the leak gate on the session
     the open in 3 created).
  5. `open_map` (Rust) on the same paths: the tool's `hostPath`/`document` fields, `InvalidMapFile`
     for the truncated file in 4 (refused locally: no `map.open` on the wire) and `OpenFailed`
     (not retry-safe) for `opened:false` (the re-entrancy case: send two `map.open`s back to back
     and the second must not pop a box).
  6. `entity.setKeyValues {nodeID:<n>, values:{"targetname":""}}` -> `applied.targetname == true`,
     no warning, and `entity.get` reads `""` back (the empty-value fix); `map.save` then shows the
     empty value in the file.

## Review fixes

`[I: offline]` — the section the offline checks care about:

- The `..` rule is "two consecutive dots", not "a `..` segment" (the same rule
  `modeldoc.open`/`pet.open` apply), so the tool, the mock and the bridge agree bit for bit;
  `bridge/test_map_path.c` pins the exact reason sentences.
- A `:` in the path is refused (`path: no ':'`): a Windows file name cannot contain one, so the only
  thing it can name is an alternate data stream — never a map.
- `map_path_eq` folds case and both separator spellings and drops a trailing separator; after that
  the two paths must be equal (the relative-tail rule of the first cut was dropped, see "Review
  fixes (opus-4)" below). A bare `probe.vmap` and another addon's identical relative path both
  refuse (negative cases in the host test).
- The client refuses `\` and `"` in the path before dispatch: serde escapes both, and the bridge's
  own "no JSON escapes" gate would then refuse the raw parameter — the user sees a local
  `InvalidParams` instead of a confusing wire refusal.
- `map.open` is not a dirty gate and must not be documented as one (it *adds* a session; the
  current one keeps its unsaved state). `reload_map` remains the dirty-gated tool.

## FIX-3B (review-opus-3 M1/L2/L5/L6) — `map.reload` gates + keyvalue numbers

`[I: offline]`: `./bridge/build.sh` (host unit test + pins + the DLL link), `cargo clippy`/`cargo
test` for `hammer-mcp` + `cs2-mcp-common` + `client-mcp`, `bridge/test_entities.py --static`. Sources: the review
(`target/orch/review-opus-3.md`), the `map.reload`/`map.save` arms, the `entity JSON region` and the
disassembly of the pinned `ReloadFromDisk` slot. **No version bump** (still `m2.16`, no new method,
**no new error code**), and still no `0x` literal in `bridge/mcp_bridge.c`
(`static_no_new_hex_literals` counts the diff, so the evidence below lives here, not in the code
comments).

### §7 row change — `map.reload`

| Method | Status | Bridge m2.16 | Params | Result | Call | Notes |
|---|---|---|---|---|---|---|
| `map.reload` | implemented `[I: offline]` (was: no `expectDoc`, no untitled gate) | gate `NEED_HAMMER`; ordered refusals (below) | `{expectDoc:{id,epoch}, force?:bool}` — the params `idabins/docs/data/bridge-protocol.md` §7 already documented for this row | unchanged `{reloaded:true, path, discardedChanges, epoch}` | `CHammerEditorSession` slot 7 `ReloadFromDisk` (`HVS_RELOAD_FROM_DISK`) | see below |

- **M1 — untitled/new documents are refused before the call.** The arm now does `map.save`'s exact
  gate (`isNew || !pathp || !*pathp`) with `map.save`'s exact code and data text: `-32015
  UntitledDocument {"reason":"no path (Save-As is modal)"}`. `ReloadFromDisk` on a session with no
  file behind it is unverified (a modal that blocks the tools main thread, a no-op, or worse) and is
  never reached; `force:true` does not unlock it. The client's `reload_map` still refuses untitled
  documents locally first (FIX-3R) — the two gates are now the same rule, so the mock is no longer
  "ahead of the bridge" (its header note is updated).
- **M1 — the return value of `ReloadFromDisk` is unreadable, so the arm keeps ignoring it.** The
  pinned body (`hammer.json` `HVS_RELOAD_FROM_DISK`, `expect[2000921]=1346144`,
  `expect[2000930]=1346208`, session vtable slot 7) ends in a tail jump into the title/view refresh,
  which the flow doc already calls "the same tail as the save path"
  (`idabins/docs/flows/file-edit-reload-compile.md` §A step 4). No path out of that function sets rax
  to a status: the last `ret` is reached with rax holding whatever the final `callq` left there, i.e.
  the slot returns undefined garbage, not a success flag. Consequences: (a) the arm still answers
  `reloaded:true` unconditionally on success — and the field is documented as "the call was made on
  a document that has a path", **not** "the file was re-read"; (b) `bridge/s2sdk.h` now declares
  the slot `void` (it was `long long`, an unstated assumption) like its siblings
  `CModelDocEditorSession::ReloadDocument` / `CPetSession::ReloadDocument` — do not re-add a return
  type without a verified one. `Save` (slot 18) is the different case: its return **is** checked
  (`map.save`), and it stays `long long`. The review's suggested "-32603 with the dirty state when
  the reload did not take" is therefore not implementable without a return value or a re-read of the
  document; the client already keeps its counter and adds a note when the reply does not confirm
  `reloaded:true` (FIX-3R M1).
- **L2 — `expectDoc` is checked, in the position every other mutator uses it.** Order of refusals:
  `-32003 NoActiveDocument` → `-32018 DocumentNotEditable` → `-32004 DocumentChanged` (new,
  `expect_doc_ok`) → `-32015 UntitledDocument` (new) → `-32016 UnsavedChanges` → the call. A tab
  switch (or a user edit that moves the fingerprint) between the client's `doc.active` and this
  request is refused instead of reloading — and, with `force`, discarding — the *other* document.
  hammer-mcp's `reload_map` sends `expectDoc` built from that `doc.active` (`expect_doc(&doc)`),
  like `delete_nodes`/`create_entities`, so a `-32004` surfaces as `DocumentChanged` (retry-safe:
  nothing ran) and the unsaved-edit counter is not reset.
- **§8**: this row can now also produce `-32004 DocumentChanged` and `-32015 UntitledDocument`,
  both already in the client taxonomy. No new code, nothing retired.
- **Client compatibility**: the new arm *requires* `expectDoc`, so a mixed pair (the new DLL with a
  `hammer-mcp` older than this change) answers `-32004 DocumentChanged {reason:"missing"}` for every
  `map.reload`. Ship the two halves together, exactly as for the m2.15 mutators; the bridge version
  stays `m2.16`, so the handshake will not catch a stale client by itself.

### L6 — keyvalue number tokens

- `kv_next` no longer returns 0 (which *ends both passes* of `apply_keyvalues`) on a number token
  the DME setters cannot hold. It consumes the token, keeps its literal text in `nval` and sets
  `over = 2` (1 stays "the string did not fit `sval`"), so:
  - `{"a":1e39,"b":"x"}` no longer drops `a` **and** `b`: `b` applies, `a` is refused on its own;
  - `1e39` sent to a String attribute is stored as the text `1e39` again (the m2.15 §3.4 `nval`
    path — the review's "legitimate input" that the previous guard removed);
  - an `int`/`bool`/`float` attribute refuses that key with `applied:false` plus the warning
    `"<name>: number out of range for this attribute"` (a String attribute or a key the entity does
    not have keeps the text; other attribute types keep their `not writable` warning).
- The `(int)ival` truncation the review named is closed at the `SetValueInt` site: a token between
  `INT32_MAX` and `2^63` was representable as a `long long` and silently stored as a *different*
  int. An int/bool attribute now refuses it with the warning
  `"<name>: number out of range for an int attribute"`. In-range tokens (including `-2147483648`)
  are unchanged.
- **L5 (unchanged, documented limit):** the `edit.batch` undo label still comes from the ops
  *listed*, not the ops *that ran* — a raw mixed batch such as `[setKeyValues, create-that-fails]`
  is labelled `"MCP: batch"`. The label can only be chosen while the group is opened, so a scan
  cannot fix it. Already documented as the "Known limit" of `protocol-diff-m2.15.md` item 16 and
  restated in `batch_undo_label`'s comment; `hammer-mcp`'s `create_entities` sends creates only and
  cannot reach it. Not attempted here.
- **review-5 L5 (unchanged, documented limit):** `apply_keyvalues`' number token lives in a
  96-byte `nval` frame buffer and a longer token's text is cut to 95 bytes, so a String attribute
  (existing or newly created) stores the **cut** text with `applied:true` and no warning. The
  same cut was already possible for long in-range float tokens before this change; only its
  reachability changed. Raw-client reachable only (`hammer-mcp` sends keyvalues as JSON strings);
  the review's suggested fix (an over-long flag that refuses the key) would turn a today-stored
  value into a refusal, so it is documented instead — see the comment at the `over == 2` gate in
  `mcp_bridge.c`.
- **review-5 §3 (unchanged, documented limit):** `SetValueString` (`HFN_SET_STRING`,
  `hammer!0x1805cf3a0`) returns **without writing** in three cases — the interned symbol pointer
  is unchanged, attribute flag bit 6 is set (likely "read-only"), or the undo-state check fails.
  All three still answer `applied:true`. The unchanged-pointer case is harmless (the value already
  equals the write); flag bit 6 on a read-only attribute could be a wrong `applied:true`.
  Documented at the call site in `mcp_bridge.c`; closing it needs a read-back after the write, a
  live-verifiable change of its own.

**Offline proof.** `./bridge/build.sh` is green: the DLL links warning-free and the host unit test
`bridge/test_entity_json.c` runs **54 cases, 0 failures** (the same 54: the m2.15
`kv_next_refuses_unrepresentable_numbers` case became `kv_next_flags_unrepresentable_numbers` — the
same three tokens now parse, carry their text and set `over=2`, and two new in-range tokens
(`4294967296`, `8000000000000000000`) are pinned as `over=0`). `bridge/test_entities.py --static`
passes (including `static_no_new_hex_literals`: the addresses stay out of the code comments), and
`cargo test` is green for `hammer-mcp`, `cs2-mcp-common` and `client-mcp` (new cases:
`crates/cs2-mcp-common/tests/mock_entities.rs::map_reload_with_a_stale_expect_doc_changes_nothing` and
`hammer-mcp/tests/lifecycle.rs::reload_map_surfaces_a_document_changed_refusal_and_keeps_the_counter`,
plus the `sent["expectDoc"]` assertion in `reload_map_on_a_clean_document_reloads_and_reports_the_host_path`).
**Negative evidence** — one revert at a time, the new tests unchanged:

- `kv_next` back to the old `return 0` for both guards: `build.sh` exits 1, 54 cases / 1 failure,
  `FAIL kv_next_flags_unrepresentable_numbers 20digit=0 1e400=0 1e39=0 representable=1 (n=3)` — a
  token the setters cannot hold ended both `apply_keyvalues` passes, so the keys after it were
  dropped as well;
- the mock's `op_map_reload` without `expect_doc_ok`: `mock_entities` 35 passed / 1 failed —
  `map_reload_with_a_stale_expect_doc_changes_nothing` (a stale and an absent `expectDoc` both
  reloaded the document instead of answering `-32004`);
- `reload_map` without `expectDoc` in its request: `lifecycle` 18 passed / 4 failed —
  `reload_map_on_a_clean_document_reloads_and_reports_the_host_path` (the assertion),
  `reload_map_surfaces_a_document_changed_refusal_and_keeps_the_counter` (the mock's built-in
  `-32004 missing` replaces the injected wire error and the call returns a result),
  `forced_reload_discards_the_changes_and_resets_the_counter` and
  `reload_map_refuses_a_dirty_unknown_document_and_force_unlocks_it` (the mock's gate now requires
  the fingerprint, so those two reloads answer `-32004 missing`).
- The `1e39`-on-a-String-attribute and the INT32 truncation behaviours are **not** host-testable
  (`apply_keyvalues` needs the live data model); they are covered by the code paths and are on the
  orchestrator's live list below.

**Live probe list for the orchestrator (added to the m2.16 run):**

1. `bridge/test_reload.py` (now sends `expectDoc` on every reload, plus check 5: the *previous*
   fingerprint → `-32004 DocumentChanged`, dirty state and undo depth untouched);
2. `map.reload {force:true}` on a document with a stale (or absent) `expectDoc` →
   `-32004 DocumentChanged {reason:"missing_id"|"doc_id"|"epoch"}`, and no reload;
3. `map.reload` on an untitled document (`File > New`, no save) → `-32015 UntitledDocument
   {"reason":"no path (Save-As is modal)"}`, no modal, no path;
4. a raw `entity.setKeyValues` `{"values":{"a":1e39,"b":"x"}}` on an entity whose `a` is a String
   attribute → `applied.a:true`, `a` == the text `1e39`; on an int attribute → `applied.a:false`
   with the "number out of range" warning and `b` applied; `{"a":4294967296}` on an int attribute →
   refused with the int warning (never a stored 0).

## Review fixes (opus-4)

`[I: offline]` — the second review of this pass; every item is addressed here or is explicitly
skipped:

- **C1 (critical).** Session slot 26 returns a `CUtlString *`, never text (`lea rax,[rcx+0x6B0]` at
  `hammer!0x13ED70`). The bridge now reads each session's path with `map_session_path`
  (`CMapDocSession_Path(docSession)` + a readability probe), the prototype in `bridge/s2sdk.h` is
  `const CUtlString *`, and the result's `path` is real text (`windows_to_host` maps it). Negative
  evidence: `bridge/test_map_path.c`'s `c1_case()` asserts that the CUtlString's ADDRESS never
  compares equal to a path while its `m_pString` does.
- **H1 (high).** Two independent gates before `OpenFile`: the Rust tool parses the whole file with
  `hammer-mcp/src/vmap` (`InvalidMapFile`, nothing sent, `retry_safe:true`), and the bridge compares
  the file's DMX header words with the reader's own header (`map_path.h`'s `map_vmap_header_ok`) —
  a 16-byte prefix would accept `keyvalues2`/`binary 8`, which share `<!-- dmx encodin`. The bridge
  answers `-32014 AssetNotFound {reason:"InvalidMapFile"}`. `OpenFailed` is no longer
  `retry_safe`.
- **M1 (medium).** The canonical spelling is required everywhere (bridge gate, Rust tool, mock):
  no `\`, no empty segment, no `.`/`..` segment, no segment ending in `.` or a space, `maps/` prefix
  with forward slashes only. `bridge/test_map_path.c`, `lifecycle.rs`'s
  `map_content_path_reasons_match_the_bridge` and `mock_entities.rs` pin the same sentences.
- **M2 (medium).** `HammerWorld` keeps a session LIST (`sessions`), so the mock answers
  `{opened:false, existing:true, active:<is the active session>}` for any file a session holds;
  `open_map_of_a_background_session_reports_existing_and_not_active` (hammer-mcp) and
  `map_open_of_a_background_session_is_existing_and_not_active` (cs2-mcp-common) cover A -> B -> A.
- **L1.** `force` is gone (kept out of the params table above).
- **L2.** The relative-tail rule is gone from `map_path_eq`: two paths must be equal after folding.
- **L3.** `map.open` logs the requested path, the resolved host path and (on success) the session
  path, so a root-spelling mismatch is visible in one line of the bridge log.
- **L4.** `map_host_gate`, `map_header_gate` and `map_session_of` are `noinline`, like
  `editor_app_root`.
- **L5.** The unreadable-vector case is still `-32603`, and the doc row was corrected: a session
  whose path cannot be read is skipped, never an error.
- **L6.** The success `path` comes from the session `OpenFile` returned; the Rust tool no longer
  trims the path (review L6b) and the dead `maps\\` branch is gone (L6c).

Skipped: the review's note that `sessionCount` is an `int` in the binary (the bridge keeps `u32`
with its `n > 4096u` guard, which catches a negative count as a huge one — harmless, as the review
says; changing the declared type would not change the layout or the behaviour).

## Empty keyvalue value (m2.15 `apply_keyvalues`, found live in this pass)

`entity.setKeyValues {"k": ""}` used to answer `applied:false` with no warning: `dm_intern("")`
returns a `CUtlSymbolLarge` with `m_pString == NULL` — Hammer's representation of the empty string —
and the write was skipped by `if (vsym.m_pString)`. The disassembly of the setter the bridge binds
(`HFN_SET_STRING` = `CDmAttribute::SetValueString` at `hammer!0x1805CF3A0`) settles it:

- its body never dereferences the new string: it loads the symbol's pointer and stores it into the
  attribute's storage (`movq (%rsi),%rax; movq %rax,(%rcx)` at `0x1805cf54b`/`0x1805cf550`), after
  an unreadable-equal early-out against the current value (`call 0x180156c60` returns the stored
  `char *`; `cmp %r8,(%rax)`), and the undo record it builds holds the same two pointers;
- Hammer's own `CDmElement` string setter (`hammer!0x1805C2E30` -> `CDmElement_SetValueString_Interned`
  at `hammer!0x1805C2E70`) interns the `char *` through `IDataModel::GetSymbol` (vfn+424/`+0x1A8`,
  the same call `dm_intern` makes) and passes the resulting symbol to that same setter with **no**
  null check — so a NULL `m_pString` is exactly what Hammer writes for `""`;
- and the read side already treats it as the empty string: `attr_cstr` (`mcp_bridge.c`) reads a NULL
  storage pointer as `""` — "an empty CUtlSymbolLarge is a null pointer (live: `targetname ""`)" —
  which is the live-verified `targetname ""` behaviour of m2.15.

So the write is now unconditional for a String attribute (existing and newly created) and answers
`applied:true`. Covered by `bridge/test_entity_json.c`'s `kv_next_empty_string_value` (the empty
value decodes whole and does not stall the parser), the mock's `empty_string_values_are_applied`,
and `keyvalues_arg_bounds_and_coercion` in `hammer-mcp` (the tool sends `""` unchanged).
