# Model import (FBX + glTF) — handoff

Branch `import/models`, worktree `<repo>/.claude/worktrees/import`, cut from
main b367b40 (streamline/agent-ux fast-forwarded into main 2026-10-28). Plan: [PLAN.md](PLAN.md) (Fable).

## Goal (user, 2026-10-28)

Import FBX and glTF/GLB models through hammer-mcp for **player models, world geometry and props**, in the
streamline style: a weaker model finishes the task with MCP tools only, and the tools say when it is done.

## User decisions (2026-10-28)

- Player models: **arbitrary rigs** — retarget onto the CS2 player skeleton (not only pre-rigged models).
- World geometry: **both** prop_static and conversion to Hammer meshes, **plus prefab support** (reusable assets).
- **No Blender**: pure Rust only. Skinned glTF -> DMX waits until the DMX schema is pinned.
- Work happens on this branch; merges to main need the user's go.

## User decisions (2026-10-29, after the spike)

- **Prefabs: add the bridge method** — `node.createPrefab` in bridge m3.18 via a hammer.dll RE brief
  (`workflows/re-brief.md`); `prefab create/instance/list/read` all through the MCP.
- **Real player: add SetModel** — server-side signature + bridge `game.setPlayerModel` (I7). CS2Fixes already
  sets player models; use it as a reference for the approach, or reverse it from the binary. CS2Fixes is
  GPL-3.0: reimplement, do not copy its code into this repo.
- Canonical skeleton question is settled by the steer: `animation/skeletons/characters/worldmodel.vnmskel`.

## Established facts (Fable consult 1, string scans of the tools install)

- FBX native (`game/bin/win64/libfbxsdk_2020_3_1.dll`; `.fbx` in modeldoc_utils.dll and resourcecompiler.dll).
  DMX, OBJ (mesh only), SMD native. **glTF/GLB not native** (no gltf strings in compiler/ModelDoc/meshsystem).
- Route: glTF -> OBJ (static) / DMX (skinned, `crates/cs2-dmx` encoder) in `hammer-mcp/src/import/`; FBX passes through.
- `vmdl/template.rs` already builds `RenderMeshFile{filename, import_scale}` / `AnimFile`; physics node classes
  `PhysicsHullFromRender`, `PhysicsMeshFromRender`, `PhysicsHullFile`, `PhysicsMeshFile` are in
  `research/modeldoc-nodetypes.json`. Material `remaps` key spelling is inferred (confirm on first live compile).
- report_ready only checks map mesh materials today; a `prop_materials` check (prop -> vmdl_c RERL -> vmat_c -> vtex_c) is planned.
- Player models in real play need `characters` in gameinfo VpkDirectories plus a server plugin; no animgraph editor in CS2.

## Plan deltas from the spike (apply when starting each WP)

- I1: add an asymmetric fixture and check whether the compiler's root Rz(90°) turns the mesh; `next`
  chain uses `StartingAnim`/`IdleAnim` (no `DefaultAnim` key); `look` should frame by model bounds.
- I3: needs bridge m3.18 `mesh.setFaceTexture mode:"axes"` (native fn already takes absolute axes).
- I4: `func_instance` path is gone; instancing = bridge `node.createPrefab` (RE) or create/list/read only.
- I6: skeleton source = vnmskel (not an agent vmdl_c); intermediate = SMD (DMX only if exact names are
  ever needed); `.vmdl` gets `NmSkeletonList/NmSkeletonReference` + `AnimGraph2List/DefaultAnimGraph2`;
  widen `get_model_info` **and `create_props`** paths (`agents/models/…`; `create_entities` works).
- All live model checks: the engine caches loaded models across map reloads — restart the tools session
  after recompiling a model, and recompile the map (it embeds the models).
- I1 live 2026-10-29: `model_import` of a textured 3-material GLB compiled with its `.vmat`/`.vtex`
  dependencies, remaps/vmat keys work, faces +X after the Rz(−90°) pre-turn.

## How to work (same rules as streamline)

- Sonnet implements per WP in its own worktree (absolute paths); Opus/Fable reviews every diff before merge into import/models.
- OOM guard: game DOWN before workspace builds, `-j2`, `ulimit -v 12000000`, ≤2 parallel builds.
- ModelDoc: one instance only (`TOOL=modeldoc cargo bridge runtime up`). Podman: `XDG_RUNTIME_DIR=<project root>/.podman/xdg`;
  stale `pause.pid` -> rename to `pause.pid.stale-<pid>`.
- Live driver: main checkout `target/orch/mcp_session.py` (+ `target/orch/c.sh`); test addon accept_p4.
- Bridge installed: m3.17 (backup of m3.16 in main `target/bridge/backup-2026-10-28-m316/`).
- Hacks -> `workflows/import/FIXME.md`. Never /tmp, never sleep, no push/PR.

## Status

- 2026-10-28: PLAN.md written (Fable consult 2). Nothing implemented. Key changes vs consult 1: the glTF
  intermediate is **SMD** (text; mesh + skeleton + weights + anims, all accepted by ModelDoc), DMX only later;
  DMX attribute names are pinned from modeldoc_utils.dll strings, so no user reference DMX is needed.
  Two new tools: `model_import {kind: prop|world|hammer_mesh|player}` and `prefab {action}`.
- Work packages: I0 live spike (blocks I1/I3/I4/I6) -> I1 core static import -> I2 materials proof ->
  I3 world-as-meshes -> I4 prefab -> I5 skinned (own rig) -> I6 player retarget -> I7 real-player route
  (bridge `game.setPlayerModel`) -> I8 docs/budget/playbook.
- Open questions (PLAN.md end) — **answered 2026-10-29, see "User decisions (2026-10-29)"**; kept for history:
  1. Prefab instancing if Hammer does not convert `func_instance`: approve a bridge `node.createPrefab`
     (hammer.dll RE lane), or keep prefabs create/list/read-only?
  2. Canonical player skeleton agent (default `tm_phoenix`)?
  3. I7: OK to add a server-side sig + Metamod pawn `SetModel`, or stop player checks at `prop_dynamic`?
  Spike I0(d) needs one prefab made by hand in the Hammer UI (user-permitted) to pin the CMapPrefab layout.
- 2026-10-29: **I0 spike done** — results in [SPIKE-I0.md](SPIKE-I0.md). Static + own-rig skinned SMD
  props work live (a, b, f). `func_instance` is excluded in CS2 (d) and no bridge method creates a
  `CMapPrefab`; absolute face UV axes need a bridge op (c). Player: canonical skeleton =
  `animation/skeletons/characters/worldmodel.vnmskel_c` (75 bones; user steer: AnimGraph2, names from the
  base VPK); real agents are `agents/models/…`; an SMD on those bones + `NmSkeletonReference` +
  `DefaultAnimGraph2` is posed by the graph like a real agent (verified after an engine restart: the first
  "scrambled" result was a generator bug seen through the engine's model cache), so **players stay on SMD**. Fixed on the way: KV3 types 26/27 (`UINT32_ZERO/ONE`, every freshly
  compiled model failed to decode) and `compile_map` closing `failed` on a good compile.
- Next (was, before the spike): I0 spike (game up, TOOL=hammer then TOOL=modeldoc as needed), evidence
  under the worktree's `target/import/spike/`, then I1.
