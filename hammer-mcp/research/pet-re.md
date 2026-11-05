# Particle editor (pet) RE notes (first pass, build 2000930)

Facts the particle (`.vpcf`) first pass is built on. `[V]` = read in the binary (IDB of
`tools/pet.dll`, sha256 `a1fefc1443d0079c`, the same file as the installed DLL; `toolframework2.dll`
IDB for the load path) or seen live; `[I]` = inferred, still to confirm live. Every address the
bridge uses is an entry in `sigs/signatures/pet.json` (macros below) or `toolframework2.json`
(`TFVS_LOAD_TOOL_SYSTEM`, `TFVS_GET_TOOL_INDEX_BY_NAME`). Never copy a number from here into code.
Older, wider static notes (control-point configs, undo internals): idabins
`docs/flows/particles-edit.md`, `docs/modules/pet.md` (reference only, same binary).

## Scope of the first pass

The building blocks, no vision:
1. **File route**: read/author/edit `.vpcf` (KV3 text) on disk, validate it against the particle
   schema, compile with `resourcecompiler`.
2. **Schema**: the authoritative list of operator classes per function array and their fields, read
   live from the game (bridge) and kept as a snapshot in the repo for offline validation.
3. **Live lifecycle** in the running tools session: load pet, open, reload after a file edit, save,
   read the open document's operator tree.

Live operator edits with in-editor undo (`CPetDoc::AddFunctionNode`, `DeleteSelected`, `Undo`) are
the second pass.

## Live facts (2026-10-13, `bridge/runtime.sh up`, csgo `-tools -addon mcp_test`)

- `vtmsend open_asset particles/x.vpcf` does **not** open pet in a csgo session: the console logs
  `WARNING: No tool (internal or external) found for asset 'particles/mcp_probe/probe.vpcf'` (the
  manifest's `m_ExcludeFromMods = ["csgo"]` hides pet from asset routing). [V]
- `vtmsend tool_request` with `tool_name = "pet"` loads `tools/pet.dll` within a second (mapped in
  the cs2.exe process); the bridge main thread stays responsive (`control.ping` `mainThreadAgeMs` 1)
  and the session runs on. Only side effect in the console: a HelpSystem line about
  `toolhelp_pulse_editor_english.txt`. [V]
- The live schema system has type scopes for **`particles.dll`** and **`pet.dll`**
  (`game.schemaProbe`). [V]
- `resourcecompiler.exe -nop4 -game /app/game/csgo -i /app/content/csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf`
  (run with `podman exec <c> bash -c 'cd /app && ./entrypoint.sh resourcecompiler.exe ...'`, next to
  the running tools) prints `OK: 1 compiled, 0 failed, 0 skipped` and writes
  `game/csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf_c`. [V]
- **resourcecompiler does not validate classes or fields**: a `.vpcf` with `_class =
  "C_OP_NoSuchThing"` and one with an unknown root key `m_bogusField` both compile `OK: 1 compiled`.
  Only a KV3 syntax error fails (`[FAIL]` line + `ERROR: 0 compiled, 1 failed`). Schema validation is
  therefore the MCP server's job. [V]
- A hand-written minimal file (header below, `_class`, `m_nMaxParticles`, one emitter, one
  initializer, two operators, one renderer, no other fields) compiles. [V]

## File format [V]

```
<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:vpcf66:version{dd571be4-586e-4f41-8b2d-5bbc969e1c53} -->
{
	_class = "CParticleSystemDefinition"
	m_nMaxParticles = 32
	m_Emitters = [ { _class = "C_OP_ContinuousEmitter" }, ]
	...
}
```

- Both GUIDs read from the KV3ID_t structs pet passes to LoadKV3/SaveKV3 (`vpcf66` at
  `pet!0x180622B98`, `text` at `pet!0x180622BB8`). pet saves `encoding text, format vpcf66`.
- pet's loader requires the root `_class` to be `CParticleSystemDefinition` ("File does not contain a
  root object of type '%s'").
- Function arrays, in pet's type index order (the index is what `IParticleSystemMgr` takes and what
  the root node stores):

  | type | key | node class in pet |
  |---:|---|---|
  | 0 | `m_PreEmissionOperators` | CPetFunctionNode |
  | 1 | `m_Emitters` | CPetFunctionNode |
  | 2 | `m_Initializers` | CPetFunctionNode |
  | 3 | `m_Operators` | CPetFunctionNode |
  | 4 | `m_ForceGenerators` | CPetFunctionNode |
  | 5 | `m_Constraints` | CPetFunctionNode |
  | 6 | `m_Renderers` | CPetFunctionNode |
  | 7 | `m_Children` | CPetChildNode (`ParticleChildrenInfo_t`: `m_ChildRef`, `m_flDelay`, ...) |
  | 8 | `m_NamedValueLocals` | CPetVariableNode (`ParticleNamedValueSource_t`) |

  plus `m_controlPointConfigurations` (own 144-byte records). Every other root key is a field of
  `CParticleSystemDefinition`. Each function entry is an object whose `_class` is a class registered
  for that type (below); its other keys are fields of that class or its bases.
- The function node's `m_nBehaviorVersion` is read after an add (CPetFunctionNode ctor).

## Object chain (all `[V]` static; macros in `pet.json`)

```
toolframework2: GetToolIndexByName(fw, u8 *out, "pet")  (TFVS_GET_TOOL_INDEX_BY_NAME, slot 22; 0xFF = unknown)
                LoadToolSystem(fw, u8 idx) -> IToolSystem2 *  (TFVS_LOAD_TOOL_SYSTEM, slot 24)
                  loads the DLL once per index (cached; never retried after a failure), calls tool slot 11;
                  this is what `tool_request pet` does minus tool slot 16 (a nop in pet).
                  Failure paths open a QMessageBox ("Failed to load/initialize tool system") -> modal.
CreateInterface("ToolSystem2_001") (pet.dll export, the only registration)
  == &g_CPetApp (PETG_APP_OBJ), vtable PETV_APP_VT
app + PETOFF_APP_SESSION (0x50)        -> active CPetSession (PETV_SESSION_VT), may be 0
session + PETOFF_SESSION_DOC (0x108)   -> CPetDoc (PETV_DOC_VT)
session + PETOFF_SESSION_ISNEW (0x19)  -> u8, untitled (FileSave would open Save As)
doc + PETOFF_DOC_ROOT (0x80)           -> CPetRootNode (PETV_ROOTNODE_VT); set to 0 at the start of every
                                          load. pet does NOT fail cleanly into this state on malformed
                                          input: ReloadDocument wedges the main thread instead (live,
                                          2026-10-13, see the reload row below).
PETFN_ROOT_GETCHILDCOUNT(root, type) / PETFN_ROOT_GETCHILD(root, type, i) -> CBasePetNode * (no bounds check)
node vtable + PETOFF_NODE_CLASSNAME (+0x90) -> const char *GetClassName(node): the function's `_class`;
                                          "ParticleChildrenInfo_t" for children; "ParticleNamedValueSource_t" for variables
PETG_PARTICLE_MGR -> IParticleSystemMgr * (particles.dll; pet's ParticleSystemMgr003 cache, 0 before pet Connect)
  mgr vtable + PETOFF_PMGR_FUNC_COUNT (+0xC0): int GetFunctionCount(mgr, int type 0..6)
  mgr vtable + PETOFF_PMGR_FUNC_GET   (+0xC8): def *GetFunction(mgr, int type, int index)
  def vtable + PETOFF_FACTORY_NAME    (+0x00): const char *GetName(def)  -- friendly name ("Render fog sprites")
  def vtable + PETOFF_FACTORY_CLASSINFO (+0x10): void **GetSchemaBinding(def, void *scratch8);
                                          *ret = SchemaClassInfoData_t * (m_pszName = the `_class`)
```

Checked in the particles.dll IDB (same build): `CParticleSystemMgr` slots 24/25 are
`GetFunctionDefinitionCount(type)` / `GetFunctionDefinition(type, i)` (`mgr+0x38+24*type` count,
`+0x40` data; **no bounds check**: the bridge must keep `0 <= i < count`, `0 <= type <= 6`). A
definition is a static object (`RegisterParticle*` -> `CParticleSystemMgr::AddFunctionDefinition`)
whose vtable has exactly three slots and **no destructor**: slot 0 `GetName` = `[this+0x20]`
(friendly name), slot 1 `GetGroup` = `int [this+0x28]` (-2 generic, -1 singleton, else a category),
slot 2 `GetSchemaBinding(this, void **out)` = `*out = *staticBinding; return out` (8-byte out). [V]

The operator picker (`pet!0x180052600`) builds its list exactly this way and hides a class whose
schema has the static metadata `MObsoleteParticleFunction`; it also checks `MGPUParticleFunction`,
`MParticleMinVersion`/`MParticleMaxVersion` against the system's behavior version, and marks
"Only one allowed" for definitions whose slot 1 returns -1 (singleton). The schema walk itself
(fields, bases, metadata names) needs no pet code: `SchemaClassInfoData_t` layouts come from the
generated `bridge/s2sdk_gen.h` (`S2_OFF_SchemaClassInfoData_t_*`, `SchemaClassFieldData_t`,
`SchemaBaseClassInfoData_t`, `SchemaMetadataEntryData_t`), and class lookups by name go through the
`particles.dll` type scope with the resolver the bridge already uses for `client.dll`.

## Calls (Windows x64, `this` first)

| What | Entry | Signature / notes |
|---|---|---|
| load | `TFVS_GET_TOOL_INDEX_BY_NAME` + `TFVS_LOAD_TOOL_SYSTEM` | see above. Only call when pet is not loaded yet (the bridge's module state for pet.dll), on the main thread. |
| open | `PETVS_APP_OPENFILE` app slot 18 | `void *OpenFile(app, const char *path, u32 flags)`; re-entrancy guard `app+0x1A3` logs and returns 0. Relative paths resolve in `CONTENT` then `GAME` [V]; whether they are addon-rooted like ModelDoc's (`particles/x.vpcf`) is [I] (same base code). An already-open path re-activates its session (`sub_18041F5C0` lookup) [V static]. **Modals**: file missing ("Cannot open ... does not exist"), outside the content dir ("Open anyway?", skipped only in test mode), load errors ("Failed to load" / "File opened with errors"). Pre-check on the host side. flags bit 0 = add to recent files (UI passes 1). |
| reload | `PETVS_SESSION_RELOAD` slot 7 | `void ReloadDocument(session)`: `LoadFromPath(GetPath())` + `SetModified(0)` + `RecompileResource`. No prompt (the prompt lives in the app's FileReload). A **malformed** file does not fail calmly: live 2026-10-13 the main thread recursed until the Wine stack overflowed and the whole process wedged (two inputs: `{{{ not kv3\n`, and a valid kv3 header followed by a truncated body). A balanced-but-invalid KV3 body wedges it the same way, and a wrong root `_class` makes the process exit (`c0000409`); unknown classes, wrong field types and a non-array function list are tolerated (workflow 05 has the table). The bridge checks KV3 grammar + root `_class` (`bridge/kv3_check.h`) and refuses with `-32602 UnloadableFile` **before** calling (and before `OpenFile`), so the document keeps its root and the main thread keeps draining. Undo history: not reset by this path [?]. |
| dirty | `PETVS_SESSION_ISDIRTY` slot 11 | `u8 IsModified(session)` = `session+0x60`. |
| set dirty | `PETVS_SESSION_SETMODIFIED` slot 12 | `void SetModified(session, u8)` (also refreshes the window title). |
| path | `PETVS_SESSION_GETPATH` slot 13 | `const char *GetPath(session)`; `""` when unset. Absolute Windows path [I]. |
| save | `PETVS_SESSION_SAVE` slot 18 | `u8 Save(session, const char *oldPath, const char *newPath, u8 a4, u8 a5)`; FileSave passes `(path, path, 0, 0)` then `SetModified(0)`. Writes KV3 text `vpcf66` to `newPath`. Refuse when `isNew`. No modal on failure (logs `WARNING: Failed to save file '%s': %s`). |
| tree | `PETFN_ROOT_GETCHILDCOUNT` / `PETFN_ROOT_GETCHILD` + node slot `PETOFF_NODE_CLASSNAME` | per type 0..8: count, then the class of each node. Read-only. |
| registry | `PETG_PARTICLE_MGR` + the four `PETOFF_PMGR_*`/`PETOFF_FACTORY_*` offsets | per type 0..6: every registered function definition: friendly name, schema class (name, metadata). |

Everything above runs on the tools main thread (bridge main-thread drain), never on the IPC thread.

## Live preview and recompile [V static]

`CPetSession::RecompileResource` serializes the doc into a buffer and hands it to the particle asset
bound at `session+0x230` (asset type `0x66637076`); pet's `AppRunFrame` does this for any session
whose `+0x2B9` byte is set by a doc-modified event. ReloadDocument calls it too. So after a file
edit, `pet.reload` is enough for pet's own preview; the `.vpcf_c` on disk comes from
`resourcecompiler`, and also from pet: [V live 2026-10-13] opening or reloading a file in pet makes the asset system recompile it (`On-demand recompile ... reason: No Game File` / `file CRC changed` in the console) and a fresh `.vpcf_c` appears under `game/csgo_addons/<addon>/` for files that were never passed to `particle_compile`.

## Open questions (answered ones carry their live verification; the rest stay open)

- [x] [V live 2026-10-13] `OpenFile` with the addon-relative form `particles/x.vpcf` opens the file in
      the running pet (`test_pet.py` steps 4/5/8: `pet.open` -> `opened:true`, `active.hasRoot:true`,
      `pet.active` agrees; five bad absolute/escaped paths refused before `OpenFile`). `GetPath` returns the absolute
      Windows path (`Z:\app\content\csgo_addons\mcp_test\particles\...`), the same file the bridge's
      `pet.open` gate resolves. ~~Still open: the absolute-path form as `OpenFile` input.~~
      Closed unprobed (second pass, 2026-10-13): not needed — no tool can reach it, since
      `particle_open`/the bridge's `pet.open` gates refuse absolute paths before `OpenFile` is
      ever called; dropped from the live plan (step 22).
- [x] [V live 2026-10-13] pet loaded through `LoadToolSystem` works for the first pass: `pet.load`
      picked index 5, pet.dll reached `verified` a few seconds later, and `pet.open` /
      `pet.active` / `pet.reload` / `pet.save` / `pet.schema` / `pet.functionTypes` all answer.
- [x] [V live 2026-10-13] `ReloadDocument` with an invalid file: the old "empty doc, no modal" claim
      is **wrong**. The main thread recurses until the Wine stack overflows and the process wedges
      (seen twice, two input shapes); podman cannot stop it. A wrong root `_class` exits the process (`c0000409`). Guard: `bridge/kv3_check.h` (grammar +
      root class) + `-32602 UnloadableFile {reason, path}` before `ReloadDocument` and `OpenFile`.
- [x] [V live 2026-10-13] Save from the bridge works: a clean document saves nothing (`skippedClean`);
      `pet.save {force:true}` re-serializes it (vpcf66 text, tabs, CRLF, no final newline — the
      cs2-kv3 writer's Valve layout) and `SetModified(0)` clears dirty. pet may add the preview
      `m_controlPointConfigurations` entry on save. The file writer matches that layout (`Particle::write`).
- [x] [V live 2026-10-13] pet recompiles on open/reload and the asset system writes the `.vpcf_c`
      (see "Live preview and recompile").
- [x] [V live 2026-10-13] Friendly-name slot 0 returns readable names for every registry type:
      `pet.functionTypes` read 376 definitions (PreEmission 49, Emitters 4, Initializers 98,
      Operators 161, ForceGenerators 16, Constraints 13, Renderers 35) with non-empty names and
      class bindings; all 23 `sigs/signatures/pet.json` entries verify.

## Second pass: live edits with undo, the document text, preview render [V live 2026-10-13, bridge m2.13]

All entries below are in `sigs/signatures/pet.json`. After the live runs: 55 of the 59 entries are
`verified` (everything the bridge calls, incl. the pet object vtables and the undo slots); still
`proposed` are `PETFN_DOC_ADDFUNCTION` / `PETFN_DOC_REMOVENODE` (the native single-node path, not
used by the bridge) and `PETOFF_DOC_SELECTION_COUNT` / `PETOFF_DOC_SELECTION_DATA` (the offsets
are set by the bridge's selection reset, but no run ever had a UI selection in play, so they were
never exercised).

### Undo model
pet's undo is **whole-document snapshots**. `CPetDoc::BeginUndoStep(doc, name, notify)`
(`PETFN_DOC_BEGINUNDOSTEP`) opens a named step on the doc's history (`doc + PETOFF_DOC_UNDOHIST`)
and records the doc itself; the record is a full clone (CPetDoc undoable slot 0 =
`SaveToKV3` of the live root into a new doc, `LoadFromKV3` into its root). `Undo`/`Redo`
(`PETFN_DOC_UNDO`/`REDO`) call history slot 2 (apply-and-pop) on the history / its partner
(`+ PETOFF_UNDOHIST_PARTNER`); restoring is `LoadFromKV3` of the snapshot into the **live root**
(the root pointer is kept, the child nodes are rebuilt). `canUndo`/`canRedo` = history slot 3
`HasEntries` (`PETVS_UNDOHIST_HASENTRIES`). max 50 steps. Live confirmation through the bridge
(2026-10-13): `pet.apply` is one undo step with the label shown as given; Undo -> `pet.text`
byte-identical to before; Redo restores; an empty history answers `done:false
nothing_to_undo` (a result, not an error); Undo/Redo leave the document dirty; `canUndo`
survives both `pet.save` and `pet.reload {force:true}` (the history is not reset by
`ReloadDocument`).

Every pet edit entry point has the same shape: `BeginUndoStep(name)` -> change the tree ->
`RefreshCaches(root, doc + PETOFF_DOC_CACHES)` -> emit `PetEvent_FunctionListChanged_t`. The
property editor's field write ("Set %s") is `BeginUndoStep(doc, "Set <field>", 1)` followed by the
field write into the node's KV3. `notify = 1` emits `PetEvent_DocumentModified_t`, whose session
handler (`PETFN_SESSION_ONDOCMODIFIED`) sets `session+0x2B9` (AppRunFrame recompiles the preview
next frame) and `SetModified(session, 1)`. Undo/Redo do **not** emit DocumentModified, and emit
FunctionListChanged only when an array count changed.

So a generic live edit is: parse the new document text into a KV3 context
(`PETFN_KV3CTX_CTOR(ctx, 0)`, tier0 export `LoadKV3(KeyValues3 *kv, CUtlString *err,
const char *text, const KV3ID_t &PETG_KV3ID_VPCF66, const char *name, u32 flags)`,
`PETFN_KV3CTX_ROOT(ctx)`) -> **only after the parse succeeded**, `BeginUndoStep(doc, label, 1)`
-> the LoadFromFile strip + filter (below) -> `PETFN_ROOT_LOADFROMKV3(root, kv)` (exactly what
Undo does) -> `RefreshCaches` -> the selection reset -> `PETFN_NOTIFY_FUNCLIST` ->
`PETFN_KV3CTX_DTOR(ctx)`. One undo step in pet's own history, the editor's Undo restores the
previous tree. pet keeps the context as a 4608-byte stack block (`sub rsp, 12A0h` frames).
This is what the bridge's `pet.apply` implements (m2.13); parse-before-step is deliberate, so a
`LoadKV3` failure cannot record an empty undo step.

### LoadFromFile's strip + filter [V static (IDA, review pass), behaviour live 2026-10-13]
`CPetDoc::LoadFromFile` does **more** than `LoadFromKV3` around it:
- **before** it: `pet!sub_1804E7B20` strips the parsed KV3 to the schema, using the particle
  type scope (`pet!sub_180118110`); without this, unknown fields/classes would ride into the tree.
- **after** it: `pet!sub_18011EE30` then `pet!sub_180120900` — the root behavior-version check
  and the function-node filter that drops unregistered / out-of-version `_class` entries (this
  is the silent drop the live runs measure: `applied:true`, array count unchanged).

The first-pass `pet.apply` skipped these, so the review pass (8a2acc5) added the strip + filter +
behavior-version pins and the bridge now runs the same sequence. pet's own undo apply
(`pet!sub_18011C670`, the history slot that restores a snapshot) runs **none** of the four: its
snapshots are recorded from pet's own serialization (the live tree it would strip/filter has
already been through it), so the Undo restore path is safe to skip them.

### Selection layout [V static (IDA); the reset is in the m2.13 apply sequence]
The doc's selection is `{type, index}` pairs: count at `doc+0x68`
(`PETOFF_DOC_SELECTION_COUNT`), the entries at `+0x70` (`PETOFF_DOC_SELECTION_DATA`). pet's "no
selection" is the entry `{9,-1}`. After a `pet.apply` load the bridge resets every entry to
`{9,-1}`: `GetChild` has no bounds check, so a stale index past a shrunken array would be a
dangling-by-index (not a dangling node pointer). Unverified live: no run had a UI node selected
during an apply, so the reset itself was never exercised.

Native single-node edits (each opens its own undo step; **not taken** by the bridge's pass 2 —
the whole-document `pet.apply` covers add/remove with undo via full snapshots, and
`AddFunctionNode` records a step even on failure, one reason it is excluded):
- `AddFunctionNode(doc, displayName|NULL, className, type 0..6, NULL)` (`PETFN_DOC_ADDFUNCTION`):
  returns the node, or 0 when `className` is not registered for `type` (the undo step is still
  recorded: pre-check against the registry). Selects the new node.
- `RemoveNode(doc, type, node)` (`PETFN_DOC_REMOVENODE`, "Remove '%s'"): node from
  `PETFN_ROOT_GETCHILD`.
- Variables: `AddVariableNode(doc, name, CPulseValueFullType *)` (`pet!0x180119720`, not pinned):
  the type struct is `{int PulseValueType_t; ptr; CGlobalSymbol}` and the callee destroys it.

### Document text without touching the file
`CPetDoc::SaveToFile(doc, path, CUtlString *err, std::function *cb)` (`PETFN_DOC_SAVETOFILE`)
serializes the live tree (same writer as Save) to any path without changing the session's path
or dirty flag; `cb` is read at `+0x38` and skipped when 0 (pass 64 zero bytes).

### Preview render (vision)
`CPetApp + PETOFF_APP_MAINWND` -> `CQPetMainWnd` (+ `PETOFF_MAINWND_PREVIEW`) -> `CQParticlePreview`
(+ `PETOFF_PREVIEW_SCENEWIDGET`) -> `CQToolSceneWidget` (the preview viewport): `+0x50` `CToolScene`
(scene world at `+0x20`), `+0x58` the viewport camera. The widget's paint (`pet!0x1803E34F0`) has a
software path that is a render-to-memory: `CToolScene::Render(scene, camera, wnd|0, swapchain|0,
&RenderViewport_t{flags 1, x 0, y 0, w, h, 0, 1}, pixels, imageFormat)` (`PETFN_TOOLSCENE_RENDER`);
with `wnd = 0` it builds a `COffscreenToolSceneWindow` from the camera, runs the scene system's
Begin/Finish/Wait bracket itself and reads the colour target back with the render device's
`ReadTexturePixels` into `pixels` (row pitch `w * bpp`). Image formats (pet table `0x18082F0C0`):
0 RGBA8888 (4 B), 2 RGB888 (3 B; what the widget uses). The pixels are display-ready (the widget
shows them as a QImage unchanged).
Live 2026-10-13 (`pet.render`, m2.13): the camera candidate (`CToolSceneCamera` vtable) is
confirmed (no `bad_camera` in any run); 320x240 / 480x320 render in ~5-15 ms,
`untouchedPixels == 0`, and the PNG shows the preview contents (grid, axes, pet's stats overlay,
the sprites) — the pixel facts and the result shape are now live too; the Begin/Finish/Wait
bracket internals stay a static reading.

### KV3 nesting depth [V live 2026-10-13]
tier0's text `LoadKV3` (the pet.apply parse) answered root fields nested 64/128/200/255/256
levels live, main thread fine after each; 257 is refused by the bridge's checker (`too_deep`)
before pet. The caps stay 256 (bridge `KV3_CHECK_DEPTH_CAP` = cs2-kv3 `MAX_DEPTH`). pet's file
loader uses the CUtlBuffer `LoadKV3` overload — not probed separately.

### m_NamedValueLocals [V live 2026-10-13]
Element class `ParticleNamedValueSource_t` { `m_Name` CUtlString, `m_IsPublic` bool (default true),
`m_ValueType` CPulseValueFullType (default `"PVAL_VOID"`, so a string such as `"PVAL_FLOAT"`),
`m_DefaultConfig` `ParticleNamedValueConfiguration_t` { `m_ConfigName`, `m_ConfigValue` (KeyValues3),
`m_BoundValuePath`, `m_iAttachType` (`"PATTACH_INVALID"`), `m_strEntityScope`,
`m_strAttachmentName` } } (GameTracking schema dump, particleslib).
Live answer (candidate entry applied through `pet.apply`, read back through `pet.text` and the
saved file — the same form in both): pet writes each entry `m_Name`, `m_IsPublic` (**only when
false** — true is the default and omitted), then `m_DefaultConfig { m_ConfigName, m_ConfigValue,
m_BoundValuePath, m_iAttachType, m_strEntityScope, m_strAttachmentName }`, then `m_ValueType`; no
`_class`. An entry without `m_DefaultConfig` gets a full default config with `m_ConfigValue =
null`. A non-string `m_ValueType` (`2`) becomes `"PVAL_VOID"` (invalid -> the default): the entry
is still accepted (type-8 count 1 in every probe), so to get a real type the value must be the
string `"PVAL_*"`. `add_variable`/`remove_variable` build exactly this observed form.
