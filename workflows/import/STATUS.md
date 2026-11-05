# Model import: status (attachments, 2026-11-03)

Branch `import/attachments` on top of main 48364d6 (I8). Plan: [PLAN.md](PLAN.md); open items and every inference:
[FIXME.md](FIXME.md); the agent-facing overview: `hammer-mcp/AGENT.md` "Model import overview".

| WP | Delivered | Commit | Live |
|---|---|---|---|
| I0 | spike: SMD route, Rz(-90) pre-turn, `func_instance` excluded, absolute face axes need a bridge op | e3be6aa | yes (SPIKE-I0.md) |
| I1 | `model_import` prop/world: glTF -> SMD + vmat + vmdl, fbx/obj/smd pass through, compile | 6e25056, da51ed7 | yes |
| I2 | `report_ready` `prop_materials`, `materials:"existing"` + `materialMap` | 556a89a | yes |
| bridge m3.18 | `node.createPrefab`, `node.prefabFromSelection`, `mesh.setFaceTexture mode:"axes"` | 03a2c5c, c1eb5ae, ba1fdef | yes |
| I3 | `kind:"hammer_mesh"` (planar merge, chunks, exact UV axes), `align_texture mode:"axes"` | 1e6fce9 | yes |
| I4 | `prefab create/instance/list/read`, `CMapPrefab` in `src/vmap` | 1ed1341 (7ca2bda) | yes |
| I3/I4 review | pairing by corners+winding, overwrite, single-step undo note | fcc6ce5 | partly |
| I5 | skinned glTF: skeleton, weights, clips -> rigged `prop_dynamic` | 2a1de1f, cff4355, 6ed6e97 | yes |
| I6 | `kind:"player"`: retarget onto the CS2 skeleton (vnmskel), AnimGraph2; `agents/models/**` paths | f00bb31, 5a7df28, 23fc89a, c85d87f | yes (Mixamo 18/18) |
| I7 | `player_model` (cs_script `SetModel` + point_script + hidden precache props); bridge route deferred | 2669410, 2dab986, f5ce5b1, c223df1 | yes for `any` + bots; `ct`/`t`, `bots:false` unproven |
| I8 | playbook `import_model`, overview docs, `look` on a prefab nodeID, `model_compile` engine-cache note, budget +202 B | 48364d6 | offline only |
| attachments | player: reference agent's 17 attachments + `cstrike` hitboxes, all 75 bones kept (`BoneMarkup`), mixed-case names (`RenameBone`), `worldmodel` graph identifier; `referenceAgent`; budget +170 B | branch import/attachments | yes (pawn holds the pistol, hitbox overlay) |

## Open items

- Player model: weapon attachments + hitboxes done (FIXME "Attachments"); hit registration by gunfire untested; no
  facial bones, no first-person arms (`hudmodel` graph), no ragdoll (FIXME I6).
- Engine model cache: a recompiled model shows only after a tools-session restart and a recompiled map (documented
  everywhere an agent looks).
- `look` on a prefab frames origin +-64 only and needs the map saved; no targetname lookup (FIXME I8).
- Skinned: one skin, 4 influences, no morphs, no non-uniform joint scale, one rigid collision hull (FIXME I5).
- hammer_mesh: no smoothing groups, no displacement; a chunk is two undo steps; no resume after a partial failure (FIXME I3).
- Deferred: the bridge route `game.setPlayerModel` (needs its own RE brief); prefab attributes (`fixupEntityNames`, ...)
  are not settable (FIXME I4).
- The `import_model` playbook has not been walked by an agent against a live Hammer (FIXME I8).
