# crates/cs2-launcher — `cs2mcp-launcher`, the desktop app

A window over `cs2mcp-native` (`crates/cs2-native`, read its README for what the launcher does to
an install and how it puts it back). It prepares the user's own Steam CS2 install in one click,
starts the game, shows the readiness ladder until the bridge answers, and registers the MCP
servers with Claude. Everything runs in-process through the `cs2_native` library on worker
threads; no command line is parsed or scraped. eframe/egui 0.37 (glow renderer, X11 + Wayland).

## The window

* **Settings** (top): the Steam mode (`gbe`: the emulator, offline, no account; `client`: the
  user's running Steam client), Workshop Tools (`-tools`, Windows only), map, addon, extra game
  arguments, capture on/off, "keep installed after the game exits", and under "Advanced" the
  game root, an extra Steam library, the state dir and the strategy. The settings are not saved
  between runs yet.
* **Status**: the install (path, game build vs. the build this release's signatures target,
  other installs found), the strategy, whether the tree is prepared (manifest), drift (files
  Steam changed under a session), the session (pids, alive), and the lock's holder when another
  command has it.
* **Prepare + Launch** (`up`, then the readiness ladder live), **Prepare only**, **Stop**
  (`down`: also cancels a running wait), **Repair** (`uninstall`: restores a prepared install
  without a session, after a crash or a reboot), **Clean up leftovers…** (a preview first, then
  `cleanup --yes` after a confirmation).
* **Log** (bottom): every event of the running action.
* While `client` is selected the settings say that the game runs with `-insecure` (VAC off, no
  official or secure servers). `-insecure` is always added and
  `-secure` is refused; nothing in the app can change that.

### Findings

A finding that needs a decision (`crates/cs2-native/README.md`, "Findings") stops the action and
opens a dialog with what was found, the paths, and the advice. The buttons are only the ones that
resolve that finding:

| button | what it does |
|---|---|
| Continue anyway | runs the action again with `--accept-conflicts` (keep what was found as it is) |
| Clean up… | the cleanup preview, then the move (orphans, an older layout, a replaced Valve file) |
| Mark as lost… | after a confirmation, `--forget-missing-backups`: restore everything else, record the files whose backups are gone as lost (Steam's "Verify integrity" brings Valve's back) |
| Copy details | the findings as JSON, for a bug report |

A blocker (a tampered payload) has no "Continue anyway".

## Register with Claude

The MCP config is the one `cs2mcp-native mcp-config` prints (both servers, absolute paths; from
the AppImage the command is `$APPIMAGE client-mcp ...`). The transport selector offers stdio;
HTTP is listed and disabled until the servers speak it.

* **Claude Desktop**: the config files offered are
  * Windows: `%APPDATA%\Claude\claude_desktop_config.json` (the documented location, offered even
    when it does not exist yet), and the Microsoft Store build's
    `%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude\claude_desktop_config.json` only
    when that file exists;
  * macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`;
  * Linux (no official build): `$XDG_CONFIG_HOME/Claude/claude_desktop_config.json` only when it
    exists;
  * or any path typed in.

  The servers are merged into `mcpServers` member by member: every other member keeps its text
  and its place, a file that is not a JSON object is refused. The diff is shown; **Back up the
  file and write** copies the old file to `<name>.cs2mcp-backup-<timestamp>` next to it and writes
  the new one atomically. Claude Desktop reads the file at start: restart it.
* **Claude Code**: the commands are shown first (`claude mcp add-json --scope user <name>
  <json>`, with `claude mcp remove --scope user <name>` before each when "replace entries of the
  same name" is ticked) and can be copied; **Run** appears when a `claude` executable is on
  `PATH` and runs them, showing their output.
* **Copy JSON**: the `mcpServers` block for any other client.

## Multicall and `supervise`

The binary dispatches before it opens a window:

```
cs2mcp-launcher                          the window
cs2mcp-launcher cs2mcp-native <args>     the package's cs2mcp-native (likewise client-mcp, hammer-mcp)
cs2mcp-launcher supervise <session file> the session supervisor (for a build without cs2mcp-native next to it)
```

The first form is what makes the AppImage one file: `<AppImage> client-mcp ...` is what the MCP
config names, and `<AppImage> cs2mcp-native supervise ...` is what outlives the window. For the
servers and the game the AppImage's `AppRun` environment is removed first
(`cs2_native::selfexe::scrub_appimage_env`); `cs2mcp-native` keeps it, because it needs
`$APPIMAGE` to name itself.

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

* **Run here** (Linux): the window starts (Wayland and XWayland), `cargo test -p cs2-launcher`
  (the config merge, its order and refusals, the diff, the backup + write, the Claude Code
  commands). The AppImage and the .deb were built and run (multicall, `cs2mcp-native up`/`down`
  through the AppImage on a fake game tree). The actions the buttons start are the
  `cs2_native` functions the CLI runs; the buttons themselves were not clicked through.
* **Not verified**: how the window looks (an automated screenshot came out blank on this
  Wayland desktop; nobody has looked at it with a real game install), anything on Windows (the
  win64 build is `cargo check`/`clippy` only here, the MSI is built in CI only), the Claude
  Desktop paths on a machine with Claude Desktop installed, **Run** with a real `claude` CLI.
