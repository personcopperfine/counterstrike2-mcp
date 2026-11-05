# Valve Developer Wiki deep dive

Oct 24, 2026

A crawl of developer.valvesoftware.com's Source 2 pages, filling the gaps left in the main tab.

## Gaps resolved

The wiki settles units, KV3 flags, most ModelDoc node names, `.vtex`, sound files and AnimGraph 1, and corrects two claims in the main tab (`-lightmapCPU` is gone; `.vmap` version isn't fixed at 35). It does **not** contain a CS2 `.vmat` text sample, any ModelDoc/vpcf format GUID, or anything on CS2 player models / AnimGraph 2 — those must come from files on a real install.

| Gap | Status | What the wiki says |
| --- | --- | --- |
| Units | Resolved | "1 game unit = 2.54 cm"; "Divide metres by 0.0254 to get inches, which equal Hammer game units" |
| KV3 flags | Resolved | `resource`, `resourcename` (no underscore), `panorama`, `soundevent`, `subclass`, plus `deferred_resource` (older Dota page). Syntax `key = resource:"path"` |
| KV3 headers | Partial | Generic header confirmed; `.vsndevts` uses the **generic** header. No modeldoc / vpcf / vsmart / vpost GUIDs anywhere |
| Legacy `.vmdl` | New | `<!-- schema text {7e125a45-3d83-4043-b292-9e24f8ef27b4} generic {198980d8-3a93-4919-b4c6-dd1fb07a3a4b} -->` = old Model Editor format (`CV*` classes, `m_*` keys). **The wiki's `VMDL/*` pages document this legacy format, not ModelDoc** |
| S1 model wrapper | New | Generic KV3 header + `{ m_sMDLFilename = "models/x.mdl" }` compiles a Source 1 `.mdl` |
| ModelDoc node names | Mostly | See Source 2 core section. Still missing: bodygroup, attachment, hitbox, jiggle-bone, cloth class names and snake\_case keys |
| `.vtex` | Resolved | DMX `keyvalues2_noids`; full `CDmeVtex` schema captured |
| `.vmat` text | **Not on wiki** | VMAT page is a stub, Material Editor page blocked. Use shipped examples (see CS2 / HL:Alyx sections) |
| `.vmap` | Partial | Binary DMX; a talk page says current header version is **40** (older 37) — never hard-code. Element names only in VRF docs on s2v.app |
| `-lightmapCPU` | **Correction** | Removed in the Armory update (three pages agree). Drop it from any tool or doc |
| Steam Audio | Resolved | No CLI flags: UI "Steam Audio" tab → Change Settings; stored as 16 worldspawn keys `steamaudio_reverb_*` / `steamaudio_pathing_*` |
| Nav compile | Resolved | No options; included in Fast/Full/Final |
| `-lightmapVRadQuality` | Not on wiki | Read Hammer's Command Line box |
| resourcecompiler / dmxconvert | Resolved | Full flag lists; **dmxconvert overwrites the input when `-o` is omitted** |
| VPK (Source 2) | Not on wiki | `vpk.exe` marked "Todo"; use Source 2 Viewer to create VPKs |
| Hammer command list | Not on wiki | In-app: F1 → right-click "All Commands"; Tools → Options → Key Bindings; Command History panel |
| AnimGraph 1 | Resolved | Nodes, states, transitions, debug recording (HL:Alyx section) |
| CS2 player models / AG2 | **Not on wiki** | CS2 Animgraph page blocked; hub says the Animgraph Editor is not in CS2 |
| VConsole port | **Conflict** | SteamVR Home docs say 29009; earlier research said 29000. Probe both |

## CS2 Workshop Tools

The CS2 section (37 subpages, 89 read) is strongest on shipped templates, the CS2 entity list, sound, cs\_script and lighting limits, and weakest on materials and models. The single most useful find for weak agents: **Valve ships template maps and example assets that agents should copy instead of building from scratch.** ([hub](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools))

**Shipped templates and fixtures** ([Examples](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Examples))

| File | Use |
| --- | --- |
| `content/csgo_addons/addon_template/maps/xxx_mapname_xxx.vmap` | Everything a defuse map needs, with dev notes |
| `content/csgo/maps/templates/template_{defuse,hostage,wingman}.vmap` | Mode-specific starting maps |
| `content/csgo/maps/editor/zoo/{ancient,train,script,smartprop}_zoo.vmap` | Reference scenes; `map editor/zoo/script_zoo` |
| `.../addon_template/maps/content_examples/lighting_info.vmap` | Lights + post-processing reference |
| `content/csgo/workshop/content_examples/breakable_glass/breakable_glass_01.vmdl` (+ `.vmat`, `_broken.vmat`) | A real CS2 ModelDoc + material to read the format from |
| `.../addon_template/soundevents/soundevents_addon.vsndevts` | Sound event template |
| Prefab `basic_entities` | Starter entities |

**Entities: what CS2 removes** ([Counter-Strike 2.fgd](https://developer.valvesoftware.com/wiki/Counter-Strike_2.fgd))

- `@exclude`d (do nothing / unavailable): `light_spot`, `light_omni`, `light_dynamic`, `env_cubemap`, `env_sprite`, `env_sun`, `env_tonemap_controller`, `env_volumetric_fog_controller/volume`, `func_instance`, `func_illusionary`, `func_wall(_toggle)`, `func_detail_blocker`, `func_precipitation`, `func_shatterglass`, `color_correction(_volume)`, `postprocess_controller`, `fog_volume`, `path_particle_rope`, `env_projectedtexture`, `info_landmark`.
- CS2-specific: `env_cs_place` (`place_name`; **mesh must be box-shaped**), `cs_minimap_boundary`, `info_map_parameters`, `info_map_region`, `game_money`, `func_nav_blocker`, `func_water`, `func_conveyor`, `env_gradient_fog`, `env_cubemap_fog`, `csm_fov_override`, `map_preview_camera_path(_node)`, team intro entities, `end_of_match`, `post_processing_volume`, `point_script`.
- Known bugs: `func_breakable` spawns no gibs (use `prop_dynamic` + BreakPieceEmbedded); a `prop_dynamic` whose model is set up for `prop_physics` **is deleted on load**.
- Includes: base.fgd, lights.fgd, lights2.fgd, markup\_volumes.fgd, postprocessing.fgd. Auto-visgroup tool materials include `toolsclip_<surface>.vmat`, `tools_cs_place.vmat`, `toolsblockbullets_cs.vmat`.

**Lighting limits** ([Lighting](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Level_Design/Lighting))

- `light_barn`: max **380** unshadowed dynamic lights per map; max **4** overlapping stationary lights per face. `light_omni2` shapes Point/Sphere/Tube; dynamic shadows cost 4 atlas slots. `light_rect` static only. Per Pixel: 1 shadowed spot/ortho + 8 omni.
- Lighting Complexity view (CS2 legend): gray, blue, aquamarine, green, yellow, **orange = too many**. The older generic page uses a different legend — prefer the CS2 one.
- Probe volume gaps → black or fullbright players. Debug: `r_light_probe_volume_debug_grid 1`, `mat_fullbright 2` (diffuse), `mat_fullbright 8` (reflection cost). Edge Fade Dist in inches (0 hard, 32 soft).
- Menus: Preview Baked Lighting → Bake All Lighting / Bake Lighting On Selection; F5 unlit, F6 lit, Alt+F6 path-traced preview. RAM 16 GB min / 32 GB rec; 12 GB+ VRAM for large lightmaps.
- **CPU baking is gone** since the Armory update.

**Sound** ([Addon Sounds](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Addon_Sounds))

- File **must** be `soundevents/soundevents_addon.vsndevts`; renamed or moved files aren't precached. Generic KV3 header.
- Types: `csgo_mega` (most sounds), `csgo_music`, `csgo_3d` (ambient). **Key differs by type**: music uses `vsnd_files`, sfx uses `vsnd_files_track_01`.
- Other keys: `volume`, `volume_convar` ("snd\_musicvolume"), `pitch`, `mixgroup` ("Ambient"), `distance_volume_mapping_curve`.
- Reference `sounds/x.vsnd`, never `.wav`. Same event name overrides a base-game sound. Place with `snd_event_point`; verify with `snd_list_soundevents`. A loose `.vpk` loaded with `map` won't precache soundevents.

**cs\_script** ([Scripting API](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Scripting_API))

- `import { Instance } from "cs_script/point_script";` in `content/csgo_addons/<addon>/scripts/x.js`; point\_script `cs_script` = `x.vjs`. TypeScript unconfirmed on the wiki; Pulse "unavailable to end users".
- Wiring pattern: `trigger_multiple` OnStartTouch → point\_script `RunScriptInput` with param `begin_overlap` ↔ `Instance.OnScriptInput("begin_overlap", …)`. Names must match exactly (the wiki page itself mismatches them).
- Full API captured in project notes: `EntFireAtName({name,input,value?,delay?})`, `TraceLine/Box/Sphere`, `TraceBullet`, `GiveNamedItem`, `OnBeforePlayerDamage` (return modify or `{abort:true}`), 20+ event hooks, enums (`CSGearSlot` RIFLE 0 … BOOSTS 5). `Debug*` only in dev; `SetNextThink` rounds to tick.

**Game modes** ([Game Modes](https://developer.valvesoftware.com/wiki/Counter-Strike:_Global_Offensive/Game_Modes))

- **CS2's `map` takes no mode argument**: `game_type 0; game_mode 1; map de_x` (Competitive). Casual 0/0, Wingman 0/2, Arms Race 1/0, Deathmatch 1/2, Custom 3/0.
- `maps/<map>.kv` sets default\_game\_type/mode; `maps/cfg/<map>.cfg` runs for Custom when `sv_autoexec_mapname_cfg` ≠ 0.

**Particle Editor unlock (exact)** ([Particles](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Particles))

- `game/bin/sdkenginetools.txt`, block `m_Name = "pet"`: delete from `m_ExcludeFromMods =` through its `]`.
- `game/bin/assettypes_common.txt`, block `particle_asset`: delete `m_HideForRetailMods =` through `]`.
- Bonus: `assettypes_common.txt` is a machine-readable registry — every `m_Ext` with its `m_CompilerIdentifier` (e.g. `vpcf` → `CompileParticle`). Parse it instead of hard-coding asset types.

**Other recipes**

- **Water**: material from `materials/liquids/` on one flat face; delete other faces. Too deep → players can't jump out.
- **Vertex blend painting**: Shift+V, Paint On "Selected Faces"; error "The active material does not support vertex painting." Faces on the same channel must share a material.
- **Breakable window**: full ModelDoc recipe (Mesh Operation Stack, Box Primitive, PhysicsHullFromRender `glass`/`window`, prop\_data `Glass.Window` health 1, BreakPieceEmbedded, Voronoi Plane Primitive 64 points, Inflate 0.75, "Assign Vertex Pivot Data"). "The model won't compile without a RenderMeshFile." ([BreakableWindows](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/BreakableWindows))
- **3D skybox split**: Ctrl+X skybox geometry → new map `<map>_skybox` → Ctrl+Shift+V "Start at center of original" → Map Properties Map Type `3D Skybox` → `skybox_reference` at 0,0,0 in main map → copy `light_environment` + `env_sky` into the skybox map.
- **Map import errors**: missing `sky_csgo_night_flatrt.vmt` → set skybox to sky\_dust; log stops at "Prepping bsp for S2 import!" → spaces in path; "Failed to load file (invalid)" → rename `game/bin/win64/vpk.signatures`. GUI: `import_map_community_gui.exe`.
- **Weapon finishes**: shader `customweapon`; albedo check via VConsole ConVar Helper → "cs2workshop" → Albedo.
- **Shipped map naming**: 3D skyboxes as `prefabs/<map>/<map>_skybox`, menu backgrounds `<map>_vanity`, `workshop_preview_<map>`.
- **Hammer needs the game running.** Player models are hidden from the Asset Browser unless files are edited, which "will stop the game from working online".

## HL:Alyx Workshop Tools

The HL:Alyx section (\~100 pages read) is the best Valve documentation for **ModelDoc** and the only real documentation for **AnimGraph 1** and **Lua**; it has no Materials subsection and its particle pages are stubs. Much of the ModelDoc material transfers to CS2. ([hub](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools))

**ModelDoc recipes** ([Modeling](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools/Modeling))

| Archetype | Nodes | Rules |
| --- | --- | --- |
| Static Prop Model | RenderMeshFile (Import Scale) + PhysicsMeshFile (Simplify, Max Vertices Per Mesh) | "Only static props can use meshes for collision." Place as `prop_static` |
| Physics Prop | RenderMeshFile + PhysicsHullFile (Hull Per Mesh, Face Merge Angle, Incremental Face Reduction, Surface Property) + `GenericGameData: prop_data` | "Dynamic objects must use hulls or basic physics shapes." Recompile after changing Surface Property |
| Jointed Physics Prop | Skeleton → Bone, PhysicsShapeBox per part, PhysicsJointRevolute (Parent/Child Body, Enable Limit, max 90°), prop\_data | "Without bones, all physics shapes belong to one physics body." Reset child Origin after copying to bone |
| Breakable | prop\_data with health + BreakPieceEmbedded (child render + physics) | "Each BreakPieceEmbedded compiles into its own vmdl\_c"; make pieces slightly smaller. Also BreakPiece (Random Spawn Chance), BreakPieceExternal (separate VMDL), BreakPieceSimpleParticles; break commands like `break_create_particle` |

- **LODs**: `LODGroupList` → `LODGroup` (Add Mesh, Switch Threshold set with the camera button at current zoom; first = LOD0; LOD1 < LOD2) and `LODGroupALL` for meshes at every LOD. Toolbar LOD dropdown / "Auto LOD".
- **Skins**: `MaterialGroupList` → `MaterialGroup`; picked in Hammer via Object Properties → Render Properties → "Skin".
- **Troubleshooting** ([page](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools/Modeling/Model_Troubleshooting)): preview doesn't update → click "Compile Model" (manual only); renders untextured → add `DefaultMaterialGroup`; morphs need DMX ("DMX keeps more data") + a material with "Morphs Supported" + an `EmptyAnim`; missing bones → Show Empty Categories → `BoneMarkupList` → Bone Cull Type "None"; dots in bone names become `_` and break animations.
- "RenderMeshFile is a reference to the source file, not a copy" — deleting or renaming the FBX breaks the model.
- Test materials shipped: `primary_red/green/blue.vmat`, `reflectivity_10/50/90.vmat`. Game Preview: play button top-left of the 3D viewport, then Spawn / Grab / Shoot tools.

**Importing Source 1 models**: KV3 header + `{ m_sMDLFilename = "models/humans/group01/male_07.mdl" }` with the `.mdl/.vvd/.vtx/.phy` copied into content; create `.vmat`s at the same paths as the old `.vmt`s. Or ModelDoc File → Import → "Import and Convert old .VMDL" (outputs FBX; flex rules don't survive). CLI: `cs_mdl_import -game csgo -r -i <in> -o <out> *.mdl` with `-lods` (default LOD0 only), `-YupToZup`, `-header_hull_bounds`, `-l filelist.txt`. ([page](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools/Importing_Source_1_Models))

**AnimGraph 1** ([Core Concepts](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools/Animgraph/Core_Concepts), [State Machines](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools/Animgraph/State_Machine_Nodes), [Debugging](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools/Animgraph/Debugging_AnimGraphs))

- Nodes: Final Pose (root; no input = T-pose), Animation Clip (Sequence, Loop), State Machine, Motion Matching, Mover. Motors: Path, Player Control. Params: Bool/Enum/Int/Float/Vector — prefer Enum over Int.
- States: exactly one Start State; End, Passthrough, Exclusive Root Motion. Tag timing "While Current" recommended ("On Exit" may never fire).
- **Transitions are checked top to bottom; the first fully-true one wins** — reorder to change priority. Operators `== != < > <= >=`.
- Angles in degrees: 0 forward, + left. Server updates AI graphs \~10×/s; clients \~100 ms behind.
- Debug: `ent_animgraph_record [id|classname]` → `save_animgraph_recording` → files in `game\<mod>\SAVE\animgraph` → "Load Replay". Errors panel fills only after "Start Preview". Ctrl+F searches nodes/params/tags.
- ModelDoc "Movement Settings" node: walk/run speeds in inches per second.

**Lua** ([Scripting API](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools/Scripting_API), [Entity Scripts](https://developer.valvesoftware.com/wiki/Entity_Scripts))

- "Only entity scripts are available." Files on the **game** side: `game/hlvr_addons/<addon>/scripts/vscripts/`. Keyvalue `vscripts` (space-separated, relative to vscripts), `thinkfunction` (default 0.1 s).
- Pattern: `logic_auto` OnMapSpawn → `logic_script` → `CallScriptFunction` param `SayHello` (no `()`, no `.lua`). "Run (Skip Build)" reloads scripts; `script_reload_code <path>`.
- Use `RedirectOutput` (ConnectOutput "does not work"), `SpawnEntityFromTableSynchronous` (CreateByClassname "does not initialize the entity"). Quaternion broken; `SetRenderingEnabled` may crash.
- Windows hidden extensions save `.lua.txt` — a classic silent failure worth a lint.

**Sound (HL:Alyx)** ([Addon Sounds](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools/Addon_Sounds))

- `soundevents/<addon>_soundevents.vsndevts` + manifest `resourcemanifests/<addon>_addon_resources.vrman` (`resourceManifest` key); compile with `resourcecompiler.exe -i -f "<full path>.vrman"`.
- Types: `hlvr_default_3d`, `hlvr_2d_w_falloff`, `hlvr_2d_w_occlusion`, `hlvr_music_3d`, `hlvr_music_3d_takeover`, `hlvr_music_2d`, `hlvr_music_fixed_rotation`, `hlvr_music_start_multi_quad`. Keys: `volume_falloff_min/max`, `spread_min/max`, `volume_fade_in/out`, `base` (inherit).
- Addon soundevents load only after `addon_tools_map <map>` or a reload in the Asset Browser. A second page contradicts this one (`src1_3d`, `point_soundevent`, `soundevents_addon.vsndevts`) — prefer Addon\_Sounds.

**Level design specifics**

- Template: File → New Map From Template → `maps\templates\basic_setup.vmap` (has a nav seed). Playable Maps entry is relative to maps/ with no extension (`testmaps/example`).
- Nav: Navigation tool → "Create Walkable Seed" → `func_nav_markup`; flood-fills at compile; `nav_edit 1` in game.
- Lighting: **hard limit of 4 indexed lights per surface** (1–2 preferred); only one casts dynamic shadows at a time. Baked Lighting Complexity: black 0, red 1, orange 2, yellow 3, white 4, cyan over limit. Perf budget: GPU and CPU < 11 ms at 90 Hz.
- Vis: world geometry blocks vis, entities don't (prop\_static only with "Vis Occluder"); outer shell faces must point inward; `vis_debug_tracelos` writes `los.bin` that future compiles reuse. Loaded vis clusters aren't saved.
- Map extensions: prefab of `content/hlvr/maps/release/<map>.vmap` with Editor Only + Transform Locked; link in Edit Addon Info; test `addon_tools_map a2_pistol`. Can't remove entities or change baked lighting.
- Post-processing: Tone Mapping and Bloom run before colour correction regardless of stack order; if several, the bottom one wins.
- Entity recipes captured in notes: ladders, buttons, keycard doors, hacking/toner puzzles, weapon equips (`info_hlvr_equip_player`), `logic_playerproxy` inputs.

**Shipped examples (vmat/vmap text templates)** ([Examples](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools/Examples))

- Maps: `content\hlvr\maps\workshop_examples\{cables,tilemesh,hotspot,vis,postprocessvolume,combine_encounter,hacking,vehicles_and_prefabs}_example.vmap`; full shipped map sources in `content\hlvr\maps\release`.
- Sky/light presets: `prefabs/environment_settings/environment_{day_01,night_01,overcast_01,storm_01,sunset_01,…}.vmap`.
- Materials: `content\hlvr\materials\workshop_examples\` — `brick/brick_1c_comp_hs_example.vmat`, `blend/brick_1c_comp_hs_to_dead_xen_example.vmat`, `hotspot/example_wall_plaster1_hs.vmat`, `skybox/skybox_workshop.vmat`. These are the authoritative `.vmat` text samples the wiki lacks.

## Source 2 core

Only 27 pages exist under `Source_2/Docs` (no Entities, Path Tool, Smart Props, Displacement or Paint pages), so the value here is in the file-format, CLI and FGD pages. The biggest trap: **an agent searching the wiki for "VMDL" lands on the legacy Model Editor format and will write the wrong file.** ([Source 2](https://developer.valvesoftware.com/wiki/Source_2))

**Detect the file dialect from line 1 before editing**

| Line 1 starts with | Dialect | Edit with |
| --- | --- | --- |
| `<!-- kv3 … format:modeldocNN:… -->` | ModelDoc `.vmdl` (`_class` nodes) | ModelDoc tools / KV3 |
| `<!-- schema text {7e125a45-…} generic {198980d8-…} -->` | Legacy Model Editor `.vmdl` (`CVModel`, `m_*` keys) | Don't mix; convert via ModelDoc File → Import → "Import and Convert old .VMDL" |
| `<!-- kv3 … format:generic:… -->` + `m_sMDLFilename` | Source 1 `.mdl` wrapper | Leave as is |
| `<!-- kv3 … format:generic:… -->` | `.vsndevts`, `.vdata`, generic KV3 | KV3 |
| `<!-- dmx encoding keyvalues2_noids 1 format vtex 1 -->` | `.vtex` | DMX text |
| binary / `<!-- dmx encoding binary N format vmap M -->` | `.vmap` | `dmxconvert` to keyvalues2 first; N and M vary by game and era |

**Verified ModelDoc node classes** ([Common ModelDoc Nodes](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools/Modeling/Common_ModelDoc_Nodes), [ModelDoc Editor](https://developer.valvesoftware.com/wiki/ModelDoc_Editor))

`RenderMeshList`, `RenderMeshFile`, `RenderPrimitiveBox`, `RenderPrimitiveSphere`, `LODGroupList`, `LODGroup`, `LODGroupALL`, `MaterialGroupList`, `MaterialGroup`, `DefaultMaterialGroup`, `PhysicsShapeList`, `PhysicsShapeBox` (+ Capsule/Sphere), `PhysicsHullFile`, `PhysicsMeshFile`, `PhysicsHullFromRender`, `PhysicsJointList`, `PhysicsJointRevolute`, `Skeleton`/`Bone`, `BoneMarkupList`, `AnimFile`, `EmptyAnim`, `GenericGameData`, `BreakPiece`, `BreakPieceEmbedded`, `BreakPieceExternal`, `BreakPieceSimpleParticles`, `RenameStream`. Still unverified: bodygroup, attachment, hitbox, jiggle-bone and cloth class names. Tip: Outliner → "Show Empty Categories" lists every node type in-app.

**Legacy Model Editor schema (useful for semantics, not syntax)** ([VMDL](https://developer.valvesoftware.com/wiki/VMDL))

- LODs: max **8**; first LOD switch distance must be **0** or the base mesh never shows.
- Material groups: the first must match the default materials; every group must have the same material count.
- Hitboxes: boxes aligned to the parent joint, no rotation; per-hitbox `m_sSurfaceProperty`, `m_nGroupId`.
- Attachments: influences with translation/rotation offsets and weight; "Is Root" for jointless models.
- Bodygroups = mesh groups (`m_bEnabledByDefault`, `m_meshList`, `m_physList`) — bodygroups can swap collision too.
- Physics mesh defaults: auto mass on, mass override 100, exact geometry off.

**FBX and Blender** ([FBX](https://developer.valvesoftware.com/wiki/Dota_2_Workshop_Tools/Modeling/FBX), [Exporting from Blender](https://developer.valvesoftware.com/wiki/Source_2/Docs/Modeling/Exporting_a_mesh_from_Blender))

- Material binding order: (1) FBX material custom string property **`FBX_vmatPath`** (case-sensitive, e.g. `materials/models/props_items/red`); (2) material name; (3) texture filenames; (4) texture names with `_color`/`_normal` stripped.
- Blender export: Apply Scalings "FBX All", **Forward X, Up Z**, Custom Properties on, Ctrl+A apply rotation + scale; prop faces −Y in Blender. Units: inches, or 6 Blender units = 10 Hammer units with unit system None.
- Vertex blend on a model: ModelDoc right-click render mesh → Add RenameStream `color$0` → `VertexPaintBlendParams$`; black = layer 1, red = 2, green = 3. The map compiler doesn't see blended surfaces on models — split hulls per surface type.

**Command-line tools** ([Resourcecompiler](https://developer.valvesoftware.com/wiki/Resourcecompiler), [Dmxconvert](https://developer.valvesoftware.com/wiki/Dmxconvert), [Command line options](<https://developer.valvesoftware.com/wiki/Command_line_options_(Source_2)>))

- resourcecompiler: `-i`, `-filelist`, `-r`, `-nop4`, `-game`, `-v`, `-f`, `-fshallow`, `-fshallow2`, `-pc` (default), `-novpk`, `-vpkincr`, `-pauseiferror`, `-pause`, `-changelist`, `-p4cachefiles`. Needs a GPU. Map components can't be compiled separately (e.g. only VENTS).
- dmxconvert: `dmxconvert -i <in> [-ie <hint>] [-o <out>] [-oe <encoding>] [-of <format>]`, plus `-r` and `-upconvert`. **"If no output file is specified, dmx to dmx conversion will overwrite the input"** — always pass `-o`. Encodings include `keyvalues2`, `keyvalues2_noids`, `binary`, `binary_seqids`.
- cs\_mdl\_import: `cs_mdl_import -game csgo -r -i <in> -o <out> *.mdl` (`-lods`, `-YupToZup`, `-header_hull_bounds`).
- Launch: `-tools`, `-addon <name>`, `-vconsole`, `-console`, `-dev`, `-gpuraytracing`, `-nostyle`, `-retail`, `-insecure`, `-allowmultiple`, `-vulkan`, `-novr`; `+map x +buildcubemaps`.
- Porting a VMF: Hammer File → Open with filter "Source 1 Map Files (.vmf)", after adding a `Source1Import` block to gameinfo (`importmod`, `importdir`, `createStaticOverlays`, `createPathParticleRopes`).

**`.vtex` schema** — DMX `keyvalues2_noids`: `CDmeVtex` → `m_inputTextureArray` (`CDmeInputTexture`: `m_fileName`, `m_colorSpace`, `m_typeString`), `m_outputTypeString` (2D/CUBE…), `m_outputFormat` (DXT1/DXT5/BC7/RGBA16161616F…), `m_textureOutputChannelArray` (`CDmeTextureOutputChannel`: `m_srcChannels`, `m_dstChannels`, `m_mipAlgorithm` / `CDmeImageProcessor`). Needed for particle sprites, gradient/cubemap fog and vertex-animation textures.

**FGD syntax (the entity source of truth)** ([Counter-Strike 2.fgd](https://developer.valvesoftware.com/wiki/Counter-Strike_2.fgd), [Base.fgd/CS2](https://developer.valvesoftware.com/wiki/Base.fgd/Counter-Strike_2))

- Property: `name(type) [ group="…" min="0" max="1" report ] : "Label" : default : "Tooltip"`; conditional `enabled={ variable="x" value="0" }`.
- Directives: `@BaseClass`, `@PointClass`, `@SolidClass`, `@OverrideClass`, `@PathClass`, `@PathNodeClass`, `@EntityGroup`, `@VisGroupFilter`, `@include`, `@exclude`.
- Types beyond Source 1: `resource:material`, `resource:texture`, `resource:vsnap`, `instance_file`, `node_id`, `animgraph`, `animgraph_enum`, `sequence`, `particlesystem`, `scriptlist`, `vdata_choice:<file>`, `curve`, `parentAttachment`.
- `metadata{}`: `entity_tool_name` / `entity_tool_group` / `entity_tool_tip` (drive Hammer's Entity Tool palette), `auto_apply_material`, `class_game_keys {isPointPrefab, targetMapName}`.
- The wiki's `Prop_dynamic`, `Logic_script`, `Prop_Data` and `Clip_texture` pages are Source 1-only — parse the game's FGDs instead.

**Entity conventions** ([List of entities](<https://developer.valvesoftware.com/wiki/List_of_entities_(Source_2)>))

- Every entity: `origin`, `angles`, `scales`, `transformLocked`, `force_hidden`, `editorOnly`, `targetname`, `parentname`, `parentAttachmentName` (`!bonemerge`), `local.origin/angles/scales`, `useLocalOffset`, `vscripts`.
- Common inputs: Kill, KillHierarchy, AddOutput, FireUser1–4, SetParent, SetParentAttachment(MaintainOffset), ClearParent, RunScriptFile/Code, CallScriptFunction. Outputs OnUser1–4, OnKilled.
- AddOutput syntax: `<output> <target>:<input>:<param>:<delay>:<max times>` (−1 = infinite). Special targets `!self`, `!activator`, `!caller`, `!picker`.
- Spawnflags are stored one key per bit: `spawnflags#<bit>`.
- Light2 family (CS2): `brightness_units`, `brightness_lumens` (1000 lm default), `colortemperature`, `directlight` 0 None / 1 Static / 2 Stationary / 3 Dynamic, `castshadows`, `shadowmapsize`, `bouncelight`.
- Steam Audio lives on worldspawn: `steamaudio_reverb_*`, `steamaudio_pathing_*` (incl. grid spacing).
- `trigger_multiple` `wait` 0 means 0.2 s; −1 fires once. `logic_auto` refires on every CS2 round reset.
- Debug I/O: `ent_messages_draw 1`, `ent_pause`, `ent_step`.

**Hammer (game-agnostic)** ([Hammer Overview](https://developer.valvesoftware.com/wiki/Source_2/Docs/Level_Design/Hammer_Overview), [Mesh Editing 4](https://developer.valvesoftware.com/wiki/Source_2/Docs/Level_Design/Basic_Construction/Mesh_Editing_4))

- New hotkeys: Tab local-space gizmo; Shift+Q pick workplane (Esc resets); M merge; Ctrl+T tie to entity (type classname **and press Enter** or it won't apply); Shift+G repeat command; Ctrl+G Fast Texture Tool; Ctrl+Shift+P Map Properties; Ctrl+Shift+V Paste Special; Z fly; Shift+A recentre; F2/F3/F4 Top/Front/Side.
- **F inverts faces in Mesh/Face mode but bevels in Edge mode.** Shift+P is bound to both Texture Projection and Path Tool. Shift+drag copies only in Meshes mode (else extrudes).
- Scale tool: after dragging an axis, type a factor on the numpad ("2.0"). Ctrl+LMB disables angle snap.
- Prefabs: Selected Objects → New Prefab From Selection ("Maintain World Offset" / "Center On Origin"); edit by double-clicking in the 3D view, not the Outliner; expose properties with the chain-link icon; route I/O with Map → Map Relays. Instances are local to their map.
- "Sealing maps against the void is no longer required" (but helps vis); "toolsnodraw is no longer needed on unseen surfaces. Delete those faces instead."
- Object Properties → settings icon → "Raw Data" shows real keyvalue names.

**Source 2 Viewer** decompiles `vmap_c → vmap`, `vmdl_c → vmdl`, `vmat_c → vmat` "retaining as much data as possible", creates VPKs from a folder, and reads `.nav` and shaders — the practical way to get real `.vmat` and ModelDoc text from shipped content. ([page](https://developer.valvesoftware.com/wiki/Source_2_Viewer))

## Dota 2 & SteamVR Home tools

Dota 2 and SteamVR Home have Valve's oldest and most detailed tool docs. The **Particle Editor material transfers almost entirely** — the [Particle Editor page](<https://developer.valvesoftware.com/wiki/Particle_Editor_(Source_2)>) says it "still applies to … all newer Source 2" games, including HL:Alyx and CS2. Deadlock and Artifact have no tool docs. Transfer to CS2 / HL:Alyx is noted per item.

**Particles** (transfer: high) ([Overview](https://developer.valvesoftware.com/wiki/Dota_2_Workshop_Tools/Particles/Particle_System_Overview), [Properties](https://developer.valvesoftware.com/wiki/Source_2_Particle_System_Properties), [Performance](https://developer.valvesoftware.com/wiki/Dota_2_Workshop_Tools/Particles/Particle_Performance))

- **Minimal working effect** (editor display names): Render sprites + Lifespan Decay + Movement Basic + Position Within Sphere Random (speed max 200) + Emit continuously. Save as `particles/<name>.vpcf`. This is the template `create_particle(template="blank")` should produce.
- "Initializers only set up starting properties … operators take over" each frame.
- Base property defaults (omit when unchanged): Initial Particles 0, **Max Particles 1000** (Performance page says 1004), bounding box ±10, Color 255 255 255 255, Radius 5, Lifetime 1, Max Draw Distance 100000, Time To Sleep When Not Drawn 8 s. Some properties appear only with **"Show Hidden Attributes"**.
- Control points: up to **64**; CP0 = system origin and orientation; CPs carry only scalars and vectors; editor CP values "only exist in the Particle Editor". Write expensive results to a CP that children read.
- Renderers: sprites use **`.vtex`**; projected and cable renderers use **`.vmat`**.
- Collision constraint modes: 0 per-particle trace (default, expensive), 1 per-frame plane set, 2 initial trace down, **3 nearest trace (recommended)**. Preview needs "plane geometry" ground to show collision.
- Performance: memory is reserved for Max Particles, so set the real count; keep counts **multiples of 4** (SIMD); a system and its children share a thread.
- Catalogues of editor display names: 47 operators, 26 initializers, 3 emitters, 4 constraints, 4 forces (in project notes). Gotchas: "Rotation Speed Random" needs a "Rotation Basic" operator; "Sequence Random" extreme values can crash. **The wiki's Particle System Renderers page is Source 1** — its `render_animated_sprites` names don't apply.
- Copy/branch an effect tree: File → "Copy System Definition…" with name remapping, Preview and overwrite guard ([Particle Copy Tool](https://developer.valvesoftware.com/wiki/Dota_2_Workshop_Tools/Particle_Copy_Tool)).
- **Two `.vpcf` text formats exist**: legacy "schema text" and KV3. `game\bin\win64\vpcf_to_kv3.exe -r *.vpcf` converts in place (documented for Dota; presence in CS2/HL:Alyx bin unverified). Detect by header; never hand-write schema text.
- Snapshots: `.vsnap` (from a model "fx" mesh) feed "Init from CP snapshot" initializers.

**Materials** (transfer: medium — UI shared, shader names differ)

- Material Editor: File → "Force Compile To Disk"; the Preview "Log" tab shows compile errors (usually bad image paths); Shader Features gate which variables appear.
- Dynamic expressions (triangle → f(x)): float4 math with `time()`, `lerp`, `smoothstep`, `random(min,max)`, `exists(attr)`; missing attributes silently fall back. CS2 attributes include `$ent_health`, `$ent_age`, `$ent_random`, `$liquid_surface`.
- Material attributes can drive compile behaviour (Dota: `dota.nav.walkable 1`).
- Sky material: shader Sky, lat-long 8192×4096 or 4096×2048. Gradient fog texture: power-of-two with alpha (RGB colour, alpha opacity), referenced from a hand-made `.vtex`.

**Models** (transfer: rules high, legacy UI low)

- Red glowing mesh = missing material → material remap. Glowing wireframes = bad FBX export.
- Collision is convexified by default; "Collision using exact geometry" only for static props (also fixes props falling through floors).
- Animation: "Source2 only deals with skeletal animation"; animation FBX = skeleton only; idle = copy an animation with end frame 0; sound on animation via event `AE_CL_PLAYSOUND` + soundevent in Data String.
- Folder names: "only lower case letters, numbers and underscores."

**Hammer** (transfer: high)

- Extra hotkeys: V edge loop, Alt+B interpolated bridge, keypad +/− grow/shrink selection, comma/period subdivision level, Shift+O toggle editor-only objects, Shift+Arrow duplicate entity.
- **Command History** panel + Shift+G "repeat command" (can repeat several) — Hammer's only native macro facility ([page](https://developer.valvesoftware.com/wiki/Dota_2_Workshop_Tools/Level_Design/Command_History)).
- Duplicate targetnames show in **bold orange** in Object Properties.
- World layers: Tools → Create World Layer + `info_world_layer` (`ShowWorldLayerAndSpawnEntities`).
- Legacy Source 1 entities that do nothing in Source 2: `ambient_generic`, `func_areaportal(window)`, `func_occluder`, `func_lod`, `func_viscluster`, `info_overlay`, `infodecal`, `light_irradvolume`.
- No lights → map renders fullbright. Delete the `_vrad3` folder before upload (it can crash the upload). 8k Final lightmaps need \~64 GB RAM.

**Sound** (mechanics transfer; type names don't)

- Source: 16-bit PCM 44.1 kHz WAV; always reference as `.vsnd`. `vsnd_files` takes a string or array (random, no repeat until exhausted).
- "A map must be running for the soundevent file to compile." Test with `snd_sos_start_soundevent <name>`; parse errors appear on the VConsole `ResourceCompilerSystem` channel.

**Tool configuration** ([Sdkenginetools.txt](https://developer.valvesoftware.com/wiki/Sdkenginetools.txt))

- `m_EngineTools` / `m_ExternalTools`; `m_LimitToMods`, `m_ExcludeFromMods`; `m_ToolIcon` with `game:` prefix (CS2).
- **`m_Command` placeholders**: `{FilePath}`, `{FileFolder}`, `{FileName}`, `{GameFolder}`, `{ContentFolder}`, `{ModName}`, `{LineNo}`, `{ColNo}`. An `m_ExternalTools` entry could add a right-click "Send to agent" in the Asset Browser. Steam "verify files" reverts edits.

**Scripting and debugging** (HL:Alyx yes, CS2 no)

- VConsole: **SteamVR docs say "Set the port to 29009"** (earlier research: 29000) — probe both. Filter on `VScript` and `ResourceCompilerSystem` channels.
- `script_attach_debugger` / `cl_script_attach_debugger`; `cl_panorama_script_help_2` dumps the live Panorama API; Lua map scripts in `scripts/vscripts/map_scripts/<map>.lua` (SteamVR).

**QA checklist pattern** ([SteamVR Checklist](https://developer.valvesoftware.com/wiki/SteamVR/Environments/Checklist)): prop black → add light; lit one side → add box cubemap; falls through floor → exact-geometry collision; black void → add sky; delete test maps from both content and game before submitting. A ready template for `report_ready`.

## New tools and description changes

The wiki suggests about 15 new tools and a handful of wording changes to the main tab's boilerplate. The highest-value additions are dialect detection, template copying, FGD lookup and a particle linter, because each removes a whole class of silent failure for weak agents.

**New tools** (same `name(arg: type = default): description` form)

```
detect_asset_dialect(path: str): Classify a source file from its first line: modeldoc_kv3 | legacy_model_editor ("schema text {7e125a45-…}") | s1_mdl_wrapper (m_sMDLFilename) | generic_kv3 | vpcf_kv3 | vpcf_schema_text | vtex_dmx | vmap_dmx_binary | vmap_dmx_text. Call before any edit. The wiki's VMDL/* pages describe the LEGACY format; never apply them to ModelDoc files.
new_map_from_template(template: "cs2_addon_template"|"cs2_defuse"|"cs2_hostage"|"cs2_wingman"|"hlvr_basic_setup", name: str, confirm): Copy a Valve template map into the addon. The CS2 addon template already has spawns, buyzones, bombsites and dev notes; prefer this over building from an empty map.
list_examples(kind: "map"|"material"|"model"|"prefab"|"soundevents"|"any" = "any"): Valve's shipped example files (zoo maps, workshop_examples, environment_settings prefabs, breakable_glass). These are the authoritative text templates for .vmat and ModelDoc, which the wiki doesn't document.
list_asset_types(): Parse game/bin/assettypes_common.txt: every source extension with its compiler (m_Ext → m_CompilerIdentifier) and block encoding. Use instead of guessing what compiles.
read_fgd_class(classname?: str, prefix?: str): Keys (with types, defaults, choices), inputs and outputs from the game's FGDs, honouring @include / @OverrideClass / @exclude. Returns "excluded in this game" for classes CS2 removes (light_spot, env_cubemap, func_instance…). Wiki entity pages are often Source 1-only; this is the source of truth.
lint_entities(map: str): Flag excluded classes, Source 1 entities that do nothing in Source 2 (ambient_generic, func_areaportal, func_lod, info_overlay…), duplicate targetnames, env_cs_place on a non-box mesh, prop_dynamic using a physics-only model (deleted on load), func_breakable in CS2 (no gibs), unknown keys.
lint_model(path: str): First LOD at distance/threshold 0; ≤ 8 LODs; material groups equal in count with group 0 = defaults; PhysicsMeshFile only with prop_static; prop_physics has prop_data; bone names without dots; morphs have a Morphs Supported material + EmptyAnim; no RenderMeshFile = won't compile.
lint_particle(path: str): Max Particles left at default (suggest the real count, multiple of 4); collision mode 0 (suggest 3); default ±10 bounds on a large effect (culling); sprite renderer pointing at .vmat or cable/projected renderer at .vtex; Rotation Speed without Rotation Basic; duplicate functions where the last one wins; legacy schema-text format.
branch_particle(source: str, rename_from: str, rename_to: str, overwrite: bool = false, dry_run: bool = true, confirm): Copy a particle system and all children with names remapped, rewriting child references (the editor's "Copy System Definition…"). Prefer branching a shipped effect over writing one from scratch.
add_soundevent(name: str, files: str[], type?: str, volume: float = 1.0, confirm): Write into the game's required file (CS2: soundevents/soundevents_addon.vsndevts; HL:Alyx: <addon>_soundevents.vsndevts + .vrman manifest), reference .vsnd paths, pick vsnd_files vs vsnd_files_track_01 by type, then verify with snd_list_soundevents. Soundevents only compile while a map is running.
scaffold_cs_script(name: str, inputs?: str[], confirm): Create scripts/<name>.js importing cs_script/point_script, place a point_script with cs_script=<name>.vjs, and wire RunScriptInput params to matching Instance.OnScriptInput handlers. Fails if any wired param has no handler.
attach_lua_script(entity: str, path: str, think: bool = false, confirm): HL:Alyx only. Write game/hlvr_addons/<addon>/scripts/vscripts/<path>.lua and set vscripts. Uses RedirectOutput (ConnectOutput doesn't work) and SpawnEntityFromTableSynchronous. Refuses .lua.txt.
set_game_mode_and_load(map: str, mode: "casual"|"competitive"|"wingman"|"deathmatch"|"armsrace"|"custom"): CS2's map command takes no mode, so this sends game_type/game_mode first, then map.
split_3d_skybox(selection: Selector, confirm): Move the selection into <map>_skybox, set Map Type 3D Skybox, place skybox_reference at 0,0,0 and copy light_environment + env_sky.
enable_particle_editor(confirm): CS2 only. Remove csgo from m_ExcludeFromMods (pet) and m_HideForRetailMods (particle_asset). Idempotent; warns Steam verify reverts it.
vtex_create(path: str, inputs: [{file, color_space}], format: "DXT1"|"DXT5"|"BC7"|"RGBA16161616F"|…, type: "2D"|"CUBE" = "2D", mips: bool = true, confirm): Write a DMX keyvalues2_noids .vtex for particle sprites, fog gradients and vertex-animation textures.
lighting_diagnostics(view: "complexity"|"reflection"|"probe_grid"|"luxels"): Switch the visualisation, take a look, and explain the legend (CS2: orange = too many lights; HL:Alyx: cyan).
publish_preflight(): Stale _c files, .los and _vrad3 folders, test maps in content and game maps/, VpkDirectories drops, missing sky/light/cubemap, addoninfo maps entry.
```

**Changes to the main tab's boilerplate**

- `build_map`: drop any mention of `-lightmapCPU` (removed in the Armory update). Add: "Hammer requires the game running."
- `run_console_command` / `health`: probe VConsole on **29000 and 29009**; one client only.
- Any DMX conversion: always pass `-o` — dmxconvert overwrites the input otherwise.
- `edit_kv3`: validate flag spelling (`resourcename`, not `resource_name`); refuse unknown format GUIDs; allow the generic header only for generic types.
- `place_entity` / `edit_entity`: write spawnflags as `spawnflags#<bit>` keys; Ctrl+T-style mesh entities need the classname confirmed.
- `place_light`: CS2 → `light_omni2` / `light_barn` / `light_rect` (static only); brightness in lumens; `directlight` 0–3.
- `import_model`: set `FBX_vmatPath` on materials when coming from Blender; document Forward X / Up Z / FBX All.
- `create_particle`: base template = the five-function minimal effect; sprites take `.vtex`.
- Description wording to reuse: "F inverts faces in Mesh/Face mode but bevels in Edge mode"; "Control point values in the editor are preview-only"; "Lua scripts live on the game side, not content"; "1 unit = 1 inch = 2.54 cm, Z up".

## Crawl log

Four parallel crawls logged about 460 wiki URLs; roughly 420 returned content and about 25 distinct pages stayed blocked (fetch `client_error`, no workaround attempted). Full per-page notes are saved in the project under `research/vdc/`.

| Crawl | URLs logged | Scope |
| --- | --- | --- |
| CS2 Workshop Tools | 89 | All 37 CS2 subpages + CS2 FGD, maps, game modes, related Source 2 pages |
| HL:Alyx Workshop Tools | 112 | Modeling, AnimGraph, Lua, level design, sound, examples (82-page category) |
| Source 2 core | 143 | Source\_2/Docs (all 27), VMDL/VMAT/VTEX/VMAP/KV3/DMX, CLI tools, FGDs, entity lists |
| Dota 2 & SteamVR Home | 119 | Particles, materials, modeling, Hammer, sound, sdkenginetools, Deadlock/Artifact (stubs) |

**Still blocked** — several have parentheses in the title, which the fetcher seems to choke on. Opening these in a browser and pasting or attaching them would close the remaining gaps, especially the first three:

- [Material\_Editor\_(Source\_2)](<https://developer.valvesoftware.com/wiki/Material_Editor_(Source_2)>) — likely has `.vmat` detail
- [VMDL\_(Source\_2)](<https://developer.valvesoftware.com/wiki/VMDL_(Source_2)>) and its subpages
- [VPCF](https://developer.valvesoftware.com/wiki/VPCF) — likely has the vpcf text form
- [CS2 Animgraph](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Animgraph), [CS2 Maps Workshop](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Level_Design/Maps_Workshop)
- [Surface\_properties](https://developer.valvesoftware.com/wiki/Surface_properties), [Hlvr.fgd](https://developer.valvesoftware.com/wiki/Hlvr.fgd), [Inputs\_and\_Outputs](https://developer.valvesoftware.com/wiki/Inputs_and_Outputs), [Targetname](https://developer.valvesoftware.com/wiki/Targetname)
- [Counter-Strike\_2/Console\_commands](https://developer.valvesoftware.com/wiki/Counter-Strike_2/Console_commands), [HL:Alyx console commands](https://developer.valvesoftware.com/wiki/List_of_Half-Life:_Alyx_console_commands_and_variables)
- [Cs\_minimap\_boundary](https://developer.valvesoftware.com/wiki/Cs_minimap_boundary), [Post\_processing\_volume](https://developer.valvesoftware.com/wiki/Post_processing_volume), [Smartprop\_Variables](https://developer.valvesoftware.com/wiki/Smartprop_Variables)
- HL:Alyx Light\_Cookies; Dota Terrain\_Editing and Height\_Fog; SteamVR Fog\_Tutorial\_VTEX and lightcookies

**Caveat**: the fetcher summarises pages and caps quotes at \~125 characters, so long passages are paraphrased; short code, keys and labels are exact. The CS2 console-variable page covers A–L only (build 9832).
