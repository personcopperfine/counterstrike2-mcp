# 06 — Asset browser / asset editor

## Goal
Find and open assets by name from the model: list/search the content tree, open an asset in the
right tool (map -> Hammer, `.vmdl` -> ModelDoc, `.vpcf` -> pet), and report what is open.

## Prerequisites
- 03 done (session tools, path map). Read `../idabins/docs/data/vtoolmessage.md` (Phase-1
  `open_asset`/`command` verbs), `tool-plugin-admission.md` "MCP relevance",
  `file-edit-reload-compile.md` §B (open on an already-open map leaks a session),
  `../idabins/docs/interfaces/CHammerApp.md` (session vector), `signatures/assetbrowser.json`,
  `assetsystem.json`, `assetpreview.json` (what is already located).

## Steps
1. **File-system route (no bridge)**: `list_assets {root, glob, kinds}` and `find_assets {query}`
   over `<app>/content/<addon>/` and `game/csgo/` (compiled), mapping extensions to tools; cached
   index with mtime invalidation; host->Windows path rendering for later bridge calls.
2. **Open via VToolMessage** (`vtmsend.exe` `_message="open_asset" _data="maps/x.vmap"`): the proven
   Phase-1 verb, routed by the framework to the owning tool (`runtime.sh cmd_open`). Tool `open_asset`
   with the Hammer session-leak pre-check (`doc.list` when advertised; else warn) and
   `AssetNotFound` on a failed lookup.
3. **Bridge route** (`file.open_asset`, `-32011` in m2.8): same tool, preferred when implemented
   because it returns `{opened, existing}` synchronously.
4. **Asset browser state**: what the Asset Browser shows/selects is Qt UI — out of scope; instead
   `asset_info {name}` via `AssetSystem001` lookups (bridge method to add: `asset.info` ->
   type, content path, compiled path, exists) — needs sig entries in `assetsystem.json`.
5. **Asset editor** (property grids for `.vmat` etc.): file route only (`vmat` is KV3 text) —
   `read_kv3`, `write_kv3` generic tools with explicit-writer policy; compile via `resourcecompiler`
   plan.

## Definition of done
- `find_assets "dust2"` returns content + compiled hits with the owning tool; `open_asset` opens a
  map (or reports `existing`) without leaking a session; `asset_info` answers for one of each kind.

## Open questions
- Does `open_asset` through VToolMessage reach ModelDoc/pet for non-map assets in the csgo mod
  (pet is visibility-filtered)? Runtime check.
- `asset.info` bridge method: which `AssetSystem001` virtuals (`vfn+184/192` in the Hammer page) are
  stable enough to pin — tag `[I]` until verified.
- Whether a generic KV3 writer belongs in hammer-mcp or in a shared "files" tool group.

## Evidence
- VToolMessage `open_asset`/`command` verbs [V, used daily by `runtime.sh`]; `file.open_asset` guard
  analysis [V static]; asset browser running under Wine (`worknotes/evidence/2026-10-05-asset-browser-wine.png`).
