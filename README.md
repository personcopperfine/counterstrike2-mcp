# cs2-mcp

MCP servers that let an AI assistant (Claude, Codex, Cursor, ...) drive **Counter-Strike 2** and
its **Workshop Tools** on your own machine: build and edit Hammer maps, import and compile models
and particles, then load the map, walk around in it, take screenshots and run playtests.

There are two servers:

| Server | What it drives | Platforms |
|---|---|---|
| `cs2-client` (`client-mcp`) | the running game: player state, input, console, screenshots, in-game tests | Windows, Linux |
| `cs2-hammer` (`hammer-mcp`) | the Workshop Tools: Hammer, ModelDoc, the particle editor, compiles, and the game inside the tools | Windows |

## Requirements

* Counter-Strike 2 installed through Steam (the Workshop Tools as well, for `cs2-hammer`).
* Windows 10/11 x64, or Linux x86_64 (kernel 5.11 or newer) with a Vulkan GPU driver.
* An MCP client: Claude Code, Claude Desktop, Codex, Cursor, or anything else that runs stdio
  MCP servers.

> **Warning.** The game is always started with `-insecure`, so VAC is off and you can't join
> official or secure servers while cs2-mcp is running. The launcher puts your install back the
> way it was when you press **Stop**.

## 1. Install

Download the file for your system from the
[latest release](https://github.com/personcopperfine/counterstrike2-mcp/releases):

| System | File | Install |
|---|---|---|
| Windows | `cs2mcp-<version>-windows-x86_64.msi` | Run it. It installs per user (no admin prompt) to `%LOCALAPPDATA%\Programs\cs2-mcp` and adds a Start menu entry. |
| Windows (portable) | `cs2mcp-<version>-windows-x86_64.zip` | Unzip it anywhere. |
| Linux (any distro) | `cs2mcp-<version>-linux-x86_64.AppImage` | `chmod +x` it and run it. |
| Debian / Ubuntu | `cs2mcp-<version>-linux-amd64.deb` | `sudo apt install ./cs2mcp-<version>-linux-amd64.deb` (installs to `/opt/cs2mcp`) |
| Linux (portable) | `cs2mcp-<version>-linux-x86_64.tar.gz` | Extract it anywhere. |

`SHA256SUMS` in the release lists the checksum of every file. Nothing is code-signed, so Windows
SmartScreen may ask you to confirm the first run.

## 2. Start the game

Open **cs2mcp-launcher** (Start menu, the AppImage, or `cs2mcp-launcher` in the extracted
folder):

1. Pick the Steam mode. `gbe` runs offline with no Steam account. `client` uses the Steam client
   you're already logged in to.
2. On Windows, tick **Workshop Tools** if you want `cs2-hammer`.
3. Press **Prepare + Launch** and wait until every step is checked.
4. When you're done, press **Stop** (or just close the window). That stops the game and restores
   your install.

## 3. Add the servers to your MCP client

The launcher prints the server config for your machine. Run it once, in a terminal:

```sh
# Windows (PowerShell)
& "$env:LOCALAPPDATA\Programs\cs2-mcp\cs2mcp-native.exe" mcp-config
# Linux, AppImage
./cs2mcp-<version>-linux-x86_64.AppImage cs2mcp-native mcp-config
# Linux, .deb
/opt/cs2mcp/cs2mcp-native mcp-config
# zip / tar.gz: run cs2mcp-native from the extracted folder
```

It prints a standard `mcpServers` block with the absolute paths for your machine:

```json
{ "mcpServers": {
  "cs2-hammer": { "command": "C:\\Users\\you\\AppData\\Local\\Programs\\cs2-mcp\\hammer-mcp.exe",
                  "args": ["--discovery", "...", "--expect-build", "..."] },
  "cs2-client": { "command": "C:\\Users\\you\\AppData\\Local\\Programs\\cs2-mcp\\client-mcp.exe",
                  "args": ["--discovery", "...", "--expect-build", "...", "--console-log", "..."] } } }
```

Use `--server client` or `--server hammer` to print only one of them. If it says "no CS2 install
found", point it at your Steam library: `--library <steam library folder>` (before
`mcp-config`). The servers connect to the
game the launcher started, so start the game first (step 2). After a game or cs2-mcp update, run
`mcp-config` again.

Your MCP client starts the servers from this config (stdio). Add them as follows:

### Claude Code

Per project: save the printed block as `.mcp.json` in the project's root folder. Claude Code asks
you to approve the servers the first time.

Or add them for every project, one server at a time, with the `command` and `args` from the
printed block:

```sh
claude mcp add --scope user cs2-client -- <command> <args...>
claude mcp add --scope user cs2-hammer -- <command> <args...>
```

Check with `claude mcp list`, or `/mcp` inside a session.

### Claude Desktop

Settings → Developer → **Edit Config** opens `claude_desktop_config.json`. Merge the printed
`mcpServers` entries into it, then restart Claude Desktop.

### Codex (CLI and IDE extension)

```sh
codex mcp add cs2-client -- <command> <args...>
codex mcp add cs2-hammer -- <command> <args...>
```

Or write the servers into `~/.codex/config.toml` yourself:

```toml
[mcp_servers.cs2-client]
command = '<command>'
args = ['--discovery', '<...>', '--expect-build', '<...>', '--console-log', '<...>']
startup_timeout_sec = 30
```

(Single-quoted TOML strings keep Windows backslashes as they are.) Check with `codex mcp list`,
or `/mcp` inside Codex.

### Cursor, Gemini CLI, Windsurf and others

Most clients read the same `mcpServers` format. Paste the printed block into the client's MCP
config: `~/.cursor/mcp.json` (Cursor), `~/.gemini/settings.json` (Gemini CLI),
`~/.codeium/windsurf/mcp_config.json` (Windsurf). VS Code's `.vscode/mcp.json` calls the key
`servers` instead of `mcpServers`, with the same entries inside.

### Try it

Ask your assistant to *"call session_status on cs2-client"*. It should report the connected game
and its build. From there, try *"load de_dust2, walk to the A site and take a screenshot"*, or, on
Windows with the Workshop Tools, *"make a new map with a small room and a light, compile it and
run it"*.

## Tools

A selection. Each server lists all of its tools to your MCP client, with full descriptions.

**`cs2-client`, the game**

| Tool | What it does |
|---|---|
| `session_status` | game build, connection, which tool groups are available |
| `game_state` | local player: map, position, view angles, health, active weapon |
| `radar` | players, bomb and hostages, projected onto the map overview |
| `game_rules` | round and warmup state, round times, bomb planted |
| `entity_find` | find entities in the running game by class or name |
| `console_exec`, `console_tail`, `cvar_get` | run console commands, read the console and cvars |
| `setpos`, `look_at`, `input_set`, `input_sequence` | teleport, aim and press buttons, as a player would |
| `fire_weapon`, `select_weapon`, `use_entity` | shoot, switch weapons, press use on an entity |
| `game_snapshot`, `game_restore`, `round_start` | save and restore the player's state; put the round in a known state |
| `screenshot`, `observe`, `ocr` | capture the game view and read text from it |
| `game_test` | declarative in-game test: setup, steps, expectations, teardown (JUnit output) |
| `demo_record_start`, `demo_record_stop` | record a SourceTV demo |

**`cs2-hammer`, the Workshop Tools**

| Tool | What it does |
|---|---|
| `new_map`, `open_map`, `save_map` | create, open and save Hammer maps |
| `create_block`, `create_mesh`, `extrude_mesh_face`, `set_mesh_material`, `align_texture` | build and texture geometry |
| `apply_layout` | build many meshes and entities from one JSON layout |
| `create_entities`, `set_entity_keyvalues`, `add_entity_connections` | place entities and wire their inputs/outputs, checked against the FGD |
| `create_props`, `list_models`, `find_assets` | find models, materials, sounds, particles and place props |
| `create_trigger`, `create_sound_entities`, `prefab` | triggers, sound entities, prefabs |
| `look`, `render_view` | see the map: framed images of a target, renders from any camera |
| `get_selection`, `set_selection` | read or change what's selected in Hammer |
| `compile_map`, `run_map`, `play_test` | compile the map, load it in the engine, play through steps |
| `model_import`, `model_edit`, `model_compile`, `model_render` | import glTF/FBX models, edit .vmdl files, compile and render them in ModelDoc |
| `particle_create`, `particle_live_edit`, `particle_compile`, `particle_render` | build and preview particle systems |
| `cs_script` | TypeScript map scripts (`point_script`) for the addon |
| `report_ready` | pre-release check of the map, with a fix for each problem |
| `get_playbook` | step-by-step procedures for whole tasks (blockout a map, import a model, ...) |

## Uninstalling

Press **Stop** in the launcher first (or **Repair** after a crash), so your game install is put
back. Then uninstall the app (Windows "Apps", `sudo apt remove cs2-mcp`, or delete the AppImage or
folder). If you skipped that, any version of cs2-mcp can still restore the install later with
`cs2mcp-native uninstall`.

## Contributing

Building from source, the architecture and the game-update procedure are documented for
developers and agents in [AGENTS.md](AGENTS.md).

## License

See [LICENSE](LICENSE).
