# cs2-mcp — agent entry point

Read this first, then the guide for the part you were asked to touch. Each crate keeps its own
`AGENT.md`; this file only routes you there and lists the rules that apply everywhere.

| You were asked to… | Read |
|---|---|
| update pointers / signatures / bindings after a CS2 update | [workflows/game-update.md](workflows/game-update.md): follow it step by step |
| work on the Rust Source 2 SDK wrapper | [crates/s2sdk/README.md](crates/s2sdk/README.md) |
| work on the signature database | [sigs/README.md](sigs/README.md) |
| work on the bridge DLL (Wine/Hammer side) | [bridge/README.md](bridge/README.md) |
| work on the client MCP server | [client-mcp/AGENT.md](client-mcp/AGENT.md) |
| work on the Hammer/tools MCP server | [hammer-mcp/AGENT.md](hammer-mcp/AGENT.md) |
| spawn a reverse-engineering (IDA) agent | [workflows/re-brief.md](workflows/re-brief.md): front-load its prompt from the skeleton there |

## Layout

```
cs2-mcp/
├── client-mcp/         MCP server for the game client
├── hammer-mcp/         MCP server for the workshop tools
├── crates/
│   ├── s2sdk/
│   │   ├── upstream/       git submodule: personcopperfine/s2sdk (branch cs2mcp/main, a fork of
│   │   │                   alliedmodders/s2sdk cs2), the C++ SDK headers
│   │   ├── s2sdk-sys/      Rust bindings generated from upstream/ (gen/gen.py; UPSTREAM.lock pins the commit)
│   │   ├── s2sdk/          safe layer: CreateInterface, schema system, convars, game events
│   │   └── s2sdk-probe/    LD_PRELOAD payload + live test in a Linux dedicated server
│   ├── metamod/upstream/ git submodule: alliedmodders/metamod-source (+ nested third_party/khook); the
│   │                   second gen.py upstream (METAMOD.lock), Metamod plugin + KHook interfaces
│   ├── cs2-bridge/     the Rust bridge (cdylib, Linux + win64): tool plugin now, Metamod plugin next
│   ├── cs2-mcp-common/ shared crate
│   ├── cs2-kv3/        KV3 text (Valve layout writer; reads ValveKeyValue's KV3 grammar too)
│   ├── cs2-kv1/        KeyValues1 text + binary (port of ValveResourceFormat/ValveKeyValue)
│   ├── cs2-dmx/        DMX binary 1-5/9 + keyvalues2 1-4 (port of ValveResourceFormat/Datamodel.NET)
│   └── cs2-vk-capture/ Vulkan layer that captures frames from the game's renderer
├── sigs/               signature database + resolver for the bridge (Windows DLLs)
├── bridge/             the Rust bridge's pins/tools: pins.py -> bridge_pins.h +
│                       crates/cs2-bridge/src/generated/pins.rs, build-rs.sh /
│                       install-rs.sh / runtime.sh, the live test_*.py
├── workflows/          cross-crate procedures (game update), update reports, upstream PR drafts
└── workshop/           git submodule: personcopperfine/cs2-docker-workshop, the workshop tools
                        image's source (published as ghcr.io/personcopperfine/cs2-workshop);
                        `workshop/run/` (gitignored there) holds the runtime state
```

**Project root** = this checkout by default (`CS2_MCP_PROJECT_ROOT` / `--project-root` points
elsewhere; `CS2DOCKER_ROOT` is the deprecated alias the shell tools still read). The shell and
Python tools resolve a linked git worktree to the main checkout (`bridge/lib-root.sh`,
`bridge/cs2mcp_root.py`); a Rust binary defaults to the checkout it was built in. Under the root:
the Wine game + tools install `workshop/run/app` (copied out of the workshop image's `/app/`),
`workshop/run/cache`, podman's runtime dir `.podman/xdg`, `artifacts/`. The containers run the
client image (`ghcr.io/personcopperfine/cs2-docker:latest` by default, built by CI in
[personcopperfine/cs2-docker](https://github.com/personcopperfine/cs2-docker); a custom build,
`podman build -t <tag> <clone>/client`, is selected with `CS2_MCP_IMAGE` / `--image`, `IMG` for
`bridge/runtime.sh`); the Linux game lives in the podman volume `cs2-home`. An older layout, a
parent directory holding this repo as `cs2-mcp/` next to `workshop/`, still works with
`CS2_MCP_PROJECT_ROOT=<that directory>`.

## Who owns pointers

- **Linux / Rust (`crates/s2sdk/`)**: interfaces, vtable slots, struct layouts and call signatures all
  come from the upstream C++ headers via `crates/s2sdk/s2sdk-sys/gen/gen.py`. When the game changes, the
  fix goes into the headers and gets regenerated, and it ends up as a pull request to
  alliedmodders/s2sdk so the community shares it. **Never hand-write an address, slot or offset in
  Rust.**
- **Windows / bridge (`crates/cs2-bridge`, driven by `bridge/`)**: the bridge is the Rust crate;
  its vtable slots, member offsets, struct sizes and consts come from the same headers via the
  generated `s2sdk_sys` (`crates/s2sdk/s2sdk-sys/src/generated/`), and its RVAs from
  `crates/cs2-bridge/src/generated/pins.rs`, which `bridge/pins.py --rust` writes alongside
  `bridge/bridge_pins.h` (that generated header stays for `sigs/scripts/sdkcheck.py` and
  history). A new one is a header declaration first. A sigs entry with an `sdk` field is the
  binary measurement that holds a generated number to the game (`pins.py` and `sdkcheck.py`
  refuse a mismatch). Never hand-write an address, slot or offset in the bridge.

## Rules that apply everywhere

1. **Generated files are never edited by hand**: `crates/s2sdk/s2sdk-sys/ir/`,
   `crates/s2sdk/s2sdk-sys/src/generated/`, `crates/s2sdk/s2sdk-sys/tests/layout.rs`,
   `bridge/bridge_pins.h`. Change the input, then regenerate.
2. **Shared checkouts.** The main `cs2-mcp` checkout is used by other sessions. Never switch
   branches, reset, or use `git stash` there. Work in a git worktree, and commit only the files
   you changed.
3. **Valve binaries never go into git.** Game DLLs/.so files live in the game installs and under
   `target/` (gitignored); don't paste their bytes into a PR or issue. Signature JSON, RVAs, masked
   patterns, hashes and header diffs are fine to share.
4. **No outward actions without the user.** Don't push, open PRs or issues, or log in to Steam
   with an account unless the user asked you to in this task. Prepare PR text as a file instead
   (`workflows/upstream-prs/`).
5. **Wait on signals, not timers.** Poll a log line, file or process with a deadline. Never write
   a fixed `sleep N` and hope.
6. **`/tmp` may be a RAM tmpfs** (it is a 16 GB one on the development host). Put build output and
   downloads under `target/<name>/` (game-update downloads: `$CS2_UPDATE_DIR`, default
   `target/cs2-update/`), never `/tmp`.
7. **At most ~3 game containers at once** (GPU memory). Check `podman ps` before starting one.
   Podman needs `XDG_RUNTIME_DIR=<project root>/.podman/xdg` on every call.
   **Only one ModelDoc instance at a time**: open models only in a tools container started with
   `TOOL=modeldoc bridge/runtime.sh up` (labelled `cs2mcp.tool=modeldoc`). `runtime.sh` refuses a
   second one and never replaces a running one; stop it explicitly
   (`CONTAINER=<name> bridge/runtime.sh down`) only if it is yours. Agents share that instance
   in turn; they never start their own.
8. **Report honestly.** If a step failed or was skipped, say which one and paste the output. A
   claim is "verified" only if you read it in the binary or saw it live.
9. **Stop and escalate** rather than guess when a workflow says STOP. A wrong pointer that looks
   right costs more than an unfinished update.

## Checks that must stay green

```sh
git submodule update --init crates/s2sdk/upstream crates/metamod/upstream workshop
git -C crates/metamod/upstream submodule update --init third_party/khook   # metamod's own submodule (gen.py reads khook.hpp)
cargo clippy --workspace --all-targets          # no warnings
cargo test  --workspace                         # no game needed
cargo check -p s2sdk-sys -p s2sdk -p cs2-bridge --target x86_64-pc-windows-msvc   # the win64 half (cs2-bridge: its IToolSystem2 slot ties compile only here)
cargo clippy -p cs2-bridge --all-targets --target x86_64-pc-windows-msvc -- -D warnings   # the win64 bridge half lints clean too
python3 -I crates/s2sdk/s2sdk-sys/gen/gen.py --check   # Rust bindings match UPSTREAM.lock
python3 sigs/scripts/sdkcheck.py                # generated SDK numbers (incl. every sigs `sdk` entry) vs the game binaries (needs target/game-bins)
python3 sigs/scripts/sigcheck.py validate       # signature DB structure
python3 sigs/scripts/sigcheck.py resolve --strict # every signature resolves in the game binaries
./bridge/build-rs.sh                            # pins.py --check --rust, builds the win64 + linux bridge, export gate (needs target/game-bins + target/game-bins-linux-<build>, sigs/README.md "Binaries")
```
