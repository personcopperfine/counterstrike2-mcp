# ModelDoc RE notes (first pass, build 2000930)

Facts the ModelDoc first pass is built on. `[V]` = read in the binary (IDB of
`modeldoc_editor.dll`, sha256 `1744c2c553a7ede9`, same as the installed DLL) or seen live; `[I]` =
inferred. Every address the bridge uses is an entry in `sigs/signatures/modeldoc_editor.json`
(macro names below); nothing here is to be hand-copied into code.

## Scope of the first pass

Edit `.vmdl` sources (models, props, player models all share the format) through the **file
route**, then make the running ModelDoc pick the change up (**reload**), save in-editor edits, and
**compile**. Live per-node edits with in-editor undo (`CModelDoc::CreateNode`, attribute writes)
are the second pass: their undo record/write order is still unverified (idabins
`docs/flows/modeldoc-edit.md` MCP §5a).

## Live facts (2026-10-12, `TOOL=modeldoc cargo bridge runtime up`)

- `vtmsend open_asset <path relative to the addon>` (`cargo bridge runtime open models/<x>.vmdl`) loads
  `modeldoc_editor.dll` (+ `modeldoc_utils.dll`, `modtools.dll`, `toolscenenodes.dll`) on demand and
  opens the document in its own top-level window titled `ModelDoc :: <name>`. [V]
  The content-root form `csgo_addons/mcp_test/models/...` raises the asset browser's modal
  **"Unrecognized Asset"**, which blocks the tools main thread (bridge `mainThreadAgeMs` keeps growing,
  `modeldoc_editor.dll` never loads); the addon-relative `models/...` opens. Reproduced live
  2026-10-13 with ModelDoc's unclean-exit flags both set and reset, so the flags are not the cause.
  [V runtime]
- A file outside the active addon (e.g. core `content/csgo/workshop/content_examples/...`) opens
  as **`[ READ ONLY ]`** even though the file is writable. Editable models live under
  `content/csgo_addons/<addon>/`; the runtime starts with `-addon mcp_test`. [V]
- Opening a second file opens a second session/window; both stay open. [V]
- `resourcecompiler.exe -nop4 -game /app/game/csgo -i /app/content/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl`
  run through `podman exec <c> bash -c 'cd /app && ./entrypoint.sh resourcecompiler.exe ...'`
  next to the running tools: 6 s, writes `game/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl_c`
  (+ generated child `glass_probe/piece.vmdl`). Output ends with
  `OK: <n> compiled, <m> failed, <k> skipped, 0m:03s`; dependency failures print `[FAIL]` lines
  and do **not** count in `failed` (missing `.tga` sources of a core material did exactly that). [V]
- `-game /app/game/csgo_addons/mcp_test` is wrong: `FATAL ERROR: ... unable to load gameinfo.gi`. [V]
- The on-disk format: first line
  `<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:modeldoc36:version{...} -->`
  (older files say `modeldoc36`; the editor saves `modeldoc41`). Body: `rootNode = { _class = "RootNode"
  children = [ { _class = "...", name = "...", children = [...] , <attrs> }, ... ] }`. Tabs for
  indentation, `key = value`, arrays `[ a, b, ]` with trailing commas, `[ 0.0, 1.0, 2.0 ]` inline for
  vectors. [V]

## Object chain (all `[V]` static; macros in `modeldoc_editor.json`)

```
CreateInterface("ToolSystem2_ModelDoc")  (modeldoc_editor.dll export; ret 0 = found)
  == &g_CModelDocEditorApp (MDG_APP_OBJ), vtable MDV_APP_VT
app + MDOFF_APP_SESSION (0x50)        -> active CModelDocEditorSession (vtable MDV_SESSION_VT), may be 0
session + MDOFF_SESSION_DOC (0x118)   -> CModelDoc (vtable MDV_DOC_VT), replaced on every load/reload
session + MDOFF_SESSION_ASSET (0xBF8) -> asset object (AssetSystem001); 0 => Compile is a no-op
session + MDOFF_SESSION_COMPILE_STATUS (0xF98) -> status-sink OBJECT (vtable; slot 0 takes a CBufferString),
                                         NOT a char *: reading it as a string returned garbage live. Never read.
session + MDOFF_SESSION_ISNEW (0x19)  -> u8, untitled document (FileSave would open Save As)
```

The asset object is (re)bound by session slot 25 from SetPath (slot 10): the path is looked up in
`CONTENT`, then `GAME`. A document opened from a content path therefore has one. [V]

## Calls (Windows x64, `this` first)

| What | Entry | Signature / notes |
|---|---|---|
| open | `MDVS_APP_OPENFILE` app slot 18 | `void *OpenFile(app, const char *path, u32 flags)`; flags 1 (what the UI passes). Re-entrancy guard `app+0x1A3` logs and returns 0. Relative paths resolve in `CONTENT` then `GAME`, **rooted at the addon**: `models/x.vmdl` opens, `csgo_addons/<addon>/models/x.vmdl` returns 0 [V runtime]; an already open file just re-activates its session. **Modals**: file missing ("Cannot open"), path outside the content dir ("Open anyway?", skipped only in test mode), load errors ("File opened with errors" / "Error opening file"). A modal blocks the main thread: pre-check on the host side. |
| reload | `MDVS_SESSION_RELOAD` session slot 7 | `void ReloadDocument(session)`: ResetDocument (new CModelDoc, **undo history gone**) + LoadFromFile(GetPath) + refresh. No prompt, no modal; load errors only logged. |
| dirty | `MDVS_SESSION_ISDIRTY` slot 11 | `u8 IsModified(session)` = `doc+0x3BC` (0 if no doc). |
| set dirty | `MDVS_SESSION_SETMODIFIED` slot 12 | `void SetModified(session, u8)`; FileSave calls it with 0 after a successful save. |
| path | `MDVS_SESSION_GETPATH` slot 13 | `const char *GetPath(session)`: absolute Windows path (`session+0x58`), `""` when unset. |
| save | `MDVS_SESSION_SAVE` slot 18 | `u8 SaveAndNotify(session, const char *oldPath, const char *newPath, u8 a4, u8 a5)`; the FileSave handler passes `(path, path, 0, 0)` then calls slot 12 with 0. Writes KV3 text `modeldoc41`. Refuse when `isNewFile` (FileSave would open Save As). A VCS checkout prompt exists only for p4-managed dependent files (`-nop4` runtime: not expected). |
| compile | `MDFN_SESSION_COMPILE` (non-virtual) | `void Compile(session, int mode)`; mode is unused apart from an event payload (callers pass a garbage edx) - pass 0. Returns nothing useful: saves first when modified (SaveImpl), then calls asset slot 17 with `compiler_hints`, **synchronous**, no modal; the outcome goes to the status sink at `+0xF98` (not readable as a string): the `.vmdl_c` mtime is the success signal. **Never** use session slot 21 `SaveAndCompile`: it opens a "Compile?" modal. |
| node types | `MDG_NODETYPE_COUNT` (int), `MDG_NODETYPE_ARRAY` (`desc **`) | descriptor `{u32 typeId; ...; const char *className at MDOFF_NODETYPE_NAME (+8)}`; iterated by the shipped CreateOneOfEveryNode. Gives the authoritative list of `_class` names. |

Everything above runs on the tools main thread (bridge main-thread drain), never on the IPC thread.

- **A file created after the tools started has no asset object** (`+0xBF8` = 0) [V runtime]:
  neither ReloadDocument nor a resourcecompiler build of it binds one, so editor-side Compile is a
  no-op (the bridge answers `no_asset`). Compile new files with resourcecompiler.

## Viewport render (vision, bridge m2.14 `modeldoc.render`)

Static facts, all `[V]` (build 2000930; macros in `modeldoc_editor.json`):

```
session + MDOFF_SESSION_MAINWND (0x110)          -> CQModelDocEditorWnd (MDV_EDITORWND_VT)
    set in session init sub_180224360 (mov [rsi+110h], rdi; then the window init sub_180127920);
    the session is created by sub_180201340. The window's +0x130 points back at the session.
mainwnd + MDOFF_MAINWND_SCENEWIDGET (0x1E0)      -> CQModelDocEditorSceneWidget (MDV_SCENEWIDGET_VT),
    the central widget (sub_180127F40)
scenewidget + MDOFF_SCENEWIDGET_TOOLWIDGET (0x90) -> CQModelDocToolSceneWidget (MDV_TOOLWIDGET_VT)
    (sub_180102D10); base CQModelDocToolSceneWidgetBase -> CQToolSceneWidget (ctor sub_180B45BA0)
toolwidget + MDOFF_TOOLWIDGET_SCENE (0x50)       -> CToolScene (MDV_TOOLSCENE_VT)
toolwidget + MDOFF_TOOLWIDGET_CAMERA (0x58)      -> CToolSceneCamera (MDV_SCENECAMERA_VT)
MDFN_TOOLSCENE_RENDER = CToolScene::Render (0x180B53770)
```

The tools library (CQToolSceneWidget, CToolScene, CToolSceneCamera) is linked statically into
modeldoc_editor.dll, as it is into pet. The function signature and the widget paint
(`sub_180B491F0`) are byte-identical to pet's, so the call and pixel facts in `pet-re.md` "Preview
render (vision)" carry over. In short, `Render(scene, camera, 0, 0, &vp, pixels, 0)` builds an
offscreen window from the camera and reads RGBA8888 back.

Each session has its own main window, so a render needs an active session.

Floating "Preview Window" docks (`CQModelDocEditorPreviewWindow`, ctor `sub_1800FF610`) each hold a
second widget (`CQModelDocEditorPreviewToolSceneWidget` at +0x58). They are not rendered. The
render always uses the central viewport with its current camera.

## Live results (2026-10-12, bridge m2.11)

`test_modeldoc.py` passes on the single ModelDoc instance: active/open/reload/save/compile/
nodeTypes all answer on the main thread; `GetPath` gives `Z:\app\content\csgo_addons\...`;
**413** node classes (`modeldoc-nodetypes.json`); editor Compile advances the `.vmdl_c` mtime.

## What the file route must guard against

- A `.vmdl` that fails to load pops a modal in OpenFile (not in Reload). Validate KV3 syntax and
  `_class` names (registry list above) before telling the editor to open a file we wrote.
- Reload discards unsaved in-editor changes and the undo history: refuse while `IsModified`
  unless `force`.
- Never write outside `content/csgo_addons/<addon>/` (core content is read-only to the editor and
  must not be modified); keep a backup before overwriting.

## References for node classes and keys

- ValveResourceFormat (MIT) `ValveResourceFormat/IO/Extract/ModelExtract*.cs`: the decompiler
  writes `.vmdl` (RenderMeshList/RenderMeshFile, PhysicsShapeList/PhysicsHullFile, AnimationList,
  BodyGroupList, MaterialGroupList, AttachmentList, HitboxSetList, BoneMarkupList, LODGroupList,
  GameDataList, ...) - use it as a source of facts (class names, keys, value shapes), not code.
- `content/csgo/workshop/content_examples/breakable_glass/breakable_glass_01.vmdl` (procedural:
  no external mesh file; compiles standalone).
- idabins `docs/flows/modeldoc-edit.md` (full static flow incl. node edits and undo).
