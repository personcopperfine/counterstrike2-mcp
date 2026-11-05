# 01 — Docker runtime backend: client image, gbe, lavapipe, no host X11

## Goal
`runtime::docker::DockerRuntime` launches, waits for, inspects and stops the two session flavours
(WineTools with the bridge; LinuxClient headless) and a dedicated server later, replacing
`run_vtest.sh`'s launch half and `harness/run-cs2.sh` for the server's needs; plus our thin image
layer in `docker/`.

## Prerequisites
- the client image: `ghcr.io/personcopperfine/cs2-docker:latest` (pulled), or a local build of the
  `cs2-docker` submodule (`podman build -t localhost/cs2-docker cs2-docker/client`, then
  `--image localhost/cs2-docker`); game volume `cs2-home` or host dir
  `cs2-docker/client/.cache/game` for Linux; `workshop/run/app` materialised for Wine
  (`../idabins/worknotes/wine-tools-bringup.md`).
- podman trap: `XDG_RUNTIME_DIR=<project root>/.podman/xdg`; `--network=host`
  (pasta/unshare denied on this host); several CS2 containers may run, capped by `--max-sessions`
  (about 3 fit in GPU memory at 1080p).
- Read `cs2-docker/client/entrypoint.sh` (steamcmd validate, gbe setup, `cs2_video.txt` seeding with
  llvmpipe vendor id, `LD_PRELOAD` audio libs, `-condebug -conclearlog`, console tail),
  `../harness/run-cs2.sh` (restore-then-fix preamble, `Xvfb :99 -fbdir`, snapshots),
  `../.github/skills/cs2-lavapipe/SKILL.md` (environment facts, traps T1–T17).

## Steps
1. **LinuxClient (headless, proven path)**: `client-mcp/docker/linux-client.sh`, a port of
   `harness/run-cs2.sh` (restore-then-fix, shadercache, gbe saves, Xvfb :99, env, `cs2.sh` with
   run-cs2's flags `-novid -nosound -condebug -conclearlog`; each block cites its source line).
   The runtime starts it as `podman run -d [--gpus all] --name cs2mcp-linux-client-<id>
   --network=host -v cs2-home:/home/steam -v cs2-shadercache:/cache -v <artifacts>:/work
   -v <repo>/client-mcp/docker/linux-client.sh:/cs2mcp/linux-client.sh:ro -e CS2_GAME_DIR=<game>
   -u 0 --entrypoint bash ghcr.io/personcopperfine/cs2-docker /cs2mcp/linux-client.sh [args]`. `-u 0` because the
   preamble edits `/etc/os-release` and the game's bin dir. The game is under the home volume
   (`/home/steam/steamapps/common/Counter-Strike Global Offensive/game`, run-cs2.sh:9); no `-steam`,
   no `-insecure` (run-cs2.sh uses neither). Testscript runs are refused on Linux. The script removes
   the game's console.log before launch (client/entrypoint.sh:246) and mirrors it into
   `<artifacts>/console.log`. `--gpus all` optional (NVIDIA present here); lavapipe fork otherwise.
   Screenshots do not come from the X server: frames come from the Vulkan capture layer
   (`launch_session` mounts it; see `docker/README.md`). Xvfb is only the display the game needs.
2. **WineTools (bridge-capable, needs host X today)**: as `run_vtest.sh`: `podman run -d --gpus all
   --network=host -v <app>:/app -v <cache>:/cache -v /tmp/.X11-unix:/tmp/.X11-unix
   -v <xauth>:/root/.Xauthority:ro -e DISPLAY=:0 ... ./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console
   -condebug -insecure [-testscript /app/vtest/<suite>.txt] +engine_no_focus_sleep 0
   +r_always_render_all_windows 1` (`FOCUS_ARGS` in `runtime/docker.rs`: without the second flag a
   `-tools` session renders nothing while its game window lacks focus). Stage suites
   into `<app>/vtest/` first (04). Bridge load = `tool_request cs2_bridge` via `vtmsend.exe`
   (`bridge/runtime.sh` `load_rs_bridge`), once the VToolMessage channel is up (see step 3).
   Headless WineTools is the hammer-mcp 08 problem (same fix applies; link, do not duplicate work).
3. **Readiness ladder** as typed states: container up -> X server ok (`xdpyinfo`) -> `console.log`
   exists -> `SwitchToLoop game`/menu reached (Linux) or VToolMessage channel up (Wine) -> optional
   bridge hello -> optional parked checkpoint. Wine probes, polled by `wait_session` and
   `launch_session` (never a blind sleep): the channel is up when `vtmsend.exe` prints
   `creator version` (probed once a second; `OpenMutex failed` = not yet). Bridge hello: once a
   discovery file at least as new as the launch exists, `control.hello` on it (every 500 ms); the
   tool_request is posted right after the channel is up and again every 5 s while no discovery
   file appears. A timeout quotes the last probe error and the `mcp_bridge_rs.log` lines since launch.
   Parked needs bridge hello first (the stages are in order): `bridge: false` parks without it.
4. **`exec`/`logs`**: `podman exec` for `vtmsend.exe`, `xdump`, `xkey`; logs = `podman logs` +
   `console.log` tail (+ `mcp_bridge_rs.log` for Wine).
5. **Guards** (in this order, all before `podman run`; AGENT.md rule 8):
   - suite staged: `testscript` must exist at `<app>/vtest/<name>.txt` (`SuiteNotStaged`);
   - cookie: `--xauthority` / `XAUTHORITY`, else the newest `/run/user/<uid>/xauth_*` (run_vtest.sh:44);
   - in-process gate plus cross-process lock `<project>/.podman/cs2mcp-launch.lock` across ps -> run;
   - identical running request: return it with `reused: true`;
   - global cap: at most `--max-sessions` (default 2) running `cs2*` containers, foreign ones
     included (`TooManySessions`);
   - one session per kind per shared mount: one WineTools per app dir, one LinuxClient per
     `cs2-home` volume (`KindBusy`). Inspected through `podman inspect {{json .Mounts}}`.
   - WineTools moves a stale `console.log` aside to `console.log.prev` (`run_vtest.sh:52`), right
     before `run`, and removes a stale `session.json` (a SIGKILLed bridge never deletes it).
   Every podman call has a timeout (10 s for ps/inspect/rm/logs/exec, 60 s for run); a missing
   binary is `PodmanUnavailable`. A container that exits before the target stage ends the wait with
   its exit code and the last 20 log lines (`readiness.exited`). Waits are bounded by the call budget
   (`min(timeoutMs, budget - 2 s)`); a longer wait is `pending: true` and continues with `wait_session`.
   Per-session writes are NOT isolated yet, which is why one session per kind is the rule (TODO:
   overlay the game dir and the home volume before lifting it). Still open: >= 15 GB free RAM check
   for map loads.
   **Stop** (`stop_session`, `run_testscript` teardown, server shutdown): podman is never killed
   mid-stop. `podman stop -t 0` (PID 1 is `cs2mcp_launch.exe`, the launcher; a zero timeout SIGKILLs it and the
   container's PID namespace takes `cs2.exe` with it) is spawned detached;
   the exit is awaited with `podman wait --condition=exited` (the inspect poll is the fallback when
   that command fails), bounded by the call budget. A container still stopping at the end of the
   budget is `pending: true` with `state: "stopping"`; `stop_session` again continues the same stop.
   Only then `podman rm <name>` (no `-f`), and the owned entry and a wine-tools `session.json` go.
   A stopping container still holds its app dir and cap slot, and is never reused.
6. **Our image layer** (`docker/Dockerfile`): `FROM ghcr.io/personcopperfine/cs2-docker` + `tesseract-ocr`,
   `xdump`/`xkey` built from `../harness`, `ffmpeg`/`ffprobe` (07), the headless compositor
   candidate; image `localhost/cs2-mcp-client`.
7. **Artifacts**: every session gets `<out>/<name>-<ts>/` with `console.log`, PNGs, `.dem`,
   `.webm`, capture-layer frames (`capture/`) — the judges (04) read from there.
8. Tests: argv builders as data; fake `podman` on `PATH` for the ladder; one live test per flavour
   behind `--features live`.

## Definition of done
- `launch_session {kind: LinuxClient}` boots to the menu headlessly with a non-black capture-layer frame
  (`screenshot`; same thresholds as `tests/cs2/t30`: `nonblack_frac >= 0.05`, `unique_colors >= 500`);
  `launch_session {kind: WineTools, testscript: sandbox}` parks and the bridge answers hello.

## Open questions
- Does the Linux build honour `-testscript`/`-insecure` the same way (tier0 `CTestScriptMgr` was
  reversed on Windows `tier0.dll`; engine2 calls `StartFromCommandLine` in every boot there) —
  verify on `libtier0.so` (`../idabins/linux/...`) and at runtime.
- Dedicated server flavour: `cs2 -dedicated` strings exist in `libengine2.so`; no run in this repo yet (05).
- Image pinning vs. the bridge's fail-closed pins (parent README open decision 6).

## Evidence
- Linux headless boot + snapshots [proven: `harness/run-cs2.sh`, `tests/cs2/t30_cs2_boot.sh`].
- Wine `-tools -testscript` sessions [proven: `run_vtest.sh move_jump` 17/17 checks; gametest runner].
