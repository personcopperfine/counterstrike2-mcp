# crates/cs2-launcher — `cs2mcp-launcher`, the desktop app

A window over the `cs2mcp-native` CLI (`crates/cs2-native`; its README says what the CLI does
to an install and how it puts it back). It prepares the user's own Steam CS2 install in one
click, starts the game, shows the readiness ladder until the bridge answers, and puts the install
back. It writes no MCP client config (`cs2mcp-native mcp-config` prints one).
eframe/egui 0.37 (glow renderer, X11 + Wayland).

**Every action is a CLI run** (`src/worker.rs`): the window starts `cs2mcp-native --events jsonl
--no-dialog [settings] <command>` as a child process, renders its progress (one JSON event per
stderr line: `info`, `warn`, `stage`) and reads its result (the JSON on stdout). The exit code
decides what comes next: 0 done, 2 a decision is needed (the result's `findings`), anything else
an error dialog. The window does no install work of its own; it links `cs2_native` only for the
event and finding types and for locating its own files (`selfexe`). The CLI it runs is the
package's: next to the launcher (zip, MSI), `/opt/cs2mcp` (.deb), `$APPDIR/opt/cs2mcp`
(AppImage), else a development build's sibling; without one every action fails with that path.

## The window

* **Settings** (top): the Steam mode (`gbe`: the emulator, offline, no account; `client`: the
  user's running Steam client), Workshop Tools (`-tools`, Windows only), map, addon, extra game
  arguments, capture on/off, and under "Install and state" the game root, an extra Steam
  library, the state dir and the strategy. The settings are not saved between runs yet.
* **Status** (`status --preflight`): the install (path, game build vs. the build this release's
  signatures target), the strategy, whether the tree is prepared (manifest), drift (files Steam
  changed under a session), the session, the lock's holder when another command has it, and the
  preflight findings (payload, leftovers, the build).
* **Prepare + Launch** (`up --no-wait`, then `wait`: the ladder live), **Prepare only**
  (`prepare`), **Stop** (`down`: stop the game, restore, verify; a running `wait` is ended first,
  a running `up` finishes and `down` runs after it), **Repair** (`uninstall`: restores a prepared
  install without a session, after a crash or a reboot), **Clean up leftovers…** (`cleanup`, a
  preview, then `cleanup --yes` after a confirmation).
* **Log** (bottom): the command line of each run and every event it reported.
* While `client` is selected the settings say that the game runs with `-insecure` (VAC off, no
  official or secure servers). `-insecure` is always added and `-secure` is refused; nothing in
  the app can change that.
* **Closing the window** while the install is prepared, a session is alive, a command runs, or
  no status was read yet asks first: "Stop, restore and close" runs `down`, reads the status
  again, and closes only when it shows no manifest and no live session; "Keep the window open"
  cancels. A failed restore keeps the window open with the error. The install is never left
  prepared because the window closed. (The game's own exit restores it as well: the CLI's
  supervisor does that whether the window is open or not.)
* **Errors** (a failed prepare/launch/wait/restore, a missing install) open a dialog of the
  window's own; the CLI runs with `--no-dialog`, so there is never a second, native one.

### Findings

A finding that needs a decision (`crates/cs2-native/README.md`, "Findings") stops the action and
opens a dialog with what was found, the paths, and the advice. The buttons are only the ones that
resolve that finding, and each is a CLI flag or command:

| button | CLI |
|---|---|
| Continue anyway | the action again with `--accept-conflicts` (keep what was found as it is) |
| Clean up… | `cleanup` (the preview), `cleanup --yes` after a confirmation, then the stopped action again (orphans, an older layout, a replaced Valve file with setup_gbe's backup) |
| Mark as lost… | after a confirmation, the action again with `--forget-missing-backups`: restore everything else, record the files whose backups are gone as lost (Steam's "Verify integrity" brings Valve's back) |
| Copy details | the findings as JSON, for a bug report |

A blocker (a tampered payload, a replaced Valve file without a backup) has no "Continue anyway":
there is no flag for it.

## Multicall

The binary dispatches before it opens a window:

```
cs2mcp-launcher                          the window
cs2mcp-launcher cs2mcp-native <args>     the package's cs2mcp-native (likewise client-mcp, hammer-mcp)
```

That stays in the binary for the AppImage, whose single entry point it is: `<AppImage>
client-mcp ...` is what the MCP config names, and `<AppImage> cs2mcp-native supervise ...` is the
session supervisor, which has to outlive the window (and the AppImage mount the window runs
from). For the servers the AppImage's `AppRun` environment is removed first
(`cs2_native::selfexe::scrub_appimage_env`); `cs2mcp-native` keeps it, because it needs
`$APPIMAGE` to name itself. The launcher's own `supervise` is gone: the CLI's `up` starts the
supervisor as `cs2mcp-native supervise` (in an AppImage `$APPIMAGE cs2mcp-native supervise`).

## Uninstalling the app

Uninstalling the app (the MSI's "Uninstall", removing the AppImage or `apt remove`) does **not**
restore a prepared game install: the manifest and backups live in the state dir
(`%LOCALAPPDATA%\cs2mcp-native`, `$XDG_STATE_HOME/cs2mcp-native`), which no uninstaller removes.
Press **Stop** (or **Repair** after a crash) first, then uninstall. A tree that was left prepared
can still be restored later by any cs2-mcp version: `cs2mcp-native uninstall`.

## Packaging (`packaging/`)

The release workflow (`.github/workflows/release.yml`, `RELEASING.md`) builds all of these; they
can be built by hand from a `crates/cs2-native/scripts/package.sh` directory:

```sh
cargo install cargo-packager --version 0.11.8 --locked
python3 -I crates/cs2-launcher/packaging/packager.py --format appimage --verify \
    --package-dir target/cs2mcp-native-linux --version v0.1.0 --out-dir target/dist/appimage
python3 -I crates/cs2-launcher/packaging/packager.py --format deb \
    --package-dir target/cs2mcp-native-linux --version v0.1.0 --out-dir target/dist/deb
# Windows only (WiX 3, downloaded and hash-checked by cargo-packager):
python -I crates/cs2-launcher/packaging/packager.py --format wix \
    --package-dir <package.sh --os windows output> --version v0.1.0 --out-dir out
```

| format | layout | notes |
|---|---|---|
| AppImage | the launcher as `usr/bin/cs2mcp-launcher` (the entry), the package at `$APPDIR/opt/cs2mcp` | cargo-packager runs linuxdeploy (tauri-apps' `linuxdeploy` tag) and linuxdeploy-plugin-appimage (its `continuous` tag: **not pinned**); `--verify` extracts the result and checks `opt/cs2mcp` against the package's `SHA256SUMS`, so a tool rewriting a file in there cannot go unnoticed |
| .deb | `/usr/bin/cs2mcp-launcher`, the package at `/opt/cs2mcp` | section games; found through `<exe dir>/../../opt/cs2mcp` |
| MSI | `%LOCALAPPDATA%\Programs\cs2-mcp\` (the package as is), Start menu + desktop shortcut | `packaging/wix/main.wxs`: per user, no UAC (`InstallScope="perUser"`, `InstallPrivileges="limited"`); the files come from a fragment packager.py generates, one component per directory keyed on an HKCU value with a `RemoveFolder` (ICE38, ICE64), checked before WiX runs; the MSI version is the tag's `x.y.z` |

`packaging/icons/make-icons.py` regenerates `icon.png` / `icon.ico` (stdlib only).
`packaging/release-notes.py` writes the release notes from the packages' `BUILDINFO.txt`.
Nothing is code-signed.

## Status

* **Run here** (Linux): `cargo test -p cs2-launcher` (the command lines per action, the exit
  code -> outcome mapping, stderr lines -> events, when a close asks). The CLI side of the
  protocol (`--events jsonl`, the error JSON, the dialog decision with a recording stand-in for
  zenity) was run by hand. The AppImage and the .deb were built and run.
* **Not verified on Linux**: the buttons clicked through (no X server tooling on this host);
  see the Windows acceptance run in `workflows/native-launcher-report.md` for the button path.
