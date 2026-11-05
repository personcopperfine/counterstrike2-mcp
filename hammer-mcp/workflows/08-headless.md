# 08 — Headless: tools without the host X11

## Goal
A tools session (`cs2.exe -tools` under Proton) starts and runs in a container with **no host
display**, so the server works on headless hosts/CI and does not fight the user's desktop.

## Prerequisites
- 01 done. Read `../idabins/worknotes/wine-tools-bringup.md` "Why Xvfb fails": under Xvfb the RandR
  output is disconnected with no EDID -> wine writes `BAD_EDID` -> DXVK "using blank" display
  metadata -> `EXCEPTION_INT_DIVIDE_BY_ZERO` in `dxgi.dll` from the bundled `SDL3.dll`. On the host
  **Xwayland** the game runs. Helpers that were tried: `../workshop/run/setedid.c`, `readedid.c`,
  `mk_edid.py`, `patch_edid_key.py` (Xvfb does not persist the RandR property across clients).

## Steps (candidates in order; stop at the first that boots)
1. **Headless Wayland compositor + Xwayland in the container** (`weston --backend=headless` or
   `sway` headless + `Xwayland`; or `cage`): matches the known-good host path (Xwayland), gives a
   connected RandR output with a mode. Package availability on steamrt4/trixie to check; add to
   `docker/Dockerfile`. Launch via `docker/entrypoint.sh headless wine-tools ...`.
2. **Xorg with the `dummy` video driver** (`xserver-xorg-video-dummy`, fixed modeline) — a real X
   server with a connected output; EDID still absent, so it may hit the same DXVK path — test.
3. **Xvfb + EDID fix made persistent**: a small X client that sets the `EDID` RandR output property
   and *stays connected* (the helpers above exited), or a wine registry pre-seed of
   `Device Parameters\EDID` + `dxvk.conf` (`dxgi.forceRefreshRate = 60` is already set; look for
   DXVK options that skip EDID/monitor metadata).
4. **Validate** each candidate with the same ladder as 01 (channel up -> bridge -> `doc.active`),
   plus `view.render` and a window grab (07) to prove pixels exist; record the RSS/CPU cost.
5. **Runtime integration**: `LaunchSpec.headless = true` selects the in-container display; the
   backend exports `DISPLAY` to the chosen server, never mounts `/tmp/.X11-unix` or `XAUTHORITY`.
6. **Focus**: keep `+engine_no_focus_sleep 0 +r_always_render_all_windows 1` (see "Focus" below);
   the engine needs no focus event to run or render.

## Definition of done
- `hammer-mcp --launch` with `headless:true` reaches "document open" and `render_view` returns a
  non-blank PNG on a host with no `DISPLAY`; documented in `docker/README.md` with the package list.

## Open questions
- Which candidate works — all are unverified for Wine+DXVK+CS2 here. Budget one day per candidate.
- GPU under a headless compositor: `--gpus all` + Xwayland without a host session (NVIDIA EGL
  device platform vs. lavapipe); lavapipe-only tools sessions may be too slow for Hammer's viewport
  but renders are offscreen anyway.
- Interaction with `xalia.exe` (disabled) and Qt's X11 platform plugin under Xwayland.

## Evidence
- Linux client headless under `Xvfb :99` is proven (`../harness/run-cs2.sh`, `tests/cs2/t30`);
  Wine tools under Xvfb fail with the documented EDID/DXVK path; host Xwayland works [proven].

## Focus (build 2000930, measured 2026-10-19 on host Xwayland/KWin)
- In `-tools` mode `CRenderService::ShouldRenderFrame` (engine2.dll!0x1e8130,
  libengine2.so!0x43f320) is `!toolsMode || IInputSystem::HasMouseFocus (slot 68) ||
  CRenderService::ShouldRender() || pendingRenders > 0`, and `ShouldRender` (engine2.dll!0x1e82d0,
  libengine2.so!0x43fe90) tests `r_always_render_all_windows` (flags 0, default 0) first.
  When it is false `CRenderService::OnClientOutput` (engine2.dll!0x1e7bd0) skips `RenderViews` and the
  game window presents nothing; `CToolService::ThrottleRenderingToYieldCPU` (engine2.dll!0x1fb9b0)
  also waits 10 ms per frame when the app is inactive too.
- Live, Asset Browser holding X focus, mouse not on the game window: main loop ~115 frames/s, but
  0 presents in 30 s (capture requests time out); after `r_always_render_all_windows 1` (console or
  `+` on the command line) 111-118 presents/s, no focus change. `r_render_coordination_state` prints
  the inputs (`IsAppActive`, `HasMouseFocus`, the convars) to console.log.
- `engine_no_focus_sleep` (FCVAR_ARCHIVE, default 20, saved as 0 in `cs2_machine_convars.vcfg`) is
  only the `_MainLoop` sleep while the app is inactive (engine2.dll!0x212fa0 decides; a minimized
  window sleeps `max(100, value)` ms); it never gated rendering, which is why it "did nothing".
- Startup was not focus-bound here: channel up 10-21 s either way. KWin activates the new Asset
  Browser window on launch (focus leaves the user's window); nothing in the engine needs that.
