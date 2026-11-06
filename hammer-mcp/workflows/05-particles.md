# 05 — Particle editor (pet) and the `.vpcf` route

## Goal
Author and edit particle systems (`.vpcf`, KV3 text) in the active addon: write, compile and
round-trip them with the running pet. The first pass was no-vision (correctness = the schema +
the compiled file, not the viewport); the second pass adds the preview render (`particle_render`,
below) as the one verified way to *see* the system.

## Status (first pass, 2026-10-13, build 2000930): done, verified live
13 `particle_*` tools (after the ModelDoc tools, `PET_TOOL_ORDER`), bridge **m2.12** `pet.*`
(7 methods; `sigs/signatures/pet.json`, 23 entries, all verified). The documented loop below ran
end to end live (`test_pet.py`). Live node edits with in-editor undo are the second pass
(below) — done too now, verified live.

| Tool | What | Live |
|---|---|---|
| `particle_read` | header, root fields, per-array entry counts from disk; `function:"m_Operators/2"` reads one entry | [V] |
| `particle_list` | flat function-entry list from disk, filterable by array/class, with counts | file layer (same parse as `particle_read`); not in the 2026-10-13 loop |
| `particle_validate` | structure + schema vs `pet-schema.json`: `ok`, `errors`, `warnings`, `schemaSource` | [V] |
| `particle_edit` | ops (`add_function`/`remove_function`/`move_function`/`set_field`/…), all-or-nothing, backup + atomic write; an unknown class is refused before anything is written | [V] |
| `particle_create` | minimal template (`m_nMaxParticles`, one emitter/initializer, two operators, one renderer, `m_nBehaviorVersion = 12`); refuses an existing file unless `overwrite` | [V] |
| `particle_compile` | `resourcecompiler.exe` in any tools container; `ok` = the `.vpcf_c` was written / up to date | [V] "OK: 1 compiled" |
| `particle_status` | never raises: bridge status, `petLoaded`, missing `pet.*` capabilities, the active document (+ mapped host path, `inAddon`, the nine arrays keyed by file key) | [V] |
| `particle_types` | `pet.functionTypes` grouped by array (376: PreEmission 49, Emitters 4, Initializers 98, Operators 161, ForceGenerators 16, Constraints 13, Renderers 35); `writeSnapshot` writes `pet-schema.json` | [V] |
| `particle_schema` | one class: live `pet.schema` when the bridge advertises it, else the local snapshot (`source` says which) | [V] snapshot route |
| `particle_load` | one-shot `pet.load`, then wait for the `pet.dll` module state to settle; `loaded:false` means restart the session | [V] LoadToolSystem index 5, module verified |
| `particle_open` | open a `.vpcf` of the active addon in the running pet; pre-validated on the host; refuses dirty/unknown-unless-force | [V] |
| `particle_reload` | reload the active document from disk after a file edit; pre-validated on the host (the crash rule); refuses dirty/unknown-unless-force | [V] the edit reached pet (its autosave had the new field) |
| `particle_save` | save to the document's own path (never Save As: refuses untitled/outside-addon); `force` re-serializes a clean document | [V] force, dirty false afterwards |

Typical loop (verified live): `particle_validate` → `particle_create` → `particle_edit` →
`particle_compile` → `particle_open` → `particle_status` → `particle_edit` → `particle_reload`
→ `particle_save` → `particle_read`. Writes land only in the active addon
(`ContentPaths::resolve_write_ext`); a project root reached through a symlink resolves too (the
`WriteOutsideAddon` refusal on symlinked roots is fixed in `vmdl/paths.rs`, shared with
ModelDoc).

Pieces: `hammer-mcp/src/vpcf/` (the KV3 layer: parse/write, the nine arrays, ops, schema,
template), `src/tools/particle_file.rs` (file route), `particle_live.rs` (live half),
`particle_live_edit.rs` (second-pass live-edit/undo tools), `particle_render.rs` (render tool),
`particles.rs` (order), bridge m2.13 `pet.*` (12 methods; `crates/cs2-bridge/PROTOCOL.md` §17), sigs
`sigs/signatures/pet.json` (59 entries: 55 verified, 4 proposed). RE: `research/pet-re.md`; older static
notes (`../idabins/docs/flows/particles-edit.md`, `docs/modules/pet.md`) only fill in what
pet-re.md does not carry.

## Second pass (2026-10-13, build 2000930, bridge m2.13): done, verified live
Five more tools (`PET_TOOL_ORDER` = 18); the bridge gains `pet.text/apply/undo/redo/render`
(12 `pet.*` methods total). Verified live: `test_pet.py` 17/17, `test_pet_live.py`
(core and `--render`, `--namedvalue`, `--depth`) all PASS, and the MCP loop
`particle_open` -> `particle_live_read` -> `particle_live_edit` -> `particle_render` ->
`particle_undo` -> `particle_redo` -> `particle_save` -> `particle_read` -> an unknown-class
refusal (nothing sent) -> a dirty `particle_reload` refusal -> `particle_reload {force:true}`.

| Tool | What | Live |
|---|---|---|
| `particle_live_read` | the live document text (`pet.text`): pet's own serialization of the tree (byte-identical to its save layout), plus `dirty`, `isNewFile`, `canUndo`, `canRedo`, `bytes` and the mapped host path; `root`/`counts`/`function` (`"m_Operators/2"`, `"m_NamedValueLocals/0"`) like `particle_read` | [V] |
| `particle_live_edit` | the same ops as `particle_edit` applied to the live tree as **one `pet.apply`, one undo step**; all-or-nothing on the host (structure always, schema unless `skipSchema`): a refused edit sends nothing and pet is untouched; `label` defaults to `MCP: <op names>` cut to 64 chars and shows as given in pet's Undo menu; text
cap 384 KB (`TextTooLarge`, nothing sent) | [V] add_function, add_variable, set_root_field |
| `particle_undo` / `particle_redo` | pet's own whole-document history: one bridge call each; an empty history is `NothingToUndo`/`NothingToRedo` (a result, retry-safe), never a crash | [V] |
| `particle_render` | the preview viewport rendered to a PNG by `pet.render` (offscreen `CToolScene::Render`, RGBA8888, default 1024x768, `width*height <= 8388608`), mapped to a host path, inlined as an image block when `inline:true` | [V] 320x240 / 480x320 in ~5 ms, the preview sprites visible in the PNG |

The render bracket note: the widget's Begin/Finish/Wait bracket that `pet.render` relies on is a
static reading (`research/pet-re.md`); the live runs verify the written PNG, the result stats and
that the main thread stays responsive — not the bracket internals.

### Dirty / undo rules
- A live edit leaves the document **dirty** (pet's own `SetModified`), so `particle_reload` /
  `particle_open` refuse until `particle_save` (or `force`). Verified live: a `particle_reload`
  after an edit answered `-32016 UnsavedChanges`; `{force:true}` reloaded.
- `particle_undo` / `particle_redo` leave it dirty too: undoing back to the saved text does not
  clear the flag (pet's own behaviour).
- `canUndo` **survives both** `particle_save` and `particle_reload {force:true}` — pet's history
  is not reset by a save or a `ReloadDocument` (verified live).
- `particle_live_read` is a pure read: `pet.text` leaves the document's path and dirty flag alone.

### What one pet.apply does (the bridge, m2.13)
1. **Gate**: label/text params validated (`-32602 InvalidParams`), then the same `vpcf_check`
   grammar + root-class gate as `pet.reload`/`pet.open` — malformed text never reaches pet.
   Refusals verified live (doc untouched, main thread fine): `no_kv3_header`, `unbalanced`,
   `wrong_root_class` -> `-32602 UnloadableFile`; empty text and a 65-char label -> `InvalidParams`.
2. **Parse before the undo step** (tier0's text `LoadKV3`, resolved per call from `tier0.dll`):
   a parse failure answers `UnloadableFile {reason:"kv3_load_failed"}` and leaves the tree and the
   history untouched — never an empty undo step.
3. **LoadFromFile's strip + filter** (IDA-checked, m2.13): on success the bridge strips the parsed
   KV3 to the schema and drops unregistered/out-of-version function nodes, exactly what
   `CPetDoc::LoadFromFile` does around its own `LoadFromKV3` (unchanged-tree failure after this
   point reports `arrays:null` + `arraysError`, not an error).
4. **Selection reset**: every selection entry set to pet's "none" `{9,-1}` — the selection is
   `{type, index}` pairs (count at `doc+0x68`, entries at `+0x70`), not node pointers, so nothing
   dangles; the reset bounds it because `GetChild` has no bounds check.
5. `RefreshCaches` and the two notifications pet's own edits emit (the preview recompiles next
   frame, the title shows modified); result dirty:true, `canUndo:true`, and the same `arrays`
   builder as `pet.active` (`pet.active` agrees live).

**Unknown function classes** are refused by the tool-side schema check before anything is sent
(`ValidationFailed`, nothing sent — verified live). What slips past `skipSchema` reaches `pet.apply`
and pet's own filter drops it silently: `applied:true`, the array count unchanged (re-verified
live through `pet.apply`; unknown root keys are stripped the same way).

### KV3 depth
One depth cap of **256** in both the Rust parser (`cs2_kv3::MAX_DEPTH`) and the bridge's checker:
a `pet.apply` nested deeper than 256 is refused (`too_deep`) without reaching pet. Probed live at
root-field depths 64/128/200/255/256: all parsed, main thread fine; 257 refused. pet's file loader
(the CUtlBuffer `LoadKV3` overload) was not probed separately.

### m_NamedValueLocals (array type 8)
`add_variable {name, value_type, index?, is_public?, default_value?}` and `remove_variable {index}`
in `particle_live_edit` (add/remove an entry in `m_NamedValueLocals`). pet's observed form, read
back live through `pet.text` (and the same in the saved file): each entry has `m_Name`,
`m_IsPublic` (**only when false**; true is the default and omitted), then
`m_DefaultConfig { m_ConfigName, m_ConfigValue, m_BoundValuePath, m_iAttachType, m_strEntityScope,
m_strAttachmentName }`, then `m_ValueType`; no `_class`. An entry without `m_DefaultConfig` gets a
full default config with `m_ConfigValue = null`; a non-string `m_ValueType` becomes `"PVAL_VOID"`
(invalid -> the default), so the string form is required. The element class pet reports:
`ParticleNamedValueSource_t`.

### Still unverified (second pass)
- A pet **UI selection during an apply**: both live runs were headless, so the selection-reset
  path is unexercised. The selection is `{type, index}` pairs, so the failure mode is a stale
  index (GetChild has no bounds check), not a dangling pointer; the bridge sets every entry to
  pet's "none" `{9,-1}` after the rebuild, as pet's undo apply and RemoveNode do. The tree rebuild
  itself (LoadFromKV3 into the live root) is the path pet's undo takes; the runs exercised it
  through pet.undo/pet.redo (tree and text restored byte-identical), not through the UI's Ctrl+Z.
- The render's Begin/Finish/Wait bracket internals (above).

## Safety rules
- **Never hand pet a file it cannot load.** pet's loader does not fail calmly. Live 2026-10-13
  (every case through `ReloadDocument`):

  | input | pet |
  |---|---|
  | `{{{ not kv3`, or a valid header + truncated body | main-thread stack overflow, process wedged |
  | balanced but invalid KV3 (a key with no value) | stack overflow, wedged |
  | root `_class` other than `CParticleSystemDefinition` | process exits (unhandled exception `c0000409`) |
  | unknown function `_class` | tolerated: the entry is silently dropped |
  | wrong field type, function list that is not an array | tolerated |

  A wedged process ignores podman's stop (kill the processes). Two guards, both verified live:
  (1) `particle_open` / `particle_reload` take the bridge lock, check the dirty state, then parse +
  validate the file and send at once (parse and structure are never skippable; `skipSchema` only
  skips the class/field checks pet tolerates); (2) the bridge checks the file itself before
  `OpenFile` and `ReloadDocument` (`kv3_check.h`: KV3 grammar + root `_class`, host path for
  `pet.open` = `<app>\content\csgo_addons\<-addon>\<path>`) and answers
  `-32602 UnloadableFile {reason, path}` (also `read_failed` for a missing file, which would be a
  modal). `cargo test -p cs2-bridge --test kv3_corpus` (needs the Wine content tree) runs the checker over every KV3 file in the content tree
  (all accepted). Writing a broken file to disk while pet has it open is harmless (no file
  watcher): only pet loading the broken bytes is dangerous. The same must-refuse set guards live
  text: `pet.apply` answers `UnloadableFile`/`InvalidParams` before pet sees anything (lexer gate
  + root class + full KV3 parse; every refusal verified live), so malformed text cannot reach the
  live tree either.
- **The recovery modal.** pet autosaves open documents every 15 s to
  `content/_toolsautosave/pet/<addon path>/<name>001.vpcf_autosave`. After an unclean exit (any
  killed/wedged runtime) `game/_toolsettings/pet/settings.txt` carries
  `"Closed Correctly" = false` (+ per-file `"ClosedCorrectly N"`), and the next `pet.open`
  blocks the main thread on the modal "Recover Files?". `cargo bridge runtime up` resets those flags
  (autosaves kept) before the launch; verified live after a kill.
- **One tools container.** pet runs in the default tools container (`cs2tools`,
  `cargo bridge runtime up`). The crash-flag reset is skipped when another tools container shares
  the tree (`cargo bridge runtime` cannot know whether pet is open there), so do not run two tools
  containers against the same workshop tree. `pet.load` issued immediately after `up` can
  return without loading (toolframework2 not yet verified): retry once `control.ping` shows
  toolframework2 verified.

## The schema snapshot
`<repo>/hammer-mcp/research/pet-schema.json` — committed, the live snapshot
(380 classes, `placeholder:false`, build 2000930; since the second pass it carries
`ParticleNamedValueConfiguration_t`, which the `m_NamedValueLocals` default-config check needs). Refresh in a live session with
`particle_types {writeSnapshot:true}`: it fetches `pet.schema` for every registered class plus
the root and the two structural ones and writes the file atomically; a failed lookup aborts the
write (never a partial snapshot); identical on a second live run. `test_pet.py
--write-snapshot <file>` produces the same shape. With no usable file at that path the built-in
copy (compiled from the same file) is used and `schemaSource` says `built-in (build 2000930)`;
a snapshot that is placeholder/malformed fails closed with `SchemaUnavailable` — `skipSchema`
skips only the class/field check, never the structural one. `resourcecompiler` accepts unknown
classes/fields (only a KV3 syntax error fails it), so schema validation is this server's job.

## File format notes
- pet writes KV3 text `vpcf66`, tabs, CRLF, no final newline — exactly what the cs2-kv3 writer
  behind `Particle::write` produces; `resourcecompiler` compiles that form.
- `m_Children` entries have **no `_class`**: `{ m_ChildRef = resource:"particles/x.vpcf" }`
  (the element class is `ParticleChildrenInfo_t`). `m_NamedValueLocals` entries (element class
  `ParticleNamedValueSource_t`) are handled the same way now; pet's observed written form is in
  the second-pass section.
- pet may add a root
  `m_controlPointConfigurations = [ { m_name = "preview" m_drivers = [ { m_iAttachType = "PATTACH_WORLDORIGIN" m_entityName = "self" } ] } ]`
  when it saves (its preview control point).
- `m_nBehaviorVersion`: pet greets a file without one with the red banner *"This particle
  system is using old behavior version 0, while current version is 12!"* (a missing field reads
  as 0) and does not write the field itself when it is 0; the template writes
  `m_nBehaviorVersion = 12`, and the validator warns when it is missing or older.

## Done in the second pass (was "next")
- Live node edits through pet's document model with undo: done. The whole-document `pet.apply`
  covers add/remove/move/set with one undo step and full snapshots (`research/pet-re.md`
  "Second pass"). The native single-node path (`AddFunctionNode` / `RemoveNode`) was not taken:
  its only extra is that `AddFunctionNode` selects the new node in the UI, and
  `pet.addFunction`/`pet.removeFunction` were decided against.
- `m_NamedValueLocals`: done (type 8, add/remove variables, pet's observed form above).
- Vision/preview: done as far as `particle_render` (the preview scene to a PNG as an image
  block). Screenshots or OCR of the pet window are still not planned or done.

## Still open
- A bridge-side guard for `pet.open` needs RE of pet's path resolution (the bridge cannot
  resolve pet's search paths into host files): today the tool-side validation is the only
  guard (absolute paths are refused by the tool gate before `OpenFile`; the bridge-side
  `vpcf_check` covers the resolved host file).
