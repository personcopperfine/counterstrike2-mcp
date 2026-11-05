# Hammer trigger (brush entity) RE notes (build 2000930)

Facts the trigger tools and bridge m2.19 are built on (`list_triggers`, `create_trigger`,
`tie_to_entity`, `move_to_world`, `set_trigger_volume`). Every address the bridge uses is an entry
in `sigs/signatures/hammer.json` (pins -> `bridge_pins.h`); never copy a number from here into
code. Distilled from the RE session's `target/re-triggers.md`; post-implementation facts (the live
acceptance, the runtime verdicts) are in `hammer-mcp/AGENT.md` "## Triggers".

hammer.dll build **2000930**. **Every address here is a real 2000930 address**
(RVA = address - 0x180000000); the IDB's `sub_<addr>` names are stale.

Evidence tags: **[V live]** seen in the running Hammer (probe DLL, "Probe" at the end), **[V bin]**
read in the decompiled/disassembled binary, **[V file]** read in a saved `.vmap`, **[I]** inferred.

---------------------------------------------------------------------------------------------------

## Q1. Tie to Entity / Untie (the commands the tools re-implement)

### What the binary does [V bin]
* Commands registered by name: `"TieToEntity"` -> `0x180140820`, `"MoveToWorld"` (menu "Untie From
  Entity") -> `0x1801413F0`; context-menu builder `0x18039CB50`; hot key Ctrl+T.
* `CMapDoc::TieToEntity(doc, existingOrNull)` = `0x1808889D0` re-collects the **current selection**
  and asks a modal question when an existing brush entity is selected — so the bridge cannot call
  it with arbitrary nodes; it is re-implemented instead (short, ordinary callees):
  eligibility per node (`0x18087C970`) is a bitmask (mesh -> 1, smart prop/model entity -> 2,
  group/deformer/brush entity -> OR of children); groups/brush entities are flattened to their
  child meshes when only meshes remain; one undo group (`AddOperation`, "To Entity");
  a new entity (when no existing one) is `CreateNodeByName`'s body with **origin/angles := the
  first mesh's**; `RecordNodesAndParents` (`0x18086DED0`) records each node plus its parent's
  tree (this is what makes the reparent undoable); then `CMapNode::AddChild` (slot 57,
  `0x18084F670`) reparents each mesh, and an old brush-entity parent left with 0 children is
  removed (undoable). **No material change anywhere in the tie path**, and `toolstrigger` does not
  occur in hammer.dll at all.
* `AddChild` never changes transforms: node transforms are world-space
  (`GetWorldTransform 0x1809EB2A0` builds from the node's own origin/angles only), so a reparented
  mesh keeps its world geometry.
* `CMapDoc::MoveToWorld(doc, nodes)` = `0x1808874B0` takes the node list as a **parameter**
  (so the bridge can call it directly with a stack-built vector), opens its own undo group
  ("Untie from entity") — do NOT `undo_open` first — and reparents each child to the entity's
  PARENT (world, or the group/prefab the entity sat in), then removes the entity.

### Live results (probe DLL, scratch map) [V live]
* mesh.create -> tie as `trigger_once`: entity created with origin = the mesh's pivot, parent =
  world; mesh origin unchanged (world geometry kept); the FGD defaults are written by
  CreateNodeByName itself (`spawnflags "4097"`, `StartDisabled "0"`).
* gamedata `type == 5` <=> the class is a `@SolidClass` (triggers, func_buyzone,
  post_processing_volume: 5; `info_player_counterterrorist`: 4).
* `mesh.setGeometry` on the **tied** mesh: works unchanged, world coordinates, mesh stays a child,
  pivot re-centred, entity origin NOT updated (harmless).
* Entity SetOrigin (slot 17) moves the tied meshes with it; undo restores both.
* undo of the tie -> mesh back under the world, entity gone; redo -> tied again (same ids).
* untie -> children under the world, entity deleted, undo label is Hammer's own
  **"Untie from entity"**; tie into an EXISTING brush entity (`entityID`) adds a second child;
  a retie from another brush entity auto-deletes the emptied old one; undo restores all.

### Recipe: `entity.tieMeshes {nodeIDs, class?|entityID?, keyValues?, expectDoc}`
Tools main thread (mutating). Validate ALL before `undo_open` (a refusal leaves no group):
`nodeIDs` 1..=64 distinct meshes, found, not locked; exactly one of `class` (a `@SolidClass` —
the gate is in Rust, the FGD; the bridge re-checks the created node's gamedata type == 5 and
deletes it in-group otherwise) / `entityID` (a brush entity, not locked); a mesh whose parent is
already the target is skipped (`alreadyTied`). Then one group `"MCP: tie meshes to entity"`:
create the entity when needed (origin/angles from the FIRST mesh, keyvalues via the
`entity.create` SetKeyValue path), `RecordNodesAndParents` **before** the `AddChild` calls
(Hammer's order — it records the old parents so undo restores both child lists), `AddChild`
(slot 57) per mesh, delete an emptied old brush-entity parent, `DocSetModified`. Reply: entity
nodeID, class, origin, child ids, removed old entities, doc tail.

### Recipe: `entity.untie {nodeID, expectDoc}`
Validate: a brush entity (gdType 5), not locked, `childCount >= 1`; collect the child ids BEFORE
the call. Then **do not call `undo_open`** — build the one-element node vector and call
`HFN_DOC_MOVE_TO_WORLD` directly (it opens its own "Untie from entity" group), then
`DocSetModified`. Side effect: Hammer selects the untied children (UI state, harmless).

### Bridge m2.19 (what was actually implemented)
`entity.tieMeshes {nodeIDs, class?/entityID?, keyValues?, allowRetie?, expectDoc}` -> the entity
id, the bridge's `undoLabel`, the `handle`, a `keyValues {applied, warnings}` echo, `alreadyTied`
skips, `removedEmptyEntities`; refusals (`not a mesh`, `already tied`, missing/point `entityID`,
locked node, `not a solid (brush) class`) all carry `{reason, nodeID}` and open **no** undo group;
an all-`alreadyTied` tie answers `{noop:true, undoLabel:null}` with no group. `entity.untie
{nodeID, expectDoc}` -> `{removed, children, undoLabel:"Untie from entity"}`.
`entity.get` gained the `children` rows (`[{nodeID, class}]`, capped at 1024 with
`childrenTruncated`) that `list_triggers`/`set_trigger_volume` enumerate.

## Q2. Creating a brush entity directly [V live]
`CreateNodeByName("trigger_multiple")` + `CreateMeshElement` + `InsertNode(doc, mesh, entity)`
+ `RecordNode` is identical in effect to a tie (same `AddChild` path). The tools instead use
`mesh.create` + `entity.tieMeshes` in one logical flow (`create_trigger`), and
`set_trigger_volume`'s `mesh.setGeometry` for a resize. A solid entity with NO mesh child is
legal in the editor but the compiler emits no volume for it [I]. Existing methods on a tied mesh:
`mesh.setGeometry` [V live], `mesh.setMaterial` [I: same RecordNodeTree + AssignMaterialToFaces
path], `node.setTransform` on the entity moves the tied meshes [V live], `node.list` recurses
into entity children [V live].

## Q3. Saved format [V file]
* A brush entity is a `CMapEntity` element whose `children` array holds `CMapMesh` elements
  (Valve's own `template_defuse.vmap`: func_bomb_target 33 -> mesh 31). No flag marks "brush
  entity": the attributes are the same as a point entity's. What makes one is (a) the FGD class
  being `@SolidClass` and (b) having `CMapMesh` children — exactly what `list_triggers` tests.
* Child mesh `origin` is **world-space** (equals the entity origin right after a tie, diverges
  after a rebuild); mesh vertex positions are relative to the mesh origin. World bounds of a
  trigger = union over child meshes of `origin + R(angles)·(scales∘p)` (the `.vmap` mesh reader's
  rule).
* Connections are saved as usual (`connectionsData` -> `DmeConnectionData`, e.g.
  `OnStartTouch -> kill_ct Kill`, targetType 7) — the file route reads them there.
* FGD: `@SolidClass` -> `ClassKind::Solid`; `base.fgd`'s `trigger_multiple`/`trigger_once` are
  `@SolidClass base(Trigger)` / `base(TriggerOnce)`; `TriggerOnce` carries the 12 `spawnflags`
  (1 Clients + 4096 mass, both default on), the `metadata { auto_apply_material }` helper and
  `OnStartTouch`; `Trigger` adds OnStartTouchAll/OnEndTouch/OnEndTouchAll; `trigger_multiple`
  adds `wait`/`OnTrigger`. Live Hammer agrees (gdType 5).

## Q4. Trigger material [V bin + V file]
Hammer does **not** apply a material on tie (no material call in the tie path, `toolstrigger` not
in hammer.dll). The **class** makes it a trigger, not the material (Valve's own template meshes
`post_processing_volume` with a trash material). `toolstrigger` is the convention because it is
invisible in game and orange in the editor [I]. The tools therefore default the volume's material
to the class's FGD `auto_apply_material`, else `materials/tools/toolstrigger.vmat`.

## Q5. Compile + runtime [V live]
* A saved probe map with two `trigger_*` volumes + `OnStartTouch -> <prop>.Kill` compiled clean
  with the `compile_map.sh` recipe (22 compiled, 0 failed, 225 s); the brush entities compile to
  `entities/<targetname>_<nodeID>.vmdl` (the compile log names them).
* Runtime (tools engine, `game_type 0; game_mode 1; map … nomapvalidation=1`): `setpos` inside a
  trigger box killed the named prop at the first poll; the negative-control prop stayed. Note:
  triggers are server-only, so the client's `game.entities {name}` finds 0 — check a trigger's
  EFFECT, not the trigger.
* The m2.19 live acceptance repeated this end to end through the tools themselves (STATUS.md
  "FINAL LIVE": OnStartTouch -> Kill on spawn, `trigger_push` velocity, `trigger_hurt` damage).

## Sig-DB entries (ADDED, status `proposed`; fingerprints merged; sigcheck validate 0 problems)

| entry | locator | 2000930 | bridge |
|---|---|---|---|
| `HFN_DOC_MOVE_TO_WORLD` | sig `48 89 54 24 10 48 89 4C 24 08 53 57 48 81 EC 88 00 00 00` | 0x8874B0 | yes |
| `HFN_RECORD_NODES_AND_PARENTS` | sig `44 89 44 24 18 48 89 54 24 10 55 56 57 41 55` | 0x86DED0 | yes |
| `HVS_NODE_ADD_CHILD` | vslot HV_MAPENTITY_VT 57 | 0x84F670 | yes |
| `HOFF_NODE_PARENT` | sig `48 8B 9E 98 01 00 00 49 3B DE` disp 3 | 0x198 | yes |
| `HOFF_ENTITY_GAMEDATA_CLASS` | sig `48 8B 87 50 05 00 00 48 85 C0 0F 84 ?? ?? ?? ??` disp 3 | 0x550 | yes |
| `HOFF_GDCLASS_TYPE` | sig `83 78 0C 05 0F 85 ?? ?? ?? ?? 48 89 6C 24 48` disp 2 size 1 | 0xC | yes |
| `HFN_DOC_TIE_TO_ENTITY` | sig `48 8B C4 48 89 48 08 55 53 48 8D 68 A1 48 81 EC B8 00 00 00` | 0x8889D0 | no (reference; selection-based + modal) |

Already pinned and used by the recipes: `HFN_CREATE_NODE_BY_NAME`, `HFN_INSERT_NODE`,
`HFN_DELETE_NODE`, `HFN_RECORD_NODE`, `HFN_RECORD_NODE_TREE`, `HFN_ADD_OPERATION`,
`HFN_GET_ACTIVE_HISTORY`, `HFN_DOC_SET_MODIFIED`, `HOFF_NODE_CHILD_COUNT/HANDLES`,
`HOFF_NODE_ORIGIN/ANGLES`.

## Open risks / still unverified
* `entity.tieMeshes` with a non-mesh input (groups, props, smart props — Hammer's flatten /
  "Tie Static Objects" paths) is refused by the recipe, never exercised live.
* Untie of an entity inside a group/prefab: children go to the entity's parent [V bin], not
  tested live (no prefab in the scratch map).
* A tie whose old parent is a **locked** node (`NodeIsLocked`): the bridge refuses, not
  exercised live.
* A **rotated first mesh**: the tie copies the first mesh's angles onto the entity (Hammer does
  too) — untested with non-zero angles.
* `node.delete` of the last tied mesh should remove the emptied entity [I], not tested.
* The entity origin is not kept at the volume centre after mesh rebuilds (Hammer behaves the
  same); harmless for triggers.
* `MoveToWorld` changes the Hammer selection (selects the children) — UI-only.

## Probe (how the [V live] evidence was produced)
`target/orch/re/probe/` = copy of `bridge/` + a hidden `probe.trig` method (ops
info/tie/untie/meshIn/setOrigin, raw RVAs), built with `build.sh` against the main checkout's
`target/game-bins`, installed temporarily. Installed DLL before: m2.18
`d5899665…`; **restored afterwards, sha256 verified identical**. Container `cs2tools-trig`
stopped; the doc was saved clean before `down` (no autosave). Renders:
`workshop/run/app/renders/tr_tied.png`, `tr_setup.png`; driver scripts
`target/orch/re/scripts/{rpc,t,q5,dump_ent,mats}.py`. The worktree's `bridge/` sources are
unchanged (only the regenerated `bridge_pins.h`).
