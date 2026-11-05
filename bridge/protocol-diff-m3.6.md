# Protocol changes in m3.6 — lane MAP (`map.open`, `map.reload`, `map.save`)

Bridge build `m3.6` (Rust bridge) = m3.5 + this group.

**One row group moved from "not ported" to served.** The Rust bridge now answers the three
Hammer map-lifecycle methods byte-for-byte like the C bridge at m2.20. `map.run` stays a blocked
row (`registry.rs:157`, the C never implemented it either). Nothing else changed: the method
table is unchanged (84 registered / 69 implemented), no new error code, no changed field.

## The wire, field by field

| Where | m2.20 (C bridge) | this build |
|---|---|---|
| `capabilities.methods` | every implemented, unhidden row whose needs are verified | the same, **intersected with the ported list**: the previous rows + `map.open`, `map.reload`, `map.save` |
| `map.open` result | `{"opened":b,"existing":b,"path":<json>,"active":b,"epoch":N}` (`mcp_bridge_map.inc:252-255`, `:289-297`) in a 2048-byte result buffer | identical bytes, same buffer size, overflow answered `-32603 "InternalError: map.open result too large"` (`mcp_bridge.c:6091`) |
| `map.open` errors | `-32602 InvalidParams` (missing/empty path; the raw-token escape scan; `map_open_path_ok`'s reason strings), `-32017 PrerequisiteFailed` `no_app_root`/`no_addon`, `-32014 AssetNotFound` `no_such_file`/`not_a_file`/`InvalidMapFile`, `-32003 NoActiveDocument` `hammer_unavailable`, `-32603 InternalError` `host_path_too_long`/`session_vector_unreadable` | identical, same order of gates (path shape → host file exists → binary-v9 DMX header → resolve app → session walk → OpenFile) |
| `map.reload` result | `{"reloaded":true,"path":<json>,"discardedChanges":b,"epoch":N}` (`mcp_bridge.c:6157-6165`) | identical (the path is the PRE-reload copy, review-5 L2; `discardedChanges` is the pre-reload dirty flag) |
| `map.reload` errors | `-32003 NoActiveDocument`, `-32018 DocumentNotEditable`, `-32004 DocumentChanged` (`expectDoc`), `-32015 UntitledDocument {"reason":"no path (Save-As is modal)"}`, `-32016 UnsavedChanges {"reason":"document is dirty; pass force:true to discard"}` | identical |
| `map.save` result | `{"saved":b,"path":<json>,"skippedClean":b}` (`mcp_bridge.c:6199-6206`) | identical, including `skippedClean:true` when the document was clean and Save was NOT called |
| `map.save` errors | `-32003`, `-32018`, `-32015` (same data text) — note: no `expectDoc` gate and no epoch member, as in the C | identical |
| the `force` member (`map.reload`) | `json_find(params,"force")[0] == 't'` (`:6121-6125`) | identical |

## What the three bodies do

* `map.open` (`bridge/mcp_bridge_map.inc:215-272`): the four gates run before any `OpenFile`,
  because OpenFile pops modal boxes that wedge the tools main thread:
  1. the path shape (the canonical addon-relative spelling only — `map_open_path_ok`),
  2. the host file must exist under `<app>\content\csgo_addons\<addon>\` (`CreateFileA` +
     `GetFileType` with the C's flags),
  3. the file's first `MAP_VMAP_HEADER_MAX` (512) bytes must carry the binary-v9 DMX header
     (`map_vmap_header_ok`),
  4. the session walk (`map_session_of`, `mcp_bridge_map.inc:154-211`): the app's
     `m_Sessions` vector (`m_Size` at 0, `m_pMemory` at 8), each entry `obj_is`-checked, its
     doc path text read from the docSession's `m_Doc.m_Path.m_pString` (the `char *` at
     +0x6B0, review C1 — NOT slot 26, which returns the CUtlString's address) and compared
     with `map_path_eq`. An existing match answers `existing:true` and OpenFile is NOT called;
     an unusable vector refuses `-32603 session_vector_unreadable` (fail loudly, never call
     OpenFile with the leak gate off); an unusable per-session path is skipped (review L6).
  `OpenFile` itself is the generated typed vtable field
  `CBaseEditorApp_vtable.OpenFile` (`S2_SLOT_CBaseEditorApp_OpenFile`, 18), called with the
  requested ADDON-RELATIVE path and flags 1.
* `map.reload` (`mcp_bridge.c:6097-6165`): `active_doc` → `doc_editable` → `expect_doc_ok`
  (the C's `mcp_bridge.c:1920-1938`, over `g_fp.docSeq`/`g_fp.epoch` — the hammer lane's
  `fingerprint_ids`) → the doc-session reads (`m_pOwner`, `m_bIsNew` as a byte,
  `m_Doc.m_Path.m_pString`) → `DocOrDependencyChanged` through the bind table (`HFN_DOC_
OR_DEPENDENCY_CHANGED`) → the untitled/dirty gates → the path text copied into a 512-byte
  buffer BEFORE the call → `ReloadFromDisk` (the generated typed field
  `CBaseEditorSession_vtable.ReloadDocument` at `S2_SLOT_CBaseEditorSession_ReloadDocument`,
  7; `void`, no return value read — review M1) → `doc_fingerprint` → the reply.
* `map.save` (`mcp_bridge.c:6166-6233`): `active_doc` → `doc_editable` → the same doc-session
  reads → the untitled gate → `DocOrDependencyChanged` → `Save(session, pathp, pathp, 0, 0)`
  only when dirty (the generated typed field `CBaseEditorSession_vtable.Save` at
  `S2_SLOT_CBaseEditorSession_Save`, 18) → the reply with `skippedClean`.

## Places the port deliberately differs from the C (no byte on the wire for any readable path)

1. **Unguarded game reads got `mem_ok` guards**: the `*pathp` byte of the untitled gate
   (`mcp_bridge.c:6186`, the C derefs unguarded) is probed before it is read. Readable inputs
   produce the same bytes; a garbage pointer produces the C's "untitled" refusal instead of a
   crash.
2. **`map.save`'s reply path is probed with a 4096-byte `md_str`** (the C hands the raw
   `char *` to `sb_addjson`, `:6204`, reading unbounded). Same decision as the hammer lane's
   `doc_facts` (`protocol-diff-m3-hammer.md` #2); no Hammer-resolved path is anywhere near it.
3. **The doc path COPY uses `md_str` (`mcp_bridge.c:5169-5186`), not the hammer lane's
   `probe_str`**: `md_str` answers `-1` (unusable) for a string of exactly `cap-1` bytes and
   keeps a raw (possibly mid-code-point) prefix at a cut; `probe_str` answers 1 there and trims
   to a UTF-8 boundary. The C's `map_session_path` requires `md_str(...) == 1`, so the port
   calls the same semantics (`ops/map.rs`'s `md_str`, cross-platform so the host tests can
   drive it; pet's is `cfg(windows)`).
4. **Log lines with game paths go out `String::from_utf8_lossy`** (the C `logline`s raw ANSI
   bytes). Log-only; the wire bytes are the raw bytes as in the C.
5. **`-32003`/`-32018`/`-32004` data objects** are built in `OpErr`'s 512-byte buffer, not the
   arms' 192-byte scratch. The reasons are the C's short strings, so both truncate at the same
   byte (same note as `protocol-diff-m3-hammer.md` #5).
6. **`Save`'s return is read as the C header's own `u8`**: the generated typed field returns
   a Rust `bool`, but the C header spells the same slot `u8 (*Save)(...)` (return defined
   only in al, `bridge/s2sdk.h:363`) and the C reads `u8 rc`, testing `rc ? 1 : 0`
   (`mcp_bridge.c:6191-6192`). The slot entry is therefore taken through the generated
   `CHammerEditorSession_slots::Save` index and transmuted to that `-> u8` signature, tested
   `!= 0` - reading a Rust `bool` would be undefined for any flag byte that is not exactly
   0 or 1. `OpenFile`'s generated `*mut CBaseEditorSession` return is compared to null
   exactly as the C does.
7. **The header window's read goes through `std::fs::File::from_raw_handle` + `std::io::Read`**
   because this crate's `windows-sys` feature set has no `ReadFile` (the pet lane's
   `read_vpcf` made the same substitution). Same `CreateFileA` flags, same "failed read = a
   zero-byte read" answer.
8. **Non-Windows builds have no reader.** The registration, gates, validation and builders
   exist; the `-32017 hammer.dll` gate answers before any body. Any body that still ran fails
   loudly (`hammer_unavailable`), never inventing an answer. The live parity pass runs the
   Windows build.

## Parity coverage (the live pass the orchestrator runs)

* `map.reload` — `bridge/test_reload.py` (five live cases: dirty without force → `-32016`,
  `force:true` → `reloaded, discardedChanges:true, epoch bumped`, clean reload,
  stale `expectDoc` → `-32004`).
* `map.save` — `bridge/test_client.py` case 11 (`skippedClean` on a clean document),
  `bridge/test_undo.py` (the save round trips around undo), `bridge/test_geometry.py` and
  `bridge/test_entities.py` / `bridge/test_set_keyvalues.py` (mutate and save).
* `map.open` — no dedicated live script exists in the C suite either (the C bridge's open
  flows are exercised by the operator cycle that loads `probe.vmap`); the path-shape and
  header gates' C text is covered by the host tests `bridge/test_map_path.c` (wired into
  `bridge/build.sh:48-49`), whose Rust twins are `crates/cs2-bridge/tests/map_path.rs`.

The Rust-side host tests are `crates/cs2-bridge/src/ops/map.rs` (`#[cfg(test)] mod tests`:
builder bytes, param gates, host and header gates over injected facts, `expect_doc_ok`,
`md_str`, and the three arms over a fake reader) and `crates/cs2-bridge/tests/ops_map.rs`
(the `-32017` gate over the real `wiring::BridgeState`, capabilities).
