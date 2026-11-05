# Proposed protocol changes for bridge m2.13 (particle editor second pass: live edit, undo, render)

Additive over m2.12; no major bump, **no new error codes**. Five new methods (all executed on the
tools main thread like every other game call): `pet.text`, `pet.apply`, `pet.undo`, `pet.redo`
(WP-B1) and `pet.render` (WP-B2). RE: `hammer-mcp/research/pet-re.md` "Second pass"; shared
contracts: `target/orch/pet-plan2.md` §0.1. Consumers: `hammer-mcp`'s `particle_live_*` /
`particle_render` tools behind capability gating (wave 2); nothing in Rust ever sees an address.

**Modality note.** None of the five methods opens a modal. `pet.text` serializes the live tree to a
private path with `CPetDoc::SaveToFile` (it neither changes the session's path nor the dirty flag),
`pet.apply` replaces the tree from already-validated text, and `pet.undo`/`pet.redo` are the same
whole-document snapshot restore the editor's own Ctrl+Z/Ctrl+Y run. Malformed text never reaches
pet's loader: `pet.apply` runs the same `vpcf_check` grammar + root-class gate `pet.reload`/
`pet.open` run (m2.12 §7.1), and it parses into a KV3 context **before** it opens an undo step.
Untitled documents are allowed by all four live-edit methods (`pet.save` keeps refusing them).

## §7 method table — new rows

Every row was `[I] until live`. The 2026-10-13 runs prove them — `bridge/test_pet.py` 17/17 and
`bridge/test_pet_live.py` (core, `--render`, `--namedvalue`, `--depth`) all PASS against a running
pet — so the five rows below now carry `[V runtime 2026-10-13]` (the live-found fixes are
recorded in §5/§6).

| Method | Status | Bridge m2.13 | Params | Result | Call | Notes |
|---|---|---|---|---|---|---|
| `pet.text` | implemented `[V runtime 2026-10-13]` | gate `NEED_PET` | `{}` | `{text:string, bytes:int, path:string, dirty:bool, isNewFile:bool, canUndo:bool, canRedo:bool}` | common prologue; `PETFN_DOC_SAVETOFILE(doc, PET_TEXT_PATH, &err, cb)` (`cb` = a static 64-byte zero block, read at `+0x38` and skipped when 0); `pet_read_vpcf(PET_TEXT_PATH)` (m2.12 helper); the two flags from `PETVS_SESSION_ISDIRTY` (11) and `u8 session+PETOFF_SESSION_ISNEW`; `canUndo`/`canRedo` from `PETVS_UNDOHIST_HASENTRIES` (3) on `doc+PETOFF_DOC_UNDOHIST` and its `+PETOFF_UNDOHIST_PARTNER` | `text` is the live tree serialized as KV3 text vpcf66 (the same writer `pet.save` uses), up to `PET_TEXT_MAX` = 393216 bytes; `bytes == len(text.encode())`. `path` is the **document's own** `GetPath` (1024-byte copy), not the private file. The private file `<LOCALAPPDATA>\cs2mcpbridge\pet_text.vpcf` is left in place and overwritten each call; it is outside every content search path, so the asset system never sees it. `-32017 PrerequisiteFailed {reason}`: `text_path_too_long`, `save_to_file_failed` (with the callee's `CUtlString` err text folded in, bounded), `text_read_failed`, `text_too_large` (a >384 KB or reply-buffer-overflowing document - never a cut string). Empty reply-buffer overflow is answered as `text_too_large`, not `-32603`. |
| `pet.apply` | implemented `[V runtime 2026-10-13]` | gate `NEED_PET \| NEED_TIER0` | `{text:string (1..393216 bytes after unescape), label?:string (1..64 printable ASCII 0x20..0x7E without `"` or `\`, default "MCP apply")}` | `{applied:true, label:string, bytes:int, dirty:bool, canUndo:bool, canRedo:bool, arrays:[{type,count,classes}] | null, arraysError?:{reason}}` | (1) params: `json_text_unescape` (`bridge/json_text.h`, **NUL-terminates** the result — `LoadKV3` reads a C string; found live 2026-10-13, `extraneous tokens past end of KV3 document` without it) on the raw `text` value; (2) `vpcf_check(text, len, &why)`; (3) common prologue; (4) `memset(g_petKv3Ctx)` + `PETFN_KV3CTX_CTOR(ctx,0)` + `PETFN_KV3CTX_ROOT(ctx)` (= `ctx+120`) + tier0's `?LoadKV3@@...` export resolved per call via `GetModuleHandleA("tier0.dll")` (must equal the verified `PIN_TIER0` base); (5) `PETFN_KV3_STRIP_TO_SCHEMA(kv, PETFN_SCHEMA_TYPESCOPE())` (`-32017 no_type_scope` when null) -> `PETFN_DOC_BEGINUNDOSTEP(doc, label, 1)` -> `PETFN_ROOT_LOADFROMKV3(root, kv)` -> `PETFN_ROOT_FILTER_FUNCTIONS(root, PETFN_ROOT_BEHAVIORVERSION(root))` -> every selection entry set to pet's "none" `{9,-1}` (`PETOFF_DOC_SELECTION_COUNT/DATA`) -> `PETFN_ROOT_REFRESHCACHES(root, doc+PETOFF_DOC_CACHES)` -> `PETFN_NOTIFY_FUNCLIST(block)` with a 4-qword block whose `[3]` is the session -> `PETFN_SESSION_ONDOCMODIFIED(session)`; (6) `PETFN_KV3CTX_DTOR(ctx)`, free the text, `arrays` via the `pet.active` builder | **Parse before the undo step**: a fail anywhere through (2)-(4) leaves the tree and the history untouched (a `LoadKV3` failure must not record an empty step). `-32602 InvalidParams` with message `text (string) required`, `text: 1..393216 bytes`, or `label: 1..64 printable ASCII (0x20..0x7E), no quote or backslash`; the unescape refusals (`unterminated`, `bad_escape`, `nul`, `non_ascii_escape`, `surrogate`, `control_byte`, `too_long`) are `-32602 InvalidParams <reason>`. `-32602 UnloadableFile {reason, path}` from `vpcf_check` (`path` = `"pet.apply:text"`) and for `{reason:"kv3_load_failed", path:<tier0 err text, <=99 chars, or "?">}` (the `CUtlString` the export filled is leaked on purpose: the bridge has no tier0 free; rare path, bytes). `-32017 PrerequisiteFailed {reason}`: `kv3_root_null`, `tier0_handle_mismatch`, `loadkv3_export_missing`, the prologue's `bad_undo_history`/`bad_redo_history`, and the prologue errors. After the tree changed nothing is an error: an unlistable rebuilt tree answers `applied:true, arrays:null, arraysError:{reason}` (a retry would add a second undo step). The strip and the filter are `CPetDoc::LoadFromFile`'s own, in its order (IDA-checked 2026-10-13): unknown keys and unregistered/out-of-version function classes are dropped exactly as on open. `LoadKV3` is called as `LoadKV3(kv, &err, text, &PETG_KV3ID_VPCF66, "", 0)` - the IDA-checked form pet's own text-overload callers use (flags 0, empty name). |
| `pet.undo` | implemented `[V runtime 2026-10-13]` | gate `NEED_PET` | `{}` | `{done:bool, reason?:"nothing_to_undo", dirty:bool, canUndo:bool, canRedo:bool, arrays}` | common prologue; `HasEntries(doc+PETOFF_DOC_UNDOHIST)`; if it has entries `PETFN_DOC_UNDO(doc)` then `PETFN_NOTIFY_FUNCLIST(block)` + `PETFN_SESSION_ONDOCMODIFIED(session)`; flags and `arrays` re-read | pet's `Undo` emits neither `FunctionListChanged` nor `DocumentModified`; the bridge emits both so the preview recompiles and the title says modified. An empty history is a **result** (`done:false`, `reason:"nothing_to_undo"`), never an error and never a crash. |
| `pet.redo` | implemented `[V runtime 2026-10-13]` | gate `NEED_PET` | `{}` | `{done:bool, reason?:"nothing_to_redo", dirty:bool, canUndo:bool, canRedo:bool, arrays}` | as `pet.undo` but target = `hist+PETOFF_UNDOHIST_PARTNER` and `PETFN_DOC_REDO(doc)` | same two extra notifications. `done:false` + `reason:"nothing_to_redo"` when the partner history is empty. |
| `pet.render` | implemented `[V runtime 2026-10-13]` (WP-B2) | gate `NEED_PET` | `{width?:int 16..4096 (default 1024), height?:int 16..4096 (default 768), name?:string (render_name rules)}; width*height <= 8388608` | `{path:"Z:\\app\\renders\\<stem>.png", file:"<stem>.png", width:int, height:int, bytes:int, format:"RGBA8888", hasDocument:bool, stats:{untouchedPixels, nonBlackPixels, meanLuma, minLuma, maxLuma, crc32}, durationMs:int}` | params first; `resolve_pet()`; `app+PETOFF_APP_MAINWND` (`PETV_MAINWND_VT`) -> `+PETOFF_MAINWND_PREVIEW` (`PETV_PREVIEW_VT`) -> `+PETOFF_PREVIEW_SCENEWIDGET` (`PETV_SCENEWIDGET_VT`) -> `+PETOFF_SCENEWIDGET_SCENE` (`PETV_TOOLSCENE_VT`) and `+PETOFF_SCENEWIDGET_CAMERA` (`PETV_SCENECAMERA_VT`, a **candidate** identity); `PETFN_TOOLSCENE_RENDER(scene, camera, 0, 0, &vp{1,0,0,w,h,0,1}, px, 0)` (format 0 = RGBA8888); then the `world.render` tail (sentinel per u32, the stats loop with no sRGB LUT, `crc_*`, `png_encode`, `CreateDirectoryA("Z:\\app\\renders")`, `write_file_atomic`) | `-32602 InvalidParams` for out-of-range width/height, a product over 8388608, or a bad `name`, before any object is touched. `-32019 NoViewport {reason}`: `pet_unavailable`, `no_main_window`, `bad_main_window`, `no_preview`, `bad_preview`, `no_scene_widget`, `bad_scene_widget`, `no_scene`, `bad_scene`, `no_camera`, `bad_camera` (a bad camera also logs the observed vtable RVA against `PIN_PET` so the sigs candidate can be fixed - fail closed). `-32020 RenderFailed {reason:"readback did not run (no pixels written)"}` when the sentinel count is unchanged. `hasDocument` is whether a document is active, not a requirement. The `PETV_SCENECAMERA_VT` candidate identity was confirmed live 2026-10-13 (no `bad_camera` in any run; 320x240/480x320 render in ~5-15 ms, `untouchedPixels == 0`, PNG shows the preview). |

## §8 error codes — no new codes

m2.13 adds **no** error code. Reused, with the reason strings above:
`-32003 NoActiveDocument` (`no_root`, `bad_root`, plus the `active_pet` reasons `pet_not_loaded`,
`pet_verifying`, `pet_mismatch`, `pet_unavailable`, `no_session`, `bad_session`, `no_document`,
`bad_doc`), `-32015 UntitledDocument` (`pet.save` only), `-32016 UnsavedChanges` (`pet.reload`),
`-32017 PrerequisiteFailed`, `-32019 NoViewport`, `-32020 RenderFailed`, `-32602 InvalidParams`,
and `-32602 InvalidParams` with message `UnloadableFile` (`data:{reason, path}` - the m2.12 gate
reasons plus `kv3_load_failed` for `pet.apply`). `pet.undo`/`pet.redo` deliberately answer an empty
history as a **result** (`done:false`, `reason`), not as an error.

## §5/§6 notes

- Method count: **77 registered / 59 implemented** (m2.12: 72/54; the five new rows are `impl: 1`).
  `control.version.bridgeVersion` is `m2.13`. No new module pin (`PIN_MODULE_COUNT` stays 12); the
  m2.13 sigs entries add macros to the existing `pet.dll` block only.
- `control.capabilities`/`control.ping` gate the new methods on the same `NEED_PET` (and, for
  `pet.apply`, `NEED_TIER0`) mask the older pet methods use; until pet.dll verifies they answer the
  generic `-32017 PrerequisiteFailed {module:"pet.dll", check:"identity", state:...}`.
- `sb_addjson` now emits `\n`, `\r` and `\t` as their two-character escapes (still valid JSON). It
  shrinks `pet.text` from a ~6x worst case to ~1.3x for the CRLF/tab-heavy writer output, which is
  what keeps a 384 KB document inside the ~1 MB reply line. The Rust client parses both forms; the
  Python test clients use `json.loads`.
- `pet.apply`'s JSON string-body unescaper is `bridge/json_text.h`, a pure-C header (no includes,
  no libc, no Windows types) with the same conventions as `kv3_check.h`. `bridge/test_json_text.c`
  is its HOST unit test, compiled and run by `build.sh` before the Windows build (13 cases:
  short escapes, `\uXXXX`, and the refusals `nul`/`non_ascii_escape`/`surrogate`/`unterminated`/
  `control_byte`/`bad_escape`/`too_long`, plus `json_text_end` and a byte-identical round-trip of
  the `probe.vpcf` fixture; a missing fixture fails the build). It refuses a raw byte `< 0x20`
  anywhere in the string and every `\uXXXX` above 0x7F: serde_json writes non-ASCII as raw UTF-8,
  so an escaped non-ASCII is a different producer and a lone surrogate cannot be a document byte.
- **Two live-found fixes (2026-10-13, b94266d; both refused safely before the fix, no crash):**
  (1) `pet.apply`'s unescaped text was not NUL-terminated, so tier0's `LoadKV3` (a C-string
  reader) kept reading and answered *"extraneous tokens past end of KV3 document"*; the unescaper
  now NUL-terminates (test case in `test_json_text.c`), and the tier0 `CUtlString` error text on
  that path is sanitized to ASCII before it goes on the wire. (2) the request params scan counted
  braces inside JSON strings, so a `text` value containing `{{{` was mis-scanned and lost its
  params; the scan now skips strings (`test_pet_live.py` covers a brace-in-string apply).
- The KV3 parse context is one `static` 16-byte-aligned 16384-byte buffer (`PET_KV3CTX_BYTES`),
  zeroed before each `PETFN_KV3CTX_CTOR`. Static because the freestanding Windows build has no
  `__chkstk` for a large stack local; the RE says the live object is 4608 bytes (`sub rsp, 12A0h`
  frames) with its arena flag byte at `ctx+4400`, so 16 KB over-allocates. The ctor does not clear,
  hence the explicit zero.
- The `LoadKV3` export is resolved **per call** by its mangled name
  (`?LoadKV3@@YA_NPEAVKeyValues3@@PEAVCUtlString@@PEBDAEBUKV3ID_t@@2I@Z`) from
  `GetModuleHandleA("tier0.dll")`, and the handle must be the verified `PIN_TIER0` base
  (`tier0_handle_mismatch` otherwise). It is never cached across a module state change. The
  `CUtlString` err the export fills on failure is leaked on purpose (there is no tier0 free in the
  bridge); the path is rare and the leak is bytes.
- Dirty/undo semantics (plan §4, for the tool docs and the live run): `pet.apply` and `pet.undo`/
  `pet.redo` set modified; undoing back to the saved state leaves the document dirty (pet's own
  behaviour). `particle_reload`/`particle_open` keep refusing a dirty document without `force`;
  selection is `{type, index}` pairs at `doc+0x68/0x70`, so a live edit rebuilds the tree under
  the same exposure as the editor's own Ctrl+Z (the plan's step 17, a node selected during an
  apply, was **not exercised**: both runs were headless).
  `BeginUndoStep(notify=1)` emits `DocumentModified` before the tree changes and the bridge emits
  `OnDocModified` again after the load; the handler is idempotent (`session+0x2B9` +
  `SetModified(1)`), and the preview recompile happens on the next `AppRunFrame`.
- Text size: the request line is 1 MB and the reply line `MAX_LINE` (1 MB) with a `MAX_LINE-2048`
  scratch buffer for the pet result; `PET_TEXT_MAX` = 384 KB both ways. Real game `.vpcf`s are a
  few hundred KB at most; a bigger one falls back to the file route.
- Live results (2026-10-13, all PASS): every probe in the list below was answered
  (`test_pet.py` 17/17; `canUndo` survives save and forced reload; two live bugs found and fixed
  above). Steps:
- Original live-probe list for the orchestrator (`bridge/test_pet.py`, steps 12-17): `pet.text` on
  the open probe starts with `<!-- kv3`, carries `C_OP_ContinuousEmitter`, `bytes ==
  len(text.encode())`, `dirty/canUndo/canRedo false`, `path` ends `probe.vpcf`; `pet.apply` of that
  text with one more `C_OP_Decay` appended reports `applied:true`, `dirty:true`, `canUndo:true`,
  `canRedo:false` and `pet.active` agrees; `pet.undo` -> `done:true`, `pet.redo` -> `done:true`, a
  second `pet.undo` -> `done:false reason nothing_to_undo`; the refusals (`{{{ not kv3`,
  truncated body, wrong root class, empty text, a 65-char label) leave the document unchanged and
  the main thread draining. A tolerated-but-dropped unknown `_class` in `m_Operators` returns
  `applied:true` with the count unchanged (m2.12 §7.1).
