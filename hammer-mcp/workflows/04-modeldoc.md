# 04 — ModelDoc editor

## Goal
Edit models, props and player models (`.vmdl`, KV3 text) and get them into the running ModelDoc and
compiled, with the same safety model as Hammer (explicit writes, backups, no auto-save).

## Status (first pass + vision pass, 2026-10-13, build 2000930): done, verified live
The **file route**: `.vmdl` sources are edited on disk, the running editor reloads them, then they are
compiled. Live per-node edits with in-editor undo are the second pass (deferred, see below).

| Tool | What | Live |
|---|---|---|
| `model_status` | editor loaded/verified, missing `modeldoc.*` caps, active doc + host path, hint | [V] |
| `model_read` / `model_list_nodes` | node tree from the file (`source:"file"`) | [V] |
| `model_edit` | ops `add_node/remove_node/move_node/set_attr/remove_attr`, all-or-nothing, class-checked, backup + atomic write, addon only | [V] |
| `model_create` | `static_prop` / `skeletal` templates | [V] (written + classes; compile of a template needs a real mesh) |
| `model_compile` | route `resourcecompiler` (default; `podman exec` into the ModelDoc container) or `editor` (bridge `Compile`); `ok` = `.vmdl_c` updated, or skipped as up to date | [V] both routes |
| `model_open` | pre-validated open in the running editor (refuses dirty/unknown unless `force`) | [V] |
| `model_reload` | `ReloadDocument` after `model_edit` (drops the editor's undo history) | [V] |
| `model_save` | editor save to its own path (never Save As) | [V] clean doc |
| `model_node_types` | the 413 `_class` names; `writeSnapshot` refreshes `research/modeldoc-nodetypes.json` | [V] |
| `model_render` | offscreen render of the active session's main 3D viewport (its current camera) to `<app>/renders/<name>.png`, inlined as an image block (downscaled above 1920x1080) unless `inline:false`; needs an open `.vmdl` tab (`NoViewport no_session` otherwise) | [V] (vision pass, bridge m2.14) |

Typical loop: `model_status` → `model_open` → `model_edit` → `model_reload` → `model_render` (look) → `model_compile`.

Pieces: `crates/cs2-kv3/` (KV3 text parser/writer), `hammer-mcp/src/vmdl/` (model layer), `src/tools/model_file.rs`
and `model_live.rs`, bridge m2.11 `modeldoc.*` (`crates/cs2-bridge/PROTOCOL.md` §16), `model_render.rs` + bridge m2.14 `modeldoc.render` (`crates/cs2-bridge/PROTOCOL.md` §15.4), sigs `sigs/signatures/modeldoc_editor.json`. RE: `research/modeldoc-re.md`.

## Rule: one ModelDoc instance at a time
Every live ModelDoc step runs in the single tools container started with
`TOOL=modeldoc cargo bridge runtime up` (label `cs2mcp.tool=modeldoc`). `cargo bridge runtime` enforces it
(flock + label check): a second `TOOL=modeldoc up` fails, no `up` replaces the running instance,
and `open <x>.vmdl` refuses in any other container. Workers never start their own: they queue
on the one instance, and the orchestrator owns its lifecycle. `model_compile` only `exec`s into the
labelled container; any future Rust launch path for ModelDoc must apply the same check.

## Facts that shape the tools (all [V runtime])
- `modeldoc.open` paths are relative to the **addon** content root (`models/x.vmdl`); `csgo_addons/<addon>/...` returns 0.
  The same holds for `cargo bridge runtime open`: `vtmsend open_asset csgo_addons/<addon>/models/...` raises the modal
  "Unrecognized Asset", which blocks the main thread. Use `cargo bridge runtime open models/<x>.vmdl`.
- `model_render` follows the **active** session: after `model_open` of another file, the render shows
  that file. A `.vmdl` created after the tools started renders as an empty grid, because it has no asset
  and its mesh does not load.
- The editor's compile status (`session+0xF98`) is an object, not a string: the bridge never reads it;
  compile success is the `.vmdl_c` mtime (or resourcecompiler's summary).
- A `.vmdl` created after the tools started has **no asset object**: editor compile is impossible
  (`no_asset`), compile it with resourcecompiler.
- `OpenFile` modals (missing file, outside the content dir, load errors) block the main thread: the
  bridge refuses absolute/escaped/non-`.vmdl` paths and the tool pre-validates the file. A model with
  an unresolvable mesh opened without a modal.
- resourcecompiler: `-game /app/game/csgo` (not the addon dir); a bare `[FAIL]` follows its error line;
  dependency failures do not count in `failed`.

## Deferred (second pass)
- Live node edits through `CModelDoc` with undo (`CreateNode`, attribute writes): the attribute
  write/record ordering is still unverified static RE (idabins `docs/flows/modeldoc-edit.md`).
- Editor dirty-state paths (`model_reload` while dirty → `UnsavedChanges`, `force` →
  `discardedChanges:true`, `model_save` of a dirty doc): covered by mock tests only; needs a UI edit to
  make the live doc dirty.
- Template key names beyond `_class` (`filename`, `import_scale`, ...) are VRF-derived [I]: only a
  compile with a real mesh source confirms them (the install ships no `.fbx/.dmx`).
- Player-model specifics (AnimGraph, hitboxes, bodygroups) beyond what the generic node ops reach.
- Can Hammer and ModelDoc share one `-tools` session with one bridge? (per-module `NEED_*` gating says yes.)
