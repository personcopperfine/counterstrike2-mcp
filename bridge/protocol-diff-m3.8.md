# Protocol changes in m3.8 — lane RENDER (`view.render`, `world.render`, `pet.render`,
`modeldoc.render`)

Bridge build `m3.8` (Rust bridge) = m3.7 + this group.

**Four rows moved from "not ported" to served.** The Rust bridge now answers the four
render-to-PNG methods byte-for-byte like the C bridge at m2.20, including their different result
buffers and overflow arms. Nothing else changed: the method table is unchanged, no new error code,
no changed field.

## The wire, field by field

| Where | m2.20 (C bridge) | this build |
|---|---|---|
| `view.render` result | `{"path","file","width","height","bytes","camera":{...},"viewport":{...},"view":{...},"toolMaterials","format","srgb","stats","ms"}` (`mcp_bridge.c:2819-2855`) in a **4096-byte** buffer with **no** `ovf` arm (`:6065-6076`: a truncated object is appended) | identical bytes, same 4096-byte builder, same absent overflow arm |
| `view.render` arm | `active_doc` first: a missing document is `-32003 NoActiveDocument` with `{"reason":<reason>}` in a 192-byte scratch (`:6066-6071`) | identical, same 192-byte scratch, before any parameter is read |
| `view.render` errors | `-32019 NoViewport` `no_view_manager`/`bad_view_manager`/`no_3d_view`/`bad_view` (`:2672-2678`), then `-32602 InvalidParams` (width/height, format, origin, angles, fov, znear/zfar, name — in the C's order `:2683-2730`), then `-32020 RenderFailed` `out of memory`/`readback did not run (no pixels written)`/`png encode: out of memory`/`could not write Z:\app\renders` | identical strings, codes and order |
| `world.render` result | `{"path","file","width","height","bytes","camera":{...},"pipeline":"Tools->CSGO","srgb","stats","ms"}` (`:3081-3104`) in a **4096-byte** buffer with **no** overflow arm (`:6051-6061`) | identical bytes, same builder, same absent overflow arm |
| `world.render` errors | `-32020 RenderFailed` (`engine interface identity check failed`, `engine interface not found (Connect factory)`, `scene world identity check failed`, `Tools rendering pipeline not found`, `AddView failed`, `scratch target allocation failed`, `colour target has no texture`, `ReadTexturePixels failed`, `readback did not run (no pixels written)`, `png encode: out of memory`, `could not write Z:\app\renders`), `-32021 NoGameWorld`, `-32602` (origin/angles required, bounds, fov, znear/zfar, name — `:2932-2947`) | identical, in the C's order (engine walk before the parameters) |
| `pet.render` result | `{"path","file","width","height","bytes","format":"RGBA8888","hasDocument","stats","durationMs"}` (`mcp_bridge_pet_render.inc:149-165`) in the pet arm's `MAX_LINE - 2048` buffer, overflow `-32603 "InternalError: pet result too large"` (`mcp_bridge.c:6222-6231`) | identical bytes and buffer size, same overflow answer |
| `pet.render` errors | `-32602` width/height (`16..4096`, `<= 8388608` px) and name first (`inc:57-72`), then `-32019 NoViewport` with `pet_unavailable`/`no_main_window`/`bad_main_window`/`no_preview`/`bad_preview`/`no_scene_widget`/`bad_scene_widget`/`no_scene`/`bad_scene`/`no_camera`/`bad_camera` (`inc:172-201`), then `-32020` (`out of memory`/`readback did not run (no pixels written)`/`png encode: out of memory`/`could not write Z:\app\renders`) | identical; the camera mismatch also logs `pet.render: camera vtable rva <n>` at the same points |
| `modeldoc.render` result | `pet.render`'s exact shape (`inc:78-167`), in the modeldoc arm's 256 KiB buffer, overflow `-32603 "InternalError: modeldoc result too large"` (`mcp_bridge.c:6205-6219`) | identical (same shared tail) |
| `modeldoc.render` errors | `-32602` first, then `-32019 NoViewport` `modeldoc_mismatch`/`modeldoc_verifying`/`modeldoc_not_loaded`/`modeldoc_unavailable`/`no_session`/`bad_session`/`no_main_window`/`bad_main_window`/`no_scene_widget`/`bad_scene_widget`/`no_tool_widget`/`bad_tool_widget`/`no_scene`/`bad_scene`/`no_camera`/`bad_camera` (`modeldoc_render.inc:54-97`), then the shared `-32020`s | identical, including `md_render_bad_hop`'s two log lines per bad hop |
| `hasDocument` (pet/modeldoc) | `active_pet(&reason) ? 1 : 0` (`inc:216`, `:112`) | the same document chain (`pet::game::platform::active_session` / `modeldoc::active_modeldoc_with`) |
| output paths | `Z:\app\renders\<stem>.png`; `CreateDirectoryA` first, atomic write through `<path>.tmp` (`:2602-2622`, built in a 300-byte SB; `stem` in 80 bytes, `path` in 200, `file` in 96) | identical sizes and path rules; a `stem`/path that overflows its buffer answers the C's `could not write` |
| default name | `render_<GetTickCount64>_<++g_renderSeq>` (`:2624-2646`): the tick is **ms since boot**, and the counter moves only in the default branch | identical shape and identical laziness: the tick is `GetTickCount64` (Windows) / `CLOCK_BOOTTIME` ms (Linux host), and `++g_renderSeq` runs only when no usable `name` was given (see 1) |

## The four bodies

* `view.render` (`mcp_bridge.c:2668-2857`): arm `active_doc` → document → `CMapDocSession_Owner`
  → `m_pViewMgr` (`obj_is HV_MAPVIEWMGR_VT`) → `GetPreferred3DView` (`HFN_GET_PREFERRED_3D_VIEW`) →
  `obj_is HV_MAPVIEW_VT` → `GetCamera` (slot 6) → `CameraGetPosition`/`CameraGetAngles` and the
  camera's embedded `CFrustum` → the C's parameter order → `FrustumInit`/`frustum_set`/
  `FrustumUpdate` → the sentinel buffer → the optional `ToggleToolsMaterials` flip →
  `SessionUpdateViews` → `RenderViewToMemory` → the flip back → convert/stats (with the sRGB LUT
  for format 0) → CRC/PNG → the atomic write → the reply.
* `world.render` (`:2918-3105`): `engine_ifaces` (the `Connect` factory by interface name,
  identity checks, the pinned `SV_SCENEVIEW_VT`/`SV_PIPELINE_ALIAS_VT` slots, the `SceneFns`/
  `EngineFns` binds) → `GetSceneWorld` → `FindRenderingPipeline(TOKEN_TOOLS)` → the C's parameter
  order → `FrustumInit`/`frustum_set`/`FrustumUpdate` → the 1024-byte attribute block (zeroed, then
  `AttrsCtor`) → `Wait`/`BeginRenderingViews`/`CreateViewId`/`AddView` → `AddFlags(TOOLS3D |
  SIZED_SCRATCH)` → `CreateScratchTargets` → the two handle-table sets → `BeginLayerGroup("Main")`
  → `AddLayersToView` → `EndLayerGroup` → `GetLayerViewport` → `MarkRenderTargetForReadback` →
  `FinishRenderingViews(FLT_MAX)` + `Wait` → `GetRenderTargetTexture` → `ReadTexturePixels` →
  `AttrsDtor` → convert/stats → CRC/PNG → the write → the reply. The whole bracket runs inside
  the one main-thread call, as in the C.
* `pet.render` (`pet_render.inc:203-221`): parameters → `resolve_pet` → `app->mainWnd` (generated
  base size 488) → preview `+0x1D0` → scene widget `+0x110` → scene `+0x50` / camera `+0x58`,
  every hop `obj_is`-checked → `active_pet` for `hasDocument` → the shared tail with the module's
  own `CToolScene::Render` (`PETFN_TOOLSCENE_RENDER`).
* `modeldoc.render` (`modeldoc_render.inc:99-118`): parameters → the module-state reasons →
  `resolve_modeldoc` → `m_pActiveSession` → `m_pMainWnd` → scene widget `+0x1E0` → tool widget
  `+0x90` → scene `+0x50` / camera `+0x58` → `active_modeldoc` for `hasDocument` → the shared
  tail with `MDFN_TOOLSCENE_RENDER`.

## Places the port deliberately differs from the C (no byte on the wire for any readable path)

1. **`GetTickCount64()` is `Instant`-based only for the reported deltas.** The **absolute** tick is
   on the wire in the default name (`path`/`file` = `render_<tick>_<seq>.png`, `:2645`), and
   hammer-mcp relies on it staying unique across bridge unload/reload
   (`hammer-mcp/src/tools/render.rs:490-494`), so that tick is the real boot-relative value:
   `GetTickCount64()` on Windows (`Win32_System_SystemInformation`), `clock_gettime(CLOCK_BOOTTIME)`
   on the Linux host (same quantity: monotonic since boot, including suspend, via the crate's
   existing `libc` dependency), and wall-clock `SystemTime` ms on any other host. `ms.*`,
   `readback` and `durationMs` report **differences** only, so they keep the `Instant` clock
   (`now_ms`): monotonic with the same millisecond granularity, hence the C's deltas.
2. **`write_file_atomic`'s write goes through `std::fs::File::from_raw_handle` + `std::io::Write`**
   (the `CreateFileA` call keeps the C's `GENERIC_WRITE`, share 0 and `CREATE_ALWAYS`; the close is
   the `File`'s drop; `MoveFileExA(REPLACE_EXISTING)` and `DeleteFileA` are the C's). This crate's
   `windows-sys` feature set has no `WriteFile` without `Win32_System_IO`, and the same
   substitution is already used by the map lane's header read. The 64 MiB chunk loop is kept.
3. **Allocation failure is not modelled.** Every `if (!px || !raw) { ... "out of memory" }` and the
   `png encode: out of memory` branch (`:2743-2748`, `:2803`, `:2953-2958`, `:3066`,
   `inc:84-89`, `:132`) are unreachable from Rust: `Vec`/`png_encode` abort on allocation failure
   instead of answering. The `zlen > 0x7FFFFFFF` refusal in `png_encode` is kept (`Option::None`).
4. **`json_bool` bounds-checks the literal.** The C compares `p[0..4]`/`p[0..5]` past the NUL if
   the value is the last byte of the buffer; the port stops at the buffer end. A well-formed
   `true`/`false` answers identically.
5. **`view.render`'s `GetCamera` slot is an `expect`.** Slot 6 of `HV_MAPVIEW_VT` is pinned and
   re-checked by `obj_is`, so the generated `Option` is always `Some`; a `None` would panic into
   the tick's `-32603` where the C would call a null pointer. Unreachable after the module gate.
6. **Log lines with game paths go out `String::from_utf8_lossy`** (the C `logline`s raw ANSI
   bytes). Log-only.
7. **`tag_log`/`tag_lognum` use `format!`** instead of the C's 160-byte `SB` + `lognum`'s
   56-character prefix cut. Both tags are 12 characters and the `what` strings are short
   (`begin`, `finish`, `wrote`, `<hop> vtable rva `), so no line is cut.
8. **Non-Windows builds register the rows and answer the C's own refusals, but cannot render.**
   `view.render` answers the arm's `-32003 {"reason":"hammer_not_loaded"}` (no hammer.dll),
   `world.render` answers `-32020 engine interface not found (Connect factory)` (no `Connect`
   factory), `pet.render`/`modeldoc.render` answer their chain's `pet_unavailable` /
   `modeldoc_not_loaded` — every one after the C's own validation order. Bodies are unreachable
   in production behind the `-32017` gate; the live parity pass runs the Windows build.
9. **`view.render`'s reply builder is a separate function** (`view_reply_into`) rather than inline
   `sb_add`s, like every other lane's builders; the host tests hold it to the C's sequence byte
   for byte. The same for `world.render` and the shared ToolScene tail.
10. **`world.render` closes its frame on a Rust panic.** The C cannot panic between
    `BeginRenderingViews` and `FinishRenderingViews`, but Rust can (a vtable `Option`'s `.expect()`),
    so the port binds every table entry it can before the frame opens and a small `Frame` guard
    runs the C's `FinishRenderingViews(FLT_MAX)` + wait if the body unwinds. The tick still answers
    `-32603`; only the engine state on that (unreachable-after-the-gate) path differs from a C
    crash, and no reply bytes change.

## Parity coverage (the live pass the orchestrator runs)

* `view.render` — `bridge/test_view_render.py`: the default 640x360 render decodes and is
  non-blank (`untouchedPixels == 0`, `maxLuma > minLuma`), two explicit cameras differ, a repeat
  is stable, `hideToolMaterials` changes the pixels and is restored, the four parameter guards
  (`width 8`, `fov 0`, bad name, malformed origin) answer `-32602` and render nothing, and
  `doc.active` stays clean.
* `pet.render` — `bridge/test_pet_render.py`: the 320x240 render's `path`/`file`/dims/`format`/
  `durationMs`, `untouchedPixels == 0` and real contrast; guards `width 8`, `name .x`,
  `4096x4096`.
* `modeldoc.render` — `bridge/test_modeldoc_render_live.py`: the method is advertised, the result
  has `pet.render`'s exact key set, `hasDocument` agrees with `modeldoc.active`, a second render
  rewrites the same path, the three guards answer `-32602` and write nothing, and a 64x64 render
  after the refusals still succeeds.
* `view.render` as a prop baseline — `bridge/test_props.py` (`render_baseline`, `crc0`).
* `world.render` — **no live Python script exists** (`grep -l 'world\.render' bridge/test_*.py`
  is empty; it is exercised by the `render_world` workflow docs only). Its coverage here is the
  unit tests over the reply builder + parameters and the Linux integration test that pins the
  engine-walk-before-parameters order.

## `--static` mode

None of the render scripts has a `--static` mode; `bridge/test_pins_rs.py` is the static C↔Rust
table comparator and does not know these rows. A `--static` check for this group would need to:
grep `mcp_bridge.c`'s four dispatch arms for the result buffer sizes (4096 without an `ovf` arm,
`MAX_LINE - 2048`, 256 KiB with one) and compare them with `render::{view,pet,modeldoc}`'s
`RESULT_BUF`; grep the two `.inc` files for the reply key order and compare it with the
`sb_add` sequence in `view_reply_into` / `world_reply_into` / `toolscene_reply_into`; and grep the
`HANDLERS` table for the four names.
