# 07 — Vision + OCR

## Goal
The model can look: every render/screenshot is returned as an MCP image block (bounded size) with
pixel stats, and `ocr` extracts text (dialog titles, error boxes, property panels) so stalls and
modals are diagnosable without a human at the screen.

## Prerequisites
- 03 (`render_view`, `render_world`) and 01 (runtime `exec`, host paths). `tesseract` on the host
  (`/usr/bin/tesseract` exists) and in our image layer (`docker/Dockerfile`).

## Steps
1. **Capture sources** (`src/capture`), one enum, one result shape (`png`, `width`, `height`,
   `source`, `stats{nonBlackPixels, meanLuma, crc32}`):
   - bridge renders (`view.render`/`world.render`) — no display, deterministic camera [proven];
   - **tools window / dialogs**: on host X, `xdump`-style per-window grab (`../harness/xdump.c`
     works on any X display; find the Hammer/dialog window by name via `xwininfo -root -tree`) or
     a root grab (`import -window root`, `spectacle -b -n -f -o`); under an in-container display
     (08) the same tools inside the container; `Xvfb -fbdir` XWD + `../tests/lib/xwd.py` for the
     Linux path.
2. **Image block policy**: inline PNG when `w*h <= 1920*1080`, else downscale a copy for the block
   and keep the full file path in the result; never inline more than one image per call.
3. **OCR** (`capture/ocr.rs`): `tesseract <png> stdout --psm 6` (and `--psm 11` for sparse UI),
   optional crop rectangle, upscale x2 for small UI fonts; return `{text, lines[], psm, engine}`.
   Tool `ocr {png? | last:true, crop?, psm?}`.
4. **Modal detection** tool `detect_dialog`: when the liveness gate reports `HammerBlocked`, grab the
   screen, list top-level windows (title + geometry), OCR the smallest focused one, return
   `{blocked:true, windows[], dialogText}` — model-readable diagnosis of "a modal stopped slot 13".
   Dismissing it stays manual/opt-in (desktop input policy).
5. **Known artefacts**: game `png_screenshot` has R/B swapped — fix on read; blank renders are
   detected by `untouchedPixels`/`nonBlackPixels` and annotated.
6. Tests: pure-function tests for stats/downscale; OCR golden test on a checked-in PNG of a known
   dialog (`fixtures/ocr/`), skipped when `tesseract` is absent.

## Definition of done
- `render_view` + `ocr` on a map with a text decal returns the text; `detect_dialog` identifies a
  deliberately opened "Reload from disk?" prompt on host X.

## Open questions
- Native OCR crate (`tesseract`/`tesseract-rs`) vs CLI: CLI first; revisit if latency matters for
  the playtest loop in client-mcp (same module).
- Qt accessibility (Xalia is disabled in the tree: `xalia.exe.disabled`) as a text source instead of
  OCR — possible but unexplored.
- Vision for the model beyond PNG: should tools return a compact "scene description" (entities in
  frustum from the saved `.vmap`) alongside the image? Cheap once file reads exist.

## Evidence
- `view.render`/`world.render` PNG + stats [V runtime]; `harness/xdump.c`, `tests/lib/xwd.py` [proven on Xvfb];
  R/B swap in `png_screenshot` [observed, `vtest/README.md`].
