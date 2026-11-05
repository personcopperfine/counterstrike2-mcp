# cs2-vk-capture

`VK_LAYER_CS2MCP_capture`: a Vulkan layer that copies the swapchain image a game presents to disk
when a capture is requested. client-mcp screenshots come from the game's renderer this way, not from
an X server grab.

CS2 presents through Vulkan in both session flavours, so one layer covers both:

- native Linux client: the host Vulkan loader, backed by lavapipe or NVIDIA;
- Windows `cs2.exe -vulkan` under Proton: winevulkan forwards to the host Linux loader (not verified,
  see "Proton and pressure-vessel" below).

The layer uses only the Vulkan dispatch chain. It contains no game addresses and no signatures. The
only exported symbol is `vkNegotiateLoaderLayerInterfaceVersion` (check with `nm -D`); every other
entry point is reached through the loader-provided GIPA/GDPA.

## Build and install

```sh
CARGO_TARGET_DIR=<repo>/target/vk cargo build -p cs2-vk-capture            # debug
CARGO_TARGET_DIR=<repo>/target/vk cargo build -p cs2-vk-capture --release  # release
```

A deployable directory (both manifests resolve `library_path` relative to their own directory):

```
<layerdir>/
  VkLayer_cs2mcp_capture.json           explicit manifest   (VK_INSTANCE_LAYERS)
  libcs2_vk_capture.so                  the library (copy or symlink)
  implicit/VkLayer_cs2mcp_capture.json  implicit manifest   (CS2MCP_CAPTURE=1)
```

The implicit manifest lives in its own directory so the loader never sees the layer twice. Both
manifests declare `api_version` `1.4.0` (`tests/manifest.rs` keeps them in sync).

## Enabling it

Explicit (only the processes you name):

```sh
VK_ADD_LAYER_PATH=<layerdir> VK_INSTANCE_LAYERS=VK_LAYER_CS2MCP_capture \
CS2MCP_CAPTURE_DIR=<capturedir> <game command>
```

Implicit (every Vulkan process with the variable set):

```sh
VK_ADD_IMPLICIT_LAYER_PATH=<layerdir>/implicit CS2MCP_CAPTURE=1 \
CS2MCP_CAPTURE_DIR=<capturedir> <game command>
```

Disable an implicit layer with `CS2MCP_CAPTURE_DISABLE=1`.

Check that the loader sees it:

```sh
VK_ADD_LAYER_PATH=<layerdir> VK_INSTANCE_LAYERS=VK_LAYER_CS2MCP_capture vulkaninfo --summary
# "Instance Layers" lists VK_LAYER_CS2MCP_capture 1.4.0
```

Capture directory: `CS2MCP_CAPTURE_DIR` if set; else `$XDG_RUNTIME_DIR/cs2mcp-capture`; else
`/tmp/cs2mcp-capture-<uid>`. The server creates the directory 0700 and the layer never creates it.
Both sides refuse a directory that is not a real directory (a symlink is refused), not owned by the
effective user, or that has any group or other permission bits. The layer then ignores requests and
logs (with `CS2MCP_CAPTURE_DEBUG=1`); the server returns an error. Set `CS2MCP_CAPTURE_DIR`
explicitly when the two sides run in different containers.

### Linux client in the container (`ghcr.io/personcopperfine/cs2-docker`)

The CS2 process is the one that loads the host Vulkan loader, so the variables must be set in the
container's environment and the layer directory must be mounted into the container:

```sh
podman run ... \
  -v <layerdir>:/opt/cs2mcp-layer:ro \
  -v <capturedir>:/cs2mcp-capture \
  -e VK_ADD_LAYER_PATH=/opt/cs2mcp-layer \
  -e VK_INSTANCE_LAYERS=VK_LAYER_CS2MCP_capture \
  -e CS2MCP_CAPTURE_DIR=/cs2mcp-capture \
  ghcr.io/personcopperfine/cs2-docker ...
```

Use `VK_ADD_IMPLICIT_LAYER_PATH=/opt/cs2mcp-layer/implicit` with `-e CS2MCP_CAPTURE=1` to enable it
for every Vulkan process in the container. The server reads `<capturedir>` on the host, so the
mount must be the same directory from both sides. With lavapipe, also pass the ICD variables the
image already uses (`VK_DRIVER_FILES`).

### Windows client under Wine (`cs2.exe -vulkan`)

The layer runs inside the Linux process that hosts Wine, because winevulkan calls the host loader
from there. Three things must hold:

1. The `VK_*` variables and `CS2MCP_CAPTURE_DIR` are in the environment of the Wine process (the
   Proton launch), not only on the Windows side. Windows-side environment variables do not reach the
   host loader.
2. The layer directory and the capture directory are visible inside the sandbox (see below).
3. The game runs with `-vulkan`, so it does not go through DXVK.

Example launch options (adapt the paths):

```text
VK_ADD_LAYER_PATH=<layerdir> VK_INSTANCE_LAYERS=VK_LAYER_CS2MCP_capture CS2MCP_CAPTURE_DIR=<capturedir> %command% -vulkan
```

### Windows host (`cs2.exe -vulkan`, `cs2mcp-native`)

The same crate builds for `x86_64-pc-windows-msvc` as `cs2_vk_capture.dll`. As on Linux, it
exports only `vkNegotiateLoaderLayerInterfaceVersion` (check with `llvm-objdump -p`). Its manifest
is `windows/VkLayer_cs2mcp_capture.json`, with `library_path` `.\cs2_vk_capture.dll`: the Windows
loader resolves a path against the manifest's directory only when it contains a backslash.
`cs2mcp-native up` puts the manifest and the DLL in one directory and names it for the game only,
with `VK_ADD_LAYER_PATH`, `VK_INSTANCE_LAYERS` and `CS2MCP_CAPTURE_DIR`
(`crates/cs2-native/README.md`, "Capture"). The loader ignores `VK_ADD_LAYER_PATH` in an elevated
process.

Capture directory without `CS2MCP_CAPTURE_DIR`: `%LOCALAPPDATA%\cs2mcp-capture`. Both sides require
a real directory (no reparse point) whose owner SID is the process token's user or its default
owner (`src/winsec.rs`). The DACL is not checked: a directory under `%LOCALAPPDATA%` inherits the
profile's private one. Result files are created with `CREATE_NEW` and renamed into place. Verified
live on the Windows test VM (NVIDIA, `-tools` and client mode).

### Proton and pressure-vessel (UNVERIFIED)

Not tested against a live Proton session. The known risks, to check before relying on the Wine path:

- Steam Linux Runtime (`pressure-vessel`) runs the game in a container. Environment variables set
  outside it may be dropped, and the layer directory and capture directory must be mounted into it.
  Pass the variables through and bind-mount the paths (`PRESSURE_VESSEL_FILESYSTEMS_RW` /
  `PRESSURE_VESSEL_FILESYSTEMS_RO`, or the runtime's pass-through options).
- The runtime may ship its own Vulkan loader or ICDs. The layer then must be visible to *that*
  loader, which is not necessarily the host one.
- `XDG_RUNTIME_DIR` inside the sandbox may differ from the host's. Set `CS2MCP_CAPTURE_DIR`
  explicitly for the Wine path.
- The request and result files cross the container boundary through that mount. A mount that is
  not shared (or a different uid in the container) makes requests invisible and results
  unreadable.

## Protocol

1. The server writes `request.json` with `{"id":"<id>","format":"png"}`. The write is exclusive (a
   hard link from a private temp file), so an existing request is never overwritten. Taking or
   withdrawing a request is a rename to a private name followed by an id check, so a request the
   server publishes meanwhile is never deleted by mistake. `id` matches
   `[A-Za-z0-9_-]{1,64}`. A request older than 10 s by mtime is treated as abandoned.
2. On each `vkQueuePresentKHR` the layer checks for a request at most every 16 ms, and only while no
   capture is in flight. If one exists and the presented swapchain is capturable, the layer records
   and submits the copy. While the present shows only non-capturable swapchains (or a queue the layer
   does not track), the request stays pending; after 2 s or 60 presents without a capturable target it
   is answered with `{"id","error":"swapchain not capturable"}`.
3. The layer writes `<id>.png`, then `<id>.json` (each via an exclusive temp file and a rename),
   then removes `request.json` if it still names `<id>`. Results:
   - success: `{"id","width","height","format","presentIndex","ms","pid","exe"}`. `presentIndex` is
     the 0-based index of the `vkQueuePresentKHR` call the layer saw (counting all presents in the
     process). `ms` is the Unix time in milliseconds when the copy was taken. `pid` and `exe` identify
     the process that wrote the frame;
   - failure: `{"id","error","pid","exe"}`.
4. The server (`cs2_mcp_common::capture::vklayer::request_frame`) waits for `<id>.json` and returns
   `<id>.png`. `cleanup(dir, keep_last)` removes old result files.

## Present path

Only the recording and the submit happen on the present thread. The layer:

1. records PRESENT_SRC -> TRANSFER_SRC, `vkCmdCopyImageToBuffer` into a host-visible readback buffer,
   then TRANSFER_SRC -> PRESENT_SRC, in a transient command pool created for that capture;
2. submits it on the present's queue. The submission waits on the application's present wait
   semaphores (stage TRANSFER) and signals a layer-owned semaphore;
3. calls the real present with that semaphore as its only wait, so the present is ordered after the
   copy;
4. hands the capture to a readback thread. The thread waits for the fence once, maps the buffer,
   publishes the files, and returns the buffer to the swapchain's cache. If the fence does not signal
   in time, the GPU objects are not freed: they go to the device's deferred list and are freed on a
   later present once `vkGetFenceStatus` reports completion.

Layer-owned semaphores are kept one per (swapchain, image index) and reused: re-acquiring an image
implies the previous present finished waiting on it. A semaphore whose present did not succeed is
retired instead. Retired semaphores and readback buffers of a destroyed swapchain are freed with the
device (see "Deviations").

Failure rules: before the submit, any failure becomes `{id, error}` and the present is unchanged.
After a successful submit nothing can fail back into the present; the original present is never sent
with its semaphores already consumed. The readback thread runs under `catch_unwind`, and the capture
slot is released in `Pending`'s drop, so a panic there cannot leave the layer refusing captures.

The buffer barrier TRANSFER -> HOST makes the copy visible to host reads (a fence alone does not, and
HOST_CACHED memory may be non-coherent); the readback still invalidates the range.

Destroying a swapchain waits (bounded, 2 s) only on the fences of its own copies that may still run;
it never idles the whole device. On timeout its objects go to the deferred lists, and the copy
may then outlive the swapchain: it still reads the image through its own fence, so the layer keeps that
fence until it signals. The application's own presentation of those images is outside the layer's
control. A swapchain destroyed while its copy is still running is the case the frame-200/201 harness
case exercises.

Swapchains: `TRANSFER_SRC` is added to `imageUsage` only if the surface supports it, the swapchain is
not PROTECTED or a SHARED present mode, and `vkGetPhysicalDeviceImageFormatProperties2` accepts the
final usage (honouring a `VkImageFormatListCreateInfo` on a mutable-format swapchain). Otherwise the
swapchain is passed through and never captured.

## Formats and colour

Readback and conversion to RGB8 (alpha dropped): `B8G8R8A8_UNORM`, `B8G8R8A8_SRGB`,
`R8G8B8A8_UNORM`, `R8G8B8A8_SRGB`, `A2B10G10R10_UNORM_PACK32`, `A2R10G10B10_UNORM_PACK32`. Only PNG
output is supported.

Only the `SRGB_NONLINEAR` colour space is captured. An HDR or wide-gamut swapchain (for example
`A2B10G10R10` with `HDR10_ST2084`) gets `{id, error}`, never a silent linear conversion.

## Debugging

`CS2MCP_CAPTURE_DEBUG=1` logs to stderr. Nothing is logged otherwise. Write errors on stderr are
ignored. Every entry point runs under `catch_unwind`.

## Tests

- Unit tests: `CARGO_TARGET_DIR=<repo>/target/vk cargo test -p cs2-vk-capture` (conversions, the
  frame pattern, the manifests).
- Test knob: `CS2MCP_CAPTURE_FENCE_TIMEOUT_MS=0` never waits on a copy, so every capture takes the
  deferred path (`fence_timeout_defers_and_recovers`). Default 10000 ms. The knob is honoured only in
  debug builds (`debug_assertions`) or when the crate is built with `--features test-knobs`; a release
  build ignores it. The live tests use the debug build from `cargo build -p cs2-vk-capture`.
- Live tests (need a built layer and an ICD). `cargo build` the layer first: `cargo test` builds
  only the rlib, not the cdylib the loader loads.

  ```sh
  CARGO_TARGET_DIR=<repo>/target/vk cargo build -p cs2-vk-capture
  CARGO_TARGET_DIR=<repo>/target/vk cargo test -p cs2-vk-capture --test live -- --ignored --nocapture
  ```

  `harness_schedule_is_pixel_exact` runs `tests/vkcube.sh` with `vk_present_harness`, which plays the
  server itself: eight captures (adjacent and mid-run, one after a soft swapchain recreate, and one
  requested at frame 200 whose swapchain is destroyed at frame 201 without a device idle), each
  verified pixel for pixel against the frame its `presentIndex` names. `request_frame_end_to_end`
  runs the real `request_frame` against a continuously presenting harness (the harness exits on a
  stop file, so validation output is flushed).
- Validation: point `CS2VK_VALIDATION_DIR` at a directory with a Khronos validation manifest (its
  `library_path` must be `./libVkLayer_khronos_validation.so`, relative to that directory; a bare
  name is looked up by the system loader and does not load). The script sets
  `VK_LAYER_VALIDATE_SYNC=1` for synchronization validation, requires the loader to report the layer
  as inserted, and fails on any `VUID` or `SYNC-` message. The Rust live tests apply the same checks.

  To get the layer without root on Ubuntu: `apt-get download vulkan-validationlayers` into a
  directory, `dpkg-deb -x` it, copy `usr/share/vulkan/explicit_layer.d/VkLayer_khronos_validation.json`
  and link `usr/lib/x86_64-linux-gnu/libVkLayer_khronos_validation.so` side by side, and rewrite the
  manifest's `library_path` to `./libVkLayer_khronos_validation.so`.
- Shell by hand: `CS2VK_LAYER_SO=... CS2VK_HARNESS=... CS2VK_ICD=... tests/vkcube.sh`. `CS2VK_APP=vkcube`
  runs `vkcube` on a private `Xvfb :93` when both are installed; it checks only the result files.
