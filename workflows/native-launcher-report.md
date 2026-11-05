# Native launcher (`cs2mcp-native`): milestone 1 report, 2026-11-04

Crate: `crates/cs2-native` (design: its `README.md`; hacks and gaps: its `FIXME.md`). Branch
`native-launcher`. Game build 2000940, bridge m3.18.

"**live**" below means seen on the Windows 11 test VM (`cs2-wintest`, RTX 2070 SUPER, Steam
installed and logged in, CS2 + Workshop Tools in `C:\SteamLibrary`), with the output quoted from
the run. Everything else is **unverified**.

## Milestone 1: gbe on the VM, end to end (live)

Command (via a `.cmd` wrapper run in the interactive session, FIXME 12):
`cs2mcp-native --library C:\SteamLibrary up --steam gbe --tools`

1. **prepare**: discovery through `C:\SteamLibrary\steamapps\appmanifest_730.acf`; 20 manifest
   entries (18 written paths + 2 runtime files), backups + SHA-256 in
   `%LOCALAPPDATA%\cs2mcp-native\32b646b51a29fa64\`.
2. **launch + ladder** (last of five cycles):
   ```
   ready: ProcessUp after 0.5s
   ready: ConsoleLogExists after 5.0s
   ready: MetamodLoaded after 8.5s
   ready: ToolChannel after 9.0s
   ready: DiscoveryFresh after 48.6s
   ready: BridgeHello after 49.1s
   ```
   `launch.log`: `[cs2mcp-mmload] hooked 1 kernel32 import slot(s) of LoadLibraryExW` and
   `[cs2mcp-mmload] redirect: ...\csgo\bin\win64\server.dll -> ...\addons\metamod\bin\win64\server.dll`.
3. **session_status**: Linux `hammer-mcp` through an ssh tunnel (FIXME 11, the Windows servers
   do not build yet): `"connected":true, "protocol":1, "gameBuild":"2000940",
   "bridgeVersion":"m3.18"`, 9 modules `verified` (hammer/modeldoc/pet `not_loaded`, no
   document open).
4. **down** (0.7 s): terminated the launcher and cs2.exe; the supervisor restored:
   ```
   restore: 2 restored, 17 removed, 1 unchanged, 0 conflicts, 1 game-written files saved
   every file we touched is byte-for-byte the user's again (20 entries checked)
   the game itself wrote 4 paths in the install during the session (not undone):
     changed: game/_toolsettings/asset_browser/settings.txt
     changed: game/core/_bakeresourcecache/maps/editor/toolscene_default_baked/env_cubemap_47.vtex_c
     changed: game/core/_bakeresourcecache/maps/editor/toolscene_default_baked/env_light_probe_volume_47.vtex_c
     changed: game/csgo/tools_thumbnail_cache.sqlite3-shm
   ```
5. **Independent check** (`tree-hash` of all 4347 entries before `up` and after `down`, SHA-256
   for files up to 64 MB, plus mtime): no entry added or removed; the only differences are those
   4 game-written files, **mtime only** (same size and SHA-256) in this run. In the first run the
   content of `settings.txt` and the sqlite `-shm` changed too (the Workshop Tools writing their
   own state, as they do when started from Steam).

So: every file the launcher wrote is restored byte-for-byte, mtimes included (live, 5 cycles).
The install as a whole is **not** byte-for-byte, because Workshop Tools write into it; those
writes are listed, not undone (FIXME 5).

Bugs the live cycles found and fixed (commits 665371b, 1532cc1): a killed launcher's cs2.exe
held `console.log` and still mapped `steam_api64.dll` for a moment after its process vanished
from the list; the supervisor closed its own kill-on-close job (and killed itself) before
restoring; `up`'s late session save erased the supervisor pid; `up`'s inherited stdout kept the
caller waiting; the last session's `launch.log` answered the ladder.

## Client mode with the real Steam account (live, once, as approved)

`up --steam client --tools --map de_dust2`. Steam running and logged in (ActiveUser <accountid>).
- No `steam_appid.txt` from us. The direct launch **did not relaunch through Steam**: the
  injected process ran Metamod and the bridge and stayed up until `down`.
  `console.log`: `[SteamNetSockets] AuthStatus (steamid:<steamid64>)`, USRLOCAL
  `Steam\userdata\<accountid>\730\local\`, `[Server] SV:  VAC secure mode disabled.`,
  `ss_loading -> ss_active` on de_dust2.
- Ladder: BridgeHello after 56.1 s. `session_status`: connected, protocol 1, build 2000940.
- `bridge.log`: `[cs2-bridge mm] Load: Metamod present, KHook installed, ...`, then
  `[cs2-bridge mm+tool] Connect` / `Init`: the **unsigned tool plugin is accepted** by `-tools`
  under the real Steam client, the same as under gbe.
- `down`: `1 restored, 12 removed, 1 unchanged, 0 conflicts`, 14 entries byte-for-byte; the
  tree diff shows only the same 4 game-written files (mtime only).

## `-insecure` enforcement (live)

With the tree prepared, `cs2mcp_launch.exe -tools -console` (no `-insecure`):
`[cs2mcp-launch] FAIL: refusing to inject without -insecure on the command line (...)`, exit 3,
no cs2.exe started. `cs2mcp-native` itself always puts `-insecure` in argv (unit tests). The
remaining gap: a Steam-UI `-tools` session started while the tree is prepared loads the tool
plugin without `-insecure` (FIXME 1; needs a bridge-side check).

## windows-native.md §4.2 / §4.3 answers

- **§4.2 mmload on real Windows 11: works (live).** kernel32's IAT slot for
  `LoadLibraryExW` was found (`hooked 1 kernel32 import slot(s)`) and the server.dll redirect
  happened; no fallback needed on this Windows build.
- **§4.3 direct launch with the real Steam client, no `steam_appid.txt`: starts normally,
  no relaunch (live, once).** The game wrote `game\bin\win64\steam_appid.txt` itself (the file
  already existed in the user's tree from an earlier run; it is a runtime entry, its mtime is put
  back on restore). `up --steam client` refuses when `steam.exe` is not running.
- **§4.3 unsigned tool plugin under real Steam `-tools`: accepted (live).**
- **§4.3 gbe modifies `steam_api64.dll`:** it does, so `--steam` has no default (the user picks
  `gbe` or `client`), and the original DLL is backed up and restored (live: restored
  byte-for-byte, SHA-256 `2A7857…` back in place).

## Open questions / not verified

- **Linux**: no native Steam on the dev host, so the Linux path (`--strategy overlay`, `cs2.sh`
  with the mmload `LD_PRELOAD`) is **unverified** end to end; only the unprivileged
  userns + overlayfs probe ran live on the host (FIXME 3). Needs a Linux machine with Steam CS2.
- Client mode was run once (as agreed), with `--tools --map`. A plain client session (no
  `-tools`) under real Steam, and a session longer than a few minutes, are unverified.
- Whether Steam's "Verify integrity" or an update during a prepared session is handled as the
  README describes (conflict, left alone) is covered by unit tests only.
- *(done since: the MCP servers build and run on Windows, see the two sections below)*
- Should the launcher back up and undo the Workshop Tools' own writes into the install
  (FIXME 5), or is listing them enough?
- *(done since: the bridge refuses without `-insecure`, see below)*

## Windows fixes (branch `native-launcher-windows-fixes`), 2026-11-04

Same VM, build 2000940, bridge m3.18 rebuilt from this branch (`cs2_bridge.dll` SHA-256
`22ac5080...`, `cs2mcp-native.exe` `2a13d20a...`, `client-mcp.exe` `b2f9199f...`,
`hammer-mcp.exe` `55c3ac06...`). **live** = seen on the VM in this run.

**The MCP servers on Windows (FIXME 17, milestone M0).** `client-mcp.exe` and `hammer-mcp.exe`
cross-built here (`cargo build --release --target x86_64-pc-windows-msvc`), copied to
`C:\cs2test\winfix\` with `sigs\` next to them, started with the args `cs2mcp-native
mcp-config --server-dir C:\cs2test\winfix` printed (`--app-root <install root>`, `--project-root
<state>\mcp`), and driven over ssh stdio from the host (`target/winfix/client.sh`, FIXME 11).
After `up --steam gbe --tools --map de_dust2` (BridgeHello after 83.9 s, then 51.4 s in the final
cycle), all **live** on the VM:
- client `session_status`: `flavour: native-windows`, `backend: native`, bridge connected, 33
  methods once the map ran; 20 of 23 groups available, the 3 off with reasons (`capture`: no
  layer; `session`, `multiplayer`: "Windows host: sessions are started and stopped with
  `cs2mcp-native up` / `down` ...").
- `game_state`, `game_tick`, `game_rules`, `console_exec status` / `jointeam 3 1`, `cvar_get`,
  `console_tail` (the install's `game\csgo\console.log` through `--console-log`).
- input: `input_sequence` forward 64 ticks + jump moved the player 239.7 units and raised z by 40
  (the first try ran in freezetime: hook installed, distance 0); `input_set {angles}`,
  `input_clear`; `fire_weapon` (1 shot, clip 13 -> 12).
- sounds: `sounds_status` installed the wrap; `sounds_drain` returned 179 events
  (`Weapon_hkp2000.Single`, `SolidMetal.BulletImpact`, `CT_Gravel.StepLeft`, ...).
- `checkpoint {name}`: answered `no test script is active` (correct; a test script run needs
  `run_testscript`, a podman tool, so the checkpoint **with** a listening script is unverified).
- **`render_world` (FIXME 16): works.** The bridge wrote
  `C:\Users\tester\AppData\Local\cs2mcpbridge\renders\winfix_dust2.png` (640x360, a correct
  de_dust2 view, 50037 unique colours) and client-mcp read it back on the same host (inline image).
- hammer-mcp `session_status` connected; `get_entity_class trigger_multiple` (the FGD from the
  install through `--app-root`) and `list_models` (the install's VPKs).
- `down`: `treeClean: true`, 0 verify failures, no cs2.exe left.

**Bridge `-insecure` gate (FIXME 1), negative case live.** `prepare --steam gbe --tools`, then
`cs2.exe -tools -vulkan -console -condebug` started directly (no launcher, no `-insecure`; gbe,
offline), then `vtm-bridge-request` posted from the interactive session (the VToolMessage mutex is
per logon session; the ssh session gets `OpenMutex failed`). `%LOCALAPPDATA%\cs2mcpbridge\bridge.log`
(the new native default log path):
```
[cs2-bridge none] CreateInterface -> object
[cs2-bridge tool] Connect
[cs2-bridge tool] Init
[cs2-bridge tool] cs2-mcp bridge: refusing to start without -insecure on the command line (...)
[cs2-bridge tool] tick 1 ... tick 2048
```
No loopback listener on cs2.exe, `session.json` not rewritten. The tick counter and the read-only
module hash verifier still run (no RPC, nothing served). `uninstall` afterwards: 20 entries
byte-for-byte, the same 4 game-written paths as always. The positive case (every `up` above
passes `-insecure`) reached BridgeHello. The Metamod `Load` refusal is covered by
`tests/vac_gate.rs` only; on Windows Metamod is only loaded through `cs2mcp_launch.exe`, which
refuses without `-insecure` itself.

**Not verified live:** the Wine/Proton container path with this branch's bridge (renders still go
to `Z:\app\renders` when `Z:\app` exists, unit-tested; the gate needs `-insecure`, which
`bridge/runtime.sh` and the client-mcp runtime always pass); FIXME 7/8 fixes (unit test /
compile only); the Linux launcher (FIXME 3).

## FIXMEs round (branch `native-launcher-fixmes`), 2026-11-05

Same VM, build 2000940. Code in 37c0fa3 + c8fea16 (fixes found live). Package
`target/cs2mcp-native-windows` (`scripts/package.sh`, FIXME 13) copied as it is to
`C:\cs2test\fixmes`: `cs2_bridge.dll` `ad6f088f...`, `client-mcp.exe` `4db45a22...`, first
`cs2mcp-native.exe` `53216082...` (the c8fea16 rebuilds of `cs2mcp-native.exe` and
`hammer-mcp.exe` were copied over it for the last cycles). The dev tool `cs2mcp-native-dev.exe`
went to `C:\cs2test\fixmes-dev.exe`. **live** = seen in this run; every item is in `FIXME.md`.
(The VM's sshd service had vanished; `msiexec /fa C:\cs2wintest\OpenSSH-Win64-v10.4.0.0.msi`
repaired it.)

On the VM, all **live**:
- `up --steam gbe --tools --map de_dust2` with `CS2_BRIDGE_RENDERS=C:\cs2test\fixmes\renders-Jörg`:
  BridgeHello after 183.9 s; `status` shows `inParentJob: true` (the scheduled task's job refused
  breakaway, FIXME 10).
- FIXME 16: `render_world` wrote `...\renders-Jörg\fixmes_jorg.png`, and client-mcp read it back
  inline.
- FIXME 17: `particle_compile` ran as bridge job `j1-mw34sen1`, ok. Its log is in
  `%LOCALAPPDATA%\cs2mcpbridge\jobs` and no `cs2mcp_jobs` appeared in the install.
  `run_map {unload:true}` over ssh: `RuntimeUnavailable` (OpenMutex, other logon session, as
  intended). The same call from `mcpdrive.ps1` in the interactive session: posted natively. After
  the console.log fix (c8fea16; hammer-mcp had watched `<project-root>\workshop/run/app\...`) it
  returned `ok: true` with the signal `Spawn Server: <empty>`.
- FIXME 18: `launch.command` and the unreachable note name `cs2mcp-native up`.
- FIXME 6: a `down` fired while `up` was starting waited on `<state>/lock`, then terminated the
  launcher and the game in 1.4 s, and the supervisor restored (0 verify failures). The first cut
  released the lock after the supervisor's pid only: `down` found nothing to kill and waited out
  120 s (fixed in c8fea16, then seen working as above).
- FIXME 8 (`cs2mcp-native-dev wait-pid`, non-elevated): lsass, wininit and services returned
  `gone:false` at 0 ms, a missing pid `gone:true`, and a short-lived child `gone:true` after 2.1 s.
- FIXME 7: with a read-only `steam_api64.dll`, `uninstall` failed in 0.06 s naming the file read-only.
  After `attrib -R`, a second `uninstall` was byte-for-byte clean.
- FIXME 1 / tick: `cs2.exe -tools` started directly, without `-insecure`. The bridge refused,
  counted ticks up to 1024, logged 0 `verify:` lines (9e4c59f logged 18) and opened no loopback
  listener. `uninstall` was clean.
- Every cycle ended with `down`/`uninstall` and `treeClean: true`. The final `tree-diff` against
  the pre-run baseline showed only the game's 4 usual files plus `game/csgo/rpt/2026_11_05/`
  (FIXME 5). The test addon was removed and the VM is shut off.

Container path (Wine, this branch's bridge, **live**): a `runtime.sh up` from this worktree passed
the gate (Load, Init, listener), and `world.render` wrote `Z:\app\renders\fixmes_container.png` (a
correct de_dust2 view). `job.start` logs still go to `Z:\app\cs2mcp_jobs`. For that run the shared
tree's `cs2_bridge.dll` / `cs2mcp_launch.exe` / `cs2mcp_mmload.dll` / `sdkenginetools.txt` were
backed up to `target/bridge/backup-native-launcher-fixmes-2026-11-04/` in the main checkout,
replaced, and restored after `down` (hashes checked).

**Not verified live:** `cs2mcp-native open`; `compile_map` and `model_compile` on Windows (same
bridge-job route as `particle_compile`); `GBE_FROM_SOURCE=1`; the parent-pid branch of FIXME 9;
`package.sh --os linux`; the Linux launcher (FIXME 3); the Metamod `Load` refusal (no supported path
reaches it, FIXME 20); `wait_pid`'s query-handle poll branch.

**The user's answers (2026-11-05):** FIXME 5: keep listing the game's own writes. FIXME 14: take
gbe_fork's release. Done: `stage-gbe.sh` downloads `release-2026_09_16_2` (= `GBE_REF`)
`emu-win-release-vs22.7z`, checks its pinned SHA-256 and extracts the DLL, which is byte-identical
to the Wine install's copy every run used. Screen capture on Windows is the next work package.

## gbe's Linux libraries from gbe_fork's release (branch `native-launcher-gbe-linux`), 2026-11-05

Goal: a native Linux user gets gbe's `libsteam_api.so` + `steamclient.so` without podman or the
`localhost/cs2-docker` image.

- `stage-gbe.sh`: one pin block (tag `release-2026_09_16_2` = commit `8479ddbc...` = `GBE_REF`)
  with a SHA-256 per asset; `GBE_LINUX_FROM=release|image|source`, defaulting to `image` when
  podman has the image (the bytes every container run used) and `release` otherwise.
- `cs2mcp-native fetch-gbe [--out DIR] [--archive FILE]` (Linux): the release path with the pin
  compiled in, `curl` + `tar` (no new crates), into `<state dir>/gbe`, which `--steam gbe` uses
  when the payload has no gbe for the host. Only runs when the user runs it.

Run here (dev host, no Steam): `emu-linux-release.tar.bz2` downloaded twice (once by hand, once
by `fetch-gbe` into a fresh state dir), 57,603,546 bytes, SHA-256
`59b284b3a22513dabde29f2fe4e256e0e76ac154738d13bf46da064c78b9a12e` = the pin. The cached
rerun, `--archive`, a wrong archive (refused before extraction, nothing written), `stage-gbe.sh`
with `release`, `image`, auto with no image and auto with podman off `PATH`, a ref mismatch and
a non-emulator DLL (both refused), and `discover` on a package layout picking `payload/gbe`, then
`<state>/gbe`, then the payload path again for the error.

| file (x64 regular) | release | `localhost/cs2-docker` `/app` (image 2026-10-03) |
|---|---|---|
| `libsteam_api.so` | `874817c98a2f6adbfbb78576ed56ae2bf9acf83379a0eb0843b73ca6d7e53b3c` (9,499,328 B) | `ac12d08ba868019b180cb7b3236613867fdad861ad13595f05e5a170ed9b79b6` (9,585,072 B) |
| `steamclient.so` | `8f505dff95411a2b332c60f457dade05cf9817995b2be5177d8a251b12c05f58` (9,257,248 B) | `e2c6d6736fbb7bcbbf8a01df9f752b7bde9690e4738601e19066d7a1881e96ab` (9,351,184 B) |

**Not byte-identical.** Both carry `configs.main.ini`. `.comment`: release `GCC: (Ubuntu
13.3.0-6ubuntu2~24.04.1) 13.3.0`, image `GCC: (Debian 14.2.0-19) 14.2.0`; build IDs differ.
Nothing else was compared.

**Not verified:** a game run with the release libs, in a container or on a Linux Steam install
(FIXME 3); `GBE_LINUX_FROM=source`; `fetch-gbe` without network (curl's error path).

## Capture on Windows (FIXME 17), 2026-11-05

The user's next work package. Same VM, build 2000940, package `target/cs2mcp-native-windows`
copied to `C:\cs2test\cap`. **live** = seen in this run.

- `cs2-vk-capture` builds for win64. Its Unix-only parts (the capture dir's owner and mode check,
  `0600`/`O_NOFOLLOW`, the `/tmp` fallback, `/proc/self/exe`) have Windows equivalents: the
  `%LOCALAPPDATA%` default, an owner-SID check (`winsec.rs`, the server does the same check),
  `CREATE_NEW`, `current_exe`. There is a Windows manifest. The Linux live tests still pass
  (`harness_schedule_is_pixel_exact`, `request_frame_end_to_end`,
  `fence_timeout_defers_and_recovers`).
- cs2-native: `payload/vklayer/<plat>/`, `--vk-layer-dir`, `up --no-capture`, `Session.capture`.
  The supervisor sets the three variables in the launcher's environment, and `cs2mcp_launch.exe`
  starts `cs2.exe` with a NULL environment block, so the game inherits them. `status` shows the
  session's capture. `mcp-config` passes client-mcp `--capture-dir <state>\capture`.
- client-mcp: `--capture-dir`. Capture reads that dir as session `native`; `observe`'s console part
  uses `--console-log`; the bridge gate accepts `native`; `session_status` lists capture as
  available.
- **live**, `up --steam gbe --tools --map de_dust2` (BridgeHello after 65.5 s): the supervisor log
  shows the layer; `cs2.exe` has `C:\cs2test\cap\payload\vklayer\win64\cs2_vk_capture.dll` loaded.
  client-mcp over ssh: `screenshot` gave 1280x720 `R8G8B8A8_SRGB`, 81774 unique colours, a correct
  de_dust2 view with the HUD. `observe` returned a frame. `session: "native"` works, and another
  name is refused. `down`: `treeClean`, `byteForByte`.
- **live**, client mode `up --steam gbe --map de_dust2` (BridgeHello after 52.9 s): `screenshot` gave
  1920x1080, a correct frame. `observe` returned frame, console (the install's `console.log`) and
  `game.tick` (session `native`) with no errors. `down`: `treeClean`.
- Bugs found live and fixed: `observe` passed `native` to the console resolver (refused) and the
  tick gate. `package.sh` built only `cs2mcp-native`, because `--bin` restricts every `-p`, so the
  servers it shipped were whatever was last built in `target/`. Earlier packages may have carried
  stale servers; whether the previous round's VM servers matched c8fea16 was not checked.
- **Not verified live:** `up --no-capture`; a payload without the layer; an elevated game; the Linux
  supervisor path (FIXME 3). The VM is shut off.
