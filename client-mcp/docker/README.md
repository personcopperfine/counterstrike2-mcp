# docker/ — "our own image" bits

[personcopperfine/cs2-docker](https://github.com/personcopperfine/cs2-docker) (`client/Dockerfile`) and the `workshop/` submodule
(`workshop/Dockerfile`) own the game/runtime images (published as `ghcr.io/personcopperfine/cs2-docker` and `ghcr.io/personcopperfine/cs2-workshop`). This directory holds what THIS server adds on top (a thin layer `FROM ghcr.io/personcopperfine/cs2-docker`):
OCR (`tesseract-ocr` + eng data), a headless display stack candidate, helper binaries
(`vtmsend.exe`, `xdump`, `xkey`), and an entrypoint that launches the session the `docker`
runtime backend asks for (the Linux launch scripts are `docker/linux-client.sh` and
`docker/linux-dedicated.sh`, both sourcing `docker/lib-preamble.sh`; `entrypoint.sh` is still a stub).
See workflows 01 (runtime), 05 (dedicated server) and the headless/vision workflows.

## Capture layer (`build-vk-layer.sh`)

Screenshots and `observe` read frames from the game's own Vulkan swapchain through the
`cs2-vk-capture` layer, not from an X server. The host builds it once:

```sh
client-mcp/docker/build-vk-layer.sh            # -> <cs2-mcp>/target/vklayer (CS2_MCP_VK_LAYER_DIR)
client-mcp/docker/build-vk-layer.sh /some/dir  # another directory: point CS2_MCP_VK_LAYER_DIR at it
```

The script runs `cargo build --release -p cs2-vk-capture` (honours `CARGO_TARGET_DIR`) and installs
`libcs2_vk_capture.so` and `VkLayer_cs2mcp_capture.json` side by side. `session_status` reports
`runtime.vkLayer.valid`; a launch mounts the layer only while both files are present.

What a launch adds when the layer is valid (and `capture` is not `false`):

- `-v <layerdir>:/cs2mcp/vk:ro`, `-e VK_ADD_LAYER_PATH=/cs2mcp/vk`,
  `-e VK_INSTANCE_LAYERS=VK_LAYER_CS2MCP_capture`, `-e CS2MCP_CAPTURE_DIR=/work/capture`;
- `<artifacts>/capture` created 0700 on the host (it is `/work/capture` in the container).

Linux client: `/work` is the artifacts dir already. Wine tools: the artifacts dir is mounted at
`/work` only for capturing sessions, and the layer environment goes to the container, so it reaches
the Wine process through `entrypoint.sh`. **The Wine path is unverified**: Proton / pressure-vessel
may drop the environment or hide the mounts (see `crates/cs2-vk-capture/README.md`, "Proton and
pressure-vessel"). Check it with `screenshot` on a wine-tools session before relying on it.

### Ownership assumption (rootless podman)

The layer runs inside the container as root (`-u 0` for LinuxClient; the game's root). The server
creates `<artifacts>/capture` as the user that runs `client-mcp`, mode 0700. Both sides check it
before use: the layer refuses a capture dir that is not a real directory owned by its own effective
uid, and the server does the same with its uid (`cs2_mcp_common::capture::vklayer`).

This works only because rootless podman maps the container's root to the invoking host user, so the
layer's effective uid matches the directory owner. Rootful podman (container root is host root) or
`--userns=keep-id` breaks it: the layer's uid is not the owner's, so it ignores every request, and
`screenshot`/`observe` answer `CaptureUnavailable` after the timeout. Check the host's `podman info`
(`rootless: true`) before launching with capture; the layer's own debug log (`CS2MCP_CAPTURE_DEBUG=1`)
shows the refusal.

## Launcher mode (`--native-package`)

With `client-mcp --native-package <dir>` (`CS2_MCP_NATIVE_PACKAGE`), linux-client sessions no longer
depend on `cargo bridge install`: the container runs the launcher, which prepares the `cs2-home`
volume itself and gives it back unchanged when the game exits. `<dir>` is a cs2mcp package
(`crates/cs2-native/scripts/package.sh --os linux` output, or an unpacked
`cs2mcp-<version>-linux-x86_64.tar.gz` release).

What a launch adds: `-v <dir>:/cs2mcp/native:ro -e CS2MCP_NATIVE=/cs2mcp/native`, and
`linux-client.sh` then runs

```sh
/cs2mcp/native/cs2mcp-native --game-root <volume game root> --state-dir /home/steam/.cs2mcp-native \
    --gbe-save-dir "$GSE_SAVE_DIR" run --steam gbe --no-capture --cleanup-legacy -- <harness flags> <args>
```

- **One command, in the foreground**: it lays the bridge, the Metamod loader and gbe down in place
  (each change in a write-ahead manifest with backups, in the state dir on the same volume), runs
  `cs2.sh` in its own process group, and when the game exits restores and verifies the tree. Its
  exit code is the game's (3: the tree was not restored; the next run restores it first). The JSON
  result is written to `<artifacts>/cs2mcp-native-run.json`.
- **Stopping**: SIGTERM/SIGINT reach the launcher (PID 1 is the script, which forwards them); it
  TERMs the game's group, KILLs it after 15 s, then restores. The runtime stops these containers
  with `podman stop -t 60` (`NATIVE_STOP_SECS`); a plain `podman stop` (10 s) or `-t 0` would kill
  the restore, which the next run then does first. Stop it by hand with `podman stop -t 60 <name>`.
- **gbe belongs to the launcher**: the preamble's gbe steps (settings + `steam_settings` link,
  `/app/setup_gbe.sh`) are skipped. `--cleanup-legacy` moves what an older `cargo bridge
  install` (install-rs.sh) / setup_gbe.sh left on the volume into the state dir first, and puts
  Valve's `libsteam_api.so` back from setup_gbe.sh's `~/.steam/gbe_orig_libsteam_api.so`. Leftovers it cannot place stop the
  launch (exit 2, findings in the JSON); `CS2MCP_NATIVE_ACCEPT_CONFLICTS=1` goes on regardless.
- **No dedicated server beside it**: dedicated servers keep `cargo bridge install` on the same volume, so in
  this mode a client and a server never run together (`may_conflict`).
- **The package can also be baked into an image** instead of mounted: put it at `/cs2mcp/native`
  (read-only is fine; nothing writes there) and set `CS2MCP_NATIVE=/cs2mcp/native`.
- The Wine tools tree is unaffected (`cargo bridge install`, as before).

**Live**: `linux-client.sh` in this mode ran the real game on the `cs2-home` volume in the
native-test image (`native-test/`, `workflows/native-launcher-report.md`): prepare, the bridge's
hello, `podman kill` + recovery, `podman stop` + a verified restore. A launch through client-mcp
itself (`--native-package`) has been checked only with a fake game tree.
