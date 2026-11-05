# 09 — Source Filmmaker (low priority); workshop publishing deferred

## Decision (2026-10-11)
**Workshop manager / item editor / publishing are out of scope for now.** Publishing needs a real
Steam login and Steam UGC (`steam_api64.dll`), and the runtime uses the gbe_fork emulator; nothing
Steam-facing can be exercised here. No tools, no bridge work, no dry-run previews. Revisit only if a
logged-in local-install runtime (workflow 10) exists and the user asks.

## Goal (SFM only)
Minimal, safe coverage of SFM: a survey, then — if the survey says go — open/list/render for SFM
sessions through the same patterns as the other tools.

## Prerequisites
- 03–06 done (patterns, asset open, renders). SFM has **no** reversing pages yet (`sfm.dll` is in the
  tools list in `../idabins/docs/README.md` only).

## Steps
1. **Survey** (`research/sfm-survey.md`, FORMAT tags): session file format (`.dmx`?), whether the
   framework `open_asset` route opens SFM sessions in the csgo mod, whether a `view.render`-like
   offscreen render exists, what admission/visibility gates apply (`tool-plugin-admission.md`), and
   whether SFM runs under Wine in this runtime at all.
2. **Go/no-go** on bridge methods (`sfm.active`, `sfm.render`) based on the survey; if go, the same
   path as ModelDoc (04): pins -> `crates/cs2-bridge` handler -> §7 row -> tools.

## Definition of done
- Survey note with a go/no-go. Nothing else is promised.

## Open questions
- SFM under Wine: untested in this runtime; it may need the Steam client (gbe limits).

## Evidence
- None for SFM. Publish-path reversing (`../idabins/docs/flows/workshop-publish.md`) stays as
  reference material only.
