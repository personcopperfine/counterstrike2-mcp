# Model import — hacks, gaps, throwaway helpers

## Spike I0 (2026-10-29)

- HACK (not in git): `target/import/spike/kv3probe.rs` was a scratch `hammer-mcp/examples/` binary (dump
  every KV3 block of a file or `vpk:<dir.vpk>!<path>`); moved out of the crate after the spike. If a
  `.vmdl_c`/`.vnmskel_c` dump tool is needed again, make it a real `get_model_info include:` option instead.
- HACK (not in git): `target/import/spike/gen_smd.py`, `gen_nm_smd.py`: hand-written SMD generators for the
  spike; superseded by `hammer-mcp/src/import/smd.rs` (I1) and the DMX writer (I6).
- LEFTOVER: spike models in the test addon `accept_p4/models/import_spike/` (content + game). The map
  `accept_p4/maps/convoy_slice.vmap` was restored from its pre-spike copy; the edited one is
  `target/import/spike/convoy_slice.spike-edited.vmap`. The compiled `convoy_slice.vpk` still has the
  spike props until the next `compile_map`.
- GAP: `get_model_info` takes only `models/…` paths (agents live in `agents/models/…`).
- GAP: `look` frames a custom prop with a generic ±64 box and a corner pose (landed outside the map); an
  explicit `pose` was needed.
- GAP: a running engine keeps a loaded `.vmdl_c` even across `run_map` (corrected in I1 live: only a tools
  session restart shows a recompiled model).
- GAP (bridge): `mesh.setFaceTexture` has no absolute-axes mode (I3 blocker, m3.18).

## I1 (core static import)

- VERIFIED LIVE 2026-10-29: shader `csgo_complex.vfx` with `TextureColor` + `g_vColorTint` renders (texture
  and tint-only materials); `remaps` `from`/`to` works; the vmdl compile builds the `.vmat_c`/`.vtex_c`
  dependencies. Still unverified: `TextureNormal`, `TextureRoughness`, `TextureMetalness` (the live GLB had none).
- VERIFIED LIVE: the compiler turns SMD content by Rz(+90); `smd::to_smd` pre-turns by Rz(-90), so the
  `(x,y,z)_gltf -> (z,x,y)_cs2` map holds in game (front marker at +X, left marker at +Y).
- GAP: the metallicRoughness texture is not split (G roughness / B metalness); needs pixel decoding (`png`/JPEG)
  or a DMX/vtex route. Normal maps are not converted (glTF OpenGL green channel).
- GAP: metallic/roughness factors without a texture, emissive, occlusion, alpha mode, doubleSided are dropped
  (reported in `dropped`).
- GAP: missing NORMAL gets smooth normals generated per vertex (no unwelding for flat shading).
- GAP: a pass-through file (fbx/obj/smd) is not read: no tri counts, no `TooLarge`, no material names.
- HACK: base64/percent decoding are small local functions in `import/gltf_read.rs` (avoids a direct `base64`
  dependency); replace if more URI cases show up.
- DONE in I2: `report_ready` `prop_materials` (see hammer-mcp/AGENT.md).

## I1 live (2026-10-29)

- DONE 2026-10-29 (I2 live): the test props (nodeIDs 145-149) were deleted from `accept_p4/maps/convoy_slice.vmap`
  and the map saved; the compiled `convoy_slice.vpk` keeps them until the next `compile_map`. The models stay
  in the addon (`models/import_live`, `import_existing`, `import_spike`).
- GAP: `create_props` and `get_model_info` refuse `agents/models/…` (only `models/…`); `create_entities` with a
  `model` keyvalue works.
- DONE (I8): `model_compile` now notes that a running engine keeps a loaded model (restart the tools session, recompile
  the map) and offers `compile_map` as `next`; same wording as `model_import`'s `look` step.

## I2 (materials proof)

- GAP: `prop_materials` does not open shipped models (model-level only); a shipped model's own missing material
  is not detected. Models inside `game/csgo` loose dirs count as shipped.
- GAP: it reads only `model` keyvalues of entities in the saved `.vmap`; `.vmdl` references from `.vsnd`/scripts
  or an instance of another map are not followed.
- INFERRED (live test must confirm): a compiled `.vmdl_c` lists its `.vmat` remap targets in `RERL` (the check
  relies on it; if only the default materials are listed, a missing remap target passes silently).
- GAP: `materialMap` on pass-through formats cannot be validated against the file (not read); a wrong source name
  silently does nothing besides the warning.
- HACK: `check_prop_materials_in` (doc(hidden) pub) exists only so tests can hand-build a `Document`.

## I2 live (2026-10-29)

- VERIFIED LIVE: `prop_materials` passes on 4 addon + 1 shipped model; hiding a `.vmat_c` fails with
  `material … missing` + `compile_asset`, hiding a `.vtex_c` fails with `texture … missing (of …vmat)`.
- VERIFIED LIVE: a compiled `.vmdl_c`'s RERL lists its remap targets (shipped `materials/dev/*.vmat` for a
  `materials:"existing"` import), so the check sees them. `UnknownMaterial` suggests `reflectivity_30/10/20`
  for a typo `reflectivity_3` against the real pak.

## Bridge m3.18 (prefabs + absolute face axes)

- DONE 2026-10-29: `mesh.setFaceTexture mode:"axes"`, `node.createPrefab`, `node.prefabFromSelection` live-verified
  (the m3.18 note's "Live result"; contract: `crates/cs2-bridge/PROTOCOL.md` §11.4, §11.5, §12.6); installed in the Wine tools tree (m3.17 backup:
  `target/import/cs2_bridge.m3.17.dll`).
- HACK (not in git): `target/import/live_m318.py`, a raw bridge client for the three ops (needs the MCP session
  stopped: the bridge takes one client).
- LEFTOVER: `accept_p4/maps/m318/two_boxes.vmap` (prefab file written by the live test; the nodes were deleted from
  `convoy_slice.vmap` and saved). Reuse as the I4 fixture or delete.
- GAP: `mesh.get` has no per-face texture read, so "other faces untouched" after `axes` is not checked live.
- GAP: `node.prefabFromSelection` refuses a missing directory; the I4 tool must create it.

## I3 (world-as-meshes, `kind:"hammer_mesh"`)

- INFERRED (live test must confirm): normalised UV = Hammer texel / texture size, with the texel formula
  `dot(p, axisU.xyz)/scaleU + axisU.w` verified in the RE (`workflows/streamline/re/wp15-re.md`) but the divide
  by the texture size only inferred. Look for tiling that is off by a power of two.
- INFERRED: V runs down the image in Hammer (default floor axes U=+x, V=-y), the same as glTF, so glTF UVs are used
  without the `1 - v` flip the SMD needs. A vertically mirrored texture on a live face means this is wrong: flip
  `v` in `planar::build` (one line) and the axes.
- INFERRED: the compiled `.vtex_c` keeps the source PNG/JPEG pixel size (the axes divide by it). A compiler that
  resizes/clamps big textures would skew the tiling.
- INFERRED: Hammer's face index (`mesh.setFaceTexture` / `mesh.get`) for a fresh mesh follows creation order. The
  tool does not rely on it: it pairs by face centroid via `mesh.get` (`chunk::match_faces`, 0.02 unit tolerance,
  same corner count); the creation-order fallback (no `mesh.get`, or `MeshTooLarge`) is only a warning.
- GAP: a chunk is two undo steps (create + align), not one undo group (the bridge has no batch for
  `mesh.create` + `mesh.setFaceTexture`). `undo_last` twice removes one chunk.
- GAP: no resume after a partial failure; the created nodes are listed and must be deleted.
- GAP: `textureSize` cannot be given for `materials:"existing"/"none"` (assumed 512x512, warned). A `textureSize`
  arg or a lookup of the shipped `.vtex_c` header would fix it.
- GAP: no smoothing groups / normals, displacement, instancing; vertex colours, UV set 1 and alpha modes are in
  `dropped` as for props. The face's texture is written to UV set 0 only.
- GAP: UV offsets are not wrapped into the first tile (large UV ranges give large texel offsets in f32; error is
  ~1e-3 texels at 5e4 texels, fine, not tested beyond that).
- HACK: texture axis numbers are sent as the shortest decimal of the f32 value (`chunk::f32_json`) to halve the
  request size; the bridge stores f32, so `exact` still holds when the decimal round-trips (rare edge: a decimal
  next to an f32 rounding boundary).
- HACK: region growth caps (`MAX_REGION` 64 triangles, `MAX_LOOP` 32 corners) keep the merge linear; a large flat
  floor becomes many faces of up to ~8x8 quads rather than one polygon.
- HACK: corners within sin 1e-5 of straight are dropped from merged faces (T-junction of float noise); the same
  tolerance admits a nearly straight corner as convex.
- HACK: a face that collapses when rounded to 4 decimals / f32 (or whose Newell area is < 1e-4) is dropped, not
  sent, because `create_mesh`'s gates would refuse the whole chunk for it.
- GAP: `materials/models/<name>/` is the materials folder for world meshes too (reused from the prop import so the
  materials are identical); a world-specific folder (`materials/<name>/`) would be tidier.
- LIVE CHECKS owed: 5k-triangle glTF, `look` shows the textures aligned, `undo_last` twice removes the last
  chunk, `exact:true` on every chunk, Hammer does not weld/drop merged polygons (a `faces` count mismatch shows in
  `warnings`), a face with a collinear-free 16-corner polygon is accepted by `mesh.create`.
## I4 (prefab)

- INFERRED (live check): the `CMapPrefab` element layout in `src/vmap/prefab.rs`. No Hammer-saved `.vmap` of
  any installed tree contains a `CMapPrefab` (`accept_p4/maps/convoy_slice.vmap` no longer has the nodes;
  `maps/m318/two_boxes.vmap` is a prefab TARGET). Names are the hammer.dll ones (`re-prefab.md`: `CMapPrefab :
  CMapInstance : CMapNode`, attributes `targetMapPath`, `fixupEntityNames`, `loadAtRuntime`, `loadIfNested`,
  `useTargetnameAsPrefix`, `entityIOTargetName`, `prefabRuntimeEntity`) with the node attributes (`nodeID`,
  `origin`, `angles`, `scales`, `children`) spelled like the other node classes. The reader treats every
  attribute as optional, so a different spelling lists the node with `targetMapPath:""` instead of failing;
  `prefabRuntimeEntity` is not listed (type unknown). Verify by `prefab instance` + `save_map` + `prefab read`
  / `list_entities` on the saved map.
- INFERRED (live check): whether Hammer stores the loaded target's nodes as `children` of the saved
  `CMapPrefab`. If it does, `list_entities`/`entity_view` would list the prefab's entities as if they were the
  map's own (the entity view walks every element with `entity_properties`). Check on the saved map after
  `instance`; if so, exclude descendants of a `CMapPrefab` from the entity and mesh views.
- OPEN LIVE QUESTION: does `compile_map` resolve and bake a prefab whose `targetMapPath` is
  `maps/prefabs/...` (expected yes)? And does the baked map contain the instanced entities twice with the
  same targetname (fixup/prefix: `fixupEntityNames` is left at Hammer's default, not settable here)?
- INFERRED: `selection.set {op:"replace", nodeIDs}` keeps the current selection mode; `create` over a mix
  of meshes and entities relies on the bridge's `selection_mode` gate (groups/objects/meshes). A refusal comes
  back as the bridge error, with the selection already changed (`previousSelection` is reported only on
  success and on `SelectionNotApplied`).
- GAP: `create` does not restore the previous selection (the originals are deleted by Hammer); it reports it.
- GAP: no `list` of prefabs referenced by the open map (`CMapPrefab` nodes of the saved file whose target lies
  outside `maps/prefabs/`); no live `node.list {class:"prefab"}` count in the live `get_scene_info` route.
- GAP: `fixupEntityNames`, `loadAtRuntime`, `targetname` prefix and the other prefab attributes cannot be set
  (the bridge leaves them at Hammer's defaults, `re-prefab.md`); `instance` takes no `targetname`/`fixup`.
- GAP: `prefab create` does not write the prefab into a sub-directory (`name` has no `/`); `instance` and
  `read` accept any addon `maps/....vmap` path.
- GAP: the name charset (`[a-z0-9_-]`, lowercase) is a choice, not a Hammer rule.

## I3 + I4 live (2026-10-29, merged import/models 7ca2bda, bridge m3.18)

Evidence in `target/import/live3/` (gitignored): `gen_terrain.py` (HACK: 5000-tri terrain GLB, flat plateau, UV
tiling 4x, checker with a yellow uv(0,0) corner and a red +u bar), the tool replies, `topdown_pair.png`.

- VERIFIED LIVE: `model_import kind:"hammer_mesh"` of the 5k-tri GLB: 4 chunks, 4576 faces (plateau merged),
  `uvExact:true`, no warnings. Top-down renders of the Hammer meshes and of the same GLB imported as a prop
  (SMD path) are identical (corner positions, bar direction, checker phase): the texel/texture-size divide, the
  V orientation and the `.vtex_c` keeping the PNG size all hold (the three I3 INFERRED items above).
- VERIFIED LIVE: two undo steps per chunk (align, then create) remove a chunk (live mesh count back to saved + 3).
- VERIFIED LIVE: `prefab create {nodeIDs:[prop_static, light_omni2], centered:true}` -> `maps/prefabs/terrain_lit.vmap`
  written + loaded; two `instance` calls (one turned 90°); after `save_map`, `list_entities` lists the three
  `CMapPrefab` nodes with `targetMapPath`, origin/angles and flags read from the Hammer-saved file (the layout
  inference holds), and the prefab contents are NOT listed as the map's own entities (no `children` problem).
  `prefab list` counts the file; `compile_map` OK (0 failed) and `run_map` shows the instances in game.
- DONE (I8): `look {target:<prefab nodeID>}` now resolves the prefab (see `## I8`); it used to refuse ("not an entity").
- LEFTOVER: `accept_p4` models `terrain_prop`, materials `models/terrain_mesh`, prefab `maps/prefabs/terrain_lit.vmap`
  (reuse as fixtures or delete); the test nodes were deleted from `convoy_slice.vmap` and saved, but the compiled
  `convoy_slice.vpk` still has them until the next `compile_map`.
- Not checked live: a 16-corner polygon, duplicate targetnames across instances (no named entity in the prefab),
  `centered:false`.
- FIXED 2026-10-29 (review + live): a byte-identical rewrite of a generated file bumped its mtime, the compiler
  skipped it (same content) and the skip judge called the compile failed (`CompileFailed` on a re-import with
  `overwrite:true`). Generated files are now written only when their bytes differ (`model_import::write_generated`),
  and `overwrite:true` compiles with `-f` (prop and hammer_mesh). Live re-check: hammer_mesh overwrite OK, 4 chunks,
  `uvExact:true`, every face paired by corners + winding.
- HACK/NOTE: never start `cargo bridge runtime up` from a shell that ran `ulimit -v` (the OOM guard): Wine inherits the
  cap and the engine dies with `mmap() Cannot allocate memory` (the VToolMessage channel never appears). Keep the
  ulimit inside a subshell around cargo.

## I5 (skinned glTF, own rig; offline, nothing run live yet)

- LIVE CHECK OWED (all `[I]`): import a skinned, animated GLB, compile, place as `prop_dynamic` with `StartingAnim`,
  `look {mode:"frames"}` shows it moving as in a glTF viewer, +X front. Specifically unverified in game:
  (1) that the Rz(+90) compiler turn applies to bone *poses* of a multi-bone SMD exactly as to the single-bone spike
  (the spike's `spike_tube` was 2 bones and animated, so likely); (2) bone names reach the model lowercased as written;
  (3) `AnimFile` `name` = the sequence name `StartingAnim` expects; (4) `StartingAnim`/`StartingAnimationLoopMode`
  are accepted through `create_props` `keyValues` (FGD validation of those keys); (5) the pose of a root bone that
  carries the pre-turn Rz(-90) in its rest rotation (the animation's non-root bones are parent-relative, so unaffected).
- GAP: SMD has no scale: non-uniform joint scale, or a scale that changes after bind, is dropped (listed); a
  scale animation therefore does nothing unless it equals the bind scale.
- GAP: collision is one rigid hull of the bind pose (no per-bone hulls / physics bones).
- GAP: only the first skin; more than 4 influences per vertex lose the weakest (JOINTS_1 dropped); morph targets and
  node animation of unskinned meshes dropped; CUBICSPLINE tangents ignored (key values only).
- GAP: clips start at their first key (a clip whose first key is at t=2 s begins there, not at 0 with a hold).
- INFERENCE: nodes that are not joints but sit between joints are baked into the child's world, so a hierarchy of
  `joint -> non-joint -> joint` becomes a direct bone parent link with the intermediate transform folded into the
  child's local; animated non-joint nodes are sampled the same way.
- INFERENCE: a weightless skinned vertex binds to the root bone; a skinned primitive without JOINTS_0 is rigid.
- HACK: none (no scratch generators; the Euler step is covered by a round-trip test over +-90 deg pitch).

## I5 live (2026-11-03, bridge m3.18)

- VERIFIED LIVE: `target/import/live5/gen_rig.py` (HACK, gitignored: 3-joint tube up glTF +Y, red nose at glTF +Z on
  the base joint, clip `bend` turns mid+tip about +X to 45° and back over 2 s) -> `model_import` (3 bones, 61 frames),
  `create_props prop_dynamic` with the `next` keyValues (`StartingAnim:"bend"`, looping), `compile_map` OK, `run_map`:
  in-game `look` frames show the tube bending progressively toward +X, the nose side (`frames.png`). The compiler's
  Rz(+90°) turn and the pre-turn hold for a multi-bone animated SMD; `AnimFile` name = sequence name; FGD accepts
  the keyValues. Not checked live: non-joint animated ancestors, CUBICSPLINE, the 4-link weights, name collisions.
- OBSERVED ONCE: the first import after `open_map` reported `compile: {ok:false, error:"InternalError",
  errorType:"ProtocolError"}` ("not compiled: InternalError"); `model_compile` of the same file right after and a
  re-import both compiled OK. Likely the first bridge `job.*` request right after a map load; not reproduced.
- FIXED: the prop path warned "replaced existing" for byte-identical files (not rewritten since 72510fb); it now
  warns only for files whose bytes differ.
- LEFTOVER: `accept_p4/models/rig_tube` (content + compiled); the prop node was deleted from `convoy_slice.vmap`.

## I5 review fixes (branch import/i5fix, all offline-tested in `hammer-mcp/tests/model_import_rig.rs`)

- FIXED: node cycles / a node with two parents are refused (`ImportParse`, naming the node) before any walk; the
  parents-first walk has a visited set and the import errors if it misses a node (joint-only cycles).
- FIXED: WEIGHTS_0 summed in f64 (an f32 sum of values near 3e38 was inf); a non-finite or <=1e-6 sum is unweighted
  (rigid to the root bone, counted in the warning); `write_rigged` falls back to bone 0 (the single root) when a
  vertex has no positive link instead of indexing an empty list.
- FIXED: the sample cap (2M `frames * joints`) is cumulative over all clips, and the frame count is range-checked in
  f64 before the `usize` cast (a key at 1e30 is refused). CHOICE: the error is `ImportParse`, not `TooLarge`, because
  `TooLarge` is worded in triangles.
- FIXED: non-finite keyframe values are refused like times; rotation keys are normalised when read (zero = identity).
- FIXED: rest locals use the node's exact `matrix()` (shear, zero axes); decomposed TRS is used only for nodes an
  animation targets (a non-finite decomposition falls back to identity pieces). A zero-axis ancestor now yields finite
  output with the joint's scale reported as dropped; its rotation is arbitrary (Gram-Schmidt on a degenerate axis).
- FIXED: `frame_of` returns a signed scale (negative for a reflection), so a mirror that appears only after bind fails
  the bind/pose ratio test and is listed under "scale dropped" instead of silently rotating wrong.
- FIXED: the last frame of a clip samples exactly the last key (keys 0 and 0.04 s: 2 frames, the second at 0.04).
- FIXED: JOINTS_0 is checked against the skin's own joint count (synthetic root excluded). CHOICE: refuse the file
  (`ImportParse`) rather than bind the vertex to the root.

## I6 (player retarget, pure Rust; offline, nothing run live yet)

- LIVE CHECK OWED (all `[I]`): import the procedural humanoid (`hammer-mcp/tests/common/humanoid.rs`, saved as a GLB) as
  `kind:"player"`, compile, place as `prop_dynamic` beside a real `agents/models/tm_phoenix/tm_phoenix.vmdl`: same pose
  under the graph, same facing (+X front), proportions plausible. Unverified in game: (1) that the mesh in vnmskel
  model space with no Rz(-90) pre-turn faces like the agent (SPIKE-I0 verified the stick figure; a textured, skinned
  mesh with real weights is new); (2) `prop_dynamic` needs no `use_animgraph`/`animgraph2_identifier` keyvalues
  (the FGD default of `use_animgraph` is 1; the spike placed the stick figure through `create_entities` without them);
  (3) the SMD importer lowercases bone names and the weights still bind by name (spike: yes for the stick figure);
  (4) the roll/stretch math on a real character (arms in A vs T pose), hands and heads orientation.
- INFERENCE: the real `.vnmskel_c` stores `m_parentIndices` as numbers the loader reads through `as_f64` (int or
  double); `m_ID` is read when present. The fixture encodes the numbers as doubles; the `#[ignore]`d test
  `the_installed_skeleton_matches_the_fixture` proves the real file decodes and equals the embedded numbers (run it
  with the game installed: `cargo test -p hammer-mcp --test model_import_player -- --ignored`).
- INFERENCE: every vnmskel bone points along its rest frame's +X (the right side mirrored, -X); verified numerically on
  the 75 poses from the dump (dot with the child direction 1.0 for every chain bone, 0.97 for the hands). Terminal
  bones rely on it.
- CLOSED (FIXME "Attachments"): no hitboxes. `HitboxSet_Auto` is in `modeldoc-nodetypes.json` but with no property list; an empty `HitboxSet`
  is emitted (as the spike). Players shot at via a real pawn (I7) need hitboxes: look at what `tm_phoenix.vmdl`
  carries (`model_read`) and copy it.
- GAP: no facial/eye/jiggle/twist bones, no weapon attachments (`AttachmentList` empty), no first-person arms, no
  ragdoll/physics (default `physics:"none"`).
- GAP: VRM mapping is by bone NAME only; the VRM `humanoid.humanBones` extension (node index per humanoid bone) is
  not read, so a VRM with non-standard node names needs `boneMap`.
- GAP: the structural fallback assumes glTF conventions (front +Z, up from the trunk) for left/right and cannot
  place `ball_*`/fingers; a head chain that ends in a head-top end marker is recognised only when that last segment is
  under half the previous one or the head branches (eyes), else the marker is taken for the head (use `boneMap`).
- GAP: a rig with a Z-up skin matrix still works (axes are derived from the data), but a mirrored (left-handed) rig is
  not detected: the conform rotation is always proper, so mirrored characters would face wrongly.
- CHOICE: the `agent` argument of the plan was dropped (the skeleton and AnimGraph2 are shared by every agent); naming
  it is the dispatcher's "unknown argument" refusal. `upAxis` with kind player is refused (derived from the rig).
- CHOICE: the model lands in `models/<name>/` (not `agents/`); `get_model_info`/`create_props`/the catalog also accept
  `agents/models/**` so a shipped agent can be placed beside it. `list_models` without a prefix now lists the
  `agents/models/**` entries too.
- GAP: `clavicle_*` and `neck_0` are required and one source joint fills one role, so a rig whose upper arm hangs
  straight off the chest (no clavicle) or with no neck cannot import even with `boneMap`. Fix: make them optional and
  synthesise them (CS2 rest position, the chain predecessor's Q, ratio 1, no weights). (Opus review 2026-11-03, low.)
- CHOICE (Opus review 2026-11-03): the stretch is limited to the bone's own segment (the CS2 pelvis -> spine_0 is under
  an inch, so a whole-axis stretch squashed the hips ~5x); joints closer than `max(5% of the CS2 length, 0.2% of the
  torso)` are coincident, not a direction; a terminal bone takes the source child closest to its CS2 +X after the
  predecessor's rotation, else inherits it (hair/jaw/prop joints no longer tip the head); a chain running up the
  source hierarchy is refused. The live box humanoid predates these; the bind math changed only for the hips region
  and normals.
- CHOICE: the confidence number is a heuristic (name/explicit = 1, structural = 0.7, over the 18 required roles).
- HACK: none (the vnmskel numbers in `tests/common/vnmskel_data.rs` were generated once from the SPIKE-I0 dump by a
  throwaway script, not kept).

## I6 live (2026-11-03, bridge m3.18)

Evidence in the `import` worktree's `target/import/live6/` (gitignored): `gen_humanoid.py` (HACK: 25-bone Mixamo T-pose
humanoid, left limbs blue, right red, yellow nose at glTF +Z), the import replies, `compare3.png`, `seq.png`.

- VERIFIED LIVE: `model_import kind:"player"` maps the Mixamo rig 18/18 required (confidence 1.0), compiles with the two
  Nm nodes; `the_installed_skeleton_matches_the_fixture` passes against the real pak. As `prop_dynamic` with empty
  keyValues (no `use_animgraph`/`animgraph2_identifier` needed) beside a real `tm_phoenix` and the I0 stick figure, the
  graph drives it: same stance, same facing, coherent motion over 6 frames, no exploding vertices. Bone names bind
  after the compiler lowercases them.
- FIXED LIVE: the first build wrote the mesh/skeleton with no Rz(-90) pre-turn (the I0 rule as handed over). In game the
  graph hides it (it poses the bones in skeleton space), but Hammer's viewport showed the compiled bind turned a quarter
  against the agent. The player SMD now gets the pre-turn on the root bones, vertices and normals
  (`bake::pose_lines_turned`); Hammer shows the bind on the agent's A-pose, facing the same way.
- OBSERVED: at one moment, the arms of the import, phoenix and the stick figure differ in detail (the idle graph
  fidgets are not synchronised between props); not a retarget defect as far as a box figure can show. A real
  character mesh (I8 or the user) is the next check for roll/stretch quality at hands and head.
- FIXED: a partial finger chain was aligned by count (Mixamo `HandMiddle1` alone -> `finger_middle_2_*`, rigify
  `thumb.01` alone -> `finger_thumb_2_*`); joints now keep their table-row position (at least their rank among the
  found ones, so the merged VRM 0/1 thumb row still fills from the base). Test `a_partial_finger_chain_keeps_its_segments`.
- LEFTOVER: `accept_p4/models/humanoid_player`; test nodes deleted from `convoy_slice.vmap` (saved).

## I7 spike (2026-11-03, cs_script, no bridge change)

Evidence in the `import` worktree's `target/import/live7/` (gitignored): `player_model.ts` (HACK: the spike script,
`OnPlayerReset` -> `QueueAfterThinks` -> `pawn.SetModel(...)` + a position/model log), `error_model_no_precache.png`,
`pawn_front.png`, `pawn_back.png`.

- VERIFIED LIVE: the game's own script API `BaseModelEntity.SetModel` (point_script.d.ts) on the real CT pawn of a
  wine-tools session (`launch_session {kind:"wine-tools", addon:"accept_p4", map:"convoy_slice"}`, `map_workshop`) puts
  the I6 humanoid on the player: AnimGraph2 keeps driving it (pose changes frame to frame), it faces forward (nose
  front, its right arm on its right), no exploding vertices, the model sticks (logged every tick). No RE was needed
  for this route.
- VERIFIED LIVE: **precache is required.** Without the model in the map's resource manifest the console prints
  `RESOURCE_TYPE_MODEL resource '...' requested but is not in the system (Missing from a manifest?)` then
  `ERROR: ... requested is not loaded and may have been deleted`, and the pawn shows the engine's ERROR model. A
  `prop_dynamic` of the model in the map fixes it (the compiled map lists it). A bridge `game.setPlayerModel` would
  need the same: a map prop, or a precache hook (`IEntityResourceManifest` add during the server's precache, the
  CS2Fixes approach) for maps that don't carry it.
- VERIFIED LIVE: Hammer's in-tools `run_map` is NOT a test bed for the real player: it loads models on demand (hides
  the precache error), the local pawn stays at (0,0,0), and `Instance.Delay(3)` fires every ~30 s.
- GAP (seen live): the pistol lies at the feet and the arms keep the idle/A-pose instead of holding it: the imported
  model has no weapon attachments (`AttachmentList` empty, I6 GAP) and presumably no hand IK targets. Copying the
  agent's attachments (and hitboxes) is the next I6/I7 item.
- OBSERVED: `+back` through console_exec does not move the parked harness player; locomotion is unchecked.
- LEFTOVER: `accept_p4` cs_script `player_model` (src + `maps/scripts/player_model.*`), `convoy_slice.vmap` point_script
  `i7_spike_script` (nodeID 145), props `i7_precache_humanoid` (146), `i7_ref_phoenix` (147); map saved + compiled
  with them.

## I7 (`player_model` tool, cs_script route)

Code: `hammer-mcp/src/tools/player_model.rs`, tests `hammer-mcp/tests/player_model.rs`.

- VERIFIED LIVE (2026-11-03, bridge m3.18; `target/import/live7/tool_pawn.png`, `tool_precache_origin.png`):
  `player_model {action:"set", model:"models/humanoid_player/humanoid_player.vmdl"}` in one call wrote/built/compiled
  the script and created `player_model_script` + `player_model_precache_0` at [0,0,0]; after save_map + compile_map, a
  wine-tools `map_workshop accept_p4 convoy_slice` session's CT pawn wears the model (no manifest warning, no ERROR
  model, the graph drives it), and nothing is visible at the origin (standing at x=45 looking at it: the prop there,
  half inside the wall face, shows no part). Solidity not probed. The `any` slot with `bots:true` is what ran; `ct`/`t`
  and `bots:false` remain unproven live.

- INFERENCE (precache prop keyvalues): `solid 0`, `rendermode kRenderNone`, `disableshadows 1`, chosen from the real
  `game/core/base.fgd` (`prop_dynamic` = `prop_dynamic_base` + `EnableDisable` + ...): `solid` choice 0 "Not Solid",
  `RenderModeChoices` `kRenderNone` "Dont Render", `Studiomodel.disableshadows` 1 "No Shadows". Invisible verified live
  (above); non-solid not probed. The `model` keyvalue is
  the only thing the compiler needs for the manifest. `StartDisabled` (EnableDisable) is deliberately not used: the FGD
  gives no evidence that a disabled entity still precaches. If `kRenderNone` ever stops the manifest listing, try
  `StartDisabled 1` instead.
- GAP (live): unproven in a game: the generated script's team/bot filter, `bots:false`
  (`player.GetPlayerController().IsBot()`), `ct`/`t` slots (team numbers 3/2 are from point_script.d.ts), the
  `point_script` created at [0,0,0] (or at the existing `player_model_script`'s origin).
- DECISION: no cs_script project -> `NoProject` with `next` cs_script init; `player_model` never runs `npm install`
  itself. An existing project whose `node_modules` is missing is installed by `cs_script build` (as for any script).
- DECISION: a hand-written `cs_script/src/player_model/index.ts` (no generated first line) is never overwritten
  (`ScriptNotOurs`). The I7 spike leftover in `accept_p4` is exactly such a file: move or delete it before using the
  tool there. Its `i7_spike_script` / `i7_precache_humanoid` / `i7_ref_phoenix` map entities are not ours (other
  targetnames) and stay; a map holding both the spike script and `player_model_script` would run two handlers.
- DECISION: one `bots` flag for the whole script; an omitted `bots` on `set` keeps the stored value (default true).
- HACK (mock): the shared mock bridge's `ENTITY_CLASSES` gained `point_script` (a real `csgo.fgd` PointClass) so the
  fake bridge accepts the create.
- GAP (model): attachments / hand IK of the imported model are still missing (I6/I7 spike GAP), so the pawn holds no
  weapon visibly; unchanged by this tool.
- LIMIT (review): a `point_script` that someone renamed away from `player_model_script` is not seen by the tool (exact
  targetname lookups); with `maps/scripts/player_model.vjs` still on it the game runs a SECOND handler. Our precache
  props are looked up as `player_model_precache_<k>` for k below the model count + 8, so leftovers numbered beyond that
  are missed. Lookups are exact on purpose: a whole-class scan is cut at the bridge's 2000 rows (`truncated`).
- LIMIT (review): with `bots:false` a pawn whose `GetPlayerController()` is `undefined` is treated as human (gets the
  model).
- DECISION (review): `clear` to an empty rule set removes the map entities FIRST and runs write/build/compile as best
  effort (reported under `scriptSteps`); a failed sync reports what it did (`created`/`createdNodes`/`fixed`/`deleted`)
  and suggests `open_map` only for a missing document. A marked `index.ts` with an unparseable rules const is
  `ScriptDamaged` for `status` and is rewritten by `set`/`clear` (the marker proves ownership); a marked file with no
  rollup target gets only the target back.

## I8 (docs, playbook, look on prefabs)

- DECISION: `look` resolves a prefab node ONLY from the saved active `.vmap` (the live `entity.get` answers "not an
  entity" for a `CMapPrefab`, and `node.list` has no prefab data we rely on): a prefab made seconds ago needs `save_map`
  first, and the refusal says so. The box is origin +-64 (the point-entity rule); the prefab's contents bounds would need
  the prefab file's own mesh/entity extents (not read: `look` stays cheap), so a large prefab is under-framed; pass a box.
- GAP: no targetname lookup for a prefab (the reader keeps no targetname for `CMapPrefab`); only the nodeID.
- GAP (unproven live): the prefab `look` was tested on the mock bridge with a `cs2-dmx` fixture, not on a Hammer-saved map
  (the `CMapPrefab` layout is the one the I4 live run confirmed).
- DECISION: the `import_model` playbook picks its steps by `kind` (prop | world | hammer_mesh | player), one body each,
  all under 2500 chars; a `rigged` kind does not exist (a skinned glTF is `prop`). It has no `name` argument: the model
  stem is the result's `model` (`get_playbook` already uses `name` for the playbook).
- HACK (test): `tests/look.rs` copies the `CMapPrefab` fixture builder of `tests/prefab.rs`.
- GAP (unproven live): the playbook text was checked against the tool schemas, not walked by an agent; the player branch's
  client-mcp calls (`launch_session`, `console_exec`, `screenshot`) are another server's tools, so the offline check only
  knows their names.
- Budget: tools/list +202 B (92522 -> 92724): `look` description, `get_playbook` description and `source`/`kind`.

## Attachments (player weapon attachments + hitboxes; branch import/attachments)

Code: `hammer-mcp/src/import/retarget/agent.rs`, `vmdl/template.rs` (`attachments`, `hitboxSets`, `keepBones`,
`renameBones`, `animGraph2Identifiers`), `model_import` `referenceAgent`; tests `hammer-mcp/tests/model_import_markup.rs`.
Scratch evidence (gitignored) in this worktree's `target/attach/`: `probe/` (HACK: throwaway JSON dumper of every KV3
block of a resource, not committed), `agents/*.json` (all 92 `agents/models/**` dumps, build 2000930), `phoenix.json`,
`vnmskel.json`, `ours_before.json` (the I7 import), `cycle.sh` (HACK: scratch Hammer up/compile/down loop), `live/`
(replies, compiled dumps, vmdl variants, screenshots).

- VERIFIED (read in the data, builds 2000930 and 2000940): agents carry attachments in every `MDAT` (`m_attachments`,
  all single-influence) and hitboxes in `MDAT.m_hitboxsets`; the hitbox set `cstrike` (19 capsules) is identical in all
  80 agents (`every_installed_agent_has_the_same_hitboxes`, passes on 2000940); attachment names: CT 17 (with
  `defusekit`, `primary_smg`), T 15; `weapon` (bone `wpn`, identity), `weapon_hand_r/l` (`hand_R/L`), `weapon_center`
  (root transform, z 40), `clip_limit` identical everywhere, holster offsets per agent. Every agent bone shared with
  `worldmodel.vnmskel` has the vnmskel's parent-space rest pose within 1e-3 except `root_motion`/`pelvis`/`wpnPivot`/
  `wpn` (the agent root rest is turned 120 degrees about (1,1,1); the world frame of `pelvis` is the same).
- VERIFIED LIVE (2026-11-03, build 2000940, bridge m3.18; `live/pawn_holds_front.png`, `pawn_holds_side.png`,
  `pawn_hitboxes.png`): `model_import kind:"player"` of the live6 box humanoid (tool output only, no hand edit), then
  `player_model set`, `save_map`, `compile_map` (0 failed), then a wine-tools client session on `convoy_slice`: the CT
  pawn is animated by the graph and holds the P2000 in both hands in the aim stance, the knife sits holstered on the left
  thigh (`knife` attachment); `ent_hitbox player` draws the 19 capsules (group colours: head, chest, stomach, arms, legs)
  on the posed bones. The compiled model (read back): 75 bones with the vnmskel's case, 17 attachments (offsets and
  rotations equal to `ctm_sas`, quaternion signs aside), set `cstrike` with 19 hitboxes, `m_animGraph2Refs` `""` +
  `worldmodel`.
- FOUND LIVE, in this order (each a separate compile + client run):
  1. attachments + hitboxes alone (`live/pawn_attachments_only.png`): the compiler accepts every `Attachment`/
     `HitboxCapsule` key; the pistol floats in front of the chest, arms hang. An attachment's bone is kept (`wpn`
     appeared: 21 bones) but `wpnPivot`/`root_motion` were still discarded.
  2. `do_not_discard_skeleton_bones = true` on `NmSkeletonReference`: compile FAILS (`Failed to find compiler for file
     ...content/csgo_addons/<addon>/animation/skeletons/characters/worldmodel.vnmskel`, then `NmSkeletonReference :
     Failed to load the compiled skeleton`): the flag makes the compiler load the skeleton as a child resource and it
     only looks for a source. Dropped. `BoneMarkup {target_bone, do_not_discard: true}` per bone works (75 bones).
  3. `RenameBone` modifiers under the `RenderMeshFile` restore the mixed case (`arm_upper_L`...); still bind pose (and
     `+duck` did not crouch it: `live/pawn_renamed_no_graph_duck.png`). The pistol position was the `wpn` REST point.
  4. `AnimGraph2 {name:"worldmodel", filename}` beside the `DefaultAnimGraph2`: the pawn animates and holds the
     weapon. So the I7 report "AnimGraph2 keeps driving it (pose changes frame to frame)" was wrong for the pawn: it
     stood in the bind pose (only a `prop_dynamic` uses the default graph).
- INFERENCE: whether the `RenameBone`s are needed at all (step 4 was not tried without them). They are kept: the
  compiled names then equal the agents' and the vnmskel's.
- INFERENCE: `relative_angles` = VRF `ToEulerAngles` of the quaternion; the read-back rotations equal the agent's, so
  the convention holds for these 17. Lowercase `parent_bone`/`target_bone` resolve case-insensitively (seen: the
  read-back attachments name the renamed `hand_R`).
- INFERENCE: an empty `parent_bone` = model origin: read back as root-transform `weapon_center` with offset z 40.
- CHOICE: copy from a reference agent read from the game at import time (default `agents/models/ctm_sas/ctm_sas.vmdl`,
  the CT superset; `referenceAgent` overrides, `"none"` writes no markup) rather than a built-in table: holster offsets
  differ per agent and the pak follows game updates. The I6 CHOICE that dropped `agent` stays (skeleton/graph are
  shared); `referenceAgent` names only the markup source.
- CHOICE: hitboxes are not scaled (identical across every agent body; the import's bones have the CS2 lengths). A
  bulky/thin mesh gets standard hit areas, not its silhouette.
- HACK (approximation): `primary` sits on the agent-only jiggle bone `jiggle_primary`; it is folded into `spine_3`
  through the agent's REST pose, so the rifle-on-back no longer jiggles. A fold may not end on the root bone.
- HACK (test data): `tests/common/agent_data.rs` was generated once from the `ctm_sas` dump by a throwaway script (not
  kept), numbers and names only; `the_installed_default_agent_matches_the_fixture` (`--ignored`, passes on 2000940)
  proves it equals the real file.
- HACK (live harness): Hammer's own unclean-exit flags (`game/_toolsettings/hammer/settings.txt`, `"Closed Correctly"`
  / `"ClosedCorrectly <n>"` = false) blocked the first `open_map` on a "Recover Files?" modal after the game-update
  thread's cycle; no host input tool exists, so with no tools container running I reset them to true by hand (the
  same edit `cargo bridge runtime up` makes for pet; a copy of the file before is `live/hammer_settings.before.txt`; autosaves kept).
  Every `cargo bridge runtime down` (podman rm -f) sets them false again. FOLLOW-UP: `cargo bridge runtime up` should clear Hammer's flags like
  pet's.
- GAP (not tested): hit registration by gunfire (a bot shooting the pawn, damage per group); only the debug overlay was
  seen. `sv_showplayerhitboxes 1` drew nothing here; `ent_hitbox player` did.
- GAP: no `hudmodel` (viewmodel graph + `viewmodel.vnmskel`) or `uimodel` graph identifiers; first-person arms remain a
  GAP (I6). No `ao_proxy_capsule_list`, no `BoneConstraintList` (agents' twist/eye constraints in `m_keyValueText`), no
  physics ragdoll (`PHYS`).
- GAP: `ct`/`t` slots and `bots:false` of `player_model` still unproven (I7); only the `any` slot ran.
