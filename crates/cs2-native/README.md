# crates/cs2-native — `cs2mcp-native`, the launcher for a user's own CS2 install

`cs2mcp-native` lets cs2-mcp drive a CS2 that the user installed through Steam, on Linux or
Windows, without our podman containers. It prepares the game tree (bridge, Metamod, launcher,
optionally the gbe Steam emulator), launches the game the way the containers do, waits until the
bridge answers, and prints the MCP server config for the user's client. `down` stops the game
and leaves the install exactly as it was before. The desktop app (`crates/cs2-launcher`,
`cs2mcp-launcher`) is a GUI over this crate; `run` is the same thing for a container.

Background: `workflows/windows-native.md` (§3.2 runtime, §3.3 install on a Steam tree, §4.1 VAC,
§4.3 direct launch), `client-mcp/workflows/10-local-install.md`,
`hammer-mcp/workflows/10-local-install.md`, `crates/mm-launch/README.md`,
`bridge/install-rs.sh` (the file layout this crate reproduces).

Status: see "Status" at the bottom and `FIXME.md`. Anything not marked **live** there is
unverified.

## Commands

```
cs2mcp-native discover                   # find the install (Steam libraries or --game-root), print build/tools/state
cs2mcp-native up    --steam gbe|client [--tools] [--map <m>] [-- <extra game args>]
cs2mcp-native status                     # session + readiness + tree state (no changes)
cs2mcp-native down                       # stop the game, restore the tree, verify it
cs2mcp-native uninstall                  # restore the tree without a session (after a crash, a reboot)
cs2mcp-native run   --steam gbe [--cleanup-legacy] [-- <args>]   # container mode (Linux), below
cs2mcp-native cleanup [--yes]            # list (move away with --yes) our files no manifest records
cs2mcp-native mcp-config [--server hammer|client|both] [--transport stdio]   # print the MCP config JSON
cs2mcp-native open <asset>               # post open_asset into the running -tools session (runtime.sh open)
cs2mcp-native fetch-gbe [--out DIR] [--archive FILE]   # Linux: download gbe_fork's pinned Linux release into <state>/gbe
```

`up` = `prepare` (lay the files down), start the supervisor, `wait` (the readiness ladder).
`prepare` and `wait` also exist on their own for debugging; `up --no-wait` skips the ladder.
Global flags for the cases the launcher will not decide alone (see "Findings"):
`--accept-conflicts` and `--forget-missing-backups`.
`open` (a map opens in Hammer, a `.vmdl` loads ModelDoc) has to run on the game's desktop: the
VToolMessage channel is per logon session.

`cs2mcp-native-dev` (same crate, not packaged) holds the live-check helpers: `tree-hash`,
`tree-diff`, `vtm-command`, `vtm-bridge-request`, `gbe-interfaces`, `wait-pid`.

### Packaging

`scripts/package.sh [--os windows|linux] [--out DIR] [--no-build] [--archive]` builds
(`cargo --release`, `-j${JOBS:-2}`) and lays out a self-contained directory, the layout the
defaults below read:

```
<out>/cs2mcp-launcher[.exe]          the desktop app (crates/cs2-launcher)
<out>/cs2mcp-native[.exe]  client-mcp[.exe]  hammer-mcp[.exe]
<out>/sigs/                          the servers' signature DB (their `<exe dir>/sigs` fallback)
<out>/payload/bridge-rs/<plat>/      bridge/build-rs.sh output
<out>/payload/metamod/<plat>/addons/ the Metamod build (+ payload/metamod/BUILDINFO-<plat>.txt)
<out>/payload/gbe/<plat>/            scripts/stage-gbe.sh output (+ GBE_REF.txt)
<out>/payload/vklayer/<plat>/        the capture layer (crates/cs2-vk-capture, built by package.sh)
<out>/BUILDINFO.txt                  version, git sha (+ dirty), toolchain, target game build, the
                                     pins gate's outcome, gbe, Metamod source/version/sha256s and
                                     the hl2sdk commit, "code_signing=none"
<out>/SHA256SUMS                     every file above
```

Inputs: `BRIDGE_DIR`, `METAMOD_DIR`, `GBE_DIR` (defaults `<repo>/target/{bridge-rs,metamod,gbe}`,
Metamod falling back to the main checkout's). Nothing is fetched; a missing input stops it.
Metamod on both platforms is AlliedModders' newest official dev build,
`bridge/metamod/fetch-metamod.sh linux|win64`, recorded with its build name, URL and SHA-256
(`bridge/metamod/build-metamod.sh linux` still builds Linux from source for development; a
package made from that says so in its BUILDINFO).
`--archive` also writes `target/dist/cs2mcp-<VERSION>-<os>-x86_64.{zip,tar.gz}` (`VERSION`,
default `git describe`). A package whose `payload/gbe` lacks the host's files still runs
`--steam gbe` on Linux after `cs2mcp-native fetch-gbe` (below).

The installers wrap that directory unchanged (`crates/cs2-launcher/packaging/packager.py`, see
`crates/cs2-launcher/README.md`): the per-user MSI installs it to
`%LOCALAPPDATA%\Programs\cs2-mcp`, the .deb to `/opt/cs2mcp`, the AppImage carries it at
`$APPDIR/opt/cs2mcp` and is a multicall binary (`<AppImage> cs2mcp-native|client-mcp|hammer-mcp
...`; the supervisor and the MCP config name `$APPIMAGE`, because the mount path changes on every
start). The AppImage's `AppRun` environment (`APPDIR`, `APPIMAGE`, a `LD_LIBRARY_PATH` into the
mount) is removed before the game or a server is started (`selfexe::scrub_appimage_env`). The
release workflow (`.github/workflows/release.yml`, `RELEASING.md`) builds all of them.

**Payload integrity.** A package's `payload/` is checked against its `SHA256SUMS` before
`prepare` copies anything (`src/payload.rs`): a missing, changed or extra file (a partial
download, a zip unpacked over an older version) is a `payloadTampered` blocker; a development
tree without `SHA256SUMS` is reported as `payloadUnverified` and goes on.

## Findings: nothing is decided by guessing

Whatever differs from what the launcher expected is a finding (`src/findings.rs`), printed and
in the JSON result (`"findings": [...]`); the GUI shows it as a dialog.

| severity | what happens |
|---|---|
| `info`, `warn` | reported; the command goes on |
| `decision` | before anything changes (`prepare`, `up`, `run`): the command stops, exit code 2, unless `--accept-conflicts` (GUI: "Continue anyway"). After a restore (`down`, `uninstall`, `run`): the restore is done, the exit code is 2 unless `--accept-conflicts` |
| `blocker` | refused; only the flag the finding names resolves it, or nothing does |

The cases (`code`): `recovered` (an unfinished manifest from a crashed session was restored
first), `conflicts` (files Steam changed under a session: Steam's bytes are kept, the finding
says to verify the game files), `missingBackups` (a blocker: the manifest's backups are gone or
damaged; `--forget-missing-backups` restores the rest and records those files as `lostBackups`,
our bytes are removed from them and Steam's "Verify integrity" brings Valve's back),
`verifyFailed`, `buildMismatch` (the game build is not `sigs/CURRENT_BUILD`: a warning, the bridge
may not resolve its signatures), `legacy` / `orphans` (below), `replacedGameFile`,
`payloadTampered` / `payloadUnverified` (above), `multipleInstalls`. A command stopped on a
finding prints `{"error": ..., "findings": [...]}` and exits 2; other errors exit 1.

**Leftovers** (`src/leftovers.rs`): our files in a tree that no manifest records, because the
state dir was deleted while the tree was prepared, or because `bridge/install-rs.sh` (current or
the old gameinfo-era layout) or the client image's `setup_gbe.sh` installed into it. `discover`,
`status` and `prepare` report them (`orphans`, `legacy`; a decision: without a choice a new
`prepare` would record them as the user's own files). `cleanup` lists them; `cleanup --yes`
moves them into `<state root>/cleanup-<ts>/` (never deletes), puts Valve's library back from
setup_gbe's own backup, un-edits `sdkenginetools.txt` only when that gives the byte-exact Valve
file back, and never touches `gameinfo.gi` (`install-rs.sh uninstall` owns that).

**Recorded pids carry their identity** (`src/procid.rs`): the start time, the boot id and the
PID namespace (Linux), the creation time (Windows). A pid in an old `session.json` after a reboot,
or from another container on the same volume, is stale, not "the game is still running", so
`down` never kills an unrelated process and `uninstall` is not refused because of one. The state
lock records its holder (pid, command, start time); a command waiting on it names that holder.

## Container mode: `run`

`cs2mcp-native run --steam gbe [--cleanup-legacy] [--no-capture] -- <game args>` (Linux) is one
foreground session for a container on the game's volume (`src/run.rs`): prepare **in place**
with the manifest (container runtimes refuse the user namespace the overlay needs; `--strategy
auto` becomes `in-place`), run the game in its own process group, wait, then restore and verify
the volume before it exits. SIGTERM / SIGINT / SIGHUP stop the game the way `down` does (TERM to
the group, KILL after 15 s), then restore. The exit code is the game's (128 + signal when it was
killed), 3 when the volume could not be restored, 2 for an open decision. The JSON result goes to
stdout, the game's own stdout to stderr. It may be PID 1: it becomes a child subreaper and reaps
everything it inherits.

The state dir belongs on the same volume (`--state-dir <volume>/.cs2mcp-native`): a container
killed before its restore leaves the manifest there and the next `run` restores it first; a
second container on the same volume waits on the lock. `--cleanup-legacy` first moves the old
container layout out of the volume (install-rs.sh's files, setup_gbe.sh's emulator; the same
moves as `cleanup --yes`). client-mcp uses it with `--native-package <dir>` for `linux-client`
sessions (`client-mcp/docker/README.md`, "Launcher mode"): the package is mounted read-only and
`podman stop -t 60` leaves the restore its time.

## Steam modes (`--steam`)

| mode | what talks to the game | install changes for Steam | account |
|---|---|---|---|
| `gbe` | the gbe_fork (Goldberg) Steam emulator, offline | Windows: `bin/win64/steam_api64.dll` replaced by gbe's, `bin/win64/steam_settings/` (with its own `steam_appid.txt`). Linux: `bin/linuxsteamrt64/libsteam_api.so` + `steamclient.so` replaced/added, `bin/linuxsteamrt64/steam_settings/` | none (a placeholder SteamID, `configs.user.ini`) |
| `client` | the user's running, logged-in Steam client | none: a direct `cs2.exe` launch with Steam running attaches to it (the game writes `bin/win64/steam_appid.txt` itself; it is a runtime entry). `up` refuses when `steam.exe` is not running (cs2 would stop on a modal "Failed to connect with local Steam Client process"). **live** on the test VM | the user's |

gbe's binaries are not built here. `--gbe-dir` points at a directory holding them
(`win64/steam_api64.dll`, `linux64/{libsteam_api.so,steamclient.so}`). Without it the launcher
takes `payload/gbe` (or `<repo>/target/gbe/`) when that has the host's files, else
`<state dir>/gbe` when that has them (what `fetch-gbe` writes).

`scripts/stage-gbe.sh` fills `<repo>/target/gbe/`. One pin block in the script names gbe_fork's
release for `GBE_REF` (`GBE_REF`, else the one `GBE_DOCKERFILE` names when it is set, else the pin
itself; another ref stops. No Dockerfile is read unless `GBE_DOCKERFILE` points at one, e.g. a
clone of [personcopperfine/cs2-docker](https://github.com/personcopperfine/cs2-docker)'s
`client/Dockerfile`): tag, commit and each
asset's SHA-256, checked after the download (cached in `target/gbe-release/<tag>/`). The Windows
DLL comes from that release's `emu-win-release-vs22.7z` (`7z`; gbe_fork builds Windows only with
MSVC), or `GBE_WIN_DLL=<path>`. The Linux libs come from `GBE_LINUX_FROM`:

| `GBE_LINUX_FROM` | source | needs |
|---|---|---|
| `image` (default when podman has the client image: `GBE_IMAGE`, else `CS2_MCP_IMAGE`, else `ghcr.io/personcopperfine/cs2-docker:latest`) | the image's `/app/`, the libs every container run used | podman + the image |
| `release` (default otherwise) | the release's `emu-linux-release.tar.bz2`, `release/regular/x64/` extracted with `tar` | curl, tar, bzip2 |
| `source` (or `GBE_FROM_SOURCE=1`) | a client Dockerfile's `builder` stage at `GBE_REF` (`GBE_DOCKERFILE`, required: the published [`client/Dockerfile`](https://github.com/personcopperfine/cs2-docker/blob/main/client/Dockerfile) pulls prebuilt gbe and has no such stage) | podman + `GBE_DOCKERFILE` |

The release and image libs are both gbe at `GBE_REF` but not byte-identical (their `.comment`
names different compilers: Ubuntu GCC 13.3.0 in the release, Debian GCC 14.2.0 in the image).
`GBE_REF.txt` records which source each half came from. Every copy is refused unless it is the
emulator (it names `configs.main.ini`).

`cs2mcp-native fetch-gbe` (Linux) does the `release` path without the repo or podman, for a
packaged launcher: the same pin is compiled in (`src/gbe.rs`; a test holds it equal to the
script's), it downloads with `curl` into `<state dir>/gbe-release/<tag>/` (or takes
`--archive FILE`), refuses a SHA-256 mismatch, extracts the two libraries with `tar`, refuses a
non-emulator, and writes `<state dir>/gbe/linux64/` + `GBE_REF.txt` (or `--out DIR`). It only
downloads when it is run; `prepare`/`up` never fetch. No HTTP or archive crate: the workspace has
none, so it shells out to `curl` and `tar` (bzip2), which a Linux desktop has.
`steam_interfaces.txt` is generated from the install's **own** original Steam API library (the
same scan gbe's `generate_interfaces` does), so it matches the user's build.

## The file set

Everything goes in under the install root (`.../Counter-Strike Global Offensive`). The layout
and the generated text are byte-identical to `bridge/install-rs.sh` (wine/linux).

| path | Windows | Linux | why |
|---|---|---|---|
| `game/bin/win64/tools/cs2_bridge.dll` | yes | – | the bridge (tool plugin + Metamod plugin, ONE image) |
| `game/bin/sdkenginetools.txt` | edited (only with `--tools`) | – | the `cs2_bridge` entry, inserted before the `hammer` entry |
| `game/bin/win64/cs2mcp_launch.exe`, `cs2mcp_mmload.dll` | yes | – | gameinfo-free Metamod loading (mm-launch) |
| `game/csgo/addons/cs2mcp/bin/linuxsteamrt64/{libcs2_bridge.so,libcs2mcp_mmload.so}` | – | yes | the bridge + the `dlopen` interposer |
| `game/csgo/addons/metamod/...` | `bin/win64/{server.dll,metamod.2.cs2.dll}`, `metaplugins.ini` | the whole Linux build | Metamod, standard layout |
| `game/csgo/addons/metamod/cs2mcp.vdf` | `"file" "../bin/win64/tools/cs2_bridge.dll"` | `"file" "addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so"` | our Metamod plugin registration |
| gbe files | see above | see above | `--steam gbe` only |

`gameinfo.gi` is never touched (mm-launch). Metamod is on every launch path, tools or not: we
are one plugin among many.

## Not modifying the user's install

### Linux: overlay (default `--strategy overlay`)

The game runs in a private **user + mount namespace** (`unshare(CLONE_NEWUSER|CLONE_NEWNS)`,
the current uid/gid mapped to themselves, so the game runs as the user and holds no capability
after `exec`). Inside it, kernel overlayfs (`userxattr`, unprivileged since Linux 5.11) is
mounted **over the install root**:

```
lowerdir = <install root>            (the user's tree, read-only through the overlay)
upperdir = <state>/overlay/upper     (our files, staged before the mount; everything the game writes)
workdir  = <state>/overlay/work
```

The user's tree is never written, not even by the game (`console.log`, `cfg/`, shader caches
all land in `upper/`). Nothing has to be restored: the mount disappears with the last process
in the namespace. `down` kills the game; `upper/` is kept until the next `up` so its logs can be
read (`<state>/overlay/upper/game/csgo/console.log`).

If the kernel or a security policy refuses the namespace (e.g. AppArmor's
`apparmor_restrict_unprivileged_userns` profile on a confined binary, or `user.max_user_namespaces=0`),
`up` stops with the error and the fallback to use: `--strategy in-place`, the Windows method
below. fuse-overlayfs is not used yet (`FIXME.md`).

### Windows (and Linux `--strategy in-place`): backup + restore with a manifest

Windows has no unprivileged overlay (bindflt/ProjFS need admin or a driver), so the files are
written into the tree and every change is recorded first, in a write-ahead **manifest**.

State lives outside the tree: `%LOCALAPPDATA%\cs2mcp-native\<install-id>\` (Linux
`$XDG_STATE_HOME/cs2mcp-native/<install-id>/`, default `~/.local/state/...`), where
`<install-id>` is the first 16 hex digits of the SHA-256 of the canonical install root.

```
<state>/manifest.json      the record below (atomic: temp file + rename, flushed)
<state>/backup/0001 ...    byte copies of every file that existed before we changed it
<state>/session.json       the running session (pids, mode, args, start time)
<state>/lock               the state lock (below)
<state>/logs/              launch.log (launcher + mmload lines), bridge.log, console-<ts>.log
<state>/history/           manifests of finished sessions (restored-<ts>.json)
```

`manifest.json` (format `cs2mcp-native/manifest/1`):

```json
{
  "format": "cs2mcp-native/manifest/1",
  "installRoot": "C:\\SteamLibrary\\steamapps\\common\\Counter-Strike Global Offensive",
  "build": "2000940",
  "steam": "gbe",
  "tools": true,
  "state": "applied",
  "createdAt": 1762250000,
  "entries": [
    { "path": "game/bin/win64/steam_api64.dll", "kind": "file", "role": "ours",
      "before": { "sha256": "…", "size": 317080, "backup": "backup/0001" },
      "after":  { "sha256": "…", "size": 11429800 } },
    { "path": "game/bin/win64/steam_settings", "kind": "dir", "role": "ours", "before": null },
    { "path": "game/csgo/console.log", "kind": "file", "role": "runtime",
      "before": null, "after": null }
  ]
}
```

Rules:

* **Write-ahead.** For each path: back the existing file up (copy, hash the copy, flush), append
  the entry with `before` and the planned `after`, write the manifest, and only then write the
  file (temp file next to it + rename). A crash at any point leaves a manifest that covers every
  change already on disk.
* `role: "ours"` = a file or directory we write. `role: "runtime"` = a file the **game** writes
  during our session that the user may also have (`game/csgo/console.log` from `-condebug`;
  Windows `game/bin/win64/steam_appid.txt`, which the game writes on a direct launch). Its
  `before` is backed up; on restore our session's copy is moved to `logs/` and the user's bytes
  (or absence, and the mtime) are put back.
* **Game writes outside the manifest.** `prepare` also stores a size+mtime listing of the whole
  install (`tree-before.json`, no hashing). Restore compares it with the tree afterwards and
  reports every other path the game wrote (`gameWritten`). These are **not undone**: a Workshop
  Tools run rewrites `game/_toolsettings/asset_browser/settings.txt`,
  `game/csgo/tools_thumbnail_cache.sqlite3-shm` and `game/core/_bakeresourcecache/...` exactly as
  it does when the user starts the tools from Steam. So "byte-for-byte" means every file *we*
  touched; the rest of the tree is the game's own business, listed so nobody has to guess.
* **Locked files.** A killed game is torn down asynchronously on Windows; for a moment its
  mapped DLLs refuse a rename and its open files a delete. Every restore rename/remove retries on
  a sharing violation / access denied for up to 15 s.
* **Restore** walks the entries in reverse. For a file:
  * current = `after` (ours, untouched) → put `before` back (or delete it if `before` is null);
  * current = `before` → already restored;
  * missing → restore `before` (if any);
  * anything else → **changed by someone else** (a Steam update or "Verify integrity" rewrote
    it): left alone and reported. Steam's new bytes win over our old backup.
  Directories we created are removed only when empty; a non-empty one is reported.
* **Verify** after the restore: every `before` file hashes to its recorded SHA-256, every created
  path is gone. `down` prints `every file we touched is byte-for-byte the user's again (N entries
  checked)` or the list of differences, then the `gameWritten` list, and the manifest moves to
  `history/`.
* `up` on a tree that still has an applied manifest (Steam may have reverted part of it): the old
  manifest is restored first, then a fresh one is applied, so `before` is always the user's
  current bytes.
* `up` refuses while a CS2 process runs from the tree (the files are in use; the user's own
  session would see a half-changed tree).
* **One command at a time.** `prepare`, `up`, `down` and `uninstall` take `<state>/lock` (an OS
  file lock: `LockFileEx`/`flock`, gone with the process that held it). `up` holds it until the
  supervisor has recorded its own pid and the launcher's, so a `down` started meanwhile waits
  (`waiting for <state>/lock ...`) and then finds the game to stop, instead of restoring under a
  starting game. The supervisor does not take it.

Hashing uses SHA-256 throughout, so the manifest also proves what was installed.

## VAC: `-insecure` is enforced in code (windows-native.md §4.1)

Injected code (Metamod, the bridge, mmload) must never run in a VAC-secured session of a real
Steam account. Four independent checks:

1. `cs2mcp-native` builds the game command line itself and always puts `-insecure` in it. User
   extra args after `--` cannot remove it, and `-secure`/`+sv_lan 0`-style overrides are not
   filtered but cannot undo `-insecure` either (the engine has no "secure again" flag). Extra
   args containing `-insecure` twice are harmless.
2. `cs2mcp_launch.exe` (crates/mm-launch) **refuses to inject without `-insecure` in its own
   command-line tail**: it exits 3 with `[cs2mcp-launch] FAIL: refusing to inject without
   -insecure ...` before creating the game process. Every container launch line already passes it.
3. On Linux, Metamod itself refuses to load in a client without `-dedicated`/`-insecure`
   (spike client-noinsecure, `bridge/install-rs.sh` header), and the bridge is only loaded
   through Metamod there.
4. **The bridge itself** (`crates/cs2-bridge/src/vac.rs`) checks the process command line at
   both lifecycle starts: without `-insecure` (or `-dedicated`) the tool plugin's `Init` logs
   `refusing to start without -insecure ...` and leaves the wiring uninitialised (no listener,
   no request served; `Init` still answers 1 so the tools start normally), and the Metamod
   `Load` returns false with the same message. This closes the window where a `-tools` session
   started from the Steam UI while the tree is prepared would load the bridge through the
   `sdkenginetools.txt` entry. The supervisor still restores the tree as soon as the game exits,
   and `--tools` stays opt-in.

## Launch, processes and readiness

* **Windows:** `up` starts a detached `cs2mcp-native supervise` process (breakaway from `up`'s
  job when allowed; `up`'s own std handles are made non-inheritable first, so the caller's pipe
  closes when `up` returns). The supervisor records its own pid, joins a fresh
  `KILL_ON_JOB_CLOSE` job (so the game dies with a killed supervisor; when `up`'s own job
  refused the breakaway the supervisor stays in that job too, says so, and `status` shows
  `inParentJob: true`: closing that job ends the session before the restore), starts
  `game\bin\win64\cs2mcp_launch.exe <args>` with stdout/stderr to a fresh `logs/launch.log`
  (the launcher puts cs2.exe in its own kill-on-close job), waits for it, waits until no
  `cs2.exe`/`cs2mcp_launch.exe` runs from the install any more, and **restores the tree**
  (`--keep-installed` keeps it). `down` terminates the launcher and every `cs2.exe` whose image
  is under the install root (or, with an image no longer readable, whose parent is a recorded
  launcher pid), waits on their process handles, waits for the supervisor to finish
  the restore, and restores on its own whatever is still recorded. `uninstall` refuses while a
  game process runs from the install.
  `CS2_BRIDGE_LOG=<state>/logs/bridge.log` is set, because the bridge's default Windows log path
  is the container's `Z:\app\...`.
* **Linux:** the supervisor enters the namespace, mounts the overlay and runs
  `game/cs2.sh` with `LD_PRELOAD=csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so`
  (relative to the game dir, as the containers do; mm-launch README "Facts").
* Game args, fixed part: `-insecure -console -condebug` (Windows also `-vulkan`, as the
  containers), `-tools` with `--tools`, `+map <m>` with `--map`, then the focus flags
  `+engine_no_focus_sleep 0 +r_always_render_all_windows 1`, then the user's extra args.
* Readiness ladder (each stage with a deadline, polling real signals, no fixed sleeps):
  1. `ProcessUp`: the launcher is alive; `launch.log` has no `FAIL:` line (checked only once the
     supervisor has started the launcher, so the last session's log cannot answer).
  2. `MetamodLoaded`: `[cs2mcp-mmload] redirect:` in `launch.log` (Windows) / the game's stderr
     (Linux).
  3. `ConsoleLogExists`: `game/csgo/console.log` written after our start.
  4. Tools only: `ToolChannel`, the VToolMessage mapping exists; then a `tool_request` for
     `cs2_bridge` is posted (re-posted every 5 s) - `vtmsend.exe` ported to Rust (`src/vtm.rs`).
  5. `BridgeHello`: a discovery file newer than our start, then `control.hello` with its token
     over loopback TCP answers with protocol 1 and the game build. Client sessions (no `-tools`)
     only start the listener once a map runs, so `--map` is needed to reach this stage there.
* Discovery file: Windows `%LOCALAPPDATA%\cs2mcpbridge\session.json`, Linux
  `$XDG_RUNTIME_DIR/cs2mcpbridge/session.json` (both written by the bridge, unchanged).

## MCP servers

The servers already speak stdio and attach to a running bridge through the discovery file, so
the launcher does not run them; the MCP client does. `mcp-config` prints a ready-to-paste
`mcpServers` block with absolute paths (the server dir: `--server-dir`, else the package dir,
or `cs2mcp-native`'s own dir outside a package, when `hammer-mcp` is there, else the building
repo's `target/release` when it exists on this machine, else that same dir;
from an AppImage the command is `$APPIMAGE` with `client-mcp`/`hammer-mcp` as its first
argument). `--transport stdio` is the only transport so far; `http` is refused with "not
available yet". Nothing here writes a client's config file: paste the block yourself.

```json
{ "mcpServers": {
  "cs2-hammer": { "command": "<repo>/target/release/hammer-mcp",
                  "args": ["--discovery", "<discovery path>", "--expect-build", "2000940"] },
  "cs2-client": { "command": "<repo>/target/release/client-mcp",
                  "args": ["--discovery", "<discovery path>", "--expect-build", "2000940",
                           "--console-log", "<console.log path>"] } } }
```

Attach-only (no `--launch`): the session is the launcher's. `--console-log` is the overlay's
`upper/` copy on Linux. An in-place session (Windows, Linux `--strategy in-place`) also gets
`--app-root <install root>` (the tree the file tools read); on Windows `--project-root
<state>/mcp` too (where the servers put `artifacts/`; their built-in default is the build
machine's checkout).

**Windows (milestone M0, the smallest cut):** `client-mcp.exe` and `hammer-mcp.exe` build for
`x86_64-pc-windows-msvc`. The podman runtime and the Vulkan capture client compile there but are
inert: `launch_session` refuses, the `session`/`multiplayer`/`capture` groups report why, and
`session_status` says `backend: native`, flavour `native-windows`. Bridge-written files
(`render_world`'s PNG) come back as plain Windows paths: the bridge writes renders to
`%LOCALAPPDATA%\cs2mcpbridge\renders\` when there is no container `Z:\app`
(`CS2_BRIDGE_RENDERS` overrides; any path, non-ASCII included), and on a Windows host a Windows
path maps to itself. `launch.command` and the "bridge not reachable" note name
`cs2mcp-native up`. `screenshot`/`observe` work through the capture layer (Capture, below).

hammer-mcp's VToolMessage posts (`run_map`'s `disconnect`/`map`, the `console_exec` fallback,
`open_map`'s first-map `open_asset`) go straight into the channel's shared memory on Windows
(`crates/cs2-vtm`, the `vtmsend.exe` port this launcher's ladder also uses; replies say
`container: "native"`), and its console watch reads the install's `game\csgo\console.log`. The
channel is per logon session: hammer-mcp has to run on the game's desktop (an MCP client there);
started over ssh it answers `RuntimeUnavailable` with that reason. Compiles (`model_compile`,
`particle_compile`, `compile_map`) are bridge jobs (`job.start`): the bridge runs
`resourcecompiler.exe` from the install and keeps its job logs in
`%LOCALAPPDATA%\cs2mcpbridge\jobs` (never in the install); without the bridge they refuse with
the `cs2mcp-native up` hint.

### Capture

`screenshot` and `observe` read frames from the Vulkan capture layer (`crates/cs2-vk-capture`,
`VK_LAYER_CS2MCP_capture`), loaded into the game only. When the payload has
`vklayer/<win64|linux64>/` (the manifest plus `cs2_vk_capture.dll` / `libcs2_vk_capture.so`;
`--vk-layer-dir` / `CS2MCP_VK_LAYER_DIR` override it), `up` records it in the session. The
supervisor then gives the launcher (and through it `cs2.exe`, which inherits the environment)
`VK_ADD_LAYER_PATH=<layer dir>`, `VK_INSTANCE_LAYERS=VK_LAYER_CS2MCP_capture` (both ahead of any
value the user already set) and `CS2MCP_CAPTURE_DIR=<state>/capture`, which it creates. This is an
explicit layer: no registry key, no implicit layer, and nothing changes for any other Vulkan
program. `up --no-capture` leaves it out, and a payload without the layer says so at `up`.
`mcp-config` gives client-mcp `--capture-dir <state>/capture`. Such a server captures only the
session `native`: `session_status` shows the capture group as available, and `observe` takes its
console part from `--console-log` and its tick from the bridge. The game must present through
Vulkan (`-vulkan`, which `up` always passes). The Windows loader ignores `VK_ADD_LAYER_PATH` in
an elevated process, so an elevated game would run without the layer. Both sides check the
capture dir: no reparse point, and owned by this user (the token's user or its default owner).

## Status

See `FIXME.md` (hacks) and `workflows/native-launcher-report.md` (what was seen live).
