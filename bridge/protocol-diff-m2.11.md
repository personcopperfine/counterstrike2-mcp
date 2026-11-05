# Proposed protocol changes for bridge m2.11 (ModelDoc first pass: .vmdl lifecycle)

Additive over m2.10; no major bump. One new pin: `PIN_MODELDOC` =
`modeldoc_editor.dll` (late-loaded: it only appears when a ModelDoc window is open;
`modules_poll` picks it up then, `verify_all_thread` verifies it last). Six new methods,
all gated `NEED_MODELDOC` and all executed on the tools main thread like every other
game call. RE: `hammer-mcp/research/modeldoc-re.md`; design: `target/orch/modeldoc-plan.md`
§1 "Bridge methods (m2.11)". Consumer: `hammer-mcp/src/tools/model_live.rs` (WP6) behind
capability gating; nothing in Rust ever sees an address.

**Modality warning (test-side guard, design §1 "Policies"):** `OpenFile` pops a modal for a
missing file, a path outside the content dir, and load errors — a modal blocks the main
thread while it is up (queued requests still time out normally via the IPC `QueueTimeout`
sweep). Only host-validated paths of files under `content/csgo_addons/<addon>/` are sent from a tool,
written **relative to that addon content root** (`models/x/y.vmdl`). `ReloadDocument` never prompts. The "Compile?" modal variant
`SaveAndCompile` (session slot 21) is deliberately never called; `Compile` (non-virtual)
is modal-free.

## §7 method table — new rows

| Method | Status | Bridge m2.11 | Params | Result | Call (page) | Notes |
|---|---|---|---|---|---|---|
| `modeldoc.active` | ready [V runtime 2026-10-12] | implemented | `{}` | `{doc:{path, dirty:bool, isNewFile:bool, hasAsset:bool, compileStatus:null, docType:"vmdl"}}` or `{doc:null, reason}` | `CreateInterface("ToolSystem2_ModelDoc")` (export, **ret 0 = found**) identity == `MDG_APP_OBJ`; `activeSession` at `MDOFF_APP_SESSION`; session/doc identity-checked against `MDV_SESSION_VT`/`MDV_DOC_VT` per call (doc is replaced on every load/reload); `GetPath` (slot 13), `IsModified` (slot 11) | read-only, main thread. **Never errors on no document** (but while `modeldoc_editor.dll` is not verified the generic `-32017 PrerequisiteFailed {module}` gate answers first, so the three `modeldoc_*` module reasons below are defensive only). `reason` is one of `modeldoc_not_loaded`, `modeldoc_verifying`, `modeldoc_mismatch`, `modeldoc_unavailable`, `no_session`, `bad_session`, `no_document`, `bad_doc`. `compileStatus` is always `null`: `MDOFF_SESSION_COMPILE_STATUS` is a status-sink object, never read [V runtime]. `path` is an absolute Windows path from `GetPath` (`""` when unset). |
| `modeldoc.open` | ready [V runtime 2026-10-12] | implemented | `{path: string relative to the addon content root, e.g. models/x/y.vmdl; 1..512 chars, no ".."}` | `{opened:bool, path, active:{...piece as modeldoc.active doc}\|null}` | `app->OpenFile(app, path, 1)` (app slot `MDVS_APP_OPENFILE`, returns session or `0`) | `-32602` on a bad path, refused before `OpenFile` (its modals block the main thread): JSON escapes, control characters, absolute or drive-qualified paths, a non-`.vmdl` extension, `..`. `-32003 NoActiveDocument {reason:"modeldoc_unavailable"}` when the app object does not resolve. `opened:false` when `OpenFile` returned `0` (failure / re-entrancy guard `app+0x1A3`) - a **result**, not an error, because the call may also just re-activate an already-open file. `active` is re-resolved after the call (the opened session when it succeeded, else the prior one). An already-open path re-activates instead of duplicating; a genuinely second file opens a second session and both stay open (closing sessions is not exposed). Relative paths resolve in the `CONTENT` then `GAME` search paths, which are rooted at the addon: `csgo_addons/<addon>/models/...` returns `0` [V runtime], `models/...` opens. May pop a modal for a missing/broken file - the tool pre-checks; say so in any report. |
| `modeldoc.reload` | ready [V runtime 2026-10-12] | implemented | `{force?:bool}` | `{reloaded:bool, path, discardedChanges:bool}` | `session->ReloadDocument(session)` (session slot `MDVS_SESSION_RELOAD`) | `-32003 NoActiveDocument {reason}`; `-32015 UntitledDocument` for an untitled/path-less document (ReloadDocument would load `""`); `-32016 UnsavedChanges` when `IsModified` and not `force` (Reload resets the undo history and would discard the in-editor changes). `discardedChanges` is the pre-call dirty state. `path` is captured from `GetPath` before the call. `reloaded` re-resolves the chain after the call (ReloadDocument replaces the `CModelDoc`). No prompt, no modal; load errors are only logged by the editor. |
| `modeldoc.save` | ready [V runtime 2026-10-12] | implemented | `{}` | `{saved:bool, skippedClean:bool, path}` | `session->SaveAndNotify(session, path, path, 0, 0)` (session slot `MDVS_SESSION_SAVE`), then `SetModified(session, 0)` (slot `MDVS_SESSION_SETMODIFIED`) only when it returned nonzero - exactly the FileSave handler's recipe | `-32003`; `-32015 UntitledDocument` when `isNewFile` (`MDOFF_SESSION_ISNEW`) **or** `GetPath` is empty (Save As would open a modal). A clean document skips the API call entirely (`skippedClean:true`, nothing is written). `path` is copied into a bridge buffer **before** the save (the path string may be reallocated). A read-only doc (opened outside the addon) reaching the save returns its own rc, reported as `saved:false` - the tool refuses those paths itself. |
| `modeldoc.compile` | ready [V runtime 2026-10-12] | implemented | `{}` | `{status:null, failed:null, savedFirst:bool}` | non-virtual `CModelDocEditorSession::Compile` bound from `MDFN_SESSION_COMPILE`; `Compile(session, 0)` (mode unused - callers pass a garbage edx; 0) | `-32003`; `-32017 PrerequisiteFailed {reason:"no_asset"}` when `session+MDOFF_SESSION_ASSET` is `0` (a document opened from a content path has an asset; Compile would silently no-op on `null` so the bridge refuses first). **`Compile` returns nothing** (`void`) and its status lands in a sink object the bridge never reads: `status` and `failed` are always `null`; the tool verifies the `.vmdl_c` mtime [V runtime]. Synchronous, saves first when modified (`savedFirst` is the pre-call dirty state), no modal. |
| `modeldoc.nodeTypes` | ready [V runtime 2026-10-12] | implemented | `{}` | `{count:int, types:[{typeId:int, className:string}]}` | module globals `MDG_NODETYPE_COUNT` (int) and `MDG_NODETYPE_ARRAY` (`desc **`), each descriptor `{u32 typeId; ...; const char *className at MDOFF_NODETYPE_NAME}` | `-32017 PrerequisiteFailed` when the count is out of bounds (0 or > 4096, refuses to even loop), the descriptor array is unreadable, a descriptor is unreadable, or a `className` is unreadable - fail closed, never emit a partial list. The array always lives in the modeldoc image, so no session/document is required: it is the authoritative `_class` list. `className` strings are bounded-copied (128 B) before the JSON writer. Reply buffer 256 KB. |

## §8 error codes — new

None (m2.11 reuses `-32003 NoActiveDocument`, `-32015 UntitledDocument`, `-32016
UnsavedChanges`, `-32017 PrerequisiteFailed` and `-32602 InvalidParams`).

## §5/§6 notes

- Method count: 65 registered / 47 implemented (m2.10: 59/41; all six new methods
  implemented).
- Modules: `PIN_MODULE_COUNT` is now 11; `modeldoc_editor.dll` appears in
  `control.capabilities.modules` / `control.ping.modules` / `control.version.hostModules`
  with the states `not_loaded` (the usual state until a ModelDoc window opens), `verifying`,
  `verified`, `mismatch`. When a ModelDoc window is open the capabilities list gains the
  `modeldoc.*` methods (revision bumps on the verify event).
- `modeldoc_editor.dll` is loaded when a ModelDoc window opens and fully verified before any
  `modeldoc.*` call answers anything but `-32017 PrerequisiteFailed {module:
  "modeldoc_editor.dll", ...}`.
- Every new method copies game-engine strings out into bounded bridge-owned buffers
  (`GetPath` 1024 B, class names 128 B, open path 520 B; page-checked copies) before any
  use; `modeldoc.save` copies the path before `SaveAndNotify` (which may reallocate `session`
  strings) and refuses a `GetPath` that does not fit (a cut path would repoint the session).
- The `modeldoc.open` `active` object and `modeldoc.active` share one builder, so a session
  that died between the two calls is reported consistently (`null`/reason).
- Live results (2026-10-12, build 2000930, `bridge/test_modeldoc.py` on the single ModelDoc
  instance): all six methods answer. `CreateInterface` returns 0 with the expected identity;
  paths come back as `Z:\app\content\csgo_addons\mcp_test\...`; `nodeTypes` = **413**
  classes (snapshot `hammer-mcp/research/modeldoc-nodetypes.json`); reload/save clean;
  `compile` advances the `.vmdl_c` mtime.
- **`session+MDOFF_SESSION_COMPILE_STATUS` is not a string** [V runtime]: reading it as
  `char *` produced binary garbage. It is a status-sink object (its slot 0 takes a
  CBufferString). The bridge never reads it: `compileStatus`, `status` and `failed` are
  always `null`. The **`.vmdl_c` mtime is the success signal** (and the resourcecompiler route's
  `OK: n compiled, m failed` line).
- **New files have no asset object** [V runtime]: a `.vmdl` created after the tools started
  opens with `hasAsset:false`, and neither `modeldoc.reload` nor a resourcecompiler build
  binds one. `modeldoc.compile` then answers `-32017 {reason:"no_asset"}`. Tools must compile
  such files through the resourcecompiler route.
- Original live-probe list (now checked): `CreateInterface("ToolSystem2_ModelDoc")`
  really returns `0` on success with the identity `base+MDG_APP_OBJ`; `GetPath` produces
  `Z:\app\...`-style Wine paths (PathMap is case-insensitive); `IsModified` stays 0 right
  after open/reload; the compiled `glass_probe.vmdl_c` mtime advances on a successful
  `Compile`; `nodeTypes` count lands in the dozens (not 0, not >4096) and contains
  `RootNode`, `RenderMeshFile`, `BreakPieceList`; a second `modeldoc.active` after
  `modeldoc.open` of a copy shows the copy. Everything runs on the main-thread drain,
  never the IPC thread.
