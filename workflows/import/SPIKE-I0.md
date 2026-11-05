# I0 live spike — results (2026-10-29)

Tools container `ADDON=accept_p4 bridge/runtime.sh up` (bridge m3.17, game build 2000933), hammer-mcp from
this branch. Evidence (dumps, generators, frames) in this worktree's `target/import/spike/` (gitignored);
spike models stay in the test addon under `models/import_spike/` (content + compiled).

| # | Question | Answer | Verified |
|---|---|---|---|
| a | static SMD in `RenderMeshFile` compiles, bounds read back | **yes**: `spike_cube.smd` (32 u cube) compiles via `model_compile` (resourcecompiler), render and physics bounds exactly ±16; `prop_static` renders textured in game | live |
| b | skinned SMD + anim compiles, bones in `m_modelSkeleton` | **yes**: 2-bone tube, bones `base`/`tip` parent −1/0, sequence `spike_bend`; as `prop_dynamic` with `StartingAnim` + `ANIM_LOOP_MODE_LOOPING` the tube bends in game (frames 1 vs 3) | live |
| c | `mesh.setFaceTexture` with absolute `axisU/axisV/scale` | **no, not exposed**: the native function writes absolute axes and the reply reads them back, but the bridge op only takes relative `shift/scale/rotate` (orthogonal axes, no shear). I3 needs a bridge op `mode:"axes"` (m3.18) | code read (`crates/cs2-bridge/src/ops/mesh.rs` `tex_parse`) |
| d | prefab layout / `func_instance` | **`func_instance` does not exist in CS2** (`csgo.fgd`: `@exclude func_instance`; base.fgd has it). No shipped `.vmap` holds a `CMapPrefab`. No bridge method creates one (methods list). Prefab instancing therefore needs a bridge `node.createPrefab` (hammer.dll RE) or a hand-made sample to copy | FGD + file scan + live methods list |
| e | player skeleton + animgraph | see below: skeleton = `worldmodel.vnmskel`, an SMD on its bones + `NmSkeletonReference` + `DefaultAnimGraph2` compiles and the graph poses it like a real agent | live (after an engine restart, see the correction) |
| f | `model_compile` without a ModelDoc container | **works**: it went through the bridge `job.*` route (`compiler:"bridge"`) in a `TOOL=hammer` container; the `MODELDOC` pin only matters on the podman fallback | live |

## Bugs found and fixed on the way (this branch)

1. **KV3 types 26/27.** Every block of a model compiled by the current resourcecompiler (`MDAT`, `PHYS`,
   `ASEQ`, `AGRP`, `CTRL`, `RED2`) failed to decode with `unknown KV3 value type 26` (27 in `CTRL`), so
   `get_model_info` had no bounds/sequences for any freshly compiled model. They are constant
   `UINT32_ZERO` / `UINT32_ONE` (no lane read; ValveResourceFormat master added the same numbers; shipped VPK
   models predate them). `hammer-mcp/src/resource/kv3bin.rs`; both test encoders now emit them.
2. **`compile_map` closed `failed` on a good compile** ("summary could not be parsed" while the log ends in
   `OK: 17 compiled, 0 failed`). The 2026-10-21 fallback read `log.host`, which the raw bridge `job.status`
   reply never carries; now it resolves `log.relative` too (`compile_map.rs`, regression test
   `an_ok_status_without_a_summary_is_judged_from_the_relative_log`).

## Player models (e) — what the files say

- `characters/models/<agent>/*.vmdl_c` are **1-bone `dummy` placeholders** (orange default material), on both
  the tools and the client install. The real agents are `agents/models/<agent>/<agent>.vmdl_c`.
- `agents/models/tm_phoenix/tm_phoenix.vmdl_c` DATA: 86 bones (`root_motion pelvis spine_0..3 neck_0 head_0
  clavicle_L …` + jiggle/twist/eye/scap bones), `m_vecNmSkeletonRefs = [animation/skeletons/characters/
  worldmodel.vnmskel, …/viewmodel.vnmskel]`, `m_animGraph2Refs = [{"" : animation/graphs/worldmodel/
  worldmodel.vnmgraph}, {uimodel: …/ui/uimodel.vnmgraph}, {hudmodel: …/viewmodel/viewmodel.vnmgraph}, …]`.
- **The canonical skeleton is `animation/skeletons/characters/worldmodel.vnmskel_c`** (user steer
  2026-10-29: player models use AnimGraph2, bone names come from the base game's VPK): 75 bones
  `m_boneIDs`, `m_parentIndices`, `m_parentSpaceReferencePose` and `m_modelSpaceReferencePose` as
  `[tx,ty,tz,scale,qx,qy,qz,qw]`, 12 mask definitions, 1 secondary skeleton. The decoder already reads it.
  The agent `.vmdl_c` skeleton is a different superset (no `wpnHand_*`, `attach*`, `camera`; adds
  twist/jiggle bones) in another frame (root `[.5,.5,.5,.5]`), so retargeting must target the vnmskel.
- ModelDoc nodes (VRF ModelExtract, `[I]`): `NmSkeletonList > NmSkeletonReference{filename}` and
  `AnimGraph2List > DefaultAnimGraph2{filename}` (+ `AnimGraph2{name, filename}` for the others). With
  these, `spike_agent.vmdl` compiles and DATA carries the two refs exactly.

### The stick-figure test

`target/import/spike/gen_nm_smd.py` writes an SMD on the 75 vnmskel bones (parent-space rest pose as Source
RadianEuler, gimbal-safe; a box per joint + a bar to the parent, rigid weights). Placed as `prop_dynamic`:

- **without** the NM/graph nodes: an upright, coherent figure (bind pose right);
- **with** `NmSkeletonReference` + `DefaultAnimGraph2`: scrambled upper body, static over 1.8 s.

**Correction (2026-10-29, I1 live run):** that scramble was *my* generator's Euler bug (gimbal lock at
the pelvis/spine's ±90° pitch, fixed in `gen_nm_smd.py`) seen through the engine's **model cache** — a
running engine keeps a loaded `.vmdl_c` across `run_map`/map reloads, so the fixed model was never shown.
After an engine restart: the graph-driven stick figure stands in the same stepping pose as a real
`agents/models/tm_phoenix/tm_phoenix.vmdl` placed beside it on the same graph (frames
`spike_agent8/9`, static over 3 s — a `prop_dynamic` does not feed the graph's gameplay inputs).

(I6 live, 2026-11-03: the graph hides a quarter turn of the whole SMD in game, so "no pre-turn" here was not a
proof; the player writer applies the same Rz(−90°) pre-turn as props, and Hammer's viewport confirms the bind.)

So **an SMD on the vnmskel bones binds to AnimGraph2** although the importer lowercases bone names
(`clavicle_L` → `clavicle_l`, checked in the `.vmdl_c` strings) and reorders them by depth. **SMD stays
the intermediate for players too**; DMX (VRF `IO/ContentFormats/DmxMeshBuilder.cs` + `DmxModel.cs`, MIT)
stays a fallback only if a later check needs exact names.

The importer also **turns SMD content by Rz(+90°)** (root `[0,0,.707,.707]`; I1 live: an import written
facing +X came out facing +Y). `hammer-mcp/src/import/smd.rs` pre-turns by Rz(−90°).

## Other notes for the WPs

- `get_model_info` refuses paths outside `models/` (agents are `agents/models/…`, skeletons
  `animation/skeletons/…`): widen for I6, plus `include:"skeleton"` reading `.vnmskel_c`.
- `prop_dynamic` keys in this FGD: `StartingAnim`, `StartingAnimationLoopMode`
  (`ANIM_LOOP_MODE_LOOPING`/`…_NOT_LOOPING`), `IdleAnim`, `use_animgraph`, `animgraph2_identifier`
  (no `DefaultAnim`). The plan's `next` chain must use these.
- **A running engine never reloads a recompiled `.vmdl_c`**, not even across `run_map`/map reloads: restart the tools session (`runtime.sh down` + `up`) to see a model change. The map compile also packs the props' models into the map `.vpk`, so recompile the map too.
- `look {target:<nodeID>, mode:"frames"}` frames a compiled custom model with a generic ±64 box and a corner
  pose that landed outside the map; an explicit `pose` worked. When the target is a prop, `look` should use
  `get_model_info` bounds (I1 polish or a FIXME).
- `compile_map` refuses while the engine has the map mounted; `run_map {unload:true}` first (works).
