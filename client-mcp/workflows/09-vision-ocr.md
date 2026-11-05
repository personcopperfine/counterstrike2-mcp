# 09 — Vision + OCR for the game client

## Goal
Pixels and text from the running game as MCP image/text blocks, for both flavours: menu/dialog
detection, HUD reads, and playtest observations.

## Status
- **done (implemented, fake-tested; LinuxClient frames live-tested by the orchestrator):**
  `screenshot {session?, timeoutMs?, ocr?, maxWidth?}` and `observe {image: inline|small|none, ...}`
  read the game's Vulkan swapchain through the capture layer (`cs2-vk-capture`). `ocr {png|last}`
  (tesseract) and `render_world` (freecam) were already in place. The dialog check runs on OCR text
  (`observe {ocr: true}`); the live first-boot "Settings Recommendation" modal gave `screen: dialog`.
- **open:** HUD crops (`hud_read`), the `detect_screen` tool as its own name (today the classifier is
  inside `observe`), and the Wine capture path (unverified; see `client-mcp/docker/README.md`).

## Sources
- **Vulkan layer (the only source for `screenshot` and `observe`)**: `cs2-vk-capture` copies the
  swapchain image the game presents when the server requests a frame. Requires the session to be
  launched with the layer (`launch_session`, the default when `CS2_MCP_VK_LAYER_DIR` is valid).
  Results carry the presenting pid, the exe and the present index. X server grabs (Xvfb framebuffer
  dumps, XWD, `xdump`, host screenshot tools) are **not** a source. The X display (Xvfb in the
  LinuxClient container, host `:0` for WineTools) only has to exist for the game to run.
- **Freecam render** (WineTools + bridge): `render_world` (`world.render`). Not the player's view,
  but any-size offscreen map renders [proven].
- **Engine screenshot** (`png_screenshot <name>` -> `csgo/screenshots/<name>.png`, wait for
  `Wrote_Screenshot`, R/B swapped): [proven] in both flavours, not wired to a tool. Kept for test
  scripts that want the engine's own picture.

## Steps
1. **Image block policy**: `screenshot` inlines the frame at full size when it is at most
   1920x1080 (larger: a downscaled copy), or narrower with `maxWidth`. `observe` defaults to
   `image: "small"` (at most 640 px wide). The full-size PNG is always the `path`. One image per
   call; stats always included.
2. **OCR**: `tesseract` CLI (`--psm 6|11`), optional crop + 2x upscale; `ocr {png|last, crop, psm}`.
   A frame OCR runs at most 10 s. OCR failure does not fail a capture (`ocr.error`).
3. **Dialog/menu detection** (`observe`'s `screen`): `dialog` when the OCR text has a known modal
   phrase ("SETTINGS RECOMMENDATION", "DON'T SHOW AGAIN") or an `OK` button line (a standalone `OK`
   line, or a whole-word `OK` with a cue such as `Hz`, `Cancel`, `Apply`); `loading` when the frame
   has 32 or fewer distinct colours; otherwise `unknown`. Without `ocr: true` there is no `dialog`.
4. **HUD helpers** (open): `hud_read` with fixed crop presets per resolution (health/ammo/timer/money
   for 1280x720 and 1920x1080), returning parsed numbers with confidence.
5. **Perf**: a frame takes about 120 ms on the orchestrator's LinuxClient run (lavapipe/NVIDIA); the
   per-call budget gives the frame 5 s, OCR 10 s, and the bridge tick 3 s. `screenshot` also keeps
   the last 50 captures per session directory.
6. **Tests**: the fake layer (`client-mcp/tests/capture_tools.rs`): frames and metadata, timeouts,
   layer errors, concurrent captures, cleanup, image modes, a wine session with /app and /work.
   OCR golden on a checked-in HUD PNG (skipped without `tesseract`) is still open.

## Definition of done
- Linux client at the menu: `screenshot` returns a non-black image block and `observe {ocr: true}`
  says `dialog` on the first-boot modal, `menu` at the main menu once the menu phrases are known.
  (The dialog part is done; the menu classifier is open.)
- Parked WineTools in-game: `hud_read` returns health=100 at spawn (open).

## Open questions
- HUD text via OCR is brittle (fonts, scaling): a `game.state` bridge method (08) supersedes it
  where the bridge exists; for LinuxClient OCR stays the only path.
- Panorama UI text extraction without OCR (Panorama DOM / `panorama.dll` debugging hooks) — unexplored.
- Wine capture: whether Proton / pressure-vessel passes the layer environment and mounts through
  (`crates/cs2-vk-capture/README.md`, "Proton and pressure-vessel").

## Evidence
- Vulkan layer: 20/20 captures at about 120 ms each, B8G8R8A8_SRGB, 1152x720 (orchestrator, live
  LinuxClient on NVIDIA); end-to-end `screenshot` ~0.4 s and `observe {ocr:true}` = `dialog` on the
  0 Hz modal (coordinator, live).
- `png_screenshot` + `Wrote_Screenshot` checkpoint [proven: `snap.vtest`]; R/B swap [observed].
