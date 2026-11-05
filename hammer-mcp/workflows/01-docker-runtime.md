# 01 — Docker runtime backend (phase 1)

## Goal
`runtime::docker::DockerRuntime` brings a tools session up/down, loads the bridge, and exposes
paths/logs — replacing `bridge/runtime.sh` for the server's needs. Plus a thin
image layer of our own (`docker/`) on top of the published client image.

## Prerequisites
- Images: `ghcr.io/personcopperfine/cs2-docker` (client userland: steamrt4, Xvfb, lavapipe fork, gbe; source in the
  `cs2-docker` submodule), notes in
  `../.github/skills/cs2-lavapipe/SKILL.md`; the workshop tree materialised at `workshop/run/app`
  with content merged, gbe `steam_api64.dll`, `S:` drive disabled, `/cache` volume
  (`../idabins/worknotes/wine-tools-bringup.md` steps 1–5).
- Bridge built and installed: `runtime.sh build && runtime.sh install` (manifest entry in
  `game/bin/sdkenginetools.txt`, read at toolframework2 Init -> needs a restart).
- `vtmsend.exe` present in the tree (`/app/entrypoint.sh vtmsend.exe`).
- Host X display + `XAUTHORITY` (until workflow 08).

## Steps
1. **Facts -> code.** Encode `runtime.sh cmd_up` in `DockerRuntime::up`:
   `podman run -d --gpus all --name <c> --network=host -v <app>:/app -v <cache>:/cache
   -v /tmp/.X11-unix:/tmp/.X11-unix -v <xauth>:/root/.Xauthority:ro -e DISPLAY=:0
   --entrypoint /bin/bash ghcr.io/personcopperfine/cs2-docker -c "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -tools
   -vulkan -console -condebug [-addon X] +engine_no_focus_sleep 0
   +r_always_render_all_windows 1"` (08 "Focus": the second flag keeps an unfocused session rendering).
   `--gpus all` only when
   `nvidia-smi` works; lavapipe otherwise (slow but boots).
2. **Readiness ladder** (each a typed state in `RuntimeStatus.notes`): container running -> VToolMessage
   channel up (`vtmsend` echo probe prints `creator version`) -> bridge loaded (`tool_request
   cs2_bridge` posted, `session.json` appears) -> `control.hello` ok -> module identity checks settled
   (`control.capabilities.modules[*].state` not `not_loaded|verifying`) -> document open (`doc.active`).
3. **`exec`**: `podman exec -i <c> bash -c 'cd /app && ./entrypoint.sh <exe> ...'` for `vtmsend.exe`
   (payload file `/app/vtm_payload.kv3`, KV3 header as in `runtime.sh put_payload`).
4. **`logs`**: `podman logs` tail + `<app>/mcp_bridge_rs.log` + `<app>/game/csgo/console.log`.
5. **Guards**: refuse `up` when any `cs2*` container exists (one CS2 at a time; RAM), when the
   discovery file is stale from a previous run (delete before launch), when `XAUTHORITY` is missing.
   Set `XDG_RUNTIME_DIR` for podman if the rootless-runtime trap applies (`SKILL.md` T1).
6. **Our image layer** (`docker/Dockerfile`): `FROM ghcr.io/personcopperfine/cs2-docker` + `tesseract-ocr`,
   helper binaries (`xdump`, `xkey` from `../harness`), and the headless display candidate (08).
   `docker/entrypoint.sh wine-tools ...` wraps the command above so the backend passes a flavour,
   not a shell string. Image name `localhost/cs2-mcp-tools`.
7. **`--launch`** in the server = `runtime.up()` + readiness ladder with a 120 s wait; attach-only
   otherwise (`session_status` prints the equivalent command).
8. Tests: unit-test the command builders (argv as data), mock `podman` with a fake binary on `PATH`
   for the ladder; one live test behind `--features live`.

## Definition of done
- `hammer-mcp --launch` from a cold state reaches "document open" on `maps/probe.vmap` (addon
   `mcp_test`) within 3 min on a warm shader cache, and `session_status` shows every ladder step.
- `runtime.sh` is no longer needed for the server's own loop (it stays for bridge development).

## Open questions
- Image pinning (parent README open decision 6): pin the tools image digest / block CS2 updates in
  the scratch tree, or rely on the bridge's fail-closed pins + WORKFLOW §10.
- `workshop/run/` is outside this repo and gitignored in the parent (open decision 2); the backend
  should fail with a clear message when it is missing, and `research/` may get a materialise script.
- Rootless `--gpus all` + CDI on other hosts; lavapipe-only tools sessions are untested (the
  wine bring-up used NVIDIA).
- Security stance `--network=host` + token only (single-user machine) — state in docs, revisit for
  shared hosts (parent README open decision 5).

## Evidence
- `bridge/runtime.sh` (`up`, `wait_channel`, `load_bridge`, `wait_doc`, `wait_modules`) [proven, `cycle` passes on 2000930].
- `../idabins/worknotes/wine-tools-bringup.md` [proven on host X; Xvfb failure documented].
- `workshop/Dockerfile` (file-source image, Proton prefix layout), `workshop/entrypoint.sh` (wine env, exe allowlist).
