# cs2-native FIXME: hacks, gaps, things to come back to

Every hack or throwaway helper this crate (or its live test) relies on, and every known gap.
Remove an entry when it is fixed (the numbers stay; "Fixed" at the bottom lists what went).

## Product gaps

3. **Linux not live-tested.** There is no native Steam on the dev host (only the container's
   `cs2-home` volume), so `up` on Linux (overlay + `cs2.sh` + mmload `LD_PRELOAD`) has only had
   its overlay probe run live (`overlay::probe`, zfs host, unprivileged userns + kernel overlayfs
   `userxattr`). The full ladder on a real Linux Steam install is **unverified**.
4. **No fuse-overlayfs fallback.** A kernel that refuses unprivileged overlayfs gets the
   "use `--strategy in-place`" error instead.
9. **`processes_under` matches by image path**, plus (since this round) a named process whose
   image can no longer be read when its parent is one of the session's recorded launcher pids
   (`game_pids`). A dying `cs2.exe` whose launcher pid was never recorded (a crash before the
   supervisor saved it) is still only covered by the restore's locked-file retries. The
   parent-pid branch has no test (Windows-only) and was not hit live: in every live cycle the
   images stayed readable until the processes were gone.
10. **The supervisor stays in the parent's job when breakaway is refused** (seen live: the VM's
    scheduled-task job refuses it). If that job is closed, the supervisor and the game die before
    a restore; `down`/`uninstall` then restore from the manifest. Since this round the supervisor
    detects it (`IsProcessInJob` before joining its own job), says so, and `status` shows
    `inParentJob: true` (**live** on the VM). It cannot escape the job; a launcher started from a
    plain console or Explorer is not in one.
17. **Windows MCP servers: what is still not wired.**
    * The podman groups (`launch_session`, `run_testscript`, `server_up`/`client_connect`'s
      server, `stop_session`) refuse with a pointer to `cs2mcp-native`: by design, the session is
      the launcher's. `run_testscript` (a test script with listening checkpoints) has no native
      equivalent yet.
    * Capture: the DLL's DACL is not checked, only its owner (the Unix side also checks the mode
      bits). `up --no-capture`, the "no capture layer" message and an elevated game (the loader
      then ignores `VK_ADD_LAYER_PATH`) were not run live. The Linux supervisor sets the same
      variables; that is **unverified** (FIXME 3).
    * Native VToolMessage posts only work from the game's logon session: hammer-mcp started by an
      MCP client on the desktop posts (**live**), one started over ssh gets `RuntimeUnavailable`
      with that reason (**live**). `cs2mcp-native open <asset>` uses the same post with an
      `open_asset` payload; not run live.
    * Compiles on Windows are bridge jobs only (model/particle **live** via `particle_compile`;
      `compile_map` and `model_compile` share the route but were not run live). Without a
      connected bridge advertising `job.start` they refuse with the `cs2mcp-native up` hint. A
      host-side `resourcecompiler.exe` run (no session needed) is not built: the bridge route
      covers every session the launcher starts, and a direct run would need its own lingering-tree
      handling (`vrad3.exe`, `compile_map.rs` "Quirks").
    * The compile result's `outputPath` mixes separators on Windows
      (`...\game\csgo_addons/fixmes_test/particles/probe.vpcf_c`): cosmetic, the file is found.
    * `--project-root`'s compile-time default is the build machine's checkout: `mcp-config`
      passes `<state>/mcp` on Windows, a hand-written config must too.
19. **win64 `--all-targets` clippy of `cs2-mcp-common` fails** in its test-only fake podman
    (`src/mock/podman.rs`: `std::os::unix::fs::PermissionsExt`). Pre-existing; the win64 check of
    the libs and bins is clean. The mock would need a cfg gate (it writes a bash script, so its
    tests cannot run on Windows anyway).
20. **The Metamod `Load` refusal is unit/integration-tested only** (`tests/vac_gate.rs`). No
    supported path reaches it live: on Windows Metamod is only injected by `cs2mcp_launch.exe`,
    which refuses without `-insecure` itself, and on Linux Metamod refuses a client without
    `-insecure`/`-dedicated` before it loads plugins. The tool-role refusal is live (see Fixed).

## Test-harness hacks (the live VM run, not shipped)

11. **The Windows servers are driven from the host over ssh stdio**
    (`target/fixmes-vm/{client.sh,hammer.sh}` + `mcp_tools.py`); posts that need the game's logon
    session go through `target/fixmes-vm/mcpdrive.ps1` (a PowerShell MCP stdio client) started by
    the VM host's `runui.sh` helper (outside this repo).
12. **Launching on the VM through the VM host's `runui.sh` helper (outside this repo)** (a scheduled task in the
    interactive session; the ssh session has no desktop) with `C:\cs2test\fixmes\up*.cmd`
    wrappers that redirect `up`'s output to files. The scheduled task's job is the one that
    refuses breakaway (item 10).
13. *(packaging itself is fixed, see below)* `target/game-bins` and `target/game-bins-linux-2000940`
    are hard links of the `native-launcher-windows-fixes` worktree's copies (for `cargo bridge build`);
    the Metamod payload comes from the main checkout's `target/metamod` (`package.sh` falls back
    to it).

## Fixed

* 1 (no bridge-side `-insecure` check): `crates/cs2-bridge/src/vac.rs`. Without `-insecure` (or
  `-dedicated`) the tool `Init` leaves the wiring uninitialised (no listener) and the Metamod
  `Load` refuses with the reason. Since this round the refused tool's tick only counts frames:
  `wiring::tick` (module verifier, Hammer fingerprint, drain) never runs (**live**: 0 `verify:`
  lines up to `tick 1024`, where 9e4c59f logged 18; `tests/vac_gate.rs`).
* 2 (Windows MCP servers do not build): they build and attach; what is left is item 17.
* 5 (game-written files are reported, not undone): **decided by the user 2026-11-05: keep
  listing them.** The Workshop Tools' own writes (`game/_toolsettings/asset_browser/settings.txt`,
  `game/csgo/tools_thumbnail_cache.sqlite3-shm`, `game/core/_bakeresourcecache/...`,
  `game/csgo/rpt/<date>/`) and a compile's output under `game/csgo_addons/<addon>/` stay, as after
  any Tools run; `gameWritten` lists them.
* 6 (session-file race): `<state>/lock` (`Dirs::lock`, an OS file lock) held by `prepare`, `up`
  (until the supervisor has recorded its own pid **and** the launcher's), `down` and `uninstall`.
  **Live**: a `down` fired while `up` was starting waited on the lock, then terminated the
  launcher and the game within 1.4 s and the supervisor restored (0 verify failures). The first
  cut released the lock after the supervisor's pid only; that `down` found nothing to kill and
  waited out 120 s (seen live, fixed).
* 7 (15 s retries on a read-only file): access denied on a read-only target at once (**live**:
  `uninstall` with a read-only `steam_api64.dll` failed in 0.06 s naming it read-only; a second
  `uninstall` after `attrib -R` was byte-for-byte clean). A read-only file we *created* is not a
  case: `std::fs::remove_file` removes it.
* 8 (`wait_pid` treats "cannot open" as gone): only a missing pid is gone (**live** from the
  non-elevated desktop session via `cs2mcp-native-dev wait-pid`: lsass/wininit/services
  `gone:false` at once, a missing pid `gone:true`, a short-lived child `gone:true` after it
  exited). The poll-through-a-query-handle branch was not hit (both opens were refused).
* 13 (no packaging step): `scripts/package.sh [--os windows|linux] [--out DIR] [--no-build]` lays
  out the exe, the servers, `sigs/`, `payload/{bridge-rs,metamod,gbe}` and `SHA256SUMS`
  (**live**: the VM run used `target/cs2mcp-native-windows` copied as it is to
  `C:\cs2test\fixmes`).
* 14 (gbe's Windows DLL copied from the Wine install): `scripts/stage-gbe.sh` takes it from
  gbe_fork's own release for `GBE_REF` (the user's choice, 2026-11-05, over a Visual Studio
  build: gbe_fork builds Windows only with MSVC). Pinned in the script: tag
  `release-2026_09_16_2` = commit `8479ddbc` (= the Dockerfile's `GBE_REF`, checked; another ref
  stops with "update the release pin"), asset `emu-win-release-vs22.7z` SHA-256 `d311dead...`,
  checked after the download (cached in `target/gbe-release/<tag>/`); `release/regular/x64/` is
  extracted with `7z`. **Run here**: the download, the hash check, the ref mismatch refusal and
  the cached rerun; the DLL is byte-identical (`e853944d...`) to the copy the Wine tools install
  carries, so it is the one every live run used. `GBE_WIN_DLL=<path>` still copies a file instead.
  `GBE_FROM_SOURCE=1` (Linux libs built at `GBE_REF`) is still **unverified**.
  Linux half (branch `native-launcher-gbe-linux`, 2026-11-05): the same pin block carries
  `emu-linux-release.tar.bz2` (SHA-256 `59b284b3...`); `GBE_LINUX_FROM=release|image|source`
  picks the Linux source, defaulting to `image` when podman has the image and `release`
  otherwise. `cs2mcp-native fetch-gbe` does the release path from the launcher itself (pin
  compiled in, curl + tar, into `<state dir>/gbe`, which `Artifacts::resolve` falls back to).
  **Run here**: both downloads, the hash checks, the extraction, the cached reruns, `--archive`,
  the hash-mismatch and ref-mismatch refusals, every `GBE_LINUX_FROM` but `source`, the
  no-podman default, and the payload-then-state path pick on a package layout. The release
  libs are gbe (`configs.main.ini`) but **not** byte-identical to the image's `/app` copies:
  release `874817c9...`/`8f505dff...` vs image `ac12d08b...`/`e2c6d673...`
  (`libsteam_api.so`/`steamclient.so`); their `.comment` sections name Ubuntu GCC 13.3.0 and
  Debian GCC 14.2.0. Whether the game runs with the release libs is **unverified** (no Linux
  Steam here, FIXME 3; no container run used them either).
* 15 (hidden diagnostics in the shipped binary): moved to the `cs2mcp-native-dev` binary (not
  packaged): `tree-hash`, `tree-diff`, `vtm-command`, `vtm-bridge-request`, `gbe-interfaces`,
  `wait-pid`. `cs2mcp-native open <asset>` is public (the `cargo bridge runtime open` post).
* 16 (bridge renders need an ASCII path): `write_file_atomic` uses the wide-char APIs and the
  renders dir takes any non-empty value (**live**: `CS2_BRIDGE_RENDERS=C:\cs2test\fixmes\renders-Jörg`,
  `render_world` wrote `...\renders-Jörg\fixmes_jorg.png` and client-mcp read it back; the
  container still writes `Z:\app\renders\`, **live**).
* 17 (as first written: the MCP servers do not build for `x86_64-pc-windows-msvc`): the docker
  and vk-capture parts are cfg-gated, `--app-root`, the Windows discovery default and the
  `native-windows` flavour exist. This round: `crates/cs2-vtm` (the vtmsend port, shared by the
  launcher and hammer-mcp); hammer-mcp posts natively on Windows (`run_map`, the `console_exec`
  fallback, the first-map `open_asset`; `run_map {unload:true}` **live**) and watches the
  install's console.log there (it watched `<project-root>/workshop/...`, seen live); bridge job
  logs stay out of the install (`%LOCALAPPDATA%\cs2mcpbridge\jobs`, they went to
  `<install>\cs2mcp_jobs`); `sigs/` ships via item 13. What is left is the new item 17.
* 17 (screen capture on Windows, the user's next work package 2026-11-05): `cs2-vk-capture`
  builds for win64 (`cs2_vk_capture.dll`, exports only `vkNegotiateLoaderLayerInterfaceVersion`;
  `windows/VkLayer_cs2mcp_capture.json`; the capture dir defaults to `%LOCALAPPDATA%\cs2mcp-capture`
  and must be a real directory owned by this user, checked by the layer and the server).
  `package.sh` ships `payload/vklayer/<plat>/`, `up` records it and the supervisor names it in the
  game's environment as an explicit layer (README "Capture"). `mcp-config` passes client-mcp
  `--capture-dir <state>/capture` (session `native`). **Live** on the VM: the loader put
  `cs2_vk_capture.dll` into `cs2.exe`; `screenshot` returned a correct de_dust2 frame (1280x720
  `-tools`, 1920x1080 client mode); `observe` returned frame, console and tick. `session_status`
  lists capture as available; another session name is refused. `down` was `treeClean` after both.
  Found on the way, fixed: `package.sh` built only `cs2mcp-native` (`--bin` limits every package),
  so it shipped stale `client-mcp.exe`/`hammer-mcp.exe` from whatever build was in `target/`.
* 18 (session-status texts name `cargo bridge runtime`): on Windows `launch.command` is
  `cs2mcp-native up --steam gbe --tools` and the unreachable note names `cs2mcp-native up`
  (**live**); `model_status` hints `cs2mcp-native open models/<file>.vmdl`.
