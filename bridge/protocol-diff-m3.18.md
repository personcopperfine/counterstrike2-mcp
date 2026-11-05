# Protocol changes in m3.18: `node.createPrefab`, `node.prefabFromSelection`, `mesh.setFaceTexture` mode `axes`

Bridge build `m3.18` is m3.17 plus two methods and one mode. RE notes:
`target/orch/import/re-prefab.md` (hammer.dll 2000933). Five pins (already in
`sigs/signatures/hammer.json` -> `crates/cs2-bridge/src/generated/pins.rs`): `HFN_DOC_ADD_PREFAB`,
`HFN_DOC_PREFAB_FROM_SELECTION`, `HFN_PREFAB_SET_TARGET_MAP` (pinned, not called), `HFN_PREFAB_GET_TARGET_MAP`,
`HFN_PREFAB_GET_WORLD`. Function types are typed in `ops/hammer.rs`; the bodies are `ops/prefab.rs`.

Both new rows are `NEED_HAMMER`, queued main-thread writes behind the usual write gate
(`-32003` / `-32018` / `-32004 expectDoc`), registered in `RUST_METHODS`, so `control.capabilities` lists them.
Replies carry the m2.17 write `doc` tail.

## Paths (both prefab methods)

`path` is a map of the active addon: `maps/.../x.vmap`, validated by `map.open`'s own shape gate
(`-32602`: no `..`, no escapes, no absolute path, `.vmap`, under `maps/`), then turned into the absolute content
path `<app>\content\csgo_addons\<addon>\<path>` by `map.open`'s host gate (`-32017 no_app_root / no_addon`).
Hammer is handed that ABSOLUTE path (the UI passes absolute paths; Hammer itself makes it content-relative).
Neither method accepts a path of a map that is open in Hammer (any session, found by `map.open`'s session walk):
`-32602 {reason:"path: that map is open in Hammer ..."}`.

## `node.createPrefab`

```
{ path, origin?:[x,y,z], angles?:[pitch,yaw,roll] }
```

- Refusals before the undo group opens: bad path (`-32602`), bad `origin` (finite, same range as `node.setTransform`)
  or `angles` (`-32602`), missing file / not a file (`-32014 AssetNotFound no_such_file|not_a_file`), not a binary-v9
  vmap (`-32014 InvalidMapFile`), open map (`-32602`).
- Sequence (the "ImportPrefab" command without its dialog): `AddOperation(hist, selection, "MCP: create prefab", 0)`
  -> `HFN_DOC_ADD_PREFAB(doc, abs)` (null -> `-32603 InternalError`, group open, nothing recorded) ->
  `RecordNode(hist, prefab + CMapNode::BASE_OFFSET_IUndoable, 0)` (flag 0 as the command does) ->
  `SetOrigin` / `SetAngles` (slots 17/18, the `node.setTransform` writes) -> `DocSetModified`.
- If Hammer stored no target path (the silent failure in the notes): `-32603 PrefabPathRejected`,
  `maybeExecuted:true` (an empty prefab IS in the document and in the undo group: `undo.undo`).
- Reply:
  `{nodeID, class:"prefab", targetMapPath, loaded, origin, angles, undoLabel:"MCP: create prefab"[, warning]}`.
  `targetMapPath` is read back (`GET_TARGET_MAP`, content-relative, e.g. `maps/sub/x.vmap`); `loaded` is
  `GET_WORLD != 0`. `warning` appears when the map is not loaded (it may load after `map.save` + `map.reload`) or
  when the stored path differs (case-insensitively) from the requested one.
- Undo: one group, `undo.undo` removes the prefab node. The referenced `.vmap` is untouched.

## `node.prefabFromSelection`

```
{ path, centered?: bool }      // default false = keep the world offset
```

- Checks first (all before the undo group): path as above; the target must NOT exist (`-32602 ... already exists`;
  the worker overwrites silently); its directory must exist (`-32014 directory_missing`, the caller creates it);
  not an open map; the selection object exists, count > 0 (`-32017 PrerequisiteFailed selection_empty`), and the
  selection mode is 0..2 = groups/objects/meshes (`-32017 selection_mode`).
- Sequence (the "CreatePrefabFromSelection(Centered)" command without its save dialog):
  `AddOperation(hist, selection, "MCP: create prefab from selection", 0)` ->
  `HFN_DOC_PREFAB_FROM_SELECTION(doc, abs, frame, centered)` (null -> `-32603 InternalError`, `maybeExecuted:true`) ->
  `DocSetModified`. The worker records its own prefab, copies the selection into it, deletes the originals, WRITES
  the .vmap, and selects the prefab.
- `frame`: a ZERO-filled 256-byte buffer, not an identity matrix. The notes (section 3) say the worker only reads a
  3x4 matrix at `+0x24` of a `0xB4`-byte struct to derive the prefab's angles, and recommend a zero buffer: its
  `MatrixAngles` result is angles (0,0,0), identical to an identity frame, and nothing depends on an offset the
  headers do not declare. (Deviation from the task text, which asked for identity at 0x24: following the notes.)
- Reply: `{nodeID, class, targetMapPath, written, loaded, selectedCount, centered, fileStaysOnUndo:true,
  undoLabel:"MCP: create prefab from selection"[, warning]}`. `written` = the file exists after the call.
  `selectedCount` is the selection size BEFORE the call (a good proxy for the originals removed; the exact removed
  count is not read from the worker, hence no `removed`).
- Undo: one group restores the original nodes and removes the prefab node. The written `.vmap` STAYS on disk
  (`fileStaysOnUndo:true`); delete it yourself if unwanted.

## `mesh.setFaceTexture` mode `axes`

```
{ nodeID, texSet?, mode:"axes",
  faces:[ {index, axisU:[x,y,z,offset], axisV:[x,y,z,offset], scale:[su,sv]}, ... ] }
```

Absolute axes per face (`HFN_MESH_SET_FACE_TEXTURE` writes one triple into every face it is given, so it is called
once per face, all inside the one `MCP: set face texture` undo entry). Validation (`-32602`, before the game is
touched): 1..65536 entries, unique `index` < the mesh's face slots, every number finite, `scale` > 0 on both
components, `axisU`/`axisV` xyz not zero-length and not parallel (`|u x v| > 1e-6 |u||v|`; non-unit axes are accepted),
`texSet` 0|1; `justify`, `keepScale`, `shift`, `scale`, `rotate` are refused with this mode. A face the getter
cannot read is `-32603 TextureSetUnknown` (before the group opens; `maybeExecuted` after a write).

Reply: the usual `mesh.setFaceTexture` shape (`mode:"axes"`, `touched`, `readBackTruncated`, `faces` read back per
face, `undoLabel`) plus `"exact": bool` (only in this mode): every touched face read back bit-for-bit what was
requested. A write that Hammer normalised or did not store is NOT an error: compare the read-back with the request.

## Verified vs inferred

Verified (read in the disassembly, RE notes sections 0-3, and by unit tests): the call sequences, the undo usage, the
signatures, that origin/angles slots 17/18 are valid on `CMapPrefab`, `RecordNode` flag 0 for the import path.
Inferred / NOT run live: that `HFN_DOC_ADD_PREFAB` loads the referenced map before it returns (`loaded` is read back
to tell); that the zero frame yields angles 0; that GET_TARGET_MAP returns the content-relative path under Wine;
that an absolute Wine host path is converted by Hammer's filesystem as it is for the UI's dialog paths; that
Hammer stores `axes` verbatim (`exact`).

## Live checks (for the reviewer; not run by the implementer)

1. `node.createPrefab` of a small map: `loaded:true`, `targetMapPath` content-relative, the prefab shows in
   `node.list` as class `prefab`; `origin`/`angles` honoured; `undo.undo` removes it; `map.save` + `map.reload`
   keeps it. A second call with the active document's own path -> `-32602` (open map).
2. `node.prefabFromSelection`: select two meshes (selection mode objects), run with `centered` false then true on
   fresh paths: file exists (`written:true`), originals gone from `node.list`, the prefab selected, `undo.undo`
   restores the originals and leaves the file. Existing target / missing dir / empty selection / face mode errors.
3. `mesh.setFaceTexture` `axes` on a box face: read-back equals the request (`exact:true`), a non-unit axis, `undo.undo`
   restores the previous axes, other faces untouched.

## Live result (2026-10-29, accept_p4 `maps/convoy_slice.vmap`, build 2000933)

All three checks passed against the installed m3.18 (raw bridge client, `target/import-re/live_m318.py`, gitignored):

- `mesh.setFaceTexture` `axes` on a new box, faces 0 and 2 (one non-unit axis `[0,0,-2,3.5]`): `exact:true`, read-back
  equals the request; parallel axes refused `-32602`; `undo.undo` pops `MCP: set face texture`.
- `node.prefabFromSelection` over two meshes (mode `objects`), `centered:true`: `written:true`, `loaded:true`,
  `targetMapPath` content-relative, originals gone, prefab origin = the selection's bounds centre; `undo.undo` restores
  both meshes and the file stays; `undo.redo` brings the prefab back. Empty selection, existing file and missing
  directory refused as specified.
- `node.createPrefab` of that file with `origin`/`angles [0,45,0]`: `loaded:true`, transform honoured (render shows the
  turned copy); the open document's own path refused (open map), a missing file `-32014 no_such_file`.
- `map.save` + `map.reload` keep both prefabs with their origins. The zero frame gave angles (0,0,0) as inferred.
- Not checked: other faces' axes untouched after `axes` (no texture read in `mesh.get`), `centered:false`.
