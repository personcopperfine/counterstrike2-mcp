# Proposed protocol changes for bridge m2.19 (the trigger pass: tie/untie, entity children)

Additive over m2.18; no major bump. Three changes, all of them the brush-entity (trigger)
writes/reads the Hammer trigger workflow needs:

1. **`entity.tieMeshes`** — the Hammer-faithful "Tie To Entity" as a bridge primitive: turn
   world meshes into a new brush entity, or add volumes to an existing one. One undo group.
2. **`entity.untie`** — Hammer's own `CMapDoc::MoveToWorld` ("Untie From Entity") with a
   1-entry node vector; it opens its own undo group, so the bridge opens none.
3. **`entity.get` gains `children`** — one level, the read the trigger tools need
   (`children:[{nodeID, class}]`, capped at 1024 with `childrenTruncated`/`childrenTotal`,
   `null` when the child array is unreadable).

Sources read for this pass: `target/re-triggers.md` (the RE report, every recipe claim `[V live]`
2026-10-16 via a probe DLL) and `target/plan-triggers.md` §5 (the FINAL wire contract the Rust
tools and the mock are written against), plus `bridge/mcp_bridge.c`, `bridge/s2sdk.h` and
`sigs/signatures/hammer.json`. The recipe's C sketch is `re-triggers.md` Q1; the production code
uses the pins, the gates and the validation order, never a literal address.

**Status.** The RE facts are `[V live]`/`[V bin]` (build 2000930: tie/untie/retie with undo and
redo, the origin rule, the emptied-entity rule, gdType 5 ⇔ @SolidClass, world geometry kept).
The methods and reply shapes below are **[I: offline]**: `./bridge/build.sh` is green (host unit
tests 125 cases, pins, sdkcheck 16 PASS / 0 FAIL, the DLL link, no compiler warnings),
`python3 -I bridge/test_triggers.py --static` is green. **Live is the orchestrator's run**
(`bridge/test_triggers.py`) - it is what promotes the new rows to `[V runtime]`.

**FIX-B (review-b + the first live run).** The orchestrator's live run (`target/test_triggers-1.log`)
worked; the FAIL rows were the all-`alreadyTied` no-op opening an empty undo group (L1) and the
test's running-counter baselines cascading from it (L2). FIX-B (this diff, branch
`triggers/fix-b`) fixes that and every review-b MED, and the changes below are already written
into the sections that follow: the all-`alreadyTied` request is a **success no-op without a
group**; the unknown-class refusal is **`-32006 UnknownClass`** (was `-32602`); both in-group
failures carry **`undoGroupOpened`/`undoLabel`** (no `maybeExecuted`); the **`-32602`
keyValues prescan** refuses `classname`/`origin`/`angles` keys and malformed objects before the
group; the **prefab/instance gate** refuses meshes (and an `entityID` target) whose ancestor
chain leaves the world/groups/entities; a **retie checks the old parent's lock**; the
AddChild-refusal error carries **`maybeExecuted` + progress lists** and calls `DocSetModified`;
`entityID`/child ids are never emitted as `-1`; `entity.untie` reports `meshesTruncated` past
1024 children instead of refusing. **A live re-run of `test_triggers.py` is still the
orchestrator's step** - until then every row here is `[I: offline]` again.

## §7 method table - new and changed rows

| Method | Status | Bridge m2.19 | Params | Result | Call | Notes |
|---|---|---|---|---|---|---|
| `entity.tieMeshes` | **new `[I: offline]`** | `NEED_HAMMER`; refusals in order `-32003 NoActiveDocument` -> `-32018 DocumentNotEditable` -> `-32004 DocumentChanged` -> param/shape `-32602` (incl. the keyValues prescan) -> `-32006 UnknownClass` -> `-32022` -> per-node `-32602`/`-32018` (lock, prefab/instance gate) -> `-32602 already tied` -> (retie) `-32018` on the old parent (all before the undo group); the two documented in-group failures carry `undoGroupOpened`/`undoLabel` | `{class?: string 1..63, entityID?: int 0..2147483647, nodeIDs: [1..64 distinct ints], keyValues?: {k:text} (only with class), allowRetie?: bool=false, expectDoc}`; exactly one of `class`/`entityID` | `{nodeID (omitted when unreadable), handle, class:"entity", classname, created:bool, origin:[x,y,z], meshes:[{nodeID|null, origin}], alreadyTied:[ids], removedEmptyEntities:[ids], keyValues? (additive echo, only when keyValues were requested), noop:false, undoLabel:"MCP: tie meshes to entity", doc:{id,topSerial,undoDepth}}`; an all-`alreadyTied` request succeeds with NO group, `noop:true, undoLabel:null` and the doc tail unchanged | `undo_open("MCP: tie meshes to entity")` -> (`class` form) `CreateNodeByName(doc, cls, firstMesh.origin, firstMesh.angles)` -> keyValues via `apply_keyvalues` (the entity.create SetKeyValue path) -> `RecordNodesAndParents(hist, &vec, 0)` **before the reparents** -> per mesh `ent->vt[57] AddChild(ent, mesh)`; an old parent that is a brush entity with 0 children afterwards is `DeleteNode(doc, old)` in the same group -> `DocSetModified` | see below (the origin rule, the validation order, the two documented in-group failures) |
| `entity.untie` | **new `[I: offline]`** | `NEED_HAMMER`; the same doc-gate order, then `-32602 "not a brush entity"` / `-32018` before anything runs; a corrupt child count (> 1000000) or an unreadable array is `-32603 "child list unreadable"`, while a valid count above 1024 is listed capped with `meshesTruncated:true` (FIX-B review-b L7); `-32603 "entity has no parent"` when the entity's own parent field is unreadable (review-b L8, MoveToWorld calls `entParent->vfn[57]` unchecked) | `{nodeID: int 0..2147483647, expectDoc}` | `{nodeID, meshes:[child ids (an unreadable id is skipped, never -1)], entityDeleted:true, meshesTruncated?:true, undoLabel:"Untie from entity", doc:{...}}` | (no undo_open) `DocMoveToWorld(doc, &{1,0,&one,1,S2_UTLVEC_EXTERNAL})` - Hammer opens its own group "Untie from entity", records recursively, moves every child to the entity's parent and deletes the entity -> `DocSetModified` | see below (the children are read BEFORE the call; the entity is never touched afterwards) |
| `entity.get` | unchanged `[V]`, new `children` | unchanged | unchanged | new `children:[{nodeID, class}]` (one level, the same class strings `node.list` emits), `childrenTruncated:true` + `childrenTotal` when more than 1024 exist, `null` when the child array is unreadable (a read failure is not "no children") | reads `node->childCount`/`node->childHandles` (`HOFF_NODE_CHILD_COUNT`/`HOFF_NODE_CHILD_HANDLES`) like `geo_walk`, skips non-map-node handles, every pointer `mem_ok`-guarded | `entity.find`'s per-entity bodies do not carry it (they never carried per-entity detail) |

Everything else (the mesh/node rows, `edit.batch`'s whitelist, the `doc` tail, `undo.undo`/
`undo.redo`) is unchanged. `entity.tieMeshes` and `entity.untie` are NOT in the `edit.batch`
whitelist (the batch's own review rule: no new method enters it without a batch-side recorded-node
decision; the tie's group is self-contained).

### `entity.tieMeshes` - the validation order and why

- **Before the undo group opens** (the review-13 rule: a refused call leaves no group):
  - exactly one of `class` / `entityID` (`-32602`); `class` non-empty, at most 63 bytes
    (`-32602`; the bridge parses into a 128-byte buffer and refuses anything past 63, so the
    silent cut `json_string_at` would do can never masquerade as a shorter class);
  - `keyValues` only with `class` and only as an object (`-32602`): a retie target keeps its own
    keyvalues, and the create form is the only one that can apply fresh ones. Since FIX-B the
    WHOLE object is dry-parsed before the group (`tie_kv_prescan`, review-b L2/L3): a malformed
    object (`{"a":}` - the apply pass would silently skip every key after the bad value) is
    `-32602 "keyValues: malformed object"`, and a `classname`/`origin`/`angles` key (compared
    case-insensitively, the way SetKeyValue matches) is `-32602 "keyValues: <key> is refused on
    a tie"` - it would run SetKeyValue AFTER the solid-class gate and reclass or move the new
    entity. Other per-key problems stay the apply pass's warnings (the entity.create shape);
  - `allowRetie` exactly `true`/`false` when present (`-32602`; read top-level only - the same
    reason `entity.create` reads `origin` top-level);
  - `nodeIDs`: 1..64 distinct integers in `0..2147483647` (`-32602`; the same reader
    `node.delete` uses, so a fraction/exponent/over-i32 id is refused, not aliased);
  - `entityID` form: the node must exist (`-32022 NodeNotFound`), be a `CMapEntity` whose FGD
    class is a solid class - `gdType == 5` (`-32602 "not a brush entity"`) - not be locked
    (`-32018`), and pass the prefab/instance gate below (`-32602 "entity is inside a
    prefab/instance"` carrying `data.nodeID` = the entityID);
  - `class` form: the class must resolve in the `CMapNodeFactory` registry (`-32006
    UnknownClass`, the exact error `entity.create` answers - review-b M4: the old `-32602
    InvalidParams` was mock/bridge drift the Rust tool does not map). **The @SolidClass gate
    itself is Rust-side** (the hammer-mcp FGD: `ClassKind::Solid`) - the gamedata class type of
    a name is only reachable through a created node (re-triggers.md: the lookup lives in the FGD
    lib behind CMapEntity's class setter and is NOT pinned, so a bridge-side pre-create type
    lookup is not an option - that is why the check stays in-group, see below), so the bridge
    re-checks it *after* the create (below);
  - every `nodeIDs` entry: found (`-32022 NodeNotFound {nodeID}`), a `CMapMesh`
    (`-32602 "not a mesh" {nodeID}` - groups/entities/props are refused; Hammer's flatten and
    "Tie Static Objects" paths are out of scope), inside the document proper (the
    prefab/instance gate below, `-32602 "mesh is inside a prefab/instance" {nodeID}`), not
    locked (`-32018`), and either under the world, under a group, or:
    - under the target entity itself (entityID form) -> **skipped**, listed in `alreadyTied`
      (exactly what Hammer skips). **If EVERY mesh is already under the target, the whole call
      is a success no-op and opens NO group** (FIX-B live L1, review-b L4): the reply is the
      normal success shape with `created:false`, `alreadyTied:[...]`, `removedEmptyEntities:[]`
      and the doc tail unchanged (`undoDepth`/`topSerial` untouched) - the old behaviour pushed
      an empty "MCP: tie meshes to entity" group and marked the doc modified;
    - under ANOTHER entity -> `-32602 "already tied" {nodeID, parentNodeID}` unless
      `allowRetie:true`, in which case the mesh is moved and its emptied old entity is deleted
      in the group and reported in `removedEmptyEntities`. **With `allowRetie:true` the OLD
      parent is lock-checked too** (review-b M2: the retie empties it and `DeleteNode`s it -
      `node.delete` refuses locked nodes, so a retie must not bypass the lock on the entity it
      destroys): `-32018 "node is locked"` with `data.nodeID` = the old parent's id.
      (A mesh under a group is never "tied": `AddChild` moves it between the layer containers.
      The gate treats every entity parent as tied - a mesh under a point entity is abnormal but
      still parented - while the emptied-deletion rule below stays `gdType == 5`.)
- **The prefab/instance gate** (review-b M5). No existing bridge helper tells prefab ownership
  (entity.create / node.setTransform / mesh.* never walk ancestors), so the gate is a parent
  walk: from the node up via `HOFF_NODE_PARENT` (`mem_ok` each step, bounded at 64 like
  `geo_walk`), every ancestor must be the document's world root (compared by pointer, so a
  sub-map's own CMapWorld does not pass), a `CMapGroup`, or a `CMapEntity`; anything else
  (`CMapPrefab`, `CMapInstance`, ...) refuses. The walk runs for every mesh AND for the
  `entityID` target, before the group opens. Why: `geo_find` DOES descend into
  prefab/instance children, and `AddChild` moves nodes between containers with no file check -
  a mesh picked from `node.list` can name a node owned by a referenced sub-map, and reparenting
  it into a host-map entity would desync the two documents (Hammer's UI never does this:
  instance contents are not individually selectable). A parent that reads as null stops the
  walk as OK - the same under-world status quo the tie gate already gives such a node.
- **The errors name their node.** The `-32022`/`-32602` refusals about a specific node carry
  `data.nodeID` (and `data.parentNodeID` for "already tied", and the -32018 old-parent lock
  from the retie gate), which is what the Rust tools map into `NodeNotFound` / `NotAMesh` /
  `MeshAlreadyTied`. A `-1` is never emitted: a reply field whose id read failed is omitted
  (`nodeID`) or `null` (a `meshes` row) - review-b L6.
- **The group.** One `AddOperation("MCP: tie meshes to entity")`. In `class` form:
  `CreateNodeByName(doc, cls, firstMesh.origin, firstMesh.angles)` - **the FIRST mesh's
  origin/angles, not the bounds centre** (Hammer's rule, `[V live]`: the mesh's pivot, which mesh
  rebuilds put at the bounds centre). The create records itself in this group (re-props.md Q1),
  so the keyvalue writes need no extra RecordNodeTree. The keyvalues go through the exact
  `entity.create` path (`GetEntityProperties` + `apply_keyvalues`, String writes via
  `CMapGameDataNode::SetKeyValue`), so FGD-validated keys land the same way they do on
  `entity.create`. Hammer applies the FGD default keyvalues itself on create
  (`trigger_once` spawnflags "4097" `[V live]`); an explicit `keyValues.spawnflags` wins.
- **The defensive solid-class check.** After the create, `gdType =
  *(int*)(*(u8**)(ent + HOFF_ENTITY_GAMEDATA_CLASS) + HOFF_GDCLASS_TYPE)` (both pointers
  `mem_ok`-guarded; null/unreadable = class unknown to the FGD). If it is not 5, the created node
  is `DeleteNode`d **in the same group** and the op answers `-32602 "not a solid (brush) class"`.
  Documented undo effect: the group stays open as a **no-op** - `undoDepth` is +1 and one
  `undo.undo` (label "MCP: tie meshes to entity") pops it, leaving the document content
  unchanged. This is the one refusal that happens after the group opens (a bridge-side pre-create
  class-type lookup does not exist un-pinned - re-triggers.md: the gamedata class of a name is
  only reachable through a created node - so the in-group check stays, FIX-B L3 option (b)), and
  the live test pins both the +1 and the pop. (Hammer itself never checks: it ties under whatever
  class and the map checker complains later.) Since FIX-B the error carries
  `data:{reason, undoGroupOpened:true, undoLabel:"MCP: tie meshes to entity"}` (review-b M3 /
  live L3): the Rust side can count the refusal AND know one undo restores the depth.
  `maybeExecuted` is deliberately ABSENT - nothing of the caller's intent executed, so
  `live_common` must not mark an edit - and the same marker is on the other in-group failure,
  the `-32603 CreateNodeByName failed` path.
- **The reparent.** `RecordNodesAndParents(hist, &vec, 0)` runs **before** the `AddChild` calls
  (Hammer's order): it records each node and, when it has a parent, that parent non-recursively,
  which is what makes the reparent undo restore both child lists. The vector is the caller-owned
  stack shape `{count, pad, data, alloc, flags}` with the entity first (entityID form - Hammer
  records the selected entity too) then every requested mesh (including `alreadyTied` ones, which
  are recorded but skipped by the reparent loop, like Hammer skips them). Then one
  `AddChild` per mesh - vtable slot 57, `S2_SLOT`-asserted in `s2sdk.h`, pinned as
  `HVS_NODE_ADD_CHILD`. `AddChild` never transforms: node transforms are world-space
  (`CMapNode::GetWorldTransform` builds from the node's own origin/angles only), so a tied mesh
  keeps its world geometry `[V live]`; the reply echoes each mesh's origin read back after the
  tie. An `AddChild` that returns 0 AND whose parent read-back is not the entity is
  `-32603 InternalError` with `data.maybeExecuted:true` (the earlier ties in the group already
  ran - verify with `entity.get`, do not retry), the same contract `entity.removeConnections`
  uses. Since FIX-B (review-b M6) that path also calls `DocSetModified` BEFORE returning (the
  document WAS modified - the old path returned without it, so closing the map might not prompt
  to save) and names the progress in the error data: `{reason, nodeID (the refused mesh),
  maybeExecuted:true, tied:[ids tied so far], removedEmptyEntities:[ids already deleted]}` -
  each list capped at 10 entries (the error buffer is fixed; a successful tie's reply reports
  every id). Finally: an old parent that is a brush entity (`gdType == 5`) and now has 0 children is
  `DeleteNode`d in the same group and its nodeID reported in `removedEmptyEntities` (Hammer's
  `RemoveNode(old, 3)` rule; `[V live]`: the emptied trigger_multiple was auto-deleted and undo
  restored it). `DocSetModified` closes the op; the `doc` tail is appended by `geo_dispatch`.
  The FIRST mesh's origin/angles are also read before the group opens, and an unreadable pair
  refuses `-32603` instead of creating the entity at `[0,0,0]` with a normal-looking reply
  (review-b L5).
- **The reply.** `created` distinguishes the two forms; `classname` is read back from the
  entity's own `entity_properties` (so the entityID form reports the target's real class);
  `handle` is the epoch-scoped token (`entity.create`'s shape, usable in `edit.batch`); `origin`
  is the entity's origin read back; `nodeID` is OMITTED when the entity's id is unreadable
  (review-b L6 - `entity.create`'s convention; a reader must never see a `-1` it could address),
  and a `meshes` row whose id read failed carries `"nodeID":null`. The `keyValues` echo (only
  when the request carried keyValues) is **additive** beyond the §5 contract - the same
  `applied`/`warnings` shape
  `entity.create` replies with, so a refused key can never be silent.
- **No material change** (re-triggers.md Q4 `[V bin]`: `toolstrigger` never occurs in the tie
  path; the class makes it a trigger). The Rust tools default the volume material when they
  create the meshes.

### `entity.untie` - why no undo_open and what the reply can promise

- The gate: `geo_entity_node` (`-32022` / `-32602 "not an entity"`), then `gdType == 5`
  (`-32602 "not a brush entity"` - a point entity's children are not tied meshes) and
  `childCount >= 1` (an entity without children is refused with the same text, per the wire
  contract), then the lock check, then (FIX-B review-b L8, defensive) the entity's own parent
  read: per the RE, MoveToWorld calls `entParent->vfn[57]` without a check, so a node whose
  parent field is unreadable is `-32603 "entity has no parent"` instead of a null dereference.
  A child count that is negative or above 1000000 (corrupt) is `-32603 "child list unreadable"`
  - fail closed rather than call into Hammer with an unverified vector. Since FIX-B (review-b
  L7) a VALID count above 1024 is no longer that refusal (Hammer itself has no cap; the old
  text misreported a valid state as an internal fault): the reply lists the first 1024 child
  ids and adds `meshesTruncated:true` when more exist. An id whose read fails is skipped, never
  reported as `-1` (review-b L6).
- The child ids are collected **before** the call (the entity is deleted by it; the node is never
  touched again - not even `node_id_of`).
- `DocMoveToWorld(doc, v)` with a 1-entry stack vector. It opens its OWN undo group ("Untie from
  entity" - `undo.undo` reports exactly that label `[V live]`), records recursively, moves every
  child to the **entity's parent** (the world, or a group the entity was in - not necessarily the
  world) and deletes the entity. Calling `undo_open` first would stack a second empty group on
  top of it. Side effect: Hammer selects the untied children - UI state, harmless.
- `entityDeleted` is `true` by the contract of the call (`RemoveNode(ent, 3)`); the bridge does
  not re-read the node after the call (it may be freed). A client verifies with `entity.get`
  (`-32022`) - the live test does.
- Not covered (documented, re-triggers.md "Open risks"): an entity inside a group/prefab unties
  to the group (children go to the entity's parent `[V bin]`), not tested live.

### `entity.get` children - the same walk as node.list, one level

- `childCount`/`childHandles` with the same guards `geo_walk` uses (`0..1000000` sanity, the
  handle array `mem_ok`-checked end to start); non-map-node handles are skipped, not reported.
- `class` is `node_class` - the same strings `node.list` emits (`mesh`/`entity`/`group`/
  `prefab`/`instance`/`world`/`other`), so a brush entity's children read
  `[{"nodeID":M,"class":"mesh"}, ...]`.
- Output cap 1024 (`CHILDREN_LIST_MAX`); past it the reply carries
  `childrenTruncated:true` + `childrenTotal` (the real map-node child count) - the same shape the
  connections vector lists under, never a silent cut. `nodeID` is `null` when a child's id
  attribute is unreadable (FIX-B review-b L6: never `-1` - a reader would address node -1), and
  the corrupt-count sanity check runs BEFORE the handle-array probe (review-b L10: the old order
  `mem_ok(hs + n - 1)`'d a wild address first).

## §8 error codes

**No new codes.** Reused: `-32602 InvalidParams` (every param rule above; "not a mesh",
"already tied", "not a brush entity", "not a solid (brush) class" with
`undoGroupOpened`/`undoLabel`, the keyValues prescan, the prefab/instance gate), `-32022
NodeNotFound` (a nodeID/entityID the live document does not have), `-32018 DocumentNotEditable`
(the document gate, a locked node/entity, and since FIX-B the LOCKED OLD PARENT of an
`allowRetie` retie - `data.nodeID` names it), `-32006 UnknownClass` (FIX-B review-b M4: a tie
class not in the node-factory registry, the exact error `entity.create` answers), `-32004
DocumentChanged`, `-32003 NoActiveDocument`, `-32603 InternalError` (`AddChild` refused after
earlier ties ran, `maybeExecuted:true` + the progress lists; an untie child list that cannot be
read; an untie entity whose own parent is unreadable; an unreadable first-mesh origin/angles).

## §5/§6 notes

- Method count: **84 registered / 69 implemented** (m2.18: 82/67; the two new rows).
  `control.version.bridgeVersion` and every `"build"` field are **`m2.19`**.
- **New pins** (all added by the RE pass, committed on this branch's base, `bridge_pins.h`
  regenerated by `pins.py` - 308 macros): `HFN_DOC_MOVE_TO_WORLD`
  (`CMapDoc::MoveToWorld`, `hammer!0x8874B0`), `HFN_RECORD_NODES_AND_PARENTS`
  (`CMapDocHistory::RecordNodesAndParents`, `hammer!0x86DED0`), `HVS_NODE_ADD_CHILD`
  (vslot `HV_MAPENTITY_VT` 57 = `CMapNode::AddChild`, verified in memory at load like every
  vslot), `HOFF_NODE_PARENT` (0x198), `HOFF_ENTITY_GAMEDATA_CLASS` (0x550),
  `HOFF_GDCLASS_TYPE` (0xC). `HFN_DOC_TIE_TO_ENTITY` stays reference-only (selection-based +
  modal; never pinned for calls). No new module: `PIN_MODULE_COUNT` stays 12.
- **`s2sdk.h`**: `CMapNodeVtbl` gains slot 57 `AddChild` (`S2_SLOT` against
  `HVS_NODE_ADD_CHILD`); `struct CMapNode` gains the `parent` field (`S2_FIELD` against
  `HOFF_NODE_PARENT`); `HammerFns` gains `RecordNodesAndParents` and `DocMoveToWorld` (bound from
  the pins next to `RecordNodeTree`). No literal address or offset appears in `mcp_bridge.c` -
  the diff's added lines carry no `0x` literal (bridge/test_triggers.py `--static` checks it,
  like `test_entities.py` does).
- Version bump outside the new code: `bridge/test_entities.py`, `test_props.py`,
  `test_modeldoc.py`, `test_modeldoc_render_live.py`, `test_pet.py` and `test_pet_live.py` assert
  the bridge build and are bumped to `m2.19` (the same set m2.18 bumped, plus `test_props.py`).
- New live test: `bridge/test_triggers.py` (run it against a tools session with a scratch map
  open; its `--static` mode is the offline half of the same contract). It follows the map-editing
  conventions: run-unique targetnames and its own coordinate band (band 4, x ~ 80000), cleanup
  pops only its own groups and saves again.
- The Rust side gates the tie class on the FGD (`ClassKind::Solid`) BEFORE sending; the bridge's
  gdType check is the defensive second net. `create_trigger`'s undoGroups accounting counts the
  tie as one group ("MCP: tie meshes to entity") and `move_to_world` as one ("Untie from
  entity").

## Live probe list for the orchestrator (`bridge/test_triggers.py` covers all of it)

1. `control.version` -> `m2.19`; `control.capabilities` advertises `entity.tieMeshes`,
   `entity.untie`.
2. `mesh.create` a box far from the origin -> `entity.tieMeshes {class:"trigger_once",
   keyValues:{targetname, spawnflags:"4097"}}`: reply `origin` == the mesh origin, `created:true`,
   `doc` tail; `entity.get` -> `children == [{nodeID, class:"mesh"}]`, `spawnflags == "4097"`.
3. A second mesh into the same trigger via `entityID` -> `created:false`, 2 children.
4. A third mesh into a second trigger; retie without `allowRetie` -> `-32602 already tied`
   naming both nodes, undo depth unchanged; with `allowRetie` -> moved, the emptied second
   trigger deleted (`removedEmptyEntities == [E2]`, `entity.get` -> `-32022`), 3 children.
   Tying a mesh that is ALREADY under the target (every mesh `alreadyTied`) is a SUCCESS no-op:
   no group, doc state unchanged (FIX-B L1).
5. The refusals - each row re-reads `doc.active` right before it checks (FIX-B L2: no running
   counter, one bug cannot cascade): point class via `class` -> `"not a solid (brush) class"`
   with `undoGroupOpened:true`/`undoLabel` (and the documented no-op group: depth +1, popped by
   one undo), a class the factory does not know -> `-32006 UnknownClass` (FIX-B M4),
   `keyValues` with `classname`/`origin`/`angles` or a malformed object -> `-32602` before any
   group (FIX-B L2/L3), `entity.untie` on a point entity and on a mesh -> `-32602`, the param
   matrix (no target, both targets, keyValues with entityID, bad allowRetie,
   empty/duplicate/65 nodeIDs, unknown ids) -> `-32602`/`-32022` with the depth and topSerial
   unchanged per row. Explicit `NOT RUN` rows (never silent passes): the locked-old-parent
   retie (FIX-B M2) when no method can set a lock, and the prefab/instance gate (FIX-B M5)
   when the scratch map has no prefab (or no reachable mesh inside one).
6. `entity.untie` on the trigger -> the meshes back with unchanged origins, the entity gone
   (`-32022`), one `undo.undo` (label "Untie from entity") restores entity + children.
7. **Not covered by the script** (the orchestrator may add them): the saved-file vertex check
   (`save_map` + the vmap reader: the tied mesh's `origin`/`meshData` unchanged - the live route
   can only read origins, `mesh.get` is file-only), undo x N back to the untouched map, a tie
   under a group parent, a rotated first mesh (the entity inherits its angles), and the
   1024-children truncation row.

## Offline proof

`./bridge/build.sh` is green: `sigcheck.py validate` (15 files, 4278 entries, 0 problems),
`sigcheck.py resolve --strict`, `pins.py` (the committed `bridge_pins.h` matches the regenerated
one, 308 macros), `gen.py --check` (both ABIs), `sdkcheck.py` (16 PASS / 6 SKIP / 0 FAIL), the
host unit tests (`test_entity_json` 125 cases - unchanged by this pass, they cover the shared
JSON readers the tie params reuse) and the DLL link - with no compiler warning.
`python3 -I bridge/test_triggers.py --static` is PASS.

**Negative evidence** (revert one thing at a time, the tests unchanged):

- the two rows back to `impl 0`: `test_triggers.py --static` fails on the rows and the dispatch
  arms, and `control.capabilities` would stop advertising them (the Rust capability gate
  `entity.tieMeshes` would report BridgeTooOld);
- `undo_open` moved back after the already-tied gate (or the gate moved into the group):
  the static order check (`gate < undo_open < RecordNodesAndParents < AddChild`) fails;
- ANY other refusal gate moved into the group (the M1-strengthened static checks): the
  "refusal check not before undo_open: <needle>" row fails (verified here by mutating a copy:
  relocating the "already tied" builder after `undo_open` fails the static run);
- the all-`alreadyTied` no-op removed (or moved below `undo_open`): the static
  "at_n == n no-op must return before undo_open" row fails (also verified by mutation), and
  live the empty-group bug from `target/test_triggers-1.log` returns;
- `RecordNodesAndParents` moved after the `AddChild` loop: the same order check fails, and the
  live undo would restore the wrong child lists (the RE's reason for the order);
- `entity.untie` gaining an `undo_open`: the static "must NOT undo_open" check fails, and live
  there would be two groups per untie (the bridge's empty one on top of Hammer's);
- the defensive `gd_type(ent) != 5` check removed: `refuse_point_class` (static text row) fails
  and a point class would be tied like Hammer's UI does;
- the `gd_type(old) == 5` emptied-deletion rule removed: the static row fails and the live
  `removedEmptyEntities == [E2]` check would come back empty;
- `entity.get`'s children field removed: the static children rows fail and the Rust
  `list_triggers` live route would read `children: null` (its documented BridgeTooOld shape).

## Applying this in idabins (not done here)

`../../idabins/docs/data/bridge-protocol.md` §7 needs: the two new rows with their params,
results and call chains, the `entity.get` `children` shape, the method count 84/69 and the
`m2.19` build string. §8 needs no new row. `../idabins/docs/interfaces/` would want the
`CMapNode` parent field and the `AddChild` slot (slot 57), plus `RecordNodesAndParents`/
`MoveToWorld` in the HammerFns table - the RE report (`target/re-triggers.md`) is the source for
both. The docs live in the `idabins` checkout, which this pass must not touch (AGENTS.md: work
in the assigned worktree).
