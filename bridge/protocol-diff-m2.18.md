# Proposed protocol changes for bridge m2.18 (the prop pass: real keyvalue writes, node.setTransform, entity I/O)

Additive over m2.17; no major bump. Four changes, all of them Hammer-side writes or reads the prop
workflow needs:

1. **Keyvalue writes go through Hammer's own setter.** `entity.create`'s and
   `entity.setKeyValues`' String writes (and a key the entity does not have yet) call
   `CMapGameDataNode::SetKeyValue` (`HFN_SET_KEY_VALUE`, `hammer!0x18083E450`) instead of the raw
   `CDmAttribute::SetValueString`, and the node tree is recorded once per node so the undo group
   really reverts the values (`valueUndo` becomes `"recorded"`). Typed (int/float/bool) attributes
   keep the typed setters; a key whose only match is a different spelling is refused.
2. **`entity.create` takes an optional `scales`**, written through the entity `SetScales` vslot in
   the same group as the creation.
3. **`node.setTransform {nodeID, origin?, angles?, scales?, expectDoc}`** — the first transform
   write: one group, one RecordNodeTree, then the scale/angle/origin setters (vtable slots 19/18/17).
4. **Entity I/O**: `entity.get` reports the live connection vector instead of a hard-coded `null`,
   and two new methods write it: `entity.addConnections` and `entity.removeConnections`.

Sources read for this pass: `target/re/re-props.md` (the RE report, every claim below is tagged
with its tag) and `target/re/probe_fn.c` (the throwaway probe whose calls are the live-verified call
shape; production code uses the pins, the gates and the validation, never `hb + 0x...`), plus
`bridge/mcp_bridge.c`, `bridge/s2sdk.h` and `sigs/signatures/hammer.json`.

**Status.** The RE facts this pass builds on are `[V live]`/`[V bin]` (a probe bridge on hammer
build 2000930: a `prop_dynamic`'s model drawn at once through SetKeyValue, value + drawn-model undo
with RecordNodeTree, the transform setters moving/rotating/scaling a node, the I/O vector's add /
remove / undo / save round trip). The methods and the reply shapes below are **[I: offline]**:
`./bridge/build.sh` (host unit tests, pins, sdkcheck, the DLL link), `python3 -I
bridge/test_entities.py --static` and `python3 -I bridge/test_props.py --static` are green.
**Live is the orchestrator's run** (`bridge/test_props.py`) - it is what promotes the new rows to
`[V runtime]`.

## §7 method table - new and changed rows

| Method | Status | Bridge m2.18 | Params | Result | Call | Notes |
|---|---|---|---|---|---|---|
| `node.setTransform` | **new `[I: offline]`** | `NEED_HAMMER`; refusals in order `-32003 NoActiveDocument` -> `-32018 DocumentNotEditable` -> `-32004 DocumentChanged` | `{nodeID: int 0..2147483647, origin?: [x,y,z], angles?: [p,y,r], scales?: [x,y,z], expectDoc}`; at least one of the three | `{nodeID, class:"entity"\|"mesh", origin:[x,y,z], angles:[p,y,r], scales:[x,y,z], doc:{id,topSerial,undoDepth}}` | `AddOperation("MCP: set transform")` -> `RecordNodeTree(hist, node->undoable, 1)` -> slot 19 `SetScales` -> 18 `SetAngles` -> 17 `SetOrigin` (`HV_MAPENTITY_VT` / `HV_MAPMESH_VT`, the same slot numbers) -> `DocSetModified` | see below |
| `entity.addConnections` | **new `[I: offline]`** | `NEED_HAMMER`; the same gate order | `{nodeID, connections: [1..64 x {output, target, input, param?, delay?, timesToFire?}], expectDoc}` | `{nodeID, added:[{index, outputName, targetType, targetName, inputName, overrideParam, delay, timesToFire}], count, landed:[{index, landed}], undoLabel:"MCP: add connections", doc:{...}}` | `AddOperation("MCP: add connections")` -> `RecordNodeTree` -> per entry the 80-byte src (targetType 7, `Kv3Init(src+0x30,1,8)`) -> `AddConnection` -> `Kv3Free` -> `DocSetModified` | see below (`index`, the count check, the cap, `landed`) |
| `entity.removeConnections` | **new `[I: offline]`** | `NEED_HAMMER`; the same gate order | `{nodeID, indices?: [1..512 distinct ints], match?: {output?, target?, input?}, expectDoc}`; exactly one of `indices`/`match` | `{nodeID, removed:[ascending indices], count (the number REMOVED), remaining (the vector size after the group), undoLabel:"MCP: remove connections", doc:{...}}` | resolve the index set against the live vector **before** `AddOperation("MCP: remove connections")` -> `RecordNodeTree` -> `RemoveConnection(node, idx << 16)` per index in **descending** order -> `DocSetModified` | a 0 return after the group is open is `-32603` with `data.maybeExecuted:true` |
| `entity.create` | unchanged `[V]`, + optional `scales`, + `nodeID` | unchanged | `{class, origin?, angles?, **scales?**, keyValues?, expectDoc}` | `{handle, **nodeID** (the created node's id, absent when unreadable), class, origin, **scales**, undoLabel, keyValues?, doc:{...}}` (the `scales` echo is always present; `[1,1,1]` when the param was absent) | `CreateNodeByName` as before, then slot 19 `SetScales` in the same group (no extra RecordNodeTree: the creation record covers it) | `scales` is validated before `CreateNodeByName`/the undo group (`-32602`) |
| `entity.setKeyValues` | unchanged `[V]`, the **String writes change** | unchanged | unchanged | unchanged shape; `valueUndo` is now **`"recorded"`** (was `"group-only"`) | the String/missing-key branch is `SetKeyValue(node, key, text)`; `RecordNodeTree(hist, node->undoable, 1)` once per node after `undo_open` and before the first write (skipped for a node the same `edit.batch` created) | see below |
| `entity.get` | unchanged `[V]`, `connections` becomes the live vector and `scales` is added | unchanged | unchanged | `connections` is now `[{outputName, targetType, targetName, inputName, overrideParam, delay, timesToFire}, ...]` for a `CMapEntity` (it was hard-coded `null`); **new `scales:[x,y,z]`** read from the node field (with `origin`/`angles`, which were already there); new `connectionsTruncated:true` + `connectionsTotal` when the vector holds more than 512 entries | reads `node+HOFF_IONODE_CONN_COUNT` (0x4A0) / `HOFF_IONODE_CONN_DATA` (0x4A8), 80-byte entries, at most 512 listed, every pointer `mem_ok`-guarded | `[]` for an entity with none, `null` for a non-entity or an unreadable vector (a count outside `0..65535`). A connection string that did not fit carries `"truncated":true` on its entry. `entity.find`'s per-entity bodies do **not** carry any of it (they never had a connections key) |

Everything else (the mesh/node rows, `edit.batch`'s whitelist `entity.create` + `entity.setKeyValues`,
the `doc` tail of m2.17, `undo.undo`/`undo.redo`) is unchanged.

### `node.setTransform` - the validation and why these numbers

- **Before the undo group opens** (the review-13 rule: a refused call must leave no empty group):
  - `nodeID` present, an integer in `0..2147483647` (`-32602`) - the nodeID aliasing rule;
  - at least one of `origin`/`angles`/`scales` present and well-formed (`-32602`); a present but
    malformed array is `-32602`, never silently skipped;
  - `origin`: every component finite and `|c| <= 1048576` (2^20; live 2026-10-16: probe.vmap entities sit at x~71000, so the first 65536 bound refused real moves);
  - `angles`: finite (any angle is legal - Hammer normalizes);
  - `scales`: each finite and in `(0, 1000]` - 0, negative and NaN are refused (`-32602`), the same
    bound `entity.create` now applies.
- Then the node must exist (`-32022 NodeNotFound`), be a `CMapEntity` or `CMapMesh` (`-32602` for
  anything else: world/group/prefab/instance) and not be locked (`-32018`).
- **The calls.** The transform values are C++ fields of `CMapNode`, written only through the
  vtable setters: slot 17 `SetOrigin`, 18 `SetAngles`, 19 `SetScales` (re-props.md Q2's table; the
  mesh and the prefab/group override the same slots). Each setter stores its field, forwards to the
  node's helpers and invalidates the transform and bounds, so **no extra bounds/view refresh call is
  needed** (`[V live]`: the chair moved, rotated and scaled at once). The entity overrides also move
  attached children. The bridge calls scales -> angles -> origin (`[V bin]`: Hammer's own "Clear
  Rotation and Scale" order).
- **The reply is read back from the node**, not echoed from the params: `origin`/`angles`/`scales`
  come out of `HOFF_NODE_ORIGIN`/`ANGLES`/`SCALES` (a field that cannot be read answers `null`),
  `class` is `node_class` ("entity"/"mesh"), and the m2.17 `doc` tail is appended by `geo_dispatch`
  like every other mutating geo op.
- **Slot 17 is `SetOrigin`, not `SetAngles`.** re-props.md Q2 proves it (the base store of slot 17
  writes origin +0xA0, which `CreateNodeByName`/`SetPositionAndAngles` fill from the *origin*
  argument; the mesh's IDA name and the reused `CMeshEditScope` label are misleading).
  `bridge/s2sdk.h`'s `CMapNodeVtbl` now names 17/18/19 `SetOrigin`/`SetAngles`/`SetScales` with
  `S2_SLOT` asserts against `HVS_ENTITY_SET_ORIGIN`/`ANGLES`/`SCALES` and two `_Static_assert`s that
  the mesh pins are the same cells (`HVS_MESH_SET_ROTATION == HVS_ENTITY_SET_ANGLES`,
  `HVS_MESH_SET_SCALES == HVS_ENTITY_SET_SCALES`). `HVS_MESH_SET_ANGLES` keeps its sig entry
  (never rename an entry) and is retired: no bridge code reads that macro any more.

### `geo_reset_mesh`: the slot-17 bug this pass fixes

`mesh.setGeometry` first empties the mesh and identity-transforms it so `InitializeFromPolygonGroup`
can take world points. It did that by calling the member that used to be named `SetAngles` - i.e.
slot 17, **`SetOrigin`** - so a rotated mesh lost its **origin** and kept its rotation, and the new
geometry (built from world points) was placed relative to a moved pivot. It now calls slot 18
(`SetAngles`, the corrected member) with zeros **and** resets the scales through slot 19 with
`{1,1,1}`, in Hammer's own "Clear Rotation and Scale" order (scales, then angles): a mesh scaled by
`node.setTransform` would otherwise build its new world-point geometry under a 2x node scale.
`[I: offline]`; the mesh transform path itself is `[V live]` only for entities (re-props.md "Open
risks": a mesh transform was not tested live).

### `entity.setKeyValues` / `entity.create`: why `SetKeyValue` and what changed in the reply

- **Why.** `[V bin]` `CDmAttribute::SetValueString` (what `apply_keyvalues` used) ends in the
  DataModel's notify path, which reaches only the *owning element's* vtable slot 0 - and
  `entity_properties`' slot 0 is a guard-check stub - so the entity helpers never see the write.
  `[V live]` the result was the white placeholder box for every raw-written model.
  `CMapGameDataNode::SetKeyValue` writes the attribute **and** calls the node's `OnKeyValueChanged`
  handler when the value changed, where `CMapEntity`'s rebuilds the helpers and updates the
  `CModelHelper` (loads and draws the model, updates the bounds; it also handles
  rendercolor/renderamt/disableshadows/solid/visoccluder). `[V live]` the chair draws at once
  through the probe.
- **The call.** `void SetKeyValue(node, const char *key, const char *value)` (`HFN_SET_KEY_VALUE`);
  `value` is a plain C string it interns itself (the old `dm_intern` + null-symbol
  `SetValueString` dance is gone with the raw setter). It requires the `entity_properties` element:
  the bridge calls `GetEntityProperties(node)` first and refuses the op when it is null (it derefs
  the element unguarded).
- **Typed attributes stay on the typed setters.** `SetKeyValue` writes a String attribute; a key
  that exists as int/float/bool is not in its path, and `apply_keyvalues` keeps `SetValueInt` /
  `SetValueFloat` for those (a `SetKeyValue` call there would silently store nothing - the type
  gate). A key the entity does not have is created by `SetKeyValue` itself, always as a String
  (`S2_AT_STRING`, the §3.4 coercion: never int/float/bool from the JSON kind).
- **Case variants are refused.** `SetKeyValue` matches an existing key **case-insensitively**,
  while the `previous` pass (and `FindAttribute`) is exact. A key whose only match is a different
  spelling would be written by `SetKeyValue` while `previous` reported `null` for it, so the bridge
  refuses that key: `applied:false` plus a `warnings` entry naming the existing spelling. Nothing
  is written for it and the keys after it still parse.
- **Real value undo.** `RecordNodeTree(hist, node->undoable, 1)` once per node, after `undo_open`
  and before the first write - what every Hammer edit does (`[V live]`: `undo.undo` reverts the
  model value *and* the drawn model; without it the group popped but the value stayed, the m2.15
  gap). A node created earlier in the same `edit.batch` is skipped (its `CreateNodeByName` already
  recorded it in this group, re-props.md Q1). `entity.create` needs no extra record for the same
  reason (the keyvalue writes survive undo -> redo, `[V live]`).
  - **Once per node per group, tracked by the node itself.** The skip used to be keyed on the
    batch's 16-entry handle table, so a 17th create in one batch or *two* `setKeyValues` ops on the
    same existing node recorded that node twice in one group; whether Hammer's second record is a
    no-op or a snapshot the undo replays in the wrong order is not known from the RE (W2 review L7),
    and Hammer's own paths record each node once. The batch now keeps a small `recorded[]` list of
    node ids (a created node's id comes from its create reply, a written node's from its handle or
    `nodeID`), and a node that is already in the group is not recorded again.
- **`entity.create`'s reply carries the created node's `nodeID`** (`node_id_of`, the same read
  `node.list`/`entity.get` address nodes with). Before this, the reply carried only a `handle` (batch
  scope) so a client had to `entity.find` by a run-unique keyvalue to learn the id; the field is
  additive and simply absent when the id cannot be read.
- **`valueUndo:"recorded"`.** The field name is unchanged (an additive value change): an older
  reader that treated `"group-only"` as "undo does not revert the values" must now expect the
  values to come back. The bridge no longer has a "group-only" value anywhere.
- **`entity.create {scales}`.** `CreateNodeByName` takes only origin/angles, so a create with
  scales calls the entity `SetScales` vslot (slot 19) right after it, in the same group; the reply
  echoes `scales` (`[1,1,1]` when the param was absent). Not tested live as a *create* param, but
  it is the same slot that was verified live on an existing node, and the same pattern the entity
  branch uses (re-props.md Q2). `entity.get` now reports the same field, so a client can read the
  stored scales back instead of trusting the echo.
- **Two `SetKeyValue` behaviours the bridge cannot change and does document** (offline review):
  - it early-outs when the new value equals the old one **case-insensitively**, so a value change
    that only differs in case writes the attribute but skips `OnKeyValueChanged`. That also means
    a node whose model was raw-written by an m2.15-m2.17 bridge keeps its placeholder box if the
    *same* model is set again (the probe's `[V live]` finding: it has to be set to `""` first, then
    to the new model);
  - int/float/bool attributes keep the typed setters (this pass's scope is the String path), so a
    typed `rendercolor`/`solid` write does not refresh the helpers either - the same gap re-props.md
    Q1's "open risks" names (calling vfn 267 `OnKeyValueChanged` after a typed write would close it
    and is a later pass's job).

### Entity I/O: what is in memory (and why the DME array is not touched)

- `[V bin + V live]` the live connections are a C++ vector on `CMapEntityIONode`:
  `count = *(int *)(node + 0x4A0)`, `data = *(u8 **)(node + 0x4A8)`, 80-byte elements. The layout
  (`s2sdk.h`'s `(b) S2ConnectionSrc`, `_Static_assert`s on every offset and on `sizeof == 80`):
  `source` 0x00 (a load-filled `targetname`, no saved attribute), `outputName` 0x08, `targetName`
  0x10, `inputName` 0x18, `overrideParam` 0x20, `targetType` 0x28, `paramMap` (KeyValues3) 0x30,
  `delay` 0x40, `timesToFire` 0x44, `serial` 0x4C.
- `connectionsData` (the DME element array Hammer creates in `CMapEntityIONode::OnConstruction`) is
  **only materialized at save and consumed at load** - writing it live is pointless (the next save
  rebuilds it from the vector) and the bridge does not.
- `[V live]` the round trip: two connections added by the probe appear in the vector, `map.save`
  writes exactly those two to the file with `targetType 7`, a removal leaves the other one,
  `undo.undo` restores it, and after `map.reload` the vector is back with `source` filled from the
  owner's targetname.
- **`entity.get`.** The reply's `connections` is the live vector with the .vmap reader's field
  names (`outputName/targetType/targetName/inputName/overrideParam/delay/timesToFire`), so the live
  shape and the saved file compare field by field. A string that is null or unreadable is `""`;
  the walk stops at the first unreadable element instead of guessing.
- **`entity.addConnections`.** `AddConnection(node, &id, &src)` (`HFN_ADD_CONNECTION`,
  `hammer!0x180987220`) copies the 80-byte source into the vector (five `strlen` + `CUtlString`
  copies and a KV3 copy) and sets `*outId = index << 16`. The source must carry an **initialized**
  KeyValues3 at `src+0x30` because the copy reads it: the bridge calls `Kv3Init(src+0x30, 1, 8)`
  (`HFN_KV3_INIT`, the exact "Add New Output" call) before and `Kv3Free` (`HFN_KV3_FREE`) after -
  the same pair the probe used. The five string pointers are only read during the call (Hammer
  copies them into its own `CUtlString`s), so bridge-owned buffers are fine; the bridge never calls
  a `CUtlString` destructor on them. `source` (+0x00) stays null: it is not a saved attribute and
  a load fills it, so nothing a caller could verify rides on it. `targetType` is 7 (the UI default
  and every saved Valve connection; the DME constructor's default 2 is not what a save produces).
  - **The read-back is cross-checked, not trusted.** `*outId` must be `index << 16` (low word 0)
    *and* the vector count must have grown by exactly one *and* the index must be the previous
    count - so a wrong call shape cannot make the bridge report a different entry as the added one.
    An entry the vector did not confirm is `{"readback":false}` (with the index when the call
    reported one); `count` is `len(added)`, the entries the vector confirmed, never the calls
    attempted. A `readback:false` entry means the add is **maybe-executed**: read `entity.get`
    instead of retrying it.
  - **`landed` is the per-wire-entry outcome** (added after the W2 review found that a
    `readback:false` entry could hide behind a bare `count`): `landed[i]` belongs to
    `connections[i]` of the request - the same length, the same order - and is
    `{index: <the live index it landed on, or null when the call reported none>, landed: true|false}`.
    A confirmed entry's index equals its `added[]` index; a `false` entry is the bridge's
    maybe-executed case (nothing else in the reply distinguishes it from a confirmed one beyond
    that field, and `added` has one fewer element).
  - **The cap.** The call is refused with `-32602` *before* any write when the entity would hold
    more than 512 live connections (the count `entity.get` lists and `removeConnections` can
    address in one call). The bridge can therefore never build a vector it could not read back or
    repair. The reply buffer is sized for the worst case (64 entries of four 255-byte, fully
    escaped names), so a large call can no longer answer `-32603` *after* the connections were
    added.
- **`entity.removeConnections`.** `RemoveConnection(node, id)` (`HFN_REMOVE_CONNECTION`,
  `hammer!0x180987E70`) returns 0 unless `(id & 0xFFFF) == 0` and `id >> 16 < count`, then frees the
  entry and **shifts the ones after it** - hence the descending walk, and hence the rule that the
  index set is resolved against the live vector *before* the group opens (a stale index in a
  second call is a `-32602`, not a removal of the wrong connection).
  - **The counts.** `count` is the number of connections **removed** by this call (it has always
    been, the name is misleading) and is kept for compatibility; `remaining` (added after the W2
    review, which found the Rust tool reading `count` as the remainder) is the vector's size after
    the group - the count read before the group minus the removals. A client that wants "how many
    are left" reads `remaining`; nothing changes for a reader that used `count` as the removed set's
    size (`len(removed) == count` always holds).
  - **The `match` filter** takes `output`/`target`/`input` (at least one, 1..255 bytes each, no
    control characters) and is parsed with the same escape-aware reader the connections array uses
    (`kv_next`): a value is decoded (`\n`, `\uXXXX`) and refused when it does not fit, so the
    filter is the caller's exact bytes and a cut filter can never select a prefix. The comparison
    is exact and case-sensitive; a live field that did not copy whole never matches. Zero hits is
    `-32026`; more than 512 hits is `-32602` (nothing opened in either case).
  - A removal that fails with the group already open is `-32603` with `data.maybeExecuted:true`
    (the earlier removals in the group ran; verify with `entity.get`, do not retry).

## §8 error codes

**New: `-32026 ConnectionNotFound`** — `entity.removeConnections`' `match` filter matched no live
connection; nothing was opened, nothing ran.

Reused: `-32602 InvalidParams` (every param rule above, an out-of-range or duplicate index, a
non-entity node for the I/O ops, a non-entity/mesh node for `node.setTransform`), `-32022
NodeNotFound`, `-32018 DocumentNotEditable` (the document gate and a locked node), `-32004
DocumentChanged`, `-32003 NoActiveDocument`, `-32603 InternalError` (the connection vector
unreadable before the group; `RemoveConnection` returning 0 **after** it, with
`data.maybeExecuted:true`). Nothing is retired.

## §5/§6 notes

- Method count: **82 registered / 67 implemented** (m2.17: 79/64; the three new rows).
  `control.version.bridgeVersion` and every `"build"` field are **`m2.18`**.
- **No new pin entries were needed** for this pass: `sigs/signatures/hammer.json` already carries
  `HFN_SET_KEY_VALUE`, `HFN_ADD_CONNECTION`, `HFN_REMOVE_CONNECTION`, `HFN_KV3_INIT`,
  `HFN_KV3_FREE`, `HOFF_IONODE_CONN_COUNT`, `HOFF_IONODE_CONN_DATA`, `HOFF_NODE_SCALES`,
  `HVS_ENTITY_SET_ORIGIN/ANGLES/SCALES`, `HVS_MESH_SET_ROTATION` and `HVS_MESH_SET_SCALES` (status
  `proposed`, from the RE pass), and `bridge/bridge_pins.h` was regenerated from them on this
  branch's base (302 macros). `bridge/s2sdk.h` gains the `HammerFns` entries and the `(b)`
  `S2ConnectionSrc` layout; `PIN_MODULE_COUNT` stays 12.
- The diff against the base branch still introduces **no `0x` literal** in
  `bridge/mcp_bridge.c` (`bridge/test_entities.py --static`, `static_no_new_hex_literals`): every
  number is a `bridge_pins.h` macro, and the two literals the recipe needs are decimal
  (`targetType 7`, the `4294967295u` out-id seed).
- Version bump outside the new code: `bridge/test_entities.py`, `test_modeldoc.py`,
  `test_modeldoc_render_live.py`, `test_pet.py` and `test_pet_live.py` assert the bridge build and
  are bumped to `m2.18` (the same set m2.17 bumped). `test_entities.py --static` also asserts the
  new source contract (`static_props_pass`) and its m2.15 sections moved with the behaviour they
  pin: the String coercion is now the `SetKeyValue` write, `valueUndo` is `"recorded"`, and
  `entity.get`'s `connections` is a live list.
- **The Rust side must accept `valueUndo:"recorded"`**: `hammer-mcp/src/tools/entities_live.rs`
  (its doc comment and the `revertHint` it derives from `"group-only"`) and
  `crates/cs2-mcp-common/src/mock/hammer.rs` (the mock reply) carry the old literal. That is a `hammer-mcp`
  change, not this bridge pass; until it lands, the tool's `previous`-based revert hint is stale but
  harmless (the value is already reverted by the group).
- Host unit tests (this pass): `bridge/test_entity_json.c` gained 58 cases over the lifted region
  (`xform_cases`, `conn_cases`, `rm_cases`) - 113 cases total, all negative paths included (0 /
  negative / NaN / over-max scales, over-bound and non-finite origins, empty and 65-entry
  connection arrays, unknown members, wrong value kinds, control characters, 255 vs 256-byte
  strings, timesToFire 0/-2/1.5, duplicate/negative/fractional/exponent/over-i32 indices, missing
  commas, the 512/513 index cap, and the descending-order helper).
- New live test: `bridge/test_props.py` (run it against a tools session with `probe.vmap` open; its
  `--static` mode is the offline half of the same contract). It follows the map-editing
  conventions: run-unique targetnames and a coordinate band of its own (band 3), cleanup pops only
  its own groups and saves again.

## Live probe list for the orchestrator (`bridge/test_props.py` covers all of it)

1. `control.version` -> `m2.18`; `control.capabilities` advertises `node.setTransform`,
   `entity.addConnections`, `entity.removeConnections`.
2. The model draw: `entity.create prop_dynamic` (no model) -> `view.render` crc0 -> `setKeyValues
   {model}` -> the crc changes (polled, the ModelHelper may draw a frame later); a second model
   changes it again; `undo.undo` restores the value **and** the crc.
3. `node.setTransform` on an entity: the reply reads all three fields back, `entity.get` agrees,
   the render changes, `undo.undo` restores origin/angles (and the scale value, read back through
   the next call's reply).
4. The I/O round trip: two `addConnections` -> `entity.get` lists exactly those two with
   `targetType 7`; `removeConnections {match}` leaves the other one; `undo.undo` restores both.
5. Every refusal in the test's list (-32602 set, -32026) and the undo depth unchanged after them.
6. **Not covered by the script** (the orchestrator may add them): `node.setTransform` on a **mesh**
   (re-props.md "Open risks": the mesh override wraps the store in a `CMeshEditScope`), the same on
   a group/prefab node, and `entity.create {scales}` on a *mesh* class. Their offline paths are the
   same slots the script verifies on an entity.

## Review fixes (offline, before the live run)

An independent offline review of the diff (a read-only pass, no code changed by the reviewer)
found five things worth fixing; all of them are in, one host-test case earlier had already caught a
sixth:

1. **A successful add could answer an error.** `entity.addConnections`' reply buffer was 16 KB and
   the overflow check runs in `geo_dispatch` *after* the writes, so 14+ maximum-length entries
   would have answered `-32603` with the connections already added (a client that retried would
   duplicate them). The buffer is now sized for the worst case (64 entries x four fully escaped
   255-byte names, ~148 KB).
2. **The 512 cap was treated as "unreadable" and the bridge could cross it.** `conn_vec` now
   distinguishes the *sanity* bound (`0..65535`, the id space `index << 16` can address) from the
   *output* bound (512 entries listed, with `connectionsTruncated`/`connectionsTotal` when there
   are more), `entity.addConnections` refuses an add that would cross 512 before it opens a group,
   and a `match` with more than 512 hits is `-32602` (nothing opened).
3. **`geo_reset_mesh` did not reset the scales** - reachable now that `node.setTransform` can scale
   a mesh, and it would have built the new world-point geometry under a non-unit scale. It resets
   scales to `{1,1,1}` through slot 19, in Hammer's own order.
4. **The case-variant refusal only covered the missing-key branch.** `SetKeyValue` writes the
   *first* case-insensitive match, so on an entity that holds two spellings of a key (only an older
   bridge could have made that) it could write the other one while `previous` reports this one. The
   check now runs on the String branch too, and the warning names the spelling the entity really
   has (so the caller can re-send the key as stored).
5. **The `match` filter could be cut or mis-decoded.** It went through `json_string_top` (no
   escape decoding, silent cut at 255). It is parsed with the same escape-aware `kv_next` the
   connections array uses, a value that does not fit is `-32602`, and a live field that did not
   copy whole never matches (so a cut filter can never select a prefix).
6. **(host-test catch, first build)** `conn_array_parse` looked at the object's `}` before it
   dispatched the member, so the last member of every connection object was silently dropped;
   the order is now dispatch-then-separator and the new host cases pin it.
7. Also from the review: the add read-back is cross-checked against the vector count (item above),
   a connection string that did not fit is flagged per entry (`"truncated":true`) instead of being
   emitted as a silent prefix, `entity.get` reports `scales`, `node.setTransform` carries an
   `undoLabel` like the other writes, and a `readback:false` add no longer reports the `0xFFFFFFFF`
   sentinel as `index: 65535` (it reports no index unless the call wrote one).

## Review-2 (W2) fixes - the reply-contract corrections

A second review over the m2.18 bridge **and** the Rust/mock side (read-only, no live run) found the
reply contract mismatched between the three: the bridge answered one thing, the mock another, and
the Rust tool a third. Fixed on the bridge here (`hammer-mcp`'s side and the mock carry their own
notes):

1. **`entity.removeConnections` now answers an explicit `remaining`.** The reply's `count` is - and
   always was - the number of connections **removed**, while a client read it as "how many are
   left" (a 5-connection entity that lost 3 was reported as holding 3). `remaining` is the vector's
   size after the group: `count - n`, with `count` read *before* the undo group opened. `count`
   stays for compatibility; `len(removed) == count` always holds.
2. **`entity.addConnections` carries the per-entry outcome twice over.** `added[i]` keeps the
   bridge's own field set (`index, outputName, targetType, targetName, inputName, overrideParam,
   delay, timesToFire` - `sb_add_conn_fields`) and `count` is `len(added)`; the new positional
   **`landed[]`** array (`{index, landed}`, one per *wire* entry, so `landed[i]` belongs to
   `connections[i]` of the request) makes a maybe-executed entry attributable even when `added`
   dropped it. The Rust tool sets `outcomeUnknown:true` and lists the wire positions under
   `unconfirmed` when any entry is `landed:false`.
3. **`entity.create` reports the created node's `nodeID`** (`node_id_of`, the id `node.list` and
   `entity.get` address nodes by), in the standalone reply and in an `edit.batch` result entry. The
   field is absent when the id cannot be read; older readers ignore it.
4. **The origin refusal names the gate's own bound.** `node.setTransform`'s text said
   `<= 65536` while `XFORM_ORIGIN_MAX` had moved to 1048576, so a client "correcting" its value
   from the message stayed wrong. The text is now built from the macro (`xform_origin_reason`,
   pinned by a host case and read out of the live error's `data.reason` by `test_props.py`).
5. **`edit.batch` records each node once per group.** The created-node skip was keyed on the
   16-entry handle table; a 17th create or a second `setKeyValues` on the same existing node called
   `RecordNodeTree` twice in one group. The batch keeps a small `recorded[]` node-id table and asks
   **`batch_record_decision`** (host-tested: first write records, second write on the same node does
   not, another node does, `-1` never does, past the 32-entry cap it records but cannot remember) -
   fed by each create reply's `nodeID` and by each successful keyvalue write.

Not changed (deliberate): `entity.addConnections`' `-32603 maybeExecuted` and `-32026` stay on
their codes (the WP asks for `-32012`-style "verify, do not retry" *data* on the InternalError;
`-32026` is a new code the Rust side maps to `None`, which is safe), and the lenient array commas
(`[a,,b]`) stay lenient.

## Offline proof

`./bridge/build.sh` is green: `sigcheck.py validate` (0 problems), `sigcheck.py resolve --strict`,
`pins.py` (the committed `bridge_pins.h` matches the regenerated one, 302 macros), `gen.py --check`
(both ABIs), `sdkcheck.py` (16 PASS / 6 SKIP / 0 FAIL), the host unit tests
(`test_entity_json` 113 cases on the first m2.18 build, 125 after the W2 review round - 0 failures
both times) and the DLL link - with no compiler warning.
`python3 -I bridge/test_entities.py --static` is **PASS** (with the new `static_props_pass` section
and the updated m2.15 expectations) and `python3 -I bridge/test_props.py --static` is **PASS** (its
`static_props_contract` grew the W2 rows: `remaining`, `landed`, `xform_origin_reason`, the create
`nodeID`, `batch_node_recorded`). The offline review rounds above were applied
before this proof run (the fixes 1-7 and the W2 corrections are part of the build the proof
describes).

**Negative evidence** - one revert at a time, the tests unchanged:

- the three rows back to `impl 0`: `static_props_pass` fails (the rows), `test_props.py --static`
  fails (the rows and their dispatch arms);
- `apply_keyvalues` back to `SetValueString`: `static_string_coercion` and the host test's
  coercion section fail, and `test_props.py`'s model-draw checks are the live half of the same
  claim (the model would stay a box);
- `valueUndo` back to `"group-only"`: `static_previous_and_valueundo` fails;
- `geo_reset_mesh` back to the old member (slot 17), or without the scales reset: `static_props_pass`
  fails on the slot-18 fix (and the scale reset is named in the same check);
- removing the `}`-before-dispatch order fix in `conn_array_parse`: the host test's
  `conn_parse_*` cases for the last member fail (this is the bug the first build caught: the last
  member of an object was silently dropped);
- (W2) `xform_origin_reason` back to the literal `<= 65536`: the host test's
  `xform_origin_text_names_the_bound` and `xform_origin_text_has_no_old_bound` fail (run:
  `target/negative.sh` step 1) and `test_props.py --static` fails on the W2 row; the live
  `refuse_origin_text_names_the_bound` step reads the same text out of `data.reason`;
- (W2) `remaining` dropped from the remove reply: `static_props_contract` fails, and the Rust
  `remove_reports_the_remaining_count_not_the_removed_one` fails too (the tool then reports `null`
  + the note, never a wrong number) - `target/negative.sh` step 2 and `target/negative-rust.sh`
  step C;
- (W2) `batch_record_decision` back to "always record" (never asks `batch_node_recorded`):
  `batch_record_decision_second_skips` and `batch_record_decision_cap` fail
  (`target/negative.sh` step 3), and `static_props_contract` fails (the call site row);
- (W2) the Rust `unconfirmed_entries` back to the old pass-through: the tool test
  `a_maybe_executed_add_is_reported_not_passed_through` fails; the assumed `added` fallback back to
  `output/target/input/param`: the unit test `connections_arg_validates_every_member` fails;
  the mock's `entity.get` without `scales`: `relative_scales_use_a_read_that_reports_them` and the
  `list_props` live tests fail; the mock's create reply without `nodeID`:
  `create_entities_batches_creates_and_never_returns_a_handle` and
  `a_partial_failure_passes_through_with_failed_index_and_bumps_the_counter` fail
  (`target/negative-rust.sh`, steps A-E).

## Applying this in idabins (not done here)

`../../idabins/docs/data/bridge-protocol.md` §7 needs: the three new rows with their params,
results and call chains, the `scales` param and the `scales` echo on `entity.create`, the
`entity.get` `connections` shape, the `valueUndo:"recorded"` note on `entity.setKeyValues`, the
method count 82/67 and the `m2.18` build string; §8 needs the new `-32026 ConnectionNotFound` row.
`../idabins/docs/interfaces/` would want the `CMapEntityIONode` connection record and the
`CMapNode` transform setters (slot 17 = `SetOrigin`) - the RE report (`target/re/re-props.md`) is
the source for both. The docs live in the `idabins` checkout, which this pass must not touch
(`AGENTS.md`: work in the assigned worktree).
