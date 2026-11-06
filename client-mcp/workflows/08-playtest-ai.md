# 08 — AI playtest: the model drives the game

## Goal
An MCP client model plays: it calls `observe` (screenshot + OCR + state + tick), decides, calls
`input_set` (buttons/angles for N frames), repeats; sessions are recorded (demo) and traced in a
format `cs2-gym` can replay.

## Status
- **done (observe half):** `observe {session?, capture?, ocr?, image?, consoleLines?, since?}` returns
  the frame (Vulkan capture layer, <= 5 s; OCR <= 10 s), the console lines since the cursor and the
  checkpoint events, `game.tick` (<= 3 s) only when the session is the bridge's own wine-tools
  session, and a screen guess. Each part is independent (`errors[]`).
- **open:** `input_set`-driven `step`, `episode_start/end`, the JSONL trace, `game.state`.
- Frames never come from an X server grab (workflow 09).

## Prerequisites
- 01 (WineTools session with the bridge), 03, 04 (parked harness), 09 (captures). Read
  `../idabins/docs/flows/client-input-pipeline.md`, `worknotes/inputhook-scratch.md`,
  `worknotes/vtest/input_hook_test.py` (yaw 90.00 exact, +-358 u along Y, jump +45..48 u),
  `../cs2-gym/docs/DESIGN.md` (obs/action spaces, `cs2gym.proto` usercmd mirror).

## Steps
1. **Act**: `input_set {press[], release[], tap[], frames, angles|null}` -> bridge `input.set`;
   names = game input values (`forward back left right jump duck attack attack2 use reload sprint
   ...`, listed in the result's `buttons`); `input_clear` on every error path and session end
   (`input.uninstall` at bridge PreShutdown too). Angle override is absolute `[pitch,yaw,roll]`;
   add relative `turn {dyaw, dpitch}` server-side.
2. **Observe**: `observe {capture: game|x|none, ocr: bool, since_tick?}` ->
   `{tick, frames, png?, ocrText?, state, radar?, sounds[], events[]}`. State comes from inside the game
   (workflow 11, bridge m2.9): `game_state` (view/eye angles, origin/eye/velocity, health, flags, active
   weapon), `weapons` (ammo + inventory), `radar` (every player/bomb/hostage with the game's spotted flags,
   projected onto the overview), `sounds_drain` (sound events since the last observation: name, source
   entindex, origin). Console `getpos` stays as the cross-check oracle. All fields are schema-resolved
   at runtime (no offsets in Rust or C); the bridge handlers are `mcp_bridge_game.inc`.
   **Not live-verified yet** (11 step 4).
3. **Step semantics**: `step {action, frames}` = `input_set` then wait until `game.tick` advanced by
   `ceil(frames/…)`; expose `wait_ticks {n}`. Document that CS2 ticks at 64/s and the hook applies
   edges per frame at frac 0 (sub-tick timing is an open item in the flow page).
4. **Episode control**: `episode_start {map, spawn|setpos, bots, record:bool}` = parked harness +
   `round` reset + `tv_record`; `episode_end` = `input_clear`, `tv_stoprecord`, artifacts
   (demo, per-step PNGs, JSONL trace of `{tick, action, obs summary}`).
5. **Gym compatibility**: the JSONL trace uses `CBaseUserCmdPB` field names (`buttons`, `viewangles`,
   `forwardmove`, `leftmove`, `subtick_moves`) so `cs2-gym` can replay traces into its simulator and
   the real game can serve as its oracle; a Python adapter in `research/gym_adapter.py` wraps
   `episode_*`/`step`/`observe` as an RLGym-style env over MCP stdio.
6. **Safety/perf**: cap frames per `input_set`; every `observe` with `capture` costs a screenshot
   (game `png_screenshot` ~hundreds of ms under lavapipe; faster on GPU); provide `capture:none`.
   Playtests run only in `-insecure` local sessions.

## Definition of done
- A scripted "agent" (research script) walks from CT spawn to a target using only `observe` +
  `input_set`, with a demo and a JSONL trace; the trace replays in the game via a generated test
  script (sanity) and loads in `cs2-gym`'s format.

## Open questions
- Reward/success signals: `game_state`/`radar` give distance/health/alive directly once live-verified; kill feed lines via console until then (`game.state`
  exists; what the first bridge read-method set should be (origin/angles/health/weapon/ammo/round).
- First-person capture under WineTools: the capture layer under Proton (unverified) vs
  `png_screenshot` (R/B swap, engine-side). X window grabs are not an option.
- Multiple agents = multiple clients = off-host (05).

## Evidence
- `input.*` `[V runtime]` (`input_hook_test.py` PASS x3); `game.tick` `[V runtime]`;
  gametest reset modes [proven]; `cs2-gym` M0 done, no sim yet.
