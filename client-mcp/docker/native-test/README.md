# native-test: the Linux acceptance image

The client image with a Linux launcher package baked in; its entrypoint is `linux-client.sh` in
launcher mode, so a container runs `cs2mcp-native run --steam gbe` against the game in the
`cs2-home` volume and restores + verifies the tree when it ends (`podman stop`), or on the next
start after a crash (`podman kill`). Results of the last run: `workflows/native-launcher-report.md`.

```sh
crates/cs2-native/scripts/package.sh --os linux        # target/cs2mcp-native-linux
client-mcp/docker/native-test/build.sh                 # localhost/cs2mcp-native-test:latest
export XDG_RUNTIME_DIR=<project root>/.podman/xdg
podman ps -a --filter volume=cs2-home                  # nothing else may use the volume
podman run -d --name cs2mcp-native-test --network=host \
  -v cs2-home:/home/steam -v cs2-shadercache:/cache -v "$PWD/target/native-test/work:/work" \
  [-e CS2MCP_NATIVE_ACCEPT_CONFLICTS=1 -e CS2MCP_NATIVE_CLEANUP_LEGACY=0] \
  localhost/cs2mcp-native-test:latest [game args, default: +map de_dust2]
podman stop -t 90 cs2mcp-native-test                   # restore + verify; /work/cs2mcp-native-run.json
```

Readiness: `[cs2mcp-mmload] redirect:` in `podman logs`, then a
`/home/steam/.local/share/cs2mcpbridge/session.json` newer than the start, then
`podman exec cs2mcp-native-test /cs2mcp/native/cs2mcp-native --discovery <that file>
--game-root "/home/steam/steamapps/common/Counter-Strike Global Offensive"
--state-dir /home/steam/.cs2mcp-native status --hello`. The tree is compared with
`cs2mcp-native-dev tree-hash` through `podman unshare` (the volume belongs to a subuid).

The two variables keep an old client-image layout in the volume (setup_gbe.sh's gbe and
`steam_settings` link, install-rs.sh files) and put it back as found; without them the run stops
on those findings (`CS2MCP_NATIVE_ACCEPT_CONFLICTS=0`) or moves them out of the volume for good
(`--cleanup-legacy`, linux-client.sh's default).
