# TODO: vision support, what is not finished (2026-10-13)

"Vision" here means that a tool returns what the editor or game shows as an MCP image block, with
pixel stats, and that a blocked UI can be diagnosed without a human.

Branch state: `modeldoc/vision` (worktree `.claude/worktrees/mdvision`) was cut from `pet/integ`.
Nothing is merged to main or pushed. Merge order: `pet/integ`, then `modeldoc/vision`. The installed
workshop DLL is m2.14; the m2.13 backup is `target/orch/mcp_bridge.m2.13.dll`.

## Done (verified live)

| Surface | Tool | Bridge |
|---|---|---|
| Particle editor preview | `particle_render` | `pet.render` (m2.13) |
| ModelDoc main viewport (active session) | `model_render` | `modeldoc.render` (m2.14) |
| Game client | `screenshot`, `observe`, `ocr`, `render_world` (client-mcp) | Vulkan capture layer, `world.render` |

## 1. Hammer: the render tools are stubs (biggest gap)

The bridge methods `view.render` and `world.render` are implemented and verified live. But in
hammer-mcp, `render_view` and `render_world` are still stubs, like 22 of the 24 Hammer tools
(`hammer-mcp/src/tools/hammer.rs`; only `session_status` has a body).
- Give both tools real bodies on the shared pipeline: `particle_render::run_render`, which handles
  PathMap, `RenderFileMissing`, decode, stats and the inline/downscale policy.
- `view.render` pixels need the sRGB LUT. pet's and ModelDoc's do not, so the shared pipeline must
  handle both.
- Add mock arms and tests mirroring `tests/model_render.rs`.
- This also blocks any "agent builds a test map" guide. Mesh, entity, save and compile are stubs
  too, so a flash worker cannot author a map through hammer-mcp yet.

## 2. Dialog / modal detection in the tools (hammer-mcp has none)

Modals block the tools main thread. Today alone that happened with ModelDoc's "Unrecognized Asset"
and pet's "Recover Files?", and the only symptom is `mainThreadAgeMs` growing.
- hammer-mcp has no `ocr` and no `detect_dialog` (workflow 07, steps 3–4, not started).
- **Decision needed:** workflow 07 plans X window grabs (`xdump`, `import -window root`), but the
  standing rule is "Vulkan hook only, never X grabs". The tools UI is Qt, not the game swapchain.
  Options:
  - window titles only, from `xwininfo` (what I used today; it names the modal, with no pixels)
  - the bridge reading the Qt modal's text in-process
  - an exception to the rule
- At minimum: when `mainThreadAgeMs` is stale, `session_status` / `model_status` / pet status should
  report the top-level window titles. Dismissing a modal stays manual.

## 3. Particle editor (`particle_render`) leftovers

- Unverified: the widget's render-context acquire/release bracket around `CToolScene::Render`. The
  call is bare and works live, but the bracket is not understood.
- Unchecked: whether the `wnd == 0` offscreen window is freed after each render (a possible leak
  per render). Measure the process memory over about 100 renders.
- No camera control: the render uses pet's preview camera as it is (no orbit/zoom/frame-system).
- The playback time is not controlled (which frame of the simulation gets rendered).

## 4. ModelDoc (`model_render`) leftovers

- No camera control (orbit, or frame the model's bounds). The render uses the viewport's current
  camera.
- The floating "Preview Window" docks (`CQModelDocEditorPreviewWindow`, a second widget at +0x58)
  are not rendered.
- Same unverified internals as pet: the render bracket, and offscreen window cleanup.
- Only the active session renders. There is no "render file X" without first switching to it with
  `model_open`.
- A `.vmdl` created after the tools started renders as an empty grid (no asset, so no mesh). The tool
  does not warn about it; it could check `hasAsset` and add a note.

## 5. Game client (client-mcp) leftovers (from `client-mcp/workflows/09-vision-ocr.md`)

- `hud_read` (HUD crops) is not done.
- `detect_screen` as its own tool is not done; the classifier lives inside `observe`.
- The Wine capture path is unverified; only the Linux client is live-tested.

## 6. Not started

- Asset browser (workflow 06): no thumbnail or preview render.
- SFM (workflow 09): the survey has to decide first whether an `sfm.render` exists.
- Headless (workflow 08): prove that every render above works with nothing shown on the desktop.
  Today's runs used `:0`.
- Optional (workflow 07 open question): a compact text "scene description" alongside each image
  (entities in the frustum, model nodes).

## Housekeeping found along the way

- `bridge/runtime.sh` `wait_modules` treats `not_loaded` modules as pending, so a ModelDoc container
  always warns "did not settle in 60s".
- Open ModelDoc models addon-relative only: `runtime.sh open models/<x>.vmdl`. This is fixed in the
  docs and the `model_status` hint.
