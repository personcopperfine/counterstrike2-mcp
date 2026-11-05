# Tools report: hammer-mcp and client-mcp after the agent-streamline refactor

Branch `streamline/agent-ux` (tip b5f1a0d). Written from the dumps in `workflows/streamline/dumps/`:
`after-hammer.json`, `after-client.json` (initialize + tools/list of today's builds, 2026-10-27) and the
baselines `before-hammer.json`, `before-client.json` (main df1d3cc, 2026-10-26). The tables were generated
by `target/tools-report/gen.py` from those files; the status column comes from the evidence docs named below.

## Summary

Sizes are the byte length of the `tools` array as compact JSON
(`json.dumps(tools, separators=(",",":"), ensure_ascii=False)`), plus the `instructions` string from `initialize`.

| Server | Tools before | Tools after | tools/list before | tools/list after | Change | instructions before | instructions after |
|---|---|---|---|---|---|---|---|
| hammer-mcp | 80 | 99 | 93 714 B | 85 973 B | -7 741 B (-8.3%) | 1 251 B | 2 858 B |
| client-mcp | 45 | 46 | 63 279 B | 31 637 B | -31 642 B (-50.0%) | 401 B | 1 799 B |

- hammer-mcp: `hammer-mcp/tests/tools_list_budget.rs` states a WP2.2 target of 47 000 B ("about half of the
  93847 B before"; documentation only, not enforced) and enforces `MAX_TOOLS_LIST_BYTES = 85_973` (equal to
  the measured size, so the guard has no slack). The target was **not reached**: the file says the text pass
  alone cannot get there because the schema structure is about 41 KB by itself. The size was 80 679 B after
  WP2.2 and grew back as tools were added (`look` +1837 B, `report_ready` +1273 B, `get_playbook` +762 B,
  `find_assets` and the facing text +1156 B, run-4 fixes +266 B). Description caps in the same test: 700 chars per
  tool (1 100 for `play_test`, `compile_map`, `run_map`, `new_map`, `create_trigger`,
  `create_sound_entities`), 120 chars per property. The 93 714 B baseline here differs slightly from the
  93 847 B quoted in the test comment (different measurement point).
- client-mcp: roughly halved (WP2.2/2.3 text and annotation work, no budget guard cited here).
- Instructions grew on both servers on purpose (first call, intent, traps; see the dumps).
- MCP capabilities: hammer-mcp now also advertises `prompts` (before: tools only); client-mcp is tools only.

Added (20 tools). hammer-mcp (19): `set_addon`, `new_map`, `get_scene_info`, `apply_layout`,
`align_texture`, `console_exec`, `cvar_get`, `input_set`, `input_clear`, `input_sequence`, `game_state`,
`game_rules`, `entity_find`, `play_test`, `look`, `report_ready`, `find_assets`, `get_playbook`, and
`game_local` (an alias). client-mcp (1): `cvar_get`.
Removed: none on either server. Renamed: none; three old names remain as aliases (below).
Several added tools share names with client-mcp tools (`console_exec`, `cvar_get`, `input_*`, `game_state`,
`game_rules`, `entity_find`): hammer-mcp drives the tools-session engine, client-mcp
the game client, and the shared play code lives in `crates/cs2-mcp-common`.

Aliases (`Registry::alias`, listed with a one-line "Deprecated: use ..." description, annotations taken from the
target; FIXME "Play follow-ups" says to hide or remove them next release):

| Alias | Use instead |
|---|---|
| `get_entity_live` | `get_entity {source:"live"}` |
| `find_entities_live` | `find_entities {source:"live"}` |
| `game_local` | `game_state` |

MCP prompts (hammer-mcp `prompts/list`, `hammer-mcp/src/playbooks.rs`; the same texts are the tool `get_playbook`):
`blockout_map` (greybox from an empty addon to compiled, loaded and walked), `iterate_loop` (edit, save, compile
novis, run_map, play_test, look; repeat), `cs_script` (attach and test a point_script; says where the tools
stop), `particle_set` (author, validate, compile and see a folder of .vpcf effects), `release` (full compile
and the release `report_ready`). Arguments (all optional): `map` (blockout_map, iterate_loop, release), `script`
(cs_script), `folder` (particle_set).

Status counts (by the rules below):

| Server | new, live-verified | new, tested only | changed, live-verified | changed, not re-run live | unchanged | total |
|---|---|---|---|---|---|---|
| hammer-mcp | 19 | 0 | 18 | 62 | 0 | 99 |
| client-mcp | 0 | 1 | 0 | 45 | 0 | 46 |

No tool is `unchanged`: the WP2.2 pass rewrote every description and annotation set on both servers (the
"changed" parenthesis in each row says which parts differ from the baseline: text, schema, annotations).

Acceptance (ACCEPTANCE.md, a Sonnet agent with no parent, ti_convoy slice through the MCP only, fresh addon):
run 1 (after P1) blocked at step 1 after about 13 calls with 1 GAP (no tool created a map); run 2 (with
`new_map`) completed the map but needed 2073 tool calls, 2019 of them `compile_map {jobId}` polls, about 54 real
work, 2 GAPs; run 3 (after live-fix D) passed with 79 calls (9 `compile_map`, 14 `play_test`, 9 `entity_find`,
10 `run_map`, 12 `game_state`), 0 GAPs; run 4 (after P3) passed with 25 calls (2 `compile_map` waits), 0 GAPs,
1 ERROR (`compile_map {waitMs}` without a jobId was refused; fixed afterwards), and it stopped on the verdict of
`report_ready`. Commands outside the MCP: 0 in every run. A P5 live smoke (2026-10-27) then exercised
`find_assets`, `compile_map {waitMs}` start-and-wait, `report_ready`, `look {target:"map"}` and `get_model_info`.
No acceptance run exists after P4 (`get_playbook` and the prompts) or P5; the orchestrator ran a live smoke of `get_playbook`, `find_assets`, `compile_map {waitMs}` on start, `report_ready {mode}`, `look {target:"map"}` and model facing (FIXME.md "P5 live smoke"). The prompts themselves (`prompts/get`) were not exercised live; a stdio test covers them.

## Legend

- **Kind** (from the dump annotations; every tool also has `openWorldHint:false`): `RO` = `readOnlyHint:true`;
  `M` = mutating, `destructiveHint:false`; `M, idem` = also `idempotentHint:true`; `D` = no
  `destructiveHint` given, so the MCP default (destructive) applies (writers such as `save_map`, `reload_map`,
  and compiles that overwrite outputs).
- **Args**: schema property names; `name*` is required. The MCP schemas carry types and enums; see the dump.
- **Purpose**: first sentence of the tool description, cut at 150 chars. Where the sentence ends in a colon the
  description continues (result shape, failure modes).
- **Status**:
  - `new`: not in the baseline dump.
  - `changed (text/schema/annotations)`: in both dumps and different; there is no other kind in this refactor.
  - `live-verified`: seen working against a real Hammer or game in ACCEPTANCE.md runs 2-4 (run 1 only
    checked descriptions and failed on `open_map`), LIVE-P1.md, LIVE-P1b.md, LIVE-P1c.md, or FIXME.md "P5 live
    smoke". A tool that only appears in those docs as an error case or as advice is not counted
    (for example `get_mesh` and `get_entity_live` returned MeshNotFound/EntityNotFound after an undo;
    `game_rules` and `create_trigger` appear only in findings text).
  - `tested only`: new and covered by unit/integration/mock tests, never run live.
  - `not re-run live`: existed before, was only text/annotation/schema-changed here, and none of the docs above
    shows a live run of it in this refactor.
- Live evidence concerns hammer-mcp only. No client-mcp tool is shown running live in those docs (the live
  `console_exec`/`cvar_get`/`game_state` runs are hammer-mcp's tools-session versions).

## hammer-mcp (100 tools incl. 2 deprecated aliases, 5 prompts; live tools/list 2026-10-28)

### Session and documents

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `session_status` | RO | Bridge/session status: build, liveness (mainThreadAgeMs), capabilities, active document + host path, unsaved bridge edits, jobs. | - | - | changed (annotations), live-verified |
| `set_addon` | M, idem | Choose the addon the file tools work in (compile_map, run_map, list_models, soundevents, model_*, particle_*, map file reads). | addon | - | new, live-verified |
| `new_map` | M | Create a .vmap in the active addon from a template and (default) open it. | clear open overwrite path* template | - | new, live-verified |
| `open_map` | M | Open a .vmap in Hammer. | path* | - | changed (text+schema+annotations), live-verified |
| `save_map` | D | Save the live map to its path (the only tool that writes the .vmap). | - | - | changed (text+annotations), live-verified |
| `reload_map` | D | Reload the live map from disk, discarding unsaved edits and the undo history. | force | - | changed (text+schema+annotations), not re-run live |
| `undo_last` | M | Undo one MCP-created undo group. | acceptUserEdits | - | changed (text+schema+annotations), live-verified |
| `get_selection` | RO | What is selected in the live Hammer document: `mode`, `nodeIDs`, `count`, and in faces mode `faces: [{nodeID, faces:[i]}]` per mesh (indices as... | - | - | changed (text+annotations), not re-run live |
| `set_selection` | M | Change the selection in the live Hammer document so the user sees what you work on. | faces nodeIDs op* | - | changed (text+schema+annotations), not re-run live |
| `get_scene_info` | RO | Overview of the map: entity counts per class, mesh count, bounds, spawns, and the first `limit` entities whose classname/targetname contains `query`. | fields limit path query source | - | new, live-verified (P5 smoke, through look {target:"map"}) |

### Entities

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `list_entities` | RO | Entities from the saved .vmap on disk (not unsaved edits; see find_entities). | classnames limit path | - | changed (text+schema+annotations), live-verified |
| `find_entities` | RO | Find entities by classname/targetname/sphere. | class limit nameContains path source targetname withKeyvalues within | - | changed, live-verified 2026-10-28 (`class` filter; a wrong key such as `classname` is now refused with didYouMean) |
| `get_entity` | RO | One entity (keyvalues, I/O connections) by nodeID or targetname. | nodeID path source targetname | - | changed (text+schema+annotations), not re-run live |
| `create_entities` | M | Create entities in one undo group (never saves). | allowUnknownSoundEvent entities* strict undoLabel | - | changed (text+schema+annotations), not re-run live |
| `set_entity_keyvalues` | M | Set keyvalues on an existing entity by nodeID (live, not saved; returns previous values). | force keyValues* nodeID* strict | - | changed (text+schema+annotations), not re-run live |
| `list_entity_classes` | RO | Entity classes from the game's FGD files (filter by substring/kind/tag). | filter kind limit tag | - | changed (text+schema+annotations), not re-run live |
| `get_entity_class` | RO | One entity class from the FGD: resolved keyvalues (type, default, choices, flags), inputs, outputs, bases. | class* includeBases | - | changed (text+schema+annotations), live-verified |
| `add_entity_connections` | M | Add I/O connections to a live entity by nodeID (one undo group, never saves). | connections* force nodeID* | - | changed (text+schema+annotations), live-verified |
| `remove_entity_connections` | M | Remove live entity I/O connections by nodeID, by `indices` or `match` (output/target/input; safer, indices shift with every edit). | indices match nodeID* | - | changed (text+schema+annotations), not re-run live |
| `tie_to_entity` | M | Tie existing live world meshes to a brush entity: a NEW one (`class`) or an existing one (`entityID`). | allowRetie class entityID keyValues material meshNodeIDs* spawnflags targetname | - | changed (text+schema+annotations), live-verified |
| `get_entity_live` | RO | Deprecated: use `get_entity` {"source":"live"}. | - | deprecated alias of `get_entity {source:"live"}` | changed (text+schema+annotations), not re-run live |
| `find_entities_live` | RO | Deprecated: use `find_entities` {"source":"live"}. | - | deprecated alias of `find_entities {source:"live"}` | changed (text+schema+annotations), not re-run live |

### Meshes and layout

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `apply_layout` | M | Build many meshes and entities in the LIVE map from ONE JSON layout: path (a .json inside the project) or layout (inline), exactly one. | allowUnknownSoundEvent dryRun layout name path strict | - | new, live-verified |
| `list_meshes` | RO | List mesh (brush) nodes of a .vmap: nodeID, origin, bounds, vertex/face counts, materials. | limit path within | - | changed (text+schema+annotations), not re-run live |
| `get_mesh` | RO | Full geometry of one mesh by nodeID: WORLD vertices, faces (counter-clockwise loops), faceMaterials, faceInfo (centre/normal), bounds. | nodeID* path | - | changed (text+schema+annotations), not re-run live |
| `create_mesh` | M | Create a mesh in the LIVE map from WORLD-space vertices and faces (vertex-index loops). | faceMaterials faces* material orientOutward vertices* | - | changed (text+schema+annotations), not re-run live |
| `create_block` | M | Create a box, wedge (ramp) or cylinder mesh in the LIVE map. | bottomMaterial center height material max min radius riseToward shape sideMaterial sides topMaterial | - | changed (text+schema+annotations), live-verified |
| `set_mesh_geometry` | M | Replace ALL polygons of a LIVE mesh (nodeID) with new WORLD-space vertices and faces. | faceMaterials faces* material nodeID* vertices* | - | changed (text+schema+annotations), not re-run live |
| `transform_mesh` | M | Translate, yaw-rotate (degrees about the pivot) and/or scale a LIVE mesh (nodeID). | nodeID* pivot rotateYaw scale translate | - | changed (text+schema+annotations), not re-run live |
| `move_mesh_vertices` | M | Offset vertices (indices from get_mesh) of a LIVE mesh by [dx, dy, dz], e.g. to make a slope. | nodeID* offset* vertices* | - | changed (text+schema+annotations), not re-run live |
| `extrude_mesh_face` | M | Push one face (index from get_mesh) of a LIVE mesh out along its normal by `distance` (negative = recess), adding side quads. | distance* face* nodeID* sideMaterial | - | changed (text+schema+annotations), not re-run live |
| `delete_mesh_faces` | M | Remove faces (indices from get_mesh) of a LIVE mesh; remaining indices are renumbered, so call get_mesh again. | faces* nodeID* | - | changed (text+schema+annotations), not re-run live |
| `set_mesh_material` | M | Assign a material to a LIVE mesh's faces (all faces if `faces` omitted). | faces material* nodeID* | - | changed (text+schema+annotations), not re-run live |
| `align_texture` | M | Set the texture mapping (UV) of a LIVE mesh's faces. | faces justify keepScale mode* nodeID* rotate scale shift texSet | - | new, live-verified |
| `delete_nodes` | M | Delete meshes/entities/groups by nodeID in the live document (one undo group, undo_last restores; never saves). | nodeIDs* | - | changed (text+schema+annotations), live-verified |
| `set_node_transform` | M | Move/rotate/scale one live node (entity or mesh) by nodeID. | angles nodeID* origin relative scales | - | changed (text+schema+annotations), live-verified |

### Props, triggers, sounds in the map

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `list_props` | RO | Prop entities of a map, from the saved .vmap (source file) or the live document (source live). | class includeKeyValues includeOtherModelEntities kind limit model offset path source targetname within | - | changed (text+schema+annotations), not re-run live |
| `create_props` | M | Place prop entities in one undo group (never saves). | allowUncompiled props* | - | changed (text+schema+annotations), not re-run live |
| `list_triggers` | RO | List brush entities (triggers, or every @SolidClass with kind "solid") from the SAVED .vmap (source "file", reports staleRead) or the LIVE... | class includeConnections includeKeyValues kind limit offset path source targetname within withinMode | - | changed (text+schema+annotations), not re-run live |
| `create_trigger` | M | Create a trigger brush entity in the LIVE map from exactly one volume (`box`, `geometry` or `meshNodeIDs`) and tie it to the new entity. | allowRetie box class connections force geometry keyValues material meshNodeIDs retexture spawnflags targetname | - | changed (text+schema+annotations), not re-run live |
| `move_to_world` | M | Move a brush entity's meshes back under its parent (world/group) and delete the entity. | deleteEntity nodeID* | - | changed (text+schema+annotations), not re-run live |
| `set_trigger_volume` | M | Replace a LIVE brush entity's volume with a new `box` or explicit `geometry` (WORLD coordinates), rebuilding one of its tied meshes in place. | box geometry material meshNodeID nodeID* | - | changed (text+schema+annotations), not re-run live |
| `create_sound_entities` | M | Create sound entities in the LIVE map: `kind` point\|sphere\|alignedbox\|orientedbox\|soundscape\|soundscape_proxy, or any FGD tags(Sound) `class`. | allowUnknownSoundEvent entities* strict undoLabel | - | changed (text+schema+annotations), not re-run live |

### Compile and jobs

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `compile_map` | D | Compile a Hammer map headless; starts a background job ({jobId, state}). | cancel jobId mode path stages timeoutMs waitMs | - | changed (text+schema+annotations), live-verified |
| `compile_asset` | D | Start the resource compiler on ONE asset of the active addon (vmap vmdl vpcf vsndevts vmat vtex vsnd vsmart vanim vmks); returns {jobId,... | flags game path* timeoutMs | - | changed (text+schema+annotations), not re-run live |
| `job_status` | RO | Status of a compile job (compile_asset or compile_map): state running\|ok\|failed\|cancelled\|timeout, elapsedMs, summary, failLines, logTail. | jobId waitMs | - | changed (text+schema+annotations), live-verified |
| `job_cancel` | D | End a compile job (bridge job id or c<N>) and its whole process tree. | jobId* | - | changed (text+schema+annotations), not re-run live |

### Play-test (tools-session engine)

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `run_map` | M | Load the COMPILED map in the tools session's engine and wait for its load signal. | gameMode gameType ignoreRunningCompile path restartIfDead team timeoutMs unload | - | changed (text+schema+annotations), live-verified |
| `play_test` | M | Run steps against the local player in the running map; stops at the first failure. | keepInput steps* | - | new, live-verified |
| `game_state` | RO | The local player in the tools session's engine: inGame, map, player {team, alive, origin, velocity, eye angles, flags, active weapon}. | intervalMs samples | - | new, live-verified |
| `game_local` | RO | Deprecated: use `game_state`. | - | deprecated alias of `game_state` | new, live-verified |
| `game_rules` | RO | Round and warmup state of the RUNNING game: warmup, freeze, team intro, restart, round times, win status, bomb planted. | - | - | new, live-verified (run 4: the agent read freeze state) |
| `entity_find` | RO | Find entities in the RUNNING game by class (designer name) and/or name (targetname): entindex, class, name, origin, angles, health, toggleState. | class max name | - | new, live-verified |
| `input_set` | M | Press/release/tap virtual buttons for the local player and set the view-angle override (usercmd level). | angles frames press release tap | - | new, live-verified |
| `input_clear` | M | Release every virtual button and drop the view-angle override. | - | - | new, live-verified |
| `input_sequence` | M | Run up to 32 input_set steps in order in one call. | steps* | - | new, live-verified |
| `console_exec` | M | Run one console line in the tools session's engine; returns the console.log lines it produced (the last maxLines). | command* maxLines waitFor waitMs | - | new, live-verified |
| `cvar_get` | RO | Read console variables: per name {found, value (text), type, default, flags}. | names* | - | new, live-verified |
| `drain_sounds` | M | Read AND consume the sound events started since the last drain (name, entity, ageMs, decoded params and world position). | filter installOnly max resolve | - | changed (text+schema+annotations), not re-run live |

### Rendering and look

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `look` | RO | See the map: framed images of a target from poses it computes (no camera maths). | count intervalMs maxSize mode name pose source target | - | new, live-verified |
| `render_view` | RO | Render the LIVE map (unsaved edits included) from a camera with Hammer's offscreen renderer: PNG under <app>/renders/ plus an inline image... | angles distance fov height hideToolMaterials inline lookAt name origin pitch width yaw zfar znear | - | changed (text+schema+annotations), not re-run live |
| `render_world` | RO | Render the map loaded in the ENGINE (compiled .vpk: game lighting, sky, entities, no editor helpers) from a required camera, without Hammer. | angles distance fov height inline lookAt name origin pitch width yaw zfar znear | - | changed (text+schema+annotations), not re-run live |

### Ship check and playbooks

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `report_ready` | RO | Ship check: runs the checks and answers verdict pass\|warn\|fail\|incomplete, each check pass\|warn\|fail\|skipped with detail and fix. | checks debugPatterns map mode profile | - | new, live-verified |
| `cs_script` | W | The addon's TypeScript map-script project (Source2ZE create-cs-script 2.23.1): init, add, write, read, update, build, compile, status. | action* content file name scripts waitMs | - | new, live-verified 2026-10-28 (accept_p4: init+npm, write, type errors as file:line, build, hot reload `reloaded:true`, `.js` -> `.vjs_c` through bridge m3.17, script log line in game) |
| `get_playbook` | RO | Step-by-step procedure for a whole task, as text (the same as the MCP prompt): blockout_map, iterate_loop, cs_script, particle_set, release. | folder map name script | - | new, live-verified (P5 smoke: list + iterate_loop text) |

### Assets and models

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `find_assets` | RO | Find assets in the addon and the game paks by kind: material, model, particle, sound (.vsnd), texture. | cursor kind* limit prefix query source | - | new, live-verified |
| `list_models` | RO | List .vmdl models in the game VPKs and the current addon. | compiledOnly limit offset prefix query source | - | changed (text+schema+annotations), not re-run live |
| `get_model_info` | RO | Compiled-model facts for placing a prop: render/physics bounds, `facing` (forward +x/-x/+y/-y from front/rear attachments, else unknown +... | include limit model* | - | changed (text+schema+annotations), live-verified |

### ModelDoc

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `model_status` | RO | ModelDoc editor status: whether modeldoc_editor.dll is loaded, missing capabilities, the active document (+ host path, inAddon) and a launch/open... | - | - | changed (text+annotations), not re-run live |
| `model_read` | RO | Read a .vmdl from disk: one node subtree as JSON (_class, name, attrs, children). | - | - | changed (text+annotations), not re-run live |
| `model_list_nodes` | RO | Flat node list of a .vmdl on disk (path /i/j, _class, name, childCount, attrKeys), filterable by class/name. | - | - | changed (text+annotations), not re-run live |
| `model_edit` | D | Edit a .vmdl in the active addon: ops add_node/remove_node/move_node/set_attr/remove_attr, all-or-nothing, validated, backup + atomic write. | - | - | changed (text+annotations), not re-run live |
| `model_create` | D | Create a new .vmdl in the active addon from a template (static_prop, skeletal). | - | - | changed (annotations), not re-run live |
| `model_compile` | D | Compile a .vmdl to .vmdl_c. | - | - | changed (text+annotations), not re-run live |
| `model_open` | M | Open a .vmdl of the active addon in the running ModelDoc. | - | - | changed (text+annotations), not re-run live |
| `model_reload` | D | Reload the active ModelDoc document from disk (after model_edit); discards editor undo history. | - | - | changed (text+annotations), not re-run live |
| `model_save` | D | Save the active ModelDoc document to its own path (never Save As: refuses untitled documents and files outside the addon). | - | - | changed (annotations), not re-run live |
| `model_node_types` | RO | Every ModelDoc node class name (_class) the running editor knows, sorted; writeSnapshot stores them under hammer-mcp/research/. | - | - | changed (text+annotations), not re-run live |
| `model_render` | RO | Render ModelDoc's main 3D viewport to a PNG under <app>/renders/ and attach it as an image (downscaled above 1920x1080). | height inline name width | - | changed (text+schema+annotations), not re-run live |

### Particles

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `particle_read` | RO | Read a .vpcf from disk: the kv3 header, root non-array fields, per-array entry counts; function:"m_Operators/2" returns that entry's fields. | - | - | changed (text+annotations), not re-run live |
| `particle_list` | RO | Flat list of a .vpcf's function entries from disk (array, type, index, class, fieldKeys), filterable by array and class; counts gives entries per... | - | - | changed (text+annotations), not re-run live |
| `particle_validate` | RO | Validate a .vpcf on disk against the particle schema snapshot (structure, classes, fields); returns ok, errors, warnings. | - | - | changed (text+annotations), not re-run live |
| `particle_edit` | D | Edit a .vpcf in the active addon: ops... | - | - | changed (text+annotations), not re-run live |
| `particle_create` | D | Create a new .vpcf in the active addon from the minimal template (maxParticles, renderer). | - | - | changed (annotations), live-verified |
| `particle_compile` | D | Compile a .vpcf (or a folder; recursive) to .vpcf_c with resourcecompiler. | children force path* recursive timeoutMs | - | changed (text+schema+annotations), live-verified |
| `particle_status` | RO | Particle editor (pet) status: whether pet.dll is loaded, missing capabilities, the active .vpcf document (+ host path, inAddon) and a launch/load... | - | - | changed (text+annotations), not re-run live |
| `particle_types` | M | Every particle function class the running pet registers, grouped by array. | - | - | changed (text+annotations), not re-run live |
| `particle_schema` | RO | Schema (bases, flattened fields, metadata) of one particle class, from the running pet if available, else the local snapshot. | - | - | changed (text+annotations), not re-run live |
| `particle_load` | M | Load the particle editor tool into the running tools session and wait for pet.dll to verify. | - | - | changed (text+annotations), not re-run live |
| `particle_open` | M | Open a .vpcf of the active addon in the running pet. | - | - | changed (text+annotations), not re-run live |
| `particle_reload` | D | Reload the active pet document from disk after a file edit. | - | - | changed (text+annotations), not re-run live |
| `particle_save` | D | Save the active pet document to its own path (never Save As: refuses untitled documents and files outside the addon). | force | - | changed (text+schema+annotations), not re-run live |
| `particle_live_read` | RO | Read the live pet document as pet serialized it: path, dirty/canUndo/canRedo, kv3 header, root non-array fields, per-array entry counts;... | function | - | changed (text+schema+annotations), not re-run live |
| `particle_live_edit` | M | Edit the live pet document in-editor with undo: ops... | allowObsolete label ops* skipSchema | - | changed (text+schema+annotations), not re-run live |
| `particle_undo` | M | Undo the last live pet edit (same history as the editor's Ctrl+Z). | - | - | changed (text+annotations), not re-run live |
| `particle_redo` | M | Redo the last undone live pet edit. | - | - | changed (text+annotations), not re-run live |
| `particle_render` | RO | Render the particle editor's preview viewport to a PNG under <app>/renders/ and attach it as an image (downscaled above 1920x1080). | height inline name width | - | changed (text+schema+annotations), not re-run live |

### Soundevents

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `list_soundevents` | RO | List soundevents available to the game and the active addon (game VPKs + addon files; no bridge). | file hasPosition includeProperties limit name offset prefix query source type | - | changed (text+schema+annotations), live-verified |
| `edit_soundevents` | D | Edit a .vsndevts (default: the addon's soundevents_addon.vsndevts) keeping comments. | - | - | changed (text+annotations), not re-run live |
| `compile_soundevents` | D | Compile a .vsndevts to <file>_c (args: path?, timeoutMs?). | - | - | changed (text+annotations), not re-run live |

## client-mcp (46 tools)

No live run of any client-mcp tool is recorded in this refactor's evidence docs. All 45 existing tools were
rewritten (text, annotations, some schemas); `cvar_get` is new.

### Session

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `session_status` | RO | Runtime + flavour + bridge + build + liveness + tool-group availability; never raises. | - | - | changed (annotations), not re-run live |
| `launch_session` | M | Start a CS2 container: linux-client (headless), wine-tools (host X, -tools, bridge) or wine-client (Windows client, no -tools). | addon args bridge capture connect gpus headless kind* map testscript timeoutMs waitFor | - | changed (text+schema+annotations), not re-run live |
| `wait_session` | RO | Wait for a launched session's readiness stage. | container timeoutMs waitFor | - | changed (text+schema+annotations), not re-run live |
| `stop_session` | D | Stop and remove a session container (cs2mcp-* only). | container | - | changed (text+schema+annotations), not re-run live |
| `server_up` | M | Launch a headless Linux dedicated server (RCON, host network) on map (default de_dust2) and port (default 27015; PortInUse when bound). | args bots hibernate map port rconPassword timeoutMs waitFor | - | changed (text+schema+annotations), not re-run live |
| `client_connect` | M | Send `connect <address>` to a client session. | address container | - | changed (text+schema+annotations), not re-run live |

### Console and cvars

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `console_tail` | RO | Last N lines of a console.log, optional regex filter. | grep lines session | - | changed (text+schema+annotations), not re-run live |
| `console_wait` | RO | Wait for a new console.log line matching a regex (lines after the call starts, unless fromStart). | fromStart regex* session since timeoutMs | - | changed (text+schema+annotations), not re-run live |
| `console_exec` | M | Queue a console command through the bridge (-testscript or -tools sessions; not during a level load). | command* confirm confirmTimeoutMs session | - | changed (text+schema+annotations), not re-run live |
| `cvar_get` | RO | Read console variables (found, value, type, default, flags). | names* | - | new, tested only |
| `server_console_exec` | M | Run one console command on a dedicated server over RCON. | command* container port rconPassword | - | changed (text+schema+annotations), not re-run live |

### Input and movement

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `input_set` | M | Press/release/tap virtual buttons and set the view-angle override. | angles frames press release tap turn | - | changed (text+schema+annotations), not re-run live |
| `input_clear` | M | Release every virtual button and disable the angle override. | - | - | changed (text+annotations), not re-run live |
| `input_sequence` | M | Run up to 32 input.set steps in order. | steps* | - | changed (text+schema+annotations), not re-run live |
| `look_at` | M | Aim the view at a point or an entity (entindex) and wait until it is within 1 degree. | eye hold target* timeoutMs | - | changed (text+schema+annotations), not re-run live |
| `setpos` | M | Teleport the local player (setpos_exact/setang_exact), then wait until it is at origin and stopped. | angleTolerance angles exact floor minZ origin* timeoutMs tolerance zTolerance | - | changed (text+schema+annotations), not re-run live |
| `select_weapon` | M | Select a weapon without keypresses. | command defIndex handle slot weapon | - | changed (text+annotations), not re-run live |
| `fire_weapon` | M | Shoot: per shot, release then tap attack until clip1 drops by one, then check the shot sounds. | frames shots timeoutMs weapon | - | changed (text+schema+annotations), not re-run live |
| `use_entity` | M | Use an entity: aim, press use, wait 32 ticks, re-read it. | maxDistance target* timeoutMs | - | changed (text+schema+annotations), not re-run live |
| `ent_fire` | M | Send `ent_fire <target> <input> [value] [delay]` through console.exec and confirm the engine ran it; the effect is NOT verified (read it back). | allowDestructive delay input* target* value | - | changed (text+schema+annotations), not re-run live |

### Game state

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `game_state` | RO | Local player: map, tick, view angles, origin/eye/velocity, health, flags, active weapon. | - | - | changed (text+annotations), not re-run live |
| `game_rules` | RO | Round and warmup state: warmup, freeze, team intro, restart, game phase, rounds played, round times and win status, bomb planted. | - | - | changed (text+annotations), not re-run live |
| `entity_find` | RO | Find entities in the RUNNING game by class (designer name) and/or name (targetname). | class max name | - | changed (text+schema+annotations), not re-run live |
| `weapons` | RO | Local inventory: active weapon and every held weapon (entindex, name, defIndex, clip1/clip2/reserve). | - | - | changed (text+annotations), not re-run live |
| `radar` | RO | Players (team, alive, health, origin, eye yaw, spotted), bomb and hostages, projected onto the map overview. | map | - | changed (text+schema+annotations), not re-run live |
| `game_tick` | RO | Current server tick and input-hook frame count. | - | - | changed (text+annotations), not re-run live |
| `game_wait` | RO | Wait until a predicate over the local player holds. | angleTolerance angles clip1 clip1Delta defIndex entindex from minDistance origin pollMs requireStopped roundStartTime roundsPlayed ticks timeoutMs tolerance until* weapon zTolerance | - | changed (text+schema+annotations), not re-run live |
| `round_start` | M | Put the round in a known state (idempotent). | cvars freezetime mode respawn roundtime timeoutMs | - | changed (text+schema+annotations), not re-run live |
| `game_snapshot` | RO | Save the local player (pose, velocity, health, armor, money, team, weapons) and the round summary under name (32 at most, in this process). | export name* | - | changed (text+schema+annotations), not re-run live |
| `game_restore` | D | Restore a snapshot on the same map: pose, health, armor, missing weapons, then the active weapon. | name parts snapshot timeoutMs | - | changed (text+schema+annotations), not re-run live |
| `sounds_drain` | M | Drain recorded game sound events (name, source entindex, origin). | max resolve | - | changed (text+annotations), not re-run live |
| `sounds_status` | M | Install the sound-event sink (idempotent) and report ring counters. | mode | - | changed (text+annotations), not re-run live |

### Tests and scripts

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `checkpoint` | M | Fire a test-script checkpoint. | name* | - | changed (text+schema+annotations), not re-run live |
| `run_testscript` | M | Stage a CTestScriptMgr suite (library name or inline script) and launch a wine-tools session with -testscript. | demo inline judge suite timeoutMs | - | changed (text+schema+annotations), not re-run live |
| `test_status` | RO | State of a run_testscript run: running\|passed\|failed\|error, checkpoint timeline, artifacts once finished. | runId* | - | changed (text+annotations), not re-run live |
| `test_wait` | RO | Wait for a run to finish, then return test_status; pending: true when still running. | runId* timeoutMs | - | changed (text+schema+annotations), not re-run live |
| `script_run` | M | Run the scripts of a compiled addon map: launch or reuse the session, fire entry, match the expect regexes in order over the [cs_script] console... | addon anyLine entry expect* kind* map path source timeoutMs | - | changed (text+schema+annotations), not re-run live |
| `game_test` | M | Declarative in-game test. | spec* | - | changed (text+schema+annotations), not re-run live |
| `game_test_results` | RO | Stored results of this process's game_test runs: all or one by name. | format name | - | changed (text+schema+annotations), not re-run live |

### Capture

| Tool | Kind | Purpose | Args (* required) | Alias / replaced by | Status |
|---|---|---|---|---|---|
| `render_world` | RO | Offscreen freecam render of the loaded map (wine-tools bridge). | angles* fov height name origin* srgb width zfar znear | - | changed (text+schema+annotations), not re-run live |
| `screenshot` | RO | Capture the game view: the next frame the session presents, from its Vulkan swapchain via the capture layer. | maxWidth ocr session timeoutMs | - | changed (text+schema+annotations), not re-run live |
| `observe` | RO | One playtest observation: screenshot, console.log tail or lines since a cursor, checkpoint events, game.tick, a screen guess (dialog\|loading\|unknown). | capture consoleLines image ocr session since | - | changed (text+schema+annotations), not re-run live |
| `ocr` | RO | Extract text from the last capture (last: true) or a host PNG with tesseract. | crop last png psm | - | changed (text+schema+annotations), not re-run live |
| `demo_record_start` | D | Start a SourceTV recording. | name* session | - | changed (text+schema+annotations), not re-run live |
| `demo_record_stop` | D | Stop a SourceTV recording; returns the host .dem path and bytes. | name* session | - | changed (text+schema+annotations), not re-run live |
| `demo_to_video` | D | Turn a demo in csgo/ into a video with the engine encoder (a wine-tools run_testscript; poll test_wait). | bitrate countFrames demo* format framerate height name* timeoutMs width | - | changed (text+schema+annotations), not re-run live |

## Not verified live (new tools, from FIXME.md)

All tools: unknown top-level argument names are refused (`InvalidParams`, `data.unknownArguments`, `didYouMean`) by the registry since 2026-10-28; open schemas still accept anything.

These were not exercised against a real Hammer or game in this refactor.

- The 5 playbooks (FIXME P4): no agent has followed one live. `get_playbook` was called live (P5 smoke),
  but the steps are only checked against the tool schemas by `hammer-mcp/tests/playbooks.rs`. Assumed,
  not seen: the `info_particle_system` keys `effect_name` and `start_active`; the `m_ChildRef` resource
  form in the `particle_set` playbook. (`movedir:"0 0 0"` as +X for `func_movelinear` was seen live in
  run 4: the mover went x 256 -> 862.)
- `get_scene_info` (FIXME "Play follow-ups" 4-5): the duplicate-targetname case (`AmbiguousTargetname` through
  `get_entity` with a real `entity.find limit:2`) and live bounds computed from mesh origins (not extents) were
  not run live.
- `game_rules` (hammer): new, shares code with the client tool; no live run in the docs. `play_test`'s freeze wait
  was seen indirectly (run 3 and 4 notes), not the tool itself.
- `look` (FIXME WP3.1 item 6; run 4 saw an editor `tour` and a `player_eye` image only): that `render_world` from the
  framed poses shows the target; that `game.local` carries `player.eye` (else origin + 64); that `source:"editor"`
  poses match the engine's; `frames` against a moving entity; `player_eye` against a drone camera.
  The P5 smoke saw `target:"map"` framed on mesh extents.
- `report_ready` (FIXME WP3.2): the `movedir` direction (read as source `AngleVectors`) and the straight-line travel
  assumption were not verified against a mover that was run; the `set_mesh_material {faces}` alternative on a real
  mesh; `authoredposition` / `startposition` against a real mover; `debug_patterns` / `loose_compiled` on the
  ti_convoy addon. Materials check: `renders:"ok"` does not look at shaders or non-`.vtex` references; a resource
  with no `RERL` block is treated as referencing nothing (assumption: the compiler omits RERL when the list is
  empty; not read in the compiler). Release profile verdict is `incomplete` by design (`no_metamod_load`).
- `find_assets` (FIXME P5): sounds are `.vsnd` files only, not soundevent names; paths are lowercased. The live smoke
  covered materials only (`cobblestone_001.vmat` flagged `missing_textures`); model, particle, sound and texture
  kinds were tested but not run live.
- `get_model_info` `facing` (changed): live only for the no-attachment case (car_a `unknown`). The attachment-based
  front/rear vote is tested, never seen on a model that has attachments.
- `align_texture` (FIXME WP1.5): `rotate`'s sign against Hammer's RotateTexture90CW is unverified (no headless way to
  read the face-edit UI; LIVE-P1 "PARTIAL"). `world`, `fit` and undo passed live.
- `console_exec` / `cvar_get` (FIXME WP1.3, live-fix A-D): passed live with findings, but not run live: the
  marker-command wait on the vtm route (does the vtm route print the marker line, does the channel split on `;`),
  the long-line refusal threshold (only 1117/1135-byte lines proven dropped; the guard refuses earlier), and
  the `rcon_password` redaction was fixed after the first run and re-checked in LIVE-P1b.
- `input_set` release+tap in one call (bridge m3.15 `regap`): live-seen in LIVE-P1 (second jump produced); the
  test `crates/cs2-bridge/tests/ops_hooks.rs` is the regression guard.
- `compile_map` / `compile_asset` / `particle_compile` (changed): `-f` as the remedy for a compiler "skipped"
  verdict, and the `-world -phys -vis -bakelighting` allowlist (bridge m3.16), were not run live;
  the `job.start` vs `-f` path is still untested.
- `open_map` first-map route through the vtm `open_asset` (live-fix C item 1) and the other items of LIVE-P1b
  "New findings" A-C: mock tests only; `new_map` replaced the need for it in practice and was live-verified.
