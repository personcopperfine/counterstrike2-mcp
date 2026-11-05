# Source 2 MCP — Workflow Research & Tool Boilerplate

Oct 24, 2026 · @M

## Purpose & key takeaways

No Source 2 / Hammer 5 MCP server exists in public yet; the closest analogue is hammer-mcp (Source 1, 77 tools), and the most transferable patterns come from it, Blender Lab, ahujasid's blender-mcp, Epic's Unreal MCP, Unity MCP and Houdini MCP. The goal is a server whose tools carry the workflow themselves, so a weaker agent succeeds without a smarter one steering.

What makes weaker agents succeed, in order of impact:

1. **Encode the workflow in three places** — server `instructions` (what it's for, first call, recurring traps), an MCP prompt per editor (numbered steps + a written definition of done), and cross-references inside each tool description ("Run `validate_vmat` first"). Weak models lose early context; the most recent text wins.
2. **Every mutating result returns `next_step`.** The model follows the last instruction it read.
3. **Errors are instructions** — list valid choices, did-you-mean, the correct sibling tool, and whether retrying is safe.
4. **Lookups before writes.** Agents must never invent asset paths, shader names or entity classnames; give them `find_assets`, `read_shader_params`, `read_entity_class` that return names in exactly the form the writers accept.
5. **Validate inside the tool, write atomically.** Compile-check after every write and refuse to leave a half-built asset.
6. **Compound tools for the hard parts** (e.g. `build_room`, `create_prop_from_mesh`) — weak models fail at long fine-grained sequences.
7. **A verdict tool as the stopping condition** (`report_addon_ready`) with pass/warn/fail/skipped — never "pass" when nothing was checked.
8. **Vision is mandatory in "done".** hammer-mcp found agents called its render tool once in 145 calls; the fix was a one-call tour image plus the rule "done is three greens and a look".
9. **Code execution is a last resort**, labelled as such in its description.
10. **Name by side effect**: `read_*` observes, `run_*` executes, `verb_noun` mutates — backed by MCP `readOnlyHint` / `destructiveHint`.

On the "training data" angle: models know Blender's `bpy` far better than any Source 2 API. Exploit that by making Source 2 tools *feel* like the Blender/Unreal ones (`get_scene_info`, `get_object_info`, `look`, `execute_*_code`, `import_asset(target_size=…)`), and by speaking in metres-and-inches conversions where the model will otherwise guess.

## Reference MCP servers

Seven servers were read at the source-code level (tool schemas and docstrings, not READMEs). hammer-mcp is Source 1 (VMF/BSP, vbsp/vvis/vrad) but its patterns transfer almost one-for-one.

| Server | Architecture | Tool style | Vision | Workflow encoding |
| --- | --- | --- | --- | --- |
| [hammer-mcp](https://github.com/Sycatle/hammer-mcp) | stdio, offline file edits + Wine compilers; never touches a live engine | 77 narrow tools: `read_*` / `run_*` / `verb_noun`; `confirm`, `dryRun`, `.bak` on writes | Offline rasteriser: `render_vmf_view`, `render_vmf_tour` (contact sheet), `render_vmf_plan` | Server `instructions` + Claude skill with build order + "done" definition |
| [Blender Lab MCP](https://www.blender.org/lab/mcp-server/) (official) | FastMCP → TCP socket add-on :9876; every tool ships bpy code; `*_for_cli` headless twins | Summaries (`get_objects_summary`, `get_object_detail_summary`), docs search, `execute_blender_code` as last resort | Window/area PNG **plus a JSON "screenshot"** for non-vision models | Long `instructions` primer: each line names a trap + fix |
| [ahujasid/blender-mcp](https://github.com/ahujasid/blender-mcp) | Add-on socket :9876 | v2: consolidated to \~6 tools (`get_scene_info`, `execute_blender_code`, `look`, `search_assets`, `import_asset`, `generate_3d`) | `look(mode=viewport\|camera\|angles\|frames)`, restores settings afterwards | Classic `asset_creation_strategy` MCP prompt; v2 instructions |
| [Epic Unreal MCP](https://dev.epicgames.com/documentation/unreal-engine/unreal-mcp-in-unreal-editor?lang=en-US) (UE 5.8, experimental) | HTTP inside editor :8000, game thread | Toolsets behind `list_toolsets` / `describe_toolset` / `call_tool` | Async image results | Claude Code plugin skills + in-editor `AgentSkillToolset` |
| [flopperam/unreal-engine-mcp](https://github.com/flopperam/unreal-engine-mcp) | WebSocket plugin | `*_brief` → `*_inspect` → `*_edit` → `*_commit`; macro builders (`construct_house`) | `window_capture` | `skills` tool loads workflow docs on demand |
| [CoplayDev/unity-mcp](https://github.com/CoplayDev/unity-mcp) | Editor package + Python | `manage_*(action=…)` multiplexed; `batch_execute`; `editor_state.advice` readiness | `manage_camera(screenshot, batch="surround")` | "Resource-first" skill |
| [capoomgit/houdini-mcp](https://github.com/capoomgit/houdini-mcp) | Socket | `get_parameter_schema` → `set_parameters` (did-you-mean); `find_error_nodes`, `cook_node` | `render_quad_views` | Docstrings |

**Source 2 prior art to reuse or talk to:**

- **Source 2 Viewer (VRF) MCP server** — debug builds only, `dotnet run --project GUI -- --mcp`, endpoint `http://127.0.0.1:13338/mcp`. Opens files, moves the camera, inspects entities/models/particles, screenshots. Strong candidate for your vision backend on compiled assets. [docs](https://s2v.app/ValveResourceFormat/guides/mcp-server.html)
- **Source2Viewer-CLI** — decompiles `_c` files, `--vpk_create`, glTF export, `--kv_flatten` for diffs. [docs](https://s2v.app/ValveResourceFormat/guides/command-line.html)
- **[Hammer5Tools](https://github.com/dertwist/Hammer5Tools)** — CS2 toolkit (SmartProp editor, soundevents, navmesh→radar) with headless CLI and MCP stdio modes (read-only).
- **[dota2-mcp](https://github.com/Demon673/dota2-mcp)** — working relay on the **VConsole2 port 127.0.0.1:29000** (`console_send`, `dota_compile_asset`, `vfx_preview`). Needs `-tools` or `-vconsole`; the port accepts one client, so a relay blocks the vconsole GUI.
- **[cs2-hammer-mcp](https://github.com/roemer2201/cs2-hammer-mcp)** — design doc only; key finding: `.vmap` is DMX, not KV3.

## Cross-cutting patterns worth copying

These are the concrete mechanics behind the takeaways, with wording you can lift. Quotes are verbatim from source code.

**Start-here / health tool.** hammer-mcp `health`: *"Start here when a tool reports something missing, or is missing itself."* ahujasid `get_addon_status`: *"Call it once at the start."* Report dependencies, versions, tool count, stale build — and never throw.

**Scene inspection that scales.** ahujasid v2 `get_scene_info(query, root, fields, limit=20)` returns one header line + one line per object, with named field groups (`placement`, `contents`, `health`, `settings`). Bad field → `"Error: unknown fields X. Pick from: …"`. Blender Lab: *"Walk the collection hierarchy for an overview first. Don't dump entire scenes - inspect progressively."*

**Say what a tool is NOT for.** Unity: *"NOT for searching — use the find\_gameobjects tool … NOT for component management — use the manage\_components tool."* hammer: *"Whether the material exists is NOT checked here: use read\_game\_content for that."*

**`next_step` in every mutating result.** hammer clip: *"A cut through a wall of the hull opens the map. Compile and read\_leak."* ahujasid import: *"Afterwards check the reported bounding box, put the object on the ground, and look at it."*

**Errors as instructions.** Godot: `"Possible solutions:\n- …"`. Houdini: *"Unknown names fail per-parameter with did-you-mean suggestions."* hammer on wrong file type: names the right tools. mcp-core on a schema failure after a write: *"do NOT retry, read the state back instead."*

**Compiler-log translation.** hammer `read_compile_log` maps each regex to *"what the message actually means"* and the tool to run next — build the same table for resourcecompiler output (see Saving & compiling).

**Validate-then-write, atomically.** hammer: *"Every solid it writes is passed straight back through read\_vmf\_solids before the file is touched, and the write is refused if any of them fails."* Houdini: *"On invalid input the node is removed, never left half-configured."*

**Schema discovery before parameter writes.** Houdini `get_parameter_schema(path, pattern)` → names, types, ranges, menu options, then `set_parameters` in one undoable call. This is exactly what Material Editor and Particle Editor tools need.

**Batching.** Unity `batch_execute(commands, fail_fast)`: *"creating 5 cubes → use 1 batch\_execute with 5 create commands."* flopperam: *"Defer compile — batch many, then bp\_commit once."* One undo transaction per tool call.

**Long jobs.** ahujasid `generate_3d(job=…, wait_seconds=50)`: waits, then returns a resumable handle; *"Repeating a request whose generation is still unfinished resumes it instead of paying again."* Use for map builds.

**Readiness.** Unity `editor_state.advice {ready_for_tools, blocking_reasons, recommended_retry_after_ms}` and a server-side preflight returning `busy` + `retry_after_ms`. Apply during Hammer builds and asset compiles.

**Destructive-op gates.** `confirm: true` + `dryRun` + `.bak`; refuse empty selectors (*"refuses an empty selector rather than emptying the map"*); refuse to overwrite on create. Let an operator allowlist pre-approve, or long sessions get interrupted dozens of times.

**Code exec as last resort.** Houdini: *"LAST RESORT: prefer the dedicated tools … they validate input, report structured errors and are undoable as a single step."* Return full tracebacks; block always-fatal calls with the alternative in the message.

**Loose inputs.** Unity accepts `"true"`, `"5"`, `[x,y,z]`, `{x,y,z}`. Weaker models emit all of these.

**Honest verdicts.** pass / warn / fail / skipped; never pass when nothing was checked. hammer: *"Read `overall`, not `errorCount`."*

**Description length.** Epic says cut anything inferable from the signature; hammer keeps long domain-trap text. For weaker models: short signature facts, but always keep the trap, the units and the next-step text.

**Intent gating** (flopperam skill): *"Question → do NOT make changes. Inspection → do NOT modify anything. Modification → observe → execute → verify. When intent is unclear, default to answering rather than modifying."*

## Source 2 fundamentals & source-isms

Agents edit `content/` and the compiler writes `game/`; almost every Source 2 mistake an agent makes traces back to that split, the binary `.vmap`, or invented KV3 headers. Tags: **\[V\]** fetched source confirms, **\[V-comm\]** community tools only, **UNVERIFIED** needs checking against a real file.

**Directory split** \[V\] ([source2.wiki](https://www.source2.wiki/Basics/working-on-content/content-and-game))

- `content/<root>/<addon>/` = authored sources, never loaded by the game. `game/<root>/<addon>/` = compiled output, what ships. Roots: `csgo_addons`, `hlvr_addons`, `dota_addons`.
- Paths mirror and the extension gains `_c`: `content/.../materials/wall.vmat` → `game/.../materials/wall.vmat_c`. **Maps are the exception**: `maps/x.vmap` → `game/.../maps/x.vpk`.
- Asset references are relative to the addon root: `materials/wall.vmat`, not a full path.
- Game-side, hand-edited, never compiled: `addoninfo.txt`, `cfg/`, radar `.txt`, and HL:Alyx Lua in `scripts/vscripts/`.
- Filenames: lowercase, digits, `-`, `_` only; 260-char path limit (generated texture names add \~40).
- Deleting a source does **not** delete its `_c`; stale outputs keep loading and get uploaded.

**Asset types** \[V\] ([asset types](https://www.source2.wiki/FileFormats/asset-types))

| Asset | Source ext | Text format |
| --- | --- | --- |
| Map | `vmap` | **DMX** (binary by default) |
| Model | `vmdl` | KV3 (ModelDoc) |
| Material | `vmat` | KV1 `Layer0` (HL:Alyx confirmed; CS2 UNVERIFIED) |
| Texture | `vtex` | DMX keyvalues2 |
| Particle | `vpcf` | KV3 |
| Smart prop | `vsmart` | KV3 |
| Sound events | `vsndevts` | KV3 |
| Post-process | `vpost` | KV3 |
| AnimGraph 1 | `vanmgrph` | KV3 |
| AnimGraph 2 | `vnmskel` / `vnmclip` / `vnmgraph` | KV3 |
| Pulse graph | `vpulse` | KV3 |
| cs\_script | `.js` → `vjs`, `.ts` → `vts` | source |

**KV3** \[V\] ([VDC](https://developer.valvesoftware.com/wiki/KeyValues3))

- Header: `<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->`
- `key = value`, no commas between members; arrays `[ a, b, ]` (trailing comma OK); `//` and `/* */` comments; multiline strings in `"""`; flags like `resource:"path"`.
- **Never invent a format GUID** (`modeldoc28`, `vpcf`…). Copy the header from an existing file made by the same game's tools.

**`.vmap` is DMX** \[V\] ([dmx](https://www.source2.wiki/FileFormats/dmx))

- Binary header `<!-- dmx encoding binary 9 format vmap 35 -->`. Convert: `dmxconvert.exe -i map.vmap -o map_text.vmap -oe keyvalues2`, back with `-oe binary`. Or Hammer File → Save Copy As Text.
- Structure: `CMapRootElement` → `world` (`CMapWorld`) → `children` (`CMapEntity`, mesh nodes). Every element has a GUID `id`. Don't hard-code version 35.
- Libraries: Datamodel.NET (C#), datamodel.py (Blender Source Tools), datamodel-rs, sourcepp.

**Units and axes**

- 1 Hammer unit ≈ 1 inch; Z-up. 1 m ≈ 39.37 u. Give agents both numbers, because models think in metres from Blender data.
- Player hull (CS:GO reference, CS2 close): 32×32×72 standing, 54 crouched, eye \~64.

**Launching and mounting** \[V\]

- Launchers: `game/bin/win64/csgocfg.exe`, `hlvrcfg.exe`, `dota2cfg.exe`. Options: `-tools`, `-addon <name>`, `-vconsole`, `-retail`, `-vulkan`, `-gpuraytracing`, `+map foo`.
- **Tools mode mounts exactly one addon**; cross-addon assets render missing.
- Avoid "Edit Addon Map" (reported broken); use Launch Tools → Asset Browser.
- **CS2 Particle Editor is hidden**: remove `csgo` from `m_ExcludeFromMods` for `pet` in `game/bin/sdkenginetools.txt` and from `m_HideForRetailMods` for `particle_asset` in `game/bin/assettypes_common.txt`.

**Automation surface** (what an MCP server can actually drive)

| Surface | Game | Status |
| --- | --- | --- |
| `resourcecompiler.exe`, `dmxconvert.exe`, VPK CLIs | all | \[V\] |
| VConsole2 TCP 127.0.0.1:29000 (one client) | all, needs `-tools`/`-vconsole` | \[V-comm\] |
| cs\_script (`point_script`, JS) | CS2 | \[V\] |
| Lua vscript | HL:Alyx, Dota 2 | \[V\]; **not in CS2** |
| Hammer Python / public editor API | Valve games | no public evidence — treat as unavailable |
| Hammer C# API | s&box only (deprecated) | \[V, s&box\] |

Practical consequence: a Source 2 server is part *offline file editor* (KV3/DMX + resourcecompiler, hammer-mcp style) and part *live relay* (VConsole + screenshots). Valve exposes no Blender-style `execute_code` inside Hammer, so unless your server already has its own in-editor bridge, the escape hatch is `run_console_command` and raw KV3 edits.

**Don't trust** AI-generated mapping wikis (e.g. pages mentioning "Skybox 2.0", `vmap_performance_scanner`). Prefer VDC, source2.wiki and the [s2v.app schema explorer](https://s2v.app/SchemaExplorer/cs2/convars). s&box answers don't transfer to CS2/HL:Alyx.

## Saving & compiling

Asset editors (Material Editor, ModelDoc, Particle Editor) compile on save; maps compile only through Hammer's Build dialog (F9) or the equivalent resourcecompiler command. The single most useful automation trick: **Hammer's Build → Command Line tab prints the exact resourcecompiler invocation** for the current settings — read it rather than guessing flags. \[V\] ([Compiling](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Level_Design/Compiling))

**Map build presets (CS2)** \[V\]

| Preset | Stages | Lightmaps | Use when |
| --- | --- | --- | --- |
| Fast Compile | world, physics, nav | none | Iterating on layout |
| Entities Only | world (keeps lightmaps) | unchanged | Entity/I-O tweaks; add Build Physics for clip fixes |
| Full Compile | world, lighting, physics, vis, nav, Steam Audio, cubemaps | 1024, Standard | Test lighting |
| Final Compile | as Full | 2048, Final | Release (Lighting page suggests 8192/Final) |

**resourcecompiler** (`game/bin/win64/resourcecompiler.exe`) \[V\] ([source2.wiki](https://www.source2.wiki/EngineTools/ResourceCompiler))

- `-i <files>` (wildcards; **always add `-r` with wildcards**), `-filelist <txt>`, `-game <dir with gameinfo.gi>` when input isn't under `content/`.
- Force: `-f` all, `-fshallow` top level, `-fshallow2` top + children. Also `-novpk` (loose map output), `-skiptype vmat,vtex`, `-dependency_check_only`, `-v`, `-nop4`.
- Needs a GPU + driver to run at all.
- Real CS2 map invocation seen in the wild \[V-comm\]:

```
resourcecompiler.exe -threads 5 -fshallow -maxtextureres 256 -dxlevel 110 -quiet -unbufferedio
  -i "<...>\content\csgo_addons\<addon>\maps\<map>.vmap" -noassert
  -world -bakelighting -lightmapMaxResolution 1024 -lightmapDoWeld -lightmapVRadQuality 1
  -lightmapLocalCompile -phys -vis -nav -retail -breakpad -nop4 -outroot "<temp>" [-lightmapcpu]
```

- `-lightmapVRadQuality` 0/1/2 ↔ Fast/Standard/Final is UNVERIFIED. Steam Audio flags UNVERIFIED — read the Command Line tab.
- GPU baking needs RT cores (RTX 20xx+, RDNA2+, Arc). `-lightmapCPU` was removed in the Armory update — there is no CPU fallback.

**No leaks in Source 2.** Vis uses an inside/outside flood; gaps make vis spill and create junk clusters but don't abort the compile. Seal with `toolsskybox`. CS2 uses VIS3 (GPU meshlets); HL:Alyx uses VIS2 (keep contributors grid-aligned). Debug: Map → View Compiled Vis Data (`.los`), `vis_debug_show`, `sc_no_vis`. \[V\] ([visibility](https://www.source2.wiki/EngineTools/HammerEditor/visibility))

**Compile-log translation table** (seed for your `read_compile_log` tool)

| Message | Meaning | Fix / next tool |
| --- | --- | --- |
| `Failed to read scene_meshes.dmx` | `_vrad3/scene_meshes.dmx` > 2 GB | Reduce triangle count on heavy meshes |
| `Light mapper didn't return valid triangles` / `VK_ERROR_DEVICE_LOST` | NVIDIA driver issue | Driver 561.09 Studio recommended (CPU baking no longer exists); not `-lightmapcpu` |
| `Mesh with material <x> is extremely large in lightmap` | One face eating texel budget | Lower Lightmap Resolution Bias on that face |
| `Requested non-existent write path VRAD3_PATH!` | No confirmed fix | Report to user |
| Missing material / pink-black checker in game | Reference to an uncompiled or misspelled `.vmat` | `find_assets` then recompile |

**Saving rules for tools**

- Hammer autosaves are not a substitute: save the `.vmap` before every build (Epic's rule applies: *"Save first, then save again… MCP edits are not always undoable across compilation boundaries."*).
- After a source edit, recompile that one asset (`-fshallow`) and read back the `_c` mtime: don't trust exit code alone.
- Delete stale `_c` files when deleting sources; delete the `.los` VIS-debug file before publishing (can be hundreds of MB).

**Publishing** \[V\] ([Workshop Manager](https://www.source2.wiki/EngineTools/WorkshopManager), [mounting](https://www.source2.wiki/Basics/content-mounting))

- Workshop Manager packs the addon's **game side** into one VPK; `content/` stays local.
- CS2 packs only directories listed in `gameinfo.gi` → `AddonConfig` → `VpkDirectories`; everything else is **silently dropped**. Editing `gameinfo.gi` breaks VAC/signature checks and is reverted by updates.
- Retail clients only accept packed `.vpk` addons. 3D skybox map should be `<map>_skybox`. Without a `maps` key in `addoninfo.txt`, the first map alphabetically loads.
- Packing outside the tools: `Source2Viewer-CLI --vpk_create` (documented); `vpk.exe` (Source 2) exists but syntax is undocumented. 4 GiB limit.

## Materials & shaders

Agents should pick from shipped shaders only (custom `.vfx` can't load in CS2), name textures with the standard suffixes, and copy key spellings from an existing `.vmat` in the same game — the exact CS2 text form is the biggest unverified gap.

**Shaders**

| Game | Shader file | Use |
| --- | --- | --- |
| CS2 | `csgo_environment.vfx` | **Default for world geometry** |
| CS2 | `csgo_complex.vfx` | Props, emissive, tint mask |
| CS2 | `csgo_environment_blend.vfx` | Vertex-painted blend (2–3 layers; count UNVERIFIED) |
| CS2 | Glass, Static Overlay, Foliage, Simple 3layer Parallax, Water Fancy, Sky | `.vfx` names follow the pattern, UNVERIFIED |
| HL:Alyx | `vr_complex.vfx`, `vr_simple.vfx` | Props/world |

&#91;V\] ([Material Creation](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Materials/Material_Creation), [Csgo Complex](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Materials/Material_Creation/Csgo_Complex))

**Texture suffixes** (csgo\_complex inputs) \[V\]: `_color` (no baked lighting), `_normal`, `_rough`, `_metal`, `_ao`, `_trans`, `_mask` (tint: black none, white full), `_selfillum`, `_detail`, `_detailmask`, `_decal`. Power-of-two sizes. Normal-map Y convention is UNVERIFIED; test with a known bump.

**`.vmat` shape** (HL:Alyx, `Layer0` KV1 form) \[V-comm\] ([template](https://github.com/MarkMeltzer/bs2source2/blob/master/vmat_template_complex.txt))

```
Layer0
{
	shader "vr_complex.vfx"
	F_TRANSLUCENT 1
	TextureColor "materials/foo/foo_color.tga"
	TextureNormal "materials/foo/foo_normal.tga"
	TextureRoughness "materials/default/default_rough.tga"
	g_flMetalness "0.000"
	g_vColorTint "[1.000000 1.000000 1.000000 0.000000]"
	g_vTexCoordScale "[1.000 1.000]"
}
```

- `F_*` = static-combo feature flags (`F_ALPHA_TEST`, `F_TRANSLUCENT`, `F_SELF_ILLUM`…). `g_fl*` / `g_v*` / `g_b*` / `g_n*` = parameters. `Texture*` = source images; the editor auto-generates the `.vtex`.
- CS2 UI toggles: Translucent, Alpha Test (ref 0.01–0.99), Additive Blend, Tint Mask, Metalness Texture, Self Illum, Detail, Decal, Texture Animation, Secondary UV, Do Not Cast Shadows, Render Backfaces, Paint Vertex Colors.
- **UNVERIFIED for CS2**: `Layer0` vs KV3; layer-suffixed keys on environment shaders (`TextureColor1`?); surface prop key (believed `SystemAttributes { PhysicsSurfaceProperties "concrete" }`). Have the server read a shipped `.vmat` (or decompile a `.vmat_c` with Source2Viewer-CLI) and expose its keys through `read_shader_params`.

**Manual `.vtex`** — only for particle sprites, fog and similar. DMX keyvalues2 (`<!-- dmx encoding keyvalues2_noids 1 format vtex 1 -->`); keys `m_inputTextureArray` (`m_fileName`, `m_colorSpace` srgb|linear), `m_outputFormat` (e.g. DXT1), `m_textureOutputChannelArray`, `m_mipAlgorithm`. \[V\] ([VTEX](<https://developer.valvesoftware.com/wiki/VTEX_(Source_2)>))

**Hotspot texturing** — make a `.rect` in the Image Subrect Editor, set "Rectangle Definition File" + world mapping size in the material's Attributes, apply in Hammer with the hotspot tool or Fast Texture Tool (Ctrl+G). \[V\]

**Tool materials (CS2, `materials/tools/`)** \[V\] ([source2.wiki](https://www.source2.wiki/ToolTextures))

| Material | Effect |
| --- | --- |
| `toolsnodraw` (+ `_wood`/`_metal`/`_plastic`/`_grate`) | Collides, blocks vis + bullets, not rendered |
| `toolsskybox` | Seals the map, blocks vis (use instead of deprecated `skyvisblocker`) |
| `toolsclip` / `toolsplayerclip` / `toolsnpcclip` / `toolsgrenadeclip` | Clip for all / players / NPCs / grenades |
| `toolsinvisible`, `toolsblockbullets` | Bullet-solid clip / bullet blocker |
| `toolstrigger` | Trigger volumes |
| `toolsblocklight` / `toolssolidblocklight` | Blocks light (solid one also collides + vis) |
| `toolslightmapres` | Extra lightmap texel density in volume |
| `toolsnavclip` / `toolsnavattribute` | Block nav generation / nav attribute volume |
| `toolsblocksound` | Steam Audio blocker |
| `toolsinvisibleladder` | Climbable ladder |
| `visblocker` | Vis only, stripped from final map |
| `toolsblockbomb` | Legacy — use clip + `trigger_bomb_reset` |

**Surface properties** (`vsurf`) drive impact sounds, decals, particles, and auto-mass on physics hulls — so a wrong surface prop shows up as a wrong-weight prop, not just a wrong sound.

## Props (ModelDoc)

A prop is a `.vmdl` (KV3, ModelDoc format) that points at source meshes; a usable physics prop needs a render mesh, a physics hull with a surface prop, and `prop_data` game data — **models without `prop_data` cannot be used with `prop_physics`**. \[V\] ([Physics Prop](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools/Modeling/Physics_Prop))

**`.vmdl` skeleton** (verified class names; copy the header from a real file)

```
<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:modeldoc28:version{<COPY FROM EXISTING FILE>} -->
{
  rootNode =
  {
    _class = "RootNode"
    children =
    [
      { _class = "MaterialGroupList" children = [ { _class = "DefaultMaterialGroup" remaps = [ { from = "src.vmat" to = "materials/x/x.vmat" } ] } ] },
      { _class = "RenderMeshList"    children = [ { _class = "RenderMeshFile" filename = "models/x/x.fbx" import_scale = 1.0 } ] },
      { _class = "PhysicsShapeList"  children = [ { _class = "PhysicsHullFile" filename = "models/x/x_phys.fbx" surface_prop = "default" collision_prop = "default" } ] },
      { _class = "GameDataList"      children = [ { _class = "GenericGameData" game_class = "prop_data" } ] },
    ]
    model_archetype = ""            // e.g. "physics_prop_model", "animated_model"
    primary_associated_entity = ""  // e.g. "prop_physics", "prop_dynamic"
  }
}
```

- Verified: `RootNode`, `RenderMeshFile`, `PhysicsShapeList`, `PhysicsHullFile`, `PhysicsMeshFile`, `PhysicsHullFromRender`, `PhysicsShapeCapsule`, `RenderPrimitiveBox`, `DefaultMaterialGroup`, `PhysicsBodyMarkup`, `GenericGameData`; keys `surface_prop`, `collision_prop`, `model_archetype`, `primary_associated_entity`, `anim_graph_name`.
- UNVERIFIED spellings: `remaps`, `import_scale/translation/rotation`, `GameDataList`, `game_class`, `LODGroupList`/`LODGroup` (`switch_threshold`), `BodyGroupList`/`BodyGroupChoice`, `AttachmentList`, `HitboxSetList`, `JiggleBoneList`.
- Don't mix with the legacy pre-ModelDoc `CVmeshList` / `CVmesh` style.

**Physics options** \[V, s&box wiki\]

| Node | When |
| --- | --- |
| `PhysicsHullFromRender` | Quick and inaccurate; prototypes |
| `PhysicsHullFile` | The usual choice; separate low-poly hull mesh |
| `PhysicsMeshFile` | Static concave collision only — never for movable props |
| Multiple hulls | Any concave movable shape (hull per mesh / per element) |

**Workflow** \[V\]

1. Put FBX/DMX/OBJ in `content/<root>/<addon>/models/<name>/`.
2. ModelDoc → star button → Add Meshes; pick an archetype (it warns about missing `prop_data`).
3. RenderMeshFile; untick physics-only meshes. Map source materials via DefaultMaterialGroup remaps.
4. PhysicsHullFile: "Hull Per Mesh" or "Hull Per Element"; raise Face Merge Angle; "Incremental Face Reduction"; red vertex count = too many. Set Surface Property on each hull.
5. Add `GenericGameData: prop_data`. Optional mass: PhysicsBodyMarkup → Mass Override (kg).
6. Click "Needs Compiling"; test in Game Preview (Spawn, Grab, Shoot).

**From Hammer geometry**: select meshes → right-click → Selected Objects → **Create Model From Selection** → save under `content/.../models/`. Smart props: Collapse Smart Props first. \[V\]

**Scale, origin, LOD**

- Origin = source file origin; fix with import translation/rotation rather than re-exporting.
- Scale: Blender metres vs Hammer inches is the #1 cause of tiny/giant props. Give the tool a `target_size` argument (ahujasid pattern: *"Size in metres of the model's largest dimension (chair 1.0, car 4.5, cup 0.12)"*) and compute `import_scale` from measured bounds.
- LODs are not auto-generated in community pipelines; only supplied LOD meshes are used.
- Placement entity: `prop_static` (baked, cheapest), `prop_dynamic` (animated/toggleable), `prop_physics` (needs `prop_data`).

## Player models & characters

HL:Alyx has a documented character pipeline; CS2 does not. CS2 custom player models are a **community-server-only** workflow, and AnimGraph 2 (live around 2026-04-21) re-authored all third-person animation, so older community ports may break. Treat CS2 player-model tools as experimental and say so in their descriptions. \[V\] ([HLTV](https://www.hltv.org/news/44430/animgraph2-goes-live-in-latest-cs2-update))

**HL:Alyx character pipeline** \[V\] ([Custom Character Basics](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools/Custom_Character_Basics))

1. FBX into `content/hlvr_addons/<addon>/models/`.
2. ModelDoc: Add Meshes → DefaultMaterialGroup → "Add Simple Animations…".
3. Animation processing: **Extract Motion** (reference bone; TX/TY for walks, RZ for turns), AnimSmoothLoop, Looping checkbox. Without Extract Motion the pelvis drifts then snaps back.
4. AnimGraph 1 (`.vanmgrph`): set a preview model first; Final Pose is the root and unconnected nodes don't evaluate; Movement Settings speeds are inches/second. Example graph: `biped_standard.vanmgrph`.
5. Assign the graph in ModelDoc (Outliner settings → `anim_graph_name`), recompile.
6. In the map: Generic Actor with World Model + Entity Scripts (Lua); drive via I/O `SetAnimGraphParameter` with `<param>=<value>`. Test: `addon_tools_map custom_character`.

**CS2 player models** \[V-comm\] ([PlayerModelChanger](https://github.com/samyycX/CS2-PlayerModelChanger))

- No official custom-agent Workshop category. Delivery is CounterStrikeSharp `PlayerModelChanger` + `MultiAddonManager`.
- Packing needs `characters` added to `VpkDirectories` in `gameinfo.gi`, which breaks VAC/signature checks; the plugin warns it "can cause a GSLT ban".
- Path convention: `characters/models/<author>/<name>/<name>.vmdl`, plus an `_arms` variant (without it, default arms are used).
- AG2 files: `vnmskel` (skeleton + bone masks), `vnmclip` (clips + events), `vnmgraph` (graph). `vanmgrph` is AG1.
- Common approach (UNVERIFIED): build against the shipped CS2 player skeleton by basing on a shipped player `.vmdl` (base model or prefab) so it inherits the animgraph and hitboxes.

**Checklist a tool should verify for any character**

- Skeleton matches the target (bone names + count) — expose `read_model_info` returning bones.
- Hitbox set present; jiggle bones / cloth are ModelDoc nodes (class names UNVERIFIED; VRF CLI's `--skip_softbody_reconstruction` implies cloth data lives in `vmdl_c`).
- Bodygroups and material groups (skins) defined and named.
- Animgraph assigned and compiles; preview the idle and a locomotion clip with a multi-angle screenshot.
- For CS2: warn the user about the GSLT/VAC implications before editing `gameinfo.gi`.

## World design & world building

Hammer 5 is mesh-based (faces, edges, vertices), not brush-based: there are no booleans, you delete faces and bridge instead, and free-floating vertices aren't allowed. A weak agent building a map needs layout metrics in numbers, a seal check, and a mandatory look. \[V\] ([Mesh Editing](https://developer.valvesoftware.com/wiki/Source_2/Docs/Level_Design/Basic_Construction/Mesh_Editing_1))

**Editor vocabulary the agent should know** \[V\] ([Hammer Overview](https://developer.valvesoftware.com/wiki/Source_2/Docs/Level_Design/Hammer_Overview))

- Selection modes 1–6: Vertices, Edges, Faces, Meshes, Objects, Groups.
- Tools: Block (Shift+B), Clipping (Shift+X), Mirror (Shift+F), Texture Projection, Paint/vertex blend (Shift+V), Displacement (Shift+D), Entity (Shift+E), Physics Simulation (Shift+C), Path Tool, Tile Editor.
- Mesh ops: extrude (Shift+drag in component modes; **duplicates** in Meshes mode), Bridge (B, needs equal edge counts), Fill polygon (P, needs a closed loop), Cut (C), Flip normals (F), Delete removes faces vs Backspace merges.
- Texturing: Face Edit tab (Align, Scale, Shift, Rotate, Fit, Justify), Texture Lock toggles, UV Peel in Edges mode.
- Grid `[` / `]` powers of two; build is F9.

**CS2 layout metrics** (CS:GO reference; CS2 close, UNVERIFIED) \[V\] ([Mapper's Reference](https://developer.valvesoftware.com/wiki/CS:GO_Mapper%27s_Reference))

| Item | Units |
| --- | --- |
| Player hull | 32×32×72 (crouch 54), eye \~64 |
| Step height / stair step | 18 / ≤18 tall, ≥16 deep |
| Walkable slope | ≤ 45.57° |
| Jump / crouch-jump | 54 / 63–65 |
| No-peek wall | 65 walking, 47 crouched |
| Ceiling clearance | 73 walking, 55 crouched |
| Safe fall | ≤ 162 |
| Bullet-proof-feeling wall | ≥ 32 thick |
| Big crate / small crate | 64×64×72 / 48×48, ≤44 tall |
| Smoke | \~256 diameter × 128 tall (200-wide corridor fully covered) |

**Core entities** \[V\] ([EntityList](https://www.source2.wiki/EntityList))

| Entity | Key facts |
| --- | --- |
| `info_player_terrorist` / `info_player_counterterrorist` | Spawns |
| `func_buyzone` | `TeamNum` 2=T, 3=CT; one team per zone |
| `func_bomb_target` | A/B by lowest index or place name; bots use nav plant spots |
| `trigger_bomb_reset` | Blocks C4 (replaces `toolsblockbomb`) |
| `light_environment` + `env_sky` | Sun + sky IBL; "Estimate Lighting from HDR Skybox" |
| `light_omni2`, `light_barn`, `light_rect` (baked only) | `light_omni`/`light_spot` deprecated; Stationary max 4 overlapping per surface |
| `env_combined_light_probe_volume` | Probes + box cubemap; `box_mins`/`box_maxs`, `voxel_size` 12/24/48/108; cover all playable space |
| `env_cs_place`, `cs_minimap_boundary`, `cs_minimap_volume` | Callouts and radar |
| `skybox_reference` | 3D skybox: separate `<map>_skybox` vmap via `targetMapName` |
| `info_particle_system` | `effect_name`, `start_active`, `cpoint1..63`; inputs Start/Stop/DestroyImmediately |
| `point_script` | CS2 scripting; `cs_script` = `resource:vjs`; input `RunScriptInput` |
| `prop_static` / `prop_dynamic` / `prop_physics` | Baked / animated / simulated (needs `prop_data`) |

Common I/O on most entities: Kill, KillHierarchy, AddOutput, FireUser1–4, SetParent, ClearParent. CS2 has no Lua vscript, so use `point_script` for logic.

**Lighting and performance** \[V\] ([Lighting](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Level_Design/Lighting))

- Per-face Lightmap Resolution Bias (power of 2): keep low on foliage; one big face with high bias ruins density map-wide. Minor props: Baked Lighting = No (probes instead). Check with `mat_luxels 1`.
- Each mesh gets reflections from exactly one cubemap; debug with `r_cubemap_debug_colors`, `r_light_probe_volume_debug_colors`.
- Seal with `toolsskybox`; nodraw unseen faces; mark props Vis Occluder only when they truly block view; `visibility_hint` in open areas; `info_cull_triangles` for out-of-bounds geometry. Steam Audio dominates big-map build time.
- Nav: Full and Fast presets include `-nav`; block with `toolsnavclip`; test with `bot_add_t`, `bot_goto_mark`, `bot_show_battlefront`.

**Other systems**: Smart props (`.vsmart`: Model, Group, PickOne, FitOnLine, PlaceOnPath elements + variables/expressions), prefabs (`.vmap` with a prefab flag), Tile Editor, Path Tool (prop splines, ropes). CS:GO map import: `game\csgo\import_scripts\import_map_community.py` (Python 2.7.14, `csgo_legacy` beta).

**Suggested build order** (adapted from hammer-mcp's skill): blockout shell with nodraw → seal with skybox → openings/routes → spawns + objectives → metric check (clearances, jumps, sightlines) → Fast Compile + walk-through look → detail + materials → lights + probe volumes → Full Compile → multi-angle look + verdict.

## Particles

A particle system is a `.vpcf` (KV3, `_class = "CParticleSystemDefinition"`) made of operator arrays; the authoritative field list per class is the [s2v.app schema explorer](https://s2v.app/SchemaExplorer/cs2/particles/CParticleSystemDefinition), which is exactly what a `read_particle_schema` tool should wrap (Houdini's `get_parameter_schema` pattern). Remember the CS2 Particle Editor is hidden by default (see fundamentals). \[V\] ([CS2 Particles](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Particles))

**Top-level structure** \[V\]

- Arrays: `m_PreEmissionOperators`, `m_Emitters`, `m_Initializers`, `m_Operators`, `m_ForceGenerators`, `m_Constraints`, `m_Renderers`, `m_Children`.
- Scalars: `m_nMaxParticles`, `m_nInitialParticles`, `m_ConstantColor`, `m_flConstantRadius`, `m_flConstantLifespan`, `m_BoundingBoxMin/Max`, `m_flMaxDrawDistance`, `m_nBehaviorVersion`, `m_controlPointConfigurations`, `m_hFallback`.

**Minimum viable system** (one entry from each group)

| Group | Common classes | Key fields |
| --- | --- | --- |
| Emitter | `C_OP_ContinuousEmitter`, `C_OP_InstantaneousEmitter`, `C_OP_MaintainEmitter` | `m_flEmitRate`, `m_flEmissionDuration`, `m_flStartTime` |
| Initializer | `C_INIT_RandomLifeTime`, `C_INIT_CreateWithinSphereTransform`, `C_INIT_RandomRadius`*, `C_INIT_RandomColor`* | `m_fLifetimeMin/Max`; `m_fRadiusMin/Max`, `m_fSpeedMin/Max` |
| Operator | `C_OP_BasicMovement`, `C_OP_Decay`*, `C_OP_FadeAndKill`*, `C_OP_InterpolateRadius`\* | `m_Gravity`, `m_fDrag` |
| Renderer | `C_OP_RenderSprites`, `C_OP_RenderRopes`, `C_OP_RenderAsModels`, `CBaseTrailRenderer` family | `m_vecTexturesInput` (≥1 `TextureGroup_t`) |

\* common but not verified by fetch. There are 101 initializers and 34 renderers in the CS2 schema.

**Traps**

- Float/vector fields are typed input objects (`CParticleCollectionFloatInput`, `CPerParticleFloatInput`), not bare numbers. Text shape like `{ m_nType = "PF_TYPE_LITERAL" m_flLiteralValue = 10.0 }` is UNVERIFIED — copy from an existing vpcf.
- When two functions in a group do the same thing, **the last one wins**.
- Control-point preview values and the Controls panel are editor-only; they don't ship.
- A system without `C_OP_Decay`-style lifetime handling and a sane `m_nMaxParticles` can run forever or flood memory — have a validator flag it.
- Children: `m_Children` entries reference `resource:"particles/…vpcf"` with delay and end-cap flags (`ParticleChildrenInfo_t`; field names UNVERIFIED).
- Placing in a map: `info_particle_system` with `effect_name`; fire Stop before Start to restart.

**Vision tip**: particles must be verified over time. Use a frame strip (ahujasid `look(mode="frames", frame_count=6)` pattern) via Source 2 Viewer's MCP or the in-editor preview, not a single still.

## Vision support

Agents don't look unless the workflow forces them to: hammer-mcp logged its render tool being called **once in 145 tool calls**, and the map that passed every numeric check had a door painted flat on a wall. Make looking cheap (one call, small image), multi-angle by default, and part of the written definition of done.

**Rules to bake in**

1. **Look before and after every visual change** (ahujasid classic prompt: *"Use get\_viewport\_screenshot() BEFORE making changes … AFTER executing code or importing assets to verify the result"*).
2. **One call, whole picture.** Offer a contact-sheet tour (hammer `render_vmf_tour`) and multi-angle presets (`angles` = front, right, top, three-quarter; Unity `surround`).
3. **Small by default.** `max_size=768`; *"Images stay in the conversation, so a smaller max\_size keeps long sessions cheap."*
4. **Restore state.** *"Every setting changed to take the picture is restored afterwards."* (camera, render mode, selection).
5. **Stable colour coding.** hammer colours each face by a hash of its material, so the same wall is the same colour across edits — great for diffing.
6. **Text "screenshot" twin** for non-vision models (Blender Lab `get_screenshot_of_window_as_json`): window layout, active tool, selection, viewport camera.
7. **Describe in writing before concluding.** hammer: *"Done is three greens and a look … described in writing before you conclude."* Forcing a written description makes weak models actually inspect the image.
8. **Ask a specific question of the image.** Pair each look with a checklist ("Are all faces textured? Any pink/black checker? Is the prop resting on the floor? Any light leaks?").

**Source 2 capture options**

| Need | Source |
| --- | --- |
| Live Hammer / ModelDoc / Material Editor viewport | Your in-editor bridge or OS window capture of the tool window |
| Compiled asset preview (model, particle, map) | Source 2 Viewer MCP (`127.0.0.1:13338/mcp`): open file, move camera, screenshot |
| In-game view | VConsole screenshot command (exact CS2 name UNVERIFIED) + `setpos`/`setang` (cheats) |
| Lighting diagnostics | `mat_luxels 1`, `r_cubemap_debug_colors`, `r_light_probe_volume_debug_colors`, Hammer F5 fullbright / F6 lit |
| Vis diagnostics | `vis_debug_show`, Hammer vis contributors view |
| Top-down plan | Orthographic top view, or render your own plan from parsed `.vmap` geometry (hammer `render_vmf_plan` style) |

**Visual failure modes to name in the checklist**: pink/black checker (missing material), stretched UVs, z-fighting coplanar faces, floating or buried props (check bounds vs floor), wrong scale (compare to a 72-unit player), light leaks at mesh seams, flat-lit areas (no probe volume), particles invisible at frame 0 (need a frame strip).

## Boilerplate tool signatures

About 60 tools in ten families, written in your `name(arg: type = default): description` form. Names mirror Blender/Unreal conventions where models already have priors (`get_scene_info`, `look`, `import_model(target_size_m)`). Descriptions follow the patterns above: what it's for, what it's NOT for, units, and the next step. Keep or cut to match what your server already has.

**Shared conventions** (apply to every tool)

```
# Every mutating tool also takes:
#   dry_run: bool = false   — "Check and report without writing. Every validation runs either way."
#   confirm: bool = false   — "Required (true) to run: this tool writes, deletes or executes."
#   backup:  bool = true    — "Copy the file to <file>.bak before writing. Nothing is copied on a no-op."
# Every tool returns: { ok, data, warnings[], next_step, verdict? }
#   errors: { ok:false, error, valid_choices?[], did_you_mean?[], use_instead?, retry_safe:bool, retry_after_ms? }
# Paths are addon-relative ("materials/wall.vmat"), lowercase. Distances in Hammer units (1u ≈ 1 inch; 1 m ≈ 39.37u), Z up.
# Vectors accept [x,y,z], {x,y,z} or "x y z". Booleans accept true/"true".
```

**1. Session & health**

```
health(): Toolchain status: game install, active addon, editor bridge, resourcecompiler, dmxconvert, VConsole link, Source 2 Viewer link, tool count. Start here once per session, and again whenever a tool reports something missing. Never fails; missing pieces are listed with how to fix them.
get_editor_state(): Whether the tools are ready for a call: open editors, active document, unsaved changes, build/compile in progress, game running. Returns ready_for_tools, blocking_reasons[], retry_after_ms. Call before a long sequence of writes and after any "busy" error.
list_addons(game: "cs2"|"hlvr"|"dota2" = configured): Addons on this machine with their content/ and game/ roots and whether each is the mounted one. Tools mode mounts exactly one addon; assets from others render missing.
search_docs(query: str, topic?: "saving"|"materials"|"models"|"characters"|"world"|"particles"|"entities"|"console", limit: int = 5): Search the bundled Source 2 reference notes (this document). Use before guessing a key name, flag or classname.
```

**2. Lookups (never invent a name)**

```
find_assets(query: str, kind: "material"|"model"|"particle"|"texture"|"sound"|"smartprop"|"prefab"|"any" = "any", scope: "addon"|"game"|"all" = "all", limit: int = 50, previews: int = 0): Search addon and shipped content by name. This is the Asset Browser, which an agent otherwise doesn't have. Returns paths in exactly the form other tools accept. previews attaches up to 6 thumbnails — cheaper than placing the wrong asset.
read_asset_info(path: str): Type, source file, compiled _c path and mtime, whether the compile is stale, and what the asset references and is referenced by. Use to answer "is this compiled and current?" NOT for model internals — use read_model_info.
read_shader_params(shader: str, filter?: str): Feature flags (F_*), parameters (g_*) and texture inputs (Texture*) a shader accepts, with types, defaults and ranges, read from a shipped .vmat of that shader. Call before create_material or edit_material. Unknown shader returns the list of installed ones.
read_entity_class(classname?: str, prefix?: str, limit: int = 100): What the game's FGD declares for an entity class: keyvalues with types and defaults, inputs, outputs. Called with no classname it lists classes. Answers "does this class accept that key" without guessing from the wiki.
read_surface_props(filter?: str): Valid physics surface property names. Surface props drive impact sounds, decals and auto-mass.
```

**3. Saving & compiling**

```
save_document(editor: "hammer"|"modeldoc"|"material"|"particle"|"active" = "active", path?: str): Save the open document. Asset editors compile on save; Hammer does not — use build_map. Save before every build: edits are not always undoable across a compile.
compile_asset(path: str | str[], force: "none"|"shallow"|"deep" = "shallow", wait_seconds: int = 45, job?: str): Compile one or more content/ assets with resourcecompiler and return translated findings, not raw output. Verifies the _c file was actually rewritten. If not done in wait_seconds, returns a job handle; call again with job= to keep waiting. NOT for maps — use build_map.
build_map(map: str, preset: "fast"|"entities_only"|"full"|"final" = "fast", lightmap_res?: 256|512|1024|2048|4096|8192, run_after: bool = false, wait_seconds: int = 45, job?: str, confirm): Build a .vmap to maps/<name>.vpk. fast = world+physics+nav (layout iteration); entities_only keeps lightmaps; full/final bake lighting and need an RT-capable GPU. Returns per-stage findings. Run validate_map first — it catches most of what makes a long build fail.
read_build_command(map: str, preset: str): The exact resourcecompiler command line Hammer would run for these settings (Build → Command Line tab). Use instead of guessing flags.
read_compile_log(text?: str, job?: str): Turn compiler output into findings, each with what the message means and the tool to run next (e.g. "scene_meshes.dmx" → triangle budget; "extremely large in lightmap" → lower Lightmap Resolution Bias).
clean_stale_outputs(dry_run: bool = true, confirm): List (and with confirm, delete) _c files in game/ whose source no longer exists in content/, plus .los vis-debug files. Stale outputs keep loading and get uploaded.
validate_publish(): Check what the Workshop upload will contain: maps listed in addoninfo.txt, folders outside VpkDirectories that will be silently dropped, stale outputs, files over limits, uppercase or illegal filenames.
```

**4. Materials & shaders**

```
create_material(path: str, shader: str = "csgo_environment.vfx", textures: {color?, normal?, rough?, metal?, ao?, trans?, mask?, selfillum?}, params?: {name: value}, features?: str[], surface_prop?: str, dry_run, confirm): Create a .vmat from source images and compile it. Texture paths are addon-relative; images should follow the _color/_normal/_rough suffix convention and be power-of-two. Refuses unknown params with did-you-mean from read_shader_params. Use csgo_complex.vfx for props, csgo_environment.vfx for world. next_step: look at it on a preview sphere or apply_material.
read_material(path: str): Shader, flags, params and textures of a .vmat, plus whether every texture exists and the compile is current.
edit_material(path: str, set?: {name: value}, unset?: str[], dry_run, confirm): Change parameters on an existing .vmat and recompile. Changing shader is NOT supported here — create a new material.
apply_material(material: str, target: Selector, dry_run, confirm): Put a material on faces or whole meshes in the open map. Selector = { ids?, name?, material?, within?{mins,maxs}, facing?: "up"|"down"|"side"|"any", min_area? }; an empty selector is refused. Does NOT check the material exists — use find_assets first.
align_texture(target: Selector, mode: "world"|"face"|"fit"|"justify" = "world", scale?: [u,v], shift?: [u,v], rotate?: float, dry_run, confirm): Realign UVs on selected faces. Whether it looks right is for the eye: look afterwards.
apply_tool_material(kind: "nodraw"|"skybox"|"clip"|"playerclip"|"npcclip"|"grenadeclip"|"trigger"|"blocklight"|"navclip"|"blockbullets"|"ladder", target: Selector, confirm): Apply a tools/ material by intent rather than path. skybox seals the map and blocks vis; nodraw collides and blocks vis but doesn't render.
```

**5. Props & models (ModelDoc)**

```
import_model(source: str | str[], out_path: str, target_size_m?: float, up_axis: "z"|"y" = "z", physics: "none"|"hull_from_render"|"hull_file"|"mesh_file" = "hull_from_render", physics_source?: str, surface_prop: str = "default", entity: "prop_static"|"prop_dynamic"|"prop_physics" = "prop_static", material_remap?: {from: to}, dry_run, confirm): One call that does a whole prop correctly: copies FBX/OBJ/DMX into content/models/, writes the .vmdl with render mesh, material remaps, physics, and prop_data when entity is prop_physics, then compiles. target_size_m is the largest dimension in metres (chair 1.0, crate 0.9, car 4.5, cup 0.12); import_scale is computed from measured bounds. mesh_file is static-only and refused for prop_physics. next_step: look(mode="angles") and check bounds sit on z=0.
read_model_info(path: str): Bounds (units and metres), origin offset, bones, materials and remaps, physics shapes with surface props, LODs, bodygroups, material groups, attachments, hitboxes, sequences, animgraph, prop_data present. What Hammer's model viewer shows before you place a prop.
edit_model(path: str, ops: Op[], dry_run, confirm): Batch-edit a .vmdl and recompile once. Op = { op: "set_material_remap"|"add_lod"|"add_bodygroup"|"add_material_group"|"add_attachment"|"add_physics_hull"|"set_surface_prop"|"set_mass_kg"|"set_prop_data"|"set_import_transform", ... }. Unknown op returns the list. The whole batch is refused if the compile fails; nothing is half-applied.
create_model_from_selection(target: Selector, out_path: str, confirm): Hammer's Selected Objects → Create Model From Selection. Turns map meshes into a reusable .vmdl. Collapse smart props first.
```

**6. Player models & characters** (experimental in CS2 — say so)

```
read_skeleton(path: str): Bone hierarchy with names, parents and rest transforms of a model or source file.
compare_skeletons(a: str, b: str): Bones missing, extra, renamed (fuzzy) or re-parented between two skeletons. Run before building a character against a game skeleton; a mismatch is the usual reason animations break.
create_character_model(source: str, out_path: str, base_model: str, arms_model?: str, hitboxes: "inherit"|"generate" = "inherit", confirm): EXPERIMENTAL. Build a character .vmdl on top of a shipped player model so it inherits skeleton, animgraph and hitboxes. CS2 custom player models are community-server only and AnimGraph 2 (2026) may break older approaches. next_step: look(mode="frames") on idle and run.
set_anim_graph(model: str, graph: str, confirm): Assign a .vanmgrph (AG1) or .vnmgraph (AG2) to a model and recompile.
add_animations(model: str, files: str[], extract_motion?: "none"|"xy"|"xy_rz" = "none", loop: bool = true, confirm): Add clips to a model. Locomotion clips need extract_motion="xy" or the pelvis drifts then snaps back.
```

**7. World building (Hammer)**

```
get_scene_info(query?: str, fields?: ("placement"|"contents"|"health"|"settings")[], root?: str, limit: int = 20): Facts about the open map as text: one header line (map, counts, selection, grid, unsaved), then one line per object. No image — to see the map, use look. Inspect progressively; don't dump big maps. Unknown field returns "Pick from: …".
get_object_info(id: str): Everything about one object: class, transform, keyvalues, I/O, mesh stats, materials per face group, visgroup.
create_block(mins: Vec3, maxs: Vec3, material: str = "materials/tools/toolsnodraw.vmat", name?: str, dry_run, confirm): One box mesh. For a room use build_room.
build_room(mins: Vec3, maxs: Vec3, wall_thickness: int = 16, materials: {floor, walls, ceiling}, openings?: [{wall: "n"|"s"|"e"|"w", center: float, width: int = 64, height: int = 112, sill: int = 0}], ceiling: bool = true, dry_run, confirm): Compound: builds floor, walls and ceiling as separate meshes with openings cut, outward faces nodraw. Default door 64×112 clears a standing player (72). You own the envelope; this owns the construction.
edit_mesh(target: Selector, ops: Op[], dry_run, confirm): Batched mesh ops. Op = extrude{distance} | bevel{width, segments} | bridge{other} | cut{plane} | clip{plane, keep} | hollow{thickness} | flip_normals | merge_vertices{tolerance} | delete_faces. bridge needs equal edge counts; refuses ops that leave free-floating vertices.
transform_objects(target: Selector, move?: Vec3, rotate?: {axis, degrees, pivot?}, scale?: Vec3, snap_to_grid?: int, texture_lock: bool = true, dry_run, confirm): Move, turn or scale. Reports bounds before and after.
delete_objects(target: Selector, confirm): Refuses an empty selector rather than emptying the map.
place_entity(classname: str, origin: Vec3, angles?: Vec3, keyvalues?: {k: v}, targetname?: str, dry_run, confirm): Place a point entity. Keyvalues are checked against read_entity_class; unknown keys fail with did-you-mean.
place_prop(model: str, origin: Vec3, angles?: Vec3, kind: "static"|"dynamic"|"physics" = "static", snap_to_floor: bool = true, confirm): Place a model. snap_to_floor traces down and rests the model's bounds on the surface. physics requires prop_data in the model.
edit_entity(id: str, set?: {k: v}, unset?: str[], confirm): Change keyvalues on an entity.
connect_io(source: str, output: str, target: str, input: str, param?: str, delay: float = 0, times: int = -1, confirm): Add an entity I/O connection. Output and input names are checked against both classes' FGD entries.
place_light(kind: "environment"|"omni"|"barn"|"rect", origin: Vec3, color: [r,g,b] = [255,255,255], brightness: float = 1, mode: "baked"|"stationary"|"per_pixel" = "baked", angles?: Vec3, confirm): Uses light_omni2/light_barn (light_omni/light_spot are deprecated). Warns when more than 4 stationary lights overlap a surface.
cover_with_probe_volumes(playable_area?: {mins, maxs}, voxel_size: 12|24|48|108 = 24, confirm): Fill playable space with env_combined_light_probe_volume boxes, origin at eye height. Without them dynamic objects light flat.
measure_clearance(at: Vec3, hull: "standing"|"crouching" = "standing"): How much room there is at a point in each direction, naming the object that bounds it. Standing hull 32×32×72, crouch 54.
trace(from: Vec3, to: Vec3, hull: "ray"|"standing"|"crouching" = "ray"): First hit along a line or swept player box. Use hull for "can a player get through".
validate_map(map?: str, rules: "cs2_competitive"|"cs2_wingman"|"hlvr"|"none" = "cs2_competitive"): Pre-build lint: sealed with toolsskybox, spawns per team, buyzones, bomb targets, free vertices, missing materials, oversized lightmap bias, props without prop_data, unreachable spawns. Returns pass/warn/fail/skipped per rule — read overall, not the count.
```

**8. Particles**

```
read_particle_schema(class?: str, group?: "emitter"|"initializer"|"operator"|"renderer"|"force"|"constraint", filter?: str): Classes in a group, or one class's fields with types and defaults (from the game's schema). Call before edit_particle; field values are typed input objects, not bare numbers.
create_particle(path: str, template: "blank"|"smoke"|"sparks"|"dust"|"fire"|"embers"|"rain" = "blank", overrides?: {color?, rate?, lifetime_s?, radius?, gravity?: Vec3, sprite?: str, max_particles?: int}, dry_run, confirm): Write a working .vpcf from a known-good template (emitter + lifetime + movement + sprite renderer) and compile it. Enable the Particle Editor in CS2 first (health reports this).
edit_particle(path: str, ops: Op[], dry_run, confirm): Op = add_function{group, class, fields} | remove_function{group, index} | set_field{group, index, field, value} | add_child{ref, delay}. When two functions in a group do the same thing, the last wins — the result warns about duplicates.
validate_particle(path: str): Checks max_particles, a lifetime/decay path, at least one renderer with a texture, bounding box, control points used vs configured.
```

**9. Vision**

```
look(target?: str | Selector, mode: "viewport"|"angles"|"tour"|"plan"|"frames"|"player_eye" = "viewport", shading: "lit"|"fullbright"|"wireframe"|"lightmap_density"|"tool_materials" = "lit", frame_count: int = 6, max_size: int = 768): See the work as one image. angles = front/right/top/three-quarter grid; tour = contact sheet of eye-height views per room; plan = dimensioned top-down; frames = strip over time (particles, animation); player_eye = 64u eye height at target. Every setting changed to take the picture is restored. For counts and positions use get_scene_info. Describe what you see in writing before concluding.
describe_view(): Text twin of look for models without vision: active editor, camera, visible objects with screen positions, selection.
focus(target: str | Selector): Frame the viewport on a target (like Hammer's Z / F).
```

**10. Verdicts & escape hatches**

```
report_ready(scope: "asset"|"map"|"addon", path?: str, profile?: str): The stopping condition. Every other reader answers "how much"; this answers "is it done": compiled and current, no missing references, lint clean, a look taken since the last change, publish contents valid. pass/warn/fail/skipped per criterion; never pass when nothing was checked.
run_console_command(command: str, wait_ms: int = 500, confirm): LAST RESORT. Send a command over VConsole (needs -tools or -vconsole; the port takes one client). Prefer dedicated tools — they validate input and report structured errors. Returns console output captured during wait_ms.
edit_kv3(path: str, ops: [{op: "set"|"delete"|"append", path: str, value?: any}], dry_run, confirm): LAST RESORT for .vmdl/.vpcf/.vsmart/.vsndevts fields no dedicated tool covers. Preserves the header, validates syntax, compiles, and rolls back if the compile fails. NOT for .vmap (binary DMX).
```

**MCP prompts** (workflow recipes the client can pull): `blockout_cs2_map`, `import_prop`, `make_material`, `make_particle`, `character_model`, `publish_addon` — each is one of the playbooks below.

## Agent playbooks

Each playbook is meant to ship as an MCP prompt (and be mirrored in server `instructions` as a one-line pointer). Every one ends in the same definition of done: **compile clean, `report_ready` pass, and a look described in writing.**

**Server `instructions` draft** (the only text some clients read before deciding to load tools)

```
Source 2 Workshop Tools (CS2 / HL:Alyx) bridge. Use it to build maps in Hammer, make props in ModelDoc,
author materials and particles, compile, and look at the result.
Start with health, then get_editor_state. Never invent asset paths, shader params or entity classes:
use find_assets, read_shader_params, read_entity_class first.
Traps: edit content/, never game/. .vmap is binary DMX - use the map tools, not edit_kv3.
Units are Hammer units (~1 inch, Z up); a player is 72 tall. Tools mode mounts one addon.
Question -> answer, change nothing. Inspection -> read only. Modification -> observe, change, verify.
Done = compile clean + report_ready pass + look, described in writing.
run_console_command and edit_kv3 are last resorts.
```

**Playbook: `import_prop`**

1. `health` → `get_editor_state`.
2. `find_assets(query, kind="model")` — reuse a shipped model if one fits.
3. `import_model(source, out_path, target_size_m, entity, surface_prop)`; pick `physics="hull_file"` if a low-poly hull exists.
4. `read_model_info` — check bounds in metres match intent, origin at base, prop\_data present for physics.
5. Materials: `find_assets(kind="material")` or `create_material`, then `edit_model(set_material_remap)`.
6. `look(target, mode="angles")` — write what you see: scale vs a player, textures present, no pink/black.
7. `report_ready(scope="asset", path)`.

**Playbook: `make_material`**

1. `read_shader_params(shader)` (environment for world, complex for props).
2. Confirm texture files exist and follow suffixes; convert to power-of-two if needed.
3. `create_material(…, dry_run=true)` → fix any did-you-mean → `create_material(…, confirm=true)`.
4. `apply_material` to a test face → `look(shading="lit")` → adjust `edit_material` / `align_texture`.
5. `report_ready(scope="asset")`.

**Playbook: `blockout_cs2_map`**

1. `health`, `list_addons`, `get_scene_info` (new or existing map?).
2. Write the layout plan in text first: lanes, sites, spawns, approximate sizes in units (use the metrics table).
3. `build_room` per area with nodraw/dev materials; `edit_mesh` for connectors; doors ≥ 64×112.
4. Seal: `apply_tool_material(kind="skybox")` on the outer shell.
5. `place_entity` spawns (≥5 per team), `func_buyzone` per team, `func_bomb_target` A and B.
6. `measure_clearance` / `trace` on chokepoints, jumps (≤54 high), and crouch spots.
7. `validate_map(rules="cs2_competitive")` until overall is pass.
8. `build_map(preset="fast")` → `read_compile_log`.
9. `look(mode="tour")` and `look(mode="plan")`; describe every room in writing; fix and repeat 6–9.
10. Only then: real materials, lights (`place_light` environment + `env_sky`), `cover_with_probe_volumes`, `build_map(preset="full")`, final look, `report_ready(scope="map")`.

**Playbook: `make_particle`**

1. `health` (CS2: Particle Editor must be enabled).
2. `create_particle(template=nearest)` with overrides.
3. `read_particle_schema` before any `edit_particle` field.
4. `validate_particle` → `look(mode="frames", frame_count=6)`; describe motion over time.
5. Place with `place_entity("info_particle_system", effect_name=path, start_active=1)`; `look` in context.

**Playbook: `character_model`** (experimental)

1. Tell the user CS2 custom player models are community-server only and touch `gameinfo.gi`.
2. `compare_skeletons(source, base_model)`; stop and report if bones don't map.
3. `create_character_model(…, base_model, hitboxes="inherit")`.
4. `look(mode="frames")` on idle and run; check arms variant.
5. `report_ready(scope="asset")`.

**Playbook: `publish_addon`**

1. `report_ready(scope="addon")`.
2. `clean_stale_outputs(dry_run=true)` → confirm.
3. `validate_publish` — fix dropped folders, missing `maps` key, illegal names.
4. Hand off to the user for the Workshop Manager upload (preview image, visibility Hidden first).

## Open gaps & sources

Update: the Valve wiki crawl in VDC deep dive resolves several of these and corrects two claims here (`-lightmapCPU` is gone; the `.vmap` header version varies, 35 to 40).

These couldn't be verified from public pages; have the server read them from real files on the install (Hammer's Build → Command Line tab, Source2Viewer decompiles, FGDs under `game/*/tools` and `game/core`):

- [ ] CS2 `.vmat` text form (`Layer0` KV1 vs KV3) and exact CS2 shader parameter names
- [ ] ModelDoc and vpcf KV3 format GUIDs
- [ ] LOD, bodygroup, hitbox, attachment, jiggle-bone node class/key names
- [ ] Steam Audio and nav compile flags; `-lightmapVRadQuality` value mapping
- [ ] AnimGraph 2 authoring and the CS2 custom player-model pipeline
- [ ] Hammer's 144 `tools/hammer` console commands (names not visible in the dump)

**MCP servers**

- [hammer-mcp](https://github.com/Sycatle/hammer-mcp) · [README](https://raw.githubusercontent.com/Sycatle/hammer-mcp/main/README.md) · [source-map skill](https://raw.githubusercontent.com/Sycatle/hammer-mcp/main/.claude/skills/source-map/SKILL.md) · [@projectsociety/mcp-core](https://www.npmjs.com/package/@projectsociety/mcp-core)
- [Blender Lab MCP](https://www.blender.org/lab/mcp-server/) · [repo](https://projects.blender.org/lab/blender_mcp)
- [ahujasid/blender-mcp](https://github.com/ahujasid/blender-mcp) · [current server.py](https://raw.githubusercontent.com/ahujasid/blender-mcp/main/src/blender_mcp/server.py) · [classic 1.5.5](https://pypi.org/project/blender-mcp/1.5.5/)
- [Epic Unreal MCP docs](https://dev.epicgames.com/documentation/unreal-engine/unreal-mcp-in-unreal-editor?lang=en-US) · [Epic skills plugin](https://github.com/EpicGames/unreal-engine-skills-for-claude-code-plugin)
- [chongdashu/unreal-mcp](https://github.com/chongdashu/unreal-mcp) · [flopperam/unreal-engine-mcp](https://github.com/flopperam/unreal-engine-mcp)
- [CoplayDev/unity-mcp](https://github.com/CoplayDev/unity-mcp) · [Coding-Solo/godot-mcp](https://github.com/Coding-Solo/godot-mcp) · [capoomgit/houdini-mcp](https://github.com/capoomgit/houdini-mcp) · [PatrickPalmer/MayaMCP](https://github.com/PatrickPalmer/MayaMCP)
- Source 2: [Source 2 Viewer MCP](https://s2v.app/ValveResourceFormat/guides/mcp-server.html) · [VRF CLI](https://s2v.app/ValveResourceFormat/guides/command-line.html) · [Hammer5Tools](https://github.com/dertwist/Hammer5Tools) · [dota2-mcp](https://github.com/Demon673/dota2-mcp) · [cs2-hammer-mcp](https://github.com/roemer2201/cs2-hammer-mcp) · [CS2RemoteConsole](https://github.com/theokyr/CS2RemoteConsole)

**Source 2 references**

- source2.wiki: [content vs game](https://www.source2.wiki/Basics/working-on-content/content-and-game) · [addons](https://www.source2.wiki/Basics/working-on-content/addons) · [mounting](https://www.source2.wiki/Basics/content-mounting) · [ResourceCompiler](https://www.source2.wiki/EngineTools/ResourceCompiler) · [asset types](https://www.source2.wiki/FileFormats/asset-types) · [KV3](https://www.source2.wiki/FileFormats/kv3) · [DMX](https://www.source2.wiki/FileFormats/dmx) · [visibility](https://www.source2.wiki/EngineTools/HammerEditor/visibility) · [tool textures](https://www.source2.wiki/ToolTextures) · [entity list](https://www.source2.wiki/EntityList) · [smart props](https://www.source2.wiki/EngineTools/HammerEditor/SmartProps) · [particle guide](https://www.source2.wiki/EngineTools/ParticleEditor/particle-editor-guide)
- Valve Developer Community: [CS2 Compiling](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Level_Design/Compiling) · [CS2 Lighting](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Level_Design/Lighting) · [Material Creation](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Materials/Material_Creation) · [Csgo Complex](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Materials/Material_Creation/Csgo_Complex) · [CS2 Particles](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Particles) · [CS2 Scripting API](https://developer.valvesoftware.com/wiki/Counter-Strike_2_Workshop_Tools/Scripting_API) · [Hammer Overview](https://developer.valvesoftware.com/wiki/Source_2/Docs/Level_Design/Hammer_Overview) · [Mesh Editing](https://developer.valvesoftware.com/wiki/Source_2/Docs/Level_Design/Basic_Construction/Mesh_Editing_1) · [Mesh Texturing](https://developer.valvesoftware.com/wiki/Source_2/Docs/Level_Design/Basic_Construction/Mesh_Texturing) · [KeyValues3](https://developer.valvesoftware.com/wiki/KeyValues3) · [VTEX](<https://developer.valvesoftware.com/wiki/VTEX_(Source_2)>) · [HLA Physics Prop](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools/Modeling/Physics_Prop) · [HLA Custom Character Basics](https://developer.valvesoftware.com/wiki/Half-Life:_Alyx_Workshop_Tools/Custom_Character_Basics) · [env\_combined\_light\_probe\_volume](https://developer.valvesoftware.com/wiki/Env_combined_light_probe_volume) · [CS:GO Mapper's Reference](https://developer.valvesoftware.com/wiki/CS:GO_Mapper%27s_Reference)
- Schema: [s2v.app Schema Explorer (CS2 particles)](https://s2v.app/SchemaExplorer/cs2/particles/CParticleSystemDefinition) · [CS2 convars](https://s2v.app/SchemaExplorer/cs2/convars)
- Community: [CS2 map compile batch](https://gist.github.com/NotHosomi/9bcd840b269d08254f644801d6597bfb) · [vmat template](https://github.com/MarkMeltzer/bs2source2/blob/master/vmat_template_complex.txt) · [CS2-PlayerModelChanger](https://github.com/samyycX/CS2-PlayerModelChanger) · [AG2 live (HLTV)](https://www.hltv.org/news/44430/animgraph2-goes-live-in-latest-cs2-update) · [s&box materials guide](https://wiki.facepunch.com/sbox/guide_to_materials)
