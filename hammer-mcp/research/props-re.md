# Hammer prop support RE notes (build 2000930)

Facts the prop tools and bridge m2.18 are built on (`create_props`, `set_node_transform`, entity I/O).
Every address the bridge uses is an entry in `sigs/signatures/hammer.json` (pins -> `bridge_pins.h`);
never copy a number from here into code. Post-implementation facts (live acceptance, mesh scale
readback) are in `hammer-mcp/AGENT.md` "## Props".

hammer.dll build **2000930**, sha256 `d3ee1e5cf8351660…` (the IDB `idabins/windows/game/bin/win64/tools/hammer.dll.i64`
and the installed `workshop/run/app/game/bin/win64/tools/hammer.dll` are byte-identical). The IDB still
carries the old build's `sub_<addr>` names, so `CMapDoc::sub_18087F6E0_CreateNodeByName` really lives at
`hammer!0x18087F720`. **Every address here is a real 2000930 address** (RVA = address - 0x180000000).

Evidence tags: **[V live]** means seen in the running Hammer through a probe build of the bridge (see
"Probe" at the end). **[V bin]** means read in the decompiled or disassembled binary. **[I]** means inferred.

---------------------------------------------------------------------------------------------------

## Q1. Model refresh (prop model loads and draws)

### Answer
* **The current bridge path does NOT load or draw the model.** [V live] `entity.create {class:"prop_dynamic",
  keyValues:{model:"models/generic/terrace_set_01/terrace_chair_01.vmdl"}}` gives only the white
  placeholder box, the same box as a `prop_dynamic` with no model. Re-rendering several times does not
  change the CRC (renders `workshop/run/app/renders/pp_cam1.png`, `pp_cam3.png`). The same applies to
  `entity.setKeyValues {model}`, because it uses the same raw setter (`apply_keyvalues` → `HFN_SET_STRING`).
* Why [V bin]: `CDmAttribute::SetValueString` (0x1805CF3A0) ends in `sub_1805EA1A0`, which calls the
  *owning element's* vtable slot 0 (`OnAttributeChanged`) only when attribute flag 0x200 is set. The owning
  element is the `entity_properties` child (`CEditGameClassProps`, vtable 0x1817602B8), and its slot 0 is
  `_guard_check_icall_nop`. The DataModel notify bits it sets are not consumed by the entity helpers either
  (the live result confirms this). So nothing reaches the CMapEntity.
* **Hammer's own path is `CMapGameDataNode::SetKeyValue(node, key, value)` at `hammer!0x18083E450`**
  (non-virtual). [V bin + V live] Its callers include the asset-spray "place prop_static with model"
  (0x180AF8310 / 0x180AF94C0), "Update local offset" (0x1803AAB90), the KV3 entity importer
  (0x180328110) and about 90 others. What it does:
  1. `elem = DataModel->GetElement(*(u32*)(node+0x538))` (the entity_properties handle). It reads the old
     value of `key` by walking the attribute list case-insensitively (`V_stricmp`).
  2. It writes the new value with `SetValueString` on that attribute, or on a newly created
     `CreateAttribute(elem, key, 5)`, then clears the element's cached fields `elem+0xB0` (byte) and
     `elem+0xB8` (qword).
  3. **If old != new** (case-insensitive) it calls `node->vfn[0x858/8 = 267](node, key, value)`, the
     `OnKeyValueChanged` handler. For CMapEntity that is 0x180898BD0: for FGD fields of model type, or
     fields with `can_change_model`, it rebuilds the helpers (`sub_180899200`) and updates the
     `CModelHelper` (`sub_1807ED140`), which loads and draws the model and updates the bounds. It also
     handles rendercolor, renderamt, disableshadows, solid and visoccluder. For `targetname` it does extra
     work. If `byte node+0x560 == 0` it also calls `sub_180867660(node,0)` (marks modified) and
     `sub_1808673C0(node,key)` (doc listeners).
* [V live] Through the probe: create a `prop_dynamic` with no model, then `SetKeyValue(node,"model",M)`,
  and **the chair is drawn at once** (`renders/pq_1.png`). On a node whose model was already raw-written
  to the same text, `SetKeyValue` with the same value **does nothing** (the old==new early-out; the box
  stays). Setting `""` and then a new model draws it (`pq_2.png`, dust crate).
* [V live] Side note: a node that comes back through redo or reload (deserialization) builds its helpers,
  so raw-written models show after `undo.redo` (`pq_5.png`) and also after a map reload.

### Bridge recipe (replaces the String branch of `apply_keyvalues`)
On the tools main thread (as now):
```c
CDmElement *child = g_hfn.GetEntityProperties(node);   // MUST exist first: SetKeyValue derefs the element unguarded
// existing String attribute OR missing key:
g_hfn.SetKeyValue(node, key, text);                    // HFN_SET_KEY_VALUE: void(void *node, const char *key, const char *value)
// typed (int/float/bool) attribute: keep today's typed setter, then notify like Hammer does:
//   node->vt[267](node, key, text)   (OnKeyValueChanged, vfn+0x858; pin a vslot if used)
```
* `value` is a plain C string (SetKeyValue interns it itself). An empty string is fine.
* Applied/`previous` reporting can stay as it is (read before the write). Note that SetKeyValue finds
  existing keys **case-insensitively** while `FindAttribute` is symbol-exact, so the `previous` pass can
  disagree on case-variant keys. Either refuse such keys or report them.
* Keep `g_hfn.DocSetModified(doc, doc)` after the writes. [V live] With it, map.save writes the file
  (`skippedClean:false`).
* **Undo (big win, Q1 and setKeyValues both):** call
  `g_hfn.RecordNodeTree(hist, node->undoable, 1)` **once per node, after `undo_open` and before the first
  write**. That is what every Hammer edit does (0x1803AAB90 "Update local offset", 0x1803A2EE0, "Add New
  Output"…).
  [V live] With RecordNodeTree, `undo.undo` reverts the model value **and the drawn model** (`pq_3.png`),
  and `undo.redo` brings both back. Without it, undo pops the group but the value stays (the same
  "group-only" gap m2.15 documented). `valueUndo:"group-only"` can then become a real undo (needs a
  protocol diff).
* **entity.create**: keep `CreateNodeByName` (it records the creation itself), then write the keyvalues
  with `SetKeyValue`. No extra RecordNodeTree is needed for a node created in the same group.
  [V live] Keyvalues written after `CreateNodeByName` survive undo→redo (the redo snapshot is taken at undo
  time; `pp_redo` was found again after redo). The asset-spray template (0x180AF8310) does the same calls
  in a slightly different order (SetKeyValue before InsertNode + RecordNode).

---------------------------------------------------------------------------------------------------

## Q2. Node transform (origin / angles / scales)

### Answer
* Transform values are C++ fields of CMapNode: **origin +0xA0, angles +0xAC, scales +0xB8** (3 floats
  each). [V bin] The base stores are `sub_1809EB1C0` → +0xA0, `sub_1809EB1E0` → +0xAC, `sub_1809EB280` →
  +0xB8. They are not separate DME attributes. They are written through **virtual setters**:

  | slot | vfn off | meaning | CMapEntity | CMapMesh | CMapNode base |
  |---:|---:|---|---|---|---|
  | 17 | +0x88 | SetOrigin(node, const float[3]) | 0x1808959E0 | 0x1809A23F0 | 0x180851680 |
  | 18 | +0x90 | SetAngles(node, const float[3]) | 0x180895A50 | 0x1809A2540 | 0x180851700 |
  | 19 | +0x98 | SetScales(node, const float[3]) | 0x180895B40 | 0x1809A2690 | 0x180851780 |

  Group and prefab override these too (vtables 0x18178D4A8 and 0x18177D800). Each setter stores the field,
  forwards to the node's helpers, and calls `vfn+0x80(node,3)`, which invalidates the transform and bounds.
  The entity versions also move attached children (`sub_18085D100`). **No separate bounds or view refresh
  call is needed.** [V live] The chair moved, rotated 45° and doubled in size at once (`pq_6.png`).
* Hammer's template is **"Clear Rotation and Scale" `hammer!0x1803A2EE0`** [V bin]: `AddOperation(hist,
  *(doc+0x120), label, 0)`, then for each node `RecordNodeTree(hist, node+0xC8, 1)`,
  `vfn+0x98(node,{1,1,1})`, `vfn+0x90(node,{0,0,0})`. The same shape appears in "Update local offset", "Move
  Node" (0x18007DD50) and others.
* [V live] Undo restored origin, angles and scales exactly (probe read after `undo.undo`).

### Recipe: `node.setTransform {nodeID, origin?, angles?, scales?, expectDoc}`
```c
// gates as entity.setKeyValues: active_doc, doc_editable, expect_doc_ok; node = geo_find(doc,id);
// class check: obj_is(node, HV_MAPENTITY_VT) or obj_is(node, HV_MAPMESH_VT) (pinned slots below), else refuse;
// refuse a locked node (g_hfn.NodeIsLocked(node,0)); validate all three arrays BEFORE undo_open.
CMapDocHistory *hist = undo_open(doc, "MCP: set transform");
g_hfn.RecordNodeTree(hist, node->undoable, 1);
if (has_origin) node->vt->SetOrigin(node, origin);   // slot 17
if (has_angles) node->vt->SetAngles(node, angles);   // slot 18
if (has_scales) node->vt->SetScales(node, scales);   // slot 19
g_hfn.DocSetModified(doc, doc);
// reply: read back node+0xA0/+0xAC/+0xB8 (HOFF_NODE_ORIGIN/ANGLES/SCALES) + sb_add_doc_tail
```
* Mesh: CMapMesh's overrides wrap the store in a CMeshEditScope. Setting a mesh's origin moves the mesh
  (pivot). [I] Its vertices are local to the pivot, so the world geometry moves with it. Not tested live
  on a mesh.
* Order: Hammer does scales then angles. Order does not matter for independent fields.
* **entity.create `scales`:** `CreateNodeByName` takes only origin and angles. Call
  `node->vt->SetScales(node, s)` right after it, in the same group. No RecordNodeTree is needed, because
  the creation record already covers it. Not tested live, but it is the same slot that worked on an
  existing node.

### Existing bug found [V bin]
`bridge/s2sdk.h` declares slot 17 as `SetAngles` (pinned `HVS_MESH_SET_ANGLES`, "CMapMesh::SetAngles").
**Slot 17 is SetOrigin.** The proof: the CMapNode base slot 17 stores +0xA0, which
`CreateNodeByName`/`SetPositionAndAngles` fill from the *origin* argument, and "Clear Rotation and Scale"
sets angles through slot 18 (+0x90). The IDB name and the mesh's CMeshEditScope label "SetAngles" are
misleading: Valve reused the same scope string in slot 18 as well.
`geo_reset_mesh` (mcp_bridge.c ~3317) therefore **zeroes the origin, not the angles**, of a rotated mesh
before `SetPivotPosition(0)`. A rotated mesh keeps its rotation in `mesh.setGeometry`, so the result is
wrong. Fix: call slot 18 (new pin `HVS_MESH_SET_ROTATION`) with zeros. The entry `HVS_MESH_SET_ANGLES` is
kept as is (rule: never rename an entry) and its use is retired.

---------------------------------------------------------------------------------------------------

## Q3. Entity I/O connections

### Answer
* **In memory, the connections are a C++ vector on CMapEntityIONode, not the DME array**. [V bin + V live]
  The layout is `count = *(int*)(node+0x4A0)`, `data = *(u8**)(node+0x4A8)`, `cap = node+0x4B0`, with
  **80-byte elements**:

  | off | type | saved DME attr (`DmeConnectionData`) |
  |---:|---|---|
  | +0x00 | CUtlString (char*) | none: source entity name; load fills it with the owner's targetname (or `"<<null>>"`); not saved |
  | +0x08 | CUtlString | `outputName` (String) |
  | +0x10 | CUtlString | `targetName` (String) |
  | +0x18 | CUtlString | `inputName` (String) |
  | +0x20 | CUtlString | `overrideParam` (String) |
  | +0x28 | int | `targetType` (Int). Hammer's UI default and every saved Valve connection use **7** (the DME ctor default is 2) |
  | +0x30 | KeyValues3 (16 B) | `paramMap` (optional) |
  | +0x40 | float | `delay` (Float) |
  | +0x44 | int | `timesToFire` (Int, -1 = always) |
  | +0x48/+0x49 | u8,u8 | none (0) |
  | +0x4C | int | serial (global counter) |

  `connectionsData` (attribute created in `CMapEntityIONode` OnConstruction 0x180989320, element array
  type 33, storage node+0x4E8) is **only materialized at save** (`sub_180984B50` → `sub_1809876D0`: one
  `DmeConnectionData` per vector entry; attribute names verified in `CDmeConnectionData` OnConstruction
  0x1809841D0 and in saved files). It is **consumed at load** (`sub_180984A40` → `sub_180987380`) and then
  emptied. **Do not write `connectionsData` live**: the next save rebuilds it from the vector.
* Hammer's add/remove ("Object Properties > Outputs") [V bin]:
  * "Add New Output" `0x18020CA20`: `AddOperation(GetUndoHistory(doc), *(doc+0x120), "Add New Output", 0)`;
    per node `RecordNodeTree(hist, node+0xC8, 1)`; then `CMapEntityIONode::AddConnection(node, &id, &src)`
    = **`hammer!0x180987220`**.
  * "Delete Output" `0x18020CD80`: the same group plus RecordNodeTree, then
    `CMapEntityIONode::RemoveConnection(node, id)` = **`hammer!0x180987E70`**.
  * `AddConnection(node, u32 *outId, const ConnSrc *src)` grows the vector, constructs the new element
    (`sub_18099F4B0`) and copies `src` through `sub_18099F5E0`. That copy reads the five strings from src as
    `char*` (strlen + `CUtlString::SetDirect`), copies `+0x28,+0x40,+0x44,+0x48,+0x49`, and **copies the KV3
    at src+0x30** (`sub_1813E9550`). The source KV3 must therefore be a valid, initialized KeyValues3.
    `*outId = index << 16`.
  * `RemoveConnection(node, id)` returns 0 unless `(id & 0xFFFF) == 0` and `id>>16 < count`, then frees and
    shifts the entries. **Indices shift after a removal**, so remove in descending index order.
* [V live] Probe results: adding `OnUser1 → pp_dyn2.SetAnimation("idle") delay 0.5 times -1` and a
  second connection showed both in the vector. `map.save`, then `vmap_dmx.py get 108` shows **exactly**
  those two `connections` with `targetType 7`. `rmconn index 0` left the second one. `undo.undo` restored
  it, and a second `undo.undo` removed the second add. After `map.reload` both come back, with `+0x00 =
  "pp_dyn1"` (the source's targetname).
* **Does `entity.get` report connections today? No.** `op_entity_get` hardcodes `"connections":null`
  (mcp_bridge.c ~3172). Reading the vector is cheap and safe: copy the strings with `probe_str`, as the
  probe did.

### Recipe: `entity.addConnections {nodeID, connections:[{output, target, input, param?, delay?, timesToFire?}], expectDoc}`
```c
// validate everything (node is an entity: obj_is HV_MAPENTITY_VT; not locked; all strings <= N bytes,
// output/target/input non-empty; delay finite >= 0; timesToFire int >= -1) BEFORE undo_open
CMapDocHistory *hist = undo_open(doc, "MCP: add connections");
g_hfn.RecordNodeTree(hist, node->undoable, 1);
for each c:
    u8 src[80] = {0};                       // aligned 8
    *(const char**)(src+0x00) = targetname_or_NULL; // optional; Hammer's own load sets the source name
    *(const char**)(src+0x08) = c.output;   *(const char**)(src+0x10) = c.target;
    *(const char**)(src+0x18) = c.input;    *(const char**)(src+0x20) = c.param ? c.param : "";
    *(int*)(src+0x28)  = 7;                 // targetType: what the UI and all saved connections use
    g_hfn.Kv3Init(src+0x30, 1, 8);          // HFN_KV3_INIT: void*(void *kv, u8 type, u8 subtype), exactly "Add New Output"
    *(float*)(src+0x40) = c.delay;          // default 0
    *(int*)(src+0x44)   = c.timesToFire;    // default -1
    u32 id = 0xFFFFFFFF;
    g_hfn.AddConnection(node, &id, src);    // HFN_ADD_CONNECTION
    g_hfn.Kv3Free(src+0x30);                // HFN_KV3_FREE
    // report index = id >> 16
g_hfn.DocSetModified(doc, doc);
```
`src`'s string pointers are only read during the call (copied into Hammer-owned CUtlStrings), so bridge
buffers are fine. Never call the CUtlString destructor on them.

### Recipe: `entity.removeConnections {nodeID, indices:[...] | match:{output?,target?,input?}, expectDoc}`
Resolve the indices against the live vector first (refuse out-of-range indices and duplicates), then
`undo_open`, `RecordNodeTree(hist, node->undoable, 1)`, and for each index in **descending order**
`g_hfn.RemoveConnection(node, (u32)idx << 16)` (a 0 return is a bug, so report it), then `DocSetModified`.
Prefer `match` or a read-then-index flow with expectDoc, because indices are positional.

### entity.get connections (read-only)
`count = *(int*)(node+HOFF_IONODE_CONN_COUNT)`; for `i < count` (cap it, e.g. 512),
`e = data + 80*i`: strings at +8/+0x10/+0x18/+0x20 (`char*`, may be NULL → ""), `targetType` +0x28,
`delay` +0x40, `timesToFire` +0x44. Emit the same names as the .vmap reader (`outputName, targetType,
targetName, inputName, overrideParam, delay, timesToFire`) so the live and file shapes match. Guard every
pointer with `mem_ok`.

---------------------------------------------------------------------------------------------------

## Q4 (compile): not done
Skipped (optional). The scratch map `mcp_test/maps/prop_probe.vmap` is now **saved** with two
`prop_dynamic`s (nodeIDs 108/109; 108 has a model, `targetname pp_dyn1`, and the two connections above),
the raw-created `pp_redo` crate and a modelless 109, so the orchestrator can compile it. [I] A
`prop_static` written through `SetKeyValue` is the exact state the asset-spray tool produces (0x180AF8310
places `prop_static` that way), so it should compile into the world like any hand-placed one.

---------------------------------------------------------------------------------------------------

## Sig-DB entries (ADDED to sigs/signatures/hammer.json in this worktree, status `proposed`)
Generated with `sigs/ida/ida_sigs.py` (`dump`) in the IDB. Fingerprints come from `sigs/ida/ida_fp.py`
(its `main()` run against the IDB, output in `target/orch/re/hammer-fp-new.json`). **Only the 10 new
keys** were merged into `sigs/fingerprints/hammer-fp.json`, and `HVS_MESH_SET_ROTATION` was added to
`_weak`: its prologue is identical to slot 17's within 256 B, the same blind spot the existing
`HVS_MESH_SET_ANGLES` has. ida_fp also re-computed different fingerprints for the existing
`GetRedoHistory`/`HFN_DOC_SET_MODIFIED`. Those were **left untouched**; that is pre-existing drift, not
mine. `python3 -I sigs/scripts/sigcheck.py validate` → `15 files, 4271 entries, 0 problems`.

| entry | locator | 2000930 |
|---|---|---|
| `HFN_SET_KEY_VALUE` | sig `40 55 56 57 48 83 EC 40 48 8B F9` | 0x83E450 |
| `HFN_ADD_CONNECTION` | sig `48 89 6C 24 18 57 41 56 41 57 48 83 EC 20 48 63 A9 A0 04 00 00` | 0x987220 |
| `HFN_REMOVE_CONNECTION` | sig `48 89 74 24 18 57 48 83 EC 20 48 8B F9 66 85 D2` | 0x987E70 |
| `HFN_KV3_INIT` | sig `40 53 48 83 EC 30 80 FA 06` | 0x13E9470 |
| `HFN_KV3_FREE` | base `HFN_REMOVE_CONNECTION` + `{"call": 0x51}` | 0x13E9520 |
| `HOFF_IONODE_CONN_COUNT` | sig `3B B1 A0 04 00 00 0F 8D ?? ?? ?? ??` disp 2 | 0x4A0 |
| `HOFF_IONODE_CONN_DATA` | sig `48 8B 99 A8 04 00 00 4C 89 74 24 38` disp 3 | 0x4A8 |
| `HOFF_NODE_SCALES` | sig `F2 0F 11 81 B8 00 00 00 8B 42 08` disp 4 | 0xB8 |
| `HVS_ENTITY_SET_ORIGIN` | vslot HV_MAPENTITY_VT 17 | 0x8959E0 |
| `HVS_ENTITY_SET_ANGLES` | vslot HV_MAPENTITY_VT 18 | 0x895A50 |
| `HVS_ENTITY_SET_SCALES` | vslot HV_MAPENTITY_VT 19 | 0x895B40 |
| `HVS_MESH_SET_ROTATION` | vslot HV_MAPMESH_VT 18 | 0x9A2540 |
| `HVS_MESH_SET_SCALES` | vslot HV_MAPMESH_VT 19 | 0x9A2690 |

Worker notes:
* `CMapNodeVtbl` in `s2sdk.h` needs slots 17/18/19 named **SetOrigin/SetAngles/SetScales**, with
  `S2_SLOT` asserts against the new macros. The old `SetAngles` member at 17 is the bug above.
* `HammerFns`: `SetKeyValue`, `AddConnection`, `RemoveConnection`, `Kv3Init`, `Kv3Free` with the
  signatures above. All are MS x64; none return anything the bridge needs except `AddConnection`'s outId
  and `RemoveConnection`'s u8.
* The other fields used (`HOFF_NODE_ORIGIN` 0xA0, `HOFF_NODE_ANGLES` 0xAC, `HOFF_NODE_UNDOABLE` 0xC8,
  `HFN_RECORD_NODE_TREE`, `HFN_GET_ENTITY_PROPERTIES`, `HFN_DOC_SET_MODIFIED`) are already pinned.
* `bridge/pins.py` / `bridge_pins.h` were NOT regenerated in the worktree (see the summary for why).

## Open risks
* `SetKeyValue` writes only String attributes. If a key exists as int/float/bool, the `SetValueString`
  inside it silently no-ops (the type gate). Keep the typed branch and call `OnKeyValueChanged` (vfn
  slot 267) after it, or refuse. [V bin]
* `SetKeyValue` dereferences the entity_properties element without a null check. Always call
  `GetEntityProperties(node)` first (it ensures or creates the element). [V bin]
* Case-insensitive key match in SetKeyValue vs the case-sensitive `FindAttribute` used by `previous`. [V bin]
* `RecordNodeTree` + `SetKeyValue` gives real value undo [V live]. The DME `CUndoAttributeSetValueElement`s
  that the setter may also push are not new; today's bridge has the same exposure.
* Not tested live: node.setTransform on a **mesh** (slot 17 origin through the mesh edit scope), and
  `scales` at create time. Both use the same slots as the verified entity path.
* The ModelHelper load is synchronous for cached models. A model that is not cached yet may draw one or
  more frames later [I]. Poll `view.render` CRC with a deadline in the tests.
* Live gotcha: stopping the container with unsaved edits leaves
  `content/_toolsautosave/hammer/.../<map>NNN.vmap_autosave`. The next `up` then shows a modal
  **"Recover Files?"** that blocks the main thread (every request answers `QueueTimeout`, modules
  `not_loaded`). Save or undo before `down`, or delete your own autosave file.

## Probe (how the [V live] evidence was produced)
(Local scratch of the RE session, not in git.) `target/orch/re/probe/` is a throwaway copy of `bridge/` plus a hidden `probe.props` method
(`target/orch/re/probe_fn.c`) that calls the RVAs above directly. It was built with `build.sh` against the
main checkout's `target/game-bins`, installed temporarily into the scratch tree, and the original
`mcp_bridge.dll` (sha256 b8b2420d…) was restored afterwards (backup:
`target/orch/re/mcp_bridge.dll.installed-backup`). Driver: `target/orch/re/rpc.py`. Renders:
`workshop/run/app/renders/pp_*.png`, `pq_*.png`. The worktree's `bridge/` sources are unchanged.
