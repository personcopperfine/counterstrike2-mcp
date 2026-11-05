# cs2-mcp

Two Rust **stdio MCP servers** for Counter-Strike 2 / Source 2, built on the in-process bridge DLL
(`crates/cs2-bridge`, pinned by the signature database in `sigs/`), Rust bindings of the Source 2 SDK
(`crates/s2sdk/`), and two container images: the game client image
`ghcr.io/personcopperfine/cs2-docker` (built by CI in
[personcopperfine/cs2-docker](https://github.com/personcopperfine/cs2-docker)) and the workshop
tools image published from the `workshop/` submodule:

| Crate | Target | Guide |
|---|---|---|
| [`hammer-mcp/`](hammer-mcp/) | Windows workshop tools under Proton: Hammer, ModelDoc, particle editor (pet), asset browser; SFM low priority (workshop publishing deferred) | [hammer-mcp/AGENT.md](hammer-mcp/AGENT.md) |
| [`client-mcp/`](client-mcp/) | the game client: in-game unit tests, multiplayer, CS script / VScript, demo -> video, AI playtest | [client-mcp/AGENT.md](client-mcp/AGENT.md) |
| [`crates/cs2-mcp-common/`](crates/cs2-mcp-common/) | shared library: bridge client, tool registry, rmcp adapter, runtime abstraction, sig-DB reader, capture, mock bridge (feature `mock`) | module docs |
| [`crates/cs2-bridge/`](crates/cs2-bridge/) | the bridge DLL/.so the game loads (tool plugin + Metamod plugin) and its JSON-RPC wire contract | [crates/cs2-bridge/README.md](crates/cs2-bridge/README.md), [PROTOCOL.md](crates/cs2-bridge/PROTOCOL.md) |
| [`crates/cs2-bridge-tools/`](crates/cs2-bridge-tools/) | `cargo bridge <command>`: the pins generator (`crates/cs2-bridge/src/generated/pins.rs`), build + export gate, install/ensure, the Wine tools runtime, the Metamod build | [crates/cs2-bridge/README.md](crates/cs2-bridge/README.md) "The commands" |
| [`sigs/`](sigs/) | signature database the bridge pins come from, resolver and update judge | [sigs/README.md](sigs/README.md) |
| [`crates/s2sdk/`](crates/s2sdk/) | Rust bindings generated from the s2sdk headers (`crates/s2sdk/upstream` submodule: personcopperfine/s2sdk, a fork of alliedmodders/s2sdk), safe layer, live probe | [crates/s2sdk/README.md](crates/s2sdk/README.md) |
| [`crates/cs2-native/`](crates/cs2-native/) | `cs2mcp-native`, the launcher for a CS2 the user installed through Steam (no containers) | [crates/cs2-native/README.md](crates/cs2-native/README.md) |

Agents start at [AGENTS.md](AGENTS.md); after a CS2 update follow
[workflows/game-update.md](workflows/game-update.md).

Each server directory is self-contained for **docs** (own `AGENT.md`, `workflows/`); shared
conventions are duplicated in both guides on purpose. The **code** behind those conventions is
shared through `cs2-mcp-common`.

## Layout and project root

```sh
git clone --recurse-submodules https://github.com/personcopperfine/counterstrike2-mcp.git
# or, in an existing clone:
git submodule update --init crates/s2sdk/upstream crates/metamod/upstream workshop
git -C crates/metamod/upstream submodule update --init third_party/khook
```

| Submodule | Repo | What it is |
|---|---|---|
| `crates/s2sdk/upstream` | [personcopperfine/s2sdk](https://github.com/personcopperfine/s2sdk) (branch `cs2mcp/main`) | the Source 2 SDK headers the bindings are generated from |
| `crates/metamod/upstream` | [alliedmodders/metamod-source](https://github.com/alliedmodders/metamod-source) | Metamod:Source + KHook headers |
| `workshop` | [personcopperfine/cs2-docker-workshop](https://github.com/personcopperfine/cs2-docker-workshop) | the workshop tools image (Hammer etc. under Wine/Proton, a file-source image), published as `ghcr.io/personcopperfine/cs2-workshop` |

The **project root** is this checkout unless `CS2_MCP_PROJECT_ROOT` (or the servers'
`--project-root`) says otherwise; `CS2DOCKER_ROOT` is still read by the shell tools as a
deprecated alias. Runtime state lives under it and is never committed:

```
<project root>/workshop/run/app     the Windows CS2 + tools tree with its Proton prefix (mounted at /app)
<project root>/workshop/run/cache   the tools container's /cache
<project root>/.podman/xdg          podman's XDG_RUNTIME_DIR (also the `cargo bridge install` locks in .podman/)
<project root>/artifacts/           client-mcp run artifacts
```

`cargo bridge` (and the shell tools in `crates/cs2-native/scripts/`) resolve a linked git
worktree to the repo's main checkout, so worktrees share one game tree; a Rust server defaults to
the checkout it was built in (pass `--project-root` to a worktree build). A parent directory
holding this repo as `cs2-mcp/` next to `workshop/` (the layout this repo grew up in) works when
`CS2_MCP_PROJECT_ROOT` names it.

Images: the servers and `cargo bridge runtime` run `ghcr.io/personcopperfine/cs2-docker:latest`
(podman pulls it on first use); its source is
[personcopperfine/cs2-docker](https://github.com/personcopperfine/cs2-docker) (`client/`), whose
CI publishes it. A custom build from a clone of that repo is selected with `CS2_MCP_IMAGE` /
`--image` (servers) or `IMG` / `--image` (`cargo bridge runtime up`):

```sh
git clone https://github.com/personcopperfine/cs2-docker.git <clone>
podman build -t localhost/cs2-docker <clone>/client
export CS2_MCP_IMAGE=localhost/cs2-docker
```

The thin derived images (`client-mcp/docker/Dockerfile`, `hammer-mcp/docker/Dockerfile`) take
the base as `--build-arg BASE=<image>` (default the ghcr ref). The workshop image is a scratch
image of files (the game + tools tree and its Proton prefix); its `/app/` is the tree the tools
container mounts. Copy it to `workshop/run/app` once (it is large):

```sh
id=$(podman create ghcr.io/personcopperfine/cs2-workshop:latest /entrypoint.sh)
mkdir -p workshop/run/app && podman cp "$id:/app/." workshop/run/app/ && podman rm "$id"
```

`cargo bridge install install` then installs the bridge, Metamod and the launcher into it
([crates/cs2-bridge/README.md](crates/cs2-bridge/README.md)).

## Supported targets

* **Linux with containers** (podman; Docker untested) — the default. Both servers drive sessions in containers of the
  published client image: the Wine tools session (Hammer, ModelDoc, pet; `hammer-mcp`, and
  `client-mcp`'s WineTools flavour with the bridge) runs the tree at `workshop/run/app` and needs
  the host X display; the Linux client (`client-mcp`'s LinuxClient flavour) runs headless under
  in-container Xvfb with the game in the `cs2-home` volume (the image's own entrypoint installs it
  there with steamcmd, anonymous login). Wine sessions run with `--gpus all` (an NVIDIA GPU and its
  container toolkit); the Linux client defaults to the image's lavapipe (software Vulkan) and takes
  `gpus: true`. podman is what the code calls (`--podman` / `CS2_MCP_PODMAN`, `PODMAN` for the
  scripts); a `docker`-compatible CLI in its place is untested.
* **Linux native** — `cs2mcp-native` (`crates/cs2-native`) prepares a CS2 the user installed
  through Steam (the Linux build), launches it with the bridge loaded as a Metamod plugin and
  prints the MCP server config; the user's tree is never written (kernel overlayfs in an
  unprivileged user namespace, Linux 5.11+; `--strategy in-place` otherwise). It needs the
  packaged payload (`crates/cs2-native/scripts/package.sh --os linux`: the bridge, the Metamod
  build, the capture layer) and, for `--steam gbe`, the gbe_fork Linux libraries
  (`cs2mcp-native fetch-gbe`, curl + tar). The workshop tools are Windows-only, so this target
  serves `client-mcp`; `--tools` is refused on Linux.

`cs2mcp-native` also runs on Windows against a Steam install (`workflows/windows-native.md`);
that path is not a supported target of this repo yet.

## Building

```sh
cargo build --workspace                 # Linux half (servers, bridge .so, tools)
cargo bridge build                      # the bridge + loader for win64 + linux (needs target/game-bins*)
```

The win64 half (`--target x86_64-pc-windows-msvc`: the bridge DLL, the launcher, the capture
layer) cross-builds on Linux and needs two things on top of `rustup target add
x86_64-pc-windows-msvc`:

* **`lld-link`** on PATH (LLVM's lld; some distributions ship it as `lld-link-<version>`:
  symlink it, or set `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER`);
* the **MSVC CRT + Windows SDK** libraries in `target/xwin`
  (`crates/s2sdk/s2sdk-sys/gen/fetch-xwin.sh` installs `xwin` and splats them there, accepting
  Microsoft's license; a sysroot elsewhere can be symlinked to `target/xwin`).
  `.cargo/config.toml` links against `target/xwin/{crt,sdk}/lib/...` relative to the workspace
  root.

Status (2026-10-11): **bootstrapped** — both servers speak MCP over stdio, the bridge client and
mock are full ports of the Python POC, `session_status` works in both (plus a guarded
`console_exec` in client-mcp); the remaining tools are capability-gated stubs that point at their
workflow. Phase order everywhere: **docker runtime first**, **local game install later** (same
`Runtime` trait).

```sh
cargo clippy --workspace --all-targets   # warning-free
cargo test  --workspace                  # no game needed
```

Design decisions (details in each AGENT.md): `rmcp` 3.5 for MCP over stdio behind a one-file
adapter; our own tool registry with capability gating, per-call budgets and handle stripping;
`runtime::Runtime` trait with `docker` (podman) now and `local` later; **no addresses in Rust** —
the signature DB (`sigs/signatures`) pins the bridge DLL and the servers only read it by name
for build pinning; vision = bridge renders and game frames read from the Vulkan swapchain by our
capture layer (`cs2-vk-capture`), returned as MCP image blocks, OCR via the `tesseract` CLI (no
X server grabs); headless = Linux client under in-container Xvfb today (the display only),
Wine tools still need the host X display (work item).
