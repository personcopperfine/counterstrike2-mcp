# bridge/metamod — Metamod:Source 2.0-dev binaries for the spikes

## Releases and the containers: the official dev builds (`fetch-metamod.sh`)

`fetch-metamod.sh linux|win64` takes AlliedModders' newest official Metamod:Source 2.0 dev build
from the mmsdrop listing (`mmsource-latest-linux` / `mmsource-latest-windows`), checks the
archive's SHA-256 against `MM_SHA256` (or `MM_LINUX_SHA256` / `MM_WIN64_SHA256`) when one is set,
extracts only the CS2 files plan.rs and install-rs.sh install, and writes
`target/metamod/<plat>/addons/metamod/` plus `target/metamod/<plat>/BUILDINFO-download.txt`
(build name, version, URL, archive and file SHA-256s). It is unpinned on purpose: each release
takes the newest build and records which one (`package.sh` copies the record into
`BUILDINFO.txt`; the release notes quote it). The release workflow, `package.sh` and the docker
path (`install-rs.sh` reads `target/metamod/linux/addons`) all use it. `MM_ARCHIVE=<file>` uses
a local archive instead of downloading.

`build-metamod.sh` below builds Linux from source at a pinned metamod-source commit. **Releases
do not use it**; it stays for development (a Metamod change of ours, or bisecting against the
headers' commit).

## Building from source (development only)

The Rust bridge (WP0.M) is loaded as a **Metamod:Source 2.0-dev** plugin, so WP0.1 (Wine
`cs2.exe -tools`) and WP0.2 (Linux client) need Metamod binaries built from the same
metamod-source commit our s2sdk headers pin against: `170058a` ("Bump khook with the latest
changes", 2026-10-18), nested khook `0ca2b013`.

## What the script does

`build-metamod.sh` does the whole job in containers from the upstream CI recipe
(`target/ext/metamod-source/.github/workflows/build-release.yml`) — no host toolchain, no
hand-written pointers, nothing cached outside `target/`:

* `Containerfile` derives the upstream CI image
  (`ghcr.io/alliedmodders/build-containers/debian11-clang22`) with a pinned AMBuild baked in;
  `build-metamod.sh image` builds it.
* `build-metamod.sh linux` copies the reference clone (read-only, `MM_REF`, default
  `<main checkout>/target/ext/metamod-source`, the repo's main checkout per `bridge/lib-root.sh`) into
  `target/metamod-deps/src/`, pins `MM_COMMIT` (submodules recursively, from the reference's own
  object store — no network), brings the cs2 SDK in with upstream's
  `support/checkout-deps.sh -s cs2`, then runs
  `configure.py --enable-optimize --sdks=cs2 --targets=x86_64 --hl2sdk-root=/deps` + `ambuild`
  inside the image and copies `build/package/addons` to `target/metamod/linux/addons`
  (`addons/metamod/bin/{linux64/server.so,linuxsteamrt64/{libserver.so,metamod.2.cs2.so}}`).
* `build-metamod.sh all` runs image, linux, then win64 (which stops, see below).

Everything under `target/metamod-deps/` is cache and survives runs; a second run logs
`src cache hit` / `deps cache hit: … not cloning` and re-runs only the (incremental) build.
Outputs land in `target/metamod/{linux,win64}/addons` plus `target/metamod/BUILDINFO.txt`
(metamod/khook/safetyhook/s2sdk commits, image digest, AMBuild commit, sha256 + `file` of every
binary). Outputs are only replaced after a successful build, so a failed run (e.g. a bad
`MM_COMMIT`) leaves the previous output untouched. All containers run with `--rm` and are
labelled `cs2mcp.metamod=<platform>`. Podman needs
`XDG_RUNTIME_DIR=<project root>/.podman/xdg` (the script sets it when unset);
`MM_NETWORK` (default `host`) selects the container network — the rootless pasta backend on this
host fails with `pasta failed with exit code 1: Couldn't open network namespace … Permission
denied`, and the network mode does not affect the artifacts.

## win64: STOPPED, blocked in AMBuild

`build-metamod.sh win64` exits 3 with the blocker list; there are **no** win64 binaries and
`target/metamod/win64/` is never created. AMBuild (the tool, pinned at
`01212cb57c96561f664b6dfb2ee10e66de6f81e4`) cannot cross-compile with clang-cl:

1. **No cross target support.** AMBuild's only knob is the `target=` phrase on `DetectCxx`
   (`ambuild2/frontend/v2_2/cpp/detect.py:95-126`); MM:S passes only `target_arch`, and there is
   no `--target-os`/`--target_platform`. AMBuild's own Windows cross path is mingw-only
   (`find_default_compiler` → `x86_64-w64-mingw32-*`), whose C++ ABI is not the game's.
   Consequence, measured by `win64-attempt`: the probe argv is
   `clang-cl -m64 test.c -o test-c` — no target triple, gcc-style `-o`, `-m64` from the arch
   rules (`cpp_rules.py:28`) — so clang-cl compiles for Windows (with the `INCLUDE` we must set
   for the SDK) and then hands COFF objects to GNU `ld`, which cannot resolve the MSVC CRT:
   `undefined reference to '__security_cookie' … clang-cl: error: linker command failed with
   exit code 1`. AMBuild never finishes compiler detection, so nothing is built.
2. **clang-cl is always vendor "clang".** The vendor is read from the probe program's macros
   (`verify.py:190-212`) and clang-cl defines `__clang__`, so AMBuildScript's
   `cxx.like('msvc')` branch (line 174) is unreachable; the gcc conventions apply instead
   (`-MD -MF dep` from `gcc.py:66-69`, which clang-cl reads as `/MD` + "unknown argument", and a
   `-shared -o x.dll` link through the compiler driver rather than lld-link).
3. **The SDK glue keys off the compiler family, not the target**
   (`hl2sdk-manifests/SdkHelpers.ambuild:229` gives a non-msvc compiler `COMPILER_GCC, _LINUX,
   LINUX, POSIX`), and protoc is chosen by target platform (`SdkTarget.__init__` →
   `devtools/bin/protoc.exe`, a PE binary that needs wine).

Fixing that means patching three pinned upstream repositories (ambuild + metamod-source +
hl2sdk-manifests) into a hand-rolled cross toolchain — a new upstream feature, and the result
would no longer be the recipe upstream CI runs. The win64 plugin should come from the GitHub
Actions release artifact of the same commit (MSVC on a Windows runner); that is the
orchestrator's call.

Evidence, both reproducible:

```sh
bridge/metamod/build-metamod.sh win64-probe    # toolchain is fine
#   clang-cl 22.1.8 + lld-link + the xwin sysroot link a real DLL:
#   No such file or directory ...
#   File: probe.dll / Format: COFF-x86-64 / Export { Name: CreateInterface }
#   probe.dll: PE32+ executable (DLL) (GUI) x86-64, for Windows
bridge/metamod/build-metamod.sh win64-attempt  # AMBuild half: the two build scripts in the
#   copy are adapted (git diff -> target/metamod-deps/win64-buildsys.patch, C++ sources
#   untouched) and the build still dies in AMBuild's compiler probe; full log:
#   target/metamod-deps/win64-ambuild.log
```

## Launching: the loader is injected, not found through gameinfo

Metamod's `server.dll` loader is never on a game search path any more: there is **one** loading
path per platform, and `csgo/gameinfo.gi` is not edited (the install writes files only). Both
paths are the ones the `-tools` and client/dedicated launches use today.

* **Wine (`-tools`, wine-client)**: the container starts `game/bin/win64/cs2mcp_launch.exe`
  (`crates/mm-launch`) instead of `cs2.exe` - `entrypoint.sh cs2mcp_launch.exe <the cs2.exe
  arguments>` from `bridge/runtime.sh up`, `docker.rs` `wine_command` / `wine_client_command`.
  The launcher starts `cs2.exe` from its own directory `CREATE_SUSPENDED`, passes the Metamod
  loader path in env `CS2MCP_MM_LOADER` (absolute, derived from `<exe dir>\..\..\csgo\addons\metamod\bin\win64\server.dll`),
  injects `cs2mcp_mmload.dll` from its own directory with `CreateRemoteThread(LoadLibraryW)`,
  resumes the game, waits and exits with cs2's exit code (best effort: a kill-on-close job object keeps the game
  from outliving it; logged and skipped if Wine refuses it). The DLL swaps kernel32's IAT slot for `kernelbase!LoadLibraryExW`: the first
  call whose path ends in `\csgo\bin\win64\server.dll` goes to the Metamod loader instead, and
  every other call passes through untouched. Any failure before the resume `TerminateProcess`es
  cs2.exe and exits **3** with one `[cs2mcp-launch] FAIL: <reason>` line, so a session never runs
  Metamod-less by accident. This depends on Wine's kernel32 importing `LoadLibraryExW` from
  kernelbase and the engine calling kernel32's (true on this Wine; a Wine/Proton change makes the
  launcher refuse loudly instead of loading nothing).
* **Linux (client + dedicated)**: `csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so` is
  prepended to `LD_PRELOAD` **on the `cs2.sh` command only** in
  `client-mcp/docker/linux-client.sh` and `linux-dedicated.sh`; it is not exported, so the
  preamble's `libasound`/`libpulse` and the optional s2sdk probe keep their place and only the game
  process carries the loader (relative path: the absolute game path has spaces, which `LD_PRELOAD`
  cannot carry; cwd is the game dir). The `.so` exports `dlopen`; the first absolute
  `dlopen(".../csgo/bin/linuxsteamrt64/libserver.so")` is redirected to
  `csgo/addons/metamod/bin/linuxsteamrt64/libserver.so`, once - stderr
  `[cs2mcp-mmload] redirect: <from> -> <to>` is the success marker. Both scripts refuse to launch
  when the loader is missing (clear message, exit 1), and a missing Metamod loader at the redirect
  prints `[cs2mcp-mmload] FAIL: ...` and aborts.
* **Not a target**: real Windows / Steam-launched retail CS2. Our launch paths are the containers
  only; a game started by Steam gets no loader injection (and the launcher/injection hooks a
  kernel32 IAT slot, never Valve code bytes).

## The layout (both platforms): Metamod in its standard addons tree

`bridge/install-rs.sh install|ensure` lays this down on both platforms (backups, `uninstall`,
`status`, `ensure` as before); `install`/`ensure` on a tree with the old layout also migrate it -
they revert our old gameinfo lines, remove `game/metamod/`, `content/metamod/` and the loader
vdfs, and put the files below in place.

Wine - `<app>/game` (the tools tree; `wine-client` mounts the same tree and the same files serve
it):

```
game/bin/win64/cs2mcp_launch.exe            # starts cs2.exe + injects the loader (crates/mm-launch)
game/bin/win64/cs2mcp_mmload.dll            # the kernel32 IAT hook that redirects the server.dll load
game/bin/win64/tools/cs2_bridge.dll         # the ONE bridge image (the manifest entry loads it too)
game/csgo/addons/metamod/bin/win64/server.dll         # the loader (the Metamod build's own file)
game/csgo/addons/metamod/bin/win64/metamod.2.cs2.dll  # the core, loaded next to the loader
game/csgo/addons/metamod/metaplugins.ini    # config/plugins stay where the core looks (<mod>/addons/metamod)
game/csgo/addons/metamod/cs2mcp.vdf         # "file" "../bin/win64/tools/cs2_bridge.dll"
game/bin/sdkenginetools.txt                 # + the cs2_bridge entry (the tool role)
```

**Gone** (from an install and from an old tree on migration): `game/metamod/`, `content/metamod/`,
both `gameinfo.gi` lines. The one remaining game-file edit is `game/bin/sdkenginetools.txt` (the
tool-role manifest entry the tools framework reads at Init; nothing to do with Metamod).

`cs2mcp.vdf` resolves `../bin/win64/tools/cs2_bridge.dll` against the mod dir (`game/csgo`), so it
names the SAME tools-DLL copy the `sdkenginetools.txt` entry loads: Metamod's provider dlopens it
first (-insecure), and toolframework2's manifest-driven load then refcounts the same HMODULE
instead of mapping a second image - one module, one set of vtables/exports. That is why there is no
`game/csgo/addons/cs2mcp/` copy any more (older installs left one; `uninstall` removes it, see
install-rs.sh's LEGACY_*).

Linux - `<game>` in the `cs2-home` volume:

```
csgo/addons/metamod/                        # copy of <cs2-mcp>/target/metamod/linux/addons/metamod
                                            #   (bin/{linux64/server.so,linuxsteamrt64/
                                            #    {libserver.so,metamod.2.cs2.so}}, metaplugins.ini,
                                            #    README.txt)
csgo/addons/metamod/cs2mcp.vdf              # "file" "addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so"
csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so        # built by bridge/build-rs.sh
csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so     # the LD_PRELOAD loader
```

**Gone**: the `gameinfo.gi` lines and the VSP loader files `csgo/addons/metamod.vdf` /
`metamod_x64.vdf` (unused without a search path). Here the vdf's `file` points at the addons copy:
there is no `-tools` manifest on Linux, Metamod dlopens the plugin straight out of the addons tree
(single image again). The Linux client must launch with `-insecure`: Metamod:Source refuses without
`-dedicated` or `-insecure` (`MMS: Fatal error: ...`); `-dedicated` covers the dedicated server.
A Linux client container and a Linux dedicated container can run at the same time; both mount
`cs2-home` (the volume's files are subuid-owned, so everything runs through `podman unshare`).

`bridge/install-rs.sh --platform linux install|uninstall|status|ensure` is the productised form of the
spike (`PODMAN`/`CS2_LINUX_GAME` env for fake trees; the same backup/uninstall/ensure contract as wine).

## Why the old `-tools` layout was different

The abort below was live-verified 2026-10-19 (build 2000930, win64 CI release 1483); the gameinfo-free
standard layout that replaces it was live-verified on all four launch paths 2026-10-20 (build 2000933). Until this change `-tools` used
`game/metamod/` + `content/metamod/` and the search-path lines `Game metamod` before `Game csgo`
plus `Write csgo`, because the standard install with `Game csgo/addons/metamod` aborts at startup:

* A `Game` root under `csgo/` becomes mod csgo's FIRST root. assetsystem then looks for csgo's
  read-only asset DB (`readonly_tools_asset_info.bin`) in `csgo/addons/metamod/`, misses it, scans
  all of `game/csgo` as assets and reads `shaders_pc_000.vpk` as a VPK directory: `FATAL ERROR: VPK
  directory for '...shaders_pc_000.vpk' corrupt` (`assetsystem!0x8A350`, `!0xCDAA0`, `!0x341C90`).
  Without `content/csgo/addons/metamod` it aborts even earlier (`Failed to traverse directory`).
* Only the `game` SearchPaths key adds GAMEBIN entries (`engine2!0x193A90`); `GameBin` is not a key
  (it lands in SHADER_SOURCE and Metamod never loads).

So the abort came from the **search path**, not from where the files sat. The spike kept the
standard `game/csgo/addons/metamod/bin/win64/` files, left `gameinfo.gi` pristine and launched
`entrypoint.sh cs2mcp_launch.exe -tools ...`: the engine's `LoadLibraryExW(...\csgo\bin\win64\
server.dll)` was redirected once, Metamod's loader found the real game DLL by parsing the
unmodified gameinfo itself (`loader/gamedll.cpp` `mm_DetectGameInformation`: every `Game` path's
`bin/<os>/server`, skipping its own), Metamod loaded, the bridge logged `[mm] Load`, a
`tool_request` answered `[cs2-bridge mm+tool] Init` (ONE image, vdf unchanged
`../bin/win64/tools/cs2_bridge.dll`), `metamod.plugins` listed the bridge, `server.dll` was
verified as the game's (not the stub), and `game.saveRestore` on/off worked. No asset-system abort,
no `game/metamod`, no `content/metamod`. The Linux spike did the same with the `LD_PRELOAD`
redirect, `gameinfo.gi` pristine, `DEFAULT_WRITE_PATH` still the game's `csgo/` (**without**
`Write csgo`) and `meta list` = `[01] cs2-mcp bridge`.

Result: one loading path per platform, the game's `gameinfo.gi` never edited, `ensure` no longer
has to re-apply anything after a Steam update.

The loader is **also named `server.dll`** and Wine maps it before the game's: `GetModuleHandleA("server.dll")`
answers the loader, whose identity is not the game's (live: `server.dll` `mismatch`, `game.saveRestore`
-32017). The bridge therefore picks pinned modules by full path (`s2sdk::module::Module::find_game`: an image
under a `metamod` directory below the game root never counts; the one under the game root wins; anything
still ambiguous is refused). The Linux loader (`csgo/addons/metamod/bin/linuxsteamrt64/libserver.so`) has the
same name problem and the same rule.
Historical repro of the OLD `-tools` layout (gameinfo search path; kept for the RE trail):
`target/orch/rust-bridge/wp0-live.sh run` (backs up, installs, tests, restores). RE notes:
`target/orch/rust-bridge/re/mmtools-re.md`.
