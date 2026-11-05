# Proposed protocol changes for bridge m2.14 (ModelDoc vision pass: `modeldoc.render`)

Additive over m2.13; no major bump, **no new error codes**. One new method, `modeldoc.render`,
executed on the tools main thread like every other game call (main-thread drain) and rendering
the ModelDoc editor's main 3D viewport offscreen to a PNG. RE: `target/orch/plan.md` §0 (all
`[V]` static, modeldoc_editor.dll build 2000930; the pins are in
`sigs/signatures/modeldoc_editor.json`, status `proposed`). Consumer: `hammer-mcp`'s
`model_render` tool (WP-T) behind the existing capability gating; nothing in Rust ever sees an
address.

**Modality note.** `modeldoc.render` opens no modal, takes no lock and changes no state: it
reads the viewport's current camera, builds a `COffscreenToolSceneWindow` from it (`wnd == 0`),
runs the scene system's own Begin/Finish/Wait bracket inside the one call and reads the colour
target back into a caller buffer. It is the same call the editor's own viewport paint makes, and
the same one `pet.render` already ships. No document is required (see `hasDocument`).

## §7 method table — new row

| Method | Status | Bridge m2.14 | Params | Result | Call | Notes |
|---|---|---|---|---|---|---|
| `modeldoc.render` | implemented `[I: offline]` — `./bridge/build.sh` and `bridge/test_modeldoc.py --static` pass; run `bridge/test_modeldoc_render_live.py` to promote it to `[V runtime]` | gate `NEED_MODELDOC` | `{width?:int 16..4096 (default 1024), height?:int 16..4096 (default 768), name?:string (render_name rules)}; width*height <= 8388608` | `{path:"Z:\\app\\renders\\<stem>.png", file:"<stem>.png", width:int, height:int, bytes:int, format:"RGBA8888", hasDocument:bool, stats:{untouchedPixels, nonBlackPixels, meanLuma, minLuma, maxLuma, crc32}, durationMs:int}` | params first; `resolve_modeldoc()` (module state -> `modeldoc_mismatch`/`verifying`/`not_loaded`, then `modeldoc_unavailable`); `app+MDOFF_APP_SESSION` (`MDV_SESSION_VT`) -> `+MDOFF_SESSION_MAINWND` (`MDV_EDITORWND_VT`) -> `+MDOFF_MAINWND_SCENEWIDGET` (`MDV_SCENEWIDGET_VT`) -> `+MDOFF_SCENEWIDGET_TOOLWIDGET` (`MDV_TOOLWIDGET_VT`) -> `+MDOFF_TOOLWIDGET_SCENE` (`MDV_TOOLSCENE_VT`) and `+MDOFF_TOOLWIDGET_CAMERA` (`MDV_SCENECAMERA_VT`, a **candidate** identity); `MDFN_TOOLSCENE_RENDER(scene, camera, 0, 0, &vp{1,0,0,w,h,0,1}, px, 0)` (format 0 = RGBA8888); then the shared render tail | `-32602 InvalidParams` for out-of-range width/height, a product over 8388608, or a bad `name`, **before any object is touched**, with `pet.render`'s exact messages (`width/height: 16..4096 each, at most 8388608 pixels`, `name: [A-Za-z0-9._-], 1..64 chars, no leading dot`). `-32019 NoViewport {reason}`: `modeldoc_mismatch`, `modeldoc_verifying`, `modeldoc_not_loaded`, `modeldoc_unavailable`, `no_session`, `bad_session`, `no_main_window`, `bad_main_window`, `no_scene_widget`, `bad_scene_widget`, `no_tool_widget`, `bad_tool_widget`, `no_scene`, `bad_scene`, `no_camera`, `bad_camera`. Every hop is null -> `no_*`, then an identity mismatch -> `bad_*`, and **every** `bad_*` hop logs the observed vtable RVA against `PIN_MODELDOC` (so a wrong candidate can be fixed from the bridge log - fail closed). `-32020 RenderFailed {reason}`: `out of memory`, `readback did not run (no pixels written)`, `png encode: out of memory`, `could not write Z:\app\renders` - the same cases and messages as `pet.render`. The pixels are display-ready, so no sRGB LUT is applied (unlike `view.render`). The active session owns the main window, so a render needs one; the document itself is optional and only shows up as `hasDocument` (an open `.vmdl` tab is the normal live case). |

## §8 error codes — no new codes

m2.14 adds **no** error code. Reused, with the reason strings above: `-32602 InvalidParams`,
`-32019 NoViewport`, `-32020 RenderFailed`, plus the generic `-32017 PrerequisiteFailed
{module:"modeldoc_editor.dll", check:"identity", state:...}` the `NEED_MODELDOC` gate answers
while the module is unverified.

## §5/§6 notes

- Method table: **78 registered / 60 implemented** (m2.13: 77/59; the new row is `impl: 1`).
  `control.version.bridgeVersion` and every `"build"` field are **`m2.14`**.
- **No new pins.** `PIN_MODULE_COUNT` stays 12. The method reuses the macros the render-chain sigs
  commit (`859522c`) added to the existing `modeldoc_editor.dll` block (`MDFN_TOOLSCENE_RENDER`,
  `MDV_EDITORWND_VT`, `MDV_SCENEWIDGET_VT`, `MDV_TOOLWIDGET_VT`, `MDV_TOOLSCENE_VT`,
  `MDV_SCENECAMERA_VT`, `MDOFF_SESSION_MAINWND`, `MDOFF_MAINWND_SCENEWIDGET`,
  `MDOFF_SCENEWIDGET_TOOLWIDGET`, `MDOFF_TOOLWIDGET_SCENE`, `MDOFF_TOOLWIDGET_CAMERA`, status
  `proposed`), so the module hash and the other `modeldoc.*` methods are untouched. This pass adds
  no signature; `bridge_pins.h` is generated, never hand-edited.
- **One shared render tail.** `pet.render` and `modeldoc.render` now call one static helper
  (`toolscene_render_png`, with `toolscene_render_params` for the parameter contract) in
  `bridge/mcp_bridge_pet_render.inc`, so the two ops cannot drift: identical bounds, identical
  `-32602` messages, identical sentinel/stats/crc/png/atomic-write code and identical result
  JSON. `pet.render`'s wire output is byte-identical to m2.13 (same 16 result fragments in the
  same order) and its log lines keep the `pet.render:` prefix through the helper's `tag`
  argument. `modeldoc.render` emits no JSON of its own.
- The ModelDoc object chain lives entirely in `PIN_MODELDOC` and never mixes in a pet pin; the
  camera vtable is the one candidate the RE flagged (pet live-verified the same class inside
  pet.dll, not modeldoc_editor.dll), which is why a `bad_camera` logs its RVA.
- Result buffer: the `modeldoc.*` dispatch keeps its 256 KB scratch buffer (sized for
  `modeldoc.nodeTypes`); `modeldoc.render`'s result is a few hundred bytes, so no change.
- **One live-test pin to bump outside this WP.** `bridge/test_pet.py` (live step 0) and
  `bridge/test_pet_live.py` (step 1) assert `bridgeVersion == "m2.13"`; the m2.14 bump makes
  them fail as written, exactly as the m2.13 pass had to bump the m2.12 pins
  (`261a25f bridge tests: test_pet.py expects m2.13`). Neither file is in WP-B's file list, so
  this pass leaves them untouched: bump the two literals to `"m2.14"` before running the pet
  live suite. (Neither script has an offline mode; both are orchestrator-run live tests.)
- `bridge/test_modeldoc.py --static` is the offline/static expectation for this pass: it reads
  the committed sources and pins the build string, the seven `modeldoc.*` method rows
  (`NEED_MODELDOC, 1, 0, 0`), `pet.render`'s unchanged row, the chain pins and reasons, and the
  shared result skeleton. A missing or renamed file fails it - there is no skip.
- Live probe list for the orchestrator (`bridge/test_modeldoc_render_live.py`, run against the
  `TOOL=modeldoc` instance after `bridge/test_modeldoc.py`): version `m2.14` and
  `modeldoc.render` advertised; a 320x240 render whose host PNG is 320x240 and exactly `bytes`
  long, `untouchedPixels == 0`, `maxLuma > minLuma`, `crc32` 8 hex chars, `hasDocument` equal to
  what `modeldoc.active` reports; a second render of the same name overwriting the same path; the
  three `-32602` refusals (`width 8`, `name ".x"`, `4096x4096`) each followed by a live
  `control.ping` (`mainThreadAgeMs < 5000`) and by a renders-directory snapshot proving no file
  appeared; and a final 64x64 render proving the path still works. Not yet run here - a live
  claim needs the run's output.
