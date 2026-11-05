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
